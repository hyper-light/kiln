//! Bounded legacy ownership migration. Absence of a PID, marker or source is never proof.
//! Clearing requires an exact changed kernel boot identity, unchanged evidence, exclusive
//! admission, and no outstanding persistent daemon ownership. No force mode exists.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct Legacy {
    source: PathBuf,
    sha256: String,
    #[serde(default)]
    boot_id: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct Report {
    operation: &'static str,
    mode: &'static str,
    cleared: usize,
    evidence: Vec<Evidence>,
    docker_ownership: Vec<serde_json::Value>,
    daemon_journals: Vec<super::super::docker::daemon::JournalReport>,
}

#[derive(Serialize)]
struct Evidence {
    fence: PathBuf,
    source: Option<PathBuf>,
    recorded_boot: Option<String>,
    current_boot: Option<String>,
    eligible: bool,
    diagnostic: String,
}

fn boot() -> anyhow::Result<Option<String>> {
    #[cfg(target_os = "linux")]
    { Ok(Some(std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim().to_owned())) }
    #[cfg(not(target_os = "linux"))]
    { Ok(None) }
}

fn valid_boot(value: &str) -> bool {
    value.len() == 36 && value.bytes().enumerate().all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { c == b'-' } else { c.is_ascii_hexdigit() })
}

/// Shared local operation implementation; read-only unless the caller requests cold-boot
/// reconciliation. Native process proof never removes daemon ownership or its unknown intents.
pub(crate) fn run(operation: &'static str, after_cold_boot: bool) -> anyhow::Result<Report> {
    let admission = super::Admission::production();
    let current = boot()?;
    let _host = if after_cold_boot { Some(crate::reach::leases::host_guard(&admission, false)?) } else { None };
    let mut records = super::records(&admission)?;
    records.sort_by(|a, b| a.0.cmp(&b.0));
    let daemon = records.iter().any(|(_, record)| record.docker.is_some());
    let docker_ownership = records.into_iter().filter_map(|(fence, record)| record.docker.map(|ownership| (fence, ownership)))
        .map(|(fence, ownership)| Ok(serde_json::json!({ "process_fence": fence, "ownership": ownership.report(&admission)? })))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let unknown = admission.root().join("daemon");
    // The journal backend can have completed records too. A directory's existence is not a
    // daemon outcome; its own reconciliation must discharge outstanding evidence first.
    let daemon = daemon || outstanding_journals(&unknown)?;
    let daemon_journals = journal_reports(&unknown)?;
    let mut report = reconcile(&admission.root().join("legacy-ci"), current.as_deref(), after_cold_boot, daemon)?;
    report.operation = operation;
    report.docker_ownership = docker_ownership;
    report.daemon_journals = daemon_journals;
    Ok(report)
}

/// U12: project existing daemon diagnostics, including journals whose process record is
/// absent. Inspection neither contacts the daemon nor supplies recovery or capacity proof.
fn journal_reports(root: &Path) -> anyhow::Result<Vec<super::super::docker::daemon::JournalReport>> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut reports = entries.map(|entry| super::super::docker::daemon::Journal::at(entry?.path()).report()).collect::<anyhow::Result<Vec<_>>>()?;
    reports.sort_by(|a, b| a.journal.cmp(&b.journal));
    Ok(reports)
}

fn outstanding_journals(root: &Path) -> anyhow::Result<bool> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        // Use the daemon's own proof projection. Closed, discharged journals are harmless;
        // pending, open, malformed or unreadable journals remain separate physical fences.
        if super::super::docker::daemon::Journal::at(entry?.path()).drained().is_err() { return Ok(true) }
    }
    Ok(false)
}

fn reconcile(root: &Path, current: Option<&str>, apply: bool, daemon: bool) -> anyhow::Result<Report> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Report { operation: "", mode: if apply { "after-cold-boot" } else { "inspect" }, cleared: 0, evidence: Vec::new(), docker_ownership: Vec::new(), daemon_journals: Vec::new() }),
        Err(e) => return Err(e.into()),
    };
    let mut evidence = Vec::new();
    let mut clear = Vec::new();
    for entry in entries {
        let fence = entry?.path();
        let checked = (|| -> anyhow::Result<(Legacy, String)> {
            let saved: Legacy = serde_json::from_slice(&std::fs::read(&fence)?)?;
            anyhow::ensure!(saved.source.is_absolute(), "legacy source is not absolute");
            let parent = saved.source.parent().and_then(Path::file_name).and_then(|p| p.to_str());
            anyhow::ensure!(matches!(parent, Some("processes" | "pgids")), "source is not a legacy process ownership record");
            let pid = saved.source.file_name().and_then(|p| p.to_str()).and_then(|p| p.split('.').next()).and_then(|p| p.parse::<i32>().ok());
            anyhow::ensure!(pid.is_some_and(|pid| pid > 0), "legacy source has no valid process-group identity");
            let name = crate::util::sha256_hex(saved.source.to_string_lossy().as_bytes());
            anyhow::ensure!(fence.file_stem().and_then(|p| p.to_str()) == Some(name.as_str()), "legacy fence does not name its recorded source");
            anyhow::ensure!(saved.sha256.len() == 64 && saved.sha256.bytes().all(|c| c.is_ascii_hexdigit()), "legacy source digest is invalid");
            match std::fs::read(&saved.source) {
                Ok(bytes) => anyhow::ensure!(crate::util::sha256_hex(&bytes) == saved.sha256, "legacy source changed; retaining its evidence"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
                Err(e) => return Err(e.into()),
            }
            let previous = saved.boot_id.as_deref().context("legacy evidence has no authoritative kernel boot identity")?;
            let now = current.context("this platform has no supported authoritative kernel boot identity")?;
            anyhow::ensure!(valid_boot(previous) && valid_boot(now), "invalid kernel boot identity");
            anyhow::ensure!(previous != now, "the kernel boot has not changed; PID or marker absence cannot prove termination");
            anyhow::ensure!(!daemon, "persistent daemon ownership remains; kernel cold boot cannot prove restartable or remote workloads stopped");
            Ok((saved, "a changed kernel boot proves the recorded local processes cannot execute".into()))
        })();
        let (source, recorded_boot, eligible, diagnostic) = match checked {
            Ok((saved, diagnostic)) => {
                clear.push((fence.clone(), saved.source.clone()));
                (Some(saved.source), saved.boot_id, true, diagnostic)
            }
            Err(error) => {
                let saved = std::fs::read(&fence).ok().and_then(|b| serde_json::from_slice::<Legacy>(&b).ok());
                (saved.as_ref().map(|s| s.source.clone()), saved.and_then(|s| s.boot_id), false, format!("{error:#}"))
            }
        };
        evidence.push(Evidence { fence, source, recorded_boot, current_boot: current.map(str::to_owned), eligible, diagnostic });
    }
    evidence.sort_by(|a, b| a.fence.cmp(&b.fence));
    if apply {
        let denied = evidence.iter().filter(|e| !e.eligible).map(|e| format!("{}: {}", e.fence.display(), e.diagnostic)).collect::<Vec<_>>();
        anyhow::ensure!(denied.is_empty(), "legacy ownership remains fenced:\n{}", denied.join("\n"));
        for (fence, source) in &clear { super::remove(source)?; super::remove(fence)?; }
    }
    Ok(Report { operation: "", mode: if apply { "after-cold-boot" } else { "inspect" }, cleared: if apply { clear.len() } else { 0 }, evidence, docker_ownership: Vec::new(), daemon_journals: Vec::new() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u12_local_inspection_reports_unresolved_daemon_intents_without_mutation() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("daemon");
        assert!(journal_reports(&root)?.is_empty());
        assert!(!root.exists(), "inspection does not initialize daemon admission");
        let journal = root.join("orphan");
        std::fs::create_dir_all(&journal)?;
        let pending = journal.join("unknown.pending");
        std::fs::write(&pending, b"legacy unknown outcome")?;
        let reports = journal_reports(&root)?;
        let report = reports.first().context("orphan daemon report")?;
        assert_eq!(report.admission, "open");
        assert!(report.resource_removal_proof_required);
        let intent = report.unresolved.first().context("unresolved daemon intent")?;
        assert_eq!(intent.evidence, pending);
        assert_eq!(intent.state, "unknown_outcome_without_generation_proof");
        assert_eq!(std::fs::read(&pending)?, b"legacy unknown outcome");
        assert!(!journal.join("closed").exists(), "inspection does not quiesce a journal");
        Ok(())
    }

    #[test]
    fn u03_legacy_reconciliation_requires_cold_kernel_and_exact_source() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("legacy");
        let source = dir.path().join("pgids/123");
        std::fs::create_dir_all(source.parent().context("fixture parent")?)?;
        let fence = root.join(format!("{}.json", crate::util::sha256_hex(source.to_string_lossy().as_bytes())));
        let old = "11111111-1111-1111-1111-111111111111";
        let new = "22222222-2222-2222-2222-222222222222";
        std::fs::write(&source, b"old ownership")?;
        super::super::save(&fence, &serde_json::json!({ "source": source, "sha256": crate::util::sha256_hex(b"old ownership"), "boot_id": old }))?;
        let report = reconcile(&root, Some(new), false, false)?;
        assert_eq!(report.cleared, 0);
        assert!(source.exists() && fence.exists(), "inspection is read-only");
        assert!(reconcile(&root, Some(old), true, false).is_err());
        assert!(reconcile(&root, Some(new), true, true).is_err(), "kernel proof does not discharge daemon ownership");
        std::fs::write(&source, b"new ownership")?;
        assert!(reconcile(&root, Some(new), true, false).is_err(), "a changed source is another owner");
        assert!(source.exists() && fence.exists());
        std::fs::write(&source, b"old ownership")?;
        let report = reconcile(&root, Some(new), true, false)?;
        assert_eq!(report.cleared, 1);
        assert!(!source.exists() && !fence.exists());
        Ok(())
    }
}

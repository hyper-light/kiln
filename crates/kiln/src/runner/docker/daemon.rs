//! U03: submission evidence for work owned by an external daemon. A negative list is only
//! useful after admission is closed and every submitted operation has a known outcome.
//! These files extend the execution's physical fence; they are not a capacity ledger.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use parking_lot::Mutex;

static GATES: LazyLock<Mutex<BTreeMap<PathBuf, Arc<Mutex<()>>>>> = LazyLock::new(|| Mutex::new(BTreeMap::new()));

#[derive(Clone, Debug)]
pub(crate) struct Journal { admission: super::super::processes::Admission, path: PathBuf, gate: Arc<Mutex<()>> }

/// U03: only closed admission with no unresolved submission can issue this proof. There is
/// deliberately no recovery API accepting a negative list, elapsed time or an operator bool.
/// A recovered pending intent remains fenced until its original operation supplies completion.
pub(crate) struct Drained { _private: () }

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct Generation { boot: String, endpoint: PathBuf, daemon_id: String }
impl Generation { pub(crate) fn daemon_id(&self) -> &str { &self.daemon_id } }

#[derive(serde::Serialize, serde::Deserialize)]
struct Intent { generation: Option<Generation>, #[serde(default, skip_serializing_if = "Option::is_none")] cleanup_workspace: Option<PathBuf> }

#[derive(Debug, serde::Serialize)]
pub(crate) struct JournalReport {
    pub journal: PathBuf,
    pub admission: &'static str,
    pub unresolved: Vec<IntentReport>,
    pub resource_removal_proof_required: bool,
    pub diagnostic: &'static str,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct IntentReport {
    pub evidence: PathBuf,
    pub state: &'static str,
    pub local_endpoint: Option<PathBuf>,
    pub daemon_id: Option<String>,
    pub boot_generation: Option<String>,
    pub cleanup_workspace: Option<PathBuf>,
}

/// A pinned Linux socket inode prevents a pathname swap between peer verification and the
/// CLI's actual connection. The retained descriptor names the exact verified listener.
#[derive(Debug)]
pub(crate) struct LocalPin {
    #[cfg(target_os = "linux")]
    _inode: std::fs::File,
    pub path: PathBuf,
    pub generation: Generation,
}

impl LocalPin {
    pub(crate) async fn capture(endpoint: &Path) -> anyhow::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd as _;
            use std::os::unix::fs::OpenOptionsExt as _;
            let inode = std::fs::OpenOptions::new().read(true).custom_flags(nix::libc::O_PATH | nix::libc::O_CLOEXEC).open(endpoint)?;
            let path = PathBuf::from(format!("/proc/{}/fd/{}", std::process::id(), inode.as_raw_fd()));
            let stream = hyper_rt::local::LocalStream::connect(&path).await?;
            let generation = verified_generation(&stream, endpoint).await?;
            Ok(Self { _inode: inode, path, generation })
        }
        #[cfg(not(target_os = "linux"))]
        { let _ = endpoint; anyhow::bail!("U03: verified local daemon boot-generation recovery is supported only on Linux") }
    }
}

/// The opaque proof comes from the actual connected peer and a completed /info response,
/// never a caller-supplied epoch, Docker version, socket absence or an operator boolean.
pub(crate) async fn verified_generation(stream: &hyper_rt::local::LocalStream, endpoint: &Path) -> anyhow::Result<Generation> {
    #[cfg(target_os = "linux")]
    {
        let pid = stream.peer()?.pid.ok_or_else(|| anyhow::anyhow!("U03: local daemon peer has no kernel process identity"))?;
        let executable = std::fs::read_link(format!("/proc/{pid}/exe"))?;
        anyhow::ensure!(executable.file_name().and_then(|n| n.to_str()) == Some("dockerd"), "U03: Unix endpoint is not a direct local Docker daemon; no boot-generation proof available");
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim().to_owned();
        anyhow::ensure!(uuid::Uuid::parse_str(&boot).is_ok(), "U03: host boot generation is unavailable or invalid");
        let daemon_id = super::super::docker_sock::daemon_id(stream).await?;
        Ok(Generation { boot, endpoint: endpoint.to_owned(), daemon_id })
    }
    #[cfg(not(target_os = "linux"))]
    { let _ = (stream, endpoint); anyhow::bail!("U03: this platform provides no supported local daemon boot-generation proof") }
}

impl Journal {
    pub(crate) fn execution(label: &str) -> Self { Self::execution_in(super::super::processes::Admission::production(), label) }

    pub(crate) fn execution_in(admission: super::super::processes::Admission, label: &str) -> Self {
        let path = admission.root().join("daemon").join(crate::util::sha256_hex(label.as_bytes()));
        Self::at_in(path, admission)
    }

    pub(crate) fn ensure_execution(&self, admission: &super::super::processes::Admission, label: &str) -> anyhow::Result<()> {
        anyhow::ensure!(&self.admission == admission && self.path == admission.root().join("daemon").join(crate::util::sha256_hex(label.as_bytes())), "U03: managed Docker journal does not belong to this execution/admission owner");
        Ok(())
    }

    pub(crate) fn at(path: PathBuf) -> Self { Self::at_in(path, super::super::processes::Admission::production()) }

    pub(crate) fn at_in(path: PathBuf, admission: super::super::processes::Admission) -> Self {
        let gate = GATES.lock().entry(path.clone()).or_insert_with(|| Arc::new(Mutex::new(()))).clone();
        Self { admission, path, gate }
    }

    #[cfg(test)]
    pub(crate) fn begin(&self) -> anyhow::Result<Operation> {
        self.begin_with(None)
    }

    pub(crate) fn begin_with(&self, generation: Option<Generation>) -> anyhow::Result<Operation> {
        self.begin_intent(Intent { generation, cleanup_workspace: None })
    }

    pub(crate) fn reserve_workspace(&self, workspace: PathBuf, generation: Option<Generation>) -> anyhow::Result<Operation> {
        self.begin_intent(Intent { generation, cleanup_workspace: Some(workspace) })
    }

    fn begin_intent(&self, intent: Intent) -> anyhow::Result<Operation> {
        let _gate = self.gate.lock();
        self.check_open()?;
        std::fs::create_dir_all(&self.path)?;
        let path = self.path.join(format!("{}.pending", crate::util::new_id()));
        // No arguments, environment, request bodies or registry credentials enter this file.
        persist(&path, &serde_json::to_vec(&intent)?)?;
        Ok(Operation { path, admission: self.admission.clone() })
    }

    fn check_open(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.path.join("closed").try_exists()?, "U03: Docker execution is quiesced; refusing a future daemon mutation");
        Ok(())
    }

    pub(crate) fn ensure_open(&self) -> anyhow::Result<()> { let _gate = self.gate.lock(); self.check_open() }

    /// U03: read-only evidence for `kiln runner reconcile`. No CLI/daemon contact, gate
    /// mutation, credential-store access, request arguments or request bodies are involved.
    pub(crate) fn report(&self) -> anyhow::Result<JournalReport> {
        let _gate = self.gate.lock();
        let mut unresolved = Vec::new();
        let exists = self.path.try_exists()?;
        let closed = exists && self.path.join("closed").try_exists()?;
        if exists {
            for entry in std::fs::read_dir(&self.path)? {
                let path = entry?.path();
                if path.file_name().and_then(|n| n.to_str()) == Some("closed") { continue }
                let bytes = match std::fs::read(&path) {
                    Ok(bytes) => bytes,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e.into()),
                };
                let intent = serde_json::from_slice::<Intent>(&bytes).ok();
                let cleanup_workspace = intent.as_ref().and_then(|i| i.cleanup_workspace.clone());
                let generation = intent.and_then(|i| i.generation);
                unresolved.push(IntentReport {
                    evidence: path,
                    state: if cleanup_workspace.is_some() { "reserved_or_submitted_workspace_cleanup" } else if generation.is_some() { "awaiting_completion_or_verified_local_generation" } else { "unknown_outcome_without_generation_proof" },
                    local_endpoint: generation.as_ref().map(|g| g.endpoint.clone()),
                    daemon_id: generation.as_ref().map(|g| g.daemon_id.clone()),
                    boot_generation: generation.map(|g| g.boot),
                    cleanup_workspace,
                });
            }
        }
        unresolved.sort_by(|a, b| a.evidence.cmp(&b.evidence));
        Ok(JournalReport {
            journal: self.path.clone(), admission: if closed { "closed" } else if exists { "open" } else { "not_initialized" }, unresolved,
            resource_removal_proof_required: true,
            diagnostic: "Request completion or verified local boot change proves only no future request commit. Capacity additionally requires closed admission, drain proof before and after a successful labelled-resource removal sweep, and kernel termination proof. Remote pods and intentionally external deployments are not covered by local Docker boot evidence. No age, operator assertion or manual evidence deletion supplies proof.",
        })
    }

    pub(crate) fn recover_generation(&self, current: &Generation) -> anyhow::Result<usize> {
        let _gate = self.gate.lock();
        anyhow::ensure!(self.path.join("closed").try_exists()?, "U03: generation recovery requires closed daemon admission");
        let mut recovered = 0;
        for entry in std::fs::read_dir(&self.path)? {
            let path = entry?.path();
            if path.extension().and_then(|n| n.to_str()) != Some("pending") { continue }
            let intent = serde_json::from_slice::<Intent>(&std::fs::read(&path)?).ok();
            let Some(previous) = intent.and_then(|i| i.generation) else { continue };
            if uuid::Uuid::parse_str(&previous.boot).is_err() || previous.daemon_id.is_empty() || !previous.endpoint.is_absolute() { continue }
            // The persistent daemon ID pins its data root. A same-boot daemon restart or a
            // different daemon cannot discharge a request; all old local producers must be
            // gone, and the same daemon must have restored its durable labelled objects.
            if previous.boot != current.boot && previous.endpoint == current.endpoint && previous.daemon_id == current.daemon_id {
                std::fs::remove_file(&path)?;
                sync_parent(&path)?;
                recovered += 1;
            }
        }
        Ok(recovered)
    }

    pub(crate) fn quiesce(&self) -> anyhow::Result<()> {
        let _gate = self.gate.lock();
        std::fs::create_dir_all(&self.path)?;
        let closed = self.path.join("closed");
        if !closed.try_exists()? { persist(&closed, b"U03: no future daemon submissions\n")?; }
        Ok(())
    }

    pub(crate) fn drained(&self) -> anyhow::Result<Drained> {
        self.drained_except(None)
    }

    pub(crate) fn drained_except_reserved(&self, reserved: &Operation) -> anyhow::Result<Drained> {
        anyhow::ensure!(reserved.path.parent() == Some(self.path.as_path()), "U03: cleanup reservation belongs to a different execution journal");
        self.drained_except(Some(&reserved.path))
    }

    fn drained_except(&self, reserved: Option<&PathBuf>) -> anyhow::Result<Drained> {
        let _gate = self.gate.lock();
        anyhow::ensure!(self.path.join("closed").try_exists()?, "U03: Docker submission admission has not closed");
        for entry in std::fs::read_dir(&self.path)? {
            let path = entry?.path();
            anyhow::ensure!(path.file_name().and_then(|n| n.to_str()) == Some("closed") || reserved == Some(&path),
                "U03: Docker operation has an unknown or active outcome at {}; retaining physical fence and recovery intent: an empty daemon list cannot prove no future commit. Recovery waits for its original response owner, or a verified new host boot generation at the same direct local daemon endpoint and persistent daemon ID, followed by labelled cleanup. Remote/unverified/legacy intents remain fenced without supported proof; never erase their evidence manually", path.display());
        }
        Ok(Drained { _private: () })
    }
}

/// Drop deliberately preserves the intent. Cancellation, a failed transport, and a crash
/// cannot distinguish a rejected request from one which will commit later in the daemon.
#[derive(Debug)]
pub(crate) struct Operation { path: PathBuf, admission: super::super::processes::Admission }
impl Operation {
    pub(crate) fn completed(self) -> anyhow::Result<()> {
        self.retire()
    }

    /// A gathered socket request never forwarded its header, or an unused prepared command
    /// never launched. This exact non-submission proof cannot be replaced by an empty list.
    pub(crate) fn not_submitted(self) -> anyhow::Result<()> {
        self.retire()
    }

    fn retire(self) -> anyhow::Result<()> {
        std::fs::remove_file(&self.path)?;
        sync_parent(&self.path)
    }
}

pub(crate) struct Reply { pub success: bool, pub stdout: Vec<u8>, pub stderr: Vec<u8> }

/// U03: the response owner outlives the caller's cancellation/deadline. The original Docker
/// Group/journal retains the allocation; this companion containment owns only the CLI which
/// must finish draining its request. Killing that CLI early would destroy our only proof.
pub(crate) async fn drain(command: std::process::Command, operation: Operation, work: PathBuf) -> anyhow::Result<Reply> {
    let execution = super::super::processes::Execution::cleanup(operation.admission.clone(), work);
    let scope = execution.clone();
    let launched = crate::rt::blocking(move || super::super::processes::spawn_duplex(command, &scope)).await;
    let result = async {
        let duplex = launched??;
        duplex.stdin.close();
        async fn collect(input: async_channel::Receiver<Vec<u8>>) -> Vec<u8> {
            let mut bytes = Vec::new();
            while let Ok(chunk) = input.recv().await { bytes.extend(chunk); }
            bytes
        }
        let (stdout, stderr, code) = futures::join!(collect(duplex.stdout), collect(duplex.stderr), duplex.exit);
        let code = code.map_err(|_| anyhow::anyhow!("U03: Docker request owner lost its CLI exit proof; retaining unknown-outcome recovery intent"))?;
        Ok::<_, anyhow::Error>(Reply { success: code == Some(0), stdout, stderr })
    }.await;
    // No descendant of the response-owning CLI can issue a later request once its marker
    // retires. This is the existing kernel containment proof, not a separate controller.
    execution.stop().await;
    let reply = result?;
    if reply.success { operation.completed()?; }
    Ok(reply)
}

fn persist(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    sync_parent(path)
}
fn sync_parent(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    for parent in path.ancestors().skip(1) {
        std::fs::File::open(parent)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u03_unknown_daemon_commit_survives_quiesce_and_recovery() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?.keep();
        let admission = super::super::super::processes::Admission::private(&dir)?;
        let journal = Journal::at_in(dir.join("execution"), admission.clone());
        let delayed = journal.begin()?;
        journal.quiesce()?;
        assert!(journal.begin().is_err(), "no new mutation can pass cleanup");
        assert!(journal.drained().is_err(), "negative lists cannot discharge an in-flight create");
        let recovered = Journal::at_in(dir.join("execution"), admission);
        assert!(recovered.drained().is_err(), "restart preserves the unknown-outcome fence");
        delayed.completed()?;
        recovered.drained()?;
        Ok(())
    }

    #[test]
    fn u03_cancelled_daemon_request_keeps_recovery_intent() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?.keep();
        let admission = super::super::super::processes::Admission::private(&dir)?;
        let journal = Journal::at_in(dir.join("execution"), admission.clone());
        drop(journal.begin()?);
        journal.quiesce()?;
        assert!(journal.drained().is_err());
        Ok(())
    }

    #[test]
    fn u03_reserved_workspace_cleanup_never_reopens_daemon_admission() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?.keep();
        let admission = super::super::super::processes::Admission::private(&dir)?;
        let journal = Journal::at_in(dir.join("execution"), admission.clone());
        let cleanup = journal.begin()?;
        let earlier = journal.begin()?;
        journal.quiesce()?;
        assert!(journal.begin().is_err());
        assert!(journal.drained_except_reserved(&cleanup).is_err(), "a reserved utility cannot race an earlier unknown create");
        earlier.completed()?;
        journal.drained_except_reserved(&cleanup)?;
        assert!(journal.drained().is_err(), "the cleanup reservation still holds capacity");
        cleanup.not_submitted()?;
        journal.drained()?;
        Ok(())
    }

    #[test]
    fn u03_generation_recovery_requires_new_boot_and_exact_local_daemon() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?.keep();
        let admission = super::super::super::processes::Admission::private(&dir)?;
        let journal = Journal::at_in(dir.join("execution"), admission.clone());
        let previous = Generation { boot: "00000000-0000-4000-8000-000000000001".into(), endpoint: dir.join("docker.sock"), daemon_id: "fixture-daemon".into() };
        drop(journal.begin_with(Some(previous.clone()))?);
        journal.quiesce()?;
        assert_eq!(journal.recover_generation(&previous)?, 0, "daemon restart within the same boot is not proof");
        let mut current = Generation { boot: "00000000-0000-4000-8000-000000000002".into(), ..previous.clone() };
        current.daemon_id = "different-data-root".into();
        assert_eq!(journal.recover_generation(&current)?, 0);
        current.daemon_id = previous.daemon_id.clone();
        current.endpoint = dir.join("other.sock");
        assert_eq!(journal.recover_generation(&current)?, 0);
        assert!(journal.drained().is_err());
        current.endpoint = previous.endpoint;
        assert_eq!(journal.recover_generation(&current)?, 1);
        journal.drained()?;
        Ok(())
    }

    #[test]
    fn u03_unverified_unknown_outcome_cannot_use_boot_recovery() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?.keep();
        let admission = super::super::super::processes::Admission::private(&dir)?;
        let journal = Journal::at_in(dir.join("execution"), admission.clone());
        drop(journal.begin()?);
        journal.quiesce()?;
        let current = Generation { boot: "00000000-0000-4000-8000-000000000002".into(), endpoint: dir.join("docker.sock"), daemon_id: "fixture-daemon".into() };
        assert_eq!(journal.recover_generation(&current)?, 0);
        assert!(journal.drained().is_err(), "remote, unverified and legacy records never receive fabricated generation evidence");
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn u03_cli_cancel_detaches_waiter_but_preserves_response_owner() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?.keep();
            let admission = super::super::super::processes::Admission::private(&dir)?;
            let journal = Journal::at_in(dir.join("execution"), admission.clone());
            let operation = journal.begin()?;
            let release = dir.join("allow-response");
            let mut command = std::process::Command::new("sh");
            command.args(["-c", "while [ ! -e \"$1\" ]; do sleep 0.01; done; printf drained", "sh"]).arg(&release);
            let work = dir.join("owner");
            let task = super::super::super::processes::within(None, || crate::rt::spawn(drain(command, operation, work)));
            drop(task); // Exactly how the caller's canceled/deadline-expired waiter detaches.
            journal.quiesce()?;
            assert!(journal.drained().is_err(), "request ownership survives cancellation");
            std::fs::write(release, b"respond")?;
            crate::rt::timeout(std::time::Duration::from_secs(5), async {
                while journal.drained().is_err() { crate::rt::sleep(std::time::Duration::from_millis(10)).await; }
            }).await.map_err(|_| anyhow::anyhow!("the fixture's original response owner never drained"))?;
            journal.drained()?;
            Ok(())
        })
    }
}

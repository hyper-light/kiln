//! SBOMs of a tree (docs/ci.md W18-R03): every lockfile under a directory, through
//! `kiln_ci::sbom`, and what `syft` finds of other ecosystems when it is on `PATH`, as one
//! SPDX 2.3 or CycloneDX 1.6 document. `kiln sbom [DIR]` prints one; `kiln/attest@v1` and
//! `attest: { sbom: true }` attest one for their subjects.

use std::path::{Path, PathBuf};
use std::time::Duration;

use kiln_ci::sbom::{self, Doc, Lock, Package};

/// Directories never searched for lockfiles: what tools install or build.
const SKIP: &[&str] = &[".git", "node_modules", "target", ".venv", "venv", "vendor", "__pycache__", ".tox", "dist", "build"];

/// The lockfiles under `dir`, with what each pins. In a directory with both, `go.sum` is read,
/// not `go.mod`.
pub fn scan(dir: &Path) -> Vec<(PathBuf, Vec<Package>)> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        let mut files: Vec<(PathBuf, Lock)> = Vec::new();
        for e in entries.filter_map(Result::ok) {
            let (path, name) = (e.path(), e.file_name().to_string_lossy().into_owned());
            match e.file_type() {
                Ok(t) if t.is_dir() && !SKIP.contains(&name.as_str()) && !name.starts_with('.') => todo.push(path),
                Ok(t) if t.is_file() => {
                    if let Some(kind) = sbom::kind_of(&name) {
                        files.push((path, kind));
                    }
                }
                _ => {}
            }
        }
        let has_sum = files.iter().any(|(_, k)| *k == Lock::GoSum);
        for (path, kind) in files {
            if kind == Lock::GoMod && has_sum {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            match sbom::parse(kind, &text) {
                Ok(p) if !p.is_empty() => out.push((path, p)),
                Ok(_) => {}
                Err(e) => tracing::warn!(file = %path.display(), error = %e, "a lockfile couldn't be read"),
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).unwrap_or_default()
}

/// What `syft` finds under `dir` of the ecosystems kiln reads no lockfile of, when it is on
/// `PATH` (`KILN_SBOM_SYFT=0` leaves it out).
pub async fn syft(dir: &Path) -> Vec<Package> {
    if std::env::var("KILN_SBOM_SYFT").is_ok_and(|v| v == "0") {
        return Vec::new();
    }
    let Some(bin) = crate::os::which("syft") else { return Vec::new() };
    let mut c = std::process::Command::new(bin);
    c.arg("scan").arg(format!("dir:{}", dir.display())).args(["-o", "spdx-json", "-q"]);
    match crate::proc::output(c, Duration::from_secs(600)).await {
        Ok(o) if o.status.success() => match serde_json::from_slice::<serde_json::Value>(&o.stdout) {
            Ok(doc) => sbom::from_spdx(&doc).into_iter().filter(|p| !p.eco.built_in()).collect(),
            Err(e) => {
                tracing::warn!(error = %e, "syft's document couldn't be read");
                Vec::new()
            }
        },
        Ok(o) => {
            tracing::warn!(stderr = %String::from_utf8_lossy(&o.stderr).trim(), "syft failed");
            Vec::new()
        }
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "syft didn't finish");
            Vec::new()
        }
    }
}

/// The SBOM of `dir` as `format` (`spdx` or `cyclonedx`), named `name`.
pub async fn document(dir: &Path, name: &str, version: Option<&str>, format: &str) -> anyhow::Result<serde_json::Value> {
    let d = dir.to_path_buf();
    let mut packages: Vec<Package> = crate::rt::blocking(move || scan(&d)).await?.into_iter().flat_map(|(_, p)| p).collect();
    packages.extend(syft(dir).await);
    let doc = Doc { name: name.to_owned(), version: version.map(str::to_owned), created: now_rfc3339(), tool: format!("kiln-{}", env!("CARGO_PKG_VERSION")), packages };
    let id = uuid::Uuid::now_v7().hyphenated().to_string();
    match format {
        "spdx" | "spdx-json" => Ok(sbom::spdx(&doc, &format!("https://kiln.dev/spdx/{}/{id}", name.replace(['/', ' '], "-")))),
        "cyclonedx" | "cdx" | "cyclonedx-json" => Ok(sbom::cyclonedx(&doc, &format!("urn:uuid:{id}"))),
        other => anyhow::bail!("`{other}`: an SBOM is `spdx` or `cyclonedx`"),
    }
}

/// `kiln sbom`: the packages a tree's lockfiles pin, as an SBOM.
#[derive(clap::Args)]
pub struct SbomArgs {
    /// The tree (the current directory by default).
    dir: Option<PathBuf>,
    /// `spdx` (SPDX 2.3) or `cyclonedx` (CycloneDX 1.6).
    #[arg(long, default_value = "spdx")]
    format: String,
    /// What it describes (the directory's name by default).
    #[arg(long)]
    name: Option<String>,
    /// Write it here instead of printing it.
    #[arg(long, short)]
    out: Option<PathBuf>,
}

pub async fn cli(a: SbomArgs) -> anyhow::Result<()> {
    let dir = a.dir.unwrap_or_else(|| PathBuf::from("."));
    let name = a.name.unwrap_or_else(|| std::fs::canonicalize(&dir).ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_else(|| "sbom".into()));
    let v = document(&dir, &name, None, &a.format).await?;
    let text = serde_json::to_string_pretty(&v)?;
    match a.out {
        Some(p) => std::fs::write(&p, text + "\n")?,
        None => println!("{text}"),
    }
    Ok(())
}

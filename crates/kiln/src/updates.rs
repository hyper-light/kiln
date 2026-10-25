//! Upgrading kiln from its own UI (System → Updates).
//!
//! * **Runners**, without a restart and without losing anything, always through
//!   `kiln install --agent --upgrade` on the machine: a runner from before in-place upgrades gets
//!   a new runner beside it (the old one keeps its terminals, agent sessions and jobs, and is
//!   retired once they end; see `crate::upgrade`), a newer one takes the new binary in place.
//!   Runners that announce [`CAP_UPGRADE`] are sent [`ServerMsg::Upgrade`] and do it themselves.
//!   Older ones are upgraded through a terminal session kiln opens on them for the purpose (never
//!   listed as anyone's terminal) that downloads the binary from this server, checks it and runs
//!   the same command, so a runner that knows nothing of this can still be upgraded.
//!   Runners are brought to this server's build (the one it speaks the protocol of); machines of
//!   another platform get the matching build: one uploaded here, else this version's GitHub
//!   release, fetched once and then served from here.
//! * **The server**: in Kubernetes, a rollout restart of its Deployment (the new pod starts on the
//!   image now on the node; a check pod reads which build that image holds, so kiln can say
//!   whether restarting changes anything). Installed as a service under `kiln supervise`, it
//!   replaces its own binary (an uploaded one, or a newer GitHub release) and the supervisor
//!   restarts it.
//!
//! Jobs live in memory (the latest few) and reach the UI as `update_job` events; `updates`
//! events say the overview changed.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use parking_lot::Mutex;
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::hub::Hub;
use crate::proto::{AgentMsg, CAP_UPGRADE, ServerMsg};

/// Jobs kept for the UI.
const KEEP_JOBS: usize = 40;
/// Output lines kept per job.
const KEEP_LINES: usize = 400;
/// How long `install --agent --upgrade` may take on a runner. How long the server waits on an
/// upgrade is [`Waits`].
const INSTALL_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// How long the new runner has to report in after its install finished.
const REPORT_IN: Duration = Duration::from_secs(90);
/// How often GitHub is asked about releases (it allows 60 unauthenticated requests an hour).
const RELEASE_TTL: Duration = Duration::from_secs(15 * 60);
/// How long an image check stays good.
const IMAGE_TTL: Duration = Duration::from_secs(10 * 60);
/// Largest binary accepted for upload or from a release.
pub const MAX_BINARY: usize = 256 << 20;
/// Terminal sessions kiln opens to upgrade older runners carry this in their command: they are
/// never listed (no one's terminal tab), also after a restart that adopts them back.
pub const HIDDEN_MARK: &str = "# kiln-upgrade ";
/// What a terminal-driven upgrade prints when it ends, for the job to read.
pub(crate) const OK_MARK: &str = "KILN-UPGRADE-OK";
pub(crate) const FAIL_MARK: &str = "KILN-UPGRADE-FAIL";
/// Labels of the pod that reads which build an image holds. Deliberately not
/// `app.kubernetes.io/managed-by=kiln`: the job-pod reaper owns those.
const CHECK_SELECTOR: &str = "kiln.dev/role=image-check";

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Running,
    Done,
    Failed,
}

/// One upgrade, as the UI follows it.
#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub id: String,
    /// "runner" or "server".
    pub kind: &'static str,
    /// The machine, or "server".
    pub target: String,
    /// "native" (the runner's own upgrade command), "terminal" (through a terminal on an older
    /// runner), "build" (the machine builds the newest kiln itself, `crate::fleet`), "restart"
    /// (Kubernetes rollout) or "replace" (the server's binary).
    pub method: &'static str,
    pub state: JobState,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub from_build: Option<String>,
    pub to_build: Option<String>,
    /// The commit it brings the machine to, when kiln knows it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// A build's: the machine that builds it, when another than the runner's own
    /// (`crate::fleet`); the runner downloads that build.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on: Option<String>,
    /// The machine reported running `to_build`.
    pub reached: bool,
    pub log: Vec<String>,
    pub error: Option<String>,
    /// Kiln stopped waiting for it: which of its [`Waits`] ran out ("upgrade", "jobs" or "build").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timed_out: Option<&'static str>,
    /// Its last log line is a progress line (a terminal's carriage return draws it over): the
    /// next line takes its place.
    #[serde(skip)]
    progress: bool,
    /// The runner upgrades blue/green ([`crate::proto::CAP_BLUE_GREEN`]), and (`handed_over`) it
    /// said the new build took over.
    #[serde(skip)]
    blue_green: bool,
    #[serde(skip)]
    handed_over: bool,
}

/// A binary kiln can hand out for a platform.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct Binary {
    pub target: String,
    pub sha256: String,
    pub size: u64,
    /// "server" (this server's own), "upload", "release" (a GitHub release), "built" (by kiln's
    /// own release pipeline, `run` and `sha`) or "machine" (a machine built it of itself, `by`
    /// and `sha`; `crate::fleet`).
    pub origin: String,
    pub at: i64,
    #[serde(default)]
    pub build: String,
    /// The run of the release pipeline that built it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<i64>,
    /// The commit it was built from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    /// The machine that built it of itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ReleaseInfo {
    /// The latest release's version, when there is one.
    pub latest: Option<String>,
    /// It is newer than this server.
    pub newer: bool,
    pub url: Option<String>,
    pub error: Option<String>,
    /// 0: not asked yet.
    pub checked_at: i64,
}

/// Which build the server's image holds on its node (Kubernetes).
#[derive(Debug, Clone, Serialize, Default)]
pub struct ImageCheck {
    pub image: String,
    pub node: Option<String>,
    pub build: Option<String>,
    /// It differs from the running server's build: a restart starts that one.
    pub newer: bool,
    pub error: Option<String>,
    /// 0: not checked yet.
    pub checked_at: i64,
}

#[derive(Default)]
pub struct Updates {
    jobs: Mutex<VecDeque<Job>>,
    release: Mutex<Option<(Instant, ReleaseInfo)>>,
    image: Mutex<Option<(Instant, ImageCheck)>>,
    checking_release: AtomicBool,
    checking_image: AtomicBool,
    /// A push to the release branch: the server's commit may be there now (`in_step_loop`).
    in_step_wake: crate::rt::Notify,
    /// How long upgrades are waited on, once read (`waits`).
    waits: Mutex<Option<Waits>>,
    /// Every runner on the newest kiln.
    pub fleet: crate::fleet::Fleet,
    /// The binaries kept for other platforms, by target, as kiln's store of bytes holds them
    /// ([`load`]).
    kept: Mutex<HashMap<String, Binary>>,
}

impl Updates {
    /// The release branch moved (a push kiln saw): look again at once whether every platform can
    /// be built from the server's commit, rather than at the next tick.
    pub fn wake_in_step(&self) {
        self.in_step_wake.notify();
    }
}

/// A short, stable id of a build: the start of its binary's SHA-256.
pub fn build_id(sha256_hex: &str) -> String {
    sha256_hex.get(..12).unwrap_or(sha256_hex).to_owned()
}

/// This binary's SHA-256 (hex) and size, computed once.
pub fn own_binary() -> Result<(String, u64), String> {
    static OWN: OnceLock<Result<(String, u64), String>> = OnceLock::new();
    OWN.get_or_init(|| {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        hash_file(&exe).map_err(|e| format!("{e:#}"))
    })
    .clone()
}

/// Reads this build's hash ([`own_binary`]) once, off the runtime. The runner and the server ask
/// for it from async code without waiting (`own_build()`), and the first ask reads the whole
/// binary: seconds for a test build, a runtime shard and every task on it held that long.
pub async fn warm_own_binary() {
    let _ = crate::rt::blocking(own_binary).await;
}

/// This binary's build id ("unknown" if it can't be read): the build it was before kiln signed it
/// on this Mac, when it did (`crate::os::macsign`).
pub fn own_build() -> String {
    own_binary().map_or_else(
        |_| "unknown".to_owned(),
        |(h, _)| std::env::current_exe().ok().and_then(|p| crate::os::macsign::build_of(&p, &h)).unwrap_or_else(|| build_id(&h)),
    )
}

pub(crate) fn hash_file(p: &Path) -> anyhow::Result<(String, u64)> {
    use std::io::Read as _;
    let mut f = std::fs::File::open(p).with_context(|| format!("open {}", p.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut size = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        size += n as u64;
        h.update(buf.get(..n).unwrap_or_default());
    }
    Ok((hex::encode(h.finalize()), size))
}

/// The release build for an OS and architecture (Linux gets the static one).
fn release_target(os: &str, arch: &str) -> Option<&'static str> {
    Some(match (os, arch) {
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        ("windows", "aarch64") => "aarch64-pc-windows-msvc",
        _ => return None,
    })
}

/// The build a machine of this OS and architecture is given: this server's own binary when it
/// is of the same platform, else the release build for it.
pub fn target_for(os: &str, arch: &str) -> Option<String> {
    if os == std::env::consts::OS && arch == std::env::consts::ARCH {
        return Some(crate::api::TARGET.to_owned());
    }
    release_target(os, arch).map(str::to_owned)
}

/// The release asset that could replace this server's binary.
fn own_release_target() -> Option<&'static str> {
    let t = crate::api::TARGET;
    crate::api::RELEASE_TARGETS.iter().copied().find(|r| *r == t).or_else(|| release_target(std::env::consts::OS, std::env::consts::ARCH))
}

fn release_asset_url(version: &str, target: &str) -> String {
    let exe = if target.contains("-windows-") { ".exe" } else { "" };
    format!("{}/v{version}/kiln-{version}-{target}{exe}", crate::api::RELEASES)
}

/// Platforms a binary may be kept for.
pub fn known_target(target: &str) -> bool {
    target == crate::api::TARGET || crate::api::RELEASE_TARGETS.contains(&target)
}

// ------------------------------------------------------------------ binaries kept here

/// Binaries uploaded here, fetched from a release or built are kept in kiln's store of bytes
/// (docs/blobs.md): the binary at `binaries/kiln-<target>`, what kiln knows of it at
/// `binaries/kiln-<target>.json`. They were `<data>/binaries/kiln-<target>` and its `.json`; the
/// local store takes those where they lie ([`take_in`]).
pub fn binary_key(target: &str) -> String {
    format!("binaries/kiln-{target}")
}

fn meta_key(target: &str) -> String {
    format!("binaries/kiln-{target}.json")
}

/// Where binaries were kept before they were in kiln's store of bytes.
fn old_dir(cfg: &crate::config::Config) -> PathBuf {
    cfg.server.data_dir.join("binaries")
}

/// The binary kept here for `target`, if any.
pub fn stored(hub: &Hub, target: &str) -> Option<Binary> {
    known_target(target).then(|| hub.updates.kept.lock().get(target).cloned()).flatten()
}

/// Reads what the store keeps of each platform's binary into [`Updates`]: a binary whose bytes
/// aren't whole there is left out. Whether every one could be read (one the store couldn't answer
/// for is read again later: [`load_until_read`]).
pub async fn load(hub: &Hub) -> bool {
    load_into(hub.blobs.blobs.as_ref(), &hub.updates.kept).await
}

/// [`load`] from `blobs` into `kept`.
async fn load_into(blobs: &dyn kiln_seams::blobs::Blobs, kept: &Mutex<HashMap<String, Binary>>) -> bool {
    use kiln_seams::blobs::Refusal;
    // Each platform the store answered for: its binary, or none; one it couldn't is left as it is.
    let mut answered: Vec<(&str, Option<Binary>)> = Vec::new();
    let mut targets: Vec<&str> = crate::api::RELEASE_TARGETS.to_vec();
    if !targets.contains(&crate::api::TARGET) {
        targets.push(crate::api::TARGET);
    }
    let all = targets.len();
    for t in targets {
        let meta = match crate::blobs::io::get_all(blobs, &meta_key(t), 1 << 20).await {
            Ok((_, bytes)) => serde_json::from_slice::<Binary>(&bytes).ok(),
            Err(Refusal::NotFound(_)) => None,
            Err(e @ Refusal::Unavailable(_)) => {
                tracing::warn!(target: "kiln.blobs", event = "get", key = %meta_key(t), error = %e, "a kept binary couldn't be read: read again later");
                continue;
            }
            Err(e) => {
                tracing::warn!(target: "kiln.blobs", event = "get", key = %meta_key(t), error = %e, "a kept binary couldn't be read");
                None
            }
        };
        let Some(meta) = meta else {
            answered.push((t, None));
            continue;
        };
        match blobs.head(&binary_key(t)).await {
            Ok(m) if m.size == meta.size && m.etag == meta.sha256 => answered.push((t, Some(meta))),
            Err(Refusal::Unavailable(e)) => tracing::warn!(target: "kiln.blobs", event = "get", key = %binary_key(t), error = %e, "a kept binary couldn't be read: read again later"),
            Ok(_) | Err(_) => {
                tracing::warn!(target: "kiln.blobs", event = "get", key = %binary_key(t), "a kept binary isn't whole: left out");
                answered.push((t, None));
            }
        }
    }
    let read = answered.len() == all;
    let mut kept = kept.lock();
    for (t, b) in answered {
        match b {
            Some(b) => kept.insert(t.to_owned(), b),
            None => kept.remove(t),
        };
    }
    read
}

/// [`load`] (at once, when `now`; else a minute on), and again a while apart (a minute, doubling to
/// a quarter of an hour), until the store answers for every binary: one unavailable when the server
/// started isn't left out for good.
pub async fn load_until_read(hub: Arc<Hub>, now: bool) {
    let mut wait = Duration::from_secs(60);
    if !now {
        crate::rt::sleep(wait).await;
    }
    while !load(&hub).await {
        crate::rt::sleep(wait).await;
        wait = wait.saturating_mul(2).min(Duration::from_secs(900));
    }
    tracing::info!(target: "kiln.blobs", event = "get", "the kept binaries are all read");
}

/// Takes the binaries kept as files (`<data>/binaries`) into the local store, where they lie: each
/// one the store doesn't hold for its platform, or holds an older one of (an older kiln run on this
/// store again, a rollback, kept a newer one). Each start; a file taken stays where it lies.
pub async fn take_in(hub: &Hub, local: &crate::blobs::local::LocalBlobs) -> anyhow::Result<()> {
    take_in_from(&hub.cfg, local).await
}

/// [`take_in`] for the server `cfg` names (`kiln blobs copy`).
pub async fn take_in_from(cfg: &crate::config::Config, local: &crate::blobs::local::LocalBlobs) -> anyhow::Result<()> {
    use kiln_seams::blobs::{Blobs as _, Refusal};
    let dir = old_dir(cfg);
    if !dir.is_dir() {
        return Ok(());
    }
    let d = dir.clone();
    // Each platform's binary kept as a file whole, its modification time, and what kiln knew of it.
    let found = crate::rt::blocking(move || -> Vec<(&'static str, PathBuf, PathBuf, i64, Binary)> {
        let mut out = Vec::new();
        for t in crate::api::RELEASE_TARGETS.iter().copied().chain([crate::api::TARGET]) {
            let (bin, meta) = (d.join(format!("kiln-{t}")), d.join(format!("kiln-{t}.json")));
            let Some(m) = std::fs::read(&meta).ok().and_then(|b| serde_json::from_slice::<Binary>(&b).ok()) else { continue };
            let Ok(md) = std::fs::metadata(&bin) else { continue };
            let at = crate::blobs::local::modified_ms(&md);
            if md.len() == m.size && !out.iter().any(|(k, ..): &(&str, PathBuf, PathBuf, i64, Binary)| *k == t) {
                out.push((t, bin, meta, at, m));
            }
        }
        out
    })
    .await?;
    let mut files = Vec::new();
    for (t, bin, meta, at, m) in found {
        match local.head(&binary_key(t)).await {
            // The file taken before, or the same binary kept another way; or a newer one kept.
            Ok(kept) if kept.etag == m.sha256 || kept.modified_ms >= at => continue,
            // An older kiln kept a newer one: it's the platform's.
            Ok(_) => {
                for k in [binary_key(t), meta_key(t)] {
                    match local.delete(&k).await {
                        Ok(()) | Err(Refusal::NotFound(_)) => {}
                        Err(e) => anyhow::bail!("{k}: {e}"),
                    }
                }
            }
            Err(Refusal::NotFound(_)) => {}
            Err(e) => anyhow::bail!("{}: {e}", binary_key(t)),
        }
        files.push((binary_key(t), bin, Some(m.sha256.clone())));
        files.push((meta_key(t), meta, None));
    }
    let taken = local.adopt(files).await?.taken;
    if taken > 0 {
        tracing::info!(target: "kiln.blobs", event = "adopt", taken, "runner binaries kept as files taken in where they lie");
    }
    Ok(())
}

/// The binary kept for `target` as a stream, with what kiln knows of it.
pub fn stream(hub: &Hub, target: &str) -> Option<(Binary, async_channel::Receiver<bytes::Bytes>)> {
    let b = stored(hub, target)?;
    let rx = crate::blobs::io::read_stream(Arc::clone(&hub.blobs.blobs), binary_key(target), Some(b.sha256.clone()), 0, b.size.saturating_sub(1));
    Some((b, rx))
}

/// It looks like an executable kiln could hand out (ELF, Mach-O or PE).
fn executable(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x7fELF") || bytes.starts_with(b"MZ") || bytes.starts_with(&[0xcf, 0xfa, 0xed, 0xfe]) || bytes.starts_with(&[0xca, 0xfe, 0xba, 0xbe])
}

/// Keep `bytes` as the binary for `target` (an upload, or a release download).
pub async fn store(hub: &Hub, target: &str, bytes: Vec<u8>, origin: &str) -> anyhow::Result<Binary> {
    keep(hub, target, bytes, origin, None, None).await
}

/// Keep the binary `machine` built of `commit` for its own platform (`crate::fleet`): what the
/// other machines of the platform download.
pub async fn keep_built(hub: &Hub, target: &str, bytes: Vec<u8>, machine: &str, commit: &str) -> anyhow::Result<Binary> {
    keep(hub, target, bytes, "machine", None, Some((machine.to_owned(), commit.to_ascii_lowercase()))).await
}

/// Whether a run's binaries become what runners upgrade to: kiln's own release pipeline, built
/// from a release tag or from the release branch, and from the commit this server runs (`own`;
/// any, when it was built without knowing one), so no platform runs ahead of or behind it. A
/// manual run on any other branch (trying out a change) builds, but nothing upgrades to it.
pub fn adopts(u: &crate::config::Updates, run: &crate::store::Run, own: &str) -> bool {
    run.repo_name == u.release_repo
        && run.pipeline == u.release_pipeline
        && (run.event == "tag" || run.branch == u.release_branch)
        && (own.is_empty() || run.sha.eq_ignore_ascii_case(own))
}

/// What the release pipeline's job left in its artifacts (`dist/kiln-<target>[.exe]`): kept as the
/// binaries each other platform's runners upgrade to. This server's own platform keeps its own
/// build from a run of the server's commit; a run of a newer one (`crate::fleet`) brings that
/// platform's too.
pub async fn adopt_into(hub: &Hub, artifacts: &Path, run: &crate::store::Run) -> anyhow::Result<Vec<Binary>> {
    let (a, r) = (artifacts.to_path_buf(), run.clone());
    let found = crate::rt::blocking(move || binaries_in(&a, &r)).await??;
    let mut kept = Vec::new();
    for (target, bytes) in found {
        kept.push(keep(hub, &target, bytes, "built", Some((run.number, run.sha.clone())), None).await?);
    }
    Ok(kept)
}

/// The binaries in a release run's artifacts, by target, as [`adopt_into`] takes them.
fn binaries_in(artifacts: &Path, run: &crate::store::Run) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
    use std::io::Read as _;
    let mut archive = tar::Archive::new(std::fs::File::open(artifacts).with_context(|| format!("open {}", artifacts.display()))?);
    let mut kept = Vec::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().into_owned();
        let name = path.rsplit('/').next().unwrap_or(&path);
        let Some(target) = name.strip_prefix("kiln-").map(|t| t.strip_suffix(".exe").unwrap_or(t)) else { continue };
        if !known_target(target) || (target == crate::api::TARGET && run.sha.eq_ignore_ascii_case(crate::api::COMMIT)) || entry.size() > MAX_BINARY as u64 {
            continue;
        }
        let mut bytes = Vec::with_capacity(usize::try_from(entry.size()).unwrap_or(0));
        entry.read_to_end(&mut bytes)?;
        // An archive cut short (its upload broke off) reads as a shorter file, with no error: never
        // one runners would upgrade to. Nor what isn't its platform's program.
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != entry.size() {
            tracing::warn!(target, run = run.number, got = bytes.len(), size = entry.size(), "a release binary cut short in its run's artifacts: not kept");
            break;
        }
        if !runs_on(target, &bytes) {
            tracing::warn!(target, run = run.number, "a release binary that isn't its platform's program: not kept");
            continue;
        }
        kept.push((target.to_owned(), bytes));
    }
    Ok(kept)
}

/// Whether `bytes` is a program `target` runs: an ELF on Linux, a Mach-O (or a universal one) on
/// macOS, a PE on Windows, by its first bytes.
fn runs_on(target: &str, bytes: &[u8]) -> bool {
    let head = bytes.get(..4).unwrap_or_default();
    if target.contains("linux") {
        head == b"\x7fELF"
    } else if target.contains("apple-darwin") {
        // 64-bit Mach-O, either way round, or a universal (fat) one.
        [[0xcf, 0xfa, 0xed, 0xfe], [0xfe, 0xed, 0xfa, 0xcf], [0xca, 0xfe, 0xba, 0xbe]].iter().any(|m| head == m)
    } else if target.contains("windows") {
        head.get(..2) == Some(b"MZ".as_slice())
    } else {
        true
    }
}

// ------------------------------------------------------------------ every platform in step

/// How often the other platforms' binaries are checked against the server's commit.
const IN_STEP_EVERY: Duration = Duration::from_secs(5 * 60);
/// A build of the server's commit that failed is tried again after this long.
const IN_STEP_RETRY: Duration = Duration::from_secs(30 * 60);
/// At most this many builds of one commit.
const IN_STEP_TRIES: usize = 3;
/// A build that couldn't start, because the commit isn't in the repository yet (deployed before
/// it was pushed), is started again after this long, for as long as it takes.
const IN_STEP_WAIT: Duration = Duration::from_secs(10 * 60);

/// The other platforms' binaries against the commit this server runs.
#[derive(Debug, Clone, Serialize, Default, PartialEq, Eq)]
pub struct InStep {
    /// The commit this server is built from; None: built without one (`KILN_COMMIT`), so nothing
    /// can be kept in step with it.
    pub commit: Option<String>,
    /// Every other platform's binary kept here is built from that commit.
    pub in_step: bool,
    /// The platforms whose binary isn't (all of them, when none is kept yet).
    pub behind: Vec<String>,
}

/// The other platforms' binaries kept here against `commit`. This server's own platform is one
/// of them when `commit` is a newer one than the server's (`crate::fleet`).
pub fn in_step(hub: &Hub, commit: &str) -> InStep {
    let own = commit.eq_ignore_ascii_case(crate::api::COMMIT);
    let others: Vec<&str> = crate::api::RELEASE_TARGETS.iter().copied().filter(|t| !(own && *t == crate::api::TARGET)).collect();
    let kept: Vec<Binary> = others.iter().filter_map(|t| stored(hub, t)).collect();
    let mut s = in_step_of(&others, &kept, commit);
    if !own && !kept.iter().any(|b| b.target == crate::api::TARGET) && !s.behind.iter().any(|t| t == crate::api::TARGET) {
        s.behind.push(crate::api::TARGET.to_owned());
        s.in_step = false;
    }
    s
}

fn in_step_of(others: &[&str], kept: &[Binary], commit: &str) -> InStep {
    let behind: Vec<String> = if kept.is_empty() {
        others.iter().map(|t| (*t).to_owned()).collect()
    } else {
        kept.iter().filter(|b| !b.sha.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(commit))).map(|b| b.target.clone()).collect()
    };
    InStep { in_step: !commit.is_empty() && behind.is_empty(), commit: (!commit.is_empty()).then(|| commit.to_owned()), behind }
}

/// The last build of the server's commit that couldn't start, here (it leaves no run).
#[derive(Debug, Default)]
struct Starts {
    /// Unix seconds.
    last: Option<i64>,
    /// Release jobs whose binaries were kept here already (`keep_in_step`).
    kept: std::collections::HashSet<String>,
}

impl Starts {
    /// The newest kiln moved to `commit`: kept for it afresh. Another commit's binaries may have
    /// been kept since this one's were (a server just started, its own commit not yet fetched,
    /// takes a runner's older one as the newest for a moment): its runs' are kept again.
    fn moved(&mut self, commit: &str, was: &mut String) {
        if commit != was {
            self.last = None;
            self.kept.clear();
            was.clone_from(&commit.to_owned());
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Next {
    /// One is on its way or done, or builds of it failed enough.
    Nothing,
    /// The last try was too recent.
    Wait,
    Build,
}

/// What to do about the server's commit, from the release runs of it and the last start that
/// failed. Only builds that ran and failed count against it: a commit that isn't pushed yet is
/// waited for.
fn next(runs: &[crate::store::Run], starts: &Starts, now: i64) -> Next {
    if runs.iter().any(|r| !r.status.is_terminal() || r.status.is_success()) || runs.len() >= IN_STEP_TRIES {
        return Next::Nothing;
    }
    let within = |at: Option<i64>, d: Duration| at.is_some_and(|t| now.saturating_sub(t) < i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    let ran = runs.iter().map(|r| r.finished_at.unwrap_or(r.created_at)).max();
    if within(ran, IN_STEP_RETRY) || within(starts.last, IN_STEP_WAIT) { Next::Wait } else { Next::Build }
}

fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

/// Runs the release pipeline on `commit` of the release branch. Its binaries are what runners of
/// every other platform upgrade to once it succeeds (`adopts`).
pub async fn build_commit(hub: &Hub, repo: &crate::store::Repo, commit: &str) -> anyhow::Result<crate::store::Run> {
    let u = &hub.cfg.updates;
    let t = crate::gitops::Trigger { event: "manual", branch: u.release_branch.clone(), sha: commit.to_owned(), before: None, pipeline: Some(u.release_pipeline.clone()), payload: None };
    let runs = crate::gitops::trigger(hub, repo, t).await?;
    runs.into_iter().next().ok_or_else(|| anyhow::anyhow!("{} has no pipeline called {} at {}", u.release_repo, u.release_pipeline, short(commit)))
}

/// Keeps every platform in step with the server (`[updates] in_step`): whenever the binaries kept
/// for the other platforms aren't built from the commit this server runs, it runs the release
/// pipeline on that commit, so every runner can be upgraded to exactly what the server runs. A
/// commit not in the repository yet is tried again later.
pub async fn in_step_loop(hub: Arc<Hub>) -> anyhow::Result<()> {
    use futures::FutureExt as _;
    if crate::api::COMMIT.is_empty() || !hub.cfg.updates.in_step {
        hub.shutdown.cancelled().await;
        return Ok(());
    }
    let mut tick = crate::rt::Interval::new(IN_STEP_EVERY);
    let mut starts = Starts::default();
    let mut was = String::new();
    loop {
        futures::select_biased! {
            () = hub.shutdown.cancelled().fuse() => return Ok(()),
            // Pushed: a commit that wasn't in the repository may be now, so no waiting.
            () = hub.updates.in_step_wake.notified().fuse() => starts.last = None,
            () = tick.tick().fuse() => {}
        }
        // The newest kiln (`crate::fleet`): the server's commit, or a newer one a runner runs.
        let commit = hub.updates.fleet.target().map_or_else(|| crate::api::COMMIT.to_owned(), |t| t.commit);
        starts.moved(&commit, &mut was);
        if let Err(e) = keep_in_step(&hub, &commit, &mut starts).await {
            tracing::warn!(commit = short(&commit), error = %format!("{e:#}"), "couldn't build every platform from the newest commit");
        }
    }
}

async fn keep_in_step(hub: &Hub, commit: &str, starts: &mut Starts) -> anyhow::Result<()> {
    if in_step(hub, commit).in_step {
        return Ok(());
    }
    let u = &hub.cfg.updates;
    let repo = hub.store.repos().await?.into_iter().find(|r| r.name == u.release_repo).ok_or_else(|| anyhow::anyhow!("kiln doesn't build {} (no repository of that name)", u.release_repo))?;
    let runs: Vec<crate::store::Run> =
        hub.store.runs(Some(repo.id.clone()), 200, None).await?.into_iter().filter(|r| r.pipeline == u.release_pipeline && r.sha.eq_ignore_ascii_case(commit)).collect();
    // Built when it was pushed, before this server ran it (nothing then took it): kept now.
    for run in &runs {
        for job in hub.store.jobs(run.id.clone(), false).await.unwrap_or_default() {
            if job.status == crate::store::Status::Succeeded && starts.kept.insert(job.id.clone()) {
                hub.adopt_release(&job.id, run).await;
            }
        }
    }
    if in_step(hub, commit).in_step || next(&runs, starts, crate::util::now_unix()) != Next::Build {
        return Ok(());
    }
    match build_commit(hub, &repo, commit).await {
        Ok(run) => {
            tracing::info!(run = run.number, commit = short(commit), "building every platform from the server's commit");
            hub.store.audit("kiln".into(), "upgrade.build".into(), format!("{} {} #{} for the server's commit {}", u.release_repo, u.release_pipeline, run.number, short(commit))).await;
            hub.emit(&serde_json::json!({ "type": "updates" }));
            Ok(())
        }
        Err(e) => {
            starts.last = Some(crate::util::now_unix());
            Err(e)
        }
    }
}

/// Keeps `bytes` as the binary for `target` in kiln's store of bytes: the binary, then what kiln
/// knows of it; a release run's (`built`), or a machine's own build (`by`: the machine and commit).
async fn keep(hub: &Hub, target: &str, bytes: Vec<u8>, origin: &str, built: Option<(i64, String)>, by: Option<(String, String)>) -> anyhow::Result<Binary> {
    anyhow::ensure!(known_target(target), "kiln has no platform called {target}");
    anyhow::ensure!(bytes.len() > 1 << 20, "that file is too small to be a kiln binary");
    anyhow::ensure!(executable(&bytes), "that file is not an executable (ELF, Mach-O or PE)");
    let size = bytes.len() as u64;
    let blobs = &hub.blobs.blobs;
    let kept = blobs.put(&binary_key(target), bytes, kiln_seams::blobs::Put::default()).await.map_err(|e| anyhow::anyhow!("keeping the binary for {target}: {e}"))?;
    let (run, sha) = built.map_or((None, None), |(n, s)| (Some(n), Some(s)));
    let mut meta = Binary { target: target.to_owned(), build: build_id(&kept.etag), sha256: kept.etag, size, origin: origin.to_owned(), at: crate::util::now_unix(), run, sha, by: None };
    if let Some((machine, commit)) = by {
        meta.sha = Some(commit);
        meta.by = Some(machine);
    }
    blobs.put(&meta_key(target), serde_json::to_vec(&meta)?, kiln_seams::blobs::Put::default()).await.map_err(|e| anyhow::anyhow!("keeping what kiln knows of the binary for {target}: {e}"))?;
    hub.updates.kept.lock().insert(target.to_owned(), meta.clone());
    Ok(meta)
}

/// Forget the binary kept for `target`.
pub async fn unstore(hub: &Hub, target: &str) -> bool {
    if !known_target(target) {
        return false;
    }
    let had = hub.updates.kept.lock().remove(target).is_some();
    for k in [meta_key(target), binary_key(target)] {
        if let Err(e) = hub.blobs.blobs.delete(&k).await {
            tracing::warn!(target: "kiln.blobs", event = "collect", key = %k, error = %e, "a binary let go couldn't be deleted");
        }
    }
    had
}

/// Every binary kiln can hand out right now.
pub fn binaries(hub: &Hub) -> Vec<Binary> {
    let mut v = Vec::new();
    if let Ok((sha256, size)) = own_binary() {
        let sha = (!crate::api::COMMIT.is_empty()).then(|| crate::api::COMMIT.to_owned());
        v.push(Binary { target: crate::api::TARGET.to_owned(), build: build_id(&sha256), sha256, size, origin: "server".to_owned(), at: 0, run: None, sha, by: None });
    }
    let mut targets: Vec<&str> = crate::api::RELEASE_TARGETS.to_vec();
    if !targets.contains(&crate::api::TARGET) {
        targets.push(crate::api::TARGET);
    }
    for t in targets {
        // An upload already installed here is the server's own binary: listed once.
        if let Some(b) = stored(hub, t)
            && !v.iter().any(|x| x.target == b.target && x.sha256 == b.sha256)
        {
            v.push(b);
        }
    }
    v
}

/// GET with redirects followed (GitHub release assets redirect to their storage).
pub(crate) async fn get_following(client: &crate::http::client::Client, url: &str, timeout: Duration) -> anyhow::Result<crate::http::client::Response> {
    let mut url = url.to_owned();
    for _ in 0..6 {
        let resp = client.get(&url).timeout(timeout).send().await?;
        if (300..400).contains(&resp.status())
            && let Some(loc) = resp.header("location")
        {
            url = loc.to_owned();
            continue;
        }
        return Ok(resp);
    }
    anyhow::bail!("too many redirects from {url}")
}

/// Download a release asset (`None`: the release has no such asset).
async fn download_release(version: &str, target: &str) -> anyhow::Result<Option<Vec<u8>>> {
    let url = release_asset_url(version, target);
    let client = crate::http::client::Client::public()?;
    let resp = get_following(&client, &url, Duration::from_secs(300)).await.with_context(|| format!("download {url}"))?;
    if resp.status() == 404 {
        return Ok(None);
    }
    anyhow::ensure!(resp.is_success(), "GitHub answered {} for {url}", resp.status());
    Ok(Some(resp.bytes(MAX_BINARY).await?.to_vec()))
}

/// The binary for `target`, made available at `/dl/kiln/<target>`: this server's own, one kept
/// here, else this version's release fetched from GitHub. Returns (sha256, size).
pub async fn ensure_binary(hub: &Hub, target: &str) -> anyhow::Result<(String, u64)> {
    if target == crate::api::TARGET {
        return own_binary().map_err(|e| anyhow::anyhow!("can't read this server's own binary: {e}"));
    }
    if let Some(b) = stored(hub, target) {
        return Ok((b.sha256, b.size));
    }
    let version = env!("CARGO_PKG_VERSION");
    let Some(bytes) = download_release(version, target).await? else {
        anyhow::bail!(
            "kiln has no binary for {target}: this server is {}, none was uploaded for {target}, and GitHub has no v{version} release with one. Upload a kiln binary for {target} under System → Builds.",
            crate::api::TARGET
        );
    };
    let b = store(hub, target, bytes, "release").await?;
    Ok((b.sha256, b.size))
}

// ------------------------------------------------------------------ checks

/// The latest GitHub release as last seen, asking again in the background when that is stale.
pub fn release(hub: &Arc<Hub>, force: bool) -> ReleaseInfo {
    let cached = hub.updates.release.lock().clone();
    let stale = cached.as_ref().is_none_or(|(at, _)| at.elapsed() > RELEASE_TTL);
    if (stale || force) && !hub.updates.checking_release.swap(true, Ordering::AcqRel) {
        let h = Arc::clone(hub);
        crate::rt::spawn(async move {
            let r = fetch_release()
                .await
                .unwrap_or_else(|e| ReleaseInfo { error: Some(format!("{e:#}")), checked_at: crate::util::now_unix(), ..ReleaseInfo::default() });
            *h.updates.release.lock() = Some((Instant::now(), r));
            h.updates.checking_release.store(false, Ordering::Release);
            h.emit(&serde_json::json!({ "type": "updates" }));
        });
    }
    cached.map(|(_, r)| r).unwrap_or_default()
}

async fn fetch_release() -> anyhow::Result<ReleaseInfo> {
    let client = crate::http::client::Client::public()?;
    let resp = client
        .get("https://api.github.com/repos/hyper-light/kiln/releases/latest")
        .header("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(15))
        .send()
        .await?;
    let now = crate::util::now_unix();
    if resp.status() == 404 {
        // No release published (or the repository isn't public).
        return Ok(ReleaseInfo { checked_at: now, ..ReleaseInfo::default() });
    }
    anyhow::ensure!(resp.is_success(), "GitHub answered {}", resp.status());
    #[derive(serde::Deserialize)]
    struct Rel {
        tag_name: String,
        html_url: Option<String>,
    }
    let rel: Rel = resp.json(4 << 20).await?;
    let latest = rel.tag_name.trim_start_matches('v').to_owned();
    Ok(ReleaseInfo { newer: newer_than(&latest, env!("CARGO_PKG_VERSION")), latest: Some(latest), url: rel.html_url, error: None, checked_at: now })
}

/// Semantic-version comparison of `a` against `b` (a pre-release is older than its release).
pub fn newer_than(a: &str, b: &str) -> bool {
    fn parts(v: &str) -> (Vec<u64>, bool) {
        let (core, pre) = v.split_once('-').map_or((v, false), |(c, _)| (c, true));
        (core.split('.').map(|p| p.parse::<u64>().unwrap_or(0)).collect(), pre)
    }
    let (pa, pre_a) = parts(a);
    let (pb, pre_b) = parts(b);
    match pa.cmp(&pb) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => !pre_a && pre_b,
    }
}

/// Which build the server's image holds, as last checked; checked again in the background when
/// that is stale (Kubernetes only).
pub fn image(hub: &Arc<Hub>, force: bool) -> Option<ImageCheck> {
    kube_identity()?;
    let cached = hub.updates.image.lock().clone();
    let stale = cached.as_ref().is_none_or(|(at, _)| at.elapsed() > IMAGE_TTL);
    if (stale || force) && !hub.updates.checking_image.swap(true, Ordering::AcqRel) {
        let h = Arc::clone(hub);
        crate::rt::spawn(async move {
            let node = std::env::var("NODE_NAME").ok().filter(|n| !n.is_empty());
            let image = h.cfg.kubernetes.agent_image.clone();
            let c = match check_image(&h, &image, node.as_deref()).await {
                Ok(c) => c,
                Err(e) => ImageCheck { image, node, error: Some(format!("{e:#}")), checked_at: crate::util::now_unix(), ..ImageCheck::default() },
            };
            *h.updates.image.lock() = Some((Instant::now(), c));
            h.updates.checking_image.store(false, Ordering::Release);
            h.emit(&serde_json::json!({ "type": "updates" }));
        });
    }
    Some(cached.map(|(_, c)| c).unwrap_or_default())
}

/// The image check pod: the image's own `sha256sum` over its kiln binary, as small and locked
/// down as a pod gets (it passes the `restricted` Pod Security level and the job quota).
fn check_pod(name: &str, image: &str, pull_policy: &str, node: Option<&str>, priority_class: Option<&str>) -> serde_json::Value {
    let mut spec = serde_json::json!({
        "restartPolicy": "Never",
        "automountServiceAccountToken": false,
        "activeDeadlineSeconds": 120,
        "terminationGracePeriodSeconds": 0,
        "securityContext": { "runAsNonRoot": true, "seccompProfile": { "type": "RuntimeDefault" } },
        "containers": [{
            "name": "check",
            "image": image,
            "imagePullPolicy": pull_policy,
            "command": ["sha256sum", "/usr/local/bin/kiln"],
            "resources": { "requests": { "cpu": "10m", "memory": "16Mi" }, "limits": { "memory": "64Mi" } },
            "securityContext": { "allowPrivilegeEscalation": false, "readOnlyRootFilesystem": true, "capabilities": { "drop": ["ALL"] } }
        }]
    });
    if let Some(obj) = spec.as_object_mut() {
        if let Some(n) = node {
            obj.insert("nodeName".into(), n.into());
        }
        if let Some(pc) = priority_class {
            obj.insert("priorityClassName".into(), pc.into());
        }
    }
    serde_json::json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": { "name": name, "labels": { "kiln.dev/role": "image-check", "app.kubernetes.io/name": "kiln-image-check" } },
        "spec": spec,
    })
}

/// Start a short pod on the server's node with the server's image (as a restart would) and read
/// the SHA-256 of the kiln binary in it. Uses the job namespace and the access kiln already has
/// there (create pods, read their logs).
async fn check_image(hub: &Hub, image: &str, node: Option<&str>) -> anyhow::Result<ImageCheck> {
    let k = &hub.cfg.kubernetes;
    anyhow::ensure!(!image.is_empty(), "no kiln image is configured (kubernetes.agent_image)");
    let api = crate::kubeapi::KubeApi::discover(&k.namespace)?;
    // Leftovers of a check that never finished.
    for p in api.list_pods(CHECK_SELECTOR).await.unwrap_or_default() {
        if let Some(n) = p.metadata.name {
            let _ = api.delete_pod(&n).await;
        }
    }
    let id = crate::util::new_id();
    let name = format!("kiln-image-check-{}", id.get(id.len().saturating_sub(8)..).unwrap_or("x"));
    let pod: k8s_openapi::api::core::v1::Pod = serde_json::from_value(check_pod(&name, image, &k.image_pull_policy, node, k.priority_class.as_deref()))?;
    api.create_pod(&pod).await.context("start the image check pod")?;
    let out = async {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            crate::rt::sleep(Duration::from_secs(1)).await;
            let Some(p) = api.get_pod(&name).await? else { anyhow::bail!("the check pod disappeared") };
            let status = p.status.unwrap_or_default();
            match status.phase.as_deref() {
                Some("Succeeded") => break,
                Some("Failed") => anyhow::bail!("the check pod failed: {}", status.message.unwrap_or_default()),
                _ => {}
            }
            let waiting = status.container_statuses.iter().flatten().filter_map(|c| c.state.as_ref()?.waiting.as_ref()).find_map(|w| {
                let r = w.reason.clone().unwrap_or_default();
                matches!(r.as_str(), "ErrImagePull" | "ImagePullBackOff" | "ErrImageNeverPull" | "InvalidImageName" | "CreateContainerError")
                    .then(|| format!("{r}: {}", w.message.clone().unwrap_or_default()))
            });
            if let Some(w) = waiting {
                anyhow::bail!("the image can't start: {w}");
            }
            if Instant::now() > deadline {
                anyhow::bail!("the check pod didn't finish in 90 s");
            }
        }
        api.pod_log(&name).await
    }
    .await;
    let _ = api.delete_pod(&name).await;
    let log = out?;
    let sha = log.split_whitespace().next().unwrap_or_default();
    anyhow::ensure!(sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit()), "unexpected output from the check pod: {}", log.trim());
    let build = build_id(sha);
    Ok(ImageCheck { image: image.to_owned(), node: node.map(str::to_owned), newer: build != own_build(), build: Some(build), error: None, checked_at: crate::util::now_unix() })
}

// ------------------------------------------------------------------ jobs

impl Updates {
    pub(crate) fn insert(&self, job: Job) {
        let mut jobs = self.jobs.lock();
        jobs.push_back(job);
        while jobs.len() > KEEP_JOBS {
            jobs.pop_front();
        }
    }

    /// Newest first.
    pub fn jobs(&self) -> Vec<Job> {
        self.jobs.lock().iter().rev().cloned().collect()
    }

    pub fn job(&self, id: &str) -> Option<Job> {
        self.jobs.lock().iter().find(|j| j.id == id).cloned()
    }

    pub(crate) fn running(&self, id: &str) -> bool {
        self.jobs.lock().iter().any(|j| j.id == id && j.state == JobState::Running)
    }

    pub(crate) fn reached(&self, id: &str) -> bool {
        self.jobs.lock().iter().any(|j| j.id == id && j.reached)
    }

    /// A runner that reports the commit job `id` brings it to, on a build other than the one it
    /// ran before, has arrived as well. Its machine signed the build again (a Mac's `kiln
    /// signing`), which changes its bytes and so the build it reports.
    pub(crate) fn reached_commit(&self, hub: &Hub, id: &str, machine: &str) {
        let Some(j) = self.jobs.lock().iter().find(|j| j.id == id && !j.reached && j.commit.is_some()).cloned() else { return };
        let build = crate::fleet::newest_build(hub, machine);
        let commit = build.as_deref().and_then(|b| crate::fleet::runner_commit(hub, machine, Some(b)));
        if let (true, Some(build), Some(commit)) = (arrived_by_commit(&j, build.as_deref(), commit.as_deref()), build, commit) {
            self.update(hub, id, |j| {
                j.reached = true;
                push_line(j, format!("{machine} is running {} (build {build}, signed again on the machine)", crate::fleet::short(&commit)));
            });
        }
    }

    /// The upgrade is through: the new build reported in and, on a runner that upgrades
    /// blue/green, the old one said it took over.
    pub(crate) fn through(&self, id: &str) -> bool {
        self.jobs.lock().iter().any(|j| j.id == id && j.reached && (!j.blue_green || j.handed_over))
    }

    /// A job already running for `target`.
    pub(crate) fn running_for(&self, kind: &str, target: &str) -> Option<Job> {
        self.jobs.lock().iter().find(|j| j.kind == kind && j.target == target && j.state == JobState::Running).cloned()
    }

    /// Change a job and tell the UI.
    fn update(&self, hub: &Hub, id: &str, f: impl FnOnce(&mut Job)) {
        let snapshot = {
            let mut jobs = self.jobs.lock();
            let Some(j) = jobs.iter_mut().find(|j| j.id == id) else { return };
            f(j);
            j.clone()
        };
        hub.emit(&serde_json::json!({ "type": "update_job", "job": snapshot }));
    }

    pub(crate) fn say(&self, hub: &Hub, id: &str, line: impl Into<String>) {
        let line = line.into();
        self.update(hub, id, |j| push_line(j, line));
    }

    /// The line a terminal is drawing over (a progress bar): shown last, in place of the one before.
    pub(crate) fn progress(&self, hub: &Hub, id: &str, line: impl Into<String>) {
        let line = line.into();
        self.update(hub, id, |j| {
            push_line(j, line);
            j.progress = true;
        });
    }

    /// The progress line showing, if any, goes: what was drawn there finished as a hidden line.
    pub(crate) fn end_progress(&self, hub: &Hub, id: &str) {
        if self.jobs.lock().iter().any(|j| j.id == id && j.progress) {
            self.update(hub, id, |j| {
                if std::mem::take(&mut j.progress) {
                    j.log.pop();
                }
            });
        }
    }

    /// A build job's machine says which build it made: the job is reached when that reports in.
    pub(crate) fn set_to_build(&self, hub: &Hub, id: &str, build: &str) {
        self.update(hub, id, |j| j.to_build = Some(build.to_owned()));
    }

    pub(crate) fn finish(&self, hub: &Hub, id: &str, result: Result<String, String>) {
        let now = crate::util::now_unix();
        let error = result.as_ref().err().cloned();
        self.update(hub, id, |j| {
            if j.state != JobState::Running {
                return;
            }
            j.ended_at = Some(now);
            match result {
                Ok(note) => {
                    j.state = JobState::Done;
                    if !note.is_empty() {
                        push_line(j, note);
                    }
                }
                Err(e) => {
                    j.state = JobState::Failed;
                    push_line(j, format!("failed: {e}"));
                    j.error = Some(e);
                }
            }
        });
        if let Some(j) = self.job(id) {
            crate::fleet::on_job_end(hub, &j, error.as_deref());
        }
        hub.emit(&serde_json::json!({ "type": "updates" }));
    }

    /// Kiln stops waiting for it: its `which` wait ([`Waits`]) ran out.
    pub(crate) fn time_out(&self, hub: &Hub, id: &str, which: &'static str, error: String) {
        self.update(hub, id, |j| j.timed_out = Some(which));
        self.finish(hub, id, Err(error));
    }

    /// [`AgentMsg::UpgradeLog`].
    pub fn on_log(&self, hub: &Hub, job: &str, line: String) {
        self.say(hub, job, line);
    }

    /// [`AgentMsg::UpgradeDone`]. A success completes the job once the new build has reported
    /// in (the job's driver waits for that); a failure completes it now. One kiln had stopped
    /// waiting for is upgraded after all once it is through ([`Self::late`]).
    pub fn on_done(&self, hub: &Hub, job: &str, ok: bool, error: Option<String>) {
        if ok {
            self.update(hub, job, |j| {
                j.handed_over = j.blue_green;
                push_line(j, if j.blue_green { "the new build took over" } else { "the runner finished installing" }.to_owned());
            });
            self.late(hub, job);
        } else {
            self.finish(hub, job, Err(error.unwrap_or_else(|| "the runner reported a failure".to_owned())));
        }
    }

    /// [`AgentMsg::Build`] from a machine: an upgrade of it to that build has arrived, also one
    /// kiln had stopped waiting for ([`Self::late`]).
    pub fn on_build(&self, hub: &Hub, machine: &str, build: &str) {
        let ids: Vec<String> = self
            .jobs
            .lock()
            .iter()
            .filter(|j| j.awaits(machine, build))
            .map(|j| j.id.clone())
            .collect();
        for id in ids {
            self.update(hub, &id, |j| {
                j.reached = true;
                push_line(j, format!("{machine} is running build {build}"));
            });
            self.late(hub, &id);
        }
        self.late_by_commit(hub, machine);
    }

    /// The commit `machine`'s runner runs may be known now ([`AgentMsg::Facts`], or a build kiln
    /// knows the commit of): an upgrade kiln had stopped waiting for that brought it there on a build
    /// its machine signed again ([`Self::reached_commit`]) is upgraded after all.
    pub(crate) fn late_by_commit(&self, hub: &Hub, machine: &str) {
        let ids: Vec<String> =
            self.jobs.lock().iter().filter(|j| j.kind == "runner" && j.target == machine && j.state == JobState::Failed && !j.reached).map(|j| j.id.clone()).collect();
        for id in ids {
            self.reached_commit(hub, &id, machine);
            self.late(hub, &id);
        }
    }

    /// A job kiln had stopped waiting for whose machine got through after all (the new build
    /// reported in and, on a runner that upgrades blue/green, took over): it is upgraded, and
    /// reads so.
    fn late(&self, hub: &Hub, id: &str) {
        let now = crate::util::now_unix();
        let mut through = None;
        self.update(hub, id, |j| {
            if j.through_late(now) {
                through = Some(j.clone());
            }
        });
        if let Some(j) = through {
            crate::fleet::on_job_end(hub, &j, None);
            hub.emit(&serde_json::json!({ "type": "updates" }));
        }
    }
}

impl Job {
    /// It waits for `machine` to run `build`: a runner's upgrade, not done (also one kiln stopped
    /// waiting for), whose build hasn't reported in.
    fn awaits(&self, machine: &str, build: &str) -> bool {
        self.kind == "runner" && self.target == machine && self.state != JobState::Done && !self.reached && self.to_build.as_deref() == Some(build)
    }

    /// Kiln had stopped waiting for it, and its machine got through since (the new build reported
    /// in and, upgrading blue/green, took over): it is done after all, and says so. Whether it was.
    fn through_late(&mut self, now: i64) -> bool {
        if self.state != JobState::Failed || !self.reached || (self.blue_green && !self.handed_over) {
            return false;
        }
        let was = self.error.take().unwrap_or_default();
        self.timed_out = None;
        self.state = JobState::Done;
        self.ended_at = Some(now);
        push_line(self, format!("upgraded after all: it got there after kiln had stopped waiting ({was})"));
        true
    }
}

/// Whether a runner whose newest process runs `build`, built from `commit`, is where job `j`
/// brings it: a build other than the one it ran before, of the job's commit.
fn arrived_by_commit(j: &Job, build: Option<&str>, commit: Option<&str>) -> bool {
    let (Some(want), Some(build), Some(commit)) = (j.commit.as_deref(), build, commit) else { return false };
    !j.reached && j.from_build.as_deref() != Some(build) && commit.eq_ignore_ascii_case(want)
}

fn push_line(j: &mut Job, line: String) {
    // A progress line gives way to the next line (the terminal draws it again after).
    if std::mem::take(&mut j.progress) {
        j.log.pop();
    }
    j.log.push(line);
    if j.log.len() > KEEP_LINES {
        let cut = j.log.len() - KEEP_LINES;
        j.log.drain(..cut);
    }
}

pub(crate) fn new_job(kind: &'static str, target: &str, method: &'static str, from_build: Option<String>, to_build: Option<String>) -> Job {
    Job {
        id: crate::util::new_id(),
        kind,
        target: target.to_owned(),
        method,
        state: JobState::Running,
        started_at: crate::util::now_unix(),
        ended_at: None,
        from_build,
        to_build,
        commit: None,
        on: None,
        reached: false,
        log: Vec::new(),
        error: None,
        timed_out: None,
        progress: false,
        blue_green: false,
        handed_over: false,
    }
}

// ------------------------------------------------------------------ runners

/// A machine's runner as System → Updates shows it.
#[derive(Debug, Clone, Serialize)]
pub struct RunnerInfo {
    pub name: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub connected: bool,
    pub version: String,
    /// The platform build it is given.
    pub target: Option<String>,
    /// The build it would be upgraded to (`None`: no binary for its platform here yet; an
    /// upgrade tries this version's GitHub release).
    pub target_build: Option<String>,
    /// The build its newest runner process runs (`None`: too old to say).
    pub build: Option<String>,
    /// It runs the newest kiln (`crate::fleet`); without a newest known, the build kiln has for
    /// its platform.
    pub up_to_date: bool,
    /// The commit it runs, when kiln knows it.
    pub commit: Option<String>,
    /// Where it stands against the newest kiln (`None`: no newest known).
    pub standing: Option<crate::fleet::Standing>,
    /// What it does next to reach the newest kiln, as the end of a sentence about it ("builds
    /// 94f0847a1b2c itself", "waits for …").
    pub next: Option<String>,
    /// How: "download", "build", "wait", "upgrading" (it is), or "left" (left alone).
    pub how: Option<&'static str>,
    /// The release run it waits for, and how far that is.
    pub waits_on: Option<crate::fleet::ReleaseRun>,
    /// Its kiln checkout, as it reports it.
    pub checkout: Option<String>,
    /// "native" or "terminal".
    pub method: &'static str,
    /// Terminals are allowed on it (an older runner is upgraded through one).
    pub terminal: bool,
    /// What it does with a newer kiln, as chosen for it.
    pub policy: Policy,
    /// Its machine refuses upgrades (its own kiln config: `[updates] accept = false`): it is never
    /// upgraded, whatever its policy here.
    pub locked: bool,
    /// Where the build it would get comes from ("this server's own build", "built by release run
    /// #141", "built by adas-macbook", ...), when there is one.
    pub from: Option<String>,
    /// The ways a person can have it upgraded to the newest kiln now, each with what it does or
    /// why it can't (empty when it runs the newest, or kiln knows no newest).
    pub ways: Vec<WayOpen>,
    pub instances: Vec<crate::instances::InstanceView>,
}

/// How a person has a runner upgraded: a binary of the newest kiln, downloaded; or kiln built from
/// the server's source, on the machine itself or, for a Mac that can't, on another machine whose
/// build it then downloads (`crate::fleet`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Way {
    Download,
    Build,
}

/// One way a runner can be upgraded now, or why it can't.
#[derive(Debug, Clone, Serialize)]
pub struct WayOpen {
    pub way: Way,
    pub ok: bool,
    /// What it does ("downloads build 493003c712b0"), or why it can't ("no binary of 31c0bbaf734c
    /// for aarch64-apple-darwin yet").
    pub why: String,
    /// A build's: the machine it's made on, when another than the runner's own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on: Option<String>,
}

/// What a runner does with a newer kiln (`crate::fleet`), as chosen for it in System → Updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Policy {
    /// Upgraded as soon as there is a build of the newest kiln for it.
    Auto,
    /// Told a newer kiln is available, and where it comes from; upgraded when someone chooses to.
    #[default]
    Notify,
    /// Left alone: never upgraded, on its own or with the others, until its policy changes.
    Off,
}

impl Policy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Notify => "notify",
            Self::Off => "off",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::Auto),
            "notify" => Some(Self::Notify),
            "off" => Some(Self::Off),
            _ => None,
        }
    }
}

/// Where a runner's policy is kept (none: the default).
fn policy_key(machine: &str) -> String {
    format!("updates/policy/{machine}")
}

/// `machine`'s update policy: Notify unless someone chose otherwise.
pub async fn policy(hub: &Hub, machine: &str) -> Policy {
    hub.store.kv_get(policy_key(machine)).await.ok().flatten().and_then(|v| Policy::parse(&v)).unwrap_or_default()
}

/// Set `machine`'s update policy.
pub async fn set_policy(hub: &Hub, machine: &str, p: Policy) -> anyhow::Result<()> {
    if p == Policy::default() { hub.store.kv_delete(policy_key(machine)).await } else { hub.store.kv_set(policy_key(machine), p.as_str().to_owned()).await }
}

/// How long kiln waits on a runner's upgrade before it calls it failed: `[updates]` in kiln.toml,
/// unless changed in System → Updates. A change applies to upgrades under way too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Waits {
    /// Minutes a runner has to switch to its new build and report in, once it has no jobs running.
    pub upgrade_minutes: u64,
    /// Minutes kiln keeps an upgrade open while the runner finishes the jobs it is running.
    pub wait_for_jobs_minutes: u64,
    /// Minutes a machine has to build kiln itself, install included.
    pub build_minutes: u64,
}

/// What each wait may be, in minutes.
const WAIT_LIMITS: [(&str, u64, u64); 3] = [("upgrade_minutes", 1, 24 * 60), ("wait_for_jobs_minutes", 1, 7 * 24 * 60), ("build_minutes", 5, 24 * 60)];

impl Waits {
    /// As kiln.toml sets them.
    pub fn of(u: &crate::config::Updates) -> Self {
        Self { upgrade_minutes: u.upgrade_minutes, wait_for_jobs_minutes: u.wait_for_jobs_minutes, build_minutes: u.build_minutes }
    }

    fn values(self) -> [u64; 3] {
        [self.upgrade_minutes, self.wait_for_jobs_minutes, self.build_minutes]
    }

    fn from_values([upgrade_minutes, wait_for_jobs_minutes, build_minutes]: [u64; 3]) -> Self {
        Self { upgrade_minutes, wait_for_jobs_minutes, build_minutes }
    }

    /// Each one within what kiln takes.
    pub fn check(self) -> anyhow::Result<()> {
        for ((name, lo, hi), v) in WAIT_LIMITS.into_iter().zip(self.values()) {
            anyhow::ensure!((lo..=hi).contains(&v), "{name} is {v}: it can be from {lo} to {hi} minutes");
        }
        Ok(())
    }

    /// Brought within what kiln takes (a kiln.toml can say anything).
    fn clamped(self) -> Self {
        let mut v = self.values();
        for (x, (_, lo, hi)) in v.iter_mut().zip(WAIT_LIMITS) {
            *x = (*x).clamp(lo, hi);
        }
        Self::from_values(v)
    }

    pub fn upgrade(self) -> Duration {
        minutes(self.upgrade_minutes)
    }

    pub fn jobs(self) -> Duration {
        minutes(self.wait_for_jobs_minutes)
    }

    pub fn build(self) -> Duration {
        minutes(self.build_minutes)
    }
}

fn minutes(m: u64) -> Duration {
    Duration::from_secs(m.saturating_mul(60))
}

/// A wait as a person says it: "15 minutes", "an hour", "6 hours", "90 minutes".
pub fn said(d: Duration) -> String {
    let m = d.as_secs() / 60;
    match m {
        60 => "an hour".to_owned(),
        1 => "a minute".to_owned(),
        m if m >= 120 && m % 60 == 0 => format!("{} hours", m / 60),
        m => format!("{m} minutes"),
    }
}

/// Where the waits chosen in System → Updates are kept (none: kiln.toml's).
const WAITS_KEY: &str = "updates/waits";

/// How long upgrades are waited on now.
pub async fn waits(hub: &Hub) -> Waits {
    if let Some(w) = *hub.updates.waits.lock() {
        return w;
    }
    let chosen = hub.store.kv_get(WAITS_KEY.to_owned()).await.ok().flatten().and_then(|v| serde_json::from_str::<Waits>(&v).ok());
    let w = chosen.unwrap_or_else(|| Waits::of(&hub.cfg.updates)).clamped();
    *hub.updates.waits.lock() = Some(w);
    w
}

/// The waits were changed in System → Updates: kiln.toml's don't apply.
pub async fn waits_changed(hub: &Hub) -> bool {
    hub.store.kv_get(WAITS_KEY.to_owned()).await.ok().flatten().is_some()
}

/// Wait this long from now on (`None`: as kiln.toml says).
pub async fn set_waits(hub: &Hub, w: Option<Waits>) -> anyhow::Result<()> {
    match w {
        Some(w) => {
            w.check()?;
            hub.store.kv_set(WAITS_KEY.to_owned(), serde_json::to_string(&w)?).await?;
        }
        None => hub.store.kv_delete(WAITS_KEY.to_owned()).await?,
    }
    *hub.updates.waits.lock() = None;
    Ok(())
}

/// The runners' gauges (`crate::metrics`): up, behind, update policy and the kiln each runs.
pub async fn measure(hub: &Arc<Hub>) {
    let rs = runners(hub).await;
    let m = crate::metrics::global();
    m.runner_up.replace(rs.iter().map(|r| (vec![r.name.clone()], i64::from(r.connected))));
    m.runner_behind.replace(rs.iter().map(|r| (vec![r.name.clone()], i64::from(matches!(r.standing, Some(crate::fleet::Standing::Behind | crate::fleet::Standing::Unknown))))));
    m.runner_update_policy.replace(rs.iter().map(|r| (vec![r.name.clone(), r.policy.as_str().to_owned()], 1)));
    m.runner_version_info.replace(rs.iter().map(|r| {
        let version = r.commit.as_deref().map(|c| short(c).to_owned()).or_else(|| r.build.clone()).unwrap_or_else(|| "unknown".to_owned());
        (vec![r.name.clone(), version], 1)
    }));
}

/// Where a build kiln hands out comes from, said for a person.
pub fn from_words(b: &Binary) -> String {
    match b.origin.as_str() {
        "server" => "this server's own build".to_owned(),
        "built" => b.run.map_or_else(|| "built by kiln's release pipeline".to_owned(), |n| format!("built by release run #{n}")),
        "machine" => b.by.as_ref().map_or_else(|| "built by one of its machines".to_owned(), |m| format!("built by {m}")),
        "upload" => "uploaded here".to_owned(),
        "release" => "a GitHub release".to_owned(),
        other => other.to_owned(),
    }
}

pub async fn runners(hub: &Arc<Hub>) -> Vec<RunnerInfo> {
    let mine = own_build();
    let mut out: Vec<RunnerInfo> = hub
        .agent_list()
        .await
        .into_iter()
        // Outposts aren't installed, so there's nothing of them to upgrade.
        .filter(|a| !a.ephemeral && !a.outpost)
        .map(|a| {
            let instances = hub.runner_processes(&a.name);
            let newest = instances.iter().find(|i| i.newest);
            let build = newest.and_then(|i| i.build.clone());
            let target = target_for(&a.os, &a.arch);
            let target_build = target.as_deref().and_then(|t| if t == crate::api::TARGET { Some(mine.clone()) } else { stored(hub, t).map(|b| b.build) });
            RunnerInfo {
                up_to_date: build.is_some() && build == target_build,
                method: if newest.is_some_and(|i| i.can_upgrade) { "native" } else { "terminal" },
                terminal: a.terminal,
                policy: Policy::default(),
                locked: false,
                from: None,
                ways: Vec::new(),
                target,
                target_build,
                build,
                name: a.name,
                hostname: a.hostname,
                os: a.os,
                arch: a.arch,
                connected: a.connected,
                version: a.version,
                instances,
                commit: None,
                standing: None,
                next: None,
                how: None,
                waits_on: None,
                checkout: None,
            }
        })
        .collect();
    out.sort_by(|a, b| b.connected.cmp(&a.connected).then_with(|| a.name.cmp(&b.name)));
    // Against the newest kiln, when there is one: the commit each runs, and how it gets there.
    let newest = hub.updates.fleet.target();
    for r in &mut out {
        r.policy = policy(hub, &r.name).await;
        r.locked = hub.updates.fleet.refuses(&r.name);
        r.commit = crate::fleet::runner_commit(hub, &r.name, r.build.as_deref());
        r.checkout = crate::fleet::reported(hub, &r.name).and_then(|k| k.checkout);
        let Some(t) = &newest else { continue };
        let b = r.target.as_deref().and_then(|target| crate::fleet::binary_for(hub, target, &t.commit));
        r.from = b.as_ref().map(from_words);
        r.target_build = b.map(|b| b.build);
        let s = crate::fleet::standing(hub, r, t).await;
        r.up_to_date = s == crate::fleet::Standing::Current;
        r.standing = Some(s);
        if r.connected
            && let Some((how, next)) = crate::fleet::next(hub, r, t, s).await
        {
            if how == "wait" {
                r.waits_on = crate::fleet::waits_on(hub, r, t).await;
            }
            r.how = Some(how);
            r.next = Some(next);
        }
        if r.connected && !r.up_to_date && !r.locked && r.how != Some("left") {
            r.ways = crate::fleet::ways(hub, r, t).await;
        }
    }
    out
}

/// Start upgrading `machine`'s runner to the newest kiln (`crate::fleet`): the `way` asked for, or
/// a binary of it when kiln has one for its platform, else a build of it (on the machine itself,
/// or one another machine makes for its platform, which it downloads). A person asking now isn't
/// held back by the wait between automatic tries. With no newest known (a server built without
/// knowing its commit), to the build kiln has for its platform. Nothing it runs is stopped.
pub async fn upgrade_runner(hub: &Arc<Hub>, machine: &str, way: Option<Way>) -> anyhow::Result<Job> {
    if let Some(j) = hub.updates.running_for("runner", machine) {
        return Ok(j);
    }
    let r = runners(hub).await.into_iter().find(|r| r.name == machine).ok_or_else(|| anyhow::anyhow!("there is no runner called {machine}"))?;
    anyhow::ensure!(r.policy != Policy::Off, "updates are off for {machine}: turn them on to upgrade it");
    anyhow::ensure!(!r.locked, "{machine} refuses upgrades: its own kiln config says [updates] accept = false");
    anyhow::ensure!(r.connected, "{machine} is not connected");
    if let Some(t) = hub.updates.fleet.target() {
        anyhow::ensure!(!r.up_to_date, "{machine} already runs the newest kiln ({})", crate::fleet::short(&t.commit));
        let plan = match way {
            None => crate::fleet::plan(hub, &r, &t).await,
            Some(w) => {
                let open = crate::fleet::ways(hub, &r, &t).await.into_iter().find(|o| o.way == w);
                match open {
                    Some(o) if !o.ok => anyhow::bail!("{machine} can't be upgraded that way: {}", o.why),
                    _ => crate::fleet::plan_by(hub, &r, &t, w)?,
                }
            }
        };
        return match plan {
            crate::fleet::Plan::Download(b) => upgrade_with(hub, &r, &b, "admin").await,
            crate::fleet::Plan::Build(b) => crate::fleet::build(hub, &r, &t.commit, &b, "admin").await,
            crate::fleet::Plan::Join { job, on } => crate::fleet::join(hub, &r, &t.commit, &job, &on, "admin").await,
            crate::fleet::Plan::Wait(why) => anyhow::bail!("{machine} can't be upgraded yet: it {why}"),
        };
    }
    anyhow::ensure!(way != Some(Way::Build), "{machine} can't build kiln itself while kiln knows no newest commit to build");
    let target = r.target.clone().ok_or_else(|| anyhow::anyhow!("kiln has no build for {} on {}", r.os, r.arch))?;
    let (sha256, size) = ensure_binary(hub, &target).await?;
    let b = binaries(hub).into_iter().find(|b| b.target == target && b.sha256 == sha256).unwrap_or_else(|| Binary {
        build: build_id(&sha256),
        target,
        sha256,
        size,
        origin: "release".to_owned(),
        at: 0,
        run: None,
        sha: None,
        by: None,
    });
    upgrade_with(hub, &r, &b, "admin").await
}

/// Which runner process is `r`'s newest, by the start of its id: the service of one set up beside
/// another is named after it (empty for the machine's usual one).
pub(crate) fn instance8(r: &RunnerInfo) -> String {
    r.instances
        .iter()
        .find(|i| i.newest && !i.legacy && i.id != crate::upgrade::MAIN)
        .map(|i| i.id.chars().filter(char::is_ascii_alphanumeric).take(8).collect::<String>())
        .unwrap_or_default()
}

/// Start upgrading `r` to binary `b` (`by`: who asked, for the audit log). Nothing it runs is
/// stopped.
pub async fn upgrade_with(hub: &Arc<Hub>, r: &RunnerInfo, b: &Binary, by: &str) -> anyhow::Result<Job> {
    let machine = &r.name;
    anyhow::ensure!(
        r.method == "native" || r.terminal,
        "{machine} runs a kiln too old to upgrade itself and has terminals turned off: run `kiln install --agent --upgrade` with a new kiln binary on it"
    );
    anyhow::ensure!(r.build.as_deref() != Some(b.build.as_str()), "{machine} already runs build {}", b.build);
    let mut job = new_job("runner", machine, r.method, r.build.clone(), Some(b.build.clone()));
    job.commit.clone_from(&b.sha);
    let id = job.id.clone();
    hub.updates.insert(job.clone());
    hub.store.audit(by.into(), "upgrade.runner".into(), format!("{machine} to build {} ({}, {})", b.build, b.target, r.method)).await;
    hub.updates.say(hub, &id, format!("upgrading {machine} to build {} ({}); nothing it runs is stopped", b.build, b.target));
    let (h, machine, b) = (Arc::clone(hub), machine.clone(), b.clone());
    if r.method == "native" {
        crate::rt::spawn(async move { native(&h, &id, &machine, &b).await });
    } else {
        let inst8 = instance8(r);
        crate::rt::spawn(async move { terminal(&h, &id, &machine, &b, &inst8).await });
    }
    Ok(job)
}

/// Job `id` goes on to upgrade `machine` to `b`, a build another machine made for its platform
/// (`crate::fleet`): downloaded and installed as [`upgrade_with`] has it done. Nothing it runs is
/// stopped.
pub(crate) async fn upgrade_into(hub: &Arc<Hub>, id: &str, machine: &str, b: &Binary) {
    let u = &hub.updates;
    let Some(r) = runners(hub).await.into_iter().find(|r| r.name == machine) else {
        u.finish(hub, id, Err(format!("there is no runner called {machine}")));
        return;
    };
    let cant = if !r.connected {
        Some(format!("{machine} is not connected"))
    } else if r.method != "native" && !r.terminal {
        Some(format!("{machine} runs a kiln too old to upgrade itself and has terminals turned off: run `kiln install --agent --upgrade` with a new kiln binary on it"))
    } else {
        None
    };
    if let Some(e) = cant {
        u.finish(hub, id, Err(e));
        return;
    }
    if r.build.as_deref() == Some(b.build.as_str()) {
        u.finish(hub, id, Ok(format!("{machine} runs build {} already", b.build)));
        return;
    }
    u.update(hub, id, |j| {
        j.from_build.clone_from(&r.build);
        j.to_build = Some(b.build.clone());
    });
    hub.store.audit("kiln".into(), "upgrade.runner".into(), format!("{machine} to build {} ({}, {}), built by {}", b.build, b.target, r.method, b.by.as_deref().unwrap_or("another machine"))).await;
    u.say(hub, id, format!("upgrading {machine} to build {} ({}); nothing it runs is stopped", b.build, b.target));
    if r.method == "native" {
        native(hub, id, machine, b).await;
    } else {
        terminal(hub, id, machine, b, &instance8(&r)).await;
    }
}

/// The runner does it itself: it fetches the binary from this server, checks it and runs
/// `install --agent --upgrade`, reporting back (UpgradeLog / UpgradeDone). A runner that takes
/// the binary in place restarts its own process (same pid, terminals handed over) and so may
/// never send UpgradeDone: the new build reporting in is what completes the job.
async fn native(hub: &Arc<Hub>, id: &str, machine: &str, b: &Binary) {
    let u = &hub.updates;
    // That exact build: the server may hand out more than one for a platform.
    let msg = ServerMsg::Upgrade { job: id.to_owned(), path: format!("/dl/kiln/{}/{}", b.target, b.build), sha256: b.sha256.clone(), size: b.size };
    // A live command: refused at once while its runner is away. Refused, the upgrade ends there;
    // unanswered (the runner restarting into its new build, say), it goes on to see what came of it.
    match hub.command_start(machine, &msg).await.answer(crate::hub::COMMAND_WITHIN).await {
        crate::wire::commands::Answer::Taken | crate::wire::commands::Answer::Sent | crate::wire::commands::Answer::Unanswered { .. } | crate::wire::commands::Answer::Parked => {}
        other => {
            u.finish(hub, id, Err(format!("{machine}: {}", other.words())));
            return;
        }
    }
    // A runner that upgrades blue/green is through once it says its new build took over.
    let blue_green = {
        let inst = hub.instances.lock();
        inst.newest(machine).and_then(|p| inst.get(p)).is_some_and(|i| i.caps & crate::proto::CAP_BLUE_GREEN != 0)
    };
    u.update(hub, id, |j| j.blue_green = blue_green);
    u.say(hub, id, if blue_green { "asked the runner to upgrade itself: the new build starts beside it and takes over" } else { "asked the runner to upgrade itself" });
    let since = Instant::now();
    // The runner switches once its running jobs are done: that wait is theirs, not the upgrade's,
    // so the upgrade's own starts once they are.
    let mut idle_since = since;
    let (mut waiting_on, mut waited) = (0, false);
    loop {
        crate::rt::sleep(Duration::from_secs(1)).await;
        if !u.running(id) {
            return;
        }
        u.reached_commit(hub, id, machine);
        if u.through(id) {
            u.finish(hub, id, Ok(String::new()));
            return;
        }
        let w = waits(hub).await;
        let busy: usize = hub.runner_processes(machine).iter().map(|i| i.jobs).sum();
        if busy > 0 {
            if since.elapsed() > w.jobs() {
                let e = format!("{machine} kept running jobs for {}, so it never switched; it does once it is idle", said(w.jobs()));
                u.time_out(hub, id, "jobs", e);
                return;
            }
            idle_since = Instant::now();
            waited = true;
            if busy != waiting_on {
                u.say(hub, id, format!("{machine} waits for {} before it switches", if busy == 1 { "its running job".to_owned() } else { format!("its {busy} running jobs") }));
            }
        } else if idle_since.elapsed() > w.upgrade() {
            let after = if waited { " of its last job ending" } else { "" };
            u.time_out(hub, id, "upgrade", format!("the new runner didn't report in within {}{after}; see the runner's log", said(w.upgrade())));
            return;
        }
        waiting_on = busy;
    }
}

/// The upgrade script for a runner from before [`CAP_UPGRADE`], run through its terminal support
/// so it needs nothing new on the machine. It finds the service the runner belongs to (its
/// cgroup; `inst8` names a runner set up beside another, whose service is `<base>-<inst8>`) and
/// runs `install --agent --upgrade` for it. Free of single quotes and backslashes, so that
/// `sh -c '<script>'` means the same in sh, bash, zsh and fish, whichever the user's shell is.
pub fn terminal_script(job: &str, urls: &[String], sha256: &str, inst8: &str) -> String {
    let urls = urls.join(" ");
    let strip = if inst8.is_empty() { String::new() } else { format!("case \"$svc\" in *-{inst8}) svc=${{svc%-{inst8}}} ;; esac\n") };
    format!(
        r#"{HIDDEN_MARK}{job}
set -u
dir=$(mktemp -d 2>/dev/null || echo "${{TMPDIR:-/tmp}}/kiln-upgrade-$$")
mkdir -p "$dir"
bin="$dir/kiln"
svc=kiln-agent
if [ -r /proc/self/cgroup ]; then
  cg=$(grep -m1 -E "^0::|name=systemd" /proc/self/cgroup | cut -d: -f3)
  case "$cg" in
    */kiln*.service) svc=$(basename "$cg" .service) ;;
  esac
fi
{strip}echo "service $svc"
fetched=""
for url in {urls}; do
  echo "downloading $url"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --retry 2 -o "$bin" "$url" && fetched=1 && break
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$bin" "$url" && fetched=1 && break
  else
    echo "{FAIL_MARK} this machine has neither curl nor wget"; rm -rf "$dir"; exit 3
  fi
done
if [ -z "$fetched" ]; then echo "{FAIL_MARK} could not download the new kiln binary"; rm -rf "$dir"; exit 4; fi
want="{sha256}"
if command -v sha256sum >/dev/null 2>&1; then got=$(sha256sum "$bin" | cut -d" " -f1)
elif command -v shasum >/dev/null 2>&1; then got=$(shasum -a 256 "$bin" | cut -d" " -f1)
else got="$want"; echo "no sha256sum or shasum here: the download is not verified"; fi
if [ "$got" != "$want" ]; then echo "{FAIL_MARK} the download does not match: sha256 $got, expected $want"; rm -rf "$dir"; exit 5; fi
echo "verified the download"
chmod 755 "$bin"
"$bin" install --agent --upgrade --service "$svc"
rc=$?
rm -rf "$dir"
if [ "$rc" = 0 ]; then echo "{OK_MARK}"; else echo "{FAIL_MARK} install --agent --upgrade exited with $rc"; fi
exit "$rc"
"#
    )
}

/// Where a machine can reach this server: its public URL, then the one pods use.
pub(crate) fn server_bases(hub: &Hub) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    for base in [hub.cfg.server.public_url.as_str(), hub.cfg.kubernetes.server_url.as_deref().unwrap_or("")] {
        let base = base.trim_end_matches('/');
        let usable = (base.starts_with("http://") || base.starts_with("https://")) && !base.contains(|c: char| c.is_whitespace() || "'\\\"`$;&|<>()".contains(c));
        if usable && !v.iter().any(|u| u == base) {
            v.push(base.to_owned());
        }
    }
    v
}

/// Where a machine can fetch build `build` for `target` from this server.
fn download_urls(hub: &Hub, target: &str, build: &str) -> Vec<String> {
    server_bases(hub).into_iter().map(|base| format!("{base}/dl/kiln/{target}/{build}")).collect()
}

/// Terminal output line by line, as the terminal shows it: a carriage return starts the line over,
/// so a progress bar that keeps drawing itself is one line, its latest drawing.
#[derive(Default)]
pub(crate) struct Screen {
    line: String,
    /// A carriage return came last: what follows draws over the line.
    back: bool,
    /// The line under way has been drawn over.
    redrawn: bool,
}

/// What [`Screen::feed`] shows.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Shown {
    /// A line a newline ended.
    Line(String),
    /// The line being drawn over (a progress bar), as it is now.
    Progress(String),
}

impl Screen {
    /// The lines `text` ends, then the line under way when it's being drawn over.
    pub(crate) fn feed(&mut self, text: &str) -> Vec<Shown> {
        let mut out = Vec::new();
        for c in strip(text).chars() {
            match c {
                '\n' => {
                    out.push(Shown::Line(std::mem::take(&mut self.line)));
                    self.back = false;
                    self.redrawn = false;
                }
                '\r' => self.back = true,
                c => {
                    if std::mem::take(&mut self.back) {
                        self.redrawn |= !self.line.trim().is_empty();
                        self.line.clear();
                    }
                    self.line.push(c);
                }
            }
        }
        if (self.back || self.redrawn) && !self.line.trim().is_empty() {
            out.push(Shown::Progress(self.line.clone()));
        }
        out
    }
}

/// A line of an upgrade's script that is kiln's own: never shown as it's being drawn.
pub(crate) fn marked(line: &str) -> bool {
    line.starts_with(HIDDEN_MARK) || line.contains(OK_MARK) || line.contains(FAIL_MARK)
}

/// Terminal output without escape sequences or control characters, but for line ends (`\n`, and
/// `\r`, for [`Screen`]) and tabs.
fn strip(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.peek() {
                // CSI: ESC [ … final byte in @..~
                Some('[') => {
                    chars.next();
                    for n in chars.by_ref() {
                        if ('@'..='~').contains(&n) {
                            break;
                        }
                    }
                }
                // OSC: ESC ] … BEL or ESC \
                Some(']') => {
                    chars.next();
                    while let Some(n) = chars.next() {
                        if n == '\x07' || (n == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                _ => {
                    chars.next();
                }
            },
            '\n' | '\r' | '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Through a terminal on the runner (it knows nothing of upgrades): download, check, install.
async fn terminal(hub: &Arc<Hub>, id: &str, machine: &str, b: &Binary, inst8: &str) {
    use crate::termsess::Frame;
    let u = &hub.updates;
    let urls = download_urls(hub, &b.target, &b.build);
    if urls.is_empty() {
        u.finish(hub, id, Err("kiln has no public URL (server.public_url) the machine could download from".to_owned()));
        return;
    }
    let script = terminal_script(id, &urls, &b.sha256, inst8);
    let session = crate::termsess::open_agent(hub, machine.to_owned(), Some(format!("sh -c '{script}'")), 200, 50);
    u.say(hub, id, "this runner predates upgrade commands: running the upgrade in a terminal on it (not listed anywhere)");
    let mut att = session.attach();
    let mut screen = Screen::default();
    let mut ok_marked = false;
    let mut feed = |text: &str| {
        for shown in screen.feed(text) {
            let line = match shown {
                Shown::Line(line) => line,
                Shown::Progress(p) => {
                    if !marked(&p) {
                        u.progress(hub, id, p.trim());
                    }
                    continue;
                }
            };
            let line = line.trim_end();
            if line.is_empty() {
                continue;
            }
            if line.starts_with(HIDDEN_MARK) {
                u.end_progress(hub, id);
                continue;
            }
            if line.contains(OK_MARK) {
                ok_marked = true;
                u.end_progress(hub, id);
                continue;
            }
            u.say(hub, id, line.strip_prefix(FAIL_MARK).map_or(line, str::trim));
        }
    };
    feed(&String::from_utf8_lossy(&att.replay));
    let started = Instant::now();
    let mut timed_out = false;
    let result = loop {
        // Looked at again at least every minute: a longer wait chosen meanwhile applies.
        let limit = waits(hub).await.upgrade();
        let left = limit.saturating_sub(started.elapsed());
        if left.is_zero() {
            timed_out = true;
            break Err(format!("the upgrade didn't finish in {}", said(limit)));
        }
        let Ok(frame) = crate::rt::timeout(left.min(Duration::from_secs(60)), att.live.recv()).await else {
            continue;
        };
        match frame {
            Ok(Frame::Data(d)) => feed(&String::from_utf8_lossy(&d)),
            Ok(Frame::Exit { code, error }) => {
                feed("\n");
                break match (code, error) {
                    (Some(0), _) => Ok(()),
                    (_, Some(e)) if e == "__reconnect__" => Err(format!("{machine} disconnected during the upgrade")),
                    (_, Some(e)) => Err(e),
                    (c, None) => Err(format!("the upgrade exited with {}", c.map_or_else(|| "a signal".to_owned(), |c| c.to_string()))),
                };
            }
            Ok(Frame::Size { .. }) | Err(async_broadcast::RecvError::Overflowed(_)) => {}
            Err(async_broadcast::RecvError::Closed) => break Err("the upgrade's terminal closed".to_owned()),
        }
    };
    session.detach();
    if let Err(e) = result
        && !ok_marked
    {
        if timed_out {
            session.close();
            u.time_out(hub, id, "upgrade", e);
        } else {
            u.finish(hub, id, Err(e));
        }
        return;
    }
    // Installed: the new runner reports its build when it connects, which completes the job.
    let until = Instant::now() + REPORT_IN;
    while Instant::now() < until {
        if !u.running(id) {
            return;
        }
        if u.reached(id) {
            u.finish(hub, id, Ok(String::new()));
            return;
        }
        crate::rt::sleep(Duration::from_secs(1)).await;
    }
    u.finish(hub, id, Ok("installed; the new runner hasn't reported in yet (it shows under Runners when it does)".to_owned()));
}

/// Upgrade every connected runner that isn't on the newest kiln (one running a branch or a local
/// build on purpose is left alone: upgrade it by itself).
pub async fn upgrade_all(hub: &Arc<Hub>) -> Vec<Result<Job, String>> {
    let mut out = Vec::new();
    for r in runners(hub).await {
        // Runners whose updates are off are left alone, with the others too.
        if r.policy != Policy::Off && !r.locked && r.connected && !r.up_to_date && !matches!(r.standing, Some(crate::fleet::Standing::Branch | crate::fleet::Standing::Local)) {
            out.push(upgrade_runner(hub, &r.name, None).await.map_err(|e| format!("{}: {e:#}", r.name)));
        }
    }
    out
}

// ------------------------------------------------------------------ the server

/// What could replace this server's binary.
#[derive(Debug, Clone, Serialize)]
pub struct Replacement {
    /// "upload" or "release".
    pub origin: &'static str,
    /// Known for an upload; a release's is known once downloaded.
    pub build: Option<String>,
    pub version: Option<String>,
}

/// How this server runs and what upgrading it means.
#[derive(Debug, Clone, Serialize)]
pub struct ServerInfo {
    pub version: &'static str,
    pub build: String,
    pub target: &'static str,
    /// "kubernetes", "service" (under `kiln supervise`) or "standalone".
    pub mode: &'static str,
    pub deployment: Option<String>,
    pub namespace: Option<String>,
    pub image: Option<ImageCheck>,
    /// A check (GitHub, the image) is under way; an `updates` event follows.
    pub checking: bool,
    /// A newer build is ready to run: an image on the node (Kubernetes) or a binary.
    pub newer: bool,
    /// The action that upgrades it: "restart" (Kubernetes), "replace" (service) or none.
    pub action: Option<&'static str>,
    pub replacement: Option<Replacement>,
    pub release: ReleaseInfo,
    /// Plain words about upgrading this server.
    pub note: String,
}

/// The Deployment a pod belongs to: pods are named `<deployment>-<replicaset hash>-<pod hash>`.
pub fn deployment_of(pod: &str) -> Option<String> {
    let mut parts: Vec<&str> = pod.rsplitn(3, '-').collect();
    if parts.len() != 3 {
        return None;
    }
    parts.pop().filter(|d| !d.is_empty()).map(str::to_owned)
}

/// (Deployment, namespace) when this server runs as a pod.
pub(crate) fn kube_identity() -> Option<(String, String)> {
    std::env::var("KUBERNETES_SERVICE_HOST").ok()?;
    let pod = std::env::var("POD_NAME").or_else(|_| std::env::var("HOSTNAME")).ok()?;
    let ns = std::env::var("POD_NAMESPACE")
        .ok()
        .filter(|n| !n.is_empty())
        .or_else(|| std::fs::read_to_string("/var/run/secrets/kubernetes.io/serviceaccount/namespace").ok().map(|s| s.trim().to_owned()))?;
    Some((deployment_of(&pod)?, ns))
}

fn supervised() -> bool {
    std::env::var("KILN_SUPERVISED").is_ok_and(|v| v == "1")
}

pub fn server(hub: &Arc<Hub>, force: bool) -> ServerInfo {
    let release = release(hub, force);
    let build = own_build();
    let image = image(hub, force);
    let mut s = ServerInfo {
        version: env!("CARGO_PKG_VERSION"),
        build: build.clone(),
        target: crate::api::TARGET,
        mode: "standalone",
        deployment: None,
        namespace: None,
        image: image.clone(),
        checking: hub.updates.checking_release.load(Ordering::Acquire) || hub.updates.checking_image.load(Ordering::Acquire),
        newer: false,
        action: None,
        replacement: None,
        release: release.clone(),
        note: String::new(),
    };
    if let Some((deployment, ns)) = kube_identity() {
        let img = image.unwrap_or_default();
        let node = img.node.as_deref().unwrap_or("its node");
        s.mode = "kubernetes";
        s.newer = img.newer;
        s.action = Some("restart");
        s.note = match (&img.build, &img.error) {
            (Some(b), _) if img.newer => format!("The {} image on {node} holds build {b}; this server runs {build}. Restarting Deployment {deployment} starts a pod on it.", img.image),
            (Some(_), _) => format!("Up to date: the {} image on {node} holds this build. Load a newer kiln image onto the nodes, then check again.", img.image),
            (None, Some(e)) => format!("kiln couldn't check which build the {} image holds ({e}). A restart starts a pod on whatever image is on the node.", img.image),
            (None, None) => "Checking which build the image on the node holds…".to_owned(),
        };
        s.deployment = Some(deployment);
        s.namespace = Some(ns);
        return s;
    }
    let mine = own_binary().ok();
    let upload = stored(hub, crate::api::TARGET).filter(|b| mine.as_ref().is_none_or(|(h, _)| *h != b.sha256));
    let replacement = match (&upload, &release) {
        (Some(b), _) => Some(Replacement { origin: "upload", build: Some(b.build.clone()), version: None }),
        (None, r) if r.newer && own_release_target().is_some() => Some(Replacement { origin: "release", build: None, version: r.latest.clone() }),
        _ => None,
    };
    let sup = supervised();
    s.mode = if sup { "service" } else { "standalone" };
    s.newer = replacement.is_some();
    s.action = (sup && replacement.is_some()).then_some("replace");
    s.note = match (&replacement, sup) {
        (Some(r), true) if r.origin == "upload" => format!(
            "The uploaded build {} is ready. Installing it replaces this server's binary; kiln's supervisor restarts the server into it within seconds.",
            r.build.as_deref().unwrap_or("?")
        ),
        (Some(r), true) => format!(
            "kiln {} is released. Installing it downloads it from GitHub and replaces this server's binary; kiln's supervisor restarts the server into it within seconds.",
            r.version.as_deref().unwrap_or("?")
        ),
        (Some(_), false) => "A newer build is ready, but this server isn't running under its supervisor (`kiln supervise`), so it can't restart itself: replace the binary and restart it the way you started it.".to_owned(),
        (None, _) if release.checked_at == 0 => "Checking GitHub for a newer release…".to_owned(),
        (None, _) if release.error.is_some() => "No newer build here, and GitHub couldn't be asked. Upload a kiln binary for this platform to install it.".to_owned(),
        (None, _) if release.latest.is_none() => "No newer build: no kiln release is published on GitHub yet. Upload a kiln binary for this platform to install it.".to_owned(),
        (None, _) => format!("Up to date: kiln {} is the latest release. Upload a kiln binary for this platform to install another build.", env!("CARGO_PKG_VERSION")),
    };
    s.replacement = replacement;
    s
}

/// Upgrade the server: a rollout restart (Kubernetes) or its binary replaced (supervised).
pub async fn upgrade_server(hub: &Arc<Hub>, action: &str) -> anyhow::Result<Job> {
    if let Some(j) = hub.updates.running_for("server", "server") {
        return Ok(j);
    }
    let info = server(hub, false);
    match action {
        "restart" => {
            let (deployment, ns) = kube_identity().ok_or_else(|| anyhow::anyhow!("kiln isn't running in Kubernetes"))?;
            let to = info.image.as_ref().and_then(|i| i.build.clone());
            let job = new_job("server", "server", "restart", Some(info.build.clone()), to);
            let id = job.id.clone();
            hub.updates.insert(job.clone());
            hub.store.audit("admin".into(), "upgrade.server".into(), format!("rollout restart of {ns}/{deployment}")).await;
            hub.updates.say(hub, &id, format!("restarting Deployment {ns}/{deployment}"));
            let res = match crate::kubeapi::KubeApi::discover(&ns) {
                Ok(api) => api.restart_deployment(&deployment).await,
                Err(e) => Err(e),
            };
            match res {
                Ok(()) => hub.updates.finish(
                    hub,
                    &id,
                    Ok("Kubernetes is starting the new pod; this page reconnects when it is up. Terminals and agents on runners keep running.".to_owned()),
                ),
                Err(e) => {
                    let forbidden = e.downcast_ref::<crate::kubeapi::ApiError>().is_some_and(|a| a.code == 403);
                    hub.updates.finish(
                        hub,
                        &id,
                        Err(if forbidden {
                            format!(
                                "kiln's service account may not restart Deployment {deployment}: apply the current kiln chart (its Role allows get and patch on that Deployment). ({e:#})"
                            )
                        } else {
                            format!("{e:#}")
                        }),
                    );
                }
            }
            Ok(hub.updates.job(&id).unwrap_or(job))
        }
        "replace" => {
            anyhow::ensure!(info.mode == "service", "kiln isn't running under its supervisor, so it can't restart into a new binary");
            let r = info.replacement.clone().ok_or_else(|| anyhow::anyhow!("there is no newer kiln binary for {}: upload one first", crate::api::TARGET))?;
            let exe = std::env::current_exe()?;
            let job = new_job("server", "server", "replace", Some(info.build.clone()), r.build.clone());
            let id = job.id.clone();
            hub.updates.insert(job.clone());
            hub.store.audit("admin".into(), "upgrade.server".into(), format!("replace {} ({})", exe.display(), r.origin)).await;
            let h = Arc::clone(hub);
            let id2 = id.clone();
            crate::rt::spawn(async move {
                let res = replace_binary(&h, &id2, &r, exe).await;
                h.updates.finish(
                    &h,
                    &id2,
                    res.map(|()| "installed; kiln's supervisor restarts the server into it within seconds and this page reconnects".to_owned()).map_err(|e| format!("{e:#}")),
                );
            });
            Ok(job)
        }
        other => anyhow::bail!("unknown server action {other:?} (restart or replace)"),
    }
}

async fn replace_binary(hub: &Arc<Hub>, id: &str, r: &Replacement, exe: PathBuf) -> anyhow::Result<()> {
    let u = &hub.updates;
    let bytes = if r.origin == "upload" {
        stored(hub, crate::api::TARGET).ok_or_else(|| anyhow::anyhow!("the uploaded binary is gone"))?;
        crate::blobs::io::get_all(hub.blobs.blobs.as_ref(), &binary_key(crate::api::TARGET), MAX_BINARY as u64).await.map_err(|e| anyhow::anyhow!("reading the uploaded binary: {e}"))?.1
    } else {
        let version = r.version.clone().ok_or_else(|| anyhow::anyhow!("no release version"))?;
        let target = own_release_target().ok_or_else(|| anyhow::anyhow!("releases have no build for this platform"))?;
        u.say(hub, id, format!("downloading kiln {version} for {target} from GitHub"));
        download_release(&version, target).await?.ok_or_else(|| anyhow::anyhow!("the v{version} release has no binary for {target}"))?
    };
    anyhow::ensure!(executable(&bytes), "that binary is not an executable");
    let build = build_id(&hex::encode(Sha256::digest(&bytes)));
    anyhow::ensure!(build != own_build(), "that is the build already running");
    u.update(hub, id, |j| j.to_build = Some(build.clone()));
    u.say(hub, id, format!("installing build {build} over {}", exe.display()));
    crate::rt::blocking(move || -> anyhow::Result<()> {
        let dir = exe.parent().ok_or_else(|| anyhow::anyhow!("no directory for {}", exe.display()))?;
        let tmp = dir.join(format!(".kiln.upgrade.{}", std::process::id()));
        std::fs::write(&tmp, &bytes).with_context(|| format!("write {} (may kiln write there?)", tmp.display()))?;
        crate::os::make_executable(&tmp)?;
        std::fs::rename(&tmp, &exe).with_context(|| format!("replace {}", exe.display()))?;
        Ok(())
    })
    .await
    .and_then(|r| r)
}

// ------------------------------------------------------------------ the runner's side

static UPGRADING: AtomicBool = AtomicBool::new(false);

/// Whether this runner is fetching an upgrade now (`crate::doctor` leaves the caches alone meanwhile).
pub fn upgrading() -> bool {
    UPGRADING.load(Ordering::Acquire)
}

/// The downloaded binary and its install's output, in the runner's own directory.
fn upgrade_files(own_dir: &Path) -> (PathBuf, PathBuf) {
    (own_dir.join(if cfg!(windows) { "kiln-upgrade.exe" } else { "kiln-upgrade" }), own_dir.join("kiln-upgrade.log"))
}

/// Remove what an upgrade left behind (a runner that took the new binary in place restarted
/// before it could clean up).
pub fn clean_leftovers(own_dir: &Path) {
    let (bin, _) = upgrade_files(own_dir);
    let _ = std::fs::remove_file(bin);
}

/// What a runner is told to upgrade to ([`ServerMsg::Upgrade`]).
pub struct Order<'a> {
    pub job: &'a str,
    pub path: &'a str,
    pub sha256: &'a str,
    pub size: u64,
}

/// [`ServerMsg::Upgrade`] on a runner: fetch the binary from the server, check it, run its
/// `install --agent --upgrade --service <service>` (beside the installed service, or in place
/// for a runner set up that way), reporting each line of its output. Nothing this runner runs is
/// stopped by it.
pub async fn run_on_runner(http: &crate::http::client::Client, server_url: &str, order: &Order<'_>, own_dir: &Path, service: &str, up: &crate::exec::Uplink) -> anyhow::Result<()> {
    anyhow::ensure!(!UPGRADING.swap(true, Ordering::AcqRel), "an upgrade is already running on this runner");
    let r = upgrade_here(http, server_url, order, own_dir, service, up).await;
    UPGRADING.store(false, Ordering::Release);
    r
}

/// Download the binary `order` names from the server into `bin`, and check it: Ok with its build.
async fn download_verified(http: &crate::http::client::Client, server_url: &str, order: &Order<'_>, bin: &Path, up: &crate::exec::Uplink) -> anyhow::Result<String> {
    use std::io::Write as _;
    let say = |line: String| up.msg(AgentMsg::UpgradeLog { job: order.job.to_owned(), line });
    let (path, sha256, size) = (order.path, order.sha256, order.size);
    anyhow::ensure!(path.starts_with("/dl/kiln/") && !path.contains(".."), "refusing to fetch {path}");
    anyhow::ensure!(usize::try_from(size).is_ok_and(|s| s <= MAX_BINARY), "the binary is too large ({size} bytes)");
    let url = format!("{}{path}", server_url.trim_end_matches('/'));
    say(format!("downloading {url}")).await;
    let mut resp = http.get(&url).timeout(Duration::from_secs(600)).send().await?;
    anyhow::ensure!(resp.is_success(), "the server answered {} for {url}", resp.status());
    let mut file = std::fs::File::create(bin).with_context(|| format!("create {}", bin.display()))?;
    let mut h = Sha256::new();
    let mut got = 0u64;
    while let Some(chunk) = resp.chunk().await? {
        got += chunk.len() as u64;
        if got > size {
            drop(file);
            let _ = std::fs::remove_file(bin);
            anyhow::bail!("the download is larger than announced ({size} bytes)");
        }
        h.update(&chunk);
        file.write_all(&chunk)?;
    }
    file.sync_all()?;
    drop(file);
    let hash = hex::encode(h.finalize());
    if hash != sha256 || got != size {
        let _ = std::fs::remove_file(bin);
        anyhow::bail!("the download does not match: sha256 {hash}, {got} bytes; expected {sha256}, {size} bytes");
    }
    crate::os::make_executable(bin)?;
    let build = build_id(&hash);
    say(format!("verified build {build} ({got} bytes)")).await;
    Ok(build)
}

/// For a blue/green upgrade (`crate::bluegreen`): the binary `order` names, downloaded, checked and
/// kept under `<state>/versions/<build>/`, where the new build's runner runs from (nothing that
/// runs is replaced). Its path and build.
pub async fn fetch_build(http: &crate::http::client::Client, server_url: &str, order: &Order<'_>, state_dir: &Path, up: &crate::exec::Uplink) -> anyhow::Result<(PathBuf, String)> {
    anyhow::ensure!(!UPGRADING.swap(true, Ordering::AcqRel), "an upgrade is already running on this runner");
    let r = async {
        let versions = state_dir.join("versions");
        let mut mk = std::fs::DirBuilder::new();
        mk.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut mk, 0o700);
        mk.create(&versions)?;
        let part = versions.join(format!(".download-{}", std::process::id()));
        let build = download_verified(http, server_url, order, &part, up).await?;
        let dir = versions.join(&build);
        mk.create(&dir)?;
        let exe = dir.join(if cfg!(windows) { "kiln.exe" } else { "kiln" });
        // On a Mac, signed as kiln (this machine's identity): macOS knows it as the program it
        // let onto the local network already.
        if let Err(e) = crate::os::macsign::sign_as(&part, &exe, &build) {
            tracing::warn!(error = %format!("{e:#}"), "couldn't sign the new build: macOS may ask again before it reaches the server");
        }
        std::fs::rename(&part, &exe).with_context(|| format!("keep {}", exe.display()))?;
        Ok((exe, build))
    }
    .await;
    UPGRADING.store(false, Ordering::Release);
    r
}

/// Every build kept under `<state>/versions/` but `keep`'s (the one running) and the newest few:
/// removed.
pub fn prune_versions(state_dir: &Path, keep: &str) {
    let Ok(entries) = std::fs::read_dir(state_dir.join("versions")) else { return };
    let mut dirs: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_str().is_some_and(|n| n != keep && !n.starts_with('.')))
        .filter_map(|e| Some((e.metadata().and_then(|m| m.modified()).ok()?, e.path())))
        .collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.0));
    // The newest two besides the running one stay: what a rollback goes back to.
    for (_, d) in dirs.into_iter().skip(2) {
        let _ = std::fs::remove_dir_all(d);
    }
}

async fn upgrade_here(http: &crate::http::client::Client, server_url: &str, order: &Order<'_>, own_dir: &Path, service: &str, up: &crate::exec::Uplink) -> anyhow::Result<()> {
    use std::io::{Read as _, Seek as _};
    let say = |line: String| up.msg(AgentMsg::UpgradeLog { job: order.job.to_owned(), line });
    let (bin, log_path) = upgrade_files(own_dir);
    download_verified(http, server_url, order, &bin, up).await?;
    say(format!("kiln install --agent --upgrade --service {service}")).await;

    // Its output goes to a file, not a pipe: a runner that takes the binary in place re-executes
    // itself while the install runs, and the install must not die writing to a closed pipe.
    let log = std::fs::File::create(&log_path).with_context(|| format!("create {}", log_path.display()))?;
    let mut cmd = std::process::Command::new(&bin);
    cmd.args(["install", "--agent", "--upgrade", "--service", service]).stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().with_context(|| format!("run {}", bin.display()))?;
    let started = Instant::now();
    let mut offset = 0u64;
    let mut pending = String::new();
    let status = loop {
        crate::rt::sleep(Duration::from_millis(300)).await;
        let exited = child.try_wait()?;
        if let Ok(mut f) = std::fs::File::open(&log_path)
            && f.seek(std::io::SeekFrom::Start(offset)).is_ok()
        {
            let mut buf = Vec::new();
            if let Ok(n) = f.read_to_end(&mut buf) {
                offset += n as u64;
                pending.push_str(&String::from_utf8_lossy(&buf));
            }
        }
        while let Some(i) = pending.find('\n') {
            let line: String = pending.drain(..=i).collect();
            if !line.trim().is_empty() {
                say(line.trim_end().to_owned()).await;
            }
        }
        if let Some(st) = exited {
            break st;
        }
        if started.elapsed() > INSTALL_TIMEOUT {
            let _ = child.kill();
            let _ = std::fs::remove_file(&bin);
            anyhow::bail!("install --agent --upgrade didn't finish in 15 minutes");
        }
    };
    if !pending.trim().is_empty() {
        say(pending.trim_end().to_owned()).await;
    }
    let _ = std::fs::remove_file(&bin);
    anyhow::ensure!(status.success(), "install --agent --upgrade exited with {status}");
    Ok(())
}

/// The service a runner process belongs to: the one it replaces (a runner set up beside another
/// upgrades in place through that), else its own unit (Linux: from its cgroup; macOS: from the
/// label of the launchd job that started it), else the default.
pub fn runner_service(state_dir: &Path, instance: Option<&str>) -> String {
    if let Some(id) = instance
        && let Ok(b) = std::fs::read(crate::upgrade::instance_dir(state_dir, id).join("replaces.json"))
        && let Ok(r) = serde_json::from_slice::<crate::upgrade::Replaces>(&b)
    {
        return r.unit;
    }
    // launchd names its job here (`io.github.hyper-light.<unit>`); a runner started otherwise
    // (by hand, from a terminal) has "0" or nothing.
    if cfg!(target_os = "macos")
        && let Some(unit) = std::env::var("XPC_SERVICE_NAME").ok().and_then(|l| l.strip_prefix("io.github.hyper-light.").map(str::to_owned))
        && unit.starts_with("kiln")
    {
        return unit;
    }
    #[cfg(target_os = "linux")]
    if let Ok(cg) = std::fs::read_to_string("/proc/self/cgroup")
        && let Some(unit) = cg.lines().filter_map(|l| l.rsplit('/').next()).find_map(|seg| seg.strip_suffix(".service"))
        && unit.starts_with("kiln")
    {
        return unit.to_owned();
    }
    "kiln-agent".to_owned()
}

/// What a runner announces after Hello ([`AgentMsg::Build`]): what this build can do,
/// `CAP_DOCKER` when a container CLI works here (docs/ci.md W13-R01: probed until it answers,
/// waited for 10 s at a time), and `CAP_FIRECRACKER` when it can boot microVMs (W13-R05).
pub async fn runner_build() -> AgentMsg {
    let docker = if crate::rt::blocking(crate::runner::docker::available).await.unwrap_or(false) { crate::proto::CAP_DOCKER } else { 0 };
    #[cfg(target_os = "linux")]
    let microvms = if crate::runner::firecracker::Machine::find().is_some() { crate::proto::CAP_FIRECRACKER } else { 0 };
    #[cfg(not(target_os = "linux"))]
    let microvms = 0;
    // Under a supervisor that can start a new build beside it (`kiln supervise` on Unix).
    let blue_green = if crate::bluegreen::Supervisor::from_env().is_some() { crate::proto::CAP_BLUE_GREEN } else { 0 };
    // Rootless engines are where untrusted legs may run (docs/ci.md W18-R07).
    let rootless = if docker != 0 && crate::rt::blocking(crate::runner::docker::rootless_engine).await.unwrap_or(false) { crate::proto::CAP_DOCKER_ROOTLESS } else { 0 };
    AgentMsg::Build { build: own_build(), caps: RUNNER_CAPS | docker | microvms | blue_green | rootless }
}

/// What a runner says of itself after its build ([`AgentMsg::Facts`]): for now, the kiln it
/// runs and where its kiln checkout is (`crate::fleet`).
pub fn runner_facts() -> AgentMsg {
    let facts = kiln_ci::facts::RunnerFacts {
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        kiln: Some(kiln_facts()),
        probed_at: crate::util::now_unix(),
        ..kiln_ci::facts::RunnerFacts::default()
    };
    AgentMsg::Facts { json: serde_json::to_string(&facts).unwrap_or_default() }
}

/// The kiln this runner runs, and where kiln's source is on its machine.
pub fn kiln_facts() -> kiln_ci::facts::KilnFacts {
    kiln_ci::facts::KilnFacts {
        commit: crate::api::COMMIT.to_owned(),
        build: own_build(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        target: crate::api::TARGET.to_owned(),
        checkout: kiln_checkout().map(|p| p.to_string_lossy().into_owned()),
    }
}

/// A checkout of kiln on this machine: `KILN_SOURCE`, else one of the usual places in the home
/// directory (`crate::fleet::USUAL`).
pub fn kiln_checkout() -> Option<PathBuf> {
    let looks = |p: &Path| p.join("crates/kiln/Cargo.toml").is_file() && p.join("ui/package.json").is_file();
    if let Some(p) = std::env::var_os("KILN_SOURCE").map(PathBuf::from).filter(|p| looks(p)) {
        return Some(p);
    }
    let home = crate::os::home_dir();
    crate::fleet::USUAL.iter().map(|u| home.join(u)).find(|p| looks(p))
}

/// Everything this build's runner can do ([`AgentMsg::Build`]).
pub const RUNNER_CAPS: u32 = CAP_UPGRADE
    | crate::proto::CAP_DOCTOR
    | crate::proto::CAP_UPDATES
    | crate::proto::CAP_FACTS
    | crate::proto::CAP_TAKEOVER
    | crate::proto::CAP_DIRS
    | crate::proto::CAP_TAKEOVER_STOP
    | crate::proto::CAP_SYNC
    | crate::proto::CAP_REPOS
    | crate::proto::CAP_FRONT_DOOR
    | crate::proto::CAP_CI2
    | crate::proto::CAP_ACTIONS
    | crate::proto::CAP_REACH
    | crate::proto::CAP_JOB_TTY
    | crate::proto::CAP_FILES
    | crate::proto::CAP_STREAM
    | crate::proto::CAP_PREVIEW
    | crate::proto::CAP_PORTS
    | crate::proto::CAP_ENGINES
    | crate::proto::CAP_WORKSPACE
    | crate::proto::CAP_POOLS
    | crate::proto::CAP_RECEIVED
    | crate::proto::CAP_UNDEPLOY
    | crate::proto::CAP_CLUSTER;

/// What an outpost (`kiln outpost`) does ([`AgentMsg::Build`]): a runner's terminals, AI sessions
/// and agents (taking a session over, listing folders), none of its jobs, upgrades, front door,
/// repo copies or reach.
pub const OUTPOST_CAPS: u32 =
    crate::proto::CAP_OUTPOST | crate::proto::CAP_TAKEOVER | crate::proto::CAP_DIRS | crate::proto::CAP_TAKEOVER_STOP | crate::proto::CAP_FILES;

/// An outpost's [`AgentMsg::Build`].
pub fn outpost_build() -> AgentMsg {
    AgentMsg::Build { build: own_build(), caps: OUTPOST_CAPS }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A runner upgrade kiln gave up on: the machine got there later.
    fn given_up(blue_green: bool) -> Job {
        let mut j = new_job("runner", "adacachy", "native", Some("1e8fb739442b".into()), Some("493003c712b0".into()));
        j.blue_green = blue_green;
        j.state = JobState::Failed;
        j.error = Some("the new runner didn't report in within 15 minutes; see the runner's log".into());
        j.timed_out = Some("upgrade");
        j
    }

    #[test]
    fn an_upgrade_that_got_there_late_is_upgraded_after_all() {
        let mut j = given_up(false);
        assert!(j.awaits("adacachy", "493003c712b0"), "a job given up on still waits for its build");
        assert!(!j.awaits("adacachy", "another"), "only for its own build");
        assert!(!j.awaits("fedora", "493003c712b0"), "and its own machine");
        j.reached = true;
        assert!(j.through_late(42));
        assert_eq!((j.state, j.error.as_deref(), j.timed_out, j.ended_at), (JobState::Done, None, None, Some(42)));
        assert!(j.log.last().is_some_and(|l| l.starts_with("upgraded after all") && l.contains("15 minutes")), "{:?}", j.log);
        assert!(!j.through_late(43), "once");
        assert!(!j.awaits("adacachy", "493003c712b0"), "done: it waits for nothing");
    }

    #[test]
    fn a_given_up_upgrade_that_arrived_on_a_build_signed_again_is_upgraded_after_all() {
        let mut j = given_up(false);
        j.commit = Some("94f0847a1b2c3d4e5f60718293a4b5c6d7e8f901".into());
        // The Mac signed the build again: it reports another build id, of the same commit.
        assert!(!j.awaits("adacachy", "7c0ffee12345"), "not the build it was handed");
        assert!(arrived_by_commit(&j, Some("7c0ffee12345"), Some("94F0847A1B2C3D4E5F60718293A4B5C6D7E8F901")));
        assert!(!arrived_by_commit(&j, Some("1e8fb739442b"), Some("94f0847a1b2c3d4e5f60718293a4b5c6d7e8f901")), "still the build it ran before");
        j.reached = true;
        assert!(j.through_late(7));
        assert_eq!(j.state, JobState::Done);
    }

    #[test]
    fn a_blue_green_upgrade_needs_its_handover_too() {
        let mut j = given_up(true);
        j.reached = true;
        assert!(!j.through_late(1), "the new build reported in, but the old runner never said it took over");
        assert_eq!(j.state, JobState::Failed);
        j.handed_over = true;
        assert!(j.through_late(2));
        assert_eq!(j.state, JobState::Done);
    }

    #[test]
    fn a_failure_its_machine_never_got_past_stays_failed() {
        let mut j = given_up(false);
        assert!(!j.through_late(1), "nothing reported in");
        let mut running = given_up(false);
        running.state = JobState::Running;
        running.reached = true;
        assert!(!running.through_late(1), "a running job finishes the usual way");
    }

    #[test]
    fn waits_come_from_kiln_toml_and_stay_within_reason() {
        let w = Waits::of(&crate::config::Updates::default());
        assert_eq!((w.upgrade(), w.jobs(), w.build()), (Duration::from_secs(15 * 60), Duration::from_secs(6 * 3600), Duration::from_secs(3600)));
        assert!(w.check().is_ok());
        let odd = Waits { upgrade_minutes: 0, wait_for_jobs_minutes: 1_000_000, build_minutes: 2 };
        assert!(odd.check().is_err_and(|e| e.to_string().contains("upgrade_minutes is 0")), "a change is refused");
        assert_eq!(odd.clamped(), Waits { upgrade_minutes: 1, wait_for_jobs_minutes: 7 * 24 * 60, build_minutes: 5 }, "a kiln.toml is brought within");
        assert!(serde_json::from_str::<Waits>(r#"{"upgrade_minutes": 30, "wait_for_jobs_minutes": 60, "build_minutes": 90, "other": 1}"#).is_err(), "nothing unknown");
    }

    #[test]
    fn waits_read_as_a_person_says_them() {
        let said_of = |m: u64| said(minutes(m));
        assert_eq!([said_of(1), said_of(15), said_of(60), said_of(90), said_of(120), said_of(360)], ["a minute", "15 minutes", "an hour", "90 minutes", "2 hours", "6 hours"]);
    }

    #[test]
    fn runners_announce_every_capability_they_have() -> anyhow::Result<()> {
        use crate::proto::{CAP_DIRS, CAP_FRONT_DOOR, CAP_TAKEOVER, CAP_TAKEOVER_STOP};
        let AgentMsg::Build { caps, build } = crate::rt::block_on(async { Ok(runner_build().await) })? else { anyhow::bail!("runner_build is not a Build") };
        assert!(!build.is_empty());
        for cap in [CAP_UPGRADE, CAP_TAKEOVER, CAP_DIRS, CAP_TAKEOVER_STOP, CAP_FRONT_DOOR] {
            assert_eq!(caps & cap, cap, "capability {cap} is announced");
        }
        Ok(())
    }

    #[test]
    fn only_releases_from_a_tag_or_the_release_branch_are_adopted() {
        let u = crate::config::Updates::default();
        let run = |pipeline: &str, event: &str, branch: &str| crate::store::Run {
            id: "r".into(),
            repo_id: "k".into(),
            repo_name: "hyper-light/kiln".into(),
            number: 1,
            pipeline: pipeline.into(),
            event: event.into(),
            branch: branch.into(),
            sha: "0d21482abcdef".into(),
            message: String::new(),
            author: String::new(),
            status: crate::store::Status::Succeeded,
            created_at: 0,
            started_at: None,
            finished_at: None,
            error: None,
            concurrency_group: None,
        };
        assert!(adopts(&u, &run("release", "manual", "main"), ""));
        assert!(adopts(&u, &run("release", "tag", "v0.2.0"), ""));
        assert!(!adopts(&u, &run("release", "manual", "try-something"), ""));
        assert!(!adopts(&u, &run("ci", "push", "main"), ""));
        let mut other = run("release", "manual", "main");
        other.repo_name = "hyper-light/focal".into();
        assert!(!adopts(&u, &other, ""));
        // A server that knows its commit takes only builds of that commit: none ahead, none behind.
        assert!(adopts(&u, &run("release", "manual", "main"), "0D21482ABCDEF"));
        assert!(!adopts(&u, &run("release", "manual", "main"), "1111111111111"));
        assert!(!adopts(&u, &run("release", "tag", "v0.2.0"), "1111111111111"));
    }

    #[test]
    fn every_other_platform_is_in_step_once_its_binary_is_built_from_the_servers_commit() {
        let others = ["aarch64-apple-darwin", "x86_64-pc-windows-msvc"];
        let built = |target: &str, sha: Option<&str>, origin: &str| Binary {
            target: target.into(),
            sha256: String::new(),
            size: 0,
            origin: origin.into(),
            at: 0,
            build: String::new(),
            run: None,
            sha: sha.map(str::to_owned),
            by: None,
        };
        let c = "a".repeat(40);
        // Nothing kept yet: every platform is behind.
        let none = in_step_of(&others, &[], &c);
        assert_eq!((none.in_step, none.behind.len()), (false, 2));
        // One from an older commit, one uploaded (no commit): both behind.
        let old = in_step_of(&others, &[built(others[0], Some(&"b".repeat(40)), "built"), built(others[1], None, "upload")], &c);
        assert_eq!(old.behind, vec![others[0].to_owned(), others[1].to_owned()]);
        // Both built from it (in any case): in step.
        let now = in_step_of(&others, &[built(others[0], Some(&c), "built"), built(others[1], Some(&c.to_ascii_uppercase()), "built")], &c);
        assert_eq!((now.in_step, now.behind.len(), now.commit.as_deref()), (true, 0, Some(c.as_str())));
        // A server that doesn't know its commit is never in step with one.
        assert!(!in_step_of(&others, &[built(others[0], Some(&c), "built")], "").in_step);
    }

    #[test]
    fn the_servers_commit_is_built_once_waited_for_until_pushed_and_given_up_after_three_failed_builds() {
        let run = |status: crate::store::Status, finished: Option<i64>| crate::store::Run {
            id: String::new(),
            repo_id: String::new(),
            repo_name: "hyper-light/kiln".into(),
            number: 1,
            pipeline: "release".into(),
            event: "manual".into(),
            branch: "main".into(),
            sha: "c".repeat(40),
            message: String::new(),
            author: String::new(),
            status,
            created_at: finished.unwrap_or(0),
            started_at: None,
            finished_at: finished,
            error: None,
            concurrency_group: None,
        };
        use crate::store::Status::{Failed, Running, Succeeded};
        let now = 1_000_000;
        let none = Starts::default();
        assert_eq!(next(&[], &none, now), Next::Build);
        assert_eq!(next(&[run(Running, None)], &none, now), Next::Nothing);
        assert_eq!(next(&[run(Succeeded, Some(now - 10))], &none, now), Next::Nothing);
        assert_eq!(next(&[run(Failed, Some(now - 60))], &none, now), Next::Wait);
        assert_eq!(next(&[run(Failed, Some(now - 3600))], &none, now), Next::Build);
        // A start that failed (the commit wasn't pushed yet) waits 10 minutes, then tries again, for
        // as long as it takes: only builds that ran and failed count.
        assert_eq!(next(&[], &Starts { last: Some(now - 60), ..Starts::default() }, now), Next::Wait);
        assert_eq!(next(&[], &Starts { last: Some(now - 11 * 60), ..Starts::default() }, now), Next::Build);
        assert_eq!(next(&[run(Failed, Some(now - 7200))], &Starts { last: Some(now - 11 * 60), ..Starts::default() }, now), Next::Build);
        let failed3 = [run(Failed, Some(now - 9000)), run(Failed, Some(now - 8000)), run(Failed, Some(now - 7200))];
        assert_eq!(next(&failed3, &none, now), Next::Nothing);
    }

    /// A server just started took a runner's older commit as the newest for a moment (its own not
    /// fetched yet), and kept that commit's binaries over its own: back on its own, its release
    /// run's binaries are kept again (2026-10-20: jupiter waited for a Windows build of 5806c0d
    /// its server had, overwritten by df17c34's).
    #[test]
    fn the_newest_moving_back_keeps_its_binaries_again() {
        let mut starts = Starts { last: Some(5), ..Starts::default() };
        let mut was = String::new();
        starts.moved("5806c0d", &mut was);
        starts.kept.insert("release-597-windows".to_owned());
        starts.moved("5806c0d", &mut was);
        assert!(starts.kept.contains("release-597-windows"), "still the same commit: nothing kept again");
        starts.moved("df17c34", &mut was);
        starts.kept.insert("release-525-windows".to_owned());
        starts.moved("5806c0d", &mut was);
        assert!(starts.kept.is_empty() && starts.last.is_none() && was == "5806c0d", "{starts:?}");
    }

    #[test]
    fn the_release_pipelines_binaries_are_kept_for_the_other_platforms() -> anyhow::Result<()> {
        crate::rt::block_on(async { release_binaries_are_kept().await })
    }

    async fn release_binaries_are_kept() -> anyhow::Result<()> {
        let hub = crate::hub::tests::scratch_hub().await?;
        let dir = tempfile::tempdir()?;
        let fake = |magic: &[u8]| -> Vec<u8> {
            let mut b = magic.to_vec();
            b.resize(2 << 20, 7);
            b
        };
        // A Mac's and Windows' binaries, and the server's own platform's (one of those two on a
        // server that is one).
        const OTHERS: [&str; 2] = ["aarch64-apple-darwin", "x86_64-pc-windows-msvc"];
        let mut entries = vec![("dist/kiln-aarch64-apple-darwin".to_owned(), fake(&[0xcf, 0xfa, 0xed, 0xfe])), ("dist/kiln-x86_64-pc-windows-msvc.exe".to_owned(), fake(b"MZ"))];
        if !OTHERS.contains(&crate::api::TARGET) {
            entries.push((format!("dist/kiln-{}", crate::api::TARGET), fake(b"\x7fELF")));
        }
        entries.push(("dist/SHA256SUMS".to_owned(), b"sums".to_vec()));
        let mut tar = tar::Builder::new(Vec::new());
        for (name, bytes) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_size(bytes.len() as u64);
            h.set_mode(0o755);
            h.set_cksum();
            tar.append_data(&mut h, &name, bytes.as_slice())?;
        }
        let artifacts = dir.path().join("artifacts.tar");
        std::fs::write(&artifacts, tar.into_inner()?)?;
        let run = crate::store::Run {
            id: "r".into(),
            repo_id: "k".into(),
            repo_name: "hyper-light/kiln".into(),
            number: 12,
            pipeline: "release".into(),
            event: "manual".into(),
            branch: "main".into(),
            sha: "0d21482abcdef".into(),
            message: String::new(),
            author: String::new(),
            status: crate::store::Status::Succeeded,
            created_at: 0,
            started_at: None,
            finished_at: None,
            error: None,
            concurrency_group: None,
        };
        // A run of a newer commit than the server's (`crate::fleet`): every platform's, the
        // server's own included.
        let kept = adopt_into(&hub, &artifacts, &run).await?;
        let mut targets: Vec<&str> = kept.iter().map(|b| b.target.as_str()).collect();
        targets.sort_unstable();
        let mut all = vec!["aarch64-apple-darwin", "x86_64-pc-windows-msvc", crate::api::TARGET];
        all.sort_unstable();
        all.dedup();
        assert_eq!(targets, all);
        assert!(kept.iter().all(|b| b.origin == "built" && b.run == Some(12) && b.sha.as_deref() == Some("0d21482abcdef")));
        // Kept in kiln's store of bytes, and read back as kept when the server starts again.
        let mac = hub.blobs.blobs.head(&binary_key("aarch64-apple-darwin")).await?;
        assert_eq!((mac.size, Some(mac.etag)), (2 << 20, stored(&hub, "aarch64-apple-darwin").map(|b| b.sha256)));
        hub.updates.kept.lock().clear();
        load(&hub).await;
        assert_eq!(stored(&hub, "aarch64-apple-darwin").map(|b| b.run), Some(Some(12)));
        // A run of the server's own commit: its platform keeps the server's own build.
        if !crate::api::COMMIT.is_empty() {
            let own = crate::store::Run { sha: crate::api::COMMIT.to_owned(), ..run };
            let kept = adopt_into(&hub, &artifacts, &own).await?;
            let mut targets: Vec<&str> = kept.iter().map(|b| b.target.as_str()).collect();
            targets.sort_unstable();
            let others: Vec<&str> = OTHERS.into_iter().filter(|t| *t != crate::api::TARGET).collect();
            assert_eq!(targets, others, "this server's own platform keeps its own build");
        }
        Ok(())
    }

    /// CP-T10.1: binaries kept as files (`<data>/binaries`) are taken into the local store where
    /// they lie; one an older kiln kept since (run on the store again: a rollback) replaces the
    /// store's older one, and an older file leaves the store's newer one alone.
    #[test]
    fn binaries_kept_as_files_are_taken_in_and_the_newest_is_kept() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let local = hub.blobs.local.clone().ok_or_else(|| anyhow::anyhow!("not the local store"))?;
            let (dir, t) = (old_dir(&hub.cfg), crate::api::TARGET);
            std::fs::create_dir_all(&dir)?;
            let elf = |n: u8| -> Vec<u8> {
                let mut b = b"\x7fELF".to_vec();
                b.resize(2 << 20, n);
                b
            };
            // As an older kiln keeps one: the binary and what it knows of it, as of `at`.
            let as_files = |bytes: &[u8], at: std::time::SystemTime| -> anyhow::Result<String> {
                let sha256 = crate::util::sha256_hex(bytes);
                let meta = Binary { target: t.into(), sha256: sha256.clone(), size: bytes.len() as u64, origin: "upload".into(), at: 0, build: String::new(), run: None, sha: None, by: None };
                let bin = dir.join(format!("kiln-{t}"));
                std::fs::write(&bin, bytes)?;
                std::fs::write(dir.join(format!("kiln-{t}.json")), serde_json::to_vec(&meta)?)?;
                std::fs::File::options().write(true).open(&bin)?.set_modified(at)?;
                Ok(sha256)
            };
            let now = std::time::SystemTime::now();
            let first = as_files(&elf(1), now - Duration::from_secs(60))?;
            take_in(&hub, &local).await?;
            load(&hub).await;
            assert_eq!(stored(&hub, t).map(|b| b.sha256), Some(first));
            // Replaced here: the files it was taken from go.
            let uploaded = store(&hub, t, elf(2), "upload").await?.sha256;
            assert!(!dir.join(format!("kiln-{t}")).exists());
            // Rolled back, the older kiln kept a newer one; started again, it's the one.
            let newer = as_files(&elf(3), now + Duration::from_secs(60))?;
            take_in(&hub, &local).await?;
            load(&hub).await;
            assert_eq!(hub.blobs.blobs.head(&binary_key(t)).await?.etag, newer);
            assert_eq!(stored(&hub, t).map(|b| b.sha256), Some(newer.clone()));
            assert_ne!(uploaded, newer);
            // A file older than what the store keeps changes nothing.
            as_files(&elf(4), now - Duration::from_secs(3600))?;
            take_in(&hub, &local).await?;
            assert_eq!(hub.blobs.blobs.head(&binary_key(t)).await?.etag, newer);
            Ok(())
        })
    }

    #[test]
    fn a_release_binary_cut_short_or_not_a_program_is_never_kept() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut cfg = crate::config::Config::default();
        cfg.server.data_dir = dir.path().to_owned();
        let mut mac = vec![0xcf, 0xfa, 0xed, 0xfe];
        mac.resize(2 << 20, 7);
        // Windows' isn't its program; the Mac's is cut short, as an upload that broke off leaves it.
        let mut tar = tar::Builder::new(Vec::new());
        for (name, bytes) in [("dist/kiln-x86_64-pc-windows-msvc.exe", b"not a program".to_vec()), ("dist/kiln-aarch64-apple-darwin", mac)] {
            let mut h = tar::Header::new_gnu();
            h.set_size(bytes.len() as u64);
            h.set_mode(0o755);
            h.set_cksum();
            tar.append_data(&mut h, name, bytes.as_slice())?;
        }
        let mut archive = tar.into_inner()?;
        archive.truncate(archive.len().saturating_sub(1 << 20));
        let artifacts = dir.path().join("artifacts.tar");
        std::fs::write(&artifacts, archive)?;
        let run = crate::store::Run {
            id: "r".into(),
            repo_id: "k".into(),
            repo_name: "hyper-light/kiln".into(),
            number: 13,
            pipeline: "release".into(),
            event: "push".into(),
            branch: "main".into(),
            sha: "0d21482abcdef".into(),
            message: String::new(),
            author: String::new(),
            status: crate::store::Status::Succeeded,
            created_at: 0,
            started_at: None,
            finished_at: None,
            error: None,
            concurrency_group: None,
        };
        crate::rt::block_on(async move {
            let hub = crate::hub::tests::scratch_hub_with(|c| c.server.data_dir = cfg.server.data_dir.clone()).await?;
            let kept = adopt_into(&hub, &artifacts, &run).await?;
            assert!(kept.is_empty(), "{kept:?}");
            for t in ["aarch64-apple-darwin", "x86_64-pc-windows-msvc"] {
                assert!(matches!(hub.blobs.blobs.head(&binary_key(t)).await, Err(kiln_seams::blobs::Refusal::NotFound(_))), "{t} kept");
            }
            Ok(())
        })
    }

    /// CP-T10.1, AC-T10.1.1: a store that can't answer leaves what kiln knows of each binary as it
    /// was, and says not all of it was read; once it answers, every binary it keeps is read.
    #[test]
    fn binaries_are_read_again_once_the_store_answers() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            use kiln_seams::blobs::{Blobs, Put, SimBlobs, SimLimits};
            let sim: Arc<dyn Blobs> = Arc::new(SimBlobs::new(crate::rt::clock(), SimLimits::default()));
            let t = crate::api::TARGET;
            let binary = |bytes: &[u8]| Binary { target: t.into(), sha256: crate::util::sha256_hex(bytes), size: bytes.len() as u64, origin: "upload".into(), at: 0, build: String::new(), run: None, sha: None, by: None };
            let (older, newer) = (binary(b"older"), binary(b"\x7fELF, newer"));
            sim.put(&binary_key(t), b"\x7fELF, newer".to_vec(), Put::default()).await?;
            sim.put(&meta_key(t), serde_json::to_vec(&newer)?, Put::default()).await?;
            let flaky = crate::blobs::tests::Flaky::down(Arc::clone(&sim));
            let kept = Mutex::new(HashMap::from([(t.to_owned(), older.clone())]));
            assert!(!load_into(&flaky, &kept).await, "the store didn't answer");
            assert_eq!(kept.lock().get(t).map(|b| b.sha256.clone()), Some(older.sha256), "what it knew, it knows still");
            flaky.down.store(false, Ordering::Release);
            assert!(load_into(&flaky, &kept).await);
            assert_eq!(kept.lock().get(t).map(|b| b.sha256.clone()), Some(newer.sha256));
            Ok(())
        })
    }

    #[test]
    fn versions_compare_semantically() {
        assert!(newer_than("0.2.0", "0.1.0"));
        assert!(newer_than("0.10.0", "0.9.9"));
        assert!(!newer_than("0.1.0", "0.1.0"));
        assert!(!newer_than("0.1.0-rc1", "0.1.0"));
        assert!(newer_than("0.1.0", "0.1.0-rc1"));
        assert!(!newer_than("0.0.9", "0.1.0"));
    }

    #[test]
    fn deployments_from_pod_names() {
        assert_eq!(deployment_of("kiln-755cd5cb98-29w8b").as_deref(), Some("kiln"));
        assert_eq!(deployment_of("my-kiln-server-5d4f-abcde").as_deref(), Some("my-kiln-server"));
        assert_eq!(deployment_of("kiln"), None);
    }

    #[test]
    fn platforms() {
        assert_eq!(target_for(std::env::consts::OS, std::env::consts::ARCH).as_deref(), Some(crate::api::TARGET), "same platform: this server's own binary");
        assert_eq!(release_target("macos", "aarch64"), Some("aarch64-apple-darwin"));
        assert_eq!(release_target("linux", "x86_64"), Some("x86_64-unknown-linux-musl"));
        assert_eq!(target_for("plan9", "mips"), None);
        assert!(known_target(crate::api::TARGET));
        assert!(!known_target("../../etc/passwd"));
        assert_eq!(release_asset_url("1.2.3", "x86_64-pc-windows-msvc"), format!("{}/v1.2.3/kiln-1.2.3-x86_64-pc-windows-msvc.exe", crate::api::RELEASES));
    }

    #[test]
    fn the_terminal_script_reads_the_same_in_every_shell() {
        for inst8 in ["", "0199a2b3"] {
            let s = terminal_script("job1", &["http://h:7070/dl/kiln/x".to_owned()], "abc123", inst8);
            // `sh -c '<script>'` must reach sh unchanged from sh, bash, zsh and fish.
            assert!(!s.contains('\''), "no single quotes");
            assert!(!s.contains('\\'), "no backslashes");
            assert!(s.starts_with(HIDDEN_MARK));
            assert!(s.contains("install --agent --upgrade --service \"$svc\""));
            assert!(s.contains("abc123"));
            assert_eq!(s.contains("svc=${svc%-0199a2b3}"), !inst8.is_empty());
        }
    }

    #[test]
    fn the_terminal_script_runs() -> anyhow::Result<()> {
        if std::process::Command::new("curl").arg("--version").output().is_err() {
            return Ok(());
        }
        // A fake `kiln` behind a file:// URL stands in for the download.
        let dir = tempfile::tempdir()?;
        let fake = dir.path().join("kiln");
        std::fs::write(&fake, "#!/bin/sh\necho \"fake kiln $*\"\n")?;
        let sha = hex::encode(Sha256::digest(std::fs::read(&fake)?));
        let url = format!("file://{}", fake.display());
        let script = terminal_script("j", std::slice::from_ref(&url), &sha, "");
        let out = std::process::Command::new("sh").arg("-c").arg(&script).env("TMPDIR", dir.path()).output()?;
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{text}");
        assert!(text.contains("fake kiln install --agent --upgrade --service"), "{text}");
        assert!(text.contains(OK_MARK), "{text}");
        // As a runner's terminal runs it: `sh -c '<script>'` handed to the user's own shell.
        for shell in ["sh", "bash", "zsh", "fish"] {
            if std::process::Command::new(shell).arg("-c").arg("true").output().is_err() {
                continue;
            }
            let out = std::process::Command::new(shell).arg("-c").arg(format!("sh -c '{script}'")).env("TMPDIR", dir.path()).output()?;
            let text = String::from_utf8_lossy(&out.stdout);
            assert!(out.status.success() && text.contains(OK_MARK), "{shell}: {text} {}", String::from_utf8_lossy(&out.stderr));
        }
        let bad = terminal_script("j", &[url], "0000", "");
        let out = std::process::Command::new("sh").arg("-c").arg(&bad).env("TMPDIR", dir.path()).output()?;
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(!out.status.success());
        assert!(text.contains(FAIL_MARK) && text.contains("does not match"), "{text}");
        Ok(())
    }

    #[test]
    fn terminal_output_becomes_plain_lines() {
        let mut screen = Screen::default();
        assert_eq!(screen.feed("\x1b[2mhello\x1b[0m\r\nworld\x1b]0;title\x07!\n"), [Shown::Line("hello".into()), Shown::Line("world!".into())]);
        assert_eq!(screen.feed("a\x1b]2;t\x1b\\b\n"), [Shown::Line("ab".into())]);
    }

    #[test]
    fn a_runner_signed_again_on_its_machine_has_arrived_once_it_reports_the_jobs_commit() {
        let to = "31c0bbaf734c4b130b43dcfaec93cc647c1ce536";
        let mut j = new_job("runner", "mac", "native", Some("89cfa54c0577".into()), Some("b8a054982136".into()));
        j.commit = Some(to.into());
        // Still the build it ran before: not yet, whatever commit it says.
        assert!(!arrived_by_commit(&j, Some("89cfa54c0577"), Some(to)));
        // Another build (its machine signed the new one again) of the job's commit: arrived.
        assert!(arrived_by_commit(&j, Some("46a3145906a2"), Some(&to.to_ascii_uppercase())));
        // Another commit, or none known: not.
        assert!(!arrived_by_commit(&j, Some("46a3145906a2"), Some("561a240d637481421c8a402205027de71b100b2e")));
        assert!(!arrived_by_commit(&j, Some("46a3145906a2"), None));
        // A job that doesn't know its commit waits for its build to be reported.
        j.commit = None;
        assert!(!arrived_by_commit(&j, Some("46a3145906a2"), Some(to)));
    }

    #[test]
    fn a_progress_bar_is_one_line_that_keeps_up() {
        // cargo's: drawn, then drawn over (a carriage return), lines it finishes printed above it.
        let mut screen = Screen::default();
        assert_eq!(screen.feed("   Building [=> ] 1/364: a\r"), [Shown::Progress("   Building [=> ] 1/364: a".into())]);
        assert_eq!(screen.feed("   Building [==> ] 2/364: b\r"), [Shown::Progress("   Building [==> ] 2/364: b".into())]);
        assert_eq!(
            screen.feed("\x1b[K   Compiling kiln\n   Building [===> ] 363/364: kiln\r"),
            [Shown::Line("   Compiling kiln".into()), Shown::Progress("   Building [===> ] 363/364: kiln".into())]
        );
        // A line's CR and LF in two reads: the line, once it ends.
        let mut screen = Screen::default();
        assert_eq!(screen.feed("done\r"), [Shown::Progress("done".into())]);
        assert_eq!(screen.feed("\n"), [Shown::Line("done".into())]);
        // In the job's log it's the last line, drawn over in place, and gives way to the next line.
        let mut j = new_job("runner", "m", "build", None, None);
        push_line(&mut j, "Compiling a".into());
        for p in ["Building 1/3", "Building 2/3"] {
            push_line(&mut j, p.into());
            j.progress = true;
        }
        assert_eq!(j.log, ["Compiling a", "Building 2/3"]);
        push_line(&mut j, "Compiling b".into());
        assert_eq!(j.log, ["Compiling a", "Compiling b"]);
    }

    #[test]
    fn job_bookkeeping() {
        let u = Updates::default();
        let j = new_job("runner", "box", "native", None, Some("abc".into()));
        u.insert(j.clone());
        assert_eq!(u.running_for("runner", "box").map(|x| x.id), Some(j.id.clone()));
        assert!(u.running_for("runner", "other").is_none());
        for _ in 0..(KEEP_JOBS + 5) {
            u.insert(new_job("server", "server", "restart", None, None));
        }
        assert_eq!(u.jobs().len(), KEEP_JOBS);
    }

    #[test]
    fn the_image_check_pod_is_not_a_job_pod() -> anyhow::Result<()> {
        let v = check_pod("kiln-image-check-x", "kiln:dev", "IfNotPresent", Some("minikube-m02"), Some("kiln-job"));
        let pod: k8s_openapi::api::core::v1::Pod = serde_json::from_value(v.clone())?;
        let labels = pod.metadata.labels.unwrap_or_default();
        // The job-pod reaper deletes everything labelled managed-by=kiln that isn't a job.
        assert!(!labels.contains_key("app.kubernetes.io/managed-by"));
        assert_eq!(labels.get("kiln.dev/role").map(String::as_str), Some("image-check"));
        assert_eq!(CHECK_SELECTOR, "kiln.dev/role=image-check");
        let spec = pod.spec.unwrap_or_default();
        assert_eq!(spec.node_name.as_deref(), Some("minikube-m02"));
        assert_eq!(spec.priority_class_name.as_deref(), Some("kiln-job"));
        assert_eq!(spec.automount_service_account_token, Some(false));
        let c = spec.containers.first().ok_or_else(|| anyhow::anyhow!("no container"))?;
        assert_eq!(c.command.as_deref(), Some(&["sha256sum".to_owned(), "/usr/local/bin/kiln".to_owned()][..]));
        assert!(c.resources.as_ref().and_then(|r| r.requests.as_ref()).is_some_and(|r| r.contains_key("cpu") && r.contains_key("memory")), "the job quota needs requests");
        let none = check_pod("n", "i", "Always", None, None);
        assert!(none.pointer("/spec/nodeName").is_none() && none.pointer("/spec/priorityClassName").is_none());
        Ok(())
    }

    #[test]
    fn job_logs_are_bounded() {
        let mut j = new_job("runner", "box", "native", None, None);
        for i in 0..(KEEP_LINES + 10) {
            push_line(&mut j, i.to_string());
        }
        assert_eq!(j.log.len(), KEEP_LINES);
        assert_eq!(j.log.first().map(String::as_str), Some("10"));
    }
}

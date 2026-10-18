//! Upgrading kiln from its own UI (Settings → Updates).
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

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use parking_lot::Mutex;
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::hub::Hub;
use crate::proto::{AgentMsg, CAP_UPGRADE, LANE_CONTROL, ServerMsg};

/// Jobs kept for the UI.
const KEEP_JOBS: usize = 40;
/// Output lines kept per job.
const KEEP_LINES: usize = 400;
/// How long a runner has to finish an upgrade.
const RUNNER_TIMEOUT: Duration = Duration::from_secs(15 * 60);
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
    /// The machine reported running `to_build`.
    pub reached: bool,
    pub log: Vec<String>,
    pub error: Option<String>,
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
    /// Every runner on the newest kiln.
    pub fleet: crate::fleet::Fleet,
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

/// This binary's build id ("unknown" if it can't be read).
pub fn own_build() -> String {
    own_binary().map_or_else(|_| "unknown".to_owned(), |(h, _)| build_id(&h))
}

fn hash_file(p: &Path) -> anyhow::Result<(String, u64)> {
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

/// Binaries uploaded here or fetched from a release live in `<data>/binaries`.
fn store_dir(hub: &Hub) -> PathBuf {
    hub.cfg.server.data_dir.join("binaries")
}

fn stored_path(hub: &Hub, target: &str) -> PathBuf {
    store_dir(hub).join(format!("kiln-{target}"))
}

fn meta_path(hub: &Hub, target: &str) -> PathBuf {
    store_dir(hub).join(format!("kiln-{target}.json"))
}

/// The binary kept here for `target`, if any (and still whole).
pub fn stored(hub: &Hub, target: &str) -> Option<(PathBuf, Binary)> {
    if !known_target(target) {
        return None;
    }
    let p = stored_path(hub, target);
    let meta: Binary = serde_json::from_slice(&std::fs::read(meta_path(hub, target)).ok()?).ok()?;
    let len = std::fs::metadata(&p).ok()?.len();
    (len == meta.size).then_some((p, meta))
}

/// It looks like an executable kiln could hand out (ELF, Mach-O or PE).
fn executable(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x7fELF") || bytes.starts_with(b"MZ") || bytes.starts_with(&[0xcf, 0xfa, 0xed, 0xfe]) || bytes.starts_with(&[0xca, 0xfe, 0xba, 0xbe])
}

/// Keep `bytes` as the binary for `target` (an upload, or a release download).
pub fn store(hub: &Hub, target: &str, bytes: &[u8], origin: &str) -> anyhow::Result<Binary> {
    keep(&store_dir(hub), target, bytes, origin, None)
}

/// Keep the binary `machine` built of `commit` for its own platform (`crate::fleet`): what the
/// other machines of the platform download.
pub fn keep_built(hub: &Hub, target: &str, bytes: &[u8], machine: &str, commit: &str) -> anyhow::Result<Binary> {
    let mut b = keep(&store_dir(hub), target, bytes, "machine", None)?;
    b.sha = Some(commit.to_ascii_lowercase());
    b.by = Some(machine.to_owned());
    std::fs::write(meta_path(hub, target), serde_json::to_vec(&b)?)?;
    Ok(b)
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
pub fn adopt_into(cfg: &crate::config::Config, artifacts: &Path, run: &crate::store::Run) -> anyhow::Result<Vec<Binary>> {
    let dir = cfg.server.data_dir.join("binaries");
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
        let target = target.to_owned();
        kept.push(keep(&dir, &target, &bytes, "built", Some((run.number, run.sha.clone())))?);
    }
    Ok(kept)
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
    let kept: Vec<Binary> = others.iter().filter_map(|t| stored(hub, t).map(|(_, b)| b)).collect();
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
        if commit != was {
            starts.last = None;
            was.clone_from(&commit);
        }
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
    if next(&runs, starts, crate::util::now_unix()) != Next::Build {
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

/// Keeps `bytes` as the binary for `target` in `dir` (`<data>/binaries`).
fn keep(dir: &Path, target: &str, bytes: &[u8], origin: &str, built: Option<(i64, String)>) -> anyhow::Result<Binary> {
    anyhow::ensure!(known_target(target), "kiln has no platform called {target}");
    anyhow::ensure!(bytes.len() > 1 << 20, "that file is too small to be a kiln binary");
    anyhow::ensure!(executable(bytes), "that file is not an executable (ELF, Mach-O or PE)");
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let tmp = dir.join(format!(".kiln-{target}.part"));
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    crate::os::make_executable(&tmp)?;
    std::fs::rename(&tmp, dir.join(format!("kiln-{target}")))?;
    let sha256 = hex::encode(Sha256::digest(bytes));
    let (run, sha) = built.map_or((None, None), |(n, s)| (Some(n), Some(s)));
    let meta = Binary { target: target.to_owned(), build: build_id(&sha256), sha256, size: bytes.len() as u64, origin: origin.to_owned(), at: crate::util::now_unix(), run, sha, by: None };
    std::fs::write(dir.join(format!("kiln-{target}.json")), serde_json::to_vec(&meta)?)?;
    Ok(meta)
}

/// Forget the binary kept for `target`.
pub fn unstore(hub: &Hub, target: &str) -> bool {
    if !known_target(target) {
        return false;
    }
    let had = std::fs::remove_file(meta_path(hub, target)).is_ok();
    let _ = std::fs::remove_file(stored_path(hub, target));
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
        if let Some((_, b)) = stored(hub, t)
            && !v.iter().any(|x| x.target == b.target && x.sha256 == b.sha256)
        {
            v.push(b);
        }
    }
    v
}

/// GET with redirects followed (GitHub release assets redirect to their storage).
async fn get_following(client: &crate::http::client::Client, url: &str, timeout: Duration) -> anyhow::Result<crate::http::client::Response> {
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
    if let Some((_, b)) = stored(hub, target) {
        return Ok((b.sha256, b.size));
    }
    let version = env!("CARGO_PKG_VERSION");
    let Some(bytes) = download_release(version, target).await? else {
        anyhow::bail!(
            "kiln has no binary for {target}: this server is {}, none was uploaded for {target}, and GitHub has no v{version} release with one. Upload a kiln binary for {target} under Settings → Updates.",
            crate::api::TARGET
        );
    };
    let b = store(hub, target, &bytes, "release")?;
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

    /// [`AgentMsg::UpgradeLog`].
    pub fn on_log(&self, hub: &Hub, job: &str, line: String) {
        self.say(hub, job, line);
    }

    /// [`AgentMsg::UpgradeDone`]. A success completes the job once the new build has reported
    /// in (the job's driver waits for that); a failure completes it now.
    pub fn on_done(&self, hub: &Hub, job: &str, ok: bool, error: Option<String>) {
        if ok {
            self.say(hub, job, "the runner finished installing");
        } else {
            self.finish(hub, job, Err(error.unwrap_or_else(|| "the runner reported a failure".to_owned())));
        }
    }

    /// [`AgentMsg::Build`] from a machine: an upgrade of it to that build has arrived.
    pub fn on_build(&self, hub: &Hub, machine: &str, build: &str) {
        let ids: Vec<String> = self
            .jobs
            .lock()
            .iter()
            .filter(|j| j.kind == "runner" && j.target == machine && j.state == JobState::Running && !j.reached && j.to_build.as_deref() == Some(build))
            .map(|j| j.id.clone())
            .collect();
        for id in ids {
            self.update(hub, &id, |j| {
                j.reached = true;
                push_line(j, format!("{machine} is running build {build}"));
            });
        }
    }
}

fn push_line(j: &mut Job, line: String) {
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
        reached: false,
        log: Vec::new(),
        error: None,
    }
}

// ------------------------------------------------------------------ runners

/// A machine's runner as Settings → Updates shows it.
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
    /// Its kiln checkout, as it reports it.
    pub checkout: Option<String>,
    /// "native" or "terminal".
    pub method: &'static str,
    /// Terminals are allowed on it (an older runner is upgraded through one).
    pub terminal: bool,
    pub instances: Vec<crate::instances::InstanceView>,
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
            let target_build = target.as_deref().and_then(|t| if t == crate::api::TARGET { Some(mine.clone()) } else { stored(hub, t).map(|(_, b)| b.build) });
            RunnerInfo {
                up_to_date: build.is_some() && build == target_build,
                method: if newest.is_some_and(|i| i.can_upgrade) { "native" } else { "terminal" },
                terminal: a.terminal,
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
                checkout: None,
            }
        })
        .collect();
    out.sort_by(|a, b| b.connected.cmp(&a.connected).then_with(|| a.name.cmp(&b.name)));
    // Against the newest kiln, when there is one: the commit each runs, and how it gets there.
    let newest = hub.updates.fleet.target();
    for r in &mut out {
        r.commit = crate::fleet::runner_commit(hub, &r.name, r.build.as_deref());
        r.checkout = crate::fleet::reported(hub, &r.name).and_then(|k| k.checkout);
        let Some(t) = &newest else { continue };
        r.target_build = r.target.as_deref().and_then(|target| crate::fleet::binary_for(hub, target, &t.commit)).map(|b| b.build);
        let s = crate::fleet::standing(hub, r, t).await;
        r.up_to_date = s == crate::fleet::Standing::Current;
        r.standing = Some(s);
        if r.connected
            && let Some((how, next)) = crate::fleet::next(hub, r, t, s).await
        {
            r.how = Some(how);
            r.next = Some(next);
        }
    }
    out
}

/// Start upgrading `machine`'s runner to the newest kiln (`crate::fleet`): a binary of it when
/// kiln has one for its platform, else a build of it on the machine itself. With no newest known
/// (a server built without knowing its commit), to the build kiln has for its platform. Nothing it
/// runs is stopped.
pub async fn upgrade_runner(hub: &Arc<Hub>, machine: &str) -> anyhow::Result<Job> {
    if let Some(j) = hub.updates.running_for("runner", machine) {
        return Ok(j);
    }
    let r = runners(hub).await.into_iter().find(|r| r.name == machine).ok_or_else(|| anyhow::anyhow!("there is no runner called {machine}"))?;
    anyhow::ensure!(r.connected, "{machine} is not connected");
    if let Some(t) = hub.updates.fleet.target() {
        anyhow::ensure!(!r.up_to_date, "{machine} already runs the newest kiln ({})", crate::fleet::short(&t.commit));
        return match crate::fleet::plan(hub, &r, &t).await {
            crate::fleet::Plan::Download(b) => upgrade_with(hub, &r, &b, "admin").await,
            crate::fleet::Plan::Build(c) => crate::fleet::self_build(hub, &r, &t.commit, c, "admin").await,
            crate::fleet::Plan::Wait(why) => anyhow::bail!("{machine} can't be upgraded yet: it {why}"),
        };
    }
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

/// The runner does it itself: it fetches the binary from this server, checks it and runs
/// `install --agent --upgrade`, reporting back (UpgradeLog / UpgradeDone). A runner that takes
/// the binary in place restarts its own process (same pid, terminals handed over) and so may
/// never send UpgradeDone: the new build reporting in is what completes the job.
async fn native(hub: &Arc<Hub>, id: &str, machine: &str, b: &Binary) {
    let u = &hub.updates;
    // That exact build: the server may hand out more than one for a platform.
    let msg = ServerMsg::Upgrade { job: id.to_owned(), path: format!("/dl/kiln/{}/{}", b.target, b.build), sha256: b.sha256.clone(), size: b.size };
    if !hub.send_to_agent(machine, LANE_CONTROL, &msg).await {
        u.finish(hub, id, Err(format!("{machine} is not connected")));
        return;
    }
    u.say(hub, id, "asked the runner to upgrade itself");
    let deadline = Instant::now() + RUNNER_TIMEOUT;
    while Instant::now() < deadline {
        crate::rt::sleep(Duration::from_secs(1)).await;
        if !u.running(id) {
            return;
        }
        if u.reached(id) {
            u.finish(hub, id, Ok(String::new()));
            return;
        }
    }
    u.finish(hub, id, Err("the new runner didn't report in within 15 minutes; see the runner's log".to_owned()));
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

/// Terminal output without escape sequences or carriage returns.
pub(crate) fn plain(s: &str) -> String {
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
            '\n' | '\t' => out.push(c),
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
    let mut pending = String::new();
    let mut ok_marked = false;
    let mut feed = |text: &str| {
        pending.push_str(&plain(text));
        while let Some(i) = pending.find('\n') {
            let line: String = pending.drain(..=i).collect();
            let line = line.trim_end();
            if line.is_empty() || line.starts_with(HIDDEN_MARK) {
                continue;
            }
            if line.contains(OK_MARK) {
                ok_marked = true;
                continue;
            }
            u.say(hub, id, line.strip_prefix(FAIL_MARK).map_or(line, str::trim));
        }
    };
    feed(&String::from_utf8_lossy(&att.replay));
    let deadline = Instant::now() + RUNNER_TIMEOUT;
    let result = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let Ok(frame) = crate::rt::timeout(left, att.live.recv()).await else {
            break Err("the upgrade didn't finish in 15 minutes".to_owned());
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
        if e.contains("15 minutes") {
            session.close();
        }
        u.finish(hub, id, Err(e));
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
        if r.connected && !r.up_to_date && !matches!(r.standing, Some(crate::fleet::Standing::Branch | crate::fleet::Standing::Local)) {
            out.push(upgrade_runner(hub, &r.name).await.map_err(|e| format!("{}: {e:#}", r.name)));
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
    let upload = stored(hub, crate::api::TARGET).map(|(_, b)| b).filter(|b| mine.as_ref().is_none_or(|(h, _)| *h != b.sha256));
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
        let (src, _) = stored(hub, crate::api::TARGET).ok_or_else(|| anyhow::anyhow!("the uploaded binary is gone"))?;
        std::fs::read(&src).with_context(|| format!("read {}", src.display()))?
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

async fn upgrade_here(http: &crate::http::client::Client, server_url: &str, order: &Order<'_>, own_dir: &Path, service: &str, up: &crate::exec::Uplink) -> anyhow::Result<()> {
    use std::io::{Read as _, Seek as _, Write as _};
    let say = |line: String| up.msg(AgentMsg::UpgradeLog { job: order.job.to_owned(), line });
    let (path, sha256, size) = (order.path, order.sha256, order.size);
    anyhow::ensure!(path.starts_with("/dl/kiln/") && !path.contains(".."), "refusing to fetch {path}");
    anyhow::ensure!(usize::try_from(size).is_ok_and(|s| s <= MAX_BINARY), "the binary is too large ({size} bytes)");
    let url = format!("{}{path}", server_url.trim_end_matches('/'));
    say(format!("downloading {url}")).await;
    let mut resp = http.get(&url).timeout(Duration::from_secs(600)).send().await?;
    anyhow::ensure!(resp.is_success(), "the server answered {} for {url}", resp.status());
    let (bin, log_path) = upgrade_files(own_dir);
    let mut file = std::fs::File::create(&bin).with_context(|| format!("create {}", bin.display()))?;
    let mut h = Sha256::new();
    let mut got = 0u64;
    while let Some(chunk) = resp.chunk().await? {
        got += chunk.len() as u64;
        if got > size {
            drop(file);
            let _ = std::fs::remove_file(&bin);
            anyhow::bail!("the download is larger than announced ({size} bytes)");
        }
        h.update(&chunk);
        file.write_all(&chunk)?;
    }
    file.sync_all()?;
    drop(file);
    let hash = hex::encode(h.finalize());
    if hash != sha256 || got != size {
        let _ = std::fs::remove_file(&bin);
        anyhow::bail!("the download does not match: sha256 {hash}, {got} bytes; expected {sha256}, {size} bytes");
    }
    crate::os::make_executable(&bin)?;
    say(format!("verified build {} ({got} bytes)", build_id(&hash))).await;
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
        if started.elapsed() > RUNNER_TIMEOUT {
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
/// upgrades in place through that), else its own unit (Linux: from its cgroup), else the default.
pub fn runner_service(state_dir: &Path, instance: Option<&str>) -> String {
    if let Some(id) = instance
        && let Ok(b) = std::fs::read(crate::upgrade::instance_dir(state_dir, id).join("replaces.json"))
        && let Ok(r) = serde_json::from_slice::<crate::upgrade::Replaces>(&b)
    {
        return r.unit;
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

/// What a runner announces after Hello ([`AgentMsg::Build`]): what this build can do, and
/// `CAP_DOCKER` when a container CLI works here (docs/ci.md W13-R01, probed once).
pub fn runner_build() -> AgentMsg {
    let docker = if crate::runner::docker::available() { crate::proto::CAP_DOCKER } else { 0 };
    AgentMsg::Build { build: own_build(), caps: RUNNER_CAPS | docker }
}

/// What a runner says of itself after its build ([`AgentMsg::Facts`]): for now, the kiln it
/// runs and where its kiln checkout is (`crate::fleet`).
pub fn runner_facts() -> AgentMsg {
    let facts = kiln_ci::facts::RunnerFacts {
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        kiln: Some(kiln_ci::facts::KilnFacts {
            commit: crate::api::COMMIT.to_owned(),
            build: own_build(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            target: crate::api::TARGET.to_owned(),
            checkout: kiln_checkout().map(|p| p.to_string_lossy().into_owned()),
        }),
        probed_at: crate::util::now_unix(),
        ..kiln_ci::facts::RunnerFacts::default()
    };
    AgentMsg::Facts { json: serde_json::to_string(&facts).unwrap_or_default() }
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
    | crate::proto::CAP_JOB_TTY;

/// What an outpost (`kiln outpost`) does ([`AgentMsg::Build`]): a runner's terminals, AI sessions
/// and agents (taking a session over, listing folders), none of its jobs, upgrades, front door,
/// repo copies or reach.
pub const OUTPOST_CAPS: u32 =
    crate::proto::CAP_OUTPOST | crate::proto::CAP_TAKEOVER | crate::proto::CAP_DIRS | crate::proto::CAP_TAKEOVER_STOP;

/// An outpost's [`AgentMsg::Build`].
pub fn outpost_build() -> AgentMsg {
    AgentMsg::Build { build: own_build(), caps: OUTPOST_CAPS }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runners_announce_every_capability_they_have() -> anyhow::Result<()> {
        use crate::proto::{CAP_DIRS, CAP_FRONT_DOOR, CAP_TAKEOVER, CAP_TAKEOVER_STOP};
        let AgentMsg::Build { caps, build } = runner_build() else { anyhow::bail!("runner_build is not a Build") };
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
        assert_eq!(next(&[], &Starts { last: Some(now - 60) }, now), Next::Wait);
        assert_eq!(next(&[], &Starts { last: Some(now - 11 * 60) }, now), Next::Build);
        assert_eq!(next(&[run(Failed, Some(now - 7200))], &Starts { last: Some(now - 11 * 60) }, now), Next::Build);
        let failed3 = [run(Failed, Some(now - 9000)), run(Failed, Some(now - 8000)), run(Failed, Some(now - 7200))];
        assert_eq!(next(&failed3, &none, now), Next::Nothing);
    }

    #[test]
    fn the_release_pipelines_binaries_are_kept_for_the_other_platforms() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut cfg = crate::config::Config::default();
        cfg.server.data_dir = dir.path().to_owned();
        let fake = |magic: &[u8]| -> Vec<u8> {
            let mut b = magic.to_vec();
            b.resize(2 << 20, 7);
            b
        };
        let mut tar = tar::Builder::new(Vec::new());
        for (name, bytes) in [
            ("dist/kiln-aarch64-apple-darwin".to_owned(), fake(&[0xcf, 0xfa, 0xed, 0xfe])),
            ("dist/kiln-x86_64-pc-windows-msvc.exe".to_owned(), fake(b"MZ")),
            (format!("dist/kiln-{}", crate::api::TARGET), fake(b"\x7fELF")),
            ("dist/SHA256SUMS".to_owned(), b"sums".to_vec()),
        ] {
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
        let kept = adopt_into(&cfg, &artifacts, &run)?;
        let mut targets: Vec<&str> = kept.iter().map(|b| b.target.as_str()).collect();
        targets.sort_unstable();
        let mut all = vec!["aarch64-apple-darwin", "x86_64-pc-windows-msvc", crate::api::TARGET];
        all.sort_unstable();
        assert_eq!(targets, all);
        assert!(kept.iter().all(|b| b.origin == "built" && b.run == Some(12) && b.sha.as_deref() == Some("0d21482abcdef")));
        assert!(dir.path().join("binaries/kiln-aarch64-apple-darwin").exists());
        // A run of the server's own commit: its platform keeps the server's own build.
        if !crate::api::COMMIT.is_empty() {
            let own = crate::store::Run { sha: crate::api::COMMIT.to_owned(), ..run };
            let kept = adopt_into(&cfg, &artifacts, &own)?;
            let mut targets: Vec<&str> = kept.iter().map(|b| b.target.as_str()).collect();
            targets.sort_unstable();
            assert_eq!(targets, ["aarch64-apple-darwin", "x86_64-pc-windows-msvc"], "this server's own platform keeps its own build");
        }
        Ok(())
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
        assert_eq!(plain("\x1b[2mhello\x1b[0m\r\nworld\x1b]0;title\x07!"), "hello\nworld!");
        assert_eq!(plain("a\x1b]2;t\x1b\\b"), "ab");
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

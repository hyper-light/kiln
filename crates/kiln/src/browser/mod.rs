//! The browser kiln keeps on a machine for previews (docs/previews.md): Chrome for Testing at a
//! pinned version ([`install`]), started headless the first time an agent there needs it and
//! shared by the machine's agents, each page in a context of its own (no cookies or storage
//! shared), spoken to over the DevTools protocol ([`cdp`]). It stops after a while unused and
//! starts again when it's needed. It never uses a browser the person installed, nor their profile.
//!
//! On Unix kiln speaks to it over a pipe only kiln holds: nothing else on the machine can drive it
//! (a DevTools port on loopback is anyone's there), and it ends when kiln does, however kiln ends.

pub mod apps;
pub mod ports;
pub mod record;
pub mod cdp;
pub mod install;
pub mod page;
pub mod preview;
pub mod stream;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use parking_lot::Mutex;

use cdp::Cdp;

/// The browser stops after this long with no page open.
const IDLE: Duration = Duration::from_secs(600);
/// How long it may take to start (the first time on a machine, after its download).
const STARTING: Duration = Duration::from_secs(45);

pub struct Browser {
    pub(crate) cdp: Arc<Cdp>,
    process: Mutex<Option<Process>>,
    profile: PathBuf,
    /// Its profile outlives it (a conversation's streams keep their cookies).
    kept: bool,
    /// Pages open now, and when the last one was (unix ms).
    open: AtomicUsize,
    used: AtomicI64,
}

static SHARED: OnceLock<futures::lock::Mutex<Option<Arc<Browser>>>> = OnceLock::new();

/// Where this machine's browser lives (kiln's state folder on it), set as the runner starts.
static HOME: OnceLock<PathBuf> = OnceLock::new();

pub fn set_home(dir: &Path) {
    let _ = HOME.set(dir.to_path_buf());
}

/// Something asked of the machine's browser (`crate::reach::local::Req::Browser`): JSON with its
/// `op`. `preview` ([`preview::Ask`]) answers [`preview::Seen`].
pub async fn ask(json: &str) -> anyhow::Result<String> {
    let home = HOME.get().ok_or_else(|| anyhow::anyhow!("this kiln process has no preview browser (it isn't a runner)"))?;
    let v: serde_json::Value = serde_json::from_str(json)?;
    match v.get("op").and_then(serde_json::Value::as_str) {
        Some("preview") => Ok(serde_json::to_string(&preview::preview(home, serde_json::from_value(v)?).await?)?),
        Some("act") => Ok(serde_json::to_string(&stream::act(home, serde_json::from_value(v)?).await?)?),
        other => anyhow::bail!("the preview browser doesn't do {other:?}"),
    }
}

/// The machine's browser, started (and installed, once) when it isn't running.
pub async fn shared(dir: &Path) -> anyhow::Result<Arc<Browser>> {
    let lock = SHARED.get_or_init(|| futures::lock::Mutex::new(None));
    let mut g = lock.lock().await;
    if let Some(b) = g.as_ref()
        && b.alive()
    {
        b.used.store(crate::util::now_ms(), Ordering::Relaxed);
        return Ok(b.clone());
    }
    if let Some(old) = g.take() {
        old.stop();
    }
    let b = Arc::new(Browser::start(dir).await?);
    *g = Some(b.clone());
    crate::rt::spawn(stop_when_idle(Arc::downgrade(&b)));
    Ok(b)
}

/// Stop the browser once nothing has used it for [`IDLE`].
async fn stop_when_idle(b: std::sync::Weak<Browser>) {
    loop {
        crate::rt::sleep(Duration::from_secs(60)).await;
        let Some(browser) = b.upgrade() else { return };
        if !browser.alive() {
            return;
        }
        let idle = crate::util::now_ms() - browser.used.load(Ordering::Relaxed);
        if browser.open.load(Ordering::Relaxed) == 0 && idle > i64::try_from(IDLE.as_millis()).unwrap_or(i64::MAX) {
            let lock = SHARED.get_or_init(|| futures::lock::Mutex::new(None));
            let mut g = lock.lock().await;
            if g.as_ref().is_some_and(|s| Arc::ptr_eq(s, &browser)) {
                *g = None;
            }
            browser.stop();
            tracing::info!("the preview browser stopped: unused for a while");
            return;
        }
    }
}

/// How a browser starts: in a profile that outlives it (else a fresh one that goes with it), and
/// with an extension (its folder and id) let do what it asks without anyone's gesture.
#[derive(Default)]
pub struct Launch {
    pub profile: Option<PathBuf>,
    pub extension: Option<PathBuf>,
    pub id: &'static str,
}

impl Browser {
    async fn start(dir: &Path) -> anyhow::Result<Self> {
        Self::start_with(dir, Launch::default()).await
    }

    pub(crate) async fn start_with(dir: &Path, launch: Launch) -> anyhow::Result<Self> {
        let exe = install::ensure(dir).await?;
        let profiles = dir.join("browser").join("profiles");
        clear_leftovers(&profiles);
        let kept = launch.profile.is_some();
        let profile = launch.profile.unwrap_or_else(|| profiles.join(format!("p{}-{}-{}", std::process::id(), crate::util::now_ms(), hex::encode(rand::random::<[u8; 4]>()))));
        std::fs::create_dir_all(&profile)?;
        let mut args: Vec<String> = [
            "--headless=new",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-background-networking",
            "--disable-component-update",
            "--disable-sync",
            "--disable-default-apps",
            "--disable-breakpad",
            "--metrics-recording-only",
            "--password-store=basic",
            "--use-mock-keychain",
            "--hide-scrollbars",
            // Text measured the same on every machine (no hinting by the screen's settings).
            "--font-render-hinting=none",
            "--disable-features=Translate,MediaRouter,OptimizationHints,DialMediaRouteProvider",
        ]
        .iter()
        .map(|a| (*a).to_owned())
        .collect();
        args.push(format!("--user-data-dir={}", profile.display()));
        // Where the system won't let the browser sandbox itself (a container, Ubuntu's AppArmor),
        // the machine says so in its environment; the default is the sandbox.
        if std::env::var("KILN_BROWSER_SANDBOX").is_ok_and(|v| v == "0") {
            args.push("--no-sandbox".to_owned());
        }
        if let Some(ext) = &launch.extension {
            args.push(format!("--load-extension={}", ext.display()));
            args.push(format!("--disable-extensions-except={}", ext.display()));
            args.push(format!("--allowlisted-extension-id={}", launch.id));
            // A streamed page plays its sound when it starts it, as a person's tab would after a tap.
            args.push("--autoplay-policy=no-user-gesture-required".to_owned());
        }
        let (mut process, cdp) = self::launch(&exe, args, &profile).await?;
        // Answering is being ready.
        match crate::rt::timeout(STARTING, cdp.call(None, "Browser.getVersion", serde_json::json!({}))).await {
            Ok(Ok(_)) => {}
            Ok(Err(_)) | Err(_) => {
                process.kill();
                anyhow::bail!("the preview browser didn't start: {}", why_not(&profile));
            }
        }
        tracing::info!(version = install::VERSION, pid = process.id(), "the preview browser started");
        Ok(Self { cdp, process: Mutex::new(Some(process)), profile, kept, open: AtomicUsize::new(0), used: AtomicI64::new(crate::util::now_ms()) })
    }

    pub fn alive(&self) -> bool {
        self.cdp.alive() && self.process.lock().as_mut().is_some_and(Process::alive)
    }

    pub(crate) fn stop(&self) {
        if let Some(mut p) = self.process.lock().take() {
            p.kill();
        }
        if !self.kept {
            let _ = std::fs::remove_dir_all(&self.profile);
        }
    }

    /// A page of its own: its own context, so nothing (cookies, storage, cache) is shared with
    /// another page.
    pub async fn page(self: &Arc<Self>) -> anyhow::Result<page::Page> {
        self.used.store(crate::util::now_ms(), Ordering::Relaxed);
        page::Page::open(self.clone()).await
    }

    /// A tab of the browser's own (its profile's cookies and storage, its extension's to see),
    /// opened at `url`.
    pub async fn tab(self: &Arc<Self>, url: &str) -> anyhow::Result<page::Page> {
        self.used.store(crate::util::now_ms(), Ordering::Relaxed);
        page::Page::open_in(self.clone(), None, url).await
    }

    pub(crate) fn opened(&self) {
        self.open.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn closed(&self) {
        self.open.fetch_sub(1, Ordering::Relaxed);
        self.used.store(crate::util::now_ms(), Ordering::Relaxed);
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The first target `pick` takes (`Target.getTargets`' entries), waiting a few seconds for it.
pub(crate) async fn target(cdp: &Cdp, pick: impl Fn(&serde_json::Value) -> bool) -> anyhow::Result<String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let all = cdp.call(None, "Target.getTargets", serde_json::json!({})).await?;
        if let Some(t) = all.get("targetInfos").and_then(serde_json::Value::as_array).and_then(|ts| ts.iter().find(|t| pick(t)))
            && let Some(id) = t.get("targetId").and_then(serde_json::Value::as_str)
        {
            return Ok(id.to_owned());
        }
        anyhow::ensure!(std::time::Instant::now() < deadline, "the browser has no such target");
        crate::rt::sleep(Duration::from_millis(100)).await;
    }
}

/// `expression`'s value in `session` (a promise waited for).
pub(crate) async fn evaluate(cdp: &Cdp, session: &str, expression: &str) -> anyhow::Result<serde_json::Value> {
    let r = cdp.call(Some(session), "Runtime.evaluate", serde_json::json!({"expression": expression, "awaitPromise": true, "returnByValue": true})).await?;
    if let Some(d) = r.get("exceptionDetails") {
        let text = d.get("exception").and_then(|x| x.get("description")).and_then(serde_json::Value::as_str).or_else(|| d.get("text").and_then(serde_json::Value::as_str)).unwrap_or("it threw");
        anyhow::bail!("{text}");
    }
    Ok(r.get("result").and_then(|x| x.get("value")).cloned().unwrap_or(serde_json::Value::Null))
}

/// The browser's process: on Unix spawned with its pipe on fds 3 and 4 (in a process group of its
/// own, so its renderers go with it), elsewhere a child listening on a loopback port.
enum Process {
    #[cfg(unix)]
    Spawned(nix::unistd::Pid),
    #[cfg(not(unix))]
    Child(std::process::Child),
}

impl Process {
    fn id(&self) -> u32 {
        match self {
            #[cfg(unix)]
            Self::Spawned(pid) => pid.as_raw().unsigned_abs(),
            #[cfg(not(unix))]
            Self::Child(c) => c.id(),
        }
    }

    fn alive(&mut self) -> bool {
        match self {
            #[cfg(unix)]
            Self::Spawned(pid) => matches!(nix::sys::wait::waitpid(*pid, Some(nix::sys::wait::WaitPidFlag::WNOHANG)), Ok(nix::sys::wait::WaitStatus::StillAlive)),
            #[cfg(not(unix))]
            Self::Child(c) => matches!(c.try_wait(), Ok(None)),
        }
    }

    fn kill(&mut self) {
        match self {
            #[cfg(unix)]
            Self::Spawned(pid) => {
                let _ = nix::sys::signal::killpg(*pid, nix::sys::signal::Signal::SIGKILL);
                let _ = nix::sys::wait::waitpid(*pid, None);
            }
            #[cfg(not(unix))]
            Self::Child(c) => {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }
}

/// `fd`, or a copy of it above 4: the low ones it takes on the way are held (in `held`) until the
/// browser has started, so each copy is the next free one up.
#[cfg(unix)]
fn above_four(fd: std::os::fd::OwnedFd, held: &mut Vec<std::os::fd::OwnedFd>) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::AsRawFd as _;
    let mut fd = fd;
    while fd.as_raw_fd() <= 4 {
        let next = fd.try_clone()?;
        held.push(fd);
        fd = next;
    }
    Ok(fd)
}

/// A pipe neither of whose ends outlives an `exec` (made so at once where the system can).
#[cfg(unix)]
fn pipe() -> anyhow::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    #[cfg(target_os = "linux")]
    {
        Ok(nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)?)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let (r, w) = nix::unistd::pipe()?;
        for fd in [&r, &w] {
            nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC))?;
        }
        Ok((r, w))
    }
}

/// Start the browser and connect to it.
#[cfg(unix)]
async fn launch(exe: &Path, mut args: Vec<String>, profile: &Path) -> anyhow::Result<(Process, Arc<Cdp>)> {
    use nix::fcntl::OFlag;
    use nix::spawn::{PosixSpawnAttr, PosixSpawnFileActions, PosixSpawnFlags, posix_spawn};
    use nix::sys::stat::Mode;
    use std::ffi::CString;
    use std::os::fd::AsRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;
    args.insert(0, exe.to_string_lossy().into_owned());
    args.push("--remote-debugging-pipe".to_owned());
    args.push("about:blank".to_owned());
    let c = |b: &[u8]| CString::new(b).map_err(|_| anyhow::anyhow!("a NUL in the browser's command or environment"));
    let argv = args.iter().map(|a| c(a.as_bytes())).collect::<anyhow::Result<Vec<_>>>()?;
    let envp = std::env::vars_os()
        .map(|(k, v)| {
            let mut kv = k.as_bytes().to_vec();
            kv.push(b'=');
            kv.extend_from_slice(v.as_bytes());
            c(&kv)
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    // What it reads (its fd 3) and what it writes (its fd 4); kiln's ends close as it starts. Its
    // ends are above 4 first: one that were 3 or 4 itself would be lost on the way (a copy onto
    // itself keeps it closing at exec on macOS; one copied onto 3 before 3 is read gone).
    let (its_in, to_it) = pipe()?;
    let (from_it, its_out) = pipe()?;
    let mut held = Vec::new();
    let its_in = above_four(its_in, &mut held)?;
    let its_out = above_four(its_out, &mut held)?;
    let log = profile.join("browser.log");
    let mut fa = PosixSpawnFileActions::init()?;
    fa.add_open(0, "/dev/null", OFlag::O_RDONLY, Mode::empty())?;
    fa.add_open(1, "/dev/null", OFlag::O_WRONLY, Mode::empty())?;
    fa.add_open(2, log.as_path(), OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_TRUNC, Mode::from_bits_truncate(0o600))?;
    fa.add_dup2(its_in.as_raw_fd(), 3)?;
    fa.add_dup2(its_out.as_raw_fd(), 4)?;
    let mut attr = PosixSpawnAttr::init()?;
    attr.set_flags(PosixSpawnFlags::POSIX_SPAWN_SETPGROUP)?;
    attr.set_pgroup(nix::unistd::Pid::from_raw(0))?;
    let pid = posix_spawn(exe, &fa, &attr, &argv, &envp).map_err(|e| anyhow::anyhow!("the preview browser didn't start: {e}"))?;
    drop((its_in, its_out, held));
    let mut process = Process::Spawned(pid);
    match Cdp::over_pipe(std::fs::File::from(to_it), std::fs::File::from(from_it)) {
        Ok(cdp) => Ok((process, cdp)),
        Err(e) => {
            process.kill();
            Err(e)
        }
    }
}

/// Start the browser and connect to it: listening on a loopback port, its address in a file of
/// its profile once it does.
#[cfg(not(unix))]
async fn launch(exe: &Path, mut args: Vec<String>, profile: &Path) -> anyhow::Result<(Process, Arc<Cdp>)> {
    use std::process::{Command, Stdio};
    args.push("--remote-debugging-port=0".to_owned());
    args.push("about:blank".to_owned());
    let log = std::fs::File::create(profile.join("browser.log"))?;
    let child = Command::new(exe).args(&args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::from(log)).spawn().map_err(|e| anyhow::anyhow!("the preview browser didn't start: {e}"))?;
    let mut process = Process::Child(child);
    let at = profile.join("DevToolsActivePort");
    let deadline = std::time::Instant::now() + STARTING;
    let (port, path) = loop {
        if let Ok(text) = std::fs::read_to_string(&at) {
            let mut lines = text.lines();
            if let (Some(port), Some(path)) = (lines.next(), lines.next()) {
                break (port.trim().to_owned(), path.trim().to_owned());
            }
        }
        if !process.alive() || std::time::Instant::now() > deadline {
            process.kill();
            anyhow::bail!("the preview browser didn't start: {}", why_not(profile));
        }
        crate::rt::sleep(Duration::from_millis(100)).await;
    };
    match Cdp::connect(&format!("ws://127.0.0.1:{port}{path}")).await {
        Ok(cdp) => Ok((process, cdp)),
        Err(e) => {
            process.kill();
            Err(e)
        }
    }
}

/// Why the browser didn't start, from what it printed: the libraries it's missing, the sandbox it
/// couldn't make, or its last words.
fn why_not(profile: &Path) -> String {
    let log = std::fs::read_to_string(profile.join("browser.log")).unwrap_or_default();
    if let Some(lib) = log.lines().find(|l| l.contains("error while loading shared libraries")) {
        return format!("this machine lacks a library it needs ({}); install the browser's dependencies (on Debian/Ubuntu: the libnss3, libatk-bridge2.0-0, libgbm1, libxkbcommon0 and libasound2 packages)", lib.trim());
    }
    if log.contains("No usable sandbox") {
        return "this machine won't let it sandbox itself; allow unprivileged user namespaces, or set KILN_BROWSER_SANDBOX=0 in the runner's environment".to_owned();
    }
    log.lines().rev().find(|l| !l.trim().is_empty()).map_or_else(|| "it said nothing".to_owned(), |l| l.trim().chars().take(300).collect())
}

/// Profiles kiln processes that are gone left (one stopped without stopping its browser, before
/// the pipe that ends a browser with kiln): the browser still using one is stopped (only one whose
/// command line names that profile), then it goes. A profile of this process (another of its
/// browsers) or of another living kiln process (a runner's next build, starting beside it) is in
/// use, and stays.
fn clear_leftovers(profiles: &Path) {
    let Ok(entries) = std::fs::read_dir(profiles) else { return };
    for e in entries.flatten() {
        let dir = e.path();
        // `p<pid>-<ms>`: whose it is.
        let owner = e.file_name().to_string_lossy().strip_prefix('p').and_then(|r| r.split('-').next()).and_then(|p| p.parse::<u32>().ok());
        if owner.is_some_and(|pid| pid == std::process::id() || alive(pid)) {
            continue;
        }
        #[cfg(unix)]
        if let Ok(target) = std::fs::read_link(dir.join("SingletonLock"))
            && let Some(pid) = target.to_string_lossy().rsplit('-').next().and_then(|p| p.parse::<i32>().ok())
            && std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| String::from_utf8_lossy(&c).contains(&*dir.to_string_lossy()))
        {
            let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGKILL);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Whether process `pid` is running (one that can't be asked counts as running).
fn alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        i32::try_from(pid).is_ok_and(|p| !matches!(nix::sys::signal::kill(nix::unistd::Pid::from_raw(p), None), Err(nix::errno::Errno::ESRCH)))
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_profiles_of_gone_processes_are_cleared() -> anyhow::Result<()> {
        let profiles = std::env::temp_dir().join(format!("kiln-profiles-test-{}", std::process::id()));
        let mine = profiles.join(format!("p{}-1-aa", std::process::id()));
        // No process has this pid (beyond what Linux and macOS give out).
        let gone = profiles.join("p4194399-1-bb");
        for d in [&mine, &gone] {
            std::fs::create_dir_all(d)?;
        }
        clear_leftovers(&profiles);
        assert!(mine.exists(), "this process's other browser keeps its profile");
        assert!(!gone.exists(), "a gone process's profile goes");
        std::fs::remove_dir_all(&profiles)?;
        Ok(())
    }
}

//! U08: the shared Git transport and read mirrors for CI, actions and deployments.
//!
//! A source is read from a bare mirror ([`Mirror`]): kiln resolves its revision to a commit,
//! asks for the hashes of the paths its render reads (one `cat-file --batch-check` for any
//! number of them), and extracts exactly those paths when it renders. A path's hash is the
//! address of what is under it, so two commits that leave an app's paths alone give its render
//! the same key and the app is not rendered again.
//!
//! Blocking callers use [`Mirror::open`] and [`Mirror::fetch`]. Custody-aware async opening
//! uses [`Mirror::open_owned_using`]: its executor receives the same fences before submission,
//! and the returned operation keeps them for fetch/selection. Command-only adapters are legacy
//! and cannot retain those fences through canceled physical process cleanup.

use std::collections::HashMap;
use std::future::Future;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, OnceLock, Weak};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context as _, Result, bail};
use futures::lock::{Mutex, OwnedMutexGuard};

/// How to authenticate to a remote.
#[derive(Clone, Default)]
pub struct Auth {
    /// An `Authorization` header value (`Basic …`, `Bearer …`) for HTTPS remotes.
    pub header: Option<String>,
    /// A private key file for SSH remotes.
    pub ssh_key: Option<PathBuf>,
    /// Skip TLS verification (Argo CD's `insecure`).
    pub insecure: bool,
    /// Additional URL-scoped HTTP headers: the URL prefix and the whole header.
    pub headers: Vec<(String, String)>,
    /// Use only kiln's explicit credentials, without system or global Git configuration.
    pub isolated: bool,
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Auth")
            .field("header", &self.header.as_ref().map(|_| "<redacted>"))
            .field("ssh_key", &self.ssh_key)
            .field("insecure", &self.insecure)
            .field("headers", &self.headers.len())
            .field("isolated", &self.isolated)
            .finish()
    }
}

/// Read-mirror policy. Hosted repositories and release repositories with authored refs are
/// authoritative writers, not one of these roles.
#[derive(Clone, Copy, Debug, Default)]
pub enum ReadRole {
    #[default]
    Source,
    /// Actions keep commits eagerly and load trees only when their files are read.
    Action,
}

/// The ref namespaces this reader tracks; fetching never prunes a namespace it doesn't own.
#[derive(Clone, Copy, Debug, Default)]
pub enum Refs {
    #[default]
    HeadsTags,
    HeadsTagsPulls,
}

impl Refs {
    fn specs(self) -> &'static [&'static str] {
        match self {
            Self::HeadsTags => &["+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"],
            Self::HeadsTagsPulls => &["+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*", "+refs/pull/*/head:refs/pull/*/head"],
        }
    }
}

/// How a read mirror is populated, apart from the credentials used for each operation.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    pub role: ReadRole,
    pub refs: Refs,
}

/// A bare mirror of a remote.
#[derive(Clone, Debug)]
pub struct Mirror {
    /// The mirror's directory.
    pub dir: PathBuf,
    /// The remote's URL.
    pub url: String,
    auth: Auth,
    options: Options,
    gate: Arc<Mutex<()>>,
    /// Public directory/URL fields remain compatibility views, never authority to retarget
    /// captured credentials or a directory's already-assigned gate.
    identity: Arc<(PathBuf, String)>,
}

/// The meaning of a short revision at a particular call site. Source repositories choose
/// branches first; GitHub actions choose tags first. Fully qualified refs keep their meaning.
#[derive(Clone, Copy, Debug)]
pub enum RefOrder {
    HeadsFirst,
    TagsFirst,
}

/// U08: one admitted mutation/selection owns both existing mirror fences. The mirror and its
/// credentials are captured here; changing a caller's `Mirror` afterwards cannot change them.
/// Select and retain every root needed by an operation before releasing this capability.
/// Pinned reads and external renderers then run without holding the mutation fence.
pub struct OwnedOperation {
    mirror: Mirror,
    custody: OperationCustody,
    fenced: bool,
}

/// An opaque holding of the SAME fences, for transfer to the existing physical process owner
/// before it can spawn. Cloning this does not admit a new operation or acquire another lock.
/// Its last copy must not be dropped on cancellation until exact subtree retirement is proved.
/// Executors taking only `Command` do not implement that transfer: see `Mirror::operation`.
#[derive(Clone)]
pub struct OperationCustody {
    _held: Arc<OperationHeld>,
}

struct OperationHeld {
    _file: std::fs::File,
    _local: OwnedMutexGuard<()>,
}

/// U08: an immutable selected ref and reachable full commit, bound to the original mirror,
/// credentials and ref policy. The existing permanent pin keeps reads safe after another
/// fetch moves/prunes its source ref, including after this handle is dropped. Owner-based pin
/// expiry is a separate migration; dropping a reader is never permission to prune its root.
#[derive(Clone, Debug)]
pub struct SelectedCommit {
    mirror: Mirror,
    selected_ref: String,
    sha: String,
}

impl SelectedCommit {
    pub fn sha(&self) -> &str { &self.sha }
    /// The selected full ref, `HEAD`, revision expression, or full commit hash.
    pub fn selected_ref(&self) -> &str { &self.selected_ref }
    pub fn dir(&self) -> &Path { &self.mirror.dir }
    pub fn commit(&self) -> Result<Commit> { self.mirror.commit(&self.sha) }
    pub fn read(&self, path: &str) -> Result<Option<Vec<u8>>> { self.mirror.read(&self.sha, path) }
    pub fn list(&self, dir: &str, recurse: bool) -> Result<Vec<Entry>> { self.mirror.list(&self.sha, dir, recurse) }
    pub fn hashes(&self, paths: &[&str]) -> Result<Vec<Option<String>>> { self.mirror.hashes(&self.sha, paths) }
    pub fn with_links(&self, paths: &[String]) -> Result<Vec<String>> { self.mirror.with_links(&self.sha, paths) }
    pub fn extract(&self, paths: &[&str], dest: &Path) -> Result<()> { self.mirror.extract(&self.sha, paths, dest) }
}

/// Every read-mirror writer in this process shares one gate for a directory.
fn gate(dir: &Path) -> Arc<Mutex<()>> {
    static GATES: OnceLock<parking_lot::Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();
    let mut gates = GATES.get_or_init(Default::default).lock();
    if let Some(existing) = gates.get(dir).and_then(Weak::upgrade) {
        return existing;
    }
    gates.retain(|_, g| g.strong_count() > 0);
    let gate = Arc::new(Mutex::new(()));
    gates.insert(dir.to_path_buf(), Arc::downgrade(&gate));
    gate
}

/// U08: a stable sibling inode fences publication and every read-mirror mutation across
/// processes. It is outside the clone destination and is never unlinked or replaced.
fn lock_path(dir: &Path) -> Result<PathBuf> {
    let mut name = dir.file_name().context("a read mirror has no directory name")?.to_os_string();
    name.push(".kiln-read.lock");
    Ok(dir.with_file_name(name))
}

/// The host configures this bounded share in its existing process-wide blocking pool.
pub const LOCK_SHARE: &str = "git-mirror-lock";

async fn physical_lock(dir: &Path) -> Result<std::fs::File> {
    let path = lock_path(dir)?;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) { std::fs::create_dir_all(parent)?; }
    let file = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(&path)
        .with_context(|| format!("opening read-mirror fence {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => return Ok(file),
        Err(std::fs::TryLockError::Error(error)) => return Err(error).context("locking read mirror"),
        Err(std::fs::TryLockError::WouldBlock) => {}
    }
    // U08: OS waits use a bounded share of the canonical pool, never its fs share:
    // the mirror's owner may need fs capacity to launch or finish Git. Bridge results with
    // futures' runtime-independent channel; hyper-rt leaf receivers need a shard waker.
    let (send, receive) = futures::channel::oneshot::channel();
    let job = Arc::new(parking_lot::Mutex::new(Some((file, send))));
    let pending = job.clone();
    let submitted = hyper_rt::blocking::run(LOCK_SHARE, move || {
        if let Some((file, send)) = pending.lock().take() {
            if send.is_canceled() { return; }
            let result = file.lock().map(|()| file);
            // Cancellation while the OS waits cannot cancel that syscall; the pool keeps
            // its slot charged until it returns, then a discarded result releases the file.
            let _ = send.send(result);
        }
    });
    match submitted {
        Ok(_completion) => receive.await.context("read-mirror fence waiter stopped")?.context("locking read mirror"),
        Err(hyper_rt::error::RtError::BadConfig { what: "a blocking job with no pool running" })
            if hyper_rt::futures::shard_id().is_none() => {
                // Standalone synchronous Mirror::open has no runtime to start. Its existing
                // caller thread may wait; never start another pool or block a shard here.
                let (file, _) = job.lock().take().context("read-mirror fence job disappeared")?;
                file.lock().context("locking standalone read mirror")?;
                Ok(file)
            }
        Err(error) => Err(error).context("admitting read-mirror fence wait"),
    }
}

/// One entry of a tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Path relative to the repository root.
    pub path: String,
    /// Whether it is a directory.
    pub tree: bool,
    /// Whether it is a symbolic link (its blob is the link's target).
    pub link: bool,
    /// Its object hash.
    pub hash: String,
}

/// A commit as people read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    /// Its hash.
    pub sha: String,
    /// Its author's name.
    pub author: String,
    /// Its author's email.
    pub email: String,
    /// When it was authored, seconds since the epoch.
    pub at: i64,
    /// Its message's first line.
    pub subject: String,
    /// The rest of its message.
    pub body: String,
}

/// A completed Git command's unsuccessful exit, distinct from executor/timeout/retirement
/// errors. Revision selection may try its next candidate ONLY for rev-parse's known missing
/// revision exit (1); an unknown physical outcome must never launch another phase.
#[derive(Debug)]
pub struct GitExit {
    status: ExitStatus,
    diagnostic: String,
}

impl GitExit {
    pub fn new(status: ExitStatus, diagnostic: String) -> Self { Self { status, diagnostic } }
    pub fn code(&self) -> Option<i32> { self.status.code() }
}

impl std::fmt::Display for GitExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "git failed: {}", self.diagnostic.trim()) }
}

impl std::error::Error for GitExit {}

/// U08: exact full-commit verification completed with rev-parse's known missing exit (1).
/// This context is never attached to pin/update-ref or an unknown executor outcome.
#[derive(Debug)]
pub struct MissingCommit { _private: () }

impl std::fmt::Display for MissingCommit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("requested Git commit is absent from the read mirror") }
}

impl std::error::Error for MissingCommit {}

/// A noninteractive Git command. Credentials live in its environment, never its arguments.
/// A single header belongs only to `url`; `headers` each carry their own URL scope.
pub fn command(dir: Option<&Path>, url: Option<&str>, auth: &Auth) -> Command {
    let mut c = Command::new("git");
    c.env("GIT_TERMINAL_PROMPT", "0").env("GCM_INTERACTIVE", "never").env("LC_ALL", "C")
        .env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new");
    if cfg!(unix) {
        c.env("GIT_ASKPASS", "/bin/true").env("SSH_ASKPASS", "/bin/true");
    }
    if auth.isolated {
        c.env("GIT_CONFIG_GLOBAL", if cfg!(windows) { "NUL" } else { "/dev/null" }).env("GIT_CONFIG_NOSYSTEM", "1");
    }
    if let Some(d) = dir {
        c.arg("--git-dir").arg(d);
    }
    let mut headers = auth.headers.clone();
    if let Some(h) = &auth.header {
        headers.push((url.unwrap_or_default().to_owned(), format!("Authorization: {h}")));
    }
    c.env("GIT_CONFIG_COUNT", headers.len().to_string());
    if !headers.is_empty() {
        for (i, (scope, header)) in headers.into_iter().enumerate() {
            let key = if scope.is_empty() { "http.extraheader".to_owned() } else { format!("http.{scope}.extraheader") };
            c.env(format!("GIT_CONFIG_KEY_{i}"), key).env(format!("GIT_CONFIG_VALUE_{i}"), header);
        }
    }
    if auth.insecure {
        c.arg("-c").arg("http.sslVerify=false");
    }
    if let Some(k) = &auth.ssh_key {
        let key = k.to_string_lossy().replace('\'', "'\\''");
        c.env("GIT_SSH_COMMAND", format!("ssh -i '{key}' -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new"));
    }
    c
}

fn git(dir: Option<&Path>, url: &str, auth: &Auth) -> Command {
    let mut c = command(dir, Some(url), auth);
    // U08: request foreground automatic maintenance so a successful mirror mutation also
    // finishes its maintenance before releasing the existing fence. Task selection and
    // thresholds stay with Git. Public command() also serves authoritative repositories;
    // this policy belongs only to the shared read-mirror engine.
    c.args(["-c", "maintenance.autoDetach=false", "-c", "gc.autoDetach=false"]);
    c
}

fn run(mut c: Command) -> Result<Vec<u8>> {
    let out = c.stdin(Stdio::null()).output().context("running git")?;
    if !out.status.success() {
        return Err(GitExit::new(out.status, String::from_utf8_lossy(&out.stderr).into_owned()).into());
    }
    Ok(out.stdout)
}

fn text(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).trim().to_owned()
}

/// The object path git takes for `path` at `commit` (`<commit>^{tree}` for the root).
fn spec(commit: &str, path: &str) -> String {
    let p = path.trim_matches('/');
    if p.is_empty() || p == "." { format!("{commit}^{{tree}}") } else { format!("{commit}:{p}") }
}

impl Mirror {
    /// The mirror of `url` in `dir`, cloned if it isn't there yet.
    pub fn open(dir: &Path, url: &str, auth: Auth) -> Result<Mirror> {
        futures::executor::block_on(Self::open_using(dir, url, auth, Options::default(), |c| std::future::ready(run(c))))
    }

    /// Legacy Command-only executor. It cannot transfer custody to a physical process owner;
    /// use `open_owned_using` for cancellation-safe opening with an admitted native executor.
    pub async fn open_using<F, Fut>(dir: &Path, url: &str, auth: Auth, options: Options, mut execute: F) -> Result<Mirror>
    where
        F: FnMut(Command) -> Fut,
        Fut: Future<Output = Result<Vec<u8>>>,
    {
        Self::open_import_using(dir, url, auth, options, &[], &mut execute).await
    }

    /// Opens under the existing fences and keeps that SAME operation for later phases.
    /// Before any submission that can outlive its waiter, the executor must retain the supplied
    /// custody in its canonical physical owner. It may return success or `GitExit` only after
    /// exact retirement; every other error is an unknown outcome and stops opening.
    /// The host must reconcile its durable admission before calling this and revalidate after
    /// acquiring custody, before each launch. This API supplies no resource/provider admission.
    pub async fn open_owned_using<F, Fut>(dir: &Path, url: &str, auth: Auth, options: Options, mut execute: F) -> Result<OwnedOperation>
    where
        F: FnMut(Command, OperationCustody) -> Fut,
        Fut: Future<Output = Result<Vec<u8>>>,
    {
        Self::open_import_owned_using(dir, url, auth, options, &[], &mut execute).await
    }

    /// U08: opens the canonical source and imports old read mirrors once. The old directories
    /// remain recoverable until maintenance removes them; only their immutable objects are
    /// linked. Cached commits absent from the remote remain available after the cutover.
    pub fn open_import(dir: &Path, url: &str, auth: Auth, legacy: &[PathBuf]) -> Result<Mirror> {
        futures::executor::block_on(Self::open_import_using(dir, url, auth, Options::default(), legacy, |c| std::future::ready(run(c))))
    }

    /// Legacy Command-only import adapter, with the same cancellation limitation as
    /// `open_using`. Only the custody-aware entry point can hand its fences to the executor.
    pub async fn open_import_using<F, Fut>(dir: &Path, url: &str, auth: Auth, options: Options, legacy: &[PathBuf], mut execute: F) -> Result<Mirror>
    where
        F: FnMut(Command) -> Fut,
        Fut: Future<Output = Result<Vec<u8>>>,
    {
        Self::open_import_owned_using(dir, url, auth, options, legacy, |command, _custody| execute(command)).await?.into_mirror()
    }

    /// Import/open with one captured identity, policy and fence, without a hidden fetch.
    /// Every Git command receives custody of the SAME File before executor invocation. An
    /// unknown probe never falls through to clone/import, and an unknown clone leaves its
    /// temporary path for the canonical owner's recovery instead of racing physical cleanup.
    /// The executor and host have the same admission/retirement duties as `open_owned_using`.
    pub async fn open_import_owned_using<F, Fut>(dir: &Path, url: &str, auth: Auth, options: Options, legacy: &[PathBuf], mut execute: F) -> Result<OwnedOperation>
    where
        F: FnMut(Command, OperationCustody) -> Fut,
        Fut: Future<Output = Result<Vec<u8>>>,
    {
        static CLONES: AtomicU64 = AtomicU64::new(0);
        let gate = gate(dir);
        let mirror = Mirror { dir: dir.to_owned(), url: url.to_owned(), auth, options, gate, identity: Arc::new((dir.to_owned(), url.to_owned())) };
        let mut operation = mirror.operation().await?;
        let custody = operation.custody();
        let mut execute = |command| execute(command, custody.clone());
        let auth = &mirror.auth;
        if !dir.join("HEAD").exists() {
            if let Some(parent) = dir.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let temp = dir.with_extension(format!("clone-{}-{}", std::process::id(), CLONES.fetch_add(1, Ordering::Relaxed)));
            let mut imported = false;
            for old in legacy {
                if old == dir || !old.join("HEAD").is_file() { continue; }
                let mut c = git(Some(old), url, auth);
                c.args(["remote", "get-url", "origin"]);
                let origin = match operation.execute_phase(c, &mut execute).await {
                    Ok(output) => text(output),
                    Err(error) if error.downcast_ref::<GitExit>().is_some() => continue,
                    Err(error) => return Err(error).context("checking a legacy read-mirror source"),
                };
                if origin == url {
                    if let Err(error) = copy_mirror(old, &temp, false) {
                        let _ = std::fs::remove_dir_all(&temp);
                        return Err(error).context("importing a legacy read mirror");
                    }
                    imported = true;
                    break;
                }
            }
            let cloned = if imported { Ok(Vec::new()) } else {
                let mut c = git(None, url, auth);
                c.args(["clone", "--bare", "--quiet"]);
                if matches!(options.role, ReadRole::Action) { c.arg("--filter=tree:0"); }
                c.arg(url).arg(&temp);
                operation.execute_phase(c, &mut execute).await
            };
            if let Err(error) = cloned.with_context(|| format!("cloning {url}")) {
                if !operation.fenced { let _ = std::fs::remove_dir_all(&temp); }
                return Err(error);
            }
            if let Err(error) = std::fs::rename(&temp, dir) {
                let _ = std::fs::remove_dir_all(&temp);
                return Err(error).with_context(|| format!("publishing mirror {}", dir.display()));
            }
        }
        let mut c = git(Some(dir), url, auth);
        c.args(["remote", "set-url", "origin", url]);
        operation.execute_phase(c, &mut execute).await.with_context(|| format!("setting the source of {}", dir.display()))?;
        for old in legacy {
            if old == dir || !old.join("HEAD").is_file() { continue; }
            let stamp = dir.join("kiln-imports").join(format!("{:016x}", xxhash_rust::xxh3::xxh3_64(old.to_string_lossy().as_bytes())));
            if stamp.is_file() { continue; }
            let mut c = git(Some(old), url, auth);
            c.args(["remote", "get-url", "origin"]);
            let origin = match operation.execute_phase(c, &mut execute).await {
                Ok(output) => text(output),
                Err(error) if error.downcast_ref::<GitExit>().is_some() => continue,
                Err(error) => return Err(error).context("checking a legacy read-mirror source"),
            };
            if origin != url { continue; }
            copy_mirror(&old.join("objects"), &dir.join("objects"), true)?;
            std::fs::create_dir_all(stamp.parent().unwrap_or(dir))?;
            std::fs::write(stamp, b"1\n")?;
        }
        Ok(operation)
    }

    /// U08: acquire the existing local and cross-process fences as an owned capability.
    /// Physical waits still use `LOCK_SHARE`; no new worker, registry or admission queue is
    /// introduced. Existing caller budgets/physical pool limits apply. The legacy Command-only
    /// executor cannot keep custody through canceled process cleanup: a physical process owner
    /// must retain `OwnedOperation::custody` BEFORE spawn to close that separate lifecycle gap.
    /// The host must also revalidate its existing durable orphan fence after this File has
    /// been acquired and before spawning: an owner can crash while this acquisition waits.
    pub async fn operation(&self) -> Result<OwnedOperation> {
        anyhow::ensure!(self.dir == self.identity.0 && self.url == self.identity.1, "a read mirror's source identity cannot be retargeted");
        let local = self.gate.clone().lock_owned().await;
        let file = physical_lock(&self.dir).await?;
        Ok(OwnedOperation { mirror: self.clone(), custody: OperationCustody { _held: Arc::new(OperationHeld { _file: file, _local: local }) }, fenced: false })
    }

    /// Select and retain a commit without fetching. Multiple refs that must see one snapshot
    /// are selected through one `OwnedOperation` instead of reacquiring between selections.
    pub fn select(&self, revision: &str, order: RefOrder) -> Result<SelectedCommit> {
        futures::executor::block_on(self.select_using(revision, order, |c| std::future::ready(run(c))))
    }

    pub async fn select_using<F, Fut>(&self, revision: &str, order: RefOrder, execute: F) -> Result<SelectedCommit>
    where
        F: FnMut(Command) -> Fut,
        Fut: Future<Output = Result<Vec<u8>>>,
    {
        self.operation().await?.select_using(revision, order, execute).await
    }

    /// Fetches everything the remote has.
    pub fn fetch(&self) -> Result<()> {
        futures::executor::block_on(self.fetch_using(|c| std::future::ready(run(c))))
    }

    /// Refreshes only this reader's ref namespaces using the caller's process executor.
    pub async fn fetch_using<F, Fut>(&self, execute: F) -> Result<()>
    where
        F: FnMut(Command) -> Fut,
        Fut: Future<Output = Result<Vec<u8>>>,
    {
        self.operation().await?.fetch_using(execute).await
    }

    /// Compatibility lookup: the commit a revision names, without a retained lifetime.
    /// Use `select` or one owned operation's `select_using` for subsequent reads; a raw SHA
    /// returned here alone does not keep the object reachable across another writer's prune.
    pub fn resolve(&self, revision: &str) -> Result<String> {
        let rev = if revision.is_empty() { "HEAD" } else { revision };
        for candidate in [format!("refs/heads/{rev}"), format!("refs/tags/{rev}"), rev.to_owned()] {
            let mut c = git(Some(&self.dir), &self.url, &self.auth);
            c.args(["rev-parse", "--verify", "--quiet", &format!("{candidate}^{{commit}}")]);
            if let Ok(out) = run(c) {
                let sha = text(out);
                if !sha.is_empty() {
                    return Ok(sha);
                }
            }
        }
        bail!("{} has no revision {rev:?}", self.url)
    }

    /// Who wrote a commit, when, and what it says.
    pub fn commit(&self, commit: &str) -> Result<Commit> {
        let mut c = git(Some(&self.dir), &self.url, &self.auth);
        c.args(["show", "--no-patch", "--format=%H%x00%an%x00%ae%x00%at%x00%s%x00%b", &format!("{commit}^{{commit}}"), "--"]);
        let out = String::from_utf8_lossy(&run(c)?).into_owned();
        let mut f = out.splitn(6, '\0');
        let mut next = || f.next().unwrap_or("").to_owned();
        let (sha, author, email, at, subject, body) = (next(), next(), next(), next(), next(), next());
        if sha.trim().is_empty() {
            bail!("{} has no commit {commit}", self.url);
        }
        Ok(Commit { sha: sha.trim().to_owned(), author, email, at: at.trim().parse().unwrap_or(0), subject, body: body.trim().to_owned() })
    }

    /// The hash of each path at `commit` (`None` where nothing is there), in one call.
    pub fn hashes(&self, commit: &str, paths: &[&str]) -> Result<Vec<Option<String>>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let mut c = git(Some(&self.dir), &self.url, &self.auth);
        c.args(["cat-file", "--batch-check=%(objectname) %(objecttype)"]);
        let mut child = c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().context("running git")?;
        let mut input = String::new();
        for p in paths {
            input.push_str(&spec(commit, p));
            input.push('\n');
        }
        // Written while its answers are read, never all before: git answers each line as it reads
        // it and stops reading once the pipe its answers go to is full (64 KiB, or 8 KiB once its
        // user's pipes pass `pipe-user-pages-soft`).
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let mut out = String::new();
        let (wrote, read) = std::thread::scope(|s| {
            let writer = s.spawn(move || stdin.map_or(Ok(()), |mut stdin| stdin.write_all(input.as_bytes())));
            let read = stdout.map_or(Ok(0), |mut stdout| stdout.read_to_string(&mut out));
            if read.is_err() {
                // The writer may be blocked on a git that no one reads: it ends with git.
                let _ = child.kill();
            }
            (writer.join(), read)
        });
        child.wait()?;
        read?;
        wrote.map_err(|_| anyhow::anyhow!("writing to git failed"))??;
        Ok(out
            .lines()
            .map(|l| {
                let mut it = l.split_whitespace();
                match (it.next(), it.next()) {
                    (Some(hash), Some("tree" | "blob")) => Some(hash.to_owned()),
                    _ => None,
                }
            })
            .collect())
    }

    /// A file's contents at `commit`.
    pub fn read(&self, commit: &str, path: &str) -> Result<Option<Vec<u8>>> {
        let mut c = git(Some(&self.dir), &self.url, &self.auth);
        c.args(["cat-file", "blob", &spec(commit, path)]);
        let out = c.stdin(Stdio::null()).output().context("running git")?;
        Ok(out.status.success().then_some(out.stdout))
    }

    /// The entries under `dir` at `commit` (recursively when `recurse`).
    pub fn list(&self, commit: &str, dir: &str, recurse: bool) -> Result<Vec<Entry>> {
        let mut c = git(Some(&self.dir), &self.url, &self.auth);
        c.args(["ls-tree", "-z", "--full-tree"]);
        if recurse {
            c.arg("-r");
        }
        c.arg(commit);
        let d = dir.trim_matches('/');
        if !d.is_empty() && d != "." {
            c.arg("--").arg(format!("{d}/"));
        }
        let out = run(c)?;
        Ok(out
            .split(|b| *b == 0)
            .filter(|r| !r.is_empty())
            .filter_map(|record| {
                let line = String::from_utf8_lossy(record);
                let (meta, path) = line.split_once('\t')?;
                let mut it = meta.split_whitespace();
                let (mode, kind, hash) = (it.next()?, it.next()?, it.next()?);
                Some(Entry { path: path.to_owned(), tree: kind == "tree", link: mode == "120000", hash: hash.to_owned() })
            })
            .collect())
    }

    /// `paths` plus the targets of the symbolic links under them, followed until nothing new
    /// is reached: a render that reads through a link reads its target, so the target is part of
    /// what the render's key covers. A link out of the repository is refused, as Argo CD refuses
    /// it.
    pub fn with_links(&self, commit: &str, paths: &[String]) -> Result<Vec<String>> {
        let mut all: Vec<String> = paths.to_vec();
        let mut todo: Vec<String> = paths.to_vec();
        let mut rounds = 0;
        while let Some(p) = todo.pop() {
            rounds += 1;
            if rounds > 4096 {
                bail!("too many symbolic links under {}", paths.join(", "));
            }
            let entries = self.list_path(commit, &p)?;
            for e in entries.into_iter().filter(|e| e.link) {
                let target = self.read(commit, &e.path)?.map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
                let dir = e.path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                let Some(resolved) = crate::render::join(dir, target.trim()) else {
                    bail!("{} links outside the repository ({target})", e.path);
                };
                if !all.iter().any(|a| covers(a, &resolved)) {
                    all.push(resolved.clone());
                    todo.push(resolved);
                }
            }
        }
        all.sort();
        all.dedup();
        Ok(all)
    }

    /// The entries at or under `path`: the path itself if it is a file or link, else everything
    /// beneath it.
    fn list_path(&self, commit: &str, path: &str) -> Result<Vec<Entry>> {
        let p = path.trim_matches('/');
        let mut c = git(Some(&self.dir), &self.url, &self.auth);
        c.args(["ls-tree", "-r", "-z", "--full-tree", commit]);
        if !p.is_empty() && p != "." {
            c.arg("--").arg(p);
        }
        let out = run(c)?;
        Ok(out
            .split(|b| *b == 0)
            .filter(|r| !r.is_empty())
            .filter_map(|record| {
                let line = String::from_utf8_lossy(record);
                let (meta, path) = line.split_once('\t')?;
                let mut it = meta.split_whitespace();
                let (mode, kind, hash) = (it.next()?, it.next()?, it.next()?);
                Some(Entry { path: path.to_owned(), tree: kind == "tree", link: mode == "120000", hash: hash.to_owned() })
            })
            .collect())
    }

    /// The paths that differ between two commits.
    pub fn changed(&self, from: &str, to: &str) -> Result<Vec<String>> {
        let mut c = git(Some(&self.dir), &self.url, &self.auth);
        c.args(["diff-tree", "-r", "-z", "--name-only", "--no-commit-id", from, to]);
        let out = run(c)?;
        Ok(out.split(|b| *b == 0).filter(|r| !r.is_empty()).map(|r| String::from_utf8_lossy(r).into_owned()).collect())
    }

    /// Writes the files under `paths` at `commit` into `dest`, keeping their repository layout.
    /// Paths that don't exist are skipped.
    pub fn extract(&self, commit: &str, paths: &[&str], dest: &Path) -> Result<()> {
        let hashes = self.hashes(commit, paths)?;
        let present: Vec<&str> = paths.iter().zip(&hashes).filter(|(_, h)| h.is_some()).map(|(p, _)| *p).collect();
        std::fs::create_dir_all(dest)?;
        if present.is_empty() {
            return Ok(());
        }
        let mut c = git(Some(&self.dir), &self.url, &self.auth);
        c.args(["archive", "--format=tar", commit]);
        if !present.iter().any(|p| p.trim_matches('/').is_empty() || *p == ".") {
            c.arg("--");
            for p in &present {
                c.arg(p.trim_matches('/'));
            }
        }
        let mut child = c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().context("running git archive")?;
        std::thread::scope(|scope| -> Result<()> {
            // U08: drain both owned pipes while Git runs. Keep diagnostics bounded even on a
            // successful archive, and never let an extraction error leave its child unreaped.
            let reading = child.stderr.take().context("git archive has no diagnostics pipe").and_then(|mut stderr| {
                std::thread::Builder::new().name("kiln-git-archive-stderr".into()).spawn_scoped(scope, move || -> std::io::Result<(Vec<u8>, bool)> {
                    let mut prefix = Vec::with_capacity(64 * 1024);
                    stderr.by_ref().take(64 * 1024).read_to_end(&mut prefix)?;
                    let discarded = std::io::copy(&mut stderr, &mut std::io::sink())?;
                    Ok((prefix, discarded != 0))
                }).context("starting git archive diagnostics reader")
            });
            let (reader, setup) = match reading {
                Ok(reader) => (Some(reader), Ok(())),
                Err(error) => (None, Err(error)),
            };
            let extracted = setup.and_then(|()| -> Result<()> {
                let stdout = child.stdout.take().context("git archive has no output")?;
                let mut archive = tar::Archive::new(stdout);
                archive.set_preserve_permissions(false);
                archive.unpack(dest).context("unpacking git archive")?;
                // Git may still be writing padding after tar's end marker.
                std::io::copy(&mut archive.into_inner(), &mut std::io::sink()).context("reading git archive")?;
                Ok(())
            });
            // A failed setup has not taken stdout; a failed unpack/drain already dropped it.
            // Close it before stopping only this owned child, then wait even if stopping fails.
            drop(child.stdout.take());
            if extracted.is_err() { let _ = child.kill(); }
            let status = match child.wait() {
                Ok(status) => Ok(status),
                Err(first) => {
                    let stopped = child.kill();
                    child.wait().with_context(|| format!("git archive child {} reap outcome is unknown after wait failed ({first}) and stop returned {stopped:?}; final wait failed", child.id()))
                }
            };
            // Always collect the scoped reader before propagating extraction/wait failures.
            let diagnostics = match reader {
                Some(reader) => reader.join().map_err(|_| anyhow::anyhow!("git archive diagnostics reader failed"))
                    .and_then(|read| read.context("reading git archive diagnostics")),
                None => Ok((Vec::new(), false)),
            };
            let status = status?;
            extracted?;
            let (stderr, truncated) = diagnostics?;
            if !status.success() {
                let marker = if truncated { "\n[git archive stderr truncated after 65536 bytes]" } else { "" };
                bail!("git archive failed: {}{marker}", String::from_utf8_lossy(&stderr).trim());
            }
            Ok(())
        })
    }

    /// Whether the mirror holds a commit.
    pub fn has(&self, commit: &str) -> bool {
        let mut c = git(Some(&self.dir), &self.url, &self.auth);
        c.args(["cat-file", "-e", &format!("{commit}^{{commit}}")]);
        run(c).is_ok()
    }

    /// Fetches what the branches and tags don't hold: a commit by its hash, or a refspec
    /// (`+refs/pull/7/head:refs/kiln/pull/7`).
    pub fn fetch_one(&self, what: &str) -> Result<()> {
        futures::executor::block_on(self.fetch_one_using(what, |c| std::future::ready(run(c))))
    }

    /// Fetches a pinned commit or extra ref using the caller's process executor.
    pub async fn fetch_one_using<F, Fut>(&self, what: &str, execute: F) -> Result<()>
    where
        F: FnMut(Command) -> Fut,
        Fut: Future<Output = Result<Vec<u8>>>,
    {
        self.operation().await?.fetch_one_using(what, execute).await
    }

    /// Keep a resolved run/action commit reachable even after its branch or tag moves and Git
    /// performs automatic collection. This shares the mirror's writer gate with fetching.
    pub async fn retain_using<F, Fut>(&self, sha: &str, execute: F) -> Result<()>
    where
        F: FnMut(Command) -> Fut,
        Fut: Future<Output = Result<Vec<u8>>>,
    {
        self.operation().await?.retain_using(sha, execute).await.map(|_| ())
    }

    /// Every tag and the commit it names (an annotated tag's peeled to its commit).
    pub fn tags(&self) -> Result<Vec<(String, String)>> {
        let mut c = git(Some(&self.dir), &self.url, &self.auth);
        c.args(["for-each-ref", "--format=%(refname:short)%00%(objectname)%00%(*objectname)", "refs/tags"]);
        Ok(text(run(c)?)
            .lines()
            .filter_map(|l| {
                let mut f = l.split('\0');
                let (name, object, peeled) = (f.next()?, f.next()?, f.next().unwrap_or(""));
                Some((name.to_owned(), if peeled.is_empty() { object } else { peeled }.to_owned()))
            })
            .collect())
    }

    /// The commit a tag names, if the mirror has the tag.
    pub fn tag(&self, name: &str) -> Option<String> {
        let mut c = git(Some(&self.dir), &self.url, &self.auth);
        c.args(["rev-parse", "--verify", "--quiet", &format!("refs/tags/{name}^{{commit}}")]);
        run(c).ok().map(text).filter(|s| !s.is_empty())
    }

    /// The remote's default branch, as the mirror's `HEAD` names it.
    pub fn default_branch(&self) -> Option<String> {
        let mut c = git(Some(&self.dir), &self.url, &self.auth);
        c.args(["symbolic-ref", "--quiet", "--short", "HEAD"]);
        run(c).ok().map(text).filter(|b| !b.is_empty())
    }

    /// Branches, the most recently committed first (at most `max`): name, commit, and when it
    /// was committed (seconds since the epoch).
    pub fn branches(&self, max: usize) -> Result<Vec<(String, String, i64)>> {
        let mut c = git(Some(&self.dir), &self.url, &self.auth);
        c.args(["for-each-ref", "--sort=-committerdate", &format!("--count={max}"), "--format=%(refname:short)%00%(objectname)%00%(committerdate:unix)", "refs/heads"]);
        Ok(text(run(c)?)
            .lines()
            .filter_map(|l| {
                let mut f = l.split('\0');
                Some((f.next()?.to_owned(), f.next()?.to_owned(), f.next()?.trim().parse().unwrap_or(0)))
            })
            .collect())
    }

    /// A commit's first parent; `None` for a root commit.
    pub fn parent(&self, commit: &str) -> Result<Option<String>> {
        let mut c = git(Some(&self.dir), &self.url, &self.auth);
        c.args(["rev-list", "--parents", "--max-count=1", commit, "--"]);
        Ok(text(run(c)?).split_whitespace().nth(1).map(str::to_owned))
    }

    /// The newest commit both histories hold, if they meet.
    pub fn merge_base(&self, a: &str, b: &str) -> Option<String> {
        let mut c = git(Some(&self.dir), &self.url, &self.auth);
        c.args(["merge-base", a, b]);
        run(c).ok().map(text).filter(|s| !s.is_empty())
    }

    /// A stack: the commits from `base` (left out; `None` for the whole history) up to `head`
    /// along first parents, oldest first. More than `max` is an error.
    pub fn stack(&self, base: Option<&str>, head: &str, max: usize) -> Result<Vec<String>> {
        let mut c = git(Some(&self.dir), &self.url, &self.auth);
        c.args(["rev-list", "--first-parent", &format!("--max-count={}", max + 1), head]);
        if let Some(b) = base {
            c.arg(format!("^{b}"));
        }
        c.arg("--");
        let mut out: Vec<String> = text(run(c)?).lines().map(str::to_owned).collect();
        if out.len() > max {
            bail!("more than {max} commits lead from {} to {head}", base.unwrap_or("the first commit"));
        }
        out.reverse();
        Ok(out)
    }
}

impl OwnedOperation {
    /// The captured directory, while this operation retains its existing mutation fences.
    pub fn dir(&self) -> &Path { &self.mirror.dir }

    /// Release this operation after known completed phases. Pin every root needed by later
    /// reads first; an unknown phase never returns a usable mirror capability here.
    pub fn into_mirror(self) -> Result<Mirror> {
        anyhow::ensure!(!self.fenced, "a read-mirror operation has an unknown physical outcome");
        Ok(self.mirror)
    }

    /// Lend the same fences to the canonical physical process owner. Merely keeping this in
    /// the waiter future is insufficient; the owner must retain its clone before submission.
    pub fn custody(&self) -> OperationCustody { self.custody.clone() }

    /// Commands stay bound to the captured source, credentials and operation's phase state.
    fn command(&self) -> Command { git(Some(&self.mirror.dir), &self.mirror.url, &self.mirror.auth) }

    async fn execute_phase<F, Fut>(&mut self, command: Command, mut execute: F) -> Result<Vec<u8>>
    where
        F: FnMut(Command) -> Fut,
        Fut: Future<Output = Result<Vec<u8>>>,
    {
        anyhow::ensure!(!self.fenced, "a read-mirror operation has an unknown physical outcome");
        // Arm before invoking the executor: dropping this phase's future leaves the same
        // operation fenced. The physical owner separately retains its custody clone.
        self.fenced = true;
        let result = execute(command).await;
        if result.is_ok() || result.as_ref().is_err_and(|error| error.downcast_ref::<GitExit>().is_some()) { self.fenced = false; }
        result
    }

    /// Refresh the configured namespaces atomically, keeping both fences for later selection.
    /// There is exactly one fetch; selection and pinning never fetch implicitly.
    pub async fn fetch_using<F, Fut>(&mut self, mut execute: F) -> Result<()>
    where
        F: FnMut(Command) -> Fut,
        Fut: Future<Output = Result<Vec<u8>>>,
    {
        let mut c = self.command();
        c.args(["fetch", "--atomic", "--prune", "--quiet", "--force", &self.mirror.url]).args(self.mirror.options.refs.specs());
        self.execute_phase(c, &mut execute).await.with_context(|| format!("fetching {}", self.mirror.url)).map(|_| ())
    }

    /// Fetch only an explicitly requested extra ref or full commit. This does not refresh
    /// heads/tags as a side effect or drop the fence between its object and ref publication.
    pub async fn fetch_one_using<F, Fut>(&mut self, what: &str, mut execute: F) -> Result<()>
    where
        F: FnMut(Command) -> Fut,
        Fut: Future<Output = Result<Vec<u8>>>,
    {
        let mut c = self.command();
        let spec = if full_hash(what) { format!("{what}:refs/kiln/pins/{what}") } else { what.to_owned() };
        c.args(["fetch", "--atomic", "--quiet", "--force", &self.mirror.url, &spec]);
        self.execute_phase(c, &mut execute).await.with_context(|| format!("fetching {what} from {}", self.mirror.url)).map(|_| ())
    }

    /// Resolve and pin under the SAME operation. Short-name precedence is explicit; the
    /// successful candidate is returned alongside its full SHA and cannot be re-resolved by a
    /// later read. Full refs are tried literally, without inventing a different namespace.
    pub async fn select_using<F, Fut>(&mut self, revision: &str, order: RefOrder, mut execute: F) -> Result<SelectedCommit>
    where
        F: FnMut(Command) -> Fut,
        Fut: Future<Output = Result<Vec<u8>>>,
    {
        let rev = if revision.is_empty() { "HEAD" } else { revision };
        // U08: a full object ID is an exact pin, even when a branch/tag has that hex name.
        // retain_using verifies the resolved ID equals the requested commit before pinning.
        if full_hash(rev) { return self.retain_using(rev, execute).await; }
        let candidates = if rev.starts_with("refs/") {
            vec![rev.to_owned()]
        } else {
            let heads = format!("refs/heads/{rev}");
            let tags = format!("refs/tags/{rev}");
            match order {
                RefOrder::HeadsFirst => vec![heads, tags, rev.to_owned()],
                RefOrder::TagsFirst => vec![tags, heads, rev.to_owned()],
            }
        };
        for candidate in candidates {
            let mut c = self.command();
            c.args(["rev-parse", "--verify", "--quiet", "--end-of-options", &format!("{candidate}^{{commit}}")]);
            let out = match self.execute_phase(c, &mut execute).await {
                Ok(out) => out,
                Err(error) if error.downcast_ref::<GitExit>().is_some_and(|exit| exit.code() == Some(1)) => continue,
                Err(error) => return Err(error).context("selecting a Git revision"),
            };
            let sha = text(out);
            anyhow::ensure!(full_hash(&sha), "Git returned an invalid full commit hash");
            self.pin_locked(&sha, &mut execute).await?;
            let selected_ref = if candidate == rev && !rev.starts_with("refs/") && matches!(order, RefOrder::TagsFirst) { sha.clone() } else { candidate };
            return Ok(SelectedCommit { mirror: self.mirror.clone(), selected_ref, sha });
        }
        bail!("{} has no revision {rev:?}", self.mirror.url)
    }

    /// Pin an exact full commit without interpreting it as a branch or tag name.
    pub async fn retain_using<F, Fut>(&mut self, sha: &str, mut execute: F) -> Result<SelectedCommit>
    where
        F: FnMut(Command) -> Fut,
        Fut: Future<Output = Result<Vec<u8>>>,
    {
        anyhow::ensure!(full_hash(sha), "a retained Git object must be a full commit hash");
        let mut c = self.command();
        c.args(["rev-parse", "--verify", "--quiet", "--end-of-options", &format!("{sha}^{{commit}}")]);
        let out = self.execute_phase(c, &mut execute).await.map_err(|error| {
            if error.downcast_ref::<GitExit>().is_some_and(|exit| exit.code() == Some(1)) { error.context(MissingCommit { _private: () }) } else { error }
        })?;
        let resolved = text(out);
        anyhow::ensure!(full_hash(&resolved) && resolved.eq_ignore_ascii_case(sha), "a retained Git object must name that exact commit");
        self.pin_locked(&resolved, execute).await?;
        Ok(SelectedCommit { mirror: self.mirror.clone(), selected_ref: resolved.clone(), sha: resolved })
    }

    /// Single-ref publication uses Git's atomic update-ref. A failed selection returns no
    /// capability; already-created permanent pins are safe holdings, never half a snapshot.
    async fn pin_locked<F, Fut>(&mut self, sha: &str, mut execute: F) -> Result<()>
    where
        F: FnMut(Command) -> Fut,
        Fut: Future<Output = Result<Vec<u8>>>,
    {
        let mut c = self.command();
        c.args(["update-ref", &format!("refs/kiln/pins/{sha}"), sha]);
        self.execute_phase(c, &mut execute).await.map(|_| ())
    }
}

fn full_hash(sha: &str) -> bool { matches!(sha.len(), 40 | 64) && sha.bytes().all(|b| b.is_ascii_hexdigit()) }

/// Import immutable object files with hard links when possible, metadata with copies. Every
/// individual publication is atomic, so an interrupted import can be retried in place.
fn copy_mirror(from: &Path, to: &Path, objects: bool) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().ends_with(".lock") || name == "kiln-mirror" { continue; }
        let (src, dst) = (entry.path(), to.join(&name));
        let ty = entry.file_type()?;
        if ty.is_symlink() { bail!("a read mirror contains a symbolic link: {}", src.display()); }
        if ty.is_dir() {
            copy_mirror(&src, &dst, objects || name == "objects")?;
        } else if ty.is_file() && !dst.is_file() {
            let tmp = dst.with_extension(format!("import-{}", std::process::id()));
            // Never overwrite a leftover hard link in place: that could alter its old source.
            let _ = std::fs::remove_file(&tmp);
            if !objects || std::fs::hard_link(&src, &tmp).is_err() { std::fs::copy(&src, &tmp)?; }
            std::fs::rename(&tmp, &dst)?;
        }
    }
    Ok(())
}

/// Whether `dir` is `path` or one of its parents.
pub fn covers(dir: &str, path: &str) -> bool {
    let d = dir.trim_matches('/');
    d.is_empty() || path == d || path.strip_prefix(d).is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// U08: a source offline during cutover still has its old refs and pinned/unreferenced
    /// commits. An interrupted/repeated import does not need another clone or lose objects.
    #[test]
    fn u08_legacy_read_mirrors_import_offline_without_losing_cached_commits() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work)?;
        sh(&work, &["init", "-q", "-b", "main"])?;
        std::fs::write(work.join("a"), "first")?;
        sh(&work, &["add", "."])?;
        sh(&work, &["commit", "-qm", "first"])?;
        let url = work.to_string_lossy().into_owned();
        let old = tmp.path().join("old.git");
        let first = Mirror::open(&old, &url, Auth::default())?;
        let sha = first.resolve("main")?;
        // An orphaned cached commit is not brought over by cloning reachable refs alone.
        std::fs::write(work.join("a"), "cached pin")?;
        sh(&work, &["commit", "-qam", "cached"])?;
        let pin = sh(&work, &["rev-parse", "HEAD"])?;
        first.fetch_one(&pin)?;
        sh(&old, &["update-ref", "-d", &format!("refs/kiln/pins/{pin}")])?;
        std::fs::rename(&work, tmp.path().join("offline"))?;
        let canonical = tmp.path().join("canonical.git");
        for _ in 0..2 {
            let operation = futures::executor::block_on(Mirror::open_import_owned_using(
                &canonical, &url, Auth::default(), Options::default(), std::slice::from_ref(&old), |command, custody| {
                    let _held = custody;
                    let result = (|| -> Result<Vec<u8>> {
                        assert!(!command.get_args().any(|arg| arg == "clone" || arg == "fetch"), "offline import opens without another network transfer");
                        let probe = std::fs::OpenOptions::new().read(true).write(true).open(lock_path(&canonical)?)?;
                        assert!(matches!(probe.try_lock(), Err(std::fs::TryLockError::WouldBlock)), "every import command receives the existing physical fence");
                        run(command)
                    })();
                    std::future::ready(result)
                },
            ))?;
            assert_eq!(operation.dir(), canonical.as_path());
            let mirror = operation.into_mirror()?;
            assert_eq!(mirror.resolve("main")?, sha);
            assert_eq!(mirror.commit(&pin)?.subject, "cached");
            futures::executor::block_on(mirror.retain_using(&pin, |c| std::future::ready(run(c))))?;
            sh(&canonical, &["gc", "--quiet", "--prune=now"])?;
            assert_eq!(mirror.commit(&pin)?.subject, "cached", "a pinned action/run survives source ref changes and collection");
        }
        let uncertain = tmp.path().join("uncertain-import.git");
        let (mut attempts, mut held) = (0, None);
        let failed = futures::executor::block_on(Mirror::open_import_owned_using(
            &uncertain, &url, Auth::default(), Options::default(), std::slice::from_ref(&old), |_, custody| {
                attempts += 1;
                held = Some(custody);
                std::future::ready(Err(anyhow::anyhow!("legacy source probe outcome unknown")))
            },
        ));
        assert!(failed.is_err());
        assert_eq!(attempts, 1, "unknown source identity never falls through to clone or another command");
        assert!(!uncertain.join("HEAD").exists());
        let probe = std::fs::OpenOptions::new().read(true).write(true).open(lock_path(&uncertain)?)?;
        assert!(matches!(probe.try_lock(), Err(std::fs::TryLockError::WouldBlock)), "failed opening leaves physical custody with its executor");
        drop(held.take());
        probe.try_lock()?;
        Ok(())
    }

    /// U08: an independently opened fence holds a cold opener out; after release the
    /// opener succeeds, and the same inode remains the next handle's exclusion point.
    #[test]
    fn u08_cross_handle_fence_coordinates_cold_open_without_replacing_inode() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let source = tmp.path().join("source.git");
        std::fs::create_dir(&source)?;
        sh(&source, &["init", "--bare", "-q"])?;
        let dir = tmp.path().join("read.git");
        let path = lock_path(&dir)?;
        let owner = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(&path)?;
        owner.try_lock()?;
        let (started, ready) = std::sync::mpsc::channel();
        let (send, result) = std::sync::mpsc::channel();
        let (target, url) = (dir.clone(), source.to_string_lossy().into_owned());
        let worker = std::thread::spawn(move || {
            let _ = started.send(());
            let mut clones = 0;
            let opened = futures::executor::block_on(Mirror::open_owned_using(&target, &url, Auth::default(), Options::default(), |command, custody| {
                let _held = custody;
                if command.get_args().any(|arg| arg == "clone") { clones += 1; }
                assert!(!command.get_args().any(|arg| arg == "fetch"), "opening does not add a fetch phase");
                std::future::ready(run(command))
            })).and_then(OwnedOperation::into_mirror);
            assert_eq!(clones, 1);
            let _ = send.send(opened);
        });
        ready.recv()?;
        let before_release = result.recv_timeout(std::time::Duration::from_millis(30));
        owner.unlock()?; // Always release before asserting/joining, including failure paths.
        worker.join().map_err(|_| anyhow::anyhow!("cold opener thread failed"))?;
        assert!(matches!(before_release, Err(std::sync::mpsc::RecvTimeoutError::Timeout)), "cold opener must wait for the independent owner");
        let mirror = result.recv()??;
        assert_eq!(mirror.dir, dir);
        let admitted = futures::executor::block_on(mirror.operation())?;
        let custody = admitted.custody();
        drop(admitted);
        let probe = std::fs::OpenOptions::new().read(true).write(true).open(&path)?;
        assert!(matches!(probe.try_lock(), Err(std::fs::TryLockError::WouldBlock)), "a lent custody token still owns the same stable inode after the waiter drops");
        drop(custody);
        probe.try_lock()?;

        // Accepted command inputs can outlive a canceled opening waiter. Its original File
        // remains held by the executor's token, and no open capability was published.
        let canceled = tmp.path().join("canceled.git");
        let url = source.to_string_lossy();
        let held = Arc::new(parking_lot::Mutex::new(None));
        let accepted = held.clone();
        let mut opening = Box::pin(Mirror::open_owned_using(&canceled, &url, Auth::default(), Options::default(), move |_, custody| {
            *accepted.lock() = Some(custody);
            std::future::pending::<Result<Vec<u8>>>()
        }));
        let mut context = std::task::Context::from_waker(futures::task::noop_waker_ref());
        assert!(opening.as_mut().poll(&mut context).is_pending());
        drop(opening);
        assert!(!canceled.join("HEAD").exists());
        let canceled_probe = std::fs::OpenOptions::new().read(true).write(true).open(lock_path(&canceled)?)?;
        assert!(matches!(canceled_probe.try_lock(), Err(std::fs::TryLockError::WouldBlock)));
        drop(held.lock().take());
        canceled_probe.try_lock()?;

        let uncertain = tmp.path().join("uncertain-clone.git");
        let (mut held, mut unfinished) = (None, None);
        let failed = futures::executor::block_on(Mirror::open_owned_using(&uncertain, &url, Auth::default(), Options::default(), |command, custody| {
            held = Some(custody);
            let result: Result<Vec<u8>> = command.get_args().last().map(PathBuf::from).context("clone has no temporary destination").and_then(|temp| {
                std::fs::create_dir_all(&temp)?;
                std::fs::write(temp.join("still-owned"), b"in flight")?;
                unfinished = Some(temp);
                Err(anyhow::anyhow!("clone physical outcome unknown"))
            });
            std::future::ready(result)
        }));
        assert!(failed.is_err());
        assert!(!uncertain.join("HEAD").exists());
        assert!(unfinished.as_ref().is_some_and(|temp| temp.join("still-owned").is_file()), "unknown clone must not race removal of its still-owned temporary directory");
        let uncertain_probe = std::fs::OpenOptions::new().read(true).write(true).open(lock_path(&uncertain)?)?;
        assert!(matches!(uncertain_probe.try_lock(), Err(std::fs::TryLockError::WouldBlock)));
        drop(held.take());
        uncertain_probe.try_lock()?;
        Ok(())
    }

    fn sh(dir: &Path, args: &[&str]) -> Result<String> {
        // Free of this machine's git config: one that signs tags makes a lightweight `git tag` ask
        // for a message.
        let out = Command::new("git").current_dir(dir).args(args).env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t").env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1").output()?;
        if !out.status.success() {
            bail!("{}", String::from_utf8_lossy(&out.stderr));
        }
        Ok(text(out.stdout))
    }

    #[test]
    fn a_commit_says_who_when_and_what() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work)?;
        sh(&work, &["init", "-q", "-b", "main"])?;
        std::fs::write(work.join("a.yaml"), "kind: ConfigMap\n")?;
        sh(&work, &["add", "."])?;
        sh(&work, &["commit", "-qm", "Fix the probe\n\nIt timed out on cold starts."])?;
        let head = sh(&work, &["rev-parse", "HEAD"])?;
        let m = Mirror::open(&tmp.path().join("mirror.git"), work.to_str().unwrap_or(""), Auth::default())?;
        let c = m.commit("main")?;
        assert_eq!(c.sha, head);
        assert_eq!((c.author.as_str(), c.email.as_str()), ("t", "t@t"));
        assert_eq!(c.subject, "Fix the probe");
        assert_eq!(c.body, "It timed out on cold starts.");
        assert!(c.at > 1_600_000_000);
        assert!(m.commit("0000000000000000000000000000000000000000").is_err());
        Ok(())
    }

    #[test]
    fn tags_name_their_commits_annotated_or_not() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work)?;
        sh(&work, &["init", "-q", "-b", "main"])?;
        std::fs::write(work.join("a.yaml"), "kind: ConfigMap\n")?;
        sh(&work, &["add", "."])?;
        sh(&work, &["commit", "-qm", "one"])?;
        let one = sh(&work, &["rev-parse", "HEAD"])?;
        sh(&work, &["tag", "v1.0.0"])?;
        sh(&work, &["tag", "-a", "release-1", "-m", "the first release"])?;
        std::fs::write(work.join("a.yaml"), "kind: Secret\n")?;
        sh(&work, &["commit", "-qam", "two"])?;
        let two = sh(&work, &["rev-parse", "HEAD"])?;
        sh(&work, &["tag", "-a", "v1.1.0", "-m", "annotated"])?;
        let m = Mirror::open(&tmp.path().join("mirror.git"), work.to_str().unwrap_or(""), Auth::default())?;
        let mut tags = m.tags()?;
        tags.sort();
        assert_eq!(tags, [("release-1".to_owned(), one.clone()), ("v1.0.0".to_owned(), one.clone()), ("v1.1.0".to_owned(), two.clone())], "an annotated tag names its commit, not itself");
        assert_eq!(m.tag("v1.1.0"), Some(two));
        assert_eq!(m.tag("release-1"), Some(one));
        assert_eq!(m.tag("main"), None, "a branch is no tag");
        assert_eq!(m.tag("v9"), None);
        Ok(())
    }

    #[test]
    fn a_stack_is_its_commits_above_where_it_leaves_trunk() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work)?;
        sh(&work, &["init", "-q", "-b", "main"])?;
        let commit = |msg: &str| -> Result<String> {
            std::fs::write(work.join(format!("{msg}.yaml")), "kind: ConfigMap\n")?;
            sh(&work, &["add", "."])?;
            sh(&work, &["commit", "-qm", msg])?;
            sh(&work, &["rev-parse", "HEAD"])
        };
        let root = commit("root")?;
        let trunk = commit("trunk")?;
        sh(&work, &["checkout", "-qb", "stack"])?;
        let d1 = commit("d1")?;
        let d2 = commit("d2")?;
        sh(&work, &["checkout", "-q", "main"])?;
        let moved = commit("moved")?;
        let m = Mirror::open(&tmp.path().join("mirror.git"), work.to_str().unwrap_or(""), Auth::default())?;
        assert_eq!(m.default_branch().as_deref(), Some("main"));
        let names: Vec<String> = m.branches(10)?.into_iter().map(|(name, _, _)| name).collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"main".to_owned()) && names.contains(&"stack".to_owned()));
        assert_eq!(m.branches(1)?.len(), 1);
        assert!(m.has(&d2) && !m.has("0000000000000000000000000000000000000000"));
        assert_eq!(m.parent(&d1)?.as_deref(), Some(trunk.as_str()));
        assert_eq!(m.parent(&root)?, None);
        let base = m.merge_base(&moved, &d2);
        assert_eq!(base.as_deref(), Some(trunk.as_str()));
        assert_eq!(m.stack(base.as_deref(), "stack", 10)?, [d1.clone(), d2.clone()]);
        assert!(m.stack(base.as_deref(), "stack", 1).is_err());
        assert_eq!(m.stack(Some(&d2), &d2, 10)?, Vec::<String>::new());
        assert_eq!(m.stack(None, &trunk, 10)?, [root, trunk]);
        Ok(())
    }

    #[test]
    fn paths_hash_by_content_and_extract_with_their_layout() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let work = tmp.path().join("work");
        std::fs::create_dir_all(work.join("deploy/web"))?;
        std::fs::create_dir_all(work.join("deploy/api"))?;
        sh(&work, &["init", "-q", "-b", "main"])?;
        std::fs::write(work.join("deploy/web/cm.yaml"), "kind: ConfigMap\n")?;
        std::fs::write(work.join("deploy/api/cm.yaml"), "kind: ConfigMap\n")?;
        sh(&work, &["add", "."])?;
        sh(&work, &["commit", "-qm", "one"])?;
        let one = sh(&work, &["rev-parse", "HEAD"])?;
        std::fs::write(work.join("deploy/api/cm.yaml"), "kind: Secret\n")?;
        sh(&work, &["commit", "-qam", "two"])?;
        let two = sh(&work, &["rev-parse", "HEAD"])?;

        let m = Mirror::open(&tmp.path().join("mirror.git"), work.to_str().unwrap_or(""), Auth::default())?;
        assert_eq!(m.resolve("main")?, two);
        let h1 = m.hashes(&one, &["deploy/web", "deploy/api", "nope"])?;
        let h2 = m.hashes(&two, &["deploy/web", "deploy/api", "nope"])?;
        assert_eq!(h1.first(), h2.first(), "web didn't change");
        assert_ne!(h1.get(1), h2.get(1), "api did");
        assert_eq!(h1.get(2), Some(&None));
        assert_eq!(m.changed(&one, &two)?, ["deploy/api/cm.yaml"]);
        assert_eq!(m.read(&two, "deploy/api/cm.yaml")?.as_deref(), Some(&b"kind: Secret\n"[..]));
        let out = tmp.path().join("out");
        m.extract(&two, &["deploy/web", "missing"], &out)?;
        assert!(out.join("deploy/web/cm.yaml").exists());
        assert!(!out.join("deploy/api").exists());
        let entries = m.list(&two, "deploy", true)?;
        assert_eq!(entries.iter().filter(|e| !e.tree).count(), 2);

        // U08: one operation snapshots a colliding branch/tag without a hidden second fetch.
        // After the next writer moves/prunes those refs and collects aggressively, all reads
        // and extraction from the old selection still use its original reachable commit.
        sh(&work, &["branch", "same", &two])?;
        sh(&work, &["tag", "same", &one])?;
        sh(&work, &["branch", &one, &two])?;
        sh(&work, &["tag", &one, &two])?;
        let mut operation = futures::executor::block_on(m.operation())?;
        let mut fetches = 0;
        let mut execute = |c: Command| {
            if c.get_args().any(|a| a == "fetch") { fetches += 1; }
            std::future::ready(run(c))
        };
        futures::executor::block_on(operation.fetch_using(&mut execute))?;
        let source = futures::executor::block_on(operation.select_using("same", RefOrder::HeadsFirst, &mut execute))?;
        let action = futures::executor::block_on(operation.select_using("same", RefOrder::TagsFirst, &mut execute))?;
        let explicit = futures::executor::block_on(operation.select_using("refs/heads/same", RefOrder::TagsFirst, &mut execute))?;
        let pinned_source = futures::executor::block_on(operation.select_using(&one, RefOrder::HeadsFirst, &mut execute))?;
        let pinned_action = futures::executor::block_on(operation.select_using(&one, RefOrder::TagsFirst, &mut execute))?;
        let hex_head = futures::executor::block_on(operation.select_using(&format!("refs/heads/{one}"), RefOrder::TagsFirst, &mut execute))?;
        let hex_tag = futures::executor::block_on(operation.select_using(&format!("refs/tags/{one}"), RefOrder::HeadsFirst, &mut execute))?;
        assert_eq!((source.sha(), source.selected_ref()), (two.as_str(), "refs/heads/same"));
        assert_eq!((action.sha(), action.selected_ref()), (one.as_str(), "refs/tags/same"));
        assert_eq!(explicit.sha(), source.sha(), "a required heads ref never falls back to a same-named tag");
        assert_eq!((pinned_source.sha(), pinned_action.sha()), (one.as_str(), one.as_str()), "literal full commit pins bypass same-named branches and tags for both precedences");
        assert_eq!((pinned_source.selected_ref(), pinned_action.selected_ref()), (one.as_str(), one.as_str()));
        assert_eq!((hex_head.sha(), hex_tag.sha()), (two.as_str(), two.as_str()), "qualified hex-named refs retain their explicit targets");
        assert_eq!(fetches, 1, "selection and reachability publication do not refresh the source again");
        let mut attempts = 0;
        let unknown = futures::executor::block_on(operation.select_using("same", RefOrder::HeadsFirst, |_| {
            attempts += 1;
            std::future::ready(Err(anyhow::anyhow!("physical outcome unknown")))
        }));
        assert!(unknown.is_err());
        let refused = futures::executor::block_on(operation.fetch_using(|_| {
            attempts += 1;
            std::future::ready(Ok(Vec::new()))
        }));
        assert!(refused.is_err());
        assert_eq!(attempts, 1, "unknown executor/retirement failure fences fallback and later phases");
        assert!(operation.into_mirror().is_err(), "an unknown phase cannot return a usable mirror capability");
        sh(&work, &["reset", "--hard", &one])?;
        sh(&work, &["branch", "-D", "same", &one])?;
        sh(&work, &["tag", "-d", "same", &one])?;
        m.fetch()?;
        sh(&m.dir, &["reflog", "expire", "--expire=now", "--all"])?;
        sh(&m.dir, &["gc", "--quiet", "--prune=now"])?;
        assert_eq!(m.resolve("main")?, one, "the new refresh sees the moved source ref");
        assert!(m.select("refs/heads/same", RefOrder::HeadsFirst).is_err(), "the removed ref really was pruned");
        assert_eq!(source.read("deploy/api/cm.yaml")?.as_deref(), Some(&b"kind: Secret\n"[..]));
        assert_eq!(action.read("deploy/api/cm.yaml")?.as_deref(), Some(&b"kind: ConfigMap\n"[..]));
        assert_eq!(source.hashes(&["deploy/web", "deploy/api", "nope"])?, h2);
        let pinned_out = tmp.path().join("pinned-out");
        source.extract(&["deploy/api"], &pinned_out)?;
        assert_eq!(std::fs::read(pinned_out.join("deploy/api/cm.yaml"))?, b"kind: Secret\n");
        Ok(())
    }

    #[test]
    fn many_paths_hash_without_git_s_pipes_filling() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let work = tmp.path().join("work");
        std::fs::create_dir_all(work.join("deploy"))?;
        sh(&work, &["init", "-q", "-b", "main"])?;
        std::fs::write(work.join("deploy/cm.yaml"), "kind: ConfigMap\n")?;
        sh(&work, &["add", "."])?;
        sh(&work, &["commit", "-qm", "one"])?;
        let head = sh(&work, &["rev-parse", "HEAD"])?;
        let m = Mirror::open(&tmp.path().join("mirror.git"), work.to_str().unwrap_or(""), Auth::default())?;
        // ~1 MB of answers: far past a pipe's 64 KiB, let alone the 8 KiB a crowded machine's
        // pipes get. On a thread of its own, so a git that stops answering fails the test.
        let mut paths: Vec<String> = (0..20_000).map(|i| format!("deploy/missing-{i}")).collect();
        paths.push("deploy".to_owned());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
            let _ = tx.send(m.hashes(&head, &paths));
        });
        let hashes = rx.recv_timeout(std::time::Duration::from_secs(60)).map_err(|_| anyhow::anyhow!("git cat-file never answered"))??;
        assert_eq!(hashes.len(), 20_001);
        assert!(hashes.iter().take(20_000).all(Option::is_none));
        assert!(hashes.last().is_some_and(Option::is_some), "deploy is there");
        Ok(())
    }
}

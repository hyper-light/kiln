//! Step reuse on the runner (docs/ci.md W24-R07): a step that says `memo:` is made a unit before it
//! runs (`kiln_ci::work::memo`), which the server's work ledger keys and looks up on its one path
//! (W24-R30; `/_kiln/work/v1/`, `crate::ci::services::memo`) with the leg's runtime token, as
//! `kiln/cache@v1` reaches the server's cache (`runner::cache`). A hit isn't run: the server gives
//! the step its source's log, summary, annotations and test reports, and here its outputs, the
//! variables it set, what it put on `PATH` and its `run.env` lines are replayed; the files it kept
//! (`memo.outputs`) are put in place only once a later step runs. A miss runs, and a success is
//! kept: its files as a tree in the content store, then its result. A failure, a cancellation or a
//! timeout never is. Each memo step says in its log which of these it was, and why.
//!
//! Only the leg's own steps are keyed: a background step or a composite action's step runs as any
//! other, but what earlier hits left is put in place before it does.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::time::{Duration, Instant};

use kiln_ci::wf::ir::Step;
use kiln_ci::wire::Conclusion;
use kiln_ci::work::key::{self, Platform, Unit};
use kiln_ci::work::memo::{self, Definition, Digest, Entry, Facts, Memo, Replay, Tree};
use serde_json::{Value, json};

use super::{Leg, State};

/// The largest tree a step's files make, and the most bytes they hold.
const MAX_TREE: u64 = 64 << 20;
const MAX_BYTES: u64 = 16 << 30;
/// Files up to this size are fetched whole.
const WHOLE: u64 = 8 << 20;
/// Uploads and downloads at once (a batch of `join_all`, well under the 30 it polls with the task's
/// own waker).
const AT_ONCE: usize = 4;
/// Digests one `missing` asks about (the server takes up to 200,000).
const ASK_AT_ONCE: usize = 100_000;

/// A request under way, boxed so a batch of them is [`Send`].
type Call<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

/// Runs a batch of calls together; the first error is theirs. `join_all`, not `buffer_unordered`:
/// hyper-rt's sockets know the task polling them by its waker, which `FuturesUnordered` replaces
/// with one of its own (`cd::live`'s `bounded` says more).
async fn together(calls: Vec<Call<'_>>) -> anyhow::Result<()> {
    futures::future::join_all(calls).await.into_iter().collect()
}

/// What `tool` says of its version (W28-R71: `rustc -vV`, else `--version`), run where the step
/// runs with the step's `PATH`; why not when it can't be asked.
async fn probe(tool: &str, path: &str, wd: &Path) -> Result<String, String> {
    // The program as the step's `PATH` finds it; else where a login shell's `PATH`, or the usual
    // toolchain folders, have it (a launchd service's `PATH` has none of them, and a step that
    // adds them to its own `PATH` does so in its script).
    let home = crate::os::home_dir();
    let usual = [home.join(".cargo/bin"), PathBuf::from("/opt/homebrew/bin"), PathBuf::from("/usr/local/bin")];
    let login = crate::reach::serve::login_path().unwrap_or_default();
    let found = std::env::split_paths(path)
        .chain(std::env::split_paths(&login))
        .chain(usual)
        .flat_map(|d| [d.join(tool), d.join(format!("{tool}.exe"))])
        .find(|p| p.is_file());
    let mut c = Command::new(found.as_deref().unwrap_or_else(|| Path::new(tool)));
    c.args(memo::version_args(tool)).current_dir(wd).env("PATH", path);
    match crate::proc::output(c, Duration::from_secs(30)).await {
        Ok(o) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).into_owned()),
        Ok(o) => Err(format!("`{tool}` didn't say its version: {}", String::from_utf8_lossy(&o.stderr).trim())),
        Err(e) => Err(format!("`{tool}` couldn't be asked its version: {e:#}")),
    }
}

/// What a request sends: bytes, or a file read as it goes.
enum Body {
    Bytes(Vec<u8>),
    File(PathBuf),
}

/// `git` in the workspace: what it printed, when it succeeded.
async fn git_out(ws: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let mut c = Command::new("git");
    c.args(["-c", "safe.directory=*", "-C"]).arg(ws).args(args).env("GIT_TERMINAL_PROMPT", "0").env("GIT_OPTIONAL_LOCKS", "0");
    match crate::proc::output(c, Duration::from_secs(300)).await {
        Ok(o) if o.status.success() => Some(o.stdout),
        _ => None,
    }
}

/// What a leg keeps of its memo steps (W24-R03, R07).
#[derive(Debug, Clone, Default)]
pub(super) struct Chain {
    /// What earlier memo steps left, as later steps' keys take it: `step.<n>` to its [`Replay`]'s
    /// digest, or to how it ended when nothing was kept.
    upstream: BTreeMap<String, String>,
    /// Their `memo.outputs`: no later step's inputs read these from the workspace, as their digests
    /// stand for them.
    outputs: Vec<String>,
    /// Hits whose files aren't in place yet, oldest first.
    pending: VecDeque<Pending>,
    /// The variables the leg had as its first step began (kiln's and the workflow's): those earlier
    /// steps set besides them are a key's too.
    base: Option<BTreeSet<String>>,
    /// Why memo steps run without the ledger, once the server said it has none.
    off: Option<String>,
    /// Memo steps that left a result, by their own key, for portable steps' keys (W28-R70).
    earlier: Vec<memo::Earlier>,
}

#[derive(Debug, Clone)]
struct Pending {
    tree: Digest,
    /// `run #3 (<id>)`.
    from: String,
}

/// What a step is to do, looked up.
pub(super) enum Prepared {
    /// Recorded before: replay it.
    Hit(Box<Hit>),
    /// Run it: keyed, its success is kept.
    Run(Option<Box<Keyed>>),
    /// It can't run: what an earlier hit left couldn't be put in place.
    Fail(String),
}

/// A memo step that runs, and the leg as it found it, to tell what it changed.
pub(super) struct Keyed {
    /// Its unit, as the server's ledger keys it (W24-R30).
    unit: Unit,
    /// The lease it executes under, other legs' identical steps waiting on it (W28-R72), and what
    /// renews it while it runs.
    lease: Option<String>,
    renewal: Option<Renewal>,
    memo: Memo,
    started: Instant,
    env: BTreeMap<String, String>,
    path: Vec<String>,
    run_env: usize,
    masks: usize,
}

/// A recorded success for a step's key.
pub(super) struct Hit {
    key: String,
    /// The step's own weak key, as portable steps after it name it (W28-R70).
    weak: String,
    replay: Replay,
    memo: Memo,
    source: Value,
}

/// Renews a lease every quarter of its life while the step runs (W28-R72); dropped, it stops.
pub(super) struct Renewal {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for Renewal {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// How often a lease is renewed: a quarter of its life.
const RENEW_EVERY: Duration = Duration::from_millis(kiln_ci::work::flight::LEASE_TTL_MS.unsigned_abs() / 4);

fn short(key: &str) -> &str {
    key.get(..12).unwrap_or(key)
}

/// What the server said: its services put it in `message`, its routes' errors in `error` (an older
/// server's `not found` among them).
fn said(v: &Value) -> String {
    v.get("message").or_else(|| v.get("error")).and_then(Value::as_str).unwrap_or("no reason given").to_owned()
}

/// Whether `name` is a variable a toolchain reads ([`memo::TOOLCHAIN_ENV`]).
fn toolchain_var(name: &str) -> bool {
    memo::TOOLCHAIN_ENV.iter().any(|t| t.strip_suffix('*').map_or(*t == name, |prefix| name.starts_with(prefix)))
}

impl Leg {
    /// `/_kiln/work/v1/<what>`, as this leg reaches the server.
    fn work_url(&self, what: &str) -> String {
        format!("{}/_kiln/work/v1/{what}", crate::runner::artifacts::Service::origin_of(&self.a.runtime.results_url))
    }

    pub(super) async fn work_post(&self, what: &str, body: &Value, wait: Duration) -> anyhow::Result<(u16, Value)> {
        let resp = self.http.post(&self.work_url(what)).bearer(&self.a.runtime.runtime_token).json(body).timeout(wait).send().await?;
        let status = resp.status();
        let v = if status == 204 { Value::Null } else { resp.json(64 << 20).await.unwrap_or_default() };
        Ok((status, v))
    }

    /// Whether `text` holds a secret: anything the leg masks but kiln's own tokens for it, which
    /// every step gets and no key keeps (W24-R05).
    fn memo_secret_in(&self, st: &State, text: &str) -> bool {
        let rt = &self.a.runtime;
        let own = [Some(&rt.github_token), Some(&rt.runtime_token), rt.id_token_request_token.as_ref()];
        let t = own.into_iter().flatten().filter(|x| !x.is_empty()).fold(text.to_owned(), |t, x| t.replace(x.as_str(), ""));
        st.masker.mask(&t) != t
    }

    /// Before a step runs (W24-R07): a memo step's key looked up, a hit to replay; else whatever
    /// earlier hits left put in place, as the step may read it.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn memo_prepare(&self, st: &mut State, idx: u32, step: &Step, nested: bool, env: &BTreeMap<String, String>, wd: &Path, shell: Option<&str>, script: Option<&str>, with: &BTreeMap<String, String>, wait_until: Instant) -> Prepared {
        if nested {
            return Prepared::Run(None);
        }
        if st.memo.base.is_none() {
            st.memo.base = Some(st.env.keys().cloned().collect());
        }
        let memo = match step.kiln.memo.as_ref().map(Memo::from_yval) {
            None | Some(Ok(None)) => None,
            Some(Ok(Some(m))) => Some(m),
            Some(Err(e)) => {
                self.annotate(idx, "warning", &format!("memo: {e}: the step runs as any other")).await;
                None
            }
        };
        let mut keyed = None;
        if let Some(m) = memo {
            if let Some(why) = st.memo.off.clone() {
                self.note(idx, &format!("memo: {why}: the step runs")).await;
            } else if st.stop.is_some() {
                self.note(idx, "memo: a background step runs as any other: only the leg's own steps are reused (docs/ci.md W24-R07)").await;
            } else if !usize::try_from(idx).is_ok_and(|i| i < self.a.steps.len()) {
                // Its log's index is past the job's steps: a step rerun in place (W17-R04).
                self.note(idx, "memo: a step rerun in place runs as any other, and isn't kept (docs/ci.md W24-R07)").await;
            } else {
                match self.memo_key(st, step, &m, env, wd, shell, script, with).await {
                    Err(why) => self.note(idx, &format!("memo: {why}: the step runs")).await,
                    Ok(unit) => {
                        // The ledger keys the unit on its one path (W24-R30); its weak key names it here.
                        let weak = key::weak(&unit);
                        // A miss is a turn among the legs that reach the same step now (W28-R72): one
                        // executes it, the others wait for it (W28-R75), as long as the job may.
                        let ask = json!({ "unit": unit, "step": idx, "max_age": m.max_age, "flight": true });
                        let mut answer = self.work_post("lookup", &ask, Duration::from_secs(60)).await;
                        let mut waited: Option<(String, Instant)> = None;
                        while let Ok((202, v)) = &answer {
                            let said = v.pointer("/in_flight/said").and_then(Value::as_str).unwrap_or("another leg's step").to_owned();
                            if waited.as_ref().is_none_or(|(w, _)| *w != said) {
                                let turn = v.pointer("/in_flight/turn").and_then(Value::as_bool).unwrap_or(false);
                                let why = if turn { format!("waiting for {said} to take its turn at the same work, ahead of this one") } else { format!("waiting on {said}: the same work in flight") };
                                self.note(idx, &format!("memo: {why} (key {})", short(&weak.hex()))).await;
                                waited = Some((said, waited.map_or_else(Instant::now, |(_, t)| t)));
                            }
                            if Instant::now() >= wait_until {
                                self.note(idx, "memo: the job's time ran out waiting for the same work in flight: the step runs").await;
                                answer = Ok((204, Value::Null));
                                break;
                            }
                            answer = self.work_post("wait", &ask, Duration::from_secs(60)).await;
                        }
                        if let Some((said, since)) = &waited {
                            self.note(idx, &format!("memo: waited {:.1} s on {said}", since.elapsed().as_secs_f64())).await;
                        }
                        let mut lease = None;
                        match answer {
                            Ok((200, v)) => {
                                let replay = serde_json::from_value::<BTreeMap<String, String>>(v.get("outputs").cloned().unwrap_or_default()).map_err(|e| e.to_string()).and_then(|o| Replay::decode(&o));
                                match replay {
                                    Ok(replay) => {
                                        let key = v.get("key").and_then(Value::as_str).unwrap_or_default().to_owned();
                                        return Prepared::Hit(Box::new(Hit { key, weak: weak.hex(), replay, memo: m, source: v.get("source").cloned().unwrap_or_default() }));
                                    }
                                    Err(e) => self.note(idx, &format!("memo: the recorded result doesn't read ({e}): the step runs")).await,
                                }
                            }
                            Ok((201, v)) => {
                                lease = v.get("lease").and_then(Value::as_str).map(str::to_owned);
                                self.note(idx, &format!("memo: no earlier success with these inputs (key {}): the step runs, and the same step elsewhere waits on it", short(&weak.hex()))).await;
                            }
                            Ok((204, _)) => self.note(idx, &format!("memo: no earlier success with these inputs (key {}): the step runs", short(&weak.hex()))).await,
                            Ok((404, v)) if said(&v) == "not found" => {
                                st.memo.off = Some("the server keeps no steps' results (it's older than docs/ci.md W24-R07)".into());
                                self.note(idx, "memo: the server keeps no steps' results (it's older than docs/ci.md W24-R07): the step runs").await;
                            }
                            Ok((status, v)) => self.note(idx, &format!("memo: the lookup failed (HTTP {status}: {}): the step runs", said(&v))).await,
                            Err(e) => self.note(idx, &format!("memo: the work ledger couldn't be reached ({e:#}): the step runs")).await,
                        }
                        if st.memo.off.is_none() {
                            let renewal = lease.clone().map(|l| self.renew_while_running(l));
                            keyed = Some(Box::new(Keyed { unit, lease, renewal, memo: m, started: Instant::now(), env: st.env.clone(), path: st.path.entries().to_vec(), run_env: st.run_env.len(), masks: st.masks_added.len() }));
                        }
                    }
                }
            }
        }
        match self.memo_materialize(st, idx).await {
            Ok(()) => Prepared::Run(keyed),
            Err(e) => {
                if let Some(l) = keyed.and_then(|k| k.lease) {
                    self.release(&l).await;
                }
                Prepared::Fail(e)
            }
        }
    }

    /// Renews `lease` until the step ends (W28-R72): its waiters wait as long as it runs, and take
    /// their turn within a lease's life if its runner is lost.
    fn renew_while_running(&self, lease: String) -> Renewal {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (flag, http, url, token) = (std::sync::Arc::clone(&stop), std::sync::Arc::clone(&self.http), self.work_url("renew"), self.a.runtime.runtime_token.clone());
        crate::rt::spawn(async move {
            loop {
                crate::rt::sleep(RENEW_EVERY).await;
                if flag.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                let held = match http.post(&url).bearer(&token).json(&json!({ "lease": lease })).timeout(Duration::from_secs(30)).send().await {
                    Ok(r) if r.status() == 200 => r.json(1 << 16).await.ok().and_then(|v: Value| v.get("held").and_then(Value::as_bool)).unwrap_or(true),
                    _ => true,
                };
                if !held {
                    tracing::debug!(lease, "a step's lease lapsed while it ran: another leg may run it too");
                    return;
                }
            }
        });
        Renewal { stop }
    }

    /// Lets a lease go without a success, so the next leg waiting takes its turn (W28-R73).
    async fn release(&self, lease: &str) {
        if let Err(e) = self.work_post("release", &json!({ "lease": lease }), Duration::from_secs(30)).await {
            tracing::debug!(lease, error = %format!("{e:#}"), "a step's lease couldn't be let go: it lapses");
        }
    }

    /// The step's unit (W24-R01), which the server's ledger keys (W24-R30): the step evaluated, the
    /// environment the rules keep, the platform, its inputs' git trees, what came before it and its
    /// trust. Why not, when it can't be keyed: it receives secrets (W24-R05), or its inputs can't be
    /// known.
    #[allow(clippy::too_many_arguments)]
    async fn memo_key(&self, st: &State, step: &Step, m: &Memo, env: &BTreeMap<String, String>, wd: &Path, shell: Option<&str>, script: Option<&str>, with: &BTreeMap<String, String>) -> Result<Unit, String> {
        let a = &self.a;
        if !step.kiln.with_secrets.is_empty() || step.uses.as_ref().and_then(|u| a.actions.get(u)).is_some_and(|x| !x.secrets.is_empty()) {
            return Err("it's given secrets, so it isn't reused (W24-R05's `memo.secrets: hmac` isn't built yet)".into());
        }
        if !step.kiln.integrations.is_empty() {
            return Err("it authenticates to integrations, so it isn't reused (W24-R05)".into());
        }
        if step.uses.as_deref().is_some_and(|u| u.starts_with("docker://") && !u.contains("@sha256:")) {
            return Err("its image is named by a tag, which may move, so it isn't reused: name it by digest (`docker://image@sha256:…`)".into());
        }
        let (l, rt) = (&st.layout, &a.runtime);
        // The leg's own paths and its own tokens, as names: every leg has its own, and they don't
        // change what the step does (W24-R05).
        let mut places = vec![(l.workspace.to_string_lossy().into_owned(), "workspace"), (l.temp.to_string_lossy().into_owned(), "temp"), (l.tool_cache.to_string_lossy().into_owned(), "tools"), (l.runner_workspace.to_string_lossy().into_owned(), "runner")];
        places.extend([(rt.github_token.clone(), "github.token"), (rt.runtime_token.clone(), "runtime.token")]);
        let un = |s: &str| memo::unplaced(s, &places);
        let working_directory = wd.strip_prefix(&l.workspace).map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_else(|_| un(&wd.to_string_lossy()));
        let definition = Definition {
            uses: step.uses.clone(),
            pin: step.uses.as_ref().and_then(|u| a.actions.get(u)).map(|r| format!("{}@{}:{}", r.repo, r.sha, r.path)),
            run: script.map(un),
            shell: shell.map(un),
            with: with.iter().map(|(k, v)| (k.clone(), un(v))).collect(),
            env: step.env.iter().filter_map(|(k, _)| env.get(k).map(|v| (k.clone(), un(v)))).collect(),
            working_directory,
            test_results: step.kiln.test_results.as_ref().and_then(|t| serde_json::to_string(t).ok()),
            outputs: m.outputs.clone(),
        };
        // What the step gets: the runner's toolchain variables, kiln's and GitHub's, the job's and its own.
        let mut all: BTreeMap<String, String> = std::env::vars().filter(|(k, _)| toolchain_var(k)).collect();
        all.extend(self.env_vars(st));
        all.extend(env.iter().map(|(k, v)| (k.clone(), v.clone())));
        let text = serde_json::to_string(&(script, shell, with, &definition.env)).unwrap_or_default();
        if self.memo_secret_in(st, &text) || all.iter().any(|(k, v)| !key::per_run(k) && self.memo_secret_in(st, v)) {
            return Err("it receives a secret, so it isn't reused (W24-R05's `memo.secrets: hmac` isn't built yet)".into());
        }
        let all: BTreeMap<String, String> = all.into_iter().map(|(k, v)| (k, un(&v))).collect();
        let base = st.memo.base.clone().unwrap_or_default();
        let mut declared: BTreeSet<String> = a.workflow_env.iter().chain(a.job_env.iter()).chain(step.env.iter()).map(|(k, _)| k.clone()).collect();
        declared.extend(a.env_from.iter().map(|(k, _)| k.clone()));
        declared.extend(m.env.iter().cloned());
        declared.extend(st.env.keys().filter(|k| !base.contains(*k)).cloned());
        let image = match &st.containers {
            Some(c) if c.job.is_some() => c.images().await.into_iter().find(|(role, _, _)| role == "job").map(|(_, _, id)| id),
            _ => None,
        };
        let platform = Platform {
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            libc: (if cfg!(target_os = "linux") { if super::musl_here(st) { "musl" } else { "gnu" } } else { "" }).into(),
            executor: if a.executor.kind.is_empty() { "host".into() } else { a.executor.kind.clone() },
            image,
            toolchains: BTreeMap::new(),
        };
        let mut upstream = st.memo.upstream.clone();
        if a.contexts.needs.as_object().is_some_and(|n| !n.is_empty()) {
            upstream.insert("needs".into(), memo::digest_json(&a.contexts.needs));
        }
        if let Some(e) = a.contexts.run.get("env").filter(|e| e.as_object().is_some_and(|o| !o.is_empty())) {
            upstream.insert("run".into(), memo::digest_json(e));
        }
        // A local action's own files are part of what the step does, whatever its inputs say.
        let mut declared_inputs = m.inputs.clone();
        if let Some(dir) = step.uses.as_deref().and_then(|u| u.strip_prefix("./")).map(|d| d.trim_end_matches('/')).filter(|d| !d.is_empty() && !declared_inputs.is_empty()) {
            declared_inputs.push(dir.to_owned());
        }
        let inputs = self.memo_inputs(st, &declared_inputs, &m.outputs).await?;
        if m.platform.is_some() {
            // A portable step (W28-R70, R71): its toolchains' identities, at the precision it gives.
            let mut platform = platform;
            for (tool, precision) in &m.tools {
                if st.containers.as_ref().is_some_and(|c| c.job.is_some()) {
                    return Err(format!("`memo.tools` are asked on the runner, and this step runs in a job container, where `{tool}` may differ"));
                }
                let text = probe(tool, &super::leg_path(st, env.get("PATH").map(String::as_str)), wd).await?;
                let id = memo::identity(*precision, &text).ok_or_else(|| format!("`{tool}` names no version (it said `{}`)", text.trim()))?;
                platform.toolchains.insert(tool.clone(), id);
            }
            let facts = Facts { definition, env: all, declared, platform, inputs, upstream, trusted: a.trusted };
            return Ok(memo::portable_unit(&facts, m, &st.memo.earlier));
        }
        let facts = Facts { definition, env: all, declared, platform, inputs, upstream, trusted: a.trusted };
        Ok(memo::unit(&facts))
    }

    /// The step's inputs (W24-R01): its declared paths' git trees at the workspace's `HEAD`, and what
    /// git sees differ from them in the workspace (what `.gitignore` leaves out it doesn't), memo
    /// steps' outputs (its own and earlier ones') left out as their digests stand for them; with
    /// none declared, the whole tree.
    async fn memo_inputs(&self, st: &State, declared: &[String], own: &[String]) -> Result<BTreeMap<String, String>, String> {
        let ws = st.layout.workspace.clone();
        let not_git = "the workspace isn't a git checkout, so its inputs can't be keyed";
        let tree = git_out(&ws, &["rev-parse", "--verify", "--quiet", "HEAD^{tree}"]).await.ok_or(not_git)?;
        let tree = String::from_utf8_lossy(&tree).trim().to_owned();
        let listing = if declared.is_empty() { Vec::new() } else { git_out(&ws, &["ls-tree", "-r", "--full-tree", "HEAD"]).await.ok_or(not_git)? };
        let status = git_out(&ws, &["status", "--porcelain=v1", "-z", "--untracked-files=all", "--no-renames"]).await.ok_or(not_git)?;
        let outputs: Vec<String> = st.memo.outputs.iter().chain(own.iter()).cloned().collect();
        let paths: Vec<String> = status
            .split(|b| *b == 0)
            .filter_map(|e| e.get(3..))
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .filter(|p| !outputs.iter().any(|o| memo::under(p, o)))
            .collect();
        let changed = crate::rt::blocking(move || {
            paths
                .into_iter()
                .map(|p| {
                    let full = ws.join(&p);
                    let d = match std::fs::symlink_metadata(&full) {
                        Ok(meta) if meta.file_type().is_symlink() => std::fs::read_link(&full).map(|t| format!("link {}", t.to_string_lossy())).unwrap_or_else(|_| "-".into()),
                        Ok(meta) if meta.is_file() => crate::sync::blobs::hash_file(&full).unwrap_or_else(|_| "-".into()),
                        _ => "-".into(),
                    };
                    (p, d)
                })
                .collect::<BTreeMap<String, String>>()
        })
        .await
        .map_err(|e| format!("its inputs: {e:#}"))?;
        memo::inputs(declared, &tree, &String::from_utf8_lossy(&listing), &changed)
    }

    /// A hit, replayed (W24-R07): what it left for the steps after it, its files put in place once
    /// one runs; and where it came from, said in its log and as a notice. Its outputs.
    pub(super) async fn memo_replay(&self, st: &mut State, idx: u32, hit: Hit) -> BTreeMap<String, String> {
        let Hit { key, weak, replay, memo: m, source } = hit;
        for (k, v) in &replay.env {
            st.env.insert(k.clone(), v.clone());
        }
        for p in &replay.path {
            st.path.add(p);
        }
        st.run_env.extend(replay.run_env.iter().cloned());
        st.step_json = replay.json.iter().filter_map(|(k, t)| serde_json::from_str(t).ok().map(|v| (k.clone(), v))).collect();
        let text = |k: &str| source.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
        let from = format!("run #{} ({})", source.get("number").and_then(Value::as_i64).unwrap_or_default(), text("run"));
        if let Some(tree) = &replay.files {
            st.memo.pending.push_back(Pending { tree: tree.clone(), from: from.clone() });
        }
        st.memo.upstream.insert(format!("step.{idx}"), replay.digest());
        st.memo.earlier.push(memo::Earlier { key: weak, outputs: m.outputs.clone(), digest: replay.digest() });
        st.memo.outputs.extend(m.outputs.iter().cloned());
        let runner = Some(text("runner")).filter(|r| !r.is_empty()).map(|r| format!(" on {r}")).unwrap_or_default();
        let took = source.get("ms").and_then(Value::as_u64).map(|ms| format!(", in {:.1} s", ms as f64 / 1000.0)).unwrap_or_default();
        let files = if replay.files.is_some() { "; its files are put in place when a later step runs" } else { "" };
        let why = format!("Reused from {from}: this step succeeded there with the same inputs{runner}{took} (key {}), so it isn't run again: its log above is that run's{files}.", short(&key));
        self.note(idx, &format!("memo: {why}")).await;
        self.annotate(idx, "notice", &why).await;
        replay.outputs
    }

    /// After a memo step ran (W24-R07): a success kept (its files first), anything else not, and
    /// what it left as the keys of the steps after it take it. Only where its digest stands for
    /// its files (it took them in) do the keys after it leave those files out of their inputs; else
    /// they read them as any other.
    pub(super) async fn memo_record(&self, st: &mut State, idx: u32, mut k: Keyed, outcome: Conclusion, outputs: &BTreeMap<String, String>) {
        let (lease, renewal) = (k.lease.clone(), k.renewal.take());
        self.memo_keep(st, idx, k, outcome, outputs).await;
        drop(renewal);
        // Kept, the server let the lease go with it; anything else lets it go here (W28-R73).
        if let Some(l) = lease {
            self.release(&l).await;
        }
    }

    async fn memo_keep(&self, st: &mut State, idx: u32, k: Keyed, outcome: Conclusion, outputs: &BTreeMap<String, String>) {
        let at = format!("step.{idx}");
        if outcome != Conclusion::Success {
            st.memo.upstream.insert(at, format!("{} (not kept)", super::word(outcome)));
            self.note(idx, &format!("memo: not kept for reuse: the step's outcome is {}, and only a success is kept (docs/ci.md W24-R07)", super::word(outcome))).await;
            return;
        }
        let mut replay = Replay {
            outputs: outputs.clone(),
            json: st.step_json.iter().map(|(n, v)| (n.clone(), v.to_string())).collect(),
            env: st.env.iter().filter(|(n, v)| k.env.get(*n) != Some(*v)).map(|(n, v)| (n.clone(), v.clone())).collect(),
            path: st.path.entries().iter().filter(|p| !k.path.contains(*p)).cloned().collect(),
            run_env: st.run_env.get(k.run_env..).map(<[(String, String)]>::to_vec).unwrap_or_default(),
            files: None,
        };
        // A secret never goes into the ledger.
        if st.masks_added.len() > k.masks || replay.encode().values().any(|v| self.memo_secret_in(st, v)) {
            st.memo.upstream.insert(at, "secret (not kept)".into());
            self.note(idx, "memo: not kept for reuse: what the step left holds a secret (W24-R05)").await;
            return;
        }
        let mut stored = true;
        if !k.memo.outputs.is_empty() {
            let (ws, given) = (st.layout.workspace.clone(), k.memo.outputs.clone());
            let walked = crate::rt::blocking(move || walk(&ws, &given)).await.and_then(|r| r);
            let made = walked.and_then(|(list, files)| {
                let tree = memo::tree(&list).map_err(|e| anyhow::anyhow!(e))?;
                Ok((serde_json::to_vec(&tree)?, list.len(), files))
            });
            match made {
                Ok((_, 0, _)) => {}
                Ok((bytes, n, _)) if bytes.len() as u64 > MAX_TREE => {
                    stored = false;
                    replay.files = Some(Digest::of(&bytes));
                    self.note(idx, &format!("memo: not kept for reuse: its outputs' {n} entries make a tree of {} MiB, more than the {} MiB one holds", bytes.len() >> 20, MAX_TREE >> 20)).await;
                }
                Ok((bytes, n, files)) => {
                    let digest = Digest::of(&bytes);
                    replay.files = Some(digest.clone());
                    match self.memo_upload(bytes, &digest, &files).await {
                        Ok(sent) => self.note(idx, &format!("memo: its outputs kept as a tree of {n} entries ({sent} objects sent, the rest already kept)")).await,
                        Err(e) => {
                            stored = false;
                            self.note(idx, &format!("memo: not kept for reuse: its outputs couldn't be stored: {e:#}")).await;
                        }
                    }
                }
                Err(e) => {
                    st.memo.upstream.insert(at, "unread outputs (not kept)".into());
                    self.note(idx, &format!("memo: not kept for reuse: its outputs couldn't be read: {e:#}")).await;
                    return;
                }
            }
        }
        st.memo.upstream.insert(at, replay.digest());
        st.memo.earlier.push(memo::Earlier { key: key::weak(&k.unit).hex(), outputs: k.memo.outputs.clone(), digest: replay.digest() });
        st.memo.outputs.extend(k.memo.outputs.iter().cloned());
        if !stored {
            return;
        }
        let ms = u64::try_from(k.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let body = json!({ "unit": k.unit, "step": idx, "ms": ms, "max_age": k.memo.max_age, "outputs": replay.encode(), "lease": k.lease });
        match self.work_post("record", &body, Duration::from_secs(120)).await {
            Ok((201, _)) => self.note(idx, &format!("memo: kept for reuse (key {})", short(&key::weak(&k.unit).hex()))).await,
            Ok((status, v)) => self.note(idx, &format!("memo: not kept for reuse: the server said HTTP {status}: {}", said(&v))).await,
            Err(e) => self.note(idx, &format!("memo: not kept for reuse: the work ledger couldn't be reached ({e:#})")).await,
        }
    }

    /// The tree's files the server lacks, sent a few at a time, then the tree itself: how many.
    async fn memo_upload(&self, tree: Vec<u8>, digest: &Digest, files: &BTreeMap<String, PathBuf>) -> anyhow::Result<usize> {
        let asked: Vec<&String> = files.keys().chain(std::iter::once(&digest.hash)).collect();
        let mut missing: BTreeSet<String> = BTreeSet::new();
        for some in asked.chunks(ASK_AT_ONCE) {
            let (status, v) = self.work_post("missing", &json!({ "digests": some }), Duration::from_secs(120)).await?;
            anyhow::ensure!(status == 200, "HTTP {status}: {}", said(&v));
            missing.extend(v.get("missing").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned));
        }
        let todo: Vec<(&String, &PathBuf)> = files.iter().filter(|(d, _)| missing.contains(*d)).collect();
        for batch in todo.chunks(AT_ONCE) {
            let mut calls: Vec<Call<'_>> = Vec::with_capacity(batch.len());
            for (d, p) in batch {
                calls.push(Box::pin(self.memo_put(d, Body::File(p.to_path_buf()))));
            }
            together(calls).await?;
        }
        let mut n = todo.len();
        if missing.contains(&digest.hash) {
            self.memo_put(&digest.hash, Body::Bytes(tree)).await?;
            n += 1;
        }
        // Kept in this runner's content store too (W24-R08), so a hit here puts them in place
        // without asking the server, and its LAN peers may ask for them (W24-R27).
        let paths: Vec<PathBuf> = files.values().cloned().collect();
        self.local_keep(paths).await;
        Ok(n)
    }

    /// This runner's content store (W24-R08's cache service), and the scope this leg holds objects
    /// in there: its repository and domain.
    async fn local_store(&self) -> Option<(std::sync::Arc<crate::runner::work::store::Store>, crate::runner::work::store::Scope)> {
        let svc = crate::runner::work::service::ensure().await?;
        Some((std::sync::Arc::clone(svc.store()), crate::runner::work::store::Scope::new(&self.a.repo, if self.a.trusted { "trusted" } else { "untrusted" })))
    }

    /// Files put in the runner's content store, held for the leg's scope; what can't be is said.
    async fn local_keep(&self, paths: Vec<PathBuf>) {
        let Some((store, scope)) = self.local_store().await else { return };
        let kept = crate::rt::blocking(move || -> anyhow::Result<()> {
            for p in paths {
                let (digest, _) = store.put_file(&p)?;
                store.hold(&scope, &digest)?;
            }
            Ok(())
        })
        .await
        .and_then(|r| r);
        if let Err(e) = kept {
            tracing::debug!(error = %format!("{e:#}"), "a memo step's files weren't kept in the runner's content store");
        }
    }

    async fn memo_put(&self, digest: &str, body: Body) -> anyhow::Result<()> {
        let req = self.http.put(&self.work_url(&format!("blobs/{digest}"))).bearer(&self.a.runtime.runtime_token).header("content-type", "application/octet-stream").timeout(Duration::from_secs(1800));
        let req = match body {
            Body::Bytes(b) => req.body(b),
            Body::File(p) => req.body_stream(crate::rt::file_source(p)?),
        };
        let resp = req.send().await?;
        let status = resp.status();
        anyhow::ensure!((200..300).contains(&status), "{digest}: HTTP {status}: {}", said(&resp.json(1 << 20).await.unwrap_or_default()));
        Ok(())
    }

    /// What earlier hits left (their files' trees), put in place in the workspace before a step
    /// runs: only what isn't there as the tree has it is fetched (W24-R07).
    pub(super) async fn memo_materialize(&self, st: &mut State, idx: u32) -> Result<(), String> {
        while let Some(p) = st.memo.pending.front().cloned() {
            let n = self.memo_put_in_place(&st.layout.workspace, &p).await.map_err(|e| format!("memo: the files reused from {} couldn't be put in place: {e:#}", p.from))?;
            st.memo.pending.pop_front();
            self.note(idx, &format!("memo: the files reused from {} put in place first ({n} fetched)", p.from)).await;
        }
        Ok(())
    }

    async fn memo_put_in_place(&self, ws: &Path, p: &Pending) -> anyhow::Result<usize> {
        let resp = self.http.get(&self.work_url(&format!("blobs/{}", p.tree.hash))).bearer(&self.a.runtime.runtime_token).timeout(Duration::from_secs(120)).send().await?;
        anyhow::ensure!(resp.is_success(), "its tree: HTTP {}", resp.status());
        let bytes = resp.bytes(usize::try_from(MAX_TREE).unwrap_or(usize::MAX)).await?;
        anyhow::ensure!(Digest::of(&bytes) == p.tree, "its tree arrived corrupted");
        let tree: Tree = serde_json::from_slice(&bytes)?;
        let list = memo::entries(&tree).map_err(|e| anyhow::anyhow!(e))?;
        let (at, l) = (ws.to_path_buf(), list.clone());
        let need = crate::rt::blocking(move || plan(&at, &l)).await??;
        for batch in need.chunks(AT_ONCE) {
            let mut calls: Vec<Call<'_>> = Vec::with_capacity(batch.len());
            for (to, digest, executable) in batch {
                calls.push(Box::pin(self.memo_fetch(digest, to, *executable)));
            }
            together(calls).await?;
        }
        let at = ws.to_path_buf();
        crate::rt::blocking(move || links(&at, &list)).await??;
        Ok(need.len())
    }

    /// One file of a tree, fetched beside where it goes, checked, then put there; nothing is left
    /// behind when it can't be.
    async fn memo_fetch(&self, d: &Digest, to: &Path, executable: bool) -> anyhow::Result<()> {
        let name = to.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let part = to.with_file_name(format!(".{name}.kiln-memo-{}", crate::util::new_id()));
        let placed = match self.memo_download(d, &part).await {
            Ok(wrote) => {
                let (p, t, want) = (part.clone(), to.to_path_buf(), d.clone());
                crate::rt::blocking(move || -> anyhow::Result<()> {
                    anyhow::ensure!(wrote == want.size_bytes && crate::sync::blobs::hash_file(&p)? == want.hash, "it arrived corrupted");
                    if t.symlink_metadata().is_ok_and(|m| m.is_dir()) {
                        std::fs::remove_dir_all(&t)?;
                    }
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt as _;
                        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }))?;
                    }
                    #[cfg(not(unix))]
                    let _ = executable;
                    std::fs::rename(&p, &t)?;
                    Ok(())
                })
                .await
                .and_then(|r| r)
            }
            Err(e) => Err(e),
        };
        if placed.is_err() {
            let p = part.clone();
            let _ = crate::rt::blocking(move || std::fs::remove_file(p)).await;
        }
        placed.map_err(|e| e.context(format!("`{}`", to.display())))
    }

    /// A stored object's bytes, into `part`: how many. A small one comes whole and is written
    /// without a sync (the workspace goes with the leg anyway); a large one is written as it comes.
    async fn memo_download(&self, d: &Digest, part: &Path) -> anyhow::Result<u64> {
        // The runner's content store first (W24-R08): an object this runner kept or fetched before.
        if let Some((store, _)) = self.local_store().await {
            let (hash, p) = (d.hash.clone(), part.to_path_buf());
            if crate::rt::blocking(move || store.copy_to(&hash, &p)).await.and_then(|r| r).unwrap_or(false) {
                return Ok(d.size_bytes);
            }
        }
        let wrote = self.memo_download_from_server(d, part).await?;
        self.local_keep(vec![part.to_path_buf()]).await;
        Ok(wrote)
    }

    async fn memo_download_from_server(&self, d: &Digest, part: &Path) -> anyhow::Result<u64> {
        let mut resp = self.http.get(&self.work_url(&format!("blobs/{}", d.hash))).bearer(&self.a.runtime.runtime_token).timeout(Duration::from_secs(600)).send().await?;
        anyhow::ensure!(resp.is_success(), "HTTP {}", resp.status());
        if d.size_bytes <= WHOLE {
            let bytes = resp.bytes(usize::try_from(d.size_bytes).unwrap_or(usize::MAX)).await?;
            let p = part.to_path_buf();
            return Ok(crate::rt::blocking(move || std::fs::write(&p, &bytes).map(|()| bytes.len() as u64)).await??);
        }
        let (tx, done) = crate::rt::file_sink(part.to_path_buf(), d.size_bytes)?;
        while let Some(c) = resp.chunk().await? {
            if tx.send(c).await.is_err() {
                break;
            }
        }
        drop(tx);
        done.await
    }
}

/// What a step's `memo.outputs` hold: entries from the workspace, and each file's path by its
/// digest. Links are kept as links; what isn't there is left out.
fn walk(ws: &Path, given: &[String]) -> anyhow::Result<(Vec<Entry>, BTreeMap<String, PathBuf>)> {
    let (mut entries, mut files, mut total) = (Vec::new(), BTreeMap::new(), 0u64);
    let mut stack: Vec<(String, PathBuf)> = given.iter().map(|g| (g.clone(), ws.join(g))).collect();
    while let Some((rel, full)) = stack.pop() {
        let Ok(meta) = std::fs::symlink_metadata(&full) else { continue };
        let kind = meta.file_type();
        if kind.is_symlink() {
            entries.push(Entry::Link { path: rel, target: std::fs::read_link(&full)?.to_string_lossy().into_owned() });
        } else if kind.is_dir() {
            for e in std::fs::read_dir(&full)? {
                let e = e?;
                stack.push((format!("{rel}/{}", e.file_name().to_string_lossy()), e.path()));
            }
            entries.push(Entry::Dir { path: rel });
        } else if kind.is_file() {
            let digest = Digest { hash: crate::sync::blobs::hash_file(&full)?, size_bytes: meta.len() };
            total = total.saturating_add(digest.size_bytes);
            anyhow::ensure!(total <= MAX_BYTES, "they hold more than {} GiB", MAX_BYTES >> 30);
            #[cfg(unix)]
            let executable = std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111 != 0;
            #[cfg(not(unix))]
            let executable = false;
            files.entry(digest.hash.clone()).or_insert(full);
            entries.push(Entry::File { path: rel, digest, executable });
        }
        anyhow::ensure!(entries.len() <= memo::MAX_ENTRIES, "they hold more than {} entries", memo::MAX_ENTRIES);
    }
    Ok((entries, files))
}

/// A tree's directories made in the workspace (a link where one goes replaced, never followed), and
/// the files it holds that aren't there as it has them: where each goes, its digest, and whether
/// it's executable. Nothing lands outside the workspace.
fn plan(ws: &Path, entries: &[Entry]) -> anyhow::Result<Vec<(PathBuf, Digest, bool)>> {
    std::fs::create_dir_all(ws)?;
    let root = ws.canonicalize()?;
    let inside = |p: &Path| p.parent().is_some_and(|d| super::stays_inside(&root, d));
    let mut need = Vec::new();
    for e in entries {
        let to = ws.join(e.path());
        anyhow::ensure!(inside(&to), "`{}` would land outside the workspace", e.path());
        match e {
            Entry::Dir { .. } => {
                if to.symlink_metadata().is_ok_and(|m| !m.is_dir()) {
                    std::fs::remove_file(&to)?;
                }
                std::fs::create_dir_all(&to)?;
            }
            Entry::File { digest, executable, .. } => {
                let there = to.symlink_metadata().is_ok_and(|m| m.is_file() && m.len() == digest.size_bytes) && crate::sync::blobs::hash_file(&to).is_ok_and(|h| h == digest.hash);
                if !there {
                    need.push((to, digest.clone(), *executable));
                }
            }
            Entry::Link { .. } => {}
        }
    }
    Ok(need)
}

/// A tree's links made in the workspace, once its files are there.
fn links(ws: &Path, entries: &[Entry]) -> anyhow::Result<()> {
    let root = ws.canonicalize()?;
    for e in entries {
        let Entry::Link { path, target } = e else { continue };
        let to = ws.join(path);
        anyhow::ensure!(to.parent().is_some_and(|d| super::stays_inside(&root, d)), "`{path}` would land outside the workspace");
        if std::fs::read_link(&to).is_ok_and(|t| t == Path::new(target)) {
            continue;
        }
        match to.symlink_metadata() {
            Ok(m) if m.is_dir() => std::fs::remove_dir_all(&to)?,
            Ok(_) => std::fs::remove_file(&to)?,
            Err(_) => {}
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, &to)?;
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::exec::{Up, Uplink};
    use crate::proto::{AgentMsg, JobResult};
    use crate::rt::Shutdown;
    use kiln_ci::wire::{AssignmentV2, JobEventV2};
    use std::sync::Arc;

    /// What a leg sent: its logs by step and its events.
    struct Sent {
        result: JobResult,
        logs: BTreeMap<u32, String>,
        events: Vec<JobEventV2>,
    }

    impl Sent {
        fn finished(&self, idx: u32) -> Option<(Conclusion, Option<i32>, BTreeMap<String, String>)> {
            self.events.iter().find_map(|e| match e {
                JobEventV2::StepFinished { idx: i, outcome, exit_code, outputs, .. } if *i == idx => Some((*outcome, *exit_code, outputs.clone())),
                _ => None,
            })
        }

        fn log(&self, idx: u32) -> &str {
            self.logs.get(&idx).map_or("", String::as_str)
        }

        fn notices(&self, idx: u32) -> Vec<&str> {
            self.events.iter().filter_map(|e| match e {
                JobEventV2::Annotation { idx: i, level, message, .. } if *i == idx && level == "notice" => Some(message.as_str()),
                _ => None,
            }).collect()
        }
    }

    fn git(dir: &Path, args: &[&str]) -> anyhow::Result<()> {
        let o = std::process::Command::new("git").args(["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "init.defaultBranch=main", "-c", "commit.gpgsign=false"]).args(args).current_dir(dir).env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null").output()?;
        anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        Ok(())
    }

    /// The first job of `workflow` (the kiln dialect), run as `leg` of `run` against the server at
    /// `base`, in a workspace holding `files` as one commit; what it sent goes into `hub`'s logs, as
    /// a server keeps them.
    async fn run(hub: &crate::hub::Hub, base: &str, token: &str, (run, leg): (&str, &str), work: &Path, workflow: &str, files: &[(&str, &str)]) -> anyhow::Result<Sent> {
        let (w, d) = kiln_ci::wf::parse_workflow(".kiln/ci.yaml", workflow, kiln_ci::expr::Dialect::Kiln);
        anyhow::ensure!(d.iter().all(|x| !x.is_error()), "{d:?}");
        let job = w.and_then(|w| w.jobs.into_iter().next()).ok_or_else(|| anyhow::anyhow!("no job"))?;
        let ws = super::super::Layout::new(work, leg, "acme/app", None).workspace;
        std::fs::create_dir_all(&ws)?;
        for (p, text) in files {
            let full = ws.join(p);
            if let Some(dir) = full.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(full, text)?;
        }
        git(&ws, &["init", "-q"])?;
        git(&ws, &["add", "."])?;
        git(&ws, &["commit", "-qm", "files"])?;
        let a = AssignmentV2 {
            job_id: leg.into(),
            run_id: run.into(),
            repo: "acme/app".into(),
            dialect: "kiln".into(),
            steps: job.steps.clone(),
            job_env: job.env.clone(),
            contexts: kiln_ci::wire::Contexts { github: json!({ "event_name": "push", "ref": "refs/heads/main", "sha": "abc123", "repository": "acme/app", "event": {} }), ..Default::default() },
            runtime: kiln_ci::wire::Runtime { results_url: base.into(), runtime_token: token.into(), ..Default::default() },
            timeout_minutes: 5,
            trusted: true,
            ..AssignmentV2::default()
        };
        let (tx, rx) = async_channel::unbounded();
        let l = Leg { a, work_dir: work.to_path_buf(), runner_name: "runner-a".into(), up: Uplink(tx), cancel: Shutdown::new(), http: Arc::new(crate::http::client::Client::public()?), warm: Default::default(), cache_max: None };
        let result = l.run().await;
        let (mut logs, mut events) = (BTreeMap::<u32, String>::new(), Vec::new());
        while let Ok(m) = rx.try_recv() {
            match m {
                Up::Log(c) => {
                    hub.logs.append(&c.job, c.idx, &c.data);
                    logs.entry(c.idx).or_default().push_str(&String::from_utf8_lossy(&c.data));
                }
                Up::Msg(AgentMsg::JobEvent { json, .. }) => events.push(serde_json::from_str(&json)?),
                _ => {}
            }
        }
        Ok(Sent { result, logs, events })
    }

    const WORKFLOW: &str = r#"on: push
jobs:
  build:
    runs-on: local
    steps:
      - id: build
        run: |
          mkdir -p out
          echo "built $(cat src/version) as $RANDOM$RANDOM$RANDOM" | tee out/bin
          chmod +x out/bin
          echo "made=yes" >> "$GITHUB_OUTPUT"
          echo "MODE=built" >> "$GITHUB_ENV"
        memo: { inputs: [src], outputs: [out] }
      - run: echo "tested $(cat out/bin) in $MODE with ${{ steps.build.outputs.made }}"; test -x out/bin
      - run: echo "checked as $RANDOM$RANDOM$RANDOM"; test -f ok
        memo: check
        continue-on-error: true
"#;

    /// W24-R07, the runner and the server together: a memo step's success is kept with its files;
    /// the next leg on the same inputs doesn't run it (its log is the first's, its outputs and the
    /// variable it set are replayed, it says where it came from), and the step after it, which
    /// runs, finds the files it left; a failed memo step is never kept, so it runs again.
    #[test]
    fn w24_r07_a_hit_replays_the_step_without_running_it_and_failures_are_never_reused() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let first_token = crate::ci::services::memo::tests::leg(&hub, "acme/app", "run1", "leg1", true).await?;
            let second_token = crate::ci::services::memo::tests::leg(&hub, "acme/app", "run2", "leg2", true).await?;
            let base = crate::ci::services::memo::tests::serve(Arc::clone(&hub))?;
            let work = tempfile::tempdir()?;
            let files = [("src/version", "1\n"), ("README.md", "memo\n")];
            let first = run(&hub, &base, &first_token, ("run1", "leg1"), work.path(), WORKFLOW, &files).await?;
            anyhow::ensure!(first.result.status == crate::store::Status::Succeeded, "{:?}\n{:?}", first.result, first.logs);
            let built = first.log(0).lines().find(|l| l.starts_with("built 1 as ")).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("it built: {:?}", first.logs))?;
            anyhow::ensure!(first.log(0).contains("memo: kept for reuse"), "{}", first.log(0));
            anyhow::ensure!(first.log(1).contains(&format!("tested {built} in built with yes")), "{}", first.log(1));
            anyhow::ensure!(first.log(2).contains("not kept for reuse") && first.finished(2).is_some_and(|f| f.0 == Conclusion::Failure), "{}", first.log(2));
            let checked = first.log(2).lines().find(|l| l.starts_with("checked as ")).map(str::to_owned).unwrap_or_default();

            let second = run(&hub, &base, &second_token, ("run2", "leg2"), work.path(), WORKFLOW, &files).await?;
            anyhow::ensure!(second.result.status == crate::store::Status::Succeeded, "{:?}\n{:?}", second.result, second.logs);
            // The memo step wasn't run: nothing it printed came from this leg, and it says where
            // its result came from; its log is the first leg's, its outputs the first's.
            anyhow::ensure!(!second.log(0).contains("built 1 as"), "it ran: {}", second.log(0));
            anyhow::ensure!(second.log(0).contains("Reused from run #1 (run1)"), "{}", second.log(0));
            anyhow::ensure!(second.notices(0).iter().any(|n| n.contains("run #1 (run1)")), "{:?}", second.events);
            let copied = String::from_utf8_lossy(&hub.logs.read("leg2", 0).await).into_owned();
            anyhow::ensure!(copied.contains(&built), "the source's log: {copied}");
            let (outcome, exit, outputs) = second.finished(0).ok_or_else(|| anyhow::anyhow!("no end"))?;
            anyhow::ensure!(outcome == Conclusion::Success && exit == Some(0) && outputs.get("made").map(String::as_str) == Some("yes"), "{outcome:?} {exit:?} {outputs:?}");
            // The step after it ran, on the files the first leg's build left, its variable set.
            anyhow::ensure!(second.log(1).contains("put in place first") && second.log(1).contains(&format!("tested {built} in built with yes")), "{}", second.log(1));
            // The failed memo step was never kept: it ran again.
            let again = second.log(2).lines().find(|l| l.starts_with("checked as ")).map(str::to_owned).unwrap_or_default();
            anyhow::ensure!(!again.is_empty() && again != checked && !second.log(2).contains("Reused"), "{}", second.log(2));
            Ok(())
        })
    }
}

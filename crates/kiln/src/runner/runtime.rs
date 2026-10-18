//! The step runtime for `AssignV2` (docs/ci.md W06): runs one leg's steps the way GitHub's runner
//! does. The workspace layout, `GITHUB_*` variables and contexts, each step evaluated in GitHub's
//! order, GitHub's shells, workflow commands and environment files between steps, masking before
//! anything leaves the machine, step and job outputs, timeouts and cancellation. What it decides
//! comes from `kiln_ci` (expressions, commands, environment files, masking); this module runs the
//! processes and talks to the server.
//!
//! `uses:` steps are W07's; until they land, such a step fails saying so.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use futures::FutureExt as _;
use kiln_ci::expr::{self, Budget, Condition, Contexts, Dialect, ExprError, StatusView, Template, Value};
use kiln_ci::runner::commands::{self, Line, Processor};
use kiln_ci::runner::envfiles::{self, PrependPath};
use kiln_ci::runner::mask::Masker;
use kiln_ci::runner::matchers;
use kiln_ci::wf::ir::{Step, Tpl, YVal};
use kiln_ci::wf::validate::{Site, options};
use kiln_ci::wire::{AssignmentV2, Conclusion, JobEventV2};

use crate::exec::Uplink;
use crate::proto::{AgentMsg, JobResult, LOG_CHUNK};
use crate::rt::{Interval, Shutdown};
use crate::store::Status;

/// One leg, run on this machine.
#[derive(Clone)]
pub struct Leg {
    pub a: AssignmentV2,
    pub work_dir: PathBuf,
    pub runner_name: String,
    pub up: Uplink,
    pub cancel: Shutdown,
    /// For actions and Node runtimes.
    pub http: std::sync::Arc<crate::http::client::Client>,
}

/// What a step runs: a script under a shell, or a program (an action's `node main.js`).
enum Run<'a> {
    Script { script: &'a str, shell: Option<&'a str> },
    Program { prog: String, args: Vec<String> },
    /// A Docker action's container (W07-R06): `docker run` with GitHub's mounts.
    Container { cli: super::docker::Cli, image: String, entrypoint: Option<String>, args: Vec<String> },
}

/// What a process left: how it ended, its outputs and its saved state (`$GITHUB_STATE`).
struct Ran {
    end: End,
    outputs: BTreeMap<String, String>,
    state: Vec<(String, String)>,
}

/// A secret or variable `name` set to `true` (W06-R13: `ACTIONS_STEP_DEBUG`, `ACTIONS_RUNNER_DEBUG`).
fn debug_flag(a: &AssignmentV2, name: &str) -> bool {
    let truthy = |v: &str| v.eq_ignore_ascii_case("true");
    a.secrets.iter().any(|s| s.name == name && truthy(&s.value)) || a.contexts.vars.get(name).and_then(serde_json::Value::as_str).is_some_and(truthy)
}

/// Debug logging (`RUNNER_DEBUG=1`, `runner.debug`, what `core.isDebug()` reads): a debug rerun,
/// or `ACTIONS_STEP_DEBUG` or `ACTIONS_RUNNER_DEBUG` set.
fn runner_debug(a: &AssignmentV2) -> bool {
    a.debug.runner_debug || a.debug.step_debug || debug_flag(a, "ACTIONS_RUNNER_DEBUG") || debug_flag(a, "ACTIONS_STEP_DEBUG")
}

/// The step outputs (step id, name) the job's secret outputs read (W04-R22).
fn secret_feeds(a: &AssignmentV2, dialect: Dialect) -> std::collections::BTreeSet<(String, String)> {
    let mut out = std::collections::BTreeSet::new();
    for (_, t) in a.outputs.iter().filter(|(k, _)| a.secret_outputs.contains(k)) {
        let Ok(tpl) = Template::parse_with(&t.src, &options(Site::JobOutputs, dialect)) else { continue };
        for r in tpl.references() {
            if r.root() == "steps" && r.name(2) == Some("outputs")
                && let (Some(step), Some(name)) = (r.name(1), r.name(3))
            {
                out.insert((step.to_owned(), name.to_owned()));
            }
        }
    }
    out
}

/// Whether a program is on `PATH`.
fn which(prog: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(prog).is_file() || d.join(format!("{prog}.exe")).is_file()))
}

/// GitHub's view of a typed output (W04-R20): its JSON, but a string as itself.
fn json_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// An action's post step, run after the main steps in reverse order (W06-R11).
#[derive(Clone)]
struct Post {
    name: String,
    node: PathBuf,
    script: PathBuf,
    env: BTreeMap<String, String>,
    cond: Option<Tpl>,
    wd: PathBuf,
    uses: String,
}

/// Where an action is, once fetched.
struct Located {
    dir: PathBuf,
    repo: String,
    git_ref: String,
}

/// GitHub's cancel sequence (W05-R18).
const SIGINT_GRACE: Duration = Duration::from_millis(7_500);
const SIGTERM_GRACE: Duration = Duration::from_millis(2_500);

/// Background steps running at once in a leg (W06-R17); more wait their turn.
const MAX_BACKGROUND: usize = 10;
/// `cancel:` of a background step: SIGTERM, then SIGKILL after this.
const BACKGROUND_GRACE: Duration = Duration::from_secs(10);
/// How long post steps have once the job's steps are done, a cancelled or timed-out job's
/// included (GitHub's `cancel-timeout-minutes`).
const CLEANUP_TIME: Duration = Duration::from_secs(5 * 60);

/// The paths of W06-R01.
#[derive(Clone)]
struct Layout {
    workspace: PathBuf,
    runner_workspace: PathBuf,
    temp: PathBuf,
    tool_cache: PathBuf,
    commands: PathBuf,
}

impl Layout {
    /// `hosted` is the `docker` executor's image: its tools are kept apart from the host's, as
    /// they are built for another system.
    fn new(work: &Path, leg: &str, repo: &str, hosted: Option<&str>) -> Layout {
        let digest = crate::util::sha256_hex(leg.as_bytes());
        let tools = work.join("_tool");
        let runner_workspace = work.join("w").join(digest.get(..8).unwrap_or("leg"));
        let name = repo.rsplit('/').next().unwrap_or("repo");
        let temp = runner_workspace.join("_temp");
        Layout {
            workspace: runner_workspace.join(name),
            commands: temp.join("_runner_file_commands"),
            temp,
            tool_cache: match hosted {
                Some(image) => tools.join(image.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' { c } else { '-' }).collect::<String>()),
                None => tools,
            },
            runner_workspace,
        }
    }
}

/// `p` with `.` and `..` resolved by name, as .NET's `GetFullPath` does.
fn lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// How a step ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    Exit(Option<i32>),
    Cancelled,
    TimedOut,
}

/// A finished step, as the `steps` context holds it.
#[derive(Debug, Clone, Default)]
struct Done {
    id: Option<String>,
    outputs: BTreeMap<String, String>,
    /// Typed outputs (W04-R20): what the kiln dialect sees for these names.
    json: BTreeMap<String, serde_json::Value>,
    outcome: Option<Conclusion>,
    conclusion: Option<Conclusion>,
}

fn word(c: Conclusion) -> &'static str {
    match c {
        Conclusion::Success => "success",
        Conclusion::Failure => "failure",
        Conclusion::Cancelled => "cancelled",
        Conclusion::Skipped => "skipped",
    }
}

fn runner_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macOS",
        "windows" => "Windows",
        _ => "Linux",
    }
}

fn runner_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "ARM64",
        "arm" => "ARM",
        "x86" => "X86",
        _ => "X64",
    }
}

/// The contexts of one evaluation on the runner (W06-R03).
struct Cx<'a> {
    values: Vec<(&'static str, Value)>,
    status: StatusView,
    workspace: &'a Path,
}

impl Contexts for Cx<'_> {
    fn get(&self, name: &str) -> Option<Value> {
        self.values.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone())
    }

    fn status(&self) -> StatusView {
        self.status
    }

    fn hash_files(&self, patterns: &[String]) -> Result<String, ExprError> {
        super::hashfiles::Workspace::new(self.workspace)?.hash(patterns)
    }
}

fn str_map(m: &BTreeMap<String, String>) -> Value {
    Value::object(m.iter().map(|(k, v)| (k.clone(), Value::str(v))))
}

/// The leg's state as its steps change it.
#[derive(Clone)]
struct State {
    /// The next step's title starts with this (a step rerun in place, W17-R04).
    rerun_label: Option<String>,
    /// Post steps run: as on GitHub, a cancelled job's cleanup runs to its end, on time of its own.
    cleanup: bool,
    dialect: Dialect,
    layout: Layout,
    /// The job's environment: the workflow's and the job's `env`, `env-from`, `$GITHUB_ENV`.
    env: BTreeMap<String, String>,
    path: PrependPath,
    steps: Vec<Done>,
    failed: bool,
    cancelled: bool,
    masker: Masker,
    secrets: BTreeMap<String, String>,
    github: Value,
    base: Vec<(&'static str, Value)>,
    step_debug: bool,
    allow_unsecure: bool,
    /// Process groups the steps started, all ended when the leg does (GitHub keeps a step's
    /// background processes alive until the job ends).
    groups: Vec<i32>,
    posts: Vec<Post>,
    /// The index the next pre or post step gets (after the job's own steps).
    next_idx: u32,
    /// `GITHUB_SERVER_URL` as the leg reaches it, which [`NOT_GHES`] stands in for.
    server_url: String,
    /// The leg's containers (W13-R08): its network, services and job container.
    containers: Option<super::docker::Containers>,
    /// The problem matchers steps added (W06-R06).
    matchers: matchers::Job,
    /// A background step's own stop (`cancel:`, W06-R17).
    stop: Option<Shutdown>,
    /// Values `::add-mask::` added, for a background step's `wait` to carry into the leg.
    masks_added: Vec<String>,
    /// The running step's typed outputs (`$KILN_OUTPUT_JSON`, W04-R20), for its `steps` entry.
    step_json: BTreeMap<String, serde_json::Value>,
    /// `$KILN_RUN_ENV` lines (W04-R21), for `run.env`.
    run_env: Vec<(String, String)>,
    /// Step outputs (step id, name) that feed a secret job output (W04-R22): masked the moment
    /// their step ends, so they're never reported or kept in the clear.
    sensitive: std::collections::BTreeSet<(String, String)>,
}

/// A background step that finished, and what it changed: applied to the leg at its `wait`, in
/// the order the steps were declared (W06-R17).
struct BgDone {
    id: String,
    /// Where it was declared: the order its changes apply in.
    order: u32,
    env: Vec<(String, String)>,
    path: Vec<String>,
    masks: Vec<String>,
    done: Option<Done>,
    posts: Vec<Post>,
    groups: Vec<i32>,
    conclusion: Conclusion,
}

type BgFuture<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = BgDone> + Send + 'a>>;

/// A leg's background steps (W06-R17): running beside the steps that follow them, folded into
/// the leg when waited for.
struct Background<'a> {
    turns: &'a async_lock::Semaphore,
    running: futures::stream::FuturesUnordered<BgFuture<'a>>,
    /// Each started step's declaration order and stop.
    started: BTreeMap<String, (u32, Shutdown)>,
    finished: BTreeMap<String, BgDone>,
    /// Steps whose changes the leg has.
    folded: std::collections::BTreeSet<String>,
}

impl<'a> Background<'a> {
    fn new(turns: &'a async_lock::Semaphore) -> Self {
        Background { turns, running: futures::stream::FuturesUnordered::new(), started: BTreeMap::new(), finished: BTreeMap::new(), folded: std::collections::BTreeSet::new() }
    }

    /// Runs `fut` (the leg's next step) while the background steps run on.
    async fn drive<T>(&mut self, fut: impl std::future::Future<Output = T>) -> T {
        use futures::StreamExt as _;
        let fut = fut.fuse();
        futures::pin_mut!(fut);
        loop {
            if self.running.is_empty() {
                return fut.await;
            }
            futures::select_biased! {
                r = fut => return r,
                d = self.running.select_next_some() => {
                    self.finished.insert(d.id.clone(), d);
                }
            }
        }
    }

    /// Lets steps just started begin now (take their turn, start their process), instead of
    /// when the leg next waits on something: polls them until each is waiting.
    fn kick(&mut self) {
        use futures::{FutureExt as _, StreamExt as _};
        while let Some(Some(d)) = self.running.next().now_or_never() {
            self.finished.insert(d.id.clone(), d);
        }
    }

    /// Waits until each of `ids` that started has finished.
    async fn settle(&mut self, ids: &[String]) {
        use futures::StreamExt as _;
        while ids.iter().any(|id| self.started.contains_key(id) && !self.finished.contains_key(id)) {
            match self.running.next().await {
                Some(d) => {
                    self.finished.insert(d.id.clone(), d);
                }
                None => break,
            }
        }
    }

    /// The started steps not yet folded into the leg, in declaration order.
    fn unfolded(&self) -> Vec<String> {
        let mut v: Vec<(&u32, &String)> = self.started.iter().filter(|(id, _)| !self.folded.contains(*id)).map(|(id, (o, _))| (o, id)).collect();
        v.sort();
        v.into_iter().map(|(_, id)| id.clone()).collect()
    }
}

/// What reading one process's output keeps.
struct Reading {
    proc: Processor,
    /// The line so far.
    line: Vec<u8>,
    /// From the deprecated `set-output` command.
    set_output: BTreeMap<String, String>,
    matchers: matchers::Step,
    /// A command failed (an unreadable matcher file, a disabled command): the step fails, as on
    /// GitHub.
    failed: bool,
}

/// The `GITHUB_SERVER_URL` actions that refuse GitHub Enterprise Server see (docs/ci.md
/// W08-R08). The toolkit counts every server but github.com, `*.ghe.com` and `*.localhost` as
/// GHES. On GHES, `@actions/artifact` v2 and later (so `upload-artifact@v4`,
/// `download-artifact@v4` and every newer version) throw `GHESNotSupportedError`. kiln serves
/// those actions' protocol, so they run with this URL instead, and what they print or output
/// gets the real one back. Their requests go to `ACTIONS_RESULTS_URL` and `GITHUB_API_URL`,
/// never to this one.
pub const NOT_GHES: &str = "http://kiln.localhost";

/// Whether any of an action's scripts carry the toolkit's GHES refusal.
fn refuses_ghes(dir: &Path, scripts: &[&str]) -> bool {
    const NEEDLE: &[u8] = b"GHESNotSupportedError";
    scripts.iter().any(|f| std::fs::read(dir.join(f)).is_ok_and(|b| b.windows(NEEDLE.len()).any(|w| w == NEEDLE)))
}

impl State {
    /// `s` with [`NOT_GHES`] put back to the real server.
    fn unfake(&self, s: &str) -> String {
        s.replace(NOT_GHES, &self.server_url)
    }

    fn unfake_bytes(&self, b: Vec<u8>) -> Vec<u8> {
        let needle = NOT_GHES.as_bytes();
        if b.windows(needle.len()).any(|w| w == needle) { self.unfake(&String::from_utf8_lossy(&b)).into_bytes() } else { b }
    }

    fn cx(&self) -> Cx<'_> {
        let mut values = self.base.clone();
        values.push(("github", self.github.clone()));
        values.push(("env", str_map(&self.env)));
        values.push(("secrets", str_map(&self.secrets)));
        let mut job = vec![("status".to_owned(), Value::str(if self.cancelled { "cancelled" } else if self.failed { "failure" } else { "success" }))];
        if let Some(c) = &self.containers {
            let (container, services) = c.contexts();
            job.push(("container".to_owned(), Value::from_json(&container)));
            job.push(("services".to_owned(), Value::from_json(&services)));
        }
        values.push(("job", Value::object(job)));
        let steps = Value::object(self.steps.iter().filter_map(|d| {
            let id = d.id.clone()?;
            Some((
                id,
                Value::object([
                    ("outputs".to_owned(), self.step_outputs(d)),
                    ("outcome".to_owned(), d.outcome.map_or(Value::Null, |c| Value::str(word(c)))),
                    ("conclusion".to_owned(), d.conclusion.map_or(Value::Null, |c| Value::str(word(c)))),
                ]),
            ))
        }));
        values.push(("steps", steps));
        Cx { values, status: StatusView { failed: self.failed, cancelled: self.cancelled, skipped: false }, workspace: &self.layout.workspace }
    }

    /// A step's outputs as this dialect sees them: kiln's typed ones as their JSON values.
    fn step_outputs(&self, d: &Done) -> Value {
        if self.dialect != Dialect::Kiln || d.json.is_empty() {
            return str_map(&d.outputs);
        }
        Value::object(d.outputs.iter().map(|(k, v)| (k.clone(), d.json.get(k).map_or_else(|| Value::str(v), Value::from_json))))
    }

    fn render(&self, t: &Tpl, site: Site, what: &str) -> Result<String, String> {
        let tpl = Template::parse_with(&t.src, &options(site, self.dialect)).map_err(|e| format!("{what}: {}", e.message))?;
        expr::render(&tpl, &self.cx(), &mut Budget::default()).map_err(|e| format!("{what}: {}", e.message))
    }

    fn value(&self, v: &YVal, site: Site, what: &str) -> Result<Value, String> {
        Ok(match v {
            YVal::Str(t) if t.has_expr() => {
                let tpl = Template::parse_with(&t.src, &options(site, self.dialect)).map_err(|e| format!("{what}: {}", e.message))?;
                expr::eval_value(&tpl, &self.cx(), &mut Budget::default()).map_err(|e| format!("{what}: {}", e.message))?
            }
            YVal::Str(t) => Value::str(&t.src),
            YVal::Bool(b) => Value::Bool(*b),
            YVal::Num(n) => Value::Number(*n),
            YVal::Null => Value::Null,
            other => Value::from_json(&serde_json::to_value(other).unwrap_or_default()),
        })
    }

    fn condition(&self, t: Option<&Tpl>) -> Result<bool, String> {
        match t {
            Some(t) => {
                let c = Condition::parse(&t.src, &options(Site::StepIf, self.dialect)).map_err(|e| format!("if: {}", e.message))?;
                expr::eval_condition(&c, &self.cx(), &mut Budget::default()).map_err(|e| format!("if: {}", e.message))
            }
            None => Ok(!self.failed && !self.cancelled),
        }
    }
}

/// `rx.recv()` while `enabled`, else a future that never completes: a closed channel answers at
/// once, and polled first in a `select` it would starve the step's exit.
async fn recv_if(rx: &async_channel::Receiver<Vec<u8>>, enabled: bool) -> Option<Vec<u8>> {
    if enabled { rx.recv().await.ok() } else { futures::future::pending().await }
}

/// GitHub's shells (W06-R05): the program, its arguments with the script's path, the script's
/// extension, and how the script is wrapped.
fn shell_command(shell: Option<&str>, script: &Path) -> Result<(String, Vec<String>), String> {
    let path = script.to_string_lossy().to_string();
    let simple = |prog: &str, args: &[&str]| (prog.to_owned(), args.iter().map(|a| if *a == "{0}" { path.clone() } else { (*a).to_owned() }).collect());
    Ok(match shell.map(str::trim).filter(|s| !s.is_empty()) {
        None if cfg!(windows) => {
            let ps = if crate::os::have("pwsh") { "pwsh" } else { "powershell" };
            (ps.to_owned(), vec!["-command".into(), format!(". '{path}'")])
        }
        None if crate::os::have("bash") => simple("bash", &["-e", "{0}"]),
        None => simple("sh", &["-e", "{0}"]),
        Some("bash") => simple("bash", &["--noprofile", "--norc", "-eo", "pipefail", "{0}"]),
        Some("sh") => simple("sh", &["-e", "{0}"]),
        Some(ps @ ("pwsh" | "powershell")) => (ps.to_owned(), vec!["-command".into(), format!(". '{path}'")]),
        Some("cmd") => (std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".into()), vec!["/D".into(), "/E:ON".into(), "/V:OFF".into(), "/S".into(), "/C".into(), format!("CALL \"{path}\"")]),
        Some("python") => simple("python", &["{0}"]),
        Some(custom) => {
            if !custom.contains("{0}") {
                return Err(format!("the shell `{custom}` must contain {{0}}, where the script's path goes"));
            }
            let mut parts = custom.split_whitespace();
            let prog = parts.next().unwrap_or("sh").to_owned();
            (prog, parts.map(|a| a.replace("{0}", &path)).collect())
        }
    })
}

/// The script's extension and text for a shell (W06-R05): `.sh`, `.ps1` (with GitHub's wrapper),
/// `.cmd` (CRLF), `.py`.
fn script_file(shell: Option<&str>, body: &str) -> (&'static str, String) {
    let name = shell.map(str::trim).and_then(|s| s.split_whitespace().next()).unwrap_or(if cfg!(windows) { "pwsh" } else { "bash" });
    match name {
        "pwsh" | "powershell" => (".ps1", format!("$ErrorActionPreference = 'stop'\n{body}\nif ((Test-Path -LiteralPath variable:\\LASTEXITCODE)) {{ exit $LASTEXITCODE }}")),
        "cmd" => (".cmd", body.replace("\r\n", "\n").replace('\n', "\r\n")),
        "python" => (".py", body.to_owned()),
        "bash" | "sh" => (".sh", body.to_owned()),
        _ => ("", body.to_owned()),
    }
}

/// Kept legs' runtimes and states, by leg: what rerunning one of their steps needs (W17-R04).
static PARKED: std::sync::LazyLock<parking_lot::Mutex<std::collections::HashMap<String, (Leg, State)>>> = std::sync::LazyLock::new(Default::default);

/// A kept leg let go: its state goes too.
pub fn unpark(job: &str) {
    PARKED.lock().remove(job);
}

/// Runs step `step` of kept leg `job` again in place, as step `idx` of its log (W17-R04): with the
/// environment, paths and outputs the leg ended with, whatever its earlier steps concluded.
/// `false` when the leg isn't kept here.
pub fn rerun_step(job: &str, step: u32, idx: u32) -> bool {
    let Some((leg, mut st)) = PARKED.lock().remove(job) else { return false };
    let job = job.to_owned();
    crate::rt::spawn(async move {
        let def = usize::try_from(step).ok().and_then(|i| leg.a.steps.get(i)).cloned();
        match def {
            Some(def) if !def.is_control() => {
                st.failed = false;
                st.cancelled = false;
                st.rerun_label = Some("Rerun: ".into());
                let deadline = Instant::now() + Duration::from_secs(u64::from(leg.a.timeout_minutes.max(1)) * 60);
                let _ = leg.step(&mut st, idx, &def, deadline, false).await;
                for g in &st.groups {
                    crate::proc::kill_orphan_group(*g, &format!("KILN_LEG={job}"));
                }
                st.groups.clear();
            }
            _ => {
                leg.start(idx, "Rerun", "internal", None).await;
                leg.note(idx, &format!("Step {} can't be rerun: it isn't one of the job's steps.", step + 1)).await;
                leg.event(&JobEventV2::StepFinished { idx, outcome: Conclusion::Failure, conclusion: Conclusion::Failure, exit_code: None, outputs: BTreeMap::new() }).await;
            }
        }
        // Still kept (not released meanwhile): ready for the next rerun.
        if super::kept::kept_until(&job).is_some() {
            PARKED.lock().insert(job, (leg, st));
        }
    });
    true
}

impl Leg {
    pub async fn run(self) -> JobResult {
        let job = self.a.job_id.clone();
        let started = Instant::now();
        let layout = Layout::new(&self.work_dir, &self.a.job_id, &self.a.repo, self.hosted_image());
        let ws = layout.runner_workspace;
        super::workspaces::claim(&self.work_dir, &ws).await;
        let tools = super::workspaces::ToolCache::take(&layout.tool_cache);
        let r = std::panic::AssertUnwindSafe(self.run_inner(tools.path())).catch_unwind().await;
        drop(tools);
        let (status, error) = match r {
            Ok(Ok(s)) => (s, None),
            Ok(Err(e)) => (Status::Failed, Some(format!("{e:#}"))),
            Err(_) => (Status::Failed, Some("the runtime failed unexpectedly".into())),
        };
        // The workspace goes with the leg, unless a failed leg is kept to look into (W17-R02).
        let keep = if matches!(status, Status::Failed | Status::TimedOut) { self.a.debug.keep_on_failure_minutes } else { None };
        if let Some(until) = super::kept::ended(&job, ws, keep).await {
            self.event(&JobEventV2::Kept { until }).await;
        }
        let _ = started;
        JobResult { job, status, exit_code: None, error, peak_rss: None, cpu_seconds: None }
    }

    async fn event(&self, e: &JobEventV2) {
        if let Ok(json) = serde_json::to_string(e) {
            self.up.msg(AgentMsg::JobEvent { job: self.a.job_id.clone(), json }).await;
        }
    }

    async fn log(&self, idx: u32, data: Vec<u8>) {
        if !data.is_empty() {
            for piece in data.chunks(LOG_CHUNK) {
                self.up.log(&self.a.job_id, idx, piece.to_vec()).await;
            }
        }
    }

    async fn note(&self, idx: u32, text: &str) {
        self.up.note(&self.a.job_id, idx, text).await;
    }

    fn env_vars(&self, st: &State) -> BTreeMap<String, String> {
        let a = &self.a;
        let g = &a.contexts.github;
        let s = |k: &str| g.get(k).and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
        let l = &st.layout;
        let mut v: BTreeMap<String, String> = BTreeMap::new();
        let mut set = |k: &str, val: String| {
            v.insert(k.to_owned(), val);
        };
        set("CI", "true".into());
        set("GITHUB_ACTIONS", "true".into());
        for (k, f) in [
            ("GITHUB_ACTOR", "actor"),
            ("GITHUB_ACTOR_ID", "actor_id"),
            ("GITHUB_TRIGGERING_ACTOR", "triggering_actor"),
            ("GITHUB_BASE_REF", "base_ref"),
            ("GITHUB_HEAD_REF", "head_ref"),
            ("GITHUB_EVENT_NAME", "event_name"),
            ("GITHUB_REF", "ref"),
            ("GITHUB_REF_NAME", "ref_name"),
            ("GITHUB_REF_TYPE", "ref_type"),
            ("GITHUB_REPOSITORY", "repository"),
            ("GITHUB_REPOSITORY_ID", "repository_id"),
            ("GITHUB_REPOSITORY_OWNER", "repository_owner"),
            ("GITHUB_REPOSITORY_OWNER_ID", "repository_owner_id"),
            ("GITHUB_RETENTION_DAYS", "retention_days"),
            ("GITHUB_RUN_ID", "run_id"),
            ("GITHUB_RUN_NUMBER", "run_number"),
            ("GITHUB_RUN_ATTEMPT", "run_attempt"),
            ("GITHUB_SHA", "sha"),
            ("GITHUB_WORKFLOW", "workflow"),
            ("GITHUB_WORKFLOW_REF", "workflow_ref"),
            ("GITHUB_WORKFLOW_SHA", "workflow_sha"),
        ] {
            set(k, s(f));
        }
        set("GITHUB_REF_PROTECTED", g.get("ref_protected").and_then(serde_json::Value::as_bool).unwrap_or(false).to_string());
        set("GITHUB_SERVER_URL", a.runtime.facade_url.clone());
        set("GITHUB_API_URL", a.runtime.api_url.clone());
        set("GITHUB_GRAPHQL_URL", a.runtime.graphql_url.clone());
        set("GITHUB_JOB", a.leg.job_id.rsplit('/').next().unwrap_or_default().to_owned());
        set("GITHUB_WORKSPACE", l.workspace.to_string_lossy().to_string());
        set("GITHUB_EVENT_PATH", l.temp.join("_github_workflow").join("event.json").to_string_lossy().to_string());
        set("RUNNER_OS", runner_os().into());
        set("RUNNER_ARCH", runner_arch().into());
        set("RUNNER_NAME", self.runner_name.clone());
        set("RUNNER_TEMP", l.temp.to_string_lossy().to_string());
        set("RUNNER_TOOL_CACHE", l.tool_cache.to_string_lossy().to_string());
        set("AGENT_TOOLSDIRECTORY", l.tool_cache.to_string_lossy().to_string());
        set("RUNNER_WORKSPACE", l.runner_workspace.to_string_lossy().to_string());
        set("RUNNER_ENVIRONMENT", "self-hosted".into());
        if runner_debug(a) {
            set("RUNNER_DEBUG", "1".into());
        }
        let rt = &a.runtime;
        set("ACTIONS_RUNTIME_URL", rt.results_url.clone());
        set("ACTIONS_RUNTIME_TOKEN", rt.runtime_token.clone());
        set("ACTIONS_RESULTS_URL", rt.results_url.clone());
        set("ACTIONS_CACHE_URL", rt.cache_url.clone());
        // The toolkit takes any server but github.com for GitHub Enterprise Server, and uses its
        // cache protocol (`ACTIONS_CACHE_URL`), which kiln serves (W08-R07).
        if let Some(m) = &rt.cache_mode {
            set("ACTIONS_CACHE_MODE", m.clone());
        }
        if let (Some(u), Some(t)) = (&rt.id_token_url, &rt.id_token_request_token) {
            set("ACTIONS_ID_TOKEN_REQUEST_URL", u.clone());
            set("ACTIONS_ID_TOKEN_REQUEST_TOKEN", t.clone());
        }
        set("KILN_ATTEMPT", a.attempt.to_string());
        set("KILN_EXECUTOR", if a.executor.kind.is_empty() { "host".into() } else { a.executor.kind.clone() });
        set("KILN_TOKEN", rt.github_token.clone());
        set("KILN_JOB", a.job_id.clone());
        set("KILN_RUN", a.run_id.clone());
        v
    }

    async fn run_inner(&self, tool_cache: PathBuf) -> anyhow::Result<Status> {
        let a = &self.a;
        let dialect = if a.dialect == "kiln" { Dialect::Kiln } else { Dialect::Gha };
        let layout = Layout { tool_cache, ..Layout::new(&self.work_dir, &a.job_id, &a.repo, self.hosted_image()) };
        let (ws, temp, cmds) = (layout.workspace.clone(), layout.temp.clone(), layout.commands.clone());
        let event = serde_json::to_vec(&a.contexts.github.get("event").cloned().unwrap_or_default()).unwrap_or_default();
        crate::rt::blocking(move || -> std::io::Result<()> {
            let _ = std::fs::remove_dir_all(&temp);
            std::fs::create_dir_all(&ws)?;
            std::fs::create_dir_all(&cmds)?;
            std::fs::create_dir_all(temp.join("_github_workflow"))?;
            std::fs::write(temp.join("_github_workflow").join("event.json"), event)
        })
        .await??;
        let secrets: BTreeMap<String, String> = a.secrets.iter().map(|s| (s.name.clone(), s.value.clone())).collect();
        let mut masks: Vec<String> = secrets.iter().filter(|(k, _)| !kiln_ci::secrets::DEBUG_FLAGS.contains(&k.as_str())).map(|(_, v)| v.clone()).collect();
        masks.extend(a.masks.iter().cloned());
        masks.push(a.runtime.github_token.clone());
        masks.push(a.runtime.runtime_token.clone());
        let step_debug = a.debug.step_debug || debug_flag(a, "ACTIONS_STEP_DEBUG");
        // The paths expressions see: the leg's own. In the `docker` executor's container (the
        // leg's machine) they're the container's; a job's `container:` sees the host's, as on
        // GitHub (`github.workspace` there is the runner's path).
        let hosted = self.hosted_image().map(|_| self.container_map(&layout));
        let seen = |p: &Path| hosted.as_ref().map_or_else(|| p.to_string_lossy().into_owned(), |m| m.translate(&p.to_string_lossy()));
        let base = vec![
            ("vars", Value::from_json(&a.contexts.vars)),
            ("needs", Value::from_json(&a.contexts.needs)),
            ("strategy", Value::from_json(&a.contexts.strategy)),
            ("matrix", Value::from_json(&a.contexts.matrix)),
            ("inputs", Value::from_json(&a.contexts.inputs)),
            ("run", Value::from_json(&a.contexts.run)),
            (
                "runner",
                Value::object([
                    ("name".to_owned(), Value::str(&self.runner_name)),
                    ("os".to_owned(), Value::str(runner_os())),
                    ("arch".to_owned(), Value::str(runner_arch())),
                    ("temp".to_owned(), Value::str(&seen(&layout.temp))),
                    ("tool_cache".to_owned(), Value::str(&seen(&layout.tool_cache))),
                    ("debug".to_owned(), Value::str(if runner_debug(a) { "1" } else { "" })),
                    ("environment".to_owned(), Value::str("self-hosted")),
                ]),
            ),
        ];
        // The github context, completed with what only the runner knows.
        let mut github = a.contexts.github.clone();
        if let Some(m) = github.as_object_mut() {
            m.insert("workspace".into(), serde_json::json!(seen(&layout.workspace)));
            m.insert("token".into(), serde_json::json!(a.runtime.github_token));
            m.insert("job".into(), serde_json::json!(a.leg.job_id.rsplit('/').next().unwrap_or_default()));
            m.insert("event_path".into(), serde_json::json!(seen(&layout.temp.join("_github_workflow").join("event.json"))));
            m.insert("action".into(), serde_json::json!(""));
            // The server, as this runner reaches it.
            m.insert("server_url".into(), serde_json::json!(a.runtime.facade_url));
            m.insert("api_url".into(), serde_json::json!(a.runtime.api_url));
            m.insert("graphql_url".into(), serde_json::json!(a.runtime.graphql_url));
        }
        let mut st = State {
            rerun_label: None,
            cleanup: false,
            dialect,
            layout,
            env: BTreeMap::new(),
            path: PrependPath::default(),
            steps: Vec::new(),
            failed: false,
            cancelled: false,
            masker: Masker::new(&masks),
            secrets,
            github: Value::from_json(&github),
            base,
            step_debug,
            allow_unsecure: false,
            groups: Vec::new(),
            posts: Vec::new(),
            next_idx: u32::try_from(a.steps.len()).unwrap_or(u32::MAX),
            server_url: a.runtime.facade_url.clone(),
            containers: None,
            matchers: matchers::Job::default(),
            stop: None,
            masks_added: Vec::new(),
            step_json: BTreeMap::new(),
            run_env: Vec::new(),
            sensitive: secret_feeds(a, dialect),
        };
        // The job's environment: the workflow's, `env-from`, then the job's (W04-R21, W06-R04).
        for (k, v) in &a.env_from {
            st.env.insert(k.clone(), v.clone());
        }
        for (scope, list) in [("env", &a.workflow_env), ("jobs.env", &a.job_env)] {
            for (k, t) in list {
                let site = if scope == "env" { Site::WfEnv } else { Site::JobEnv };
                match st.render(t, site, &format!("{scope}.{k}")) {
                    Ok(v) => {
                        st.env.insert(k.clone(), v);
                    }
                    Err(e) => {
                        self.note(0, &format!("Error: {e}")).await;
                        return Ok(Status::Failed);
                    }
                }
            }
        }
        st.allow_unsecure = st.env.get("ACTIONS_ALLOW_UNSECURE_COMMANDS").is_some_and(|v| v == "true");
        let deadline = Instant::now() + Duration::from_secs(u64::from(a.timeout_minutes.max(1)) * 60);
        let mut timed_out = false;
        let executor_container = a.executor.kind == "docker" && a.executor.image.is_some();
        if (a.job_container.is_some() || !a.job_services.is_empty() || executor_container) && !self.init_containers(&mut st).await {
            if let Some(c) = &st.containers {
                c.stop().await;
            }
            return Ok(Status::Failed);
        }
        // What a terminal into the leg opens (W17-R01): its workspace and environment, or its
        // container.
        let mut shell_env = self.env_vars(&st);
        shell_env.extend(st.env.iter().map(|(k, v)| (k.clone(), v.clone())));
        let exec = st.containers.as_ref().and_then(|c| c.job.as_ref().map(|j| (c, j))).map(|(c, j)| {
            let inside: BTreeMap<String, String> = shell_env.iter().map(|(k, v)| (k.clone(), j.map.translate(v))).collect();
            c.cli.shell_argv(&j.id, j.user.as_deref(), &j.map.translate(&st.layout.workspace.to_string_lossy()), &inside)
        });
        super::kept::running(&a.job_id, super::kept::Shell { workdir: st.layout.workspace.clone(), env: shell_env, masks: masks.clone(), exec });
        // The kiln dialect's implicit checkout (W06-R14); GitHub's workflows never get one.
        if a.checkout == "auto" && !self.implicit_checkout(&mut st).await {
            st.failed = true;
        }
        self.pre_steps(&mut st, deadline).await;
        let turns = async_lock::Semaphore::new(MAX_BACKGROUND);
        let mut bg = Background::new(&turns);
        for (idx, step) in a.steps.iter().enumerate() {
            let idx = u32::try_from(idx).unwrap_or(u32::MAX);
            if self.cancel.is_cancelled() {
                st.cancelled = true;
            }
            if step.is_control() {
                self.control(&mut st, &mut bg, idx, step, deadline).await;
                continue;
            }
            if self.background(&mut st, &mut bg, idx, idx, step, deadline) {
                bg.kick();
                continue;
            }
            let end = bg.drive(self.step(&mut st, idx, step, deadline, false)).await;
            if end == Some(End::TimedOut) && Instant::now() >= deadline {
                timed_out = true;
                st.failed = true;
                self.note(idx, &format!("The job running on runner {} has exceeded the maximum execution time of {} minutes.", self.runner_name, a.timeout_minutes)).await;
            }
        }
        // An implicit `wait-all` before the post steps (W06-R17), so the job's outputs see them.
        let rest = bg.unfolded();
        if !rest.is_empty() {
            bg.settle(&rest).await;
            self.fold(&mut st, &mut bg, &rest);
        }
        self.job_outputs(&st).await;
        self.post_steps(&mut st).await;
        // A failed leg kept to look into keeps its containers up (W17-R02).
        let keep = (st.failed || timed_out) && !st.cancelled && a.debug.keep_on_failure_minutes.is_some_and(|m| m > 0);
        if let Some(c) = st.containers.take() {
            if keep {
                super::kept::hold(&a.job_id, c);
            } else {
                let idx = st.next_idx;
                st.next_idx += 1;
                self.start(idx, "Stop containers", "internal", None).await;
                c.stop().await;
                self.event(&JobEventV2::StepFinished { idx, outcome: Conclusion::Success, conclusion: Conclusion::Success, exit_code: None, outputs: BTreeMap::new() }).await;
            }
        }
        // The leg's processes end with it (GitHub's "Cleaning up orphan processes").
        for g in &st.groups {
            crate::proc::kill_orphan_group(*g, &format!("KILN_LEG={}", a.job_id));
        }
        st.groups.clear();
        let status = if timed_out {
            Status::TimedOut
        } else if st.cancelled || self.cancel.is_cancelled() {
            Status::Cancelled
        } else if st.failed {
            Status::Failed
        } else {
            Status::Succeeded
        };
        // A kept leg keeps its state, to rerun a step in place (W17-R04).
        if keep {
            PARKED.lock().insert(a.job_id.clone(), (self.clone(), st));
        }
        Ok(status)
    }

    /// `checkout: auto` (W06-R14): the run's commit in the workspace before the first step, as a
    /// step of its own. A shallow git fetch through the server's git facade with the leg's token
    /// (`.git` kept for tools that need it), else the facade's tarball of the commit.
    async fn implicit_checkout(&self, st: &mut State) -> bool {
        let idx = st.next_idx;
        st.next_idx += 1;
        self.start(idx, "Check out the commit", "internal", None).await;
        let r = self.checkout_commit(st, idx).await;
        if let Err(e) = &r {
            self.annotate(idx, "error", &format!("Checking out the commit failed: {e:#}")).await;
        }
        let c = if r.is_ok() { Conclusion::Success } else { Conclusion::Failure };
        self.event(&JobEventV2::StepFinished { idx, outcome: c, conclusion: c, exit_code: None, outputs: BTreeMap::new() }).await;
        r.is_ok()
    }

    async fn checkout_commit(&self, st: &State, idx: u32) -> anyhow::Result<()> {
        use base64::Engine as _;
        let a = &self.a;
        let sha = a.contexts.github.get("sha").and_then(serde_json::Value::as_str).filter(|s| !s.is_empty()).ok_or_else(|| anyhow::anyhow!("the run has no commit"))?.to_owned();
        let ws = st.layout.workspace.clone();
        let remote = format!("{}/{}", st.server_url.trim_end_matches('/'), a.repo);
        let token = a.runtime.github_token.clone();
        if crate::rt::blocking(|| which("git")).await.unwrap_or(false) {
            // The token rides in git's environment config, never on a command line.
            let header = format!("AUTHORIZATION: basic {}", base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}")));
            let w = ws.to_string_lossy().to_string();
            let steps: [Vec<&str>; 4] = [
                vec!["init", "--quiet", &w],
                vec!["-C", &w, "fetch", "--quiet", "--depth=1", "--no-tags", &remote, &sha],
                vec!["-C", &w, "checkout", "--quiet", "--detach", "FETCH_HEAD"],
                vec!["-C", &w, "remote", "add", "origin", &remote],
            ];
            for args in steps {
                // As typed in the workspace: without `-C <workspace>`.
                let shown: Vec<&str> = if args.first() == Some(&"-C") { args.iter().skip(2).copied().collect() } else { args.iter().copied().filter(|x| *x != w.as_str()).collect() };
                self.note(idx, &format!("git {}", shown.join(" "))).await;
                let mut cmd = Command::new("git");
                cmd.args(&args).env("GIT_TERMINAL_PROMPT", "0").env("GIT_CONFIG_COUNT", "1").env("GIT_CONFIG_KEY_0", "http.extraheader").env("GIT_CONFIG_VALUE_0", &header);
                let out = crate::proc::output(cmd, Duration::from_secs(1800)).await?;
                if !out.status.success() {
                    anyhow::bail!("git {}: {}", args.get(2).copied().unwrap_or_default(), String::from_utf8_lossy(&out.stderr).trim());
                }
            }
            return Ok(());
        }
        // No git: the commit's tarball from the REST facade, its top directory stripped.
        let url = format!("{}/api/v3/repos/{}/tarball/{sha}", st.server_url.trim_end_matches('/'), a.repo);
        self.note(idx, &format!("git isn't installed here: downloading {} at {}", a.repo, sha.get(..12).unwrap_or(&sha))).await;
        let mut resp = self.http.get(&url).bearer(&token).timeout(Duration::from_secs(600)).send().await?;
        if !resp.is_success() {
            anyhow::bail!("the commit's tarball: HTTP {}", resp.status());
        }
        let mut bytes = Vec::new();
        while let Some(c) = resp.chunk().await? {
            bytes.extend_from_slice(&c);
            anyhow::ensure!(bytes.len() < 8 << 30, "the commit is over 8 GiB");
        }
        crate::rt::blocking(move || -> anyhow::Result<()> {
            std::fs::create_dir_all(&ws)?;
            let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(bytes.as_slice()));
            for entry in ar.entries()? {
                let mut entry = entry?;
                let path = entry.path()?.into_owned();
                let rel: PathBuf = path.components().skip(1).collect();
                if rel.as_os_str().is_empty() || rel.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
                    continue;
                }
                let target = ws.join(rel);
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                entry.unpack(target)?;
            }
            Ok(())
        })
        .await??;
        Ok(())
    }

    /// Starts `step` in the background when it says so (W06-R17): on a copy of the leg's state,
    /// beside the steps that follow, its turn among [`MAX_BACKGROUND`] at once. Whether it did.
    fn background<'a>(&'a self, st: &mut State, bg: &mut Background<'a>, idx: u32, order: u32, step: &'a Step, deadline: Instant) -> bool {
        let on = match &step.background {
            Some(v) => st.value(v, Site::StepContinueOnError, "background").map(|v| v.truthy()).unwrap_or(false),
            None => false,
        };
        if !on {
            return false;
        }
        self.start_background(st, bg, idx, order, step, step.id.clone().unwrap_or_else(|| format!("__background_{idx}")), deadline);
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn start_background<'a>(&'a self, st: &State, bg: &mut Background<'a>, idx: u32, order: u32, step: &'a Step, id: String, deadline: Instant) {
        let stop = Shutdown::new();
        bg.started.insert(id.clone(), (order, stop.clone()));
        let mut s = st.clone();
        s.stop = Some(stop.clone());
        s.masks_added.clear();
        let (env0, path0, posts0, groups0, steps0) = (st.env.clone(), st.path.entries().to_vec(), st.posts.len(), st.groups.len(), st.steps.len());
        let turns = bg.turns;
        let fut: BgFuture<'a> = Box::pin(async move {
            // Its turn, unless it is cancelled first.
            let permit = futures::select_biased! {
                () = stop.cancelled().fuse() => None,
                p = turns.acquire().fuse() => Some(p),
            };
            let end = match permit {
                Some(_turn) => self.step(&mut s, idx, step, deadline, false).await,
                None => {
                    let name = step.id.clone().unwrap_or_else(|| "a background step".into());
                    self.start(idx, &name, if step.uses.is_some() { "uses" } else { "run" }, step.uses.clone()).await;
                    self.finish(&mut s, idx, step.id.clone(), Conclusion::Cancelled, Conclusion::Cancelled, None, BTreeMap::new(), false).await;
                    Some(End::Cancelled)
                }
            };
            let done = s.steps.get(steps0..).and_then(<[Done]>::last).cloned();
            let conclusion = done.as_ref().and_then(|d| d.conclusion).unwrap_or(if end == Some(End::Cancelled) { Conclusion::Cancelled } else { Conclusion::Success });
            BgDone {
                id,
                order,
                env: s.env.iter().filter(|(k, v)| env0.get(*k) != Some(*v)).map(|(k, v)| (k.clone(), v.clone())).collect(),
                path: s.path.entries().iter().rev().filter(|d| !path0.contains(d)).cloned().collect(),
                masks: std::mem::take(&mut s.masks_added),
                done,
                posts: s.posts.get(posts0..).map(<[Post]>::to_vec).unwrap_or_default(),
                groups: s.groups.get(groups0..).map(<[i32]>::to_vec).unwrap_or_default(),
                conclusion,
            }
        });
        bg.running.push(fut);
    }

    /// Folds finished background steps into the leg, in declaration order: environment, path,
    /// masks, the `steps` context, post steps. Whether any of them failed.
    fn fold(&self, st: &mut State, bg: &mut Background<'_>, ids: &[String]) -> bool {
        let mut list: Vec<&BgDone> = ids.iter().filter(|id| !bg.folded.contains(*id)).filter_map(|id| bg.finished.get(id)).collect();
        list.sort_by_key(|d| d.order);
        let mut failed = false;
        for d in list {
            for (k, v) in &d.env {
                st.env.insert(k.clone(), v.clone());
            }
            for p in &d.path {
                st.path.add(p);
            }
            for m in &d.masks {
                st.masker.add(m);
            }
            if let Some(done) = &d.done {
                st.steps.push(done.clone());
            }
            st.posts.extend(d.posts.iter().cloned());
            st.groups.extend(d.groups.iter().copied());
            failed |= d.conclusion == Conclusion::Failure;
        }
        for id in ids {
            if bg.finished.contains_key(id) {
                bg.folded.insert(id.clone());
            }
        }
        failed
    }

    /// `wait`, `wait-all`, `cancel` and `parallel` (W06-R17): they always run and take no `if`.
    async fn control<'a>(&'a self, st: &mut State, bg: &mut Background<'a>, idx: u32, step: &'a Step, deadline: Instant) {
        let (name, ids) = if !step.parallel.is_empty() {
            let mut ids = Vec::new();
            for (n, child) in step.parallel.iter().enumerate() {
                let cidx = st.next_idx;
                st.next_idx += 1;
                let id = child.id.clone().unwrap_or_else(|| format!("__parallel_{idx}_{n}"));
                // Declared where the group is, in their order within it.
                let order = idx.saturating_mul(1000).saturating_add(u32::try_from(n).unwrap_or(0));
                self.start_background(st, bg, cidx, order, child, id.clone(), deadline);
                ids.push(id);
            }
            bg.kick();
            (step.name.as_ref().map_or_else(|| "Run in parallel".to_owned(), |t| st.render(t, Site::StepName, "name").unwrap_or_else(|_| t.src.clone())), ids)
        } else if !step.cancel.is_empty() {
            for id in &step.cancel {
                if let Some((_, stop)) = bg.started.get(id) {
                    stop.cancel();
                }
            }
            (format!("Cancel {}", step.cancel.join(", ")), step.cancel.clone())
        } else if step.wait_all {
            ("Wait for all background steps".to_owned(), bg.unfolded())
        } else {
            (format!("Wait for {}", step.wait.join(", ")), step.wait.clone())
        };
        self.start(idx, &st.masker.mask(&name), "control", None).await;
        bg.settle(&ids).await;
        let failed = self.fold(st, bg, &ids);
        // A failed step fails the `wait` that includes it (its `continue-on-error` made it a
        // success already); a cancelled one doesn't fail its `cancel`.
        let c = if failed && step.cancel.is_empty() { Conclusion::Failure } else { Conclusion::Success };
        if c == Conclusion::Failure {
            st.failed = true;
        }
        self.finish(st, idx, step.id.clone(), c, c, None, BTreeMap::new(), false).await;
    }

    /// One step (W06-R04): `if`, name, env, working directory, shell, timeout,
    /// continue-on-error, run, environment files, outcome and conclusion.
    /// `nested`: a composite action's step, which reports into its caller's step.
    async fn step(&self, st: &mut State, idx: u32, step: &Step, job_deadline: Instant, nested: bool) -> Option<End> {
        let id = step.id.clone();
        // Without a `name`, GitHub titles a step from its evaluated script's first line.
        let fallback_name = match (&step.run, &step.uses) {
            (Some(r), _) => {
                let script = st.render(r, Site::StepRun, "run").unwrap_or_else(|_| r.src.clone());
                format!("Run {}", script.lines().find(|l| !l.trim().is_empty()).unwrap_or_default().trim())
            }
            (None, Some(u)) => format!("Run {u}"),
            _ => "Step".to_owned(),
        };
        let name = match &step.name {
            Some(t) => st.render(t, Site::StepName, "name").unwrap_or_else(|_| t.src.clone()),
            None => fallback_name,
        };
        let name = st.masker.mask(&name);
        let name = match st.rerun_label.take() {
            Some(label) => format!("{label}{name}"),
            None => name,
        };
        let kind = if step.uses.is_some() { "uses" } else { "run" };
        let run = match st.condition(step.cond.as_ref()) {
            Ok(r) => r,
            Err(e) => {
                self.begin(nested, idx, &name, kind, step.uses.clone()).await;
                self.note(idx, &format!("Error: {e}")).await;
                st.failed = true;
                self.finish(st, idx, id, Conclusion::Failure, Conclusion::Failure, None, BTreeMap::new(), nested).await;
                return None;
            }
        };
        if !run {
            if !nested {
                self.begin(nested, idx, &name, kind, step.uses.clone()).await;
            }
            self.finish(st, idx, id, Conclusion::Skipped, Conclusion::Skipped, None, BTreeMap::new(), nested).await;
            return None;
        }
        self.begin(nested, idx, &name, kind, step.uses.clone()).await;
        let continue_on_error = match &step.continue_on_error {
            Some(v) => st.value(v, Site::StepContinueOnError, "continue-on-error").map(|v| v.truthy()).unwrap_or(false),
            None => false,
        };
        let fail = |e: String| e;
        let prepared = (|| -> Result<_, String> {
            let mut env = st.env.clone();
            for (k, t) in &step.env {
                env.insert(k.clone(), st.render(t, Site::StepEnv, &format!("env.{k}"))?);
            }
            let wd = match step.working_directory.as_ref().or(self.a.job_defaults.working_directory.as_ref()).or(self.a.workflow_defaults.working_directory.as_ref()) {
                Some(t) => {
                    let w = st.render(t, Site::StepWorkingDirectory, "working-directory")?;
                    let p = PathBuf::from(&w);
                    if p.is_absolute() { p } else { st.layout.workspace.join(p) }
                }
                None => st.layout.workspace.clone(),
            };
            let shell = match step.shell.as_ref().or(self.a.job_defaults.shell.as_ref()).or(self.a.workflow_defaults.shell.as_ref()) {
                Some(t) => Some(st.render(t, Site::StepRun, "shell")?),
                None => None,
            };
            let timeout = match &step.timeout_minutes {
                Some(v) => {
                    let m = st.value(v, Site::StepTimeout, "timeout-minutes")?;
                    let n = match m {
                        Value::Number(n) => n,
                        Value::String(s) => s.trim().parse().map_err(|_| format!("timeout-minutes: `{s}` is not a number"))?,
                        _ => return Err("timeout-minutes must be a number".into()),
                    };
                    Some(Duration::from_secs_f64((n.max(0.0)) * 60.0))
                }
                None => None,
            };
            let script = match &step.run {
                Some(t) => Some(st.render(t, Site::StepRun, "run")?),
                None => None,
            };
            let mut with = BTreeMap::new();
            for (k, v) in &step.with {
                with.insert(k.clone(), st.value(v, Site::StepWith, &format!("with.{k}"))?.coerce_string());
            }
            Ok((env, wd, shell, timeout, script, with))
        })();
        let (env, wd, shell, timeout, script, with) = match prepared {
            Ok(x) => x,
            Err(e) => {
                self.note(idx, &format!("Error: {}", fail(e))).await;
                let c = if continue_on_error { Conclusion::Success } else { Conclusion::Failure };
                st.failed |= c == Conclusion::Failure;
                self.finish(st, idx, id, Conclusion::Failure, c, None, BTreeMap::new(), nested).await;
                return None;
            }
        };
        let deadline = match timeout {
            Some(t) => job_deadline.min(Instant::now() + t),
            None => job_deadline,
        };
        let ran = match (&script, &step.uses) {
            (Some(script), _) => self.exec(st, idx, Run::Script { script, shell: shell.as_deref() }, &wd, env, deadline).await,
            (None, Some(uses)) => Box::pin(self.action(st, idx, step, uses, with, &wd, env, deadline)).await,
            (None, None) if step.is_control() => Err(anyhow::anyhow!("`wait`, `wait-all`, `cancel` and `parallel` can't be steps of a composite action (docs/ci.md W06-R17)")),
            (None, None) => Err(anyhow::anyhow!("the step runs neither a script nor an action")),
        };
        let (end, outputs) = match ran {
            Ok(r) => (r.end, r.outputs),
            Err(e) => {
                self.note(idx, &format!("Error: {e:#}")).await;
                (End::Exit(None), BTreeMap::new())
            }
        };
        // W16-R01: the step's test reports go to the server, which keeps and parses them. With
        // `quarantine: enforce`, a failure of quarantined tests only passes, with a warning (R05).
        let excused = match &step.kiln.test_results {
            Some(spec) if !matches!(end, End::Cancelled) => self.test_reports(st, idx, spec, &wd).await && !matches!(end, End::Exit(Some(0))) && self.a.quarantine,
            _ => false,
        };
        let (outcome, exit_code) = match end {
            End::Exit(Some(0)) => (Conclusion::Success, Some(0)),
            End::Exit(code) if excused => {
                self.annotate(idx, "warning", &format!("Process completed with exit code {}: every test that failed is quarantined (.kiln/quarantine.yaml), so the step passes.", code.map_or_else(|| "none".to_owned(), |c| c.to_string()))).await;
                (Conclusion::Failure, code)
            }
            End::Exit(code) => {
                let msg = match code {
                    Some(c) => format!("Process completed with exit code {c}."),
                    None => "Process ended without an exit code (killed by a signal).".to_owned(),
                };
                self.annotate(idx, "error", &msg).await;
                (Conclusion::Failure, code)
            }
            End::TimedOut => {
                self.annotate(idx, "error", "The action has timed out.").await;
                (Conclusion::Failure, None)
            }
            End::Cancelled => {
                st.cancelled = true;
                (Conclusion::Cancelled, None)
            }
        };
        let conclusion = if outcome == Conclusion::Failure && (continue_on_error || excused) { Conclusion::Success } else { outcome };
        if conclusion == Conclusion::Failure {
            st.failed = true;
        }
        self.finish(st, idx, id, outcome, conclusion, exit_code, outputs, nested).await;
        Some(end)
    }

    /// A step's `test-results:` (W16-R01): `path` (one, a list, or lines; `@actions/glob`'s
    /// patterns, from the step's working directory) and `format` (found from each file when left
    /// out). Each report goes to the server, which keeps and parses it; what it found goes to the
    /// step's log. Whether there were failures and quarantine covers every one of them (W16-R05).
    async fn test_reports(&self, st: &mut State, idx: u32, spec: &YVal, wd: &Path) -> bool {
        const MAX_REPORTS: usize = 64;
        const MAX_BYTES: u64 = 64 << 20;
        let (path_v, format_v) = match spec {
            YVal::Map(_) => (spec.get("path").cloned(), spec.get("format").cloned()),
            other => (Some(other.clone()), None),
        };
        let mut patterns: Vec<String> = Vec::new();
        let items = match path_v {
            Some(YVal::Seq(items)) => items,
            Some(v) => vec![v],
            None => Vec::new(),
        };
        for v in &items {
            match st.value(v, Site::StepWith, "test-results.path") {
                Ok(x) => patterns.extend(x.coerce_string().lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned)),
                Err(e) => {
                    self.annotate(idx, "warning", &format!("test-results: {e}")).await;
                    return false;
                }
            }
        }
        if patterns.is_empty() {
            self.annotate(idx, "warning", "test-results: say where the reports are (`test-results: { path: … }`)").await;
            return false;
        }
        let format = match &format_v {
            Some(v) => st.value(v, Site::StepWith, "test-results.format").map(|x| x.coerce_string()).unwrap_or_default(),
            None => String::new(),
        };
        // Patterns are the working directory's; the workspace is what they may reach.
        let root = st.layout.workspace.clone();
        let rel = wd.strip_prefix(&root).map(|r| r.to_string_lossy().replace('\\', "/")).unwrap_or_default();
        let patterns: Vec<String> = patterns
            .iter()
            .map(|p| {
                let (neg, body) = p.strip_prefix('!').map_or(("", p.as_str()), |b| ("!", b));
                if Path::new(body).is_absolute() || rel.is_empty() { p.clone() } else { format!("{neg}{rel}/{body}") }
            })
            .collect();
        let files = match super::hashfiles::Workspace::new(&root).and_then(|w| w.matching(&patterns)) {
            Ok(f) => f,
            Err(e) => {
                self.annotate(idx, "warning", &format!("test-results: {}", e.message)).await;
                return false;
            }
        };
        if files.is_empty() {
            self.annotate(idx, "warning", &format!("test-results: no report at {}", patterns.join(", "))).await;
            return false;
        }
        let base = self.a.runtime.results_url.trim_end_matches('/').to_owned();
        let (mut failed, mut all_quarantined) = (0u64, true);
        for f in files.iter().take(MAX_REPORTS) {
            let full = root.join(f);
            let size = std::fs::metadata(&full).map(|m| m.len()).unwrap_or(0);
            if size > MAX_BYTES {
                self.annotate(idx, "warning", &format!("test-results: {f} is {} MiB, more than the {} MiB a report may be", size >> 20, MAX_BYTES >> 20)).await;
                continue;
            }
            let body = match std::fs::read(&full) {
                Ok(b) => b,
                Err(e) => {
                    self.annotate(idx, "warning", &format!("test-results: {f}: {e}")).await;
                    continue;
                }
            };
            let url = format!("{base}/_kiln/test-results?step={idx}&format={}&path={}&root={}", crate::ci::cli::enc(&format), crate::ci::cli::enc(f), crate::ci::cli::enc(&root.to_string_lossy()));
            let sent = async {
                let resp = self.http.put(&url).bearer(&self.a.runtime.runtime_token).body(body).timeout(Duration::from_secs(120)).send().await?;
                let status = resp.status();
                let v: serde_json::Value = resp.json(1 << 20).await.unwrap_or_default();
                anyhow::ensure!((200..300).contains(&status), "{}", v.get("error").and_then(serde_json::Value::as_str).unwrap_or("the server refused the report"));
                Ok(v)
            }
            .await;
            let v = match sent {
                Ok(v) => v,
                Err(e) => {
                    self.annotate(idx, "warning", &format!("test-results: {f}: {e:#}")).await;
                    all_quarantined = false;
                    continue;
                }
            };
            let n = |k: &str| v.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0);
            let (fails, errors) = (n("failed"), n("errors"));
            failed += fails + errors;
            let mut line = format!("Test results ({f}, {}): {} passed, {} failed, {} skipped", v.get("format").and_then(serde_json::Value::as_str).unwrap_or("?"), n("passed"), fails + errors, n("skipped"));
            let names: Vec<&str> = v.get("failures").and_then(serde_json::Value::as_array).into_iter().flatten().filter_map(serde_json::Value::as_str).take(10).collect();
            if !names.is_empty() {
                line.push_str(&format!("\n  failed: {}", names.join(", ")));
            }
            let q = v.get("quarantine").cloned().unwrap_or_default();
            let quarantined: Vec<String> = q.get("quarantined").and_then(serde_json::Value::as_array).into_iter().flatten().filter_map(|x| x.get(0).and_then(serde_json::Value::as_str).map(str::to_owned)).collect();
            if !quarantined.is_empty() {
                line.push_str(&format!("\n  quarantined: {}", quarantined.join(", ")));
            }
            self.note(idx, &line).await;
            for w in v.get("warnings").and_then(serde_json::Value::as_array).into_iter().flatten().filter_map(serde_json::Value::as_str) {
                self.annotate(idx, "warning", w).await;
            }
            if fails + errors > 0 && q.get("only_quarantined").and_then(serde_json::Value::as_bool) != Some(true) {
                all_quarantined = false;
            }
        }
        failed > 0 && all_quarantined
    }

    /// A step begins: its own row, or a header in its composite caller's log.
    async fn begin(&self, nested: bool, idx: u32, name: &str, kind: &str, action: Option<String>) {
        if nested {
            self.note(idx, name).await;
        } else {
            self.start(idx, name, kind, action).await;
        }
    }

    /// Where a Docker action sees the runner's directories (GitHub's mounts).
    fn action_paths(&self, st: &State) -> super::docker::PathMap {
        let l = &st.layout;
        super::docker::PathMap::new(vec![
            (l.workspace.clone(), "/github/workspace".into()),
            (l.temp.join("_github_home"), "/github/home".into()),
            (l.temp.join("_github_workflow"), "/github/workflow".into()),
            (l.commands.clone(), "/github/file_commands".into()),
            (l.temp.clone(), "/github/runner_temp".into()),
        ])
    }

    /// A container spec from the job's `container` or a `services` entry, its keys evaluated.
    fn container_spec(&self, st: &State, c: &kiln_ci::wf::ir::Container, image: Option<&String>, service: bool) -> Result<super::docker::Spec, String> {
        let (site, creds, env_site) = if service { (Site::JobServices, Site::JobServiceCredentials, Site::JobServiceEnv) } else { (Site::JobContainer, Site::JobContainerCredentials, Site::JobContainerEnv) };
        let image = match image {
            Some(i) => i.clone(),
            None => st.render(&c.image, site, "image")?,
        };
        let credentials = match &c.credentials {
            Some((u, p)) => Some((st.render(u, creds, "credentials.username")?, st.render(p, creds, "credentials.password")?)),
            None => None,
        };
        let mut env = Vec::new();
        for (k, t) in &c.env {
            env.push((k.clone(), st.render(t, env_site, &format!("env.{k}"))?));
        }
        let mut ports = Vec::new();
        for p in &c.ports {
            ports.push(st.value(p, site, "ports")?.coerce_string());
        }
        let mut volumes = Vec::new();
        for v in &c.volumes {
            volumes.push(st.render(v, site, "volumes")?);
        }
        let options = match &c.options {
            Some(o) => Some(st.render(o, site, "options")?),
            None => None,
        };
        Ok(super::docker::Spec { image, credentials, env, ports, volumes, options, docker_socket: false, user: None })
    }

    /// "Initialize containers" (W13-R08): the leg's network, services and job container, before
    /// its first step. Whether they started.
    async fn init_containers(&self, st: &mut State) -> bool {
        let idx = st.next_idx;
        st.next_idx += 1;
        self.start(idx, "Initialize containers", "internal", None).await;
        let r = self.try_init_containers(st, idx).await;
        let ok = r.is_ok();
        if let Err(e) = r {
            self.note(idx, &format!("Error: {e:#}")).await;
            st.failed = true;
        }
        let c = if ok { Conclusion::Success } else { Conclusion::Failure };
        self.event(&JobEventV2::StepFinished { idx, outcome: c, conclusion: c, exit_code: None, outputs: BTreeMap::new() }).await;
        ok
    }

    /// The `docker` executor's image, when the leg runs in one rather than in its `container:`.
    fn hosted_image(&self) -> Option<&str> {
        let a = &self.a;
        if a.job_container.is_none() && a.executor.kind == "docker" { a.executor.image.as_deref() } else { None }
    }

    /// Whether the `docker` executor's container gets the host's Docker socket (W13-R04: the
    /// routing rules' `docker: socket`, for trusted runs).
    fn docker_inside(&self) -> bool {
        self.hosted_image().is_some() && self.a.executor.options.get("docker").and_then(serde_json::Value::as_str) == Some("socket")
    }

    /// Where the leg's directories are inside its containers. A job's `container:` gets
    /// GitHub's `/__w`. The `docker` executor's container (the leg's machine) keeps the host's
    /// paths for the workspace, temporary and action directories: what a step bind-mounts
    /// through the host's Docker (`docker run -v $GITHUB_WORKSPACE:/src`) is what the daemon
    /// finds, and every such leg on a runner has its workspace at the same depth, so the paths a
    /// cache saved relative to it (`actions/cache`) restore in any other.
    fn container_map(&self, l: &Layout) -> super::docker::PathMap {
        let repo = self.a.repo.rsplit('/').next().unwrap_or("repo").to_owned();
        let work = &self.work_dir;
        let same = self.hosted_image().is_some();
        let at = |host: &Path, inside: String| if same { host.to_string_lossy().into_owned() } else { inside };
        super::docker::PathMap::new(vec![
            (l.workspace.clone(), at(&l.workspace, format!("/__w/{repo}/{repo}"))),
            (l.temp.join("_github_home"), "/github/home".into()),
            (l.temp.join("_github_workflow"), "/github/workflow".into()),
            (l.temp.clone(), at(&l.temp, "/__w/_temp".into())),
            (work.join("_actions"), at(&work.join("_actions"), "/__w/_actions".into())),
            // The `docker` executor's container is the machine, laid out as GitHub's hosted
            // runners are (prebuilt tools expect it); a job's `container:` gets `/__t`.
            (l.tool_cache.clone(), if self.hosted_image().is_some() { "/opt/hostedtoolcache".into() } else { "/__t".into() }),
            (work.join("_externals"), "/__e".into()),
        ])
    }

    async fn try_init_containers(&self, st: &mut State, idx: u32) -> anyhow::Result<()> {
        let a = &self.a;
        let bin = crate::rt::blocking(super::docker::find).await?.ok_or_else(|| anyhow::anyhow!("this job uses `container:` or `services:`, and this runner has no working Docker or Podman"))?;
        let config = st.layout.temp.join("_docker_config");
        let l = &st.layout;
        let work = self.work_dir.clone();
        let dirs = [config.clone(), l.temp.join("_github_home"), l.temp.join("_github_workflow"), work.join("_actions"), l.tool_cache.clone(), work.join("_externals")];
        crate::rt::blocking(move || dirs.iter().try_for_each(std::fs::create_dir_all)).await??;
        let map = self.container_map(l);
        let workdir = map.translate(&l.workspace.to_string_lossy());
        // The job's `container:` (GitHub-exact), else the `docker` executor's image (W13-R04).
        let job = match (&a.job_container, &a.executor.image) {
            (Some(c), _) => Some(super::docker::Spec { docker_socket: true, ..self.container_spec(st, c, a.container.as_ref(), false).map_err(anyhow::Error::msg)? }),
            (None, Some(image)) if a.executor.kind == "docker" => Some(super::docker::Spec { image: image.clone(), user: super::docker::host_user(), docker_socket: self.docker_inside(), ..super::docker::Spec::default() }),
            _ => None,
        };
        let mut services = Vec::new();
        for (key, c) in &a.job_services {
            services.push((key.clone(), self.container_spec(st, c, a.services.get(key), true).map_err(anyhow::Error::msg)?));
        }
        let said = parking_lot::Mutex::new(Vec::<String>::new());
        let say = |m: String| said.lock().push(m);
        let started = super::docker::Containers::start(super::docker::Cli::new(&bin, config), &a.job_id, map, &workdir, job, services, &say).await;
        let lines = std::mem::take(&mut *said.lock());
        for line in lines {
            self.log(idx, format!("{}\r\n", st.masker.mask(&line)).into_bytes()).await;
        }
        st.containers = Some(started?);
        Ok(())
    }

    async fn start(&self, idx: u32, name: &str, kind: &str, action: Option<String>) {
        self.event(&JobEventV2::StepStarted { idx, name: name.to_owned(), kind: kind.to_owned(), action }).await;
    }

    #[allow(clippy::too_many_arguments)]
    async fn finish(&self, st: &mut State, idx: u32, id: Option<String>, outcome: Conclusion, conclusion: Conclusion, exit_code: Option<i32>, outputs: BTreeMap<String, String>, nested: bool) {
        // A step output a secret job output reads is masked from here on (W04-R22).
        if let Some(step) = &id {
            for (k, v) in &outputs {
                if !v.is_empty() && st.sensitive.contains(&(step.clone(), k.clone())) && st.masker.add(v) {
                    self.event(&JobEventV2::Mask { value_sha256: crate::util::sha256_hex(v.as_bytes()) }).await;
                }
            }
        }
        if !nested {
            let masked: BTreeMap<String, String> = outputs.iter().map(|(k, v)| (k.clone(), st.masker.mask(v))).collect();
            self.event(&JobEventV2::StepFinished { idx, outcome, conclusion, exit_code, outputs: masked }).await;
        }
        let json = std::mem::take(&mut st.step_json);
        st.steps.push(Done { id, outputs, json, outcome: Some(outcome), conclusion: Some(conclusion) });
    }

    async fn annotate(&self, idx: u32, level: &str, message: &str) {
        self.event(&JobEventV2::Annotation { idx, level: level.to_owned(), message: message.to_owned(), title: None, file: None, line: None, end_line: None, col: None, end_col: None, log_offset: 0 }).await;
    }

    /// Runs a script or a program and processes what it prints and writes (W06-R05 to W06-R08).
    async fn exec(&self, st: &mut State, idx: u32, what: Run<'_>, wd: &Path, mut env: BTreeMap<String, String>, deadline: Instant) -> anyhow::Result<Ran> {
        let id = crate::util::new_id();
        let in_job = st.containers.as_ref().and_then(|c| c.job.as_ref()).is_some() && !matches!(what, Run::Container { .. });
        let (file, body, shell) = match &what {
            Run::Script { script, shell } => {
                // In a job container without bash, the default shell is sh (GitHub-exact).
                let shell = match shell {
                    None if in_job && st.containers.as_ref().and_then(|c| c.job.as_ref()).is_some_and(|j| !j.has_bash) => Some("sh"),
                    other => *other,
                };
                let (ext, body) = script_file(shell, script);
                (Some(st.layout.temp.join(format!("{id}{ext}"))), body, shell)
            }
            Run::Program { .. } | Run::Container { .. } => (None, String::new(), None),
        };
        let files: Vec<(&str, PathBuf)> = ["GITHUB_ENV", "GITHUB_OUTPUT", "GITHUB_PATH", "GITHUB_STATE", "GITHUB_STEP_SUMMARY", "KILN_OUTPUT_JSON", "KILN_RUN_ENV"]
            .iter()
            .map(|k| (*k, st.layout.commands.join(format!("{}_{id}", k.to_ascii_lowercase()))))
            .collect();
        {
            let (file, body, paths, wd) = (file.clone(), body.clone(), files.iter().map(|(_, p)| p.clone()).collect::<Vec<_>>(), wd.to_path_buf());
            crate::rt::blocking(move || -> std::io::Result<()> {
                if let Some(f) = &file {
                    std::fs::write(f, body)?;
                }
                for p in paths {
                    std::fs::write(p, b"")?;
                }
                std::fs::create_dir_all(wd)
            })
            .await??;
        }
        for (k, v) in self.env_vars(st) {
            env.entry(k).or_insert(v);
        }
        for (k, p) in &files {
            env.insert((*k).to_owned(), p.to_string_lossy().to_string());
        }
        if let Some((_, p)) = files.iter().find(|(k, _)| *k == "GITHUB_OUTPUT") {
            env.insert("KILN_OUTPUT".into(), p.to_string_lossy().to_string());
        }
        env.insert("KILN_LEG".into(), self.a.job_id.clone());
        let sep = if cfg!(windows) { ';' } else { ':' };
        let base_path = std::env::var("PATH").unwrap_or_default();
        env.insert("PATH".into(), st.path.apply(env.get("PATH").map_or(base_path.as_str(), String::as_str), sep));
        let mut container_run: Option<(super::docker::Cli, String, Option<String>, Vec<String>)> = None;
        let (prog, args) = match (what, &file) {
            (Run::Script { .. }, Some(f)) => shell_command(shell, f).map_err(|e| anyhow::anyhow!(e))?,
            (Run::Program { prog, args }, _) => (prog, args),
            (Run::Container { cli, image, entrypoint, args }, _) => {
                container_run = Some((cli, image, entrypoint, args));
                (String::new(), Vec::new())
            }
            (Run::Script { .. }, None) => anyhow::bail!("no script"),
        };
        let job_container = if in_job { st.containers.as_ref().and_then(|c| c.job.as_ref().map(|j| (c.cli.clone(), j.clone()))) } else { None };
        let cmd = if let Some((cli, image, entrypoint, args)) = container_run {
            // A Docker action: GitHub's mounts, every value by name.
            let map = self.action_paths(st);
            let mut inner: BTreeMap<String, String> = env.iter().map(|(k, v)| (k.clone(), map.translate(v))).collect();
            inner.remove("PATH");
            inner.insert("HOME".into(), "/github/home".into());
            inner.insert("GITHUB_WORKSPACE".into(), "/github/workspace".into());
            let network = st.containers.as_ref().map(|c| c.network.clone());
            let label = format!("kiln.leg={}", self.a.job_id);
            let name = format!("kiln_{}_{}", self.a.job_id, id.get(id.len().saturating_sub(8)..).unwrap_or(&id));
            let run_args = super::docker::action_run_args(&name, &label, network.as_deref(), &map.mounts(), &inner, entrypoint.as_deref(), &image, &args);
            let mut c = Command::new(&cli.bin);
            c.args(&run_args).envs(&inner);
            c
        } else if let Some((cli, j)) = job_container {
            // A job container's step: `docker exec`, paths translated (W13-R08).
            let mut inner: BTreeMap<String, String> = env.iter().map(|(k, v)| (k.clone(), j.map.translate(v))).collect();
            inner.insert("PATH".into(), st.path.apply(&j.path, ':'));
            inner.insert("HOME".into(), j.home.clone());
            let args: Vec<String> = args.iter().map(|a| j.map.translate(a)).collect();
            cli.exec_command(&j.id, j.user.as_deref(), &j.map.translate(&wd.to_string_lossy()), &inner, &j.map.translate(&prog), &args)
        } else {
            let mut c = Command::new(&prog);
            c.args(&args).current_dir(wd).envs(&env);
            c
        };
        let mut running = crate::proc::spawn_group(cmd)?;
        st.groups.push(running.pgid);
        let mut r = Reading { proc: Processor::new(st.allow_unsecure), line: Vec::new(), set_output: BTreeMap::new(), matchers: st.matchers.step(), failed: false };
        let mut flush = Interval::new(Duration::from_millis(100));
        let mut end: Option<End> = None;
        let mut output_done = false;
        // A background step's `cancel:` (W06-R17).
        let stop = st.stop.clone();
        let stopped = async {
            match &stop {
                Some(s) => s.cancelled().await,
                None => futures::future::pending().await,
            }
        }
        .fuse();
        futures::pin_mut!(stopped);
        loop {
            if output_done && end.is_some() {
                break;
            }
            if end.is_some() {
                // Daemons keep the pipes open: drain briefly once the step itself exited.
                match crate::rt::timeout(Duration::from_secs(2), running.output.recv()).await {
                    Ok(Ok(c)) => self.feed(st, idx, &mut r, &c).await,
                    Ok(Err(_)) | Err(_) => output_done = true,
                }
                continue;
            }
            enum Ev {
                Chunk(Option<Vec<u8>>),
                Exit(Option<i32>),
                Flush,
                Cancel,
                Stop,
                Deadline,
            }
            // Cleanup isn't cut short by the cancel it may follow.
            let cleanup = st.cleanup;
            let cancelled = std::pin::pin!(async {
                if cleanup {
                    futures::future::pending::<()>().await;
                }
                self.cancel.cancelled().await;
            });
            let ev = futures::select_biased! {
                c = recv_if(&running.output, !output_done).fuse() => Ev::Chunk(c),
                code = (&mut running.exit).fuse() => Ev::Exit(code.ok().flatten()),
                () = cancelled.fuse() => Ev::Cancel,
                () = stopped => Ev::Stop,
                () = crate::rt::sleep_until(deadline).fuse() => Ev::Deadline,
                () = flush.tick().fuse() => Ev::Flush,
            };
            match ev {
                Ev::Chunk(Some(c)) => self.feed(st, idx, &mut r, &c).await,
                Ev::Chunk(None) => output_done = true,
                Ev::Exit(code) => end = Some(End::Exit(code)),
                Ev::Flush => self.partial(st, idx, &mut r).await,
                Ev::Cancel => {
                    crate::proc::interrupt_group(&mut running, SIGINT_GRACE, SIGTERM_GRACE).await;
                    end = Some(End::Cancelled);
                }
                Ev::Stop => {
                    crate::proc::terminate_group(&mut running, BACKGROUND_GRACE).await;
                    end = Some(End::Cancelled);
                }
                Ev::Deadline => {
                    crate::proc::interrupt_group(&mut running, SIGINT_GRACE, SIGTERM_GRACE).await;
                    end = Some(End::TimedOut);
                }
            }
        }
        // A last line without its newline is a line still.
        if !r.line.is_empty() {
            let rest = std::mem::take(&mut r.line);
            self.whole(st, idx, &mut r, rest).await;
        }
        let tail = st.masker.finish();
        self.log(idx, tail).await;
        // The environment files (W06-R07).
        let read = |p: &PathBuf| std::fs::read_to_string(p).unwrap_or_default();
        let get = |k: &str| files.iter().find(|(x, _)| *x == k).map(|(_, p)| p.clone()).unwrap_or_default();
        let (envf, outf, pathf, statef, sumf, jsonf, runf) = (get("GITHUB_ENV"), get("GITHUB_OUTPUT"), get("GITHUB_PATH"), get("GITHUB_STATE"), get("GITHUB_STEP_SUMMARY"), get("KILN_OUTPUT_JSON"), get("KILN_RUN_ENV"));
        let texts = crate::rt::blocking(move || (read(&envf), read(&outf), read(&pathf), read(&statef), read(&sumf), read(&jsonf), read(&runf))).await?;
        let (env_t, out_t, path_t, state_t, sum_t, json_t, run_t) = texts;
        let mut outputs = std::mem::take(&mut r.set_output);
        let mut failed_files = false;
        // kiln's typed outputs (W04-R20): `{"name": …, "value": <any JSON>}` per line.
        for (n, line) in json_t.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
            let parsed: Result<serde_json::Value, _> = serde_json::from_str(line.trim());
            match parsed.ok().and_then(|v| Some((v.get("name")?.as_str()?.to_owned(), v.get("value")?.clone()))) {
                Some((name, value)) => {
                    outputs.insert(name.clone(), json_text(&value));
                    st.step_json.insert(name, value);
                }
                None => {
                    self.annotate(idx, "error", &format!("$KILN_OUTPUT_JSON line {}: expected {{\"name\": …, \"value\": …}}", n + 1)).await;
                    failed_files = true;
                }
            }
        }
        // `run.env` (W04-R21): name=value lines, as `$GITHUB_ENV` takes them.
        match envfiles::env(&run_t) {
            Ok(pairs) => st.run_env.extend(pairs),
            Err(e) => {
                self.annotate(idx, "error", &format!("$KILN_RUN_ENV: {e}")).await;
                failed_files = true;
            }
        }
        match envfiles::env(&env_t) {
            Ok(pairs) => {
                for (k, v) in pairs {
                    st.env.insert(k, v);
                }
            }
            Err(e) => {
                self.annotate(idx, "error", &e).await;
                failed_files = true;
            }
        }
        match envfiles::pairs(&out_t) {
            Ok(pairs) => outputs.extend(pairs.into_iter().map(|(k, v)| (k, st.unfake(&v)))),
            Err(e) => {
                self.annotate(idx, "error", &e).await;
                failed_files = true;
            }
        }
        for d in envfiles::path_lines(&path_t) {
            st.path.add(&d);
        }
        if !sum_t.is_empty() {
            match envfiles::summary(&sum_t) {
                Ok(md) => self.event(&JobEventV2::Summary { idx, markdown: st.masker.mask(&st.unfake(md)) }).await,
                Err(e) => self.annotate(idx, "warning", &e).await,
            }
        }
        if let Some(f) = file {
            let _ = crate::rt::blocking(move || std::fs::remove_file(f)).await;
        }
        let state = envfiles::pairs(&state_t).unwrap_or_default();
        let end = end.unwrap_or(End::Exit(None));
        let end = if (failed_files || r.failed) && end == End::Exit(Some(0)) { End::Exit(Some(1)) } else { end };
        Ok(Ran { end, outputs, state })
    }

    /// Output from the step, line by line.
    async fn feed(&self, st: &mut State, idx: u32, r: &mut Reading, chunk: &[u8]) {
        for &b in chunk {
            r.line.push(b);
            if b == b'\n' {
                let whole = std::mem::take(&mut r.line);
                self.whole(st, idx, r, whole).await;
            }
        }
    }

    /// A whole line: a command, else an issue a problem matcher makes of it, else text (masked
    /// and logged).
    async fn whole(&self, st: &mut State, idx: u32, r: &mut Reading, whole: Vec<u8>) {
        let whole = st.unfake_bytes(whole);
        let text = String::from_utf8_lossy(&whole).to_string();
        match r.proc.line(&text) {
            Line::Text | Line::Unknown => match r.matchers.line(&text) {
                Some(issue) => self.issue(st, idx, issue).await,
                None => self.text(st, idx, &whole).await,
            },
            Line::Refused(m) => {
                self.annotate(idx, "error", &format!("Unable to process command '{}' successfully.", st.masker.mask(text.trim_end()))).await;
                self.annotate(idx, "error", &m).await;
                r.failed = true;
            }
            Line::Command(c) => self.command(st, idx, c, r).await,
        }
    }

    /// A problem matcher's issue, logged and annotated as a workflow command's would be. Its file
    /// is kept when it is one of the repository's, relative to the workspace.
    async fn issue(&self, st: &mut State, idx: u32, i: matchers::Issue) {
        let message = st.masker.mask(&i.message);
        let color = match i.level {
            "error" => "31",
            "warning" => "33",
            _ => "36",
        };
        self.text(st, idx, format!("\x1b[{color}m{}: {message}\x1b[0m\n", i.level).as_bytes()).await;
        let file = match i.file.as_deref() {
            Some(f) => self.issue_file(st, f, i.from_path.as_deref()).await,
            None => None,
        };
        self.event(&JobEventV2::Annotation { idx, level: i.level.to_owned(), message, title: None, file, line: i.line, end_line: i.end_line, col: i.col, end_col: i.end_col, log_offset: 0 }).await;
    }

    /// A matched file as the repository has it: rooted in `from_path`'s directory, else the
    /// workspace; a job container's path is the host's first.
    async fn issue_file(&self, st: &State, file: &str, from_path: Option<&str>) -> Option<String> {
        let mut p = PathBuf::from(file);
        if p.is_relative()
            && let Some(dir) = from_path.and_then(|f| Path::new(f).parent()).filter(|d| !d.as_os_str().is_empty())
        {
            p = dir.join(p);
        }
        let ws = st.layout.workspace.clone();
        let p = match st.containers.as_ref().and_then(|c| c.job.as_ref()) {
            _ if p.is_relative() => ws.join(p),
            Some(j) => j.map.to_host(&p.to_string_lossy())?,
            None => p,
        };
        let p = lexical(&p);
        let rel = p.strip_prefix(&ws).ok()?.to_string_lossy().replace('\\', "/");
        crate::rt::blocking(move || p.is_file()).await.ok().filter(|f| *f).map(|_| rel)
    }

    /// The problem matchers of a matcher file a step names: a job container's path is the
    /// host's first, and a relative one is the workspace's.
    async fn load_matchers(&self, st: &State, file: &str) -> Result<Vec<matchers::Config>, String> {
        let p = match st.containers.as_ref().and_then(|c| c.job.as_ref()) {
            Some(j) if Path::new(file).is_absolute() => j.map.to_host(file).unwrap_or_else(|| PathBuf::from(file)),
            _ if Path::new(file).is_relative() => st.layout.workspace.join(file),
            _ => PathBuf::from(file),
        };
        let shown = p.display().to_string();
        let text = crate::rt::blocking(move || std::fs::read_to_string(&p)).await.map_err(|e| e.to_string())?.map_err(|e| format!("Could not read the matcher file '{shown}': {e}"))?;
        matchers::parse(&text)
    }

    /// A line still being written: shown now unless it may be a command, or problem matchers
    /// need it whole.
    async fn partial(&self, st: &mut State, idx: u32, r: &mut Reading) {
        if r.line.is_empty() || (!st.matchers.is_empty() && r.line.len() < 64 << 10) {
            return;
        }
        let line = &mut r.line;
        let head = String::from_utf8_lossy(line.get(..line.len().min(4)).unwrap_or_default()).to_string();
        let t = head.trim_start();
        if t.starts_with("::") || t.starts_with("##") || t.is_empty() || ":#".starts_with(t) {
            return;
        }
        let rest = st.unfake_bytes(std::mem::take(line));
        self.text(st, idx, &rest).await;
    }

    async fn text(&self, st: &mut State, idx: u32, data: &[u8]) {
        // Pipes give LF; the log viewer is a terminal.
        let mut crlf = Vec::with_capacity(data.len() + 8);
        for &b in data {
            if b == b'\n' && crlf.last() != Some(&b'\r') {
                crlf.push(b'\r');
            }
            crlf.push(b);
        }
        let out = st.masker.push(&crlf);
        self.log(idx, out).await;
    }

    async fn command(&self, st: &mut State, idx: u32, c: commands::Command, r: &mut Reading) {
        use kiln_ci::runner::commands::Command as C;
        match c {
            C::AddMask(v) => {
                if st.masker.add(&v) {
                    st.masks_added.push(v.clone());
                    self.event(&JobEventV2::Mask { value_sha256: crate::util::sha256_hex(v.as_bytes()) }).await;
                }
            }
            C::Group(title) => {
                let t = st.masker.mask(&title);
                self.text(st, idx, format!("\x1b[1m▸ {t}\x1b[0m\n").as_bytes()).await;
                self.event(&JobEventV2::Group { idx, op: "start".into(), title: Some(t), log_offset: 0 }).await;
            }
            C::EndGroup => self.event(&JobEventV2::Group { idx, op: "end".into(), title: None, log_offset: 0 }).await,
            C::Debug(m) => {
                if st.step_debug {
                    let m = st.masker.mask(&m);
                    self.text(st, idx, format!("\x1b[35m##[debug]{m}\x1b[0m\n").as_bytes()).await;
                }
            }
            C::Annotation(a) => {
                let message = st.masker.mask(&a.message);
                let color = match a.level.as_str() {
                    "error" => "31",
                    "warning" => "33",
                    _ => "36",
                };
                self.text(st, idx, format!("\x1b[{color}m{}: {message}\x1b[0m\n", a.level).as_bytes()).await;
                self.event(&JobEventV2::Annotation { idx, level: a.level, message, title: a.title.map(|t| st.masker.mask(&t)), file: a.file, line: a.line, end_line: a.end_line, col: a.col, end_col: a.end_col, log_offset: 0 }).await;
            }
            C::StopCommands(_) | C::Echo(_) => {}
            C::AddMatcher(file) if file.trim().is_empty() => self.annotate(idx, "warning", "File path must be specified.").await,
            C::AddMatcher(file) => match self.load_matchers(st, file.trim()).await {
                Ok(configs) => {
                    let (said, added) = st.matchers.add(configs);
                    r.matchers.added(&added);
                    self.text(st, idx, format!("{said}\n").as_bytes()).await;
                }
                Err(e) => {
                    self.annotate(idx, "error", &format!("Unable to process command '::add-matcher::{file}' successfully.")).await;
                    self.annotate(idx, "error", &e).await;
                    r.failed = true;
                }
            },
            C::RemoveMatcher { owner, file } => {
                let owners = match (owner.is_empty(), file.is_empty()) {
                    (false, false) => return self.annotate(idx, "warning", "Either specify an owner name or a file path in ##[remove-matcher] command. Both values cannot be set.").await,
                    (true, true) => return self.annotate(idx, "warning", "Either an owner name or a file path must be specified in ##[remove-matcher] command.").await,
                    (false, true) => vec![owner],
                    (true, false) => match self.load_matchers(st, &file).await {
                        Ok(configs) => configs.into_iter().map(|c| c.owner).collect(),
                        Err(e) => {
                            self.annotate(idx, "error", &format!("Unable to process command '::remove-matcher::{file}' successfully.")).await;
                            self.annotate(idx, "error", &e).await;
                            r.failed = true;
                            return;
                        }
                    },
                };
                let (said, gone) = st.matchers.remove(&owners);
                r.matchers.removed(&gone);
                self.text(st, idx, format!("{said}\n").as_bytes()).await;
            }
            C::SetOutput { name, value } => {
                self.annotate(idx, "warning", "The `set-output` command is deprecated and will be disabled soon. Please upgrade to using Environment Files.").await;
                r.set_output.insert(name, value);
            }
            C::SaveState { .. } => {
                self.annotate(idx, "warning", "The `save-state` command is deprecated and will be disabled soon. Please upgrade to using Environment Files.").await;
            }
            C::SetEnv { name, value } => {
                st.env.insert(name, value);
            }
            C::AddPath(p) => st.path.add(&p),
        }
    }

    /// The job's outputs, after its steps (W06-R10); one holding a secret is not set, as GitHub
    /// doesn't.
    async fn job_outputs(&self, st: &State) {
        let a = &self.a;
        if a.outputs.is_empty() && a.exports_env.is_empty() && !(a.run_env && !st.run_env.is_empty()) && a.environment_url.is_none() {
            return;
        }
        // `environment.url` (W15-R04): its deployment's URL, as the steps left things.
        let environment_url = match &a.environment_url {
            Some(t) => match st.render(t, Site::JobOutputs, "environment.url") {
                Ok(v) if st.masker.mask(&v) != v => {
                    self.annotate(0, "warning", "Skip the environment's url since it may contain secret.").await;
                    None
                }
                Ok(v) => Some(v).filter(|v| !v.is_empty()),
                Err(e) => {
                    self.annotate(0, "warning", &e).await;
                    None
                }
            },
            None => None,
        };
        let (mut outputs, mut json_outputs, mut secret_values) = (BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
        for (k, t) in &a.outputs {
            let what = format!("outputs.{k}");
            // A secret output (W04-R22) keeps its value: it goes to the server to be sealed.
            if a.secret_outputs.contains(k) {
                match st.render(t, Site::JobOutputs, &what) {
                    Ok(v) if !v.is_empty() => {
                        secret_values.insert(k.clone(), v);
                    }
                    Ok(_) => {}
                    Err(e) => self.annotate(0, "warning", &e).await,
                }
                continue;
            }
            match st.render(t, Site::JobOutputs, &what) {
                Ok(v) if st.masker.mask(&v) != v => self.annotate(0, "warning", &format!("Skip output '{k}' since it may contain secret.")).await,
                Ok(v) => {
                    // kiln: a typed value stays typed for kiln consumers (W04-R20).
                    if st.dialect == Dialect::Kiln
                        && let Ok(tpl) = Template::parse_with(&t.src, &options(Site::JobOutputs, st.dialect))
                        && tpl.single_expr().is_some()
                        && let Ok(val) = expr::eval_value(&tpl, &st.cx(), &mut Budget::default())
                        && !matches!(val, Value::String(_) | Value::Null)
                    {
                        json_outputs.insert(k.clone(), val.to_json());
                    }
                    outputs.insert(k.clone(), v);
                }
                Err(e) => self.annotate(0, "warning", &e).await,
            }
        }
        // `exports: { env }` (W04-R21): the final values, masked ones withheld.
        let mut env = BTreeMap::new();
        for name in &a.exports_env {
            match st.env.get(name) {
                Some(v) if st.masker.mask(v) != *v => self.annotate(0, "warning", &format!("Skip exporting '{name}' since it may contain secret.")).await,
                Some(v) => {
                    env.insert(name.clone(), v.clone());
                }
                None => {}
            }
        }
        let run_env = if a.run_env { st.run_env.iter().filter(|(_, v)| st.masker.mask(v) == **v).cloned().collect() } else { Vec::new() };
        let secret_outputs = secret_values.keys().cloned().collect();
        self.event(&JobEventV2::JobOutputs { outputs, json_outputs, env, run_env, secret_outputs, secret_values, environment_url }).await;
    }
}

/// GitHub's input variable: `INPUT_<NAME>`, upper-cased, spaces as `_` (hyphens kept).
fn input_var(name: &str) -> String {
    format!("INPUT_{}", name.replace(' ', "_").to_uppercase())
}

/// The action's `GITHUB_ACTION` when its step has no `id`: `__owner_repo`, as GitHub names it.
fn action_name(uses: &str) -> String {
    let base = uses.split('@').next().unwrap_or(uses);
    format!("__{}", base.trim_start_matches("./").replace(['/', '.', '-'], "_"))
}

/// Reads an action's metadata (`action.yml` or `action.yaml`, W02-R10).
fn load_action(dir: &Path) -> anyhow::Result<kiln_ci::wf::Action> {
    let file = ["action.yml", "action.yaml"].iter().map(|n| dir.join(n)).find(|p| p.exists()).ok_or_else(|| anyhow::anyhow!("no action.yml in {}", dir.display()))?;
    let src = std::fs::read_to_string(&file)?;
    let (a, d) = kiln_ci::wf::parse_action(&src);
    if let Some(e) = d.iter().find(|x| x.is_error()) {
        anyhow::bail!("{}", e.render(&file.to_string_lossy()));
    }
    a.ok_or_else(|| anyhow::anyhow!("{} is not an action's metadata", file.display()))
}

/// One Node runtime download at a time (they share `<work>/_externals`).
static NODE_LOCK: async_lock::Mutex<()> = async_lock::Mutex::new(());
static ACTION_LOCK: async_lock::Mutex<()> = async_lock::Mutex::new(());

impl Leg {
    /// Where an action is: in the workspace (`./`), or fetched at its pinned commit.
    async fn locate(&self, uses: &str) -> anyhow::Result<Located> {
        if let Some(rel) = uses.strip_prefix("./") {
            let dir = Layout::new(&self.work_dir, &self.a.job_id, &self.a.repo, None).workspace.join(rel);
            return Ok(Located { dir, repo: String::new(), git_ref: String::new() });
        }
        if uses.starts_with("docker://") {
            anyhow::bail!("`{uses}`: Docker actions run with Docker executors (docs/ci.md W13); this runner can't run them yet");
        }
        // Remote actions, and `$/` ones (this repository at the running commit, W07-R01): as the
        // run's start resolved them.
        let r = self.a.actions.get(uses).ok_or_else(|| anyhow::anyhow!("`{uses}` was not resolved when the run started"))?;
        let root = self.fetch_action(r).await?;
        let dir = if r.path.is_empty() { root } else { root.join(&r.path) };
        if uses.starts_with("$/") {
            return Ok(Located { dir, repo: String::new(), git_ref: String::new() });
        }
        let git_ref = uses.rsplit_once('@').map(|(_, r)| r.to_owned()).unwrap_or_default();
        Ok(Located { dir, repo: r.repo.clone(), git_ref })
    }

    /// An action's code at its commit, kept under `<work>/_actions/<owner>/<repo>/<sha>`.
    async fn fetch_action(&self, r: &kiln_ci::wire::ActionRef) -> anyhow::Result<PathBuf> {
        // A `$/` action's tarball holds its directory only: kept apart by path.
        let key = if r.url.contains("?path=") {
            let d = crate::util::sha256_hex(r.path.as_bytes());
            format!("{}-{}", r.sha, d.get(..12).unwrap_or(&d))
        } else {
            r.sha.clone()
        };
        let dir = self.work_dir.join("_actions").join(r.repo.replace("..", "_")).join(key);
        let done = dir.join(".kiln-complete");
        if done.exists() {
            return Ok(dir);
        }
        let _g = ACTION_LOCK.lock().await;
        if done.exists() {
            return Ok(dir);
        }
        let mut resp = self.http.get(&r.url).bearer(&self.a.runtime.runtime_token).timeout(Duration::from_secs(300)).send().await?;
        if !resp.is_success() {
            anyhow::bail!("downloading {}@{}: HTTP {}", r.repo, r.sha, resp.status());
        }
        let mut bytes = Vec::new();
        while let Some(c) = resp.chunk().await? {
            bytes.extend_from_slice(&c);
            anyhow::ensure!(bytes.len() < 1 << 30, "the action is over 1 GiB");
        }
        let d = dir.clone();
        crate::rt::blocking(move || -> anyhow::Result<()> {
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d)?;
            tar::Archive::new(flate2::read::GzDecoder::new(bytes.as_slice())).unpack(&d)?;
            std::fs::write(d.join(".kiln-complete"), b"")?;
            Ok(())
        })
        .await??;
        Ok(dir)
    }

    /// A Node runtime for `using` (W07-R05): `node24` on Node 24; `node20`, `node16` and
    /// `node12` on Node 20, as GitHub forces. Official builds from nodejs.org, checked against
    /// their `SHASUMS256.txt`, kept in `<work>/_externals`.
    async fn node(&self, using: &str) -> anyhow::Result<PathBuf> {
        let major = if using == "node24" { 24 } else { 20 };
        let dir = self.work_dir.join("_externals").join(format!("node{major}"));
        let bin = if cfg!(windows) { dir.join("node.exe") } else { dir.join("bin").join("node") };
        if bin.exists() {
            return Ok(bin);
        }
        let _g = NODE_LOCK.lock().await;
        if bin.exists() {
            return Ok(bin);
        }
        let os = match std::env::consts::OS {
            "linux" => "linux",
            "macos" => "darwin",
            other => anyhow::bail!("no Node {major} build for {other} here yet"),
        };
        let arch = match std::env::consts::ARCH {
            "x86_64" => "x64",
            "aarch64" => "arm64",
            "arm" => "armv7l",
            other => anyhow::bail!("no Node {major} build for {other}"),
        };
        let base = format!("https://nodejs.org/dist/latest-v{major}.x");
        let sums = self.http.get(&format!("{base}/SHASUMS256.txt")).timeout(Duration::from_secs(60)).send().await?.bytes(1 << 20).await?;
        let sums = String::from_utf8_lossy(&sums).to_string();
        let suffix = format!("-{os}-{arch}.tar.gz");
        let (sha, file) = sums
            .lines()
            .filter_map(|l| l.split_once("  "))
            .find(|(_, f)| f.starts_with("node-v") && f.ends_with(&suffix))
            .map(|(h, f)| (h.trim().to_owned(), f.trim().to_owned()))
            .ok_or_else(|| anyhow::anyhow!("nodejs.org lists no {suffix} build of Node {major}"))?;
        let mut resp = self.http.get(&format!("{base}/{file}")).timeout(Duration::from_secs(600)).send().await?;
        anyhow::ensure!(resp.is_success(), "downloading {file}: HTTP {}", resp.status());
        let mut bytes = Vec::new();
        while let Some(c) = resp.chunk().await? {
            bytes.extend_from_slice(&c);
        }
        let got = {
            use sha2::Digest as _;
            hex::encode(sha2::Sha256::digest(&bytes))
        };
        anyhow::ensure!(got == sha, "{file}: SHA-256 {got}, SHASUMS256.txt says {sha}");
        let d = dir.clone();
        crate::rt::blocking(move || -> anyhow::Result<()> {
            let tmp = d.with_extension("partial");
            let _ = std::fs::remove_dir_all(&tmp);
            std::fs::create_dir_all(&tmp)?;
            let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(bytes.as_slice()));
            for entry in ar.entries()? {
                let mut entry = entry?;
                let path = entry.path()?.into_owned();
                // Strip `node-vX.Y.Z-os-arch/`.
                let rest: PathBuf = path.components().skip(1).collect();
                if rest.as_os_str().is_empty() {
                    continue;
                }
                entry.unpack(tmp.join(rest))?;
            }
            let _ = std::fs::remove_dir_all(&d);
            std::fs::rename(&tmp, &d)?;
            Ok(())
        })
        .await??;
        Ok(bin)
    }

    /// An action's inputs (W07-R05): `with`, else the default, evaluated as an expression; inputs
    /// the action doesn't declare are passed on with GitHub's warning.
    async fn inputs(&self, st: &State, idx: u32, action: &kiln_ci::wf::Action, with: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for i in &action.inputs {
            let given = with.iter().find(|(k, _)| k.eq_ignore_ascii_case(&i.name)).map(|(_, v)| v.clone());
            let v = match given {
                Some(v) => v,
                None => match &i.default {
                    Some(t) => st.render(t, Site::StepWith, &format!("inputs.{}", i.name)).unwrap_or_else(|_| t.src.clone()),
                    None => String::new(),
                },
            };
            out.insert(i.name.clone(), v);
        }
        let unexpected: Vec<&String> = with.keys().filter(|k| !action.inputs.iter().any(|i| i.name.eq_ignore_ascii_case(k))).collect();
        if !unexpected.is_empty() {
            let valid: Vec<String> = action.inputs.iter().map(|i| format!("'{}'", i.name)).collect();
            let names: Vec<String> = unexpected.iter().map(|k| format!("'{k}'")).collect();
            self.annotate(idx, "warning", &format!("Unexpected input(s) {}, valid inputs are [{}]", names.join(", "), valid.join(", "))).await;
            for k in unexpected {
                out.insert(k.clone(), with.get(k).cloned().unwrap_or_default());
            }
        }
        out
    }

    /// The container CLI for Docker actions: the leg's, or this machine's.
    async fn cli(&self, st: &State) -> anyhow::Result<super::docker::Cli> {
        if let Some(c) = &st.containers {
            return Ok(c.cli.clone());
        }
        let bin = crate::rt::blocking(super::docker::find).await?.ok_or_else(|| anyhow::anyhow!("Docker actions need Docker or Podman, and this runner has neither working"))?;
        let config = st.layout.temp.join("_docker_config");
        let c = config.clone();
        crate::rt::blocking(move || std::fs::create_dir_all(c)).await??;
        Ok(super::docker::Cli::new(&bin, config))
    }

    /// A `uses:` step (W07): JavaScript, composite, Docker.
    #[allow(clippy::too_many_arguments)]
    async fn action(&self, st: &mut State, idx: u32, step: &Step, uses: &str, with: BTreeMap<String, String>, wd: &Path, env: BTreeMap<String, String>, deadline: Instant) -> anyhow::Result<Ran> {
        // `uses: docker://image`: the image itself, with `with.entrypoint` and `with.args`.
        if uses.starts_with("docker://") {
            let cli = self.cli(st).await?;
            let said = parking_lot::Mutex::new(Vec::<String>::new());
            let image = super::docker::action_image(&cli, Path::new("."), uses, "", &|m: String| said.lock().push(m)).await?;
            let lines = std::mem::take(&mut *said.lock());
            for line in lines {
                self.log(idx, format!("{line}\r\n").into_bytes()).await;
            }
            let args = with.get("args").map(|a| super::docker::split_options(a)).unwrap_or_default();
            let entrypoint = with.get("entrypoint").cloned();
            return self.exec(st, idx, Run::Container { cli, image, entrypoint, args }, wd, env, deadline).await;
        }
        let loc = self.locate(uses).await?;
        let dir = loc.dir.clone();
        let action = crate::rt::blocking(move || load_action(&dir)).await??;
        let inputs = self.inputs(st, idx, &action, &with).await;
        let mut aenv = env;
        for (k, v) in &inputs {
            aenv.insert(input_var(k), v.clone());
        }
        aenv.insert("GITHUB_ACTION".into(), step.id.clone().unwrap_or_else(|| action_name(uses)));
        aenv.insert("GITHUB_ACTION_PATH".into(), loc.dir.to_string_lossy().to_string());
        aenv.insert("GITHUB_ACTION_REPOSITORY".into(), loc.repo.clone());
        aenv.insert("GITHUB_ACTION_REF".into(), loc.git_ref.clone());
        for (k, t) in &action.runs.env {
            aenv.insert(k.clone(), st.render(t, Site::StepEnv, &format!("runs.env.{k}")).unwrap_or_else(|_| t.src.clone()));
        }
        match action.runs.using.as_str() {
            "node12" | "node16" | "node20" | "node24" => {
                let node = self.node(&action.runs.using).await?;
                let main = action.runs.main.as_deref().ok_or_else(|| anyhow::anyhow!("`{uses}` has no `runs.main`"))?;
                let script = loc.dir.join(main);
                let (dir, scripts) = (loc.dir.clone(), [Some(main.to_owned()), action.runs.post.clone()].into_iter().flatten().collect::<Vec<_>>());
                if crate::rt::blocking(move || refuses_ghes(&dir, &scripts.iter().map(String::as_str).collect::<Vec<_>>())).await.unwrap_or(false) {
                    aenv.insert("GITHUB_SERVER_URL".into(), NOT_GHES.into());
                }
                let ran = self.exec(st, idx, Run::Program { prog: node.to_string_lossy().to_string(), args: vec![script.to_string_lossy().to_string()] }, wd, aenv.clone(), deadline).await?;
                if let Some(post) = &action.runs.post {
                    let mut penv = aenv;
                    for (k, v) in &ran.state {
                        penv.insert(format!("STATE_{k}"), v.clone());
                    }
                    let name = action.name.clone().unwrap_or_else(|| uses.to_owned());
                    st.posts.push(Post { name: format!("Post {name}"), node, script: loc.dir.join(post), env: penv, cond: action.runs.post_if.clone(), wd: wd.to_path_buf(), uses: uses.to_owned() });
                }
                Ok(ran)
            }
            "composite" => Box::pin(self.composite(st, idx, &action, &loc, &inputs, aenv, deadline)).await,
            "docker" => {
                // A Docker action (W07-R06): its image built or pulled, then run with GitHub's mounts.
                let cli = self.cli(st).await?;
                let spec_image = action.runs.image.clone().ok_or_else(|| anyhow::anyhow!("`{uses}` has no `runs.image`"))?;
                let tag = format!("kiln-action/{}:{}", if loc.repo.is_empty() { "local".to_owned() } else { loc.repo.to_ascii_lowercase().replace('/', "-") }, crate::util::sha256_hex(loc.dir.to_string_lossy().as_bytes()).get(..12).unwrap_or("latest"));
                let said = parking_lot::Mutex::new(Vec::<String>::new());
                let image = super::docker::action_image(&cli, &loc.dir, &spec_image, &tag, &|m: String| said.lock().push(m)).await;
                let lines = std::mem::take(&mut *said.lock());
                for line in lines {
                    self.log(idx, format!("{line}\r\n").into_bytes()).await;
                }
                let image = image?;
                // `runs.args` see the action's own `inputs`.
                let saved = st.base.clone();
                st.base.retain(|(k, _)| *k != "inputs");
                st.base.push(("inputs", str_map(&inputs)));
                let args: Result<Vec<String>, String> = action.runs.args.iter().map(|t| st.render(t, Site::StepWith, "runs.args")).collect();
                st.base = saved;
                let args = args.map_err(anyhow::Error::msg)?;
                if let Some(pre) = action.runs.pre_entrypoint.clone().filter(|e| !e.is_empty()) {
                    let r = self.exec(st, idx, Run::Container { cli: cli.clone(), image: image.clone(), entrypoint: Some(pre), args: Vec::new() }, wd, aenv.clone(), deadline).await?;
                    if r.end != End::Exit(Some(0)) {
                        return Ok(r);
                    }
                }
                let ran = self.exec(st, idx, Run::Container { cli: cli.clone(), image: image.clone(), entrypoint: action.runs.entrypoint.clone(), args }, wd, aenv.clone(), deadline).await?;
                if let Some(post) = action.runs.post_entrypoint.clone().filter(|e| !e.is_empty()) {
                    let r = self.exec(st, idx, Run::Container { cli, image, entrypoint: Some(post), args: Vec::new() }, wd, aenv, deadline).await?;
                    if r.end != End::Exit(Some(0)) {
                        return Ok(r);
                    }
                }
                Ok(ran)
            }
            other => anyhow::bail!("`runs.using: {other}` is no kind of action this runner knows"),
        }
    }

    /// A composite action (W07-R07): its steps run in this leg, with their own `inputs` and
    /// `steps` contexts and `github.action_path`; its outputs map its steps' outputs.
    #[allow(clippy::too_many_arguments)]
    async fn composite(&self, st: &mut State, idx: u32, action: &kiln_ci::wf::Action, loc: &Located, inputs: &BTreeMap<String, String>, env: BTreeMap<String, String>, deadline: Instant) -> anyhow::Result<Ran> {
        let saved_steps = std::mem::take(&mut st.steps);
        let saved_base = st.base.clone();
        let saved_github = st.github.clone();
        let saved_failed = st.failed;
        let job_env = std::mem::replace(&mut st.env, env.clone());
        st.base.retain(|(k, _)| *k != "inputs");
        st.base.push(("inputs", str_map(inputs)));
        if let Value::Object(m) = &st.github {
            let mut members: Vec<(String, Value)> = m.0.iter().filter(|(k, _)| &**k != "action_path").map(|(k, v)| (k.to_string(), v.clone())).collect();
            members.push(("action_path".to_owned(), Value::str(&loc.dir.to_string_lossy())));
            st.github = Value::object(members);
        }
        st.failed = false;
        let mut end = End::Exit(Some(0));
        for s in &action.runs.steps {
            if let Some(e @ (End::Cancelled | End::TimedOut)) = Box::pin(self.step(st, idx, s, deadline, true)).await {
                end = e;
            }
        }
        if st.failed && end == End::Exit(Some(0)) {
            end = End::Exit(Some(1));
        }
        let mut outputs = BTreeMap::new();
        for o in &action.outputs {
            if let Some(v) = &o.value {
                match st.render(v, Site::StepRun, &format!("outputs.{}", o.name)) {
                    Ok(x) => {
                        outputs.insert(o.name.clone(), x);
                    }
                    Err(e) => self.annotate(idx, "warning", &e).await,
                }
            }
        }
        // `$GITHUB_ENV` written inside reaches the job's later steps, as on GitHub.
        let mut merged = job_env;
        for (k, v) in &st.env {
            if env.get(k) != Some(v) {
                merged.insert(k.clone(), v.clone());
            }
        }
        st.env = merged;
        st.steps = saved_steps;
        st.base = saved_base;
        st.github = saved_github;
        st.failed = saved_failed;
        Ok(Ran { end, outputs, state: Vec::new() })
    }

    /// Pre steps (W06-R11): every remote JavaScript action's `pre`, before the job's steps, in
    /// step order, under `pre-if` (default `always()`).
    async fn pre_steps(&self, st: &mut State, deadline: Instant) {
        for step in &self.a.steps {
            let Some(uses) = &step.uses else { continue };
            if uses.starts_with("./") || uses.starts_with("docker://") || !self.a.actions.contains_key(uses) {
                continue;
            }
            let Ok(loc) = self.locate(uses).await else { continue };
            let dir = loc.dir.clone();
            let Ok(Ok(action)) = crate::rt::blocking(move || load_action(&dir)).await else { continue };
            let Some(pre) = &action.runs.pre else { continue };
            if !action.runs.using.starts_with("node") {
                continue;
            }
            let run = match &action.runs.pre_if {
                Some(t) => st.condition(Some(t)).unwrap_or(false),
                None => true,
            };
            let idx = st.next_idx;
            st.next_idx += 1;
            let name = format!("Pre {}", action.name.clone().unwrap_or_else(|| uses.clone()));
            self.start(idx, &name, "pre", Some(uses.clone())).await;
            if !run {
                self.event(&JobEventV2::StepFinished { idx, outcome: Conclusion::Skipped, conclusion: Conclusion::Skipped, exit_code: None, outputs: BTreeMap::new() }).await;
                continue;
            }
            let with: BTreeMap<String, String> = step.with.iter().filter_map(|(k, v)| st.value(v, Site::StepWith, k).ok().map(|x| (k.clone(), x.coerce_string()))).collect();
            let inputs = self.inputs(st, idx, &action, &with).await;
            let mut env = st.env.clone();
            for (k, v) in &inputs {
                env.insert(input_var(k), v.clone());
            }
            env.insert("GITHUB_ACTION_PATH".into(), loc.dir.to_string_lossy().to_string());
            env.insert("GITHUB_ACTION_REPOSITORY".into(), loc.repo.clone());
            env.insert("GITHUB_ACTION_REF".into(), loc.git_ref.clone());
            let (dir, script) = (loc.dir.clone(), pre.clone());
            if crate::rt::blocking(move || refuses_ghes(&dir, &[script.as_str()])).await.unwrap_or(false) {
                env.insert("GITHUB_SERVER_URL".into(), NOT_GHES.into());
            }
            let r = match self.node(&action.runs.using).await {
                Ok(node) => self.exec(st, idx, Run::Program { prog: node.to_string_lossy().to_string(), args: vec![loc.dir.join(pre).to_string_lossy().to_string()] }, &st.layout.workspace.clone(), env, deadline).await,
                Err(e) => Err(e),
            };
            let ok = matches!(r, Ok(Ran { end: End::Exit(Some(0)), .. }));
            if let Err(e) = &r {
                self.note(idx, &format!("Error: {e:#}")).await;
            }
            if !ok {
                st.failed = true;
            }
            let c = if ok { Conclusion::Success } else { Conclusion::Failure };
            self.event(&JobEventV2::StepFinished { idx, outcome: c, conclusion: c, exit_code: None, outputs: BTreeMap::new() }).await;
        }
    }

    /// Post steps (W06-R11): in reverse order of the actions that ran, under `post-if` (default
    /// `always()`), with the `STATE_*` their main step saved.
    async fn post_steps(&self, st: &mut State) {
        let posts = std::mem::take(&mut st.posts);
        st.cleanup = true;
        let deadline = Instant::now() + CLEANUP_TIME;
        for p in posts.into_iter().rev() {
            let run = match &p.cond {
                Some(t) => st.condition(Some(t)).unwrap_or(false),
                None => true,
            };
            let idx = st.next_idx;
            st.next_idx += 1;
            self.start(idx, &p.name, "post", Some(p.uses.clone())).await;
            if !run {
                self.event(&JobEventV2::StepFinished { idx, outcome: Conclusion::Skipped, conclusion: Conclusion::Skipped, exit_code: None, outputs: BTreeMap::new() }).await;
                continue;
            }
            let r = self.exec(st, idx, Run::Program { prog: p.node.to_string_lossy().to_string(), args: vec![p.script.to_string_lossy().to_string()] }, &p.wd, p.env, deadline).await;
            let ok = matches!(r, Ok(Ran { end: End::Exit(Some(0)), .. }));
            if let Err(e) = &r {
                self.note(idx, &format!("Error: {e:#}")).await;
            }
            if !ok {
                st.failed = true;
            }
            let c = if ok { Conclusion::Success } else { Conclusion::Failure };
            self.event(&JobEventV2::StepFinished { idx, outcome: c, conclusion: c, exit_code: None, outputs: BTreeMap::new() }).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A leg run here, with what it sent: logs by step, and events. (Its tests run `bash` and
    /// `sh`, so they and their helpers are Unix-only.)
    #[cfg(unix)]
    fn run_leg(workflow: &str, secrets: &[(&str, &str)]) -> anyhow::Result<(JobResult, BTreeMap<u32, String>, Vec<JobEventV2>)> {
        run_leg_in(workflow, secrets, Dialect::Gha)
    }

    /// [`run_leg`] for a workflow of either dialect: a kiln one gets its job's kiln settings.
    #[cfg(unix)]
    fn run_leg_in(workflow: &str, secrets: &[(&str, &str)], dialect: Dialect) -> anyhow::Result<(JobResult, BTreeMap<u32, String>, Vec<JobEventV2>)> {
        run_leg_prepared(workflow, secrets, dialect, &|_| Ok(()))
    }

    /// [`run_leg_in`], with `prepare` setting up the work directory first.
    #[cfg(unix)]
    fn run_leg_prepared(workflow: &str, secrets: &[(&str, &str)], dialect: Dialect, prepare: &dyn Fn(&Path) -> anyhow::Result<()>) -> anyhow::Result<(JobResult, BTreeMap<u32, String>, Vec<JobEventV2>)> {
        let path = if dialect == Dialect::Kiln { ".kiln/t.yaml" } else { ".github/workflows/t.yml" };
        let (w, d) = kiln_ci::wf::parse_workflow(path, workflow, dialect);
        anyhow::ensure!(d.iter().all(|x| !x.is_error()), "{d:?}");
        let run_env = w.as_ref().is_some_and(|w| w.run_env);
        let job = w.and_then(|w| w.jobs.into_iter().next()).ok_or_else(|| anyhow::anyhow!("no job"))?;
        let tmp = tempfile::tempdir()?;
        prepare(tmp.path())?;
        let a = AssignmentV2 {
            job_id: "leg-1".into(),
            repo: "acme/app".into(),
            dialect: if dialect == Dialect::Kiln { "kiln".into() } else { "gha".into() },
            exports_env: job.kiln.exports_env.clone(),
            secret_outputs: job.outputs.iter().filter(|o| o.secret).map(|o| o.name.clone()).collect(),
            run_env,
            steps: job.steps.clone(),
            job_env: job.env.clone(),
            outputs: job.outputs.iter().map(|o| (o.name.clone(), o.value.clone())).collect(),
            contexts: kiln_ci::wire::Contexts { github: serde_json::json!({"event_name": "push", "ref": "refs/heads/main", "sha": "abc", "repository": "acme/app", "event": {}}), ..Default::default() },
            secrets: secrets.iter().map(|(n, v)| kiln_ci::wire::Secret { name: (*n).into(), value: (*v).into() }).collect(),
            timeout_minutes: 5,
            ..AssignmentV2::default()
        };
        use crate::exec::Up;
        let (tx, rx) = async_channel::unbounded();
        let leg = Leg { a, work_dir: tmp.path().to_path_buf(), runner_name: "test-runner".into(), up: Uplink(tx), cancel: Shutdown::new(), http: std::sync::Arc::new(crate::http::client::Client::public()?) };
        let result = crate::rt::block_on(async move { Ok(leg.run().await) })?;
        let (mut logs, mut events) = (BTreeMap::<u32, String>::new(), Vec::new());
        while let Ok(m) = rx.try_recv() {
            match m {
                Up::Log(c) => logs.entry(c.idx).or_default().push_str(&String::from_utf8_lossy(&c.data)),
                Up::Msg(AgentMsg::JobEvent { json, .. }) => events.push(serde_json::from_str(&json)?),
                _ => {}
            }
        }
        Ok((result, logs, events))
    }

    #[cfg(unix)]
    fn finished(events: &[JobEventV2], idx: u32) -> Option<(Conclusion, Conclusion, BTreeMap<String, String>)> {
        events.iter().find_map(|e| match e {
            JobEventV2::StepFinished { idx: i, outcome, conclusion, outputs, .. } if *i == idx => Some((*outcome, *conclusion, outputs.clone())),
            _ => None,
        })
    }

    /// W06-S01, S03, S04 and the environment files, on this machine.
    #[cfg(unix)]
    #[test]
    fn a_github_job_runs_as_github_runs_it() -> anyhow::Result<()> {
        let wf = r###"on: push
jobs:
  t:
    runs-on: x
    env:
      GREETING: hello
    outputs:
      x: ${{ steps.a.outputs.x }}
    steps:
      - id: a
        run: |
          echo "x=1" >> "$GITHUB_OUTPUT"
          echo "FROM_A=yes" >> "$GITHUB_ENV"
          echo "/opt/kiln-test-bin" >> "$GITHUB_PATH"
          echo "::add-mask::masked-value-123"
          echo "secret is ${{ secrets.TOKEN }} and masked-value-123"
          echo "::warning file=a.rs,line=1::careful"
          echo "## Summary" >> "$GITHUB_STEP_SUMMARY"
      - id: b
        run: test "$FROM_A" = yes && test "$GREETING" = hello && echo "b sees ${{ steps.a.outputs.x }} and $PATH"
      - id: c
        continue-on-error: true
        run: exit 3
      - if: failure()
        run: echo never
      - id: d
        run: echo "c was ${{ steps.c.outcome }}/${{ steps.c.conclusion }}"
"###;
        let (r, logs, events) = run_leg(wf, &[("TOKEN", "hunter2-long-secret")])?;
        assert_eq!(r.status, Status::Succeeded, "{r:?} {logs:?}");
        let all: String = logs.values().cloned().collect();
        assert!(!all.contains("hunter2-long-secret") && !all.contains("masked-value-123"), "{all}");
        assert!(logs.get(&0).is_some_and(|l| l.contains("secret is *** and ***")), "{logs:?}");
        assert_eq!(finished(&events, 0).map(|f| f.2.get("x").cloned()), Some(Some("1".into())));
        assert!(logs.get(&1).is_some_and(|l| l.contains("b sees 1 and /opt/kiln-test-bin:")), "{logs:?}");
        assert_eq!(finished(&events, 2).map(|f| (f.0, f.1)), Some((Conclusion::Failure, Conclusion::Success)), "continue-on-error");
        assert_eq!(finished(&events, 3).map(|f| f.1), Some(Conclusion::Skipped), "if: failure() after a step that continued");
        assert!(logs.get(&4).is_some_and(|l| l.contains("c was failure/success")), "{logs:?}");
        assert!(events.iter().any(|e| matches!(e, JobEventV2::Annotation { level, message, file, .. } if level == "warning" && message == "careful" && file.as_deref() == Some("a.rs"))));
        assert!(events.iter().any(|e| matches!(e, JobEventV2::Summary { markdown, .. } if markdown.contains("## Summary"))));
        assert!(events.iter().any(|e| matches!(e, JobEventV2::JobOutputs { outputs, .. } if outputs.get("x").map(String::as_str) == Some("1"))));
        Ok(())
    }

    /// W07-R07: a local composite action: inputs with defaults, outputs from its steps,
    /// `$GITHUB_ENV` reaching the job's later steps, its steps in the caller's log.
    #[cfg(unix)]
    #[test]
    fn a_local_composite_action() -> anyhow::Result<()> {
        use base64::Engine as _;
        // Written by a step, base64-encoded: a `run:` step's own `${{ }}` are interpolated before
        // it runs (as on GitHub), and these belong to the action.
        let action = r#"name: Greet
inputs:
  who: { description: who, required: true }
  greeting: { description: how, default: "hello ${{ github.event_name }}" }
outputs:
  said: { description: what, value: "${{ steps.say.outputs.said }}" }
runs:
  using: composite
  steps:
    - id: say
      shell: bash
      run: |
        echo "${{ inputs.greeting }}, ${{ inputs.who }}"
        echo "said=${{ inputs.greeting }}, ${{ inputs.who }}" >> "$GITHUB_OUTPUT"
        echo "GREETED=yes" >> "$GITHUB_ENV"
"#;
        let b64 = base64::engine::general_purpose::STANDARD.encode(action);
        let wf = format!(
            "on: push\njobs:\n  t:\n    runs-on: x\n    steps:\n      - run: mkdir -p .github/actions/greet && echo {b64} | base64 -d > .github/actions/greet/action.yml\n      - id: greet\n        uses: ./.github/actions/greet\n        with:\n          who: kiln\n      - run: echo \"it said '${{{{ steps.greet.outputs.said }}}}' and GREETED=$GREETED\"\n"
        );
        let wf = wf.as_str();
        let (r, logs, events) = run_leg(wf, &[])?;
        assert_eq!(r.status, Status::Succeeded, "{r:?} {logs:?}");
        assert!(logs.get(&1).is_some_and(|l| l.contains("hello push, kiln")), "{logs:?}");
        assert_eq!(finished(&events, 1).map(|f| f.2.get("said").cloned()), Some(Some("hello push, kiln".into())));
        assert!(logs.get(&2).is_some_and(|l| l.contains("it said 'hello push, kiln' and GREETED=yes")), "{logs:?}");
        Ok(())
    }

    /// W06-R06: problem matchers a step adds apply to the later steps' output: matched lines
    /// become annotations (with the file when it is the repository's) in place of the line, until
    /// removed. A matcher file that can't be read, or a disabled command, fails its step, as on
    /// GitHub.
    #[cfg(unix)]
    #[test]
    fn problem_matchers() -> anyhow::Result<()> {
        let wf = r###"on: push
jobs:
  t:
    runs-on: x
    steps:
      - run: |
          mkdir -p src && echo 'let x: number = "a";' > src/index.ts
          cat > tsc.json <<'EOF'
          { "problemMatcher": [ { "owner": "tsc", "pattern": [ { "regexp": "^([^\\s].*)[\\(:](\\d+)[,:](\\d+)(?:\\):\\s+|\\s+-\\s+)(error|warning|info)\\s+TS(\\d+)\\s*:\\s*(.*)$", "file": 1, "line": 2, "column": 3, "severity": 4, "code": 5, "message": 6 } ] } ] }
          EOF
          echo "::add-matcher::tsc.json"
      - run: |
          echo "src/index.ts(1,5): error TS2322: Type 'string' is not assignable to type 'number'."
          echo "gone/missing.ts(2,1): warning TS1: no such file"
      - run: echo "::remove-matcher owner=tsc::"
      - run: 'echo "src/index.ts(1,5): error TS2322: after removal"'
      - run: echo "::add-matcher::nope.json"
      - if: always()
        run: echo "::set-env name=A::b"
"###;
        let (r, logs, events) = run_leg(wf, &[])?;
        assert_eq!(r.status, Status::Failed, "{logs:?}");
        let ann = |m: &str| events.iter().find_map(|e| match e {
            JobEventV2::Annotation { idx, level, message, file, line, col, .. } if message == m => Some((*idx, level.clone(), file.clone(), *line, *col)),
            _ => None,
        });
        assert!(logs.get(&0).is_some_and(|l| l.contains("Added matchers: 'tsc'. Problem matchers scan action output")), "{logs:?}");
        assert_eq!(ann("Type 'string' is not assignable to type 'number'."), Some((1, "error".into(), Some("src/index.ts".into()), Some(1), Some(5))));
        assert_eq!(ann("no such file"), Some((1, "warning".into(), None, Some(2), Some(1))), "a file that isn't there is dropped");
        assert!(logs.get(&1).is_some_and(|l| l.contains("error: Type 'string'") && !l.contains("src/index.ts(1,5)")), "the issue replaces the line: {logs:?}");
        assert_eq!(finished(&events, 1).map(|f| f.1), Some(Conclusion::Success), "issues alone don't fail a step");
        assert!(logs.get(&2).is_some_and(|l| l.contains("Removed matchers: 'tsc'")), "{logs:?}");
        assert!(ann("after removal").is_none() && logs.get(&3).is_some_and(|l| l.contains("src/index.ts(1,5): error TS2322: after removal")), "{logs:?}");
        assert_eq!(finished(&events, 4).map(|f| f.1), Some(Conclusion::Failure));
        assert_eq!(ann("Unable to process command '::add-matcher::nope.json' successfully.").map(|a| a.0), Some(4));
        assert_eq!(finished(&events, 5).map(|f| f.1), Some(Conclusion::Failure), "a disabled command fails its step");
        assert_eq!(ann("Unable to process command '::set-env name=A::b' successfully.").map(|a| a.0), Some(5));
        Ok(())
    }

    /// W06-AC01: the toolkit conformance action (F-20, built on the current `@actions/core`,
    /// `exec` and `io`), each effect seen by a later step, its post step, or the leg's events.
    /// Runs on this machine's Node 24 instead of downloading one; skipped without it.
    #[cfg(unix)]
    #[test]
    fn toolkit_conformance() -> anyhow::Result<()> {
        let Some(node) = std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join("node")).find(|n| n.is_file())) else { return Ok(()) };
        let version = std::process::Command::new(&node).arg("--version").output()?;
        if !String::from_utf8_lossy(&version.stdout).starts_with("v24.") {
            return Ok(());
        }
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/toolkit-action");
        let wf = format!(
            r###"on: push
jobs:
  t:
    runs-on: x
    steps:
      - run: mkdir -p .github/actions && cp -r "{}" .github/actions/toolkit
      - id: t
        uses: ./.github/actions/toolkit
        with:
          required: yes-req
      - run: |
          echo "o1=${{{{ steps.t.outputs.o1 }}}} EXPORTED=$EXPORTED" 'multi=${{{{ toJSON(steps.t.outputs.multi) }}}}'
          case "$PATH" in /opt/f20-bin:*) echo "path first" ;; esac
          echo "the secret again: super-secret-f20"
"###,
            fixture.display()
        );
        let prepare = |work: &Path| -> anyhow::Result<()> {
            let bin = work.join("_externals/node24/bin");
            std::fs::create_dir_all(&bin)?;
            std::os::unix::fs::symlink(&node, bin.join("node"))?;
            Ok(())
        };
        let (r, logs, events) = run_leg_prepared(&wf, &[("ACTIONS_STEP_DEBUG", "true")], Dialect::Gha, &prepare)?;
        let all: String = logs.values().cloned().collect();
        assert_eq!(r.status, Status::Succeeded, "{r:?} {all}");
        let step = logs.get(&1).cloned().unwrap_or_default();
        assert!(step.contains(r#"inputs: required=yes-req missing=[] padded=[spaced] raw=[  spaced  ] flag=true lines=["a","b","c"]"#), "{step}");
        assert!(step.contains("required absent: Input required and not supplied: absent"), "{step}");
        assert!(step.contains("printing the secret: ***") && !all.contains("super-secret-f20"), "setSecret masks from then on");
        assert!(step.contains("a debug line"), "debug shows with ACTIONS_STEP_DEBUG: {step}");
        assert!(step.contains("from exec") && step.contains("exec output: captured (exit 0)"), "{step}");
        assert!(step.contains("which sh: true") && step.contains("is debug: true"), "{step}");
        assert!(events.iter().any(|e| matches!(e, JobEventV2::Group { op, title, .. } if op == "start" && title.as_deref() == Some("G"))));
        let ann = |m: &str| events.iter().find_map(|e| match e {
            JobEventV2::Annotation { level, message, title, file, line, end_line, col, end_col, .. } if message == m => Some((level.clone(), title.clone(), file.clone(), *line, *end_line, *col, *end_col)),
            _ => None,
        });
        assert_eq!(ann("a notice"), Some(("notice".into(), Some("N".into()), Some("README.md".into()), Some(1), None, None, None)));
        assert_eq!(ann("a warning"), Some(("warning".into(), None, Some("README.md".into()), Some(2), Some(3), Some(1), Some(4))));
        assert!(ann("Error: an error object").is_some_and(|a| a.0 == "error"), "{events:?}");
        assert!(events.iter().any(|e| matches!(e, JobEventV2::Summary { markdown, .. } if markdown.contains("<h1>F-20</h1>") && markdown.contains("<th>name</th>") && markdown.contains("<td>v1</td>"))));
        let later = logs.get(&2).cloned().unwrap_or_default();
        assert!(later.contains(r#"o1=v1 EXPORTED=e1 multi="line one\nline two""#), "{later}");
        assert!(later.contains("path first") && later.contains("the secret again: ***"), "{later}");
        assert!(all.contains("post: state=state-v"), "the post step reads the state: {all}");
        Ok(())
    }

    /// A cancelled leg's post steps run to their end, as GitHub's do: the cancel stops the step
    /// that was running, not the cleanup after it.
    #[cfg(unix)]
    #[test]
    fn post_steps_run_after_a_cancel() -> anyhow::Result<()> {
        let Some(node) = std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join("node")).find(|n| n.is_file())) else { return Ok(()) };
        if !std::process::Command::new(&node).arg("--version").output().is_ok_and(|v| String::from_utf8_lossy(&v.stdout).starts_with("v24.")) {
            return Ok(());
        }
        let wf = r###"on: push
jobs:
  t:
    runs-on: x
    steps:
      - run: |
          mkdir -p .github/actions/slow
          printf 'name: slow\nruns:\n  using: node24\n  main: main.js\n  post: post.js\n' > .github/actions/slow/action.yml
          echo 'setTimeout(() => {}, 60000)' > .github/actions/slow/main.js
          echo 'setTimeout(() => console.log("post: cleaned up"), 1500)' > .github/actions/slow/post.js
      - uses: ./.github/actions/slow
"###;
        let (w, _) = kiln_ci::wf::parse_workflow(".github/workflows/t.yml", wf, Dialect::Gha);
        let job = w.and_then(|w| w.jobs.into_iter().next()).ok_or_else(|| anyhow::anyhow!("no job"))?;
        let tmp = tempfile::tempdir()?;
        let bin = tmp.path().join("_externals/node24/bin");
        std::fs::create_dir_all(&bin)?;
        std::os::unix::fs::symlink(&node, bin.join("node"))?;
        let a = AssignmentV2 { job_id: "leg-c".into(), repo: "acme/app".into(), dialect: "gha".into(), steps: job.steps.clone(), timeout_minutes: 5, ..AssignmentV2::default() };
        let (tx, rx) = async_channel::unbounded();
        let cancel = Shutdown::new();
        let leg = Leg { a, work_dir: tmp.path().to_path_buf(), runner_name: "test-runner".into(), up: Uplink(tx), cancel: cancel.clone(), http: std::sync::Arc::new(crate::http::client::Client::public()?) };
        let result = crate::rt::block_on(async move {
            crate::rt::spawn(async move {
                crate::rt::sleep(Duration::from_secs(3)).await;
                cancel.cancel();
            });
            Ok::<_, anyhow::Error>(leg.run().await)
        })?;
        let (mut logs, mut events) = (String::new(), Vec::new());
        while let Ok(m) = rx.try_recv() {
            match m {
                crate::exec::Up::Log(c) => logs.push_str(&String::from_utf8_lossy(&c.data)),
                crate::exec::Up::Msg(AgentMsg::JobEvent { json, .. }) => events.push(serde_json::from_str::<JobEventV2>(&json)?),
                _ => {}
            }
        }
        assert_eq!(result.status, Status::Cancelled, "{logs}");
        assert!(logs.contains("post: cleaned up"), "the post step ran to its end: {logs}");
        let post = events.iter().find_map(|e| match e {
            JobEventV2::StepStarted { idx, kind, .. } if kind == "post" => Some(*idx),
            _ => None,
        });
        assert_eq!(post.and_then(|i| finished(&events, i)).map(|f| f.1), Some(Conclusion::Success), "{events:?}");
        Ok(())
    }

    /// W04-R20 to R22 on the runner: typed outputs, exported environment, `run.env`, and a secret
    /// output whose value leaves only for sealing (the step output it reads is masked).
    #[cfg(unix)]
    #[test]
    fn kiln_outputs() -> anyhow::Result<()> {
        let wf = r###"name: t
on: push
run-env: true
jobs:
  build:
    runs-on: x
    outputs:
      info: ${{ steps.meta.outputs.info }}
      plain: ${{ steps.meta.outputs.plain }}
      token: { value: "${{ steps.meta.outputs.secret }}", secret: true }
    exports: { env: [BUILD_ID] }
    steps:
      - id: meta
        run: |
          echo '{"name": "info", "value": {"version": "1.2.3", "count": 3}}' >> "$KILN_OUTPUT_JSON"
          echo "plain=hello" >> "$GITHUB_OUTPUT"
          echo "secret=s3cr3t-value-123" >> "$GITHUB_OUTPUT"
          echo "BUILD_ID=build-42" >> "$GITHUB_ENV"
          echo "SHARED=from-build" >> "$KILN_RUN_ENV"
      - run: echo "typed ${{ steps.meta.outputs.info.version }} ${{ steps.meta.outputs.info.count }}"
"###;
        let (r, logs, events) = run_leg_in(wf, &[], Dialect::Kiln)?;
        assert_eq!(r.status, Status::Succeeded, "{r:?} {logs:?}");
        assert!(logs.get(&1).is_some_and(|l| l.contains("typed 1.2.3 3")), "the kiln dialect sees typed outputs: {logs:?}");
        let outs = events.iter().find_map(|e| match e {
            JobEventV2::JobOutputs { outputs, json_outputs, env, run_env, secret_outputs, secret_values, .. } => Some((outputs.clone(), json_outputs.clone(), env.clone(), run_env.clone(), secret_outputs.clone(), secret_values.clone())),
            _ => None,
        });
        let Some((outputs, json_outputs, env, run_env, secret_outputs, secret_values)) = outs else { anyhow::bail!("no job outputs") };
        assert_eq!(json_outputs.get("info"), Some(&serde_json::json!({"version": "1.2.3", "count": 3})));
        assert_eq!(outputs.get("plain").map(String::as_str), Some("hello"));
        assert!(!outputs.contains_key("token"), "a secret output never travels as a plain one");
        assert_eq!((secret_outputs, secret_values.get("token").map(String::as_str)), (vec!["token".to_owned()], Some("s3cr3t-value-123")));
        assert_eq!(env.get("BUILD_ID").map(String::as_str), Some("build-42"));
        assert_eq!(run_env, vec![("SHARED".to_owned(), "from-build".to_owned())]);
        // The step output the secret one reads was masked before it was reported.
        let step = finished(&events, 0).map(|f| f.2).unwrap_or_default();
        assert_eq!(step.get("secret").map(String::as_str), Some("***"), "{step:?}");
        Ok(())
    }

    /// W06-AC08: background steps on the host executor.
    #[cfg(unix)]
    #[test]
    fn background_steps() -> anyhow::Result<()> {
        let wf = r###"on: push
jobs:
  t:
    runs-on: x
    outputs:
      late: ${{ steps.late.outputs.x }}
    steps:
      - id: server
        background: true
        run: |
          echo ready > ready.txt
          echo "port=4000" >> "$GITHUB_OUTPUT"
          echo "FROM_BG=1" >> "$GITHUB_ENV"
          sleep 1
          echo "server done"
      - run: |
          for i in $(seq 50); do test -f ready.txt && break; sleep 0.1; done
          echo "read $(cat ready.txt); before the wait FROM_BG='$FROM_BG' port='${{ steps.server.outputs.port }}'"
      - wait: server
      - run: echo "after the wait FROM_BG=$FROM_BG port=${{ steps.server.outputs.port }}"
      - id: soft
        background: true
        continue-on-error: true
        run: exit 4
      - wait: soft
      - id: stubborn
        background: true
        run: |
          trap '' TERM
          echo "ignoring SIGTERM"
          sleep 60
      - run: sleep 1
      - cancel: stubborn
      - id: n0
        background: true
        run: |
          mkdir -p running && touch running/n0 && ls running | wc -l >> counts
          sleep 1
          rm running/n0
      - id: n1
        background: true
        run: |
          mkdir -p running && touch running/n1 && ls running | wc -l >> counts
          sleep 1
          rm running/n1
      - id: n2
        background: true
        run: |
          mkdir -p running && touch running/n2 && ls running | wc -l >> counts
          sleep 1
          rm running/n2
      - id: n3
        background: true
        run: |
          mkdir -p running && touch running/n3 && ls running | wc -l >> counts
          sleep 1
          rm running/n3
      - id: n4
        background: true
        run: |
          mkdir -p running && touch running/n4 && ls running | wc -l >> counts
          sleep 1
          rm running/n4
      - id: n5
        background: true
        run: |
          mkdir -p running && touch running/n5 && ls running | wc -l >> counts
          sleep 1
          rm running/n5
      - id: n6
        background: true
        run: |
          mkdir -p running && touch running/n6 && ls running | wc -l >> counts
          sleep 1
          rm running/n6
      - id: n7
        background: true
        run: |
          mkdir -p running && touch running/n7 && ls running | wc -l >> counts
          sleep 1
          rm running/n7
      - id: n8
        background: true
        run: |
          mkdir -p running && touch running/n8 && ls running | wc -l >> counts
          sleep 1
          rm running/n8
      - id: n9
        background: true
        run: |
          mkdir -p running && touch running/n9 && ls running | wc -l >> counts
          sleep 1
          rm running/n9
      - id: n10
        background: true
        run: |
          mkdir -p running && touch running/n10 && ls running | wc -l >> counts
          sleep 1
          rm running/n10
      - id: n11
        background: true
        run: |
          mkdir -p running && touch running/n11 && ls running | wc -l >> counts
          sleep 1
          rm running/n11
      - wait-all: true
      - run: echo "most at once $(sort -n counts | tail -1)"
      - parallel:
          - id: p1
            run: echo "v=one" >> "$GITHUB_OUTPUT"
          - id: p2
            run: echo "v=two" >> "$GITHUB_OUTPUT"
      - run: echo "parallel gave ${{ steps.p1.outputs.v }} ${{ steps.p2.outputs.v }}"
      - id: late
        background: true
        run: sleep 1 && echo "x=late" >> "$GITHUB_OUTPUT"
      - id: bad
        background: true
        run: exit 3
      - wait: bad
      - if: always()
        run: echo "after the failed wait"
"###;
        let t = Instant::now();
        let (r, logs, events) = run_leg(wf, &[])?;
        let all: String = logs.values().cloned().collect();
        assert_eq!(r.status, Status::Failed, "the failed background step fails its wait: {all}");
        let log = |i: u32| logs.get(&i).cloned().unwrap_or_default();
        assert!(log(1).contains("read ready; before the wait FROM_BG='' port=''"), "{}", log(1));
        assert!(log(0).contains("server done"), "its own log: {:?}", logs.get(&0));
        assert!(log(3).contains("after the wait FROM_BG=1 port=4000"), "{}", log(3));
        assert_eq!(finished(&events, 5).map(|f| f.1), Some(Conclusion::Success), "continue-on-error: the wait succeeds");
        assert_eq!(finished(&events, 6).map(|f| f.1), Some(Conclusion::Cancelled), "cancelled");
        assert!(t.elapsed() >= BACKGROUND_GRACE, "SIGTERM was ignored: SIGKILL after the grace period");
        let most = all.lines().find_map(|l| l.trim().strip_prefix("most at once ")).and_then(|n| n.trim().parse::<u32>().ok());
        assert_eq!(most, Some(10), "12 background steps, 10 at once: {all}");
        assert!(all.contains("parallel gave one two"), "{all}");
        assert!(events.iter().any(|e| matches!(e, JobEventV2::JobOutputs { outputs, .. } if outputs.get("late").map(String::as_str) == Some("late"))), "the implicit wait-all before the job's outputs");
        assert!(all.contains("after the failed wait"));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn failures_timeouts_and_skips() -> anyhow::Result<()> {
        let wf = "on: push
jobs:
  t:
    runs-on: x
    steps:
      - run: exit 2
      - run: echo skipped
      - if: always()
        timeout-minutes: 0.02
        run: sleep 30
      - if: failure()
        run: echo ran-after-failure
";
        let t = Instant::now();
        let (r, logs, events) = run_leg(wf, &[])?;
        assert_eq!(r.status, Status::Failed);
        assert_eq!(finished(&events, 1).map(|f| f.1), Some(Conclusion::Skipped));
        assert_eq!(finished(&events, 2).map(|f| f.0), Some(Conclusion::Failure), "timed out");
        assert!(events.iter().any(|e| matches!(e, JobEventV2::Annotation { message, .. } if message == "The action has timed out.")));
        assert!(events.iter().any(|e| matches!(e, JobEventV2::Annotation { message, .. } if message == "Process completed with exit code 2.")));
        assert!(logs.get(&3).is_some_and(|l| l.contains("ran-after-failure")));
        assert!(t.elapsed() < Duration::from_secs(20), "the timeout cut the sleep short: {:?}", t.elapsed());
        Ok(())
    }

    #[test]
    fn shells() -> Result<(), String> {
        let p = Path::new("/t/s.sh");
        assert_eq!(shell_command(Some("bash"), p)?, ("bash".to_owned(), ["--noprofile", "--norc", "-eo", "pipefail", "/t/s.sh"].map(str::to_owned).to_vec()));
        assert_eq!(shell_command(Some("sh"), p)?, ("sh".to_owned(), vec!["-e".to_owned(), "/t/s.sh".to_owned()]));
        assert_eq!(shell_command(Some("python"), p)?, ("python".to_owned(), vec!["/t/s.sh".to_owned()]));
        assert_eq!(shell_command(Some("perl -w {0}"), p)?, ("perl".to_owned(), vec!["-w".to_owned(), "/t/s.sh".to_owned()]));
        assert!(shell_command(Some("perl -w"), p).is_err());
        assert_eq!(script_file(Some("cmd"), "a\nb").1, "a\r\nb");
        assert!(script_file(Some("pwsh"), "x").1.starts_with("$ErrorActionPreference = 'stop'"));
        assert_eq!(script_file(Some("bash"), "x").0, ".sh");
        Ok(())
    }
}

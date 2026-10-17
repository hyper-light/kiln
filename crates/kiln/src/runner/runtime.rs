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
use kiln_ci::wf::ir::{Step, Tpl, YVal};
use kiln_ci::wf::validate::{Site, options};
use kiln_ci::wire::{AssignmentV2, Conclusion, JobEventV2};

use crate::exec::Uplink;
use crate::proto::{AgentMsg, JobResult, LOG_CHUNK};
use crate::rt::{Interval, Shutdown};
use crate::store::Status;

/// One leg, run on this machine.
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
}

/// What a process left: how it ended, its outputs and its saved state (`$GITHUB_STATE`).
struct Ran {
    end: End,
    outputs: BTreeMap<String, String>,
    state: Vec<(String, String)>,
}

/// An action's post step, run after the main steps in reverse order (W06-R11).
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

/// The paths of W06-R01.
struct Layout {
    workspace: PathBuf,
    runner_workspace: PathBuf,
    temp: PathBuf,
    tool_cache: PathBuf,
    commands: PathBuf,
}

impl Layout {
    fn new(work: &Path, leg: &str, repo: &str) -> Layout {
        let digest = crate::util::sha256_hex(leg.as_bytes());
        let runner_workspace = work.join("w").join(digest.get(..8).unwrap_or("leg"));
        let name = repo.rsplit('/').next().unwrap_or("repo");
        let temp = runner_workspace.join("_temp");
        Layout {
            workspace: runner_workspace.join(name),
            commands: temp.join("_runner_file_commands"),
            temp,
            tool_cache: work.join("_tool"),
            runner_workspace,
        }
    }
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
struct State {
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
}

impl State {
    fn cx(&self) -> Cx<'_> {
        let mut values = self.base.clone();
        values.push(("github", self.github.clone()));
        values.push(("env", str_map(&self.env)));
        values.push(("secrets", str_map(&self.secrets)));
        values.push(("job", Value::object([("status".to_owned(), Value::str(if self.cancelled { "cancelled" } else if self.failed { "failure" } else { "success" }))])));
        let steps = Value::object(self.steps.iter().filter_map(|d| {
            let id = d.id.clone()?;
            Some((
                id,
                Value::object([
                    ("outputs".to_owned(), str_map(&d.outputs)),
                    ("outcome".to_owned(), d.outcome.map_or(Value::Null, |c| Value::str(word(c)))),
                    ("conclusion".to_owned(), d.conclusion.map_or(Value::Null, |c| Value::str(word(c)))),
                ]),
            ))
        }));
        values.push(("steps", steps));
        Cx { values, status: StatusView { failed: self.failed, cancelled: self.cancelled, skipped: false }, workspace: &self.layout.workspace }
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

impl Leg {
    pub async fn run(self) -> JobResult {
        let job = self.a.job_id.clone();
        let started = Instant::now();
        let r = std::panic::AssertUnwindSafe(self.run_inner()).catch_unwind().await;
        let (status, error) = match r {
            Ok(Ok(s)) => (s, None),
            Ok(Err(e)) => (Status::Failed, Some(format!("{e:#}"))),
            Err(_) => (Status::Failed, Some("the runtime failed unexpectedly".into())),
        };
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
        if a.debug.runner_debug {
            set("RUNNER_DEBUG", "1".into());
        }
        let rt = &a.runtime;
        set("ACTIONS_RUNTIME_URL", rt.results_url.clone());
        set("ACTIONS_RUNTIME_TOKEN", rt.runtime_token.clone());
        set("ACTIONS_RESULTS_URL", rt.results_url.clone());
        set("ACTIONS_CACHE_URL", rt.cache_url.clone());
        set("ACTIONS_CACHE_SERVICE_V2", "True".into());
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

    async fn run_inner(&self) -> anyhow::Result<Status> {
        let a = &self.a;
        let dialect = if a.dialect == "kiln" { Dialect::Kiln } else { Dialect::Gha };
        let layout = Layout::new(&self.work_dir, &a.job_id, &a.repo);
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
        let mut masks: Vec<String> = secrets.values().cloned().collect();
        masks.extend(a.masks.iter().cloned());
        masks.push(a.runtime.github_token.clone());
        masks.push(a.runtime.runtime_token.clone());
        let step_debug = a.debug.step_debug || secrets.get("ACTIONS_STEP_DEBUG").is_some_and(|v| v == "true");
        let base = vec![
            ("vars", Value::from_json(&a.contexts.vars)),
            ("needs", Value::from_json(&a.contexts.needs)),
            ("strategy", Value::from_json(&a.contexts.strategy)),
            ("matrix", Value::from_json(&a.contexts.matrix)),
            ("inputs", Value::from_json(&a.contexts.inputs)),
            (
                "runner",
                Value::object([
                    ("name".to_owned(), Value::str(&self.runner_name)),
                    ("os".to_owned(), Value::str(runner_os())),
                    ("arch".to_owned(), Value::str(runner_arch())),
                    ("temp".to_owned(), Value::str(&layout.temp.to_string_lossy())),
                    ("tool_cache".to_owned(), Value::str(&layout.tool_cache.to_string_lossy())),
                    ("debug".to_owned(), Value::str(if a.debug.runner_debug { "1" } else { "" })),
                    ("environment".to_owned(), Value::str("self-hosted")),
                ]),
            ),
        ];
        // The github context, completed with what only the runner knows.
        let mut github = a.contexts.github.clone();
        if let Some(m) = github.as_object_mut() {
            m.insert("workspace".into(), serde_json::json!(layout.workspace.to_string_lossy()));
            m.insert("token".into(), serde_json::json!(a.runtime.github_token));
            m.insert("job".into(), serde_json::json!(a.leg.job_id.rsplit('/').next().unwrap_or_default()));
            m.insert("event_path".into(), serde_json::json!(layout.temp.join("_github_workflow").join("event.json").to_string_lossy()));
            m.insert("action".into(), serde_json::json!(""));
            // The server, as this runner reaches it.
            m.insert("server_url".into(), serde_json::json!(a.runtime.facade_url));
            m.insert("api_url".into(), serde_json::json!(a.runtime.api_url));
            m.insert("graphql_url".into(), serde_json::json!(a.runtime.graphql_url));
        }
        let mut st = State {
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
        self.pre_steps(&mut st, deadline).await;
        for (idx, step) in a.steps.iter().enumerate() {
            let idx = u32::try_from(idx).unwrap_or(u32::MAX);
            if self.cancel.is_cancelled() {
                st.cancelled = true;
            }
            let end = self.step(&mut st, idx, step, deadline, false).await;
            if end == Some(End::TimedOut) && Instant::now() >= deadline {
                timed_out = true;
                st.failed = true;
                self.note(idx, &format!("The job running on runner {} has exceeded the maximum execution time of {} minutes.", self.runner_name, a.timeout_minutes)).await;
            }
        }
        self.job_outputs(&st).await;
        self.post_steps(&mut st, deadline).await;
        // The leg's processes end with it (GitHub's "Cleaning up orphan processes").
        for g in &st.groups {
            crate::proc::kill_orphan_group(*g, &format!("KILN_LEG={}", a.job_id));
        }
        Ok(if timed_out {
            Status::TimedOut
        } else if st.cancelled || self.cancel.is_cancelled() {
            Status::Cancelled
        } else if st.failed {
            Status::Failed
        } else {
            Status::Succeeded
        })
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
            (None, None) if step.is_control() => Err(anyhow::anyhow!("background steps (`background`, `wait`, `parallel`, docs/ci.md W06-R17) can't run on this runner yet")),
            (None, None) => Err(anyhow::anyhow!("the step runs neither a script nor an action")),
        };
        let (end, outputs) = match ran {
            Ok(r) => (r.end, r.outputs),
            Err(e) => {
                self.note(idx, &format!("Error: {e:#}")).await;
                (End::Exit(None), BTreeMap::new())
            }
        };
        let (outcome, exit_code) = match end {
            End::Exit(Some(0)) => (Conclusion::Success, Some(0)),
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
        let conclusion = if outcome == Conclusion::Failure && continue_on_error { Conclusion::Success } else { outcome };
        if conclusion == Conclusion::Failure {
            st.failed = true;
        }
        self.finish(st, idx, id, outcome, conclusion, exit_code, outputs, nested).await;
        Some(end)
    }

    /// A step begins: its own row, or a header in its composite caller's log.
    async fn begin(&self, nested: bool, idx: u32, name: &str, kind: &str, action: Option<String>) {
        if nested {
            self.note(idx, name).await;
        } else {
            self.start(idx, name, kind, action).await;
        }
    }

    async fn start(&self, idx: u32, name: &str, kind: &str, action: Option<String>) {
        self.event(&JobEventV2::StepStarted { idx, name: name.to_owned(), kind: kind.to_owned(), action }).await;
    }

    #[allow(clippy::too_many_arguments)]
    async fn finish(&self, st: &mut State, idx: u32, id: Option<String>, outcome: Conclusion, conclusion: Conclusion, exit_code: Option<i32>, outputs: BTreeMap<String, String>, nested: bool) {
        if !nested {
            let masked: BTreeMap<String, String> = outputs.iter().map(|(k, v)| (k.clone(), st.masker.mask(v))).collect();
            self.event(&JobEventV2::StepFinished { idx, outcome, conclusion, exit_code, outputs: masked }).await;
        }
        st.steps.push(Done { id, outputs, outcome: Some(outcome), conclusion: Some(conclusion) });
    }

    async fn annotate(&self, idx: u32, level: &str, message: &str) {
        self.event(&JobEventV2::Annotation { idx, level: level.to_owned(), message: message.to_owned(), title: None, file: None, line: None, end_line: None, col: None, end_col: None, log_offset: 0 }).await;
    }

    /// Runs a script or a program and processes what it prints and writes (W06-R05 to W06-R08).
    async fn exec(&self, st: &mut State, idx: u32, what: Run<'_>, wd: &Path, mut env: BTreeMap<String, String>, deadline: Instant) -> anyhow::Result<Ran> {
        let id = crate::util::new_id();
        let (file, body, shell) = match &what {
            Run::Script { script, shell } => {
                let (ext, body) = script_file(*shell, script);
                (Some(st.layout.temp.join(format!("{id}{ext}"))), body, *shell)
            }
            Run::Program { .. } => (None, String::new(), None),
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
        let (prog, args) = match (what, &file) {
            (Run::Script { .. }, Some(f)) => shell_command(shell, f).map_err(|e| anyhow::anyhow!(e))?,
            (Run::Program { prog, args }, _) => (prog, args),
            (Run::Script { .. }, None) => anyhow::bail!("no script"),
        };
        let mut cmd = Command::new(&prog);
        cmd.args(&args).current_dir(wd).envs(&env);
        let mut running = crate::proc::spawn_group(cmd)?;
        st.groups.push(running.pgid);
        let mut proc = Processor::new(st.allow_unsecure);
        let mut line: Vec<u8> = Vec::new();
        let mut set_output: BTreeMap<String, String> = BTreeMap::new();
        let mut flush = Interval::new(Duration::from_millis(100));
        let mut end: Option<End> = None;
        let mut output_done = false;
        loop {
            if output_done && end.is_some() {
                break;
            }
            if end.is_some() {
                // Daemons keep the pipes open: drain briefly once the step itself exited.
                match crate::rt::timeout(Duration::from_secs(2), running.output.recv()).await {
                    Ok(Ok(c)) => self.feed(st, idx, &mut proc, &mut line, &c, &mut set_output).await,
                    Ok(Err(_)) | Err(_) => output_done = true,
                }
                continue;
            }
            enum Ev {
                Chunk(Option<Vec<u8>>),
                Exit(Option<i32>),
                Flush,
                Cancel,
                Deadline,
            }
            let ev = futures::select_biased! {
                c = recv_if(&running.output, !output_done).fuse() => Ev::Chunk(c),
                code = (&mut running.exit).fuse() => Ev::Exit(code.ok().flatten()),
                () = self.cancel.cancelled().fuse() => Ev::Cancel,
                () = crate::rt::sleep_until(deadline).fuse() => Ev::Deadline,
                () = flush.tick().fuse() => Ev::Flush,
            };
            match ev {
                Ev::Chunk(Some(c)) => self.feed(st, idx, &mut proc, &mut line, &c, &mut set_output).await,
                Ev::Chunk(None) => output_done = true,
                Ev::Exit(code) => end = Some(End::Exit(code)),
                Ev::Flush => self.partial(st, idx, &mut line).await,
                Ev::Cancel => {
                    crate::proc::interrupt_group(&mut running, SIGINT_GRACE, SIGTERM_GRACE).await;
                    end = Some(End::Cancelled);
                }
                Ev::Deadline => {
                    crate::proc::interrupt_group(&mut running, SIGINT_GRACE, SIGTERM_GRACE).await;
                    end = Some(End::TimedOut);
                }
            }
        }
        if !line.is_empty() {
            let rest = std::mem::take(&mut line);
            self.text(st, idx, &rest).await;
        }
        let tail = st.masker.finish();
        self.log(idx, tail).await;
        // The environment files (W06-R07).
        let read = |p: &PathBuf| std::fs::read_to_string(p).unwrap_or_default();
        let get = |k: &str| files.iter().find(|(x, _)| *x == k).map(|(_, p)| p.clone()).unwrap_or_default();
        let (envf, outf, pathf, statef, sumf, jsonf, runf) = (get("GITHUB_ENV"), get("GITHUB_OUTPUT"), get("GITHUB_PATH"), get("GITHUB_STATE"), get("GITHUB_STEP_SUMMARY"), get("KILN_OUTPUT_JSON"), get("KILN_RUN_ENV"));
        let texts = crate::rt::blocking(move || (read(&envf), read(&outf), read(&pathf), read(&statef), read(&sumf), read(&jsonf), read(&runf))).await?;
        let (env_t, out_t, path_t, state_t, sum_t, _json_t, _run_t) = texts;
        let mut outputs = set_output;
        let mut failed_files = false;
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
            Ok(pairs) => outputs.extend(pairs),
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
                Ok(md) => self.event(&JobEventV2::Summary { idx, markdown: st.masker.mask(md) }).await,
                Err(e) => self.annotate(idx, "warning", &e).await,
            }
        }
        if let Some(f) = file {
            let _ = crate::rt::blocking(move || std::fs::remove_file(f)).await;
        }
        let state = envfiles::pairs(&state_t).unwrap_or_default();
        let end = end.unwrap_or(End::Exit(None));
        let end = if failed_files && end == End::Exit(Some(0)) { End::Exit(Some(1)) } else { end };
        Ok(Ran { end, outputs, state })
    }

    /// Output from the step: whole lines go through the command processor; text is masked and
    /// logged.
    async fn feed(&self, st: &mut State, idx: u32, proc: &mut Processor, line: &mut Vec<u8>, chunk: &[u8], set_output: &mut BTreeMap<String, String>) {
        for &b in chunk {
            line.push(b);
            if b == b'\n' {
                let whole = std::mem::take(line);
                let text = String::from_utf8_lossy(&whole).to_string();
                match proc.line(&text) {
                    Line::Text | Line::Unknown => self.text(st, idx, &whole).await,
                    Line::Refused(m) => self.annotate(idx, "error", &m).await,
                    Line::Command(c) => self.command(st, idx, c, set_output).await,
                }
            }
        }
    }

    /// A line still being written: shown now unless it may be a command.
    async fn partial(&self, st: &mut State, idx: u32, line: &mut Vec<u8>) {
        if line.is_empty() {
            return;
        }
        let head = String::from_utf8_lossy(line.get(..line.len().min(4)).unwrap_or_default()).to_string();
        let t = head.trim_start();
        if t.starts_with("::") || t.starts_with("##") || t.is_empty() || ":#".starts_with(t) {
            return;
        }
        let rest = std::mem::take(line);
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

    async fn command(&self, st: &mut State, idx: u32, c: commands::Command, set_output: &mut BTreeMap<String, String>) {
        use kiln_ci::runner::commands::Command as C;
        match c {
            C::AddMask(v) => {
                if st.masker.add(&v) {
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
            C::StopCommands(_) | C::Echo(_) | C::AddMatcher(_) | C::RemoveMatcher(_) => {}
            C::SetOutput { name, value } => {
                self.annotate(idx, "warning", "The `set-output` command is deprecated and will be disabled soon. Please upgrade to using Environment Files.").await;
                set_output.insert(name, value);
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
        if self.a.outputs.is_empty() {
            return;
        }
        let mut outputs = BTreeMap::new();
        for (k, t) in &self.a.outputs {
            match st.render(t, Site::JobOutputs, &format!("outputs.{k}")) {
                Ok(v) if st.masker.mask(&v) != v => self.annotate(0, "warning", &format!("Skip output '{k}' since it may contain secret.")).await,
                Ok(v) => {
                    outputs.insert(k.clone(), v);
                }
                Err(e) => self.annotate(0, "warning", &e).await,
            }
        }
        self.event(&JobEventV2::JobOutputs { outputs, json_outputs: BTreeMap::new(), env: BTreeMap::new(), run_env: Vec::new(), secret_outputs: Vec::new() }).await;
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
            let dir = Layout::new(&self.work_dir, &self.a.job_id, &self.a.repo).workspace.join(rel);
            return Ok(Located { dir, repo: String::new(), git_ref: String::new() });
        }
        if uses.starts_with("docker://") {
            anyhow::bail!("`{uses}`: Docker actions run with Docker executors (docs/ci.md W13); this runner can't run them yet");
        }
        if uses.starts_with("$/") {
            anyhow::bail!("`{uses}`: actions of the running commit (`$/`) arrive with docs/ci.md W07-R01's mirror reads");
        }
        let r = self.a.actions.get(uses).ok_or_else(|| anyhow::anyhow!("`{uses}` was not resolved when the run started"))?;
        let root = self.fetch_action(r).await?;
        let dir = if r.path.is_empty() { root } else { root.join(&r.path) };
        let git_ref = uses.rsplit_once('@').map(|(_, r)| r.to_owned()).unwrap_or_default();
        Ok(Located { dir, repo: r.repo.clone(), git_ref })
    }

    /// An action's code at its commit, kept under `<work>/_actions/<owner>/<repo>/<sha>`.
    async fn fetch_action(&self, r: &kiln_ci::wire::ActionRef) -> anyhow::Result<PathBuf> {
        let dir = self.work_dir.join("_actions").join(r.repo.replace("..", "_")).join(&r.sha);
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

    /// A `uses:` step (W07): JavaScript, composite (Docker with W13).
    #[allow(clippy::too_many_arguments)]
    async fn action(&self, st: &mut State, idx: u32, step: &Step, uses: &str, with: BTreeMap<String, String>, wd: &Path, env: BTreeMap<String, String>, deadline: Instant) -> anyhow::Result<Ran> {
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
            "docker" => anyhow::bail!("`{uses}` is a Docker action: those run with Docker executors (docs/ci.md W13), which this runner doesn't have yet"),
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
    async fn post_steps(&self, st: &mut State, deadline: Instant) {
        let posts = std::mem::take(&mut st.posts);
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
        let (w, d) = kiln_ci::wf::parse_workflow(".github/workflows/t.yml", workflow, Dialect::Gha);
        anyhow::ensure!(d.iter().all(|x| !x.is_error()), "{d:?}");
        let job = w.and_then(|w| w.jobs.into_iter().next()).ok_or_else(|| anyhow::anyhow!("no job"))?;
        let tmp = tempfile::tempdir()?;
        let a = AssignmentV2 {
            job_id: "leg-1".into(),
            repo: "acme/app".into(),
            dialect: "gha".into(),
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

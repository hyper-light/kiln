//! Checks across a whole workflow (docs/ci.md W02-R05, W02-R08): needs and cycles, every
//! expression parsed with exactly the contexts and functions GitHub allows at its key (Appendix
//! B), and the lints `KCI6001`–`KCI6006`.

use super::diag::{Diag, Edit, Pos, Range, suggest};
use super::ir::{Axis, Job, JobSecrets, Matrix, RunsOn, Step, Tpl, Workflow, YVal};
use crate::expr::{Condition, Dialect, Expr, ParseOptions, template_spans};

/// Where an expression stands, which decides what it may read (Appendix B).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Site {
    RunName,
    WfConcurrency,
    WfEnv,
    JobConcurrency,
    JobContainer,
    JobContainerCredentials,
    JobContainerEnv,
    JobContinueOnError,
    JobDefaultsRun,
    JobEnv,
    JobEnvironment,
    JobEnvironmentUrl,
    JobIf,
    JobName,
    JobOutputs,
    JobRunsOn,
    JobSecrets,
    JobServices,
    JobServiceCredentials,
    JobServiceEnv,
    JobStrategy,
    JobTimeout,
    JobWith,
    StepContinueOnError,
    StepEnv,
    StepIf,
    StepName,
    StepRun,
    StepTimeout,
    StepWith,
    StepWorkingDirectory,
    CallInputDefault,
    CallOutputValue,
    /// kiln keys evaluated with the job (requires, image, …) and strategy keys that see `matrix`.
    KilnJob,
    KilnMatrixWhere,
}

/// The contexts and special functions available at a site (GitHub's table, docs/ci.md
/// Appendix B), plus kiln's contexts in the kiln dialect.
pub fn options(site: Site, dialect: Dialect) -> ParseOptions {
    use Site::*;
    let (ctx, status, hash): (&[&str], bool, bool) = match site {
        RunName | WfConcurrency => (&["github", "inputs", "vars"], false, false),
        WfEnv => (&["github", "secrets", "inputs", "vars"], false, false),
        JobConcurrency => (&["github", "needs", "strategy", "matrix", "inputs", "vars"], false, false),
        JobContainer | JobServices => (&["github", "needs", "strategy", "matrix", "vars", "inputs"], false, false),
        JobContainerCredentials | JobServiceCredentials => (&["github", "needs", "strategy", "matrix", "env", "vars", "secrets", "inputs"], false, false),
        JobContainerEnv | JobServiceEnv => (&["github", "needs", "strategy", "matrix", "job", "runner", "env", "vars", "secrets", "inputs"], false, false),
        JobContinueOnError | JobName | JobRunsOn | JobTimeout | JobEnvironment => (&["github", "needs", "strategy", "matrix", "vars", "inputs"], false, false),
        JobDefaultsRun => (&["github", "needs", "strategy", "matrix", "env", "vars", "inputs"], false, false),
        JobEnv => (&["github", "needs", "strategy", "matrix", "vars", "secrets", "inputs"], false, false),
        JobEnvironmentUrl => (&["github", "needs", "strategy", "matrix", "job", "runner", "env", "vars", "steps", "inputs"], false, false),
        JobIf => (&["github", "needs", "vars", "inputs"], true, false),
        JobOutputs => (&["github", "needs", "strategy", "matrix", "job", "runner", "env", "vars", "secrets", "steps", "inputs"], false, false),
        JobSecrets => (&["github", "needs", "strategy", "matrix", "secrets", "inputs", "vars"], false, false),
        JobStrategy => (&["github", "needs", "vars", "inputs"], false, false),
        JobWith => (&["github", "needs", "strategy", "matrix", "inputs", "vars"], false, false),
        StepIf => (&["github", "needs", "strategy", "matrix", "job", "runner", "env", "vars", "steps", "inputs"], true, true),
        StepContinueOnError | StepEnv | StepName | StepRun | StepTimeout | StepWith | StepWorkingDirectory => {
            (&["github", "needs", "strategy", "matrix", "job", "runner", "env", "vars", "secrets", "steps", "inputs"], false, true)
        }
        CallInputDefault => (&["github", "inputs", "vars"], false, false),
        CallOutputValue => (&["github", "jobs", "vars", "inputs"], false, false),
        KilnJob => (&["github", "needs", "strategy", "matrix", "vars", "inputs", "env", "secrets"], false, false),
        KilnMatrixWhere => (&["github", "needs", "matrix", "vars", "inputs"], false, false),
    };
    let mut contexts: Vec<String> = ctx.iter().map(|s| (*s).to_owned()).collect();
    if dialect == Dialect::Kiln && contexts.iter().any(|c| c == "github") {
        for extra in ["git", "kiln", "run", "diff", "stack", "event", "branch", "repo", "sha"] {
            contexts.push(extra.to_owned());
        }
    }
    ParseOptions { dialect, contexts: Some(contexts), status_functions: status, hash_files: hash }
}

/// Where in a file an offset inside a template is.
fn locate(t: &Tpl, byte: usize, col_in_body: Option<u32>) -> Range {
    let src = &t.src;
    let start = t.range.start;
    let before = src.get(..byte).unwrap_or_default();
    let extra = col_in_body.map_or(0, |c| c.saturating_sub(1));
    let at = if !src.contains('\n') && t.range.start.line == t.range.end.line {
        Pos { line: start.line, col: start.col + u32::try_from(before.chars().count()).unwrap_or(0) + extra, offset: start.offset }
    } else {
        // A block scalar starts at its first line of text, at its indentation (which every line
        // shares).
        let lines = u32::try_from(before.matches('\n').count()).unwrap_or(0);
        let col = start.col + u32::try_from(before.rsplit('\n').next().unwrap_or_default().chars().count()).unwrap_or(0) + extra;
        Pos { line: start.line + lines, col, offset: start.offset }
    };
    Range { start: at, end: at }
}

struct V<'a> {
    dialect: Dialect,
    src: &'a str,
    diags: &'a mut Vec<Diag>,
}

impl V<'_> {
    /// Parses every expression in `t` as `site` allows, reporting where each problem is.
    fn tpl(&mut self, t: &Tpl, site: Site, path: &str) -> Vec<Expr> {
        let opts = options(site, self.dialect);
        let spans = match template_spans(&t.src, self.dialect) {
            Ok(s) => s,
            Err(e) => {
                let r = locate(t, usize::try_from(e.column.unwrap_or(1).saturating_sub(1)).unwrap_or(0), None);
                self.diags.push(Diag::error(e.code, e.message, r).at(path));
                return Vec::new();
            }
        };
        let mut out = Vec::new();
        for (a, b) in spans {
            let body = t.src.get(a..b).unwrap_or_default();
            match Expr::parse_with(body, &opts) {
                Ok(e) => out.push(e),
                Err(e) => {
                    let r = locate(t, a, e.column);
                    let msg = if e.code == "KCI3001" { format!("{} is not available here ({path}): it may use {}", e.message, opts.contexts.as_deref().unwrap_or_default().join(", ")) } else { e.message };
                    self.diags.push(Diag::error(e.code, msg, r).at(path));
                }
            }
        }
        out
    }

    /// An `if:`: a bare expression or a template (GitHub accepts both).
    fn cond(&mut self, t: &Tpl, site: Site, path: &str) -> Vec<Expr> {
        if t.has_expr() {
            let exprs = self.tpl(t, site, path);
            if let Ok(c) = Condition::parse(&t.src, &options(site, self.dialect))
                && c.has_literal_text()
            {
                self.diags.push(
                    Diag::warning("KCI6001", "text outside `${{ }}` makes this condition a string, which is always true: put the whole condition inside `${{ }}` or drop the braces", t.range).at(path),
                );
            }
            return exprs;
        }
        match Expr::parse_with(&t.src, &options(site, self.dialect)) {
            Ok(e) => vec![e],
            Err(e) => {
                let r = locate(t, 0, e.column);
                self.diags.push(Diag::error(e.code, e.message, r).at(path));
                Vec::new()
            }
        }
    }

    fn yval(&mut self, v: &YVal, site: Site, path: &str) {
        let mut ts = Vec::new();
        v.templates(path, &mut ts);
        for (p, t) in ts {
            if t.has_expr() {
                self.tpl(t, site, &p);
            }
        }
    }

    fn env(&mut self, env: &[(String, Tpl)], site: Site, path: &str) {
        for (k, t) in env {
            if t.has_expr() {
                self.tpl(t, site, &format!("{path}.{k}"));
            }
        }
    }
}

/// Every check of W02 that needs the whole workflow. `src` is the file's text, for fixes that
/// edit it.
pub fn validate(wf: &Workflow, dialect: Dialect, src: &str, diags: &mut Vec<Diag>) {
    let mut v = V { dialect, src, diags };
    graph(wf, v.diags);
    if let Some(t) = &wf.run_name {
        v.tpl(t, Site::RunName, "run-name");
    }
    if let Some(c) = &wf.concurrency {
        v.tpl(&c.group, Site::WfConcurrency, "concurrency.group");
    }
    v.env(&wf.env, Site::WfEnv, "env");
    for t in &wf.on {
        for i in &t.inputs {
            if let Some(d) = &i.default {
                v.yval(d, Site::CallInputDefault, &format!("on.{}.inputs.{}.default", t.event, i.name));
            }
        }
        for o in &t.outputs {
            v.tpl(&o.value, Site::CallOutputValue, &format!("on.{}.outputs.{}.value", t.event, o.name));
        }
    }
    let prt = wf.trigger("pull_request_target").is_some();
    for j in &wf.jobs {
        let p = format!("jobs.{}", j.id);
        if let Some(t) = &j.name {
            v.tpl(t, Site::JobName, &format!("{p}.name"));
        }
        if let Some(t) = &j.cond {
            v.cond(t, Site::JobIf, &format!("{p}.if"));
        }
        match &j.runs_on {
            Some(RunsOn::Labels(ls)) => {
                for l in ls {
                    v.tpl(l, Site::JobRunsOn, &format!("{p}.runs-on"));
                }
            }
            Some(RunsOn::Group { group, labels }) => {
                for l in group.iter().chain(labels) {
                    v.tpl(l, Site::JobRunsOn, &format!("{p}.runs-on"));
                }
            }
            None => {}
        }
        if let Some(e) = &j.environment {
            v.tpl(&e.name, Site::JobEnvironment, &format!("{p}.environment"));
            // Not in GitHub's table: as `environment`, the closest key (Appendix B).
            if let Some(YVal::Str(t)) = &e.deployment {
                v.tpl(t, Site::JobEnvironment, &format!("{p}.environment.deployment"));
            }
            if let Some(u) = &e.url {
                v.tpl(u, Site::JobEnvironmentUrl, &format!("{p}.environment.url"));
            }
        }
        if let Some(c) = &j.concurrency {
            v.tpl(&c.group, Site::JobConcurrency, &format!("{p}.concurrency"));
        }
        for o in &j.outputs {
            v.tpl(&o.value, Site::JobOutputs, &format!("{p}.outputs.{}", o.name));
        }
        v.env(&j.env, Site::JobEnv, &format!("{p}.env"));
        for t in j.defaults.shell.iter().chain(&j.defaults.working_directory) {
            v.tpl(t, Site::JobDefaultsRun, &format!("{p}.defaults.run"));
        }
        if let Some(YVal::Str(t)) = &j.timeout_minutes {
            v.tpl(t, Site::JobTimeout, &format!("{p}.timeout-minutes"));
        }
        if let Some(YVal::Str(t)) = &j.continue_on_error {
            v.tpl(t, Site::JobContinueOnError, &format!("{p}.continue-on-error"));
        }
        if let Some(s) = &j.strategy {
            match &s.matrix {
                Some(Matrix::Expr(t)) => {
                    v.tpl(t, Site::JobStrategy, &format!("{p}.strategy.matrix"));
                }
                Some(Matrix::Spec(m)) => {
                    for (k, a) in m.axes.iter().map(|(k, a)| (k.as_str(), a)).chain(m.platforms.iter().map(|a| ("platforms", a))) {
                        match a {
                            Axis::Expr(t) => {
                                v.tpl(t, Site::JobStrategy, &format!("{p}.strategy.matrix.{k}"));
                            }
                            Axis::Values(vals) => {
                                for x in vals {
                                    v.yval(x, Site::JobStrategy, &format!("{p}.strategy.matrix.{k}"));
                                }
                            }
                        }
                    }
                    for x in m.include.iter().chain(&m.exclude) {
                        v.yval(x, Site::JobStrategy, &format!("{p}.strategy.matrix"));
                    }
                    if let Some(w) = &m.filter {
                        v.cond(w, Site::KilnMatrixWhere, &format!("{p}.strategy.matrix.where"));
                    }
                }
                None => {}
            }
            for x in s.fail_fast.iter().chain(&s.max_parallel) {
                v.yval(x, Site::JobStrategy, &format!("{p}.strategy"));
            }
            if let Some(w) = &s.when {
                v.cond(w, Site::KilnJob, &format!("{p}.strategy.when"));
            }
        }
        if let Some(c) = &j.container {
            container(&mut v, c, Site::JobContainer, Site::JobContainerCredentials, Site::JobContainerEnv, &format!("{p}.container"));
        }
        for (sid, c) in &j.services {
            container(&mut v, c, Site::JobServices, Site::JobServiceCredentials, Site::JobServiceEnv, &format!("{p}.services.{sid}"));
        }
        for (k, x) in &j.with {
            v.yval(x, Site::JobWith, &format!("{p}.with.{k}"));
        }
        if let Some(JobSecrets::Map(m)) = &j.secrets {
            v.env(m, Site::JobSecrets, &format!("{p}.secrets"));
        }
        for t in j.kiln.requires.iter().chain(&j.kiln.image) {
            v.tpl(t, Site::KilnJob, &p);
        }
        background_refs(v.diags, &j.steps, &p);
        let mut flat: Vec<(String, &super::ir::Step)> = Vec::new();
        flatten(&j.steps, &format!("{p}.steps"), &mut flat);
        for (sp, s) in flat {
            if let Some(t) = &s.cond {
                v.cond(t, Site::StepIf, &format!("{sp}.if"));
            }
            if let Some(t) = &s.name {
                v.tpl(t, Site::StepName, &format!("{sp}.name"));
            }
            if let Some(t) = &s.run {
                let exprs = v.tpl(t, Site::StepRun, &format!("{sp}.run"));
                let shell = s.shell.as_ref().or(j.defaults.shell.as_ref()).or(wf.defaults.shell.as_ref()).map(|t| t.src.as_str());
                let cx = RunCx { src: v.src, step: s, dialect: v.dialect, shell, windows: on_windows(j) };
                lint_run(v.diags, t, &exprs, &format!("{sp}.run"), &cx);
            }
            if let Some(t) = &s.shell {
                v.tpl(t, Site::StepRun, &format!("{sp}.shell"));
            }
            if let Some(t) = &s.working_directory {
                v.tpl(t, Site::StepWorkingDirectory, &format!("{sp}.working-directory"));
            }
            v.env(&s.env, Site::StepEnv, &format!("{sp}.env"));
            for (k, x) in &s.with {
                v.yval(x, Site::StepWith, &format!("{sp}.with.{k}"));
            }
            if let Some(YVal::Str(t)) = &s.timeout_minutes {
                v.tpl(t, Site::StepTimeout, &format!("{sp}.timeout-minutes"));
            }
            if let Some(YVal::Str(t)) = &s.continue_on_error {
                v.tpl(t, Site::StepContinueOnError, &format!("{sp}.continue-on-error"));
            }
            // Not in GitHub's table: as `continue-on-error`, the closest key (Appendix B).
            if let Some(YVal::Str(t)) = &s.background {
                v.tpl(t, Site::StepContinueOnError, &format!("{sp}.background"));
            }
            if let Some(u) = &s.uses {
                lint_uses(v.diags, u, s, prt, &format!("{sp}.uses"));
            }
        }
    }
}

/// Steps, with `parallel` groups' steps in place, each with its path.
fn flatten<'a>(steps: &'a [super::ir::Step], path: &str, out: &mut Vec<(String, &'a super::ir::Step)>) {
    for (i, s) in steps.iter().enumerate() {
        let sp = format!("{path}[{i}]");
        out.push((sp.clone(), s));
        flatten(&s.parallel, &format!("{sp}.parallel"), out);
    }
}

/// `wait` and `cancel` name earlier background steps (`KCI4007`).
fn background_refs(diags: &mut Vec<Diag>, steps: &[super::ir::Step], path: &str) {
    let mut background: Vec<&str> = Vec::new();
    for (i, s) in steps.iter().enumerate() {
        for id in s.wait.iter().chain(&s.cancel) {
            if !background.contains(&id.as_str()) {
                let what = if s.wait.contains(id) { "waits for" } else { "cancels" };
                diags.push(Diag::error("KCI4007", format!("this step {what} `{id}`, which is not an earlier background step"), s.range).at(format!("{path}.steps[{i}]")));
            }
        }
        if s.background.is_some()
            && let Some(id) = &s.id
        {
            background.push(id);
        }
    }
}

fn container(v: &mut V<'_>, c: &super::ir::Container, image: Site, creds: Site, env: Site, path: &str) {
    v.tpl(&c.image, image, &format!("{path}.image"));
    if let Some((u, p)) = &c.credentials {
        v.tpl(u, creds, &format!("{path}.credentials.username"));
        v.tpl(p, creds, &format!("{path}.credentials.password"));
    }
    v.env(&c.env, env, &format!("{path}.env"));
    for t in c.volumes.iter().chain(&c.options) {
        v.tpl(t, image, path);
    }
}

/// `needs` that name no job, a job needing itself, and cycles (Kahn's algorithm).
fn graph(wf: &Workflow, diags: &mut Vec<Diag>) {
    let ids: Vec<&str> = wf.jobs.iter().map(|j| j.id.as_str()).collect();
    for j in &wf.jobs {
        for n in &j.needs {
            let need = n.job();
            if need == j.id {
                diags.push(Diag::error("KCI4002", format!("job `{}` needs itself", j.id), j.range).at(format!("jobs.{}.needs", j.id)));
            } else if !ids.contains(&need) {
                let hint = suggest(need, ids.iter().copied()).map(|s| format!(" (did you mean `{s}`?)")).unwrap_or_default();
                diags.push(Diag::error("KCI4001", format!("job `{}` needs `{need}`, which is not a job here{hint}", j.id), j.range).at(format!("jobs.{}.needs", j.id)));
            }
        }
    }
    // Kahn's algorithm over each job's distinct needs of other jobs here (needs of itself or of
    // no job are reported above). Every job is ready at most once, so this ends on any input.
    let deps: Vec<Vec<usize>> = wf
        .jobs
        .iter()
        .enumerate()
        .map(|(i, j)| {
            let mut d: Vec<usize> = j.needs.iter().filter_map(|n| ids.iter().position(|x| *x == n.job())).filter(|k| *k != i).collect();
            d.sort_unstable();
            d.dedup();
            d
        })
        .collect();
    let mut indeg: Vec<usize> = deps.iter().map(Vec::len).collect();
    let mut ready: Vec<usize> = indeg.iter().enumerate().filter(|(_, d)| **d == 0).map(|(i, _)| i).collect();
    let mut seen = 0usize;
    while let Some(i) = ready.pop() {
        seen += 1;
        for (k, d) in deps.iter().enumerate() {
            if d.contains(&i)
                && let Some(n) = indeg.get_mut(k)
                && *n > 0
            {
                *n -= 1;
                if *n == 0 {
                    ready.push(k);
                }
            }
        }
    }
    if seen < wf.jobs.len() {
        let stuck: Vec<&str> = wf.jobs.iter().enumerate().filter(|(i, _)| indeg.get(*i).is_some_and(|d| *d > 0)).map(|(_, j)| j.id.as_str()).collect();
        let range = wf.jobs.iter().find(|j| stuck.contains(&j.id.as_str())).map(|j| j.range).unwrap_or_default();
        diags.push(Diag::error("KCI4002", format!("the needs of {} form a cycle", stuck.iter().map(|s| format!("`{s}`")).collect::<Vec<_>>().join(", ")), range).at("jobs"));
    }
}

/// Untrusted text GitHub's security guidance says never to interpolate into scripts.
const UNTRUSTED: &[&str] = &[
    "github.event.issue.title",
    "github.event.issue.body",
    "github.event.pull_request.title",
    "github.event.pull_request.body",
    "github.event.comment.body",
    "github.event.review.body",
    "github.event.review_comment.body",
    "github.event.pages.*.page_name",
    "github.event.commits.*.message",
    "github.event.commits.*.author.email",
    "github.event.commits.*.author.name",
    "github.event.head_commit.message",
    "github.event.head_commit.author.email",
    "github.event.head_commit.author.name",
    "github.event.pull_request.head.ref",
    "github.event.pull_request.head.label",
    "github.event.pull_request.head.repo.default_branch",
    "github.event.workflow_run.head_branch",
    "github.event.workflow_run.head_commit.message",
    "github.event.workflow_run.head_commit.author.email",
    "github.event.workflow_run.head_commit.author.name",
    "github.event.workflow_run.display_title",
    "github.event.discussion.title",
    "github.event.discussion.body",
    "github.head_ref",
];

fn path_text(p: &crate::expr::ContextPath) -> String {
    p.0.iter()
        .map(|s| match s {
            crate::expr::PathSeg::Name(n) => n.to_ascii_lowercase(),
            crate::expr::PathSeg::Index(i) => i.to_string(),
            crate::expr::PathSeg::Any => "*".to_owned(),
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// Is `r` (a reference, as [`path_text`] writes it) one of [`UNTRUSTED`], or inside one?
fn untrusted(r: &str) -> bool {
    UNTRUSTED.iter().any(|u| {
        let mut have = r.split('.');
        u.split('.').all(|want| have.next().is_some_and(|h| want == "*" || h == want))
    })
}

/// What the `KCI6004` fix needs to know about a `run:`.
struct RunCx<'a> {
    src: &'a str,
    step: &'a Step,
    dialect: Dialect,
    /// The step's shell, or its job's or workflow's default.
    shell: Option<&'a str>,
    /// The job runs on Windows, where the default shell is `pwsh`.
    windows: bool,
}

fn on_windows(j: &Job) -> bool {
    let labels: Vec<&Tpl> = match &j.runs_on {
        Some(RunsOn::Labels(ls)) => ls.iter().collect(),
        Some(RunsOn::Group { group, labels }) => group.iter().chain(labels).collect(),
        None => Vec::new(),
    };
    labels.iter().any(|l| l.src.to_ascii_lowercase().contains("windows"))
}

fn lint_run(diags: &mut Vec<Diag>, t: &Tpl, exprs: &[Expr], path: &str, cx: &RunCx<'_>) {
    let refs: Vec<String> = exprs.iter().flat_map(Expr::references).map(|p| path_text(&p)).collect();
    if let Some(bad) = refs.iter().find(|r| untrusted(r)) {
        let d = Diag::warning("KCI6004", format!("`{bad}` is untrusted input interpolated into a script, which allows script injection: pass it through `env:` and use the variable"), t.range).at(path);
        diags.push(match injection_fix(t, cx) {
            Some(edits) => d.with_fix("Move the value into `env:` and read it as an environment variable", edits),
            None => d,
        });
    }
    if refs.iter().any(|r| r.starts_with("secrets.")) {
        diags.push(Diag::warning("KCI6005", "a secret interpolated into a script can leak through the script file and process list: pass it through `env:`", t.range).at(path));
    }
    if t.src.contains("::set-output") || t.src.contains("::save-state") {
        diags.push(Diag::warning("KCI6006", "`::set-output` and `::save-state` are deprecated: write to `$GITHUB_OUTPUT` and `$GITHUB_STATE`", t.range).at(path));
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shell {
    Posix,
    Pwsh,
    Cmd,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Quote {
    None,
    Single,
    Double,
}

/// The fix for `KCI6004` (W02-S05): each `${{ }}` of the script that reads untrusted input moves
/// into the step's `env:`, and the script reads the variable instead, quoted for its shell (a
/// variable is data to the shell; interpolated text is code). `None` when that can't be done
/// safely: a flow-style step or `env`, a shell whose syntax isn't known, or a place in the file
/// that doesn't hold the text it should (a quoted scalar with escapes, say).
fn injection_fix(t: &Tpl, cx: &RunCx<'_>) -> Option<Vec<Edit>> {
    let shell = match cx.shell {
        Some(s) if s.contains("${{") => return None,
        Some(s) => s.split_whitespace().next().unwrap_or_default(),
        None if cx.windows => "pwsh",
        None => "bash",
    };
    let shell = match shell {
        "bash" | "sh" => Shell::Posix,
        "pwsh" | "powershell" => Shell::Pwsh,
        "cmd" => Shell::Cmd,
        _ => return None,
    };
    let lines: Vec<&str> = cx.src.split('\n').collect();
    let opts = options(Site::StepRun, cx.dialect);
    let mut moved: Vec<(String, String)> = Vec::new();
    let mut edits = Vec::new();
    for (a, b) in template_spans(&t.src, cx.dialect).ok()? {
        let body = t.src.get(a..b)?.trim();
        let Ok(e) = Expr::parse_with(body, &opts) else { continue };
        let Some(r) = e.references().iter().map(path_text).find(|r| untrusted(r)) else { continue };
        let start = a.checked_sub(3)?;
        let outer = t.src.get(start..b + 2)?;
        let len = u32::try_from(outer.chars().count()).ok()?;
        let at = locate(t, start, None).start;
        if text_at(&lines, at, outer.chars().count())? != outer {
            return None;
        }
        let name = match moved.iter().find(|(x, _)| x == body) {
            Some((_, n)) => n.clone(),
            None => {
                let n = var_name(&r, cx.step, &moved);
                moved.push((body.to_owned(), n.clone()));
                n
            }
        };
        let before = t.src.get(..start)?;
        let text = match shell {
            Shell::Posix => match quote_state(before, '\\') {
                Quote::None => format!("\"${{{name}}}\""),
                Quote::Double => format!("${{{name}}}"),
                Quote::Single => format!("'\"${{{name}}}\"'"),
            },
            Shell::Pwsh => match quote_state(before, '`') {
                Quote::Single => return None,
                _ => format!("$env:{name}"),
            },
            Shell::Cmd => format!("%{name}%"),
        };
        edits.push(Edit { range: Range { start: at, end: Pos { col: at.col + len, ..at } }, text });
    }
    if moved.is_empty() {
        return None;
    }
    // The new entries go first in the step's `env`, or start the step when it has none.
    let s = cx.step;
    let (at, open) = match s.env_range {
        Some(r) if !s.env.is_empty() && s.env.iter().all(|(k, _)| !k.is_empty()) => (r.start, false),
        Some(_) => return None,
        None => (s.range.start, true),
    };
    if char_at(&lines, at)? == '{' {
        return None;
    }
    let indent = " ".repeat(usize::try_from(at.col.checked_sub(1)?).ok()?);
    let mut text = if open { "env:\n".to_owned() } else { String::new() };
    for (i, (body, name)) in moved.iter().enumerate() {
        if open {
            text.push_str(&format!("{indent}  "));
        } else if i > 0 {
            text.push_str(&indent);
        }
        text.push_str(&format!("{name}: {}\n", yaml_scalar(&format!("${{{{ {body} }}}}"))));
    }
    text.push_str(&indent);
    edits.insert(0, Edit { range: Range { start: at, end: at }, text });
    edits.sort_by_key(|e| (e.range.start.line, e.range.start.col));
    Some(edits)
}

/// `n` characters of the file from `at`.
fn text_at(lines: &[&str], at: Pos, n: usize) -> Option<String> {
    let line = lines.get(usize::try_from(at.line.checked_sub(1)?).ok()?)?;
    let skip = usize::try_from(at.col.checked_sub(1)?).ok()?;
    let out: String = line.chars().skip(skip).take(n).collect();
    (out.chars().count() == n).then_some(out)
}

fn char_at(lines: &[&str], at: Pos) -> Option<char> {
    text_at(lines, at, 1)?.chars().next()
}

/// Where a script is at its end, quote-wise: outside quotes, or inside single or double ones.
/// `esc` is the shell's escape character (`\` for POSIX shells, `` ` `` for PowerShell);
/// comments are skipped.
fn quote_state(script: &str, esc: char) -> Quote {
    let mut q = Quote::None;
    let (mut escaped, mut comment, mut prev) = (false, false, '\n');
    for c in script.chars() {
        if comment {
            comment = c != '\n';
        } else if escaped {
            escaped = false;
        } else {
            match q {
                Quote::Single if c == '\'' => q = Quote::None,
                Quote::Single => {}
                Quote::Double if c == esc => escaped = true,
                Quote::Double if c == '"' => q = Quote::None,
                Quote::Double => {}
                Quote::None if c == esc => escaped = true,
                Quote::None if c == '\'' => q = Quote::Single,
                Quote::None if c == '"' => q = Quote::Double,
                Quote::None if c == '#' && prev.is_whitespace() => comment = true,
                Quote::None => {}
            }
        }
        prev = c;
    }
    q
}

/// An environment variable's name for a reference: `github.event.pull_request.title` →
/// `PULL_REQUEST_TITLE`, unique within the step.
fn var_name(r: &str, s: &Step, taken: &[(String, String)]) -> String {
    let tail = r.strip_prefix("github.event.").or_else(|| r.strip_prefix("github.")).unwrap_or(r);
    let base: String = tail.split('.').filter(|p| *p != "*").collect::<Vec<_>>().join("_").chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect();
    let free = |n: &str| !s.env.iter().any(|(k, _)| k.eq_ignore_ascii_case(n)) && !taken.iter().any(|(_, t)| t.eq_ignore_ascii_case(n));
    if free(&base) {
        return base;
    }
    (2..).map(|i| format!("{base}_{i}")).find(|n| free(n)).unwrap_or(base)
}

/// `v` as a YAML scalar: plain when that reads back the same, single-quoted otherwise.
fn yaml_scalar(v: &str) -> String {
    if v.contains(": ") || v.contains(" #") || v.contains('\'') || v.contains('"') || v.ends_with(':') {
        format!("'{}'", v.replace('\'', "''"))
    } else {
        v.to_owned()
    }
}

fn lint_uses(diags: &mut Vec<Diag>, u: &str, step: &super::ir::Step, prt: bool, path: &str) {
    if u.starts_with("./") || u.starts_with("$/") || u.starts_with("docker://") || u.starts_with("kiln/") || u.starts_with("kiln://") {
        return;
    }
    if let Some((repo, r)) = u.split_once('@') {
        let sha = r.len() == 40 && r.bytes().all(|b| b.is_ascii_hexdigit());
        if !sha {
            diags.push(Diag::info("KCI6002", format!("`{u}` follows a tag or branch: pin it to a full commit SHA so it can't change under you"), step.range).at(path));
        }
        if prt && repo.eq_ignore_ascii_case("actions/checkout") {
            let head = step.with.iter().any(|(k, v)| k == "ref" && v.as_str().is_some_and(|s| s.contains("pull_request.head") || s.contains("head_ref")));
            if head {
                diags.push(
                    Diag::warning("KCI6003", "`pull_request_target` runs with the base repository's secrets; checking out the pull request's head runs untrusted code with them", step.range).at(path),
                );
            }
        }
    }
}

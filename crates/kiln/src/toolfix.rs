//! A job that can't run for want of a tool: what's missing, and where (`GET /api/jobs/:id/tools`),
//! and an agent kiln starts on that runner to install it, the job run again once it has
//! (`POST /api/jobs/:id/tools/fix`).
//!
//! What's missing comes from two places:
//! - a failed job's step whose log says a command wasn't there ("command not found", as bash, zsh,
//!   dash, `env` and Windows say it; "No developer tools were found" on a Mac without Xcode's
//!   command-line tools);
//! - a job waiting for a runner whose facts meet its `requires` (docs/ci.md W05-R22): for each
//!   runner its labels fit, the constraints its facts don't meet.
//!
//! The agent is started as the person who asks, through the route table (`ops::call`): asking needs
//! `runs.rerun` on the job's repository, the agent `agents.run` as starting any agent does, and it
//! has at most their permissions. Its brief is to install only what's missing and to ask a person
//! when a step needs one. When it has settled, kiln asks the runner to probe its facts again: a
//! failed job is run again (its run's failed jobs, as Retry does); a waiting one goes there by
//! itself once its runner meets it.

use std::sync::Arc;
use std::time::Duration;

use http::StatusCode;
use serde::Serialize;
use serde_json::{Value, json};

use crate::hub::Hub;
use crate::store::{Job, Status};

/// A tool's stand-in for "Xcode's command-line tools": macOS answers its `git`, `make` and
/// compilers with a stub that asks to install them.
pub const XCODE_TOOLS: &str = "xcode-command-line-tools";

/// How much of a failed step's log is read for what wasn't there: its end, where it failed.
const LOG_TAIL: u64 = 256 * 1024;

/// How long kiln follows a fix's agent before it stops waiting (the agent goes on).
const FOLLOW_FOR: Duration = Duration::from_secs(3 * 60 * 60);

/// How long a fix that ended is remembered (seconds): its job's page says how it went.
const FORGET_AFTER: i64 = 7 * 24 * 60 * 60;

/// A fix under way, or how it ended, for one job.
#[derive(Debug, Clone, Serialize)]
pub struct Fix {
    pub agent: String,
    pub runner: String,
    pub tools: Vec<String>,
    /// `working`, `needs_you` (the agent waits on a person), `checking` (the runner probes its
    /// facts again), `retried` (the job runs again), `waiting` (a waiting job's runner meets it
    /// now), `still_missing`, `stopped` (kiln stopped following it).
    pub state: String,
    pub note: Option<String>,
    pub by: String,
    pub started_at: i64,
}

/// What a job lacks, and where.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Missing {
    /// Tools a failed step's log says weren't there, newest last.
    pub tools: Vec<String>,
    /// The runner the job failed on.
    pub on: Option<String>,
    /// The log's line that says so.
    pub why: Option<String>,
    /// Why no agent can install them where it failed ([`agent_cannot`], or a pod's image lacks
    /// them): its page says so instead of offering one.
    pub no_agent: Option<String>,
    /// For a job waiting on its `requires`: each runner its labels fit, with what it doesn't meet.
    pub runners: Vec<Unmet>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Unmet {
    pub runner: String,
    /// The constraints its facts don't meet.
    pub unmet: Vec<String>,
    /// The tools they name.
    pub tools: Vec<String>,
    /// Whether an agent there can fix it: installing those is all it takes ([`only_tools`]: not for
    /// `os == macos`), and the runner hosts agents.
    pub fixable: bool,
    /// Why no agent can work there ([`agent_cannot`]).
    pub no_agent: Option<String>,
}

/// Why no agent can work on `runner` now, if none can: kiln's agents run in a connected runner's
/// terminals, never in a pod.
pub fn agent_cannot(a: Option<&crate::hub::AgentLive>, runner: &str) -> Option<String> {
    match a {
        None => Some(format!("kiln doesn't know {runner} now")),
        Some(a) if a.ephemeral => Some(format!("{runner} is a pod, which hosts no agent")),
        Some(a) if !a.connected => Some(format!("{runner} is away: kiln offers an agent once it's back")),
        Some(a) if !a.terminal => Some(format!("{runner}'s runner was started without terminals, so it hosts no agent")),
        Some(_) => None,
    }
}

/// The commands a log says weren't there, each once, as they come: in the last 400 lines.
pub fn not_found(log: &str) -> Vec<(String, String)> {
    let lines: Vec<&str> = log.lines().collect();
    let mut out: Vec<(String, String)> = Vec::new();
    for line in lines.iter().skip(lines.len().saturating_sub(400)) {
        let l = strip_ansi(line);
        let l = l.trim();
        if let Some(t) = tool_of(l)
            && !out.iter().any(|(o, _)| *o == t)
        {
            out.push((t, l.to_owned()));
        }
    }
    out
}

/// The command a shell's line says wasn't there.
fn tool_of(l: &str) -> Option<String> {
    if l.contains("No developer tools were found") {
        return Some(XCODE_TOOLS.to_owned());
    }
    // bash: npm: command not found · bash: line 3: npm: command not found · /bin/sh: npm: command not found
    if let Some(rest) = l.strip_suffix(": command not found") {
        return command(rest.rsplit(": ").next()?);
    }
    // zsh: command not found: npm
    if let Some((_, name)) = l.split_once("command not found: ") {
        return command(name);
    }
    // sh: 1: npm: not found (dash)
    if let Some(rest) = l.strip_suffix(": not found")
        && (rest.starts_with("sh:") || rest.starts_with("/bin/sh:") || rest.starts_with("dash:"))
    {
        return command(rest.rsplit(": ").next()?);
    }
    // env: ‘npm’: No such file or directory
    if let Some(rest) = l.strip_prefix("env: ").or_else(|| l.strip_prefix("/usr/bin/env: "))
        && let Some(name) = rest.strip_suffix(": No such file or directory")
    {
        return command(name);
    }
    // 'npm' is not recognized as an internal or external command (cmd)
    if let Some(rest) = l.strip_prefix('\'')
        && let Some((name, tail)) = rest.split_once('\'')
        && tail.trim_start().starts_with("is not recognized as an internal or external command")
    {
        return command(name);
    }
    // The term 'npm' is not recognized as a name of a cmdlet (PowerShell)
    if let Some((_, rest)) = l.split_once("The term '")
        && let Some((name, tail)) = rest.split_once('\'')
        && tail.contains("is not recognized as")
    {
        return command(name);
    }
    None
}

/// A command's name as a line quotes it: its path's last part, nothing that isn't a name.
fn command(name: &str) -> Option<String> {
    let n = name.trim().trim_matches(|c: char| matches!(c, '\'' | '"' | '`' | '‘' | '’'));
    let n = n.rsplit(['/', '\\']).next().unwrap_or(n);
    let ok = !n.is_empty() && n.len() <= 64 && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'));
    ok.then(|| n.to_owned())
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for d in chars.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// The tools a job's `requires` name (`tools has node`, `tools has 'docker buildx'`).
pub fn tools_in(requires: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in requires.iter().flat_map(|r| tool_terms(r).0) {
        if !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

/// Whether installing tools is all a constraint asks (`tools has node && tools has cargo`): without
/// its `tools has` terms, only `&&`, `||` and parentheses are left of it.
pub fn only_tools(r: &str) -> bool {
    let (names, left) = tool_terms(r);
    !names.is_empty() && left.chars().all(|c| c.is_whitespace() || matches!(c, '&' | '|' | '(' | ')'))
}

/// The tools a constraint names, and what's left of it without them.
fn tool_terms(r: &str) -> (Vec<String>, String) {
    let (mut names, mut left, mut rest) = (Vec::new(), String::new(), r);
    while let Some((at, len)) = ["tools has ", "tools contains "].iter().filter_map(|p| rest.find(p).map(|i| (i, p.len()))).min() {
        left.push_str(rest.get(..at).unwrap_or_default());
        let after = rest.get(at + len..).unwrap_or_default().trim_start();
        let (name, tail) = match after.chars().next() {
            Some(q @ ('\'' | '"')) => {
                let body = after.get(1..).unwrap_or_default();
                body.split_once(q).unwrap_or((body, ""))
            }
            _ => {
                let end = after.find(|c: char| c.is_whitespace() || matches!(c, ')' | '&' | '|')).unwrap_or(after.len());
                (after.get(..end).unwrap_or_default(), after.get(end..).unwrap_or_default())
            }
        };
        if !name.is_empty() {
            names.push(name.to_owned());
        }
        rest = tail;
    }
    left.push_str(rest);
    (names, left)
}

/// What `job` lacks: from its failed step's log, or for a waiting one, from its runners' facts.
pub async fn missing(hub: &Hub, job: &Job) -> Missing {
    let mut m = Missing::default();
    if matches!(job.status, Status::Failed) {
        for s in job.steps.iter().filter(|s| matches!(s.status, Status::Failed)) {
            let log = hub.logs.tail(&job.id, s.idx, LOG_TAIL).await;
            let found = not_found(&String::from_utf8_lossy(&log));
            if let Some((_, why)) = found.last() {
                m.why = Some(why.clone());
            }
            for (t, _) in found {
                if !m.tools.contains(&t) {
                    m.tools.push(t);
                }
            }
        }
        if !m.tools.is_empty()
            && let Some(runner) = job.agent.clone()
        {
            // A pod's tools are its image's: an agent on the runner would install them nowhere.
            m.no_agent = if job.executor.as_deref() == Some("k8s") {
                Some(format!("it ran in a pod: add {} to the job's image or toolchain", m.tools.join(", ")))
            } else {
                agent_cannot(hub.agents.lock().get(&runner), &runner)
            };
            m.on = Some(runner);
        }
    }
    if matches!(job.status, Status::Queued | Status::Pending)
        && let Some(spec) = &job.spec
        && !spec.requires.is_empty()
    {
        m.runners = crate::hub::unmet(hub, &job.labels, &spec.requires);
    }
    m
}

/// `GET /api/jobs/:id/tools`: what it lacks, and the fix under way for it, if any.
pub async fn report(hub: &Hub, id: &str) -> Result<Value, (StatusCode, String)> {
    let job = hub.store.job(id.to_owned()).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, "no such job".to_owned()))?;
    let m = missing(hub, &job).await;
    let fix = hub.tool_fixes.lock().get(id).cloned();
    Ok(json!({ "job": id, "missing": m, "fix": fix }))
}

#[derive(Debug, serde::Deserialize)]
pub struct Ask {
    /// The runner to install them on: by default the one the job failed on.
    pub runner: Option<String>,
    /// The agent's harness (claude by default).
    pub kind: Option<String>,
    /// Its permission mode, at most the asker's (the harness's default when left out).
    pub mode: Option<String>,
}

/// `POST /api/jobs/:id/tools/fix`: an agent on the runner installs what the job lacks; kiln
/// follows it, checks the runner's facts again, and runs the job again.
pub async fn fix(hub: Arc<Hub>, caller: crate::authz::Caller, id: &str, ask: Ask) -> Result<Value, (StatusCode, String)> {
    let job = hub.store.job(id.to_owned()).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, "no such job".to_owned()))?;
    let m = missing(hub.as_ref(), &job).await;
    let (runner, tools) = match ask.runner.clone().or_else(|| m.on.clone()) {
        Some(r) if m.on.as_deref() == Some(r.as_str()) && !m.tools.is_empty() => match &m.no_agent {
            Some(why) => return Err((StatusCode::CONFLICT, format!("no agent can install {} for {id}: {why}", m.tools.join(", ")))),
            None => (r, m.tools.clone()),
        },
        Some(r) => match m.runners.iter().find(|u| u.runner == r) {
            Some(u) if u.fixable => (r, u.tools.clone()),
            Some(Unmet { no_agent: Some(why), .. }) => return Err((StatusCode::CONFLICT, format!("no agent can work on {r}: {why}"))),
            Some(u) => return Err((StatusCode::BAD_REQUEST, format!("{r} needs more than tools for {id}: {}", u.unmet.join(", ")))),
            None => return Err((StatusCode::BAD_REQUEST, format!("{id} lacks nothing kiln can name on {r}"))),
        },
        None => match m.runners.iter().find(|u| u.fixable) {
            Some(u) => (u.runner.clone(), u.tools.clone()),
            None => return Err((StatusCode::BAD_REQUEST, format!("{id} lacks no tool kiln can name"))),
        },
    };
    let run = hub.store.run(job.run_id.clone()).await.map_err(internal)?;
    let repo = run.as_ref().map(|r| r.repo_name.clone()).unwrap_or_default();
    let number = run.as_ref().map(|r| r.number).unwrap_or_default();
    let prompt = brief(&runner, &job.display, &repo, number, &tools, m.why.as_deref(), &m.runners);
    let kind = ask.kind.clone().unwrap_or_else(|| "claude".to_owned());
    let mut input = json!({ "kind": kind, "on": runner, "cwd": "~", "prompt": prompt, "name": format!("Install {} on {runner}", tools.join(", ")) });
    if let Some(mode) = &ask.mode
        && let Some(o) = input.as_object_mut()
    {
        o.insert("mode".into(), json!(mode));
    }
    // The job is this ask's while its agent starts: one asked for meanwhile is told so.
    let f = Fix { agent: String::new(), runner: runner.clone(), tools: tools.clone(), state: "working".into(), note: None, by: caller.name.clone(), started_at: crate::util::now_unix() };
    let before = {
        let mut g = hub.tool_fixes.lock();
        if g.get(id).is_some_and(under_way) {
            return Err((StatusCode::CONFLICT, "an agent is already installing what this job lacks".into()));
        }
        g.retain(|_, o| under_way(o) || f.started_at - o.started_at < FORGET_AFTER);
        g.insert(id.to_owned(), f.clone())
    };
    // The agent didn't start: the job's fix is what it was.
    let undo = || {
        let mut g = hub.tool_fixes.lock();
        match &before {
            Some(b) => {
                g.insert(id.to_owned(), b.clone());
            }
            None => {
                g.remove(id);
            }
        }
    };
    let cx = crate::ops::Cx { hub: Arc::clone(&hub), caller: caller.clone() };
    let started = match crate::ops::call(cx.clone(), "agents.start", input).await {
        Ok(v) => v,
        Err(e) => {
            undo();
            return Err(e);
        }
    };
    let agent = started.pointer("/agents/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
    if agent.is_empty() {
        undo();
        return Err((StatusCode::BAD_GATEWAY, format!("no agent started on {runner}: {started}")));
    }
    let f = Fix { agent: agent.clone(), ..f };
    hub.tool_fixes.lock().insert(id.to_owned(), f.clone());
    hub.store.audit(caller.name.clone(), "job.tools.fix".into(), format!("{id}: {} on {runner}, agent {agent}", tools.join(", "))).await;
    hub.emit_job(id).await;
    let (hub2, id2) = (Arc::clone(&hub), id.to_owned());
    crate::rt::spawn(async move { follow(hub2, cx, id2).await });
    Ok(json!({ "fix": f }))
}

/// The agent's first message.
fn brief(runner: &str, job: &str, repo: &str, number: i64, tools: &[String], why: Option<&str>, runners: &[Unmet]) -> String {
    let what = tools.iter().map(|t| if t == XCODE_TOOLS { "Xcode's command-line tools".to_owned() } else { format!("`{t}`") }).collect::<Vec<_>>().join(", ");
    let reason = match why {
        Some(w) => format!("failed on this machine because a command wasn't there: `{w}`"),
        None => {
            let unmet = runners.iter().find(|u| u.runner == runner).map(|u| u.unmet.join(", ")).unwrap_or_default();
            format!("is waiting for a runner that meets its requirements, and this one doesn't meet: {unmet}")
        }
    };
    format!(
        "kiln asks you to set up this machine ({runner}) for CI: job \"{job}\" (run #{number} of {repo}) {reason}.\n\n\
         Install what it needs: {what}. kiln's runner runs as a service (launchd on macOS, systemd on Linux) whose PATH is short: on a Mac it adds \
         /opt/homebrew/bin, /usr/local/bin and ~/.cargo/bin. Install each so that PATH finds it, then check it runs (`--version`).\n\n\
         Change nothing else on this machine. If a step needs a person (a password, a dialog such as the installer of Xcode's command-line tools, a \
         sign-in), say exactly what they'll see and what to do, and wait for them. When everything runs, say it's done: kiln then checks this \
         machine's tools again and runs the job again."
    )
}

/// Follows a fix's agent until it's done (idle, stopped or ended), then checks the runner and runs
/// the job again. While it waits on a person, is paused or its machine is away, kiln waits for a
/// change rather than for it to settle (which it already has).
async fn follow(hub: Arc<Hub>, cx: crate::ops::Cx, id: String) {
    let Some(agent) = hub.tool_fixes.lock().get(&id).map(|f| f.agent.clone()) else { return };
    let until = std::time::Instant::now() + FOLLOW_FOR;
    let (mut wait_for, mut misses) = ("settled", 0u32);
    loop {
        if std::time::Instant::now() >= until || misses > 10 {
            set(&hub, &id, "stopped", Some("kiln stopped following the agent: it may still be at work".into())).await;
            return;
        }
        if crate::ops::call(cx.clone(), "agents.wait", json!({ "agent": agent, "until": wait_for, "timeout_s": 600, "hand_over": false })).await.is_err() {
            misses += 1;
            crate::rt::sleep(Duration::from_secs(30)).await;
        }
        let stand = crate::ops::call(cx.clone(), "agents.get", json!({ "agent": agent })).await.ok().and_then(|v| v.pointer("/agent/stand").and_then(Value::as_str).map(str::to_owned));
        match stand.as_deref() {
            Some("idle" | "stopped" | "ended") => break,
            Some("working") => {
                set(&hub, &id, "working", None).await;
                wait_for = "settled";
            }
            Some("needs_you") => {
                set(&hub, &id, "needs_you", Some("the agent waits on a person: open it".into())).await;
                wait_for = "change";
            }
            Some(other) => {
                set(&hub, &id, "working", Some(format!("the agent is {other}"))).await;
                wait_for = "change";
            }
            None => {
                misses += 1;
                crate::rt::sleep(Duration::from_secs(30)).await;
            }
        }
    }
    set(&hub, &id, "checking", None).await;
    check(hub, &id).await;
}

/// After the agent: the runner's facts again, then the job again.
async fn check(hub: Arc<Hub>, id: &str) {
    let Some(fix) = hub.tool_fixes.lock().get(id).cloned() else { return };
    let _ = crate::api::probe_agent(&hub, &fix.runner).await;
    let Ok(Some(job)) = hub.store.job(id.to_owned()).await else { return };
    let facts = hub.agents.lock().get(&fix.runner).and_then(|a| a.facts.as_deref().cloned());
    let have = |t: &str| facts.as_ref().is_some_and(|f| f.tools.contains_key(fact_key(t)));
    let missing: Vec<String> = fix.tools.iter().filter(|t| fact_known(t) && !have(t)).cloned().collect();
    if !missing.is_empty() {
        set(&hub, id, "still_missing", Some(format!("{} still lacks {} after the agent", fix.runner, missing.join(", ")))).await;
        return;
    }
    match job.status {
        Status::Failed | Status::TimedOut | Status::Cancelled => match hub.store.retry_run(job.run_id.clone(), true).await {
            Ok(_) => {
                hub.emit_run(&job.run_id).await;
                hub.wake.notify();
                hub.store.audit(fix.by.clone(), "run.retry".into(), format!("{} (after an agent installed {} on {})", job.run_id, fix.tools.join(", "), fix.runner)).await;
                set(&hub, id, "retried", None).await;
            }
            Err(e) => set(&hub, id, "still_missing", Some(format!("couldn't run it again: {e:#}"))).await,
        },
        Status::Queued | Status::Pending => {
            hub.wake.notify();
            set(&hub, id, "waiting", Some(format!("{} meets it now: the job goes there when it has a slot", fix.runner))).await;
        }
        // Someone ran it again meanwhile.
        _ => set(&hub, id, "retried", Some("it ran again meanwhile".into())).await,
    }
}

/// The fact a runner's probe reports a command as (its tool's name in `facts.tools`).
fn fact_key(t: &str) -> &str {
    match t {
        "npm" | "npx" | "corepack" => "node",
        "rustc" | "rustup" => "cargo",
        "pip3" | "python" => "python3",
        "buildx" => "docker buildx",
        other => other,
    }
}

/// Whether the runner's probe reports this tool at all (`runner::probe::TOOLS`): the others are
/// checked by running the job.
fn fact_known(t: &str) -> bool {
    crate::runner::probe::TOOLS.iter().any(|(name, _, _)| *name == fact_key(t))
}

/// A fix whose agent is still at work, or whose runner kiln is checking: one at a time per job.
fn under_way(f: &Fix) -> bool {
    matches!(f.state.as_str(), "working" | "needs_you" | "checking")
}

async fn set(hub: &Hub, id: &str, state: &str, note: Option<String>) {
    let changed = match hub.tool_fixes.lock().get_mut(id) {
        Some(f) if f.state != state || f.note != note => {
            f.state = state.to_owned();
            f.note = note;
            true
        }
        _ => false,
    };
    if changed {
        hub.emit_job(id).await;
    }
}

fn internal(e: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shells' ways of saying a command wasn't there, as release run #1171's macOS job said it
    /// on a Mac with no developer tools.
    #[test]
    fn a_log_says_which_command_wasnt_there() {
        let log = "[kiln] job macOS (ARM64), on a Mac · attempt 1 · agent adas-mac-studio-3.local\n\
                   [kiln] git checkout failed (git clone: xcode-select: note: No developer tools were found, requesting install.\n\
                   bash: npm: command not found\n\
                   \n[kiln] step 1 exited with Some(127)\n";
        let found: Vec<String> = not_found(log).into_iter().map(|(t, _)| t).collect();
        assert_eq!(found, vec![XCODE_TOOLS.to_owned(), "npm".to_owned()]);
        for (line, tool) in [
            ("bash: line 3: cargo: command not found", "cargo"),
            ("/bin/sh: /usr/local/bin/node: command not found", "node"),
            ("zsh: command not found: rustup", "rustup"),
            ("sh: 1: python3: not found", "python3"),
            ("env: ‘bun’: No such file or directory", "bun"),
            ("'npm' is not recognized as an internal or external command,", "npm"),
            ("npm : The term 'npm' is not recognized as a name of a cmdlet, function, script file", "npm"),
            ("\u{1b}[31mbash: go: command not found\u{1b}[0m", "go"),
        ] {
            assert_eq!(not_found(line).first().map(|(t, _)| t.as_str()), Some(tool), "{line}");
        }
        assert!(not_found("error: could not compile `kiln`\ntest result: FAILED. 1 passed; 1 failed").is_empty());
        assert!(not_found("grep: pattern not found").is_empty(), "a tool's own words aren't a missing command");
    }

    /// The tools a job's `requires` name, as the release's macOS job names them.
    #[test]
    fn requires_name_their_tools() {
        let r: Vec<String> = ["os == macos", "arch == aarch64", "tools has cargo", "tools has node && tools has 'docker buildx'"].iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(tools_in(&r), vec!["cargo".to_owned(), "node".to_owned(), "docker buildx".to_owned()]);
        assert!(tools_in(&["disk.free >= 50Gi".to_owned()]).is_empty());
        // What an agent can fix by installing, and what it can't.
        assert!(only_tools("tools has cargo") && only_tools("(tools has node || tools has bun) && tools has 'docker buildx'"));
        for r in ["os == macos", "os == macos && tools has cargo", "!tools has docker", "disk.free >= 50Gi"] {
            assert!(!only_tools(r), "{r}");
        }
    }

    /// What a probe reports a command as, so kiln can tell it's there now.
    #[test]
    fn commands_map_to_the_facts_that_report_them() {
        assert_eq!(fact_key("npm"), "node");
        assert_eq!(fact_key("rustc"), "cargo");
        assert!(fact_known("npm") && fact_known("cargo") && !fact_known(XCODE_TOOLS) && !fact_known("jq"));
    }

    /// A job that failed at `npm` says so, and on which runner. Asking for its fix needs what
    /// starting any agent needs (`agents.run`, on the server), not only `runs.rerun` on the job's
    /// repository: a developer of one repository starts no agent on a runner. A runner that hosts
    /// no agent is said so, and no agent is asked of it. Neither claims the job's fix.
    #[test]
    fn a_fix_s_agent_needs_what_any_agent_needs() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let hub = crate::hub::tests::scratch_hub().await?;
            let repo = hub
                .store
                .upsert_repo(crate::store::Repo {
                    id: "r1".into(),
                    name: "hyper-light/kiln".into(),
                    clone_url: "https://example/kiln".into(),
                    branches: vec![],
                    poll_seconds: 60,
                    enabled: true,
                    created_at: 0,
                    last_polled_at: None,
                    last_error: None,
                    mode: "upstream".into(),
                    default_branch: None,
                    mirror: None,
                })
                .await?;
            let p = crate::pipeline::parse("name: release\njobs:\n  macos: {steps: [{run: npm ci}]}\n")?;
            let run = crate::store::Run {
                id: "run1".into(),
                repo_id: repo.id.clone(),
                repo_name: repo.name.clone(),
                number: 0,
                pipeline: "release".into(),
                event: "push".into(),
                branch: "main".into(),
                sha: "c0ffee".into(),
                message: String::new(),
                author: String::new(),
                status: Status::Queued,
                created_at: 1,
                started_at: None,
                finished_at: None,
                error: None,
                concurrency_group: None,
            };
            hub.store.create_run(run, crate::pipeline::plan(&p, &crate::pipeline::Context::default())?).await?;
            let job = hub.store.jobs("run1".into(), false).await?.first().map(|j| j.id.clone()).unwrap_or_default();
            hub.agents.lock().insert("adas-mac-studio-3".into(), crate::hub::tests::live_runner("adas-mac-studio-3", "macos", "aarch64"));
            assert!(hub.store.claim_job(job.clone(), "adas-mac-studio-3".into(), "baremetal".into()).await?);
            hub.logs.append(&job, 0, b"$ npm ci\nbash: line 1: npm: command not found\n");
            hub.logs.finish(&job);
            hub.store.step_update(job.clone(), 0, Status::Failed, Some(127)).await?;
            hub.store.finish_job(job.clone(), Status::Failed, Some(127), None, None, None).await?;

            let lacks = report(&hub, &job).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            assert_eq!(lacks.pointer("/missing/tools"), Some(&json!(["npm"])));
            assert_eq!(lacks.pointer("/missing/on"), Some(&json!("adas-mac-studio-3")));
            assert_eq!(lacks.pointer("/missing/why"), Some(&json!("bash: line 1: npm: command not found")));
            assert_eq!(lacks.pointer("/missing/no_agent"), Some(&Value::Null));

            let grant = crate::authz::Grant::parse("developer@repo:hyper-light/kiln").map_err(anyhow::Error::msg)?;
            let dev = crate::authz::Caller::with(crate::authz::gate::Kind::User, "user:dev", Some(crate::authz::Rights { grants: vec![grant], ..Default::default() }), None);
            let refused = fix(Arc::clone(&hub), dev, &job, Ask { runner: None, kind: None, mode: None }).await;
            assert_eq!(refused.err().map(|(c, _)| c), Some(StatusCode::FORBIDDEN));
            assert!(hub.tool_fixes.lock().get(&job).is_none(), "a refused ask claims nothing");

            if let Some(a) = hub.agents.lock().get_mut("adas-mac-studio-3") {
                a.terminal = false;
            }
            let lacks = report(&hub, &job).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            assert!(lacks.pointer("/missing/no_agent").and_then(Value::as_str).is_some_and(|w| w.contains("without terminals")), "{lacks}");
            let shut = fix(Arc::clone(&hub), crate::authz::Caller::owner(), &job, Ask { runner: None, kind: None, mode: None }).await;
            assert_eq!(shut.err().map(|(c, _)| c), Some(StatusCode::CONFLICT));
            assert!(hub.tool_fixes.lock().get(&job).is_none(), "nor does one no agent can do");
            Ok(())
        })
    }
}

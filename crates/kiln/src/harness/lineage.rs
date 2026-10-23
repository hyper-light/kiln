//! Agents that run agents (docs/agents.md, "Agents that run agents"): an agent an agent starts is
//! its child, and it gets no more than its parent has.
//!
//! - **Who the parent is** comes from how the caller reached kiln (its MCP token, its runner
//!   vouching for it, the machine's socket seeing its process: `Caller::agent`), never from what
//!   it asks. The child's spec carries it (`Spec::parent`), so it holds across restarts and moves.
//! - **No more than its parent.** Every harness's permission modes stand in one order: plan < ask
//!   < accept edits < auto < bypass ([`Level`]). A child's mode is at most its parent's (checked as
//!   it starts); bypass needs the parent's own `allow_bypass`; a parent whose commands run only on
//!   its own machine (`reach` off) has children that do too, on that machine.
//! - **Limits.** Chains go at most `[agents] max_depth` deep below the agent a person started, and
//!   one agent has at most `[agents] max_children` children running at once.
//! - **Where.** `on` is a machine's name, or what it needs (`linux/amd64`, `+gpu`, `cpus>=16`),
//!   picked as reach picks: of the machines that suit and have the harness, the idlest.

use http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::kinds::{Kind, Protocol};
use super::model::Status;
use super::session::{Info, Spec};
use crate::hub::Hub;

/// A refusal: an HTTP status and why, in words the caller can act on.
pub type Refused = (StatusCode, String);

fn refuse(msg: impl Into<String>) -> Refused {
    (StatusCode::FORBIDDEN, msg.into())
}

fn bad(msg: impl Into<String>) -> Refused {
    (StatusCode::BAD_REQUEST, msg.into())
}

/// How much a permission mode lets an agent do without asking a person, the same for every
/// harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// It reads and plans; it changes nothing.
    Plan,
    /// It asks before edits and commands.
    Ask,
    /// It edits files without asking; it asks before commands.
    AcceptEdits,
    /// It goes ahead with what's safe (a classifier, a sandbox) and asks about the rest.
    Auto,
    /// It skips every check.
    Bypass,
}

impl Level {
    pub fn words(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Ask => "ask",
            Self::AcceptEdits => "accept edits",
            Self::Auto => "auto",
            Self::Bypass => "bypass",
        }
    }
}

/// The mode a harness starts in when none is given.
fn default_mode(k: &Kind) -> &'static str {
    if k.id == "devin" {
        return "accept-edits";
    }
    match k.protocol {
        Protocol::Acp(_) => super::acp::KILN_ASK,
        _ => k.modes().1,
    }
}

/// An ACP agent's own mode, placed by its words (each agent names its own: Gemini CLI's
/// `autoEdit` and `yolo`, opencode's `plan` and `build`, Goose's `approve` and `auto`).
pub(super) fn acp_level(mode: &str) -> Option<Level> {
    let m: String = mode.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_ascii_lowercase();
    let has = |w: &str| m.contains(w);
    Some(if has("yolo") || has("bypass") || has("danger") || has("fullaccess") || has("skip") {
        Level::Bypass
    } else if has("plan") || has("readonly") || has("read") || has("chat") {
        Level::Plan
    } else if has("edit") || has("accept") {
        Level::AcceptEdits
    } else if has("auto") || has("build") || has("smart") {
        Level::Auto
    } else if has("default") || has("ask") || has("approve") || has("manual") || has("normal") {
        Level::Ask
    } else {
        return None;
    })
}

/// Where mode `mode` of harness `kind` stands in [`Level`]'s order (an empty mode is the harness's
/// own default). `None`: a harness or mode kiln can't place.
pub fn level(kind: &str, mode: &str) -> Option<Level> {
    let k = Kind::get(kind)?;
    let mode = match mode.trim() {
        "" => default_mode(k),
        m => m,
    };
    match k.protocol {
        Protocol::ClaudeStreamJson => match mode {
            "plan" => Some(Level::Plan),
            // dontAsk never asks: it refuses whatever isn't already allowed.
            "default" | "dontAsk" => Some(Level::Ask),
            "acceptEdits" => Some(Level::AcceptEdits),
            "auto" => Some(Level::Auto),
            "bypassPermissions" => Some(Level::Bypass),
            _ => None,
        },
        Protocol::CodexAppServer => match mode {
            "read-only" => Some(Level::Plan),
            "untrusted" => Some(Level::Ask),
            // It edits and runs commands in its folder's sandbox without asking.
            "auto" => Some(Level::Auto),
            "full-access" => Some(Level::Bypass),
            _ => None,
        },
        Protocol::AgyStreamJson => match mode {
            super::antigravity::PLAN => Some(Level::Plan),
            super::antigravity::ASK => Some(Level::Ask),
            super::antigravity::ACCEPT_EDITS => Some(Level::AcceptEdits),
            super::antigravity::BYPASS => Some(Level::Bypass),
            _ => None,
        },
        Protocol::Acp(_) if k.id == "devin" => match mode {
            "plan" => Some(Level::Plan),
            "ask" => Some(Level::Ask),
            "accept-edits" => Some(Level::AcceptEdits),
            "smart" => Some(Level::Auto),
            "bypass" => Some(Level::Bypass),
            _ => None,
        },
        Protocol::Acp(_) => match mode {
            super::acp::KILN_ASK => Some(Level::Ask),
            super::acp::KILN_BYPASS => Some(Level::Bypass),
            other => acp_level(other),
        },
        Protocol::Terminal => None,
    }
}

/// Where an agent stands as a parent: its mode's level, an unplaceable one taken as ask (it asks
/// a person, as most modes do).
pub fn ceiling(parent: &Info) -> Level {
    level(&parent.kind, &parent.mode).unwrap_or(Level::Ask)
}

/// The modes of harness `k` kiln knows before it starts, with their levels, in its own order
/// (whether this machine could use bypass is its harness's to say as it starts).
fn modes_of(k: &Kind) -> Vec<(String, Level)> {
    let ids: Vec<String> = match k.protocol {
        Protocol::ClaudeStreamJson => super::claude::modes(true).into_iter().map(|m| m.id).collect(),
        Protocol::CodexAppServer => super::codex::modes(true).into_iter().map(|m| m.id).collect(),
        Protocol::AgyStreamJson => super::antigravity::modes().into_iter().map(|m| m.id).collect(),
        Protocol::Acp(_) if k.id == "devin" => ["plan", "ask", "accept-edits", "smart", "bypass"].map(str::to_owned).to_vec(),
        Protocol::Acp(_) => vec![super::acp::KILN_ASK.to_owned(), super::acp::KILN_BYPASS.to_owned()],
        Protocol::Terminal => Vec::new(),
    };
    ids.into_iter().filter_map(|id| level(k.id, &id).map(|l| (id, l))).collect()
}

/// The mode a child in harness `kind` starts in under a parent at `ceiling`: the one `asked` for,
/// when it's within it; with none asked, its harness's default when that's within it, else the
/// most its harness offers that is. Always one, so the harness's own settings (Claude Code's
/// `defaultMode`) can't start it higher. `bypass_ok`: the parent may use bypass itself. Other ACP
/// agents asked for nothing start in their own default mode (kiln learns their modes as they run).
pub fn child_mode(kind: &str, asked: Option<&str>, ceiling: Level, bypass_ok: bool) -> Result<Option<String>, String> {
    let k = Kind::get(kind).ok_or_else(|| format!("unknown harness {kind:?}"))?;
    let known = modes_of(k);
    let within: Vec<&(String, Level)> = known.iter().filter(|(_, l)| *l <= ceiling && (*l != Level::Bypass || bypass_ok)).collect();
    let list = |v: &[&(String, Level)]| v.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>().join(", ");
    match asked.map(str::trim).filter(|m| !m.is_empty()) {
        Some(mode) => {
            let Some(l) = level(kind, mode) else {
                return Err(format!("kiln can't tell where {}'s mode {mode:?} stands, so it can't hold it to its parent's (at most {}): pick one of {}", k.name, ceiling.words(), list(&within)));
            };
            let offered = matches!(k.protocol, Protocol::Acp(_)) || known.iter().any(|(id, _)| id == mode);
            if !offered {
                return Err(format!("{} has no mode {mode:?}: its modes are {}", k.name, known.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>().join(", ")));
            }
            if l > ceiling {
                return Err(format!(
                    "an agent can't start one with more than it has: {mode} is {}, above its own {}. Pick a mode at most {}{}",
                    l.words(),
                    ceiling.words(),
                    ceiling.words(),
                    if within.is_empty() { String::new() } else { format!(": {}", list(&within)) }
                ));
            }
            if l == Level::Bypass && !bypass_ok {
                return Err("bypass needs its parent's own permission to skip checks, and its parent was started without it".to_owned());
            }
            Ok(Some(mode.to_owned()))
        }
        None if matches!(k.protocol, Protocol::Acp(_)) && k.id != "devin" && ceiling >= Level::Ask => Ok(None),
        None => {
            let default = default_mode(k);
            if within.iter().any(|(id, _)| id == default) {
                return Ok(Some(default.to_owned()));
            }
            let top = within.iter().map(|(_, l)| *l).max().ok_or_else(|| format!("{} has no mode at most {}, its parent's: start a harness that has one (Claude Code and Codex can plan)", k.name, ceiling.words()))?;
            within.iter().find(|(_, l)| *l == top).map(|(id, _)| Some(id.clone())).ok_or_else(|| format!("{} has no mode at most {}", k.name, ceiling.words()))
        }
    }
}

/// Where kiln keeps which agent started agent `id` (beside its spec, for one whose machine forgot).
pub fn parent_key(id: &str) -> String {
    format!("agents/parent/{id}")
}

/// How deep agent `id` is: how many agents started it, one from the other (0: a person did).
pub async fn depth(hub: &Hub, id: &str) -> u32 {
    let mut n = 0u32;
    let mut at = id.to_owned();
    // A chain never loops (a parent exists before its child), but a bound costs nothing.
    for _ in 0..64 {
        let parent = match hub.harness.info(&at).and_then(|i| i.parent) {
            Some(p) => Some(p),
            None => hub.store.kv_get(parent_key(&at)).await.ok().flatten(),
        };
        match parent {
            Some(p) if !p.is_empty() => {
                n = n.saturating_add(1);
                at = p;
            }
            _ => break,
        }
    }
    n
}

/// Whether agent `id` descends from agent `ancestor` (a child of it, or of one of its children…).
/// Whether `caller` may steer agent `id`: send it messages, plans or goals, change or clear its
/// goals, interrupt or end it. A person may steer any agent they reach; an agent only itself and
/// the agents it started (and theirs).
pub fn may_steer(hub: &Hub, caller: &crate::authz::Caller, id: &str) -> Result<(), Refused> {
    let Some(me) = caller.agent.as_deref() else { return Ok(()) };
    if me == id || descends(&hub.harness.list(), id, me) {
        return Ok(());
    }
    Err((StatusCode::FORBIDDEN, format!("an agent steers and ends only the agents it started (and theirs): {} isn't one of {}'s", short(id), short(me))))
}

/// How an agent may send agent `id` a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// A person, the agent itself, or an agent it started (and theirs): in any mode, as its own.
    Own,
    /// Another agent that works for the same person: queued, its receiver told who sent it.
    Peer,
}

/// Whether `caller` may send agent `id` a message (docs/agents.md, "Messages between agents"): a
/// person any agent they reach; an agent itself and the agents it started in any mode, as
/// [`may_steer`] has it, and any other agent that works for the same person, as a queued message
/// from it. Agents that work for someone else, or for someone kiln can't name, aren't sent to.
pub async fn may_message(hub: &Hub, caller: &crate::authz::Caller, id: &str) -> Result<Reach, Refused> {
    let Some(me) = caller.agent.as_deref() else { return Ok(Reach::Own) };
    if me == id || descends(&hub.harness.list(), id, me) {
        return Ok(Reach::Own);
    }
    match (crate::mcp::acts_for(hub, me).await, crate::mcp::acts_for(hub, id).await) {
        (Some(mine), Some(theirs)) if mine == theirs => Ok(Reach::Peer),
        _ => Err(refuse(format!("an agent messages the agents it started and those that work for the same person: {} works for someone else", short(id)))),
    }
}

pub fn descends(all: &[Info], id: &str, ancestor: &str) -> bool {
    let mut at = id;
    for _ in 0..64 {
        let Some(p) = all.iter().find(|i| i.id == at).and_then(|i| i.parent.as_deref()) else { return false };
        if p == ancestor {
            return true;
        }
        at = p;
    }
    false
}

/// Every agent kiln still lists that agent `id` started, and theirs, nearest first.
pub fn descendants(all: &[Info], id: &str) -> Vec<Info> {
    let mut out: Vec<Info> = Vec::new();
    let mut frontier = vec![id.to_owned()];
    while let Some(p) = frontier.pop() {
        for c in all.iter().filter(|i| i.parent.as_deref() == Some(p.as_str())) {
            if out.iter().any(|o| o.id == c.id) || c.id == id {
                continue;
            }
            frontier.push(c.id.clone());
            out.push(c.clone());
        }
    }
    out
}

/// The agents still running that agent `id` started, and theirs, nearest first.
pub fn live_descendants(all: &[Info], id: &str) -> Vec<Info> {
    descendants(all, id).into_iter().filter(|c| c.status != Status::Exited).collect()
}

/// What an agent asks to start (`agents.start`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Ask {
    /// The harness (`claude`, `codex`, …).
    pub kind: String,
    /// Where: a machine's name (`local` for the server), or what it needs (`linux/amd64`,
    /// `+gpu`). The caller's own machine when it's an agent, else the server or the best runner.
    pub on: Option<String>,
    pub cwd: Option<String>,
    pub prompt: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub mode: Option<String>,
    pub name: Option<String>,
    pub count: Option<usize>,
    /// `folder` (the folder itself, the only one there is until workspaces are built).
    pub workspace: Option<String>,
}

/// Who starts agents: the agent that asks (none: a person), whom they act for, how the audit
/// trail names them, and the plan run they're for.
#[derive(Debug, Clone, Default)]
pub struct Starter {
    pub parent: Option<Info>,
    pub opener: Option<String>,
    pub actor: String,
    pub plan_run: Option<String>,
}

/// The server itself as a machine to pick (`local`), when it runs agents.
fn server_as_runner(hub: &Hub) -> Option<crate::reach::directory::RunnerInfo> {
    if !hub.cfg.terminal.enabled {
        return None;
    }
    let harnesses = hub.harness.kinds().get("local").map(|ks| ks.iter().filter(|k| k.available).map(|k| k.id.clone()).collect()).unwrap_or_default();
    Some(crate::reach::directory::RunnerInfo {
        name: "local".to_owned(),
        hostname: crate::os::hostname(),
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        connected: true,
        reach: true,
        harnesses,
        ..crate::reach::directory::RunnerInfo::default()
    })
}

/// The machines that could run harness `kind` for `want`, best first (the idlest, as reach picks),
/// and why each other one was passed over. The server counts as `local`.
fn candidates(hub: &Hub, want: &crate::reach::directory::Want) -> (Vec<String>, Vec<String>) {
    let outposts: Vec<String> = hub.agents.lock().values().filter(|a| a.outpost).map(|a| a.name.clone()).collect();
    let mut machines = hub.reach_directory();
    machines.retain(|r| !outposts.contains(&r.name));
    machines.extend(server_as_runner(hub));
    let mut ok: Vec<(f32, String)> = Vec::new();
    let mut passed: Vec<String> = Vec::new();
    for r in &machines {
        let local = r.name == "local";
        // The server is named `local` (or `server`) when asked for by name.
        let named = want.name.as_deref().is_some_and(|n| local && (n == "local" || n == "server" || n == r.hostname));
        let mut w = want.clone();
        if named {
            w.name = None;
        }
        let why_not = w.mismatch(r).or_else(|| (!r.connected).then(|| "offline".to_owned()));
        if let Some(why) = why_not {
            passed.push(format!("{}: {why}", r.name));
            continue;
        }
        // Its room as reach measures it (an overloaded machine only when none other suits); CI
        // slots aside, which agents don't take.
        let score = crate::reach::directory::room(r.strain(), 0, 0);
        ok.push((score, r.name.clone()));
    }
    ok.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    (ok.into_iter().map(|(_, n)| n).collect(), passed)
}

/// The target (`local`, `agent:<machine>`) agents of harness `kind` start on for `on`; none given,
/// `default` (the parent's machine), else the server, else the best runner that has the harness.
pub fn pick(hub: &Hub, on: Option<&str>, kind: &str, default: Option<&str>) -> Result<String, Refused> {
    let on = on.map(str::trim).filter(|o| !o.is_empty());
    let on = match (on, default) {
        (Some(o), _) => o.to_owned(),
        (None, Some(d)) => return Ok(d.to_owned()),
        (None, None) => String::new(),
    };
    if let Some(m) = on.strip_prefix("agent:") {
        return Ok(format!("agent:{m}"));
    }
    let mut want = crate::reach::directory::Want::parse(&on).map_err(|e| bad(format!("`on`: {e:#}")))?;
    // A machine named, and nothing else asked of it: that one (it says itself if it can't start
    // the harness), whatever its list of harnesses says yet.
    if let Some(name) = want.name.clone().filter(|_| want == crate::reach::directory::Want { name: want.name.clone(), ..Default::default() }) {
        if matches!(name.as_str(), "local" | "server") || (name == crate::os::hostname() && !hub.agents.lock().contains_key(&name)) {
            return Ok("local".to_owned());
        }
        return match hub.agents.lock().get(&name) {
            Some(a) if a.connected && !a.outpost && !a.ephemeral => Ok(format!("agent:{name}")),
            Some(_) => Err((StatusCode::CONFLICT, format!("{name} isn't connected (or doesn't run agents): pick another machine"))),
            None => Err((StatusCode::CONFLICT, format!("kiln has no machine named {name}"))),
        };
    }
    want.harness = Some(kind.to_owned());
    let (best, passed) = candidates(hub, &want);
    match best.into_iter().next() {
        Some(m) if m == "local" => Ok("local".to_owned()),
        Some(m) => Ok(format!("agent:{m}")),
        None => {
            let what = if on.is_empty() { format!("a machine with {kind}") } else { want.describe() };
            Err((StatusCode::CONFLICT, if passed.is_empty() { format!("no machine suits {what}: none is connected") } else { format!("no machine suits {what}: {}", passed.join("; ")) }))
        }
    }
}

/// Whether agents on `target` (`local`, `agent:<machine>`) can have workspaces of their own: its
/// kiln asks its machine about folders, and its machine has Slates and can mount a volume.
pub async fn workspaces_on(hub: &Hub, target: &str) -> Result<(), String> {
    let facts = match target.strip_prefix("agent:") {
        None => crate::workspace::slates::facts().await,
        Some(machine) => {
            let caps = hub.instances.lock().newest_with(machine, crate::proto::CAP_WORKSPACE).is_some();
            if !caps {
                return Err(format!("{machine}'s kiln is older than workspaces: upgrade it (System → Updates)"));
            }
            match hub.agents.lock().get(machine).and_then(|a| a.facts.as_ref().and_then(|f| f.workspaces.clone())) {
                Some(f) => f,
                None => return Err(format!("{machine} hasn't said yet whether it has Slates: try again in a minute")),
            }
        }
    };
    if facts.available { Ok(()) } else { Err(facts.why_not.unwrap_or_else(|| "Slates isn't installed there".to_owned())) }
}

/// Whether `target` names the machine `info` runs on.
fn same_machine(info: &Info, target: &str) -> bool {
    info.target == target || (target == "local" && info.target.is_empty())
}

/// What a parent's children may be, checked before any starts: its depth, how many it runs, its
/// mode and its reach. Returns the child's mode and reach.
pub async fn hold(hub: &Hub, parent: &Info, ask: &Ask, target: &str, count: usize) -> Result<(Option<String>, Option<String>), Refused> {
    let limits = &hub.cfg.agents;
    let d = depth(hub, &parent.id).await;
    if d >= limits.max_depth {
        return Err(refuse(format!(
            "{} is {} agents deep below the one a person started, the most there may be ([agents] max_depth = {}): it can't start more",
            parent.name, d, limits.max_depth
        )));
    }
    let all = hub.harness.list();
    let running = all.iter().filter(|i| i.parent.as_deref() == Some(parent.id.as_str()) && i.status != Status::Exited).count();
    let kiln_max = usize::try_from(limits.max_children).unwrap_or(usize::MAX);
    // Its own, when lower: the most subagents it runs at once (its Agent settings).
    let own = parent.max_subagents.and_then(|m| usize::try_from(m).ok()).filter(|m| *m < kiln_max);
    let max = own.unwrap_or(kiln_max);
    if running.saturating_add(count) > max {
        let s = if running == 1 { "" } else { "s" };
        return Err(refuse(match own {
            Some(_) => format!("{} runs {running} agent{s} it started, and its settings let it run at most {max} at once: end some first, or raise that in its Agent settings", parent.name),
            None => format!("{} runs {running} agent{s} it started, and one agent runs at most {max} at once ([agents] max_children): end some first", parent.name),
        }));
    }
    let mode = child_mode(&ask.kind, ask.mode.as_deref(), ceiling(parent), !parent.no_bypass).map_err(refuse)?;
    // Its commands run only on its own machine: so do its children's, there.
    let reach = parent.reach.clone();
    if reach.as_deref() == Some("off") && !same_machine(parent, target) {
        return Err(refuse(format!("{}'s commands run only on its own machine (its reach is off), so the agents it starts run there too", parent.name)));
    }
    Ok((mode, reach))
}

/// Start agents for `by`: checked against its parent when it's an agent, on the machine picked,
/// each recorded with whom it acts for and who started it.
pub async fn start(hub: &Hub, by: &Starter, ask: Ask) -> Result<Vec<Info>, Refused> {
    let kind = Kind::get(&ask.kind).ok_or_else(|| bad(format!("unknown harness {:?}: claude, codex, devin, gemini, opencode, goose, amp or antigravity", ask.kind)))?;
    if !kind.structured() {
        return Err(bad(format!("{} has no structured mode", kind.name)));
    }
    let volume = match ask.workspace.as_deref().map(str::trim) {
        None | Some("" | "folder") => false,
        Some("volume") => true,
        Some(w) => return Err(bad(format!("`workspace` is \"folder\" (its folder itself) or \"volume\" (a Slates volume of its own over the folder), not {w:?}"))),
    };
    let count = ask.count.unwrap_or(1).clamp(1, 16);
    let default_target = by.parent.as_ref().map(|p| if p.target.is_empty() { "local".to_owned() } else { p.target.clone() });
    let target = pick(hub, ask.on.as_deref(), kind.id, default_target.as_deref())?;
    if volume {
        workspaces_on(hub, &target).await.map_err(|why| (StatusCode::CONFLICT, format!("an agent can't have a workspace of its own there: {why}")))?;
    }
    if target == "local" && !hub.cfg.terminal.enabled {
        return Err(refuse("terminals (and so agents) are off on the server: pick a runner with `on`"));
    }
    let (mode, reach, allow_bypass) = match &by.parent {
        Some(p) => {
            let (mode, reach) = hold(hub, p, &ask, &target, count).await?;
            (mode, reach, !p.no_bypass)
        }
        None => (ask.mode.clone().filter(|m| !m.trim().is_empty()), None, true),
    };
    // Its folder: the one asked for, else its parent's where they share a machine, else home.
    let cwd = ask.cwd.clone().filter(|c| !c.trim().is_empty()).or_else(|| by.parent.as_ref().filter(|p| same_machine(p, &target)).map(|p| p.cwd.clone()).filter(|c| !c.is_empty()));
    let spec = Spec {
        kind: kind.id.to_owned(),
        cwd,
        model: ask.model.clone().filter(|m| !m.trim().is_empty()),
        mode,
        prompt: ask.prompt.clone().filter(|p| !p.trim().is_empty()),
        name: ask.name.clone().filter(|n| !n.trim().is_empty()),
        effort: ask.effort.clone().filter(|e| !e.trim().is_empty()),
        allow_bypass,
        reach,
        parent: by.parent.as_ref().map(|p| p.id.clone()),
        plan_run: by.plan_run.clone(),
        workspace: volume.then(|| crate::workspace::Workspace { kind: "volume".to_owned(), ..Default::default() }),
        ..Spec::default()
    };
    let infos = hub.harness.open(hub, &target, spec, count).await.map_err(|e| (StatusCode::CONFLICT, format!("{e:#}")))?;
    for i in &infos {
        if let Some(o) = &by.opener {
            let _ = hub.store.kv_set(crate::mcp::opener_key(&i.id), o.clone()).await;
        }
        if let Some(p) = &by.parent {
            let _ = hub.store.kv_set(parent_key(&i.id), p.id.clone()).await;
        }
        if ask.prompt.as_deref().is_some_and(|p| !p.trim().is_empty()) {
            super::wait::sent(&i.id, None);
        }
    }
    let parent = by.parent.as_ref().map_or_else(String::new, |p| format!(" for {} ({})", p.name, short(&p.id)));
    hub.store.audit(by.actor.clone(), "agent.open".into(), format!("{} {} on {target}{parent}", infos.len(), kind.name)).await;
    Ok(infos)
}

/// An agent's id as lists shorten it.
pub fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// What a parent's live children are, for ending it (`agents.stop` with `children`, and the
/// console's End).
pub fn children_brief(hub: &Hub, id: &str) -> Value {
    let all = hub.harness.list();
    json!(live_descendants(&all, id).iter().map(|i| json!({"id": i.id, "name": i.name, "kind": i.kind, "status": i.status, "parent": i.parent})).collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_harness_s_modes_stand_in_one_order() {
        assert!(Level::Plan < Level::Ask && Level::Ask < Level::AcceptEdits && Level::AcceptEdits < Level::Auto && Level::Auto < Level::Bypass);
        assert_eq!(level("claude", "plan"), Some(Level::Plan));
        assert_eq!(level("claude", "default"), Some(Level::Ask));
        assert_eq!(level("claude", ""), Some(Level::Ask), "Claude Code's own default asks");
        assert_eq!(level("claude", "dontAsk"), Some(Level::Ask));
        assert_eq!(level("claude", "acceptEdits"), Some(Level::AcceptEdits));
        assert_eq!(level("claude", "auto"), Some(Level::Auto));
        assert_eq!(level("claude", "bypassPermissions"), Some(Level::Bypass));
        assert_eq!(level("codex", "read-only"), Some(Level::Plan));
        assert_eq!(level("codex", "untrusted"), Some(Level::Ask));
        assert_eq!(level("codex", ""), Some(Level::Auto), "Codex's own default works in its sandbox unasked");
        assert_eq!(level("codex", "full-access"), Some(Level::Bypass));
        assert_eq!(level("antigravity", "plan"), Some(Level::Plan));
        assert_eq!(level("antigravity", "default"), Some(Level::Ask));
        assert_eq!(level("antigravity", ""), Some(Level::AcceptEdits));
        assert_eq!(level("antigravity", "bypass"), Some(Level::Bypass));
        assert_eq!(level("gemini", "kiln-ask"), Some(Level::Ask));
        assert_eq!(level("gemini", "kiln-bypass"), Some(Level::Bypass));
        assert_eq!(level("gemini", "yolo"), Some(Level::Bypass));
        assert_eq!(level("gemini", "autoEdit"), Some(Level::AcceptEdits));
        assert_eq!(level("opencode", "plan"), Some(Level::Plan));
        assert_eq!(level("opencode", "build"), Some(Level::Auto));
        assert_eq!(level("claude", "turbo"), None);
        assert_eq!(level("nonsense", "plan"), None);
    }

    #[test]
    fn a_child_gets_no_more_than_its_parent() {
        // Asked for bypass under a parent that asks: refused, saying what it may have.
        let e = child_mode("claude", Some("bypassPermissions"), Level::Ask, true).err().unwrap_or_default();
        assert!(e.contains("bypassPermissions is bypass, above its own ask") && e.contains("default"), "{e}");
        assert!(child_mode("codex", Some("full-access"), Level::Ask, true).is_err());
        assert!(child_mode("codex", Some("auto"), Level::AcceptEdits, true).is_err(), "Codex's auto is above accept edits");
        // Within it: as asked.
        assert_eq!(child_mode("claude", Some("plan"), Level::Ask, true), Ok(Some("plan".to_owned())));
        assert_eq!(child_mode("claude", Some("acceptEdits"), Level::Auto, true), Ok(Some("acceptEdits".to_owned())));
        // Bypass under bypass needs the parent's own allow_bypass.
        assert_eq!(child_mode("claude", Some("bypassPermissions"), Level::Bypass, true), Ok(Some("bypassPermissions".to_owned())));
        assert!(child_mode("claude", Some("bypassPermissions"), Level::Bypass, false).is_err());
        // Nothing asked: the harness's default when it's within, else the most that is.
        assert_eq!(child_mode("claude", None, Level::Ask, true), Ok(Some("default".to_owned())));
        assert_eq!(child_mode("claude", None, Level::Plan, true), Ok(Some("plan".to_owned())));
        assert_eq!(child_mode("codex", None, Level::Ask, true), Ok(Some("untrusted".to_owned())));
        assert_eq!(child_mode("codex", None, Level::Plan, true), Ok(Some("read-only".to_owned())));
        assert_eq!(child_mode("codex", None, Level::Bypass, true), Ok(Some("auto".to_owned())), "its default, not the most it could have");
        assert_eq!(child_mode("antigravity", None, Level::Ask, true), Ok(Some("default".to_owned())));
        // An ACP agent below ask has nothing kiln knows to start it in.
        assert!(child_mode("gemini", None, Level::Plan, true).is_err());
        assert_eq!(child_mode("gemini", None, Level::Ask, true), Ok(None));
        // Unknown modes and modes a harness doesn't have are refused, never let through.
        assert!(child_mode("claude", Some("turbo"), Level::Bypass, true).is_err());
        assert!(child_mode("codex", Some("acceptEdits"), Level::Bypass, true).is_err(), "Codex has no acceptEdits");
    }

    #[test]
    fn devin_children_keep_native_modes_within_their_parents_permissions() {
        assert_eq!(level("devin", ""), Some(Level::AcceptEdits));
        for (mode, l) in [("plan", Level::Plan), ("ask", Level::Ask), ("accept-edits", Level::AcceptEdits), ("smart", Level::Auto), ("bypass", Level::Bypass)] {
            assert_eq!(level("devin", mode), Some(l));
            assert_eq!(child_mode("devin", Some(mode), l, true), Ok(Some(mode.to_owned())));
        }
        assert_eq!(child_mode("devin", None, Level::Plan, false), Ok(Some("plan".into())));
        assert_eq!(child_mode("devin", None, Level::Ask, false), Ok(Some("ask".into())));
        assert_eq!(child_mode("devin", None, Level::AcceptEdits, false), Ok(Some("accept-edits".into())));
        assert_eq!(child_mode("devin", None, Level::Bypass, true), Ok(Some("accept-edits".into())));
        assert!(child_mode("devin", Some("accept-edits"), Level::Ask, true).is_err());
        assert!(child_mode("devin", Some("smart"), Level::AcceptEdits, true).is_err());
        assert!(child_mode("devin", Some("bypass"), Level::Auto, true).is_err());
        assert!(child_mode("devin", Some("bypass"), Level::Bypass, false).is_err());
        for unknown in ["kiln-ask", "kiln-bypass", "auto", "code", "readonly"] {
            assert_eq!(level("devin", unknown), None);
            assert!(child_mode("devin", Some(unknown), Level::Bypass, true).is_err());
        }
    }

    fn info(id: &str, parent: Option<&str>, status: Status) -> Info {
        Info { id: id.into(), parent: parent.map(str::to_owned), status, ..Info::default() }
    }

    #[test]
    fn descendants_are_followed_down_the_chain() {
        let all = vec![info("a", None, Status::Idle), info("b", Some("a"), Status::Running), info("c", Some("b"), Status::Idle), info("d", Some("a"), Status::Exited), info("e", Some("d"), Status::Running), info("x", None, Status::Idle)];
        assert!(descends(&all, "c", "a") && descends(&all, "b", "a") && descends(&all, "e", "a"));
        assert!(!descends(&all, "a", "a") && !descends(&all, "x", "a") && !descends(&all, "a", "c"));
        let live: Vec<String> = live_descendants(&all, "a").into_iter().map(|i| i.id).collect();
        assert!(live.contains(&"b".to_owned()) && live.contains(&"c".to_owned()) && live.contains(&"e".to_owned()));
        assert!(!live.contains(&"d".to_owned()), "an ended child isn't live (its own children are)");
        assert!(live_descendants(&all, "x").is_empty());
        assert_eq!(descendants(&all, "a").len(), 4, "ended ones too, for ending them with their parent");
    }
}

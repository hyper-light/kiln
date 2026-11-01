//! What a person holds an agent's subagents to at each of their tool calls, whatever the harness
//! (docs/agents.md, "Pausing" and "What each may do"): a pause, and what each may do ([`Perms`]).
//! The harness says which subagent a call is in and what kind of call it is (Claude Code's hook,
//! `claude::HOLD_HOOK`; Codex's, `codex::hook_args`); the answers are kiln's, the same for each.

use super::core::Core;
use super::model::{Grant, Perms, ToolKind};

/// Subagent `task`, or one it's under, is paused by a person (`Task::paused_at`).
pub fn paused(core: &Core, task: &str) -> bool {
    let mut at = core.state.tasks.iter().find(|t| t.id == task);
    // A chain no deeper than the tasks there are: a loop of parents ends.
    for _ in 0..=core.state.tasks.len() {
        let Some(t) = at else { return false };
        if t.paused_at.is_some() {
            return true;
        }
        at = t.parent_task.as_deref().and_then(|p| core.state.tasks.iter().find(|u| u.id == p));
    }
    false
}

/// What a call in subagent `task` (none: the agent's own) may do: its session's perms, narrowed
/// by each subagent's from the outermost to `task`. None: no limits.
pub fn perms_over(core: &Core, task: Option<&str>) -> Option<Perms> {
    let mut line: Vec<&Perms> = Vec::new();
    let mut at = task.and_then(|t| core.state.tasks.iter().find(|u| u.id == t));
    for _ in 0..=core.state.tasks.len() {
        let Some(t) = at else { break };
        if let Some(p) = &t.perms {
            line.push(p);
        }
        at = t.parent_task.as_deref().and_then(|p| core.state.tasks.iter().find(|u| u.id == p));
    }
    line.into_iter().rev().fold(core.state.perms.clone(), |over, p| Perms::within(Some(p), over.as_ref()))
}

/// What a person's perms say of a tool call of `kind` in subagent `task` (none: the agent's own),
/// touching `paths` (none named: its folder): refused ("deny") or asked ("ask"), and why, in words
/// the agent reads; none where they let it go as it would.
pub fn verdict(core: &Core, task: Option<&str>, kind: ToolKind, paths: &[&str]) -> Option<(&'static str, String)> {
    let perms = perms_over(core, task)?;
    let cwd = core.state.cwd.as_str();
    // Past its limits, it finishes with what it has: its own task's time and tokens, or the session's.
    let t = task.and_then(|t| core.state.tasks.iter().find(|x| x.id == t));
    let since = t.map(|t| t.started).or_else(|| core.events().next().map(|e| e.at)).unwrap_or_else(Core::now);
    let used = t.map_or(core.state.usage.input_tokens + core.state.usage.output_tokens, |t| t.tokens);
    let finish = "Make no more tool calls: finish now with what you have.";
    if let Some(m) = perms.minutes
        && Core::now() - since > i64::from(m) * 60_000
    {
        return Some(("deny", format!("Your {m} minutes are up (a person's limit on you). {finish}")));
    }
    if let Some(n) = perms.tokens
        && used >= n
    {
        return Some(("deny", format!("You've used your {n} tokens (a person's limit on you). {finish}")));
    }
    let (grant, what) = match kind {
        ToolKind::Read | ToolKind::Search => (perms.read, "read files"),
        ToolKind::Edit => (perms.edit, "change files"),
        ToolKind::Shell | ToolKind::Mcp => (perms.run, "run commands"),
        ToolKind::Web => (perms.web, "use the web"),
        _ => return None,
    };
    match grant {
        Grant::Off => return Some(("deny", format!("You may not {what} (a person turned it off for you). Do without it, or say what you'd need."))),
        Grant::Ask => return Some(("ask", format!("A person sees each time you {what}."))),
        Grant::Allow => {}
    }
    // Its folder: a file call stays in it; a command can reach anywhere, so a person sees each.
    let folder = perms.folder.as_deref()?;
    let root = lexical(folder, cwd);
    match kind {
        ToolKind::Read | ToolKind::Search | ToolKind::Edit => {
            let paths = if paths.is_empty() { vec![cwd] } else { paths.to_vec() };
            paths
                .into_iter()
                .find(|p| {
                    let at = lexical(p, cwd);
                    at != root && !at.starts_with(&format!("{root}/"))
                })
                .map(|p| ("deny", format!("{p} is outside your folder, {folder} (a person's limit on you). Stay in it.")))
        }
        ToolKind::Shell => Some(("ask", format!("Your folder is {folder}, and a command can reach outside it: a person sees each."))),
        _ => None,
    }
}

/// Path `p` from folder `cwd`, its `.` and `..` worked out as written (no links followed), with
/// Windows' separators and drives taken as `/` and a first part.
pub fn lexical(p: &str, cwd: &str) -> String {
    let (p, cwd) = (p.replace('\\', "/"), cwd.replace('\\', "/"));
    let absolute = p.starts_with('/') || p.as_bytes().get(1) == Some(&b':');
    let joined = if absolute { p } else { format!("{cwd}/{p}") };
    let mut parts: Vec<&str> = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    format!("/{}", parts.join("/"))
}

/// The subagent of the session `task` names (its id, or its spawning call's), as its index.
fn subagent(core: &Core, task: &str) -> Option<usize> {
    core.state.tasks.iter().position(|t| t.subagent() && (t.id == task || t.tool_id.as_deref() == Some(task)))
}

/// A person pauses subagent `task` (`on`), or resumes it. False: the session has no such subagent.
pub fn pause(core: &mut Core, task: &str, on: bool) -> bool {
    let Some(i) = subagent(core, task) else { return false };
    if let Some(t) = core.state.tasks.get_mut(i) {
        t.paused_at = on.then(Core::now);
    }
    core.touch_state();
    true
}

/// What a person lets subagent `task` do (none: no limits of its own), never more than its session
/// and the subagent it's under let it ([`perms_over`]). False: the session has no such subagent.
pub fn set_perms(core: &mut Core, task: &str, perms: Option<Perms>) -> bool {
    let Some(i) = subagent(core, task) else { return false };
    let up = core.state.tasks.get(i).and_then(|t| t.parent_task.clone());
    let over = perms_over(core, up.as_deref());
    if let Some(t) = core.state.tasks.get_mut(i) {
        t.perms = perms.map(|p| Perms::within(Some(&p), over.as_ref()).unwrap_or(p));
    }
    core.touch_state();
    true
}

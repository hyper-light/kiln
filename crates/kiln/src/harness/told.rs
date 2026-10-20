//! A parent hears from its children: when an agent another agent started settles (its turn over,
//! a person needed, stopped, ended, paused at a usage limit), kiln tells its parent, as Claude
//! Code tells an agent of its own background subagents: a message after the parent's turn, which
//! starts one if the parent is idle. So a parent needn't wait for its children: it can start
//! them, end its turn, and take up what each concluded when it's told. `agents_wait` is for the
//! parent that would rather wait; what it saw that way (or through `agents_result`) isn't told
//! again. Children that settle together are told of in one message.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use parking_lot::Mutex;

use super::session::Info;
use super::wait::{settled, stand};

/// How long a child has stood settled before its parent is told: a wait under way for it returns
/// first, and one only between two steps isn't reported.
const SETTLE_MS: i64 = 3_000;
/// The most of a child's last words a message carries (`agents_result` has them whole).
const SAID_MAX: usize = 400;

/// What was last seen of a child.
#[derive(Debug, Clone, Default)]
struct Seen {
    /// Its stand and the end of its last turn.
    key: String,
    /// Since when it has stood so.
    since: i64,
    /// Seen at work since its parent was last told: only a settle after work is told of.
    worked: bool,
    /// The settle its parent was told of, or saw itself.
    told: String,
}

static SEEN: LazyLock<Mutex<HashMap<String, Seen>>> = LazyLock::new(Default::default);

fn key_of(info: &Info) -> String {
    format!("{}:{}", stand(info), info.last_turn.as_ref().map_or(0, |t| t.at))
}

/// Its parent saw `child` as it stands now (`agents_wait`, `agents_result`): not told of it again.
pub fn saw(child: &Info) {
    let key = key_of(child);
    SEEN.lock().entry(child.id.clone()).or_default().told = key;
}

/// Each child that settled since it was last looked at, `SETTLE_MS` on: what its parent is told,
/// by parent (one message for the children that settled together). Children first seen settled
/// (the server just started) aren't told of, nor ones a goal stack drives (`driven`: they settle
/// between goals; their parent is told once no stack drives them).
fn due(all: &[Info], now: i64, driven: &[String]) -> Vec<(String, String)> {
    let mut by_parent: HashMap<String, Vec<String>> = HashMap::new();
    let mut seen = SEEN.lock();
    for c in all {
        let Some(parent) = c.parent.as_deref() else { continue };
        let (st, key) = (stand(c), key_of(c));
        let e = seen.entry(c.id.clone()).or_insert_with(|| Seen { key: key.clone(), since: now, worked: false, told: key.clone() });
        if e.key != key {
            e.key.clone_from(&key);
            e.since = now;
        }
        if st == "working" {
            e.worked = true;
            continue;
        }
        if driven.contains(&c.id) {
            continue;
        }
        if !settled(st) || !e.worked || e.told == key || now - e.since < SETTLE_MS {
            continue;
        }
        e.told = key;
        e.worked = false;
        by_parent.entry(parent.to_owned()).or_default().push(line(c));
    }
    let live: HashSet<&str> = all.iter().map(|i| i.id.as_str()).collect();
    seen.retain(|id, _| live.contains(id.as_str()));
    let mut out: Vec<(String, String)> = by_parent
        .into_iter()
        .map(|(parent, lines)| {
            let text = if lines.len() == 1 { lines.join("") } else { format!("Agents you started:\n{}", lines.iter().map(|l| format!("- {l}")).collect::<Vec<_>>().join("\n")) };
            (parent, text)
        })
        .collect();
    out.sort();
    out
}

/// One child as its parent is told of it.
fn line(c: &Info) -> String {
    let short: String = c.id.chars().take(8).collect();
    let who = if c.name.trim().is_empty() { format!("Agent {short}") } else { format!("Agent \u{201c}{}\u{201d} ({short})", c.name.trim()) };
    let said = super::model::clip_middle(c.summary.trim(), SAID_MAX).0;
    match stand(c) {
        "idle" => {
            let how = match c.last_turn.as_ref().map(|t| t.how.as_str()) {
                Some("cut") => " part-way",
                Some("refused") => ", refusing",
                Some("limit") => " at its turn limit",
                _ => "",
            };
            let said = if said.is_empty() { String::new() } else { format!(": {said}") };
            format!("{who}, which you started, ended its turn{how}{said}. agents_result {{\"agent\": \"{}\"}} has its whole reply and what it changed.", c.id)
        }
        "needs_you" => {
            let asks: Vec<String> = c.approvals.iter().map(|a| a.title.clone()).filter(|t| !t.trim().is_empty()).collect();
            let asks = if asks.is_empty() { String::new() } else { format!(" ({})", asks.join("; ")) };
            format!("{who}, which you started, waits on a person{asks}: tell the person what it asks and where (its page in kiln). It goes on once they answer.")
        }
        "stopped" => {
            let why = c.exit.as_ref().and_then(|e| e.error.clone()).map(|e| format!(": {}", super::model::clip_middle(e.trim(), SAID_MAX).0)).unwrap_or_default();
            format!("{who}, which you started, stopped{why}.")
        }
        "paused" => format!("{who}, which you started, is paused at a usage limit: it goes on when the limit resets."),
        _ => format!("{who}, which you started, ended."),
    }
}

/// Tells parents of their children that settled (`due`): queued after each parent's turn.
/// `driven`: the agents a goal stack drives now.
pub async fn tick(hub: &crate::hub::Hub, all: &[Info], now: i64, driven: &[String]) {
    for (parent, text) in due(all, now, driven) {
        crate::workspace::follow::deliver(hub, &parent, text).await;
    }
}

/// Whether `agent` has children at work: an agent that ended its turn to let them work isn't
/// prodded to go on (it's told when each settles).
pub fn children_working(all: &[Info], agent: &str) -> bool {
    all.iter().any(|c| c.parent.as_deref() == Some(agent) && stand(c) == "working")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::model::{Status, TurnEnd};

    fn asks(title: &str) -> anyhow::Result<crate::harness::model::Approval> {
        Ok(serde_json::from_value(serde_json::json!({"id": "r1", "tool_id": null, "tool_name": "Bash", "tool": "shell", "title": title, "input": {}, "cwd": "/w", "at": 1}))?)
    }

    fn child(id: &str, status: Status, ended_at: i64, summary: &str) -> Info {
        Info {
            id: id.into(),
            name: format!("Child {id}"),
            parent: Some("p1".into()),
            status,
            summary: summary.into(),
            last_turn: (ended_at > 0).then(|| TurnEnd { at: ended_at, how: "done".into(), ..TurnEnd::default() }),
            ..Info::default()
        }
    }

    /// A child that worked and settled is told of once, after it has stood settled a while; one
    /// first seen settled (a restart) isn't; children that settle together go in one message; what
    /// the parent saw itself isn't told again.
    #[test]
    fn a_parent_is_told_when_its_children_settle_and_needn_t_wait() -> anyhow::Result<()> {
        SEEN.lock().clear();
        let t0 = 1_000_000;
        // First seen: one already done (a restart), one at work.
        let done_before = child("c0", Status::Idle, 10, "old news");
        assert!(due(&[done_before.clone(), child("c1", Status::Running, 0, "")], t0, &[]).is_empty());
        assert!(children_working(&[child("c1", Status::Running, 0, "")], "p1"));
        // c1 and c2 at work, then both done.
        assert!(due(&[done_before.clone(), child("c1", Status::Running, 0, ""), child("c2", Status::Running, 0, "")], t0 + 100, &[]).is_empty());
        let settled = [done_before.clone(), child("c1", Status::Idle, t0 + 200, "Fixed the parser; tests pass."), child("c2", Status::Idle, t0 + 300, "Docs updated.")];
        assert!(due(&settled, t0 + 400, &[]).is_empty(), "not before it has stood settled a while");
        let told = due(&settled, t0 + 400 + SETTLE_MS, &[]);
        assert_eq!(told.len(), 1, "one message for the parent: {told:?}");
        let (parent, text) = told.first().cloned().unwrap_or_default();
        assert_eq!(parent, "p1");
        assert!(text.contains("Agents you started") && text.contains("Fixed the parser; tests pass.") && text.contains("Docs updated.") && text.contains("agents_result"), "{text}");
        assert!(!text.contains("old news"), "what was done before the server saw it isn't told");
        assert!(due(&settled, t0 + 10_000, &[]).is_empty(), "told once");
        assert!(!children_working(&settled, "p1"));
        // c1 at work again, then waiting on a person; the parent saw that itself (agents_wait).
        let _ = due(&[child("c1", Status::Running, t0 + 200, "")], t0 + 11_000, &[]);
        let mut asking = child("c1", Status::Waiting, t0 + 200, "");
        asking.approvals.push(asks("Run cargo publish")?);
        saw(&asking);
        assert!(due(std::slice::from_ref(&asking), t0 + 20_000, &[]).is_empty());
        assert!(due(std::slice::from_ref(&asking), t0 + 20_000 + SETTLE_MS, &[]).is_empty(), "what the parent saw isn't told again");
        // Children of no agent (a person started them) are nobody's to tell.
        let mut orphan = child("c3", Status::Running, 0, "");
        orphan.parent = None;
        let _ = due(std::slice::from_ref(&orphan), t0, &[]);
        orphan.status = Status::Idle;
        assert!(due(&[orphan], t0 + 30_000, &[]).is_empty());
        // A child its goal stack drives settles between goals unannounced; once no stack drives
        // it, its parent is told.
        let driven = vec!["c4".to_owned()];
        let _ = due(&[child("c4", Status::Running, 0, "")], t0 + 40_000, &driven);
        let between = child("c4", Status::Idle, t0 + 41_000, "Goal 1 met.");
        assert!(due(std::slice::from_ref(&between), t0 + 50_000, &driven).is_empty(), "between goals");
        let told = due(std::slice::from_ref(&between), t0 + 60_000, &[]);
        assert!(told.first().is_some_and(|(p, t)| p == "p1" && t.contains("Goal 1 met.")), "once left to itself: {told:?}");
        Ok(())
    }

    #[test]
    fn what_a_parent_is_told_says_what_its_child_needs() -> anyhow::Result<()> {
        let mut asking = child("c9", Status::Waiting, 5, "");
        asking.approvals.push(asks("Run cargo publish")?);
        let text = line(&asking);
        assert!(text.contains("waits on a person (Run cargo publish)") && text.contains("tell the person"), "{text}");
        let mut crashed = child("c8", Status::Exited, 5, "");
        crashed.exit = Some(crate::harness::model::ExitInfo { code: Some(1), error: Some("claude exited with 1".into()) });
        assert!(line(&crashed).contains("stopped: claude exited with 1"), "{}", line(&crashed));
        Ok(())
    }
}

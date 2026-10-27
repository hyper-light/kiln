//! When to say something about an agent, and what. The server's notifier hears every change of
//! every agent (`harness` events) and asks [`Watch::heard`]; a note it calls for is sent once it
//! has settled ([`Watch::due`]), so a state that comes and goes says nothing, and nothing is said
//! twice:
//!
//! - **Needs you**: an approval, a question or a plan waits on you that wasn't said before.
//! - **Finished**: a turn ended that the server saw at work, not because a person stopped it. It
//!   settles longest: kiln may keep the agent going (`goals::keep`), and then nothing is said
//!   until it ends for real.
//! - **Stopped**: it crashed or couldn't start.
//! - **Paused**: it reached a usage limit.
//!
//! What was said is kept (`push_said`), so a restart, or a machine coming back, says nothing again.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use base64::Engine as _;
use sha2::Digest as _;

use crate::harness::model::{Status, ToolKind};
use crate::harness::session::Info;

/// Where an agent stands, as Mission Control says it (ui/src/views/harness/mission.tsx, `standOf`;
/// "finished" and "ready" are both [`Stand::Idle`] here: what a person has seen is the browser's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stand {
    Needs,
    Stopped,
    Paused,
    Working,
    Idle,
    Offline,
    Ended,
}

pub fn stand_of(i: &Info) -> Stand {
    if !i.approvals.is_empty() || i.status == Status::Waiting {
        return Stand::Needs;
    }
    if i.status == Status::Exited && i.exit.as_ref().is_some_and(|e| e.error.is_some()) {
        return Stand::Stopped;
    }
    // Waiting for a limit to reset (a turn under way finishes first).
    if i.pause.is_some() && !matches!(i.status, Status::Running | Status::Exited | Status::Disconnected) {
        return Stand::Paused;
    }
    match i.status {
        Status::Running | Status::Starting => Stand::Working,
        Status::Idle => Stand::Idle,
        Status::Disconnected => Stand::Offline,
        Status::Waiting => Stand::Needs,
        Status::Exited => Stand::Ended,
    }
}

/// What a note says of an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    NeedsYou,
    Finished,
    Stopped,
    Paused,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::NeedsYou, Kind::Finished, Kind::Stopped, Kind::Paused];

    /// Its event, as rules, webhooks and devices name it.
    pub fn event(self) -> &'static str {
        match self {
            Kind::NeedsYou => "agent.needs_you",
            Kind::Finished => "agent.finished",
            Kind::Stopped => "agent.stopped",
            Kind::Paused => "agent.paused",
        }
    }

    /// What the events socket calls it ([`Kind::event`]).
    pub fn notice(self) -> crate::wire::events::AgentNoticeKind {
        use crate::wire::events::AgentNoticeKind;
        match self {
            Kind::NeedsYou => AgentNoticeKind::NeedsYou,
            Kind::Finished => AgentNoticeKind::Finished,
            Kind::Stopped => AgentNoticeKind::Stopped,
            Kind::Paused => AgentNoticeKind::Paused,
        }
    }

    pub fn of_event(e: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.event() == e)
    }

    /// How long it settles before it's sent.
    fn settle(self) -> Duration {
        match self {
            Kind::NeedsYou => Duration::from_millis(400),
            Kind::Finished => Duration::from_secs(8),
            Kind::Stopped | Kind::Paused => Duration::from_secs(2),
        }
    }

    /// The stand it's about.
    fn stand(self) -> Stand {
        match self {
            Kind::NeedsYou => Stand::Needs,
            Kind::Finished => Stand::Idle,
            Kind::Stopped => Stand::Stopped,
            Kind::Paused => Stand::Paused,
        }
    }
}

/// A note waiting to settle: check it with [`Watch::due`] after `after`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Due {
    pub agent: String,
    pub seq: u64,
    pub after: Duration,
}

/// A note to send: the agent as it is now, what, and the key that says it once.
#[derive(Debug, Clone)]
pub struct Note {
    pub info: Info,
    pub kind: Kind,
    pub key: String,
}

#[derive(Debug)]
struct Seen {
    info: Info,
    stand: Stand,
    /// The newest turn end dealt with (`TurnEnd::at`), and whether it was seen at work since.
    end: i64,
    busy: bool,
    /// The note waiting to settle: what, its key, its number.
    pending: Option<(Kind, String, u64)>,
}

#[derive(Debug, Default)]
pub struct Watch {
    seen: HashMap<String, Seen>,
    said: HashMap<(String, Kind), String>,
    seq: u64,
}

/// The approvals waiting, as a key: each one's id (a request is known by its id alone: asked again
/// after a restart, with another time, it's still the one said).
fn needs_key(i: &Info) -> String {
    if i.approvals.is_empty() {
        return format!("waiting@{}", i.turn_started.unwrap_or(0));
    }
    let mut keys: Vec<String> = i.approvals.iter().map(|a| a.id.clone()).collect();
    keys.sort();
    keys.join("\n")
}

/// An approval's part of a needs key, as said before too (`id@at`, before ids alone).
fn request_of(k: &str) -> &str {
    if k.starts_with("waiting@") { k } else { k.split('@').next().unwrap_or(k) }
}

/// Whether `key` holds an approval `said` didn't.
fn new_needs(key: &str, said: Option<&String>) -> bool {
    let Some(said) = said else { return true };
    let old: HashSet<&str> = said.split('\n').map(request_of).collect();
    key.split('\n').any(|k| !old.contains(request_of(k)))
}

impl Watch {
    /// What was said of an agent before (from the store).
    pub fn remember(&mut self, agent: &str, kind: &str, key: &str) {
        if let Some(k) = Kind::of_event(kind) {
            self.said.insert((agent.to_owned(), k), key.to_owned());
        }
    }

    /// Every agent as last heard.
    pub fn infos(&self) -> impl Iterator<Item = &Info> {
        self.seen.values().map(|s| &s.info)
    }

    /// An agent changed: a note to settle, when one is called for.
    pub fn heard(&mut self, info: Info) -> Option<Due> {
        let id = info.id.clone();
        let stand = stand_of(&info);
        let end_now = info.last_turn.as_ref().map_or(0, |t| t.at);
        let at_work = matches!(info.status, Status::Running | Status::Waiting | Status::Starting);
        // First heard (a new agent, or the server just started): what's there is where it stands,
        // not news, but for what was never said.
        let (first, before, mut end, mut busy, pending) = match self.seen.get(&id) {
            Some(s) => (false, s.stand, s.end, s.busy, s.pending.clone()),
            None => (true, stand, end_now, false, None),
        };
        busy |= at_work;
        let said = |k: Kind| self.said.get(&(id.clone(), k));
        let call: Option<(Kind, String)> = match stand {
            Stand::Needs => {
                let key = needs_key(&info);
                new_needs(&key, said(Kind::NeedsYou)).then_some((Kind::NeedsYou, key))
            }
            Stand::Stopped => {
                let key = info.exit.as_ref().and_then(|e| e.error.clone()).unwrap_or_default();
                // Seen alive just before: a crash. Otherwise (first heard, or back from offline)
                // only one not said yet.
                let from_alive = !first && matches!(before, Stand::Working | Stand::Needs | Stand::Paused | Stand::Idle);
                (from_alive || said(Kind::Stopped) != Some(&key)).then_some((Kind::Stopped, key))
            }
            Stand::Paused => {
                let key = info.pause.as_ref().map(|p| p.since.to_string()).unwrap_or_default();
                (said(Kind::Paused) != Some(&key)).then_some((Kind::Paused, key))
            }
            Stand::Idle => match info.last_turn.as_ref() {
                Some(t) if t.at > end && t.how == "stopped" => {
                    // You stopped it: nothing to tell you.
                    end = t.at;
                    busy = false;
                    None
                }
                Some(t) if t.at > end && busy => Some((Kind::Finished, t.at.to_string())),
                _ => None,
            },
            Stand::Working | Stand::Offline | Stand::Ended => None,
        };
        let mut due = None;
        let pending = match (call, pending) {
            // Already settling: it goes when it's due.
            (Some((kind, key)), Some(p)) if p.0 == kind && p.1 == key => Some(p),
            (Some((kind, key)), _) => {
                self.seq += 1;
                due = Some(Due { agent: id.clone(), seq: self.seq, after: kind.settle() });
                Some((kind, key, self.seq))
            }
            // Where it stood for it no longer: it's not said.
            (None, Some(p)) if p.0.stand() != stand => None,
            (None, p) => p,
        };
        self.seen.insert(id, Seen { info, stand, end, busy, pending });
        due
    }

    /// A note settled: sent now if the agent still stands where it did, said again never.
    pub fn due(&mut self, agent: &str, seq: u64) -> Option<Note> {
        let s = self.seen.get_mut(agent)?;
        let (kind, key, g) = s.pending.clone()?;
        if g != seq {
            return None;
        }
        s.pending = None;
        let now = match (kind, stand_of(&s.info)) {
            (Kind::NeedsYou, Stand::Needs) => {
                let k = needs_key(&s.info);
                new_needs(&k, self.said.get(&(agent.to_owned(), kind))).then_some(k)
            }
            (Kind::Finished, Stand::Idle) => match s.info.last_turn.as_ref().map(|t| t.at) {
                Some(at) if at.to_string() == key => {
                    s.end = at;
                    s.busy = false;
                    Some(key.clone())
                }
                _ => None,
            },
            (Kind::Stopped, Stand::Stopped) => Some(s.info.exit.as_ref().and_then(|e| e.error.clone()).unwrap_or_default()),
            (Kind::Paused, Stand::Paused) => s.info.pause.as_ref().filter(|p| p.since.to_string() == key).map(|_| key.clone()),
            _ => None,
        }?;
        self.said.insert((agent.to_owned(), kind), now.clone());
        Some(Note { info: s.info.clone(), kind, key: now })
    }

    /// An agent is gone for good.
    pub fn closed(&mut self, agent: &str) {
        self.seen.remove(agent);
        self.said.retain(|(a, _), _| a != agent);
    }
}

// ---------------------------------------------------------------------------------- the words

/// What Mission Control calls an agent (`agentLabel`): the name it was given; else its
/// conversation's title; else the first line of what it was asked; else its harness.
pub fn label(i: &Info) -> String {
    let name = i.name.trim();
    if !name.is_empty() && !name.eq_ignore_ascii_case(i.kind_name.trim()) {
        return name.to_owned();
    }
    let title = i.title.trim();
    if !title.is_empty() {
        return title.to_owned();
    }
    let first = i.goal.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
    if !first.is_empty() {
        return clip(first, 80);
    }
    if name.is_empty() { i.kind_name.clone() } else { name.to_owned() }
}

/// At most `n` characters, an ellipsis when cut.
pub fn clip(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().nth(n).is_some() {
        out.pop();
        out.push('…');
    }
    out
}

/// Markdown as plain words on one line (Mission Control's `plain`): code blocks go; code,
/// emphasis and links keep their words; heading and list marks go.
pub fn plain(md: &str) -> String {
    let mut text = String::new();
    let mut fence = false;
    for line in md.lines() {
        let t = line.trim_start();
        if t.starts_with("```") {
            fence = !fence;
            text.push(' ');
            continue;
        }
        if fence {
            continue;
        }
        let t = t.trim_start_matches('#').trim_start();
        let t = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")).unwrap_or(t);
        let t = match t.split_once(['.', ')']) {
            Some((n, rest)) if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) && rest.starts_with(' ') => rest,
            _ => t,
        };
        text.push_str(t);
        text.push(' ');
    }
    let text = text.replace("**", "").replace('`', "");
    // [words](url) → words
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(open) = rest.find('[') {
        let (before, after) = rest.split_at(open);
        out.push_str(before);
        let link = after.get(1..).and_then(|a| a.split_once("](")).and_then(|(words, tail)| tail.split_once(')').map(|(_, tail)| (words, tail)));
        match link {
            Some((words, tail)) if !words.contains('[') => {
                out.push_str(words);
                rest = tail;
            }
            _ => {
                out.push('[');
                rest = after.get(1..).unwrap_or_default();
            }
        }
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A pause, in words (Mission Control's `pauseLine`, without the clock: the server doesn't know
/// whose time zone).
fn pause_line(p: &crate::harness::model::Pause) -> String {
    let limit = p.label.as_deref().map_or_else(|| "a usage limit".to_owned(), |l| format!("the {l} limit"));
    let what = if p.reason == "threshold" {
        format!("stopped at its stop point for {limit}")
    } else if p.by == "account" {
        format!("another agent on this account reached {limit}")
    } else {
        format!("reached {limit}")
    };
    let when = if p.auto { "resumes by itself when it resets" } else { "resumes when you resume it" };
    format!("{what} · {when}")
}

/// The one line Mission Control shows for the agent, as the note's text.
pub fn line(i: &Info, kind: Kind) -> String {
    let text = match kind {
        Kind::NeedsYou => match i.approvals.first() {
            None => "Waiting for you".to_owned(),
            Some(a) => {
                let more = if i.approvals.len() > 1 { format!(" (+{} more)", i.approvals.len() - 1) } else { String::new() };
                match a.tool {
                    ToolKind::Question => format!("Asks: {}{more}", a.title),
                    ToolKind::Plan => format!("Proposes a plan{more}"),
                    other => {
                        let tool = if a.tool_name.trim().is_empty() { serde_json::to_value(other).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default() } else { a.tool_name.clone() };
                        format!("{tool}: {}{more}", a.title)
                    }
                }
            }
        },
        Kind::Finished => {
            let said = plain(&i.summary);
            let what = [said.as_str(), i.goal.trim(), i.title.trim()].into_iter().find(|s| !s.is_empty()).unwrap_or("Done");
            format!("Finished: {what}")
        }
        Kind::Stopped => format!("Stopped: {}", i.exit.as_ref().and_then(|e| e.error.as_deref()).filter(|e| !e.trim().is_empty()).unwrap_or("it stopped unexpectedly")),
        Kind::Paused => match &i.pause {
            Some(p) => format!("Paused: {}", pause_line(p)),
            None => "Paused".to_owned(),
        },
    };
    clip(&text.split_whitespace().collect::<Vec<_>>().join(" "), 240)
}

/// The push services' `Topic` for an agent (32 URL-safe base64 characters): a newer note about it
/// replaces one still waiting to be delivered.
pub fn topic(agent: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(agent.as_bytes())).chars().take(32).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::model::{ExitInfo, Pause, TurnEnd};
    use crate::harness::session::ApprovalBrief;

    fn agent(status: Status) -> Info {
        Info { id: "ag1".into(), kind: "claude".into(), kind_name: "Claude Code".into(), name: "Mantle".into(), status, ..Info::default() }
    }

    fn ask(id: &str, at: i64, tool: ToolKind, title: &str) -> ApprovalBrief {
        ApprovalBrief { id: id.into(), tool, tool_name: if tool == ToolKind::Shell { "Bash".into() } else { String::new() }, title: title.into(), at }
    }

    fn ended(at: i64, how: &str) -> Option<TurnEnd> {
        Some(TurnEnd { at, how: how.into(), why: None, tools: 0, after_error: false, asks: false, kept: 0 })
    }

    /// Runs `heard` and, for what it calls for, `due`: the notes that would go.
    fn say(w: &mut Watch, info: Info) -> Vec<(Kind, String)> {
        let Some(d) = w.heard(info) else { return Vec::new() };
        w.due(&d.agent, d.seq).map(|n| vec![(n.kind, line(&n.info, n.kind))]).unwrap_or_default()
    }

    #[test]
    fn stands_as_mission_control_says_them() {
        assert_eq!(stand_of(&agent(Status::Running)), Stand::Working);
        assert_eq!(stand_of(&Info { approvals: vec![ask("a", 1, ToolKind::Question, "which database?")], ..agent(Status::Running) }), Stand::Needs);
        assert_eq!(stand_of(&Info { exit: Some(ExitInfo { code: Some(1), error: Some("no such file".into()), ..Default::default() }), ..agent(Status::Exited) }), Stand::Stopped);
        assert_eq!(stand_of(&agent(Status::Exited)), Stand::Ended);
        let pause = Pause { reason: "limit".into(), window: None, label: Some("Session".into()), said: String::new(), until: None, since: 5, auto: true, by: "harness".into(), cut: None, checkpoint: None, tries: 0, checking: None, next_check: None };
        assert_eq!(stand_of(&Info { pause: Some(pause.clone()), ..agent(Status::Idle) }), Stand::Paused);
        assert_eq!(stand_of(&Info { pause: Some(pause), ..agent(Status::Running) }), Stand::Working, "its turn finishes first");
        assert_eq!(stand_of(&agent(Status::Disconnected)), Stand::Offline);
    }

    #[test]
    fn needs_you_once_per_approval() {
        let mut w = Watch::default();
        assert!(say(&mut w, agent(Status::Starting)).is_empty(), "first heard: where it stands");
        assert!(say(&mut w, agent(Status::Running)).is_empty());
        let one = Info { approvals: vec![ask("q1", 10, ToolKind::Question, "which database?")], ..agent(Status::Waiting) };
        assert_eq!(say(&mut w, one.clone()), vec![(Kind::NeedsYou, "Asks: which database?".to_owned())]);
        assert!(say(&mut w, one.clone()).is_empty(), "the same approval again says nothing");
        // A second one comes: said, with the first still there.
        let two = Info { approvals: vec![ask("q1", 10, ToolKind::Question, "which database?"), ask("b2", 11, ToolKind::Shell, "cargo test -p kiln")], ..agent(Status::Waiting) };
        assert_eq!(say(&mut w, two), vec![(Kind::NeedsYou, "Asks: which database? (+1 more)".to_owned())]);
        // One answered, one left: nothing new.
        assert!(say(&mut w, Info { approvals: vec![ask("b2", 11, ToolKind::Shell, "cargo test -p kiln")], ..agent(Status::Waiting) }).is_empty());
        // The machine goes away and comes back with it still waiting: said already.
        assert!(say(&mut w, agent(Status::Disconnected)).is_empty());
        assert!(say(&mut w, Info { approvals: vec![ask("b2", 11, ToolKind::Shell, "cargo test -p kiln")], ..agent(Status::Waiting) }).is_empty());
        // A new one after: said.
        assert!(say(&mut w, agent(Status::Running)).is_empty());
        let plan = Info { approvals: vec![ask("p3", 30, ToolKind::Plan, "the plan")], ..agent(Status::Waiting) };
        assert_eq!(say(&mut w, plan), vec![(Kind::NeedsYou, "Proposes a plan".to_owned())]);
    }

    #[test]
    fn a_burst_of_approvals_is_one_note_and_a_flap_is_none() -> anyhow::Result<()> {
        let mut w = Watch::default();
        w.heard(agent(Status::Running));
        let first = w.heard(Info { approvals: vec![ask("a", 1, ToolKind::Shell, "ls")], ..agent(Status::Waiting) });
        let second = w.heard(Info { approvals: vec![ask("a", 1, ToolKind::Shell, "ls"), ask("b", 2, ToolKind::Shell, "pwd")], ..agent(Status::Waiting) });
        let (Some(first), Some(second)) = (first, second) else { anyhow::bail!("both call for a note") };
        assert_eq!(w.due(&first.agent, first.seq).map(|n| n.kind), None, "the newer one replaces it");
        let n = w.due(&second.agent, second.seq);
        assert_eq!(n.map(|n| line(&n.info, n.kind)), Some("Bash: ls (+1 more)".to_owned()));

        // Asked, then answered (by another device, or a rule) before it settled: nothing.
        let mut w = Watch::default();
        w.heard(agent(Status::Running));
        let d = w.heard(Info { approvals: vec![ask("c", 3, ToolKind::Shell, "rm -rf target")], ..agent(Status::Waiting) });
        w.heard(agent(Status::Running));
        assert!(d.and_then(|d| w.due(&d.agent, d.seq)).is_none());
        Ok(())
    }

    #[test]
    fn finished_when_a_turn_it_was_seen_at_ends_unless_you_stopped_it() {
        let mut w = Watch::default();
        assert!(say(&mut w, Info { last_turn: ended(100, "done"), ..agent(Status::Idle) }).is_empty(), "an old turn's end isn't news");
        assert!(say(&mut w, agent(Status::Running)).is_empty());
        let done = Info { last_turn: ended(200, "done"), summary: "**All 160 tests** pass. See [the log](https://x/y).".into(), ..agent(Status::Idle) };
        assert_eq!(say(&mut w, done.clone()), vec![(Kind::Finished, "Finished: All 160 tests pass. See the log.".to_owned())]);
        assert!(say(&mut w, done).is_empty(), "once");
        // Stopped by a person: nothing.
        assert!(say(&mut w, agent(Status::Running)).is_empty());
        assert!(say(&mut w, Info { last_turn: ended(300, "stopped"), ..agent(Status::Idle) }).is_empty());
        // Kept going by kiln before it settled: only the real end is said.
        assert!(say(&mut w, agent(Status::Running)).is_empty());
        let cut = w.heard(Info { last_turn: ended(400, "cut"), ..agent(Status::Idle) });
        w.heard(agent(Status::Running));
        assert!(cut.and_then(|d| w.due(&d.agent, d.seq)).is_none());
        assert_eq!(say(&mut w, Info { last_turn: ended(500, "done"), goal: "Fix the flaky test".into(), ..agent(Status::Idle) }), vec![(Kind::Finished, "Finished: Fix the flaky test".to_owned())]);
        // Its machine went away mid-turn and came back after the turn ended: said.
        assert!(say(&mut w, agent(Status::Running)).is_empty());
        assert!(say(&mut w, agent(Status::Disconnected)).is_empty());
        assert_eq!(say(&mut w, Info { last_turn: ended(600, "done"), ..agent(Status::Idle) }).len(), 1);
    }

    #[test]
    fn stopped_and_paused_once_each() {
        let mut w = Watch::default();
        w.heard(agent(Status::Running));
        let crash = Info { exit: Some(ExitInfo { code: Some(101), error: Some("claude: command not found".into()), ..Default::default() }), ..agent(Status::Exited) };
        assert_eq!(say(&mut w, crash.clone()), vec![(Kind::Stopped, "Stopped: claude: command not found".to_owned())]);
        assert!(say(&mut w, crash.clone()).is_empty());
        assert!(say(&mut w, agent(Status::Disconnected)).is_empty());
        assert!(say(&mut w, crash.clone()).is_empty(), "back from offline, said already");
        // Restarted, and it crashes the same way again: that's news.
        assert!(say(&mut w, agent(Status::Starting)).is_empty());
        assert_eq!(say(&mut w, crash).len(), 1);

        let pause = Pause { reason: "limit".into(), window: None, label: Some("Session".into()), said: String::new(), until: None, since: 77, auto: true, by: "harness".into(), cut: None, checkpoint: None, tries: 0, checking: None, next_check: None };
        let paused = Info { pause: Some(pause), ..agent(Status::Idle) };
        let mut w = Watch::default();
        w.heard(agent(Status::Running));
        assert_eq!(say(&mut w, paused.clone()), vec![(Kind::Paused, "Paused: reached the Session limit · resumes by itself when it resets".to_owned())]);
        assert!(say(&mut w, paused).is_empty());
    }

    #[test]
    fn a_restart_says_nothing_already_said() -> anyhow::Result<()> {
        let waiting = Info { approvals: vec![ask("q1", 10, ToolKind::Question, "which database?")], ..agent(Status::Waiting) };
        let mut before = Watch::default();
        before.heard(agent(Status::Running));
        let d = before.heard(waiting.clone());
        let n = d.and_then(|d| before.due(&d.agent, d.seq));
        let Some(n) = n else { anyhow::bail!("said") };
        // The server restarts: what was said comes back from the store.
        let mut after = Watch::default();
        after.remember("ag1", n.kind.event(), &n.key);
        assert!(say(&mut after, waiting).is_empty());
        // One asked while the server was down is said.
        let mut fresh = Watch::default();
        let down = Info { approvals: vec![ask("q2", 20, ToolKind::Question, "ship it?")], ..agent(Status::Waiting) };
        assert_eq!(say(&mut fresh, down).len(), 1);
        // A crash said before the restart isn't said again; one that happened meanwhile is.
        let crash = Info { exit: Some(ExitInfo { code: Some(1), error: Some("out of memory".into()), ..Default::default() }), ..agent(Status::Exited) };
        let mut after = Watch::default();
        after.remember("ag1", "agent.stopped", "out of memory");
        assert!(say(&mut after, crash.clone()).is_empty());
        assert_eq!(say(&mut Watch::default(), crash).len(), 1);
        Ok(())
    }

    /// A request is known by its id: asked again after a restart (a runner's, its harness's) with
    /// another time, it's said already; a key said before ids alone (`id@at`) is taken as its id.
    #[test]
    fn a_request_asked_again_after_a_restart_is_said_already() {
        let mut w = Watch::default();
        w.heard(agent(Status::Running));
        let first = Info { approvals: vec![ask("q1", 10, ToolKind::Question, "which database?")], ..agent(Status::Waiting) };
        assert_eq!(say(&mut w, first).len(), 1);
        let again = Info { approvals: vec![ask("q1", 99, ToolKind::Question, "which database?")], ..agent(Status::Waiting) };
        let mut after = Watch::default();
        after.remember("ag1", "agent.needs_you", "q1");
        assert!(say(&mut after, again.clone()).is_empty(), "asked again at another time: the same request");
        let mut upgraded = Watch::default();
        upgraded.remember("ag1", "agent.needs_you", "q1@10");
        assert!(say(&mut upgraded, again).is_empty(), "said before as id@at");
        // Waiting without a request still goes by its turn.
        assert!(new_needs("waiting@5", Some(&"waiting@4".to_owned())));
        assert!(!new_needs("waiting@5", Some(&"waiting@5".to_owned())));
    }

    #[test]
    fn words() {
        assert_eq!(label(&Info { name: "Claude Code".into(), title: "Flaky retry test".into(), ..agent(Status::Idle) }), "Flaky retry test");
        assert_eq!(label(&Info { name: "claude code".into(), goal: "\n Fix the parser \nthen docs".into(), ..agent(Status::Idle) }), "Fix the parser");
        assert_eq!(label(&agent(Status::Idle)), "Mantle");
        assert_eq!(plain("# Done\n- **Tests** pass\n```\ncargo test\n```\n1. See `x` and [docs](http://d)"), "Done Tests pass See x and docs");
        assert_eq!(plain("a [b] c"), "a [b] c");
        assert_eq!(clip("abcdef", 4), "abc…");
        assert_eq!(clip("abcd", 4), "abcd");
        assert_eq!(topic("0199f2a4-8c1e-7d3b-9a2f-5e6d7c8b9a01").len(), 32);
        assert!(topic("x").chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_ne!(topic("a"), topic("b"));
        let mut long = agent(Status::Idle);
        long.summary = "word ".repeat(200);
        long.last_turn = ended(1, "done");
        assert!(line(&long, Kind::Finished).chars().count() <= 240);
    }
}

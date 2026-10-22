//! Keep going: an agent that stops part-way through its work, with no goal stack driving it, is
//! told to go on (docs/goals.md, "Keep going"). Any harness: it reads how the agent's last turn
//! ended (`State::last_turn`, which every adapter reports) and what the server saw of it.
//!
//! A turn is taken as stopped part-way when:
//!
//! - its harness cut it short: Claude Code's stop sequence or output limit, an error part-way, a
//!   Codex turn that failed, an ACP agent out of tokens;
//! - its last step was a tool that failed, with nothing said after it;
//! - it said nothing about the work ("No response requested.");
//! - its last words say it's going on ("Next, I'll wire the UI.", "Let me check the logs:") and
//!   then the turn ended there;
//! - its to-dos are still open and its last words don't ask you anything;
//! - its harness started again under it (a crash, a resume): the server saw it at work, and it's
//!   back idle with that turn never having ended. Only for an agent whose runner reports how its
//!   turns end: one that doesn't (a runner from before, a harness kiln runs as a terminal) would
//!   look started again after every turn.
//!
//! Not when you stopped it, it waits on you (an approval, a question, last words that ask), a
//! message is queued, a background task of its runs or an agent it started is at work (it's told
//! when that settles, `harness::told`), a usage limit paused it, a goal stack drives it, or it's
//! switched off for this agent (the "keep_going" control). Kiln keeps an agent going
//! at most [`MAX_KEPT`] turns in a row since a person last said something, and stops sooner when
//! the turns it starts do no work.

use std::collections::HashMap;
use std::sync::LazyLock;

use kiln_seams::HarnessPort;
use parking_lot::Mutex;

use crate::harness::core::KEEP_ID;
use crate::harness::model::{Command, SendMode, Status};
use crate::harness::session::Info;

/// Turns in a row kiln starts, since a person last said something.
pub const MAX_KEPT: u32 = 5;
/// Turns kiln started that called no tool, after which it stops trying.
const IDLE_KEPT: u32 = 2;
/// How long a turn's end settles before kiln acts: a message of yours, queued or typed, goes first.
const SETTLE_MS: i64 = 4_000;
/// A turn that ended longer ago than this (by its machine's clock) is left alone.
const STALE_MS: i64 = 10 * 60_000;

/// What the server saw of one agent. Times are the server's own, so a machine whose clock is off
/// can't make a turn that ended look like one cut short.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Seen {
    /// Last seen at work (running, waiting, starting).
    busy_at: i64,
    /// Its newest turn end (`TurnEnd::at`, its machine's clock), and when the server first saw it.
    end: i64,
    end_seen: i64,
    /// What was last decided on (kept going, or left alone), as the time of what it was about.
    handled: i64,
}

static SEEN: LazyLock<Mutex<HashMap<String, Seen>>> = LazyLock::new(Default::default);

/// Why kiln keeps an agent going.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Why {
    /// Its harness cut the turn short (its word for how).
    Cut(Option<String>),
    AfterError,
    /// It said nothing about the work.
    Hollow,
    /// Its last words say it's going on (them, shortened), and the turn ended there.
    Promised(String),
    Todos { open: u32 },
    Restarted,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Next {
    Wait,
    /// Nothing to do about what happened at `at`.
    Leave { at: i64 },
    Keep { why: Why, at: i64 },
}

/// Words that say nothing about the work: empty, or a model answering a turn it took as nothing
/// it needed to answer.
fn hollow(summary: &str) -> bool {
    let s = summary.trim();
    s.is_empty() || s.starts_with("No response requested")
}

/// The last sentence of `summary` when it says the agent is going on, by itself, now: "I'll run the
/// suite next.", "Let me check the logs:", "Continuing with the migration…". Not when it waits on
/// you ("I'll wait for your go-ahead", "Let me know…", "…once you confirm"), asks something, or
/// leaves things where they are.
fn promises_more(summary: &str) -> Option<String> {
    let text = summary.trim().trim_end_matches(['…', '.', '*', '_', '`', ' ']).trim_end();
    let colon = text.ends_with(':');
    let text = text.trim_end_matches(':');
    let last = text.rsplit(['.', '!', '?', '\n']).map(str::trim).find(|s| !s.is_empty())?;
    let clean = last.trim_start_matches(['-', '*', '>', '"', ' ']).trim();
    let lower = clean.to_lowercase();
    if summary.trim_end().ends_with('?') || ["you", "let me know", "wait", "once ", " if ", "leave", "stop here", "hand"].iter().any(|w| lower.contains(w)) {
        return None;
    }
    const GOING_ON: &[&str] = &[
        "i'll ", "i will ", "i'm going to ", "i am going to ", "let me ", "next, i", "next i", "now i'll", "now i will", "now let me", "continuing", "proceeding",
        "moving on to", "on to ", "starting on ", "going to ",
    ];
    let going = GOING_ON.iter().any(|g| lower.starts_with(g));
    (going || (colon && ["checking", "running", "looking", "reading", "fixing", "testing"].iter().any(|g| lower.starts_with(g)))).then(|| crate::harness::model::clip_middle(clean, 120).0)
}

/// What the server makes of one agent at `now`, given what it `seen` before (updated here, and
/// what's decided on is never decided again).
pub(super) fn decide(info: &Info, seen: &mut Seen, now: i64) -> Next {
    let next = judge(info, seen, now);
    if let Next::Leave { at } | Next::Keep { at, .. } = &next {
        seen.handled = *at;
    }
    next
}

fn judge(info: &Info, seen: &mut Seen, now: i64) -> Next {
    let end = info.last_turn.as_ref().map_or(0, |t| t.at);
    // A newer end only (one that's gone, its state lost in a restart, isn't a new one).
    if end > seen.end {
        seen.end = end;
        seen.end_seen = now;
    }
    // A turn under way. Starting isn't one: an idle agent's harness starts again when it's
    // switched to another harness or handed to a runner's new build, and taking that for a turn
    // cut short sent idle agents back to work after every switch and runner upgrade.
    if matches!(info.status, Status::Running | Status::Waiting) {
        seen.busy_at = now;
        return Next::Wait;
    }
    if info.status == Status::Starting {
        return Next::Wait;
    }
    // At work after its last turn ended, and idle now with no end since: started again under it.
    // Only for an agent known to report its turns' ends.
    let restarted = seen.end > 0 && seen.busy_at > seen.end_seen;
    let at = if restarted { seen.busy_at } else { seen.end_seen };
    if at == 0 || at <= seen.handled {
        return Next::Wait;
    }
    let free = info.status == Status::Idle && info.approvals.is_empty() && info.queued == 0 && info.tasks_running == 0 && info.pause.is_none();
    if !free || now - at < SETTLE_MS {
        return Next::Wait;
    }
    if info.keep_off {
        return Next::Leave { at };
    }
    if restarted {
        return Next::Keep { why: Why::Restarted, at };
    }
    let Some(t) = info.last_turn.as_ref() else { return Next::Leave { at } };
    if now - t.at > STALE_MS || matches!(t.how.as_str(), "stopped" | "limit" | "refused" | "blocked") || t.asks {
        return Next::Leave { at };
    }
    // Kept going enough: as many turns in a row as it may, or ones that did nothing.
    if t.kept >= MAX_KEPT || (t.kept >= IDLE_KEPT && t.tools == 0) {
        tracing::info!(agent = %info.id, kept = t.kept, "keep going: stopped trying");
        return Next::Leave { at };
    }
    let why = if t.how == "cut" {
        Why::Cut(t.why.clone())
    } else if t.after_error {
        Why::AfterError
    } else if hollow(&info.summary) {
        Why::Hollow
    } else if let Some(said) = promises_more(&info.summary) {
        Why::Promised(said)
    } else if info.todos_total > info.todos_done {
        Why::Todos { open: info.todos_total - info.todos_done }
    } else {
        return Next::Leave { at };
    };
    Next::Keep { why, at }
}

/// What kiln says to it, in its own words (shown as kiln's).
pub(super) fn message(why: &Why) -> String {
    let go_on = "Pick up where you left off and carry on with the work. If it's all done, say so; if you need something from me, ask.";
    match why {
        Why::Cut(how) => format!("Your last turn stopped part-way{}. {go_on}", how.as_deref().map(|h| format!(" ({h})")).unwrap_or_default()),
        Why::AfterError => "Your last step failed and the turn ended there. See what went wrong, then carry on. If it's all done, say so; if you're blocked, say what you need.".to_owned(),
        Why::Hollow => format!("Your last turn ended without a word about the work. {go_on}"),
        Why::Promised(said) => format!("You said you'd carry on (\"{said}\"), then the turn ended. Go ahead. If it's all done, say so; if you need something from me, ask."),
        Why::Todos { open } => format!(
            "{open} of your to-dos {} still open. Carry on with {}, or mark {} done if {} are. If you need something from me, ask.",
            if *open == 1 { "is" } else { "are" },
            if *open == 1 { "it" } else { "them" },
            if *open == 1 { "it" } else { "them" },
            if *open == 1 { "it" } else { "they" },
        ),
        Why::Restarted => format!("Your harness started again part-way through a turn. {go_on}"),
    }
}

/// One agent with no goal stack driving it, at `now`: kept going if it stopped part-way. Agents
/// it started that are at work (`children`) are work it has going, as its background tasks are:
/// it isn't prodded while they work, and is told when each settles (`harness::told`).
pub async fn tick<P>(port: &P, info: &Info, now: i64, children: bool)
where
    P: HarnessPort<Agent = Info, Command = Command>,
{
    let held;
    let info = if children {
        held = Info { tasks_running: info.tasks_running + 1, ..info.clone() };
        &held
    } else {
        info
    };
    let next = {
        let mut g = SEEN.lock();
        // First seen (the server just started): only what happens from now on counts.
        let seen = g.entry(info.id.clone()).or_insert_with(|| Seen {
            end: info.last_turn.as_ref().map_or(0, |t| t.at),
            end_seen: now,
            handled: now,
            ..Seen::default()
        });
        decide(info, seen, now)
    };
    if let Next::Keep { why, .. } = next {
        let c = Command::Send { id: format!("{KEEP_ID}{now}"), text: message(&why), mode: SendMode::Queue, attachments: Vec::new(), overrides: None };
        tracing::info!(agent = %info.id, why = ?why, "keep going");
        let _ = port.send(&info.id, c).await;
    }
}

/// Forget agents that are gone.
pub fn retain(ids: &[String]) {
    SEEN.lock().retain(|id, _| ids.contains(id));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::model::TurnEnd;

    fn agent(status: Status, end: Option<TurnEnd>) -> Info {
        Info { id: "a".into(), status, last_turn: end, ..Info::default() }
    }

    fn ended(how: &str, at: i64) -> TurnEnd {
        TurnEnd { at, how: how.into(), tools: 3, ..TurnEnd::default() }
    }

    /// The server first sees the agent at work, then its turn ending as `end` at `at` (server time).
    fn after_turn(end: TurnEnd, summary: &str, todos: (u32, u32), now: i64) -> Next {
        let mut seen = Seen::default();
        assert_eq!(decide(&agent(Status::Running, None), &mut seen, now - 20_000), Next::Wait);
        let mut i = agent(Status::Idle, Some(end));
        i.summary = summary.into();
        (i.todos_done, i.todos_total) = todos;
        assert_eq!(decide(&i, &mut seen, now - 10_000), Next::Wait, "it settles first");
        decide(&i, &mut seen, now)
    }

    #[test]
    fn a_turn_cut_short_goes_on() {
        let now = 1_792_000_000_000;
        let next = after_turn(TurnEnd { why: Some("stop_sequence".into()), ..ended("cut", now - 12_000) }, "No response requested.", (0, 0), now);
        assert!(matches!(next, Next::Keep { why: Why::Cut(Some(ref w)), .. } if w == "stop_sequence"), "{next:?}");
    }

    #[test]
    fn hollow_words_after_work_and_open_todos_go_on() {
        let now = 1_792_000_000_000;
        assert!(matches!(after_turn(ended("done", now - 12_000), "No response requested.", (0, 0), now), Next::Keep { why: Why::Hollow, .. }));
        assert!(matches!(after_turn(ended("done", now - 12_000), "Tests pass; next the docs.", (1, 3), now), Next::Keep { why: Why::Todos { open: 2 }, .. }));
        let failed = TurnEnd { after_error: true, ..ended("done", now - 12_000) };
        assert!(matches!(after_turn(failed, "", (0, 0), now), Next::Keep { why: Why::AfterError, .. }));
    }

    #[test]
    fn a_turn_thats_done_or_stopped_or_asking_is_left_alone() {
        let now = 1_792_000_000_000;
        assert!(matches!(after_turn(ended("done", now - 12_000), "All done: 41/41 pass.", (3, 3), now), Next::Leave { .. }));
        assert!(matches!(after_turn(ended("stopped", now - 12_000), "", (0, 2), now), Next::Leave { .. }));
        // What it needed wasn't allowed (a harness that can't ask): going on would be denied again.
        assert!(matches!(after_turn(ended("blocked", now - 12_000), "", (0, 2), now), Next::Leave { .. }));
        let asks = TurnEnd { asks: true, ..ended("cut", now - 12_000) };
        assert!(matches!(after_turn(asks, "Merge it now?", (0, 2), now), Next::Leave { .. }));
        // Kept going enough already, or the turns kiln started did nothing.
        let many = TurnEnd { kept: MAX_KEPT, ..ended("cut", now - 12_000) };
        assert!(matches!(after_turn(many, "", (0, 0), now), Next::Leave { .. }));
        let idle = TurnEnd { kept: IDLE_KEPT, tools: 0, ..ended("cut", now - 12_000) };
        assert!(matches!(after_turn(idle, "", (0, 0), now), Next::Leave { .. }));
    }

    #[test]
    fn switched_off_waiting_on_you_or_paused_it_waits_or_is_left() {
        let now = 1_792_000_000_000;
        let mut seen = Seen::default();
        decide(&agent(Status::Running, None), &mut seen, now - 20_000);
        let mut i = agent(Status::Idle, Some(ended("cut", now - 12_000)));
        i.queued = 1;
        assert_eq!(decide(&i, &mut seen, now - 10_000), Next::Wait, "it settles first");
        assert_eq!(decide(&i, &mut seen, now), Next::Wait, "your queued message goes first");
        i.queued = 0;
        i.keep_off = true;
        assert!(matches!(decide(&i, &mut seen, now), Next::Leave { .. }));
        assert_eq!(decide(&i, &mut seen, now + 5_000), Next::Wait, "decided once");
    }

    #[test]
    fn started_again_under_a_turn_goes_on_whatever_its_clock() {
        let now = 1_792_000_000_000;
        let mut seen = Seen::default();
        // Its last turn ended (by a clock far ahead of the server's), then it was at work again.
        decide(&agent(Status::Idle, Some(ended("done", now + 3_600_000))), &mut seen, now - 60_000);
        decide(&agent(Status::Running, Some(ended("done", now + 3_600_000))), &mut seen, now - 30_000);
        // Back idle, that turn never ended (the end it reports is the old one, or none at all).
        assert!(matches!(decide(&agent(Status::Idle, None), &mut seen, now), Next::Keep { why: Why::Restarted, .. }));
        // An ordinary turn: at work, then an end seen after it; not a restart.
        let mut seen = Seen::default();
        decide(&agent(Status::Running, None), &mut seen, now - 30_000);
        assert!(!matches!(decide(&agent(Status::Idle, Some(ended("done", now - 3_600_000))), &mut seen, now), Next::Keep { why: Why::Restarted, .. }));
    }

    #[test]
    fn an_idle_harness_that_starts_again_is_not_a_turn_cut_short() {
        // Its turn ended, then its harness started again with nothing under way (a switch to
        // another harness, a runner's new build taking it over): it stays idle.
        let now = 1_792_000_000_000;
        let mut seen = Seen::default();
        decide(&agent(Status::Running, None), &mut seen, now - 60_000);
        let mut done = agent(Status::Idle, Some(ended("done", now - 50_000)));
        done.summary = "Done: the change is in and its tests pass.".into();
        assert_eq!(decide(&done, &mut seen, now - 45_000), Next::Wait, "it settles first");
        assert!(matches!(decide(&done, &mut seen, now - 30_000), Next::Leave { .. }), "nothing to go on with");
        decide(&agent(Status::Starting, Some(ended("done", now - 50_000))), &mut seen, now - 20_000);
        for t in [now, now + 60_000] {
            assert_eq!(decide(&done, &mut seen, t), Next::Wait, "at {t}");
        }
    }

    #[test]
    fn an_agent_that_never_reports_its_turns_is_never_taken_as_started_again() {
        // A runner from before, or a harness kiln runs as a terminal: at work, then idle, no end.
        let now = 1_792_000_000_000;
        let mut seen = Seen::default();
        decide(&agent(Status::Running, None), &mut seen, now - 30_000);
        assert!(matches!(decide(&agent(Status::Idle, None), &mut seen, now), Next::Leave { .. } | Next::Wait));
        assert_eq!(decide(&agent(Status::Idle, None), &mut seen, now + 60_000), Next::Wait);
    }

    #[test]
    fn last_words_that_say_it_goes_on_and_then_dont_go_on() {
        let now = 1_792_000_000_000;
        for said in ["Tests pass. I'll now update the docs.", "Let me check the remaining failures:", "Next, I'll wire the UI.", "Continuing with the migration…", "The box is behind. Checking the runner's log:"] {
            assert!(matches!(after_turn(ended("done", now - 12_000), said, (0, 0), now), Next::Keep { why: Why::Promised(_), .. }), "{said}");
        }
        for said in ["Let me know if you want the docs updated too.", "I'll wait for your go-ahead.", "I'll do that once you confirm.", "Done: 41/41 pass.", "Should I go on with the rest?", "I'll leave the rest for you."] {
            assert!(!matches!(after_turn(ended("done", now - 12_000), said, (0, 0), now), Next::Keep { .. }), "{said}");
        }
        assert!(message(&Why::Promised("Next, I'll wire the UI".into())).starts_with("You said you'd carry on (\"Next, I'll wire the UI\")"));
    }

    #[test]
    fn what_kiln_says_names_why() {
        assert!(message(&Why::Cut(Some("max_tokens".into()))).starts_with("Your last turn stopped part-way (max_tokens)."));
        assert!(message(&Why::Todos { open: 1 }).starts_with("1 of your to-dos is still open."));
        assert!(message(&Why::Todos { open: 3 }).starts_with("3 of your to-dos are still open."));
    }
}

//! Pausing a session at a usage limit, and going on when the limit resets (docs/agents.md,
//! "Usage limits: pause and resume").
//!
//! A session pauses when its harness says its account reached a usage limit (Claude Code's
//! `rate_limit` error, Codex's `usageLimitExceeded`, an ACP agent's limit error), when another
//! session on the account did and the limit is its own too (`Command::LimitHold`, from the server;
//! [`account_hold`]), or once a limit is as full as `State::pause_at`: between turns, or in the
//! middle of one at its next step (what runs then, a command or an edit, finishes first;
//! [`stop_mid_turn`]). While it is paused nothing goes to the harness: kiln's queue, goals and
//! plan steps wait, and a message sent meanwhile joins the queue. Its folder is kept as it was in
//! a hidden git ref (`Effect::Checkpoint`).
//!
//! Once the window starts over (`Pause::until`, plus a margin and a spread across the account's
//! sessions), kiln asks the harness for its usage. A limit no longer reached ends the pause: the
//! turn the limit cut off goes on first ([`CONTINUE`]), then the queue. One still reached waits
//! for the window's next reset. A harness that can't say goes on and finds out. A person can end
//! the pause at any time, or keep it until they do (`Pause::auto`).

use super::core::{Core, Effect};
use super::model::{Body, Checkpoint, Command, Cut, Level, Limit, Pause, QueueItem, Status, ToolStatus, same_account};

/// A usage limit was reached.
pub const LIMIT: &str = "limit";
/// A limit got as full as `State::pause_at`.
pub const THRESHOLD: &str = "threshold";

/// After a window's reset, how long before kiln checks it: the account's usage takes a moment
/// to start over.
const AFTER_RESET_MS: i64 = 30_000;
/// Resumes of one account's sessions are spread over this long, so they don't all start at once.
const SPREAD_MS: i64 = 90_000;
/// How long a check waits for the harness's answer before going on regardless.
const CHECK_WAIT_MS: i64 = 30_000;
/// When the reset time isn't known: check after these delays, the last one again and again.
const RETRY_MS: [i64; 4] = [10 * 60_000, 20 * 60_000, 40 * 60_000, 60 * 60_000];

/// What the harness is told when it goes on with a turn a limit cut off.
pub const CONTINUE: &str = "Your previous turn was cut off by a usage limit, which has reset now. Continue where you left off.";
/// The same, for the harness the session switched to while it was paused.
pub const CONTINUE_ELSEWHERE: &str = "The previous turn was cut off by the other model's usage limit. Continue where it left off.";

/// What a harness said about a limit it reached.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Hit {
    /// The window (`Limit::id`), when known.
    pub window: Option<String>,
    /// Unix ms it starts over, when known.
    pub until: Option<i64>,
    /// Its words.
    pub said: String,
}

impl Hit {
    /// The harness's words, with the window and reset taken from the session's limits: the reached
    /// window that resets last (a weekly limit outlasts the 5-hour one). `until` (from the error
    /// itself) is used when the limits say nothing.
    pub fn from_limits(limits: &[Limit], said: impl Into<String>, until: Option<i64>, now: i64) -> Self {
        let said = said.into();
        let reached = limits
            .iter()
            .filter(|l| l.status.as_deref() == Some("rejected") || l.used_percent.is_some_and(|u| u >= 100.0))
            .filter(|l| l.resets_at.is_some_and(|r| r > now))
            .max_by_key(|l| l.resets_at.unwrap_or(0));
        match reached {
            Some(l) => Hit { window: Some(l.id.clone()), until: l.resets_at, said },
            None => Hit { window: None, until: until.filter(|u| *u > now).or_else(|| try_again_in(&said, now)), said },
        }
    }
}

/// Whether a harness's words say its account's usage limit was reached (not an output, context
/// or turn limit).
pub fn says_limit(text: &str) -> bool {
    let t = text.to_lowercase();
    let usage = ["usage limit", "session limit", "weekly limit", "rate limit", "rate_limit", "usage_limit", "quota", "plan limit", "5-hour limit", "5 hour limit"]
        .iter()
        .any(|w| t.contains(w));
    let reached = ["hit your", "reached", "exceeded", "resets", "try again", "limit_reached", "exhausted"].iter().any(|w| t.contains(w));
    usage && reached
}

/// "… try again in 2 hours 13 minutes." → when that is (Codex's words when its limit is reached).
pub fn try_again_in(text: &str, now: i64) -> Option<i64> {
    let t = text.to_lowercase();
    let rest = t.split("try again in").nth(1)?;
    let mut total: i64 = 0;
    let mut number: Option<i64> = None;
    for word in rest.split(|c: char| c.is_whitespace() || c == ',').filter(|w| !w.is_empty()) {
        let word = word.trim_end_matches(['.', ')', ';']);
        if let Ok(n) = word.parse::<i64>() {
            number = Some(n);
            continue;
        }
        let unit = if word.starts_with("day") {
            86_400_000
        } else if word.starts_with("hour") || word == "h" || word == "hr" || word == "hrs" {
            3_600_000
        } else if word.starts_with("min") || word == "m" {
            60_000
        } else if word.starts_with("sec") || word == "s" {
            1_000
        } else if word == "and" {
            continue;
        } else {
            break;
        };
        if let Some(n) = number.take() {
            total = total.saturating_add(n.saturating_mul(unit));
        }
    }
    (total > 0).then(|| now.saturating_add(total))
}

/// Start a pause, or carry on with one: nothing more goes to the harness until it ends. True when
/// this started it. `by`: "harness", "account" or "threshold".
pub fn hold(core: &mut Core, reason: &str, by: &str, hit: Hit, cut: Option<Cut>) -> bool {
    let now = Core::now();
    if let Some(p) = core.state.pause.as_mut() {
        // Already paused: what's new about it counts (a later reset, the harness's own words).
        let mut changed = false;
        if let Some(u) = hit.until.filter(|u| p.until.is_none_or(|old| *u > old)) {
            p.until = Some(u);
            p.next_check = None;
            changed = true;
        }
        if p.window.is_none() && hit.window.is_some() {
            p.window = hit.window;
            changed = true;
        }
        if p.said.is_empty() && !hit.said.is_empty() {
            p.said = hit.said;
            changed = true;
        }
        if p.cut.is_none() && cut.is_some() {
            p.cut = cut;
            changed = true;
        }
        // A limit reached outranks a stop point.
        if reason == LIMIT && p.reason != LIMIT {
            p.reason = LIMIT.to_owned();
            changed = true;
        }
        if changed {
            core.touch_state();
        }
        return false;
    }
    let label = hit.window.as_ref().and_then(|w| core.state.limits.iter().find(|l| &l.id == w)).map(|l| l.label.clone());
    let auto = !core.state.manual_resume;
    let text = pause_words(reason, by, label.as_deref(), &hit.said, auto, cut.is_some());
    let next_check = hit.until.is_none().then(|| now.saturating_add(retry_after(0)));
    core.state.pause = Some(Pause {
        reason: reason.to_owned(),
        window: hit.window,
        label,
        said: hit.said,
        until: hit.until,
        since: now,
        auto,
        by: by.to_owned(),
        cut,
        checkpoint: None,
        tries: 0,
        checking: None,
        next_check,
    });
    core.upsert(&format!("pause-{now}"), None, Body::Notice { level: Level::Warn, text });
    core.effects.push(Effect::Checkpoint { at: now });
    core.touch_state();
    true
}

/// A limit another session on the account reached (`Command::LimitHold`): its harness is asked
/// for its usage, and the answer settles it ([`checked`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Ask {
    pub hit: Hit,
    /// The account that reached it, when the server said.
    pub account: Option<String>,
    /// Unix ms the harness was asked, once it was.
    pub asked: Option<i64>,
}

/// Another session on the account reached a usage limit (`Command::LimitHold`): this one pauses
/// until it resets too, rather than finding out by failing, if the limit is its own as well. Only
/// the account it's signed in to now counts: a limit of `account` (the one that reached it, as the
/// server says) when it's signed in as another leaves it be, as does one its own limits show isn't
/// its own ([`shares`]: another account's window, another model's). Otherwise its harness is asked
/// for its usage first ([`step`], then [`checked`]), and only that answer holds it: what it knew
/// of its limits may be out of date, or from before it signed in as someone else, and a session
/// with room is never paused for another's limit. Paused, it keeps to its own settings, as at any
/// limit: whether it goes on by itself at the reset, and whether on another machine.
pub fn account_hold(core: &mut Core, hit: Hit, account: Option<String>) {
    // On no account kiln knows: no other session's limit is its own.
    let Some(own) = core.state.account.as_deref() else { return };
    if account.as_deref().is_some_and(|a| !same_account(a, own)) {
        return;
    }
    if shares(&core.state.limits, &core.state.model, hit.window.as_deref(), hit.until, Core::now()) == Some(false) {
        return;
    }
    core.limit_ask = Some(Ask { hit, account, asked: None });
}

/// Whether a limit another session on the account reached (its window, and when that starts over)
/// is this session's too, as its own limits say. Some(false): they show it isn't: that window starts
/// over at another time here (its usage is another account's), or the window is another model's
/// (Weekly · Opus, to a session on Sonnet). Some(true): they show it reached here too. None: they
/// don't say (nothing known of the window here, only from before it last started over, or not full
/// when last reported). With no window said: whether any of its own is reached.
pub fn shares(limits: &[Limit], model: &str, window: Option<&str>, until: Option<i64>, now: i64) -> Option<bool> {
    let Some(window) = window else {
        return limits.iter().any(|l| for_model(&l.id, model) && reached(l, now)).then_some(true);
    };
    if !for_model(window, model) {
        return Some(false);
    }
    let own = limits.iter().find(|l| l.id == window && l.unit.is_none())?;
    let resets = own.resets_at.filter(|r| *r > now)?;
    if until.is_some_and(|u| resets.abs_diff(u) > SAME_RESET_MS.unsigned_abs()) {
        return Some(false);
    }
    reached(own, now).then_some(true)
}

/// Whether a window counts for a session on `model`: the account's own windows count for every
/// model; one model's (`seven_day_opus`, `model:fable`) for that model, or one kiln can't tell.
fn for_model(window: &str, model: &str) -> bool {
    let only = window.strip_prefix("model:").or_else(|| window.strip_prefix("seven_day_").filter(|m| matches!(*m, "opus" | "sonnet")));
    let model = model.to_lowercase();
    only.is_none_or(|m| !model.starts_with("claude-") || model.contains(m))
}

/// Whether a window is used up as last reported (or said reached, when its use isn't known), and
/// hasn't started over since.
fn reached(l: &Limit, now: i64) -> bool {
    l.unit.is_none() && l.resets_at.is_none_or(|r| r > now) && l.used_percent.map_or(l.status.as_deref() == Some("rejected"), |u| u >= 100.0)
}

/// The timeline's one line about a pause; `cut`: it stopped a turn part-way.
fn pause_words(reason: &str, by: &str, label: Option<&str>, said: &str, auto: bool, cut: bool) -> String {
    let limit = label.map_or_else(|| "a usage limit".to_owned(), |l| format!("its {l} limit"));
    let what = match (reason, by) {
        // `said`: "96% used, the stop point is 95%".
        (THRESHOLD, _) if !said.is_empty() => format!("Paused at the stop point: {limit} is {said}"),
        (THRESHOLD, _) => format!("Paused at the stop point: {limit} is as full as the stop point set for it"),
        (_, "account") => format!("Paused: another agent on this account reached {}", label.map_or_else(|| "a usage limit".to_owned(), |l| format!("the {l} limit"))),
        _ if !said.trim().is_empty() => format!("Paused: {}", said.trim().trim_end_matches('.')),
        _ => format!("Paused: it reached {limit}"),
    };
    let then = if auto { "it goes on by itself once the limit resets" } else { "it goes on when you resume it" };
    let turn = if reason == THRESHOLD && cut { " The step it was on finished; the turn goes on from there." } else { "" };
    format!("{what}.{turn} Queued messages and goals wait; {then}.")
}

/// End the pause: the timeline says why. The pause ended is returned, for the adapter to go on
/// with what it cut off (`Pause::cut`) and then the queue.
pub fn release(core: &mut Core, why: &str) -> Option<Pause> {
    let p = core.state.pause.take()?;
    let now = Core::now();
    // A window that started over isn't reached any more, whatever an older event said.
    for l in &mut core.state.limits {
        if l.status.as_deref() == Some("rejected") && l.used_percent.is_none_or(|u| u < 100.0) {
            l.status = None;
        }
    }
    core.upsert(&format!("resume-{now}"), None, Body::Notice { level: Level::Info, text: why.to_owned() });
    core.touch_state();
    Some(p)
}

/// What the adapter does on a tick (`step`).
#[derive(Debug, PartialEq)]
pub enum Step {
    Nothing,
    /// Ask the harness for its usage (the answer goes to `checked`).
    Check,
    /// The pause ended: go on.
    GoOn(Box<Pause>),
}

/// A pause's timer, every tick: time to check whether its limit reset, or to give up waiting for
/// the answer to a check and go on. `key` spreads one account's sessions apart (its session id).
/// Another session's limit to ask about ([`account_hold`]) is asked first; with no answer, it's
/// let be.
pub fn step(core: &mut Core, now: i64, key: &str) -> Step {
    match core.limit_ask.as_ref().map(|a| a.asked) {
        Some(None) => {
            if let Some(a) = core.limit_ask.as_mut() {
                a.asked = Some(now);
            }
            return Step::Check;
        }
        Some(Some(at)) if now.saturating_sub(at) >= CHECK_WAIT_MS => core.limit_ask = None,
        _ => {}
    }
    let Some(p) = core.state.pause.as_ref() else { return Step::Nothing };
    if !p.auto {
        return Step::Nothing;
    }
    if let Some(started) = p.checking {
        if now.saturating_sub(started) < CHECK_WAIT_MS {
            return Step::Nothing;
        }
        // No answer: go on, and let the harness say.
        return release(core, "The limit's reset time came; going on.").map_or(Step::Nothing, |p| Step::GoOn(Box::new(p)));
    }
    let due = match (p.next_check, p.until) {
        (Some(at), _) => now >= at,
        (None, Some(until)) => now >= until.saturating_add(AFTER_RESET_MS).saturating_add(spread(key)),
        (None, None) => now >= p.since.saturating_add(retry_after(p.tries)),
    };
    if !due {
        return Step::Nothing;
    }
    if let Some(p) = core.state.pause.as_mut() {
        p.checking = Some(now);
    }
    core.touch_state();
    Step::Check
}

/// The harness answered a check (its usage is in `State::limits` now): the pause ends when its
/// limit isn't reached any more, else it waits for the window's next reset (or checks again
/// later, when that isn't known). The pause ended is returned. Asked about another session's
/// limit ([`account_hold`]), it pauses only if its own usage shows that limit reached.
pub fn checked(core: &mut Core, now: i64) -> Option<Pause> {
    // Held for another session's limit only while still signed in to that account (a sign-in seen
    // meanwhile is another), and as the fresh usage says.
    if let Some(ask) = core.limit_ask.take()
        && ask.account.as_deref().is_none_or(|a| core.state.account.as_deref().is_some_and(|own| same_account(a, own)))
        && shares(&core.state.limits, &core.state.model, ask.hit.window.as_deref(), ask.hit.until, now) == Some(true)
    {
        hold(core, LIMIT, "account", ask.hit, None);
    }
    let p = core.state.pause.as_ref()?;
    // A limit the account no longer has (it was signed out of, or its limit started over early):
    // the window waited for resets at another time now and isn't full. It goes on now rather than
    // at a reset that won't come.
    if p.checking.is_none() && !core.state.manual_resume && gone(&core.state.limits, p, cap_of(p, core.state.pause_at)) {
        let why = match p.label.as_deref() {
            Some(l) => format!("The {l} limit it waited for is gone; going on."),
            None => "The limit it waited for is gone; going on.".to_owned(),
        };
        return release(core, &why);
    }
    p.checking?;
    let cap = cap_of(p, core.state.pause_at);
    let reached = still_reached(&core.state.limits, p.window.as_deref(), cap, now);
    if reached == Some(true) {
        let next = reset_of(&core.state.limits, p.window.as_deref(), cap, now);
        if let Some(p) = core.state.pause.as_mut() {
            p.checking = None;
            p.tries = p.tries.saturating_add(1);
            match next {
                Some(r) => {
                    p.until = Some(r);
                    p.next_check = None;
                }
                None => p.next_check = Some(now.saturating_add(retry_after(p.tries))),
            }
        }
        core.touch_state();
        return None;
    }
    let why = match core.state.pause.as_ref().and_then(|p| p.label.clone()) {
        Some(l) => format!("The {l} limit reset; going on."),
        None => "The limit reset; going on.".to_owned(),
    };
    release(core, &why)
}

/// Whether the window pause `p` waits for is gone: the usage now says it resets at another time
/// than the pause waits for, and isn't full.
fn gone(limits: &[Limit], p: &Pause, cap: f64) -> bool {
    let (Some(window), Some(until)) = (p.window.as_deref(), p.until) else { return false };
    limits.iter().filter(|l| l.id == window && l.unit.is_none()).any(|l| l.used_percent.is_some_and(|u| u < cap) && l.resets_at.is_some_and(|r| r.abs_diff(until) > SAME_RESET_MS.unsigned_abs()))
}

/// Whether the window a pause waits for is still as full as it may get: Some(true) yes,
/// Some(false) no, None when the limits don't say.
fn still_reached(limits: &[Limit], window: Option<&str>, cap: f64, now: i64) -> Option<bool> {
    let mut seen = false;
    for l in limits.iter().filter(|l| window.is_none_or(|w| l.id == w) && l.unit.is_none()) {
        let Some(used) = l.used_percent else { continue };
        seen = true;
        if used >= cap && l.resets_at.is_none_or(|r| r > now) {
            return Some(true);
        }
    }
    seen.then_some(false)
}

/// When the window a pause waits for starts over next, as the limits say.
fn reset_of(limits: &[Limit], window: Option<&str>, cap: f64, now: i64) -> Option<i64> {
    limits
        .iter()
        .filter(|l| window.is_none_or(|w| l.id == w) && l.used_percent.is_some_and(|u| u >= cap))
        .filter_map(|l| l.resets_at)
        .filter(|r| *r > now)
        .max()
}

/// How full a pause's window may be for it to end: under 100 % for a limit reached, under the stop
/// point for a threshold.
fn cap_of(p: &Pause, pause_at: Option<u8>) -> f64 {
    match (p.reason.as_str(), pause_at) {
        (THRESHOLD, Some(at)) => f64::from(at),
        _ => 100.0,
    }
}

/// A window's reset, as different reports give it, drifts a little; one that started over is hours
/// (or days) later.
const SAME_RESET_MS: i64 = 10 * 60_000;

/// The windows as full as the stop point (`State::pause_at`) now, but those a person resumed past.
fn past_stop_point(core: &Core) -> impl Iterator<Item = &Limit> {
    let at = core.state.pause_at.map(f64::from);
    let now = Core::now();
    core.state.limits.iter().filter(move |l| {
        let waived = core.state.stop_waived.iter().any(|(id, until)| *id == l.id && l.resets_at.is_some_and(|r| r <= until.saturating_add(SAME_RESET_MS)));
        at.is_some_and(|at| l.used_percent.is_some_and(|u| u >= at)) && l.unit.is_none() && l.resets_at.is_some_and(|r| r > now) && !waived
    })
}

/// The limit as full as the stop point (`State::pause_at`), when one is: the one that resets
/// last, as the pause's hit.
fn over_stop_point(core: &Core) -> Option<Hit> {
    let at = core.state.pause_at?;
    past_stop_point(core)
        .max_by_key(|l| l.resets_at.unwrap_or(0))
        .map(|l| Hit { window: Some(l.id.clone()), until: l.resets_at, said: format!("{}% used, the stop point is {at}%", l.used_percent.unwrap_or(0.0).round()) })
}

/// A person resumed it: the windows now past its stop point are left be until they start over
/// (they chose to go on regardless); a limit it reaches still pauses it.
pub fn waive_stop_point(core: &mut Core) {
    let now = Core::now();
    let mut waived: Vec<(String, i64)> = core.state.stop_waived.iter().filter(|(_, until)| *until > now).cloned().collect();
    waived.extend(past_stop_point(core).filter_map(|l| l.resets_at.map(|r| (l.id.clone(), r))).collect::<Vec<_>>());
    if waived != core.state.stop_waived {
        core.state.stop_waived = waived;
        core.touch_state();
    }
}

/// Between turns, before kiln's queue (or a goal, or a message) starts another: once a limit is
/// as full as `State::pause_at`, a pause starts here instead. True when the session is paused.
pub fn at_threshold(core: &mut Core) -> bool {
    if core.state.pause.is_some() {
        return true;
    }
    let Some(hit) = over_stop_point(core) else { return false };
    hold(core, THRESHOLD, THRESHOLD, hit, None)
}

/// In the middle of a turn, once a limit is as full as `State::pause_at`: the turn stops at its
/// next step, not at its end (an agent working on its own can go for hours in one turn). What
/// runs then, a command or an edit, finishes first; then the session pauses, and the turn it cut
/// off goes on first when the pause ends. True when the turn is to stop now: the caller
/// interrupts the harness. A turn waiting on a person (an approval) uses nothing meanwhile, and is
/// left to them.
pub fn stop_mid_turn(core: &mut Core) -> bool {
    if core.state.pause.is_some() || core.state.status != Status::Running || core.hold {
        return false;
    }
    let Some(hit) = over_stop_point(core) else { return false };
    if step_under_way(core) {
        return false;
    }
    let cut = cut_now(core);
    hold(core, THRESHOLD, THRESHOLD, hit, Some(cut))
}

/// A tool the turn runs now: a command, an edit, a read (a subagent's own steps count; the call
/// that started the subagent only waits on them, so it doesn't).
fn step_under_way(core: &Core) -> bool {
    let Some(since) = core.state.turn_started else { return false };
    core.events()
        .rev()
        .take_while(|e| e.at >= since)
        .any(|e| matches!(&e.body, Body::Tool(t) if t.status == ToolStatus::Running && t.task.is_none()))
}

/// A command on its way to the harness: a message sent while the session is paused (or at its
/// stop point) joins the queue instead (None); anything else goes on.
pub fn let_through(core: &mut Core, c: Command) -> Option<Command> {
    if !matches!(c, Command::Send { .. }) || !at_threshold(core) {
        return Some(c);
    }
    if let Command::Send { id, text, attachments, overrides, .. } = c {
        core.state.queue.push(QueueItem { id, text, at: Core::now(), attachments, overrides, reply_to: None });
        core.touch_state();
    }
    None
}

/// Whether kiln's queue may send its next message now: not held for a switch, not paused, not at
/// the stop point (which starts a pause).
pub fn open(core: &mut Core) -> bool {
    !core.hold && !at_threshold(core)
}

/// The job an `Effect::Checkpoint` comes back under (`run_done`), followed by the pause's start.
pub const CHECKPOINT_JOB: &str = "kiln-checkpoint:";

/// A checkpoint was made (code 0, output "<ref> <commit>") or couldn't be (the folder isn't a
/// repository): the pause it was made for keeps it.
pub fn checkpointed(core: &mut Core, at: &str, code: Option<i32>, output: &str) {
    let Ok(at) = at.parse::<i64>() else { return };
    if code != Some(0) {
        return;
    }
    let mut words = output.split_whitespace();
    let (Some(git_ref), Some(commit)) = (words.next(), words.next()) else { return };
    if let Some(p) = core.state.pause.as_mut().filter(|p| p.since == at) {
        p.checkpoint = Some(Checkpoint { git_ref: git_ref.to_owned(), commit: commit.to_owned(), at });
        core.touch_state();
    }
}

/// Keep `cwd`'s repository as it is now, untracked files included and ignored ones not, in the
/// commit a hidden ref (`refs/kiln/checkpoints/<session>/<at>`) points at: (ref, commit). A
/// throwaway index is used, so the folder, its index and its branches stay as they are. None
/// when `cwd` isn't in a repository (or git isn't there).
pub fn snapshot(cwd: &str, session: &str, at: i64) -> Option<(String, String)> {
    use std::process::{Command as Proc, Stdio};
    let index = std::env::temp_dir().join(format!("kiln-checkpoint-{}-{at}.index", session.replace(['/', '\\'], "_")));
    let git = |args: &[&str], with_index: bool| -> Option<String> {
        let mut c = Proc::new("git");
        c.arg("-C").arg(cwd).args(args).stdin(Stdio::null()).stderr(Stdio::null());
        for (k, v) in [("GIT_AUTHOR_NAME", "kiln"), ("GIT_AUTHOR_EMAIL", "kiln@localhost"), ("GIT_COMMITTER_NAME", "kiln"), ("GIT_COMMITTER_EMAIL", "kiln@localhost")] {
            c.env(k, v);
        }
        if with_index {
            c.env("GIT_INDEX_FILE", &index);
        }
        let o = c.output().ok().filter(|o| o.status.success())?;
        Some(String::from_utf8_lossy(&o.stdout).trim().to_owned())
    };
    let made = (|| {
        let top = git(&["rev-parse", "--show-toplevel"], false)?;
        let head = git(&["rev-parse", "--verify", "--quiet", "HEAD"], false).filter(|h| !h.is_empty());
        // From HEAD's tree, so files that didn't change cost nothing; then everything not ignored.
        if head.is_some() {
            git(&["read-tree", "HEAD"], true)?;
        }
        git(&["-C", &top, "add", "--all"], true)?;
        let tree = git(&["write-tree"], true)?;
        let message = format!("kiln checkpoint of agent {session} at {at}");
        let mut args = vec!["commit-tree", tree.as_str(), "-m", message.as_str()];
        if let Some(h) = &head {
            args.extend(["-p", h.as_str()]);
        }
        let commit = git(&args, false)?;
        let git_ref = format!("refs/kiln/checkpoints/{session}/{at}");
        git(&["update-ref", &git_ref, &commit], false)?;
        Some((git_ref, commit))
    })();
    let _ = std::fs::remove_file(&index);
    made
}

/// Ids kiln gives the messages it queues itself (`continue_later`).
pub const KILN_ID: &str = "kiln-";

/// The cut-off turn goes on once the harness can take it (it ended, or a turn is under way): kiln's
/// message waits at the front of the queue.
pub fn continue_later(core: &mut Core, text: &str) {
    core.state.queue.insert(0, QueueItem { id: format!("{KILN_ID}{}", crate::util::new_id()), text: text.to_owned(), at: Core::now(), attachments: Vec::new(), overrides: None, reply_to: None });
    core.touch_state();
}

/// A queued message went out: kiln's own reads as kiln's in the timeline.
pub fn sent_from_queue(core: &mut Core, id: &str) {
    if id.starts_with(KILN_ID) {
        from_kiln(core, id);
    }
}

/// Mark a message kiln sent itself (`CONTINUE`) as kiln's in the timeline.
pub fn from_kiln(core: &mut Core, id: &str) {
    if let Some(e) = core.get_mut(id)
        && let Body::User { origin, .. } = &mut e.body
    {
        *origin = Some("kiln".to_owned());
    }
}

/// The turn the session is in now (or last began), for a pause that cuts it off: the latest
/// message it was sent.
pub fn cut_now(core: &Core) -> Cut {
    let last = core
        .events()
        .filter_map(|e| match &e.body {
            Body::User { text, client_id, .. } => Some((client_id.clone(), text.clone())),
            _ => None,
        })
        .next_back();
    let (message, text) = last.unwrap_or_default();
    Cut { message, text: super::model::clip_middle(&text, 300).0, at: Core::now() }
}

/// Spread over `SPREAD_MS`, the same for the same key.
fn spread(key: &str) -> i64 {
    // FNV-1a: stable across runs and machines.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in key.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    i64::try_from(h % u64::try_from(SPREAD_MS).unwrap_or(1)).unwrap_or(0)
}

fn retry_after(tries: u32) -> i64 {
    let i = usize::try_from(tries).unwrap_or(usize::MAX);
    RETRY_MS.get(i).or(RETRY_MS.last()).copied().unwrap_or(3_600_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(id: &str, used: f64, resets_at: i64) -> Limit {
        Limit { id: id.to_owned(), label: if id == "five_hour" { "Session".into() } else { "Weekly".into() }, used_percent: Some(used), resets_at: Some(resets_at), ..Limit::default() }
    }

    #[test]
    fn limit_words_are_told_from_other_limits() {
        assert!(says_limit("You've hit your session limit · resets 10:30pm (America/Los_Angeles)"));
        assert!(says_limit("You've hit your weekly limit · resets Oct 20, 9am"));
        assert!(says_limit("You've hit your usage limit. Upgrade to Pro or try again in 2 hours 13 minutes."));
        assert!(says_limit("Rate limit reached for requests"));
        assert!(!says_limit("Stopped: the turn limit was reached."));
        assert!(!says_limit("Stopped: the output limit was reached."));
        assert!(!says_limit("Context limit reached"));
    }

    #[test]
    fn try_again_in_reads_codexs_words() {
        let now = 1_000_000;
        assert_eq!(try_again_in("You've hit your usage limit. Try again in 2 hours 13 minutes.", now), Some(now + 2 * 3_600_000 + 13 * 60_000));
        assert_eq!(try_again_in("try again in 1 day, 3 hours and 5 minutes", now), Some(now + 86_400_000 + 3 * 3_600_000 + 5 * 60_000));
        assert_eq!(try_again_in("try again in 45s", now), None);
        assert_eq!(try_again_in("try again in 45 seconds", now), Some(now + 45_000));
        assert_eq!(try_again_in("no hint here", now), None);
    }

    #[test]
    fn a_hit_takes_the_reached_window_that_resets_last() {
        let now = 1_000;
        let limits = [window("five_hour", 100.0, 5_000), window("seven_day", 100.0, 90_000), window("seven_day_opus", 40.0, 99_000)];
        let hit = Hit::from_limits(&limits, "You've hit your weekly limit", None, now);
        assert_eq!((hit.window.as_deref(), hit.until), (Some("seven_day"), Some(90_000)));
        // Nothing reached in the limits: the error's own reset, else its words.
        let hit = Hit::from_limits(&[window("five_hour", 20.0, 5_000)], "x", Some(7_000), now);
        assert_eq!((hit.window, hit.until), (None, Some(7_000)));
        let hit = Hit::from_limits(&[], "try again in 3 minutes", None, now);
        assert_eq!(hit.until, Some(now + 180_000));
    }

    #[test]
    fn a_pause_holds_once_and_says_so_once() {
        let mut core = Core::new();
        core.state.limits = vec![window("five_hour", 100.0, Core::now() + 3_600_000)];
        let hit = Hit::from_limits(&core.state.limits, "You've hit your session limit · resets 10:30pm", None, Core::now());
        assert!(hold(&mut core, LIMIT, "harness", hit.clone(), None));
        assert!(!hold(&mut core, LIMIT, "harness", hit, None));
        let notices = core.events().filter(|e| matches!(e.body, Body::Notice { .. })).count();
        assert_eq!(notices, 1);
        let p = core.state.pause.clone().unwrap_or_default();
        assert_eq!((p.reason.as_str(), p.window.as_deref(), p.label.as_deref(), p.auto), (LIMIT, Some("five_hour"), Some("Session"), true));
        assert!(core.effects.iter().any(|e| matches!(e, Effect::Checkpoint { .. })));
    }

    #[test]
    fn at_the_reset_it_checks_then_goes_on_or_waits_again() {
        let mut core = Core::new();
        let now = Core::now();
        let until = now + 60_000;
        core.state.limits = vec![window("five_hour", 100.0, until)];
        hold(&mut core, LIMIT, "harness", Hit { window: Some("five_hour".into()), until: Some(until), said: String::new() }, None);
        // Before the reset (and its margin and spread): nothing.
        assert_eq!(step(&mut core, now + 1_000, "s1"), Step::Nothing);
        let due = until + AFTER_RESET_MS + SPREAD_MS;
        assert_eq!(step(&mut core, due, "s1"), Step::Check);
        // Still reached (it resets again later): waits for that reset.
        core.state.limits = vec![window("five_hour", 100.0, due + 3_600_000)];
        assert!(checked(&mut core, due).is_none());
        let p = core.state.pause.clone().unwrap_or_default();
        assert_eq!((p.until, p.tries, p.checking), (Some(due + 3_600_000), 1, None));
        // Next time it isn't: the pause ends.
        let later = due + 3_600_000 + AFTER_RESET_MS + SPREAD_MS;
        assert_eq!(step(&mut core, later, "s1"), Step::Check);
        core.state.limits = vec![window("five_hour", 3.0, later + 5 * 3_600_000)];
        assert!(checked(&mut core, later).is_some());
        assert!(core.state.pause.is_none());
    }

    #[test]
    fn a_pause_for_a_window_that_started_over_at_another_time_goes_on_at_the_next_usage() {
        let mut core = Core::new();
        let now = Core::now();
        let until = now + 3 * 86_400_000;
        core.state.limits = vec![window("seven_day", 100.0, until)];
        hold(&mut core, LIMIT, "account", Hit { window: Some("seven_day".into()), until: Some(until), said: String::new() }, None);
        assert!(checked(&mut core, now).is_none() && core.state.pause.is_some());
        // The week it waits for, a little less full or its reset said a little differently: it waits.
        core.state.limits = vec![window("seven_day", 97.0, until + 60_000)];
        assert!(checked(&mut core, now).is_none() && core.state.pause.is_some());
        // Its usage says the week starts over days later, and has room (another account now, or
        // the limit started over early): it goes on.
        core.state.limits = vec![window("seven_day", 67.0, until + 3 * 86_400_000)];
        let ended = checked(&mut core, now);
        assert!(ended.is_some() && core.state.pause.is_none());
        // One a person resumes by hand waits for the person, whatever the usage says.
        core.state.manual_resume = true;
        hold(&mut core, LIMIT, "account", Hit { window: Some("seven_day".into()), until: Some(until), said: String::new() }, None);
        assert!(checked(&mut core, now).is_none() && core.state.pause.is_some());
    }

    #[test]
    fn a_check_with_no_answer_goes_on() {
        let mut core = Core::new();
        hold(&mut core, LIMIT, "harness", Hit { window: None, until: Some(1_000), said: String::new() }, None);
        let t = 1_000 + AFTER_RESET_MS + SPREAD_MS;
        assert_eq!(step(&mut core, t, "k"), Step::Check);
        assert_eq!(step(&mut core, t + 1_000, "k"), Step::Nothing);
        assert!(matches!(step(&mut core, t + CHECK_WAIT_MS, "k"), Step::GoOn(_)));
        assert!(core.state.pause.is_none());
    }

    #[test]
    fn manual_resume_never_ends_by_itself() {
        let mut core = Core::new();
        core.state.manual_resume = true;
        hold(&mut core, LIMIT, "harness", Hit { window: None, until: Some(1_000), said: String::new() }, None);
        assert_eq!(step(&mut core, 10_000_000_000, "k"), Step::Nothing);
        assert!(release(&mut core, "Resumed by you.").is_some());
    }

    #[test]
    fn the_stop_point_pauses_between_turns() {
        let mut core = Core::new();
        let later = Core::now() + 3_600_000;
        core.state.limits = vec![window("five_hour", 91.0, later)];
        assert!(open(&mut core));
        core.state.pause_at = Some(90);
        assert!(!open(&mut core));
        let p = core.state.pause.clone().unwrap_or_default();
        assert_eq!((p.reason.as_str(), p.until), (THRESHOLD, Some(later)));
        // It ends once the window is under the stop point again.
        if let Some(p) = core.state.pause.as_mut() {
            p.checking = Some(later);
        }
        core.state.limits = vec![window("five_hour", 2.0, later + 5 * 3_600_000)];
        assert!(checked(&mut core, later).is_some());
    }

    fn shell(status: ToolStatus) -> Body {
        use super::super::model::{ToolCall, ToolKind};
        Body::Tool(Box::new(ToolCall {
            tool_id: "t".into(),
            name: "Bash".into(),
            tool: ToolKind::Shell,
            title: "cargo build".into(),
            detail: None,
            input: serde_json::Value::Null,
            status,
            output: None,
            truncated: false,
            diffs: Vec::new(),
            todos: Vec::new(),
            task: None,
            exit_code: None,
            started: 0,
            ended: None,
            server: None,
            tool_short: None,
            skill: None,
            runner: None,
        }))
    }

    #[test]
    fn the_stop_point_stops_a_long_turn_at_its_next_step() {
        let mut core = Core::new();
        let later = Core::now() + 3_600_000;
        core.state.limits = vec![window("five_hour", 96.0, later)];
        core.state.status = Status::Running;
        core.state.turn_started = Some(Core::now() - 1_000);
        core.upsert("u1", None, Body::User { text: "port the parser".into(), images: 0, client_id: Some("u1".into()), origin: Some("human".into()), attachments: Vec::new(), reply_to: None });
        core.upsert("t1", None, shell(ToolStatus::Running));
        // No stop point: it works on.
        assert!(!stop_mid_turn(&mut core));
        core.state.pause_at = Some(95);
        // The build under way finishes first.
        assert!(!stop_mid_turn(&mut core));
        assert!(core.state.pause.is_none());
        core.upsert("t1", None, shell(ToolStatus::Done));
        // Between steps: the turn stops, and goes on from there once the limit resets.
        assert!(stop_mid_turn(&mut core));
        let p = core.state.pause.clone().unwrap_or_default();
        assert_eq!((p.reason.as_str(), p.until), (THRESHOLD, Some(later)));
        assert_eq!(p.cut.map(|c| c.text), Some("port the parser".to_owned()));
        let said = core.events().filter_map(|e| if let Body::Notice { text, .. } = &e.body { Some(text.clone()) } else { None }).next_back().unwrap_or_default();
        assert!(said.starts_with("Paused at the stop point: its Session limit is 96% used, the stop point is 95%. The step it was on finished"), "{said}");
        // Once paused, it isn't asked to stop again.
        assert!(!stop_mid_turn(&mut core));
    }

    #[test]
    fn resumed_past_the_stop_point_it_goes_on_until_the_window_starts_over() {
        let mut core = Core::new();
        let reset = Core::now() + 3_600_000;
        core.state.pause_at = Some(95);
        core.state.limits = vec![window("five_hour", 97.0, reset), window("seven_day", 40.0, reset + 86_400_000)];
        assert!(at_threshold(&mut core));
        // You resume it: the window past the stop point is left be (it doesn't stop again at once).
        waive_stop_point(&mut core);
        assert!(release(&mut core, "Resumed by you.").is_some());
        assert!(!at_threshold(&mut core));
        // A report a little off on the reset is the same window.
        core.state.limits = vec![window("five_hour", 98.0, reset + 60_000), window("seven_day", 40.0, reset + 86_400_000)];
        assert!(!at_threshold(&mut core));
        // Another window past it still stops it.
        core.state.limits = vec![window("five_hour", 98.0, reset), window("seven_day", 96.0, reset + 86_400_000)];
        assert!(at_threshold(&mut core));
        assert_eq!(core.state.pause.as_ref().and_then(|p| p.window.clone()).as_deref(), Some("seven_day"));
        release(&mut core, "Resumed by you.");
        // Once the window starts over (its reset hours later), the stop point holds for it again.
        core.state.limits = vec![window("five_hour", 96.0, reset + 5 * 3_600_000)];
        assert!(at_threshold(&mut core));
    }

    #[test]
    fn the_stop_point_leaves_a_turn_waiting_on_a_person_or_under_the_limit() {
        let mut core = Core::new();
        core.state.pause_at = Some(95);
        core.state.turn_started = Some(Core::now() - 1_000);
        core.state.limits = vec![window("five_hour", 99.0, Core::now() + 3_600_000)];
        // Waiting on an approval: it uses nothing meanwhile.
        core.state.status = Status::Waiting;
        assert!(!stop_mid_turn(&mut core));
        // Under the stop point: it works on.
        core.state.status = Status::Running;
        core.state.limits = vec![window("five_hour", 60.0, Core::now() + 3_600_000)];
        assert!(!stop_mid_turn(&mut core));
        // A window already past its reset says nothing about now.
        core.state.limits = vec![window("five_hour", 99.0, Core::now() - 1)];
        assert!(!stop_mid_turn(&mut core));
        assert!(core.state.pause.is_none());
    }

    #[test]
    fn another_accounts_or_models_limit_isnt_its_own() {
        let now = 1_000_000;
        let reset = now + 3_600_000;
        let own = [window("five_hour", 12.0, reset), window("seven_day", 39.0, reset + 86_400_000)];
        // The window starts over hours apart from the one reached: another account's.
        assert_eq!(shares(&own, "claude-opus-5-5", Some("five_hour"), Some(reset - 9_000_000), now), Some(false));
        // The same window, not used up as last said here: its harness is asked.
        assert_eq!(shares(&own, "claude-opus-5-5", Some("five_hour"), Some(reset + 1_000), now), None);
        // Used up here too: its own.
        let full = [window("five_hour", 100.0, reset)];
        assert_eq!(shares(&full, "claude-opus-5-5", Some("five_hour"), Some(reset), now), Some(true));
        // Nothing known of it here, or only from before it started over: asked.
        assert_eq!(shares(&[], "claude-opus-5-5", Some("five_hour"), Some(reset), now), None);
        assert_eq!(shares(&[window("five_hour", 100.0, now - 1)], "claude-opus-5-5", Some("five_hour"), Some(reset), now), None);
        // One model's window, to a session on another model; to one on it, as its limits say.
        let opus = [window("seven_day_opus", 100.0, reset)];
        assert_eq!(shares(&opus, "claude-fable-5-1", Some("seven_day_opus"), Some(reset), now), Some(false));
        assert_eq!(shares(&opus, "claude-opus-5-5", Some("seven_day_opus"), Some(reset), now), Some(true));
        assert_eq!(shares(&[], "claude-opus-5-5", Some("model:fable"), Some(reset), now), Some(false));
        // A model kiln can't tell: asked.
        assert_eq!(shares(&[], "default", Some("model:fable"), Some(reset), now), None);
        // No window said: any of its own used up.
        assert_eq!(shares(&full, "claude-opus-5-5", None, None, now), Some(true));
        assert_eq!(shares(&own, "claude-opus-5-5", None, None, now), None);
    }

    #[test]
    fn another_sessions_limit_holds_it_only_when_its_own_usage_says_so() {
        let reset = Core::now() + 3_600_000;
        let session = |used: f64, resets_at: i64| {
            let mut core = Core::new();
            core.state.account = Some("claude:max:abc".into());
            core.state.model = "claude-opus-5-5".into();
            core.state.limits = vec![window("five_hour", used, resets_at)];
            core
        };
        let hit = || Hit { window: Some("five_hour".into()), until: Some(reset), said: "You've hit your session limit".into() };
        let theirs = || Some("claude:max:abc".to_owned());
        // Another account's window: it goes on, and asks nothing.
        let mut other = session(12.0, reset + 9_000_000);
        account_hold(&mut other, hit(), theirs());
        assert!(other.state.pause.is_none() && other.limit_ask.is_none());
        // Used up here too, as last said: still asked first (what it knew may be out of date, or
        // another account's), and the answer holds it, as one held for the account.
        let now = Core::now();
        let mut full = session(100.0, reset);
        account_hold(&mut full, hit(), theirs());
        assert!(full.state.pause.is_none());
        assert_eq!(step(&mut full, now, "k"), Step::Check);
        assert!(checked(&mut full, now + 2_000).is_none());
        assert_eq!(full.state.pause.as_ref().map(|p| (p.by.as_str(), p.until)), Some(("account", Some(reset))));
        // Not used up as last said: its harness is asked first, once, and its answer settles it.
        let mut near = session(97.0, reset);
        account_hold(&mut near, hit(), theirs());
        assert!(near.state.pause.is_none());
        assert_eq!(step(&mut near, now, "k"), Step::Check);
        assert_eq!(step(&mut near, now + 1_000, "k"), Step::Nothing);
        near.state.limits = vec![window("five_hour", 100.0, reset)];
        assert!(checked(&mut near, now + 2_000).is_none());
        assert_eq!(near.state.pause.as_ref().map(|p| (p.by.as_str(), p.until)), Some(("account", Some(reset))));
        // The answer says there's room: it goes on.
        let mut room = session(97.0, reset);
        account_hold(&mut room, hit(), theirs());
        assert_eq!(step(&mut room, now, "k"), Step::Check);
        room.state.limits = vec![window("five_hour", 98.0, reset)];
        assert!(checked(&mut room, now + 2_000).is_none());
        assert!(room.state.pause.is_none() && room.limit_ask.is_none());
        // No answer: it's let be.
        let mut quiet = session(97.0, reset);
        account_hold(&mut quiet, hit(), theirs());
        assert_eq!(step(&mut quiet, now, "k"), Step::Check);
        assert_eq!(step(&mut quiet, now + CHECK_WAIT_MS, "k"), Step::Nothing);
        assert!(quiet.state.pause.is_none() && quiet.limit_ask.is_none());
        // On no account kiln knows: no other session's limit is its own.
        let mut none = session(100.0, reset);
        none.state.account = None;
        account_hold(&mut none, hit(), theirs());
        assert!(none.state.pause.is_none());
    }

    /// A limit of an account the session isn't signed in to now is never its own: the server's
    /// word for whose it was is kept to, at the hold and again when its harness answers (a sign-in
    /// seen meanwhile), and the session isn't paused, its own window used up or not.
    #[test]
    fn a_limit_of_another_account_never_holds_a_session_signed_in_elsewhere() {
        let reset = Core::now() + 3_600_000;
        let now = Core::now();
        let hit = || Hit { window: Some("five_hour".into()), until: Some(reset), said: "You've hit your session limit".into() };
        let on = |account: &str| {
            let mut core = Core::new();
            core.state.account = Some(account.into());
            core.state.model = "claude-opus-5-5".into();
            core.state.limits = vec![window("five_hour", 100.0, reset)];
            core
        };
        // Signed in as someone else, its own window full at the same moment: not held, not asked.
        let mut elsewhere = on("claude:claude-max:fff");
        account_hold(&mut elsewhere, hit(), Some("claude:claude-max:abc".into()));
        assert!(elsewhere.state.pause.is_none() && elsewhere.limit_ask.is_none());
        // The same account, its plan named in other words: its own.
        let mut same = on("claude:max:abc");
        account_hold(&mut same, hit(), Some("claude:claude-max:abc".into()));
        assert!(same.limit_ask.is_some());
        // Asked, then signed in as someone else before the answer: the answer holds nothing.
        let mut moved = on("claude:claude-max:abc");
        account_hold(&mut moved, hit(), Some("claude:claude-max:abc".into()));
        assert_eq!(step(&mut moved, now, "k"), Step::Check);
        moved.state.account = Some("claude:claude-max:fff".into());
        assert!(checked(&mut moved, now + 2_000).is_none());
        assert!(moved.state.pause.is_none() && moved.limit_ask.is_none());
        // Another harness's account is another account.
        let mut codex = on("codex:pro:abc");
        account_hold(&mut codex, hit(), Some("claude:claude-max:abc".into()));
        assert!(codex.state.pause.is_none() && codex.limit_ask.is_none());
        // A server that didn't say whose it was: the session's own usage decides, as before.
        let mut unsaid = on("claude:claude-max:fff");
        account_hold(&mut unsaid, hit(), None);
        assert_eq!(step(&mut unsaid, now, "k"), Step::Check);
        unsaid.state.limits = vec![window("five_hour", 40.0, reset)];
        assert!(checked(&mut unsaid, now + 2_000).is_none());
        assert!(unsaid.state.pause.is_none());
    }

    #[test]
    fn a_message_sent_while_paused_waits_in_the_queue() {
        let mut core = Core::new();
        let send = |id: &str| Command::Send { id: id.into(), text: "go on".into(), mode: Default::default(), attachments: vec![], overrides: None, reply_to: None };
        assert!(let_through(&mut core, send("m1")).is_some());
        hold(&mut core, LIMIT, "harness", Hit::default(), None);
        assert!(let_through(&mut core, send("m2")).is_none());
        assert!(let_through(&mut core, Command::Interrupt).is_some());
        assert_eq!(core.state.queue.iter().map(|q| q.id.as_str()).collect::<Vec<_>>(), vec!["m2"]);
    }

    #[test]
    fn a_checkpoint_keeps_the_folder_without_touching_it() -> anyhow::Result<()> {
        let dir = std::env::temp_dir().join(format!("kiln-checkpoint-test-{}", crate::util::new_id()));
        std::fs::create_dir_all(&dir)?;
        let git = |args: &[&str]| -> anyhow::Result<String> {
            let o = std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()?;
            anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
            Ok(String::from_utf8_lossy(&o.stdout).trim().to_owned())
        };
        git(&["init", "-q"])?;
        std::fs::write(dir.join("a.txt"), "one\n")?;
        std::fs::write(dir.join(".gitignore"), "target/\n")?;
        git(&["add", "-A"])?;
        git(&["commit", "-qm", "first"])?;
        let head = git(&["rev-parse", "HEAD"])?;
        // Mid-work: a changed file, a new one, one ignored.
        std::fs::write(dir.join("a.txt"), "two\n")?;
        std::fs::write(dir.join("b.txt"), "new\n")?;
        std::fs::create_dir_all(dir.join("target"))?;
        std::fs::write(dir.join("target/out"), "big\n")?;
        let status = git(&["status", "--porcelain"])?;
        let cwd = dir.to_string_lossy().into_owned();
        let (git_ref, commit) = snapshot(&cwd, "agent1", 42).ok_or_else(|| anyhow::anyhow!("no checkpoint"))?;
        assert_eq!(git_ref, "refs/kiln/checkpoints/agent1/42");
        assert_eq!(git(&["rev-parse", &git_ref])?, commit);
        assert_eq!(git(&["show", &format!("{commit}:a.txt")])?, "two");
        assert_eq!(git(&["show", &format!("{commit}:b.txt")])?, "new");
        assert!(git(&["show", &format!("{commit}:target/out")]).is_err());
        assert_eq!(git(&["rev-parse", &format!("{commit}^")])?, head);
        // The folder, its index and its branch are as they were.
        assert_eq!(git(&["status", "--porcelain"])?, status);
        assert_eq!(git(&["rev-parse", "HEAD"])?, head);
        // Not a repository: no checkpoint.
        let plain = std::env::temp_dir().join(format!("kiln-plain-{}", crate::util::new_id()));
        std::fs::create_dir_all(&plain)?;
        assert!(snapshot(&plain.to_string_lossy(), "agent1", 43).is_none());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&plain);
        Ok(())
    }

    #[test]
    fn a_continue_that_cant_go_now_waits_first_in_the_queue() {
        let mut core = Core::new();
        core.state.queue.push(QueueItem { id: "m2".into(), text: "then the tests".into(), at: 1, attachments: vec![], overrides: None, reply_to: None });
        continue_later(&mut core, CONTINUE);
        assert!(core.state.queue.first().is_some_and(|q| q.id.starts_with(KILN_ID) && q.text == CONTINUE));
        assert_eq!(core.state.queue.len(), 2);
    }

    #[test]
    fn spread_is_stable_and_bounded() {
        assert_eq!(spread("abc"), spread("abc"));
        assert!((0..SPREAD_MS).contains(&spread("abc")));
        assert!((0..SPREAD_MS).contains(&spread("")));
    }
}

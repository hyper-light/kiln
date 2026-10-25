//! The UI's commands to agent sessions, each under an id its client made (docs/wire.md, "The UI's
//! sockets"): taken once however often it comes, and each time answered, `ack` or `nack`. Only a
//! final answer is kept under its id, and a command sent again gets it. A parked one is interim:
//! sent again, it's answered parked, until its runner's answer comes. A transient one (`pending`,
//! or a nack's `again`) isn't kept, and a client sends the command again under its id after a
//! while: a message, which its inbox takes once, until it's taken. Agreed with T3 for CP-T3.4's composer outbox. Kept per
//! session, in the server's memory only: an id still waiting for its answer, or parked, until its
//! answer comes; of the answered, the newest [`KEEP`], none longer than [`KEEP_FOR`] after its
//! answer. A server that restarts lets go of them; a message's own inbox still takes it once.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use futures::channel::oneshot;
use parking_lot::Mutex;
use serde_json::json;

use super::registry::Sent;

/// The answered ids a session keeps, at the most.
pub const KEEP: usize = 1024;
/// How long an answered id is kept, from its answer.
pub const KEEP_FOR: Duration = Duration::from_secs(600);
/// How long an id waits for its answer at the most: its command's is in by then (a parked one's
/// within [`PARK_FOR`](crate::wire::commands::PARK_FOR)), and one somehow not is let go of.
const OPEN_FOR: Duration = crate::wire::commands::PARK_FOR.saturating_add(KEEP_FOR);

/// What a command is, for what its answers mean: a message (`send`, `skill_run`), and whether it
/// goes to an inbox that takes it once under its id (a message with one, to a runner with inboxes;
/// another is taken unchecked).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Kind {
    pub message: bool,
    pub deduped: bool,
}

/// What an answer is to whoever sent the command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stand {
    /// What became of it, for good: kept under its id, and its client stops.
    Final,
    /// Parked for a runner that's away: kept until its runner's answer comes, which takes its place.
    Interim,
    /// It didn't get through this time and can be sent again under its id (`again`): not kept.
    Again,
}

/// What `sent` is to the client of a command of `kind`.
pub fn stand(sent: &Sent, kind: Kind) -> Stand {
    use super::inbox::Refusal as Inbox;
    use crate::wire::channel::Refusal as Code;
    match sent {
        Sent::Taken | Sent::Accepted(_) | Sent::NoSession => Stand::Final,
        Sent::Parked => Stand::Interim,
        // Its inbox didn't say in time (`pending`): sent again, the inbox answers a repeat if it
        // took it, and takes it if it was lost. Else it went unchecked (`unchecked`), to a runner
        // where nothing takes it once: sent again, it would be a second prompt.
        Sent::Ok if kind.deduped => Stand::Again,
        Sent::Ok => Stand::Final,
        Sent::Refused(Inbox::Moved { .. } | Inbox::Behind { .. } | Inbox::Full { .. } | Inbox::Unavailable { .. } | Inbox::StaleEpoch { .. }) => Stand::Again,
        Sent::Refused(_) => Stand::Final,
        Sent::Rejected { code: Code::StaleEpoch, .. } if kind.message => Stand::Again,
        Sent::Rejected { .. } => Stand::Final,
        // It may have been applied: only an inbox can say it was, when it comes again.
        Sent::Unanswered(_) if kind.deduped => Stand::Again,
        Sent::Unanswered(_) => Stand::Final,
        // A message that couldn't go yet goes once its runner's back. Anything else would surprise
        // done later.
        Sent::Unreachable if kind.message => Stand::Again,
        Sent::Unreachable => Stand::Final,
    }
}

/// What an id is as a command comes under it.
#[derive(Debug)]
pub enum Taken {
    /// New: the caller sends it on, and says what becomes of it ([`Asked::answered`]). The
    /// receiver gets the first of that.
    New(oneshot::Receiver<Sent>),
    /// It came before and hasn't been answered yet: the receiver gets its answer when it comes.
    Waiting(oneshot::Receiver<Sent>),
    /// It came before: what became of it, final or interim (parked).
    Answered(Sent),
}

/// An id's command: what it is (as it was when it first came), where it stands, and since when.
#[derive(Debug)]
struct Entry {
    kind: Kind,
    /// When it came, or was last answered.
    at: Instant,
    state: State,
}

#[derive(Debug)]
enum State {
    Waiting(Vec<oneshot::Sender<Sent>>),
    Answered(Sent),
}

impl Entry {
    /// Waiting for its answer, or parked: let go of, what came again under its id would go on again.
    fn open(&self) -> bool {
        matches!(self.state, State::Waiting(_) | State::Answered(Sent::Parked))
    }
}

#[derive(Debug, Default)]
struct Log {
    entries: HashMap<String, Entry>,
    /// The ids, oldest first.
    order: VecDeque<String>,
}

impl Log {
    /// Lets go of the answered ids past the bound, oldest first, and those answered long enough
    /// ago. One still open stays until it's answered, unless it waited longer than any command
    /// does ([`OPEN_FOR`]): its receivers then hear it was let go of.
    fn tidy(&mut self, now: Instant) {
        let mut over = self.order.len().saturating_sub(KEEP);
        let entries = &mut self.entries;
        self.order.retain(|key| {
            let go = entries.get(key).is_none_or(|e| {
                let age = now.saturating_duration_since(e.at);
                if e.open() { age >= OPEN_FOR } else { over > 0 || age >= KEEP_FOR }
            });
            if go {
                over = over.saturating_sub(1);
                entries.remove(key);
            }
            !go
        });
    }
}

/// Every session's ids (the registry's).
#[derive(Debug, Default)]
pub struct Asked {
    sessions: Mutex<HashMap<String, Log>>,
}

impl Asked {
    /// Command `id`, of `kind`, came for session `session` from `scope` (whose ids they are: a
    /// caller's), at `now`: where it stands, and what it is (as it was when it first came, for what
    /// its answers mean).
    pub fn take(&self, session: &str, scope: &str, id: &str, kind: Kind, now: Instant) -> (Kind, Taken) {
        let key = format!("{scope}\u{1f}{id}");
        let mut g = self.sessions.lock();
        let log = g.entry(session.to_owned()).or_default();
        log.tidy(now);
        let (tx, rx) = oneshot::channel();
        match log.entries.get_mut(&key) {
            Some(e) => match &mut e.state {
                State::Answered(sent) => (e.kind, Taken::Answered(sent.clone())),
                State::Waiting(waiting) => {
                    waiting.push(tx);
                    (e.kind, Taken::Waiting(rx))
                }
            },
            None => {
                log.entries.insert(key.clone(), Entry { kind, at: now, state: State::Waiting(vec![tx]) });
                log.order.push_back(key);
                log.tidy(now);
                (kind, Taken::New(rx))
            }
        }
    }

    /// What became of command `id` for session `session` from `scope`, at `now`: said to each
    /// socket or call waiting for it, and kept as its [`stand`] says, from `now`. A final or an
    /// interim answer is what the command gets when it comes again (a final one in an interim's
    /// place); a transient one isn't kept, so it goes on again.
    pub fn answered(&self, session: &str, scope: &str, id: &str, sent: &Sent, now: Instant) {
        let key = format!("{scope}\u{1f}{id}");
        let waiting = {
            let mut g = self.sessions.lock();
            let Some(log) = g.get_mut(session) else { return };
            let Some(entry) = log.entries.get_mut(&key) else { return };
            if stand(sent, entry.kind) == Stand::Again {
                log.order.retain(|k| *k != key);
                match log.entries.remove(&key) {
                    Some(Entry { state: State::Waiting(waiting), .. }) => waiting,
                    _ => Vec::new(),
                }
            } else {
                entry.at = now;
                match std::mem::replace(&mut entry.state, State::Answered(sent.clone())) {
                    State::Waiting(waiting) => waiting,
                    State::Answered(_) => Vec::new(),
                }
            }
        };
        for w in waiting {
            let _ = w.send(sent.clone());
        }
    }

    /// Session `session` ended: its ids go with it.
    pub fn forget(&self, session: &str) {
        self.sessions.lock().remove(session);
    }
}

/// The frame that answers command `id` (of `kind`) on a viewer's socket: `ack`, with what became of
/// it, or `nack`, with its kind (`code`), why, and whether it may be sent again (`again`).
pub fn frame(id: &str, sent: &Sent, kind: Kind) -> String {
    let again = stand(sent, kind) == Stand::Again;
    let ack = |outcome: &str| json!({"t": "ack", "id": id, "outcome": outcome});
    let nack = |code: &str, why: String| json!({"t": "nack", "id": id, "code": code, "why": why, "again": again});
    let v = match sent {
        Sent::Taken => ack("taken"),
        Sent::Parked => ack("parked"),
        Sent::Ok if kind.deduped => ack("pending"),
        Sent::Ok => ack("unchecked"),
        Sent::Accepted(a) => json!({"t": "ack", "id": id, "outcome": "accepted", "accepted": a}),
        Sent::Refused(r) => {
            let code = serde_json::to_value(r).ok().and_then(|v| v.get("refused").and_then(serde_json::Value::as_str).map(str::to_owned)).unwrap_or_else(|| "refused".to_owned());
            json!({"t": "nack", "id": id, "code": code, "why": r.to_string(), "again": again, "refused": r})
        }
        Sent::Rejected { code, .. } => nack(&serde_json::to_value(code).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_else(|| "refused".to_owned()), why(sent)),
        Sent::Unanswered(_) => nack("unanswered", why(sent)),
        Sent::NoSession => nack("no_session", why(sent)),
        Sent::Unreachable => nack("unreachable", why(sent)),
    };
    v.to_string()
}

/// The frame that refuses command `id` (none: what couldn't be read far enough to have one) before
/// it's sent on: `nack`, its kind (`code`) and why. Never to be sent again as it is.
pub fn nack(id: Option<&str>, code: &str, why: &str) -> String {
    json!({"t": "nack", "id": id, "code": code, "why": why, "again": false}).to_string()
}

/// The frame that answers what a viewer sent that couldn't be read as a command: `nack`,
/// `malformed`, under its id when that much could be read.
pub fn unreadable(id: Option<&str>, why: &str) -> String {
    nack(id, "malformed", &format!("not a command: {why}"))
}

fn why(sent: &Sent) -> String {
    sent.failure().map(|(_, why)| why).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMAND: Kind = Kind { message: false, deduped: false };
    const MESSAGE: Kind = Kind { message: true, deduped: true };

    #[test]
    fn a_command_is_taken_once_and_each_that_sent_it_gets_its_answer() -> anyhow::Result<()> {
        let asked = Asked::default();
        let now = Instant::now();
        let (_, Taken::New(mut first)) = asked.take("s1", "u1", "c1", COMMAND, now) else { anyhow::bail!("new the first time") };
        // Sent again before its answer (its socket dropped, and the client's next one sent it).
        let (_, Taken::Waiting(mut again)) = asked.take("s1", "u1", "c1", COMMAND, now) else { anyhow::bail!("waiting the second time") };
        anyhow::ensure!(first.try_recv()?.is_none(), "nothing yet");
        asked.answered("s1", "u1", "c1", &Sent::Taken, now);
        anyhow::ensure!(first.try_recv()? == Some(Sent::Taken) && again.try_recv()? == Some(Sent::Taken), "each gets the answer");
        // And once answered, what became of it, sent nowhere again.
        anyhow::ensure!(matches!(asked.take("s1", "u1", "c1", COMMAND, now), (_, Taken::Answered(Sent::Taken))));
        // The same id from another caller, or for another session, is another command.
        anyhow::ensure!(matches!(asked.take("s1", "u2", "c1", COMMAND, now), (_, Taken::New(_))));
        anyhow::ensure!(matches!(asked.take("s2", "u1", "c1", COMMAND, now), (_, Taken::New(_))));
        Ok(())
    }

    #[test]
    fn only_a_final_answer_is_kept_and_a_parked_one_until_its_final_comes() -> anyhow::Result<()> {
        let asked = Asked::default();
        let now = Instant::now();
        // A message that couldn't go (its runner away): sent again, it goes on again.
        let (_, Taken::New(mut m)) = asked.take("s1", "u1", "m1", MESSAGE, now) else { anyhow::bail!("new") };
        asked.answered("s1", "u1", "m1", &Sent::Unreachable, now);
        anyhow::ensure!(m.try_recv()? == Some(Sent::Unreachable), "its sender hears it");
        anyhow::ensure!(matches!(asked.take("s1", "u1", "m1", MESSAGE, now), (_, Taken::New(_))), "and it goes on again");
        // Its inbox didn't say in time: likewise.
        asked.answered("s1", "u1", "m1", &Sent::Ok, now);
        anyhow::ensure!(matches!(asked.take("s1", "u1", "m1", MESSAGE, now), (_, Taken::New(_))), "on its way isn't taken");
        // Parked: sent again, parked again, until its runner's answer takes its place.
        asked.answered("s1", "u1", "m1", &Sent::Parked, now);
        anyhow::ensure!(matches!(asked.take("s1", "u1", "m1", MESSAGE, now), (_, Taken::Answered(Sent::Parked))));
        asked.answered("s1", "u1", "m1", &Sent::Taken, now);
        anyhow::ensure!(matches!(asked.take("s1", "u1", "m1", MESSAGE, now), (_, Taken::Answered(Sent::Taken))));
        // A model to change to, refused while its runner is away, is final: done later, it would
        // surprise.
        let (_, Taken::New(_)) = asked.take("s1", "u1", "c2", COMMAND, now) else { anyhow::bail!("new") };
        asked.answered("s1", "u1", "c2", &Sent::Unreachable, now);
        anyhow::ensure!(matches!(asked.take("s1", "u1", "c2", COMMAND, now), (_, Taken::Answered(Sent::Unreachable))));
        Ok(())
    }

    #[test]
    fn what_a_command_is_stays_as_it_was_when_it_came() -> anyhow::Result<()> {
        // A message to a runner with inboxes, sent again once its runner seems to have none (it's
        // away, say): its answers mean what they meant when it came.
        let asked = Asked::default();
        let now = Instant::now();
        let (_, Taken::New(_)) = asked.take("s1", "u1", "m1", MESSAGE, now) else { anyhow::bail!("new") };
        let unchecked = Kind { message: true, deduped: false };
        let (kind, Taken::Waiting(_)) = asked.take("s1", "u1", "m1", unchecked, now) else { anyhow::bail!("waiting") };
        anyhow::ensure!(kind == MESSAGE, "{kind:?}");
        // Its inbox didn't say in time: not kept, for a message to an inbox.
        asked.answered("s1", "u1", "m1", &Sent::Ok, now);
        anyhow::ensure!(matches!(asked.take("s1", "u1", "m1", unchecked, now), (k, Taken::New(_)) if k == unchecked), "it goes on again, as what it is now");
        Ok(())
    }

    #[test]
    fn answered_ids_past_the_bound_or_kept_long_enough_are_let_go_of_open_ones_once_answered() -> anyhow::Result<()> {
        let asked = Asked::default();
        let now = Instant::now();
        // Parked for a runner that's away: kept past the bound and past the time an answered id is
        // kept, until its runner's answer comes; that's kept as long from then.
        asked.take("s1", "u1", "p", COMMAND, now);
        asked.answered("s1", "u1", "p", &Sent::Parked, now);
        for n in 0..=KEEP {
            let id = format!("c{n}");
            asked.take("s1", "u1", &id, COMMAND, now);
            asked.answered("s1", "u1", &id, &Sent::Taken, now);
        }
        anyhow::ensure!(matches!(asked.take("s1", "u1", "c0", COMMAND, now), (_, Taken::New(_))), "the oldest went past the bound");
        anyhow::ensure!(matches!(asked.take("s1", "u1", &format!("c{KEEP}"), COMMAND, now), (_, Taken::Answered(_))), "the newest is kept");
        let later = now + KEEP_FOR;
        anyhow::ensure!(matches!(asked.take("s1", "u1", &format!("c{KEEP}"), COMMAND, later), (_, Taken::New(_))), "kept long enough");
        anyhow::ensure!(matches!(asked.take("s1", "u1", "p", COMMAND, later), (_, Taken::Answered(Sent::Parked))), "still parked");
        asked.answered("s1", "u1", "p", &Sent::Taken, later);
        anyhow::ensure!(matches!(asked.take("s1", "u1", "p", COMMAND, later + KEEP_FOR / 2), (_, Taken::Answered(Sent::Taken))), "kept from its answer");
        anyhow::ensure!(matches!(asked.take("s1", "u1", "p", COMMAND, later + KEEP_FOR), (_, Taken::New(_))));
        // One waiting for its answer is kept until it comes, or until it waited longer than any.
        let asked = Asked::default();
        let (_, Taken::New(mut waiting)) = asked.take("s1", "u1", "w", COMMAND, now) else { anyhow::bail!("new") };
        anyhow::ensure!(matches!(asked.take("s1", "u1", "w", COMMAND, later), (_, Taken::Waiting(_))));
        asked.take("s1", "u1", "x", COMMAND, now + OPEN_FOR);
        anyhow::ensure!(waiting.try_recv().is_err(), "let go of, its answer never comes");
        Ok(())
    }

    #[test]
    fn each_answer_is_an_ack_or_a_nack_saying_how_and_whether_again() -> anyhow::Result<()> {
        use super::super::inbox::Refusal as Inbox;
        use crate::wire::channel::Refusal;
        let frame = |s: &Sent, k: Kind| -> anyhow::Result<serde_json::Value> { Ok(serde_json::from_str(&frame("c1", s, k))?) };
        let ack = frame(&Sent::Taken, COMMAND)?;
        anyhow::ensure!(ack == json!({"t": "ack", "id": "c1", "outcome": "taken"}), "{ack}");
        anyhow::ensure!(frame(&Sent::Parked, MESSAGE)?.get("outcome") == Some(&json!("parked")));
        // On its way: to a runner where nothing takes it once, final; to an inbox that hasn't
        // said yet, to send again.
        let unchecked = Kind { message: true, deduped: false };
        for k in [COMMAND, unchecked] {
            anyhow::ensure!(frame(&Sent::Ok, k)? == json!({"t": "ack", "id": "c1", "outcome": "unchecked"}) && stand(&Sent::Ok, k) == Stand::Final, "{k:?}");
        }
        anyhow::ensure!(frame(&Sent::Ok, MESSAGE)? == json!({"t": "ack", "id": "c1", "outcome": "pending"}) && stand(&Sent::Ok, MESSAGE) == Stand::Again);
        let unknown = frame(&Sent::Rejected { code: Refusal::Unknown, why: "no session s1 here".into() }, COMMAND)?;
        anyhow::ensure!(unknown.get("code") == Some(&json!("unknown")) && unknown.get("again") == Some(&json!(false)), "{unknown}");
        anyhow::ensure!(unknown.get("why").and_then(serde_json::Value::as_str).is_some_and(|w| w.contains("no session s1 here")), "{unknown}");
        let full = frame(&Sent::Refused(Inbox::Full { session: "s1".into(), limit: 3 }), MESSAGE)?;
        anyhow::ensure!(full.get("code") == Some(&json!("full")) && full.pointer("/refused/limit") == Some(&json!(3)) && full.get("again") == Some(&json!(true)), "{full}");
        let reused = frame(&Sent::Refused(Inbox::Reused { session: "s1".into(), message: "m1".into() }), MESSAGE)?;
        anyhow::ensure!(reused.get("code") == Some(&json!("reused")) && reused.get("again") == Some(&json!(false)), "{reused}");
        for (s, k, code, again) in [
            (Sent::Unanswered("cut".into()), COMMAND, "unanswered", false),
            (Sent::Unanswered("cut".into()), MESSAGE, "unanswered", true),
            (Sent::NoSession, MESSAGE, "no_session", false),
            (Sent::Unreachable, COMMAND, "unreachable", false),
            (Sent::Unreachable, MESSAGE, "unreachable", true),
        ] {
            let f = frame(&s, k)?;
            anyhow::ensure!(f.get("code") == Some(&json!(code)) && f.get("again") == Some(&json!(again)) && f.get("why").and_then(serde_json::Value::as_str).is_some_and(|w| !w.is_empty()), "{f}");
        }
        let bad: serde_json::Value = serde_json::from_str(&unreadable(None, "expected value"))?;
        anyhow::ensure!(bad.get("code") == Some(&json!("malformed")) && bad.get("id") == Some(&serde_json::Value::Null) && bad.get("again") == Some(&json!(false)), "{bad}");
        Ok(())
    }
}

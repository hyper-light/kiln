//! What waits in an agent's queue, as the server sees it, and the messages kiln keeps there itself
//! while the agent's turn goes on: a parent's news of its children (`super::told`), a goal stack's
//! word to its agent (`crate::goals`).
//!
//! What's queued behind a turn all lands when the turn ends, however long it ran. So kiln keeps one
//! message of each of its own waiting at a time ([`Kept`]): what it has to say next goes as an edit
//! of the one waiting (`Command::QueueEdit`), or not at all when it says the same, never as another
//! message behind it. The server sees a queue as the agent's machine last said (`Info::queue`, each
//! message's id and a digest of its words). An edit that reaches the machine just after the turn
//! ended, the message gone with it, does nothing there: so what an edit said goes again, in a
//! message of its own, unless the queue was seen holding it ([`Kept::lost`]).
//!
//! kiln's own messages carry what they are in their ids ([`made`]: news, a goal stack's word), so
//! a server that started since finds the one waiting and keeps that one up to date ([`Kept::found`])
//! rather than queue another. A message one agent sends another carries its sender in its id
//! ([`from_agent`]), so the receiver's `agents_wait` can hand it over rather than leave it behind
//! the turn.

use std::collections::{HashSet, VecDeque};
use std::hash::{Hash as _, Hasher as _};
use std::sync::LazyLock;

use parking_lot::Mutex;

use super::model::Status;
use super::session::Info;

/// How long a message kiln just queued may not show in its agent's queue yet (its machine says so a
/// moment later): until then it's taken to wait there.
pub const SHOW_MS: i64 = 10_000;

/// How the id of a message one agent sent another begins: `from-agent-<sender>-<an id of its own>`
/// (not `kiln-`: it isn't kiln's own, `super::pause::sent_from_queue`).
const FROM_AGENT: &str = "from-agent-";
/// How the ids of kiln's news of an agent's children begin (`super::told`).
pub const NEWS: &str = "news-";
/// How the ids of a goal stack's words begin (`crate::goals`).
pub const GOAL: &str = "goal-";
/// The most ids of its own kiln remembers making ([`made`]).
const MADE_MAX: usize = 4096;

/// A message waiting in a session's queue, as the server keeps it: its id, a digest of its words,
/// and how long they are (bytes).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Queued {
    pub id: String,
    pub digest: u64,
    pub len: usize,
}

impl Queued {
    pub fn of(id: &str, text: &str) -> Self {
        Self { id: id.to_owned(), digest: digest(text), len: text.len() }
    }
}

/// The ids of its own messages this server made, the latest [`MADE_MAX`] (in order, and to look up).
static MADE: LazyLock<Mutex<(VecDeque<String>, HashSet<String>)>> = LazyLock::new(Default::default);

/// A new id for a message of kiln's own of kind `kind` ([`NEWS`], [`GOAL`]), remembered as this
/// server's.
pub fn made(kind: &str) -> String {
    let id = format!("{kind}{}", crate::util::new_id());
    let mut g = MADE.lock();
    let (order, ids) = &mut *g;
    if order.len() >= MADE_MAX
        && let Some(old) = order.pop_front()
    {
        ids.remove(&old);
    }
    order.push_back(id.clone());
    ids.insert(id.clone());
    id
}

/// A message of kind `kind` that kiln queued before this server started (one it made itself isn't).
pub fn from_before(id: &str, kind: &str) -> bool {
    id.starts_with(kind) && !MADE.lock().1.contains(id)
}

/// A digest of a message's words: whether what waits says what kiln last had it say.
pub fn digest(text: &str) -> u64 {
    let mut h = std::hash::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// A new id for a message agent `sender` sends another.
pub fn from_agent(sender: &str) -> String {
    format!("{FROM_AGENT}{sender}-{}", crate::util::new_id())
}

/// The agent that sent the message with this id, when another agent sent it.
pub fn sender_of(id: &str) -> Option<&str> {
    id.strip_prefix(FROM_AGENT)?.split_once('-').map(|(sender, _)| sender).filter(|s| !s.is_empty())
}

/// A message kiln queued for an agent and keeps up to date while it waits there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kept {
    pub id: String,
    /// What kiln last had it say: what it was sent with, or an edit since.
    pub text: String,
    /// What reaches the agent should the message go now: what it was sent with (a message sent
    /// isn't lost), or what its queue was last seen holding.
    pub held: String,
    /// Its queue was seen holding it.
    seen: bool,
    /// Its agent was free when it was sent, so it took it at once: it never waited.
    direct: bool,
    /// When it was sent (unix ms).
    at: i64,
}

/// Where a kept message is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Now {
    /// In its agent's queue, or sent a moment ago and not shown there yet.
    Waiting,
    /// Not there (any more): its agent took it, or someone took it out.
    Gone,
}

impl Kept {
    /// Message `id` was just sent to `to`, saying `text`.
    pub fn sent(id: String, text: String, to: &Info, now: i64) -> Self {
        Self { id, held: text.clone(), text, seen: false, direct: free(to), at: now }
    }

    /// Message `id`, found waiting at `now` (queued before this server started), saying `holds`
    /// (empty: words not known).
    pub fn found(id: String, holds: String, now: i64) -> Self {
        Self { id, held: holds.clone(), text: holds, seen: true, direct: false, at: now }
    }

    /// Where it is, by the agent's queue as `info` has it (none: the agent is gone), at `now`; what
    /// the queue holds of it is noted.
    pub fn look(&mut self, info: Option<&Info>, now: i64) -> Now {
        let Some(info) = info else { return Now::Gone };
        match info.queue.iter().find(|q| q.id == self.id) {
            Some(q) => {
                self.seen = true;
                if q.digest == digest(&self.text) {
                    self.held.clone_from(&self.text);
                }
                Now::Waiting
            }
            None if !self.seen && !self.direct && now.saturating_sub(self.at) < SHOW_MS => Now::Waiting,
            None => Now::Gone,
        }
    }

    /// What it last said may not have reached the agent: its queue wasn't seen holding that.
    pub fn lost(&self) -> bool {
        self.text != self.held
    }

    /// When it was sent (unix ms).
    pub fn at(&self) -> i64 {
        self.at
    }
}

/// Takes a message at once: free (stopped counts: a message starts it again), nothing queued,
/// nothing waiting on a person, no usage limit holding it.
fn free(info: &Info) -> bool {
    matches!(info.status, Status::Idle | Status::Exited) && info.queued == 0 && info.approvals.is_empty() && info.pause.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(status: Status, queue: Vec<Queued>) -> Info {
        Info { id: "a1".into(), status, queued: queue.len(), queue, ..Info::default() }
    }

    #[test]
    fn a_kept_message_waits_while_its_queue_holds_it_and_knows_what_it_held() {
        let now = 1_000_000;
        let busy = agent(Status::Running, vec![]);
        // Not shown yet: taken to wait, a moment; never shown, it went.
        let mut never = Kept::sent("m0".into(), "one child settled".into(), &busy, now);
        assert_eq!(never.look(Some(&busy), now + 1_000), Now::Waiting);
        assert_eq!(never.look(Some(&busy), now + SHOW_MS), Now::Gone);
        // Shown, then edited: what it holds is what it was sent with until the queue holds the edit.
        let mut m = Kept::sent("m1".into(), "two children settled".into(), &busy, now);
        assert_eq!(m.look(Some(&agent(Status::Running, vec![Queued::of("m1", "two children settled")])), now + 2_000), Now::Waiting);
        m.text = "three children settled".into();
        assert!(m.lost(), "the edit isn't seen held yet");
        assert_eq!(m.look(Some(&agent(Status::Running, vec![Queued::of("m1", "two children settled")])), now + 3_000), Now::Waiting);
        assert!(m.lost());
        assert_eq!(m.look(Some(&agent(Status::Running, vec![Queued::of("m1", "three children settled")])), now + 4_000), Now::Waiting);
        assert!(!m.lost(), "held");
        // Gone once seen: at once, whatever the time.
        assert_eq!(m.look(Some(&busy), now + 4_500), Now::Gone);
        assert_eq!(m.look(None, now + 4_500), Now::Gone, "its agent is gone");
        // Sent to a free agent, it was taken at once: gone as soon as it isn't there.
        let mut direct = Kept::sent("m2".into(), "hi".into(), &agent(Status::Idle, vec![]), now);
        assert_eq!(direct.look(Some(&busy), now + 1), Now::Gone);
        // Paused at a usage limit, it's queued: it waits.
        let paused = Info { pause: Some(crate::harness::model::Pause { reason: "limit".into(), ..Default::default() }), ..agent(Status::Idle, vec![]) };
        let mut held = Kept::sent("m3".into(), "hi".into(), &paused, now);
        assert_eq!(held.look(Some(&paused), now + 1), Now::Waiting);
    }

    #[test]
    fn a_message_from_an_agent_says_who_sent_it_in_its_id() {
        let id = from_agent("01a15c10306176b4a096f6c8d3100c54");
        assert_eq!(sender_of(&id), Some("01a15c10306176b4a096f6c8d3100c54"));
        assert!(!id.starts_with(crate::harness::pause::KILN_ID), "not kiln's own: {id}");
        assert_ne!(from_agent("a"), from_agent("a"), "each its own");
        assert_eq!(sender_of("01a16251c49a75fba41b1fae971ba8a9"), None, "a person's");
        assert_eq!(sender_of("kiln-keep-1792650891215"), None, "kiln's own");
        assert_eq!(sender_of("from-agent--x"), None);
        assert_eq!(digest("a"), digest("a"));
        assert_ne!(digest("a"), digest("b"));
        assert_eq!(Queued::of("m", "four").len, 4);
    }

    /// kiln knows its own messages from those a server before it queued: one it made isn't from
    /// before; one of the kind it didn't make is.
    #[test]
    fn kiln_tells_its_own_messages_from_those_queued_before_it_started() {
        let news = made(NEWS);
        assert!(news.starts_with(NEWS) && !from_before(&news, NEWS));
        assert!(from_before("news-01a16251c49a75fba41b1fae971ba8a9", NEWS), "from before it started");
        assert!(!from_before("news-01a16251c49a75fba41b1fae971ba8a9", GOAL), "another kind");
        assert!(!from_before("01a16251c49a75fba41b1fae971ba8a9", NEWS), "a person's");
        let found = Kept::found("goal-1".into(), String::new(), 5);
        assert!(!found.lost() && found.text.is_empty());
    }
}

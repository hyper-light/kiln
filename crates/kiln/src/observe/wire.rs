//! What the server and a runner say of the observe group (`docs/observe.md` §5), inside
//! `ServerMsg::Observe` and `AgentMsg::Observe` on a lane of their own: a [`Msg`] as JSON, so either
//! side can add fields, and beside it the bytes it carries (a hyper-raft message's, or a write's
//! entry). A snapshot's files travel over HTTP instead (`GET /api/observe/v1/snapshot/...`), with
//! the token [`Msg::Member`] gives a machine. A side that doesn't know a kind skips it.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Bytes a frame carries, at the most: a bigger payload goes in parts (kiln's sync lane's part).
pub const PART: usize = 512 << 10;
/// The largest payload put back together: a write's entry (`kiln_obs::group::ENTRY_BYTES` and its
/// command), an append of one such entry, or an image naming every file a store keeps, with room
/// to spare; a message claiming more parts than this is refused, not allocated.
pub const MAX_PAYLOAD: usize = 16 << 20;
const MAX_PARTS: u32 = MAX_PAYLOAD.div_ceil(PART) as u32;
/// How long the parts of one payload wait for the rest: past it the sender sends again.
const PARTS_WAIT: Duration = Duration::from_secs(60);
/// Payloads being put back together at once, from one side.
const JOINING: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Msg {
    /// A hyper-raft message, its bytes beside: whole (`parts` 0) or part `part` of `parts` of
    /// message `id`.
    Raft {
        #[serde(default)]
        id: u64,
        #[serde(default)]
        part: u32,
        #[serde(default)]
        parts: u32,
    },
    /// Runner → server: a write to propose, its entry's bytes beside (`kiln_obs::command`), whole
    /// or in parts as a raft message's.
    Write {
        sender: u64,
        seq: u64,
        #[serde(default)]
        id: u64,
        #[serde(default)]
        part: u32,
        #[serde(default)]
        parts: u32,
    },
    /// Server → runner: what a write it sent came to.
    Written { sender: u64, seq: u64, outcome: Outcome },
    /// Server → runner: this machine's member of the group, and the token its snapshot fetches
    /// carry; `group` names the group, so that a runner holding another's replica starts again.
    /// Sent when the runner connects.
    Member {
        member: u64,
        token: String,
        #[serde(default)]
        group: String,
    },
    /// Server → runner: another runner process of this machine replicates now: let go.
    Release,
    /// Server → runner: take the full digest of the replica (every sample and record) and say it
    /// in the next status: what an operator, or a test, checks replicas by.
    Verify,
    /// Runner → server: how this machine's replica is doing.
    Status(Status),
    /// Server → runner: the scrape targets this machine reads (`super::scrape`), all of them each
    /// time, sent when it connects and when they change.
    Scrapes { targets: Vec<(String, super::scrape::Target)> },
    /// Server → runner: the log sources this machine reads (`super::collect`), as scrape targets.
    LogSources { sources: Vec<(String, super::collect::Source)> },
    /// Server → runner: the log pipelines every node applies (`super::pipeline`), in order.
    Pipelines { pipelines: Vec<super::pipeline::PipelineDef> },
    /// Server → runner: the log indexes every node routes records by (`super::indexes`), in
    /// order, and the parts of those over their daily quota now.
    Indexes {
        indexes: Vec<super::indexes::Kept>,
        limited: Vec<u16>,
    },
    /// Server → runner: the scanning groups every node runs (`super::scanner`), in order, and the
    /// key their hashes are made with.
    Scanner {
        groups: Vec<super::scanner::GroupDef>,
        key: String,
    },
    /// Server → runner: the synthetic tests this machine runs (`super::synthetics`), as scrape
    /// targets; and one of them to run now.
    Synthetics { tests: Vec<(String, super::synthetics::Test)> },
    RunSynthetic { name: String },
    /// Server → runner: the DogStatsD listeners this machine opens (`super::statsd`), as scrape
    /// targets.
    Statsd { listeners: Vec<(String, super::statsd::Listener)> },
    /// Runner → server: a profiler's upload its Agent took (`super::profiles::pack`), whole or in
    /// parts as a raft message's.
    Profile {
        #[serde(default)]
        id: u64,
        #[serde(default)]
        part: u32,
        #[serde(default)]
        parts: u32,
    },
}

/// What a write came to, as its sender needs it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Outcome {
    /// Applied now, or before (sent twice).
    pub applied: bool,
    pub duplicate: bool,
    /// Its batch stopped at an item that does not decode.
    pub failed: Option<String>,
    /// Series the group does not know: the sender defines them again.
    pub unknown: Vec<u64>,
    /// Definitions refused (a fingerprint not its key's), and series past the group's bound.
    pub refused: Vec<u64>,
    pub over_limit: Vec<u64>,
    /// Samples and log records too far ahead of the write's time, dropped
    /// (`kiln_obs::store::AHEAD_MS`).
    #[serde(default)]
    pub ahead: u64,
}

impl Outcome {
    pub fn of(w: &kiln_obs::store::Written) -> Self {
        use kiln_obs::store::Written;
        match w {
            Written::Applied(a) => Self { applied: true, unknown: a.unknown.clone(), refused: a.refused.clone(), over_limit: a.over_limit.clone(), ahead: a.counts.ahead + a.counts.logs_ahead, ..Self::default() },
            Written::Duplicate => Self { applied: true, duplicate: true, ..Self::default() },
            Written::Failed(e) => Self { failed: Some(e.to_string()), ..Self::default() },
        }
    }
}

/// How a machine's replica is doing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Status {
    pub member: u64,
    /// Through where it applied the log, and how far that is durable.
    pub applied: u64,
    pub durable: u64,
    pub series: u64,
    pub blocks: u64,
    /// Bytes observe's folder holds on the machine.
    pub disk_bytes: u64,
    /// Fetching a snapshot's files: bytes so far and in all.
    pub fetching: Option<(u64, u64)>,
    /// Writes of this machine's waiting to be sent, and points dropped for room.
    pub queued: u64,
    pub dropped: u64,
    /// Samples and records of its writes too far ahead of their time (its clock, or its senders'),
    /// dropped by the group.
    pub ahead: u64,
    /// The replica's digest (`kiln_obs::store::Store::digest`) as of `applied`: of its series.
    pub digest: u64,
    /// The full digest last asked for ([`Msg::Verify`]), and the index it is of.
    pub verified: Option<(u64, u64)>,
    pub error: Option<String>,
    /// Unix seconds.
    pub at: i64,
    /// How each scrape target it reads goes.
    pub scrapes: Vec<(String, super::scrape::State)>,
    /// How each log source it reads goes.
    pub collect: Vec<(String, super::collect::State)>,
    /// How each synthetic test it runs goes.
    pub synthetics: Vec<(String, super::synthetics::State)>,
    /// How each DogStatsD listener it opens goes.
    pub statsd: Vec<(String, super::statsd::State)>,
}

pub fn encode(msg: &Msg) -> String {
    serde_json::to_string(msg).unwrap_or_default()
}

pub fn decode(json: &str) -> Option<Msg> {
    serde_json::from_str(json).ok()
}

/// A payload's frames: one, or the parts of a bigger one, each with the message `make` builds for
/// its `(id, part, parts)`.
pub fn frames(id: u64, bytes: Vec<u8>, make: impl Fn(u64, u32, u32) -> Msg) -> Vec<(Msg, Vec<u8>)> {
    if bytes.len() <= PART {
        return vec![(make(0, 0, 0), bytes)];
    }
    let parts = u32::try_from(bytes.len().div_ceil(PART)).unwrap_or(u32::MAX);
    bytes.chunks(PART).zip(0..).map(|(chunk, part)| (make(id, part, parts), chunk.to_vec())).collect()
}

/// Parts of payloads, put back together: at most [`JOINING`] at once from one side, none past
/// [`MAX_PAYLOAD`], none kept past [`PARTS_WAIT`].
#[derive(Default)]
pub struct Joiner {
    open: HashMap<(String, u64), Joining>,
}

struct Joining {
    parts: Vec<Option<Vec<u8>>>,
    have: usize,
    bytes: usize,
    since: Instant,
}

impl Joiner {
    /// A payload's bytes once its last part came (`parts` 0 or 1: it came whole).
    pub fn add(&mut self, from: &str, id: u64, part: u32, parts: u32, bytes: Vec<u8>) -> Option<Vec<u8>> {
        if parts <= 1 {
            return (bytes.len() <= MAX_PAYLOAD).then_some(bytes);
        }
        if parts > MAX_PARTS || part >= parts || bytes.len() > PART {
            return None;
        }
        self.open.retain(|_, j| j.since.elapsed() < PARTS_WAIT);
        let key = (from.to_owned(), id);
        if !self.open.contains_key(&key) && self.open.keys().filter(|(f, _)| f == from).count() >= JOINING {
            return None;
        }
        let j = self.open.entry(key.clone()).or_insert_with(|| Joining { parts: vec![None; parts as usize], have: 0, bytes: 0, since: Instant::now() });
        if j.parts.len() != parts as usize {
            // The same id with another count: not one payload's parts.
            self.open.remove(&key);
            return None;
        }
        let slot = j.parts.get_mut(part as usize)?;
        if slot.is_none() {
            j.bytes += bytes.len();
            *slot = Some(bytes);
            j.have += 1;
        }
        if j.have < j.parts.len() {
            return None;
        }
        let j = self.open.remove(&key)?;
        let mut whole = Vec::with_capacity(j.bytes);
        for p in j.parts.into_iter().flatten() {
            whole.extend_from_slice(&p);
        }
        Some(whole)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raft(id: u64, part: u32, parts: u32) -> Msg {
        Msg::Raft { id, part, parts }
    }

    #[test]
    fn a_big_payload_goes_in_parts_and_comes_back_whole() {
        let bytes: Vec<u8> = (0..(PART * 2 + 17)).map(|i| (i % 251) as u8).collect();
        let sent = frames(9, bytes.clone(), raft);
        assert_eq!(sent.len(), 3);
        let mut j = Joiner::default();
        let mut whole = None;
        // Out of order, one twice.
        for i in [2, 0, 0, 1] {
            let Some((Msg::Raft { id, part, parts }, b)) = sent.get(i).cloned() else { continue };
            whole = whole.or(j.add("m", id, part, parts, b));
        }
        assert_eq!(whole, Some(bytes));
        assert!(matches!(frames(1, vec![1, 2, 3], raft).first(), Some((Msg::Raft { parts: 0, .. }, _))));
    }

    #[test]
    fn a_count_past_the_bound_is_refused_not_allocated() {
        let mut j = Joiner::default();
        assert_eq!(j.add("m", 1, 0, u32::MAX, vec![0; 10]), None);
        assert_eq!(j.add("m", 1, 5, 3, vec![0; 10]), None);
        assert_eq!(j.add("m", 1, 0, 2, vec![0; PART + 1]), None);
        // One side holds only so many open at once; another side still may.
        for id in 0..JOINING as u64 {
            assert_eq!(j.add("a", id, 0, 2, vec![1]), None);
        }
        assert_eq!(j.add("a", 99, 0, 2, vec![1]), None);
        assert!(j.open.keys().all(|(f, _)| f == "a") && j.open.len() == JOINING);
        assert_eq!(j.add("b", 1, 0, 2, vec![1]), None);
        assert_eq!(j.add("b", 1, 1, 2, vec![2]), Some(vec![1, 2]));
    }

    #[test]
    fn messages_round_trip_as_json() {
        let m = Msg::Written { sender: 7, seq: 9, outcome: Outcome { applied: true, unknown: vec![1, 2], ..Outcome::default() } };
        assert_eq!(decode(&encode(&m)), Some(m));
        assert!(decode(r#"{"kind":"release","later":1}"#).is_some());
        assert_eq!(decode(r#"{"kind":"teleport"}"#), None);
        let s: Status = serde_json::from_str(r#"{"member":3,"applied":4}"#).unwrap_or_default();
        assert_eq!((s.member, s.applied, s.error), (3, 4, None));
    }
}

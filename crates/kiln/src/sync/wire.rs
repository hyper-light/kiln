//! What the server and a runner say about synced repos, inside `ServerMsg::Sync` and
//! `AgentMsg::Sync` on a lane of their own: a [`Msg`] as JSON, so either side can add fields, and
//! beside it the bytes of the hyper-raft message it carries, if any. Contents travel over HTTP
//! instead (`GET`/`PUT /api/sync/blobs/:hash`), with the token [`Msg::Copies`] gives a machine.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::Step;
use super::scan::Skipped;

/// A raft message's bytes in one frame, at the most: a bigger one (an image) goes in parts.
pub const PART: usize = 512 << 10;
/// How long the parts of one message wait for the rest.
const PARTS_WAIT: Duration = Duration::from_secs(60);
/// Messages in parts put back together at once, at the most.
const JOINING: usize = 16;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Msg {
    /// A hyper-raft message of `repo`'s group, its bytes beside: whole (`parts` 0), or part `part`
    /// of `parts` of message `id`.
    Raft {
        repo: String,
        #[serde(default)]
        id: u64,
        #[serde(default)]
        part: u32,
        #[serde(default)]
        parts: u32,
    },
    /// Server → runner: every copy this machine holds, whole, and the token its blob requests
    /// carry. Sent when the runner connects and whenever its copies change.
    Copies { copies: Vec<CopySpec>, token: String },
    /// Server → runner: another runner process of this machine syncs now: let go of the copies.
    Release,
    /// Runner → server: a copy's steps for `repo`'s log, in order, their contents on the server.
    Steps { repo: String, steps: Vec<Step> },
    /// Server → runner: contents `repo`'s steps name that the server lacks: the steps are taken
    /// once the contents are there and the steps sent again.
    Want { repo: String, hashes: Vec<String> },
    /// Runner → server: how a copy is doing.
    Status(CopyStatus),
}

/// A copy, as the server tells its machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopySpec {
    pub repo: String,
    pub name: String,
    /// The copy's member of the repo's group.
    pub member: u64,
    pub path: String,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_size: Option<u64>,
}

/// How a copy is doing, as its machine says.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CopyStatus {
    pub repo: String,
    pub member: u64,
    /// Through where the folder has taken the log.
    pub applied: u64,
    /// Committed steps the folder hasn't taken yet.
    pub behind: usize,
    /// Steps made here the log hasn't brought back.
    pub unsent: usize,
    /// Contents the folder waits for.
    pub waiting: usize,
    /// Conflict copies in the folder.
    pub conflicts: usize,
    /// Files the last look left out (a few of them).
    pub skipped: Vec<Skipped>,
    /// What went wrong last, if nothing has gone right since.
    pub error: Option<String>,
    /// Unix seconds.
    pub at: i64,
}

/// A message as it goes on the wire: the JSON and the raft bytes beside it.
pub fn encode(msg: &Msg) -> String {
    serde_json::to_string(msg).unwrap_or_default()
}

pub fn decode(json: &str) -> Option<Msg> {
    serde_json::from_str(json).ok()
}

/// A raft message's frames for `repo`: one, or the parts of a bigger one, numbered `id`.
pub fn frames(repo: &str, id: u64, bytes: Vec<u8>) -> Vec<(Msg, Vec<u8>)> {
    if bytes.len() <= PART {
        return vec![(Msg::Raft { repo: repo.to_owned(), id: 0, part: 0, parts: 0 }, bytes)];
    }
    let parts = bytes.chunks(PART).count();
    let parts = u32::try_from(parts).unwrap_or(u32::MAX);
    bytes
        .chunks(PART)
        .zip(0..)
        .map(|(chunk, part)| (Msg::Raft { repo: repo.to_owned(), id, part, parts }, chunk.to_vec()))
        .collect()
}

/// Parts of messages, put back together.
#[derive(Default)]
pub struct Joiner {
    open: HashMap<(String, u64), Joining>,
}

struct Joining {
    parts: Vec<Option<Vec<u8>>>,
    have: usize,
    since: Instant,
}

impl Joiner {
    /// A message's bytes once its last part came (`parts` 0: it came whole). The parts of a message
    /// whose rest never came are dropped after a while: Raft sends it again.
    pub fn add(&mut self, from: &str, id: u64, part: u32, parts: u32, bytes: Vec<u8>) -> Option<Vec<u8>> {
        if parts <= 1 {
            return Some(bytes);
        }
        self.open.retain(|_, j| j.since.elapsed() < PARTS_WAIT);
        let key = (from.to_owned(), id);
        if !self.open.contains_key(&key) && self.open.len() >= JOINING {
            return None;
        }
        let total = usize::try_from(parts).ok()?;
        let j = self.open.entry(key.clone()).or_insert_with(|| Joining { parts: vec![None; total], have: 0, since: Instant::now() });
        let slot = j.parts.get_mut(usize::try_from(part).ok()?)?;
        if slot.is_none() {
            *slot = Some(bytes);
            j.have += 1;
        }
        if j.have < j.parts.len() {
            return None;
        }
        let j = self.open.remove(&key)?;
        Some(j.parts.into_iter().flatten().flatten().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_big_message_goes_in_parts_and_comes_back_whole() {
        let bytes: Vec<u8> = (0..(PART * 2 + 17)).map(|i| (i % 251) as u8).collect();
        let sent = frames("r", 9, bytes.clone());
        assert_eq!(sent.len(), 3);
        let mut j = Joiner::default();
        let mut whole = None;
        // Out of order, one twice.
        for i in [2, 0, 0, 1] {
            let Some((Msg::Raft { id, part, parts, .. }, b)) = sent.get(i).cloned() else { continue };
            whole = whole.or(j.add("m", id, part, parts, b));
        }
        assert_eq!(whole, Some(bytes));
        let small = frames("r", 1, vec![1, 2, 3]);
        assert!(matches!(small.first(), Some((Msg::Raft { parts: 0, .. }, _))));
    }

    #[test]
    fn messages_round_trip_as_json() {
        let m = Msg::Want { repo: "r".into(), hashes: vec!["ab".into()] };
        assert_eq!(decode(&encode(&m)), Some(m));
        // A field this side doesn't know is ignored.
        assert!(decode(r#"{"kind":"release","later":1}"#).is_some());
    }
}

//! A topic on the server (docs/browser-protocol.md, "Subscriptions"): its state, its revision,
//! and a bounded log of its latest changes, from which a subscriber whose cursor the log still
//! covers is resumed with only what it missed, and any other gets a snapshot. A change is applied
//! and encoded once, then shared by every subscriber's queue.

use std::collections::VecDeque;
use std::sync::Arc;

use crate::conn::TopicSpec;
use crate::frames::{Cursor, Delta};
use crate::state::{Misfit, TopicState};

/// How many changes, and how many of their bytes, a topic's log keeps by default.
pub const LOG_CHANGES: usize = 1024;
pub const LOG_BYTES: usize = 256 * 1024;
/// How much of a text topic's stream is kept for snapshots.
pub const TEXT_KEEP: usize = 1 << 20;

/// A change as published: its revision, and the delta every subscriber shares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    pub rev: u64,
    pub delta: Arc<Delta>,
}

/// How a subscription starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Start {
    /// The topic's state as deltas, at `cursor`.
    Snapshot { cursor: Cursor, items: Vec<Delta> },
    /// Only the changes after `from`.
    Resume { from: Cursor, changes: Vec<Published> },
    /// Not at all.
    Refuse { code: crate::frames::EndCode, why: String },
}

/// The bytes a delta holds, roughly: what the log and the send queues count.
pub fn weight(d: &Delta) -> usize {
    let fields = |f: &[crate::wire::Bytes]| f.iter().map(|b| b.0.len().saturating_add(1)).sum::<usize>();
    16usize.saturating_add(match d {
        Delta::Set { fields: f, .. } | Delta::Put { fields: f, .. } | Delta::Patch { fields: f, .. } => fields(f),
        Delta::Remove { .. } => 0,
        Delta::Append { data, .. } | Delta::Extend { data, .. } => data.0.len(),
    })
}

/// One topic.
#[derive(Debug, Clone)]
pub struct TopicLog {
    pub spec: TopicSpec,
    born: u64,
    rev: u64,
    state: TopicState,
    log: VecDeque<Published>,
    log_bytes: usize,
    max_changes: usize,
    max_bytes: usize,
    text_keep: usize,
}

impl TopicLog {
    /// A topic of the server incarnation `born`.
    pub fn new(spec: TopicSpec, born: u64) -> Self {
        Self { spec, born, rev: 0, state: TopicState::default(), log: VecDeque::new(), log_bytes: 0, max_changes: LOG_CHANGES, max_bytes: LOG_BYTES, text_keep: TEXT_KEEP }
    }

    pub fn with_log(mut self, changes: usize, bytes: usize) -> Self {
        self.max_changes = changes;
        self.max_bytes = bytes;
        self
    }

    /// Keeps at most `bytes` of a text topic's stream for snapshots.
    pub fn with_text_keep(mut self, bytes: usize) -> Self {
        self.text_keep = bytes;
        self
    }

    pub fn cursor(&self) -> Cursor {
        Cursor { born: self.born, rev: self.rev }
    }

    pub fn state(&self) -> &TopicState {
        &self.state
    }

    /// The oldest revision the log can resume from (a cursor at it or after gets only changes).
    pub fn resumes_from(&self) -> u64 {
        self.log.front().map_or(self.rev, |p| p.rev.saturating_sub(1))
    }

    /// Applies a change and logs it: its revision is the topic's next.
    pub fn publish(&mut self, delta: Delta) -> Result<Published, Misfit> {
        self.state.apply(&delta)?;
        self.state.trim_text(self.text_keep);
        self.rev = self.rev.saturating_add(1);
        let p = Published { rev: self.rev, delta: Arc::new(delta) };
        self.log_bytes = self.log_bytes.saturating_add(weight(&p.delta));
        self.log.push_back(p.clone());
        while self.log.len() > self.max_changes || (self.log_bytes > self.max_bytes && self.log.len() > 1) {
            if let Some(old) = self.log.pop_front() {
                self.log_bytes = self.log_bytes.saturating_sub(weight(&old.delta));
            }
        }
        Ok(p)
    }

    /// How a subscription with `cursor` starts: resumed when the cursor is this incarnation's and
    /// the log holds every change after it, else a snapshot.
    pub fn start(&self, cursor: Option<&Cursor>) -> Start {
        if let Some(c) = cursor
            && c.born == self.born
            && c.rev <= self.rev
            && c.rev >= self.resumes_from()
        {
            let changes = self.log.iter().filter(|p| p.rev > c.rev).cloned().collect();
            return Start::Resume { from: c.clone(), changes };
        }
        Start::Snapshot { cursor: self.cursor(), items: self.state.snapshot() }
    }
}

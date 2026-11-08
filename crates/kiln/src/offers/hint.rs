//! What a runner says of its admission, as a hint to the server (docs/reach.md §2.2): the runner's
//! own admission stays what decides (`kiln_ci::place::runner`); the server only shows it.
//!
//! JSON inside one wire frame (`AgentMsg::Offers { json }`, lb/core's, under the named feature
//! [`super::FEATURE`]), so a field added later is optional and a peer a version apart reads it.

use serde::{Deserialize, Serialize};

use kiln_ci::sched::state::Class;

/// The dimensions a run asks for and a runner offers, as `kiln_ci::place::ask::Amounts` counts
/// them: milli-CPUs, bytes of memory, bytes of disk, GPUs.
pub const DIMS: usize = 4;

/// An amount in each dimension, in [`DIMS`]' order.
pub type Amounts = [u64; DIMS];

/// A runner's word on its admission, superseding the last it said in the same epoch.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hint {
    /// Its admission's epoch (one past the last its process took): a hint of an older one is
    /// dropped.
    pub epoch: u64,
    /// Rising with each hint it sends in an epoch: one at or below the last heard is dropped.
    pub seq: u64,
    /// The runner's clock as it said this (unix ms): the durations in [`Quoted::Waits`] count from
    /// it, so its clock's offset from the server's never enters (AUDIT E3).
    pub at: i64,
    /// What kiln may place there in all: its budget, else the machine.
    pub budget: Amounts,
    /// What's free for an ordinary run there now, as its admission measures it.
    pub room: Amounts,
    /// How many wait in its line, by class, in [`Class::ALL`]'s order.
    pub queued: [u32; 5],
    /// Whether it takes work.
    pub state: Taking,
    /// Its admission's answer for each ask size it knows.
    pub quotes: Vec<Quote>,
    /// As an owner (the asking node): runs of its own that wait now, or whose wait changed.
    pub waits: Vec<Wait>,
    /// As an owner: runs of its own that no longer wait (started, withdrawn, ended).
    pub gone: Vec<String>,
    /// `waits` is every run it owns that waits, replacing what it said before (its first hint
    /// after it connects).
    pub whole: bool,
    /// Its placement kernel's word (the `place.fast` feature, docs/lb.md): its room for a new run
    /// as an owner's kernel picks by, nothing while runs wait in its line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fast: Option<kiln_ci::place::msg::FastWord>,
    /// Its topology (LB08, `crate::reach::topo`): its coordinate, its link to the server, its own
    /// network and subnets, as of this hint. Never what makes a hint due.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topo: Option<crate::reach::topo::Word>,
}

/// Whether a runner takes work, and why not.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Taking {
    #[default]
    Taking,
    /// An upgrade is pending: it takes nothing new.
    Draining,
    /// Its machine is under strain, as the gate's guard sees it (sense's predicate): `why` says how.
    Strained { why: String },
}

/// The runner's answer for one ask size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quote {
    pub ask: Amounts,
    pub answer: Quoted,
}

/// What a probe of a size would be answered there now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "answer", rename_all = "snake_case")]
pub enum Quoted {
    /// It would start now.
    Now,
    /// It would wait: its reason's kind (`room`, `kept`, `line`, `whole`, `draining`, `away`, as
    /// `kiln_ci::place::Reason` names them), and when it would start, in milliseconds from the
    /// hint's `at` (soonest and latest; `None`: not known).
    Waits {
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        soonest_in: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        latest_in: Option<u64>,
    },
    /// It can never run there as asked, and why.
    Never { why: String },
}

/// One run its owner holds that waits.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Wait {
    /// Its id, unique among its owner's runs.
    pub run: String,
    /// What the runner must be, as `--needs` spells it (`linux/amd64 +gpu`); empty: anything.
    pub need: String,
    /// The room it asks for.
    pub ask: Amounts,
    pub class: Class,
    /// When it began to wait, in the owner's clock (unix ms), as the hint's `at` is.
    pub since: i64,
    /// Why it waits, as its owner's answer counts it first.
    pub reason: String,
}

/// A runner's room as the server passes it on to the other runners' owners
/// (`ServerMsg::Offers`, docs/needs.md E5): what its last hint said is free for an ordinary run,
/// for an owner to choose where to probe by (`kiln_ci::place::owner::Owner::hint`). A hint, never
/// a claim; the newest of a runner's wins.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Room {
    pub runner: String,
    pub epoch: u64,
    pub seq: u64,
    pub room: Amounts,
    /// The runner's kernel word ([`Hint::fast`]), for owners' kernels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fast: Option<kiln_ci::place::msg::FastWord>,
}

impl Room {
    /// What `hint` from `runner` passes on.
    pub fn of(runner: &str, hint: &Hint) -> Room {
        Room { runner: runner.to_owned(), epoch: hint.epoch, seq: hint.seq, room: hint.room, fast: hint.fast }
    }

    /// Its order among the runner's hints, across its epochs: a later epoch's first is after an
    /// earlier's last (an epoch is one past the last its runner took, a hint's seq counts within one).
    pub fn order(&self) -> u64 {
        self.epoch.saturating_mul(1 << 32).saturating_add(self.seq.min(u64::from(u32::MAX)))
    }
}

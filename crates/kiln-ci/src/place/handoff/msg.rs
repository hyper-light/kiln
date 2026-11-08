//! What nodes say to each other in a hand-off (LB03): JSON, inside the reach link's placement
//! variant each way (`Ask::Place`, `Say::Place`), tagged by `op` as placement's own words are, with
//! ops of their own, so a node from before reads none of them as placement's and lets them go.
//! Each names the run by its first [`Handle`].

use serde::{Deserialize, Serialize};

use super::{Ballot, Handle, Held};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Word {
    /// A runner that holds the run, and can't reach its owner as it needs to, asks the node first
    /// in the run's rendezvous order to adopt it: the members it knows, what it holds, the ballot it
    /// promised for it if it did (a claim goes above it, even one of a node that thought it owned
    /// the run), and the node it can't reach (an in-situ report, Huang et al., OSDI 2018).
    Orphan {
        handle: Handle,
        members: Vec<String>,
        held: Held,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ballot: Option<Ballot>,
        why: String,
    },
    /// The answer to [`Word::Orphan`]: it claims the run under `ballot`, or doesn't, and why.
    Adopting {
        handle: Handle,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ballot: Option<Ballot>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        why: Option<String>,
    },
    /// Phase 1, claimant to member: promise to take no word about the run under a lower ballot, and
    /// say what you hold of it.
    Claim { handle: Handle, ballot: Ballot, members: Vec<String> },
    /// The member's promise: what it holds of the run (none: nothing), the members it knows.
    Promise {
        handle: Handle,
        ballot: Ballot,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        held: Option<Held>,
        members: Vec<String>,
    },
    /// The member promised `ballot` (to `ballot.by`): the claim or word it answers isn't taken.
    /// To the run's first owner, word that the run was handed off.
    Refused { handle: Handle, ballot: Ballot },
    /// The claimant owns the run (every member promised it): the run's words go to it now, under
    /// the run's adopted id ([`Handle::adopted_id`]).
    Own { handle: Handle, ballot: Ballot },
    /// An owner adds runner `add` to the run's members before it probes it there, under `ballot`
    /// (its first owner's, unless said).
    Extend {
        handle: Handle,
        add: String,
        #[serde(default, skip_serializing_if = "is_first")]
        ballot: Ballot,
    },
    /// The member took runner `add` into the run's members.
    Extended { handle: Handle, add: String },
}

fn is_first(b: &Ballot) -> bool {
    *b == Ballot::FIRST
}

impl Word {
    pub fn handle(&self) -> &Handle {
        match self {
            Word::Orphan { handle, .. }
            | Word::Adopting { handle, .. }
            | Word::Claim { handle, .. }
            | Word::Promise { handle, .. }
            | Word::Refused { handle, .. }
            | Word::Own { handle, .. }
            | Word::Extend { handle, .. }
            | Word::Extended { handle, .. } => handle,
        }
    }
}

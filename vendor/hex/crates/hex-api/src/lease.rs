//! A Shards image VM's credential leases (Shards' D242; joint plan step 33, its step 6; docs/design/credential-grants.md
//! C3): the seam through which its orchestrator's broker answers them (CLAUDE.md §9). A VM's credential grants
//! ([`crate::CredentialGrant`]) name credentials by reference; each value is leased just in time, as the VM's egress
//! first needs it, from its orchestrator, to that run alone, and every lease is given back as the run lets go of it,
//! its end among the ways.
//!
//! - [`Lessors`]: what a node's orchestrator gives it: a [`Lessor`] for each run of a VM with credential grants, made
//!   as the run boots.
//! - [`Lessor`]: one run's leases, as Shards' run host asks them (`shards_api::image::Lessor`, which Hex's image class
//!   answers through it).
//! - [`sim::SimLessors`], its simulator, and [`conformance`], the contract both it and every orchestrator's pass.
//!
//! A value is held as briefly as it can be: in a [`Value`], wiped as it drops and never printed.

use std::sync::Arc;
use std::time::Duration;

use crate::{BoxId, Refusal};

pub mod conformance;
pub mod sim;

/// A credential's value: wiped as it drops, and never printed (its `Debug` says its length alone).
pub struct Value(Vec<u8>);

impl Value {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    /// Its bytes, handed on whole with nothing left here to wipe: to a holder that wipes them in turn (Shards'
    /// `Secret`).
    pub fn into_bytes(mut self) -> Vec<u8> {
        std::mem::take(&mut self.0)
    }
}

impl Drop for Value {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.0);
    }
}

impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Value({} bytes)", self.0.len())
    }
}

/// A credential's value leased to a run: the lessor's name for the lease, by which the run gives it back
/// ([`Lessor::release`]), never its value; the value; and how long it lives from now, none where it lives until its
/// run ends.
#[derive(Debug)]
pub struct Lease {
    pub lease: String,
    pub value: Value,
    pub life: Option<Duration>,
}

/// Why a lease is refused, by Shards' kinds (D242's `LeaseRefusal`), which its run host answers the VM's egress with,
/// each in its orchestrator's words, never a value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LeaseRefused {
    /// Its broker leases the run none of it: a credential its store doesn't hold, or one its policy keeps from the run.
    #[error("its broker leases the run none of it: {0}")]
    Denied(String),
    /// Asked under the key the run had before a move replaced it (Shards' D238, D176 part 6): that key, and the run's
    /// key now where it has one, each 64 lowercase hex digits. Its broker alone says which key is current.
    #[error("{asked} is the key its run had before it moved: its key is {}", .current.as_deref().unwrap_or("none"))]
    KeyReplaced {
        asked: String,
        current: Option<String>,
    },
    /// Its broker couldn't be asked, or failed, in its words: a later ask may be leased.
    #[error("its broker couldn't lease it: {0}")]
    Unavailable(String),
    /// The run has ended: nothing is leased to it after.
    #[error("the run has ended: nothing is leased to it after")]
    Ended,
    /// Its lease ended before it was answered: a later ask is leased anew.
    #[error("its lease ended before it was answered")]
    Lapsed,
}

/// One run's leases (Shards' D242): its orchestrator's, for that run alone, from its broker. Every lease it gives is
/// given back once, as the run lets go of it or ends.
pub trait Lessor: Send + Sync {
    /// Credential `reference`'s value, leased to the run now, or why not, typed. Called on a thread of its own, once
    /// for each ask, `reference` one the run's grants name. `key` is the SHA-256 of the run's own key where it binds an
    /// agent-control endpoint (Shards' D252), the identity its leases are issued to, which its broker alone says is
    /// current; none where it binds none, its lessor's run alone asking.
    fn lease(&self, reference: &str, key: Option<[u8; 32]>) -> Result<Lease, LeaseRefused>;
    /// The lease [`lease`](Self::lease) named `lease`, given back: its run let go of it (it lapsed before it was passed
    /// on, or came after the run released the rest) or ended. Called once for each lease given.
    fn release(&self, lease: &str);
}

/// What a node's orchestrator gives it for its Shards image VMs' credentials: a [`Lessor`] for each run of a VM with
/// credential grants, made as the run boots, for that run alone (box `id`'s, one run each).
pub trait Lessors: Send + Sync {
    fn lessor(&self, id: &BoxId) -> Result<Arc<dyn Lessor>, Refusal>;
}

#[cfg(test)]
mod tests;

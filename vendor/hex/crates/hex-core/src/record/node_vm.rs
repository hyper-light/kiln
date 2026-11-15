//! A node VM's record (joint plan step 39): a VM the node runs for itself, its `oci` class's guest under shards-init,
//! never a box. It is a holder of its own in the node's ledger (`hex_sim::peer::Holder::Node`), named by its run
//! ([`NodeRun`]), and its record lies apart from box and pool records, so no box API (list, inspect, delete, signal,
//! the take-back's listing) can see it by construction (ROOT's ruling of 2026-11-13), while the node's charge and
//! capacity accounting count it.

use std::path::PathBuf;

use hex_api::{Allocation, Exit, LearnedEnd, LearnedHolder, NodeRun, NodeVmPhase, Refusal};
use serde::{Deserialize, Serialize};

use super::{MAX_RECORD_BYTES, RECORD_VERSION};

/// What the node knows of one node VM it runs for itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeVmRecord {
    pub version: u32,
    pub run: NodeRun,
    /// The node image it boots, pinned by digest (`HEX_GUEST_IMAGE`).
    pub image: String,
    /// Its admitted size: its receipt in the node's ledger, and what Shards boots.
    pub allocation: Allocation,
    pub phase: NodeVmPhase,
    /// Where its run host's diagnostics are kept, whole (D258).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<PathBuf>,
    /// Its run's end, once it has ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<Exit>,
    /// Why the charge learner's last resort ended or is ending its VM, written before its end (C5, intent first) and kept
    /// across the node's restarts: a node VM is never a box, so its row is where its end is kept, and a node's start
    /// reads it back into its learner's ends before it lets the row go (joint plan step 39).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_resort: Option<LearnedEnd>,
    /// Its place in its node's start order (C3), which its node's store gives it as its row is first written, so its
    /// learner orders it among the host's VMs as it began: before every box its node makes after it boots. None in a
    /// row a node wrote before it kept one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<u64>,
}

fn damaged(why: &str) -> Refusal {
    Refusal::Invalid {
        what: "a node VM's record",
        why: why.to_owned(),
    }
}

impl NodeVmRecord {
    /// A record whole: this format's version, an image pinned by its digest ([`hex_api::check_pinned`]), its receipt
    /// whole, an exit where and only where its run ended, and a last-resort end of its own VM's alone. Its receipt is
    /// held to the VM it sizes ([`Allocation::vm_size`], within its cost profile's own bounds and units) and to booking
    /// no more than it charges: the node sized the VM, so no workload of a box's is kept for it to be rederived from.
    pub fn check(&self) -> Result<(), Refusal> {
        if self.version != RECORD_VERSION {
            return Err(damaged("a format version this node doesn't read"));
        }
        // Pinned by its digest, as its node read it (the review's C-17): a row naming a tag would boot whatever the
        // tag names by the time its node boots it again.
        hex_api::check_pinned("a node VM's record", &self.image)?;
        self.allocation.vm_size()?;
        if self
            .allocation
            .booked
            .is_some_and(|booked| !self.allocation.resources.holds(booked))
        {
            return Err(damaged("its receipt books past what it charges"));
        }
        if self
            .last_resort
            .as_ref()
            .is_some_and(|end| end.id != LearnedHolder::Node(self.run))
        {
            return Err(damaged("its last-resort end names another holder"));
        }
        match (self.phase, self.exit.is_some()) {
            (NodeVmPhase::Ended, false) => Err(damaged("an ended run with no exit")),
            (NodeVmPhase::Booting | NodeVmPhase::Ready, true) => {
                Err(damaged("an exit on a run that hasn't ended"))
            }
            _ => Ok(()),
        }
    }

    /// The record as its row's bytes: its JSON and a newline, within [`MAX_RECORD_BYTES`], measured before any byte is
    /// allocated, as a pool record's are.
    pub fn encode(&self) -> Result<Vec<u8>, Refusal> {
        self.check()?;
        struct Length(usize);
        impl std::io::Write for Length {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 = self
                    .0
                    .checked_add(bytes.len())
                    .ok_or(std::io::ErrorKind::InvalidData)?;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut length = Length(0);
        serde_json::to_writer(&mut length, self)
            .map_err(|_| damaged("it couldn't be measured as JSON"))?;
        let count = length
            .0
            .checked_add(1)
            .filter(|count| *count <= MAX_RECORD_BYTES)
            .ok_or(Refusal::Exhausted {
                what: "node VM record bytes",
                bound: MAX_RECORD_BYTES,
            })?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(count)
            .map_err(|_| Refusal::Exhausted {
                what: "node VM record bytes",
                bound: count,
            })?;
        serde_json::to_writer(&mut bytes, self)
            .map_err(|_| damaged("it couldn't be written as JSON"))?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    /// The record a row's `bytes` hold, whole and checked; refused, never guessed at.
    pub fn decode(bytes: &[u8]) -> Result<Self, Refusal> {
        if bytes.len() > MAX_RECORD_BYTES || !bytes.ends_with(b"\n") {
            return Err(Refusal::Exhausted {
                what: "node VM record bytes",
                bound: MAX_RECORD_BYTES,
            });
        }
        let record: Self =
            serde_json::from_slice(bytes).map_err(|_| damaged("it isn't a node VM's record"))?;
        record.check()?;
        Ok(record)
    }
}

#[cfg(test)]
mod tests;

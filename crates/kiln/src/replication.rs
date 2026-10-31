//! U15: transport framing only. Amphora owns elections, commits, membership and recovery.
//! At most 258 MiB of partial messages across 16 senders, retained for 30 seconds without fragment progress. Overflow
//! refuses the frame; the product transport decides whether and when to resend its message.

pub mod legacy;
pub(crate) mod legacy_read;
mod carrier;
#[cfg(test)]
mod recovery;
pub use carrier::start as carrier;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use futures::FutureExt as _;
use serde::{Deserialize, Serialize};

pub const PART: usize = 512 << 10;
// U15: product snapshots are bounded at 256 MiB plus a 2 MiB wire envelope. The carrier
// must accept that same envelope, otherwise valid snapshot recovery would fail permanently.
pub const MESSAGE_MAX: usize = 258 << 20;
const OPEN_MAX: usize = 16;
const WAIT: Duration = Duration::from_secs(30);
const READINESS_WAIT: Duration = Duration::from_secs(120);

pub fn open_database(path: &std::path::Path, log_path: &std::path::Path, config: &Config) -> anyhow::Result<amphora::database::Database> {
    config.validate()?;
    Ok(amphora::database::Database::open_replicated(path, log_path, amphora_api::replication::ReplicaConfig { node_id: config.id, voters: config.voters.clone(), learners: config.learners.clone(), bootstrap: config.bootstrap.clone() }, crate::store::deadline())?)
}

/// U15: readiness observes a known product leader and completed product bootstrap, so the
/// first SQL deadline starts after seeding. Follower-origin calls use the product's ReadIndex
/// and proposal forwarding; the host never campaigns or changes membership.
pub async fn wait_for_writer(database: &amphora::database::Database, shutdown: &crate::rt::Shutdown) -> anyhow::Result<()> {
    let until = Instant::now() + READINESS_WAIT;
    let mut observed = None;
    let mut unavailable = None;
    loop {
        anyhow::ensure!(Instant::now() < until, "Amphora writer readiness exceeded its startup deadline");
        let checking = database.clone();
        let status = futures::select_biased! {
            () = shutdown.cancelled().fuse() => anyhow::bail!("shutdown while waiting for Amphora writer readiness"),
            () = crate::rt::sleep(until.saturating_duration_since(Instant::now())).fuse() => anyhow::bail!("Amphora writer readiness exceeded its startup deadline"),
            status = crate::rt::blocking_in(crate::rt::SHARE_STORE, move || checking.replica_status(until.min(Instant::now() + Duration::from_secs(5)))).fuse() => status?,
        };
        let status = match status {
            Ok(Some(status)) => { unavailable = None; status },
            Ok(None) => return Ok(()),
            Err(error) if matches!(&error, amphora_api::Refusal::Unavailable(_) | amphora_api::Refusal::Timeout) => {
                let reason = error.to_string();
                if unavailable.as_ref() != Some(&reason) {
                    tracing::warn!(target: "kiln.replication", event = "writer_unavailable", error = %reason, "SQL initialization waits for product owner readiness");
                    unavailable = Some(reason);
                }
                anyhow::ensure!(shutdown.sleep(Duration::from_millis(100)).await, "shutdown while waiting for Amphora writer readiness");
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        anyhow::ensure!(status.fenced.is_none(), "Amphora replica is fenced: {}", status.fenced.as_deref().unwrap_or_default());
        if status.leader != 0 && !status.bootstrapping { return Ok(()); }
        let role = (status.term, status.leader, status.bootstrapping);
        if observed != Some(role) {
            tracing::info!(target: "kiln.replication", event = "writer_standby", node = status.node_id, leader = status.leader, term = status.term, bootstrapping = status.bootstrapping, "SQL initialization waits for a known product leader and completed bootstrap; replication remains active");
            observed = Some(role);
        }
        anyhow::ensure!(shutdown.sleep(Duration::from_millis(100)).await, "shutdown while waiting for Amphora writer readiness");
    }
}

pub fn status(database: &amphora::database::Database, configured: bool) -> serde_json::Value {
    match database.replica() {
        Some(replica) => match replica.status() {
            // Keep legacy read-view keys without inventing an old preupdate sequence or file
            // measurement. Durable/application indices and membership come only from product.
            Ok(status) => serde_json::json!({ "engine": "amphora", "group": true, "seq": null, "applied": status.applied, "learners": status.learners.len(), "bytes": null, "replication": status }),
            Err(error) => serde_json::json!({ "engine": "amphora", "group": true, "seq": null, "applied": null, "learners": null, "bytes": null, "replication": { "unavailable": error.to_string() } }),
        },
        // U15: an SDK client has no local carrier handle; that says nothing about the remote
        // owner's quorum. Keep unavailable indices unknown instead of calling it a local store.
        None if configured => serde_json::json!({ "engine": "amphora", "group": true, "seq": null, "applied": null, "learners": null, "bytes": null, "replication": { "mode": "client", "quorum": null } }),
        None => serde_json::json!({ "engine": "amphora", "group": false, "replication": { "mode": "local", "quorum": false } }),
    }
}

/// U15: explicitly declared product members and authenticated runner routes. Kiln never elects,
/// promotes or removes a member based on connectivity. `hub` is a transport relay, not a leader.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub id: u64,
    pub hub: u64,
    pub voters: Vec<u64>,
    #[serde(default)]
    pub learners: Vec<u64>,
    #[serde(default)]
    pub peers: BTreeMap<String, u64>,
    /// Explicit shared baseline identity for migrating an existing database. The product owns
    /// committed seed transfer; connection recency never chooses a source.
    #[serde(default)]
    pub bootstrap: Option<amphora_api::replication::Bootstrap>,
}
impl Config {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.id != 0 && self.hub != 0 && !self.voters.is_empty(), "Amphora requires nonzero member ids and at least one voter");
        anyhow::ensure!(self.voters.len().saturating_add(self.learners.len()) <= 64 && self.peers.len() <= 63, "Amphora host transport supports at most 64 members");
        let mut members = std::collections::BTreeSet::new();
        for id in self.voters.iter().chain(&self.learners) {
            anyhow::ensure!(*id != 0 && members.insert(*id), "duplicate or zero Amphora member id");
        }
        anyhow::ensure!(members.contains(&self.id) && members.contains(&self.hub), "Amphora local and hub ids must be declared product members");
        if let Some(bootstrap) = &self.bootstrap {
            anyhow::ensure!(members.contains(&bootstrap.source_node), "Amphora baseline source must be a declared member");
        }
        let mut routed = std::collections::BTreeSet::from([self.hub]);
        for (machine, id) in &self.peers {
            anyhow::ensure!(!machine.is_empty() && machine.len() <= 63 && crate::pki::sanitize_name(machine) == *machine && !crate::pki::reserved(machine), "Amphora route must use a canonical, nonreserved machine certificate name");
            anyhow::ensure!(members.contains(id) && routed.insert(*id), "invalid or duplicate Amphora authenticated runner route");
        }
        anyhow::ensure!(routed == members, "every Amphora member requires an authenticated runner route, except the hub");
        Ok(())
    }
    pub fn machine(&self, member: u64) -> Option<&str> { self.peers.iter().find_map(|(machine, id)| (*id == member).then_some(machine.as_str())) }
    pub fn source(&self, machine: &str, member: u64) -> bool { self.peers.get(machine).copied() == Some(member) }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    pub from: u64,
    pub to: u64,
    pub id: u64,
    pub part: u32,
    pub parts: u32,
    pub bytes: Vec<u8>,
}

impl Frame {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.from != 0 && self.to != 0, "Amphora transport member zero is invalid");
        anyhow::ensure!(self.bytes.len() <= PART && self.parts > 0 && self.parts <= u32::try_from(MESSAGE_MAX / PART)? && self.part < self.parts, "invalid Amphora transport fragment");
        Ok(())
    }
}

pub fn frames(from: u64, to: u64, id: u64, bytes: Vec<u8>) -> anyhow::Result<Vec<Frame>> {
    anyhow::ensure!(from != 0 && to != 0, "Amphora transport member zero is invalid");
    anyhow::ensure!(bytes.len() <= MESSAGE_MAX, "Amphora transport message exceeds {MESSAGE_MAX} bytes");
    let parts = u32::try_from(bytes.len().div_ceil(PART).max(1))?;
    if bytes.is_empty() { return Ok(vec![Frame { from, to, id, part: 0, parts, bytes }]); }
    bytes.chunks(PART).enumerate().map(|(part, bytes)| Ok(Frame { from, to, id, part: u32::try_from(part)?, parts, bytes: bytes.to_vec() })).collect()
}

#[derive(Default)]
pub struct Incoming { open: BTreeMap<(u64, u64, u64), Partial> }
struct Partial { at: Instant, parts: Vec<Option<Vec<u8>>>, bytes: usize }

impl Incoming {
    pub fn receive(&mut self, frame: Frame, now: Instant) -> anyhow::Result<Option<Vec<u8>>> {
        frame.validate()?;
        self.open.retain(|_, p| now.saturating_duration_since(p.at) < WAIT);
        let key = (frame.from, frame.to, frame.id);
        if frame.parts == 1 {
            anyhow::ensure!(!self.open.contains_key(&key), "Amphora transport fragment count changed");
            return Ok(Some(frame.bytes));
        }
        let used: usize = self.open.values().map(|p| p.bytes).sum();
        anyhow::ensure!(used.saturating_add(frame.bytes.len()) <= MESSAGE_MAX, "Amphora transport reassembly byte budget exhausted");
        anyhow::ensure!(self.open.contains_key(&key) || self.open.len() < OPEN_MAX, "Amphora transport reassembly slot budget exhausted");
        let total = usize::try_from(frame.parts)?;
        let p = self.open.entry(key).or_insert_with(|| Partial { at: now, parts: vec![None; total], bytes: 0 });
        anyhow::ensure!(p.parts.len() == total, "Amphora transport fragment count changed");
        let slot = p.parts.get_mut(usize::try_from(frame.part)?).ok_or_else(|| anyhow::anyhow!("invalid Amphora transport fragment index"))?;
        if let Some(old) = slot {
            anyhow::ensure!(*old == frame.bytes, "Amphora transport duplicate fragment differs");
        } else {
            p.bytes += frame.bytes.len();
            p.at = now;
            *slot = Some(frame.bytes);
        }
        if p.parts.iter().any(Option::is_none) { return Ok(None); }
        let p = self.open.remove(&key).ok_or_else(|| anyhow::anyhow!("Amphora transport assembly disappeared"))?;
        Ok(Some(p.parts.into_iter().flatten().flatten().collect()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn u15_product_routes_reject_missing_and_duplicate_members() -> anyhow::Result<()> {
        let mut config = Config { id: 1, hub: 1, voters: vec![1, 2, 3], learners: Vec::new(), peers: BTreeMap::from([("runner-a".into(), 2), ("runner-b".into(), 3)]), bootstrap: None };
        config.validate()?;
        assert!(config.source("runner-a", 2));
        assert!(!config.source("runner-b", 2));
        config.peers.remove("runner-a");
        config.peers.insert("Runner A".into(), 2);
        assert!(config.validate().is_err(), "an alias must not silently map to a different certificate identity");
        config.peers.remove("Runner A");
        config.peers.insert("runner-a".into(), 2);
        config.peers.remove("runner-b");
        assert!(config.validate().is_err());
        config.peers.insert("runner-b".into(), 2);
        assert!(config.validate().is_err());
        Ok(())
    }

    #[test]
    fn u15_product_transport_expires_partial_messages_and_rejects_conflicting_duplicates() -> anyhow::Result<()> {
        let chunks = frames(1, 2, 9, vec![7; PART + 1])?;
        let first = chunks.into_iter().next().ok_or_else(|| anyhow::anyhow!("missing fragment"))?;
        let mut incoming = Incoming::default();
        let now = Instant::now();
        assert!(incoming.receive(first.clone(), now)?.is_none());
        let mut different = first.clone();
        different.bytes = vec![8; PART];
        assert!(incoming.receive(different, now).is_err());
        let mut whole = first;
        whole.parts = 1;
        whole.bytes = vec![9];
        assert_eq!(incoming.receive(whole, now + WAIT)?, Some(vec![9]));
        Ok(())
    }

    #[test]
    fn u15_product_transport_reassembles_reordered_frames_and_refuses_invalid_parts() -> anyhow::Result<()> {
        let bytes = vec![7; PART + 19];
        let mut frames = frames(1, 2, 3, bytes.clone())?;
        let last = frames.pop().ok_or_else(|| anyhow::anyhow!("missing frame"))?;
        let first = frames.pop().ok_or_else(|| anyhow::anyhow!("missing frame"))?;
        let mut incoming = Incoming::default();
        let now = Instant::now();
        assert_eq!(incoming.receive(last.clone(), now)?, None);
        assert_eq!(incoming.receive(last.clone(), now)?, None);
        assert_eq!(incoming.receive(first, now)?, Some(bytes));
        let mut bad = last;
        bad.parts = u32::MAX;
        assert!(incoming.receive(bad, now).is_err());
        Ok(())
    }
}

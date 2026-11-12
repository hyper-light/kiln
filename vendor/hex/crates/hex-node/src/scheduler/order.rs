//! LB03: fence old peer connections before handing their effects to the FIFO peer core.
//! A sender durably increments its epoch at boot, then numbers each peer's messages. A receiver
//! commits its high-water mark before the core acts. Retrying the current message is idempotent;
//! a delayed older message cannot recreate a box after delete/ended/forget.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use hex_api::Refusal;
use hex_sim::peer::{Records, Stop};

use crate::records::Files;

pub struct Order {
    files: Files,
    pub epoch: u64,
    received: BTreeMap<u32, (u64, u64, [u8; 32])>,
    configured: Vec<u32>,
}

fn damaged(why: impl Into<String>) -> Refusal {
    Refusal::Broken(why.into())
}

fn storage(op: &str, why: Stop) -> Refusal {
    match why {
        Stop::Full { what, bound } => Refusal::Exhausted { what, bound },
        why => damaged(format!("{op}: {why:?}")),
    }
}

fn checked_peer(peer: u32, configured: &[u32]) -> Result<(), Refusal> {
    if configured.binary_search(&peer).is_err() {
        return Err(Refusal::Invalid {
            what: "peer ordering sender",
            why: format!("unconfigured peer {peer}"),
        });
    }
    Ok(())
}

impl Order {
    /// Configured senders bound the receive ledger, plus one local sender-epoch row.
    pub fn open_with_peers(path: &Path, configured: &BTreeSet<u32>) -> Result<Self, Refusal> {
        let mut ids = Vec::new();
        ids.try_reserve_exact(configured.len())
            .map_err(|_| Refusal::Exhausted {
                what: "peer ordering configured senders",
                bound: configured.len(),
            })?;
        // BTreeSet order gives binary-search membership; reserve covers every push.
        ids.extend(configured.iter().copied());
        let configured = ids;
        let max_peers = configured.len();
        let most = max_peers.checked_add(1).ok_or(Refusal::Exhausted {
            what: "peer ordering records including sender epoch",
            bound: max_peers,
        })?;
        let mut files =
            Files::open_with_limit(path, most).map_err(|e| storage("peer order files", e))?;
        let mut epoch = 0u64;
        let mut received = BTreeMap::new();
        for (name, value) in files.all().map_err(|e| storage("peer order rows", e))? {
            if name == "epoch" {
                epoch = value
                    .parse()
                    .map_err(|_| damaged("invalid peer sender epoch"))?;
            } else if let Some(id) = name.strip_prefix("peer-") {
                let id: u32 = id.parse().map_err(|_| damaged("invalid peer order ID"))?;
                if name != format!("peer-{id}") {
                    return Err(damaged("noncanonical peer order ID"));
                }
                checked_peer(id, &configured)?;
                let mut words = value.split_whitespace();
                if words.next() != Some("o1") {
                    return Err(damaged("invalid peer order version"));
                }
                let epoch = words
                    .next()
                    .ok_or_else(|| damaged("missing peer epoch"))?
                    .parse()
                    .map_err(|_| damaged("invalid peer epoch"))?;
                let seq = words
                    .next()
                    .ok_or_else(|| damaged("missing peer sequence"))?
                    .parse()
                    .map_err(|_| damaged("invalid peer sequence"))?;
                if epoch == 0 || seq == 0 {
                    return Err(damaged("zero persisted peer message epoch or sequence"));
                }
                let hex = words
                    .next()
                    .ok_or_else(|| damaged("missing peer fingerprint"))?;
                if hex.len() != 64 || words.next().is_some() {
                    return Err(damaged("invalid peer fingerprint"));
                }
                let mut digest = [0u8; 32];
                for (out, pair) in digest.iter_mut().zip(hex.as_bytes().as_chunks::<2>().0) {
                    let pair = std::str::from_utf8(pair)
                        .map_err(|_| damaged("invalid peer fingerprint text"))?;
                    *out = u8::from_str_radix(pair, 16)
                        .map_err(|_| damaged("invalid peer fingerprint hex"))?;
                }
                let stamp = (epoch, seq, digest);
                received.insert(id, stamp);
            } else {
                return Err(damaged("unknown peer order record"));
            }
        }
        epoch = epoch
            .checked_add(1)
            .ok_or_else(|| damaged("peer sender epoch exhausted"))?;
        files
            .put("epoch", &epoch.to_string())
            .map_err(|e| storage("committing peer epoch", e))?;
        Ok(Self {
            files,
            epoch,
            received,
            configured,
        })
    }

    /// Caller holds its mutex until the matching driver operation has returned. This serializes
    /// old/new connections across physical effects, including a request already queued at reconnect.
    pub fn accept(
        &mut self,
        peer: u32,
        epoch: u64,
        seq: u64,
        digest: [u8; 32],
    ) -> Result<bool, Refusal> {
        checked_peer(peer, &self.configured)?;
        if epoch == 0 || seq == 0 {
            return Err(damaged("zero peer message epoch or sequence"));
        }
        let stamp = (epoch, seq);
        if let Some(&(last_epoch, last_seq, last_digest)) = self.received.get(&peer) {
            let last = (last_epoch, last_seq);
            if stamp < last {
                return Ok(false);
            }
            if stamp == last {
                if digest != last_digest {
                    return Err(damaged("a retried peer sequence changed its message"));
                }
                return Ok(true);
            }
        } else if self.received.len() >= self.configured.len() {
            return Err(Refusal::Exhausted {
                what: "peer ordering records",
                bound: self.configured.len(),
            });
        }
        use std::fmt::Write;
        let mut fingerprint = String::new();
        for byte in digest {
            write!(&mut fingerprint, "{byte:02x}").map_err(|e| damaged(e.to_string()))?;
        }
        self.files
            .put(
                &format!("peer-{peer}"),
                &format!("o1 {epoch} {seq} {fingerprint}"),
            )
            .map_err(|e| storage("committing peer message order", e))?;
        self.received.insert(peer, (epoch, seq, digest));
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC-T7.4.1/LB03: exactly the configured senders plus one sender epoch fit; unknown senders
    /// cannot change an existing file or allocate another record, including after a restart.
    #[test]
    fn order_uses_exact_configured_sender_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let peers = BTreeSet::from([4, 9]);
        let mut order = Order::open_with_peers(dir.path(), &peers).unwrap();
        assert!(order.accept(4, 1, 1, [1; 32]).unwrap());
        assert!(order.accept(9, 1, 1, [2; 32]).unwrap());
        let before = std::fs::read(dir.path().join("peer-4.rec")).unwrap();
        assert!(matches!(
            order.accept(5, 1, 1, [0; 32]),
            Err(Refusal::Invalid { .. })
        ));
        assert!(!dir.path().join("peer-5.rec").exists());
        assert_eq!(
            std::fs::read(dir.path().join("peer-4.rec")).unwrap(),
            before
        );
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            peers.len() + 1
        );
        drop(order);
        let mut restored = Order::open_with_peers(dir.path(), &peers).unwrap();
        assert!(restored.accept(4, 1, 1, [1; 32]).unwrap());
        assert!(restored.accept(4, 1, 0, [1; 32]).is_err());
        assert!(restored.accept(9, 1, 2, [3; 32]).unwrap());
        let empty = tempfile::tempdir().unwrap();
        let mut no_peers = Order::open_with_peers(empty.path(), &BTreeSet::new()).unwrap();
        assert!(matches!(
            no_peers.accept(4, 1, 1, [0; 32]),
            Err(Refusal::Invalid { .. })
        ));
        assert_eq!(std::fs::read_dir(empty.path()).unwrap().count(), 1);
    }

    /// AC-T7.4.1/LB03: wrong persisted membership, aliases and malformed rows refuse before the
    /// epoch write; valid durable state is preserved for explicit recovery, never silently deleted.
    #[test]
    fn order_refuses_wrong_or_corrupt_restart_without_mutating_valid_records() {
        for (name, value) in [
            ("peer-9", format!("o1 1 1 {}", "00".repeat(32))),
            ("peer-04", format!("o1 1 1 {}", "00".repeat(32))),
            ("peer-4", format!("o1 0 1 {}", "00".repeat(32))),
            ("peer-4", format!("o1 1 0 {}", "00".repeat(32))),
            ("peer-4", "malformed".into()),
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("epoch.rec"), "7").unwrap();
            let path = dir.path().join(format!("{name}.rec"));
            std::fs::write(&path, &value).unwrap();
            assert!(
                Order::open_with_peers(dir.path(), &BTreeSet::from([4])).is_err(),
                "{name}"
            );
            assert_eq!(
                std::fs::read_to_string(dir.path().join("epoch.rec")).unwrap(),
                "7"
            );
            assert_eq!(std::fs::read_to_string(path).unwrap(), value);
        }
        let dir = tempfile::tempdir().unwrap();
        let mut order = Order::open_with_peers(dir.path(), &BTreeSet::from([4, 9])).unwrap();
        order.accept(4, 1, 1, [0; 32]).unwrap();
        order.accept(9, 1, 1, [0; 32]).unwrap();
        drop(order);
        let epoch = std::fs::read(dir.path().join("epoch.rec")).unwrap();
        assert!(matches!(
            Order::open_with_peers(dir.path(), &BTreeSet::from([4])),
            Err(Refusal::Exhausted { bound: 2, .. })
        ));
        assert_eq!(std::fs::read(dir.path().join("epoch.rec")).unwrap(), epoch);
        assert!(dir.path().join("peer-9.rec").is_file());
    }

    #[test]
    fn lb03_delayed_old_start_stays_fenced_after_restart() {
        let temp = tempfile::tempdir().unwrap();
        let mut order = Order::open_with_peers(temp.path(), &BTreeSet::from([4])).unwrap();
        let sender_epoch = order.epoch;
        assert!(order.accept(4, 1, 1, [0; 32]).unwrap()); // Start.
        assert!(order.accept(4, 1, 2, [0; 32]).unwrap()); // Delete, with physical deletion completed under lock.
        assert!(order.accept(4, 1, 3, [0; 32]).unwrap()); // Forget.
        assert!(!order.accept(4, 1, 1, [0; 32]).unwrap());
        drop(order);
        let mut order = Order::open_with_peers(temp.path(), &BTreeSet::from([4])).unwrap();
        assert!(order.epoch > sender_epoch);
        assert!(!order.accept(4, 1, 1, [0; 32]).unwrap());
        assert!(order.accept(4, 2, 1, [0; 32]).unwrap()); // New connection generation after owner reboot.
        assert!(!order.accept(4, 1, u64::MAX, [0; 32]).unwrap());
        assert!(order.accept(4, 2, 1, [1; 32]).is_err());
        assert!(order.accept(4, 2, 1, [0; 32]).unwrap()); // Current delivery can repeat after a lost response.
    }
}

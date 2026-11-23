//! AD-1. Heartbeats share the batched reach socket, independently of any per-session stream.
use crate::convo::raft::node_id;
use crate::liveness::{Disk, Event, Shared};
use crate::reach::{e2e::Identity, quic::PeerEntry};
use anyhow::{Context, Result};
use aws_lc_rs::signature::{
    ECDSA_P384_SHA384_FIXED, ECDSA_P384_SHA384_FIXED_SIGNING, EcdsaKeyPair, KeyPair,
    ParsedPublicKey,
};
use hyper_liveness::{Change, Output};
use hyper_rt::udp::Batched;
use hyper_timing::Exposure;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

const MAGIC: &[u8; 5] = b"\x00KLH2";
// RFC 5480 uncompressed P-384 point and IEEE P1363 fixed-length ECDSA signature.
const POINT: usize = 97;
const SIG: usize = 96;
const HEADER: usize = MAGIC.len() + 3 * size_of::<u64>() + POINT;
pub(crate) const ENVELOPE_BYTES: usize = HEADER + SIG;
struct Signing {
    key: EcdsaKeyPair,
    spki_prefix: Vec<u8>,
    me: u64,
}
impl Signing {
    fn open(id: &Identity) -> Result<Self> {
        use x509_parser::prelude::*;
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P384_SHA384_FIXED_SIGNING, id.key().secret_der())
            .map_err(|_| anyhow::anyhow!("unsupported reach heartbeat key"))?;
        let (_, cert) = X509Certificate::from_der(&id.cert_der)
            .map_err(|_| anyhow::anyhow!("invalid reach certificate"))?;
        let prefix = cert
            .public_key()
            .raw
            .strip_suffix(key.public_key().as_ref())
            .context("reach key differs from its certificate")?;
        Ok(Self {
            key,
            spki_prefix: prefix.to_vec(),
            me: node_id(&id.name),
        })
    }
    fn packet<'a>(&self, to: u64, to_run: u64, data: &[u8], buf: &'a mut [u8]) -> Option<&'a [u8]> {
        let wanted = HEADER.checked_add(data.len())?;
        let out = buf.get_mut(..wanted.checked_add(SIG)?)?;
        let (signed, signature) = out.split_at_mut(wanted);
        let mut dst = &mut *signed;
        use std::io::Write as _;
        for bytes in [
            MAGIC.as_slice(),
            &self.me.to_be_bytes(),
            &to.to_be_bytes(),
            &to_run.to_be_bytes(),
            self.key.public_key().as_ref(),
            data,
        ] {
            dst.write_all(bytes).ok()?;
        }
        let value = self
            .key
            .sign(&aws_lc_rs::rand::SystemRandom::new(), signed)
            .ok()?;
        if signature.len() != value.as_ref().len() {
            return None;
        }
        signature.copy_from_slice(value.as_ref());
        Some(out)
    }
    fn verify<'a>(&self, data: &'a [u8], peers: &mut HashMap<u64, Peer>) -> Option<(u64, u64, &'a [u8])> {
        let fields = data.strip_prefix(MAGIC.as_slice())?;
        let (src, fields) = fields.split_first_chunk::<8>()?;
        let (dst, fields) = fields.split_first_chunk::<8>()?;
        let (to_run, fields) = fields.split_first_chunk::<8>()?;
        let from = u64::from_be_bytes(*src);
        if u64::from_be_bytes(*dst) != self.me || from == self.me {
            return None;
        }
        let peer = peers.get_mut(&from)?;
        let (key, fields) = fields.split_first_chunk::<POINT>()?;
        let (payload, sig) = fields.split_last_chunk::<SIG>()?;
        if payload.len() > hyper_liveness::MAX_BYTES {
            return None;
        }
        // A directory pin change discards the entire Peer. Check and parse its P-384
        // point once per enrollment, then verify each datagram with the pinned key.
        if peer.key.is_none() {
            let digest = Sha256::new().chain_update(&self.spki_prefix).chain_update(key).finalize();
            if digest.as_slice() != peer.pin { return None; }
            peer.key = Some(ParsedPublicKey::new(&ECDSA_P384_SHA384_FIXED, key).ok()?);
        }
        let enrolled = peer.key.as_ref()?;
        if enrolled.as_ref() != key {
            return None;
        }
        enrolled.verify_sig(data.get(..data.len().checked_sub(SIG)?)?, sig).ok()?;
        Some((from, u64::from_be_bytes(*to_run), payload))
    }
}
struct Peer {
    pin: [u8; 32],
    key: Option<ParsedPublicKey>,
    run: u64,
    listed: bool,
    addresses: Vec<SocketAddr>,
    next: usize,
    last: Option<SocketAddr>,
}
impl Peer {
    fn new(pin: [u8; 32]) -> Self {
        Self { pin, key: None, run: 0, listed: true, addresses: Vec::new(), next: 0, last: None }
    }
    fn update_addresses(&mut self, addresses: &[SocketAddr]) {
        if self.addresses == addresses { return; }
        if self.last.is_some_and(|addr| self.addresses.contains(&addr) && !addresses.contains(&addr)) {
            self.last = None;
        }
        self.addresses.clear();
        self.addresses.extend_from_slice(addresses);
        self.next = 0;
    }
    /// The first signed packet introduces the sender's incarnation. A packet signed
    /// before this receiver restarted cannot supply evidence to its new detector.
    fn accepts(&mut self, local_run: u64, to_run: u64, message: &[u8]) -> bool {
        let Ok(beat) = hyper_liveness::Heartbeat::decode(message) else { return false; };
        if beat.run == 0 || beat.run < self.run { return false; }
        self.run = beat.run;
        to_run == local_run
    }
}
struct Datagram {
    bytes: [u8; hyper_liveness::MAX_BYTES],
    len: usize,
}
#[derive(Default)]
struct Sent {
    changes: Vec<Change>,
    flush: bool,
    // One latest heartbeat per attached peer. Socket congestion preserves each peer's turn.
    pending: HashMap<u64, Datagram>,
    order: VecDeque<u64>,
}
impl Output for Sent {
    fn heartbeat(&mut self, peer: u64, msg: &[u8]) {
        if msg.len() > hyper_liveness::MAX_BYTES { return; }
        let packet = self.pending.entry(peer).or_insert_with(|| {
            self.order.push_back(peer);
            Datagram { bytes: [0; hyper_liveness::MAX_BYTES], len: 0 }
        });
        if let Some(dst) = packet.bytes.get_mut(..msg.len()) {
            dst.copy_from_slice(msg);
            packet.len = msg.len();
        }
    }
    fn change(&mut self, c: Change) { self.changes.push(c); }
    fn flush(&mut self) { self.flush = true; }
}
impl Sent {
    fn send(&mut self, sign: &Signing, peers: &mut HashMap<u64, Peer>, socket: &mut Batched, attached: impl Fn(u64) -> bool) {
        while !self.order.is_empty() && socket.slot().is_some() {
            let Some(id) = self.order.pop_front() else { break; };
            let Some(msg) = self.pending.remove(&id) else { continue; };
            // The last shared group can disappear while the socket is congested.
            if !attached(id) { continue; }
            let Some(p) = peers.get_mut(&id) else { continue; };
            let Some(addr) = p.last.or_else(|| p.addresses.get(p.next).copied()) else { continue; };
            if !p.addresses.is_empty() { p.next = p.next.wrapping_add(1) % p.addresses.len(); }
            let mut packet = [0; ENVELOPE_BYTES + hyper_liveness::MAX_BYTES];
            if let Some(bytes) = msg.bytes.get(..msg.len).and_then(|raw| sign.packet(id, p.run, raw, &mut packet)) {
                socket.queue(addr, bytes);
            }
        }
    }
}
pub(super) struct Plane {
    shared: Arc<Shared>,
    sign: Signing,
    peers: HashMap<u64, Peer>,
    disk: Disk,
    state: Sent,
    saved: Option<Exposure>,
    measured_floor: Option<Duration>,
    observed_revision: Option<u64>,
    received: bool,
    wake: Option<(u64, Instant)>,
}
impl Plane {
    pub(super) fn open(dir: &Path, id: &Identity, wake: &Arc<crate::rt::Notify>) -> Result<Self> {
        let sign = Signing::open(id)?;
        let shared = Shared::get(sign.me);
        let disk = Disk::open(dir, &shared, wake)?;
        Ok(Self {
            shared,
            sign,
            peers: HashMap::new(),
            disk,
            state: Sent::default(),
            saved: None,
            measured_floor: None,
            observed_revision: None,
            received: false,
            wake: None,
        })
    }
    pub(super) fn incarnation(&self) -> (std::sync::Weak<Shared>, u64) {
        (Arc::downgrade(&self.shared), self.disk.generation)
    }
    pub(super) fn directory(&mut self, entries: &[PeerEntry]) {
        for peer in self.peers.values_mut() { peer.listed = false; }
        for e in entries {
            let id = node_id(&e.name);
            let mut pin = [0; 32];
            if hex::decode_to_slice(&e.fingerprint, &mut pin).is_err() {
                continue;
            }
            let peer = self.peers.entry(id).or_insert_with(|| Peer::new(pin));
            if peer.pin != pin { *peer = Peer::new(pin); }
            peer.listed = true;
            peer.update_addresses(&e.addrs);
        }
        self.peers.retain(|_, peer| peer.listed);
    }
    pub(super) fn recognizes(data: &[u8]) -> bool {
        data.starts_with(MAGIC)
    }
    pub(super) fn receive(
        &mut self,
        from_addr: SocketAddr,
        stamp: u64,
        data: &[u8],
    ) {
        let Some((from, to_run, message)) = self.sign.verify(data, &mut self.peers) else {
            return;
        };
        if !self.peers.get_mut(&from).is_some_and(|p| p.accepts(self.disk.run, to_run, message)) { return; }
        self.received = true;
        let result = self.shared.with_generation(self.disk.generation, |l| {
            let unmeasured = l.round_trip(from).is_none_or(|r| r.samples() == 0);
            let old = l.report(from).map(|r| r.configurations);
            let accepted = l.on_heartbeat(from, message, stamp, &mut self.state).is_ok();
            (
                accepted,
                accepted
                    && (unmeasured && l.round_trip(from).filter(|r| r.samples() > 0).is_some()
                        || old != l.report(from).map(|r| r.configurations)),
            )
        });
        if let Some((true, timing)) = result {
            if let Some(p) = self.peers.get_mut(&from) {
                p.last = Some(from_addr);
            }
            if timing {
                self.shared.publish(self.disk.generation, Event::Timing(from));
            }
        }
    }
    pub(super) fn drive(&mut self, now: u64, stop: bool) {
        // A busy QUIC stream can drain this socket many times between detector events.
        // Unrelated packets have supplied no new evidence and do not require a log flush.
        let revision = self.shared.revision();
        if !stop && !self.received && self.observed_revision == Some(revision)
            && self.wake.is_none_or(|(deadline, _)| now < deadline)
        { return; }
        self.received = false;
        self.observed_revision = Some(revision);
        let Some((due, history, disk_changed, timing)) = self.shared.with_generation(self.disk.generation, |l| {
            if let Some((asked, _)) = self.wake
                && now >= asked
            {
                l.on_wait(asked, now);
            }
            let disk = l.own_disk_slow();
            if stop {
                l.stop(now, &mut self.state);
            } else {
                l.poll(now, &mut self.state);
            }
            let timing = l.granularity().zip(l.flush_mean()).and_then(|(grain, flush)| {
                let floor = grain.saturating_add(flush);
                self.measured_floor.is_none_or(|old| old.abs_diff(floor) >= grain).then_some(floor)
            });
            (l.wake(), l.history(), l.own_disk_slow() != disk, timing)
        }) else {
            return;
        };
        if let Some(floor) = timing {
            self.measured_floor = Some(floor);
            for peer in self.peers.keys().copied() {
                self.shared.publish(self.disk.generation, Event::Timing(peer));
            }
        }
        self.disk.submit(
            (self.saved != Some(history)).then_some(history),
            std::mem::take(&mut self.state.flush),
        );
        self.saved = Some(history);
        for change in self.state.changes.drain(..) {
            tracing::info!(target:"kiln.liveness", node=self.sign.me, peer=change.peer(), ?change, "shared node liveness changed");
            if matches!(change, Change::Suspected(_)) && let Some(peer) = self.peers.get_mut(&change.peer()) {
                peer.last = None; // Probe the enrolled alternatives on the next heartbeat.
            }
            self.shared.publish(self.disk.generation, Event::Change(change));
        }
        if disk_changed {
            self.shared.publish(self.disk.generation, Event::LocalDisk);
        }
        if self.wake.map(|(w, _)| w) != due {
            self.wake = due.and_then(|w| {
                // Verification and QUIC dispatch may have taken time since the receive
                // watermark. Park against the clock now, preserving the detector's deadline.
                let clock = self.shared.now_ns();
                Instant::now()
                    .checked_add(Duration::from_nanos(w.saturating_sub(clock)))
                    .map(|i| (w, i))
            });
        }
    }
    pub(super) fn send(&mut self, socket: &mut Batched) -> bool {
        if self.shared.generation() != self.disk.generation { return false; }
        self.state.send(&self.sign, &mut self.peers, socket, |peer| {
            self.shared.with_generation(self.disk.generation, |l| l.trust(peer).is_some()) == Some(true)
        });
        !self.state.order.is_empty()
    }
    pub(super) fn deadline(&self) -> Option<Instant> {
        self.wake.map(|(_, i)| i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper_liveness::{Heartbeat, Liveness, Settings, Write};
    use kiln_sim::queue::Queue;

    #[derive(Default)]
    struct Observed {
        changes: Vec<Change>,
    }
    impl Output for Observed {
        fn heartbeat(&mut self, _: u64, _: &[u8]) {}
        fn change(&mut self, c: Change) {
            self.changes.push(c);
        }
        fn flush(&mut self) {}
    }
    struct Pair {
        _dir: tempfile::TempDir,
        receiver: Signing,
        sender: Signing,
        peers: HashMap<u64, Peer>,
        detector: Liveness,
        seen: Observed,
        wire: Queue<(SocketAddr, Vec<u8>)>,
        clock: u64,
    }
    impl Pair {
        fn new() -> Result<Self> {
            let dir = tempfile::tempdir()?;
            let a = Identity::load_or_create(&dir.path().join("a"), "a")?;
            let b = Identity::load_or_create(&dir.path().join("b"), "b")?;
            let receiver = Signing::open(&a)?;
            let sender = Signing::open(&b)?;
            let mut pin = [0; 32];
            hex::decode_to_slice(b.fingerprint, &mut pin)?;
            let peers = HashMap::from([(
                sender.me,
                Peer {
                    pin,
                    key: None,
                    run: 0,
                    listed: true,
                    addresses: vec!["127.0.0.1:1".parse()?],
                    next: 0,
                    last: None,
                },
            )]);
            let mut detector = Liveness::new(Settings {
                local: receiver.me,
                run: 1,
                history: Exposure::new(),
                max_peers: 1,
                resolution: Duration::from_nanos(1),
            })?;
            detector.set_disk_apart(true);
            detector.attach(sender.me)?;
            let mut seen = Observed::default();
            detector.on_durable(Write::Log, 0, 1);
            detector.poll(2, &mut seen);
            detector.on_durable(Write::Liveness, 2, 3);
            detector.poll(3, &mut seen);
            let wake = detector.wake().context("the attached pair requested no wake")?;
            let clock = wake + 1;
            detector.on_wait(wake, clock);
            detector.poll(clock, &mut seen);
            anyhow::ensure!(detector.granularity().is_some(), "fixture lacks an observed timer wake");
            Ok(Self {
                _dir: dir,
                receiver,
                sender,
                peers,
                detector,
                seen,
                clock,
                wire: Queue::new(),
            })
        }
        fn send(&mut self, signer: Option<&Signing>, run: u64, seq: u64, addr: &str) -> Result<()> {
            self.send_for(signer, run, seq, addr, 1)
        }
        fn send_for(&mut self, signer: Option<&Signing>, run: u64, seq: u64, addr: &str, to_run: u64) -> Result<()> {
            self.clock += 1;
            let h = Heartbeat {
                run,
                seq,
                interval_ns: 1,
                floor_ns: 1,
                ask_ns: 0,
                sent_ns: self.clock,
                late_ns: 0,
                flushes: seq + 1,
                flush_age_ns: 0,
                apart: true,
                disk_slow: false,
                echo: None,
                stopping: false,
            };
            let mut plain = [0; hyper_liveness::MAX_BYTES];
            let mut packet = [0; ENVELOPE_BYTES + hyper_liveness::MAX_BYTES];
            let encoded = signer
                .unwrap_or(&self.sender)
                .packet(self.receiver.me, to_run, h.encode(&mut plain), &mut packet)
                .context("signing test datagram")?;
            anyhow::ensure!(self.wire.push(
                Duration::from_nanos(self.clock),
                (addr.parse()?, encoded.to_vec())
            ));
            Ok(())
        }
        fn receive(&mut self) -> Result<bool> {
            let (at, (from, packet)) = self.wire.pop().context("no packet")?;
            let Some((id, to_run, beat)) = self.receiver.verify(&packet, &mut self.peers) else {
                return Ok(false);
            };
            if !self.peers.get_mut(&id).is_some_and(|p| p.accepts(1, to_run, beat)) { return Ok(false); }
            let accepted = self
                .detector
                .on_heartbeat(id, beat, u64::try_from(at.as_nanos())?, &mut self.seen)
                .is_ok();
            if accepted && let Some(p) = self.peers.get_mut(&id) {
                p.last = Some(from);
            }
            Ok(accepted)
        }
    }
    #[test]
    fn ad1_a_spoofed_heartbeat_changes_nothing() -> Result<()> {
        let mut p = Pair::new()?;
        let dir = tempfile::tempdir()?;
        let other = Signing::open(&Identity::load_or_create(dir.path(), "b")?)?;
        p.send(Some(&other), 1, 0, "127.0.0.1:1")?;
        anyhow::ensure!(
            !p.receive()?
                && p.seen.changes.is_empty()
                && p.detector.report(p.sender.me).is_some_and(|r| r.taken == 0)
        );
        p.send(None, 1, 0, "127.0.0.1:1")?;
        anyhow::ensure!(p.receive()?);
        p.send(Some(&other), 1, 1, "127.0.0.1:2")?;
        anyhow::ensure!(!p.receive()? && p.detector.report(p.sender.me).is_some_and(|r| r.taken == 1));
        Ok(())
    }
    #[test]
    fn ad1_unrelated_socket_drains_do_not_generate_history_writes() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let identity = Identity::load_or_create(dir.path(), &crate::util::new_id())?;
        let wake = Arc::new(crate::rt::Notify::new());
        let mut plane = Plane::open(dir.path(), &identity, &wake)?;
        plane.shared.with(|live| -> Result<()> {
            live.attach(node_id("distant-peer"))?;
            live.on_durable(Write::Log, 0, 1_000);
            Ok(())
        }).context("detector did not open")??;
        let now = 1_000;
        plane.drive(now, false);
        let deadline = plane.wake.context("detector did not schedule the first pair")?.0;
        anyhow::ensure!(deadline > now + 1);
        let before = plane.shared.with(|live| live.history());
        for stamp in now + 1..deadline.min(now + 1_000) { plane.drive(stamp, false); }
        anyhow::ensure!(plane.shared.with(|live| live.history()) == before);
        plane.drive(deadline, false);
        anyhow::ensure!(plane.shared.with(|live| live.history()) != before);
        Ok(())
    }
    #[test]
    fn ad1_a_heartbeat_from_an_older_run_is_refused() -> Result<()> {
        let mut p = Pair::new()?;
        p.send(None, 3, 0, "127.0.0.1:1")?;
        anyhow::ensure!(p.receive()?);
        p.send(None, 2, 1, "127.0.0.1:1")?;
        anyhow::ensure!(
            !p.receive()? && p.detector.report(p.sender.me).is_some_and(|r| r.taken == 1)
        );
        Ok(())
    }
    #[test]
    fn ad1_a_restarted_receiver_requires_a_heartbeat_signed_for_its_run() -> Result<()> {
        let mut p = Pair::new()?;
        p.send_for(None, 5, 10, "127.0.0.1:1", 0)?;
        anyhow::ensure!(!p.receive()? && p.detector.report(p.sender.me).is_some_and(|r| r.taken == 0));
        p.send(None, 4, 11, "127.0.0.1:1")?;
        anyhow::ensure!(!p.receive()? && p.seen.changes.is_empty());
        p.send(None, 5, 12, "127.0.0.1:1")?;
        anyhow::ensure!(p.receive()? && p.detector.report(p.sender.me).is_some_and(|r| r.taken == 1));
        Ok(())
    }
    #[test]
    fn ad1_a_node_changing_address_stays_the_same_peer() -> Result<()> {
        let mut p = Pair::new()?;
        for (seq, address) in [(0, "127.0.0.1:1"), (1, "127.0.0.1:2")] {
            p.send(None, 1, seq, address)?;
            anyhow::ensure!(
                p.receive()?
                    && p.peers.get(&p.sender.me).and_then(|v| v.last) == Some(address.parse()?)
            );
        }
        anyhow::ensure!(
            p.detector
                .report(p.sender.me)
                .is_some_and(|r| r.groups == 1 && r.taken == 2)
        );
        Ok(())
    }
    #[test]
    fn ad1_a_full_heartbeat_queue_never_blocks_the_node() -> Result<()> {
        crate::rt::block_on(async {
            let mut p = Pair::new()?;
            let mut socket = Batched::bind(
                "127.0.0.1:0".parse::<SocketAddr>()?,
                hyper_rt::udp::Io { batch: 1 },
                true,
            )?;
            let mut output = Sent::default();
            output.heartbeat(p.sender.me, b"first");
            output.send(&p.receiver, &mut p.peers, &mut socket, |id| p.detector.trust(id).is_some());
            output.heartbeat(p.sender.me, b"full");
            output.send(&p.receiver, &mut p.peers, &mut socket, |id| p.detector.trust(id).is_some());
            output.heartbeat(p.sender.me, b"newest");
            output.flush();
            anyhow::ensure!(output.flush && socket.pending() && output.order.len() == 1 && output.pending.len() == 1);
            let packet = output.pending.get(&p.sender.me).context("missing held heartbeat")?;
            anyhow::ensure!(packet.bytes.get(..packet.len) == Some(b"newest".as_slice()));
            p.detector.detach(p.sender.me)?;
            let mut empty = Batched::bind("127.0.0.1:0".parse::<SocketAddr>()?, hyper_rt::udp::Io { batch: 1 }, true)?;
            output.send(&p.receiver, &mut p.peers, &mut empty, |id| p.detector.trust(id).is_some());
            anyhow::ensure!(!empty.pending() && output.order.is_empty() && output.pending.is_empty());
            Ok(())
        })
    }
}

//! The fleet's topology as each node learns it (LB08; docs/reach.md §2.3): `kiln_ci::topo` fed
//! with what flows anyway, and its estimates given to the asking node's owner.
//!
//! Where each sample comes from, none of them a probe:
//! - **Round trips**: each runner's QUIC connection to the server keeps its own smoothed round
//!   trip from the acknowledgements of what it sends (its keep-alives included, every 2 s); the
//!   runner reads it at its own host sample ([`read_now`]), as it reads its counters, and feeds it
//!   to its coordinate. The server places itself by the round trips the runners say in their words.
//!   A relayed link's own round trips are those of the answers its sends owe (LB03's detection on
//!   use, no ping: placement's and the hand-off's words and a link's features, said as soon as
//!   they're read): less its two legs, they're the relay's own time.
//! - **Rates**: reach's syncs, which say their bytes and how long they took (`local.rs`).
//! - **The runner's own network**: its interfaces' byte counters ([`Reader`], `sysinfo`: Linux,
//!   macOS and Windows, no privilege), at the same host sample.
//! - **Outages**: the server's own connects and disconnects of each runner ([`Fleet::up`],
//!   [`Fleet::down`]).
//!
//! Where it travels, nothing new sent: a runner's word ([`Word`], in the hint it sends whenever
//! its admission changes, `crate::offers::hint::Hint::topo`), and the server's directory
//! ([`Entry`], in each `RunnerInfo`, and the server's own coordinate beside the runners). A word
//! is never sent for the topology's sake: a coordinate that moved waits for the next hint, and is
//! aged by whoever holds it meanwhile.

use std::collections::BTreeMap;

use kiln_ci::topo::coord::Filter;
use kiln_ci::topo::{Announce, Node, domains, estimate, fixed, net, rate, sites, stability};
use serde::{Deserialize, Serialize};

/// The server's name among the topology's nodes.
pub const SERVER: &str = "server";

/// What a runner says of its topology in its word.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Word {
    /// Its coordinate, and how fast its links drift.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub c: Option<Announce>,
    /// Its link to the server as its QUIC connection measures it: filtered round trip and spread.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leg: Option<estimate::Leg>,
    /// Its own network, bytes a second: now and the most seen.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub net: Option<net::Word>,
    /// Its IP subnets (`addr/prefix`): a fact sites and failure domains take.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub subnets: Vec<String>,
}

/// What the directory says of a runner's topology.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Entry {
    #[serde(flatten)]
    pub word: Word,
    /// How old its word is, ms, as the server answered.
    pub age_ms: u64,
    /// Its link's drops (an hour, Q32) and mean outage (ms), from the server's connects and
    /// disconnects of it, the fleet's mean outage while its own are unmeasured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<stability::Path>,
    /// Its drops, and its time up, ms.
    pub drops: u64,
    pub up_ms: u64,
    pub domain: domains::Domain,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
}

/// One reading at a runner's host sample: its round trip to the server and its counters.
#[derive(Debug, Clone, Default)]
pub struct Read {
    pub at: i64,
    pub rtt_ns: Option<u64>,
    pub counters: Vec<net::Counters>,
    pub subnets: Vec<String>,
}

/// The interfaces' counters and subnets, through `sysinfo` on every system.
pub struct Reader {
    nets: sysinfo::Networks,
}

impl Default for Reader {
    fn default() -> Self {
        Reader { nets: sysinfo::Networks::new_with_refreshed_list() }
    }
}

/// Interfaces whose bytes never leave the machine: loopback, and the bridges and pairs that join
/// its containers and VMs to it (Docker, libvirt, CNI plugins, VMware, Apple's virtualization).
fn local_only(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "lo" || n.starts_with("lo0") || n.contains("loopback") || ["veth", "docker", "br-", "virbr", "cni", "flannel", "cali", "vmnet", "vmenet", "bridge"].iter().any(|p| n.starts_with(p))
}

/// `addr/prefix` of the network an address is on: its host bits cleared.
fn subnet(ip: std::net::IpAddr, prefix: u8) -> Option<String> {
    match ip {
        std::net::IpAddr::V4(a) if !a.is_loopback() && !a.is_link_local() && prefix <= 32 => {
            let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
            Some(format!("{}/{prefix}", std::net::Ipv4Addr::from(u32::from(a) & mask)))
        }
        std::net::IpAddr::V6(a) if !a.is_loopback() && (a.segments().first().copied().unwrap_or(0) & 0xffc0) != 0xfe80 && prefix <= 128 => {
            let mask = u128::MAX.checked_shl(128 - u32::from(prefix)).unwrap_or(0);
            Some(format!("{}/{prefix}", std::net::Ipv6Addr::from(u128::from(a) & mask)))
        }
        _ => None,
    }
}

impl Reader {
    /// The counters of every interface that carries the machine's own traffic, and its subnets.
    pub fn read(&mut self) -> (Vec<net::Counters>, Vec<String>) {
        self.nets.refresh(true);
        let mut counters = Vec::new();
        let mut subnets = Vec::new();
        for (name, data) in self.nets.list() {
            if local_only(name) {
                continue;
            }
            counters.push(net::Counters { name: name.clone(), rx: data.total_received(), tx: data.total_transmitted() });
            for n in data.ip_networks() {
                if let Some(s) = subnet(n.addr, n.prefix) {
                    subnets.push(s);
                }
            }
        }
        subnets.sort();
        subnets.dedup();
        (counters, subnets)
    }
}

/// A reading at `at`: the QUIC connection's smoothed round trip to the server, read from the
/// endpoint as the host sample is taken (blocks briefly: for the blocking pool), and the counters.
pub fn read_now(net: &crate::net::NetHandle, reader: &parking_lot::Mutex<Reader>, at: i64) -> Read {
    let rtt_ns = net
        .sample(crate::observe::kiln::QUIC_SAMPLE_WAIT)
        .and_then(|s| s.peers.into_iter().find(|p| p.name == SERVER).map(|p| u64::try_from(p.path.rtt.as_nanos()).unwrap_or(u64::MAX)))
        .filter(|&n| n > 0);
    let (counters, subnets) = reader.lock().read();
    Read { at, rtt_ns, counters, subnets }
}

/// What the asking node knows and works out: its own coordinate, network and transfers, and what
/// the directory says of the others.
#[derive(Debug, Clone)]
pub struct Here {
    me: String,
    node: Node,
    meter: net::Meter,
    subnets: Vec<String>,
    server: Option<Announce>,
    peers: BTreeMap<String, (Entry, i64)>,
    pairs: BTreeMap<String, rate::Pair>,
    relay: Filter,
    /// Each tree's syncs: the most one moved, and the last to each runner, bytes.
    syncs: BTreeMap<String, (u64, BTreeMap<String, u64>)>,
}

fn id(name: &str) -> u64 {
    fixed::id_of(name)
}

impl Here {
    pub fn new(me: &str) -> Here {
        Here { me: me.to_owned(), node: Node::new(id(me)), meter: net::Meter::default(), subnets: Vec::new(), server: None, peers: BTreeMap::new(), pairs: BTreeMap::new(), relay: Filter::default(), syncs: BTreeMap::new() }
    }

    /// A host sample's reading.
    pub fn read(&mut self, r: Read) {
        if let Some(rtt) = r.rtt_ns {
            let server = self.server.unwrap_or_default();
            self.node.sample(id(SERVER), server, rtt, r.at);
        }
        self.meter.read(r.at, r.counters, None);
        self.subnets = r.subnets;
    }

    /// The directory came: the server's coordinate and each runner's entry, at `now`.
    pub fn directory<'a>(&mut self, server: Option<Announce>, runners: impl IntoIterator<Item = (&'a str, Option<&'a Entry>)>, now: i64) {
        if server.is_some() {
            self.server = server;
        }
        self.peers = runners.into_iter().filter_map(|(n, e)| Some((n.to_owned(), (e?.clone(), now)))).collect();
    }

    /// One of its transfers to `peer` (a sync of tree `key`): `bytes` in `us`.
    pub fn transferred(&mut self, peer: &str, key: Option<&str>, bytes: u64, us: u64) {
        let rtt = self.latency(peer, 0).map_or(0, |l| l.us);
        self.pairs.entry(peer.to_owned()).or_default().add(rate::Transfer { bytes, us }, rtt);
        if let Some(k) = key {
            let (most, last) = self.syncs.entry(k.to_owned()).or_default();
            *most = (*most).max(bytes);
            last.insert(peer.to_owned(), bytes);
        }
    }

    /// A relayed link's own round trip to `peer` (reach's ping and its pong), ms: less the two
    /// legs, the relay's own time.
    pub fn pinged(&mut self, peer: &str, rtt_ms: u64) {
        let (Some(mine), Some(theirs)) = (self.leg(), self.peers.get(peer).and_then(|(e, _)| e.word.leg)) else { return };
        let legs = mine.rtt_ns.saturating_add(theirs.rtt_ns);
        self.relay.push(rtt_ms.saturating_mul(1_000_000).saturating_sub(legs).max(1));
    }

    fn leg(&self) -> Option<estimate::Leg> {
        let (rtt_ns, spread_ns) = self.node.link(id(SERVER))?;
        Some(estimate::Leg { rtt_ns, spread_ns, age_ms: 0 })
    }

    /// What it says of itself in its word.
    pub fn word(&self) -> Word {
        Word { c: Some(self.node.announce()), leg: self.leg(), net: Some(self.meter.word), subnets: self.subnets.clone() }
    }

    fn me_end(&self) -> estimate::End {
        let mine = self.peers.get(&self.me).map(|(e, _)| e);
        estimate::End { coord: Some((self.node.announce(), 0)), leg: self.leg(), path: mine.and_then(|e| e.path), access: self.meter.word.access, site: mine.and_then(|e| e.site.clone()) }
    }

    fn end(&self, peer: &str, now: i64) -> Option<estimate::End> {
        let (e, at) = self.peers.get(peer)?;
        let age = e.age_ms.saturating_add(u64::try_from(now.saturating_sub(*at)).unwrap_or(0));
        let leg = e.word.leg.map(|l| estimate::Leg { age_ms: age, ..l });
        Some(estimate::End { coord: e.word.c.map(|c| (c, age)), leg, path: e.path, access: e.word.net.map(|n| n.access).unwrap_or_default(), site: e.site.clone() })
    }

    fn relay_leg(&self) -> Option<estimate::Leg> {
        Some(estimate::Leg { rtt_ns: self.relay.value()?, spread_ns: self.relay.spread(), age_ms: 0 })
    }

    fn latency(&self, peer: &str, now: i64) -> Option<estimate::Latency> {
        estimate::latency(&self.me_end(), &self.end(peer, now)?, None, self.relay_leg())
    }

    /// Everything the owner is given for `peer`: latency, rate and outages ([`estimate::link`]).
    pub fn link(&self, peer: &str, now: i64) -> Option<estimate::Link> {
        if peer == self.me {
            return None;
        }
        let them = self.end(peer, now)?;
        let me = self.me_end();
        let rtt = estimate::latency(&me, &them, None, self.relay_leg()).map_or(0, |l| l.us);
        let own = self.pairs.get(peer).and_then(|p| p.fit(rtt));
        // Another runner of the same site, measured from here, stands for the path to it.
        let site = them.site.as_ref().and_then(|s| {
            self.pairs.iter().filter(|(n, _)| n.as_str() != peer && self.peers.get(n.as_str()).and_then(|(e, _)| e.site.as_ref()) == Some(s)).find_map(|(_, p)| p.fit(rtt))
        });
        let relay = self.peers.get(SERVER).and_then(|(e, _)| e.word.net.map(|n| n.access));
        Some(estimate::link(&me, &them, own.as_ref(), site.as_ref(), None, self.relay_leg(), relay.as_slice()))
    }

    /// The link between two other runners, as the directory lets this node work it out: their
    /// legs and the relay's time, their ends' rates. For locality: where a run's inputs are (a
    /// producing leg's runner, a warm cache's holder) against where it might run.
    pub fn between(&self, from: &str, to: &str, now: i64) -> Option<estimate::Link> {
        if from == self.me {
            return self.link(to, now);
        }
        let (a, b) = (self.end(from, now)?, self.end(to, now)?);
        Some(estimate::link(&a, &b, None, None, None, self.relay_leg(), &[]))
    }

    /// How long a run's inputs take to reach `candidate`, µs, each from where it is (none: a
    /// link's rate unknown).
    pub fn inputs_us(&self, inputs: &[estimate::Input], candidate: &str, now: i64) -> Option<u64> {
        estimate::inputs_us(inputs, candidate, |holder| if holder == candidate { None } else { self.between(holder, candidate, now) })
    }

    /// Every runner the directory knows but this one: its link.
    pub fn links(&self, now: i64) -> BTreeMap<String, estimate::Link> {
        self.peers.keys().filter_map(|p| Some((p.clone(), self.link(p, now)?))).collect()
    }

    /// What a sync of tree `key` to `peer` is expected to move, bytes, from reach's own records:
    /// its last sync of the tree there (what changed since is about what changed before), else the
    /// most any sync of it moved (a copy made from nothing).
    pub fn sync_bytes(&self, key: &str, peer: &str) -> Option<u64> {
        let (most, last) = self.syncs.get(key)?;
        last.get(peer).copied().or(Some(*most))
    }

    /// How long a sync of tree `key` to `peer` is expected to take, µs.
    pub fn sync_us(&self, key: &str, peer: &str, now: i64) -> Option<u64> {
        self.link(peer, now)?.transfer_us(self.sync_bytes(key, peer)?)
    }
}

/// The server's view of the fleet: its own coordinate, each runner's word, and each runner's
/// link's history.
#[derive(Debug, Clone)]
pub struct Fleet {
    node: Node,
    words: BTreeMap<String, (Word, i64)>,
    links: BTreeMap<String, stability::History>,
    co: domains::CoDrops,
    machines: BTreeMap<String, String>,
    /// The significance co-dropping runners are judged at (the person's setting; Q32).
    pub alpha: u64,
}

impl Default for Fleet {
    fn default() -> Self {
        Fleet { node: Node::new(id(SERVER)), words: BTreeMap::new(), links: BTreeMap::new(), co: domains::CoDrops::default(), machines: BTreeMap::new(), alpha: domains::DEFAULT_ALPHA }
    }
}

/// How long the transport takes, at most, to notice a link dropped: QUIC's idle timeout (the
/// endpoint's own setting, `crate::net::limits`), the window drops seen together fall in.
pub fn notice_ms() -> u64 {
    u64::try_from(crate::net::limits(true).idle_timeout.as_millis()).unwrap_or(u64::MAX)
}

impl Fleet {
    /// A runner's word, at `now`: kept for the directory, and its leg's round trip a sample of the
    /// server's own coordinate.
    pub fn heard(&mut self, runner: &str, w: &Word, now: i64) {
        if let (Some(c), Some(leg)) = (w.c, w.leg) {
            self.node.sample(id(runner), c, leg.rtt_ns, now);
        }
        self.words.insert(runner.to_owned(), (w.clone(), now));
    }

    /// Its runner process's machine.
    pub fn machine(&mut self, runner: &str, machine: &str) {
        self.machines.insert(runner.to_owned(), machine.to_owned());
    }

    /// A runner's link came up.
    pub fn up(&mut self, runner: &str, now: i64) {
        self.links.entry(runner.to_owned()).or_default().up(now);
    }

    /// A runner's link dropped (its last process gone).
    pub fn down(&mut self, runner: &str, now: i64) {
        self.links.entry(runner.to_owned()).or_default().down(now);
        self.co.dropped(runner, now, notice_ms());
    }

    /// The server started with these histories kept: their spans since it stopped are unwatched.
    pub fn restore(&mut self, links: BTreeMap<String, stability::History>, co: domains::CoDrops) {
        self.links = links;
        for h in self.links.values_mut() {
            // When it stopped isn't known: the span it watched last is credited to its start.
            h.away(h.at);
        }
        self.co = co;
    }

    /// A runner's history and the fleet's co-drops, to keep.
    pub fn kept(&self, runner: &str) -> (Option<String>, Option<String>) {
        (self.links.get(runner).and_then(|h| serde_json::to_string(h).ok()), serde_json::to_string(&self.co).ok())
    }

    /// The server's own coordinate.
    pub fn server(&self) -> Announce {
        self.node.announce()
    }

    /// Every runner's directory entry, at `now`: its word, its link's outages, its domains and site.
    pub fn entries(&self, now: i64) -> BTreeMap<String, Entry> {
        let pooled = stability::pooled_outage(self.links.values());
        let seen: BTreeMap<String, domains::Seen> = self.links.iter().map(|(r, h)| (r.clone(), domains::Seen { drops: h.drops, up_ms: h.up_ms_at(now) })).collect();
        let learned = domains::learn(&self.co, &seen, &self.machines, notice_ms(), self.alpha);
        let names: Vec<&String> = self.words.keys().collect();
        let members: Vec<sites::Member> = names.iter().map(|n| sites::Member { name: (*n).clone(), subnets: self.words.get(*n).map(|(w, _)| w.subnets.clone()).unwrap_or_default() }).collect();
        let end = |n: &str| -> Option<estimate::End> {
            let (w, at) = self.words.get(n)?;
            Some(estimate::End { coord: w.c.map(|c| (c, u64::try_from(now.saturating_sub(*at)).unwrap_or(0))), ..estimate::End::default() })
        };
        let site = sites::group(&members, |i, j| {
            let l = estimate::latency(&end(names.get(i)?)?, &end(names.get(j)?)?, None, None)?;
            Some((l.us, l.err_us?))
        });
        let mut out = BTreeMap::new();
        for (r, (w, at)) in &self.words {
            let h = self.links.get(r).copied().unwrap_or_default();
            let domain = domains::Domain { machine: self.machines.get(r).cloned(), subnets: w.subnets.clone(), learned: learned.get(r).cloned() };
            out.insert(
                r.clone(),
                Entry {
                    word: w.clone(),
                    age_ms: u64::try_from(now.saturating_sub(*at)).unwrap_or(0),
                    path: (h.up_ms_at(now) > 0).then(|| stability::Path::link(&h, now, pooled)),
                    drops: h.drops,
                    up_ms: h.up_ms_at(now),
                    domain,
                    site: site.get(r).cloned(),
                },
            );
        }
        out
    }
}

/// The owner's estimates as the kernel takes them, for one candidate: integers only.
pub fn kernel_terms(l: &estimate::Link, run_ms: u64, bytes: u64) -> (u64, u64, u64) {
    let latency = l.latency.map_or(0, |x| x.us);
    let transfer = l.transfer_us(bytes).unwrap_or(0);
    (latency, transfer, l.stall_ms(run_ms))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_ci::topo::Coord;

    const MS: u64 = 1_000_000;

    fn word(h_ns: i64, leg_ns: u64) -> Word {
        Word { c: Some(Announce { c: Coord { x: [0, 0], h: h_ns, e: 1 << 28 }, ..Default::default() }), leg: Some(estimate::Leg { rtt_ns: leg_ns, spread_ns: leg_ns / 10, age_ms: 0 }), net: Some(net::Word { rx: 0, tx: 0, access: rate::Access { peak_rx: 100_000_000, peak_tx: 40_000_000, link: None } }), subnets: vec!["192.168.1.0/24".into()] }
    }

    /// LB08: the server keeps each runner's word and its link's drops and outages, and says them in
    /// the directory with domains and sites; a restart's gap is nobody's outage.
    #[test]
    fn lb08_the_directory_says_each_runner_s_topology() -> anyhow::Result<()> {
        let mut f = Fleet::default();
        f.heard("a", &word(i64::try_from(MS).unwrap_or(0), 2 * MS), 0);
        f.heard("b", &word(i64::try_from(MS).unwrap_or(0), 3 * MS), 0);
        f.machine("a", "m1");
        f.up("a", 0);
        f.down("a", 600_000);
        f.up("a", 630_000);
        let e = f.entries(1_230_000);
        let a = e.get("a").ok_or_else(|| anyhow::anyhow!("no entry for a"))?;
        anyhow::ensure!(a.drops == 1 && a.up_ms == 1_200_000, "{a:?}");
        anyhow::ensure!(a.path.is_some_and(|p| p.outage_ms == 30_000), "{:?}", a.path);
        anyhow::ensure!(a.site == e.get("b").and_then(|b| b.site.clone()), "one subnet, one site");
        anyhow::ensure!(a.domain.machine.as_deref() == Some("m1"), "{:?}", a.domain);
        println!("a word {} bytes, a directory entry {} bytes, the server's coordinate {} bytes", serde_json::to_string(&a.word)?.len(), serde_json::to_string(a)?.len(), serde_json::to_string(&f.server())?.len());
        // A restart: what was kept comes back, and the time it was down counts for nothing.
        let (link, co) = f.kept("a");
        let mut g = Fleet::default();
        g.restore([("a".to_owned(), serde_json::from_str(&link.unwrap_or_default())?)].into_iter().collect(), serde_json::from_str(&co.unwrap_or_default())?);
        g.up("a", 9_000_000);
        g.heard("a", &word(0, 2 * MS), 9_000_000);
        let a = g.entries(9_000_000).remove("a").ok_or_else(|| anyhow::anyhow!("no entry after the restart"))?;
        anyhow::ensure!(a.drops == 1 && a.path.is_some_and(|p| p.outage_ms == 30_000), "{a:?}");
        Ok(())
    }

    /// LB08: the asking node's owner gets each runner's link: a relayed link's latency from its two
    /// legs, its rate from this node's own syncs there once it has some (else its ends' bound), its
    /// outages from both legs', and the next sync's expected size from reach's own records.
    #[test]
    fn lb08_the_owner_gets_each_runner_s_link() -> anyhow::Result<()> {
        let mut here = Here::new("me");
        for k in 0..8i64 {
            let moved = u64::try_from(k).unwrap_or(0) * 50_000_000;
            here.read(Read { at: k * 5_000, rtt_ns: Some(2 * MS), counters: vec![net::Counters { name: "en0".into(), rx: moved, tx: moved }], subnets: Vec::new() });
        }
        let entry = |w: Word, drops: u64| Entry { word: w, age_ms: 0, path: Some(stability::Path { per_hour: drops << 32, outage_ms: 20_000 }), drops, up_ms: 3_600_000, ..Entry::default() };
        let (peer, me) = (entry(word(0, 3 * MS), 2), entry(word(0, 2 * MS), 1));
        here.directory(Some(Announce::default()), [("peer", Some(&peer)), ("me", Some(&me))], 40_000);
        let l = here.link("peer", 40_000).ok_or_else(|| anyhow::anyhow!("no link to peer"))?;
        let lat = l.latency.ok_or_else(|| anyhow::anyhow!("no latency"))?;
        anyhow::ensure!(lat.source == estimate::Source::Legs && lat.us.abs_diff(5_000) <= 1, "{lat:?}");
        anyhow::ensure!(l.rate.is_some_and(|r| r.source == rate::Source::Edges && r.bps == 10_000_000), "{:?}", l.rate);
        anyhow::ensure!(l.outages.is_some_and(|p| p.per_hour == 3 << 32), "{:?}", l.outages);
        for (bytes, us) in [(40_000_000u64, 2_010_000u64), (4_000_000, 210_000), (400_000, 30_000)] {
            here.transferred("peer", Some("tree"), bytes, us);
        }
        let l = here.link("peer", 40_000).ok_or_else(|| anyhow::anyhow!("no link to peer"))?;
        anyhow::ensure!(l.rate.is_some_and(|r| r.source == rate::Source::Measured && r.bps.abs_diff(20_000_000) < 1_000_000), "{:?}", l.rate);
        anyhow::ensure!(here.sync_bytes("tree", "peer") == Some(400_000) && here.sync_bytes("tree", "other") == Some(40_000_000), "reach's own records");
        anyhow::ensure!(here.link("me", 40_000).is_none(), "no link to itself");
        // Locality: an input on `peer` costs its transfer to `me`'s neighbor; one on the candidate, nothing.
        let third = entry(word(0, 4 * MS), 0);
        here.directory(Some(Announce::default()), [("peer", Some(&peer)), ("me", Some(&me)), ("third", Some(&third))], 40_000);
        let input = [estimate::Input { on: "peer".into(), bytes: 40_000_000 }];
        anyhow::ensure!(here.inputs_us(&input, "peer", 40_000) == Some(0), "an input where it runs costs nothing");
        let far = here.inputs_us(&input, "third", 40_000).ok_or_else(|| anyhow::anyhow!("no estimate from peer to third"))?;
        anyhow::ensure!(far.abs_diff(1_000_000) < 1_000, "40 MB at the 40 MB/s peer sends: {far} µs");
        Ok(())
    }

    /// LB08: this machine's interfaces read as any user may, with their subnets.
    #[test]
    fn lb08_this_machine_s_interfaces_are_read_unprivileged() {
        let (counters, subnets) = Reader::default().read();
        println!("{} interfaces, subnets {subnets:?}", counters.len());
        assert!(counters.iter().all(|c| !local_only(&c.name)));
        assert_eq!(subnet("192.168.1.77".parse().unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)), 24), Some("192.168.1.0/24".to_owned()));
        assert_eq!(subnet("127.0.0.1".parse().unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)), 8), None);
    }
}

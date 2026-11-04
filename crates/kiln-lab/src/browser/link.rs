//! The browser lab's emulated links, as data, and the `ip` and `tc` commands that make them (goal 1
//! of the browser protocol program, plan 01a1a832e9cc steps 1 and 2; research f15 to f20, f25, f26).
//! Everything here is pure: a link spec in, command lines out, so each profile's commands are
//! tested without touching a namespace or a network.
//!
//! The layout (`docs/browser-lab.md`, "The machine"): a server namespace and K client namespaces,
//! each joined to the lab's own namespace (the link) by a veth pair. The link namespace routes
//! between them and shapes each client's link on its own side of it: the downlink on its egress
//! toward the client, the uplink on an ifb its ingress from the client is redirected to. Nothing is
//! shaped on loopback or on a host's own sending side (netem's caveat: f17). Each client also has an
//! unshaped control pair straight to the server namespace, for the clock's calibration.

use serde::{Deserialize, Serialize};

/// The largest frame on a veth of MTU 1500: what a full packet costs the link.
pub const FRAME: u64 = 1514;
pub const MTU: u32 = 1500;

/// How a direction loses packets.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "model")]
pub enum Loss {
    /// None.
    None,
    /// Each packet lost on its own at `pct` percent (the bar's model: f16).
    Random { pct: f64 },
    /// Gilbert-Elliott bursts with mean loss `pct` and bursts of `burst` packets on average
    /// (everything lost in the bad state, nothing in the good: f19). Outside the bar's cells unless
    /// Ada adds them.
    Bursts { pct: f64, burst: f64 },
}

impl Loss {
    /// Its name in a condition's (`0`, `1`, `5`, `ge1`).
    pub fn name(&self) -> String {
        match self {
            Loss::None => "0".to_owned(),
            Loss::Random { pct } => trim(*pct),
            Loss::Bursts { pct, .. } => format!("ge{}", trim(*pct)),
        }
    }

    /// netem's words for it.
    fn netem(&self) -> Vec<String> {
        match self {
            Loss::None => Vec::new(),
            Loss::Random { pct } => vec!["loss".into(), "random".into(), format!("{}%", trim(*pct))],
            Loss::Bursts { pct, burst } => {
                // Bad state loses everything (1-h = 100%), good nothing (1-k = 0%); r leaves bad
                // after `burst` packets on average, p enters it so the mean is `pct`.
                let l = (pct / 100.0).clamp(0.0, 0.99);
                let r = 1.0 / burst.max(1.0);
                let p = l * r / (1.0 - l);
                vec!["loss".into(), "gemodel".into(), format!("{}%", trim(p * 100.0)), format!("{}%", trim(r * 100.0)), "100%".into(), "0%".into()]
            }
        }
    }
}

fn trim(v: f64) -> String {
    let s = format!("{v:.4}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() { "0".to_owned() } else { s.to_owned() }
}

/// What UDP meets on a link (goal 5's fallbacks).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "udp")]
pub enum Udp {
    /// It passes as everything else does.
    #[default]
    Pass,
    /// Every UDP packet is dropped, both ways.
    Blocked,
    /// It passes until `after_s` seconds into the round, then is dropped both ways (a middlebox
    /// that lets a handshake through, then drops).
    DroppedAfter { after_s: u64 },
}

/// One direction of a link.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Way {
    pub rate_kbit: u64,
    /// One-way delay and its normal jitter (σ), in milliseconds.
    pub delay_ms: f64,
    pub jitter_ms: f64,
    /// How long a full queue holds at the rate, in milliseconds (the bufferbloat: f26).
    pub queue_ms: f64,
}

impl Way {
    /// A full packet's time on the wire, in milliseconds.
    pub fn packet_ms(&self) -> f64 {
        #[allow(clippy::cast_precision_loss)]
        let bits = (FRAME * 8) as f64;
        #[allow(clippy::cast_precision_loss)]
        let rate = self.rate_kbit.max(1) as f64;
        bits / rate
    }

    /// netem's `limit`, in packets. netem with a rate sends each packet at the later of its own
    /// delay and its predecessor's departure, plus its time on the wire (so nothing reorders), and
    /// counts every packet it holds, delayed or queued. Full, it holds `limit` full packets, so a
    /// packet let in leaves `limit` packet-times later: that is the one-way delay plus the queue,
    /// plus the packet's own time.
    pub fn limit(&self) -> u64 {
        let pt = self.packet_ms();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let n = ((self.queue_ms + self.delay_ms) / pt).round().max(0.0) as u64;
        n + 1
    }

    /// The queue in bytes that `limit` gives (full packets).
    pub fn queue_bytes(&self) -> u64 {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let bytes = (self.queue_ms / 1000.0 * (self.rate_kbit * 1000) as f64 / 8.0) as u64;
        bytes
    }
}

/// A network profile: both directions, and what UDP meets.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    pub down: Way,
    pub up: Way,
    #[serde(default)]
    pub udp: Udp,
    /// In the bar's cells (Ada's conditions), or one the lab offers beside them.
    pub in_bar: bool,
    /// What it stands for, a line.
    pub about: String,
}

impl Profile {
    /// The base round-trip time, in milliseconds.
    pub fn rtt_ms(&self) -> f64 {
        self.down.delay_ms + self.up.delay_ms
    }
}

fn way(rate_kbit: u64, delay_ms: f64, jitter_ms: f64, queue_ms: f64) -> Way {
    Way { rate_kbit, delay_ms, jitter_ms, queue_ms }
}

fn symmetric(name: &str, w: Way, in_bar: bool, about: &str) -> Profile {
    Profile { name: name.to_owned(), down: w.clone(), up: w, udp: Udp::Pass, in_bar, about: about.to_owned() }
}

/// Every profile the lab knows, the bar's first. New ones (goals 3, 5, 6, 7, 8) go here.
pub fn profiles() -> Vec<Profile> {
    let slow = |kbit| way(kbit, 400.0, 50.0, 4000.0);
    vec![
        symmetric("good", way(100_000, 10.0, 0.0, 100.0), true, "100 Mbit/s each way, 20 ms RTT, a queue of 100 ms"),
        symmetric("100k", slow(100), true, "100 kbit/s each way, 800 ms base RTT, jitter σ 50 ms each way, a queue of 4 s"),
        symmetric("50k", slow(50), true, "50 kbit/s each way, 800 ms base RTT, jitter σ 50 ms each way, a queue of 4 s"),
        Profile {
            name: "50k-asym".into(),
            down: slow(50),
            up: way(30, 400.0, 50.0, 4000.0),
            udp: Udp::Pass,
            in_bar: false,
            about: "50k with a 30 kbit/s uplink (WebPageTest's slow profiles send slower than they take: f27)".into(),
        },
        Profile { udp: Udp::Blocked, in_bar: false, about: "good, every UDP packet dropped (goal 5's fallback)".into(), ..symmetric("good-udp-blocked", way(100_000, 10.0, 0.0, 100.0), false, "") },
        Profile {
            udp: Udp::DroppedAfter { after_s: 5 },
            in_bar: false,
            about: "good, UDP dropped from 5 s into the round (a middlebox that passes the handshake: goal 5)".into(),
            ..symmetric("good-udp-dropped", way(100_000, 10.0, 0.0, 100.0), false, "")
        },
        Profile { udp: Udp::Blocked, in_bar: false, about: "50k, every UDP packet dropped (goal 5's fallback)".into(), ..symmetric("50k-udp-blocked", slow(50), false, "") },
    ]
}

/// The bar's loss rates (independent random loss each way: f16).
pub fn bar_losses() -> Vec<Loss> {
    vec![Loss::None, Loss::Random { pct: 1.0 }, Loss::Random { pct: 5.0 }]
}

/// A loss by its name (`0`, `1`, `5`, `ge1`: bursts of 3 packets).
pub fn loss_named(name: &str) -> Option<Loss> {
    if let Some(p) = name.strip_prefix("ge") {
        return p.parse().ok().map(|pct| Loss::Bursts { pct, burst: 3.0 });
    }
    let pct: f64 = name.parse().ok()?;
    Some(if pct == 0.0 { Loss::None } else { Loss::Random { pct } })
}

pub fn profile_named(name: &str) -> Option<Profile> {
    profiles().into_iter().find(|p| p.name == name)
}

/// A condition: a profile, its loss, and the seed its loss draws from this round (the same for
/// every target of the round, so each meets the same draws in order: f16, goal 6).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Condition {
    pub profile: Profile,
    pub loss: Loss,
    pub seed: u64,
}

impl Condition {
    /// `50k/1`, `good/0`.
    pub fn name(&self) -> String {
        format!("{}/{}", self.profile.name, self.loss.name())
    }

    /// The uplink's seed: its own, from the round's.
    pub fn up_seed(&self) -> u64 {
        self.seed ^ 0x9e37_79b9_7f4a_7c15
    }
}

/// netem's words for one direction.
pub fn netem(w: &Way, loss: &Loss, seed: u64) -> Vec<String> {
    let mut a: Vec<String> = vec!["netem".into(), "limit".into(), w.limit().to_string(), "delay".into(), format!("{}ms", trim(w.delay_ms))];
    if w.jitter_ms > 0.0 {
        a.extend([format!("{}ms", trim(w.jitter_ms)), "distribution".into(), "normal".into()]);
    }
    a.extend(loss.netem());
    a.extend(["rate".into(), format!("{}kbit", w.rate_kbit), "seed".into(), seed.to_string()]);
    a
}

fn words(s: &[&str]) -> Vec<String> {
    s.iter().map(|w| (*w).to_owned()).collect()
}

/// A command and the namespace it runs in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cmd {
    pub ns: Ns,
    pub argv: Vec<String>,
    /// Taking away what may not be there: its failure is no error.
    pub may_fail: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ns {
    /// The lab's own: the link that routes and shapes.
    Link,
    Server,
    Client(usize),
}

fn cmd(ns: Ns, argv: Vec<String>) -> Cmd {
    Cmd { ns, argv, may_fail: false }
}

/// Addresses: the server's on the link and on each control pair, and client `i`'s.
pub fn server_ip() -> &'static str {
    "10.77.0.1"
}
pub fn client_ip(i: usize) -> String {
    format!("10.78.{i}.2")
}
pub fn control_server_ip(i: usize) -> String {
    format!("10.79.{i}.1")
}
pub fn control_client_ip(i: usize) -> String {
    format!("10.79.{i}.2")
}
fn mac(kind: u8, i: usize, end: u8) -> String {
    format!("02:6b:{kind:02x}:{:02x}:{:02x}:{end:02x}", (i >> 8) & 0xff, i & 0xff)
}

/// What every interface is set to before it carries anything: no offloads (netem counts and loses
/// whole packets, so a 64 KB segment must not pass as one: f20), no IPv6 (no router solicitations
/// or neighbour discovery drawing on a seeded loss).
fn quiet(ns: Ns, dev: &str, ethtool: bool) -> Vec<Cmd> {
    let mut out = vec![cmd(ns, words(&["ip", "link", "set", "dev", dev, "mtu", &MTU.to_string(), "addrgenmode", "none"]))];
    if ethtool {
        out.push(cmd(ns, words(&["ethtool", "-K", dev, "tso", "off", "gso", "off", "gro", "off", "tx", "off", "rx", "off"])));
    } else {
        out.push(cmd(ns, words(&["ip", "link", "set", "dev", dev, "gso_max_segs", "1"])));
    }
    out
}

/// The commands that make the lab's network for `clients` clients, the server's and each client's
/// namespace given by the PID holding it (`server_pid`, `client_pids`). `ethtool` says whether
/// offloads are turned off by ethtool (else by `ip`'s GSO bounds).
pub fn topology(server_pid: u32, client_pids: &[u32], ethtool: bool) -> Vec<Cmd> {
    let mut out = Vec::new();
    let all_ns = std::iter::once(Ns::Link).chain(std::iter::once(Ns::Server)).chain((0..client_pids.len()).map(Ns::Client));
    for ns in all_ns {
        out.push(cmd(ns, words(&["sysctl", "-qw", "net.ipv6.conf.all.disable_ipv6=1", "net.ipv6.conf.default.disable_ipv6=1"])));
        out.push(cmd(ns, words(&["ip", "link", "set", "lo", "up"])));
    }
    out.push(cmd(Ns::Link, words(&["sysctl", "-qw", "net.ipv4.ip_forward=1"])));
    // The server's side of the link.
    let (s_link, s_srv) = (mac(0, 0, 0xfe), mac(0, 0, 0x01));
    out.push(cmd(Ns::Link, words(&["ip", "link", "add", "s0", "address", &s_link, "type", "veth", "peer", "name", "eth0", "address", &s_srv, "netns", &server_pid.to_string()])));
    out.extend(quiet(Ns::Link, "s0", ethtool));
    out.extend(quiet(Ns::Server, "eth0", ethtool));
    out.push(cmd(Ns::Link, words(&["ip", "addr", "add", "10.77.0.254/24", "dev", "s0"])));
    out.push(cmd(Ns::Link, words(&["ip", "link", "set", "s0", "up"])));
    out.push(cmd(Ns::Link, words(&["ip", "neigh", "replace", server_ip(), "lladdr", &s_srv, "dev", "s0", "nud", "permanent"])));
    out.push(cmd(Ns::Server, words(&["ip", "addr", "add", "10.77.0.1/24", "dev", "eth0"])));
    out.push(cmd(Ns::Server, words(&["ip", "link", "set", "eth0", "up"])));
    out.push(cmd(Ns::Server, words(&["ip", "neigh", "replace", "10.77.0.254", "lladdr", &s_link, "dev", "eth0", "nud", "permanent"])));
    out.push(cmd(Ns::Server, words(&["ip", "route", "add", "10.78.0.0/16", "via", "10.77.0.254"])));
    for (i, pid) in client_pids.iter().enumerate() {
        let (c, ifb) = (format!("c{i}"), format!("ifb{i}"));
        let (c_link, c_cli) = (mac(1, i, 0xfe), mac(1, i, 0x02));
        let gw = format!("10.78.{i}.1");
        out.push(cmd(Ns::Link, words(&["ip", "link", "add", &c, "address", &c_link, "type", "veth", "peer", "name", "eth0", "address", &c_cli, "netns", &pid.to_string()])));
        out.extend(quiet(Ns::Link, &c, ethtool));
        out.extend(quiet(Ns::Client(i), "eth0", ethtool));
        out.push(cmd(Ns::Link, words(&["ip", "addr", "add", &format!("{gw}/24"), "dev", &c])));
        out.push(cmd(Ns::Link, words(&["ip", "link", "set", &c, "up"])));
        out.push(cmd(Ns::Link, words(&["ip", "neigh", "replace", &client_ip(i), "lladdr", &c_cli, "dev", &c, "nud", "permanent"])));
        out.push(cmd(Ns::Client(i), words(&["ip", "addr", "add", &format!("{}/24", client_ip(i)), "dev", "eth0"])));
        out.push(cmd(Ns::Client(i), words(&["ip", "link", "set", "eth0", "up"])));
        out.push(cmd(Ns::Client(i), words(&["ip", "neigh", "replace", &gw, "lladdr", &c_link, "dev", "eth0", "nud", "permanent"])));
        out.push(cmd(Ns::Client(i), words(&["ip", "route", "add", "default", "via", &gw])));
        // The uplink's ifb: what comes in from the client is shaped on its way out of it.
        out.push(cmd(Ns::Link, words(&["ip", "link", "add", &ifb, "type", "ifb"])));
        out.push(cmd(Ns::Link, words(&["ip", "link", "set", &ifb, "up"])));
        out.push(cmd(Ns::Link, words(&["tc", "qdisc", "add", "dev", &c, "handle", "ffff:", "ingress"])));
        out.push(cmd(Ns::Link, words(&["tc", "filter", "add", "dev", &c, "parent", "ffff:", "prio", "10", "matchall", "action", "mirred", "egress", "redirect", "dev", &ifb])));
        // The control pair, unshaped, straight between the client and the server.
        let (k_srv, k_cli) = (mac(2, i, 0x01), mac(2, i, 0x02));
        out.push(cmd(Ns::Server, words(&["ip", "link", "add", &format!("k{i}"), "address", &k_srv, "type", "veth", "peer", "name", "ctl", "address", &k_cli, "netns", &pid.to_string()])));
        out.extend(quiet(Ns::Server, &format!("k{i}"), ethtool));
        out.extend(quiet(Ns::Client(i), "ctl", ethtool));
        out.push(cmd(Ns::Server, words(&["ip", "addr", "add", &format!("{}/24", control_server_ip(i)), "dev", &format!("k{i}")])));
        out.push(cmd(Ns::Server, words(&["ip", "link", "set", &format!("k{i}"), "up"])));
        out.push(cmd(Ns::Server, words(&["ip", "neigh", "replace", &control_client_ip(i), "lladdr", &k_cli, "dev", &format!("k{i}"), "nud", "permanent"])));
        out.push(cmd(Ns::Client(i), words(&["ip", "addr", "add", &format!("{}/24", control_client_ip(i)), "dev", "ctl"])));
        out.push(cmd(Ns::Client(i), words(&["ip", "link", "set", "ctl", "up"])));
        out.push(cmd(Ns::Client(i), words(&["ip", "neigh", "replace", &control_server_ip(i), "lladdr", &k_srv, "dev", "ctl", "nud", "permanent"])));
    }
    out
}

/// The commands that put condition `c` on client `i`'s link (replacing what was there), all in
/// the link namespace.
pub fn shape(i: usize, c: &Condition) -> Vec<Cmd> {
    let (dev, ifb) = (format!("c{i}"), format!("ifb{i}"));
    let mut down = words(&["tc", "qdisc", "replace", "dev", &dev, "root"]);
    down.extend(netem(&c.profile.down, &c.loss, c.seed));
    let mut up = words(&["tc", "qdisc", "replace", "dev", &ifb, "root"]);
    up.extend(netem(&c.profile.up, &c.loss, c.up_seed()));
    let mut out = vec![cmd(Ns::Link, down), cmd(Ns::Link, up)];
    out.extend(udp_rules(i, c.profile.udp == Udp::Blocked));
    out
}

/// The filters that drop UDP on client `i`'s link both ways (`on`), or that take them off. The
/// uplink's sits before the ifb's redirect; the downlink's on an ingress of the server's side.
pub fn udp_rules(i: usize, on: bool) -> Vec<Cmd> {
    let dev = format!("c{i}");
    let del = Cmd { may_fail: true, ..cmd(Ns::Link, words(&["tc", "filter", "del", "dev", &dev, "parent", "ffff:", "prio", "1"])) };
    if !on {
        let del_down = Cmd { may_fail: true, ..cmd(Ns::Link, words(&["tc", "filter", "del", "dev", "s0", "parent", "ffff:", "prio", &(i + 1).to_string()])) };
        return vec![del, del_down];
    }
    let drop_up = cmd(Ns::Link, words(&["tc", "filter", "add", "dev", &dev, "parent", "ffff:", "prio", "1", "protocol", "ip", "u32", "match", "ip", "protocol", "17", "0xff", "action", "drop"]));
    let drop_down = cmd(
        Ns::Link,
        words(&["tc", "filter", "replace", "dev", "s0", "parent", "ffff:", "prio", &(i + 1).to_string(), "protocol", "ip", "u32", "match", "ip", "protocol", "17", "0xff", "match", "ip", "dst", &format!("{}/32", client_ip(i)), "action", "drop"]),
    );
    vec![del, drop_up, cmd(Ns::Link, words(&["tc", "qdisc", "replace", "dev", "s0", "handle", "ffff:", "ingress"])), drop_down]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn join(c: &Cmd) -> String {
        c.argv.join(" ")
    }

    fn cond(name: &str, loss: &str) -> Condition {
        Condition { profile: profile_named(name).unwrap_or_else(|| symmetric("missing", way(1, 0.0, 0.0, 0.0), false, "")), loss: loss_named(loss).unwrap_or(Loss::None), seed: 7 }
    }

    /// plan 01a1a832e9cc step 1: each of the bar's profiles renders to the commands it should, and
    /// building them touches nothing.
    #[test]
    fn g1_s1_every_bar_profile_renders_its_netem() {
        let want = [
            ("good", "0", "tc qdisc replace dev c3 root netem limit 909 delay 10ms rate 100000kbit seed 7"),
            ("good", "1", "tc qdisc replace dev c3 root netem limit 909 delay 10ms loss random 1% rate 100000kbit seed 7"),
            ("100k", "5", "tc qdisc replace dev c3 root netem limit 37 delay 400ms 50ms distribution normal loss random 5% rate 100kbit seed 7"),
            ("50k", "1", "tc qdisc replace dev c3 root netem limit 19 delay 400ms 50ms distribution normal loss random 1% rate 50kbit seed 7"),
        ];
        for (p, l, down) in want {
            let c = cond(p, l);
            let cmds = shape(3, &c);
            assert_eq!(cmds.first().map(join).as_deref(), Some(down), "{p}/{l}");
            let up = cmds.get(1).map(join).unwrap_or_default();
            assert!(up.starts_with("tc qdisc replace dev ifb3 root netem"), "{up}");
            assert!(up.ends_with(&format!("seed {}", c.up_seed())), "{up}");
            assert!(cmds.iter().all(|c| c.ns == Ns::Link));
        }
    }

    /// A full queue holds what the profile says: 4 s at 50 and 100 kbit/s, 100 ms on the good
    /// link, within a packet's time.
    #[test]
    fn g1_s2_a_full_queue_holds_its_time() {
        for p in profiles().into_iter().filter(|p| p.in_bar) {
            let w = &p.down;
            #[allow(clippy::cast_precision_loss)]
            let held = w.limit() as f64 * w.packet_ms() - w.delay_ms - w.packet_ms();
            assert!((held - w.queue_ms).abs() <= w.packet_ms(), "{}: {held} ms against {}", p.name, w.queue_ms);
        }
        assert_eq!(profile_named("50k").map(|p| p.down.queue_bytes()), Some(25_000));
        assert_eq!(profile_named("100k").map(|p| p.down.queue_bytes()), Some(50_000));
    }

    #[test]
    fn g1_s2_bursty_loss_keeps_its_mean() {
        let l = Loss::Bursts { pct: 1.0, burst: 3.0 };
        let w = l.netem();
        assert_eq!(w.get(..2).map(<[String]>::to_vec), Some(vec!["loss".to_owned(), "gemodel".to_owned()]));
        // p/(p+r) is the share of time in the bad state, all of it lost.
        let num = |s: Option<&String>| s.and_then(|s| s.trim_end_matches('%').parse::<f64>().ok()).unwrap_or(0.0);
        let (p, r) = (num(w.get(2)), num(w.get(3)));
        assert!((p / (p + r) - 0.01).abs() < 1e-3, "{w:?}");
        assert_eq!(loss_named("ge1"), Some(l));
        assert_eq!(loss_named("0"), Some(Loss::None));
    }

    #[test]
    fn g1_s1_the_topology_routes_and_pins_its_neighbours() {
        let t = topology(100, &[200, 201], true);
        let lines: Vec<String> = t.iter().map(join).collect();
        assert!(lines.contains(&"ip link add c1 address 02:6b:01:00:01:fe type veth peer name eth0 address 02:6b:01:00:01:02 netns 201".to_owned()), "{lines:#?}");
        assert!(lines.contains(&"tc filter add dev c0 parent ffff: prio 10 matchall action mirred egress redirect dev ifb0".to_owned()));
        assert!(lines.iter().any(|l| l == "ethtool -K s0 tso off gso off gro off tx off rx off"));
        // Every neighbour is pinned: no ARP draws on a seeded loss.
        assert_eq!(lines.iter().filter(|l| l.contains("nud permanent")).count(), 2 + 2 * 4);
        assert!(t.iter().filter(|c| c.ns == Ns::Client(1)).any(|c| join(c) == "ip route add default via 10.78.1.1"));
        let no_ethtool = topology(100, &[200], false);
        assert!(no_ethtool.iter().map(join).any(|l| l == "ip link set dev eth0 gso_max_segs 1"));
    }

    #[test]
    fn g1_s2_udp_can_be_blocked_both_ways() {
        let c = cond("good-udp-blocked", "0");
        let lines: Vec<String> = shape(2, &c).iter().map(join).collect();
        assert!(lines.iter().any(|l| l.contains("dev c2 parent ffff: prio 1 protocol ip u32 match ip protocol 17 0xff action drop")), "{lines:#?}");
        assert!(lines.iter().any(|l| l.contains("dev s0 parent ffff: prio 3") && l.contains("10.78.2.2/32")), "{lines:#?}");
        let open: Vec<String> = shape(2, &cond("good", "0")).iter().map(join).collect();
        assert!(!open.iter().any(|l| l.contains("action drop")));
    }
}

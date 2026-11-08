//! The topology's tests (LB08): coordinates converging on synthetic fleets (one LAN; three sites
//! over a WAN; a node behind a slow uplink; a flapping link), their prediction error against the
//! papers' (Dabek et al. 2004: a median relative error of 11 % with two dimensions and a height),
//! determinism, and one planted mistake per rule, each caught.
#![allow(clippy::float_arithmetic, clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]

use std::collections::BTreeMap;

use super::coord::{Announce, Coord, Node};
use super::fixed::{self, ONE};
use super::rate::{self, Access, Pair, Source, Transfer};
use super::stability::{History, Path};
use super::{Mutant, domains, estimate, net, planted, sites};

/// Dabek et al. 2004's median relative error for two dimensions and a height (abstract; §5.4).
const DABEK_MEDIAN: f64 = 0.11;
const US: i64 = 1_000;
const MS: i64 = 1_000_000;

/// A fleet as it truly is: each node's place and its access link's delay, ns.
#[derive(Clone)]
struct Truth {
    at: Vec<(i64, i64)>,
    h: Vec<i64>,
}

impl Truth {
    fn rtt(&self, i: usize, j: usize) -> u64 {
        let (Some(a), Some(b)) = (self.at.get(i), self.at.get(j)) else { return 0 };
        let (dx, dy) = ((a.0 - b.0) as f64, (a.1 - b.1) as f64);
        let plane = (dx * dx + dy * dy).sqrt() as i64;
        (plane + self.h.get(i).copied().unwrap_or(0) + self.h.get(j).copied().unwrap_or(0)).max(1) as u64
    }
    fn len(&self) -> usize {
        self.at.len()
    }
}

fn draw(s: &mut u64) -> u64 {
    fixed::splitmix(s)
}

/// A uniform draw in `[lo, hi)`.
fn uniform(s: &mut u64, lo: i64, hi: i64) -> i64 {
    let span = (hi - lo).max(1) as u64;
    lo + (draw(s) % span) as i64
}

/// Sites as centers (ns) with `per` nodes each, spread `spread` around them, heights in `[h0, h1)`.
fn fleet(seed: u64, centers: &[(i64, i64)], per: usize, spread: i64, h0: i64, h1: i64) -> Truth {
    let mut s = seed;
    let mut at = Vec::new();
    let mut h = Vec::new();
    for c in centers {
        for _ in 0..per {
            at.push((c.0 + uniform(&mut s, -spread, spread), c.1 + uniform(&mut s, -spread, spread)));
            h.push(uniform(&mut s, h0, h1));
        }
    }
    Truth { at, h }
}

/// How samples are drawn: each round, each node samples another, its round trip off by up to
/// `jitter` (relative, either way). Node `flap.0`'s link flaps: down a third of the time (60
/// rounds of every 180, nothing sampled on it), and every `flap.1`th of its samples ten times its
/// truth (Ledlie et al.'s "rare but persistent" anomalies, as around a link's drops).
#[derive(Clone, Copy)]
struct Draws {
    rounds: usize,
    jitter: f64,
    flap: Option<(usize, usize)>,
    /// Only the server (node 0) is sampled, as today's star of links.
    star: bool,
    /// Node 0 hears its first site-mate this share of the time (a skewed neighbor set).
    skew: Option<f64>,
}

fn run(t: &Truth, d: Draws, seed: u64) -> Vec<Node> {
    let n = t.len();
    let mut nodes: Vec<Node> = (0..n).map(|i| Node::new(fixed::id_of(&format!("n{i}")))).collect();
    let mut s = seed;
    let mut count = vec![0usize; n];
    for round in 0..d.rounds {
        let now = (round as i64) * 1000;
        for i in 0..n {
            let j = if d.star {
                if i == 0 { 1 + (draw(&mut s) as usize) % (n - 1) } else { 0 }
            } else if let (Some(p), 0) = (d.skew, i) {
                if (draw(&mut s) % 1000) as f64 / 1000.0 < p { 1 } else { 2 + (draw(&mut s) as usize) % (n - 2) }
            } else {
                let k = (draw(&mut s) as usize) % (n - 1);
                if k >= i { k + 1 } else { k }
            };
            if let Some((who, _)) = d.flap
                && (i == who || j == who)
                && (round / 60) % 3 == 2
            {
                continue;
            }
            let truth = t.rtt(i, j) as f64;
            let noise = 1.0 + d.jitter * ((draw(&mut s) % 2001) as f64 / 1000.0 - 1.0);
            let mut rtt = truth * noise;
            if let Some((who, every)) = d.flap
                && (i == who || j == who)
                && let Some(c) = count.get_mut(who)
            {
                *c += 1;
                if (*c).is_multiple_of(every) {
                    rtt *= 10.0;
                }
            }
            let theirs = nodes.get(j).map(Node::announce).unwrap_or_default();
            let peer = fixed::id_of(&format!("n{j}"));
            if let Some(me) = nodes.get_mut(i) {
                me.sample(peer, theirs, rtt.max(1.0) as u64, now);
            }
        }
    }
    nodes
}

/// Relative errors of every pair's prediction against its truth, sorted.
fn errors(t: &Truth, nodes: &[Node], pairs: impl Fn(usize, usize) -> bool) -> Vec<f64> {
    let mut out = Vec::new();
    for i in 0..t.len() {
        for j in (i + 1)..t.len() {
            if !pairs(i, j) {
                continue;
            }
            let (Some(a), Some(b)) = (nodes.get(i), nodes.get(j)) else { continue };
            let truth = t.rtt(i, j) as f64;
            out.push((a.coord.rtt(&b.coord) as f64 - truth).abs() / truth);
        }
    }
    out.sort_by(f64::total_cmp);
    out
}

fn quantile(v: &[f64], q: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let i = ((v.len() - 1) as f64 * q).round() as usize;
    v.get(i).copied().unwrap_or(f64::NAN)
}

const MESH: Draws = Draws { rounds: 400, jitter: 0.05, flap: None, star: false, skew: None };

fn lan() -> Truth {
    fleet(1, &[(0, 0)], 8, 100 * US, 20 * US, 120 * US)
}

fn three_sites() -> Truth {
    fleet(2, &[(0, 0), (12 * MS, 0), (5 * MS, 20 * MS)], 4, 100 * US, 20 * US, 120 * US)
}

fn slow_uplink() -> Truth {
    let mut t = fleet(3, &[(0, 0), (12 * MS, 0)], 4, 100 * US, 20 * US, 120 * US);
    if let Some(h) = t.h.get_mut(5) {
        *h += 15 * MS;
    }
    t
}

/// Each synthetic fleet's median and 90th percentile, against Dabek's 11 %.
#[test]
fn lb08_coordinates_converge_on_synthetic_fleets() {
    let cases: [(&str, Truth, Draws); 4] = [
        ("one LAN", lan(), MESH),
        ("three sites over a WAN", three_sites(), MESH),
        ("a node behind a slow uplink", slow_uplink(), MESH),
        ("a flapping link", three_sites(), Draws { flap: Some((3, 20)), ..MESH }),
    ];
    for (name, t, d) in cases {
        let nodes = run(&t, d, 7);
        let e = errors(&t, &nodes, |_, _| true);
        let (med, p90) = (quantile(&e, 0.5), quantile(&e, 0.9));
        let odd = match name { "a node behind a slow uplink" => Some(5), "a flapping link" => Some(3), _ => None };
        let rest = errors(&t, &nodes, |i, j| odd.is_none_or(|o| i != o && j != o));
        let mut abs: Vec<f64> = (0..t.len()).flat_map(|i| ((i + 1)..t.len()).map(move |j| (i, j))).filter_map(|(i, j)| Some((nodes.get(i)?.coord.rtt(&nodes.get(j)?.coord) as f64 - t.rtt(i, j) as f64).abs() / 1e6)).collect();
        abs.sort_by(f64::total_cmp);
        println!("{name}: median {:.1} %, 90th {:.1} % over {} pairs (90th {:.2} ms off); without its odd node {:.1} %, {:.1} %", med * 100.0, p90 * 100.0, e.len(), quantile(&abs, 0.9), quantile(&rest, 0.5) * 100.0, quantile(&rest, 0.9) * 100.0);
        assert!(med < DABEK_MEDIAN, "{name}: median relative error {med:.3} is past Dabek's 11 %");
    }
}

/// Today's star (every runner's one link the server): legs measured make a relayed link's latency
/// as good as its measurements, while the runners' own coordinates vouch for nothing they never
/// measured.
fn star_holds() -> bool {
    let t = three_sites();
    let nodes = run(&t, Draws { star: true, ..MESH }, 9);
    let (Some(a), Some(b), Some(srv)) = (nodes.get(5), nodes.get(9), nodes.first()) else { return false };
    let leg = |n: &Node, peer: &Node| n.link(peer.id).map(|(rtt, spread)| estimate::Leg { rtt_ns: rtt, spread_ns: spread, age_ms: 0 });
    let end = |n: &Node, l: Option<estimate::Leg>| estimate::End { coord: Some((n.announce(), 0)), leg: l, ..Default::default() };
    let (ea, eb) = (end(a, leg(a, srv)), end(b, leg(b, srv)));
    let Some(lat) = estimate::latency(&ea, &eb, None, None) else { return false };
    let truth = (t.rtt(5, 0) + t.rtt(9, 0)) as f64 / 1000.0;
    let off = (lat.us as f64 - truth).abs() / truth;
    println!("star: relayed {} µs against {truth:.0} µs ({:.1} % off), {:?}", lat.us, off * 100.0, lat.source);
    let coords = estimate::latency(&estimate::End { leg: None, ..ea }, &estimate::End { leg: None, ..eb }, None, None);
    lat.source == estimate::Source::Legs && off < 0.1 && a.announce().v.is_none() && coords.is_some_and(|c| c.err_us.is_none())
}

#[test]
fn lb08_a_relayed_link_is_its_legs() {
    assert!(star_holds());
    assert!(planted(Some(Mutant::LegsIgnored), || !star_holds()), "LegsIgnored not caught");
}

/// The error estimate tracks the error: converged, no node takes itself as wrong by half.
fn error_learned() -> bool {
    let nodes = run(&three_sites(), MESH, 11);
    nodes.iter().all(|n| n.coord.e < ONE / 2)
}

#[test]
fn lb08_a_node_learns_how_wrong_it_is() {
    assert!(error_learned());
    assert!(planted(Some(Mutant::ErrorFrozen), || !error_learned()), "ErrorFrozen not caught");
}

/// Neighbor decay: a node that hears one neighbor most of the time still places itself by the
/// others it hears rarely (Ledlie et al. 2007 §4.2's skewed neighbor sets).
fn decay_holds() -> f64 {
    let t = three_sites();
    let nodes = run(&t, Draws { skew: Some(0.95), ..MESH }, 13);
    let e = errors(&t, &nodes, |i, _| i == 0);
    quantile(&e, 0.5)
}

#[test]
fn lb08_rare_neighbors_keep_their_pull() {
    let (with, without) = (decay_holds(), planted(Some(Mutant::OnlyLatest), decay_holds));
    println!("skewed: median {:.1} % with decay, {:.1} % without", with * 100.0, without * 100.0);
    assert!(with < DABEK_MEDIAN && without > with, "OnlyLatest not caught: {with} against {without}");
}

/// Drift: a link whose round trip wanders makes an old coordinate less sure than a fresh one.
fn drift_measured() -> bool {
    let mut n = Node::new(1);
    let peer = Announce { c: Coord { x: [0, 0], h: 100 * US, e: ONE / 20 }, ..Announce::default() };
    let mut s = 5u64;
    let mut rtt = 1_000_000f64;
    for k in 0..3600i64 {
        // A random walk of 0.5 % a step, sampled at irregular times.
        rtt *= 1.0 + 0.005 * ((draw(&mut s) % 2001) as f64 / 1000.0 - 1.0);
        let at = k * 5000 + uniform(&mut s, 0, 4000);
        n.sample(2, peer, rtt as u64, at);
    }
    let a = n.announce();
    a.d > 0 && a.c.aged(a.d, 3_600_000) > a.c.e
}

#[test]
fn lb08_an_old_coordinate_grows_less_sure() {
    assert!(drift_measured());
    assert!(planted(Some(Mutant::DriftIgnored), || !drift_measured()), "DriftIgnored not caught");
}

/// A pair's rate follows a level shift (He et al. 2005 §V.B): 100 MB/s, then 10 MB/s.
fn shift_followed() -> bool {
    let mut p = Pair::default();
    let rtt = 2_000;
    let at = |bps: u64, bytes: u64| Transfer { bytes, us: 2_000 + bytes * 1_000_000 / bps };
    for k in 0..6u64 {
        p.add(at(100_000_000, (k + 1) * 8_000_000), rtt);
    }
    for k in 0..4u64 {
        p.add(at(10_000_000, (k + 1) * 3_000_000), rtt);
    }
    p.fit(rtt).is_some_and(|f| f.rate_bps.abs_diff(10_000_000) < 4_000_000)
}

#[test]
fn lb08_a_pair_s_rate_follows_a_shift() {
    assert!(shift_followed());
    assert!(planted(Some(Mutant::NoLevelShift), || !shift_followed()), "NoLevelShift not caught");
}

/// Hockney's fit recovers a pair's fixed cost and rate from transfers of mixed sizes.
#[test]
fn lb08_a_pair_s_fit_is_its_cost_and_rate() {
    let mut p = Pair::default();
    for (k, bytes) in [10_000u64, 2_000_000, 40_000, 9_000_000, 600_000].into_iter().enumerate() {
        let jitter = [0i64, 300, -200, 500, -100].get(k).copied().unwrap_or(0);
        p.add(Transfer { bytes, us: (3_000 + bytes / 50 + jitter.unsigned_abs()) }, 1_000);
    }
    let f = p.fit(1_000);
    println!("fit: {f:?}");
    assert!(f.is_some_and(|f| f.rate_bps.abs_diff(50_000_000) < 2_000_000 && f.fixed_us.abs_diff(3_000) < 500));
}

/// A pair never seen: its sites' measured pair, held to its ends; else its ends' bound.
fn site_used() -> bool {
    let fast = Access { peak_rx: 100_000_000, peak_tx: 100_000_000, link: None };
    let site = rate::Fit { fixed_us: 5_000, rate_bps: 20_000_000, sd_us: 0, samples: 4 };
    let r = rate::estimate(None, Some(&site), &fast, &fast, &[]);
    let edges = rate::estimate(None, None, &fast, &Access { peak_rx: 50_000_000, ..fast }, &[]);
    r.is_some_and(|r| r.source == Source::Site && r.bps == 20_000_000) && edges.is_some_and(|e| e.source == Source::Edges && e.bps == 50_000_000)
}

#[test]
fn lb08_a_pair_never_seen_is_bounded() {
    assert!(site_used());
    assert!(planted(Some(Mutant::SiteIgnored), || !site_used()), "SiteIgnored not caught");
}

/// A flapping link: down every ten minutes for thirty seconds.
fn flapping() -> History {
    let mut h = History::default();
    let mut t = 0i64;
    for _ in 0..12 {
        h.up(t);
        t += 600_000;
        h.down(t);
        t += 30_000;
    }
    h.up(t);
    h
}

fn stall_counted() -> bool {
    let h = flapping();
    let now = 12 * 630_000 + 1;
    let p = h.p_drop(600_000, now);
    let stall = h.stall_ms(600_000, now, None);
    println!("flapping: {:.2} drops an hour, outage {:?} ms, P(drop in 10 min) {:.3}, stall {stall} ms", h.rate(now).map_or(0.0, |(r, _)| r as f64 / ONE as f64), h.mean_outage_ms(), p as f64 / ONE as f64);
    // 1 − e^(−1) = 0.632 of a 30 s outage.
    stall.abs_diff(18_964) < 200 && h.mean_outage_ms() == Some(30_000)
}

#[test]
fn lb08_a_flapping_link_s_stall_is_its_drops_times_outages() {
    assert!(stall_counted());
    assert!(planted(Some(Mutant::StallIgnoresDrops), || !stall_counted()), "StallIgnoresDrops not caught");
    // A relayed path: legs' rates add, the outage weighted by them.
    let p = Path::of(&[Path { per_hour: 6 * ONE, outage_ms: 30_000 }, Path { per_hour: 2 * ONE, outage_ms: 10_000 }]);
    assert_eq!(p, Path { per_hour: 8 * ONE, outage_ms: 25_000 });
    // No drop in an hour up: rate 0, its bound three an hour (the rule of three).
    let mut quiet = History::default();
    quiet.up(0);
    assert_eq!(quiet.rate(3_600_000), Some((0, 3 * ONE)));
}

/// Domains: two runners on one switch drop together five times among independent others; the
/// others' chance coincidences join nothing.
fn domains_hold() -> bool {
    let mut co = domains::CoDrops::default();
    let mut seen: BTreeMap<String, domains::Seen> = BTreeMap::new();
    let day = 86_400_000i64;
    let window = 10_000u64;
    let mut s = 21u64;
    let mut drops: Vec<(i64, String)> = Vec::new();
    for r in 0..10 {
        for _ in 0..6 {
            drops.push((uniform(&mut s, 0, day), format!("r{r}")));
        }
    }
    for k in 0..5i64 {
        let at = k * (day / 5) + 1_000;
        drops.push((at, "sw-a".into()));
        drops.push((at + 3_000, "sw-b".into()));
    }
    drops.sort();
    for (at, r) in &drops {
        co.dropped(r, *at, window);
        let e = seen.entry(r.clone()).or_default();
        e.drops += 1;
        e.up_ms = u64::try_from(day).unwrap_or(0);
    }
    let learned = domains::learn(&co, &seen, &BTreeMap::new(), window, domains::DEFAULT_ALPHA);
    let joined = learned.iter().filter(|(r, d)| r != d).count();
    learned.get("sw-b").map(String::as_str) == Some("sw-a") && joined == 1
}

#[test]
fn lb08_what_fails_together_is_one_domain() {
    assert!(domains_hold());
    assert!(planted(Some(Mutant::EveryCoDropCounts), || !domains_hold()), "EveryCoDropCounts not caught");
    // One machine is one domain whatever its drops.
    let seen: BTreeMap<String, domains::Seen> = [("a", 1), ("b", 1)].into_iter().map(|(r, n)| (r.to_owned(), domains::Seen { drops: n, up_ms: 1000 })).collect();
    let machines = [("a".to_owned(), "m1".to_owned()), ("b".to_owned(), "m1".to_owned())].into_iter().collect();
    let d = domains::learn(&domains::CoDrops::default(), &seen, &machines, 10_000, domains::DEFAULT_ALPHA);
    assert_eq!(d.get("b").map(String::as_str), Some("a"));
}

/// Sites: three sites found in coordinate space; one noisy LAN stays one.
fn sites_hold() -> bool {
    let t = three_sites();
    let nodes = run(&t, MESH, 17);
    let members: Vec<sites::Member> = (0..t.len()).map(|i| sites::Member { name: format!("n{i:02}"), subnets: Vec::new() }).collect();
    let edge = |i: usize, j: usize| {
        let (a, b) = (nodes.get(i)?.announce(), nodes.get(j)?.announce());
        let l = estimate::latency(&estimate::End { coord: Some((a, 0)), ..Default::default() }, &estimate::End { coord: Some((b, 0)), ..Default::default() }, None, None)?;
        Some((l.us, l.err_us?))
    };
    let found = sites::group(&members, edge);
    let distinct: std::collections::BTreeSet<&String> = found.values().collect();
    // One LAN whose tree's edges differ by less than their error: no cut.
    let lan = [(100u64, 80u64), (120, 80), (260, 80)];
    let four: Vec<sites::Member> = (0..4).map(|i| sites::Member { name: format!("l{i}"), subnets: Vec::new() }).collect();
    let chain = |i: usize, j: usize| if j == i + 1 { lan.get(i).copied() } else { None };
    let one = sites::group(&four, chain);
    let lan_sites: std::collections::BTreeSet<&String> = one.values().collect();
    println!("sites: {} found in three sites, {} in one LAN", distinct.len(), lan_sites.len());
    distinct.len() == 3 && lan_sites.len() == 1
}

#[test]
fn lb08_sites_are_clusters_the_error_can_tell() {
    assert!(sites_hold());
    assert!(planted(Some(Mutant::CutInsideNoise), || !sites_hold()), "CutInsideNoise not caught");
}

/// A runner's network rate from its counters: the busiest interface, peaks kept until the
/// interfaces change.
#[test]
fn lb08_a_runner_s_network_rate_is_its_busiest_interface() {
    let c = |name: &str, rx: u64, tx: u64| net::Counters { name: name.into(), rx, tx };
    let mut m = net::Meter::default();
    m.read(0, vec![c("en0", 0, 0), c("utun3", 0, 0)], None);
    m.read(1_000, vec![c("en0", 50_000_000, 5_000_000), c("utun3", 48_000_000, 4_000_000)], None);
    m.read(2_000, vec![c("en0", 60_000_000, 6_000_000), c("utun3", 57_000_000, 5_000_000)], None);
    assert_eq!((m.word.rx, m.word.tx, m.word.access.peak_rx, m.word.access.peak_tx), (10_000_000, 1_000_000, 50_000_000, 5_000_000));
    assert_eq!(m.word.free(), (40_000_000, 4_000_000));
    // Another network: learned again.
    m.read(3_000, vec![c("en1", 0, 0)], Some(125_000_000));
    assert_eq!(m.word.access.peak_rx, 0);
}

/// Determinism: one fleet's run gives the same coordinates bit for bit, here and on every machine
/// (the digest below is what this run gives on arm64 and x86-64 alike).
#[test]
fn lb08_coordinates_are_deterministic() {
    let digest = |nodes: &[Node]| {
        let mut h = String::new();
        for n in nodes {
            h.push_str(&format!("{:?}{}{}|", n.coord.x, n.coord.h, n.coord.e));
        }
        fixed::id_of(&h)
    };
    let (a, b) = (digest(&run(&three_sites(), MESH, 23)), digest(&run(&three_sites(), MESH, 23)));
    println!("digest {a:#018x}");
    assert_eq!(a, b);
    assert_eq!(a, DIGEST, "a different machine, or a changed update, gave other coordinates");
}

/// What [`lb08_coordinates_are_deterministic`] gives.
const DIGEST: u64 = 0xe6a0_0d0a_40bd_32f6;

/// The costs the report quotes: one update, one round-trip estimate, one rate for a pair never
/// seen. `cargo test -p kiln-ci --release topo::tests::lb08_costs -- --ignored --nocapture`.
#[test]
#[ignore = "timing, for the report"]
fn lb08_costs() {
    use std::time::Instant;
    let t = three_sites();
    let mut nodes = run(&t, MESH, 29);
    let theirs = nodes.get(1).map(Node::announce).unwrap_or_default();
    let n = 1_000_000u64;
    let Some(me) = nodes.get_mut(0) else { return };
    let start = Instant::now();
    for k in 0..n {
        me.sample(fixed::id_of("n1"), theirs, 1_000_000 + (k % 7) * 1000, 400_000 + i64::try_from(k).unwrap_or(0));
    }
    let update = start.elapsed().as_nanos() as f64 / n as f64;
    let (a, b) = (me.coord, theirs.c);
    let start = Instant::now();
    let mut acc = 0u64;
    for k in 0..n {
        acc = acc.wrapping_add(std::hint::black_box(&a).rtt(std::hint::black_box(&b)) ^ k);
    }
    let rtt = start.elapsed().as_nanos() as f64 / n as f64;
    let end = Access { peak_rx: 100_000_000, peak_tx: 50_000_000, link: Some(125_000_000) };
    let start = Instant::now();
    for _ in 0..n {
        acc = acc.wrapping_add(rate::estimate(None, None, std::hint::black_box(&end), &end, &[end]).map_or(0, |r| r.bps));
    }
    let never = start.elapsed().as_nanos() as f64 / n as f64;
    let word = serde_json::to_string(&me.announce()).map_or(0, |s| s.len());
    println!("update {update:.0} ns ({} neighbors), rtt {rtt:.1} ns, rate never seen {never:.1} ns; announce {word} bytes ({acc})", me.neighbors().len());
}

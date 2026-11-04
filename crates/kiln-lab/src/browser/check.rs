//! Each profile checked by measurement before any protocol is measured on it (plan 01a1a832e9cc
//! step 2): a TCP bulk transfer's throughput against the set rate, idle pings' median RTT against
//! the base RTT, a full queue's delay against its set time, and seeded loss, its rate inside the
//! 99% binomial band and its dropped indices the same for the same seed.

use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::link::{self, Condition, Loss, Ns, Profile};
use super::tools::{Pinged, Sunk};
use super::topo::Lab;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProfileCheck {
    pub profile: String,
    /// TCP bulk throughput over 60 s (from 5 s in), frames on the client's interface, against the
    /// set rate: 95 to 101 passes.
    pub throughput_pct: f64,
    pub throughput_ok: bool,
    pub rtt_base_ms: f64,
    pub rtt_median_ms: f64,
    pub rtt_pings: usize,
    pub rtt_off_pct: f64,
    pub rtt_ok: bool,
    /// A full queue's delay (a packet's one-way delay with the queue full, less an idle full
    /// packet's), against the set queue: within 10% passes. Slow profiles only.
    pub queue_set_ms: f64,
    pub queue_ms: Option<f64>,
    pub queue_ok: Option<bool>,
    pub reordered: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LossCheck {
    pub direction: String,
    pub pct: f64,
    pub seed: u64,
    pub sent: u64,
    pub lost: u64,
    pub band: (u64, u64),
    pub inside: bool,
    /// Whether the dropped indices equal the first run's of the same seed (none for the first).
    pub same_as_first: Option<bool>,
    /// How many dropped indices it shares with seed one's first run (another seed only).
    pub shared_with_first: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Checks {
    pub profiles: Vec<ProfileCheck>,
    pub losses: Vec<LossCheck>,
    pub pass: bool,
    pub network: Vec<String>,
}

/// The 99% band of a Binomial(n, p) count, by the normal approximation (n p ≥ 1,000 here).
pub fn band99(n: u64, p: f64) -> (u64, u64) {
    #[allow(clippy::cast_precision_loss)]
    let nf = n as f64;
    let mean = nf * p;
    let sd = (nf * p * (1.0 - p)).sqrt();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let lo = (mean - 2.576 * sd).floor().max(0.0) as u64;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let hi = (mean + 2.576 * sd).ceil() as u64;
    (lo, hi)
}

fn median(v: &mut [f64]) -> Option<f64> {
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n == 0 {
        return None;
    }
    Some(f64::midpoint(*v.get((n - 1) / 2)?, *v.get(n / 2)?))
}

struct Tools<'a> {
    lab: &'a Lab,
    exe: std::path::PathBuf,
}

impl Tools<'_> {
    fn spawn(&self, ns: Ns, args: &[String]) -> anyhow::Result<Child> {
        let mut argv = vec![self.exe.to_string_lossy().into_owned(), "browser".into(), "tool".into()];
        argv.extend(args.iter().cloned());
        Ok(self.lab.command(ns, &argv)?.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?)
    }

    fn done<T: for<'de> Deserialize<'de>>(child: Child) -> anyhow::Result<T> {
        let out = child.wait_with_output()?;
        anyhow::ensure!(out.status.success(), "a lab tool failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        let text = String::from_utf8_lossy(&out.stdout);
        Ok(serde_json::from_str(text.lines().last().unwrap_or_default())?)
    }
}

fn s(v: impl ToString) -> String {
    v.to_string()
}

fn addr(ip: &str, port: u16) -> anyhow::Result<SocketAddr> {
    Ok(format!("{ip}:{port}").parse()?)
}

/// Runs every check on the bar's profiles, one client link each, at once (so they also show the
/// links don't disturb each other), then the loss checks on the first: everything to `out`.
pub fn run(out: &Path, seed: u64) -> anyhow::Result<Checks> {
    let profiles: Vec<Profile> = link::profiles().into_iter().filter(|p| p.in_bar).collect();
    let mut lab = Lab::up(profiles.len())?;
    for (i, p) in profiles.iter().enumerate() {
        lab.shape(i, &Condition { profile: p.clone(), loss: Loss::None, seed })?;
    }
    let tools = Tools { lab: &lab, exe: std::env::current_exe()? };
    // Throughput: a bulk transfer from the server to each client, 70 s, the window from 5 s.
    eprintln!("kiln-lab: throughput, 70 s on {} links", profiles.len());
    let mut sinks = Vec::new();
    for i in 0..profiles.len() {
        sinks.push(tools.spawn(Ns::Client(i), &[s("tcp-sink"), s("--bind"), format!("{}:5001", link::client_ip(i))])?);
    }
    std::thread::sleep(Duration::from_millis(300));
    let mut sources = Vec::new();
    for i in 0..profiles.len() {
        sources.push(tools.spawn(Ns::Server, &[s("tcp-source"), s("--to"), format!("{}:5001", link::client_ip(i)), s("--secs"), s(70)])?);
    }
    std::thread::sleep(Duration::from_secs(5));
    let before: Vec<_> = (0..profiles.len()).map(|i| lab.counters(i)).collect::<anyhow::Result<_>>()?;
    let t1 = Instant::now();
    std::thread::sleep(Duration::from_secs(60));
    let after: Vec<_> = (0..profiles.len()).map(|i| lab.counters(i)).collect::<anyhow::Result<_>>()?;
    let window = t1.elapsed().as_secs_f64();
    for c in sources {
        let _: serde_json::Value = Tools::done(c)?;
    }
    for c in sinks {
        let _: serde_json::Value = Tools::done(c)?;
    }
    // RTT: pings from each client to an echo on the server, on idle links.
    eprintln!("kiln-lab: pings, 1,000 on each link");
    let mut echo = tools.spawn(Ns::Server, &[s("udp-echo"), s("--bind"), s("0.0.0.0:5002")])?;
    std::thread::sleep(Duration::from_millis(300));
    let mut pings = Vec::new();
    for (i, p) in profiles.iter().enumerate() {
        let interval = if p.rtt_ms() < 100.0 { 20 } else { 100 };
        pings.push(tools.spawn(Ns::Client(i), &[s("udp-ping"), s("--to"), s(addr(link::server_ip(), 5002)?), s("--count"), s(1000), s("--interval-ms"), s(interval)])?);
    }
    let pinged: Vec<Pinged> = pings.into_iter().map(Tools::done).collect::<anyhow::Result<_>>()?;
    let _ = echo.kill();
    let _ = echo.wait();
    // A full queue: full frames at twice the rate for 60 s, from the server to each slow client.
    eprintln!("kiln-lab: full queues, 60 s");
    let mut queue_sinks = Vec::new();
    for (i, p) in profiles.iter().enumerate().filter(|(_, p)| p.down.queue_ms >= 1000.0) {
        #[allow(clippy::cast_precision_loss)]
        let pps = 2.0 * (p.down.rate_kbit * 1000) as f64 / 8.0 / link::FRAME as f64;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let count = (pps * 60.0) as u64;
        let sink = tools.spawn(Ns::Client(i), &[s("udp-sink"), s("--bind"), format!("{}:5003", link::client_ip(i)), s("--count"), s(count), s("--quiet-ms"), s(15_000), s("--delays")])?;
        queue_sinks.push((i, pps, count, sink));
    }
    std::thread::sleep(Duration::from_millis(300));
    let mut blasts = Vec::new();
    for (i, pps, count, _) in &queue_sinks {
        // 1472 bytes of UDP: a 1514-byte frame.
        blasts.push(tools.spawn(Ns::Server, &[s("udp-blast"), s("--to"), format!("{}:5003", link::client_ip(*i)), s("--count"), s(count), s("--pps"), s(pps), s("--size"), s(1472)])?);
    }
    for b in blasts {
        let _: serde_json::Value = Tools::done(b)?;
    }
    let mut queues = std::collections::BTreeMap::new();
    for (i, _, _, sink) in queue_sinks {
        let sunk: Sunk = Tools::done(sink)?;
        queues.insert(i, sunk);
    }
    let mut checks = Vec::new();
    for (i, p) in profiles.iter().enumerate() {
        let (b, a) = (before.get(i).copied().unwrap_or_default(), after.get(i).copied().unwrap_or_default());
        #[allow(clippy::cast_precision_loss)]
        let got_bits = a.since(b).rx_bytes as f64 * 8.0 / window;
        #[allow(clippy::cast_precision_loss)]
        let throughput_pct = got_bits / (p.down.rate_kbit * 1000) as f64 * 100.0;
        let pg = pinged.get(i).cloned().unwrap_or_default();
        #[allow(clippy::cast_precision_loss)]
        let mut rtts: Vec<f64> = pg.rtts_us.iter().map(|r| *r as f64 / 1000.0).collect();
        let rtt_median_ms = median(&mut rtts).unwrap_or(f64::NAN);
        let rtt_off_pct = (rtt_median_ms - p.rtt_ms()).abs() / p.rtt_ms() * 100.0;
        let (queue_ms, queue_ok, reordered) = match queues.get(&i) {
            Some(q) => {
                // The plateau: the last third of the packets that came.
                let n = q.delays_us.len();
                #[allow(clippy::cast_precision_loss)]
                let mut tail: Vec<f64> = q.delays_us.iter().skip(n * 2 / 3).map(|d| *d as f64 / 1000.0).collect();
                let full = median(&mut tail).unwrap_or(f64::NAN);
                let held = full - p.down.delay_ms - p.down.packet_ms();
                (Some(held), Some((held - p.down.queue_ms).abs() <= p.down.queue_ms * 0.10), q.reordered)
            }
            None => (None, None, 0),
        };
        checks.push(ProfileCheck {
            profile: p.name.clone(),
            throughput_pct,
            throughput_ok: (95.0..=101.0).contains(&throughput_pct),
            rtt_base_ms: p.rtt_ms(),
            rtt_median_ms,
            rtt_pings: pg.rtts_us.len(),
            rtt_off_pct,
            rtt_ok: rtt_off_pct <= 5.0,
            queue_set_ms: p.down.queue_ms,
            queue_ms,
            queue_ok,
            reordered,
        });
    }
    // Loss, on the first (good) link: 100,000 numbered packets each way at 1% and 5%, seed one
    // twice, then another seed once.
    let losses = loss_checks(&mut lab, profiles.first().cloned().ok_or_else(|| anyhow::anyhow!("no profiles"))?, seed)?;
    let pass = checks.iter().all(|c| c.throughput_ok && c.rtt_ok && c.queue_ok != Some(false)) && losses.iter().all(|l| l.inside && l.same_as_first != Some(false) && l.shared_with_first.is_none_or(|n| n < l.lost / 2));
    let out_checks = Checks { profiles: checks, losses, pass, network: lab.log.clone() };
    std::fs::create_dir_all(out)?;
    std::fs::write(out.join("profile-checks.json"), serde_json::to_vec_pretty(&out_checks)?)?;
    std::fs::write(out.join("profile-checks.md"), markdown(&out_checks))?;
    Ok(out_checks)
}

fn loss_checks(lab: &mut Lab, good: Profile, seed: u64) -> anyhow::Result<Vec<LossCheck>> {
    const N: u64 = 100_000;
    let exe = std::env::current_exe()?;
    let other = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
    let mut out = Vec::new();
    for pct in [1.0, 5.0] {
        for direction in ["down", "up"] {
            let mut first: Option<Vec<u64>> = None;
            for (run, sd) in [(0, seed), (1, seed), (2, other)] {
                eprintln!("kiln-lab: loss {pct}% {direction}, seed {sd}");
                lab.shape(0, &Condition { profile: good.clone(), loss: Loss::Random { pct }, seed: sd })?;
                let tools = Tools { lab, exe: exe.clone() };
                let (sink_ns, sink_ip, blast_ns) = if direction == "down" { (Ns::Client(0), link::client_ip(0), Ns::Server) } else { (Ns::Server, link::server_ip().to_owned(), Ns::Client(0)) };
                let sink = tools.spawn(sink_ns, &[s("udp-sink"), s("--bind"), format!("{sink_ip}:5004"), s("--count"), s(N), s("--quiet-ms"), s(3000)])?;
                std::thread::sleep(Duration::from_millis(300));
                let blast = tools.spawn(blast_ns, &[s("udp-blast"), s("--to"), format!("{sink_ip}:5004"), s("--count"), s(N), s("--pps"), s(20_000)])?;
                let _: serde_json::Value = Tools::done(blast)?;
                let sunk: Sunk = Tools::done(sink)?;
                let lost = sunk.missing.len() as u64;
                let band = band99(N, pct / 100.0);
                let (same_as_first, shared_with_first) = match (&first, run) {
                    (None, _) => (None, None),
                    (Some(f), 1) => (Some(*f == sunk.missing), None),
                    (Some(f), _) => {
                        let set: std::collections::HashSet<&u64> = f.iter().collect();
                        (None, Some(sunk.missing.iter().filter(|m| set.contains(m)).count() as u64))
                    }
                };
                if first.is_none() {
                    first = Some(sunk.missing.clone());
                }
                out.push(LossCheck { direction: direction.to_owned(), pct, seed: sd, sent: N, lost, band, inside: (band.0..=band.1).contains(&lost), same_as_first, shared_with_first });
            }
        }
    }
    Ok(out)
}

pub fn markdown(c: &Checks) -> String {
    use std::fmt::Write as _;
    let mut o = String::new();
    let _ = writeln!(o, "| profile | throughput (% of rate) | median RTT (ms) against base | off | full queue (ms) against set | reordered |\n| --- | --- | --- | --- | --- | --- |");
    for p in &c.profiles {
        let q = p.queue_ms.map_or_else(|| "-".to_owned(), |q| format!("{q:.0} against {:.0}{}", p.queue_set_ms, if p.queue_ok == Some(true) { "" } else { " (off)" }));
        let _ = writeln!(
            o,
            "| {} | {:.1}{} | {:.1} against {:.0} ({} pings) | {:.1}%{} | {q} | {} |",
            p.profile,
            p.throughput_pct,
            if p.throughput_ok { "" } else { " (off)" },
            p.rtt_median_ms,
            p.rtt_base_ms,
            p.rtt_pings,
            p.rtt_off_pct,
            if p.rtt_ok { "" } else { " (off)" },
            p.reordered
        );
    }
    let _ = writeln!(o, "\n| loss | way | seed | lost of 100,000 | 99% band | same indices as seed's first run | shared with the first seed's |\n| --- | --- | --- | --- | --- | --- | --- |");
    for l in &c.losses {
        let same = l.same_as_first.map_or_else(|| "-".to_owned(), |s| if s { "yes".to_owned() } else { "**no**".to_owned() });
        let shared = l.shared_with_first.map_or_else(|| "-".to_owned(), |n| n.to_string());
        let _ = writeln!(o, "| {}% | {} | {} | {}{} | {} to {} | {same} | {shared} |", l.pct, l.direction, l.seed, l.lost, if l.inside { "" } else { " (outside)" }, l.band.0, l.band.1);
    }
    let _ = writeln!(o, "\n{}", if c.pass { "Every check passed." } else { "**Not every check passed.**" });
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn g1_s2_the_binomial_band() {
        assert_eq!(band99(100_000, 0.01), (918, 1082));
        let (lo, hi) = band99(100_000, 0.05);
        assert!(lo < 5000 && hi > 5000 && hi - lo < 400, "{lo} {hi}");
    }
}

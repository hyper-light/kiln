//! Probes: each target checked on a thread of its own, at the interval the engine derives for it
//! (`kiln_ci::headroom::detect::interval_us`), so one that hangs (an fsync stuck behind a whole file
//! system's writeback) is seen hanging while it hangs, by how long it's been out, instead of
//! holding up the others or the engine. A probe gives up at its target's window (its tolerance, but
//! for a target that answers with a level it measured): by then it is harm already, and the next
//! one starts.

use std::io::{Read as _, Write as _};
use std::net::{TcpStream, ToSocketAddrs as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use super::settings::Probe;
use kiln_ci::headroom::Answer;

/// One target to probe.
#[derive(Clone, Debug, PartialEq)]
pub struct Spec {
    pub name: String,
    pub probe: Probe,
    /// How long a probe may be out: its target's window (`super::settings::Target::window_ms`).
    pub window: Duration,
}

struct Slot {
    name: String,
    /// When the probe out now started (µs since `base`), 0 when none is.
    inflight: AtomicU64,
    /// How long to wait between probes (µs), as the engine last said.
    every_us: AtomicU64,
    answers: Mutex<Vec<Answer>>,
}

/// The probes of one set of targets; dropping it stops them.
pub struct Probes {
    pub specs: Vec<Spec>,
    slots: Vec<Arc<Slot>>,
    stop: Arc<AtomicBool>,
    base: Instant,
}

impl Probes {
    /// A thread per target, each probing at once and then at its interval (until the engine
    /// says, once per window); `wake` is called when one answers.
    pub fn start(specs: Vec<Spec>, wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let base = Instant::now();
        let mut slots = Vec::new();
        for spec in &specs {
            let slot = Arc::new(Slot { name: spec.name.clone(), inflight: AtomicU64::new(0), every_us: AtomicU64::new(spec.window.as_micros() as u64), answers: Mutex::new(Vec::new()) });
            slots.push(Arc::clone(&slot));
            let (probe, window, stop, wake) = (spec.probe.clone(), spec.window, Arc::clone(&stop), Arc::clone(&wake));
            let spawned = std::thread::Builder::new().name(format!("kiln-probe-{}", spec.name.chars().take(8).collect::<String>())).spawn(move || {
                let mut seen = Seen::default();
                while !stop.load(Ordering::Relaxed) {
                    slot.inflight.store((base.elapsed().as_micros() as u64).max(1), Ordering::Release);
                    let at = Instant::now();
                    let answer = match once(&probe, window, &mut seen) {
                        // Out its window is an answer too: that long.
                        Ok(None) | Err(Failed::Slow) => Answer::Took(at.elapsed().as_micros() as f64),
                        // What the target measured (the server's delay), however long asking took.
                        Ok(Some(us)) => Answer::Took(us),
                        Err(Failed::Down) => Answer::Unavailable,
                    };
                    slot.inflight.store(0, Ordering::Release);
                    slot.answers.lock().push(answer);
                    wake();
                    std::thread::sleep(Duration::from_micros(slot.every_us.load(Ordering::Acquire)));
                }
            });
            if let Err(e) = spawned {
                tracing::warn!(error = %e, target = %spec.name, "headroom: a probe couldn't start");
            }
        }
        Self { specs, slots, stop, base }
    }

    /// The answers since the last call, and how long each probe out now has been out (µs).
    pub fn drain(&self) -> (Vec<(String, Answer)>, Vec<(String, f64)>) {
        let now = self.base.elapsed().as_micros() as u64;
        let mut answers = Vec::new();
        let mut out = Vec::new();
        for s in &self.slots {
            answers.extend(std::mem::take(&mut *s.answers.lock()).into_iter().map(|a| (s.name.clone(), a)));
            let since = s.inflight.load(Ordering::Acquire);
            if since > 0 {
                out.push((s.name.clone(), now.saturating_sub(since) as f64));
            }
        }
        (answers, out)
    }

    /// Probe `name` every `every_us` from now.
    pub fn set_interval(&self, name: &str, every_us: f64) {
        if let Some(s) = self.slots.iter().find(|s| s.name == name)
            && every_us.is_finite()
            && every_us >= 1.0
        {
            s.every_us.store(every_us as u64, Ordering::Release);
        }
    }

    /// The shortest interval any target is probed at (µs).
    pub fn fastest_us(&self) -> u64 {
        self.slots.iter().map(|s| s.every_us.load(Ordering::Acquire)).min().unwrap_or(1_000_000).max(1)
    }
}

impl Drop for Probes {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Why a probe got no answer.
enum Failed {
    /// It ran out its target's window: slowness (harm), not a failure.
    Slow,
    /// Refused or failed: down for its own reasons.
    Down,
}

/// What a probe keeps between its tries: the fsync's sequence number, and the server's totals at
/// the last read of its delay.
#[derive(Default)]
struct Seen {
    seq: u64,
    /// The server's probes' lateness added up (s), and how many ran, at the last read.
    totals: Option<(f64, f64)>,
}

/// One probe of `probe`: what the target says it measured (µs), where it says.
fn once(probe: &Probe, window: Duration, seen: &mut Seen) -> Result<Option<f64>, Failed> {
    match probe {
        Probe::Fsync { path } => fsync(path, &mut seen.seq).map(|()| None).map_err(|_| Failed::Down),
        Probe::Http { url } => http(url, window).map(|()| None),
        Probe::Tcp { addr } => connect(addr, window).map(|_| None),
        Probe::Delay { url } => delay(url, window).and_then(|now| since(&mut seen.totals, &now)).map(Some),
        // Measured on what agents are handed (`super::messages`), never probed.
        Probe::Messages => Err(Failed::Down),
    }
}

/// A kiln server's figures for its delay (`/healthz/delay`, its fields in seconds).
#[derive(Debug, Clone, Copy, PartialEq, serde::Deserialize)]
struct Delay {
    late_s: f64,
    probes: f64,
    held_s: f64,
    period_s: f64,
}

/// How late the server's ready work ran on average since the last read (µs), every probe of every
/// shard counted, in periods of this build's own (`crate::shard_health::PERIOD`, the target's
/// tolerance): a server asked for another period is measured against its own. A shard held by a
/// long poll counts once its probe runs, its lateness in the total: a hold under way isn't the
/// answer while others' probes run (a shard serving one long request is the server's own work,
/// which pacing kiln's can't shorten; in the lab such holds reached 10 to 38 ms calm), only when
/// no probe ran at all since the last read. The first read has nothing to compare, and a server
/// that started again since starts over: no answer yet, and no harm.
fn since(totals: &mut Option<(f64, f64)>, now: &Delay) -> Result<f64, Failed> {
    let before = totals.replace((now.late_s, now.probes));
    let ran = before.and_then(|(late, probes)| (now.late_s >= late && now.probes > probes).then(|| (now.late_s - late) / (now.probes - probes)));
    let worst = match ran {
        Some(mean) => mean,
        None if before.is_some_and(|(_, probes)| now.probes == probes) && now.held_s > 0.0 => now.held_s,
        None => return Err(Failed::Down),
    };
    let scale = crate::shard_health::PERIOD.as_secs_f64() / now.period_s;
    let us = worst * scale * 1e6;
    if us.is_finite() && us >= 0.0 { Ok(us) } else { Err(Failed::Down) }
}

/// `GET` a kiln server's `/healthz/delay`: its figures for how late its shards run ready work. A
/// server without them (older), or one that fails, is down, not slow.
fn delay(url: &str, window: Duration) -> Result<Delay, Failed> {
    /// More than the few hundred bytes the answer is: past this it isn't one.
    const MOST: u64 = 4096;
    let rest = url.strip_prefix("http://").ok_or(Failed::Down)?;
    let (host, path) = rest.split_once('/').map_or((rest, "/".to_owned()), |(h, p)| (h, format!("/{p}")));
    let addr = if host.contains(':') && !host.ends_with(']') { host.to_owned() } else { format!("{host}:80") };
    let mut s = connect(&addr, window)?;
    // In one write: a request in pieces is a packet each.
    let ask = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: kiln-headroom\r\nConnection: close\r\n\r\n");
    s.write_all(ask.as_bytes()).map_err(|_| Failed::Down)?;
    let mut answer = Vec::new();
    match s.take(MOST).read_to_end(&mut answer) {
        Ok(_) => {}
        Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => return Err(Failed::Slow),
        Err(_) => return Err(Failed::Down),
    }
    let text = String::from_utf8_lossy(&answer);
    let (head, body) = text.split_once("\r\n\r\n").ok_or(Failed::Down)?;
    let code: u16 = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).ok_or(Failed::Down)?;
    if code != 200 {
        return Err(Failed::Down);
    }
    let d: Delay = serde_json::from_str(body.trim()).map_err(|_| Failed::Down)?;
    let sane = |v: f64| v.is_finite() && v >= 0.0;
    if sane(d.late_s) && sane(d.probes) && sane(d.held_s) && d.period_s.is_finite() && d.period_s > 0.0 { Ok(d) } else { Err(Failed::Down) }
}

/// A few bytes written to a file of kiln's own in `dir`, then made durable.
fn fsync(dir: &str, seq: &mut u64) -> std::io::Result<()> {
    *seq = seq.wrapping_add(1);
    let path = std::path::Path::new(dir).join(".kiln-headroom-probe");
    let mut f = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(&path)?;
    f.write_all(&seq.to_le_bytes())?;
    f.sync_data()
}

fn connect(addr: &str, window: Duration) -> Result<TcpStream, Failed> {
    let to = addr.to_socket_addrs().map_err(|_| Failed::Down)?.next().ok_or(Failed::Down)?;
    let s = TcpStream::connect_timeout(&to, window).map_err(|e| if e.kind() == std::io::ErrorKind::TimedOut { Failed::Slow } else { Failed::Down })?;
    let _ = s.set_read_timeout(Some(window));
    let _ = s.set_write_timeout(Some(window));
    Ok(s)
}

/// `GET` an `http://` URL: an answer under 500 is healthy, 500 and up is the target failing.
fn http(url: &str, window: Duration) -> Result<(), Failed> {
    let rest = url.strip_prefix("http://").ok_or(Failed::Down)?;
    let (host, path) = rest.split_once('/').map_or((rest, "/".to_owned()), |(h, p)| (h, format!("/{p}")));
    let addr = if host.contains(':') && !host.ends_with(']') { host.to_owned() } else { format!("{host}:80") };
    let mut s = connect(&addr, window)?;
    write!(s, "GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: kiln-headroom\r\nConnection: close\r\n\r\n").map_err(|_| Failed::Down)?;
    let mut head = [0u8; 16];
    let mut n = 0;
    while n < head.len() {
        let Some(rest) = head.get_mut(n..) else { break };
        match s.read(rest) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => return Err(Failed::Slow),
            Err(_) => return Err(Failed::Down),
        }
    }
    let line = String::from_utf8_lossy(head.get(..n).unwrap_or_default());
    let code: u16 = line.split_whitespace().nth(1).and_then(|c| c.parse().ok()).ok_or(Failed::Down)?;
    if code >= 500 { Err(Failed::Down) } else { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_for(p: &Probes, f: impl Fn(&[(String, Answer)], &[(String, f64)]) -> bool) -> (Vec<(String, Answer)>, Vec<(String, f64)>) {
        let mut seen = Vec::new();
        for _ in 0..300 {
            let (a, o) = p.drain();
            seen.extend(a);
            if f(&seen, &o) {
                return (seen, o);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        (seen, Vec::new())
    }

    #[test]
    fn an_fsync_answers_and_a_refused_port_is_unavailable_not_slow() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let closed = std::net::TcpListener::bind("127.0.0.1:0")?;
        let addr = closed.local_addr()?.to_string();
        drop(closed);
        let specs = vec![
            Spec { name: "disk".into(), probe: Probe::Fsync { path: dir.path().display().to_string() }, window: Duration::from_secs(60) },
            Spec { name: "gone".into(), probe: Probe::Tcp { addr }, window: Duration::from_secs(60) },
        ];
        let p = Probes::start(specs, Arc::new(|| {}));
        let (a, _) = wait_for(&p, |a, _| a.iter().any(|(n, _)| n == "disk") && a.iter().any(|(n, _)| n == "gone"));
        anyhow::ensure!(a.iter().any(|(n, x)| n == "disk" && matches!(x, Answer::Took(_))), "{a:?}");
        anyhow::ensure!(a.iter().any(|(n, x)| n == "gone" && *x == Answer::Unavailable), "{a:?}");
        Ok(())
    }

    /// Goal 8 (plan 01a1a59c2b29 step 5): a server's delay probe answers with how late the server's
    /// ready work ran on average between two of its reads, from the totals the server says (every
    /// probe of every shard counted), and with a hold under way only when no probe ran at all; the
    /// first read has nothing to compare, and a server without the figures (older, or a 404) is
    /// down, not slow.
    #[test]
    fn g8_s5_a_servers_delay_is_how_late_its_work_ran_since_the_last_read() -> anyhow::Result<()> {
        use std::io::{Read as _, Write as _};
        let serve = |answers: Vec<(&'static str, String)>| -> anyhow::Result<String> {
            let l = std::net::TcpListener::bind("127.0.0.1:0")?;
            let url = format!("http://{}/healthz/delay", l.local_addr()?);
            std::thread::spawn(move || {
                for (mut c, (status, body)) in l.incoming().flatten().zip(answers) {
                    // The whole request, so closing doesn't reset the connection.
                    let mut asked = Vec::new();
                    let mut chunk = [0u8; 1024];
                    while !asked.windows(4).any(|w| w == b"\r\n\r\n") {
                        match c.read(&mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => asked.extend_from_slice(chunk.get(..n).unwrap_or_default()),
                        }
                    }
                    let _ = write!(c, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                }
            });
            Ok(url)
        };
        let figures = |late_s: f64, probes: u64, held_s: f64| ("200 OK", format!(r#"{{"delay_s":0.002,"late_s":{late_s},"probes":{probes},"held_s":{held_s},"period_s":0.01}}"#));
        // 100 probes ran 0.25 s late in all between the first two reads: 2.5 ms on average. Then none
        // ran, a shard held 40 ms past its period: the hold. Then 100 ran 10 ms late in all while one
        // shard was held 30 ms (serving one long request): the mean, 0.1 ms, not the hold.
        let url = serve(vec![figures(1.0, 1_000, 0.0), figures(1.25, 1_100, 0.0), figures(1.25, 1_100, 0.04), figures(1.26, 1_200, 0.03)])?;
        let p = Probes::start(vec![Spec { name: "delay".into(), probe: Probe::Delay { url }, window: Duration::from_secs(2) }], Arc::new(|| {}));
        p.set_interval("delay", 1_000.0);
        let (a, _) = wait_for(&p, |a, _| a.len() >= 4);
        anyhow::ensure!(a.first() == Some(&("delay".to_owned(), Answer::Unavailable)), "the first read has nothing to compare: {a:?}");
        anyhow::ensure!(matches!(a.get(1), Some((_, Answer::Took(us))) if (us - 2_500.0).abs() < 1e-6), "{a:?}");
        anyhow::ensure!(matches!(a.get(2), Some((_, Answer::Took(us))) if (us - 40_000.0).abs() < 1e-6), "{a:?}");
        anyhow::ensure!(matches!(a.get(3), Some((_, Answer::Took(us))) if (us - 100.0).abs() < 1e-6), "{a:?}");
        drop(p);
        for answer in [("404 Not Found", String::new()), ("200 OK", "0.25\n".to_owned())] {
            let url = serve(vec![answer.clone(), answer])?;
            let p = Probes::start(vec![Spec { name: "delay".into(), probe: Probe::Delay { url }, window: Duration::from_secs(2) }], Arc::new(|| {}));
            p.set_interval("delay", 1_000.0);
            let (a, _) = wait_for(&p, |a, _| a.len() >= 2);
            anyhow::ensure!(a.iter().take(2).all(|(_, x)| *x == Answer::Unavailable), "{a:?}");
        }
        Ok(())
    }

    #[test]
    fn a_server_that_never_answers_is_out_while_it_hangs_and_answers_at_its_tolerance() -> anyhow::Result<()> {
        let l = std::net::TcpListener::bind("127.0.0.1:0")?;
        let url = format!("http://{}/healthz", l.local_addr()?);
        let _keep = std::thread::spawn(move || {
            let held: Vec<_> = l.incoming().take(2).collect();
            std::thread::sleep(Duration::from_secs(5));
            drop(held);
        });
        let p = Probes::start(vec![Spec { name: "slow".into(), probe: Probe::Http { url }, window: Duration::from_millis(200) }], Arc::new(|| {}));
        let (_, out) = wait_for(&p, |_, o| o.iter().any(|(_, age)| *age > 50_000.0));
        anyhow::ensure!(!out.is_empty(), "never seen out");
        let (a, _) = wait_for(&p, |a, _| !a.is_empty());
        anyhow::ensure!(matches!(a.first(), Some((_, Answer::Took(us))) if *us >= 200_000.0), "{a:?}");
        Ok(())
    }
}

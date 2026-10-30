//! Probes: each target checked on a thread of its own, at the interval the engine derives for it
//! (`kiln_ci::headroom::detect::interval_us`), so one that hangs (an fsync stuck behind a whole file
//! system's writeback) is seen hanging while it hangs, by how long it's been out, instead of
//! holding up the others or the engine. A probe gives up at its target's tolerance: by then it
//! is harm already, and the next one starts.

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
    pub tolerance: Duration,
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
    /// says, once per tolerance); `wake` is called when one answers.
    pub fn start(specs: Vec<Spec>, wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let base = Instant::now();
        let mut slots = Vec::new();
        for spec in &specs {
            let slot = Arc::new(Slot { name: spec.name.clone(), inflight: AtomicU64::new(0), every_us: AtomicU64::new(spec.tolerance.as_micros() as u64), answers: Mutex::new(Vec::new()) });
            slots.push(Arc::clone(&slot));
            let (probe, tolerance, stop, wake) = (spec.probe.clone(), spec.tolerance, Arc::clone(&stop), Arc::clone(&wake));
            let spawned = std::thread::Builder::new().name(format!("kiln-probe-{}", spec.name.chars().take(8).collect::<String>())).spawn(move || {
                let mut seq: u64 = 0;
                while !stop.load(Ordering::Relaxed) {
                    slot.inflight.store((base.elapsed().as_micros() as u64).max(1), Ordering::Release);
                    let at = Instant::now();
                    let answer = match once(&probe, tolerance, &mut seq) {
                        // Out its tolerance is an answer too: that long.
                        Ok(()) | Err(Failed::Slow) => Answer::Took(at.elapsed().as_micros() as f64),
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
    /// It ran out its target's tolerance: slowness (harm), not a failure.
    Slow,
    /// Refused or failed: down for its own reasons.
    Down,
}

fn once(probe: &Probe, tolerance: Duration, seq: &mut u64) -> Result<(), Failed> {
    match probe {
        Probe::Fsync { path } => fsync(path, seq).map_err(|_| Failed::Down),
        Probe::Http { url } => http(url, tolerance),
        Probe::Tcp { addr } => connect(addr, tolerance).map(drop),
    }
}

/// A few bytes written to a file of kiln's own in `dir`, then made durable.
fn fsync(dir: &str, seq: &mut u64) -> std::io::Result<()> {
    *seq = seq.wrapping_add(1);
    let path = std::path::Path::new(dir).join(".kiln-headroom-probe");
    let mut f = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(&path)?;
    f.write_all(&seq.to_le_bytes())?;
    f.sync_data()
}

fn connect(addr: &str, tolerance: Duration) -> Result<TcpStream, Failed> {
    let to = addr.to_socket_addrs().map_err(|_| Failed::Down)?.next().ok_or(Failed::Down)?;
    let s = TcpStream::connect_timeout(&to, tolerance).map_err(|e| if e.kind() == std::io::ErrorKind::TimedOut { Failed::Slow } else { Failed::Down })?;
    let _ = s.set_read_timeout(Some(tolerance));
    let _ = s.set_write_timeout(Some(tolerance));
    Ok(s)
}

/// `GET` an `http://` URL: an answer under 500 is healthy, 500 and up is the target failing.
fn http(url: &str, tolerance: Duration) -> Result<(), Failed> {
    let rest = url.strip_prefix("http://").ok_or(Failed::Down)?;
    let (host, path) = rest.split_once('/').map_or((rest, "/".to_owned()), |(h, p)| (h, format!("/{p}")));
    let addr = if host.contains(':') && !host.ends_with(']') { host.to_owned() } else { format!("{host}:80") };
    let mut s = connect(&addr, tolerance)?;
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
            Spec { name: "disk".into(), probe: Probe::Fsync { path: dir.path().display().to_string() }, tolerance: Duration::from_secs(60) },
            Spec { name: "gone".into(), probe: Probe::Tcp { addr }, tolerance: Duration::from_secs(60) },
        ];
        let p = Probes::start(specs, Arc::new(|| {}));
        let (a, _) = wait_for(&p, |a, _| a.iter().any(|(n, _)| n == "disk") && a.iter().any(|(n, _)| n == "gone"));
        anyhow::ensure!(a.iter().any(|(n, x)| n == "disk" && matches!(x, Answer::Took(_))), "{a:?}");
        anyhow::ensure!(a.iter().any(|(n, x)| n == "gone" && *x == Answer::Unavailable), "{a:?}");
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
        let p = Probes::start(vec![Spec { name: "slow".into(), probe: Probe::Http { url }, tolerance: Duration::from_millis(200) }], Arc::new(|| {}));
        let (_, out) = wait_for(&p, |_, o| o.iter().any(|(_, age)| *age > 50_000.0));
        anyhow::ensure!(!out.is_empty(), "never seen out");
        let (a, _) = wait_for(&p, |a, _| !a.is_empty());
        anyhow::ensure!(matches!(a.first(), Some((_, Answer::Took(us))) if *us >= 200_000.0), "{a:?}");
        Ok(())
    }
}

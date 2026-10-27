//! Probes: each target checked on a thread of its own, again and again, so one that hangs (an
//! fsync stuck behind a whole file system's writeback) is seen hanging while it hangs, by how long
//! it's been in flight, instead of holding up the others or the control.

use std::io::{Read as _, Write as _};
use std::net::{TcpStream, ToSocketAddrs as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use super::settings::{Probe, Target};

/// Between one probe of a target ending and the next starting.
const EVERY: Duration = Duration::from_millis(250);
/// The longest one probe may take before it's given up (it has long been counted as missing).
const GIVE_UP: Duration = Duration::from_secs(30);

/// What a target's probes say now.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "lowercase", tag = "state")]
pub enum Verdict {
    /// Not probed yet.
    Unknown,
    /// Answered within its goal.
    Met { ms: u64 },
    /// Slower than its goal, or still in flight past it.
    Missed { ms: u64 },
    /// Refused or failed: down for its own reasons, not slowness (not counted).
    Unavailable { why: String },
}

#[derive(Default)]
struct Slot {
    /// When the probe in flight started (ms since `base`), 0 when none is.
    inflight: AtomicU64,
    last: Mutex<Option<Result<u64, String>>>,
}

/// The probes of one set of targets; dropping it stops them.
pub struct Probes {
    pub targets: Vec<Target>,
    slots: Vec<Arc<Slot>>,
    stop: Arc<AtomicBool>,
    base: Instant,
}

impl Probes {
    /// Starts a thread per target; `wake` is called whenever one finishes.
    pub fn start(targets: Vec<Target>, wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let base = Instant::now();
        let mut slots = Vec::new();
        for t in &targets {
            let slot = Arc::new(Slot::default());
            slots.push(Arc::clone(&slot));
            let (probe, stop, wake) = (t.probe.clone(), Arc::clone(&stop), Arc::clone(&wake));
            let name = format!("kiln-probe-{}", t.name.chars().take(8).collect::<String>());
            let spawned = std::thread::Builder::new().name(name).spawn(move || {
                let mut seq: u64 = 0;
                while !stop.load(Ordering::Relaxed) {
                    let started = base.elapsed().as_millis() as u64;
                    slot.inflight.store(started.max(1), Ordering::Release);
                    let at = Instant::now();
                    let res = once(&probe, &mut seq).map(|()| at.elapsed().as_millis() as u64);
                    slot.inflight.store(0, Ordering::Release);
                    *slot.last.lock() = Some(res);
                    wake();
                    std::thread::sleep(EVERY);
                }
            });
            if let Err(e) = spawned {
                tracing::warn!(error = %e, target = %t.name, "headroom: a probe couldn't start");
            }
        }
        Self { targets, slots, stop, base }
    }

    /// Each target's verdict now, in `targets`' order.
    pub fn verdicts(&self) -> Vec<Verdict> {
        let now = self.base.elapsed().as_millis() as u64;
        self.targets
            .iter()
            .zip(&self.slots)
            .map(|(t, s)| {
                let goal = u64::from(t.goal_ms);
                let inflight = s.inflight.load(Ordering::Acquire);
                if inflight > 0 && now.saturating_sub(inflight) > goal {
                    return Verdict::Missed { ms: now.saturating_sub(inflight) };
                }
                match s.last.lock().clone() {
                    None => Verdict::Unknown,
                    Some(Ok(ms)) if ms <= goal => Verdict::Met { ms },
                    Some(Ok(ms)) => Verdict::Missed { ms },
                    Some(Err(why)) if why == SLOW => Verdict::Missed { ms: GIVE_UP.as_millis() as u64 },
                    Some(Err(why)) => Verdict::Unavailable { why },
                }
            })
            .collect()
    }
}

impl Drop for Probes {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// A probe that ran out of time: slowness, not a failure.
const SLOW: &str = "no answer in time";

fn once(probe: &Probe, seq: &mut u64) -> Result<(), String> {
    match probe {
        Probe::Fsync { path } => fsync(path, seq),
        Probe::Http { url } => http(url),
        Probe::Tcp { addr } => connect(addr).map(drop),
    }
}

/// A few bytes written to a file of kiln's own in `dir`, then made durable.
fn fsync(dir: &str, seq: &mut u64) -> Result<(), String> {
    *seq = seq.wrapping_add(1);
    let path = std::path::Path::new(dir).join(".kiln-headroom-probe");
    let mut f = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    f.write_all(&seq.to_le_bytes()).map_err(|e| e.to_string())?;
    f.sync_data().map_err(|e| e.to_string())
}

fn connect(addr: &str) -> Result<TcpStream, String> {
    let to = addr.to_socket_addrs().map_err(|e| format!("{addr}: {e}"))?.next().ok_or_else(|| format!("{addr}: no address"))?;
    let s = TcpStream::connect_timeout(&to, GIVE_UP).map_err(|e| if e.kind() == std::io::ErrorKind::TimedOut { SLOW.to_owned() } else { e.to_string() })?;
    let _ = s.set_read_timeout(Some(GIVE_UP));
    let _ = s.set_write_timeout(Some(GIVE_UP));
    Ok(s)
}

/// `GET` an `http://` URL: an answer under 500 is healthy, 500 and up is the target failing.
fn http(url: &str) -> Result<(), String> {
    let rest = url.strip_prefix("http://").ok_or("only http:// is probed")?;
    let (host, path) = rest.split_once('/').map_or((rest, "/".to_owned()), |(h, p)| (h, format!("/{p}")));
    let addr = if host.contains(':') && !host.ends_with(']') { host.to_owned() } else { format!("{host}:80") };
    let mut s = connect(&addr)?;
    write!(s, "GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: kiln-headroom\r\nConnection: close\r\n\r\n").map_err(|e| e.to_string())?;
    let mut head = [0u8; 16];
    let mut n = 0;
    while n < head.len() {
        let Some(rest) = head.get_mut(n..) else { break };
        match s.read(rest) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => return Err(SLOW.to_owned()),
            Err(e) => return Err(e.to_string()),
        }
    }
    let line = String::from_utf8_lossy(head.get(..n).unwrap_or_default());
    let code: u16 = line.split_whitespace().nth(1).and_then(|c| c.parse().ok()).ok_or_else(|| format!("not an HTTP answer: {line:?}"))?;
    if code >= 500 { Err(format!("HTTP {code}")) } else { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_for(p: &Probes, f: impl Fn(&[Verdict]) -> bool) -> Vec<Verdict> {
        for _ in 0..200 {
            let v = p.verdicts();
            if f(&v) {
                return v;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        p.verdicts()
    }

    #[test]
    fn an_fsync_meets_a_generous_goal_and_a_refused_port_is_unavailable_not_slow() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let closed = std::net::TcpListener::bind("127.0.0.1:0")?;
        let addr = closed.local_addr()?.to_string();
        drop(closed);
        let targets = vec![
            Target { name: "disk".into(), probe: Probe::Fsync { path: dir.path().display().to_string() }, goal_ms: 60_000 },
            Target { name: "gone".into(), probe: Probe::Tcp { addr }, goal_ms: 60_000 },
        ];
        let p = Probes::start(targets, Arc::new(|| {}));
        let v = wait_for(&p, |v| v.iter().all(|x| *x != Verdict::Unknown));
        anyhow::ensure!(matches!(v.first(), Some(Verdict::Met { .. })), "{v:?}");
        anyhow::ensure!(matches!(v.get(1), Some(Verdict::Unavailable { .. })), "{v:?}");
        Ok(())
    }

    #[test]
    fn a_server_that_answers_late_is_missing_while_it_hangs() -> anyhow::Result<()> {
        // Accepts and never answers: the probe hangs, and is seen missing by how long it's been out.
        let l = std::net::TcpListener::bind("127.0.0.1:0")?;
        let url = format!("http://{}/healthz", l.local_addr()?);
        let _keep = std::thread::spawn(move || {
            let held: Vec<_> = l.incoming().take(1).collect();
            std::thread::sleep(Duration::from_secs(5));
            drop(held);
        });
        let p = Probes::start(vec![Target { name: "slow".into(), probe: Probe::Http { url }, goal_ms: 50 }], Arc::new(|| {}));
        let v = wait_for(&p, |v| matches!(v.first(), Some(Verdict::Missed { .. })));
        anyhow::ensure!(matches!(v.first(), Some(Verdict::Missed { .. })), "{v:?}");
        Ok(())
    }
}

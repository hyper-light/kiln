//! Small network programs the lab runs inside its namespaces to check its links against their
//! profiles (plan 01a1a832e9cc step 2): numbered UDP packets (loss, its seeded indices, reordering,
//! one-way delay and a full queue's), UDP pings (RTT) and a TCP bulk transfer (throughput). Each
//! prints what it saw as one JSON line.

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::standin::now_us;

#[derive(clap::Subcommand, Clone, Debug)]
pub enum Tool {
    /// Receives numbered packets on `--bind` until `--count` came or none came for `--quiet-ms`.
    UdpSink {
        #[arg(long)]
        bind: SocketAddr,
        #[arg(long)]
        count: u64,
        #[arg(long, default_value_t = 3000)]
        quiet_ms: u64,
        /// Keep each packet's one-way delay.
        #[arg(long)]
        delays: bool,
    },
    /// Sends `--count` numbered packets of `--size` bytes to `--to`, `--pps` a second.
    UdpBlast {
        #[arg(long)]
        to: SocketAddr,
        #[arg(long)]
        count: u64,
        #[arg(long)]
        pps: f64,
        #[arg(long, default_value_t = 16)]
        size: usize,
    },
    /// Sends every packet back where it came from.
    UdpEcho {
        #[arg(long)]
        bind: SocketAddr,
    },
    /// `--count` pings of `--size` bytes to an echo at `--to`, `--interval-ms` apart: each RTT.
    UdpPing {
        #[arg(long)]
        to: SocketAddr,
        #[arg(long)]
        count: u64,
        #[arg(long, default_value_t = 100)]
        interval_ms: u64,
        #[arg(long, default_value_t = 16)]
        size: usize,
    },
    /// Takes one TCP connection on `--bind` and reads it to its end.
    TcpSink {
        #[arg(long)]
        bind: SocketAddr,
    },
    /// Writes to `--to` as fast as it takes for `--secs` seconds.
    TcpSource {
        #[arg(long)]
        to: SocketAddr,
        #[arg(long)]
        secs: u64,
    },
}

/// What a sink saw.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Sunk {
    pub received: u64,
    /// The numbers below `count` that never came, in order.
    pub missing: Vec<u64>,
    /// Packets that came after one numbered higher.
    pub reordered: u64,
    /// One-way delay of each packet as it came (µs), when asked.
    #[serde(default)]
    pub delays_us: Vec<u64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Pinged {
    pub sent: u64,
    /// Each answered ping's RTT (µs), in the order sent.
    pub rtts_us: Vec<u64>,
    pub lost: u64,
}

fn header(seq: u64, size: usize) -> Vec<u8> {
    let mut b = vec![0u8; size.max(16)];
    if let Some(s) = b.get_mut(..8) {
        s.copy_from_slice(&seq.to_be_bytes());
    }
    if let Some(s) = b.get_mut(8..16) {
        s.copy_from_slice(&now_us().to_be_bytes());
    }
    b
}

fn read_u64(b: &[u8], at: usize) -> Option<u64> {
    let s: [u8; 8] = b.get(at..at + 8)?.try_into().ok()?;
    Some(u64::from_be_bytes(s))
}

/// Paces a loop at `per_s` a second, from its start (so a late wake doesn't slow the rest).
fn pace(start: Instant, n: u64, per_s: f64) {
    #[allow(clippy::cast_precision_loss)]
    let due = start + Duration::from_secs_f64(n as f64 / per_s.max(1e-3));
    let now = Instant::now();
    if due > now {
        std::thread::sleep(due - now);
    }
}

pub fn run(t: Tool) -> anyhow::Result<()> {
    match t {
        Tool::UdpSink { bind, count, quiet_ms, delays } => {
            let s = UdpSocket::bind(bind)?;
            s.set_read_timeout(Some(Duration::from_millis(quiet_ms.max(1))))?;
            let mut seen = vec![false; usize::try_from(count).unwrap_or(0)];
            let mut out = Sunk::default();
            let mut top: Option<u64> = None;
            let mut buf = [0u8; 2048];
            let first_wait = Instant::now() + Duration::from_secs(120);
            while out.received < count {
                match s.recv(&mut buf) {
                    Ok(n) => {
                        let (Some(seq), Some(sent)) = (read_u64(buf.get(..n).unwrap_or_default(), 0), read_u64(buf.get(..n).unwrap_or_default(), 8)) else { continue };
                        if let Some(slot) = usize::try_from(seq).ok().and_then(|i| seen.get_mut(i))
                            && !*slot
                        {
                            *slot = true;
                            out.received += 1;
                        }
                        if top.is_some_and(|t| seq < t) {
                            out.reordered += 1;
                        }
                        top = Some(top.map_or(seq, |t| t.max(seq)));
                        if delays {
                            out.delays_us.push(now_us().saturating_sub(sent));
                        }
                    }
                    Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                        if top.is_some() || Instant::now() > first_wait {
                            break;
                        }
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            out.missing = seen.iter().enumerate().filter(|(_, s)| !**s).map(|(i, _)| i as u64).collect();
            println!("{}", serde_json::to_string(&out)?);
        }
        Tool::UdpBlast { to, count, pps, size } => {
            let s = UdpSocket::bind("0.0.0.0:0")?;
            let start = Instant::now();
            for seq in 0..count {
                pace(start, seq, pps);
                // A full socket buffer drops here, before the link: send again until it takes it.
                while let Err(e) = s.send_to(&header(seq, size), to) {
                    anyhow::ensure!(e.raw_os_error() == Some(105) || e.kind() == std::io::ErrorKind::WouldBlock, "send: {e}");
                    std::thread::sleep(Duration::from_micros(200));
                }
            }
            println!("{}", serde_json::json!({"sent": count, "secs": start.elapsed().as_secs_f64()}));
        }
        Tool::UdpEcho { bind } => {
            let s = UdpSocket::bind(bind)?;
            let mut buf = [0u8; 2048];
            loop {
                let (n, from) = s.recv_from(&mut buf)?;
                s.send_to(buf.get(..n).unwrap_or_default(), from)?;
            }
        }
        Tool::UdpPing { to, count, interval_ms, size } => {
            let s = UdpSocket::bind("0.0.0.0:0")?;
            s.connect(to)?;
            let r = s.try_clone()?;
            // Long enough for a reply through a full slow queue.
            r.set_read_timeout(Some(Duration::from_secs(15)))?;
            let n = usize::try_from(count).unwrap_or(0);
            let reader = std::thread::spawn(move || {
                let mut rtts = vec![None; n];
                let mut buf = [0u8; 2048];
                let mut got = 0;
                while got < n {
                    let Ok(len) = r.recv(&mut buf) else { break };
                    let b = buf.get(..len).unwrap_or_default();
                    if let (Some(seq), Some(sent)) = (read_u64(b, 0), read_u64(b, 8))
                        && let Some(slot) = usize::try_from(seq).ok().and_then(|i| rtts.get_mut(i))
                        && slot.is_none()
                    {
                        *slot = Some(now_us().saturating_sub(sent));
                        got += 1;
                    }
                }
                rtts
            });
            let start = Instant::now();
            #[allow(clippy::cast_precision_loss)]
            let per_s = 1000.0 / interval_ms.max(1) as f64;
            for seq in 0..count {
                pace(start, seq, per_s);
                s.send(&header(seq, size))?;
            }
            let rtts = reader.join().map_err(|_| anyhow::anyhow!("the ping reader stopped"))?;
            let lost = rtts.iter().filter(|r| r.is_none()).count() as u64;
            let out = Pinged { sent: count, rtts_us: rtts.into_iter().flatten().collect(), lost };
            println!("{}", serde_json::to_string(&out)?);
        }
        Tool::TcpSink { bind } => {
            let l = TcpListener::bind(bind)?;
            let (mut c, _) = l.accept()?;
            let mut buf = vec![0u8; 1 << 16];
            let mut total = 0u64;
            loop {
                let n = c.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                total += n as u64;
            }
            println!("{}", serde_json::json!({"bytes": total}));
        }
        Tool::TcpSource { to, secs } => {
            let deadline = Instant::now() + Duration::from_secs(secs);
            let mut c = TcpStream::connect_timeout(&to, Duration::from_secs(30))?;
            c.set_write_timeout(Some(Duration::from_secs(1)))?;
            let buf = vec![0x5au8; 1 << 14];
            let mut total = 0u64;
            while Instant::now() < deadline {
                match c.write(&buf) {
                    Ok(n) => total += n as u64,
                    Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            let _ = c.shutdown(std::net::Shutdown::Both);
            println!("{}", serde_json::json!({"bytes": total}));
        }
    }
    Ok(())
}

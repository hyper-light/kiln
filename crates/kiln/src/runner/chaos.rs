//! Fault injection for tests (docs/ci.md W22-AC04), compiled only with the `chaos` feature:
//! `kiln agent --chaos <spec>`, or `KILN_CHAOS` for a server's own runner. Without the feature
//! [`get`] is always `None` and nothing here runs.
//!
//! The spec is a comma list:
//! - `drop=<duration>`: the runner drops its connection to the server that often;
//! - `down=<duration>`: after each drop, the runner stays away that long before it dials again (a
//!   link cut for a while, not a blip);
//! - `delay=<duration>`: everything it sends the server waits that long first;
//! - `oom=<duration>`: a leg's step on the host is killed that long after it starts, as the OOM
//!   killer kills (SIGKILL, and counted where kiln counts the OOM killer's kills);
//! - `disk=<MiB>`: a leg's job container gets a workspace of that size, so filling it is ENOSPC;
//! - `corrupt`: a leg's uploads to the server's blob store arrive with a byte flipped;
//! - `platform=<os>/<arch>`: the runner reports that platform's OS and architecture (and the
//!   labels they give), a stand-in for a runner of another platform (W04-AC07);
//! - `rebuild=<duration>`: the server's, not its runner's: the CI engine rebuilds its runs that
//!   long after the server starts, so its runners say hello before it knows their legs (W05-R05).
//!
//! A leg's faults (`oom`, `disk`, `corrupt`) hit its first attempt only, so a retry shows how it
//! recovers; `drop` and `delay` are the connection's.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use futures::FutureExt as _;
use futures::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::http::io::RtFuturesIo;
use crate::net::Kind;
use crate::rt::Shutdown;

/// What to inject.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Chaos {
    pub drop_every: Option<Duration>,
    pub down_for: Option<Duration>,
    pub delay: Option<Duration>,
    pub oom_after: Option<Duration>,
    pub disk_mib: Option<u32>,
    pub corrupt: bool,
    pub platform: Option<String>,
    pub rebuild_after: Option<Duration>,
}

/// `300ms`, `2s`, `1m`.
fn duration(s: &str) -> Option<Duration> {
    let split = s.find(|c: char| !c.is_ascii_digit())?;
    let (n, unit) = (s.get(..split)?.parse::<u64>().ok()?, s.get(split..)?);
    match unit {
        "ms" => Some(Duration::from_millis(n)),
        "s" => Some(Duration::from_secs(n)),
        "m" => Some(Duration::from_secs(n * 60)),
        _ => None,
    }
}

/// A `--chaos` spec.
pub fn parse(spec: &str) -> anyhow::Result<Chaos> {
    let mut c = Chaos::default();
    for item in spec.split(',').map(str::trim).filter(|i| !i.is_empty()) {
        let (k, v) = item.split_once('=').map_or((item, ""), |(k, v)| (k.trim(), v.trim()));
        let d = || duration(v).ok_or_else(|| anyhow::anyhow!("chaos `{k}` takes a duration (300ms, 2s, 1m), not `{v}`"));
        match k {
            "drop" => c.drop_every = Some(d()?),
            "down" => c.down_for = Some(d()?),
            "delay" => c.delay = Some(d()?),
            "oom" => c.oom_after = Some(d()?),
            "disk" => c.disk_mib = Some(v.parse().map_err(|_| anyhow::anyhow!("chaos `disk` takes MiB, not `{v}`"))?),
            "corrupt" => c.corrupt = true,
            "platform" => {
                kiln_ci::plan::matrix::Platform::parse(v).map_err(|e| anyhow::anyhow!("chaos `platform`: {e}"))?;
                c.platform = Some(v.to_owned());
            }
            "rebuild" => c.rebuild_after = Some(d()?),
            other => anyhow::bail!("chaos has no fault `{other}`: drop, down, delay, oom, disk, corrupt, platform or rebuild"),
        }
    }
    Ok(c)
}

#[cfg(feature = "chaos")]
static CHAOS: std::sync::OnceLock<Chaos> = std::sync::OnceLock::new();

/// Turns fault injection on for this process, once.
#[cfg(feature = "chaos")]
pub fn set(c: Chaos) {
    tracing::warn!(chaos = ?c, "fault injection is on (a test build)");
    let _ = CHAOS.set(c);
}

/// What's injected, if anything.
pub fn get() -> Option<&'static Chaos> {
    #[cfg(feature = "chaos")]
    {
        CHAOS.get()
    }
    #[cfg(not(feature = "chaos"))]
    {
        None
    }
}

/// A leg's faults: its first attempt's.
pub fn for_leg(attempt: u32) -> Option<&'static Chaos> {
    get().filter(|_| attempt <= 1)
}

/// Kills chaos made as the OOM killer would, added to the kernel's count.
pub static OOM_KILLS: AtomicU64 = AtomicU64::new(0);

pub fn oom_kills() -> u64 {
    OOM_KILLS.load(Ordering::Relaxed)
}

/// What the runner sends the server, each frame `by` after it was handed in, in order: a slow
/// network's latency (`delay`), which holds nothing else up.
pub struct DelayLine {
    tx: async_channel::Sender<(Instant, Due)>,
    by: Duration,
}

/// A frame out of the [`DelayLine`], due: its lane, kind and bytes, and whether the next
/// connection sends it again anyway.
pub struct Due {
    pub lane: u32,
    pub kind: Kind,
    pub data: Vec<u8>,
    pub again: bool,
}

impl DelayLine {
    /// The line, and where each frame comes out when it's due: back to the runner's loop, which
    /// sends it if it's connected and welcomed then, and keeps it for the next connection if not,
    /// as it does any frame (one sent straight on could reach a connection the server hasn't
    /// taken the runner's Hello on yet, and be dropped there). A frame marked `again` (a leg's
    /// event or result, which the next connection sends again from the runner's journal, in
    /// order) isn't kept: sent ahead of the journal, it would make the server skip what's before
    /// it.
    pub fn new(by: Duration) -> (DelayLine, async_channel::Receiver<Due>) {
        let (tx, rx) = async_channel::unbounded::<(Instant, Due)>();
        let (due_tx, due_rx) = async_channel::unbounded::<Due>();
        crate::rt::spawn(async move {
            while let Ok((due, frame)) = rx.recv().await {
                crate::rt::sleep_until(due).await;
                if due_tx.send(frame).await.is_err() {
                    return;
                }
            }
        });
        (DelayLine { tx, by }, due_rx)
    }

    pub fn send(&self, lane: u32, kind: Kind, data: Vec<u8>, again: bool) {
        let _ = self.tx.try_send((Instant::now() + self.by, Due { lane, kind, data, again }));
    }
}

/// A proxy to the server for a leg's tools (`corrupt`): the first byte of each upload to the blob
/// store (`PUT /_kiln/blob/…`) flipped, everything else as it was. One request per connection (it
/// says `Connection: close`), so it reads heads only. It stops when dropped.
pub struct Corrupting {
    /// `http://127.0.0.1:<port>`, the leg's tools' server in place of the real one.
    pub origin: String,
    /// The real one's (`http://host:port`).
    server: String,
    stop: Shutdown,
}

impl Drop for Corrupting {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl Corrupting {
    /// `url` through the proxy, when it's the server's.
    pub fn route(&self, url: &str) -> String {
        match url.strip_prefix(self.server.as_str()).filter(|rest| rest.is_empty() || rest.starts_with('/')) {
            Some(rest) => format!("{}{rest}", self.origin),
            None => url.to_owned(),
        }
    }

    /// In front of `server`, an `http://` URL (an HTTPS server's uploads aren't corrupted).
    pub fn start(server: &str) -> anyhow::Result<Corrupting> {
        let uri: http::Uri = server.parse().with_context(|| format!("bad server URL {server}"))?;
        anyhow::ensure!(uri.scheme_str() == Some("http"), "chaos `corrupt` needs a server on plain HTTP, not {server}");
        let host = uri.host().context("a server URL without a host")?.trim_matches(['[', ']']).to_owned();
        let port = uri.port_u16().unwrap_or(80);
        let real = format!("http://{}", uri.authority().context("a server URL without a host")?);
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let origin = format!("http://{}", listener.local_addr()?);
        let stop = Shutdown::new();
        let sd = stop.clone();
        crate::rt::spawn(async move {
            let listener = match hyper_rt::tcp::TcpListener::from_owned(hyper_rt::tcp::OwnedStream::from(listener), 64) {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(error = %crate::rt::to_io(e), "chaos: the corrupting proxy can't listen");
                    return;
                }
            };
            loop {
                let accepted = futures::select_biased! {
                    () = sd.cancelled().fuse() => return,
                    r = listener.accept().fuse() => r,
                };
                let Ok(client) = accepted else { continue };
                let (host, sd) = (host.clone(), sd.clone());
                crate::rt::spawn(async move {
                    let _ = crate::discovery::until_stopped(&sd, async {
                        if let Err(e) = forward(RtFuturesIo::new(client), &host, port).await {
                            tracing::debug!(error = %format!("{e:#}"), "chaos: a proxied request ended");
                        }
                    })
                    .await;
                });
            }
        });
        tracing::warn!(%origin, server, "chaos: the leg's uploads go through a proxy that corrupts them");
        Ok(Corrupting { origin, server: real, stop })
    }
}

/// One request to the server, its upload's first byte flipped.
async fn forward(mut client: RtFuturesIo, host: &str, port: u16) -> anyhow::Result<()> {
    let mut read = Vec::new();
    let mut chunk = vec![0u8; 64 << 10];
    let end = loop {
        let n = client.read(&mut chunk).await?;
        anyhow::ensure!(n > 0, "closed before its head");
        read.extend_from_slice(chunk.get(..n).unwrap_or_default());
        if let Some(p) = read.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
        anyhow::ensure!(read.len() < 64 << 10, "a head of more than 64 KiB");
    };
    let mut body = read.split_off(end);
    let head = String::from_utf8_lossy(&read).into_owned();
    let mut pending = head.starts_with("PUT /_kiln/blob/");
    let mut out: String = head.lines().filter(|l| !l.is_empty() && !l.to_ascii_lowercase().starts_with("connection:")).map(|l| format!("{l}\r\n")).collect();
    out.push_str("Connection: close\r\n\r\n");
    let mut server = RtFuturesIo::new(crate::http::io::connect(host, port).await?);
    server.write_all(out.as_bytes()).await?;
    while pending {
        if let Some(b) = body.first_mut() {
            *b ^= 0xff;
            pending = false;
            tracing::warn!("chaos: an upload's first byte flipped");
        }
        server.write_all(&body).await?;
        if pending {
            let n = client.read(&mut chunk).await?;
            anyhow::ensure!(n > 0, "closed before its body");
            body = chunk.get(..n).unwrap_or_default().to_vec();
        } else {
            body.clear();
        }
    }
    server.write_all(&body).await?;
    crate::frontdoor::proxy::pipe(client, server).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w22_ac04_specs() -> anyhow::Result<()> {
        let c = parse("drop=2s, delay=300ms,oom=1s,disk=8,corrupt")?;
        assert_eq!(
            c,
            Chaos {
                drop_every: Some(Duration::from_secs(2)),
                down_for: None,
                delay: Some(Duration::from_millis(300)),
                oom_after: Some(Duration::from_secs(1)),
                disk_mib: Some(8),
                corrupt: true,
                platform: None,
                rebuild_after: None
            }
        );
        assert_eq!(parse("platform=darwin/arm64")?.platform.as_deref(), Some("darwin/arm64"));
        assert_eq!(parse("rebuild=4s")?.rebuild_after, Some(Duration::from_secs(4)));
        assert_eq!(parse("drop=5s,down=30s")?.down_for, Some(Duration::from_secs(30)));
        assert!(parse("platform=plan9/mips").is_err());
        assert!(parse("flood=1s").is_err_and(|e| e.to_string().contains("no fault `flood`")));
        assert!(parse("oom=soon").is_err());
        assert_eq!(parse("")?, Chaos::default());
        Ok(())
    }
}

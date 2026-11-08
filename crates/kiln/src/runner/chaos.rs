//! Fault injection for tests (docs/ci.md W22-AC04), compiled only with the `chaos` feature:
//! `kiln agent --chaos <spec>`, or `KILN_CHAOS` for a server's own runner. Without the feature
//! [`get`] is always `None` and nothing here runs.
//!
//! The spec is a comma list:
//! - `drop=<duration>`: the runner drops its connection to the server that often;
//! - `down=<duration>`: after each drop, the runner stays away that long before it dials again (a
//!   link cut for a while, not a blip);
//! - `delay=<duration>`: everything it sends the server waits that long first;
//! - `lag=<duration>`: the runner takes each thing its jobs send (a log's chunk, a job's event)
//!   that long late, as a busy machine's runner falls behind its jobs: a job's end comes while
//!   what it sent before still waits (W27-R05);
//! - `oom=<duration>`: a leg's step on the host is killed that long after it starts, as the OOM
//!   killer kills (SIGKILL, and counted where kiln counts the OOM killer's kills);
//! - `disk=<MiB>`: a leg's job container gets a workspace of that size, so filling it is ENOSPC;
//! - `corrupt`: a leg's uploads to the server's blob store arrive with a byte flipped;
//! - `fetch`: a leg's first ask for a Node build from the internet fails as a request that timed
//!   out does, so its step asks again ([`crate::runner::runtime`]'s download tries);
//! - `platform=<os>/<arch>`: the runner reports that platform's OS and architecture (and the
//!   labels they give), a stand-in for a runner of another platform (W04-AC07);
//! - `rebuild=<duration>`: the server's, not its runner's: the CI engine rebuilds its runs that
//!   long after the server starts, so its runners say hello before it knows their legs (W05-R05).
//! - `claim=<duration>`: the server's: the CI engine's first claim of a grant (written before its
//!   Assign is sent) is held back that long and then fails, as a store that can't write would
//!   (W05-R05).
//! - `stop=<job>`: the server's: the CI engine grants that job's legs as items that stop when their
//!   lease is lost (`on_lease_loss: stop`), as no CI leg does, so their runner stops them at its
//!   lease's end (W05-R37).
//! - `refuse=<job>`: the runner's own admission refuses the first leg of that job it's sent, as one
//!   with no room for it would (W05-R40: a gang's member refused at its runner).
//! - `shed=<lane>[/<variant>]:<every>`: this process's QUIC layer drops about one frame in `every`
//!   of `lane` (`control`, `term` ... or its number; of those whose first byte, a message's
//!   variant index, is `variant`, when given), picked by a seeded draw, as a frame lost below the
//!   channel would be, counted as `chaos` (docs/wire.md, "Loss below the channel"). A runner's
//!   or, from `KILN_CHAOS`, a server's.
//! - `twice=<lane>[/<variant>]:<every>` and `late=<lane>[/<variant>]:<every>`: as `shed`, the
//!   frames it picks sent twice, or held back and sent after the next frame on their lane (out of
//!   order), not dropped. Each may be given beside the others, each drawing on its own.
//! - `unknown=<lane>[/<variant>]:<every>` and `garble=<lane>[/<variant>]:<every>`: as `shed`, the
//!   frames it picks made unreadable, not dropped: their variant index made one past what their
//!   sender's enum has, a kind the receiver doesn't know; or cut short, given bytes after them, or a
//!   byte past their index flipped (docs/wire.md, "What can't be read").
//! - `seed=<n>`: the frames those faults pick are another seed's, the same each run (a lab run
//!   again on a seed goes wrong as it did).
//! - `fanout=<frames>`: the server's: each session's live feed keeps that many frames for its
//!   viewers (`crate::harness::live`), so one that stops reading falls behind sooner (and is caught
//!   up in-band).
//! - `old`: the runner says nothing of what it can do past `Build`'s 32 bits (no `Caps`), as a
//!   runner from before the channel says nothing (docs/wire.md): the server sends it commands and
//!   takes its updates as it did such runners'.
//! - `unsigned`: the runner says nothing of custody (no `CAP_CUSTODY`, and taking over blue/green
//!   it doesn't say it takes a session signed for) and signs no hand-over or receipt, as a runner
//!   from before signed hand-overs does (docs/ci.md W27-R08): its hand-overs go unsigned.
//! - `stale_words`: the server's: as kiln certifies an agent session at an epoch for a process
//!   other than the one it counted as its holder, it also sends that one the words about a
//!   hand-over it sent before transfers named the holder (hand it to the newest, it was taken), as
//!   it did at 14:18 on 2026-10-25 (docs/ci.md W27-R08), so a test plays the drop again.
//! - `identity_late=<duration>`: the server's: it answers each leg's identity that long late, as a
//!   busy server does (docs/ci.md W27-R05), so a test sees the leg's first step wait for it, or,
//!   past that wait's bound, run with its records unsigned.
//! - `tarball_changed`: the server's: a commit's tarball it serves a leg has a byte changed after
//!   kiln signed its hand-on of it, as a tarball changed in transit is (docs/ci.md W27-R08), so a
//!   test sees the runner refuse it before the leg runs and kiln record the break.
//! - `no_git`: the runner acts as if git weren't installed: a leg checks its commit out from the
//!   server's tarball, as on a machine without git (docs/ci.md W27-R08's tarball hand-on).
//! - `peer_object_changed`: an object of its cache this runner sends a peer has a byte changed after
//!   it signed its hand-on of it, as in transit (docs/ci.md W27-R08, W24-R27): the peer refuses it
//!   and kiln records the break.
//! - `blob_changed=<key>`: the server's: the object at `key` reads back from its store of bytes
//!   with its first byte changed, as a store whose bytes changed under kiln (docs/ci.md W27-R08):
//!   the read is refused and kiln records the break.
//! - `mantle_lost`: the server's: the first answer a Mantle cell gives each of kiln's mutations is
//!   lost on its way back, after the cell applied it (CP-T10.2, docs/blobs.md "The Mantle store"):
//!   kiln asks again with its identity, and a native upload is resumed.
//! - `feed_changed`: the first batch of a conversation store's feed this process signs for has a
//!   byte changed after it signed its hand-on, once (docs/ci.md W27-R08): the taker refuses it,
//!   kiln records the break, and the batch goes again as it was.
//! - `receipts_unkept`: the server's: no receipt of a batch it takes from a runner's conversation
//!   store is ever kept, so it says no Ack of any (an Ack waits for its receipt: consts-audit R4).
//!   Each runner's feed holds what its store commits after its first batch, as behind a kiln.db
//!   that never keeps a receipt, so a restarted server's store lacks what came since (docs/wire.md,
//!   "Restarts").
//! - `sync_file_changed`: the first piece of a file this runner syncs to another (`kiln run`), `.git`
//!   aside, has a byte changed after it hashed it, as in transit (docs/ci.md W27-R08): the other
//!   runner refuses the copy and kiln records the break.
//! - `copy_changed`: the server's: the first copy of kiln.db it hands on to a runner's learner has a
//!   byte in its middle changed after kiln hashed it, once, as in transit (docs/ci.md W27-R08's
//!   cluster group): the learner refuses it, kiln records the break, and the learner fetches another.
//! - `entry_changed`: the server's: the first entry with a write in it that it sends a learner that
//!   checks the log's checkpoints, of those after the learner said so, has its last byte changed,
//!   as has every copy of it sent again, the message made again whole, as in transit (docs/ci.md
//!   W27-R08's cluster group): the learner's next checkpoint doesn't match, kiln records the break,
//!   and the learner copies the database again.
//! - `relay_calls=<drop|swap|alter>`: the runner, a relay of its agents' calls to kiln, fails every
//!   second act it passes on (docs/ci.md W27-R08's relays): it numbers it on its stream and passes
//!   it on not at all; passes the act before on under its record; or changes a byte of the ask after
//!   recording it. kiln refuses the swapped and the altered, and records each, and the gap, as a
//!   break naming the runner.
//! - `relay_forward=<drop|swap|alter>`: the same of this runner's blue/green forwarder, every second
//!   frame of kiln's it passes on to its new build, which refuses it and tells kiln.
//! - `relay_words=<drop|swap|alter>` and `relay_frames=<drop|swap|alter>`: the server's: its relay
//!   of agents' words to the runners holding the agents they're for, and of reach links' frames,
//!   fail every second one so (a reach link's: frames 2, 4 …): a word the recipient's runner
//!   refuses, a frame the link's other end can't read; each break recorded naming kiln's server.
//! - `copy=<path>`: every frame this runner sends, appended to `path` as it goes below the channel
//!   (its lane and length, then its bytes), so a test can say what crossed the network (W27-R02).
//! - `keys=<path>`: each identity key this runner process makes (`crate::runner::identity`),
//!   appended to `path` under what it's for, so a test can look for those very bytes where they
//!   must never be (W27-R02).
//!
//! A leg's faults (`oom`, `disk`, `corrupt`, `fetch`) hit its first attempt only, so a retry shows
//! how it recovers; `drop` and `delay` are the connection's.

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
    /// Its jobs' sends taken this late (`lag=`).
    pub lag: Option<Duration>,
    pub oom_after: Option<Duration>,
    pub disk_mib: Option<u32>,
    pub corrupt: bool,
    pub fetch: bool,
    pub platform: Option<String>,
    pub rebuild_after: Option<Duration>,
    pub claim_fails: Option<Duration>,
    pub stop_job: Option<String>,
    pub refuse_job: Option<String>,
    /// What goes wrong below the channel: `shed`, `twice`, `late`, in the order given, drawn from
    /// `seed`.
    pub faults: Vec<crate::net::Shed>,
    pub seed: u64,
    pub fanout: Option<usize>,
    pub old: bool,
    /// Signs no hand-over (`unsigned`).
    pub unsigned: bool,
    /// The server's: sends a session's former holder its stale hand-over words (`stale_words`).
    pub stale_words: bool,
    /// The server's: answers each leg's identity this late (`identity_late=`).
    pub identity_late: Option<Duration>,
    /// The server's: a tarball it serves a leg changes after kiln signed its hand-on of it
    /// (`tarball_changed`).
    pub tarball_changed: bool,
    /// Acts as if git weren't installed: legs check out from the tarball (`no_git`).
    pub no_git: bool,
    /// An object it sends a peer changes after its hand-on (`peer_object_changed`).
    pub peer_object_changed: bool,
    /// The server's: the object at this key reads back changed from its store of bytes
    /// (`blob_changed=`).
    pub blob_changed: Option<String>,
    /// The server's: a Mantle cell's first answer to each of kiln's mutations is lost after the
    /// cell applied it (`mantle_lost`).
    pub mantle_lost: bool,
    /// The first batch of a feed it signs for changes after its hand-on, once (`feed_changed`).
    pub feed_changed: bool,
    /// The server's: says no Ack of any batch it takes from a runner's feed, its receipt never kept
    /// (`receipts_unkept`).
    pub receipts_unkept: bool,
    /// A file it syncs to another runner changes after it hashed it (`sync_file_changed`).
    pub sync_file_changed: bool,
    /// The server's: the first copy of kiln.db it hands on to a learner changes after it hashed it
    /// (`copy_changed`).
    pub copy_changed: bool,
    /// The server's: the first entry of its cluster log it sends a learner that checks the log
    /// changes on its way (`entry_changed`).
    pub entry_changed: bool,
    /// Its relay of its agents' calls fails every second one (`relay_calls=`).
    pub relay_calls: Option<RelayFault>,
    /// Its blue/green forwarder fails every second frame (`relay_forward=`).
    pub relay_forward: Option<RelayFault>,
    /// The server's: its relay of agents' words fails every second one (`relay_words=`).
    pub relay_words: Option<RelayFault>,
    /// The server's: its relay of reach links' frames fails every second one (`relay_frames=`).
    pub relay_frames: Option<RelayFault>,
    /// Where every frame this process sends is copied (`copy=`).
    pub copy: Option<std::path::PathBuf>,
    /// Where each identity key this process makes is copied (`keys=`).
    pub keys: Option<std::path::PathBuf>,
}

/// What a faulty relay does to every second thing it passes on (docs/ci.md W27-R08's relays).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayFault {
    /// Numbers it and records passing it on, then passes it on not at all.
    Drop,
    /// Passes another on in its place, under its record.
    Swap,
    /// Changes a byte of it after recording it.
    Alter,
}

impl RelayFault {
    fn parse(v: &str) -> Option<Self> {
        match v {
            "drop" => Some(Self::Drop),
            "swap" => Some(Self::Swap),
            "alter" => Some(Self::Alter),
            _ => None,
        }
    }

    /// Whether it fails the `n`th thing its relay passes on (from 1): every second one.
    pub fn strikes(n: u64) -> bool {
        n > 0 && n.is_multiple_of(2)
    }
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

/// `term/46:7`: one frame in 7 of lane `term` whose first byte is 46; `control:5`: one in 5 of
/// lane `control`. Done to as `act` says.
fn shed(v: &str, act: crate::net::Act) -> Option<crate::net::Shed> {
    let (what, every) = v.split_once(':')?;
    let (lane, variant) = match what.split_once('/') {
        Some((lane, variant)) => (lane, Some(variant.parse().ok()?)),
        None => (what, None),
    };
    let every: u64 = every.parse().ok().filter(|n| *n > 0)?;
    Some(crate::net::Shed { lane: crate::proto::lane_named(lane)?, variant, every, act })
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
            "lag" => c.lag = Some(d()?),
            "oom" => c.oom_after = Some(d()?),
            "disk" => c.disk_mib = Some(v.parse().map_err(|_| anyhow::anyhow!("chaos `disk` takes MiB, not `{v}`"))?),
            "corrupt" => c.corrupt = true,
            "fetch" => c.fetch = true,
            "platform" => {
                kiln_ci::plan::matrix::Platform::parse(v).map_err(|e| anyhow::anyhow!("chaos `platform`: {e}"))?;
                c.platform = Some(v.to_owned());
            }
            "rebuild" => c.rebuild_after = Some(d()?),
            "claim" => c.claim_fails = Some(d()?),
            "stop" if !v.is_empty() => c.stop_job = Some(v.to_owned()),
            "refuse" if !v.is_empty() => c.refuse_job = Some(v.to_owned()),
            "shed" | "twice" | "late" | "unknown" | "garble" => {
                let act = match k {
                    "shed" => crate::net::Act::Drop,
                    "twice" => crate::net::Act::Twice,
                    "late" => crate::net::Act::Late,
                    "unknown" => crate::net::Act::Unknown(0),
                    _ => crate::net::Act::Garble,
                };
                c.faults.push(shed(v, act).ok_or_else(|| anyhow::anyhow!("chaos `{k}` takes <lane>[/<variant>]:<every> (term/46:7), not `{v}`"))?);
            }
            "fanout" => c.fanout = Some(v.parse().ok().filter(|n: &usize| *n > 0).ok_or_else(|| anyhow::anyhow!("chaos `fanout` takes a number of frames, not `{v}`"))?),
            "old" => c.old = true,
            "unsigned" => c.unsigned = true,
            "stale_words" => c.stale_words = true,
            "identity_late" => c.identity_late = Some(d()?),
            "tarball_changed" => c.tarball_changed = true,
            "no_git" => c.no_git = true,
            "peer_object_changed" => c.peer_object_changed = true,
            "feed_changed" => c.feed_changed = true,
            "receipts_unkept" => c.receipts_unkept = true,
            "blob_changed" => {
                anyhow::ensure!(!v.is_empty(), "chaos `blob_changed` takes an object's key");
                c.blob_changed = Some(v.to_owned());
            }
            "sync_file_changed" => c.sync_file_changed = true,
            "copy_changed" => c.copy_changed = true,
            "entry_changed" => c.entry_changed = true,
            "mantle_lost" => c.mantle_lost = true,
            "relay_calls" | "relay_forward" | "relay_words" | "relay_frames" => {
                let f = RelayFault::parse(v).ok_or_else(|| anyhow::anyhow!("chaos `{k}` takes drop, swap or alter, not `{v}`"))?;
                match k {
                    "relay_calls" => c.relay_calls = Some(f),
                    "relay_forward" => c.relay_forward = Some(f),
                    "relay_words" => c.relay_words = Some(f),
                    _ => c.relay_frames = Some(f),
                }
            }
            "copy" | "keys" => {
                anyhow::ensure!(!v.is_empty(), "chaos `{k}` takes a file's path");
                let path = Some(std::path::PathBuf::from(v));
                if k == "copy" {
                    c.copy = path;
                } else {
                    c.keys = path;
                }
            }
            "seed" => c.seed = v.parse().map_err(|_| anyhow::anyhow!("chaos `seed` takes a number, not `{v}`"))?,
            other => anyhow::bail!("chaos has no fault `{other}`: drop, down, delay, lag, oom, disk, corrupt, fetch, platform, rebuild, claim, stop=<job>, refuse=<job>, shed, twice, late, unknown, garble, seed, fanout, old, unsigned, stale_words, identity_late, tarball_changed, no_git, peer_object_changed, blob_changed, mantle_lost, feed_changed, sync_file_changed, copy_changed, entry_changed, relay_calls, relay_forward, relay_words, relay_frames, copy or keys"),
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

/// An identity key this process just made, for `slot` (`crate::runner::identity`): copied where
/// `keys=` says, under what it's for. `pem` is asked for only then: nothing serializes a key
/// without it.
pub fn made_key(slot: &str, pem: impl FnOnce() -> Option<String>) {
    use std::io::Write as _;
    let Some(path) = get().and_then(|c| c.keys.as_ref()) else { return };
    let Some(pem) = pem() else {
        tracing::warn!(slot, "chaos: a key made can't be copied");
        return;
    };
    let wrote = std::fs::OpenOptions::new().create(true).append(true).open(path).and_then(|mut f| write!(f, "{slot}\n{pem}\n"));
    if let Err(e) = wrote {
        tracing::warn!(error = %e, path = %path.display(), "chaos: can't copy a key made");
    }
}

/// A leg's faults: its first attempt's.
pub fn for_leg(attempt: u32) -> Option<&'static Chaos> {
    get().filter(|_| attempt <= 1)
}

/// Whether this is the first claim `claim=` fails (once a process).
static CLAIM_FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The hold before a claim fails (`claim=`), for the first claim only.
pub fn claim_fails() -> Option<Duration> {
    get().and_then(|c| c.claim_fails).filter(|_| !CLAIM_FAILED.swap(true, Ordering::Relaxed))
}

/// Whether the runner's first refusal `refuse=` makes is had (once a process).
static REFUSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the runner's admission refuses a leg of `job` (a job's id, its callers' before it:
/// `call/test`) it's sent (`refuse=`): the first of that job's only.
pub fn refuses(job: &str) -> bool {
    get().and_then(|c| c.refuse_job.as_deref()).is_some_and(|j| job.rsplit('/').next() == Some(j)) && !REFUSED.swap(true, Ordering::Relaxed)
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
                lag: None,
                oom_after: Some(Duration::from_secs(1)),
                disk_mib: Some(8),
                corrupt: true,
                fetch: false,
                platform: None,
                rebuild_after: None,
                claim_fails: None,
                stop_job: None,
                refuse_job: None,
                faults: Vec::new(),
                seed: 0,
                fanout: None,
                old: false,
                unsigned: false,
                stale_words: false,
                identity_late: None,
                tarball_changed: false,
                no_git: false,
                peer_object_changed: false,
                blob_changed: None,
                mantle_lost: false,
                feed_changed: false,
                receipts_unkept: false,
                sync_file_changed: false,
                copy_changed: false,
                entry_changed: false,
                relay_calls: None,
                relay_forward: None,
                relay_words: None,
                relay_frames: None,
                copy: None,
                keys: None
            }
        );
        assert!(parse("tarball_changed")?.tarball_changed && parse("no_git")?.no_git && parse("peer_object_changed")?.peer_object_changed);
        assert_eq!(parse("blob_changed=artifacts/7/app")?.blob_changed.as_deref(), Some("artifacts/7/app"));
        assert!(parse("blob_changed").is_err(), "it names an object's key");
        assert!(parse("feed_changed")?.feed_changed);
        assert!(parse("receipts_unkept")?.receipts_unkept);
        assert!(parse("mantle_lost")?.mantle_lost);
        assert!(parse("sync_file_changed")?.sync_file_changed);
        // W27-R08's cluster group: a copy, and an entry, changed on their way.
        assert!(parse("copy_changed")?.copy_changed && parse("entry_changed")?.entry_changed);
        // W27-R08's relays, each failing every second thing it passes on.
        let c = parse("relay_calls=drop,relay_forward=swap,relay_words=alter,relay_frames=drop")?;
        assert_eq!((c.relay_calls, c.relay_forward, c.relay_words, c.relay_frames), (Some(RelayFault::Drop), Some(RelayFault::Swap), Some(RelayFault::Alter), Some(RelayFault::Drop)));
        assert!(parse("relay_calls=lose").is_err() && parse("relay_frames").is_err(), "drop, swap or alter");
        assert_eq!((1..=6).filter(|n| RelayFault::strikes(*n)).collect::<Vec<_>>(), vec![2, 4, 6]);
        assert_eq!(parse("platform=darwin/arm64")?.platform.as_deref(), Some("darwin/arm64"));
        // W27-R02's test copies: what a runner sends, and the keys it makes.
        let c = parse("copy=/tmp/a.frames,keys=/tmp/a.keys")?;
        assert_eq!((c.copy.as_deref(), c.keys.as_deref()), (Some(std::path::Path::new("/tmp/a.frames")), Some(std::path::Path::new("/tmp/a.keys"))));
        assert!(parse("copy=").is_err() && parse("keys").is_err(), "each names its file");
        assert!(parse("drop=6s,old")?.old);
        assert!(parse("unsigned")?.unsigned && parse("stale_words")?.stale_words);
        assert_eq!(parse("identity_late=6s")?.identity_late, Some(Duration::from_secs(6)));
        assert!(parse("identity_late").is_err(), "it takes a duration");
        assert!(parse("fetch")?.fetch);
        assert_eq!(parse("rebuild=4s")?.rebuild_after, Some(Duration::from_secs(4)));
        assert_eq!(parse("claim=3s")?.claim_fails, Some(Duration::from_secs(3)));
        assert_eq!(parse("stop=stopper")?.stop_job.as_deref(), Some("stopper"));
        assert!(parse("stop").is_err());
        assert_eq!(parse("refuse=sim")?.refuse_job.as_deref(), Some("sim"));
        assert!(parse("refuse").is_err());
        assert_eq!(parse("drop=5s,down=30s")?.down_for, Some(Duration::from_secs(30)));
        assert_eq!(parse("lag=300ms")?.lag, Some(Duration::from_millis(300)));
        assert!(parse("platform=plan9/mips").is_err());
        assert!(parse("flood=1s").is_err_and(|e| e.to_string().contains("no fault `flood`")));
        assert!(parse("oom=soon").is_err());
        assert_eq!(parse("")?, Chaos::default());
        // Frames lost below the channel: a lane by name or number, of one message variant or all.
        use crate::net::{Act, Shed};
        assert_eq!(parse("shed=term/46:7")?.faults, [Shed { lane: crate::proto::LANE_TERM, variant: Some(46), every: 7, act: Act::Drop }]);
        assert_eq!(parse("shed=0:5")?.faults, [Shed { lane: crate::proto::LANE_CONTROL, variant: None, every: 5, act: Act::Drop }]);
        assert!(parse("shed=term:0").is_err() && parse("shed=nowhere:3").is_err() && parse("shed=term/x:3").is_err());
        // Frames sent twice, or late, beside those dropped, each its own.
        let faults = parse("shed=term/46:7,twice=term/46:5,late=control:9")?.faults;
        assert_eq!(faults.iter().map(|f| (f.act, f.every)).collect::<Vec<_>>(), [(Act::Drop, 7), (Act::Twice, 5), (Act::Late, 9)]);
        // Frames made unreadable (docs/wire.md, "What can't be read"): a kind the receiver doesn't
        // know, one past what its sender's enum has, or garbled.
        let faults = parse("unknown=control/10:3,garble=term:4")?.faults;
        assert_eq!(faults.iter().map(|f| (f.act, f.variant, f.every)).collect::<Vec<_>>(), [(Act::Unknown(0), Some(10), 3), (Act::Garble, None, 4)]);
        assert_eq!(faults.first().map(|f| f.past(52).act), Some(Act::Unknown(52)));
        assert_eq!(faults.get(1).map(|f| f.past(52).act), Some(Act::Garble));
        assert!(parse("twice=term:0").is_err() && parse("late=nowhere:2").is_err());
        assert_eq!(parse("shed=term:3,seed=42")?.seed, 42);
        assert!(parse("seed=x").is_err());
        assert_eq!(parse("fanout=32")?.fanout, Some(32));
        assert!(parse("fanout=0").is_err());
        Ok(())
    }
}

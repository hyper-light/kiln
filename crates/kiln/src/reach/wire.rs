//! What two runners say inside an encrypted link (`docs/reach.md` §4): postcard messages, each
//! behind a 4-byte big-endian length. The runner that opened the link asks ([`Ask`]); the one it
//! reached does the work and answers ([`Say`]).
//!
//! Everything a run says is numbered by byte offset, so a link that drops loses nothing: the asking
//! side opens a new link, names the session and how far it got ([`Ask::Hello`]), and the other side
//! replays from there. Processes keep running while no link is up.

use serde::{Deserialize, Serialize};

/// The inner protocol's version; a peer with another is told so and the link ends.
pub const VERSION: u32 = 1;

/// The largest message either side sends or accepts.
pub const MAX_MESSAGE: usize = 4 << 20;

/// Output a run may have sent and not yet had acknowledged.
pub const WINDOW: u64 = 4 << 20;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Ask {
    /// First on every link: which session this is (the same across links) and, for each run the
    /// asker still follows, the offset it has output up to.
    Hello { version: u32, session: String, resume: Vec<Resume> },
    Run(Box<RunSpec>),
    Stdin { id: u64, data: Vec<u8> },
    StdinEnd { id: u64 },
    /// Output up to `upto` arrived: it may be forgotten, and more sent.
    Ack { id: u64, upto: u64 },
    Signal { id: u64, signal: Signal },
    /// The asker has everything of this run (its exit too) and needs nothing more of it.
    Forget { id: u64 },
    Sync(SyncAsk),
    Ping { at: i64 },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Resume {
    pub id: u64,
    /// The output offset the asker has up to.
    pub from: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Say {
    /// The answer to Hello: who answers, and which runs it still has (`resumed`) or lost.
    Welcome { version: u32, host: Host, resumed: Vec<u64>, lost: Vec<u64> },
    /// The link is refused (another version, a runner this one does not accept), with why.
    Refused { reason: String },
    Started { id: u64, pid: u32, cwd: String },
    /// Output at `offset` of the run's one stream of output (stdout and stderr interleaved as they
    /// came, each piece tagged). A jump in offsets means output was dropped (see `Exit::lost`).
    Output { id: u64, offset: u64, stream: Stream, data: Vec<u8> },
    /// The run ended. `end`: the offset its output ends at; `lost`: bytes it dropped because
    /// nobody read them in time.
    Exit { id: u64, code: Option<i32>, signal: Option<i32>, error: Option<String>, end: u64, lost: u64 },
    Sync(SyncSay),
    Pong { at: i64 },
}

/// The machine a link reached.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Host {
    pub name: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub home: String,
    pub cpus: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Stream {
    Out,
    Err,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Signal {
    Interrupt,
    Terminate,
    Kill,
}

/// A command to run.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RunSpec {
    /// The asker's id for it, unique within its session.
    pub id: u64,
    /// Shell text, run by bash (`sh` where there is no bash).
    pub command: String,
    /// Where: absolute, `~`-relative, or relative to the synced copy (`mirror`); the home folder
    /// without one.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Run inside the copy a sync made (its key, see [`SyncAsk::Plan`]); `cwd` is then relative to it.
    #[serde(default)]
    pub mirror: Option<String>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// The asker sends input; without it stdin is empty.
    #[serde(default)]
    pub stdin: bool,
    /// Ended (and reported as such) after this long.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// How long it may wait for its asker once no link is up before it is stopped.
    #[serde(default)]
    pub linger_ms: Option<u64>,
    /// A named shell whose working folder and exported variables carry from one run to the next.
    #[serde(default)]
    pub shell: Option<String>,
}

/// One file or folder of a tree being synced.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    /// Relative, `/`-separated, never `..`.
    pub path: String,
    pub kind: EntryKind,
    pub size: u64,
    /// Modification time, nanoseconds since the epoch.
    pub mtime_ns: i64,
    /// Unix permission bits (0 where there are none).
    pub mode: u32,
    /// A symlink's target.
    #[serde(default)]
    pub target: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SyncAsk {
    /// The tree as the asker has it, in pieces (`more` until the last). `key` names the copy on
    /// the other side, kept between syncs so the next one sends only what changed.
    Plan { key: String, entries: Vec<Entry>, more: bool },
    /// A piece of a file the other side asked for.
    File { key: String, path: String, offset: u64, data: Vec<u8>, last: bool },
    /// Every file asked for was sent: make the copy match the plan.
    Commit { key: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SyncSay {
    /// The files to send (they differ, or are missing); `root` is where the copy lives.
    Need { key: String, root: String, paths: Vec<String> },
    Done { key: String, root: String, written: u32, removed: u32, bytes: u64 },
    Failed { key: String, error: String },
}

/// Put a message behind its length.
pub fn frame<T: Serialize>(msg: &T) -> anyhow::Result<Vec<u8>> {
    let body = postcard::to_allocvec(msg).map_err(|e| anyhow::anyhow!("encode: {e}"))?;
    if body.len() > MAX_MESSAGE {
        anyhow::bail!("message of {} bytes is over the {MAX_MESSAGE}-byte limit", body.len());
    }
    let len = u32::try_from(body.len())?;
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Collects a link's plaintext and hands out whole messages.
#[derive(Default)]
pub struct Deframer {
    buf: Vec<u8>,
}

impl Deframer {
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// The next whole message, if one arrived. An error (too large, undecodable) ends the link.
    pub fn next<T: for<'de> Deserialize<'de>>(&mut self) -> anyhow::Result<Option<T>> {
        let Some(head) = self.buf.first_chunk::<4>() else { return Ok(None) };
        let len = usize::try_from(u32::from_be_bytes(*head))?;
        if len > MAX_MESSAGE {
            anyhow::bail!("message of {len} bytes is over the {MAX_MESSAGE}-byte limit");
        }
        let Some(body) = self.buf.get(4..4 + len) else { return Ok(None) };
        let msg = postcard::from_bytes(body).map_err(|e| anyhow::anyhow!("decode: {e}"))?;
        self.buf.drain(..4 + len);
        Ok(Some(msg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_survive_any_split() -> anyhow::Result<()> {
        let msgs = vec![
            Ask::Hello { version: VERSION, session: "s".into(), resume: vec![Resume { id: 3, from: 77 }] },
            Ask::Run(Box::new(RunSpec { id: 1, command: "cargo test".into(), cwd: Some("crates/kiln".into()), ..RunSpec::default() })),
            Ask::Stdin { id: 1, data: vec![0, 1, 2, 255] },
            Ask::Sync(SyncAsk::Plan { key: "k".into(), entries: vec![Entry { path: "a/b".into(), kind: EntryKind::File, size: 3, mtime_ns: -1, mode: 0o644, target: None }], more: false }),
        ];
        let mut bytes = Vec::new();
        for m in &msgs {
            bytes.extend(frame(m)?);
        }
        // Byte by byte: nothing comes out early, everything comes out whole and in order.
        let mut d = Deframer::default();
        let mut got = Vec::new();
        for b in &bytes {
            d.push(std::slice::from_ref(b));
            while let Some(m) = d.next::<Ask>()? {
                got.push(m);
            }
        }
        assert_eq!(got, msgs);
        Ok(())
    }

    #[test]
    fn an_oversized_or_garbled_message_is_an_error() {
        let mut d = Deframer::default();
        d.push(&u32::MAX.to_be_bytes());
        assert!(d.next::<Say>().is_err());
        let mut d = Deframer::default();
        d.push(&[0, 0, 0, 2, 0xff, 0xff]);
        assert!(d.next::<Say>().is_err());
    }
}

//! What two runners say inside an encrypted link (`docs/reach.md` §4): postcard messages, each
//! behind a 4-byte big-endian length. The runner that opened the link asks ([`Ask`]); the one it
//! reached does the work and answers ([`Say`]).
//!
//! Everything a run says is numbered by byte offset, so a link that drops loses nothing: the asking
//! side opens a new link, names the session and how far it got ([`Ask::Hello`]), and the other side
//! replays from there. Processes keep running while no link is up.

use serde::{Deserialize, Serialize};

/// The inner protocol's version this build speaks. A link runs at the asker's version when the
/// doer speaks it too (from [`MIN_VERSION`] up); a runner from before speaks only 1, and refuses a
/// Hello at any other, so the asker asks again at 1.
pub const VERSION: u32 = 2;

/// The oldest version a link may run at.
pub const MIN_VERSION: u32 = 1;

/// The version a link carries custody records at ([`Ask::Custody`], [`Say::Custody`]; docs/ci.md
/// W27-R08): never on a link below it.
pub const CUSTODY: u32 = 2;

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
    /// A piece of something this runner hands the other to take over (an agent session moving
    /// there: its record, its timeline). `offset` 0 starts it (again); the other side answers with
    /// [`Say::AdoptGot`] as pieces land and [`Say::Adopted`] once it took it over, or couldn't.
    Adopt { id: u64, offset: u64, data: Vec<u8>, last: bool },
    /// An object of the other runner's cache, for a peer that lacks it (docs/ci.md W24-R27): from
    /// `offset`, at most [`WINDOW`] of it, which the other side sends as [`Say::ObjectData`] (or
    /// [`Say::ObjectMissing`]); the asker asks again from where it got to. Appended last:
    /// postcard numbers variants by place.
    Object { id: u64, repo: String, digest: String, offset: u64 },
    /// A custody record of what this side hands the other, as JSON (docs/ci.md W27-R08): a sync's
    /// workspace (before its [`SyncAsk::Commit`]) or a run (before its [`Ask::Run`]), its hand-on
    /// and the delegation the other side takes it under, or why it goes unsigned. Only on a link at
    /// [`CUSTODY`] or later. Appended last: postcard numbers variants by place.
    Custody { json: String },
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
    /// Bytes of a hand-over that arrived so far: the asker keeps at most [`WINDOW`] beyond this in
    /// flight.
    AdoptGot { id: u64, bytes: u64 },
    /// The hand-over was taken over (`error` None), or why not.
    Adopted { id: u64, error: Option<String> },
    /// A piece of an object asked for ([`Ask::Object`]): `last` once its end is in it.
    ObjectData { id: u64, offset: u64, data: Vec<u8>, last: bool },
    /// The object asked for isn't here, for that repository.
    ObjectMissing { id: u64 },
    /// A custody record of this side's, as JSON (docs/ci.md W27-R08): its receipt of a sync's
    /// workspace (before its [`SyncSay::Done`]) or of a run (before it starts), or its hand-on of a
    /// run's output (before its [`Say::Exit`]), or why it goes unsigned. Only on a link at
    /// [`CUSTODY`] or later. Appended last.
    Custody { json: String },
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
    /// The tree as the asker has it, in pieces (`start` on the first, `more` until the last).
    /// `key` names the copy on the other side, kept between syncs so the next one sends only what
    /// changed.
    Plan { key: String, entries: Vec<Entry>, start: bool, more: bool },
    /// A piece of a file the other side asked for.
    File { key: String, path: String, offset: u64, data: Vec<u8>, last: bool },
    /// Every file asked for was sent: make the copy match the plan.
    Commit { key: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SyncSay {
    /// The files to send (they differ, or are missing); `root` is where the copy lives.
    Need { key: String, root: String, paths: Vec<String> },
    /// File bytes written so far: the asker keeps at most [`WINDOW`] beyond this in flight.
    Got { key: String, bytes: u64 },
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
    pub fn take<T: for<'de> Deserialize<'de>>(&mut self) -> anyhow::Result<Option<T>> {
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
            Ask::Sync(SyncAsk::Plan { key: "k".into(), entries: vec![Entry { path: "a/b".into(), kind: EntryKind::File, size: 3, mtime_ns: -1, mode: 0o644, target: None }], start: true, more: false }),
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
            while let Some(m) = d.take::<Ask>()? {
                got.push(m);
            }
        }
        assert_eq!(got, msgs);
        Ok(())
    }

    #[test]
    fn objects_are_appended_after_every_older_variant() -> anyhow::Result<()> {
        let first = |b: Vec<u8>| b.first().copied();
        let adopt = first(postcard::to_allocvec(&Ask::Adopt { id: 1, offset: 0, data: Vec::new(), last: true })?);
        let object = first(postcard::to_allocvec(&Ask::Object { id: 1, repo: "acme/app".into(), digest: "d".into(), offset: 0 })?);
        anyhow::ensure!(object == adopt.map(|i| i + 1), "Ask::Object comes right after Adopt");
        let adopted = first(postcard::to_allocvec(&Say::Adopted { id: 1, error: None })?);
        let data = first(postcard::to_allocvec(&Say::ObjectData { id: 1, offset: 0, data: vec![1], last: true })?);
        let missing = first(postcard::to_allocvec(&Say::ObjectMissing { id: 1 })?);
        anyhow::ensure!(data == adopted.map(|i| i + 1) && missing == adopted.map(|i| i + 2), "Say's come after Adopted");
        Ok(())
    }

    /// W27-R08 (the Transfers plan's step 12): custody frames come after every older variant, so a
    /// link at version 1 reads as it did; and a link carries them only from [`CUSTODY`] on.
    #[test]
    fn w27_r08_custody_frames_are_appended_after_every_older_variant() -> anyhow::Result<()> {
        let first = |b: Vec<u8>| b.first().copied();
        let object = first(postcard::to_allocvec(&Ask::Object { id: 1, repo: "acme/app".into(), digest: "d".into(), offset: 0 })?);
        let custody = first(postcard::to_allocvec(&Ask::Custody { json: "{}".into() })?);
        anyhow::ensure!(custody == object.map(|i| i + 1), "Ask::Custody comes right after Object");
        let missing = first(postcard::to_allocvec(&Say::ObjectMissing { id: 1 })?);
        let said = first(postcard::to_allocvec(&Say::Custody { json: "{}".into() })?);
        anyhow::ensure!(said == missing.map(|i| i + 1), "Say::Custody comes right after ObjectMissing");
        anyhow::ensure!(MIN_VERSION == 1 && CUSTODY > MIN_VERSION && VERSION >= CUSTODY, "a build from before speaks 1, which carries no custody");
        Ok(())
    }

    #[test]
    fn an_oversized_or_garbled_message_is_an_error() {
        let mut d = Deframer::default();
        d.push(&u32::MAX.to_be_bytes());
        assert!(d.take::<Say>().is_err());
        let mut d = Deframer::default();
        d.push(&[0, 0, 0, 2, 0xff, 0xff]);
        assert!(d.take::<Say>().is_err());
    }
}

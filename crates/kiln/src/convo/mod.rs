//! Conversations, kept per event, replicated and searchable (`docs/conversations.md`): what each
//! agent session said and did, on disk as it happens, so it is resumed exactly after a restart,
//! resumed on another machine, and found again.
//!
//! - [`rows`]: what a session's updates become in the store (no engine in sight).
//! - [`store`]: a machine's store, its writer thread, and the tee that feeds it from the sessions.
//! - [`turso`]: the engine: Turso, embedded.
//! - [`record`]: the harness's own record (transcripts, rollouts), followed into the store.
//! - [`feed`]: a runner's store sent to the server's, over the runner's link.
//! - [`resume`]: a session packed from the server's store, to go on on another machine.
//! - [`mirror`]: runners that keep a copy of the server's store.
//! - [`telemetry`]: what each store and feed is doing, for the System page and its dashboards.
//! - [`board`]: the server's board of every store: their state, series and what happened lately.

pub mod amphora;
pub mod board;
pub mod feed;
pub mod inbox;
pub mod mirror;
pub mod record;
pub mod resume;
pub mod rows;
pub mod seam;
pub mod sql;
pub mod store;
pub mod telemetry;
pub mod turso;

/// The store file in a machine's state folder.
pub const FILE: &str = "conversations.db";
/// A mirror's copy of the server's store, beside a runner's own (`mirror`).
pub const MIRROR_FILE: &str = "mirror.db";

/// Which engine keeps the conversation store (CP-T1.1): Turso, unless `KILN_CONVERSATION_ENGINE` says `amphora`.
/// Both run the one store kept in SQL (`sqlstore.rs`); Amphora's engine is Turso's lineage with its pages checksummed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Turso,
    Amphora,
}

/// The variable that chooses the conversation store's engine ([`Kind::configured`]).
pub const ENGINE_VAR: &str = "KILN_CONVERSATION_ENGINE";

impl Kind {
    /// Every engine, for what must hold on each (tests).
    pub const ALL: [Kind; 2] = [Kind::Turso, Kind::Amphora];

    /// The engine `KILN_CONVERSATION_ENGINE` names (`turso` or `amphora`; unset or empty: Turso). Anything else is an
    /// error naming what it takes, never a guess.
    pub fn configured() -> anyhow::Result<Self> {
        match std::env::var(ENGINE_VAR) {
            Err(std::env::VarError::NotPresent) => Ok(Self::Turso),
            Err(e) => Err(anyhow::anyhow!("{ENGINE_VAR}: {e}")),
            Ok(v) => Self::named(&v),
        }
    }

    /// The engine called `name` (`turso` or `amphora`, any case; empty: Turso).
    pub fn named(name: &str) -> anyhow::Result<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "" | "turso" => Ok(Self::Turso),
            "amphora" => Ok(Self::Amphora),
            other => anyhow::bail!("{ENGINE_VAR} is `{other}`: it takes `turso` or `amphora`"),
        }
    }

    /// Its store at `path`, made and migrated as needed. A file the other engine made is refused ([`OtherEngine`]),
    /// never opened: Amphora's engine would open Turso's without its page checksums, and pages Turso writes in
    /// Amphora's leave their checksums stale, which Amphora then refuses page by page (CP-T1.1, seam review 3).
    pub fn open(self, path: &std::path::Path) -> anyhow::Result<Box<dyn store::Engine>> {
        if let Some(made_by) = made_by(path)?
            && made_by != self
        {
            return Err(OtherEngine { file: path.to_path_buf(), made_by, asked: self }.into());
        }
        Ok(match self {
            Self::Turso => Box::new(turso::Turso::open(path)?),
            Self::Amphora => Box::new(amphora::Amphora::open(path)?),
        })
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Turso => "turso",
            Self::Amphora => "amphora",
        })
    }
}

/// A store's file the other engine made, refused by name (CP-T1.1): it stays where it is, whole, never set aside, and
/// the person is told which engine keeps it and how to go on.
#[derive(Debug)]
pub struct OtherEngine {
    pub file: std::path::PathBuf,
    pub made_by: Kind,
    pub asked: Kind,
}

impl std::fmt::Display for OtherEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (file, made_by, asked) = (self.file.display(), self.made_by, self.asked);
        write!(
            f,
            "the conversation store {file} was made by {made_by}, and {ENGINE_VAR} asks for {asked}: it isn't opened, and conversations aren't kept until it is. Set {ENGINE_VAR}={made_by} to go on with it, or move it (and its -wal) away for {asked} to begin a new one"
        )
    }
}

impl std::error::Error for OtherEngine {}

/// The bytes each page keeps free at its end, by which engine made the file: the database header's byte 20 (SQLite's
/// file format, sqlite.org/fileformat.html §1.3, "Bytes of unused reserved space at the end of each page"). Turso
/// keeps none (vendor/turso/core/storage/sqlite3_ondisk.rs, its header's default); Amphora's engine, built with
/// `checksum` as kiln builds it, keeps 8 for each page's checksum (vendor/amphora/crates/engine/storage/checksum.rs,
/// `CHECKSUM_SIZE`). Format values, as those sources state them.
const RESERVED_AT: usize = 20;
const TURSO_RESERVED: u8 = 0;
const AMPHORA_RESERVED: u8 = 8;
/// The database header's first 16 bytes and its length (the same format, §1.3).
const DB_MAGIC: &[u8; 16] = b"SQLite format 3\0";
const DB_HEADER: usize = 100;
/// The write-ahead log's header and each frame's (§4.1): its magic (either byte order), its page size at 8, and a
/// frame's page number at the frame's start.
const WAL_HEADER: u64 = 32;
const WAL_FRAME_HEADER: u64 = 24;
const WAL_MAGIC: [u32; 2] = [0x377f_0682, 0x377f_0683];

/// Which engine made the store at `path`, by its header (None: there's no database there yet, or it isn't one, and
/// opening it says so). A store never checkpointed keeps its header in its log alone: read there then.
pub fn made_by(path: &std::path::Path) -> anyhow::Result<Option<Kind>> {
    use std::io::{Read, Seek, SeekFrom};
    let of = |header: &[u8]| -> Option<Kind> {
        if header.len() < DB_HEADER || header.get(..DB_MAGIC.len()) != Some(DB_MAGIC.as_slice()) {
            return None;
        }
        match header.get(RESERVED_AT).copied() {
            Some(TURSO_RESERVED) => Some(Kind::Turso),
            Some(AMPHORA_RESERVED) => Some(Kind::Amphora),
            _ => None,
        }
    };
    let read_at = |f: &mut std::fs::File, at: u64, n: usize| -> anyhow::Result<Option<Vec<u8>>> {
        f.seek(SeekFrom::Start(at))?;
        let mut buf = vec![0u8; n];
        match f.read_exact(&mut buf) {
            Ok(()) => Ok(Some(buf)),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
            Err(e) => Err(e.into()),
        }
    };
    match std::fs::File::open(path) {
        Ok(mut f) => {
            if let Some(kind) = read_at(&mut f, 0, DB_HEADER)?.as_deref().and_then(of) {
                return Ok(Some(kind));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(anyhow::Error::from(e).context(format!("read {}", path.display()))),
    }
    let wal = beside(path, "-wal");
    let mut f = match std::fs::File::open(&wal) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(anyhow::Error::from(e).context(format!("read {}", wal.display()))),
    };
    let Some(h) = read_at(&mut f, 0, 12)? else { return Ok(None) };
    let word = |b: &[u8], at: usize| b.get(at..at.saturating_add(4)).and_then(|w| <[u8; 4]>::try_from(w).ok()).map(u32::from_be_bytes);
    if !word(&h, 0).is_some_and(|m| WAL_MAGIC.contains(&m)) {
        return Ok(None);
    }
    let page = u64::from(word(&h, 8).unwrap_or(0));
    if !usize::try_from(page).is_ok_and(|p| p >= DB_HEADER) {
        return Ok(None);
    }
    let len = f.metadata()?.len();
    // The first frame of page 1 holds the header; frames are read by their page number alone, one at a time, as far as
    // the log goes.
    let mut at = WAL_HEADER;
    while at.saturating_add(WAL_FRAME_HEADER).saturating_add(page) <= len {
        let Some(fh) = read_at(&mut f, at, 4)? else { break };
        if word(&fh, 0) == Some(1) {
            return Ok(read_at(&mut f, at.saturating_add(WAL_FRAME_HEADER), DB_HEADER)?.as_deref().and_then(of));
        }
        at = at.saturating_add(WAL_FRAME_HEADER).saturating_add(page);
    }
    Ok(None)
}

/// The store at `path` on the engine configured ([`Kind::configured`]).
pub fn open_engine(path: &std::path::Path) -> anyhow::Result<Box<dyn store::Engine>> {
    Kind::configured()?.open(path)
}

/// This machine's conversation store in `dir`, its writer started: None when it is turned off
/// (`KILN_CONVERSATIONS=off`) or can't be opened, and the machine runs as it did without one.
/// `keep_closed`: how long the records of sessions that ended are kept (None: for good).
pub fn open(dir: &std::path::Path, upkeep: store::Upkeep) -> Option<store::Store> {
    open_file(dir, FILE, upkeep)
}

/// Whether conversation stores are turned off on this machine (`KILN_CONVERSATIONS=off`).
pub fn off() -> bool {
    std::env::var("KILN_CONVERSATIONS").is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "off" | "0" | "false" | "no"))
}

/// The store kept in file `file` of `dir` (see [`open`]).
pub fn open_file(dir: &std::path::Path, file: &str, upkeep: store::Upkeep) -> Option<store::Store> {
    if off() {
        tracing::info!("the conversation store is off (KILN_CONVERSATIONS)");
        return None;
    }
    let kind = configured_said()?;
    opened(kind, &dir.join(file), upkeep).ok()
}

/// The engine configured, or None with the error said loudly: nothing is opened, made or set aside on a guess.
fn configured_said() -> Option<Kind> {
    Kind::configured().map_err(|e| tracing::error!(error = %format!("{e:#}"), "the conversation store wasn't opened; conversations aren't kept")).ok()
}

/// The store at `path` on `kind`, its writer started, said either way. A file the other engine made is said as an
/// error, naming both and what to do ([`OtherEngine`]).
fn opened(kind: Kind, path: &std::path::Path, upkeep: store::Upkeep) -> anyhow::Result<store::Store> {
    let opened = kind.open(path).and_then(|engine| store::Store::start(engine, upkeep));
    match &opened {
        Ok(_) => tracing::info!(file = %path.display(), engine = %kind, "conversations are kept in the store"),
        Err(e) if e.downcast_ref::<OtherEngine>().is_some() => tracing::error!(file = %path.display(), error = %format!("{e:#}"), "the conversation store is another engine's"),
        Err(e) => tracing::warn!(file = %path.display(), error = %format!("{e:#}"), "the conversation store couldn't be opened; conversations aren't kept"),
    }
    opened
}

/// Move a store's file at `from`, once let go of ([`store::Store::close`]), to `to`, with its log
/// beside it: for the process that goes on with it there (a new build taking over from the runner
/// being upgraded keeps that runner's store, `crate::bluegreen`). Never over a file at `to`.
pub fn move_file(from: &std::path::Path, to: &std::path::Path) -> anyhow::Result<()> {
    for s in ["", "-wal", "-shm"] {
        anyhow::ensure!(!beside(to, s).exists(), "{} is there already", beside(to, s).display());
    }
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::rename(from, to).map_err(|e| anyhow::Error::from(e).context(format!("move {} to {}", from.display(), to.display())))?;
    for s in ["-wal", "-shm"] {
        match std::fs::rename(beside(from, s), beside(to, s)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(anyhow::Error::from(e).context(format!("move {}", beside(from, s).display()))),
        }
    }
    // The renames themselves on disk (not on Windows, which opens no folder).
    if let Some(dir) = to.parent()
        && let Ok(d) = std::fs::File::open(dir)
    {
        let _ = d.sync_all();
    }
    Ok(())
}

/// What a store's file set aside is called, after its own name ([`set_aside`]): the first such.
pub const SET_ASIDE: &str = ".unopened";

/// A file beside `path`: its name with `suffix` added (a store's log is its name and `-wal`).
fn beside(path: &std::path::Path, suffix: &str) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    std::path::PathBuf::from(name)
}

/// Set the store's file at `path` aside, with its log, kept whole for a person to look at: moved
/// beside it as `<file>.unopened` (`.unopened.2` and on, if that's taken), its log by the same name
/// and `-wal`, so that it still opens as it is there; never over a file. Where it went.
pub fn set_aside(path: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    let to = (1..=100u32)
        .map(|i| beside(path, &if i == 1 { SET_ASIDE.to_owned() } else { format!("{SET_ASIDE}.{i}") }))
        .find(|to| ["", "-wal", "-shm"].iter().all(|s| !beside(to, s).exists()))
        .ok_or_else(|| anyhow::anyhow!("no name beside {} is free to set it aside under", path.display()))?;
    move_file(path, &to)?;
    Ok(to)
}

/// The store in file `file` of `dir`, as [`open_file`] opens it. One that won't open is set aside
/// ([`set_aside`]) once a new store opens in its place: what it holds is kept, beside it, and the
/// machine goes on keeping its conversations. When not even a new one opens, it stays where it was,
/// for a later start to try. The store (None: none opened, or the store is off), and where the one
/// set aside went. Blocking. Tried once: a store handed over is let go of before it's handed
/// ([`store::Store::close`] answers once its files are closed), so no other process holds it as it
/// opens; it was tried three times, a second apart, for one not quite let go of, and a file that
/// doesn't open as a store opens no better later.
pub fn open_or_set_aside(dir: &std::path::Path, file: &str, upkeep: store::Upkeep) -> (Option<store::Store>, Option<std::path::PathBuf>) {
    if off() {
        tracing::info!("the conversation store is off (KILN_CONVERSATIONS)");
        return (None, None);
    }
    match configured_said() {
        Some(kind) => open_or_set_aside_on(kind, dir, file, upkeep),
        None => (None, None),
    }
}

/// [`open_or_set_aside`] on `kind`. A store another engine made is a store, not a file that won't open: it is left
/// where it is, whole, and said ([`OtherEngine`]), never set aside for an empty one (CP-T1.1, seam review 3).
fn open_or_set_aside_on(kind: Kind, dir: &std::path::Path, file: &str, upkeep: store::Upkeep) -> (Option<store::Store>, Option<std::path::PathBuf>) {
    let path = dir.join(file);
    match opened(kind, &path, upkeep) {
        Ok(st) => return (Some(st), None),
        Err(e) if e.downcast_ref::<OtherEngine>().is_some() => return (None, None),
        Err(_) => {}
    }
    if !path.exists() {
        return (None, None);
    }
    let aside = match set_aside(&path) {
        Ok(aside) => aside,
        Err(e) => {
            tracing::warn!(file = %path.display(), error = %format!("{e:#}"), "a conversation store that wouldn't open couldn't be set aside");
            return (None, None);
        }
    };
    if let Ok(st) = opened(kind, &path, upkeep) {
        tracing::warn!(file = %path.display(), aside = %aside.display(), "a conversation store that wouldn't open is set aside, kept whole; a new one takes its place");
        return (Some(st), Some(aside));
    }
    // Not even a new one: what that began goes, and the one set aside goes back where it was.
    for s in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(beside(&path, s));
    }
    if let Err(e) = move_file(&aside, &path) {
        tracing::warn!(file = %path.display(), aside = %aside.display(), error = %format!("{e:#}"), "a conversation store set aside couldn't be put back where it was; it stays set aside");
        return (None, Some(aside));
    }
    (None, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::core::Update;
    use crate::harness::model::{Body, Event};

    fn said(text: &str) -> Update {
        Update::Events { events: vec![Event { id: "m1".into(), seq: 1, at: 1_000, parent: None, by: None, body: Body::Assistant { text: text.into(), streaming: false } }] }
    }

    /// CP-T1.1 (seam review 3): a store one engine made, opened with the other configured, is refused by name (which
    /// engine made it, which was asked), left where it is, whole, never set aside for an empty one; its own engine
    /// opens it as it was. Each way, Turso to Amphora and back. Without it, Amphora opened Turso's file without its
    /// checksums, and a store refused was set aside and a new, empty one put in its place.
    #[test]
    fn cp_t1_1_a_store_the_other_engine_made_is_refused_by_name_and_left_whole() -> anyhow::Result<()> {
        for (made, asked) in [(Kind::Turso, Kind::Amphora), (Kind::Amphora, Kind::Turso)] {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join(FILE);
            anyhow::ensure!(made_by(&path)?.is_none(), "{made}: an engine named before there's a file");
            let st = store::Store::start(made.open(&path)?, store::Upkeep::default())?;
            st.send("a1", &said("kept"));
            st.sync()?;
            // Open, its header perhaps in its log alone: named all the same.
            anyhow::ensure!(made_by(&path)? == Some(made), "{made}'s open store read as {:?}", made_by(&path)?);
            st.close()?;
            anyhow::ensure!(made_by(&path)? == Some(made), "{made}'s closed store read as {:?}", made_by(&path)?);
            let refused = asked.open(&path).err().ok_or_else(|| anyhow::anyhow!("{asked} opened {made}'s store"))?;
            let other = refused.downcast_ref::<OtherEngine>().ok_or_else(|| anyhow::anyhow!("{asked} refused {made}'s store untyped: {refused:#}"))?;
            anyhow::ensure!((other.made_by, other.asked) == (made, asked), "{other}");
            let told = other.to_string();
            anyhow::ensure!(told.contains(&format!("{ENGINE_VAR}={made}")), "it doesn't say how to go on: {told}");
            let (st, aside) = open_or_set_aside_on(asked, dir.path(), FILE, store::Upkeep::default());
            anyhow::ensure!(st.is_none() && aside.is_none(), "{asked} on {made}'s store: a store {}, set aside at {aside:?}", st.is_some());
            anyhow::ensure!(path.exists() && !beside(&path, SET_ASIDE).exists(), "{made}'s store moved");
            let again = store::Store::start(made.open(&path)?, store::Upkeep::default())?;
            anyhow::ensure!(again.kept("a1", 10)?.map(|k| k.events.len()) == Some(1), "{made}'s store lost what it held");
            again.close()?;
        }
        Ok(())
    }
}

//! A conversation store's Turso file copied into an Amphora file (CP-T1.1, CP-T1.5; kiln goal 0d717247): every
//! table, row by row, each value whole, under the same numbers, so the copy is the same store: its identity
//! (`meta.id`), its numbers (every row's `rev`, the inbox's revisions), and where it stands in each peer's feed
//! (`peers`). A runner's hello gets the number it got before, and its feed goes on from there. The feed itself
//! (`Engine::changes` into `Engine::take`) would number every row again, as the new store's own writes, under an
//! identity of its own, and leave out `peers`, `copied` and `meta`: docs/records-inventory.md, "The conversation
//! store", says why that isn't the copy.
//!
//! It's the copy compaction makes inside one engine (`Engine::compact_some`, "under the same numbers"), across
//! two: with the store let go (its writers stopped, its log empty), into a file beside the target that becomes
//! the target only once it's whole and [`compare`]s the same, `meta` (its identity among it) copied last.

use std::path::{Path, PathBuf};

use super::amphora::Amphora;
use super::store::Engine as _;
use super::turso::Turso;

/// One value as an engine keeps it, its storage class with it, between the two engines' own types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cell {
    Null,
    Integer(i64),
    /// A real's bits (`f64::to_bits`): exact.
    Real(u64),
    Text(String),
    Blob(Vec<u8>),
}

/// The table copied last: the store's identity and its last number are in it, so a copy stopped part way has
/// neither of the source's.
const META: &str = "meta";

/// What [`copy`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Copied {
    /// Each table, and the rows copied into it.
    pub tables: Vec<(String, usize)>,
    /// The store's identity and its last number, read from the copy opened as a store: the source's.
    pub id: String,
    pub top: u64,
}

/// A row that differs between the source and the copy: the table, its place in rowid order, and each side's row
/// (None: that side has no row there).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Difference {
    pub table: String,
    pub at: usize,
    pub from: Option<Vec<Cell>>,
    pub to: Option<Vec<Cell>>,
}

/// `path` with `suffix` after its name.
fn beside(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// The bytes of the file at `path` (0: none).
fn size(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

/// The Turso store's file at `from` copied into a new Amphora file at `into`, every table row by row, then
/// compared: it fails, `into` not made, when the copy differs, or when `from` moved while it was copied. `from`'s
/// store must be let go first (its server or runner stopped: `Store::close`), so its log is empty.
pub fn copy(from: &Path, into: &Path) -> anyhow::Result<Copied> {
    anyhow::ensure!(from.is_file(), "no conversation store at {}", from.display());
    anyhow::ensure!(!into.exists(), "{} is there already: the copy makes it", into.display());
    anyhow::ensure!(size(&beside(from, "-wal")) == 0, "{}'s log isn't empty: let its store go first (stop its server or runner)", from.display());
    let part = beside(into, ".copy.tmp");
    for stale in [part.clone(), beside(&part, "-wal"), beside(&part, "-shm")] {
        if stale.exists() {
            std::fs::remove_file(&stale).map_err(|e| anyhow::anyhow!("{}: {e}", stale.display()))?;
        }
    }
    let mut src = Turso::open(from)?;
    let (id, top, rev) = (src.id(), src.top(), src.copy_meta("rev")?);
    let tables = {
        let mut dst = Amphora::open(&part)?;
        let (ours, theirs) = (src.copy_meta("schema")?, dst.copy_meta("schema")?);
        anyhow::ensure!(ours == theirs, "the two engines' stores are at different migrations: {ours:?} and {theirs:?}");
        let (mut names, theirs) = (src.copy_tables()?, dst.copy_tables()?);
        anyhow::ensure!(names == theirs, "the two stores' tables differ: {names:?} and {theirs:?}");
        // `meta` last: its identity is the source's only once everything else is there.
        names.retain(|t| t != META);
        names.push(META.to_owned());
        let mut tables = Vec::with_capacity(names.len());
        for table in names {
            let (mut after, mut first, mut rows) = (i64::MIN, true, 0);
            loop {
                let (page, last) = src.copy_page(&table, after)?;
                if page.is_empty() && !first {
                    break;
                }
                // Each table emptied as its first page goes in: the new file's own `meta` replaced by the source's.
                dst.copy_put(&table, &page, first)?;
                rows += page.len();
                first = false;
                if page.is_empty() {
                    break;
                }
                after = last;
            }
            tables.push((table, rows));
        }
        dst.copy_flush()?;
        tables
    };
    // Read from the file: another process's write moves it, where this one's number (`top`) doesn't.
    anyhow::ensure!(src.copy_meta("rev")? == rev, "{} was written while it was copied: let its store go first", from.display());
    let found = compare_open(&src, &Amphora::open(&part)?)?;
    anyhow::ensure!(found.is_empty(), "the copy differs from {} in {} rows, the first: {:?}", from.display(), found.len(), found.first());
    // Only the file moves into place: a log beside it that still held anything would be left behind.
    anyhow::ensure!(size(&beside(&part, "-wal")) == 0, "the copy's log isn't empty after it was flushed: {} left as it is", part.display());
    for f in ["-wal", "-shm"] {
        let side = beside(&part, f);
        if side.exists() {
            std::fs::remove_file(&side).map_err(|e| anyhow::anyhow!("{}: {e}", side.display()))?;
        }
    }
    std::fs::rename(&part, into).map_err(|e| anyhow::anyhow!("{} into {}: {e}", part.display(), into.display()))?;
    let mut copied = Amphora::open(into)?;
    let (cid, ctop) = (copied.id(), copied.top());
    anyhow::ensure!(cid == id && ctop == top, "the copy opened as a store is {cid} at {ctop}, not {id} at {top}");
    Ok(Copied { tables, id: cid, top: ctop })
}

/// Every row of every table of the Turso store at `from` against the Amphora store at `into`, in rowid order:
/// each place where they differ.
pub fn compare(from: &Path, into: &Path) -> anyhow::Result<Vec<Difference>> {
    compare_open(&Turso::open(from)?, &Amphora::open(into)?)
}

fn compare_open(src: &Turso, dst: &Amphora) -> anyhow::Result<Vec<Difference>> {
    let (names, theirs) = (src.copy_tables()?, dst.copy_tables()?);
    let mut out = Vec::new();
    for table in names.iter().chain(theirs.iter().filter(|t| !names.contains(t))) {
        let (mut a, mut b, mut at) = (Some(i64::MIN), Some(i64::MIN), 0);
        // Both sides read a step at a time: a side without the table reads as none.
        let has = (names.contains(table), theirs.contains(table));
        while a.is_some() || b.is_some() {
            let (ours, next_a) = match (a, has.0) {
                (Some(after), true) => src.copy_page(table, after)?,
                _ => (Vec::new(), 0),
            };
            let (copy, next_b) = match (b, has.1) {
                (Some(after), true) => dst.copy_page(table, after)?,
                _ => (Vec::new(), 0),
            };
            a = (!ours.is_empty()).then_some(next_a);
            b = (!copy.is_empty()).then_some(next_b);
            let n = ours.len().max(copy.len());
            for i in 0..n {
                let (x, y) = (ours.get(i), copy.get(i));
                if x != y {
                    out.push(Difference { table: table.clone(), at: at + i, from: x.cloned(), to: y.cloned() });
                }
            }
            at += n;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use kiln_seams::records::{Tx, Write};

    use super::*;
    use crate::convo::store::Engine;
    use crate::harness::core::Update;
    use crate::harness::model::{Body, Event};

    fn opened(id: &str) -> Update {
        Update::Open { info: Box::new(crate::harness::session::Info { id: id.into(), kind: "claude".into(), ..Default::default() }), since: None }
    }

    fn said(id: &str, seq: u64, text: &str) -> Update {
        Update::Events { events: vec![Event { id: id.into(), seq, at: 1_000 + i64::try_from(seq).unwrap_or(0), parent: None, by: None, body: Body::Assistant { text: text.into(), streaming: false } }] }
    }

    /// A Turso store with what each table keeps: a session's events and record, what another store's feed
    /// brought (so `peers` says where it stands in it), and a session's inbox (its revisions). Let go, as a
    /// stopped server leaves it.
    fn filled(path: &Path) -> anyhow::Result<()> {
        let dir = path.parent().ok_or_else(|| anyhow::anyhow!("no folder"))?;
        let mut other = Turso::open(&dir.join("other.db"))?;
        other.apply(&crate::convo::rows::of("theirs", &opened("theirs"), 0))?;
        other.apply(&crate::convo::rows::of("theirs", &said("t1", 1, "from the runner"), 0))?;
        let batch = other.changes(0, usize::MAX, true, true)?;
        let mut t = Turso::open(path)?;
        t.apply(&crate::convo::rows::of("s1", &opened("s1"), 0))?;
        for i in 1..=3 {
            t.apply(&crate::convo::rows::of("s1", &said(&format!("e{i}"), i, &format!("said {i}: ünïcode, \"quoted\"")), 0))?;
        }
        t.take("runner/m1", Some("m1"), 0, &batch)?;
        let tx = Tx { conds: Vec::new(), writes: vec![Write::Put { family: "inbox.took".into(), key: b"msg-1".to_vec(), value: b"{\"at\":1}".to_vec() }] };
        let sessions = vec!["s1".to_owned()];
        let done = t.inbox_commit_batch(&[(&tx, sessions.as_slice())])?;
        anyhow::ensure!(done.iter().all(Result::is_ok), "the inbox's commit: {done:?}");
        t.release()
    }

    /// CP-T1.1 / CP-T1.5: the copy is the same store. Its identity and last number are the source's; every table
    /// compares the same row by row; the feed reads the same rows under the same numbers from it (a peer going on
    /// from a number gets what it would have from the source); `peers` says the same of the runner's feed; and the
    /// inbox keeps each row's revision.
    #[test]
    fn cp_t1_5_a_turso_store_copied_into_amphora_is_the_same_store() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (from, into) = (dir.path().join("conversations.db"), dir.path().join("conversations.amphora.db"));
        filled(&from)?;
        let copied = copy(&from, &into)?;
        let mut src = Turso::open(&from)?;
        anyhow::ensure!(copied.id == src.id() && copied.top == src.top() && copied.top > 0, "{copied:?}");
        for t in ["sessions", "events", "records", "peers", "inbox", "meta"] {
            anyhow::ensure!(copied.tables.iter().any(|(name, _)| name == t), "{t} copied: {:?}", copied.tables);
        }
        anyhow::ensure!(copied.tables.last().is_some_and(|(t, _)| t == META), "meta last: {:?}", copied.tables);
        anyhow::ensure!(compare(&from, &into)?.is_empty(), "the same, row by row");
        let mut dst = Amphora::open(&into)?;
        anyhow::ensure!(dst.changes(0, usize::MAX, true, true)? == src.changes(0, usize::MAX, true, true)?, "the feed reads the same rows under the same numbers");
        anyhow::ensure!(dst.changes(2, usize::MAX, true, true)? == src.changes(2, usize::MAX, true, true)?, "and from a number on");
        anyhow::ensure!(dst.peers()? == src.peers()? && dst.peer("runner/m1")? == src.peer("runner/m1")? && src.peer("runner/m1")? > 0, "where it stands in the runner's feed");
        anyhow::ensure!(dst.inbox_get("inbox.took", b"msg-1")? == src.inbox_get("inbox.took", b"msg-1")?, "the inbox row and its revision");
        Ok(())
    }

    /// A row changed in the copy after it was made is found by its table and place; a target that is there
    /// already, or a source still written (its log not empty), is refused before anything is made.
    #[test]
    fn cp_t1_5_a_conversation_copy_that_differs_is_found_and_one_in_the_way_refused() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (from, into) = (dir.path().join("conversations.db"), dir.path().join("conversations.amphora.db"));
        filled(&from)?;
        copy(&from, &into)?;
        let again = match copy(&from, &into) {
            Ok(c) => anyhow::bail!("copied over a store that was there: {c:?}"),
            Err(e) => e.to_string(),
        };
        anyhow::ensure!(again.contains("there already"), "{again}");
        {
            let mut dst = Amphora::open(&into)?;
            dst.apply(&crate::convo::rows::of("s1", &said("e4", 4, "only in the copy"), 0))?;
            dst.release()?;
        }
        let found = compare(&from, &into)?;
        anyhow::ensure!(found.iter().any(|d| d.table == "events" && d.from.is_none() && d.to.is_some()), "the copy's own event found: {found:?}");
        {
            let mut src = Turso::open(&from)?;
            src.apply(&crate::convo::rows::of("s1", &said("e5", 5, "written, not let go"), 0))?;
            let other = dir.path().join("second.amphora.db");
            if size(&beside(&from, "-wal")) > 0 {
                let refused = match copy(&from, &other) {
                    Ok(c) => anyhow::bail!("copied a store being written: {c:?}"),
                    Err(e) => e.to_string(),
                };
                anyhow::ensure!(refused.contains("log isn't empty") && !other.exists(), "{refused}");
            }
            src.release()?;
        }
        Ok(())
    }
}

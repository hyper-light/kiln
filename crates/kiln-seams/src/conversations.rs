//! Conversations: the store kiln keeps its agents' conversations in (docs/conversations.md; CP-T1.1). Entries are
//! appended durably to conversations, read back by a conversation's history, and handed on by a change feed of
//! revisions, which kiln's peers and followers read. Kiln's store runs on Turso or on Amphora's engine
//! (`crates/kiln/src/convo`), each behind this seam; [`SimConversations`] keeps them in memory.
//!
//! [`load`] is the scenario every implementation runs beside the suite: many conversations written at once, a burst
//! of readers following the change feed, a restart mid-stream, with the latencies, the feed's lag and the memory held.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use futures::future::BoxFuture;
use parking_lot::{Condvar, Mutex, RwLock};

use crate::conformance::{Case, Failure};

/// One entry of a conversation, known by its id within it; appended again, it's kept as it was appended last.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Entry {
    pub conversation: String,
    pub id: String,
    pub seq: u64,
    pub text: String,
}

/// An entry as the change feed hands it on: the revision that committed it as it is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changed {
    pub rev: u64,
    pub entry: Entry,
}

/// A page of the change feed: changes after a revision, oldest first, and the revision read through.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Page {
    pub changes: Vec<Changed>,
    pub upto: u64,
}

/// Why a store refused or failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Let go of ([`Conversations::close`]): nothing more is asked of this handle.
    #[error("the conversation store is closed")]
    Closed,
    /// Past a bound of the implementation's, said.
    #[error("refused: {0}")]
    Refused(String),
    /// The implementation failed, in its words.
    #[error("{0}")]
    Failed(String),
}

/// The conversation store, as kiln uses it.
pub trait Conversations: Send + Sync + 'static {
    /// Commits `entries` durably, all or nothing: once it answers Ok they're in every read and stay through a restart;
    /// when it answers an error none of them is, save one Refused as not knowing in time whether they were written,
    /// after which all of them may yet be. No read, the feed and the top included, ever sees part of a batch.
    fn append(&self, entries: &[Entry]) -> Result<(), Error>;
    /// The newest `limit` entries of `conversation` with a sequence below `before` (any, when None), oldest first.
    fn history(&self, conversation: &str, before: Option<u64>, limit: usize) -> Result<Vec<Entry>, Error>;
    /// What was committed after revision `from`, oldest first, about `max_bytes` of it (one change at least, when
    /// there is one), and the revision it read through.
    fn changes(&self, from: u64, max_bytes: usize) -> Result<Page, Error>;
    /// The newest revision committed.
    fn top(&self) -> Result<u64, Error>;
    /// Waits for a revision past `after` to be committed, woken by that commit (never by polling or a timer): the
    /// newest revision then, at once when one is already. A wait under way is refused Closed when its handle is let go.
    fn wait(&self, after: u64) -> Result<u64, Error>;
    /// Lets go of its files, once all it took is committed; everything asked of the handle after is refused Closed.
    fn close(&self) -> Result<(), Error>;
}

/// What a case needs: the store, opened (again) where the fixture keeps it, with what was committed before there.
pub trait ConversationsFixture: Send + Sync {
    fn open(&self) -> Result<Arc<dyn Conversations>, Failure>;
    /// While `failing`, the store's writes fail as a disk that refuses them would (CONVO-7: a batch that failed
    /// leaves nothing of itself).
    fn fail_writes(&self, failing: bool);
}

/// The entries the simulator holds at most (past them an append is refused): a bound of the simulator's, not of the
/// contract's.
pub const SIM_ENTRIES: usize = 1_000_000;

#[derive(Default)]
struct SimDisk {
    rev: u64,
    /// Each entry by its conversation and id, with the revision that committed it as it is.
    entries: HashMap<(String, String), (u64, Entry)>,
    /// The entry each live revision committed.
    log: BTreeMap<u64, (String, String)>,
}

/// The conversation store in memory: what it commits stays with its fixture, as on a disk, across handles.
pub struct SimConversations {
    disk: Arc<Mutex<SimDisk>>,
    /// Told of each commit, and of a handle let go of ([`Conversations::wait`]), with the disk's lock held.
    commits: Arc<Condvar>,
    /// The disk refusing writes ([`ConversationsFixture::fail_writes`]).
    failing: Arc<AtomicBool>,
    closed: AtomicBool,
}

impl SimConversations {
    fn open_on(disk: &Arc<Mutex<SimDisk>>, commits: &Arc<Condvar>, failing: &Arc<AtomicBool>) -> Self {
        Self { disk: Arc::clone(disk), commits: Arc::clone(commits), failing: Arc::clone(failing), closed: AtomicBool::new(false) }
    }

    fn live(&self) -> Result<(), Error> {
        if self.closed.load(Ordering::Acquire) { Err(Error::Closed) } else { Ok(()) }
    }
}

fn bytes_of(e: &Entry) -> usize {
    e.conversation.len().saturating_add(e.id.len()).saturating_add(e.text.len()).saturating_add(std::mem::size_of::<u64>())
}

impl Conversations for SimConversations {
    fn append(&self, entries: &[Entry]) -> Result<(), Error> {
        self.live()?;
        let mut d = self.disk.lock();
        // Refused or failed before any entry goes in: all or nothing.
        if self.failing.load(Ordering::Acquire) {
            return Err(Error::Failed("the simulated disk refused the write".to_owned()));
        }
        let new = entries.iter().filter(|e| !d.entries.contains_key(&(e.conversation.clone(), e.id.clone()))).count();
        if d.entries.len().saturating_add(new) > SIM_ENTRIES {
            return Err(Error::Refused(format!("the simulator holds {SIM_ENTRIES} entries")));
        }
        for e in entries {
            let key = (e.conversation.clone(), e.id.clone());
            if d.entries.get(&key).is_some_and(|(_, was)| was == e) {
                continue;
            }
            d.rev = d.rev.saturating_add(1);
            let rev = d.rev;
            if let Some((old, _)) = d.entries.insert(key.clone(), (rev, e.clone())) {
                d.log.remove(&old);
            }
            d.log.insert(rev, key);
        }
        drop(d);
        self.commits.notify_all();
        Ok(())
    }

    fn history(&self, conversation: &str, before: Option<u64>, limit: usize) -> Result<Vec<Entry>, Error> {
        self.live()?;
        let d = self.disk.lock();
        let mut all: Vec<Entry> = d.entries.values().map(|(_, e)| e).filter(|e| e.conversation == conversation && before.is_none_or(|b| e.seq < b)).cloned().collect();
        all.sort_by_key(|e| e.seq);
        let skip = all.len().saturating_sub(limit);
        Ok(all.into_iter().skip(skip).collect())
    }

    fn changes(&self, from: u64, max_bytes: usize) -> Result<Page, Error> {
        self.live()?;
        let d = self.disk.lock();
        let mut page = Page { changes: Vec::new(), upto: from.max(d.rev) };
        let mut bytes = 0usize;
        for (rev, key) in d.log.range(from.saturating_add(1)..) {
            let Some((_, e)) = d.entries.get(key) else { continue };
            if !page.changes.is_empty() && bytes.saturating_add(bytes_of(e)) > max_bytes {
                page.upto = rev.saturating_sub(1);
                break;
            }
            bytes = bytes.saturating_add(bytes_of(e));
            page.changes.push(Changed { rev: *rev, entry: e.clone() });
        }
        Ok(page)
    }

    fn top(&self) -> Result<u64, Error> {
        self.live()?;
        Ok(self.disk.lock().rev)
    }

    fn wait(&self, after: u64) -> Result<u64, Error> {
        let mut d = self.disk.lock();
        loop {
            self.live()?;
            if d.rev > after {
                return Ok(d.rev);
            }
            self.commits.wait(&mut d);
        }
    }

    fn close(&self) -> Result<(), Error> {
        // Under the disk's lock, so a wait between its look and its sleep hears it.
        let d = self.disk.lock();
        self.closed.store(true, Ordering::Release);
        drop(d);
        self.commits.notify_all();
        Ok(())
    }
}

/// The simulator's fixture: one disk, opened as often as a case asks.
#[derive(Default)]
pub struct SimConversationsFixture {
    disk: Arc<Mutex<SimDisk>>,
    commits: Arc<Condvar>,
    failing: Arc<AtomicBool>,
}

impl ConversationsFixture for SimConversationsFixture {
    fn open(&self) -> Result<Arc<dyn Conversations>, Failure> {
        Ok(Arc::new(SimConversations::open_on(&self.disk, &self.commits, &self.failing)))
    }

    fn fail_writes(&self, failing: bool) {
        self.failing.store(failing, Ordering::Release);
    }
}

fn check(ok: bool, why: impl FnOnce() -> String) -> Result<(), Failure> {
    if ok { Ok(()) } else { Err(why()) }
}

fn err(e: Error) -> Failure {
    e.to_string()
}

fn entry(conversation: &str, id: &str, seq: u64, text: &str) -> Entry {
    Entry { conversation: conversation.to_owned(), id: id.to_owned(), seq, text: text.to_owned() }
}

/// Follows the feed from `from` to `to`, `max_bytes` a page, adding what it hands on to `seen`: where it got to.
pub fn follow(c: &dyn Conversations, from: u64, to: u64, max_bytes: usize, seen: &mut Vec<Changed>) -> Result<u64, Failure> {
    let mut at = from;
    while at < to {
        let page = c.changes(at, max_bytes).map_err(err)?;
        check(page.upto > at || !page.changes.is_empty(), || format!("the feed stood still at {at} below {to}"))?;
        at = page.upto;
        seen.extend(page.changes);
    }
    Ok(at)
}

/// What the feed handed on, held to what was appended (each entry once, never again changed; `appended` holds each
/// writer's entries in the order it appended them, each committed before its next): every entry exactly once, its
/// revisions rising, and each writer's entries in the order it appended them.
pub fn exactly_once(seen: &[Changed], appended: &[Vec<Entry>]) -> Result<(), Failure> {
    let all: HashSet<&Entry> = appended.iter().flatten().collect();
    let mut last = 0u64;
    let mut at: HashMap<&Entry, usize> = HashMap::with_capacity(seen.len());
    for (i, c) in seen.iter().enumerate() {
        check(c.rev > last, || format!("revision {} came after {last}", c.rev))?;
        last = c.rev;
        check(all.contains(&c.entry), || format!("{:?} was never appended", c.entry))?;
        check(at.insert(&c.entry, i).is_none(), || format!("{:?} came twice", c.entry.id))?;
    }
    check(at.len() == all.len(), || format!("{} of {} entries came", at.len(), all.len()))?;
    for mine in appended {
        let mut before: Option<(usize, &Entry)> = None;
        for e in mine {
            let Some(&i) = at.get(e) else { return Err(format!("{:?} never came", e.id)) };
            if let Some((j, prev)) = before {
                check(i > j, || format!("{:?} came before {:?}, appended after it", e.id, prev.id))?;
            }
            before = Some((i, e));
        }
    }
    Ok(())
}

/// Writers appending `per` entries each, one an append, to conversations of their own, while one reader follows the
/// feed from `from`, then catches up to the top: what each writer appended, in order, and where the reader got to.
fn write_and_follow(c: &Arc<dyn Conversations>, round: usize, writers: usize, per: u64, from: u64, seen: &mut Vec<Changed>) -> Result<(Vec<Vec<Entry>>, u64), Failure> {
    // The reader follows until it has every entry of the round, waiting on each commit when it's at the top.
    let want = usize::try_from(per).unwrap_or(usize::MAX).saturating_mul(writers);
    let (appended, at) = std::thread::scope(|s| -> Result<(Vec<Vec<Entry>>, u64), Failure> {
        let handles: Vec<_> = (0..writers)
            .map(|w| {
                let c = Arc::clone(c);
                s.spawn(move || -> Result<Vec<Entry>, Failure> {
                    let conv = format!("c{w}");
                    let mut mine = Vec::new();
                    for i in 0..per {
                        let seq = u64::try_from(round).unwrap_or(u64::MAX).saturating_mul(per).saturating_add(i);
                        let e = entry(&conv, &format!("r{round}w{w}m{i}"), seq, "words");
                        c.append(std::slice::from_ref(&e)).map_err(err)?;
                        mine.push(e);
                    }
                    Ok(mine)
                })
            })
            .collect();
        let reader = s.spawn(|| -> Result<(Vec<Changed>, u64), Failure> {
            let mut got = Vec::new();
            let mut at = from;
            while got.len() < want {
                let page = c.changes(at, 4096).map_err(err)?;
                if page.changes.is_empty() && page.upto <= at {
                    c.wait(at).map_err(err)?;
                    continue;
                }
                at = page.upto;
                got.extend(page.changes);
            }
            Ok((got, at))
        });
        let mut appended = Vec::with_capacity(writers);
        let mut failed = None;
        for h in handles {
            match h.join() {
                Ok(Ok(mine)) => appended.push(mine),
                Ok(Err(e)) => failed = Some(e),
                Err(_) => failed = Some("a writer stopped".to_owned()),
            }
        }
        // A writer that failed leaves the reader short: its handle let go of, its wait is refused.
        if failed.is_some() {
            let _ = c.close();
        }
        let read = reader.join().map_err(|_| "the reader stopped".to_owned())?;
        if let Some(e) = failed {
            return Err(e);
        }
        let (got, at) = read?;
        seen.extend(got);
        Ok((appended, at))
    })?;
    let top = c.top().map_err(err)?;
    let at = follow(c.as_ref(), at, top, 4096, seen)?;
    Ok((appended, at))
}

fn c1<F: ConversationsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let c = f.open()?;
        let a: Vec<Entry> = (1..=10).map(|i| entry("a", &format!("m{i}"), i, &format!("reply {i}"))).collect();
        c.append(&a).map_err(err)?;
        c.append(&[entry("b", "m1", 1, "other"), entry("b", "m2", 2, "other")]).map_err(err)?;
        let seqs = |h: Vec<Entry>| h.iter().map(|e| e.seq).collect::<Vec<_>>();
        let newest = seqs(c.history("a", None, 4).map_err(err)?);
        check(newest == vec![7, 8, 9, 10], || format!("the newest 4: {newest:?}"))?;
        let older = seqs(c.history("a", Some(7), 3).map_err(err)?);
        check(older == vec![4, 5, 6], || format!("3 before 7: {older:?}"))?;
        let b = c.history("b", None, 10).map_err(err)?;
        check(b.len() == 2 && b.iter().all(|e| e.conversation == "b" && e.text == "other"), || format!("b's: {b:?}"))
    })
}

fn c2<F: ConversationsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let c = f.open()?;
        let first = entry("a", "m1", 1, "first");
        c.append(std::slice::from_ref(&first)).map_err(err)?;
        let top = c.top().map_err(err)?;
        c.append(std::slice::from_ref(&first)).map_err(err)?;
        let again = c.top().map_err(err)?;
        check(again == top, || format!("the same entry again moved the top from {top} to {again}"))?;
        let revised = entry("a", "m1", 1, "revised");
        c.append(std::slice::from_ref(&revised)).map_err(err)?;
        let h = c.history("a", None, 10).map_err(err)?;
        check(h == vec![revised.clone()], || format!("its history: {h:?}"))?;
        let mut seen = Vec::new();
        follow(c.as_ref(), top, c.top().map_err(err)?, 1 << 20, &mut seen)?;
        let got: Vec<&Entry> = seen.iter().map(|ch| &ch.entry).collect();
        check(got == vec![&revised], || format!("the feed after the first: {got:?}"))?;
        let mut all = Vec::new();
        follow(c.as_ref(), 0, c.top().map_err(err)?, 1 << 20, &mut all)?;
        let got: Vec<&Entry> = all.iter().map(|ch| &ch.entry).collect();
        check(got == vec![&revised], || format!("the feed from the start: {got:?}"))
    })
}

fn c3<F: ConversationsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let c = f.open()?;
        let mut seen = Vec::new();
        let (appended, _) = write_and_follow(&c, 0, 4, 50, 0, &mut seen)?;
        exactly_once(&seen, &appended)
    })
}

fn c4<F: ConversationsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let mut seen = Vec::new();
        let mut appended: Vec<Vec<Entry>> = Vec::new();
        let mut at = 0u64;
        for round in 0..3 {
            let c = f.open()?;
            let top = c.top().map_err(err)?;
            check(top >= at, || format!("opened again at revision {top}, below the {at} read before"))?;
            let (more, now) = write_and_follow(&c, round, 3, 20, at, &mut seen)?;
            // Writer w of each round appends to conversation w after the round before: one writer's order across them.
            for (w, mine) in more.into_iter().enumerate() {
                match appended.get_mut(w) {
                    Some(was) => was.extend(mine),
                    None => appended.push(mine),
                }
            }
            at = now;
            c.close().map_err(err)?;
            check(matches!(c.top(), Err(Error::Closed)), || "a closed handle answered".to_owned())?;
        }
        let c = f.open()?;
        let top = c.top().map_err(err)?;
        check(top == at, || format!("opened again at revision {top}, the feed read through {at}"))?;
        exactly_once(&seen, &appended)
    })
}

fn c5<F: ConversationsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let c = f.open()?;
        let text = "x".repeat(100);
        let all: Vec<Entry> = (1..=20).map(|i| entry("a", &format!("m{i}"), i, &text)).collect();
        c.append(&all).map_err(err)?;
        let one = c.changes(0, 1).map_err(err)?;
        check(one.changes.len() == 1, || format!("a page of 1 byte held {} changes", one.changes.len()))?;
        let mut seen = Vec::new();
        let top = c.top().map_err(err)?;
        let at = follow(c.as_ref(), 0, top, 1, &mut seen)?;
        check(at == top && seen.len() == all.len(), || format!("paging a change at a time reached {at} of {top} with {} of {}", seen.len(), all.len()))?;
        let rest = c.changes(top, 1 << 20).map_err(err)?;
        check(rest.changes.is_empty() && rest.upto == top, || format!("past the top: {rest:?}"))
    })
}

fn c6<F: ConversationsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let c = f.open()?;
        c.append(&[entry("a", "m1", 1, "first")]).map_err(err)?;
        let top = c.top().map_err(err)?;
        let before = c.wait(top.saturating_sub(1)).map_err(err)?;
        check(before == top, || format!("a wait past {} answered {before}, the top {top}", top.saturating_sub(1)))?;
        // Past the top: answered by the next commit, whenever the wait began.
        let (woke, closed) = std::thread::scope(|s| {
            let waiter = s.spawn(|| c.wait(top));
            let appended = c.append(&[entry("a", "m2", 2, "second")]);
            let woke = waiter.join().map_err(|_| "the waiter stopped".to_owned()).and_then(|w| w.map_err(err));
            // Past the top again, nothing more appended: the wait ends only as its handle is let go of, refused Closed.
            let now = c.top().unwrap_or(u64::MAX);
            let held = &c;
            let waiter = s.spawn(move || held.wait(now));
            let closed = c.close();
            let refused = waiter.join().map_err(|_| "the waiter stopped".to_owned());
            (appended.map_err(err).and(woke), closed.map_err(err).and(refused))
        });
        let woke = woke?;
        check(woke > top, || format!("a wait past {top} answered {woke}"))?;
        let refused = closed?;
        check(matches!(refused, Err(Error::Closed)), || format!("a wait as its handle was let go of answered {refused:?}"))
    })
}

fn c7<F: ConversationsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let c = f.open()?;
        let kept = entry("a", "m1", 1, "kept");
        c.append(std::slice::from_ref(&kept)).map_err(err)?;
        let top = c.top().map_err(err)?;
        // A batch across two conversations, a new entry, another conversation's first and one entry revised.
        let batch = vec![entry("a", "m2", 2, "second"), entry("b", "m1", 1, "first of b"), entry("a", "m1", 1, "revised")];
        f.fail_writes(true);
        let answer = c.append(&batch);
        f.fail_writes(false);
        check(matches!(answer, Err(Error::Failed(_) | Error::Refused(_))), || format!("a batch whose writes failed answered {answer:?}"))?;
        let now = c.top().map_err(err)?;
        check(now == top, || format!("a failed batch moved the top from {top} to {now}"))?;
        let a = c.history("a", None, 10).map_err(err)?;
        check(a == vec![kept.clone()], || format!("after a failed batch, a's history: {a:?}"))?;
        let b = c.history("b", None, 10).map_err(err)?;
        check(b.is_empty(), || format!("after a failed batch, b's history: {b:?}"))?;
        let page = c.changes(top, 1 << 20).map_err(err)?;
        check(page.changes.is_empty(), || format!("after a failed batch, the feed handed on {:?}", page.changes))?;
        // Writing again, the same batch goes in whole.
        c.append(&batch).map_err(err)?;
        let a = c.history("a", None, 10).map_err(err)?;
        check(a == vec![entry("a", "m1", 1, "revised"), entry("a", "m2", 2, "second")], || format!("the batch made again, a's history: {a:?}"))?;
        let b = c.history("b", None, 10).map_err(err)?;
        check(b == vec![entry("b", "m1", 1, "first of b")], || format!("the batch made again, b's history: {b:?}"))
    })
}

/// The suite: each case a clause of docs/seams.md's Conversations section.
pub fn cases<F: ConversationsFixture>() -> Vec<Case<F>> {
    vec![
        Case { id: "CONVO-1", says: "appended entries are read back by their conversation's history, the newest asked for before a sequence, oldest first", check: c1::<F> },
        Case { id: "CONVO-2", says: "an entry appended again by its id is kept as appended last, once; the same again commits nothing, and the feed hands it on once more only when it changed", check: c2::<F> },
        Case { id: "CONVO-3", says: "the feed from any revision hands on every entry committed after it exactly once, oldest first, while writers append at once", check: c3::<F> },
        Case { id: "CONVO-4", says: "what was committed and its revisions stay through a restart: a reader goes on from where it was, nothing missed or seen twice; a closed handle is refused Closed", check: c4::<F> },
        Case { id: "CONVO-5", says: "a page of one byte holds one change, paging by where each read through reaches the top, and past the top there is nothing", check: c5::<F> },
        Case { id: "CONVO-6", says: "a wait past a revision answers at once when one is committed past it, else with the commit that passes it; one under way is refused Closed as its handle is let go of", check: c6::<F> },
        Case { id: "CONVO-7", says: "an append is all or nothing: a batch whose writes failed answers an error and leaves none of its entries in any read, the feed and the top included; made again, it goes in whole", check: c7::<F> },
    ]
}

/// The load scenario's shape: what it runs, at any scale.
#[derive(Debug, Clone)]
pub struct Load {
    /// Conversations written at once.
    pub conversations: usize,
    /// Writer threads, each appending to the conversations in turn.
    pub writers: usize,
    /// Appends each writer makes, an entry each.
    pub appends: usize,
    /// Readers following the change feed from the start, each reading the history of what it was handed.
    pub readers: usize,
    /// The bytes of each entry's text.
    pub text_bytes: usize,
    /// The bytes a page of the feed asks for.
    pub page_bytes: usize,
    /// The newest entries a reader asks of a conversation's history, for the last change of each page it reads.
    pub history_entries: usize,
    /// Let go of and opened again once half the appends are acknowledged, the writers still appending and the readers
    /// mid-feed (seam review 14: it was quiesced, nothing in flight).
    pub restart: bool,
}

/// The concurrency ladder (Ada, 2026-11-21 07:24Z): the writers and readers at each rung of a run of record.
pub const LADDER: [usize; 7] = [1, 8, 32, 128, 512, 2048, 4096];

/// Percentiles by nearest rank (p50, p99, p99.9) of what was measured, and how many.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Percentiles {
    pub n: usize,
    pub p50: u64,
    pub p99: u64,
    pub p999: u64,
}

impl Percentiles {
    pub fn of(mut v: Vec<u64>) -> Self {
        v.sort_unstable();
        let n = v.len();
        // Nearest rank: the smallest value with at least `per_mille`/1000 of them at or below it.
        let at = |per_mille: usize| n.saturating_mul(per_mille).div_ceil(1000).saturating_sub(1);
        let get = |per_mille: usize| v.get(at(per_mille)).copied().unwrap_or(0);
        Self { n, p50: get(500), p99: get(990), p999: get(999) }
    }
}

/// What the load scenario measured.
#[derive(Debug, Clone, Default)]
pub struct LoadReport {
    /// Writer and reader threads, the rung's concurrency.
    pub threads: usize,
    /// Each append's time to commit, microseconds (one retried through the restart counts its wait for it).
    pub append_us: Percentiles,
    /// Each history read's time, microseconds.
    pub history_us: Percentiles,
    /// Each feed read's time, microseconds.
    pub feed_us: Percentiles,
    /// How many revisions behind the top each feed read answered, the top read just after.
    pub feed_lag_revs: Percentiles,
    /// The time from the writers' last commit to every reader at the top, microseconds.
    pub catch_up_us: u64,
    /// From the first append to every reader at the top, microseconds, and the appends acknowledged a second in it.
    pub wall_us: u64,
    pub appends_per_s: u64,
    pub appended: usize,
    /// Appends answered Closed by the restart under way and made again on the store opened again.
    pub retried: usize,
    pub restarts: usize,
    /// The process's resident memory at the end and at its highest (Linux's VmRSS and VmHWM), bytes; before the
    /// store opened; and the harness's own (its samples and its checks, counted), which isn't the store's.
    pub rss_bytes: Option<u64>,
    pub rss_peak_bytes: Option<u64>,
    pub rss_before_bytes: Option<u64>,
    pub harness_bytes: u64,
    /// What the store holds: the process's growth past where it stood before the store opened, less the harness's own
    /// (seam review 12). Linux's alone (None elsewhere: no /proc).
    pub store_bytes: Option<u64>,
}

/// The process's resident memory and its peak, from /proc/self/status (kB there, as Linux's proc(5) states).
fn memory() -> (Option<u64>, Option<u64>) {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else { return (None, None) };
    let field = |name: &str| {
        status.lines().find_map(|l| l.strip_prefix(name)).and_then(|rest| rest.trim().trim_end_matches("kB").trim().parse::<u64>().ok()).map(|kb| kb.saturating_mul(1024))
    };
    (field("VmRSS:"), field("VmHWM:"))
}

fn micros(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_micros()).unwrap_or(u64::MAX)
}

/// The model check as a reader goes (seam review 12: each reader kept every change it saw, so the checker, not the
/// store, set the memory reported): writer `w`'s append `i` is entry `w{w}a{i}` at sequence `i * writers + w`, so a
/// counter a writer holds it to exactly once, in its writer's order, its revisions rising.
struct Check {
    writers: usize,
    appends: usize,
    /// The next append each writer's feed must hand on.
    next: Vec<usize>,
    last_rev: u64,
    seen: usize,
}

impl Check {
    fn new(writers: usize, appends: usize) -> Self {
        Self { writers, appends, next: vec![0; writers], last_rev: 0, seen: 0 }
    }

    fn saw(&mut self, c: &Changed) -> Result<(), Failure> {
        check(c.rev > self.last_rev, || format!("revision {} came after {}", c.rev, self.last_rev))?;
        self.last_rev = c.rev;
        let never = || format!("{:?} was never appended", c.entry.id);
        let n = usize::try_from(c.entry.seq).map_err(|_| never())?;
        let (w, i) = (n.checked_rem(self.writers).ok_or_else(never)?, n.checked_div(self.writers).ok_or_else(never)?);
        check(i < self.appends && c.entry.id == format!("w{w}a{i}"), never)?;
        let want = self.next.get_mut(w).ok_or_else(never)?;
        check(i == *want, || if i < *want { format!("{:?} came twice", c.entry.id) } else { format!("{:?} came before w{w}a{}, appended after it", c.entry.id, *want) })?;
        *want = want.saturating_add(1);
        self.seen = self.seen.saturating_add(1);
        Ok(())
    }

    fn whole(&self) -> bool {
        self.seen >= self.writers.saturating_mul(self.appends)
    }

    fn bytes(&self) -> u64 {
        u64::try_from(self.next.capacity().saturating_mul(std::mem::size_of::<usize>())).unwrap_or(u64::MAX)
    }
}

/// The handle every thread of the scenario uses, opened again by the restart while they go on.
struct Shared<'a, F> {
    f: &'a F,
    current: RwLock<Arc<dyn Conversations>>,
    /// Set when a thread failed: the others stop.
    stop: AtomicBool,
    retried: AtomicUsize,
}

impl<F: ConversationsFixture> Shared<'_, F> {
    /// `op` on the handle now; answered Closed by the restart under way, on the store opened again (the write lock
    /// held through the restart makes the next look wait for it).
    fn on<T>(&self, mut op: impl FnMut(&dyn Conversations) -> Result<T, Error>) -> Result<T, Failure> {
        loop {
            if self.stop.load(Ordering::Acquire) {
                return Err("stopped: another thread of the scenario failed".to_owned());
            }
            let c = Arc::clone(&self.current.read());
            match op(c.as_ref()) {
                Err(Error::Closed) => {
                    self.retried.fetch_add(1, Ordering::Relaxed);
                }
                r => return r.map_err(err),
            }
        }
    }

    /// Let go of the store and open it again, the others' work in flight.
    fn restart(&self) -> Result<(), Failure> {
        let mut c = self.current.write();
        c.close().map_err(err)?;
        *c = self.f.open()?;
        Ok(())
    }

    /// A thread failed: the others stop, a reader's wait under way refused as its handle is let go of.
    fn fail(&self, e: Failure) -> Failure {
        self.stop.store(true, Ordering::Release);
        let _ = self.current.read().close();
        e
    }
}

struct Writer {
    append_us: Vec<u64>,
    last_commit: Option<Instant>,
}

struct Reader {
    feed_us: Vec<u64>,
    history_us: Vec<u64>,
    lag: Vec<u64>,
    check_bytes: u64,
    done: Instant,
}

fn joined<T>(h: std::thread::ScopedJoinHandle<'_, Result<T, Failure>>, who: &str) -> Result<T, Failure> {
    h.join().map_err(|_| format!("a {who} stopped"))?
}

/// The load scenario on what `f` opens: writers append to many conversations at once while readers follow the feed
/// from the start, each waiting on the store's commits when it's at the top (never polling), and (`restart`) the store
/// let go of and opened again half way, with appends and reads in flight. Every reader's feed is held to what was
/// appended as it goes (each entry exactly once, in its writer's order) before anything measured is reported.
pub fn load<F: ConversationsFixture>(f: &F, l: &Load) -> Result<LoadReport, Failure> {
    let (rss_before, _) = memory();
    let total = l.writers.saturating_mul(l.appends);
    let half = if l.restart && total >= 2 { total / 2 } else { 0 };
    let sh = Shared { f, current: RwLock::new(f.open()?), stop: AtomicBool::new(false), retried: AtomicUsize::new(0) };
    let acked = AtomicUsize::new(0);
    let restarts = AtomicUsize::new(0);
    let text = "w".repeat(l.text_bytes);
    let conversations = l.conversations.max(1);
    let began = Instant::now();
    let (writers, readers) = std::thread::scope(|s| -> Result<(Vec<Writer>, Vec<Reader>), Failure> {
        let writers: Vec<_> = (0..l.writers)
            .map(|w| {
                let (sh, text, acked, restarts) = (&sh, text.as_str(), &acked, &restarts);
                s.spawn(move || -> Result<Writer, Failure> {
                    let mut me = Writer { append_us: Vec::with_capacity(l.appends), last_commit: None };
                    for i in 0..l.appends {
                        let n = i.saturating_mul(l.writers).saturating_add(w);
                        let e = entry(&format!("c{}", n % conversations), &format!("w{w}a{i}"), u64::try_from(n).unwrap_or(u64::MAX), text);
                        let t = Instant::now();
                        // One answered Closed by the restart goes again, whole, on the store opened again: the same
                        // entry, which commits nothing more if it was in.
                        sh.on(|c| c.append(std::slice::from_ref(&e))).map_err(|e| sh.fail(e))?;
                        me.append_us.push(micros(t));
                        me.last_commit = Some(Instant::now());
                        // The append that makes half the scenario's restarts it, the others' appends and reads under way.
                        if half > 0 && acked.fetch_add(1, Ordering::AcqRel).saturating_add(1) == half {
                            sh.restart().map_err(|e| sh.fail(e))?;
                            restarts.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Ok(me)
                })
            })
            .collect();
        let readers: Vec<_> = (0..l.readers)
            .map(|_| {
                let sh = &sh;
                s.spawn(move || -> Result<Reader, Failure> {
                    let mut me = Reader { feed_us: Vec::new(), history_us: Vec::new(), lag: Vec::new(), check_bytes: 0, done: Instant::now() };
                    let mut model = Check::new(l.writers, l.appends);
                    let mut at = 0u64;
                    let read = |me: &mut Reader, model: &mut Check, at: &mut u64| -> Result<(), Failure> {
                        let t = Instant::now();
                        let page = sh.on(|c| c.changes(*at, l.page_bytes))?;
                        me.feed_us.push(micros(t));
                        let top = sh.on(|c| c.top())?;
                        me.lag.push(top.saturating_sub(page.upto));
                        if let Some(last) = page.changes.last() {
                            let t = Instant::now();
                            sh.on(|c| c.history(&last.entry.conversation, None, l.history_entries))?;
                            me.history_us.push(micros(t));
                        }
                        for ch in &page.changes {
                            model.saw(ch)?;
                        }
                        // At the top with more to come: woken by the commit that passes it.
                        if page.changes.is_empty() && page.upto <= *at && !model.whole() {
                            sh.on(|c| c.wait(*at))?;
                        }
                        *at = page.upto.max(*at);
                        Ok(())
                    };
                    while !model.whole() {
                        read(&mut me, &mut model, &mut at).map_err(|e| sh.fail(e))?;
                    }
                    me.check_bytes = model.bytes();
                    me.done = Instant::now();
                    Ok(me)
                })
            })
            .collect();
        // The first failure is the one said; any stops the rest.
        let mut failed = None;
        let mut ws = Vec::with_capacity(l.writers);
        for h in writers {
            match joined(h, "writer") {
                Ok(w) => ws.push(w),
                Err(e) => failed = failed.or(Some(sh.fail(e))),
            }
        }
        let mut rs = Vec::with_capacity(l.readers);
        for h in readers {
            match joined(h, "reader") {
                Ok(r) => rs.push(r),
                Err(e) => failed = failed.or(Some(sh.fail(e))),
            }
        }
        match failed {
            Some(e) => Err(e),
            None => Ok((ws, rs)),
        }
    })?;
    let ended = readers.iter().map(|r| r.done).max().unwrap_or_else(Instant::now);
    // Memory as the run ends, before the samples are gathered for their percentiles.
    let (rss, peak) = memory();
    let samples: usize = writers.iter().map(|w| w.append_us.capacity()).chain(readers.iter().map(|r| r.feed_us.capacity().saturating_add(r.history_us.capacity()).saturating_add(r.lag.capacity()))).sum();
    let harness_bytes = u64::try_from(samples.saturating_mul(std::mem::size_of::<u64>())).unwrap_or(u64::MAX).saturating_add(readers.iter().map(|r| r.check_bytes).sum());
    let last = writers.iter().filter_map(|w| w.last_commit).max();
    let wall_us = u64::try_from(ended.saturating_duration_since(began).as_micros()).unwrap_or(u64::MAX);
    let appended: usize = writers.iter().map(|w| w.append_us.len()).sum();
    let report = LoadReport {
        threads: l.writers.saturating_add(l.readers),
        append_us: Percentiles::of(writers.iter().flat_map(|w| w.append_us.iter().copied()).collect()),
        history_us: Percentiles::of(readers.iter().flat_map(|r| r.history_us.iter().copied()).collect()),
        feed_us: Percentiles::of(readers.iter().flat_map(|r| r.feed_us.iter().copied()).collect()),
        feed_lag_revs: Percentiles::of(readers.iter().flat_map(|r| r.lag.iter().copied()).collect()),
        catch_up_us: last.map_or(0, |t| u64::try_from(ended.saturating_duration_since(t).as_micros()).unwrap_or(u64::MAX)),
        wall_us,
        appends_per_s: u64::try_from(appended).unwrap_or(u64::MAX).saturating_mul(1_000_000).checked_div(wall_us).unwrap_or(0),
        appended,
        retried: sh.retried.load(Ordering::Relaxed),
        restarts: restarts.load(Ordering::Relaxed),
        rss_bytes: rss,
        rss_peak_bytes: peak,
        rss_before_bytes: rss_before,
        harness_bytes,
        store_bytes: rss.zip(rss_before).map(|(r, b)| r.saturating_sub(b).saturating_sub(harness_bytes)),
    };
    sh.current.read().close().map_err(err)?;
    Ok(report)
}

/// The load scenario up a ladder of concurrency: at each rung `n` of `rungs`, `n` writers and `n` readers (and as many
/// conversations, at least `base`'s), on a store `fixture` makes afresh: a report a rung.
pub fn ladder<F: ConversationsFixture>(base: &Load, rungs: &[usize], mut fixture: impl FnMut() -> Result<F, Failure>) -> Result<Vec<(usize, LoadReport)>, Failure> {
    rungs
        .iter()
        .map(|&n| {
            let l = Load { writers: n, readers: n, conversations: base.conversations.max(n), ..base.clone() };
            let f = fixture()?;
            load(&f, &l).map(|r| (n, r)).map_err(|e| format!("at {n} writers and readers: {e}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_simulator_passes_every_clause() -> Result<(), String> {
        let report = futures::executor::block_on(crate::conformance::run("Conversations", &cases::<SimConversationsFixture>(), || async { SimConversationsFixture::default() }));
        report.into_result().map(|_| ())
    }

    #[test]
    fn cp_t1_1_the_load_scenario_runs_on_the_simulator_each_entry_once() -> Result<(), String> {
        let l = Load { conversations: 8, writers: 4, appends: 50, readers: 3, text_bytes: 64, page_bytes: 4096, history_entries: 50, restart: true };
        let r = load(&SimConversationsFixture::default(), &l)?;
        check(r.appended == 200 && r.restarts == 1 && r.append_us.n == 200 && r.threads == 7, || format!("{r:?}"))
    }

    /// CP-T1.1 (seam review 11): the ladder runs a rung each, its writers and readers at the rung's count.
    #[test]
    fn cp_t1_1_the_load_scenario_climbs_the_ladder_on_the_simulator() -> Result<(), String> {
        let l = Load { conversations: 4, writers: 0, appends: 6, readers: 0, text_bytes: 16, page_bytes: 4096, history_entries: 8, restart: true };
        let rungs = LADDER.get(..3).ok_or("no ladder")?;
        let rs = ladder(&l, rungs, || Ok(SimConversationsFixture::default()))?;
        let got: Vec<(usize, usize, usize)> = rs.iter().map(|(n, r)| (*n, r.threads, r.appended)).collect();
        check(got == vec![(1, 2, 6), (8, 16, 48), (32, 64, 192)], || format!("{got:?}"))
    }

    /// CP-T1.1 (seam review 12): the check as it goes refuses a change lost, repeated or out of its writer's order.
    #[test]
    fn cp_t1_1_the_running_check_refuses_a_repeated_or_reordered_change() {
        let ch = |w: usize, i: usize, rev: u64| Changed { rev, entry: entry("c", &format!("w{w}a{i}"), u64::try_from(i * 2 + w).unwrap_or(0), "t") };
        let mut ok = Check::new(2, 2);
        for c in [ch(0, 0, 1), ch(1, 0, 2), ch(0, 1, 3), ch(1, 1, 4)] {
            assert!(ok.saw(&c).is_ok(), "{c:?}");
        }
        assert!(ok.whole());
        let mut twice = Check::new(2, 2);
        assert!(twice.saw(&ch(0, 0, 1)).is_ok());
        assert!(twice.saw(&ch(0, 0, 2)).is_err(), "twice");
        let mut skipped = Check::new(2, 2);
        assert!(skipped.saw(&ch(0, 1, 1)).is_err(), "reordered");
        let mut falling = Check::new(2, 2);
        assert!(falling.saw(&ch(0, 0, 2)).is_ok());
        assert!(falling.saw(&ch(1, 0, 1)).is_err(), "revisions falling");
        assert!(!falling.whole(), "lost");
    }

    /// CP-T1.1: the model check refuses a feed that loses, repeats or reorders what one writer appended.
    #[test]
    fn cp_t1_1_the_model_check_refuses_a_lost_repeated_or_reordered_change() {
        let (a, b, x) = (entry("c", "m1", 1, "t"), entry("c", "m2", 2, "t"), entry("d", "m1", 1, "t"));
        let feed = |es: &[(&Entry, u64)]| es.iter().map(|(e, rev)| Changed { rev: *rev, entry: (*e).clone() }).collect::<Vec<_>>();
        let appended = vec![vec![a.clone(), b.clone()], vec![x.clone()]];
        assert!(exactly_once(&feed(&[(&a, 1), (&x, 2), (&b, 3)]), &appended).is_ok());
        assert!(exactly_once(&feed(&[(&a, 1), (&x, 2)]), &appended).is_err(), "lost");
        assert!(exactly_once(&feed(&[(&a, 1), (&x, 2), (&b, 3), (&b, 4)]), &appended).is_err(), "twice");
        assert!(exactly_once(&feed(&[(&b, 1), (&x, 2), (&a, 3)]), &appended).is_err(), "reordered");
        assert!(exactly_once(&feed(&[(&a, 2), (&x, 1), (&b, 3)]), &appended).is_err(), "revisions falling");
    }

    #[test]
    fn percentiles_by_nearest_rank() {
        let p = Percentiles::of((1..=1000).rev().collect());
        assert_eq!((p.n, p.p50, p.p99, p.p999), (1000, 500, 990, 999));
        assert_eq!(Percentiles::of(Vec::new()), Percentiles::default());
    }
}

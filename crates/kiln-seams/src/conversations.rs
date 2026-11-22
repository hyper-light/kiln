//! Conversations: the store kiln keeps its agents' conversations in (docs/conversations.md; CP-T1.1). Entries are
//! appended durably to conversations, read back by a conversation's history, and handed on by a change feed of
//! revisions, which kiln's peers and followers read. Kiln's store runs on Turso or on Amphora's engine
//! (`crates/kiln/src/convo`), each behind this seam; [`SimConversations`] keeps them in memory.
//!
//! [`load`] is the scenario every implementation runs beside the suite: many conversations written at once, a burst
//! of readers following the change feed, a restart mid-stream, with the latencies, the feed's lag and the memory held.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use futures::future::BoxFuture;
use parking_lot::Mutex;

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
    /// Commits `entries` durably: once it answers Ok they're in every read and stay through a restart.
    fn append(&self, entries: &[Entry]) -> Result<(), Error>;
    /// The newest `limit` entries of `conversation` with a sequence below `before` (any, when None), oldest first.
    fn history(&self, conversation: &str, before: Option<u64>, limit: usize) -> Result<Vec<Entry>, Error>;
    /// What was committed after revision `from`, oldest first, about `max_bytes` of it (one change at least, when
    /// there is one), and the revision it read through.
    fn changes(&self, from: u64, max_bytes: usize) -> Result<Page, Error>;
    /// The newest revision committed.
    fn top(&self) -> Result<u64, Error>;
    /// Lets go of its files, once all it took is committed; everything asked of the handle after is refused Closed.
    fn close(&self) -> Result<(), Error>;
}

/// What a case needs: the store, opened (again) where the fixture keeps it, with what was committed before there.
pub trait ConversationsFixture: Send + Sync {
    fn open(&self) -> Result<Arc<dyn Conversations>, Failure>;
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
    closed: AtomicBool,
}

impl SimConversations {
    fn open_on(disk: &Arc<Mutex<SimDisk>>) -> Self {
        Self { disk: Arc::clone(disk), closed: AtomicBool::new(false) }
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

    fn close(&self) -> Result<(), Error> {
        self.closed.store(true, Ordering::Release);
        Ok(())
    }
}

/// The simulator's fixture: one disk, opened as often as a case asks.
#[derive(Default)]
pub struct SimConversationsFixture(Arc<Mutex<SimDisk>>);

impl ConversationsFixture for SimConversationsFixture {
    fn open(&self) -> Result<Arc<dyn Conversations>, Failure> {
        Ok(Arc::new(SimConversations::open_on(&self.0)))
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
    let done = AtomicBool::new(false);
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
            while !done.load(Ordering::Acquire) {
                let page = c.changes(at, 4096).map_err(err)?;
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
        done.store(true, Ordering::Release);
        let (got, at) = reader.join().map_err(|_| "the reader stopped".to_owned())??;
        if let Some(e) = failed {
            return Err(e);
        }
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

/// The suite: each case a clause of docs/seams.md's Conversations section.
pub fn cases<F: ConversationsFixture>() -> Vec<Case<F>> {
    vec![
        Case { id: "CONVO-1", says: "appended entries are read back by their conversation's history, the newest asked for before a sequence, oldest first", check: c1::<F> },
        Case { id: "CONVO-2", says: "an entry appended again by its id is kept as appended last, once; the same again commits nothing, and the feed hands it on once more only when it changed", check: c2::<F> },
        Case { id: "CONVO-3", says: "the feed from any revision hands on every entry committed after it exactly once, oldest first, while writers append at once", check: c3::<F> },
        Case { id: "CONVO-4", says: "what was committed and its revisions stay through a restart: a reader goes on from where it was, nothing missed or seen twice; a closed handle is refused Closed", check: c4::<F> },
        Case { id: "CONVO-5", says: "a page of one byte holds one change, paging by where each read through reaches the top, and past the top there is nothing", check: c5::<F> },
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
    /// Let go of and opened again once the writers are half way, readers mid-feed.
    pub restart: bool,
}

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
    /// Each append's time to commit, microseconds.
    pub append_us: Percentiles,
    /// Each history read's time, microseconds.
    pub history_us: Percentiles,
    /// Each feed read's time, microseconds.
    pub feed_us: Percentiles,
    /// How many revisions behind the top each feed read answered, the top read just after.
    pub feed_lag_revs: Percentiles,
    /// The time from the writers' last commit to every reader at the top, microseconds.
    pub catch_up_us: u64,
    pub appended: usize,
    pub restarts: usize,
    /// The process's resident memory at the end and at its highest (Linux's VmRSS and VmHWM), bytes.
    pub rss_bytes: Option<u64>,
    pub rss_peak_bytes: Option<u64>,
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

struct Phase {
    /// Each writer's appends, in the order it made them.
    appended: Vec<Vec<Entry>>,
    append_us: Vec<u64>,
    history_us: Vec<u64>,
    feed_us: Vec<u64>,
    lag: Vec<u64>,
    last_commit: Option<Instant>,
}

/// One stretch of the scenario on `c`: writers append `from..to` of their appends while readers follow the feed
/// from where each was (`at`), until the writers end, then (`catch_up`) on to the top.
fn phase(c: &Arc<dyn Conversations>, l: &Load, from: usize, to: usize, at: &mut [u64], seen: &mut [Vec<Changed>], catch_up: bool) -> Result<Phase, Failure> {
    let text = "w".repeat(l.text_bytes);
    let writing = AtomicBool::new(true);
    let conversations = l.conversations.max(1);
    std::thread::scope(|s| -> Result<Phase, Failure> {
        let writers: Vec<_> = (0..l.writers)
            .map(|w| {
                let (c, text) = (Arc::clone(c), text.as_str());
                s.spawn(move || -> Result<(Vec<Entry>, Vec<u64>, Option<Instant>), Failure> {
                    let (mut mine, mut us, mut last) = (Vec::new(), Vec::new(), None);
                    for i in from..to {
                        let n = i.saturating_mul(l.writers).saturating_add(w);
                        let conv = format!("c{}", n % conversations);
                        let e = entry(&conv, &format!("w{w}a{i}"), u64::try_from(n).unwrap_or(u64::MAX), text);
                        let t = Instant::now();
                        c.append(std::slice::from_ref(&e)).map_err(err)?;
                        us.push(micros(t));
                        last = Some(Instant::now());
                        mine.push(e);
                    }
                    Ok((mine, us, last))
                })
            })
            .collect();
        let readers: Vec<_> = at
            .iter_mut()
            .zip(seen.iter_mut())
            .map(|(at, seen)| {
                let (c, writing) = (Arc::clone(c), &writing);
                s.spawn(move || -> Result<(Vec<u64>, Vec<u64>, Vec<u64>), Failure> {
                    let (mut feed_us, mut history_us, mut lag) = (Vec::new(), Vec::new(), Vec::new());
                    let mut read = |at: &mut u64| -> Result<bool, Failure> {
                        let t = Instant::now();
                        let page = c.changes(*at, l.page_bytes).map_err(err)?;
                        feed_us.push(micros(t));
                        let top = c.top().map_err(err)?;
                        lag.push(top.saturating_sub(page.upto));
                        *at = page.upto;
                        if let Some(last) = page.changes.last() {
                            let t = Instant::now();
                            c.history(&last.entry.conversation, None, l.history_entries).map_err(err)?;
                            history_us.push(micros(t));
                        }
                        let moved = !page.changes.is_empty();
                        seen.extend(page.changes);
                        Ok(moved || *at < top)
                    };
                    while writing.load(Ordering::Acquire) {
                        read(at)?;
                    }
                    if catch_up {
                        while read(at)? {}
                    }
                    Ok((feed_us, history_us, lag))
                })
            })
            .collect();
        let mut p = Phase { appended: Vec::new(), append_us: Vec::new(), history_us: Vec::new(), feed_us: Vec::new(), lag: Vec::new(), last_commit: None };
        let mut failed = None;
        for h in writers {
            match h.join() {
                Ok(Ok((mine, us, last))) => {
                    p.appended.push(mine);
                    p.append_us.extend(us);
                    p.last_commit = p.last_commit.max(last);
                }
                Ok(Err(e)) => failed = Some(e),
                Err(_) => failed = Some("a writer stopped".to_owned()),
            }
        }
        writing.store(false, Ordering::Release);
        for h in readers {
            match h.join() {
                Ok(Ok((feed_us, history_us, lag))) => {
                    p.feed_us.extend(feed_us);
                    p.history_us.extend(history_us);
                    p.lag.extend(lag);
                }
                Ok(Err(e)) => failed = Some(e),
                Err(_) => failed = Some("a reader stopped".to_owned()),
            }
        }
        match failed {
            Some(e) => Err(e),
            None => Ok(p),
        }
    })
}

/// The load scenario on what `f` opens: every reader's feed held to what was appended (each entry exactly once,
/// in order) before anything measured is reported.
pub fn load<F: ConversationsFixture>(f: &F, l: &Load) -> Result<LoadReport, Failure> {
    let mut at = vec![0u64; l.readers];
    let mut seen: Vec<Vec<Changed>> = vec![Vec::new(); l.readers];
    let half = if l.restart { l.appends / 2 } else { l.appends };
    let mut c = f.open()?;
    let mut phases = vec![phase(&c, l, 0, half, &mut at, &mut seen, !l.restart)?];
    let mut restarts = 0;
    if l.restart {
        c.close().map_err(err)?;
        c = f.open()?;
        restarts = 1;
        phases.push(phase(&c, l, half, l.appends, &mut at, &mut seen, true)?);
    }
    let caught_up = Instant::now();
    // Writer w's appends of each phase, the phases in turn: one writer's order through the restart.
    let mut appended: Vec<Vec<Entry>> = vec![Vec::new(); l.writers];
    for p in &phases {
        for (mine, all) in p.appended.iter().zip(appended.iter_mut()) {
            all.extend(mine.iter().cloned());
        }
    }
    for s in &seen {
        exactly_once(s, &appended)?;
    }
    let last = phases.iter().filter_map(|p| p.last_commit).max();
    let (rss, peak) = memory();
    let all = |pick: fn(&Phase) -> &Vec<u64>| Percentiles::of(phases.iter().flat_map(|p| pick(p).iter().copied()).collect());
    let report = LoadReport {
        append_us: all(|p| &p.append_us),
        history_us: all(|p| &p.history_us),
        feed_us: all(|p| &p.feed_us),
        feed_lag_revs: all(|p| &p.lag),
        catch_up_us: last.map_or(0, |t| u64::try_from(caught_up.saturating_duration_since(t).as_micros()).unwrap_or(u64::MAX)),
        appended: appended.iter().map(Vec::len).sum(),
        restarts,
        rss_bytes: rss,
        rss_peak_bytes: peak,
    };
    c.close().map_err(err)?;
    Ok(report)
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
        check(r.appended == 200 && r.restarts == 1 && r.append_us.n == 200, || format!("{r:?}"))
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

//! Elle-style isolation checks of the versioned storage's commit validations (G8 step 1,
//! AC-T1.3.6, T-G8.1.1): Kingsbury and Alvaro, "Elle: Inferring Isolation Anomalies from
//! Experimental Observations" (PVLDB 14(3), 2020), on list-append and read-write-register
//! histories, the anomalies named as Adya's ("Weak Consistency", MIT PhD thesis, 1999).
//!
//! The checker is the list-append one of Amphora's write-slot checks
//! (`crates/amphora/src/database/replicated.rs`, `list_violations`: ww edges from each list's
//! order, wr from the last element read, rw to the element after a read), which lives in a crate
//! above this one; here it is carried over and taken to Elle's whole vocabulary: G0, G1a, G1b,
//! G1c, G-single and G2 each found and named, for registers as well as lists, with intermediate
//! and aborted reads and each transaction's own consistency.
//!
//! The workload is Elle's own generator, `elle.txn/wr-txns` at its defaults (jepsen-io/elle,
//! `src/elle/txn.clj`): 10 keys live at once, key `i` drawn with probability 2^i/1023 (its
//! exponential distribution, base 2), transactions of 1 to 4 operations, each a read or a write
//! with even odds, and a key retired for a fresh one once it has taken the writes it was given,
//! 1 plus a draw of a Zipf distribution over 256 (probability of `k` in proportion to `1/k`, as
//! its docstring says). Reads of odd seeds are spans (`read_range` from the key, two rows), so
//! phantoms are in reach too. 32 transactions are open at once: the step's 32 writers.
//!
//! Two drivers run the histories: one thread interleaving the 32 transactions' operations in an
//! order a seed decides (each seed is replayed exactly), and 32 threads at once (the order the
//! hardware makes). Each seed runs 256 transactions, the most writes Elle's generator gives one
//! key, so a seed's history can retire keys. `AMPHORA_ELLE_SEEDS` (100 unless set),
//! `AMPHORA_ELLE_FIRST` (1) and `AMPHORA_ELLE_VALIDATION` (`precision`, `ssi`, `occ`; all three
//! unless set) choose the seeds and validations, so the coordinator's 10,000-seed runs are the
//! same tests.

use super::blink::Descender;
use super::version::{Txn as VTxn, Validation, Versioned};
use crate::error::LimboError;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// One operation of a transaction as it ran: what it wrote, or what it read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mop {
    Append { key: u64, element: u64 },
    List { key: u64, seen: Vec<u64> },
    Write { key: u64, value: u64 },
    Reg { key: u64, seen: Option<u64> },
}

/// A transaction as it ran: its operations, whether it committed, and its commit timestamp.
#[derive(Clone, Debug)]
pub struct Record {
    pub mops: Vec<Mop>,
    pub committed: bool,
    pub ts: u64,
}

/// What a history shows. Adya's G0 (a cycle of writes), G1a (an aborted write read), G1b (an
/// intermediate write read), G1c (a cycle of writes and reads), G-single (a cycle with one
/// anti-dependency) and G2 (with more); then Elle's own: a transaction that disagrees with
/// itself, reads no order of versions explains, a committed write lost, a value nobody wrote.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Anomaly {
    G0,
    G1a,
    G1b,
    G1c,
    GSingle,
    G2,
    Internal,
    Order,
    Lost,
    Garbage,
}

/// The dependency graph over committed transactions, by edge kind.
struct Graph {
    ww: Vec<BTreeSet<usize>>,
    wr: Vec<BTreeSet<usize>>,
    rw: Vec<BTreeSet<usize>>,
}

impl Graph {
    fn new(n: usize) -> Self {
        Self {
            ww: vec![BTreeSet::new(); n],
            wr: vec![BTreeSet::new(); n],
            rw: vec![BTreeSet::new(); n],
        }
    }

    fn successors<'a>(
        kinds: &'a [&'a Vec<BTreeSet<usize>>],
        n: usize,
    ) -> impl Iterator<Item = usize> + 'a {
        kinds.iter().flat_map(move |k| k[n].iter().copied())
    }

    /// Whether the edges of `kinds` hold a cycle (Kahn's algorithm: what can't be sorted).
    fn cyclic(&self, kinds: &[&Vec<BTreeSet<usize>>]) -> bool {
        let n = self.ww.len();
        let mut incoming = vec![0usize; n];
        for from in 0..n {
            for to in Self::successors(kinds, from) {
                incoming[to] += 1;
            }
        }
        let mut ready: Vec<usize> = (0..n).filter(|v| incoming[*v] == 0).collect();
        let mut sorted = 0;
        while let Some(v) = ready.pop() {
            sorted += 1;
            for to in Self::successors(kinds, v) {
                incoming[to] -= 1;
                if incoming[to] == 0 {
                    ready.push(to);
                }
            }
        }
        sorted != n
    }

    /// Whether `to` is reached from `from` along the edges of `kinds` (breadth first).
    fn reaches(&self, kinds: &[&Vec<BTreeSet<usize>>], from: usize, to: usize) -> bool {
        let mut seen = vec![false; self.ww.len()];
        let mut queue = VecDeque::from([from]);
        seen[from] = true;
        while let Some(v) = queue.pop_front() {
            if v == to {
                return true;
            }
            for next in Self::successors(kinds, v) {
                if !seen[next] {
                    seen[next] = true;
                    queue.push_back(next);
                }
            }
        }
        false
    }

    /// Each cycle class the graph holds: G0 (ww alone), G1c (a wr edge closed by ww and wr),
    /// G-single (an rw edge closed by ww and wr), G2 (an rw edge closed only through another).
    fn classify(&self, out: &mut BTreeSet<Anomaly>) {
        let all = [&self.ww, &self.wr, &self.rw];
        if !self.cyclic(&all) {
            return;
        }
        let deps = [&self.ww, &self.wr];
        if self.cyclic(&[&self.ww]) {
            out.insert(Anomaly::G0);
        }
        for (u, targets) in self.wr.iter().enumerate() {
            if targets.iter().any(|v| self.reaches(&deps, *v, u)) {
                out.insert(Anomaly::G1c);
                break;
            }
        }
        for (u, targets) in self.rw.iter().enumerate() {
            for v in targets {
                if self.reaches(&deps, *v, u) {
                    out.insert(Anomaly::GSingle);
                } else if self.reaches(&all, *v, u) {
                    out.insert(Anomaly::G2);
                }
            }
        }
    }
}

/// Committed transactions' node numbers, by their index in the history.
fn nodes(history: &[Record]) -> BTreeMap<usize, usize> {
    history
        .iter()
        .enumerate()
        .filter(|(_, r)| r.committed)
        .enumerate()
        .map(|(n, (i, _))| (i, n))
        .collect()
}

/// The anomalies of a list-append history whose lists finally read `finals` (the order of each
/// key's versions: Elle's, from its longest read).
pub fn check_lists(history: &[Record], finals: &BTreeMap<u64, Vec<u64>>) -> BTreeSet<Anomaly> {
    let mut out = BTreeSet::new();
    let node = nodes(history);
    let mut graph = Graph::new(node.len());
    // Each element's appender and key, and each transaction's last element appended to a key.
    let mut appended: BTreeMap<u64, (usize, u64)> = BTreeMap::new();
    let mut last: BTreeMap<(usize, u64), u64> = BTreeMap::new();
    for (i, r) in history.iter().enumerate() {
        for m in &r.mops {
            if let Mop::Append { key, element } = m {
                appended.insert(*element, (i, *key));
                last.insert((i, *key), *element);
            }
        }
    }
    let empty = Vec::new();
    let list = |k: u64| finals.get(&k).unwrap_or(&empty);
    let writer = |e: u64| appended.get(&e).and_then(|(i, _)| node.get(i).copied());
    for (k, l) in finals {
        let mut seen = BTreeSet::new();
        for e in l {
            match appended.get(e) {
                Some((_, key)) if key != k => {
                    out.insert(Anomaly::Garbage);
                }
                None => {
                    out.insert(Anomaly::Garbage);
                }
                Some((i, _)) if !history[*i].committed => {
                    out.insert(Anomaly::G1a);
                }
                Some(_) => {}
            }
            if !seen.insert(*e) {
                out.insert(Anomaly::Garbage);
            }
        }
        for pair in l.windows(2) {
            if let (Some(a), Some(b)) = (writer(pair[0]), writer(pair[1])) {
                if a != b {
                    graph.ww[a].insert(b);
                }
            }
        }
    }
    for (i, r) in history.iter().enumerate() {
        let Some(&me) = node.get(&i) else {
            continue;
        };
        // Each key's list as this transaction last saw it, and what it appended since.
        let mut mine: BTreeMap<u64, (Option<Vec<u64>>, Vec<u64>)> = BTreeMap::new();
        for m in &r.mops {
            match m {
                Mop::Append { key, element } => {
                    if !list(*key).contains(element) {
                        out.insert(Anomaly::Lost);
                    }
                    mine.entry(*key).or_default().1.push(*element);
                }
                Mop::List { key, seen } => {
                    let (known, since) = mine.entry(*key).or_default();
                    let agrees = match known {
                        Some(before) => before.iter().chain(since.iter()).eq(seen.iter()),
                        None => seen.ends_with(since),
                    };
                    if !agrees {
                        out.insert(Anomaly::Internal);
                    }
                    *known = Some(seen.clone());
                    since.clear();
                    for e in seen {
                        match appended.get(e) {
                            None => {
                                out.insert(Anomaly::Garbage);
                            }
                            Some((w, _)) if !history[*w].committed => {
                                out.insert(Anomaly::G1a);
                            }
                            Some(_) => {}
                        }
                    }
                    if let Some(&e) = seen.last() {
                        if let Some((w, _)) = appended.get(&e) {
                            if *w != i && last.get(&(*w, *key)) != Some(&e) {
                                out.insert(Anomaly::G1b);
                            }
                        }
                        if let Some(from) = writer(e) {
                            if from != me {
                                graph.wr[from].insert(me);
                            }
                        }
                    }
                    let l = list(*key);
                    if !l.starts_with(seen) {
                        out.insert(Anomaly::Order);
                        continue;
                    }
                    if let Some(to) = l.get(seen.len()).and_then(|e| writer(*e)) {
                        if to != me {
                            graph.rw[me].insert(to);
                        }
                    }
                }
                Mop::Write { .. } | Mop::Reg { .. } => {}
            }
        }
    }
    graph.classify(&mut out);
    out
}

/// The anomalies of a read-write-register history whose registers finally read `finals`. Each
/// key's versions are ordered by their writers' commit timestamps, and the final read checks
/// that order: the last version is what a snapshot after every commit reads.
pub fn check_registers(
    history: &[Record],
    finals: &BTreeMap<u64, Option<u64>>,
) -> BTreeSet<Anomaly> {
    let mut out = BTreeSet::new();
    let node = nodes(history);
    let mut graph = Graph::new(node.len());
    let mut written: BTreeMap<u64, (usize, u64)> = BTreeMap::new();
    let mut last: BTreeMap<(usize, u64), u64> = BTreeMap::new();
    for (i, r) in history.iter().enumerate() {
        for m in &r.mops {
            if let Mop::Write { key, value } = m {
                written.insert(*value, (i, *key));
                last.insert((i, *key), *value);
            }
        }
    }
    // Each key's versions in commit order, and each version's place (0: the initial absence).
    let mut versions: BTreeMap<u64, Vec<(u64, u64)>> = BTreeMap::new();
    for ((i, key), value) in &last {
        if history[*i].committed {
            versions
                .entry(*key)
                .or_default()
                .push((history[*i].ts, *value));
        }
    }
    let mut place: BTreeMap<(u64, u64), usize> = BTreeMap::new();
    for (key, vs) in versions.iter_mut() {
        vs.sort_unstable();
        for (p, (_, value)) in vs.iter().enumerate() {
            place.insert((*key, *value), p + 1);
        }
    }
    let keys: BTreeSet<u64> = versions.keys().chain(finals.keys()).copied().collect();
    for k in keys {
        let newest = versions.get(&k).and_then(|vs| vs.last()).map(|(_, v)| *v);
        if finals.get(&k).copied().flatten() != newest {
            out.insert(Anomaly::Lost);
        }
    }
    let writer = |v: u64| written.get(&v).and_then(|(i, _)| node.get(i).copied());
    for vs in versions.values() {
        for pair in vs.windows(2) {
            if let (Some(a), Some(b)) = (writer(pair[0].1), writer(pair[1].1)) {
                if a != b {
                    graph.ww[a].insert(b);
                }
            }
        }
    }
    for (i, r) in history.iter().enumerate() {
        let Some(&me) = node.get(&i) else {
            continue;
        };
        // Each key's value as this transaction last read or wrote it.
        let mut mine: BTreeMap<u64, Option<u64>> = BTreeMap::new();
        for m in &r.mops {
            match m {
                Mop::Write { key, value } => {
                    mine.insert(*key, Some(*value));
                }
                Mop::Reg { key, seen } => {
                    if let Some(expected) = mine.get(key) {
                        if expected != seen {
                            out.insert(Anomaly::Internal);
                        }
                    }
                    mine.insert(*key, *seen);
                    let p = match seen {
                        None => 0,
                        Some(v) => match written.get(v) {
                            None => {
                                out.insert(Anomaly::Garbage);
                                continue;
                            }
                            Some((_, k)) if k != key => {
                                out.insert(Anomaly::Garbage);
                                continue;
                            }
                            Some((w, _)) if *w == i => continue,
                            Some((w, _)) if !history[*w].committed => {
                                out.insert(Anomaly::G1a);
                                continue;
                            }
                            Some((w, _)) => {
                                if last.get(&(*w, *key)) != Some(v) {
                                    out.insert(Anomaly::G1b);
                                    continue;
                                }
                                if let Some(from) = writer(*v) {
                                    graph.wr[from].insert(me);
                                }
                                place.get(&(*key, *v)).copied().unwrap_or(0)
                            }
                        },
                    };
                    let next = versions.get(key).and_then(|vs| vs.get(p));
                    if let Some(to) = next.and_then(|(_, v)| writer(*v)) {
                        if to != me {
                            graph.rw[me].insert(to);
                        }
                    }
                }
                Mop::Append { .. } | Mop::List { .. } => {}
            }
        }
    }
    graph.classify(&mut out);
    out
}

// Elle's `wr-txns` defaults (the module's comment).
const KEYS: usize = 10;
const KEY_BASE_SCALE: u64 = 2046; // (2^10 - 1) * 2 / (2 - 1): `key-dist-scale` at base 2.
const MIN_LEN: u64 = 1;
const MAX_LEN: u64 = 4;
const MAX_WRITES: usize = 256;
/// The step's 32 writers: transactions open at once.
const WRITERS: usize = 32;
/// Transactions a seed runs: Elle's most writes to one key.
const TXNS: usize = MAX_WRITES;

/// An operation the generator plans.
#[derive(Clone, Copy, Debug)]
pub enum Plan {
    Read(u64),
    Write(u64),
}

/// A key live in the generator: how many writes it may take, and has taken.
struct Slot {
    key: u64,
    budget: u64,
    written: u64,
}

/// Elle's `wr-txns` generator.
pub struct Gen {
    rng: u64,
    slots: Vec<Slot>,
    next_key: u64,
    /// The Zipf distribution's cumulative weights over 1..=256.
    zipf: Vec<f64>,
}

fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

impl Gen {
    pub fn new(seed: u64) -> Self {
        let mut total = 0.0;
        let zipf = (1..=MAX_WRITES)
            .map(|k| {
                total += 1.0 / k as f64;
                total
            })
            .collect();
        let mut g = Self {
            rng: seed,
            slots: Vec::new(),
            next_key: KEYS as u64,
            zipf,
        };
        for key in 0..KEYS as u64 {
            let budget = g.budget();
            g.slots.push(Slot {
                key,
                budget,
                written: 0,
            });
        }
        g
    }

    fn next(&mut self) -> u64 {
        splitmix(&mut self.rng)
    }

    /// 1 plus a draw over 1..=256 with weights `1/k`, as `(inc (rand/zipf max-writes-per-key))`.
    fn budget(&mut self) -> u64 {
        let total = *self.zipf.last().unwrap();
        let x = (self.next() >> 11) as f64 / (1u64 << 53) as f64 * total;
        let k = self.zipf.partition_point(|c| *c <= x);
        1 + (k as u64 + 1).min(MAX_WRITES as u64)
    }

    /// A slot by Elle's exponential distribution: `floor(log2(r + 2)) - 1`, `r` in [0, 2046).
    fn slot(&mut self) -> usize {
        let r = self.next() % KEY_BASE_SCALE;
        (63 - (r + 2).leading_zeros() - 1) as usize
    }

    pub fn txn(&mut self) -> Vec<Plan> {
        let len = MIN_LEN + self.next() % (MAX_LEN - MIN_LEN + 1);
        (0..len)
            .map(|_| {
                let i = self.slot();
                if self.next().is_multiple_of(2) {
                    return Plan::Read(self.slots[i].key);
                }
                if self.slots[i].written >= self.slots[i].budget {
                    let budget = self.budget();
                    self.slots[i] = Slot {
                        key: self.next_key,
                        budget,
                        written: 0,
                    };
                    self.next_key += 1;
                }
                self.slots[i].written += 1;
                Plan::Write(self.slots[i].key)
            })
            .collect()
    }
}

/// Which history: lists appended to or registers written; reads of a key or of a span.
#[derive(Clone, Copy, Debug)]
pub struct Workload {
    pub lists: bool,
    pub spans: bool,
}

fn key_bytes(key: u64) -> [u8; 8] {
    key.to_be_bytes()
}

fn key_of(bytes: &[u8]) -> u64 {
    u64::from_be_bytes(bytes.try_into().unwrap())
}

/// A list as a row: each element LEB128 (Elle's longest lists fit a cell: 256 elements under
/// 2^14, two bytes each).
fn encode(list: &[u64]) -> Vec<u8> {
    let mut out = Vec::new();
    for e in list {
        let mut v = *e;
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | 0x80);
        }
    }
    out
}

fn decode(bytes: &[u8]) -> Vec<u64> {
    let mut out = Vec::new();
    let (mut v, mut shift) = (0u64, 0);
    for b in bytes {
        v |= u64::from(b & 0x7f) << shift;
        shift += 7;
        if b & 0x80 == 0 {
            out.push(v);
            v = 0;
            shift = 0;
        }
    }
    out
}

fn observed(w: Workload, key: u64, row: Option<&[u8]>) -> Mop {
    if w.lists {
        Mop::List {
            key,
            seen: row.map(decode).unwrap_or_default(),
        }
    } else {
        Mop::Reg {
            key,
            seen: row.map(key_of),
        }
    }
}

/// Runs one planned operation in `txn`, what it read or wrote added to `mops`.
fn step(
    v: &Versioned,
    d: &mut Descender,
    w: Workload,
    txn: &mut VTxn,
    plan: Plan,
    ids: &AtomicU64,
    mops: &mut Vec<Mop>,
) -> crate::Result<()> {
    let t = v.tree(0)?;
    match plan {
        Plan::Write(key) => {
            let id = ids.fetch_add(1, Ordering::Relaxed);
            if w.lists {
                // The write's own read, which the first writer's win makes current.
                let mut list = t
                    .get(&v.pool, d, &v.clock, &txn.snapshot(), &key_bytes(key))?
                    .map(|r| decode(&r))
                    .unwrap_or_default();
                list.push(id);
                t.put(&v.pool, d, &v.clock, txn, &key_bytes(key), &encode(&list))?;
                mops.push(Mop::Append { key, element: id });
            } else {
                t.put(
                    &v.pool,
                    d,
                    &v.clock,
                    txn,
                    &key_bytes(key),
                    &id.to_be_bytes(),
                )?;
                mops.push(Mop::Write { key, value: id });
            }
        }
        Plan::Read(key) if !w.spans => {
            let row = t.read(&v.pool, d, &v.clock, txn, &key_bytes(key))?;
            mops.push(observed(w, key, row.as_deref()));
        }
        Plan::Read(key) => {
            let rows = t.read_range(&v.pool, d, &v.clock, txn, Some(&key_bytes(key)), 2)?;
            // The span read: to the last row given when it gave two, else to the end. Keys of
            // it no row holds read as absent.
            let to = if rows.len() == 2 {
                rows.last().map(|(k, _)| key_of(k))
            } else {
                None
            };
            let got: BTreeMap<u64, &[u8]> = rows
                .iter()
                .map(|(k, r)| (key_of(k), r.as_slice()))
                .collect();
            let mut keys: BTreeSet<u64> = got.keys().copied().collect();
            for j in [key, key + 1] {
                if to.is_none_or(|to| j <= to) {
                    keys.insert(j);
                }
            }
            for j in keys {
                mops.push(observed(w, j, got.get(&j).copied()));
            }
        }
    }
    Ok(())
}

/// A run's history, the final state of its keys, and its counts.
pub struct Run {
    pub history: Vec<Record>,
    pub finals: Vec<(u64, Vec<u8>)>,
    pub committed: u64,
    /// Commits a validation refused.
    pub refused: u64,
    /// Writes refused by the first writer's win.
    pub conflicts: u64,
}

impl Run {
    pub fn anomalies(&self, w: Workload) -> BTreeSet<Anomaly> {
        if w.lists {
            let finals = self.finals.iter().map(|(k, r)| (*k, decode(r))).collect();
            check_lists(&self.history, &finals)
        } else {
            let finals = self
                .finals
                .iter()
                .map(|(k, r)| (*k, Some(key_of(r))))
                .collect();
            check_registers(&self.history, &finals)
        }
    }
}

fn database(validation: Validation) -> Versioned {
    // A page holds four of the largest cells (page.rs, MAX_CELL) and a split leaves at least two:
    // a leaf for every two keys a seed can make, and fewer inner pages than leaves.
    let frames = KEYS + TXNS * MAX_LEN as usize;
    Versioned::new(frames, 1, |_| Box::new(|a: &[u8], b: &[u8]| a.cmp(b)))
        .unwrap()
        .with_validation(validation)
}

fn finals(v: &Versioned) -> Vec<(u64, Vec<u8>)> {
    let mut d = Descender::new();
    let snap = v.clock.snapshot().unwrap();
    let rows = v
        .tree(0)
        .unwrap()
        .scan(&v.pool, &mut d, &v.clock, &snap, None, usize::MAX)
        .unwrap();
    v.clock.close(snap.start);
    rows.into_iter().map(|(k, r)| (key_of(&k), r)).collect()
}

/// Ends `txn`: committed (its timestamp) or refused, counted; any other error fails the run.
fn finish(v: &Versioned, d: &mut Descender, txn: VTxn, run: &mut Run, mops: Vec<Mop>) {
    match v.commit(txn, d) {
        Ok(ts) => {
            run.committed += 1;
            run.history.push(Record {
                mops,
                committed: true,
                ts,
            });
        }
        Err(LimboError::SerializationFailure) => {
            run.refused += 1;
            run.history.push(Record {
                mops,
                committed: false,
                ts: 0,
            });
        }
        Err(e) => panic!("commit: {e}"),
    }
}

/// A run in one thread: 32 transactions open at once, the next operation each time that of one
/// the seed picks.
pub fn interleaved(validation: Validation, w: Workload, seed: u64) -> Run {
    struct Open {
        txn: VTxn,
        plan: Vec<Plan>,
        at: usize,
        mops: Vec<Mop>,
    }
    let v = database(validation);
    let mut d = Descender::new();
    let mut generator = Gen::new(seed);
    let mut pick = seed ^ 0x5bd1_e995;
    let ids = AtomicU64::new(1);
    let mut run = Run {
        history: Vec::new(),
        finals: Vec::new(),
        committed: 0,
        refused: 0,
        conflicts: 0,
    };
    let mut open: Vec<Option<Open>> = (0..WRITERS).map(|_| None).collect();
    let mut started = 0;
    while started < TXNS || open.iter().any(Option::is_some) {
        let at = (splitmix(&mut pick) % WRITERS as u64) as usize;
        let Some(mut o) = open[at].take() else {
            if started < TXNS {
                started += 1;
                open[at] = Some(Open {
                    txn: v.clock.begin().unwrap(),
                    plan: generator.txn(),
                    at: 0,
                    mops: Vec::new(),
                });
            }
            continue;
        };
        let Some(&plan) = o.plan.get(o.at) else {
            finish(&v, &mut d, o.txn, &mut run, o.mops);
            continue;
        };
        match step(&v, &mut d, w, &mut o.txn, plan, &ids, &mut o.mops) {
            Ok(()) => {
                o.at += 1;
                open[at] = Some(o);
            }
            Err(LimboError::WriteWriteConflict) => {
                run.conflicts += 1;
                v.abort(o.txn, &mut d).unwrap();
                run.history.push(Record {
                    mops: o.mops,
                    committed: false,
                    ts: 0,
                });
            }
            Err(e) => panic!("seed {seed}: {e}"),
        }
    }
    run.finals = finals(&v);
    run
}

/// A run on 32 threads at once, each taking its next transaction from the shared generator.
pub fn threaded(validation: Validation, w: Workload, seed: u64) -> Run {
    let v = Arc::new(database(validation));
    let generator = Arc::new(Mutex::new(Gen::new(seed)));
    let ids = Arc::new(AtomicU64::new(1));
    let started = Arc::new(AtomicUsize::new(0));
    let threads: Vec<_> = (0..WRITERS)
        .map(|_| {
            let (v, generator, ids, started) =
                (v.clone(), generator.clone(), ids.clone(), started.clone());
            std::thread::spawn(move || {
                let mut d = Descender::new();
                let mut run = Run {
                    history: Vec::new(),
                    finals: Vec::new(),
                    committed: 0,
                    refused: 0,
                    conflicts: 0,
                };
                'txns: while started.fetch_add(1, Ordering::Relaxed) < TXNS {
                    let plan = generator.lock().unwrap().txn();
                    let mut txn = v.clock.begin().unwrap();
                    let mut mops = Vec::new();
                    for p in plan {
                        match step(&v, &mut d, w, &mut txn, p, &ids, &mut mops) {
                            Ok(()) => {}
                            Err(LimboError::WriteWriteConflict) => {
                                run.conflicts += 1;
                                v.abort(txn, &mut d).unwrap();
                                run.history.push(Record {
                                    mops,
                                    committed: false,
                                    ts: 0,
                                });
                                continue 'txns;
                            }
                            Err(e) => panic!("seed {seed}: {e}"),
                        }
                    }
                    finish(&v, &mut d, txn, &mut run, mops);
                }
                run
            })
        })
        .collect();
    let mut all = Run {
        history: Vec::new(),
        finals: Vec::new(),
        committed: 0,
        refused: 0,
        conflicts: 0,
    };
    for t in threads {
        let run = t.join().unwrap();
        all.history.extend(run.history);
        all.committed += run.committed;
        all.refused += run.refused;
        all.conflicts += run.conflicts;
    }
    all.finals = finals(&v);
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(name: &str, default: u64) -> u64 {
        std::env::var(name)
            .ok()
            .map_or(default, |v| v.parse().unwrap())
    }

    fn validations() -> Vec<Validation> {
        match std::env::var("AMPHORA_ELLE_VALIDATION").as_deref() {
            Ok("precision") => vec![Validation::Precision],
            Ok("ssi") => vec![Validation::Ssi],
            Ok("occ") => vec![Validation::Occ],
            Ok(other) => panic!("AMPHORA_ELLE_VALIDATION: {other}: precision, ssi or occ"),
            Err(_) => vec![Validation::Precision, Validation::Ssi, Validation::Occ],
        }
    }

    fn txn(mops: Vec<Mop>, committed: bool, ts: u64) -> Record {
        Record {
            mops,
            committed,
            ts,
        }
    }
    fn app(key: u64, element: u64) -> Mop {
        Mop::Append { key, element }
    }
    fn list(key: u64, seen: &[u64]) -> Mop {
        Mop::List {
            key,
            seen: seen.to_vec(),
        }
    }
    fn lists(finals: &[(u64, &[u64])]) -> BTreeMap<u64, Vec<u64>> {
        finals.iter().map(|(k, l)| (*k, l.to_vec())).collect()
    }

    /// T-G8.1.1 (the checker): do check hand-made list-append histories, one an anomaly each.
    /// Expect: each named as its class, and a serial history as none.
    #[test]
    fn g8_1_the_list_checker_names_each_anomaly() {
        let serial = [
            txn(vec![app(1, 1)], true, 1),
            txn(vec![list(1, &[1]), app(1, 2)], true, 2),
        ];
        assert!(check_lists(&serial, &lists(&[(1, &[1, 2])])).is_empty());
        let g0 = [
            txn(vec![app(1, 1), app(2, 2)], true, 1),
            txn(vec![app(1, 3), app(2, 4)], true, 2),
        ];
        let found = check_lists(&g0, &lists(&[(1, &[1, 3]), (2, &[4, 2])]));
        assert!(found.contains(&Anomaly::G0), "{found:?}");
        let g1a = [
            txn(vec![app(1, 1)], false, 0),
            txn(vec![list(1, &[1])], true, 1),
        ];
        let found = check_lists(&g1a, &lists(&[]));
        assert!(found.contains(&Anomaly::G1a), "{found:?}");
        let g1b = [
            txn(vec![app(1, 1), app(1, 2)], true, 1),
            txn(vec![list(1, &[1])], true, 2),
        ];
        let found = check_lists(&g1b, &lists(&[(1, &[1, 2])]));
        assert!(found.contains(&Anomaly::G1b), "{found:?}");
        let g1c = [
            txn(vec![app(1, 1), list(2, &[2])], true, 1),
            txn(vec![app(2, 2), list(1, &[1])], true, 2),
        ];
        let found = check_lists(&g1c, &lists(&[(1, &[1]), (2, &[2])]));
        assert_eq!(found, BTreeSet::from([Anomaly::G1c]));
        let single = [
            txn(vec![list(1, &[]), list(2, &[2])], true, 1),
            txn(vec![app(1, 1), app(2, 2)], true, 2),
        ];
        let found = check_lists(&single, &lists(&[(1, &[1]), (2, &[2])]));
        assert_eq!(found, BTreeSet::from([Anomaly::GSingle]));
        let skew = [
            txn(vec![list(1, &[]), app(2, 1)], true, 1),
            txn(vec![list(2, &[]), app(1, 2)], true, 2),
        ];
        let found = check_lists(&skew, &lists(&[(1, &[2]), (2, &[1])]));
        assert_eq!(found, BTreeSet::from([Anomaly::G2]));
        let internal = [txn(vec![app(1, 1), list(1, &[])], true, 1)];
        let found = check_lists(&internal, &lists(&[(1, &[1])]));
        assert!(found.contains(&Anomaly::Internal), "{found:?}");
        let lost = [txn(vec![app(1, 1)], true, 1)];
        let found = check_lists(&lost, &lists(&[]));
        assert!(found.contains(&Anomaly::Lost), "{found:?}");
    }

    /// T-G8.1.1 (the checker): do check hand-made register histories. Expect: read skew named
    /// G-single, aborted and intermediate reads G1a and G1b, write skew G2, a lost last write
    /// Lost, and a serial history none.
    #[test]
    fn g8_1_the_register_checker_names_each_anomaly() {
        let w = |key, value| Mop::Write { key, value };
        let r = |key, seen| Mop::Reg { key, seen };
        let regs = |f: &[(u64, u64)]| -> BTreeMap<u64, Option<u64>> {
            f.iter().map(|(k, v)| (*k, Some(*v))).collect()
        };
        let serial = [
            txn(vec![w(1, 1)], true, 1),
            txn(vec![r(1, Some(1)), w(1, 2)], true, 2),
        ];
        assert!(check_registers(&serial, &regs(&[(1, 2)])).is_empty());
        let single = [
            txn(vec![r(1, None), r(2, Some(2))], true, 1),
            txn(vec![w(1, 1), w(2, 2)], true, 2),
        ];
        let found = check_registers(&single, &regs(&[(1, 1), (2, 2)]));
        assert_eq!(found, BTreeSet::from([Anomaly::GSingle]));
        let g1a = [
            txn(vec![w(1, 1)], false, 0),
            txn(vec![r(1, Some(1))], true, 1),
        ];
        let found = check_registers(&g1a, &BTreeMap::new());
        assert!(found.contains(&Anomaly::G1a), "{found:?}");
        let g1b = [
            txn(vec![w(1, 1), w(1, 2)], true, 1),
            txn(vec![r(1, Some(1))], true, 2),
        ];
        let found = check_registers(&g1b, &regs(&[(1, 2)]));
        assert!(found.contains(&Anomaly::G1b), "{found:?}");
        let skew = [
            txn(vec![r(1, None), w(2, 1)], true, 1),
            txn(vec![r(2, None), w(1, 2)], true, 2),
        ];
        let found = check_registers(&skew, &regs(&[(1, 2), (2, 1)]));
        assert_eq!(found, BTreeSet::from([Anomaly::G2]));
        let lost = [txn(vec![w(1, 1)], true, 1), txn(vec![w(1, 2)], true, 2)];
        let found = check_registers(&lost, &regs(&[(1, 1)]));
        assert!(found.contains(&Anomaly::Lost), "{found:?}");
    }

    /// Each seed's two histories, lists and registers, point reads on even seeds and spans on
    /// odd ones, through `run`; failing on any anomaly. Committed, refused and conflicting
    /// transactions summed.
    fn seeds(
        validation: Validation,
        first: u64,
        count: u64,
        run: fn(Validation, Workload, u64) -> Run,
    ) -> [u64; 3] {
        let mut sums = [0u64; 3];
        for seed in first..first + count {
            for lists in [true, false] {
                let w = Workload {
                    lists,
                    spans: seed % 2 == 1,
                };
                let r = run(validation, w, seed);
                let found = r.anomalies(w);
                assert!(
                    found.is_empty(),
                    "{validation:?} {w:?} seed {seed}: {found:?}"
                );
                sums[0] += r.committed;
                sums[1] += r.refused;
                sums[2] += r.conflicts;
            }
        }
        sums
    }

    /// T-G8.1.1, AC-G8.1.1: do run Elle's list-append and register histories under each
    /// serializable validation, seeds `AMPHORA_ELLE_FIRST` on (`AMPHORA_ELLE_SEEDS` of them),
    /// 32 transactions open at once in an order each seed decides. Expect: no G0, G1a, G1b,
    /// G1c, G-single or G2 (nor a transaction disagreeing with itself, an order no versions
    /// explain, or a lost write), with commits made and refused by validation (not vacuous).
    #[test]
    fn g8_1_interleaved_histories_have_no_anomaly_under_each_validation() {
        let (first, count) = (env("AMPHORA_ELLE_FIRST", 1), env("AMPHORA_ELLE_SEEDS", 100));
        for validation in validations() {
            let [committed, refused, conflicts] = seeds(validation, first, count, interleaved);
            eprintln!(
                "{validation:?}: seeds {first}..{}: {committed} committed, {refused} refused by \
                 validation, {conflicts} by the first writer",
                first + count
            );
            assert!(committed > 0 && refused > 0, "{validation:?}");
        }
    }

    /// T-G8.1.1: as the interleaved run, on 32 threads at once (`AMPHORA_ELLE_THREADED_SEEDS`,
    /// 4 unless set): the order the hardware makes, the publish lock and the latches contended.
    #[test]
    fn g8_1_threaded_histories_have_no_anomaly_under_each_validation() {
        let count = env("AMPHORA_ELLE_THREADED_SEEDS", 4);
        for validation in validations() {
            let [committed, refused, conflicts] = seeds(validation, 1, count, threaded);
            eprintln!(
                "{validation:?} threaded: {committed} committed, {refused} refused, {conflicts} \
                 conflicts"
            );
            assert!(committed > 0, "{validation:?}");
        }
    }

    /// T-G8.1.1's control: do the interleaved runs with no validation (snapshot isolation).
    /// Expect: write skew (G2) found in some seed, and nothing snapshot isolation forbids (G0,
    /// G1a, G1b, G1c, G-single) in any: the checker sees what validation is there to stop.
    #[test]
    fn g8_1_snapshot_isolation_shows_g2_and_nothing_it_forbids() {
        let mut g2 = 0;
        for seed in 1..=env("AMPHORA_ELLE_SEEDS", 100) {
            for lists in [true, false] {
                let w = Workload {
                    lists,
                    spans: seed % 2 == 1,
                };
                let found = interleaved(Validation::Snapshot, w, seed).anomalies(w);
                if found.contains(&Anomaly::G2) {
                    g2 += 1;
                }
                let forbidden: Vec<_> = found.iter().filter(|a| **a != Anomaly::G2).collect();
                assert!(forbidden.is_empty(), "{w:?} seed {seed}: {forbidden:?}");
            }
        }
        eprintln!("snapshot isolation: G2 in {g2} histories");
        assert!(
            g2 > 0,
            "no write skew found: the checker or the workload is vacuous"
        );
    }

    /// The generator's key draw: do draw 2,046,000 slots. Expect: slot `i` 2^i/1023 of them
    /// (Elle's exponential distribution), within six standard deviations of the binomial count.
    #[test]
    fn g8_1_the_generator_draws_keys_as_elle_does() {
        let mut g = Gen::new(7);
        let mut counts = [0u64; KEYS];
        let n = KEY_BASE_SCALE * 1000;
        for _ in 0..n {
            counts[g.slot()] += 1;
        }
        for (i, c) in counts.iter().enumerate() {
            let p = f64::from(1u32 << i) / 1023.0;
            let (want, sd) = (n as f64 * p, (n as f64 * p * (1.0 - p)).sqrt());
            assert!(
                (*c as f64 - want).abs() < 6.0 * sd,
                "slot {i}: {c} against {want}"
            );
        }
    }
}

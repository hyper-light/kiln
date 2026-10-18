//! Series and the index over them (`docs/observe.md` §4.1, §4.2).
//!
//! A series' id is dense, in order of first appearance, alike on every replica because each applies
//! the same batches in the same order. Postings — for each metric and each `key:value` tag, the ids
//! of its series — stay sorted for free: ids only grow. A selection intersects them smallest first
//! with galloping search (Bentley and Yao 1976; Demaine, López-Ortiz and Munro, SODA 2000).

use hashbrown::HashTable;

use crate::batch::{BatchError, Kind, fingerprint, parse_key};
use crate::hash::Keyed;
use crate::symbols::{Sym, Symbols, SymbolsFull};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IndexError {
    #[error(transparent)]
    Key(#[from] BatchError),
    #[error(transparent)]
    Symbols(#[from] SymbolsFull),
    #[error("the series table is full")]
    Full,
    #[error("fingerprint {0:016x} names another series")]
    Collision(u64),
    #[error("fingerprint {0:016x} is not its key's")]
    WrongFingerprint(u64),
}

/// What defining a series found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Defined {
    New(u32),
    Known(u32),
}

impl Defined {
    pub fn id(self) -> u32 {
        match self {
            Self::New(id) | Self::Known(id) => id,
        }
    }
}

/// What the index keeps of a series.
#[derive(Debug, Clone, Copy)]
pub struct SeriesInfo {
    pub metric: Sym,
    tags_at: u32,
    tags_len: u32,
    pub kind: Kind,
}

/// A posting list's owner: a metric or a tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owner {
    Metric(Sym),
    Tag(Sym, Sym),
}

#[derive(Debug)]
struct Posting {
    owner: Owner,
    ids: Vec<u32>,
}

#[derive(Debug, Default)]
pub struct Index {
    hash: Keyed,
    keys: Vec<u8>,
    key_ends: Vec<u32>,
    /// Each series' fingerprint, and the table that finds a series by it.
    fps: Vec<u64>,
    fp_table: HashTable<u32>,
    series: Vec<SeriesInfo>,
    tag_pairs: Vec<(Sym, Sym)>,
    post_table: HashTable<u32>,
    posts: Vec<Posting>,
    /// Each tag key's values, for wildcards and listing.
    values_table: HashTable<u32>,
    values: Vec<(Sym, Vec<Sym>)>,
    metrics: Vec<Sym>,
    /// Series written again with another kind than their first (the first is kept).
    pub kind_conflicts: u64,
}

impl Index {
    pub fn new(hash: Keyed) -> Self {
        Self { hash, ..Self::default() }
    }

    fn owner_hash(&self, o: Owner) -> u64 {
        match o {
            Owner::Metric(m) => self.hash.word(u64::from(m.0) | (1 << 63)),
            Owner::Tag(k, v) => self.hash.word((u64::from(k.0) << 32) | u64::from(v.0)),
        }
    }

    fn key_of(&self, id: u32) -> Option<&[u8]> {
        let i = id as usize;
        let end = *self.key_ends.get(i)? as usize;
        let start = if i == 0 { 0 } else { *self.key_ends.get(i - 1)? as usize };
        self.keys.get(start..end)
    }

    /// The series with fingerprint `fp`.
    #[inline]
    pub fn by_fp(&self, fp: u64) -> Option<u32> {
        self.fp_table.find(self.hash.word(fp), |&id| self.fps.get(id as usize) == Some(&fp)).copied()
    }

    /// The series with canonical key `key`.
    pub fn find(&self, key: &[u8]) -> Option<u32> {
        self.by_fp(fingerprint(key)).filter(|&id| self.key_of(id) == Some(key))
    }

    /// Defines series `fp` with canonical key `key`, if new. A new definition's fingerprint is
    /// checked against its key (a sender's bug cannot point a fingerprint at another series); a
    /// known one's key must be the one it has.
    pub fn define(&mut self, symbols: &mut Symbols, fp: u64, key: &[u8], kind: Kind) -> Result<Defined, IndexError> {
        if let Some(id) = self.by_fp(fp) {
            if self.key_of(id) != Some(key) {
                return Err(IndexError::Collision(fp));
            }
            if self.series.get(id as usize).is_some_and(|s| s.kind != kind) {
                self.kind_conflicts += 1;
            }
            return Ok(Defined::Known(id));
        }
        if fingerprint(key) != fp {
            return Err(IndexError::WrongFingerprint(fp));
        }
        let parsed = parse_key(key)?;
        let id = u32::try_from(self.series.len()).map_err(|_| IndexError::Full)?;
        let end = u32::try_from(self.keys.len() + key.len()).map_err(|_| IndexError::Full)?;
        let tags_at = u32::try_from(self.tag_pairs.len()).map_err(|_| IndexError::Full)?;
        let metric = symbols.intern(parsed.metric)?;
        let mut pairs = Vec::with_capacity(parsed.tags.len());
        for (k, v) in &parsed.tags {
            pairs.push((symbols.intern(k)?, symbols.intern(v)?));
        }
        self.keys.extend_from_slice(key);
        self.key_ends.push(end);
        self.fps.push(fp);
        let (fps, hash) = (&self.fps, &self.hash);
        self.fp_table.insert_unique(hash.word(fp), id, |&id| fps.get(id as usize).map_or(0, |&f| hash.word(f)));
        self.series.push(SeriesInfo { metric, tags_at, tags_len: pairs.len() as u32, kind });
        if self.post(Owner::Metric(metric), id) {
            self.metrics.push(metric);
        }
        for &(k, v) in &pairs {
            if self.post(Owner::Tag(k, v), id) {
                self.add_value(k, v);
            }
        }
        self.tag_pairs.extend_from_slice(&pairs);
        Ok(Defined::New(id))
    }

    /// Adds `id` to `owner`'s postings; whether the list is new.
    fn post(&mut self, owner: Owner, id: u32) -> bool {
        let h = self.owner_hash(owner);
        if let Some(&i) = self.post_table.find(h, |&i| self.posts.get(i as usize).is_some_and(|p| p.owner == owner)) {
            if let Some(p) = self.posts.get_mut(i as usize) {
                p.ids.push(id);
            }
            return false;
        }
        let i = self.posts.len() as u32;
        self.posts.push(Posting { owner, ids: vec![id] });
        let (posts, hasher) = (&self.posts, self.hash);
        self.post_table.insert_unique(h, i, |&i| posts.get(i as usize).map_or(0, |p| owner_hash_of(&hasher, p.owner)));
        true
    }

    fn add_value(&mut self, key: Sym, value: Sym) {
        let h = self.hash.word(u64::from(key.0));
        if let Some(&i) = self.values_table.find(h, |&i| self.values.get(i as usize).is_some_and(|v| v.0 == key)) {
            if let Some(v) = self.values.get_mut(i as usize) {
                v.1.push(value);
            }
            return;
        }
        let i = self.values.len() as u32;
        self.values.push((key, vec![value]));
        let (values, hasher) = (&self.values, self.hash);
        self.values_table.insert_unique(h, i, |&i| values.get(i as usize).map_or(0, |v| hasher.word(u64::from(v.0.0))));
    }

    fn postings(&self, owner: Owner) -> &[u32] {
        let h = self.owner_hash(owner);
        self.post_table
            .find(h, |&i| self.posts.get(i as usize).is_some_and(|p| p.owner == owner))
            .and_then(|&i| self.posts.get(i as usize))
            .map_or(&[], |p| p.ids.as_slice())
    }

    fn values_of(&self, key: Sym) -> &[Sym] {
        let h = self.hash.word(u64::from(key.0));
        self.values_table
            .find(h, |&i| self.values.get(i as usize).is_some_and(|v| v.0 == key))
            .and_then(|&i| self.values.get(i as usize))
            .map_or(&[], |v| v.1.as_slice())
    }

    pub fn len(&self) -> usize {
        self.series.len()
    }

    pub fn is_empty(&self) -> bool {
        self.series.is_empty()
    }

    pub fn info(&self, id: u32) -> Option<SeriesInfo> {
        self.series.get(id as usize).copied()
    }

    /// A series' tags, in key order.
    pub fn tags(&self, info: &SeriesInfo) -> &[(Sym, Sym)] {
        let start = info.tags_at as usize;
        self.tag_pairs.get(start..start + info.tags_len as usize).unwrap_or(&[])
    }

    /// Every metric's name.
    pub fn metrics(&self) -> &[Sym] {
        &self.metrics
    }

    /// Every value of tag `key`.
    pub fn tag_values(&self, symbols: &Symbols, key: &str) -> Vec<Sym> {
        symbols.find(key).map(|k| self.values_of(k).to_vec()).unwrap_or_default()
    }

    /// The series `sel` selects, sorted, into `out`. Each constraint (the metric unless it is `*`,
    /// then each positive clause) is a union of posting lists; the smallest is the start and the
    /// rest are intersected in order of size; negative clauses are subtracted last.
    pub fn select(&self, symbols: &Symbols, sel: &Selector, out: &mut Vec<u32>) {
        out.clear();
        let mut constraints: Vec<(usize, Vec<&[u32]>)> = Vec::new();
        match &sel.metric {
            NameMatch::Exact(m) => {
                let list = symbols.find(m).map_or(&[][..], |s| self.postings(Owner::Metric(s)));
                constraints.push((list.len(), vec![list]));
            }
            NameMatch::Glob(g) if g.chars().all(|c| c == '*') => {}
            NameMatch::Glob(g) => {
                let lists: Vec<&[u32]> = self.metrics.iter().filter(|&&m| glob(g, symbols.get(m))).map(|&m| self.postings(Owner::Metric(m))).collect();
                constraints.push((lists.iter().map(|l| l.len()).sum(), lists));
            }
        }
        let mut negative: Vec<Vec<&[u32]>> = Vec::new();
        for c in &sel.clauses {
            let lists = self.clause_lists(symbols, c);
            if c.negate {
                negative.push(lists);
            } else {
                constraints.push((lists.iter().map(|l| l.len()).sum(), lists));
            }
        }
        constraints.sort_by_key(|c| c.0);
        let n = self.series.len();
        let mut acc: Vec<u32> = Vec::new();
        let mut tmp: Vec<u32> = Vec::new();
        let mut u: Vec<u32> = Vec::new();
        let mut first = true;
        for (_, lists) in &constraints {
            if first {
                union_lists(lists, n, &mut acc);
                first = false;
            } else {
                if acc.is_empty() {
                    break;
                }
                match lists.as_slice() {
                    [one] => intersect(&acc, one, &mut tmp),
                    _ => {
                        union_lists(lists, n, &mut u);
                        intersect(&acc, &u, &mut tmp);
                    }
                }
                std::mem::swap(&mut acc, &mut tmp);
            }
        }
        if first {
            // No constraint but negations: every series.
            acc.extend(0..n as u32);
        }
        for lists in &negative {
            if acc.is_empty() {
                break;
            }
            union_lists(lists, n, &mut u);
            subtract(&acc, &u, &mut tmp);
            std::mem::swap(&mut acc, &mut tmp);
        }
        *out = acc;
    }

    fn clause_lists<'s>(&'s self, symbols: &Symbols, c: &Clause) -> Vec<&'s [u32]> {
        let Some(key) = symbols.find(&c.key) else { return Vec::new() };
        let mut lists = Vec::new();
        for v in &c.values {
            match v {
                ValueMatch::Exact(x) => {
                    if let Some(vs) = symbols.find(x) {
                        lists.push(self.postings(Owner::Tag(key, vs)));
                    }
                }
                ValueMatch::Glob(g) => {
                    for &vs in self.values_of(key) {
                        if glob(g, symbols.get(vs)) {
                            lists.push(self.postings(Owner::Tag(key, vs)));
                        }
                    }
                }
                ValueMatch::Any => {
                    for &vs in self.values_of(key) {
                        lists.push(self.postings(Owner::Tag(key, vs)));
                    }
                }
            }
        }
        lists
    }

    /// Bytes held by the index's own structures (symbols apart).
    pub fn heap_bytes(&self) -> usize {
        self.keys.capacity()
            + self.key_ends.capacity() * 4
            + self.fps.capacity() * 8
            + self.fp_table.capacity() * 4
            + self.series.capacity() * std::mem::size_of::<SeriesInfo>()
            + self.tag_pairs.capacity() * 8
            + self.post_table.capacity() * 4
            + self.posts.iter().map(|p| p.ids.capacity() * 4 + std::mem::size_of::<Posting>()).sum::<usize>()
            + self.values.iter().map(|v| v.1.capacity() * 4 + 32).sum::<usize>()
    }
}

fn owner_hash_of(h: &Keyed, o: Owner) -> u64 {
    match o {
        Owner::Metric(m) => h.word(u64::from(m.0) | (1 << 63)),
        Owner::Tag(k, v) => h.word((u64::from(k.0) << 32) | u64::from(v.0)),
    }
}

/// Which metric.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameMatch {
    Exact(String),
    /// `*` any run of characters, `?` one.
    Glob(String),
}

/// One tag's condition: any of `values` (OR), negated or not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clause {
    pub key: String,
    pub values: Vec<ValueMatch>,
    pub negate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueMatch {
    Exact(String),
    Glob(String),
    /// The series has the key, whatever its value (`key:*`).
    Any,
}

/// A metric and the conditions on its tags (AND).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selector {
    pub metric: NameMatch,
    pub clauses: Vec<Clause>,
}

impl Selector {
    pub fn metric(name: &str) -> Self {
        Self { metric: NameMatch::Exact(name.to_owned()), clauses: Vec::new() }
    }

    pub fn tag(mut self, key: &str, value: &str) -> Self {
        let v = if value == "*" { ValueMatch::Any } else if value.contains(['*', '?']) { ValueMatch::Glob(value.to_owned()) } else { ValueMatch::Exact(value.to_owned()) };
        self.clauses.push(Clause { key: key.to_owned(), values: vec![v], negate: false });
        self
    }

    pub fn not_tag(mut self, key: &str, value: &str) -> Self {
        self = self.tag(key, value);
        if let Some(c) = self.clauses.last_mut() {
            c.negate = true;
        }
        self
    }
}

/// Whether `text` matches `pattern` (`*` any run, `?` one character): the two-pointer algorithm
/// that remembers the last star, linear for patterns with one star and at worst O(n·m), without
/// recursion or allocation. The literal prefix before the first wildcard is checked first, which is
/// most of the work for the common `name.*` patterns.
pub fn glob(pattern: &str, text: &str) -> bool {
    let literal = pattern.find(['*', '?']).unwrap_or(pattern.len());
    let Some(prefix) = pattern.get(..literal) else { return false };
    if !text.starts_with(prefix) {
        return false;
    }
    if literal == pattern.len() {
        return text.len() == pattern.len();
    }
    let (p, t) = (pattern.as_bytes(), text.as_bytes());
    // Byte positions; `?` steps over one whole character of the text.
    let (mut pi, mut ti) = (literal, literal);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        match p.get(pi) {
            Some(b'*') => {
                star = Some((pi, ti));
                pi += 1;
            }
            Some(b'?') => {
                pi += 1;
                ti += char_len(t, ti);
            }
            Some(&c) if Some(&c) == t.get(ti) => {
                pi += 1;
                ti += 1;
            }
            _ => match star {
                Some((sp, st)) => {
                    let next = st + char_len(t, st);
                    pi = sp + 1;
                    ti = next;
                    star = Some((sp, next));
                }
                None => return false,
            },
        }
    }
    p.get(pi..).is_some_and(|rest| rest.iter().all(|&c| c == b'*'))
}

/// The byte length of the UTF-8 character that starts at `at` (1 past the end or inside one).
fn char_len(t: &[u8], at: usize) -> usize {
    match t.get(at) {
        Some(&b) if b >= 0xf0 => 4,
        Some(&b) if b >= 0xe0 => 3,
        Some(&b) if b >= 0xc0 => 2,
        _ => 1,
    }
}

/// `out` = `a` ∩ `b`, both sorted: each element of the shorter found in the longer by galloping
/// (exponential then binary search) from where the last was found.
pub fn intersect(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    let (small, large) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let mut lo = 0usize;
    for &x in small {
        // Gallop: the first index at or past x.
        let mut step = 1usize;
        let mut hi = lo;
        while large.get(hi).is_some_and(|&y| y < x) {
            lo = hi + 1;
            hi = hi.saturating_add(step);
            step = step.saturating_mul(2);
        }
        let end = hi.min(large.len());
        let at = large.get(lo..end).map_or(0, |s| s.partition_point(|&y| y < x));
        lo += at;
        match large.get(lo) {
            Some(&y) if y == x => {
                out.push(x);
                lo += 1;
            }
            Some(_) => {}
            None => break,
        }
    }
}

/// `out` = `a` − `b`, both sorted.
pub fn subtract(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    let mut j = 0usize;
    for &x in a {
        while b.get(j).is_some_and(|&y| y < x) {
            j += 1;
        }
        if b.get(j) != Some(&x) {
            out.push(x);
        }
    }
}

/// `out` = the union of sorted `lists` of ids below `n`: a bitmap over the ids when the lists are
/// long enough that scanning `n / 64` words costs less than sorting them, else a sort.
pub fn union_lists(lists: &[&[u32]], n: usize, out: &mut Vec<u32>) {
    let total: usize = lists.iter().map(|l| l.len()).sum();
    if lists.len() <= 1 || total.saturating_mul(total.max(2).ilog2() as usize) < n / 64 {
        union_into(lists, out);
        return;
    }
    out.clear();
    let mut bits = vec![0u64; n.div_ceil(64)];
    for l in lists {
        for &id in *l {
            if let Some(w) = bits.get_mut(id as usize / 64) {
                *w |= 1 << (id % 64);
            }
        }
    }
    out.reserve(total);
    for (i, &w) in bits.iter().enumerate() {
        let mut w = w;
        while w != 0 {
            let b = w.trailing_zeros();
            out.push((i * 64) as u32 + b);
            w &= w - 1;
        }
    }
}

/// `out` = the union of sorted `lists`.
pub fn union_into(lists: &[&[u32]], out: &mut Vec<u32>) {
    out.clear();
    match lists {
        [] => {}
        [one] => out.extend_from_slice(one),
        _ => {
            for l in lists {
                out.extend_from_slice(l);
            }
            out.sort_unstable();
            out.dedup();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::SeriesKey;
    use crate::testkit::Rng;

    fn key(metric: &str, tags: &[(&str, &str)]) -> SeriesKey {
        SeriesKey::new(metric, tags.iter().copied(), Kind::Gauge).unwrap()
    }

    fn index_of(series: &[(&str, &[(&str, &str)])]) -> (Index, Symbols) {
        let h = Keyed::with_keys(1, 2);
        let mut idx = Index::new(h);
        let mut sym = Symbols::new(h);
        for (m, tags) in series {
            let k = key(m, tags);
            idx.define(&mut sym, k.fp, &k.key, Kind::Gauge).unwrap();
        }
        (idx, sym)
    }

    #[test]
    fn selection_by_metric_tags_wildcards_alternatives_and_negations() {
        let (idx, sym) = index_of(&[
            ("cpu", &[("host", "web-1"), ("env", "prod")]),
            ("cpu", &[("host", "web-2"), ("env", "prod")]),
            ("cpu", &[("host", "db-1"), ("env", "prod")]),
            ("cpu", &[("host", "web-3"), ("env", "dev")]),
            ("mem", &[("host", "web-1"), ("env", "prod")]),
            ("cpu.idle", &[("host", "web-1")]),
        ]);
        let mut out = Vec::new();
        idx.select(&sym, &Selector::metric("cpu"), &mut out);
        assert_eq!(out, vec![0, 1, 2, 3]);
        idx.select(&sym, &Selector::metric("cpu").tag("env", "prod"), &mut out);
        assert_eq!(out, vec![0, 1, 2]);
        idx.select(&sym, &Selector::metric("cpu").tag("host", "web-*"), &mut out);
        assert_eq!(out, vec![0, 1, 3]);
        idx.select(&sym, &Selector::metric("cpu").tag("host", "web-*").not_tag("env", "dev"), &mut out);
        assert_eq!(out, vec![0, 1]);
        idx.select(&sym, &Selector::metric("cpu").tag("nothere", "x"), &mut out);
        assert!(out.is_empty());
        idx.select(&sym, &Selector::metric("nothere"), &mut out);
        assert!(out.is_empty());
        let sel = Selector { metric: NameMatch::Glob("cpu*".into()), clauses: vec![Clause { key: "host".into(), values: vec![ValueMatch::Exact("web-1".into()), ValueMatch::Exact("db-1".into())], negate: false }] };
        idx.select(&sym, &sel, &mut out);
        assert_eq!(out, vec![0, 2, 5]);
        idx.select(&sym, &Selector::metric("cpu.idle").tag("host", "*"), &mut out);
        assert_eq!(out, vec![5]);
        idx.select(&sym, &Selector::metric("cpu").not_tag("host", "*"), &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn a_series_is_found_again_by_its_key_whatever_the_order_its_tags_came_in() {
        let h = Keyed::with_keys(1, 2);
        let mut idx = Index::new(h);
        let mut sym = Symbols::new(h);
        let ka = key("m", &[("b", "2"), ("a", "1")]);
        let kb = key("m", &[("a", "1"), ("b", "2")]);
        let a = idx.define(&mut sym, ka.fp, &ka.key, Kind::Gauge).unwrap();
        let b = idx.define(&mut sym, kb.fp, &kb.key, Kind::Count).unwrap();
        assert!(matches!(a, Defined::New(_)) && matches!(b, Defined::Known(_)));
        let a = a.id();
        assert_eq!(a, b.id());
        assert_eq!(idx.kind_conflicts, 1);
        assert_eq!(idx.by_fp(ka.fp), Some(a));
        // A fingerprint that is not its key's, and a known fingerprint with another key, are refused.
        let other = key("m", &[("a", "9")]);
        assert_eq!(idx.define(&mut sym, ka.fp ^ 1, &other.key, Kind::Gauge), Err(IndexError::WrongFingerprint(ka.fp ^ 1)));
        assert_eq!(idx.define(&mut sym, ka.fp, &other.key, Kind::Gauge), Err(IndexError::Collision(ka.fp)));
        let info = idx.info(a).unwrap();
        let tags: Vec<(&str, &str)> = idx.tags(&info).iter().map(|&(k, v)| (sym.get(k), sym.get(v))).collect();
        assert_eq!(tags, vec![("a", "1"), ("b", "2")]);
        assert_eq!(idx.find(&key("m", &[("a", "1"), ("b", "2")]).key), Some(a));
    }

    #[test]
    fn set_operations_agree_with_a_plain_model() {
        let mut rng = Rng::new(3);
        for _ in 0..500 {
            let mut a: Vec<u32> = (0..rng.below(300)).map(|_| rng.below(1000) as u32).collect();
            let mut b: Vec<u32> = (0..rng.below(300)).map(|_| rng.below(1000) as u32).collect();
            a.sort_unstable();
            a.dedup();
            b.sort_unstable();
            b.dedup();
            let mut out = Vec::new();
            intersect(&a, &b, &mut out);
            let want: Vec<u32> = a.iter().copied().filter(|x| b.contains(x)).collect();
            assert_eq!(out, want);
            subtract(&a, &b, &mut out);
            let want: Vec<u32> = a.iter().copied().filter(|x| !b.contains(x)).collect();
            assert_eq!(out, want);
            union_into(&[&a, &b], &mut out);
            let mut want: Vec<u32> = a.iter().chain(&b).copied().collect();
            want.sort_unstable();
            want.dedup();
            assert_eq!(out, want);
            union_lists(&[&a, &b, &a], 1000, &mut out);
            assert_eq!(out, want);
        }
    }

    #[test]
    fn globs() {
        for (p, t, m) in [
            ("web-*", "web-12", true),
            ("web-*", "db-1", false),
            ("*", "", true),
            ("", "", true),
            ("", "a", false),
            ("a?c", "abc", true),
            ("a?c", "ac", false),
            ("*.cpu.*", "system.cpu.user", true),
            ("*a*b*c*", "xaxbxcx", true),
            ("*a*b*c*", "xaxcxbx", false),
            ("ü*", "üñ", true),
            ("**", "x", true),
            ("?ñ", "üñ", true),
            ("*ñ", "aüñ", true),
            ("a*", "b", false),
            ("abc", "abc", true),
            ("abc", "abcd", false),
            ("*:9100", "10.0.0.1:9100", true),
            ("*:9100", "10.0.0.1:9101", false),
        ] {
            assert_eq!(glob(p, t), m, "{p:?} {t:?}");
        }
    }
}

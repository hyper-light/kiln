//! The search across a change (docs/ci.md W26-R07): kiln-seams' [`Search`] on vorpal, vendored at
//! vendor/vorpal (vendor/vorpal/UPSTREAM.md says what of it kiln takes).
//!
//! Each tree's index is a folder of its own under the root it's opened on (the server's cache),
//! named by the tree's id and written whole before it's renamed into place:
//! - `contents.bin`: every file's bytes, one after another;
//! - `table.bin`: the files, by path, each with where its bytes are and its definitions, read by
//!   vorpal's outline (the rules vorpal ships for each language) at build;
//! - `trigrams/`: every file's byte trigrams in vorpal's trigram store, which says which files may
//!   hold a query's literals, so a query reads only those.
//!
//! A text query is a regex (its words, escaped, or the regex between its slashes) run over the
//! files the trigrams admit, a hit per line; a code query is compiled by vorpal's pattern engine for
//! each language the tree's files are in, and matched against the syntax trees of the files of that
//! language whose trigrams hold the pattern's literals. A hit names the innermost definition whose
//! span holds where it starts. Work runs on a pool of its own; the trees kept are bounded, the one
//! searched least recently forgotten past the bound, and a few are kept open between searches.

use std::collections::HashMap;
use std::io::Write as _;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use futures::future::BoxFuture;
use kiln_seams::search::{self as seam, Built, File, Found, Hit, Mode, Query, Refusal, Search, Symbol};
use parking_lot::Mutex;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use vorpal_core::matcher::{Pattern, Prefilter, regex_literal_branches, regex_required_literals};
use vorpal_core::trigram::{MaskMap, extract, plan};
use vorpal_kg::identity::{FileKey, bucket_count_for};
use vorpal_kg::trigramstore::{self, CandidateSet, FileTrigrams, TrigramStore};
use vorpal_language::{Language as _, LanguageExt as _, SupportLang};
use vorpal_outline::DEFAULT_OUTLINE_RULES;
use vorpal_outline::combined_extractor::CombinedExtractors;
use vorpal_outline::extractor::{SerializableOutlineRule, parse_outline_rules};

/// The index's format: an index of another is built again.
const FORMAT: u32 = 1;
/// Indexes kept open between searches (their tables read, their trigrams mapped).
const OPEN_MAX: usize = 8;
const CONTENTS: &str = "contents.bin";
const TABLE: &str = "table.bin";
/// Touched at each search: the order trees are forgotten in, kept across restarts.
const USED: &str = "used";

/// How much a [`VorpalSearch`] keeps and uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Trees kept: past them, building another forgets the one searched least recently.
    pub trees: usize,
    /// Threads its pool runs builds and searches on.
    pub threads: usize,
}

impl Default for Limits {
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map_or(2, std::num::NonZeroUsize::get);
        Self { trees: 64, threads: (cores / 2).clamp(2, 16) }
    }
}

/// kiln-seams' [`Search`] on vorpal. Clones share their trees and pool.
#[derive(Clone)]
pub struct VorpalSearch {
    inner: Arc<Inner>,
}

struct Inner {
    root: PathBuf,
    limits: Limits,
    /// Made at the first build or search: a server that never searches starts no threads.
    pool: OnceLock<Result<rayon::ThreadPool, String>>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Every tree built, the one searched (or built) least recently first.
    order: Vec<String>,
    /// Indexes open, the one used last last.
    open: Vec<(String, Arc<Index>)>,
}

/// What `table.bin` holds.
#[derive(Serialize, Deserialize)]
struct Table {
    format: u32,
    files: u64,
    skipped: u64,
    bytes: u64,
    buckets: u32,
    folds: Vec<u64>,
    rows: Vec<Row>,
}

#[derive(Serialize, Deserialize)]
struct Row {
    path: String,
    offset: u64,
    len: u64,
    defs: Vec<Def>,
}

#[derive(Serialize, Deserialize)]
struct Def {
    start: u32,
    end: u32,
    name: String,
    kind: String,
}

/// An index, open.
struct Index {
    table: Table,
    store: Option<TrigramStore>,
    contents: std::fs::File,
    /// Each file's key in the trigram store, and the files under it.
    by_key: HashMap<u64, Vec<usize>>,
    /// Each file's language, by vorpal's routing of its path.
    langs: Vec<Option<SupportLang>>,
}

impl Index {
    fn built(&self) -> Built {
        Built { files: self.table.files, skipped: self.table.skipped, bytes: self.table.bytes }
    }

    fn bytes(&self, i: usize) -> std::io::Result<Vec<u8>> {
        let row = self.table.rows.get(i).ok_or_else(|| std::io::Error::other("no such file"))?;
        let len = usize::try_from(row.len).map_err(std::io::Error::other)?;
        read_at(&self.contents, row.offset, len)
    }

    /// The files the literal branches admit (each branch's files hold all its literals; the union of
    /// the branches), or none when every file is a candidate: no store, a branch with no literal of
    /// three bytes to prune by, or a bucket the store doesn't cover.
    fn admitted(&self, branches: &[Vec<String>]) -> Option<Vec<usize>> {
        let store = self.store.as_ref()?;
        let mut union: Option<CandidateSet> = None;
        for branch in branches {
            let lits: Vec<&[u8]> = branch.iter().map(String::as_bytes).collect();
            let set = store.candidates(&plan(&lits)?);
            match union.as_mut() {
                Some(u) => u.union_with(&set),
                None => union = Some(set),
            }
        }
        let set = union.filter(CandidateSet::is_complete)?;
        let mut files: Vec<usize> = set.admitted().iter().filter_map(|k| self.by_key.get(k)).flatten().copied().collect();
        files.sort_unstable();
        files.dedup();
        Some(files)
    }

    fn symbol_at(&self, i: usize, at: usize) -> Option<Symbol> {
        let defs = &self.table.rows.get(i)?.defs;
        let at = u32::try_from(at).ok()?;
        defs.iter().filter(|d| d.start <= at && at < d.end).min_by_key(|d| d.end.saturating_sub(d.start)).map(|d| Symbol { name: d.name.clone(), kind: d.kind.clone() })
    }
}

#[cfg(unix)]
fn read_at(f: &std::fs::File, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
    use std::os::unix::fs::FileExt as _;
    let mut buf = vec![0u8; len];
    f.read_exact_at(&mut buf, offset)?;
    Ok(buf)
}

#[cfg(windows)]
fn read_at(f: &std::fs::File, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
    use std::os::windows::fs::FileExt as _;
    let mut buf = vec![0u8; len];
    let mut done = 0usize;
    while let Some(rest) = buf.get_mut(done..).filter(|r| !r.is_empty()) {
        let n = f.seek_read(rest, offset.saturating_add(done as u64))?;
        if n == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
        }
        done = done.saturating_add(n);
    }
    Ok(buf)
}

#[cfg(not(any(unix, windows)))]
fn read_at(f: &std::fs::File, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut f = f.try_clone()?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

fn unavailable(what: &str, e: impl std::fmt::Display) -> Refusal {
    Refusal::Unavailable(format!("{what}: {e}"))
}

/// The language vorpal routes a path to, among the grammars kiln takes.
fn language_of(path: &str) -> Option<SupportLang> {
    SupportLang::from_path(path)
}

/// A language's name as a query and a hit say it: vorpal's, lowercased (`rust`, `tsx`).
pub fn language_name(lang: SupportLang) -> String {
    format!("{lang:?}").to_lowercase()
}

/// vorpal's outline rules, compiled once for each language kiln takes.
fn extractors() -> &'static HashMap<SupportLang, CombinedExtractors<SupportLang>> {
    static EXTRACTORS: OnceLock<HashMap<SupportLang, CombinedExtractors<SupportLang>>> = OnceLock::new();
    EXTRACTORS.get_or_init(|| {
        let rules: Vec<SerializableOutlineRule<SupportLang>> = parse_outline_rules(DEFAULT_OUTLINE_RULES).unwrap_or_default();
        let mut by: HashMap<SupportLang, Vec<SerializableOutlineRule<SupportLang>>> = HashMap::new();
        for rule in rules {
            let lang = rule.common().language;
            if lang.is_enabled() {
                by.entry(lang).or_default().push(rule);
            }
        }
        by.into_iter().filter_map(|(lang, rules)| CombinedExtractors::try_from(rules, &Default::default()).ok().map(|e| (lang, e))).collect()
    })
}

/// A symbol type as vorpal's outline names it (`function`, `enumMember`).
fn kind_name(debug: &str) -> String {
    let mut chars = debug.chars();
    chars.next().map(|c| c.to_lowercase().chain(chars).collect()).unwrap_or_default()
}

/// The definitions vorpal's outline reads in `text`: each item but imports, and each member.
fn definitions(lang: SupportLang, text: &str) -> Vec<Def> {
    let Some(ex) = extractors().get(&lang) else { return Vec::new() };
    let grep = lang.grep(text);
    let def = |start: usize, end: usize, name: &str, kind: String| Some(Def { start: u32::try_from(start).ok()?, end: u32::try_from(end).ok()?, name: name.to_owned(), kind });
    let mut out = Vec::new();
    for item in ex.extract(grep.root()) {
        if item.is_import {
            continue;
        }
        let r = &item.entry.range.byte_offset;
        out.extend(def(r.start, r.end, &item.entry.name, kind_name(&format!("{:?}", item.entry.symbol_type))));
        for m in &item.members {
            let r = &m.entry.range.byte_offset;
            out.extend(def(r.start, r.end, &m.entry.name, kind_name(&format!("{:?}", m.entry.symbol_type))));
        }
    }
    out
}

/// One file read for the index.
struct Read {
    path: String,
    bytes: Vec<u8>,
    defs: Vec<Def>,
    trigrams: FileTrigrams,
    digest: u64,
}

/// Each line's start in `bytes`, from the first.
fn line_starts(bytes: &[u8]) -> Vec<usize> {
    std::iter::once(0).chain(memchr::memchr_iter(b'\n', bytes).map(|i| i.saturating_add(1))).collect()
}

/// The line (from 1) holding byte `at`, and where it starts.
fn line_at(starts: &[usize], at: usize) -> (usize, usize) {
    let i = starts.partition_point(|s| *s <= at).saturating_sub(1);
    (i.saturating_add(1), starts.get(i).copied().unwrap_or(0))
}

/// The hit at byte `at` (ending at `end`) of file `i`, whose lines start at `starts`.
fn hit(index: &Index, i: usize, bytes: &[u8], starts: &[usize], at: usize, end: usize) -> Option<Hit> {
    let row = index.table.rows.get(i)?;
    let (line, line_start) = line_at(starts, at);
    let (end_line, _) = line_at(starts, end.saturating_sub(1).max(at));
    let rest = bytes.get(line_start..)?;
    let line_end = memchr::memchr(b'\n', rest).map_or(bytes.len(), |n| line_start.saturating_add(n));
    let column = std::str::from_utf8(bytes.get(line_start..at)?).map_or(0, |s| s.chars().count());
    Some(Hit {
        path: row.path.clone(),
        line: u32::try_from(line).unwrap_or(u32::MAX),
        column: u32::try_from(column.saturating_add(1)).unwrap_or(u32::MAX),
        end_line: u32::try_from(end_line).unwrap_or(u32::MAX),
        text: seam::hit_text(&String::from_utf8_lossy(bytes.get(line_start..line_end)?)),
        symbol: index.symbol_at(i, at),
    })
}

impl VorpalSearch {
    /// Searches whose indexes live under `root`, made if it isn't there; trees already built there
    /// are kept, in the order they were last searched.
    pub fn open(root: impl Into<PathBuf>, limits: Limits) -> Result<Self, String> {
        let root = root.into();
        std::fs::create_dir_all(&root).map_err(|e| format!("{}: {e}", root.display()))?;
        let mut kept: Vec<(SystemTime, String)> = Vec::new();
        for entry in std::fs::read_dir(&root).map_err(|e| format!("{}: {e}", root.display()))?.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            if name.starts_with('.') {
                // A build or a forgetting a restart cut short.
                let _ = std::fs::remove_dir_all(&path);
                continue;
            }
            if seam::check_tree(&name).is_err() || !path.join(TABLE).is_file() {
                continue;
            }
            let used = std::fs::metadata(path.join(USED)).or_else(|_| std::fs::metadata(path.join(TABLE))).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
            kept.push((used, name));
        }
        kept.sort();
        let order = kept.into_iter().map(|(_, n)| n).collect();
        Ok(Self { inner: Arc::new(Inner { root, limits, pool: OnceLock::new(), state: Mutex::new(State { order, open: Vec::new() }) }) })
    }

    /// Where its indexes live.
    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    /// Runs `f` on the pool; a panic in it is an answer, `Unavailable`.
    fn run<T: Send + 'static>(&self, f: impl FnOnce(&Inner) -> Result<T, Refusal> + Send + 'static) -> BoxFuture<'static, Result<T, Refusal>> {
        let pool = self.inner.pool.get_or_init(|| {
            rayon::ThreadPoolBuilder::new()
                .num_threads(self.inner.limits.threads.max(1))
                .thread_name(|i| format!("kiln-search-{i}"))
                .panic_handler(|_| {})
                .build()
                .map_err(|e| format!("the search's threads: {e}"))
        });
        let pool = match pool {
            Ok(pool) => pool,
            Err(e) => return Box::pin(futures::future::ready(Err(Refusal::Unavailable(e.clone())))),
        };
        let inner = Arc::clone(&self.inner);
        let (tx, rx) = futures::channel::oneshot::channel();
        pool.spawn(move || {
            let out = std::panic::catch_unwind(AssertUnwindSafe(|| f(&inner))).unwrap_or_else(|_| Err(Refusal::Unavailable("the search stopped short (a panic)".into())));
            let _ = tx.send(out);
        });
        Box::pin(async move { rx.await.unwrap_or_else(|_| Err(Refusal::Unavailable("the search ended without an answer".into()))) })
    }
}

impl Inner {
    fn dir(&self, tree: &str) -> PathBuf {
        self.root.join(tree)
    }

    /// `tree` searched (or built) now: last in the order, and its folder says so.
    fn touch(&self, tree: &str) {
        let mut st = self.state.lock();
        st.order.retain(|t| t != tree);
        st.order.push(tree.to_owned());
        drop(st);
        let used = self.dir(tree).join(USED);
        if let Ok(f) = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&used) {
            let _ = f.set_modified(SystemTime::now());
        }
    }

    /// The index of `tree`, opened if it isn't; none when it isn't built.
    fn index(&self, tree: &str) -> Result<Option<Arc<Index>>, Refusal> {
        {
            let mut st = self.state.lock();
            if let Some(pos) = st.open.iter().position(|(t, _)| t == tree) {
                let entry = st.open.remove(pos);
                let index = Arc::clone(&entry.1);
                st.open.push(entry);
                return Ok(Some(index));
            }
        }
        let dir = self.dir(tree);
        let table = match std::fs::read(dir.join(TABLE)) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(unavailable("its table", e)),
        };
        let Ok(table) = postcard::from_bytes::<Table>(&table) else { return Ok(None) };
        if table.format != FORMAT {
            return Ok(None);
        }
        let contents = std::fs::File::open(dir.join(CONTENTS)).map_err(|e| unavailable("its files", e))?;
        let store = TrigramStore::open(&dir, &table.folds);
        let mut by_key: HashMap<u64, Vec<usize>> = HashMap::new();
        for (i, row) in table.rows.iter().enumerate() {
            by_key.entry(FileKey::of(&row.path).0).or_default().push(i);
        }
        let langs = table.rows.iter().map(|r| language_of(&r.path)).collect();
        let index = Arc::new(Index { table, store, contents, by_key, langs });
        let mut st = self.state.lock();
        st.open.retain(|(t, _)| t != tree);
        st.open.push((tree.to_owned(), Arc::clone(&index)));
        while st.open.len() > OPEN_MAX {
            st.open.remove(0);
        }
        if !st.order.iter().any(|t| t == tree) {
            st.order.push(tree.to_owned());
        }
        Ok(Some(index))
    }

    /// Drops `tree`'s folder: renamed out of the way first, so a reader never sees half of it.
    fn remove(&self, tree: &str) -> Result<(), Refusal> {
        {
            let mut st = self.state.lock();
            st.order.retain(|t| t != tree);
            st.open.retain(|(t, _)| t != tree);
        }
        let dir = self.dir(tree);
        if !dir.exists() {
            return Ok(());
        }
        let trash = self.root.join(format!(".forgotten-{tree}-{}", nonce()));
        std::fs::rename(&dir, &trash).map_err(|e| unavailable("forgetting it", e))?;
        // Still mapped by a search under way (on Windows, it can't go yet): the next start removes it.
        let _ = std::fs::remove_dir_all(&trash);
        Ok(())
    }

    fn build(&self, tree: &str, files: Vec<File>) -> Result<Built, Refusal> {
        seam::check_tree(tree)?;
        if let Some(index) = self.index(tree)? {
            self.touch(tree);
            return Ok(index.built());
        }
        if files.len() > seam::TREE_FILES_MAX {
            return Err(Refusal::TooBig { what: format!("a tree of {} files", files.len()), limit: seam::TREE_FILES_MAX as u64 });
        }
        let total = files.iter().map(|f| f.bytes.len() as u64).fold(0u64, u64::saturating_add);
        if total > seam::TREE_BYTES_MAX {
            return Err(Refusal::TooBig { what: format!("a tree of {total} bytes"), limit: seam::TREE_BYTES_MAX });
        }
        let mut paths = std::collections::HashSet::new();
        for f in &files {
            seam::check_path(&f.path)?;
            if !paths.insert(f.path.as_str()) {
                return Err(Refusal::Invalid { what: "a file's path", why: format!("{} comes twice", f.path) });
            }
        }
        drop(paths);
        let count = files.len() as u64;
        let mut read: Vec<Read> = files
            .into_par_iter()
            .map_init(MaskMap::default, |scratch, f| {
                if f.bytes.len() > seam::FILE_MAX || seam::is_binary(&f.bytes) {
                    return None;
                }
                let defs = match (language_of(&f.path), std::str::from_utf8(&f.bytes)) {
                    (Some(lang), Ok(text)) => definitions(lang, text),
                    _ => Vec::new(),
                };
                let key = FileKey::of(&f.path).0;
                let mut postings = Vec::new();
                extract(&f.bytes, scratch, &mut postings);
                let digest = xxhash_rust::xxh3::xxh3_64(&f.bytes);
                Some(Read { path: f.path, bytes: f.bytes, defs, trigrams: FileTrigrams { file_key: key, indexed: true, postings }, digest })
            })
            .flatten()
            .collect();
        read.sort_by(|a, b| a.path.cmp(&b.path));
        let skipped = count.saturating_sub(read.len() as u64);
        let buckets = bucket_count_for(read.len());
        let mask = u64::from(buckets.saturating_sub(1));
        let mut pairs: Vec<Vec<(u64, u64)>> = vec![Vec::new(); buckets as usize];
        for r in &read {
            if let Some(b) = usize::try_from(r.trigrams.file_key & mask).ok().and_then(|k| pairs.get_mut(k)) {
                b.push((r.trigrams.file_key, r.digest));
            }
        }
        let folds: Vec<u64> = pairs.iter_mut().map(trigramstore::content_fold).collect();

        let tmp = self.root.join(format!(".building-{tree}-{}", nonce()));
        let written = (|| -> std::io::Result<Table> {
            std::fs::create_dir_all(&tmp)?;
            let mut contents = std::io::BufWriter::new(std::fs::File::create(tmp.join(CONTENTS))?);
            let mut rows = Vec::with_capacity(read.len());
            let mut trigrams = Vec::with_capacity(read.len());
            let mut offset = 0u64;
            for r in read {
                contents.write_all(&r.bytes)?;
                let len = r.bytes.len() as u64;
                rows.push(Row { path: r.path, offset, len, defs: r.defs });
                trigrams.push(r.trigrams);
                offset = offset.saturating_add(len);
            }
            contents.flush()?;
            let table = Table { format: FORMAT, files: rows.len() as u64, skipped, bytes: offset, buckets, folds: folds.clone(), rows };
            trigramstore::save(&tmp, trigrams, buckets, &folds, None)?;
            std::fs::write(tmp.join(TABLE), postcard::to_allocvec(&table).map_err(std::io::Error::other)?)?;
            Ok(table)
        })();
        let table = match written {
            Ok(t) => t,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&tmp);
                return Err(unavailable("writing its index", e));
            }
        };
        let built = Built { files: table.files, skipped: table.skipped, bytes: table.bytes };
        if let Err(e) = std::fs::rename(&tmp, self.dir(tree)) {
            let _ = std::fs::remove_dir_all(&tmp);
            // Built at once by another: theirs stands.
            return match self.index(tree)? {
                Some(index) => Ok(index.built()),
                None => Err(unavailable("putting its index in place", e)),
            };
        }
        self.touch(tree);
        let over: Vec<String> = {
            let st = self.state.lock();
            let n = st.order.len().saturating_sub(self.limits.trees.max(1));
            st.order.iter().filter(|t| *t != tree).take(n).cloned().collect()
        };
        for t in over {
            self.remove(&t)?;
        }
        Ok(built)
    }

    fn search(&self, tree: &str, query: &Query) -> Result<Found, Refusal> {
        seam::check_tree(tree)?;
        seam::check_query(query)?;
        let index = self.index(tree)?.ok_or_else(|| Refusal::NotBuilt { tree: tree.to_owned() })?;
        self.touch(tree);
        match query.mode {
            Mode::Text => text(&index, query),
            Mode::Code => code(&index, query),
        }
    }
}

fn nonce() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let t = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    format!("{}-{t}-{n}", std::process::id())
}

fn text(index: &Index, query: &Query) -> Result<Found, Refusal> {
    let p = seam::text_pattern(&query.q)?;
    let re = regex::bytes::RegexBuilder::new(&format!("(?m){}", p.regex)).build().map_err(|e| Refusal::BadQuery { why: format!("the regex doesn't parse: {e}") })?;
    let (branches, required): (Vec<Vec<String>>, Vec<String>) = match (&p.exact, p.is_regex) {
        (Some(words), _) => (vec![vec![words.clone()]], vec![words.clone()]),
        (None, true) => (regex_literal_branches(&p.regex), regex_required_literals(&p.regex)),
        (None, false) => (Vec::new(), Vec::new()),
    };
    let candidates: Vec<usize> = if branches.is_empty() { (0..index.table.rows.len()).collect() } else { index.admitted(&branches).unwrap_or_else(|| (0..index.table.rows.len()).collect()) };
    let prefilter = Prefilter::from_literals(required.iter().map(String::as_str).collect());
    let per_file: Vec<Result<Vec<Hit>, Refusal>> = candidates
        .par_iter()
        .with_min_len(16)
        .map(|&i| {
            let bytes = index.bytes(i).map_err(|e| unavailable("reading a file", e))?;
            if !prefilter.may_match_bytes(&bytes) {
                return Ok(Vec::new());
            }
            let mut starts: Option<Vec<usize>> = None;
            let mut hits = Vec::new();
            let mut last_line = 0usize;
            for m in re.find_iter(&bytes) {
                let starts = starts.get_or_insert_with(|| line_starts(&bytes));
                let (line, _) = line_at(starts, m.start());
                if line == last_line {
                    continue;
                }
                last_line = line;
                // A text hit is the line its match starts on.
                hits.extend(hit(index, i, &bytes, starts, m.start(), m.start().saturating_add(1)));
            }
            Ok(hits)
        })
        .collect();
    let mut hits = Vec::new();
    for r in per_file {
        hits.extend(r?);
    }
    Ok(seam::found(hits, query.limit, index.table.files))
}

fn code(index: &Index, query: &Query) -> Result<Found, Refusal> {
    let only = match query.lang.as_deref() {
        Some(name) => Some(name.parse::<SupportLang>().map_err(|e| Refusal::BadQuery { why: format!("no language `{name}`: {e}") })?),
        None => None,
    };
    // Markdown reads any text as prose, never as an error: a code pattern searches it only when
    // asked to by name.
    let mut present: Vec<SupportLang> = index.langs.iter().flatten().copied().filter(|l| only.map_or(*l != SupportLang::Markdown, |o| o == *l)).collect();
    present.sort_by_key(|l| language_name(*l));
    present.dedup();
    if present.is_empty() {
        return Ok(seam::found(Vec::new(), query.limit, index.table.files));
    }
    let mut compiled: Vec<(SupportLang, Pattern, Prefilter)> = Vec::new();
    let mut why: Option<String> = None;
    for lang in present {
        match Pattern::try_new_smart(&query.q, lang) {
            // A pattern tree-sitter could only read by recovering from an error matches nothing
            // meant: it doesn't parse in that language.
            Ok(p) if p.has_error() => {
                why.get_or_insert_with(|| format!("{}: it reads only as an error", language_name(lang)));
            }
            Ok(p) => {
                let pf = Prefilter::for_pattern(&p);
                compiled.push((lang, p, pf));
            }
            Err(e) => {
                why.get_or_insert_with(|| format!("{}: {e}", language_name(lang)));
            }
        }
    }
    if compiled.is_empty() {
        return Err(Refusal::BadQuery { why: format!("the pattern doesn't parse in any language of the tree's files ({})", why.unwrap_or_default()) });
    }
    let mut work: Vec<(usize, usize)> = Vec::new();
    for (c, (lang, pattern, _)) in compiled.iter().enumerate() {
        let literals: Vec<String> = pattern.required_literals().into_iter().map(str::to_owned).collect();
        let of_lang = |i: &usize| index.langs.get(*i).copied().flatten() == Some(*lang);
        let files: Vec<usize> = match (!literals.is_empty()).then(|| index.admitted(&[literals])).flatten() {
            Some(admitted) => admitted.into_iter().filter(of_lang).collect(),
            None => (0..index.table.rows.len()).filter(of_lang).collect(),
        };
        work.extend(files.into_iter().map(|i| (c, i)));
    }
    let per_file: Vec<Result<Vec<Hit>, Refusal>> = work
        .par_iter()
        .map(|&(c, i)| {
            let Some((lang, pattern, prefilter)) = compiled.get(c) else { return Ok(Vec::new()) };
            let bytes = index.bytes(i).map_err(|e| unavailable("reading a file", e))?;
            if !prefilter.may_match_bytes(&bytes) {
                return Ok(Vec::new());
            }
            let Ok(text) = std::str::from_utf8(&bytes) else { return Ok(Vec::new()) };
            let grep = lang.grep(text);
            let starts = line_starts(&bytes);
            Ok(grep.root().find_all(pattern).filter_map(|m| {
                let r = m.range();
                hit(index, i, &bytes, &starts, r.start, r.end)
            }).collect())
        })
        .collect();
    let mut hits = Vec::new();
    for r in per_file {
        hits.extend(r?);
    }
    Ok(seam::found(hits, query.limit, index.table.files))
}

impl Search for VorpalSearch {
    fn build<'a>(&'a self, tree: &'a str, files: Vec<File>) -> BoxFuture<'a, Result<Built, Refusal>> {
        let tree = tree.to_owned();
        self.run(move |inner| inner.build(&tree, files))
    }

    fn built<'a>(&'a self, tree: &'a str) -> BoxFuture<'a, Result<Option<Built>, Refusal>> {
        let tree = tree.to_owned();
        self.run(move |inner| {
            seam::check_tree(&tree)?;
            Ok(inner.index(&tree)?.map(|i| i.built()))
        })
    }

    fn search<'a>(&'a self, tree: &'a str, query: Query) -> BoxFuture<'a, Result<Found, Refusal>> {
        let tree = tree.to_owned();
        self.run(move |inner| inner.search(&tree, &query))
    }

    fn forget<'a>(&'a self, tree: &'a str) -> BoxFuture<'a, Result<(), Refusal>> {
        let tree = tree.to_owned();
        self.run(move |inner| {
            seam::check_tree(&tree)?;
            inner.remove(&tree)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_seams::search::{SearchFixture, cases};

    struct Fixture {
        _dir: tempfile::TempDir,
        search: VorpalSearch,
    }

    const TREES: usize = 3;

    impl SearchFixture for Fixture {
        fn search(&self) -> &dyn Search {
            &self.search
        }
        fn trees_max(&self) -> usize {
            TREES
        }
    }

    fn fixture() -> Result<Fixture, String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let search = VorpalSearch::open(dir.path().join("search"), Limits { trees: TREES, threads: 2 })?;
        Ok(Fixture { _dir: dir, search })
    }

    /// W26-R07: vorpal passes the Search suite, each case on a fixture of its own.
    #[test]
    fn w26_r07_vorpal_passes_the_search_suite() -> Result<(), String> {
        let mut failed = Vec::new();
        let all = cases::<Fixture>();
        for case in &all {
            let f = fixture()?;
            if let Err(why) = futures::executor::block_on((case.check)(&f)) {
                failed.push(format!("{}: {why}", case.id));
            }
        }
        if failed.is_empty() { Ok(()) } else { Err(format!("{} of {} cases failed:\n{}", failed.len(), all.len(), failed.join("\n"))) }
    }

    /// W26-R07: trees built are kept across a restart, in the order they were searched.
    #[test]
    fn w26_r07_trees_outlive_a_restart() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let root = dir.path().join("search");
        let one = |s: &str| vec![File { path: "a.rs".into(), bytes: format!("fn {s}() {{}}\n").into_bytes() }];
        {
            let s = VorpalSearch::open(&root, Limits { trees: 2, threads: 1 })?;
            futures::executor::block_on(s.build("t1", one("one"))).map_err(|e| e.to_string())?;
            futures::executor::block_on(s.build("t2", one("two"))).map_err(|e| e.to_string())?;
        }
        let s = VorpalSearch::open(&root, Limits { trees: 2, threads: 1 })?;
        let found = futures::executor::block_on(s.search("t1", Query { mode: Mode::Text, q: "one".into(), lang: None, limit: 10 })).map_err(|e| e.to_string())?;
        assert_eq!(found.hits.first().and_then(|h| h.symbol.clone()), Some(Symbol { name: "one".into(), kind: "function".into() }));
        futures::executor::block_on(s.build("t3", one("three"))).map_err(|e| e.to_string())?;
        assert_eq!(futures::executor::block_on(s.built("t2")).map_err(|e| e.to_string())?, None, "t2 was searched least recently");
        assert!(futures::executor::block_on(s.built("t1")).map_err(|e| e.to_string())?.is_some());
        Ok(())
    }

    /// W26-R07: every language kiln takes has its outline rules.
    #[test]
    fn w26_r07_every_grammar_kiln_takes_reads_definitions() {
        let langs: Vec<String> = SupportLang::all_langs().iter().map(|l| language_name(*l)).collect();
        assert_eq!(langs.len(), 16, "{langs:?}");
        for lang in ["rust", "python", "typescript", "tsx", "javascript", "go", "java", "c", "cpp"] {
            let l = lang.parse::<SupportLang>().unwrap_or(SupportLang::Json);
            assert!(extractors().contains_key(&l), "{lang} has no outline rules");
        }
        let defs = definitions(SupportLang::TypeScript, "export function f(a: number) { return a }\nclass C { m() {} }\n");
        let got: Vec<(String, String)> = defs.into_iter().map(|d| (d.name, d.kind)).collect();
        assert!(got.contains(&("f".into(), "function".into())) && got.contains(&("m".into(), "method".into())), "{got:?}");
    }
}

//! The runner's local content store (docs/ci.md W24-R08), at `<work>/_cas`:
//!
//! - **Objects**, named by their content's SHA-256: whole files up to [`WHOLE_MAX`], larger ones
//!   as the store's content-defined chunks (`kiln_ci::store::cdc`, the cuts the server makes, kept
//!   as `kiln/cache@v1`'s local tier keeps them), so two versions of a large object share every
//!   chunk the change didn't touch. Every read is checked against the name; a copy that isn't
//!   what its name says is moved aside and reads as missing.
//! - **Entries**: a tool's name for an object (sccache's key, Bazel's action digest, a Turborepo
//!   hash) in a namespace, under a [`Scope`]: a repository and a trust domain (W24-R21). They're
//!   the work ledger's (`crate::ci::work`, in `ledger.db`): each is a unit of kind `action` whose
//!   definition is the tool's own key ([`entry_key`]), recorded and looked up on the ledger's one
//!   path, as every key is (W24-R30).
//! - **Holdings**: the objects each scope put, so an existence check answers for the asker's own
//!   scopes only. Bytes are kept once whoever sends them, but a repository never learns what
//!   another stored: it sends the bytes again (`ci/services/chunks.rs` refuses probing alike).
//!
//! Holdings live in memory and in an append-only journal (`index.log`), read back when the store
//! opens and rewritten then when it holds mostly lines since replaced.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead as _, Read as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use kiln_ci::store::cdc;
use kiln_ci::work::key::{self, Key, Kind, PathSet, Platform, Unit};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::ci::work::{Provenance, Recorded};
use crate::runner::cache::Local;

/// Objects up to this size are kept whole, larger ones in content-defined chunks: one no larger
/// than the store's largest chunk would be a chunk or two anyway.
pub const WHOLE_MAX: u64 = cdc::MAX as u64;

/// A repository and a trust domain (docs/ci.md W24-R21): what an entry belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Scope {
    pub repo: String,
    /// `trusted`, `protected`, `untrusted` or `person:<id>`.
    pub domain: String,
}

impl Scope {
    pub fn new(repo: &str, domain: &str) -> Scope {
        Scope { repo: repo.to_owned(), domain: domain.to_owned() }
    }

    /// As the work ledger keys its rows (`owner/repo|domain|`, `crate::ci::work::scope`).
    pub fn key(&self) -> String {
        crate::ci::work::scope(&self.repo, &self.domain)
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.repo, self.domain)
    }
}

/// What a name stands for in a scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub digest: String,
    pub size: u64,
    /// When it was put, in seconds since the epoch.
    pub at: i64,
    /// What the tool sent with it and wants back with it (Turborepo's `x-artifact-tag`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub meta: BTreeMap<String, String>,
}

/// How an object is kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kept {
    /// One file.
    Whole(PathBuf),
    /// Its chunks, in order: each one's digest and size.
    Chunks(Vec<(String, u64)>),
}

/// Whether `d` names an object: 64 lowercase hex digits.
pub fn is_digest(d: &str) -> bool {
    d.len() == 64 && d.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// One line of the journal.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
enum Line {
    /// An object a scope put.
    Hold { scope: Scope, digest: String },
}

#[derive(Debug, Default)]
struct Index {
    held: HashMap<Scope, HashSet<String>>,
}

impl Index {
    fn apply(&mut self, line: Line) {
        let Line::Hold { scope, digest } = line;
        self.held.entry(scope).or_default().insert(digest);
    }

    fn lines(&self) -> usize {
        self.held.values().map(HashSet::len).sum::<usize>()
    }
}

/// A tool's name for an object as a unit of work (W24-R30: every key on the ledger's one path):
/// an action whose definition is the tool's own key in its namespace, in the scope's domain.
pub fn entry_key(ns: &str, name: &str, domain: &str) -> Key {
    key::weak(&entry_unit(ns, name, domain))
}

/// The unit of `ns`/`name` in `domain` that [`entry_key`] keys: what the ledger looks up and
/// records (W24-R30).
pub fn entry_unit(ns: &str, name: &str, domain: &str) -> Unit {
    Unit {
        kind: Kind::Action,
        definition: format!("kiln/cas {ns}\n{name}"),
        secrets: Default::default(),
        env: Default::default(),
        platform: Platform::default(),
        inputs: Default::default(),
        upstream: Default::default(),
        trust: domain.to_owned(),
    }
}

/// An entry as the ledger records it: its object and size among the outputs, what the tool sent
/// with it as `meta:<header>`, when it was put as when it finished. A removed entry is recorded
/// with no object.
fn recorded(e: Option<&Entry>, domain: &str, at: i64) -> Recorded {
    let mut outputs = BTreeMap::new();
    if let Some(e) = e {
        outputs.insert("object".to_owned(), e.digest.clone());
        outputs.insert("size".to_owned(), e.size.to_string());
        for (k, v) in &e.meta {
            outputs.insert(format!("meta:{k}"), v.clone());
        }
    }
    let outcome = if e.is_some() { "success" } else { "removed" };
    Recorded { outcome: outcome.to_owned(), outputs, provenance: Provenance { finished_at: at, ..Provenance::default() }, domain: domain.to_owned(), ..Recorded::default() }
}

fn entry_of(r: &Recorded) -> Option<Entry> {
    let digest = r.outputs.get("object").filter(|d| is_digest(d))?.clone();
    let size = r.outputs.get("size").and_then(|s| s.parse().ok()).unwrap_or(0);
    let meta = r.outputs.iter().filter_map(|(k, v)| k.strip_prefix("meta:").map(|k| (k.to_owned(), v.clone()))).collect();
    Some(Entry { digest, size, at: r.provenance.finished_at, meta })
}

/// What `scope` records for `ns`/`name` on the ledger: an entry, a removal (`Some(None)`), or
/// nothing.
fn ledger_entry(c: &rusqlite::Connection, scope: &Scope, ns: &str, name: &str) -> anyhow::Result<Option<Option<Entry>>> {
    let same = |p: &PathSet| p.clone();
    Ok(crate::ci::work::lookup(c, &scope.key(), &entry_unit(ns, name, &scope.domain), &same)?.map(|h| entry_of(&h.recorded)))
}

/// The store.
pub struct Store {
    root: PathBuf,
    chunks: Local,
    index: RwLock<Index>,
    journal: Mutex<std::fs::File>,
    /// The work ledger the entries are on (`ledger.db`).
    ledger: crate::store::Store,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").field("root", &self.root).finish_non_exhaustive()
    }
}

impl Store {
    /// The store under `work` (`<work>/_cas`), its index read back.
    pub fn open(work: &Path) -> anyhow::Result<Store> {
        let root = work.join("_cas");
        for d in ["objects", "chunks", "tmp"] {
            std::fs::create_dir_all(root.join(d)).with_context(|| format!("create {}", root.join(d).display()))?;
        }
        let log = root.join("index.log");
        let mut index = Index::default();
        let mut read = 0usize;
        if let Ok(f) = std::fs::File::open(&log) {
            for line in std::io::BufReader::new(f).lines() {
                let Ok(line) = line else { break };
                // A line cut short by a crash is the last one: what came before stands.
                let Ok(parsed) = serde_json::from_str::<Line>(&line) else { continue };
                index.apply(parsed);
                read += 1;
            }
        }
        // Mostly lines since replaced: written again, one line for each thing that stands.
        if read > 1024 && read > index.lines() * 2 {
            let tmp = root.join("index.log.tmp");
            let mut out = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
            for (scope, set) in &index.held {
                for digest in set {
                    serde_json::to_writer(&mut out, &Line::Hold { scope: scope.clone(), digest: digest.clone() })?;
                    out.write_all(b"\n")?;
                }
            }
            out.flush()?;
            drop(out);
            std::fs::rename(&tmp, &log)?;
        }
        let journal = std::fs::OpenOptions::new().create(true).append(true).open(&log).with_context(|| format!("open {}", log.display()))?;
        let ledger = crate::store::Store::open(&root.join("ledger.db")).context("the runner's work ledger")?;
        Ok(Store { chunks: Local { dir: root.join("chunks"), max: u64::MAX }, root, index: RwLock::new(index), journal: Mutex::new(journal), ledger })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// This runner's work ledger (`crate::ci::work`'s tables, in `ledger.db`): the entries here,
    /// and `kiln run`'s results (`crate::reach::reuse`).
    pub fn ledger(&self) -> &crate::store::Store {
        &self.ledger
    }

    /// Where a temporary file is made: in the store, so it's renamed into place.
    pub fn temp(&self) -> PathBuf {
        self.root.join("tmp").join(crate::util::new_id())
    }

    fn object_path(&self, digest: &str) -> PathBuf {
        self.root.join("objects").join(digest.get(0..2).unwrap_or("00")).join(digest.get(2..4).unwrap_or("00")).join(digest)
    }

    fn list_path(&self, digest: &str) -> PathBuf {
        self.object_path(digest).with_extension("chunks")
    }

    /// How the object is kept here, when it is.
    pub fn kept(&self, digest: &str) -> Option<Kept> {
        if !is_digest(digest) {
            return None;
        }
        let whole = self.object_path(digest);
        if whole.is_file() {
            return Some(Kept::Whole(whole));
        }
        let list: Vec<(String, u64)> = serde_json::from_slice(&std::fs::read(self.list_path(digest)).ok()?).ok()?;
        Some(Kept::Chunks(list))
    }

    /// Whether the object is here.
    pub fn has(&self, digest: &str) -> bool {
        self.kept(digest).is_some()
    }

    fn write_whole(&self, digest: &str, data: &[u8]) -> anyhow::Result<()> {
        let at = self.object_path(digest);
        if at.is_file() {
            return Ok(());
        }
        if let Some(d) = at.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = self.temp();
        std::fs::write(&tmp, data).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &at).with_context(|| format!("place {}", at.display()))?;
        Ok(())
    }

    fn write_list(&self, digest: &str, list: &[(String, u64)]) -> anyhow::Result<()> {
        let at = self.list_path(digest);
        if let Some(d) = at.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = self.temp();
        std::fs::write(&tmp, serde_json::to_vec(list)?)?;
        std::fs::rename(&tmp, &at)?;
        Ok(())
    }

    /// Keeps `data`: its digest and size.
    pub fn put_bytes(&self, data: &[u8]) -> anyhow::Result<(String, u64)> {
        let digest = hex::encode(Sha256::digest(data));
        let size = data.len() as u64;
        if self.has(&digest) {
            return Ok((digest, size));
        }
        if size <= WHOLE_MAX {
            self.write_whole(&digest, data)?;
        } else {
            let mut list = Vec::new();
            let mut at = 0usize;
            for n in cdc::cuts(data) {
                let piece = data.get(at..at + n).unwrap_or_default();
                let d = hex::encode(Sha256::digest(piece));
                self.chunks.put(&d, piece)?;
                list.push((d, n as u64));
                at += n;
            }
            self.write_list(&digest, &list)?;
        }
        Ok((digest, size))
    }

    /// Keeps the file at `path` (left as it is): its digest and size. A large one is cut as it's
    /// read, never held whole in memory.
    pub fn put_file(&self, path: &Path) -> anyhow::Result<(String, u64)> {
        let len = std::fs::metadata(path).with_context(|| format!("read {}", path.display()))?.len();
        if len <= WHOLE_MAX {
            return self.put_bytes(&std::fs::read(path)?);
        }
        let (digest, size, list) = crate::runner::cache::chunks(path, Some(&self.chunks))?;
        if !self.has(&digest) {
            let pairs: Vec<(String, u64)> = list.into_iter().map(|c| (c.digest, c.size)).collect();
            self.write_list(&digest, &pairs)?;
        }
        Ok((digest, size))
    }

    /// A copy that isn't what its name says goes aside, where nothing reads it.
    fn set_aside(&self, path: &Path, digest: &str) {
        let aside = self.root.join("corrupt");
        let _ = std::fs::create_dir_all(&aside);
        if std::fs::rename(path, aside.join(digest)).is_err() {
            let _ = std::fs::remove_file(path);
        }
        tracing::warn!(target: "kiln.work", object = %digest, "a copy in the runner's content store wasn't what its name says: set aside");
    }

    /// The object's bytes, checked against its name; `None` when it isn't here (or wasn't what
    /// its name says, and has been set aside).
    pub fn read(&self, digest: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let Some(kept) = self.kept(digest) else { return Ok(None) };
        let data = match kept {
            Kept::Whole(path) => {
                let Ok(data) = std::fs::read(&path) else { return Ok(None) };
                if hex::encode(Sha256::digest(&data)) != digest {
                    self.set_aside(&path, digest);
                    return Ok(None);
                }
                return Ok(Some(data));
            }
            Kept::Chunks(list) => {
                let mut data = Vec::with_capacity(usize::try_from(list.iter().map(|(_, n)| n).sum::<u64>()).unwrap_or(0));
                for (d, n) in &list {
                    // A chunk is checked as it's read; one that's gone or wrong leaves the object
                    // incomplete, and so missing.
                    let Some(piece) = self.chunks.get(d).filter(|p| p.len() as u64 == *n) else { return Ok(None) };
                    data.extend_from_slice(&piece);
                }
                data
            }
        };
        if hex::encode(Sha256::digest(&data)) != digest {
            self.set_aside(&self.list_path(digest), digest);
            return Ok(None);
        }
        Ok(Some(data))
    }

    /// The object written to `out`, checked; `false` when it isn't here.
    pub fn copy_to(&self, digest: &str, out: &Path) -> anyhow::Result<bool> {
        match self.kept(digest) {
            None => Ok(false),
            Some(Kept::Whole(path)) => {
                let mut f = std::fs::File::open(&path)?;
                let mut o = std::fs::File::create(out)?;
                let mut h = Sha256::new();
                let mut buf = vec![0u8; 1 << 20];
                loop {
                    let n = f.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    let piece = buf.get(..n).unwrap_or_default();
                    h.update(piece);
                    o.write_all(piece)?;
                }
                if hex::encode(h.finalize()) != digest {
                    drop(o);
                    let _ = std::fs::remove_file(out);
                    self.set_aside(&path, digest);
                    return Ok(false);
                }
                Ok(true)
            }
            Some(Kept::Chunks(list)) => {
                let mut o = std::fs::File::create(out)?;
                let mut h = Sha256::new();
                for (d, n) in &list {
                    let Some(piece) = self.chunks.get(d).filter(|p| p.len() as u64 == *n) else {
                        drop(o);
                        let _ = std::fs::remove_file(out);
                        return Ok(false);
                    };
                    h.update(&piece);
                    o.write_all(&piece)?;
                }
                if hex::encode(h.finalize()) != digest {
                    drop(o);
                    let _ = std::fs::remove_file(out);
                    self.set_aside(&self.list_path(digest), digest);
                    return Ok(false);
                }
                Ok(true)
            }
        }
    }

    /// A chunk of a large object, checked (for a reader that streams one).
    pub fn chunk(&self, digest: &str) -> Option<Vec<u8>> {
        self.chunks.get(digest)
    }

    fn journal(&self, line: &Line) -> anyhow::Result<()> {
        let mut text = serde_json::to_vec(line)?;
        text.push(b'\n');
        self.journal.lock().write_all(&text)?;
        Ok(())
    }

    /// The first of `scopes` with an entry for `name` in `ns`, and the entry: looked up on the
    /// ledger (`crate::ci::work::lookup`). A scope that removed its entry passes to the next.
    pub async fn entry(&self, scopes: &[Scope], ns: &str, name: &str) -> anyhow::Result<Option<(Scope, Entry)>> {
        let (scopes, ns, name) = (scopes.to_vec(), ns.to_owned(), name.to_owned());
        self.ledger
            .call(move |c| {
                for s in scopes {
                    if let Some(Some(e)) = ledger_entry(c, &s, &ns, &name)? {
                        return Ok(Some((s, e)));
                    }
                }
                Ok(None)
            })
            .await
    }

    /// Sets `name` in `ns` of `scope` to `entry` (the object is held by the scope too), recorded
    /// on the ledger (`crate::ci::work::record`). Whether it changed anything.
    pub async fn set_entry(&self, scope: &Scope, ns: &str, name: &str, entry: Entry) -> anyhow::Result<bool> {
        self.hold(scope, &entry.digest)?;
        let (s, ns, name) = (scope.clone(), ns.to_owned(), name.to_owned());
        self.ledger
            .call(move |c| {
                if let Some(Some(e)) = ledger_entry(c, &s, &ns, &name)?
                    && e.digest == entry.digest
                    && e.meta == entry.meta
                {
                    return Ok(false);
                }
                crate::ci::work::record(c, &s.key(), &entry_unit(&ns, &name, &s.domain), &PathSet::new(), &recorded(Some(&entry), &s.domain, entry.at))?;
                Ok(true)
            })
            .await
    }

    /// Removes `name` from `ns` of `scope` (recorded as removed: the ledger keeps no gap in a
    /// key's history); whether it was there.
    pub async fn remove_entry(&self, scope: &Scope, ns: &str, name: &str) -> anyhow::Result<bool> {
        let (s, ns, name) = (scope.clone(), ns.to_owned(), name.to_owned());
        self.ledger
            .call(move |c| {
                if !matches!(ledger_entry(c, &s, &ns, &name)?, Some(Some(_))) {
                    return Ok(false);
                }
                crate::ci::work::record(c, &s.key(), &entry_unit(&ns, &name, &s.domain), &PathSet::new(), &recorded(None, &s.domain, crate::util::now_unix()))?;
                Ok(true)
            })
            .await
    }

    /// The first of `scopes` that holds the object, if it's here.
    pub fn holder(&self, scopes: &[Scope], digest: &str) -> Option<Scope> {
        let found = {
            let index = self.index.read();
            scopes.iter().find(|s| index.held.get(*s).is_some_and(|h| h.contains(digest))).cloned()
        };
        found.filter(|_| self.has(digest))
    }

    /// Records that `scope` holds the object; whether it's new to it.
    pub fn hold(&self, scope: &Scope, digest: &str) -> anyhow::Result<bool> {
        if self.index.read().held.get(scope).is_some_and(|h| h.contains(digest)) {
            return Ok(false);
        }
        self.journal(&Line::Hold { scope: scope.clone(), digest: digest.to_owned() })?;
        self.index.write().held.entry(scope.clone()).or_default().insert(digest.to_owned());
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(n: usize, seed: u64) -> Vec<u8> {
        let mut s = seed.max(1);
        let mut out = Vec::with_capacity(n + 8);
        while out.len() < n {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            out.extend_from_slice(&s.wrapping_mul(0x2545_f491_4f6c_dd1d).to_le_bytes());
        }
        out.truncate(n);
        out
    }

    /// W24-R08: small objects are kept whole and large ones in the store's chunks; either reads
    /// back as it went in, and two versions of a large object share their unchanged chunks.
    #[test]
    fn w24_r08_objects_whole_and_chunked() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path())?;
        let small = noise(300 << 10, 1);
        let (d, n) = store.put_bytes(&small)?;
        assert_eq!((d.clone(), n), (crate::util::sha256_hex(&small), small.len() as u64));
        assert!(matches!(store.kept(&d), Some(Kept::Whole(_))));
        assert_eq!(store.read(&d)?.as_deref(), Some(small.as_slice()));
        // Large: cut into chunks as the server cuts them, from a file, never whole in memory.
        let big = noise(20 << 20, 2);
        let f = dir.path().join("big");
        std::fs::write(&f, &big)?;
        let (bd, bn) = store.put_file(&f)?;
        assert_eq!((bd.clone(), bn), (crate::util::sha256_hex(&big), big.len() as u64));
        let Some(Kept::Chunks(list)) = store.kept(&bd) else { anyhow::bail!("a large object is kept in chunks") };
        assert_eq!(list.iter().map(|(_, n)| *n as usize).collect::<Vec<_>>(), cdc::cuts(&big));
        assert!(store.read(&bd)?.as_deref() == Some(big.as_slice()));
        let out = dir.path().join("out");
        assert!(store.copy_to(&bd, &out)?);
        assert!(std::fs::read(&out)? == big);
        // A second version, one edit in the middle: most chunks are shared.
        let mut edited = big.clone();
        edited.splice(10 << 20..10 << 20, b"an edit".iter().copied());
        let (ed, _) = store.put_bytes(&edited)?;
        let Some(Kept::Chunks(list2)) = store.kept(&ed) else { anyhow::bail!("chunked") };
        let shared = list2.iter().filter(|c| list.contains(c)).count();
        assert!(shared + 3 >= list.len(), "{shared} of {} chunks shared", list.len());
        // Bytes put again are the same object.
        assert_eq!(store.put_bytes(&small)?.0, d);
        assert!(store.read(&"0".repeat(64))?.is_none() && !store.has("not a digest"));
        Ok(())
    }

    /// W24-R08: a copy that isn't what its name says is never served: it's set aside and reads as
    /// missing, so the next put keeps it again.
    #[test]
    fn w24_r08_a_corrupted_copy_is_refused() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path())?;
        let data = noise(64 << 10, 3);
        let (d, _) = store.put_bytes(&data)?;
        let Some(Kept::Whole(path)) = store.kept(&d) else { anyhow::bail!("whole") };
        std::fs::write(&path, b"poison")?;
        assert!(store.read(&d)?.is_none());
        assert!(!store.has(&d) && dir.path().join("_cas/corrupt").join(&d).exists());
        store.put_bytes(&data)?;
        assert_eq!(store.read(&d)?.as_deref(), Some(data.as_slice()));
        Ok(())
    }

    /// W24-R08, R30: entries are per scope, on the work ledger, and survive the store opening
    /// again; so do holdings. A removed entry lets the next scope's show.
    #[test]
    fn w24_r08_entries_and_holdings_are_per_scope_and_kept() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        crate::rt::block_on(async move {
            let (a, b, t) = (Scope::new("acme/app", "untrusted"), Scope::new("acme/other", "trusted"), Scope::new("acme/app", "trusted"));
            let digest = {
                let store = Store::open(&root)?;
                let (d, n) = store.put_bytes(b"compiled")?;
                let e = Entry { digest: d.clone(), size: n, at: 1, meta: BTreeMap::from([("x-artifact-tag".to_owned(), "sig".to_owned())]) };
                anyhow::ensure!(store.set_entry(&a, "sccache", "k1", e.clone()).await?);
                anyhow::ensure!(!store.set_entry(&a, "sccache", "k1", e.clone()).await?, "the same again changes nothing");
                anyhow::ensure!(store.entry(std::slice::from_ref(&a), "sccache", "k1").await?.map(|(_, e)| e.digest) == Some(d.clone()));
                anyhow::ensure!(store.entry(std::slice::from_ref(&b), "sccache", "k1").await?.is_none());
                anyhow::ensure!(store.entry(std::slice::from_ref(&t), "sccache", "k1").await?.is_none(), "another domain's ledger holds nothing of it");
                anyhow::ensure!(store.holder(&[b.clone(), a.clone()], &d) == Some(a.clone()));
                anyhow::ensure!(store.holder(std::slice::from_ref(&b), &d).is_none(), "another repository's object isn't its to know of");
                // Removed from its own scope, the trusted entry under it shows.
                let (td, tn) = store.put_bytes(b"trusted build")?;
                store.set_entry(&t, "sccache", "gone", Entry { digest: td.clone(), size: tn, at: 2, meta: BTreeMap::new() }).await?;
                store.set_entry(&a, "sccache", "gone", e).await?;
                anyhow::ensure!(store.entry(&[a.clone(), t.clone()], "sccache", "gone").await?.map(|(s, _)| s) == Some(a.clone()));
                anyhow::ensure!(store.remove_entry(&a, "sccache", "gone").await? && !store.remove_entry(&a, "sccache", "gone").await?);
                anyhow::ensure!(store.entry(&[a.clone(), t.clone()], "sccache", "gone").await?.map(|(s, e)| (s, e.digest)) == Some((t.clone(), td)));
                // On the ledger: a unit whose definition is the tool's key.
                let same = |p: &PathSet| p.clone();
                let (sk, wk) = (a.key(), entry_unit("sccache", "k1", "untrusted"));
                anyhow::ensure!(store.ledger().call(move |c| crate::ci::work::lookup(c, &sk, &wk, &same)).await?.is_some(), "recorded on the ledger");
                d
            };
            let store = Store::open(&root)?;
            let (scope, e) = store.entry(&[b.clone(), a.clone()], "sccache", "k1").await?.ok_or_else(|| anyhow::anyhow!("kept across opening"))?;
            anyhow::ensure!((scope, e.digest.as_str(), e.meta.get("x-artifact-tag").map(String::as_str), e.at) == (a.clone(), digest.as_str(), Some("sig"), 1));
            anyhow::ensure!(store.entry(std::slice::from_ref(&a), "sccache", "gone").await?.is_none());
            anyhow::ensure!(store.holder(std::slice::from_ref(&a), &digest) == Some(a));
            anyhow::ensure!(entry_key("sccache", "k1", "trusted") != entry_key("sccache", "k1", "untrusted") && entry_key("go", "k1", "trusted") != entry_key("sccache", "k1", "trusted"));
            Ok(())
        })
    }
}

//! The work ledger behind one interface (docs/ci.md W24-R02): its rows in kiln's `work_records`
//! table, or in the `Records` seam (kiln's own durable Records in its store, the simulator,
//! Amphora once CP-T1.5 lands). The ledger's one path (`ci::work`: keying, recording, lookups,
//! e-classes, trust, signatures, verification, test results, the reaper; W24-R30) is written once
//! over [`Rows`], so it decides alike on either.
//!
//! - **The table.** A connection to kiln's store, or a transaction on one, is [`Rows`] as it is:
//!   each read and write made as it's asked, a transaction a savepoint ([`crate::store::tx`]), the
//!   audit log's rows in `audit2` beside the ledger's.
//! - **Records** ([`OnRecords`]): each read made as it's asked (linearizable), a write alone a
//!   commit of its own, and a transaction's writes ([`Rows::atomically`]) kept until it ends, its
//!   reads seeing them, then committed as one. An insert-only write ([`Rows::put_new`]) is a
//!   condition there that its key holds nothing, so two writers racing for a key leave one entry
//!   (W24-R22). Its audit rows go to the audit log of the store it's given, once the commit holds.
//!   It runs the seam's futures to their end, so the ledger runs on it off the runtime's threads
//!   and off the blocking pool's too, whose few places the seam's own calls need (kiln's Records
//!   runs in its store, through the pool): on a thread of its own, as [`Ledger`] runs it, or a
//!   test's.
//! - **A row** is a family, a key, a value and a stamp (`at`, which orders a weak key's path sets,
//!   newest highest). The table keeps the stamp in a column of its own. Records has none, so there a
//!   row's value is its stamp (8 bytes, big-endian) followed by the table's value: a row moves from
//!   one to the other as it is.
//! - **Bounds.** Records takes keys to 1024 bytes, values to 1 MiB (the stamp's 8 bytes included)
//!   and 1000 writes a commit; the table takes any. What Records can't hold it refuses, typed, and
//!   nothing of the transaction is written: a unit's path set past a MiB is the one row that may
//!   reach it.
//! - **Bytes** stay in the content store (`cas/<sha256>`, through `Blobs`): [`blob_keys`] are the
//!   keys a result names there, and [`unreachable`] the ones the store doesn't hold whole.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use kiln_seams::blobs::{self, Blobs};
use kiln_seams::records::{self, Cond, Consistency, Records, Scan, Tx, Write};
use rusqlite::{Connection, params};

use super::{Family, Recorded};
use crate::store::Store;

/// A row of the audit log (W19-R05) the ledger writes: what it refused or passed over as a breach of
/// the trust rules, or did on its own (W24-R21, R23, AC06). It's no request's, so it's a row of its
/// own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Audit {
    /// Whose doing it was: a runner's name, or `kiln`.
    pub principal: String,
    pub action: String,
    pub target: String,
    /// How it ended: `refused`, `quarantined`.
    pub result: String,
    pub detail: String,
}

/// Where the ledger keeps its rows (W24-R02): what each implementation does.
pub trait Rows {
    /// A row's value.
    fn get(&self, family: Family, key: &str) -> anyhow::Result<Option<Vec<u8>>>;
    /// The rows whose keys start with `prefix`, as key, value and stamp: highest stamp first, then
    /// by key.
    fn scan(&self, family: Family, prefix: &str) -> anyhow::Result<Vec<(String, Vec<u8>, i64)>>;
    /// A row, written or replaced.
    fn put(&self, family: Family, key: &str, value: &[u8], at: i64) -> anyhow::Result<()>;
    /// A row written only where its key holds none, and whether it was (W24-R22: an entry is never
    /// overwritten): refused however the writers race.
    fn put_new(&self, family: Family, key: &str, value: &[u8], at: i64) -> anyhow::Result<bool>;
    /// A row gone.
    fn delete(&self, family: Family, key: &str) -> anyhow::Result<()>;
    /// A row of the audit log.
    fn audit(&self, row: Audit) -> anyhow::Result<()>;
    /// Runs `f` so that what it writes is made together, or none of it when it fails.
    fn atomically(&self, f: &mut dyn FnMut(&dyn Rows) -> anyhow::Result<()>) -> anyhow::Result<()>;
}

// ------------------------------------------------------------------------------------- the table

/// The audit log's row, on `c`.
fn audit_on(c: &Connection, a: &Audit) -> anyhow::Result<()> {
    let principal = if a.principal.is_empty() { "kiln" } else { a.principal.as_str() };
    c.execute(
        "INSERT INTO audit2 (ts, principal, action, target, result, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![crate::util::now_unix(), principal, a.action, a.target, a.result, a.detail],
    )?;
    Ok(())
}

/// The ledger in kiln's `work_records` table (migration `20261025-0215_w24_work.sql`): a
/// connection to kiln's store, each read and write made on it as it's asked.
impl Rows for Connection {
    fn get(&self, family: Family, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(super::get(self, family, key)?)
    }

    fn scan(&self, family: Family, prefix: &str) -> anyhow::Result<Vec<(String, Vec<u8>, i64)>> {
        Ok(super::scan(self, family, prefix)?)
    }

    fn put(&self, family: Family, key: &str, value: &[u8], at: i64) -> anyhow::Result<()> {
        Ok(super::put(self, family, key, value, at)?)
    }

    fn put_new(&self, family: Family, key: &str, value: &[u8], at: i64) -> anyhow::Result<bool> {
        Ok(super::put_new(self, family, key, value, at)?)
    }

    fn delete(&self, family: Family, key: &str) -> anyhow::Result<()> {
        Ok(super::delete(self, family, key)?)
    }

    fn audit(&self, row: Audit) -> anyhow::Result<()> {
        audit_on(self, &row)
    }

    fn atomically(&self, f: &mut dyn FnMut(&dyn Rows) -> anyhow::Result<()>) -> anyhow::Result<()> {
        let tx = crate::store::tx(self)?;
        f(&tx)?;
        tx.commit()?;
        Ok(())
    }
}

/// A transaction on a connection to kiln's store is the table's [`Rows`] as its connection is.
macro_rules! rows_of_the_connection {
    ($($t:ty),*) => {$(
        impl Rows for $t {
            fn get(&self, family: Family, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
                Rows::get(&**self, family, key)
            }

            fn scan(&self, family: Family, prefix: &str) -> anyhow::Result<Vec<(String, Vec<u8>, i64)>> {
                Rows::scan(&**self, family, prefix)
            }

            fn put(&self, family: Family, key: &str, value: &[u8], at: i64) -> anyhow::Result<()> {
                Rows::put(&**self, family, key, value, at)
            }

            fn put_new(&self, family: Family, key: &str, value: &[u8], at: i64) -> anyhow::Result<bool> {
                Rows::put_new(&**self, family, key, value, at)
            }

            fn delete(&self, family: Family, key: &str) -> anyhow::Result<()> {
                Rows::delete(&**self, family, key)
            }

            fn audit(&self, row: Audit) -> anyhow::Result<()> {
                Rows::audit(&**self, row)
            }

            fn atomically(&self, f: &mut dyn FnMut(&dyn Rows) -> anyhow::Result<()>) -> anyhow::Result<()> {
                Rows::atomically(&**self, f)
            }
        }
    )*};
}

rows_of_the_connection!(crate::store::Tx<'_>, rusqlite::Transaction<'_>);

// ----------------------------------------------------------------------------------- on Records

/// The ledger in Records (docs/seams.md, "Records"): its families as they are named
/// (`work.result`…), each key as it is, each value its stamp and then the table's value. Its audit
/// rows go to `audit`'s audit log.
#[derive(Clone)]
pub struct OnRecords {
    records: Arc<dyn Records>,
    audit: Store,
}

/// The stamp's bytes, before a row's value in Records.
const STAMP: usize = 8;

/// A row's value as Records keeps it: its stamp, then the value.
pub fn stamped(at: i64, value: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(STAMP.saturating_add(value.len()));
    v.extend_from_slice(&at.to_be_bytes());
    v.extend_from_slice(value);
    v
}

/// A row's stamp and value, from what Records keeps.
fn unstamped(family: &str, key: &[u8], kept: &[u8]) -> anyhow::Result<(i64, Vec<u8>)> {
    let (at, value) = kept.split_first_chunk::<STAMP>().ok_or_else(|| anyhow::anyhow!("{family} {:?}: {} bytes, short of a row's stamp", String::from_utf8_lossy(key), kept.len()))?;
    Ok((i64::from_be_bytes(*at), value.to_vec()))
}

fn refused(e: records::Refusal) -> anyhow::Error {
    anyhow::Error::new(e).context("the work ledger's Records")
}

/// A write of a family's row, as Records takes it.
fn write_of(family: &str, key: &str, row: Option<(Vec<u8>, i64)>) -> Write {
    match row {
        Some((value, at)) => Write::Put { family: family.to_owned(), key: key.as_bytes().to_vec(), value: stamped(at, &value) },
        None => Write::Delete { family: family.to_owned(), key: key.as_bytes().to_vec() },
    }
}

/// Rows as the table answers a scan: highest stamp first, then by key.
fn table_order(rows: &mut [(String, Vec<u8>, i64)]) {
    rows.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
}

impl OnRecords {
    /// The ledger in `records`, its audit rows in `audit`'s audit log.
    pub fn new(records: Arc<dyn Records>, audit: Store) -> OnRecords {
        OnRecords { records, audit }
    }

    /// Commits `writes`, holding `absent`'s keys empty until then; nothing at all when there are none.
    fn commit(&self, absent: Vec<(&'static str, String)>, writes: Vec<Write>) -> anyhow::Result<()> {
        if writes.is_empty() {
            return Ok(());
        }
        let conds = absent.into_iter().map(|(family, key)| Cond::Absent { family: family.to_owned(), key: key.into_bytes() }).collect();
        let r = Arc::clone(&self.records);
        crate::rt::block_on(async move { r.commit(Tx { conds, writes }).await.map(|_| ()).map_err(refused) })
    }
}

impl Rows for OnRecords {
    fn get(&self, family: Family, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let (r, name, k) = (Arc::clone(&self.records), family.name(), key.as_bytes().to_vec());
        let row = crate::rt::block_on(async move { r.get(name, &k, Consistency::Linearizable).await.map_err(refused) })?;
        row.map(|r| unstamped(name, &r.key, &r.value).map(|(_, v)| v)).transpose()
    }

    fn scan(&self, family: Family, prefix: &str) -> anyhow::Result<Vec<(String, Vec<u8>, i64)>> {
        let (r, name, p) = (Arc::clone(&self.records), family.name(), prefix.as_bytes().to_vec());
        let kept = crate::rt::block_on(async move {
            let mut rows = Vec::new();
            let mut after: Option<Vec<u8>> = None;
            loop {
                let page = r.scan(name, Scan { prefix: p.clone(), after: after.take(), limit: records::SCAN_MAX, at: Consistency::Linearizable }).await.map_err(refused)?;
                after = page.rows.last().map(|row| row.key.clone());
                rows.extend(page.rows);
                if !page.more || after.is_none() {
                    break;
                }
            }
            Ok(rows)
        })?;
        let mut out = Vec::with_capacity(kept.len());
        for row in kept {
            let (at, value) = unstamped(name, &row.key, &row.value)?;
            let key = String::from_utf8(row.key).map_err(|e| anyhow::anyhow!("{name}: a key that isn't UTF-8 ({e})"))?;
            out.push((key, value, at));
        }
        table_order(&mut out);
        Ok(out)
    }

    fn put(&self, family: Family, key: &str, value: &[u8], at: i64) -> anyhow::Result<()> {
        self.commit(Vec::new(), vec![write_of(family.name(), key, Some((value.to_vec(), at)))])
    }

    fn put_new(&self, family: Family, key: &str, value: &[u8], at: i64) -> anyhow::Result<bool> {
        match self.commit(vec![(family.name(), key.to_owned())], vec![write_of(family.name(), key, Some((value.to_vec(), at)))]) {
            Ok(()) => Ok(true),
            Err(e) if matches!(e.downcast_ref::<records::Refusal>(), Some(records::Refusal::Conflict { .. })) => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn delete(&self, family: Family, key: &str) -> anyhow::Result<()> {
        self.commit(Vec::new(), vec![write_of(family.name(), key, None)])
    }

    fn audit(&self, row: Audit) -> anyhow::Result<()> {
        let store = self.audit.clone();
        crate::rt::block_on(async move { store.call(move |c| audit_on(c, &row)).await })
    }

    fn atomically(&self, f: &mut dyn FnMut(&dyn Rows) -> anyhow::Result<()>) -> anyhow::Result<()> {
        let pending = Pending { base: self, writes: RefCell::default(), absent: RefCell::default(), audits: RefCell::default() };
        f(&pending)?;
        let Pending { writes, absent, audits, .. } = pending;
        let writes = writes.into_inner().into_iter().map(|((family, key), row)| write_of(family, &key, row)).collect();
        self.commit(absent.into_inner().into_iter().collect(), writes)?;
        for a in audits.into_inner() {
            self.audit(a)?;
        }
        Ok(())
    }
}

/// A transaction on Records, under way: its writes and audit rows kept until it ends, its reads
/// seeing its writes, its insert-only writes conditions that their keys hold nothing.
struct Pending<'a> {
    base: &'a OnRecords,
    /// Each row written, by family and key: its value and stamp, or `None` where it's deleted.
    writes: RefCell<BTreeMap<(&'static str, String), Option<(Vec<u8>, i64)>>>,
    absent: RefCell<BTreeSet<(&'static str, String)>>,
    audits: RefCell<Vec<Audit>>,
}

impl Rows for Pending<'_> {
    fn get(&self, family: Family, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
        if let Some(row) = self.writes.borrow().get(&(family.name(), key.to_owned())) {
            return Ok(row.as_ref().map(|(v, _)| v.clone()));
        }
        self.base.get(family, key)
    }

    fn scan(&self, family: Family, prefix: &str) -> anyhow::Result<Vec<(String, Vec<u8>, i64)>> {
        let mut rows: BTreeMap<String, (Vec<u8>, i64)> = self.base.scan(family, prefix)?.into_iter().map(|(k, v, at)| (k, (v, at))).collect();
        for ((f, k), row) in self.writes.borrow().iter() {
            if *f != family.name() || !k.starts_with(prefix) {
                continue;
            }
            match row {
                Some((v, at)) => rows.insert(k.clone(), (v.clone(), *at)),
                None => rows.remove(k),
            };
        }
        let mut out: Vec<(String, Vec<u8>, i64)> = rows.into_iter().map(|(k, (v, at))| (k, v, at)).collect();
        table_order(&mut out);
        Ok(out)
    }

    fn put(&self, family: Family, key: &str, value: &[u8], at: i64) -> anyhow::Result<()> {
        self.writes.borrow_mut().insert((family.name(), key.to_owned()), Some((value.to_vec(), at)));
        Ok(())
    }

    fn put_new(&self, family: Family, key: &str, value: &[u8], at: i64) -> anyhow::Result<bool> {
        if self.get(family, key)?.is_some() {
            return Ok(false);
        }
        let k = (family.name(), key.to_owned());
        // Held by nothing in Records when it was read: it must still be when this commits.
        if !self.writes.borrow().contains_key(&k) {
            self.absent.borrow_mut().insert(k.clone());
        }
        self.writes.borrow_mut().insert(k, Some((value.to_vec(), at)));
        Ok(true)
    }

    fn delete(&self, family: Family, key: &str) -> anyhow::Result<()> {
        self.writes.borrow_mut().insert((family.name(), key.to_owned()), None);
        Ok(())
    }

    fn audit(&self, row: Audit) -> anyhow::Result<()> {
        self.audits.borrow_mut().push(row);
        Ok(())
    }

    /// Part of the transaction under way.
    fn atomically(&self, f: &mut dyn FnMut(&dyn Rows) -> anyhow::Result<()>) -> anyhow::Result<()> {
        f(self)
    }
}

// ------------------------------------------------------------------------ for callers that wait

/// The ledger for callers that wait (a service, such as the Remote Execution API's action cache):
/// in the `work_records` table, through the store's calls, or in Records (W24-R02).
#[derive(Clone)]
pub enum Ledger {
    /// The `work_records` table in kiln's store.
    Table(Store),
    /// Records (kiln's own, `hub.records`, or Amphora's), its audit rows in `audit`'s audit log.
    Records { records: Arc<dyn Records>, audit: Store },
}

impl Ledger {
    /// Runs `f`, the ledger's one path (`ci::work`), on its rows: on the table on the store's
    /// thread, on Records on a thread of its own (not one of the blocking pool's places, which the
    /// seam's calls it waits on need).
    pub async fn run<T, F>(&self, f: F) -> anyhow::Result<T>
    where
        F: FnOnce(&dyn Rows) -> anyhow::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        match self {
            Ledger::Table(store) => store.call(move |c| f(c)).await,
            Ledger::Records { records, audit } => {
                let on = OnRecords::new(Arc::clone(records), audit.clone());
                let (tx, rx) = futures::channel::oneshot::channel();
                std::thread::Builder::new().name("kiln-ledger".into()).spawn(move || {
                    let _ = tx.send(f(&on));
                })?;
                rx.await.map_err(|_| anyhow::anyhow!("the work ledger's thread ended without an answer"))?
            }
        }
    }
}

// ----------------------------------------------------------------------------------------- bytes

/// The content digest a value of the ledger names, when it names one: 64 lowercase hex digits,
/// bare or as `sha256:<hex>`. A result's log and test reports are digests; an output is a digest
/// (a file or tree in the content store) or a value (W24-R02).
pub fn digest_of(value: &str) -> Option<&str> {
    let hex = value.strip_prefix("sha256:").unwrap_or(value);
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))).then_some(hex)
}

/// The keys of what a result keeps in the content store (W24-R02): its log, its test reports and
/// those of its outputs that are digests, each at `cas/<sha256>`.
pub fn blob_keys(r: &Recorded) -> BTreeSet<String> {
    r.logs.iter().chain(&r.reports).chain(r.outputs.values()).filter_map(|v| digest_of(v)).map(crate::ci::services::cas::key).collect()
}

/// Those of [`blob_keys`] the store doesn't hold whole: no object there, or one whose content
/// isn't the digest (its etag is its content's SHA-256). None when every byte the result names is
/// reachable through `Blobs`.
pub async fn unreachable(blobs: &dyn Blobs, r: &Recorded) -> Result<Vec<String>, blobs::Refusal> {
    let mut out = Vec::new();
    for key in blob_keys(r) {
        match blobs.head(&key).await {
            Ok(m) if key.strip_prefix("cas/") == Some(m.etag.as_str()) => {}
            Ok(_) | Err(blobs::Refusal::NotFound(_)) => out.push(key),
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests;

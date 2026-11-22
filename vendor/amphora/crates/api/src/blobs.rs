//! The bytes Amphora keeps outside itself: its backups (CP-T1.12, docs/design/backup.md). Mantle
//! (kiln's track T10) keeps them; Amphora reaches it only through this trait (CLAUDE.md §9), which
//! kiln implements over its `kiln_seams::blobs::Blobs` seam (S3's semantics, Mantle behind it). It
//! asks for the least a backup needs: an object written once and never changed, read whole, listed
//! by prefix in key order, and deleted. [`SimBlobs`] is its simulator; [`conformance`] is what every
//! implementation passes.
//!
//! Objects are written once: [`Blobs::create`] refuses a key that holds an object
//! ([`BlobRefusal::Exists`]), so a writer that lost the answer to a create can read the object back
//! to learn whether it landed, and never overwrites what another writer made.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Instant;

/// The bytes of a key at most: S3's limit on an object key ("Naming Amazon S3 objects": "The name
/// for a key is a sequence of Unicode characters whose UTF-8 encoding is at most 1,024 bytes long").
pub const BLOB_KEY_MAX: usize = 1024;

/// What an object store refused, and why.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlobRefusal {
    #[error("an object already exists at {0}")]
    Exists(String),
    #[error("no object at {0}")]
    NotFound(String),
    #[error("{what} has size {size}, over {limit}")]
    TooLarge {
        what: String,
        size: usize,
        limit: usize,
    },
    #[error("the store holds its most ({limit} {what})")]
    Full { what: String, limit: usize },
    #[error("invalid {what}: {why}")]
    Invalid { what: String, why: String },
    #[error("the deadline elapsed; a create may still land")]
    Timeout,
    #[error("the object store is unavailable: {0}")]
    Unavailable(String),
}

/// One page of a listing: keys in byte order, each with its object's size.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listing {
    pub objects: Vec<(String, u64)>,
    /// More keys follow the last one listed.
    pub truncated: bool,
}

/// An object store, as a backup uses one.
pub trait Blobs: Send + Sync {
    /// The bytes an object may hold at most in this store.
    fn object_max(&self) -> usize;
    /// Writes `bytes` at `key` if no object is there; refused [`BlobRefusal::Exists`] if one is,
    /// which it leaves as it was.
    fn create(&self, key: &str, bytes: &[u8], until: Instant) -> Result<(), BlobRefusal>;
    /// The object at `key`, whole.
    fn get(&self, key: &str, until: Instant) -> Result<Vec<u8>, BlobRefusal>;
    /// The keys under `prefix` after `after` (none: from the first), in byte order, `limit` at
    /// most (a limit of 0 is refused).
    fn list(
        &self,
        prefix: &str,
        after: Option<&str>,
        limit: usize,
        until: Instant,
    ) -> Result<Listing, BlobRefusal>;
    /// Removes the object at `key`; a key with no object is no error.
    fn delete(&self, key: &str, until: Instant) -> Result<(), BlobRefusal>;
}

fn invalid(what: &str, why: &str) -> BlobRefusal {
    BlobRefusal::Invalid {
        what: what.into(),
        why: why.into(),
    }
}

/// A key's shape, as every implementation checks it: 1 to [`BLOB_KEY_MAX`] bytes, no control
/// characters or backslashes, no empty segment, no '/' at either end (kiln's BLOBS-21 to BLOBS-23).
pub fn check_key(key: &str) -> Result<(), BlobRefusal> {
    if key.is_empty() {
        return Err(invalid("key", "empty"));
    }
    if key.len() > BLOB_KEY_MAX {
        return Err(BlobRefusal::TooLarge {
            what: "key".into(),
            size: key.len(),
            limit: BLOB_KEY_MAX,
        });
    }
    if key.chars().any(|c| c.is_control() || c == '\\') {
        return Err(invalid("key", "a control character or a backslash"));
    }
    if key.starts_with('/') || key.ends_with('/') || key.contains("//") {
        return Err(invalid("key", "an empty segment"));
    }
    Ok(())
}

/// The simulator: objects in memory, bounded in count and in bytes, each refusal typed.
pub struct SimBlobs {
    objects: Mutex<BTreeMap<String, Vec<u8>>>,
    object_max: usize,
    count_max: usize,
    bytes_max: usize,
}

impl SimBlobs {
    /// A store of `count_max` objects and `bytes_max` bytes at most, each object `object_max` bytes
    /// at most.
    #[must_use]
    pub fn new(object_max: usize, count_max: usize, bytes_max: usize) -> Self {
        Self {
            objects: Mutex::new(BTreeMap::new()),
            object_max,
            count_max,
            bytes_max,
        }
    }

    fn objects(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, Vec<u8>>>, BlobRefusal> {
        self.objects
            .lock()
            .map_err(|_| BlobRefusal::Unavailable("the simulator's lock was poisoned".into()))
    }

    /// The bytes it holds, all objects together.
    pub fn bytes(&self) -> Result<usize, BlobRefusal> {
        let objects = self.objects()?;
        held(&objects)
    }
}

fn held(objects: &BTreeMap<String, Vec<u8>>) -> Result<usize, BlobRefusal> {
    let mut sum = 0usize;
    for bytes in objects.values() {
        sum = sum
            .checked_add(bytes.len())
            .ok_or_else(|| invalid("store", "size overflow"))?;
    }
    Ok(sum)
}

fn expired(until: Instant) -> Result<(), BlobRefusal> {
    if Instant::now() >= until {
        Err(BlobRefusal::Timeout)
    } else {
        Ok(())
    }
}

impl Blobs for SimBlobs {
    fn object_max(&self) -> usize {
        self.object_max
    }

    fn create(&self, key: &str, bytes: &[u8], until: Instant) -> Result<(), BlobRefusal> {
        expired(until)?;
        check_key(key)?;
        if bytes.len() > self.object_max {
            return Err(BlobRefusal::TooLarge {
                what: "object".into(),
                size: bytes.len(),
                limit: self.object_max,
            });
        }
        let mut objects = self.objects()?;
        let held = held(&objects)?;
        if objects.contains_key(key) {
            return Err(BlobRefusal::Exists(key.into()));
        }
        if objects.len() >= self.count_max {
            return Err(BlobRefusal::Full {
                what: "objects".into(),
                limit: self.count_max,
            });
        }
        if held
            .checked_add(bytes.len())
            .is_none_or(|n| n > self.bytes_max)
        {
            return Err(BlobRefusal::Full {
                what: "bytes".into(),
                limit: self.bytes_max,
            });
        }
        objects.insert(key.into(), bytes.to_vec());
        Ok(())
    }

    fn get(&self, key: &str, until: Instant) -> Result<Vec<u8>, BlobRefusal> {
        expired(until)?;
        check_key(key)?;
        self.objects()?
            .get(key)
            .cloned()
            .ok_or_else(|| BlobRefusal::NotFound(key.into()))
    }

    fn list(
        &self,
        prefix: &str,
        after: Option<&str>,
        limit: usize,
        until: Instant,
    ) -> Result<Listing, BlobRefusal> {
        expired(until)?;
        if limit == 0 {
            return Err(invalid("limit", "zero"));
        }
        let objects = self.objects()?;
        let mut listing = Listing::default();
        let from = match after {
            Some(after) if after >= prefix => std::ops::Bound::Excluded(after.to_string()),
            _ => std::ops::Bound::Included(prefix.to_string()),
        };
        for (key, bytes) in objects.range((from, std::ops::Bound::Unbounded)) {
            if !key.starts_with(prefix) {
                break;
            }
            if listing.objects.len() >= limit {
                listing.truncated = true;
                break;
            }
            listing.objects.push((
                key.clone(),
                u64::try_from(bytes.len()).map_err(|_| invalid("object", "size overflow"))?,
            ));
        }
        Ok(listing)
    }

    fn delete(&self, key: &str, until: Instant) -> Result<(), BlobRefusal> {
        expired(until)?;
        check_key(key)?;
        self.objects()?.remove(key);
        Ok(())
    }
}

/// What every implementation of [`Blobs`] does (CLAUDE.md §9), run against an empty store that
/// holds at least 3 objects of 2 bytes: each failure named. Keys are under `prefix`.
pub fn conformance(store: &dyn Blobs, prefix: &str, until: Instant) -> Result<(), String> {
    let fail = |what: &str, got: String| Err(format!("{what}: {got}"));
    let a = format!("{prefix}/a");
    let b = format!("{prefix}/b");
    let c = format!("{prefix}/c");
    // AMPHORA-BLOBS-1: a created object is got back whole.
    if let Err(e) = store.create(&a, b"A1", until) {
        return fail("create", e.to_string());
    }
    match store.get(&a, until) {
        Ok(bytes) if bytes == b"A1" => {}
        other => return fail("get after create", format!("{other:?}")),
    }
    // AMPHORA-BLOBS-2: a create over an object is refused Exists and leaves it as it was.
    match store.create(&a, b"A2", until) {
        Err(BlobRefusal::Exists(_)) => {}
        other => return fail("create over an object", format!("{other:?}")),
    }
    match store.get(&a, until) {
        Ok(bytes) if bytes == b"A1" => {}
        other => return fail("get after a refused create", format!("{other:?}")),
    }
    // AMPHORA-BLOBS-3: a get of a key with no object is refused NotFound.
    match store.get(&b, until) {
        Err(BlobRefusal::NotFound(_)) => {}
        other => return fail("get of no object", format!("{other:?}")),
    }
    // AMPHORA-BLOBS-4: a listing gives a prefix's keys in byte order, after its cursor, at most
    // its limit, saying when more follow; a limit of 0 is refused.
    for key in [&c, &b] {
        if let Err(e) = store.create(key, b"xy", until) {
            return fail("create", e.to_string());
        }
    }
    let first = match store.list(&format!("{prefix}/"), None, 2, until) {
        Ok(listing) => listing,
        Err(e) => return fail("list", e.to_string()),
    };
    if first.objects != vec![(a.clone(), 2), (b.clone(), 2)] || !first.truncated {
        return fail("list's first page", format!("{first:?}"));
    }
    match store.list(&format!("{prefix}/"), Some(&b), 2, until) {
        Ok(rest) if rest.objects == vec![(c.clone(), 2)] && !rest.truncated => {}
        other => return fail("list after a cursor", format!("{other:?}")),
    }
    match store.list(&format!("{prefix}/"), None, 0, until) {
        Err(BlobRefusal::Invalid { .. }) => {}
        other => return fail("list with a limit of 0", format!("{other:?}")),
    }
    // AMPHORA-BLOBS-5: a deleted object is gone; deleting a key with no object succeeds; a key
    // deleted can be created again.
    if let Err(e) = store.delete(&b, until) {
        return fail("delete", e.to_string());
    }
    match store.get(&b, until) {
        Err(BlobRefusal::NotFound(_)) => {}
        other => return fail("get after delete", format!("{other:?}")),
    }
    if let Err(e) = store.delete(&b, until) {
        return fail("delete of no object", e.to_string());
    }
    if let Err(e) = store.create(&b, b"B2", until) {
        return fail("create after delete", e.to_string());
    }
    // AMPHORA-BLOBS-6: an object over the store's most is refused TooLarge and stores nothing.
    let max = store.object_max();
    if let Some(over) = max.checked_add(1) {
        let d = format!("{prefix}/d");
        match store.create(&d, &vec![0u8; over], until) {
            Err(BlobRefusal::TooLarge { .. }) => {}
            other => return fail("create over the most", format!("{other:?}")),
        }
        match store.get(&d, until) {
            Err(BlobRefusal::NotFound(_)) => {}
            other => return fail("get after a refused create", format!("{other:?}")),
        }
    }
    // AMPHORA-BLOBS-7: a malformed key is refused Invalid.
    for key in [
        format!("{prefix}//e"),
        format!("{prefix}/e\n"),
        format!("{prefix}\\e"),
    ] {
        match store.create(&key, b"", until) {
            Err(BlobRefusal::Invalid { .. }) => {}
            other => return fail("create at a malformed key", format!("{other:?}")),
        }
    }
    for key in [a, b, c] {
        if let Err(e) = store.delete(&key, until) {
            return fail("delete", e.to_string());
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_macros)]
mod tests {
    use super::*;

    /// AMPHORA-BLOBS-1 to 7: the simulator passes the seam's conformance suite.
    #[test]
    fn amphora_blobs_conformance_on_the_simulator() {
        let store = SimBlobs::new(16, 100, 1 << 20);
        let until = Instant::now() + std::time::Duration::from_secs(10);
        conformance(&store, "suite", until).unwrap();
        assert_eq!(store.bytes().unwrap(), 0);
    }

    /// The simulator refuses past its count and its bytes, storing nothing.
    #[test]
    fn amphora_blobs_simulator_bounds() {
        let store = SimBlobs::new(16, 2, 20);
        let until = Instant::now() + std::time::Duration::from_secs(10);
        store.create("a", &[0; 16], until).unwrap();
        assert!(matches!(
            store.create("b", &[0; 16], until),
            Err(BlobRefusal::Full { .. })
        ));
        store.create("b", &[0; 4], until).unwrap();
        assert!(matches!(
            store.create("c", &[], until),
            Err(BlobRefusal::Full { .. })
        ));
        assert_eq!(store.bytes().unwrap(), 20);
    }
}

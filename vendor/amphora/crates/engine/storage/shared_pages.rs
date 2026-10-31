//! Amphora: the pages a database's connections share (Amphora's `docs/design/store.md`, decision 20).
//!
//! Each connection keeps a page cache of its own; a page it misses there is looked up here before
//! it is read from the file. What is kept is a page's image as committed, under the version it is
//! (a frame of the log, in the log's generation, or the database file's), so whichever connection
//! read it in first, every connection whose snapshot sees that version reads the same bytes: no
//! read, no checksum. A lookup copies the image out under its shard's read lock, so readers of the
//! same pages proceed together and none holds a count on a buffer every other reads (decision 29).
//! The images never change: a frame's is committed (a writer shares
//! only frames committed before its transaction began, so no frame of a write in progress is ever
//! here; decision 27), and a file version is dropped before the database file's page is written
//! or the file cut. A connection that writes a shared buffer copies it first (`Pager::add_dirty`).
//!
//! Bounded: at most the pages it was made for, kept by CLOCK in shards by page number, each shard a
//! slice of the bound; a page added to a full shard evicts the first page the hand finds not read
//! since it last passed. One pool can serve many databases, a budget for them all: each file that
//! joins it gets a number of its own, part of its pages' keys, so no two files' pages meet, and a
//! file's pages leave with it when its database closes.

use crate::io::Buffer;
use crate::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use crate::sync::{Arc, RwLock};
use rustc_hash::FxHashMap;

/// Shards: a lock each, so readers of different pages seldom meet.
const SHARDS: usize = 64;
const SHARDS_U64: u64 = 64;

/// Which version of a page an image is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Version {
    /// The image a frame of the log holds. A generation's frames number from 1 again after the
    /// log restarts, so the generation (the header's `checkpoint_seq`) is part of the version.
    Frame { generation: u32, frame: u64 },
    /// The database file's.
    File,
}

/// A page of one database file and the version of it an image is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Key {
    /// The file it is a page of: the number the pool gave the file ([`SharedPages::join`]).
    pub file: u64,
    pub page: u64,
    pub version: Version,
}

/// An image kept, and its key's place in its shard's ring.
struct Entry {
    buffer: Arc<Buffer>,
    /// Read since the hand last passed: set by lookups under the shard's read lock, and only when
    /// not set already, so a page every reader reads isn't written at every read.
    visited: AtomicBool,
    at: usize,
}

#[derive(Default)]
struct Shard {
    /// Each image under its key, found by one lookup.
    map: FxHashMap<Key, Entry>,
    /// The keys in the order the hand passes them; `None` where an image was dropped.
    ring: Vec<Option<Key>>,
    hand: usize,
    added: u64,
}

/// A shard, and its lookups counted beside its lock (a lookup holds the lock to read, so it can't
/// count under it): one count a shard, where one for the pool, written by every reader's every
/// lookup, would move its line between their cores each time (decision 22).
#[derive(Default)]
struct ShardCell {
    shard: RwLock<Shard>,
    hits: AtomicU64,
    misses: AtomicU64,
}

/// Lookups that found an image, lookups that didn't, and images added: for tests and meters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub hits: u64,
    pub misses: u64,
    pub added: u64,
}

/// The pages a database's connections share.
pub struct SharedPages {
    shards: Box<[ShardCell]>,
    /// Pages each shard keeps at most.
    per_shard: usize,
    /// The number the next file to join gets.
    files: AtomicU64,
}

impl SharedPages {
    /// Shares at most `pages` pages (at least one a shard).
    pub fn new(pages: usize) -> Self {
        let per_shard = pages.checked_div(SHARDS).unwrap_or(0).max(1);
        Self {
            shards: (0..SHARDS).map(|_| ShardCell::default()).collect(),
            per_shard,
            files: AtomicU64::new(0),
        }
    }

    /// A number for one more database file reading through these pages, its keys' `file`: every
    /// file that joins gets its own.
    pub fn join(&self) -> u64 {
        self.files.fetch_add(1, Ordering::Relaxed)
    }

    fn shard(&self, page: u64) -> Option<&ShardCell> {
        let at = page.checked_rem(SHARDS_U64).unwrap_or(0);
        self.shards.get(usize::try_from(at).ok()?)
    }

    /// Copies the image of `key` into `dest`, if one is kept and is `dest`'s size: whether it was.
    pub fn copy_into(&self, key: &Key, dest: &mut [u8]) -> bool {
        let Some(cell) = self.shard(key.page) else {
            return false;
        };
        let found = {
            let shard = cell.shard.read();
            match shard.map.get(key) {
                Some(entry) if entry.buffer.len() == dest.len() => {
                    if !entry.visited.load(Ordering::Relaxed) {
                        entry.visited.store(true, Ordering::Relaxed);
                    }
                    dest.copy_from_slice(entry.buffer.as_slice());
                    true
                }
                _ => false,
            }
        };
        let count = if found { &cell.hits } else { &cell.misses };
        count.fetch_add(1, Ordering::Relaxed);
        found
    }

    /// The image of `key`, if one is kept, as a buffer of its own (`copy_into`'s lookup).
    #[cfg(test)]
    pub fn get(&self, key: &Key) -> Option<Arc<Buffer>> {
        let mut image = vec![0u8; 4096];
        self.copy_into(key, &mut image)
            .then(|| Arc::new(Buffer::new(image)))
    }

    /// Keeps `buffer` as the image of `key`, evicting by CLOCK if its shard is full. An image
    /// already kept under `key` stays (both are the same committed bytes).
    pub fn insert(&self, key: Key, buffer: Arc<Buffer>) {
        let Some(cell) = self.shard(key.page) else {
            return;
        };
        let mut shard = cell.shard.write();
        if let Some(entry) = shard.map.get(&key) {
            entry.visited.store(true, Ordering::Relaxed);
            return;
        }
        let at = if shard.ring.len() < self.per_shard {
            shard.ring.push(None);
            shard.ring.len().checked_sub(1)
        } else {
            Self::victim(&mut shard)
        };
        let Some(at) = at else {
            return;
        };
        if let Some(place) = shard.ring.get_mut(at) {
            *place = Some(key);
            shard.map.insert(
                key,
                Entry {
                    buffer,
                    visited: AtomicBool::new(false),
                    at,
                },
            );
            shard.added = shard.added.wrapping_add(1);
        }
    }

    /// The slot CLOCK frees in a full shard: the first empty one the hand meets, or the first
    /// not read since the hand last passed, its image dropped (two turns at most).
    fn victim(shard: &mut Shard) -> Option<usize> {
        let len = shard.ring.len();
        for _ in 0..len.saturating_mul(2) {
            let at = shard.hand;
            shard.hand = match at.checked_add(1) {
                Some(next) if next < len => next,
                _ => 0,
            };
            let key = match shard.ring.get(at) {
                Some(Some(key)) => *key,
                Some(None) => return Some(at),
                None => return None,
            };
            match shard.map.get(&key) {
                Some(entry) if entry.visited.load(Ordering::Relaxed) => {
                    entry.visited.store(false, Ordering::Relaxed);
                }
                _ => {
                    shard.map.remove(&key);
                    if let Some(place) = shard.ring.get_mut(at) {
                        *place = None;
                    }
                    return Some(at);
                }
            }
        }
        None
    }

    /// Drops `file`'s database file version of `page`: done before the file's page is written.
    pub fn forget_file_page(&self, file: u64, page: u64) {
        let key = Key {
            file,
            page,
            version: Version::File,
        };
        if let Some(cell) = self.shard(page) {
            let mut shard = cell.shard.write();
            if let Some(entry) = shard.map.remove(&key) {
                if let Some(place) = shard.ring.get_mut(entry.at) {
                    *place = None;
                }
            }
        }
    }

    /// Drops every database file version of `file`'s pages: done before the file is cut.
    pub fn forget_file(&self, file: u64) {
        self.forget_where(|key| key.file == file && key.version == Version::File);
    }

    /// Drops every image of `file`'s pages, at every version: done when the file leaves the pool
    /// (its database closed), so a pool that outlives its databases keeps none of theirs.
    pub fn forget_all(&self, file: u64) {
        self.forget_where(|key| key.file == file);
    }

    /// Drops every image whose key `matches`, a shard at a time.
    fn forget_where(&self, matches: impl Fn(&Key) -> bool) {
        for cell in self.shards.iter() {
            let mut shard = cell.shard.write();
            let gone: Vec<(Key, usize)> = shard
                .map
                .iter()
                .filter(|(key, _)| matches(key))
                .map(|(key, entry)| (*key, entry.at))
                .collect();
            for (key, at) in gone {
                shard.map.remove(&key);
                if let Some(place) = shard.ring.get_mut(at) {
                    *place = None;
                }
            }
        }
    }

    /// Pages it keeps at most.
    pub fn capacity(&self) -> usize {
        self.per_shard.saturating_mul(SHARDS)
    }

    /// Images kept now.
    pub fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|cell| cell.shard.read().map.len())
            .fold(0usize, usize::saturating_add)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Lookups and additions so far, the shards' summed.
    pub fn counts(&self) -> Counts {
        self.shards
            .iter()
            .fold(Counts::default(), |sum, cell| Counts {
                hits: sum.hits.wrapping_add(cell.hits.load(Ordering::Relaxed)),
                misses: sum.misses.wrapping_add(cell.misses.load(Ordering::Relaxed)),
                added: sum.added.wrapping_add(cell.shard.read().added),
            })
    }
}

/// One database file's place in a pool of shared pages: the pool, and the file's number in it.
/// Every key the file's pages take is made here, so none lacks the file's number. Its handles share
/// one membership: when the last goes (the database closed), the file's pages leave the pool.
#[derive(Clone)]
pub struct SharedFile {
    member: Arc<Member>,
}

/// A file's place in a pool, held while any handle on it is.
struct Member {
    pages: Arc<SharedPages>,
    file: u64,
}

impl Drop for Member {
    fn drop(&mut self) {
        self.pages.forget_all(self.file);
    }
}

impl SharedFile {
    /// Joins `pages` as a file of its own.
    pub fn join(pages: Arc<SharedPages>) -> Self {
        let file = pages.join();
        Self {
            member: Arc::new(Member { pages, file }),
        }
    }

    /// The pool.
    pub fn pages(&self) -> &Arc<SharedPages> {
        &self.member.pages
    }

    /// The key of this file's `page` at `version`.
    pub fn key(&self, page: u64, version: Version) -> Key {
        Key {
            file: self.member.file,
            page,
            version,
        }
    }

    /// Drops this file's database file version of `page`.
    pub fn forget_page(&self, page: u64) {
        self.member.pages.forget_file_page(self.member.file, page);
    }

    /// Drops every database file version of this file's pages.
    pub fn forget_file(&self) {
        self.member.pages.forget_file(self.member.file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(byte: u8) -> Arc<Buffer> {
        Arc::new(Buffer::new(vec![byte; 4096]))
    }

    fn file(page: u64) -> Key {
        Key {
            file: 0,
            page,
            version: Version::File,
        }
    }

    fn frame(page: u64, generation: u32, frame: u64) -> Key {
        Key {
            file: 0,
            page,
            version: Version::Frame { generation, frame },
        }
    }

    /// Do: keep a page's file image and two frame images. Expect: each found under its own
    /// version only, the same buffer as kept, and a frame of another generation not found.
    #[test]
    fn each_version_is_found_under_its_own_key() {
        let shared = SharedPages::new(1024);
        shared.insert(file(7), buffer(1));
        shared.insert(frame(7, 1, 10), buffer(2));
        shared.insert(frame(7, 1, 12), buffer(3));
        assert_eq!(shared.get(&file(7)).unwrap().as_slice()[0], 1);
        assert_eq!(shared.get(&frame(7, 1, 10)).unwrap().as_slice()[0], 2);
        assert_eq!(shared.get(&frame(7, 1, 12)).unwrap().as_slice()[0], 3);
        assert!(
            shared.get(&frame(7, 2, 10)).is_none(),
            "another generation's frame 10"
        );
        assert!(shared.get(&file(8)).is_none());
        assert_eq!(
            shared.counts(),
            Counts {
                hits: 3,
                misses: 2,
                added: 3
            }
        );
    }

    /// Do: forget a page's file version, then every file version. Expect: frame images stay.
    #[test]
    fn a_file_write_forgets_the_file_version_alone() {
        let shared = SharedPages::new(1024);
        for page in 1..=5 {
            shared.insert(file(page), buffer(1));
            shared.insert(frame(page, 1, page), buffer(2));
        }
        shared.forget_file_page(0, 3);
        assert!(shared.get(&file(3)).is_none());
        assert!(shared.get(&file(4)).is_some());
        assert!(shared.get(&frame(3, 1, 3)).is_some());
        shared.forget_file(0);
        assert!((1..=5).all(|page| shared.get(&file(page)).is_none()));
        assert!((1..=5).all(|page| shared.get(&frame(page, 1, page)).is_some()));
    }

    /// Do: add twice the bound's pages, reading a few of the first ones as they go. Expect: never
    /// more than the bound kept, and the pages read again kept over those read once.
    #[test]
    fn past_its_bound_it_evicts_by_clock() {
        let bound = SHARDS * 4;
        let shared = SharedPages::new(bound);
        let hot: Vec<u64> = (0..SHARDS as u64).collect();
        for page in 0..(bound as u64 * 2) {
            shared.insert(file(page), buffer(0));
            for &h in &hot {
                let _ = shared.get(&file(h));
            }
        }
        assert!(shared.len() <= bound, "{} kept", shared.len());
        assert!(
            hot.iter().all(|&h| shared.get(&file(h)).is_some()),
            "the pages read again stay"
        );
    }

    /// Do: two files join one pool and keep the same page at the same versions; forget one
    /// file's page, then its whole file. Expect: each finds its own image alone, and forgetting
    /// one file's versions leaves the other's (decision 20: one budget for many databases).
    #[test]
    fn files_sharing_a_pool_keep_their_pages_apart() {
        let pool = Arc::new(SharedPages::new(1024));
        let a = SharedFile::join(pool.clone());
        let b = SharedFile::join(pool.clone());
        assert_ne!(a.key(7, Version::File), b.key(7, Version::File));
        for (shared, byte) in [(&a, 1), (&b, 2)] {
            pool.insert(shared.key(7, Version::File), buffer(byte));
            let at = Version::Frame {
                generation: 1,
                frame: 3,
            };
            pool.insert(shared.key(7, at), buffer(byte + 10));
        }
        assert_eq!(pool.get(&a.key(7, Version::File)).unwrap().as_slice()[0], 1);
        assert_eq!(pool.get(&b.key(7, Version::File)).unwrap().as_slice()[0], 2);
        a.forget_page(7);
        assert!(pool.get(&a.key(7, Version::File)).is_none());
        assert!(pool.get(&b.key(7, Version::File)).is_some());
        pool.insert(a.key(8, Version::File), buffer(3));
        a.forget_file();
        assert!(pool.get(&a.key(8, Version::File)).is_none());
        assert!(pool.get(&b.key(7, Version::File)).is_some());
        let frame = Version::Frame {
            generation: 1,
            frame: 3,
        };
        assert_eq!(pool.get(&a.key(7, frame)).unwrap().as_slice()[0], 11);
        assert_eq!(pool.get(&b.key(7, frame)).unwrap().as_slice()[0], 12);
    }

    /// Do: keep an image, look it up into a buffer of its size and into a smaller one. Expect: the
    /// first gets the image's bytes and counts a hit, with no new holder of the pool's buffer; the
    /// second gets nothing and counts a miss (decision 29: a lookup copies the image out).
    #[test]
    fn a_lookup_copies_the_image_out_and_holds_nothing() {
        let pool = SharedPages::new(1024);
        let kept = buffer(7);
        pool.insert(file(3), kept.clone());
        let holders = Arc::strong_count(&kept);
        let mut dest = vec![0u8; 4096];
        assert!(pool.copy_into(&file(3), &mut dest));
        assert!(dest.iter().all(|&b| b == 7));
        assert_eq!(Arc::strong_count(&kept), holders);
        let mut small = vec![0u8; 512];
        assert!(!pool.copy_into(&file(3), &mut small));
        assert_eq!(
            pool.counts(),
            Counts {
                hits: 1,
                misses: 1,
                added: 1
            }
        );
    }

    /// Do: two files join one pool and keep pages at both versions; drop one of the first file's
    /// two handles, then the other. Expect: its pages stay while a handle is left and all go with
    /// the last, the second file's untouched (decision 26: a process's pool outlives its databases
    /// and keeps none of a closed one's pages).
    #[test]
    fn a_file_that_leaves_the_pool_takes_its_pages_with_it() {
        let pool = Arc::new(SharedPages::new(1024));
        let a = SharedFile::join(pool.clone());
        let b = SharedFile::join(pool.clone());
        let at = Version::Frame {
            generation: 2,
            frame: 9,
        };
        for page in 1..=20 {
            pool.insert(a.key(page, Version::File), buffer(1));
            pool.insert(a.key(page, at), buffer(2));
            pool.insert(b.key(page, Version::File), buffer(3));
        }
        let (a_key, a_frame, b_key) = (
            a.key(5, Version::File),
            a.key(5, at),
            b.key(5, Version::File),
        );
        let other_handle = a.clone();
        drop(a);
        assert!(pool.get(&a_key).is_some(), "a handle is left");
        drop(other_handle);
        assert!(pool.get(&a_key).is_none());
        assert!(pool.get(&a_frame).is_none());
        assert_eq!(pool.len(), 20, "the second file's pages alone");
        assert_eq!(pool.get(&b_key).unwrap().as_slice()[0], 3);
        drop(b);
        assert!(pool.is_empty());
    }
}

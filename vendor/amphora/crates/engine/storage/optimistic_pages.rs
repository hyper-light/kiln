//! Amphora (G3 step 6, prototype F): shared pages read without a lock, under a version check
//! (docs/design/engine.md decision 3; docs/design/store.md decision 43).
//!
//! A lookup in the pool of shared pages (`shared_pages.rs`) takes its shard's read lock: a write
//! to the lock's word, one line every reader of the shard's pages writes, so at 32 readers the
//! line moves between their cores at each lookup. Here, in front of the shards, are frames whose
//! images are atomic page words under a sequence that is odd while a writer changes them
//! (LeanStore's optimistic latch: Leis et al., "LeanStore", ICDE 2018; "The ART of Practical
//! Synchronization", DaMoN 2016, version validation). A reader reads the sequence, the key and the
//! words, then the sequence again: equal and even, it has a whole image of the key it asked for;
//! otherwise it asks the shard under its lock, never retrying. Safe Rust: every word read while a
//! writer may write it is an atomic, read relaxed, ordered by an acquire fence before the second
//! read of the sequence (Boehm, "Can Seqlocks Get Along with Programming Language Memory Models?",
//! MSPC 2012: the reader's loads relaxed, then a fence, then the check).
//!
//! What a frame holds is what the shard holds under the same key, and keys never change their
//! bytes (a frame's image is committed; a file version is forgotten before the file's page is
//! written): so a whole image read here is the image the shard would have given. A key forgotten
//! by the pool is cleared here before the forget returns. A frame's writer takes the frame by
//! turning its sequence odd with a compare-and-swap (LeanStore's exclusive latch: the same word the
//! readers check), so writers of different frames never meet on a line and no lock spans the
//! frames: an image found under its shard's lock and not here is copied in, if no other writer is
//! at its frame, replacing whatever the frame held (direct-mapped by the key's hash); a forget
//! waits for the frame's writer, which holds it for one image's stores.
//!
//! Lookups found here are counted on a stripe of their thread's own, so the count writes no line
//! another reader writes.

use crate::storage::shared_pages::{Key, Version};
use crate::sync::atomic::{fence, AtomicU64, Ordering};
use std::cell::Cell;
use std::sync::OnceLock;

/// How shared pages are looked up (the prototype's switch; `AMPHORA_SHARED_PAGE_READS` sets it for
/// a process: `locked` or `optimistic`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SharedPageReads {
    /// Today's: under the shard's read lock.
    Locked = 0,
    /// The frames read under a version check first, then the shard under its lock.
    Optimistic = 1,
}

static PROCESS_DEFAULT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(UNSET);
const UNSET: u8 = u8::MAX;

impl SharedPageReads {
    /// The way pools made from now on take: `set_process_default`'s if called, else
    /// `AMPHORA_SHARED_PAGE_READS` (read once), else `Locked`.
    pub fn process_default() -> Self {
        static FROM_ENV: OnceLock<SharedPageReads> = OnceLock::new();
        match PROCESS_DEFAULT.load(std::sync::atomic::Ordering::Relaxed) {
            UNSET => *FROM_ENV.get_or_init(|| {
                match std::env::var("AMPHORA_SHARED_PAGE_READS").as_deref() {
                    Ok("optimistic") => Self::Optimistic,
                    _ => Self::Locked,
                }
            }),
            1 => Self::Optimistic,
            _ => Self::Locked,
        }
    }

    /// Sets the way pools made from now on in this process take (a bench's or a test's switch).
    pub fn set_process_default(mode: Self) {
        PROCESS_DEFAULT.store(mode as u8, std::sync::atomic::Ordering::Relaxed);
    }
}

/// A key's words: its file, its page, its version's kind and generation, and its frame. A frame
/// holding no image has kind `EMPTY`.
const EMPTY: u64 = 0;
const FILE: u64 = 1;
const FRAME: u64 = 2;

fn words_of(key: &Key) -> [u64; 4] {
    match key.version {
        Version::File => [key.file, key.page, FILE, 0],
        Version::Frame { generation, frame } => [
            key.file,
            key.page,
            FRAME | u64::from(generation).checked_shl(2).unwrap_or(0),
            frame,
        ],
    }
}

fn key_of(words: [u64; 4]) -> Option<Key> {
    let [file, page, kind, frame] = words;
    let version = match kind & 0b11 {
        FILE => Version::File,
        FRAME => Version::Frame {
            generation: u32::try_from(kind.checked_shr(2)?).ok()?,
            frame,
        },
        _ => return None,
    };
    Some(Key {
        file,
        page,
        version,
    })
}

/// A frame, alone on its cache line pair (the adjacent-line prefetcher pulls lines in pairs, so
/// 128 bytes, as crossbeam's `CachePadded` aligns on x86_64 and aarch64).
#[repr(align(128))]
struct Frame {
    /// Odd while a writer changes the frame.
    sequence: AtomicU64,
    key: [AtomicU64; 4],
    words: Box<[AtomicU64]>,
}

#[repr(align(128))]
struct Stripe(AtomicU64);

/// The frames read without a lock, for images of one size.
pub(crate) struct Frames {
    frames: Box<[Frame]>,
    /// Bytes an image takes: a whole number of words.
    page_bytes: usize,
    /// Lookups answered here, a stripe a thread.
    hits: Box<[Stripe]>,
}

/// Each thread's number, taken once from a counter (a write to a shared line once per thread,
/// never per lookup).
fn thread_number() -> usize {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    thread_local! {
        static NUMBER: Cell<Option<usize>> = const { Cell::new(None) };
    }
    NUMBER.with(|n| match n.get() {
        Some(v) => v,
        None => {
            let v = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            n.set(Some(v));
            v
        }
    })
}

impl Frames {
    /// `count` frames (at least one) of `page_bytes` each; None unless `page_bytes` is a whole,
    /// nonzero number of words.
    pub(crate) fn new(count: usize, page_bytes: usize, stripes: usize) -> Option<Self> {
        if page_bytes == 0 || page_bytes.checked_rem(8) != Some(0) {
            return None;
        }
        let words = page_bytes.checked_div(8)?;
        Some(Self {
            frames: (0..count.max(1))
                .map(|_| Frame {
                    sequence: AtomicU64::new(0),
                    key: [0; 4].map(AtomicU64::new),
                    words: (0..words).map(|_| AtomicU64::new(0)).collect(),
                })
                .collect(),
            page_bytes,
            hits: (0..stripes.max(1))
                .map(|_| Stripe(AtomicU64::new(0)))
                .collect(),
        })
    }

    pub(crate) fn page_bytes(&self) -> usize {
        self.page_bytes
    }

    /// Frames kept.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.frames.len()
    }

    fn frame(&self, key: &Key) -> Option<&Frame> {
        let [a, b, c, d] = words_of(key);
        // FxHash's mix over the key's words.
        let hash = [a, b, c, d].iter().fold(0u64, |h, w| {
            (h.rotate_left(5) ^ w).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95)
        });
        let at = usize::try_from(hash.checked_rem(u64::try_from(self.frames.len()).ok()?)?).ok()?;
        self.frames.get(at)
    }

    /// Copies the image of `key` into `dest` if a frame holds it whole: whether it did. One
    /// attempt, no lock, no write to any line but this thread's stripe; `dest` holds nothing of
    /// use when it says no.
    pub(crate) fn read(&self, key: &Key, dest: &mut [u8]) -> bool {
        if dest.len() != self.page_bytes {
            return false;
        }
        let Some(frame) = self.frame(key) else {
            return false;
        };
        let before = frame.sequence.load(Ordering::Acquire);
        if before & 1 == 1 {
            return false;
        }
        let wanted = words_of(key);
        if frame
            .key
            .iter()
            .zip(wanted)
            .any(|(word, w)| word.load(Ordering::Relaxed) != w)
        {
            return false;
        }
        for (bytes, word) in dest
            .as_chunks_mut::<8>()
            .0
            .iter_mut()
            .zip(frame.words.iter())
        {
            *bytes = word.load(Ordering::Relaxed).to_le_bytes();
        }
        fence(Ordering::Acquire);
        if frame.sequence.load(Ordering::Relaxed) != before {
            return false;
        }
        let stripe = thread_number().checked_rem(self.hits.len()).unwrap_or(0);
        if let Some(count) = self.hits.get(stripe) {
            count.0.fetch_add(1, Ordering::Relaxed);
        }
        true
    }

    /// Writes `image` as `key`'s into its frame, unless another writer is at that frame (it is
    /// then left out: the next lookup finds it under the shard's lock again) or the frame holds it
    /// already. Called while the pool keeps `key`'s image, under its shard's lock.
    pub(crate) fn offer(&self, key: &Key, image: &[u8]) {
        if image.len() != self.page_bytes {
            return;
        }
        let Some(frame) = self.frame(key) else {
            return;
        };
        let wanted = words_of(key);
        if Self::holds(frame, wanted) {
            return;
        }
        let Some(begun) = Self::try_latch(frame) else {
            return;
        };
        Self::write(frame, begun, wanted, image);
    }

    /// Whether `frame`'s key words are `key`'s (read by its writer, or as a hint).
    fn holds(frame: &Frame, key: [u64; 4]) -> bool {
        frame
            .key
            .iter()
            .zip(key)
            .all(|(word, w)| word.load(Ordering::Relaxed) == w)
    }

    /// Takes `frame` for writing, if no writer has it: its sequence turned odd by a
    /// compare-and-swap, the odd value returned. Readers that read it odd, or read it changed after
    /// their words, ask the shard instead.
    fn try_latch(frame: &Frame) -> Option<u64> {
        let even = frame.sequence.load(Ordering::Relaxed);
        if even & 1 == 1 {
            return None;
        }
        let begun = even.wrapping_add(1);
        frame
            .sequence
            .compare_exchange(even, begun, Ordering::Acquire, Ordering::Relaxed)
            .ok()?;
        // The odd sequence before any word stored (Boehm 2012's writer: the fence after it).
        fence(Ordering::Release);
        Some(begun)
    }

    /// Takes `frame` for writing once its writer, if any, is done: a writer holds a frame for one
    /// image's stores and nothing else, so the wait is that long at most.
    fn latch(frame: &Frame) -> u64 {
        loop {
            if let Some(begun) = Self::try_latch(frame) {
                return begun;
            }
            crate::thread::spin_loop();
            crate::thread::yield_now();
        }
    }

    /// Gives `frame` back unchanged (its sequence even again; a reader that overlapped asks the
    /// shard, as after any write).
    fn unlatch(frame: &Frame, begun: u64) {
        frame
            .sequence
            .store(begun.wrapping_add(1), Ordering::Release);
    }

    /// Clears every frame whose key `matches`: done by the pool before a forget returns.
    pub(crate) fn forget_where(&self, matches: impl Fn(&Key) -> bool) {
        for frame in self.frames.iter() {
            let begun = Self::latch(frame);
            let words = [0, 1, 2, 3].map(|i| {
                frame
                    .key
                    .get(i)
                    .map_or(EMPTY, |w| w.load(Ordering::Relaxed))
            });
            if key_of(words).is_some_and(|key| matches(&key)) {
                Self::write(frame, begun, [0, 0, EMPTY, 0], &[]);
            } else {
                Self::unlatch(frame, begun);
            }
        }
    }

    /// Clears `key`'s frame if it holds `key`.
    pub(crate) fn forget(&self, key: &Key) {
        let Some(frame) = self.frame(key) else {
            return;
        };
        let begun = Self::latch(frame);
        let wanted = words_of(key);
        if Self::holds(frame, wanted) {
            Self::write(frame, begun, [0, 0, EMPTY, 0], &[]);
        } else {
            Self::unlatch(frame, begun);
        }
    }

    /// Called by the frame's writer, its sequence odd (`begun`, from `try_latch`): the key and
    /// the words stored, the sequence made even again (`log_index::HeaderCopy::store`'s order).
    fn write(frame: &Frame, begun: u64, key: [u64; 4], image: &[u8]) {
        for (word, w) in frame.key.iter().zip(key) {
            word.store(w, Ordering::Relaxed);
        }
        for (word, bytes) in frame.words.iter().zip(image.as_chunks::<8>().0) {
            word.store(u64::from_le_bytes(*bytes), Ordering::Relaxed);
        }
        Self::unlatch(frame, begun);
    }

    /// Lookups answered here so far.
    pub(crate) fn hits(&self) -> u64 {
        self.hits
            .iter()
            .fold(0u64, |sum, s| sum.wrapping_add(s.0.load(Ordering::Relaxed)))
    }
}

#[cfg(all(test, not(shuttle)))]
mod tests {
    use super::*;
    use crate::io::Buffer;
    use crate::storage::shared_pages::SharedPages;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    const PAGE: usize = 4096;

    /// Every word of page `page`'s image at version `version`: the page above bit 40, the version
    /// below, so a word read says whose image it came from.
    fn word(page: u64, version: u64) -> u64 {
        (page << 40) | version
    }

    fn image(page: u64, version: u64) -> Vec<u8> {
        word(page, version).to_le_bytes().repeat(PAGE / 8)
    }

    fn frame_key(page: u64, frame: u64) -> Key {
        Key {
            file: 0,
            page,
            version: Version::Frame {
                generation: 1,
                frame,
            },
        }
    }

    fn file_key(page: u64) -> Key {
        Key {
            file: 0,
            page,
            version: Version::File,
        }
    }

    /// G3 step 6 (T-G3.6.2). Do: offer an image to the frames, read it, read another key and another size,
    /// forget it, read it again. Expect: the image whole, the others not found, nothing after the
    /// forget; a key's kind and generation read back as written.
    #[test]
    fn g3_s6_frames_give_back_what_was_offered_until_forgotten() {
        let frames = Frames::new(8, PAGE, 4).unwrap();
        let key = frame_key(3, 9);
        frames.offer(&key, &image(3, 9));
        let mut dest = vec![0u8; PAGE];
        assert!(frames.read(&key, &mut dest));
        assert_eq!(dest, image(3, 9));
        assert!(!frames.read(&frame_key(3, 10), &mut dest));
        assert!(!frames.read(&key, &mut vec![0u8; 512]));
        assert_eq!(key_of(words_of(&key)), Some(key));
        assert_eq!(key_of(words_of(&file_key(5))), Some(file_key(5)));
        frames.forget(&key);
        assert!(!frames.read(&key, &mut dest));
        frames.offer(&file_key(4), &image(4, 1));
        frames.forget_where(|k| k.version == Version::File);
        assert!(!frames.read(&file_key(4), &mut dest));
        assert_eq!(frames.hits(), 1);
        assert!(
            Frames::new(8, 12, 4).is_none(),
            "not a whole number of words"
        );
    }

    /// T-G3.6.2. Do: a pool made `Locked` and one made `Optimistic`, an image kept in each and looked up
    /// twice. Expect: the locked pool makes no frames; the optimistic one makes them, charged to
    /// its budget, and answers the second lookup from them.
    #[test]
    fn g3_s6_only_the_optimistic_pool_reads_without_its_lock() {
        let locked = SharedPages::with_reads(1024, SharedPageReads::Locked);
        let optimistic = SharedPages::with_reads(1024, SharedPageReads::Optimistic);
        let mut dest = vec![0u8; PAGE];
        for pool in [&locked, &optimistic] {
            pool.insert(file_key(7), Arc::new(Buffer::new(image(7, 1))));
            assert!(pool.copy_into(&file_key(7), &mut dest));
            assert!(pool.copy_into(&file_key(7), &mut dest));
            assert_eq!(dest, image(7, 1));
        }
        assert!(locked.front_made().is_none());
        assert_eq!(locked.charged(), 0);
        let front = optimistic.front_made().unwrap();
        assert_eq!(optimistic.charged(), front.len());
        assert_eq!(front.hits(), 1);
        optimistic.forget_file_page(0, 7);
        assert!(!optimistic.copy_into(&file_key(7), &mut dest));
    }

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n.max(1)
        }
    }

    /// G3 step 6's test (T-G3.6.1, AC-G3.6.2's correctness side), its short instance (AMPHORA_G3S6_SECONDS runs it as long as asked, the
    /// coordinator's 60 s run; AMPHORA_G3S6_COMMITS, as many commits as asked). Do: a writer
    /// commits frames of 48 pages, two pages a commit, into a pool too small to keep them (so the
    /// shards evict and the frames are written over all the time), and forgets and writes again
    /// file versions as a checkpoint would; 32 readers each take the committed frame as their
    /// snapshot and look up a page at a version in it, and a page's file version. Expect: every
    /// image a reader gets is whole and the version it asked for (no word of a version past its
    /// snapshot, no word of another page or version), and the frames answered lookups (not vacuous).
    #[test]
    fn g3_s6_32_readers_and_a_writer_see_no_unsnapshotted_version_and_no_torn_page() {
        const PAGES: u64 = 48;
        const FILE_MARK: u64 = 1 << 39;
        let commits: u64 = std::env::var("AMPHORA_G3S6_COMMITS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5_000);
        let until = std::env::var("AMPHORA_G3S6_SECONDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .map(|s| std::time::Instant::now() + std::time::Duration::from_secs(s));
        let pool = Arc::new(SharedPages::with_reads(64 * 8, SharedPageReads::Optimistic));
        let committed = Arc::new(AtomicU64::new(0));
        let done = Arc::new(AtomicBool::new(false));
        let readers: Vec<_> = (0..32u64)
            .map(|r| {
                let (pool, committed, done) = (pool.clone(), committed.clone(), done.clone());
                std::thread::spawn(move || {
                    let mut rng = Rng((r + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
                    let mut dest = vec![0u8; PAGE];
                    let (mut found, mut unsnapshotted, mut torn) = (0u64, 0u64, 0u64);
                    while !done.load(Ordering::Acquire) {
                        let snapshot = committed.load(Ordering::Acquire);
                        if snapshot == 0 {
                            std::hint::spin_loop();
                            continue;
                        }
                        // A recent frame of the snapshot, and the page it wrote.
                        let frame = snapshot - rng.below(snapshot.min(32));
                        let page = if rng.below(2) == 0 {
                            frame % PAGES
                        } else {
                            (frame * 7) % PAGES
                        };
                        if pool.copy_into(&frame_key(page, frame), &mut dest) {
                            found += 1;
                            for bytes in dest.as_chunks::<8>().0 {
                                let w = u64::from_le_bytes(*bytes);
                                if w != word(page, frame) {
                                    if w & ((1 << 40) - 1) > snapshot {
                                        unsnapshotted += 1;
                                    } else {
                                        torn += 1;
                                    }
                                    break;
                                }
                            }
                        }
                        let page = rng.below(PAGES);
                        if pool.copy_into(&file_key(page), &mut dest) {
                            let first = u64::from_le_bytes(dest[..8].try_into().unwrap());
                            let whole = dest
                                .as_chunks::<8>()
                                .0
                                .iter()
                                .all(|b| u64::from_le_bytes(*b) == first);
                            if !whole || first >> 40 != page || first & FILE_MARK == 0 {
                                torn += 1;
                            }
                        }
                    }
                    (found, unsnapshotted, torn)
                })
            })
            .collect();
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        let mut frame = 0u64;
        let mut epoch = 0u64;
        while frame < commits || until.is_some_and(|u| std::time::Instant::now() < u) {
            frame += 1;
            for page in [frame % PAGES, (frame * 7) % PAGES] {
                pool.insert(
                    frame_key(page, frame),
                    Arc::new(Buffer::new(image(page, frame))),
                );
            }
            committed.store(frame, Ordering::Release);
            // A checkpoint's nemesis: a file page forgotten and its new image kept, or every file
            // version forgotten at once.
            match rng.below(16) {
                0 => {
                    let page = rng.below(PAGES);
                    epoch += 1;
                    pool.forget_file_page(0, page);
                    pool.insert(
                        file_key(page),
                        Arc::new(Buffer::new(image(page, FILE_MARK | epoch))),
                    );
                }
                1 => pool.forget_file(0),
                _ => {}
            }
        }
        done.store(true, Ordering::Release);
        let (mut found, mut unsnapshotted, mut torn) = (0, 0, 0);
        for reader in readers {
            let (f, u, t) = reader.join().unwrap();
            found += f;
            unsnapshotted += u;
            torn += t;
        }
        let from_frames = pool.front_made().map_or(0, Frames::hits);
        assert_eq!(unsnapshotted, 0, "images of versions past the snapshot");
        assert_eq!(torn, 0, "torn images");
        assert!(
            found > 0 && from_frames > 0,
            "found {found}, {from_frames} without a lock"
        );
    }
}

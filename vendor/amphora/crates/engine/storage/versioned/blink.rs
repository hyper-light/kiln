//! Amphora: the versioned storage's B-link tree (Amphora's `docs/design/engine.md`, decision 3).
//!
//! Lehman and Yao's tree on the buffer pool: each page bounded above and linked to its right
//! sibling, so an operation that reaches a page a split has just divided moves right until its key
//! is below the page's bound, and none holds a latch on a parent while it holds one on a child.
//! Inner pages are read from copies each descender keeps, current while their frame's version is
//! the one they were copied under ([`Descender`]); leaves are latched shared to read and exclusive
//! to write. A writer holds at most two latches, a page and the new page it splits into, and then
//! the parent alone; pages are never merged (an emptied page stays, as in Lehman and Yao's tree).
//! A split of the root makes the new root before the root's latch goes, as Lehman and Yao's
//! splitter holds the page it split until it has the level above: otherwise a writer that moved
//! right into the new page could split it in turn and find no level above it to post to.
//! Every loop over pages is bounded: by the tree's height going down, and by the pages handed out
//! moving right.

use super::page::{self, Compare, Kind, Tail};
use super::pool::Pool;
use crate::error::LimboError;
use crate::sync::atomic::{AtomicU64, Ordering};
use crate::Result;
use rustc_hash::FxHashMap;
use std::cmp::Ordering as Order;

/// Levels a tree may have: past it an insert is refused (a typed error, never a stack).
pub const MAX_LEVELS: usize = 32;

/// Copies of inner pages a descender keeps at most; past it, it forgets them all.
const COPIES: usize = 1024;

/// Orders keys: a tree's comparison.
pub type Comparator = Box<dyn Fn(&[u8], &[u8]) -> Order + Send + Sync>;

/// A B-link tree in a pool: its root, which a split of the root replaces under the root's latch
/// (so one split at a time replaces it).
pub struct Tree {
    root: AtomicU64,
    cmp: Comparator,
    /// A test's pause of the next root split (`Tree::pause_next_root_split`).
    #[cfg(test)]
    pause: crate::sync::Mutex<Option<Pause>>,
}

/// Holds the next root split once the root's latch is released: `entered` says it has paused, and
/// it goes on when `release` says so (or is dropped).
#[cfg(test)]
struct Pause {
    entered: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

/// The copies of inner pages one reader or writer descends through, by page, each under the
/// frame version it was copied at.
#[derive(Default)]
pub struct Descender {
    copies: FxHashMap<u64, (u64, Vec<u8>)>,
}

impl Descender {
    pub fn new() -> Self {
        Self::default()
    }

    /// `page`'s image as of now: the copy kept, if its frame's version hasn't moved, or a new one.
    fn image<'a>(&'a mut self, pool: &Pool, page: u64) -> Result<&'a [u8]> {
        let now = pool.version(page)?;
        let current = self.copies.get(&page).is_some_and(|(v, _)| *v == now);
        if !current {
            let read = pool.read(page)?;
            let copy = read.to_vec();
            let version = read.version();
            drop(read);
            if self.copies.len() >= COPIES {
                self.copies.clear();
            }
            self.copies.insert(page, (version, copy));
        }
        self.copies
            .get(&page)
            .map(|(_, image)| image.as_slice())
            .ok_or_else(|| LimboError::InternalError("a page copy just made is missing".into()))
    }
}

fn too_tall() -> LimboError {
    LimboError::InternalError(format!("a B-link tree of more than {MAX_LEVELS} levels"))
}

/// The page right of `from`, for a key past its bound, its moves counted: a level's right links
/// never cycle, so more moves than pages handed out (which concurrent splits keep adding to) is a
/// damaged tree.
fn step_right(pool: &Pool, moves: &mut u64, from: u64, next: Option<u64>) -> Result<u64> {
    *moves = moves.checked_add(1).ok_or(LimboError::IntegerOverflow)?;
    if *moves > pool.allocated() {
        return Err(LimboError::Corrupt(format!(
            "versioned storage: right links from page {from} that don't end"
        )));
    }
    next.ok_or_else(|| no_end(from))
}

fn no_end(page: u64) -> LimboError {
    LimboError::Corrupt(format!(
        "versioned storage: page {page}'s bound is below a key, and it has no right sibling"
    ))
}

impl Tree {
    /// A new, empty tree: one leaf, its root.
    pub fn create(pool: &Pool, cmp: Comparator) -> Result<Self> {
        let root = pool.allocate()?;
        {
            let mut w = pool.write(root)?;
            page::init(&mut w, Kind::Leaf, 0)?;
        }
        Ok(Self {
            root: AtomicU64::new(root),
            cmp,
            #[cfg(test)]
            pause: crate::sync::Mutex::new(None),
        })
    }

    /// The tree whose root is `root`, already in the pool.
    pub fn open(root: u64, cmp: Comparator) -> Self {
        Self {
            root: AtomicU64::new(root),
            cmp,
            #[cfg(test)]
            pause: crate::sync::Mutex::new(None),
        }
    }

    pub fn root(&self) -> u64 {
        self.root.load(Ordering::Acquire)
    }

    fn compare(&self) -> Compare<'_> {
        &*self.cmp
    }

    /// The leaf whose keys may include `key`, and the inner pages passed on the way down, one a
    /// level from the leaf's parent up (where a split's separator goes: the parent may have split
    /// since, which moving right from it mends).
    fn descend(&self, pool: &Pool, d: &mut Descender, key: &[u8]) -> Result<(u64, Vec<u64>)> {
        let cmp = self.compare();
        let mut page = self.root();
        let mut path = Vec::new();
        for _ in 0..MAX_LEVELS {
            let mut moves = 0u64;
            let image = loop {
                let image = d.image(pool, page)?;
                if !page::beyond(image, key, cmp)? {
                    break d.image(pool, page)?;
                }
                page = step_right(pool, &mut moves, page, page::right(image)?)?;
            };
            let level = page::level(image)?;
            if level == 0 {
                path.reverse();
                return Ok((page, path));
            }
            let child = page::route(image, key, cmp)?;
            path.push(page);
            if level == 1 {
                path.reverse();
                return Ok((child, path));
            }
            page = child;
        }
        Err(too_tall())
    }

    /// Whether a row of `key` and `value` fits a page: larger rows are refused (`TooBig`). The key
    /// must fit an inner page's cell too, where it may go as a separator.
    pub fn check_row(key: &[u8], value: &[u8]) -> Result<()> {
        if page::fits(key, Tail::Value(value))? && page::fits(key, Tail::Child(0))? {
            Ok(())
        } else {
            Err(LimboError::TooBig)
        }
    }

    /// `f` on `key`'s cell (None: no cell), under its leaf's shared latch.
    pub fn read_with<R>(
        &self,
        pool: &Pool,
        d: &mut Descender,
        key: &[u8],
        f: impl FnOnce(Option<Cell<'_>>) -> Result<R>,
    ) -> Result<R> {
        let cmp = self.compare();
        let (mut leaf, _) = self.descend(pool, d, key)?;
        let mut moves = 0u64;
        loop {
            let r = pool.read(leaf)?;
            if page::beyond(&r, key, cmp)? {
                leaf = step_right(pool, &mut moves, leaf, page::right(&r)?)?;
                continue;
            }
            let cell = match page::search(&r, key, cmp)? {
                Ok(i) => Some(Cell {
                    flags: page::flags(&r, i)?,
                    value: page::value(&r, i)?,
                }),
                Err(_) => None,
            };
            return f(cell);
        }
    }

    /// `f` on `key`'s cell (None: no cell) under its leaf's exclusive latch, then the change it
    /// returns made to the cell before the latch goes; what `f` returned besides. A value put must
    /// fit ([`Tree::check_row`], asked before: `f` has run by the time a page could refuse it).
    pub fn modify<R>(
        &self,
        pool: &Pool,
        d: &mut Descender,
        key: &[u8],
        f: impl FnOnce(Option<Cell<'_>>) -> Result<(Change, R)>,
    ) -> Result<R> {
        let cmp = self.compare();
        let (mut leaf, path) = self.descend(pool, d, key)?;
        let mut moves = 0u64;
        let mut w = loop {
            let w = pool.write(leaf)?;
            if page::beyond(&w, key, cmp)? {
                leaf = step_right(pool, &mut moves, leaf, page::right(&w)?)?;
                continue;
            }
            break w;
        };
        let found = page::search(&w, key, cmp)?;
        let cell = match found {
            Ok(i) => Some(Cell {
                flags: page::flags(&w, i)?,
                value: page::value(&w, i)?,
            }),
            Err(_) => None,
        };
        let (change, out) = f(cell)?;
        let split = match (change, found) {
            (Change::Keep, _) => None,
            (Change::Mark(flags), Ok(i)) => {
                page::set_flags(&mut w, i, flags)?;
                None
            }
            (Change::Mark(_), Err(_)) => {
                return Err(LimboError::InternalError(
                    "versioned storage: flags set on a cell that isn't there".into(),
                ))
            }
            (Change::Remove, Ok(i)) => {
                page::remove(&mut w, i)?;
                None
            }
            (Change::Remove, Err(_)) => None,
            (Change::Put(value, flags), Ok(i)) => {
                if page::replace_flagged(&mut w, i, &value, flags)? {
                    None
                } else {
                    // No room for the new value: the old goes, the new is inserted after the
                    // leaf splits.
                    page::remove(&mut w, i)?;
                    Some(self.split_and_insert(pool, &mut w, leaf, key, &value, flags)?)
                }
            }
            (Change::Put(value, flags), Err(i)) => {
                if page::insert_flagged(&mut w, i, key, Tail::Value(&value), flags)? {
                    None
                } else {
                    Some(self.split_and_insert(pool, &mut w, leaf, key, &value, flags)?)
                }
            }
        };
        // A split root makes the new root before its latch goes (the module's note).
        let grew = match &split {
            Some(split) => self.grow(pool, split)?,
            None => false,
        };
        drop(w);
        #[cfg(test)]
        if grew {
            self.pause_after_root_split();
        }
        if let (Some(split), false) = (split, grew) {
            self.post(pool, d, split, &path)?;
        }
        Ok(out)
    }

    /// The value under `key`, if any.
    pub fn get(&self, pool: &Pool, d: &mut Descender, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.read_with(pool, d, key, |cell| Ok(cell.map(|c| c.value.to_vec())))
    }

    /// Puts `value` under `key`: the value it replaced, if any. A row too large for a page is
    /// refused (`TooBig`).
    pub fn put(
        &self,
        pool: &Pool,
        d: &mut Descender,
        key: &[u8],
        value: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        Self::check_row(key, value)?;
        self.modify(pool, d, key, |cell| {
            let old = cell.map(|c| c.value.to_vec());
            Ok((Change::Put(value.to_vec(), page::Flags::default()), old))
        })
    }

    /// Splits the full, latched `w` (page `no`) into it and a new page, inserts `key`'s cell into
    /// whichever half covers it, and returns the separator and the new page for the parent.
    fn split_and_insert(
        &self,
        pool: &Pool,
        w: &mut [u8],
        no: u64,
        key: &[u8],
        value: &[u8],
        flags: page::Flags,
    ) -> Result<Split> {
        let cmp = self.compare();
        let right_no = pool.allocate()?;
        let mut right = pool.write(right_no)?;
        let separator = page::split(w, &mut right, right_no)?;
        let target: &mut [u8] = if cmp(key, &separator) == Order::Less {
            w
        } else {
            &mut right
        };
        let at = match page::search(target, key, cmp)? {
            Ok(i) | Err(i) => i,
        };
        if !page::insert_flagged(target, at, key, Tail::Value(value), flags)? {
            return Err(LimboError::Corrupt(
                "versioned storage: half a split page without room for its cell".into(),
            ));
        }
        let level = page::level(w)?;
        Ok(Split {
            left: no,
            right: right_no,
            separator,
            level,
        })
    }

    /// Posts a split to the level above: the separator and the new page into the parent, which may
    /// split in turn, and so on up; a split of the root makes a new root.
    fn post(&self, pool: &Pool, d: &mut Descender, first: Split, path: &[u64]) -> Result<()> {
        let cmp = self.compare();
        let mut split = first;
        let mut levels = path.iter();
        for _ in 0..MAX_LEVELS {
            let parent = match levels.next() {
                Some(&p) => p,
                // The tree grew since the descent: the parent is found from the root, which is
                // above this split's page (a split root grows before its latch goes).
                None => self.parent_of(pool, d, &split)?,
            };
            let mut node = parent;
            let mut moves = 0u64;
            let next = loop {
                let mut w = pool.write(node)?;
                if page::beyond(&w, &split.separator, cmp)? {
                    node = step_right(pool, &mut moves, node, page::right(&w)?)?;
                    continue;
                }
                let at = match page::search(&w, &split.separator, cmp)? {
                    Ok(i) | Err(i) => i,
                };
                if page::insert(&mut w, at, &split.separator, Tail::Child(split.right))? {
                    return Ok(());
                }
                let next = self.split_inner(pool, &mut w, node, &split)?;
                if self.grow(pool, &next)? {
                    drop(w);
                    #[cfg(test)]
                    self.pause_after_root_split();
                    return Ok(());
                }
                break next;
            };
            split = next;
        }
        Err(too_tall())
    }

    /// Splits the full inner page `w` (page `no`) and posts `child`'s separator into its half.
    fn split_inner(&self, pool: &Pool, w: &mut [u8], no: u64, child: &Split) -> Result<Split> {
        let cmp = self.compare();
        let right_no = pool.allocate()?;
        let mut right = pool.write(right_no)?;
        let separator = page::split(w, &mut right, right_no)?;
        let target: &mut [u8] = if cmp(&child.separator, &separator) == Order::Less {
            w
        } else {
            &mut right
        };
        let at = match page::search(target, &child.separator, cmp)? {
            Ok(i) | Err(i) => i,
        };
        if !page::insert(target, at, &child.separator, Tail::Child(child.right))? {
            return Err(LimboError::Corrupt(
                "versioned storage: half a split inner page without room for its cell".into(),
            ));
        }
        let level = page::level(w)?;
        Ok(Split {
            left: no,
            right: right_no,
            separator,
            level,
        })
    }

    /// Makes a new root over `split` if its page is the root: true once made. Called under the
    /// latch of the page that split, so the root it compares with can't change meanwhile: only
    /// this, under the root's latch, replaces the root.
    fn grow(&self, pool: &Pool, split: &Split) -> Result<bool> {
        if self.root() != split.left {
            return Ok(false);
        }
        let level = split.level.checked_add(1).ok_or_else(too_tall)?;
        if usize::from(level) >= MAX_LEVELS {
            return Err(too_tall());
        }
        let root = pool.allocate()?;
        {
            let mut w = pool.write(root)?;
            page::init(&mut w, Kind::Inner, split.left)?;
            page::set_level(&mut w, level)?;
            if !page::insert(&mut w, 0, &split.separator, Tail::Child(split.right))? {
                return Err(LimboError::Corrupt(
                    "versioned storage: a new root without room for one cell".into(),
                ));
            }
        }
        self.root.store(root, Ordering::Release);
        Ok(true)
    }

    /// The page one level above `split`'s pages whose keys include its separator, found from the
    /// root.
    fn parent_of(&self, pool: &Pool, d: &mut Descender, split: &Split) -> Result<u64> {
        let cmp = self.compare();
        let want = split.level.checked_add(1).ok_or_else(too_tall)?;
        let mut page = self.root();
        for _ in 0..MAX_LEVELS {
            let image = d.image(pool, page)?;
            let level = page::level(image)?;
            if level == want {
                return Ok(page);
            }
            if level < want {
                break;
            }
            page = page::route(image, &split.separator, cmp)?;
        }
        Err(LimboError::Corrupt(
            "versioned storage: no level above a split page".into(),
        ))
    }

    /// Removes `key`: the value it held, if any. Pages emptied stay (they are never merged).
    pub fn remove(&self, pool: &Pool, d: &mut Descender, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.modify(pool, d, key, |cell| {
            let old = cell.map(|c| c.value.to_vec());
            Ok((Change::Remove, old))
        })
    }

    /// `f` on each key and cell from `from` on (from the least key with none), in order, each
    /// leaf's under its shared latch, until `f` says stop (false).
    pub fn scan_with(
        &self,
        pool: &Pool,
        d: &mut Descender,
        from: Option<&[u8]>,
        mut f: impl FnMut(&[u8], Cell<'_>) -> Result<bool>,
    ) -> Result<()> {
        let cmp = self.compare();
        let mut leaf = match from {
            Some(key) => self.descend(pool, d, key)?.0,
            None => self.leftmost(pool, d)?,
        };
        let mut moves = 0u64;
        loop {
            let r = pool.read(leaf)?;
            for i in 0..page::count(&r)? {
                let k = page::key(&r, i)?;
                if from.is_some_and(|f| cmp(k, f) == Order::Less) {
                    continue;
                }
                let cell = Cell {
                    flags: page::flags(&r, i)?,
                    value: page::value(&r, i)?,
                };
                if !f(k, cell)? {
                    return Ok(());
                }
            }
            match page::right(&r)? {
                Some(next) => leaf = step_right(pool, &mut moves, leaf, Some(next))?,
                None => return Ok(()),
            }
        }
    }

    /// Up to `most` keys and values from `from` on (from the least key with none), in order.
    pub fn scan(
        &self,
        pool: &Pool,
        d: &mut Descender,
        from: Option<&[u8]>,
        most: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = Vec::new();
        if most == 0 {
            return Ok(out);
        }
        self.scan_with(pool, d, from, |k, cell| {
            out.push((k.to_vec(), cell.value.to_vec()));
            Ok(out.len() < most)
        })?;
        Ok(out)
    }

    /// Clears every cell's versions and drops removed rows' cells (recovery: no snapshot outlives
    /// the process, decision 8). A leaf is written only if a cell of it needs this, so leaves
    /// without one stay clean; recovery runs alone, so none changes between the read and the write.
    pub fn clear_versions(&self, pool: &Pool, d: &mut Descender) -> Result<()> {
        let mut leaf = self.leftmost(pool, d)?;
        let mut moves = 0u64;
        loop {
            let (flagged, right) = {
                let r = pool.read(leaf)?;
                let mut flagged = false;
                for i in 0..page::count(&r)? {
                    let f = page::flags(&r, i)?;
                    flagged |= f.versioned || f.removed;
                }
                (flagged, page::right(&r)?)
            };
            if flagged {
                let mut w = pool.write(leaf)?;
                // Each turn removes a cell or moves past one.
                let mut i = 0usize;
                while i < page::count(&w)? {
                    let f = page::flags(&w, i)?;
                    if f.removed {
                        page::remove(&mut w, i)?;
                        continue;
                    }
                    if f.versioned {
                        page::set_flags(&mut w, i, page::Flags::default())?;
                    }
                    i = i.checked_add(1).ok_or(LimboError::IntegerOverflow)?;
                }
            }
            match right {
                Some(next) => leaf = step_right(pool, &mut moves, leaf, Some(next))?,
                None => return Ok(()),
            }
        }
    }

    /// The leftmost leaf.
    fn leftmost(&self, pool: &Pool, d: &mut Descender) -> Result<u64> {
        let mut page = self.root();
        for _ in 0..MAX_LEVELS {
            let image = d.image(pool, page)?;
            if page::level(image)? == 0 {
                return Ok(page);
            }
            page = page::first(image)?;
        }
        Err(too_tall())
    }
}

#[cfg(test)]
impl Tree {
    /// Pauses the next split of the root once the root's latch is released: a receiver that hears
    /// when it has, and a sender that lets it go on.
    fn pause_next_root_split(
        &self,
    ) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (entered, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release) = std::sync::mpsc::channel();
        *self.pause.lock() = Some(Pause { entered, release });
        (entered_rx, release_tx)
    }

    fn pause_after_root_split(&self) {
        let pause = self.pause.lock().take();
        if let Some(pause) = pause {
            pause.entered.send(()).ok();
            pause.release.recv().ok();
        }
    }
}

/// A key's cell as a tree hands it to a callback: its flags and its value, in the latched page.
#[derive(Clone, Copy, Debug)]
pub struct Cell<'a> {
    pub flags: page::Flags,
    pub value: &'a [u8],
}

/// What [`Tree::modify`] makes of a key's cell.
#[derive(Debug)]
pub enum Change {
    /// Leaves it as it is.
    Keep,
    /// Puts this value with these flags (the cell made if there was none).
    Put(Vec<u8>, page::Flags),
    /// Sets the cell's flags.
    Mark(page::Flags),
    /// Removes the cell.
    Remove,
}

/// A split: the page that split, the page it split into, the key dividing them and their level.
struct Split {
    left: u64,
    right: u64,
    separator: Vec<u8>,
    level: u8,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::Arc;
    use std::collections::BTreeMap;

    fn bytes() -> Comparator {
        Box::new(|a: &[u8], b: &[u8]| a.cmp(b))
    }

    fn key(n: u64) -> Vec<u8> {
        n.to_be_bytes().to_vec()
    }

    /// Do: put, replace and remove 20,000 random keys with values of random sizes in one tree.
    /// Expect: every key's value as a sorted map's, after each step for the key and at the end for
    /// all of them in order by a scan; the tree several levels tall.
    #[test]
    fn one_writer_keeps_the_tree_as_a_sorted_map() {
        let pool = Pool::new(20_000);
        let tree = Tree::create(&pool, bytes()).unwrap();
        let mut d = Descender::new();
        let mut model = BTreeMap::new();
        let mut seed = 11u64;
        for _ in 0..20_000 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let k = key((seed >> 32) % 8_000);
            let v = vec![
                u8::try_from((seed >> 8) & 0xff).unwrap();
                usize::try_from((seed >> 16) % 200).unwrap()
            ];
            if (seed >> 60).is_multiple_of(4) {
                assert_eq!(tree.remove(&pool, &mut d, &k).unwrap(), model.remove(&k));
            } else {
                assert_eq!(
                    tree.put(&pool, &mut d, &k, &v).unwrap(),
                    model.insert(k.clone(), v)
                );
            }
            assert_eq!(tree.get(&pool, &mut d, &k).unwrap(), model.get(&k).cloned());
        }
        let all = tree.scan(&pool, &mut d, None, usize::MAX).unwrap();
        assert_eq!(all, model.into_iter().collect::<Vec<_>>());
        let root = pool.read(tree.root()).unwrap();
        assert!(
            page::level(&root).unwrap() >= 2,
            "a tree of one or two levels"
        );
    }

    /// Do: 32 threads put disjoint keys at once (interleaved, so they meet in the same leaves),
    /// each reading back what it put, while 4 threads read keys at random, five times over a fresh
    /// tree. Expect: every key put readable by every thread after, a scan in order holding exactly
    /// them; no reader ever finds a key with another key's value, and none walks right into an
    /// error while splits add pages under it (a bound on its moves fixed when it began did).
    #[test]
    fn many_writers_and_readers_at_once_lose_nothing() {
        for _ in 0..5 {
            many_at_once();
        }
    }

    fn many_at_once() {
        let pool = Arc::new(Pool::new(40_000));
        let tree = Arc::new(Tree::create(&pool, bytes()).unwrap());
        let writers = 32u64;
        let each = 1_500u64;
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let readers: Vec<_> = (0..4)
            .map(|r| {
                let (pool, tree, done) = (pool.clone(), tree.clone(), done.clone());
                std::thread::spawn(move || {
                    let mut d = Descender::new();
                    let mut seed = 99u64 + r;
                    while !done.load(std::sync::atomic::Ordering::Relaxed) {
                        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                        let n = (seed >> 33) % (writers * each);
                        if let Some(v) = tree.get(&pool, &mut d, &key(n)).unwrap() {
                            assert_eq!(v, key(n), "key {n} with another's value");
                        }
                    }
                })
            })
            .collect();
        let threads: Vec<_> = (0..writers)
            .map(|w| {
                let (pool, tree) = (pool.clone(), tree.clone());
                std::thread::spawn(move || {
                    let mut d = Descender::new();
                    for i in 0..each {
                        let n = i * writers + w;
                        tree.put(&pool, &mut d, &key(n), &key(n)).unwrap();
                        assert_eq!(tree.get(&pool, &mut d, &key(n)).unwrap(), Some(key(n)));
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        for r in readers {
            r.join().unwrap();
        }
        let mut d = Descender::new();
        for n in 0..writers * each {
            assert_eq!(
                tree.get(&pool, &mut d, &key(n)).unwrap(),
                Some(key(n)),
                "key {n}"
            );
        }
        let all = tree.scan(&pool, &mut d, None, usize::MAX).unwrap();
        assert_eq!(u64::try_from(all.len()).unwrap(), writers * each);
        assert!(all.windows(2).all(|w| w[0].0 < w[1].0));
    }

    /// Do: split the root, a leaf and then an inner page, with the splitter paused once the root's
    /// latch is released, and meanwhile put keys past the split until the root's new right page
    /// splits too. Expect: every put done and every key read back after: the level above the new
    /// page exists before another writer reaches it. (Under load, `many_at_once`'s writers met
    /// this: one splitting that page while the splitter of the root was descheduled found no level
    /// above it, Corrupt "no level above a split page".) Many writers at once: AC-T1.3.1.
    #[test]
    fn a_root_split_is_whole_before_another_writer_reaches_its_new_page() {
        for level in [0u8, 1] {
            root_split_raced(level);
        }
    }

    /// Of keys 0, 1, ... put in order into a new tree, the first whose put splits a root of `level`.
    fn root_split_at(level: u8) -> u64 {
        let pool = Pool::new(4_000);
        let tree = Tree::create(&pool, bytes()).unwrap();
        let mut d = Descender::new();
        (0..100_000u64)
            .find(|&n| {
                tree.put(&pool, &mut d, &key(n), &key(n)).unwrap();
                page::level(&pool.read(tree.root()).unwrap()).unwrap() > level
            })
            .unwrap()
    }

    fn root_split_raced(level: u8) {
        let n = root_split_at(level);
        let pool = Arc::new(Pool::new(4_000));
        let tree = Arc::new(Tree::create(&pool, bytes()).unwrap());
        let mut d = Descender::new();
        for k in 0..n {
            tree.put(&pool, &mut d, &key(k), &key(k)).unwrap();
        }
        let (entered, release) = tree.pause_next_root_split();
        let splitter = {
            let (pool, tree) = (pool.clone(), tree.clone());
            std::thread::spawn(move || tree.put(&pool, &mut Descender::new(), &key(n), &key(n)))
        };
        entered.recv().unwrap();
        // As many keys again, all past the split's separator: the root's new right page fills and
        // splits while the splitter waits.
        for k in n + 1..=2 * n {
            let put = tree.put(&pool, &mut d, &key(k), &key(k));
            assert!(
                put.is_ok(),
                "key {k} put while a root of level {level} split: {put:?}"
            );
        }
        release.send(()).unwrap();
        assert!(splitter.join().unwrap().is_ok());
        for k in 0..=2 * n {
            assert_eq!(
                tree.get(&pool, &mut d, &key(k)).unwrap(),
                Some(key(k)),
                "key {k}"
            );
        }
        let all = tree.scan(&pool, &mut d, None, usize::MAX).unwrap();
        assert_eq!(u64::try_from(all.len()).unwrap(), 2 * n + 1);
        assert!(all.windows(2).all(|w| w[0].0 < w[1].0));
    }

    /// Do: a row larger than a page can hold a quarter of. Expect: `TooBig`, the tree unchanged.
    #[test]
    fn a_row_too_large_for_a_page_is_refused() {
        let pool = Pool::new(10);
        let tree = Tree::create(&pool, bytes()).unwrap();
        let mut d = Descender::new();
        let big = vec![0u8; page::MAX_CELL];
        assert!(matches!(
            tree.put(&pool, &mut d, b"k", &big),
            Err(LimboError::TooBig)
        ));
        assert_eq!(tree.scan(&pool, &mut d, None, 10).unwrap(), vec![]);
    }
}

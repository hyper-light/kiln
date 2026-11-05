//! The lock census (goal 5 of the API runtime program, plan 01a1a59c2aae step 1): every `Mutex` and
//! `RwLock` a crate takes, counted at the place it's taken. Never part of kiln's own build:
//! `scripts/locks.py census` copies this file into a scratch copy of the tree as each instrumented
//! crate's `census` module and rewrites the crate's `parking_lot::{Mutex, RwLock}` and
//! `std::sync::{Mutex, RwLock}` to the wrappers below (`pl` and `st`), which hand back the same
//! guards (so `Condvar`s and guard maps work as before). Each acquisition is counted by its call
//! site (`#[track_caller]`), with whether the lock was already held and how long it then waited, and
//! the first few addresses of the locks taken there (sites that take one lock share its address).
//! `/metrics` says it all as `kiln_lock_*` series ([`render`]), which kiln-lab keeps for each round's
//! window and fill (`Round::locks`), and `scripts/locks.py rank` ranks.
//!
//! The table is a fixed array of atomics, claimed a slot per site by compare-and-swap: counting takes
//! no lock of its own.

#![allow(dead_code)]

use std::panic::Location;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

/// Call sites the table holds; past it, sites go uncounted (`kiln_lock_sites_full_total`).
const SLOTS: usize = 1 << 14;
/// Addresses kept per site.
const ADDRS: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// `parking_lot::Mutex::lock`, `try_lock`.
    PlMutex = 1,
    PlRead = 2,
    PlWrite = 3,
    /// `std::sync::Mutex::lock`, `try_lock`.
    StdMutex = 4,
    StdRead = 5,
    StdWrite = 6,
    /// A call of kiln's store (`Store::call`), at its caller: how many, and how long each took (its
    /// wait for the store's one connection and its run), as `wait`.
    StoreCall = 7,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::PlMutex => "mutex",
            Kind::PlRead => "read",
            Kind::PlWrite => "write",
            Kind::StdMutex => "std_mutex",
            Kind::StdRead => "std_read",
            Kind::StdWrite => "std_write",
            Kind::StoreCall => "store_call",
        }
    }
}

/// A store call ended: counted at its caller, with how long it took.
pub fn store_call(at: &'static Location<'static>, took: std::time::Duration) {
    let Some(s) = slot(at, Kind::StoreCall) else { return };
    let ns = u64::try_from(took.as_nanos()).unwrap_or(u64::MAX);
    s.taken.fetch_add(1, Ordering::Relaxed);
    s.wait_ns.fetch_add(ns, Ordering::Relaxed);
    s.max_wait_ns.fetch_max(ns, Ordering::Relaxed);
}

struct Slot {
    /// The site's location's address with its kind, `0` while free.
    key: AtomicUsize,
    site: OnceLock<(&'static Location<'static>, Kind)>,
    taken: AtomicU64,
    /// Acquisitions that found it held (and waited), and `try_` calls it refused.
    contended: AtomicU64,
    wait_ns: AtomicU64,
    max_wait_ns: AtomicU64,
    addrs: [AtomicUsize; ADDRS],
}

impl Slot {
    const fn new() -> Self {
        Self {
            key: AtomicUsize::new(0),
            site: OnceLock::new(),
            taken: AtomicU64::new(0),
            contended: AtomicU64::new(0),
            wait_ns: AtomicU64::new(0),
            max_wait_ns: AtomicU64::new(0),
            addrs: [const { AtomicUsize::new(0) }; ADDRS],
        }
    }
}

static TABLE: [Slot; SLOTS] = [const { Slot::new() }; SLOTS];
static FULL: AtomicU64 = AtomicU64::new(0);

fn slot(at: &'static Location<'static>, kind: Kind) -> Option<&'static Slot> {
    let key = ((at as *const Location<'static> as usize) << 3) | kind as usize;
    let mut i = (key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 7) & (SLOTS - 1);
    for _ in 0..SLOTS {
        let s = TABLE.get(i)?;
        let k = s.key.load(Ordering::Acquire);
        if k == key {
            return Some(s);
        }
        if k == 0 {
            match s.key.compare_exchange(0, key, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => {
                    let _ = s.site.set((at, kind));
                    return Some(s);
                }
                Err(now) if now == key => return Some(s),
                Err(_) => {}
            }
        }
        i = (i + 1) & (SLOTS - 1);
    }
    FULL.fetch_add(1, Ordering::Relaxed);
    None
}

fn took(at: &'static Location<'static>, kind: Kind, addr: usize, waited: Option<Instant>) {
    let Some(s) = slot(at, kind) else { return };
    s.taken.fetch_add(1, Ordering::Relaxed);
    if let Some(began) = waited {
        let ns = u64::try_from(began.elapsed().as_nanos()).unwrap_or(u64::MAX);
        s.contended.fetch_add(1, Ordering::Relaxed);
        s.wait_ns.fetch_add(ns, Ordering::Relaxed);
        s.max_wait_ns.fetch_max(ns, Ordering::Relaxed);
    }
    for a in &s.addrs {
        let v = a.load(Ordering::Relaxed);
        if v == addr {
            break;
        }
        if v == 0 {
            match a.compare_exchange(0, addr, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(now) if now == addr => break,
                Err(_) => {}
            }
        }
    }
}

fn refused(at: &'static Location<'static>, kind: Kind) {
    if let Some(s) = slot(at, kind) {
        s.contended.fetch_add(1, Ordering::Relaxed);
    }
}

fn address<T: ?Sized>(r: &T) -> usize {
    r as *const T as *const () as usize
}

/// The census as Prometheus text: a series of each kind per site.
pub fn render(o: &mut String) {
    use std::fmt::Write as _;
    let _ = writeln!(o, "# TYPE kiln_lock_taken_total counter");
    for s in &TABLE {
        let Some((at, kind)) = s.site.get() else { continue };
        let labels = format!("site=\"{}:{}:{}\",kind=\"{}\"", at.file().replace('\\', "/"), at.line(), at.column(), kind.name());
        let addrs: Vec<String> = s.addrs.iter().map(|a| a.load(Ordering::Relaxed)).filter(|a| *a != 0).map(|a| format!("{a:x}")).collect();
        let _ = writeln!(o, "kiln_lock_taken_total{{{labels}}} {}", s.taken.load(Ordering::Relaxed));
        let _ = writeln!(o, "kiln_lock_contended_total{{{labels}}} {}", s.contended.load(Ordering::Relaxed));
        #[allow(clippy::cast_precision_loss)]
        let _ = writeln!(o, "kiln_lock_wait_seconds_total{{{labels}}} {}", s.wait_ns.load(Ordering::Relaxed) as f64 / 1e9);
        #[allow(clippy::cast_precision_loss)]
        let _ = writeln!(o, "kiln_lock_wait_max_seconds{{{labels}}} {}", s.max_wait_ns.load(Ordering::Relaxed) as f64 / 1e9);
        let _ = writeln!(o, "kiln_lock_addresses{{{labels},addrs=\"{}\"}} 1", addrs.join(" "));
    }
    let _ = writeln!(o, "kiln_lock_sites_full_total {}", FULL.load(Ordering::Relaxed));
}

// census:parking_lot (a crate without parking_lot gets the file without what follows, to the mark's end)
/// `parking_lot`'s locks, counted.
pub mod pl {
    use super::{Kind, Location, address, refused, took};
    use std::time::Instant;

    pub struct Mutex<T: ?Sized>(parking_lot::Mutex<T>);

    impl<T> Mutex<T> {
        pub const fn new(value: T) -> Self {
            Self(parking_lot::Mutex::new(value))
        }

        pub fn into_inner(self) -> T {
            self.0.into_inner()
        }
    }

    impl<T: ?Sized> Mutex<T> {
        #[track_caller]
        pub fn lock(&self) -> parking_lot::MutexGuard<'_, T> {
            let at = Location::caller();
            if let Some(g) = self.0.try_lock() {
                took(at, Kind::PlMutex, address(self), None);
                return g;
            }
            let began = Instant::now();
            let g = self.0.lock();
            took(at, Kind::PlMutex, address(self), Some(began));
            g
        }

        #[track_caller]
        pub fn try_lock(&self) -> Option<parking_lot::MutexGuard<'_, T>> {
            let at = Location::caller();
            let g = self.0.try_lock();
            match &g {
                Some(_) => took(at, Kind::PlMutex, address(self), None),
                None => refused(at, Kind::PlMutex),
            }
            g
        }
    }

    impl<T: ?Sized> std::ops::Deref for Mutex<T> {
        type Target = parking_lot::Mutex<T>;
        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl<T: ?Sized> std::ops::DerefMut for Mutex<T> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.0
        }
    }

    impl<T: Default> Default for Mutex<T> {
        fn default() -> Self {
            Self::new(T::default())
        }
    }

    impl<T> From<T> for Mutex<T> {
        fn from(value: T) -> Self {
            Self::new(value)
        }
    }

    impl<T: ?Sized + std::fmt::Debug> std::fmt::Debug for Mutex<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.0.fmt(f)
        }
    }

    pub struct RwLock<T: ?Sized>(parking_lot::RwLock<T>);

    impl<T> RwLock<T> {
        pub const fn new(value: T) -> Self {
            Self(parking_lot::RwLock::new(value))
        }

        pub fn into_inner(self) -> T {
            self.0.into_inner()
        }
    }

    impl<T: ?Sized> RwLock<T> {
        #[track_caller]
        pub fn read(&self) -> parking_lot::RwLockReadGuard<'_, T> {
            let at = Location::caller();
            if let Some(g) = self.0.try_read() {
                took(at, Kind::PlRead, address(self), None);
                return g;
            }
            let began = Instant::now();
            let g = self.0.read();
            took(at, Kind::PlRead, address(self), Some(began));
            g
        }

        #[track_caller]
        pub fn write(&self) -> parking_lot::RwLockWriteGuard<'_, T> {
            let at = Location::caller();
            if let Some(g) = self.0.try_write() {
                took(at, Kind::PlWrite, address(self), None);
                return g;
            }
            let began = Instant::now();
            let g = self.0.write();
            took(at, Kind::PlWrite, address(self), Some(began));
            g
        }

        #[track_caller]
        pub fn try_read(&self) -> Option<parking_lot::RwLockReadGuard<'_, T>> {
            let at = Location::caller();
            let g = self.0.try_read();
            match &g {
                Some(_) => took(at, Kind::PlRead, address(self), None),
                None => refused(at, Kind::PlRead),
            }
            g
        }

        #[track_caller]
        pub fn try_write(&self) -> Option<parking_lot::RwLockWriteGuard<'_, T>> {
            let at = Location::caller();
            let g = self.0.try_write();
            match &g {
                Some(_) => took(at, Kind::PlWrite, address(self), None),
                None => refused(at, Kind::PlWrite),
            }
            g
        }
    }

    impl<T: ?Sized> std::ops::Deref for RwLock<T> {
        type Target = parking_lot::RwLock<T>;
        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl<T: ?Sized> std::ops::DerefMut for RwLock<T> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.0
        }
    }

    impl<T: Default> Default for RwLock<T> {
        fn default() -> Self {
            Self::new(T::default())
        }
    }

    impl<T> From<T> for RwLock<T> {
        fn from(value: T) -> Self {
            Self::new(value)
        }
    }

    impl<T: ?Sized + std::fmt::Debug> std::fmt::Debug for RwLock<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.0.fmt(f)
        }
    }
}
// census:end

/// The standard library's locks, counted.
pub mod st {
    use super::{Kind, Location, address, refused, took};
    use std::sync::{LockResult, MutexGuard, RwLockReadGuard, RwLockWriteGuard, TryLockError, TryLockResult};
    use std::time::Instant;

    pub struct Mutex<T: ?Sized>(std::sync::Mutex<T>);

    impl<T> Mutex<T> {
        pub const fn new(value: T) -> Self {
            Self(std::sync::Mutex::new(value))
        }

        pub fn into_inner(self) -> LockResult<T> {
            self.0.into_inner()
        }
    }

    impl<T: ?Sized> Mutex<T> {
        #[track_caller]
        pub fn lock(&self) -> LockResult<MutexGuard<'_, T>> {
            let at = Location::caller();
            match self.0.try_lock() {
                Ok(g) => {
                    took(at, Kind::StdMutex, address(self), None);
                    Ok(g)
                }
                Err(TryLockError::Poisoned(p)) => {
                    took(at, Kind::StdMutex, address(self), None);
                    Err(p)
                }
                Err(TryLockError::WouldBlock) => {
                    let began = Instant::now();
                    let g = self.0.lock();
                    took(at, Kind::StdMutex, address(self), Some(began));
                    g
                }
            }
        }

        #[track_caller]
        pub fn try_lock(&self) -> TryLockResult<MutexGuard<'_, T>> {
            let at = Location::caller();
            let g = self.0.try_lock();
            match &g {
                Err(TryLockError::WouldBlock) => refused(at, Kind::StdMutex),
                _ => took(at, Kind::StdMutex, address(self), None),
            }
            g
        }
    }

    impl<T: ?Sized> std::ops::Deref for Mutex<T> {
        type Target = std::sync::Mutex<T>;
        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl<T: ?Sized> std::ops::DerefMut for Mutex<T> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.0
        }
    }

    impl<T: Default> Default for Mutex<T> {
        fn default() -> Self {
            Self::new(T::default())
        }
    }

    impl<T> From<T> for Mutex<T> {
        fn from(value: T) -> Self {
            Self::new(value)
        }
    }

    impl<T: ?Sized + std::fmt::Debug> std::fmt::Debug for Mutex<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.0.fmt(f)
        }
    }

    pub struct RwLock<T: ?Sized>(std::sync::RwLock<T>);

    impl<T> RwLock<T> {
        pub const fn new(value: T) -> Self {
            Self(std::sync::RwLock::new(value))
        }

        pub fn into_inner(self) -> LockResult<T> {
            self.0.into_inner()
        }
    }

    impl<T: ?Sized> RwLock<T> {
        #[track_caller]
        pub fn read(&self) -> LockResult<RwLockReadGuard<'_, T>> {
            let at = Location::caller();
            match self.0.try_read() {
                Ok(g) => {
                    took(at, Kind::StdRead, address(self), None);
                    Ok(g)
                }
                Err(TryLockError::Poisoned(p)) => {
                    took(at, Kind::StdRead, address(self), None);
                    Err(p)
                }
                Err(TryLockError::WouldBlock) => {
                    let began = Instant::now();
                    let g = self.0.read();
                    took(at, Kind::StdRead, address(self), Some(began));
                    g
                }
            }
        }

        #[track_caller]
        pub fn write(&self) -> LockResult<RwLockWriteGuard<'_, T>> {
            let at = Location::caller();
            match self.0.try_write() {
                Ok(g) => {
                    took(at, Kind::StdWrite, address(self), None);
                    Ok(g)
                }
                Err(TryLockError::Poisoned(p)) => {
                    took(at, Kind::StdWrite, address(self), None);
                    Err(p)
                }
                Err(TryLockError::WouldBlock) => {
                    let began = Instant::now();
                    let g = self.0.write();
                    took(at, Kind::StdWrite, address(self), Some(began));
                    g
                }
            }
        }

        #[track_caller]
        pub fn try_read(&self) -> TryLockResult<RwLockReadGuard<'_, T>> {
            let at = Location::caller();
            let g = self.0.try_read();
            match &g {
                Err(TryLockError::WouldBlock) => refused(at, Kind::StdRead),
                _ => took(at, Kind::StdRead, address(self), None),
            }
            g
        }

        #[track_caller]
        pub fn try_write(&self) -> TryLockResult<RwLockWriteGuard<'_, T>> {
            let at = Location::caller();
            let g = self.0.try_write();
            match &g {
                Err(TryLockError::WouldBlock) => refused(at, Kind::StdWrite),
                _ => took(at, Kind::StdWrite, address(self), None),
            }
            g
        }
    }

    impl<T: ?Sized> std::ops::Deref for RwLock<T> {
        type Target = std::sync::RwLock<T>;
        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl<T: ?Sized> std::ops::DerefMut for RwLock<T> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.0
        }
    }

    impl<T: Default> Default for RwLock<T> {
        fn default() -> Self {
            Self::new(T::default())
        }
    }

    impl<T> From<T> for RwLock<T> {
        fn from(value: T) -> Self {
            Self::new(value)
        }
    }

    impl<T: ?Sized + std::fmt::Debug> std::fmt::Debug for RwLock<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.0.fmt(f)
        }
    }
}

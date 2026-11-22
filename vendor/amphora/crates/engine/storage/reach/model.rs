//! G7 step 3's model test: each way to reach a page, under 32 threads and a pool a tenth of the
//! file, returns the pages a reference map says it holds: no stale page, no torn page, no page
//! read from a frame after it was evicted to hold another.

use super::direct::Direct;
use super::swizzle::Swizzle;
use super::table::Table;
use super::{Reach, Store, WORDS};
use crate::error::LimboError;
use crate::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// The plan's 32 clients.
const THREADS: u64 = 32;
const PAGES: u64 = 2048;
/// A tenth of the file (plan step 4's pool), so most reads miss and evict.
const FRAMES: usize = (PAGES / 10) as usize;
const OPS: u64 = 3000;

/// Word `i` of page `no` at generation `g`: word 0 the page, word 1 the generation, the rest a
/// function of all three, so a page put together from two writes or two pages doesn't check.
fn word(no: u64, g: u64, i: usize) -> u64 {
    match i {
        0 => no,
        1 => g,
        _ => {
            let mut z = no ^ g.rotate_left(24) ^ (i as u64).rotate_left(48);
            z = (z ^ (z >> 31)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z ^ (z >> 29)
        }
    }
}

fn fill(no: u64, w: &mut [u64]) {
    for (i, x) in w.iter_mut().enumerate() {
        *x = word(no, 0, i);
    }
}

/// What a read of a page saw: its page number, its generation, and whether every word agrees.
fn look(w: &[AtomicU64]) -> (u64, u64, bool) {
    let no = w[0].load(Ordering::Relaxed);
    let g = w[1].load(Ordering::Relaxed);
    let whole = w
        .iter()
        .enumerate()
        .all(|(i, x)| x.load(Ordering::Relaxed) == word(no, g, i));
    (no, g, whole)
}

fn next(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

/// 32 threads, each writing only its own pages (page `n` is thread `(n - 1) % 32`'s) and reading
/// any: its own pages at exactly the generation it last wrote, every other page whole, as the page
/// asked for, and never older than it saw that page before. Then every page, read alone, is the
/// generation the reference map holds.
fn model<P: Reach>(make: impl Fn(Arc<Store>) -> P) {
    let store = Arc::new(Store::new(PAGES, fill).unwrap());
    let pool = make(store);
    let maps: Vec<Vec<u64>> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..THREADS)
            .map(|t| {
                let pool = &pool;
                s.spawn(move || {
                    let mut c = pool.client();
                    // The reference map of this thread's own pages, and the generation it last saw
                    // of every page.
                    let mut mine = vec![0u64; PAGES as usize + 1];
                    let mut seen = vec![0u64; PAGES as usize + 1];
                    let mut x = t + 1;
                    for _ in 0..OPS {
                        let r = next(&mut x);
                        if r.is_multiple_of(4) {
                            let no = (r / 4 % (PAGES / THREADS)) * THREADS + t + 1;
                            let g = mine[no as usize] + 1;
                            pool.write(&mut c, no, |w| {
                                for (i, x) in w.iter().enumerate() {
                                    x.store(word(no, g, i), Ordering::Relaxed);
                                }
                            })
                            .unwrap();
                            mine[no as usize] = g;
                        } else {
                            let no = r % PAGES + 1;
                            let (got, g, whole) = pool.read(&mut c, no, look).unwrap();
                            assert_eq!(
                                got,
                                no,
                                "{}: page {no} read another page's frame",
                                pool.name()
                            );
                            assert!(whole, "{}: page {no} torn at generation {g}", pool.name());
                            if (no - 1) % THREADS == t {
                                assert_eq!(
                                    g,
                                    mine[no as usize],
                                    "{}: page {no} stale for its writer",
                                    pool.name()
                                );
                            } else {
                                assert!(
                                    g >= seen[no as usize],
                                    "{}: page {no} went back from {} to {g}",
                                    pool.name(),
                                    seen[no as usize]
                                );
                            }
                            seen[no as usize] = g;
                        }
                    }
                    pool.release(&mut c);
                    mine
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut c = pool.client();
    for no in 1..=PAGES {
        let t = ((no - 1) % THREADS) as usize;
        let want = maps[t][no as usize];
        let (got, g, whole) = pool.read(&mut c, no, look).unwrap();
        assert!(
            got == no && whole && g == want,
            "{}: page {no} ended at {g} (whole: {whole}), the map says {want}",
            pool.name()
        );
    }
    pool.release(&mut c);
    // Not vacuous: the pool evicted, and wrote pages back and read them again.
    let st = pool.stats();
    assert!(st.evictions > PAGES, "{}: {st:?}", pool.name());
}

/// T-NS.3.1: (a), a sharded page table with private pins, under the model.
#[test]
fn g7_s3_model_table_with_eviction_32_threads() {
    model(|s| Table::new(s, FRAMES).unwrap());
}

/// T-NS.3.1: (b), references by frame index read optimistically, under the model.
#[test]
fn g7_s3_model_swizzle_with_eviction_32_threads() {
    model(|s| Swizzle::new(s, FRAMES).unwrap());
}

/// T-NS.3.1: (c), a direct array of frames by page number, under the model.
#[test]
fn g7_s3_model_direct_with_eviction_32_threads() {
    model(|s| Direct::new(s, FRAMES).unwrap());
}

/// T-NS.3.1: a page outside the file is refused by each, not read.
#[test]
fn g7_s3_pages_outside_the_file_are_refused() {
    fn refuses<P: Reach>(p: P) {
        let mut c = p.client();
        for no in [0, PAGES + 1] {
            assert!(
                matches!(
                    p.read(&mut c, no, |w| w.len()),
                    Err(LimboError::InvalidArgument(_))
                ),
                "{} read page {no}",
                p.name()
            );
            assert!(
                matches!(
                    p.write(&mut c, no, |w| w.len()),
                    Err(LimboError::InvalidArgument(_))
                ),
                "{} wrote page {no}",
                p.name()
            );
        }
        assert_eq!(p.read(&mut c, 1, |w| w.len()).unwrap(), WORDS);
    }
    let s = Arc::new(Store::new(PAGES, fill).unwrap());
    refuses(Table::new(s.clone(), FRAMES).unwrap());
    refuses(Swizzle::new(s.clone(), FRAMES).unwrap());
    refuses(Direct::new(s, FRAMES).unwrap());
}

/// T-NS.3.1: (a) with every frame pinned refuses a page that needs one (`Busy`), and loads it once a pin
/// is let go.
#[test]
fn g7_s3_table_refuses_when_every_frame_is_pinned() {
    let s = Arc::new(Store::new(8, fill).unwrap());
    let t = Table::new(s, 2).unwrap();
    let (mut a, mut b, mut c) = (t.client(), t.client(), t.client());
    t.read(&mut a, 1, look).unwrap();
    t.read(&mut b, 2, look).unwrap();
    assert!(matches!(t.read(&mut c, 3, look), Err(LimboError::Busy)));
    t.release(&mut a);
    assert_eq!(t.read(&mut c, 3, look).unwrap(), (3, 0, true));
}

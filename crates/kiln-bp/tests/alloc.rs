//! bp-g2 step 2, AC: a refused decode allocates at most its input's length, over frames claiming
//! lengths and counts up to u64::MAX. A counting allocator (this test's binary alone) measures
//! every byte asked for while each is read.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use kiln_bp::frames::{Frames, KINDS, Kind};
use kiln_bp::wire::Writer;

struct Counting;

static ASKED: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ASKED.fetch_add(layout.size(), Ordering::Relaxed);
        // SAFETY: passed through to the system allocator with the caller's layout.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: as `alloc`.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new: usize) -> *mut u8 {
        ASKED.fetch_add(new, Ordering::Relaxed);
        // SAFETY: as `alloc`.
        unsafe { System.realloc(ptr, layout, new) }
    }
}

#[global_allocator]
static A: Counting = Counting;

/// The claims: every power of two up to 2^63, u64::MAX, and the frame bound's edges.
fn claims() -> Vec<u64> {
    let mut v: Vec<u64> = (0..64).map(|i| 1u64 << i).collect();
    v.extend([u64::MAX, (1 << 20) + 1, u64::from(u32::MAX)]);
    v
}

#[test]
fn bp_g2_s2_refused_decodes_allocate_at_most_their_input() {
    let mut inputs: Vec<Vec<u8>> = Vec::new();
    for claim in claims() {
        // The envelope's own length.
        let mut w = Writer::new();
        w.varint(u64::from(Kind::Tail.index()));
        w.varint(claim);
        w.buf.extend([0u8; 8]);
        inputs.push(w.buf);
        // A length or count inside each kind's body, at each field it could be: a body of a few
        // bytes whose every position is tried as the claim.
        for k in KINDS {
            for lead in 0..4usize {
                let mut body = vec![1u8; lead];
                let mut c = Writer::new();
                c.varint(claim);
                body.extend(c.buf);
                body.extend([0u8; 4]);
                let mut w = Writer::new();
                w.varint(u64::from(k.index()));
                w.bytes(&body);
                inputs.push(w.buf);
            }
        }
    }
    let mut worst = 0f64;
    let mut refused = 0usize;
    for input in &inputs {
        let before = ASKED.load(Ordering::Relaxed);
        let mut any_refused = false;
        for r in Frames::new(input) {
            any_refused |= r.is_err();
        }
        let asked = ASKED.load(Ordering::Relaxed).saturating_sub(before);
        if any_refused {
            refused += 1;
            let ratio = asked as f64 / input.len() as f64;
            worst = worst.max(ratio);
            assert!(asked <= input.len(), "{asked} bytes asked for reading {} bytes: {input:02x?}", input.len());
        }
    }
    println!("{} hostile inputs, {refused} refused; the most a refused one asked for: {worst:.2}x its length", inputs.len());
    assert!(refused > 1000);
}

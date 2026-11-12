//! Coordinator-owned allocation/latency fixture. It never connects to a server or changes a session.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

use kiln::harness::model::{QueueItem, State, Status};
use kiln::harness::state_view::{self, Read};

struct Measured;
static ACTIVE: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static REALLOCS: AtomicUsize = AtomicUsize::new(0);
static COPIED: AtomicUsize = AtomicUsize::new(0);
static TOTAL: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn allocated(bytes: usize) {
    TOTAL.fetch_add(bytes, Relaxed);
    let now = LIVE.fetch_add(bytes, Relaxed).saturating_add(bytes);
    PEAK.fetch_max(now, Relaxed);
}

// SAFETY: Every operation forwards the unchanged pointer/layout to System. Counters never
// allocate, and the single-threaded fixture measures only objects created and dropped in scope.
unsafe impl GlobalAlloc for Measured {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: GlobalAlloc's caller provides a valid allocation layout.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() && ACTIVE.load(Relaxed) {
            ALLOCS.fetch_add(1, Relaxed);
            allocated(layout.size());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        if ACTIVE.load(Relaxed) {
            LIVE.fetch_sub(layout.size(), Relaxed);
        }
        // SAFETY: GlobalAlloc's caller provides System's live allocation and its original layout.
        unsafe { System.dealloc(p, layout) }
    }
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: GlobalAlloc's caller provides a live allocation, its layout, and a valid new size.
        let next = unsafe { System.realloc(p, layout, size) };
        if !next.is_null() && ACTIVE.load(Relaxed) {
            REALLOCS.fetch_add(1, Relaxed);
            // This is the maximum payload a moving realloc could copy, not observed physical copies.
            COPIED.fetch_add(layout.size().min(size), Relaxed);
            LIVE.fetch_sub(layout.size(), Relaxed);
            allocated(size);
        }
        next
    }
}

#[global_allocator]
static ALLOCATOR: Measured = Measured;

fn measure(
    name: &str,
    sample: usize,
    run: impl FnOnce() -> anyhow::Result<usize>,
) -> anyhow::Result<()> {
    for c in [&ALLOCS, &REALLOCS, &COPIED, &TOTAL, &LIVE, &PEAK] {
        c.store(0, Relaxed);
    }
    ACTIVE.store(true, Relaxed);
    let start = Instant::now();
    let result = run();
    let elapsed = start.elapsed();
    ACTIVE.store(false, Relaxed);
    let bytes = result?;
    println!(
        "{}",
        serde_json::json!({"case":name,"sample":sample,"elapsed_ns":elapsed.as_nanos(),"output_bytes":bytes,
        "allocations":ALLOCS.load(Relaxed),"reallocations":REALLOCS.load(Relaxed),"realloc_copy_upper_bytes":COPIED.load(Relaxed),
        "total_requested_bytes":TOTAL.load(Relaxed),"peak_live_requested_bytes":PEAK.load(Relaxed),"retained_requested_bytes":LIVE.load(Relaxed)})
    );
    Ok(())
}

fn fixture(count: usize, text: &str) -> State {
    State {
        status: Status::Running,
        queue: (0..count)
            .map(|i| QueueItem {
                id: format!("queued-{i}"),
                text: text.to_owned(),
                at: i as i64,
                attachments: Vec::new(),
                overrides: None,
                reply_to: None,
            })
            .collect(),
        ..State::default()
    }
}

fn main() -> anyhow::Result<()> {
    let samples = std::env::args()
        .nth(1)
        .map(|v| v.parse::<usize>())
        .transpose()?
        .unwrap_or(20);
    let text = "same authoritative message ".repeat(100);
    let cases = [
        ("ordinary", fixture(1, "small")),
        ("oversized_bodies", fixture(600, &text)),
        ("oversized_index", fixture(10_000, "small")),
    ];
    for (name, state) in cases {
        let read = Read {
            revision: state_view::field_revision(&state, "queue")?,
            field: "$queue_index".into(),
            path: Vec::new(),
            offset: 0,
        };
        for sample in 0..samples {
            measure(&format!("{name}_unbounded_serialization"), sample, || {
                Ok(serde_json::to_vec(&state)?.len())
            })?;
            measure(&format!("{name}_bounded_view"), sample, || {
                let projected = state_view::view(&state, state_view::FRAME)?.into_state()?;
                Ok(serde_json::to_vec(&projected)?.len())
            })?;
            measure(&format!("{name}_index_page"), sample, || {
                let page = state_view::read(&state, &read, state_view::FRAME)?;
                Ok(serde_json::to_vec(&page)?.len())
            })?;
        }
    }
    Ok(())
}

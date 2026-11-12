//! Microbench for the OCI seccomp compile (CLAUDE.md §7 names the run; this is a microbench, not a
//! class-for-class competitive bench — that A/B against crun belongs on the bench host). The filter
//! compiles on the container create path (`init::become_process`), so its allocations and time are
//! the seccomp contribution to start latency. This reports, for a representative profile and a
//! trivial one, the allocations, bytes and best-of-N wall time of one `seccomp::compile`.
//!
//!   cargo bench -p hex-bench --bench seccomp_compile
//!
//! The representative input is `seccomp_profile.oci.json`: the containers/common default profile
//! (github.com/containers/common, Apache-2.0) rendered to the OCI runtime-spec `linux.seccomp` form
//! (archMap and includes/excludes resolved), 40 groups over 479 syscall names — "Docker's default
//! profile is a good test input".
// A bench (not production, CLAUDE.md): it opts into the allocator harness's `unsafe` (each block
// carries its SAFETY note) and the test-style allowances, as the other benches do.
#![allow(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::disallowed_macros,
    clippy::print_stdout,
    clippy::arithmetic_side_effects,
    clippy::cast_precision_loss
)]

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("SKIP: the seccomp compile bench is Linux-only");
}

#[cfg(target_os = "linux")]
mod counting {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub static ALLOCS: AtomicUsize = AtomicUsize::new(0);
    pub static BYTES: AtomicUsize = AtomicUsize::new(0);

    /// The system allocator, counting allocation calls and the bytes they ask for.
    pub struct Counting;

    // SAFETY: every method forwards to the system allocator with the same arguments; the atomic
    // counters are the only addition and touch no allocation state.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size(), Ordering::Relaxed);
            // SAFETY: forwarding the caller's layout to the system allocator.
            unsafe { System.alloc(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // SAFETY: forwarding the caller's pointer and layout to the system allocator.
            unsafe { System.dealloc(ptr, layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(new_size, Ordering::Relaxed);
            // SAFETY: forwarding the caller's pointer, layout and new size to the system allocator.
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    pub fn snapshot() -> (usize, usize) {
        (
            ALLOCS.load(Ordering::Relaxed),
            BYTES.load(Ordering::Relaxed),
        )
    }
}

#[cfg(target_os = "linux")]
#[global_allocator]
static ALLOC: counting::Counting = counting::Counting;

#[cfg(target_os = "linux")]
fn main() {
    use std::hint::black_box;
    use std::time::{Duration, Instant};

    use hex_runtime::linux::oci::seccomp::compile;
    use hex_runtime::linux::oci::spec::Seccomp;

    let docker: Seccomp = serde_json::from_str(include_str!("seccomp_profile.oci.json"))
        .expect("the representative profile parses");
    let trivial: Seccomp = serde_json::from_str(
        r#"{"defaultAction":"SCMP_ACT_ERRNO","defaultErrnoRet":38,
            "syscalls":[{"names":["read","write","close","exit","exit_group"],"action":"SCMP_ACT_ALLOW"}]}"#,
    )
    .expect("the trivial profile parses");

    for (name, profile) in [("docker-default", &docker), ("trivial", &trivial)] {
        // Warm up (page in, settle), then count one compile's allocations.
        for _ in 0..200 {
            black_box(compile(profile).unwrap());
        }
        let (a0, b0) = counting::snapshot();
        let prog = compile(profile).unwrap();
        let (a1, b1) = counting::snapshot();
        let insns = prog.len();
        black_box(prog);

        let mut best = Duration::MAX;
        let mut sum = Duration::ZERO;
        let iters = 2000;
        for _ in 0..iters {
            let t = Instant::now();
            let p = compile(profile).unwrap();
            let e = t.elapsed();
            best = best.min(e);
            sum += e;
            black_box(p);
        }
        println!(
            "{name:<16} allocs={:<5} bytes={:<8} bpf_insns={insns:<5} best={:>8.3}us mean={:>8.3}us",
            a1 - a0,
            b1 - b0,
            best.as_secs_f64() * 1e6,
            (sum.as_secs_f64() / iters as f64) * 1e6,
        );
    }
}

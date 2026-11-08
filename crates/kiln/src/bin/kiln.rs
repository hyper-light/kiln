//! The single kiln binary: server, agent, supervisor, installer and CLI, all of it the library's (`kiln::entry`).

/// The `jemalloc` feature's allocator, on Linux (goal 11, plan 01a1a87b2e56 step 4): production's, which the release
/// build (ci/release/build.sh) takes by default since the lab's A/B picked it over musl's malloc, by resident memory
/// and latency; its settings, for giving memory back, are `kiln::os::alloc`'s. Elsewhere, and without the feature,
/// the platform's allocator as before.
#[cfg(all(feature = "jemalloc", target_os = "linux"))]
#[global_allocator]
static ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn main() -> std::process::ExitCode {
    kiln::entry::main()
}

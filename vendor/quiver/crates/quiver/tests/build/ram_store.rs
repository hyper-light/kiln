// T-T2.1.2: hyper-durable's RamStore named in a build that isn't a test's: it fails, as RamStore
// is only there for tests (vendor/hyper-raft/UPSTREAM.md).
fn main() {
    let _store = hyper_durable::RamStore::new();
}

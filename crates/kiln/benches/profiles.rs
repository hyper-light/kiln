//! Continuous profiles (`docs/observe.md` §8.11, §12.36): a busy service's profiles as Go writes
//! them (synthetic, their shape a real service's: thousands of functions under a few entry points,
//! stacks 10 to 40 deep sharing their callers, a minute's CPU in tens of thousands of distinct
//! stacks; gzip'd as Go sends them) and Go's own fixtures. Each step of their way, timed (the least
//! of nine, and their median; the CPU charged a run) with its allocations:
//!
//! - an upload read and checked (`intake::read`: taken apart, each profile inflated and read whole,
//!   its types' values summed); a Java upload's recording read into its profiles (kiln-obs's
//!   fixture, dd-trace-java's), repacked as a runner sends it on, and read again;
//! - a profile read (`pprof::read`) and summed into a graph (`Flame::add`);
//! - uploads kept (`Store::put`, a batch synced once) and the store opened again (its header files
//!   read: what a server's start costs), with the bytes its index holds an upload;
//! - a type summed over the uploads a query chooses (`query::flame`), on one thread and on a few;
//! - a graph drawn for the API (`Flame::drawn`, `Flame::by_function`).
//!
//! `cargo bench -p kiln --bench profiles` (`KOBS_DIR`: where the store goes, the system's
//! temporary folder else; `KOBS_UPLOADS`: the store's uploads, 20,000 unless said).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss, clippy::cast_possible_truncation)]

use std::io::Write as _;
use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use hyper_measure::{alloc, usage};
use kiln::observe::profiles::query::{self, Asked};
use kiln::observe::profiles::{intake, store::Store};
use kiln_obs::profile::flame::Flame;
use kiln_obs::profile::pprof;

#[global_allocator]
static COUNTING: alloc::Counting = alloc::Counting;

struct Took(Duration, Duration, Option<Duration>);

fn measure<T>(mut f: impl FnMut() -> T) -> (Took, alloc::Counts) {
    let mut times = Vec::new();
    let mut counted = alloc::Counts::default();
    let cpu = usage::this().ok();
    for n in 0..9 {
        let t = Instant::now();
        if n == 1 {
            alloc::begin();
        }
        std::hint::black_box(f());
        if n == 1 {
            counted = alloc::end();
        }
        times.push(t.elapsed());
    }
    let cpu = cpu.zip(usage::this().ok()).map(|(a, b)| Duration::from_nanos(b.since(&a).cpu_ns() / 9));
    times.sort();
    (Took(times[0], times[times.len() / 2], cpu), counted)
}

fn line(what: &str, t: &Took, a: &alloc::Counts, more: &str) {
    let cpu = t.2.map_or_else(|| "?".to_owned(), |c| format!("{:.1}", c.as_secs_f64() * 1e3));
    println!("{what:<58} {:>9.3} / {:>9.3}ms (CPU {cpu:>7} ms) {:>9} allocations  {more}", t.0.as_secs_f64() * 1e3, t.1.as_secs_f64() * 1e3, a.calls());
}

/// A deterministic generator (SplitMix64).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// Protobuf's wire format, as much as a profile needs.
fn varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn key(out: &mut Vec<u8>, field: u32, wire: u8) {
    varint(out, (u64::from(field) << 3) | u64::from(wire));
}

fn bytes_field(out: &mut Vec<u8>, field: u32, b: &[u8]) {
    key(out, field, 2);
    varint(out, b.len() as u64);
    out.extend_from_slice(b);
}

fn uint_field(out: &mut Vec<u8>, field: u32, v: u64) {
    key(out, field, 0);
    varint(out, v);
}

/// A minute's CPU profile of a busy service, as Go's `runtime/pprof` writes it (inflated): `functions`
/// functions under a few entry points, `stacks` distinct stacks 10 to 40 deep (each a walk down a
/// call tree, so stacks share their callers as a real service's do), each sample's values a count
/// and its nanoseconds at 100 Hz.
fn cpu_profile(functions: usize, stacks: usize, seed: u64) -> Vec<u8> {
    let mut rng = Rng(seed);
    let mut strings: Vec<String> = vec![String::new(), "samples".into(), "count".into(), "cpu".into(), "nanoseconds".into()];
    let mut out = Vec::new();
    // Sample types: samples/count, cpu/nanoseconds.
    for (t, u) in [(1u64, 2u64), (3, 4)] {
        let mut vt = Vec::new();
        uint_field(&mut vt, 1, t);
        uint_field(&mut vt, 2, u);
        bytes_field(&mut out, 1, &vt);
    }
    // Functions and their locations (a line each), ids from 1.
    let pkgs = [
        "runtime",
        "net/http",
        "encoding/json",
        "github.com/acme/shop/internal/cart",
        "github.com/acme/shop/internal/orders",
        "github.com/jackc/pgx/v5",
        "google.golang.org/grpc",
        "crypto/sha256",
        "main",
    ];
    for f in 1..=functions {
        let pkg = pkgs[f % pkgs.len()];
        let name = format!("{pkg}.(*handler{}).step{f}", f % 37);
        let file = format!("/src/{pkg}/file{}.go", f % 211);
        strings.push(name);
        let name_ix = strings.len() as u64 - 1;
        strings.push(file);
        let file_ix = strings.len() as u64 - 1;
        let mut func = Vec::new();
        uint_field(&mut func, 1, f as u64);
        uint_field(&mut func, 2, name_ix);
        uint_field(&mut func, 3, name_ix);
        uint_field(&mut func, 4, file_ix);
        bytes_field(&mut out, 5, &func);
        let mut line = Vec::new();
        uint_field(&mut line, 1, f as u64);
        uint_field(&mut line, 2, 10 + (f as u64 % 500));
        let mut loc = Vec::new();
        uint_field(&mut loc, 1, f as u64);
        uint_field(&mut loc, 3, 0x40_0000 + f as u64 * 16);
        bytes_field(&mut loc, 4, &line);
        bytes_field(&mut out, 4, &loc);
    }
    // Stacks: walks down a call tree whose node k's children are drawn from a few hundred
    // functions each (a callee chosen by the path so far, so walks share their prefixes).
    for _ in 0..stacks {
        let depth = 10 + rng.below(31) as usize;
        let mut path: Vec<u64> = Vec::with_capacity(depth);
        let mut at = 1 + rng.below(4);
        for d in 0..depth {
            path.push(at);
            let fan = if d < 4 { 3 } else { 12 };
            at = 1 + (at.wrapping_mul(2_654_435_761).wrapping_add(rng.below(fan)) % functions as u64);
        }
        // Leaf first.
        path.reverse();
        let mut sample = Vec::new();
        let mut ids = Vec::new();
        for id in &path {
            varint(&mut ids, *id);
        }
        bytes_field(&mut sample, 1, &ids);
        let count = 1 + rng.below(4);
        let mut vals = Vec::new();
        varint(&mut vals, count);
        varint(&mut vals, count * 10_000_000);
        bytes_field(&mut sample, 2, &vals);
        bytes_field(&mut out, 2, &sample);
    }
    for s in &strings {
        bytes_field(&mut out, 6, s.as_bytes());
    }
    uint_field(&mut out, 9, 1_792_771_200_000_000_000);
    uint_field(&mut out, 10, 60_000_000_000);
    let mut pt = Vec::new();
    uint_field(&mut pt, 1, 3);
    uint_field(&mut pt, 2, 4);
    bytes_field(&mut out, 11, &pt);
    uint_field(&mut out, 12, 10_000_000);
    out
}

fn gzip(b: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(b).unwrap();
    e.finish().unwrap()
}

fn rfc(ms: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).unwrap().format(&time::format_description::well_known::Rfc3339).unwrap()
}

/// An upload of `files` as dd-trace-go writes one, its start `start_ms`.
fn upload(files: &[(&str, &[u8])], start_ms: i64, tags: &str) -> (String, Vec<u8>) {
    let boundary = "b0und4ry";
    let mut body = Vec::new();
    let mut part = |name: &str, bytes: &[u8]| {
        body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes());
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    };
    for (n, b) in files {
        part(n, b);
    }
    let names: Vec<String> = files.iter().map(|(n, _)| format!("{n:?}")).collect();
    let event = format!(r#"{{"start":"{}","end":"{}","attachments":[{}],"tags_profiler":"{tags}","family":"go","version":"4"}}"#, rfc(start_ms), rfc(start_ms + 60_000), names.join(","));
    part("event", event.as_bytes());
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

const NOW_MS: i64 = 1_792_771_200_000;

/// An upload as dd-trace-java sends one: its recording `jfr` in the part `main`, its file
/// `main.jfr`, then `event`.
fn java_upload(jfr: &[u8]) -> (String, Vec<u8>) {
    let boundary = "kiln-bench-java-0f3a9c";
    let mut body = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"main\"; filename=\"main.jfr\"\r\nContent-Type: application/octet-stream\r\n\r\n").into_bytes();
    body.extend_from_slice(jfr);
    body.extend_from_slice(format!("\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"event\"; filename=\"event.json\"\r\nContent-Type: application/json\r\n\r\n").as_bytes());
    body.extend_from_slice(br#"{"attachments":["main.jfr"],"tags_profiler":"service:checkout-java,env:prod,language:jvm","start":"2026-10-23T23:12:06.393Z","end":"2026-10-23T23:12:12.323Z","family":"java","version":"4"}"#);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

fn main() {
    assert!(alloc::installed(), "the counting allocator");
    let uploads: usize = std::env::var("KOBS_UPLOADS").ok().and_then(|v| v.parse().ok()).unwrap_or(20_000);
    let base = std::env::var_os("KOBS_DIR").map_or_else(std::env::temp_dir, std::path::PathBuf::from);
    let fixture_cpu = include_bytes!("../../kiln-obs/src/fixtures/pprof/go-cpu.pprof");
    let fixture_heap = include_bytes!("../../kiln-obs/src/fixtures/pprof/go-heap.pprof");
    let big = cpu_profile(8_000, 40_000, 1);
    let big_gz = gzip(&big);
    println!("a busy service's CPU profile: {} KiB inflated, {} KiB gzip'd; Go's fixtures: {} and {} bytes gzip'd\n", big.len() >> 10, big_gz.len() >> 10, fixture_cpu.len(), fixture_heap.len());

    // An upload read and checked.
    let (ct, body) = upload(&[("cpu.pprof", fixture_cpu), ("delta-heap.pprof", fixture_heap)], NOW_MS, "service:checkout,env:prod");
    let (t, a) = measure(|| intake::read(&ct, body.clone()).unwrap().profiles.len());
    line("an upload read (Go's fixtures: cpu and heap)", &t, &a, &format!("{} bytes", body.len()));
    let (ct_big, body_big) = upload(&[("cpu.pprof", &big_gz)], NOW_MS, "service:checkout,env:prod");
    let (t, a) = measure(|| intake::read(&ct_big, body_big.clone()).unwrap().profiles.len());
    line("an upload read (a busy service's CPU)", &t, &a, &format!("{:.0} MB/s inflated", big.len() as f64 / t.0.as_secs_f64() / 1e6));

    // A Java upload as dd-trace-java sends it (kiln-obs's fixture: its recording, zstd'd), read into
    // its profiles; repacked, as a runner sends it on; and the repacked read again, at the server.
    let jfr = include_bytes!("../../kiln-obs/src/fixtures/jfr/dd-trace-java-1.67.1-burn.jfr.zst");
    let (ct_java, body_java) = java_upload(jfr);
    let (t, a) = measure(|| intake::read(&ct_java, body_java.clone()).unwrap().profiles.len());
    let up = intake::read(&ct_java, body_java.clone()).unwrap();
    let made: usize = up.profiles.iter().map(|p| p.plain_len).sum();
    line("a Java upload read (its recording into its profiles)", &t, &a, &format!("{} KiB sent, {} profiles of {} KiB", body_java.len() >> 10, up.profiles.len(), made >> 10));
    let (t, a) = measure(|| intake::repack(&up).0.len());
    let (repacked, ct_repacked) = intake::repack(&up);
    line("  repacked to send on", &t, &a, &format!("{} KiB", repacked.len() >> 10));
    let (t, a) = measure(|| intake::read(&ct_repacked, repacked.clone()).unwrap().profiles.len());
    line("  read again where it is sent", &t, &a, "");

    // A profile read, and summed.
    let (t, a) = measure(|| pprof::read(&big).unwrap().samples.len());
    let samples = pprof::read(&big).unwrap().samples.len();
    line("a busy service's CPU profile read (pprof::read)", &t, &a, &format!("{samples} samples, {:.0} MB/s", big.len() as f64 / t.0.as_secs_f64() / 1e6));
    let p = pprof::read(&big).unwrap();
    let (t, a) = measure(|| {
        let mut g = Flame::new();
        g.add(&p, 1);
        g.nodes.len()
    });
    let mut g = Flame::new();
    g.add(&p, 1);
    line("  summed into a new graph (Flame::add)", &t, &a, &format!("{} nodes", g.nodes.len()));
    let (t, a) = measure(|| {
        g.add(&p, 1);
        g.nodes.len()
    });
    line("  summed into a graph that has its nodes", &t, &a, "");
    let mut other = Flame::new();
    other.add(&pprof::read(&cpu_profile(8_000, 40_000, 2)).unwrap(), 1);
    let (t, a) = measure(|| {
        let mut m = Flame::new();
        m.merge(&g);
        m.merge(&other);
        m.nodes.len()
    });
    line("two graphs merged", &t, &a, "");
    let (t, a) = measure(|| g.drawn((g.nodes[0].total as f64 * 1e-4) as i64).len());
    line("a graph drawn (a node under 1/10,000 left out)", &t, &a, &format!("{} of {} nodes drawn", g.drawn((g.nodes[0].total as f64 * 1e-4) as i64).len(), g.nodes.len()));
    let (t, a) = measure(|| g.by_function().len());
    line("its functions by self (by_function)", &t, &a, "");

    // Kept, a store of `uploads` uploads of a hundred services' processes, each a fixture's cpu and
    // heap (the busy profile's bytes would take gigabytes: its reads are measured above).
    let dir = tempfile::tempdir_in(&base).unwrap();
    let s = Store::open(dir.path()).unwrap();
    let tags: Vec<String> = (0..100).map(|i| format!("service:svc-{},env:prod,version:1.{},host:host-{},runtime-id:{i:08x}-0000", i % 25, i % 3, i % 40)).collect();
    let mut parsed: Vec<intake::Upload> = Vec::with_capacity(uploads);
    for i in 0..uploads {
        let start = NOW_MS - (uploads - i) as i64 * 60_000 / 100;
        let (ct, body) = upload(&[("cpu.pprof", fixture_cpu), ("delta-heap.pprof", fixture_heap)], start, &format!("{},profile_seq:{i}", tags[i % 100]));
        parsed.push(intake::read(&ct, body).unwrap());
    }
    let t = Instant::now();
    alloc::begin();
    for (k, chunk) in parsed.chunks(32).enumerate() {
        s.put(chunk, NOW_MS + k as i64).unwrap();
    }
    let a = alloc::end();
    let took = t.elapsed();
    let st = s.stats();
    println!(
        "\n{:<58} {:>9.3}ms, {:.1} µs an upload, {} allocations  {} uploads in {} segments, {} MiB",
        "kept, in batches of 32 (a sync a batch)",
        took.as_secs_f64() * 1e3,
        took.as_secs_f64() * 1e6 / uploads as f64,
        a.calls(),
        st.uploads,
        st.segments,
        st.bytes >> 20
    );
    drop(s);
    alloc::begin();
    let t = Instant::now();
    let s = Store::open(dir.path()).unwrap();
    let took = t.elapsed();
    let a = alloc::end();
    println!(
        "{:<58} {:>9.3}ms, {} allocations, {} bytes held: {:.0} bytes an upload",
        "opened again (its header files read)",
        took.as_secs_f64() * 1e3,
        a.calls(),
        a.live,
        a.live as f64 / s.stats().uploads as f64
    );

    // Summed over what a query chooses.
    let search = kiln_obs::logs::search::parse("service:svc-3 env:prod").unwrap();
    let mut chosen = 0usize;
    s.scan(i64::MIN, i64::MAX, &mut |_| {
        chosen += 1;
        ControlFlow::Continue(())
    });
    let threads = std::thread::available_parallelism().map_or(1, |n| (n.get() / 4).clamp(1, 8));
    for (limit, label) in [(60, "an hour's of one service (60)"), (500, "500 uploads of one service")] {
        let asked = Asked { from_ns: i64::MIN, to_ns: i64::MAX, search: &search, only: None, profile: "cpu.pprof", kind: "cpu", limit };
        for th in [1, threads] {
            let (t, a) = measure(|| query::flame(&s, &asked, th).read);
            let r = query::flame(&s, &asked, th);
            line(&format!("{label}, on {th} thread{}", if th == 1 { "" } else { "s" }), &t, &a, &format!("{} of {} read, {:.0} uploads/s", r.read, r.matched, r.read as f64 / t.0.as_secs_f64()));
        }
    }
    println!("\n({chosen} uploads in the store; the query's {threads} threads a quarter of the cores)");
}

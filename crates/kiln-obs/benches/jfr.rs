//! Java's recordings read into profiles (`docs/observe.md` §8.11, §12.37): a busy service's minute
//! as dd-trace-java's profiler records it (synthetic, its shape a real one's: thousands of methods
//! in hundreds of classes, stacks 20 to 80 deep sharing their callers as a framework's do, tens of
//! thousands of distinct stacks; CPU, wall and allocation samples on them, most on a few), read
//! (`jfr::to_pprof`: its metadata, its pools, every event, the profiles written). Time (the least
//! of nine, and their median), allocations and page faults (minor, major: Linux's own count, of the
//! second run), MB/s and events a second; and recordings of the
//! tracer's own where `KOBS_JFR` names them (uncompressed, as its debug dump writes them; paths
//! separated by colons).
//!
//! `cargo bench -p kiln-obs --bench jfr` (`KOBS_METHODS`, 12,000; `KOBS_STACKS`, 20,000;
//! `KOBS_EVENTS`, 100,000).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::cast_precision_loss, clippy::cast_possible_truncation)]

use std::time::{Duration, Instant};

use hyper_measure::alloc;
use kiln_obs::profile::jfr;

#[global_allocator]
static COUNTING: alloc::Counting = alloc::Counting;

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

/// JFR's varint, as short as it goes.
fn var(out: &mut Vec<u8>, mut v: u64) {
    for _ in 0..8 {
        if v < 0x80 {
            out.push(v as u8);
            return;
        }
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn utf8(out: &mut Vec<u8>, s: &str) {
    out.push(3);
    var(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

/// A record: its size (padded to four bytes, as ddprof writes it) and its type and bytes.
fn record(out: &mut Vec<u8>, type_id: u64, body: &[u8]) {
    let mut ty = Vec::new();
    var(&mut ty, type_id);
    let mut v = (ty.len() + body.len() + 4) as u64;
    for i in 0..4 {
        out.push(if i == 3 { (v & 0x7f) as u8 } else { ((v & 0x7f) as u8) | 0x80 });
        v >>= 7;
    }
    out.extend_from_slice(&ty);
    out.extend_from_slice(body);
}

/// The classes of a recording: (id, name, fields: (name, class, in a pool, an array)).
type Classes = [(u64, &'static str, &'static [(&'static str, u64, bool, bool)])];

const CLASSES: &Classes = &[
    (1, "long", &[]),
    (2, "int", &[]),
    (3, "boolean", &[]),
    (4, "float", &[]),
    (5, "java.lang.String", &[]),
    (10, "jdk.types.Symbol", &[("string", 5, false, false)]),
    (11, "java.lang.Class", &[("classLoader", 1, false, false), ("name", 10, true, false), ("package", 10, true, false), ("modifiers", 2, false, false)]),
    (12, "jdk.types.Method", &[("type", 11, true, false), ("name", 10, true, false), ("descriptor", 10, true, false), ("modifiers", 2, false, false), ("hidden", 3, false, false)]),
    (13, "jdk.types.FrameType", &[("description", 5, false, false)]),
    (14, "jdk.types.StackFrame", &[("method", 12, true, false), ("lineNumber", 2, false, false), ("bytecodeIndex", 2, false, false), ("type", 13, true, false)]),
    (15, "jdk.types.StackTrace", &[("truncated", 3, false, false), ("frames", 14, false, true)]),
    (16, "java.lang.Thread", &[("osName", 5, false, false), ("osThreadId", 1, false, false), ("javaName", 5, false, false), ("javaThreadId", 1, false, false)]),
    (
        101,
        "datadog.ExecutionSample",
        &[
            ("startTime", 1, false, false),
            ("eventThread", 16, true, false),
            ("stackTrace", 15, true, false),
            ("spanId", 1, false, false),
            ("localRootSpanId", 1, false, false),
            ("weight", 1, false, false),
        ],
    ),
    (
        102,
        "datadog.MethodSample",
        &[
            ("startTime", 1, false, false),
            ("eventThread", 16, true, false),
            ("stackTrace", 15, true, false),
            ("spanId", 1, false, false),
            ("localRootSpanId", 1, false, false),
            ("weight", 1, false, false),
        ],
    ),
    (
        103,
        "datadog.ObjectSample",
        &[
            ("startTime", 1, false, false),
            ("eventThread", 16, true, false),
            ("stackTrace", 15, true, false),
            ("objectClass", 11, true, false),
            ("size", 1, false, false),
            ("weight", 4, false, false),
            ("spanId", 1, false, false),
            ("localRootSpanId", 1, false, false),
        ],
    ),
    (110, "jdk.ActiveSetting", &[("startTime", 1, false, false), ("id", 1, false, false), ("name", 5, false, false), ("value", 5, false, false)]),
];

/// The metadata event of `CLASSES`, its strings UTF-8 (ddprof's way).
fn metadata(out: &mut Vec<u8>) {
    let mut strings: Vec<String> = Vec::new();
    let mut ix = |s: &str| -> u64 {
        if let Some(i) = strings.iter().position(|x| x == s) {
            return i as u64;
        }
        strings.push(s.to_owned());
        strings.len() as u64 - 1
    };
    let mut tree = Vec::new();
    let el = |tree: &mut Vec<u8>, name: u64, attrs: &[(u64, u64)], kids: u64| {
        var(tree, name);
        var(tree, attrs.len() as u64);
        for (k, v) in attrs {
            var(tree, *k);
            var(tree, *v);
        }
        var(tree, kids);
    };
    let (root, meta, region, class, field) = (ix("root"), ix("metadata"), ix("region"), ix("class"), ix("field"));
    el(&mut tree, root, &[], 2);
    el(&mut tree, meta, &[], CLASSES.len() as u64);
    for (id, name, fields) in CLASSES {
        let attrs = [(ix("id"), ix(&id.to_string())), (ix("name"), ix(name))];
        el(&mut tree, class, &attrs, fields.len() as u64);
        for &(name, cls, pool, array) in fields.iter() {
            let mut attrs = vec![(ix("name"), ix(name)), (ix("class"), ix(&cls.to_string()))];
            if pool {
                attrs.push((ix("constantPool"), ix("true")));
            }
            if array {
                attrs.push((ix("dimension"), ix("1")));
            }
            el(&mut tree, field, &attrs, 0);
        }
    }
    el(&mut tree, region, &[], 0);
    let mut body = Vec::new();
    for _ in 0..3 {
        var(&mut body, 0);
    }
    var(&mut body, strings.len() as u64);
    for s in &strings {
        utf8(&mut body, s);
    }
    body.extend_from_slice(&tree);
    record(out, 0, &body);
}

/// A minute's recording as ddprof writes one (one chunk: its metadata, its events, its pools last):
/// `methods` methods in `methods / 12` classes, `stacks` distinct stacks 20 to 80 deep, each a walk
/// down a call tree from a few entry points whose callees are drawn by the path so far (so stacks
/// share their callers); `events` samples, half CPU (a weight of one interval or two), a third
/// wall, the rest allocations, each on a stack drawn so that a few stacks have most of them.
fn recording(methods: u64, stacks: u64, events: u64, seed: u64) -> Vec<u8> {
    let mut rng = Rng(seed);
    let classes = (methods / 12).max(1);
    let mut body = Vec::new();
    let meta_at = 68;
    metadata(&mut body);
    // Its interval (10 ms), as ddprof says it.
    let mut setting = Vec::new();
    var(&mut setting, 0);
    var(&mut setting, 101);
    utf8(&mut setting, "interval");
    utf8(&mut setting, "10000000");
    record(&mut body, 110, &setting);
    let mut ev = Vec::new();
    for i in 0..events {
        // A few stacks most of the samples: a draw below a draw.
        let under = 1 + rng.below(stacks);
        let stack = 1 + rng.below(under);
        let thread = 1 + rng.below(64);
        ev.clear();
        var(&mut ev, 1_000_000 + i * 600);
        var(&mut ev, thread);
        var(&mut ev, stack);
        let kind = i % 6;
        if kind < 5 {
            var(&mut ev, rng.next() >> 1);
            var(&mut ev, rng.next() >> 1);
            var(&mut ev, 1 + u64::from(rng.below(8) == 0));
            record(&mut body, if kind < 3 { 101 } else { 102 }, &ev);
        } else {
            var(&mut ev, 1 + rng.below(classes));
            var(&mut ev, 16 + rng.below(4096));
            ev.extend_from_slice(&((1 + rng.below(64)) as f32).to_be_bytes());
            var(&mut ev, rng.next() >> 1);
            var(&mut ev, rng.next() >> 1);
            record(&mut body, 103, &ev);
        }
    }
    // Its pools: symbols, classes, methods, frame types, threads, stacks.
    let cp_at = 68 + body.len();
    let mut cp = Vec::new();
    var(&mut cp, 0);
    var(&mut cp, 0);
    var(&mut cp, 0);
    var(&mut cp, 1);
    var(&mut cp, 6);
    let pkgs = [
        "org/springframework/web/servlet",
        "org/apache/catalina/core",
        "com/fasterxml/jackson/databind",
        "io/netty/channel",
        "java/util/concurrent",
        "com/acme/shop/cart",
        "com/acme/shop/orders",
        "org/hibernate/engine",
    ];
    let symbols = classes + methods + 1;
    var(&mut cp, 10);
    var(&mut cp, symbols);
    for c in 1..=classes {
        var(&mut cp, c);
        utf8(&mut cp, &format!("{}/Handler{c}", pkgs[(c % pkgs.len() as u64) as usize]));
    }
    for m in 1..=methods {
        var(&mut cp, classes + m);
        utf8(&mut cp, &format!("step{m}"));
    }
    var(&mut cp, symbols);
    utf8(&mut cp, "()V");
    var(&mut cp, 11);
    var(&mut cp, classes);
    for c in 1..=classes {
        var(&mut cp, c);
        var(&mut cp, 0);
        var(&mut cp, c);
        var(&mut cp, c);
        var(&mut cp, 1);
    }
    var(&mut cp, 12);
    var(&mut cp, methods);
    for m in 1..=methods {
        var(&mut cp, m);
        var(&mut cp, 1 + m % classes);
        var(&mut cp, classes + m);
        var(&mut cp, symbols);
        var(&mut cp, 1);
        cp.push(0);
    }
    var(&mut cp, 13);
    var(&mut cp, 3);
    for (k, s) in [(1, "JIT compiled"), (2, "Interpreted"), (3, "Inlined")] {
        var(&mut cp, k);
        utf8(&mut cp, s);
    }
    var(&mut cp, 16);
    var(&mut cp, 64);
    for t in 1..=64u64 {
        var(&mut cp, t);
        utf8(&mut cp, &format!("http-nio-8080-exec-{t}"));
        var(&mut cp, 1000 + t);
        utf8(&mut cp, &format!("http-nio-8080-exec-{t}"));
        var(&mut cp, t);
    }
    var(&mut cp, 15);
    var(&mut cp, stacks);
    let mut path = Vec::with_capacity(80);
    for s in 1..=stacks {
        let depth = 20 + rng.below(61);
        path.clear();
        let mut at = 1 + rng.below(6);
        for d in 0..depth {
            path.push(at);
            let fan = if d < 12 { 2 } else { 10 };
            at = 1 + (at.wrapping_mul(2_654_435_761).wrapping_add(rng.below(fan)) % methods);
        }
        var(&mut cp, s);
        cp.push(0);
        var(&mut cp, depth);
        // Leaf first.
        for &m in path.iter().rev() {
            var(&mut cp, m);
            var(&mut cp, 10 + m % 400);
            var(&mut cp, m % 90);
            var(&mut cp, 1 + rng.below(3));
        }
    }
    record(&mut body, 1, &cp);
    let size = 68 + body.len();
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&jfr::MAGIC);
    out.extend_from_slice(&2u16.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes());
    for v in [size as u64, cp_at as u64, meta_at as u64, 1_792_771_200_000_000_000, 60_000_000_000, 0, 1_000_000_000] {
        out.extend_from_slice(&v.to_be_bytes());
    }
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(&body);
    out
}

/// This process's page faults so far, minor and major (Linux's `/proc/self/stat`; none elsewhere).
fn faults() -> Option<(u64, u64)> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // After its name (in parentheses, which may hold spaces): state, ppid, pgrp, session, tty,
    // tpgid, flags, minflt, cminflt, majflt.
    let rest = stat.get(stat.rfind(')')? + 1..)?;
    let f: Vec<&str> = rest.split_whitespace().collect();
    Some((f.get(7)?.parse().ok()?, f.get(9)?.parse().ok()?))
}

fn measure<T>(mut f: impl FnMut() -> T) -> (Duration, Duration, alloc::Counts, Option<(u64, u64)>) {
    let mut times = Vec::new();
    let mut counted = alloc::Counts::default();
    let mut faulted = None;
    for n in 0..9 {
        let before = if n == 1 { faults() } else { None };
        let t = Instant::now();
        if n == 1 {
            alloc::begin();
        }
        std::hint::black_box(f());
        if n == 1 {
            counted = alloc::end();
            faulted = before.zip(faults()).map(|(a, b)| (b.0.saturating_sub(a.0), b.1.saturating_sub(a.1)));
        }
        times.push(t.elapsed());
    }
    times.sort();
    (times[0], times[times.len() / 2], counted, faulted)
}

/// One recording read, said: its time, its rates, its allocations.
fn line(what: &str, bytes: &[u8]) {
    let c = jfr::to_pprof(bytes).unwrap();
    let (least, median, a, faulted) = measure(|| jfr::to_pprof(bytes).unwrap().events);
    let made: usize = c.profiles.iter().map(|m| m.pprof.len()).sum();
    println!(
        "{what:<44} {:>8.2} / {:>8.2}ms {:>9} allocations {:>7} page faults  {:>6.0} MB/s, {:>5.1} M events/s; {} KiB in, {} events, {} KiB of pprof out ({})",
        least.as_secs_f64() * 1e3,
        median.as_secs_f64() * 1e3,
        a.calls(),
        faulted.map_or_else(|| "?".to_owned(), |(minor, major)| format!("{minor}+{major}")),
        bytes.len() as f64 / least.as_secs_f64() / 1e6,
        c.events as f64 / least.as_secs_f64() / 1e6,
        bytes.len() >> 10,
        c.events,
        made >> 10,
        c.profiles.iter().map(|m| m.name.trim_end_matches(".pprof")).collect::<Vec<_>>().join(" "),
    );
}

fn main() {
    assert!(alloc::installed(), "the counting allocator");
    let env = |k: &str, d: u64| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let (methods, stacks, events) = (env("KOBS_METHODS", 12_000), env("KOBS_STACKS", 20_000), env("KOBS_EVENTS", 100_000));
    for (what, m, s, e) in
        [("a quiet service's minute", methods / 4, stacks / 10, events / 10), ("a busy service's minute", methods, stacks, events), ("a very busy one's", methods * 2, stacks * 3, events * 4)]
    {
        let r = recording(m, s, e, 7);
        line(&format!("{what} ({}k methods, {}k stacks)", m / 1000, s / 1000), &r);
    }
    for path in std::env::var("KOBS_JFR").unwrap_or_default().split(':').filter(|p| !p.is_empty()) {
        let bytes = std::fs::read(path).unwrap();
        line(&format!("dd-trace-java's ({})", std::path::Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or("")), &bytes);
    }
}

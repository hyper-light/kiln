//! The notebooks' store (`docs/observe.md` §12.33): 10,000 notebooks as a team's years of them
//! (investigations, runbooks, postmortems, reports; a few Markdown cells, graphs of Datadog's
//! definitions, a log stream), each a file: opened (each file's head read, as the store does, against
//! each read whole), listed as the list asks (picked, ordered, a page), one read and answered as the
//! API answers it (each graph drawn by the dashboards' importer), and a postmortem made of kiln's
//! template and an incident of a full timeline (1,000 cells). Time (the median of nine), allocations.
//!
//! `cargo bench -p kiln --bench notebooks` (`KOBS_NOTEBOOKS`: how many, 10,000; `KOBS_DIR`: where the
//! files go, the system's temporary folder else).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_possible_wrap)]

use std::time::{Duration, Instant};

use hyper_measure::alloc;
use kiln::observe::incidents::{Cell as Said, Incident, TimelineCell};
use kiln::observe::notebooks::http::NotebookOut;
use kiln::observe::notebooks::{Cell, Metadata, Notebook, Notebooks, Pick, Summary, postmortem, sort};
use serde_json::{Value, json};

#[global_allocator]
static COUNTING: alloc::Counting = alloc::Counting;

const T0: i64 = 1_700_000_000_000;

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
        self.next() % n
    }

    fn words(&mut self, n: usize) -> String {
        const W: [&str; 12] = ["latency", "rose", "after", "the", "deploy", "pool", "drained", "errors", "fell", "rolled", "back", "checkout"];
        (0..n).map(|_| W[self.below(12) as usize]).collect::<Vec<_>>().join(" ")
    }
}

fn notebook(id: u64, r: &mut Rng) -> Notebook {
    let svc = ["checkout", "search", "payments", "web", "api"][r.below(5) as usize];
    let mut cells = Vec::new();
    let mut next = 0;
    let mut push = |a: Value| {
        next += 1;
        cells.push(Cell { id: format!("c{next}"), attributes: a });
    };
    for _ in 0..1 + r.below(3) {
        let (head, long) = (r.words(4), 150 + r.below(300) as usize);
        let body = r.words(long);
        push(json!({ "definition": { "type": "markdown", "text": format!("## {head}\n\n{body}") } }));
    }
    for _ in 0..3 + r.below(6) {
        let m = ["app.latency.p95", "app.errors", "system.cpu.user", "trace.http.request.hits"][r.below(4) as usize];
        push(
            json!({ "definition": { "type": "timeseries", "title": r.words(3), "requests": [{ "q": format!("avg:{m}{{service:{svc}}} by {{host}}"), "display_type": "line", "style": { "palette": "dog_classic" } }] }, "graph_size": "m" }),
        );
    }
    push(json!({ "definition": { "type": "log_stream", "query": format!("service:{svc} status:error"), "columns": ["host", "service"] }, "graph_size": "s" }));
    let kind = ["investigation", "runbook", "postmortem", "report", "documentation"][r.below(5) as usize];
    Notebook {
        id,
        name: format!("{svc} {}", r.words(3)),
        cells,
        time: json!({ "live_span": "1h" }),
        metadata: Metadata { is_template: r.below(50) == 0, take_snapshots: false, kind: Some(kind.into()) },
        template_variables: None,
        author: format!("user:{}", ["ada", "bob", "cy", "dee"][r.below(4) as usize]),
        created: T0 + id as i64 * 3_600_000,
        modified: T0 + id as i64 * 3_600_000 + r.below(86_400_000) as i64,
        incident: None,
        next,
    }
}

/// An incident of a full timeline: notes, changes, its monitor's alerts.
fn incident() -> Incident {
    let mut timeline = Vec::new();
    for k in 0..1_000u64 {
        let cell = match k % 3 {
            0 => Said::Markdown { content: format!("note {k}: the pool drained again, rolled back the gateway"), important: k % 50 == 0 },
            1 => Said::Monitor {
                monitor_id: "m1".into(),
                monitor: "Checkout errors".into(),
                state: if k % 2 == 0 { "ALERT".into() } else { "OK".into() },
                value: Some(k as f64),
                message: "errors at their bound".into(),
            },
            _ => Said::Changed { what: "severity".into(), from: "SEV-3".into(), to: "SEV-2".into() },
        };
        timeline.push(TimelineCell {
            id: format!("c{k}"),
            created: T0 + k as i64 * 60_000,
            author: if k % 3 == 1 { "monitor:m1".into() } else { format!("user:{}", ["ada", "bob"][(k % 2) as usize]) },
            cell,
        });
    }
    let v = json!({
        "id": "0190f0e0-0000-7000-8000-000000000001", "public_id": 7, "title": "Checkout requests failing", "customer_impacted": true,
        "customer_impact_scope": "Every checkout fails", "customer_impact_start": T0, "customer_impact_end": T0 + 3_600_000,
        "severity": "SEV-2", "state": "resolved", "commander": "ada", "created": T0 + 60_000, "modified": T0 + 7_200_000, "detected": T0,
        "declared": T0 + 60_000, "resolved": T0 + 7_200_000, "created_by": "user:ada", "last_modified_by": "user:ada",
    });
    let mut i: Incident = serde_json::from_value(v).unwrap();
    i.timeline = timeline;
    i
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

/// What `f` takes (the median of nine) and allocates (its second run).
fn measure<T>(mut f: impl FnMut() -> T) -> (Duration, alloc::Counts) {
    let mut times = Vec::new();
    let mut counted = alloc::Counts::default();
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
    (median(times), counted)
}

fn main() {
    let n: u64 = std::env::var("KOBS_NOTEBOOKS").ok().and_then(|v| v.parse().ok()).unwrap_or(10_000);
    let root = std::env::var("KOBS_DIR").map_or_else(|_| std::env::temp_dir(), std::path::PathBuf::from);
    let dir = tempfile::tempdir_in(&root).unwrap();
    std::fs::create_dir_all(dir.path().join("notebooks")).unwrap();
    let mut r = Rng(7);
    let mut disk = 0usize;
    for id in 1..=n {
        let b = serde_json::to_vec(&notebook(id, &mut r)).unwrap();
        disk += b.len();
        std::fs::write(dir.path().join(format!("notebooks/{id}.json")), &b).unwrap();
    }
    println!("{n} notebooks, {:.1} MiB on disk ({} B each on average), in {}", disk as f64 / 1_048_576.0, disk / n as usize, root.display());

    // Opened: each file's head (as the store does), against each read whole.
    let mut times = Vec::new();
    let mut held = 0i64;
    for k in 0..3 {
        alloc::begin();
        let t = Instant::now();
        let kept = Notebooks::open(dir.path(), "https://kiln.example.com");
        times.push(t.elapsed());
        let c = alloc::end();
        if k == 0 {
            held = c.live;
        }
        drop(kept);
    }
    println!("{:<56} {:>10.1}ms  {:.2} MiB held", "opened: each file's head", median(times).as_secs_f64() * 1e3, held as f64 / 1_048_576.0);
    let (whole, a) = measure(|| (1..=n).filter_map(|id| std::fs::read(dir.path().join(format!("notebooks/{id}.json"))).ok().and_then(|b| serde_json::from_slice::<Notebook>(&b).ok())).count());
    println!("{:<56} {:>10.1}ms {:>12} allocations", "each file read whole (as a notebook)", whole.as_secs_f64() * 1e3, a.calls());

    let kept = Notebooks::open(dir.path(), "https://kiln.example.com");
    for (what, pick) in [
        ("listed: every notebook, a page of 100", Pick::default()),
        ("listed: runbooks", Pick::default().with_kind("runbook")),
        ("listed: a part of the name (\"latency\")", Pick::default().query(Some("Latency"))),
    ] {
        let (t, a) = measure(|| {
            kept.read_summaries(|all| {
                let mut l: Vec<&Summary> = all.values().filter(|s| pick.keeps(s)).collect();
                sort(&mut l, "modified", true).unwrap();
                l.iter().take(100).map(|s| (*s).clone()).collect::<Vec<_>>()
            })
        });
        println!("{what:<56} {:>10.3}ms {:>12} allocations", t.as_secs_f64() * 1e3, a.calls());
    }
    let (t, a) = measure(|| {
        kept.read_summaries(|all| {
            let mut l: Vec<&Summary> = all.values().collect();
            sort(&mut l, "name", false).unwrap();
            l.iter().take(100).map(|s| (*s).clone()).collect::<Vec<_>>()
        })
    });
    println!("{:<56} {:>10.3}ms {:>12} allocations", "listed: by name (case aside), a page of 100", t.as_secs_f64() * 1e3, a.calls());
    let (t, a) = measure(|| {
        let nb = kept.get(n / 2).unwrap();
        serde_json::to_vec(&serde_json::json!({ "data": NotebookOut::of(&nb) })).unwrap().len()
    });
    println!("{:<56} {:>10.3}ms {:>12} allocations", "one read and answered (its graphs drawn)", t.as_secs_f64() * 1e3, a.calls());
    let i = incident();
    let (t, a) = measure(|| postmortem::draft(&i, None, "https://kiln.example.com", T0 + 10_000_000));
    let d = postmortem::draft(&i, None, "https://kiln.example.com", T0 + 10_000_000);
    let bytes: usize = d.cells.iter().map(|(_, c)| c.to_string().len()).sum();
    println!("{:<56} {:>10.3}ms {:>12} allocations  ({} KiB)", "a postmortem of kiln's template, 1,000 timeline cells", t.as_secs_f64() * 1e3, a.calls(), bytes / 1024);
}

//! The incidents' store (`docs/observe.md` §12.32): an organization's years of incidents (most
//! resolved; their services, teams and severities; timelines of notes, changes and their monitors'
//! alerts; todos, postmortems and links), each a file: opened (its time, the bytes it holds in
//! memory against those on disk); searched as the incidents page and Datadog's API search them
//! (Datadog's syntax, what matched counted by facet, a page of 50 answered), read in place against
//! a copy of every incident (as the store first read them), the answer's bytes written as the
//! server writes them; one read; and one changed (a note), its own file written against the whole
//! store written. Time (the median of nine), allocations.
//!
//! `cargo bench -p kiln --bench incidents` (`KOBS_INCIDENTS`: how many, 10,000; `KOBS_DIR`: where
//! the files go, a disk's folder for what writing costs there: the system's temporary folder else).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_possible_wrap)]

use std::io::Write as _;
use std::time::{Duration, Instant};

use hyper_measure::alloc;
use kiln::observe::incidents::{Attachment, Cell, Field, FromMonitor, Handle, Incident, Incidents, TimelineCell, Todo, http, search};
use serde_json::Value;

#[global_allocator]
static COUNTING: alloc::Counting = alloc::Counting;

const T0: i64 = 1_700_000_000_000;

/// SplitMix64: the same incidents every run.
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
        const WORDS: [&str; 16] =
            ["rolled", "back", "the", "deploy", "latency", "errors", "recovered", "paged", "on-call", "database", "failover", "queue", "backlog", "drained", "customers", "retried"];
        (0..n).map(|_| WORDS[self.below(16) as usize]).collect::<Vec<_>>().join(" ")
    }
}

const WHAT: [&str; 8] = ["errors elevated", "latency high", "checkout failing", "queue backlog", "disk full", "certificate expiring", "deploy failed", "5xx spike"];

/// Incident `p`, as one comes to be over its life.
fn incident(p: u64, r: &mut Rng) -> Incident {
    let svc = format!("svc-{:02}", r.below(40));
    let services: Vec<Value> = std::iter::once(svc.clone()).chain((0..r.below(3)).map(|_| format!("svc-{:02}", r.below(40)))).map(Value::String).collect();
    let severity = ["SEV-1", "SEV-2", "SEV-2", "SEV-3", "SEV-3", "SEV-3", "SEV-4", "SEV-4", "SEV-5", "UNKNOWN"][r.below(10) as usize];
    let state = match r.below(20) {
        0 => "active",
        1 | 2 => "stable",
        _ => "resolved",
    };
    let created = T0 + p as i64 * 3_600_000;
    let by = format!("user:{}", ["ada", "bob", "cy", "dee", "eve"][r.below(5) as usize]);
    let mut i = Incident {
        id: format!("{:08x}-0000-7000-8000-{:012x}", p, r.next() & 0xFFFF_FFFF_FFFF),
        public_id: p,
        title: format!("{svc} {}", WHAT[r.below(8) as usize]),
        customer_impacted: r.below(3) == 0,
        customer_impact_scope: None,
        customer_impact_start: None,
        customer_impact_end: None,
        severity: severity.into(),
        state: state.into(),
        fields: [
            ("services".to_owned(), Field { kind: "multiselect".into(), value: Value::Array(services) }),
            ("teams".to_owned(), Field { kind: "multiselect".into(), value: Value::Array(vec![Value::String(format!("team-{}", r.below(10)))]) }),
            ("summary".to_owned(), Field { kind: "textbox".into(), value: Value::String(r.words(30)) }),
        ]
        .into(),
        commander: Some(by.clone()),
        notification_handles: vec![Handle { display_name: "ops".into(), handle: "@slack-ops".into() }],
        created,
        modified: created,
        detected: created - 120_000,
        declared: created,
        resolved: None,
        created_by: by.clone(),
        last_modified_by: by.clone(),
        is_test: false,
        monitor: None,
        timeline: Vec::new(),
        todos: Vec::new(),
        attachments: Vec::new(),
        next: 0,
    };
    if i.customer_impacted {
        i.customer_impact_scope = Some(r.words(12));
        i.customer_impact_start = Some(created - 300_000);
    }
    if r.below(2) == 0 {
        i.monitor = Some(FromMonitor { id: format!("m{:016x}", r.next()), name: format!("{svc} errors"), group: vec![("service".into(), svc.clone())] });
    }
    let mut t = created;
    let push = |i: &mut Incident, t: i64, cell: Cell| {
        i.next += 1;
        i.timeline.push(TimelineCell { id: format!("c{}", i.next), created: t, author: by.clone(), cell });
    };
    push(&mut i, t, Cell::Changed { what: "declared".into(), from: String::new(), to: format!("{severity} · active") });
    for _ in 0..5 + r.below(11) {
        t += 60_000 + r.below(600_000) as i64;
        let n = 12 + r.below(30) as usize;
        let content = r.words(n);
        push(&mut i, t, Cell::Markdown { content, important: r.below(8) == 0 });
        if i.monitor.is_some() && r.below(3) == 0 {
            let m = i.monitor.clone().unwrap();
            push(&mut i, t, Cell::Monitor { monitor_id: m.id, monitor: m.name, state: ["ALERT", "OK"][r.below(2) as usize].into(), value: Some(r.below(1000) as f64), message: r.words(10) });
        }
    }
    if state != "active" {
        t += 600_000;
        push(&mut i, t, Cell::Changed { what: "state".into(), from: "active".into(), to: "stable".into() });
    }
    if state == "resolved" {
        t += 1_800_000;
        push(&mut i, t, Cell::Changed { what: "state".into(), from: "stable".into(), to: "resolved".into() });
        i.resolved = Some(t);
        if i.customer_impacted {
            i.customer_impact_end = Some(t - 600_000);
        }
        i.fields.insert("root_cause".into(), Field { kind: "textbox".into(), value: Value::String(r.words(20)) });
        if r.below(5) < 3 {
            i.next += 1;
            i.attachments.push(Attachment {
                id: format!("a{}", i.next),
                attachment_type: "postmortem".into(),
                url: format!("https://docs.example.com/pm/{p}"),
                title: format!("Postmortem #{p}"),
                modified: t,
            });
        }
    }
    for k in 0..r.below(5) {
        i.next += 1;
        let done = (state == "resolved" || k % 2 == 0).then_some(t);
        i.todos.push(Todo { id: format!("t{}", i.next), content: r.words(6), assignees: vec![by.clone()], due_date: None, completed: done, created, modified: t });
    }
    i.modified = t;
    i
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

/// What `f` takes (the median of nine) and allocates (its second run).
fn measure<T>(mut f: impl FnMut() -> T) -> (Duration, alloc::Counts, T) {
    let mut times = Vec::new();
    let mut counted = alloc::Counts::default();
    let mut out = None;
    for n in 0..9 {
        let t = Instant::now();
        if n == 1 {
            alloc::begin();
        }
        let o = std::hint::black_box(f());
        if n == 1 {
            counted = alloc::end();
        }
        times.push(t.elapsed());
        out = Some(o);
    }
    (median(times), counted, out.unwrap())
}

/// A file written as the store writes one: whole, synced, renamed over the last, its folder synced.
fn write_whole(path: &std::path::Path, bytes: &[u8]) {
    let tmp = path.with_extension("tmp-bench");
    let mut f = std::fs::File::create(&tmp).unwrap();
    f.write_all(bytes).unwrap();
    f.sync_all().unwrap();
    std::fs::rename(&tmp, path).unwrap();
    std::fs::File::open(path.parent().unwrap()).unwrap().sync_all().unwrap();
}

fn main() {
    let n: u64 = std::env::var("KOBS_INCIDENTS").ok().and_then(|v| v.parse().ok()).unwrap_or(10_000);
    let root = std::env::var("KOBS_DIR").map_or_else(|_| std::env::temp_dir(), std::path::PathBuf::from);
    let dir = tempfile::tempdir_in(&root).unwrap();
    std::fs::create_dir_all(dir.path().join("incidents")).unwrap();
    let mut r = Rng(42);
    let mut disk = 0usize;
    for p in 1..=n {
        let b = serde_json::to_vec(&incident(p, &mut r)).unwrap();
        disk += b.len();
        std::fs::write(dir.path().join(format!("incidents/{p}.json")), &b).unwrap();
    }
    println!("{n} incidents, {:.1} MiB on disk ({} B each on average), in {}", disk as f64 / 1048576.0, disk / n as usize, root.display());

    // Opened: each file read.
    let mut times = Vec::new();
    let mut held = 0i64;
    for k in 0..3 {
        alloc::begin();
        let t = Instant::now();
        let kept = Incidents::open(dir.path());
        times.push(t.elapsed());
        let c = alloc::end();
        if k == 0 {
            held = c.live;
        }
        drop(kept);
    }
    println!("{:<52} {:>10.1}ms  {:.1} MiB held ({:.2}× its bytes on disk)", "opened", median(times).as_secs_f64() * 1e3, held as f64 / 1048576.0, held as f64 / disk as f64);

    let kept = Incidents::open(dir.path());
    println!("\n{:<52} {:>12} {:>10} {:>12} {:>10} {:>8}", "searched (a page of 50, the facets counted)", "in place", "allocs", "copied", "allocs", "matched");
    for q in ["", "state:active", "state:resolved AND severity:(SEV-1 OR SEV-2)", "services:svc-07", "teams:team-3 postmortem:true", "checkout"] {
        let query = search::parse(q).unwrap();
        let (inplace, a1, answer) = measure(|| kept.read_all(|all| serde_json::to_vec(&http::search_page(all, &query, true, 0, 50, None)).unwrap()));
        let (copied, a2, again) = measure(|| {
            let all = kept.list();
            let refs: Vec<&Incident> = all.iter().collect();
            serde_json::to_vec(&http::search_page(&refs, &query, true, 0, 50, None)).unwrap()
        });
        assert_eq!(answer, again, "{q}: the same answer either way");
        let matched = serde_json::from_slice::<Value>(&answer).unwrap().pointer("/data/attributes/total").and_then(Value::as_u64).unwrap_or(0);
        println!("{:<52} {:>10.2}ms {:>10} {:>10.2}ms {:>10} {:>8}", if q.is_empty() { "(all)" } else { q }, inplace.as_secs_f64() * 1e3, a1.calls(), copied.as_secs_f64() * 1e3, a2.calls(), matched);
    }
    let (one, a, _) = measure(|| kept.read("5000", |i| serde_json::to_vec(&http::IncidentOut::of(i)).unwrap()));
    let (value, b, _) = measure(|| kept.read("5000", http::incident_json));
    println!("{:<52} {:>10.3}ms {:>10}", "one read (its page's), by its public id, written", one.as_secs_f64() * 1e3, a.calls());
    println!("{:<52} {:>10.3}ms {:>10}", "one read, as a JSON value (as first answered)", value.as_secs_f64() * 1e3, b.calls());

    // Changed: a note, its own file written; against the store written whole.
    let target = kept.list().iter().find(|i| i.open()).map(|i| i.id.clone()).unwrap();
    let mut note = 0;
    let (own, a, _) = measure(|| {
        note += 1;
        kept.note(&target, &format!("note {note}: rolled back the deploy"), false, "user:bench", T0).unwrap()
    });
    let all = kept.list();
    let whole = dir.path().join("whole.json");
    let (everything, b, _) = measure(|| {
        let bytes = serde_json::to_vec(&all).unwrap();
        write_whole(&whole, &bytes);
        bytes.len()
    });
    println!("\n{:<52} {:>10.2}ms {:>10}", "a note: its incident's file written", own.as_secs_f64() * 1e3, a.calls());
    println!("{:<52} {:>10.2}ms {:>10}", "a note: the whole store written (as first kept)", everything.as_secs_f64() * 1e3, b.calls());
}

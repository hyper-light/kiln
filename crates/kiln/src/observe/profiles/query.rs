//! Profiles asked of (`docs/observe.md` §8.11): uploads chosen by their time and a search of their
//! tags in log search's syntax (`kiln_obs::logs::search`: `service:checkout env:prod -host:web-2`,
//! `version:1.*`), the search decided once a series; listed newest first; their tags' values
//! counted (facets); their sample types; one type of theirs summed into a flame graph, read on a
//! few threads, each its own graph and reader, the graphs merged.

use std::collections::{BTreeMap, HashMap};
use std::ops::ControlFlow;

use kiln_obs::logs::search::{Fields, RowScratch, Search};
use kiln_obs::profile::flame::Flame;

use super::store::{Handle, Store, View};

/// An upload's tags as a record's fields to a search: `service` and `host` its own, its family its
/// `source`, every other tag an attribute (`env:prod`, `@version:1.2`).
struct Tags<'v, 'a>(&'v View<'a>);

impl Fields for Tags<'_, '_> {
    fn message(&self) -> &str {
        ""
    }
    fn service(&self) -> &str {
        self.0.tag("service").unwrap_or("")
    }
    fn host(&self) -> &str {
        self.0.tag("host").unwrap_or("")
    }
    fn source(&self) -> &str {
        self.0.family()
    }
    fn severity(&self) -> u8 {
        0
    }
    fn attr(&self, key: &str) -> Option<&str> {
        self.0.tag(key)
    }
    fn trace_id(&self) -> Option<[u8; 16]> {
        None
    }
    fn span_id(&self) -> Option<[u8; 8]> {
        None
    }
}

/// The uploads of `[from_ns, to_ns]` that `search` matches, newest first, each visited until `each`
/// breaks; the search decided once a series.
pub fn select(store: &Store, from_ns: i64, to_ns: i64, search: &Search, each: &mut dyn FnMut(&View<'_>) -> ControlFlow<()>) {
    let mut decided: HashMap<u32, bool, foldhash::fast::RandomState> = HashMap::default();
    let mut scratch = RowScratch::default();
    store.scan(from_ns, to_ns, &mut |v| {
        let matched = *decided.entry(v.series()).or_insert_with(|| search.fields(&Tags(v), &mut scratch));
        if matched { each(v) } else { ControlFlow::Continue(()) }
    });
}

/// The newest `limit` uploads matched, and how many matched.
pub fn list(store: &Store, from_ns: i64, to_ns: i64, search: &Search, limit: usize) -> (Vec<Handle>, usize) {
    let (mut out, mut matched) = (Vec::new(), 0usize);
    select(store, from_ns, to_ns, search, &mut |v| {
        matched += 1;
        if out.len() < limit {
            out.push(v.handle());
        }
        ControlFlow::Continue(())
    });
    (out, matched)
}

/// Each tag's values among the uploads matched, with how many uploads have each (most first, the
/// first `per_key`), by key; and how many matched.
pub fn facets(store: &Store, from_ns: i64, to_ns: i64, search: &Search, per_key: usize) -> (BTreeMap<String, Vec<(String, u64)>>, usize) {
    let mut by_series: HashMap<u32, (u64, Vec<(String, String)>), foldhash::fast::RandomState> = HashMap::default();
    let mut matched = 0usize;
    select(store, from_ns, to_ns, search, &mut |v| {
        matched += 1;
        by_series.entry(v.series()).or_insert_with(|| (0, v.tags().map(|(k, v)| (k.to_owned(), v.to_owned())).collect())).0 += 1;
        ControlFlow::Continue(())
    });
    let mut counts: BTreeMap<String, HashMap<String, u64>> = BTreeMap::new();
    for (n, tags) in by_series.into_values() {
        for (k, v) in tags {
            *counts.entry(k).or_default().entry(v).or_default() += n;
        }
    }
    let out = counts
        .into_iter()
        .map(|(k, values)| {
            let mut values: Vec<(String, u64)> = values.into_iter().collect();
            values.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            values.truncate(per_key);
            (k, values)
        })
        .collect();
    (out, matched)
}

/// A sample type among the uploads matched: its profile, its type and unit, and how many uploads
/// hold it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kind {
    pub profile: String,
    pub kind: String,
    pub unit: String,
    pub uploads: u64,
}

/// The sample types the uploads matched hold, by profile then type.
pub fn kinds(store: &Store, from_ns: i64, to_ns: i64, search: &Search) -> Vec<Kind> {
    let mut by_shape: HashMap<u32, (u64, Vec<(String, String, String)>), foldhash::fast::RandomState> = HashMap::default();
    select(store, from_ns, to_ns, search, &mut |v| {
        by_shape.entry(v.shape()).or_insert_with(|| (0, v.profiles().flat_map(|(name, types)| types.into_iter().map(move |(t, u)| (name.to_owned(), t.to_owned(), u.to_owned()))).collect())).0 += 1;
        ControlFlow::Continue(())
    });
    let mut counts: BTreeMap<(String, String, String), u64> = BTreeMap::new();
    for (n, kinds) in by_shape.into_values() {
        for k in kinds {
            *counts.entry(k).or_default() += n;
        }
    }
    counts.into_iter().map(|((profile, kind, unit), uploads)| Kind { profile, kind, unit, uploads }).collect()
}

/// A flame graph asked for: of the uploads of `[from_ns, to_ns]` that `search` matches, or of one
/// upload alone; profile `profile`'s type `kind`; `limit` uploads read at the most.
pub struct Asked<'q> {
    pub from_ns: i64,
    pub to_ns: i64,
    pub search: &'q Search,
    pub only: Option<Handle>,
    pub profile: &'q str,
    pub kind: &'q str,
    pub limit: usize,
}

/// A flame graph summed: its unit, how many uploads held the type, were read, and failed to read;
/// whether it is their average (a snapshot's type).
pub struct Summed {
    pub flame: Flame,
    pub unit: String,
    pub matched: usize,
    pub read: usize,
    pub failed: usize,
    pub averaged: bool,
}

/// Whether type `kind` is a snapshot of a moment (the heap live, goroutines), not a count of what
/// happened over the profile's time (CPU, allocations, contention): summed over uploads, a snapshot
/// counts the same memory once an upload, so its graph is their average over time (each process's,
/// summed), as Datadog shows the heap.
pub fn snapshot(kind: &str) -> bool {
    kind.starts_with("inuse") || kind.starts_with("heap-live") || matches!(kind, "goroutine" | "goroutines" | "heap-space" | "live-heap" | "space" | "objects")
}

/// The uploads asked for that hold the type: each one's handle, the type's place in its profile and
/// its series, newest first; and the type's unit.
fn chosen(store: &Store, asked: &Asked<'_>) -> (Vec<(Handle, usize, u32)>, String) {
    let (mut out, mut unit) = (Vec::new(), String::new());
    let mut by_shape: HashMap<u32, Option<(usize, String)>, foldhash::fast::RandomState> = HashMap::default();
    select(store, asked.from_ns, asked.to_ns, asked.search, &mut |v| {
        if asked.only.is_some_and(|h| h != v.handle()) {
            return ControlFlow::Continue(());
        }
        let place = by_shape.entry(v.shape()).or_insert_with(|| v.type_of(asked.profile, asked.kind).map(|(i, u)| (i, u.to_owned())));
        if let Some((i, u)) = place {
            if unit.is_empty() {
                unit.clone_from(u);
            }
            out.push((v.handle(), *i, v.series()));
        }
        if asked.only.is_some() { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
    });
    (out, unit)
}

/// Uploads one thread reads at the least: fewer, and a thread more costs more than it saves.
const PER_THREAD: usize = 8;

/// The flame graph asked for, read on `threads` threads at the most (each at a lower priority than
/// the server's other work). Where more uploads hold the type than `limit`, `limit` of them are
/// read, evenly across the time asked.
pub fn flame(store: &Store, asked: &Asked<'_>, threads: usize) -> Summed {
    let (mut chosen, unit) = chosen(store, asked);
    let matched = chosen.len();
    let limit = asked.limit.max(1);
    if chosen.len() > limit {
        chosen = (0..limit).filter_map(|i| chosen.get(i * matched / limit).copied()).collect();
    }
    let threads = threads.clamp(1, chosen.len().div_ceil(PER_THREAD).max(1));
    let part = chosen.len().div_ceil(threads).max(1);
    let profile = asked.profile;
    let read_part = |part: &[(Handle, usize, u32)]| -> (Flame, usize) {
        // One reader, buffer and pprof scratch a thread, kept from one upload to the next.
        let mut reader = store.reader();
        let mut scratch = kiln_obs::profile::pprof::Scratch::default();
        let (mut g, mut buf, mut failed) = (Flame::new(), Vec::new(), 0usize);
        for &(h, i, _) in part {
            match reader.profile(h, profile, &mut buf) {
                Ok(true) => match kiln_obs::profile::pprof::read_with(&buf, &mut scratch) {
                    Ok(p) => g.add(&p, i),
                    Err(_) => failed += 1,
                },
                // Let go since it was chosen, or damaged.
                Ok(false) | Err(_) => failed += 1,
            }
        }
        (g, failed)
    };
    let parts: Vec<(Flame, usize)> = if threads == 1 {
        vec![read_part(&chosen)]
    } else {
        std::thread::scope(|s| {
            let running: Vec<_> = chosen
                .chunks(part)
                .map(|c| {
                    s.spawn(move || {
                        crate::os::lower_thread_priority();
                        read_part(c)
                    })
                })
                .collect();
            // A thread that panicked (none should: the crate's lints) counts its uploads failed.
            running.into_iter().zip(chosen.chunks(part)).map(|(t, c)| t.join().unwrap_or_else(|_| (Flame::new(), c.len()))).collect()
        })
    };
    let failed: usize = parts.iter().map(|(_, f)| f).sum();
    let mut graphs: Vec<Flame> = parts.into_iter().map(|(g, _)| g).collect();
    // The threads' graphs merged in pairs at once, a round at a time (log2 of them, not one a
    // thread in turn): each pair the one before into the one after it, so the graph is the same
    // whichever thread ends first.
    while graphs.len() > 1 {
        let mut pairs = Vec::with_capacity(graphs.len() / 2);
        let mut rest = graphs.into_iter();
        let mut odd = None;
        loop {
            match (rest.next(), rest.next()) {
                (Some(a), Some(b)) => pairs.push((a, b)),
                (Some(a), None) => {
                    odd = Some(a);
                    break;
                }
                _ => break,
            }
        }
        graphs = if pairs.len() == 1 {
            pairs
                .into_iter()
                .map(|(mut a, b)| {
                    a.merge(&b);
                    a
                })
                .collect()
        } else {
            std::thread::scope(|s| {
                let running: Vec<_> = pairs
                    .into_iter()
                    .map(|(mut a, b)| {
                        s.spawn(move || {
                            crate::os::lower_thread_priority();
                            a.merge(&b);
                            a
                        })
                    })
                    .collect();
                // A thread that panicked (none should: the crate's lints) gives an empty graph.
                running.into_iter().map(|t| t.join().unwrap_or_else(|_| Flame::new())).collect()
            })
        };
        graphs.extend(odd);
    }
    let mut flame = graphs.pop().unwrap_or_default();
    let read = chosen.len() - failed;
    let averaged = snapshot(asked.kind) && read > 1;
    if averaged {
        // Over time: the uploads read, a process's each (a series' each).
        let processes = chosen.iter().map(|&(_, _, s)| s).collect::<std::collections::HashSet<u32>>().len();
        flame.scale(i64::try_from(processes).unwrap_or(i64::MAX), i64::try_from(read).unwrap_or(i64::MAX));
    }
    Summed { flame, unit, matched, read, failed, averaged }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::super::intake::{self, tests as fx};
    use super::*;

    const HOUR_S: i64 = 1_792_771_200;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        for (i, tags) in
            ["service:checkout,env:prod,version:1.4.2,host:web-1", "service:checkout,env:prod,version:1.4.3,host:web-2", "service:search,env:staging,version:2.0,host:web-1"].iter().enumerate()
        {
            for m in 0..4i64 {
                let t = HOUR_S + 60 * m + i as i64;
                let at = time::OffsetDateTime::from_unix_timestamp(t).unwrap().format(&time::format_description::well_known::Rfc3339).unwrap();
                let body = fx::upload_of("b", &[("cpu.pprof", fx::CPU), ("delta-heap.pprof", fx::HEAP)], &at, tags);
                s.put(&[intake::read("multipart/form-data; boundary=b", body).unwrap()], t * 1000).unwrap();
            }
        }
        (dir, s)
    }

    fn search(q: &str) -> Search {
        kiln_obs::logs::search::parse(q).unwrap()
    }

    /// Chosen by log search's syntax over their tags; listed newest first; their tags' values and
    /// their types counted.
    #[test]
    fn uploads_are_chosen_by_a_search_of_their_tags() {
        let (_dir, s) = store();
        let (all, matched) = list(&s, i64::MIN, i64::MAX, &search("service:checkout env:prod -host:web-2"), 3);
        assert_eq!((all.len(), matched), (3, 4));
        assert_eq!(list(&s, i64::MIN, i64::MAX, &search("version:1.4.* OR service:search"), 100).1, 12);
        assert_eq!(list(&s, i64::MIN, i64::MAX, &search("source:go host:web-1"), 100).1, 8);
        assert_eq!(list(&s, (HOUR_S + 60) * 1_000_000_000, (HOUR_S + 122) * 1_000_000_000, &search("*"), 100).1, 6, "by their start");
        let (f, n) = facets(&s, i64::MIN, i64::MAX, &search("env:prod"), 10);
        assert_eq!(n, 8);
        assert_eq!(f["host"], [("web-1".to_owned(), 4), ("web-2".to_owned(), 4)]);
        assert_eq!(f["service"], [("checkout".to_owned(), 8)]);
        let k = kinds(&s, i64::MIN, i64::MAX, &search("*"));
        assert_eq!(k.iter().map(|k| (k.profile.as_str(), k.kind.as_str(), k.uploads)).collect::<Vec<_>>()[..2], [("cpu.pprof", "cpu", 12), ("cpu.pprof", "samples", 12)]);
    }

    /// A type summed over the uploads chosen, on one thread or several alike; `limit` of them read
    /// evenly where more hold it; one upload alone.
    #[test]
    fn a_type_is_summed_over_the_uploads_chosen() {
        let (_dir, s) = store();
        let q = search("service:checkout");
        let asked = |limit, only| Asked { from_ns: i64::MIN, to_ns: i64::MAX, search: &q, only, profile: "cpu.pprof", kind: "cpu", limit };
        let one = flame(&s, &asked(100, None), 1);
        let four = flame(&s, &asked(100, None), 4);
        assert_eq!((one.matched, one.read, one.failed, one.unit.as_str()), (8, 8, 0, "nanoseconds"));
        assert_eq!(one.flame.nodes[0].total, 8 * 1_500_000_000);
        assert_eq!((four.flame.nodes[0].total, four.flame.nodes.len(), four.flame.profiles), (one.flame.nodes[0].total, one.flame.nodes.len(), 8));
        // Every upload (twelve): read on two threads, each its own graph, merged.
        let every = search("*");
        let all = |threads| flame(&s, &Asked { search: &every, ..asked(100, None) }, threads);
        let (one_all, two_all) = (all(1), all(4));
        assert_eq!((two_all.read, two_all.flame.nodes[0].total, two_all.flame.profiles), (12, 12 * 1_500_000_000, 12));
        assert_eq!(two_all.flame.nodes.len(), one_all.flame.nodes.len(), "the same graph");
        // Merged in pairs at once, however many threads: the same graph drawn, the same functions.
        let shown = |f: &Flame| {
            let drawn: Vec<(String, i64, i64)> = f.drawn(0).iter().map(|d| (if d.function == u32::MAX { String::new() } else { f.name(d.function).0.to_owned() }, d.total, d.own)).collect();
            let mut by: Vec<(String, i64, i64)> = f.by_function().iter().map(|&(g, own, total)| (f.name(g).0.to_owned(), own, total)).collect();
            by.sort();
            (drawn, by)
        };
        for threads in [3, 4, 8] {
            let t = all(threads);
            assert_eq!((t.read, t.failed, t.flame.profiles, t.flame.samples), (one_all.read, 0, one_all.flame.profiles, one_all.flame.samples));
            assert_eq!(shown(&t.flame), shown(&one_all.flame), "{threads} threads");
        }
        let some = flame(&s, &asked(3, None), 2);
        assert_eq!((some.matched, some.read, some.flame.nodes[0].total), (8, 3, 3 * 1_500_000_000));
        let h = list(&s, i64::MIN, i64::MAX, &q, 1).0[0];
        let alone = flame(&s, &asked(100, Some(h)), 4);
        assert_eq!((alone.matched, alone.flame.nodes[0].total), (1, 1_500_000_000));
        let heap = flame(&s, &Asked { profile: "delta-heap.pprof", kind: "alloc_space", ..asked(100, None) }, 2);
        assert_eq!((heap.flame.nodes[0].total, heap.unit.as_str(), heap.averaged), (8 * 13_105_703, "bytes", false), "allocations summed");
        // The heap live: its two processes' (two series of checkout), each averaged over its uploads.
        let live = flame(&s, &Asked { profile: "delta-heap.pprof", kind: "inuse_space", ..asked(100, None) }, 2);
        assert_eq!((live.flame.nodes[0].total, live.averaged), (2 * 10_278_807, true), "a snapshot averaged over time, its processes summed");
        let none = flame(&s, &Asked { kind: "nonesuch", ..asked(100, None) }, 2);
        assert_eq!((none.matched, none.read, none.flame.nodes[0].total), (0, 0, 0));
    }
}

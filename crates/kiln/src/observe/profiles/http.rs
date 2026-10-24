//! The profiles' API (`docs/observe.md` §8.11), under `/api/observe/v1/profiles`: uploads listed
//! and read, their tags' values and their sample types counted, a type summed into a flame graph
//! (and its functions by self), an upload's profile as pprof. Times are milliseconds since the
//! epoch (`from`, `to`: the hour before now where unsaid); `query` is a search of the uploads'
//! tags in log search's syntax.

use std::collections::HashMap;

use http::{StatusCode, header};
use kiln_obs::logs::search::Search;
use serde::Serialize;

use super::query::{self, Asked};
use super::store::{Handle, Header, Store};
use crate::http::server::{self as srv, Response};
use crate::observe::api::{Fail, fail};

/// Uploads listed at the most; values a facet gives; functions the top gives; uploads a flame graph
/// reads unless asked for fewer, and at the most.
const LIST_MAX: usize = 500;
const FACET_VALUES: usize = 50;
const TOP: usize = 200;
const FLAME_UPLOADS: usize = 500;
const FLAME_UPLOADS_MAX: usize = 5_000;
/// A flame graph's nodes drawn at the most, and the smallest share of its whole one is drawn at
/// unless asked (one in ten thousand: a pixel's of a graph ten thousand pixels wide).
const DRAWN_MAX: usize = 50_000;
const MIN_SHARE: f64 = 1e-4;

fn store() -> Result<&'static std::sync::Arc<Store>, Fail> {
    super::get().map(super::Profiles::store).ok_or_else(|| fail(StatusCode::SERVICE_UNAVAILABLE, "profiles are kept by the server, and open as it starts"))
}

/// `from` and `to` (ms) as nanoseconds: the hour before `to` where `from` is unsaid, now where `to`
/// is.
fn range(q: &HashMap<String, String>) -> Result<(i64, i64), Fail> {
    let ms = |k: &str| q.get(k).map(|v| v.parse::<i64>().map_err(|_| fail(StatusCode::BAD_REQUEST, format!("?{k}= (ms since the epoch)")))).transpose();
    let to = ms("to")?.unwrap_or_else(crate::observe::clock_ms);
    let from = ms("from")?.unwrap_or(to - 3_600_000);
    if from > to {
        return Err(fail(StatusCode::BAD_REQUEST, "from is after to"));
    }
    Ok((from.saturating_mul(1_000_000), to.saturating_mul(1_000_000).saturating_add(999_999)))
}

fn search(q: &HashMap<String, String>) -> Result<Search, Fail> {
    kiln_obs::logs::search::parse(q.get("query").map_or("", String::as_str)).map_err(|e| fail(StatusCode::BAD_REQUEST, format!("query: {e}")))
}

fn count(q: &HashMap<String, String>, k: &str, default: usize, max: usize) -> Result<usize, Fail> {
    Ok(q.get(k).map(|v| v.parse::<usize>().map_err(|_| fail(StatusCode::BAD_REQUEST, format!("?{k}= (a count)")))).transpose()?.unwrap_or(default).min(max))
}

/// An upload's id: its handle.
pub fn id_of(h: Handle) -> String {
    format!("{}-{}-{}", h.segment, h.head, h.head_len)
}

fn handle_of(id: &str) -> Result<Handle, Fail> {
    let mut it = id.splitn(3, '-');
    let parsed = (|| Some(Handle { segment: it.next()?.parse().ok()?, head: it.next()?.parse().ok()?, head_len: it.next()?.parse().ok()? }))();
    parsed.ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such upload"))
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, Fail> + Send + 'static) -> Result<T, Fail> {
    crate::rt::blocking(f).await.map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
}

#[derive(Serialize)]
struct TypeOut {
    #[serde(rename = "type")]
    kind: String,
    unit: String,
    /// Its values summed over the profile's samples.
    total: i64,
}

#[derive(Serialize)]
struct ProfileOut {
    name: String,
    types: Vec<TypeOut>,
}

/// An upload as listed: its id, times, profiler's language, tags (`key:value`, as its profiler and
/// its Agent said them) and profiles.
#[derive(Serialize)]
struct UploadOut {
    id: String,
    start_ms: i64,
    end_ms: i64,
    family: String,
    tags: Vec<String>,
    profiles: Vec<ProfileOut>,
}

impl UploadOut {
    fn of(h: Handle, head: Header) -> Self {
        Self {
            id: id_of(h),
            start_ms: head.start_ns.div_euclid(1_000_000),
            end_ms: head.end_ns.div_euclid(1_000_000),
            family: head.family,
            tags: head.tags.into_iter().map(|(k, v)| if v.is_empty() { k } else { format!("{k}:{v}") }).collect(),
            profiles: head.profiles.into_iter().map(|(name, types)| ProfileOut { name, types: types.into_iter().map(|(kind, unit, total)| TypeOut { kind, unit, total }).collect() }).collect(),
        }
    }
}

#[derive(Serialize)]
struct ListOut {
    uploads: Vec<UploadOut>,
    /// Uploads the search matched in the time asked.
    matched: usize,
}

/// `GET profiles`: the newest uploads the search matches (`limit`, 50 unless said, 500 at the most).
pub async fn list(q: &HashMap<String, String>) -> Result<Response, Fail> {
    let (s, (from, to), search, limit) = (store()?, range(q)?, search(q)?, count(q, "limit", 50, LIST_MAX)?);
    let out = blocking(move || {
        let (handles, matched) = query::list(s, from, to, &search, limit);
        let mut reader = s.reader();
        let mut uploads = Vec::with_capacity(handles.len());
        for h in handles {
            // One let go since it was listed is passed over.
            if let Ok(Some(head)) = reader.header(h) {
                uploads.push(UploadOut::of(h, head));
            }
        }
        Ok(ListOut { uploads, matched })
    })
    .await?;
    Ok(srv::json(StatusCode::OK, &out))
}

#[derive(Serialize)]
struct EndpointOut {
    endpoint: String,
    count: u64,
}

#[derive(Serialize)]
struct OneOut {
    #[serde(flatten)]
    upload: UploadOut,
    /// Requests by endpoint while it was collected.
    endpoints: Vec<EndpointOut>,
}

#[derive(Serialize)]
struct CountsOut {
    kept: u64,
    /// Refused: the writer's queue full, the disk short of room.
    busy: u64,
    disk: u64,
    /// Read again here and found wrong, or past the store's bounds.
    malformed: u64,
    /// Holding no pprof profile (Java's JFR recordings): not kept.
    not_pprof: u64,
    /// Lost to a write that failed.
    failed: u64,
}

#[derive(Serialize)]
struct StatusOut {
    uploads: usize,
    segments: usize,
    bytes: u64,
    series: usize,
    oldest_ms: Option<i64>,
    newest_ms: Option<i64>,
    /// What became of the uploads the server took since it started.
    counts: CountsOut,
}

/// `GET profiles/status`: the store's size, and what became of the uploads since the server started.
pub fn status() -> Result<Response, Fail> {
    let p = super::get().ok_or_else(|| fail(StatusCode::SERVICE_UNAVAILABLE, "profiles are kept by the server, and open as it starts"))?;
    let (st, c) = (p.store().stats(), p.counts());
    let n = |a: &std::sync::atomic::AtomicU64| a.load(std::sync::atomic::Ordering::Relaxed);
    Ok(srv::json(
        StatusCode::OK,
        &StatusOut {
            uploads: st.uploads,
            segments: st.segments,
            bytes: st.bytes,
            series: st.series,
            oldest_ms: st.oldest_ns.map(|t| t.div_euclid(1_000_000)),
            newest_ms: st.newest_ns.map(|t| t.div_euclid(1_000_000)),
            counts: CountsOut { kept: n(&c.kept), busy: n(&c.busy), disk: n(&c.disk), malformed: n(&c.malformed), not_pprof: n(&c.not_pprof), failed: n(&c.failed) },
        },
    ))
}

/// `GET profiles/{id}`.
pub async fn one(id: &str) -> Result<Response, Fail> {
    let (s, h) = (store()?, handle_of(id)?);
    let out = blocking(move || {
        let mut reader = s.reader();
        let gone = || fail(StatusCode::NOT_FOUND, "no such upload (let go past the retention, or never kept)");
        let head = reader.header(h).map_err(|_| gone())?.ok_or_else(gone)?;
        let endpoints = reader.endpoints(h).map_err(|_| gone())?.unwrap_or_default();
        Ok(OneOut { upload: UploadOut::of(h, head), endpoints: endpoints.into_iter().map(|(endpoint, count)| EndpointOut { endpoint, count }).collect() })
    })
    .await?;
    Ok(srv::json(StatusCode::OK, &out))
}

/// `GET profiles/{id}/{name}`: the upload's profile `name` as pprof, inflated (what `go tool pprof`
/// reads).
pub async fn download(id: &str, name: &str) -> Result<Response, Fail> {
    let (s, h, name) = (store()?, handle_of(id)?, name.to_owned());
    let (bytes, name) = blocking(move || {
        let mut out = Vec::new();
        match s.reader().profile(h, &name, &mut out) {
            Ok(true) => Ok((out, name)),
            Ok(false) | Err(_) => Err(fail(StatusCode::NOT_FOUND, "no such profile in that upload")),
        }
    })
    .await?;
    let r = srv::response(StatusCode::OK, "application/octet-stream", srv::full(bytes));
    let file: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')).collect();
    Ok(srv::with_header(r, header::CONTENT_DISPOSITION, &format!("attachment; filename=\"{file}\"")))
}

#[derive(Serialize)]
struct ValueOut {
    value: String,
    uploads: u64,
}

#[derive(Serialize)]
struct FacetOut {
    key: String,
    values: Vec<ValueOut>,
}

#[derive(Serialize)]
struct FacetsOut {
    facets: Vec<FacetOut>,
    matched: usize,
}

/// Tags a facet list puts first, in this order: Datadog's unified service tags, then where.
const FIRST: [&str; 5] = ["service", "env", "version", "host", "runtime"];

/// `GET profiles/facets`: each tag's values among the uploads matched (the most common 50).
pub async fn facets(q: &HashMap<String, String>) -> Result<Response, Fail> {
    let (s, (from, to), search) = (store()?, range(q)?, search(q)?);
    let out = blocking(move || {
        let (by_key, matched) = query::facets(s, from, to, &search, FACET_VALUES);
        let mut facets: Vec<FacetOut> = by_key.into_iter().map(|(key, values)| FacetOut { key, values: values.into_iter().map(|(value, uploads)| ValueOut { value, uploads }).collect() }).collect();
        facets.sort_by_key(|f| FIRST.iter().position(|k| *k == f.key).unwrap_or(FIRST.len()));
        Ok(FacetsOut { facets, matched })
    })
    .await?;
    Ok(srv::json(StatusCode::OK, &out))
}

#[derive(Serialize)]
struct KindOut {
    profile: String,
    #[serde(rename = "type")]
    kind: String,
    unit: String,
    uploads: u64,
}

#[derive(Serialize)]
struct KindsOut {
    types: Vec<KindOut>,
}

/// `GET profiles/types`: the sample types the uploads matched hold.
pub async fn kinds(q: &HashMap<String, String>) -> Result<Response, Fail> {
    let (s, (from, to), search) = (store()?, range(q)?, search(q)?);
    let types = blocking(move || Ok(query::kinds(s, from, to, &search))).await?;
    Ok(srv::json(StatusCode::OK, &KindsOut { types: types.into_iter().map(|k| KindOut { profile: k.profile, kind: k.kind, unit: k.unit, uploads: k.uploads }).collect() }))
}

/// A flame graph as drawn: its functions (their names and files), its nodes root first, each
/// node's children after it by name, and its functions by self.
#[derive(Serialize)]
struct FlameOut {
    profile: String,
    #[serde(rename = "type")]
    kind: String,
    unit: String,
    /// Uploads that held the type, were read (`limit` at the most, evenly across the time), and
    /// failed to read; whether the graph is their average over time (a snapshot's type: the heap
    /// live, goroutines) rather than their sum.
    averaged: bool,
    matched: usize,
    read: usize,
    failed: usize,
    samples: u64,
    /// The whole: the root's total.
    total: i64,
    names: Vec<String>,
    files: Vec<String>,
    /// Each node drawn: `[function, parent, total, self]`, its function a place in `names` (the
    /// root's -1), its parent a place in `nodes`; a node under one in ten thousand of the whole
    /// (`min_share`), or past 50,000 nodes, left out with all under it.
    nodes: Vec<(i64, u32, i64, i64)>,
    /// Functions by self, most first: `[function, self, total]` (a recursive function's total
    /// counted once).
    top: Vec<(u32, i64, i64)>,
}

/// `GET profiles/flame?profile=&type=`: profile `profile`'s type `type` (`cpu.pprof`, `cpu`) summed
/// over the uploads matched (`limit` read, 500 unless said, 5,000 at the most, evenly across the
/// time), or of one (`id`).
pub async fn flame(q: &HashMap<String, String>) -> Result<Response, Fail> {
    let (s, (from, to), search) = (store()?, range(q)?, search(q)?);
    let profile = q.get("profile").cloned().ok_or_else(|| fail(StatusCode::BAD_REQUEST, "?profile= (the profile's name: cpu.pprof)"))?;
    let kind = q.get("type").cloned().ok_or_else(|| fail(StatusCode::BAD_REQUEST, "?type= (its sample type: cpu)"))?;
    let only = q.get("id").map(|id| handle_of(id)).transpose()?;
    let limit = count(q, "limit", FLAME_UPLOADS, FLAME_UPLOADS_MAX)?;
    let min_share = q.get("min_share").map(|v| v.parse::<f64>().map_err(|_| fail(StatusCode::BAD_REQUEST, "?min_share= (a share of the whole)"))).transpose()?.unwrap_or(MIN_SHARE).clamp(0.0, 1.0);
    let threads = std::thread::available_parallelism().map_or(1, |n| (n.get() / 4).clamp(1, 8));
    let out = blocking(move || {
        let asked = Asked { from_ns: if only.is_some() { i64::MIN } else { from }, to_ns: if only.is_some() { i64::MAX } else { to }, search: &search, only, profile: &profile, kind: &kind, limit };
        let summed = query::flame(s, &asked, threads);
        Ok(drawn(summed, profile.clone(), kind.clone(), min_share))
    })
    .await?;
    Ok(srv::json(StatusCode::OK, &out))
}

/// One side of a comparison: its uploads (held the type, read, failed), its whole and samples.
#[derive(Serialize)]
struct SideOut {
    matched: usize,
    read: usize,
    failed: usize,
    total: i64,
    samples: u64,
}

impl SideOut {
    fn of(s: &query::Summed) -> Self {
        Self { matched: s.matched, read: s.read, failed: s.failed, total: s.flame.nodes.first().map_or(0, |r| r.total), samples: s.flame.samples }
    }
}

/// Two graphs compared: their functions, the union of their paths drawn as one side sizes them,
/// and their functions by how much their self's share of each whole changed.
#[derive(Serialize)]
struct CompareOut {
    profile: String,
    #[serde(rename = "type")]
    kind: String,
    unit: String,
    averaged: bool,
    /// What is compared, and what it is compared with.
    after: SideOut,
    before: SideOut,
    names: Vec<String>,
    files: Vec<String>,
    /// Each node drawn: `[function, parent, total after, self after, total before, self before]`,
    /// as `nodes` of a flame graph, sized by `by`'s side.
    nodes: Vec<(i64, u32, i64, i64, i64, i64)>,
    /// Functions by how much their self's share of the whole changed, most first: `[function, self
    /// after, self before, total after, total before]`.
    top: Vec<(u32, i64, i64, i64, i64)>,
}

/// `GET profiles/compare?profile=&type=`: a type summed over the uploads matched (or one, `id`) and
/// over another time's or search's (`base_from`, `base_to`, `base_query` (the same search unless
/// said), or `base_id`), compared path by path; drawn as the first's totals size it, or the
/// other's (`by=before`).
pub async fn compare(q: &HashMap<String, String>) -> Result<Response, Fail> {
    let (s, (from, to), search) = (store()?, range(q)?, search(q)?);
    let profile = q.get("profile").cloned().ok_or_else(|| fail(StatusCode::BAD_REQUEST, "?profile= (the profile's name: cpu.pprof)"))?;
    let kind = q.get("type").cloned().ok_or_else(|| fail(StatusCode::BAD_REQUEST, "?type= (its sample type: cpu)"))?;
    let only = q.get("id").map(|id| handle_of(id)).transpose()?;
    let base_only = q.get("base_id").map(|id| handle_of(id)).transpose()?;
    let ms = |k: &str| q.get(k).map(|v| v.parse::<i64>().map_err(|_| fail(StatusCode::BAD_REQUEST, format!("?{k}= (ms since the epoch)")))).transpose();
    let (base_from, base_to) = match (ms("base_from")?, ms("base_to")?) {
        (Some(f), Some(t)) if f <= t => (f.saturating_mul(1_000_000), t.saturating_mul(1_000_000).saturating_add(999_999)),
        (None, None) => (from, to),
        _ => return Err(fail(StatusCode::BAD_REQUEST, "?base_from= and ?base_to=, both (ms since the epoch), the first the earlier")),
    };
    let base_search = match q.get("base_query") {
        Some(b) => kiln_obs::logs::search::parse(b).map_err(|e| fail(StatusCode::BAD_REQUEST, format!("base_query: {e}")))?,
        None => search.clone(),
    };
    let by_before = q.get("by").is_some_and(|b| b == "before");
    let limit = count(q, "limit", FLAME_UPLOADS, FLAME_UPLOADS_MAX)?;
    let min_share = q.get("min_share").map(|v| v.parse::<f64>().map_err(|_| fail(StatusCode::BAD_REQUEST, "?min_share= (a share of the whole)"))).transpose()?.unwrap_or(MIN_SHARE).clamp(0.0, 1.0);
    let threads = std::thread::available_parallelism().map_or(1, |n| (n.get() / 4).clamp(1, 8));
    let out = blocking(move || {
        let side = |from_ns: i64, to_ns: i64, search: &Search, only: Option<Handle>| {
            let (from_ns, to_ns) = if only.is_some() { (i64::MIN, i64::MAX) } else { (from_ns, to_ns) };
            query::flame(s, &Asked { from_ns, to_ns, search, only, profile: &profile, kind: &kind, limit }, threads)
        };
        let after = side(from, to, &search, only);
        let before = side(base_from, base_to, &base_search, base_only);
        let c = before.flame.compare(&after.flame);
        let whole = if by_before { c.flame.nodes.first().map_or(0, |r| r.total) } else { c.second.first().map_or(0, |r| r.0) };
        let nodes = c.drawn(!by_before, (whole as f64 * min_share).ceil() as i64);
        let shares = [before.flame.nodes.first().map_or(0, |r| r.total).max(1) as f64, after.flame.nodes.first().map_or(0, |r| r.total).max(1) as f64];
        let mut by = c.by_function();
        // Most changed first: its self's share of its whole, after less before; alike, by name.
        by.sort_by(|a, b| {
            let d = |x: &(u32, [i64; 2], [i64; 2])| (x.1[1] as f64 / shares[1] - x.1[0] as f64 / shares[0]).abs();
            d(b).total_cmp(&d(a)).then_with(|| c.flame.name(a.0).cmp(&c.flame.name(b.0)))
        });
        by.truncate(TOP);
        let (mut names, mut files, mut place) = (Vec::new(), Vec::new(), HashMap::<u32, u32>::new());
        let mut name_of = |f: u32| -> u32 {
            *place.entry(f).or_insert_with(|| {
                let (n, file) = c.flame.name(f);
                names.push(n.to_owned());
                files.push(file.to_owned());
                u32::try_from(names.len() - 1).unwrap_or(u32::MAX)
            })
        };
        let nodes = nodes.iter().map(|d| (if d.function == u32::MAX { -1 } else { i64::from(name_of(d.function)) }, d.parent, d.total[1], d.own[1], d.total[0], d.own[0])).collect();
        let top = by.iter().map(|&(f, own, total)| (name_of(f), own[1], own[0], total[1], total[0])).collect();
        Ok(CompareOut {
            profile: profile.clone(),
            kind: kind.clone(),
            unit: if after.unit.is_empty() { before.unit.clone() } else { after.unit.clone() },
            averaged: after.averaged || before.averaged,
            after: SideOut::of(&after),
            before: SideOut::of(&before),
            names,
            files,
            nodes,
            top,
        })
    })
    .await?;
    Ok(srv::json(StatusCode::OK, &out))
}

/// A graph summed, drawn: nodes under `min_share` of the whole, or past [`DRAWN_MAX`], left out.
fn drawn(summed: query::Summed, profile: String, kind: String, min_share: f64) -> FlameOut {
    let g = &summed.flame;
    let whole = g.nodes.first().map_or(0, |r| r.total);
    let mut min_total = (whole as f64 * min_share).ceil() as i64;
    if g.nodes.len() > DRAWN_MAX {
        // The DRAWN_MAX-th largest total: no more nodes than that are at least it (a node's total
        // is at least each of its children's, so what is left out takes all under it).
        let mut totals: Vec<i64> = g.nodes.iter().map(|n| n.total).collect();
        let (_, kth, _) = totals.select_nth_unstable_by(DRAWN_MAX - 1, |a, b| b.cmp(a));
        min_total = min_total.max(kth.saturating_add(1));
    }
    let nodes = g.drawn(min_total);
    let mut by_function = g.by_function();
    by_function.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2)).then_with(|| g.name(a.0).cmp(&g.name(b.0))));
    by_function.truncate(TOP);
    // Functions named once, as they are first drawn or listed.
    let (mut names, mut files, mut place) = (Vec::new(), Vec::new(), HashMap::<u32, u32>::new());
    let mut name_of = |f: u32| -> u32 {
        *place.entry(f).or_insert_with(|| {
            let (n, file) = g.name(f);
            names.push(n.to_owned());
            files.push(file.to_owned());
            u32::try_from(names.len() - 1).unwrap_or(u32::MAX)
        })
    };
    let nodes: Vec<(i64, u32, i64, i64)> = nodes.iter().map(|d| (if d.function == u32::MAX { -1 } else { i64::from(name_of(d.function)) }, d.parent, d.total, d.own)).collect();
    let top: Vec<(u32, i64, i64)> = by_function.iter().map(|&(f, own, total)| (name_of(f), own, total)).collect();
    FlameOut {
        profile,
        kind,
        unit: summed.unit,
        averaged: summed.averaged,
        matched: summed.matched,
        read: summed.read,
        failed: summed.failed,
        samples: g.samples,
        total: whole,
        names,
        files,
        nodes,
        top,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// A graph of 60,000 leaves under one function, each its own value, drawn within 50,000 nodes:
    /// the largest kept, every node's parent drawn before it, the root's whole kept; the functions
    /// by self, the largest first.
    #[test]
    fn a_graph_past_its_bound_is_drawn_within_it() {
        use kiln_obs::profile::pprof::{Function, Profile, Sample, ValueType};
        let names: Vec<String> = (0..60_001).map(|i| format!("f{i}")).collect();
        let functions: Vec<Function<'_>> = names.iter().map(|n| Function { name: n, file: "x.go" }).collect();
        let (mut stacks, mut values, mut samples) = (Vec::new(), Vec::new(), Vec::new());
        for i in 1..=60_000u32 {
            let at = stacks.len() as u32;
            // Leaf first: f<i> under f0.
            stacks.extend([i, 0]);
            values.push(i64::from(i));
            samples.push(Sample { stack: (at, at + 2), values: (i - 1, i), labels: (0, 0) });
        }
        let p = Profile { sample_types: vec![ValueType { kind: "cpu", unit: "nanoseconds" }], functions, stacks, values, samples, ..Profile::default() };
        let mut flame = kiln_obs::profile::flame::Flame::new();
        flame.add(&p, 0);
        let summed = query::Summed { flame, unit: "nanoseconds".into(), matched: 1, read: 1, failed: 0, averaged: false };
        let out = drawn(summed, "cpu.pprof".into(), "cpu".into(), 0.0);
        assert!(out.nodes.len() <= DRAWN_MAX, "{} nodes", out.nodes.len());
        assert!(out.nodes.len() > DRAWN_MAX - 10, "{} nodes", out.nodes.len());
        assert!(out.nodes.iter().enumerate().skip(1).all(|(i, n)| (n.1 as usize) < i), "a parent before its children");
        assert_eq!(out.total, (1..=60_000i64).sum::<i64>());
        let smallest_kept = out.nodes.iter().skip(2).map(|n| n.2).min().unwrap();
        assert!(smallest_kept > 9_000, "the largest kept: {smallest_kept}");
        assert_eq!((out.top[0].1, out.names[out.top[0].0 as usize].as_str()), (60_000, "f60000"));
    }

    /// An id is its upload's handle, and back; another string is no upload.
    #[test]
    fn an_id_names_its_upload() {
        let h = Handle { segment: 1_792_771_200_000, head: 8, head_len: 412 };
        assert_eq!(handle_of(&id_of(h)).unwrap(), h);
        assert!(handle_of("1-2").is_err());
        assert!(handle_of("a-b-c").is_err());
    }
}

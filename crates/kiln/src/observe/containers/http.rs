//! Containers' API (`docs/observe.md` §10.10), kiln's: every container listed (running, stopped or
//! both; found by a part of its name, image, id, pod, namespace or host, or by a tag, `*` any part
//! of its value; ordered by a figure; a page), and one whole (by its id, or the first 12 or more of
//! its hex as `docker ps` shows it).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use ::http::StatusCode;
use serde::Serialize;
use serde::ser::{SerializeSeq as _, Serializer};

use super::super::api::{Fail, fail};
use super::inventory::{Container, Figure, Inventory};
use crate::http::server::{self as srv, Response};

/// A page's containers when it does not say, and at the most.
pub const COUNT: usize = 100;
pub const COUNT_MAX: usize = 1_000;

fn param<'a>(q: &'a HashMap<String, String>, k: &str) -> Option<&'a str> {
    q.get(k).map(|v| v.trim()).filter(|v| !v.is_empty())
}

/// A term of a search, read once for every container it is matched against.
enum Term<'a> {
    /// `key:value`, `*` any part of the value: one of its tags, `container_id` among them.
    Tag(&'a str, &'a str),
    /// `key*:value`, a key's glob too: a tag matched whole, `key:value`.
    Glob(&'a str),
    /// A part of its name, image, id, pod, namespace or host, case aside: lowercased, found by
    /// memchr's search (SIMD's) in each of them lowercased.
    Text(Box<memchr::memmem::Finder<'static>>),
}

/// A search (`filter`): containers matching each of its terms.
pub struct Filter<'a> {
    terms: Vec<Term<'a>>,
    /// A tag written `key:value` for a key's glob.
    buf: String,
    /// A name lowercased, for a part to be found in it.
    lower: Vec<u8>,
}

/// `s` lowercased into `out`: a byte at a time where it is ASCII (most names), else a character at
/// a time.
fn lowered(s: &str, out: &mut Vec<u8>) {
    out.clear();
    if s.is_ascii() {
        out.extend(s.bytes().map(|b| b.to_ascii_lowercase()));
    } else {
        let mut b = [0u8; 4];
        for c in s.chars().flat_map(char::to_lowercase) {
            out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
        }
    }
}

impl<'a> Filter<'a> {
    pub fn parse(filter: &'a str) -> Self {
        let terms = filter
            .split_whitespace()
            .map(|t| match t.split_once(':') {
                Some((k, _)) if !k.is_empty() && k.contains(['*', '?']) => Term::Glob(t),
                Some((k, v)) if !k.is_empty() => Term::Tag(k, v),
                _ => {
                    let mut needle = Vec::new();
                    lowered(t, &mut needle);
                    Term::Text(Box::new(memchr::memmem::Finder::new(&needle).into_owned()))
                }
            })
            .collect();
        Self { terms, buf: String::new(), lower: Vec::new() }
    }

    /// Whether `c` matches every term: nothing allocated.
    pub fn matches(&mut self, c: &Container) -> bool {
        let (buf, lower) = (&mut self.buf, &mut self.lower);
        self.terms.iter().all(|t| match t {
            Term::Tag(k, v) => (*k == "container_id" && kiln_obs::index::glob(v, &c.id)) || c.tags.iter().any(|(tk, tv)| tk == k && kiln_obs::index::glob(v, tv)),
            Term::Glob(whole) => {
                let mut tagged = |k: &str, v: &str| {
                    buf.clear();
                    let _ = write!(buf, "{k}:{v}");
                    kiln_obs::index::glob(whole, buf)
                };
                tagged("container_id", &c.id) || c.tags.iter().any(|(k, v)| tagged(k, v))
            }
            Term::Text(f) => {
                let mut has = |s: Option<&str>| {
                    s.is_some_and(|s| {
                        lowered(s, lower);
                        f.find(lower).is_some()
                    })
                };
                has(Some(&c.id)) || has(c.name()) || has(c.image()) || has(c.host()) || has(c.tag(&["pod_name", "k8s.pod.name"])) || has(c.tag(&["kube_namespace", "k8s.namespace.name"]))
            }
        })
    }
}

/// A figure to order by, missing ones last whichever way.
#[allow(clippy::cast_precision_loss)]
fn sort_key(c: &Container, field: &str) -> Option<f64> {
    let f = &c.figures;
    match field {
        "cpu" => f.cpu,
        // As the list shows it: its working set, else its usage (Docker's collector gives a
        // working set alone).
        "memory" => f.working_set.or(f.memory),
        // Of its limit: the nearest to theirs first.
        "cpu_limit" => f.cpu.zip(f.cpu_limit).filter(|(_, l)| *l > 0.0).map(|(u, l)| u / l),
        "memory_limit" => f.working_set.or(f.memory).zip(f.memory_limit).filter(|(_, l)| *l > 0.0).map(|(u, l)| u / l),
        "throttled" => f.throttled,
        "io" => Some(f.io_read.unwrap_or(0.0) + f.io_write.unwrap_or(0.0)),
        "uptime" => f.uptime,
        "last_reported" => Some(c.last_ms as f64),
        _ => None,
    }
}

/// A figure as given: to hundredths.
fn round(v: Option<f64>) -> Option<f64> {
    v.map(|v| (v * 100.0).round() / 100.0)
}

#[derive(Serialize)]
struct ImageOut<'a> {
    name: Option<&'a str>,
    tag: Option<&'a str>,
    short: Option<&'a str>,
}

#[derive(Serialize)]
struct KubeOut<'a> {
    namespace: Option<&'a str>,
    pod: Option<&'a str>,
    container: Option<&'a str>,
    qos: Option<&'a str>,
}

#[derive(Serialize)]
struct ComposeOut<'a> {
    service: Option<&'a str>,
    project: Option<&'a str>,
}

#[derive(Serialize)]
struct FiguresOut {
    cpu_nanocores: Option<f64>,
    cpu_limit_nanocores: Option<f64>,
    cpu_throttled_ns: Option<f64>,
    memory_bytes: Option<f64>,
    working_set_bytes: Option<f64>,
    memory_limit_bytes: Option<f64>,
    pids: Option<f64>,
    io_read_bytes: Option<f64>,
    io_write_bytes: Option<f64>,
    uptime_seconds: Option<f64>,
}

/// Its tags, each `key:value` written as it is given.
struct TagsOut<'a>(&'a [(&'static str, Arc<str>)]);

impl Serialize for TagsOut<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(self.0.len()))?;
        for (k, v) in self.0 {
            seq.serialize_element(&TagOut(k, v))?;
        }
        seq.end()
    }
}

struct TagOut<'a>(&'a str, &'a str);

impl Serialize for TagOut<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(&format_args!("{}:{}", self.0, self.1))
    }
}

/// A container as the API gives one, borrowed from the list.
#[derive(Serialize)]
pub struct ContainerOut<'a> {
    id: &'a str,
    name: Option<&'a str>,
    state: &'static str,
    host: Option<&'a str>,
    runtime: Option<&'a str>,
    image: ImageOut<'a>,
    kube: KubeOut<'a>,
    compose: ComposeOut<'a>,
    last_reported: i64,
    series: usize,
    figures: FiguresOut,
    tags: TagsOut<'a>,
}

impl<'a> ContainerOut<'a> {
    pub fn of(c: &'a Container, now_ms: i64) -> Self {
        let f = &c.figures;
        Self {
            id: &c.id,
            name: c.name(),
            state: if c.running(now_ms) { "running" } else { "stopped" },
            host: c.host(),
            runtime: c.tag(&["runtime", "container.runtime"]),
            image: ImageOut { name: c.tag(&["image_name", "container.image.name"]), tag: c.tag(&["image_tag", "container.image.tag"]), short: c.tag(&["short_image"]) },
            kube: KubeOut {
                namespace: c.tag(&["kube_namespace", "k8s.namespace.name"]),
                pod: c.tag(&["pod_name", "k8s.pod.name"]),
                container: c.tag(&["kube_container_name", "k8s.container.name"]),
                qos: c.tag(&["kube_qos"]),
            },
            compose: ComposeOut { service: c.tag(&["compose_service"]), project: c.tag(&["compose_project"]) },
            last_reported: c.last_ms,
            series: c.series,
            figures: FiguresOut {
                cpu_nanocores: round(f.cpu),
                cpu_limit_nanocores: round(f.cpu_limit),
                cpu_throttled_ns: round(f.throttled),
                memory_bytes: round(f.memory),
                working_set_bytes: round(f.working_set),
                memory_limit_bytes: round(f.memory_limit),
                pids: round(f.pids),
                io_read_bytes: round(f.io_read),
                io_write_bytes: round(f.io_write),
                uptime_seconds: round(f.uptime),
            },
            tags: TagsOut(&c.tags),
        }
    }
}

/// The containers `q` asks for (`state`: running unless said, stopped, all; `filter`; `sort_field`
/// of name, cpu, memory, cpu_limit, memory_limit, throttled, io, uptime or last_reported;
/// `sort_dir`; `start`, `count`), and how many matched.
pub fn listed<'a>(inv: &'a Inventory, q: &HashMap<String, String>) -> Result<(Vec<&'a Container>, usize), Fail> {
    let now = inv.at_ms;
    let state = param(q, "state").unwrap_or("running");
    if !matches!(state, "running" | "stopped" | "all") {
        return Err(fail(StatusCode::BAD_REQUEST, "state: running, stopped or all"));
    }
    let field = param(q, "sort_field").unwrap_or("name");
    if !matches!(field, "name" | "cpu" | "memory" | "cpu_limit" | "memory_limit" | "throttled" | "io" | "uptime" | "last_reported") {
        return Err(fail(StatusCode::BAD_REQUEST, "sort_field: name, cpu, memory, cpu_limit, memory_limit, throttled, io, uptime or last_reported"));
    }
    let desc = match param(q, "sort_dir") {
        None => field != "name",
        Some("asc") => false,
        Some("desc") => true,
        Some(_) => return Err(fail(StatusCode::BAD_REQUEST, "sort_dir: asc or desc")),
    };
    let num = |k: &str, d: usize| param(q, k).map_or(Ok(d), |v| v.parse::<usize>().map_err(|_| fail(StatusCode::BAD_REQUEST, format!("{k}: a number"))));
    let (start, count) = (num("start", 0)?, num("count", COUNT)?.clamp(1, COUNT_MAX));
    let mut filter = Filter::parse(param(q, "filter").unwrap_or(""));
    let mut list: Vec<&Container> = inv.containers.iter().filter(|c| (state == "all" || (state == "running") == c.running(now)) && filter.matches(c)).collect();
    let matched = list.len();
    if field == "name" {
        if desc {
            list.reverse();
        }
    } else {
        // Each one's figure read once; then what the page needs put first (a selection, linear),
        // and that alone ordered.
        let mut keyed: Vec<(Option<f64>, &Container)> = list.iter().map(|c| (sort_key(c, field), *c)).collect();
        let order = |(x, a): &(Option<f64>, &Container), (y, b): &(Option<f64>, &Container)| {
            let by = match (x, y) {
                (Some(x), Some(y)) => {
                    if desc {
                        y.total_cmp(x)
                    } else {
                        x.total_cmp(y)
                    }
                }
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            };
            by.then_with(|| a.id.cmp(&b.id))
        };
        let shown = start.saturating_add(count).min(matched);
        if shown < matched {
            keyed.select_nth_unstable_by(shown, order);
            keyed.truncate(shown);
        }
        keyed.sort_unstable_by(order);
        list.clear();
        list.extend(keyed.into_iter().map(|(_, c)| c));
    }
    list.drain(..start.min(list.len()));
    list.truncate(count);
    Ok((list, matched))
}

#[derive(Serialize)]
struct TotalsOut {
    running: usize,
    stopped: usize,
}

/// A list's page as the API answers it.
#[derive(Serialize)]
pub struct ListOut<'a> {
    containers: Vec<ContainerOut<'a>>,
    total_matching: usize,
    total_returned: usize,
    totals: TotalsOut,
}

/// The page `q` asks for.
pub fn list_out<'a>(inv: &'a Inventory, q: &HashMap<String, String>) -> Result<ListOut<'a>, Fail> {
    let (page, matched) = listed(inv, q)?;
    Ok(ListOut {
        total_returned: page.len(),
        containers: page.into_iter().map(|c| ContainerOut::of(c, inv.at_ms)).collect(),
        total_matching: matched,
        totals: TotalsOut { running: inv.running, stopped: inv.containers.len() - inv.running },
    })
}

/// `GET containers`.
pub async fn list(q: &HashMap<String, String>) -> Result<Response, Fail> {
    let inv = super::inventory::get().inventory().await?;
    Ok(srv::json(StatusCode::OK, &list_out(&inv, q)?))
}

/// The containers on `host`: running ones, then those stopped within the hour; each by name.
pub fn on_host<'a>(inv: &'a Inventory, host: &str) -> Vec<ContainerOut<'a>> {
    let mut on: Vec<&Container> = inv.containers.iter().filter(|c| c.host() == Some(host)).collect();
    on.sort_by_key(|c| !c.running(inv.at_ms));
    on.into_iter().map(|c| ContainerOut::of(c, inv.at_ms)).collect()
}

/// The container `id` names: its id, or the first 12 or more of its hex (one alone).
pub fn find<'a>(inv: &'a Inventory, id: &str) -> Option<&'a Container> {
    inv.containers.iter().find(|c| c.id == id).or_else(|| {
        if id.len() < 12 {
            return None;
        }
        let mut it = inv.containers.iter().filter(|c| c.id.starts_with(id));
        let one = it.next()?;
        it.next().is_none().then_some(one)
    })
}

/// A line of a container's chart: its name and the query that draws it.
#[derive(Serialize)]
struct LineOut {
    name: &'static str,
    query: String,
}

/// A container's charts, each line drawn by the first of its figure's sources that reported it
/// (kiln's check's series, or an OpenTelemetry collector's in its own names and units): a line no
/// source reported is not there.
#[derive(Serialize)]
struct ChartsOut {
    /// Cores.
    cpu: Vec<LineOut>,
    /// Bytes.
    memory: Vec<LineOut>,
    /// Milliseconds a second.
    throttled: Vec<LineOut>,
    /// Bytes a second.
    io: Vec<LineOut>,
}

impl ChartsOut {
    fn of(c: &Container) -> Self {
        let lines = |of: &[(Figure, &'static str)]| of.iter().filter_map(|&(f, name)| Some(LineOut { name, query: c.query(f)? })).collect();
        Self {
            cpu: lines(&[(Figure::Cpu, "In use"), (Figure::CpuLimit, "Limit")]),
            memory: lines(&[(Figure::Memory, "In use"), (Figure::WorkingSet, "Working set"), (Figure::MemoryLimit, "Limit")]),
            throttled: lines(&[(Figure::Throttled, "Throttled")]),
            io: lines(&[(Figure::IoRead, "Read"), (Figure::IoWrite, "Written")]),
        }
    }
}

/// A container as `GET containers/{id}` gives it: as listed, and its charts.
#[derive(Serialize)]
struct OneOut<'a> {
    #[serde(flatten)]
    container: ContainerOut<'a>,
    charts: ChartsOut,
}

/// `GET containers/{id}`.
pub async fn one(id: &str) -> Result<Response, Fail> {
    let inv = super::inventory::get().inventory().await?;
    let c = find(&inv, id).ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such container (listed an hour past its last sample)"))?;
    Ok(srv::json(StatusCode::OK, &OneOut { container: ContainerOut::of(c, inv.at_ms), charts: ChartsOut::of(c) }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::super::inventory::Figures;
    use super::*;

    fn container(id: &str, name: &str, host: &str, cpu: Option<f64>, limit: Option<f64>, last_ms: i64) -> Container {
        Container {
            id: id.into(),
            series: 10,
            last_ms,
            tags: vec![("host", host.into()), ("container_name", name.into()), ("image_name", format!("shop/{name}").into()), ("kube_namespace", "shop".into())],
            figures: Figures { cpu, cpu_limit: limit, ..Figures::default() },
            drawn: [None; super::super::inventory::FIGURES],
        }
    }

    fn q(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    /// Listed as asked: running unless said; found by a part of a name, by a tag's glob; ordered
    /// by a figure (missing ones last), by its share of its limit; a page; found by a prefix of its id.
    #[test]
    fn containers_are_listed_as_asked() {
        let now = 1_000_000_000;
        let mut inv = Inventory {
            at_ms: now,
            running: 0,
            containers: vec![
                container(&format!("a1{}", "0".repeat(62)), "api", "node-1", Some(9e8), Some(1e9), now),
                container(&format!("b2{}", "0".repeat(62)), "batch", "node-2", Some(2e8), None, now - 10_000),
                container(&format!("c3{}", "0".repeat(62)), "cache", "node-1", None, None, now - 600_000),
            ],
        };
        inv.running = inv.containers.iter().filter(|c| c.running(now)).count();
        let names = |q: HashMap<String, String>| -> Vec<String> { listed(&inv, &q).unwrap().0.iter().filter_map(|c| c.name().map(str::to_owned)).collect() };
        assert_eq!(names(q(&[])), ["api", "batch"], "running unless said");
        assert_eq!(names(q(&[("state", "stopped")])), ["cache"]);
        assert_eq!(names(q(&[("state", "all"), ("filter", "host:node-1")])), ["api", "cache"]);
        assert_eq!(names(q(&[("filter", "image_name:shop/b*")])), ["batch"]);
        assert_eq!(names(q(&[("filter", "PI")])), ["api"], "a part of its name, case aside");
        assert_eq!(names(q(&[("sort_field", "cpu")])), ["api", "batch"], "the busiest first");
        assert_eq!(names(q(&[("state", "all"), ("sort_field", "cpu"), ("sort_dir", "asc")])), ["batch", "api", "cache"], "missing last whichever way");
        assert_eq!(names(q(&[("sort_field", "cpu_limit")])), ["api", "batch"], "a share of its limit first, none last");
        assert_eq!(names(q(&[("count", "1"), ("start", "1")])), ["batch"]);
        assert!(listed(&inv, &q(&[("sort_field", "size")])).is_err() && listed(&inv, &q(&[("state", "gone")])).is_err());
        let id = format!("b2{}", "0".repeat(62));
        assert_eq!(find(&inv, &id[..12]).map(|c| c.name()), Some(Some("batch")));
        assert!(find(&inv, &id[..11]).is_none() && find(&inv, "000000000000").is_none(), "too short, or not one alone");
        let v = serde_json::to_value(ContainerOut::of(&inv.containers[0], now)).unwrap();
        assert_eq!((v["state"].as_str(), v["image"]["name"].as_str(), v["figures"]["cpu_nanocores"].as_f64()), (Some("running"), Some("shop/api"), Some(9e8)));
        assert_eq!(v["tags"][1], "container_name:api", "its tags, each key:value");
        let page = serde_json::to_value(list_out(&inv, &q(&[("state", "all"), ("count", "2")])).unwrap()).unwrap();
        assert_eq!((page["total_matching"].as_u64(), page["total_returned"].as_u64(), page["totals"]["running"].as_u64(), page["totals"]["stopped"].as_u64()), (Some(3), Some(2), Some(2), Some(1)));
        assert_eq!(on_host(&inv, "node-1").iter().map(|c| c.id).collect::<Vec<_>>(), [inv.containers[0].id.as_str(), inv.containers[2].id.as_str()], "running first");
        assert_eq!(names(q(&[("state", "all"), ("filter", "kube_*:shop host:node-2")])), ["batch"], "a key's glob");
        assert_eq!(names(q(&[("state", "all"), ("filter", "container_id:c3*")])), ["cache"], "by its id's glob");
        assert!(names(q(&[("count", "5"), ("start", "9")])).is_empty(), "past the end");
        let all = names(q(&[("state", "all"), ("sort_field", "cpu"), ("sort_dir", "asc")]));
        let paged: Vec<String> = (0..3).flat_map(|i| names(q(&[("state", "all"), ("sort_field", "cpu"), ("sort_dir", "asc"), ("count", "1"), ("start", &i.to_string())]))).collect();
        assert_eq!(paged, all, "a page at a time, the whole order");
    }
}

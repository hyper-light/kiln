//! The infrastructure list's API (`docs/observe.md` §10.9). At kiln's paths (`/api/observe/v1/…`),
//! with a session: Datadog's host list and totals as its API answers them (`GET /api/v1/hosts`, its
//! `filter`, `sort_field`, `sort_dir`, `start`, `count`, `from`, `include_hosts_metadata`;
//! `/api/v1/hosts/totals`), each host with kiln's own beside (its status, figures, platform, runner,
//! containers); and a host muted and unmuted (`POST /api/v1/host/{name}/mute|unmute`): a downtime
//! of every monitor's groups of the host, as Datadog's mute makes. At Datadog's own paths the
//! ingest token, a writer's, reads and mutes nothing.

use std::collections::HashMap;

use ::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use super::super::api::{Fail, fail};
use super::{Host, Infra, Inventory, LISTED_MS};
use crate::http::server::{self as srv, Request, Response};

/// Hosts a page holds at the most (Datadog's).
pub const COUNT_MAX: usize = 1000;
/// Hosts a page holds when it does not say.
pub const COUNT: usize = 100;

fn infra() -> Result<&'static std::sync::Arc<Infra>, Fail> {
    super::get().ok_or_else(|| fail(StatusCode::SERVICE_UNAVAILABLE, "the infrastructure list is kept by the server"))
}

/// A host's id: a stable number of its name (FNV-1a, to 53 bits so JSON's numbers hold it).
fn id_of(name: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h & ((1 << 53) - 1)
}

/// Whether one term of a filter matches the host: `key:value` (`*` for any part) one of its tags,
/// or `host:<name>`; else a part of its name, an alias, an app or a source, whatever the case.
fn term_matches(h: &Host, term: &str, tags: &[String]) -> bool {
    if let Some((k, v)) = term.split_once(':')
        && !k.is_empty()
    {
        let glob = |t: &str| kiln_obs::index::glob(&format!("{k}:{v}"), t);
        return glob(&format!("host:{}", h.name)) || tags.iter().any(|t| glob(t));
    }
    let t = term.to_lowercase();
    let has = |s: &str| s.to_lowercase().contains(&t);
    has(&h.name) || h.aliases.iter().any(|a| has(a)) || h.apps.iter().any(|a| has(a)) || h.sources.iter().any(|s| has(s))
}

/// Whether the host matches every term of `filter`.
fn matches(h: &Host, filter: &str) -> bool {
    let terms: Vec<&str> = filter.split_whitespace().collect();
    if terms.is_empty() {
        return true;
    }
    let tags: Vec<String> = h.tags_by_source().into_values().flatten().chain(h.tags.iter().map(|(k, v)| format!("{k}:{v}"))).collect();
    terms.iter().all(|t| term_matches(h, t, &tags))
}

/// A figure to sort by, missing ones last whichever way.
fn sort_key(h: &Host, field: &str, now_ms: i64) -> Option<f64> {
    let f = &h.figures;
    #[allow(clippy::cast_precision_loss)]
    match field {
        "cpu" => f.cpu,
        "iowait" => f.iowait,
        "load" => f.load,
        "memory" => f.memory,
        "disk" => f.disk,
        "apps" => Some(h.apps.len() as f64),
        "last_reported" => h.last_reported_ms.map(|t| t as f64),
        "status" => Some(match h.status(now_ms) {
            "reporting" => 2.0,
            "late" => 1.0,
            _ => 0.0,
        }),
        _ => None,
    }
}

fn seconds(ms: i64) -> i64 {
    ms.div_euclid(1000)
}

fn round(v: Option<f64>) -> Value {
    v.map_or(Value::Null, |v| json!((v * 100.0).round() / 100.0))
}

/// A host as Datadog's host list gives one, kiln's own beside under `kiln`.
pub fn host_json(h: &Host, now_ms: i64, with_meta: bool) -> Value {
    let mut o = json!({
        "name": h.name,
        "host_name": h.name,
        "id": id_of(&h.name),
        "aliases": h.aliases,
        "apps": h.apps,
        "sources": h.sources,
        "up": h.up(now_ms),
        "is_muted": h.mute.is_some(),
        "mute_timeout": h.mute.as_ref().and_then(|m| m.end_ms).map(seconds),
        "last_reported_time": h.last_reported_ms.map(seconds),
        "metrics": { "cpu": round(h.figures.cpu), "iowait": round(h.figures.iowait), "load": round(h.figures.load) },
        "tags_by_source": h.tags_by_source(),
    });
    let p = h.platform();
    let f = &h.figures;
    let kiln = json!({
        "status": h.status(now_ms),
        "seen": (h.seen_ms > i64::MIN).then_some(h.seen_ms),
        "series": h.series,
        "figures": { "cpu": round(f.cpu), "iowait": round(f.iowait), "load": round(f.load), "load_norm": round(f.load_norm), "memory": round(f.memory), "disk": round(f.disk) },
        "platform": {
            "os": p.os, "kernel": p.kernel, "arch": p.arch, "cpu_model": p.cpu_model, "cores": p.cores, "threads": p.threads,
            "memory_bytes": p.memory_bytes, "cloud": p.cloud, "instance_type": p.instance_type, "region": p.region,
            "agent_version": p.agent_version, "kiln_version": p.kiln_version,
        },
        "runner": h.runner.as_ref().map(|r| json!({ "name": r.name, "connected": r.connected, "last_seen": r.last_seen_ms, "version": r.version, "labels": r.labels })),
        "mute": h.mute.as_ref().map(|m| json!({ "downtime": m.downtime, "end": m.end_ms, "message": m.message })),
    });
    if let Some(map) = o.as_object_mut() {
        if with_meta && let Some(a) = &h.agent {
            map.insert("meta".into(), Value::Object(a.meta.clone()));
        }
        map.insert("kiln".into(), kiln);
    }
    o
}

fn param<'a>(q: &'a HashMap<String, String>, k: &str) -> Option<&'a str> {
    q.get(k).map(String::as_str).map(str::trim).filter(|v| !v.is_empty())
}

fn flag(q: &HashMap<String, String>, k: &str) -> bool {
    param(q, k).is_some_and(|v| v == "true" || v == "1")
}

/// Datadog's `from`: seconds since the epoch; a host listed when it reported since then (three
/// hours ago, when none).
fn from_ms(q: &HashMap<String, String>, now_ms: i64) -> Result<i64, Fail> {
    match param(q, "from") {
        None => Ok(now_ms - LISTED_MS),
        Some(v) => v.parse::<i64>().map(|s| s.saturating_mul(1000)).map_err(|_| fail(StatusCode::BAD_REQUEST, "from: seconds since the epoch")),
    }
}

/// The hosts `q` asks for, sorted and paged, and how many matched.
pub fn listed<'a>(inv: &'a Inventory, q: &HashMap<String, String>) -> Result<(Vec<&'a Host>, usize), Fail> {
    let now = inv.at_ms;
    let from = from_ms(q, now)?;
    let filter = param(q, "filter").unwrap_or("");
    let field = param(q, "sort_field").unwrap_or("name");
    if !matches!(field, "name" | "status" | "cpu" | "iowait" | "load" | "memory" | "disk" | "apps" | "last_reported") {
        return Err(fail(StatusCode::BAD_REQUEST, "sort_field: name, status, cpu, iowait, load, memory, disk, apps or last_reported"));
    }
    let desc = match param(q, "sort_dir") {
        None | Some("asc") => false,
        Some("desc") => true,
        Some(_) => return Err(fail(StatusCode::BAD_REQUEST, "sort_dir: asc or desc")),
    };
    let start: usize = param(q, "start").map(str::parse).transpose().map_err(|_| fail(StatusCode::BAD_REQUEST, "start: a number"))?.unwrap_or(0);
    let count: usize = param(q, "count").map(str::parse).transpose().map_err(|_| fail(StatusCode::BAD_REQUEST, "count: a number"))?.unwrap_or(COUNT).min(COUNT_MAX);
    let mut hosts: Vec<&Host> = inv.hosts.iter().filter(|h| h.seen_ms >= from && matches(h, filter)).collect();
    if field != "name" {
        hosts.sort_by(|a, b| {
            let (x, y) = (sort_key(a, field, now), sort_key(b, field, now));
            let order = match (x, y) {
                (Some(x), Some(y)) => {
                    if desc {
                        y.total_cmp(&x)
                    } else {
                        x.total_cmp(&y)
                    }
                }
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            };
            order.then_with(|| a.name.cmp(&b.name))
        });
    } else if desc {
        hosts.reverse();
    }
    let matched = hosts.len();
    Ok((hosts.into_iter().skip(start).take(count).collect(), matched))
}

/// `GET hosts`: Datadog's host list.
pub async fn hosts(q: &HashMap<String, String>) -> Result<Response, Fail> {
    let inv = infra()?.inventory().await?;
    let (page, matched) = listed(&inv, q)?;
    let meta = flag(q, "include_hosts_metadata");
    let list: Vec<Value> = page.iter().map(|h| host_json(h, inv.at_ms, meta)).collect();
    Ok(srv::json(StatusCode::OK, &json!({ "host_list": list, "total_matching": matched, "total_returned": list.len() })))
}

/// `GET hosts/totals`: Datadog's, the hosts up (a sample within two hours) and active (one).
pub async fn totals(q: &HashMap<String, String>) -> Result<Response, Fail> {
    let inv = infra()?.inventory().await?;
    let from = match param(q, "from") {
        None => i64::MIN,
        Some(_) => from_ms(q, inv.at_ms)?,
    };
    let since = |h: &&Host| h.last_reported_ms.is_some_and(|t| t >= from);
    let up = inv.hosts.iter().filter(since).filter(|h| h.up(inv.at_ms)).count();
    let active = inv.hosts.iter().filter(since).filter(|h| h.active(inv.at_ms)).count();
    Ok(srv::json(StatusCode::OK, &json!({ "total_up": up, "total_active": active })))
}

/// `GET hosts/{name}`: a host whole, what the Agent said of it (its inventory, its checks) and its
/// containers.
pub async fn host(name: &str) -> Result<Response, Fail> {
    let inv = infra()?.inventory().await?;
    let h = inv.host(name).ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such host"))?;
    let mut o = host_json(h, inv.at_ms, true);
    let containers = super::containers(name).await.unwrap_or_default();
    if let Some(k) = o.get_mut("kiln").and_then(Value::as_object_mut) {
        if let Some(a) = &h.agent {
            k.insert("inventory".into(), Value::Object(a.inventory.clone()));
            k.insert("agent_updated".into(), json!(a.updated_ms));
        }
        if let Some(f) = h.runner.as_ref().and_then(|r| r.facts.as_ref()) {
            k.insert("gpus".into(), json!(f.gpus.iter().map(|g| json!({ "vendor": g.vendor, "model": g.model, "memory": g.memory })).collect::<Vec<_>>()));
        }
    }
    if let Some(map) = o.as_object_mut() {
        map.insert("containers".into(), Value::Array(containers));
    }
    Ok(srv::json(StatusCode::OK, &o))
}

/// A mute as Datadog's API takes one: `end` in seconds (none: until unmuted), a message, whether
/// it replaces a mute the host has.
#[derive(Debug, Default, Deserialize)]
pub struct MuteBody {
    #[serde(default)]
    pub end: Option<i64>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub r#override: bool,
}

/// Whether a host's name reads whole in a downtime's scope (`host:<name>`).
fn scoped(name: &str) -> bool {
    !name.is_empty() && name.len() <= 255 && !name.contains(|c: char| c.is_whitespace() || "()\"*,:".contains(c))
}

/// `POST hosts/{name}/mute`: a downtime of every monitor's groups of the host, from now to `end`.
pub async fn mute(name: &str, req: Request) -> Result<Response, Fail> {
    let body: MuteBody = super::super::api::read_json(req.into_body(), 64 << 10).await?;
    let i = infra()?;
    let inv = i.inventory().await?;
    let h = inv.host(name).ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such host"))?;
    if !scoped(name) {
        return Err(fail(StatusCode::BAD_REQUEST, "this host's name does not read whole in a downtime's scope"));
    }
    let now = super::super::clock_ms();
    let end_ms = body.end.map(|s| s.saturating_mul(1000));
    if end_ms.is_some_and(|e| e <= now) {
        return Err(fail(StatusCode::BAD_REQUEST, "end: a time to come (seconds since the epoch)"));
    }
    let message = body.message.unwrap_or_default();
    let monitors = super::super::api::monitors_kept()?;
    let id = match &h.mute {
        Some(_) if !body.r#override => return Err(fail(StatusCode::BAD_REQUEST, "the host is muted already: override to change its mute")),
        Some(m) => m.downtime.clone(),
        None => String::new(),
    };
    let d: super::super::downtimes::Downtime = serde_json::from_value(json!({
        "id": id,
        "message": message,
        "monitor_identifier": { "monitor_tags": ["*"] },
        "scope": format!("host:{name}"),
        "schedule": { "end": end_ms },
    }))
    .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    monitors.downtimes.put(d).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    i.forget();
    Ok(srv::json(StatusCode::OK, &json!({ "action": "Muted", "hostname": name, "end": body.end, "message": message })))
}

/// `POST hosts/{name}/unmute`: its mute's downtime canceled.
pub async fn unmute(name: &str) -> Result<Response, Fail> {
    let i = infra()?;
    let inv = i.inventory().await?;
    let h = inv.host(name).ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such host"))?;
    let m = h.mute.as_ref().ok_or_else(|| fail(StatusCode::BAD_REQUEST, "the host is not muted"))?;
    super::super::api::monitors_kept()?.downtimes.cancel(&m.downtime).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    i.forget();
    Ok(srv::json(StatusCode::OK, &json!({ "action": "Unmuted", "hostname": name })))
}

/// Datadog's host API at its paths (`/api/v1/hosts…`, `/api/v1/host/…`), the ingest token found
/// good: a writer's token reads and mutes nothing.
pub fn datadog() -> Response {
    srv::json(StatusCode::FORBIDDEN, &json!({ "errors": ["hosts are read and muted through kiln's API (/api/observe/v1/hosts), with a session"] }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::super::{Figures, Mute};
    use super::*;

    fn host(name: &str, cpu: Option<f64>, last: Option<i64>) -> Host {
        Host {
            name: name.into(),
            aliases: vec![format!("{name}.example.com")],
            apps: vec!["agent".into(), "system".into()],
            sources: vec!["agent".into()],
            last_reported_ms: last,
            seen_ms: last.unwrap_or(0),
            series: 3,
            figures: Figures { cpu, ..Figures::default() },
            tags: vec![("cloud.region".into(), "eu-west-1".into())],
            agent: None,
            runner: None,
            mute: None,
        }
    }

    fn q(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    /// Datadog's list: hosts that reported within three hours unless `from` says, a filter's terms
    /// each a part of the name (an alias, an app) or a tag, sorted by a figure (those without it
    /// last), paged; and its errors said.
    #[test]
    fn hosts_are_listed_as_datadog_s_api_lists_them() {
        let now = 100 * 3_600_000;
        let inv = Inventory {
            at_ms: now,
            hosts: vec![
                host("api-1", Some(40.0), Some(now - 60_000)),
                host("api-2", None, Some(now - 120_000)),
                host("db-1", Some(90.0), Some(now - 30_000)),
                host("old-1", Some(5.0), Some(now - 5 * 3_600_000)),
            ],
        };
        let names = |q: &HashMap<String, String>| listed(&inv, q).unwrap().0.iter().map(|h| h.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&q(&[])), ["api-1", "api-2", "db-1"], "old-1 reported over three hours ago");
        let from = ((now - 6 * 3_600_000) / 1000).to_string();
        assert_eq!(names(&q(&[("from", from.as_str())])).len(), 4);
        assert_eq!(names(&q(&[("filter", "API")])), ["api-1", "api-2"], "a part of its name, whatever the case");
        assert_eq!(names(&q(&[("filter", "example.com db")])), ["db-1"], "every term");
        assert_eq!(names(&q(&[("filter", "cloud.region:eu-*")])).len(), 3, "a tag, `*` for any part");
        assert_eq!(names(&q(&[("filter", "host:db-1")])), ["db-1"]);
        assert_eq!(names(&q(&[("sort_field", "cpu"), ("sort_dir", "desc")])), ["db-1", "api-1", "api-2"], "the host without its figure last");
        assert_eq!(names(&q(&[("sort_field", "cpu")])), ["api-1", "db-1", "api-2"]);
        assert_eq!(names(&q(&[("sort_dir", "desc")])), ["db-1", "api-2", "api-1"]);
        let (page, matched) = listed(&inv, &q(&[("start", "1"), ("count", "1")])).unwrap();
        assert_eq!((page.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), matched), (vec!["api-2"], 3));
        for bad in [("sort_field", "nope"), ("sort_dir", "up"), ("from", "yesterday"), ("count", "x")] {
            assert_eq!(listed(&inv, &q(&[bad])).err().map(|e| e.0), Some(StatusCode::BAD_REQUEST), "{bad:?}");
        }
    }

    /// A host as Datadog's API gives one: times in seconds, its metrics, its mute; its meta asked
    /// for; its id its name's, the same each time.
    #[test]
    fn a_host_reads_as_datadog_s() {
        let now = 1_792_324_800_000;
        let mut h = host("web-1", Some(12.345), Some(now - 15_000));
        h.mute = Some(Mute { downtime: "dt1".into(), end_ms: Some(now + 3_600_000), message: "patching".into() });
        let j = host_json(&h, now, false);
        assert_eq!((j["last_reported_time"].as_i64(), j["up"].as_bool(), j["is_muted"].as_bool()), (Some(1_792_324_785), Some(true), Some(true)));
        assert_eq!((j["mute_timeout"].as_i64(), j["metrics"]["cpu"].as_f64()), (Some(1_792_328_400), Some(12.35)));
        assert_eq!((j["kiln"]["status"].as_str(), j.get("meta")), (Some("reporting"), None));
        assert_eq!(j["id"], host_json(&h, now, false)["id"]);
        assert_ne!(j["id"], host_json(&host("web-2", None, None), now, false)["id"]);
        assert!(j["id"].as_u64().is_some_and(|i| i < 1 << 53));
        assert!(!scoped("a host") && !scoped("x(y)") && scoped("web-1.example.com"));
    }
}

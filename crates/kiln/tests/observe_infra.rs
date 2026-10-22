//! The infrastructure list, live (docs/observe.md §10.9, §13): a kiln server and a runner as
//! processes of their own, the runner looking at its host every 2 s; Datadog's Agent 7.84's own
//! payloads of its host, as it sent them (its host metadata, its checks, its inventory), and its
//! series. Each host listed with what reports it, its figures and its platform, as Datadog's host
//! list gives them; found by a part of its name and by a tag; one muted, a monitor's group of it
//! held while another host's alerts, then unmuted; Datadog's host API refused to the ingest token;
//! what the Agent said kept through the server's kill.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::time::{Duration, Instant};

use common::{Fleet, now_ms, raw};
use serde_json::{Value, json};

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/src/observe/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

/// A request to Datadog's paths with the key `key`.
fn datadog(f: &Fleet, key: &str, method: &str, path: &str, body: &[u8]) -> (u16, String) {
    let (code, b) = raw(f.http, method, path, &format!("DD-API-KEY: {key}\r\nContent-Type: application/json\r\n"), body).unwrap();
    (code, String::from_utf8_lossy(&b).into_owned())
}

/// The Agent's series of its host now: CPU idle 70% (30% in use), I/O wait 2.5%, a load of 0.75,
/// 60% of memory usable, its fullest disk 55% used, NTP's offset (an app of its own).
fn agent_series(host: &str) -> Vec<u8> {
    let t = now_ms() as f64 / 1000.0;
    let s = |metric: &str, v: f64| json!({ "metric": metric, "points": [[t - 20.0, v], [t - 5.0, v]], "type": "gauge", "host": host, "tags": ["env:capture"] });
    serde_json::to_vec(&json!({ "series": [
        s("system.cpu.idle", 70.0), s("system.cpu.iowait", 2.5), s("system.load.15", 0.75), s("system.load.norm.15", 0.05),
        s("system.mem.pct_usable", 0.6), s("system.disk.in_use", 0.55), s("datadog.agent.running", 1.0), s("ntp.offset", 0.001),
    ] }))
    .unwrap()
}

/// Waits up to `within` for `done` of the host list (`query` its parameters).
fn listed(f: &Fleet, query: &str, within: Duration, what: &str, done: impl Fn(&[Value]) -> bool) -> anyhow::Result<Vec<Value>> {
    let start = Instant::now();
    loop {
        let all = f.call("GET", &format!("/api/observe/v1/hosts?{query}"), None)?["host_list"].as_array().cloned().unwrap_or_default();
        if done(&all) {
            return Ok(all);
        }
        anyhow::ensure!(start.elapsed() < within, "never: {what}\n{all:#?}\n{}", f.logs());
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn one<'a>(all: &'a [Value], name: &str) -> Option<&'a Value> {
    all.iter().find(|h| h["name"] == name)
}

fn names(all: &[Value]) -> Vec<String> {
    all.iter().filter_map(|h| h["name"].as_str().map(str::to_owned)).collect()
}

#[test]
#[ignore = "starts real processes"]
fn hosts_are_listed_with_what_reports_them_and_muted() -> anyhow::Result<()> {
    let mut f = Fleet::with_env(|_| String::new(), &[("KILN_OBSERVE_HOSTS_SAVE_MS", "500")])?;
    f.start_runner_with("r1", &[("KILN_OBSERVE_HOST_SECONDS", "2")])?;
    let ingest = std::fs::read_to_string(f.root.path().join("server/observe/ingest.token"))?.trim().to_owned();

    // The Agent's word of probe-host, as it said it, and its series.
    for (path, file) in [("/intake/", "dd-agent-host-7.84.json"), ("/intake/", "dd-agent-checks-7.84.json"), ("/api/v1/metadata", "dd-agent-inventory-host-7.84.json")] {
        let (code, b) = datadog(&f, &ingest, "POST", path, &fixture(file));
        anyhow::ensure!(code == 202 || code == 200, "{path} {file}: {code} {b}");
    }
    let posted = now_ms() / 1000;
    let (code, b) = datadog(&f, &ingest, "POST", "/api/v1/series", &agent_series("probe-host"));
    anyhow::ensure!(code == 202, "the Agent's series: {code} {b}");

    // Both listed: the runner from kiln's own figures (its CPU once it has looked twice), the
    // Agent's host from its series and its word.
    // (A runner may come by the HTTP tunnel, 20 s on, where UDP to the server is not answered.)
    let all = listed(&f, "include_hosts_metadata=true", Duration::from_secs(120), "r1 and probe-host with their figures", |all| {
        one(all, "r1").is_some_and(|h| h["metrics"]["cpu"].is_number() && h["kiln"]["figures"]["memory"].is_number()) && one(all, "probe-host").is_some_and(|h| h["metrics"]["cpu"].is_number())
    })?;
    let r1 = one(&all, "r1").unwrap();
    anyhow::ensure!(r1["up"] == true && r1["kiln"]["status"] == "reporting" && r1["sources"].as_array().is_some_and(|s| s.contains(&json!("kiln"))), "{r1:#}");
    anyhow::ensure!(r1["apps"].as_array().is_some_and(|a| a.contains(&json!("system"))) && r1["kiln"]["runner"]["name"] == "r1" && r1["kiln"]["platform"]["os"].is_string(), "{r1:#}");
    let p = one(&all, "probe-host").unwrap();
    anyhow::ensure!(p["sources"] == json!(["agent"]) && p["apps"] == json!(["agent", "ntp", "system"]), "{p:#}");
    anyhow::ensure!(p["metrics"] == json!({ "cpu": 30.0, "iowait": 2.5, "load": 0.75 }) && p["kiln"]["figures"]["memory"] == 40.0 && p["kiln"]["figures"]["disk"] == 55.0, "{p:#}");
    anyhow::ensure!(p["meta"]["agent_version"] == "7.84.4" && p["meta"]["agent_checks"].as_array().map(Vec::len) == Some(13) && p["meta"]["platform"] == "linux", "{p:#}");
    anyhow::ensure!(p["tags_by_source"]["Datadog"].as_array().is_some_and(|t| t.contains(&json!("team:observe"))), "{p:#}");
    anyhow::ensure!(p["kiln"]["platform"]["cores"] == 16 && p["kiln"]["platform"]["os"] == "ubuntu 24.04", "its inventory says its platform: {p:#}");
    let last = p["last_reported_time"].as_i64().unwrap_or(0);
    anyhow::ensure!((posted - 6..=posted).contains(&last), "its newest sample, in seconds (posted {posted}): {last}");
    // Without its metadata asked, none.
    let plain = listed(&f, "", Duration::from_secs(5), "listed", |all| one(all, "probe-host").is_some())?;
    anyhow::ensure!(one(&plain, "probe-host").unwrap().get("meta").is_none());

    // Totals, filters, the order, a page.
    let t = f.call("GET", "/api/observe/v1/hosts/totals", None)?;
    anyhow::ensure!(t["total_up"].as_u64().unwrap_or(0) >= 2 && t["total_active"].as_u64().unwrap_or(0) >= 2, "{t}");
    anyhow::ensure!(names(&listed(&f, "filter=team:observe", Duration::from_secs(5), "by its tag", |_| true)?) == ["probe-host"]);
    anyhow::ensure!(names(&listed(&f, "filter=PROBE", Duration::from_secs(5), "by a part of its name", |_| true)?) == ["probe-host"]);
    let by_cpu = f.call("GET", "/api/observe/v1/hosts?sort_field=cpu&sort_dir=desc&count=1", None)?;
    anyhow::ensure!(by_cpu["total_returned"] == 1 && by_cpu["total_matching"].as_u64().unwrap_or(0) >= 2, "{by_cpu}");
    anyhow::ensure!(f.request("GET", "/api/observe/v1/hosts?sort_field=nonesuch", None)?.0 == 400);

    // probe-host whole: what the Agent's inventory says, its checks.
    let whole = f.call("GET", "/api/observe/v1/hosts/probe-host", None)?;
    anyhow::ensure!(whole["kiln"]["inventory"]["kernel_release"] == "7.2.9-2-cachyos" && whole["containers"] == json!([]), "{whole:#}");
    anyhow::ensure!(f.request("GET", "/api/observe/v1/hosts/nonesuch", None)?.0 == 404);

    // A monitor of every host; probe-host muted for an hour: its group held, r1's alerting told.
    let m = f.call(
        "POST",
        "/api/observe/v1/monitors",
        Some(
            &json!({ "name": "e2e hot", "query": "last(last_1m):max:e2e.infra.hot{*} by {host} > 100", "message": "{{host.name}} hot", "options": { "every": 10, "thresholds": { "critical": 100 } } }),
        ),
    )?;
    let id = m["id"].as_str().unwrap().to_owned();
    let end = now_ms() / 1000 + 3600;
    let muted = f.call("POST", "/api/observe/v1/hosts/probe-host/mute", Some(&json!({ "end": end, "message": "patching" })))?;
    anyhow::ensure!(muted == json!({ "action": "Muted", "hostname": "probe-host", "end": end, "message": "patching" }), "{muted}");
    let (code, b) = f.request("POST", "/api/observe/v1/hosts/probe-host/mute", Some(&json!({ "end": end })))?;
    anyhow::ensure!(code == 400 && String::from_utf8_lossy(&b).contains("override"), "muted already: {code}");
    f.call("POST", "/api/observe/v1/hosts/probe-host/mute", Some(&json!({ "end": end + 60, "message": "patching", "override": true })))?;
    let dts = f.call("GET", "/api/observe/v1/downtimes", None)?;
    let dt = dts["downtimes"].as_array().into_iter().flatten().find(|d| d["scope"] == "host:probe-host").cloned().unwrap_or_default();
    anyhow::ensure!(dt["monitor_identifier"]["monitor_tags"] == json!(["*"]) && dt["status"] == "active" && dt["message"] == "patching", "one downtime, its end changed: {dts:#}");
    let dt_id = dt["id"].as_str().unwrap().to_owned();
    let all = listed(&f, "", Duration::from_secs(30), "probe-host muted", |all| one(all, "probe-host").is_some_and(|h| h["is_muted"] == true))?;
    anyhow::ensure!(one(&all, "probe-host").unwrap()["mute_timeout"] == end + 60, "{all:#?}");
    f.write("e2e.infra.hot", &["probe-host", "r1"], now_ms() - 50_000, 6, |_, _| 150.0)?;
    let start = Instant::now();
    loop {
        let events = f.call(
            "POST",
            "/api/observe/v1/logs/search",
            Some(&json!({ "query": format!("source:kiln.event @monitor_id:{id} @event.type:monitor.alert"), "from": now_ms() - 600_000, "to": now_ms() + 60_000, "limit": 10 })),
        )?;
        let records = events["records"].as_array().cloned().unwrap_or_default();
        let of = |host: &str| records.iter().find(|e| e["attributes"]["group.host"] == host).cloned();
        if let (Some(held), Some(told)) = (of("probe-host"), of("r1")) {
            anyhow::ensure!(held["attributes"]["downtime_id"] == dt_id.as_str() && held["attributes"]["muted"] == "true", "probe-host's alert held: {held:#}");
            anyhow::ensure!(told["attributes"]["downtime_id"].is_null() && told["attributes"]["muted"] == "false", "r1's told: {told:#}");
            break;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(60), "probe-host held, r1 told: {records:#?}\n{}", f.logs());
        std::thread::sleep(Duration::from_millis(500));
    }
    // Unmuted: its downtime canceled.
    anyhow::ensure!(f.call("POST", "/api/observe/v1/hosts/probe-host/unmute", None)? == json!({ "action": "Unmuted", "hostname": "probe-host" }));
    anyhow::ensure!(f.call("GET", &format!("/api/observe/v1/downtimes/{dt_id}"), None)?["status"] == "canceled");
    listed(&f, "", Duration::from_secs(30), "probe-host unmuted", |all| one(all, "probe-host").is_some_and(|h| h["is_muted"] == false))?;
    anyhow::ensure!(f.request("POST", "/api/observe/v1/hosts/probe-host/unmute", None)?.0 == 400, "not muted");

    // Datadog's host API: the ingest token reads and mutes nothing.
    anyhow::ensure!(datadog(&f, &ingest, "GET", "/api/v1/hosts", b"").0 == 403);
    anyhow::ensure!(datadog(&f, &ingest, "POST", "/api/v1/host/probe-host/mute", b"{}").0 == 403);

    // Killed and started again: what the Agent said is kept (saved within half a second).
    std::thread::sleep(Duration::from_secs(2));
    f.kill_server();
    f.start_server()?;
    let all = listed(&f, "include_hosts_metadata=true", Duration::from_secs(60), "probe-host's word kept", |all| one(all, "probe-host").is_some_and(|h| h["meta"]["agent_version"] == "7.84.4"))?;
    anyhow::ensure!(one(&all, "probe-host").unwrap()["kiln"]["platform"]["cores"] == 16);
    Ok(())
}

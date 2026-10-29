//! Notebooks and postmortems live (docs/observe.md §11.1, §9.5, §13): a kiln server as a process of
//! its own. A notebook made with Datadog's body (Markdown, a timeseries, a log stream), read with
//! how kiln draws each graph, changed whole (a cell kept by its id, one dropped, one added), listed
//! as Datadog's list asks (templates apart, by type, by a part of the name, a page, without cells);
//! what Datadog's API refuses refused. An incident's postmortem written of kiln's template (every
//! variable filled: its facts, impact, timeline, action items, links; on the incident's time) and
//! attached as its postmortem, a second refused; another's of a template of one's own. A viewer
//! reads and is refused writing; Datadog's notebooks API refused to the ingest token; all of it kept
//! through the server's kill.
//!
//! `cargo test -p kiln --test observe_notebooks -- --ignored --nocapture`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::time::Duration;

use common::{Fleet, at, now_ms, raw};
use serde_json::{Value, json};

const NB: &str = "/api/observe/v1/notebooks";
const INC: &str = "/api/observe/v1/incidents";

fn ask(f: &Fleet, method: &str, path: &str, body: Option<&Value>) -> anyhow::Result<(u16, Value)> {
    let (code, b) = f.request(method, path, body)?;
    Ok((code, if b.is_empty() { Value::Null } else { serde_json::from_slice(&b).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&b).into_owned())) }))
}

fn notebook(name: &str, cells: Vec<Value>, metadata: Value) -> Value {
    json!({ "data": { "type": "notebooks", "attributes": { "name": name, "cells": cells, "time": { "live_span": "1h" }, "status": "published", "metadata": metadata } } })
}

fn cell(attributes: Value) -> Value {
    json!({ "type": "notebook_cells", "attributes": attributes })
}

fn markdown(text: &str) -> Value {
    cell(json!({ "definition": { "type": "markdown", "text": text } }))
}

/// The texts of a notebook's Markdown cells, in order.
fn texts(n: &Value) -> Vec<String> {
    at(n, &["attributes", "cells"]).as_array().into_iter().flatten().filter_map(|c| at(c, &["attributes", "definition", "text"]).as_str().map(str::to_owned)).collect()
}

fn ids(list: &Value) -> Vec<u64> {
    at(list, &["data"]).as_array().into_iter().flatten().filter_map(|n| at(n, &["id"]).as_u64()).collect()
}

#[test]
#[ignore = "starts a kiln server as a process"]
fn notebooks_are_written_listed_and_a_postmortem_is_made_of_an_incident() -> anyhow::Result<()> {
    let mut f = Fleet::new()?;
    f.until(Duration::from_secs(30), "the server leads", |s| at(s, &["leader"]).as_u64() == Some(1))?;

    // Made with Datadog's body: Markdown, a timeseries, a log stream.
    let body = notebook(
        "Checkout latency",
        vec![
            markdown("# Why checkout slowed"),
            cell(json!({ "definition": { "type": "timeseries", "requests": [{ "q": "avg:e2e.latency{*} by {host}", "display_type": "line" }] }, "graph_size": "m" })),
            cell(json!({ "definition": { "type": "log_stream", "query": "service:checkout status:error", "columns": [] }, "graph_size": "s", "time": { "live_span": "4h" } })),
        ],
        json!({ "is_template": false, "take_snapshots": false, "type": "investigation" }),
    );
    let (code, made) = ask(&f, "POST", NB, Some(&body))?;
    anyhow::ensure!(code == 200, "made: {code} {made}");
    let n = at(&made, &["data"]).clone();
    anyhow::ensure!(
        at(&n, &["id"]) == 1 && at(&n, &["type"]) == "notebooks" && at(&n, &["attributes", "metadata", "type"]) == "investigation" && at(&n, &["attributes", "author", "handle"]) == "admin",
        "as made: {n}"
    );
    let cells = at(&n, &["attributes", "cells"]).as_array().cloned().unwrap_or_default();
    anyhow::ensure!(cells.iter().map(|c| at(c, &["id"]).as_str().unwrap_or("")).collect::<Vec<_>>() == ["c1", "c2", "c3"], "its cells' ids: {n}");
    anyhow::ensure!(at(&cells[0], &["kiln"]).is_null(), "Markdown drawn as it is");
    anyhow::ensure!(at(&cells[1], &["kiln", "widget", "kind"]) == "timeseries", "the graph as kiln draws it: {}", cells[1]);
    anyhow::ensure!(at(&cells[1], &["kiln", "widget"]).to_string().contains("avg:e2e.latency{*} by {host}"), "its query: {}", cells[1]);
    anyhow::ensure!(at(&cells[2], &["kiln", "widget", "kind"]) == "logs" && at(&cells[2], &["attributes", "time", "live_span"]) == "4h", "the log stream on its own time: {}", cells[2]);
    anyhow::ensure!(f.call("GET", &format!("{NB}/1"), None)?["data"] == n, "read back as made");

    // Changed whole: its first cell kept (its text changed), the graph dropped, a cell added.
    let changed = json!({ "data": { "type": "notebooks", "attributes": {
        "name": "Checkout latency, found", "time": { "live_span": "1d" },
        "cells": [{ "id": "c1", "type": "notebook_cells", "attributes": { "definition": { "type": "markdown", "text": "# Why: the pool" } } }, markdown("A note")],
    } } });
    let n = f.call("PUT", &format!("{NB}/1"), Some(&changed))?["data"].clone();
    let cell_ids: Vec<&str> = at(&n, &["attributes", "cells"]).as_array().into_iter().flatten().filter_map(|c| at(c, &["id"]).as_str()).collect();
    anyhow::ensure!(cell_ids == ["c1", "c4"] && texts(&n) == ["# Why: the pool", "A note"] && at(&n, &["attributes", "time", "live_span"]) == "1d", "changed: {n}");
    anyhow::ensure!(at(&n, &["attributes", "metadata", "type"]).is_null(), "metadata changed whole, as Datadog's PUT: {n}");

    // Listed as Datadog's list asks.
    let mk = |name: &str, metadata: Value| -> anyhow::Result<u64> { Ok(f.call("POST", NB, Some(&notebook(name, vec![markdown("x")], metadata)))?["data"]["id"].as_u64().unwrap_or(0)) };
    let runbook = mk("DB failover runbook", json!({ "type": "runbook" }))?;
    let template = mk("Our postmortem", json!({ "is_template": true, "type": "postmortem" }))?;
    let list = |q: &str| f.call("GET", &format!("{NB}?{q}"), None);
    anyhow::ensure!(ids(&list("")?) == [runbook, 1], "the newest changed first, templates apart");
    anyhow::ensure!(ids(&list("is_template=true")?) == [template] && ids(&list("type=runbook")?) == [runbook] && ids(&list("query=CHECKOUT")?) == [1], "templates, a type, a part of the name");
    anyhow::ensure!(ids(&list("sort_field=name&sort_dir=asc")?) == [1, runbook], "by name");
    let page = list("count=1&start=1&include_cells=false")?;
    anyhow::ensure!(ids(&page) == [1] && page["data"][0]["attributes"].get("cells").is_none() && page["data"][0]["attributes"].get("time").is_none(), "a page without cells: {page}");
    anyhow::ensure!(page["meta"]["page"] == json!({ "total_count": 3, "total_filtered_count": 2 }), "counted: {page}");

    // Refused, saying why.
    for (body, says) in [
        (notebook(&"x".repeat(81), vec![], json!({})), "name"),
        (notebook("x", vec![cell(json!({ "definition": { "type": "geomap" } }))], json!({})), "cell of type"),
        (notebook("x", vec![], json!({ "type": "diary" })), "metadata.type"),
        (json!({ "data": { "type": "notebooks", "attributes": { "name": "x", "cells": [], "time": { "live_span": "2h" } } } }), "live_span"),
    ] {
        let (code, why) = ask(&f, "POST", NB, Some(&body))?;
        anyhow::ensure!(code == 400 && why.to_string().contains(says), "{says}: {code} {why}");
    }
    anyhow::ensure!(f.request("GET", &format!("{NB}/one"), None)?.0 == 404 && f.request("GET", &format!("{NB}/99"), None)?.0 == 404, "no such notebook");

    // An incident's postmortem, of kiln's template.
    let t0 = now_ms();
    let declared = f.call(
        "POST",
        INC,
        Some(&json!({ "data": { "type": "incidents", "attributes": {
            "title": "Checkout requests failing",
            "customer_impacted": true, "customer_impact_scope": "Every checkout fails with a 502",
            "fields": { "severity": { "type": "dropdown", "value": "SEV-2" }, "services": { "type": "multiselect", "value": ["checkout"] }, "root_cause": { "type": "textbox", "value": "The gateway's pool ran dry" } },
            "initial_cells": [{ "cell_type": "markdown", "content": { "content": "Paged payments." } }],
        }, "relationships": { "commander_user": { "data": { "type": "users", "id": "ada" } } } } })),
    )?;
    let incident = declared["data"]["id"].as_str().unwrap_or("").to_owned();
    f.call("POST", &format!("{INC}/1/relationships/todos"), Some(&json!({ "data": { "attributes": { "content": "Alert on the pool's size", "assignees": ["bob"] } } })))?;
    f.call("PATCH", &format!("{INC}/1"), Some(&json!({ "data": { "attributes": { "state": "resolved" } } })))?;
    let (code, pm) = ask(&f, "POST", &format!("{INC}/1/postmortem"), None)?;
    anyhow::ensure!(code == 201, "written: {code} {pm}");
    let pm = pm["data"].clone();
    let pid = pm["id"].as_u64().unwrap_or(0);
    anyhow::ensure!(
        at(&pm, &["attributes", "name"]) == "Postmortem: #1 Checkout requests failing"
            && at(&pm, &["attributes", "metadata", "type"]) == "postmortem"
            && at(&pm, &["attributes", "kiln", "incident"]).as_str() == Some(incident.as_str()),
        "a postmortem of incident #1: {pm}"
    );
    let all = texts(&pm).join("\n\n");
    for want in [
        "# Postmortem: Checkout requests failing",
        "| Severity | SEV-2 |",
        "| Commander | ada |",
        "| Services | checkout |",
        "The gateway's pool ran dry",
        "Every checkout fails with a 502",
        "- [ ] Alert on the pool's size (bob)",
        "Paged payments.",
        "State active → resolved",
    ] {
        anyhow::ensure!(all.contains(want), "{want:?} in its postmortem:\n{all}");
    }
    anyhow::ensure!(!all.contains("{{incident."), "every variable filled:\n{all}");
    let start = pm["attributes"]["time"]["start"].as_str().unwrap_or("");
    anyhow::ensure!(start < pm["attributes"]["time"]["end"].as_str().unwrap_or("") && !start.is_empty(), "on the incident's time: {}", pm["attributes"]["time"]);
    let i = f.call("GET", &format!("{INC}/1"), None)?;
    let post = &i["data"]["attributes"]["kiln"]["postmortem"];
    anyhow::ensure!(post["url"].as_str().is_some_and(|u| u.ends_with(&format!("/observe/notebooks/{pid}"))) && post["title"] == pm["attributes"]["name"], "attached as its postmortem: {i}");
    let (code, why) = ask(&f, "POST", &format!("{INC}/1/postmortem"), None)?;
    anyhow::ensure!(code == 409 && why.to_string().contains("has a postmortem"), "one an incident: {code} {why}");
    anyhow::ensure!(ids(&list("type=postmortem")?) == [pid], "one postmortem made");

    // Another's, of a template of one's own: its Markdown filled, its graph on the notebook's time.
    let own = f.call(
        "PUT",
        &format!("{NB}/{template}"),
        Some(&json!({ "data": { "type": "notebooks", "attributes": { "name": "Our postmortem", "time": { "live_span": "1h" }, "metadata": { "is_template": true, "type": "postmortem" }, "cells": [
            markdown("## {{incident.title}}, led by {{incident.commander}} ({{incident.status}})"),
            cell(json!({ "definition": { "type": "timeseries", "requests": [{ "q": "sum:checkout.errors{*}" }] }, "time": { "live_span": "1h" } })),
        ] } } })),
    )?;
    anyhow::ensure!(at(&own, &["data", "attributes", "metadata", "is_template"]) == true, "a template: {own}");
    f.call("POST", INC, Some(&json!({ "data": { "attributes": { "title": "Search slow" } } })))?;
    let (code, why) = ask(&f, "POST", &format!("{INC}/2/postmortem"), Some(&json!({ "template_id": runbook })))?;
    anyhow::ensure!(code == 404 && why.to_string().contains("template"), "a notebook not a template: {code} {why}");
    let two = f.call("POST", &format!("{INC}/2/postmortem"), Some(&json!({ "template_id": template })))?["data"].clone();
    anyhow::ensure!(texts(&two) == ["## Search slow, led by  (Active)"], "its variables filled: {two}");
    anyhow::ensure!(
        two["attributes"]["cells"][1]["attributes"].get("time").is_none() && two["attributes"]["cells"][1]["kiln"]["widget"]["kind"] == "timeseries",
        "the graph on the notebook's time: {two}"
    );

    // A viewer reads and is refused writing; Datadog's paths refused to the ingest token.
    let t = f.call("POST", "/api/tokens", Some(&json!({ "grants": ["viewer"], "user": "owner", "note": "e2e viewer" })))?;
    let viewer = at(&t, &["token"]).as_str().unwrap_or("").to_owned();
    let as_viewer = |method: &str, path: &str, body: &Value| -> anyhow::Result<u16> {
        Ok(raw(f.http, method, path, &format!("Authorization: Bearer {viewer}\r\nContent-Type: application/json\r\n"), body.to_string().as_bytes())?.0)
    };
    anyhow::ensure!(as_viewer("GET", NB, &Value::Null)? == 200 && as_viewer("GET", &format!("{NB}/1"), &Value::Null)? == 200, "a viewer reads");
    anyhow::ensure!(
        as_viewer("POST", NB, &body)? == 403 && as_viewer("DELETE", &format!("{NB}/1"), &Value::Null)? == 403 && as_viewer("POST", &format!("{INC}/2/postmortem"), &json!({}))? == 403,
        "a viewer writes nothing"
    );
    let ingest = std::fs::read_to_string(f.root.path().join("server/observe/ingest.token"))?.trim().to_owned();
    for method in ["GET", "POST"] {
        let (code, _) = raw(f.http, method, "/api/v1/notebooks", &format!("DD-API-KEY: {ingest}\r\nContent-Type: application/json\r\n"), body.to_string().as_bytes())?;
        anyhow::ensure!(code == 403, "{method} Datadog's notebooks API with the ingest token: {code}");
    }

    // Kept through the server's kill; removed, gone.
    let before = f.call("GET", &format!("{NB}/{pid}"), None)?;
    f.kill_server();
    f.start_server()?;
    f.until(Duration::from_secs(60), "the server leads again", |s| at(s, &["leader"]).as_u64() == Some(1))?;
    anyhow::ensure!(f.call("GET", &format!("{NB}/{pid}"), None)? == before, "the postmortem kept");
    anyhow::ensure!(f.call("GET", &format!("{NB}?count=1000"), None)?["meta"]["page"]["total_count"] == 5, "all five kept");
    let next = f.call("POST", NB, Some(&notebook("After", vec![], json!({}))))?;
    anyhow::ensure!(next["data"]["id"] == 6, "ids go on: {next}");
    anyhow::ensure!(f.request("DELETE", &format!("{NB}/{runbook}"), None)?.0 == 204 && f.request("GET", &format!("{NB}/{runbook}"), None)?.0 == 404, "removed");
    anyhow::ensure!(now_ms() >= t0, "the clock went on");
    Ok(())
}

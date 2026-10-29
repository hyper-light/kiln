//! Incident management live (docs/observe.md §9.5, §13): a kiln server as a process of its own,
//! and services for it to tell (Slack, PagerDuty, Opsgenie, a webhook) on a port of their own. An
//! incident declared with Datadog's body, told to the integrations it names; found by Datadog's
//! search, what matched counted by facet; its todos and attachments; changed (its severity, stable,
//! resolved), each change on its timeline and told, PagerDuty's incident and Opsgenie's alert
//! closed by one key; Datadog's figures of it. A monitor whose message names `@incident-sev1`
//! declares one as its group alerts, puts its recovery on that one's timeline, and declares another
//! once that one is resolved. A viewer reads and is refused declaring; Datadog's incident API is
//! refused to the ingest token; all of it kept through the server's kill.
//!
//! `cargo test -p kiln --test observe_incidents -- --ignored --nocapture`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::time::{Duration, Instant};

use common::{Fleet, Mock, at, now_ms, raw};
use serde_json::{Value, json};

const BASE: &str = "/api/observe/v1/incidents";

/// Waits up to `within` for `done`.
fn wait(f: &Fleet, what: &str, within: Duration, mut done: impl FnMut() -> anyhow::Result<bool>) -> anyhow::Result<()> {
    let start = Instant::now();
    loop {
        if done()? {
            return Ok(());
        }
        anyhow::ensure!(start.elapsed() < within, "never: {what}\n{}", f.logs());
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn rfc3339(ms: i64) -> anyhow::Result<String> {
    Ok(time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)?.format(&time::format_description::well_known::Rfc3339)?)
}

fn ms_of(v: &Value) -> anyhow::Result<i64> {
    let s = v.as_str().ok_or_else(|| anyhow::anyhow!("not a time: {v}"))?;
    Ok(i64::try_from(time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)?.unix_timestamp_nanos() / 1_000_000)?)
}

/// A query's text as a URL's.
fn enc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { char::from(b).to_string() } else { format!("%{b:02X}") }).collect()
}

/// A request's status and its body as JSON (null when it has none).
fn ask(f: &Fleet, method: &str, path: &str, body: Option<&Value>) -> anyhow::Result<(u16, Value)> {
    let (code, b) = f.request(method, path, body)?;
    Ok((code, if b.is_empty() { Value::Null } else { serde_json::from_slice(&b).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&b).into_owned())) }))
}

/// The incidents `query` matches: their public ids, the total, the facets.
fn search(f: &Fleet, query: &str) -> anyhow::Result<(Vec<u64>, u64, Value)> {
    let r = f.call("GET", &format!("{BASE}/search?query={}&page[size]=100", enc(query)), None)?;
    let a = at(&r, &["data", "attributes"]);
    let ids = at(a, &["incidents"]).as_array().into_iter().flatten().filter_map(|i| at(i, &["data", "attributes", "public_id"]).as_u64()).collect();
    Ok((ids, at(a, &["total"]).as_u64().unwrap_or(0), at(a, &["facets"]).clone()))
}

/// Whether the mock heard, at `path`, a body `f` holds of.
fn heard(mock: &Mock, path: &str, f: impl Fn(&Value) -> bool) -> bool {
    mock.at(path).iter().any(|g| f(&g.json()))
}

fn text(v: &Value) -> &str {
    at(v, &["text"]).as_str().unwrap_or("")
}

/// The states of the monitor cells on incident `id`'s timeline, in order.
fn monitor_cells(f: &Fleet, id: u64) -> anyhow::Result<Vec<String>> {
    let t = f.call("GET", &format!("{BASE}/{id}/timeline"), None)?;
    Ok(at(&t, &["data"]).as_array().into_iter().flatten().filter(|c| at(c, &["cell_type"]) == "monitor").filter_map(|c| at(c, &["state"]).as_str().map(str::to_owned)).collect())
}

/// The incidents a monitor declared: their public ids, the newest first.
fn of_monitor(f: &Fleet, monitor: &str) -> anyhow::Result<Vec<u64>> {
    let all = f.call("GET", &format!("{BASE}?page[size]=100"), None)?;
    Ok(at(&all, &["data"])
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| at(i, &["attributes", "kiln", "monitor", "id"]) == monitor)
        .filter_map(|i| at(i, &["attributes", "public_id"]).as_u64())
        .collect())
}

#[test]
#[ignore = "starts a kiln server as a process, and services for it to tell"]
fn an_incident_is_declared_told_worked_resolved_and_kept() -> anyhow::Result<()> {
    let mut f = Fleet::new()?;
    f.until(Duration::from_secs(30), "the server leads", |s| at(s, &["leader"]).as_u64() == Some(1))?;
    let mock = Mock::start(&[])?;
    for (name, body) in [
        ("ops", json!({ "kind": "slack", "url": mock.url("/slack") })),
        ("oncall", json!({ "kind": "pagerduty", "routing_key": "R0UT1NGKEY", "url": mock.url("/pagerduty") })),
        ("ops-g", json!({ "kind": "opsgenie", "api_key": "G3N13K3Y", "url": mock.url("/opsgenie") })),
        ("hook", json!({ "kind": "webhook", "url": mock.url("/hook") })),
    ] {
        let (code, b) = ask(&f, "PUT", &format!("/api/observe/v1/integrations/{name}"), Some(&body))?;
        anyhow::ensure!(code == 204, "integration {name}: {code} {b}");
    }

    // Declared with Datadog's body: its severity a field, its services and teams, the integrations
    // it tells, a first note, its commander; the customers' impact starting five minutes ago.
    let t0 = now_ms();
    let declare = json!({ "data": { "type": "incidents", "attributes": {
        "title": "Checkout requests failing",
        "customer_impacted": true,
        "customer_impact_scope": "Every checkout fails with a 502",
        "customer_impact_start": rfc3339(t0 - 300_000)?,
        "fields": {
            "severity": { "type": "dropdown", "value": "SEV-2" },
            "services": { "type": "multiselect", "value": ["checkout", "payments"] },
            "teams": { "type": "multiselect", "value": ["payments"] },
        },
        "notification_handles": [
            { "display_name": "Ops", "handle": "@slack-ops" },
            { "display_name": "On call", "handle": "@pagerduty-oncall" },
            { "handle": "@opsgenie-ops-g" },
            { "handle": "@webhook-hook" },
        ],
        "initial_cells": [{ "cell_type": "markdown", "content": { "content": "Paged **payments**." }, "important": true }],
    }, "relationships": { "commander_user": { "data": { "type": "users", "id": "ada" } } } } });
    let (code, made) = ask(&f, "POST", BASE, Some(&declare))?;
    anyhow::ensure!(code == 201, "declared: {code} {made}");
    let one = at(&made, &["data"]).clone();
    let a = at(&one, &["attributes"]);
    anyhow::ensure!(
        at(a, &["public_id"]) == 1
            && at(a, &["severity"]) == "SEV-2"
            && at(a, &["state"]) == "active"
            && at(a, &["fields", "severity", "value"]) == "SEV-2"
            && at(a, &["fields", "services", "value"]) == &json!(["checkout", "payments"]),
        "as declared: {one}"
    );
    anyhow::ensure!(at(&one, &["relationships", "commander_user", "data", "id"]) == "ada" && at(&one, &["relationships", "created_by_user", "data", "id"]) == "admin", "who: {one}");
    let detected = ms_of(at(a, &["detected"]))?;
    let ttd = at(a, &["time_to_detect"]).as_i64().unwrap_or(-1);
    anyhow::ensure!((300..=315).contains(&ttd) && at(a, &["time_to_internal_response"]) == 0, "detected five minutes after the impact began, declared at once: {a}");

    // Told: Slack, PagerDuty (its severity by SEV-2), Opsgenie (P2), the webhook (its event).
    wait(&f, "every integration told of its declaring", Duration::from_secs(30), || Ok(["/slack", "/pagerduty", "/opsgenie/v2/alerts", "/hook"].iter().all(|p| !mock.at(p).is_empty())))?;
    let slack = mock.at("/slack").first().map(common::Got::json).unwrap_or_default();
    anyhow::ensure!(
        text(&slack) == "[Declared] Incident #1: Checkout requests failing" && slack.to_string().contains("Severity *SEV-2*") && slack.to_string().contains("Commander *ada*"),
        "slack: {slack}"
    );
    let pd = mock.at("/pagerduty").first().map(common::Got::json).unwrap_or_default();
    anyhow::ensure!(
        at(&pd, &["event_action"]) == "trigger"
            && at(&pd, &["payload", "severity"]) == "error"
            && at(&pd, &["payload", "class"]) == "kiln.incident"
            && at(&pd, &["payload", "custom_details", "facts", "Severity"]) == "SEV-2",
        "pagerduty: {pd}"
    );
    anyhow::ensure!(at(&pd, &["links"]).to_string().contains("/observe/incidents/1\""), "its link: {pd}");
    let dedup = at(&pd, &["dedup_key"]).as_str().unwrap_or("").to_owned();
    let og = mock.at("/opsgenie/v2/alerts").first().map(common::Got::json).unwrap_or_default();
    anyhow::ensure!(
        at(&og, &["alias"]).as_str() == Some(dedup.as_str()) && at(&og, &["priority"]) == "P2" && at(&og, &["message"]).as_str().is_some_and(|m| m.starts_with("[Declared] Incident #1")),
        "opsgenie: {og}"
    );
    let hook = mock.at("/hook").first().cloned().ok_or_else(|| anyhow::anyhow!("the webhook"))?;
    anyhow::ensure!(
        hook.header("x-kiln-event") == Some("incident.declared") && at(&hook.json(), &["notice", "headline"]) == "[Declared] Incident #1: Checkout requests failing",
        "the webhook: {}",
        hook.json()
    );

    // Found by Datadog's search; what matched counted by facet; a malformed query refused.
    anyhow::ensure!(search(&f, "state:active AND severity:(SEV-1 OR SEV-2)")?.0 == [1], "by state and severity");
    anyhow::ensure!(search(&f, "services:payments teams:pay*")?.0 == [1] && search(&f, "services:search")?.1 == 0 && search(&f, "checkout")?.0 == [1], "by its fields, a word of its title");
    let (_, _, facets) = search(&f, "")?;
    anyhow::ensure!(at(&facets, &["severity"]) == &json!([{ "name": "SEV-2", "count": 1 }]) && at(&facets, &["state"]) == &json!([{ "name": "active", "count": 1 }]), "facets: {facets}");
    let services = at(&facets, &["fields"]).as_array().into_iter().flatten().find(|x| at(x, &["name"]) == "services").map(|x| at(x, &["facets"]).clone()).unwrap_or_default();
    anyhow::ensure!(services == json!([{ "name": "checkout", "count": 1 }, { "name": "payments", "count": 1 }]), "its fields' facets: {facets}");
    let (code, _) = f.request("GET", &format!("{BASE}/search?query={}", enc("state:active OR severity:SEV-1")), None)?;
    anyhow::ensure!(code == 400, "facets joined by OR: {code}");

    // Its todos: two made, one done, one removed.
    let todo = |content: &str, due: Value| json!({ "data": { "type": "incident_todos", "attributes": { "content": content, "assignees": ["ada"], "due_date": due } } });
    let roll = f.call("POST", &format!("{BASE}/1/relationships/todos"), Some(&todo("Roll back checkout-api", Value::Null)))?;
    let pm = f.call("POST", &format!("{BASE}/1/relationships/todos"), Some(&todo("Write the postmortem", json!("2026-10-30"))))?;
    anyhow::ensure!(at(&pm, &["data", "attributes", "due_date"]) == "2026-10-30" && at(&pm, &["data", "attributes", "assignees"]) == &json!(["ada"]), "a todo: {pm}");
    let (roll, pm) = (at(&roll, &["data", "id"]).as_str().unwrap_or("").to_owned(), at(&pm, &["data", "id"]).as_str().unwrap_or("").to_owned());
    let done = f.call("PATCH", &format!("{BASE}/1/relationships/todos/{roll}"), Some(&json!({ "data": { "attributes": { "completed": rfc3339(now_ms())? } } })))?;
    anyhow::ensure!(at(&done, &["data", "attributes", "completed"]).is_string(), "done: {done}");
    let i = f.call("GET", &format!("{BASE}/1"), None)?;
    anyhow::ensure!(at(&i, &["data", "attributes", "kiln", "todos"]) == 2 && at(&i, &["data", "attributes", "kiln", "todos_open"]) == 1, "its todos counted: {i}");
    anyhow::ensure!(
        f.request("DELETE", &format!("{BASE}/1/relationships/todos/{pm}"), None)?.0 == 204 && f.request("DELETE", &format!("{BASE}/1/relationships/todos/{pm}"), None)?.0 == 404,
        "a todo removed once"
    );

    // Its postmortem (one), a link; what is not HTTP(S) refused.
    let attach =
        |kind: &str, url: &str, title: &str| json!({ "data": { "type": "incident_attachments", "attributes": { "attachment_type": kind, "attachment": { "documentUrl": url, "title": title } } } });
    anyhow::ensure!(f.request("POST", &format!("{BASE}/1/attachments"), Some(&attach("postmortem", "https://docs.example.com/pm/1", "Postmortem: checkout")))?.0 == 201, "its postmortem");
    let (code, why) = ask(&f, "POST", &format!("{BASE}/1/attachments"), Some(&attach("postmortem", "https://docs.example.com/pm/2", "Another")))?;
    anyhow::ensure!(code == 400 && why.to_string().contains("one postmortem"), "a second postmortem: {code} {why}");
    anyhow::ensure!(f.request("POST", &format!("{BASE}/1/attachments"), Some(&attach("link", "javascript:alert(1)", "x")))?.0 == 400, "a script's URL");
    anyhow::ensure!(f.request("POST", &format!("{BASE}/1/attachments"), Some(&attach("link", "https://grafana.example.com/d/checkout", "Checkout")))?.0 == 201, "a link");
    anyhow::ensure!(search(&f, "postmortem:true")?.0 == [1], "found by its postmortem");

    // Worked: a note; its severity raised (as Datadog's field), stable, then resolved with the
    // impact's end: each change on its timeline, each told.
    f.call("POST", &format!("{BASE}/1/timeline"), Some(&json!({ "content": "Rolled back checkout-api to v41.", "important": false })))?;
    f.call("PATCH", &format!("{BASE}/1"), Some(&json!({ "data": { "type": "incidents", "attributes": { "fields": { "severity": { "type": "dropdown", "value": "SEV-1" } } } } })))?;
    wait(&f, "slack told of SEV-1", Duration::from_secs(30), || Ok(heard(&mock, "/slack", |v| text(v) == "[SEV-1] Incident #1: Checkout requests failing")))?;
    wait(&f, "pagerduty's incident raised to critical", Duration::from_secs(30), || {
        Ok(heard(&mock, "/pagerduty", |v| at(v, &["event_action"]) == "trigger" && at(v, &["payload", "severity"]) == "critical" && at(v, &["dedup_key"]).as_str() == Some(dedup.as_str())))
    })?;
    f.call("PATCH", &format!("{BASE}/1"), Some(&json!({ "data": { "attributes": { "state": "stable" } } })))?;
    wait(&f, "slack told it is stable", Duration::from_secs(30), || Ok(heard(&mock, "/slack", |v| text(v).starts_with("[Stable] Incident #1"))))?;
    let ended = detected + 30_000;
    let r = f.call("PATCH", &format!("{BASE}/1"), Some(&json!({ "data": { "attributes": { "state": "resolved", "customer_impact_end": rfc3339(ended)? } } })))?;
    let a = at(&r, &["data", "attributes"]);
    anyhow::ensure!(
        at(a, &["state"]) == "resolved" && at(a, &["resolved"]).is_string() && at(a, &["time_to_repair"]) == 30 && at(a, &["time_to_resolve"]).as_i64().is_some_and(|s| s >= 0),
        "resolved, its figures: {a}"
    );
    anyhow::ensure!(at(a, &["customer_impact_duration"]).as_i64().is_some_and(|s| (330..=345).contains(&s)), "the impact's duration: {a}");
    wait(&f, "pagerduty's incident resolved and opsgenie's alert closed, by one key", Duration::from_secs(30), || {
        Ok(heard(&mock, "/pagerduty", |v| at(v, &["event_action"]) == "resolve" && at(v, &["dedup_key"]).as_str() == Some(dedup.as_str()))
            && !mock.at(&format!("/opsgenie/v2/alerts/{dedup}/close?identifierType=alias")).is_empty())
    })?;
    wait(&f, "slack told of its resolving", Duration::from_secs(30), || Ok(heard(&mock, "/slack", |v| text(v).starts_with("[Resolved] Incident #1"))))?;
    let t = f.call("GET", &format!("{BASE}/1/timeline"), None)?;
    let said: Vec<String> = at(&t, &["data"])
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| match at(c, &["cell_type"]).as_str() {
            Some("changed") => format!("{} {}→{}", at(c, &["what"]).as_str().unwrap_or(""), at(c, &["from"]).as_str().unwrap_or(""), at(c, &["to"]).as_str().unwrap_or("")),
            Some("markdown") => format!("note {}", at(c, &["content"]).as_str().unwrap_or("")),
            other => format!("{other:?}"),
        })
        .collect();
    anyhow::ensure!(
        said == [
            "declared →SEV-2 · active",
            "note Paged **payments**.",
            "postmortem →Postmortem: checkout",
            "note Rolled back checkout-api to v41.",
            "severity SEV-2→SEV-1",
            "state active→stable",
            "state stable→resolved"
        ],
        "its timeline: {said:#?}"
    );

    // A monitor naming @incident-sev1: its group alerting declares one, SEV-1, telling the
    // integrations its message names; its recovery goes on that one's timeline.
    let m = f.call(
        "POST",
        "/api/observe/v1/monitors",
        Some(&json!({
            "name": "e2e checkout errors",
            "query": "last(last_1m):max:e2e.checkout.errors{*} by {host} > 100",
            "message": "{{#is_alert}}errors on {{host.name}} at {{value}} @incident-sev1 @slack-ops{{/is_alert}}{{#is_recovery}}{{host.name}} recovered @incident-sev1{{/is_recovery}}",
            "options": { "every": 10, "thresholds": { "critical": 100 } },
        })),
    )?;
    let monitor = at(&m, &["id"]).as_str().unwrap_or("").to_owned();
    let errors = |v: f64| f.write("e2e.checkout.errors", &["web-1"], now_ms() - 50_000, 6, |_, _| v);
    errors(150.0)?;
    wait(&f, "the monitor declaring an incident", Duration::from_secs(60), || Ok(!of_monitor(&f, &monitor)?.is_empty()))?;
    let declared = of_monitor(&f, &monitor)?;
    anyhow::ensure!(declared == [2], "one incident of its group: {declared:?}");
    let two = f.call("GET", &format!("{BASE}/2"), None)?;
    let a = at(&two, &["data", "attributes"]);
    anyhow::ensure!(
        at(a, &["title"]) == "e2e checkout errors on host:web-1"
            && at(a, &["severity"]) == "SEV-1"
            && at(a, &["fields", "detection_method", "value"]) == "monitor"
            && at(a, &["notification_handles"]) == &json!([{ "display_name": "slack-ops", "handle": "@slack-ops" }]),
        "as its monitor declared it: {two}"
    );
    anyhow::ensure!(at(a, &["declared_by", "name"]).as_str() == Some(format!("monitor:{monitor}").as_str()), "declared by its monitor: {two}");
    wait(&f, "slack told of #2", Duration::from_secs(30), || Ok(heard(&mock, "/slack", |v| text(v) == "[Declared] Incident #2: e2e checkout errors on host:web-1")))?;
    anyhow::ensure!(monitor_cells(&f, 2)? == ["ALERT"], "its alert on its timeline");
    errors(50.0)?;
    wait(&f, "its recovery on the timeline", Duration::from_secs(60), || Ok(monitor_cells(&f, 2)? == ["ALERT", "OK"]))?;
    anyhow::ensure!(at(&f.call("GET", &format!("{BASE}/2"), None)?, &["data", "attributes", "state"]) == "active", "people resolve it, not its monitor");
    // Resolved, it alerting again declares another.
    f.call("PATCH", &format!("{BASE}/2"), Some(&json!({ "data": { "attributes": { "state": "resolved" } } })))?;
    errors(170.0)?;
    wait(&f, "another incident of the group", Duration::from_secs(60), || Ok(of_monitor(&f, &monitor)? == [3, 2]))?;

    // A viewer reads and is refused declaring; a developer declares.
    let mint = |grant: &str| -> anyhow::Result<String> {
        let t = f.call("POST", "/api/tokens", Some(&json!({ "grants": [grant], "user": "owner", "note": format!("e2e {grant}") })))?;
        at(&t, &["token"]).as_str().map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no token: {t}"))
    };
    let as_token = |token: &str, method: &str, path: &str, body: Option<&Value>| -> anyhow::Result<(u16, Value)> {
        let b = body.map(serde_json::to_vec).transpose()?.unwrap_or_default();
        let (code, out) = raw(f.http, method, path, &format!("Authorization: Bearer {token}\r\nContent-Type: application/json\r\n"), &b)?;
        Ok((code, serde_json::from_slice(&out).unwrap_or(Value::Null)))
    };
    let (viewer, developer) = (mint("viewer")?, mint("developer")?);
    let drill = json!({ "data": { "attributes": { "title": "A drill", "is_test": true } } });
    anyhow::ensure!(as_token(&viewer, "GET", &format!("{BASE}/1"), None)?.0 == 200 && as_token(&viewer, "GET", &format!("{BASE}/search?query=state%3Aresolved"), None)?.0 == 200, "a viewer reads");
    anyhow::ensure!(
        as_token(&viewer, "POST", BASE, Some(&drill))?.0 == 403 && as_token(&viewer, "POST", &format!("{BASE}/1/timeline"), Some(&json!({ "content": "x" })))?.0 == 403,
        "a viewer declares nothing, notes nothing"
    );
    let (code, d) = as_token(&developer, "POST", BASE, Some(&drill))?;
    anyhow::ensure!(code == 201 && at(&d, &["data", "attributes", "public_id"]) == 4 && at(&d, &["data", "attributes", "is_test"]) == true, "a developer declares: {code} {d}");
    let by = at(&d, &["data", "relationships", "created_by_user", "data", "id"]).as_str().unwrap_or("");
    anyhow::ensure!(!by.is_empty() && by != "admin", "declared by the developer's token: {by}");

    // Datadog's incident API at its paths: the ingest token, a writer of data, reads none.
    let ingest = std::fs::read_to_string(f.root.path().join("server/observe/ingest.token"))?.trim().to_owned();
    for method in ["GET", "POST"] {
        let (code, _) = raw(f.http, method, "/api/v2/incidents", &format!("DD-API-KEY: {ingest}\r\nContent-Type: application/json\r\n"), declare.to_string().as_bytes())?;
        anyhow::ensure!(code == 403, "{method} Datadog's incident API with the ingest token: {code}");
    }

    // Refused, saying why.
    for (body, says) in [
        (json!({ "data": { "attributes": { "title": "x".repeat(513) } } }), "title"),
        (json!({ "data": { "attributes": { "title": "x", "severity": "SEV-9" } } }), "severity"),
        (json!({ "data": { "attributes": { "title": "x", "customer_impacted": true } } }), "customer_impact_scope"),
        (json!({ "data": { "attributes": { "title": "x", "fields": { "services": { "type": "multiselect", "value": "checkout" } } } } }), "multiselect"),
    ] {
        let (code, why) = ask(&f, "POST", BASE, Some(&body))?;
        anyhow::ensure!(code == 400 && why.to_string().contains(says), "{says}: {code} {why}");
    }

    // Kept through the server's kill (nothing flushed, nothing said).
    let before = f.call("GET", &format!("{BASE}/1/timeline"), None)?;
    f.kill_server();
    f.start_server()?;
    f.until(Duration::from_secs(60), "the server leads again", |s| at(s, &["leader"]).as_u64() == Some(1))?;
    anyhow::ensure!(f.call("GET", &format!("{BASE}/1/timeline"), None)? == before, "its timeline kept");
    let i = f.call("GET", &format!("{BASE}/1"), None)?;
    anyhow::ensure!(
        at(&i, &["data", "attributes", "state"]) == "resolved"
            && at(&i, &["data", "attributes", "kiln", "todos"]) == 1
            && at(&i, &["data", "attributes", "kiln", "postmortem", "title"]) == "Postmortem: checkout",
        "kept: {i}"
    );
    anyhow::ensure!(search(&f, "")?.1 == 4, "all four kept");
    let next = f.call("POST", BASE, Some(&json!({ "data": { "attributes": { "title": "After the restart" } } })))?;
    anyhow::ensure!(at(&next, &["data", "attributes", "public_id"]) == 5, "public ids go on: {next}");
    // The drill removed.
    anyhow::ensure!(
        f.request("DELETE", &format!("{BASE}/4"), None)?.0 == 204 && f.request("GET", &format!("{BASE}/4"), None)?.0 == 404 && f.request("DELETE", &format!("{BASE}/4"), None)?.0 == 404,
        "removed once"
    );
    Ok(())
}

//! The Sensitive Data Scanner, live (docs/observe.md §8.9, §13): a kiln server and a runner as
//! processes of their own, a scanning group made through the API and run on both. Records taken by
//! the server's API, by the runner's OTLP receiver and by a log source the runner collects are
//! stored as the group's rules make them: a card number replaced, an email hashed (alike on both
//! machines, and as the scanner's test hashes it), an AWS key ID replaced, a password given its
//! value replaced, an order number partly hidden; each record tagged with what matched; a line with
//! nothing sensitive stored as it came. No raw value is found by searching for it. A rule turned off,
//! then the group removed: what comes after is stored as it came, on the runner too.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::io::Write as _;
use std::time::{Duration, Instant};

use common::{Fleet, caught_up, now_ms, raw};
use serde_json::{Value, json};

const CARD: &str = "4111 1111 1111 1111";
const EMAIL: &str = "jane.doe@example.com";
const AWS_KEY: &str = "AKIAIOSFODNN7EXAMPLE";
const PASSWORD: &str = "hunter2-secret";

fn search(f: &Fleet, query: &str, from: i64) -> anyhow::Result<Vec<Value>> {
    let found = f.call("POST", "/api/observe/v1/logs/search", Some(&json!({ "query": query, "from": from, "to": now_ms() + 60_000, "limit": 100 })))?;
    Ok(found["records"].as_array().cloned().unwrap_or_default())
}

/// Waits for a record `query` finds, or fails saying so with the fleet's logs.
fn one(f: &Fleet, query: &str, from: i64) -> anyhow::Result<Value> {
    let start = Instant::now();
    loop {
        if let Some(r) = search(f, query, from)?.into_iter().next() {
            return Ok(r);
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(60), "nothing came for {query}\n{}", f.logs());
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// An OTLP/JSON request of one log record of `service`, its body and attributes.
fn otlp(service: &str, body: &str, attrs: &[(&str, &str)]) -> Vec<u8> {
    let attrs: Vec<Value> = attrs.iter().map(|(k, v)| json!({ "key": k, "value": { "stringValue": v } })).collect();
    let ns = (now_ms() * 1_000_000).to_string();
    serde_json::to_vec(&json!({ "resourceLogs": [{ "resource": { "attributes": [{ "key": "service.name", "value": { "stringValue": service } }] }, "scopeLogs": [{ "logRecords": [{ "timeUnixNano": ns, "severityNumber": 9, "body": { "stringValue": body }, "attributes": attrs }] }] }] }))
        .unwrap()
}

fn tags(r: &Value, key: &str) -> Vec<String> {
    r["attributes"][key].as_str().map(|v| v.split(',').map(str::to_owned).collect()).unwrap_or_default()
}

#[test]
#[ignore = "starts real processes"]
fn sensitive_values_are_replaced_on_every_machine_before_they_are_kept() -> anyhow::Result<()> {
    let mut f = Fleet::new()?;
    let t0 = now_ms() - 60_000;
    // The group, made before the runner joins: it is told it as it connects.
    let group = json!({ "name": "pii", "rules": [
        { "name": "cards", "standard_pattern_id": "visa_card", "text_replacement": { "type": "replacement_string", "replacement_string": "[card]" } },
        { "name": "emails", "standard_pattern_id": "email_address", "text_replacement": { "type": "hash" } },
        { "name": "aws keys", "standard_pattern_id": "aws_access_key_id", "text_replacement": { "type": "replacement_string" } },
        { "name": "passwords", "standard_pattern_id": "secret_assignment", "text_replacement": { "type": "replacement_string", "replacement_string": "***" } },
        { "name": "orders", "pattern": "order-(?P<v>\\d{6})", "tags": ["sensitive_data:order", "team:shop"], "text_replacement": { "type": "partial_replacement_from_beginning", "number_of_chars": 3 } },
    ]});
    let g = f.call("POST", "/api/observe/v1/scanner/groups", Some(&group))?;
    let gid = g["id"].as_str().unwrap_or("").to_owned();
    anyhow::ensure!(g["rules"].as_array().is_some_and(|r| r.len() == 5 && r.iter().all(|x| x["id"].as_str().is_some_and(|i| !i.is_empty()))), "each rule an id: {g}");
    // What is refused, saying why.
    for (body, says) in [
        (json!({ "name": "bad", "rules": [{ "name": "x", "pattern": "(" }] }), "its pattern"),
        (json!({ "name": "bad", "rules": [{ "name": "x", "standard_pattern_id": "nope" }] }), "no standard pattern"),
        (json!({ "name": "pii", "rules": [] }), "already"),
    ] {
        let (code, b) = f.request("POST", "/api/observe/v1/scanner/groups", Some(&body))?;
        anyhow::ensure!(code == 400 && String::from_utf8_lossy(&b).contains(says), "{body}: {code} {}", String::from_utf8_lossy(&b));
    }
    // The hash this kiln makes of the email, as its test answers it.
    let tried = f.call("POST", "/api/observe/v1/scanner/test", Some(&json!({ "records": [{ "message": EMAIL }] })))?;
    let hash = tried["records"][0]["message"].as_str().unwrap_or("").to_owned();
    anyhow::ensure!(hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()), "the email's hash: {tried}");
    let patterns = f.call("GET", "/api/observe/v1/scanner/standard_patterns", None)?;
    anyhow::ensure!(patterns["standard_patterns"].as_array().is_some_and(|p| p.len() >= 30 && p.iter().any(|x| x["id"] == "visa_card" && x["category"] == "credit_card")), "{patterns}");

    f.start_runner("r1")?;
    f.until(Duration::from_secs(90), "r1 a learner, caught up", |s| caught_up(s, &["r1"]))?;

    // Through the server's API.
    let records = json!([{ "message": format!("paid with {CARD} by {EMAIL}"), "service": "api-path", "attributes": { "login": format!("password={PASSWORD}"), "order": "order-123456" } }]);
    anyhow::ensure!(f.request("POST", "/api/observe/v1/logs", Some(&records))?.0 == 202, "the API took them");
    // Through the runner's OTLP receiver (decoded there).
    let r1 = *f.otlp.get("r1").ok_or_else(|| anyhow::anyhow!("r1's receiver"))?;
    let (code, _) =
        raw(r1, "POST", "/v1/logs", "Content-Type: application/json\r\n", &otlp("otlp-path", &format!("contact {EMAIL} about order-654321"), &[("user.email", EMAIL), ("note", "nothing here")]))?;
    anyhow::ensure!(code == 200, "the runner's receiver: {code}");
    // From a file the runner collects: a text line, a JSON line, and a line with nothing in it.
    let dir = f.root.path().join("scanned");
    std::fs::create_dir_all(&dir)?;
    let mut file = std::fs::File::create(dir.join("app.log"))?;
    writeln!(file, "key {AWS_KEY} leaked for order-777777")?;
    writeln!(file, r#"{{"msg":"signup","level":"info","user":{{"email":"{EMAIL}","plan":"pro"}}}}"#)?;
    writeln!(file, "a healthy line with nothing in it")?;
    drop(file);
    let (code, b) = f.request(
        "PUT",
        "/api/observe/v1/log_sources/scanned",
        Some(&json!({ "type": "file", "paths": [format!("{}/*.log", dir.display())], "start": "beginning", "service": "file-path", "on": "r1" })),
    )?;
    anyhow::ensure!(code == 204, "the log source: {code} {}", String::from_utf8_lossy(&b));

    // As stored: the server's.
    let r = one(&f, "service:api-path", t0)?;
    anyhow::ensure!(r["message"] == format!("paid with [card] by {hash}").as_str(), "the API's record: {r}");
    anyhow::ensure!(r["attributes"]["login"] == "password=***" && r["attributes"]["order"] == "order-***456", "its attributes: {r}");
    let mut kinds = tags(&r, "sensitive_data");
    kinds.sort();
    anyhow::ensure!(kinds == ["email_address", "order", "secret_assignment", "visa_card"], "its tags: {r}");
    anyhow::ensure!(tags(&r, "sensitive_data_category").contains(&"credit_card".to_owned()) && r["attributes"]["team"] == "shop", "its categories and a rule's own tag: {r}");
    // The runner's, through OTLP: the same hash.
    let r = one(&f, "service:otlp-path", t0)?;
    anyhow::ensure!(
        r["message"] == format!("contact {hash} about order-***321").as_str() && r["attributes"]["user.email"] == hash.as_str() && r["attributes"]["note"] == "nothing here",
        "the runner's OTLP record: {r}"
    );
    // The runner's, from its file.
    let r = one(&f, "service:file-path \"leaked\"", t0)?;
    anyhow::ensure!(r["message"] == "key [sensitive_data] leaked for order-***777" && r["host"] == "r1", "the file's text line: {r}");
    let r = one(&f, "service:file-path signup", t0)?;
    anyhow::ensure!(r["attributes"]["user.email"] == hash.as_str() && r["attributes"]["user.plan"] == "pro", "the file's JSON line: {r}");
    let r = one(&f, "service:file-path healthy", t0)?;
    anyhow::ensure!(r["message"] == "a healthy line with nothing in it" && r["attributes"]["sensitive_data"].is_null(), "a line with nothing sensitive, as it came: {r}");
    // No raw value is found by searching for it, in a message or an attribute.
    for q in [
        format!("\"{CARD}\""),
        format!("\"{EMAIL}\""),
        AWS_KEY.to_owned(),
        format!("\"{PASSWORD}\""),
        "\"order-123456\"".to_owned(),
        format!("@user.email:\"{EMAIL}\""),
        format!("@login:\"password={PASSWORD}\""),
    ] {
        let found = search(&f, &q, t0)?;
        anyhow::ensure!(found.is_empty(), "{q} found {found:?}");
    }

    // A rule turned off: what comes after keeps its card, on the runner too (told it at once).
    let mut changed = f.call("GET", &format!("/api/observe/v1/scanner/groups/{gid}"), None)?;
    let cards_id = changed["rules"][0]["id"].clone();
    changed["rules"][0]["is_enabled"] = json!(false);
    let put = f.call("PUT", &format!("/api/observe/v1/scanner/groups/{gid}"), Some(&changed))?;
    anyhow::ensure!(put["rules"][0]["id"] == cards_id, "the rule kept its id: {put}");
    let start = Instant::now();
    let mut n = 0;
    loop {
        n += 1;
        let (code, _) = raw(r1, "POST", "/v1/logs", "Content-Type: application/json\r\n", &otlp("after-off", &format!("probe {n} paid with 4012 8888 8888 1881"), &[]))?;
        anyhow::ensure!(code == 200, "{code}");
        std::thread::sleep(Duration::from_millis(700));
        if search(&f, "service:after-off", t0)?.iter().any(|r| r["message"].as_str().is_some_and(|m| m.ends_with("4012 8888 8888 1881"))) {
            break;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(60), "the runner never stopped replacing cards\n{}", f.logs());
    }
    // The group removed: an email after it is stored as it came.
    anyhow::ensure!(f.request("DELETE", &format!("/api/observe/v1/scanner/groups/{gid}"), None)?.0 == 204, "removed");
    anyhow::ensure!(f.request("GET", &format!("/api/observe/v1/scanner/groups/{gid}"), None)?.0 == 404, "gone");
    let start = Instant::now();
    loop {
        let (code, _) = raw(r1, "POST", "/v1/logs", "Content-Type: application/json\r\n", &otlp("after-removed", "mail after@example.com", &[]))?;
        anyhow::ensure!(code == 200, "{code}");
        std::thread::sleep(Duration::from_millis(700));
        if search(&f, "service:after-removed", t0)?.iter().any(|r| r["message"] == "mail after@example.com") {
            break;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(60), "the runner never stopped scanning\n{}", f.logs());
    }
    Ok(())
}

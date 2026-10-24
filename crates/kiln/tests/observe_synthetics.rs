//! Synthetic tests, live (docs/observe.md §10.4, §13): a kiln server and a runner as processes of
//! their own, the locations; an HTTP endpoint the test serves (switched to answer 500 and back), a
//! TLS one (kiln's own TLS, its certificate from a CA the test makes), and a port nothing
//! listens on. What a user relies on: each test runs from each location it names and is judged by
//! its assertions; Datadog's series of its runs are written, by location; a location that starts
//! failing, or recovers, is said once; a test that alerts owns a monitor that tells a webhook when
//! enough locations fail and when they recover; run now, each location runs it at once; removed,
//! its monitor goes with it.
//!
//! `cargo test -p kiln --test observe_synthetics -- --ignored`.
#![cfg(unix)]

mod common;

use std::io::{Read as _, Write as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use common::{Fleet, at, caught_up, free_port, now_ms};
use serde_json::{Value, json};

/// An HTTP endpoint: `/health` answers `{"status":"ok"}` (or 500 while `down`), `/old` redirects to
/// it; each request counted.
struct Endpoint {
    port: u16,
    down: Arc<AtomicBool>,
    asked: Arc<AtomicU64>,
}

impl Endpoint {
    fn start() -> anyhow::Result<Self> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let (down, asked) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicU64::new(0)));
        let (d, a) = (down.clone(), asked.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let (d, a) = (d.clone(), a.clone());
                std::thread::spawn(move || {
                    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match s.read(&mut chunk) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(chunk.get(..n).unwrap_or(&[])),
                        }
                    }
                    a.fetch_add(1, Ordering::Relaxed);
                    let head = String::from_utf8_lossy(&buf);
                    let path = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
                    let answer = match (path.as_str(), d.load(Ordering::Relaxed)) {
                        ("/old", _) => "HTTP/1.1 301 Moved Permanently\r\nLocation: /health\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
                        (_, true) => "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: 22\r\nConnection: close\r\n\r\n{\"status\":\"degraded\"}\n".to_owned(),
                        _ => "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nX-Served-By: e2e\r\nContent-Length: 16\r\nConnection: close\r\n\r\n{\"status\":\"ok\"}\n".to_owned(),
                    };
                    let _ = s.write_all(answer.as_bytes());
                });
            }
        });
        Ok(Self { port, down, asked })
    }
}

/// A CA, and a certificate it signs for `localhost` valid 90 days: (the CA's PEM, the leaf's, its
/// key's).
fn certificates() -> anyhow::Result<(String, String, String)> {
    let ca_key = rcgen::KeyPair::generate()?;
    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new())?;
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca.distinguished_name.push(rcgen::DnType::CommonName, "kiln test synthetics CA");
    let ca_pem = ca.self_signed(&ca_key)?.pem();
    let issuer = rcgen::Issuer::from_ca_cert_pem(&ca_pem, ca_key)?;
    let key = rcgen::KeyPair::generate()?;
    let mut leaf = rcgen::CertificateParams::new(vec!["localhost".to_owned()])?;
    let now = time::OffsetDateTime::now_utc();
    leaf.not_before = now - time::Duration::days(1);
    leaf.not_after = now + time::Duration::days(90);
    let leaf = leaf.signed_by(&key, &issuer)?;
    Ok((ca_pem, leaf.pem(), key.serialize_pem()))
}

/// A TLS endpoint on 127.0.0.1:`port`, kiln's own TLS (rustls) with a certificate of the test's
/// CA, answering each request with a page (as `openssl s_server -www` did; LibreSSL's, macOS's,
/// cannot bind one address), until dropped.
struct TlsEndpoint {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TlsEndpoint {
    fn start(port: u16, cert: &str, key: &str) -> anyhow::Result<Self> {
        let server = Arc::new(kiln::http::tls::ServerTls::from_pem(cert, key)?);
        let listener = std::net::TcpListener::bind(("127.0.0.1", port))?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = std::thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                let Ok((stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                };
                let server = server.clone();
                std::thread::spawn(move || {
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    // A configuration of its own: the connection's state machine borrows it.
                    let Ok(config) = server.config() else { return };
                    let Ok(conn) = hyper_quic::rustls::ServerConnection::new(&config) else { return };
                    let mut tls = hyper_quic::rustls::StreamOwned::new(conn, config, stream);
                    // The request (or none: a handshake alone, a certificate's check), then a page.
                    let mut buf = [0u8; 4096];
                    let _ = tls.read(&mut buf);
                    let body = "<html><body>kiln's test TLS endpoint</body></html>";
                    let _ = write!(tls, "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    tls.conn.send_close_notify();
                    let _ = tls.flush();
                });
            }
        });
        Ok(Self { stop, thread: Some(thread) })
    }
}

impl Drop for TlsEndpoint {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A test's state at each location, from the API: location → (passed, runs).
fn states(f: &Fleet, name: &str) -> anyhow::Result<std::collections::BTreeMap<String, (Option<bool>, u64, Value)>> {
    let all = f.call("GET", "/api/observe/v1/synthetics", None)?;
    let t = at(&all, &["synthetics"]).as_array().into_iter().flatten().find(|t| at(t, &["name"]) == name).cloned().unwrap_or(Value::Null);
    Ok(at(&t, &["machines"])
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| {
            let st = at(m, &["state"]);
            (at(m, &["machine"]).as_str().unwrap_or("").to_owned(), (at(st, &["passed"]).as_bool(), at(st, &["runs"]).as_u64().unwrap_or(0), st.clone()))
        })
        .collect())
}

/// Waits until `done` holds of test `name`'s states.
fn until_states(f: &Fleet, name: &str, what: &str, within: Duration, done: impl Fn(&std::collections::BTreeMap<String, (Option<bool>, u64, Value)>) -> bool) -> anyhow::Result<std::collections::BTreeMap<String, (Option<bool>, u64, Value)>> {
    let start = Instant::now();
    loop {
        let s = states(f, name)?;
        if done(&s) {
            return Ok(s);
        }
        anyhow::ensure!(start.elapsed() < within, "{name}: {what}: {s:?}\n{}", f.logs());
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// A query's series by the value of their tag `key`, each one's values.
fn by(f: &Fleet, q: &str, key: &str, from: i64, to: i64) -> anyhow::Result<std::collections::BTreeMap<String, Vec<Option<f64>>>> {
    let a = f.call("POST", "/api/observe/v1/query", Some(&json!({ "query": q, "from": from, "to": to, "step": 10_000 })))?;
    Ok(at(&a, &["series"]).as_array().into_iter().flatten().map(|s| (at(s, &["tags", key]).as_str().unwrap_or("").to_owned(), at(s, &["values"]).as_array().into_iter().flatten().map(Value::as_f64).collect())).collect())
}

fn count(f: &Fleet, query: &str, from: i64) -> anyhow::Result<u64> {
    let found = f.call("POST", "/api/observe/v1/logs/search", Some(&json!({ "query": query, "from": from, "to": now_ms() + 60_000, "limit": 100 })))?;
    Ok(at(&found, &["records"]).as_array().map_or(0, Vec::len) as u64)
}

#[test]
#[ignore = "starts a kiln server and a runner as processes, and endpoints for them to test"]
fn synthetic_tests_run_from_each_location_alert_when_enough_fail_and_recover() -> anyhow::Result<()> {
    let t0 = now_ms();
    let endpoint = Endpoint::start()?;
    let (ca, cert, key) = certificates()?;
    let tls_port = free_port()?;
    let _tls = TlsEndpoint::start(tls_port, &cert, &key)?;
    // A receiver for the monitor's notifications.
    let hook = std::net::TcpListener::bind("127.0.0.1:0")?;
    let hook_port = hook.local_addr()?.port();
    let heard: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    {
        let heard = heard.clone();
        std::thread::spawn(move || {
            for stream in hook.incoming() {
                let Ok(mut s) = stream else { continue };
                let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                loop {
                    match s.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(chunk.get(..n).unwrap_or(&[])),
                    }
                    let text = String::from_utf8_lossy(&buf);
                    if let Some((head, body)) = text.split_once("\r\n\r\n") {
                        let len: usize = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0))).unwrap_or(0);
                        if body.len() >= len {
                            heard.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(body.to_owned());
                            break;
                        }
                    }
                }
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
            }
        });
    }

    let mut f = Fleet::new()?;
    f.start_runner("r1")?;
    f.until(Duration::from_secs(90), "r1 a learner, caught up", |s| caught_up(s, &["r1"]))?;
    let (code, _) = f.request("PUT", "/api/observe/v1/integrations/hook", Some(&json!({ "kind": "webhook", "url": format!("http://127.0.0.1:{hook_port}/hook") })))?;
    anyhow::ensure!(code == 204);
    let base = format!("http://127.0.0.1:{}", endpoint.port);
    let tests = [
        (
            "api",
            json!({ "type": "http", "url": format!("{base}/old"), "assertions": [
                { "type": "status_code", "operator": "is", "target": 200 },
                { "type": "body", "operator": "contains", "target": "\"ok\"" },
                { "type": "header", "operator": "is", "property": "x-served-by", "target": "e2e" },
                { "type": "response_time", "operator": "less_than", "target": 5000 }
            ], "locations": ["server", "r1"], "every_s": 10, "tags": ["team:web"], "alert": { "message": "api down @webhook-hook", "min_locations": 2 } }),
        ),
        ("tls", json!({ "type": "tls", "host": "localhost", "port": tls_port, "ca": ca, "assertions": [{ "type": "certificate_days", "operator": "more_than", "target": 30 }], "locations": ["server"], "every_s": 10 })),
        ("https", json!({ "type": "http", "url": format!("https://localhost:{tls_port}/"), "ca": ca, "locations": ["r1"], "every_s": 10 })),
        ("dns", json!({ "type": "dns", "host": "localhost", "assertions": [{ "type": "address", "operator": "is", "target": "127.0.0.1" }], "locations": ["*"], "every_s": 10 })),
        ("closed", json!({ "type": "tcp", "host": "127.0.0.1", "port": 9, "locations": ["server"], "every_s": 10 })),
        ("hourly", json!({ "type": "tcp", "host": "127.0.0.1", "port": endpoint.port, "locations": ["server", "r1"], "every_s": 3600 })),
    ];
    for (name, body) in &tests {
        let (code, b) = f.request("PUT", &format!("/api/observe/v1/synthetics/{name}"), Some(body))?;
        anyhow::ensure!(code == 204, "{name}: {code} {}", String::from_utf8_lossy(&b));
    }
    // A test the server tries once, nothing kept.
    let tried = f.call("POST", "/api/observe/v1/synthetics/new/test", Some(&json!({ "type": "http", "url": format!("{base}/health"), "assertions": [{ "type": "status_code", "operator": "is", "target": 201 }] })))?;
    anyhow::ensure!(at(&tried, &["passed"]) == false && at(&tried, &["failures"]).to_string().contains("status_code is 201: was 200") && at(&tried, &["firstbyte_ms"]).as_f64().is_some(), "{tried}");

    // Each runs from each location, judged by its assertions.
    let both = |s: &std::collections::BTreeMap<String, (Option<bool>, u64, Value)>| ["server", "r1"].iter().all(|m| s.get(*m).is_some_and(|x| x.0 == Some(true)));
    let api = until_states(&f, "api", "passing at the server and r1", Duration::from_secs(60), both)?;
    anyhow::ensure!(api.values().all(|(_, _, st)| at(st, &["status"]) == 200), "the redirect followed: {api:?}");
    until_states(&f, "dns", "every machine resolving", Duration::from_secs(60), both)?;
    let tls = until_states(&f, "tls", "a certificate 90 days long", Duration::from_secs(60), |s| s.get("server").is_some_and(|x| x.0 == Some(true)))?;
    let minutes = tls.get("server").map(|x| at(&x.2, &["certificate_minutes"]).as_i64().unwrap_or(0)).unwrap_or(0);
    anyhow::ensure!((89 * 1440..=90 * 1440).contains(&minutes), "its certificate's minutes left: {minutes}");
    until_states(&f, "https", "an HTTPS request from r1 trusting the CA given", Duration::from_secs(60), |s| s.get("r1").is_some_and(|x| x.0 == Some(true)))?;
    let closed = until_states(&f, "closed", "failing", Duration::from_secs(60), |s| s.get("server").is_some_and(|x| x.0 == Some(false)))?;
    anyhow::ensure!(closed.get("server").is_some_and(|x| at(&x.2, &["failed"]).to_string().contains("connect")), "{closed:?}");
    // Datadog's series, by location.
    let to = now_ms() + 60_000;
    let by_location = by(&f, "avg:synthetics.http.response.time{test:api} by {location}", "location", t0 - 60_000, to)?;
    anyhow::ensure!(by_location.keys().map(String::as_str).collect::<Vec<_>>() == ["r1", "server"] && by_location.values().all(|v| v.iter().flatten().any(|ms| *ms > 0.0 && *ms < 5000.0)), "the response time at each location: {by_location:?}");
    let runs = by(&f, "sum:synthetics.test_runs{test:api,status:passed,team:web} by {location}.as_count()", "location", t0 - 60_000, to)?;
    anyhow::ensure!(runs.len() == 2, "{runs:?}");
    for (q, what) in [("avg:synthetics.http.connect.time{test:api}", "connect"), ("avg:synthetics.http.firstbyte.time{test:api}", "first byte"), ("avg:synthetics.http.redirect.time{test:api}", "the redirect"), ("avg:synthetics.ssl.time_to_expiry{test:tls}", "expiry"), ("avg:synthetics.dns.response.time{test:dns}", "DNS")] {
        anyhow::ensure!(by(&f, q, "test", t0 - 60_000, to)?.values().any(|v| v.iter().flatten().count() > 0), "no {what} series: {q}");
    }
    anyhow::ensure!(count(&f, "source:kiln.event @event.type:synthetics @test:closed @result:failed", t0)? == 1, "a failing location said once");

    // Run now: the hourly test runs at once at both locations.
    let (code, _) = f.request("POST", "/api/observe/v1/synthetics/hourly/run", None)?;
    anyhow::ensure!(code == 202);
    until_states(&f, "hourly", "run at once at both", Duration::from_secs(20), |s| ["server", "r1"].iter().all(|m| s.get(*m).is_some_and(|x| x.1 >= 1)))?;

    // The endpoint fails: both locations fail, the monitor alerts, and recovers once it is back.
    let before = endpoint.asked.load(Ordering::Relaxed);
    endpoint.down.store(true, Ordering::Relaxed);
    until_states(&f, "api", "failing at both", Duration::from_secs(60), |s| ["server", "r1"].iter().all(|m| s.get(*m).is_some_and(|x| x.0 == Some(false))))?;
    anyhow::ensure!(endpoint.asked.load(Ordering::Relaxed) > before);
    let start = Instant::now();
    while !heard.lock().map(|h| h.iter().any(|b| b.contains("\"state\":\"ALERT\"") && b.contains("api down"))).unwrap_or(false) {
        anyhow::ensure!(start.elapsed() < Duration::from_secs(90), "the monitor never alerted: {:?}\n{}", heard.lock().map(|h| h.clone()).unwrap_or_default(), f.logs());
        std::thread::sleep(Duration::from_millis(250));
    }
    let monitor = f.call("GET", "/api/observe/v1/monitors/syn-api", None)?;
    anyhow::ensure!(at(&monitor, &["state"]) == "ALERT", "{monitor}");
    endpoint.down.store(false, Ordering::Relaxed);
    let start = Instant::now();
    while !heard.lock().map(|h| h.iter().any(|b| b.contains("\"state\":\"OK\""))).unwrap_or(false) {
        anyhow::ensure!(start.elapsed() < Duration::from_secs(90), "the monitor never recovered\n{}", f.logs());
        std::thread::sleep(Duration::from_millis(250));
    }
    anyhow::ensure!(count(&f, "source:kiln.event @event.type:synthetics @test:api @result:failed", t0)? == 2, "each location's failing said once");
    let start = Instant::now();
    while count(&f, "source:kiln.event @event.type:synthetics @test:api @result:passed", t0)? < 2 {
        anyhow::ensure!(start.elapsed() < Duration::from_secs(30), "each location's recovery said");
        std::thread::sleep(Duration::from_millis(250));
    }

    // Removed: its monitor goes with it.
    let (code, _) = f.request("DELETE", "/api/observe/v1/synthetics/api", None)?;
    anyhow::ensure!(code == 204);
    let (code, _) = f.request("GET", "/api/observe/v1/monitors/syn-api", None)?;
    anyhow::ensure!(code == 404, "its monitor still there: {code}");
    Ok(())
}

//! Error Tracking, live (docs/observe.md §8.8, §13): a kiln server and a runner as processes, and a
//! shop (Flask, in Docker) that fails three ways: a cart's item it will not total (`ValueError`, in
//! `Cart.total`), an order it cannot load (`KeyError`, in `load`), a body it cannot parse (json's
//! `JSONDecodeError`, raised in json's own code, called from `parse`), each failure's message its
//! own (the order's number; a cart's id, in quotes). Traced by OpenTelemetry's own Python SDK (its
//! spans and its logs, the app's log of each failure, to the server over OTLP/HTTP) and by
//! Datadog's own (ddtrace, straight to the runner's receiver).
//!
//! What a user relies on: each place's failures one issue of each service and source (the SDK's
//! spans, its logs; ddtrace's spans), every failure counted, its type and its place (its file, its
//! function) as its stack says; an issue resolved stays so while the version it was resolved in
//! fails again, and comes back for review, a regression, when a new version fails so; an issue's
//! latest error whole (its stack), and its errors found by its id. And Datadog's Error Tracking
//! monitors, telling a webhook: a monitor of new issues tells each as it comes (its type, message,
//! place and stack), its end once resolved, nothing of one ignored, and the regression again; one
//! made after the issues came tells the regression alone; one of impact (more than nine errors in
//! an hour) tells the issues past it while for review, none resolved.
//!
//! It needs Docker: `cargo test -p kiln --test observe_errors -- --ignored --nocapture`.

mod common;

use std::io::{Read, Write};
use std::process::Command;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use common::{Fleet, at, free_port, now_ms};
use serde_json::{Value, json};

const OTEL_IMAGE: &str = "kiln-otel-python:e2e";
const OTEL_DOCKERFILE: &str = "FROM python:3.12-slim\nRUN pip install --no-cache-dir flask requests opentelemetry-sdk opentelemetry-exporter-otlp-proto-http opentelemetry-exporter-otlp-proto-grpc opentelemetry-instrumentation-flask opentelemetry-instrumentation-requests\n";
const DD_IMAGE: &str = "kiln-ddtrace-python:e2e";
const DD_DOCKERFILE: &str = "FROM python:3.12-slim\nRUN pip install --no-cache-dir ddtrace==4.16.0 flask==3.1.3 requests\n";

/// Failures each place makes, each run.
const EACH: u64 = 5;

const SHOP: &str = r#"
import http.server, json, logging, os, socket, subprocess, sys, threading, time, urllib.error, urllib.request

MODE = sys.argv[1]
if MODE == "dd":
    import ddtrace.auto  # Flask's integration, before Flask is imported
    try:
        from ddtrace.trace import tracer
    except ImportError:
        from ddtrace import tracer


class Cart:
    def __init__(self, items):
        self.items = items

    def total(self):
        for i in self.items:
            if i < 0:
                raise ValueError(f"bad item {i} in cart 'c-{-i * 7}'")
        return sum(self.items)


def load(order):
    raise KeyError(f"order {order}")


def parse(body):
    return json.loads(body)


def control(port, app_port, flush):
    # Untraced: whether the app listens, and its telemetry flushed.
    class H(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            ok = True
            if self.path == "/ready":
                try:
                    socket.create_connection(("127.0.0.1", app_port), timeout=1).close()
                except OSError:
                    ok = False
            elif self.path == "/flush":
                flush()
            self.send_response(200 if ok else 503)
            self.send_header("Content-Length", "0")
            self.end_headers()

        def log_message(self, *a):
            pass

    s = http.server.ThreadingHTTPServer(("127.0.0.1", port), H)
    threading.Thread(target=s.serve_forever, daemon=True).start()


def shop():
    from flask import Flask
    app = Flask("shop")
    if MODE == "otel":
        from opentelemetry import trace
        from opentelemetry._logs import set_logger_provider
        from opentelemetry.exporter.otlp.proto.http._log_exporter import OTLPLogExporter
        from opentelemetry.exporter.otlp.proto.http.trace_exporter import OTLPSpanExporter
        from opentelemetry.instrumentation.flask import FlaskInstrumentor
        from opentelemetry.sdk._logs import LoggerProvider, LoggingHandler
        from opentelemetry.sdk._logs.export import BatchLogRecordProcessor
        from opentelemetry.sdk.resources import Resource
        from opentelemetry.sdk.trace import TracerProvider
        from opentelemetry.sdk.trace.export import BatchSpanProcessor
        server, headers = os.environ["KILN"], {"Authorization": "Bearer " + os.environ["TOKEN"]}
        res = Resource.create({"service.name": "shop-otel", "deployment.environment.name": "e2e", "service.version": os.environ["VERSION"]})
        tp = TracerProvider(resource=res)
        tp.add_span_processor(BatchSpanProcessor(OTLPSpanExporter(endpoint=server + "/v1/traces", headers=headers), schedule_delay_millis=200))
        trace.set_tracer_provider(tp)
        lp = LoggerProvider(resource=res)
        lp.add_log_record_processor(BatchLogRecordProcessor(OTLPLogExporter(endpoint=server + "/v1/logs", headers=headers), schedule_delay_millis=200))
        set_logger_provider(lp)
        # Flask logs each failure it answers 500 with, its exception with it.
        app.logger.addHandler(LoggingHandler(level=logging.ERROR, logger_provider=lp))
        FlaskInstrumentor().instrument_app(app)

        def flush():
            tp.force_flush()
            lp.force_flush()
    else:
        def flush():
            tracer.flush()

    @app.route("/total/<int:i>")
    def total(i):
        return str(Cart([1, -i]).total())

    @app.route("/load/<int:i>")
    def load_order(i):
        return str(load(i))

    @app.route("/parse/<int:i>")
    def parse_body(i):
        return str(parse("{bad " + str(i)))

    control(int(os.environ["CTL"]), int(os.environ["PORT"]), flush)
    app.run(host="127.0.0.1", port=int(os.environ["PORT"]))


def get(url):
    try:
        with urllib.request.urlopen(url, timeout=30) as r:
            return r.status
    except urllib.error.HTTPError as e:
        return e.code


def drive():
    shop_url, ctl = "http://127.0.0.1:" + os.environ["PORT"], "http://127.0.0.1:" + os.environ["CTL"]
    for _ in range(600):
        try:
            if get(ctl + "/ready") == 200:
                break
        except Exception:
            pass
        time.sleep(0.1)
    codes = []
    first, each = int(os.environ["FIRST"]), int(os.environ["EACH"])
    for place in os.environ["PLACES"].split(","):
        for i in range(first, first + each):
            codes.append(get(f"{shop_url}/{place}/{i}"))
    get(ctl + "/flush")
    time.sleep(1)
    print("CODES " + json.dumps(codes), flush=True)


if MODE.startswith("drive-"):
    kid = subprocess.Popen([sys.executable, "-u", __file__, MODE[len("drive-"):]])
    try:
        drive()
    finally:
        kid.terminate()
else:
    shop()
"#;

fn docker(args: &[&str], stdin: Option<&str>) -> anyhow::Result<String> {
    use std::io::Write as _;
    let mut c = Command::new("docker");
    c.args(args).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    if stdin.is_some() {
        c.stdin(std::process::Stdio::piped());
    }
    let mut child = c.spawn()?;
    if let (Some(text), Some(mut i)) = (stdin, child.stdin.take()) {
        i.write_all(text.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    anyhow::ensure!(out.status.success(), "docker {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn until(what: &str, within: Duration, mut done: impl FnMut() -> anyhow::Result<bool>, logs: &dyn Fn() -> String) -> anyhow::Result<()> {
    let start = Instant::now();
    loop {
        if done()? {
            return Ok(());
        }
        anyhow::ensure!(start.elapsed() < within, "{what}\n{}", logs());
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// The shop run once in `mode` (`otel`, `dd`), failing `EACH` times at each of `places`, its orders
/// from `first`, its version `version`: every request answered 500.
fn fail(mode: &str, places: &str, first: u64, version: &str, env: &[(&str, String)]) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("shop.py"), SHOP)?;
    #[cfg(unix)]
    Command::new("chmod").args(["-R", "a+rX"]).arg(dir.path()).output()?;
    // The shop and its driver in one container (each other at its own 127.0.0.1); the server and the
    // runner where the container reaches this machine.
    let mut args: Vec<String> = vec!["run".into(), "--rm".into()];
    args.extend(common::DockerNet::get().args(&[], &[]));
    args.extend(["-v".to_owned(), format!("{}:/e2e:ro", dir.path().display())]);
    let mut all: Vec<(&str, String)> = vec![
        ("PORT", free_port()?.to_string()),
        ("CTL", free_port()?.to_string()),
        ("FIRST", first.to_string()),
        ("EACH", EACH.to_string()),
        ("PLACES", places.to_owned()),
        ("VERSION", version.to_owned()),
    ];
    all.extend(env.iter().cloned());
    for (k, v) in all {
        args.push("-e".to_owned());
        args.push(format!("{k}={v}"));
    }
    let image = if mode == "dd" { DD_IMAGE } else { OTEL_IMAGE };
    args.extend([image, "python", "-u", "/e2e/shop.py", &format!("drive-{mode}")].map(str::to_owned));
    let out = docker(&args.iter().map(String::as_str).collect::<Vec<_>>(), None)?;
    let codes: Vec<u64> = out.lines().find_map(|l| l.strip_prefix("CODES ")).map(serde_json::from_str).transpose()?.ok_or_else(|| anyhow::anyhow!("the driver's answers:\n{out}"))?;
    let want = EACH * places.split(',').count() as u64;
    anyhow::ensure!(codes.len() as u64 == want && codes.iter().all(|&c| c == 500), "{mode}: {codes:?}");
    Ok(())
}

fn issues(f: &Fleet, from: i64) -> anyhow::Result<Vec<Value>> {
    let l = f.call("GET", &format!("/api/observe/v1/errors/issues?from={from}&to={}&all=true&limit=1000", now_ms() + 60_000), None)?;
    Ok(at(&l, &["issues"]).as_array().cloned().unwrap_or_default())
}

/// The issue of `service`'s `source` errors grouped by `function`.
fn find<'a>(all: &'a [Value], service: &str, source: &str, function: &str) -> Option<&'a Value> {
    all.iter().find(|i| at(i, &["service"]) == service && at(i, &["source"]) == source && at(i, &["function"]) == function)
}

/// The id of an issue the scenario found.
fn id_of(all: &[Value], service: &str, source: &str, function: &str) -> anyhow::Result<String> {
    find(all, service, source, function).and_then(|i| at(i, &["id"]).as_str()).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no issue of {service}'s {source} in {function}"))
}

/// A receiver of webhooks: each body it is sent (JSON), in order.
fn receiver() -> anyhow::Result<(u16, Arc<Mutex<Vec<Value>>>)> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let heard: Arc<Mutex<Vec<Value>>> = Arc::default();
    let kept = heard.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
            let (mut buf, mut chunk) = (Vec::new(), [0u8; 8192]);
            loop {
                match s.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(chunk.get(..n).unwrap_or(&[])),
                }
                let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                let head = String::from_utf8_lossy(buf.get(..end).unwrap_or(&[])).to_ascii_lowercase();
                let len: usize = head.lines().find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0))).unwrap_or(0);
                let body = buf.get(end + 4..).unwrap_or(&[]);
                if body.len() >= len {
                    if let Ok(v) = serde_json::from_slice::<Value>(body.get(..len).unwrap_or(body)) {
                        kept.lock().unwrap_or_else(PoisonError::into_inner).push(v);
                    }
                    break;
                }
            }
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        }
    });
    Ok((port, heard))
}

/// What monitor `id` told the receiver, in order: each notice.
fn notices(heard: &Mutex<Vec<Value>>, id: &str) -> Vec<Value> {
    heard.lock().unwrap_or_else(PoisonError::into_inner).iter().filter(|b| at(b, &["notice", "monitor_id"]) == id).map(|b| at(b, &["notice"]).clone()).collect()
}

/// The issue a notice, or a monitor's group, is of: its group's one tag.
fn issue_in(v: &Value) -> &str {
    at(v, &["group"]).as_array().and_then(|g| g.first()).and_then(|t| t.get(1)).and_then(Value::as_str).unwrap_or("")
}

/// The issues monitor `id` told of as alerting, in order.
fn alerted(heard: &Mutex<Vec<Value>>, id: &str) -> Vec<String> {
    notices(heard, id).iter().filter(|n| at(n, &["state"]) == "ALERT").map(|n| issue_in(n).to_owned()).collect()
}

#[test]
#[ignore = "needs Docker: runs a Flask shop under OpenTelemetry's Python SDK and Datadog's tracer, and a kiln server and runner as processes"]
fn a_shop_s_failures_are_issues_grouped_as_datadog_groups_them_and_regress_in_a_new_version() -> anyhow::Result<()> {
    let t0 = now_ms() - 60_000;
    docker(&["build", "-q", "-t", OTEL_IMAGE, "-"], Some(OTEL_DOCKERFILE))?;
    docker(&["build", "-q", "-t", DD_IMAGE, "-"], Some(DD_DOCKERFILE))?;
    let mut f = Fleet::new()?;
    let ingest = std::fs::read_to_string(f.root.path().join("server").join("observe").join("ingest.token"))?.trim().to_owned();
    let apm = free_port()?;
    f.start_runner_with("web-1", &[("KILN_DD_TRACE_ADDR", &format!("127.0.0.1:{apm}"))])?;
    until("the runner's receiver", Duration::from_secs(60), || Ok(common::raw(apm, "GET", "/info", "", b"").is_ok_and(|(c, _)| c == 200)), &|| f.logs())?;
    until(
        "the runner replicating",
        Duration::from_secs(60),
        || Ok(at(&f.call("GET", "/api/observe/v1/status", None)?, &["machines"]).as_array().is_some_and(|m| m.iter().any(|m| at(m, &["up"]) == true))),
        &|| f.logs(),
    )?;
    let host = common::DockerNet::get().host();
    let otel = |token: &str| vec![("KILN", format!("http://{host}:{}", f.http)), ("TOKEN", token.to_owned())];
    let dd = vec![
        ("DD_SERVICE", "shop-dd".to_owned()),
        ("DD_AGENT_HOST", host.to_owned()),
        ("DD_TRACE_AGENT_PORT", apm.to_string()),
        ("DD_ENV", "e2e".to_owned()),
        ("DD_VERSION", "1.0".to_owned()),
        ("DD_REMOTE_CONFIGURATION_ENABLED", "false".to_owned()),
        ("DD_INSTRUMENTATION_TELEMETRY_ENABLED", "false".to_owned()),
    ];
    // Datadog's Error Tracking monitors of the OTel shop, telling a webhook: of its new issues (each
    // one's type, message, place and stack said), and of its issues of impact (more than nine
    // errors in an hour).
    let (hook, heard) = receiver()?;
    let (code, _) = f.request("PUT", "/api/observe/v1/integrations/hook", Some(&json!({ "kind": "webhook", "url": format!("http://127.0.0.1:{hook}/hook") })))?;
    anyhow::ensure!((200..300).contains(&code), "the webhook: {code}");
    let monitor = |name: &str, query: &str, message: &str| -> anyhow::Result<String> {
        let m = f.call("POST", "/api/observe/v1/monitors", Some(&json!({ "name": name, "type": "error-tracking alert", "query": query, "message": message, "options": { "every": 10 } })))?;
        at(&m, &["id"]).as_str().map(str::to_owned).ok_or_else(|| anyhow::anyhow!("the monitor {name}: {m}"))
    };
    let new = monitor(
        "New shop-otel issue",
        r#"error-tracking("service:shop-otel").source("backend").new().rollup("count").by("issue.id").last("1d") > 0"#,
        "{{#is_alert}}New issue {{issue.attributes.error.type}}: {{issue.attributes.error.message}} at {{issue.attributes.error.file}}\n{{issue.attributes.error.stack}}{{/is_alert}}{{#is_recovery}}{{issue.attributes.error.type}} recovered{{/is_recovery}} @webhook-hook",
    )?;
    let impact = monitor(
        "shop-otel issue of impact",
        r#"error-tracking("service:shop-otel").source("backend").impact().rollup("count").by("issue.id").last("1h") > 9"#,
        "{{issue.attributes.error.type}}: {{value}} errors @webhook-hook",
    )?;
    fail("otel", "total,load,parse", 1, "1.0", &otel(&ingest))?;
    fail("dd", "total,load,parse", 1, "1.0", &dd)?;

    // Each place's failures one issue of each service and source, every failure counted, its type
    // and its place as its stack says (json's own frames passed over: where the shop called it).
    let want: [(&str, &str, &str, &str); 9] = [
        ("shop-otel", "span", "total", "ValueError"),
        ("shop-otel", "span", "load", "KeyError"),
        ("shop-otel", "span", "parse", "json.decoder.JSONDecodeError"),
        ("shop-otel", "log", "total", "ValueError"),
        ("shop-otel", "log", "load", "KeyError"),
        ("shop-otel", "log", "parse", "JSONDecodeError"),
        ("shop-dd", "span", "total", "builtins.ValueError"),
        ("shop-dd", "span", "load", "builtins.KeyError"),
        ("shop-dd", "span", "parse", "json.decoder.JSONDecodeError"),
    ];
    let last = std::cell::RefCell::new(Vec::new());
    until(
        "every failure an issue's, counted",
        Duration::from_secs(120),
        || {
            let all = issues(&f, t0)?;
            let done = all.len() == want.len() && want.iter().all(|(s, src, func, _)| find(&all, s, src, func).is_some_and(|i| at(i, &["errors"]).as_f64() == Some(EACH as f64)));
            *last.borrow_mut() = all;
            Ok(done)
        },
        &|| format!("{}\n{}", serde_json::to_string_pretty(&*last.borrow()).unwrap_or_default(), f.logs()),
    )?;
    let all = last.borrow().clone();
    for (service, source, function, kind) in want {
        let i = find(&all, service, source, function).ok_or_else(|| anyhow::anyhow!("{service} {source} {function}"))?;
        anyhow::ensure!(at(i, &["type"]) == kind && at(i, &["file"]).as_str().is_some_and(|p| p.ends_with("/e2e/shop.py")), "{service} {source} {function}: {i}");
        anyhow::ensure!(at(i, &["status"]) == "for_review" && at(i, &["regression"]) == false && at(i, &["versions"]) == &json!(["1.0"]), "{i}");
    }
    let total = find(&all, "shop-otel", "span", "total").ok_or_else(|| anyhow::anyhow!("its issue"))?;
    let id = at(total, &["id"]).as_str().unwrap_or("").to_owned();
    anyhow::ensure!(at(total, &["message"]).as_str().is_some_and(|m| m.starts_with("bad item -")), "{total}");

    // Each of the OTel shop's six issues told once as new, with what it is; none of ddtrace's.
    let mut otel_ids: Vec<String> = want.iter().filter(|w| w.0 == "shop-otel").map(|(s, src, func, _)| id_of(&all, s, src, func)).collect::<anyhow::Result<_>>()?;
    otel_ids.sort();
    until("each new issue told", Duration::from_secs(90), || Ok(alerted(&heard, &new).len() >= otel_ids.len()), &|| format!("{:?}\n{}", notices(&heard, &new), f.logs()))?;
    let mut told = alerted(&heard, &new);
    told.sort();
    anyhow::ensure!(told == otel_ids, "told {told:?}, not {otel_ids:?}");
    let said = notices(&heard, &new).into_iter().find(|n| issue_in(n) == id).ok_or_else(|| anyhow::anyhow!("its notice"))?;
    let message = at(&said, &["message"]).as_str().unwrap_or("");
    anyhow::ensure!(
        message.starts_with("New issue ValueError: bad item -") && message.contains("/e2e/shop.py\nTraceback (most recent call last):") && message.contains("in total"),
        "its type, message, place and stack said: {said}"
    );
    anyhow::ensure!(at(&said, &["link"]).as_str().is_some_and(|l| l.ends_with(&format!("/observe/errors/{id}"))), "its page the link: {said}");
    // A monitor of new issues made now tells none of these.
    let again = monitor("A new issue anywhere", r#"error-tracking("*").new().by("issue.id").last("1d") > 0"#, "{{issue.attributes.error.type}} came @webhook-hook")?;
    // One ignored: its alert ends, saying nothing (Datadog's: an ignored issue is muted).
    let load = id_of(&all, "shop-otel", "span", "load")?;
    let r = f.call("PUT", &format!("/api/observe/v1/errors/issues/{load}"), Some(&json!({ "status": "ignored" })))?;
    anyhow::ensure!(at(&r, &["status"]) == "ignored", "{r}");

    // Resolved, its version failing again: still resolved, its errors counted.
    let r = f.call("PUT", &format!("/api/observe/v1/errors/issues/{id}"), Some(&json!({ "status": "resolved" })))?;
    anyhow::ensure!(at(&r, &["status"]) == "resolved", "{r}");
    // Its alert ends, told; the ignored one's too, told nothing, its event saying it was muted.
    until("its alert's end told", Duration::from_secs(60), || Ok(notices(&heard, &new).iter().any(|n| at(n, &["state"]) == "OK" && issue_in(n) == id)), &|| {
        format!("{:?}\n{}", notices(&heard, &new), f.logs())
    })?;
    let ended = || -> anyhow::Result<bool> {
        let m = f.call("GET", &format!("/api/observe/v1/monitors/{new}"), None)?;
        Ok(at(&m, &["groups"]).as_array().is_some_and(|gs| gs.iter().any(|g| issue_in(g) == load && at(g, &["state"]) == "OK")))
    };
    until("the ignored issue's alert ended", Duration::from_secs(60), ended, &|| f.logs())?;
    anyhow::ensure!(!notices(&heard, &new).iter().any(|n| issue_in(n) == load && at(n, &["state"]) != "ALERT"), "an ignored issue's end told: {:?}", notices(&heard, &new));
    let ends = || -> anyhow::Result<Vec<(String, String)>> {
        let events =
            f.call("POST", "/api/observe/v1/logs/search", Some(&json!({ "query": format!("source:kiln.event @monitor_id:{new} @state:OK"), "from": t0, "to": now_ms() + 60_000, "limit": 50 })))?;
        Ok(at(&events, &["records"])
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| (at(r, &["attributes", "group.issue.id"]).as_str().unwrap_or("").to_owned(), at(r, &["attributes", "muted"]).as_str().unwrap_or("").to_owned()))
            .collect())
    };
    until("the ends' events", Duration::from_secs(30), || Ok(ends()?.len() >= 2), &|| f.logs())?;
    let ends = ends()?;
    anyhow::ensure!(
        ends.len() == 2 && ends.contains(&(load.clone(), "true".to_owned())) && ends.contains(&(id.clone(), "false".to_owned())),
        "the ends: the ignored one's muted, the resolved one's told: {ends:?}"
    );
    fail("otel", "total", 100, "1.0", &otel(&ingest))?;
    let now_of = |f: &Fleet| -> anyhow::Result<Value> { f.call("GET", &format!("/api/observe/v1/errors/issues/{id}?from={t0}&to={}", now_ms() + 60_000), None) };
    until("its old version's errors counted", Duration::from_secs(90), || Ok(at(&now_of(&f)?, &["issue", "errors"]).as_f64() == Some(2.0 * EACH as f64)), &|| f.logs())?;
    // The registry reads the metric each ten seconds: give it two.
    std::thread::sleep(Duration::from_secs(25));
    let d = now_of(&f)?;
    anyhow::ensure!(at(&d, &["issue", "status"]) == "resolved" && at(&d, &["issue", "regression"]) == false, "its old version: no regression: {d}");

    // A new version failing so: a regression, back for review.
    fail("otel", "total", 200, "2.0", &otel(&ingest))?;
    until(
        "a regression",
        Duration::from_secs(90),
        || {
            let d = now_of(&f)?;
            Ok(at(&d, &["issue", "status"]) == "for_review" && at(&d, &["issue", "regression"]) == true && at(&d, &["issue", "errors"]).as_f64() == Some(3.0 * EACH as f64))
        },
        &|| f.logs(),
    )?;
    let d = now_of(&f)?;
    anyhow::ensure!(at(&d, &["issue", "versions"]) == &json!(["1.0", "2.0"]), "{d}");
    let versions = at(&d, &["versions"]).as_array().cloned().unwrap_or_default();
    let by = |v: &str| versions.iter().find(|x| at(x, &["version"]) == v).and_then(|x| at(x, &["errors"]).as_f64());
    anyhow::ensure!(by("1.0") == Some(2.0 * EACH as f64) && by("2.0") == Some(EACH as f64), "{versions:?}");
    let timeline: f64 = at(&d, &["timeline", "values"]).as_array().into_iter().flatten().filter_map(Value::as_f64).sum();
    anyhow::ensure!(timeline == 3.0 * EACH as f64, "its timeline: {timeline}");
    let stack = at(&d, &["latest", "attributes", "error.stack"]).as_str().unwrap_or("");
    anyhow::ensure!(stack.starts_with("Traceback (most recent call last):") && stack.contains("in total"), "its latest error, whole: {d}");
    let activity = at(&d, &["issue", "activity"]).as_array().cloned().unwrap_or_default();
    anyhow::ensure!(activity.len() == 3 && activity.last().is_some_and(|a| at(a, &["by"]) == "kiln" && at(a, &["why"]).as_str().is_some_and(|w| w.contains("2.0"))), "{activity:?}");
    // Its errors found by its id.
    let found = f.call("POST", "/api/observe/v1/spans/search", Some(&json!({ "query": format!("@issue.id:{id}"), "from": t0, "to": now_ms() + 60_000, "limit": 1000 })))?;
    let spans = at(&found, &["spans"]).as_array().map_or(0, Vec::len) as u64;
    anyhow::ensure!(spans == 3 * EACH, "{spans} of its spans found by its id");

    // The regression told: as new again by the first monitor; alone by the one made after the
    // issues came; and by the monitor of impact once for review, past nine errors in the hour, as
    // the log's issue of the same place was (never resolved, past them with its old version's).
    until(
        "the regression told",
        Duration::from_secs(90),
        || Ok(alerted(&heard, &new).iter().filter(|i| **i == id).count() == 2 && alerted(&heard, &again) == [id.clone()] && alerted(&heard, &impact).contains(&id)),
        &|| format!("{:?}\n{:?}\n{:?}\n{}", notices(&heard, &new), notices(&heard, &again), notices(&heard, &impact), f.logs()),
    )?;
    // And nothing more, two evaluations on.
    std::thread::sleep(Duration::from_secs(25));
    let (a, b, c) = (notices(&heard, &new), notices(&heard, &impact), notices(&heard, &again));
    anyhow::ensure!(a.len() == otel_ids.len() + 2, "the monitor of new issues: six new, an end, the regression: {a:?}");
    anyhow::ensure!(c.len() == 1, "the monitor made after the issues came: the regression alone: {c:?}");
    let mut past = alerted(&heard, &impact);
    past.sort();
    let mut want_past = vec![id.clone(), id_of(&all, "shop-otel", "log", "total")?];
    want_past.sort();
    anyhow::ensure!(b.len() == 2 && past == want_past && b.iter().all(|n| at(n, &["value"]).as_f64().is_some_and(|v| v >= 10.0)), "the monitor of impact: {b:?}");
    eprintln!(
        "  errors     {} failures of three places, under OpenTelemetry's SDK (spans, logs) and ddtrace: {} issues, each counted whole; resolved, a new version's failure a regression; {} notices of three Error Tracking monitors",
        2 * 3 * EACH,
        want.len(),
        a.len() + b.len() + c.len()
    );
    Ok(())
}

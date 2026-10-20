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
//! latest error whole (its stack), and its errors found by its id.
//!
//! It needs Docker: `cargo test -p kiln --test observe_errors -- --ignored --nocapture`.
#![cfg(unix)]

mod common;

use std::process::Command;
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
    Command::new("chmod").args(["-R", "a+rX"]).arg(dir.path()).output()?;
    let mut args: Vec<String> = ["run", "--rm", "--network", "host", "-v", &format!("{}:/e2e:ro", dir.path().display())].iter().map(|s| (*s).to_owned()).collect();
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
    let otel = |token: &str| vec![("KILN", format!("http://127.0.0.1:{}", f.http)), ("TOKEN", token.to_owned())];
    let dd = vec![
        ("DD_SERVICE", "shop-dd".to_owned()),
        ("DD_AGENT_HOST", "127.0.0.1".to_owned()),
        ("DD_TRACE_AGENT_PORT", apm.to_string()),
        ("DD_ENV", "e2e".to_owned()),
        ("DD_VERSION", "1.0".to_owned()),
        ("DD_REMOTE_CONFIGURATION_ENABLED", "false".to_owned()),
        ("DD_INSTRUMENTATION_TELEMETRY_ENABLED", "false".to_owned()),
    ];
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

    // Resolved, its version failing again: still resolved, its errors counted.
    let r = f.call("PUT", &format!("/api/observe/v1/errors/issues/{id}"), Some(&json!({ "status": "resolved" })))?;
    anyhow::ensure!(at(&r, &["status"]) == "resolved", "{r}");
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
    eprintln!(
        "  errors     {} failures of three places, under OpenTelemetry's SDK (spans, logs) and ddtrace: {} issues, each counted whole; resolved, a new version's failure a regression",
        2 * 3 * EACH,
        want.len()
    );
    Ok(())
}

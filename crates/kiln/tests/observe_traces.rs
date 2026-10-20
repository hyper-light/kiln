//! Traces, live (docs/observe.md §8.7, §13): a kiln server and a runner as processes, and two
//! services instrumented with OpenTelemetry's own Python SDK (Flask's and requests'
//! instrumentation, in Docker): a frontend exporting OTLP/HTTP to the server, a cart service it calls
//! exporting OTLP/gRPC to the runner's receiver, the cart's database call failing one order in ten.
//! What a user relies on: each request a trace of four spans across the two services, found by its
//! id, placed as its waterfall draws it, its error where it happened; the frontend's log of it found
//! by the trace's id; the services' requests, errors and latency, the cart's resources and its calls
//! to its database, from the trace metrics; the map of who calls whom; a span the specification
//! calls invalid refused and said so.
//!
//! It needs Docker: `cargo test -p kiln --test observe_traces -- --ignored --nocapture`.
#![cfg(unix)]

mod common;

use std::process::Command;
use std::time::{Duration, Instant};

use common::{Fleet, at, free_port, now_ms};
use serde_json::{Value, json};

const IMAGE: &str = "kiln-otel-python:e2e";
/// Requests the driver makes: orders 0 to 39, those ending in 3 failing at the cart's database.
const ORDERS: u64 = 40;

const DOCKERFILE: &str = "FROM python:3.12-slim\nRUN pip install --no-cache-dir flask requests opentelemetry-sdk opentelemetry-exporter-otlp-proto-http opentelemetry-exporter-otlp-proto-grpc opentelemetry-instrumentation-flask opentelemetry-instrumentation-requests\n";

const APPS: &str = r#"
import json, logging, os, subprocess, sys, time, urllib.error, urllib.request

def provider(service, exporter):
    from opentelemetry import trace
    from opentelemetry.sdk.resources import Resource
    from opentelemetry.sdk.trace import TracerProvider
    from opentelemetry.sdk.trace.export import BatchSpanProcessor
    res = Resource.create({"service.name": service, "deployment.environment.name": "e2e", "service.version": "1.2.3", "host.name": "kiln-otel-e2e"})
    p = TracerProvider(resource=res)
    p.add_span_processor(BatchSpanProcessor(exporter, schedule_delay_millis=200))
    trace.set_tracer_provider(p)
    return p, res

def frontend():
    from flask import Flask, jsonify
    import requests
    from opentelemetry import trace
    from opentelemetry.exporter.otlp.proto.http.trace_exporter import OTLPSpanExporter
    from opentelemetry.exporter.otlp.proto.http._log_exporter import OTLPLogExporter
    from opentelemetry.instrumentation.flask import FlaskInstrumentor
    from opentelemetry.instrumentation.requests import RequestsInstrumentor
    from opentelemetry._logs import set_logger_provider
    from opentelemetry.sdk._logs import LoggerProvider, LoggingHandler
    from opentelemetry.sdk._logs.export import BatchLogRecordProcessor
    server, headers = os.environ["KILN"], {"Authorization": "Bearer " + os.environ["TOKEN"]}
    p, res = provider("frontend", OTLPSpanExporter(endpoint=server + "/v1/traces", headers=headers))
    lp = LoggerProvider(resource=res)
    lp.add_log_record_processor(BatchLogRecordProcessor(OTLPLogExporter(endpoint=server + "/v1/logs", headers=headers), schedule_delay_millis=200))
    set_logger_provider(lp)
    log = logging.getLogger("frontend")
    log.setLevel(logging.INFO)
    log.addHandler(LoggingHandler(level=logging.INFO, logger_provider=lp))
    app = Flask("frontend")
    FlaskInstrumentor().instrument_app(app, excluded_urls="flush")
    RequestsInstrumentor().instrument()
    cart = "http://127.0.0.1:" + os.environ["CART_PORT"]

    @app.route("/checkout/<int:order>")
    def checkout(order):
        tid = format(trace.get_current_span().get_span_context().trace_id, "032x")
        log.info("checkout %d", order)
        r = requests.get(cart + "/cart/" + str(order))
        return jsonify(trace_id=tid, cart=r.status_code), (200 if r.status_code < 500 else 502)

    @app.route("/flush")
    def flush():
        p.force_flush()
        lp.force_flush()
        return "ok"

    app.run(host="127.0.0.1", port=int(os.environ["FE_PORT"]))

def cart():
    from flask import Flask
    from opentelemetry import trace
    from opentelemetry.exporter.otlp.proto.grpc.trace_exporter import OTLPSpanExporter
    from opentelemetry.instrumentation.flask import FlaskInstrumentor
    p, _ = provider("cart", OTLPSpanExporter(endpoint=os.environ["GRPC"], insecure=True))
    app = Flask("cart")
    FlaskInstrumentor().instrument_app(app, excluded_urls="flush")
    tracer = trace.get_tracer("cart.db")

    @app.route("/cart/<int:order>")
    def get_cart(order):
        with tracer.start_as_current_span("SELECT carts", kind=trace.SpanKind.CLIENT, attributes={"db.system": "postgresql", "db.statement": "SELECT * FROM carts WHERE id = %s"}):
            time.sleep(0.02)
            if order % 10 == 3:
                raise ValueError("cart %d is locked" % order)
        return "ok"

    @app.route("/flush")
    def flush():
        p.force_flush()
        return "ok"

    app.run(host="127.0.0.1", port=int(os.environ["CART_PORT"]))

def get(url):
    try:
        with urllib.request.urlopen(url, timeout=30) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()

def drive():
    fe, ca = "http://127.0.0.1:" + os.environ["FE_PORT"], "http://127.0.0.1:" + os.environ["CART_PORT"]
    for _ in range(300):
        try:
            if get(fe + "/flush")[0] == 200 and get(ca + "/flush")[0] == 200:
                break
        except Exception:
            pass
        time.sleep(0.1)
    ids = {}
    for i in range(int(os.environ["ORDERS"])):
        code, body = get(fe + "/checkout/" + str(i))
        ids[i] = [code, json.loads(body)["trace_id"]]
    get(ca + "/flush")
    get(fe + "/flush")
    print("IDS " + json.dumps(ids), flush=True)

if sys.argv[1] == "all":
    kids = [subprocess.Popen([sys.executable, "-u", __file__, r]) for r in ("frontend", "cart")]
    try:
        drive()
    finally:
        for k in kids:
            k.terminate()
else:
    {"frontend": frontend, "cart": cart}[sys.argv[1]]()
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

fn until(what: &str, within: Duration, mut done: impl FnMut() -> anyhow::Result<bool>, f: &Fleet) -> anyhow::Result<()> {
    let start = Instant::now();
    loop {
        if done()? {
            return Ok(());
        }
        anyhow::ensure!(start.elapsed() < within, "{what}\n{}", f.logs());
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn spans(f: &Fleet, query: &str, from: i64) -> anyhow::Result<Vec<Value>> {
    let found = f.call("POST", "/api/observe/v1/spans/search", Some(&json!({ "query": query, "from": from, "to": now_ms() + 60_000, "limit": 1000 })))?;
    Ok(at(&found, &["spans"]).as_array().cloned().unwrap_or_default())
}

fn attr<'a>(span: &'a Value, key: &str) -> &'a str {
    at(span, &["attributes", key]).as_str().unwrap_or("")
}

#[test]
#[ignore = "needs Docker: runs OpenTelemetry's Python SDK, and a kiln server and runner as processes"]
fn traces_from_opentelemetry_s_sdk_are_kiln_s_traces_services_and_map() -> anyhow::Result<()> {
    let t0 = now_ms() - 60_000;
    let dir = tempfile::tempdir()?;
    docker(&["build", "-q", "-t", IMAGE, "-"], Some(DOCKERFILE))?;
    let versions =
        docker(&["run", "--rm", IMAGE, "sh", "-c", "pip list 2>/dev/null | grep -i -E '^(opentelemetry-(sdk|exporter-otlp-proto-(http|grpc)|instrumentation-(flask|requests))|flask|grpcio) '"], None)?;
    eprintln!("  sdk        {}", versions.split_whitespace().collect::<Vec<_>>().join(" "));
    let mut f = Fleet::new()?;
    let ingest = std::fs::read_to_string(f.root.path().join("server").join("observe").join("ingest.token"))?.trim().to_owned();
    // The runner, its OTLP/gRPC receiver on a port of the test's.
    let grpc = free_port()?;
    f.start_runner_with("web-1", &[("KILN_OTLP_GRPC_ADDR", &format!("127.0.0.1:{grpc}"))])?;
    let runner_http = *f.otlp.get("web-1").ok_or_else(|| anyhow::anyhow!("the runner's port"))?;
    until("the runner's receiver", Duration::from_secs(60), || Ok(common::raw(runner_http, "GET", "/healthz", "", b"").is_ok_and(|(c, _)| c == 200)), &f)?;
    until(
        "the runner replicating",
        Duration::from_secs(60),
        || Ok(at(&f.call("GET", "/api/observe/v1/status", None)?, &["machines"]).as_array().is_some_and(|m| m.iter().any(|m| at(m, &["up"]) == true))),
        &f,
    )?;

    // A span without a trace id: refused, and said so (OTLP's partial success).
    let invalid = {
        use kiln_obs::otlp::pb::Writer;
        let mut w = Writer::default();
        w.message(1, |rs| rs.message(2, |ss| ss.message(2, |s| s.str(5, "no ids"))));
        w.buf
    };
    let (code, body) = common::raw(f.http, "POST", "/v1/traces", &format!("Authorization: Bearer {ingest}\r\nContent-Type: application/x-protobuf\r\n"), &invalid)?;
    anyhow::ensure!(code == 200 && !body.is_empty(), "{code}: the partial success");
    let (code, body) = common::raw(
        f.http,
        "POST",
        "/v1/traces",
        &format!("Authorization: Bearer {ingest}\r\nContent-Type: application/json\r\n"),
        br#"{"resourceSpans":[{"scopeSpans":[{"spans":[{"name":"no ids"}]}]}]}"#,
    )?;
    let said: Value = serde_json::from_slice(&body)?;
    anyhow::ensure!(code == 200 && at(&said, &["partialSuccess", "rejectedSpans"]) == "1", "{code} {said}");

    // The services, and the driver's requests.
    std::fs::write(dir.path().join("apps.py"), APPS)?;
    std::process::Command::new("chmod").args(["-R", "a+rX"]).arg(dir.path()).output()?;
    let (fe, cart) = (free_port()?, free_port()?);
    let out = docker(
        &[
            "run",
            "--rm",
            "--network",
            "host",
            "-v",
            &format!("{}:/e2e:ro", dir.path().display()),
            "-e",
            &format!("KILN=http://127.0.0.1:{}", f.http),
            "-e",
            &format!("TOKEN={ingest}"),
            "-e",
            &format!("GRPC=http://127.0.0.1:{grpc}"),
            "-e",
            &format!("FE_PORT={fe}"),
            "-e",
            &format!("CART_PORT={cart}"),
            "-e",
            &format!("ORDERS={ORDERS}"),
            IMAGE,
            "python",
            "-u",
            "/e2e/apps.py",
            "all",
        ],
        None,
    )?;
    let ids: Value = out.lines().find_map(|l| l.strip_prefix("IDS ")).map(serde_json::from_str).transpose()?.ok_or_else(|| anyhow::anyhow!("the driver's trace ids:\n{out}"))?;
    let failing = |i: u64| i % 10 == 3;
    for i in 0..ORDERS {
        let code = at(&ids, &[&i.to_string()]).get(0).and_then(Value::as_u64);
        anyhow::ensure!(code == Some(if failing(i) { 502 } else { 200 }), "order {i}: {code:?}");
    }
    let trace_of = |i: u64| at(&ids, &[&i.to_string()]).get(1).and_then(Value::as_str).unwrap_or("").to_owned();

    // Every span: the frontend's through the server, the cart's through the runner and the group.
    until("the frontend's spans", Duration::from_secs(60), || Ok(spans(&f, "service:frontend", t0)?.len() == 2 * ORDERS as usize), &f)?;
    until("the cart's spans, by way of the runner", Duration::from_secs(60), || Ok(spans(&f, "service:cart", t0)?.len() == 2 * ORDERS as usize), &f)?;
    // The trace explorer's syntax: the database's failures, each 20 ms or more.
    let db_errors = spans(&f, "service:cart resource_name:\"SELECT carts\" status:error @duration:>=20ms", t0)?;
    anyhow::ensure!(db_errors.len() == (0..ORDERS).filter(|&i| failing(i)).count(), "{} database errors", db_errors.len());

    // A failing order's trace, by its id: four spans across both services, as a waterfall draws them.
    let id = trace_of(13);
    let t = f.call("GET", &format!("/api/observe/v1/traces/{id}"), None)?;
    let placed = at(&t, &["spans"]).as_array().cloned().unwrap_or_default();
    let kinds: Vec<(&str, &str, u64)> = placed.iter().map(|s| (at(s, &["service"]).as_str().unwrap_or(""), attr(s, "span.kind"), at(s, &["depth"]).as_u64().unwrap_or(99))).collect();
    anyhow::ensure!(kinds == vec![("frontend", "server", 0), ("frontend", "client", 1), ("cart", "server", 2), ("cart", "client", 3)], "{kinds:?}");
    anyhow::ensure!(at(&t, &["roots"]) == 1 && at(&t, &["errors"]).as_u64().is_some_and(|e| e >= 3) && at(&t, &["truncated"]) == false, "{t}");
    let db = placed.get(3).ok_or_else(|| anyhow::anyhow!("the database's span"))?;
    anyhow::ensure!(attr(db, "error.type") == "ValueError" && attr(db, "error.message") == "cart 13 is locked" && attr(db, "peer.service") == "postgresql", "{db}");
    let (Some(root), Some(server)) = (placed.first(), placed.get(2)) else { anyhow::bail!("the trace's spans: {t}") };
    anyhow::ensure!(at(db, &["parent"]) == 2 && at(server, &["parent"]) == 1, "each its parent's child");
    anyhow::ensure!(attr(root, "env") == "e2e" && attr(root, "version") == "1.2.3", "the unified service tags");
    let offsets: Vec<i64> = placed.iter().filter_map(|s| at(s, &["offset_ns"]).as_i64()).collect();
    anyhow::ensure!(offsets.first() == Some(&0) && offsets.windows(2).all(|w| matches!(w, [a, b] if a <= b)), "{offsets:?}");
    // The frontend's log of it, found by the trace's id.
    until(
        "the log, by its trace",
        Duration::from_secs(30),
        || {
            let found = f.call("POST", "/api/observe/v1/logs/search", Some(&json!({ "query": format!("trace_id:{id}"), "from": t0, "to": now_ms() + 60_000 })))?;
            Ok(at(&found, &["records"]).as_array().is_some_and(|r| matches!(r.as_slice(), [one] if at(one, &["message"]) == "checkout 13")))
        },
        &f,
    )?;

    // The services, from the trace metrics: counted on the server once their windows settle.
    let range = format!("from={t0}&to={}", now_ms() + 60_000);
    let errors = (0..ORDERS).filter(|&i| failing(i)).count() as f64;
    until(
        "the services' requests and errors",
        Duration::from_secs(90),
        || {
            let s = f.call("GET", &format!("/api/observe/v1/apm/services?{range}"), None)?;
            let row = |name: &str| at(&s, &["services"]).as_array().and_then(|a| a.iter().find(|r| at(r, &["service"]) == name).cloned()).unwrap_or(Value::Null);
            let (fe, ca) = (row("frontend"), row("cart"));
            Ok(at(&fe, &["hits"]).as_f64() == Some(ORDERS as f64)
                && at(&fe, &["errors"]).as_f64() == Some(errors)
                && at(&ca, &["hits"]).as_f64() == Some(ORDERS as f64)
                && at(&ca, &["errors"]).as_f64() == Some(errors))
        },
        &f,
    )?;
    let svc = f.call("GET", &format!("/api/observe/v1/apm/services/cart?{range}"), None)?;
    let p50 = at(&svc, &["summary", "p50_s"]).as_f64().unwrap_or(0.0);
    anyhow::ensure!((0.02..1.0).contains(&p50), "the cart's median, its 20 ms sleep and more: {p50}");
    let resources = at(&svc, &["resources"]).as_array().cloned().unwrap_or_default();
    anyhow::ensure!(matches!(resources.as_slice(), [one] if at(one, &["hits"]).as_f64() == Some(ORDERS as f64)), "{resources:?}");
    let calls = at(&svc, &["calls"]).as_array().cloned().unwrap_or_default();
    anyhow::ensure!(calls.iter().any(|c| at(c, &["peer.service"]) == "postgresql" && at(c, &["hits"]).as_f64() == Some(ORDERS as f64) && at(c, &["errors"]).as_f64() == Some(errors)), "{calls:?}");

    // The map: the frontend calls the cart (its spans answered it), the cart its database (named).
    let m = f.call("GET", &format!("/api/observe/v1/apm/map?{range}"), None)?;
    let edge = |from: &str, to: &str| at(&m, &["edges"]).as_array().and_then(|e| e.iter().find(|x| at(x, &["from"]) == from && at(x, &["to"]) == to).cloned()).unwrap_or(Value::Null);
    let (fc, cd) = (edge("frontend", "cart"), edge("cart", "postgresql"));
    anyhow::ensure!(at(&fc, &["calls"]).as_f64() == Some(ORDERS as f64) && at(&fc, &["inferred"]) == false, "{m}");
    anyhow::ensure!(at(&cd, &["calls"]).as_f64() == Some(ORDERS as f64) && at(&cd, &["errors"]).as_f64() == Some(errors) && at(&cd, &["inferred"]) == true, "{m}");
    eprintln!("  traces     {ORDERS} requests, 4 spans each across two services (OTLP/HTTP and OTLP/gRPC), traces by id, logs by trace, services, resources, calls and the map: all in kiln");
    Ok(())
}

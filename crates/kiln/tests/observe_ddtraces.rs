//! Datadog's traces, live (docs/observe.md §8.6, §8.7, §13): two services traced by Datadog's own
//! Python tracer (ddtrace 4.16, Flask's and requests' integrations): a shop that calls a cart, the
//! cart's database span failing one order in five. The shop keeps the trace of every even order and
//! drops every odd one, as a sampling rule does (`manual.keep`, `manual.drop`). Their tracers send
//! to Datadog's Agent itself (7.84, in Docker) with its trace agent pointed at a kiln server
//! (`DD_APM_DD_URL`); or straight to a kiln runner's receiver, on the Agent's port, no Agent
//! anywhere.
//!
//! What a user relies on, either way: the traces kept are in kiln whole, across both services,
//! found by their ids, placed as their waterfall draws them, their errors where they happened;
//! those dropped are not; and the services' requests, errors and latency are of every request, kept
//! or not (the Agent's stats, or the runner's own count, as Datadog counts them), none counted
//! twice, each series of the host that counted it.
//!
//! It needs Docker: `cargo test -p kiln --test observe_ddtraces -- --ignored --nocapture`.
#![cfg(unix)]

mod common;

use std::process::Command;
use std::time::{Duration, Instant};

use common::{Fleet, at, free_port, now_ms};
use serde_json::{Value, json};

const AGENT: &str = "gcr.io/datadoghq/agent:7.84.4";
const IMAGE: &str = "kiln-ddtrace-python:e2e";
const HOST: &str = "kiln-apm-e2e";
/// Requests the driver makes: orders 0 to 39, those ending in 4 or 9 failing at the cart's
/// database; the even ones' traces kept.
const ORDERS: u64 = 40;

const DOCKERFILE: &str = "FROM python:3.12-slim\nRUN pip install --no-cache-dir ddtrace==4.16.0 flask==3.1.3 requests\n";

const APPS: &str = r#"
import http.server, json, os, socket, subprocess, sys, threading, time, urllib.error, urllib.request

# The services are traced, the driver not (its calls are not the shop's callers).
if sys.argv[1] != "all":
    import ddtrace.auto  # Flask's and requests' integrations, before either is imported
    try:
        from ddtrace.trace import tracer
    except ImportError:
        from ddtrace import tracer
    from ddtrace.constants import MANUAL_DROP_KEY, MANUAL_KEEP_KEY

def control(port, app_port):
    # Untraced (ddtrace patches no http.server): whether the app listens, and its tracer flushed.
    class H(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            ok = True
            if self.path == "/ready":
                try:
                    socket.create_connection(("127.0.0.1", app_port), timeout=1).close()
                except OSError:
                    ok = False
            elif self.path == "/flush":
                tracer.flush()
            self.send_response(200 if ok else 503)
            self.send_header("Content-Length", "0")
            self.end_headers()
        def log_message(self, *a):
            pass
    s = http.server.ThreadingHTTPServer(("127.0.0.1", port), H)
    threading.Thread(target=s.serve_forever, daemon=True).start()

def shop():
    from flask import Flask, jsonify
    import requests
    app = Flask("shop")
    cart = "http://127.0.0.1:" + os.environ["CART_PORT"]

    @app.route("/checkout/<int:order>")
    def checkout(order):
        root = tracer.current_root_span()
        root.set_tag(MANUAL_DROP_KEY if order % 2 else MANUAL_KEEP_KEY)
        r = requests.get(cart + "/cart/" + str(order))
        return jsonify(trace_id=format(root.trace_id, "032x"), cart=r.status_code), (200 if r.status_code < 500 else 502)

    control(int(os.environ["SHOP_CTL"]), int(os.environ["SHOP_PORT"]))
    app.run(host="127.0.0.1", port=int(os.environ["SHOP_PORT"]))

def cart():
    from flask import Flask
    app = Flask("cart")

    @app.route("/cart/<int:order>")
    def get_cart(order):
        with tracer.trace("postgres.query", service="cart-db", resource="SELECT carts", span_type="sql") as s:
            s.set_tag("db.system", "postgresql")
            time.sleep(0.02)
            if order % 5 == 4:
                raise ValueError("cart %d is locked" % order)
        return "ok"

    control(int(os.environ["CART_CTL"]), int(os.environ["CART_PORT"]))
    app.run(host="127.0.0.1", port=int(os.environ["CART_PORT"]))

def get(url):
    try:
        with urllib.request.urlopen(url, timeout=30) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()

def drive():
    url = lambda p: "http://127.0.0.1:" + os.environ[p]
    shop, shop_ctl, cart_ctl, agent = url("SHOP_PORT"), url("SHOP_CTL"), url("CART_CTL"), url("APM_PORT")
    for _ in range(600):
        try:
            if get(agent + "/info")[0] == 200 and get(shop_ctl + "/ready")[0] == 200 and get(cart_ctl + "/ready")[0] == 200:
                break
        except Exception:
            pass
        time.sleep(0.1)
    ids = {}
    for i in range(int(os.environ["ORDERS"])):
        code, body = get(shop + "/checkout/" + str(i))
        ids[i] = [code, json.loads(body)["trace_id"]]
    get(cart_ctl + "/flush")
    get(shop_ctl + "/flush")
    time.sleep(2)
    print("IDS " + json.dumps(ids), flush=True)

if sys.argv[1] == "all":
    kids = [subprocess.Popen([sys.executable, "-u", __file__, r], env=dict(os.environ, DD_SERVICE=r)) for r in ("shop", "cart")]
    try:
        drive()
    finally:
        for k in kids:
            k.terminate()
else:
    {"shop": shop, "cart": cart}[sys.argv[1]]()
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

/// The Agent in a container on the host's network, its trace agent's receiver on `apm`.
struct Agent {
    name: String,
}

impl Agent {
    fn start(http: u16, key: &str, apm: u16) -> anyhow::Result<Self> {
        let name = format!("kiln-ddapm-{}-{http}", std::process::id());
        let kiln = format!("http://127.0.0.1:{http}");
        let env = [
            ("DD_API_KEY", key.to_owned()),
            ("DD_DD_URL", kiln.clone()),
            ("DD_HOSTNAME", HOST.to_owned()),
            ("DD_APM_ENABLED", "true".to_owned()),
            ("DD_APM_DD_URL", kiln),
            ("DD_APM_RECEIVER_PORT", apm.to_string()),
            ("DD_APM_DEBUG_PORT", free_port()?.to_string()),
            ("DD_DOGSTATSD_PORT", free_port()?.to_string()),
            ("DD_CMD_PORT", free_port()?.to_string()),
            ("DD_EXPVAR_PORT", free_port()?.to_string()),
            ("DD_LOGS_ENABLED", "false".to_owned()),
            ("DD_PROCESS_AGENT_ENABLED", "false".to_owned()),
            ("DD_INVENTORIES_ENABLED", "false".to_owned()),
            ("DD_REMOTE_CONFIGURATION_ENABLED", "false".to_owned()),
        ];
        let mut c = Command::new("docker");
        c.args(["run", "-d", "--rm", "--name", &name, "--network", "host"]);
        for (k, v) in &env {
            c.args(["-e", &format!("{k}={v}")]);
        }
        let out = c.arg(AGENT).output()?;
        anyhow::ensure!(out.status.success(), "docker run: {}", String::from_utf8_lossy(&out.stderr));
        Ok(Self { name })
    }

    fn logs(&self) -> String {
        Command::new("docker").args(["logs", "--tail", "40", &self.name]).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr)).unwrap_or_default()
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.name]).output();
    }
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

fn spans(f: &Fleet, query: &str, from: i64) -> anyhow::Result<Vec<Value>> {
    let found = f.call("POST", "/api/observe/v1/spans/search", Some(&json!({ "query": query, "from": from, "to": now_ms() + 60_000, "limit": 1000 })))?;
    Ok(at(&found, &["spans"]).as_array().cloned().unwrap_or_default())
}

fn attr<'a>(span: &'a Value, key: &str) -> &'a str {
    at(span, &["attributes", key]).as_str().unwrap_or("")
}

/// The tracer's image built, its versions said.
fn image() -> anyhow::Result<()> {
    docker(&["build", "-q", "-t", IMAGE, "-"], Some(DOCKERFILE))?;
    let versions = docker(&["run", "--rm", IMAGE, "sh", "-c", "pip list 2>/dev/null | grep -i -E '^(ddtrace|flask|requests) '"], None)?;
    eprintln!("  tracer     {}", versions.split_whitespace().collect::<Vec<_>>().join(" "));
    Ok(())
}

/// The services and the driver's orders through them, their tracers sending to `apm` (an Agent's
/// receiver, or kiln's): each order's answer and trace id.
fn orders(apm: u16) -> anyhow::Result<Value> {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("apps.py"), APPS)?;
    Command::new("chmod").args(["-R", "a+rX"]).arg(dir.path()).output()?;
    let (shop, cart, shop_ctl, cart_ctl) = (free_port()?, free_port()?, free_port()?, free_port()?);
    let mut args: Vec<String> = ["run", "--rm", "--network", "host", "-v", &format!("{}:/e2e:ro", dir.path().display())].iter().map(|s| (*s).to_owned()).collect();
    for (k, v) in [
        ("SHOP_PORT", shop.to_string()),
        ("CART_PORT", cart.to_string()),
        ("SHOP_CTL", shop_ctl.to_string()),
        ("CART_CTL", cart_ctl.to_string()),
        ("APM_PORT", apm.to_string()),
        ("ORDERS", ORDERS.to_string()),
        ("DD_AGENT_HOST", "127.0.0.1".to_owned()),
        ("DD_TRACE_AGENT_PORT", apm.to_string()),
        ("DD_ENV", "e2e".to_owned()),
        ("DD_VERSION", "1.2.3".to_owned()),
        ("DD_REMOTE_CONFIGURATION_ENABLED", "false".to_owned()),
        ("DD_INSTRUMENTATION_TELEMETRY_ENABLED", "false".to_owned()),
    ] {
        args.push("-e".to_owned());
        args.push(format!("{k}={v}"));
    }
    args.extend([IMAGE, "python", "-u", "/e2e/apps.py", "all"].map(str::to_owned));
    let out = docker(&args.iter().map(String::as_str).collect::<Vec<_>>(), None)?;
    let ids: Value = out.lines().find_map(|l| l.strip_prefix("IDS ")).map(serde_json::from_str).transpose()?.ok_or_else(|| anyhow::anyhow!("the driver's trace ids:\n{out}"))?;
    for i in 0..ORDERS {
        let code = at(&ids, &[&i.to_string()]).get(0).and_then(Value::as_u64);
        anyhow::ensure!(code == Some(if failing(i) { 502 } else { 200 }), "order {i}: {code:?}");
    }
    Ok(ids)
}

fn failing(i: u64) -> bool {
    i % 5 == 4
}

fn kept(i: u64) -> bool {
    i.is_multiple_of(2)
}

/// What a user relies on of the orders' traces, wherever they came in: the kept whole and the
/// dropped not, a trace by its id as its waterfall draws it, every request counted once (`host` the
/// series' and the spans' host: the Agent's, or the kiln runner's that was the Agent).
fn the_traces_and_their_metrics(f: &Fleet, t0: i64, ids: &Value, host: &str, logs: &dyn Fn() -> String) -> anyhow::Result<f64> {
    let trace_of = |i: u64| at(ids, &[&i.to_string()]).get(1).and_then(Value::as_str).unwrap_or("").to_owned();
    // The kept traces, whole: each of the shop's, the cart's and its database's spans.
    let traces = |service: &str| -> anyhow::Result<std::collections::BTreeSet<String>> {
        Ok(spans(f, &format!("service:{service} source:datadog-agent"), t0)?.iter().filter_map(|s| at(s, &["trace_id"]).as_str().map(str::to_owned)).collect())
    };
    let want: std::collections::BTreeSet<String> = (0..ORDERS).filter(|&i| kept(i)).map(trace_of).collect();
    until("the kept traces", Duration::from_secs(120), || Ok(traces("shop")? == want && traces("cart")? == want && traces("cart-db")? == want), logs)?;
    // The dropped ones are not.
    let dropped = trace_of(9);
    anyhow::ensure!(spans(f, &format!("trace_id:{dropped}"), t0)?.is_empty(), "a dropped trace is not kept");
    // The trace explorer's syntax on them: the database's kept failures, each 20 ms or more.
    let db_errors = spans(f, "service:cart-db resource_name:\"SELECT carts\" status:error @duration:>=20ms", t0)?;
    anyhow::ensure!(db_errors.len() == (0..ORDERS).filter(|&i| failing(i) && kept(i)).count(), "{} database errors", db_errors.len());

    // A kept failing order's trace, by its id: the shop's request, its call, the cart's request, the
    // database's span that failed, each its caller's descendant, as a waterfall draws them.
    let id = trace_of(14);
    let t = f.call("GET", &format!("/api/observe/v1/traces/{id}"), None)?;
    let placed = at(&t, &["spans"]).as_array().cloned().unwrap_or_default();
    let nth = |i: usize| placed.get(i).cloned().unwrap_or(Value::Null);
    let find = |service: &str, op: &str| placed.iter().position(|s| at(s, &["service"]) == service && attr(s, "operation_name") == op);
    let (Some(root), Some(call), Some(served), Some(db)) = (
        find("shop", "flask.request"),
        placed.iter().position(|s| attr(s, "span.kind") == "client" && attr(s, "operation_name") == "requests.request"),
        find("cart", "flask.request"),
        find("cart-db", "postgres.query"),
    ) else {
        anyhow::bail!("the trace's spans: {t}")
    };
    anyhow::ensure!(root == 0 && at(&nth(root), &["depth"]) == 0 && attr(&nth(root), "span.kind") == "server", "{t}");
    anyhow::ensure!(root < call && call < served && served < db, "placed as drawn: {root} {call} {served} {db}");
    let ancestors = |mut i: usize| {
        let mut up = Vec::new();
        while let Some(p) = at(&nth(i), &["parent"]).as_u64() {
            i = p as usize;
            up.push(i);
        }
        up
    };
    anyhow::ensure!(ancestors(db).contains(&served) && ancestors(served).contains(&call) && ancestors(call).contains(&root), "each its caller's descendant: {t}");
    let failed = nth(db);
    anyhow::ensure!(
        at(&failed, &["status"]) == "error" && attr(&failed, "error.message") == "cart 14 is locked" && attr(&failed, "error.type") == "builtins.ValueError" && attr(&failed, "span.type") == "sql",
        "{failed}"
    );
    let first = nth(root);
    anyhow::ensure!(attr(&first, "env") == "e2e" && attr(&first, "version") == "1.2.3" && at(&first, &["host"]) == host, "the unified service tags, the Agent's host: {first}");
    anyhow::ensure!(at(&t, &["roots"]) == 1 && at(&t, &["truncated"]) == false, "{t}");

    // The services, from the stats: every request, kept or dropped, once.
    let range = format!("from={t0}&to={}", now_ms() + 120_000);
    let errors = (0..ORDERS).filter(|&i| failing(i)).count() as f64;
    let resource = |svc: &Value, name: &str| at(svc, &["resources"]).as_array().and_then(|a| a.iter().find(|r| at(r, &["resource_name"]) == name).cloned()).unwrap_or(Value::Null);
    until(
        "the services' requests and errors, from the stats",
        Duration::from_secs(120),
        || {
            // A service none of whose stats have come yet is not found.
            let (Ok(shop), Ok(cart)) = (f.call("GET", &format!("/api/observe/v1/apm/services/shop?{range}"), None), f.call("GET", &format!("/api/observe/v1/apm/services/cart?{range}"), None)) else {
                return Ok(false);
            };
            let (s, c) = (resource(&shop, "GET /checkout/<int:order>"), resource(&cart, "GET /cart/<int:order>"));
            Ok(at(&s, &["hits"]).as_f64() == Some(ORDERS as f64)
                && at(&s, &["errors"]).as_f64() == Some(errors)
                && at(&c, &["hits"]).as_f64() == Some(ORDERS as f64)
                && at(&c, &["errors"]).as_f64() == Some(errors))
        },
        logs,
    )?;
    // The database, a service of its tracer's naming: its spans top-level, so its own requests.
    let db_svc = f.call("GET", &format!("/api/observe/v1/apm/services/cart-db?{range}"), None)?;
    let selects = resource(&db_svc, "SELECT carts");
    anyhow::ensure!(at(&selects, &["hits"]).as_f64() == Some(ORDERS as f64) && at(&selects, &["errors"]).as_f64() == Some(errors), "{db_svc}");
    let p50 = at(&db_svc, &["summary", "p50_s"]).as_f64().unwrap_or(0.0);
    anyhow::ensure!((0.02..1.0).contains(&p50), "the database's median, its 20 ms sleep and more: {p50}");
    let list = f.call("GET", &format!("/api/observe/v1/apm/services?{range}"), None)?;
    let listed: Vec<&str> = at(&list, &["services"]).as_array().into_iter().flatten().filter_map(|r| at(r, &["service"]).as_str()).collect();
    anyhow::ensure!(["shop", "cart", "cart-db"].iter().all(|s| listed.contains(s)), "{listed:?}");
    // The series of the host the stats came from.
    let by_host = f.call("POST", "/api/observe/v1/query", Some(&json!({ "query": "sum:trace.hits{service:cart-db} by {host}.as_count()", "from": t0, "to": now_ms() + 120_000 })))?;
    let hosts: Vec<&str> = at(&by_host, &["series"]).as_array().into_iter().flatten().filter_map(|s| at(s, &["tags", "host"]).as_str()).collect();
    anyhow::ensure!(hosts == vec![host], "{by_host}");

    // The map: who calls whom from the spans kept (the shop by way of its requests integration,
    // which ddtrace names a service of its own, as Datadog's map shows it), each service's requests
    // and errors from its stats, every request.
    let m = f.call("GET", &format!("/api/observe/v1/apm/map?{range}"), None)?;
    let edge = |from: &str, to: &str| at(&m, &["edges"]).as_array().and_then(|e| e.iter().find(|x| at(x, &["from"]) == from && at(x, &["to"]) == to).cloned()).unwrap_or(Value::Null);
    let node = |name: &str| at(&m, &["nodes"]).as_array().and_then(|e| e.iter().find(|x| at(x, &["service"]) == name).cloned()).unwrap_or(Value::Null);
    let kept_n = (0..ORDERS).filter(|&i| kept(i)).count() as f64;
    // Every call, kept or dropped: each edge's share of the trace metrics that count its calls (the
    // requests integration's calls out, the database's spans its entries), not the kept traces'.
    for (from, to) in [("shop", "requests"), ("requests", "cart"), ("cart", "cart-db")] {
        let e = edge(from, to);
        anyhow::ensure!(at(&e, &["calls"]).as_f64() == Some(ORDERS as f64) && at(&e, &["counted"]) == true, "{from} -> {to}: {m}");
    }
    anyhow::ensure!(at(&edge("cart", "cart-db"), &["errors"]).as_f64() == Some(errors), "the database's failures, every one: {m}");
    for name in ["shop", "cart", "cart-db"] {
        anyhow::ensure!(at(&node(name), &["hits"]).as_f64() == Some(ORDERS as f64) && at(&node(name), &["errors"]).as_f64() == Some(errors), "{name}: {m}");
    }
    Ok(kept_n)
}

#[test]
#[ignore = "needs Docker: runs Datadog's Agent and its Python tracer, and a kiln server as a process"]
fn datadog_s_tracer_through_its_agent_is_kiln_s_traces_and_its_stats_kiln_s_trace_metrics() -> anyhow::Result<()> {
    let t0 = now_ms() - 60_000;
    image()?;
    let f = Fleet::new()?;
    let ingest = std::fs::read_to_string(f.root.path().join("server").join("observe").join("ingest.token"))?.trim().to_owned();
    let apm = free_port()?;
    let agent = Agent::start(f.http, &ingest, apm)?;
    let ids = orders(apm)?;
    let kept_n = the_traces_and_their_metrics(&f, t0, &ids, HOST, &|| format!("--- agent\n{}\n{}", agent.logs(), f.logs()))?;
    eprintln!("  ddtrace    {ORDERS} requests across two services through the Agent: the {kept_n} traces it kept, whole; every request counted from its stats, once");
    Ok(())
}

#[test]
#[ignore = "needs Docker: runs Datadog's Python tracer, and a kiln server and runner as processes"]
fn datadog_s_tracer_straight_to_a_kiln_runner_is_kiln_s_traces_and_their_trace_metrics() -> anyhow::Result<()> {
    let t0 = now_ms() - 60_000;
    image()?;
    let mut f = Fleet::new()?;
    // The runner, its trace agent's receiver on a port of the test's: no Agent anywhere.
    let apm = free_port()?;
    f.start_runner_with("web-1", &[("KILN_DD_TRACE_ADDR", &format!("127.0.0.1:{apm}"))])?;
    until("the runner's receiver", Duration::from_secs(60), || Ok(common::raw(apm, "GET", "/info", "", b"").is_ok_and(|(c, _)| c == 200)), &|| f.logs())?;
    until(
        "the runner replicating",
        Duration::from_secs(60),
        || Ok(at(&f.call("GET", "/api/observe/v1/status", None)?, &["machines"]).as_array().is_some_and(|m| m.iter().any(|m| at(m, &["up"]) == true))),
        &|| f.logs(),
    )?;
    let (code, info) = common::raw(apm, "GET", "/info", "", b"")?;
    let info: Value = serde_json::from_slice(&info)?;
    anyhow::ensure!(code == 200 && at(&info, &["endpoints"]) == &json!(["/v0.4/traces", "/v0.5/traces"]) && at(&info, &["client_drop_p0s"]) == false, "{info}");
    let ids = orders(apm)?;
    let kept_n = the_traces_and_their_metrics(&f, t0, &ids, "web-1", &|| f.logs())?;
    eprintln!("  ddtrace    {ORDERS} requests across two services straight to a kiln runner: the {kept_n} traces their tracers kept, whole; every request counted by the runner, once");
    Ok(())
}

/// A tracer's `/v0.4/traces` payload of one trace of `service`: its request's span and a call out
/// to its database (`peer.service` `<service>-db`), the request's sampling priority as its tracer
/// decided it.
fn v04_trace(service: &str, trace: u64, start_ns: i64, priority: f64) -> Vec<u8> {
    fn s(out: &mut Vec<u8>, v: &str) {
        out.push(0xd9);
        out.push(v.len() as u8);
        out.extend_from_slice(v.as_bytes());
    }
    fn u(out: &mut Vec<u8>, v: u64) {
        out.push(0xcf);
        out.extend_from_slice(&v.to_be_bytes());
    }
    let db = format!("{service}-db");
    let mut b = vec![0x91, 0x92];
    for (span, parent, kind, name) in [(1u64, 0u64, "server", "GET /a"), (2, 1, "client", "GET")] {
        b.push(0x8a);
        for (k, v) in [("service", service), ("name", "web.request"), ("resource", name)] {
            s(&mut b, k);
            s(&mut b, v);
        }
        s(&mut b, "trace_id");
        u(&mut b, trace);
        s(&mut b, "span_id");
        u(&mut b, trace.wrapping_mul(4) + span);
        s(&mut b, "parent_id");
        u(&mut b, if parent == 0 { 0 } else { trace.wrapping_mul(4) + parent });
        s(&mut b, "start");
        u(&mut b, start_ns as u64);
        s(&mut b, "duration");
        u(&mut b, 2_000_000);
        s(&mut b, "meta");
        b.push(if parent == 0 { 0x82 } else { 0x83 });
        s(&mut b, "span.kind");
        s(&mut b, kind);
        s(&mut b, "env");
        s(&mut b, "sampled");
        if parent != 0 {
            s(&mut b, "peer.service");
            s(&mut b, &db);
        }
        s(&mut b, "metrics");
        if parent == 0 {
            b.push(0x81);
            s(&mut b, "_sampling_priority_v1");
            b.push(0xcb);
            b.extend_from_slice(&priority.to_be_bytes());
        } else {
            b.push(0x80);
        }
    }
    b
}

/// Sends `body` to a runner's receiver as a tracer does, sending it again while the runner is
/// starting (503) or behind on its writes (429: nothing of it taken), for half a minute at the
/// most; the runner's answer.
fn put_traces(apm: u16, body: &[u8]) -> anyhow::Result<Value> {
    let start = Instant::now();
    loop {
        let (code, answer) = common::raw(apm, "PUT", "/v0.4/traces", "Content-Type: application/msgpack\r\nDatadog-Meta-Lang: rust\r\n", body)?;
        if code == 200 {
            return Ok(serde_json::from_slice(&answer)?);
        }
        anyhow::ensure!(matches!(code, 429 | 503) && start.elapsed() < Duration::from_secs(30), "{code}: {}", String::from_utf8_lossy(&answer));
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The spans `q` finds from `from` to `to`, counted.
fn span_count(f: &Fleet, q: &str, from: i64, to: i64) -> anyhow::Result<u64> {
    let a = f.call("POST", "/api/observe/v1/spans/aggregate", Some(&json!({ "query": q, "from": from, "to": to, "measure": "count" })))?;
    Ok(at(&a, &["groups"]).as_array().into_iter().flatten().filter_map(|g| at(g, &["value"]).as_f64()).sum::<f64>().round() as u64)
}

/// Every value of every series `q` gives from `from` to `to`, in steps of `step`, summed.
fn metric_sum(f: &Fleet, q: &str, from: i64, to: i64, step: i64) -> anyhow::Result<f64> {
    let a = f.call("POST", "/api/observe/v1/query", Some(&json!({ "query": q, "from": from, "to": to, "step": step })))?;
    Ok(at(&a, &["series"]).as_array().into_iter().flatten().flat_map(|s| at(s, &["values"]).as_array().cloned().unwrap_or_default()).filter_map(|v| v.as_f64()).sum())
}

/// A runner of the fleet whose receiver takes tracers' traces on `apm`, with `env` besides,
/// started; [`traced`] waits for it.
fn start_traced(f: &mut Fleet, name: &str, apm: u16, env: &[(&str, &str)]) -> anyhow::Result<()> {
    let addr = format!("127.0.0.1:{apm}");
    let mut all = vec![("KILN_DD_TRACE_ADDR", addr.as_str())];
    all.extend_from_slice(env);
    f.start_runner_with(name, &all)
}

/// Waits for runner `name`'s receiver on `apm` to answer, and for the runner to replicate.
fn traced(f: &Fleet, name: &str, apm: u16) -> anyhow::Result<()> {
    until("the runner's receiver", Duration::from_secs(60), || Ok(common::raw(apm, "GET", "/info", "", b"").is_ok_and(|(c, _)| c == 200)), &|| f.logs())?;
    until(
        "the runner replicating",
        Duration::from_secs(60),
        || Ok(at(&f.call("GET", "/api/observe/v1/status", None)?, &["machines"]).as_array().is_some_and(|m| m.iter().any(|m| at(m, &["machine"]) == name && at(m, &["up"]) == true))),
        &|| f.logs(),
    )
}

/// Waits until a trace sent to runner `apm`'s receiver is stored on the server (its service
/// `probe`): the runner's way to the server writes. A runner's first link over the HTTP tunnel (UDP
/// to the server dropped here) can stall some fifteen seconds and be made again; what a runner takes
/// as it stops is written only while it reaches the server (three seconds at the most).
fn writing(f: &Fleet, apm: u16, probe: &str) -> anyhow::Result<()> {
    put_traces(apm, &v04_trace(probe, 0x5eed_0000_0000_0001u64.wrapping_add(probe.len() as u64), now_ms() * 1_000_000, 1.0))?;
    let (from, to) = (now_ms() - 600_000, now_ms() + 600_000);
    until("the runner's writes reaching the server", Duration::from_secs(90), || Ok(span_count(f, &format!("service:{probe}"), from, to)? >= 2), &|| f.logs())
}

/// [`start_traced`], then [`traced`].
fn traced_runner(f: &mut Fleet, name: &str, apm: u16, env: &[(&str, &str)]) -> anyhow::Result<()> {
    start_traced(f, name, apm, env)?;
    traced(f, name, apm)
}

/// A tracer that keeps what kiln's sampler says (ddtrace's rule: a trace kept when its id times
/// Knuth's factor, modulo 2^64, is below its rate's share of 2^64), sending straight to a runner whose
/// target is five traces a second: its service's rate falls to its share; the traces it keeps are
/// stored, the ones it drops are not; every one is counted by the trace metrics.
#[test]
#[ignore = "starts a kiln server and a runner as processes, and sends for half a minute"]
fn a_tracer_straight_to_kiln_keeps_its_share_and_every_trace_is_counted() -> anyhow::Result<()> {
    let t0 = now_ms();
    let mut f = Fleet::new()?;
    let apm = free_port()?;
    traced_runner(&mut f, "web-1", apm, &[("KILN_DD_TARGET_TPS", "5")])?;
    let (mut rate, mut sent, mut kept, mut late_sent, mut late_kept) = (1.0f64, 0u64, Vec::new(), 0u64, 0u64);
    let started = Instant::now();
    let mut t = 0u64;
    while started.elapsed() < Duration::from_secs(26) {
        t += 1;
        let trace = 0x9e37_79b9_7f4a_7c15u64.wrapping_mul(t);
        let keep = trace.wrapping_mul(1_111_111_111_111_111_111) < (rate * u64::MAX as f64) as u64;
        let rates = put_traces(apm, &v04_trace("busy", trace, now_ms() * 1_000_000, if keep { 1.0 } else { 0.0 }))?;
        if let Some(r) = at(&rates, &["rate_by_service", "service:busy,env:sampled"]).as_f64() {
            rate = r;
        }
        sent += 1;
        if keep {
            kept.push(trace);
        }
        if started.elapsed() > Duration::from_secs(16) {
            late_sent += 1;
            late_kept += u64::from(keep);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // Some fifty a second against five: its share a tenth, once the window has seen it.
    anyhow::ensure!((0.05..0.2).contains(&rate), "the rate it was given: {rate}");
    let share = late_kept as f64 / late_sent.max(1) as f64;
    anyhow::ensure!((0.03..0.3).contains(&share), "kept at the end: {late_kept} of {late_sent}");
    // Stored: the traces it kept, whole, and none of those it dropped.
    let (from, to) = (t0 - 60_000, now_ms() + 60_000);
    until("the kept traces stored", Duration::from_secs(60), || Ok(span_count(&f, "service:busy", from, to)? >= 2 * kept.len() as u64), &|| f.logs())?;
    let stored = span_count(&f, "service:busy", from, to)?;
    anyhow::ensure!(stored == 2 * kept.len() as u64, "{stored} spans stored of {} kept traces", kept.len());
    // Counted: every trace sent, kept or dropped, its request an entry and its call an exit.
    let last = std::cell::Cell::new((0.0, 0.0));
    until(
        "every trace counted",
        Duration::from_secs(90),
        || {
            let got = (metric_sum(&f, "sum:trace.hits{service:busy}.as_count()", from, to, 10_000)?, metric_sum(&f, "sum:trace.outbound.hits{service:busy}.as_count()", from, to, 10_000)?);
            last.set(got);
            Ok(got == (sent as f64, sent as f64))
        },
        &|| format!("{sent} sent; trace.hits and trace.outbound.hits {:?}\n{}", last.get(), f.logs()),
    )?;
    // The map: its calls to its database every one sent, from the trace metrics, though a tenth of
    // its traces are kept.
    let m = f.call("GET", &format!("/api/observe/v1/apm/map?from={from}&to={to}"), None)?;
    let e = at(&m, &["edges"]).as_array().and_then(|e| e.iter().find(|x| at(x, &["from"]) == "busy" && at(x, &["to"]) == "busy-db").cloned()).unwrap_or(Value::Null);
    anyhow::ensure!(at(&e, &["calls"]).as_f64() == Some(sent as f64) && at(&e, &["counted"]) == true && at(&e, &["inferred"]) == true, "busy -> busy-db: {e}\n{m}");
    eprintln!("  sampled    {sent} traces sent, {} kept ({:.0}% at the end, its rate {rate:.3}); every one counted, on the map too", kept.len(), 100.0 * share);
    Ok(())
}

/// A runner stopped (as an operator's stop, or an upgrade's) while its tracer sends, and started
/// again: what it took before it stopped is written as it stops, its spans and the buckets it was
/// still counting, and what it counts once back is written beside what it wrote, not over it. Each
/// half's spans end some forty seconds ahead (taken as they say, within a minute), so the bucket
/// they are counted in is open across the stop and both of the runner's lives write it. As it
/// starts again its port is held a moment (as its last build holds it until it retires, after an
/// upgrade): its receiver takes the port once it is let go.
#[test]
#[ignore = "starts a kiln server and a runner as processes, and the runner again"]
fn a_runner_stopped_and_started_again_counts_every_trace_once() -> anyhow::Result<()> {
    const HALF: u64 = 200;
    let t0 = now_ms();
    let mut f = Fleet::new()?;
    let apm = free_port()?;
    let keep_all = [("KILN_DD_TARGET_TPS", "0")];
    traced_runner(&mut f, "web-1", apm, &keep_all)?;
    writing(&f, apm, "probe-first")?;
    let bucket = (now_ms() + 40_000) / 10_000 * 10_000;
    let send = |from: u64| -> anyhow::Result<()> {
        for t in from..from + HALF {
            put_traces(apm, &v04_trace("restarted", 0x9e37_79b9_7f4a_7c15u64.wrapping_mul(t + 1), (bucket + 1_000 + t as i64) * 1_000_000, 1.0))?;
        }
        Ok(())
    };
    send(0)?;
    let stopping = Instant::now();
    f.stop_runner("web-1");
    let stopped_in = stopping.elapsed();
    let held = std::net::TcpListener::bind(("127.0.0.1", apm))?;
    start_traced(&mut f, "web-1", apm, &keep_all)?;
    until("the runner finding its port held", Duration::from_secs(60), || Ok(f.logs().contains("the receiver's port is held")), &|| f.logs())?;
    drop(held);
    traced(&f, "web-1", apm)?;
    writing(&f, apm, "probe-again")?;
    // The bucket is written twenty seconds past its end: the runner must be back before then.
    let back = now_ms();
    anyhow::ensure!(back < bucket + 25_000, "the runner was back {} s after the bucket began: too late for this scenario", (back - bucket) / 1000);
    send(HALF)?;
    let (from, to) = (t0 - 60_000, now_ms() + 120_000);
    until("every span stored", Duration::from_secs(60), || Ok(span_count(&f, "service:restarted", from, to)? >= 4 * HALF), &|| f.logs())?;
    let stored = span_count(&f, "service:restarted", from, to)?;
    anyhow::ensure!(stored == 4 * HALF, "{stored} spans stored of {}", 4 * HALF);
    let last = std::cell::Cell::new((0.0, 0.0));
    until(
        "every trace counted once",
        Duration::from_secs(90),
        || {
            let got = (metric_sum(&f, "sum:trace.hits{service:restarted}.as_count()", from, to, 10_000)?, metric_sum(&f, "sum:trace.outbound.hits{service:restarted}.as_count()", from, to, 10_000)?);
            last.set(got);
            Ok(got == ((2 * HALF) as f64, (2 * HALF) as f64))
        },
        &|| format!("{} sent; trace.hits and trace.outbound.hits {:?}\n{}", 2 * HALF, last.get(), f.logs()),
    )?;
    // Both lives' writes, in the one bucket: each a point of its own, at its own place in it.
    let a = f.call("POST", "/api/observe/v1/query", Some(&json!({ "query": "sum:trace.hits{service:restarted}.as_count()", "from": bucket, "to": bucket + 10_000, "step": 1_000 })))?;
    let points: Vec<f64> = at(&a, &["series"]).as_array().into_iter().flatten().flat_map(|s| at(s, &["values"]).as_array().cloned().unwrap_or_default()).filter_map(|v| v.as_f64()).collect();
    anyhow::ensure!(points == [HALF as f64, HALF as f64] || points.len() == 1 && points.first() == Some(&((2 * HALF) as f64)), "the bucket's points: {points:?}");
    eprintln!("  restarted  stopped in {:.1} s; {} traces sent across it, {stored} spans stored, every trace counted once ({points:?} in its bucket)", stopped_in.as_secs_f64(), 2 * HALF);
    Ok(())
}

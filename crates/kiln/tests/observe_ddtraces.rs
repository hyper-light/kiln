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
    for (from, to) in [("shop", "requests"), ("requests", "cart"), ("cart", "cart-db")] {
        anyhow::ensure!(at(&edge(from, to), &["calls"]).as_f64() == Some(kept_n), "{from} -> {to}: {m}");
    }
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

//! Azure Event Hubs read over Kafka's protocol, live (docs/observe.md §10.3, §13): a kiln server
//! as a process of its own, and Apache Kafka's own broker (its image, KRaft, one node) in the place
//! of a namespace's Kafka endpoint, taking Event Hubs' SASL PLAIN user `$ConnectionString` and its
//! connection string. What a user relies on: every diagnostic record of every event is stored once,
//! named as Datadog's forwarder names it; events sent before the integration within its lookback
//! are read; gzip'd batches too; a server stopped, or killed, and started again goes on from where
//! it was, repeating nothing, as does the broker restarted while read; a wrong connection string
//! is said, in the broker's words.
//!
//! It needs Docker: `cargo test -p kiln --test observe_eventhubs -- --ignored`.

mod common;

use std::io::Write as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::{Fleet, at, free_port, now_ms};
use serde_json::{Value, json};

const IMAGE: &str = "apache/kafka:4.1.0";
const CONNECTION: &str = "Endpoint=sb://kiln-test.servicebus.windows.net/;SharedAccessKeyName=listen;SharedAccessKey=c2VjcmV0";
const HUB: &str = "insights-logs";

/// A Kafka broker in a container: SASL PLAIN on the port the test reaches, plaintext inside.
struct Broker {
    name: String,
    port: u16,
}

impl Broker {
    fn start() -> anyhow::Result<Self> {
        let port = free_port()?;
        let name = format!("kiln-eventhubs-{}-{port}", std::process::id());
        let jaas = format!("org.apache.kafka.common.security.plain.PlainLoginModule required user_$ConnectionString=\"{CONNECTION}\";");
        let env = [
            // The image's start script asks for it once SASL is on; the listener's own JAAS
            // configuration (below) is what takes the user.
            ("KAFKA_OPTS", "-Dkiln.test=1".to_owned()),
            ("KAFKA_NODE_ID", "1".to_owned()),
            ("KAFKA_PROCESS_ROLES", "broker,controller".to_owned()),
            ("KAFKA_LISTENERS", "SASL_PLAINTEXT://0.0.0.0:9092,INTERNAL://0.0.0.0:29092,CONTROLLER://0.0.0.0:9093".to_owned()),
            ("KAFKA_ADVERTISED_LISTENERS", format!("SASL_PLAINTEXT://127.0.0.1:{port},INTERNAL://localhost:29092")),
            ("KAFKA_LISTENER_SECURITY_PROTOCOL_MAP", "CONTROLLER:PLAINTEXT,INTERNAL:PLAINTEXT,SASL_PLAINTEXT:SASL_PLAINTEXT".to_owned()),
            ("KAFKA_CONTROLLER_LISTENER_NAMES", "CONTROLLER".to_owned()),
            ("KAFKA_INTER_BROKER_LISTENER_NAME", "INTERNAL".to_owned()),
            ("KAFKA_CONTROLLER_QUORUM_VOTERS", "1@localhost:9093".to_owned()),
            ("KAFKA_SASL_ENABLED_MECHANISMS", "PLAIN".to_owned()),
            ("KAFKA_LISTENER_NAME_SASL__PLAINTEXT_PLAIN_SASL_JAAS_CONFIG", jaas),
            ("KAFKA_OFFSETS_TOPIC_REPLICATION_FACTOR", "1".to_owned()),
            ("KAFKA_TRANSACTION_STATE_LOG_REPLICATION_FACTOR", "1".to_owned()),
            ("KAFKA_TRANSACTION_STATE_LOG_MIN_ISR", "1".to_owned()),
        ];
        let mut c = Command::new("docker");
        c.args(["run", "-d", "--rm", "--name", &name, "-p", &format!("127.0.0.1:{port}:9092")]);
        for (k, v) in &env {
            c.args(["-e", &format!("{k}={v}")]);
        }
        let out = c.arg(IMAGE).output()?;
        anyhow::ensure!(out.status.success(), "docker run: {}", String::from_utf8_lossy(&out.stderr));
        let b = Self { name, port };
        let start = Instant::now();
        loop {
            if b.kafka("kafka-topics.sh", &["--list"]).is_ok() {
                return Ok(b);
            }
            anyhow::ensure!(start.elapsed() < Duration::from_secs(90), "the broker did not come up:\n{}", b.logs());
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// One of Kafka's own tools, inside the container, against its plaintext listener.
    fn kafka(&self, tool: &str, args: &[&str]) -> anyhow::Result<String> {
        let out = Command::new("docker").args(["exec", &self.name, &format!("/opt/kafka/bin/{tool}"), "--bootstrap-server", "localhost:29092"]).args(args).output()?;
        anyhow::ensure!(out.status.success(), "{tool}: {}", String::from_utf8_lossy(&out.stderr));
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Events sent as a producer sends them: each a key (its partition) and a value, in batches,
    /// gzip'd when `gzip`.
    fn produce(&self, topic: &str, events: &[(String, String)], gzip: bool) -> anyhow::Result<()> {
        let mut c = Command::new("docker");
        c.args(["exec", "-i", &self.name, "/opt/kafka/bin/kafka-console-producer.sh", "--bootstrap-server", "localhost:29092", "--topic", topic, "--property", "parse.key=true", "--property", "key.separator=\t"]);
        if gzip {
            c.args(["--compression-codec", "gzip"]);
        }
        let mut child = c.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn()?;
        {
            let mut stdin = child.stdin.take().ok_or_else(|| anyhow::anyhow!("no stdin"))?;
            for (k, v) in events {
                writeln!(stdin, "{k}\t{v}")?;
            }
        }
        let out = child.wait_with_output()?;
        anyhow::ensure!(out.status.success(), "the producer: {}", String::from_utf8_lossy(&out.stderr));
        Ok(())
    }

    fn logs(&self) -> String {
        Command::new("docker").args(["logs", "--tail", "40", &self.name]).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr)).unwrap_or_default()
    }
}

impl Drop for Broker {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.name]).output();
    }
}

fn rfc3339(ms: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).map(|t| t.format(&time::format_description::well_known::Rfc3339).unwrap_or_default()).unwrap_or_default()
}

/// `n` events from `from` on, each a diagnostic setting's `{"records": [...]}` of two resource logs
/// (an App Service's HTTP log, every fifth a 500) and a metric record, keyed over the partitions.
fn events(from: usize, n: usize, t: i64) -> Vec<(String, String)> {
    (from..from + n)
        .map(|i| {
            let status = if i % 5 == 0 { 500 } else { 200 };
            let time = rfc3339(t + i as i64);
            let value = json!({ "records": [
                { "time": time, "resourceId": "/SUBSCRIPTIONS/S-1/RESOURCEGROUPS/RG-1/PROVIDERS/MICROSOFT.WEB/SITES/CHECKOUT", "category": "AppServiceHTTPLogs", "operationName": "Microsoft.Web/sites/log", "level": if status == 500 { "Error" } else { "Informational" }, "properties": { "CsUriStem": format!("/cart/{i}"), "ScStatus": status, "message": format!("GET /cart/{i} {status}") } },
                { "time": time, "resourceId": "/SUBSCRIPTIONS/S-1/RESOURCEGROUPS/RG-1/PROVIDERS/MICROSOFT.WEB/SITES/CHECKOUT", "category": "AppServiceConsoleLogs", "operationName": "Microsoft.Web/sites/log", "level": "Informational", "properties": { "message": format!("event {i} handled") } },
                { "time": time, "resourceId": "/SUBSCRIPTIONS/S-1/RESOURCEGROUPS/RG-1/PROVIDERS/MICROSOFT.WEB/SITES/CHECKOUT", "metricName": "Requests", "total": 1, "count": 1, "timeGrain": "PT1M" }
            ] });
            (format!("k{}", i % 7), value.to_string())
        })
        .collect()
}

/// The records the server holds of `query`, counted.
fn count(f: &Fleet, query: &str, from: i64) -> anyhow::Result<u64> {
    let found = f.call("POST", "/api/observe/v1/logs/aggregate", Some(&json!({ "query": query, "from": from, "to": now_ms() + 600_000 })))?;
    Ok(at(&found, &["groups"]).as_array().and_then(|g| g.first()).and_then(|g| at(g, &["value"]).as_f64()).unwrap_or(0.0) as u64)
}

/// Waits until the server holds `n` records of `query`, and a moment more: never more than `n`.
fn exactly(f: &Fleet, query: &str, from: i64, n: u64) -> anyhow::Result<()> {
    let start = Instant::now();
    eprintln!("  waiting for {n} records");
    loop {
        let have = count(f, query, from)?;
        anyhow::ensure!(have <= n, "{have} records of {query}, more than the {n} sent: some twice\n{}", f.logs());
        if have == n {
            break;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(90), "{have} records of {query}, not {n}\n{}", f.logs());
        std::thread::sleep(Duration::from_millis(250));
    }
    eprintln!("  {n} records in {:.1} s", start.elapsed().as_secs_f64());
    // A record taken twice would show by now: offers are applied within a second.
    std::thread::sleep(Duration::from_secs(3));
    let have = count(f, query, from)?;
    if have != n {
        let found = f.call("POST", "/api/observe/v1/logs/search", Some(&json!({ "query": query, "from": from, "to": now_ms() + 600_000, "limit": 1000 })))?;
        let mut by: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        for r in at(&found, &["records"]).as_array().into_iter().flatten() {
            *by.entry(at(r, &["message"]).as_str().unwrap_or("").to_owned()).or_default() += 1;
        }
        anyhow::bail!("{have} records of {query}, then; not {n}: {:?}\n{}", by.iter().take(12).collect::<Vec<_>>(), f.logs());
    }
    Ok(())
}

fn stream(f: &Fleet, name: &str) -> anyhow::Result<Value> {
    let all = f.call("GET", "/api/observe/v1/clouds", None)?;
    let c = at(&all, &["clouds"]).as_array().into_iter().flatten().find(|c| at(c, &["name"]) == name).cloned().ok_or_else(|| anyhow::anyhow!("no integration {name}"))?;
    Ok(at(&c, &["status", "streams"]).as_array().and_then(|s| s.first()).cloned().unwrap_or(Value::Null))
}

#[test]
#[ignore = "needs Docker: runs Apache Kafka's broker, and a kiln server as a process"]
fn an_event_hub_s_records_are_stored_once_each_through_restarts_and_kills() -> anyhow::Result<()> {
    let broker = Broker::start()?;
    broker.kafka("kafka-topics.sh", &["--create", "--topic", HUB, "--partitions", "3"])?;
    let t = now_ms() - 120_000;
    // Thirty events before kiln reads the hub: within its lookback, so read.
    broker.produce(HUB, &events(0, 30, t), false)?;
    let mut f = Fleet::new()?;
    let cfg = json!({ "kind": "azure", "metrics": false, "interval_s": 300, "lookback_s": 3600, "tags": ["team:pay"], "event_hubs": { "namespace": "kiln-test", "hubs": [HUB], "connection_string": CONNECTION, "endpoint": format!("127.0.0.1:{}", broker.port), "tls": false } });
    let put = f.request("PUT", "/api/observe/v1/clouds/hubs", Some(&cfg))?;
    anyhow::ensure!(put.0 == 204, "{} {}", put.0, String::from_utf8_lossy(&put.1));
    // The connection string is a secret: never shown again.
    let shown = f.call("GET", "/api/observe/v1/clouds", None)?;
    anyhow::ensure!(!shown.to_string().contains("c2VjcmV0"), "{shown}");
    let q = "source:azure.web service:CHECKOUT";
    let from = t - 60_000;
    exactly(&f, q, from, 60)?;
    // Named as Datadog's forwarder names them; an error by its level; the metric records counted.
    let found = f.call("POST", "/api/observe/v1/logs/search", Some(&json!({ "query": format!("{q} status:error"), "from": from, "to": now_ms() + 600_000, "limit": 100 })))?;
    let errors = at(&found, &["records"]).as_array().cloned().unwrap_or_default();
    anyhow::ensure!(errors.len() == 6, "every fifth of 30 an error: {}", errors.len());
    let e = errors.first().ok_or_else(|| anyhow::anyhow!("no error record"))?;
    anyhow::ensure!(at(e, &["attributes", "subscription_id"]) == "S-1" && at(e, &["attributes", "resource_group"]) == "RG-1" && at(e, &["attributes", "azure.event_hub"]) == HUB && at(e, &["attributes", "team"]) == "pay" && at(e, &["attributes", "properties.ScStatus"]) == "500", "{e:#}");
    anyhow::ensure!(at(e, &["message"]).as_str().is_some_and(|m| m.starts_with("GET /cart/") && m.ends_with(" 500")), "{e:#}");
    let s = stream(&f, "hubs")?;
    anyhow::ensure!(at(&s, &["state"]) == "reading" && at(&s, &["partitions"]) == 3 && at(&s, &["skipped"]) == 30 && at(&s, &["events"]) == 30, "{s:#}");

    // Gzip'd batches.
    broker.produce(HUB, &events(30, 30, t), true)?;
    exactly(&f, q, from, 120)?;

    // Stopped and started again: from where it was.
    f.stop_gently();
    f.start_server()?;
    broker.produce(HUB, &events(60, 10, t), false)?;
    exactly(&f, q, from, 140)?;

    // Killed while idle, its offsets written down a second after their writes: nothing again.
    std::thread::sleep(Duration::from_secs(2));
    f.kill_server();
    f.start_server()?;
    broker.produce(HUB, &events(70, 10, t), true)?;
    exactly(&f, q, from, 160)?;
    let start = Instant::now();
    loop {
        let s = stream(&f, "hubs")?;
        if at(&s, &["lag"]) == 0 && at(&s, &["state"]) == "reading" {
            break;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(30), "{s:#}");
        std::thread::sleep(Duration::from_millis(250));
    }

    // The broker restarted while read: the stream fails, says so, connects again and goes on,
    // what it had offered written once.
    broker.produce(HUB, &events(80, 10, t), false)?;
    exactly(&f, q, from, 180)?;
    let out = Command::new("docker").args(["restart", "-t", "5", &broker.name]).output()?;
    anyhow::ensure!(out.status.success(), "docker restart: {}", String::from_utf8_lossy(&out.stderr));
    let start = Instant::now();
    while broker.kafka("kafka-topics.sh", &["--list"]).is_err() {
        anyhow::ensure!(start.elapsed() < Duration::from_secs(90), "the broker did not come back:\n{}", broker.logs());
        std::thread::sleep(Duration::from_millis(500));
    }
    broker.produce(HUB, &events(90, 10, t), true)?;
    exactly(&f, q, from, 200)?;
    // It failed and recovered, each said once as an event.
    for (result, sev) in [("failing", "error"), ("recovered", "info")] {
        let said = count(&f, &format!("source:kiln.event @event.type:cloud.stream @hub:{HUB} @result:{result} status:{sev}"), from)?;
        anyhow::ensure!(said >= 1, "no {result} event of the stream");
    }

    // A wrong connection string: said, in the broker's words.
    let bad = json!({ "kind": "azure", "metrics": false, "event_hubs": { "namespace": "kiln-test", "hubs": [HUB], "connection_string": "Endpoint=sb://kiln-test.servicebus.windows.net/;SharedAccessKeyName=listen;SharedAccessKey=d3Jvbmc", "endpoint": format!("127.0.0.1:{}", broker.port), "tls": false } });
    anyhow::ensure!(f.request("PUT", "/api/observe/v1/clouds/wrong", Some(&bad))?.0 == 204);
    let start = Instant::now();
    loop {
        let s = stream(&f, "wrong")?;
        if at(&s, &["state"]) == "failing" {
            anyhow::ensure!(at(&s, &["error"]).as_str().is_some_and(|e| e.contains("SASL_AUTHENTICATION_FAILED")), "{s:#}");
            break;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(30), "{s:#}");
        std::thread::sleep(Duration::from_millis(250));
    }
    // Deleted, its offsets go with it.
    anyhow::ensure!(f.request("DELETE", "/api/observe/v1/clouds/hubs", None)?.0 == 204);
    let offsets = f.root.path().join("server").join("observe").join("eventhubs-hubs.json");
    let start = Instant::now();
    while offsets.exists() {
        anyhow::ensure!(start.elapsed() < Duration::from_secs(10), "{} is still there", offsets.display());
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

//! Continuous profiles, live (docs/observe.md §8.11, §13): profilers' uploads as Datadog's tracers
//! send them, kept by the server and summed into flame graphs.
//!
//! - Go's own profiles (kiln-obs's fixtures) uploaded to a kiln runner's receiver (the Agent's
//!   port) and to the server's intake as an Agent sends them: each kept, the runner's host and the
//!   tracer's container on the one it sent on (the tags an Agent adds), the Agent's on the other;
//!   listed and searched by their tags; their CPU summed into a flame graph whose hottest function
//!   is the fixtures' own (SHA-256's block function); a profile read back byte for byte; all of it
//!   the same once the server starts again. A malformed upload is refused at the runner.
//! - A Go service's own profiler (dd-trace-go v2.11.2, in Docker) uploading to a runner: its CPU's
//!   flame graph's hottest path its own hashing loop, its allocations' its own allocating loop.
//! - Python's (ddtrace 4.16: libdatadog's exporter) through Datadog's Agent (7.84) proxying to the
//!   server's intake: the same of its loops, the Agent's host on each upload.
//! - Java's (dd-trace-java 1.67.1: a JFR recording of the JDK's events and its own profiler's) to a
//!   runner, read into profiles there: its CPU most of all its recursive function, its allocations
//!   its own loop's, its lock waits its threads' on their monitor, its exceptions where it throws
//!   them, its wall time.
//!
//! `cargo test -p kiln --test observe_profiles -- --ignored --nocapture` (the last three need Docker).
#![cfg(unix)]

mod common;

use std::process::Command;
use std::time::{Duration, Instant};

use common::{Fleet, at, free_port, now_ms};
use serde_json::Value;

const CPU: &[u8] = include_bytes!("../../kiln-obs/src/fixtures/pprof/go-cpu.pprof");
const HEAP: &[u8] = include_bytes!("../../kiln-obs/src/fixtures/pprof/go-heap.pprof");

/// An upload as dd-trace-go writes one: each file a part named as itself, then `event`; its start
/// `start_ms`, a minute long.
fn upload(files: &[(&str, &[u8])], start_ms: i64, tags: &str) -> anyhow::Result<(String, Vec<u8>)> {
    let boundary = format!("kiln-e2e-{start_ms}");
    let mut body = Vec::new();
    let mut part = |name: &str, ct: &str, bytes: &[u8]| {
        body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{name}\"\r\nContent-Type: {ct}\r\n\r\n").as_bytes());
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    };
    for (name, bytes) in files {
        part(name, "application/octet-stream", bytes);
    }
    let rfc = |ms: i64| -> anyhow::Result<String> { Ok(time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)?.format(&time::format_description::well_known::Rfc3339)?) };
    let names: Vec<String> = files.iter().map(|(n, _)| format!("{n:?}")).collect();
    let event = format!(
        r#"{{"start":"{}","end":"{}","attachments":[{}],"tags_profiler":"{tags}","family":"go","version":"4","endpoint_counts":{{"GET /cart":120}}}}"#,
        rfc(start_ms)?,
        rfc(start_ms + 60_000)?,
        names.join(",")
    );
    part("event", "application/json", event.as_bytes());
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    Ok((format!("multipart/form-data; boundary={boundary}"), body))
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

fn list(f: &Fleet, query: &str, from: i64) -> anyhow::Result<Vec<Value>> {
    let q = format!("/api/observe/v1/profiles?from={from}&to={}&limit=500&query={}", now_ms() + 120_000, enc(query));
    Ok(at(&f.call("GET", &q, None)?, &["uploads"]).as_array().cloned().unwrap_or_default())
}

fn enc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

fn tags(u: &Value) -> Vec<String> {
    at(u, &["tags"]).as_array().into_iter().flatten().filter_map(|t| t.as_str().map(str::to_owned)).collect()
}

/// A flame graph asked of the server: its whole, and each function's self and total by name.
struct Graph {
    v: Value,
}

impl Graph {
    fn ask(f: &Fleet, query: &str, from: i64, profile: &str, kind: &str) -> anyhow::Result<Self> {
        let q = format!("/api/observe/v1/profiles/flame?from={from}&to={}&query={}&profile={}&type={}", now_ms() + 120_000, enc(query), enc(profile), enc(kind));
        Ok(Self { v: f.call("GET", &q, None)? })
    }

    fn total(&self) -> i64 {
        at(&self.v, &["total"]).as_i64().unwrap_or(0)
    }

    fn name(&self, i: i64) -> &str {
        at(&self.v, &["names"]).as_array().and_then(|n| usize::try_from(i).ok().and_then(|i| n.get(i))).and_then(Value::as_str).unwrap_or("")
    }

    /// The functions by self: (name, self, total), most first.
    fn top(&self) -> Vec<(String, i64, i64)> {
        at(&self.v, &["top"]).as_array().into_iter().flatten().filter_map(|r| Some((self.name(r.get(0)?.as_i64()?).to_owned(), r.get(1)?.as_i64()?, r.get(2)?.as_i64()?))).collect()
    }

    /// The total of the function whose name ends with `name`, from the top's.
    fn total_of(&self, name: &str) -> i64 {
        self.top().iter().filter(|(n, _, _)| n.ends_with(name)).map(|(_, _, t)| *t).max().unwrap_or(0)
    }
}

#[test]
#[ignore = "runs a kiln server and runner as processes"]
fn profiles_sent_to_a_runner_and_to_the_server_are_kept_searched_and_summed() -> anyhow::Result<()> {
    let t0 = now_ms() - 120_000;
    let mut f = Fleet::new()?;
    let apm = free_port()?;
    f.start_runner_with("web-1", &[("KILN_DD_TRACE_ADDR", &format!("127.0.0.1:{apm}"))])?;
    until("the runner's receiver", Duration::from_secs(60), || Ok(common::raw(apm, "GET", "/info", "", b"").is_ok_and(|(c, _)| c == 200)), &|| f.logs())?;
    until(
        "the runner connected",
        // A runner whose QUIC dial goes unanswered takes the HTTP tunnel after 20 s, and a loaded
        // machine takes longer: connecting is not what this checks.
        Duration::from_secs(120),
        || Ok(at(&f.call("GET", "/api/observe/v1/status", None)?, &["machines"]).as_array().is_some_and(|m| m.iter().any(|m| at(m, &["up"]) == true))),
        &|| f.logs(),
    )?;
    let (code, info) = common::raw(apm, "GET", "/info", "", b"")?;
    anyhow::ensure!(code == 200 && String::from_utf8_lossy(&info).contains("/profiling/v1/input"), "the receiver says it takes profiles: {}", String::from_utf8_lossy(&info));

    // To the runner, as a tracer sends it: its container said in a header, no host of its own.
    let start = now_ms() - 61_000;
    let (ct, body) = upload(&[("cpu.pprof", CPU), ("delta-heap.pprof", HEAP), ("metrics.json", b"[]")], start, "service:checkout,env:prod,version:1.4.2,runtime:go,profile_seq:1")?;
    let (code, answer) = common::raw(apm, "POST", "/profiling/v1/input", &format!("Content-Type: {ct}\r\nDatadog-Container-ID: 0123456789abcdef\r\n"), &body)?;
    anyhow::ensure!(code == 202, "the runner takes it: {code} {}", String::from_utf8_lossy(&answer));
    // A malformed one is refused there, saying why.
    let (code, answer) = common::raw(apm, "POST", "/profiling/v1/input", &format!("Content-Type: {ct}\r\n"), b"--nothing--")?;
    anyhow::ensure!(code == 400, "a malformed upload: {code} {}", String::from_utf8_lossy(&answer));
    // To the server's intake, as an Agent forwards one: its key, and the tags it adds.
    let ingest = std::fs::read_to_string(f.root.path().join("server").join("observe").join("ingest.token"))?.trim().to_owned();
    let (ct2, body2) = upload(&[("cpu.pprof", CPU)], start + 1_000, "service:search,env:staging,version:2.0,profile_seq:9")?;
    let (code, _) = common::raw(f.http, "POST", "/api/v2/profile", &format!("Content-Type: {ct2}\r\n"), &body2)?;
    anyhow::ensure!(code == 401 || code == 403, "no key, not taken: {code}");
    let headers = format!("Content-Type: {ct2}\r\nDD-API-KEY: {ingest}\r\nX-Datadog-Additional-Tags: host:agent-host,default_env:none\r\n");
    let (code, answer) = common::raw(f.http, "POST", "/api/v2/profile", &headers, &body2)?;
    anyhow::ensure!(code == 202, "the server takes it: {code} {}", String::from_utf8_lossy(&answer));

    until("both uploads listed", Duration::from_secs(60), || Ok(list(&f, "*", t0)?.len() == 2), &|| f.logs())?;
    let check = |f: &Fleet| -> anyhow::Result<()> {
        let all = list(f, "*", t0)?;
        let by = |svc: &str| all.iter().find(|u| tags(u).contains(&format!("service:{svc}"))).cloned().unwrap_or(Value::Null);
        let (checkout, search) = (by("checkout"), by("search"));
        let (ct, st) = (tags(&checkout), tags(&search));
        anyhow::ensure!(ct.contains(&"host:web-1".to_owned()) && ct.contains(&"container_id:0123456789abcdef".to_owned()), "the runner's host and the tracer's container: {ct:?}");
        anyhow::ensure!(st.contains(&"host:agent-host".to_owned()) && st.contains(&"default_env:none".to_owned()), "the Agent's tags: {st:?}");
        anyhow::ensure!(at(&checkout, &["start_ms"]).as_i64() == Some(start) && at(&checkout, &["family"]) == "go", "{checkout}");
        let names: Vec<&str> = at(&checkout, &["profiles"]).as_array().into_iter().flatten().filter_map(|p| at(p, &["name"]).as_str()).collect();
        anyhow::ensure!(names == ["cpu.pprof", "delta-heap.pprof"], "its pprof profiles, metrics.json not: {names:?}");
        // Searched by their tags, in log search's syntax.
        anyhow::ensure!(list(f, "service:checkout env:prod", t0)?.len() == 1 && list(f, "host:agent-host OR host:web-1", t0)?.len() == 2 && list(f, "-env:prod", t0)?.len() == 1, "searched by tags");
        // CPU summed over both: 1.5 s each, the fixtures' hashing loop the hottest by far.
        let g = Graph::ask(f, "*", t0, "cpu.pprof", "cpu")?;
        anyhow::ensure!(g.total() == 3_000_000_000 && at(&g.v, &["read"]) == 2 && at(&g.v, &["unit"]) == "nanoseconds", "{}", g.v);
        let top = g.top();
        let first = top.first().cloned().unwrap_or_default();
        anyhow::ensure!(first.0.contains("sha256") && first.1 * 10 >= g.total() * 9, "the hottest function by self: {first:?}");
        let alone = Graph::ask(f, "service:search", t0, "cpu.pprof", "cpu")?;
        anyhow::ensure!(alone.total() == 1_500_000_000, "one service's: {}", alone.v);
        // Compared, one service with the other: the same profile, every path's share the same.
        let cmp = f.call("GET", &format!("/api/observe/v1/profiles/compare?from={t0}&to={}&query=service:checkout&base_query=service:search&profile=cpu.pprof&type=cpu", now_ms() + 120_000), None)?;
        let nodes = at(&cmp, &["nodes"]).as_array().cloned().unwrap_or_default();
        anyhow::ensure!(
            at(&cmp, &["after", "total"]) == 1_500_000_000 && at(&cmp, &["before", "total"]) == 1_500_000_000 && nodes.len() > 3 && nodes.iter().all(|n| n.get(2) == n.get(4) && n.get(3) == n.get(5)),
            "compared path by path: {cmp}"
        );
        // The heap's allocations, of the one upload that sent it.
        let heap = Graph::ask(f, "*", t0, "delta-heap.pprof", "alloc_space")?;
        anyhow::ensure!(heap.total() == 13_105_703 && at(&heap.v, &["unit"]) == "bytes", "{}", heap.v);
        // Read back byte for byte, as `go tool pprof` reads it.
        let id = at(&checkout, &["id"]).as_str().unwrap_or("").to_owned();
        let (code, bytes) = f.get_raw(&format!("/api/observe/v1/profiles/{id}/cpu.pprof"))?;
        let mut want = Vec::new();
        std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(CPU), &mut want)?;
        anyhow::ensure!(code == 200 && bytes == want, "the profile read back: {code}, {} bytes of {}", bytes.len(), want.len());
        let one = f.call("GET", &format!("/api/observe/v1/profiles/{id}"), None)?;
        anyhow::ensure!(at(&one, &["endpoints"]).get(0).map(|e| at(e, &["endpoint"]) == "GET /cart" && at(e, &["count"]) == 120) == Some(true), "its endpoints: {one}");
        Ok(())
    };
    check(&f)?;
    let st = f.call("GET", "/api/observe/v1/profiles/status", None)?;
    anyhow::ensure!(at(&st, &["uploads"]) == 2 && at(&st, &["counts", "kept"]) == 2 && at(&st, &["counts", "malformed"]) == 0 && at(&st, &["segments"]) == 1, "the store's status: {st}");
    // The same once the server starts again: read again from its files.
    f.kill_server();
    f.start_server()?;
    until("the server back", Duration::from_secs(60), || Ok(f.request("GET", "/api/observe/v1/profiles?limit=1", None).is_ok_and(|(c, _)| c == 200)), &|| f.logs())?;
    check(&f)?;
    eprintln!("  profiles   two uploads, to a runner and to the server's intake: kept with an Agent's tags, searched, summed, read back, the same after a restart");
    Ok(())
}

fn docker(args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("docker").args(args).output()?;
    anyhow::ensure!(out.status.success(), "docker {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A container, removed when dropped.
struct Container(String);

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.0]).output();
    }
}

impl Container {
    fn logs(&self) -> String {
        Command::new("docker").args(["logs", "--tail", "40", &self.0]).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr)).unwrap_or_default()
    }
}

const GO_IMAGE: &str = "kiln-ddprof-go:e2e";
const GO_MAIN: &str = r#"package main

import (
	"crypto/sha256"
	"log"
	"os"
	"time"

	"github.com/DataDog/dd-trace-go/v2/profiler"
)

var sink [][]byte

// burn hashes: what its CPU profile shows.
func burn(d time.Duration) {
	b := make([]byte, 4096)
	for end := time.Now().Add(d); time.Now().Before(end); {
		s := sha256.Sum256(b)
		b[0] = s[0]
	}
}

// grow allocates: what its heap profile shows.
func grow() {
	sink = sink[:0]
	for i := 0; i < 256; i++ {
		sink = append(sink, make([]byte, 64<<10))
	}
}

func main() {
	if err := profiler.Start(profiler.WithPeriod(10*time.Second), profiler.WithProfileTypes(profiler.CPUProfile, profiler.HeapProfile)); err != nil {
		log.Fatal(err)
	}
	run, err := time.ParseDuration(os.Getenv("RUN_FOR"))
	if err != nil {
		log.Fatal(err)
	}
	for end := time.Now().Add(run); time.Now().Before(end); {
		burn(150 * time.Millisecond)
		grow()
	}
	profiler.Stop()
}
"#;

#[test]
#[ignore = "needs Docker: builds a Go service with dd-trace-go's profiler, and runs a kiln server and runner as processes"]
fn a_go_service_s_own_profiler_through_a_runner_shows_its_own_hot_functions() -> anyhow::Result<()> {
    let ctx = tempfile::tempdir()?;
    std::fs::write(ctx.path().join("main.go"), GO_MAIN)?;
    std::fs::write(
        ctx.path().join("Dockerfile"),
        "FROM golang:1.27.2-bookworm\nWORKDIR /src\nCOPY main.go .\nRUN go mod init kiln.dev/profiled && go get github.com/DataDog/dd-trace-go/v2@v2.11.2 && go mod tidy && go build -o /profiled .\n",
    )?;
    docker(&["build", "-q", "-t", GO_IMAGE, &ctx.path().display().to_string()])?;
    let t0 = now_ms() - 60_000;
    let mut f = Fleet::new()?;
    let apm = free_port()?;
    f.start_runner_with("go-host", &[("KILN_DD_TRACE_ADDR", &format!("127.0.0.1:{apm}"))])?;
    until(
        "the runner connected",
        // A runner whose QUIC dial goes unanswered takes the HTTP tunnel after 20 s, and a loaded
        // machine takes longer: connecting is not what this checks.
        Duration::from_secs(120),
        || {
            Ok(common::raw(apm, "GET", "/info", "", b"").is_ok_and(|(c, _)| c == 200)
                && at(&f.call("GET", "/api/observe/v1/status", None)?, &["machines"]).as_array().is_some_and(|m| m.iter().any(|m| at(m, &["up"]) == true)))
        },
        &|| f.logs(),
    )?;
    let name = format!("kiln-ddprof-go-{}", std::process::id());
    docker(&[
        "run",
        "-d",
        "--name",
        &name,
        "--network",
        "host",
        "-e",
        "DD_AGENT_HOST=127.0.0.1",
        "-e",
        &format!("DD_TRACE_AGENT_PORT={apm}"),
        "-e",
        "DD_SERVICE=kiln-e2e-go",
        "-e",
        "DD_ENV=e2e",
        "-e",
        "DD_VERSION=1.0.0",
        "-e",
        "DD_INSTRUMENTATION_TELEMETRY_ENABLED=false",
        "-e",
        "RUN_FOR=35s",
        GO_IMAGE,
        "/profiled",
    ])?;
    let c = Container(name);
    let logs = || format!("--- service\n{}\n{}", c.logs(), f.logs());
    until("two uploads of the service", Duration::from_secs(120), || Ok(list(&f, "service:kiln-e2e-go", t0)?.len() >= 2), &logs)?;
    let u = list(&f, "service:kiln-e2e-go", t0)?;
    let first = u.first().cloned().unwrap_or(Value::Null);
    let t = tags(&first);
    // dd-trace-go says its own host (its machine's name), which an Agent's `host` does not
    // replace; its runtime's tags as it says them.
    anyhow::ensure!(
        t.iter().any(|x| x.starts_with("host:"))
            && t.contains(&"env:e2e".to_owned())
            && t.contains(&"version:1.0.0".to_owned())
            && t.contains(&"profiler_version:v2.11.2".to_owned())
            && at(&first, &["family"]) == "go",
        "its tags: {t:?}"
    );
    let g = Graph::ask(&f, "service:kiln-e2e-go", t0, "cpu.pprof", "cpu")?;
    let burn = g.total_of("main.burn");
    anyhow::ensure!(g.total() > 0 && burn * 2 >= g.total(), "its hashing loop the most of its CPU: {burn} of {}\n{:?}", g.total(), g.top().get(..5));
    let heap = Graph::ask(&f, "service:kiln-e2e-go", t0, "delta-heap.pprof", "alloc_space")?;
    let grow = heap.total_of("main.grow");
    anyhow::ensure!(heap.total() > 0 && grow * 2 >= heap.total(), "its allocating loop the most of its allocations: {grow} of {}\n{:?}", heap.total(), heap.top().get(..5));
    eprintln!(
        "  profiles   dd-trace-go v2.11.2's profiler through a runner: {} uploads; main.burn {:.0}% of CPU, main.grow {:.0}% of allocations",
        u.len(),
        100.0 * burn as f64 / g.total() as f64,
        100.0 * grow as f64 / heap.total() as f64
    );
    Ok(())
}

const PY_IMAGE: &str = "kiln-ddtrace-python:e2e";
const PY_DOCKERFILE: &str = "FROM python:3.12-slim\nRUN pip install --no-cache-dir ddtrace==4.16.0 flask==3.1.3 requests\n";
const PY_APP: &str = r#"
import hashlib, os, time
import ddtrace.auto  # the profiler starts with it (DD_PROFILING_ENABLED)

sink = []

def burn(seconds):
    end = time.monotonic() + seconds
    b = b"x" * 4096
    while time.monotonic() < end:
        b = hashlib.sha256(b).digest() * 128

def grow():
    global sink
    sink = [bytearray(65536) for _ in range(64)]

end = time.monotonic() + float(os.environ["RUN_FOR"])
while time.monotonic() < end:
    burn(0.15)
    grow()
"#;
const AGENT: &str = "gcr.io/datadoghq/agent:7.84.4";
const AGENT_HOST: &str = "kiln-prof-e2e";

#[test]
#[ignore = "needs Docker: runs Datadog's Agent and its Python tracer's profiler, and a kiln server as a process"]
fn a_python_service_s_profiler_through_datadog_s_agent_shows_its_own_hot_function() -> anyhow::Result<()> {
    // The traces' test's image (its layers cached where that test ran).
    let ctx = tempfile::tempdir()?;
    std::fs::write(ctx.path().join("Dockerfile"), PY_DOCKERFILE)?;
    docker(&["build", "-q", "-t", PY_IMAGE, &ctx.path().display().to_string()])?;
    let t0 = now_ms() - 60_000;
    let f = Fleet::new()?;
    let ingest = std::fs::read_to_string(f.root.path().join("server").join("observe").join("ingest.token"))?.trim().to_owned();
    let apm = free_port()?;
    let agent = Container(format!("kiln-ddprof-agent-{}", std::process::id()));
    let kiln = format!("http://127.0.0.1:{}", f.http);
    let env = [
        format!("DD_API_KEY={ingest}"),
        format!("DD_DD_URL={kiln}"),
        format!("DD_HOSTNAME={AGENT_HOST}"),
        "DD_APM_ENABLED=true".to_owned(),
        format!("DD_APM_DD_URL={kiln}"),
        format!("DD_APM_PROFILING_DD_URL={kiln}/api/v2/profile"),
        format!("DD_APM_RECEIVER_PORT={apm}"),
        format!("DD_APM_DEBUG_PORT={}", free_port()?),
        format!("DD_DOGSTATSD_PORT={}", free_port()?),
        format!("DD_CMD_PORT={}", free_port()?),
        format!("DD_EXPVAR_PORT={}", free_port()?),
        "DD_LOGS_ENABLED=false".to_owned(),
        "DD_PROCESS_AGENT_ENABLED=false".to_owned(),
        "DD_INVENTORIES_ENABLED=false".to_owned(),
        "DD_REMOTE_CONFIGURATION_ENABLED=false".to_owned(),
    ];
    let mut args: Vec<String> = ["run", "-d", "--name", &agent.0, "--network", "host"].iter().map(|s| (*s).to_owned()).collect();
    for e in &env {
        args.push("-e".to_owned());
        args.push(e.clone());
    }
    args.push(AGENT.to_owned());
    docker(&args.iter().map(String::as_str).collect::<Vec<_>>())?;
    until("the Agent's receiver", Duration::from_secs(90), || Ok(common::raw(apm, "GET", "/info", "", b"").is_ok_and(|(c, _)| c == 200)), &|| agent.logs())?;
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("app.py"), PY_APP)?;
    Command::new("chmod").args(["-R", "a+rX"]).arg(dir.path()).output()?;
    let app = Container(format!("kiln-ddprof-py-{}", std::process::id()));
    docker(&[
        "run",
        "-d",
        "--name",
        &app.0,
        "--network",
        "host",
        "-v",
        &format!("{}:/e2e:ro", dir.path().display()),
        "-e",
        "DD_AGENT_HOST=127.0.0.1",
        "-e",
        &format!("DD_TRACE_AGENT_PORT={apm}"),
        "-e",
        "DD_SERVICE=kiln-e2e-py",
        "-e",
        "DD_ENV=e2e",
        "-e",
        "DD_VERSION=2.0.0",
        "-e",
        "DD_PROFILING_ENABLED=true",
        "-e",
        "DD_PROFILING_UPLOAD_INTERVAL=10",
        "-e",
        "DD_INSTRUMENTATION_TELEMETRY_ENABLED=false",
        "-e",
        "DD_REMOTE_CONFIGURATION_ENABLED=false",
        "-e",
        "RUN_FOR=35",
        PY_IMAGE,
        "python",
        "-u",
        "/e2e/app.py",
    ])?;
    let logs = || format!("--- app\n{}\n--- agent\n{}\n{}", app.logs(), agent.logs(), f.logs());
    until("two uploads of the service", Duration::from_secs(150), || Ok(list(&f, "service:kiln-e2e-py", t0)?.len() >= 2), &logs)?;
    let u = list(&f, "service:kiln-e2e-py", t0)?;
    let first = u.first().cloned().unwrap_or(Value::Null);
    let t = tags(&first);
    anyhow::ensure!(t.contains(&format!("host:{AGENT_HOST}")) && t.contains(&"env:e2e".to_owned()) && at(&first, &["family"]) == "python", "the Agent's host on it: {t:?}");
    // Its CPU type, whatever its profile is named.
    let types = f.call("GET", &format!("/api/observe/v1/profiles/types?from={t0}&to={}&query=service:kiln-e2e-py", now_ms() + 120_000), None)?;
    let cpu = at(&types, &["types"]).as_array().into_iter().flatten().find(|t| at(t, &["type"]) == "cpu-time").cloned().ok_or_else(|| anyhow::anyhow!("no cpu-time: {types}"))?;
    let g = Graph::ask(&f, "service:kiln-e2e-py", t0, at(&cpu, &["profile"]).as_str().unwrap_or(""), "cpu-time")?;
    let burn = g.total_of("burn");
    anyhow::ensure!(g.total() > 0 && burn * 2 >= g.total(), "its hashing loop the most of its CPU: {burn} of {}\n{:?}", g.total(), g.top().get(..5));
    eprintln!("  profiles   ddtrace 4.16's profiler through Datadog's Agent 7.84: {} uploads ({}), burn {:.0}% of CPU", u.len(), at(&cpu, &["profile"]), 100.0 * burn as f64 / g.total() as f64);
    Ok(())
}

const JAVA_IMAGE: &str = "kiln-ddprof-java:e2e";
/// dd-trace-java 1.67.1 as Maven Central has it, checked by its SHA-256 (its SHA-1 as Maven Central
/// lists it, 4bfe4418814513dfd3f00bb60d6f4614257023eb, checked where this was taken).
const JAVA_DOCKERFILE: &str = "FROM eclipse-temurin:21-jdk
ADD --chmod=644 --checksum=sha256:beb620dc2e4783fd328a1ae23f8fd4fcb757ac23e5bb05efb04c6aa3e51a5e43 https://repo1.maven.org/maven2/com/datadoghq/dd-java-agent/1.67.1/dd-java-agent-1.67.1.jar /dd-java-agent.jar
COPY Burn.java /app/
RUN javac -d /app /app/Burn.java
";
/// A small app for a profiler to watch: its CPU in a recursive Fibonacci, its allocations and
/// exceptions in its loop, two threads taking turns on a monitor (its lock waits), and their
/// sleeps (wall time).
const JAVA_APP: &str = r#"public class Burn {
  static volatile long sink;
  static final Object LOCK = new Object();

  static long fib(int n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }

  static void contend() {
    while (true) {
      synchronized (LOCK) {
        try { Thread.sleep(20); } catch (InterruptedException e) { return; }
      }
    }
  }

  public static void main(String[] a) throws Exception {
    for (int i = 0; i < 2; i++) {
      Thread t = new Thread(Burn::contend, "contend-" + i);
      t.setDaemon(true);
      t.start();
    }
    java.util.List<byte[]> keep = new java.util.ArrayList<>();
    long end = System.currentTimeMillis() + Long.parseLong(System.getenv("RUN_MS"));
    for (long i = 0; System.currentTimeMillis() < end; i++) {
      sink += fib(24);
      if ((i & 15) == 0) {
        keep.add(new byte[32 * 1024]);
        if (keep.size() > 256) keep.clear();
        try { throw new IllegalStateException("x"); } catch (IllegalStateException e) { sink++; }
      }
    }
  }
}
"#;

#[test]
#[ignore = "needs Docker: runs a Java app under dd-trace-java's profiler, and a kiln server and runner as processes"]
fn a_java_service_s_recordings_through_a_runner_show_its_own_hot_functions() -> anyhow::Result<()> {
    let ctx = tempfile::tempdir()?;
    std::fs::write(ctx.path().join("Burn.java"), JAVA_APP)?;
    std::fs::write(ctx.path().join("Dockerfile"), JAVA_DOCKERFILE)?;
    docker(&["build", "-q", "-t", JAVA_IMAGE, &ctx.path().display().to_string()])?;
    let t0 = now_ms() - 60_000;
    let mut f = Fleet::new()?;
    let apm = free_port()?;
    f.start_runner_with("java-host", &[("KILN_DD_TRACE_ADDR", &format!("127.0.0.1:{apm}"))])?;
    until(
        "the runner connected",
        // A runner whose QUIC dial goes unanswered takes the HTTP tunnel after 20 s, and a loaded
        // machine takes longer: connecting is not what this checks.
        Duration::from_secs(120),
        || {
            Ok(common::raw(apm, "GET", "/info", "", b"").is_ok_and(|(c, _)| c == 200)
                && at(&f.call("GET", "/api/observe/v1/status", None)?, &["machines"]).as_array().is_some_and(|m| m.iter().any(|m| at(m, &["up"]) == true)))
        },
        &|| f.logs(),
    )?;
    let app = Container(format!("kiln-ddprof-java-{}", std::process::id()));
    docker(&[
        "run",
        "-d",
        "--name",
        &app.0,
        "--network",
        "host",
        "-e",
        "DD_AGENT_HOST=127.0.0.1",
        "-e",
        &format!("DD_TRACE_AGENT_PORT={apm}"),
        "-e",
        "DD_SERVICE=kiln-e2e-java",
        "-e",
        "DD_ENV=e2e",
        "-e",
        "DD_VERSION=3.0.0",
        "-e",
        "DD_PROFILING_ENABLED=true",
        "-e",
        "DD_PROFILING_START_DELAY=1",
        "-e",
        "DD_PROFILING_UPLOAD_PERIOD=10",
        // Its wall-clock samples of every thread, not of those in a trace alone (its default: the
        // app has no trace).
        "-e",
        "DD_PROFILING_DDPROF_WALL_CONTEXT_FILTER=false",
        "-e",
        "DD_INSTRUMENTATION_TELEMETRY_ENABLED=false",
        "-e",
        "DD_REMOTE_CONFIGURATION_ENABLED=false",
        "-e",
        "RUN_MS=40000",
        JAVA_IMAGE,
        "java",
        "-javaagent:/dd-java-agent.jar",
        "-cp",
        "/app",
        "Burn",
    ])?;
    let logs = || format!("--- app\n{}\n{}", app.logs(), f.logs());
    until("two uploads of the service", Duration::from_secs(150), || Ok(list(&f, "service:kiln-e2e-java", t0)?.len() >= 2), &logs)?;
    let u = list(&f, "service:kiln-e2e-java", t0)?;
    let first = u.first().cloned().unwrap_or(Value::Null);
    let t = tags(&first);
    anyhow::ensure!(t.contains(&"env:e2e".to_owned()) && t.contains(&"version:3.0.0".to_owned()) && at(&first, &["family"]) == "java", "its tags: {t:?}");
    // Its recordings' profiles, whatever else the tracer's settings add.
    let types = f.call("GET", &format!("/api/observe/v1/profiles/types?from={t0}&to={}&query=service:kiln-e2e-java", now_ms() + 120_000), None)?;
    let has = |profile: &str, kind: &str| at(&types, &["types"]).as_array().into_iter().flatten().any(|t| at(t, &["profile"]) == profile && at(t, &["type"]) == kind);
    for (profile, kind) in [("cpu.pprof", "cpu"), ("wall.pprof", "wall"), ("alloc.pprof", "alloc_space"), ("lock.pprof", "delay"), ("exceptions.pprof", "exception-samples")] {
        anyhow::ensure!(has(profile, kind), "{profile}'s {kind}: {types}");
    }
    let cpu = Graph::ask(&f, "service:kiln-e2e-java", t0, "cpu.pprof", "cpu")?;
    let fib = cpu.total_of("Burn.fib");
    anyhow::ensure!(cpu.total() > 0 && fib * 2 >= cpu.total(), "its recursive function the most of its CPU: {fib} of {}\n{:?}", cpu.total(), cpu.top().get(..5));
    let alloc = Graph::ask(&f, "service:kiln-e2e-java", t0, "alloc.pprof", "alloc_space")?;
    let main = alloc.total_of("Burn.main");
    anyhow::ensure!(alloc.total() > 0 && main * 2 >= alloc.total(), "its loop the most of its allocations: {main} of {}\n{:?}", alloc.total(), alloc.top().get(..5));
    let lock = Graph::ask(&f, "service:kiln-e2e-java", t0, "lock.pprof", "delay")?;
    let waits = lock.total_of("Burn.contend");
    anyhow::ensure!(waits * 2 >= lock.total() && waits > 0, "its threads waiting on its monitor: {waits} of {}\n{:?}", lock.total(), lock.top().get(..5));
    let thrown = Graph::ask(&f, "service:kiln-e2e-java", t0, "exceptions.pprof", "exception-samples")?;
    anyhow::ensure!(thrown.total_of("Burn.main") > 0, "its exceptions where it throws them: {:?}", thrown.top().get(..5));
    eprintln!(
        "  profiles   dd-trace-java 1.67.1's recordings through a runner: {} uploads; Burn.fib {:.0}% of CPU, Burn.main {:.0}% of allocations, {:.0} ms of lock waits",
        u.len(),
        100.0 * fib as f64 / cpu.total() as f64,
        100.0 * main as f64 / alloc.total() as f64,
        waits as f64 / 1e6
    );
    Ok(())
}

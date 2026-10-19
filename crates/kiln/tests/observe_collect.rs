//! Logs collected on machines, live (docs/observe.md §8.1, §13): a kiln server and a runner as
//! processes of their own. Files the runner reads as a log source says, written as an app writes
//! them, rotated (renamed; copied then truncated), the runner stopped and killed while lines come;
//! containers' logs in Kubernetes' and Docker's layouts; and the journal, through a stand-in for
//! `journalctl` that keeps cursors as the real one does. Every line carries its number, so each is
//! counted where it is stored: one lost or repeated is named.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss, clippy::cast_possible_truncation)]

mod common;

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::{Fleet, at, caught_up, now_ms};
use serde_json::{Value, json};

/// Line `seq` as an app writes it: JSON, a token in it to mask, and every tenth a health check
/// (`seq % 10 == 7`) the source's rules leave out.
fn line(seq: u64) -> String {
    let what = if seq % 10 == 7 { "GET /healthz 200" } else { "order paid" };
    format!("{}\n", json!({ "seq": seq, "level": if seq.is_multiple_of(50) { "error" } else { "info" }, "msg": format!("{what} token=t{seq}x"), "route": "/pay" }))
}

fn excluded(seq: u64) -> bool {
    seq % 10 == 7
}

fn append(path: &Path, seqs: std::ops::Range<u64>) -> anyhow::Result<()> {
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    let text: String = seqs.map(line).collect();
    f.write_all(text.as_bytes())?;
    Ok(())
}

/// Each line's count of records stored (by its `@seq`), of `query`.
fn counts(f: &Fleet, query: &str, from: i64) -> anyhow::Result<BTreeMap<u64, u64>> {
    let a = f.call("POST", "/api/observe/v1/logs/aggregate", Some(&json!({ "query": query, "from": from, "to": now_ms() + 60_000, "by": ["@seq"] })))?;
    let mut out = BTreeMap::new();
    for g in at(&a, &["groups"]).as_array().into_iter().flatten() {
        let seq = at(g, &["group"]).as_object().and_then(|o| o.values().next()).and_then(|v| v.as_str().and_then(|s| s.parse().ok()).or_else(|| v.as_u64()));
        if let Some(seq) = seq {
            out.insert(seq, at(g, &["value"]).as_f64().unwrap_or(0.0) as u64);
        }
    }
    Ok(out)
}

/// Waits until every line of `seqs` the rules keep is stored; then the lines lost (none, or the
/// wait failed) and repeated, and none of those left out stored.
fn settled(f: &Fleet, query: &str, from: i64, seqs: std::ops::Range<u64>, within: Duration) -> anyhow::Result<BTreeMap<u64, u64>> {
    let start = Instant::now();
    loop {
        let c = counts(f, query, from)?;
        let missing: Vec<u64> = seqs.clone().filter(|s| !excluded(*s) && !c.contains_key(s)).collect();
        if missing.is_empty() {
            let left_out: Vec<u64> = seqs.clone().filter(|s| excluded(*s) && c.contains_key(s)).collect();
            anyhow::ensure!(left_out.is_empty(), "lines the rules leave out were stored: {left_out:?}");
            return Ok(c);
        }
        if start.elapsed() > within {
            anyhow::bail!("{} lines of {seqs:?} never stored, the first {:?}\n{}", missing.len(), &missing[..missing.len().min(20)], f.logs());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn repeated(c: &BTreeMap<u64, u64>, seqs: std::ops::Range<u64>) -> Vec<(u64, u64)> {
    seqs.filter_map(|s| c.get(&s).filter(|&&n| n > 1).map(|&n| (s, n))).collect()
}

fn put_source(f: &Fleet, name: &str, s: &Value) -> anyhow::Result<()> {
    let (code, body) = f.request("PUT", &format!("/api/observe/v1/log_sources/{name}"), Some(s))?;
    anyhow::ensure!(code == 204, "PUT {name}: {code} {}", String::from_utf8_lossy(&body));
    Ok(())
}

/// How the source goes on `machine`, as the server says, once that machine's status says it has
/// written records reading it since `after` (its status comes every 10 s; the last one of a
/// process gone stays until the next process says otherwise).
fn state(f: &Fleet, source: &str, machine: &str, after: i64) -> anyhow::Result<Value> {
    let start = Instant::now();
    loop {
        let all = f.call("GET", "/api/observe/v1/log_sources", None)?;
        let s = at(&all, &["log_sources"]).as_array().into_iter().flatten().find(|s| at(s, &["name"]) == source).cloned().unwrap_or(Value::Null);
        let st = at(&s, &["machines"]).as_array().into_iter().flatten().find(|m| at(m, &["machine"]) == machine).map(|m| at(m, &["state"]).clone()).unwrap_or(Value::Null);
        let fresh = at(&st, &["since_ms"]).as_i64().is_some_and(|t| t >= after);
        if (fresh && at(&st, &["records"]).as_u64().is_some_and(|n| n > 0)) || start.elapsed() > Duration::from_secs(30) {
            return Ok(st);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Writes `seqs` to `path` at about `per_s` lines a second, until done or told to stop.
fn writer(path: PathBuf, seqs: std::ops::Range<u64>, per_s: u64, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<anyhow::Result<u64>> {
    std::thread::spawn(move || {
        let start = Instant::now();
        let mut next = seqs.start;
        while next < seqs.end && !stop.load(Ordering::Relaxed) {
            let due = seqs.start + (start.elapsed().as_secs_f64() * per_s as f64) as u64 + 1;
            let to = due.min(seqs.end);
            if to > next {
                append(&path, next..to)?;
                next = to;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(next)
    })
}

#[test]
#[ignore = "starts real processes"]
fn a_runner_s_files_are_stored_once_each_through_rotation_truncation_a_stop_and_a_kill() -> anyhow::Result<()> {
    let mut f = Fleet::new()?;
    f.start_runner("r1")?;
    f.until(Duration::from_secs(90), "r1 a learner, caught up", |s| caught_up(s, &["r1"]))?;
    let t0 = now_ms() - 60_000;
    let dir = f.root.path().join("applogs");
    std::fs::create_dir_all(&dir)?;
    let log = dir.join("app.log");
    // Lines there before the source: read from the beginning, as it says.
    append(&log, 0..500)?;
    let source = json!({
        "type": "file",
        "paths": [format!("{}/*.log", dir.display())],
        "start": "beginning",
        "service": "shop",
        "tags": ["team:pay"],
        "rules": [
            { "type": "exclude_at_match", "name": "health checks", "pattern": "healthz" },
            { "type": "mask_sequences", "name": "tokens", "pattern": "token=[a-z0-9]+", "replace_placeholder": "token=[masked]" },
        ],
        "on": "r1",
    });
    // What it would read, tried on the server first (here, the same files).
    let tried = f.call("POST", "/api/observe/v1/log_sources/app/test", Some(&source))?;
    anyhow::ensure!(at(&tried, &["ok"]) == true && at(&tried, &["matched"]) == 1 && at(&tried, &["records"]).as_array().is_some_and(|r| r.len() == 20), "{tried:#}");
    anyhow::ensure!(at(&tried, &["records"]).as_array().into_iter().flatten().all(|r| at(r, &["message"]).as_str().is_some_and(|m| m.contains("token=[masked]"))), "{tried:#}");
    put_source(&f, "app", &source)?;
    let q = "service:shop";
    let c = settled(&f, q, t0, 0..500, Duration::from_secs(60))?;
    anyhow::ensure!(repeated(&c, 0..500).is_empty(), "{:?}", repeated(&c, 0..500));
    // A record as stored: masked, its level, its file and the source's tag.
    let found = f.call("POST", "/api/observe/v1/logs/search", Some(&json!({ "query": "service:shop @seq:50", "from": t0, "to": now_ms() + 60_000, "limit": 5 })))?;
    let r = at(&found, &["records"]).as_array().and_then(|r| r.first()).cloned().unwrap_or(Value::Null);
    anyhow::ensure!(at(&r, &["message"]) == "order paid token=[masked]" && at(&r, &["status"]) == "error" && at(&r, &["host"]) == "r1" && at(&r, &["source"]) == "file", "{r:#}");
    anyhow::ensure!(at(&r, &["attributes", "team"]) == "pay" && at(&r, &["attributes", "route"]) == "/pay" && at(&r, &["attributes", "log.file.path"]) == log.display().to_string().as_str(), "{r:#}");

    // Renamed (logrotate's `create`): the old file read to its end, late lines too; the new one
    // from its start.
    append(&log, 500..600)?;
    std::fs::rename(&log, dir.join("app.log.1"))?;
    append(&dir.join("app.log.1"), 600..650)?;
    append(&log, 650..800)?;
    let c = settled(&f, q, t0, 0..800, Duration::from_secs(60))?;
    anyhow::ensure!(repeated(&c, 0..800).is_empty(), "renamed: {:?}", repeated(&c, 0..800));

    // Copied, then truncated (logrotate's `copytruncate`): the copy is known and not read again;
    // the truncated file from its start.
    append(&log, 800..900)?;
    std::fs::copy(&log, dir.join("app-copy.log"))?;
    std::fs::OpenOptions::new().write(true).truncate(true).open(&log)?;
    append(&log, 900..1000)?;
    let c = settled(&f, q, t0, 0..1000, Duration::from_secs(60))?;
    anyhow::ensure!(repeated(&c, 0..1000).is_empty(), "copied and truncated: {:?}", repeated(&c, 0..1000));

    // Stopped (SIGTERM) while lines come, and started again: none lost, none repeated.
    let stop = Arc::new(AtomicBool::new(false));
    let w = writer(log.clone(), 1000..5000, 2000, stop.clone());
    std::thread::sleep(Duration::from_millis(700));
    f.stop_runner("r1");
    std::thread::sleep(Duration::from_millis(500));
    f.start_runner("r1")?;
    let wrote = w.join().map_err(|_| anyhow::anyhow!("the writer panicked"))??;
    anyhow::ensure!(wrote == 5000);
    let c = settled(&f, q, t0, 0..5000, Duration::from_secs(90))?;
    let said: String = std::fs::read_to_string(f.root.path().join("r1.log")).unwrap_or_default().lines().filter(|l| l.contains("log source") || l.contains("shutting") || l.contains("signal")).collect::<Vec<_>>().join("\n");
    anyhow::ensure!(repeated(&c, 1000..5000).is_empty(), "a stop repeated {} lines: {:?}\n{said}", repeated(&c, 1000..5000).len(), &repeated(&c, 1000..5000)[..repeated(&c, 1000..5000).len().min(10)]);

    // Killed (SIGKILL) while lines come: none lost; those in flight may be stored twice, no more.
    let w = writer(log.clone(), 5000..9000, 2000, stop.clone());
    std::thread::sleep(Duration::from_millis(700));
    f.kill_runner("r1");
    std::thread::sleep(Duration::from_millis(500));
    f.start_runner("r1")?;
    w.join().map_err(|_| anyhow::anyhow!("the writer panicked"))??;
    let c = settled(&f, q, t0, 0..9000, Duration::from_secs(90))?;
    let twice = repeated(&c, 5000..9000);
    anyhow::ensure!(twice.iter().all(|&(_, n)| n == 2) && twice.len() <= 3000, "a kill repeated {} lines (at most what was in flight)", twice.len());
    eprintln!("a kill repeated {} of 4,000 lines (in flight when it came); a stop none", twice.len());

    let st = state(&f, "app", "r1", 0)?;
    anyhow::ensure!(at(&st, &["files"]).as_u64().is_some_and(|n| n >= 1) && at(&st, &["records"]).as_u64().is_some_and(|n| n > 0) && at(&st, &["error"]).is_null(), "{st:#}");
    Ok(())
}

/// A stand-in for `journalctl`: the entries of `$JOURNAL_ENTRIES` (a line each, its cursor
/// `c=<its line's number>`), followed; from after `--after-cursor`, from the end with `--lines=0`.
const JOURNALCTL: &str = r#"#!/bin/sh
after=0; tail_=all
for a in "$@"; do
  case "$a" in
    --after-cursor=c=*) after="${a#--after-cursor=c=}" ;;
    --lines=0) tail_=0 ;;
  esac
done
if [ "$tail_" = 0 ] && [ "$after" = 0 ]; then after=$(wc -l < "$JOURNAL_ENTRIES"); fi
exec tail -n "+$((after + 1))" -F "$JOURNAL_ENTRIES" 2>/dev/null
"#;

/// RFC 3339 of `ms` ago, to the nanosecond `ns` past it.
fn rfc3339(ago_ms: i64, ns: i64) -> String {
    let t = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from((now_ms() - ago_ms) * 1_000_000 + ns)).unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    t.format(&time::format_description::well_known::Rfc3339).unwrap_or_default()
}

fn journal_entry(n: u64, priority: u8, ident: &str, message: &str) -> String {
    format!("{}\n", json!({ "__CURSOR": format!("c={n}"), "__REALTIME_TIMESTAMP": format!("{}", (now_ms() - 1000) * 1000), "PRIORITY": priority.to_string(), "SYSLOG_IDENTIFIER": ident, "_SYSTEMD_UNIT": format!("{ident}.service"), "_PID": "4242", "MESSAGE": message }))
}

fn search(f: &Fleet, from: i64, q: &str) -> anyhow::Result<Vec<Value>> {
    let a = f.call("POST", "/api/observe/v1/logs/search", Some(&json!({ "query": q, "from": from, "to": now_ms() + 60_000, "limit": 100 })))?;
    Ok(at(&a, &["records"]).as_array().cloned().unwrap_or_default())
}

/// The records `q` finds, once there are `n` of them (a minute at the most).
fn found(f: &Fleet, from: i64, q: &str, n: usize) -> anyhow::Result<Vec<Value>> {
    let start = Instant::now();
    loop {
        let r = search(f, from, q)?;
        if r.len() >= n {
            return Ok(r);
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(60), "{q}: {} of {n}\n{}", r.len(), f.logs());
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[test]
#[ignore = "starts real processes"]
fn containers_logs_and_the_journal_are_stored_with_what_they_name() -> anyhow::Result<()> {
    // Before anything is written: the journal's entries carry their own times.
    let t0 = now_ms() - 60_000;
    let mut f = Fleet::new()?;
    let root = f.root.path().to_owned();
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin)?;
    let fake = bin.join("journalctl");
    std::fs::write(&fake, JOURNALCTL)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))?;
    }
    let entries = root.join("journal.jsonl");
    std::fs::write(&entries, [journal_entry(1, 6, "sshd", "Accepted publickey for ops"), journal_entry(2, 3, "sshd", "Failed password for root")].concat())?;
    let env = [("KILN_JOURNALCTL", fake.to_str().unwrap_or("")), ("JOURNAL_ENTRIES", entries.to_str().unwrap_or(""))];
    f.start_runner_with("r1", &env)?;
    f.until(Duration::from_secs(90), "r1 a learner, caught up", |s| caught_up(s, &["r1"]))?;
    // A pod's container log, CRI's lines: a long line in parts, then JSON.
    let pod = root.join("k8s/pods/shop_web-1_0f2c/nginx");
    std::fs::create_dir_all(&pod)?;
    std::fs::write(pod.join("0.log"), format!("{} stdout P first half, \n{} stdout F second half\n{} stderr F {{\"level\":\"error\",\"msg\":\"upstream timed out\"}}\n", rfc3339(3000, 1), rfc3339(3000, 2), rfc3339(2000, 0)))?;
    // A Docker container's json-file, its name and Compose service in its config.
    let d = root.join("docker/c0ffee");
    std::fs::create_dir_all(&d)?;
    std::fs::write(d.join("config.v2.json"), r#"{"Name":"/shop-api-1","Config":{"Image":"shop/api:3","Labels":{"com.docker.compose.service":"api"}}}"#)?;
    let lines: String = ["listening on :8080\n", "WARN slow query\n"].iter().map(|l| format!("{}\n", json!({ "log": l, "stream": "stdout", "time": rfc3339(1000, 0) }))).collect();
    std::fs::write(d.join("c0ffee-json.log"), lines)?;
    put_source(&f, "pods", &json!({ "type": "kubernetes", "paths": [format!("{}/k8s/pods/*/*/*.log", root.display())], "start": "beginning", "on": "r1" }))?;
    put_source(&f, "containers", &json!({ "type": "docker", "paths": [format!("{}/docker/*/*-json.log", root.display())], "start": "beginning", "on": "r1" }))?;
    put_source(&f, "journal", &json!({ "type": "journald", "start": "beginning", "units": ["sshd.service"], "on": "r1" }))?;
    let pods = found(&f, t0, "source:kubernetes", 2)?;
    let joined = pods.iter().find(|r| at(r, &["message"]) == "first half, second half").cloned().unwrap_or(Value::Null);
    anyhow::ensure!(at(&joined, &["service"]) == "nginx" && at(&joined, &["attributes", "k8s.pod.name"]) == "web-1" && at(&joined, &["attributes", "k8s.namespace.name"]) == "shop" && at(&joined, &["attributes", "log.iostream"]) == "stdout", "{pods:#?}");
    anyhow::ensure!(pods.iter().any(|r| at(r, &["message"]) == "upstream timed out" && at(r, &["status"]) == "error"), "{pods:#?}");
    let docker = found(&f, t0, "source:docker", 2)?;
    anyhow::ensure!(docker.iter().all(|r| at(r, &["service"]) == "api" && at(r, &["attributes", "container.name"]) == "shop-api-1" && at(r, &["attributes", "container.image.name"]) == "shop/api:3"), "{docker:#?}");
    anyhow::ensure!(docker.iter().any(|r| at(r, &["message"]) == "WARN slow query" && at(r, &["status"]) == "warn"), "{docker:#?}");
    let journal = found(&f, t0, "source:journald", 2)?;
    anyhow::ensure!(journal.iter().any(|r| at(r, &["message"]) == "Failed password for root" && at(r, &["status"]) == "error" && at(r, &["service"]) == "sshd" && at(r, &["attributes", "systemd.unit"]) == "sshd.service"), "{journal:#?}");
    for name in ["pods", "containers", "journal"] {
        let st = state(&f, name, "r1", 0)?;
        anyhow::ensure!(at(&st, &["records"]).as_u64().is_some_and(|n| n > 0) && at(&st, &["error"]).is_null(), "{name}: {st:#}");
    }
    // The runner stopped and started again, an entry more meanwhile: read from after its cursor,
    // none twice.
    f.stop_runner("r1");
    std::fs::OpenOptions::new().append(true).open(&entries)?.write_all(journal_entry(3, 4, "sshd", "Connection reset by peer").as_bytes())?;
    let restarted = now_ms();
    f.start_runner_with("r1", &env)?;
    let journal = found(&f, t0, "source:journald", 3)?;
    std::thread::sleep(Duration::from_secs(3));
    let journal_now = search(&f, t0, "source:journald")?;
    anyhow::ensure!(journal.len() == 3 && journal_now.len() == 3, "the journal's records after a restart: {journal_now:#?}");
    // Its counts are since it started: the journal's one entry since; the files' none, nothing new.
    let st = state(&f, "journal", "r1", restarted)?;
    let said: String = std::fs::read_to_string(f.root.path().join("r1.log")).unwrap_or_default().lines().filter(|l| l.contains("journal") || l.contains("log source") || l.contains("connected") || l.contains("enrolled")).collect::<Vec<_>>().join("\n");
    let mut stored: Vec<String> = search(&f, t0, "source:journald")?.iter().map(|r| format!("{} {}", at(r, &["ts_ns"]), at(r, &["message"]))).collect();
    stored.push(format!("{:#}", f.call("POST", "/api/observe/v1/logs/aggregate", Some(&json!({ "query": "source:journald", "from": t0, "to": now_ms() + 60_000, "by": ["message"] })))?));
    anyhow::ensure!(at(&st, &["records"]) == 1 && at(&st, &["error"]).is_null(), "journal: {st:#}\n{stored:#?}\n{said}");
    Ok(())
}

/// A BSD syslog time (`Mmm dd hh:mm:ss`, UTC) of now.
fn bsd_now() -> String {
    let t = time::OffsetDateTime::now_utc();
    let month = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"][usize::from(u8::from(t.month())) - 1];
    format!("{month} {:>2} {:02}:{:02}:{:02}", t.day(), t.hour(), t.minute(), t.second())
}

#[test]
#[ignore = "starts real processes"]
fn syslog_is_received_over_udp_and_tcp_with_what_its_messages_name() -> anyhow::Result<()> {
    let t0 = now_ms() - 60_000;
    let mut f = Fleet::new()?;
    f.start_runner("r1")?;
    f.until(Duration::from_secs(90), "r1 a learner, caught up", |s| caught_up(s, &["r1"]))?;
    let (udp, tcp) = (common::free_port()?, common::free_port()?);
    put_source(&f, "syslog", &json!({ "type": "syslog", "listen": [format!("udp://127.0.0.1:{udp}"), format!("tcp://127.0.0.1:{tcp}")], "rules": [{ "type": "exclude_at_match", "pattern": "noise" }], "tags": ["site:lab"], "on": "r1" }))?;
    // Bound once the runner has the source: a datagram before that is lost, as UDP's would be.
    let start = Instant::now();
    loop {
        let st = state(&f, "syslog", "r1", 0)?;
        if at(&st, &["since_ms"]).is_i64() && at(&st, &["error"]).is_null() {
            break;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(60), "the source never started on r1: {st:#}");
        std::thread::sleep(Duration::from_millis(500));
    }
    let now = time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339)?;
    let sock = std::net::UdpSocket::bind("127.0.0.1:0")?;
    // RFC 5424 with structured data; RFC 3164; a message the rules leave out.
    sock.send_to(format!("<165>1 {now} router-1 bgpd 812 PEER [peer@32473 ip=\"10.0.0.2\" state=\"down\"] neighbor 10.0.0.2 went down").as_bytes(), ("127.0.0.1", udp))?;
    sock.send_to(format!("<34>{} web-1 sudo[4242]: auth failure for ops", bsd_now()).as_bytes(), ("127.0.0.1", udp))?;
    sock.send_to(format!("<14>{} web-1 app: noise", bsd_now()).as_bytes(), ("127.0.0.1", udp))?;
    // TCP: counted frames, two in one write and one split across two; then newline and NUL ended.
    let mut c = std::net::TcpStream::connect(("127.0.0.1", tcp))?;
    let framed = |m: &str| format!("{} {m}", m.len());
    let a = format!("<13>1 {now} db-1 postgres - - - checkpoint complete");
    let b = format!("<11>1 {now} db-1 postgres - - - FATAL: too many connections");
    let cm = format!("<12>1 {now} db-1 postgres - - - slow query: 2.3 s");
    c.write_all(format!("{}{}", framed(&a), framed(&b)).as_bytes())?;
    let whole = framed(&cm);
    let (first, second) = whole.split_at(whole.len() / 2);
    c.write_all(first.as_bytes())?;
    std::thread::sleep(Duration::from_millis(200));
    c.write_all(second.as_bytes())?;
    drop(c);
    let mut c = std::net::TcpStream::connect(("127.0.0.1", tcp))?;
    c.write_all(format!("<30>{} cache-1 redis[7]: saved the dataset\n<27>{} cache-1 redis[7]: out of memory\0", bsd_now(), bsd_now()).as_bytes())?;
    drop(c);
    let got = found(&f, t0, "source:syslog", 7)?;
    std::thread::sleep(Duration::from_secs(2));
    let got_now = search(&f, t0, "source:syslog")?;
    anyhow::ensure!(got.len() == 7 && got_now.len() == 7, "seven messages (the noise left out): {got_now:#?}");
    let one = |m: &str| got.iter().find(|r| at(r, &["message"]) == m).cloned().unwrap_or(Value::Null);
    let bgp = one("neighbor 10.0.0.2 went down");
    // Priority 165: local4, notice (OpenTelemetry's info).
    anyhow::ensure!(at(&bgp, &["service"]) == "bgpd" && at(&bgp, &["host"]) == "router-1" && at(&bgp, &["status"]) == "info", "{bgp:#}");
    anyhow::ensure!(at(&bgp, &["attributes", "sd.peer@32473.ip"]) == "10.0.0.2" && at(&bgp, &["attributes", "syslog.facility"]) == "local4" && at(&bgp, &["attributes", "syslog.msgid"]) == "PEER" && at(&bgp, &["attributes", "site"]) == "lab", "{bgp:#}");
    let sudo = one("auth failure for ops");
    // Priority 34: auth, critical (fatal).
    anyhow::ensure!(at(&sudo, &["service"]) == "sudo" && at(&sudo, &["host"]) == "web-1" && at(&sudo, &["status"]) == "fatal" && at(&sudo, &["attributes", "syslog.procid"]) == "4242" && at(&sudo, &["attributes", "network.client.ip"]) == "127.0.0.1", "{sudo:#}");
    anyhow::ensure!(at(&one("FATAL: too many connections"), &["status"]) == "error" && !one("slow query: 2.3 s").is_null() && !one("checkpoint complete").is_null(), "{got:#?}");
    anyhow::ensure!(at(&one("out of memory"), &["status"]) == "error" && at(&one("saved the dataset"), &["service"]) == "redis", "{got:#?}");
    let st = state(&f, "syslog", "r1", 0)?;
    anyhow::ensure!(at(&st, &["records"]) == 7 && at(&st, &["excluded"]) == 1 && at(&st, &["dropped"]) == 0, "{st:#}");
    Ok(())
}

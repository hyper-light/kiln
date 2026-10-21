//! Logs collected on machines, live (docs/observe.md §8.1, §13): a kiln server and a runner as
//! processes of their own. Files the runner reads as a log source says, written as an app writes
//! them, rotated (renamed; copied then truncated), the runner stopped and killed while lines come;
//! containers' logs in Kubernetes' and Docker's layouts; and the journal, through a stand-in for
//! `journalctl` that keeps cursors as the real one does. Every line carries its number, so each is
//! counted where it is stored: one lost or repeated is named. JSON lines and text lines come mixed,
//! as records and as the rows the runner writes where it reads them (§12.17).
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

/// Line `seq` as an app writes it: JSON, or text one in three (`seq % 3 == 1`: written as its row
/// where it is read, no pipeline reading it); a token in it to mask, and every tenth a health check
/// (`seq % 10 == 7`) the source's rules leave out.
fn line(seq: u64) -> String {
    let what = if seq % 10 == 7 { "GET /healthz 200" } else { "order paid" };
    let error = seq.is_multiple_of(50);
    if seq % 3 == 1 {
        return format!("{} {what} token=t{seq}x seq={seq} of {}\n", if error { "ERROR" } else { "INFO" }, group(seq));
    }
    format!("{}\n", json!({ "seq": seq, "level": if error { "error" } else { "info" }, "msg": format!("{what} token=t{seq}x"), "route": "/pay" }))
}

/// A text line's group, a word its search finds: 300 text lines to a group, under a search's 1,000
/// records even with each stored twice.
fn group(seq: u64) -> String {
    format!("g{}x", seq / 900)
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

/// Each line's count of records stored, of `query`: a JSON line's by its `@seq`, a text line's by
/// the `seq=` its message says (each group of `seqs` searched).
fn counts(f: &Fleet, query: &str, from: i64, seqs: std::ops::Range<u64>) -> anyhow::Result<BTreeMap<u64, u64>> {
    let a = f.call("POST", "/api/observe/v1/logs/aggregate", Some(&json!({ "query": query, "from": from, "to": now_ms() + 60_000, "by": ["@seq"] })))?;
    let mut out = BTreeMap::new();
    for g in at(&a, &["groups"]).as_array().into_iter().flatten() {
        let seq = at(g, &["group"]).as_object().and_then(|o| o.values().next()).and_then(|v| v.as_str().and_then(|s| s.parse().ok()).or_else(|| v.as_u64()));
        if let Some(seq) = seq {
            out.insert(seq, at(g, &["value"]).as_f64().unwrap_or(0.0) as u64);
        }
    }
    for g in seqs.start / 900..seqs.end.div_ceil(900) {
        let found = f.call("POST", "/api/observe/v1/logs/search", Some(&json!({ "query": format!("{query} {}", group(g * 900)), "from": from, "to": now_ms() + 60_000, "limit": 1000 })))?;
        let records = at(&found, &["records"]).as_array().cloned().unwrap_or_default();
        anyhow::ensure!(records.len() < 1000, "a group's search was cut at {} records", records.len());
        for r in &records {
            let seq = at(r, &["message"]).as_str().and_then(|m| m.split_once("seq=")).and_then(|(_, rest)| rest.split(' ').next()?.parse::<u64>().ok());
            if let Some(seq) = seq {
                *out.entry(seq).or_insert(0) += 1;
            }
        }
    }
    Ok(out)
}

/// Waits until every line of `seqs` the rules keep is stored; then the lines lost (none, or the
/// wait failed) and repeated, and none of those left out stored.
fn settled(f: &Fleet, query: &str, from: i64, seqs: std::ops::Range<u64>, within: Duration) -> anyhow::Result<BTreeMap<u64, u64>> {
    let start = Instant::now();
    loop {
        let c = counts(f, query, from, seqs.clone())?;
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
    // A text line as stored, from the row the runner wrote: masked, the level it starts with, its
    // file and the source's tag.
    let found = f.call("POST", "/api/observe/v1/logs/search", Some(&json!({ "query": "service:shop \"seq=100 of\"", "from": t0, "to": now_ms() + 60_000, "limit": 5 })))?;
    let r = at(&found, &["records"]).as_array().and_then(|r| r.first()).cloned().unwrap_or(Value::Null);
    anyhow::ensure!(at(&r, &["message"]) == "ERROR order paid token=[masked] seq=100 of g0x" && at(&r, &["status"]) == "error" && at(&r, &["host"]) == "r1" && at(&r, &["source"]) == "file", "{r:#}");
    anyhow::ensure!(at(&r, &["attributes", "team"]) == "pay" && at(&r, &["attributes", "log.file.path"]) == log.display().to_string().as_str() && at(&r, &["attributes", "route"]).is_null(), "{r:#}");

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

/// nginx's access lines as nginx writes them: `seq` in the path, a 5xx every tenth.
fn nginx_line(seq: u64) -> String {
    let t = time::OffsetDateTime::now_utc();
    let month = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"][usize::from(u8::from(t.month())) - 1];
    let status = if seq % 10 == 9 { 502 } else { 200 };
    format!("192.0.2.{} - - [{:02}/{month}/{}:{:02}:{:02}:{:02} +0000] \"GET /orders/{seq}?page=2 HTTP/1.1\" {status} 512 \"-\" \"Mozilla/5.0 (X11; Linux x86_64) Firefox/131.0\"\n", seq % 250, t.day(), t.year(), t.hour(), t.minute(), t.second())
}

#[test]
#[ignore = "starts real processes"]
fn pipelines_parse_logs_where_they_are_taken_on_every_machine() -> anyhow::Result<()> {
    let t0 = now_ms() - 60_000;
    let mut f = Fleet::new()?;
    f.start_runner("r1")?;
    f.until(Duration::from_secs(90), "r1 a learner, caught up", |s| caught_up(s, &["r1"]))?;
    // A pipeline of yours, for records posted to the server.
    let made = f.call(
        "POST",
        "/api/observe/v1/pipelines",
        Some(&json!({ "name": "shop timings", "filter": { "query": "source:shop" }, "processors": [
            { "type": "grok-parser", "name": "timing", "source": "message", "grok": { "match_rules": "timing %{word:http.method} %{notSpace:http.url} took %{number:duration}ms level=%{word:lvl}" } },
            { "type": "arithmetic-processor", "expression": "duration / 1000", "target": "duration_s" },
            { "type": "status-remapper", "sources": ["lvl"] }
        ] })),
    )?;
    let id = at(&made, &["id"]).as_str().unwrap_or("").to_owned();
    anyhow::ensure!(id.starts_with('p'), "{made:#}");
    // Tried on records, every pipeline applied now: a line its rule reads is parsed, one it does
    // not read is left as it is.
    let tried = f.call("POST", "/api/observe/v1/pipeline_test", Some(&json!({ "records": [{ "message": "GET /cart took 1250ms level=warn", "source": "shop" }, { "message": "GET /cart was slow", "source": "shop" }] })))?;
    let records = at(&tried, &["records"]).as_array().cloned().unwrap_or_default();
    anyhow::ensure!(records.len() == 2 && records.first().is_some_and(|r| at(r, &["attributes", "duration_s"]) == "1.25" && at(r, &["status"]) == "warn") && records.get(1).is_some_and(|r| at(r, &["attributes"]).as_object().is_some_and(serde_json::Map::is_empty)), "{tried:#}");
    let tried = f.call("POST", "/api/observe/v1/pipeline_test", Some(&json!({ "records": [{ "message": "GET /cart took 1250ms level=warn", "source": "shop" }], "pipeline": { "name": "t", "processors": [{ "type": "grok-parser", "grok": { "match_rules": "r %{word:a} %{notSpace:b} took %{number:ms}ms level=%{word:l}" } }] } })))?;
    anyhow::ensure!(at(&tried, &["records"]).as_array().and_then(|r| r.first()).is_some_and(|r| at(r, &["attributes", "ms"]) == "1250"), "{tried:#}");
    let (code, _) = f.request("POST", "/api/observe/v1/logs", Some(&json!([{ "message": "GET /cart took 1250ms level=warn", "service": "shop", "source": "shop", "hostname": "web-1" }])))?;
    anyhow::ensure!(code == 202, "{code}");
    let shop = found(&f, t0, "source:shop", 1)?;
    let r = shop.first().cloned().unwrap_or(Value::Null);
    anyhow::ensure!(at(&r, &["attributes", "http.method"]) == "GET" && at(&r, &["attributes", "duration_s"]) == "1.25" && at(&r, &["status"]) == "warn", "{r:#}");
    // kiln's own nginx pipeline, on the runner that reads nginx's log.
    let dir = f.root.path().join("nginx");
    std::fs::create_dir_all(&dir)?;
    let log = dir.join("access.log");
    std::fs::write(&log, (0..20).map(nginx_line).collect::<String>())?;
    put_source(&f, "nginx", &json!({ "type": "file", "paths": [format!("{}/*.log", dir.display())], "start": "beginning", "source": "nginx", "service": "edge", "on": "r1" }))?;
    let lines = found(&f, t0, "source:nginx", 20)?;
    anyhow::ensure!(lines.iter().all(|r| at(r, &["host"]) == "r1" && at(r, &["attributes", "http.method"]) == "GET" && at(r, &["attributes", "http.url_details.queryString.page"]) == "2"), "parsed on r1: {:#}", lines.first().cloned().unwrap_or(Value::Null));
    // The 5xx ones: errors, found by their status code.
    let errors = search(&f, t0, "source:nginx @http.status_code:>=500")?;
    anyhow::ensure!(errors.len() == 2 && errors.iter().all(|r| at(r, &["status"]) == "error" && at(r, &["attributes", "http.status_category"]) == "error"), "{errors:#?}");
    // Turned off: lines from then on are kept as they are, on the runner too.
    let (code, body) = f.request("PUT", "/api/observe/v1/pipelines/builtin:nginx", Some(&json!({ "name": "nginx", "is_enabled": false })))?;
    anyhow::ensure!(code == 200, "{code} {}", String::from_utf8_lossy(&body));
    let (code, _) = f.request("DELETE", "/api/observe/v1/pipelines/builtin:nginx", None)?;
    anyhow::ensure!(code == 400, "kiln's own are turned off, not removed: {code}");
    std::thread::sleep(Duration::from_secs(1));
    std::fs::OpenOptions::new().append(true).open(&log)?.write_all(nginx_line(99).as_bytes())?;
    let start = Instant::now();
    loop {
        let all = search(&f, t0, "source:nginx")?;
        if let Some(r) = all.iter().find(|r| at(r, &["message"]).as_str().is_some_and(|m| m.contains("/orders/99?"))) {
            anyhow::ensure!(at(r, &["attributes", "http.method"]).is_null(), "parsed though its pipeline is off: {r:#}");
            break;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(60), "the line after never came\n{}", f.logs());
        std::thread::sleep(Duration::from_millis(500));
    }
    let (code, _) = f.request("DELETE", &format!("/api/observe/v1/pipelines/{id}"), None)?;
    anyhow::ensure!(code == 204);
    Ok(())
}


/// A stand-in for `wevtutil qe`: a channel's events are `$WEVT_DIR/<channel>.xml`, one a line, as
/// the real one writes them. `/rd:true /c:1`: the newest; `/q:...EventRecordID>N...`: those after
/// N, oldest first, `/c:M` of them at the most (the query's other terms it leaves to the test).
const WEVTUTIL: &str = r#"#!/bin/sh
[ "$1" = qe ] || exit 1
file="$WEVT_DIR/$2.xml"
[ -f "$file" ] || { echo "The specified channel could not be found." >&2; exit 15007; }
shift 2
after=0; count=1000000; newest=0
for a in "$@"; do
  case "$a" in
    /q:*) after=$(printf '%s' "$a" | sed -n 's/.*EventRecordID>\([0-9]*\).*/\1/p') ;;
    /c:*) count="${a#/c:}" ;;
    /rd:true) newest=1 ;;
  esac
done
# As wevtutil writes them: one event after another, no line between, none after the last.
if [ "$newest" = 1 ]; then tail -n 1 "$file" | tr -d '\n'; exit 0; fi
awk -v after="$after" -v count="$count" '
  { if (match($0, /<EventRecordID>[0-9]+</)) { id = substr($0, RSTART + 15, RLENGTH - 16) + 0; if (id > after && n < count) { print; n++ } } }
' "$file" | tr -d '\n'
"#;

/// Event `id` of channel `System` as `wevtutil` renders it: its level and message.
fn win_event(id: u64, level: u8, message: &str) -> String {
    let at = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(now_ms() - 1000) * 1_000_000).unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    let t = format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:07}Z", at.year(), u8::from(at.month()), at.day(), at.hour(), at.minute(), at.second(), at.nanosecond() / 100);
    format!(
        "<Event xmlns='http://schemas.microsoft.com/win/2004/08/events/event'><System><Provider Name='kiln-e2e'/><EventID>{}</EventID><Level>{level}</Level><TimeCreated SystemTime='{t}'/><EventRecordID>{id}</EventRecordID><Execution ProcessID='4242' ThreadID='7'/><Channel>System</Channel><Computer>win-e2e</Computer><Security/></System><EventData><Data Name='seq'>{id}</Data></EventData><RenderingInfo Culture='en-US'><Message>{message} &amp; seq={id}</Message><Level>Information</Level></RenderingInfo></Event>\n",
        1000 + id % 7
    )
}

fn append_events(path: &Path, ids: std::ops::Range<u64>) -> anyhow::Result<()> {
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    for id in ids {
        // A line a write: the stand-in never reads half of one.
        f.write_all(win_event(id, if id % 10 == 3 { 2 } else { 4 }, "service state changed").as_bytes())?;
    }
    Ok(())
}

/// Each event's count of records stored (by its `seq`).
fn win_counts(f: &Fleet, from: i64) -> anyhow::Result<BTreeMap<u64, u64>> {
    let a = f.call("POST", "/api/observe/v1/logs/aggregate", Some(&json!({ "query": "source:windows_event", "from": from, "to": now_ms() + 60_000, "by": ["@windows.event_data.seq"] })))?;
    let mut out = BTreeMap::new();
    for g in at(&a, &["groups"]).as_array().into_iter().flatten() {
        if let Some(seq) = at(g, &["group"]).as_object().and_then(|o| o.values().next()).and_then(|v| v.as_str().and_then(|s| s.parse().ok())) {
            out.insert(seq, at(g, &["value"]).as_f64().unwrap_or(0.0) as u64);
        }
    }
    Ok(out)
}

fn win_settled(f: &Fleet, from: i64, seqs: std::ops::Range<u64>) -> anyhow::Result<BTreeMap<u64, u64>> {
    let start = Instant::now();
    loop {
        let c = win_counts(f, from)?;
        let missing: Vec<u64> = seqs.clone().filter(|s| !c.contains_key(s)).collect();
        if missing.is_empty() {
            return Ok(c);
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(60), "{} events of {seqs:?} never stored, the first {:?}\n{}", missing.len(), &missing[..missing.len().min(20)], f.logs());
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[test]
#[ignore = "starts real processes"]
fn windows_event_channels_are_read_once_each_through_a_stop_a_kill_and_a_clear() -> anyhow::Result<()> {
    let t0 = now_ms() - 60_000;
    let mut f = Fleet::new()?;
    let root = f.root.path().to_owned();
    let bin = root.join("bin");
    let channels = root.join("channels");
    std::fs::create_dir_all(&bin)?;
    std::fs::create_dir_all(&channels)?;
    let fake = bin.join("wevtutil");
    std::fs::write(&fake, WEVTUTIL)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))?;
    }
    let system = channels.join("System.xml");
    // Events there before the source: read from the channel's start, as it says.
    append_events(&system, 1..21)?;
    let env = [("KILN_WEVTUTIL", fake.to_str().unwrap_or("")), ("WEVT_DIR", channels.to_str().unwrap_or(""))];
    f.start_runner_with("r1", &env)?;
    f.until(Duration::from_secs(90), "r1 a learner, caught up", |s| caught_up(s, &["r1"]))?;
    put_source(&f, "win", &json!({ "type": "windows_event", "channels": ["System"], "start": "beginning", "tags": ["team:win"], "on": "r1" }))?;
    let c = win_settled(&f, t0, 1..21)?;
    anyhow::ensure!(repeated(&c, 1..21).is_empty(), "{:?}", repeated(&c, 1..21));
    // A record as stored: its message rendered, its level, what its event names.
    let found = f.call("POST", "/api/observe/v1/logs/search", Some(&json!({ "query": "source:windows_event @windows.event_data.seq:13", "from": t0, "to": now_ms() + 60_000, "limit": 5 })))?;
    let r = at(&found, &["records"]).as_array().and_then(|r| r.first()).cloned().unwrap_or(Value::Null);
    anyhow::ensure!(at(&r, &["message"]) == "service state changed & seq=13" && at(&r, &["status"]) == "error" && at(&r, &["service"]) == "kiln-e2e" && at(&r, &["host"]) == "r1", "{r:#}");
    anyhow::ensure!(at(&r, &["attributes", "windows.event_id"]) == "1006" && at(&r, &["attributes", "windows.channel"]) == "System" && at(&r, &["attributes", "team"]) == "win", "{r:#}");
    append_events(&system, 21..61)?;
    let c = win_settled(&f, t0, 1..61)?;
    anyhow::ensure!(repeated(&c, 1..61).is_empty(), "{:?}", repeated(&c, 1..61));

    // Stopped while events come, started again: from its place, none repeated.
    f.stop_runner("r1");
    append_events(&system, 61..101)?;
    f.start_runner_with("r1", &env)?;
    let c = win_settled(&f, t0, 1..101)?;
    anyhow::ensure!(repeated(&c, 1..101).is_empty(), "a stop repeated {:?}", repeated(&c, 1..101));

    // Killed while events come: none lost; those in flight stored twice at the most.
    let writer = {
        let system = system.clone();
        std::thread::spawn(move || -> anyhow::Result<()> {
            for start in (101..401).step_by(10) {
                append_events(&system, start..start + 10)?;
                std::thread::sleep(Duration::from_millis(30));
            }
            Ok(())
        })
    };
    std::thread::sleep(Duration::from_millis(400));
    f.kill_runner("r1");
    std::thread::sleep(Duration::from_millis(300));
    f.start_runner_with("r1", &env)?;
    writer.join().map_err(|_| anyhow::anyhow!("the writer panicked"))??;
    let c = win_settled(&f, t0, 1..401)?;
    let twice = repeated(&c, 101..401);
    anyhow::ensure!(twice.iter().all(|&(_, n)| n == 2) && twice.len() <= 300, "a kill repeated {} events", twice.len());
    eprintln!("a kill repeated {} of 300 events (in flight when it came); a stop none", twice.len());

    // The channel cleared (its record ids begun again): read from its start.
    std::fs::write(&system, "")?;
    let fresh: String = (1..6).map(|id| win_event(id, 3, &format!("after the clear {id}")).replace(&format!("<Data Name='seq'>{id}</Data>"), &format!("<Data Name='seq'>{}</Data>", 1000 + id))).collect();
    std::fs::write(&system, fresh)?;
    let c = win_settled(&f, t0, 1001..1006)?;
    anyhow::ensure!(repeated(&c, 1001..1006).is_empty(), "{:?}", repeated(&c, 1001..1006));
    Ok(())
}

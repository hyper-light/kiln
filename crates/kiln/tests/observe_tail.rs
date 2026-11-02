//! Live tail, end to end (`docs/observe.md` §6, §13): a kiln server as a process of its own, its
//! tail followed over a real WebSocket while records are posted to its logs API. What is checked
//! is what a person following a search sees: the records it matches, each once, as they come;
//! past the rate asked a sample whose mix is the stream's, every matched record counted; a tail
//! opened again after the server restarted shows nothing applied before; and a search that is
//! wrong, or a tail too many, says why.

mod common;

use std::net::TcpStream;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use common::{Fleet, at};

type Socket = tungstenite::WebSocket<TcpStream>;

fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A tail of `query` (at `rate` records a second, if given), its reads timing out every 200 ms.
fn tail(f: &Fleet, query: &str, rate: Option<u32>) -> anyhow::Result<Socket> {
    let stream = TcpStream::connect(("127.0.0.1", f.http))?;
    stream.set_read_timeout(Some(Duration::from_millis(200)))?;
    let rate = rate.map(|r| format!("&rate={r}")).unwrap_or_default();
    let req = http::Request::builder()
        .uri(format!("ws://127.0.0.1:{}/api/observe/v1/logs/tail?query={}{rate}", f.http, enc(query)))
        .header("Host", format!("127.0.0.1:{}", f.http))
        .header("Authorization", format!("Bearer {}", f.admin))
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", tungstenite::handshake::client::generate_key())
        .body(())?;
    let (ws, _) = tungstenite::client::client(req, stream).map_err(|e| anyhow::anyhow!("the tail's handshake: {e}"))?;
    Ok(ws)
}

/// The next message, if one comes within a read's timeout; an error once the tail is closed.
fn next(ws: &mut Socket) -> anyhow::Result<Option<Value>> {
    match ws.read() {
        Ok(tungstenite::Message::Text(t)) => Ok(Some(serde_json::from_str(t.as_str())?)),
        Ok(tungstenite::Message::Close(_)) => anyhow::bail!("closed"),
        Ok(_) => Ok(None),
        Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Messages for `d`, or until `done` says so.
fn read_for(ws: &mut Socket, d: Duration, mut done: impl FnMut(&[Value]) -> bool) -> anyhow::Result<Vec<Value>> {
    let start = Instant::now();
    let mut out = Vec::new();
    while start.elapsed() < d && !done(&out) {
        if let Some(m) = next(ws)? {
            out.push(m);
        }
    }
    Ok(out)
}

fn records(msgs: &[Value]) -> Vec<&Value> {
    msgs.iter().flat_map(|m| at(m, &["records"]).as_array().into_iter().flatten()).collect()
}

fn total(msgs: &[Value], key: &str) -> u64 {
    msgs.iter().filter_map(|m| at(m, &[key]).as_u64()).sum()
}

fn post(f: &Fleet, logs: &[Value]) -> anyhow::Result<()> {
    let start = Instant::now();
    loop {
        let (code, body) = f.request("POST", "/api/observe/v1/logs", Some(&Value::from(logs.to_vec())))?;
        match code {
            202 => return Ok(()),
            // Behind on its writes: sent again, as a client would.
            503 if start.elapsed() < Duration::from_secs(60) => std::thread::sleep(Duration::from_millis(100)),
            _ => anyhow::bail!("posting logs: {code} {}", String::from_utf8_lossy(&body)),
        }
    }
}

fn line(service: &str, i: u64) -> Value {
    json!({ "message": format!("{service} line {i}"), "service": service, "hostname": "e2e", "source": "e2e-tail", "status": "info", "attributes": { "i": i } })
}

fn ready(f: &Fleet) -> anyhow::Result<()> {
    f.until(Duration::from_secs(60), "the server leads", |s| at(s, &["leader"]).as_u64() == Some(1))?;
    Ok(())
}

/// Followed at a pace it can show in full: each record the search matches comes once, in the order
/// posted, within a couple of seconds of its post; what it does not match never comes, nor what
/// was posted before the tail opened; every row the tail read is counted, matched or not.
#[test]
#[ignore = "starts a kiln server as a process"]
fn a_tail_shows_each_record_it_matches_once_as_it_comes_and_nothing_else() -> anyhow::Result<()> {
    let f = Fleet::new()?;
    ready(&f)?;
    post(&f, &[line("e2e-tail", 999)])?;
    // Applied before the tail opens: searchable, and not the tail's.
    let start = Instant::now();
    while at(&f.call("POST", "/api/observe/v1/logs/search", Some(&json!({ "query": "@i:999", "from": 0, "to": i64::MAX / 1_000_000, "limit": 1 })))?, &["records"]).as_array().is_none_or(Vec::is_empty) {
        anyhow::ensure!(start.elapsed() < Duration::from_secs(30), "the first record never searchable");
        std::thread::sleep(Duration::from_millis(100));
    }
    let mut ws = tail(&f, "service:e2e-tail", None)?;
    let mut msgs = Vec::new();
    for round in 0..8_u64 {
        let batch: Vec<Value> = (0..6_u64).map(|k| round * 6 + k).map(|i| if i % 2 == 0 { line("e2e-tail", i) } else { line("e2e-other", i) }).collect();
        post(&f, &batch)?;
        msgs.extend(read_for(&mut ws, Duration::from_millis(500), |_| false)?);
    }
    msgs.extend(read_for(&mut ws, Duration::from_secs(10), |m| records(m).len() >= 24)?);
    let shown: Vec<u64> = records(&msgs).iter().filter_map(|r| at(r, &["attributes", "i"]).as_str().and_then(|i| i.parse().ok())).collect();
    let expected: Vec<u64> = (0..48).filter(|i| i % 2 == 0).collect();
    anyhow::ensure!(shown == expected, "shown {shown:?}\n{}", f.logs());
    anyhow::ensure!(records(&msgs).iter().all(|r| at(r, &["service"]) == "e2e-tail" && at(r, &["status"]) == "info"), "{msgs:?}");
    // Counted: 24 matched of the 48 posted (and of whatever kiln wrote of its own meanwhile).
    let (read, matched, missed) = (total(&msgs, "read"), total(&msgs, "matched"), total(&msgs, "missed"));
    anyhow::ensure!(read >= 48 && matched == 24 && missed == 0, "read {read}, matched {matched}, missed {missed}");
    // As they come: one record at a time, each on the tail within two seconds of its post.
    let mut worst = Duration::ZERO;
    for i in 100..110_u64 {
        let sent = Instant::now();
        post(&f, &[line("e2e-tail", i)])?;
        let got = read_for(&mut ws, Duration::from_secs(5), |m| records(m).iter().any(|r| at(r, &["attributes", "i"]).as_str() == Some(&i.to_string())))?;
        anyhow::ensure!(records(&got).iter().any(|r| at(r, &["attributes", "i"]).as_str() == Some(&i.to_string())), "record {i} never came");
        worst = worst.max(sent.elapsed());
    }
    eprintln!("a record posted came on the tail within {worst:?} at the most");
    anyhow::ensure!(worst < Duration::from_secs(2), "{worst:?}");
    // Nothing coming, the tail says so each second, its counts at 0.
    let quiet = read_for(&mut ws, Duration::from_millis(2_500), |_| false)?;
    anyhow::ensure!(quiet.len() >= 2 && records(&quiet).is_empty() && total(&quiet, "matched") == 0, "{quiet:?}");
    Ok(())
}

/// A flood far past the rate asked: what the tail shows is at most the rate, each record once, a
/// uniform sample (the rare service's share of it is its share of the flood); every record it
/// matched is counted, none missed.
#[test]
#[ignore = "starts a kiln server as a process"]
fn a_flood_is_shown_as_a_uniform_sample_and_counted_in_full() -> anyhow::Result<()> {
    let f = Fleet::new()?;
    ready(&f)?;
    let rate = 1_000_u32;
    let mut ws = tail(&f, "source:e2e-tail", Some(rate))?;
    let (n, per) = (40_000_u64, 2_000_u64);
    let started = Instant::now();
    let mut msgs = Vec::new();
    for b in 0..n / per {
        let batch: Vec<Value> = (b * per..(b + 1) * per).map(|i| line(if i % 10 == 3 { "e2e-rare" } else { "e2e-common" }, i)).collect();
        post(&f, &batch)?;
        msgs.extend(read_for(&mut ws, Duration::from_millis(10), |_| false)?);
    }
    let posted = started.elapsed();
    msgs.extend(read_for(&mut ws, Duration::from_secs(30), |m| total(m, "matched") + total(m, "missed") >= n)?);
    let followed = started.elapsed();
    let (matched, missed) = (total(&msgs, "matched"), total(&msgs, "missed"));
    anyhow::ensure!(matched + missed == n, "matched {matched}, missed {missed} of {n}\n{}", f.logs());
    anyhow::ensure!(missed == 0, "a tail that keeps up misses nothing: {missed}");
    let shown = records(&msgs);
    // At most the rate: over the whole, and in any message (a tick's share and what the ticks before
    // it left of theirs, a second's worth at the most).
    let most = msgs.iter().map(|m| at(m, &["records"]).as_array().map_or(0, Vec::len)).max().unwrap_or(0);
    anyhow::ensure!(most <= rate as usize, "a message of {most} records");
    anyhow::ensure!(shown.len() as f64 <= f64::from(rate) * (followed.as_secs_f64() + 1.0), "{} shown in {followed:?}", shown.len());
    let mut ids: Vec<u64> = shown.iter().filter_map(|r| at(r, &["attributes", "i"]).as_str().and_then(|i| i.parse().ok())).collect();
    let k = ids.len();
    ids.sort_unstable();
    ids.dedup();
    anyhow::ensure!(ids.len() == k, "a record shown twice");
    let rare = shown.iter().filter(|r| at(r, &["service"]) == "e2e-rare").count();
    let share = rare as f64 / k as f64;
    // A tenth of the flood is rare: the share of a uniform sample of k is within 3.3 standard
    // errors of it (the 0.999 interval).
    let se = (0.1 * 0.9 / k as f64).sqrt();
    eprintln!("{n} records posted in {posted:?}, followed in {followed:?}: {k} shown, {share:.4} of them rare (±{:.4})", 3.3 * se);
    anyhow::ensure!(k >= 1_000, "only {k} shown");
    anyhow::ensure!((share - 0.1).abs() < 3.3 * se, "rare share {share:.4} of {k}");
    Ok(())
}

/// The server killed and started again: it applies again the entries after its last cut, and a
/// tail opened as soon as it answers shows none of them, only what is posted after.
#[test]
#[ignore = "starts a kiln server as a process"]
fn a_tail_opened_after_a_restart_shows_nothing_applied_before_it() -> anyhow::Result<()> {
    let mut f = Fleet::new()?;
    ready(&f)?;
    for b in 0..20_u64 {
        post(&f, &(b * 1_000..(b + 1) * 1_000).map(|i| line("e2e-before", i)).collect::<Vec<_>>())?;
    }
    let start = Instant::now();
    while at(&f.call("POST", "/api/observe/v1/logs/search", Some(&json!({ "query": "@i:19999", "from": 0, "to": i64::MAX / 1_000_000, "limit": 1 })))?, &["records"]).as_array().is_none_or(Vec::is_empty) {
        anyhow::ensure!(start.elapsed() < Duration::from_secs(60), "the last record never searchable");
        std::thread::sleep(Duration::from_millis(100));
    }
    f.kill_server();
    f.start_server()?;
    // As soon as the API answers.
    let start = Instant::now();
    let mut ws = loop {
        match tail(&f, "*", Some(1_000)) {
            Ok(ws) => break ws,
            Err(e) => {
                anyhow::ensure!(start.elapsed() < Duration::from_secs(60), "no tail after the restart: {e}");
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };
    // The tail may be refused while the store opens; then it is opened again.
    let first = read_for(&mut ws, Duration::from_secs(1), |m| !m.is_empty())?;
    if first.iter().any(|m| at(m, &["type"]) == "error") {
        ready(&f)?;
        ws = tail(&f, "*", Some(1_000))?;
    }
    ready(&f)?;
    post(&f, &(0..5).map(|i| line("e2e-after", i)).collect::<Vec<_>>())?;
    let msgs = read_for(&mut ws, Duration::from_secs(10), |m| records(m).iter().filter(|r| at(r, &["service"]) == "e2e-after").count() >= 5)?;
    let before = records(&msgs).iter().filter(|r| at(r, &["service"]) == "e2e-before").count();
    let after = records(&msgs).iter().filter(|r| at(r, &["service"]) == "e2e-after").count();
    anyhow::ensure!(before == 0 && after == 5, "{before} applied before shown, {after} of 5 after\n{}", f.logs());
    // Every record is the store's all the same, once.
    let found = f.call("POST", "/api/observe/v1/logs/aggregate", Some(&json!({ "query": "service:e2e-before", "from": 0, "to": i64::MAX / 1_000_000 })))?;
    let groups = at(&found, &["groups"]).as_array().cloned().unwrap_or_default();
    let count = groups.first().and_then(|g| at(g, &["value"]).as_f64()).unwrap_or(0.0);
    anyhow::ensure!(count == 20_000.0, "{count} of 20,000 kept");
    Ok(())
}

/// A search that does not parse, a rate past the bound and a tail too many are each told why and
/// closed; one that closes makes room.
#[test]
#[ignore = "starts a kiln server as a process"]
fn a_wrong_search_a_wrong_rate_and_a_tail_too_many_are_told_why() -> anyhow::Result<()> {
    let f = Fleet::new()?;
    ready(&f)?;
    let refused = |ws: &mut Socket| -> anyhow::Result<String> {
        let start = Instant::now();
        loop {
            match next(ws) {
                Ok(Some(m)) if at(&m, &["type"]) == "error" => return Ok(at(&m, &["message"]).as_str().unwrap_or("").to_owned()),
                Ok(_) if start.elapsed() < Duration::from_secs(5) => {}
                Ok(m) => anyhow::bail!("no error: {m:?}"),
                Err(e) => anyhow::bail!("closed before saying why: {e}"),
            }
        }
    };
    let mut ws = tail(&f, "service:(", None)?;
    let why = refused(&mut ws)?;
    anyhow::ensure!(!why.is_empty(), "{why}");
    let mut ws = tail(&f, "*", Some(0))?;
    anyhow::ensure!(refused(&mut ws)?.contains("rate"));
    let mut ws = tail(&f, "*", Some(5_000))?;
    anyhow::ensure!(refused(&mut ws)?.contains("rate"));
    let mut open: Vec<Socket> = (0..16).map(|_| tail(&f, "*", None)).collect::<anyhow::Result<_>>()?;
    // Each is following: the idle message comes.
    for ws in &mut open {
        anyhow::ensure!(!read_for(ws, Duration::from_secs(3), |m| !m.is_empty())?.is_empty(), "a tail said nothing");
    }
    let mut extra = tail(&f, "*", None)?;
    anyhow::ensure!(refused(&mut extra)?.contains("tails follow"), "the seventeenth followed");
    if let Some(mut one) = open.pop() {
        one.close(None)?;
        // Its close read through, the server lets its place go.
        let _ = read_for(&mut one, Duration::from_secs(1), |_| false);
    }
    let start = Instant::now();
    loop {
        let mut again = tail(&f, "*", None)?;
        let got = read_for(&mut again, Duration::from_secs(2), |m| !m.is_empty())?;
        if got.iter().any(|m| at(m, &["type"]) == "records") {
            break;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(10), "no room after one closed: {got:?}");
    }
    Ok(())
}

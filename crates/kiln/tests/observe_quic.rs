//! kiln's own transport in observe, live (docs/observe.md §8, §13): a kiln server and a runner as
//! processes of their own on real QUIC sockets (hyper-quic, mutual TLS), each looking at its
//! endpoint every 2 s and writing what it measures of the connection as `kiln.quic.*`: the path
//! (round trips, probe timeout, congestion window, largest datagram) and what the connection
//! counted (packets, bytes and frames each way, losses). What a user relies on: the path's figures
//! are the path's (a round trip on loopback, the least of them no more than the smoothed one, a
//! window of two datagrams at the least), data the group carries is counted where it crosses, what
//! one end sent the other received, losses never past what was sent, and a connection that ends
//! is gone from the count, its successor counted from its own start (a counter's reset is no
//! negative traffic).
//!
//! `cargo test -p kiln --test observe_quic -- --ignored --nocapture`
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss)]

mod common;

use std::time::{Duration, Instant};

use common::{Fleet, Tables, caught_up, now_ms};
use serde_json::{Value, json};

/// Waits up to `secs` for `done` to say yes, or fails saying `what`, what it last saw and the
/// fleet's logs.
fn until(f: &Fleet, secs: u64, what: &str, mut done: impl FnMut() -> anyhow::Result<(bool, Value)>) -> anyhow::Result<Value> {
    let start = Instant::now();
    let mut seen = Value::Null;
    while start.elapsed() < Duration::from_secs(secs) {
        match done() {
            Ok((true, v)) => return Ok(v),
            Ok((false, v)) => seen = v,
            Err(e) => seen = json!(format!("{e:#}")),
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    anyhow::bail!("never: {what}; last {seen:#}\n{}", f.logs())
}

/// The last value `q` answers over the last two minutes, the largest of its series' last values.
fn last(f: &Fleet, q: &str) -> anyhow::Result<Option<f64>> {
    let b = f.call("POST", "/api/observe/v1/query", Some(&json!({ "query": q, "from": now_ms() - 120_000, "to": now_ms() + 10_000, "step": 2_000 })))?;
    Ok(b["series"].as_array().into_iter().flatten().filter_map(|s| s["values"].as_array()?.iter().rev().find_map(Value::as_f64)).reduce(f64::max))
}

/// The sum of every value `q` answers from `from` on, and the least of them.
fn total(f: &Fleet, q: &str, from: i64) -> anyhow::Result<(f64, f64)> {
    let b = f.call("POST", "/api/observe/v1/query", Some(&json!({ "query": q, "from": from, "to": now_ms() + 10_000, "step": 2_000 })))?;
    let values: Vec<f64> = b["series"].as_array().into_iter().flatten().flat_map(|s| s["values"].as_array().into_iter().flatten().filter_map(Value::as_f64)).collect();
    Ok((values.iter().sum(), values.iter().copied().fold(0.0, f64::min)))
}

/// `n` hexadecimal digits a generator of its own makes (SplitMix64): text LZ4 finds no matches in.
fn noise(seed: &mut u64, n: usize) -> String {
    let mut out = String::with_capacity(n);
    while out.len() < n {
        *seed = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = *seed;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        out.push_str(&format!("{:016x}", z ^ (z >> 31)));
    }
    out.truncate(n);
    out
}

/// Each end's view of the connection: the server's of `r1`, `r1`'s of the server.
const ENDS: [(&str, &str); 2] = [("the server's", "{peer:r1}"), ("r1's", "{host:r1,peer:server}")];

#[test]
#[ignore = "starts a kiln server and a runner as processes"]
fn each_end_of_the_quic_lane_reports_its_path_and_what_crossed_it() -> anyhow::Result<()> {
    let t0 = now_ms();
    // Each node looks at its endpoint every 2 s.
    let mut f = Fleet::with_tables(&Tables { host_seconds: 2, ..Tables::default() }, |_| String::new(), &[])?;
    let runner = [("KILN_OBSERVE_HOST_SECONDS", "2")];
    f.start_runner_with("r1", &runner)?;
    f.until(Duration::from_secs(90), "r1 a learner, caught up", |s| caught_up(s, &["r1"]))?;

    // The path's figures are the path's, at either end.
    for (end, tags) in ENDS {
        let q = |m: &str| format!("max:kiln.quic.{m}{tags}");
        until(&f, 60, &format!("{end} path reported"), || {
            let rtt = last(&f, &q("smoothed_rtt"))?;
            Ok((rtt.is_some(), json!(rtt)))
        })?;
        let (rtt, min_rtt, pto) = (last(&f, &q("smoothed_rtt"))?.unwrap_or(-1.0), last(&f, &q("min_rtt"))?.unwrap_or(-1.0), last(&f, &q("pto"))?.unwrap_or(-1.0));
        let (cwnd, datagram) = (last(&f, &q("congestion_window"))?.unwrap_or(-1.0), last(&f, &q("max_datagram"))?.unwrap_or(-1.0));
        anyhow::ensure!(rtt > 0.0 && rtt < 1.0, "{end} smoothed round trip on loopback: {rtt} s");
        anyhow::ensure!(min_rtt > 0.0 && min_rtt <= rtt, "{end} least round trip {min_rtt} s, smoothed {rtt} s");
        anyhow::ensure!(pto >= rtt, "{end} probe timeout {pto} s under its round trip {rtt} s (RFC 9002 §6.2.1)");
        // RFC 9000 §14: every path carries 1,200 bytes; RFC 9002 §7.2: the window at least two datagrams.
        anyhow::ensure!(datagram >= 1200.0, "{end} largest datagram {datagram}");
        anyhow::ensure!(cwnd >= 2.0 * datagram, "{end} congestion window {cwnd} under two datagrams of {datagram}");
    }
    // One connection held at each end.
    let connections =
        f.call("POST", "/api/observe/v1/query", Some(&json!({ "query": "max:kiln.quic.connections{*} by {host}", "from": now_ms() - 30_000, "to": now_ms() + 10_000, "step": 2_000 })))?;
    let held: Vec<(String, Option<f64>)> = connections["series"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| (s["tags"]["host"].as_str().unwrap_or("").to_owned(), s["values"].as_array().and_then(|v| v.iter().rev().find_map(Value::as_f64))))
        .collect();
    anyhow::ensure!(held.len() == 2 && held.iter().all(|(_, n)| *n == Some(1.0)), "a connection at each end: {held:?}");

    // Data the group carries crosses the lane, and is counted there: 2 MB of records no compressor
    // shrinks, written to the server, replicated to r1 over the connection.
    let before = now_ms();
    let (sent_before, _) = total(&f, "sum:kiln.quic.bytes_sent{peer:r1}.as_count()", t0)?;
    let mut seed = 7;
    for batch in 0..8 {
        let recs: Vec<Value> = (0..32).map(|i| json!({ "message": noise(&mut seed, 8_192), "service": "quic-e2e", "hostname": "e2e", "attributes": { "n": batch * 32 + i } })).collect();
        f.call("POST", "/api/observe/v1/logs", Some(&Value::Array(recs)))?;
    }
    let raw = 8.0 * 32.0 * 8_192.0;
    f.until(Duration::from_secs(60), "r1 caught up with the records", |s| caught_up(s, &["r1"]))?;
    until(&f, 60, "the records' bytes counted where they crossed", || {
        let (sent, _) = total(&f, "sum:kiln.quic.bytes_sent{peer:r1}.as_count()", t0)?;
        Ok((sent - sent_before >= raw, json!({ "sent": sent - sent_before, "raw": raw })))
    })?;
    // What one end sent the other received: the server's datagrams' bytes to r1, r1's from the
    // server, over the same time, each counted every 2 s (the two counts a sample apart at most).
    until(&f, 30, "what the server sent, r1 received", || {
        let (sent, _) = total(&f, "sum:kiln.quic.bytes_sent{peer:r1}.as_count()", before)?;
        let (received, _) = total(&f, "sum:kiln.quic.bytes_received{host:r1,peer:server}.as_count()", before)?;
        Ok(((received - sent).abs() <= 0.05 * sent, json!({ "sent": sent, "received": received })))
    })?;
    // Losses never past what was sent, at either end.
    for (end, tags) in ENDS {
        let (sent, _) = total(&f, &format!("sum:kiln.quic.packets_sent{tags}.as_count()"), t0)?;
        let (lost, _) = total(&f, &format!("sum:kiln.quic.packets_lost{tags}.as_count()"), t0)?;
        anyhow::ensure!(sent > 0.0 && lost >= 0.0 && lost <= sent, "{end} packets lost {lost} of {sent}");
    }

    // A connection that ends is gone from the count; its successor is counted from its own start.
    f.kill_runner("r1");
    until(&f, 60, "the server holds no connection once r1's is gone (its idle timeout, 10 s)", || {
        let n = last(&f, "max:kiln.quic.connections{!host:r1}")?;
        Ok((n == Some(0.0), json!(n)))
    })?;
    let gone = now_ms();
    f.start_runner_with("r1", &runner)?;
    f.until(Duration::from_secs(90), "r1 back and caught up", |s| caught_up(s, &["r1"]))?;
    until(&f, 60, "the new connection counted from its own start", || {
        let (sent, least) = total(&f, "sum:kiln.quic.packets_sent{peer:r1}.as_count()", gone)?;
        Ok((sent > 0.0 && least >= 0.0, json!({ "sent": sent, "least": least })))
    })?;
    let (_, least) = total(&f, "sum:kiln.quic.packets_sent{peer:r1}.as_count()", t0)?;
    anyhow::ensure!(least >= 0.0, "a counter's reset read as negative traffic: {least}");
    eprintln!("  the lane's figures at each end, its traffic counted where it crossed, its successor from its own start");
    Ok(())
}

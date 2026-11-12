//! CP-T2.2 step 8 (AC-T2.2.1), the kill -9 lab of kiln's events (docs/bus.md, "kiln's events"). A
//! server with an outgoing webhook subscribed to runs' changes (docs/ci.md W19-R07), a writer
//! making those changes in its kiln.db one transaction at a time (a run made, started and ended,
//! over and over; each change's event recorded with it by its trigger), and the server killed -9
//! at random points a hundred times while they're relayed and delivered (mid-delivery, mid-publish,
//! between a commit and its publish) and started again each time. At the end the endpoint's log is
//! held to every change committed: each delivered at least once, in the order committed, a
//! delivery seen twice the same change.
//!
//! The writer is this test's own thread, never killed: every change it committed it knows.
//!
//! `cargo test -p kiln --features chaos --test events_lab -- --nocapture`

#![cfg(all(unix, feature = "chaos"))]

mod common;

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{Options, http, start};
use serde_json::{Value, json};

/// Times the server is killed -9 and started again.
const KILLS: usize = 100;
/// The writer's pace: a change every so often.
const PACE: Duration = Duration::from_millis(10);

type Got = Arc<Mutex<Vec<(String, Value)>>>;

/// An endpoint answering 200 to every POST, keeping each one's delivery id and body in the order
/// they came: each request whole, its headers ended and its body all there (a server killed while it
/// wrote one leaves a piece, which is no delivery).
fn endpoint() -> anyhow::Result<(u16, Got)> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let got: Got = Arc::new(Mutex::new(Vec::new()));
    let kept = Arc::clone(&got);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let kept = Arc::clone(&kept);
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream);
                let (mut length, mut delivery, mut ended) = (0usize, String::new(), false);
                let mut line = String::new();
                loop {
                    line.clear();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    if line == "\r\n" {
                        ended = true;
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                    if let Some(v) = lower.strip_prefix("x-kiln-delivery:") {
                        delivery = v.trim().to_owned();
                    }
                }
                let mut body = vec![0; length];
                if ended
                    && reader.read_exact(&mut body).is_ok()
                    && let Ok(mut got) = kept.lock()
                {
                    got.push((delivery, serde_json::from_slice(&body).unwrap_or(Value::Null)));
                }
                let _ = reader.get_mut().write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
            });
        }
    });
    Ok((port, got))
}

/// CP-T2.2 step 8 (AC-T2.2.1): do make runs' changes in a server's kiln.db while the server is
/// killed -9 at random points 100 times and started again, its webhook subscriber listening; expect
/// every change committed delivered at least once, in the order committed.
#[test]
fn cp_t2_2_no_event_is_lost_to_100_kill_9s_of_the_server() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (port, got) = endpoint()?;
    let mut s = start(dir.path(), &Options { ci: "allow_private_targets = [\"127.0.0.1\"]", ..Options::default() })?;
    let hook = json!({ "kind": "webhook", "name": "lab", "config": { "url": format!("http://127.0.0.1:{port}/hook"), "events": ["run"] }, "secret": "lab-secret", "scope": "server" });
    let (st, body) = http(s.port, "POST", "/api/ci/integrations", &s.admin, Some(&hook))?;
    anyhow::ensure!((200..300).contains(&st), "the subscription: {st} {body}");
    let db = dir.path().join("data").join("kiln.db");
    let c = rusqlite::Connection::open(&db)?;
    c.busy_timeout(Duration::from_secs(30))?;
    // The subscription's reader stands on the topic before the first change.
    let began = Instant::now();
    while c.query_row("SELECT COUNT(*) FROM event_positions WHERE reader = 'webhooks'", [], |r| r.get::<_, i64>(0)).unwrap_or(0) == 0 {
        anyhow::ensure!(began.elapsed() < Duration::from_secs(120), "the webhooks' reader never started");
        std::thread::sleep(Duration::from_millis(50));
    }
    // Not polled: it has nothing to fetch.
    c.execute("INSERT INTO repos (id, name, clone_url, created_at, enabled) VALUES ('lab', 'acme/lab', '', 0, 0)", [])?;

    let stop = Arc::new(AtomicBool::new(false));
    let made: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let writer = {
        let (stop, made, db) = (Arc::clone(&stop), Arc::clone(&made), db.clone());
        std::thread::spawn(move || -> anyhow::Result<()> {
            let c = rusqlite::Connection::open(&db)?;
            c.busy_timeout(Duration::from_secs(30))?;
            let mut n: i64 = 0;
            while !stop.load(Ordering::SeqCst) {
                n += 1;
                let id = format!("lab{n:06}");
                for (sql, status) in [
                    // Made now: a run older than the server keeps runs would be let go before its
                    // events are read, and a run gone leaves nothing to send.
                    ("INSERT INTO runs (id, repo_id, number, pipeline, event, branch, sha, status, created_at) VALUES (?1, 'lab', ?2, 'ci', 'push', 'main', 'abc', 'queued', unixepoch())", "queued"),
                    ("UPDATE runs SET status = 'running' WHERE id = ?1 AND ?2 > 0", "running"),
                    ("UPDATE runs SET status = 'succeeded', conclusion = 'success' WHERE id = ?1 AND ?2 > 0", "succeeded"),
                ] {
                    c.execute(sql, rusqlite::params![id, n])?;
                    made.lock().map_err(|_| anyhow::anyhow!("poisoned"))?.push((id.clone(), status.to_owned()));
                    std::thread::sleep(PACE);
                }
            }
            Ok(())
        })
    };

    let started = Instant::now();
    for kill in 1..=KILLS {
        std::thread::sleep(Duration::from_millis(rand::random_range(50..1500)));
        s.restart().map_err(|e| anyhow::anyhow!("restart {kill}: {e:#}"))?;
    }
    stop.store(true, Ordering::SeqCst);
    writer.join().map_err(|_| anyhow::anyhow!("the writer panicked"))??;
    let made = made.lock().map_err(|_| anyhow::anyhow!("poisoned"))?.clone();

    // Everything committed reaches the endpoint, each change once at least, in order.
    let deadline = Instant::now() + Duration::from_secs(600);
    let (order, twice) = loop {
        let got = got.lock().map_err(|_| anyhow::anyhow!("poisoned"))?.clone();
        let mut seen: HashMap<String, (String, String)> = HashMap::new();
        let (mut order, mut twice) = (Vec::new(), 0usize);
        for (delivery, body) in &got {
            let at = (body.pointer("/run/id").and_then(Value::as_str).unwrap_or_default().to_owned(), body.pointer("/run/status").and_then(Value::as_str).unwrap_or_default().to_owned());
            match seen.insert(delivery.clone(), at.clone()) {
                Some(before) => {
                    assert_eq!(before, at, "delivery {delivery} sent twice as two changes");
                    twice += 1;
                }
                None => order.push(at),
            }
        }
        if order.len() >= made.len() || Instant::now() > deadline {
            break (order, twice);
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    // Where the kills fell, as each start found it: rows published and never recorded so, rows
    // committed and not yet published, deliveries logged and not yet sent.
    let log = std::fs::read_to_string(dir.path().join("server.log")).unwrap_or_default();
    let starts = |event: &str, field: &str| -> (usize, usize) {
        let found: Vec<u64> = log
            .lines()
            .filter(|l| l.contains(&format!("event=\"{event}\"")) || l.contains(&format!("event={event}")))
            .filter_map(|l| l.split_whitespace().find_map(|w| w.strip_prefix(&format!("{field}="))).and_then(|v| v.trim_matches('"').parse().ok()))
            .collect();
        (found.len(), found.iter().filter(|n| **n > 0).count())
    };
    let (relay_starts, published_unrecorded) = starts("relay_start", "unrecorded");
    let (_, unpublished) = starts("relay_start", "waiting");
    let (_, unsent) = starts("senders_start", "waiting");
    let lost: Vec<&(String, String)> = made.iter().filter(|m| !order.contains(m)).collect();
    if !lost.is_empty() || order != made {
        // What a failure leaves to look into: the server's directory (its log, its kiln.db), and
        // where the delivered order first went another way.
        let kept = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".cache/kiln-events-lab-failed");
        let _ = std::fs::remove_dir_all(&kept);
        let _ = Command::new("cp").args(["-a", &dir.path().to_string_lossy(), &kept.to_string_lossy()]).status();
        let at = order.iter().zip(&made).position(|(a, b)| a != b).unwrap_or(order.len().min(made.len()));
        let around = |v: &[(String, String)]| v.iter().skip(at.saturating_sub(3)).take(8).cloned().collect::<Vec<_>>();
        let raw: Vec<String> = got.lock().map_err(|_| anyhow::anyhow!("poisoned"))?.iter().map(|(d, b)| format!("{d} {} {}", b.pointer("/run/id").and_then(Value::as_str).unwrap_or("?"), b.pointer("/run/status").and_then(Value::as_str).unwrap_or("?"))).collect();
        let _ = std::fs::write(kept.join("received.txt"), raw.join("\n"));
        let _ = std::fs::write(kept.join("made.txt"), made.iter().map(|(r, s)| format!("{r} {s}")).collect::<Vec<_>>().join("\n"));
        anyhow::bail!(
            "{} of {} committed changes never delivered (the first {:?}); {} delivered first-seen, {} committed; first apart at {at}: delivered {:?}, committed {:?}; kept in {}",
            lost.len(),
            made.len(),
            lost.first(),
            order.len(),
            made.len(),
            around(&order),
            around(&made),
            kept.display()
        );
    }
    println!(
        "{KILLS} kill -9s over {:.0} s: {} changes committed, each delivered at least once and in order; {twice} delivered twice (the same delivery id). Of {relay_starts} starts, {published_unrecorded} found rows published and not recorded, {unpublished} rows committed and not published, {unsent} deliveries logged and not sent.",
        started.elapsed().as_secs_f64(),
        made.len()
    );
    Ok(())
}

//! A runner's upgrade hands its conversation stores to the new build (docs/conversations.md, "A
//! runner's upgrade"). The runner being upgraded says which store the new build goes on with first
//! (`Frame::Stores`), and one from before says nothing: the new build then opens one of its own.
//! Here, a stand-in for the runner being upgraded: one that ends before saying anything (it died, or
//! their link broke, just after the new build said it was ready), and the new build keeps a store
//! all the same, the one in its own folder, and feeds the server from it, and, never promoted, takes
//! messages again a while after (CP-T3.2, docs/sends.md, "Blue/green"); and one that hands over a
//! store the new build can't open, which it sets aside, kept whole, and goes on in a new one.
//!
//! `cargo test -p kiln --test store_handover`

#![cfg(unix)]

mod common;

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::agents::{self, Talk};
use common::{Options, Server, start};
use serde_json::{Value, json};

const NAME: &str = "green-runner";

/// One frame of the handover's socket: its length, then its JSON.
fn frame(conn: &mut std::os::unix::net::UnixStream) -> anyhow::Result<Value> {
    let mut len = [0u8; 4];
    conn.read_exact(&mut len)?;
    let mut frame = vec![0u8; usize::try_from(u32::from_be_bytes(len))?];
    conn.read_exact(&mut frame)?;
    Ok(serde_json::from_slice(&frame)?)
}

/// What the stand-in heard: the new build saying it's ready, and its answer to the store handed
/// it, if one was.
struct Heard {
    ready: Value,
    answer: Option<Value>,
}

/// A server, and a runner started as a new build taking over from a stand-in for the runner being
/// upgraded, which hears it say it's ready, hands it `handed` (that file, holding those bytes, as
/// its store) if given, then is gone: the server, the runner, its log, and what the stand-in heard.
fn taken_over_from_a_stand_in(dir: &Path, handed: Option<(PathBuf, Vec<u8>)>) -> anyhow::Result<(Server, agents::Runner, PathBuf, Heard)> {
    let s = start(dir, &Options { agent: false, ci: "", extra: "" })?;
    let sock = dir.join("handover.sock");
    let listener = std::os::unix::net::UnixListener::bind(&sock)?;
    let (heard_tx, heard) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let got = (|| -> anyhow::Result<Heard> {
            let (mut conn, _) = listener.accept()?;
            conn.set_read_timeout(Some(Duration::from_secs(60)))?;
            let ready = frame(&mut conn)?;
            let Some((file, bytes)) = handed else { return Ok(Heard { ready, answer: None }) };
            if let Some(d) = file.parent() {
                std::fs::create_dir_all(d)?;
            }
            std::fs::write(&file, bytes)?;
            let stores = serde_json::to_vec(&json!({"stores": {"own": file.to_string_lossy(), "mirror": null}}))?;
            conn.write_all(&u32::try_from(stores.len())?.to_be_bytes())?;
            conn.write_all(&stores)?;
            Ok(Heard { ready, answer: Some(frame(&mut conn)?) })
        })();
        let _ = heard_tx.send(got);
    });
    let from = sock.to_string_lossy().into_owned();
    let (runner, log) = agents::runner_with(dir, s.port, NAME, None, Talk { lines: 1, every_ms: 10 }, &[("KILN_TAKE_OVER_FROM", from.as_str())])?;
    let got = heard.recv_timeout(Duration::from_secs(150)).map_err(|_| anyhow::anyhow!("the stand-in heard nothing of the new build:\n{}", std::fs::read_to_string(&log).unwrap_or_default()))??;
    Ok((s, runner, log, got))
}

/// The id of the store runner `NAME` feeds the server from, once the server's board shows it
/// connected (failing past `within`, saying what the board and `log` say).
fn fed_store(s: &Server, log: &Path, within: Duration) -> anyhow::Result<String> {
    let deadline = Instant::now() + within;
    loop {
        let (_, body) = common::http(s.port, "GET", "/api/conversations/status", &s.admin, None)?;
        let board: Value = serde_json::from_str(&body).unwrap_or_default();
        let fed = board.get("stores").and_then(Value::as_array).into_iter().flatten().find(|st| {
            st.get("role").and_then(Value::as_str) == Some("runner") && st.get("machine").and_then(Value::as_str) == Some(NAME) && st.get("connected").and_then(Value::as_bool) == Some(true) && st.get("id").is_some_and(Value::is_string)
        });
        if let Some(id) = fed.and_then(|st| st.get("id")).and_then(Value::as_str) {
            return Ok(id.to_owned());
        }
        anyhow::ensure!(Instant::now() < deadline, "the new build fed the server no store\nboard: {body}\nrunner:\n{}", std::fs::read_to_string(log).unwrap_or_default());
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[test]
fn a_new_build_whose_old_runner_ends_before_handing_its_stores_over_keeps_one_of_its_own() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (s, _runner, log, heard) = taken_over_from_a_stand_in(dir.path(), None)?;
    assert_eq!(heard.ready.pointer("/ready/stores"), Some(&Value::Bool(true)), "it said it takes the stores over: {}", heard.ready);
    // Its store, opened in its own folder, its feed to the server's.
    let store = fed_store(&s, &log, Duration::from_secs(90))?;
    let line = std::fs::read_to_string(&log)?.lines().find(|l| l.contains("the conversations go on in this store")).map(str::to_owned).unwrap_or_default();
    assert!(line.contains(&store) && line.contains("handed=false"), "it went on in a store of its own, {store}: {line:?}");
    assert!(dir.path().join(format!("{NAME}-state")).join("conversations.db").exists(), "its store is in its own folder");
    Ok(())
}

/// CP-T3.2: a new build holds messages till it's promoted, so that a rollback never forgets what it
/// took; one whose take-over ended part-way, the runner it upgrades gone, takes them again a while
/// after (`TAKE_OVER_GRACE`), as one that took everything over does.
#[test]
fn cp_t3_2_a_new_build_whose_take_over_ends_part_way_takes_messages_again() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (_s, _runner, log, _) = taken_over_from_a_stand_in(dir.path(), None)?;
    let runner_log = || std::fs::read_to_string(&log).unwrap_or_default();
    let deadline = Instant::now() + Duration::from_secs(120);
    let line = loop {
        if let Some(l) = runner_log().lines().find(|l| l.contains("this runner takes messages again")) {
            break l.to_owned();
        }
        anyhow::ensure!(Instant::now() < deadline, "the new build still holds its messages:\n{}", runner_log());
        std::thread::sleep(Duration::from_millis(500));
    };
    assert!(line.contains("took_over=false"), "it hadn't taken everything over: {line}");
    Ok(())
}

/// A store handed over that the new build can't open (here, not a database at all) it sets aside,
/// kept whole, and goes on in a new one, saying so in its answer: the stores never hold an upgrade
/// back.
#[test]
fn a_new_build_that_cant_open_the_store_it_is_handed_sets_it_aside_and_goes_on() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("{NAME}-state")).join("conversations.db");
    let junk = vec![0xab_u8; 8192];
    let (s, _runner, log, heard) = taken_over_from_a_stand_in(dir.path(), Some((file.clone(), junk.clone())))?;
    let answer = heard.answer.unwrap_or_default();
    assert_eq!(answer.pointer("/stores_taken/kept"), Some(&Value::Bool(true)), "it went on: {answer}");
    let note = answer.pointer("/stores_taken/note").and_then(Value::as_str).unwrap_or_default();
    assert!(note.contains("set aside"), "its answer says what it did: {answer}");
    // Kept whole, beside it, where it said (for a rollback to take back), and a new store in its
    // place, fed to the server.
    let aside = answer.pointer("/stores_taken/own_aside").and_then(Value::as_str).unwrap_or_default();
    assert_eq!(Path::new(aside), dir.path().join(format!("{NAME}-state")).join("conversations.db.unopened"), "{answer}");
    assert_eq!(std::fs::read(aside)?, junk);
    fed_store(&s, &log, Duration::from_secs(90))?;
    assert_ne!(std::fs::read(&file)?, junk, "a new store took its place");
    Ok(())
}

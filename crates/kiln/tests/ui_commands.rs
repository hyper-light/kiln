//! Plan step 7 of "Nothing kiln sends between its processes is fire-and-forget" (docs/wire.md,
//! "The UI's sockets"): a viewer's commands go under ids their client makes, and each is taken once
//! however often it's sent, and answered once, `ack` or `nack`. The runner's answers come late here
//! (everything it sends waits 2 s first), so a viewer's socket drops before its command's answer
//! comes. Sent again under the same id on the client's next socket, the command isn't applied
//! again, and that socket gets its answer, once. Sent again after that, the kept answer comes at
//! once. What can't be read is answered too: `nack`, `malformed`. A command over the API under an
//! Idempotency-Key is taken once the same way.
//!
//! `cargo test -p kiln --features chaos --test ui_commands`

#![cfg(all(unix, feature = "chaos"))]

mod common;

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::agents::{self, Talk};
use common::{Options, http};
use serde_json::{Value, json};

/// A viewer's socket: what it sends, and its text frames as they come, read on a thread of their
/// own (so a wait for one never cuts a frame in two).
struct Viewer {
    to: TcpStream,
    frames: mpsc::Receiver<Value>,
}

impl Viewer {
    /// Attached to session `id`, past what it's sent on attaching.
    fn attach(port: u16, token: &str, id: &str) -> anyhow::Result<Self> {
        let mut s = agents::socket(port, token, &format!("/api/harness/sessions/{id}/ws"))?;
        agents::until_synced(&mut s)?;
        let to = s.try_clone()?;
        let (tx, frames) = mpsc::channel();
        std::thread::spawn(move || {
            while let Ok((op, payload)) = agents::frame(&mut s) {
                if op == 1
                    && let Ok(v) = serde_json::from_slice::<Value>(&payload)
                    && tx.send(v).is_err()
                {
                    return;
                }
            }
        });
        Ok(Self { to, frames })
    }

    /// Sends `text` as a client does, masked (RFC 6455, §5.2).
    fn send(&mut self, text: &str) -> anyhow::Result<()> {
        let payload = text.as_bytes();
        let mut f = vec![0x81u8];
        match payload.len() {
            n if n < 126 => f.push(0x80 | u8::try_from(n)?),
            n => {
                f.push(0x80 | 126);
                f.extend(u16::try_from(n)?.to_be_bytes());
            }
        }
        let mask = [0x37u8, 0xfa, 0x21, 0x3d];
        f.extend(mask);
        f.extend(payload.iter().zip(mask.iter().cycle()).map(|(b, m)| b ^ m));
        self.to.write_all(&f)?;
        Ok(())
    }

    /// Command `c` under `id`.
    fn command(&mut self, id: &str, c: &Value) -> anyhow::Result<()> {
        self.send(&json!({"t": "cmd", "id": id, "c": c}).to_string())
    }

    /// The answers (`ack`, `nack`) it gets within `within` to command `id` (to any, for none);
    /// the first alone when `first`.
    fn answers(&self, id: Option<&str>, within: Duration, first: bool) -> Vec<Value> {
        let deadline = Instant::now() + within;
        let mut got = Vec::new();
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            let Ok(v) = self.frames.recv_timeout(left) else { break };
            let answer = matches!(v.get("t").and_then(Value::as_str), Some("ack" | "nack"));
            if answer && id.is_none_or(|id| v.get("id").and_then(Value::as_str) == Some(id)) {
                got.push(v);
                if first {
                    break;
                }
            }
        }
        got
    }

    /// Gone, as a closed tab's socket is.
    fn drop_now(self) {
        let _ = self.to.shutdown(std::net::Shutdown::Both);
    }
}

/// How many control requests the harness got with `model`.
fn applied(controls: &Path, model: &str) -> usize {
    std::fs::read_to_string(controls).unwrap_or_default().lines().filter(|l| l.contains(&format!("\"{model}\""))).count()
}

/// `POST` `body` to `path` under an `Idempotency-Key` header: its status.
fn keyed(port: u16, token: &str, path: &str, key: &str, body: &Value) -> anyhow::Result<u16> {
    let body = body.to_string();
    let mut s = TcpStream::connect(("127.0.0.1", port))?;
    write!(
        s,
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nIdempotency-Key: {key}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    let mut answer = String::new();
    s.read_to_string(&mut answer)?;
    Ok(answer.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0))
}

#[test]
fn a_viewers_command_is_taken_once_and_answered_once_across_its_sockets() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = common::start_env(dir.path(), &Options { agent: false, ci: "", extra: "" }, false, &[])?;
    let (_runner, log) = agents::runner(dir.path(), s.port, "slow", Some("delay=2s"), Talk { lines: 1, every_ms: 10 })?;
    agents::connected(s.port, &s.admin, "slow", &log, Duration::from_secs(120))?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    let controls = work.join(".fake-claude-controls");
    std::fs::write(&controls, "")?;
    let id = agents::open_session(s.port, &s.admin, "slow", &work, Duration::from_secs(120))?;
    let set_model = |model: &str| json!({"type": "set_model", "model": model});

    // Sent, and its socket gone before the runner's answer comes (it waits 2 s).
    let mut first = Viewer::attach(s.port, &s.admin, &id)?;
    first.command("c1", &set_model("model-ui"))?;
    let early = first.answers(Some("c1"), Duration::from_millis(500), true);
    anyhow::ensure!(early.is_empty(), "answered before the runner could: {early:?}");
    first.drop_now();
    // The client's next socket sends it again under its id: applied once, answered once.
    let mut second = Viewer::attach(s.port, &s.admin, &id)?;
    second.command("c1", &set_model("model-ui"))?;
    let got = second.answers(Some("c1"), Duration::from_secs(20), true);
    anyhow::ensure!(got == [json!({"t": "ack", "id": "c1", "outcome": "taken"})], "its answer on the next socket: {got:?}\n{}", s.log());
    let more = second.answers(Some("c1"), Duration::from_secs(3), false);
    anyhow::ensure!(more.is_empty(), "answered more than once: {more:?}");
    anyhow::ensure!(applied(&controls, "model-ui") == 1, "applied {} times", applied(&controls, "model-ui"));
    // Sent again after its answer: the kept answer, at once, and nothing applied.
    let t = Instant::now();
    second.command("c1", &set_model("model-ui"))?;
    let kept = second.answers(Some("c1"), Duration::from_secs(5), true);
    anyhow::ensure!(kept == [json!({"t": "ack", "id": "c1", "outcome": "taken"})] && t.elapsed() < Duration::from_millis(1500), "the kept answer, at once: {kept:?} in {:?}", t.elapsed());
    std::thread::sleep(Duration::from_secs(3));
    anyhow::ensure!(applied(&controls, "model-ui") == 1, "applied again: {} times", applied(&controls, "model-ui"));

    // What can't be read is answered: under its id when that much could be read.
    second.send("not json")?;
    let bad = second.answers(None, Duration::from_secs(5), true);
    anyhow::ensure!(bad.first().is_some_and(|v| v.get("t") == Some(&json!("nack")) && v.get("code") == Some(&json!("malformed")) && v.get("id") == Some(&Value::Null)), "{bad:?}");
    second.command("c2", &json!({"type": "no_such_command"}))?;
    let bad = second.answers(Some("c2"), Duration::from_secs(5), true);
    anyhow::ensure!(bad.first().is_some_and(|v| v.get("t") == Some(&json!("nack")) && v.get("code") == Some(&json!("malformed"))), "{bad:?}");

    // Over the API under a key: taken once, the same.
    let command = format!("/api/harness/sessions/{id}/command");
    let st = keyed(s.port, &s.admin, &command, "k-1", &set_model("model-api"))?;
    anyhow::ensure!(st == 204, "the first: {st}");
    let st = keyed(s.port, &s.admin, &command, "k-1", &set_model("model-api"))?;
    anyhow::ensure!(st == 204, "asked again: {st}");
    std::thread::sleep(Duration::from_secs(3));
    anyhow::ensure!(applied(&controls, "model-api") == 1, "applied {} times over the API", applied(&controls, "model-api"));
    // And without a key, as before: each one applied.
    let (st, body) = http(s.port, "POST", &command, &s.admin, Some(&set_model("model-api")))?;
    anyhow::ensure!(st == 204, "{st} {body}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while applied(&controls, "model-api") < 2 {
        anyhow::ensure!(Instant::now() < deadline, "a command without a key wasn't applied");
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

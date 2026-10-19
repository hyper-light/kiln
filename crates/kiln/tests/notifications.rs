//! W19-AC05 (docs/ci.md): each event fires its rule, `throttle` and `run.fixed` work, and the
//! Slack, Discord, email and webhook channels deliver, here to local fakes: incoming-webhook URLs
//! for Slack and Discord, a signed webhook, and an SMTP server. A diff's events reach the inboxes
//! of the people they're about, who read them on the review inbox.

#![cfg(unix)]

mod common;

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{Options, Server, http, start};
use serde_json::{Value, json};

type Seen = Arc<Mutex<Vec<String>>>;

fn git(home: &Path, dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let o = Command::new("git")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "init.defaultBranch=main", "-c", "credential.helper="])
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .output()?;
    let out = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    anyhow::ensure!(o.status.success(), "git {args:?}: {out}");
    Ok(out)
}

/// A user with `grants`, and a personal access token that may do what they may.
fn person(s: &Server, name: &str, grants: &[&str]) -> anyhow::Result<String> {
    let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": name, "grants": grants })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}: {st} {body}");
    let (st, body) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&json!({ "kind": "pat", "user": name, "grants": ["owner"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}'s token: {st} {body}");
    serde_json::from_str::<Value>(&body)?.get("token").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no token in {body}"))
}

fn until(secs: u64, what: &str, mut f: impl FnMut() -> anyhow::Result<Option<String>>) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        match f()? {
            None => return Ok(()),
            Some(last) => anyhow::ensure!(Instant::now() < deadline, "{what}: {last}"),
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// An HTTP server that answers 200 to everything, keeping each request as its request line,
/// headers and body.
fn web_fake() -> anyhow::Result<(u16, Seen)> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let seen: Seen = Arc::default();
    let keep = Arc::clone(&seen);
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            let whole = loop {
                let Ok(n) = stream.read(&mut chunk) else { break None };
                if n == 0 {
                    break None;
                }
                buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
                let text = String::from_utf8_lossy(&buf).into_owned();
                if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                    let len = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok())).unwrap_or(0);
                    if rest.len() >= len {
                        break Some(text);
                    }
                }
            };
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            if let (Some(t), Ok(mut g)) = (whole, keep.lock()) {
                g.push(t);
            }
        }
    });
    Ok((port, seen))
}

/// An SMTP server that takes every message, keeping each as its envelope and data.
fn smtp_fake() -> anyhow::Result<(u16, Seen)> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let seen: Seen = Arc::default();
    let keep = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let keep = Arc::clone(&keep);
            std::thread::spawn(move || {
                let Ok(mut out) = stream.try_clone() else { return };
                let mut lines = BufReader::new(stream);
                let mut say = |s: &str| out.write_all(s.as_bytes()).is_ok();
                if !say("220 fake ESMTP\r\n") {
                    return;
                }
                let mut mail = String::new();
                let mut line = String::new();
                loop {
                    line.clear();
                    if lines.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let verb = line.split_whitespace().next().unwrap_or_default().to_ascii_uppercase();
                    let ok = match verb.as_str() {
                        "EHLO" | "HELO" => say("250-fake\r\n250 8BITMIME\r\n"),
                        "MAIL" | "RCPT" => {
                            mail.push_str(&line);
                            say("250 OK\r\n")
                        }
                        "DATA" => {
                            say("354 go on\r\n");
                            loop {
                                line.clear();
                                if lines.read_line(&mut line).unwrap_or(0) == 0 {
                                    return;
                                }
                                if line == ".\r\n" {
                                    break;
                                }
                                mail.push_str(&line);
                            }
                            if let Ok(mut g) = keep.lock() {
                                g.push(std::mem::take(&mut mail));
                            }
                            say("250 queued\r\n")
                        }
                        "QUIT" => {
                            say("221 bye\r\n");
                            return;
                        }
                        _ => say("502 what\r\n"),
                    };
                    if !ok {
                        return;
                    }
                }
            });
        }
    });
    Ok((port, seen))
}

fn seen(s: &Seen) -> Vec<String> {
    s.lock().map(|g| g.clone()).unwrap_or_default()
}

/// The finished runs of `.kiln/flip.yaml`, newest first: (sha, conclusion).
fn flips(s: &Server) -> anyhow::Result<Vec<(String, String)>> {
    let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/notes&limit=50", &s.admin, None)?;
    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    Ok(v.get("runs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|r| r.get("workflow").and_then(Value::as_str).is_some_and(|w| w.ends_with("flip.yaml")) && r.get("finished_at").is_some_and(|f| !f.is_null()))
        .map(|r| (r.get("sha").and_then(Value::as_str).unwrap_or_default().to_owned(), r.get("conclusion").and_then(Value::as_str).unwrap_or_default().to_owned()))
        .collect())
}

fn inbox(s: &Server, token: &str) -> anyhow::Result<Value> {
    let (st, body) = http(s.port, "GET", "/api/review/inbox", token, None)?;
    anyhow::ensure!(st == 200, "inbox: {st} {body}");
    Ok(serde_json::from_str(&body)?)
}

fn events_in(v: &Value) -> Vec<String> {
    v.get("notifications").and_then(Value::as_array).into_iter().flatten().filter_map(|n| n.get("event").and_then(Value::as_str)).map(str::to_owned).collect()
}

#[test]
fn w19_ac05_notifications() -> anyhow::Result<()> {
    let (web, posts) = web_fake()?;
    let (smtp, mails) = smtp_fake()?;
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true\nallow_private_targets = [\"127.0.0.1\"]", ..Options::default() })?;
    let integration = |kind: &str, name: &str, config: Value, secret: Option<String>| -> anyhow::Result<()> {
        let (st, body) = http(s.port, "POST", "/api/ci/integrations", &s.admin, Some(&json!({ "kind": kind, "name": name, "config": config, "secret": secret, "scope": "server" })))?;
        anyhow::ensure!((200..300).contains(&st), "{kind}: {st} {body}");
        Ok(())
    };
    integration("slack", "chat", json!({}), Some(format!("http://127.0.0.1:{web}/slack")))?;
    integration("discord", "team", json!({}), Some(format!("http://127.0.0.1:{web}/discord")))?;
    integration("webhook", "hook", json!({ "url": format!("http://127.0.0.1:{web}/hook") }), Some("s3cret".into()))?;
    integration("email", "mail", json!({ "host": "127.0.0.1", "port": smtp.to_string(), "tls": "none", "from": "kiln <ci@kiln.test>" }), None)?;

    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/notes", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    let dev = person(&s, "dev", &["developer@repo:acme/notes"])?;
    let rev = person(&s, "rev", &["developer@repo:acme/notes"])?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    git(&home, dir.path(), &["clone", "-q", &format!("http://x:{dev}@127.0.0.1:{}/git/acme/notes.git", s.port), "work"])?;
    let work = dir.path().join("work");
    // The trunk moves as the owner moves it (only the land queue writes it for others); dev
    // submits a diff.
    let owner = format!("http://x:{}@127.0.0.1:{}/git/acme/notes.git", s.admin, s.port);
    std::fs::write(work.join("README.md"), "notes\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &owner, "HEAD:main"])?;
    until(60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;

    // The rules, and a workflow that fails while FAIL is there.
    std::fs::create_dir_all(work.join(".kiln"))?;
    std::fs::write(
        work.join(".kiln/notify.yaml"),
        "- on: [run.failed]\n  to: [slack:chat, webhook:hook]\n  throttle: 10m\n- on: [run.fixed]\n  to: [discord:team]\n- on: [review.requested, comment.reply, mention]\n  to: [inbox, email:rev@kiln.test]\n",
    )?;
    std::fs::write(work.join(".kiln/flip.yaml"), "on: push\njobs:\n  check:\n    runs-on: local\n    steps:\n      - run: test ! -f FAIL\n")?;
    std::fs::write(work.join("FAIL"), "1\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "rules, and a failure"])?;
    git(&home, &work, &["push", "-q", &owner, "HEAD:main"])?;
    let failed1 = git(&home, &work, &["rev-parse", "HEAD"])?.trim().to_owned();
    until(240, "run.failed at Slack and the webhook", || {
        let p = seen(&posts);
        let slack = p.iter().any(|r| r.starts_with("POST /slack") && r.contains("failed on main"));
        let hook = p.iter().any(|r| r.starts_with("POST /hook") && r.contains("\"event\":\"run.failed\""));
        Ok((!(slack && hook)).then(|| format!("{p:?}\n{:?}\n{}", flips(&s).unwrap_or_default(), s.log())))
    })?;
    let hook = seen(&posts).into_iter().find(|r| r.starts_with("POST /hook")).unwrap_or_default();
    assert!(hook.to_ascii_lowercase().contains("x-kiln-signature-256: sha256="), "the webhook is signed: {hook}");
    assert!(flips(&s)?.iter().any(|(sha, c)| *sha == failed1 && c == "failure"));

    // A second failure inside the throttle: no second message. Then the fix: run.fixed.
    std::fs::write(work.join("README.md"), "notes, again\n")?;
    git(&home, &work, &["commit", "-qam", "still failing"])?;
    git(&home, &work, &["push", "-q", &owner, "HEAD:main"])?;
    let failed2 = git(&home, &work, &["rev-parse", "HEAD"])?.trim().to_owned();
    until(240, "the second failure", || Ok((!flips(&s)?.iter().any(|(sha, _)| *sha == failed2)).then(|| format!("{:?}", flips(&s)))))?;
    git(&home, &work, &["rm", "-q", "FAIL"])?;
    git(&home, &work, &["commit", "-qm", "fixed"])?;
    git(&home, &work, &["push", "-q", &owner, "HEAD:main"])?;
    until(240, "run.fixed at Discord", || {
        let p = seen(&posts);
        Ok((!p.iter().any(|r| r.starts_with("POST /discord") && r.contains("is fixed on main"))).then(|| format!("{p:?}\n{:?}", flips(&s).unwrap_or_default())))
    })?;
    let p = seen(&posts);
    assert_eq!(p.iter().filter(|r| r.starts_with("POST /slack")).count(), 1, "throttled: {p:?}");
    assert_eq!(p.iter().filter(|r| r.starts_with("POST /hook")).count(), 1, "throttled: {p:?}");

    // A diff for rev to review: review.requested. rev's comment names dev (a reply on dev's diff,
    // and a mention); dev's answer in the thread is a reply to rev.
    git(&home, &work, &["checkout", "-qb", "change"])?;
    std::fs::write(work.join("notes.txt"), "a note\n")?;
    git(&home, &work, &["add", "notes.txt"])?;
    git(&home, &work, &["commit", "-qm", "A note\n\nChange-Id: I4444444444444444444444444444444444444444"])?;
    git(&home, &work, &["push", "-q", "-o", "reviewer=rev", "origin", "HEAD:refs/for/main"])?;
    until(60, "review.requested in rev's inbox", || {
        let v = inbox(&s, &rev)?;
        Ok((!events_in(&v).iter().any(|e| e == "review.requested")).then(|| v.to_string()))
    })?;
    let (st, body) = http(s.port, "POST", "/api/diffs/D1/comments", &rev, Some(&json!({ "body": "Is this right, @dev?", "publish": true })))?;
    anyhow::ensure!(st == 200, "rev's comment: {st} {body}");
    let thread = serde_json::from_str::<Value>(&body)?.get("thread").and_then(Value::as_str).unwrap_or_default().to_owned();
    until(60, "comment.reply and mention in dev's inbox", || {
        let v = inbox(&s, &dev)?;
        let e = events_in(&v);
        Ok((!(e.iter().any(|x| x == "comment.reply") && e.iter().any(|x| x == "mention"))).then(|| v.to_string()))
    })?;
    let (st, body) = http(s.port, "POST", "/api/diffs/D1/comments", &dev, Some(&json!({ "body": "It is.", "thread": thread, "publish": true })))?;
    anyhow::ensure!(st == 200, "dev's answer: {st} {body}");
    until(60, "dev's answer in rev's inbox", || {
        let v = inbox(&s, &rev)?;
        Ok((!events_in(&v).iter().any(|e| e == "comment.reply")).then(|| v.to_string()))
    })?;
    // Nobody hears of what they did themselves; rev reads everything.
    let v = inbox(&s, &rev)?;
    assert!(!events_in(&v).iter().any(|e| e == "mention"), "rev mentioned nobody: {v}");
    assert!(v.get("unread").and_then(Value::as_i64).is_some_and(|n| n >= 2), "{v}");
    let (st, body) = http(s.port, "POST", "/api/review/inbox/read", &rev, Some(&json!({ "all": true })))?;
    anyhow::ensure!(st == 200, "read: {st} {body}");
    assert_eq!(inbox(&s, &rev)?.get("unread").and_then(Value::as_i64), Some(0));
    assert!(inbox(&s, &dev)?.get("unread").and_then(Value::as_i64).is_some_and(|n| n >= 2), "dev's are dev's own");

    // The email channel: a message for each of the three, from kiln, to the rule's address.
    until(60, "the emails", || {
        let m = seen(&mails);
        Ok((m.len() < 4).then(|| format!("{m:?}")))
    })?;
    let m = seen(&mails);
    assert!(m.iter().all(|x| x.contains("MAIL FROM:<ci@kiln.test>") && x.contains("RCPT TO:<rev@kiln.test>") && x.contains("From: kiln <ci@kiln.test>")), "{m:?}");
    // Every delivery is recorded, and went.
    let (_, body) = http(s.port, "GET", "/api/ci/notify/deliveries?limit=100", &s.admin, None)?;
    let d: Value = serde_json::from_str(&body)?;
    let rows: Vec<Value> = d.get("deliveries").and_then(Value::as_array).cloned().unwrap_or_default();
    for channel in ["slack:chat", "webhook:hook", "discord:team", "inbox", "email:rev@kiln.test"] {
        assert!(rows.iter().any(|r| r.get("channel").and_then(Value::as_str) == Some(channel) && r.get("status").and_then(Value::as_str) == Some("sent")), "{channel}: {rows:?}");
    }
    assert!(!rows.iter().any(|r| r.get("status").and_then(Value::as_str) == Some("failed")), "{rows:?}");
    Ok(())
}

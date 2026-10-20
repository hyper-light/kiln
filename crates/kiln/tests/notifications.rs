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

/// W19-R02: a finished run is a trace at an OTLP/HTTP receiver (here a local one): the run, its
/// leg and its steps as spans, each under the one it's part of, with the integration's header.
/// Verify sends the receiver an empty export.
#[test]
fn w19_r02_runs_reach_an_otlp_receiver() -> anyhow::Result<()> {
    let (web, posts) = web_fake()?;
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true\nallow_private_targets = [\"127.0.0.1\"]", ..Options::default() })?;
    let (st, body) = http(s.port, "POST", "/api/ci/integrations", &s.admin, Some(&json!({ "kind": "otel", "name": "traces", "config": { "endpoint": format!("http://127.0.0.1:{web}"), "service_name": "acme-ci" }, "secret": "x-api-key: k1", "scope": "server" })))?;
    anyhow::ensure!((200..300).contains(&st), "otel: {st} {body}");
    let (st, body) = http(s.port, "POST", "/api/ci/integrations/traces/verify", &s.admin, None)?;
    anyhow::ensure!(st == 200 && body.contains("\"ok\":true"), "verify: {st} {body}");
    assert!(seen(&posts).iter().any(|r| r.starts_with("POST /v1/traces") && r.contains("{\"resourceSpans\":[]}")), "verify's empty export: {:?}", seen(&posts));

    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/traced", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    let owner = format!("http://x:{}@127.0.0.1:{}/git/acme/traced.git", s.admin, s.port);
    std::fs::write(work.join("README.md"), "traced\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &owner, "HEAD:main"])?;
    until(60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    std::fs::write(work.join(".kiln/traced.yaml"), "on: push\njobs:\n  build:\n    runs-on: local\n    steps:\n      - name: compile\n        run: echo compiled\n      - name: test\n        run: echo tested\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "traced"])?;
    git(&home, &work, &["push", "-q", &owner, "HEAD:main"])?;
    let mut export = String::new();
    until(240, "the run's trace", || {
        export = seen(&posts).into_iter().find(|r| r.starts_with("POST /v1/traces") && r.contains("\"compile\"")).unwrap_or_default();
        Ok(export.is_empty().then(|| format!("{:?}\n{}", seen(&posts), s.log())))
    })?;
    assert!(export.to_ascii_lowercase().contains("x-api-key: k1"), "the stored header: {export}");
    let body: Value = serde_json::from_str(export.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or_default())?;
    let spans: Vec<Value> = body.pointer("/resourceSpans/0/scopeSpans/0/spans").and_then(Value::as_array).cloned().unwrap_or_default();
    let named = |n: &str| spans.iter().find(|s| s.get("name").and_then(Value::as_str) == Some(n)).cloned().unwrap_or_default();
    let (run, leg, compile, test) = (spans.first().cloned().unwrap_or_default(), named("build"), named("compile"), named("test"));
    assert!(run.get("parentSpanId").is_none() && run.pointer("/status/code") == Some(&json!(1)), "the run, a success: {body}");
    assert_eq!(leg.get("parentSpanId"), run.get("spanId"), "the leg is the run's: {body}");
    assert!(compile.get("parentSpanId") == leg.get("spanId") && test.get("parentSpanId") == leg.get("spanId"), "the steps are the leg's: {body}");
    assert_eq!(body.pointer("/resourceSpans/0/resource/attributes/0/value/stringValue"), Some(&json!("acme-ci")));
    let attrs = run.get("attributes").map(Value::to_string).unwrap_or_default();
    assert!(attrs.contains("vcs.repository.name") && attrs.contains("acme/traced") && attrs.contains("cicd.pipeline.result"), "{attrs}");
    Ok(())
}

/// W19-R06's per-person rules: dev keeps a rule of their own (a failed run that's about them, to
/// their inbox and their email). dev's failing run reaches both; the owner's failing run isn't
/// about dev, so neither hears of it. A rule for a channel that isn't a person's, and an agent
/// changing its person's rules, are refused.
#[test]
fn w19_r06_a_persons_own_rules() -> anyhow::Result<()> {
    let (smtp, mails) = smtp_fake()?;
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true\nallow_private_targets = [\"127.0.0.1\"]", ..Options::default() })?;
    let (st, body) = http(s.port, "POST", "/api/ci/integrations", &s.admin, Some(&json!({ "kind": "email", "name": "mail", "config": { "host": "127.0.0.1", "port": smtp.to_string(), "tls": "none", "from": "kiln <ci@kiln.test>" }, "scope": "server" })))?;
    anyhow::ensure!((200..300).contains(&st), "email: {st} {body}");
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/mine", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    let dev = person(&s, "dev", &["developer@repo:acme/mine"])?;
    // The owner's are the server's.
    let (st, body) = http(s.port, "GET", "/api/me/notifications", &s.admin, None)?;
    anyhow::ensure!(st == 200 && body.contains("\"person\":false"), "the owner's: {st} {body}");
    let (st, body) = http(s.port, "PUT", "/api/me/notifications", &dev, Some(&json!({ "rules": [{ "on": ["run.failed"], "to": ["slack:#ci"] }] })))?;
    anyhow::ensure!(st == 400 && body.contains("inbox"), "a channel isn't a person's: {st} {body}");
    let (st, body) = http(s.port, "PUT", "/api/me/notifications", &dev, Some(&json!({ "email": "dev@kiln.test", "rules": [{ "on": ["run.failed"], "to": ["inbox", "email"], "about_me": true }] })))?;
    anyhow::ensure!(st == 200, "dev's rule: {st} {body}");
    let mine: Value = serde_json::from_str(&http(s.port, "GET", "/api/me/notifications", &dev, None)?.1)?;
    assert_eq!(mine.get("email"), Some(&json!("dev@kiln.test")), "{mine}");
    assert_eq!(mine.pointer("/rules/0/about_me"), Some(&json!(true)), "{mine}");
    assert!(mine.get("events").and_then(Value::as_array).is_some_and(|e| e.contains(&json!("run.failed")) && !e.contains(&json!("agent.finished"))), "{mine}");
    let (st, body) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&json!({ "kind": "agent", "agent": "helper", "user": "dev", "grants": ["developer@repo:acme/mine"] })))?;
    anyhow::ensure!((200..300).contains(&st), "agent token: {st} {body}");
    let agent = serde_json::from_str::<Value>(&body)?.get("token").and_then(Value::as_str).unwrap_or_default().to_owned();
    let (st, _) = http(s.port, "PUT", "/api/me/notifications", &agent, Some(&json!({ "rules": [] })))?;
    assert_eq!(st, 403, "an agent doesn't change its person's rules");

    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    let url = |t: &str| format!("http://x:{t}@127.0.0.1:{}/git/acme/mine.git", s.port);
    std::fs::write(work.join("README.md"), "mine\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &url(&s.admin), "HEAD:main"])?;
    until(60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    std::fs::write(work.join(".kiln/fails.yaml"), "on: push\njobs:\n  check:\n    runs-on: local\n    steps:\n      - run: exit 1\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "fails"])?;
    // The owner's failing run first: not about dev.
    git(&home, &work, &["push", "-q", &url(&s.admin), "HEAD:main"])?;
    let finished = |branch: &str| -> anyhow::Result<bool> {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/mine&limit=20", &s.admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        Ok(v.get("runs").and_then(Value::as_array).into_iter().flatten().any(|r| r.get("branch").and_then(Value::as_str) == Some(branch) && r.get("conclusion").and_then(Value::as_str) == Some("failure")))
    };
    until(240, "the owner's run fails", || Ok((!finished("main")?).then(|| s.log())))?;
    // Then dev's, on a branch of theirs.
    git(&home, &work, &["checkout", "-qb", "dev-work"])?;
    std::fs::write(work.join("README.md"), "dev's\n")?;
    git(&home, &work, &["commit", "-qam", "dev's change"])?;
    git(&home, &work, &["push", "-q", &url(&dev), "HEAD:refs/heads/dev-work"])?;
    until(240, "dev's run fails", || Ok((!finished("dev-work")?).then(|| s.log())))?;
    until(60, "dev hears of their run", || {
        let n = inbox(&s, &dev)?;
        let failed: Vec<String> = n.get("notifications").and_then(Value::as_array).into_iter().flatten().filter(|x| x.get("event").and_then(Value::as_str) == Some("run.failed")).filter_map(|x| x.get("title").and_then(Value::as_str).map(str::to_owned)).collect();
        Ok((failed.len() != 1 || !failed.iter().all(|t| t.contains("dev-work"))).then(|| format!("{n}\n{:?}", seen(&mails))))
    })?;
    // By email too, once: the owner's run wasn't about dev (its subject is encoded; its recipient isn't).
    until(60, "and by email", || {
        let m = seen(&mails);
        Ok((!m.iter().any(|x| x.contains("dev@kiln.test"))).then(|| format!("{m:?}")))
    })?;
    std::thread::sleep(Duration::from_secs(2));
    let to_dev = seen(&mails).iter().filter(|x| x.contains("dev@kiln.test")).count();
    assert_eq!(to_dev, 1, "{:?}", seen(&mails));
    Ok(())
}

/// A fake Linear (GraphQL at `/graphql`) and Jira Cloud (REST at `/rest/api/3`), answering as
/// each would, keeping each request as its request line, headers and body.
fn tracker_fake() -> anyhow::Result<(u16, Seen)> {
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
            let Some(text) = whole else { continue };
            let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
            let path = head.split_whitespace().nth(1).unwrap_or_default();
            let answer = match path {
                "/graphql" if body.contains("viewer") => json!({ "data": { "viewer": { "name": "kiln-ci" } } }),
                "/graphql" if body.contains("issue(id") => {
                    let key = serde_json::from_str::<Value>(body).ok().and_then(|v| v.pointer("/variables/id").and_then(Value::as_str).map(str::to_owned)).unwrap_or_default();
                    json!({ "data": { "issue": { "id": format!("lin-{key}") } } })
                }
                "/graphql" if body.contains("commentCreate") => json!({ "data": { "commentCreate": { "success": true } } }),
                "/graphql" if body.contains("attachmentLinkURL") => json!({ "data": { "attachmentLinkURL": { "success": true } } }),
                "/graphql" if body.contains("teams(") => json!({ "data": { "teams": { "nodes": [{ "id": "team-eng" }] } } }),
                "/graphql" if body.contains("issueCreate") => json!({ "data": { "issueCreate": { "success": true, "issue": { "identifier": "ENG-100" } } } }),
                "/rest/api/3/myself" => json!({ "displayName": "kiln-ci" }),
                "/rest/api/3/issue" => json!({ "key": "OPS-100" }),
                p if p.starts_with("/rest/api/3/issue/") => json!({ "id": "1" }),
                _ => json!({ "errors": [{ "message": format!("nothing at {path}") }] }),
            };
            let out = answer.to_string();
            let _ = stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}", out.len()).as_bytes());
            if let Ok(mut g) = keep.lock() {
                g.push(text);
            }
        }
    });
    Ok((port, seen))
}

/// W11-R07: Linear and Jira as channels. A failing trunk run comments on and links the issues its
/// commit's trailers name, each tracker only its own keys; a run naming none is skipped there; a
/// test the trunk quarantines gets an issue, or a comment on the issue its entry names; a landed
/// diff tells the issues its message names.
#[test]
fn w11_r07_issue_trackers() -> anyhow::Result<()> {
    let (port, asked) = tracker_fake()?;
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true\nallow_private_targets = [\"127.0.0.1\"]", ..Options::default() })?;
    let integration = |kind: &str, name: &str, config: Value, secret: &str| -> anyhow::Result<()> {
        let (st, body) = http(s.port, "POST", "/api/ci/integrations", &s.admin, Some(&json!({ "kind": kind, "name": name, "config": config, "secret": secret, "scope": "server" })))?;
        anyhow::ensure!((200..300).contains(&st), "{kind}: {st} {body}");
        let (st, body) = http(s.port, "POST", &format!("/api/ci/integrations/{name}/verify"), &s.admin, None)?;
        anyhow::ensure!(st == 200 && body.contains("as kiln-ci"), "{kind}'s Verify says who it is: {st} {body}");
        Ok(())
    };
    integration("linear", "eng", json!({ "api": format!("http://127.0.0.1:{port}/graphql"), "prefixes": "ENG", "team": "ENG" }), "lin_api_test")?;
    integration("jira", "ops", json!({ "site": format!("http://127.0.0.1:{port}"), "email": "ci@acme.test", "prefixes": "OPS", "project": "OPS" }), "jira-token")?;
    // What the fakes were asked at `path`, whose request has `has`.
    let asked_at = |path: &str, has: &str| -> Vec<String> { seen(&asked).into_iter().filter(|r| r.split_whitespace().nth(1) == Some(path) && r.contains(has)).collect() };

    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/track", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    let dev = person(&s, "dev", &["developer@repo:acme/track"])?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    git(&home, dir.path(), &["clone", "-q", &format!("http://x:{dev}@127.0.0.1:{}/git/acme/track.git", s.port), "work"])?;
    let work = dir.path().join("work");
    let owner = format!("http://x:{}@127.0.0.1:{}/git/acme/track.git", s.admin, s.port);
    std::fs::write(work.join("README.md"), "track\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &owner, "HEAD:main"])?;
    until(60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;

    // The rules, a workflow that fails while FAIL is there, and a commit naming three issues.
    std::fs::create_dir_all(work.join(".kiln"))?;
    std::fs::write(work.join(".kiln/notify.yaml"), "- on: [run.failed]\n  to: [linear, jira]\n- on: [test.quarantined]\n  to: [linear, jira]\n- on: [diff.landed]\n  to: [linear]\n")?;
    std::fs::write(work.join(".kiln/review.yaml"), "land:\n  approvals: 0\n")?;
    std::fs::write(work.join(".kiln/flip.yaml"), "on: push\njobs:\n  check:\n    runs-on: local\n    steps:\n      - run: test ! -f FAIL\n")?;
    std::fs::write(work.join("FAIL"), "1\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "Break the build\n\nFixes: ENG-7\nRefs: OPS-3, WEB-1"])?;
    git(&home, &work, &["push", "-q", &owner, "HEAD:main"])?;
    until(240, "the failure at Linear and Jira", || {
        let linear = !asked_at("/graphql", "lin-ENG-7").is_empty() && asked_at("/graphql", "attachmentLinkURL").iter().any(|r| r.contains("lin-ENG-7"));
        let jira = !asked_at("/rest/api/3/issue/OPS-3/comment", "").is_empty() && !asked_at("/rest/api/3/issue/OPS-3/remotelink", "").is_empty();
        Ok((!(linear && jira)).then(|| format!("{:?}", seen(&asked))))
    })?;
    let comment = asked_at("/graphql", "commentCreate").into_iter().find(|r| r.contains("lin-ENG-7")).unwrap_or_default();
    assert!(comment.contains("failed on main") && comment.to_ascii_lowercase().contains("authorization: lin_api_test"), "Linear's comment, with its key: {comment}");
    let link = asked_at("/rest/api/3/issue/OPS-3/remotelink", "").concat();
    assert!(link.contains("\"globalId\"") && link.contains("/runs/"), "the run, linked once however often: {link}");
    use base64::Engine as _;
    let basic = format!("authorization: basic {}", base64::engine::general_purpose::STANDARD.encode("ci@acme.test:jira-token")).to_ascii_lowercase();
    assert!(link.to_ascii_lowercase().contains(&basic), "Jira's API token, with its account: {link}");
    assert!(asked_at("/rest/api/3/issue/WEB-1/comment", "").is_empty() && asked_at("/graphql", "\"OPS-3\"").is_empty(), "each tracker its own keys: {:?}", seen(&asked));

    // A failure naming no issue: the trackers have nothing to do, and say so.
    let deliveries = || -> anyhow::Result<Vec<Value>> {
        let (_, body) = http(s.port, "GET", "/api/ci/notify/deliveries?limit=100", &s.admin, None)?;
        Ok(serde_json::from_str::<Value>(&body)?.get("deliveries").and_then(Value::as_array).cloned().unwrap_or_default())
    };
    std::fs::write(work.join("README.md"), "track, again\n")?;
    git(&home, &work, &["commit", "-qam", "Still broken"])?;
    git(&home, &work, &["push", "-q", &owner, "HEAD:main"])?;
    until(240, "the second failure, skipped by both", || {
        let d = deliveries()?;
        let skipped = d.iter().filter(|x| x.get("status") == Some(&json!("skipped")) && x.get("error").and_then(Value::as_str).is_some_and(|e| e.contains("names no issue"))).count();
        Ok((skipped < 2).then(|| format!("{d:?}")))
    })?;
    let sent: Vec<String> = deliveries()?.iter().filter(|x| x.get("status") == Some(&json!("sent"))).filter_map(|x| x.get("channel").and_then(Value::as_str).map(str::to_owned)).collect();
    assert!(sent.contains(&"linear".to_owned()) && sent.contains(&"jira".to_owned()), "the first failure went to both: {sent:?}");

    // Two tests quarantined: an issue each at Linear; at Jira, one opened and a comment on the
    // issue the other names. The fix lands with them.
    git(&home, &work, &["rm", "-q", "FAIL"])?;
    std::fs::write(
        work.join(".kiln/quarantine.yaml"),
        "- test: \"calc::rounds\"\n  reason: races the clock\n- test: \"net::retries\"\n  reason: the sandbox has no network\n  issue: https://acme.atlassian.net/browse/OPS-5\n",
    )?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "Quarantine two tests"])?;
    git(&home, &work, &["push", "-q", &owner, "HEAD:main"])?;
    until(120, "the quarantined tests' issues", || {
        let jira_opened = !asked_at("/rest/api/3/issue", "calc::rounds").is_empty();
        let jira_commented = !asked_at("/rest/api/3/issue/OPS-5/comment", "net::retries").is_empty();
        let linear_opened = asked_at("/graphql", "issueCreate").len() >= 2;
        Ok((!(jira_opened && jira_commented && linear_opened)).then(|| format!("{:?}", seen(&asked))))
    })?;
    assert!(asked_at("/rest/api/3/issue", "net::retries").is_empty(), "the issue it names, not a new one: {:?}", seen(&asked));
    let opened = asked_at("/rest/api/3/issue", "calc::rounds").concat();
    assert!(opened.contains("\"key\":\"OPS\"") && opened.contains("\"name\":\"Bug\""), "in the integration's project, a bug: {opened}");

    // A diff whose message closes ENG-9 lands: Linear hears of it there.
    git(&home, &work, &["checkout", "-qb", "notes"])?;
    std::fs::write(work.join("notes.txt"), "a note\n")?;
    git(&home, &work, &["add", "notes.txt"])?;
    git(&home, &work, &["commit", "-qm", &format!("Add notes\n\nCloses: ENG-9\nChange-Id: I{}", "9".repeat(40))])?;
    git(&home, &work, &["push", "-q", "origin", "HEAD:refs/for/main"])?;
    until(60, "D1", || {
        let (st, body) = http(s.port, "GET", "/api/diffs/D1", &s.admin, None)?;
        Ok((st != 200).then_some(body))
    })?;
    let (st, body) = http(s.port, "POST", "/api/diffs/D1/land", &s.admin, Some(&json!({})))?;
    anyhow::ensure!(st == 200, "land: {st} {body}");
    until(180, "D1 landed, at Linear", || {
        let comments = asked_at("/graphql", "commentCreate").into_iter().filter(|r| r.contains("lin-ENG-9") && r.contains("D1 Add notes landed on main")).count();
        let links = asked_at("/graphql", "attachmentLinkURL").into_iter().filter(|r| r.contains("lin-ENG-9") && r.contains("/diffs/D1")).count();
        Ok((comments == 0 || links == 0).then(|| format!("{:?}\n{}", seen(&asked), s.log())))
    })?;
    Ok(())
}

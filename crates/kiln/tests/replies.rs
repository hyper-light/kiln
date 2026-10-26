//! W14-R05 (docs/ci.md): replies by email. A reviewer whose own rule emails them `comment.reply`
//! gets a message about their thread with a reply address only they have; their answer, read
//! from the replies mailbox over IMAP, is posted to the thread as theirs, without what it quotes.
//! The same address used from anyone else's mailbox posts nothing. Against a stand-in SMTP server
//! and a stand-in IMAP server, both here.

#![cfg(unix)]

mod common;

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{Options, Server, http, start};
use serde_json::{Value, json};

fn git(home: &Path, dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let o = Command::new("git")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "init.defaultBranch=main", "-c", "credential.helper="])
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;
    let out = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    anyhow::ensure!(o.status.success(), "git {args:?}: {out}");
    Ok(out)
}

fn until(s: &Server, secs: u64, what: &str, mut f: impl FnMut() -> anyhow::Result<Option<String>>) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        match f()? {
            None => return Ok(()),
            Some(last) => anyhow::ensure!(Instant::now() < deadline, "{what}: {last}\n{}", s.log()),
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// A user with `grants` and an email address, and a token that may do what they may.
fn person(s: &Server, name: &str, grants: &[&str]) -> anyhow::Result<String> {
    let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": name, "grants": grants })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}: {st} {body}");
    let (st, body) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&json!({ "kind": "pat", "user": name, "grants": ["owner"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}'s token: {st} {body}");
    serde_json::from_str::<Value>(&body)?.get("token").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no token in {body}"))
}

/// An SMTP server that takes every message, as it came after DATA.
fn smtp(got: Arc<Mutex<Vec<String>>>) -> anyhow::Result<u16> {
    let l = TcpListener::bind("127.0.0.1:0")?;
    let port = l.local_addr()?.port();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let got = Arc::clone(&got);
            std::thread::spawn(move || {
                let mut w = s.try_clone().ok();
                let mut r = BufReader::new(s);
                let mut say = |t: &str| {
                    if let Some(w) = w.as_mut() {
                        let _ = w.write_all(t.as_bytes());
                    }
                };
                say("220 stand-in ESMTP\r\n");
                let mut line = String::new();
                while r.read_line(&mut line).is_ok_and(|n| n > 0) {
                    let cmd = line.trim_end().to_ascii_uppercase();
                    if cmd.starts_with("EHLO") {
                        say("250-stand-in\r\n250 OK\r\n");
                    } else if cmd == "DATA" {
                        say("354 go on\r\n");
                        let mut data = String::new();
                        let mut l = String::new();
                        while r.read_line(&mut l).is_ok_and(|n| n > 0) && l != ".\r\n" {
                            data.push_str(&l);
                            l.clear();
                        }
                        if let Ok(mut g) = got.lock() {
                            g.push(data);
                        }
                        say("250 taken\r\n");
                    } else if cmd == "QUIT" {
                        say("221 bye\r\n");
                        return;
                    } else {
                        say("250 OK\r\n");
                    }
                    line.clear();
                }
            });
        }
    });
    Ok(port)
}

/// A mailbox: each message's UID, bytes and whether it's been seen.
type Mailbox = Arc<Mutex<Vec<(u32, Vec<u8>, bool)>>>;

/// An IMAP server with `mail` as its one folder, for `review` signing in with `pw`.
fn imap(mail: Mailbox) -> anyhow::Result<u16> {
    let l = TcpListener::bind("127.0.0.1:0")?;
    let port = l.local_addr()?.port();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let mail = Arc::clone(&mail);
            std::thread::spawn(move || serve_imap(s, &mail));
        }
    });
    Ok(port)
}

fn serve_imap(s: TcpStream, mail: &Mailbox) {
    let Ok(mut w) = s.try_clone() else { return };
    let mut r = BufReader::new(s);
    let _ = w.write_all(b"* OK stand-in IMAP4rev1\r\n");
    let mut line = String::new();
    while r.read_line(&mut line).is_ok_and(|n| n > 0) {
        let (tag, rest) = line.trim_end().split_once(' ').unwrap_or((line.trim_end(), ""));
        let (tag, rest) = (tag.to_owned(), rest.to_owned());
        let upper = rest.to_ascii_uppercase();
        let out = if upper.starts_with("LOGIN") {
            if rest.contains("\"review\" \"pw\"") { format!("{tag} OK signed in\r\n") } else { format!("{tag} NO who?\r\n") }
        } else if upper.starts_with("SELECT") {
            format!("* {} EXISTS\r\n{tag} OK [READ-WRITE] selected\r\n", mail.lock().map(|m| m.len()).unwrap_or(0))
        } else if upper.starts_with("UID SEARCH") {
            let uids: Vec<String> = mail.lock().map(|m| m.iter().filter(|(_, _, seen)| !seen).map(|(u, _, _)| u.to_string()).collect()).unwrap_or_default();
            format!("* SEARCH {}\r\n{tag} OK searched\r\n", uids.join(" "))
        } else if upper.starts_with("UID FETCH") {
            let set: Vec<u32> = rest.split_whitespace().nth(2).unwrap_or_default().split(',').filter_map(|u| u.parse().ok()).collect();
            let mut out = Vec::new();
            if let Ok(mut m) = mail.lock() {
                for (i, (uid, bytes, seen)) in m.iter_mut().enumerate() {
                    if set.contains(uid) {
                        out.extend_from_slice(format!("* {} FETCH (UID {uid} BODY[] {{{}}}\r\n", i + 1, bytes.len()).as_bytes());
                        out.extend_from_slice(bytes);
                        out.extend_from_slice(b")\r\n");
                        *seen = true;
                    }
                }
            }
            out.extend_from_slice(format!("{tag} OK fetched\r\n").as_bytes());
            let _ = w.write_all(&out);
            line.clear();
            continue;
        } else if upper.starts_with("LOGOUT") {
            let _ = w.write_all(format!("* BYE\r\n{tag} OK bye\r\n").as_bytes());
            return;
        } else {
            format!("{tag} BAD what?\r\n")
        };
        let _ = w.write_all(out.as_bytes());
        line.clear();
    }
}

/// What a message's base64 body says.
fn body_of(data: &str) -> String {
    use base64::Engine as _;
    let b64: String = data.split_once("\r\n\r\n").map_or("", |(_, b)| b).split_whitespace().collect();
    String::from_utf8_lossy(&base64::engine::general_purpose::STANDARD.decode(b64).unwrap_or_default()).into_owned()
}

/// A header of a message as it came after DATA.
fn header<'a>(data: &'a str, name: &str) -> Option<&'a str> {
    data.lines().take_while(|l| !l.is_empty()).find_map(|l| l.strip_prefix(&format!("{name}: ")))
}

#[test]
fn w14_r05_a_reply_by_email_posts_to_its_thread() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let sent = Arc::new(Mutex::new(Vec::new()));
    let mailbox: Mailbox = Arc::new(Mutex::new(Vec::new()));
    let (smtp_port, imap_port) = (smtp(Arc::clone(&sent))?, imap(Arc::clone(&mailbox))?);
    let s = start(dir.path(), &Options { ci: "allow_private_targets = [\"127.0.0.1\"]", ..Options::default() })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let dev = person(&s, "dev", &["developer@repo:acme/app"])?;
    let rev = person(&s, "rev", &["developer@repo:acme/app"])?;
    // The mail integration: SMTP out, and the mailbox replies come back to.
    let config = json!({
        "host": "127.0.0.1", "port": smtp_port.to_string(), "tls": "none", "from": "kiln <kiln@example.test>",
        "replies": "review@example.test", "imap_host": "127.0.0.1", "imap_port": imap_port.to_string(), "imap_tls": "none", "imap_user": "review", "imap_every": "1s",
    });
    let (st, body) = http(s.port, "POST", "/api/ci/integrations", &s.admin, Some(&json!({ "kind": "email", "name": "mail", "config": config, "secret": "pw" })))?;
    anyhow::ensure!((200..300).contains(&st), "the integration: {st} {body}");
    // rev's own rule: replies to them, by email.
    let (st, body) = http(s.port, "PUT", "/api/me/notifications", &rev, Some(&json!({ "rules": [{ "on": ["comment.reply"], "to": ["email"] }], "email": "rev@example.test" })))?;
    anyhow::ensure!((200..300).contains(&st), "rev's rule: {st} {body}");

    // D1, rev's question on it, dev's answer in its thread.
    let (home, work) = (dir.path().join("githome"), dir.path().join("tree"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&work)?;
    let push = format!("http://x:{dev}@127.0.0.1:{}/git/acme/app.git", s.port);
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "app\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &format!("http://x:{}@127.0.0.1:{}/git/acme/app.git", s.admin, s.port), "HEAD:main"])?;
    std::fs::write(work.join("README.md"), "app, changed\n")?;
    git(&home, &work, &["commit", "-qam", "change it\n\nChange-Id: I7777777777777777777777777777777777777777"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:refs/for/main"])?;
    let (st, asked) = http(s.port, "POST", "/api/diffs/D1/comments", &rev, Some(&json!({ "body": "Why this change?", "publish": true })))?;
    anyhow::ensure!(st == 200, "rev's question: {st} {asked}");
    let thread = serde_json::from_str::<Value>(&asked)?.get("thread").and_then(Value::as_str).unwrap_or_default().to_owned();
    let (st, answer) = http(s.port, "POST", "/api/diffs/D1/comments", &dev, Some(&json!({ "thread": thread, "body": "It reads better.", "publish": true })))?;
    anyhow::ensure!(st == 200, "dev's answer: {st} {answer}");

    // rev's email, with a reply address of their own.
    let mut data = String::new();
    until(&s, 60, "rev's email", || {
        data = sent.lock().map(|g| g.iter().find(|m| header(m, "To") == Some("rev@example.test")).cloned().unwrap_or_default()).unwrap_or_default();
        Ok(data.is_empty().then(|| format!("{:?}", sent.lock().map(|g| g.len()))))
    })?;
    let reply_to = header(&data, "Reply-To").unwrap_or_default().to_owned();
    assert!(reply_to.starts_with("review+") && reply_to.ends_with("@example.test"), "{data}");
    assert!(body_of(&data).contains("Reply to this email to answer in the thread."), "{}", body_of(&data));

    // Someone else using that address, then rev, quoting what they answer.
    let mail = |from: &str, text: &str| {
        format!("From: {from}\r\nTo: {reply_to}\r\nSubject: Re: D1\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{text}\r\n\r\nOn Tue, 21 Oct 2026, kiln <kiln@example.test> wrote:\r\n> dev replied on D1\r\n").into_bytes()
    };
    if let Ok(mut m) = mailbox.lock() {
        m.push((1, mail("mallory@example.test", "I'm rev, honest."), false));
        m.push((2, mail("Rev <rev@example.test>", "Fair: it does."), false));
    }
    let comments = || -> anyhow::Result<Vec<(String, String)>> {
        let (_, body) = http(s.port, "GET", "/api/diffs/D1/comments", &dev, None)?;
        let threads: Value = serde_json::from_str(&body)?;
        Ok(threads
            .get("threads")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|t| t.get("id").and_then(Value::as_str) == Some(thread.as_str()))
            .flat_map(|t| t.get("comments").and_then(Value::as_array).cloned().unwrap_or_default())
            .map(|c| (c.get("author").and_then(Value::as_str).unwrap_or_default().to_owned(), c.get("body").and_then(Value::as_str).unwrap_or_default().to_owned()))
            .collect())
    };
    until(&s, 60, "rev's emailed reply in the thread", || {
        let now = comments()?;
        Ok((!now.iter().any(|(a, b)| a == "user:rev" && b == "Fair: it does.")).then(|| format!("{now:?}")))
    })?;
    let now = comments()?;
    assert_eq!(now.len(), 3, "the question, the answer and rev's emailed reply, and nothing from anyone else: {now:?}");
    assert!(mailbox.lock().map(|m| m.iter().all(|(_, _, seen)| *seen)).unwrap_or(false), "both messages were read");
    assert!(s.log().contains("it's from mallory@example.test"), "the other one was left, saying why\n{}", s.log());
    Ok(())
}

/// Slack's Web API as a bot sees it: `chat.postMessage` (each kept) and `users.info`.
fn slack(posts: Arc<Mutex<Vec<Value>>>) -> anyhow::Result<u16> {
    let l = TcpListener::bind("127.0.0.1:0")?;
    let port = l.local_addr()?.port();
    std::thread::spawn(move || {
        for mut s in l.incoming().flatten() {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            let (head, body) = loop {
                let Ok(n) = std::io::Read::read(&mut s, &mut chunk) else { break (String::new(), String::new()) };
                if n == 0 {
                    break (String::new(), String::new());
                }
                buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
                let text = String::from_utf8_lossy(&buf).into_owned();
                if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                    let len = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok())).unwrap_or(0);
                    if rest.len() >= len {
                        break (head.to_owned(), rest.to_owned());
                    }
                }
            };
            let target = head.split_whitespace().nth(1).unwrap_or_default().to_owned();
            let bot = head.lines().any(|l| l.eq_ignore_ascii_case("authorization: Bearer xoxb-test"));
            let answer = if !bot {
                json!({ "ok": false, "error": "not_authed" })
            } else if target.starts_with("/chat.postMessage") {
                if let Ok(mut p) = posts.lock() {
                    p.push(serde_json::from_str(&body).unwrap_or(Value::Null));
                }
                json!({ "ok": true, "channel": "C42", "ts": "1792500000.000100" })
            } else if target == "/users.info?user=U7" {
                json!({ "ok": true, "user": { "id": "U7", "profile": { "email": "rev@example.test" } } })
            } else {
                json!({ "ok": false, "error": "user_not_found" })
            };
            let text = answer.to_string();
            let _ = s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len()).as_bytes());
        }
    });
    Ok(port)
}

#[test]
fn w14_r05_a_reply_in_slack_posts_to_its_thread() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let posts = Arc::new(Mutex::new(Vec::new()));
    let api = slack(Arc::clone(&posts))?;
    let s = start(dir.path(), &Options { ci: "allow_private_targets = [\"127.0.0.1\"]", ..Options::default() })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let dev = person(&s, "dev", &["developer@repo:acme/app"])?;
    let rev = person(&s, "rev", &["developer@repo:acme/app"])?;
    let (st, body) = http(s.port, "PUT", "/api/me/notifications", &rev, Some(&json!({ "rules": [], "email": "rev@example.test" })))?;
    anyhow::ensure!((200..300).contains(&st), "rev's address: {st} {body}");
    let config = json!({ "channel": "#review", "api": format!("http://127.0.0.1:{api}") });
    let (st, body) = http(s.port, "POST", "/api/ci/integrations", &s.admin, Some(&json!({ "kind": "slack", "name": "chat", "config": config, "secret": "xoxb-test" })))?;
    anyhow::ensure!((200..300).contains(&st), "the integration: {st} {body}");
    let (_, body) = http(s.port, "GET", "/api/ci/integrations/chat/replies", &s.admin, None)?;
    let events = serde_json::from_str::<Value>(&body)?.get("events_url").and_then(Value::as_str).unwrap_or_default().to_owned();
    let path = events.split_once("://").and_then(|(_, rest)| rest.split_once('/')).map(|(_, p)| format!("/{p}")).unwrap_or_default();
    anyhow::ensure!(path.starts_with("/api/ci/replies/slack/"), "its events address: {body}");

    // The trunk sends its review threads' replies to Slack; D1, a question, an answer.
    let (home, work) = (dir.path().join("githome"), dir.path().join("tree"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join(".kiln/notify.yaml"), "- on: [comment.reply]\n  to: [slack:chat]\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    // Its rules are read from the trunk as kiln hosts it: there as soon as it's pushed.
    git(&home, &work, &["push", "-q", &format!("http://x:{}@127.0.0.1:{}/git/acme/app.git", s.admin, s.port), "HEAD:main"])?;
    std::fs::write(work.join("README.md"), "app\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "add a readme\n\nChange-Id: I8888888888888888888888888888888888888888"])?;
    git(&home, &work, &["push", "-q", &format!("http://x:{dev}@127.0.0.1:{}/git/acme/app.git", s.port), "HEAD:refs/for/main"])?;
    let (st, asked) = http(s.port, "POST", "/api/diffs/D1/comments", &rev, Some(&json!({ "body": "A readme?", "publish": true })))?;
    anyhow::ensure!(st == 200, "rev's question: {st} {asked}");
    let thread = serde_json::from_str::<Value>(&asked)?.get("thread").and_then(Value::as_str).unwrap_or_default().to_owned();
    let (st, answer) = http(s.port, "POST", "/api/diffs/D1/comments", &dev, Some(&json!({ "thread": thread, "body": "Every repository wants one.", "publish": true })))?;
    anyhow::ensure!(st == 200, "dev's answer: {st} {answer}");
    until(&s, 60, "the bot's message about the thread", || {
        let (_, sent) = http(s.port, "GET", "/api/ci/notify/deliveries", &s.admin, None)?;
        Ok(posts.lock().map(|p| p.is_empty()).unwrap_or(true).then_some(sent))
    })?;

    // Slack's own check, a reply in the message's Slack thread, and a reply in a thread kiln
    // didn't start.
    let (st, body) = http(s.port, "POST", &path, "", Some(&json!({ "type": "url_verification", "challenge": "c-123" })))?;
    assert_eq!((st, serde_json::from_str::<Value>(&body)?.get("challenge").cloned()), (200, Some(json!("c-123"))), "{body}");
    let (st, _) = http(s.port, "POST", &format!("{}x", path.trim_end_matches('/')), "", Some(&json!({ "type": "url_verification", "challenge": "c" })))?;
    assert_eq!(st, 404, "another key isn't the integration's");
    let reply = |thread_ts: &str, text: &str| json!({ "type": "event_callback", "event": { "type": "message", "channel": "C42", "user": "U7", "text": text, "ts": "1792500009.000300", "thread_ts": thread_ts } });
    let (st, _) = http(s.port, "POST", &path, "", Some(&reply("1792500000.000100", "Fair enough.")))?;
    assert_eq!(st, 200);
    let (st, _) = http(s.port, "POST", &path, "", Some(&reply("1792499999.000001", "Elsewhere.")))?;
    assert_eq!(st, 200);
    let comments = || -> anyhow::Result<Vec<(String, String)>> {
        let (_, body) = http(s.port, "GET", "/api/diffs/D1/comments", &dev, None)?;
        Ok(serde_json::from_str::<Value>(&body)?
            .get("threads")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .flat_map(|t| t.get("comments").and_then(Value::as_array).cloned().unwrap_or_default())
            .map(|c| (c.get("author").and_then(Value::as_str).unwrap_or_default().to_owned(), c.get("body").and_then(Value::as_str).unwrap_or_default().to_owned()))
            .collect())
    };
    until(&s, 30, "rev's Slack reply in the thread", || {
        let now = comments()?;
        Ok((!now.iter().any(|(a, b)| a == "user:rev" && b == "Fair enough.")).then(|| format!("{now:?}")))
    })?;
    assert_eq!(comments()?.len(), 3, "nothing from the thread kiln didn't start: {:?}", comments()?);
    Ok(())
}

//! Chat (docs/deploy-api.md §12.3), against stand-ins for Slack's, Discord's, Telegram's and
//! Teams' APIs: a release's one message in each channel that can change a message, edited as the
//! release moves (Approve and Reject on it while it waits, where kiln takes the chat's buttons);
//! Teams' a message per event; and commands (deploy, approve, reject, rollback, status, link) from
//! each chat, checked as that chat signs them, acting as the person's kiln user, with the
//! environment's gates and agents policy holding them as they hold the API.

#![cfg(unix)]

mod common;

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use common::{Server, http, start_cd};
use serde_json::{Value, json};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// What the stand-in was asked (method, path with its query, body) and what it answered.
type Seen = Arc<Mutex<Vec<(String, String, Value, Value)>>>;

/// What it was asked.
fn seen(s: &Seen) -> Vec<(String, String, Value)> {
    s.lock().map(|v| v.iter().map(|(m, p, b, _)| (m.clone(), p.clone(), b.clone())).collect()).unwrap_or_default()
}

/// What it answered to `path`, in order.
fn answered(s: &Seen, path: &str) -> Vec<Value> {
    s.lock().map(|v| v.iter().filter(|(_, p, _, _)| p == path).map(|(_, _, _, a)| a.clone()).collect()).unwrap_or_default()
}

/// One request off `s`: its method, target and body.
fn read_request(s: &mut TcpStream) -> Option<(String, String, String)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = s.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(chunk.get(..n)?);
        let text = String::from_utf8_lossy(&buf).into_owned();
        if let Some((head, rest)) = text.split_once("\r\n\r\n") {
            let len = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok())).unwrap_or(0);
            if rest.len() >= len {
                let mut words = head.split_whitespace();
                return Some((words.next()?.to_owned(), words.next()?.to_owned(), rest.to_owned()));
            }
        }
    }
}

/// Slack's Web API (`/slack`), its response URLs (`/slack/hooks/…`), a Discord webhook
/// (`/discord/webhooks/1/tok`) and Discord's API (`/discord-api`), Telegram's Bot API
/// (`/telegram`) and a Teams Workflows webhook (`/teams`), each request kept.
fn chats(seen: Seen) -> anyhow::Result<u16> {
    let l = TcpListener::bind("127.0.0.1:0")?;
    let port = l.local_addr()?.port();
    std::thread::spawn(move || {
        let mut n = 0u64;
        for mut s in l.incoming().flatten() {
            let Some((method, target, body)) = read_request(&mut s) else { continue };
            n += 1;
            let v: Value = serde_json::from_str(&body).unwrap_or(Value::String(body.clone()));
            let path = target.split('?').next().unwrap_or_default().to_owned();
            let answer = match (method.as_str(), path.as_str()) {
                ("POST", "/slack/chat.postMessage") => json!({ "ok": true, "channel": "C42", "ts": format!("1792500000.{n:06}") }),
                ("POST", "/slack/chat.update") => json!({ "ok": true }),
                ("GET", "/slack/users.info") => {
                    let user = target.split("user=").nth(1).unwrap_or_default();
                    match user {
                        "UADA" => json!({ "ok": true, "user": { "id": user, "profile": { "email": "ada@example.test" } } }),
                        "UGRACE" => json!({ "ok": true, "user": { "id": user, "profile": { "email": "grace@example.test" } } }),
                        "UBOB" => json!({ "ok": true, "user": { "id": user, "profile": { "email": "bob@example.test" } } }),
                        _ => json!({ "ok": true, "user": { "id": user, "profile": { "email": "nobody@example.test" } } }),
                    }
                }
                ("POST", p) if p.starts_with("/slack/hooks/") => json!({ "ok": true }),
                ("POST", "/discord/webhooks/1/tok") => json!({ "id": format!("m{n}"), "channel_id": "D1" }),
                ("PATCH", p) if p.starts_with("/discord/webhooks/1/tok/messages/") => json!({ "id": "m" }),
                ("PATCH", p) if p.starts_with("/discord-api/webhooks/") => json!({ "id": "a" }),
                ("POST", "/telegram/bot123:abc/sendMessage") => json!({ "ok": true, "result": { "message_id": n, "chat": { "id": -1001 } } }),
                ("POST", "/telegram/bot123:abc/editMessageText") => json!({ "ok": true, "result": {} }),
                ("POST", "/telegram/bot123:abc/answerCallbackQuery" | "/telegram/bot123:abc/setWebhook") => json!({ "ok": true, "result": true }),
                ("POST", "/teams") => json!({}),
                _ => json!({ "ok": false, "error": "unknown_method" }),
            };
            if let Ok(mut all) = seen.lock() {
                all.push((method, target, v, answer.clone()));
            }
            let text = answer.to_string();
            let _ = s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len()).as_bytes());
        }
    });
    Ok(port)
}

/// A request to kiln with these headers and this body as it is: its status and body.
fn raw(port: u16, path: &str, headers: &[(&str, String)], body: &[u8]) -> anyhow::Result<(u16, String)> {
    call(port, "POST", path, headers, body)
}

/// [`raw`], with any method.
fn call(port: u16, method: &str, path: &str, headers: &[(&str, String)], body: &[u8]) -> anyhow::Result<(u16, String)> {
    let mut c = TcpStream::connect(("127.0.0.1", port))?;
    c.set_read_timeout(Some(Duration::from_secs(30)))?;
    let extra: String = headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
    write!(c, "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n", body.len())?;
    c.write_all(body)?;
    let mut out = Vec::new();
    c.read_to_end(&mut out)?;
    let text = String::from_utf8_lossy(&out).into_owned();
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let rest = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") { common::dechunk(rest) } else { rest.to_owned() };
    Ok((status, rest))
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

/// A person with these grants and an email address, and a token of theirs that does all they may.
fn person(s: &Server, name: &str, grants: &[&str]) -> anyhow::Result<String> {
    let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": name, "grants": grants })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}: {st} {body}");
    let (st, body) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&json!({ "kind": "pat", "user": name, "grants": ["owner"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}'s token: {st} {body}");
    let token = serde_json::from_str::<Value>(&body)?.get("token").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no token in {body}"))?;
    let (st, body) = http(s.port, "PUT", "/api/me/notifications", &token, Some(&json!({ "rules": [], "email": format!("{name}@example.test") })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}'s address: {st} {body}");
    Ok(token)
}

/// A person's browser session, by a sign-in link: `kiln_session=…`.
fn session(s: &Server, name: &str) -> anyhow::Result<String> {
    let (st, body) = http(s.port, "POST", &format!("/api/users/{name}/link"), &s.admin, None)?;
    anyhow::ensure!(st == 200, "{name}'s link: {st} {body}");
    let url = serde_json::from_str::<Value>(&body)?.get("url").and_then(Value::as_str).unwrap_or_default().to_owned();
    let path = url.split_once("://").and_then(|(_, rest)| rest.find('/').map(|i| rest.get(i..).unwrap_or_default().to_owned())).unwrap_or_default();
    let mut c = TcpStream::connect(("127.0.0.1", s.port))?;
    c.set_read_timeout(Some(Duration::from_secs(30)))?;
    write!(c, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n", s.port)?;
    let mut out = String::new();
    c.read_to_string(&mut out)?;
    out.lines().find_map(|l| l.to_ascii_lowercase().starts_with("set-cookie:").then(|| l.split_once(':').map(|(_, v)| v.trim().split(';').next().unwrap_or_default().to_owned())).flatten()).ok_or_else(|| anyhow::anyhow!("no session from {path}"))
}

/// A link code for the person whose session `cookie` is.
fn code(s: &Server, cookie: &str) -> anyhow::Result<String> {
    let (st, body) = raw(s.port, "/api/me/chat/codes", &[("Cookie", cookie.to_owned()), ("Content-Type", "application/json".into())], b"{}")?;
    anyhow::ensure!(st == 201, "a code: {st} {body}");
    serde_json::from_str::<Value>(&body)?.get("code").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no code in {body}"))
}

/// The path of an integration's chat address, and its key.
fn address(s: &Server, name: &str) -> anyhow::Result<(String, String)> {
    let (st, body) = http(s.port, "GET", &format!("/api/ci/integrations/{name}/chat"), &s.admin, None)?;
    anyhow::ensure!(st == 200, "{name}'s address: {st} {body}");
    let url = serde_json::from_str::<Value>(&body)?.get("address").and_then(Value::as_str).unwrap_or_default().to_owned();
    let path = url.split_once("://").and_then(|(_, rest)| rest.find('/').map(|i| rest.get(i..).unwrap_or_default().to_owned())).unwrap_or_default();
    anyhow::ensure!(path.starts_with(&format!("/api/ci/chat/")), "{body}");
    let key = path.rsplit('/').next().unwrap_or_default().to_owned();
    Ok((path, key))
}

/// Environments `staging` (agents deploy there) and `production` (grace approves what goes
/// there), and `web` and `api` in both, their messages to `channels`.
fn shop(s: &Server, channels: Value) -> anyhow::Result<()> {
    for env in [json!({ "name": "staging", "provider": "kilncd", "agents": { "deploy": true } }), json!({ "name": "production", "provider": "kilncd", "gates": { "approvals": { "count": 1, "reviewers": ["grace"] } } })] {
        let (st, body) = http(s.port, "POST", "/api/deploy/environments", &s.admin, Some(&env))?;
        anyhow::ensure!((200..300).contains(&st), "{env}: {st} {body}");
    }
    for name in ["web", "api"] {
        // A chart registry that isn't there: the version can't be looked up, so the provider checks it.
        let app = json!({ "name": name, "project": "shop", "package": { "kind": "helm", "repo": "oci://127.0.0.1:9/charts", "chart": name }, "contacts": { "channels": channels } });
        let (st, body) = http(s.port, "POST", "/api/deploy/apps", &s.admin, Some(&app))?;
        anyhow::ensure!(st == 201, "{name}: {st} {body}");
        for env in ["staging", "production"] {
            let (st, body) = http(s.port, "POST", &format!("/api/deploy/apps/{name}/environments/{env}"), &s.admin, Some(&json!({})))?;
            anyhow::ensure!(st == 201, "{name} in {env}: {st} {body}");
        }
    }
    Ok(())
}

fn release(s: &Server, id: i64) -> anyhow::Result<Value> {
    let (_, body) = http(s.port, "GET", &format!("/api/deploy/releases/{id}"), &s.admin, None)?;
    Ok(serde_json::from_str(&body).unwrap_or(Value::Null))
}

/// The number in `release 42, …`.
fn number_in(text: &str) -> i64 {
    text.split("release ").nth(1).and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next()).and_then(|n| n.parse().ok()).unwrap_or_default()
}

fn str_at<'a>(v: &'a Value, p: &str) -> &'a str {
    v.pointer(p).and_then(Value::as_str).unwrap_or_default()
}

/// Slack signs what it sends with the app's signing secret.
fn slack_signed(secret: &str, body: &[u8], at: i64) -> Vec<(&'static str, String)> {
    use hmac::{KeyInit as _, Mac as _};
    let mut base = format!("v0:{at}:").into_bytes();
    base.extend_from_slice(body);
    let Ok(mut m) = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()) else { return Vec::new() };
    m.update(&base);
    vec![("X-Slack-Request-Timestamp", at.to_string()), ("X-Slack-Signature", format!("v0={}", hex::encode(m.finalize().into_bytes())))]
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

fn form(fields: &[(&str, &str)]) -> Vec<u8> {
    let enc = |s: &str| s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { char::from(b).to_string() } else { format!("%{b:02X}") }).collect::<String>();
    fields.iter().map(|(k, v)| format!("{}={}", enc(k), enc(v))).collect::<Vec<_>>().join("&").into_bytes()
}

#[test]
fn p5_chat_slack_one_message_per_release_and_commands_as_their_person() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let got: Seen = Arc::new(Mutex::new(Vec::new()));
    let api = chats(Arc::clone(&got))?;
    let s = start_cd(dir.path(), &common::Options { ci: "allow_private_targets = [\"127.0.0.1\"]", ..common::Options::default() })?;
    // ada deploys and may approve, but isn't production's reviewer; grace is; bob only reads.
    let _ada = person(&s, "ada", &["viewer", "environments.deploy", "runs.approve"])?;
    let _grace = person(&s, "grace", &["viewer", "runs.approve"])?;
    let _bob = person(&s, "bob", &["viewer"])?;
    shop(&s, json!(["#deploys"]))?;
    let secret = "a-slack-signing-secret";
    let config = json!({ "api": format!("http://127.0.0.1:{api}/slack") });
    let (st, body) = http(s.port, "POST", "/api/ci/integrations", &s.admin, Some(&json!({ "kind": "slack", "name": "chat", "config": config, "secret": "xoxb-test", "signing_secret": secret })))?;
    anyhow::ensure!(st == 201, "slack: {st} {body}");
    assert_eq!(serde_json::from_str::<Value>(&body)?.get("has_signing_secret"), Some(&json!(true)), "{body}");
    let (path, _) = address(&s, "chat")?;

    let form_headers = |body: &[u8], at: i64| {
        let mut h = slack_signed(secret, body, at);
        h.push(("Content-Type", "application/x-www-form-urlencoded".into()));
        h
    };
    let slash = |user: &str, name: &str, text: &str| -> anyhow::Result<String> {
        let body = form(&[("command", "/kiln"), ("text", text), ("user_id", user), ("user_name", name), ("channel_id", "C42"), ("response_url", &format!("http://127.0.0.1:{api}/slack/hooks/{user}")), ("trigger_id", &format!("t.{text}"))]);
        let (st, said) = raw(s.port, &path, &form_headers(&body, now()), &body)?;
        anyhow::ensure!(st == 200, "/kiln {text}: {st} {said}");
        Ok(serde_json::from_str::<Value>(&said)?.get("text").and_then(Value::as_str).unwrap_or_default().to_owned())
    };
    // What Slack is told back at a response URL, by whom it was for.
    let answers_for = |user: &str| -> Vec<String> { seen(&got).into_iter().filter(|(m, p, _)| m == "POST" && p == &format!("/slack/hooks/{user}")).map(|(_, _, v)| str_at(&v, "/text").to_owned()).collect() };

    // Only what Slack signed: none, another secret's, or an old one, is refused.
    let body = form(&[("command", "/kiln"), ("text", "status web"), ("user_id", "UADA")]);
    assert_eq!(raw(s.port, &path, &[("Content-Type", "application/x-www-form-urlencoded".into())], &body)?.0, 401);
    let forged = { let mut h = slack_signed("another", &body, now()); h.push(("Content-Type", "application/x-www-form-urlencoded".into())); h };
    assert_eq!(raw(s.port, &path, &forged, &body)?.0, 401);
    assert_eq!(raw(s.port, &path, &form_headers(&body, now() - 900), &body)?.0, 401, "an old request isn't taken again");
    assert_eq!(raw(s.port, &format!("{}x", path), &form_headers(&body, now()), &body)?.0, 404, "another key isn't the integration's");

    // ada asks for web 1.4.2 in production: it waits for grace, and #deploys has its one message, with Approve and Reject.
    let said = slash("UADA", "ada", "deploy web 1.4.2 to production")?;
    assert!(said.starts_with("Asked: release ") && said.contains("web 1.4.2 → production, waits for approval (grace)"), "{said}\n{}", s.log());
    let id = number_in(&said);
    assert_eq!(str_at(&release(&s, id)?, "/requested_by"), "user:ada", "asked as ada");
    until(&s, 30, "the release's message in #deploys", || {
        let posts: Vec<Value> = seen(&got).into_iter().filter(|(m, p, _)| m == "POST" && p == "/slack/chat.postMessage").map(|(_, _, v)| v).collect();
        let mine = posts.iter().find(|v| str_at(v, "/channel") == "#deploys" && str_at(v, "/text").contains("waits for approval"));
        if let Some(v) = mine {
            assert_eq!(str_at(v, "/blocks/1/elements/0/action_id"), "kiln_approve", "{v}");
            assert_eq!(str_at(v, "/blocks/1/elements/1/value"), id.to_string(), "{v}");
            assert_eq!(posts.len(), 1, "one message for the release: {posts:?}");
        }
        Ok(mine.is_none().then(|| format!("{posts:?}")))
    })?;
    let ts = answered(&got, "/slack/chat.postMessage").first().map(|a| str_at(a, "/ts").to_owned()).unwrap_or_default();
    anyhow::ensure!(!ts.is_empty(), "the message's ts");

    // Its buttons act as whoever pressed them: bob may approve nothing; ada asked for it, and isn't
    // production's reviewer; someone Slack knows by an address nobody here has is nobody.
    let press = |user: &str, name: &str, action: &str| -> anyhow::Result<u16> {
        let payload = json!({ "type": "block_actions", "user": { "id": user, "username": name }, "actions": [{ "action_id": action, "value": id.to_string() }], "response_url": format!("http://127.0.0.1:{api}/slack/hooks/{user}"), "container": { "channel_id": "C42", "message_ts": ts } });
        let body = form(&[("payload", &payload.to_string())]);
        Ok(raw(s.port, &path, &form_headers(&body, now()), &body)?.0)
    };
    for (user, name) in [("UBOB", "bob"), ("UADA", "ada"), ("UNOBODY", "nobody")] {
        assert_eq!(press(user, name, "kiln_approve")?, 200);
    }
    until(&s, 30, "the refusals", || {
        let (b, a, n) = (answers_for("UBOB"), answers_for("UADA"), answers_for("UNOBODY"));
        let ok = b.iter().any(|t| t.contains("kiln won't, as user:bob") && t.contains("runs.approve"))
            && a.iter().any(|t| t.contains("isn't one of production's reviewers"))
            && n.iter().any(|t| t.contains("Nobody in kiln has the address nobody@example.test"));
        Ok((!ok).then(|| format!("{b:?} {a:?} {n:?}")))
    })?;
    assert_eq!(str_at(&release(&s, id)?, "/state"), "waiting_approval", "still waits");

    // grace approves from the message: the release goes on, and its message says so, its buttons gone.
    assert_eq!(press("UGRACE", "grace", "kiln_approve")?, 200);
    until(&s, 30, "grace's approval", || {
        let a = answers_for("UGRACE");
        Ok((!a.iter().any(|t| t.starts_with("You approved it: release"))).then(|| format!("{a:?}")))
    })?;
    until(&s, 60, "the release going on", || {
        let r = release(&s, id)?;
        Ok((str_at(&r, "/state") == "waiting_approval").then(|| r.to_string()))
    })?;
    let r = release(&s, id)?;
    assert_eq!(str_at(&r, "/approvals/0/by"), "user:grace", "{r}");
    until(&s, 60, "its message edited", || {
        let edits: Vec<Value> = seen(&got).into_iter().filter(|(m, p, _)| m == "POST" && p == "/slack/chat.update").map(|(_, _, v)| v).collect();
        let last = edits.iter().rev().find(|v| str_at(v, "/ts") == ts && str_at(v, "/channel") == "C42");
        Ok(match last {
            Some(v) if !str_at(v, "/text").contains("waits for approval") && v.pointer("/blocks/1").is_none() && str_at(v, "/text").contains("approved by grace") => None,
            _ => Some(format!("{edits:?}")),
        })
    })?;
    assert_eq!(seen(&got).into_iter().filter(|(_, p, _)| p == "/slack/chat.postMessage").count(), 1, "edited, never posted again");

    // Rejected from a slash command, with why: cancelled, and its message says who and why.
    let said = slash("UADA", "ada", "deploy api 2.1.0 to production")?;
    assert!(said.contains("api 2.1.0 → production, waits for approval"), "{said}");
    let second = number_in(&said);
    let said = slash("UGRACE", "grace", &format!("reject {second} not during the sale"))?;
    assert!(said.starts_with("You rejected it:") && said.contains("cancelled: rejected by user:grace: not during the sale"), "{said}");
    until(&s, 60, "the rejected release's message", || {
        let edits: Vec<String> = seen(&got).into_iter().filter(|(_, p, _)| p == "/slack/chat.update").map(|(_, _, v)| str_at(&v, "/text").to_owned()).collect();
        Ok((!edits.iter().any(|t| t.contains("api 2.1.0 → production was cancelled") && t.contains("rejected by grace"))).then(|| format!("{edits:?}")))
    })?;

    // Status reads as anyone who may read; a deploy needs a deployer; a rollback needs something to go back to.
    let said = slash("UBOB", "bob", "status web")?;
    assert!(said.starts_with("web\n") && said.contains("staging: nothing runs there yet") && said.contains(&format!("release {id}")), "{said}");
    let said = slash("UBOB", "bob", "deploy web 1.4.4 to staging")?;
    assert!(said.contains("kiln won't, as user:bob") && said.contains("environments.deploy"), "{said}");
    let said = slash("UADA", "ada", "rollback web in staging")?;
    assert!(said.contains("no earlier release in staging to roll back to"), "{said}");
    let said = slash("UADA", "ada", "dance")?;
    assert!(said.contains("isn't something kiln does"), "{said}");

    // A bot (an agent) acts for the person who linked it, as an agent: production doesn't let
    // agents deploy or approve; staging lets them deploy.
    let ada_session = session(&s, "ada")?;
    let link_code = code(&s, &ada_session)?;
    let mention = |text: &str, event: &str| -> anyhow::Result<()> {
        let body = json!({ "type": "event_callback", "event_id": event, "event": { "type": "app_mention", "bot_id": "B9", "bot_profile": { "name": "Claude" }, "user": "U9", "text": format!("<@UKILN> {text}"), "channel": "C42", "ts": format!("1792500100.{}", event.len()) } }).to_string();
        let mut h = slack_signed(secret, body.as_bytes(), now());
        h.push(("Content-Type", "application/json".into()));
        let (st, said) = raw(s.port, &path, &h, body.as_bytes())?;
        anyhow::ensure!(st == 200, "{text}: {st} {said}");
        Ok(())
    };
    let in_thread = |want: &str| -> Option<String> {
        let posts: Vec<String> = seen(&got).into_iter().filter(|(_, p, v)| p == "/slack/chat.postMessage" && v.get("thread_ts").is_some()).map(|(_, _, v)| str_at(&v, "/text").to_owned()).collect();
        (!posts.iter().any(|t| t.contains(want))).then(|| format!("{posts:?}"))
    };
    mention("status web", "Ev1")?;
    until(&s, 30, "an unlinked bot's answer", || Ok(in_thread("kiln doesn't know who you are")))?;
    mention(&format!("link {link_code}"), "Ev2")?;
    until(&s, 30, "the bot linked", || Ok(in_thread("acts here as an agent for user:ada")))?;
    mention("deploy web 1.4.5 to production", "Ev3")?;
    until(&s, 30, "production refusing an agent", || Ok(in_thread("agents may not release apps in production")))?;
    mention("deploy web 1.4.5 to staging", "Ev4")?;
    until(&s, 30, "staging taking an agent's release", || Ok(in_thread("Asked: release")))?;
    let (_, body) = http(s.port, "GET", "/api/deploy/releases?app=web&environment=staging", &s.admin, None)?;
    let v: Value = serde_json::from_str(&body)?;
    assert!(v.pointer("/releases/0/requested_by").and_then(Value::as_str).is_some_and(|b| b == "slack:Claude for user:ada"), "an agent for ada: {body}");
    Ok(())
}

/// Teams signs an outgoing webhook's messages with its security token.
fn teams_signed(token: &str, body: &[u8]) -> String {
    use hmac::{KeyInit as _, Mac as _};
    let key = B64.decode(token).unwrap_or_default();
    let Ok(mut m) = hmac::Hmac::<sha2::Sha256>::new_from_slice(&key) else { return String::new() };
    m.update(body);
    format!("HMAC {}", B64.encode(m.finalize().into_bytes()))
}

#[test]
fn p5_chat_discord_telegram_and_teams() -> anyhow::Result<()> {
    use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair as _};
    let dir = tempfile::tempdir()?;
    let got: Seen = Arc::new(Mutex::new(Vec::new()));
    let api = chats(Arc::clone(&got))?;
    let s = start_cd(dir.path(), &common::Options { ci: "allow_private_targets = [\"127.0.0.1\"]", ..common::Options::default() })?;
    let _ada = person(&s, "ada", &["viewer", "environments.deploy"])?;
    let grace = person(&s, "grace", &["viewer", "runs.approve"])?;
    shop(&s, json!(["discord:dc", "telegram:tg", "teams:ms"]))?;
    let pair = Ed25519KeyPair::generate().map_err(|_| anyhow::anyhow!("a key"))?;
    let teams_token = B64.encode(b"the outgoing webhook's token, 32b");
    for (kind, name, config, secret, signing) in [
        ("discord", "dc", json!({ "public_key": hex::encode(pair.public_key().as_ref()), "api": format!("http://127.0.0.1:{api}/discord-api") }), format!("http://127.0.0.1:{api}/discord/webhooks/1/tok"), None),
        ("telegram", "tg", json!({ "chat_id": "-1001", "api": format!("http://127.0.0.1:{api}/telegram") }), "123:abc".to_owned(), None),
        ("teams", "ms", json!({}), format!("http://127.0.0.1:{api}/teams"), Some(teams_token.clone())),
    ] {
        let (st, body) = http(s.port, "POST", "/api/ci/integrations", &s.admin, Some(&json!({ "kind": kind, "name": name, "config": config, "secret": secret, "signing_secret": signing })))?;
        anyhow::ensure!(st == 201, "{name}: {st} {body}");
    }
    let ((dc, _), (tg, tg_key), (ms, _)) = (address(&s, "dc")?, address(&s, "tg")?, address(&s, "ms")?);

    // Telegram's webhook is kiln's once it's set, with the address's key for its secret token.
    let (st, body) = http(s.port, "POST", "/api/ci/integrations/tg/chat", &s.admin, None)?;
    anyhow::ensure!(st == 200, "telegram's webhook: {st} {body}");
    let hook = seen(&got).into_iter().find(|(_, p, _)| p == "/telegram/bot123:abc/setWebhook").map(|(_, _, v)| v).unwrap_or_default();
    assert!(str_at(&hook, "/url").ends_with(&tg) && str_at(&hook, "/secret_token") == tg_key, "{hook}");

    let discord = |user: &str, text: &str| -> anyhow::Result<(u16, Value)> {
        let body = json!({ "id": format!("i-{text}"), "application_id": "99", "type": 2, "token": "itok", "data": { "name": "kiln", "options": [{ "name": "command", "type": 3, "value": text }] }, "member": { "user": { "id": user, "username": "someone" } } }).to_string();
        let mut msg = b"1792500000".to_vec();
        msg.extend_from_slice(body.as_bytes());
        let sig = hex::encode(pair.sign(&msg).as_ref());
        let (st, said) = raw(s.port, &dc, &[("X-Signature-Ed25519", sig), ("X-Signature-Timestamp", "1792500000".into()), ("Content-Type", "application/json".into())], body.as_bytes())?;
        Ok((st, serde_json::from_str(&said).unwrap_or(Value::Null)))
    };
    let telegram = |update: Value| -> anyhow::Result<u16> {
        let body = update.to_string();
        Ok(raw(s.port, &tg, &[("X-Telegram-Bot-Api-Secret-Token", tg_key.clone()), ("Content-Type", "application/json".into())], body.as_bytes())?.0)
    };
    let teams = |text: &str| -> anyhow::Result<(u16, String)> {
        let body = json!({ "type": "message", "id": format!("a-{text}"), "text": format!("<at>kiln</at> {text}"), "from": { "id": "29:ada", "name": "Ada", "aadObjectId": "aad-ada" } }).to_string();
        let (st, said) = raw(s.port, &ms, &[("Authorization", teams_signed(&teams_token, body.as_bytes())), ("Content-Type", "application/json".into())], body.as_bytes())?;
        Ok((st, serde_json::from_str::<Value>(&said).ok().and_then(|v| v.get("text").and_then(Value::as_str).map(str::to_owned)).unwrap_or(said)))
    };
    let telegram_said = |want: &str| -> Option<String> {
        let sent: Vec<String> = seen(&got).into_iter().filter(|(_, p, v)| p == "/telegram/bot123:abc/sendMessage" && v.get("reply_to_message_id").is_some()).map(|(_, _, v)| str_at(&v, "/text").to_owned()).collect();
        (!sent.iter().any(|t| t.contains(want))).then(|| format!("{sent:?}"))
    };

    // Each checked as its chat signs it.
    let unsigned = json!({ "type": 1 }).to_string();
    assert_eq!(raw(s.port, &dc, &[("X-Signature-Ed25519", "00".repeat(64)), ("X-Signature-Timestamp", "1".into())], unsigned.as_bytes())?.0, 401);
    assert_eq!(raw(s.port, &tg, &[("Content-Type", "application/json".into())], b"{}")?.0, 401, "Telegram's secret token is missing");
    assert_eq!(raw(s.port, &ms, &[("Authorization", "HMAC AAAA".into())], b"{}")?.0, 401);
    let (st, pong) = discord("x", "status web")?;
    assert_eq!(st, 200, "{pong}");

    // Nobody's linked yet; grace links Discord and Telegram, ada links Teams, each with a code from her session.
    assert!(str_at(&pong, "/data/content").contains("kiln doesn't know who you are in dc"), "{pong}");
    assert_eq!(pong.pointer("/data/flags"), Some(&json!(64)), "its person alone sees it");
    let (st, said) = raw(s.port, "/api/me/chat/codes", &[("Authorization", format!("Bearer {grace}")), ("Content-Type", "application/json".into())], b"{}")?;
    assert!(st == 403 && said.contains("not with a token"), "a token can't make a code: {st} {said}");
    let (grace_session, ada_session) = (session(&s, "grace")?, session(&s, "ada")?);
    let (_, linked) = discord("555", &format!("link {}", code(&s, &grace_session)?))?;
    assert_eq!(str_at(&linked, "/data/content"), "Linked: here you're user:grace in kiln.", "{linked}");
    let message = |user: i64, text: &str, n: i64| json!({ "update_id": n, "message": { "message_id": n, "from": { "id": user, "is_bot": false, "username": "grace" }, "chat": { "id": -1001, "type": "group" }, "text": text } });
    assert_eq!(telegram(message(777, &format!("/link@kiln_bot {}", code(&s, &grace_session)?), 1))?, 200);
    until(&s, 30, "Telegram's link", || Ok(telegram_said("Linked: here you're user:grace in kiln.")))?;
    let (st, said) = teams(&format!("link {}", code(&s, &ada_session)?))?;
    assert!(st == 200 && said == "Linked: here you're user:ada in kiln.", "{st} {said}");

    // ada asks from Teams: the release waits for grace; Discord and Telegram each have one message
    // of it (Telegram's with Approve and Reject), Teams a message of its waiting.
    let (_, said) = teams("deploy web 2.0.0 to production")?;
    assert!(said.contains("web 2.0.0 → production, waits for approval (grace)"), "{said}\n{}", s.log());
    let id = number_in(&said);
    let posted = || -> (Vec<Value>, Vec<Value>, Vec<Value>) {
        let all = seen(&got);
        let of = |f: &dyn Fn(&str, &str) -> bool| all.iter().filter(|(m, p, _)| f(m, p)).map(|(_, _, v)| v.clone()).collect::<Vec<_>>();
        (of(&|m, p| m == "POST" && p.starts_with("/discord/webhooks/1/tok?")), of(&|_, p| p == "/telegram/bot123:abc/sendMessage"), of(&|_, p| p == "/teams"))
    };
    until(&s, 30, "the release's messages", || {
        let (d, t, m) = posted();
        let ok = d.iter().any(|v| str_at(v, "/content").contains("waits for approval"))
            && t.iter().any(|v| str_at(v, "/text").contains("waits for approval") && str_at(v, "/reply_markup/inline_keyboard/0/0/callback_data") == format!("kiln:approve:{id}"))
            && m.iter().any(|v| v.to_string().contains("waits for approval"));
        Ok((!ok).then(|| format!("{d:?}\n{t:?}\n{m:?}")))
    })?;
    assert_eq!(posted().2.len(), 1, "Teams is told it waits, not that it started");
    assert!(seen(&got).iter().any(|(_, p, _)| p.starts_with("/discord/webhooks/1/tok?wait=true")), "Discord's says which message is kiln's");

    // grace presses Approve in Telegram: told so alone, the release goes on, and both messages say so.
    let press = json!({ "update_id": 9, "callback_query": { "id": "cq1", "from": { "id": 777, "is_bot": false, "username": "grace" }, "message": { "message_id": 3, "chat": { "id": -1001 } }, "data": format!("kiln:approve:{id}") } });
    assert_eq!(telegram(press)?, 200);
    until(&s, 30, "the button's answer", || {
        let a: Vec<Value> = seen(&got).into_iter().filter(|(_, p, _)| p == "/telegram/bot123:abc/answerCallbackQuery").map(|(_, _, v)| v).collect();
        Ok((!a.iter().any(|v| str_at(v, "/text").starts_with("You approved it") && v.get("show_alert") == Some(&json!(true)))).then(|| format!("{a:?}")))
    })?;
    until(&s, 60, "the release going on", || {
        let r = release(&s, id)?;
        Ok((str_at(&r, "/state") == "waiting_approval").then(|| r.to_string()))
    })?;
    until(&s, 60, "the messages edited", || {
        let all = seen(&got);
        let discord = all.iter().any(|(m, p, v)| m == "PATCH" && p.starts_with("/discord/webhooks/1/tok/messages/m") && !str_at(v, "/content").contains("waits for approval") && str_at(v, "/content").contains("approved by grace"));
        let telegram = all.iter().any(|(_, p, v)| p == "/telegram/bot123:abc/editMessageText" && str_at(v, "/text").contains("approved by grace") && v.pointer("/reply_markup/inline_keyboard") == Some(&json!([])));
        Ok((!(discord && telegram)).then(|| format!("{all:?}")))
    })?;

    // Status from Discord, as grace; and once she unlinks it, she's nobody there again.
    let (_, status) = discord("555", "status web in production")?;
    assert!(str_at(&status, "/data/content").contains("web in production"), "{status}");
    let cookie = [("Cookie", grace_session.clone())];
    let (st, body) = call(s.port, "GET", "/api/me/chat", &cookie, b"")?;
    let links: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let kinds: Vec<&str> = links.get("links").and_then(Value::as_array).into_iter().flatten().filter_map(|l| l.get("kind").and_then(Value::as_str)).collect();
    assert!(st == 200 && kinds == ["discord", "telegram"], "{st} {links}");
    assert_eq!(call(s.port, "DELETE", "/api/me/chat/dc/555", &cookie, b"")?.0, 204);
    assert_eq!(call(s.port, "DELETE", "/api/me/chat/dc/555", &cookie, b"")?.0, 404, "gone already");
    let (_, again) = discord("555", "status web")?;
    assert!(str_at(&again, "/data/content").contains("kiln doesn't know who you are"), "{again}");
    Ok(())
}

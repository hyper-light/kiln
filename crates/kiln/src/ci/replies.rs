//! Replies to kiln's review messages (docs/ci.md W14-R05), by email or in a chat's thread, posted
//! to their review threads as the people who wrote them.
//!
//! - **Email.** A message about a thread (`comment.reply`, `mention`) through an `email`
//!   integration with `replies` (a mailbox kiln reads: `review@example.com`) goes to each of its
//!   addresses on its own, `Reply-To: review+<token>@example.com`, the token that address's for
//!   that thread. kiln reads the mailbox's unseen messages over IMAP (`imap_host`, `imap_port`,
//!   `imap_tls`, `imap_user`, `imap_folder`; the integration's password) every `imap_every` (30 s):
//!   a reply to a token, from the address it went to, is posted to the thread by the person with
//!   that address, without what they quoted or their signature. Anything else is left, logged.
//! - **Slack.** A message posted with a bot token is remembered with its thread. A reply in its
//!   Slack thread reaches kiln through Slack's Events API at the integration's own address
//!   (`/api/ci/replies/slack/<integration>/<key>`, the key derived from the server's master key,
//!   shown on the integration), and is posted by the person whose Slack email it is
//!   (`users.info`, with the bot's token).
//! - A reply is a comment: its person must be able to review the diff (`diffs.review`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use rusqlite::{OptionalExtension as _, params};
use serde_json::{Value, json};

use super::Fail;
use super::integrations::{self, Integration};
use crate::http::server::{Request, Response};
use crate::hub::Hub;

/// How long a reply address takes replies.
const TOKEN_DAYS: i64 = 90;
/// The most messages one read of a mailbox takes.
const PER_READ: usize = 50;
/// How often each mailbox is read unless its `imap_every` says.
const EVERY: Duration = Duration::from_secs(30);

// ------------------------------------------------------------------ reply addresses

/// A new reply token for `address`'s messages about `thread`.
pub async fn token_for(hub: &Hub, thread: &str, address: &str) -> anyhow::Result<String> {
    let token: String = crate::util::new_id().chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect();
    let (t, th, a) = (token.clone(), thread.to_owned(), address.trim().to_owned());
    hub.store
        .call(move |c| {
            let now = crate::util::now_unix();
            c.execute("DELETE FROM review_reply_tokens WHERE created_at < ?1", [now.saturating_sub(TOKEN_DAYS * 86_400)])?;
            c.execute("INSERT INTO review_reply_tokens (token, thread_id, address, created_at) VALUES (?1, ?2, ?3, ?4)", params![t, th, a, now])?;
            Ok(())
        })
        .await?;
    Ok(token)
}

/// `replies` (`review@example.com`, or `kiln <review@example.com>`) with `+<token>` in its local
/// part.
pub fn reply_to(replies: &str, token: &str) -> Option<String> {
    let address = super::smtp::address(replies);
    let (local, domain) = address.split_once('@')?;
    let with = format!("{}+{token}@{domain}", local.split('+').next().unwrap_or(local));
    Some(if address == replies.trim() { with } else { replies.replace(address, &with) })
}

/// The token in `to` when it's `replies`' address with one (`review+<token>@example.com`).
fn token_in(to: &str, replies: &str) -> Option<String> {
    let (local, domain) = super::smtp::address(to).split_once('@')?;
    let (r_local, r_domain) = super::smtp::address(replies).split_once('@')?;
    let (base, token) = local.split_once('+')?;
    (base.eq_ignore_ascii_case(r_local.split('+').next().unwrap_or(r_local)) && domain.eq_ignore_ascii_case(r_domain) && !token.is_empty()).then(|| token.to_ascii_lowercase())
}

// ------------------------------------------------------------------ messages

/// A message as replies need it: who it's from, every address it went to, and its text.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Mail {
    pub from: String,
    pub to: Vec<String>,
    pub text: String,
}

/// Headers, their continuation lines unfolded, by lower-cased name (each value as it came).
fn headers(raw: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for line in raw.lines() {
        if line.starts_with([' ', '\t']) {
            if let Some((_, v)) = out.last_mut() {
                v.push(' ');
                v.push_str(line.trim());
            }
        } else if let Some((k, v)) = line.split_once(':') {
            out.push((k.trim().to_ascii_lowercase(), v.trim().to_owned()));
        }
    }
    out
}

fn header<'a>(h: &'a [(String, String)], name: &str) -> Option<&'a str> {
    h.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

/// A header's parameter (`boundary`, `charset`), unquoted.
fn param(value: &str, name: &str) -> Option<String> {
    value.split(';').skip(1).filter_map(|p| p.split_once('=')).find(|(k, _)| k.trim().eq_ignore_ascii_case(name)).map(|(_, v)| v.trim().trim_matches('"').to_owned())
}

/// The addresses in a header (`Ada <ada@example.com>, bob@example.com`).
fn addresses(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut quoted = false;
    let mut part = String::new();
    for c in value.chars() {
        match c {
            '"' => quoted = !quoted,
            ',' if !quoted => out.push(std::mem::take(&mut part)),
            _ => part.push(c),
        }
    }
    out.push(part);
    out.iter().map(|p| super::smtp::address(p).to_ascii_lowercase()).filter(|a| a.contains('@')).collect()
}

/// Quoted-printable's text, as bytes.
fn quoted_printable(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        if b == b'=' {
            match (bytes.get(i + 1), bytes.get(i + 2)) {
                // A soft line break.
                (Some(b'\r'), Some(b'\n')) => i += 3,
                (Some(b'\n'), _) => i += 2,
                (Some(&h), Some(&l)) => match u8::from_str_radix(&String::from_utf8_lossy(&[h, l]), 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b);
                        i += 1;
                    }
                },
                _ => {
                    out.push(b);
                    i += 1;
                }
            }
        } else {
            out.push(b);
            i += 1;
        }
    }
    out
}

/// A part's body, decoded as its headers say (base64, quoted-printable; UTF-8 or Latin-1).
fn decoded(h: &[(String, String)], body: &str) -> String {
    use base64::Engine as _;
    let bytes = match header(h, "content-transfer-encoding").map(str::to_ascii_lowercase).as_deref() {
        Some("base64") => base64::engine::general_purpose::STANDARD.decode(body.split_whitespace().collect::<String>()).unwrap_or_default(),
        Some("quoted-printable") => quoted_printable(body),
        _ => body.as_bytes().to_vec(),
    };
    let charset = header(h, "content-type").and_then(|t| param(t, "charset")).unwrap_or_default().to_ascii_lowercase();
    if matches!(charset.as_str(), "iso-8859-1" | "latin1" | "windows-1252") { bytes.iter().map(|&b| char::from(b)).collect() } else { String::from_utf8_lossy(&bytes).into_owned() }
}

/// A part's plain text: itself when it's `text/plain` (or untyped), else the first plain part of a
/// multipart one, depth first.
fn plain(raw: &str, depth: u8) -> Option<String> {
    let (head, body) = raw.split_once("\r\n\r\n").or_else(|| raw.split_once("\n\n")).unwrap_or((raw, ""));
    let h = headers(head);
    let kind = header(&h, "content-type").unwrap_or("text/plain").to_ascii_lowercase();
    if kind.starts_with("multipart/") {
        let boundary = param(header(&h, "content-type").unwrap_or_default(), "boundary")?;
        let marker = format!("--{boundary}");
        return body.split(marker.as_str()).skip(1).take_while(|p| !p.starts_with("--")).filter(|_| depth < 8).find_map(|p| plain(p.trim_start_matches(['\r', '\n']), depth + 1));
    }
    kind.starts_with("text/plain").then(|| decoded(&h, body))
}

/// A whole message (RFC 5322 bytes), as replies need it.
pub fn parse(raw: &[u8]) -> Mail {
    let text = String::from_utf8_lossy(raw);
    let head = text.split_once("\r\n\r\n").or_else(|| text.split_once("\n\n")).map_or(text.as_ref(), |(h, _)| h);
    let h = headers(head);
    let from = header(&h, "from").map(addresses).and_then(|a| a.into_iter().next()).unwrap_or_default();
    let mut to: Vec<String> = ["to", "cc", "delivered-to", "x-original-to", "envelope-to"].iter().filter_map(|k| header(&h, k)).flat_map(addresses).collect();
    to.sort();
    to.dedup();
    Mail { from, to, text: plain(&text, 0).unwrap_or_default() }
}

/// What a reply says, without what it quotes (`> …`, `On … wrote:`, a forwarded or Outlook header)
/// or its signature (`-- `).
pub fn said(text: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        let next = lines.get(i + 1).map_or("", |l| l.trim());
        let wrote = |s: &str| s.ends_with("wrote:") || s.ends_with("schrieb:") || s.ends_with("a écrit :") || s.ends_with("escribió:");
        if t.starts_with('>')
            || line.trim_end() == "--"
            || t.starts_with("-----Original Message-----")
            || t.starts_with("________________________________")
            || (t.starts_with("On ") && (wrote(t) || wrote(next)))
            || (t.starts_with("From: ") && lines.get(i + 1).is_some_and(|l| l.trim_start().starts_with("Sent: ") || l.trim_start().starts_with("Date: ")))
        {
            break;
        }
        kept.push(line);
    }
    kept.join("\n").trim().to_owned()
}

// ------------------------------------------------------------------ posting

/// Posts `text` to review thread `thread` as user `user` (by id), as their comment.
pub async fn post(hub: &Arc<Hub>, thread: &str, user: &str, text: &str) -> Result<(), String> {
    let t = thread.to_owned();
    let diff: Option<String> = hub.store.call(move |c| Ok(c.query_row("SELECT diff_id FROM review_threads WHERE id = ?1", [&t], |r| r.get(0)).optional()?)).await.map_err(|e| format!("{e:#}"))?;
    let diff = diff.ok_or("its thread is gone")?;
    let d = super::diffs::find(hub, &diff).await.map_err(|e| format!("{e:#}"))?.ok_or("its diff is gone")?;
    let caller = crate::authz::gate::user_caller(hub, user).await.ok_or("its person can't sign in here any more")?;
    if !caller.can("diffs.review", &crate::authz::Scope::Repo { repo: d.repo.clone() }) {
        return Err(format!("{} may not review {} (diffs.review)", caller.name, d.name));
    }
    super::review::comment(hub, &d, &caller, &json!({ "thread": thread, "body": text, "publish": true })).await.map(|_| ()).map_err(|(_, m)| m)
}

/// The live user whose address is `address`, by id.
async fn user_by_address(hub: &Hub, address: &str) -> Option<String> {
    let a = address.trim().to_owned();
    hub.store.call(move |c| Ok(c.query_row("SELECT id FROM users WHERE email = ?1 COLLATE NOCASE AND disabled_at IS NULL", [&a], |r| r.get(0)).optional()?)).await.ok().flatten()
}

/// One message from `i`'s mailbox: posted when it's a reply to a token of the address it's from.
async fn take(hub: &Arc<Hub>, i: &Integration, replies: &str, raw: &[u8]) -> Result<String, String> {
    let m = parse(raw);
    let token = m.to.iter().find_map(|t| token_in(t, replies)).ok_or("it isn't to a reply address")?;
    let tok = token.clone();
    let found: Option<(String, String)> = hub
        .store
        .call(move |c| Ok(c.query_row("SELECT thread_id, address FROM review_reply_tokens WHERE token = ?1", [&tok], |r| Ok((r.get(0)?, r.get(1)?))).optional()?))
        .await
        .map_err(|e| format!("{e:#}"))?;
    let (thread, address) = found.ok_or("its reply address isn't one kiln gave, or it's too old")?;
    // The token is for one address: from any other, it's not that person's.
    if !m.from.eq_ignore_ascii_case(&address) {
        return Err(format!("it's from {}, and its reply address went to {address}", m.from));
    }
    let text = said(&m.text);
    if text.is_empty() {
        return Err("it says nothing above what it quotes".into());
    }
    let user = user_by_address(hub, &address).await.ok_or_else(|| format!("nobody here has the address {address}"))?;
    post(hub, &thread, &user, &text).await?;
    tracing::info!(integration = %i.name, %thread, from = %address, "an emailed reply was posted to its thread");
    Ok(thread)
}

/// Reads `i`'s mailbox once: its unseen messages taken.
async fn read(hub: &Arc<Hub>, i: &Integration) -> anyhow::Result<usize> {
    let cfg = |k: &str| i.config.get(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned);
    let (Some(replies), Some(host)) = (cfg("replies"), cfg("imap_host")) else { return Ok(0) };
    let tls = super::imap::Tls::parse(&cfg("imap_tls").unwrap_or_default()).map_err(anyhow::Error::msg)?;
    let port = cfg("imap_port").map(|p| p.parse::<u16>()).transpose().map_err(|_| anyhow::anyhow!("`imap_port` isn't a port"))?.unwrap_or_else(|| tls.default_port());
    let user = cfg("imap_user").or_else(|| cfg("user")).ok_or_else(|| anyhow::anyhow!("{} has no `imap_user` (or `user`)", i.name))?;
    let password = integrations::token(hub, i).await?.ok_or_else(|| anyhow::anyhow!("{} has no password for its mailbox", i.name))?;
    let mailbox = super::imap::Mailbox { host, port, tls, user, password, folder: cfg("imap_folder").unwrap_or_else(|| "INBOX".into()), reach: Some(hub.cfg.ci.reach()) };
    let got = super::imap::unseen(&mailbox, PER_READ).await?;
    for (uid, raw) in &got {
        if let Err(why) = take(hub, i, &replies, raw).await {
            tracing::info!(integration = %i.name, uid, why, "a message in the replies mailbox was left");
        }
    }
    Ok(got.len())
}

/// When each mailbox was last read, by integration.
static READ_AT: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);

/// Reads every replies mailbox when it's due.
pub async fn run(hub: Arc<Hub>) -> anyhow::Result<()> {
    loop {
        if !hub.shutdown.sleep(Duration::from_secs(1)).await {
            return Ok(());
        }
        let Ok(all) = integrations::list(&hub).await else { continue };
        for i in all.iter().filter(|i| i.kind == "email" && i.config.get("replies").and_then(Value::as_str).is_some_and(|r| !r.trim().is_empty())) {
            let every = i.config.get("imap_every").and_then(Value::as_str).and_then(kiln_ci::trigger::cron::seconds).map_or(EVERY, |s| Duration::from_secs(s.max(1)));
            let due = READ_AT.lock().get_or_insert_with(HashMap::new).get(&i.id).is_none_or(|at| at.elapsed() >= every);
            if !due {
                continue;
            }
            READ_AT.lock().get_or_insert_with(HashMap::new).insert(i.id.clone(), Instant::now());
            if let Err(e) = Box::pin(read(&hub, i)).await {
                tracing::warn!(integration = %i.name, error = %format!("{e:#}"), "the replies mailbox couldn't be read");
            }
        }
    }
}

// ------------------------------------------------------------------ Slack

/// A Slack message kiln posted about `thread`, remembered so a reply in its Slack thread finds it.
pub async fn chat_posted(hub: &Hub, integration: &str, channel: &str, ts: &str, thread: &str) {
    let (i, ch, t, th) = (integration.to_owned(), channel.to_owned(), ts.to_owned(), thread.to_owned());
    if let Err(e) = hub
        .store
        .call(move |c| {
            Ok(c.execute("INSERT OR REPLACE INTO review_chat_posts (integration, channel, ts, thread_id, created_at) VALUES (?1, ?2, ?3, ?4, ?5)", params![i, ch, t, th, crate::util::now_unix()])?)
        })
        .await
    {
        tracing::warn!(error = %format!("{e:#}"), "a chat message's thread couldn't be remembered");
    }
}

/// The key in a Slack integration's events address.
pub(crate) fn events_key(hub: &Hub, id: &str) -> anyhow::Result<String> {
    super::secrets::derived(hub, &format!("kiln ci slack events v1\n{id}"))
}

/// The address Slack's Events API sends a Slack integration's events to (its Request URL).
pub fn events_url(hub: &Hub, i: &Integration) -> Option<String> {
    let key = events_key(hub, &i.id).ok()?;
    Some(format!("{}/api/ci/replies/slack/{}/{key}", hub.cfg.server.public_url.trim_end_matches('/'), i.id))
}

/// Slack's API, for a bot token: its `api`, else Slack's own.
pub(crate) fn slack_api(i: &Integration) -> String {
    i.config.get("api").and_then(Value::as_str).map(str::trim).filter(|a| !a.is_empty()).unwrap_or("https://slack.com/api").trim_end_matches('/').to_owned()
}

/// A Slack user's email, by their id, through the bot's `users.info`.
pub(crate) async fn slack_email(i: &Integration, token: &str, user: &str) -> Result<String, String> {
    let base = integrations::endpoint("slack:users", || slack_api(i));
    let http = crate::http::client::Client::public().map_err(|e| format!("{e:#}"))?;
    let r = http.get(&format!("{base}/users.info?user={}", crate::ci::cli::enc(user))).bearer(token).send().await.map_err(|e| format!("{e:#}"))?;
    let v: Value = r.json(1 << 20).await.map_err(|e| format!("{e:#}"))?;
    if v.get("ok") != Some(&Value::Bool(true)) {
        return Err(format!("Slack's users.info: {}", v.get("error").and_then(Value::as_str).unwrap_or("refused")));
    }
    v.pointer("/user/profile/email").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| "Slack doesn't say their email (the bot needs `users:read.email`)".to_owned())
}

/// A reply in a Slack thread kiln started: posted as its person, the one here with their email.
pub(crate) async fn slack_reply(hub: &Arc<Hub>, i: &Integration, event: &Value) -> Result<(), String> {
    let get = |k: &str| event.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    let (channel, parent, user, text) = (get("channel"), get("thread_ts"), get("user"), get("text"));
    let (id, ch, ts) = (i.id.clone(), channel.clone(), parent.clone());
    let thread: Option<String> = hub
        .store
        .call(move |c| Ok(c.query_row("SELECT thread_id FROM review_chat_posts WHERE integration = ?1 AND channel = ?2 AND ts = ?3", params![id, ch, ts], |r| r.get(0)).optional()?))
        .await
        .map_err(|e| format!("{e:#}"))?;
    let thread = thread.ok_or("it's in a thread kiln didn't start")?;
    let token = integrations::token(hub, i).await.map_err(|e| format!("{e:#}"))?.filter(|t| t.starts_with("xox")).ok_or("the integration has no bot token")?;
    let address = slack_email(i, &token, &user).await?;
    let person = super::chat::user_by_email(hub, &address).await?;
    let text = text.trim();
    if text.is_empty() {
        return Err("it says nothing".into());
    }
    post(hub, &thread, &person, text).await?;
    tracing::info!(integration = %i.name, %thread, from = %address, "a Slack reply was posted to its thread");
    Ok(())
}

/// `POST /api/ci/replies/slack/:integration/:key`: Slack's Events API, at the integration's own
/// address (`key`, from the server's master key), which takes its commands and buttons too
/// (`crate::ci::chat::slack`).
pub async fn slack_events(hub: &Arc<Hub>, id: &str, key: &str, req: Request) -> Result<Response, Fail> {
    super::chat::inbound(hub, "slack", id, key, req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w14_r05_reply_addresses() {
        assert_eq!(reply_to("review@example.com", "abc").as_deref(), Some("review+abc@example.com"));
        assert_eq!(reply_to("kiln <review@example.com>", "abc").as_deref(), Some("kiln <review+abc@example.com>"));
        assert_eq!(reply_to("nobody", "abc"), None);
        assert_eq!(token_in("Review+AbC@Example.com", "review@example.com").as_deref(), Some("abc"));
        assert_eq!(token_in("review@example.com", "review@example.com"), None);
        assert_eq!(token_in("other+abc@example.com", "review@example.com"), None);
    }

    #[test]
    fn w14_r05_a_reply_read_from_its_message() {
        let raw = "From: Rev <rev@example.test>\r\nTo: \"kiln, review\" <review+t0k@example.test>\r\nSubject: Re: D1\r\nContent-Type: multipart/alternative; boundary=\"b1\"\r\n\r\n--b1\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\nLooks good =E2=80=94 landing it.\r\n\r\nOn Tue, 21 Oct 2026, kiln <review@example.test> wrote:\r\n> =F0=9F=92=AC dev replied on D1\r\n--b1\r\nContent-Type: text/html\r\n\r\n<p>Looks good</p>\r\n--b1--\r\n";
        let m = parse(raw.as_bytes());
        assert_eq!(m.from, "rev@example.test");
        assert_eq!(m.to, ["review+t0k@example.test"]);
        assert_eq!(said(&m.text), "Looks good — landing it.");
        let b64 = "From: rev@example.test\r\nTo: review+x@example.test\r\nContent-Type: text/plain\r\nContent-Transfer-Encoding: base64\r\n\r\nWWVzLCBwbGVhc2Uu\r\n";
        assert_eq!(parse(b64.as_bytes()).text, "Yes, please.");
        assert_eq!(said("Fine by me.\n-- \nRev, on a phone\n"), "Fine by me.");
        assert_eq!(said("Agreed.\n\nOn Tue, Oct 21, 2026 at 9:00 AM kiln\n<review@example.test> wrote:\n> quoted"), "Agreed.");
        assert_eq!(said("Agreed.\n\nFrom: kiln <review@example.test>\nSent: Tuesday\n"), "Agreed.");
        assert_eq!(addresses("Ada <ada@example.com>, \"Lovelace, A\" <al@example.com>, bob@example.com"), ["ada@example.com", "al@example.com", "bob@example.com"]);
    }
}

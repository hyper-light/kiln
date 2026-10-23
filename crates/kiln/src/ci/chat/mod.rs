//! Chat (docs/deploy-api.md §12.3): Slack first, then Discord, Telegram and Teams.
//!
//! - **A release's message** goes to each of its channels once and is edited as the release moves,
//!   where the channel can change a message afterwards: a Slack bot's (`chat.update`), a Discord
//!   webhook's (its message patched), a Telegram bot's (`editMessageText`) ([`posts`]). While the
//!   release waits for approval the message carries Approve and Reject, where kiln takes that
//!   chat's buttons: a Slack app whose signing secret kiln has, a Telegram bot whose webhook is
//!   kiln's. A channel that can't change a message (an incoming webhook, Teams' Workflows) gets
//!   one message per event, as before (`crate::ci::notify::release_moved`).
//! - **Commands** (`deploy`, `approve`, `reject`, `rollback`, `status`, `link`, `help`) come from
//!   each chat to its integration's own address (`/api/ci/chat/<kind>/<integration>/<key>`, the key
//!   from the server's master key), checked as that chat signs them: a Slack app's signing secret
//!   ([`slack`]), a Discord application's Ed25519 key ([`discord`]), a Telegram bot's secret token
//!   ([`telegram`]), a Teams outgoing webhook's HMAC ([`teams`]). Each runs as the person's kiln
//!   user, through the deploy operation it is (`deploy.releases.create`, `.approve`, `.reject`,
//!   `.rollback`, and reads): judged by the route table, held by the environment's gates and agents
//!   policy, and audited, as that route is.
//! - **Whose** a chat account is: the kiln user who linked it (a code from Settings → You → Chat
//!   accounts, said to kiln in the chat: `link <code>`), or in Slack the one person here with the
//!   email Slack has for it (`users.info`). A bot's account (another app's, an agent's) acts as an
//!   agent for the person who linked it, so the environment's agents policy holds it.

pub mod discord;
pub mod posts;
pub mod slack;
pub mod teams;
pub mod telegram;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use http::{Method, StatusCode};
use rusqlite::{OptionalExtension as _, params};
use serde_json::{Value, json};

use super::Fail;
use super::integrations::{self, Integration};
use crate::authz::Caller;
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;

/// How long a link code works.
const CODE_SECS: i64 = 600;
/// How long a command's answer may take before its chat is told to wait (each wants one in 3 s).
const ANSWER_IN: Duration = Duration::from_millis(2500);
/// A link code's letters: no 0, O, 1 or I.
const CODE_LETTERS: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// What kiln takes, said when asked or when a command doesn't read.
pub const HELP: &str = "kiln takes: `deploy <app> <version> to <environment>` · `approve <release> [why]` · `reject <release> [why]` · `rollback <app> in <environment> [to <release>]` · `status <app> [in <environment>]`, or `status <release>` · `link <code>`, with a code from kiln's Settings → You → Chat accounts";

// ------------------------------------------------------------------ commands

/// A command, as said in a chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Help,
    Link(String),
    Status(Status),
    Deploy { app: String, version: String, environment: String },
    Approve { release: i64, note: Option<String> },
    Reject { release: i64, note: Option<String> },
    Rollback { app: String, environment: String, to: Option<i64> },
}

/// What `status` asks about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Release(i64),
    App(String),
    In { app: String, environment: String },
}

/// A release's number: `42`, or `#42`.
fn number(s: &str) -> Option<i64> {
    s.trim_start_matches('#').parse::<i64>().ok().filter(|n| *n > 0)
}

/// Words that only join others: `deploy web 1.4.2 to production`.
fn joining(w: &str) -> bool {
    matches!(w.to_ascii_lowercase().as_str(), "to" | "in" | "from" | "on" | "at")
}

/// `text` as a command: its first word says which (a leading `/`, as Telegram writes commands, and
/// a bot's name after `@`, are let go).
pub fn parse(text: &str) -> Result<Command, String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let Some((first, rest)) = words.split_first() else { return Ok(Command::Help) };
    let verb = first.trim_start_matches('/');
    let verb = verb.split_once('@').map_or(verb, |(v, _)| v).to_ascii_lowercase();
    // `/kiln approve 42` in a chat whose command is kiln's name.
    if verb == "kiln" {
        return parse(&rest.join(" "));
    }
    let plain: Vec<&str> = rest.iter().copied().filter(|w| !joining(w)).collect();
    match verb.as_str() {
        "" | "help" | "start" => Ok(Command::Help),
        "link" => match rest {
            [code] => Ok(Command::Link((*code).to_owned())),
            _ => Err("Say `link <code>`, with the code kiln's Settings → You → Chat accounts gives you.".into()),
        },
        "deploy" | "release" => match plain.as_slice() {
            [app, version, environment] => Ok(Command::Deploy { app: (*app).to_owned(), version: (*version).to_owned(), environment: (*environment).to_owned() }),
            _ => Err("Say `deploy <app> <version> to <environment>`.".into()),
        },
        "approve" | "reject" => {
            let (n, why) = rest.split_first().ok_or_else(|| format!("Say `{verb} <release>`, the release's number."))?;
            let release = number(n).ok_or_else(|| format!("Say `{verb} <release>`: `{n}` isn't a release's number."))?;
            let note = Some(why.join(" ")).filter(|n| !n.is_empty());
            Ok(if verb == "approve" { Command::Approve { release, note } } else { Command::Reject { release, note } })
        }
        "rollback" => {
            // `rollback web in production to 38`: the number after the last `to`, if one is.
            let (head, to) = match rest.iter().rposition(|w| w.eq_ignore_ascii_case("to")).and_then(|at| rest.get(at + 1).and_then(|n| number(n)).map(|n| (at, n))) {
                Some((at, n)) => (rest.get(..at).unwrap_or_default(), Some(n)),
                None => (rest, None),
            };
            let plain: Vec<&str> = head.iter().copied().filter(|w| !joining(w)).collect();
            match plain.as_slice() {
                [app, environment] => Ok(Command::Rollback { app: (*app).to_owned(), environment: (*environment).to_owned(), to }),
                _ => Err("Say `rollback <app> in <environment>`, and `to <release>` for one but the last that landed.".into()),
            }
        }
        "status" => match plain.as_slice() {
            [one] => Ok(Command::Status(number(one).map_or_else(|| Status::App((*one).to_owned()), Status::Release))),
            [app, environment] => Ok(Command::Status(Status::In { app: (*app).to_owned(), environment: (*environment).to_owned() })),
            _ => Err("Say `status <app>`, `status <app> in <environment>` or `status <release>`.".into()),
        },
        other => Err(format!("`{other}` isn't something kiln does. {HELP}")),
    }
}

/// Who said a command, as their chat knows them.
#[derive(Debug, Clone, Default)]
pub struct Speaker {
    /// Their account there: Slack's `U…` (a bot's `B…`), Discord's, Telegram's, Teams' AAD id.
    pub id: String,
    /// What the chat calls them.
    pub name: String,
    /// A bot's (another app's, an agent's): it acts as an agent, at most.
    pub bot: bool,
}

/// `text` said by `who` through `i`, done as the person they are here, and what to say back. `key`
/// is what the chat calls this request, so a release asked for twice (a retried delivery) is one.
pub async fn run(hub: &Arc<Hub>, i: &Integration, who: &Speaker, text: &str, key: Option<&str>) -> String {
    match Box::pin(answer(hub, i, who, text, key)).await {
        Ok(said) | Err(said) => said,
    }
}

async fn answer(hub: &Arc<Hub>, i: &Integration, who: &Speaker, text: &str, key: Option<&str>) -> Result<String, String> {
    let command = parse(text)?;
    let idempotency = key.map(|k| format!("chat:{}:{k}", i.id));
    let (op, input) = match &command {
        Command::Help => return Ok(HELP.to_owned()),
        Command::Link(code) => return link(hub, i, who, code).await,
        Command::Deploy { app, version, environment } => ("deploy.releases.create", json!({ "app": app, "environment": environment, "version": version, "idempotency_key": idempotency })),
        Command::Approve { release, note } => ("deploy.releases.approve", json!({ "id": release, "note": note })),
        Command::Reject { release, note } => ("deploy.releases.reject", json!({ "id": release, "note": note })),
        Command::Rollback { app, environment, to } => ("deploy.releases.rollback", json!({ "app": app, "environment": environment, "to": to, "idempotency_key": idempotency })),
        Command::Status(Status::Release(id)) => ("deploy.releases.get", json!({ "id": id })),
        Command::Status(Status::App(app)) => ("deploy.apps.get", json!({ "app": app })),
        Command::Status(Status::In { app, environment }) => ("deploy.registrations.get", json!({ "app": app, "environment": environment })),
    };
    let caller = person(hub, i, who).await?;
    let input = strip_nulls(input);
    let done = Box::pin(crate::deploy::ops::call(crate::ops::Cx { hub: Arc::clone(hub), caller: caller.clone() }, op, input)).await;
    let v = done.map_err(|(status, m)| if status == StatusCode::FORBIDDEN { format!("kiln won't, as {}: {m}", caller.name) } else { m })?;
    let url = |id: i64| format!("{}/deployments/releases/{id}", hub.cfg.server.public_url.trim_end_matches('/'));
    Ok(match &command {
        Command::Deploy { .. } | Command::Rollback { .. } => format!("Asked: {}", said_of(&v, &url)),
        // Still held until the worker looks again: what it waits for may be this very approval.
        Command::Approve { .. } if v.get("state").and_then(Value::as_str) == Some("waiting_approval") => {
            format!("You approved {}: it goes on once it has every approval it needs · {}", headline(&v), url(v.get("id").and_then(Value::as_i64).unwrap_or_default()))
        }
        Command::Approve { .. } => format!("You approved it: {}", said_of(&v, &url)),
        Command::Reject { .. } => format!("You rejected it: {}", said_of(&v, &url)),
        Command::Status(Status::Release(_)) => said_of(&v, &url),
        Command::Status(Status::In { .. }) => in_one(&v, &url),
        Command::Status(Status::App(app)) => {
            let flight = crate::deploy::ops::call(crate::ops::Cx { hub: Arc::clone(hub), caller }, "deploy.releases.list", json!({ "app": app, "limit": 20 })).await.unwrap_or(Value::Null);
            of_app(&v, &flight, &url)
        }
        Command::Help | Command::Link(_) => String::new(),
    })
}

/// An operation's input without the fields left unsaid.
fn strip_nulls(mut v: Value) -> Value {
    if let Some(m) = v.as_object_mut() {
        m.retain(|_, x| !x.is_null());
    }
    v
}

/// A release's state, in words.
fn state_words(r: &Value) -> String {
    let get = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or_default();
    let why = get("message").lines().next().unwrap_or_default().to_owned();
    let with = |s: &str| if why.is_empty() { s.to_owned() } else { format!("{s}: {why}") };
    match get("state") {
        "waiting" => with("waits"),
        "waiting_approval" => {
            let approvers = approvers(r);
            if approvers.is_empty() { "waits for approval".to_owned() } else { format!("waits for approval ({})", approvers.join(", ")) }
        }
        "committing" => "on its way (being committed)".to_owned(),
        "delivering" => "on its way (being delivered)".to_owned(),
        "succeeded" => "landed".to_owned(),
        "failed" => with("failed"),
        "cancelled" => with("cancelled"),
        "rolled_back" => "landed, then rolled back".to_owned(),
        "superseded" => "landed, and replaced since".to_owned(),
        other => other.to_owned(),
    }
}

/// Who may approve a release waiting for approval, as its approvals gate says.
fn approvers(r: &Value) -> Vec<String> {
    let from_gate = r.get("gates").and_then(Value::as_array).into_iter().flatten().find(|g| g.get("gate").and_then(Value::as_str) == Some("approvals")).and_then(|g| g.pointer("/evidence/reviewers"));
    r.get("approvers").or(from_gate).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect()
}

/// Which release it is: `release 42, web 1.4.2 → production`.
fn headline(r: &Value) -> String {
    let get = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or_default();
    let id = r.get("id").and_then(Value::as_i64).unwrap_or_default();
    let what = match get("kind") {
        "rollback" => format!("a rollback to {}", get("version")),
        "config" => "new values".to_owned(),
        "enable" => "enabling".to_owned(),
        "disable" => "disabling".to_owned(),
        _ => get("version").to_owned(),
    };
    format!("release {id}, {} {what} → {}", get("app"), get("environment"))
}

/// What a release is: `release 42, web 1.4.2 → production, waits for approval (grace) · <its page>`.
pub fn said_of(r: &Value, url: &dyn Fn(i64) -> String) -> String {
    let id = r.get("id").and_then(Value::as_i64).unwrap_or_default();
    format!("{}, {} · {}", headline(r), state_words(r), url(id))
}

/// What runs for an app in one environment, and what's on its way there.
fn in_one(v: &Value, url: &dyn Fn(i64) -> String) -> String {
    let reg = v.get("registration").cloned().unwrap_or(Value::Null);
    let get = |k: &str| reg.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    let mut lines = vec![match v.get("running").filter(|r| !r.is_null()) {
        Some(r) => format!("{} in {}: runs {}", get("app"), get("environment"), said_of(r, url)),
        None => format!("{} in {}: nothing runs there yet", get("app"), get("environment")),
    }];
    if let Some(r) = v.get("in_flight").filter(|r| !r.is_null()) {
        lines.push(format!("on its way: {}", said_of(r, url)));
    }
    if let Some(why) = reg.get("degraded").and_then(Value::as_str) {
        lines.push(format!("needs a person: {why}"));
    }
    lines.join("\n")
}

/// An app's environments: what runs in each, and what's on its way.
fn of_app(v: &Value, releases: &Value, url: &dyn Fn(i64) -> String) -> String {
    let name = v.pointer("/app/name").and_then(Value::as_str).unwrap_or_default();
    let mut lines: Vec<String> = Vec::new();
    for e in v.get("environments").and_then(Value::as_array).into_iter().flatten() {
        let env = e.pointer("/registration/environment").and_then(Value::as_str).unwrap_or_default();
        lines.push(match e.get("running").filter(|r| !r.is_null()) {
            Some(r) => format!("{env}: runs {}", said_of(r, url)),
            None => format!("{env}: nothing runs there yet"),
        });
    }
    for r in releases.get("releases").and_then(Value::as_array).into_iter().flatten() {
        if matches!(r.get("state").and_then(Value::as_str), Some("waiting" | "waiting_approval" | "committing" | "delivering")) {
            lines.push(format!("on its way: {}", said_of(r, url)));
        }
    }
    if lines.is_empty() {
        return format!("{name} isn't registered in any environment yet");
    }
    format!("{name}\n{}", lines.join("\n"))
}

// ------------------------------------------------------------------ whose account

/// The kiln user `who` is here, as a caller: a person, or an agent for one.
async fn person(hub: &Hub, i: &Integration, who: &Speaker) -> Result<Caller, String> {
    let uid = match linked(hub, &i.id, &who.id).await {
        Some(uid) => uid,
        None if i.kind == "slack" && !who.bot => slack::person_by_email(hub, i, &who.id).await?,
        None => return Err(format!("kiln doesn't know who you are in {} yet: kiln's Settings → You → Chat accounts gives you a code; say `link <code>` here.", i.name)),
    };
    let caller = if who.bot { crate::authz::gate::agent_for(hub, &format!("{}:{}", i.kind, who.name), &format!("user:{uid}")).await } else { crate::authz::gate::user_caller(hub, &uid).await };
    caller.ok_or_else(|| "The kiln account this chat account is linked to can't sign in any more.".to_owned())
}

/// The user a chat account was linked to, by id.
async fn linked(hub: &Hub, integration: &str, chat_user: &str) -> Option<String> {
    let (i, u) = (integration.to_owned(), chat_user.to_owned());
    hub.store.call(move |c| Ok(c.query_row("SELECT user_id FROM chat_links WHERE integration = ?1 AND chat_user = ?2", params![i, u], |r| r.get(0)).optional()?)).await.ok().flatten()
}

/// The one live person here with `email`, by id: none, or more than one, isn't anyone.
pub(crate) async fn user_by_email(hub: &Hub, email: &str) -> Result<String, String> {
    let e = email.trim().to_owned();
    let ids: Vec<String> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT id FROM users WHERE email = ?1 COLLATE NOCASE AND disabled_at IS NULL LIMIT 2")?;
            let rows = st.query_map([&e], |r| r.get(0))?;
            Ok(rows.collect::<Result<Vec<String>, _>>()?)
        })
        .await
        .map_err(|e| format!("{e:#}"))?;
    match ids.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(format!("Nobody in kiln has the address {email}: link your account (kiln's Settings → You → Chat accounts), or give yours that address.")),
        _ => Err(format!("More than one person in kiln has the address {email}: link your account instead (kiln's Settings → You → Chat accounts).")),
    }
}

/// A link code as kept: its SHA-256, its letters as they're said (any case, any dashes).
fn code_hash(code: &str) -> String {
    let letters: String = code.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_uppercase()).collect();
    crate::util::sha256_hex(letters.as_bytes())
}

/// `link <code>`: the chat account saying it is the code's person's from now on.
async fn link(hub: &Hub, i: &Integration, who: &Speaker, code: &str) -> Result<String, String> {
    let (hash, integration, chat_user, name, now) = (code_hash(code), i.id.clone(), who.id.clone(), who.name.clone(), crate::util::now_unix());
    let user: Option<String> = hub
        .store
        .call(move |c| {
            c.execute("DELETE FROM chat_link_codes WHERE expires_at < ?1", [now])?;
            // Used once: taken as it's read.
            let user: Option<String> = c.query_row("DELETE FROM chat_link_codes WHERE code = ?1 RETURNING user_id", [&hash], |r| r.get(0)).optional()?;
            if let Some(u) = &user {
                c.execute(
                    "INSERT INTO chat_links (integration, chat_user, user_id, name, created_at) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (integration, chat_user) DO UPDATE SET user_id = ?3, name = ?4, created_at = ?5",
                    params![integration, chat_user, u, name, now],
                )?;
            }
            Ok(user)
        })
        .await
        .map_err(|e| format!("{e:#}"))?;
    let user = user.ok_or("That isn't a code kiln gave, or it's been used, or it's older than ten minutes: kiln's Settings → You → Chat accounts gives you another.")?;
    let caller = crate::authz::gate::user_caller(hub, &user).await.ok_or("The kiln account that code is for can't sign in any more.")?;
    hub.store.audit(caller.name.clone(), "chat.link".into(), format!("{} {} ({})", i.name, who.id, who.name)).await;
    Ok(if who.bot {
        format!("Linked: this bot acts here as an agent for {} in kiln, as far as each environment lets agents.", caller.name)
    } else {
        format!("Linked: here you're {} in kiln.", caller.name)
    })
}

/// A new link code's letters: `KILN-7Q4M-ZD2X`.
fn new_code() -> String {
    let bytes: [u8; 8] = rand::random();
    let letters: String = bytes.iter().map(|b| char::from(CODE_LETTERS.get(usize::from(*b) % CODE_LETTERS.len()).copied().unwrap_or(b'X'))).collect();
    format!("KILN-{}-{}", letters.get(..4).unwrap_or_default(), letters.get(4..).unwrap_or_default())
}

/// `/api/me/chat…`: the chat accounts linked to the caller's (`GET`), a code to link one with
/// (`POST /api/me/chat/codes`: `link <code>`, said there within ten minutes), and one unlinked
/// (`DELETE /api/me/chat/<integration>/<account>`). A code is a signed-in person's: a chat account
/// acts with all their grants, which no token of theirs may hand out.
pub async fn mine(hub: &Hub, caller: &Caller, method: &Method, rest: &[&str]) -> Result<Response, Fail> {
    let internal = |e: anyhow::Error| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    let Some(uid) = caller.user.clone() else { return Err((StatusCode::FORBIDDEN, "a chat account is linked to a person's kiln account: sign in as one".into())) };
    match (method, rest) {
        (&Method::GET, []) => {
            let all = integrations::list(hub).await.map_err(internal)?;
            let rows: Vec<(String, String, String, i64)> = hub
                .store
                .call(move |c| {
                    let mut st = c.prepare("SELECT integration, chat_user, name, created_at FROM chat_links WHERE user_id = ?1 ORDER BY created_at")?;
                    let rows = st.query_map([&uid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
                    Ok(rows.collect::<Result<Vec<_>, _>>()?)
                })
                .await
                .map_err(internal)?;
            let links: Vec<Value> = rows
                .into_iter()
                .map(|(integration, account, name, at)| {
                    let i = all.iter().find(|i| i.id == integration);
                    json!({ "integration": i.map(|i| i.name.clone()), "kind": i.map(|i| i.kind.clone()), "id": integration, "account": account, "name": name, "created_at": at })
                })
                .collect();
            let chats: Vec<Value> = all.iter().filter(|i| matches!(i.kind.as_str(), "slack" | "discord" | "telegram" | "teams")).map(|i| json!({ "name": i.name, "kind": i.kind })).collect();
            Ok(srv::json(StatusCode::OK, &json!({ "links": links, "chats": chats })))
        }
        (&Method::POST, ["codes"]) => {
            if caller.kind != crate::authz::gate::Kind::User {
                return Err((StatusCode::FORBIDDEN, "a link code is made signed in to kiln as yourself, not with a token: the chat account it links acts with all your grants".into()));
            }
            let code = new_code();
            let (hash, expires) = (code_hash(&code), crate::util::now_unix() + CODE_SECS);
            hub.store
                .call(move |c| {
                    c.execute("DELETE FROM chat_link_codes WHERE user_id = ?1 OR expires_at < ?2", params![uid, crate::util::now_unix()])?;
                    c.execute("INSERT INTO chat_link_codes (code, user_id, expires_at) VALUES (?1, ?2, ?3)", params![hash, uid, expires])?;
                    Ok(())
                })
                .await
                .map_err(internal)?;
            Ok(srv::json(StatusCode::CREATED, &json!({ "code": code, "expires_at": expires, "say": format!("link {code}") })))
        }
        (&Method::DELETE, [integration, account]) => {
            let all = integrations::list(hub).await.map_err(internal)?;
            let id = all.iter().find(|i| i.name == *integration || i.id == *integration).map_or_else(|| (*integration).to_owned(), |i| i.id.clone());
            let (account, who) = ((*account).to_owned(), caller.name.clone());
            let gone = hub.store.call(move |c| Ok(c.execute("DELETE FROM chat_links WHERE integration = ?1 AND chat_user = ?2 AND user_id = ?3", params![id, account, uid])?)).await.map_err(internal)?;
            if gone == 0 {
                return Err((StatusCode::NOT_FOUND, "no chat account of yours by that name".into()));
            }
            hub.store.audit(who, "chat.unlink".into(), format!("{integration} {}", rest.get(1).copied().unwrap_or_default())).await;
            Ok(srv::status(StatusCode::NO_CONTENT))
        }
        _ => Err((StatusCode::NOT_FOUND, "no such route".into())),
    }
}

// ------------------------------------------------------------------ addresses

/// The key in an integration's chat address: Slack's is its events address's (W14-R05), so one
/// address takes its events, commands and buttons.
fn key(hub: &Hub, kind: &str, id: &str) -> anyhow::Result<String> {
    if kind == "slack" {
        return super::replies::events_key(hub, id);
    }
    super::secrets::derived(hub, &format!("kiln ci chat v1\n{kind}\n{id}"))
}

/// The address `i`'s chat sends kiln commands at.
pub fn address(hub: &Hub, i: &Integration) -> Option<String> {
    let key = key(hub, &i.kind, &i.id).ok()?;
    Some(format!("{}/api/ci/chat/{}/{}/{key}", hub.cfg.server.public_url.trim_end_matches('/'), i.kind, i.id))
}

/// `POST /api/ci/chat/<kind>/<integration>/<key>`: what a chat sends kiln, at that integration's
/// own address, then checked as the chat signs it.
pub async fn inbound(hub: &Arc<Hub>, kind: &str, id: &str, given: &str, req: Request) -> Result<Response, Fail> {
    let want = key(hub, kind, id).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    if !crate::util::ct_eq(want.as_bytes(), given.as_bytes()) {
        return Err((StatusCode::NOT_FOUND, "not found".into()));
    }
    let i = integrations::list(hub)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
        .into_iter()
        .find(|i| i.id == id && i.kind == kind)
        .ok_or((StatusCode::NOT_FOUND, "not found".into()))?;
    match kind {
        "slack" => Box::pin(slack::inbound(hub, i, req)).await,
        "discord" => Box::pin(discord::inbound(hub, i, req)).await,
        "telegram" => Box::pin(telegram::inbound(hub, i, want, req)).await,
        "teams" => Box::pin(teams::inbound(hub, i, req)).await,
        _ => Err((StatusCode::NOT_FOUND, "not found".into())),
    }
}

/// `GET /api/ci/integrations/<name>/chat`: where `i`'s chat sends kiln what it says, and what to set
/// there; `POST`: for a Telegram bot, its webhook set to kiln's address, so its chats' commands and
/// buttons come here.
pub async fn setup(hub: &Hub, i: &Integration, set: bool) -> Result<Value, String> {
    let address = address(hub, i).ok_or("the server's master key isn't there to make an address with")?;
    let v = match i.kind.as_str() {
        "slack" => json!({
            "address": address,
            "events_url": address,
            "interactivity_url": address,
            "commands_url": address,
            "signing_secret": i.has_signing_secret,
            "scopes": ["chat:write", "commands", "users:read", "users:read.email", "app_mentions:read"],
            "events": ["message.channels", "message.groups", "app_mention"],
            "how": "In the Slack app: Event Subscriptions, Interactivity and a slash command (/kiln), each with this address; its signing secret on this integration (`signing_secret`).",
        }),
        "discord" => json!({
            "address": address,
            "interactions_url": address,
            "public_key": i.config.get("public_key").and_then(Value::as_str).is_some_and(|k| !k.trim().is_empty()),
            "command": discord::command(),
            "how": "In the Discord application: this address as its Interactions Endpoint URL, its public key on this integration (`public_key`), and the /kiln command registered (PUT /applications/<id>/commands with the bot's token).",
        }),
        "telegram" => {
            if set {
                Box::pin(telegram::set_webhook(hub, i, &address)).await?;
            }
            json!({ "address": address, "commands": i.config.get("commands").and_then(Value::as_str) == Some("on") || set, "how": "POST here to point the bot's webhook at kiln (it takes one webhook: whatever else read its updates stops)." })
        }
        "teams" => json!({
            "address": address,
            "outgoing_webhook_url": address,
            "signing_secret": i.has_signing_secret,
            "how": "In the team: an outgoing webhook with this address as its callback URL; the security token it shows, on this integration (`signing_secret`). Then @-mention it: `@kiln status web`.",
        }),
        other => return Err(format!("a {other} integration takes no commands")),
    };
    Ok(v)
}

// ------------------------------------------------------------------ answering in time

/// `work`'s answer when it comes in time for the chat that asked ([`ANSWER_IN`]); otherwise `None`,
/// and `later` gets it once it comes.
pub(crate) async fn soon<W, L, F>(work: W, later: L) -> Option<String>
where
    W: Future<Output = String> + Send + 'static,
    L: FnOnce(String) -> F + Send + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    let mut task = crate::rt::spawn(work);
    match crate::rt::timeout(ANSWER_IN, &mut task).await {
        Ok(Ok(said)) => Some(said),
        Ok(Err(_)) => Some("kiln couldn't finish that: try again.".into()),
        Err(_) => {
            crate::rt::spawn(async move {
                if let Ok(said) = task.await {
                    later(said).await;
                }
            });
            None
        }
    }
}

/// Sends `body` to `url` as JSON and reads the answer: `reach` for an address someone gave kiln
/// (a webhook's), which must be one it may reach (W22-AC07).
pub(crate) async fn call(hub: &Hub, method: crate::http::client::Method, url: &str, bearer: Option<&str>, body: &Value, reach: bool) -> Result<Value, String> {
    let client = crate::http::client::Client::public().map_err(|e| format!("{e:#}"))?;
    let payload = serde_json::to_vec(body).map_err(|e| e.to_string())?;
    let mut req = client.request(method, url).header("content-type", "application/json").body(payload).timeout(Duration::from_secs(20));
    if reach {
        req = req.reaching(&hub.cfg.ci.reach());
    }
    if let Some(b) = bearer {
        req = req.bearer(b);
    }
    // On the heap: the request's future (the reach check, TLS) is deep.
    let resp = Box::pin(req.send()).await.map_err(|e| format!("{e:#}"))?;
    let status = resp.status();
    let answer = resp.bytes(1 << 20).await.unwrap_or_default();
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}: {}", crate::util::truncate(String::from_utf8_lossy(&answer).trim(), 200)));
    }
    Ok(serde_json::from_slice(&answer).unwrap_or(Value::Null))
}

/// The body of a request a chat sent, read whole (they're small).
pub(crate) async fn body(req: Request) -> Result<(http::HeaderMap, bytes::Bytes), Fail> {
    let (parts, body) = req.into_parts();
    let bytes = srv::body_bytes(body, 1 << 20).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    Ok((parts.headers, bytes))
}

/// A header's value, as text.
pub(crate) fn header<'a>(h: &'a http::HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

/// A form's fields (`application/x-www-form-urlencoded`).
pub(crate) fn form(body: &[u8]) -> std::collections::HashMap<String, String> {
    String::from_utf8_lossy(body)
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (srv::decode(k), srv::decode(v))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_read_as_people_say_them() {
        let deploy = |a: &str, v: &str, e: &str| Command::Deploy { app: a.into(), version: v.into(), environment: e.into() };
        assert_eq!(parse("deploy web 1.4.2 to production"), Ok(deploy("web", "1.4.2", "production")));
        assert_eq!(parse("Deploy web 1.4.2 production"), Ok(deploy("web", "1.4.2", "production")));
        assert_eq!(parse("/deploy@kiln_bot web 1.4.2 in staging"), Ok(deploy("web", "1.4.2", "staging")), "Telegram's way");
        assert_eq!(parse("/kiln deploy web 1.4.2 to staging"), Ok(deploy("web", "1.4.2", "staging")));
        assert!(parse("deploy web").is_err_and(|e| e.contains("deploy <app> <version> to <environment>")));
        assert_eq!(parse("approve 42"), Ok(Command::Approve { release: 42, note: None }));
        assert_eq!(parse("approve #42 looks good"), Ok(Command::Approve { release: 42, note: Some("looks good".into()) }));
        assert_eq!(parse("reject 42 not during the sale"), Ok(Command::Reject { release: 42, note: Some("not during the sale".into()) }));
        assert!(parse("approve it").is_err_and(|e| e.contains("isn't a release's number")));
        assert_eq!(parse("rollback web in production"), Ok(Command::Rollback { app: "web".into(), environment: "production".into(), to: None }));
        assert_eq!(parse("rollback web production to 38"), Ok(Command::Rollback { app: "web".into(), environment: "production".into(), to: Some(38) }));
        assert_eq!(parse("status 42"), Ok(Command::Status(Status::Release(42))));
        assert_eq!(parse("status web"), Ok(Command::Status(Status::App("web".into()))));
        assert_eq!(parse("status web in staging"), Ok(Command::Status(Status::In { app: "web".into(), environment: "staging".into() })));
        assert_eq!(parse("link kiln-7q4m-zd2x"), Ok(Command::Link("kiln-7q4m-zd2x".into())));
        assert_eq!(parse(""), Ok(Command::Help));
        assert_eq!(parse("/start"), Ok(Command::Help));
        assert!(parse("dance").is_err_and(|e| e.contains("isn't something kiln does") && e.contains("deploy <app>")));
    }

    #[test]
    fn a_link_code_is_said_any_way() {
        let code = new_code();
        assert!(code.starts_with("KILN-") && code.len() == 14, "{code}");
        assert!(!code.trim_start_matches("KILN-").contains(['0', 'O', '1', 'I']), "{code}");
        assert_eq!(code_hash(&code), code_hash(&code.to_ascii_lowercase()));
        assert_eq!(code_hash(&code), code_hash(&code.replace('-', "")));
        assert_ne!(code_hash(&code), code_hash(&new_code()));
    }

    #[test]
    fn a_release_said_in_words() {
        let url = |id: i64| format!("https://kiln.example/deployments/releases/{id}");
        let r = json!({ "id": 42, "app": "web", "environment": "production", "kind": "deploy", "state": "waiting_approval", "version": "1.4.2", "gates": [{ "gate": "approvals", "evidence": { "reviewers": ["grace", "linus"] } }] });
        assert_eq!(said_of(&r, &url), "release 42, web 1.4.2 → production, waits for approval (grace, linus) · https://kiln.example/deployments/releases/42");
        let held = json!({ "id": 43, "app": "web", "environment": "staging", "kind": "rollback", "state": "waiting", "version": "1.4.1", "message": "staging is frozen: the sale\nmore" });
        assert_eq!(said_of(&held, &url), "release 43, web a rollback to 1.4.1 → staging, waits: staging is frozen: the sale · https://kiln.example/deployments/releases/43");
        let app = json!({ "app": { "name": "web" }, "environments": [{ "registration": { "environment": "staging" }, "running": { "id": 40, "app": "web", "environment": "staging", "kind": "deploy", "state": "succeeded", "version": "1.4.2" } }, { "registration": { "environment": "production" }, "running": null }] });
        let flight = json!({ "releases": [r, { "id": 40, "state": "succeeded" }] });
        assert_eq!(
            of_app(&app, &flight, &url),
            "web\nstaging: runs release 40, web 1.4.2 → staging, landed · https://kiln.example/deployments/releases/40\nproduction: nothing runs there yet\non its way: release 42, web 1.4.2 → production, waits for approval (grace, linus) · https://kiln.example/deployments/releases/42"
        );
    }
}

//! Slack (docs/deploy-api.md §12.3): one address for the app's events, interactivity and slash
//! command. With the app's signing secret on the integration, every request is checked with it
//! (`X-Slack-Signature`, `v0=` HMAC-SHA256 of `v0:<timestamp>:<body>`, at most five minutes old);
//! commands and buttons are taken only then. Without it, events are taken as before (W14-R05),
//! by the address's key alone.
//!
//! - A **slash command** (`/kiln approve 42`) is answered to its person alone: at once when it's
//!   quick, otherwise at its `response_url` when it's done.
//! - A **button** on a release's message (Approve, Reject) is its command (`approve 42`), answered
//!   the same way; the message itself changes as the release moves.
//! - An **@-mention** of the app (`@kiln status web`) is a command too, answered in its thread: a
//!   bot's (another app's, an agent's) acts as an agent for the person who linked it.
//! - A **reply** in the thread of a review message is posted to its review thread (W14-R05).

use std::sync::Arc;

use http::StatusCode;
use serde_json::{Value, json};

use super::super::Fail;
use super::super::integrations::{self, Integration};
use super::Speaker;
use crate::http::client::Method;
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;

/// How old a request may be (Slack's own advice: five minutes).
const SKEW: i64 = 300;

/// Whether `signature` is Slack's for `body` at `timestamp`, signed with `secret`, `now`.
pub fn signed(secret: &str, timestamp: &str, body: &[u8], signature: &str, now: i64) -> bool {
    let Ok(at) = timestamp.trim().parse::<i64>() else { return false };
    if (now - at).abs() > SKEW {
        return false;
    }
    let mut base = format!("v0:{timestamp}:").into_bytes();
    base.extend_from_slice(body);
    let want = format!("v0={}", crate::util::hmac_hex(secret.as_bytes(), &base));
    crate::util::ct_eq(want.as_bytes(), signature.trim().as_bytes())
}

/// The bot's token, which kiln answers and looks people up with.
async fn bot_token(hub: &Hub, i: &Integration) -> Result<String, String> {
    integrations::token(hub, i).await.map_err(|e| format!("{e:#}"))?.filter(|t| t.starts_with("xox")).ok_or_else(|| "the integration has no bot token (xoxb-…)".to_owned())
}

/// The one person here whose email Slack has for `user`, by id.
pub(crate) async fn person_by_email(hub: &Hub, i: &Integration, user: &str) -> Result<String, String> {
    let token = bot_token(hub, i).await?;
    let email = super::super::replies::slack_email(i, &token, user).await?;
    super::user_by_email(hub, &email).await
}

/// Whether kiln may answer at `url`, a `response_url` from a request Slack signed: Slack's own
/// hosts, or the integration's own `api`'s (a GovSlack's, a test's).
fn answers_at(i: &Integration, url: &str) -> bool {
    let host = |u: &str| u.split_once("://").map(|(_, r)| r.split(['/', '?']).next().unwrap_or_default().to_ascii_lowercase());
    let Some(h) = host(url) else { return false };
    let slack = url.starts_with("https://") && (h.ends_with(".slack.com") || h.ends_with(".slack-gov.com") || h == "slack.com");
    let own = i.config.get("api").and_then(Value::as_str).and_then(host).is_some_and(|api| api == h);
    slack || own
}

/// Says `text` to whoever asked, at `response_url`, as only they see it.
async fn answer_at(hub: &Hub, i: &Integration, url: &str, text: &str) {
    if !answers_at(i, url) {
        tracing::warn!(integration = %i.name, "a Slack response_url wasn't Slack's: no answer sent");
        return;
    }
    let body = json!({ "response_type": "ephemeral", "replace_original": false, "text": text });
    if let Err(e) = super::call(hub, Method::Post, url, None, &body, true).await {
        tracing::info!(integration = %i.name, error = %e, "a Slack command's answer didn't go");
    }
}

/// Says `text` in `channel`'s thread `ts`.
async fn say_in_thread(hub: &Hub, i: &Integration, channel: &str, ts: &str, text: &str) {
    let token = match bot_token(hub, i).await {
        Ok(t) => t,
        Err(e) => return tracing::info!(integration = %i.name, error = %e, "a Slack mention's answer didn't go"),
    };
    let url = integrations::endpoint("slack:api", || format!("{}/chat.postMessage", super::super::replies::slack_api(i)));
    let body = json!({ "channel": channel, "thread_ts": ts, "text": text });
    if let Err(e) = super::call(hub, Method::Post, &url, Some(&token), &body, false).await {
        tracing::info!(integration = %i.name, error = %e, "a Slack mention's answer didn't go");
    }
}

/// `@kiln approve 42` without its mentions.
fn unmentioned(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("<@") {
        out.push_str(rest.get(..at).unwrap_or_default());
        rest = rest.get(at..).and_then(|r| r.find('>').map(|end| r.get(end + 1..).unwrap_or_default())).unwrap_or_default();
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A request at the Slack integration `i`'s address.
pub async fn inbound(hub: &Arc<Hub>, i: Integration, req: Request) -> Result<Response, Fail> {
    let (headers, body) = super::body(req).await?;
    let signing = integrations::signing_secret(hub, &i).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let checked = match &signing {
        Some(secret) => {
            let (ts, sig) = (super::header(&headers, "x-slack-request-timestamp").unwrap_or_default(), super::header(&headers, "x-slack-signature").unwrap_or_default());
            if !signed(secret, ts, &body, sig, crate::util::now_unix()) {
                return Err((StatusCode::UNAUTHORIZED, "not signed with the Slack app's signing secret".into()));
            }
            true
        }
        None => false,
    };
    let form = super::header(&headers, "content-type").is_some_and(|t| t.starts_with("application/x-www-form-urlencoded"));
    if !form {
        return events(hub, i, &headers, &body, checked).await;
    }
    // Commands and buttons act as people: only what Slack signed.
    if !checked {
        return Err((StatusCode::UNAUTHORIZED, "Slack's commands and buttons are taken once the integration has the app's signing secret (`signing_secret`)".into()));
    }
    let f = super::form(&body);
    if let Some(payload) = f.get("payload") {
        let p: Value = serde_json::from_str(payload).map_err(|e| (StatusCode::BAD_REQUEST, format!("{e}")))?;
        return button(hub, i, &p).await;
    }
    let get = |k: &str| f.get(k).cloned().unwrap_or_default();
    let who = Speaker { id: get("user_id"), name: get("user_name"), bot: false };
    let (text, url, trigger) = (get("text"), get("response_url"), get("trigger_id"));
    let (h, i2) = (Arc::clone(hub), i.clone());
    let work = async move { super::run(&h, &i2, &who, &text, Some(&trigger).filter(|t| !t.is_empty()).map(String::as_str)).await };
    let (h, i3) = (Arc::clone(hub), i.clone());
    let said = super::soon(work, move |said| async move { answer_at(&h, &i3, &url, &said).await }).await;
    Ok(srv::json(StatusCode::OK, &json!({ "response_type": "ephemeral", "text": said.unwrap_or_else(|| "On it…".into()) })))
}

/// A button on a release's message: Approve or Reject, as the person who pressed it.
async fn button(hub: &Arc<Hub>, i: Integration, p: &Value) -> Result<Response, Fail> {
    let action = p.pointer("/actions/0").cloned().unwrap_or(Value::Null);
    let verb = match action.get("action_id").and_then(Value::as_str) {
        Some("kiln_approve") => "approve",
        Some("kiln_reject") => "reject",
        _ => return Ok(srv::status(StatusCode::OK)),
    };
    let release = action.get("value").and_then(Value::as_str).unwrap_or_default().to_owned();
    let get = |ptr: &str| p.pointer(ptr).and_then(Value::as_str).unwrap_or_default().to_owned();
    let who = Speaker { id: get("/user/id"), name: get("/user/username"), bot: false };
    let url = get("/response_url");
    let h = Arc::clone(hub);
    crate::rt::spawn(async move {
        let said = super::run(&h, &i, &who, &format!("{verb} {release}"), None).await;
        answer_at(&h, &i, &url, &said).await;
    });
    // Slack wants its answer in 3 s; the release's message changes by itself as it moves.
    Ok(srv::status(StatusCode::OK))
}

/// Slack's Events API: its check, @-mentions of the app (commands), and replies in review
/// messages' threads (W14-R05). Each is taken after the answer, which Slack wants in 3 s.
async fn events(hub: &Arc<Hub>, i: Integration, headers: &http::HeaderMap, body: &[u8], checked: bool) -> Result<Response, Fail> {
    let v: Value = serde_json::from_slice(body).map_err(|e| (StatusCode::BAD_REQUEST, format!("{e}")))?;
    match v.get("type").and_then(Value::as_str) {
        Some("url_verification") => Ok(srv::json(StatusCode::OK, &json!({ "challenge": v.get("challenge").cloned().unwrap_or(Value::Null) }))),
        Some("event_callback") => {
            // A retry of one already taken: once is enough.
            if super::header(headers, "x-slack-retry-num").is_some() {
                return Ok(srv::status(StatusCode::OK));
            }
            let event = v.get("event").cloned().unwrap_or(Value::Null);
            let get = |k: &str| event.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
            if get("type") == "app_mention" && checked {
                let bot = event.get("bot_id").and_then(Value::as_str).map(str::to_owned);
                let name = event.pointer("/bot_profile/name").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| get("username"));
                let who = Speaker { id: bot.clone().unwrap_or_else(|| get("user")), name, bot: bot.is_some() };
                let (channel, thread) = (get("channel"), Some(get("thread_ts")).filter(|t| !t.is_empty()).unwrap_or_else(|| get("ts")));
                let (text, key) = (unmentioned(&get("text")), v.get("event_id").and_then(Value::as_str).map(str::to_owned));
                let h = Arc::clone(hub);
                crate::rt::spawn(async move {
                    let said = super::run(&h, &i, &who, &text, key.as_deref()).await;
                    say_in_thread(&h, &i, &channel, &thread, &said).await;
                });
                return Ok(srv::status(StatusCode::OK));
            }
            let reply = get("type") == "message" && event.get("thread_ts").and_then(Value::as_str).is_some_and(|t| event.get("ts").and_then(Value::as_str) != Some(t)) && event.get("bot_id").is_none() && event.get("subtype").is_none();
            if reply {
                let h = Arc::clone(hub);
                crate::rt::spawn(async move {
                    if let Err(why) = super::super::replies::slack_reply(&h, &i, &event).await {
                        tracing::info!(integration = %i.name, why, "a Slack thread's reply was left");
                    }
                });
            }
            Ok(srv::status(StatusCode::OK))
        }
        _ => Ok(srv::status(StatusCode::OK)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slacks_signature_and_what_a_mention_says() {
        // Slack's own example (api.slack.com/authentication/verifying-requests-from-slack).
        let secret = "8f742231b10e8888abcd99yyyzzz85a5";
        let body = b"token=xyzz0WbapA4vBCDEFasx0q6G&team_id=T1DC2JH3J&team_domain=testteamnow&channel_id=G8PSS9T3V&channel_name=foobar&user_id=U2CERLKJA&user_name=roadrunner&command=%2Fwebhook-collect&text=&response_url=https%3A%2F%2Fhooks.slack.com%2Fcommands%2FT1DC2JH3J%2F397700885554%2F96rGlfmibIGlgcZRskXaIFfN&trigger_id=398738663015.47445629121.803a0bc887a14d10d2c447fce8b6703c";
        let sig = "v0=a2114d57b48eac39b9ad189dd8316235a7b4a8d21a10bd27519666489c69b503";
        assert!(signed(secret, "1531420618", body, sig, 1_531_420_618 + 60));
        assert!(!signed(secret, "1531420618", body, sig, 1_531_420_618 + 600), "too old");
        assert!(!signed("another", "1531420618", body, sig, 1_531_420_618), "another secret");
        assert!(!signed(secret, "1531420618", b"text=approve+42", sig, 1_531_420_618), "another body");
        assert_eq!(unmentioned("<@U0KILN> approve 42 <@U7|grace> fine"), "approve 42 fine");
        let i = Integration {
            id: "i".into(),
            kind: "slack".into(),
            name: "chat".into(),
            config: json!({ "api": "http://127.0.0.1:9000" }),
            has_secret: true,
            has_signing_secret: true,
            scope: "server".into(),
            created_by: String::new(),
            created_at: 0,
            updated_at: 0,
            verified_at: None,
            last_error: None,
            last_used_at: None,
        };
        assert!(answers_at(&i, "https://hooks.slack.com/commands/T1/2/x"));
        assert!(answers_at(&i, "http://127.0.0.1:9000/hooks/x"), "its own api's host");
        assert!(!answers_at(&i, "https://evil.example/hooks.slack.com"));
        assert!(!answers_at(&i, "http://hooks.slack.com/x"), "Slack's only over HTTPS");
    }
}

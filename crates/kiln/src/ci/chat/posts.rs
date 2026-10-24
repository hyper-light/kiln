//! A release's message where a chat can change a message afterwards (docs/deploy-api.md §12.3):
//! posted once to a channel and edited as the release moves. A Slack bot posts with
//! `chat.postMessage` and edits with `chat.update`; a Discord webhook posts with `?wait=true` and
//! patches `messages/<id>`; a Telegram bot sends with `sendMessage` and edits with
//! `editMessageText`. While the release waits for approval, Approve and Reject go with it where
//! kiln takes that chat's buttons: a Slack app whose signing secret kiln has; a Telegram bot whose
//! webhook is kiln's (`commands: on`).

use crate::http::client::Method;
use serde_json::{Value, json};

use super::super::integrations::{self, Integration};
use crate::hub::Hub;

/// A release's message, as a chat shows it.
#[derive(Debug, Clone)]
pub struct View {
    pub title: String,
    pub line: String,
    /// Waiting for approval: its number, for Approve and Reject.
    pub asks: Option<i64>,
}

/// Where a message is: its channel (a Slack channel's id, a Telegram chat's; Discord's webhook
/// needs none) and which message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Posted {
    pub channel: String,
    pub message: String,
}

/// Whether `i` can change a message it posts, with `secret` its stored one: a Slack bot token (an
/// incoming webhook can't), a Discord webhook, a Telegram bot.
pub fn edits(i: &Integration, secret: Option<&str>) -> bool {
    match i.kind.as_str() {
        "slack" => secret.is_some_and(|s| s.starts_with("xoxb-") || s.starts_with("xoxp-")),
        "discord" | "telegram" => secret.is_some(),
        _ => false,
    }
}

/// Whether kiln takes `i`'s buttons: they'd do nothing otherwise.
pub fn buttons(i: &Integration) -> bool {
    match i.kind.as_str() {
        "slack" => i.has_signing_secret,
        "telegram" => i.config.get("commands").and_then(Value::as_str) == Some("on"),
        _ => false,
    }
}

/// Slack's mrkdwn of plain text: `&`, `<` and `>` escaped.
fn mrkdwn(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// What `i` is sent for `v`: the body a post and an edit both carry.
pub fn body(i: &Integration, v: &View) -> Value {
    let text = format!("{}\n{}", v.title, v.line);
    let asks = v.asks.filter(|_| buttons(i));
    match i.kind.as_str() {
        "slack" => {
            let mut blocks = vec![json!({ "type": "section", "text": { "type": "mrkdwn", "text": format!("*{}*\n{}", mrkdwn(&v.title), mrkdwn(&v.line)) } })];
            if let Some(id) = asks {
                blocks.push(json!({ "type": "actions", "block_id": "kiln", "elements": [
                    { "type": "button", "action_id": "kiln_approve", "style": "primary", "value": id.to_string(), "text": { "type": "plain_text", "text": "Approve" } },
                    { "type": "button", "action_id": "kiln_reject", "style": "danger", "value": id.to_string(), "text": { "type": "plain_text", "text": "Reject" } },
                ] }));
            }
            json!({ "text": text, "blocks": blocks })
        }
        "discord" => json!({ "content": format!("**{}**\n{}", v.title, v.line), "allowed_mentions": { "parse": [] } }),
        "telegram" => {
            let keyboard = asks.map_or_else(Vec::new, |id| vec![vec![json!({ "text": "Approve", "callback_data": format!("kiln:approve:{id}") }), json!({ "text": "Reject", "callback_data": format!("kiln:reject:{id}") })]]);
            json!({ "text": text, "reply_markup": { "inline_keyboard": keyboard } })
        }
        _ => json!({ "text": text }),
    }
}

/// What a message says, for knowing when it needs an edit.
pub fn shown(i: &Integration, v: &View) -> String {
    crate::util::sha256_hex(body(i, v).to_string().as_bytes())
}

/// A Discord webhook's address with `tail` on its path and `query` beside what it has (a thread's).
fn discord_url(webhook: &str, tail: &str, query: &str) -> String {
    let (path, had) = webhook.split_once('?').map_or((webhook, ""), |(p, q)| (p, q));
    let q: Vec<&str> = [had, query].into_iter().filter(|q| !q.is_empty()).collect();
    let path = path.trim_end_matches('/');
    if q.is_empty() { format!("{path}{tail}") } else { format!("{path}{tail}?{}", q.join("&")) }
}

/// Telegram's Bot API for `i`: its `api`, else Telegram's own.
pub(crate) fn telegram_api(i: &Integration) -> String {
    let own = i.config.get("api").and_then(Value::as_str).map(str::trim).filter(|a| !a.is_empty()).map(str::to_owned);
    own.unwrap_or_else(|| integrations::endpoint("telegram:api", || "https://api.telegram.org".into())).trim_end_matches('/').to_owned()
}

/// Slack's answer, when it said `ok: false`.
fn slack_ok(v: Value) -> Result<Value, String> {
    if v.get("ok") == Some(&Value::Bool(true)) { Ok(v) } else { Err(format!("Slack: {}", v.get("error").and_then(Value::as_str).unwrap_or("refused"))) }
}

/// Telegram's answer, when it said `ok: false`.
fn telegram_ok(v: Value) -> Result<Value, String> {
    if v.get("ok") == Some(&Value::Bool(true)) { Ok(v) } else { Err(format!("Telegram: {}", v.get("description").and_then(Value::as_str).unwrap_or("refused"))) }
}

/// Posts `v` through `i` (`secret` its stored one) to `target`, or the channel it posts to.
pub async fn post(hub: &Hub, i: &Integration, secret: &str, target: Option<&str>, v: &View) -> Result<Posted, String> {
    let cfg = |k: &str| i.config.get(k).and_then(Value::as_str).map(str::to_owned).filter(|v| !v.is_empty());
    let mut b = body(i, v);
    match i.kind.as_str() {
        "slack" => {
            let channel = target.map(str::to_owned).or_else(|| cfg("channel")).ok_or("a Slack bot needs a channel: `slack:#deploys`")?;
            if let Some(m) = b.as_object_mut() {
                m.insert("channel".into(), json!(channel));
            }
            let url = integrations::endpoint("slack:api", || format!("{}/chat.postMessage", super::super::replies::slack_api(i)));
            let a = slack_ok(super::call(hub, Method::Post, &url, Some(secret), &b, false).await?)?;
            let (channel, ts) = (a.get("channel").and_then(Value::as_str), a.get("ts").and_then(Value::as_str));
            match (channel, ts) {
                (Some(c), Some(t)) => Ok(Posted { channel: c.to_owned(), message: t.to_owned() }),
                _ => Err("Slack didn't say where its message is".into()),
            }
        }
        "discord" => {
            let a = super::call(hub, Method::Post, &discord_url(secret, "", "wait=true"), None, &b, true).await?;
            let id = a.get("id").and_then(Value::as_str).ok_or("Discord didn't say which message is kiln's")?;
            Ok(Posted { channel: a.get("channel_id").and_then(Value::as_str).unwrap_or_default().to_owned(), message: id.to_owned() })
        }
        "telegram" => {
            let chat = target.map(str::to_owned).or_else(|| cfg("chat_id")).ok_or("Telegram needs a chat: `telegram:<chat id>`")?;
            if let Some(m) = b.as_object_mut() {
                m.insert("chat_id".into(), json!(chat));
            }
            let a = telegram_ok(super::call(hub, Method::Post, &format!("{}/bot{secret}/sendMessage", telegram_api(i)), None, &b, false).await?)?;
            let id = a.pointer("/result/message_id").and_then(Value::as_i64).ok_or("Telegram didn't say which message is kiln's")?;
            let chat = a.pointer("/result/chat/id").map_or(chat, |c| c.as_i64().map_or_else(|| c.as_str().unwrap_or_default().to_owned(), |n| n.to_string()));
            Ok(Posted { channel: chat, message: id.to_string() })
        }
        other => Err(format!("a {other} integration can't change a message it posts")),
    }
}

/// Edits the message at `at` to say `v`.
pub async fn update(hub: &Hub, i: &Integration, secret: &str, at: &Posted, v: &View) -> Result<(), String> {
    let mut b = body(i, v);
    match i.kind.as_str() {
        "slack" => {
            if let Some(m) = b.as_object_mut() {
                m.insert("channel".into(), json!(at.channel));
                m.insert("ts".into(), json!(at.message));
            }
            let url = integrations::endpoint("slack:update", || format!("{}/chat.update", super::super::replies::slack_api(i)));
            slack_ok(super::call(hub, Method::Post, &url, Some(secret), &b, false).await?).map(|_| ())
        }
        "discord" => super::call(hub, Method::Patch, &discord_url(secret, &format!("/messages/{}", at.message), ""), None, &b, true).await.map(|_| ()),
        "telegram" => {
            if let Some(m) = b.as_object_mut() {
                m.insert("chat_id".into(), json!(at.channel));
                m.insert("message_id".into(), json!(at.message.parse::<i64>().unwrap_or_default()));
            }
            telegram_ok(super::call(hub, Method::Post, &format!("{}/bot{secret}/editMessageText", telegram_api(i)), None, &b, false).await?).map(|_| ())
        }
        other => Err(format!("a {other} integration can't change a message it posts")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn integration(kind: &str, config: Value, signing: bool) -> Integration {
        Integration {
            id: "i1".into(),
            kind: kind.into(),
            name: "chat".into(),
            config,
            has_secret: true,
            has_signing_secret: signing,
            scope: "server".into(),
            created_by: "admin".into(),
            created_at: 0,
            updated_at: 0,
            verified_at: None,
            last_error: None,
            last_used_at: None,
        }
    }

    #[test]
    fn a_message_kiln_can_change_and_the_buttons_it_carries() {
        let v = View { title: "⏸ web 1.4.2 → production waits for approval".into(), line: "release 42, asked by ada · approvers: grace · <ok> & done".into(), asks: Some(42) };
        let slack = integration("slack", json!({}), true);
        assert!(edits(&slack, Some("xoxb-1")) && !edits(&slack, Some("https://hooks.slack.com/services/x")), "a bot edits; a webhook can't");
        let b = body(&slack, &v);
        assert_eq!(b.pointer("/blocks/0/text/text").and_then(Value::as_str), Some("*⏸ web 1.4.2 → production waits for approval*\nrelease 42, asked by ada · approvers: grace · &lt;ok&gt; &amp; done"));
        assert_eq!(b.pointer("/blocks/1/elements/0/action_id").and_then(Value::as_str), Some("kiln_approve"));
        assert_eq!(b.pointer("/blocks/1/elements/1/value").and_then(Value::as_str), Some("42"));
        // Without its signing secret kiln can't take Slack's buttons: none.
        assert!(body(&integration("slack", json!({}), false), &v).pointer("/blocks/1").is_none());
        let landed = View { title: "✓ landed".into(), asks: None, ..v.clone() };
        assert_ne!(shown(&slack, &v), shown(&slack, &landed), "an edit when it says something else");
        let telegram = integration("telegram", json!({ "commands": "on" }), false);
        assert_eq!(body(&telegram, &v).pointer("/reply_markup/inline_keyboard/0/1/callback_data").and_then(Value::as_str), Some("kiln:reject:42"));
        assert_eq!(body(&telegram, &landed).pointer("/reply_markup/inline_keyboard"), Some(&json!([])), "its buttons go once it's answered");
        assert!(body(&integration("telegram", json!({}), false), &v).pointer("/reply_markup/inline_keyboard/0").is_none(), "no webhook of kiln's, no buttons");
        assert!(!edits(&integration("teams", json!({}), false), Some("https://x")), "Teams' Workflows can't");
        assert_eq!(discord_url("https://discord.com/api/webhooks/1/t", "", "wait=true"), "https://discord.com/api/webhooks/1/t?wait=true");
        assert_eq!(discord_url("https://discord.com/api/webhooks/1/t?thread_id=9", "/messages/5", ""), "https://discord.com/api/webhooks/1/t/messages/5?thread_id=9");
    }
}

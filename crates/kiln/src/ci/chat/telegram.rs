//! Telegram (docs/deploy-api.md §12.3): the bot's updates, at the integration's address, which
//! kiln makes the bot's webhook (`setWebhook`, with the address's key as its secret token: every
//! update carries it back in `X-Telegram-Bot-Api-Secret-Token`). A command in a chat the bot is in
//! (`/approve 42`, `/kiln status web`) is answered there, replying to it; a button on a release's
//! message (Approve, Reject) is its command, answered to its person alone. Whose account it is:
//! the one its person linked (`/link <code>`). Telegram's bots never see other bots.

use std::sync::Arc;

use http::StatusCode;
use serde_json::{Value, json};

use super::super::Fail;
use super::super::integrations::{self, Integration};
use super::Speaker;
use crate::http::client::Method;
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;

/// Points `i`'s bot's webhook at `address` (its key the secret token Telegram sends back), and
/// marks the integration as taking its commands (`commands: on`), so its messages carry buttons.
pub async fn set_webhook(hub: &Hub, i: &Integration, address: &str) -> Result<(), String> {
    let token = integrations::token(hub, i).await.map_err(|e| format!("{e:#}"))?.ok_or("the integration has no bot token")?;
    let secret = address.rsplit('/').next().unwrap_or_default();
    let body = json!({ "url": address, "secret_token": secret, "allowed_updates": ["message", "callback_query"] });
    let a = super::call(hub, Method::Post, &format!("{}/bot{token}/setWebhook", super::posts::telegram_api(i)), None, &body, false).await?;
    if a.get("ok") != Some(&Value::Bool(true)) {
        return Err(format!("Telegram: {}", a.get("description").and_then(Value::as_str).unwrap_or("refused")));
    }
    let mut config = i.config.clone();
    if let Some(m) = config.as_object_mut() {
        m.insert("commands".into(), json!("on"));
    }
    integrations::update(hub, &i.name, Some(config), None, None, "admin").await.map(|_| ())
}

/// Calls the bot's `method` with `body`.
async fn bot(hub: &Hub, i: &Integration, method: &str, body: &Value) -> Result<(), String> {
    let token = integrations::token(hub, i).await.map_err(|e| format!("{e:#}"))?.ok_or("the integration has no bot token")?;
    let a = super::call(hub, Method::Post, &format!("{}/bot{token}/{method}", super::posts::telegram_api(i)), None, body, false).await?;
    if a.get("ok") == Some(&Value::Bool(true)) { Ok(()) } else { Err(format!("Telegram: {}", a.get("description").and_then(Value::as_str).unwrap_or("refused"))) }
}

/// Who an update is from.
fn speaker(from: &Value) -> Speaker {
    let get = |k: &str| from.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    let id = from.get("id").map(|v| v.as_i64().map_or_else(|| v.as_str().unwrap_or_default().to_owned(), |n| n.to_string())).unwrap_or_default();
    Speaker { id, name: Some(get("username")).filter(|u| !u.is_empty()).unwrap_or_else(|| get("first_name")), bot: from.get("is_bot").and_then(Value::as_bool).unwrap_or(false) }
}

/// An update at the Telegram integration `i`'s address, whose key `key` is.
pub async fn inbound(hub: &Arc<Hub>, i: Integration, key: String, req: Request) -> Result<Response, Fail> {
    let (headers, body) = super::body(req).await?;
    let token = super::header(&headers, "x-telegram-bot-api-secret-token").unwrap_or_default();
    if !crate::util::ct_eq(token.as_bytes(), key.as_bytes()) {
        return Err((StatusCode::UNAUTHORIZED, "not the bot's webhook: its secret token is missing".into()));
    }
    let v: Value = serde_json::from_slice(&body).map_err(|e| (StatusCode::BAD_REQUEST, format!("{e}")))?;
    let update = v.get("update_id").and_then(Value::as_i64).map(|u| u.to_string());
    if let Some(q) = v.get("callback_query").cloned() {
        // A button: `kiln:approve:42`.
        let Some(text) = q.get("data").and_then(Value::as_str).and_then(|d| d.strip_prefix("kiln:")).map(|d| d.replace(':', " ")) else { return Ok(srv::status(StatusCode::OK)) };
        let who = speaker(q.get("from").unwrap_or(&Value::Null));
        let id = q.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
        let h = Arc::clone(hub);
        crate::rt::spawn(async move {
            let said = super::run(&h, &i, &who, &text, None).await;
            // At most 200 characters, shown to its person alone.
            let short: String = said.chars().take(200).collect();
            if let Err(e) = bot(&h, &i, "answerCallbackQuery", &json!({ "callback_query_id": id, "text": short, "show_alert": true })).await {
                tracing::info!(integration = %i.name, error = %e, "a Telegram button's answer didn't go");
            }
        });
        return Ok(srv::status(StatusCode::OK));
    }
    let Some(m) = v.get("message").cloned() else { return Ok(srv::status(StatusCode::OK)) };
    let text = m.get("text").and_then(Value::as_str).unwrap_or_default().trim().to_owned();
    // Only commands: what else is said in a chat isn't kiln's.
    if !text.starts_with('/') {
        return Ok(srv::status(StatusCode::OK));
    }
    let who = speaker(m.get("from").unwrap_or(&Value::Null));
    let chat = m.pointer("/chat/id").cloned().unwrap_or(Value::Null);
    let to = m.get("message_id").cloned().unwrap_or(Value::Null);
    let h = Arc::clone(hub);
    crate::rt::spawn(async move {
        let said = super::run(&h, &i, &who, &text, update.as_deref()).await;
        if let Err(e) = bot(&h, &i, "sendMessage", &json!({ "chat_id": chat, "text": said, "reply_to_message_id": to, "disable_web_page_preview": true })).await {
            tracing::info!(integration = %i.name, error = %e, "a Telegram command's answer didn't go");
        }
    });
    Ok(srv::status(StatusCode::OK))
}

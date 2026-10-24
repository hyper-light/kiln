//! Discord (docs/deploy-api.md §12.3): the application's interactions, at the integration's
//! address as its Interactions Endpoint URL, each checked with the application's public key
//! (`X-Signature-Ed25519` over `X-Signature-Timestamp` and the body; the integration's
//! `public_key`). `/kiln <command>` is answered to its person alone (`flags: 64`): at once when
//! it's quick, otherwise deferred and then written into that answer (`webhooks/<application>/
//! <token>/messages/@original`). Whose account it is: the one its person linked (`link <code>`).

use std::sync::Arc;

use http::StatusCode;
use serde_json::{Value, json};

use super::super::Fail;
use super::super::integrations::Integration;
use super::Speaker;
use crate::http::client::Method;
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;

/// Only its person sees kiln's answer.
const EPHEMERAL: u64 = 64;

/// The command, as Discord registers it (`PUT /applications/<id>/commands`).
pub fn command() -> Value {
    json!([{ "name": "kiln", "type": 1, "description": "Deploy, approve, reject, roll back, or ask what runs", "options": [{ "name": "command", "type": 3, "required": true, "description": "deploy web 1.4.2 to staging · approve 42 · reject 42 why · rollback web in production · status web · link <code>" }] }])
}

/// Whether `signature` (hex) is the application's, whose public key is `key` (hex), for `body` at
/// `timestamp`.
pub fn signed(key: &str, timestamp: &str, body: &[u8], signature: &str) -> bool {
    use aws_lc_rs::signature::{ED25519, UnparsedPublicKey};
    let (Ok(key), Ok(sig)) = (hex::decode(key.trim()), hex::decode(signature.trim())) else { return false };
    let mut msg = timestamp.as_bytes().to_vec();
    msg.extend_from_slice(body);
    UnparsedPublicKey::new(&ED25519, key).verify(&msg, &sig).is_ok()
}

/// What a command interaction says: its options, subcommands as words (`/kiln approve release:42`
/// is `approve 42`), or one option's text (`/kiln command:approve 42`).
fn said(options: &[Value]) -> Vec<String> {
    let mut words = Vec::new();
    for o in options {
        match o.get("type").and_then(Value::as_u64) {
            Some(1 | 2) => {
                words.push(o.get("name").and_then(Value::as_str).unwrap_or_default().to_owned());
                words.extend(said(o.get("options").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default()));
            }
            _ => match o.get("value") {
                Some(Value::String(s)) => words.push(s.clone()),
                Some(other) => words.push(other.to_string()),
                None => {}
            },
        }
    }
    words
}

/// Discord's API for `i`: its `api`, else Discord's own.
fn api(i: &Integration) -> String {
    let own = i.config.get("api").and_then(Value::as_str).map(str::trim).filter(|a| !a.is_empty()).map(str::to_owned);
    own.unwrap_or_else(|| super::super::integrations::endpoint("discord:api", || "https://discord.com/api/v10".into())).trim_end_matches('/').to_owned()
}

/// An interaction at the Discord integration `i`'s address.
pub async fn inbound(hub: &Arc<Hub>, i: Integration, req: Request) -> Result<Response, Fail> {
    let (headers, body) = super::body(req).await?;
    let key = i.config.get("public_key").and_then(Value::as_str).unwrap_or_default().to_owned();
    if key.trim().is_empty() {
        return Err((StatusCode::UNAUTHORIZED, "the integration has no `public_key` to check Discord's interactions with".into()));
    }
    let (ts, sig) = (super::header(&headers, "x-signature-timestamp").unwrap_or_default(), super::header(&headers, "x-signature-ed25519").unwrap_or_default());
    if !signed(&key, ts, &body, sig) {
        return Err((StatusCode::UNAUTHORIZED, "invalid request signature".into()));
    }
    let v: Value = serde_json::from_slice(&body).map_err(|e| (StatusCode::BAD_REQUEST, format!("{e}")))?;
    let user = v.pointer("/member/user").or_else(|| v.get("user")).cloned().unwrap_or(Value::Null);
    let get = |p: &str| user.get(p).and_then(Value::as_str).unwrap_or_default().to_owned();
    let who = Speaker { id: get("id"), name: Some(get("global_name")).filter(|n| !n.is_empty()).unwrap_or_else(|| get("username")), bot: user.get("bot").and_then(Value::as_bool).unwrap_or(false) };
    let text = match v.get("type").and_then(Value::as_u64) {
        // Discord's check that kiln answers.
        Some(1) => return Ok(srv::json(StatusCode::OK, &json!({ "type": 1 }))),
        Some(2) => said(v.pointer("/data/options").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default()).join(" "),
        // A button: `kiln:approve:42`.
        Some(3) => v.pointer("/data/custom_id").and_then(Value::as_str).and_then(|c| c.strip_prefix("kiln:")).unwrap_or_default().replace(':', " "),
        _ => return Err((StatusCode::BAD_REQUEST, "not an interaction kiln takes".into())),
    };
    let (application, token, key) = (v.get("application_id").and_then(Value::as_str).unwrap_or_default().to_owned(), v.get("token").and_then(Value::as_str).unwrap_or_default().to_owned(), v.get("id").and_then(Value::as_str).map(str::to_owned));
    let (h, i2) = (Arc::clone(hub), i.clone());
    let work = async move { super::run(&h, &i2, &who, &text, key.as_deref()).await };
    let h = Arc::clone(hub);
    let later = move |said: String| async move {
        let url = format!("{}/webhooks/{application}/{token}/messages/@original", api(&i));
        if let Err(e) = super::call(&h, Method::Patch, &url, None, &json!({ "content": said }), false).await {
            tracing::info!(integration = %i.name, error = %e, "a Discord command's answer didn't go");
        }
    };
    Ok(srv::json(
        StatusCode::OK,
        &match super::soon(work, later).await {
            Some(said) => json!({ "type": 4, "data": { "content": said, "flags": EPHEMERAL, "allowed_mentions": { "parse": [] } } }),
            None => json!({ "type": 5, "data": { "flags": EPHEMERAL } }),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair as _};

    #[test]
    fn an_interaction_is_the_applications_and_says_its_command() -> anyhow::Result<()> {
        let pair = Ed25519KeyPair::generate().map_err(|_| anyhow::anyhow!("a key"))?;
        let public = hex::encode(pair.public_key().as_ref());
        let body = br#"{"type":2,"data":{"name":"kiln","options":[{"name":"command","type":3,"value":"approve 42"}]}}"#;
        let mut msg = b"1792500000".to_vec();
        msg.extend_from_slice(body);
        let sig = hex::encode(pair.sign(&msg).as_ref());
        assert!(signed(&public, "1792500000", body, &sig));
        assert!(!signed(&public, "1792500001", body, &sig), "another time");
        assert!(!signed(&public, "1792500000", b"{}", &sig), "another body");
        assert!(!signed("zz", "1792500000", body, &sig), "no key");
        let v: Value = serde_json::from_slice(body)?;
        assert_eq!(said(v.pointer("/data/options").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default()).join(" "), "approve 42");
        let sub = json!([{ "name": "approve", "type": 1, "options": [{ "name": "release", "type": 4, "value": 42 }, { "name": "note", "type": 3, "value": "ship it" }] }]);
        assert_eq!(said(sub.as_array().map(Vec::as_slice).unwrap_or_default()).join(" "), "approve 42 ship it");
        Ok(())
    }
}

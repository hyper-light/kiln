//! Microsoft Teams (docs/deploy-api.md §12.3): an outgoing webhook's messages, at the
//! integration's address as its callback URL. Each is checked with the webhook's security token
//! (the integration's signing secret): `Authorization: HMAC <base64 HMAC-SHA256 of the body>`,
//! keyed with the token's bytes. `@kiln approve 42` is answered in the same request, as Teams'
//! outgoing webhooks are, so one that takes longer is told to look at the release's page. Whose
//! account it is: the one its person linked (`@kiln link <code>`). Teams' Workflows post kiln's
//! messages but can't change one, so a release there is a message per event.

use std::sync::Arc;

use base64::Engine as _;
use http::StatusCode;
use serde_json::{Value, json};

use super::super::Fail;
use super::super::integrations::{self, Integration};
use super::Speaker;
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;

/// Whether `authorization` is Teams' for `body`, with `token` the webhook's security token.
pub fn signed(token: &str, body: &[u8], authorization: &str) -> bool {
    let b64 = base64::engine::general_purpose::STANDARD;
    let (Some(given), Ok(key)) = (authorization.trim().strip_prefix("HMAC "), b64.decode(token.trim())) else { return false };
    let Ok(mac) = hex::decode(crate::util::hmac_hex(&key, body)) else { return false };
    crate::util::ct_eq(b64.encode(mac).as_bytes(), given.trim().as_bytes())
}

/// What a message says without its mentions and markup: `<at>kiln</at> approve&nbsp;42`.
fn plain(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("<at>") {
        out.push_str(rest.get(..at).unwrap_or_default());
        rest = rest.get(at..).and_then(|r| r.find("</at>").map(|end| r.get(end + 5..).unwrap_or_default())).unwrap_or_default();
    }
    out.push_str(rest);
    // Whatever other tags Teams wraps it in.
    let mut bare = String::new();
    let mut inside = false;
    for c in out.chars() {
        match c {
            '<' => inside = true,
            '>' if inside => inside = false,
            c if !inside => bare.push(c),
            _ => {}
        }
    }
    bare.replace("&nbsp;", " ").replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A message at the Teams integration `i`'s address.
pub async fn inbound(hub: &Arc<Hub>, i: Integration, req: Request) -> Result<Response, Fail> {
    let (headers, body) = super::body(req).await?;
    let token = integrations::signing_secret(hub, &i).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let Some(token) = token else { return Err((StatusCode::UNAUTHORIZED, "the integration has no outgoing webhook's security token (`signing_secret`) to check Teams' messages with".into())) };
    if !signed(&token, &body, super::header(&headers, "authorization").unwrap_or_default()) {
        return Err((StatusCode::UNAUTHORIZED, "not signed with the outgoing webhook's security token".into()));
    }
    let v: Value = serde_json::from_slice(&body).map_err(|e| (StatusCode::BAD_REQUEST, format!("{e}")))?;
    let get = |p: &str| v.pointer(p).and_then(Value::as_str).unwrap_or_default().to_owned();
    let who = Speaker { id: Some(get("/from/aadObjectId")).filter(|a| !a.is_empty()).unwrap_or_else(|| get("/from/id")), name: get("/from/name"), bot: false };
    let (text, key) = (plain(&get("/text")), Some(get("/id")).filter(|k| !k.is_empty()));
    let h = Arc::clone(hub);
    let work = async move { super::run(&h, &i, &who, &text, key.as_deref()).await };
    let said = super::soon(work, |said| async move { tracing::info!(said = %said, "a Teams command finished after its answer") }).await;
    Ok(srv::json(StatusCode::OK, &json!({ "type": "message", "text": said.unwrap_or_else(|| "On it: kiln's release page says how it goes.".into()) })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_teams_message_is_its_webhooks_and_says_its_command() {
        let token = base64::engine::general_purpose::STANDARD.encode(b"a security token of thirty-two b");
        let body = br#"{"type":"message","text":"<at>kiln</at> approve 42"}"#;
        let key = base64::engine::general_purpose::STANDARD.decode(&token).unwrap_or_default();
        let mac = hex::decode(crate::util::hmac_hex(&key, body)).unwrap_or_default();
        let auth = format!("HMAC {}", base64::engine::general_purpose::STANDARD.encode(mac));
        assert!(signed(&token, body, &auth));
        assert!(!signed(&token, b"{}", &auth), "another body");
        assert!(!signed(&token, body, "Bearer x"), "not Teams' scheme");
        assert_eq!(plain("<at>kiln</at> approve&nbsp;42"), "approve 42");
        assert_eq!(plain("<p><at>kiln</at>&nbsp;status web</p>"), "status web");
    }
}

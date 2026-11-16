//! A captured sign-in refreshed at its provider (Shards' D242, step 8's third slice; kiln plan
//! 01a1d2b88be57304b972761e2a5c6781 step 8). The broker alone calls a sign-in's provider, as its
//! CLI's own refresh would ([`Provider::upstream`]), and the CLI in the microVM is answered a token
//! response whose tokens are placeholders ([`answer`]).
//!
//! A sign-in's name says its provider: `claude/…` for Claude Code's, `codex/…` for Codex's, kiln's
//! harness kinds (`crate::accounts`). A name that names no provider is captured by no run, so
//! nothing is kept that couldn't be refreshed.
//!
//! What each CLI's refresh says is its pinned build's, as Shards read it (its
//! `docs/research/s33-veil-creds/STEP8.md` and `evidence/`): Claude Code 2.1.311 posts JSON
//! `grant_type`, `refresh_token`, `client_id` and `scope` (offset 242103577), and reads
//! `access_token`, `refresh_token` (keeping its own where none comes), `expires_in` and `scope`;
//! Codex rust-v0.167.2 posts JSON `grant_type`, `client_id` and `refresh_token`
//! (login/src/oauth/client.rs:97-113), and keeps each of `id_token`, `access_token` and
//! `refresh_token` only where one comes (auth/manager.rs:1758-1766, 1855-1860).

use futures::future::BoxFuture;
use kiln_seams::secrets::SecretValue;

use super::broker::{Captured, Tokens, Unrefreshed, Upstream};

/// A provider whose sign-ins the broker refreshes: the CLI that signed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    ClaudeCode,
    Codex,
}

/// Claude Code 2.1.311's OAuth client: its production config's `CLIENT_ID` (offset 238938203).
const CLAUDE_CODE_CLIENT: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
/// Codex rust-v0.167.2's OAuth client: `CLIENT_ID` (login/src/auth/manager.rs:1863).
const CODEX_CLIENT: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// The most of a token response read: the window the run's injector reads the same provider's
/// token responses within (Shards' `tcp::TO_GUEST`, D242 step 8's question 3).
const RESPONSE_MAX: usize = 4 << 20;

impl Provider {
    /// The provider sign-in `name` names, where it names one.
    pub fn of(name: &str) -> Option<Provider> {
        match name.split_once('/')?.0 {
            "claude" => Some(Provider::ClaudeCode),
            "codex" => Some(Provider::Codex),
            _ => None,
        }
    }

    /// Its token endpoint (RFC 6749 §3.2): the service kiln names it by, its origin and its path.
    fn token(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Provider::ClaudeCode => ("claude:token", "https://platform.claude.com", "/v1/oauth/token"),
            Provider::Codex => ("codex:token", "https://auth.openai.com", "/oauth/token"),
        }
    }

    /// Its refresh's body, as its CLI's own makes it: Claude Code asks the scope its sign-in holds
    /// again; where none is held, none is asked, which asks the scope first granted (RFC 6749 §6).
    fn body(self, refresh: &str, scope: Option<&str>) -> Result<Vec<u8>, Unrefreshed> {
        let mut body = match self {
            Provider::ClaudeCode => serde_json::json!({ "grant_type": "refresh_token", "refresh_token": refresh, "client_id": CLAUDE_CODE_CLIENT }),
            Provider::Codex => serde_json::json!({ "client_id": CODEX_CLIENT, "grant_type": "refresh_token", "refresh_token": refresh }),
        };
        if let (Provider::ClaudeCode, Some(scope), Some(fields)) = (self, scope, body.as_object_mut()) {
            fields.insert("scope".into(), scope.into());
        }
        serde_json::to_vec(&body).map_err(|e| Unrefreshed::Failed(format!("its request: {e}")))
    }

    /// Its refresh, as the broker asks it ([`super::broker::Broker::refresh`]).
    pub fn upstream(self) -> Upstream {
        Box::new(move |refresh, scope| -> BoxFuture<'static, Result<Tokens, Unrefreshed>> { Box::pin(self.refresh(refresh, scope)) })
    }

    async fn refresh(self, refresh: SecretValue, scope: Option<String>) -> Result<Tokens, Unrefreshed> {
        let (service, origin, path) = self.token();
        let url = format!("{}{path}", crate::ci::integrations::endpoint(service, || origin.to_owned()));
        let old = std::str::from_utf8(refresh.expose()).map_err(|_| Unrefreshed::Failed("its refresh token isn't UTF-8".into()))?;
        let body = self.body(old, scope.as_deref())?;
        let client = crate::http::client::github().map_err(|e| Unrefreshed::Failed(format!("{e:#}")))?;
        let response = client.post(&url).header("content-type", "application/json").body(body).send().await.map_err(|e| Unrefreshed::Failed(format!("{e:#}")))?;
        let status = response.status();
        let bytes = response.bytes(RESPONSE_MAX).await.map_err(|e| Unrefreshed::Failed(format!("{e:#}")))?;
        match status {
            200 => tokens(&bytes, old),
            // RFC 6749 §5.2: an error response is a 400, or a 401 for its client.
            400 | 401 => Err(Unrefreshed::Refused(said(status, &bytes, old))),
            _ => Err(Unrefreshed::Failed(said(status, &bytes, old))),
        }
    }
}

/// A token response's tokens and what the CLI reads beside them (RFC 6749 §5.1); its ID token
/// (Codex's) is left, so the CLI keeps the inert one its capture gave it. Refused: no access
/// token, and a scope that holds a token, which its answer would carry into the microVM.
fn tokens(body: &[u8], old: &str) -> Result<Tokens, Unrefreshed> {
    #[derive(serde::Deserialize)]
    struct Response {
        access_token: String,
        refresh_token: Option<String>,
        expires_in: Option<u64>,
        scope: Option<String>,
    }
    // serde's words are left out: they may quote the response, tokens and all.
    let r: Response = serde_json::from_slice(body).map_err(|e| Unrefreshed::Failed(format!("its token response isn't one (line {}, column {})", e.line(), e.column())))?;
    let held = |s: &str| [Some(r.access_token.as_str()), r.refresh_token.as_deref(), Some(old)].into_iter().flatten().any(|t| !t.is_empty() && s.contains(t));
    if r.scope.as_deref().is_some_and(held) {
        return Err(Unrefreshed::Failed("its token response's scope repeats a token".into()));
    }
    Ok(Tokens { access: SecretValue::new(r.access_token.into_bytes()), refresh: r.refresh_token.map(|t| SecretValue::new(t.into_bytes())), expires_in_s: r.expires_in, scope: r.scope })
}

/// What a provider said in a response that gave no tokens: RFC 6749 §5.2's `error` and
/// `error_description` where it gave them, else its status alone. Words repeating the refresh
/// token asked with are left out, as these reach the run's diagnostics and its guest.
fn said(status: u16, body: &[u8], old: &str) -> String {
    #[derive(serde::Deserialize)]
    struct Said {
        error: String,
        error_description: Option<String>,
    }
    let words = match serde_json::from_slice::<Said>(body) {
        Ok(Said { error, error_description: Some(d) }) => format!("{error}: {d}"),
        Ok(Said { error, .. }) => error,
        Err(_) => return format!("HTTP {status}"),
    };
    if !old.is_empty() && words.contains(old) {
        return format!("HTTP {status}, its words repeating the refresh token left out");
    }
    format!("HTTP {status}: {words}")
}

/// The token response a refresh's CLI is answered, its tokens the sign-in's placeholders (Shards'
/// network process writes its own in their places as it passes it on): an access token of
/// RFC 6750's `Bearer` type, as both providers' are (D242 step 8's question 7), its life from
/// `now_ms` where its provider gave it one, its refresh token where the sign-in holds one, and
/// its scope. Nothing else of its provider's response is in it.
pub fn answer(c: &Captured, now_ms: i64) -> Result<Vec<u8>, serde_json::Error> {
    let mut fields = serde_json::Map::new();
    fields.insert("access_token".into(), c.access.placeholder.clone().into());
    fields.insert("token_type".into(), "Bearer".into());
    if let Some(end) = c.expires_ms {
        fields.insert("expires_in".into(), (end.saturating_sub(now_ms).max(0) / 1000).into());
    }
    if let Some(refresh) = &c.refresh {
        fields.insert("refresh_token".into(), refresh.placeholder.clone().into());
    }
    if let Some(scope) = &c.scope {
        fields.insert("scope".into(), scope.clone().into());
    }
    serde_json::to_vec(&serde_json::Value::Object(fields))
}

#[cfg(test)]
mod tests {
    use super::super::broker::Placed;
    use super::*;

    /// Plan step 33's step 8 (D242): a sign-in's name says its provider, and one that names none
    /// names no provider.
    #[test]
    fn cp_t8_3_a_sign_ins_name_says_its_provider() {
        assert_eq!(Provider::of("claude/ada"), Some(Provider::ClaudeCode));
        assert_eq!(Provider::of("codex/ada@example.com"), Some(Provider::Codex));
        for name in ["claude", "codex", "devin/ada", "claudex/ada", "/ada", ""] {
            assert_eq!(Provider::of(name), None, "{name:?}");
        }
    }

    /// Plan step 33's step 8 (D242): each refresh's body is its CLI's own, the refresh token
    /// escaped as JSON holds it, Claude Code's scope asked where one is held.
    #[test]
    fn cp_t8_3_a_refresh_asks_what_its_cli_asks() -> anyhow::Result<()> {
        let read = |b: Vec<u8>| -> anyhow::Result<serde_json::Value> { Ok(serde_json::from_slice(&b)?) };
        let claude = read(Provider::ClaudeCode.body("rt\"1", Some("user:inference user:profile")).map_err(anyhow::Error::msg)?)?;
        assert_eq!(claude, serde_json::json!({ "grant_type": "refresh_token", "refresh_token": "rt\"1", "client_id": CLAUDE_CODE_CLIENT, "scope": "user:inference user:profile" }));
        let bare = read(Provider::ClaudeCode.body("rt-1", None).map_err(anyhow::Error::msg)?)?;
        assert_eq!(bare, serde_json::json!({ "grant_type": "refresh_token", "refresh_token": "rt-1", "client_id": CLAUDE_CODE_CLIENT }));
        let codex = read(Provider::Codex.body("rt-1", Some("openid")).map_err(anyhow::Error::msg)?)?;
        assert_eq!(codex, serde_json::json!({ "client_id": CODEX_CLIENT, "grant_type": "refresh_token", "refresh_token": "rt-1" }));
        Ok(())
    }

    /// Plan step 33's step 8 (D242): a token response's tokens are read and its ID token left;
    /// one with no access token, or whose scope repeats a token, gives none, and its words never
    /// quote it.
    #[test]
    fn cp_t8_3_a_token_response_gives_its_tokens_and_nothing_of_them_in_words() -> anyhow::Result<()> {
        let t = tokens(br#"{"id_token":"h.p.s","access_token":"at-2","refresh_token":"rt-2","expires_in":3600,"scope":"openid","token_type":"Bearer"}"#, "rt-1").map_err(anyhow::Error::msg)?;
        assert_eq!((t.access.expose(), t.refresh.as_ref().map(SecretValue::expose), t.expires_in_s, t.scope.as_deref()), (&b"at-2"[..], Some(&b"rt-2"[..]), Some(3600), Some("openid")));
        for (body, token) in [(&br#"{"refresh_token":"rt-secret-2"}"#[..], "rt-secret-2"), (br#"{"access_token":"at-2","scope":"x rt-secret-1 y"}"#, "rt-secret-1"), (br#"{"access_token":"at-secret-2","scope":"at-secret-2"}"#, "at-secret-2"), (br#"{"access_token":"at-2","expires_in":"at-secret-3"}"#, "at-secret-3")] {
            match tokens(body, "rt-secret-1") {
                Err(Unrefreshed::Failed(words)) => anyhow::ensure!(!words.contains(token), "{words}"),
                other => anyhow::bail!("{}: {:?}", String::from_utf8_lossy(body), other.map(|_| "tokens")),
            }
        }
        assert_eq!(said(400, br#"{"error":"invalid_grant","error_description":"expired"}"#, "rt-1"), "HTTP 400: invalid_grant: expired");
        assert_eq!(said(503, b"<html>down</html>", "rt-1"), "HTTP 503");
        assert!(!said(400, br#"{"error":"invalid_grant","error_description":"rt-secret-1 is revoked"}"#, "rt-secret-1").contains("rt-secret-1"));
        Ok(())
    }

    /// Plan step 33's step 8 (D242): a refresh's answer holds the sign-in's placeholders, never a
    /// token, and the life, refresh token and scope its CLI reads.
    #[test]
    fn cp_t8_3_a_refresh_is_answered_with_placeholders() -> anyhow::Result<()> {
        let placed = |p: &str, r: &str| Placed { placeholder: p.into(), reference: r.into() };
        let c = Captured { access: placed("kiln-placeholder-a", "sign-ins/claude/ada/access"), refresh: Some(placed("kiln-placeholder-r", "sign-ins/claude/ada/refresh")), expires_ms: Some(10_000_500), scope: Some("user:inference".into()) };
        let got: serde_json::Value = serde_json::from_slice(&answer(&c, 10_000)?)?;
        assert_eq!(got, serde_json::json!({ "access_token": "kiln-placeholder-a", "token_type": "Bearer", "expires_in": 9990, "refresh_token": "kiln-placeholder-r", "scope": "user:inference" }));
        let bare = Captured { refresh: None, expires_ms: Some(5), scope: None, ..c };
        let got: serde_json::Value = serde_json::from_slice(&answer(&bare, 10_000)?)?;
        assert_eq!(got, serde_json::json!({ "access_token": "kiln-placeholder-a", "token_type": "Bearer", "expires_in": 0 }));
        Ok(())
    }
}

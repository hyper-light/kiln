//! A captured sign-in refreshed at its provider (Shards' D242, step 8's third slice; kiln plan
//! 01a1d2b88be57304b972761e2a5c6781 step 8). The broker alone calls a sign-in's provider, as its
//! CLI's own refresh would ([`Provider::upstream`]), and the CLI in the microVM is answered a token
//! response whose tokens are placeholders ([`answer`]).
//!
//! A sign-in's name says its provider: `claude/…` for Claude Code's, `codex/…` for Codex's,
//! `goose/…` for goose's ChatGPT sign-in, kiln's harness kinds (`crate::accounts`). A name that
//! names no provider is captured by no run, so nothing is kept that couldn't be refreshed. Each
//! other harness's reason is its row in Shards' D242 (the sign-in profiles lane's survey).
//!
//! An isolated harness of any of them is granted its sign-in as it launches ([`SignIn`], made by
//! [`Provider::grant`] alone, so its name and its CLI come from one provider; Shards' D242, step 8;
//! Hex's C8): `<provider>/<person>`, the person it acts for by the folder kiln keeps their homes in
//! (`harness::isolated::home::person_dir`), so one sign-in of a CLI serves each person's runs, as
//! the sign-in outlives a run (D242, question 6). Its access token is filled in toward the hosts its
//! CLI sends it to ([`Provider::toward`]), with the placeholders an earlier capture gave its CLI,
//! as the file it keeps its sign-in in holds them ([`Provider::held`]).
//!
//! What each CLI's refresh says is its pinned build's, as Shards read it (its
//! `docs/research/s33-veil-creds/STEP8.md` and `evidence/`): Claude Code 2.1.311 posts JSON
//! `grant_type`, `refresh_token`, `client_id` and `scope` (offset 242103577), and reads
//! `access_token`, `refresh_token` (keeping its own where none comes), `expires_in` and `scope`;
//! Codex rust-v0.167.2 posts JSON `grant_type`, `client_id` and `refresh_token`
//! (login/src/oauth/client.rs:97-113), and keeps each of `id_token`, `access_token` and
//! `refresh_token` only where one comes (auth/manager.rs:1758-1766, 1855-1860). goose 1.59.0's
//! ChatGPT sign-in is Codex's OAuth client (`chatgpt_codex.rs`:38-39, commit 5d03ef6e): it posts
//! the same fields as a form (500-514) and reads `access_token`, `refresh_token`, and `id_token`
//! and `expires_in` where they come (461-466, 774-781); the broker refreshes it as Codex's, in
//! JSON, which that client's provider takes from Codex.

use futures::future::BoxFuture;
use kiln_seams::secrets::SecretValue;

use super::broker::{Captured, Tokens, Unrefreshed, Upstream};

/// A provider whose sign-ins the broker refreshes: the CLI that signed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    ClaudeCode,
    Codex,
    /// goose 1.59.0's ChatGPT sign-in (its `chatgpt_codex` provider), which signs in as Codex's OAuth
    /// client (`CLIENT_ID` `app_EMoamEEZ73f0CkXaXp7hrann` at `https://auth.openai.com`,
    /// crates/goose/src/providers/chatgpt_codex.rs:38-39 at commit 5d03ef6e): so its token endpoint,
    /// its refresh, the hosts its access token goes to and Shards' profile of it are Codex's; its
    /// name and its file are its own. It reads its account from its ID token's claims, unverified
    /// (412-422), which Shards' inert ID token keeps.
    Goose,
}

/// Claude Code 2.1.311's OAuth client: its production config's `CLIENT_ID` (offset 238938203).
const CLAUDE_CODE_CLIENT: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
/// Codex rust-v0.167.2's OAuth client: `CLIENT_ID` (login/src/auth/manager.rs:1863).
const CODEX_CLIENT: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// The most of a token response read: the window the run's injector reads the same provider's
/// token responses within (Shards' `tcp::TO_GUEST`, D242 step 8's question 3).
const RESPONSE_MAX: usize = 4 << 20;

/// Every provider, in the order kiln reads a sign-in's name against them.
const PROVIDERS: [Provider; 3] = [Provider::ClaudeCode, Provider::Codex, Provider::Goose];

/// A sign-in an isolated harness's launch grants it (Shards' D242, step 8; Hex's C8): its CLI's own
/// sign-in, its token response taken outside the microVM, its CLI given placeholders. Made by
/// [`Provider::grant`] alone. Its `Debug` says no placeholder (Hex's `Captured` says their lengths).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignIn {
    pub provider: Provider,
    /// Its name in kiln's store, which the broker keeps its tokens under and reads its provider from.
    pub name: String,
    /// The hosts its access token is filled in toward.
    pub toward: Vec<String>,
    /// What an earlier capture gave its CLI, which it holds still: so the placeholders stand for the
    /// sign-in's tokens again. None where its CLI holds none.
    pub captured: Option<hex_api::Captured>,
}

impl Provider {
    /// The provider sign-in `name` names, where it names one.
    pub fn of(name: &str) -> Option<Provider> {
        let word = name.split_once('/')?.0;
        PROVIDERS.into_iter().find(|p| p.word() == word)
    }

    /// The provider whose CLI kiln's harness `harness` runs (`crate::harness::kinds::Kind`'s id),
    /// where the broker answers its sign-in; none for every other harness, whose sign-in no Shards
    /// profile captures (each one's reason is its row in Shards' D242).
    pub fn of_harness(harness: &str) -> Option<Provider> {
        PROVIDERS.into_iter().find(|p| p.word() == harness)
    }

    /// What its sign-ins' names begin with: its harness kind's id.
    fn word(self) -> &'static str {
        match self {
            Provider::ClaudeCode => "claude",
            Provider::Codex => "codex",
            Provider::Goose => "goose",
        }
    }

    /// The CLI whose OAuth client it signs in as, as Hex grants it a sign-in: Shards' profile of its
    /// token endpoint is that client's (D242).
    pub fn cli(self) -> hex_api::SignInCli {
        match self {
            Provider::ClaudeCode => hex_api::SignInCli::ClaudeCode,
            Provider::Codex | Provider::Goose => hex_api::SignInCli::Codex,
        }
    }

    /// The name of `person`'s sign-in of its CLI (`owner`, `user:<id>`): its word, then the folder
    /// kiln keeps that person's homes in. None for a person kiln keeps no home for, or a name whose
    /// tokens' names would be past the store's.
    pub fn sign_in(self, person: &str) -> Option<String> {
        let name = format!("{}/{}", self.word(), crate::harness::isolated::home::person_dir(person).ok()?);
        kiln_seams::secrets::check_name(&super::broker::refresh_of(&name)).is_ok().then_some(name)
    }

    /// Its token endpoint (RFC 6749 §3.2): the service kiln names it by, its host and its path.
    fn token(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Provider::ClaudeCode => ("claude:token", "platform.claude.com", "/v1/oauth/token"),
            Provider::Codex | Provider::Goose => ("codex:token", "auth.openai.com", "/oauth/token"),
        }
    }

    /// Its token endpoint's host, where its CLI signs in and refreshes: a run's egress reaches it,
    /// or its CLI couldn't sign in through it.
    pub fn token_host(self) -> &'static str {
        self.token().1
    }

    /// The hosts its CLI sends its sign-in's access token to, which kiln's Agentfile for its harness
    /// grants: Claude Code's API (`Authorization: Bearer`, Shards' STEP8.md question 7, offset
    /// 242399004), and Codex's Responses through ChatGPT's backend (`https://chatgpt.com/backend-api/codex`,
    /// its record's "Hosts by name", docs/research/s20-codex-agentfile), as goose's is
    /// (`CODEX_API_ENDPOINT`, chatgpt_codex.rs:40). Never its token host, where Shards' injector
    /// fills nothing in.
    pub fn toward(self) -> &'static [&'static str] {
        match self {
            Provider::ClaudeCode => &["api.anthropic.com"],
            Provider::Codex | Provider::Goose => &["chatgpt.com"],
        }
    }

    /// The file its CLI keeps its sign-in in, in the folder [`Provider::file_in`] finds: Claude Code
    /// 2.1.311's `storagePath`, `.credentials.json` (Shards' evidence e_q2_creds.txt, offset
    /// 241605977), Codex rust-v0.167.2's `auth.json` (login/src/auth/storage.rs:168), and goose
    /// 1.59.0's `chatgpt_codex/tokens.json` in its configuration folder (chatgpt_codex.rs:331-333).
    pub fn file(self) -> &'static str {
        match self {
            Provider::ClaudeCode => ".credentials.json",
            Provider::Codex => "auth.json",
            Provider::Goose => "chatgpt_codex/tokens.json",
        }
    }

    /// Where its CLI's file is in its microVM, by `layout`'s run environment: in the folder Claude
    /// Code and Codex keep their configuration in (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`:
    /// `home::Layout::kept_in`), and in goose's configuration folder, `config` beneath the
    /// `GOOSE_PATH_ROOT` kiln's goose image sets (crates/goose/src/config/paths.rs:8-16, 41). None
    /// where its run environment doesn't say.
    pub fn file_in(self, layout: &crate::harness::isolated::home::Layout) -> Option<String> {
        let dir = match self {
            Provider::ClaudeCode | Provider::Codex => layout.kept_in()?,
            Provider::Goose => format!("{}/config", layout.dir_named("GOOSE_PATH_ROOT")?.trim_end_matches('/')),
        };
        Some(format!("{}/{}", dir.trim_end_matches('/'), self.file()))
    }

    /// The placeholders `text`, its CLI's file ([`Provider::file`]), holds for its access token and
    /// its refresh token where it holds one: Claude Code's `claudeAiOauth.accessToken` and
    /// `refreshToken` (offset 242352375), Codex's `tokens.access_token` and `refresh_token`
    /// (login/src/token_data.rs:20-22), goose's `access_token` and `refresh_token` at its top
    /// (`TokenData`, chatgpt_codex.rs:317-324), each of the broker's form. None where it holds no sign-in,
    /// one with a token of another form (one from before kiln captured its sign-ins, which its first
    /// refresh through Shards' injector gives placeholders), or one placeholder for both, which no
    /// capture gives and Hex refuses.
    pub fn held(self, text: &str) -> Option<(String, Option<String>)> {
        let (at, access, refresh) = match self {
            Provider::ClaudeCode => ("/claudeAiOauth", "accessToken", "refreshToken"),
            Provider::Codex => ("/tokens", "access_token", "refresh_token"),
            Provider::Goose => ("", "access_token", "refresh_token"),
        };
        let file: serde_json::Value = serde_json::from_str(text).ok()?;
        let tokens = file.pointer(at)?;
        let access = tokens.get(access)?.as_str().filter(|t| super::broker::is_placeholder(t))?.to_owned();
        let refresh = match tokens.get(refresh).and_then(serde_json::Value::as_str).filter(|t| !t.is_empty()) {
            None => None,
            Some(t) if super::broker::is_placeholder(t) && t != access => Some(t.to_owned()),
            Some(_) => return None,
        };
        Some((access, refresh))
    }

    /// The sign-in `person`'s run is granted where its `egress` (`host:port` each) reaches its
    /// token host and every host its access token goes to, which Hex and Shards hold a sign-in's
    /// hosts to: none where it doesn't, where its CLI couldn't sign in through it, or for a person
    /// with no sign-in name ([`Provider::sign_in`]). With what its CLI's file holds (`file`, its
    /// text, [`Provider::held`]), where it holds this provider's placeholders.
    pub fn grant(self, person: &str, egress: &[String], file: Option<&str>) -> Option<SignIn> {
        let reached = |host: &str| egress.iter().any(|e| e.rsplit_once(':').is_some_and(|(name, _)| name.eq_ignore_ascii_case(host)));
        if !reached(self.token_host()) || !self.toward().iter().all(|&h| reached(h)) {
            return None;
        }
        let name = self.sign_in(person)?;
        let captured = file.and_then(|text| self.held(text)).map(|(access_placeholder, refresh_placeholder)| hex_api::Captured { access: super::broker::access_of(&name), access_placeholder, refresh_placeholder });
        Some(SignIn { provider: self, name, toward: self.toward().iter().map(|h| (*h).to_owned()).collect(), captured })
    }

    /// Its refresh's body, as its OAuth client's own CLI makes it (goose's is Codex's): Claude Code
    /// asks the scope its sign-in holds again; where none is held, none is asked, which asks the
    /// scope first granted (RFC 6749 §6).
    fn body(self, refresh: &str, scope: Option<&str>) -> Result<Vec<u8>, Unrefreshed> {
        let mut body = match self {
            Provider::ClaudeCode => serde_json::json!({ "grant_type": "refresh_token", "refresh_token": refresh, "client_id": CLAUDE_CODE_CLIENT }),
            Provider::Codex | Provider::Goose => serde_json::json!({ "client_id": CODEX_CLIENT, "grant_type": "refresh_token", "refresh_token": refresh }),
        };
        if let (Provider::ClaudeCode, Some(scope), Some(fields)) = (self, scope, body.as_object_mut()) {
            fields.insert("scope".into(), scope.into());
        }
        serde_json::to_vec(&body).map_err(|e| Unrefreshed::Failed(format!("its request: {e}")))
    }

    /// Its refresh, as the broker asks it ([`super::broker::Broker::refresh`]).
    pub fn upstream(self) -> Upstream {
        Box::new(move |refresh: SecretValue, scope: Option<String>| -> BoxFuture<'static, Result<Tokens, Unrefreshed>> { Box::pin(self.refresh(refresh, scope)) })
    }

    async fn refresh(self, refresh: SecretValue, scope: Option<String>) -> Result<Tokens, Unrefreshed> {
        let (service, host, path) = self.token();
        let url = format!("{}{path}", crate::ci::integrations::endpoint(service, || format!("https://{host}")));
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
        let refused: [(&[u8], &str); 4] = [(br#"{"refresh_token":"rt-secret-2"}"#, "rt-secret-2"), (br#"{"access_token":"at-2","scope":"x rt-secret-1 y"}"#, "rt-secret-1"), (br#"{"access_token":"at-secret-2","scope":"at-secret-2"}"#, "at-secret-2"), (br#"{"access_token":"at-2","expires_in":"at-secret-3"}"#, "at-secret-3")];
        for (body, token) in refused {
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

    /// A placeholder of the broker's form, its random part `fill` repeated.
    fn placeholder(fill: char) -> String {
        format!("kiln-placeholder-{}", fill.to_string().repeat(64))
    }

    /// Plan step 33 (D242, step 8; Hex's C8): Claude Code's and Codex's harnesses sign in as their own
    /// provider, the sign-in named for its provider and its person's folder, which the broker reads
    /// its provider back from; no other harness, and no person kiln keeps no home for.
    #[test]
    fn s33_a_harnesses_sign_in_is_its_providers_named_for_its_person() {
        assert_eq!((Provider::of_harness("claude"), Provider::of_harness("codex")), (Some(Provider::ClaudeCode), Some(Provider::Codex)));
        for other in ["gemini", "opencode", "devin", "claudex", ""] {
            assert_eq!(Provider::of_harness(other), None, "{other:?}");
        }
        assert_eq!(Provider::ClaudeCode.sign_in("owner").as_deref(), Some("claude/owner"));
        assert_eq!(Provider::Codex.sign_in("user:ada").as_deref(), Some("codex/user-ada"));
        for p in PROVIDERS {
            let name = p.sign_in("user:ada");
            assert_eq!(name.as_deref().and_then(Provider::of), Some(p), "{name:?}");
            assert_eq!(p.sign_in("ada"), None, "a person kiln keeps no home for");
            assert_eq!(p.sign_in(&format!("user:{}", "a".repeat(512))), None, "a name past the store's");
        }
        assert_eq!((Provider::ClaudeCode.cli(), Provider::Codex.cli()), (hex_api::SignInCli::ClaudeCode, hex_api::SignInCli::Codex));
    }

    /// Plan step 33 (D242, step 8): a sign-in is granted only where its run's egress reaches its token
    /// host and every host its access token goes to, by name in any case; its access token goes to
    /// its provider's hosts alone.
    #[test]
    fn s33_a_sign_in_is_granted_where_its_egress_reaches_its_hosts() -> anyhow::Result<()> {
        let all: Vec<String> = ["API.anthropic.com:443", "claude.ai:443", "platform.claude.com:443"].map(String::from).to_vec();
        let granted = Provider::ClaudeCode.grant("owner", &all, None).ok_or_else(|| anyhow::anyhow!("no grant"))?;
        assert_eq!((granted.name.as_str(), granted.toward.clone(), granted.captured.clone()), ("claude/owner", vec!["api.anthropic.com".to_owned()], None));
        for gone in ["API.anthropic.com:443", "platform.claude.com:443"] {
            let short: Vec<String> = all.iter().filter(|h| *h != gone).cloned().collect();
            assert_eq!(Provider::ClaudeCode.grant("owner", &short, None), None, "without {gone}");
        }
        let codex: Vec<String> = ["api.openai.com:443", "auth.openai.com:443", "chatgpt.com:443"].map(String::from).to_vec();
        let granted = Provider::Codex.grant("user:ada", &codex, None).ok_or_else(|| anyhow::anyhow!("no codex grant"))?;
        assert_eq!((granted.name.as_str(), granted.toward), ("codex/user-ada", vec!["chatgpt.com".to_owned()]));
        assert_eq!(Provider::Codex.grant("user:ada", &all, None), None, "another provider's hosts");
        Ok(())
    }

    /// Plan step 33 (D242, step 8): the placeholders an earlier capture gave a CLI, as its file holds
    /// them, are granted again with its access token's reference; a file holding real tokens, one
    /// placeholder for both or no sign-in gives none.
    #[test]
    fn s33_a_clis_file_gives_its_earlier_captures_placeholders() -> anyhow::Result<()> {
        let (a, r) = (placeholder('a'), placeholder('b'));
        let claude = serde_json::json!({ "claudeAiOauth": { "accessToken": a, "refreshToken": r, "expiresAt": 1, "scopes": ["user:inference"] } }).to_string();
        let egress: Vec<String> = ["api.anthropic.com:443", "platform.claude.com:443"].map(String::from).to_vec();
        let granted = Provider::ClaudeCode.grant("owner", &egress, Some(&claude)).ok_or_else(|| anyhow::anyhow!("no grant"))?;
        assert_eq!(granted.captured, Some(hex_api::Captured { access: "sign-ins/claude/owner/access".into(), access_placeholder: a.clone(), refresh_placeholder: Some(r.clone()) }));
        let codex = serde_json::json!({ "OPENAI_API_KEY": null, "tokens": { "id_token": "h.p.", "access_token": a, "refresh_token": r, "account_id": "acct" } }).to_string();
        assert_eq!(Provider::Codex.held(&codex), Some((a.clone(), Some(r.clone()))));
        assert_eq!(Provider::ClaudeCode.held(&codex), None, "another CLI's file");
        let no_refresh = serde_json::json!({ "claudeAiOauth": { "accessToken": a, "refreshToken": "" } }).to_string();
        assert_eq!(Provider::ClaudeCode.held(&no_refresh), Some((a.clone(), None)));
        let unheld = [
            serde_json::json!({ "claudeAiOauth": { "accessToken": "sk-ant-oat01-real", "refreshToken": r } }).to_string(),
            serde_json::json!({ "claudeAiOauth": { "accessToken": a, "refreshToken": "sk-ant-ort01-real" } }).to_string(),
            serde_json::json!({ "claudeAiOauth": { "accessToken": a, "refreshToken": a } }).to_string(),
            serde_json::json!({ "claudeAiOauth": { "accessToken": format!("{a}0"), "refreshToken": r } }).to_string(),
            serde_json::json!({ "claudeAiOauth": { "accessToken": a.to_uppercase() } }).to_string(),
            serde_json::json!({}).to_string(),
            "not json".to_owned(),
        ];
        for text in unheld {
            assert_eq!(Provider::ClaudeCode.held(&text), None, "{text}");
        }
        Ok(())
    }

    /// Plan step 33 (Shards' D242, the sign-in profiles lane): goose's ChatGPT sign-in is Codex's OAuth
    /// client, so Hex is told Codex's CLI, and its token endpoint, its refresh and the hosts its access
    /// token goes to are Codex's; its name and the file its placeholders are read back from are its own.
    #[test]
    fn s33_gooses_chatgpt_sign_in_is_codexs_client_under_its_own_name() -> anyhow::Result<()> {
        assert_eq!((Provider::of_harness("goose"), Provider::of("goose/user-ada")), (Some(Provider::Goose), Some(Provider::Goose)));
        assert_eq!(Provider::Goose.sign_in("user:ada").as_deref(), Some("goose/user-ada"));
        assert_eq!(Provider::Goose.cli(), hex_api::SignInCli::Codex);
        assert_eq!((Provider::Goose.token(), Provider::Goose.toward()), (Provider::Codex.token(), Provider::Codex.toward()));
        let read = |b: Vec<u8>| -> anyhow::Result<serde_json::Value> { Ok(serde_json::from_slice(&b)?) };
        assert_eq!(read(Provider::Goose.body("rt-1", Some("openid")).map_err(anyhow::Error::msg)?)?, read(Provider::Codex.body("rt-1", Some("openid")).map_err(anyhow::Error::msg)?)?);
        let (a, r) = (placeholder('a'), placeholder('b'));
        let file = serde_json::json!({ "access_token": a, "refresh_token": r, "id_token": "h.p.shards-placeholder", "expires_at": "2026-11-16T00:00:00Z", "account_id": "acct" }).to_string();
        let egress: Vec<String> = ["auth.openai.com:443", "chatgpt.com:443"].map(String::from).to_vec();
        let granted = Provider::Goose.grant("user:ada", &egress, Some(&file)).ok_or_else(|| anyhow::anyhow!("no goose grant"))?;
        assert_eq!((granted.name.as_str(), granted.toward.clone()), ("goose/user-ada", vec!["chatgpt.com".to_owned()]));
        assert_eq!(granted.captured, Some(hex_api::Captured { access: "sign-ins/goose/user-ada/access".into(), access_placeholder: a.clone(), refresh_placeholder: Some(r.clone()) }));
        assert_eq!(Provider::Codex.held(&file), None, "goose's file is no Codex file");
        let codex = serde_json::json!({ "tokens": { "access_token": a, "refresh_token": r } }).to_string();
        assert_eq!(Provider::Goose.held(&codex), None, "Codex's file is no goose file");
        assert_eq!(Provider::Goose.grant("user:ada", &["auth.openai.com:443".to_owned()], None), None, "its egress doesn't reach chatgpt.com");
        Ok(())
    }

    /// Plan step 33 (Shards' D242, the sign-in profiles lane): a CLI's sign-in file is where its run
    /// environment puts it: Codex's in its `CODEX_HOME`, goose's in `config` beneath its
    /// `GOOSE_PATH_ROOT`, as kiln's goose image sets it; none where its environment doesn't say.
    #[test]
    fn s33_a_clis_sign_in_file_is_where_its_run_environment_puts_it() -> anyhow::Result<()> {
        use crate::harness::isolated::home::Layout;
        fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
            pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
        }
        let root = std::path::Path::new("/kiln/homes");
        let goose = Layout::new(root, "owner", "goose", &["/home/goose".to_owned()], &env(&[("HOME", "/home/goose"), ("GOOSE_PATH_ROOT", "/home/goose/.local/share/goose")]))?;
        assert_eq!(Provider::Goose.file_in(&goose).as_deref(), Some("/home/goose/.local/share/goose/config/chatgpt_codex/tokens.json"));
        let unsaid = Layout::new(root, "owner", "goose", &["/home/goose".to_owned()], &env(&[("HOME", "/home/goose")]))?;
        assert_eq!(Provider::Goose.file_in(&unsaid), None);
        let codex = Layout::new(root, "owner", "codex", &["/home/codex/.codex".to_owned()], &env(&[("CODEX_HOME", "/home/codex/.codex")]))?;
        assert_eq!(Provider::Codex.file_in(&codex).as_deref(), Some("/home/codex/.codex/auth.json"));
        Ok(())
    }
}

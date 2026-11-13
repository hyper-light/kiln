//! What keeps a harness from its provider, as its own stream says it, at once (kiln plan
//! 01a1cd8e862e7169a074db3d5fa33f3d, step 18; the harness bring-up's finding H6,
//! shards docs/research/measurements/harness-bringup-v1): Claude Code retries a refused credential
//! ten times with backoff, 246.5 s in all, before it gives up, and Codex without a network waits for
//! one as long as it runs. A harness says each retry as it makes it, so kiln reads the first and says
//! then that its harness can't sign in to its provider, or can't reach it ([`State::trouble`]), in
//! the harness's own words. It stands until the harness's model answers again. What to do is the
//! person's: sign it in, grant the host, or stop the turn; kiln stops nothing on its own.
//!
//! - **Claude Code** (stream-json): a `system` message `api_retry` names the HTTP status it got
//!   (`error_status`, `null` when no answer came at all) and the error's category (`error`); an
//!   `assistant` message its CLI made itself carries the category too (`error`, with
//!   `api_error_status`), as its "Not logged in · Please run /login" does, before any request. The
//!   categories are `SDKAssistantMessageError`'s (code.claude.com/docs/en/agent-sdk/typescript):
//!   `authentication_failed`, `oauth_org_not_allowed` and `cloud_credential_error` are a credential
//!   refused. A `stream_event`, or an `assistant` message with no error, is its model answering.
//! - **Codex** (app-server): an `error` notification (`willRetry`) and a failed turn's error carry
//!   `codexErrorInfo` (openai/codex rust-v0.167.2, `app-server-protocol` v2 `CodexErrorInfo`, made by
//!   `protocol/src/error.rs`'s `to_codex_protocol_error`): `unauthorized`, or a connection's
//!   `httpStatusCode` 401 or 403, is its credential refused; a connection that failed with no status
//!   is its provider unreached, and so is Codex's own "waiting for network" (`core/src/
//!   responses_retry.rs`, its unbounded wait) and a name that didn't resolve (Rust's own words for
//!   getaddrinfo's error, "failed to lookup address information", which Codex passes on inside an
//!   error of no kind, `other`). Its model's deltas, and a turn that completed, are it answering.
//!
//! The statuses are HTTP's own: 401, a request without valid credentials, and 403, credentials that
//! aren't let do it (RFC 9110 §15.5.2, §15.5.4).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::core::Core;
use super::model::{Body, Level};

/// What keeps it from its provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum TroubleKind {
    /// Its provider refused its credential (or it has none it can send).
    Auth,
    /// Its provider didn't answer: no network, a name that didn't resolve, a connection refused.
    Reach,
}

/// What its harness said keeps it from its provider (`State::trouble`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Trouble {
    pub kind: TroubleKind,
    /// The HTTP status its provider answered with (401, 403), when it answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub status: Option<u16>,
    /// What its harness said, in its own words (the last time it said it).
    pub said: String,
    /// Unix ms its harness first said it.
    pub since: i64,
    /// How many times its harness has said it since (its retries).
    pub times: u32,
}

/// One thing a harness's stream said of its provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seen {
    /// That it can't get to it.
    Trouble { kind: TroubleKind, status: Option<u16>, said: String },
    /// That its model answered.
    Answered,
}

/// Claude Code's error categories that are a credential refused.
const CLAUDE_AUTH: &[&str] = &["authentication_failed", "oauth_org_not_allowed", "cloud_credential_error"];
/// The model Claude Code names on the messages its CLI makes itself, its model asked nothing (the
/// bring-up's records: "Not logged in · Please run /login", "Invalid API key").
const CLAUDE_SYNTHETIC: &str = "<synthetic>";

/// HTTP's statuses for a credential refused (RFC 9110 §15.5.2, §15.5.4).
fn refused(status: u16) -> bool {
    matches!(status, 401 | 403)
}

/// A status as the harness gave it: a number that fits, else none.
fn status_of(v: Option<&Value>) -> Option<u16> {
    v.and_then(Value::as_u64).and_then(|s| u16::try_from(s).ok())
}

/// What one line of Claude Code's stream-json says of its provider, if anything.
pub fn of_claude(v: &Value) -> Option<Seen> {
    let ty = v.get("type").and_then(Value::as_str)?;
    match ty {
        "system" if v.get("subtype").and_then(Value::as_str) == Some("api_retry") => {
            let category = v.get("error").and_then(Value::as_str);
            let status = status_of(v.get("error_status"));
            let attempt = v.get("attempt").and_then(Value::as_u64);
            let said = match (category, status, attempt) {
                (Some(c), Some(s), Some(a)) => format!("retrying the API, attempt {a}: {c} (HTTP {s})"),
                (Some(c), None, Some(a)) => format!("retrying the API, attempt {a}: {c} (no answer)"),
                (Some(c), _, None) => format!("retrying the API: {c}"),
                (None, Some(s), _) => format!("retrying the API: HTTP {s}"),
                (None, None, _) => "retrying the API: no answer".to_owned(),
            };
            if category.is_some_and(|c| CLAUDE_AUTH.contains(&c)) || status.is_some_and(refused) {
                return Some(Seen::Trouble { kind: TroubleKind::Auth, status, said });
            }
            // `error_status` is null when its request got no answer at all; left out, it says nothing.
            if v.get("error_status").is_some_and(Value::is_null) {
                return Some(Seen::Trouble { kind: TroubleKind::Reach, status: None, said });
            }
            None
        }
        "assistant" => {
            // What its CLI says itself (a local command's output, its own errors) is no answer of its model's.
            let synthetic = v.pointer("/message/model").and_then(Value::as_str) == Some(CLAUDE_SYNTHETIC);
            let Some(category) = v.get("error").and_then(Value::as_str) else { return (!synthetic).then_some(Seen::Answered) };
            if !CLAUDE_AUTH.contains(&category) {
                return None;
            }
            let text: String = v.pointer("/message/content").and_then(Value::as_array).map(|blocks| blocks.iter().filter_map(|b| b.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join(" ")).unwrap_or_default();
            let said = if text.trim().is_empty() { category.to_owned() } else { super::claude::strip_ansi(text.trim()) };
            Some(Seen::Trouble { kind: TroubleKind::Auth, status: status_of(v.get("api_error_status")), said })
        }
        "stream_event" => Some(Seen::Answered),
        _ => None,
    }
}

/// Codex's `codexErrorInfo` kinds that carry the HTTP status a request got (`httpStatusCode`, null
/// when it got none). A retry's is always `responseStreamDisconnected` (`notify_stream_error`,
/// codex-rs `core/src/session/mod.rs`); a turn's end names how it failed.
const CODEX_STATUSED: &[&str] = &["httpConnectionFailed", "responseStreamConnectionFailed", "responseStreamDisconnected", "responseTooManyFailedAttempts"];

/// What a Codex error (`TurnError`: `message`, `codexErrorInfo`, `additionalDetails`) says of its
/// provider, if anything. A retry's message is only "Reconnecting... 2/5": what failed is in its
/// details.
fn codex_error(err: &Value) -> Option<Seen> {
    let text = |key: &str| err.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let said = match (text("message"), text("additionalDetails")) {
        (Some(m), Some(d)) => format!("{m} ({d})"),
        (m, d) => m.or(d).unwrap_or_default().to_owned(),
    };
    let info = err.get("codexErrorInfo").or_else(|| err.get("codex_error_info"));
    match info {
        Some(Value::String(s)) if s == "unauthorized" => return Some(Seen::Trouble { kind: TroubleKind::Auth, status: None, said }),
        Some(Value::Object(o)) => {
            for detail in o.iter().filter(|(k, _)| CODEX_STATUSED.contains(&k.as_str())).map(|(_, d)| d) {
                let status = status_of(detail.get("httpStatusCode"));
                if status.is_some_and(refused) {
                    return Some(Seen::Trouble { kind: TroubleKind::Auth, status, said });
                }
                // No status: no answer came (no network, a name that didn't resolve, a connection
                // refused or cut). A status that's no refusal is the provider answering otherwise.
                if detail.get("httpStatusCode").is_none_or(Value::is_null) {
                    return Some(Seen::Trouble { kind: TroubleKind::Reach, status: None, said });
                }
            }
        }
        _ => {}
    }
    // Its unbounded wait for a network, and a name that didn't resolve, in an error of no kind.
    if said.contains("waiting for network") || said.contains("failed to lookup address information") {
        return Some(Seen::Trouble { kind: TroubleKind::Reach, status: None, said });
    }
    None
}

/// What one of Codex's app-server notifications says of its provider, if anything.
pub fn of_codex(method: &str, p: &Value) -> Option<Seen> {
    match method {
        "error" => codex_error(p.get("error")?),
        "turn/completed" => match p.pointer("/turn/status").and_then(Value::as_str) {
            Some("failed") => codex_error(p.pointer("/turn/error")?),
            Some("completed") => Some(Seen::Answered),
            _ => None,
        },
        "item/agentMessage/delta" | "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => Some(Seen::Answered),
        _ => None,
    }
}

/// `seen` taken into the session's state: a trouble it hadn't said is said once, in the timeline,
/// and kept with how many times its harness has said it since; its model answering ends it, and
/// that's said too.
pub fn take(core: &mut Core, seen: Seen) {
    let harness = core.harness.clone().unwrap_or_else(|| "Its harness".to_owned());
    match seen {
        Seen::Answered => {
            let Some(was) = core.state.trouble.take() else { return };
            let words = match was.kind {
                TroubleKind::Auth => format!("{harness} is signed in to its provider again"),
                TroubleKind::Reach => format!("{harness} reaches its provider again"),
            };
            core.upsert(&format!("trouble-over-{}", was.since), None, Body::Notice { level: Level::Info, text: words });
            core.touch_state();
        }
        Seen::Trouble { kind, status, said } => {
            if let Some(t) = core.state.trouble.as_mut().filter(|t| t.kind == kind) {
                t.times = t.times.saturating_add(1);
                t.status = status.or(t.status);
                t.said = said;
                core.touch_state();
                return;
            }
            let since = Core::now();
            let words = match (kind, status) {
                (TroubleKind::Auth, Some(s)) => format!("{harness} can't sign in to its provider (HTTP {s}): {said}. It retries, but gets no further until it's signed in again."),
                (TroubleKind::Auth, None) => format!("{harness} can't sign in to its provider: {said}. It retries, but gets no further until it's signed in again."),
                (TroubleKind::Reach, _) => format!("{harness} can't reach its provider: {said}. It waits for it; a host its microVM's grants don't name is never reached."),
            };
            core.upsert(&format!("trouble-{}-{since}", if kind == TroubleKind::Auth { "auth" } else { "reach" }), None, Body::Notice { level: Level::Error, text: words });
            core.state.trouble = Some(Trouble { kind, status, said, since, times: 1 });
            core.touch_state();
        }
    }
}

#[cfg(test)]
mod tests;

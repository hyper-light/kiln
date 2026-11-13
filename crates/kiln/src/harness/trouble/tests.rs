//! H6's streams through the real adapters (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 18).
//!
//! `testdata/claude-401.jsonl` and `testdata/claude-signed-out.jsonl` are recorded, byte for byte:
//! Claude Code 2.1.322 in a Shards microVM, `claude -p --output-format stream-json --verbose`, the
//! stream-json kiln's Claude adapter reads (shards `docs/research/measurements/harness-bringup-v1/
//! records/claude-key-volume/stdout.bin`, sha256 353cd709acc69a25d723a23b39d6da0ed0c4e197c744444
//! 4b47f04f5f6af2f88, a placeholder key; `records/claude/stdout.bin`, 634ada01452b32af3819472e3cfa9
//! 170e1ce04da12e88d8f2b85ab964e43d511, no credential; shards commit 6e17b36b9).
//!
//! `testdata/codex-401.jsonl` and `testdata/codex-airgapped.jsonl` are Codex's app-server
//! notifications for those runs, made from Codex's source at rust-v0.167.2 (the version kiln pins),
//! as the bring-up ran `codex exec --json`, which prints them otherwise: a retry is an `error`
//! notification with `willRetry` (`app-server/src/bespoke_event_handling.rs`), its message
//! "Reconnecting... N/M" or "Reconnecting... waiting for network" alone, its `codexErrorInfo`
//! `responseStreamDisconnected` with the request's HTTP status, its error in `additionalDetails`
//! (`core/src/session/mod.rs`, `notify_stream_error`). exec prints them as `message (details)`, as
//! the records have them: their words are the records', split back into the two fields. The 401
//! turn's end is `to_error_event`'s (`protocol/src/error.rs`). The record cut "waiting for network
//! (C" short; Codex's own text for that error starts "Connection failed", and only that is here.

use serde_json::{Value, json};

use super::{Seen, TroubleKind, of_claude, of_codex, take};
use crate::harness::claude::Claude;
use crate::harness::codex::Codex;
use crate::harness::core::Core;
use crate::harness::model::{Body, Level};

const CLAUDE_401: &str = include_str!("testdata/claude-401.jsonl");
const CLAUDE_SIGNED_OUT: &str = include_str!("testdata/claude-signed-out.jsonl");
const CODEX_401: &str = include_str!("testdata/codex-401.jsonl");
const CODEX_AIRGAPPED: &str = include_str!("testdata/codex-airgapped.jsonl");

fn core_of(harness: &str) -> Core {
    let mut core = Core::new();
    core.harness = Some(harness.to_owned());
    core
}

/// The timeline's notices at `level`.
fn notices(core: &Core, level: Level) -> Vec<String> {
    core.events()
        .filter_map(|e| match &e.body {
            Body::Notice { level: l, text } if *l == level => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn trouble_notices(core: &Core) -> Vec<String> {
    notices(core, Level::Error).into_iter().filter(|t| t.contains("can't sign in to its provider") || t.contains("can't reach its provider")).collect()
}

/// Claude Code's 401: said at its first retry, its ten retries and its own giving up counted in
/// the one trouble, said once. The record's own backoff says how long the harness would have gone
/// on before saying so itself.
#[test]
fn step18_h6_claude_says_its_401_at_its_first_retry() -> anyhow::Result<()> {
    let mut claude = Claude::new(false);
    let mut core = core_of("Claude Code");
    let lines: Vec<&str> = CLAUDE_401.lines().collect();
    let first = lines.iter().position(|l| l.contains("\"api_retry\"")).ok_or_else(|| anyhow::anyhow!("the record has no retry"))?;
    for (i, line) in lines.iter().enumerate() {
        claude.line(&mut core, line);
        if i < first {
            anyhow::ensure!(core.state.trouble.is_none(), "said before its first retry, at line {i}");
        }
        if i == first {
            let t = core.state.trouble.clone().ok_or_else(|| anyhow::anyhow!("not said at its first retry"))?;
            anyhow::ensure!(t.kind == TroubleKind::Auth && t.status == Some(401) && t.times == 1, "{t:?}");
            anyhow::ensure!(trouble_notices(&core).len() == 1, "{:?}", notices(&core, Level::Error));
        }
    }
    let t = core.state.trouble.clone().ok_or_else(|| anyhow::anyhow!("its trouble went"))?;
    // Ten retries, then its CLI's own "Invalid API key": eleven times in one trouble, said once.
    anyhow::ensure!(t.kind == TroubleKind::Auth && t.times == 11 && t.said.contains("Invalid API key"), "{t:?}");
    let said = trouble_notices(&core);
    anyhow::ensure!(said.len() == 1 && said.iter().all(|s| s.starts_with("Claude Code can't sign in to its provider (HTTP 401)")), "{said:?}");
    // What the harness itself still had to wait out once kiln had said it (the record's own
    // `retry_delay_ms`, from its first retry on), of the turn's `duration_ms`.
    let records: Vec<Value> = lines.iter().map(|l| serde_json::from_str(l)).collect::<Result<_, _>>()?;
    let ahead: u64 = records.iter().skip(first).filter_map(|v| v.get("retry_delay_ms").and_then(Value::as_u64)).sum();
    let turn = records.iter().find_map(|v| v.get("duration_ms").and_then(Value::as_u64)).ok_or_else(|| anyhow::anyhow!("no duration"))?;
    anyhow::ensure!((ahead, turn) == (240_673, 246_515), "{ahead} ms of {turn} ms");
    Ok(())
}

/// Signed out, Claude Code refuses before any request: its own "Not logged in" is the trouble.
#[test]
fn step18_h6_claude_signed_out_is_said_as_its_cli_says_it() -> anyhow::Result<()> {
    let mut claude = Claude::new(false);
    let mut core = core_of("Claude Code");
    for line in CLAUDE_SIGNED_OUT.lines() {
        claude.line(&mut core, line);
    }
    let t = core.state.trouble.clone().ok_or_else(|| anyhow::anyhow!("not said"))?;
    anyhow::ensure!(t.kind == TroubleKind::Auth && t.status.is_none() && t.said == "Not logged in · Please run /login" && t.times == 1, "{t:?}");
    anyhow::ensure!(trouble_notices(&core).len() == 1);
    Ok(())
}

/// Codex's 401: said at its first retry, its turn's failure counted in it.
#[test]
fn step18_h6_codex_says_its_401_at_its_first_retry() -> anyhow::Result<()> {
    let mut codex = Codex::new(None, None, None, false);
    let mut core = core_of("Codex");
    let lines: Vec<&str> = CODEX_401.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        codex.line(&mut core, line);
        match i {
            0 => anyhow::ensure!(core.state.trouble.is_none(), "said as its turn started"),
            1 => {
                let t = core.state.trouble.clone().ok_or_else(|| anyhow::anyhow!("not said at its first retry"))?;
                anyhow::ensure!(t.kind == TroubleKind::Auth && t.status == Some(401) && t.said.starts_with("Reconnecting... 2/5 (unexpected status 401 Unauthorized"), "{t:?}");
            }
            _ => {}
        }
    }
    let t = core.state.trouble.clone().ok_or_else(|| anyhow::anyhow!("its trouble went"))?;
    anyhow::ensure!(t.kind == TroubleKind::Auth && t.times == 4, "{t:?}");
    let said = trouble_notices(&core);
    anyhow::ensure!(said.len() == 1 && said.iter().all(|s| s.starts_with("Codex can't sign in to its provider (HTTP 401)")), "{said:?}");
    Ok(())
}

/// Codex with no network: said at its first retry, and while it waits for a network (which it
/// would do as long as it runs).
#[test]
fn step18_h6_codex_without_a_network_is_said_while_it_waits() -> anyhow::Result<()> {
    let mut codex = Codex::new(None, None, None, false);
    let mut core = core_of("Codex");
    let lines: Vec<&str> = CODEX_AIRGAPPED.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        codex.line(&mut core, line);
        if i == 1 {
            let t = core.state.trouble.clone().ok_or_else(|| anyhow::anyhow!("not said at its first retry"))?;
            anyhow::ensure!(t.kind == TroubleKind::Reach && t.status.is_none() && t.said.contains("failed to lookup address information"), "{t:?}");
        }
    }
    let t = core.state.trouble.clone().ok_or_else(|| anyhow::anyhow!("its trouble went"))?;
    anyhow::ensure!(t.kind == TroubleKind::Reach && t.times == 3 && t.said.starts_with("Reconnecting... waiting for network"), "{t:?}");
    let said = trouble_notices(&core);
    anyhow::ensure!(said.len() == 1 && said.iter().all(|s| s.starts_with("Codex can't reach its provider")), "{said:?}");
    Ok(())
}

/// Its model answering ends it, said; what its CLI says itself doesn't; a trouble of the other kind
/// is said anew.
#[test]
fn step18_h6_an_answer_ends_it_and_another_kind_is_said_anew() -> anyhow::Result<()> {
    let mut core = core_of("Claude Code");
    let retry = |status: Value, error: &str| json!({"type": "system", "subtype": "api_retry", "attempt": 3, "max_retries": 10, "retry_delay_ms": 2011, "error_status": status, "error": error});
    for v in [retry(json!(401), "authentication_failed"), retry(json!(null), "unknown")] {
        if let Some(seen) = of_claude(&v) {
            take(&mut core, seen);
        }
    }
    let t = core.state.trouble.clone().ok_or_else(|| anyhow::anyhow!("none"))?;
    anyhow::ensure!(t.kind == TroubleKind::Reach && t.times == 1, "{t:?}");
    anyhow::ensure!(trouble_notices(&core).len() == 2, "{:?}", trouble_notices(&core));
    // The CLI's own words (a local command's output) are no answer of its model's.
    let local = json!({"type": "assistant", "message": {"model": "<synthetic>", "content": [{"type": "text", "text": "Total cost: $0.00"}]}});
    anyhow::ensure!(of_claude(&local).is_none());
    let answered = json!({"type": "stream_event", "event": {"type": "message_start"}});
    take(&mut core, of_claude(&answered).ok_or_else(|| anyhow::anyhow!("a stream event is no answer"))?);
    anyhow::ensure!(core.state.trouble.is_none());
    anyhow::ensure!(notices(&core, Level::Info).iter().any(|n| n == "Claude Code reaches its provider again"), "{:?}", notices(&core, Level::Info));
    // Nothing to end, nothing said.
    take(&mut core, Seen::Answered);
    anyhow::ensure!(notices(&core, Level::Info).len() == 1);
    // Codex's model answering ends its too.
    let mut core = core_of("Codex");
    let mut codex = Codex::new(None, None, None, false);
    for line in CODEX_401.lines().take(2) {
        codex.line(&mut core, line);
    }
    anyhow::ensure!(core.state.trouble.is_some());
    codex.line(&mut core, &json!({"method": "item/agentMessage/delta", "params": {"threadId": "th1", "turnId": "t1", "itemId": "i1", "delta": "ready"}}).to_string());
    anyhow::ensure!(core.state.trouble.is_none());
    anyhow::ensure!(notices(&core, Level::Info).iter().any(|n| n == "Codex is signed in to its provider again"));
    Ok(())
}

/// What's neither a credential refused nor a provider unreached says nothing: an overloaded or
/// rate-limited provider answered, a usage limit is `pause`'s, a server's error is its own.
#[test]
fn step18_h6_other_errors_are_no_trouble() {
    let retry = |status: Value, error: &str| json!({"type": "system", "subtype": "api_retry", "attempt": 3, "error_status": status, "error": error});
    assert_eq!(of_claude(&retry(json!(529), "overloaded")), None);
    assert_eq!(of_claude(&retry(json!(429), "rate_limit")), None);
    assert_eq!(of_claude(&retry(json!(500), "server_error")), None);
    // An older CLI's retry that names no status at all says nothing of an answer.
    assert_eq!(of_claude(&json!({"type": "system", "subtype": "api_retry", "attempt": 3, "error": "unknown"})), None);
    assert_eq!(of_claude(&json!({"type": "assistant", "error": "billing_error", "message": {"model": "<synthetic>", "content": []}})), None);
    let error = |info: Value, message: &str| json!({"error": {"message": message, "codexErrorInfo": info}, "willRetry": true, "threadId": "th1", "turnId": "t1"});
    assert_eq!(of_codex("error", &error(json!({"responseStreamDisconnected": {"httpStatusCode": 500}}), "Reconnecting... 1/5")), None);
    assert_eq!(of_codex("error", &error(json!("usageLimitExceeded"), "You've hit your usage limit.")), None);
    assert_eq!(of_codex("error", &error(json!("other"), "Reconnecting... 1/5")), None);
    assert_eq!(of_codex("turn/completed", &json!({"turn": {"id": "t1", "status": "interrupted"}})), None);
    // Its credential refused by a status anywhere a status is carried, and outright.
    assert!(matches!(of_codex("error", &error(json!({"httpConnectionFailed": {"httpStatusCode": 403}}), "x")), Some(Seen::Trouble { kind: TroubleKind::Auth, status: Some(403), .. })));
    assert!(matches!(of_codex("turn/completed", &json!({"turn": {"id": "t1", "status": "failed", "error": {"message": "Your access token could not be refreshed", "codexErrorInfo": "unauthorized"}}})), Some(Seen::Trouble { kind: TroubleKind::Auth, status: None, .. })));
}

//! The audit log, v2 (docs/ci.md W19-R05): one table, `audit2`, a row per change.
//!
//! - **A request that changes something** (any method but GET and HEAD, past authentication)
//!   writes exactly one row, refused or not: who (`principal`, and `on_behalf_of` for a token or
//!   an agent acting for a user), with which `token`, in what `scope`, the route (`action`), the
//!   path (`target`), the status (`result`) and the address (`ip`). What its handler records
//!   on the way (`Store::audit`) becomes that row's `detail`, not rows of its own: the request
//!   runs inside a slot, a task-local of sorts, that collects them.
//! - **What isn't a request** (a leg reading secrets, a scheduled exchange, a terminal session
//!   opened by a GET) writes a row of its own through `Store::audit`.
//! - Rows are kept `[retention] audit_days` (400 by default); today's v1 rows moved in with
//!   migration `20261018-2200_w19_audit2.sql`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use parking_lot::Mutex;
use rusqlite::params;
use serde_json::{Value, json};

use crate::authz::Caller;
use crate::hub::Hub;
use crate::store::Store;

/// What a request's handlers recorded, for its row.
#[derive(Debug, Default)]
pub struct Slot {
    notes: Vec<String>,
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<Mutex<Slot>>>> = const { RefCell::new(None) };
}

/// Puts the slot back as it was, even when polling unwinds.
struct Restore(Option<Arc<Mutex<Slot>>>);

impl Drop for Restore {
    fn drop(&mut self) {
        let prev = self.0.take();
        CURRENT.with(|c| *c.borrow_mut() = prev);
    }
}

/// A future that runs with `slot` as the current one: what it records goes there.
pub struct Scoped<F: Future> {
    inner: Pin<Box<F>>,
    slot: Arc<Mutex<Slot>>,
}

impl<F: Future> Future for Scoped<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let slot = Arc::clone(&self.slot);
        let _restore = Restore(CURRENT.with(|c| c.replace(Some(slot))));
        self.inner.as_mut().poll(cx)
    }
}

/// When a request is being audited here: `detail` goes into its row, and the call is done.
pub fn note(actor: &str, action: &str, detail: &str) -> bool {
    CURRENT.with(|c| match c.borrow().as_ref() {
        Some(slot) => {
            let text = if detail.is_empty() { action.to_owned() } else { format!("{action}: {detail}") };
            let mut s = slot.lock();
            // Who the handler named, when not the request's principal (`admin` for the owner).
            s.notes.push(if actor.is_empty() || actor == "admin" { text } else { format!("{text} (as {actor})") });
            true
        }
        None => false,
    })
}

/// One row.
#[derive(Debug, Clone, Default)]
pub struct Entry {
    pub principal: String,
    pub on_behalf_of: Option<String>,
    pub token: Option<String>,
    pub scope: Option<String>,
    pub action: String,
    pub target: Option<String>,
    pub result: String,
    pub ip: Option<String>,
    pub detail: String,
}

pub async fn write(store: &Store, e: Entry) {
    let now = crate::util::now_unix();
    let r = store
        .call(move |c| {
            c.execute(
                "INSERT INTO audit2 (ts, principal, on_behalf_of, token, scope, action, target, result, ip, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![now, e.principal, e.on_behalf_of, e.token, e.scope, e.action, e.target, e.result, e.ip, e.detail],
            )?;
            Ok(())
        })
        .await;
    if let Err(e) = r {
        tracing::warn!(error = %e, "audit write failed");
    }
}

/// `principal` and `on_behalf_of` from a caller's name: `agent:x for user:ada`, `pat:01ab of
/// user:ada`, `user:ada`, `admin`, `job:<leg>`.
pub fn who(caller: &Caller) -> (String, Option<String>) {
    match caller.name.split_once(" for ").or_else(|| caller.name.split_once(" of ")) {
        Some((p, b)) => (p.to_owned(), Some(b.to_owned())),
        None => (caller.name.clone(), None),
    }
}

/// A request, as its row names it.
pub struct Req<'a> {
    pub caller: &'a Caller,
    pub method: &'a http::Method,
    pub path: &'a str,
    pub q: &'a HashMap<String, String>,
    pub ip: Option<std::net::IpAddr>,
}

/// Audits a request that changes something: runs `fut` with a slot for what its handlers
/// record, then writes the one row, with the status `status_of` reads from the result.
pub async fn request<F, T>(hub: &Hub, r: Req<'_>, fut: F, status_of: impl Fn(&T) -> u16) -> T
where
    F: Future<Output = T>,
{
    let Req { caller, method, path, q, ip } = r;
    let slot = Arc::new(Mutex::new(Slot::default()));
    let out = Scoped { inner: Box::pin(fut), slot: Arc::clone(&slot) }.await;
    let segs: Vec<&str> = path.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    let rule = crate::authz::rule(method, &segs);
    let scope = crate::authz::gate::scope_of(hub, rule, &segs, q).await.text();
    let (principal, on_behalf_of) = who(caller);
    let notes = std::mem::take(&mut slot.lock().notes);
    let action = if rule.pattern == crate::authz::UNNAMED.pattern { format!("{method} {path}") } else { format!("{method} {}", rule.pattern) };
    write(
        &hub.store,
        Entry { principal, on_behalf_of, token: caller.token.clone(), scope: Some(scope), action, target: Some(path.to_owned()), result: status_of(&out).to_string(), ip: ip.map(|i| i.to_string()), detail: notes.join("; ") },
    )
    .await;
    out
}

/// `GET /api/audit`: newest first, filtered by any field (`principal`, `on_behalf_of`, `token`,
/// `scope`, `action`, `target`, `result`, `ip`: each exactly one of its comma-separated values;
/// `since`, `until`: Unix seconds; `q`: a substring anywhere), a page at a time (`before`: the
/// last row's `id`).
pub async fn list(store: &Store, q: &HashMap<String, String>) -> anyhow::Result<Value> {
    const FIELDS: &[&str] = &["principal", "on_behalf_of", "token", "scope", "action", "target", "result", "ip"];
    let mut clauses = Vec::new();
    let mut args: Vec<rusqlite::types::Value> = Vec::new();
    for f in FIELDS {
        let Some(v) = q.get(*f).map(|v| v.trim()).filter(|v| !v.is_empty()) else { continue };
        let mut any = Vec::new();
        for one in v.split(',').map(str::trim).filter(|o| !o.is_empty()) {
            args.push(one.to_owned().into());
            any.push(format!("?{}", args.len()));
        }
        if !any.is_empty() {
            clauses.push(format!("{f} IN ({})", any.join(", ")));
        }
    }
    if let Some(v) = q.get("q").map(|v| v.trim()).filter(|v| !v.is_empty()) {
        args.push(format!("%{}%", v.replace('%', "\\%").replace('_', "\\_")).into());
        let n = args.len();
        clauses.push(format!("(principal LIKE ?{n} ESCAPE '\\' OR COALESCE(on_behalf_of, '') LIKE ?{n} ESCAPE '\\' OR action LIKE ?{n} ESCAPE '\\' OR COALESCE(target, '') LIKE ?{n} ESCAPE '\\' OR detail LIKE ?{n} ESCAPE '\\')"));
    }
    for (k, op) in [("since", ">="), ("until", "<"), ("before", "<")] {
        if let Some(v) = q.get(k).and_then(|v| v.parse::<i64>().ok()) {
            args.push(v.into());
            clauses.push(format!("{} {op} ?{}", if k == "before" { "id" } else { "ts" }, args.len()));
        }
    }
    let limit: i64 = q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(200).clamp(1, 1000);
    args.push(limit.into());
    let sql = format!(
        "SELECT id, ts, principal, on_behalf_of, token, scope, action, target, result, ip, detail FROM audit2 {} ORDER BY id DESC LIMIT ?{}",
        if clauses.is_empty() { String::new() } else { format!("WHERE {}", clauses.join(" AND ")) },
        args.len()
    );
    store
        .call(move |c| {
            let mut st = c.prepare(&sql)?;
            let rows = st.query_map(rusqlite::params_from_iter(args.iter()), |r| {
                let principal: String = r.get(2)?;
                let behalf: Option<String> = r.get(3)?;
                Ok(json!({
                    "id": r.get::<_, i64>(0)?, "ts": r.get::<_, i64>(1)?, "principal": principal, "on_behalf_of": behalf,
                    "token": r.get::<_, Option<String>>(4)?, "scope": r.get::<_, Option<String>>(5)?, "action": r.get::<_, String>(6)?,
                    "target": r.get::<_, Option<String>>(7)?, "result": r.get::<_, String>(8)?, "ip": r.get::<_, Option<String>>(9)?,
                    "detail": r.get::<_, String>(10)?,
                    // v1's field, for its readers.
                    "actor": match &behalf { Some(b) => format!("{principal} for {b}"), None => principal.clone() },
                }))
            })?;
            Ok(Value::Array(rows.collect::<Result<Vec<_>, _>>()?))
        })
        .await
}

/// A workload's credentials from an integration, for kiln's ID token (W11's federation): who it
/// was for, and who set it going.
pub async fn exchange(hub: &Hub, i: &crate::ci::integrations::Integration, w: &kiln_ci::oidc::Workload<'_>, what: &str) {
    let workload = format!("{}:{}", w.source, w.app.unwrap_or(w.repository));
    let env = w.environment.map(|e| format!(", environment {e}")).unwrap_or_default();
    hub.store.audit(format!("{workload} for {}", w.actor), "integration.exchange".into(), format!("{} ({}) for {}{env}: {what}", i.name, i.kind, w.repository)).await;
}

/// Drops rows older than `days`.
pub async fn prune(store: &Store, days: u32) {
    let before = crate::util::now_unix() - i64::from(days.max(1)) * 86_400;
    let _ = store.call(move |c| Ok(c.execute("DELETE FROM audit2 WHERE ts < ?1", [before]).map(|_| ())?)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w19_r05_who() {
        let agent = Caller::with(crate::authz::gate::Kind::Agent, "agent:fixer for user:ada", None, None);
        assert_eq!(who(&agent), ("agent:fixer".into(), Some("user:ada".into())));
        let pat = Caller::with(crate::authz::gate::Kind::Pat, "pat:01ab of user:ada", None, None);
        assert_eq!(who(&pat), ("pat:01ab".into(), Some("user:ada".into())));
        assert_eq!(who(&Caller::owner()), ("admin".into(), None));
    }

    /// Inside a slot, what's recorded joins it; outside, it doesn't.
    #[test]
    fn w19_r05_slots() {
        assert!(!note("admin", "x", ""), "no request: a row of its own");
        let slot = Arc::new(Mutex::new(Slot::default()));
        let fut = Scoped {
            inner: Box::pin(async {
                assert!(note("admin", "integration.create", "audit (webhook)"));
                assert!(note("user:bob", "secret.set", "TOKEN"));
            }),
            slot: Arc::clone(&slot),
        };
        let mut fut = std::pin::pin!(fut);
        assert!(fut.as_mut().poll(&mut Context::from_waker(std::task::Waker::noop())).is_ready());
        assert!(!note("admin", "after", ""), "the slot ends with its request");
        assert_eq!(slot.lock().notes, vec!["integration.create: audit (webhook)".to_owned(), "secret.set: TOKEN (as user:bob)".to_owned()]);
    }
}

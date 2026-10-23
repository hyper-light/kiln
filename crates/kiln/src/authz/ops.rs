//! Access and the audit trail as kiln's operations (docs/ci.md §4.1, W20-AC01): the people who
//! sign in (`users.*`), what they may do (`grants.*`, `roles.*`), tokens (`tokens.*`) and the audit
//! trail (`audit.list`). Each is its HTTP route, checked by the route table as the route is and
//! run by the route's own handler, so an agent does through them exactly what its caller may.

use std::collections::HashMap;

use http::{Method, StatusCode};
use serde_json::{Value, json};

use crate::ops::{Cx, Op, Out, path, schema, text};

/// Route `segs` of the identity routes, run for the caller, its answer as an operation's.
async fn identity(cx: &Cx, method: Method, segs: &[String], q: HashMap<String, String>, body: Value) -> Out {
    let s: Vec<&str> = segs.iter().map(String::as_str).collect();
    let r = super::api::call(&cx.hub, &cx.caller, &method, &s, &q, &body).await.ok_or_else(|| (StatusCode::NOT_FOUND, "no such route".to_owned()))?;
    crate::ci::ops::answer(r).await
}

/// These keys of `v`, as a query.
fn query(v: &Value, keys: &[&str]) -> HashMap<String, String> {
    keys.iter()
        .filter_map(|k| {
            let s = match v.get(*k)? {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                _ => return None,
            };
            Some(((*k).to_owned(), s))
        })
        .collect()
}

/// `v` with only these keys (what a route's body takes).
fn only(v: &Value, keys: &[&str]) -> Value {
    Value::Object(keys.iter().filter_map(|k| Some(((*k).to_owned(), v.get(*k)?.clone()))).collect())
}

const USER: &str = "The user: their name or id.";
const AUDIT: &[&str] = &["principal", "on_behalf_of", "token", "scope", "action", "target", "result", "ip", "q", "since", "until", "before", "limit"];

pub fn list_ops() -> Vec<Op> {
    vec![
        Op {
            name: "users.list",
            summary: "The people who sign in here: each one's name, email, grants, whether they're disabled, and when they last signed in.",
            read_only: true,
            destructive: false,
            input: || schema(json!({}), &[]),
            route: |_| Ok((Method::GET, path(&["api", "users"]))),
            run: |cx, _| Box::pin(async move { identity(&cx, Method::GET, &path(&["api", "users"]), HashMap::new(), Value::Null).await }),
        },
        Op {
            name: "users.add",
            summary: "Add a person, with the grants they start with (`developer@org:acme`, `viewer`, …). users.link gives them a way in.",
            read_only: false,
            destructive: false,
            input: || {
                schema(
                    json!({"name": {"type": "string", "description": "Up to 64 letters, digits, `-`, `_`, `.` and `@`."}, "grants": {"type": "array", "items": {"type": "string"}}, "email": {"type": "string", "description": "The email single sign-on matches them by."}}),
                    &["name"],
                )
            },
            route: |_| Ok((Method::POST, path(&["api", "users"]))),
            run: |cx, v| Box::pin(async move { identity(&cx, Method::POST, &path(&["api", "users"]), HashMap::new(), only(&v, &["name", "grants", "email"])).await }),
        },
        Op {
            name: "users.remove",
            summary: "Disable a person: they can't sign in, and their tokens and sessions stop working. What they did stays in the audit trail.",
            read_only: false,
            destructive: true,
            input: || schema(json!({"user": {"type": "string", "description": USER}}), &["user"]),
            route: |v| Ok((Method::DELETE, path(&["api", "users", &text(v, "user")?]))),
            run: |cx, v| Box::pin(async move { identity(&cx, Method::DELETE, &path(&["api", "users", &text(&v, "user")?]), HashMap::new(), Value::Null).await }),
        },
        Op {
            name: "users.link",
            summary: "A one-time sign-in link for a person (to give them, or to sign them in on a new device).",
            read_only: false,
            destructive: false,
            input: || schema(json!({"user": {"type": "string", "description": USER}}), &["user"]),
            route: |v| Ok((Method::POST, path(&["api", "users", &text(v, "user")?, "link"]))),
            run: |cx, v| Box::pin(async move { identity(&cx, Method::POST, &path(&["api", "users", &text(&v, "user")?, "link"]), HashMap::new(), json!({})).await }),
        },
        Op {
            name: "users.sign_out",
            summary: "Sign a person out everywhere: every session of theirs ends.",
            read_only: false,
            destructive: false,
            input: || schema(json!({"user": {"type": "string", "description": USER}}), &["user"]),
            route: |v| Ok((Method::POST, path(&["api", "users", &text(v, "user")?, "sign-out"]))),
            run: |cx, v| Box::pin(async move { identity(&cx, Method::POST, &path(&["api", "users", &text(&v, "user")?, "sign-out"]), HashMap::new(), json!({})).await }),
        },
        Op {
            name: "grants.list",
            summary: "Grants: what each person may do and where (`developer@org:acme`, `!runs.cancel@repo:acme/app`); one person's with `user`.",
            read_only: true,
            destructive: false,
            input: || schema(json!({"user": {"type": "string", "description": USER}}), &[]),
            route: |_| Ok((Method::GET, path(&["api", "grants"]))),
            run: |cx, v| Box::pin(async move { identity(&cx, Method::GET, &path(&["api", "grants"]), query(&v, &["user"]), Value::Null).await }),
        },
        Op {
            name: "grants.add",
            summary: "Grant a person a role or a capability in a scope (`developer@org:acme`), or deny one (`!runs.cancel@repo:acme/app`).",
            read_only: false,
            destructive: false,
            input: || {
                schema(
                    json!({"user": {"type": "string", "description": USER}, "grant": {"type": "string", "description": "`<role or capability>@<scope>`, `!` first to deny it."}}),
                    &["user", "grant"],
                )
            },
            route: |_| Ok((Method::POST, path(&["api", "grants"]))),
            run: |cx, v| Box::pin(async move { identity(&cx, Method::POST, &path(&["api", "grants"]), HashMap::new(), only(&v, &["user", "grant"])).await }),
        },
        Op {
            name: "grants.remove",
            summary: "Take a grant away (its id, from grants.list).",
            read_only: false,
            destructive: true,
            input: || schema(json!({"grant": {"type": "string", "description": "The grant's id."}}), &["grant"]),
            route: |v| Ok((Method::DELETE, path(&["api", "grants", &text(v, "grant")?]))),
            run: |cx, v| Box::pin(async move { identity(&cx, Method::DELETE, &path(&["api", "grants", &text(&v, "grant")?]), HashMap::new(), Value::Null).await }),
        },
        Op {
            name: "roles.list",
            summary: "The roles: kiln's built-in ones and custom ones, each with its capabilities; and every capability there is.",
            read_only: true,
            destructive: false,
            input: || schema(json!({}), &[]),
            route: |_| Ok((Method::GET, path(&["api", "roles"]))),
            run: |cx, _| Box::pin(async move { identity(&cx, Method::GET, &path(&["api", "roles"]), HashMap::new(), Value::Null).await }),
        },
        Op {
            name: "roles.set",
            summary: "Make or change a custom role: a name and the capabilities it gives.",
            read_only: false,
            destructive: false,
            input: || {
                schema(
                    json!({"name": {"type": "string", "description": "Letters, digits, `-` and `_`; not a built-in role's."}, "capabilities": {"type": "array", "items": {"type": "string"}}}),
                    &["name", "capabilities"],
                )
            },
            route: |v| Ok((Method::PUT, path(&["api", "roles", &text(v, "name")?]))),
            run: |cx, v| Box::pin(async move { identity(&cx, Method::PUT, &path(&["api", "roles", &text(&v, "name")?]), HashMap::new(), only(&v, &["capabilities"])).await }),
        },
        Op {
            name: "roles.remove",
            summary: "Remove a custom role.",
            read_only: false,
            destructive: true,
            input: || schema(json!({"name": {"type": "string"}}), &["name"]),
            route: |v| Ok((Method::DELETE, path(&["api", "roles", &text(v, "name")?]))),
            run: |cx, v| Box::pin(async move { identity(&cx, Method::DELETE, &path(&["api", "roles", &text(&v, "name")?]), HashMap::new(), Value::Null).await }),
        },
        Op {
            name: "tokens.list",
            summary: "Tokens: your own, or everyone's with tokens.manage. Each with its kind, whom it acts for, its grants and when it expires; never the token itself.",
            read_only: true,
            destructive: false,
            input: || schema(json!({}), &[]),
            route: |_| Ok((Method::GET, path(&["api", "tokens"]))),
            run: |cx, _| Box::pin(async move { identity(&cx, Method::GET, &path(&["api", "tokens"]), HashMap::new(), Value::Null).await }),
        },
        Op {
            name: "tokens.create",
            summary: "Mint a token, shown once: a personal access token (`pat`) or an agent's (`agent`, acting for a person). From a signed-in session, or with tokens.manage (which also mints for someone else).",
            read_only: false,
            destructive: false,
            input: || {
                schema(
                    json!({
                        "kind": {"type": "string", "enum": ["pat", "agent"]},
                        "agent": {"type": "string", "description": "An agent token's agent."},
                        "user": {"type": "string", "description": "Whom it is for, when not you (tokens.manage): a user, or `owner`."},
                        "grants": {"type": "array", "items": {"type": "string"}, "description": "What it may do: `developer@repo:acme/app`, `viewer`, or `owner` for all its person may."},
                        "note": {"type": "string"},
                        "expires_in_days": {"type": "integer", "description": "A personal access token's life."}
                    }),
                    &["grants"],
                )
            },
            route: |_| Ok((Method::POST, path(&["api", "tokens"]))),
            run: |cx, v| {
                Box::pin(async move { identity(&cx, Method::POST, &path(&["api", "tokens"]), HashMap::new(), only(&v, &["kind", "agent", "user", "grants", "note", "expires_in_days"])).await })
            },
        },
        Op {
            name: "tokens.revoke",
            summary: "Revoke a token (its id): it stops working at once.",
            read_only: false,
            destructive: true,
            input: || schema(json!({"token": {"type": "string", "description": "The token's id."}}), &["token"]),
            route: |v| Ok((Method::DELETE, path(&["api", "tokens", &text(v, "token")?]))),
            run: |cx, v| Box::pin(async move { identity(&cx, Method::DELETE, &path(&["api", "tokens", &text(&v, "token")?]), HashMap::new(), Value::Null).await }),
        },
        Op {
            name: "tokens.rotate",
            summary: "Rotate a token: a new one, shown once, with the same grants; the old one stops working.",
            read_only: false,
            destructive: false,
            input: || schema(json!({"token": {"type": "string", "description": "The token's id."}}), &["token"]),
            route: |v| Ok((Method::POST, path(&["api", "tokens", &text(v, "token")?, "rotate"]))),
            run: |cx, v| Box::pin(async move { identity(&cx, Method::POST, &path(&["api", "tokens", &text(&v, "token")?, "rotate"]), HashMap::new(), json!({})).await }),
        },
        Op {
            name: "audit.list",
            summary: "The audit trail, newest first: who did what to what, for whom, with which token, from where, and how it went. Filtered by any of its fields, `q` across them, and a time range.",
            read_only: true,
            destructive: false,
            input: || {
                schema(
                    json!({
                        "principal": {"type": "string"}, "on_behalf_of": {"type": "string"}, "token": {"type": "string"}, "scope": {"type": "string"},
                        "action": {"type": "string"}, "target": {"type": "string"}, "result": {"type": "string"}, "ip": {"type": "string"},
                        "q": {"type": "string", "description": "Text anywhere in a row."},
                        "since": {"type": "integer", "description": "Unix seconds."}, "until": {"type": "integer"}, "before": {"type": "integer", "description": "Rows before this id: the next page."},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 1000}
                    }),
                    &[],
                )
            },
            route: |_| Ok((Method::GET, path(&["api", "audit"]))),
            run: |cx, v| Box::pin(async move { crate::audit::list(&cx.hub.store, &query(&v, AUDIT)).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))) }),
        },
    ]
}

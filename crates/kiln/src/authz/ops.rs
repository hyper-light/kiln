//! Access and the audit trail as kiln's operations (docs/ci.md §4.1, W20-AC01): the people who
//! sign in (`users.*`), what they may do (`grants.*`, `roles.*`), tokens (`tokens.*`) and the audit
//! trail (`audit.list`). Each is its HTTP route, checked by the route table as the route is and
//! run by the route's own handler, so an agent does through them exactly what its caller may.

use std::collections::HashMap;

use http::{Method, StatusCode};
use serde_json::{Value, json};

use crate::ops::{Cx, Input, Op, Out, path, text};

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

const AUDIT: &[&str] = &["principal", "on_behalf_of", "token", "scope", "action", "target", "result", "ip", "q", "since", "until", "before", "limit"];

pub fn list_ops() -> Vec<Op> {
    vec![
        Op {
            name: "users.list",
            summary: "The people who sign in here: each one's name, email, grants, whether they're disabled, and when they last signed in.",
            read_only: true,
            destructive: false,
            input: Input::of::<input::UsersList>(),
            route: |_| Ok((Method::GET, path(&["api", "users"]))),
            run: |cx, _| Box::pin(async move { identity(&cx, Method::GET, &path(&["api", "users"]), HashMap::new(), Value::Null).await }),
        },
        Op {
            name: "users.add",
            summary: "Add a person, with the grants they start with (`developer@org:acme`, `viewer`, …). users.link gives them a way in.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::UsersAdd>(),
            route: |_| Ok((Method::POST, path(&["api", "users"]))),
            run: |cx, v| Box::pin(async move { identity(&cx, Method::POST, &path(&["api", "users"]), HashMap::new(), only(&v, &["name", "grants", "email"])).await }),
        },
        Op {
            name: "users.remove",
            summary: "Disable a person: they can't sign in, and their tokens and sessions stop working. What they did stays in the audit trail.",
            read_only: false,
            destructive: true,
            input: Input::of::<input::UsersRemove>(),
            route: |v| Ok((Method::DELETE, path(&["api", "users", &text(v, "user")?]))),
            run: |cx, v| Box::pin(async move { identity(&cx, Method::DELETE, &path(&["api", "users", &text(&v, "user")?]), HashMap::new(), Value::Null).await }),
        },
        Op {
            name: "users.link",
            summary: "A one-time sign-in link for a person (to give them, or to sign them in on a new device).",
            read_only: false,
            destructive: false,
            input: Input::of::<input::UsersLink>(),
            route: |v| Ok((Method::POST, path(&["api", "users", &text(v, "user")?, "link"]))),
            run: |cx, v| Box::pin(async move { identity(&cx, Method::POST, &path(&["api", "users", &text(&v, "user")?, "link"]), HashMap::new(), json!({})).await }),
        },
        Op {
            name: "users.sign_out",
            summary: "Sign a person out everywhere: every session of theirs ends.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::UsersSignOut>(),
            route: |v| Ok((Method::POST, path(&["api", "users", &text(v, "user")?, "sign-out"]))),
            run: |cx, v| Box::pin(async move { identity(&cx, Method::POST, &path(&["api", "users", &text(&v, "user")?, "sign-out"]), HashMap::new(), json!({})).await }),
        },
        Op {
            name: "grants.list",
            summary: "Grants: what each person may do and where (`developer@org:acme`, `!runs.cancel@repo:acme/app`); one person's with `user`.",
            read_only: true,
            destructive: false,
            input: Input::of::<input::GrantsList>(),
            route: |_| Ok((Method::GET, path(&["api", "grants"]))),
            run: |cx, v| Box::pin(async move { identity(&cx, Method::GET, &path(&["api", "grants"]), query(&v, &["user"]), Value::Null).await }),
        },
        Op {
            name: "grants.add",
            summary: "Grant a person a role or a capability in a scope (`developer@org:acme`), or deny one (`!runs.cancel@repo:acme/app`).",
            read_only: false,
            destructive: false,
            input: Input::of::<input::GrantsAdd>(),
            route: |_| Ok((Method::POST, path(&["api", "grants"]))),
            run: |cx, v| Box::pin(async move { identity(&cx, Method::POST, &path(&["api", "grants"]), HashMap::new(), only(&v, &["user", "grant"])).await }),
        },
        Op {
            name: "grants.remove",
            summary: "Take a grant away (its id, from grants.list).",
            read_only: false,
            destructive: true,
            input: Input::of::<input::GrantsRemove>(),
            route: |v| Ok((Method::DELETE, path(&["api", "grants", &text(v, "grant")?]))),
            run: |cx, v| Box::pin(async move { identity(&cx, Method::DELETE, &path(&["api", "grants", &text(&v, "grant")?]), HashMap::new(), Value::Null).await }),
        },
        Op {
            name: "roles.list",
            summary: "The roles: kiln's built-in ones and custom ones, each with its capabilities; and every capability there is.",
            read_only: true,
            destructive: false,
            input: Input::of::<input::RolesList>(),
            route: |_| Ok((Method::GET, path(&["api", "roles"]))),
            run: |cx, _| Box::pin(async move { identity(&cx, Method::GET, &path(&["api", "roles"]), HashMap::new(), Value::Null).await }),
        },
        Op {
            name: "roles.set",
            summary: "Make or change a custom role: a name and the capabilities it gives.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::RolesSet>(),
            route: |v| Ok((Method::PUT, path(&["api", "roles", &text(v, "name")?]))),
            run: |cx, v| Box::pin(async move { identity(&cx, Method::PUT, &path(&["api", "roles", &text(&v, "name")?]), HashMap::new(), only(&v, &["capabilities"])).await }),
        },
        Op {
            name: "roles.remove",
            summary: "Remove a custom role.",
            read_only: false,
            destructive: true,
            input: Input::of::<input::RolesRemove>(),
            route: |v| Ok((Method::DELETE, path(&["api", "roles", &text(v, "name")?]))),
            run: |cx, v| Box::pin(async move { identity(&cx, Method::DELETE, &path(&["api", "roles", &text(&v, "name")?]), HashMap::new(), Value::Null).await }),
        },
        Op {
            name: "tokens.list",
            summary: "Tokens: your own, or everyone's with tokens.manage. Each with its kind, whom it acts for, its grants and when it expires; never the token itself.",
            read_only: true,
            destructive: false,
            input: Input::of::<input::TokensList>(),
            route: |_| Ok((Method::GET, path(&["api", "tokens"]))),
            run: |cx, _| Box::pin(async move { identity(&cx, Method::GET, &path(&["api", "tokens"]), HashMap::new(), Value::Null).await }),
        },
        Op {
            name: "tokens.create",
            summary: "Mint a token, shown once: a personal access token (`pat`) or an agent's (`agent`, acting for a person). From a signed-in session, or with tokens.manage (which also mints for someone else).",
            read_only: false,
            destructive: false,
            input: Input::of::<input::TokensCreate>(),
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
            input: Input::of::<input::TokensRevoke>(),
            route: |v| Ok((Method::DELETE, path(&["api", "tokens", &text(v, "token")?]))),
            run: |cx, v| Box::pin(async move { identity(&cx, Method::DELETE, &path(&["api", "tokens", &text(&v, "token")?]), HashMap::new(), Value::Null).await }),
        },
        Op {
            name: "tokens.rotate",
            summary: "Rotate a token: a new one, shown once, with the same grants; the old one stops working.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::TokensRotate>(),
            route: |v| Ok((Method::POST, path(&["api", "tokens", &text(v, "token")?, "rotate"]))),
            run: |cx, v| Box::pin(async move { identity(&cx, Method::POST, &path(&["api", "tokens", &text(&v, "token")?, "rotate"]), HashMap::new(), json!({})).await }),
        },
        Op {
            name: "audit.list",
            summary: "The audit trail, newest first: who did what to what, for whom, with which token, from where, and how it went. Filtered by any of its fields, `q` across them, and a time range.",
            read_only: true,
            destructive: false,
            input: Input::of::<input::AuditList>(),
            route: |_| Ok((Method::GET, path(&["api", "audit"]))),
            run: |cx, v| Box::pin(async move { crate::audit::list(&cx.hub.store, &query(&v, AUDIT)).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))) }),
        },
    ]
}

/// What each of these operations takes (`crate::ops::Input`): its JSON Schema is generated from
/// its type, each field's description from its doc comment.
pub mod input {
    use schemars::JsonSchema;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct UsersList {}

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct UsersAdd {
        /// Up to 64 letters, digits, `-`, `_`, `.` and `@`.
        pub name: String,
        pub grants: Option<Vec<String>>,
        /// The email single sign-on matches them by.
        pub email: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct UsersRemove {
        /// The user: their name or id.
        pub user: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct UsersLink {
        /// The user: their name or id.
        pub user: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct UsersSignOut {
        /// The user: their name or id.
        pub user: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct GrantsList {
        /// The user: their name or id.
        pub user: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct GrantsAdd {
        /// The user: their name or id.
        pub user: String,
        /// `<role or capability>@<scope>`, `!` first to deny it.
        pub grant: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct GrantsRemove {
        /// The grant's id.
        pub grant: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct RolesList {}

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct RolesSet {
        /// Letters, digits, `-` and `_`; not a built-in role's.
        pub name: String,
        pub capabilities: Vec<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct RolesRemove {
        pub name: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct TokensList {}

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    pub enum TokensCreateKind {
        Pat,
        Agent,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct TokensCreate {
        pub kind: Option<TokensCreateKind>,
        /// An agent token's agent.
        pub agent: Option<String>,
        /// Whom it is for, when not you (tokens.manage): a user, or `owner`.
        pub user: Option<String>,
        /// What it may do: `developer@repo:acme/app`, `viewer`, or `owner` for all its person may.
        pub grants: Vec<String>,
        pub note: Option<String>,
        /// A personal access token's life.
        pub expires_in_days: Option<i64>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct TokensRevoke {
        /// The token's id.
        pub token: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct TokensRotate {
        /// The token's id.
        pub token: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AuditList {
        pub principal: Option<String>,
        pub on_behalf_of: Option<String>,
        pub token: Option<String>,
        pub scope: Option<String>,
        pub action: Option<String>,
        pub target: Option<String>,
        pub result: Option<String>,
        pub ip: Option<String>,
        /// Text anywhere in a row.
        pub q: Option<String>,
        /// Unix seconds.
        pub since: Option<i64>,
        pub until: Option<i64>,
        /// Rows before this id: the next page.
        pub before: Option<i64>,
        #[schemars(range(min = 1, max = 1000))]
        pub limit: Option<i64>,
    }
}

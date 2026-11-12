//! The Agentfiles operations (`agentfiles.*`), in kiln's one operation table (`crate::ops::own`): the
//! console's `/api/agentfiles` routes, kiln's MCP tools (`agentfiles_list` ...) and code mode's
//! `kiln.agentfiles.*`. Their routes need `agents.run`; each keeps its caller to their own Agentfiles
//! ([`super::Who`]), and says another's is none.

use http::{Method, StatusCode};
use serde_json::{Value, json};

use super::{Error, Patch, Who};
use crate::ops::{Cx, Fail, Input, Op, Out, path};

fn fail(e: Error) -> Fail {
    match e {
        Error::Bad(m) => (StatusCode::BAD_REQUEST, m),
        Error::Missing => (StatusCode::NOT_FOUND, "no such Agentfile".into()),
        Error::NoRepo(r) => (StatusCode::NOT_FOUND, format!("no repository {r}")),
        Error::Conflict(m) => (StatusCode::CONFLICT, m),
        Error::Refused(m) => (StatusCode::UNPROCESSABLE_ENTITY, format!("Shards refuses this Agentfile: {m}")),
        Error::Forbidden(m) => (StatusCode::FORBIDDEN, m),
        Error::Other(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

fn json_of<T: serde::Serialize>(v: &T) -> Out {
    serde_json::to_value(v).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

fn id_of(v: &Value) -> Result<String, Fail> {
    crate::ops::text(v, "id")
}

/// A string field, as given or left out.
fn some_text(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_owned)
}

/// A flag as JSON gives it, or a query does (`1`, `true`).
fn flag(v: &Value, k: &str) -> bool {
    v.get(k).is_some_and(|b| b.as_bool().unwrap_or_else(|| matches!(b.as_str(), Some("1" | "true"))))
}

fn who(cx: &Cx) -> Result<Who, Fail> {
    Who::of(&cx.caller).map_err(fail)
}

async fn list(cx: Cx, v: Value) -> Out {
    let repo = some_text(&v, "repo");
    json_of(&super::listing(&cx.hub, &cx.caller, flag(&v, "all"), repo.as_deref()).await.map_err(fail)?)
}

async fn get(cx: Cx, v: Value) -> Out {
    json_of(&super::get(&cx.hub, &who(&cx)?, &id_of(&v)?).await.map_err(fail)?)
}

async fn create(cx: Cx, v: Value) -> Out {
    let (name, text) = (crate::ops::text(&v, "name")?, v.get("text").and_then(Value::as_str).unwrap_or_default().to_owned());
    let saved = super::create(&cx.hub, &who(&cx)?, &name, &text, &cx.caller.name).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "agentfile.create".into(), format!("{} ({}) v{}", saved.agentfile.name, saved.agentfile.id, saved.agentfile.version)).await;
    json_of(&saved)
}

async fn update(cx: Cx, v: Value) -> Out {
    let id = id_of(&v)?;
    let patch = Patch { name: some_text(&v, "name"), text: some_text(&v, "text") };
    let saved = super::update(&cx.hub, &who(&cx)?, &id, patch, &cx.caller.name).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "agentfile.update".into(), format!("{} ({}) v{}", saved.agentfile.name, saved.agentfile.id, saved.agentfile.version)).await;
    json_of(&saved)
}

async fn versions(cx: Cx, v: Value) -> Out {
    json_of(&json!({"versions": super::versions(&cx.hub, &who(&cx)?, &id_of(&v)?).await.map_err(fail)?}))
}

async fn restore(cx: Cx, v: Value) -> Out {
    let id = id_of(&v)?;
    let version = v
        .get("version")
        .and_then(|n| n.as_i64().or_else(|| n.as_str().and_then(|s| s.trim().parse().ok())))
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "`version` is the version to make current again".to_owned()))?;
    let saved = super::restore(&cx.hub, &who(&cx)?, &id, version, &cx.caller.name).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "agentfile.restore".into(), format!("{} ({}) v{version} as v{}", saved.agentfile.name, saved.agentfile.id, saved.agentfile.version)).await;
    json_of(&saved)
}

async fn delete(cx: Cx, v: Value) -> Out {
    let gone = super::delete(&cx.hub, &who(&cx)?, &id_of(&v)?).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "agentfile.delete".into(), format!("{} ({}) of {}", gone.name, gone.id, gone.owner)).await;
    Ok(json!({"deleted": true}))
}

/// A text checked as it would be kept, kept nowhere: the editor's lint.
async fn check(cx: Cx, v: Value) -> Out {
    who(&cx)?;
    let text = v.get("text").and_then(Value::as_str).unwrap_or_default();
    if let Some(p) = super::text_problem(text) {
        return Err((StatusCode::BAD_REQUEST, p));
    }
    json_of(&super::check::check(&cx.hub, text).await)
}

/// The operations, in the order they are listed.
pub fn list_ops() -> Vec<Op> {
    vec![
        Op {
            name: "agentfiles.list",
            summary: "Agentfiles (Shards' image of an agent's microVM): yours, newest change first, each without its text (`agentfiles.get` reads one); beside them, read only, those kiln ships per harness (`builtin`) and, with `repo`, that project's `.kiln/images/<name>/Agentfile` files at its default branch (`project`). `all`: everyone's, for the server's owner and those with agentfiles.manage.",
            read_only: true,
            destructive: false,
            input: Input::of::<input::AgentfilesList>(),
            route: |_| Ok((Method::GET, path(&["api", "agentfiles"]))),
            run: |cx, v| Box::pin(list(cx, v)),
        },
        Op {
            name: "agentfiles.get",
            summary: "One of your Agentfiles, with its text and version.",
            read_only: true,
            destructive: false,
            input: Input::of::<input::AgentfilesGet>(),
            route: |v| Ok((Method::GET, path(&["api", "agentfiles", &id_of(v)?]))),
            run: |cx, v| Box::pin(get(cx, v)),
        },
        Op {
            name: "agentfiles.create",
            summary: "Keep an Agentfile of yours: its `name` (an image's: lowercase letters and digits joined by `.`, `_` or `-`; once among yours) and its `text`. Shards checks the text first (its parser, the Agentfile's directives, names and grants, no base pulled): one it refuses isn't kept, and its words say why and where. Answers the Agentfile (version 1) and what Shards said (`check`: its warnings, or `checked: false` and why, where this server has no Shards).",
            read_only: false,
            destructive: false,
            input: Input::of::<input::AgentfilesCreate>(),
            route: |_| Ok((Method::POST, path(&["api", "agentfiles"]))),
            run: |cx, v| Box::pin(create(cx, v)),
        },
        Op {
            name: "agentfiles.update",
            summary: "Change one of your Agentfiles: a new `text` (checked by Shards as agentfiles.create checks it) is a new version; a new `name` alone is none.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::AgentfilesUpdate>(),
            route: |v| Ok((Method::PATCH, path(&["api", "agentfiles", &id_of(v)?]))),
            run: |cx, v| Box::pin(update(cx, v)),
        },
        Op {
            name: "agentfiles.versions",
            summary: "Every version of one of your Agentfiles' text, newest first: when, and by whom.",
            read_only: true,
            destructive: false,
            input: Input::of::<input::AgentfilesVersions>(),
            route: |v| Ok((Method::GET, path(&["api", "agentfiles", &id_of(v)?, "versions"]))),
            run: |cx, v| Box::pin(versions(cx, v)),
        },
        Op {
            name: "agentfiles.restore",
            summary: "Make an earlier version of one of your Agentfiles current again, as a new version (nothing is lost), its text checked by Shards again.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::AgentfilesRestore>(),
            route: |v| Ok((Method::POST, path(&["api", "agentfiles", &id_of(v)?, "restore"]))),
            run: |cx, v| Box::pin(restore(cx, v)),
        },
        Op {
            name: "agentfiles.delete",
            summary: "Delete one of your Agentfiles and its versions.",
            read_only: false,
            destructive: true,
            input: Input::of::<input::AgentfilesDelete>(),
            route: |v| Ok((Method::DELETE, path(&["api", "agentfiles", &id_of(v)?]))),
            run: |cx, v| Box::pin(delete(cx, v)),
        },
        Op {
            name: "agentfiles.check",
            summary: "Check an Agentfile's `text` as agentfiles.create would, keeping nothing: what Shards says (`diagnostics`, each with its rule, words and lines; an `error` one is what would refuse it), or `checked: false` and why.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::AgentfilesCheck>(),
            route: |_| Ok((Method::POST, path(&["api", "agentfiles", "check"]))),
            run: |cx, v| Box::pin(check(cx, v)),
        },
    ]
}

/// What each of these operations takes (`crate::ops::Input`): its JSON Schema is generated from its
/// type, each field's description from its doc comment.
pub mod input {
    use schemars::JsonSchema;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentfilesList {
        /// A project (`owner/name`): its `.kiln/images/<name>/Agentfile` files too.
        pub repo: Option<String>,
        /// Everyone's: the server's owner's, and those with agentfiles.manage.
        pub all: Option<bool>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentfilesGet {
        /// The Agentfile's id.
        pub id: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentfilesCreate {
        /// An image's name: lowercase letters and digits joined by `.`, `_` or `-`, once among yours.
        pub name: String,
        /// The Agentfile: a Dockerfile's instructions, and AGENT, HARNESS, SKILL, MCP, NETWORK, CONNECT,
        /// ATTACH and EXPOSE … FOR.
        pub text: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentfilesUpdate {
        /// The Agentfile's id.
        pub id: String,
        pub name: Option<String>,
        pub text: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentfilesVersions {
        /// The Agentfile's id.
        pub id: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentfilesRestore {
        /// The Agentfile's id.
        pub id: String,
        /// The version whose text becomes current again.
        #[schemars(range(min = 1))]
        pub version: i64,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentfilesDelete {
        /// The Agentfile's id.
        pub id: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentfilesCheck {
        /// The Agentfile's text.
        pub text: String,
    }
}

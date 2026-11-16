//! The Agentfiles operations (`agentfiles.*`), in kiln's one operation table (`crate::ops::own`): the
//! console's `/api/agentfiles` routes, kiln's MCP tools (`agentfiles_list` ...) and code mode's
//! `kiln.agentfiles.*`. Their routes need `agents.run`; each keeps its caller to their own Agentfiles
//! ([`super::Who`]), and says another's is none.

use http::{Method, StatusCode};
use serde_json::{Value, json};

use super::{Error, Patch, Who, settings};
use crate::ops::{Cx, Fail, Input, Op, Out, path};

fn fail(e: Error) -> Fail {
    match e {
        Error::Bad(m) => (StatusCode::BAD_REQUEST, m),
        Error::Missing => (StatusCode::NOT_FOUND, "no such Agentfile".into()),
        Error::NoRepo(r) => (StatusCode::NOT_FOUND, format!("no repository {r}")),
        Error::Conflict(m) => (StatusCode::CONFLICT, m),
        Error::Refused(m) => (StatusCode::UNPROCESSABLE_ENTITY, format!("Shards refuses this Agentfile: {m}")),
        Error::Forbidden(m) => (StatusCode::FORBIDDEN, m),
        Error::Store(m) => (StatusCode::SERVICE_UNAVAILABLE, m),
        e @ Error::Diverged(_) => (StatusCode::CONFLICT, e.to_string()),
        e @ Error::Agent { .. } => (StatusCode::UNPROCESSABLE_ENTITY, e.to_string()),
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

/// Where the texts are: the server's store of bytes.
fn blobs(cx: &Cx) -> &dyn kiln_seams::blobs::Blobs {
    cx.hub.blobs.blobs.as_ref()
}

async fn list(cx: Cx, v: Value) -> Out {
    let repo = some_text(&v, "repo");
    json_of(&super::listing(&cx.hub, &cx.caller, flag(&v, "all"), repo.as_deref(), flag(&v, "lean")).await.map_err(fail)?)
}

async fn get(cx: Cx, v: Value) -> Out {
    json_of(&super::get(&cx.hub, blobs(&cx), &who(&cx)?, &id_of(&v)?).await.map_err(fail)?)
}

async fn create(cx: Cx, v: Value) -> Out {
    let (name, text) = (crate::ops::text(&v, "name")?, v.get("text").and_then(Value::as_str).unwrap_or_default().to_owned());
    let saved = super::create(&cx.hub, blobs(&cx), &who(&cx)?, &name, &text, &cx.caller.name).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "agentfile.create".into(), format!("{} ({}) v{}", saved.agentfile.name, saved.agentfile.id, saved.agentfile.version)).await;
    json_of(&saved)
}

/// A field read as `T` where it's given (null included, as `T` reads it); none where it's left out.
fn field<T: serde::de::DeserializeOwned>(v: &Value, k: &str) -> Result<Option<T>, Fail> {
    v.get(k).map(|f| serde_json::from_value(f.clone()).map_err(|e| (StatusCode::BAD_REQUEST, format!("`{k}`: {e}")))).transpose()
}

async fn update(cx: Cx, v: Value) -> Out {
    let id = id_of(&v)?;
    let patch = Patch { name: some_text(&v, "name"), text: some_text(&v, "text") };
    let base = field::<Option<i64>>(&v, "base")?.flatten();
    let saved = crate::agentdefs::sync::agentfile_update(&cx.hub, blobs(&cx), &who(&cx)?, &id, patch, base, &cx.caller.name).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "agentfile.update".into(), format!("{} ({}) v{}", saved.agentfile.name, saved.agentfile.id, saved.agentfile.version)).await;
    json_of(&saved)
}

async fn versions(cx: Cx, v: Value) -> Out {
    json_of(&json!({"versions": super::versions(&cx.hub, blobs(&cx), &who(&cx)?, &id_of(&v)?).await.map_err(fail)?}))
}

async fn restore(cx: Cx, v: Value) -> Out {
    let id = id_of(&v)?;
    let version = v
        .get("version")
        .and_then(|n| n.as_i64().or_else(|| n.as_str().and_then(|s| s.trim().parse().ok())))
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "`version` is the version to make current again".to_owned()))?;
    let saved = super::restore(&cx.hub, blobs(&cx), &who(&cx)?, &id, version, &cx.caller.name).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "agentfile.restore".into(), format!("{} ({}) v{version} as v{}", saved.agentfile.name, saved.agentfile.id, saved.agentfile.version)).await;
    json_of(&saved)
}

async fn delete(cx: Cx, v: Value) -> Out {
    let gone = super::delete(&cx.hub, blobs(&cx), &who(&cx)?, &id_of(&v)?).await.map_err(fail)?;
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
    json_of(&super::check::check(text, &super::check::Kiln::of(&cx.hub.cfg.server)).await)
}

fn refused(r: settings::Refusal) -> Fail {
    let code = match &r {
        settings::Refusal::Unread(_) => StatusCode::UNPROCESSABLE_ENTITY,
        settings::Refusal::Undeclared { .. } => StatusCode::NOT_FOUND,
        settings::Refusal::Twice { .. } | settings::Refusal::InText(_) => StatusCode::CONFLICT,
        settings::Refusal::Value { .. } => StatusCode::BAD_REQUEST,
    };
    (code, r.to_string())
}

/// A text read as settings, kept nowhere: with `harness`, that harness's changed in it first (kiln plan
/// 01a1dd2e… P5.3).
async fn settings_of(cx: Cx, v: Value) -> Out {
    who(&cx)?;
    let text = v.get("text").and_then(Value::as_str).unwrap_or_default();
    if let Some(p) = super::text_problem(text) {
        return Err((StatusCode::BAD_REQUEST, p));
    }
    let change = settings::HarnessChange {
        source: field(&v, "source")?,
        version: field(&v, "version")?,
        model: field(&v, "model")?,
        sign_in: field(&v, "sign_in")?,
        volumes: field(&v, "volumes")?,
        egress: field(&v, "egress")?,
    };
    let text = match some_text(&v, "harness") {
        Some(h) => settings::set_harness(text, h.trim(), &change).map_err(refused)?,
        None if change == settings::HarnessChange::default() => text.to_owned(),
        None => return Err((StatusCode::BAD_REQUEST, "`harness` names the harness whose settings these are".into())),
    };
    let settings = settings::of(&text).map_err(refused)?;
    let check = super::check::check(&text, &super::check::Kiln::of(&cx.hub.cfg.server)).await;
    json_of(&settings::Viewed { text, settings, check })
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
            summary: "Keep an Agentfile of yours: its `name` (an image's: lowercase letters and digits joined by `.`, `_` or `-`; once among yours) and its `text`. Shards checks the text first (its parser, the Agentfile's directives, names and grants, no base pulled): one it refuses isn't kept, and its words say why and where. Answers the Agentfile (version 1) and what Shards said (`check`: its warnings, in Shards' words).",
            read_only: false,
            destructive: false,
            input: Input::of::<input::AgentfilesCreate>(),
            route: |_| Ok((Method::POST, path(&["api", "agentfiles"]))),
            run: |cx, v| Box::pin(create(cx, v)),
        },
        Op {
            name: "agentfiles.update",
            summary: "Change one of your Agentfiles: a new `text` (checked by Shards as agentfiles.create checks it) is a new version; a new `name` alone is none. `base`, the version your change is made over (its `version` as you read it): a change over a version another has moved on from is refused (409), saying the version it is now. Your agent definitions that name it (`agentfile:<id>`) and that it declares (`AGENT <their name>`) take what the text says of them (model, tools, skills, remote MCP servers), each a new version (`synced`); a setting changed differently on both sides since they last agreed is refused (409), naming it and each side's value.",
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
        Op {
            name: "agentfiles.settings",
            summary: "An Agentfile's `text` as settings, keeping nothing: each harness's (its `source` and the `version` it names, its `model` and `sign_in`, its `volumes` and the `egress` HOST:PORTs it may reach) and each agent's (its model, tools, skills and remote MCP servers), with what Shards says of the text (`check`). With `harness` and what to change of it (null takes a setting out), the text is changed to say it first, and answered: model and sign-in as `LABEL kiln.harness.<harness>.model` and `.sign-in` (a sign-in's name, `<harness>/<name>`, never its value), egress on a network of its own. Save the text with agentfiles.update.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::AgentfilesSettings>(),
            route: |_| Ok((Method::POST, path(&["api", "agentfiles", "settings"]))),
            run: |cx, v| Box::pin(settings_of(cx, v)),
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
        /// Those kiln ships by their names and harnesses alone, their texts left empty: for a list that
        /// shows no more of them (W21-R02: the texts are most of what the list carries).
        pub lean: Option<bool>,
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
        /// The version your change is made over: its `version` as you read it. Refused (409) where it
        /// isn't current, saying the version it is now. Without it, it's made over the version kiln finds.
        #[schemars(range(min = 1))]
        pub base: Option<i64>,
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

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentfilesSettings {
        /// The Agentfile's text.
        pub text: String,
        /// The harness whose settings change (`HARNESS <harness> FROM …`); left out, nothing changes.
        pub harness: Option<String>,
        /// Its source: a path of its build context, a Git or http(s) URL, `s3://BUCKET/KEY`, or an OCI
        /// reference.
        pub source: Option<String>,
        /// What its source names apart from where it is: an OCI tag or digest, a Git ref, an S3 object's
        /// version id. Null: none.
        pub version: Option<String>,
        /// Its model. Null: its own default.
        pub model: Option<String>,
        /// The sign-in it starts as, by its name, which says its harness (`claude/ada`), never its value.
        /// Null: its own, kept in its HOME.
        pub sign_in: Option<String>,
        /// Its volumes' mount points.
        pub volumes: Option<Vec<String>>,
        /// The HOST:PORTs it may reach.
        pub egress: Option<Vec<String>>,
    }
}

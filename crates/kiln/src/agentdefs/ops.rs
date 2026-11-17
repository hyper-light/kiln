//! The agent definitions' operations (`agentdefs.*`, kiln plan 01a1cd8e862e7169a074db3d5fa33f3d P5.1),
//! in kiln's one operation table (`crate::ops::own`): the console's `/api/agentdefs` routes, kiln's
//! MCP tools (`agentdefs_list` ...) and code mode's `kiln.agentdefs.*`. Their routes need
//! `agents.run`; each keeps its caller to their own definitions ([`super::Who`]), and says another's
//! is none. (`agents.*` are the agents kiln runs: a definition's run is one of them.)

use http::{Method, StatusCode};
use serde_json::{Value, json};

use super::{Error, Limits, Mcp, Patch, Settings, Who};
use crate::ops::{Cx, Fail, Input, Op, Out, path};

pub(crate) fn fail(e: Error) -> Fail {
    match e {
        Error::Bad(m) => (StatusCode::BAD_REQUEST, m),
        Error::Missing => (StatusCode::NOT_FOUND, "no such agent definition".into()),
        Error::Conflict(m) => (StatusCode::CONFLICT, m),
        e @ Error::Stale { .. } => (StatusCode::CONFLICT, e.to_string()),
        Error::Refused(why) => (StatusCode::UNPROCESSABLE_ENTITY, format!("kiln can't run this agent: {}", why.join("; "))),
        Error::Forbidden(m) => (StatusCode::FORBIDDEN, m),
        e @ Error::ReadOnly(_) => (StatusCode::FORBIDDEN, e.to_string()),
        Error::Unavailable(m) => (StatusCode::SERVICE_UNAVAILABLE, m),
        e @ Error::Diverged(_) => (StatusCode::CONFLICT, e.to_string()),
        Error::Other(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

fn json_of<T: serde::Serialize>(v: &T) -> Out {
    serde_json::to_value(v).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

fn id_of(v: &Value) -> Result<String, Fail> {
    crate::ops::text(v, "id")
}

fn who(cx: &Cx) -> Result<Who, Fail> {
    Who::of(&cx.caller).map_err(fail)
}

/// A field read as `T` where it's given (null included, as `T` reads it); none where it's left out.
fn field<T: serde::de::DeserializeOwned>(v: &Value, k: &str) -> Result<Option<T>, Fail> {
    v.get(k).map(|f| serde_json::from_value(f.clone()).map_err(|e| (StatusCode::BAD_REQUEST, format!("`{k}`: {e}")))).transpose()
}

/// The settings an input gives, each left out its default.
fn settings_of(v: &Value) -> Result<Settings, Fail> {
    Ok(Settings {
        harness: crate::ops::text(v, "harness")?,
        model: field::<Option<String>>(v, "model")?.flatten(),
        instructions: field(v, "instructions")?.unwrap_or_default(),
        tools: field(v, "tools")?.unwrap_or_default(),
        mcp: field(v, "mcp")?.unwrap_or_default(),
        skills: field(v, "skills")?.unwrap_or_default(),
        limits: field(v, "limits")?.unwrap_or_default(),
        agentfile: field::<Option<String>>(v, "agentfile")?.flatten(),
        volumes: field(v, "volumes")?.unwrap_or_default(),
        engine: field::<Option<crate::harness::engine::Engine>>(v, "engine")?.flatten(),
        cpus: field::<Option<u32>>(v, "cpus")?.flatten(),
        memory: field::<Option<u64>>(v, "memory")?.flatten(),
    })
}

/// A flag as JSON gives it, or a query does (`1`, `true`).
pub(crate) fn flag(v: &Value, k: &str) -> bool {
    v.get(k).is_some_and(|b| b.as_bool().unwrap_or_else(|| matches!(b.as_str(), Some("1" | "true"))))
}

async fn list(cx: Cx, v: Value) -> Out {
    let w = who(&cx)?;
    let defs = super::list(&cx.hub, &w, flag(&v, "all")).await.map_err(fail)?;
    let uses = super::uses::of(&cx.hub, &w.person).await.map_err(fail)?;
    Ok(json!({"agents": json_of(&defs)?, "reaches_all": w.all, "builtin": json_of(&super::builtin::all())?, "uses": uses}))
}

async fn get(cx: Cx, v: Value) -> Out {
    let (w, id) = (who(&cx)?, id_of(&v)?);
    match super::builtin::get(&id) {
        Some(b) => json_of(&b),
        None => json_of(&super::get(&cx.hub, &w, &id).await.map_err(fail)?),
    }
}

/// One of the caller's own standing in for one of kiln's UI agents for them, or kiln's own again.
async fn use_(cx: Cx, v: Value) -> Out {
    let kiln_agent = crate::ops::text(&v, "builtin")?;
    let id = field::<Option<String>>(&v, "id")?.flatten();
    let uses = super::uses::set(&cx.hub, &who(&cx)?, &kiln_agent, id.as_deref()).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "agentdef.use".into(), format!("{kiln_agent} as {}", id.as_deref().unwrap_or("kiln's own"))).await;
    Ok(json!({"uses": uses}))
}

/// One of the caller's, or kiln's own, made one of theirs.
async fn copy(cx: Cx, v: Value) -> Out {
    let name = v.get("name").and_then(Value::as_str);
    let made = super::copy(&cx.hub, &who(&cx)?, &id_of(&v)?, name, &cx.caller.name).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "agentdef.copy".into(), audit_line(&made)).await;
    json_of(&made)
}

fn audit_line(d: &super::AgentDef) -> String {
    format!("{} ({}) v{}", d.name, d.id, d.version)
}

/// Where `d` names one of its person's Agentfiles, that Agentfile's builds ahead of its sessions asked anew, its harness
/// one they're built for (kiln plan 01a1dd2e… P4.5). An experiment's promotion saves one too (`super::experiments::ops`).
pub(crate) async fn prebuild(cx: &Cx, d: &super::AgentDef) {
    if let Some(id) = d.settings.agentfile.as_deref().map(str::trim).and_then(|a| a.strip_prefix(crate::sandbox::isolated::KEPT)) {
        crate::agentfiles::prebuild::queue(&cx.hub, id).await;
    }
}

async fn create(cx: Cx, v: Value) -> Out {
    let (name, settings) = (crate::ops::text(&v, "name")?, settings_of(&v)?);
    let made = super::sync::create(&cx.hub, &who(&cx)?, &name, settings, &cx.caller.name).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "agentdef.create".into(), audit_line(&made)).await;
    prebuild(&cx, &made).await;
    json_of(&made)
}

async fn update(cx: Cx, v: Value) -> Out {
    let id = id_of(&v)?;
    let patch = Patch {
        name: field(&v, "name")?,
        harness: field(&v, "harness")?,
        model: field(&v, "model")?,
        instructions: field(&v, "instructions")?,
        tools: field(&v, "tools")?,
        mcp: field(&v, "mcp")?,
        skills: field(&v, "skills")?,
        limits: field(&v, "limits")?,
        agentfile: field(&v, "agentfile")?,
        volumes: field(&v, "volumes")?,
        engine: field(&v, "engine")?,
        cpus: field(&v, "cpus")?,
        memory: field(&v, "memory")?,
    };
    let base = field::<Option<i64>>(&v, "base")?.flatten();
    let saved = super::sync::update(&cx.hub, &who(&cx)?, &id, patch, base, &cx.caller.name).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "agentdef.update".into(), audit_line(&saved)).await;
    prebuild(&cx, &saved).await;
    json_of(&saved)
}

async fn versions(cx: Cx, v: Value) -> Out {
    Ok(json!({"versions": json_of(&super::versions(&cx.hub, &who(&cx)?, &id_of(&v)?).await.map_err(fail)?)?}))
}

async fn restore(cx: Cx, v: Value) -> Out {
    let id = id_of(&v)?;
    let version = v
        .get("version")
        .and_then(|n| n.as_i64().or_else(|| n.as_str().and_then(|s| s.trim().parse().ok())))
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "`version` is the version to make current again".to_owned()))?;
    let saved = super::restore(&cx.hub, &who(&cx)?, &id, version, &cx.caller.name).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "agentdef.restore".into(), format!("{} ({}) v{version} as v{}", saved.name, saved.id, saved.version)).await;
    prebuild(&cx, &saved).await;
    json_of(&saved)
}

async fn delete(cx: Cx, v: Value) -> Out {
    let gone = super::delete(&cx.hub, &who(&cx)?, &id_of(&v)?).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "agentdef.delete".into(), format!("{} ({}) of {}", gone.name, gone.id, gone.owner)).await;
    Ok(json!({"deleted": true}))
}

/// Settings checked as agentdefs.create would, kept nowhere: every reason kiln can't run them.
async fn check(cx: Cx, v: Value) -> Out {
    let w = who(&cx)?;
    let why = super::check(&cx.hub, &w.person, &settings_of(&v)?).await.map_err(fail)?;
    Ok(json!({"ok": why.is_empty(), "problems": why}))
}

/// A definition run: an agent started as `agents.start` starts one, through the one start path (its
/// route needs what `agents.start`'s does, `agents.run`; the caller's right to start agents on that
/// machine is checked there, and the start audited), with the definition's harness, model and
/// Agentfile, in a microVM of its own (sized as it says, where it says), or on its machine's host where
/// its engine is `host`, first told its instructions and the task. Each session it opens says it started
/// from this definition at this version (`super::sessions`), a start that waits for room too; while
/// one of its experiments is live, each is that experiment's draw (`super::experiments::assign`, from
/// `harness::lineage::start`).
async fn run(cx: Cx, v: Value) -> Out {
    let id = id_of(&v)?;
    let (name, version, d) = super::settings_of(&cx.hub, &who(&cx)?, &id).await.map_err(fail)?;
    let task = v.get("prompt").and_then(Value::as_str);
    let host = d.engine == Some(crate::harness::engine::Engine::Host);
    // Its task apart, for a split's variant that's another agent to retell (P7.1); its definition is kiln's to say
    // (`Ask::definition`, below), which a split matches.
    let mut ask = json!({"kind": d.harness, "isolation": if host { "none" } else { "microvm" }, "name": v.get("name").and_then(Value::as_str).unwrap_or(&name)});
    if let (Some(o), Some(t)) = (ask.as_object_mut(), task) {
        o.insert("task".into(), json!(t));
    }
    if let Some(o) = ask.as_object_mut() {
        let given = [("model", d.model.clone()), ("image", d.agentfile.clone()), ("prompt", super::first_message(&d, task))]
            .into_iter()
            .chain(["on", "cwd"].into_iter().map(|k| (k, v.get(k).and_then(Value::as_str).map(str::to_owned))));
        for (k, val) in given {
            if let Some(val) = val {
                o.insert(k.into(), json!(val));
            }
        }
    }
    let mut ask: crate::harness::lineage::Ask = serde_json::from_value(ask).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("a definition's start couldn't be made: {e}")))?;
    ask.definition = Some(super::sessions::Definition { id: id.clone(), version });
    ask.size = super::size_of(&d);
    let mut started = crate::harness::ops::start_ask(cx, ask).await?;
    if let Some(o) = started.as_object_mut() {
        o.insert("agentdef".into(), json!({"id": id, "version": version}));
    }
    Ok(started)
}

/// The sessions started from one of the caller's definitions, or from one of kiln's own as they ran it.
async fn sessions(cx: Cx, v: Value) -> Out {
    let who = who(&cx)?;
    match v.get("id").filter(|id| !id.is_null()) {
        Some(_) => super::sessions::list(&cx.hub, &who, &id_of(&v)?).await,
        None => super::sessions::all(&cx.hub, &who).await,
    }
    .map_err(fail)
}

/// The operations, in the order they are listed: their experiments' last (`super::experiments::ops`).
pub fn list_ops() -> Vec<Op> {
    let mut ops = vec![
        Op {
            name: "agentdefs.list",
            summary: "Agent definitions (an agent as data: one of kiln's ten harnesses, its model, instructions, tools and MCP servers, skills, limits and Agentfile): yours, newest change first, and beside them, read only, kiln's own (`builtin`: one per CI skill, one per harness, and the agents kiln starts from its UI, such as the about card; agentdefs.copy makes one yours). `all`: everyone's, for the server's owner and those with agentdefs.manage.",
            read_only: true,
            destructive: false,
            input: Input::of::<input::AgentdefsList>(),
            route: |_| Ok((Method::GET, path(&["api", "agentdefs"]))),
            run: |cx, v| Box::pin(list(cx, v)),
        },
        Op {
            name: "agentdefs.get",
            summary: "One of your agent definitions, with its settings and version.",
            read_only: true,
            destructive: false,
            input: Input::of::<input::AgentdefsGet>(),
            route: |v| Ok((Method::GET, path(&["api", "agentdefs", &id_of(v)?]))),
            run: |cx, v| Box::pin(get(cx, v)),
        },
        Op {
            name: "agentdefs.create",
            summary: "Keep an agent definition of yours: its `name` (lowercase letters and digits joined by `.`, `_` or `-`; once among yours), `harness` (claude, codex, devin, gemini, opencode, goose, amp, antigravity, cursor or pi), `model`, `instructions`, `tools`, `mcp` servers, `skills`, `limits`, `agentfile` (one of yours, `agentfile:<id>`, or kiln's for its harness, `harness-<harness>`; left out: its harness's own) and `volumes` (what it keeps from one of its sessions to the next on each machine: each a `name` its harness sees at `/volumes/<name>` and the most `bytes` it may hold, asked of the machine's disk as each session starts), and where its sessions run: `engine` (`microvm`, the default, or `host`, its harness a process of its machine's, with no Agentfile, size or volumes) and its microVM's `cpus` and `memory` (bytes, whole MiB; left out, kiln sizes it). Checked first as agentdefs.check checks it: one kiln can't run isn't kept, and its words say why. Answers it, version 1.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::AgentdefsCreate>(),
            route: |_| Ok((Method::POST, path(&["api", "agentdefs"]))),
            run: |cx, v| Box::pin(create(cx, v)),
        },
        Op {
            name: "agentdefs.update",
            summary: "Change one of your agent definitions: what's given changes (null `model` or `agentfile`: back to its harness's). New settings, checked as agentdefs.create checks them, are a new version; a new `name` alone is none. `base`, the version your change is made over (its `version` as you read it; over HTTP, `If-Match: \"<version>\"`): a change over a version another has moved on from is refused (409), saying the version it is now, and its route's answer carries that version whole (`current`), so nothing is overwritten unseen.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::AgentdefsUpdate>(),
            route: |v| Ok((Method::PATCH, path(&["api", "agentdefs", &id_of(v)?]))),
            run: |cx, v| Box::pin(update(cx, v)),
        },
        Op {
            name: "agentdefs.versions",
            summary: "Every version of one of your agent definitions' settings, newest first: when, and by whom.",
            read_only: true,
            destructive: false,
            input: Input::of::<input::AgentdefsVersions>(),
            route: |v| Ok((Method::GET, path(&["api", "agentdefs", &id_of(v)?, "versions"]))),
            run: |cx, v| Box::pin(versions(cx, v)),
        },
        Op {
            name: "agentdefs.restore",
            summary: "Make an earlier version of one of your agent definitions current again, as a new version (nothing is lost), checked again.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::AgentdefsRestore>(),
            route: |v| Ok((Method::POST, path(&["api", "agentdefs", &id_of(v)?, "restore"]))),
            run: |cx, v| Box::pin(restore(cx, v)),
        },
        Op {
            name: "agentdefs.delete",
            summary: "Delete one of your agent definitions and its versions.",
            read_only: false,
            destructive: true,
            input: Input::of::<input::AgentdefsDelete>(),
            route: |v| Ok((Method::DELETE, path(&["api", "agentdefs", &id_of(v)?]))),
            run: |cx, v| Box::pin(delete(cx, v)),
        },
        Op {
            name: "agentdefs.copy",
            summary: "Make one of kiln's own agent definitions (`kiln:<name>`, as agentdefs.list's `builtin` names them: one per CI skill, one per harness, and kiln's own agents such as the about card), or one of yours, one of yours: version 1, named `name` (by default as it is), checked as agentdefs.create checks it. Kiln's own are read only: copy one to change it.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::AgentdefsCopy>(),
            route: |v| Ok((Method::POST, path(&["api", "agentdefs", &id_of(v)?, "copy"]))),
            run: |cx, v| Box::pin(copy(cx, v)),
        },
        Op {
            name: "agentdefs.use",
            summary: "Have one of your own agent definitions (`id`) stand in for one of kiln's UI agents (`builtin`: one agentdefs.list marks `replaceable`, such as kiln:kiln-about, the card that answers \"ask about this code\") where kiln starts it for you: its harness, model and Agentfile, first told its instructions and then kiln's brief. Only your own use changes. Null `id`: kiln's own again. Answers what stands in for what (`uses`), as agentdefs.list does.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::AgentdefsUse>(),
            route: |_| Ok((Method::POST, path(&["api", "agentdefs", "use"]))),
            run: |cx, v| Box::pin(use_(cx, v)),
        },
        Op {
            name: "agentdefs.check",
            summary: "Check an agent definition's settings as agentdefs.create would, keeping nothing: `ok`, and each of the `problems` that would refuse it (a harness that isn't one of kiln's ten, an Agentfile you don't reach or that's for another harness, a model kiln's machines don't list for its harness, an MCP server without a name or a way in, a limit of 0, a volume without a name of its own or a bound).",
            read_only: false,
            destructive: false,
            input: Input::of::<input::AgentdefsCheck>(),
            route: |_| Ok((Method::POST, path(&["api", "agentdefs", "check"]))),
            run: |cx, v| Box::pin(check(cx, v)),
        },
        Op {
            name: "agentdefs.run",
            summary: "Run one of your agent definitions: an agent started as agents.start starts one, with its harness and model, in a microVM of its own made from its Agentfile (its harness's own where it names none) and sized as it says (or on its machine's host, its `engine` `host`), first told its instructions and then your `prompt`, its volumes plugged in as it starts (yours alone, kept on that machine for its next session there); `on` and `cwd` as agents.start takes them, `name` its name (by default the definition's). While one of its experiments is live, each session it starts is that experiment's draw: one of its variants, by their weights (agentdefs.experiments.*). Answers as agents.start does, with the definition's id and version (`agentdef`).",
            read_only: false,
            destructive: false,
            input: Input::of::<input::AgentdefsRun>(),
            route: |v| Ok((Method::POST, path(&["api", "agentdefs", &id_of(v)?, "run"]))),
            run: |cx, v| Box::pin(run(cx, v)),
        },
        Op {
            name: "agentdefs.sessions",
            summary: "The sessions started from one of your agent definitions (or from one of kiln's own, `kiln:<name>`, as you started them): newest first, each with the definition's version it started from (`version`; behind the definition where less than its `version` now), how it stands (`state`, as agents.list's `stand`; `gone` once kiln no longer has it), its `runner` and `engine` (host or microvm), its brief (`agent`), and the definition's experiment that drew it and the variant it got (`experiment`, `variant`), where one did. With what their starts took (`starts`): how many kiln recorded (`n`), and by how each started (`by_kind`: its engine), how many, how many were timed (a session's first start: from its open on its machine to its harness running) and their median (`p50_ms`); `warm`, the warm hosts its runners' nodes keep for it, where they say (none does yet). Without `id`: every definition's whose sessions you see, by its id, each answered so (one with none isn't among them).",
            read_only: true,
            destructive: false,
            input: Input::of::<input::AgentdefsSessions>(),
            route: |v| match v.get("id").filter(|id| !id.is_null()) {
                Some(_) => Ok((Method::GET, path(&["api", "agentdefs", &id_of(v)?, "sessions"]))),
                None => Ok((Method::GET, path(&["api", "agentdef-sessions"]))),
            },
            run: |cx, v| Box::pin(sessions(cx, v)),
        },
    ];
    ops.extend(super::experiments::ops::list_ops());
    ops
}

/// What each of these operations takes (`crate::ops::Input`): its JSON Schema is generated from its
/// type, each field's description from its doc comment.
pub mod input {
    use schemars::JsonSchema;
    use serde::Deserialize;

    use super::{Limits, Mcp};
    use crate::agentdefs::volumes::Volume;

    /// Where an agent's sessions run.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(rename_all = "lowercase")]
    pub enum Engine {
        /// Its harness a process of its machine's.
        Host,
        /// A microVM of its own, made from its Agentfile.
        Microvm,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentdefsList {
        /// Everyone's: the server's owner's, and those with agentdefs.manage.
        pub all: Option<bool>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentdefsGet {
        /// The agent definition's id.
        pub id: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentdefsCreate {
        /// Lowercase letters and digits joined by `.`, `_` or `-`, once among yours.
        pub name: String,
        /// One of kiln's harnesses: claude, codex, devin, gemini, opencode, goose, amp, antigravity,
        /// cursor or pi.
        pub harness: String,
        /// Its model; left out: its harness's default.
        pub model: Option<String>,
        /// Its instructions and playbook: the first of what a run of it is told.
        pub instructions: Option<String>,
        pub tools: Option<Vec<String>>,
        pub mcp: Option<Vec<Mcp>>,
        pub skills: Option<Vec<String>>,
        pub limits: Option<Limits>,
        /// One of your Agentfiles (`agentfile:<id>`) or kiln's for its harness (`harness-<harness>`);
        /// left out: its harness's own.
        pub agentfile: Option<String>,
        /// What it keeps from one of its sessions to the next on each machine: each a `name` (letters, digits,
        /// `_`, `.` and `-`; its harness sees it at `/volumes/<name>`) and the most `bytes` it may hold.
        pub volumes: Option<Vec<Volume>>,
        /// Where its sessions run: `microvm` (left out, the same: a microVM of its own, made from its
        /// Agentfile) or `host` (its harness a process of its machine's, with no Agentfile, size or volumes).
        pub engine: Option<Engine>,
        /// Its microVM's vCPUs; left out: as kiln sizes it.
        #[schemars(range(min = 1))]
        pub cpus: Option<u32>,
        /// Its microVM's memory, in bytes, a whole number of MiB; left out: as kiln sizes it.
        #[schemars(range(min = 1))]
        pub memory: Option<u64>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentdefsUpdate {
        /// The agent definition's id.
        pub id: String,
        pub name: Option<String>,
        pub harness: Option<String>,
        /// Null: back to its harness's default.
        pub model: Option<String>,
        pub instructions: Option<String>,
        pub tools: Option<Vec<String>>,
        pub mcp: Option<Vec<Mcp>>,
        pub skills: Option<Vec<String>>,
        pub limits: Option<Limits>,
        /// Null: back to its harness's own.
        pub agentfile: Option<String>,
        /// Its volumes, all of them: `[]` for none.
        pub volumes: Option<Vec<Volume>>,
        /// `microvm` or `host`; null: back to a microVM of its own.
        pub engine: Option<Engine>,
        /// Null: back to kiln's sizing.
        #[schemars(range(min = 1))]
        pub cpus: Option<u32>,
        /// In bytes, a whole number of MiB; null: back to kiln's sizing.
        #[schemars(range(min = 1))]
        pub memory: Option<u64>,
        /// The version your change is made over: its `version` as you read it. Refused (409) where it
        /// isn't current, saying the version it is now. Without it, it's made over the version kiln finds.
        #[schemars(range(min = 1))]
        pub base: Option<i64>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentdefsVersions {
        /// The agent definition's id.
        pub id: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentdefsRestore {
        /// The agent definition's id.
        pub id: String,
        /// The version whose settings become current again.
        #[schemars(range(min = 1))]
        pub version: i64,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentdefsDelete {
        /// The agent definition's id.
        pub id: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentdefsUse {
        /// One of kiln's UI agents (`kiln:<name>`) agentdefs.list marks `replaceable`.
        pub builtin: String,
        /// One of your own agent definitions; null: kiln's own again.
        pub id: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentdefsCopy {
        /// One of kiln's own (`kiln:<name>`) or one of yours.
        pub id: String,
        /// The copy's name; by default the one it has.
        pub name: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentdefsCheck {
        pub harness: String,
        pub model: Option<String>,
        pub instructions: Option<String>,
        pub tools: Option<Vec<String>>,
        pub mcp: Option<Vec<Mcp>>,
        pub skills: Option<Vec<String>>,
        pub limits: Option<Limits>,
        pub agentfile: Option<String>,
        pub volumes: Option<Vec<Volume>>,
        pub engine: Option<Engine>,
        pub cpus: Option<u32>,
        pub memory: Option<u64>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentdefsSessions {
        /// The agent definition's id: one of yours, or one of kiln's own (`kiln:<name>`). Left out:
        /// every definition's whose sessions you see, by its id.
        pub id: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentdefsRun {
        /// The agent definition's id: one of yours, or one of kiln's own (`kiln:<name>`).
        pub id: String,
        /// The task: told after its instructions.
        pub prompt: Option<String>,
        /// What it needs, or a machine's name, as agents.start takes `on`.
        pub on: Option<String>,
        /// Its folder there.
        pub cwd: Option<String>,
        /// Its name; by default the definition's.
        pub name: Option<String>,
    }
}

//! The environments' operations (`agentenvs.*`, docs/agents.md "Operations"), in kiln's one operation
//! table (`crate::ops::own`): the console's `/api/agentenvs` routes and `PUT` and `GET
//! /api/harness/sessions/:id/agentenv`, kiln's MCP tools and code mode's `kiln.agentenvs.*`. Their routes
//! need `agents.run`; each keeps its caller to their own ([`super::person`]), and says another's is none.
//! A refusal's words start with its code where docs/agents.md names one ([`super::Refusal::said`]).

use http::{Method, StatusCode};
use serde_json::{Value, json};

use super::scope::Chip;
use super::{Applied, Draft, Link, Listing, Patch, Refusal, SessionEnvs, resolve};
use crate::harness::agentenv::{Base, Var};
use crate::ops::{Cx, Fail, Input, Op, Out, path, text};

fn fail(e: Refusal) -> Fail {
    if let Refusal::Other(err) = &e {
        tracing::warn!(target: "kiln.agentenvs", error = %format!("{err:#}"), "an environment's operation failed");
    }
    e.fail()
}

fn json_of<T: serde::Serialize>(v: &T) -> Out {
    serde_json::to_value(v).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

fn person(cx: &Cx) -> Result<String, Fail> {
    super::person(&cx.caller).map_err(fail)
}

/// A field read as `T` where it's given; none where it's left out.
fn field<T: serde::de::DeserializeOwned>(v: &Value, k: &str) -> Result<Option<T>, Fail> {
    v.get(k).map(|f| serde_json::from_value(f.clone()).map_err(|e| Refusal::BadVar(format!("`{k}`: {e}")).fail())).transpose()
}

async fn list(cx: Cx, _v: Value) -> Out {
    let p = person(&cx)?;
    json_of(&Listing { agentenvs: super::list(&cx.hub, &p).await.map_err(fail)? })
}

async fn get(cx: Cx, v: Value) -> Out {
    let p = person(&cx)?;
    json_of(&super::get(&cx.hub, &p, &text(&v, "id")?).await.map_err(fail)?)
}

async fn create(cx: Cx, v: Value) -> Out {
    let p = person(&cx)?;
    let draft = Draft {
        name: text(&v, "name")?,
        description: field::<Option<String>>(&v, "description")?.flatten(),
        base: field::<Base>(&v, "base")?.unwrap_or_default(),
        vars: field::<Vec<Var>>(&v, "vars")?.unwrap_or_default(),
        tags: field::<Vec<String>>(&v, "tags")?.unwrap_or_default(),
        scope: field::<Vec<Chip>>(&v, "scope")?.unwrap_or_default(),
        auto: field::<bool>(&v, "auto")?.unwrap_or(false),
    };
    let made = super::create(&cx.hub, &p, draft).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "agentenv.create".into(), format!("{} ({}) v{}", made.name, made.id, made.version)).await;
    json_of(&made)
}

async fn update(cx: Cx, v: Value) -> Out {
    let p = person(&cx)?;
    let id = text(&v, "id")?;
    let version = v.get("version").and_then(Value::as_i64).ok_or_else(|| (StatusCode::BAD_REQUEST, "`version` is the version your change is made over: its `version` as you read it".to_owned()))?;
    let patch = Patch {
        name: field(&v, "name")?,
        description: field(&v, "description")?,
        base: field(&v, "base")?,
        vars: field(&v, "vars")?,
        tags: field(&v, "tags")?,
        scope: field(&v, "scope")?,
        auto: field(&v, "auto")?,
    };
    let changed = super::update(&cx.hub, &p, &id, version, patch).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "agentenv.update".into(), format!("{} ({}) v{}", changed.name, changed.id, changed.version)).await;
    json_of(&changed)
}

async fn delete(cx: Cx, v: Value) -> Out {
    let p = person(&cx)?;
    let gone = super::delete(&cx.hub, &p, &text(&v, "id")?).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "agentenv.delete".into(), format!("{} ({}) of {}", gone.name, gone.id, gone.owner)).await;
    Ok(json!({"deleted": true}))
}

/// The variables a machine's user has, by name and where each is set (`ServerMsg::UserEnv`), for a caller
/// who may start agents there: the server's own (`local`) read here.
async fn machine(cx: Cx, v: Value) -> Out {
    person(&cx)?;
    let asked = text(&v, "target")?;
    let target = match asked.strip_prefix("agent:").unwrap_or(&asked) {
        "local" => "local".to_owned(),
        m => format!("agent:{}", cx.hub.machine_named(m).unwrap_or_else(|| m.to_owned())),
    };
    crate::harness::lineage::may_run_on(&cx.caller, &target, "read what its user's variables are")?;
    let Some(machine) = target.strip_prefix("agent:") else {
        return json_of(&crate::harness::agentenv::answer(crate::harness::agentenv::Ask::default()).await);
    };
    let ask = serde_json::to_string(&crate::harness::agentenv::Ask::default()).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let answered = cx
        .hub
        .ask_speaking::<crate::harness::agentenv::Answer>(machine, crate::proto::feature::AGENTENV, crate::hub::COMMAND_WITHIN, move |req| crate::proto::ServerMsg::UserEnv { req, json: ask })
        .await;
    match answered {
        Ok(Some(Some(a))) => json_of(&a),
        Ok(Some(None)) => Err((StatusCode::BAD_GATEWAY, format!("{} answered nothing", cx.hub.shown(machine)))),
        Ok(None) => Err((StatusCode::CONFLICT, format!("{}'s kiln doesn't say what its user's variables are: upgrade it (System → Updates)", cx.hub.shown(machine)))),
        Err(e) => Err((StatusCode::BAD_GATEWAY, format!("{e:#}"))),
    }
}

/// The environments an agent the caller may steer is given by name, top first, its harness started again on its
/// conversation unless `restart` is false: kept as the stack it runs with once its runner took it, else as it was. Each
/// is one of the caller's, or one its stack holds already (another's an agent of theirs started it with); each once.
/// Under them, its person's applied on their own whose scope matches it now ([`super::autos`]): one listed is given by
/// name from now on, at its place; `[]` leaves those alone over its machine's user's environment.
async fn apply(cx: Cx, v: Value) -> Out {
    let p = person(&cx)?;
    let info = crate::harness::ops::known(&cx, &text(&v, "agent")?)?;
    crate::harness::lineage::may_steer(&cx.hub, &cx.caller, &info.id).await?;
    let asked: Vec<String> = field::<Vec<String>>(&v, "agentenvs")?
        .ok_or_else(|| Refusal::BadVar("`agentenvs` is the session's whole stack, top first, by id: [] for its machine's user's environment".into()).fail())?
        .into_iter()
        .map(|s| s.trim().to_owned())
        .collect();
    if let Some(i) = asked.iter().position(String::is_empty) {
        return Err(Refusal::BadVar(format!("`agentenvs`'s entry {i} names no environment")).fail());
    }
    if let Some(twice) = asked.iter().enumerate().find_map(|(i, a)| asked.iter().skip(i.saturating_add(1)).any(|b| b == a).then_some(a)) {
        return Err(Refusal::BadVar(format!("`agentenvs` names {twice} twice: a stack holds each once")).fail());
    }
    let restart = field::<bool>(&v, "restart")?.unwrap_or(true);
    let was = super::stack(&cx.hub, &info.id);
    let mut envs = Vec::with_capacity(asked.len());
    for id in &asked {
        let env = match super::get(&cx.hub, &p, id).await {
            Ok(e) => e,
            Err(Refusal::Unknown(_)) if was.iter().any(|l| l.id == *id) => match super::any(&cx.hub, id).await.map_err(fail)? {
                Some(e) => e,
                None => return Err(super::unknown(id).fail()),
            },
            Err(e) => return Err(fail(e)),
        };
        envs.push(env);
    }
    let target = if info.target.is_empty() { "local".to_owned() } else { info.target.clone() };
    let microvm = info.isolation.is_some();
    // Those applied on their own that match it now, under the ones named (none on a runner that runs no environment).
    let speaks = target.strip_prefix("agent:").is_none_or(|m| resolve::speaks(&cx.hub, m));
    let autos = match crate::mcp::whose(&cx.hub, &info.id).await.filter(|_| speaks) {
        Some(owner) => {
            let facts = super::scope::of_session(&cx.hub, &info).await.map_err(|e| fail(e.into()))?;
            super::autos(&cx.hub, &owner, &facts, microvm, &asked).await.map_err(fail)?
        }
        None => Vec::new(),
    };
    let links: Vec<Link> = asked.iter().map(|id| Link::named(id)).chain(autos.iter().map(|e| Link { id: e.id.clone(), auto: true })).collect();
    envs.extend(autos);
    if envs.is_empty()
        && let Some(m) = target.strip_prefix("agent:").filter(|m| !resolve::speaks(&cx.hub, m))
    {
        return Err(Refusal::Unsupported(format!("{}'s kiln runs no session with an environment: upgrade it (System → Updates)", cx.hub.shown(m))).fail());
    }
    for e in &envs {
        resolve::runs_there(&cx.hub, e, &target, microvm).map_err(fail)?;
    }
    let resolved = resolve::open_stack(&cx.hub, &envs, microvm, &cx.caller.name, &format!("agent session {}, applied", info.id)).await.map_err(fail)?;
    // Kept before it's sent, so the session's runner announcing it with this one finds it kept
    // (`resolve::opened`); put back as it was where its runner didn't take it.
    super::set_stack(&cx.hub, &info.id, &links, &cx.caller.name).await.map_err(fail)?;
    resolve::given(&cx.hub, &info.id, &target);
    let named = resolved.as_ref().map_or_else(|| "its machine's user's".to_owned(), |r| format!("{} ({}) v{}", r.name, r.id, r.version));
    let sent = cx.hub.harness.send_agentenv(&cx.hub, &info.id, resolved, restart).await;
    if let Some(why) = sent.failure() {
        if let Err(e) = super::set_stack(&cx.hub, &info.id, &was, &cx.caller.name).await {
            tracing::warn!(target: "kiln.agentenvs", session = %info.id, error = %e, "a session's environments couldn't be put back after its runner didn't take others");
        }
        return Err(why);
    }
    cx.hub.store.audit(cx.caller.name.clone(), "agentenv.apply".into(), format!("{named} to {}{}", info.id, if restart { ", started again" } else { "" })).await;
    let stack = super::entries(&cx.hub, &links).await.map_err(fail)?;
    json_of(&Applied { agent: info.id, stack, restarted: restart })
}

/// A session the caller sees as scopes see it: what it is now ([`super::scope::of_session`]), its stack, top first, and
/// which of the caller's environments' scopes match it.
async fn session(cx: Cx, v: Value) -> Out {
    let p = person(&cx)?;
    let info = crate::harness::ops::known(&cx, &text(&v, "agent")?)?;
    crate::harness::lineage::may_see(&cx.hub, &cx.caller, &info.id).await?;
    let facts = super::scope::of_session(&cx.hub, &info).await.map_err(|e| fail(e.into()))?;
    let stack = super::entries(&cx.hub, &super::stack(&cx.hub, &info.id)).await.map_err(fail)?;
    let matching = super::list(&cx.hub, &p).await.map_err(fail)?.into_iter().filter(|e| super::scope::matches(&e.scope, &facts)).map(|e| e.id).collect();
    json_of(&SessionEnvs { facts, stack, matching })
}

/// The operations, in the order they are listed.
pub fn list_ops() -> Vec<Op> {
    vec![
        Op {
            name: "agentenvs.list",
            summary: "Your environments for agent sessions (docs/agents.md, \"Environments\"): named sets of variables, each picked from the machine's user's (`machine`), set (`value`), one of your own secrets by its name (`secret`), or taken away (`unset`), over the machine's user's environment (`base` `user`) or only what a harness needs (`clean`). Each with `sessions`, the agents running with it. By name.",
            read_only: true,
            destructive: false,
            input: Input::of::<input::AgentenvsList>(),
            route: |_| Ok((Method::GET, path(&["api", "agentenvs"]))),
            run: |cx, v| Box::pin(list(cx, v)),
        },
        Op {
            name: "agentenvs.get",
            summary: "One of your environments, with its variables (a secret by its name, never its value) and the sessions running with it.",
            read_only: true,
            destructive: false,
            input: Input::of::<input::AgentenvsGet>(),
            route: |v| Ok((Method::GET, path(&["api", "agentenvs", &text(v, "id")?]))),
            run: |cx, v| Box::pin(get(cx, v)),
        },
        Op {
            name: "agentenvs.create",
            summary: "Keep an environment of yours: its `name` (once among yours), `description`, `base` and `vars`, in order (a later one of a name wins); its `scope`, chips `{key, value}` saying where it's offered and applied on its own (none: everywhere), and `auto`, whether it joins the stacks of your sessions its scope matches on its own (then at once, under those given by name, taking effect as each one's harness next starts). Refused naming a bad variable (`agentenv_bad_var`) or a secret you hold none of (`agentenv_secret_missing`); a name you use already, `agentenv_name_taken`. A person keeps their own: an agent can't.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::AgentenvsCreate>(),
            route: |_| Ok((Method::POST, path(&["api", "agentenvs"]))),
            run: |cx, v| Box::pin(create(cx, v)),
        },
        Op {
            name: "agentenvs.update",
            summary: "Change one of your environments over its `version` as you read it (one another change moved on from is refused, `agentenv_stale`): what's given changes, and its version goes up. Sessions running with it take its new version as their harness next starts. A change of its `scope` or `auto` has it join the stacks of your running sessions it now matches on its own, and leave those it joined on its own and matches no more (never one it was given to by name).",
            read_only: false,
            destructive: false,
            input: Input::of::<input::AgentenvsUpdate>(),
            route: |v| Ok((Method::PATCH, path(&["api", "agentenvs", &text(v, "id")?]))),
            run: |cx, v| Box::pin(update(cx, v)),
        },
        Op {
            name: "agentenvs.delete",
            summary: "Delete one of your environments. Refused while a running session's stack holds it (`agentenv_in_use`), naming them.",
            read_only: false,
            destructive: true,
            input: Input::of::<input::AgentenvsDelete>(),
            route: |v| Ok((Method::DELETE, path(&["api", "agentenvs", &text(v, "id")?]))),
            run: |cx, v| Box::pin(delete(cx, v)),
        },
        Op {
            name: "agentenvs.machine",
            summary: "Which variables a machine's user has, by name and where each is set (`login_shell`, `launchd`, `systemd_user`, `pam`, `user_registry`, `system_registry`, `runner`), never a value: what an environment's `machine` variables are picked from. `target`: a machine you may start agents on (`local` for the server).",
            read_only: true,
            destructive: false,
            input: Input::of::<input::AgentenvsMachine>(),
            route: |_| Ok((Method::GET, path(&["api", "agentenvs", "machine"]))),
            run: |cx, v| Box::pin(machine(cx, v)),
        },
        Op {
            name: "agentenvs.apply",
            summary: "Run an agent you may steer with your environments given by name from now on (`agentenvs`: their ids, top first: where two set a variable the higher wins, and the base is the top one's), its harness started again on its conversation at once, unless `restart` is false (then as it next starts). Under them, your environments applied on their own whose scope matches it stay or join (one listed is given by name from then on); []: those alone, over its machine's user's environment. Refused for a session in a microVM where one has `secret` variables (`agentenv_secret_in_microvm`), or one of its secrets you hold none of (`agentenv_secret_missing`). A person applies their own: an agent can't.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::AgentenvsApply>(),
            route: |v| Ok((Method::PUT, path(&["api", "harness", "sessions", &text(v, "agent")?, "agentenv"]))),
            run: |cx, v| Box::pin(apply(cx, v)),
        },
        Op {
            name: "agentenvs.session",
            summary: "An agent you can see as environments' scopes see it: `facts` (its machine, os, arch, harness, agent definition, model, permission and effort, the last two as `<harness>:<value>`), its `stack` of environments, top first (each with its version now, and `auto` where it joined on its own), and `matching`, the ids of your environments whose scope matches it.",
            read_only: true,
            destructive: false,
            input: Input::of::<input::AgentenvsSession>(),
            route: |v| Ok((Method::GET, path(&["api", "harness", "sessions", &text(v, "agent")?, "agentenv"]))),
            run: |cx, v| Box::pin(session(cx, v)),
        },
    ]
}

/// What each of these operations takes (`crate::ops::Input`): its JSON Schema is generated from its
/// type, each field's description from its doc comment.
pub mod input {
    use schemars::JsonSchema;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    pub enum AgentenvBase {
        /// The machine's user's environment, its variables over it.
        User,
        /// Only what a harness needs to run, its variables over that.
        Clean,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    pub enum AgentenvFrom {
        /// The machine's user's variable `of` (its own name when none), read on the machine.
        Machine,
        /// `value`, kept as it is and shown to you.
        Value,
        /// One of your own secrets, `secret`, by its name.
        Secret,
        /// Taken away from what the base gives.
        Unset,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentenvVar {
        /// Its name: letters, digits and `_`, not starting with a digit.
        pub name: String,
        pub from: AgentenvFrom,
        /// `machine`: the machine's variable it takes, when not its own name.
        pub of: Option<String>,
        /// `value`: its value.
        pub value: Option<String>,
        /// `secret`: one of your secrets by its name (`secrets.mine.list`), never its value.
        pub secret: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    pub enum AgentenvScopeKey {
        /// Its runner's machine, by id (`local`: the server).
        Machine,
        /// `linux`, `macos` or `windows`.
        Os,
        /// `amd64` or `arm64`.
        Arch,
        /// Its harness's id (`claude`, `codex`, …).
        Harness,
        /// The agent definition it started from, by id (`kiln:<name>` for kiln's own).
        Agent,
        /// Its model, as the session names it.
        Model,
        /// Its harness's permission mode, `<harness>:<mode>` (`claude:plan`).
        Permission,
        /// Its harness's effort, `<harness>:<effort>` (`codex:high`).
        Effort,
    }

    /// One chip of a scope: chips of one key are any of them, chips of different keys all of them.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentenvChip {
        pub key: AgentenvScopeKey,
        pub value: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentenvsList {}

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentenvsGet {
        /// The environment's id.
        pub id: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentenvsCreate {
        /// Its name, once among yours.
        pub name: String,
        pub description: Option<String>,
        pub base: AgentenvBase,
        /// In order: a later one of a name wins.
        pub vars: Vec<AgentenvVar>,
        /// Your own labels for it, in order.
        #[serde(default)]
        pub tags: Vec<String>,
        /// Where it's offered and applied on its own, chips in order: none, everywhere.
        #[serde(default)]
        pub scope: Vec<AgentenvChip>,
        /// Join the stacks of your sessions its scope matches on its own (off by default).
        #[serde(default)]
        pub auto: bool,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentenvsUpdate {
        /// The environment's id.
        pub id: String,
        /// The version your change is made over: its `version` as you read it.
        #[schemars(range(min = 1))]
        pub version: i64,
        pub name: Option<String>,
        /// Null takes it out.
        #[serde(default, deserialize_with = "crate::ops::present")]
        pub description: Option<Option<String>>,
        pub base: Option<AgentenvBase>,
        pub vars: Option<Vec<AgentenvVar>>,
        /// Your own labels for it, in order: all of them, as they're to be.
        pub tags: Option<Vec<String>>,
        /// Its scope's chips, all of them, as they're to be.
        pub scope: Option<Vec<AgentenvChip>>,
        /// Join the stacks of your sessions its scope matches on its own.
        pub auto: Option<bool>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentenvsDelete {
        /// The environment's id.
        pub id: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentenvsMachine {
        /// The machine: its name (`local` for the server).
        pub target: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentenvsApply {
        /// The agent's id (or the start of it).
        pub agent: String,
        /// The environments it's given by name, by id, top first (one of yours, or one its stack holds already): where
        /// two set a variable the higher wins. Those applied on their own that match it go under them; []: those alone,
        /// over the machine's user's environment.
        pub agentenvs: Vec<String>,
        /// Start its harness again on its conversation now (the default), or leave it to its next start.
        pub restart: Option<bool>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentenvsSession {
        /// The agent's id (or the start of it).
        pub agent: String,
    }
}

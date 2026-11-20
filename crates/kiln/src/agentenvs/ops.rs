//! The environments' operations (`agentenvs.*`, docs/agents.md "Operations"), in kiln's one operation
//! table (`crate::ops::own`): the console's `/api/agentenvs` routes and `PUT
//! /api/harness/sessions/:id/agentenv`, kiln's MCP tools and code mode's `kiln.agentenvs.*`. Their routes
//! need `agents.run`; each keeps its caller to their own ([`super::person`]), and says another's is none.
//! A refusal's words start with its code where docs/agents.md names one ([`super::Refusal::said`]).

use http::{Method, StatusCode};
use serde_json::{Value, json};

use super::{Applied, Brief, Draft, Listing, Patch, Refusal, resolve};
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
    };
    let made = super::create(&cx.hub, &p, draft).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "agentenv.create".into(), format!("{} ({}) v{}", made.name, made.id, made.version)).await;
    json_of(&made)
}

async fn update(cx: Cx, v: Value) -> Out {
    let p = person(&cx)?;
    let id = text(&v, "id")?;
    let version = v.get("version").and_then(Value::as_i64).ok_or_else(|| (StatusCode::BAD_REQUEST, "`version` is the version your change is made over: its `version` as you read it".to_owned()))?;
    let patch = Patch { name: field(&v, "name")?, description: field(&v, "description")?, base: field(&v, "base")?, vars: field(&v, "vars")?, tags: field(&v, "tags")? };
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

/// An environment of the caller's applied to an agent they may steer (none: its machine's user's), its
/// harness started again on its conversation unless `restart` is false: kept as the one it runs with once
/// its runner took it, else as it was.
async fn apply(cx: Cx, v: Value) -> Out {
    let p = person(&cx)?;
    let info = crate::harness::ops::known(&cx, &text(&v, "agent")?)?;
    crate::harness::lineage::may_steer(&cx.hub, &cx.caller, &info.id).await?;
    let asked = field::<Option<String>>(&v, "agentenv")?.flatten().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
    let restart = field::<bool>(&v, "restart")?.unwrap_or(true);
    let env = match asked {
        Some(id) => Some(super::get(&cx.hub, &p, &id).await.map_err(fail)?),
        None => None,
    };
    let target = if info.target.is_empty() { "local".to_owned() } else { info.target.clone() };
    let microvm = info.isolation.is_some();
    match &env {
        Some(e) => resolve::runs_there(&cx.hub, e, &target, microvm).map_err(fail)?,
        None => {
            if let Some(m) = target.strip_prefix("agent:").filter(|m| !resolve::speaks(&cx.hub, m)) {
                return Err(Refusal::Unsupported(format!("{}'s kiln runs no session with an environment: upgrade it (System → Updates)", cx.hub.shown(m))).fail());
            }
        }
    }
    let resolved = match &env {
        Some(e) => Some(resolve::open(&cx.hub, e, microvm, &cx.caller.name, &format!("agent session {}, applied", info.id)).await.map_err(fail)?),
        None => None,
    };
    let brief = resolved.as_ref().map(Brief::of);
    // Kept before it's sent, so the session's runner announcing it with this one finds it kept
    // (`resolve::opened`); put back as it was where its runner didn't take it.
    let was = super::linked(&cx.hub, &info.id);
    super::link(&cx.hub, &info.id, env.as_ref(), &cx.caller.name).await.map_err(fail)?;
    resolve::given(&cx.hub, &info.id, &target);
    let sent = cx.hub.harness.send_agentenv(&cx.hub, &info.id, resolved, restart).await;
    if let Some(why) = sent.failure() {
        let back = match was {
            Some(id) => super::any(&cx.hub, &id).await.ok().flatten(),
            None => None,
        };
        if let Err(e) = super::link(&cx.hub, &info.id, back.as_ref(), &cx.caller.name).await {
            tracing::warn!(target: "kiln.agentenvs", session = %info.id, error = %e, "a session's environment couldn't be put back after its runner didn't take another");
        }
        return Err(why);
    }
    let named = brief.as_ref().map_or_else(|| "its machine's user's".to_owned(), |b| format!("{} ({}) v{}", b.name, b.id, b.version));
    cx.hub.store.audit(cx.caller.name.clone(), "agentenv.apply".into(), format!("{named} to {}{}", info.id, if restart { ", started again" } else { "" })).await;
    json_of(&Applied { agent: info.id, agentenv: brief, restarted: restart })
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
            summary: "Keep an environment of yours: its `name` (once among yours), `description`, `base` and `vars`, in order (a later one of a name wins). Refused naming a bad variable (`agentenv_bad_var`) or a secret you hold none of (`agentenv_secret_missing`); a name you use already, `agentenv_name_taken`. A person keeps their own: an agent can't.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::AgentenvsCreate>(),
            route: |_| Ok((Method::POST, path(&["api", "agentenvs"]))),
            run: |cx, v| Box::pin(create(cx, v)),
        },
        Op {
            name: "agentenvs.update",
            summary: "Change one of your environments over its `version` as you read it (one another change moved on from is refused, `agentenv_stale`): what's given changes, and its version goes up. Sessions running with it take its new version as their harness next starts.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::AgentenvsUpdate>(),
            route: |v| Ok((Method::PATCH, path(&["api", "agentenvs", &text(v, "id")?]))),
            run: |cx, v| Box::pin(update(cx, v)),
        },
        Op {
            name: "agentenvs.delete",
            summary: "Delete one of your environments. Refused while sessions run with it (`agentenv_in_use`), naming them.",
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
            summary: "Run an agent you may steer with one of your environments from now on (`agentenv`: its id; null: its machine's user's environment), its harness started again on its conversation at once, unless `restart` is false (then as it next starts). Refused for a session in a microVM where it has `secret` variables (`agentenv_secret_in_microvm`), or one of its secrets you hold none of (`agentenv_secret_missing`). A person applies their own: an agent can't.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::AgentenvsApply>(),
            route: |v| Ok((Method::PUT, path(&["api", "harness", "sessions", &text(v, "agent")?, "agentenv"]))),
            run: |cx, v| Box::pin(apply(cx, v)),
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
        /// One of your environments, by id; null: the machine's user's environment.
        #[serde(deserialize_with = "crate::ops::present")]
        pub agentenv: Option<String>,
        /// Start its harness again on its conversation now (the default), or leave it to its next start.
        pub restart: Option<bool>,
    }
}

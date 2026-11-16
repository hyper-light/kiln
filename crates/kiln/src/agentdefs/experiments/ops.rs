//! The experiments' operations (`agentdefs.experiments.*`, kiln plan 01a1cd8e862e7169a074db3d5fa33f3d P7.1), in
//! kiln's one operation table with agent definitions' own (`crate::agentdefs::ops::list_ops`): routes under their
//! definition's (`/api/agentdefs/{id}/experiments/…`) and every one of the caller's at once as the UI reads them
//! (`GET /api/agentdef-experiments`), kiln's MCP tools (`agentdefs_experiments_list` …) and code mode's
//! `kiln.agentdefs.experiments.*`. Their routes need `agents.run`, as agent definitions' do; each keeps its caller to
//! their own definitions' (`crate::agentdefs::Who`), another's as none, and an agent's change waits on its person's
//! click as a change of a definition does (`crate::confirms`).

use http::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::{Ask, CriterionAsk, Error, Experiment, Move, VariantAsk};
use crate::agentdefs::Who;
use crate::harness::engine::Engine;
use crate::ops::{Cx, Fail, Input, Op, Out, path};

pub(crate) fn fail(e: Error) -> Fail {
    match e {
        Error::Bad(m) => (StatusCode::BAD_REQUEST, m),
        Error::Missing => (StatusCode::NOT_FOUND, "no such experiment".into()),
        Error::NoSuch(m) => (StatusCode::NOT_FOUND, m),
        Error::Conflict(m) => (StatusCode::CONFLICT, m),
        e @ Error::Stale { .. } => (StatusCode::CONFLICT, e.to_string()),
        e @ Error::Kilns(_) => (StatusCode::FORBIDDEN, e.to_string()),
        Error::Refused(why) => (StatusCode::UNPROCESSABLE_ENTITY, format!("kiln can't run this split: {}", why.join("; "))),
        Error::Def(e) => crate::agentdefs::ops::fail(e),
        Error::Other(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

fn who(cx: &Cx) -> Result<Who, Fail> {
    Who::of(&cx.caller).map_err(crate::agentdefs::ops::fail)
}

/// `v` read as an operation's input: what doesn't fit is refused, saying where and why.
fn read<T: DeserializeOwned>(v: Value) -> Result<T, Fail> {
    serde_json::from_value(v).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))
}

fn json_of<T: serde::Serialize>(v: &T) -> Out {
    serde_json::to_value(v).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// What a change answers: the experiment as it is now, as agentdefs.experiments.list shows it.
fn answer(e: &Experiment) -> Out {
    Ok(json!({"experiment": json_of(e)?}))
}

async fn audit(cx: &Cx, verb: &str, e: &Experiment, what: String) {
    let what = if what.is_empty() { String::new() } else { format!(": {what}") };
    cx.hub.store.audit(cx.caller.name.clone(), verb.into(), format!("{} ({}) of agent definition {}{what}", e.name, e.id, e.agentdef)).await;
}

impl From<crate::agentdefs::ops::input::Engine> for Engine {
    fn from(e: crate::agentdefs::ops::input::Engine) -> Engine {
        match e {
            crate::agentdefs::ops::input::Engine::Host => Engine::Host,
            crate::agentdefs::ops::input::Engine::Microvm => Engine::Microvm,
        }
    }
}

impl From<input::Variant> for VariantAsk {
    fn from(v: input::Variant) -> VariantAsk {
        VariantAsk { name: v.name, weight: v.weight, engine: v.engine.map(Engine::from), agentfile: v.agentfile, cpus: v.cpus, memory: v.memory, model: v.model }
    }
}

impl From<input::Criterion> for CriterionAsk {
    fn from(c: input::Criterion) -> CriterionAsk {
        CriterionAsk { name: c.name, query: c.query, op: c.op, bound: c.bound }
    }
}

async fn list(cx: Cx, v: Value) -> Out {
    let def = v.get("id").and_then(Value::as_str).map(str::trim).filter(|d| !d.is_empty());
    let all = crate::agentdefs::ops::flag(&v, "all");
    json_of(&super::list(&cx.hub, &who(&cx)?, def, all).await.map_err(fail)?)
}

async fn create(cx: Cx, v: Value) -> Out {
    let i: input::ExperimentsCreate = read(v)?;
    let ask = Ask { name: i.name, variants: i.variants.into_iter().map(VariantAsk::from).collect(), criteria: i.criteria.unwrap_or_default().into_iter().map(CriterionAsk::from).collect() };
    // Its draws' randomness, passed in (docs/ci.md §0.3, deterministic cores): the same seed draws the same again.
    let made = super::create(&cx.hub, &who(&cx)?, &i.id, ask, rand::random(), &cx.caller.name).await.map_err(fail)?;
    let split: Vec<String> = made.variants.iter().map(|v| format!("{} {}%", v.name.as_deref().unwrap_or(&v.id), v.weight)).collect();
    audit(&cx, "agentdef.experiment.create", &made, split.join(", ")).await;
    answer(&made)
}

async fn rename(cx: Cx, v: Value) -> Out {
    let i: input::ExperimentsRename = read(v)?;
    let e = super::rename(&cx.hub, &who(&cx)?, &i.id, &i.experiment, i.name.as_deref(), i.variant.as_deref()).await.map_err(fail)?;
    let what = i.variant.map_or_else(String::new, |v| format!("variant {v} named {}", i.name.as_deref().unwrap_or("as it's set apart")));
    audit(&cx, "agentdef.experiment.rename", &e, what).await;
    answer(&e)
}

async fn weights(cx: Cx, v: Value) -> Out {
    let i: input::ExperimentsWeights = read(v)?;
    let e = super::weights(&cx.hub, &who(&cx)?, &i.id, &i.experiment, i.weights, i.base).await.map_err(fail)?;
    let split: Vec<String> = e.variants.iter().map(|v| format!("{} {}%", v.name.as_deref().unwrap_or(&v.id), v.weight)).collect();
    audit(&cx, "agentdef.experiment.weights", &e, format!("split v{}, {}", e.version, split.join(", "))).await;
    answer(&e)
}

async fn add_variant(cx: Cx, v: Value) -> Out {
    let i: input::ExperimentsVariantsAdd = read(v)?;
    let ask = VariantAsk { name: i.name, weight: 0, engine: i.engine.map(Engine::from), agentfile: i.agentfile, cpus: i.cpus, memory: i.memory, model: i.model };
    let e = super::add_variant(&cx.hub, &who(&cx)?, &i.id, &i.experiment, ask).await.map_err(fail)?;
    let added = e.variants.last().map(|v| v.id.clone()).unwrap_or_default();
    audit(&cx, "agentdef.experiment.variant.add", &e, format!("variant {added}")).await;
    answer(&e)
}

async fn remove_variant(cx: Cx, v: Value) -> Out {
    let i: input::ExperimentsVariant = read(v)?;
    let e = super::remove_variant(&cx.hub, &who(&cx)?, &i.id, &i.experiment, &i.variant).await.map_err(fail)?;
    audit(&cx, "agentdef.experiment.variant.remove", &e, format!("variant {}", i.variant)).await;
    answer(&e)
}

async fn add_criterion(cx: Cx, v: Value) -> Out {
    let i: input::ExperimentsCriteriaAdd = read(v)?;
    let ask = CriterionAsk { name: i.name, query: i.query, op: i.op, bound: i.bound };
    let e = super::add_criterion(&cx.hub, &who(&cx)?, &i.id, &i.experiment, ask).await.map_err(fail)?;
    let what = e.criteria.last().map_or_else(String::new, |c| format!("{} ({}): {} {} {}", c.name, c.id, c.query, c.op.as_str(), c.bound));
    audit(&cx, "agentdef.experiment.criterion.add", &e, what).await;
    answer(&e)
}

async fn remove_criterion(cx: Cx, v: Value) -> Out {
    let i: input::ExperimentsCriterion = read(v)?;
    let e = super::remove_criterion(&cx.hub, &who(&cx)?, &i.id, &i.experiment, &i.criterion).await.map_err(fail)?;
    audit(&cx, "agentdef.experiment.criterion.remove", &e, format!("criterion {}", i.criterion)).await;
    answer(&e)
}

async fn moved(cx: Cx, v: Value, to: Move) -> Out {
    let i: input::Experiment = read(v)?;
    let e = super::set_state(&cx.hub, &who(&cx)?, &i.id, &i.experiment, to).await.map_err(fail)?;
    let verb = match to {
        Move::Pause => "agentdef.experiment.pause",
        Move::Resume => "agentdef.experiment.resume",
        Move::Stop => "agentdef.experiment.stop",
    };
    audit(&cx, verb, &e, String::new()).await;
    answer(&e)
}

async fn promote(cx: Cx, v: Value) -> Out {
    let i: input::ExperimentsVariant = read(v)?;
    let (e, d) = super::promote(&cx.hub, &who(&cx)?, &i.id, &i.experiment, &i.variant, &cx.caller.name).await.map_err(fail)?;
    audit(&cx, "agentdef.experiment.promote", &e, format!("variant {} made {} v{}", i.variant, d.name, d.version)).await;
    Ok(json!({"experiment": json_of(&e)?, "agentdef": json_of(&d)?}))
}

async fn delete(cx: Cx, v: Value) -> Out {
    let i: input::Experiment = read(v)?;
    let gone = super::delete(&cx.hub, &who(&cx)?, &i.id, &i.experiment).await.map_err(fail)?;
    audit(&cx, "agentdef.experiment.delete", &gone, String::new()).await;
    Ok(json!({"deleted": true}))
}

/// Experiment `experiment` of definition `id`'s route, `tail` after it.
fn under(v: &Value, method: Method, tail: &[&str]) -> Result<(Method, Vec<String>), Fail> {
    let (id, experiment) = (crate::ops::text(v, "id")?, crate::ops::text(v, "experiment")?);
    let mut p = path(&["api", "agentdefs", &id, "experiments", &experiment]);
    p.extend(tail.iter().map(|s| (*s).to_owned()));
    Ok((method, p))
}

/// The operation a request under `/api/agentdefs/{id}/experiments` is (`rest`, its path after that), and what its
/// path names of the operation's input; none where there's no such route. Each operation's own route
/// (`list_ops`) is one of these (`tests::p7_1_each_route_is_its_operation`).
pub fn op_of<'a>(method: &Method, rest: &[&'a str]) -> Option<(&'static str, Vec<(&'static str, &'a str)>)> {
    Some(match (method, rest) {
        (&Method::POST, []) => ("agentdefs.experiments.create", Vec::new()),
        (&Method::PATCH, [e]) => ("agentdefs.experiments.rename", vec![("experiment", *e)]),
        (&Method::DELETE, [e]) => ("agentdefs.experiments.delete", vec![("experiment", *e)]),
        (&Method::POST, [e, "weights"]) => ("agentdefs.experiments.weights", vec![("experiment", *e)]),
        (&Method::POST, [e, "pause"]) => ("agentdefs.experiments.pause", vec![("experiment", *e)]),
        (&Method::POST, [e, "resume"]) => ("agentdefs.experiments.resume", vec![("experiment", *e)]),
        (&Method::POST, [e, "stop"]) => ("agentdefs.experiments.stop", vec![("experiment", *e)]),
        (&Method::POST, [e, "promote"]) => ("agentdefs.experiments.promote", vec![("experiment", *e)]),
        (&Method::POST, [e, "variants"]) => ("agentdefs.experiments.variants.add", vec![("experiment", *e)]),
        (&Method::PATCH, [e, "variants", v]) => ("agentdefs.experiments.rename", vec![("experiment", *e), ("variant", *v)]),
        (&Method::DELETE, [e, "variants", v]) => ("agentdefs.experiments.variants.remove", vec![("experiment", *e), ("variant", *v)]),
        (&Method::POST, [e, "criteria"]) => ("agentdefs.experiments.criteria.add", vec![("experiment", *e)]),
        (&Method::DELETE, [e, "criteria", c]) => ("agentdefs.experiments.criteria.remove", vec![("experiment", *e), ("criterion", *c)]),
        _ => return None,
    })
}

/// The operations, in the order they are listed.
pub fn list_ops() -> Vec<Op> {
    vec![
        Op {
            name: "agentdefs.experiments.list",
            summary: "Your agent definitions' experiments (each splits a definition's new sessions between deployments, its variants, by their weights, and is judged by its criteria): `{ [definition id]: experiment[] }`, each definition's newest first, as agentdefs.experiments.create answers one. `id`: one definition's alone (kiln's own take none); `all`: everyone's, for the server's owner and those with agentdefs.manage.",
            read_only: true,
            destructive: false,
            input: Input::of::<input::ExperimentsList>(),
            route: |_| Ok((Method::GET, path(&["api", "agentdef-experiments"]))),
            run: |cx, v| Box::pin(list(cx, v)),
        },
        Op {
            name: "agentdefs.experiments.create",
            summary: "Start an experiment on one of your agent definitions (`id`; kiln's own take none, 403), live at once: while it is, each new session of the definition you start (agentdefs.run, or a UI agent it stands in for) is one of its `variants`, drawn by their weights, and kept with the session. Its `name`; its 2 to 100 `variants`, each a deployment (`engine` microvm or host; a microVM's `agentfile`, `cpus` and `memory` in bytes, each left out the definition's; `model`, left out the definition's; `name`) and its `weight`, a whole percent (together 100), its first what the definition runs now; and its `criteria` (`name`, a `query` in kiln's monitoring syntax without its comparison, `op` and `bound`), which kiln's monitors must read. A variant kiln can't run is refused (422), and one at a time is live on a definition (409 while another is). Answers `{experiment}`: its `id`, `agentdef`, `name`, `state` (live, paused, promoted or stopped) and `state_at` (since when, Unix ms), its split's `version` (1, one more with each change of its weights), `sessions` (how many it drew started), `started_at` (Unix ms), `promoted` (the variant, once one was), `by`, its `criteria` (`id`, `name`, `query`, `op`, `bound`) and `variants` (`id`, `name`, `engine`, a microVM's `agentfile`, `cpus` and `memory`, `model` where it has its own, `weight`, `sessions`, and `current`: what its definition runs now).",
            read_only: false,
            destructive: false,
            input: Input::of::<input::ExperimentsCreate>(),
            route: |v| Ok((Method::POST, path(&["api", "agentdefs", &crate::ops::text(v, "id")?, "experiments"]))),
            run: |cx, v| Box::pin(create(cx, v)),
        },
        Op {
            name: "agentdefs.experiments.rename",
            summary: "Rename one of your experiments (`id` its definition, `experiment`, `name`), or with `variant` one of its variants (no `name`: back to what sets it apart from the others), whatever it stands at. Answers `{experiment}` as agentdefs.experiments.create does.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::ExperimentsRename>(),
            route: |v| match v.get("variant").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
                Some(variant) => under(v, Method::PATCH, &["variants", variant]),
                None => under(v, Method::PATCH, &[]),
            },
            run: |cx, v| Box::pin(rename(cx, v)),
        },
        Op {
            name: "agentdefs.experiments.weights",
            summary: "Set one of your live or paused experiments' split: `weights`, every variant's id to its whole percent of new sessions (together 100), its split's next `version`; `base`, the version your change is made over, refused (409) where another change moved it on. The same weights again are no new version. Answers `{experiment}` as agentdefs.experiments.create does.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::ExperimentsWeights>(),
            route: |v| under(v, Method::POST, &["weights"]),
            run: |cx, v| Box::pin(weights(cx, v)),
        },
        Op {
            name: "agentdefs.experiments.variants.add",
            summary: "Add a variant to one of your live or paused experiments, last, at weight 0 (agentdefs.experiments.weights gives it a share): a deployment as agentdefs.experiments.create takes one, checked as its definition run so would be. Answers `{experiment}` as agentdefs.experiments.create does.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::ExperimentsVariantsAdd>(),
            route: |v| under(v, Method::POST, &["variants"]),
            run: |cx, v| Box::pin(add_variant(cx, v)),
        },
        Op {
            name: "agentdefs.experiments.variants.remove",
            summary: "Take a variant out of one of your live or paused experiments: never the one its definition runs now, and never below 2. Its share goes back to the others in proportion, its split's next version where it had one; the sessions drawn for it stay the experiment's. Answers `{experiment}` as agentdefs.experiments.create does.",
            read_only: false,
            destructive: true,
            input: Input::of::<input::ExperimentsVariant>(),
            route: |v| under(v, Method::DELETE, &["variants", &crate::ops::text(v, "variant")?]),
            run: |cx, v| Box::pin(remove_variant(cx, v)),
        },
        Op {
            name: "agentdefs.experiments.criteria.add",
            summary: "Add a criterion to one of your experiments, whatever it stands at: its `name`, its `query` in kiln's monitoring syntax (Datadog's, as kiln's monitors take one) without its comparison (`avg(last_14d):avg:kiln.session.warm_ms{experiment:<id>} by {variant}`), its `op` (>, >=, ==, !=, < or <=) and `bound`, kept as you wrote them once kiln's monitors read them. Answers `{experiment}` as agentdefs.experiments.create does.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::ExperimentsCriteriaAdd>(),
            route: |v| under(v, Method::POST, &["criteria"]),
            run: |cx, v| Box::pin(add_criterion(cx, v)),
        },
        Op {
            name: "agentdefs.experiments.criteria.remove",
            summary: "Take a criterion out of one of your experiments, whatever it stands at. Answers `{experiment}` as agentdefs.experiments.create does.",
            read_only: false,
            destructive: true,
            input: Input::of::<input::ExperimentsCriterion>(),
            route: |v| under(v, Method::DELETE, &["criteria", &crate::ops::text(v, "criterion")?]),
            run: |cx, v| Box::pin(remove_criterion(cx, v)),
        },
        Op {
            name: "agentdefs.experiments.pause",
            summary: "Pause one of your live experiments: its definition's new sessions start as the definition is until it's resumed. Paused already, it's as it is. Answers `{experiment}` as agentdefs.experiments.create does.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::Experiment>(),
            route: |v| under(v, Method::POST, &["pause"]),
            run: |cx, v| Box::pin(moved(cx, v, Move::Pause)),
        },
        Op {
            name: "agentdefs.experiments.resume",
            summary: "Resume one of your paused experiments: its definition's new sessions are drawn by its weights again, while no other of the definition's is live (409). Answers `{experiment}` as agentdefs.experiments.create does.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::Experiment>(),
            route: |v| under(v, Method::POST, &["resume"]),
            run: |cx, v| Box::pin(moved(cx, v, Move::Resume)),
        },
        Op {
            name: "agentdefs.experiments.stop",
            summary: "Stop one of your live or paused experiments for good: no more draws, its definition as it is. It stays listed, its criteria still yours to change. Answers `{experiment}` as agentdefs.experiments.create does.",
            read_only: false,
            destructive: true,
            input: Input::of::<input::Experiment>(),
            route: |v| under(v, Method::POST, &["stop"]),
            run: |cx, v| Box::pin(moved(cx, v, Move::Stop)),
        },
        Op {
            name: "agentdefs.experiments.promote",
            summary: "Promote one of a live or paused experiment's variants (`variant`): its deployment (engine, Agentfile, size, model) made its definition's next version, checked as any change of it is, and the experiment over. Answers `{experiment, agentdef}`, the definition as it is now.",
            read_only: false,
            destructive: true,
            input: Input::of::<input::ExperimentsVariant>(),
            route: |v| under(v, Method::POST, &["promote"]),
            run: |cx, v| Box::pin(promote(cx, v)),
        },
        Op {
            name: "agentdefs.experiments.delete",
            summary: "Delete one of your experiments, whatever it stands at, its variants, criteria and draws with it; the sessions it drew go on as they started. Answers `{ deleted: true }`.",
            read_only: false,
            destructive: true,
            input: Input::of::<input::Experiment>(),
            route: |v| under(v, Method::DELETE, &[]),
            run: |cx, v| Box::pin(delete(cx, v)),
        },
    ]
}

/// What each of these operations takes (`crate::ops::Input`): its JSON Schema is generated from its type, each
/// field's description from its doc comment.
pub mod input {
    use std::collections::BTreeMap;

    use schemars::JsonSchema;
    use serde::Deserialize;

    use super::super::Comparison;
    use crate::agentdefs::ops::input::Engine;

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct ExperimentsList {
        /// One agent definition's alone: one of yours (kiln's own take none).
        pub id: Option<String>,
        /// Everyone's: the server's owner's, and those with agentdefs.manage.
        pub all: Option<bool>,
    }

    /// A variant: a deployment its definition's new sessions start in, and its share of them.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Variant {
        /// Its name; left out: what sets it apart from the others.
        pub name: Option<String>,
        /// Its whole percent of new sessions: the variants' add up to 100.
        #[schemars(range(max = 100))]
        pub weight: u32,
        /// `microvm` (left out, the same) or `host` (its harness a process of its machine's, with no Agentfile or
        /// size).
        pub engine: Option<Engine>,
        /// A microVM's Agentfile: one of yours (`agentfile:<id>`) or kiln's for its harness (`harness-<harness>`);
        /// left out, its definition's.
        pub agentfile: Option<String>,
        /// A microVM's vCPUs; left out, its definition's.
        #[schemars(range(min = 1))]
        pub cpus: Option<u32>,
        /// A microVM's memory, in bytes, a whole number of MiB; left out, its definition's.
        #[schemars(range(min = 1))]
        pub memory: Option<u64>,
        /// Its model; left out, its definition's.
        pub model: Option<String>,
    }

    /// A criterion: what a variant's sessions are judged by.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Criterion {
        pub name: String,
        /// What it measures, in kiln's monitoring syntax (Datadog's, as kiln's monitors take a query), without its
        /// comparison: `avg(last_14d):avg:kiln.session.warm_ms{experiment:<id>} by {variant}`.
        pub query: String,
        /// How its value compares with `bound`: >, >=, ==, !=, < or <=.
        pub op: Comparison,
        pub bound: f64,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct ExperimentsCreate {
        /// The agent definition's id: one of yours.
        pub id: String,
        pub name: String,
        /// 2 to 100, the first what the definition runs now; their weights add up to 100.
        pub variants: Vec<Variant>,
        pub criteria: Option<Vec<Criterion>>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct ExperimentsRename {
        /// The agent definition's id.
        pub id: String,
        /// The experiment's id.
        pub experiment: String,
        /// Its new name; a variant's, left out or empty: back to what sets it apart.
        pub name: Option<String>,
        /// One of its variants, to rename that instead.
        pub variant: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct ExperimentsWeights {
        /// The agent definition's id.
        pub id: String,
        /// The experiment's id.
        pub experiment: String,
        /// Every variant's id to its whole percent of new sessions: together 100.
        pub weights: BTreeMap<String, u32>,
        /// The split version your change is made over (its `version` as you read it): refused (409) where it isn't
        /// current. Without it, it's made over the version kiln finds.
        #[schemars(range(min = 1))]
        pub base: Option<i64>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct ExperimentsVariantsAdd {
        /// The agent definition's id.
        pub id: String,
        /// The experiment's id.
        pub experiment: String,
        /// Its name; left out: what sets it apart from the others.
        pub name: Option<String>,
        /// `microvm` (left out, the same) or `host`.
        pub engine: Option<Engine>,
        /// A microVM's Agentfile; left out, its definition's.
        pub agentfile: Option<String>,
        /// A microVM's vCPUs; left out, its definition's.
        #[schemars(range(min = 1))]
        pub cpus: Option<u32>,
        /// A microVM's memory, in bytes, a whole number of MiB; left out, its definition's.
        #[schemars(range(min = 1))]
        pub memory: Option<u64>,
        /// Its model; left out, its definition's.
        pub model: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct ExperimentsVariant {
        /// The agent definition's id.
        pub id: String,
        /// The experiment's id.
        pub experiment: String,
        /// The variant's id.
        pub variant: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct ExperimentsCriteriaAdd {
        /// The agent definition's id.
        pub id: String,
        /// The experiment's id.
        pub experiment: String,
        pub name: String,
        /// What it measures, in kiln's monitoring syntax, without its comparison.
        pub query: String,
        /// >, >=, ==, !=, < or <=.
        pub op: Comparison,
        pub bound: f64,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct ExperimentsCriterion {
        /// The agent definition's id.
        pub id: String,
        /// The experiment's id.
        pub experiment: String,
        /// The criterion's id.
        pub criterion: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Experiment {
        /// The agent definition's id.
        pub id: String,
        /// The experiment's id.
        pub experiment: String,
    }
}

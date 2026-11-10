//! The skills operations (`skills.*`, docs/agents.md "Skills kiln installs"), in kiln's one
//! operation table (`crate::ops::own`): the console's `/api/skills` routes and code mode's
//! `kiln.skills.*`. A person's alone: an agent installs nothing for itself or another, so none of
//! them is an agent's tool, and each refuses an agent calling.

use std::sync::Arc;

use http::{Method, StatusCode};
use serde_json::{Value, json};

use super::{At, Error, Scope};
use crate::harness::lineage::Access;
use crate::ops::{Cx, Fail, Input, Op, Out, path};

fn fail(e: Error) -> Fail {
    match e {
        Error::Bad(m) => (StatusCode::BAD_REQUEST, m),
        Error::Missing(m) => (StatusCode::NOT_FOUND, m),
        Error::Forbidden(m) => (StatusCode::FORBIDDEN, m),
        Error::Other(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

fn text(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned)
}

fn need(v: &Value, k: &str, what: &str) -> Result<String, Fail> {
    text(v, k).ok_or_else(|| (StatusCode::BAD_REQUEST, format!("`{k}` is {what}")))
}

/// A person, never an agent.
fn person(cx: &Cx) -> Result<(), Fail> {
    if cx.caller.agent.is_some() {
        return Err((StatusCode::FORBIDDEN, "skills are installed by a person, not by an agent".into()));
    }
    Ok(())
}

/// Agent `id` as scopes see it, once the caller may act on it.
async fn agent(cx: &Cx, id: &str) -> Result<At, Fail> {
    person(cx)?;
    if crate::harness::lineage::access(&cx.hub, &cx.caller, id).await < Access::Act {
        return Err((StatusCode::NOT_FOUND, format!("no agent {id}")));
    }
    let info = cx.hub.harness.list().into_iter().find(|i| i.id == id).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no agent {id}")))?;
    let opener = cx.hub.store.kv_get(crate::mcp::opener_key(id)).await.ok().flatten();
    Ok(At { machine: crate::harness::wait::machine(&info.target).to_owned(), root: info.tree.clone().filter(|t| !t.is_empty()).unwrap_or(info.cwd), opener, id: info.id })
}

/// A package the caller may change: theirs, or any for the server's owner.
async fn own(cx: &Cx, id: &str) -> Result<super::Installed, Fail> {
    person(cx)?;
    let p = super::get(&cx.hub, id).await.map_err(fail)?;
    if p.by != cx.caller.name && !cx.caller.is_owner() {
        return Err((StatusCode::FORBIDDEN, format!("{} installed {}: they change it", p.by, p.package.name)));
    }
    Ok(p)
}

async fn list(cx: Cx, v: Value) -> Out {
    let a = agent(&cx, &need(&v, "agent", "the agent whose skills these are")?).await?;
    let all = super::all(&cx.hub).await.map_err(fail)?;
    let mine: Vec<super::Installed> = all.into_iter().filter(|p| p.scope.applies(&a)).collect();
    super::check(&cx.hub, &mine);
    let mut out = Vec::with_capacity(mine.len());
    for p in &mine {
        out.push(super::view(&cx.hub, p, Some(&a.machine)).await);
    }
    let speaks = a.machine == "local" || super::machines(&cx.hub).1.contains_key(&a.machine);
    Ok(json!({"packages": out, "machine": a.machine, "installs": speaks}))
}

async fn look(cx: Cx, v: Value) -> Out {
    let a = agent(&cx, &need(&v, "agent", "the agent it's for")?).await?;
    let from = text(&v, "from").unwrap_or_else(|| "link".into());
    let plugin = text(&v, "plugin");
    let found = match from.as_str() {
        "link" => {
            let l = super::source::link(&need(&v, "link", "a link to a repository")?).map_err(|m| (StatusCode::BAD_REQUEST, m))?;
            let (files, sha) = super::source::fetch(&cx.hub, &l).await.map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
            let src = super::Source { kind: "link".into(), label: l.label.clone(), url: l.url.clone(), git_ref: l.git_ref.clone(), path: l.path.clone(), machine: String::new() };
            let want = plugin.or_else(|| Some(l.path.clone()).filter(|p| !p.is_empty()));
            let fallback = l.label.rsplit('/').next().unwrap_or("skills").to_owned();
            super::look(&cx.hub, &cx.caller.name, src, sha, &files, want.as_deref(), &fallback).await
        }
        "folder" => {
            let path = need(&v, "path", "the folder on the agent's machine")?;
            let files = super::read_folder(&cx.hub, &a.machine, &path).await.map_err(fail)?;
            let shown = if a.machine == "local" { "server" } else { &a.machine };
            let src = super::Source { kind: "folder".into(), label: format!("{shown}:{path}"), path: path.clone(), machine: a.machine.clone(), ..super::Source::default() };
            let fallback = path.trim_end_matches('/').rsplit('/').next().unwrap_or("skills").to_owned();
            super::look(&cx.hub, &cx.caller.name, src, String::new(), &files, plugin.as_deref(), &fallback).await
        }
        "write" => {
            let files = super::source::written(&need(&v, "name", "its name")?, &need(&v, "description", "when to use it")?, &need(&v, "body", "its steps")?).map_err(|m| (StatusCode::BAD_REQUEST, m))?;
            let src = super::Source { kind: "write".into(), label: "written here".into(), ..super::Source::default() };
            super::look(&cx.hub, &cx.caller.name, src, String::new(), &files, None, "skill").await
        }
        other => return Err((StatusCode::BAD_REQUEST, format!("`from` is link, folder or write, not {other:?} (an upload is POST /api/skills/upload)"))),
    };
    answer(found.map_err(fail)?)
}

fn answer(found: Result<super::Draft, Vec<super::detect::Item>>) -> Out {
    match found {
        Ok(d) => Ok(super::draft_view(&d)),
        Err(plugins) => Ok(json!({"choose": plugins})),
    }
}

/// An upload's bytes, named by its file (`POST /api/skills/upload?agent=…&name=…`).
pub async fn upload(cx: Cx, agent_id: &str, name: &str, bytes: &[u8]) -> Out {
    agent(&cx, agent_id).await?;
    let files = super::source::unpack(name, bytes).map_err(|m| (StatusCode::BAD_REQUEST, m))?;
    let src = super::Source { kind: "upload".into(), label: name.rsplit(['/', '\\']).next().unwrap_or(name).to_owned(), ..super::Source::default() };
    let fallback = src.label.split('.').next().unwrap_or("skills").to_owned();
    answer(super::look(&cx.hub, &cx.caller.name, src, String::new(), &files, None, &fallback).await.map_err(fail)?)
}

async fn install(cx: Cx, v: Value) -> Out {
    let a = agent(&cx, &need(&v, "agent", "the agent it's installed from")?).await?;
    let picks: super::detect::Picks = v.get("picks").cloned().map(serde_json::from_value).transpose().map_err(|e| (StatusCode::BAD_REQUEST, format!("`picks`: {e}")))?.unwrap_or_default();
    let scope = match text(&v, "scope").as_deref().unwrap_or("agent") {
        "agent" => Scope::Agent { agent: a.id.clone() },
        "mine" => Scope::Mine { opener: cx.caller.opener().ok_or_else(|| (StatusCode::FORBIDDEN, "your token acts for no one's agents: install for this agent".to_owned()))? },
        "repo" => Scope::Repo { machine: a.machine.clone(), root: a.root.clone() },
        other => return Err((StatusCode::BAD_REQUEST, format!("`scope` is agent, mine or repo, not {other:?}"))),
    };
    let p = super::install(&cx.hub, &cx.caller.name, &need(&v, "draft", "what skills.look found")?, picks, scope).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "skills.install".into(), format!("{} {}@{}", p.id, p.package.name, p.sha)).await;
    Ok(super::view(&cx.hub, &p, Some(&a.machine)).await)
}

async fn set(cx: Cx, v: Value) -> Out {
    let id = need(&v, "id", "the package's id")?;
    own(&cx, &id).await?;
    let on = v.get("on").and_then(Value::as_bool).ok_or_else(|| (StatusCode::BAD_REQUEST, "`on` is true or false".to_owned()))?;
    let p = super::set(&cx.hub, &id, on).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "skills.set".into(), format!("{id} on={on}")).await;
    Ok(super::view(&cx.hub, &p, None).await)
}

async fn update(cx: Cx, v: Value) -> Out {
    let id = need(&v, "id", "the package's id")?;
    own(&cx, &id).await?;
    let p = super::update(&cx.hub, &id).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "skills.update".into(), format!("{id} {}", p.sha)).await;
    Ok(super::view(&cx.hub, &p, None).await)
}

async fn remove(cx: Cx, v: Value) -> Out {
    let id = need(&v, "id", "the package's id")?;
    let p = own(&cx, &id).await?;
    super::remove(&cx.hub, &id).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "skills.remove".into(), format!("{id} {}", p.package.name)).await;
    Ok(json!({"removed": id}))
}

async fn read(cx: Cx, v: Value) -> Out {
    person(&cx)?;
    let p = super::get(&cx.hub, &need(&v, "id", "the package's id")?).await.map_err(fail)?;
    let words = super::read_skill(&cx.hub, &p, &need(&v, "skill", "the skill's name")?).await.map_err(fail)?;
    Ok(json!({"content": words}))
}

fn id_route(v: &Value, tail: &[&str], m: Method) -> Result<(Method, Vec<String>), Fail> {
    let id = need(v, "id", "the package's id")?;
    let mut p = vec!["api", "skills", id.as_str()];
    p.extend_from_slice(tail);
    Ok((m, path(&p)))
}

/// The `skills.*` operations.
pub fn list_ops() -> Vec<Op> {
    vec![
        Op {
            name: "skills.list",
            summary: "The skills kiln installed that an agent has: each package with its source and commit, its skills and agents, whom it's for, whether it's on, a newer commit when its link has one, and its state on each machine it went to (fetching, setting up, ready, needs a tool, failed).",
            read_only: true,
            destructive: false,
            input: Input::of::<input::List>(),
            route: |_| Ok((Method::GET, path(&["api", "skills"]))),
            run: |cx, v| Box::pin(list(cx, v)),
        },
        Op {
            name: "skills.look",
            summary: "Look at a source of skills for an agent, installing nothing: a link (`from: link`, a GitHub repository or a folder or file in one, any git URL), a folder on the agent's machine (`from: folder`, `path`), or one written here (`from: write`: `name`, `description`, `body`). Answers what it holds (its skills, agents, hooks, MCP servers, setup, the tools it needs) as a `draft` to install; a marketplace of several plugins answers `choose`, to look again with `plugin`.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::Look>(),
            route: |_| Ok((Method::POST, path(&["api", "skills", "look"]))),
            run: |cx, v| Box::pin(look(cx, v)),
        },
        Op {
            name: "skills.install",
            summary: "Install a draft skills.look found, with the parts picked (`picks`: skills, agents, setup on; hooks and MCP servers off unless turned on), for this agent (`scope: agent`), every agent of yours (`mine`) or every agent in this agent's repository folder on its machine (`repo`). In place of the same package installed for the same scope.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::Install>(),
            route: |_| Ok((Method::POST, path(&["api", "skills"]))),
            run: |cx, v| Box::pin(install(cx, v)),
        },
        Op {
            name: "skills.set",
            summary: "Turn an installed package on or off (`on`): off, no agent has it, and it stays installed.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::Set>(),
            route: |v| id_route(v, &[], Method::PATCH),
            run: |cx, v| Box::pin(set(cx, v)),
        },
        Op {
            name: "skills.update",
            summary: "Fetch an installed link's package again at its branch's commit now, keeping what was picked and whom it's for.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::Id>(),
            route: |v| id_route(v, &["update"], Method::POST),
            run: |cx, v| Box::pin(update(cx, v)),
        },
        Op {
            name: "skills.remove",
            summary: "Uninstall a package: every agent that had it loses it, and its machines let go of it.",
            read_only: false,
            destructive: true,
            input: Input::of::<input::Id>(),
            route: |v| id_route(v, &[], Method::DELETE),
            run: |cx, v| Box::pin(remove(cx, v)),
        },
        Op {
            name: "skills.read",
            summary: "An installed package's skill, its SKILL.md's words.",
            read_only: true,
            destructive: false,
            input: Input::of::<input::Read>(),
            route: |v| id_route(v, &["skills", &need(v, "skill", "the skill's name")?], Method::GET),
            run: |cx, v| Box::pin(read(cx, v)),
        },
    ]
}

/// What each operation takes (`crate::ops::Input`).
pub mod input {
    use schemars::JsonSchema;
    use serde::Deserialize;

    #[derive(Deserialize, JsonSchema)]
    pub struct List {
        /// The agent whose skills these are.
        pub agent: String,
    }

    #[derive(Deserialize, JsonSchema)]
    pub struct Look {
        /// The agent it's for (a folder is on its machine).
        pub agent: String,
        /// "link" (the default), "folder" or "write".
        #[serde(default)]
        pub from: Option<String>,
        /// A link: github.com/owner/repo, …/tree/<branch>/<folder>, …/blob/<branch>/<file>, or a git URL.
        #[serde(default)]
        pub link: Option<String>,
        /// A folder on the agent's machine (`~` is its home).
        #[serde(default)]
        pub path: Option<String>,
        /// A marketplace's plugin, by name, as `choose` listed them.
        #[serde(default)]
        pub plugin: Option<String>,
        /// A written skill's name.
        #[serde(default)]
        pub name: Option<String>,
        /// A written skill's description: when to use it.
        #[serde(default)]
        pub description: Option<String>,
        /// A written skill's body (Markdown).
        #[serde(default)]
        pub body: Option<String>,
    }

    #[derive(Deserialize, JsonSchema)]
    pub struct Picks {
        #[serde(default)]
        pub skills: Option<bool>,
        #[serde(default)]
        pub agents: Option<bool>,
        #[serde(default)]
        pub hooks: Option<bool>,
        #[serde(default)]
        pub mcp: Option<bool>,
        #[serde(default)]
        pub setup: Option<bool>,
    }

    #[derive(Deserialize, JsonSchema)]
    pub struct Install {
        /// The agent it's installed from.
        pub agent: String,
        /// What skills.look found (its `draft`).
        pub draft: String,
        /// "agent" (the default), "mine" or "repo".
        #[serde(default)]
        pub scope: Option<String>,
        #[serde(default)]
        pub picks: Option<Picks>,
    }

    #[derive(Deserialize, JsonSchema)]
    pub struct Set {
        /// The package's id.
        pub id: String,
        pub on: bool,
    }

    #[derive(Deserialize, JsonSchema)]
    pub struct Id {
        /// The package's id.
        pub id: String,
    }

    #[derive(Deserialize, JsonSchema)]
    pub struct Read {
        /// The package's id.
        pub id: String,
        /// The skill's name.
        pub skill: String,
    }
}

/// Kept for callers that hold an [`Arc`] of the hub (`crate::api`).
pub fn cx(hub: &Arc<crate::hub::Hub>, caller: &crate::authz::Caller) -> Cx {
    Cx { hub: Arc::clone(hub), caller: caller.clone() }
}

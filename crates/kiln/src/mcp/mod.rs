//! kiln's MCP server: `/api/mcp` (streamable HTTP, JSON answers) and `kiln mcp` (a stdio
//! bridge to it), docs/access.md and docs/cd.md.
//!
//! **Code mode** is the point. `code_search` runs a program over the operation registry;
//! `code_run` runs a program that calls operations (`await kiln.cd.apps.list({...})`) in a
//! bounded QuickJS sandbox inside kiln, and only what the program returns enters the agent's
//! context ([`code`]). The operations are also offered one by one, as tools named after them
//! (`cd_apps_list`), for clients without code mode. kiln's skills are served as resources and
//! through the skills extension ([`skills`]).
//!
//! Callers authenticate like the rest of the API, or with an MCP token kiln mints for an agent
//! (`kiln_mcp_<label>_<hmac>`), under which every call is audited as `agent:<label>`.

pub mod code;
pub mod rpc;
pub mod skills;

use std::sync::Arc;

use http::{HeaderMap, StatusCode};
use serde_json::{Value, json};

use crate::authz::Caller;
use crate::http::server::{self as srv, Response};
use crate::hub::Hub;

/// What agents kiln starts on this machine are given: kiln's MCP server (as a Claude Code
/// `--mcp-config` file holding a token of their own) and kiln's skills (as a Claude Code plugin
/// directory).
pub struct Kit {
    /// The MCP config file every agent here could share (0600: it holds the token).
    pub claude_mcp_config: std::path::PathBuf,
    /// The plugin directory with kiln's skills.
    pub plugin_dir: std::path::PathBuf,
    /// Where this machine reaches the server's MCP endpoint.
    url: String,
    /// What its tokens are made with.
    key: String,
    /// Where each agent's own config is kept.
    dir: std::path::PathBuf,
}

impl Kit {
    /// Agent session `session`'s own MCP config (0600): a token that is that agent, so kiln knows
    /// which agent calls (`Caller::agent`) and whom it acts for. The shared one if it can't be
    /// written.
    pub fn config_for(&self, session: &str) -> std::path::PathBuf {
        match self.write_for(session) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(%session, error = %format!("{e:#}"), "an agent's own MCP config couldn't be written: it shares the machine's");
                self.claude_mcp_config.clone()
            }
        }
    }

    fn write_for(&self, session: &str) -> anyhow::Result<std::path::PathBuf> {
        let safe: String = session.chars().filter(char::is_ascii_alphanumeric).take(64).collect();
        anyhow::ensure!(!safe.is_empty(), "no session");
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(format!("{safe}.json"));
        let body = json!({"mcpServers": {"kiln": {"type": "http", "url": format!("{}/api/mcp", self.url.trim_end_matches('/')),
            "headers": {"Authorization": format!("Bearer {}", session_token(&self.key, &safe))}}}});
        write_private(&path, &serde_json::to_vec_pretty(&body)?)?;
        Ok(path)
    }
}

/// `bytes` as `path`, readable by this user alone, whole or not at all.
fn write_private(path: &std::path::Path, bytes: &[u8]) -> anyhow::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// The label of an agent session's own MCP token.
const SESSION_LABEL: &str = "session-";

/// Agent session `session`'s own MCP token: calls with it are that agent's.
pub fn session_token(admin_token: &str, session: &str) -> String {
    token(admin_token, &format!("{SESSION_LABEL}{session}"))
}

static KIT: std::sync::OnceLock<Kit> = std::sync::OnceLock::new();

/// The kit, once the server has written it.
pub fn kit() -> Option<&'static Kit> {
    KIT.get()
}

/// Writes the kit under `<data>/agents` for the server at `local_url` (how this machine reaches
/// it), so every agent kiln starts here has kiln's MCP server and skills.
pub fn init_kit(hub: &Hub, local_url: &str) -> anyhow::Result<()> {
    let dir = hub.cfg.server.data_dir.join("agents");
    let plugin = dir.join("kiln-plugin");
    write_plugin(&plugin)?;
    let config = dir.join("kiln-mcp.json");
    let body = json!({"mcpServers": {"kiln": {"type": "http", "url": format!("{}/api/mcp", local_url.trim_end_matches('/')),
        "headers": {"Authorization": format!("Bearer {}", token(&hub.admin_token, "local-agents"))}}}});
    write_private(&config, &serde_json::to_vec_pretty(&body)?)?;
    let _ = KIT.set(Kit { claude_mcp_config: config, plugin_dir: plugin, url: local_url.to_owned(), key: hub.admin_token.clone(), dir: dir.join("mcp") });
    Ok(())
}

/// Where kiln keeps whom agent session `id` acts for (`owner` or `user:<id>`), from whoever
/// started it.
pub fn opener_key(id: &str) -> String {
    format!("agents/opener/{id}")
}

/// Where the person who opened agent session `id` is kept, whatever token they used.
pub fn opened_by_key(id: &str) -> String {
    format!("agents/opened-by/{id}")
}

/// Whose agent session `session` is (`owner` or `user:<id>`): the person who opened it, whatever
/// token they used (docs/agents.md, "Whose agents"), else whom it acts for ([`acts_for`]). One
/// opened with a token narrower than its person acts for no one, but is still theirs.
pub async fn whose(hub: &Hub, session: &str) -> Option<String> {
    match hub.store.kv_get(opened_by_key(session)).await.ok().flatten() {
        Some(p) => Some(p),
        None => acts_for(hub, session).await,
    }
}

/// Whom agent session `session` acts for (`owner` or `user:<id>`): whoever started it, or for one
/// started with `kiln spawn` in a kiln shell, whoever opened that shell; the owner on a kiln nobody
/// else signs in to. `None`: kiln doesn't know.
pub async fn acts_for(hub: &Hub, session: &str) -> Option<String> {
    let shell = hub.harness.info(session).and_then(|i| i.shell);
    let opener = match hub.store.kv_get(opener_key(session)).await.ok().flatten() {
        Some(o) => Some(o),
        None => match shell {
            Some(t) => hub.store.kv_get(terminal_opener_key(&t)).await.ok().flatten(),
            None => None,
        },
    };
    match opener {
        Some(o) => Some(o),
        None if !crate::authz::gate::has_users(hub).await => Some("owner".to_owned()),
        None => None,
    }
}

/// The caller agent session `session` is (one kiln runs, on `runner` or the server): it acts for
/// whoever started it (the owner on a kiln nobody else signs in to), audited as
/// `agent:<short>[@<runner>] for <them>`, and what it starts are its children.
pub async fn session_caller(hub: &Hub, session: &str, runner: Option<&str>) -> Result<Caller, String> {
    let Some(opener) = acts_for(hub, session).await else {
        return Err("kiln doesn't know who started this agent, so it can't act for them: start the agent from Agents".to_owned());
    };
    let short: String = session.chars().take(8).collect();
    let name = runner.map_or_else(|| format!("agent:{short}"), |r| format!("agent:{short}@{r}"));
    let Some(mut caller) = crate::authz::gate::agent_for(hub, &name, &opener).await else {
        return Err("whoever started this agent can't sign in here any more".to_owned());
    };
    caller.agent = Some(session.to_owned());
    // An agent kiln started for an attachment does no more than its capabilities give it.
    if let Some(limit) = crate::ci::attach::limit_for(hub, session).await {
        caller = caller.limited_to(limit);
    }
    Ok(caller)
}

/// Where kiln keeps who opened kiln terminal `id` (`owner` or `user:<id>`).
pub fn terminal_opener_key(id: &str) -> String {
    format!("terminals/opener/{id}")
}

/// The person in kiln terminal `terminal` (one on `runner`, or on the server): whoever opened it
/// (the owner on a kiln nobody else signs in to), audited as `shell:<short>[@<runner>] for <them>`.
pub async fn terminal_caller(hub: &Hub, terminal: &str, runner: Option<&str>) -> Result<Caller, String> {
    let short: String = terminal.chars().take(8).collect();
    let t = hub.terminals.get(terminal).ok_or_else(|| format!("kiln has no terminal {short} open: run this in a terminal kiln opened"))?;
    let here = runner.map_or_else(|| "local".to_owned(), |r| format!("agent:{r}"));
    if t.target != here {
        return Err(format!("terminal {short} isn't on this machine"));
    }
    let opener = match hub.store.kv_get(terminal_opener_key(terminal)).await.ok().flatten() {
        Some(o) => o,
        None if !crate::authz::gate::has_users(hub).await => "owner".to_owned(),
        None => return Err("kiln doesn't know who opened this terminal, so it can't act for them: open a new one".to_owned()),
    };
    let name = runner.map_or_else(|| format!("shell:{short}"), |r| format!("shell:{short}@{r}"));
    crate::authz::gate::agent_for(hub, &name, &opener).await.ok_or_else(|| "whoever opened this terminal can't sign in here any more".to_owned())
}

/// The caller an MCP token of an agent session is (`session_token`), when `actor` is one: that
/// agent, if kiln still runs it. `None`: another kind of MCP token.
pub async fn token_caller(hub: &Hub, actor: &str) -> Option<Result<Caller, String>> {
    let session = actor.strip_prefix("agent:")?.strip_prefix(SESSION_LABEL)?;
    if hub.harness.info(session).is_none() {
        return Some(Err(format!("kiln doesn't run agent {} any more: its MCP token ended with it", session.chars().take(8).collect::<String>())));
    }
    Some(session_caller(hub, session, None).await)
}

/// The most an answer to a runner may be (it goes over the runner's control lane).
const RUNNER_ANSWER_MAX: usize = 4 << 20;

/// The plans operations an agent has as tools: keeping plans, running them and following their
/// runs (a plan's versions, and deleting one, are code mode's).
const PLAN_TOOLS: &[&str] = &["plans.list", "plans.get", "plans.create", "plans.update", "plans.ready", "plans.run", "plans.runs.list", "plans.runs.get", "plans.runs.stop", "plans.runs.retry"];

/// kiln's operations an agent has as tools of its own (`agents_start`, `goals_plan`, `plans_run`,
/// …): `agents.*`, `goals.*` but a stack's settings, most of `plans.*`, and `research.*`. The server says which
/// ([`runner_ask`]'s registry answer), so its agents' MCP servers offer what it has, however old
/// their runner: a deploy reaches the agents at work.
pub fn agent_ops() -> Vec<crate::ops::Op> {
    crate::harness::ops::list_ops()
        .into_iter()
        // Whether a stack plans its big goals first is the person's to say, not its agent's.
        .chain(crate::goals::ops::list_ops().into_iter().filter(|o| o.name != "goals.settings"))
        .chain(crate::plans::ops::list_ops().into_iter().filter(|o| PLAN_TOOLS.contains(&o.name)))
        .chain(crate::research::ops::list_ops())
        .collect()
}

/// What a runner asks for one of its agent sessions' MCP server (`kiln reach mcp`, whose code
/// mode runs on the runner, `crate::reach::mcp`): the operations (`registry`), one run (`call`), a
/// page it shows or streams (`page`, `stream`: `crate::harness::visuals::ask`,
/// `crate::harness::streams::ask`), or word of a message from its agent it couldn't read
/// (`unread`, `runner_unread`). The session must be one kiln knows on that runner, which
/// mustn't be a job's; it acts for whoever started it (the owner on a kiln nobody else signs in
/// to), audited as `agent:<session>@<runner> for <them>`. The answer: `{"registry", "skills"}`,
/// `{"ok": <answer>}`, or `{"error", "status"?}`.
pub async fn runner_ask(hub: &Arc<Hub>, runner: &str, ask: &crate::reach::Question) -> Value {
    use crate::reach::Question;
    if hub.agents.lock().get(runner).is_none_or(|a| a.ephemeral || a.outpost) {
        return json!({"error": "kiln's operations are for its agents' runners, not a job's"});
    }
    let (session, terminal) = match ask {
        Question::Directory => return json!({ "runners": hub.reach_directory() }),
        // Noted whoever started the session (`runner_unread`).
        Question::Unread { session, what, code, why } => return runner_unread(hub, runner, session, what, *code, why).await,
        Question::Registry { session, terminal } | Question::Call { session, terminal, .. } => (session.as_str(), terminal.as_str()),
        Question::Page { session, .. } | Question::Stream { session, .. } => (session.as_str(), ""),
        // The hub answers these itself (`Hub::on_read`).
        Question::Keep { .. } | Question::Holds { .. } | Question::Holders { .. } => return json!({"error": "not a question for kiln's operations"}),
    };
    // The person in one of its kiln shells (`kiln spawn --on`, `kiln agents wait`), for a call.
    let caller = if session.is_empty() && !terminal.is_empty() && matches!(ask, Question::Call { .. }) {
        terminal_caller(hub, terminal, Some(runner)).await
    } else if session.is_empty() || !hub.harness.located_on(session, runner) {
        return json!({"error": format!("kiln knows no agent session {session:?} on {runner}: its operations are for the agents it runs")});
    } else {
        session_caller(hub, session, Some(runner)).await
    };
    let caller = match caller {
        Ok(c) => c,
        Err(e) => return json!({"error": e}),
    };
    let answer = match ask {
        Question::Registry { .. } => {
            let skills: Vec<Value> = skills::list().into_iter().map(|s| json!({"name": s.get("name"), "description": s.get("description")})).collect();
            let tools: Vec<&str> = agent_ops().iter().map(|o| o.name).collect();
            json!({"registry": code::registry(hub), "skills": skills, "agent_tools": tools})
        }
        Question::Call { name, input, .. } => match crate::ops::misfit(&caller, name, input) {
            // An input that doesn't fit the operation: refused saying where and why, and noted,
            // on the runner's page too.
            Some(m) => {
                let mut a = misfit(hub, crate::wire::unread::Wire::McpReach, &caller, &format!("tools/call {}", tool_name(name)), name, &m, Some(crate::wire::unread::Page::Runner(runner.to_owned()))).await;
                if let Some(o) = a.as_object_mut() {
                    o.insert("status".into(), json!(StatusCode::BAD_REQUEST.as_u16()));
                }
                a
            }
            None => match crate::ops::call(crate::ops::Cx { hub: Arc::clone(hub), caller }, name, input.clone()).await {
                Ok(v) => json!({"ok": v}),
                Err((status, msg)) => json!({"error": msg, "status": status.as_u16()}),
            },
        },
        // A page it shows in its conversation (`crate::harness::visuals`), or one it streams
        // live there (`crate::harness::streams`): its own session's, audited as the agent acting
        // for whoever started it.
        Question::Page { .. } | Question::Stream { .. } => {
            // What it asks, as its handler reads it: `{"op", "session", "do", ...}`.
            let page = matches!(ask, Question::Page { .. });
            let q = serde_json::to_value(ask).unwrap_or_default();
            let r = if page { crate::harness::visuals::ask(hub, session, &q).await } else { crate::harness::streams::ask(hub, session, &q).await };
            let what = format!("{}.{}", if page { "pages" } else { "streams" }, q.get("do").and_then(Value::as_str).unwrap_or("?"));
            if what != "pages.measured" {
                let (principal, on_behalf_of) = crate::audit::who(&caller);
                let entry = crate::audit::Entry {
                    principal,
                    on_behalf_of,
                    action: what,
                    target: Some(format!("agent session {session}")),
                    result: if r.is_ok() { "ok".to_owned() } else { "error".to_owned() },
                    detail: match &r {
                        Ok(v) => v.get("url").or_else(|| v.get("path")).and_then(Value::as_str).unwrap_or("").to_owned(),
                        Err(e) => format!("{e:#}"),
                    },
                    ..crate::audit::Entry::default()
                };
                crate::audit::write(&hub.store, entry).await;
            }
            match r {
                Ok(v) => json!({"ok": v}),
                Err(e) => json!({"error": format!("{e:#}")}),
            }
        }
        Question::Directory => json!({ "runners": hub.reach_directory() }),
        Question::Unread { session, what, code, why } => runner_unread(hub, runner, session, what, *code, why).await,
        Question::Keep { .. } | Question::Holds { .. } | Question::Holders { .. } => json!({"error": "not a question for kiln's operations"}),
    };
    let size = answer.to_string().len();
    if size > RUNNER_ANSWER_MAX {
        return json!({"error": format!("the answer is {size} bytes, more than the {RUNNER_ANSWER_MAX} a runner is sent: filter or page on the server")});
    }
    answer
}

/// Word from an agent session's MCP server on `runner` (`crate::reach::mcp`) of a message its
/// agent sent that it couldn't read: noted against the session (wire `mcp_reach`), on the runner's
/// page too. Word of a session that doesn't run there is refused.
async fn runner_unread(hub: &Arc<Hub>, runner: &str, session: &str, what: &str, code: crate::wire::unread::Code, why: &str) -> Value {
    use crate::wire::unread::{Page, Sender, Unread, Wire};
    if session.is_empty() || !hub.harness.located_on(session, runner) {
        return json!({"error": format!("kiln knows no agent session {session:?} on {runner}: its operations are for the agents it runs")});
    }
    let u = Unread { wire: Wire::McpReach, from: Sender::Session { id: session.to_owned() }, what: what.to_owned(), code, why: why.to_owned() };
    let _ = hub.note_unread(u, Some(Page::Runner(runner.to_owned()))).await;
    json!({"ok": {}})
}

/// kiln's skills as a Claude Code plugin directory at `dir`.
fn write_plugin(dir: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir.join(".claude-plugin"))?;
    std::fs::write(
        dir.join(".claude-plugin").join("plugin.json"),
        serde_json::to_vec_pretty(&json!({"name": "kiln", "version": env!("CARGO_PKG_VERSION"),
            "description": "kiln's skills: running agents, deploying, diagnosing deployments and CI, defining apps, moving from Argo CD."}))?,
    )?;
    skills::install(&dir.join("skills"))?;
    Ok(())
}

/// kiln's skills for the agents a runner starts, where the server's kit isn't (another machine):
/// a plugin directory under this machine's kiln data, written once per process.
pub fn plugin_dir() -> Option<&'static std::path::Path> {
    static DIR: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        // Tests write a folder of their own: the machine's is what its real runner hands every
        // agent it starts, and a test build's skills (a branch's) mustn't reach them.
        let agents = if cfg!(test) { std::env::temp_dir().join(format!("kiln-test-agents-{}", std::process::id())) } else { crate::config::default_data_dir().join("agents") };
        let dir = agents.join("kiln-plugin");
        match write_plugin(&dir) {
            Ok(()) => Some(dir),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "couldn't write kiln's skills for agents");
                None
            }
        }
    })
    .as_deref()
}

/// Protocol versions kiln speaks, newest first.
const VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
/// The skills extension's identifier (SEP-2640).
const SKILLS_EXTENSION: &str = "io.modelcontextprotocol/skills";

const INSTRUCTIONS: &str = "kiln builds, tests and deploys code, and runs coding agents. CI is under `kiln.ci`: runs, their legs and logs, \
workflows (validate and plan before proposing), reruns, approvals, and failed legs kept to look into (`ci.legs.exec` runs \
a command in one, `ci.legs.rerun_step` reruns a step there). Deployments (GitOps CD) are under `kiln.cd`: apps, their \
sync and health, resources and why they differ, diffs, history, syncs, clusters. Agents are under `kiln.agents`: start \
agents for parts of the work on any machine (`agents_start`), carry on while they work (kiln tells you when each is done; \
`agents_wait` when you can't go on without one) and read what they concluded \
(`agents_result`), and put an agent's work up for review (`agents_change`); what they ask a person stays a person's to answer. \
Goal stacks, plans and research are under `kiln.goals`, `kiln.plans` and `kiln.research`. Prefer code_run: one program can list, filter and act on a \
whole fleet and return only what matters. code_search shows the operations and their inputs. The skills (kiln-agents, \
kiln-plans, kiln-deploy, kiln-diagnose, kiln-apps, kiln-argocd-migration, and CI's kiln-ci-author, kiln-ci-diagnose, \
kiln-ci-runners, kiln-ci-review, kiln-ci-migrate, kiln-ci-secrets and kiln-ci-performance) say how to use them.";

/// What kiln's MCP servers (this one and the runners', `crate::reach::mcp`) tell an agent about
/// its own goal stack and plans.
pub const GOALS_AND_PLANS: &str = "kiln keeps a goal stack for you: `goals_add` lines goals up (leave out `agent`: they're \
yours), one condition or objective each, in order, and kiln gives you each in turn and holds you to it until it's met, then \
the next, so you carry on without a person. To pursue a document or a list as a goal stack, add one goal per item (every \
item asked for), each saying what done is, then end your turn: the first comes to you as soon as you're free. When you've \
met the goal under way, say so with `goals_done` and what shows it (`evidence`): the next comes at once (skipping a goal \
leaves it unmet). `goals_split` splits a goal into parts you pursue first (a plan step's goal into its acceptance criteria \
and tests); `goals_update` changes one goal by its id (its words, its place, or takes it out); `goals_control` pauses the \
stack (say `why`; `resume_in_s` resumes it by itself), resumes, skips or clears it; `goals_get` says where it stands. A plan \
is kept in kiln for the person to read, change and run again: `plans_create` keeps one (its top-level list items are its \
steps; a step's `needs` are the steps it waits for), and `plans_run` runs it on you (leave out `session` and `new`: as one \
message, a step a turn, or each step a goal on your goal stack, `split` making each a goal stack of its acceptance criteria \
and tests) or with an agent of its own for each step (mode `agents`: in your harness, on your machine and in your folder \
unless you say otherwise; steps that don't wait for each other run side by side; a step given to `main` goes on your own goal \
stack; `plans_runs_get` follows the run); `only` runs just some of its steps, by number. `goals_plan` turns a goal into a plan \
(do that first with a goal that takes several steps of work, rather than starting on it): research \
it first, inside (files on your machine and others, kiln's records: `conversations_search`) and outside (the web, docs, \
papers), recording each claim with its sources and the exact words quoted (`research_add`: kiln checks each quote where it's \
cited and grades each finding A to F), write its steps with the findings each rests on, then `plans_ready`: its steps become \
the goal's parts, and once they're all met so is the goal. Whenever you present research to a person, give every claim's \
sources and its grade and score as `research_get`'s markdown has them. The kiln-plans skill says how.";

/// Who an agent kiln runs is, as kiln's MCP servers tell it.
pub fn you_are(agent: &str) -> String {
    format!("You are kiln agent {agent}: that's your id wherever an operation asks for an agent's, and kiln's operations act for whoever started you.")
}

/// What this server tells `caller` it is for: an agent kiln runs also hears about its goal stack
/// and plans, and who it is.
fn instructions(caller: &Caller) -> String {
    match caller.agent.as_deref() {
        Some(me) => format!("{INSTRUCTIONS} {GOALS_AND_PLANS} {}", you_are(me)),
        None => INSTRUCTIONS.to_owned(),
    }
}

/// An MCP token for `label` (an agent session, an attachment): `kiln_mcp_<label>_<hmac>`.
pub fn token(admin_token: &str, label: &str) -> String {
    let label: String = label.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(64).collect();
    format!("kiln_mcp_{label}_{}", crate::util::hmac_hex(admin_token.as_bytes(), format!("mcp:{label}").as_bytes()))
}

/// The label an MCP token was minted for, if it is one of kiln's.
pub fn verify(admin_token: &str, presented: &str) -> Option<String> {
    let rest = presented.strip_prefix("kiln_mcp_")?;
    let (label, mac) = rest.rsplit_once('_')?;
    let want = crate::util::hmac_hex(admin_token.as_bytes(), format!("mcp:{label}").as_bytes());
    crate::util::ct_eq(want.as_bytes(), mac.as_bytes()).then(|| label.to_owned())
}

/// Who calls `/api/mcp` as the owner: `agent:<label>` for an MCP token, `admin` for the admin
/// token or the owner's session; `None` if neither (a user's session or token is the route
/// table's to judge).
pub fn caller(hub: &Hub, headers: &HeaderMap, authed: bool) -> Option<String> {
    let bearer = headers.get(http::header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
    if let Some(label) = bearer.and_then(|t| verify(&hub.admin_token, t.trim())) {
        return Some(format!("agent:{label}"));
    }
    authed.then(|| "admin".to_owned())
}

/// The most of a body `/api/mcp` reads.
const BODY_MOST: usize = 16 << 20;

/// `POST /api/mcp`: one JSON-RPC message or a batch, each operation as `caller` may (W20-R06).
pub async fn http(hub: &Arc<Hub>, caller: Caller, body: kiln_http::Incoming) -> Result<Response, (StatusCode, String)> {
    use http_body_util::BodyExt as _;
    let read = http_body_util::Limited::new(body, BODY_MOST).collect().await.map(|b| b.to_bytes());
    Ok(respond(hub, caller, read).await)
}

/// What a body `caller` sent is answered: its messages' answers, or 202 when none has one. A body
/// it couldn't read whole is refused as JSON-RPC 2.0 has it, under a null id, and noted like any
/// message it can't read: one past [`BODY_MOST`] (413), or one cut short.
pub(crate) async fn respond(hub: &Arc<Hub>, caller: Caller, read: Result<bytes::Bytes, Box<dyn std::error::Error + Send + Sync>>) -> Response {
    let bytes = match read {
        Ok(b) => b,
        Err(e) => {
            let (status, refused) = if e.downcast_ref::<http_body_util::LengthLimitError>().is_some() {
                (StatusCode::PAYLOAD_TOO_LARGE, rpc::Refused::new(Value::Null, rpc::INVALID_REQUEST, format!("Invalid Request: more than {} MiB", BODY_MOST >> 20)))
            } else {
                (StatusCode::BAD_REQUEST, rpc::Refused::new(Value::Null, rpc::PARSE_ERROR, "Parse error: the body was cut short"))
            };
            let server = Server { hub: hub.clone(), caller };
            return srv::json(status, &server.refused(Box::new(refused)).await);
        }
    };
    match answer(hub, caller, &bytes).await {
        Some(a) => srv::json(StatusCode::OK, &a),
        None => srv::status(StatusCode::ACCEPTED),
    }
}

/// What `bytes` that `caller` sent (one JSON-RPC message or a batch) is answered, if anything.
/// Each message is read into a request, or refused as JSON-RPC 2.0 says (`rpc`) and noted against
/// whoever sent it: the agent session its MCP token names, or the person.
pub(crate) async fn answer(hub: &Arc<Hub>, caller: Caller, bytes: &[u8]) -> Option<Value> {
    let body = rpc::body(bytes);
    let server = Server { hub: hub.clone(), caller };
    let mut answers = Vec::new();
    for m in body.messages {
        let answer = match m {
            // A notification is never answered; one no client sends is noted all the same.
            Ok(req) if req.id.is_none() => {
                if let Some(r) = req.unknown_notification() {
                    server.note(&r).await;
                }
                None
            }
            Ok(req) => Some(match server.handle(&req).await {
                Ok(result) => rpc_ok(req.answer_id(), result),
                Err(refused) => server.refused(refused).await,
            }),
            Err(refused) => Some(server.refused(refused).await),
        };
        answers.extend(answer);
    }
    rpc::reply(body.batch, answers)
}

fn rpc_ok(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// A call (`what`: `tools/call goals_add`) of operation `name` whose input doesn't fit it
/// (`crate::ops::misfit`), as `caller` made it on `wire`: noted against whoever made it, and on
/// `on`'s page too; the answer to give it, saying where and why so the model can correct it.
pub(crate) async fn misfit(hub: &Hub, wire: crate::wire::unread::Wire, caller: &Caller, what: &str, name: &str, m: &crate::ops::Misfit, on: Option<crate::wire::unread::Page>) -> Value {
    let u = crate::wire::unread::Unread { wire, from: sender_of(caller), what: what.to_owned(), code: crate::wire::unread::Code::Malformed, why: m.noted() };
    let _ = hub.note_unread(u, on).await;
    m.answer(name)
}

/// What a message refused is noted from (`crate::wire::unread`): the agent session `caller` is,
/// shown on its page, or the person.
pub fn sender_of(caller: &Caller) -> crate::wire::unread::Sender {
    match caller.agent.as_deref() {
        Some(id) => crate::wire::unread::Sender::Session { id: id.to_owned() },
        None => crate::wire::unread::Sender::Person { user: caller.name.clone() },
    }
}

/// The most of a run's logs `kiln://runs/<id>/logs` gives at once.
const RUN_LOGS: usize = 4 << 20;

/// kiln's resources, as templates (docs/ci.md §4.4).
fn templates() -> Value {
    json!([
        {"uriTemplate": "kiln://runs/{run}/logs", "name": "run-logs", "title": "A run's logs", "description": "Every leg's steps' logs, in order.", "mimeType": "text/plain"},
        {"uriTemplate": "kiln://legs/{leg}/logs/{step}", "name": "step-log", "title": "A step's log", "description": "One step of a leg (its index in ci.legs.get).", "mimeType": "text/plain"},
        {"uriTemplate": "kiln://workflows/{owner}/{repo}/{path}", "name": "workflow", "title": "A workflow's file", "description": "As the repository's default branch has it.", "mimeType": "application/yaml"},
    ])
}

/// The MCP name of an operation: dots become underscores (tool names are `[a-zA-Z0-9_-]`).
pub fn tool_name(op: &str) -> String {
    op.replace('.', "_")
}

struct Server {
    hub: Arc<Hub>,
    caller: Caller,
}

impl Server {
    fn cx(&self) -> crate::ops::Cx {
        crate::ops::Cx { hub: self.hub.clone(), caller: self.caller.clone() }
    }

    /// `refused` noted against whoever sent it (`crate::wire::unread`, wire `mcp_http`), when it
    /// says a message couldn't be read.
    async fn note(&self, refused: &rpc::Refused) {
        if refused.unreadable() {
            let u = crate::wire::unread::Unread { wire: crate::wire::unread::Wire::McpHttp, from: sender_of(&self.caller), what: refused.what.clone(), code: refused.code(), why: refused.noted() };
            let _ = self.hub.note_unread(u, None).await;
        }
    }

    /// `refused`, noted ([`Server::note`]), as the answer to send.
    async fn refused(&self, refused: Box<rpc::Refused>) -> Value {
        self.note(&refused).await;
        refused.answer()
    }

    /// A request's result, or why it's refused.
    async fn handle(&self, req: &rpc::Request) -> Result<Value, Box<rpc::Refused>> {
        Ok(match req.method.as_str() {
            "initialize" => {
                let p: rpc::Initialize = req.params()?;
                let asked = p.protocol_version.unwrap_or_default();
                let version = VERSIONS.iter().find(|v| **v == asked).copied().unwrap_or(VERSIONS.first().copied().unwrap_or("2025-11-25"));
                json!({
                    "protocolVersion": version,
                    "capabilities": {
                        "tools": {"listChanged": false},
                        "resources": {"listChanged": false},
                        "extensions": {SKILLS_EXTENSION: {}},
                    },
                    "serverInfo": {"name": "kiln", "title": "kiln", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": instructions(&self.caller),
                })
            }
            "ping" => json!({}),
            "tools/list" => {
                req.params::<rpc::Paginated>()?;
                json!({"tools": tools(&self.hub)})
            }
            "tools/call" => {
                let p: rpc::ToolsCall = req.params()?;
                // A tool it doesn't have is a protocol error (MCP 2025-11-25), not the tool's.
                if !matches!(p.name.as_str(), "code_search" | "code_run") && !crate::ops::all(&self.hub).iter().any(|o| tool_name(o.name) == p.name) {
                    return Err(Box::new(req.unknown_tool(&p.name)));
                }
                self.call_tool(&p.name, Value::Object(p.arguments.unwrap_or_default())).await
            }
            "resources/list" => {
                req.params::<rpc::Paginated>()?;
                json!({"resources": skills::resources()})
            }
            "resources/read" => {
                let p: rpc::ResourcesRead = req.params()?;
                let uri = p.uri.as_str();
                if uri.starts_with("kiln://") {
                    return match self.read_kiln(uri).await {
                        Ok(text) => Ok(json!({"contents": [{"uri": uri, "mimeType": if uri.starts_with("kiln://workflows/") { "application/yaml" } else { "text/plain" }, "text": text}]})),
                        Err(e) => Err(Box::new(rpc::Refused::new(req.answer_id(), -32002, e))),
                    };
                }
                match skills::read(uri) {
                    Some(c) => json!({"contents": [c]}),
                    None => return Err(Box::new(rpc::Refused::new(req.answer_id(), -32002, format!("no resource {uri}")))),
                }
            }
            "skills/list" => json!({"skills": skills::list()}),
            "skills/get" => {
                let p: rpc::SkillsGet = req.params()?;
                let key = p.uri.or(p.name).unwrap_or_default();
                match skills::get(&key) {
                    Some(s) => s,
                    None => return Err(Box::new(rpc::Refused::new(req.answer_id(), -32002, format!("no skill {key}")))),
                }
            }
            "resources/templates/list" => {
                req.params::<rpc::Paginated>()?;
                json!({"resourceTemplates": templates()})
            }
            "prompts/list" => {
                req.params::<rpc::Paginated>()?;
                json!({"prompts": []})
            }
            _ => return Err(Box::new(req.unknown_method())),
        })
    }

    /// One of kiln's resources (docs/ci.md §4.4), read through the operations, so the caller
    /// reads exactly what it may: a run's logs (every leg's steps', in order), a leg's step's
    /// log, a workflow's file.
    async fn read_kiln(&self, uri: &str) -> Result<String, String> {
        let call = |name: &'static str, input: Value| crate::ops::call(self.cx(), name, input);
        let said = |(_, m): crate::ops::Fail| m;
        let text = |v: Value| v.get("text").and_then(Value::as_str).unwrap_or_default().to_owned();
        let parts: Vec<&str> = uri.trim_start_matches("kiln://").split('/').collect();
        match parts.as_slice() {
            ["legs", leg, "logs", step] => {
                let step: i64 = step.parse().map_err(|_| format!("{uri}: a step is its index"))?;
                Ok(text(call("ci.legs.logs", json!({"leg": leg, "step": step})).await.map_err(said)?))
            }
            ["runs", run, "logs"] => {
                let detail = call("ci.runs.get", json!({"run": run})).await.map_err(said)?;
                let mut out = String::new();
                for l in detail.get("legs").and_then(Value::as_array).cloned().unwrap_or_default() {
                    let Some(leg) = l.get("id").and_then(Value::as_str) else { continue };
                    let name = l.get("name").and_then(Value::as_str).or_else(|| l.get("key").and_then(Value::as_str)).unwrap_or(leg);
                    let steps = call("ci.legs.get", json!({"leg": leg})).await.map_err(said)?;
                    for st in steps.get("steps").and_then(Value::as_array).cloned().unwrap_or_default() {
                        let idx = st.get("idx").and_then(Value::as_i64).unwrap_or_default();
                        let log = text(call("ci.legs.logs", json!({"leg": leg, "step": idx})).await.map_err(said)?);
                        out.push_str(&format!("== {name} · {} ==\n{log}", st.get("name").and_then(Value::as_str).unwrap_or_default()));
                        if !out.ends_with('\n') {
                            out.push('\n');
                        }
                        if out.len() > RUN_LOGS {
                            out.push_str("… (the rest: read each leg's steps with ci.legs.logs)\n");
                            return Ok(out);
                        }
                    }
                }
                Ok(out)
            }
            ["workflows", owner, name, path @ ..] if !path.is_empty() => {
                let v = call("ci.workflows.get", json!({"repo": format!("{owner}/{name}"), "path": path.join("/")})).await.map_err(said)?;
                Ok(v.get("source").and_then(Value::as_str).unwrap_or_default().to_owned())
            }
            _ => Err(format!("no resource {uri}: kiln's are kiln://runs/<id>/logs, kiln://legs/<id>/logs/<step> and kiln://workflows/<owner>/<repo>/<path>")),
        }
    }

    async fn call_tool(&self, name: &str, args: Value) -> Value {
        let cx = self.cx();
        match name {
            "code_search" | "code_run" => {
                let program = args.get("program").and_then(Value::as_str).unwrap_or("").to_owned();
                let mode = if name == "code_search" { code::Mode::Search } else { code::Mode::Run { input: args.get("input").cloned().unwrap_or(Value::Null) } };
                let result = crate::rt::blocking(move || code::run(cx, mode, &program, code::Limits::default())).await;
                match result {
                    Ok(Ok(out)) => tool_ok(out),
                    Ok(Err(e)) => tool_error_value(json!({"error": e.message, "code": e.code, "calls": e.calls})),
                    Err(e) => tool_error(&format!("the sandbox could not run: {e:#}")),
                }
            }
            _ => {
                let Some(op) = crate::ops::all(&self.hub).into_iter().find(|o| tool_name(o.name) == name) else {
                    return tool_error(&format!("no tool {name}"));
                };
                // An input that doesn't fit the operation: a tool error saying where and why, so
                // the model can correct it; noted.
                if let Some(m) = crate::ops::misfit(&self.caller, op.name, &args) {
                    return tool_error_value(misfit(&self.hub, crate::wire::unread::Wire::McpHttp, &self.caller, &format!("tools/call {name}"), op.name, &m, None).await);
                }
                match crate::ops::call(cx, op.name, args).await {
                    Ok(v) => tool_ok(v),
                    Err((_, e)) => tool_error(&e),
                }
            }
        }
    }
}

fn tool_ok(v: Value) -> Value {
    let text = serde_json::to_string(&v).unwrap_or_default();
    json!({"content": [{"type": "text", "text": text}], "structuredContent": if v.is_object() { v } else { json!({"result": v}) }, "isError": false})
}

fn tool_error(msg: &str) -> Value {
    json!({"content": [{"type": "text", "text": msg}], "isError": true})
}

fn tool_error_value(v: Value) -> Value {
    let text = serde_json::to_string(&v).unwrap_or_default();
    json!({"content": [{"type": "text", "text": text}], "structuredContent": v, "isError": true})
}

/// Every tool: code mode's two, then each operation.
pub fn tools(hub: &Hub) -> Vec<Value> {
    tools_of(crate::ops::all(hub))
}

fn tools_of(ops: Vec<crate::ops::Info>) -> Vec<Value> {
    let mut out = vec![
        json!({
            "name": "code_search",
            "description": "Run a JavaScript program over kiln's operation registry and return what it returns. The program sees `registry` (every operation: {name, call, summary, read_only, destructive, input}) and `skills` ({name, description}). Example: `return registry.filter(o => o.name.startsWith(\"cd.apps\")).map(o => ({call: o.call, input: o.input}))`.",
            "inputSchema": {"type": "object", "properties": {"program": {"type": "string", "description": "The body of an async function; `return` its answer."}}, "required": ["program"]},
            "annotations": {"readOnlyHint": true, "openWorldHint": false},
        }),
        json!({
            "name": "code_run",
            "description": "Run a JavaScript program that calls kiln and return what it returns: only that enters your context. Operations are async functions on `kiln` (`await kiln.cd.apps.list({health: \"Degraded\"})`, `kiln.call(\"cd.apps.get\", {app})`); a refused call throws an Error with `result`. `input` is given as `input`. Bounded: 64 MiB, 60 s, 2,000 calls, a 4 MiB answer.",
            "inputSchema": {"type": "object", "properties": {
                "program": {"type": "string", "description": "The body of an async function; `return` its answer."},
                "input": {"description": "Any JSON, available as `input`."}
            }, "required": ["program"]},
            "annotations": {"readOnlyHint": false, "destructiveHint": true, "openWorldHint": false},
        }),
    ];
    for o in ops {
        out.push(json!({
            "name": tool_name(o.name),
            "title": o.name,
            "description": o.summary,
            "inputSchema": (o.input)(),
            "annotations": {"readOnlyHint": o.read_only, "destructiveHint": o.destructive, "openWorldHint": false},
        }));
    }
    out
}

/// `kiln mcp`: MCP over stdio for local clients, each message forwarded to the server's
/// `/api/mcp` and each answer written back, one JSON message a line.
pub async fn stdio(url: String, token: String) -> anyhow::Result<()> {
    use std::io::Write as _;
    let client = crate::http::client::Client::public()?;
    let (tx, rx) = async_channel::bounded::<Vec<u8>>(64);
    // stdin is read on a thread of its own: it blocks. Each line goes as it came, so the server
    // refuses one that isn't UTF-8 as it refuses what isn't JSON.
    std::thread::Builder::new().name("kiln-mcp-stdin".into()).spawn(move || {
        for line in rpc::lines(std::io::stdin().lock()) {
            if tx.send_blocking(line).is_err() {
                break;
            }
        }
    })?;
    while let Ok(line) = rx.recv().await {
        let resp = client
            .request(crate::http::client::Method::Post, &format!("{}/api/mcp", url.trim_end_matches('/')))
            .bearer(&token)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            // `agents_wait` may wait up to 10 minutes.
            .timeout(std::time::Duration::from_secs(660))
            .body(line.clone())
            .send()
            .await;
        let answer = match resp {
            Ok(r) if r.status() == 202 => continue,
            Ok(r) => r.bytes(64 << 20).await.map(|b| String::from_utf8_lossy(&b).into_owned()),
            Err(e) => Err(e),
        };
        let out = match answer {
            Ok(text) => text,
            // Each request it held is answered that kiln is unreachable; a notification isn't.
            Err(e) => {
                let body = rpc::body(&line);
                let ids = body.messages.into_iter().filter_map(|m| match m {
                    Ok(req) => req.id,
                    Err(refused) => Some(refused.id),
                });
                let answers = ids.map(|id| rpc::Refused::new(id, rpc::INTERNAL_ERROR, format!("kiln is unreachable: {e:#}")).answer()).collect();
                match rpc::reply(body.batch, answers) {
                    Some(a) => a.to_string(),
                    None => continue,
                }
            }
        };
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "{}", out.trim())?;
        stdout.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The operations docs/ci.md §4.1 names, each once: a row's `.x` is the last full name's
    /// area's `x` (`ci.caches.list`, `.delete`).
    fn spec_operations(doc: &str) -> Vec<String> {
        let start = doc.find("### 4.1 Routes").unwrap_or(0);
        let end = doc.get(start..).and_then(|d| d.find("### 4.2")).map_or(doc.len(), |e| start + e);
        let mut out: Vec<String> = Vec::new();
        for line in doc.get(start..end).unwrap_or_default().lines().filter(|l| l.starts_with("| ") && !l.starts_with("| Method") && !l.starts_with("| ---")) {
            let cells: Vec<String> = line.replace("\\|", "\u{1}").split('|').map(|c| c.trim().to_owned()).collect();
            let Some(names) = cells.get(3) else { continue };
            let mut last = String::new();
            for tick in names.split('`').skip(1).step_by(2) {
                let name = match tick.strip_prefix('.') {
                    Some(rest) if !last.is_empty() => format!("{}.{rest}", last.rsplit_once('.').map_or(last.as_str(), |(area, _)| area)),
                    _ => tick.to_owned(),
                };
                let dotted = name.contains('.') && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._*".contains(c));
                if dotted && !name.starts_with('.') {
                    if !out.contains(&name) {
                        out.push(name.clone());
                    }
                    last = name;
                }
            }
        }
        out
    }

    /// What code mode's `kiln` is made of: its names, and nothing to call.
    struct Listed(Vec<String>);

    impl code::Host for Listed {
        fn names(&self) -> Vec<String> {
            self.0.clone()
        }

        fn registry(&self) -> Value {
            Value::Array(Vec::new())
        }

        fn call(&self, _: &str, _: Value) -> Result<Value, Value> {
            Err(json!({"error": "not here"}))
        }
    }

    /// What §4.1 names that isn't built yet: W24's work operations, specified before they're built.
    const LEFT: &[&str] = &["ci.work.gc", "ci.work.reap.run", "ci.work.status", "ci.work.verify", "ci.work.warm", "ci.work.why"];

    /// W20-AC01: every operation docs/ci.md §4.1 names is in the one registry (what isn't built
    /// yet, [`LEFT`], aside), and every operation of the registry is an MCP tool and a function of
    /// code mode's `kiln` by the same name. Each is its HTTP route too, and `kiln op` runs any of
    /// them: `ops::tests` and `ci::ops::tests` check those.
    #[test]
    fn w20_ac01_every_operation_on_every_surface() -> Result<(), String> {
        let doc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/ci.md")).map_err(|e| e.to_string())?;
        let named = spec_operations(&doc);
        assert!(named.len() > 120, "§4.1 read: {named:?}");
        let ops = crate::ops::listed(true);
        let names: Vec<&str> = ops.iter().map(|o| o.name).collect();
        let mut missing: Vec<&str> = named
            .iter()
            .map(String::as_str)
            .filter(|n| match n.strip_suffix(".*") {
                Some(area) => !names.iter().any(|o| o.starts_with(&format!("{area}."))),
                None => !names.contains(n),
            })
            .collect();
        missing.sort_unstable();
        assert_eq!(missing, LEFT, "what §4.1 names and the registry lacks");
        // Each its tool, and no two the same.
        let tools: Vec<String> = tools_of(crate::ops::listed(true)).iter().filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_owned)).collect();
        for n in &names {
            assert!(tools.contains(&tool_name(n)), "{n} has no tool");
        }
        let mut unique = tools.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), tools.len(), "two tools share a name");
        assert_eq!(tools.len(), names.len() + 2, "code_search and code_run, then one each");
        // Each a function of code mode's `kiln`, by its name.
        let all: Vec<String> = names.iter().map(|n| (*n).to_owned()).collect();
        let program = "return input.filter(n => typeof n.split('.').reduce((o, k) => o && o[k], kiln) !== 'function')";
        let out = code::run_on(std::sync::Arc::new(Listed(all.clone())), code::Mode::Run { input: json!(all) }, program, code::Limits::default()).map_err(|e| e.message)?;
        assert_eq!(out.get("result").cloned().unwrap_or(out.clone()), json!([]), "not functions of `kiln`");
        Ok(())
    }

    #[test]
    fn mcp_tokens_verify_only_with_the_key_they_were_minted_with() {
        let t = token("admin-secret", "session-42");
        assert_eq!(verify("admin-secret", &t).as_deref(), Some("session-42"));
        assert_eq!(verify("other-secret", &t), None);
        assert_eq!(verify("admin-secret", "kiln_mcp_session-42_00"), None);
        assert_eq!(verify("admin-secret", "Bearer x"), None);
    }

    #[test]
    fn every_operation_is_a_tool_with_a_valid_name() {
        let tools = tools_of(crate::ops::listed(true));
        assert!(tools.iter().any(|t| t.get("name").and_then(Value::as_str) == Some("ci_legs_exec")), "CI's operations are tools (W20-R04)");
        assert!(tools.len() > 10);
        for t in &tools {
            let name = t.get("name").and_then(Value::as_str).unwrap_or("");
            assert!(!name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'), "{name}");
            assert!(t.get("inputSchema").and_then(|s| s.get("type")).is_some(), "{name}");
        }
    }

    fn open(id: &str) -> crate::harness::core::Update {
        crate::harness::core::Update::Open { info: Box::new(crate::harness::session::Info { id: id.into(), kind: "claude".into(), ..Default::default() }), since: None }
    }

    /// A value of the wrong type for a property whose schema is `s`: a number for a string, a
    /// string for the rest; none for one of several kinds (`anyOf`).
    fn wrong_for(s: &Value) -> Option<Value> {
        let types: Vec<&str> = match s.get("type")? {
            Value::String(t) => vec![t.as_str()],
            Value::Array(ts) => ts.iter().filter_map(Value::as_str).collect(),
            _ => return None,
        };
        if types.contains(&"string") { Some(json!(1)) } else { Some(json!("x")) }
    }

    /// Each of kiln's own operations publishes its input type's schema (as an MCP tool, and in
    /// code mode's registry), and reads each input as that type: a field it doesn't take is
    /// refused (its schema says `additionalProperties: false`), and so is one of the wrong type,
    /// each saying where.
    #[test]
    fn every_own_operation_publishes_its_input_type_and_reads_by_it() {
        let tools = tools_of(crate::ops::listed(false));
        let registry = code::registry_of(&crate::ops::listed(false));
        let mut differ = Vec::new();
        for op in crate::ops::own() {
            let schema = op.input.schema();
            let tool = tools.iter().find(|t| t.get("name").and_then(Value::as_str) == Some(tool_name(op.name).as_str()));
            let listed = registry.as_array().and_then(|r| r.iter().find(|o| o.get("name").and_then(Value::as_str) == Some(op.name)));
            if tool.and_then(|t| t.get("inputSchema")) != Some(&schema) || listed.and_then(|o| o.get("input")) != Some(&schema) {
                differ.push(op.name);
            }
            assert_eq!(schema.get("additionalProperties"), Some(&json!(false)), "{}", op.name);
            let m = op.input.fits(&json!({"zz_not_a_field": 1}));
            assert!(m.as_ref().is_err_and(|m| m.path == "/zz_not_a_field" && m.code() == "unknown_field"), "{}: {m:?}", op.name);
            let wrong = schema.get("properties").and_then(Value::as_object).and_then(|p| p.iter().find_map(|(k, v)| wrong_for(v).map(|w| (k.clone(), w))));
            if let Some((field, w)) = wrong {
                let m = op.input.fits(&json!({ field.clone(): w }));
                assert!(m.as_ref().is_err_and(|m| m.path == format!("/{field}") && m.code() == "invalid_type"), "{}: {m:?}", op.name);
            }
        }
        assert!(differ.is_empty(), "published schemas that aren't their input type's: {differ:?}");
    }

    /// `/api/mcp` answers each message it can't read as JSON-RPC 2.0 and MCP say, and notes it
    /// against the agent its token names, on its page: what isn't JSON, what isn't a request
    /// (`{}`, each of `[1,2,3]`, one without a method), a method or a tool kiln doesn't have, a
    /// list's cursor that isn't one. An argument an operation doesn't take, or of the wrong type,
    /// is a tool error saying where and why, noted too. No notification is answered, and one no
    /// client sends is noted all the same.
    #[test]
    fn what_mcp_cant_read_is_answered_as_json_rpc_says_and_noted() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            hub.harness.ingest(&hub, &crate::harness::registry::Location::Machine("box".into()), "s1", open("s1"));
            let mut agent = Caller::with(crate::authz::gate::Kind::Agent, "agent:s1", None, None);
            agent.agent = Some("s1".into());
            let ask = |body: &'static str| answer(&hub, agent.clone(), body.as_bytes());
            let counted = |code: &str| crate::metrics::wire().refused.get(&["mcp_http", code]);
            let (unknown, malformed) = (counted("unknown"), counted("malformed"));
            let code = |v: Option<Value>| v.and_then(|v| v.pointer("/error/code").and_then(Value::as_i64));
            anyhow::ensure!(code(ask("{").await) == Some(rpc::PARSE_ERROR));
            anyhow::ensure!(code(ask("{}").await) == Some(rpc::INVALID_REQUEST));
            let three = ask("[1,2,3]").await;
            let each = |e: &Value| e.pointer("/error/code") == Some(&json!(rpc::INVALID_REQUEST)) && e.get("id") == Some(&Value::Null);
            anyhow::ensure!(three.as_ref().and_then(Value::as_array).is_some_and(|a| a.len() == 3 && a.iter().all(each)), "{three:?}");
            anyhow::ensure!(code(ask(r#"{"jsonrpc":"2.0","id":1}"#).await) == Some(rpc::INVALID_REQUEST), "no method");
            anyhow::ensure!(code(ask(r#"{"jsonrpc":"2.0","id":2,"method":"teleport"}"#).await) == Some(rpc::METHOD_NOT_FOUND));
            anyhow::ensure!(code(ask(r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"teleport"}}"#).await) == Some(rpc::INVALID_PARAMS));
            let at = |v: &Option<Value>, p: &str| v.as_ref().and_then(|v| v.pointer(p)).cloned();
            let golas = ask(r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"goals_add","arguments":{"golas":["x"]}}}"#).await;
            anyhow::ensure!(at(&golas, "/result/isError") == Some(json!(true)) && at(&golas, "/result/structuredContent/code") == Some(json!("unknown_field")) && at(&golas, "/result/structuredContent/path") == Some(json!("/golas")), "{golas:?}");
            let typed = ask(r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"goals_add","arguments":{"goals":"x"}}}"#).await;
            anyhow::ensure!(at(&typed, "/result/structuredContent/code") == Some(json!("invalid_type")) && at(&typed, "/result/structuredContent/path") == Some(json!("/goals")), "{typed:?}");
            anyhow::ensure!(ask(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).await.is_none(), "a notification isn't answered");
            anyhow::ensure!(ask(r#"{"jsonrpc":"2.0","method":"notifications/teleport"}"#).await.is_none(), "nor is one no client sends");
            anyhow::ensure!(code(ask(r#"{"jsonrpc":"2.0","id":6,"method":"tools/list","params":{"cursor":5}}"#).await) == Some(rpc::INVALID_PARAMS), "a cursor is a string");
            // Each counted, noted against the agent, on its page, and audited naming it: the
            // method, the tool and the notification `unknown`, the other nine `malformed`.
            anyhow::ensure!(counted("unknown") >= unknown + 3 && counted("malformed") >= malformed + 9, "counted");
            let kept = hub.harness.list().into_iter().find(|i| i.id == "s1").and_then(|i| i.refused);
            anyhow::ensure!(kept.as_ref().map(crate::wire::unread::Kept::total) == Some(12), "{kept:?}");
            let principal: String = hub.store.call(|c| Ok(c.query_row("SELECT principal FROM audit2 WHERE action = 'security.wire_refused' AND target = 'mcp_http' LIMIT 1", [], |r| r.get(0))?)).await?;
            anyhow::ensure!(principal == "agent:s1", "{principal}");
            Ok(())
        })
    }

    /// A body `/api/mcp` can't read whole is answered as JSON-RPC 2.0 has it, under a null id,
    /// and noted against the agent: one past the most it reads (413), and one cut short (400).
    #[test]
    fn a_body_it_cant_read_whole_is_refused_and_noted() -> anyhow::Result<()> {
        use http_body_util::BodyExt as _;
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            hub.harness.ingest(&hub, &crate::harness::registry::Location::Machine("box".into()), "s1", open("s1"));
            let mut agent = Caller::with(crate::authz::gate::Kind::Agent, "agent:s1", None, None);
            agent.agent = Some("s1".into());
            let said = |r: Response| async move {
                let status = r.status();
                let body = r.into_body().collect().await.map(|b| b.to_bytes()).unwrap_or_default();
                (status, serde_json::from_slice::<Value>(&body).ok())
            };
            let code = |v: &Option<Value>| v.as_ref().and_then(|v| v.pointer("/error/code")).and_then(Value::as_i64);
            // What `http` reads of a body past the most: `Limited`'s own error.
            let past = http_body_util::Limited::new(http_body_util::Full::new(bytes::Bytes::from_static(b"[1,2,3]")), 4).collect().await.map(|b| b.to_bytes());
            let (status, v) = said(respond(&hub, agent.clone(), past).await).await;
            anyhow::ensure!(status == StatusCode::PAYLOAD_TOO_LARGE && code(&v) == Some(rpc::INVALID_REQUEST) && v.as_ref().and_then(|v| v.get("id")) == Some(&Value::Null), "{status} {v:?}");
            let (status, v) = said(respond(&hub, agent.clone(), Err("connection reset".into())).await).await;
            anyhow::ensure!(status == StatusCode::BAD_REQUEST && code(&v) == Some(rpc::PARSE_ERROR), "{status} {v:?}");
            let kept = hub.harness.list().into_iter().find(|i| i.id == "s1").and_then(|i| i.refused);
            anyhow::ensure!(kept.as_ref().map(crate::wire::unread::Kept::total) == Some(2), "{kept:?}");
            Ok(())
        })
    }

    /// Through reach (`ReachAsk`): word from a session's MCP server on a runner of what it couldn't
    /// read is noted against the session, on its page and the runner's; a call whose input doesn't
    /// fit is answered saying where and why, and noted the same way. Word of a session that runs
    /// elsewhere is refused, and not noted.
    #[test]
    fn through_reach_what_couldnt_be_read_is_noted_against_its_session() -> anyhow::Result<()> {
        use crate::reach::Question;
        use crate::wire::unread::{Code, Kept};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            for name in ["box", "mac"] {
                hub.agents.lock().insert(name.into(), crate::hub::tests::live_runner(name, "linux", "x86_64"));
            }
            hub.harness.ingest(&hub, &crate::harness::registry::Location::Machine("box".into()), "s1", open("s1"));
            let unread = Question::Unread { session: "s1".into(), what: "tools/call teleport".into(), code: Code::Unknown, why: "Unknown tool: teleport".into() };
            let a = runner_ask(&hub, "box", &unread).await;
            anyhow::ensure!(a.get("ok").is_some(), "{a}");
            let elsewhere = runner_ask(&hub, "mac", &unread).await;
            anyhow::ensure!(elsewhere.get("error").is_some(), "{elsewhere}");
            let call = Question::Call { session: "s1".into(), terminal: String::new(), name: "goals.add".into(), input: json!({"golas": ["x"]}) };
            let a = runner_ask(&hub, "box", &call).await;
            anyhow::ensure!(a.get("code") == Some(&json!("unknown_field")) && a.get("path") == Some(&json!("/golas")) && a.get("status") == Some(&json!(400)), "{a}");
            let session = hub.harness.list().into_iter().find(|i| i.id == "s1").and_then(|i| i.refused);
            anyhow::ensure!(session.as_ref().map(Kept::total) == Some(2), "the session's page: {session:?}");
            let runner = hub.agent_list().await.into_iter().find(|r| r.name == "box").and_then(|r| r.refused);
            anyhow::ensure!(runner.as_ref().map(Kept::total) == Some(2), "the runner's page: {runner:?}");
            let principal: String = hub.store.call(|c| Ok(c.query_row("SELECT principal FROM audit2 WHERE action = 'security.wire_refused' AND target = 'mcp_reach' LIMIT 1", [], |r| r.get(0))?)).await?;
            anyhow::ensure!(principal == "agent:s1", "{principal}");
            Ok(())
        })
    }

    /// An agent kiln runs is told who it is and how its goal stack and plans work; anyone else
    /// hears what kiln is.
    #[test]
    fn an_agent_hears_who_it_is_and_of_its_goals_and_plans() {
        let mut agent = Caller::with(crate::authz::gate::Kind::Agent, "agent:01a14ab9", None, None);
        agent.agent = Some("01a14ab9693976deb6ff6825f7da1c42".into());
        let told = instructions(&agent);
        assert!(told.starts_with(INSTRUCTIONS) && told.contains("You are kiln agent 01a14ab9693976deb6ff6825f7da1c42") && told.contains("goals_add") && told.contains("plans_run"), "{told}");
        assert_eq!(instructions(&Caller::owner()), INSTRUCTIONS);
        assert!(skills::list().iter().any(|s| s.get("name").and_then(Value::as_str) == Some("kiln-plans")), "the skill the instructions name is there");
    }
}

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

/// What a runner asks for one of its agent sessions' MCP server (`kiln reach mcp`, whose code
/// mode runs on the runner, `crate::reach::mcp`): `{"op": "registry", "session"}` for the
/// operations, `{"op": "call", "session", "name", "input"}` to run one, or `{"op": "page",
/// "session", "do", ...}` for a page it shows (`crate::harness::visuals::ask`). The session must be one
/// kiln knows on that runner, which mustn't be a job's; it acts for whoever started it (the owner
/// on a kiln nobody else signs in to), audited as `agent:<session>@<runner> for <them>`. The
/// answer: `{"registry", "skills"}`, `{"ok": <answer>}`, or `{"error", "status"?}`.
pub async fn runner_ask(hub: &Arc<Hub>, runner: &str, ask: &Value) -> Value {
    if hub.agents.lock().get(runner).is_none_or(|a| a.ephemeral || a.outpost) {
        return json!({"error": "kiln's operations are for its agents' runners, not a job's"});
    }
    let session = ask.get("session").and_then(Value::as_str).unwrap_or("");
    let terminal = ask.get("terminal").and_then(Value::as_str).unwrap_or("");
    // The person in one of its kiln shells (`kiln spawn --on`, `kiln agents wait`), for a call.
    let caller = if session.is_empty() && !terminal.is_empty() && ask.get("op").and_then(Value::as_str) == Some("call") {
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
    let answer = match ask.get("op").and_then(Value::as_str) {
        Some("registry") => {
            let skills: Vec<Value> = skills::list().into_iter().map(|s| json!({"name": s.get("name"), "description": s.get("description")})).collect();
            json!({"registry": code::registry(hub), "skills": skills})
        }
        Some("call") => {
            let name = ask.get("name").and_then(Value::as_str).unwrap_or("");
            let input = ask.get("input").cloned().unwrap_or(Value::Null);
            match crate::ops::call(crate::ops::Cx { hub: Arc::clone(hub), caller }, name, input).await {
                Ok(v) => json!({"ok": v}),
                Err((status, msg)) => json!({"error": msg, "status": status.as_u16()}),
            }
        }
        // A page it shows in its conversation (`crate::harness::visuals`), or one it streams
        // live there (`crate::harness::streams`): its own session's, audited as the agent acting
        // for whoever started it.
        Some(op @ ("page" | "stream")) => {
            let r = if op == "page" { crate::harness::visuals::ask(hub, session, ask).await } else { crate::harness::streams::ask(hub, session, ask).await };
            let what = format!("{}.{}", if op == "page" { "pages" } else { "streams" }, ask.get("do").and_then(Value::as_str).unwrap_or("?"));
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
        _ => json!({"error": "an unknown question"}),
    };
    let size = answer.to_string().len();
    if size > RUNNER_ANSWER_MAX {
        return json!({"error": format!("the answer is {size} bytes, more than the {RUNNER_ANSWER_MAX} a runner is sent: filter or page on the server")});
    }
    answer
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
Goal stacks and plans are under `kiln.goals` and `kiln.plans`. Prefer code_run: one program can list, filter and act on a \
whole fleet and return only what matters. code_search shows the operations and their inputs. The skills (kiln-agents, \
kiln-plans, kiln-ci-diagnose, kiln-deploy, kiln-diagnose, kiln-apps, kiln-argocd-migration) say how to use them.";

/// What kiln's MCP servers (this one and the runners', `crate::reach::mcp`) tell an agent about
/// its own goal stack and plans.
pub const GOALS_AND_PLANS: &str = "kiln keeps a goal stack for you: `goals_add` lines goals up (leave out `agent`: they're \
yours), one condition or objective each, in order, and kiln gives you each in turn and holds you to it until it's met, then \
the next, so you carry on without a person. To pursue a document or a list as a goal stack, add one goal per item (every \
item asked for), each saying what done is, then end your turn: the first comes to you as soon as you're free. `goals_get` \
says where it stands; `goals_control` pauses, skips or clears it. A plan is \
kept in kiln for the person to read, change and run again: `plans_create` keeps one (its top-level list items are its \
steps), and `plans_run` runs it on you (leave out `session` and `new`: as one message, a step a turn, or each step a goal) \
or with an agent of its own for each step (mode `agents`: in your harness, on your machine and in your folder unless you \
say otherwise; `plans_runs_get` follows the run). The kiln-plans skill says how.";

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

/// `POST /api/mcp`: one JSON-RPC message or a batch, each operation as `caller` may (W20-R06).
pub async fn http(hub: &Arc<Hub>, caller: Caller, body: hyper::body::Incoming) -> Result<Response, (StatusCode, String)> {
    let bytes = srv::body_bytes(body, 16 << 20).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let msg: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => return Ok(srv::json(StatusCode::OK, &rpc_error(Value::Null, -32700, &format!("parse error: {e}")))),
    };
    let server = Server { hub: hub.clone(), caller };
    let answer = match msg {
        Value::Array(batch) => {
            let mut out = Vec::new();
            for m in batch {
                if let Some(a) = server.handle(m).await {
                    out.push(a);
                }
            }
            if out.is_empty() { None } else { Some(Value::Array(out)) }
        }
        m => server.handle(m).await,
    };
    Ok(match answer {
        Some(a) => srv::json(StatusCode::OK, &a),
        None => srv::status(StatusCode::ACCEPTED),
    })
}

fn rpc_ok(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
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

    async fn handle(&self, msg: Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        // Notifications get no answer.
        let id = match id {
            Some(id) if !id.is_null() => id,
            _ => return None,
        };
        Some(match method {
            "initialize" => {
                let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
                let version = VERSIONS.iter().find(|v| **v == asked).copied().unwrap_or(VERSIONS.first().copied().unwrap_or("2025-11-25"));
                rpc_ok(
                    id,
                    json!({
                        "protocolVersion": version,
                        "capabilities": {
                            "tools": {"listChanged": false},
                            "resources": {"listChanged": false},
                            "extensions": {SKILLS_EXTENSION: {}},
                        },
                        "serverInfo": {"name": "kiln", "title": "kiln", "version": env!("CARGO_PKG_VERSION")},
                        "instructions": instructions(&self.caller),
                    }),
                )
            }
            "ping" => rpc_ok(id, json!({})),
            "tools/list" => rpc_ok(id, json!({"tools": tools(&self.hub)})),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                rpc_ok(id, self.call_tool(name, args).await)
            }
            "resources/list" => rpc_ok(id, json!({"resources": skills::resources()})),
            "resources/read" => {
                let uri = params.get("uri").and_then(Value::as_str).unwrap_or("");
                match skills::read(uri) {
                    Some(c) => rpc_ok(id, json!({"contents": [c]})),
                    None => rpc_error(id, -32002, &format!("no resource {uri}")),
                }
            }
            "skills/list" => rpc_ok(id, json!({"skills": skills::list()})),
            "skills/get" => {
                let key = params.get("uri").or_else(|| params.get("name")).and_then(Value::as_str).unwrap_or("");
                match skills::get(key) {
                    Some(s) => rpc_ok(id, s),
                    None => rpc_error(id, -32002, &format!("no skill {key}")),
                }
            }
            "resources/templates/list" => rpc_ok(id, json!({"resourceTemplates": []})),
            "prompts/list" => rpc_ok(id, json!({"prompts": []})),
            other => rpc_error(id, -32601, &format!("method not found: {other}")),
        })
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
    use std::io::{BufRead as _, Write as _};
    let client = crate::http::client::Client::public()?;
    let (tx, rx) = async_channel::bounded::<String>(64);
    // stdin is read on a thread of its own: it blocks.
    std::thread::Builder::new().name("kiln-mcp-stdin".into()).spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            if !line.trim().is_empty() && tx.send_blocking(line).is_err() {
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
            Err(e) => {
                let id = serde_json::from_str::<Value>(&line).ok().and_then(|v| v.get("id").cloned()).unwrap_or(Value::Null);
                rpc_error(id, -32603, &format!("kiln is unreachable: {e:#}")).to_string()
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

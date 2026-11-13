//! kiln's MCP for an isolated session's harness (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 18,
//! part 3): over the agent-control link's own `mcp` lane (Shards D254), never the network.
//!
//! Inside the microVM Shards gives each member a stdio bridge, [`BRIDGE`] (shards-init's own binary,
//! read-only in the member's view), which carries one connection of its harness's MCP to the run's
//! controller as a byte stream each way, newline-delimited JSON-RPC as `kiln reach mcp` speaks it
//! (D254). kiln's runner serves kiln's MCP over that stream in its own process, with no socket of the
//! machine, on every OS. So each harness that takes MCP takes kiln's as a stdio server naming that
//! command, with no arguments, environment or token: the lane says which member asks, and nothing in
//! the microVM, its person's volume, the stored spec or a log holds a token for it. No Agentfile grants
//! anything for it (`agentfiles::check` refuses a grant that reaches kiln itself).
//!
//! Each form is the harness's own documented one, as kiln gives it on its machine (`reach::mcp`):
//! Claude Code's `--mcp-config`, Codex's `-c mcp_servers.*`, an ACP agent's `session/new`
//! `mcpServers` (stdio, which every ACP agent takes), and Antigravity's own `mcp_config.json`, the
//! only place agy reads a server from, which is in its person's volume and so the same for every
//! session: one fixed entry suits it, as the lane, not the entry, names the session (goal 65's probe,
//! docs/research/s35-every-harness/agy-mcp). Pi takes no MCP server.

use serde_json::{Value, json};

use crate::harness::kinds::{Kind, Protocol};

/// The member's MCP bridge inside its microVM (Shards D254: its command, with no arguments,
/// environment or token).
pub const BRIDGE: &str = "/run/shards/mcp";

/// kiln's MCP as one harness takes it in a microVM.
#[derive(Debug, Clone, PartialEq)]
pub enum Config {
    /// Arguments its command line gets.
    Args(Vec<String>),
    /// An ACP agent's `session/new` `mcpServers` entry.
    Acp(Value),
    /// The entry its own config file keeps under `mcpServers.kiln`, the same for every session
    /// (Antigravity's `mcp_config.json`, put there by [`agy_config`]).
    Shared(Value),
    /// It takes none, and why.
    Refused(&'static str),
}

/// kiln's MCP, through the lane's bridge, as `kind`'s harness takes it.
pub fn for_harness(kind: &Kind) -> Config {
    match kind.protocol {
        Protocol::ClaudeStreamJson => Config::Args(vec!["--mcp-config".to_owned(), json!({"mcpServers": {"kiln": {"type": "stdio", "command": BRIDGE, "args": []}}}).to_string()]),
        Protocol::CodexAppServer => {
            let q = |s: &str| serde_json::to_string(s).unwrap_or_default();
            Config::Args(vec![
                "-c".to_owned(),
                format!("mcp_servers.kiln.command={}", q(BRIDGE)),
                "-c".to_owned(),
                "mcp_servers.kiln.args=[]".to_owned(),
                "-c".to_owned(),
                format!("mcp_servers.kiln.tool_timeout_sec={}", crate::reach::codex_tool_timeout_sec()),
            ])
        }
        Protocol::Acp(_) => Config::Acp(json!({"name": "kiln", "command": BRIDGE, "args": [], "env": []})),
        Protocol::AgyStreamJson => Config::Shared(json!({"command": BRIDGE, "args": []})),
        Protocol::PiRpc => Config::Refused("Pi takes no MCP server"),
        Protocol::Terminal => Config::Refused("it runs as a terminal program, with no structured mode"),
    }
}

/// Antigravity's `mcp_config.json` (`config`, as its person's volume holds it; `Null` when it has
/// none) with kiln's entry through the bridge as `mcpServers.kiln`, its other servers and settings
/// kept: None when it holds that entry already, when a person turned kiln's off (`agy mcp disable
/// kiln`, kept), or when it isn't a JSON object agy wrote (left as it is).
pub fn agy_config(config: &Value) -> Option<Value> {
    let Config::Shared(entry) = for_harness(Kind::get("antigravity")?) else { return None };
    let mut out = match config {
        Value::Null => json!({}),
        Value::Object(_) => config.clone(),
        _ => return None,
    };
    let servers = out.as_object_mut()?.entry("mcpServers").or_insert_with(|| json!({}));
    let servers = servers.as_object_mut()?;
    match servers.get("kiln") {
        Some(s) if s.get("disabled").and_then(Value::as_bool) == Some(true) => return None,
        Some(s) if s.get("command") == entry.get("command") && s.get("args") == entry.get("args") => return None,
        _ => {}
    }
    servers.insert("kiln".to_owned(), entry);
    Some(out)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{BRIDGE, Config, agy_config, for_harness};
    use crate::harness::kinds::Kind;

    fn kind(id: &str) -> anyhow::Result<&'static Kind> {
        Kind::get(id).ok_or_else(|| anyhow::anyhow!("no kind {id}"))
    }

    /// Every harness that takes MCP gets kiln's as a stdio server naming the lane's bridge, in its own
    /// form, with no token, no environment and no address: the lane says which member asks. Codex's
    /// tool calls wait as long as on its machine. Pi takes none. This runs on every target, Windows'
    /// included, where a harness has kiln's MCP only this way.
    #[test]
    fn step18_each_harness_gets_kilns_mcp_through_the_lanes_bridge() -> anyhow::Result<()> {
        let Config::Args(args) = for_harness(kind("claude")?) else { anyhow::bail!("claude") };
        anyhow::ensure!(args.first().map(String::as_str) == Some("--mcp-config"), "{args:?}");
        let server: Value = serde_json::from_str(args.get(1).map(String::as_str).unwrap_or_default())?;
        anyhow::ensure!(server == json!({"mcpServers": {"kiln": {"type": "stdio", "command": BRIDGE, "args": []}}}), "{server}");
        let Config::Args(args) = for_harness(kind("codex")?) else { anyhow::bail!("codex") };
        let want = ["-c", "mcp_servers.kiln.command=\"/run/shards/mcp\"", "-c", "mcp_servers.kiln.args=[]", "-c"];
        anyhow::ensure!(args.get(..want.len()).is_some_and(|a| a.iter().zip(want).all(|(x, y)| x == y)), "{args:?}");
        anyhow::ensure!(args.last() == Some(&format!("mcp_servers.kiln.tool_timeout_sec={}", crate::reach::codex_tool_timeout_sec())), "{args:?}");
        for acp in ["devin", "gemini", "opencode", "goose", "amp", "cursor"] {
            let Config::Acp(s) = for_harness(kind(acp)?) else { anyhow::bail!("{acp}") };
            anyhow::ensure!(s == json!({"name": "kiln", "command": BRIDGE, "args": [], "env": []}), "{acp}: {s}");
        }
        anyhow::ensure!(for_harness(kind("antigravity")?) == Config::Shared(json!({"command": BRIDGE, "args": []})));
        anyhow::ensure!(matches!(for_harness(kind("pi")?), Config::Refused(_)));
        for k in ["claude", "codex", "devin", "antigravity"] {
            let said = format!("{:?}", for_harness(kind(k)?));
            anyhow::ensure!(!said.contains("token") && !said.contains("Bearer") && !said.contains("KILN_") && !said.contains("http"), "{k}: {said}");
        }
        Ok(())
    }

    /// agy's shared config gets kiln's entry once, with its other servers kept; one a person turned off
    /// stays off, and a file agy didn't write is left alone.
    #[test]
    fn step18_agys_shared_config_names_the_bridge_once() -> anyhow::Result<()> {
        let fresh = agy_config(&Value::Null).ok_or_else(|| anyhow::anyhow!("none for no file"))?;
        anyhow::ensure!(fresh == json!({"mcpServers": {"kiln": {"command": BRIDGE, "args": []}}}), "{fresh}");
        anyhow::ensure!(agy_config(&fresh).is_none(), "written twice");
        let theirs = json!({"mcpServers": {"github": {"command": "gh-mcp"}, "kiln": {"command": "/old/kiln", "args": ["reach", "mcp"]}}, "theme": "dark"});
        let next = agy_config(&theirs).ok_or_else(|| anyhow::anyhow!("an old entry kept"))?;
        anyhow::ensure!(next == json!({"mcpServers": {"github": {"command": "gh-mcp"}, "kiln": {"command": BRIDGE, "args": []}}, "theme": "dark"}), "{next}");
        anyhow::ensure!(agy_config(&json!({"mcpServers": {"kiln": {"command": "/old/kiln", "disabled": true}}})).is_none(), "a person's off undone");
        anyhow::ensure!(agy_config(&json!(["not", "agy's"])).is_none());
        Ok(())
    }
}

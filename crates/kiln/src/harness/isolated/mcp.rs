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
//! Claude Code's `--mcp-config`, Codex's `-c mcp_servers.*`, and an ACP agent's `session/new`
//! `mcpServers` (stdio, which every ACP agent takes, Antigravity's own server, `agy acp`, among
//! them). Pi takes no MCP server. Antigravity's headless stream-json, which kiln ran it with before,
//! read a server only from its own `mcp_config.json` in its person's volume, where kiln wrote one
//! fixed entry (goal 65's probe, docs/research/s35-every-harness/agy-mcp): [`forget_bridge`] takes
//! it out, as its session's own would start kiln's MCP a second time.

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
        Protocol::PiRpc => Config::Refused("Pi takes no MCP server"),
        Protocol::Terminal => Config::Refused("it runs as a terminal program, with no structured mode"),
    }
}

/// Antigravity's `mcp_config.json` (`config`, as its person's volume holds it) without the one fixed entry kiln wrote
/// there for its headless stream-json (`mcpServers.kiln`, the lane's bridge with no arguments), its other servers and
/// settings kept. Whether it had it.
pub fn forget_bridge(config: &mut Value) -> bool {
    let Some(servers) = config.get_mut("mcpServers").and_then(Value::as_object_mut) else { return false };
    servers.get("kiln").is_some_and(|s| s.get("command").and_then(Value::as_str) == Some(BRIDGE) && s.get("args").is_none_or(|a| *a == json!([]))) && servers.remove("kiln").is_some()
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{BRIDGE, Config, for_harness, forget_bridge};
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
        for acp in ["devin", "gemini", "opencode", "goose", "amp", "cursor", "antigravity"] {
            let Config::Acp(s) = for_harness(kind(acp)?) else { anyhow::bail!("{acp}") };
            anyhow::ensure!(s == json!({"name": "kiln", "command": BRIDGE, "args": [], "env": []}), "{acp}: {s}");
        }
        anyhow::ensure!(matches!(for_harness(kind("pi")?), Config::Refused(_)));
        for k in ["claude", "codex", "devin", "antigravity"] {
            let said = format!("{:?}", for_harness(kind(k)?));
            anyhow::ensure!(!said.contains("token") && !said.contains("Bearer") && !said.contains("KILN_") && !said.contains("http"), "{k}: {said}");
        }
        Ok(())
    }

    /// The one fixed entry kiln wrote in agy's shared config for its headless stream-json goes, once, with its other
    /// servers and settings kept; a server of the person's own named kiln, or a file that isn't agy's, is left alone.
    #[test]
    fn step18_agys_shared_config_forgets_the_bridge_kiln_wrote() -> anyhow::Result<()> {
        let mut config = json!({"mcpServers": {"kiln": {"command": BRIDGE, "args": []}, "github": {"command": "gh-mcp"}}, "theme": "dark"});
        anyhow::ensure!(forget_bridge(&mut config), "kept");
        anyhow::ensure!(config == json!({"mcpServers": {"github": {"command": "gh-mcp"}}, "theme": "dark"}), "{config}");
        anyhow::ensure!(!forget_bridge(&mut config), "twice");
        let mut theirs = json!({"mcpServers": {"kiln": {"command": "/opt/their-kiln", "args": ["serve"]}}});
        anyhow::ensure!(!forget_bridge(&mut theirs), "a person's own kiln server went");
        let mut other: Value = json!(["not", "agy's"]);
        anyhow::ensure!(!forget_bridge(&mut other));
        Ok(())
    }
}

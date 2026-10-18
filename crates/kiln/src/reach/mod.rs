//! Reach (`docs/reach.md`): an agent, a person or a script on one runner runs commands on another
//! runner — a Linux build from a Mac, a test on the machine with the GPU — over an end-to-end
//! encrypted link the server only relays.

pub mod cli;
pub mod directory;
pub mod e2e;
pub mod local;
#[cfg(unix)]
pub mod mcp;
pub mod node;
pub mod relay;
pub mod serve;
pub mod sync;
pub mod wire;

/// How the timeline shows a call of reach's MCP tools (server `kiln-reach`, or `kiln_reach` as
/// Codex names it): a command on another machine reads as a command, saying where.
pub fn describe(server: &str, tool: &str, input: &serde_json::Value) -> Option<(crate::harness::model::ToolKind, String, Option<String>)> {
    use crate::harness::model::ToolKind;
    if server != "kiln-reach" && server != "kiln_reach" {
        return None;
    }
    let s = |k: &str| input.get(k).and_then(serde_json::Value::as_str).map(str::to_owned).filter(|v| !v.trim().is_empty());
    Some(match tool {
        "run" => {
            let on = s("on").map_or_else(|| "on the best runner".to_owned(), |o| format!("on {o}"));
            let how = if input.get("background").and_then(serde_json::Value::as_bool) == Some(true) { " · in the background" } else { "" };
            (ToolKind::Shell, s("command").unwrap_or_default(), Some(format!("{on}{how}")))
        }
        "runners" => (ToolKind::Search, s("want").map_or_else(|| "Runners".to_owned(), |w| format!("Runners for {w}")), None),
        "output" => (ToolKind::Shell, format!("output of {}", s("id").unwrap_or_default()), None),
        "stop" => (ToolKind::Shell, format!("stop {}", s("id").unwrap_or_default()), None),
        _ => return None,
    })
}

#[cfg(test)]
mod tests;

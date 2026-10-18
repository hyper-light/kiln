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

/// What a reach tool returned, as the timeline shows a command's result: its output, its exit
/// code, and where it ran. `result` is the tool's answer (its JSON, as text or structured).
pub struct Shown {
    pub output: String,
    pub exit_code: Option<i32>,
    pub runner: Option<String>,
}

pub fn shown(tool_name: &str, result: &serde_json::Value) -> Option<Shown> {
    let tool = tool_name.strip_prefix("mcp__kiln-reach__").or_else(|| tool_name.strip_prefix("mcp__kiln_reach__"))?;
    if !matches!(tool, "run" | "output" | "stop") {
        return None;
    }
    // Text holding JSON, MCP content blocks, or the structured answer itself.
    let v = match result {
        serde_json::Value::String(s) => serde_json::from_str(s).ok()?,
        serde_json::Value::Object(o) if o.contains_key("structuredContent") => o.get("structuredContent").cloned()?,
        serde_json::Value::Object(o) if o.contains_key("content") => {
            let text = o.get("content").and_then(serde_json::Value::as_array)?.iter().find_map(|b| b.get("text").and_then(serde_json::Value::as_str))?;
            serde_json::from_str(text).ok()?
        }
        other => other.clone(),
    };
    let mut output = v.get("output").and_then(serde_json::Value::as_str).unwrap_or("").to_owned();
    if let Some(e) = v.get("error").and_then(serde_json::Value::as_str) {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(e);
    }
    if v.get("running").and_then(serde_json::Value::as_bool) == Some(true) {
        output.push_str(&format!("\n[still running in the background as {}]", v.get("id").and_then(serde_json::Value::as_str).unwrap_or("?")));
    }
    Some(Shown {
        output,
        exit_code: v.get("exit_code").and_then(serde_json::Value::as_i64).and_then(|c| i32::try_from(c).ok()),
        runner: v.get("runner").and_then(serde_json::Value::as_str).filter(|r| !r.is_empty()).map(str::to_owned),
    })
}

#[cfg(test)]
mod tests;

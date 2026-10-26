//! Reach (`docs/reach.md`): an agent, a person or a script on one runner runs commands on another
//! runner — a Linux build from a Mac, a test on the machine with the GPU — over an end-to-end
//! encrypted link the server only relays.

pub mod cli;
pub mod copies;
pub mod directory;
pub mod e2e;
pub mod keep;
pub mod keeper;
pub mod local;
#[cfg(unix)]
pub mod mcp;
pub mod node;
pub mod relay;
pub mod serve;
pub mod sync;
pub mod wire;

/// The MCP servers whose tools are reach's: a runner's own kiln server (`kiln`, `crate::reach::mcp`),
/// and `kiln-reach` (`kiln_reach` as Codex names it) as sessions from before it called it.
fn ours(server: &str) -> bool {
    matches!(server, "kiln" | "kiln-reach" | "kiln_reach")
}

/// How the timeline shows a call of reach's MCP tools: a command on another machine reads as a
/// command, saying where; a program reads as its first line.
pub fn describe(server: &str, tool: &str, input: &serde_json::Value) -> Option<(crate::harness::model::ToolKind, String, Option<String>)> {
    use crate::harness::model::ToolKind;
    if !ours(server) {
        return None;
    }
    let s = |k: &str| input.get(k).and_then(serde_json::Value::as_str).map(str::to_owned).filter(|v| !v.trim().is_empty());
    Some(match tool {
        "code_run" | "code_search" => {
            let program = s("program").unwrap_or_default();
            let first = program.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with("//")).unwrap_or("").chars().take(120).collect::<String>();
            let what = if tool == "code_run" { "kiln program" } else { "kiln operations" };
            (ToolKind::Mcp, if first.is_empty() { what.to_owned() } else { first }, Some(what.to_owned()))
        }
        "run" => {
            let on = s("on").map_or_else(|| "on the best runner".to_owned(), |o| format!("on {o}"));
            let how = if input.get("background").and_then(serde_json::Value::as_bool) == Some(true) { " · in the background" } else { "" };
            (ToolKind::Shell, s("command").unwrap_or_default(), Some(format!("{on}{how}")))
        }
        "runners" => (ToolKind::Search, s("want").map_or_else(|| "Runners".to_owned(), |w| format!("Runners for {w}")), None),
        "output" => (ToolKind::Shell, format!("output of {}", s("id").unwrap_or_default()), None),
        "stop" => (ToolKind::Shell, format!("stop {}", s("id").unwrap_or_default()), None),
        "show_page" => (ToolKind::Mcp, s("title").unwrap_or_else(|| "A page".to_owned()), None),
        "show_live" => (ToolKind::Web, s("title").or_else(|| s("url")).unwrap_or_else(|| "A live page".to_owned()), s("url")),
        "use_live" => {
            let n = input.get("actions").and_then(serde_json::Value::as_array).map_or(0, Vec::len);
            (ToolKind::Web, format!("Used the live page ({n} {})", if n == 1 { "action" } else { "actions" }), None)
        }
        "preview_page" => {
            let what = s("url").or_else(|| s("path")).unwrap_or_else(|| "a page".to_owned());
            let w = input.get("width").and_then(serde_json::Value::as_u64).unwrap_or(390);
            let h = input.get("height").and_then(serde_json::Value::as_u64).unwrap_or(844);
            (ToolKind::Web, format!("Preview {what}"), Some(format!("{w}×{h}")))
        }
        _ => return None,
    })
}

/// For a call of reach's `run`: where it goes, as said (a runner's name or what it must be, empty
/// for the best), so the timeline shows it as a command on another machine from the start.
pub fn remote(server: &str, tool: &str, input: &serde_json::Value) -> Option<String> {
    (ours(server) && tool == "run").then(|| input.get("on").and_then(serde_json::Value::as_str).unwrap_or("").trim().to_owned())
}

/// What a reach tool returned, as the timeline shows a command's result: its output, its exit
/// code, and where it ran. `result` is the tool's answer (its JSON, as text or structured).
pub struct Shown {
    pub output: String,
    pub exit_code: Option<i32>,
    pub runner: Option<String>,
}

pub fn shown(tool_name: &str, result: &serde_json::Value) -> Option<Shown> {
    let tool = tool_name.strip_prefix("mcp__kiln__").or_else(|| tool_name.strip_prefix("mcp__kiln-reach__")).or_else(|| tool_name.strip_prefix("mcp__kiln_reach__"))?;
    // A page shown or previewed: what it said, on one line (its picture is for the agent; the
    // conversation shows the page itself).
    if matches!(tool, "show_page" | "preview_page" | "show_live" | "use_live") {
        let text = match result {
            serde_json::Value::String(s) => s.lines().find(|l| l.starts_with('{')).map(str::to_owned),
            serde_json::Value::Object(o) => o
                .get("content")
                .and_then(serde_json::Value::as_array)
                .and_then(|c| c.iter().find_map(|b| b.get("text").and_then(serde_json::Value::as_str)))
                .map(str::to_owned)
                .or_else(|| o.get("structuredContent").map(serde_json::Value::to_string)),
            _ => None,
        }?;
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        return Some(Shown { output: v.to_string(), exit_code: None, runner: None });
    }
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

// Two nodes through the relay, over local sockets and bash: Unix.
#[cfg(all(test, unix))]
mod tests;

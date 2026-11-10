//! W20-R01: installed Agent Skills reach every structured harness through its prompt transport.
//! Claude loads its plugins natively; Codex receives native skill input items. ACP (Devin,
//! OpenCode, Gemini, Goose, Amp) and Antigravity receive the selected instructions with their
//! asset directory. Ordinary turns advertise metadata only, for progressive discovery.
//! No global harness settings or repository files are changed. Disk work runs on the blocking pool.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::harness::kinds::Protocol;

struct Skill {
    name: String,
    native_name: String,
    description: String,
    path: PathBuf,
    body: String,
    automatic: bool,
}

fn catalogue(folder: &Path) -> Vec<Skill> {
    let mut out = Vec::new();
    let Ok(packages) = std::fs::read_dir(folder) else { return out };
    for package in packages.flatten() {
        let name = package.file_name().to_string_lossy().into_owned();
        let Ok(skills) = std::fs::read_dir(package.path().join("skills")) else { continue };
        for skill in skills.flatten() {
            let path = skill.path().join("SKILL.md");
            let body = match std::fs::File::open(&path).and_then(|file| {
                use std::io::Read as _;
                let mut body = String::new();
                file.take(super::detect::MAX_FILE + 1).read_to_string(&mut body)?;
                Ok(body)
            }) {
                Ok(body) if body.len() as u64 <= super::detect::MAX_FILE => body,
                _ => continue,
            };
            let native_name = super::detect::front(&body, "name");
            let native_name = if native_name.is_empty() { skill.file_name().to_string_lossy().into_owned() } else { native_name };
            let description = crate::harness::model::clip_middle(&super::detect::front(&body, "description"), 300).0;
            let automatic = super::detect::front(&body, "disable-model-invocation") != "true";
            out.push(Skill { name: format!("{name}:{}", skill.file_name().to_string_lossy()), native_name, description, path, body, automatic });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn input_pointer(protocol: Protocol) -> Option<&'static str> {
    match protocol {
        Protocol::CodexAppServer => Some("/params/input"),
        Protocol::Acp(_) => Some("/params/prompt"),
        Protocol::AgyStreamJson => Some("/message/content"),
        Protocol::ClaudeStreamJson | Protocol::Terminal => None,
    }
}

fn is_prompt(protocol: Protocol, value: &Value) -> bool {
    match protocol {
        Protocol::CodexAppServer => matches!(value.get("method").and_then(Value::as_str), Some("turn/start" | "turn/steer")),
        Protocol::Acp(_) => value.get("method").and_then(Value::as_str) == Some("session/prompt"),
        Protocol::AgyStreamJson => value.get("event").and_then(Value::as_str) == Some("user"),
        Protocol::ClaudeStreamJson | Protocol::Terminal => false,
    }
}

/// Whether any line needs the scoped catalogue; responses and control traffic never touch disk.
pub fn needed(protocol: Protocol, writes: &[String]) -> bool {
    // Core::write emits compact JSON. Keep large tool replies off the parser on the runtime shard.
    writes.iter().any(|w| match protocol {
        Protocol::CodexAppServer => w.contains("\"method\":\"turn/start\"") || w.contains("\"method\":\"turn/steer\""),
        Protocol::Acp(_) => w.contains("\"method\":\"session/prompt\""),
        Protocol::AgyStreamJson => w.contains("\"event\":\"user\""),
        Protocol::ClaudeStreamJson | Protocol::Terminal => false,
    })
}

fn metadata(skills: &[Skill]) -> String {
    let mut text = String::from("<kiln_installed_skills>\nThis is the current skill catalogue for this session; it replaces earlier catalogues. Use a skill when its description matches the task or the user names it. Read its SKILL.md before using it, then load only the supporting files needed. Resolve relative files and scripts from that skill's directory. Explicit user instructions take priority over skill guidance. Skills omitted from this current catalogue are no longer active.\n");
    for skill in skills.iter().filter(|s| s.automatic) {
        text.push_str(&json!({"name": skill.name, "description": skill.description, "path": skill.path}).to_string());
        text.push('\n');
    }
    text.push_str("</kiln_installed_skills>");
    text
}

fn adapt(protocol: Protocol, native: bool, mut value: Value, skills: &[Skill]) -> Value {
    if !is_prompt(protocol, &value) { return value }
    let Some(pointer) = input_pointer(protocol) else { return value };
    let Some(input) = value.pointer_mut(pointer) else { return value };
    let resolve = |text: &str| {
        text.strip_prefix(['/', '$']).and_then(|r| r.split_once(char::is_whitespace).or(Some((r, ""))))
            .and_then(|(name, args)| skills.iter().find(|s| s.name == name).map(|s| (s, args.trim().to_owned())))
    };
    // ACP can put carried conversation context before the user's request.
    let selected = if let Some(text) = input.as_str() {
        resolve(text).map(|(skill, args)| (skill, args, None))
    } else {
        input.as_array().and_then(|items| items.iter().enumerate().find_map(|(i, item)| {
            if item.get("type").and_then(Value::as_str) != Some("text") { return None }
            item.get("text").and_then(Value::as_str).and_then(resolve).map(|(skill, args)| (skill, args, Some(i)))
        }))
    };
    // Native slash commands (notably ACP /compact) must still be first in the prompt.
    let native_command = |text: &str| text.starts_with(['/', '$']);
    if selected.is_none() && (input.as_str().is_some_and(native_command) || input.as_array().is_some_and(|a| a.iter().any(|v| v.get("type").and_then(Value::as_str) == Some("text") && v.get("text").and_then(Value::as_str).is_some_and(native_command)))) { return value }
    let mut prefix = metadata(skills);
    if let Some((skill, args, index)) = selected {
        let invocation = format!("Use the installed skill {}. Its base directory is {}.\nUser arguments: {args}", skill.name, skill.path.parent().unwrap_or_else(|| Path::new("")).display());
        if let Some(items) = input.as_array_mut() {
            // Keep images/resource links and their order; only replace the invocation's text.
            if let Some(text) = index.and_then(|i| items.get_mut(i)) {
                *text = json!({"type": "text", "text": invocation});
            }
            if protocol == Protocol::CodexAppServer && native {
                items.push(json!({"type": "skill", "name": skill.native_name, "path": skill.path}));
            }
            prefix.push_str(&format!("\n<kiln_selected_skill>\n{}\n</kiln_selected_skill>", skill.body));
        } else if input.is_string() {
            prefix.push_str(&format!("\n<kiln_selected_skill>\n{}\n</kiln_selected_skill>", skill.body));
            *input = Value::String(invocation);
        }
    }
    // Even an empty catalogue revokes a previous turn's skills after disable/remove.
    if let Some(items) = input.as_array_mut() {
        items.insert(0, json!({"type": "text", "text": prefix}));
    } else if let Some(text) = input.as_str() {
        *input = Value::String(format!("{prefix}\n\n{text}"));
    }
    value
}

/// Read the session's current package links at delivery, including queued prompts. Blocking.
pub fn deliver(folder: &Path, protocol: Protocol, native: bool, writes: Vec<String>) -> Vec<String> {
    // Sessions that never had installed skills keep their original wire messages unchanged.
    if !folder.is_dir() { return writes }
    let skills = catalogue(folder);
    writes.into_iter().map(|w| {
        let Ok(value) = serde_json::from_str(&w) else { return w };
        if !is_prompt(protocol, &value) { return w }
        adapt(protocol, native, value, &skills).to_string()
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w20_r01_discovery_is_progressive_and_control_traffic_is_unchanged() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let directory = root.path().join("tools/skills/manual");
        std::fs::create_dir_all(&directory)?;
        std::fs::write(directory.join("SKILL.md"), "---\nname: manual\ndescription: Manual only.\ndisable-model-invocation: true\n---\nPrivate skill instructions.")?;
        let normal = json!({"method":"session/prompt","params":{"prompt":[{"type":"text","text":"Continue"}]}}).to_string();
        let control = json!({"id":44,"method":"session/set_mode","params":{"modeId":"ask"}}).to_string();
        let sent = deliver(root.path(), Protocol::Acp(&[]), false, vec![normal.clone(), control.clone()]);
        anyhow::ensure!(!sent.join("").contains("Private skill instructions.") && !sent.join("").contains("Manual only."), "an ordinary turn eagerly loaded a manual-only skill");
        anyhow::ensure!(sent.get(1) == Some(&control), "control traffic was changed");
        let compact = normal.replace("Continue", "/compact");
        anyhow::ensure!(deliver(root.path(), Protocol::Acp(&[]), false, vec![compact.clone()]) == [compact], "native slash commands were changed");
        let explicit = normal.replace("Continue", "/tools:manual");
        let sent = deliver(root.path(), Protocol::Acp(&[]), false, vec![explicit]);
        anyhow::ensure!(sent.join("").contains("Private skill instructions."), "manual invocation didn't load instructions");
        let missing = root.path().join("never-installed");
        anyhow::ensure!(deliver(&missing, Protocol::Acp(&[]), false, vec![normal.clone()]) == [normal], "a session without installations changed");
        Ok(())
    }

    #[test]
    fn w20_r01_every_harness_receives_scoped_skills_and_assets() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let skill = root.path().join("tools/skills/review");
        std::fs::create_dir_all(skill.join("scripts"))?;
        std::fs::write(skill.join("SKILL.md"), "---\nname: review\ndescription: Review changes.\n---\nRun scripts/check.py on the diff.")?;
        std::fs::write(skill.join("scripts/check.py"), "print('checked')")?;
        for kind in crate::harness::kinds::Kind::all().iter().filter(|k| !matches!(k.protocol, Protocol::ClaudeStreamJson | Protocol::Terminal)) {
            let protocol = kind.protocol;
            let input = json!([{"type":"text","text":"Earlier conversation context."},{"type":"text","text":"/tools:review the diff"},{"type":"image","data":"keep-image"}]);
            let value = match protocol {
                Protocol::CodexAppServer => json!({"id":7,"method":"turn/start","params":{"threadId":"thread","input":input}}),
                Protocol::Acp(_) => json!({"id":7,"method":"session/prompt","params":{"sessionId":"session","prompt":input}}),
                _ => json!({"event":"user","message":{"content":"/tools:review the diff"}}),
            };
            let sent = deliver(root.path(), protocol, true, vec![value.to_string()]);
            let sent: Value = serde_json::from_str(sent.first().ok_or_else(|| anyhow::anyhow!("no prompt"))?)?;
            let wire = sent.to_string();
            anyhow::ensure!(wire.contains("tools:review") && wire.contains("SKILL.md") && wire.contains("the diff"), "{} didn't receive the skill", kind.id);
            if protocol == Protocol::CodexAppServer {
                anyhow::ensure!(sent.pointer("/params/input").and_then(Value::as_array).is_some_and(|a| a.iter().any(|v| v.get("type") == Some(&json!("skill")))), "no native Codex skill item");
                let fallback = deliver(root.path(), protocol, false, vec![value.to_string()]);
                anyhow::ensure!(fallback.join("").contains("Run scripts/check.py on the diff.") && !fallback.join("").contains("\"type\":\"skill\""), "older Codex lost the portable invocation");
            } else {
                anyhow::ensure!(wire.contains("Run scripts/check.py on the diff."), "{} didn't receive instructions", kind.id);
            }
            if protocol != Protocol::AgyStreamJson { anyhow::ensure!(wire.contains("keep-image") && wire.contains("Earlier conversation context."), "attachment or carried context lost"); }
            let other = tempfile::tempdir()?;
            let isolated = deliver(other.path(), protocol, true, vec![value.to_string()]);
            anyhow::ensure!(!isolated.join("").contains("Run scripts/check.py") && !isolated.join("").contains(&skill.to_string_lossy().to_string()));
        }
        // A queued turn resolves against the current installation, so removed skills cannot leak.
        std::fs::remove_dir_all(root.path().join("tools"))?;
        let sent = deliver(root.path(), Protocol::Acp(&[]), false, vec![json!({"method":"session/prompt","params":{"prompt":[{"type":"text","text":"Continue"}]}}).to_string()]);
        anyhow::ensure!(!sent.join("").contains("tools:review"));
        Ok(())
    }
}

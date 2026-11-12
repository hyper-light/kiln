//! W20-R01: installed Agent Skills reach every structured harness through its prompt transport.
//! Claude loads its plugins natively; Codex receives native skill input items. ACP (Devin,
//! OpenCode, Gemini, Goose, Amp, Cursor), Antigravity and Pi receive the selected instructions with
//! their asset directory. Ordinary turns advertise metadata only, for progressive discovery.
//! No global harness settings or repository files are changed. Disk work runs on the blocking pool.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

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
        Protocol::PiRpc => Some("/message"),
        Protocol::ClaudeStreamJson | Protocol::Terminal => None,
    }
}

fn is_prompt(protocol: Protocol, value: &Value) -> bool {
    match protocol {
        Protocol::CodexAppServer => matches!(value.get("method").and_then(Value::as_str), Some("turn/start" | "turn/steer")),
        Protocol::Acp(_) => value.get("method").and_then(Value::as_str) == Some("session/prompt"),
        Protocol::AgyStreamJson => value.get("event").and_then(Value::as_str) == Some("user"),
        Protocol::PiRpc => matches!(value.get("type").and_then(Value::as_str), Some("prompt" | "steer")),
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
        Protocol::PiRpc => w.contains("\"type\":\"prompt\"") || w.contains("\"type\":\"steer\""),
        Protocol::ClaudeStreamJson | Protocol::Terminal => false,
    })
}

fn metadata(skills: &[Skill]) -> String {
    if !skills.iter().any(|s| s.automatic) { return String::new(); }
    let mut text = String::from("<kiln_installed_skills>\nThis is the current skill catalogue for this session; it replaces earlier catalogues. Use a skill when its description matches the task or the user names it. Read its SKILL.md before using it, then load only the supporting files needed. Resolve relative files and scripts from that skill's directory. Explicit user instructions take priority over skill guidance. Skills omitted from this current catalogue are no longer active.\n");
    for skill in skills.iter().filter(|s| s.automatic) {
        text.push_str(&json!({"name": skill.name, "description": skill.description, "path": skill.path}).to_string());
        text.push('\n');
    }
    text.push_str("</kiln_installed_skills>");
    text
}

fn adapt(protocol: Protocol, native: bool, mut value: Value, skills: &[Skill], catalogue: Option<&str>) -> (Value, bool) {
    if !is_prompt(protocol, &value) { return (value, false) }
    let Some(pointer) = input_pointer(protocol) else { return (value, false) };
    let Some(input) = value.pointer_mut(pointer) else { return (value, false) };
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
    if selected.is_none() && (input.as_str().is_some_and(native_command) || input.as_array().is_some_and(|a| a.iter().any(|v| v.get("type").and_then(Value::as_str) == Some("text") && v.get("text").and_then(Value::as_str).is_some_and(native_command)))) { return (value, false) }
    let mut prefix = catalogue.unwrap_or_default().to_owned();
    let changed = !prefix.is_empty() || selected.is_some();
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
    if !changed { return (value, false); }
    if let Some(items) = input.as_array_mut() {
        items.insert(0, json!({"type": "text", "text": prefix}));
    } else if let Some(text) = input.as_str() {
        *input = Value::String(format!("{prefix}\n\n{text}"));
    }
    (value, true)
}

/// Provenance for a prompt the bridge itself changed. The consumer must correlate the exact
/// native request/client identity before projecting an echoed effective prompt for display.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Projection {
    pub request_id: Option<Value>,
    pub client_id: Option<String>,
    pub effective_digest: String,
}

impl Projection {
    pub fn matches(&self, client_id: &str, effective: &str) -> bool {
        self.client_id.as_deref() == Some(client_id) && self.effective_digest == digest(effective)
    }
}

pub fn digest(text: &str) -> String { super::hex(&Sha256::digest(text.as_bytes())) }

pub struct Delivered { pub writes: Vec<String>, pub projections: Vec<Projection> }

struct Catalogue { skills: Vec<Skill>, revision: String, metadata: String }

fn snapshot(folder: &Path) -> Catalogue {
    let skills = catalogue(folder);
    let mut hash = Sha256::new();
    for skill in &skills {
        for part in [&skill.name, &skill.native_name, &skill.description, &skill.body] {
            hash.update(part.len().to_le_bytes()); hash.update(part.as_bytes());
        }
        let path = skill.path.to_string_lossy();
        hash.update(path.len().to_le_bytes()); hash.update(path.as_bytes());
        hash.update([u8::from(skill.automatic)]);
    }
    let metadata = metadata(&skills);
    Catalogue { skills, revision: super::hex(&hash.finalize()), metadata }
}

/// Extract the text exactly as native userMessage echoes join it, before attachment rendering.
/// This is for equality/correlation, never for detecting tags in arbitrary human content.
pub fn prompt_text(protocol: Protocol, value: &Value) -> Option<String> {
    let input = value.pointer(input_pointer(protocol)?)?;
    if let Some(text) = input.as_str() { return Some(text.to_owned()); }
    Some(input.as_array()?.iter().filter_map(|item| item.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"))
}

/// One provider context's loaded catalogue. Refresh only when installed files change; ordinary
/// prompts use this snapshot. Native readiness resets only what that provider has been told.
#[derive(Default)]
pub struct Session {
    catalogue: Option<Catalogue>,
    advertised: Option<(String, bool)>,
}

impl Session {
    pub fn changed(&mut self) { self.catalogue = None; }
    pub fn provider_ready(&mut self) { self.advertised = None; }
    pub fn advertised(&self) -> Option<(String, bool)> { self.advertised.clone() }
    /// Adopting the same live provider preserves its acknowledgement across a kiln upgrade;
    /// installed files are loaded again lazily, without retaining their bodies in Frozen.
    pub fn restore(advertised: Option<(String, bool)>) -> Self { Self { catalogue: None, advertised } }

    /// Blocking: called on the existing delivery pool, never by a timer or as another user turn.
    pub fn deliver(&mut self, folder: &Path, protocol: Protocol, native: bool, writes: Vec<String>) -> Delivered {
        if !needed(protocol, &writes) { return Delivered { writes, projections: Vec::new() }; }
        let catalogue = self.catalogue.get_or_insert_with(|| snapshot(folder));
        let mut delivered = Delivered { writes: Vec::new(), projections: Vec::new() };
        for original in writes {
            let Ok(value) = serde_json::from_str::<Value>(&original) else { delivered.writes.push(original); continue };
            if !is_prompt(protocol, &value) { delivered.writes.push(original); continue }
            let changed = self.advertised.as_ref().is_none_or(|(revision, _)| revision != &catalogue.revision);
            let active = !catalogue.metadata.is_empty();
            let revoked = changed && !active && self.advertised.as_ref().is_some_and(|(_, active)| *active);
            let metadata = if changed && active { Some(catalogue.metadata.as_str()) }
                else if revoked { Some("<kiln_installed_skills>\nNo automatically invokable installed skills are active in this session.\n</kiln_installed_skills>") }
                else { None };
            // An initially empty catalogue has nothing to advertise or revoke.
            if changed && !active && !revoked { self.advertised = Some((catalogue.revision.clone(), false)); }
            let request_id = value.get("id").cloned();
            let client_id = value.pointer("/params/clientUserMessageId").and_then(Value::as_str).map(str::to_owned);
            let (value, adapted) = adapt(protocol, native, value, &catalogue.skills, metadata);
            if adapted {
                if metadata.is_some() { self.advertised = Some((catalogue.revision.clone(), active)); }
                if let Some(text) = prompt_text(protocol, &value) {
                    delivered.projections.push(Projection { request_id, client_id, effective_digest: digest(&text) });
                }
                delivered.writes.push(value.to_string());
            } else { delivered.writes.push(original); }
        }
        delivered
    }
}

/// Compatibility entry point for one-shot callers. Session drivers retain Session across actual
/// prompt deliveries and invalidate it only at install/reload and provider readiness boundaries.
pub fn deliver(folder: &Path, protocol: Protocol, native: bool, writes: Vec<String>) -> Vec<String> {
    Session::default().deliver(folder, protocol, native, writes).writes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_existing_skill_folder_does_not_prepend_internal_guidance() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let human = "Please ensure all lanes report material changes.";
        let prompt = json!({"id":9,"method":"turn/start","params":{"threadId":"thread","clientUserMessageId":"human-1","input":[{"type":"text","text":human}]}}).to_string();
        let actual = deliver(root.path(), Protocol::CodexAppServer, false, vec![prompt.clone()]);
        assert_eq!(actual, vec![prompt], "an existing empty session folder must not prepend <kiln_installed_skills> to the provider's echoed human message");
        Ok(())
    }

    #[test]
    fn catalogue_changes_refresh_once_without_adding_prompts_and_literal_tags_stay_human() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let skill = root.path().join("tools/skills/review");
        std::fs::create_dir_all(&skill)?;
        std::fs::write(skill.join("SKILL.md"), "---\nname: review\ndescription: Review changes.\n---\nRead the diff.")?;
        let prompt = |id: &str, text: &str| json!({"id":id,"method":"turn/start","params":{"threadId":"thread","clientUserMessageId":id,"input":[{"type":"text","text":text}]}}).to_string();
        let mut session = Session::default();
        let first = session.deliver(root.path(), Protocol::CodexAppServer, false, vec![prompt("one", "First human request")]);
        assert_eq!(first.writes.len(), 1);
        assert_eq!(first.projections.len(), 1);
        let effective = first.writes.first().ok_or_else(|| anyhow::anyhow!("no provider prompt"))?;
        let effective: Value = serde_json::from_str(effective)?;
        let echoed = prompt_text(Protocol::CodexAppServer, &effective).ok_or_else(|| anyhow::anyhow!("no prompt text"))?;
        let projection = first.projections.first().ok_or_else(|| anyhow::anyhow!("no generated provenance"))?;
        assert!(projection.matches("one", &echoed));
        assert!(!projection.matches("different-human", &echoed));
        let same = prompt("two", "Second human request");
        let second = session.deliver(root.path(), Protocol::CodexAppServer, false, vec![same.clone()]);
        assert_eq!(second.writes, vec![same]);
        assert!(second.projections.is_empty());
        session.changed();
        let unchanged = prompt("three", "Unchanged installed catalogue");
        assert_eq!(session.deliver(root.path(), Protocol::CodexAppServer, false, vec![unchanged.clone()]).writes, vec![unchanged]);
        let literal = prompt("different-human", &echoed);
        let literal = session.deliver(root.path(), Protocol::CodexAppServer, false, vec![literal.clone()]);
        assert!(literal.projections.is_empty(), "a real user pasted catalogue-looking text; it is not generated provenance");
        let wire: Value = serde_json::from_str(literal.writes.first().ok_or_else(|| anyhow::anyhow!("literal user prompt missing"))?)?;
        assert_eq!(prompt_text(Protocol::CodexAppServer, &wire).as_deref(), Some(echoed.as_str()));
        std::fs::remove_dir_all(root.path().join("tools"))?;
        session.changed();
        let revoke = session.deliver(root.path(), Protocol::CodexAppServer, false, vec![prompt("four", "After uninstall")]);
        assert_eq!(revoke.writes.len(), 1);
        assert!(revoke.writes.first().is_some_and(|w| w.contains("No automatically invokable installed skills")));
        let empty = prompt("five", "Still empty");
        assert_eq!(session.deliver(root.path(), Protocol::CodexAppServer, false, vec![empty.clone()]).writes, vec![empty]);
        Ok(())
    }

    #[test]
    fn provider_context_readiness_readvertises_once_and_explicit_manual_invocations_remain_available() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let skill = root.path().join("tools/skills/manual");
        std::fs::create_dir_all(&skill)?;
        std::fs::write(skill.join("SKILL.md"), "---\nname: manual\ndescription: Manual.\ndisable-model-invocation: true\n---\nExplicit selected instructions.")?;
        let prompt = |text: &str| json!({"method":"session/prompt","params":{"prompt":[{"type":"text","text":text}]}}).to_string();
        let mut session = Session::default();
        let human = prompt("Ordinary human request");
        assert_eq!(session.deliver(root.path(), Protocol::Acp(&[]), false, vec![human.clone()]).writes, vec![human]);
        let explicit = session.deliver(root.path(), Protocol::Acp(&[]), false, vec![prompt("/tools:manual supplied arguments")]);
        assert!(explicit.writes.first().is_some_and(|w| w.contains("Explicit selected instructions.") && w.contains("supplied arguments")));
        assert!(!explicit.writes.first().is_some_and(|w| w.contains("<kiln_installed_skills>")), "manual-only invocation does not need empty catalogue guidance");
        std::fs::write(skill.join("SKILL.md"), "---\nname: manual\ndescription: Automatic now.\n---\nUpdated instructions.")?;
        session.changed();
        let first = session.deliver(root.path(), Protocol::Acp(&[]), false, vec![prompt("After catalogue change")]);
        assert!(first.writes.first().is_some_and(|w| w.contains("<kiln_installed_skills>")));
        session.provider_ready();
        let ready = session.deliver(root.path(), Protocol::Acp(&[]), false, vec![prompt("A real new provider context")]);
        assert!(ready.writes.first().is_some_and(|w| w.contains("<kiln_installed_skills>")));
        let later = prompt("Same provider context");
        assert_eq!(session.deliver(root.path(), Protocol::Acp(&[]), false, vec![later.clone()]).writes, vec![later]);
        Ok(())
    }

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
                // Its pictures go beside its words; the conversation carried over comes before them (`harness::pi`).
                Protocol::PiRpc => json!({"id":"m7","type":"prompt","message":"/tools:review the diff","images":[{"type":"image","data":"keep-image","mimeType":"image/png"}]}),
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
            if protocol == Protocol::PiRpc { anyhow::ensure!(wire.contains("keep-image") && sent.get("id") == Some(&json!("m7")), "Pi's picture or its message id lost"); }
            else if protocol != Protocol::AgyStreamJson { anyhow::ensure!(wire.contains("keep-image") && wire.contains("Earlier conversation context."), "attachment or carried context lost"); }
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

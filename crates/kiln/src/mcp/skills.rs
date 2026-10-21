//! kiln's skills, built into the binary: served over MCP (as resources, and through the skills
//! extension, SEP-2640: `skills/list`, `skills/get`, `skill://` URIs with each file's SHA-256 and
//! size) and written into the skills directory of every agent kiln starts ([`install`]).
//!
//! Each skill is a directory in the Agent Skills layout: a `SKILL.md` whose frontmatter names
//! and describes it, and the files it links.

use std::path::Path;

use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

struct File {
    path: &'static str,
    text: &'static str,
}

struct Skill {
    name: &'static str,
    files: &'static [File],
}

macro_rules! skill {
    ($name:literal) => {
        Skill { name: $name, files: &[File { path: "SKILL.md", text: include_str!(concat!("../../assets/skills/", $name, "/SKILL.md")) }] }
    };
}

static SKILLS: &[Skill] = &[
    skill!("kiln-agents"),
    skill!("kiln-plans"),
    skill!("kiln-deploy"),
    skill!("kiln-diagnose"),
    skill!("kiln-apps"),
    skill!("kiln-argocd-migration"),
    // CI's (docs/ci.md W20-R05).
    skill!("kiln-ci-author"),
    skill!("kiln-ci-diagnose"),
    skill!("kiln-ci-runners"),
    skill!("kiln-ci-review"),
    skill!("kiln-ci-migrate"),
    skill!("kiln-ci-secrets"),
    skill!("kiln-ci-performance"),
];

/// A frontmatter field of a `SKILL.md`.
fn field(text: &str, key: &str) -> String {
    let Some(rest) = text.strip_prefix("---\n") else { return String::new() };
    let Some((front, _)) = rest.split_once("\n---") else { return String::new() };
    front
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{key}:")))
        .map(|v| v.trim().trim_matches('"').to_owned())
        .unwrap_or_default()
}

fn sha(text: &str) -> String {
    let d = Sha256::digest(text.as_bytes());
    d.iter().map(|b| format!("{b:02x}")).collect()
}

fn describe(s: &Skill) -> Value {
    let main = s.files.iter().find(|f| f.path == "SKILL.md").map(|f| f.text).unwrap_or("");
    json!({
        "name": s.name,
        "description": field(main, "description"),
        "uri": format!("skill://{}/SKILL.md", s.name),
        "files": s.files.iter().map(|f| json!({"uri": format!("skill://{}/{}", s.name, f.path), "sha256": sha(f.text), "size": f.text.len()})).collect::<Vec<_>>(),
    })
}

/// Every skill, as `skills/list` gives them.
pub fn list() -> Vec<Value> {
    SKILLS.iter().map(describe).collect()
}

/// One skill by name or by its `SKILL.md` URI.
pub fn get(key: &str) -> Option<Value> {
    let name = key.strip_prefix("skill://").map(|r| r.split('/').next().unwrap_or("")).unwrap_or(key);
    SKILLS.iter().find(|s| s.name == name).map(describe)
}

/// Every file, as MCP resources.
pub fn resources() -> Vec<Value> {
    SKILLS
        .iter()
        .flat_map(|s| {
            s.files.iter().map(move |f| {
                json!({
                    "uri": format!("skill://{}/{}", s.name, f.path),
                    "name": format!("{}/{}", s.name, f.path),
                    "description": if f.path == "SKILL.md" { field(f.text, "description") } else { String::new() },
                    "mimeType": "text/markdown",
                })
            })
        })
        .collect()
}

/// A file's contents, by URI.
pub fn read(uri: &str) -> Option<Value> {
    let rest = uri.strip_prefix("skill://")?;
    let (name, path) = rest.split_once('/')?;
    let f = SKILLS.iter().find(|s| s.name == name)?.files.iter().find(|f| f.path == path)?;
    Some(json!({"uri": uri, "mimeType": "text/markdown", "text": f.text}))
}

/// Writes every skill under `dir` (`dir/<name>/SKILL.md`), replacing older copies.
pub fn install(dir: &Path) -> std::io::Result<()> {
    for s in SKILLS {
        let d = dir.join(s.name);
        std::fs::create_dir_all(&d)?;
        for f in s.files {
            std::fs::write(d.join(f.path), f.text)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_skill_names_and_describes_itself() {
        for s in list() {
            let name = s.get("name").and_then(Value::as_str).unwrap_or("");
            let desc = s.get("description").and_then(Value::as_str).unwrap_or("");
            assert!(!name.is_empty() && desc.len() > 40, "{name}: {desc}");
            assert!(read(&format!("skill://{name}/SKILL.md")).is_some());
        }
        assert!(get("kiln-deploy").is_some());
        // How to run agents: fanning out, briefs, waiting, low permissions, merging, never answering for the person.
        let agents = read("skill://kiln-agents/SKILL.md").and_then(|f| f.get("text").and_then(Value::as_str).map(str::to_owned)).unwrap_or_default();
        for part in ["## When to fan out", "## Briefing a child", "## Waiting and reading results", "## Keeping children's permissions low", "## Merging their work", "## Putting work up for review", "## Never answering for the person"] {
            assert!(agents.contains(part), "kiln-agents: {part}");
        }
        // Goal stacks and plans: one's own stack (and ending the turn after lining it up, saying each goal met, splitting
        // one, pausing for a while), plans (steps that wait for others), an agent per step (side by side, a step for the
        // main agent).
        let plans = read("skill://kiln-plans/SKILL.md").and_then(|f| f.get("text").and_then(Value::as_str).map(str::to_owned)).unwrap_or_default();
        for part in [
            "## Your own goal stack",
            "**end your turn**",
            "### When a goal is met: `goals_done`",
            "**Never skip a goal you met**",
            "### Splitting a goal: `goals_split`",
            "resume_in_s",
            "## Plans",
            "`needs`",
            "## One agent per step",
            "{agent: \"main\"}",
            "## What a person decides",
        ] {
            assert!(plans.contains(part), "kiln-plans: {part}");
        }
        assert!(get("skill://kiln-diagnose/SKILL.md").is_some());
    }
}

//! What a source holds, read from its files alone (docs/agents.md, "Skills kiln installs"): pure,
//! no disk, network or clock. A source is a tree of files (a repository at a commit, an upload, a
//! folder, words written in the page); what's read from it is one package: its name, the skills
//! and agents in it, the hooks and MCP servers it brings, what each skill must have set up on a
//! machine (a Python environment, a Go build) and the tools it needs there.
//!
//! In order, the first that matches:
//! 1. a marketplace (`.claude-plugin/marketplace.json`, else `.cursor-plugin/`'s): its plugin by the
//!    name the link ends in (`…/tree/main/pstack`), else its only one, else a choice;
//! 2. a plugin's manifest (`.claude-plugin/plugin.json`, `.cursor-plugin/`, `.codex-plugin/`, or a
//!    `plugin.json` of agent-plugins.org's schema): its `skills` (a folder or a list) and `agents`;
//! 3. every `SKILL.md` a few folders down, but those under `deprecated`, `misc`, dot-folders and
//!    `node_modules`;
//! 4. one Markdown file: a skill on its own, named by its frontmatter.
//!
//! Every package is laid out the one way a machine installs it ([`Package::files`]): a Claude Code
//! plugin, `.claude-plugin/plugin.json`, `skills/<name>/…`, `agents/<name>.md`, and its hooks and
//! MCP servers (`hooks/`, `.mcp.json`) only where the person turned them on.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The most one file and a package's files together may hold (W20-R01).
pub const MAX_FILE: u64 = 16 << 20;
pub const MAX_TOTAL: u64 = 64 << 20;
/// How deep skills are looked for when nothing names where they are.
const DEPTH: usize = 6;
/// Folders never looked in for skills: retired or set aside by their authors' own convention, and
/// what tools put there.
const SKIPPED: &[&str] = &["node_modules", "deprecated", "misc", "target", "dist", "__pycache__", "venv"];

/// A file of a source: its path from the source's root, its bytes, and whether it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    pub path: String,
    pub bytes: Vec<u8>,
    pub exec: bool,
}

/// How a source was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Shape {
    Marketplace,
    Plugin,
    SkillFolders,
    OneSkill,
}

impl Shape {
    pub fn word(self) -> &'static str {
        match self {
            Self::Marketplace => "marketplace",
            Self::Plugin => "plugin",
            Self::SkillFolders => "skill folders",
            Self::OneSkill => "one skill",
        }
    }
}

/// A skill or an agent of a package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

/// What a skill must have done on a machine before it works there, in its own folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Setup {
    pub skill: String,
    /// "Python", "Go", "Node", "Rust".
    pub stack: String,
    pub command: String,
}

/// What a package reads as, laid out to install.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Package {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub license: String,
    pub shape: Shape,
    pub skills: Vec<Item>,
    pub agents: Vec<Item>,
    /// Hook events it brings (`PostToolUse`, …): off until turned on.
    pub hooks: Vec<String>,
    /// MCP servers it brings, by name: off until turned on.
    pub mcp: Vec<String>,
    pub setup: Vec<Setup>,
    /// Tools it runs that a machine must have (`python3`, `gh`, `go`).
    pub needs: Vec<String>,
    /// Its files as installed, by path, from the source's paths: `skills/<name>/…` for each skill's,
    /// `agents/<name>.md`, `hooks/…`, `.mcp.json`. The manifest kiln writes is not among them.
    #[serde(skip)]
    pub files: Vec<File>,
}

/// What a source reads as: a package, or a marketplace's plugins to choose among.
#[derive(Debug, Clone, PartialEq)]
pub enum Read {
    Package(Box<Package>),
    Choose(Vec<Item>),
}

/// Why a source isn't a package.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("no skills or plugin there")]
    Nothing,
    #[error("no plugin {0:?} in its marketplace")]
    NoPlugin(String),
    #[error("{0}")]
    Bad(String),
    #[error("{what} is {size} bytes, over the {limit} a package may hold")]
    TooLarge { what: String, size: u64, limit: u64 },
}

/// A file's frontmatter field (`name:`, `description:`), or "".
pub fn front(text: &str, key: &str) -> String {
    let Some(rest) = text.strip_prefix("---").map(|r| r.trim_start_matches('\r')).and_then(|r| r.strip_prefix('\n')) else { return String::new() };
    let end = rest.find("\n---").unwrap_or(rest.len());
    let Some(head) = rest.get(..end) else { return String::new() };
    if let Ok(Value::Object(m)) = serde_yaml::from_str::<Value>(head) {
        return match m.get(key) {
            Some(Value::String(s)) => s.trim().to_owned(),
            Some(v @ (Value::Number(_) | Value::Bool(_))) => v.to_string(),
            _ => String::new(),
        };
    }
    head.lines().find_map(|l| l.strip_prefix(&format!("{key}:"))).map(|v| v.trim().trim_matches('"').to_owned()).unwrap_or_default()
}

/// A name a machine may put in a path and a harness may show: lowercase letters, digits, `-`, `_`
/// and `.`, at most 64, not starting with a dot. Others' spaces and capitals become `-` and lower.
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.trim().chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches(|c| c == '-' || c == '.').chars().take(64).collect::<String>();
    out.trim_end_matches(['-', '.']).to_owned()
}

/// A path within the source: no `..`, no root, `/`-separated, without `./`.
fn clean(p: &str) -> Option<String> {
    let mut parts = Vec::new();
    let normalized = p.replace('\\', "/");
    if normalized.starts_with('/') || normalized.contains([':', '\0']) {
        return None;
    }
    for s in normalized.split('/') {
        match s {
            "" | "." => {}
            ".." => return None,
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

fn under<'a>(path: &'a str, dir: &str) -> Option<&'a str> {
    if dir.is_empty() {
        return Some(path);
    }
    path.strip_prefix(dir)?.strip_prefix('/')
}

fn parent(path: &str) -> &str {
    path.rfind('/').and_then(|i| path.get(..i)).unwrap_or("")
}

fn base(path: &str) -> &str {
    path.rfind('/').and_then(|i| path.get(i + 1..)).unwrap_or(path)
}

struct Tree<'a> {
    files: BTreeMap<&'a str, &'a File>,
}

impl<'a> Tree<'a> {
    fn new(files: &'a [File]) -> Self {
        Self { files: files.iter().map(|f| (f.path.as_str(), f)).collect() }
    }

    fn text(&self, path: &str) -> Option<String> {
        self.files.get(path).map(|f| String::from_utf8_lossy(&f.bytes).into_owned())
    }

    fn json(&self, path: &str) -> Option<Value> {
        self.files.get(path).and_then(|f| serde_json::from_slice(&f.bytes).ok())
    }

    fn within(&self, dir: &str) -> impl Iterator<Item = (&'a str, &'a File)> + '_ {
        let dir = dir.to_owned();
        self.files.iter().filter_map(move |(p, f)| under(p, &dir).map(|rest| (rest, *f)))
    }

    /// The folders under `dir` holding a `SKILL.md`, none inside another's, looked for [`DEPTH`] deep.
    fn skill_dirs(&self, dir: &str) -> Vec<String> {
        let mut found: Vec<String> = Vec::new();
        for (rest, _) in self.within(dir) {
            if base(rest) != "SKILL.md" {
                continue;
            }
            let rel = parent(rest);
            let segs: Vec<&str> = if rel.is_empty() { Vec::new() } else { rel.split('/').collect() };
            if segs.len() > DEPTH || segs.iter().any(|s| s.starts_with('.') || SKIPPED.contains(s)) {
                continue;
            }
            found.push(if dir.is_empty() { rel.to_owned() } else if rel.is_empty() { dir.to_owned() } else { format!("{dir}/{rel}") });
        }
        found.sort();
        // A skill's own folders may hold examples with SKILL.md of their own: the outer one is the skill.
        let mut out: Vec<String> = Vec::new();
        for d in found {
            if !out.iter().any(|o| under(&d, o).is_some_and(|r| !r.is_empty())) {
                out.push(d);
            }
        }
        out
    }
}

/// The plugin a marketplace names, and where it is: `.claude-plugin`'s first (Claude Code's own
/// copy, as `cursor/plugins` keeps pstack's at `harness/pstack/claude-code`), then `.cursor-plugin`'s.
fn marketplace(tree: &Tree, want: Option<&str>) -> Result<Option<(String, Value)>, Refusal> {
    let Some(m) = tree.json(".claude-plugin/marketplace.json").or_else(|| tree.json(".cursor-plugin/marketplace.json")) else { return Ok(None) };
    let plugins: Vec<Value> = m.get("plugins").and_then(Value::as_array).cloned().unwrap_or_default();
    let source = |p: &Value| match p.get("source") {
        Some(Value::String(s)) => clean(s),
        Some(Value::Object(o)) => o.get("path").and_then(Value::as_str).and_then(clean),
        _ => None,
    };
    let named = |p: &Value| p.get("name").and_then(Value::as_str).unwrap_or("").to_owned();
    let pick = match want {
        Some(w) => plugins.iter().find(|p| named(p) == w || source(p).as_deref() == Some(w)).ok_or_else(|| Refusal::NoPlugin(w.to_owned()))?,
        None if plugins.len() == 1 => plugins.first().ok_or(Refusal::Nothing)?,
        None => return Err(Refusal::Bad(format!("choose:{}", serde_json::to_string(&plugins.iter().map(|p| Item { name: named(p), description: p.get("description").and_then(Value::as_str).unwrap_or("").to_owned() }).collect::<Vec<_>>()).unwrap_or_default()))),
    };
    // A source in another repository is that repository's to read, not this one's.
    let Some(dir) = source(pick) else { return Err(Refusal::Bad(format!("{} lives in another repository: add that one", named(pick)))) };
    Ok(Some((dir, pick.clone())))
}

fn manifest(tree: &Tree, root: &str) -> Option<Value> {
    let at = |p: &str| if root.is_empty() { p.to_owned() } else { format!("{root}/{p}") };
    [".claude-plugin/plugin.json", ".cursor-plugin/plugin.json", ".codex-plugin/plugin.json", "plugin.json"].iter().find_map(|p| tree.json(&at(p)))
}

/// The folders a manifest's `skills` or `agents` names: a folder, or a list of them; none named: `default`.
fn named_dirs(m: Option<&Value>, key: &str, root: &str, default: &str) -> Vec<String> {
    let join = |p: &str| clean(p).map(|p| if root.is_empty() { p } else if p.is_empty() { root.to_owned() } else { format!("{root}/{p}") });
    match m.and_then(|m| m.get(key)) {
        Some(Value::String(s)) => join(s).into_iter().collect(),
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).filter_map(join).collect(),
        _ => join(default).into_iter().collect(),
    }
}

/// Setup a skill's folder asks for, from the files that say how it's built, in the order they're
/// found; and the tools each runs.
fn setups(skill: &str, files: &[(&str, &File)]) -> Vec<(Setup, &'static str)> {
    let has = |n: &str| files.iter().any(|(p, _)| *p == n);
    let mut out = Vec::new();
    let mut push = |stack: &str, command: &str, tool: &'static str| out.push((Setup { skill: skill.to_owned(), stack: stack.to_owned(), command: command.to_owned() }, tool));
    if has("pyproject.toml") || has("uv.lock") {
        push("Python", "uv sync", "uv");
    } else if has("requirements.txt") {
        push("Python", "python3 -m venv .venv && .venv/bin/pip install -r requirements.txt", "python3");
    }
    if has("go.mod") {
        push("Go", "go build -o bin/ ./...", "go");
    }
    if has("package.json") {
        push("Node", if has("package-lock.json") { "npm ci --ignore-scripts" } else { "npm install --ignore-scripts" }, "npm");
    }
    if has("Cargo.toml") {
        push("Rust", "cargo build --release", "cargo");
    }
    out
}

/// The program a script's first line runs (`#!/usr/bin/env python3`: `python3`), but a shell's.
fn interpreter(bytes: &[u8]) -> Option<String> {
    let line = bytes.strip_prefix(b"#!")?;
    let line = String::from_utf8_lossy(line.split(|b| *b == b'\n').next().unwrap_or_default()).into_owned();
    let mut words = line.split_whitespace();
    let first = words.next()?;
    let prog = if base(first) == "env" { words.find(|w| !w.starts_with('-'))? } else { base(first) };
    (!matches!(prog, "sh" | "bash" | "zsh" | "dash")).then(|| prog.to_owned())
}

/// Tools a skill's words say to have, in a "Requirements" list (`gh`, `python3`), as tstack's README
/// and many skills say them: `` `name` `` items, or "[GitHub CLI](…) (`gh`)".
fn required(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('#') {
            inside = l.trim_start_matches('#').trim().to_lowercase().starts_with("requirement");
            continue;
        }
        if !inside || !(l.starts_with("- ") || l.starts_with("* ")) {
            continue;
        }
        for tick in l.split('`').skip(1).step_by(2) {
            let t = tick.trim();
            if !t.is_empty() && t.len() <= 32 && t.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
                out.push(t.to_owned());
                // W20-R01: later code spans name the skill or script using the tool.
                break;
            }
        }
    }
    out
}

/// Reads `files` (a source's whole tree) as one package. `want`: the plugin or folder a link names
/// (`…/tree/main/pstack`: `pstack`); `fallback`: the name to give one the source doesn't name (a
/// repository's).
pub fn read(files: &[File], want: Option<&str>, fallback: &str) -> Result<Read, Refusal> {
    let tree = Tree::new(files);
    let want = want.map(|w| clean(w).ok_or_else(|| Refusal::Bad("a source path stays inside its repository".into()))).transpose()?.filter(|w| !w.is_empty());
    // 1. A marketplace's plugin.
    let picked = match marketplace(&tree, want.as_deref().map(base)) {
        Ok(p) => p,
        Err(Refusal::Bad(m)) if m.starts_with("choose:") => {
            let items: Vec<Item> = m.strip_prefix("choose:").and_then(|j| serde_json::from_str(j).ok()).unwrap_or_default();
            return Ok(Read::Choose(items));
        }
        // A link into a folder of a repository with a marketplace that doesn't name it: the folder.
        Err(Refusal::NoPlugin(_)) if want.as_deref().is_some_and(|w| tree.within(w).next().is_some()) => None,
        Err(e) => return Err(e),
    };
    let (root, shape, entry) = match (&picked, &want) {
        (Some((dir, entry)), _) => (dir.clone(), Shape::Marketplace, Some(entry.clone())),
        (None, Some(w)) => (w.clone(), Shape::Plugin, None),
        (None, None) => (String::new(), Shape::Plugin, None),
    };
    // 4. One Markdown file: a link to a SKILL.md, an upload of one.
    if let Some(one) = (root.ends_with(".md") || (root.is_empty() && files.len() == 1 && files.first().is_some_and(|f| f.path.ends_with(".md")))).then(|| if root.ends_with(".md") { tree.files.get(root.as_str()).copied() } else { files.first() }).flatten() {
        let text = String::from_utf8_lossy(&one.bytes).into_owned();
        let stem = base(&one.path).trim_end_matches(".md");
        let stem = if stem.eq_ignore_ascii_case("SKILL") { base(parent(&one.path)) } else { stem };
        let name = slug(&Some(front(&text, "name")).filter(|n| !n.is_empty()).unwrap_or_else(|| stem.to_owned()));
        let name = if name.is_empty() { slug(fallback) } else { name };
        if name.is_empty() {
            return Err(Refusal::Bad("name it: its frontmatter has no `name`".into()));
        }
        let description = front(&text, "description");
        if one.bytes.len() as u64 > MAX_FILE {
            return Err(Refusal::TooLarge { what: one.path.clone(), size: one.bytes.len() as u64, limit: MAX_FILE });
        }
        let file = File { path: format!("skills/{name}/SKILL.md"), bytes: one.bytes.clone(), exec: false };
        return Ok(Read::Package(Box::new(Package {
            name: name.clone(),
            version: String::new(),
            description: description.clone(),
            license: String::new(),
            shape: Shape::OneSkill,
            skills: vec![Item { name, description }],
            agents: Vec::new(),
            hooks: Vec::new(),
            mcp: Vec::new(),
            setup: Vec::new(),
            needs: Vec::new(),
            files: vec![file],
        })));
    }
    if tree.within(&root).next().is_none() {
        return Err(if root.is_empty() { Refusal::Nothing } else { Refusal::Bad(format!("nothing at {root}")) });
    }
    // 2. A plugin's manifest, else 3. every SKILL.md.
    let m = manifest(&tree, &root);
    let shape = if m.is_none() && shape != Shape::Marketplace { Shape::SkillFolders } else { shape };
    let skill_roots = if m.is_some() || shape == Shape::Marketplace { named_dirs(m.as_ref(), "skills", &root, "skills") } else { vec![root.clone()] };
    let mut dirs: Vec<String> = skill_roots.iter().flat_map(|d| tree.skill_dirs(d)).collect();
    dirs.sort();
    dirs.dedup();
    if dirs.is_empty() && shape == Shape::SkillFolders {
        return Err(Refusal::Nothing);
    }
    let field = |k: &str| m.as_ref().or(entry.as_ref()).and_then(|v| v.get(k)).and_then(Value::as_str).unwrap_or("").trim().to_owned();
    let name = slug(&Some(field("name")).filter(|n| !n.is_empty()).unwrap_or_else(|| if root.is_empty() { fallback.to_owned() } else { base(&root).to_owned() }));
    let name = if name.is_empty() { slug(fallback) } else { name };
    if name.is_empty() {
        return Err(Refusal::Bad("it names no package: give it a name".into()));
    }
    let mut out = Vec::new();
    let mut skills = Vec::new();
    let mut setup = Vec::new();
    let mut needs: BTreeSet<String> = BTreeSet::new();
    let mut taken: BTreeSet<String> = BTreeSet::new();
    for d in &dirs {
        let text = tree.text(&if d.is_empty() { "SKILL.md".to_owned() } else { format!("{d}/SKILL.md") }).unwrap_or_default();
        let mut sname = slug(&Some(front(&text, "name")).filter(|n| !n.is_empty()).unwrap_or_else(|| base(d).to_owned()));
        if sname.is_empty() {
            sname = name.clone();
        }
        // Two folders of one package with one name: the second keeps its folder's.
        if taken.contains(&sname) {
            sname = slug(&format!("{}-{}", base(parent(d)), base(d)));
        }
        if !taken.insert(sname.clone()) {
            continue;
        }
        let own: Vec<(&str, &File)> = tree.within(d).filter(|(rest, _)| !rest.split('/').any(|s| s == ".git" || s == "node_modules")).collect();
        for (s, tool) in setups(&sname, &own) {
            needs.insert(tool.to_owned());
            setup.push(s);
        }
        for (rest, f) in &own {
            if (f.exec || rest.starts_with("scripts/") || rest.starts_with("bin/")) && let Some(t) = interpreter(&f.bytes) {
                needs.insert(t);
            }
            out.push(File { path: format!("skills/{sname}/{rest}"), bytes: f.bytes.clone(), exec: f.exec });
        }
        needs.extend(required(&text));
        skills.push(Item { name: sname, description: front(&text, "description") });
    }
    // The README beside a set of skills says what all of them need (tstack's "Requirements").
    if shape == Shape::SkillFolders && let Some(readme) = tree.text(&if root.is_empty() { "README.md".to_owned() } else { format!("{root}/README.md") }) {
        needs.extend(required(&readme));
    }
    // Agents: a plugin's own (`agents/*.md`, or where its manifest says).
    let mut agents = Vec::new();
    if m.is_some() || shape == Shape::Marketplace {
        for d in named_dirs(m.as_ref(), "agents", &root, "agents") {
            for (rest, f) in tree.within(&d) {
                if rest.contains('/') || !rest.ends_with(".md") {
                    continue;
                }
                let text = String::from_utf8_lossy(&f.bytes);
                let aname = slug(&Some(front(&text, "name")).filter(|n| !n.is_empty()).unwrap_or_else(|| rest.trim_end_matches(".md").to_owned()));
                if aname.is_empty() || agents.iter().any(|a: &Item| a.name == aname) {
                    continue;
                }
                out.push(File { path: format!("agents/{aname}.md"), bytes: f.bytes.clone(), exec: false });
                agents.push(Item { name: aname, description: front(&text, "description") });
            }
        }
    }
    // Hooks and MCP servers: what they are, kept apart until a person turns them on.
    let at = |p: &str| if root.is_empty() { p.to_owned() } else { format!("{root}/{p}") };
    let mut hooks = Vec::new();
    for (rest, f) in tree.within(&at("hooks")) {
        if rest == "hooks.json" && let Ok(Value::Object(h)) = serde_json::from_slice::<Value>(&f.bytes) {
            let events = h.get("hooks").and_then(Value::as_object).unwrap_or(&h);
            hooks.extend(events.keys().cloned());
        }
        out.push(File { path: format!("hooks/{rest}"), bytes: f.bytes.clone(), exec: f.exec });
    }
    let mut mcp = Vec::new();
    if let Some(f) = tree.files.get(at(".mcp.json").as_str()) {
        if let Ok(v) = serde_json::from_slice::<Value>(&f.bytes) {
            mcp.extend(v.get("mcpServers").and_then(Value::as_object).map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default());
        }
        out.push(File { path: ".mcp.json".into(), bytes: f.bytes.clone(), exec: false });
    }
    if skills.is_empty() && agents.is_empty() {
        return Err(Refusal::Nothing);
    }
    // What it installs, not the rest of its repository (its docs' pictures), is held to the limits.
    if let Some(f) = out.iter().find(|f| f.bytes.len() as u64 > MAX_FILE) {
        return Err(Refusal::TooLarge { what: f.path.clone(), size: f.bytes.len() as u64, limit: MAX_FILE });
    }
    let size: u64 = out.iter().map(|f| f.bytes.len() as u64).sum();
    if size > MAX_TOTAL {
        return Err(Refusal::TooLarge { what: name, size, limit: MAX_TOTAL });
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    agents.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Read::Package(Box::new(Package {
        name,
        version: field("version"),
        description: field("description"),
        license: field("license"),
        shape,
        skills,
        agents,
        hooks,
        mcp,
        setup,
        needs: needs.into_iter().collect(),
        files: out,
    })))
}

/// The parts of a package a person picked: its skills and agents, its hooks and MCP servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Picks {
    #[serde(default = "yes")]
    pub skills: bool,
    #[serde(default = "yes")]
    pub agents: bool,
    #[serde(default)]
    pub hooks: bool,
    #[serde(default)]
    pub mcp: bool,
    /// Its setup run on each machine (off: its skills are there without it).
    #[serde(default = "yes")]
    pub setup: bool,
}

fn yes() -> bool {
    true
}

impl Default for Picks {
    fn default() -> Self {
        Self { skills: true, agents: true, hooks: false, mcp: false, setup: true }
    }
}

impl Picks {
    /// Whether a file of the package (by its installed path) is among what's picked.
    pub fn takes(&self, path: &str) -> bool {
        (self.skills && path.starts_with("skills/")) || (self.agents && path.starts_with("agents/")) || (self.hooks && path.starts_with("hooks/")) || (self.mcp && path == ".mcp.json")
    }
}

/// The manifest that makes a package a Claude Code plugin, named as kiln names it.
pub fn plugin_json(p: &Package) -> Vec<u8> {
    let mut m = serde_json::json!({"name": p.name});
    if !p.version.is_empty() && let Some(o) = m.as_object_mut() {
        o.insert("version".into(), Value::String(p.version.clone()));
    }
    if !p.description.is_empty() && let Some(o) = m.as_object_mut() {
        o.insert("description".into(), Value::String(p.description.clone()));
    }
    serde_json::to_vec_pretty(&m).unwrap_or_default()
}

/// What a package installs with the parts a person picked, and its manifest.
pub fn installed(p: &Package, picks: Picks) -> Vec<File> {
    let mut out: Vec<File> = p.files.iter().filter(|f| picks.takes(&f.path)).cloned().collect();
    out.push(File { path: ".claude-plugin/plugin.json".into(), bytes: plugin_json(p), exec: false });
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(path: &str, text: &str) -> File {
        File { path: path.into(), bytes: text.as_bytes().to_vec(), exec: false }
    }

    fn skill(name: &str, desc: &str) -> String {
        format!("---\nname: {name}\ndescription: {desc}\n---\n\n# {name}\n")
    }

    fn package(r: Result<Read, Refusal>) -> anyhow::Result<Package> {
        match r {
            Ok(Read::Package(p)) => Ok(*p),
            other => anyhow::bail!("not a package: {other:?}"),
        }
    }

    #[test]
    fn w20_r01_a_marketplace_link_reads_claude_codes_copy_of_its_plugin() -> anyhow::Result<()> {
        // cursor/plugins: the marketplace points Claude Code at harness/pstack/claude-code, not pstack/.
        let files = vec![
            f(".claude-plugin/marketplace.json", r#"{"name":"cursor-plugins","plugins":[{"name":"origin-apps","source":"./origin-apps"},{"name":"pstack","source":"./harness/pstack/claude-code","description":"go deep first"}]}"#),
            f("pstack/skills/old/SKILL.md", &skill("old", "the cursor copy")),
            f("harness/pstack/claude-code/.claude-plugin/plugin.json", r#"{"name":"pstack","version":"0.15.36","license":"MIT","description":"go deep first"}"#),
            f("harness/pstack/claude-code/skills/architect/SKILL.md", &skill("architect", "Design before building.")),
            f("harness/pstack/claude-code/skills/architect/references/runner-prompt.md", "x"),
            f("harness/pstack/claude-code/skills/how/SKILL.md", &skill("how", "Explain how something works.")),
            f("harness/pstack/claude-code/agents/comment-sicko.md", "---\nname: comment-sicko\ndescription: Hates comments.\n---\n"),
            f("harness/pstack/claude-code/docs/guide/README.md", "guide"),
        ];
        let p = package(read(&files, Some("pstack"), "plugins"))?;
        assert_eq!((p.name.as_str(), p.version.as_str(), p.license.as_str(), p.shape), ("pstack", "0.15.36", "MIT", Shape::Marketplace));
        assert_eq!(p.skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["architect", "how"]);
        assert_eq!(p.agents.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["comment-sicko"]);
        let paths: Vec<&str> = p.files.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"skills/architect/references/runner-prompt.md") && paths.contains(&"agents/comment-sicko.md"), "{paths:?}");
        assert!(!paths.iter().any(|p| p.contains("guide") || p.contains("old")), "{paths:?}");
        // The marketplace's root, with two plugins and no name: a choice.
        assert!(matches!(read(&files, None, "plugins"), Ok(Read::Choose(c)) if c.len() == 2));
        assert_eq!(read(&files, Some("nope"), "plugins"), Err(Refusal::NoPlugin("nope".into())));
        Ok(())
    }

    #[test]
    fn w20_r01_a_manifest_lists_its_skills_folders() -> anyhow::Result<()> {
        // mattpocock/skills: a plugin whose manifest lists its skills, nested by category, past deprecated ones.
        let files = vec![
            f(".claude-plugin/plugin.json", r#"{"name":"mattpocock-skills","version":"1.3.2","skills":["./skills/engineering/tdd","./skills/productivity/grill-me"]}"#),
            f(".claude-plugin/marketplace.json", r#"{"name":"mattpocock","plugins":[{"name":"mattpocock-skills","source":"./"}]}"#),
            f("skills/engineering/tdd/SKILL.md", &skill("tdd", "Red, green, refactor.")),
            f("skills/engineering/tdd/agents/openai.yaml", "interface: {}"),
            f("skills/productivity/grill-me/SKILL.md", &skill("grill-me", "Interview me.")),
            f("skills/deprecated/old/SKILL.md", &skill("old", "Retired.")),
        ];
        let p = package(read(&files, None, "skills"))?;
        assert_eq!(p.name, "mattpocock-skills");
        assert_eq!(p.skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["grill-me", "tdd"]);
        assert!(p.files.iter().any(|f| f.path == "skills/tdd/agents/openai.yaml"));
        assert!(p.agents.is_empty(), "openai.yaml inside a skill isn't an agent");
        Ok(())
    }

    #[test]
    fn w20_r01_skill_folders_without_a_manifest_and_what_they_need() -> anyhow::Result<()> {
        // t3dotgg/tstack: skills/*/SKILL.md, one with a Python script, its README's requirements.
        let mut script = f("skills/babysit-pr/scripts/pr-wait", "#!/usr/bin/env python3\nprint(1)\n");
        script.exec = true;
        let files = vec![
            f("README.md", "# tstack\n\n## Requirements\n\n- An authenticated [GitHub CLI](https://cli.github.com) (`gh`).\n- `python3` for `babysit-pr`'s `pr-wait` script.\n"),
            f("skills/babysit-pr/SKILL.md", &skill("babysit-pr", "Watch a PR.")),
            script,
            f("skills/file-pr/SKILL.md", &skill("file-pr", "File a PR.")),
            f("skills/misc/x/SKILL.md", &skill("x", "Set aside.")),
        ];
        let p = package(read(&files, None, "tstack"))?;
        assert_eq!((p.name.as_str(), p.shape), ("tstack", Shape::SkillFolders));
        assert_eq!(p.skills.len(), 2);
        assert_eq!(p.needs, ["gh", "python3"], "skill and script names in prose aren't required tools");
        assert!(p.files.iter().any(|f| f.path == "skills/babysit-pr/scripts/pr-wait" && f.exec));
        Ok(())
    }

    #[test]
    fn w20_r01_code_in_a_skill_asks_for_its_setup() -> anyhow::Result<()> {
        let files = vec![
            f("bench-report/SKILL.md", &skill("bench-report", "Tables from criterion.")),
            f("bench-report/pyproject.toml", "[project]\nname='b'\n"),
            f("trace-diff/SKILL.md", &skill("trace-diff", "Diff traces.")),
            f("trace-diff/go.mod", "module x\n"),
            f("hooks/hooks.json", r#"{"hooks":{"PostToolUse":[]}}"#),
            f(".mcp.json", r#"{"mcpServers":{"traces":{"command":"bin/trace-mcp"}}}"#),
        ];
        let p = package(read(&files, None, "perf"))?;
        assert_eq!(p.setup.iter().map(|s| (s.skill.as_str(), s.command.as_str())).collect::<Vec<_>>(), [("bench-report", "uv sync"), ("trace-diff", "go build -o bin/ ./...")]);
        assert_eq!(p.needs, ["go", "uv"]);
        assert_eq!((p.hooks.as_slice(), p.mcp.as_slice()), (&["PostToolUse".to_owned()][..], &["traces".to_owned()][..]));
        // Off unless turned on: neither is installed, and the manifest kiln writes is.
        let paths: Vec<String> = installed(&p, Picks::default()).into_iter().map(|f| f.path).collect();
        assert!(paths.contains(&".claude-plugin/plugin.json".to_owned()) && !paths.iter().any(|p| p.starts_with("hooks/") || p == ".mcp.json"), "{paths:?}");
        assert!(installed(&p, Picks { hooks: true, mcp: true, ..Picks::default() }).iter().any(|f| f.path == ".mcp.json"));
        Ok(())
    }

    #[test]
    fn w20_r01_one_markdown_file_is_one_skill() -> anyhow::Result<()> {
        let p = package(read(&[f("release-notes.md", &skill("release-notes", "Notes from merged changes."))], None, "upload"))?;
        assert_eq!((p.name.as_str(), p.shape, p.files.first().map(|f| f.path.as_str())), ("release-notes", Shape::OneSkill, Some("skills/release-notes/SKILL.md")));
        // Without frontmatter, by its file's name; a link to a SKILL.md, by its folder's.
        assert_eq!(package(read(&[f("notes/My Notes.md", "# hi")], None, "x"))?.name, "my-notes");
        let tree = vec![f("skills/how/SKILL.md", "# how"), f("skills/why/SKILL.md", "# why")];
        assert_eq!(package(read(&tree, Some("skills/how/SKILL.md"), "x"))?.name, "how");
        Ok(())
    }

    #[test]
    fn w20_r01_what_isnt_a_package_says_why() -> anyhow::Result<()> {
        assert_eq!(read(&[f("src/main.rs", "fn main() {}")], None, "x"), Err(Refusal::Nothing));
        // A link's folder that isn't in the tree; one out of it isn't a folder of it.
        assert_eq!(read(&[f("a/SKILL.md", "x")], Some("b"), "x"), Err(Refusal::Bad("nothing at b".into())));
        let big = File { path: "skills/a/SKILL.md".into(), bytes: vec![b'x'; usize::try_from(MAX_FILE + 1).unwrap_or(0)], exec: false };
        assert!(matches!(read(&[big], None, "x"), Err(Refusal::TooLarge { .. })));
        assert!(matches!(read(&[f("a.md", "# a")], Some("../outside"), "x"), Err(Refusal::Bad(_))));
        assert!(matches!(read(&[f("a.md", "# a")], Some("/absolute"), "x"), Err(Refusal::Bad(_))));
        assert_eq!(slug("  My Skill!! "), "my-skill");
        assert_eq!(slug("../x"), "x");
        assert_eq!(front("---\nname: \"a b\"\ndescription: >\n  folded\n  text\n---\n", "description"), "folded text");
        Ok(())
    }
}

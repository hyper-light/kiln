//! What a moving session's harness keeps for it in its own home, outside the conversation and the
//! repository (`docs/reach.md` §8): read on the runner it leaves ([`pack`]), carried in its package
//! (`super::moving`), and put where the harness looks for it on the runner it goes to ([`install`]),
//! for the folder it works in there.
//!
//! - Claude Code (`CLAUDE_CONFIG_DIR`, else `~/.claude`): its auto memory of the project
//!   (`projects/<project>/memory/`, the project being the main folder of the git repository the
//!   session works in, a worktree's included, else its folder), merged into the project's there.
//!   And the session's own files, as it goes on under its own id: its subagents' transcripts, tool
//!   results and title beside its transcript (`projects/<folder>/<session>/`), its checkpoints for
//!   rewind (`file-history/<session>/`), its tasks (`tasks/<session>/`), its todos
//!   (`todos/<session>-*.json`) and the plans its transcript names (`plans/<slug>.md`). Coming back
//!   to a runner it was on, these take the place of the ones it left there, as its transcript does.
//! - Codex (`CODEX_HOME`, else `~/.codex`): its memories (`memories/`, but `raw_memories.md`, which
//!   Codex makes again from its database; and its notes, `memories_extensions/`), merged; and the
//!   thread's goal, read from Codex's database (`goals_<n>.sqlite`) and set on the thread the
//!   conversation goes on in there. Codex's databases are never written: its other processes use
//!   them, their schema is its version's, and they are keyed by a thread that doesn't go on there.
//!   Nor does the thread's rollout go: the conversation goes as the new thread's history, which
//!   Codex records there in a rollout of its own; a copy of the old one would be a second thread of
//!   the same conversation there, in a folder that isn't, and the old one stays where it was.
//! - Gemini CLI (`GEMINI_CLI_HOME`, else home: `.gemini/`): its memory of the project
//!   (`tmp/<project>/memory/`), merged into the project's there: under the name Gemini CLI knows
//!   the folder by there, else the one it would give it, marked so that it finds it.
//! - Antigravity (`~/.gemini/antigravity-cli`): its knowledge (`knowledge/`), merged.
//! - Goose (`~/.config/goose`): its memories (`memory/<category>.txt`), merged entry by entry.
//! - opencode, Amp and Devin keep nothing of the kind: what they keep is the session's own record,
//!   and an ACP agent goes on there in a new session, given the conversation in words.
//!
//! Merged, nothing there is written over: a file that isn't there is written; one that differs
//! stays, and the one that came goes beside it (`<name>.from-<machine>.md`). A memory's index
//! (`MEMORY.md`) takes the lines it lacks instead, pointing at what went beside, and Goose's
//! memories the entries they lack. The person's own instructions to the harness
//! (`~/.claude/CLAUDE.md`, `~/.codex/AGENTS.md`, `~/.gemini/GEMINI.md`...) are the machine's, for
//! every project there: they never go, and the runner it goes to says when its own differ. Nor do
//! credentials or settings. What doesn't fit ([`MAX_FILE`], [`MAX_TOTAL`]) stays, and is said;
//! memories go first, checkpoints last.
//!
//! Blocking: it reads and writes files.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::model::Objective;

/// The most one file, and all of them, may be.
pub const MAX_FILE: u64 = 16 << 20;
pub const MAX_TOTAL: u64 = 64 << 20;
/// How deep a folder's files are looked for.
const MAX_DEPTH: usize = 6;
/// Files that are credentials wherever they are: never carried, nor written.
const SECRETS: &[&str] =
    &[".credentials.json", "credentials.json", "credentials.toml", "auth.json", "oauth_creds.json", "google_accounts.json", "secrets.json", "secrets.yaml", "secrets.yml", ".env", ".netrc"];
/// Kinds of files that hold keys.
const KEY_EXTENSIONS: &[&str] = &["key", "pem", "p12", "pfx", "keychain", "keychain-db"];

/// What a session's harness keeps for it in its home, as a package carries it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Home {
    /// Its files, each under one of the harness's own folders.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<HomeFile>,
    /// Codex: the thread's goal, set on the thread the conversation goes on in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<Goal>,
    /// The person's own instructions to the harness on the machine it left, as one digest ("": it
    /// had none): they don't go, and the runner it goes to says when its own differ.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// What stays on the machine it left, in words (more than a move carries).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub left: Vec<String>,
}

impl Home {
    /// It has something to put in place there: a package with it is one an older runner refuses
    /// whole, rather than take without it.
    pub fn installs(&self) -> bool {
        !self.files.is_empty() || self.goal.is_some()
    }

    /// The bytes of its files.
    pub fn size(&self) -> u64 {
        // Base64: three bytes in four.
        self.files.iter().map(|f| u64::try_from(f.data.len() / 4 * 3).unwrap_or(u64::MAX)).fold(0, u64::saturating_add)
    }
}

/// A file a harness keeps: where, as a path relative to one of its folders (`/`-separated), its
/// permissions and its contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HomeFile {
    pub root: Root,
    pub path: String,
    pub mode: u32,
    /// Base64.
    pub data: String,
}

/// A goal the harness pursues across turns (Codex's thread goal), as it is set again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    pub objective: String,
    /// "active" or "paused".
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<i64>,
}

/// The folders of a harness's home a package's files go in, each found anew on the machine it
/// goes to (for the folder the session works in there).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Root {
    /// Claude Code's auto memory of the project: `projects/<project>/memory/`.
    ClaudeMemory,
    /// Claude Code's folder of the session beside its transcript: `projects/<folder>/<session>/`.
    ClaudeSession,
    /// Claude Code's checkpoints of the session, for rewind: `file-history/<session>/`.
    ClaudeHistory,
    /// Claude Code's task list of the session: `tasks/<session>/`.
    ClaudeTasks,
    /// Claude Code's todo lists (older versions'): `todos/`, those named for the session.
    ClaudeTodos,
    /// Claude Code's plans: `plans/`, those the session's transcript names.
    ClaudePlans,
    /// Codex's memories: `memories/`.
    CodexMemories,
    /// Codex's memory notes: `memories_extensions/`.
    CodexNotes,
    /// Gemini CLI's memory of the project: `tmp/<project>/memory/`.
    GeminiMemory,
    /// Antigravity's knowledge: `knowledge/`.
    AntigravityKnowledge,
    /// Goose's memories: `memory/`.
    GooseMemory,
}

impl Root {
    /// The harness whose folder it is.
    fn harness(self) -> &'static str {
        match self {
            Self::ClaudeMemory | Self::ClaudeSession | Self::ClaudeHistory | Self::ClaudeTasks | Self::ClaudeTodos | Self::ClaudePlans => "claude",
            Self::CodexMemories | Self::CodexNotes => "codex",
            Self::GeminiMemory => "gemini",
            Self::AntigravityKnowledge => "antigravity",
            Self::GooseMemory => "goose",
        }
    }

    /// The session's own files, which take the place of the ones it left there; else shared with
    /// the harness's other sessions there, and merged.
    fn own(self) -> bool {
        matches!(self, Self::ClaudeSession | Self::ClaudeHistory | Self::ClaudeTasks | Self::ClaudeTodos | Self::ClaudePlans)
    }

    /// What goes first when not everything fits: memories, then the session's work, then
    /// transcripts, then checkpoints.
    fn weight(self) -> u8 {
        match self {
            Self::ClaudeTasks | Self::ClaudeTodos | Self::ClaudePlans => 1,
            Self::ClaudeSession => 2,
            Self::ClaudeHistory => 3,
            _ => 0,
        }
    }

    /// What its files are, in words.
    fn label(self) -> &'static str {
        match self {
            Self::ClaudeMemory => "Claude Code's memory of the project",
            Self::ClaudeSession => "subagents' transcripts and tool results",
            Self::ClaudeHistory => "checkpoints for rewind",
            Self::ClaudeTasks => "tasks",
            Self::ClaudeTodos => "todos",
            Self::ClaudePlans => "plans",
            Self::CodexMemories => "Codex's memories",
            Self::CodexNotes => "Codex's memory notes",
            Self::GeminiMemory => "Gemini CLI's memory of the project",
            Self::AntigravityKnowledge => "Antigravity's knowledge",
            Self::GooseMemory => "Goose's memories",
        }
    }

    /// Its file at `path` is an index the files that came are merged into line by line
    /// (`MEMORY.md`), or a list of entries merged entry by entry (Goose's).
    fn merges(self, path: &str) -> Option<Merge> {
        match self {
            Self::ClaudeMemory | Self::GeminiMemory if path == "MEMORY.md" => Some(Merge::Lines),
            Self::GooseMemory if path.ends_with(".txt") && !path.contains('/') => Some(Merge::Entries),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Merge {
    Lines,
    Entries,
}

/// Where the harnesses keep their files on a machine, for a session's environment (which may move
/// a harness's home: `CLAUDE_CONFIG_DIR`, `CODEX_HOME`).
#[derive(Debug, Clone)]
pub struct Dirs {
    home: PathBuf,
    env: Vec<(String, String)>,
    /// The machine's own environment counts after the session's.
    machine: bool,
}

impl Dirs {
    /// This machine's, for a session with environment `env`.
    pub fn here(env: &[(String, String)]) -> Self {
        Self { home: crate::os::home_dir(), env: env.to_vec(), machine: true }
    }

    /// Those of a machine whose home is `home`, from `env` alone (a test's).
    #[cfg(test)]
    pub fn at(home: &Path, env: &[(String, String)]) -> Self {
        Self { home: home.to_path_buf(), env: env.to_vec(), machine: false }
    }

    fn var(&self, key: &str) -> Option<PathBuf> {
        self.env.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.clone()).or_else(|| if self.machine { std::env::var(key).ok() } else { None }).filter(|v| !v.trim().is_empty()).map(PathBuf::from)
    }

    fn claude(&self) -> PathBuf {
        self.var("CLAUDE_CONFIG_DIR").unwrap_or_else(|| self.home.join(".claude"))
    }

    fn codex(&self) -> PathBuf {
        self.var("CODEX_HOME").unwrap_or_else(|| self.home.join(".codex"))
    }

    fn gemini(&self) -> PathBuf {
        self.var("GEMINI_CLI_HOME").unwrap_or_else(|| self.home.clone()).join(".gemini")
    }

    fn antigravity(&self) -> PathBuf {
        self.home.join(".gemini").join("antigravity-cli")
    }

    /// The person's configuration folder (`XDG_CONFIG_HOME`, else `~/.config`).
    fn config(&self) -> PathBuf {
        self.var("XDG_CONFIG_HOME").unwrap_or_else(|| self.home.join(".config"))
    }

    fn goose(&self) -> PathBuf {
        if let Some(root) = self.var("GOOSE_PATH_ROOT") {
            return root.join("config");
        }
        if cfg!(windows)
            && let Some(app) = self.var("APPDATA")
        {
            return app.join("Block").join("goose").join("config");
        }
        self.config().join("goose")
    }

    /// `path` as the person knows it: from their home, else whole.
    fn shown(&self, path: &Path) -> String {
        match path.strip_prefix(&self.home) {
            Ok(rel) => format!("~/{}", rel.to_string_lossy().replace('\\', "/")),
            Err(_) => path.display().to_string(),
        }
    }
}

/// An id that is a single plain name (a session's, a thread's): nothing that walks folders.
fn plain(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id != "." && id != ".." && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// A file name that is a credential, or holds a key.
fn secret(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SECRETS.contains(&lower.as_str()) || lower.rsplit_once('.').is_some_and(|(_, ext)| KEY_EXTENSIONS.contains(&ext))
}

/// A name left out of what's carried and written: hidden files and folders (the harness's locks,
/// its inboxes, a repository's `.git`) but a task list's counter, and locks.
fn hidden(root: Root, name: &str) -> bool {
    (name.starts_with('.') && !(root == Root::ClaudeTasks && name == ".highwatermark")) || name.ends_with(".lock")
}

/// `rel` is a path a file of `root` may have: plain names, not too deep, none hidden or a
/// credential; and for the folders shared by every session, only the session's own (a todo list
/// named for it, a plan).
fn allowed(root: Root, rel: &str, session: Option<&str>) -> bool {
    let parts: Vec<&str> = rel.split('/').collect();
    if rel.is_empty() || rel.contains('\\') || parts.len() > MAX_DEPTH + 1 {
        return false;
    }
    if parts.iter().any(|p| p.is_empty() || *p == "." || *p == ".." || hidden(root, p) || secret(p)) {
        return false;
    }
    if !Path::new(rel).components().all(|c| matches!(c, Component::Normal(_))) {
        return false;
    }
    match root {
        Root::ClaudeTodos => session.is_some_and(|s| todo_of(rel, s)),
        Root::ClaudePlans => parts.len() == 1 && rel.ends_with(".md"),
        _ => true,
    }
}

/// A todo list of session `s` (`<s>.json`, `<s>-agent-<id>.json`).
fn todo_of(name: &str, s: &str) -> bool {
    !name.contains('/') && name.ends_with(".json") && (name == format!("{s}.json") || name.starts_with(&format!("{s}-")))
}

/// A plan of one of `slugs` (`<slug>.md`, a subagent's `<slug>-agent-<id>.md`).
fn plan_of(name: &str, slugs: &HashSet<String>) -> bool {
    !name.contains('/') && name.ends_with(".md") && slugs.iter().any(|s| name == format!("{s}.md") || name.starts_with(&format!("{s}-agent-")))
}

/// The plans a Claude Code transcript names: the `slug` of its records.
fn plan_slugs(transcript: &[String]) -> HashSet<String> {
    const KEY: &str = "\"slug\":\"";
    let mut out = HashSet::new();
    for line in transcript.iter().filter(|l| l.contains(KEY)) {
        let mut rest = line.as_str();
        while let Some(at) = rest.find(KEY) {
            rest = rest.get(at + KEY.len()..).unwrap_or("");
            let slug: String = rest.chars().take_while(|c| *c != '"').collect();
            if !slug.is_empty() && slug.len() <= 100 && slug.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
                out.insert(slug);
            }
        }
    }
    out
}

/// A file found to carry.
struct Found {
    root: Root,
    path: String,
    full: PathBuf,
    size: u64,
    mode: u32,
}

#[cfg(unix)]
fn mode_of(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    meta.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn mode_of(_meta: &std::fs::Metadata) -> u32 {
    0o644
}

/// The files under `dir`, as `root`'s, by their paths relative to it: links aren't followed, and
/// what's hidden, a credential, or `skip` names (a path relative to `dir`) is left out.
fn walk(dir: &Path, root: Root, skip: &dyn Fn(&str) -> bool, out: &mut Vec<Found>) {
    fn go(dir: &Path, rel: &str, depth: usize, root: Root, skip: &dyn Fn(&str) -> bool, out: &mut Vec<Found>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let path = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            if hidden(root, &name) || secret(&name) || skip(&path) {
                continue;
            }
            // A directory entry's type is the link's, not what it points at.
            let Ok(t) = e.file_type() else { continue };
            if t.is_dir() {
                if depth < MAX_DEPTH {
                    go(&e.path(), &path, depth + 1, root, skip, out);
                }
            } else if t.is_file()
                && let Ok(meta) = e.metadata()
            {
                out.push(Found { root, path, full: e.path(), size: meta.len(), mode: mode_of(&meta) });
            }
        }
    }
    go(dir, "", 0, root, skip, out);
}

/// `p` with its `.` and `..` resolved, as written (no links followed).
fn lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            c => out.push(c.as_os_str()),
        }
    }
    out
}

/// The folder Claude Code keys a project's auto memory by: the main folder of the git repository
/// `cwd` is in (a worktree's is its repository's), else `cwd` itself. Only folders that are there
/// count: on the machine a session arrives at, its worktree may not be made yet.
pub fn claude_project_base(cwd: &Path) -> PathBuf {
    let mut at = Some(cwd);
    while let Some(dir) = at {
        let git = dir.join(".git");
        match std::fs::symlink_metadata(&git) {
            Ok(m) if m.is_dir() => return dir.to_path_buf(),
            Ok(m) if m.is_file() => return main_of_worktree(dir, &git).unwrap_or_else(|| dir.to_path_buf()),
            _ => {}
        }
        at = dir.parent();
    }
    cwd.to_path_buf()
}

/// The main folder of the repository whose worktree is `root` (its `.git` file names its own
/// folder in the repository's, `<repository>/.git/worktrees/<name>`).
fn main_of_worktree(root: &Path, git: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(git).ok()?;
    let own = lexical(&root.join(text.trim().strip_prefix("gitdir:")?.trim()));
    let common = std::fs::read_to_string(own.join("commondir")).ok()?;
    let common = lexical(&own.join(common.trim()));
    if own.parent() != Some(common.join("worktrees").as_path()) {
        return None;
    }
    if common.file_name().is_some_and(|n| n == ".git") { common.parent().map(Path::to_path_buf) } else { Some(common) }
}

/// A key of Claude Code's settings files (its own, then the folder's local ones): the first set.
fn claude_setting(dirs: &Dirs, cwd: &str, key: &str, project: bool) -> Option<String> {
    let mut files = vec![dirs.claude().join("settings.json")];
    if project {
        files.push(Path::new(cwd).join(".claude").join("settings.json"));
    }
    files.push(Path::new(cwd).join(".claude").join("settings.local.json"));
    files.into_iter().find_map(|f| {
        let v: Value = serde_json::from_slice(&std::fs::read(f).ok()?).ok()?;
        v.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned)
    })
}

/// Where Claude Code is told to keep its auto memory instead of with the project: the
/// environment's or its settings' (`autoMemoryDirectory`; the one checked into the project
/// doesn't count, as Claude Code ignores it).
fn claude_memory_elsewhere(dirs: &Dirs, cwd: &str) -> Option<String> {
    for key in ["CLAUDE_COWORK_MEMORY_PATH_OVERRIDE", "CLAUDE_CODE_REMOTE_MEMORY_DIR"] {
        if let Some(v) = dirs.var(key) {
            return Some(v.display().to_string());
        }
    }
    claude_setting(dirs, cwd, "autoMemoryDirectory", false)
}

/// Claude Code's auto memory of the project `cwd` is in.
fn claude_memory(dirs: &Dirs, cwd: &str) -> PathBuf {
    let base = claude_project_base(Path::new(cwd));
    super::handoff::claude_project_dir(&dirs.claude(), &base.to_string_lossy()).join("memory")
}

fn claude_files(dirs: &Dirs, session: Option<&str>, cwd: &str, transcript: &[String], out: &mut Vec<Found>, left: &mut Vec<String>) {
    let config = dirs.claude();
    if let Some(s) = session {
        // Beside its transcript, wherever that is.
        let beside = super::transcript::find(&config, s).and_then(|t| t.parent().map(|d| d.join(s))).unwrap_or_else(|| super::handoff::claude_project_dir(&config, cwd).join(s));
        walk(&beside, Root::ClaudeSession, &|_| false, out);
        walk(&config.join("file-history").join(s), Root::ClaudeHistory, &|_| false, out);
        walk(&config.join("tasks").join(s), Root::ClaudeTasks, &|_| false, out);
        walk(&config.join("todos"), Root::ClaudeTodos, &|p| !todo_of(p, s), out);
        // Plans kept in the project (`plansDirectory`) go with it.
        let slugs = plan_slugs(transcript);
        if !slugs.is_empty() && claude_setting(dirs, cwd, "plansDirectory", true).is_none() {
            walk(&config.join("plans"), Root::ClaudePlans, &|p| !plan_of(p, &slugs), out);
        }
    }
    match claude_memory_elsewhere(dirs, cwd) {
        Some(at) => left.push(format!("Claude Code's memory, which it keeps in {at} there")),
        // A team's memory is Claude Code's to bring from the team's own store.
        None => walk(&claude_memory(dirs, cwd), Root::ClaudeMemory, &|p| p == "team" || p.starts_with("team/"), out),
    }
}

/// The newest of Codex's databases named `<prefix><n>.sqlite` in `home`.
fn codex_db(home: &Path, prefix: &str) -> Option<PathBuf> {
    std::fs::read_dir(home)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let n: u32 = name.strip_prefix(prefix)?.strip_suffix(".sqlite")?.parse().ok()?;
            Some((n, e.path()))
        })
        .max_by_key(|(n, _)| *n)
        .map(|(_, p)| p)
}

/// The goal of Codex thread `thread`, from its database, read only. Err: the database can't be
/// read; None: the thread has no goal to go on with (none, or it's complete).
fn codex_goal(home: &Path, thread: &str) -> Result<Option<Goal>, ()> {
    let db = codex_db(home, "goals_").ok_or(())?;
    let conn = rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX).map_err(|_| ())?;
    let _ = conn.busy_timeout(std::time::Duration::from_secs(2));
    let row = conn
        .query_row("SELECT objective, status, token_budget FROM thread_goals WHERE thread_id = ?1", [thread], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<i64>>(2)?)));
    let (objective, status, token_budget) = match row {
        Ok(r) => r,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(_) => return Err(()),
    };
    // A goal cut off by its machine's account goes on there; one waiting on a person, or whose
    // budget is spent, waits there too.
    let status = match status.as_str() {
        "active" | "usage_limited" => "active",
        "paused" | "blocked" | "budget_limited" => "paused",
        _ => return Ok(None),
    };
    Ok((!objective.trim().is_empty()).then(|| Goal { objective, status: status.to_owned(), token_budget }))
}

/// The goal as kiln saw it ([`Objective`]), when Codex's database can't say.
fn goal_of(o: &Objective) -> Option<Goal> {
    let status = match o.status.as_str() {
        "active" => "active",
        "paused" | "unmet" => "paused",
        _ => return None,
    };
    (!o.text.trim().is_empty()).then(|| Goal { objective: o.text.clone(), status: status.to_owned(), token_budget: None })
}

/// The folder Gemini CLI keys a project by: its path, resolved, without a trailing separator (in
/// lower case on Windows).
fn gemini_key(cwd: &str) -> String {
    let p = lexical(Path::new(cwd)).to_string_lossy().into_owned();
    let p = if p.len() > 1 { p.trim_end_matches(['/', '\\']).to_owned() } else { p };
    if cfg!(windows) { p.to_lowercase() } else { p }
}

/// The folder a Gemini CLI project's marker (`<base>/<id>/.project_root`) names.
fn gemini_marker(dir: &Path) -> Option<String> {
    std::fs::read_to_string(dir.join(".project_root")).ok().map(|s| s.trim().to_owned())
}

fn gemini_registry(gemini: &Path) -> HashMap<String, String> {
    let v: Value = std::fs::read(gemini.join("projects.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
    v.get("projects").and_then(Value::as_object).map(|o| o.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned()))).collect()).unwrap_or_default()
}

/// Gemini CLI's name for an id it gives a project: lower case letters, digits and dashes.
fn gemini_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The name Gemini CLI knows project `key` by under `gemini`: its registry's (`projects.json`), if
/// no marker says that name is another folder's; else the name whose marker names the folder.
fn gemini_known(gemini: &Path, key: &str) -> Option<String> {
    let bases = [gemini.join("tmp"), gemini.join("history")];
    if let Some(id) = gemini_registry(gemini).get(key).filter(|id| gemini_id(id))
        && bases.iter().all(|b| gemini_marker(&b.join(id)).is_none_or(|owner| owner == key))
    {
        return Some(id.clone());
    }
    bases.iter().find_map(|b| std::fs::read_dir(b).ok()?.flatten().find(|e| gemini_marker(&e.path()).is_some_and(|o| o == key)).map(|e| e.file_name().to_string_lossy().into_owned()))
}

/// The name Gemini CLI gives project `key` under `gemini`: the one it knows it by, else the one it
/// would claim for it (the folder's name, made a slug, numbered past the ones taken), marked
/// (`tmp/<name>/.project_root`) so that it finds the folder's when it starts there.
fn gemini_claim(gemini: &Path, key: &str) -> Option<String> {
    if let Some(id) = gemini_known(gemini, key) {
        return Some(id);
    }
    let taken: HashSet<String> = gemini_registry(gemini).into_values().collect();
    let name = Path::new(key).file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    let slug = name.chars().map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() { c } else { '-' }).collect::<String>();
    let slug = slug.split('-').filter(|s| !s.is_empty()).collect::<Vec<_>>().join("-");
    let slug = if slug.is_empty() { "project".to_owned() } else { slug };
    let bases = [gemini.join("tmp"), gemini.join("history")];
    for n in 0..1000 {
        let id = if n == 0 { slug.clone() } else { format!("{slug}-{n}") };
        if taken.contains(&id) || bases.iter().any(|b| gemini_marker(&b.join(&id)).is_some_and(|owner| owner != key)) {
            continue;
        }
        let dir = gemini.join("tmp").join(&id);
        std::fs::create_dir_all(&dir).ok()?;
        std::fs::write(dir.join(".project_root"), key).ok()?;
        return Some(id);
    }
    None
}

/// The person's own instructions to harness `kind` on a machine, for every project there: the
/// files, by their paths in the harness's folder, and where they are. None: it has none kiln
/// knows of.
fn instruction_files(kind: &str, dirs: &Dirs) -> Option<Vec<(String, PathBuf)>> {
    let one = |dir: PathBuf, name: &str| (name.to_owned(), dir.join(name));
    let files = match kind {
        "claude" => {
            let config = dirs.claude();
            let mut rules = Vec::new();
            walk(&config.join("rules"), Root::ClaudeMemory, &|_| false, &mut rules);
            let mut v: Vec<(String, PathBuf)> = rules.into_iter().filter(|f| f.path.ends_with(".md")).map(|f| (format!("rules/{}", f.path), f.full)).collect();
            v.sort();
            v.insert(0, one(config, "CLAUDE.md"));
            v
        }
        "codex" => vec![one(dirs.codex(), "AGENTS.md"), one(dirs.codex(), "AGENTS.override.md")],
        "gemini" | "antigravity" => vec![one(dirs.gemini(), "GEMINI.md")],
        "opencode" => vec![one(dirs.config().join("opencode"), "AGENTS.md")],
        "goose" => vec![one(dirs.goose(), ".goosehints")],
        "amp" => vec![one(dirs.config().join("amp"), "AGENTS.md")],
        _ => return None,
    };
    Some(files)
}

/// Those instructions as one digest ("": there are none).
fn instructions(kind: &str, dirs: &Dirs) -> Option<String> {
    use sha2::Digest as _;
    let mut h = sha2::Sha256::new();
    let mut any = false;
    for (name, path) in instruction_files(kind, dirs)? {
        if let Ok(bytes) = std::fs::read(&path) {
            any = true;
            h.update(name.as_bytes());
            h.update([0]);
            h.update(&bytes);
            h.update([0]);
        }
    }
    Some(if any { hex::encode(h.finalize()) } else { String::new() })
}

/// What harness `kind` keeps for session `session` (its own id) working in `cwd`, on this machine:
/// what a move carries. `transcript`: Claude Code's record of the session (the plans it names);
/// `objective`: the goal it pursues as kiln saw it (Codex's, should its database not say).
pub fn pack(kind: &str, dirs: &Dirs, session: Option<&str>, cwd: &str, transcript: &[String], objective: Option<&Objective>) -> Home {
    pack_within(kind, dirs, session, cwd, transcript, objective, (MAX_FILE, MAX_TOTAL))
}

fn pack_within(kind: &str, dirs: &Dirs, session: Option<&str>, cwd: &str, transcript: &[String], objective: Option<&Objective>, (max_file, max_total): (u64, u64)) -> Home {
    let session = session.filter(|s| plain(s));
    let mut home = Home::default();
    let mut found = Vec::new();
    match kind {
        "claude" => claude_files(dirs, session, cwd, transcript, &mut found, &mut home.left),
        "codex" => {
            let codex = dirs.codex();
            walk(&codex.join("memories"), Root::CodexMemories, &|p| p == "raw_memories.md", &mut found);
            walk(&codex.join("memories_extensions"), Root::CodexNotes, &|_| false, &mut found);
            home.goal = match session.map(|t| codex_goal(&codex, t)) {
                Some(Ok(g)) => g,
                Some(Err(())) | None => objective.and_then(goal_of),
            };
        }
        "gemini" => {
            let gemini = dirs.gemini();
            if let Some(id) = gemini_known(&gemini, &gemini_key(cwd)) {
                walk(&gemini.join("tmp").join(id).join("memory"), Root::GeminiMemory, &|_| false, &mut found);
            }
        }
        "antigravity" => walk(&dirs.antigravity().join("knowledge"), Root::AntigravityKnowledge, &|_| false, &mut found),
        "goose" => walk(&dirs.goose().join("memory"), Root::GooseMemory, &|_| false, &mut found),
        _ => {}
    }
    home.instructions = instructions(kind, dirs);
    // Memories first, checkpoints last; what doesn't fit stays, and is said.
    found.sort_by(|a, b| (a.root.weight(), &a.path).cmp(&(b.root.weight(), &b.path)));
    let mut total = 0u64;
    let mut left: BTreeMap<Root, (usize, u64)> = BTreeMap::new();
    for f in found {
        if f.size > max_file || total.saturating_add(f.size) > max_total {
            let e = left.entry(f.root).or_default();
            e.0 += 1;
            e.1 = e.1.saturating_add(f.size);
            continue;
        }
        let Ok(bytes) = std::fs::read(&f.full) else { continue };
        total = total.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        home.files.push(HomeFile { root: f.root, path: f.path, mode: f.mode, data: base64::engine::general_purpose::STANDARD.encode(bytes) });
    }
    for (root, (n, bytes)) in left {
        let its = if root.own() { "its " } else { "" };
        home.left.push(format!("{its}{} ({n} file{}, {} MB: more than a move carries)", root.label(), if n == 1 { "" } else { "s" }, bytes.div_ceil(1 << 20)));
    }
    home
}

/// Where `root`'s files go on this machine, for session `session` working in `cwd`; else why not.
fn place(root: Root, dirs: &Dirs, session: Option<&str>, cwd: &str) -> Result<PathBuf, String> {
    let config = dirs.claude();
    let own = |dir: PathBuf| session.map(|s| dir.join(s)).ok_or_else(|| "it has no session of its own here".to_owned());
    match root {
        Root::ClaudeMemory => match claude_memory_elsewhere(dirs, cwd) {
            Some(at) => Err(format!("this machine keeps Claude Code's memory in {at}")),
            None => Ok(claude_memory(dirs, cwd)),
        },
        Root::ClaudeSession => own(super::handoff::claude_project_dir(&config, cwd)),
        Root::ClaudeHistory => own(config.join("file-history")),
        Root::ClaudeTasks => own(config.join("tasks")),
        Root::ClaudeTodos => Ok(config.join("todos")),
        Root::ClaudePlans => Ok(config.join("plans")),
        Root::CodexMemories => Ok(dirs.codex().join("memories")),
        Root::CodexNotes => Ok(dirs.codex().join("memories_extensions")),
        Root::GeminiMemory => {
            let gemini = dirs.gemini();
            gemini_claim(&gemini, &gemini_key(cwd)).map(|id| gemini.join("tmp").join(id).join("memory")).ok_or_else(|| "Gemini CLI's projects here couldn't take it".to_owned())
        }
        Root::AntigravityKnowledge => Ok(dirs.antigravity().join("knowledge")),
        Root::GooseMemory => Ok(dirs.goose().join("memory")),
    }
}

fn write_file(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let tmp = path.with_file_name(format!(".{name}.kiln-move"));
    std::fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = if mode & 0o777 == 0 { 0o600 } else { mode & 0o777 };
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// A machine's name as it goes in a file's: letters, digits, `.`, `_` and `-`.
fn tag(from: &str) -> String {
    let t: String = from.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' }).collect();
    let t = t.trim_matches(['-', '.']).to_owned();
    if t.is_empty() { "elsewhere".to_owned() } else { t }
}

/// Where the file that came goes beside `path`, the one there that differs: `<stem>.from-<machine>
/// <.ext>` (`-2`, `-3`... when that's another's), and whether it is there already.
fn beside(path: &Path, from: &str, bytes: &[u8]) -> Option<(PathBuf, bool)> {
    let name = path.file_name()?.to_string_lossy().into_owned();
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem.to_owned(), format!(".{ext}")),
        _ => (name.clone(), String::new()),
    };
    let tag = tag(from);
    for n in 1..100 {
        let side = path.with_file_name(if n == 1 { format!("{stem}.from-{tag}{ext}") } else { format!("{stem}.from-{tag}-{n}{ext}") });
        match std::fs::read(&side) {
            Ok(b) if b == bytes => return Some((side, true)),
            Ok(_) => {}
            Err(_) => return Some((side, false)),
        }
    }
    None
}

/// `here`'s lines, then those of `came` it lacks, in their order (blank ones aside): None when it
/// lacks none.
fn merge_lines(here: &str, came: &str) -> Option<String> {
    let mut have: HashSet<&str> = here.lines().map(str::trim_end).collect();
    let add: Vec<&str> = came.lines().map(str::trim_end).filter(|l| !l.trim().is_empty() && have.insert(l)).collect();
    if add.is_empty() {
        return None;
    }
    let mut out = here.to_owned();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    for l in add {
        out.push_str(l);
        out.push('\n');
    }
    Some(out)
}

/// Goose's memories in a category's file: entries (an optional `# tags` line, then the memory), a
/// blank line after each.
fn entries(text: &str) -> Vec<&str> {
    text.split("\n\n").map(str::trim).filter(|e| !e.is_empty()).collect()
}

/// `here`'s entries, then those of `came` it lacks: None when it lacks none.
fn merge_entries(here: &str, came: &str) -> Option<String> {
    let mut have: HashSet<&str> = entries(here).into_iter().collect();
    let add: Vec<&str> = entries(came).into_iter().filter(|e| have.insert(e)).collect();
    if add.is_empty() {
        return None;
    }
    let mut out = here.trim_end().to_owned();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    for e in add {
        out.push_str(e);
        out.push_str("\n\n");
    }
    Some(out)
}

/// An index's links to the files that went beside the ones there (`(topic.md)`), made theirs.
fn relink(text: &str, moved: &[(String, String)]) -> String {
    moved.iter().fold(text.to_owned(), |t, (from, to)| t.replace(&format!("({from})"), &format!("({to})")))
}

/// How a root's files went.
#[derive(Default)]
struct Tally {
    came: usize,
    replaced: usize,
    merged: Vec<String>,
    beside: Vec<String>,
}

/// "a", "a and b", "a, b and c".
fn and_list<S: AsRef<str>>(items: &[S]) -> String {
    match items {
        [] => String::new(),
        [one] => one.as_ref().to_owned(),
        [rest @ .., last] => format!("{} and {}", rest.iter().map(AsRef::as_ref).collect::<Vec<_>>().join(", "), last.as_ref()),
    }
}

/// The first few of `v`, and how many more.
fn some_of(v: &[String]) -> String {
    let shown: Vec<&str> = v.iter().take(6).map(String::as_str).collect();
    format!("{}{}", shown.join(", "), if v.len() > 6 { format!(" and {} more", v.len() - 6) } else { String::new() })
}

/// Put `home` (from machine `from`) where harness `kind` looks for it on this machine, for session
/// `session` (its own id) going on in `cwd`; `folder`: that is the folder it worked in (else it
/// isn't on this machine, the session works in the home folder, and the project's memory stays
/// where it was). What it did, in words, for the session's timeline.
pub fn install(home: &Home, kind: &str, dirs: &Dirs, session: Option<&str>, cwd: &str, from: &str, folder: bool) -> Vec<String> {
    let session = session.filter(|s| plain(s));
    let mut places: HashMap<Root, Result<PathBuf, String>> = HashMap::new();
    let mut tally: BTreeMap<Root, Tally> = BTreeMap::new();
    let mut failed: Vec<String> = Vec::new();
    let mut stayed: Vec<&str> = Vec::new();
    // What went beside, by root: an index that came points at those.
    let mut moved: HashMap<Root, Vec<(String, String)>> = HashMap::new();
    // An index last, once what goes beside is known.
    let (index, rest): (Vec<&HomeFile>, Vec<&HomeFile>) = home.files.iter().partition(|f| f.root.merges(&f.path) == Some(Merge::Lines));
    for f in rest.into_iter().chain(index) {
        let what = format!("{} ({})", f.path, f.root.label());
        if f.root.harness() != kind || !allowed(f.root, &f.path, session) {
            failed.push(format!("{what}: not a place {kind} keeps it in"));
            continue;
        }
        if !folder && matches!(f.root, Root::ClaudeMemory | Root::GeminiMemory) {
            if !stayed.contains(&f.root.label()) {
                stayed.push(f.root.label());
            }
            continue;
        }
        let first = !places.contains_key(&f.root);
        let dir = match places.entry(f.root).or_insert_with(|| place(f.root, dirs, session, cwd)) {
            Ok(d) => d.clone(),
            // Said once for all of its files.
            Err(why) => {
                if first {
                    failed.push(format!("{} ({why})", f.root.label()));
                }
                continue;
            }
        };
        let Ok(mut bytes) = base64::engine::general_purpose::STANDARD.decode(&f.data) else {
            failed.push(format!("{what}: unreadable"));
            continue;
        };
        let merge = f.root.merges(&f.path);
        if merge == Some(Merge::Lines)
            && let Some(m) = moved.get(&f.root)
        {
            bytes = relink(&String::from_utf8_lossy(&bytes), m).into_bytes();
        }
        let path = dir.join(&f.path);
        let t = tally.entry(f.root).or_default();
        let done = match std::fs::read(&path) {
            Ok(here) if here == bytes => Ok(()),
            // The session's own, as it left it here: the one coming is newer.
            Ok(_) if f.root.own() => write_file(&path, &bytes, f.mode).map(|()| t.replaced += 1),
            Ok(here) if merge.is_some() => {
                let (here, came) = (String::from_utf8_lossy(&here), String::from_utf8_lossy(&bytes));
                let merged = if merge == Some(Merge::Lines) { merge_lines(&here, &came) } else { merge_entries(&here, &came) };
                match merged {
                    Some(text) => write_file(&path, text.as_bytes(), f.mode).map(|()| t.merged.push(f.path.clone())),
                    None => Ok(()),
                }
            }
            // This machine's stays; the one that came goes beside it.
            Ok(_) => match beside(&path, from, &bytes) {
                Some((side, there)) => {
                    let rel = side.strip_prefix(&dir).map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default();
                    moved.entry(f.root).or_default().push((f.path.clone(), rel.clone()));
                    if there { Ok(()) } else { write_file(&side, &bytes, f.mode).map(|()| t.beside.push(rel)) }
                }
                None => Err(std::io::Error::other("too many beside it already")),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => write_file(&path, &bytes, f.mode).map(|()| t.came += 1),
            Err(e) => Err(e),
        };
        if let Err(e) = done {
            failed.push(format!("{what}: {e}"));
        }
    }
    let mut said = Vec::new();
    let files = |n: usize| format!("{n} file{}", if n == 1 { "" } else { "s" });
    for (root, t) in tally.iter().filter(|(r, _)| !r.own()) {
        let mut parts = Vec::new();
        if t.came > 0 {
            parts.push(format!("{} new here", files(t.came)));
        }
        if !t.merged.is_empty() {
            let what = if *root == Root::GooseMemory { "memories" } else { "lines" };
            parts.push(format!("{} took the {what} {} lacked", and_list(&t.merged), if t.merged.len() == 1 { "it" } else { "they" }));
        }
        if !t.beside.is_empty() {
            let one = t.beside.len() == 1;
            parts.push(format!(
                "{} differ{} here, so this machine's stay{}, and the one{} that came went beside: {}",
                t.beside.len(),
                if one { "s" } else { "" },
                if one { "s" } else { "" },
                if one { "" } else { "s" },
                some_of(&t.beside)
            ));
        }
        if !parts.is_empty() {
            said.push(format!("{} came from {from}: {}", root.label(), parts.join("; ")));
        }
    }
    let own: Vec<&str> = tally.iter().filter(|(r, t)| r.own() && t.came + t.replaced > 0).map(|(r, _)| r.label()).collect();
    if !own.is_empty() {
        said.push(format!("Its {} came along", and_list(&own)));
    }
    if !stayed.is_empty() {
        said.push(format!("{} stays on {from}, as its folder isn't here", and_list(&stayed)));
    }
    if !failed.is_empty() {
        said.push(format!("These couldn't be put here, and stay on {from}: {}", some_of(&failed)));
    }
    if !home.left.is_empty() {
        said.push(format!("Left on {from}: {}", and_list(&home.left)));
    }
    if let (Some(came), Some(here)) = (&home.instructions, instructions(kind, dirs))
        && *came != here
    {
        let shown = instruction_files(kind, dirs).and_then(|v| v.into_iter().next()).map(|(_, p)| dirs.shown(&p)).unwrap_or_default();
        said.push(format!(
            "The person's own instructions to {} here ({shown}) aren't the ones it had on {from}: it goes on with this machine's",
            super::kinds::Kind::get(kind).map_or(kind, |k| k.name)
        ));
    }
    said
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(path: &Path, text: &str) -> anyhow::Result<()> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(path, text)?;
        Ok(())
    }

    /// A repository's main folder keys Claude Code's memory, from any folder in it and from its
    /// worktrees (one not made yet too); a folder outside any is its own.
    #[test]
    fn claude_codes_memory_is_the_main_folders_of_the_repository() -> anyhow::Result<()> {
        let t = tempfile::tempdir()?;
        let repo = t.path().join("kiln");
        std::fs::create_dir_all(repo.join(".git").join("worktrees").join("w"))?;
        put(&repo.join(".git").join("worktrees").join("w").join("commondir"), "../..\n")?;
        let wt = repo.join(".claude").join("worktrees").join("w");
        put(&wt.join(".git"), &format!("gitdir: {}\n", repo.join(".git").join("worktrees").join("w").display()))?;
        std::fs::create_dir_all(repo.join("crates"))?;
        assert_eq!(claude_project_base(&repo), repo);
        assert_eq!(claude_project_base(&repo.join("crates")), repo);
        assert_eq!(claude_project_base(&wt), repo);
        assert_eq!(claude_project_base(&repo.join(".claude").join("worktrees").join("not-yet")), repo);
        let plain = t.path().join("notes");
        assert_eq!(claude_project_base(&plain), plain);
        Ok(())
    }

    /// Gemini CLI's name for a folder: its registry's, else a marker's; a new one is the folder's
    /// name, numbered past those taken, and marked so Gemini CLI finds it.
    #[test]
    fn gemini_cli_finds_the_project_kiln_names_for_it() -> anyhow::Result<()> {
        let t = tempfile::tempdir()?;
        let g = t.path().join(".gemini");
        put(&g.join("projects.json"), r#"{"projects": {"/elsewhere/Kiln": "kiln", "/w/sylk": "sylk"}}"#)?;
        put(&g.join("tmp").join("kiln").join(".project_root"), "/elsewhere/Kiln")?;
        assert_eq!(gemini_known(&g, "/w/sylk").as_deref(), Some("sylk"));
        put(&g.join("history").join("hex").join(".project_root"), "/w/hex")?;
        assert_eq!(gemini_known(&g, "/w/hex").as_deref(), Some("hex"));
        assert_eq!(gemini_claim(&g, "/home/b/Projects/Kiln").as_deref(), Some("kiln-1"));
        assert_eq!(gemini_marker(&g.join("tmp").join("kiln-1")).as_deref(), Some("/home/b/Projects/Kiln"));
        // Claimed once: known from then on.
        assert_eq!(gemini_claim(&g, "/home/b/Projects/Kiln").as_deref(), Some("kiln-1"));
        assert_eq!(gemini_key("/w/a/../hex/"), "/w/hex");
        Ok(())
    }

    #[test]
    fn indexes_take_the_lines_and_entries_they_lack_and_nothing_twice() {
        let here = "- [Ours](ours.md) — hook\n- [Shared](shared.md) — same\n";
        let came = "- [Shared](shared.md) — same\n\n- [Theirs](theirs.md) — new\n- [Topic](topic.md) — differs\n";
        let came = relink(came, &[("topic.md".into(), "topic.from-mac.md".into())]);
        let merged = merge_lines(here, &came).unwrap_or_default();
        assert_eq!(merged, "- [Ours](ours.md) — hook\n- [Shared](shared.md) — same\n- [Theirs](theirs.md) — new\n- [Topic](topic.from-mac.md) — differs\n");
        assert_eq!(merge_lines(&merged, &came), None);
        let goose_here = "# deploy\nkiln deploys need approval\n\n";
        let goose_came = "# deploy\nkiln deploys need approval\n\nuse pnpm\n\n";
        let merged = merge_entries(goose_here, goose_came).unwrap_or_default();
        assert_eq!(merged, "# deploy\nkiln deploys need approval\n\nuse pnpm\n\n");
        assert_eq!(merge_entries(&merged, goose_came), None);
    }

    /// Too much: memories go first, checkpoints last, and what stays is said; a file too big for a
    /// move stays whatever it is.
    #[test]
    fn what_doesnt_fit_stays_memories_first() -> anyhow::Result<()> {
        let t = tempfile::tempdir()?;
        let config = t.path().join(".claude");
        let cwd = t.path().join("w");
        std::fs::create_dir_all(&cwd)?;
        let cwd = cwd.to_string_lossy().into_owned();
        let env = vec![("CLAUDE_CONFIG_DIR".to_owned(), config.to_string_lossy().into_owned())];
        let dirs = Dirs::at(t.path(), &env);
        let memory = claude_memory(&dirs, &cwd);
        put(&memory.join("MEMORY.md"), "- [A](a.md) — a\n")?;
        put(&memory.join("a.md"), &"a".repeat(100))?;
        put(&config.join("tasks").join("s1").join("1.json"), &"t".repeat(100))?;
        put(&config.join("file-history").join("s1").join("f@v1"), &"f".repeat(300))?;
        put(&config.join("file-history").join("s1").join("huge@v1"), &"h".repeat(2000))?;
        let home = pack_within("claude", &dirs, Some("s1"), &cwd, &[], None, (1000, 400));
        let roots: Vec<(Root, &str)> = home.files.iter().map(|f| (f.root, f.path.as_str())).collect();
        assert_eq!(roots, [(Root::ClaudeMemory, "MEMORY.md"), (Root::ClaudeMemory, "a.md"), (Root::ClaudeTasks, "1.json")]);
        assert_eq!(home.left, ["its checkpoints for rewind (2 files, 1 MB: more than a move carries)"]);
        Ok(())
    }

    /// A package can't write outside the harness's own folders, nor another harness's, nor a
    /// credential, nor the session's files without the session.
    #[test]
    fn nothing_goes_outside_the_harnesss_own_folders() -> anyhow::Result<()> {
        let t = tempfile::tempdir()?;
        let config = t.path().join(".claude");
        let env = vec![("CLAUDE_CONFIG_DIR".to_owned(), config.to_string_lossy().into_owned()), ("CODEX_HOME".to_owned(), t.path().join(".codex").to_string_lossy().into_owned())];
        let dirs = Dirs::at(t.path(), &env);
        let data = base64::engine::general_purpose::STANDARD.encode("pwned");
        let f = |root: Root, path: &str| HomeFile { root, path: path.into(), mode: 0o644, data: data.clone() };
        let home = Home {
            files: vec![
                f(Root::ClaudeMemory, "../../settings.json"),
                f(Root::ClaudeMemory, "/etc/passwd"),
                f(Root::ClaudeMemory, "a/../../x.md"),
                f(Root::ClaudeMemory, ".credentials.json"),
                f(Root::ClaudeMemory, "keys/id.pem"),
                f(Root::ClaudeTodos, "other-session.json"),
                f(Root::ClaudePlans, "nested/plan.md"),
                f(Root::CodexMemories, "MEMORY.md"),
                f(Root::ClaudeHistory, "abc@v1"),
            ],
            ..Home::default()
        };
        let said = install(&home, "claude", &dirs, None, &t.path().join("w").to_string_lossy(), "mac", true);
        let written: Vec<PathBuf> = walkdir(t.path());
        assert!(written.is_empty(), "{written:?}");
        assert!(said.iter().any(|s| s.starts_with("These couldn't be put here, and stay on mac") && s.contains("and 3 more")), "{said:?}");
        Ok(())
    }

    fn walkdir(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                out.extend(walkdir(&e.path()));
            } else {
                out.push(e.path());
            }
        }
        out
    }

    /// Codex's goal: from its database when it can be read (a usage limit's goes on), else as kiln
    /// saw it; a complete one doesn't go.
    #[test]
    fn codexs_goal_is_read_from_its_database_else_from_kilns_view() -> anyhow::Result<()> {
        let t = tempfile::tempdir()?;
        let db = rusqlite::Connection::open(t.path().join("goals_1.sqlite"))?;
        db.execute_batch(
            "CREATE TABLE thread_goals (thread_id TEXT PRIMARY KEY NOT NULL, goal_id TEXT NOT NULL, objective TEXT NOT NULL, status TEXT NOT NULL, token_budget INTEGER, tokens_used INTEGER NOT NULL DEFAULT 0, time_used_seconds INTEGER NOT NULL DEFAULT 0, created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL);
             INSERT INTO thread_goals VALUES ('th1', 'g1', 'port the parser', 'usage_limited', 5000, 10, 1, 1, 2);
             INSERT INTO thread_goals VALUES ('th2', 'g2', 'done already', 'complete', NULL, 10, 1, 1, 2);",
        )?;
        drop(db);
        assert_eq!(codex_goal(t.path(), "th1"), Ok(Some(Goal { objective: "port the parser".into(), status: "active".into(), token_budget: Some(5000) })));
        assert_eq!(codex_goal(t.path(), "th2"), Ok(None));
        assert_eq!(codex_goal(t.path(), "th3"), Ok(None));
        assert_eq!(codex_goal(&t.path().join("none"), "th1"), Err(()));
        let seen = Objective::new("ship it", "paused", None);
        let dirs = Dirs::at(t.path(), &[("CODEX_HOME".into(), t.path().join("none").to_string_lossy().into_owned())]);
        let home = pack("codex", &dirs, Some("th1"), "/w", &[], Some(&seen));
        assert_eq!(home.goal, Some(Goal { objective: "ship it".into(), status: "paused".into(), token_budget: None }));
        Ok(())
    }
}

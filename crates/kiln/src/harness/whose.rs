//! Whose a file an agent changed is (docs/agents.md, "Who changed a file, and whose it is"): the
//! Changes tab's scope shows this project's at first, then the person's, then everything.
//!
//! - **This project's** ([`FilePlace::Project`]): in a folder the session has worked in (where it
//!   started, and each it moved to), or in a working tree of one of their repositories wherever it
//!   is (a worktree under `~/.cache` is). The home folder, or a folder above it, is no project's:
//!   a session started there has the repositories it works in.
//! - **An agent's own** ([`FilePlace::Agent`]): in a harness's home (`super::homes::Dirs`), but what a
//!   person keeps there to tell it what to do ([`PERSON_FILES`], [`PERSON_DIRS`]); in kiln's own
//!   folders ([`note_kiln`]); in the machine's cache and temporary folders.
//! - **The person's** ([`FilePlace::User`]): anything else.
//!
//! Blocking: a file's working tree is found on disk.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use parking_lot::Mutex;

use super::model::FilePlace;

/// kiln's own folders in this process: its runner's state, its server's agents' record.
static KILN: LazyLock<Mutex<Vec<PathBuf>>> = LazyLock::new(|| Mutex::new(Vec::new()));

/// `dir` is kiln's own (a runner's state, a server's record): what an agent changes there is an
/// agent's own. So is its parent when that's kiln's too (`…/kiln/agent`).
pub fn note_kiln(dir: &Path) {
    let mut own = KILN.lock();
    let parent = dir.parent().filter(|p| p.file_name().is_some_and(|n| n == "kiln"));
    for d in std::iter::once(dir).chain(parent) {
        if !own.iter().any(|o| o == d) {
            own.push(d.to_path_buf());
        }
    }
}

/// Files a person keeps at the top of a harness's home to tell it what to do: its instructions
/// and its settings. Theirs, not the harness's own.
pub const PERSON_FILES: &[&str] = &["CLAUDE.md", "AGENTS.md", "AGENTS.override.md", "GEMINI.md", "settings.json", "settings.local.json", "keybindings.json", "config.toml", "config.yaml"];

/// Folders a person keeps at the top of a harness's home: its skills, commands, subagents,
/// output styles and prompts.
pub const PERSON_DIRS: &[&str] = &["skills", "commands", "agents", "output-styles", "prompts"];

/// Where a machine keeps what's an agent's own, and its home.
#[derive(Debug, Clone, Default)]
pub struct Roots {
    pub home: PathBuf,
    /// Each harness's home: its files are its own, but for [`PERSON_FILES`] and [`PERSON_DIRS`].
    pub harness: Vec<PathBuf>,
    /// Folders whose every file is an agent's own: kiln's, the cache, temporary files.
    pub own: Vec<PathBuf>,
}

impl Roots {
    /// This machine's, for a session whose environment is `env` (which may move a harness's home).
    pub fn here(env: &[(String, String)]) -> Self {
        let dirs = super::homes::Dirs::here(env);
        let mut own: Vec<PathBuf> = KILN.lock().clone();
        own.push(dirs.cache());
        own.push(std::env::temp_dir());
        if cfg!(unix) {
            own.extend([PathBuf::from("/tmp"), PathBuf::from("/var/tmp")]);
        }
        let mut roots = Self { home: dirs.home().to_path_buf(), harness: dirs.harness_homes(), own };
        roots.harness = with_real(std::mem::take(&mut roots.harness));
        roots.own = with_real(std::mem::take(&mut roots.own));
        roots
    }
}

/// Each folder as given and as it resolves (macOS's `/var` is `/private/var`): a harness names a
/// file either way.
fn with_real(mut dirs: Vec<PathBuf>) -> Vec<PathBuf> {
    let real: Vec<PathBuf> = dirs.iter().filter_map(|d| std::fs::canonicalize(d).ok()).collect();
    for r in real {
        if !dirs.contains(&r) {
            dirs.push(r);
        }
    }
    dirs
}

/// Whether `abs` is this project's: `in_tree` (its change was found in a working tree of its
/// folder's repository), or in one of `folders` (those the session has worked in), or in a working
/// tree of one of their repositories. A folder that's `home`, or above it, is none.
pub fn in_project(abs: &Path, in_tree: bool, folders: &[String], home: &Path) -> bool {
    if in_tree {
        return true;
    }
    let folders: Vec<&Path> = folders.iter().map(Path::new).filter(|f| !home.starts_with(f)).collect();
    if folders.iter().any(|f| abs.starts_with(f)) {
        return true;
    }
    let Some(file) = crate::workspace::git::place(abs) else { return false };
    folders.iter().any(|f| crate::workspace::git::place(f).is_some_and(|p| p.repo == file.repo))
}

/// Whose `abs` is, `project` saying it's this project's ([`in_project`]).
pub fn place_of(abs: &Path, project: bool, roots: &Roots) -> FilePlace {
    if project {
        return FilePlace::Project;
    }
    if let Some(rel) = roots.harness.iter().find_map(|h| abs.strip_prefix(h).ok()) {
        let mut parts = rel.components().filter_map(|c| c.as_os_str().to_str());
        let first = parts.next();
        let alone = parts.next().is_none();
        let person = first.is_some_and(|f| (alone && PERSON_FILES.contains(&f)) || (!alone && PERSON_DIRS.contains(&f)));
        return if person { FilePlace::User } else { FilePlace::Agent };
    }
    if roots.own.iter().any(|o| abs.starts_with(o)) {
        return FilePlace::Agent;
    }
    FilePlace::User
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn roots() -> Roots {
        Roots {
            home: PathBuf::from("/h"),
            harness: vec![PathBuf::from("/h/.claude"), PathBuf::from("/h/.codex"), PathBuf::from("/h/.gemini/antigravity-cli"), PathBuf::from("/h/.gemini")],
            own: vec![PathBuf::from("/h/.local/share/kiln"), PathBuf::from("/h/.cache"), PathBuf::from("/tmp")],
        }
    }

    #[test]
    fn whose_each_file_is() {
        let r = roots();
        let at = |p: &str, project: bool| place_of(Path::new(p), project, &r);
        // This project's wins wherever it is: a worktree of its repository under the cache.
        assert_eq!(at("/h/src/app/main.rs", true), FilePlace::Project);
        assert_eq!(at("/h/.cache/kiln-x/wt/src/a.rs", true), FilePlace::Project);
        // A harness's own state, kiln's, the cache and temporary files: an agent's own.
        assert_eq!(at("/h/.claude/projects/-h-src-app/memory/notes.md", false), FilePlace::Agent);
        assert_eq!(at("/h/.codex/sessions/2026/10/31/rollout.jsonl", false), FilePlace::Agent);
        assert_eq!(at("/h/.gemini/antigravity-cli/knowledge/a.md", false), FilePlace::Agent);
        assert_eq!(at("/h/.local/share/kiln/agent/browser/pictures/a.png", false), FilePlace::Agent);
        assert_eq!(at("/h/.cache/kiln-design/mock.html", false), FilePlace::Agent);
        assert_eq!(at("/tmp/retry-storm/repro.rs", false), FilePlace::Agent);
        // What a person keeps in a harness's home to tell it what to do is theirs.
        assert_eq!(at("/h/.claude/CLAUDE.md", false), FilePlace::User);
        assert_eq!(at("/h/.claude/settings.json", false), FilePlace::User);
        assert_eq!(at("/h/.claude/skills/review/SKILL.md", false), FilePlace::User);
        assert_eq!(at("/h/.codex/AGENTS.md", false), FilePlace::User);
        assert_eq!(at("/h/.codex/config.toml", false), FilePlace::User);
        // A settings file deeper in a harness's home is still the harness's.
        assert_eq!(at("/h/.claude/projects/x/settings.json", false), FilePlace::Agent);
        // Anything else it changed for the person.
        assert_eq!(at("/h/.zshrc", false), FilePlace::User);
        assert_eq!(at("/h/Projects/hyper-raft/src/log.rs", false), FilePlace::User);
    }

    fn git(dir: &Path, args: &[&str]) -> anyhow::Result<()> {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "kiln")
            .env("GIT_AUTHOR_EMAIL", "kiln@localhost")
            .env("GIT_COMMITTER_NAME", "kiln")
            .env("GIT_COMMITTER_EMAIL", "kiln@localhost")
            .output()?;
        anyhow::ensure!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        Ok(())
    }

    #[test]
    fn this_projects_files_are_in_its_folders_and_their_repositories_worktrees() -> anyhow::Result<()> {
        let t = tempfile::tempdir()?;
        let home = t.path().join("home");
        let project = home.join("src").join("app");
        let other = home.join("src").join("other");
        for repo in [&project, &other] {
            std::fs::create_dir_all(repo)?;
            git(repo, &["init", "-q"])?;
            std::fs::write(repo.join("a.rs"), "fn a() {}\n")?;
            git(repo, &["add", "."])?;
            git(repo, &["commit", "-q", "-m", "one"])?;
        }
        // A worktree of the project's repository, under the cache, and a scratch folder.
        let wt = home.join(".cache").join("kiln-x").join("wt");
        git(&project, &["worktree", "add", "-q", "--detach", &wt.to_string_lossy(), "HEAD"])?;
        let scratch = home.join(".cache").join("scratch");
        std::fs::create_dir_all(&scratch)?;
        let folders = vec![project.to_string_lossy().into_owned()];
        let mine = |p: &Path| in_project(p, false, &folders, &home);
        anyhow::ensure!(mine(&project.join("a.rs")), "its folder's file");
        anyhow::ensure!(mine(&project.join("new").join("b.rs")), "a file not made yet, in its folder");
        anyhow::ensure!(mine(&wt.join("a.rs")), "a worktree of its repository, wherever it is");
        anyhow::ensure!(!mine(&other.join("a.rs")), "another repository");
        anyhow::ensure!(!mine(&scratch.join("notes.md")), "a scratch folder");
        // A session started in the home folder has no project folder of its own.
        let at_home = vec![home.to_string_lossy().into_owned()];
        anyhow::ensure!(!in_project(&scratch.join("notes.md"), false, &at_home, &home), "the home folder is no project's");
        anyhow::ensure!(in_project(&scratch.join("notes.md"), true, &at_home, &home), "but a working tree of its repository is");
        Ok(())
    }
}

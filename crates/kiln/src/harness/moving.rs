//! An agent session moving to another runner (`docs/reach.md` §8): what goes, and how it is
//! taken over there.
//!
//! The runner the session is on packs it once its turn is over ([`pack`]): the harness's own
//! record of the conversation, kiln's timeline and state (its queue included), and its settings
//! with the folder it goes on in. The package goes over a reach link, end to end encrypted, as a
//! hand-over (`reach::node::Handle::adopt`); the runner it goes to puts the record where its harness
//! looks for it ([`install`]) and starts the session under the same id, its timeline as it was.
//!
//! - Claude Code: its transcript, as it is, in the project folder Claude Code keeps for the new
//!   folder; it resumes the same session.
//! - Codex: the conversation, as items for a new thread (the record of a thread is Codex's own
//!   database's), as a switch of harness carries it.
//! - ACP agents and Antigravity, whose own records kiln doesn't keep: the conversation in words,
//!   from kiln's timeline, which the harness there is given with the first message it gets
//!   ([`super::handoff::as_words`]).
//!
//! What the harness keeps for it in its own home besides goes too ([`super::homes`]): its
//! memories, merged into the ones there, and the session's own tasks, plans and checkpoints;
//! Codex's thread goal is set on the thread it goes on in ([`GOAL`]).

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::kinds::Protocol;
use super::model::{Event, State};
use super::session::Spec;

/// The package's format, newest: one of a version this runner doesn't know is refused, never
/// guessed at. 2 added [`Carry::Words`]; 3, what the harness keeps in its home
/// ([`Package::home`]). A package without them stays 1, so older runners still take it; one with
/// them, an older runner refuses whole rather than take it without them.
pub const VERSION: u32 = 3;

/// The format a package with `carry` is.
fn version_of(carry: &Carry) -> u32 {
    if matches!(carry, Carry::Words { .. }) { 2 } else { 1 }
}

/// Whether a runner taking packages up to [`VERSION`] reads one of `version`.
pub fn takes(version: u32) -> bool {
    (1..=VERSION).contains(&version)
}

/// The harness's record of the conversation, as the harness there takes it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Carry {
    /// Claude Code: its transcript's lines, for `--resume <session>`.
    ClaudeLines { session: String, lines: Vec<String> },
    /// A new thread's history (Codex's `thread/inject_items`).
    Seed { items: Vec<Value> },
    /// Nothing to carry (the harness never started a conversation).
    Nothing,
    /// The conversation in words, from kiln's timeline, for a harness whose own record kiln
    /// doesn't keep: it's given them with the first message it gets.
    Words { text: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Package {
    pub version: u32,
    /// kiln's id of the session: the same over there.
    pub id: String,
    /// What starts it there: its settings, the folder it goes on in.
    pub spec: Spec,
    /// The runner it comes from, and the folder it was in.
    pub from: String,
    pub from_cwd: String,
    pub carry: Carry,
    pub events: Vec<Event>,
    pub state: State,
    pub created_at: i64,
    /// Its turn was interrupted to move: it picks the turn up there.
    #[serde(default)]
    pub picks_up: bool,
    /// Where its folder stood in its repo's sync log once its last changes were in: it goes on
    /// there once the folder there has taken the log that far. None: its folder isn't synced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced: Option<crate::sync::runner::Mark>,
    /// Its repository besides its files: branches, tags, stash, what's staged, where HEAD is, and
    /// the commits the machine it goes to lacks (`super::gitstate`). None: not a synced repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<super::gitstate::GitState>,
    /// What git ignores in its repository, that a build doesn't make again (`super::carry`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignored: Option<super::carry::Carried>,
    /// The git worktree it works in, inside its repository (`super::worktree`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<super::worktree::Worktree>,
    /// What its harness keeps for it in its own home: its memories, its tasks, plans and
    /// checkpoints, its goal (`super::homes`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<super::homes::Home>,
}

impl Package {
    /// What its harness keeps for it in its home goes along: a package with something of it to put
    /// in place is version 3.
    #[must_use]
    pub fn with_home(mut self, home: super::homes::Home) -> Self {
        if home.installs() {
            self.version = self.version.max(3);
        }
        if home != super::homes::Home::default() {
            self.home = Some(home);
        }
        self
    }
}

/// What a moved session's repository waits with until its files are in: written on arrival beside
/// the session's files (`moved-git.json`), put in place before it goes on, then removed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MovedGit {
    pub from: String,
    pub state: super::gitstate::GitState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignored: Option<super::carry::Carried>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<super::worktree::Worktree>,
}

/// Where a moved session's [`MovedGit`] waits.
pub fn moved_git(session_files: &std::path::Path) -> std::path::PathBuf {
    session_files.join("moved-git.json")
}

/// Writes `bytes` at `path` readable by this user alone (it may hold secrets).
pub fn keep_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)
}

/// Variables of one machine's environment that mean nothing on another (its paths, its terminal,
/// kiln's own plumbing); the rest of a session's environment goes with it.
fn portable(env: &[(String, String)]) -> Vec<(String, String)> {
    const LOCAL: &[&str] = &["PATH", "HOME", "PWD", "OLDPWD", "SHELL", "USER", "LOGNAME", "TMPDIR", "TERM", "COLORTERM", "DISPLAY", "SSH_AUTH_SOCK", "SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY", "LANG", "LC_ALL"];
    env.iter()
        .filter(|(k, _)| !LOCAL.contains(&k.as_str()) && !k.starts_with("KILN_") && !k.starts_with("XDG_") && !k.starts_with("WAYLAND_") && !k.starts_with("DBUS_") && !k.starts_with("TERM_"))
        .cloned()
        .collect()
}

/// The conversation of a session whose harness's own record kiln doesn't keep: in words, from
/// `events` (kiln's timeline). Nothing when nothing was said yet.
pub fn words(events: &[Event], note: &str) -> Carry {
    let c = super::handoff::from_events(events);
    if c.entries.is_empty() {
        return Carry::Nothing;
    }
    Carry::Words { text: super::handoff::as_words(c, note, super::handoff::WORDS_BUDGET) }
}

/// The harness's record of `session` on this machine, for `protocol` (blocking: reads files);
/// `events`, kiln's timeline, for a harness whose own record kiln doesn't keep.
pub fn record(protocol: &Protocol, env: &[(String, String)], session: Option<&str>, events: &[Event], note: &str) -> Result<Carry, String> {
    if matches!(protocol, Protocol::Acp(_) | Protocol::AgyStreamJson) {
        return Ok(words(events, note));
    }
    let Some(session) = session.filter(|s| !s.is_empty()) else { return Ok(Carry::Nothing) };
    match protocol {
        Protocol::ClaudeStreamJson => {
            let path = super::transcript::find(&super::transcript::config_dir(env), session).ok_or_else(|| format!("Claude Code's record of session {session} is not on this machine"))?;
            let text = std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
            Ok(Carry::ClaudeLines { session: session.to_owned(), lines: text.lines().filter(|l| !l.trim().is_empty()).map(str::to_owned).collect() })
        }
        Protocol::CodexAppServer => {
            let conv = super::handoff::read_codex(env, session).ok_or_else(|| format!("Codex's record of thread {session} is not on this machine"))?;
            Ok(Carry::Seed { items: super::handoff::to_codex(&conv, Some(&format!("<kiln_handoff>\n{note}\n</kiln_handoff>"))) })
        }
        Protocol::Acp(_) | Protocol::AgyStreamJson => Ok(words(events, note)),
        Protocol::Terminal => Err("this harness's sessions can't move between machines yet".into()),
    }
}

/// Pack a session for runner `to`: `spec` is its resume spec, `cwd` the folder it goes on in there.
#[allow(clippy::too_many_arguments)]
pub fn pack(id: &str, mut spec: Spec, cwd: &str, from: &str, from_cwd: &str, carry: Carry, events: Vec<Event>, state: State) -> Package {
    spec.cwd = Some(cwd.to_owned());
    spec.env = portable(&spec.env);
    // A shell, a program and a take-over are the old machine's.
    spec.shell = None;
    spec.program = None;
    spec.takeover_pid = None;
    spec.takeover_stop = false;
    spec.prompt = None;
    spec.resume_at = None;
    spec.fork = false;
    match &carry {
        Carry::ClaudeLines { session, .. } => spec.resume = Some(session.clone()),
        Carry::Seed { .. } | Carry::Words { .. } | Carry::Nothing => spec.resume = None,
    }
    Package {
        version: version_of(&carry),
        id: id.to_owned(),
        spec,
        from: from.to_owned(),
        from_cwd: from_cwd.to_owned(),
        carry,
        events,
        state,
        created_at: crate::util::now_ms(),
        picks_up: false,
        synced: None,
        git: None,
        ignored: None,
        worktree: None,
        home: None,
    }
}

/// The transcript `carry` holds (Claude Code's), for what it names.
pub fn transcript_of(carry: &Carry) -> &[String] {
    match carry {
        Carry::ClaudeLines { lines, .. } => lines,
        Carry::Seed { .. } | Carry::Nothing | Carry::Words { .. } => &[],
    }
}

/// Every message of transcript `had` is among `lines`: `had` is an earlier copy of the same
/// conversation (records without an id of their own, such as summaries, don't count).
fn holds_all_of(lines: &[String], had: &str) -> bool {
    let id = |l: &str| serde_json::from_str::<Value>(l).ok().and_then(|v| v.get("uuid").and_then(Value::as_str).map(str::to_owned));
    let coming: HashSet<String> = lines.iter().filter_map(|l| id(l)).collect();
    had.lines().filter_map(id).all(|u| coming.contains(&u))
}

/// Put the harness's record where the harness here looks for it (blocking: writes files).
/// Returns the history a new thread starts with, for a harness that takes one.
pub fn install(pkg: &Package, cwd: &str) -> Result<Option<Vec<Value>>, String> {
    match &pkg.carry {
        Carry::ClaudeLines { session, lines } => {
            let config = super::transcript::config_dir(&pkg.spec.env);
            // A record of the session here already is the one it left moving away (it comes back):
            // the one coming holds all of it and what came since, and takes its place. One with
            // turns of its own (resumed here meanwhile) is never written over.
            let left = super::transcript::find(&config, session);
            if let Some(old) = &left {
                let had = std::fs::read_to_string(old).map_err(|e| format!("read {}: {e}", old.display()))?;
                if !holds_all_of(lines, &had) {
                    return Err(format!("Claude Code here has turns of session {session} that the one coming doesn't, and they are never written over"));
                }
            }
            let dir = super::handoff::claude_project_dir(&config, cwd);
            std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
            let path = dir.join(format!("{session}.jsonl"));
            let part = dir.join(format!(".{session}.jsonl.part"));
            let mut body = lines.join("\n");
            body.push('\n');
            if let Err(e) = std::fs::write(&part, body).and_then(|()| std::fs::rename(&part, &path)) {
                let _ = std::fs::remove_file(&part);
                return Err(format!("write {}: {e}", path.display()));
            }
            // The one it left in the folder it was in then: one record of a session, not two.
            if let Some(old) = left.filter(|o| *o != path) {
                let _ = std::fs::remove_file(old);
            }
            Ok(None)
        }
        Carry::Seed { items } => {
            let mut items = items.clone();
            // Its goal, set on the new thread once the conversation is in.
            if let Some(g) = pkg.home.as_ref().and_then(|h| h.goal.as_ref()) {
                items.push(serde_json::json!({"type": GOAL, "objective": g.objective, "status": g.status, "tokenBudget": g.token_budget}));
            }
            Ok(Some(items))
        }
        // Given to the harness with the first message it gets (`Adapter::seed`).
        Carry::Words { text } => Ok(Some(vec![serde_json::json!({"type": CONTEXT, "text": text})])),
        Carry::Nothing => Ok(None),
    }
}

/// Put what the harness keeps in its home for the session ([`Package::home`]) where the harness
/// here looks for it, for folder `cwd`; `folder`: that's the folder it worked in (else it isn't on
/// this machine). Blocking (writes files); what it did, in words, for its timeline. Once its
/// record is in ([`install`]): the session's own files take the place of what it left here only
/// once its record did.
pub fn install_home(pkg: &Package, cwd: &str, folder: bool) -> Vec<String> {
    install_home_in(pkg, &super::homes::Dirs::here(&pkg.spec.env), cwd, folder)
}

/// [`install_home`], into the harnesses' homes `dirs` names.
fn install_home_in(pkg: &Package, dirs: &super::homes::Dirs, cwd: &str, folder: bool) -> Vec<String> {
    let Some(home) = &pkg.home else { return Vec::new() };
    let session = match &pkg.carry {
        Carry::ClaudeLines { session, .. } => Some(session.as_str()),
        Carry::Seed { .. } | Carry::Nothing | Carry::Words { .. } => pkg.spec.resume.as_deref(),
    };
    super::homes::install(home, &pkg.spec.kind, dirs, session, cwd, &pkg.from, folder)
}

/// The item a seed carries the conversation in words as ([`Carry::Words`]).
pub const CONTEXT: &str = "kiln_context";

/// The item a seed carries the thread's goal as (Codex's: `thread/goal/set` on the new thread).
pub const GOAL: &str = "kiln_goal";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_harness_whose_record_kiln_doesnt_keep_moves_with_its_conversation_in_words() -> anyhow::Result<()> {
        use crate::harness::model::Body;
        let said = |text: &str| Event { id: text.into(), seq: 0, at: 0, parent: None, by: None, body: Body::Assistant { text: text.into(), streaming: false } };
        let protocol = Protocol::Acp(&["--acp"]);
        // Nothing said yet: nothing to carry.
        assert_eq!(record(&protocol, &[], None, &[], "moved").map_err(anyhow::Error::msg)?, Carry::Nothing);
        let carry = record(&protocol, &[], None, &[said("half way there")], "moved").map_err(anyhow::Error::msg)?;
        let Carry::Words { text } = &carry else { anyhow::bail!("not in words: {carry:?}") };
        assert!(text.contains("moved") && text.contains("half way there"));
        let spec = Spec { kind: "gemini".into(), resume: Some("acp-1".into()), ..Spec::default() };
        let pkg = pack("s1", spec, "/home/b/w", "a", "/home/a/w", carry, vec![], State::default());
        // A new conversation there, given the words; a package an older runner can tell it can't take.
        assert_eq!((pkg.spec.resume.as_deref(), pkg.version), (None, 2));
        assert!(takes(pkg.version) && takes(1) && takes(3) && !takes(4));
        let items = install(&pkg, "/home/b/w").map_err(anyhow::Error::msg)?.unwrap_or_default();
        assert_eq!(items.first().and_then(|i| i.get("type")).and_then(Value::as_str), Some(CONTEXT));
        // Claude Code's and Codex's packages stay as older runners read them.
        let claude = pack("s2", Spec { kind: "claude".into(), ..Spec::default() }, "/w", "a", "/w", Carry::Nothing, vec![], State::default());
        assert_eq!(claude.version, 1);
        Ok(())
    }

    #[test]
    fn a_package_carries_what_travels_and_leaves_the_machine_behind() -> anyhow::Result<()> {
        let spec = Spec {
            kind: "claude".into(),
            cwd: Some("/Users/ada/Projects/kiln".into()),
            model: Some("claude-opus-5-5".into()),
            mode: Some("acceptEdits".into()),
            effort: Some("high".into()),
            shell: Some("t1".into()),
            program: Some("/opt/claude".into()),
            env: vec![("PATH".into(), "/usr/bin".into()), ("ANTHROPIC_BASE_URL".into(), "http://mock".into()), ("KILN_TERM_SESSION".into(), "t1".into())],
            ..Spec::default()
        };
        let carry = Carry::ClaudeLines { session: "s1".into(), lines: vec!["{}".into()] };
        let p = pack("k1", spec, "~/Projects/kiln", "mac", "/Users/ada/Projects/kiln", carry, Vec::new(), State::default());
        assert_eq!(p.spec.cwd.as_deref(), Some("~/Projects/kiln"));
        assert_eq!(p.spec.resume.as_deref(), Some("s1"));
        assert_eq!((p.spec.model.as_deref(), p.spec.mode.as_deref(), p.spec.effort.as_deref()), (Some("claude-opus-5-5"), Some("acceptEdits"), Some("high")));
        assert_eq!(p.spec.env, vec![("ANTHROPIC_BASE_URL".to_owned(), "http://mock".to_owned())]);
        assert!(p.spec.shell.is_none() && p.spec.program.is_none());
        let again: Package = serde_json::from_slice(&serde_json::to_vec(&p)?)?;
        assert_eq!(again.carry, p.carry);
        // A package from a runner that never interrupts a turn to move picks nothing up.
        let mut older = serde_json::to_value(&p)?;
        older.as_object_mut().map(|o| o.remove("picks_up"));
        assert!(!serde_json::from_value::<Package>(older)?.picks_up);
        Ok(())
    }

    #[test]
    fn a_claude_transcript_lands_where_claude_code_looks_for_the_new_folder() -> anyhow::Result<()> {
        let config = tempfile::tempdir()?;
        let work = tempfile::tempdir()?;
        let cwd = work.path().to_string_lossy().into_owned();
        let spec = Spec { kind: "claude".into(), env: vec![("CLAUDE_CONFIG_DIR".into(), config.path().to_string_lossy().into_owned())], ..Spec::default() };
        let carry = Carry::ClaudeLines { session: "abc".into(), lines: vec![r#"{"type":"user"}"#.into(), r#"{"type":"assistant"}"#.into()] };
        let p = pack("k1", spec, &cwd, "mac", "/elsewhere", carry, Vec::new(), State::default());
        assert_eq!(install(&p, &cwd).map_err(|e| anyhow::anyhow!(e))?, None);
        let found = super::super::transcript::find(config.path(), "abc").ok_or_else(|| anyhow::anyhow!("not found"))?;
        assert!(found.starts_with(super::super::handoff::claude_project_dir(config.path(), &cwd)));
        assert_eq!(std::fs::read_to_string(found)?.lines().count(), 2);
        Ok(())
    }

    #[test]
    fn coming_back_it_takes_the_place_of_the_record_it_left_never_of_one_with_turns_of_its_own() -> anyhow::Result<()> {
        let config = tempfile::tempdir()?;
        let (then, now) = (tempfile::tempdir()?, tempfile::tempdir()?);
        let (then, now) = (then.path().to_string_lossy().into_owned(), now.path().to_string_lossy().into_owned());
        let spec = Spec { kind: "claude".into(), env: vec![("CLAUDE_CONFIG_DIR".into(), config.path().to_string_lossy().into_owned())], ..Spec::default() };
        let msg = |u: &str| format!(r#"{{"type":"user","uuid":"{u}"}}"#);
        let carry = |lines: Vec<String>| Carry::ClaudeLines { session: "abc".into(), lines };
        let put = |cwd: &str, lines: Vec<String>| install(&pack("k1", spec.clone(), cwd, "beta", "/elsewhere", carry(lines), Vec::new(), State::default()), cwd);
        // Here once, in one folder; it moved away and went on there.
        put(&then, vec![msg("u1"), msg("u2")]).map_err(|e| anyhow::anyhow!(e))?;
        // Back, with all of it and what came since (and a summary of its own), into another folder.
        put(&now, vec![r#"{"type":"summary","leafUuid":"u3"}"#.into(), msg("u1"), msg("u2"), msg("u3")]).map_err(|e| anyhow::anyhow!(e))?;
        let found = super::super::transcript::find(config.path(), "abc").ok_or_else(|| anyhow::anyhow!("not found"))?;
        assert!(found.starts_with(super::super::handoff::claude_project_dir(config.path(), &now)));
        assert_eq!(std::fs::read_to_string(&found)?.lines().count(), 4);
        assert!(!super::super::handoff::claude_project_dir(config.path(), &then).join("abc.jsonl").exists(), "one record of a session, not two");
        // A record here with a turn the one coming doesn't have stays as it is.
        let err = put(&now, vec![msg("u1"), msg("u2")]).err().unwrap_or_default();
        assert!(err.contains("never written over"), "{err}");
        assert_eq!(std::fs::read_to_string(&found)?.lines().count(), 4);
        Ok(())
    }

    use super::super::homes::{self, Dirs};
    use std::path::{Path, PathBuf};

    /// What a credential holds, wherever it is: never in a package.
    const SECRET: &str = "sk-live-never-travels";

    fn write(path: &Path, text: &str) -> anyhow::Result<()> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(path, text)?;
        Ok(())
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_else(|e| format!("<{}: {e}>", path.display()))
    }

    fn text(p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }

    /// Two machines, each with a home of its own (`a` and `b`, the harnesses' homes in them).
    fn machines() -> anyhow::Result<(tempfile::TempDir, PathBuf, PathBuf)> {
        let t = tempfile::tempdir()?;
        let (a, b) = (t.path().join("a"), t.path().join("b"));
        std::fs::create_dir_all(&a)?;
        std::fs::create_dir_all(&b)?;
        Ok((t, a, b))
    }

    fn claude_env(home: &Path) -> Vec<(String, String)> {
        vec![("CLAUDE_CONFIG_DIR".to_owned(), text(&home.join(".claude")))]
    }

    /// A package's way over: as JSON, which never holds a credential.
    fn over_the_wire(pkg: &Package) -> anyhow::Result<Package> {
        use base64::Engine as _;
        let wire = serde_json::to_string(pkg)?;
        assert!(!wire.contains(SECRET) && !wire.contains(&base64::engine::general_purpose::STANDARD.encode(SECRET)), "a credential travels");
        Ok(serde_json::from_str(&wire)?)
    }

    /// Claude Code, from a worktree of a repository on one machine to the same worktree of the
    /// repository's copy elsewhere on another: its transcript and the session's own files where
    /// Claude Code looks for the new folder; the project's memory merged into the one there, by the
    /// repository's main folder on each; neither its credentials nor its instructions.
    #[test]
    fn claude_codes_memory_tasks_plans_and_checkpoints_go_where_it_looks_for_the_new_folder() -> anyhow::Result<()> {
        let (_t, a, b) = machines()?;
        let (ca, cb) = (a.join(".claude"), b.join(".claude"));
        // The repository on a, the session in a worktree of it; its copy on b, elsewhere, the
        // worktree not made there yet.
        let repo_a = a.join("Projects").join("kiln");
        write(&repo_a.join(".git").join("worktrees").join("w").join("commondir"), "../..\n")?;
        let cwd_a = repo_a.join(".claude").join("worktrees").join("w");
        write(&cwd_a.join(".git"), &format!("gitdir: {}\n", text(&repo_a.join(".git").join("worktrees").join("w"))))?;
        let repo_b = b.join("src").join("kiln");
        std::fs::create_dir_all(repo_b.join(".git"))?;
        let cwd_b = repo_b.join(".claude").join("worktrees").join("w");
        // On a: the session's transcript and files, the project's memory, a credential, the
        // person's instructions.
        let lines: Vec<String> = vec![r#"{"type":"user","uuid":"u1","slug":"happy-plan"}"#.into(), r#"{"type":"assistant","uuid":"u2","slug":"happy-plan"}"#.into()];
        let project_a = super::super::handoff::claude_project_dir(&ca, &text(&cwd_a));
        write(&project_a.join("s1.jsonl"), &lines.join("\n"))?;
        write(&project_a.join("s1").join("subagents").join("agent-x.jsonl"), "{\"sub\":1}")?;
        write(&project_a.join("s1").join("tool-results").join("t1.txt"), "a long output")?;
        write(&ca.join("file-history").join("s1").join("abc@v1"), "before the edit")?;
        write(&ca.join("tasks").join("s1").join("1.json"), r#"{"id":"1","subject":"port the parser","status":"in_progress"}"#)?;
        write(&ca.join("tasks").join("s1").join(".highwatermark"), "1")?;
        write(&ca.join("tasks").join("s1").join(".lock"), "")?;
        write(&ca.join("todos").join("s1-agent-s1.json"), "[]")?;
        write(&ca.join("todos").join("other-agent-other.json"), "[\"not ours\"]")?;
        write(&ca.join("plans").join("happy-plan.md"), "# The plan")?;
        write(&ca.join("plans").join("happy-plan-agent-a1.md"), "# A subagent's plan")?;
        write(&ca.join("plans").join("someone-else.md"), "not ours")?;
        let memory_a = super::super::handoff::claude_project_dir(&ca, &text(&repo_a)).join("memory");
        write(&memory_a.join("MEMORY.md"), "- [Builds](builds.md) — cap them\n- [Deploys](deploys.md) — need approval\n")?;
        write(&memory_a.join("builds.md"), "a: cap builds at 20G")?;
        write(&memory_a.join("deploys.md"), "deploys need approval")?;
        write(&memory_a.join("team").join("shared.md"), "the team's")?;
        write(&ca.join(".credentials.json"), SECRET)?;
        write(&ca.join("CLAUDE.md"), "a's instructions")?;
        // On b: the project's memory of its own, and the person's instructions there.
        let memory_b = super::super::handoff::claude_project_dir(&cb, &text(&repo_b)).join("memory");
        write(&memory_b.join("MEMORY.md"), "- [Deploys](deploys.md) — need approval\n- [Own](own.md) — b's\n")?;
        write(&memory_b.join("builds.md"), "b: builds run on fedora")?;
        write(&memory_b.join("deploys.md"), "deploys need approval")?;
        write(&memory_b.join("own.md"), "b's own")?;
        write(&cb.join("CLAUDE.md"), "b's instructions")?;

        let home = homes::pack("claude", &Dirs::at(&a, &claude_env(&a)), Some("s1"), &text(&cwd_a), &lines, None);
        let carry = Carry::ClaudeLines { session: "s1".into(), lines };
        let spec = Spec { kind: "claude".into(), env: claude_env(&a), ..Spec::default() };
        let pkg = pack("k1", spec, &text(&cwd_b), "lab-a", &text(&cwd_a), carry, Vec::new(), State::default()).with_home(home);
        // A package an older runner refuses whole.
        assert_eq!(pkg.version, 3);
        assert!(takes(pkg.version) && !(1..=2).contains(&pkg.version));
        let mut pkg = over_the_wire(&pkg)?;
        // There, Claude Code's home is b's.
        pkg.spec.env = claude_env(&b);
        install(&pkg, &text(&cwd_b)).map_err(anyhow::Error::msg)?;
        let said = install_home_in(&pkg, &Dirs::at(&b, &claude_env(&b)), &text(&cwd_b), true);

        // Its transcript, and its own files beside it, in Claude Code's folder for the new folder.
        let project_b = super::super::handoff::claude_project_dir(&cb, &text(&cwd_b));
        assert_eq!(super::super::transcript::find(&cb, "s1"), Some(project_b.join("s1.jsonl")));
        assert_eq!(read(&project_b.join("s1").join("subagents").join("agent-x.jsonl")), "{\"sub\":1}");
        assert_eq!(read(&project_b.join("s1").join("tool-results").join("t1.txt")), "a long output");
        // Its checkpoints, tasks (their counter, not their lock), todos and plans: its own alone.
        assert_eq!(read(&cb.join("file-history").join("s1").join("abc@v1")), "before the edit");
        assert!(read(&cb.join("tasks").join("s1").join("1.json")).contains("port the parser"));
        assert_eq!(read(&cb.join("tasks").join("s1").join(".highwatermark")), "1");
        assert!(!cb.join("tasks").join("s1").join(".lock").exists());
        assert_eq!(read(&cb.join("todos").join("s1-agent-s1.json")), "[]");
        assert!(!cb.join("todos").join("other-agent-other.json").exists());
        assert_eq!(read(&cb.join("plans").join("happy-plan.md")), "# The plan");
        assert_eq!(read(&cb.join("plans").join("happy-plan-agent-a1.md")), "# A subagent's plan");
        assert!(!cb.join("plans").join("someone-else.md").exists());
        // The project's memory, merged by the repository's main folder: b's stays, its index takes
        // the lines it lacked, pointing at a's memory that differs, beside b's.
        assert_eq!(read(&memory_b.join("MEMORY.md")), "- [Deploys](deploys.md) — need approval\n- [Own](own.md) — b's\n- [Builds](builds.from-lab-a.md) — cap them\n");
        assert_eq!(read(&memory_b.join("builds.md")), "b: builds run on fedora");
        assert_eq!(read(&memory_b.join("builds.from-lab-a.md")), "a: cap builds at 20G");
        assert_eq!(read(&memory_b.join("own.md")), "b's own");
        assert!(!memory_b.join("team").exists(), "a team's memory is Claude Code's to bring");
        // Never a credential, nor a's instructions over b's: said instead.
        assert!(!cb.join(".credentials.json").exists());
        assert_eq!(read(&cb.join("CLAUDE.md")), "b's instructions");
        let all = said.join(". ");
        assert!(all.contains("Claude Code's memory of the project came from lab-a: MEMORY.md took the lines it lacked; 1 differs here, so this machine's stays, and the one that came went beside: builds.from-lab-a.md"), "{all}");
        assert!(all.contains("Its subagents' transcripts and tool results, checkpoints for rewind, tasks, todos and plans came along"), "{all}");
        assert!(all.contains("The person's own instructions to Claude Code here (~/.claude/CLAUDE.md) aren't the ones it had on lab-a"), "{all}");
        // Taken again (it comes back the same): nothing new, nothing twice.
        let again = install_home_in(&pkg, &Dirs::at(&b, &claude_env(&b)), &text(&cwd_b), true);
        assert!(again.iter().all(|s| !s.contains("came from")), "{again:?}");
        assert_eq!(read(&memory_b.join("MEMORY.md")).lines().count(), 3);
        assert!(!memory_b.join("builds.from-lab-a-2.md").exists());
        Ok(())
    }

    /// Codex: its memories merged into the ones there (but what it makes again from its
    /// database), its thread's goal set on the new thread with the conversation; neither its
    /// rollout nor its credentials.
    #[test]
    fn codexs_memories_merge_and_its_goal_goes_on_in_the_new_thread() -> anyhow::Result<()> {
        let (_t, a, b) = machines()?;
        let (xa, xb) = (a.join(".codex"), b.join(".codex"));
        write(&xa.join("memories").join("MEMORY.md"), "# a's handbook")?;
        write(&xa.join("memories").join("memory_summary.md"), "a's summary")?;
        write(&xa.join("memories").join("rollout_summaries").join("2026-10-20T10-00-00-port-parser.md"), "ported the parser")?;
        write(&xa.join("memories").join("raw_memories.md"), "made again from its database")?;
        write(&xa.join("memories_extensions").join("ad_hoc").join("notes").join("2026-10-21T09-00-00-use-pnpm.md"), "use pnpm")?;
        write(&xa.join("sessions").join("2026").join("10").join("22").join("rollout-2026-10-22T10-00-00-th1.jsonl"), "a rollout line")?;
        write(&xa.join("auth.json"), SECRET)?;
        write(&xa.join("AGENTS.md"), "the same instructions")?;
        let db = rusqlite::Connection::open(xa.join("goals_1.sqlite"))?;
        db.execute_batch(
            "CREATE TABLE thread_goals (thread_id TEXT PRIMARY KEY NOT NULL, goal_id TEXT NOT NULL, objective TEXT NOT NULL, status TEXT NOT NULL, token_budget INTEGER, tokens_used INTEGER NOT NULL DEFAULT 0, time_used_seconds INTEGER NOT NULL DEFAULT 0, created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL);
             INSERT INTO thread_goals VALUES ('th1', 'g1', 'port the parser', 'active', 200000, 1200, 60, 1, 2);",
        )?;
        drop(db);
        write(&xb.join("memories").join("MEMORY.md"), "# b's handbook")?;
        write(&xb.join("AGENTS.md"), "the same instructions")?;

        let home = homes::pack("codex", &Dirs::at(&a, &[]), Some("th1"), "/a/work", &[], None);
        let item = serde_json::json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "port the parser"}]});
        let spec = Spec { kind: "codex".into(), ..Spec::default() };
        let pkg = over_the_wire(&pack("k2", spec, "/b/work", "lab-a", "/a/work", Carry::Seed { items: vec![item.clone()] }, Vec::new(), State::default()).with_home(home))?;
        assert_eq!(pkg.version, 3);
        assert!(!serde_json::to_string(&pkg)?.contains("a rollout line"), "the rollout stays: the conversation goes as the new thread's history");
        // The new thread's history, then its goal.
        let items = install(&pkg, "/b/work").map_err(anyhow::Error::msg)?.unwrap_or_default();
        assert_eq!(items.first(), Some(&item));
        assert_eq!(items.get(1), Some(&serde_json::json!({"type": GOAL, "objective": "port the parser", "status": "active", "tokenBudget": 200000})));
        let said = install_home_in(&pkg, &Dirs::at(&b, &[]), "/b/work", true);
        let mem = xb.join("memories");
        assert_eq!(read(&mem.join("MEMORY.md")), "# b's handbook");
        assert_eq!(read(&mem.join("MEMORY.from-lab-a.md")), "# a's handbook");
        assert_eq!(read(&mem.join("memory_summary.md")), "a's summary");
        assert_eq!(read(&mem.join("rollout_summaries").join("2026-10-20T10-00-00-port-parser.md")), "ported the parser");
        assert!(!mem.join("raw_memories.md").exists());
        assert_eq!(read(&xb.join("memories_extensions").join("ad_hoc").join("notes").join("2026-10-21T09-00-00-use-pnpm.md")), "use pnpm");
        assert!(!xb.join("auth.json").exists() && !xb.join("sessions").exists() && !xb.join("goals_1.sqlite").exists());
        let all = said.join(". ");
        assert!(all.contains("Codex's memories came from lab-a: 2 files new here; 1 differs here, so this machine's stays, and the one that came went beside: MEMORY.from-lab-a.md"), "{all}");
        assert!(all.contains("Codex's memory notes came from lab-a: 1 file new here"), "{all}");
        assert!(!all.contains("instructions"), "the same instructions on both: {all}");
        Ok(())
    }

    /// Gemini CLI: its memory of the project goes to the project's folder there, under the name
    /// Gemini CLI gives the folder there (another folder has the one it had); not its chats,
    /// its inbox nor its credentials.
    #[test]
    fn gemini_clis_memory_of_the_project_goes_to_the_name_it_has_there() -> anyhow::Result<()> {
        let (_t, a, b) = machines()?;
        let (ga, gb) = (a.join(".gemini"), b.join(".gemini"));
        let (cwd_a, cwd_b) = (a.join("Projects").join("kiln"), b.join("work").join("kiln"));
        std::fs::create_dir_all(&cwd_a)?;
        std::fs::create_dir_all(&cwd_b)?;
        write(&ga.join("projects.json"), &serde_json::json!({"projects": {text(&cwd_a): "kiln"}}).to_string())?;
        write(&ga.join("tmp").join("kiln").join(".project_root"), &text(&cwd_a))?;
        write(&ga.join("tmp").join("kiln").join("memory").join("MEMORY.md"), "- [Tests](tests.md) — run them capped\n")?;
        write(&ga.join("tmp").join("kiln").join("memory").join("tests.md"), "cargo test, capped")?;
        write(&ga.join("tmp").join("kiln").join("memory").join(".inbox").join("x.patch"), "pending")?;
        write(&ga.join("tmp").join("kiln").join("chats").join("session-2026-10-22T10-00-abc.jsonl"), "a chat")?;
        write(&ga.join("oauth_creds.json"), SECRET)?;
        write(&ga.join("GEMINI.md"), "a's instructions")?;
        // On b, "kiln" is another folder's.
        write(&gb.join("projects.json"), r#"{"projects": {"/other/kiln": "kiln"}}"#)?;
        write(&gb.join("tmp").join("kiln").join(".project_root"), "/other/kiln")?;

        let events = vec![Event { id: "m1".into(), seq: 0, at: 0, parent: None, by: None, body: crate::harness::model::Body::Assistant { text: "tests pass".into(), streaming: false } }];
        let carry = words(&events, "moved");
        let home = homes::pack("gemini", &Dirs::at(&a, &[]), None, &text(&cwd_a), &[], None);
        let spec = Spec { kind: "gemini".into(), ..Spec::default() };
        let pkg = over_the_wire(&pack("k3", spec, &text(&cwd_b), "lab-a", &text(&cwd_a), carry, events, State::default()).with_home(home))?;
        assert_eq!(pkg.version, 3);
        assert!(!serde_json::to_string(&pkg)?.contains("a chat"));
        let items = install(&pkg, &text(&cwd_b)).map_err(anyhow::Error::msg)?.unwrap_or_default();
        assert_eq!(items.first().and_then(|i| i.get("type")).and_then(Value::as_str), Some(CONTEXT));
        let said = install_home_in(&pkg, &Dirs::at(&b, &[]), &text(&cwd_b), true);
        let mem = gb.join("tmp").join("kiln-1").join("memory");
        assert_eq!(read(&mem.join("MEMORY.md")), "- [Tests](tests.md) — run them capped\n");
        assert_eq!(read(&mem.join("tests.md")), "cargo test, capped");
        assert!(!mem.join(".inbox").exists() && !gb.join("tmp").join("kiln-1").join("chats").exists());
        // Gemini CLI finds the folder's by its marker.
        assert_eq!(read(&gb.join("tmp").join("kiln-1").join(".project_root")), text(&cwd_b));
        assert!(!gb.join("oauth_creds.json").exists());
        assert!(!gb.join("GEMINI.md").exists(), "instructions are the machine's");
        let all = said.join(". ");
        assert!(all.contains("Gemini CLI's memory of the project came from lab-a: 2 files new here"), "{all}");
        assert!(all.contains("The person's own instructions to Gemini CLI here (~/.gemini/GEMINI.md) aren't the ones it had on lab-a"), "{all}");
        // Its folder isn't there: it works in the home folder, and the project's memory stays.
        let elsewhere = install_home_in(&pkg, &Dirs::at(&b, &[]), &text(&b), false);
        assert!(elsewhere.iter().any(|s| s == "Gemini CLI's memory of the project stays on lab-a, as its folder isn't here"), "{elsewhere:?}");
        Ok(())
    }

    /// Antigravity's knowledge and Goose's memories, kept for every project: merged, the
    /// knowledge that differs beside, Goose's memories entry by entry.
    #[test]
    fn antigravitys_knowledge_and_gooses_memories_merge_into_the_ones_there() -> anyhow::Result<()> {
        let (_t, a, b) = machines()?;
        let (ka, kb) = (a.join(".gemini").join("antigravity-cli"), b.join(".gemini").join("antigravity-cli"));
        write(&ka.join("knowledge").join("ki-1").join("metadata.json"), r#"{"title":"kiln deploys","v":2}"#)?;
        write(&ka.join("knowledge").join("ki-1").join("artifacts").join("overview.md"), "deploys need approval")?;
        write(&ka.join("knowledge").join("knowledge.lock"), "")?;
        write(&ka.join("conversations").join("c1.db"), "a conversation")?;
        write(&ka.join("brain").join("c1").join("plan.md"), "its plan")?;
        write(&kb.join("knowledge").join("ki-1").join("metadata.json"), r#"{"title":"kiln deploys","v":1}"#)?;
        let carry = Carry::Words { text: "moved".into() };
        let home = homes::pack("antigravity", &Dirs::at(&a, &[]), Some("c1"), "/a/w", &[], None);
        let pkg = over_the_wire(&pack("k4", Spec { kind: "antigravity".into(), ..Spec::default() }, "/b/w", "lab-a", "/a/w", carry.clone(), Vec::new(), State::default()).with_home(home))?;
        assert!(!serde_json::to_string(&pkg)?.contains("a conversation"));
        let said = install_home_in(&pkg, &Dirs::at(&b, &[]), "/b/w", true);
        assert_eq!(read(&kb.join("knowledge").join("ki-1").join("metadata.json")), r#"{"title":"kiln deploys","v":1}"#);
        assert_eq!(read(&kb.join("knowledge").join("ki-1").join("metadata.from-lab-a.json")), r#"{"title":"kiln deploys","v":2}"#);
        assert_eq!(read(&kb.join("knowledge").join("ki-1").join("artifacts").join("overview.md")), "deploys need approval");
        assert!(!kb.join("knowledge").join("knowledge.lock").exists() && !kb.join("brain").exists() && !kb.join("conversations").exists());
        assert!(said.iter().any(|s| s.starts_with("Antigravity's knowledge came from lab-a: 1 file new here; 1 differs here")), "{said:?}");

        let (ga, gb) = (a.join(".config").join("goose"), b.join(".config").join("goose"));
        write(&ga.join("memory").join("development.txt"), "# rust\nuse cargo nextest\n\nprefer small commits\n\n")?;
        write(&ga.join("memory").join("personal.txt"), "the person is Ada\n\n")?;
        write(&ga.join("secrets.yaml"), SECRET)?;
        write(&ga.join(".goosehints"), "a's hints")?;
        write(&gb.join("memory").join("development.txt"), "prefer small commits\n\n")?;
        write(&gb.join(".goosehints"), "a's hints")?;
        let home = homes::pack("goose", &Dirs::at(&a, &[]), None, "/a/w", &[], None);
        let pkg = over_the_wire(&pack("k5", Spec { kind: "goose".into(), ..Spec::default() }, "/b/w", "lab-a", "/a/w", carry, Vec::new(), State::default()).with_home(home))?;
        let said = install_home_in(&pkg, &Dirs::at(&b, &[]), "/b/w", true);
        assert_eq!(read(&gb.join("memory").join("development.txt")), "prefer small commits\n\n# rust\nuse cargo nextest\n\n");
        assert_eq!(read(&gb.join("memory").join("personal.txt")), "the person is Ada\n\n");
        assert!(!gb.join("secrets.yaml").exists());
        let all = said.join(". ");
        assert!(all.contains("Goose's memories came from lab-a: 1 file new here; development.txt took the memories it lacked"), "{all}");
        assert!(!all.contains("instructions"), "{all}");
        Ok(())
    }

    /// opencode, Amp and Devin keep nothing of the kind: their records are the session's own, it
    /// goes on in a new session there with the conversation in words, and the package stays one
    /// older runners take.
    #[test]
    fn opencode_amp_and_devin_keep_nothing_that_travels() -> anyhow::Result<()> {
        let (_t, a, _b) = machines()?;
        let data = a.join(".local").join("share");
        write(&data.join("opencode").join("storage").join("session").join("p1").join("ses_1.json"), "{}")?;
        write(&data.join("opencode").join("storage").join("todo").join("ses_1.json"), "[]")?;
        write(&data.join("opencode").join("auth.json"), SECRET)?;
        write(&a.join(".config").join("opencode").join("AGENTS.md"), "a's instructions")?;
        write(&data.join("amp").join("secrets.json"), SECRET)?;
        write(&data.join("devin").join("cli").join("sessions.db"), "sessions")?;
        write(&data.join("devin").join("credentials.toml"), SECRET)?;
        for kind in ["opencode", "amp", "devin"] {
            let home = homes::pack(kind, &Dirs::at(&a, &[]), Some("ses_1"), "/a/w", &[], None);
            assert!(home.files.is_empty() && home.goal.is_none() && home.left.is_empty(), "{kind}: {home:?}");
            // The person's instructions are said, not carried (Devin's aren't known).
            assert_eq!(home.instructions.is_some(), kind != "devin", "{kind}");
            let pkg = over_the_wire(
                &pack("k6", Spec { kind: kind.into(), ..Spec::default() }, "/b/w", "lab-a", "/a/w", Carry::Words { text: "moved".into() }, Vec::new(), State::default()).with_home(home),
            )?;
            assert_eq!(pkg.version, 2, "{kind}");
        }
        Ok(())
    }
}

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

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::kinds::Protocol;
use super::model::{Event, State};
use super::session::Spec;

/// The package's format, newest: one of a version this runner doesn't know is refused, never
/// guessed at. 2 added [`Carry::Words`]; a package without it stays 1, so older runners still take
/// it.
pub const VERSION: u32 = 2;

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
    Package { version: version_of(&carry), id: id.to_owned(), spec, from: from.to_owned(), from_cwd: from_cwd.to_owned(), carry, events, state, created_at: crate::util::now_ms(), picks_up: false, synced: None, git: None, ignored: None, worktree: None }
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
        Carry::Seed { items } => Ok(Some(items.clone())),
        // Given to the harness with the first message it gets (`Adapter::seed`).
        Carry::Words { text } => Ok(Some(vec![serde_json::json!({"type": CONTEXT, "text": text})])),
        Carry::Nothing => Ok(None),
    }
}

/// The item a seed carries the conversation in words as ([`Carry::Words`]).
pub const CONTEXT: &str = "kiln_context";

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
        assert!(takes(pkg.version) && takes(1) && !takes(3));
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
}

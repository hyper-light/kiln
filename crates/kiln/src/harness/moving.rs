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

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::kinds::Protocol;
use super::model::{Event, State};
use super::session::Spec;

/// The package's format; one of another version is refused, never guessed at.
pub const VERSION: u32 = 1;

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

/// The harness's record of `session` on this machine, for `protocol` (blocking: reads files).
pub fn record(protocol: &Protocol, env: &[(String, String)], session: Option<&str>, note: &str) -> Result<Carry, String> {
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
        Protocol::Acp(_) | Protocol::Terminal => Err("this harness's sessions can't move between machines yet".into()),
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
        Carry::Seed { .. } | Carry::Nothing => spec.resume = None,
    }
    Package { version: VERSION, id: id.to_owned(), spec, from: from.to_owned(), from_cwd: from_cwd.to_owned(), carry, events, state, created_at: crate::util::now_ms(), picks_up: false }
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
        Carry::Nothing => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

//! An agent session moving to another runner (`docs/reach.md` §9): what goes, and how it is
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
    Package { version: VERSION, id: id.to_owned(), spec, from: from.to_owned(), from_cwd: from_cwd.to_owned(), carry, events, state, created_at: crate::util::now_ms() }
}

/// Put the harness's record where the harness here looks for it (blocking: writes files).
/// Returns the history a new thread starts with, for a harness that takes one.
pub fn install(pkg: &Package, cwd: &str) -> Result<Option<Vec<Value>>, String> {
    match &pkg.carry {
        Carry::ClaudeLines { session, lines } => {
            let config = super::transcript::config_dir(&pkg.spec.env);
            if super::transcript::find(&config, session).is_some() {
                return Err(format!("Claude Code here already has a session {session}"));
            }
            let dir = super::handoff::claude_project_dir(&config, cwd);
            std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
            let path = dir.join(format!("{session}.jsonl"));
            let mut body = lines.join("\n");
            body.push('\n');
            std::fs::write(&path, body).map_err(|e| format!("write {}: {e}", path.display()))?;
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
        // Twice is refused: a session is never written over.
        assert!(install(&p, &cwd).is_err());
        Ok(())
    }
}

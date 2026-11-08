//! Whose a commit is (docs/ci.md W28-R67): agents push with their machine's git identity, so a
//! commit's author doesn't say which agent made it. kiln's records do, in this order:
//!
//! 1. the agent token it was pushed with (`agent:<id>`, minted for that push alone);
//! 2. the agent change request whose last pushed commit it is (kiln's `agents/change/<agent>`);
//! 3. a `Kiln-Agent: <name> (<harness>, <id>)` line in its message;
//! 4. else the event's actor: a person.
//!
//! Fair share by actor (W28-R66) takes the actor; the explanation says where it came from.

use serde::Serialize;

/// Where an actor came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    Token,
    Change,
    Trailer,
    Event,
}

/// A commit's actor: an agent (`agent:<id>`) or a person, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Actor {
    pub id: String,
    pub source: Source,
    pub why: String,
}

/// An agent's change request as kiln keeps it: the agent, the commit it last pushed, its change.
#[derive(Debug, Clone, Copy, Default)]
pub struct Change<'a> {
    pub agent: &'a str,
    pub sha: &'a str,
    pub change: Option<&'a str>,
}

/// A push: its commit, who pushed it (the event's actor: `agent:<id>` for an agent token), and its
/// message.
#[derive(Debug, Clone, Copy, Default)]
pub struct Push<'a> {
    pub sha: &'a str,
    pub pusher: &'a str,
    pub message: &'a str,
}

/// The agent a `Kiln-Agent: <name> (<harness>, <id>)` line names, as `agent:<id>`.
pub fn trailer(message: &str) -> Option<String> {
    message.lines().rev().find_map(|line| {
        let rest = line.trim().strip_prefix("Kiln-Agent:")?;
        let inner = rest.rsplit_once('(')?.1.strip_suffix(')')?;
        let id = inner.rsplit(',').next()?.trim();
        (!id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')).then(|| format!("agent:{id}"))
    })
}

/// Who pushed, as [`Push::pusher`] takes it, from what kiln kept of the push: the agent whose token
/// it came with (`agent:<id>`, the first word of the caller kiln kept for it, `agent:<id> for
/// <whom>`), else the event's actor.
pub fn pusher(started_by: Option<&str>, actor: &str) -> String {
    started_by.and_then(|name| name.split(' ').next()).map(identity).filter(|w| w.strip_prefix("agent:").is_some_and(|id| !id.is_empty())).map_or_else(|| actor.to_owned(), str::to_owned)
}

/// The identity a principal's first word is (W26-R18): an agent's or a kiln shell's session by its
/// whole id (`agent:<id>`, `shell:<id>`), whatever runner its call came through (`agent:<id>@<runner>`:
/// a session is one wherever it runs, and the one its own token names, `agent:<id>`); any other as it
/// is (a token's label, `agent:review`; a person, `user:ada`).
pub fn identity(word: &str) -> &str {
    match word.split_once('@') {
        Some((name, _)) if ["agent:", "shell:"].iter().any(|p| name.strip_prefix(p).is_some_and(is_session)) => name,
        _ => word,
    }
}

/// Whether `id` is a session's whole id as kiln makes them: 32 lowercase hex digits.
fn is_session(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `text` (a principal, or words naming them) as people read it: each session named by its whole id
/// (`agent:<id>`, `shell:<id>`) named by its id's first eight digits, as kiln always showed them.
/// Display only: what's kept and compared is the whole id ([`identity`]).
pub fn shown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = ["agent:", "shell:"].iter().filter_map(|p| rest.find(p).map(|i| (i, p.len()))).min() {
        let (i, n) = at;
        let (before, from) = rest.split_at(i.saturating_add(n));
        out.push_str(before);
        let id = from.get(..32).filter(|id| is_session(id) && !from.get(32..33).is_some_and(|c| c.bytes().all(|b| b.is_ascii_alphanumeric())));
        match id {
            Some(id) => {
                out.push_str(id.get(..8).unwrap_or(id));
                rest = from.get(32..).unwrap_or_default();
            }
            None => rest = from,
        }
    }
    out.push_str(rest);
    out
}

/// Whose `push` is.
pub fn attribute(push: &Push, changes: &[Change]) -> Actor {
    if let Some(id) = push.pusher.strip_prefix("agent:").filter(|i| !i.is_empty()) {
        return Actor { id: format!("agent:{id}"), source: Source::Token, why: "pushed with the agent's own token".into() };
    }
    if !push.sha.is_empty()
        && let Some(c) = changes.iter().find(|c| !c.agent.is_empty() && c.sha.eq_ignore_ascii_case(push.sha))
    {
        let what = c.change.map_or_else(|| "its change request".to_owned(), |n| format!("its change {n}"));
        return Actor { id: format!("agent:{}", c.agent.trim_start_matches("agent:")), source: Source::Change, why: format!("the commit {what} last pushed") };
    }
    if let Some(id) = trailer(push.message) {
        return Actor { id, source: Source::Trailer, why: "its message's Kiln-Agent line".into() };
    }
    let who = if push.pusher.is_empty() { "unknown" } else { push.pusher };
    Actor { id: who.to_owned(), source: Source::Event, why: "who pushed it".into() }
}

#[cfg(test)]
mod tests {
    use super::{Change, Push, Source, attribute, pusher, trailer};

    /// W28-R67: a push made with an agent's token is the agent's, whatever the commit's author;
    /// anything else is its event's actor.
    /// W26-R18 (the DB query audit, 2026-11-06): a session's identity is its whole id, whatever runner
    /// its call came through; a principal naming only an id's start, a token's label or a person is as it
    /// is. People read the whole id by its start.
    #[test]
    fn w26_r18_a_session_is_its_whole_id() {
        use super::{identity, shown};
        let (a, b) = ("01a1b00011112222333344445555aaaa", "01a1b00099998888777766665555bbbb");
        assert_eq!(identity(&format!("agent:{a}@m1")), format!("agent:{a}"));
        assert_eq!(identity(&format!("agent:{b}@m1")), format!("agent:{b}"), "the same start, the same runner: another session");
        assert_eq!(identity(&format!("agent:{a}")), format!("agent:{a}"), "its own token");
        assert_eq!(identity(&format!("shell:{a}@m1")), format!("shell:{a}"));
        for kept in ["agent:01a1b000@m1", "agent:review", "user:ada", "admin", "agent:a@b@c"] {
            assert_eq!(identity(kept), kept);
        }
        assert_eq!(pusher(Some(&format!("agent:{a}@m1 for user:ada")), "ada"), format!("agent:{a}"));
        assert_eq!(shown(&format!("agent:{a}@m1 for user:ada")), "agent:01a1b000@m1 for user:ada");
        assert_eq!(shown(&format!("shell:{a} and agent:{b}")), "shell:01a1b000 and agent:01a1b000");
        assert_eq!(shown("agent:01a1b000@m1 for user:ada"), "agent:01a1b000@m1 for user:ada", "a start alone, as it was");
        assert_eq!(shown(&format!("agent:{a}ff")), format!("agent:{a}ff"), "not an id: more digits");
    }

    #[test]
    fn w28_r67_the_pusher_is_the_token_s_agent() {
        assert_eq!(pusher(Some("agent:01a1eeee for user:ada"), "Test"), "agent:01a1eeee");
        assert_eq!(pusher(Some("agent:01a1eeee for admin"), "Test"), "agent:01a1eeee");
        assert_eq!(pusher(Some("user:ada"), "Test"), "Test");
        assert_eq!(pusher(Some("pat:01a1ffff of user:ada"), "ada"), "ada");
        assert_eq!(pusher(Some("agent: for admin"), "ada"), "ada");
        assert_eq!(pusher(None, "ada"), "ada");
        let a = attribute(&Push { sha: "abc", pusher: &pusher(Some("agent:01a1eeee for admin"), "Test"), message: "plain" }, &[]);
        assert_eq!((a.id.as_str(), a.source), ("agent:01a1eeee", Source::Token));
    }

    /// W28-R67: each commit gets the actor of the first source that names one, and says which.
    #[test]
    fn w28_r67_attribution() {
        let changes = [Change { agent: "01a1aaaa", sha: "abc123", change: Some("CR12") }];
        let a = attribute(&Push { sha: "zzz", pusher: "agent:01a1bbbb", message: "" }, &changes);
        assert_eq!((a.id.as_str(), a.source), ("agent:01a1bbbb", Source::Token));
        let a = attribute(&Push { sha: "ABC123", pusher: "admin", message: "Kiln-Agent: x (devin, 01a1cccc)" }, &changes);
        assert_eq!((a.id.as_str(), a.source), ("agent:01a1aaaa", Source::Change));
        assert!(a.why.contains("CR12"));
        let msg = "Fix the parser\n\nThe body.\n\nKiln-Agent: Fix parser (claude, 01a1dddd)\n";
        let a = attribute(&Push { sha: "def", pusher: "admin", message: msg }, &changes);
        assert_eq!((a.id.as_str(), a.source), ("agent:01a1dddd", Source::Trailer));
        let a = attribute(&Push { sha: "def", pusher: "ada", message: "plain" }, &changes);
        assert_eq!((a.id.as_str(), a.source), ("ada", Source::Event));
        assert_eq!(trailer("Kiln-Agent: broken"), None);
        assert_eq!(trailer("Kiln-Agent: a (b, c d)"), None);
    }
}

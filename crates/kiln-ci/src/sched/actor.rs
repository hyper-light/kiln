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
    use super::{Change, Push, Source, attribute, trailer};

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

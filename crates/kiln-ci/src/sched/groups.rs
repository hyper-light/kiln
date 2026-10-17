//! Concurrency groups (docs/ci.md W05-R06, W05-R07), GitHub-exact, for runs (workflow-level
//! `concurrency`) and legs (job-level): at most `limit` members in progress (GitHub's is one);
//! with `queue: single` one pending member, which a newer one supersedes; with `queue: max` up to
//! `max_pending` waiting in order, and a member arriving to a full queue is cancelled;
//! `cancel-in-progress` also cancels the members in progress. A cancelled member keeps its place
//! until it has stopped, so the next one never overlaps it.

use std::collections::{BTreeMap, VecDeque};

/// How a group behaves, from its members' `concurrency`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupConfig {
    pub cancel_in_progress: bool,
    pub queue_max: bool,
    /// kiln: members in progress at once (GitHub: 1).
    pub limit: usize,
    /// `queue: max`'s bound (`[ci] max_pending`, GitHub's 100).
    pub max_pending: usize,
}

impl Default for GroupConfig {
    fn default() -> Self {
        GroupConfig { cancel_in_progress: false, queue_max: false, limit: 1, max_pending: 100 }
    }
}

/// What a group decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupAction {
    /// The member may start.
    Start(String),
    /// The member is cancelled: `running` when it was in progress (it must be stopped), else it
    /// never started.
    Cancel { member: String, running: bool, reason: String },
}

#[derive(Debug, Clone, Default)]
struct Group {
    running: Vec<String>,
    pending: VecDeque<String>,
    limit: usize,
}

/// Every live group, by key (scope and name).
#[derive(Debug, Clone, Default)]
pub struct Groups {
    groups: BTreeMap<String, Group>,
}

impl Groups {
    /// A member arrives in group `key` (its name shown as `name`).
    pub fn arrive(&mut self, key: &str, name: &str, member: &str, cfg: &GroupConfig) -> Vec<GroupAction> {
        let g = self.groups.entry(key.to_owned()).or_default();
        g.limit = cfg.limit.max(1);
        let superseded = format!("cancelled: superseded in group {name}");
        let mut out = Vec::new();
        // With `cancel-in-progress`, the members in progress are stopped; the newcomer starts once
        // they have.
        let cancelling = cfg.cancel_in_progress && !g.running.is_empty();
        if cancelling {
            for m in &g.running {
                out.push(GroupAction::Cancel { member: m.clone(), running: true, reason: superseded.clone() });
            }
        }
        if !cfg.queue_max || cfg.cancel_in_progress {
            for m in g.pending.drain(..) {
                out.push(GroupAction::Cancel { member: m, running: false, reason: superseded.clone() });
            }
        }
        if !cancelling && g.running.len() < g.limit && g.pending.is_empty() {
            g.running.push(member.to_owned());
            out.push(GroupAction::Start(member.to_owned()));
        } else if cfg.queue_max && g.pending.len() >= cfg.max_pending {
            out.push(GroupAction::Cancel { member: member.to_owned(), running: false, reason: format!("cancelled: the queue of group {name} is full ({} pending)", cfg.max_pending) });
        } else {
            g.pending.push_back(member.to_owned());
        }
        out
    }

    /// A member finished, was cancelled or went away: its place frees, and waiting members start
    /// in order.
    pub fn leave(&mut self, key: &str, member: &str) -> Vec<GroupAction> {
        let mut out = Vec::new();
        let Some(g) = self.groups.get_mut(key) else { return out };
        g.running.retain(|m| m != member);
        g.pending.retain(|m| m != member);
        while g.running.len() < g.limit.max(1) {
            let Some(next) = g.pending.pop_front() else { break };
            g.running.push(next.clone());
            out.push(GroupAction::Start(next));
        }
        if g.running.is_empty() && g.pending.is_empty() {
            self.groups.remove(key);
        }
        out
    }

    /// Who holds the group now (for the wait reason `concurrency`).
    pub fn holders(&self, key: &str) -> Vec<String> {
        self.groups.get(key).map(|g| g.running.clone()).unwrap_or_default()
    }

    pub fn is_pending(&self, key: &str, member: &str) -> bool {
        self.groups.get(key).is_some_and(|g| g.pending.iter().any(|m| m == member))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn starts(a: &[GroupAction]) -> Vec<&str> {
        a.iter().filter_map(|x| if let GroupAction::Start(m) = x { Some(m.as_str()) } else { None }).collect()
    }

    fn cancels(a: &[GroupAction]) -> Vec<(&str, bool)> {
        a.iter().filter_map(|x| if let GroupAction::Cancel { member, running, .. } = x { Some((member.as_str(), *running)) } else { None }).collect()
    }

    /// W05-AC01: GitHub's behaviours, each.
    #[test]
    fn w05_ac01_single() {
        let (mut g, c) = (Groups::default(), GroupConfig::default());
        assert_eq!(starts(&g.arrive("k", "ci", "r1", &c)), vec!["r1"]);
        // One in progress, one pending; a newer pending member supersedes the older.
        assert!(g.arrive("k", "ci", "r2", &c).is_empty());
        assert_eq!(cancels(&g.arrive("k", "ci", "r3", &c)), vec![("r2", false)]);
        assert!(g.is_pending("k", "r3") && !g.is_pending("k", "r2"));
        assert_eq!(g.holders("k"), vec!["r1"]);
        assert_eq!(starts(&g.leave("k", "r1")), vec!["r3"]);
        assert!(g.leave("k", "r3").is_empty() && g.holders("k").is_empty());
    }

    #[test]
    fn w05_ac01_cancel_in_progress() {
        let mut g = Groups::default();
        let c = GroupConfig { cancel_in_progress: true, ..GroupConfig::default() };
        assert_eq!(starts(&g.arrive("k", "ci", "r1", &c)), vec!["r1"]);
        let a = g.arrive("k", "ci", "r2", &c);
        assert_eq!(cancels(&a), vec![("r1", true)]);
        assert!(starts(&a).is_empty(), "r2 waits until r1 has stopped");
        let a = g.arrive("k", "ci", "r3", &c);
        assert_eq!(cancels(&a), vec![("r1", true), ("r2", false)]);
        assert_eq!(starts(&g.leave("k", "r1")), vec!["r3"]);
        // The cancellation reason names the group.
        assert!(matches!(g.arrive("k", "ci", "r4", &c).first(), Some(GroupAction::Cancel { reason, .. }) if reason == "cancelled: superseded in group ci"));
    }

    #[test]
    fn w05_ac01_queue_max_and_limit() {
        let mut g = Groups::default();
        let c = GroupConfig { queue_max: true, max_pending: 2, ..GroupConfig::default() };
        g.arrive("k", "ci", "r1", &c);
        assert!(g.arrive("k", "ci", "r2", &c).is_empty() && g.arrive("k", "ci", "r3", &c).is_empty());
        assert_eq!(cancels(&g.arrive("k", "ci", "r4", &c)), vec![("r4", false)], "the queue is full");
        assert_eq!(starts(&g.leave("k", "r1")), vec!["r2"], "in order");
        assert_eq!(starts(&g.leave("k", "r2")), vec!["r3"]);
        // kiln's limit: two in progress at once.
        let mut g = Groups::default();
        let c = GroupConfig { limit: 2, ..GroupConfig::default() };
        assert_eq!(starts(&g.arrive("s", "staging", "a", &c)), vec!["a"]);
        assert_eq!(starts(&g.arrive("s", "staging", "b", &c)), vec!["b"]);
        assert!(g.arrive("s", "staging", "c", &c).is_empty());
        assert_eq!(starts(&g.leave("s", "a")), vec!["c"]);
        // A pending member that goes away just leaves.
        assert!(g.arrive("s", "staging", "d", &c).is_empty());
        assert!(g.leave("s", "d").is_empty());
        assert!(!g.is_pending("s", "d"));
    }
}

//! The gray and fail-slow grade: what requesters see, compared across peers (S8; Panorama's requester
//! view, PF[12]; IASO's peer comparison, PF[13]). A requester reports each piece of work a node did as
//! its slowdown: the time it took over the time the work's class expects, in thousandths. Each window
//! the grade takes every node's median slowdown and the median of those medians across the cell: a
//! node whose median is [`SLOW_FACTOR_MILLI`] / 1000 times its peers' is slow for that window.
//!
//! - Suspect (no new placements) on its first slow window.
//! - Drain after [`DRAIN_WINDOWS`] slow windows in a row (CPI²'s 3 flags before acting, PF[16]).
//! - Once suspected, a window is still slow at [`STAY_FACTOR_MILLI`] / 1000 times its peers': a
//!   median is known only to the histogram's 25%, so a dip under the bar but within it is no evidence
//!   of recovery, and resetting on it would put off the drain.
//! - Healthy again on a window with evidence that isn't slow.
//!
//! A window with fewer than [`MIN_OBSERVATIONS`] for a node is no evidence about it, and with fewer
//! than [`MIN_PEERS`] nodes with evidence there is no peer to compare with: the grade holds. A
//! cell-wide slowdown moves the peer median with it, so it suspects nobody: that's a correlated fault,
//! for the penalties' caps and the provider correlation to read, not a node's.

use std::collections::BTreeMap;

use crate::histogram::Histogram;
use crate::{HealthError, Level};

/// One window: 1 s, so Suspect lands within two windows of a fault's start and Drain within four,
/// both inside Panorama's 7 s (PF[12]).
pub const WINDOW_NS: u64 = 1_000_000_000;
/// Observations of a node in a window that make it evidence.
pub const MIN_OBSERVATIONS: u32 = 5;
/// Nodes with evidence in a window needed to compare: one node can't be told from its peers alone,
/// and two can't say which of them is slow.
pub const MIN_PEERS: usize = 3;
/// A node is slow when its median slowdown is at least twice its peers' median.
pub const SLOW_FACTOR_MILLI: u64 = 2_000;
/// The bar a suspected node stays slow at: [`SLOW_FACTOR_MILLI`] less the histogram's 25% resolution.
pub const STAY_FACTOR_MILLI: u64 = 1_500;
/// Slow windows in a row before a drain: CPI²'s "flagged as an outlier at least 3 times" (PF[16]).
pub const DRAIN_WINDOWS: u32 = 3;

#[derive(Debug, Clone)]
struct Node {
    window: Histogram,
    slow_windows: u32,
    level: Level,
}

#[derive(Debug, Clone)]
pub struct PeerGrade {
    nodes: BTreeMap<u64, Node>,
    max_nodes: usize,
    window_start: u64,
}

/// What a closed window said about a node whose grade changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Change {
    pub node: u64,
    pub level: Level,
    /// Its median slowdown and its peers', in thousandths.
    pub median_milli: u64,
    pub peers_milli: u64,
}

impl PeerGrade {
    pub fn new(max_nodes: usize, start_ns: u64) -> Self {
        Self {
            nodes: BTreeMap::new(),
            max_nodes,
            window_start: start_ns,
        }
    }

    pub fn level(&self, node: u64) -> Level {
        self.nodes.get(&node).map_or(Level::Healthy, |n| n.level)
    }

    /// A piece of work on `node` ended at `now_ns`, `slowdown_milli` thousandths of its expected time.
    /// Closes any windows that ended first.
    pub fn observe(
        &mut self,
        node: u64,
        now_ns: u64,
        slowdown_milli: u64,
        changes: &mut Vec<Change>,
    ) -> Result<(), HealthError> {
        self.tick(now_ns, changes)?;
        if !self.nodes.contains_key(&node) && self.nodes.len() >= self.max_nodes {
            return Err(HealthError::Full {
                what: "graded nodes",
                bound: self.max_nodes,
            });
        }
        let entry = self.nodes.entry(node).or_insert_with(|| Node {
            window: Histogram::new(),
            slow_windows: 0,
            level: Level::Healthy,
        });
        entry.window.record(slowdown_milli)
    }

    /// Forget a node that left the cell.
    pub fn remove(&mut self, node: u64) {
        self.nodes.remove(&node);
    }

    /// Close every window that ended by `now_ns`, appending each grade change.
    pub fn tick(&mut self, now_ns: u64, changes: &mut Vec<Change>) -> Result<(), HealthError> {
        if now_ns < self.window_start {
            return Err(HealthError::TimeBackwards {
                what: "peer grade",
                now_ns,
                last_ns: self.window_start,
            });
        }
        // A window that would end past the last instant runs to it (saturation is the meaning).
        if now_ns < self.window_start.saturating_add(WINDOW_NS) {
            return Ok(());
        }
        self.close(changes);
        // Windows with nothing in them since change nothing; skip to the one holding `now_ns`.
        let elapsed = now_ns.saturating_sub(self.window_start);
        let skipped = elapsed / WINDOW_NS;
        self.window_start = skipped
            .checked_mul(WINDOW_NS)
            .and_then(|d| self.window_start.checked_add(d))
            .ok_or(HealthError::Overflow("grade window"))?;
        Ok(())
    }

    fn close(&mut self, changes: &mut Vec<Change>) {
        let mut medians: Vec<u64> = self
            .nodes
            .values()
            .filter(|n| n.window.total() >= MIN_OBSERVATIONS)
            .filter_map(|n| n.window.median())
            .collect();
        let peers = if medians.len() >= MIN_PEERS {
            medians.sort_unstable();
            medians.get(medians.len() / 2).copied()
        } else {
            None
        };
        for (id, node) in &mut self.nodes {
            let evidence = node.window.total() >= MIN_OBSERVATIONS;
            let median = node.window.median();
            node.window.clear();
            let (Some(peers), Some(median), true) = (peers, median, evidence) else {
                continue;
            };
            let factor = if node.level == Level::Healthy {
                SLOW_FACTOR_MILLI
            } else {
                STAY_FACTOR_MILLI
            };
            let slow = peers
                .checked_mul(factor)
                .is_some_and(|bar| median.saturating_mul(1000) >= bar);
            let before = node.level;
            if slow {
                node.slow_windows = node.slow_windows.saturating_add(1);
                node.level = if node.slow_windows >= DRAIN_WINDOWS {
                    Level::Drain
                } else {
                    Level::Suspect
                };
            } else {
                node.slow_windows = 0;
                node.level = Level::Healthy;
            }
            if node.level != before {
                changes.push(Change {
                    node: *id,
                    level: node.level,
                    median_milli: median,
                    peers_milli: peers,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = WINDOW_NS;

    /// Feeds `per_window` observations to each of `nodes` nodes for `windows` windows from `from`,
    /// node 0 at `slow0` thousandths and the rest at 1000; returns every change with its time.
    fn feed(
        g: &mut PeerGrade,
        nodes: u64,
        from: u64,
        windows: u64,
        per_window: u64,
        slow0: u64,
    ) -> Vec<(u64, Change)> {
        let mut out = Vec::new();
        for w in 0..windows {
            for k in 0..per_window {
                let t = from + w * S + k * (S / per_window);
                for n in 0..nodes {
                    let mut ch = Vec::new();
                    g.observe(n, t, if n == 0 { slow0 } else { 1000 + n }, &mut ch)
                        .unwrap();
                    out.extend(ch.into_iter().map(|c| (t, c)));
                }
            }
        }
        out
    }

    /// S8 gray/fail-slow: a node whose work runs 3x slower than its peers' is suspected within two
    /// windows and drained within four, both under Panorama's 7 s.
    #[test]
    fn a_slow_node_is_suspected_then_drained_within_its_bound() {
        let mut g = PeerGrade::new(64, 0);
        assert!(feed(&mut g, 10, 0, 5, 10, 1000).is_empty());
        let onset = 5 * S + S / 3;
        let changes = feed(&mut g, 10, onset, 6, 10, 3000);
        let suspect = changes
            .iter()
            .find(|(_, c)| c.level == Level::Suspect)
            .unwrap();
        let drain = changes
            .iter()
            .find(|(_, c)| c.level == Level::Drain)
            .unwrap();
        assert_eq!(suspect.1.node, 0);
        assert!(suspect.0 - onset <= 2 * S, "{}", suspect.0 - onset);
        assert!(drain.0 - onset <= 4 * S, "{}", drain.0 - onset);
        assert!(drain.0 - onset < 7 * S);
        assert!(changes.iter().all(|(_, c)| c.node == 0));
        assert_eq!(g.level(0), Level::Drain);
    }

    /// S8 gray/fail-slow (F2): a suspected node whose median dips for a window under the 2x bar but
    /// within the histogram's resolution of it (1.75x) is still slow, and drains at its third window;
    /// a healthy node at 1.75x is never suspected.
    #[test]
    fn a_dip_within_the_resolution_does_not_reset_a_suspect() {
        let mut g = PeerGrade::new(64, 0);
        feed(&mut g, 10, 0, 5, 10, 1000);
        let mut changes = feed(&mut g, 10, 5 * S, 1, 10, 3000);
        changes.extend(feed(&mut g, 10, 6 * S, 1, 10, 1750));
        changes.extend(feed(&mut g, 10, 7 * S, 1, 10, 3000));
        changes.extend(feed(&mut g, 10, 8 * S, 1, 10, 3000));
        let drain = changes
            .iter()
            .find(|(_, c)| c.level == Level::Drain)
            .unwrap();
        assert!(drain.0 <= 8 * S, "{}", drain.0);
        assert!(changes.iter().all(|(_, c)| c.level != Level::Healthy));
        let mut g = PeerGrade::new(64, 0);
        assert!(feed(&mut g, 10, 0, 8, 10, 1750).is_empty());
    }

    /// A cell-wide slowdown (every node 3x) suspects nobody: the peers moved with it.
    #[test]
    fn a_cell_wide_slowdown_suspects_no_node() {
        let mut g = PeerGrade::new(64, 0);
        let mut changes = Vec::new();
        for w in 0..10 {
            for k in 0..10 {
                for n in 0..10 {
                    let slow = if w >= 3 { 3000 } else { 1000 };
                    g.observe(n, w * S + k * (S / 10), slow + n, &mut changes)
                        .unwrap();
                }
            }
        }
        assert!(changes.is_empty(), "{changes:?}");
    }

    /// Too little evidence decides nothing: a slow node seen 4 times a window, or a cell of 2.
    #[test]
    fn too_little_evidence_holds_the_grade() {
        let mut g = PeerGrade::new(64, 0);
        assert!(feed(&mut g, 10, 0, 6, 4, 5000).is_empty());
        let mut g = PeerGrade::new(64, 0);
        assert!(feed(&mut g, 2, 0, 6, 10, 5000).is_empty());
    }

    /// A node that recovers is healthy again on its first good window with evidence.
    #[test]
    fn a_recovered_node_is_healthy_again() {
        let mut g = PeerGrade::new(64, 0);
        feed(&mut g, 10, 0, 5, 10, 4000);
        assert_eq!(g.level(0), Level::Drain);
        let changes = feed(&mut g, 10, 5 * S, 3, 10, 1000);
        assert_eq!(changes.last().unwrap().1.level, Level::Healthy);
        assert_eq!(g.level(0), Level::Healthy);
    }

    /// Hostile input: time going backwards and a full map are typed refusals; a maximal time and a
    /// zero or maximal slowdown are taken.
    #[test]
    fn hostile_input_is_refused_typed() {
        let mut g = PeerGrade::new(2, 10 * S);
        let mut ch = Vec::new();
        assert!(matches!(
            g.observe(0, S, 1000, &mut ch),
            Err(HealthError::TimeBackwards { .. })
        ));
        g.observe(0, 10 * S, 0, &mut ch).unwrap();
        g.observe(1, 10 * S, u64::MAX, &mut ch).unwrap();
        assert_eq!(
            g.observe(2, 10 * S, 1000, &mut ch),
            Err(HealthError::Full {
                what: "graded nodes",
                bound: 2
            })
        );
        g.observe(0, u64::MAX - 1, 1000, &mut ch).unwrap();
        g.remove(1);
        g.observe(2, u64::MAX - 1, 1000, &mut ch).unwrap();
    }
}

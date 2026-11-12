//! What a kill drill checks once its cluster is whole again: every acknowledged publish in every
//! node's committed log at the offset it was acknowledged at, no payload twice, and the three
//! logs alike.

use std::collections::{BTreeMap, BTreeSet};

/// What a drill's publisher heard: each payload acknowledged, at its offset; each refused or
/// lost with its node (it may or may not have been appended: never twice).
#[derive(Debug, Default)]
pub struct Heard {
    pub acked: BTreeMap<String, u64>,
    pub unknown: BTreeSet<String>,
}

/// Checks `logs`, each node's messages from offset 0 (offset, payload), against `heard`.
pub fn check(heard: &Heard, logs: &[(u64, Vec<(u64, String)>)]) -> Result<(), String> {
    let Some((_, first)) = logs.first() else {
        return Err("no logs".into());
    };
    for (node, log) in logs {
        if log != first {
            return Err(format!(
                "node {node}'s log differs from node {}'s: {} against {} messages",
                logs[0].0,
                log.len(),
                first.len()
            ));
        }
        let mut seen = BTreeSet::new();
        for (at, (offset, payload)) in log.iter().enumerate() {
            if *offset != at as u64 {
                return Err(format!("node {node}: offset {offset} at place {at}"));
            }
            if !seen.insert(payload.clone()) {
                return Err(format!("node {node}: {payload:?} appended twice"));
            }
            if !heard.acked.contains_key(payload) && !heard.unknown.contains(payload) {
                return Err(format!("node {node}: {payload:?} was never published"));
            }
        }
        for (payload, offset) in &heard.acked {
            match log.get(usize::try_from(*offset).unwrap()) {
                Some((_, kept)) if kept == payload => {}
                other => {
                    return Err(format!(
                        "node {node}: {payload:?}, acknowledged at {offset}, lost (found {other:?})"
                    ));
                }
            }
        }
    }
    Ok(())
}

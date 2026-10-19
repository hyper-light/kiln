//! Delivery metrics (docs/deploy-api.md §18), from the releases themselves: DORA's four keys. How
//! often changes land, how long a release takes from asked to landed, how many fail or are rolled
//! back, and how long it takes to run something good again after one did.
//!
//! A change is a release that changes what runs (a deploy, a config release or a rollback); it
//! landed once it succeeded, whatever replaced it since (`superseded`, `rolled_back`). A failed
//! change failed, or was rolled back. It is restored when a later change of its registration
//! lands: from when it failed, or from when its rollback was asked for.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::model::{Kind, Release, State};

fn landed(r: &Release) -> bool {
    matches!(r.state, State::Succeeded | State::Superseded | State::RolledBack)
}

fn change(r: &Release) -> bool {
    matches!(r.kind, Kind::Deploy | Kind::Config | Kind::Rollback)
}

/// The `p`th percentile (0 to 100) of some durations, nearest rank.
fn percentile(sorted: &[i64], p: usize) -> Option<i64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (p * sorted.len()).div_ceil(100).clamp(1, sorted.len());
    sorted.get(rank - 1).copied()
}

fn spread(mut v: Vec<i64>) -> Value {
    v.sort_unstable();
    json!({ "count": v.len(), "p50": percentile(&v, 50), "p90": percentile(&v, 90) })
}

/// The day a time falls on, UTC: `2026-10-19`.
fn day(secs: i64) -> String {
    super::repo::rfc3339(secs).get(..10).unwrap_or_default().to_owned()
}

/// The metrics of `releases` (any order, those of a registration or many) over the `days` before
/// `now`.
pub fn compute(releases: &[Release], now: i64, days: i64) -> Value {
    let since = now - days.max(1) * 86_400;
    let mut by_id: BTreeMap<i64, &Release> = BTreeMap::new();
    for r in releases {
        by_id.insert(r.id, r);
    }
    let window: Vec<&Release> = by_id.values().copied().filter(|r| change(r) && r.created_at >= since).collect();
    let landed_changes: Vec<&Release> = window.iter().copied().filter(|r| landed(r)).collect();
    let failed: Vec<&Release> = window.iter().copied().filter(|r| r.state == State::Failed || r.state == State::RolledBack).collect();
    let lead: Vec<i64> = landed_changes.iter().filter_map(|r| r.ended_at.map(|e| e - r.created_at)).filter(|d| *d >= 0).collect();

    // Restored: the next change of the same registration that landed after it went wrong.
    let mut restore = Vec::new();
    for f in &failed {
        let (start, after) = match (f.state, f.rolled_back_by.and_then(|b| by_id.get(&b))) {
            (State::RolledBack, Some(b)) => (b.created_at, b.created_at),
            _ => match f.ended_at {
                Some(e) => (e, e),
                None => continue,
            },
        };
        let next = by_id
            .values()
            .filter(|r| r.app == f.app && r.environment == f.environment && change(r) && landed(r) && r.id != f.id)
            .filter_map(|r| r.ended_at.filter(|e| *e >= after))
            .min();
        if let Some(e) = next {
            restore.push(e - start);
        }
    }

    let attempted = landed_changes.len() + window.iter().filter(|r| r.state == State::Failed).count();
    let mut days_map: BTreeMap<String, (u32, u32)> = BTreeMap::new();
    for r in &window {
        let at = r.ended_at.unwrap_or(r.created_at);
        let e = days_map.entry(day(at)).or_default();
        if landed(r) {
            e.0 += 1;
        }
        if r.state == State::Failed {
            e.1 += 1;
        }
    }
    let landed_n = landed_changes.len();
    json!({
        "days": days.max(1),
        "since": since,
        "changes": window.len(),
        "landed": landed_n,
        "per_day": (landed_n as f64) / (days.max(1) as f64),
        "lead_time_secs": spread(lead),
        "failed": failed.len(),
        "change_failure_rate": if attempted == 0 { Value::Null } else { json!((failed.len() as f64) / (attempted as f64)) },
        "time_to_restore_secs": spread(restore),
        "by_day": days_map.into_iter().map(|(d, (l, f))| json!({ "day": d, "landed": l, "failed": f })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deploy::model::Origin;

    fn rel(id: i64, kind: Kind, state: State, created: i64, ended: Option<i64>) -> Release {
        Release {
            id,
            app: "web".into(),
            environment: "production".into(),
            kind,
            state,
            origin: Origin::Api,
            version: Some(format!("1.{id}")),
            artifacts: BTreeMap::new(),
            values: None,
            commit: None,
            requested_by: "ada".into(),
            message: None,
            rollback_of: None,
            rolled_back_by: None,
            keep: false,
            provider: Value::Null,
            gates: Vec::new(),
            approvals: Vec::new(),
            promoted_from: None,
            idempotency_key: None,
            created_at: created,
            committed_at: None,
            ended_at: ended,
        }
    }

    #[test]
    fn the_four_keys_come_from_the_releases() {
        let now = 10 * 86_400;
        let mut bad = rel(2, Kind::Deploy, State::RolledBack, now - 5_000, Some(now - 4_800));
        bad.rolled_back_by = Some(3);
        let list = vec![
            rel(1, Kind::Deploy, State::Superseded, now - 9_000, Some(now - 8_400)),
            bad,
            rel(3, Kind::Rollback, State::Superseded, now - 4_000, Some(now - 3_700)),
            rel(4, Kind::Deploy, State::Failed, now - 3_000, Some(now - 2_900)),
            rel(5, Kind::Deploy, State::Succeeded, now - 2_000, Some(now - 1_500)),
            rel(6, Kind::Enable, State::Succeeded, now - 1_000, Some(now - 900)),
            rel(7, Kind::Deploy, State::Succeeded, now - 40 * 86_400, Some(now - 40 * 86_400 + 60)),
        ];
        let m = compute(&list, now, 30);
        assert_eq!(m.get("changes"), Some(&json!(5)), "the enable and the old release don't count");
        assert_eq!(m.get("landed"), Some(&json!(4)));
        assert_eq!(m.get("failed"), Some(&json!(2)), "one failed, one rolled back");
        assert_eq!(m.get("change_failure_rate"), Some(&json!(2.0 / 5.0)), "of 4 landed and 1 failed");
        assert_eq!(m.pointer("/lead_time_secs/p50"), Some(&json!(300)), "of 200, 300, 500 and 600");
        // Rolled back: from its rollback asked (now-4000) to it landing (now-3700); failed: from
        // now-2900 to the next landing (now-1500).
        assert_eq!(m.pointer("/time_to_restore_secs/count"), Some(&json!(2)));
        assert_eq!(m.pointer("/time_to_restore_secs/p90"), Some(&json!(1_400)));
        assert_eq!(m.pointer("/time_to_restore_secs/p50"), Some(&json!(300)));
    }

    #[test]
    fn percentiles_are_by_nearest_rank() {
        assert_eq!(percentile(&[], 50), None);
        assert_eq!(percentile(&[7], 90), Some(7));
        assert_eq!(percentile(&[1, 2, 3, 4], 50), Some(2));
        assert_eq!(percentile(&[1, 2, 3, 4], 90), Some(4));
    }
}

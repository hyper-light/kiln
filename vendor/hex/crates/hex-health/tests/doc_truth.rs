//! Doc truth (CLAUDE.md §6): every constant `docs/design/failure-isolation.md` names in its constants
//! table has that value in the code, and every one the code has is in the table.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros
    )
)]

use std::collections::BTreeMap;

use hex_health::{agent, antagonist, grade, penalty, retry};

const DOC: &str = include_str!("../../../docs/design/failure-isolation.md");

fn code() -> BTreeMap<&'static str, u128> {
    BTreeMap::from([
        ("grade::WINDOW_NS", u128::from(grade::WINDOW_NS)),
        (
            "grade::MIN_OBSERVATIONS",
            u128::from(grade::MIN_OBSERVATIONS),
        ),
        ("grade::MIN_PEERS", grade::MIN_PEERS as u128),
        (
            "grade::SLOW_FACTOR_MILLI",
            u128::from(grade::SLOW_FACTOR_MILLI),
        ),
        (
            "grade::STAY_FACTOR_MILLI",
            u128::from(grade::STAY_FACTOR_MILLI),
        ),
        ("grade::DRAIN_WINDOWS", u128::from(grade::DRAIN_WINDOWS)),
        (
            "penalty::BASE_EJECTION_NS",
            u128::from(penalty::BASE_EJECTION_NS),
        ),
        (
            "penalty::MAX_EJECTION_NS",
            u128::from(penalty::MAX_EJECTION_NS),
        ),
        (
            "penalty::EVICTION_MILLI_PER_S",
            u128::from(penalty::EVICTION_MILLI_PER_S),
        ),
        (
            "penalty::SECONDARY_EVICTION_MILLI_PER_S",
            u128::from(penalty::SECONDARY_EVICTION_MILLI_PER_S),
        ),
        (
            "penalty::UNHEALTHY_PER_MILLE",
            u128::from(penalty::UNHEALTHY_PER_MILLE),
        ),
        (
            "penalty::UNHEALTHY_MIN_NODES",
            u128::from(penalty::UNHEALTHY_MIN_NODES),
        ),
        (
            "penalty::LARGE_CELL_NODES",
            u128::from(penalty::LARGE_CELL_NODES),
        ),
        (
            "penalty::BACKOFF_BASE_NS",
            u128::from(penalty::BACKOFF_BASE_NS),
        ),
        (
            "penalty::BACKOFF_CAP_NS",
            u128::from(penalty::BACKOFF_CAP_NS),
        ),
        (
            "penalty::BACKOFF_RESET_NS",
            u128::from(penalty::BACKOFF_RESET_NS),
        ),
        ("retry::MAX_ATTEMPTS", u128::from(retry::MAX_ATTEMPTS)),
        ("retry::RETRY_PER_MILLE", u128::from(retry::RETRY_PER_MILLE)),
        ("retry::BUCKET_NS", u128::from(retry::BUCKET_NS)),
        (
            "antagonist::MIN_JOB_TASKS",
            antagonist::MIN_JOB_TASKS as u128,
        ),
        (
            "antagonist::MIN_TASK_SAMPLES",
            u128::from(antagonist::MIN_TASK_SAMPLES),
        ),
        (
            "antagonist::MIN_CPU_MILLI",
            u128::from(antagonist::MIN_CPU_MILLI),
        ),
        ("antagonist::SIGMAS", u128::from(antagonist::SIGMAS)),
        ("antagonist::VICTIM_FLAGS", antagonist::VICTIM_FLAGS as u128),
        (
            "antagonist::FLAG_WINDOW_NS",
            u128::from(antagonist::FLAG_WINDOW_NS),
        ),
        (
            "antagonist::CORRELATION_WINDOW_NS",
            u128::from(antagonist::CORRELATION_WINDOW_NS),
        ),
        (
            "antagonist::MIN_CORRELATION_MILLI",
            u128::try_from(antagonist::MIN_CORRELATION_MILLI).unwrap(),
        ),
        (
            "antagonist::IDENTIFY_GAP_NS",
            u128::from(antagonist::IDENTIFY_GAP_NS),
        ),
        (
            "antagonist::BEST_EFFORT_CAP_MILLI",
            u128::from(antagonist::BEST_EFFORT_CAP_MILLI),
        ),
        ("antagonist::CAP_MILLI", u128::from(antagonist::CAP_MILLI)),
        (
            "antagonist::THROTTLE_NS",
            u128::from(antagonist::THROTTLE_NS),
        ),
        ("agent::REPEAT_LIMIT", u128::from(agent::REPEAT_LIMIT)),
        (
            "agent::MAX_DISTINCT_CALLS",
            agent::MAX_DISTINCT_CALLS as u128,
        ),
    ])
}

/// The design's constants table, as name → value.
fn doc() -> BTreeMap<String, u128> {
    let (_, constants) = DOC
        .split_once("## Constants")
        .expect("the design has a constants section");
    constants
        .lines()
        .filter_map(|l| l.strip_prefix("| `"))
        .map(|l| {
            let (name, rest) = l.split_once("` | ").unwrap();
            let value = rest.split(" |").next().unwrap().trim();
            (name.to_string(), value.parse::<u128>().unwrap())
        })
        .collect()
}

/// S8 doc truth: the design's constants table and the code agree, name for name and value for value.
#[test]
fn the_designs_constants_are_the_codes() {
    let code: BTreeMap<String, u128> = code()
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    let doc = doc();
    assert_eq!(doc.len(), 33, "the table lost or gained a row");
    assert_eq!(doc, code);
}

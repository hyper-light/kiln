//! The waits census measures every wait unarmed (decision 9; the census's run 2 of 2026-11-08, whose waits ran out at
//! their bounds and so never recorded the answers past them: a wait that runs out is no sample). Its own process, as
//! [`hex_core::patience::measure_all`] holds for the whole of it.
#![allow(clippy::disallowed_macros)]

use std::time::Duration;

use hex_core::patience::{Patience, measure_all, measures_all};

/// Do: make a wait seeded from a 1 ms record, then measure all. Expect: armed at 2 ms before, unarmed after, while
/// its answers still keep its slowest; and the process says it measures all only after.
#[test]
fn measuring_all_disarms_every_wait() {
    let p = Patience::seeded("census.all", Duration::from_millis(1));
    assert_eq!(p.deadline(), Some(Duration::from_millis(2)));
    assert!(!measures_all());
    measure_all();
    assert!(measures_all());
    assert_eq!(p.deadline(), None);
    p.answered(Duration::from_millis(9));
    assert_eq!(p.slowest(), Some(Duration::from_millis(9)));
}

//! Amphora (PL.13; `docs/design/planner.md`, "Decision (lane pl13)"): the two run-time checks a
//! plan makes against the rows it actually sees, as pure cores the executor drives.
//!
//! - [`JoinSwitch`]: a nested-loop or index join that turns into a hash join once the outer rows
//!   seen pass the point where a hash join wins. The point is the learned costs'
//!   ([`LearnedWeights`]): the row at which the seeks' extra cost over probes so far would pay for
//!   building the hash table. Switching there is the ski-rental rule (Karlin, Manasse, Rudolph and
//!   Sleator, "Competitive snoopy caching", Algorithmica 3, 1988): whatever the outer side's true
//!   size, the join's cost stays under twice the cheaper of the two methods chosen in hindsight.
//!   No threshold is picked: the point moves with the weights and the build side's rows.
//! - [`MaterializePoint`]: at a point where an intermediate result is held whole (a hash table's
//!   build side, a sorter, a materialized subquery), the rows counted are set against the
//!   estimate; past the estimate's own error (the relative standard error of the statistics it
//!   came from, as PL.9 uses it), the rest of the statement is due to be planned again on the
//!   counted rows (Perron et al., "How I Learned to Stop Worrying and Love Re-optimization",
//!   ICDE 2019).
//!
//! Both hold a fixed few counters, and both hold the memory they would take to a budget the caller
//! passes (the statement's hash-table share, `vdbe::hash_table::table_share`): past it they refuse
//! with an [`AdaptiveError`] and the plan goes on as it was, which is always a correct answer.
//!
//! The executor's shell that drives them (a join emitted with both inner loops and the switch
//! between them; a materialization point that hands its rows to a second prepare) is PL.13's next
//! piece; until it lands only the tests reach these cores, so the dead-code allowance below stands
//! for PL.13 (T-PL.13.1) and goes when the shell lands.
#![cfg_attr(not(test), allow(dead_code))]

use std::fmt;

use super::learned_costs::LearnedWeights;

/// Why an adaptive check refused, or could not be made.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AdaptiveError {
    /// An input that is not a finite, non-negative number: a weight, an estimate or an error.
    NotANumber { what: &'static str, value: f64 },
    /// The hash table the switch would build needs more than its share of the statement's
    /// memory: the join stays a nested loop.
    BuildOverBudget { needed: u64, budget: u64 },
    /// The rows held at a materialization point outgrew their budget: they are not held for
    /// planning again, and the plan under way goes on.
    MaterializeOverBudget { needed: u64, budget: u64 },
    /// A row or byte count past `u64::MAX`.
    CountOverflow { what: &'static str },
}

impl fmt::Display for AdaptiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotANumber { what, value } => {
                write!(f, "adaptive join: {what} is {value}, not a finite non-negative number")
            }
            Self::BuildOverBudget { needed, budget } => write!(
                f,
                "adaptive join: a hash table of {needed} bytes is over its budget of {budget}; the join stays a nested loop"
            ),
            Self::MaterializeOverBudget { needed, budget } => write!(
                f,
                "re-optimization: {needed} bytes held is over the budget of {budget}; the plan goes on unchanged"
            ),
            Self::CountOverflow { what } => write!(f, "adaptive join: the count of {what} overflowed"),
        }
    }
}

impl std::error::Error for AdaptiveError {}

fn finite(what: &'static str, value: f64) -> Result<f64, AdaptiveError> {
    if value.is_finite() && value >= 0.0 {
        Ok(value)
    } else {
        Err(AdaptiveError::NotANumber { what, value })
    }
}

/// The outer rows after which a hash join over `build_rows` rows costs less than seeking once an
/// outer row, in the learned weights' unit (one row's visit). Building reads each build row and
/// adds it (`1 + hash_build` each); thereafter each outer row probes (`hash_probe`) where it would
/// have sought (`seek`). None when a probe costs no less than a seek: then a hash join never wins.
pub(crate) fn switch_point(
    weights: &LearnedWeights,
    build_rows: f64,
) -> Result<Option<f64>, AdaptiveError> {
    let seek = finite("the seek weight", weights.seek)?;
    let probe = finite("the probe weight", weights.hash_probe)?;
    let build = finite("the build weight", weights.hash_build)?;
    let rows = finite("the build side's rows", build_rows)?;
    let saved = seek - probe;
    if saved <= 0.0 {
        return Ok(None);
    }
    let point = rows * (1.0 + build) / saved;
    if point.is_finite() {
        Ok(Some(point))
    } else {
        Ok(None)
    }
}

/// What the join does for the outer row just counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JoinStep {
    /// Seek the inner side for this row, as planned.
    NestedLoop,
    /// Build the hash table over the inner side now, then probe it for this row and every later
    /// one.
    SwitchNow,
    /// Probe the hash table already built.
    Hash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    NestedLoop,
    Hash,
    /// The switch was due and refused (over budget): nested loops to the end.
    Refused,
}

/// One nested-loop join's switch, driven an outer row at a time.
#[derive(Debug, Clone)]
pub(crate) struct JoinSwitch {
    point: Option<f64>,
    build_bytes: u64,
    budget: u64,
    seen: u64,
    mode: Mode,
}

impl JoinSwitch {
    /// A switch for a join whose inner side has `build_rows` rows taking at most `build_bytes`
    /// (its table's pages, an upper bound known before it is read), with `budget` bytes for the
    /// hash table it would build.
    pub(crate) fn new(
        weights: &LearnedWeights,
        build_rows: f64,
        build_bytes: u64,
        budget: u64,
    ) -> Result<Self, AdaptiveError> {
        Ok(Self {
            point: switch_point(weights, build_rows)?,
            build_bytes,
            budget,
            seen: 0,
            mode: Mode::NestedLoop,
        })
    }

    /// Counts one outer row and says how to join it. The switch comes once, at the first row whose
    /// count reaches the switch point; if the hash table would be over budget it is refused once,
    /// with the error, and every later row stays a nested loop.
    pub(crate) fn outer_row(&mut self) -> Result<JoinStep, AdaptiveError> {
        self.seen = self
            .seen
            .checked_add(1)
            .ok_or(AdaptiveError::CountOverflow { what: "outer rows" })?;
        match self.mode {
            Mode::Hash => Ok(JoinStep::Hash),
            Mode::Refused => Ok(JoinStep::NestedLoop),
            Mode::NestedLoop => match self.point {
                Some(point) if self.seen as f64 >= point => {
                    if self.build_bytes > self.budget {
                        self.mode = Mode::Refused;
                        return Err(AdaptiveError::BuildOverBudget {
                            needed: self.build_bytes,
                            budget: self.budget,
                        });
                    }
                    self.mode = Mode::Hash;
                    Ok(JoinStep::SwitchNow)
                }
                _ => Ok(JoinStep::NestedLoop),
            },
        }
    }

    /// Whether the join switched to a hash join.
    pub(crate) fn switched(&self) -> bool {
        self.mode == Mode::Hash
    }

    /// The outer rows counted so far.
    pub(crate) fn rows_seen(&self) -> u64 {
        self.seen
    }
}

/// What a materialization point says once its rows are counted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Replan {
    /// The rows fall within the estimate's own error: the plan stands.
    Keep,
    /// The rows are past the estimate's error: plan the rest of the statement again on `rows`.
    Again { rows: u64 },
}

/// A point where the plan holds an intermediate result whole, with its estimate.
#[derive(Debug, Clone)]
pub(crate) struct MaterializePoint {
    estimate: f64,
    relative_error: f64,
    rows: u64,
    bytes: u64,
    budget: u64,
}

impl MaterializePoint {
    /// `estimate` rows, whose relative standard error is `relative_error` (the statistics' own:
    /// `1 / sqrt(n)` for a sample of `n`, decision "lane pl9"), held in at most `budget` bytes.
    pub(crate) fn new(
        estimate: f64,
        relative_error: f64,
        budget: u64,
    ) -> Result<Self, AdaptiveError> {
        Ok(Self {
            estimate: finite("the estimate", estimate)?,
            relative_error: finite("the estimate's error", relative_error)?,
            rows: 0,
            bytes: 0,
            budget,
        })
    }

    /// Counts one row of `bytes` held; past the budget it refuses.
    pub(crate) fn row(&mut self, bytes: u64) -> Result<(), AdaptiveError> {
        self.rows = self
            .rows
            .checked_add(1)
            .ok_or(AdaptiveError::CountOverflow {
                what: "materialized rows",
            })?;
        let held = self
            .bytes
            .checked_add(bytes)
            .ok_or(AdaptiveError::CountOverflow {
                what: "materialized bytes",
            })?;
        if held > self.budget {
            return Err(AdaptiveError::MaterializeOverBudget {
                needed: held,
                budget: self.budget,
            });
        }
        self.bytes = held;
        Ok(())
    }

    /// Once the result is whole: planned again when its rows are outside the estimate's band,
    /// `estimate / (1 + error)` to `estimate * (1 + error)`.
    pub(crate) fn verdict(&self) -> Replan {
        let band = 1.0 + self.relative_error;
        let rows = self.rows as f64;
        if rows > self.estimate * band || rows < self.estimate / band {
            Replan::Again { rows: self.rows }
        } else {
            Replan::Keep
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn weights(seek: f64, build: f64, probe: f64) -> LearnedWeights {
        LearnedWeights {
            row_in_pages: None,
            seek,
            hash_build: build,
            hash_probe: probe,
            sort: 1.0,
            page_miss: None,
        }
    }

    /// xorshift64: the tests' rows, the same every run.
    fn next(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    /// Do: a probe that costs as much as a seek, or more. Expect: no switch point: a hash join
    /// never wins (T-PL.13.2).
    #[test]
    fn t_pl_13_2_no_switch_where_a_probe_costs_a_seek() {
        assert_eq!(switch_point(&weights(5.0, 3.0, 5.0), 100.0).unwrap(), None);
        assert_eq!(switch_point(&weights(5.0, 3.0, 9.0), 100.0).unwrap(), None);
        assert!(matches!(
            switch_point(&weights(f64::NAN, 3.0, 1.0), 100.0),
            Err(AdaptiveError::NotANumber { .. })
        ));
    }

    /// Do: random weights and build sides; a planner that chose the nested loop for an estimate
    /// below the switch point; run the join on as many outer rows as estimated, then on more.
    /// Expect: no switch at the estimate; past the point, exactly one switch, at the first row
    /// whose count reaches it, counted, never timed (T-PL.13.2).
    #[test]
    fn t_pl_13_2_switch_fires_past_the_estimate_and_not_otherwise() {
        let mut state = 0x9E37_79B9_7F4A_7C15;
        for _ in 0..500 {
            let seek = 2.0 + (next(&mut state) % 400) as f64 / 10.0;
            let probe = (next(&mut state) % 100) as f64 / 100.0 * (seek - 0.5);
            let build = 0.5 + (next(&mut state) % 200) as f64 / 10.0;
            let build_rows = 1.0 + (next(&mut state) % 5_000) as f64;
            let w = weights(seek, build, probe);
            let point = switch_point(&w, build_rows).unwrap().unwrap();
            // The planner's choice: nested loop where its estimate is below the point.
            let estimate = (point - 1.0).floor().max(0.0) as u64;
            assert!(estimate as f64 * seek < build_rows * (1.0 + build) + estimate as f64 * probe);

            let mut at_estimate = JoinSwitch::new(&w, build_rows, 0, 0).unwrap();
            for _ in 0..estimate {
                assert_eq!(at_estimate.outer_row().unwrap(), JoinStep::NestedLoop);
            }
            assert!(!at_estimate.switched());

            let mut past = JoinSwitch::new(&w, build_rows, 0, 0).unwrap();
            let total = (point.ceil() as u64) * 3 + 1;
            let mut switches = 0;
            let mut first_hash = None;
            for row in 1..=total {
                match past.outer_row().unwrap() {
                    JoinStep::SwitchNow => {
                        switches += 1;
                        first_hash = Some(row);
                    }
                    JoinStep::Hash => assert!(first_hash.is_some()),
                    JoinStep::NestedLoop => assert!(first_hash.is_none()),
                }
            }
            assert_eq!(switches, 1);
            assert_eq!(first_hash, Some(point.ceil().max(1.0) as u64));
            assert_eq!(past.rows_seen(), total);
        }
    }

    /// Do: for every outer size, the cost of the switching join against the cheaper fixed method
    /// in hindsight. Expect: never twice it or more: the ski-rental bound the point is chosen for
    /// (T-PL.13.2).
    #[test]
    fn t_pl_13_2_switching_costs_under_twice_the_best_method() {
        let mut state = 0xD1B5_4A32_D192_ED03;
        for _ in 0..200 {
            let seek = 2.0 + (next(&mut state) % 400) as f64 / 10.0;
            let probe = (next(&mut state) % 100) as f64 / 100.0 * (seek - 0.5);
            let build = 0.5 + (next(&mut state) % 200) as f64 / 10.0;
            let build_rows = 1.0 + (next(&mut state) % 2_000) as f64;
            let w = weights(seek, build, probe);
            let built = build_rows * (1.0 + build);
            let point = switch_point(&w, build_rows).unwrap().unwrap();
            for outer in [
                1u64,
                2,
                10,
                point.floor() as u64,
                point.ceil() as u64,
                4 * point.ceil() as u64 + 7,
            ] {
                let mut switch = JoinSwitch::new(&w, build_rows, 0, 0).unwrap();
                let mut cost = 0.0;
                for _ in 0..outer {
                    cost += match switch.outer_row().unwrap() {
                        JoinStep::NestedLoop => seek,
                        JoinStep::SwitchNow => built + probe,
                        JoinStep::Hash => probe,
                    };
                }
                let best = (outer as f64 * seek).min(built + outer as f64 * probe);
                assert!(cost < 2.0 * best || outer == 0, "{cost} vs {best}");
            }
        }
    }

    /// Do: a switch due with the hash table over its budget. Expect: one typed refusal, then
    /// nested loops to the end, never a switch (T-PL.13.2: bounded memory).
    #[test]
    fn t_pl_13_2_a_switch_over_budget_is_refused() {
        let w = weights(10.0, 1.0, 1.0);
        let mut switch = JoinSwitch::new(&w, 9.0, 4_096, 4_095).unwrap();
        assert_eq!(switch.outer_row().unwrap(), JoinStep::NestedLoop);
        assert_eq!(
            switch.outer_row(),
            Err(AdaptiveError::BuildOverBudget {
                needed: 4_096,
                budget: 4_095
            })
        );
        for _ in 0..10 {
            assert_eq!(switch.outer_row().unwrap(), JoinStep::NestedLoop);
        }
        assert!(!switch.switched());
    }

    /// The join a test runs: an inner join of `outer` keys against `inner` (key, id) rows, by
    /// index nested loops (a binary search of the inner side sorted by key) until `switch` says
    /// otherwise, then by a hash table built over the inner side.
    fn run_join(
        outer: &[Option<u64>],
        inner: &[(Option<u64>, u64)],
        switch: Option<&mut JoinSwitch>,
    ) -> Vec<(u64, u64)> {
        let mut sorted: Vec<(u64, u64)> = inner
            .iter()
            .filter_map(|(k, id)| k.map(|k| (k, *id)))
            .collect();
        sorted.sort_unstable();
        let mut table: Option<HashMap<u64, Vec<u64>>> = None;
        let mut out = Vec::new();
        let mut switch = switch;
        for key in outer {
            let step = match switch.as_deref_mut() {
                Some(s) => s.outer_row().unwrap_or(JoinStep::NestedLoop),
                None => JoinStep::NestedLoop,
            };
            if step == JoinStep::SwitchNow {
                let mut built: HashMap<u64, Vec<u64>> = HashMap::new();
                for (k, id) in inner {
                    if let Some(k) = k {
                        built.entry(*k).or_default().push(*id);
                    }
                }
                table = Some(built);
            }
            let Some(key) = key else { continue };
            match (&table, step) {
                (Some(t), JoinStep::SwitchNow | JoinStep::Hash) => {
                    for id in t.get(key).into_iter().flatten() {
                        out.push((*key, *id));
                    }
                }
                _ => {
                    let start = sorted.partition_point(|(k, _)| k < key);
                    for (k, id) in &sorted[start..] {
                        if k != key {
                            break;
                        }
                        out.push((*key, *id));
                    }
                }
            }
        }
        out.sort_unstable();
        out
    }

    /// Do: random joins (NULL and repeated keys, outer sides far past and far under the switch
    /// point) run with the switch on and off. Expect: the same rows each time (T-PL.13.1: the
    /// answer is unchanged by the switch), and the switch fired in some runs and not in others.
    #[test]
    fn t_pl_13_1_answers_identical_with_switching_on_and_off() {
        let mut state = 0x2545_F491_4F6C_DD1D;
        let (mut fired, mut held) = (0, 0);
        for _ in 0..300 {
            let keys = 1 + next(&mut state) % 50;
            let key = |state: &mut u64| {
                let draw = next(state) % (keys + keys / 10 + 1);
                if draw < keys {
                    Some(draw)
                } else {
                    None
                }
            };
            let inner: Vec<(Option<u64>, u64)> = (0..next(&mut state) % 200)
                .map(|id| (key(&mut state), id))
                .collect();
            let outer: Vec<Option<u64>> = (0..next(&mut state) % 600)
                .map(|_| key(&mut state))
                .collect();
            let w = weights(
                2.0 + (next(&mut state) % 300) as f64 / 10.0,
                0.5 + (next(&mut state) % 100) as f64 / 10.0,
                1.0,
            );
            let budget = next(&mut state) % 2;
            let mut switch = JoinSwitch::new(&w, inner.len() as f64, 1, budget).unwrap();
            let on = run_join(&outer, &inner, Some(&mut switch));
            let off = run_join(&outer, &inner, None);
            assert_eq!(on, off);
            if switch.switched() {
                fired += 1;
            } else {
                held += 1;
            }
        }
        assert!(fired > 0 && held > 0, "fired {fired}, held {held}");
    }

    /// Do: count rows at a materialization point inside the estimate's band, then outside it on
    /// each side. Expect: kept inside, planned again outside with the rows counted (T-PL.13.3).
    #[test]
    fn t_pl_13_3_replan_only_past_the_estimates_own_error() {
        let error = 1.0 / (100.0f64).sqrt();
        for (rows, expect) in [
            (1_000, Replan::Keep),
            (1_100, Replan::Keep),
            (910, Replan::Keep),
            (1_101, Replan::Again { rows: 1_101 }),
            (900, Replan::Again { rows: 900 }),
            (0, Replan::Again { rows: 0 }),
        ] {
            let mut point = MaterializePoint::new(1_000.0, error, u64::MAX).unwrap();
            for _ in 0..rows {
                point.row(8).unwrap();
            }
            assert_eq!(point.verdict(), expect, "{rows} rows");
        }
    }

    /// Do: hold rows past a materialization point's budget; pass a meaningless estimate. Expect:
    /// typed refusals, the bytes held never past the budget (T-PL.13.3: bounded memory).
    #[test]
    fn t_pl_13_3_materialization_is_bounded() {
        let mut point = MaterializePoint::new(10.0, 0.1, 100).unwrap();
        for _ in 0..12 {
            point.row(8).unwrap();
        }
        assert_eq!(
            point.row(8),
            Err(AdaptiveError::MaterializeOverBudget {
                needed: 104,
                budget: 100
            })
        );
        assert!(matches!(
            MaterializePoint::new(f64::INFINITY, 0.1, 100),
            Err(AdaptiveError::NotANumber { .. })
        ));
        assert!(matches!(
            MaterializePoint::new(10.0, -0.1, 100),
            Err(AdaptiveError::NotANumber { .. })
        ));
    }
}

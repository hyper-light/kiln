//! Amphora (docs/design/planner.md, decision "custom plans, then a generic one"; PL.12): which plan
//! a prepared statement runs each time it runs. A statement whose estimates read a parameter's value
//! is planned again with each run's values (a custom plan) until the custom plans' measured costs
//! say the plan made at prepare (the generic one) does as well, as PostgreSQL's `plan_cache_mode`
//! does (`src/backend/utils/cache/plancache.c`, `choose_custom_plan`), without its five runs picked
//! in advance: the comparison is made from the first custom run on.
//!
//! [PlanChoice] is the rule, a pure core: it reads only the costs and times the statement gives it.

use crate::Value;
use amphora_engine_parser::ast;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// What a connection's prepared statements plan each run with (PostgreSQL's `plan_cache_mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlanCacheMode {
    /// Custom plans while their measured costs say they pay for their planning, then the generic
    /// one ([PlanChoice]).
    #[default]
    Auto,
    /// The plan made at prepare, whatever is bound.
    ForceGeneric,
    /// A plan made with each run's values, wherever an estimate reads one.
    ForceCustom,
}

impl PlanCacheMode {
    pub(crate) const fn to_u8(self) -> u8 {
        match self {
            PlanCacheMode::Auto => 0,
            PlanCacheMode::ForceGeneric => 1,
            PlanCacheMode::ForceCustom => 2,
        }
    }

    pub(crate) const fn from_u8(value: u8) -> Self {
        match value {
            1 => PlanCacheMode::ForceGeneric,
            2 => PlanCacheMode::ForceCustom,
            _ => PlanCacheMode::Auto,
        }
    }
}

/// The values the planner's estimates may read for a statement's parameters: none for the generic
/// plan, a run's bound values for a custom one. It also notes whether any estimate asked for one,
/// which is what makes a statement's plan depend on its values: a statement none asked of keeps its
/// generic plan, since a custom one would be the same plan.
#[derive(Debug, Default)]
pub(crate) struct PlanValues {
    /// By parameter index less one, as `ProgramState::parameters` keeps them.
    bound: Vec<Value>,
    asked: AtomicBool,
    /// Amphora (PL.9): each table's changes since ANALYZE, which the row counts read, and the
    /// tables the plan counted, so the statement can tell when they have moved past its estimates.
    row_changes: crate::sync::OnceLock<crate::sync::Arc<crate::row_changes::RowChanges>>,
    planned: crate::sync::Mutex<Vec<crate::row_changes::PlannedTable>>,
}

impl PlanValues {
    /// The generic plan's: no value bound.
    pub(crate) fn generic() -> Self {
        Self::default()
    }

    /// A custom plan's: a run's bound values.
    pub(crate) fn bound(bound: Vec<Value>) -> Self {
        Self {
            bound,
            ..Self::default()
        }
    }

    /// Where the row counts read each table's changes since ANALYZE (PL.9). Set once, at translate.
    pub(crate) fn attach_row_changes(
        &self,
        changes: crate::sync::Arc<crate::row_changes::RowChanges>,
    ) {
        let _already_attached = self.row_changes.set(changes);
    }

    /// The rows `table` holds now by its statistics, ANALYZE's `analyzed_rows` with the changes
    /// since, noted as a table the plan counted (PL.9). Without the counters attached (a plan made
    /// outside a connection), ANALYZE's count.
    pub(crate) fn live_rows(&self, table: &str, analyzed_rows: f64, error: f64) -> f64 {
        let Some(changes) = self.row_changes.get() else {
            return analyzed_rows;
        };
        let mut planned = self.planned.lock();
        if let Some(noted) = planned.iter().find(|noted| noted.table == table) {
            return noted.planned_rows();
        }
        let counter = changes.counter(table);
        let planned_changes = counter.load(crate::sync::atomic::Ordering::Acquire);
        let noted = crate::row_changes::PlannedTable {
            table: table.to_string(),
            counter,
            analyzed_rows,
            planned_changes,
            error,
        };
        let rows = noted.planned_rows();
        planned.push(noted);
        rows
    }

    /// The tables the plan counted (PL.9), taken once the statement is planned.
    pub(crate) fn take_planned(&self) -> Vec<crate::row_changes::PlannedTable> {
        std::mem::take(&mut *self.planned.lock())
    }

    /// The value bound to `variable`, `None` when it has none: the generic plan's, an unbound
    /// parameter's or a NULL's, which every estimate takes as unknown.
    pub(crate) fn value(&self, variable: &ast::Variable) -> Option<&Value> {
        let index = usize::try_from(variable.index.get()).ok()?.checked_sub(1)?;
        self.bound
            .get(index)
            .filter(|value| !matches!(value, Value::Null))
    }

    /// Notes that an estimate would take a parameter's value, had it one.
    pub(crate) fn note_asked(&self) {
        self.asked.store(true, Ordering::Relaxed);
    }

    /// Whether an estimate asked for a parameter's value while the statement was planned.
    pub(crate) fn asked(&self) -> bool {
        self.asked.load(Ordering::Relaxed)
    }
}

/// The plan a run takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlanKind {
    Generic,
    Custom,
}

/// One custom run, measured: its plan's estimated cost (the planner's units, a page read each),
/// the time its planning took (parse, plan and code), and the time from its planning's end to the
/// run's end.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CustomRun {
    pub cost: f64,
    pub planning: Duration,
    pub running: Duration,
}

/// The custom runs' measurements a statement keeps: their count and their sums, so its state is
/// the same few numbers however often it runs (CLAUDE.md §2).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct PlanChoice {
    runs: u64,
    cost: f64,
    planning: f64,
    running: f64,
}

impl PlanChoice {
    /// The plan the next run takes, given the generic plan's estimated cost.
    ///
    /// With no custom run measured, a custom plan: nothing says yet what the values change. After
    /// one, the generic plan once its cost is not above the custom plans' average plus what their
    /// planning added to them (`choose_custom_plan`'s rule). That planning is measured, not
    /// guessed: the custom runs' planning time over their running time is the share a custom plan
    /// pays to plan, so the custom plans' average cost, raised by that share, is what a custom run
    /// costs in the planner's own units. With `T` the running and `P` the planning summed over the
    /// custom runs: generic when `generic × T ≤ average × (T + P)`. A run measured as taking no
    /// time leaves nothing a custom plan could save, and takes the generic plan.
    ///
    /// The choice changes only as custom runs are recorded, so once the generic plan is taken it
    /// stays until the statement is planned again (a schema change, new statistics), which starts
    /// a new choice.
    pub(crate) fn next(&self, generic_cost: f64) -> PlanKind {
        if self.runs == 0 {
            return PlanKind::Custom;
        }
        let average = self.cost / self.runs as f64;
        if generic_cost * self.running <= average * (self.running + self.planning) {
            PlanKind::Generic
        } else {
            PlanKind::Custom
        }
    }

    /// Records a custom run's measurements.
    pub(crate) fn record(&mut self, run: CustomRun) {
        // A count of runs: saturating at u64::MAX runs is the stated meaning (it can't be reached).
        self.runs = self.runs.saturating_add(1);
        self.cost += run.cost.max(0.0);
        self.planning += run.planning.as_secs_f64();
        self.running += run.running.as_secs_f64();
    }

    /// The custom runs measured so far (its tests read it).
    #[cfg(test)]
    pub(crate) fn custom_runs(&self) -> u64 {
        self.runs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(cost: f64, planning_us: u64, running_us: u64) -> CustomRun {
        CustomRun {
            cost,
            planning: Duration::from_micros(planning_us),
            running: Duration::from_micros(running_us),
        }
    }

    // T-PL.12.1. Do: ask a statement no custom run has measured. Expect: a custom plan.
    #[test]
    fn t_pl_12_1_a_statement_first_runs_a_custom_plan() {
        assert_eq!(PlanChoice::default().next(100.0), PlanKind::Custom);
    }

    // T-PL.12.1. Do: record custom plans that cost what the generic plan does. Expect: the
    // generic plan next, and from then on, since nothing a custom plan does is cheaper.
    #[test]
    fn t_pl_12_1_custom_plans_no_cheaper_than_the_generic_one_give_way_to_it() {
        let mut choice = PlanChoice::default();
        choice.record(run(100.0, 50, 1_000));
        assert_eq!(choice.next(100.0), PlanKind::Generic);
        assert_eq!(choice.next(100.0), PlanKind::Generic);
    }

    // T-PL.12.1. Do: record custom plans a tenth of the generic plan's cost, their planning a
    // twentieth of their running. Expect: custom plans go on.
    #[test]
    fn t_pl_12_1_custom_plans_far_cheaper_than_the_generic_one_go_on() {
        let mut choice = PlanChoice::default();
        choice.record(run(10.0, 50, 1_000));
        choice.record(run(10.0, 50, 1_000));
        assert_eq!(choice.next(100.0), PlanKind::Custom);
    }

    // T-PL.12.1. Do: record custom plans a tenth of the generic plan's cost whose planning takes
    // twenty times their running (a point read). Expect: the generic plan, since a custom plan's
    // planning costs more than its plan saves.
    #[test]
    fn t_pl_12_1_planning_that_costs_more_than_it_saves_takes_the_generic_plan() {
        let mut choice = PlanChoice::default();
        choice.record(run(10.0, 20_000, 1_000));
        assert_eq!(choice.next(100.0), PlanKind::Generic);
    }

    // T-PL.12.1. Do: the threshold itself: custom plans averaging 50 whose planning is a share of
    // 1 of their running, against a generic plan of 100 and of 100.5. Expect: generic at 100 (not
    // above 50 x 2), custom at 100.5.
    #[test]
    fn t_pl_12_1_the_generic_plan_is_taken_at_the_custom_average_raised_by_planning() {
        let mut choice = PlanChoice::default();
        choice.record(run(40.0, 1_000, 1_000));
        choice.record(run(60.0, 1_000, 1_000));
        assert_eq!(choice.next(100.0), PlanKind::Generic);
        assert_eq!(choice.next(100.5), PlanKind::Custom);
    }

    // T-PL.12.1. Do: one cheap custom run, then a costly one (a common value). Expect: custom after
    // the first, generic once the average rises to the generic plan's.
    #[test]
    fn t_pl_12_1_the_choice_follows_the_custom_plans_average() {
        let mut choice = PlanChoice::default();
        choice.record(run(10.0, 10, 1_000));
        assert_eq!(choice.next(100.0), PlanKind::Custom);
        choice.record(run(190.0, 10, 1_000));
        assert_eq!(choice.next(100.0), PlanKind::Generic);
        assert_eq!(choice.custom_runs(), 2);
    }

    // T-PL.12.1. Do: a custom run measured as taking no time. Expect: the generic plan.
    #[test]
    fn t_pl_12_1_a_run_that_took_no_time_takes_the_generic_plan() {
        let mut choice = PlanChoice::default();
        choice.record(run(1.0, 0, 0));
        assert_eq!(choice.next(100.0), PlanKind::Generic);
    }

    // T-PL.12.2. Do: read a parameter's value from a generic plan's values and from a run's, bound
    // to 7, NULL and nothing. Expect: none from the generic plan; 7; NULL and an unbound one none.
    #[test]
    fn t_pl_12_2_an_estimate_reads_only_a_bound_value_that_is_not_null() {
        let at = |i: u32| ast::Variable::indexed(std::num::NonZeroU32::new(i).unwrap());
        assert!(PlanValues::generic().value(&at(1)).is_none());
        let values = PlanValues::bound(vec![Value::from_i64(7), Value::Null]);
        assert_eq!(values.value(&at(1)), Some(&Value::from_i64(7)));
        assert!(values.value(&at(2)).is_none());
        assert!(values.value(&at(3)).is_none());
        assert!(!values.asked());
        values.note_asked();
        assert!(values.asked());
    }
}

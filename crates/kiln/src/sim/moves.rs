//! The moves model (CP-T6.1): the sync model's machines, server and nemesis, with an agent
//! session working in the repo on one machine at a time and moving between them as
//! `harness::session` moves one (`sync::runner`'s `look_now_at`, `left_at` and `arrived`). At
//! each move it checks that the session leaves no write of its own behind (each is in the log
//! through the index it leaves at) and goes on with every one in its folder; and, once the
//! nemesis has stopped, that every move finishes. The engine is the sync model's
//! ([`super::sync`], [`Mode::Moves`]).

#[cfg(test)]
mod tests {
    use kiln_sim::seeds;

    use super::super::sync::{Counts, Mode, Plant, run};

    /// CP-T6.1, AC-T6.1.1: the moves model over the seed budget: no move leaves a write of its
    /// session's behind or goes on without one, every move finishes, and the repo converges as
    /// in the sync model; and moves happened, some waiting for their changes.
    #[test]
    fn a_moving_session_never_leaves_its_work_behind() -> Result<(), String> {
        let mut all = Counts::default();
        let report = seeds::run_budget("moves", 200, |seed| {
            let keep = if seeds::replay().is_some() { 400 } else { 0 };
            let (counts, digest) = run(seed, keep, Plant::default(), Mode::Moves)?;
            all.add(&counts);
            Ok(digest)
        });
        eprintln!("{}\n  counted: {all:?}", report.line());
        report.into_result()?;
        if seeds::replay().is_none() {
            let none = [("moves", all.moves), ("moves that waited for their changes", all.moves_waited), ("runner crashes", all.crashes), ("frames lost", all.net.dropped)];
            if let Some((what, _)) = none.iter().find(|(_, n)| *n == 0) {
                return Err(format!("no {what} over the budget: the model didn't test what it says"));
            }
        }
        Ok(())
    }

    /// CP-T6.1, AC-T6.1.4: a seed runs the same every time.
    #[test]
    fn a_moves_seed_replays_exactly() -> Result<(), String> {
        for seed in [2, 99, 5150] {
            seeds::twice(seed, |s| run(s, 0, Plant::default(), Mode::Moves).map(|(_, d)| d))?;
        }
        Ok(())
    }
}

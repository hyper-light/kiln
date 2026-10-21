//! What every seam's conformance suite is made of: cases, each one clause of the seam's contract
//! (its id is the clause's in docs/seams.md), run against a fresh implementation apiece.

use std::future::Future;

use futures::future::BoxFuture;

/// What a case found wrong, in words.
pub type Failure = String;

/// One clause of a seam's contract, checked against what fixture `F` gives.
pub struct Case<F: ?Sized> {
    /// The clause's id in docs/seams.md (`CLOCK-3`).
    pub id: &'static str,
    /// What the clause says, as the doc says it.
    pub says: &'static str,
    pub check: for<'a> fn(&'a F) -> BoxFuture<'a, Result<(), Failure>>,
}

/// What a suite's run found.
#[derive(Debug, Default)]
pub struct Report {
    pub seam: &'static str,
    pub passed: Vec<&'static str>,
    pub failed: Vec<(&'static str, Failure)>,
}

impl Report {
    /// Every case passed (and there was at least one).
    pub fn is_ok(&self) -> bool {
        self.failed.is_empty() && !self.passed.is_empty()
    }

    /// How many passed, or every failure by its clause.
    pub fn into_result(self) -> Result<usize, String> {
        if self.is_ok() {
            return Ok(self.passed.len());
        }
        let lines: Vec<String> = self.failed.iter().map(|(id, why)| format!("{id}: {why}")).collect();
        Err(format!("{}: {} of {} cases failed\n{}", self.seam, self.failed.len(), self.passed.len().saturating_add(self.failed.len()), lines.join("\n")))
    }
}

/// Runs each case against a fixture of its own (`make` builds one per case), in order, with no
/// deadline: where a case waits on the implementation, it waits until the implementation answers
/// (Spawn's fixtures bound their own waits; nothing else does). An implementation that may never
/// answer is run with [`run_within`].
pub async fn run<F, M, Fut>(seam: &'static str, cases: &[Case<F>], make: M) -> Report
where
    M: FnMut() -> Fut,
    Fut: Future<Output = F>,
{
    run_within(seam, cases, make, futures::future::pending::<()>).await
}

/// [`run`], each case failed once its deadline (`deadline()`, made afresh for each case: a sleep
/// on the implementation's own runtime) completes first. A case whose implementation never answers
/// fails, named, and the suite goes on.
pub async fn run_within<F, M, Fut, D, DFut>(seam: &'static str, cases: &[Case<F>], mut make: M, mut deadline: D) -> Report
where
    M: FnMut() -> Fut,
    Fut: Future<Output = F>,
    D: FnMut() -> DFut,
    DFut: Future<Output = ()>,
{
    let mut report = Report { seam, ..Report::default() };
    for case in cases {
        let fixture = make().await;
        let late = deadline();
        futures::pin_mut!(late);
        let outcome = match futures::future::select((case.check)(&fixture), late).await {
            futures::future::Either::Left((outcome, _)) => outcome,
            futures::future::Either::Right(((), _)) => Err("it didn't end by its deadline: the implementation never answered".to_owned()),
        };
        match outcome {
            Ok(()) => report.passed.push(case.id),
            Err(why) => report.failed.push((case.id, why)),
        }
    }
    report
}

/// The ids of `cases`, in order.
pub fn ids<F: ?Sized>(cases: &[Case<F>]) -> Vec<&'static str> {
    cases.iter().map(|c| c.id).collect()
}

/// `Ok` when `cond` holds, else the failure `why` says.
pub fn ensure(cond: bool, why: impl FnOnce() -> String) -> Result<(), Failure> {
    if cond { Ok(()) } else { Err(why()) }
}

/// `Ok` when `got == want`, else both, named.
pub fn same<T: PartialEq + std::fmt::Debug>(what: &str, got: T, want: T) -> Result<(), Failure> {
    if got == want { Ok(()) } else { Err(format!("{what}: got {got:?}, want {want:?}")) }
}

/// `v`'s refusal or error, said as a failure.
pub fn fine<T, E: std::fmt::Display>(what: &str, v: Result<T, E>) -> Result<T, Failure> {
    v.map_err(|e| format!("{what}: {e}"))
}

/// The refusal `v` should have been, or a failure saying it wasn't.
pub fn refused<T: std::fmt::Debug, E>(what: &str, v: Result<T, E>) -> Result<E, Failure> {
    match v {
        Ok(t) => Err(format!("{what}: accepted ({t:?}), should have been refused")),
        Err(e) => Ok(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Nothing;

    fn hangs(_: &Nothing) -> BoxFuture<'_, Result<(), Failure>> {
        Box::pin(futures::future::pending())
    }

    fn holds(_: &Nothing) -> BoxFuture<'_, Result<(), Failure>> {
        Box::pin(async { Ok(()) })
    }

    /// CP-M2, AC-M2.2: a case whose implementation never answers fails at its deadline, named, and
    /// the suite goes on to the next.
    #[test]
    fn a_case_that_never_ends_fails_at_its_deadline_and_the_suite_goes_on() {
        let cases = [Case { id: "X-1", says: "never ends", check: hangs }, Case { id: "X-2", says: "holds", check: holds }];
        // A deadline already passed: each case has its first poll, no more.
        let report = futures::executor::block_on(run_within("X", &cases, || async { Nothing }, || futures::future::ready(())));
        assert_eq!(report.passed, ["X-2"]);
        assert_eq!(report.failed.iter().map(|(id, _)| *id).collect::<Vec<_>>(), ["X-1"]);
        assert!(report.failed.iter().all(|(_, why)| why.contains("never answered")));
    }
}

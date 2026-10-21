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

/// Runs each case against a fixture of its own (`make` builds one per case), in order. A case has
/// no deadline of its own: where it waits on the implementation, it waits as long as its fixture
/// lets it. The simulators' fixtures end every wait they can't finish (`SimSpawnFixture::finish`
/// at its poll budget); a real implementation's bounds its own (kiln's runtime's: 10 s).
pub async fn run<F, M, Fut>(seam: &'static str, cases: &[Case<F>], mut make: M) -> Report
where
    M: FnMut() -> Fut,
    Fut: Future<Output = F>,
{
    let mut report = Report { seam, ..Report::default() };
    for case in cases {
        let fixture = make().await;
        match (case.check)(&fixture).await {
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

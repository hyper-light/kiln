# Legacy migration test validation

The production implementation is signed private checkpoint
`1fc2e3824f30f543ee44185b9effba1d6ffe8491`, tree
`549ed43ca7466eb341b0ffb6f98317b260a07a63`, parent5b7814c.
It has not been pushed or deployed. Policy, roles, exact migration red, scope and
remaining limits are recorded in LEGACY-PROMOTION.md and ROOT.md.

Unchanged source capture `check-1794400099483227000` failed test compilation with
five errors: the new test fixture used `?` for admission::Error, which intentionally
does not implement StdError, and the new preflush test retained a non-Send mutex
guard across an awaited Records read despite an explicit drop. No executed test
pass or successful release build is inferred from that failure.

This followup changes only code inside existing cfg(test) modules: convert those
test errors to anyhow using their debug representation and end the controller
assertions in a lexical scope before the await. All assertions and production
code are unchanged. The new compile and behavioral checks are pending at this
private signed build-input checkpoint. Full gate remains skipped at the user's
request; no broad platform, crash or performance result is claimed.

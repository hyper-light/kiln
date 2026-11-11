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
code are unchanged by that followup. Its unchanged test compile
`check-1794400329491891000` then exposed three borrowed temporary database paths;
the fixture futures now own those paths explicitly with async move.

Unchanged compile `check-1794400573279479000` passed. Eleven targeted migration,
database restart, cancellation, receipt-order and identity tests passed against
the pinned executable SHA256
`a5b4d08fc8ebb4dbd9396627b98148b27257784e940b59f6ca1aacd3f8f5e527`.
The coordinator regression group passed49. Further affected groups were running
when this record was written. No executed result is inferred from the earlier
failed private-selector invocation; it discovered the actual native_handover_tests
module before any test ran.

Library clippy found two visibility mismatches: a public field and preparation
method exposed the crate-private LegacyProof. Those two interfaces are narrowed
to crate visibility; runtime logic is unchanged. A fresh source capture, lint and
release/native gates remain required for this final snapshot. Full gate remains
skipped at the user's request; no broad platform, crash or performance result is
claimed. Earlier private checkpoints remain retained rather than rewritten.

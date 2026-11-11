# R18 scoped status and transfer-page correction

Implementer: `/root/kiln_status_trace`. Policy reloaded from Shards
`origin/main` `7d7e0855a5073ed5163b4fe7ed3355c9ed7c2506`; complete AGENTS,
development rules/evidence, CLAUDE, Kiln AGENTS/CI §0 and ROOT read.
Rules SHA-256: `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Kiln source base is `f555736bd887a4196c8841564cb9b4d6c1ede95f` plus the shared
uncommitted prevention batch. Scope: these two R18 failures only; no builds,
services, actors, tests, child agents or source changes outside the owned spans.

The coordinator's `r18-focused-tests.log` records 531 executed tests: 524 passed,
seven failed. The failures below are actual compiled regression failures; no live
production duplicate or communication-loss defect is inferred from them.

## News original versus latest desired snapshot

`a_run_s_news_and_a_child_s_are_one_message` discarded the command returned on the
first of `settle`'s three planning calls. The run was due immediately, while the
child only became due after the settle interval. The new custody contract preserves
the original run-only body while it awaits publication. Both lines remained in the
producer's desired state; a committed publication receipt creates a fresh combined
successor. Overwriting the unaccepted original to satisfy the old assertion would
violate the no-loss contract.

No Told production code changed. The existing fixture now uses the same real
in-memory `NewsLedger` as the other migrated news fixtures. It asserts that the
original remains exact before custody, the receipt creates a new ID with both
headings and exact run output, publication supersedes only the never-dispatched
original, and both original commands remain immutable in ledger history. Its
ended-parent assertion remains. This is an assertion migration to custody semantics,
not a claim that a live coalescing defect was reproduced.

## Exact transfer-history response size

`transfer_body_cursor_pages_repeated_generations_without_changing_entry_fields`
failed with `TooLarge { revision: 1, budget: 119 }`, although its expected one-entry
continuation encoded to exactly 119 bytes. `body_on` translated its response budget
through generic `Page` metadata which reserved the larger of `next:null` and
`next:1`. That maximum is three bytes larger than the actual partial response.

The transfer branch now measures its actual `BodyReply` wrapper and actual cursor,
with one-row lookahead distinguishing continuation from final `null`. Entry bytes
and commas are summed with checked arithmetic. Entries move once into the result;
there are no growing-page clones or repeated whole-page encodes per candidate.
The final complete response still passes the bounded encoder. The original
publication, token generation, transfer fields and transfer ordering are unchanged.
This is an algorithm description, not an allocation or performance measurement.

The same existing fixture now checks exact full, partial and final-response budgets,
one-byte-too-small refusal, and opaque cursor digit transitions 9→10 and 99→100.
It continues to verify repeated epoch moves keep the same generation, exact history
fields, and old terminal retry identity.

## Checks and frozen sources

Actual local commands, all exit 0:

- `rustfmt --edition 2024 --emit stdout` on `harness/mail.rs`,
  `harness/mail/handoffs.rs` and `harness/told.rs`, output discarded; parser only.
- `git diff --check --` those three named paths.

Rust compilation and regression execution after these edits are **unrun by this
lane**. Coordinator combined validation and guard-removal mutations remain owed.
No cross-platform, live no-loss, performance, deployment or readiness claim.

Frozen SHA-256:

- `harness/mail.rs`: `ab82862ec77898e4975cbd376ef83c9eab15aab48bd4406ce3b1f7f3d3738a05`
- `harness/mail/handoffs.rs`: `a1bba5870928594f772c057cf0f725a4b5c25c735a13921e36766de1fba23ac2`
- `harness/told.rs`: `53ef079396932fac3058a1e9f53e24b239eb649fabe71de0b8cf6f84e0e32189`

## Read-only deployment receipts from the preceding task

- `deployment-20261111T071434Z.json`: fleet/server, exact current CI job and narrow
  Shards status; SHA-256 `844da32ce39f889551a3df032806d7359f4a5cdd55f4c8bc7cbf856ad8c0ccc3`.
- `deployment-20261111T071606Z-mac-followup.json`: b24 upgrade timeout after verified
  download/installer completion; SHA-256 `d324a88dc45a1160ff379d2c0a33f3b39f91611fefe3bcb5f2fb0013f6aaf9d4`.
- `deployment-20261111T071927Z-b24-runner.json`: scoped supervisor/instance logs and
  process executable metadata; SHA-256 `fb91bead12df0b4ac4e30d7e909bb9e4941cd44086eda9c91eff1bc317a72285`.

At 07:14 UTC the server and eight connected runners had reached f555; b24 retained
the old versioned runner and three runners were offline. Shards was Running/Working
with seven queued messages, on its updated Mac owner. Linux 8f had reached f555
after the earlier read-only-filesystem failure; the earlier hold was stale.
At 07:18–19 b24 still had a live Claude child under its old runner. Zero CI jobs does
not prove no live agent process. The installed path's symlink status and the exact
reason it was not selected/reported remain unverified. No install, restart, remount,
kill or configuration mutation was performed. These are point-in-time receipts,
not deployment of the uncommitted prevention work.

## Test Records wrappers and owned commits

The next coordinator assignment covers only `Flaky` in `inbox/faults.rs` and
`Faults` in `inbox/sim.rs`. Rechecked current Shards revision/rules hash unchanged
and reread current ROOT. `Records::commit_owned` intentionally defaults to typed
Unavailable; a wrapper must explicitly preserve the implementation's allocation
owner guarantee. Both wrappers now execute the same injected-write failure counter
as their existing `commit`, then forward the exact `tx, owner` to
`inner.commit_owned`. Scans, failures, existing fixture methods, the Records trait
default and production implementations are unchanged. There is no fallback to a
bare commit. Existing owned commits through these fixtures can now exercise the
inner simulator rather than the intentional unsupported default.

Both named files pass `rustfmt --edition 2024 --emit stdout` parser checks and
scoped `git diff --check` (exit 0). Runtime/regression execution is **unrun by this
lane** and remains the coordinator's combined check. Frozen source hashes:

- `inbox/faults.rs`: `d85e0e17b3d38a98ba498dadd35d121642184fe3b227986567e88131c66e7bfb`
- `inbox/sim.rs`: `bf0c0da28b45028d597b85307f409378a262b1aed90ae808fd571513bae9eb8d`

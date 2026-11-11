# Reproduce the three legacy import reds

Origin: `c6751a28ef95060b3f0205ea56503da4b06d0ed6`.
Patch: `legacy-three-regressions-red-only.patch`, SHA256
`89626fc95edf81ddc7c2846bfdd4e8fc923965978cb0513c08506de02281cf46`.

The patch adds only three tests inside the existing Host/Session test modules.
Inbox and both production prefixes are byte-identical to c675. Private reversal
of the exact reviewed production patch reproduced every target-file SHA in
`check-1794403787664581000/before.json`. Applying this compact patch to fresh
c675 file copies independently recreated those hashes. See
`legacy-red-replay-c6751a2-1794403787664581000/proof.json`.

Root/coordinator only: prepare a NEW disposable archive outside the shared
checkout (no reset, signing change or worktree registration):

```sh
source_repo=/private/tmp/kiln-queue-prevention-20261111
proof_dir=/private/tmp/kiln-busy-forensics-20261111
repro=/private/tmp/kiln-legacy-c675-three-reds-root
mkdir "$repro"
git -C "$source_repo" archive c6751a28ef95060b3f0205ea56503da4b06d0ed6 | tar -x -C "$repro"
git -C "$repro" apply --check "$proof_dir/legacy-three-regressions-red-only.patch"
git -C "$repro" apply "$proof_dir/legacy-three-regressions-red-only.patch"
```

Run the following payload ONLY through the coordinator's normal admitted Reach
job, with its measured CPU/memory/disk envelope and owner-local cache target.
Do not run a bare unadmitted heavy build on the Mac. This preparation did not run
cargo or tests.

```sh
cd /private/tmp/kiln-legacy-c675-three-reds-root
: "${CARGO_TARGET_DIR:?set the coordinator-owned persistent cache target}"
cargo test -p kiln --lib harness::host::tests::legacy_reconciliation_retries_before_epoch_activation_after_original_only_budget -- --exact --nocapture
cargo test -p kiln --lib harness::session::tests::early_legacy_effect_waits_for_its_original_without_poisoning_readiness -- --exact --nocapture
cargo test -p kiln --lib harness::session::tests::partial_legacy_rebinding_retains_completion_until_epochs_match_and_retires_its_flight -- --exact --nocapture
```

Expected: each individual command exits101 from its specific invariant assertion,
not from a compile or infrastructure error. Root already observed the following
unchanged-source results:

- `check-1794403035078639000`: successful owner reconciliation left promotion unavailable after a temporary copy refusal (.22s).
- `check-1794403383195866000`: early stage missing its legacy row poisoned later successful reconciliation (.15s).
- `check-1794403787664581000`: a callback during partial rebinding poisoned readiness (.23s).

These are bounded component callback/storage regressions. They do not replace
the actual native provider/certificate upgrade proof, nor claim model processing.
The production fix is deliberately absent from this replay patch. All original
private candidates and logs remain preserved; the shared source was unchanged.

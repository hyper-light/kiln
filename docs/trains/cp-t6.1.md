# Train notes: CP-T6.1, kiln-sim and the real bugs it found

CP-T6.1's part of the critical path's merge train (its master plan's §6): what it delivers, what
deploying it does and how to watch it, its risks and its rollback, and what's still open.

**Where it stands (2026-10-22):**
- **Not merged:** branch `cp/t6-sim` at 6d83211, local to adacachy's repo, not pushed. It holds main
  through 6063b42 (merged at d04d8fb); the train merges main again and gates the merge.
- **Not deployed:** production runs kiln:6063b42.
- **Gated** on fedora, on 6d83211 exactly (2026-10-22, 18:27 to 18:37Z, then the UI's steps again
  once npm was on the job's PATH), every step passing: clippy; the workspace's tests; the macOS and
  Windows cross-checks; npm ci, tsc, eslint and the UI build. Log:
  `fedora:~/.cache/kiln-cp/t6-gate/6d83211/gate.log`.

## What it delivers

- **kiln-sim** (`crates/kiln-sim`, docs/sim.md): kiln's deterministic simulator. One seed decides a
  run, so a failure replays exactly from its seed.
- **kiln's models under it:** the sync engine, session moves and the goal driver, in `cargo test`
  (each model's budget of seeds, on every change request) and nightly (`.kiln/sim.yaml`: 20,000
  sync and moves seeds, 2,000 goal-driver seeds, at 03:23 UTC).
- **Nine fixes in sync** (docs/sim.md's table), each in its own commit with the seeds that found
  it and a test of the contract it broke. Most are durability: what a power loss could undo that
  sync's saved state said was done. Plus a tenth commit closing a hole the sweep found in the
  sixth fix.
- **Refactors the models run:** the runner's look and send are `sync::runner::look_now_at` and
  `to_send`; the goal driver's work each tick is `Driver::round`; sync's files go through
  `sync::fs` (`Std` in production).

## Deploying it

**Config:** nothing to set. **Migrations:** none.

**What changes on disk:**
- **More fsyncs.** Before a copy saves its state it fsyncs each folder whose names changed: its
  file's folder and those above it to the copy's root (each once a save), and the contents store's
  folders that what its steps carry went into, and the store's own. A file's bytes were already
  fsynced before its rename. Expect slower saves on slow disks; no fsync per blob was added.
- **The copy's saved state** may hold two new fields: `racy` on a file just changed when seen, and
  `behind` (paths a version was kept back from). Both are skipped when empty.

**What changes in behaviour:**
- A moving session goes on on a machine only once that copy's **saved** state holds the log through
  where it left (`Standing::saved`), not only its folder: where saves fail (a full disk, EIO), the
  move waits.
- A file changed within 3 s of a look is read again at the next look (git's racily clean entries):
  a little more hashing right after edits.
- A step goes to the server only once the copy's saved state holds it.

## Watching it

- **Sync:** copies' status errors (`the copy's state: ...`, `looking at the folder: ...`) on the
  sync pages; no new metrics.
- **Moves:** how long a session takes to go on after it moves (`harness::session`'s `files_in`).
- **CI:** the rust job's run time grows by the models' budgets (about 20 s at 8 threads on fedora).
- **Nightly:** the `sim` pipeline's first runs pass. A failure names its seed and how to replay it.

## Metric names awaiting Kiln Metrics

None: CP-T6.1 adds no metrics.

## Risks

- **Save latency** from the folder fsyncs, on slow or network disks.
- **Moves that wait** where a copy's saves keep failing: by design, but newly visible.
- **The nightly's cost:** one k8s job a night, 4 CPUs and 16 GiB, up to 4 hours.
- **What the models leave out** (docs/sim.md): the harness host's record and sends aren't run under
  kiln-sim yet; members' logs and the server's contents store are outside the disk's nemesis.
- **Found, not fixed (for T10):** the server's upload intake fsyncs a blob's bytes but not the
  folder it renames it into before answering 201: the same power-loss hole server-side, which no
  model covers.

## Rollback

- No migrations, no config: the release before runs on the same store and `kiln.toml`.
- The saved state's new fields are `serde(default)` and skipped when empty, and `State`, `Entry`
  and `Saved` don't deny unknown fields, so the release before opens a state this one saved (it
  forgets `behind`: a version kept back then waits for a change, as before).
- The fixes' behaviour goes with it: the bugs come back as they were.

## Ada's open decisions

- None asked. Landing goes through the coordinator's train on Ada's word.

## Open with others

- **Sends T3:** its send simulation moves onto kiln-sim once both are on main (agreed: T6 ports it,
  T3 reviews).
- **CP-T3.1:** the harness host's journal on `Records` takes its crash test on
  `kiln_sim::records::Crashing`.
- **T10:** the server-side upload finding above; and `sync/server.rs`'s step intake is
  `take_steps` here, which T10's branch also changes (pass its `contents.has` as `has`).

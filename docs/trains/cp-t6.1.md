# Train notes: CP-T6.1, kiln-sim and the real bugs it found

CP-T6.1's part of the critical path's merge train (its master plan's §6): what it delivers, what
deploying it does and how to watch it, its risks and its rollback, and what's still open.

**Where it stands (2026-10-24, as it lands):**
- **Merged:** with this file, as one fast-forward of main, on Ada's word (2026-10-24, "You can do
  all of these things!", to her agent's asks for fedora, the gate and the landing). Branch
  `cp/t6-sim` merged main as it moved, the last time just before the push. The landing commit's
  message has the change request's evidence and the gate's.
- **Not deployed:** deploys go through Ada.
- **Gated** before the push: clippy for Linux, macOS and Windows; the workspace's tests; npm ci,
  tsc, eslint and the UI build. The landing commit's message says where and on what. 6d83211, the
  branch before main's later merges, passed the same gate on 2026-10-22.

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
- **Found for T10, and narrower since:** the server's upload intake fsynced a blob's bytes but not
  the folder it renamed the blob into before answering 201. T10's contents store, merged here,
  keeps chunks through `blobs::chunks::keep`, which syncs that folder after the rename. The
  folders it makes for a chunk (`create_dir_all`) still aren't synced into the folders above
  them. POSIX needs that; ext4's one journal doesn't. No model covers the server's store.

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
- **T10:** the upload finding above. Its contents store is merged here: `sync/server.rs`'s step
  intake (`take_steps`) asks it whether the server has a step's contents (`contents.has`).

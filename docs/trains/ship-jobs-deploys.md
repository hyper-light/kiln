# Train notes: Ship's jobs pane and Deploys tab

Ada asked for three things on 2026-10-30. First, Ship's rows not to scroll sideways: that landed on
its own as f4082e70ab. Second, retry, pause, resume and cancel for any of a commit's jobs. Third, a
Deploys tab redrawn from scratch. She approved the designs the same day ("approved. do not forget,
you must also implement the backend logic"). They're stills on adacachy in
~/.cache/ship-design/shots: v2-jobs-* for the jobs pane, v5-deploys-* and v6-deploys-* for Deploys.
Plan 01a18e5191f0738abebeef6c8df0e88a carried the work in ten steps, and docs/ci.md holds its
requirements, W26-R21 to W26-R27.

**Where it stands (2026-10-31, when written):**
- **Merged:** with this file, onto local main, on Ada's word, as a merge with main first. The
  branch is ship-actions, with local main 656fb3e427 merged in (3ecc8312e8). It also holds
  origin/main up to d50f1fd322 (10-30). Local main and origin/main had drifted apart, by 55 commits
  and 126. Production's 96b3afd1a8 and origin's 26 commits since 10-30 aren't in this branch.
- **Deployed:** not yet. That waits for Ada's word.
- **Checked at the tree that lands:**
  - the UI's typecheck, lint, tests and build;
  - Ship's e2e, in Chromium and WebKit, at desktop and phone widths;
  - ci/gate.sh: clippy for Linux, Windows and macOS, and the workspace's tests;
  - proto.rs's enums and capability bits, diffed against production's commit.

  The landing merge's message has their results. vendor/ is the same as local main's.
- **Windows:** the freeze test passed on a real Windows runner (jupiter), freezing a real step
  tree: no CPU while frozen, and its count finished after.

## What it delivers

- **Retry and cancel any set of jobs (W26-R21).** `POST /api/runs/:id/retry` and `/cancel` take
  `jobs`, on both engines.
  - Retrying runs exactly those jobs again, passed ones too, along with the jobs that wait on them.
  - Cancelling stops exactly those. The jobs that wait on them are skipped, saying why.
  - Without `jobs`, both behave as before.
  - Another provider's run (GitHub Actions, GitLab CI, Bitbucket Pipelines) is that provider's
    to act on. Its jobs are refused, saying so ("GitHub Actions runs it, not kiln"), nothing
    changes in kiln's tables, and Ship's pane dims every verb for them.
- **Pause and Resume (W26-R22).** A running job paused is frozen where it runs. It keeps its
  machine, and its time limit stops. Resumed, it goes on from where it was. A waiting job paused
  isn't dispatched. How a job is frozen:
  - Linux: its cgroup is frozen, `kiln-jobs-<pid>/<job>` under the runner's own.
  - Elsewhere on Unix: its steps' process groups are stopped.
  - Windows: every process of its steps' job objects is suspended.
  - Its container: paused with `docker pause`.

  Only cancelling thaws a job without being asked. The routes are `POST /api/runs/:id/pause` and
  `/resume`, under a new permission, `runs.pause`, which the developer role has.
- **Ship acts on a commit's jobs (W26-R21, R22).** `POST /api/ship/jobs` takes `verb`, `jobs` or
  `scope` (failed, running, waiting, all). It acts across the commit's runs and answers job by job:
  done, or refused saying why. Each verb asks for its own permission, and each act is audited.
- **The jobs pane (W26-R23).** An opened commit's Output and Jobs become one pane:
  - the strip says what's picked and what each verb would do: Retry, Pause or Resume, and
    Cancel, which asks again;
  - the jobs are listed by stage;
  - the picked job's log sits beside the list on a desktop and under its line on a phone.
- **Roll back to any commit (W26-R25).** An app can go back to any commit one of its git sources
  has, not only to one a deployment ran. `rollback_targets` lists commits as well as deployments.
- **Auto-sync from the UI (W26-R27).** `cd.apps.set_auto_sync`, or `PUT /api/cd/apps/:app/auto-sync`,
  sets an app's auto-sync. A person's word wins until the deploy file changes its `sync.auto`.
- **Logs past pods (W26-R26).** A Compose project's services and a host's services are read
  through the same logs API and viewer as a pod.
- **Ship's Deploys (W26-R24).**
  - API: `GET /api/ship/deploys` and `GET /api/ship/deploys/:app`. `POST /api/ship/deploys` does
    each strip verb as its CD operation, with the caller's rights.
  - UI: Ship's tabs become Commits and Deploys. The Runs tab goes, and /runs lands on Ship. An
    opened app is drawn as the stills draw it, and an e2e checks the stills' content word for word.
- **Wire.** `ServerMsg::Freeze` is variant 64, right after `ReachRefused`, and `CAP_FREEZE` is bit
  53. docs/wire.md keeps bits 42 to 52 for W27's and the unification's lines: they use those bits on
  their branches.
- **Store.** Migration 20261030-2100_w26_paused adds `jobs.paused_at` and `jobs.paused_ms`. Being
  paused is `paused_at` being set; the job's status stays what it was.

## Deploying it

1. Production first, through KilnCD. Its first start adds the two columns. This train brings no
   new setting and no new metric.
2. The runners as they take the release. Each on Unix or Windows then announces `CAP_FREEZE`.
   Until a runner does, pausing a job running on it is refused saying so, and nothing is sent to it.
   A waiting job can be paused wherever it would go.
3. A token with scopes of its own needs `runs.pause` to pause or resume.

## Watching it

- Ship shows a paused job as paused ("Resume lets it go on from where it was"). When a verb can't
  apply, the strip says why.
- On a Linux runner, a paused job's cgroup reads `cgroup.freeze` 1, and its container shows as
  Paused in `docker ps`.
- The audit log has one row for every request that changes something, with who asked, refused or
  not (W19-R05). That covers Ship's jobs and deploys verbs, pause and resume. A person's auto-sync
  also gets `cd.auto_sync`.

## Risks

- A paused job holds its machine, slot and memory alike, until someone resumes or cancels it.
  Nothing resumes it by itself: Ada decided a pause keeps its machine (10-30).
- A runner process that ends ends its jobs, paused or not. A job's cgroup belongs to the process
  that runs it, and on Windows its job objects end with the runner.
- Headroom's pacer doesn't touch a job's cgroup. It paces agent sessions and transient units outside
  the runner, and never starts what someone else stopped.

## Rollback

- **The server.** An older release ignores the migration it doesn't know and never names its
  columns. But it sees a paused job as its status says, so resume or cancel every paused job first.
  Otherwise:
  - a paused waiting job is dispatched;
  - a frozen running job looks like it's running, with its time limit counting, until it's
    cancelled. Cancelling is what thaws it.

  Auto-sync set by a person is kept in the store's settings (`auto-sync/app/<id>`). An older release
  ignores it and follows each app's deploy file. An app held at a commit no deployment ran has a pin
  with `to` 0. An older release finds no artifacts recorded for it and resolves that commit's own,
  as this one does.
- **The runners.** A runner going back to an older build ends its jobs, frozen ones included, as
  any restart does. An older runner never says `CAP_FREEZE`, so it's never sent `Freeze`.

## Ada's open decisions

- On a phone, Deploys' strip doesn't repeat under the picked line. The plan said it would, but that
  came from a suggestion of mine she never answered, and every v5 and v6 phone still shows no
  repeat. Deploys follows the stills.
- Stop, Abort and Delete ask again in the strip, as Cancel does. Say if any shouldn't.

## Open with others

- **W27 (signing).** Once W27 lands, the jobs pane can link a leg's signed results
  (`GET /api/ci/legs/:id/signed`), and Deploys can show promotions and replants as their custody
  records.
- **convo-marks.** It carries the same one-line `ci_verdict` fix to ui/src/api/events.ts, in the
  same place.
- **ci-gh-public (Kiln CI -> GitHub, 01a1935d).** Its `ci::providers::act_on_set` routes a
  provider's job sets to that provider, as far as each allows (`plan_set`). Whichever of us lands
  second makes the swap:
  - its one line goes at the top of `retry_set`, `cancel_set` and `pause_set`, in place of this
    train's refusal (no provider pauses, so pause stays refused);
  - `job_verbs` takes what the provider allows.

  Ada asked them for Ship's commit rows (10-31): Hand off and Retry as icon buttons, Run and
  Cancel offered clearly. They build it on this train, stills to Ada first, with the trigger
  endpoint behind Run. A job's own actions stay in the jobs pane's strip.
- **Main's Windows test build.** It fails on crates/kiln/src/ci/review_cli.rs:899, an unused
  `use super::*`. That's main's, not this train's. The gate's Windows clippy checks libraries
  only, so it doesn't see it.

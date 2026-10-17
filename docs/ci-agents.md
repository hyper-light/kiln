# Agents on pipelines and repositories

> **Superseded by [ci.md](ci.md), W20** (2026-10-17), which adapts this draft. Kept for history.

> **Draft spec.** None of this is built yet.

Attach an agent session to a repository, a pipeline, a job or an environment, and it looks after
it. When a run fails it diagnoses why, then reruns, fixes, quarantines or asks, within the
permissions you gave it. Between failures it maintains the pipelines. Everything it does goes
through the same API, review and land queue as everyone else.

## Attaching

**In the UI:** "Attach agent" on a repository, pipeline, run or environment. Pick a live session
or start a new one (Claude Code, Codex, any ACP agent), then set what wakes it, what it may do,
its limits and who reviews its changes. Attachments made in the UI live in kiln; "Save to repo"
turns one into a diff to `.kiln/agents.yaml`.

**In git:** trunk's `.kiln/agents.yaml`, read from trunk like all policy:

```yaml
isolation: [worktree, microvm]       # where agents may work (also used by experiments)
agents:
  ci-doctor:
    harness: claude                  # claude · codex · acp:<name>
    model: opus
    effort: high
    attach: { pipelines: [ci], branches: [main] }   # or repo: true · jobs · environments
    wake: [failure, flaky, stuck, ejected]          # what starts work (below)
    can: [read, rerun, diagnose, fix, comment, quarantine]
    limits: { per-day: { sessions: 10, runner-minutes: 600, tokens: 5M }, reruns-per-failure: 2 }
    instructions: .kiln/agents/ci-doctor.md         # its playbook, versioned with the code
    reviewers: [ada]                                # who reviews its diffs
  pipeline-gardener:
    harness: codex
    attach: { repo: true }
    wake: [{ schedule: "0 6 * * 1" }]               # weekly maintenance
    can: [read, diagnose, edit-pipelines, comment]
```

## What wakes an agent

| Event | When |
| --- | --- |
| `failure` | a run of an attached pipeline fails (trunk, diffs, schedules; filtered by `branches`) |
| `ejected` | the land queue ejects a candidate (a land-check failure or a conflict) |
| `flaky` | a test or job flips between pass and fail on the same input |
| `stuck` | a job has waited past a limit (no runner, quota, a concurrency group), or a run stalls |
| `slow` | a pipeline's critical path gets slower than its recent median by a set margin |
| `schedule` | cron, for maintenance |
| `mention` | someone writes `@ci-doctor` on a run, a diff or a thread |

kiln groups failures by **signature**: the failing step plus its normalized error, with paths,
numbers and hashes stripped. A signature already being handled doesn't start a second session;
the new occurrence is added to the open incident.

## What it may do

| Capability | Lets the agent |
| --- | --- |
| `read` | runs, jobs, logs, artifacts, test results, metrics, diffs, history |
| `rerun` | rerun failed jobs (with `debug: on-failure`), up to `reruns-per-failure` |
| `cancel` | cancel runs it started, or stuck runs |
| `diagnose` | open debug terminals into kept workspaces, run commands in its own workspace, bisect |
| `fix` | submit diffs (forward fixes, reverts); they go through review and the land queue |
| `edit-pipelines` | submit diffs to `.kiln/*.yaml` (reviewed like any change) |
| `quarantine` | submit a diff to `.kiln/quarantine.yaml` marking a test flaky (still run, never blocking) |
| `comment` | comment on runs, diffs and threads; ask people questions (the choices dialog) |
| `experiment` | start experiments ([experiments.md](experiments.md)) |

There's no capability to land or to approve. An agent's diffs land like anyone's: they need the
approvals `.kiln/review.yaml` asks for, and `agents-can-approve` decides whether other agents' accepts count.
A job can opt out of agent actions in its pipeline: `agents: { rerun: false }` (deploys, for example).

## An incident, start to finish

1. **Failure bundle.** kiln collects what a person would look for first:
   - the failed jobs and steps, and log excerpts around the errors;
   - parsed test failures, and each failing test's flakiness history;
   - what changed since the last green run (commits, diffs, files);
   - what changed in the environment (runner, image digest, toolchain versions, cache keys);
   - earlier incidents with the same signature, and how they were resolved.
2. **Wake.** A new session starts with the bundle and the playbook, in an approved isolated
   workspace with the failing commit checked out. If the attachment names a live session, the
   bundle is queued into it instead.
3. **Diagnose.** Read logs, rerun with a kept workspace and open a terminal into it, reproduce
   locally, bisect the range since the last green run (`kiln bisect` runs the failing job on
   each midpoint). The agent classifies the failure: flaky, infrastructure (runner lost, OOM,
   network), regression (with the culprit diff), pipeline error, or dependency break.
4. **Act**, within its permissions: rerun (flaky or infrastructure), revert or forward-fix the
   culprit as a diff, quarantine a flaky test, fix the pipeline, or ask. A question can offer
   "revert D123" against "forward-fix", with Experiment available.
5. **Report.** The run gets an incident card: the classification, the evidence (linked log lines),
   what was done, the diffs it opened, and the outcome (green on rerun, fix landed, waiting on
   someone). The card links to the session, so anyone can read along, reply or take over.

## Maintenance

Agents woken by `schedule`, `slow` or `stuck` work on the pipeline rather than a failure:
- shorten the critical path, and add caches where keys miss;
- split slow jobs into matrix shards;
- refresh toolchain images;
- triage the quarantine list (fixed tests come out);
- tidy runner labels and pools.
Every change is a reviewed diff with before/after numbers from an experiment or a comparison run.

## Safety

- Each attachment gets its own identity (`agent:ci-doctor@hyper-light/kiln`) and a token limited to
  its capabilities and scope. Everything it does is audited under that identity, on behalf of the
  person who attached it.
- Limits stop runaway loops: sessions, tokens and runner minutes per day, and reruns per failure.
  When a limit is reached the agent stops and its incident card says so.
- One switch pauses every agent attached to a repository.

## In the UI

- **Repository and pipeline pages**: an Agents section. Each attached agent shows what it watches,
  its state (watching · working on an incident · paused), today's usage against its limits, and
  its latest actions.
- **Run page**: the incident card, live while the agent works (its state, findings as they come,
  actions taken), with "Open session", "Take over" and "Stop".
- **Mission Control**: attached agents grouped by repository; their questions arrive in "Needs you".

## API (draft)

| Method | Path | |
| --- | --- | --- |
| GET/POST | `/api/repos/:repo/attachments` | list or create (`{ session?, harness?, attach, wake, can, limits, instructions?, reviewers }`) |
| PATCH/DELETE | `/api/attachments/:id` | change, pause, remove |
| GET | `/api/incidents?repo=&state=` | incidents with signature, classification, actions, outcome |
| GET | `/api/incidents/:id` | the failure bundle and the incident's timeline |
| POST | `/api/runs/:id/bisect` | `{ job, good, bad }`: run a job across a commit range |

Agents use kiln's MCP server: tools for runs, logs (search, failure excerpts), artifacts, test
results, run comparison, rerun, bisect, debug terminals, diffs, comments, questions and
experiments, each limited to the attachment's capabilities.

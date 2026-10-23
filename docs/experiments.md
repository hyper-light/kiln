# Experiments

> **Superseded by [ci.md](ci.md), W20-R11** (2026-10-17), which adapts this draft. Kept for history.

> **Built** as [ci.md](ci.md) W20-R11 describes, which also says what's left.

When an agent asks you to choose between options, you don't have to guess. **Run an experiment**:
kiln has the agent build every option in its own isolated workspace, measures every variant
against your criteria, and brings back results, or continues with the winner if you allow it.
Options that can stack are tested in combination, so you learn which combine well and which fight.

## Starting one

- **From a question.** The choices dialog has **Experiment…** next to the answers. Pick the
  options in play, a strategy, the isolation, criteria and thresholds, repetitions, a budget, and
  who decides.
- **From an agent.** An agent can propose an experiment itself (an MCP tool). You see the same
  dialog, with Experiment preselected and the agent's suggested criteria filled in.
- **From the API or CLI.** `POST /api/experiments`, `kiln experiment start`.

## Variants

| Strategy | Variants for n options | Use it when |
| --- | --- | --- |
| Each alone | n | the options are alternatives |
| All combinations | 2ⁿ − 1 | a few options that may stack (3 → 7) |
| Pairs | n + n(n−1)/2 | more options; main effects and two-way interactions |
| Greedy stacking | rounds: n singles, then best-so-far + each remaining | many options; stops when nothing adds enough |
| Custom | the sets you pick | you know which ones make sense |

A **baseline** (none of the options) is included by default. An experiment has at most 32 variants.

### How variants are built

Agents implement each option **once**; combinations are composed from those patches:

1. kiln forks the asking agent's session once per option. Each fork has the conversation up to
   the question and the instruction "implement only this option". It works in its own workspace
   and commits the change on top of the baseline.
2. A combination applies its options' commits to the baseline in a fixed order. Clean applies
   need no agent. On a conflict, a fork resolves it in that combination's own workspace, and the
   resolution is kept as the combination's "glue" commit.
3. Greedy stacking composes best-so-far + each remaining option after every round of measurement.

Agent work grows with n, not with the number of combinations.

## Isolation: where agents work

| Method | What each variant gets |
| --- | --- |
| `worktree` | a git worktree on the agent's machine (cheapest; shares the host) |
| `slates` | a hyper-light Slates volume: in-memory, snapshotted (once Slates ships) |
| `container` | a container from the repository's development image |
| `microvm` | a microVM (strongest isolation) |

Trunk's `.kiln/agents.yaml` lists the approved methods (`isolation: [worktree, microvm]`). The
dialog offers those available on the agent's machine and explains the others. Variant sessions
inherit the asking agent's permission mode. They can commit only in their own workspace, and
can't push trunk or land.

Isolation is where code is *written*. Measurement runs on runners, with the isolation the
criteria's pipelines ask for (benchmarks on `exclusive` bench hardware).

## Criteria and measurement

| Kind | Pass/fail | Metrics |
| --- | --- | --- |
| `pipeline` | the pipeline's run succeeds on the variant | whatever its jobs emit |
| `command` | the command exits 0 in the variant's workspace | whatever it emits |
| `metric` | the metric meets its threshold | a named metric, `better: lower \| higher` |

A criterion is **required** (every repetition must pass, e.g. the test suite) or an
**objective** to optimize, with a threshold: absolute (`p99_ms ≤ 10`) or relative to the baseline
(`≥ 5 %` better).

**Metrics.** Any job or command writes JSON lines to `$KILN_METRICS`:
`{"name": "p99_ms", "value": 12.3, "unit": "ms"}`. Converters cover common tools:
`kiln metrics criterion target/criterion`, `kiln metrics hyperfine out.json`, hyperscale results.
The same metrics feed ordinary CI too: trends per branch and regression flags on diffs.

**Repetitions.** Each variant is measured r times (default 3). Runs are interleaved across
variants in a shuffled order each round, so drift on a machine spreads evenly.

**Statistics.** For each metric kiln takes the median and a 95 % bootstrap confidence interval of
the ratio to the baseline. A variant meets a relative threshold only if the whole interval clears
it; otherwise it is reported as "no clear difference". For combinations kiln also reports each
option's main effect and every pair's **interaction**: the combined effect minus the sum of the
parts. A pair that helps more together than apart is synergistic; one that cancels out is antagonistic.

## Deciding

- **Bring results back to me** (default): the question returns with a results table. Each variant
  shows required checks, every metric's change against the baseline with its interval, and kiln's
  recommendation. The recommendation is the best primary objective among variants that pass every
  required criterion; ties go to fewer options and a smaller change.
- **Pick automatically**: the agent continues with the clear winner. With no clear winner, it
  falls back to asking.

**Continuing with a variant** moves the asking agent's workspace to that variant's commits. Its
session carries on from there, and the work becomes a stack ready for review. Every variant is
kept as `refs/kiln/experiments/E12/<variant>` and can be submitted as diffs later. Workspaces are
removed after the decision (kept 7 days if asked).

While the experiment runs, the asking agent's question is answered with "running experiment E12".
The agent waits, and the result arrives as its next message.

## Budget

`max-parallel` (default 4), a time budget, token and runner-minute accounting per variant, and
the runner pools to use. When the budget runs out, remaining work is cancelled and partial
results are reported for what was measured.

## In the UI

- **Experiment page**: the question, strategy and criteria. A variant table (rows = variants;
  columns = required checks and metrics) with each variant agent's state and a beam per run.
  Deltas against the baseline show their intervals. For combinations, an option × option
  interaction grid. "Continue with this" on every row.
- **Mission Control**: an experiment is one group of agents with one status line
  ("E12 · 5 of 8 measured").

## API (draft)

| Method | Path | |
| --- | --- | --- |
| POST | `/api/experiments` | start: `{ agent, question, options, strategy, variants?, baseline, isolation, criteria, repetitions, max_parallel, budget_minutes?, decide }` |
| GET | `/api/experiments/:id` | spec, variants (sessions, workspaces, commits), runs, metrics, statistics, recommendation |
| POST | `/api/experiments/:id/decide` | `{ variant }`: continue with it |
| POST | `/api/experiments/:id/cancel` | |

`/api/events` adds `experiment` and `experiment_variant` messages.

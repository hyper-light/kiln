---
name: kiln-ci-performance
description: Make kiln CI faster and cheaper from its insights - the critical path, slow jobs and steps, cache hit rates, queue time by reason, shards balanced by timing, and reusing results whose inputs didn't change. Use when runs are slow, legs wait long, caches miss, a test suite dominates, or runner time costs too much.
---

# CI performance, from insights to changes

Measure first, change one thing, measure again. Every change is a workflow edit, so it goes
through the kiln-ci-author loop: validate, preview, propose, watch.

## Where the time goes

```js
// code_run: a repository's workflows by time, then the slowest workflow's breakdown.
const repo = await kiln.ci.insights.repos({ repo: input.repo, days: 14 });
const wfs = repo.workflows.sort((a, b) => (b.p50 ?? 0) * b.count - (a.p50 ?? 0) * a.count);
const top = wfs[0];
const w = await kiln.ci.insights.workflows({ repo: input.repo, workflow: top.workflow, days: 14 });
return {
  workflow: top.workflow, runs: w.count, p50: w.p50, p90: w.p90,
  critical_path: w.critical_path,                       // the latest run's longest chain of needs
  slowest_steps: w.steps.slice(0, 8),                    // by median: job, step, p50, p90, total
  waiting: w.jobs.map(j => ({ job: j.job, queue_p50: j.queue_p50, waits: j.waits })),
  cache: w.cache,                                        // by key prefix: hit rate, time saved
};
```

Read it in this order:

1. **The critical path** bounds a run's time: shortening a job off it changes nothing. Move work
   off it (start independent jobs without `needs`, split a long job), or make its jobs faster.
2. **Waiting** before running is capacity or policy, not the code: `waits` by reason
   (`capacity` and `no-runner` want runners or a pool; `concurrency` a group's limit;
   `exclusive` a dedicated runner; `approval` a reviewer). The kiln-ci-runners skill explains a
   leg's wait.
3. **Slow steps**: what one step costs across every run (`total`).

## Caches

A low hit rate is usually the key: one that changes every run (a timestamp, a whole-tree
`hashFiles`) never hits; one that never changes restores stale state. Key on the lockfile, add
`restore-keys` for a near match, and keep `paths` to what's expensive to rebuild:

```yaml
cache: [{ key: "cargo-${{ runner.os }}-${{ hashFiles('Cargo.lock') }}", restore-keys: ["cargo-${{ runner.os }}-"], paths: [~/.cargo/registry, target] }]
```

`local: true` keeps a cache on the runner's disk too, for big directories on long-lived runners.
`kiln.ci.caches.list({ repo })` shows each cache's key, size, scope and last use.

## Tests: shards and the slow ones

```js
const t = await kiln.ci.tests.repo({ repo: input.repo, sort: "slowest", days: 14 });
return t.tests.slice(0, 15).map(x => ({ test: x.name, file: x.file, p50_ms: x.p50_ms, p90_ms: x.p90_ms }));
```

A suite that dominates a job splits into legs by recorded duration, each leg running its part:

```yaml
strategy:
  shard: { files: "tests/**/*_test.py", target: 6m, balance: timing }
steps:
  - run: pytest $(cat "$KILN_SHARD_ITEMS")
    test-results: { path: report.xml }    # the history that balances the next run
```

Without results reported (`test-results:`), there is no timing to balance by.

## Reuse instead of rerunning

A job whose inputs didn't change can take its earlier result:
`reuse: { paths: ['crates/**', 'Cargo.lock'], env: [RUSTFLAGS] }`. Name every path and variable
the job reads; a missing one makes reuse return a result that's wrong for the change.

## Check the change worked

Propose, let the diff's runs and a few trunk runs finish, then compare the same insights for the
days after with the days before, and say what changed in numbers (p50 and p90, critical path
seconds, hit rate, runner minutes). For benchmark-sized differences, use the run's metrics
(`kiln.ci.metrics.run`), which compare against a baseline with an interval.

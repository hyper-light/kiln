---
name: kiln-ci-diagnose
description: Find out why a kiln CI run failed, classify it (flaky, infrastructure, regression, pipeline error, dependency break) and fix it, using a failed leg kept to look into. Use when a run or job failed, a leg waits forever, or a step behaves differently in CI than locally.
---

# Diagnosing CI runs

## The failure bundle, in one call

```js
// code_run: the failed legs of a repository's recent runs, with each failing step's last lines
const runs = await kiln.ci.runs.list({ repo: input.repo, conclusion: "failure", limit: input.limit ?? 10 });
const out = [];
for (const r of runs.runs) {
  const run = await kiln.ci.runs.get({ run: r.id });
  for (const l of run.legs.filter(l => ["failure", "timed_out", "startup_failure"].includes(l.conclusion))) {
    const leg = await kiln.ci.legs.get({ leg: l.id });
    // The failed step's log from just before its first error line.
    const ex = await kiln.ci.legs.failure_excerpt({ leg: l.id, lines: 30 });
    out.push({
      run: r.number, workflow: r.workflow, sha: r.sha, leg: l.key, id: l.id, error: l.error,
      step: ex.step_name, excerpt: ex.lines.map(x => x.text).join("\n"),
      annotations: leg.annotations.filter(a => a.level === "error").map(a => a.message),
    });
  }
}
return out;
```

Group what comes back by signature: the failing step and its normalized error (strip paths,
numbers, hashes). The same signature across unrelated commits is flaky or infrastructure; one
that starts at a commit and stays is a regression.

## Classify

- **Flaky**: the same commit passes on a rerun (`kiln.ci.runs.rerun({ run, mode: "failed" })`), or
  the step's failures come and go across runs of unchanged code. Rerun once; don't loop.
- **Infrastructure**: a runner, network or registry problem: `ci.legs.routing` explains a leg
  that waits (no runner meets its requirements, a capability missing); a step dies with DNS
  errors, `429`/rate limits, disk full, or the runner disconnecting.
- **Regression**: it started at a commit. Compare the run with the last green run of the
  workflow on the branch, and name the change that did it:

  ```js
  const c = await kiln.ci.runs.compare({ run: input.run }); // other: "last-green"
  return { differences: c.differences, files: c.files, slower: c.legs.flatMap(l => l.steps.filter(s => s.a !== null && s.b !== null && s.b > 2 * s.a + 5).map(s => ({ leg: l.key, ...s }))) };
  ```

  `differences` names the actions, variables, environment, images, machines and caches that
  changed; `files` the commits' changed files.
- **When did it start, how often?** Search the logs across runs for the error's text:
  `kiln.ci.logs.search({ q: "error[E0308]", repo: input.repo, days: 30 })`, newest first. Hits
  on unchanged code across days say flaky; hits only after one commit say regression.
- **Pipeline error**: the workflow itself: a run that fails before any job (`startup_failure`,
  its `error` lists diagnostics with codes and positions), a bad expression, a missing secret
  (`KCI4040`), an action that doesn't resolve. Validate before proposing a fix:
  `kiln.ci.workflows.validate({ path, content })`, then `kiln.ci.workflows.plan_preview(...)`.
- **Dependency break**: a pinned tool or action that no longer works where it runs (a toolchain
  version gone, an action that predates the runner image). The log names the version.

## Look inside a failed leg

Rerun the failed jobs, keeping what fails: its workspace, containers and environment stay, its
processes stopped, for the minutes you ask (at most a day).

```js
await kiln.ci.runs.rerun({ run: input.run, mode: "failed", keep_failed: 30 });
```

Once the leg failed again (`ci.runs.get`: its `kept_until` is set), run commands in it. Each runs
in bash in the leg's working directory with its environment, or in its job container; output is
masked like the leg's log.

```js
const r = await kiln.ci.legs.exec({ leg: input.leg, command: "git status --short; ls -la; env | grep -v -i token | sort | head -50" });
return r; // { output, exit_code, timed_out, truncated }
```

Try the fix there, then run the failing step again in place, with the environment the leg ended
with, and read its new section of the log:

```js
await kiln.ci.legs.exec({ leg: input.leg, command: "npm ci && npm test -- --runInBand 2>&1 | tail -40" });
await kiln.ci.legs.rerun_step({ leg: input.leg, step: input.step }); // step from 1, as the run page numbers them
const leg = await kiln.ci.legs.get({ leg: input.leg });
return leg.steps.slice(-1); // "Rerun: <name>", with its conclusion
```

Release the leg when done (`kiln.ci.legs.release({ leg })`), or keep it longer
(`kiln.ci.legs.keep({ leg, minutes })`).

## Fix, don't just rerun

A fix is a change to the code or the workflow, proposed for review: validate and plan-preview a
workflow change first, and say in the proposal what failed, why, and how the fix was checked
(the command that passed in the kept leg, the step rerun's conclusion). Reruns are for flaky and
infrastructure failures only, once.

## Limits

`ci.legs.exec` needs `runs.debug` on the repository, and on the host executor also
`runners.terminal` on the runner: the leg's shell is a shell on that machine. A refusal names the
capability; ask a person rather than looking for another way in.

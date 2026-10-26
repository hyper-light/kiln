---
name: kiln-ci-author
description: Write, port and change kiln CI workflows (GitHub Actions' dialect in .github/workflows, kiln's in .kiln/*.yaml, overlays), with the validate, preview, propose and watch loop. Use when adding or editing a workflow, a matrix, shards, caches, reuse, a reusable workflow, or a job's runner, or when a workflow fails before any job runs.
---

# Writing kiln CI workflows

Every change to a workflow goes the same way, and never skips a step:

1. **Validate** the text: `kiln.ci.workflows.validate({ path, content })`. Fix every error; read
   the warnings.
2. **Preview** what it would run: `kiln.ci.workflows.plan_preview({ repo, path, content, event })`.
   Check the legs, their matrix values and where each would route.
3. **Propose** it as a diff: `kiln.ci.workflows.edit({ repo, path, content, title })`. It is
   refused with its errors when it doesn't validate, and answers with its diagnostics and the plan
   a push to the trunk would make once it lands.
4. **Watch** the diff's run (`kiln.ci.runs.list({ repo, branch })`, then `kiln.ci.runs.get`) and
   fix what fails before asking for review.

```js
// code_run: check, preview and propose a workflow change in one program.
const { repo, path, content } = input;
const v = await kiln.ci.workflows.validate({ path, content });
const errors = v.diagnostics.filter(d => d.severity === "error");
if (errors.length) return { errors };
const preview = await kiln.ci.workflows.plan_preview({ repo, path, content, event: "push" });
// `runs` false says why it wouldn't run for that event (`why`); `plan.later` waits on outputs.
if (!preview.runs) return { warnings: v.diagnostics, why: preview.why };
const legs = preview.plan.legs.map(l => ({ key: l.key, matrix: l.matrix, runs_on: l.runs_on, requires: l.requires }));
const diff = await kiln.ci.workflows.edit({ repo, path, content, title: input.title });
return { warnings: v.diagnostics, legs, diff };
```

## Where workflows live, and which dialect

| Path | Dialect |
| --- | --- |
| `.github/workflows/*.yml` | GitHub Actions', strict: unknown keys are errors, as on GitHub |
| `.kiln/*.yaml` (not the policy files: `review`, `owners`, `environments`, `agents`, `quarantine`, `routing`, `notify`, `deploy`, `config`, `secrets`) | kiln's: GitHub's keys and kiln's own |
| `.kiln/overlays/<stem>.yaml` | kiln keys added to `.github/workflows/<stem>.yml` without editing it |

A commit with any `.kiln` workflow runs those and ignores `.github/workflows` unless the
repository's `github_workflows` setting is `on`. To keep a workflow running on GitHub too, leave
it in `.github/workflows` and put kiln's keys in an overlay; an overlay may also replace
`runs-on`, nothing else of GitHub's.

## kiln's keys, the ones that matter most

- **Triggers**: GitHub's events, plus `diff` (a diff's version), `land` (a land queue
  candidate), `review`, `comment`, `agent` (an agent asking, with typed `inputs`), `deploy`,
  `webhook`, `pipeline` (another workflow's run).
- **Checkout**: kiln's jobs check out the commit unless `checkout: false`; GitHub's use
  `actions/checkout`.
- **Matrices**: `strategy.matrix.platforms: [linux/amd64, linux/arm64, darwin/arm64]` makes a leg
  per platform, each on a runner of its own (with `runs-on: native`); `where:` drops
  combinations by an expression; `needs: [{ job: build, matrix: same }]` pairs legs with theirs.
- **Splitting work**: `strategy.shard: { items | files, target: 6m, balance: timing }` splits
  items or files into legs by recorded duration; each leg reads its part from
  `$KILN_SHARD_ITEMS`. `chunk` splits by count.
- **Caches and reuse**: `cache: [{ key, restore-keys, paths, scope, local }]`;
  `reuse: { paths, env }` takes an earlier result when the job's inputs are the same.
- **Outputs**: `outputs.<name>: { value, secret: true }`, `exports: { env }`, `env-from`.
- **Tests and benchmarks**: a step's `test-results: { path }`; a job's `quarantine: enforce`
  and `bench-gate: { metric, max-regression: 5% }`; metrics to `$KILN_METRICS`.
- **Where it runs**: `runs-on` (labels), `requires: ["os == linux", "memory >= 16Gi"]`,
  `prefer`, `exclusive: true`, `resources`.
- **Retries**: `retry: { max: 2, on: [runner-lost, infra] }`.
- **Debugging**: `debug: { keep-on-failure: 30m }`.

The schema for each dialect is `kiln.ci.workflows.validate`'s; when unsure of a key, validate a
small file with it and read the diagnostic, rather than guessing.

## Expressions

GitHub's (`${{ }}`, its contexts and functions), with kiln's additions in the kiln dialect:
`changed('crates/**')` (the push or diff changed files there), `git.short_sha`, `matrix.os` and
`matrix.arch` with `platforms`. A context used where it isn't available is an error, as on
GitHub: read the diagnostic's code (`KCI3xxx`) and move the expression.

## A workflow that fails before any job

Its run's conclusion is `startup_failure` and its `error` lists diagnostics with codes and
positions: validate the file at that commit (`kiln.ci.workflows.get({ repo, path, ref })` gives
its diagnostics too), fix, and propose.

## Porting a GitHub workflow

Most run unchanged in `.github/workflows`. What GitHub provides that kiln maps instead:

- hosted labels (`ubuntu-latest`) route by the server's routing rules: ask
  `kiln.ci.routing.explain({ repo, runs_on: "ubuntu-latest" })` where a job would go;
- `secrets.GITHUB_TOKEN` is kiln's job token, scoped by the job's `permissions`;
- GitHub's OIDC (`id-token: write`) is kiln's issuer, so cloud trust policies name kiln's issuer
  instead (see the kiln-ci-secrets skill).

For a whole migration from another CI system, the kiln-ci-migrate skill has the steps.

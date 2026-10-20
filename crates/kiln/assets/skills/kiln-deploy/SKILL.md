---
name: kiln-deploy
description: Ship a change through kiln's deployments - commit and watch the app render, sync and turn healthy; or release an app by version through its environments' gates (approvals, freezes, promotion) and roll it back. Use when asked to deploy, roll out, release, promote or roll back something kiln deploys.
---

# Deploying with kiln

kiln deploys what git says. You change manifests in git; kiln renders them, compares them with
the cluster and syncs. Don't edit the cluster by hand: an app with self-heal undoes it.

## Steps

1. **Find the app.** `kiln.cd.apps.list({ q: "<name>" })` (MCP: `code_run`, or the
   `cd_apps_list` tool). An app id is `project/name`.
2. **Change git.** Edit the app's source (its definition names the repository and path:
   `kiln.cd.apps.get({ app })` → `definition.sources`). Commit it as a reviewed change.
3. **Plan it before it lands.** Push the branch, then
   `kiln.cd.plans.get({ head: "<branch>", repo })` (MCP: `cd_plans_get`; one commit: `commit`;
   a pull request: `pr`). Every commit of the branch gets, for each app it touches, its own
   change, the whole branch's change against trunk, and the steps a sync would take against what
   runs now: what it creates, changes (`why`, with hunks), prunes, and the hooks it runs.
   `ok: false` means a diff fails: read its `problems` (a render that fails, an object
   `Prune=false` keeps) and fix them before landing. `notes` say what to know, such as an object
   another app deploys. `brief: true` leaves out the hunks.
4. **Let kiln see it.** Sources are polled; to look now: `kiln.cd.apps.refresh({ app })`.
5. **Sync.** Apps with `auto` sync by themselves. Otherwise `kiln.cd.apps.sync({ app })`
   (`prune: true` deletes what git no longer holds; `dry_run: true` only validates).
6. **Watch it land.** Poll `kiln.cd.apps.get({ app })` until `operation.phase` is
   `Succeeded` and the app is `Synced` and `Healthy`. A failed sync says why in
   `operation.message`; `kiln.cd.apps.resources({ app, out_of_sync: true })` says which objects
   differ and where (`why`).

## One program instead of many calls

```js
// code_run: refresh, sync and report in one call
await kiln.cd.apps.refresh({ app: input.app });
await kiln.cd.apps.sync({ app: input.app, prune: true });
const a = await kiln.cd.apps.get({ app: input.app });
return { sync: a.sync, health: a.health, op: a.operation };
```

Fleets: `kiln.cd.apps.sync_many({ project, health, ... })` starts every matching app in one call.

```js
// code_run: what a branch would deploy, as one line per app and diff
const p = await kiln.cd.plans.get({ head: input.branch, repo: input.repo, brief: true });
return { ok: p.ok, diffs: p.diffs.map(d => ({ commit: d.commit.subject, ok: d.ok,
  apps: d.apps.filter(a => a.touched).map(a => ({ app: a.app, problems: a.problems,
    steps: a.sync && a.sync.steps.map(s => `${s.step} ${s.key.kind} ${s.key.name}`) })) })) };
```

## Rolling back

Undo a bad deploy from its history, not by hand:

1. `kiln.cd.apps.history({ app })` lists deployments, newest first, each with an `id`.
2. `kiln.cd.apps.rollback({ app })` goes back to the one before what runs now (`to: <id>` picks
   one; `prune: true` also deletes what it didn't have). It waits for that render, then syncs.
3. The app holds there: automation won't redeploy the bad commit. When git changes the app (the
   fix lands), it follows git again by itself; `kiln.cd.apps.follow({ app })` lets go sooner.
   `kiln.cd.apps.get({ app }).pinned` says whether it is held, and since when.

## Rules

- Waves: objects annotated `argocd.argoproj.io/sync-wave` apply in order; a wave waits until
  the one before is healthy. Hooks (`argocd.argoproj.io/hook: PreSync`, …) run in their phase.
- An automatic sync of a revision that failed is not retried until git moves (or a retry policy
  says so). Fix forward in git.

## Releases by API: apps released by version

Some apps are released by version rather than by commit (docs/deploy-api.md): each is registered
in environments (staging, production, …), and a **release** puts a version there, through gates
(upstream environments, soak, approvals, windows, freezes). If `kiln.deploy.apps.get({ app })`
knows the app, release it this way; the tools are `deploy_*` (code mode: `kiln.deploy.*`).

1. **What runs where.** `kiln.deploy.apps.get({ app })`: each environment it's registered in, with
   the release that runs there. `kiln.deploy.registrations.versions({ app, environment })`: what
   it has run.
2. **Release.** `kiln.deploy.releases.create({ app, environment, version, artifacts, idempotency_key })`.
   Give an `idempotency_key` (any string you make up for this request): asked again with it, it's
   the same release, never a second. `artifacts` is what the version deploys, by name, ideally by
   digest: `{ image: { repository, tag, digest } }`. A version that doesn't exist is refused (422)
   and the answer names the newest ones; pick one of those, don't guess again.
3. **Wait.** `kiln.deploy.releases.wait({ id, timeout_s: 600 })` until `done`. Don't poll `get` in
   a loop, and never ask for the release again while one is in flight.
   - `succeeded`: it landed: its provider runs that version, synced and healthy.
   - `waiting`: a gate holds it (an upstream environment that doesn't run the version yet, a
     soak, a window, a freeze). Its `gates` say which, each with `passed` and `detail`, and
     `message` says why. It goes by itself once the gate lets it.
   - `waiting_approval`: people must approve it. Tell the person who asked, with its link; don't
     approve your own request.
   - `failed`: read `message` and `provider`. Its commit was reverted: what ran before runs
     again. Fix forward and release again.
4. **Promote.** `kiln.deploy.releases.create({ app, environment: "production", promote_from: "staging" })`
   releases exactly what staging runs: the same version and artifacts.
5. **Look before and after.** `kiln.deploy.registrations.diff({ app, environment })` (on Argo CD
   with `version`: what deploying it would change), `.resources` (each object's sync and health),
   `.manifests`, `.logs({ app, environment, kind: "Deployment", name })`.
6. **Act on what runs.** `kiln.deploy.registrations.actions({ app, environment, kind, name })`
   lists what its provider can do to an object now (`restart`, `pause`, `resume`, `scale`,
   `suspend`, `create-job`, `delete` for a pod), each with `disabled` and the `params` it takes;
   `kiln.deploy.registrations.action({ ..., action, params: { replicas: "3" } })` does one. A
   restart or a deleted pod fixes a stuck process; it changes no version and no values (a
   release does), and a sync may undo a scale.

```js
// code_run: release a version to staging and wait for it to land
const r = await kiln.deploy.releases.create({ app: input.app, environment: "staging",
  version: input.version, idempotency_key: `${input.app}-${input.version}-staging` });
const w = await kiln.deploy.releases.wait({ id: r.id, timeout_s: 600 });
return { id: r.id, state: w.release.state, done: w.done, message: w.release.message,
  gates: (w.release.gates || []).filter(g => !g.passed) };
```

**Rolling back.** `kiln.deploy.releases.rollback({ app, environment })` goes back to the newest
release that succeeded before what runs (`to: <release id>` picks one of
`kiln.deploy.registrations.candidates({ app, environment })`), as a new release with that one's
version, artifacts and values. `kiln.deploy.releases.cancel({ id })` stops one still waiting or
being delivered (a delivery is reverted).

**Rules for agents.**
- You release only where the environment lets agents (its `agents.deploy`); approve only where
  `agents.approve` says so, and never your own request. A 403 means a person must: say so, with
  what you would have released, and stop. Don't work around it (committing to a release
  repository by hand, or asking another agent).
- One release at a time per app and environment: while one is in flight a second is refused
  (409), or queued where the environment says so. Wait for it, or cancel it if that's what the
  person asked.
- A frozen environment holds every release until a person lifts the freeze: don't lift it.
- An environment moving to another provider (`kiln.deploy.environments.move_status`) holds every
  release there (409) until the move is done or cancelled: wait for it. Moving one is a person's.
- Secrets (a password, an API key) are a person's to set: `kiln.deploy.secrets.list({ app,
  environment })` says which exist and their versions, never values. When an app needs one that
  isn't there, ask the person to run `kiln app secret set <app> <NAME> --env <env>` (its value
  from their terminal); never put a secret in values, a commit or a message.

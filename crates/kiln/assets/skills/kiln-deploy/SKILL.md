---
name: kiln-deploy
description: Ship a change through kiln's GitOps deployments - commit, watch the app render, sync and turn healthy, and confirm what changed. Use when asked to deploy, roll out, release or promote something kiln deploys.
---

# Deploying with kiln

kiln deploys what git says. You change manifests in git; kiln renders them, compares them with
the cluster and syncs. Don't edit the cluster by hand: an app with self-heal undoes it.

## Steps

1. **Find the app.** `kiln.cd.apps.list({ q: "<name>" })` (MCP: `code_run`, or the
   `cd_apps_list` tool). An app id is `project/name`.
2. **Change git.** Edit the app's source (its definition names the repository and path:
   `kiln.cd.apps.get({ app })` → `definition.sources`). Commit it as a reviewed change.
3. **Plan it before it lands.** Push the branch, then `kiln.cd.plan({ head: "<branch>", repo })`
   (MCP: `cd_plan`; one commit: `commit`; a pull request: `pr`). Every commit of the branch gets,
   for each app it touches, its own change, the whole branch's change against trunk, and the
   steps a sync would take against what runs now: what it creates, changes (`why`, with hunks),
   prunes, and the hooks it runs. `ok: false` means a diff fails: read its `problems` (a render
   that fails, an object `Prune=false` keeps) and fix them before landing. `notes` say what to
   know, such as an object another app deploys. `brief: true` leaves out the hunks.
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
const p = await kiln.cd.plan({ head: input.branch, repo: input.repo, brief: true });
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

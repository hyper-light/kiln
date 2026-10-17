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
3. **Let kiln see it.** Sources are polled; to look now: `kiln.cd.apps.refresh({ app })`.
4. **Sync.** Apps with `auto` sync by themselves. Otherwise `kiln.cd.apps.sync({ app })`
   (`prune: true` deletes what git no longer holds; `dry_run: true` only validates).
5. **Watch it land.** Poll `kiln.cd.apps.get({ app })` until `operation.phase` is
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

## Rules

- Waves: objects annotated `argocd.argoproj.io/sync-wave` apply in order; a wave waits until
  the one before is healthy. Hooks (`argocd.argoproj.io/hook: PreSync`, …) run in their phase.
- An automatic sync of a revision that failed is not retried until git moves (or a retry policy
  says so). Fix forward in git.

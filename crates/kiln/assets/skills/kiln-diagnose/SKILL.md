---
name: kiln-diagnose
description: Find out why a kiln deployment is OutOfSync, Degraded, Progressing for too long, or failed to sync, and what to change. Use when an app or a fleet of apps is unhealthy or drifting.
---

# Diagnosing deployments

## Read the whole picture in one call

```js
// code_run: every unhealthy app with what differs and what's unhealthy
const apps = await kiln.cd.apps.list({ health: input.health ?? "Degraded", limit: 1000 });
const out = [];
for (const a of apps.apps.slice(0, 50)) {
  const r = await kiln.cd.apps.resources({ app: a.app });
  out.push({
    app: a.app, sync: a.sync, health: a.health, op: a.operation?.message,
    bad: r.resources.filter(x => x.sync !== "Synced" || (x.health && x.health !== "Healthy"))
      .map(x => ({ kind: x.kind, name: x.name, health: x.health, why: x.why, message: x.message })),
  });
}
return out;
```

## What the fields mean

- `sync`: `OutOfSync` means git and the cluster differ. Each resource's `why` names the first
  field that differs (`.spec.replicas differs`, `… is missing live`, `… is no longer set; the
  next apply removes it`) or says it is `not in the cluster` / `requires pruning`.
- `health`: the worst of the app's resources, Argo CD's rules: Deployments wait for their
  rollout, Pods degrade on crash loops, Jobs on failure, Services of type LoadBalancer wait for
  an address.
- `operation`: the running or last sync, its `trigger` (manual, auto, self-heal) and `message`.
- `source_error` on an app: its repository couldn't be fetched or rendered.

## Then

- A diff: `kiln.cd.apps.diff({ app })` gives desired and live YAML and unified hunks.
- Drift someone made by hand: sync (self-heal does it by itself when enabled).
- A bad change in git: fix forward with a commit; check `kiln.cd.apps.history({ app })` for
  what last deployed well.
- Render errors (`render failed: …`): the chart or kustomization doesn't build; reproduce with
  `helm template` / `kubectl kustomize` on the source path.

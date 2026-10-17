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

## Look inside: the tree, events and logs

`kiln.cd.apps.tree({ app })` gives the app's objects and every live object they own (ReplicaSets,
Pods, Jobs, EndpointSlices), each with `parents` (owner uids), `health`, `status` (a pod's state in
kubectl's words: `CrashLoopBackOff`, `ImagePullBackOff`, `Init:0/1`), `ready`, `restarts` and
`containers` (each with its `reason` and the `last` reason it ended). For one object,
`kiln.cd.apps.resource_events({ app, group, kind, namespace, name })` gives its Kubernetes events
newest first, and `kiln.cd.apps.resource_logs({ app, group, kind, namespace, name, previous: true })`
the last lines of a pod's log (or of a workload's pods): `previous` reads the run before a crash.

```js
// code_run: why do this app's pods crash?
const t = await kiln.cd.apps.tree({ app: input.app });
const bad = t.nodes.filter(n => n.kind === "Pod" && (n.health === "Degraded" || n.restarts > 0)).slice(0, 5);
const out = [];
for (const p of bad) {
  const key = { app: input.app, group: "", kind: "Pod", namespace: p.namespace, name: p.name };
  const ev = await kiln.cd.apps.resource_events(key);
  const logs = await kiln.cd.apps.resource_logs({ ...key, previous: p.restarts > 0, tail_lines: 30 });
  out.push({ pod: p.name, status: p.status, restarts: p.restarts, containers: p.containers,
    events: ev.events.slice(0, 5).map(e => `${e.type} ${e.reason}: ${e.message}`),
    log: logs.lines.map(l => l.text) });
}
return out;
```

## Then

- A diff: `kiln.cd.apps.diff({ app })` gives, per object that differs, the live YAML, what a sync makes of it (`predicted`) and the unified hunks between them. Only what a sync changes shows, not the fields the cluster filled in.
- Drift someone made by hand: sync (self-heal does it by itself when enabled).
- A bad change in git: fix forward with a commit; check `kiln.cd.apps.history({ app })` for
  what last deployed well.
- Render errors (`render failed: …`): the chart or kustomization doesn't build; reproduce with
  `helm template` / `kubectl kustomize` on the source path.

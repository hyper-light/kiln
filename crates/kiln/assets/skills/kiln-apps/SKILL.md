---
name: kiln-apps
description: Define what kiln deploys - apps in .kiln/deploy.yaml, Argo CD Applications kiln reads, sources (Helm, Kustomize, directories), destinations, sync policy and options. Use when adding or changing an app's definition.
---

# Defining apps

## kiln's format: `.kiln/deploy.yaml`

```yaml
apps:
  web:
    source: deploy/web                       # a path in this repository; the engine is detected
    helm: { values: [values.yaml], set: { image.tag: "${{ git.short_sha }}" } }
    to: [staging, production]                # environments from .kiln/environments.yaml
    namespace: web
    sync: { auto: true, prune: true, self-heal: true, options: [CreateNamespace=true] }
```

Engines: `Chart.yaml` → Helm; `kustomization.yaml` → Kustomize; otherwise plain YAML/JSON
(`directory: { recurse, include, exclude }`).

## Argo CD Applications

kiln reads `argoproj.io/v1alpha1` Applications with every field's meaning: `source`/`sources`
(with `ref` sources read as `$values/...`), `destination` (server URL or name; the in-cluster
URL means the cluster kiln runs in), `syncPolicy.automated` (`prune`, `selfHeal`,
`allowEmpty`), `syncOptions`, `retry`, `ignoreDifferences`. An Application an app renders becomes
its child (app of apps).

## Sync options (Argo CD's names)

`CreateNamespace=true`, `PruneLast=true`, `PrunePropagationPolicy=foreground|background|orphan`,
`ServerSideApply=true` (kiln's default), `Replace=true`, `ApplyOutOfSyncOnly=true`,
`RespectIgnoreDifferences=true`, `SkipDryRunOnMissingResource=true`, `Validate=false`; on one
object: `argocd.argoproj.io/sync-options: Prune=false` or `Delete=false`.

## Making an app through the API

`kiln.cd.apps.create({ definition: { name, project, sources: [{ repo, path, revision, engine:
"Auto" }], destination: { cluster, namespace }, sync: { auto: true, prune: true, self_heal: true }
} })`. Prefer git: a definition in git is reviewed and versioned.

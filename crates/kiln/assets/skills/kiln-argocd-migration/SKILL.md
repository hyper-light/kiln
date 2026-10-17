---
name: kiln-argocd-migration
description: Move apps from Argo CD to kiln safely - import Applications, run them in shadow mode, compare kiln's verdicts with Argo CD's, then take them over. Use when asked to migrate off Argo CD or to check kiln against it.
---

# Moving from Argo CD

1. **Import.** `kiln.cd.import({ manifests })` with the Applications' YAML (or `kiln cd import
   app.yaml`). A root app of apps brings its children with it when it renders.
2. **Shadow.** Imported apps start in shadow mode (`shadow: true`): kiln renders and compares
   but never writes. It applies as Argo CD's field manager (`argocd-controller`) and reads Argo
   CD's tracking annotation, so the same objects belong to the same apps.
3. **Compare.** For each app, kiln's `sync` and `health` should match Argo CD's
   (`kubectl -n argocd get applications`). Where they don't, `kiln.cd.apps.resources({ app,
   out_of_sync: true })` says which field differs and why.
4. **Take over.** When they agree, turn off auto-sync in Argo CD for the app first, then
   `kiln.cd.apps.take_over({ app })`. Nothing is recreated; kiln syncs it from then on.

Check a whole fleet in one program:

```js
const apps = (await kiln.cd.apps.list({ limit: 1000 })).apps.filter(a => a.shadow);
return apps.map(a => ({ app: a.app, sync: a.sync, health: a.health, differ: a.out_of_sync }));
```

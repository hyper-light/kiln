# Deploying by API: getting started

From nothing to an app released to staging, promoted to production, and rolled back, with
`kiln` (each step has its page under Deployments → Releases, and its `deploy_*` tool for
agents). The design and what is built: [deploy-api.md](deploy-api.md).

## 1. Environments

An environment is where releases go: a provider (KilnCD, kiln's own, or one of kiln's Argo CD
integrations), a release repository on kiln's git server (made when the environment is), and
the clusters it deploys to.

```sh
kiln deploy env add staging                                   # KilnCD, the cluster kiln runs in
kiln deploy env add production --cluster prod-eu --gates production-gates.yaml
kiln deploy env add production --provider argocd/prod-argo    # or through Argo CD
```

`production-gates.yaml` says what each release there waits for:

```yaml
after: [staging]        # the same version, with the same images, runs in staging first
soak: 30m               # for half an hour
promote: auto           # and then comes here by itself (or: kiln deploy … --from staging)
approvals: { count: 1, reviewers: [ada, grace] }
windows:
  - { deny: "0 17 * * 5", duration: 64h, timezone: Europe/Berlin }   # no releases over the weekend
checks: [no-snapshots, owners-set]
```

In the UI: New environment on the Releases board, then its gates on its page (and its freeze).

## 2. An app

How it deploys: a Helm chart, a path in git, or a platform KilnCD deploys to.

```sh
kiln app create web --owner payments \
  --helm-repo oci://ghcr.io/acme/charts --chart web --set 'image.tag=${{ release.version }}'
```

A template says it for every app made from it (its package, namespace, checks, and the source
repository it makes from a starter): `kiln app template-add service.yaml`, then
`kiln app create web --template service`. In the UI: New app.

No chart yet? `kiln app init web --image ghcr.io/acme/web` writes `charts/web` on kiln's base
chart (a Deployment, Service, Ingress, autoscaling, disruption budget, config, cron jobs,
migrations, with safe defaults) and `deploy/<env>.yaml`; commit it, then create the app with
`--git-repo <repository> --path charts/web`.

Register it where it runs:

```sh
kiln app register web --env staging
kiln app register web --env production
```

## 3. Release

```sh
kiln deploy web 1.4.2 --to staging          # follows it until it lands (--no-wait to not)
kiln deploy diff web --env staging --version 1.5.0   # what 1.5.0 would change, first
```

A version that isn't there is refused at once, with the newest that are. Once 1.4.2 has run in
staging for its soak, production gets it by itself (`promote: auto`) and waits for an approval:
answer it on the board (on a phone too), or

```sh
kiln deploy approve 42 --note "checked the dashboards"
```

By hand instead: `kiln deploy web --to production --from staging`.

## 4. Secrets

```sh
kiln app secret set web DB_PASSWORD --env staging       # asked for, unechoed (or piped in, or --from-file)
kiln app secret import web .env.staging --env staging    # each NAME=value, released together
kiln app secret list web --env staging                   # names and versions: values are never shown
```

Each release delivers them, at their versions as it commits, as the Secret `web-kiln` in the
app's namespace (on Argo CD too, where kiln reaches the cluster: `kiln deploy env add …
--kilncd in-cluster=<a KilnCD cluster>`), and kiln-base reads it as env with `kilnSecrets: true`.
On Lambda, ECS, Workers and Vercel they go the platform's own way (docs/deploy-api.md §3.5). A change is a release of its own, through the environment's gates; its pods
roll with it. `kiln app secret uses web --env staging` says each time a delivery read them.

## 5. Look, change, roll back

```sh
kiln deploy status web --env production      # what runs, what's on its way, what the provider says
kiln deploy resources web --env production   # each object, synced and healthy
kiln deploy logs web --env production        # its pods' last lines
kiln deploy history web --env production
kiln deploy web --to production --values values-production.yaml   # new values: a config release
kiln deploy rollback web --env production    # to the release before (--to <id> for another)
kiln deploy fleet --env staging --template service --set replicaCount=2   # many apps at once
```

A release that fails (its provider refuses it, its sync fails, or it doesn't land in time) is put
back: a revert release restores what ran before, followed until it runs again. A rollback that
fails, or a release asked `--no-revert` (a migration that can't run backwards), is held instead,
and the app's page says it needs a person until the next release lands.

## 6. People and agents

- **Messages**: an app's chat channels (`slack:#deploys`, on its page or `kiln app set web
  --channel slack:#deploys`) hear when its releases wait for approval, land, fail or are
  cancelled; the server's notification rules (`[[ci.notify]]`, `on = ["release.failed"]`) send
  them anywhere kiln sends.
- **Agents** release with the `deploy_*` tools (and read the `kiln-deploy` skill), only in
  environments whose agents policy lets them (a person sets it: `--agents`, or on its page), and
  never approve their own releases.
- **Metrics**: `kiln deploy metrics --env production`: how often changes land, how long they take,
  how many fail, how long a fix takes.

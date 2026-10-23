# Deploying by API

> **Agreed 2026-10-18; being built on the `deploy-api` branch.** It sits on top of
> [cd.md](cd.md), the CD engine, and [cd-targets.md](cd-targets.md), the platforms KilnCD deploys
> to. The [status](#status) table says what runs; [deploy-quickstart.md](deploy-quickstart.md)
> takes an app from nothing to production with what is built.

kiln runs an application's whole life by API: create it, register it in environments, deploy
versions, promote them, roll them back, change their configuration, operate them and retire them.
Every deploy is a commit to a **release repository**, and a **CD provider** (KilnCD, or Argo CD)
turns that commit into what runs. The same calls work from the API, `kiln`, the UI, chat and
agents (MCP), whichever git host keeps the repository and whichever provider deploys it.

The model comes from txst (cloud9), which deploys by committing per-environment metadata to a
GitOps repository that Argo CD watches. kiln keeps that core and removes its limits: any git host,
either CD provider, every platform KilnCD supports, apps created end to end, and no Postgres or
Redis (§17 maps each txst function to kiln; §18 lists what goes beyond it).

## At a glance

- **Deploy by API.** Apps, environments, registrations, releases, approvals, rollbacks and history
  are API resources. The UI, `kiln`, chat and MCP are clients of the same calls.
- **A deploy is a commit.** The release repository says what every environment runs. kiln writes
  it, one release a commit, and git's history is the deploy history. GitOps holds for every
  provider.
- **Any git host.** The release repository, and an app's source, live on kiln's built-in git
  server, GitHub, GitLab or Bitbucket.
- **Two CD providers, chosen per environment.** KilnCD (built in) or Argo CD (an instance per
  environment, or shared). The deploy flow, the records and the API don't change with the
  provider.
- **Every platform through KilnCD.** An environment on KilnCD reaches Kubernetes and everything
  else KilnCD deploys to (AWS, Google Cloud, Azure, Cloudflare, Vercel, …), with rollouts. An
  environment on Argo CD reaches Kubernetes.
- **Rollback is a commit too.** A rollback is a new release of an earlier version: the same on
  both providers, and git keeps saying what runs.
- **One binary.** Records live in KilnDB, kiln's embedded database. Work and events go over Focal's
  embedded event bus. No Postgres, no Redis.

## Decisions

Agreed with the user on 2026-10-18:

| # | Decision |
| --- | --- |
| A1 | Deploy by API: apps, environments, registrations, releases, approvals, rollback and history are API calls; every client uses them |
| A2 | A deploy is a commit to a release repository; git's history is the deploy history, for every provider |
| A3 | Git hosts: kiln's built-in git server, GitHub (everything kiln does with GitHub today stays), GitLab and Bitbucket |
| A4 | CD providers per environment: KilnCD or Argo CD, behind one contract (§8.1) |
| A5 | KilnCD as the provider brings every platform KilnCD supports, with the GitOps flow kept; Argo CD environments are Kubernetes only |
| A6 | Rollback is a new commit of an earlier release, on both providers; never Argo CD's own rollback |
| A7 | Records in KilnDB, kiln's embedded database (its owner confirms the interface, §14.1) |
| A8 | Work and events on Focal's embedded event bus (its owner confirms the interface, §14.2) |
| A9 | Argo CD support stays a product feature: import, shadow mode and take-over as today, and Argo CD as a CD provider. kiln's own cluster deploys with KilnCD |
| A10 | txst's functions for apps and deployments are the floor: kiln does each one (§17), and goes beyond them (§18) |

## 1. Architecture

```
 Clients     API · kiln · UI · chat · MCP (agents) · webhooks
               │
 Control     apps · environments · registrations · releases · gates · approvals · history
 plane         records: KilnDB                      work and events: Focal bus
 (kiln)        │ commit: deploy, rollback, config, enable, disable              ▲ status
               ▼                                                                 │
 Release     a git repository on a git host: kiln · GitHub · GitLab · Bitbucket
 repository    │ read                                                            │
               ▼                                                                 │
 CD          per environment ───────────────────────────────────────────────────┘
 provider      KilnCD: Kubernetes, AWS, Google Cloud, Azure, Cloudflare, Vercel, …
               Argo CD: Kubernetes
```

- **The control plane** decides what an environment should run. It checks who asks and whether
  the environment's gates allow it, records the release, writes the commit, and follows the
  provider until the release has landed or failed.
- **The release repository** holds the decision. Anything that reads git can deploy from it, and
  anyone can read it to see what runs where.
- **The CD provider** makes the environment match the repository, and reports what it did. kiln
  needs nothing from a provider beyond the contract in §8.1.

Each boundary has one question across it: the commit (what should run) and the status (what does
run). A provider that is down delays a release; it never loses one, since the commit is already
in git and the record in KilnDB.

## 2. Concepts

| Term | Meaning |
| --- | --- |
| **App** | What a team owns and ships: one or more services, deployed together, owned by a team. The unit of ownership and permission (txst's "application"). |
| **Service** | One deployable part of an app: a Kubernetes workload, a cron job, a Lambda function, a Worker. |
| **Package** | How an app is deployed: a Helm chart from an OCI or Helm repository, a path in a git repository (Helm, Kustomize, plain manifests), or a platform definition KilnCD deploys (cd-targets.md §4). |
| **Version** | What a release deploys: a semver (a prerelease is a snapshot) or any string. A version names its artifacts. |
| **Artifact** | Bytes CI built: an image digest, a chart, a bundle. Promotion moves the same bytes between environments, never a rebuild. |
| **Environment** | A stage (`staging`, `production`) with its CD provider, its release repository, its targets and its gates. |
| **Target** | Where an environment runs things: Kubernetes clusters as its provider names them, cloud accounts, platform projects. |
| **Registration** | An app in an environment: its path in the release repository, its settings there, enabled or not. |
| **Release** | One numbered change to what an environment runs: a deploy, a config change, a rollback, enabling or disabling. One release, one commit. |
| **Gate** | What a release waits for before its commit: upstream environments, soak, approvals, windows, checks, freezes. |
| **Owner** | A team: a kiln group, or a group from the identity provider. Owners may release their app where its environments allow them. |
| **Template** | Data that sets an app's defaults: package, namespace pattern, allowed targets, checks, what the app may install (txst's app types). |

A registration becomes one KilnCD app per target (`<project>/<app>-<env>`, as cd.md names apps)
when the environment's provider is KilnCD, and one Argo CD Application per target
(`<env>-<app>`, or `<cluster>-<app>` with several clusters) when it is Argo CD.

## 3. Apps

### 3.1 Creating an app

`POST /api/deploy/apps`, `kiln app create`, or the builder in the UI. Each step below is optional
and chosen in the call; an app can start as a name and an owner and grow from there.

1. **Name.** DNS-1123, unique in kiln. The source repository has the same name by default.
2. **Owners.** An owner team, and optionally a co-owner and an ops owner (txst's three), each a
   kiln group (`shop`: `/api/groups`, `kiln groups`, made and filled by someone with
   `users.manage`) or an identity-provider group (`github:acme/platform`, `gitlab:acme/platform`,
   `bitbucket:acme`: the groups a person was in when they last signed in with that provider). The
   caller must belong to the owner team, unless an admin (`users.manage`) creates it; changing an
   app's owners takes someone in the owner team it has and the one it gets. `/api/me` says which
   groups the caller is in.
3. **Template.** Sets the package defaults, the namespace pattern (`<app>`, `apps-<app>`, …), the
   targets the app may go to, what it may install (CRDs and cluster-scoped objects: operators), and
   its default checks.
4. **Source repository.** Link an existing one (`source: acme/web`, any repository kiln has), or
   have the template make it: one of kiln's own repositories (W14), named as the template says
   (`acme/{app}`), its first commit the files of the template's starter repository (its branch's
   tree, not its history) with `{{kiln.app}}`, `{{kiln.project}}`, `{{kiln.owner}}` and
   `{{kiln.repository}}` filled in, in files and in file names. The starter holds `.kiln/ci.yaml`
   that builds and publishes the image and the chart, a `Dockerfile`, and a chart (on kiln's base
   chart, §3.4, once it ships), so the app's first build starts with its first commit. kiln copies
   the starter's files and runs nothing from it; links stay links and files keep their modes.
   A name already taken is refused before anything is made, and nothing half made is kept. The
   template's `repository.host` makes it on GitHub (through kiln's GitHub App), a GitLab (with the
   token covering its group) or Bitbucket instead (§7): made there private, its first commit
   pushed with credentials that write that repository, then one of kiln's repositories as any
   upstream is, so CI runs from it (its first commit builds as a push would). One the host has
   already is refused; one made whose first commit didn't reach it is deleted again.
5. **Registries.** Create the repositories for images and charts on the chosen registry (any OCI
   registry: ECR, Artifact Registry, ACR, GHCR, Docker Hub, kiln's own later), snapshot and release
   paths, release tags immutable, lifecycle rules. Built: a template's `registry: { at, name,
   keep_snapshots }` makes, with each app, release images at `<name>` (tags immutable: a tag never
   points at another image, nor goes), snapshots at `<name>/snapshots` (tags move; only the
   newest `keep_snapshots` stay) and its chart at `charts/<name>` (immutable), and keeps them on
   the app (`registries`) and its page. `at: kiln` makes them on kiln's own registry (its
   repositories' policies); an `aws` integration on ECR (tag mutability, and lifecycle policies:
   untagged images go after a week); a `gcp` one on Artifact Registry (immutable tags, cleanup
   policies; `location`, and its `project`). GHCR, ACR and Docker Hub make a repository at its
   first push, without these rules: kiln makes nothing there. A starter's `{{kiln.image}}`,
   `{{kiln.snapshots}}` and `{{kiln.chart}}` are filled in with them, so its CI pushes there.
6. **Release repository.** kiln writes `<project>/<app>/app.yaml` (§6) in each release repository
   its environments use.
7. **Environments.** Register the app where it will run (§5).
8. **Catalog and contacts.** Owners, on-call, chat channels for release messages, links (dashboards,
   runbooks), and a `catalog-info.yaml` that Backstage reads.

txst only registers an app; creating the repository and the registries is left to the team. kiln
does both, so one call (or one page) takes an app from nothing to a first deploy.

### 3.2 Templates

A template is data, kept in kiln or in a repository, never code. txst's four types (rest,
internal, operator, datavant) are four templates; any number can be added. A template sets:

- the package kind and its defaults (chart repository layout, value files, service names);
- the namespace pattern, and which targets the app may use (clusters by label, accounts);
- what it may install: an operator template allows CRDs and cluster-scoped objects, the others
  don't. On Argo CD this becomes the AppProject's allow lists (§8.3); on KilnCD, the app's project;
- the checks its releases must pass (production rejects snapshots, for example);
- properties a team may set on the app, with their allowed values (txst's `repo_url_base`);
- the registry each app's images and chart go to (§3.1, step 5): `registry: { at: kiln, name:
  {project}/{app}, keep_snapshots: 20 }`, put by someone with `settings.manage`;
- the source repository each app gets (§3.1): `repository: { name: acme/{app}, starter:
  https://github.com/acme/service-starter, starter_ref: v3, branch: main, host: github }` (`host`:
  `kiln`, the default, `github`, `gitlab` or `bitbucket`). Each app made from
  such a template makes a repository, so the template is put by someone who may add repositories
  (`settings.manage`); apps are then made from it by whoever may make apps.

Changing an app's template is allowed and says what it changes first: namespaces, allow lists and
checks change for every registration, as a release each.

### 3.3 Maintaining apps

- **Info**: owners, template, properties, registrations, what runs where with health, the last
  releases, links. `GET /api/deploy/apps/{app}`.
- **Updates**: owners, contacts, properties, channels, template. Each is audited; changes that touch
  what runs are releases.
- **Secrets** per app and environment, from kiln's secret store, delivered to targets the way each
  platform takes them (§3.5: a Kubernetes Secret on KilnCD and on Argo CD; Lambda's variables, SSM
  parameters an ECS task reads, Workers' secret bindings, Vercel's sensitive variables).
- **Fleet changes**: one setting, or a base chart version, changed across many apps as one reviewed
  change, followed together (`POST /api/deploy/fleets`, `kiln deploy fleet`, `deploy.fleets.*`;
  `GET /api/deploy/fleets/{id}`, `--show`, says how it stands; asked again with the same key, it
  is the same change). It selects apps by name, or by template, project or owner (in an
  environment, those registered there), and is made one of two ways:
  - **As releases**, in one environment: one version to each, or values set in each app's own
    (`path=value`, merged into its values file: a config release each), each through its gates.
  - **In their sources** (`source`): a kiln-base version (`base`: the chart's dependency moved in
    place; a vendored kiln-base only to the version this kiln ships, its files with it), or values
    set in each app's chart (`charts/{app}` by default, as `kiln app init` lays it out; `chart`
    and `values` say otherwise). It's one commit per source repository on a branch of the change
    (`kiln/fleet/<id>`, trailer `Kiln-Fleet: <id>`): a pull request on GitHub and Bitbucket, a
    merge request on GitLab. On kiln's own git server, which has none, it's the branch and its
    diff, which kiln merges when asked (`POST …/fleets/{id}/merge`, `--merge`: a fast-forward, or
    a merge commit where the repository moved; a conflict is refused). Each repository's proposal
    reads as open, merged, closed, unchanged (it holds the change already) or failed (and why);
    `DELETE …/fleets/{id}` (`--withdraw`) closes what's open. It's an app's maintenance:
    environments.manage's.
- **Archive**: no new releases; history kept; what runs keeps running.
- **Delete**: deregister everywhere (objects deleted or kept, per registration), then remove the
  app. The source repository is kept (registries, once kiln makes them, too).

### 3.4 The base chart and app scaffolding

kiln ships a base Helm library chart, as txst ships its own: workloads, cron jobs, autoscaling,
disruption budgets, services, ingress, service accounts, probes, config templates, network
policies. `kiln app init` scaffolds a service and an application chart on it, with per-environment
value files. The base chart is versioned like any chart, and moving apps to a new version is a
fleet change (§3.3).

Built: `kiln-base` (a library chart kiln ships in its binary) renders an app's Deployment
(spread across nodes, restarted when its config changes, its image by digest when a release
gives one), Service, Ingress, HorizontalPodAutoscaler, PodDisruptionBudget, ServiceAccount (a
cloud identity by annotation), config files and env, cron jobs, hook jobs (a migration before
each release), NetworkPolicy and volumes, each turned on by values, with safe defaults (not
root, a read-only root filesystem with a writable `/tmp`, every capability dropped, the
runtime's seccomp profile). An app chart's one template is `{{ include "kiln-base.all" . }}`.
`kiln app init <app>` writes `charts/<app>` and `deploy/<env>.yaml` for each environment into a
repository, keeping what's there unless `--force`. The chart depends on kiln-base in kiln's own
registry: each server publishes the kiln-base it ships at start
(`oci://<kiln>/kiln/charts/kiln-base:<version>`, with Helm's media types, as `helm push` would),
which anyone pulls and nobody pushes over or deletes, the server's administrator neither, since
every app's chart depends on it. `helm dependency build` fetches it, and KilnCD lays the server's
own copy into a chart it renders, so a render never pulls it. `--vendor` (or no kiln named:
`--url`, `KILN_URL` or `--registry`) copies kiln-base into the chart's `charts/` instead. `--base`
moves only kiln-base to the version this kiln ships: the dependency in `Chart.yaml` (the rest
of it as it was) and, vendored, its files.

### 3.5 Secrets per app

An app's secrets for an environment (a database password, an API key) come from kiln's secrets
service and reach what runs without ever passing through git. The service is part of kiln, in its
one binary like everything else: no other process, port, store or dependency to run. Agreed
2026-10-19, as the first piece of a larger one: just-in-time secrets (AWS Secrets Manager's
scale, Vault's controls), which will take over the storage and issuing, inside kiln too. The code
is cut so that cutover swaps one backend module: the API, `kiln`, the UI, delivery and audit stay
as they are.

- **The service** (`crate::secrets`): a scope (today an app in an environment), its secrets by
  name, their versions, who set each and when, and every read of their values. Names are
  environment variables' (`DB_PASSWORD`), values at most 64 KiB, a scope at most 200 secrets
  and 960 KiB in all (they're delivered as one Kubernetes Secret, which holds at most 1 MiB).
  The policy is the service's, the same for every backend: who may set, list and read, and that
  agents never set values or read them.
- **Backends** (`secrets::Backend`): `list`, `versions`, `put` (a new version), `delete` (a
  tombstone version: history stays), `resolve` (the current values for a delivery, each with
  its version, and until when they hold: a lease, from a backend that issues them), `rotate`.
  - **kiln's own** (`secrets::kiln`, today): versions kept in kiln's store (the last ten with
    their values, older ones' values destroyed, their record kept), each sealed with
    AES-256-GCM under a data key of its own, the data keys wrapped by the server's master key
    (the secret store's, W09), each ciphertext bound to its scope, name and version; `rotate`
    re-seals every value under a new data key. Each read for a delivery is recorded (who, why,
    which versions).
  - **Just-in-time secrets** (later, `secrets::jit`, a module of kiln's binary like the rest):
    the same calls; leases (credentials kiln issues for a while, from a database's or a cloud's
    own) come back with `resolve`, and delivery renews them before they expire.
- **Who.** People with `secrets.manage` set and delete; anyone who reads deployments lists names
  and versions; values are never read back through the API. Agents list names and versions,
  and never set or delete (the service's one policy check, `Secrets::may`, where Vault-like
  policies will go).
- **API.** `GET /api/deploy/apps/{app}/environments/{env}/secrets` (names, versions, sizes, who,
  when), `PUT …/secrets/{name}` (`{value}`, or `{base64}` for bytes; write-only: a new version),
  `DELETE …/secrets/{name}`, `GET …/secrets/{name}/versions`, `GET …/secret-uses` (each read for
  a delivery), `POST /api/deploy/secret-keys/rotate` (every value re-sealed under a new data key,
  the old ones retired). A change answers with the release that delivers it and what happens next;
  `?release=false` keeps it for a later release. `kiln app secret set|import|list|rm|versions|uses|rotate-keys`
  (a value from stdin, asked for unechoed at a terminal, or `--from-file`: never an argument;
  `--no-release`); `kiln deploy <app> --to <env> --secrets`; `deploy.secrets.list` for agents; a
  Secrets tab on each registration's page.
- **Pinned by releases.** Each release pins the secrets' versions as it commits (`release.json`'s
  `secrets: "API_KEY@1,DB_PASSWORD@4"`, never a value), so what runs changes only through a
  release and its gates, and the release repository's history says which versions ran when.
- **On KilnCD.** The definition KilnCD deploys a release from names the pinned versions; at
  render KilnCD adds a Secret `<app>-kiln` in the app's namespace with their values, as it adds
  image pull secrets: tracked as the app's own, applied by a sync, shown masked in manifests,
  diffs and plans, never written to disk. Values are read once for each set of versions (the read
  recorded: `kilncd`, `render shop/web-staging`, the versions) and kept in memory; the versions
  (never the values) are part of the render key. Each pod template that reads the Secret (env, a
  key of it, a volume) is marked with the versions (`kiln.dev/secrets`), so its pods roll when a
  release delivers new ones. kiln-base's `kilnSecrets: true` reads it as env (optional: an app
  without secrets has none).
- **On Argo CD.** Argo CD renders from git alone, so the Secret can't be added at render. kiln
  applies it itself, as nothing else needs installing: each release writes `<app>-kiln` into the
  app's namespace before its commit reaches Argo CD (with `via: pull-request`, what the merge
  pins, as kiln sees the merge and before it hands the merge to Argo CD: an open pull request
  changes nothing that runs), through the KilnCD cluster of the same
  server that reaches Argo CD's (the environment's `targets.kilncd` names it by Argo CD's name for
  the cluster, `{ in-cluster: prod-k }`, as `kiln deploy env add … --kilncd in-cluster=prod-k`
  sets it; otherwise KilnCD's cluster of that name). It carries none
  of Argo CD's tracking, and `compare-options: IgnoreExtraneous`, so Argo CD neither shows it as
  the app's nor prunes it. The release says its versions to the chart (the ApplicationSet's
  `kiln.secrets` parameter), which kiln-base stamps on its pod templates (`kiln.dev/secrets`), so
  pods roll onto new ones as on KilnCD. The values are read once for each set of versions, as a
  release delivers them (the read recorded: `kiln`, `deliver web in production to Argo CD's
  in-cluster (through prod-k)`); a release that leaves the versions as they are leaves the Secret
  as it is. A release of an app with secrets where kiln reaches no such cluster is refused, saying
  how to add one; the first release after its last secret is deleted deletes the Secret kiln
  wrote. Where kiln can't reach the cluster at all, the External Secrets Operator with
  kiln as its webhook provider (an ExternalSecret in the chart, a token scoped to that app and
  environment) or Sealed Secrets (ciphertext in git) remain the fallbacks, each a component to
  install: not built.
- **A change is a release.** Setting or deleting a secret asks for a secrets release (a config
  release that keeps the version and values: "secrets: DB_PASSWORD v4") where something runs, on
  KilnCD or Argo CD, so the environment's gates, history and audit apply. A release still waiting takes the
  change on instead, and says so for whoever approves it; one already committing doesn't, and an
  environment that takes one release at a time refuses another (the change is kept, and the
  answer says how to release it). Where nothing runs yet, the first release delivers them. A
  rollback pins the versions current then: it restores a release's configuration, never older
  secret values.
- **Gone with what they belong to.** Deleting an app, or an environment, deletes its secrets
  there and destroys their values; the record of their versions and reads stays.
- **Platforms.** Lambda, ECS, Workers and Vercel take them their own way, through KilnCD's
  providers. The objects KilnCD applies there hold references, never values
  (`kiln-secret:app/web/production/API_KEY@2`; a render's own objects can't hold one): the
  provider reads each value from the service as it applies (the read recorded: `kilncd`,
  `deliver default/web-production's API_KEY@2`), sends it to the platform and keeps nothing. Each
  reads back as kiln's reference while the platform holds what kiln put there, otherwise as
  `kiln-secret:changed` (drift), so no value is in a desired object, a record, a plan or a view;
  and one the app no longer has is taken away.
  - **Lambda**: the function's environment variables (a secret in place of a variable of its
    name), kiln's while the function is at the revision kiln left it at; new versions are a
    configuration update and a new version of the function, which the alias follows.
  - **ECS**: an SSM SecureString parameter each (`/kiln/app/<app>/<env>/<NAME>`, under the
    account's default key for SSM, written before the task definition), read by each container
    through `secrets` (in place of a variable of its name); the versions are a docker label
    (`kiln.dev/secrets`), so new ones are a new revision and the service rolls onto it. The task
    execution role needs `ssm:GetParameters` on `parameter/kiln/app/<app>/<env>/*`. A parameter
    is kiln's while SSM holds the version kiln wrote.
  - **Workers**: `secret_text` bindings of the upload (Cloudflare never answers their text),
    kiln's while the version kiln deployed serves; secrets set outside kiln are kept across
    uploads.
  - **Vercel**: the project's sensitive production variables, kiln's while each is as kiln left
    it; production carries their versions, so new ones make a new deployment, which takes them.

  Each platform's limits apply: Lambda's 4 KB of variables in all is the tightest.

## 4. Environments

`POST /api/deploy/environments`, `kiln env add`, or trunk's `.kiln/environments.yaml` (the file
pipelines already read, [review.md](review.md)); the model is the same either way.

```yaml
production:
  provider: argocd/prod-argo          # or kilncd
  release_repo: { host: github, repo: acme/releases, branch: main, root: "" }
  targets:
    kubernetes: [prod-us, prod-eu]    # clusters, as the provider names them
    platforms:                        # KilnCD environments only (cd-targets.md §4)
      aws-lambda: { account: aws-prod, region: us-east-1 }   # an aws integration
  gates:
    after: [staging]                  # the version ran healthy there (txst's required environments)
    soak: 30m                         # …for at least this long
    approvals: { reviewers: [ada], count: 1 }   # agents answer only with agents.approve
    windows:
      - { deny: "0 0 * * 6,0", duration: 48h, timezone: UTC }
    checks: [no-snapshots, artifact-signed, { vulnerabilities: { max: high } }, owners-set]
    via: commit                       # or pull-request: the release commit is merged by a reviewer
  sync: { auto: true, prune: true, self_heal: true }
  watch: { timeout: 15m, settle: 30s, refused: 60s }
  concurrency: queue                  # a second release waits; or reject
  agents: { deploy: false, approve: false }   # what agents may do here (cd-targets.md D6)
  argocd: { applications: false }     # Argo CD: true, kiln writes each Application (no ApplicationSet)
  notify: { chat: ["#deploys"], webhooks: [] }
```

- An environment names one CD provider. Staging may run on KilnCD while production has its own
  Argo CD, and the same app moves between them.
- An environment on KilnCD may name targets of any kind; one on Argo CD names clusters registered
  in that Argo CD. Its apps run in its first cluster, on either provider.
- On Argo CD, one ApplicationSet makes the environment's Applications (§8.3); `argocd:
  { applications: true }` has kiln write each one instead, for an Argo CD that doesn't allow
  ApplicationSets. Its provider changes by a move once apps are registered there (§8.4).
- `via: pull-request` makes every release in the environment a pull request (a merge request on
  GitLab) on the release repository: the approval is the merge, made on the git host. Built on
  GitHub, GitLab and Bitbucket: a release is proposed from a branch of its own (`kiln/release-<id>-<asked at>`)
  once its gates pass, and waits in `committing`; merged, its merge is its commit (delivered, who
  merged it kept as its approval); closed unmerged, it's cancelled; cancelled in kiln, its pull
  or merge request is closed. Who may merge is the branch's protection on the host, so
  `approvals` isn't set beside it. kiln's own commits (registrations, reverts) still go to the
  branch.
- `agents` says what agents may do there: release (`deploy`: every kind of release, a cancel, a
  deregistration) and answer approvals (`approve`). Neither by default; elsewhere an agent's change
  is refused (`403`) and a person makes it. Only a person changes `agents`.

## 5. Registrations

`POST /api/deploy/apps/{app}/environments/{env}` (`kiln app register <app> --env <env>`):

1. **Check.** The template allows the environment's targets; the app's readiness checks pass
   (owners set, package exists, contacts, whatever the environment asks).
2. **Commit.** `<root>/<project>/<app>/<env>/release.json` with `enabled: false` and no version,
   and `values.yaml` from the template's defaults: one commit.
3. **Provider.** The provider gets ready to run the app from that path (§8): for KilnCD, an app
   definition; for Argo CD, the repository, project and ApplicationSet in place.

From then on:

- **Enable / disable** (txst's enable and disable) set `enabled` in `release.json`, each as a
  release. A disabled registration runs nothing: the provider removes what it deployed, or keeps it,
  as the registration says.
- **Pause / resume** turn the provider's automation off and on without changing git, for emergency
  hand edits (txst's set-autosync). The next release turns it back on.
- **Deregister** (`DELETE`) removes the registration's files (a commit) and the provider's app;
  objects are deleted unless `?keep=true`.

## 6. The release repository

```
<root>/
  <project>/<app>/app.yaml               # the app's package and defaults
  <project>/<app>/<env>/release.json     # what this environment runs: kiln writes it
  <project>/<app>/<env>/values.yaml      # this environment's values: config releases change it
```

`release.json`, the one file every provider reads:

```json
{
  "schema": 1,
  "project": "shop",
  "app": "web",
  "environment": "production",
  "release": 1042,
  "enabled": true,
  "version": "1.4.2",
  "package": { "kind": "helm", "repo": "oci://ghcr.io/acme/charts", "chart": "web", "version": "1.4.2" },
  "artifacts": {
    "image": { "repository": "ghcr.io/acme/web", "tag": "1.4.2", "digest": "sha256:9f2c…" }
  },
  "requested_by": "ada",
  "at": "2026-10-18T20:00:00Z",
  "rollback_of": null,
  "namespace": "web",
  "set": { "image.tag": "1.4.2" },
  "helm": true
}
```

- **One release, one commit.** The message says what changed (`deploy web 1.4.2 to production
  (release 1042)`), and trailers name the release and the people: `Kiln-Release: 1042`,
  `Kiln-Requested-By: ada`, `Kiln-Approved-By: …`.
- **Git says what runs.** A release the provider refused, or never got, and one cancelled while it
  was delivered, is reverted by a commit of kiln's (`Kiln-Reverts: 1042`) that puts its files back
  as they were, unless someone changed them since. A release that ran and then failed stays: roll
  back, or fix it forward. So does a commit kiln didn't make: it fails, and git keeps it.
- **Every provider reads it, the same.** What runs is the package at `version` (a chart's
  `chart_version` filled for the release), with what it `set`s (the package's `set`, filled from
  the release's version and artifacts; those filled stay strings), the registration's
  `values.yaml`, in `namespace`, rendered with Helm when `helm` says so (a Helm package, or a git
  path that holds a chart or sets something). KilnCD reads it at the release's commit; Argo CD's
  ApplicationSet makes each Application from it (§8.3). A hand edit to it is a release like any
  other, and both providers run it the same. A file from before kiln wrote `namespace`, `set`
  and `helm` is read as it was (the app's namespace, its package's `set` filled), and gets them
  with its next release.
- **JSON for kiln, YAML for people.** Argo CD's git files generator reads `release.json`'s keys as
  parameters. People edit `values.yaml` and `app.yaml` in pull requests like any file.
- **It starts with a commit.** A new release repository's first commit is kiln's README of what
  it holds (with a registration's trailer, so no scan takes it for someone's edit): Argo CD reads
  a repository only once it has a `HEAD`.
- **Safe writes.** kiln commits against the branch head it read and retries on a newer one, keeping
  everything the release didn't touch. Two releases of one registration never race: one is in
  flight at a time (§9).
- **Commits kiln didn't make are releases too.** A merged edit to `values.yaml`, or anything else
  under a registration's path, is seen (webhook or poll) and recorded as a release of origin `git`.
  Each environment looks at its own (environments share a release repository: `kiln/releases`
  by default). The records never disagree with git.

## 7. Git hosts

A git host is registered once (`POST /api/deploy/git-hosts`) and used for release repositories and
for apps' source repositories.

| | kiln | GitHub | GitLab | Bitbucket |
| --- | --- | --- | --- | --- |
| Read | its own repositories, in process | mirror (as today) | mirror | mirror |
| Write a release | commit in process | push with an App installation or fine-grained token | push with a project or group access token | push with an access token or app password |
| Change requests (`via: pull-request`) | kiln's own diffs (review.md) | pull requests | merge requests | pull requests |
| Report a release | kiln's UI | Deployments and deployment statuses | Environments and Deployments | commit build statuses |
| Notice commits | in process | webhooks, then polling | webhooks, then polling | webhooks, then polling |
| Create repositories | yes | in an organization, through the App; from template repositories | in a group, with its token; from project templates | yes |
| Pull request heads for CI | — | `refs/pull/<n>/head` (as today) | `refs/merge-requests/<iid>/head` | Data Center: `refs/pull-requests/<id>/from`; Cloud: read from the API |

- **kiln's own git server** is CI's native hosting (ci.md W14), one server for both: release
  repositories are native repositories (`kiln/releases` by default, or a project's own). kiln writes
  a release in process, with git's plumbing against a scratch index and a compare-and-swap of the
  branch, so a commit is never lost to one made beside it. Readers fetch over HTTPS through the
  hosting's endpoint: each Argo CD instance gets a read-only token for its environment's release
  repository, which reads that repository and nothing else. People change a release repository
  through reviewed change requests, and what lands is a release of origin `git`. Its repositories
  replicate with kiln's state once servers do (§14).
- **GitHub stays as it is**: discovery, `ls-remote` polling, webhook hints, mirrors, pull request
  heads, the Actions-compatible CI. A GitHub App joins the token kiln reads today, for writes,
  statuses and repository creation.
- **Release repositories on GitHub** (`release_repo: { host: github, repo: owner/name }`) go
  through kiln's GitHub App (`[ci.github]`, or the one its manifest flow made), with an
  installation token for each call that reaches that one repository with what the call needs:
  - **Made** in the owner's organization when it isn't there (private, empty: the App needs
    `administration: write` there); on a person's account, made by them and given to the App.
  - **Read** through a mirror of kiln's, fetched with a token that reads it (`contents: read`),
    once a minute, or at once when the App's webhook says it moved. KilnCD reads kiln's mirror;
    Argo CD reads GitHub with a token of its own, which kiln puts in place every half hour (one
    lasts an hour).
  - **Written** by a push of a commit made in kiln's mirror, with a lease on the head it was made
    on (`contents: write`): a commit someone pushed meanwhile makes it try again on theirs, never
    lose it.
  - **Reported**: each release is a deployment of its commit to `<environment>/<app>` (so one
    app's don't mark another's inactive), its status following the release (`in_progress`
    delivering, `success`, `failure`, `inactive` once cancelled), linking to the release on kiln.
  - **Webhooks**: the App's `push` and `pull_request` events (`POST /api/hooks/github/app`, signed
    with its secret) have a release repository looked at, and a release's pull request followed,
    at once.
- **Release repositories on a GitLab** (`release_repo: { host: gitlab, repo: group/…/name }`) go
  through the `[[ci.gitlab]]` whose token covers the project's group, as the GitHub ones do, with
  GitLab's words: made in its group when it isn't there (private, empty); read through kiln's
  mirror and written by a push with a lease, with that token; releases as merge requests
  (`via: pull-request`); each release a deployment of its commit to `<environment>/<app>`
  (`running`, `success`, `failed`, `canceled`); its `Push Hook` and `Merge Request Hook`
  (`POST /api/hooks/gitlab`, with the webhook's token) moving it at once. Argo CD reads it with a
  deploy token of the project (`read_repository`, made by kiln: it takes a Maintainer), made
  again, the one before revoked, only when Argo CD can't read with it. `GET /api/deploy/git-hosts`
  says which hosts a release repository may be on.
- **Release repositories on Bitbucket** (`release_repo: { host: bitbucket, repo: workspace/name }`)
  go through `[ci.bitbucket]`'s access token (or user and app password) where it covers the
  workspace, as the GitLab ones do, with Bitbucket's words: made in its workspace when it isn't
  there (private, empty); read through kiln's mirror and written by a push with a lease; releases
  as pull requests (`via: pull-request`; declined unmerged, cancelled; cancelled in kiln, declined
  there with a comment); each release a build status of its commit, `deploy/<environment>/<app>`
  (`INPROGRESS`, `SUCCESSFUL`, `FAILED`, `STOPPED`: Bitbucket has no deployments API outside
  Pipelines), linking to the release; its `repo:push` and `pullrequest:*` webhooks
  (`POST /api/hooks/bitbucket`, signed with the webhook's secret) moving it at once. Argo CD reads
  it over SSH with a deploy key kiln makes for that repository (Bitbucket's API makes no token
  that only reads; it takes the repository's admin), made again, the one before revoked, only
  when Argo CD can't read with it; Argo CD checks Bitbucket's host key against the ones it knows
  (bitbucket.org's are in its defaults), at `[ci.bitbucket] ssh_url` (`ssh://git@bitbucket.org`).
- **GitLab and Bitbucket** get the same: discovery, webhooks, polling, mirrors, change-request heads
  and statuses for CI (KilnCI's runs of a project there are commit statuses on it, `kiln/<workflow>`,
  following the run), and everything above for release repositories. GitLab.com and self-managed
  GitLab; Bitbucket Cloud and Data Center.

Inside kiln, one module (`crate::githost`, shared with CI's providers, ci.md W23) covers all four:
read (a mirror URL and credentials), commit (files, a message and trailers, against an expected
head), push a commit made here, open a change and follow its state, report a status or a
deployment, read a push webhook, and create a repository from a template.

## 8. CD providers

### 8.1 The contract

| Call | What it does |
| --- | --- |
| `register(app, env)` / `deregister(app, env, keep)` | ready to run the app from its path; or not, keeping or deleting what it deployed |
| `deliver(release, commit)` | the commit is in git: refresh, and sync when the environment syncs on demand |
| `status(app, env)` / `watch(app, env)` | per target: the commit and release it runs, synced, health, the operation under way, a message |
| `diff(app, env, release)` / `manifests(app, env, release)` | what a release would change, and what it renders to, before it lands |
| `resources`, `logs`, `events` | what runs, as the provider sees it |
| `action(app, env, object, action)` | restart, scale, suspend, resume, delete a pod: what the provider offers (Argo CD's resource actions; on KilnCD, kiln's: restart a workload, pause and resume a Deployment's rollout, scale, suspend and resume a CronJob or run it now, delete a pod) |
| `automation(app, env, on)` | pause or resume the provider's automation (§5) |

**When a release is done.** The provider reports, on every target of the environment, the release's
commit, synced and healthy, steady for `watch.settle` (default 30 s). **When it failed**, on
evidence rather than on the clock alone: a sync failed after its retries; or it was synced and its
health stayed Degraded for `watch.degraded` (default 60 s): Kubernetes' progress deadline passed (no
new pod became ready for `progressDeadlineSeconds`), pods can't start; or it didn't land within
`watch.timeout`, or within three times that while its provider says it's still progressing (a slow
rollout isn't a failed one). A provider that can't be reached leaves the release `delivering`,
retried with backoff until the timeout. **When it was refused**: the provider can't make anything of it (Argo CD
answers `4xx` to its Application, or reports a `ComparisonError`; KilnCD can't read its sources or
render them) for `watch.refused` (default 60 s, room for a passing trouble to clear).

**Before its commit**, each release is checked for what can be known to fail, and refused while
nothing has changed: its secrets' pinned versions still have their values; every image it runs is
in its registry (its artifacts, and on KilnCD each container image it renders); and on KilnCD it
renders as KilnCD will render it (the values it commits given inline) and its cluster takes each
object in a server-side dry run (invalid fields, an admission policy's no, a quota, a field that
can't change). What kiln can't ask (a registry or cluster it can't reach, a namespace or a kind
the sync makes first, a hook) lets it through: the provider has the last word. A rollback refused
leaves what it rolled back from running, and marks the registration degraded.

**What follows a failure** is decided on what's known, so that kiln never makes things worse:

| What happened | What kiln does |
| --- | --- |
| Refused, or it never reached the provider: nothing of it was applied | its commit is reverted (bookkeeping: what ran before runs on) |
| A deploy, config, secrets, enable or disable release failed after it was applied | **a revert**: its commit reverted as a release of kind `revert`, origin `kiln`, followed like any release until what ran before runs again. Where nothing ran before (an app's first release, or one after a disable that removed what ran), the revert removes what it made; a disable that kept what ran keeps it. The app's namespace stays, as on a disable |
| A rollback failed | **held**: not reverted, as that would bring back what it rolled back from |
| A revert failed | **held**: kiln doesn't revert its own way back |
| One asked with `no_revert` failed (a migration that can't run backwards; a secret rotated after a leak, which a revert would deliver again) | **held** |
| One that runs a Job on its way in (a Helm `pre-install`/`pre-upgrade`/`post-*` hook, Argo CD's `PreSync`/`Sync`/`PostSync`: a migration) failed, as its preflight found it | **held**: what ran before may not run on what it changed; a hook marked `kiln.dev/reversible: "true"` (kiln-base: `jobs.<name>.reversible`, an expand-then-contract change) doesn't count |
| A commit kiln didn't make (origin `git`) failed | **held**: git is its author's |
| The provider took it and can't be asked since | nothing is done blind: it's judged when the provider answers, and **held** once it hasn't for `watch.unreachable` (default 6 h). On KilnCD, that's while the app's cluster doesn't answer (asked before anything is judged, a landing too, since KilnCD's last look may be all it has); a sync that failed for that is synced again once the cluster answers. The release's `watch.timeout` runs from when the provider answers again |

A release **held** fails as it is, and its registration is marked `degraded` (why, on the board and
the app's page): it needs a person, and nothing is promoted into it by itself until the next release
that lands there clears it. A rollback refused leaves what it rolled back from running, so it marks
the registration degraded too. Kiln never retries a release by itself.

### 8.2 KilnCD

- **Register**: one KilnCD app per target (`<project>/<app>-<env>`), of origin `release`. Its sources
  are the package from `app.yaml` (a chart, a path, or a platform definition) and the release
  repository (`as: release`). `${{ release.version }}`, `${{ release.artifacts.<name> }}` and
  `${{ release.id }}` resolve from `release.json` at the commit; values are `app.yaml`'s defaults
  under `<env>/values.yaml`.
- **Deliver**: the commit is a new revision, and KilnCD syncs it: gates ran before the commit, so a
  release needs no second approval. An app with a rollout (canary, blue/green, checks; cd-targets.md
  §5) rolls out that way.
- **Status**: KilnCD's sync record names the commit, every object it applies carries
  `kiln.dev/release`, and health comes from the engine. In process: no polling.
- **Platforms**: every target KilnCD supports. AWS Lambda runs end to end today; Cloudflare Workers,
  Vercel and ECS come next, then the rest of cd-targets.md §3, each under this same release flow.

### 8.3 Argo CD

- **Connect**: an Argo CD instance is an integration of kind `argocd` (Settings → Integrations):
  its URL, an account's token (an API key, or an AppProject role's token), a CA when its
  certificate isn't public, the namespace its Applications live in, and `kiln_url`, the address
  it reaches kiln at when that isn't kiln's public URL (an Argo CD in another cluster reads the
  release repository there). Verify checks its version (`GET /api/version`) and its rights
  (`GET /api/v1/account/can-i/…`: repositories, kiln's projects and ApplicationSets, syncing their
  Applications) and says what's missing.
- **Register**: through Argo CD's API, kiln keeps in place, before an environment says it's there
  and again with each release:
  - the release repository (`POST /api/v1/repositories?upsert=true`), as
    `<kiln_url>/git/<owner>/<name>.git`, read with a token kiln makes for it that reads that
    repository and nothing else (`git.read` on it: no push, no API). It lasts 90 days and is
    replaced a month before it ends, or as soon as Argo CD can't read with it; the one it replaces
    is revoked. Argo CD tries it as it's put: one it can't read refuses the environment, saying
    where it looked;
  - the environment's AppProject, `kiln-<env>` (`POST /api/v1/projects`, upserted): the sources its
    apps come from (each app's chart or git repository, and the release repository), where they
    run (the environment's cluster, each app's namespace), and the cluster-scoped kinds they make:
    a Namespace (for `CreateNamespace`), and those its apps' templates allow (`cluster_kinds`).
    One per environment, as its ApplicationSet names it: Argo CD's API takes no ApplicationSet
    whose project is templated;
  - one ApplicationSet per environment, `kiln-<env>` (`POST /api/v1/applicationsets?upsert=true`),
    whose git files generator reads `<root>/*/*/<env>/release.json` and makes one Application per
    registration, `<env>-<app>`, in that AppProject: the package at the release's version, its
    values from the release repository (a second source, `ref: release`, read as
    `$release/<root>/<project>/<app>/<env>/values.yaml`), what the release sets and
    `kiln.release=<id>` as Helm parameters, the environment's sync policy and `CreateNamespace`.
    Its template patch builds the sources from `release.json`'s keys (Go templates), so both
    providers run the same file (§6). Registrations disabled, without a version yet, or of a
    platform are left out. It creates and updates Applications and never deletes one
    (`applicationsSync: create-update`, `preserveResourcesOnDeletion`): kiln deletes an
    Application once the ApplicationSet no longer makes it, with what it runs unless that's to be
    kept. A pause changes an Application's sync policy, which the ApplicationSet leaves to kiln
    (`ignoreApplicationDifferences`).

  Where ApplicationSets aren't allowed (`argocd: { applications: true }`), kiln writes each
  Application itself (`POST /api/v1/applications?upsert=true`), the same spec with the values
  inline, in the `default` project.
- **Deliver**: the ApplicationSet is put again with its refresh annotation, so it reads the
  release repository at once; once the release's Application is made from its `release.json` (its
  `kiln.dev/release`), it is refreshed hard, once. A release turns a paused Application's
  automation back on (§5).
- **Status**: kiln follows the Applications it made through Argo CD's stream of them
  (`GET /api/v1/stream/applications?selector=app.kubernetes.io/managed-by=kiln`), one stream per
  Argo CD: each change wakes the release in flight for its registration, and Argo CD is asked
  directly only for what the stream doesn't hold (it is down, or just started). A release has
  landed when Argo CD compared its spec (its `kiln.release`, or its version for plain manifests)
  with the release repository at a commit that holds the release's files (its `release.json` and
  `values.yaml` as the release's commit has them), and reports it Synced and Healthy, steady for
  `watch.settle`: txst's check, with the commit added.
- **Disable, deregister**: the files change first (the release, or the deregistration's commit);
  once the ApplicationSet has read them (its refresh done), kiln deletes the Application, keeping
  what it runs when asked: deleted before, it would be made again.
- **Diff, manifests, resources, logs**: `/managed-resources`, `/manifests` (another version
  previewed with `?revisions=<version>&sourcePositions=1`), the Application's resources, `/logs`.
- **Actions**: `GET /api/v1/applications/{name}/resource/actions` and `POST
  …/resource/actions/v2` (with parameters: a scale's replicas): Argo CD's built-in actions and any
  it was given; and a pod deleted (`DELETE …/resource`) for its workload to make again.
- **Pause**: the Application's automated sync off; the next release turns it back on.
- **Adopting**: an Application named as the ApplicationSet names it (`<env>-<app>`), one kiln
  wrote itself before the environment had an ApplicationSet among them, is taken over by the
  ApplicationSet with what it runs.

### 8.4 Changing an environment's provider

`POST /api/deploy/environments/{env}/move` with `{to, handover, force}` (`kiln deploy env move
<env> --to <provider> [--handover [--force]]`, the environment's page, or
`deploy.environments.move`). The release repository stays as it is: both providers read the same
`release.json` (§6).

1. **Shadow.** The new provider gets every app that runs there, and compares, never writing:
   KilnCD apps with their automation off, which know the objects Argo CD deployed by Argo CD's own
   tracking, under the name the Application gave them (its tracking method read from Argo CD's
   settings), and take them over as Argo CD's field manager; on Argo CD the environment's
   ApplicationSet, its Applications' automation off (or Applications kiln writes, likewise). The
   next release on KilnCD marks the objects as kiln's own.
2. **Compare.** For each app, where the new provider would change what runs beyond each
   provider's own tracking marks (`kiln.dev/*`, Argo CD's tracking id and instance label): the
   objects and their diffs (`GET …/move`). A move that agrees everywhere hands over when asked
   (`handover`); one that doesn't, only when told to anyway (`force`). Before it hands over, a move
   can be cancelled (`DELETE …/move`): the shadows go, and what runs, which they never touched,
   stays with the old provider.
3. **Hand over**, app by app: the old provider's automation off, the new one's on; it syncs,
   taking each object over by its tracking (KilnCD takes objects over from Argo CD's tracking,
   cd.md; Argo CD adopts objects KilnCD deployed through its own), and kiln waits until it runs
   the app Synced and Healthy. Then the old provider lets go, keeping what runs: on Argo CD, each
   Application's resources finalizer taken off, the ApplicationSet deleted, each Application
   deleted without its objects; on KilnCD, each app removed, its objects orphaned. The environment
   names its new provider.
4. **Nothing made again.** Each object's uid is read as the handover begins and after it: one whose
   uid changed, or that's gone, is named as made again.

A move goes on in the background, kept by the server and taken up again after a restart; a
handover stopped part-way (a provider away) is finished by asking again, and goes only forward.
While an environment moves, its releases, registrations and pauses wait (`409`). Once apps are
registered in an environment, its provider changes only by a move. Its clusters must carry the
same names in both providers.

## 9. Releases

`POST /api/deploy/releases` with `{ app, environment, version | values | rollback | enabled | promote_from }`:

1. **Validate.** The registration exists; the caller may release this app here (§15); the version
   and its artifacts exist in their registries: a chart's tag in its OCI registry or its Helm
   repository's index, a branch or tag of a git package. A registry that says the version isn't
   there refuses the release (`422`, with the newest versions it has); one kiln can't ask lets it
   through, and the provider has the last word.
2. **Concurrency.** One release in flight per registration. Another waits in line or is refused
   (`409`), as the environment says. An `Idempotency-Key` (header, or `idempotency_key`) returns the
   same release to a retried call, whatever it has become; a retried cancel finds it cancelled.
3. **Gates** (§9.1). A release waiting on one is `waiting`, and says what for.
4. **Record.** The release, with every gate's result, is written to KilnDB (`committing`).
5. **Commit.** `release.json` (and `values.yaml` for a config release) on the environment's release
   repository; or a pull request, with `via: pull-request`.
6. **Deliver.** The provider is told (`delivering`); the host gets a deployment status.
7. **Watch.** A work item on the bus follows the provider until the release is done, failed or timed
   out (§8.1), across restarts and between servers. Progress streams to every client.
8. **Finish.** `succeeded` or `failed`; the host's status and the release messages follow. Where the
   next environment promotes automatically, its release starts after the soak.

```
 waiting ──▶ committing ──▶ delivering ──▶ succeeded ──▶ rolled_back
    │             │              │      └─▶ failed           superseded
    └─────────────┴──────────────┴──▶ cancelled
```

**Kinds.** A *deploy* changes the version; a *config* release changes `values.yaml` (txst's
deploy-config); a *rollback* goes back to an earlier release (§10); *enable* and *disable* flip
`enabled`; a *promotion* copies an upstream release's version and artifacts.

**Cancel** stops a release that is waiting or delivering. Once committed, git already says the new
version: cancelling a delivering release reverts its commit (§6), and the provider is handed what
git says then.

### 9.1 Gates

| Gate | Passes when |
| --- | --- |
| `after` | the same version, with the same artifacts, succeeded in each upstream environment and still runs there healthy (txst's deploy-validate); one of a linked server (`staging@eu`) says so in a statement it signs (§11) |
| `soak` | it has run there that long |
| `approvals` | enough reviewers approved, from the UI, a phone, chat or `kiln`; agents only where the environment allows them (cd-targets.md D6). With `via: pull-request`, the merge is the approval |
| `windows` | no deny window covers now (Argo CD's sync windows: allow and deny, cron, duration, timezone) |
| `checks` | each check passes: built in (`no-snapshots`, `artifact-exists`, `artifact-signed`, `vulnerabilities`, `owners-set`) or external (a URL that answers pass or fail, a kiln workflow). A check is `required` or `report-only` |
| `provenance` | each artifact a new version deploys has kiln's provenance, its signature verified, from the workflows and branches the environment allows (`require_provenance: { builder, workflows, branches }`, ci.md W18-R04) |
| `freeze` | no freeze holds the environment or the app (incidents, holidays) |

Each gate's result is recorded on the release, with its evidence. A release kiln can't gate is
refused, never let through. In order: a freeze, windows, the built-in checks, provenance, `after`
and `soak`, the external checks, approvals.

How each one holds or refuses (built: all but the checks `artifact-exists`, `artifact-signed` and
`vulnerabilities`):

- **Refused at once** (`409`): a version that doesn't run upstream (`after`), or runs there with
  other artifacts; a failed check; a window that doesn't read; provenance that doesn't meet the
  requirement, an artifact not pinned by digest, or none named; a linked server's statement that
  doesn't verify, or a linked server that says the app isn't there or doesn't take the token.
  Nothing waiting would change them.
- **Held** (`waiting`, the reason in its message): a freeze, a deny window (until it closes) or no
  allow window open (until one opens), a soak still running, the same version still on its way
  upstream (an upstream release of it in flight), an external check not answered yet (its URL
  pending or out of reach, its workflow still running), provenance kiln doesn't have yet (for half
  an hour after the release is asked for: its build may still be attesting it; then it's
  refused), a linked server that can't be asked.
- **External checks** are asked once the release is recorded, and their answers stay with it: a
  URL that said pass or fail isn't asked again, a workflow runs once (a gate before them that
  holds the release doesn't make them ask again). `{ name, http: { url, timeout } }` is asked
  `POST <url>` with `{ check, server, release: { id, app, environment, kind, version, artifacts,
  requested_by, promoted_from, url } }` and kiln's OIDC token for that URL (`Authorization:
  Bearer`, the URL its audience), and answers 2xx `{ "result": "pass" | "fail" | "pending",
  "detail"?, "retry_after"? }`; it's reached as webhooks are, private networks only where
  `[ci] allow_private_targets` allows them. `{ name, workflow: { repository, workflow, ref } }`
  runs one of kiln's workflows by `workflow_dispatch` at that ref (the repository's default branch
  when none), given what it declares of `app`, `environment`, `version`, `release` and `kind`, and
  passes when the run succeeds; the run is its evidence. `report_only: true` records a check's
  answer and never holds or refuses for it. A template's checks join its apps' environments'.
- **Held for people** (`waiting_approval`; the event says `needs: approval` and who): too few
  approvals. `POST /releases/{id}/approve` (`runs.approve`, a note optional) counts one per
  person, a token or agent counting as the person it acts for; `…/reject` cancels the release,
  with who and why. Whoever asked doesn't approve their own release unless `self_approve`;
  reviewers, when named, are the only ones who may; agents answer only with `agents.approve`.
- `after` and `soak` hold a new version; a rollback, a config change or an enable runs what passed
  them before, and only freezes, windows, checks and approvals hold it. Provenance is asked of a
  new version and of what a rollback puts back.
- Changing an environment wakes what waits on its gates, so lifting a freeze lets releases go at
  once; otherwise each looks again when its gate says, at least every five minutes.

### 9.2 Promotion

`promote_from: staging` releases exactly what staging runs: the same version, the same digests,
recorded as `promoted_from` (built: `kiln deploy web --to production --from staging`). An
environment with `promote: auto` does this by itself once `after` and `soak` pass, and asks for
approvals as usual. Built: when a deploy lands in an environment, each environment that
promotes automatically with it among its `after`, and where the app is registered, gets that
release by itself, through its own gates (it waits out its soak, windows and freezes, and its
approvals). It is asked for in the name of who asked for the release that landed, so they
don't approve their own change on its way, and once per landing and environment (its key,
`promote:<release>:<environment>`). One refused (a release in flight there, or an upstream
that doesn't run the version yet) goes when the next of its `after` lands, or by hand. Config
releases and rollbacks never move on by themselves.

## 10. Rollback, versions and history

- **Rollback**: `POST /api/deploy/releases/{id}/rollback`, or a release with `rollback: { to }`.
  The candidates are the last 10 succeeded releases other than the one running. The release rolled
  away from becomes `rolled_back` and is no longer a candidate (as in txst). A rollback is a release:
  gates apply as the environment says (by default it skips `after` and `soak`, and keeps approvals).
- **Switching back instantly**: an app rolling out blue/green on KilnCD switches back to blue at once
  while blue is kept (cd-targets.md §5). That is still a release and a commit, so git says what runs.
- **Versions**: semver whose prerelease marks a snapshot (txst's rule: `1.2.3-rc1` is a snapshot,
  `1.2.3+build` is not), or any string. Artifacts are pinned by digest, so a version always deploys
  the same bytes.
- **History**: releases by app, environment, time, state and person; the git log of each path;
  what runs where across environments; the difference between two releases: version, artifacts,
  values, and rendered objects through the provider.
- **Delivery metrics** from the records: how often each app deploys, lead time (commit to running
  in production), change failure rate (failed or rolled back) and time to restore.

## 11. Linked kiln servers

txst registers each app in each of its instances separately. kiln links servers instead: apps,
owners, templates and registrations replicate (KilnDB), each environment belongs to one server
(the one holding its provider's connection), and a release is made on the server its environment
belongs to. A gate result that crosses servers (staging's run proving production's `after`) is
signed with kiln's OIDC issuer (ci.md W10), as txst signs its deployment tokens.

Built, before the records replicate (P6): a linked server is an integration of kind `kiln` (its
URL, a token of an account there that reads releases, `runs.read`; optionally the CA its
certificate is signed by and the issuer to expect), checked by asking it where it is
(`GET /api/deploy/link`) and reading its issuer's keys. An environment's `after` names an
environment of one as `staging@eu`. To gate a release, this server asks `eu` what runs of the app
in `staging` and what's on its way there (`GET /api/deploy/apps/{app}/environments/staging/statement?audience=<this
server's issuer>`); `eu` answers with a JWT its OIDC issuer signs (RS256, the key it publishes;
`sub` `deploy:staging:<app>`, this server's issuer the audience, five minutes), and this server
takes it only once the signature verifies against the keys `eu`'s issuer publishes and it is
from that issuer, for this server, and current. The gate then holds or refuses as for an
environment of its own, and keeps the statement with its result: anyone can check it again with
`eu`'s keys. A server that can't be asked holds the release; one that refuses the token, doesn't
have the app there, or answers with a statement that doesn't verify refuses it. A server serves its
issuer's keys while it makes releases. `promote: auto` follows environments of its own server.

## 12. Clients

### 12.1 API

| Resource | Calls |
| --- | --- |
| `/api/deploy/git-hosts` | list, add, test, update, remove |
| `/api/deploy/providers` | list, add (Argo CD connections; KilnCD is always there), test, update, remove |
| `/api/deploy/templates` | list, add, update, remove |
| `/api/deploy/environments` | list, add, show, update, remove, `move` (§8.4) |
| `/api/deploy/apps` | list (filters: owner, template, name), create, show, update, archive, delete |
| `/api/deploy/apps/{app}/environments/{env}` | register, show (status, history, candidates), update, deregister, `pause`, `resume`, `enable`, `disable`, `diff`, `manifests`, `resources`, `logs`, `actions`, `statement` (what runs there, signed for a linked server, §11) |
| `/api/deploy/link` | where this server is and the issuer it signs with, for linked servers (§11) |
| `/api/deploy/releases` | create, list (filters: app, environment, state, kind, person, time), show, `approve`, `reject`, `cancel`, `rollback` |
| `/api/deploy/events` | a stream (SSE or WebSocket) of releases and their progress, filtered |

Every call is in kiln's OpenAPI document with the capability it needs (§15).

### 12.2 `kiln`

```sh
kiln app create web --owner shop --template service   # its template makes acme/web; --source links one
kiln app register web --env staging
kiln deploy web 1.4.2 --to staging          # follows the release to the end
kiln deploy web --to production --from staging
kiln deploy web --to production --values values-hotfix.yaml
kiln deploy status web --env production
kiln deploy history web --env production
kiln deploy rollback web --env production [--to 1040]
kiln deploy approve 1042 [--note …]          # or reject: cancelled, with why
kiln deploy env add production --provider argocd/prod-argo --repo github:acme/releases
kiln deploy env move staging --to kilncd --handover   # compared in shadow, then handed over
kiln deploy action web --env production Deployment/web restart
```

Environments are `kiln deploy env …`: `kiln env` is CI's (a repository's environments, ci.md W15)
until the two become one.

### 12.3 UI, chat and MCP

- **UI**: Apps (the list, an app's page with environments side by side), the environments board
  (what runs where, what waits to move), a release's page (its gates, commit and live progress),
  the deploy dialog, the rollback picker, an approvals inbox that works on a phone.
- **Chat**: Slack first, then Discord, Telegram and Teams (`crates/kiln/src/ci/chat`): a message
  per release that changes as it moves (txst's deployment messages), and commands for the calls
  above.
  - *A release's message* goes to the app's own channels (`contacts.channels`: `kind:target` as
    ci.md W19-R06 names channels, a bare `#channel` being Slack's) and through the server's
    notification rules that take its events (`[[ci.notify]]`, `where` reading `release`: its
    `app`, `environment`, `kind`, `version`, `state`, `requested_by`, `approvers`): `release.started`
    as it's asked for, `release.waiting_approval` (naming who may approve), `release.succeeded`,
    `release.failed`, `release.cancelled`. Where a channel can change a message afterwards, the
    release has one message there, posted the first time and edited as it moves on (what holds
    it, committing, delivering, landed, failed, cancelled, rolled back, replaced), saying who
    approved or rejected it: a Slack bot's (`chat.update`), a Discord webhook's (its message
    patched), a Telegram bot's (`editMessageText`). While it waits for approval it carries Approve
    and Reject where kiln takes that chat's buttons: a Slack app whose signing secret kiln has, a
    Telegram bot whose webhook is kiln's. A channel that can't change a message (a Slack incoming
    webhook, Teams' Workflows, PagerDuty, Opsgenie, a webhook) gets one per event, once per
    release, event and channel, and an app's own such channels aren't told each release starts.
  - *Commands*: `deploy <app> <version> to <environment>`, `approve <release> [why]`, `reject
    <release> [why]`, `rollback <app> in <environment> [to <release>]`, `status <app> [in
    <environment>]` or `status <release>`, `link <code>`, `help`: a Slack slash command
    (`/kiln …`), button or @-mention, Discord's `/kiln`, a Telegram bot's commands (`/approve 42`)
    and buttons, an @-mention of a Teams outgoing webhook. Each reaches its integration's own
    address (`/api/ci/chat/<kind>/<integration>/<key>`, the key from the server's master key;
    `GET /api/ci/integrations/<name>/chat` shows it and what that chat needs set; a Slack app's
    events, interactivity and slash command share it), checked as its chat signs it: a Slack
    app's signing secret (`X-Slack-Signature`, at most five minutes old; the integration's
    `signing_secret`), a Discord application's Ed25519 key (`public_key`), a Telegram bot's secret
    token (`POST /api/ci/integrations/<name>/chat` points the bot's webhook at kiln with it), a
    Teams outgoing webhook's HMAC (its security token as `signing_secret`). Each runs as its
    person's kiln user, through the operation it is (`deploy.releases.create`, `.approve`,
    `.reject`, `.rollback`, `deploy.apps.get`, `deploy.registrations.get`,
    `deploy.releases.get`): judged by the route table, held by the environment's gates and agents
    policy, and audited, as that route is. Its answer is its person's alone where the chat allows
    (Slack, Discord, Telegram's buttons): at once when it's quick, else as soon as it's done.
  - *Whose account*: the kiln user who linked it, with a code from Settings → You → Chat accounts
    (`POST /api/me/chat/codes`: a signed-in person's, never a token's, since the account then acts
    with all their grants) said to kiln there within ten minutes (`link <code>`), once; or, in
    Slack, the one person here with the email Slack has for it (`users.info`; none, or more than
    one, is nobody). `GET /api/me/chat` lists one's own, `DELETE /api/me/chat/<integration>/
    <account>` unlinks one. A bot's account (another app's, an agent's; in Slack, an @-mention by
    a bot) acts as an agent for the person who linked it (`slack:<bot> for user:<them>`), so the
    environment's agents policy holds it (cd-targets.md D6).
- **MCP**: one operation per call, in kiln's registry of operations (ci.md W20): MCP tools
  (`deploy_releases_create`) and code mode's `kiln.deploy.*`. Each is one of `/api/deploy`'s routes,
  judged by the route table as that route is, and audited as it is:

  | Operation | |
  | --- | --- |
  | `deploy.environments.list`, `.get`, `.put` | environments |
  | `deploy.apps.list`, `.get`, `.put` | apps |
  | `deploy.registrations.get`, `.create`, `.delete`, `.pause`, `.resume`, `.candidates` | registrations: what runs, register, deregister, the provider's automation, what a rollback can go to |
  | `deploy.registrations.actions`, `.action` | what can be done to an object an app runs, and doing it (agents only where they may deploy) |
  | `deploy.environments.move`, `.move_status`, `.move_cancel` | an environment moved to another provider (a person's to start) |
  | `deploy.releases.list`, `.get`, `.create`, `.rollback`, `.cancel` | releases; `create` and `rollback` take an `idempotency_key` |
  | `deploy.releases.wait` | until a release is done, or moves at all (`until: any`), for up to 600 s: no polling |

  Every release moving is the event `{"type": "deploy", id, app, environment, kind, state, by,
  release}`. Agents release only where the environment lets them (§4), with skills for a first
  deploy, a promotion and a rollback to come.

## 13. What runs where, as one page

Every app's version in every environment, its health, what's waiting at a gate and why, and the
last release that failed: computed from the records and the providers' streams, never by polling
each provider per page view.

## 14. Records, work and events

### 14.1 Records: KilnDB

| Table | Holds |
| --- | --- |
| `git_hosts` | kind, URL, credentials (sealed) |
| `cd_providers` | kind, URL, credentials (sealed), namespace, version seen |
| `templates` | the template's data |
| `environments` | provider, release repository, targets, gates, sync, watch, concurrency, notify |
| `apps` | name, project, owners, template, properties, source repository, registries, contacts |
| `registrations` | app, environment, path, enabled, paused, channels, the release running |
| `releases` | number, registration, kind, version, artifacts, values digest, state, origin, commit, requested by, gate results, approvals, provider status, times, `rollback_of`, `rolled_back_by` |
| `approvals` | release, person or agent, decision, when |

kiln reaches these through one interface. KilnDB is being built by its own session; until its
interface is settled, the same interface runs on kiln's current store, so nothing above it changes
when KilnDB takes over.

### 14.2 Work and events: Focal's bus

| Stream | Class | Carries |
| --- | --- | --- |
| `deploy.releases` | persisted | every transition of every release (the record of what happened, in order) |
| `deploy.watch` | persisted work queue | one item per release delivering, leased by the server that follows it, renewed while it does, taken over by another when the lease lapses |
| `deploy.git` | persisted | commits seen on release repositories, from webhooks and polls |
| `deploy.timers` | persisted | what must happen later: a soak ends, a window opens, a timeout passes |
| `deploy.progress` | ephemeral | progress for the UI, `kiln`, chat and MCP |

Delivery is at least once, so every handler is idempotent by release. Focal's bus is being built
by its own session; until it is embeddable, kiln's event hub and a work table in the store stand in,
behind the same interface.

## 15. Security, permissions and audit

kiln's existing capabilities (authz, W10) cover these, as they do KilnCD's routes:

| Capability | Lets |
| --- | --- |
| `runs.read` | see apps, environments and releases |
| `environments.deploy` | release an app in an environment: deploy, config, rollback, enable, disable, cancel |
| `runs.approve` | approve releases in an environment |
| `environments.manage` | create and change apps, environments and registrations |
| `integrations.manage` | git hosts and providers, and their credentials |
| `users.manage` | kiln's groups and their members (`/api/groups`), and making an app for any owner team |

- Ownership is checked on every release: a release (deploy, config, rollback, enable, disable,
  cancel) or an action on what runs (a restart, a scale) is a deployer's in that environment
  (`environments.deploy` on `deploy:<env>`, or on the server: every app there), or the app's owner
  teams' where the environment lets owners release (`owners: { release: true }`, txst's rule),
  each person as far as their token's own grant would deploy there. A fleet change is a
  deployer's in its environment. The door (`/api/deploy/releases`, its cancels and rollbacks,
  actions) refuses at once anyone who neither deploys anywhere nor is in any group.
- Credentials are sealed in kiln's secret store and never returned by the API. Each one gets the
  least it needs: a release repository's write token writes that repository only; an Argo CD token
  is scoped to its projects; a reader's token on kiln's git server reads one repository.
- Every call is audited with its principal and the person it acted for; commits carry trailers;
  hosts show statuses linking back to the release.

## 16. Failures and restarts

| What happens | What kiln does |
| --- | --- |
| The commit landed, the record didn't (a crash between them) | `release.json` carries the release number: kiln finds the commit and completes the record |
| Someone pushed in between | the commit is redone on the new head, keeping their changes |
| The provider can't be reached (on KilnCD: the app's cluster doesn't answer) | the release stays `delivering`, retried with backoff; one the provider never took fails at its timeout and is reverted; one it took is judged when the provider answers (its timeout running from then), held once it hasn't for `watch.unreachable` |
| A release's sync fails, or it doesn't land within `watch.timeout` (Degraded, pods that never start) | it fails, and a revert release puts back what ran before, followed until it runs again; a rollback, a revert, or one asked `no_revert` is held instead and its registration marked degraded (§8.1) |
| The provider refuses the release (its chart doesn't render, its spec is invalid) | after `watch.refused` the release fails, its commit is reverted and the provider handed what ran before |
| A version that isn't in its registry | refused before it is recorded (`422`), with the newest versions there |
| The server restarts mid-release | the watch item's lease lapses and a server takes it again |
| The release repository can't be reached | `committing` retries until the timeout, then the release fails, nothing changed |
| A provider deploys a commit kiln didn't make | it is recorded as a release of origin `git` |

## 17. Parity with txst

| txst | kiln |
| --- | --- |
| `/application register`, name validation, app types, owners (owner, co-owner, ops owner) | create an app (§3.1), templates (§3.2), owner teams; kiln also creates the repository and registries |
| `environment register` / `deregister` | registrations (§5) |
| `info`, `list`, properties, `set-owner`, `change-app-type`, enable / disable | app info and maintenance (§3.3), enable / disable (§5) |
| `deploy` | a release (§9) |
| `deploy-validate` and deployment tokens | the `after` gate (§9.1); signed gate results between servers (§11) |
| `deploy-rollback` (last 10, the rolled-back one excluded) | rollback (§10), the same rules |
| `deploy-cancel`, `deploy-status`, `deployment history` | cancel (§9), status, history (§10) |
| `deploy-config` (values overrides) | a config release (§9) |
| `manifest`, `diff` | manifests and diff, on either provider (§8.1) |
| `get-autosync` / `set-autosync` | pause and resume (§5) |
| `set-channel`, `get-channels`, live deployment messages | channels per registration, release messages (§12.3) |
| in progress, last successful, last by version, last N, deployed versions, Argo CD URL | release queries (§12.1) |
| validations: no snapshots, approvals, external evidence (Fianu), security policy, vulnerabilities, ownership | checks (§9.1) |
| `check-permissions`, ownership-based permissions, identity-provider groups | capabilities and owner teams (§15); `can-i` for any user |
| CLI: device login, contexts, application and deployment commands | `kiln login`, `kiln app`, `kiln env`, `kiln deploy` (§12.2) |
| MCP tools (application, environment, deployment, user groups) | MCP operations (§12.3) |
| base library chart, build utils (image build, chart scaffold, package, publish) | kiln's base chart and `kiln app init` (§3.4), `.kiln/ci.yaml` templates |
| snapshots and releases by semver prerelease | versions (§10) |
| namespaces per app type, Argo CD projects per type | templates (§3.2), AppProjects (§8.3) |
| emergency operations: Argo CD resource actions, auto-sync off, value overrides | actions (§8.1), pause (§5), config releases (§9) |
| Terraform examples for registries and environment infrastructure | infrastructure as apps through KilnCD (cd-targets.md D3) |
| Postgres, Redis, asynq | KilnDB, Focal's bus (§14) |

txst's network-access and file-share commands (DNAR, NAS) are Apixio's own tooling, outside
deployments, and aren't carried over.

## 18. Beyond txst

- Any git host, either CD provider, and every platform KilnCD reaches, behind one API.
- Apps created end to end: repository, CI, chart, registries and environments, in one call or one
  page.
- Progressive delivery (canary, blue/green, checks you write) under the same release flow.
- Plans before a release lands, and previews per pull request (cd.md).
- The same bytes promoted from staging to production, pinned by digest.
- Approvals from a phone; agents allowed per environment.
- Fleet changes: many apps changed as one reviewed change.
- Delivery metrics, what runs where across environments, and a timeline per release.
- Linked servers instead of one registration per instance.
- One binary, no Postgres, no Redis.

## 19. Plan of work

| Step | What | Done when |
| --- | --- | --- |
| P1 | The core on KilnCD (Kubernetes): the records' and the bus's interfaces (on the current store until KilnDB and Focal's bus land), kiln's git server writing and serving release repositories, apps, environments, registrations and releases in the API and `kiln`, the KilnCD provider, deploy, config, rollback, cancel, enable, disable, history, the watch with its timeouts | on kiln's dev cluster, `kiln deploy` takes a request to a commit to synced and healthy to `succeeded`; a rollback does the same; killing kiln mid-release finishes it after the restart; a hand commit to the release repository shows up as a release |
| P2 | The Argo CD provider: connection, repositories, projects, ApplicationSets (and Applications), deliver, watch, status, diff, manifests, resources, logs, actions, pause | P1's scenarios pass on an environment whose provider is a real Argo CD (its own minikube profile); moving an environment from Argo CD to KilnCD hands over with nothing recreated |
| P3 | Git hosts: GitHub (App, commits, pull requests, deployment statuses, webhooks, repository creation), GitLab, Bitbucket; CI sources on GitLab and Bitbucket | a release repository on each host deploys through both providers; `via: pull-request` works on each |
| P4 | Gates: after, soak, approvals (UI, phone, chat, `kiln`; agents per D6), windows, checks (built in and external), freezes; signed results between servers | each gate holds a release and lets it go, with its evidence recorded |
| P5 | Apps end to end: creation from templates (repository, CI, chart, registries), maintenance (owners, template change, archive, delete, fleet changes), chat (Slack first), delivery metrics | an app goes from nothing to running in staging and production from one page |
| P6 | KilnDB and Focal's bus natively (the stand-ins removed); several servers: watches move between them, linked servers | a server killed mid-release, its release finished by another |
| P7 | Platforms under releases through KilnCD: Lambda, then Workers, Vercel, ECS, then cd-targets.md §3 | each platform deploys, rolls out and rolls back by API |
| P8 | The UI pages, the MCP operations and skills | every call has its page and its operation |

## Status

| Part | State |
| --- | --- |
| This design | agreed 2026-10-18 |
| P1: `/api/deploy`, `kiln deploy` and `kiln app`; environments, apps, registrations; deploy, config, rollback, enable, disable, cancel, candidates, history; releases committed in process with their trailers, delivered through KilnCD, followed to landed across restarts; commits kiln didn't make recorded as releases | built. On a live cluster (minikube): a deploy, a config release, a second version and a rollback each landed, the rolled-back release marked so; a server killed mid-release finished it after its restart with one commit; a hand commit pushed to the release repository became a release and landed; disable removed what ran and enable brought it back |
| P1: release repositories as W14 native repositories, read by token through its endpoint | built: each environment's release repository (`owner/name`) is one of kiln's native repositories, made with its row when the environment is (a name kiln follows upstream is refused), served at `/git/<owner>/<name>.git` to a token that may read it; those from before move into the hosted root at start. On the test server: a release committed into it and landed through KilnCD, a clone with a token read it, one without was refused |
| Validation and refusals, on both providers | built: a chart version (OCI or Helm repository) or git ref that isn't there is refused before its record (`422`, with the newest versions); a release its provider refuses for `watch.refused` fails and its commit is reverted, as is one cancelled while delivering or never delivered. On live clusters: a missing version refused at once on each provider; values that break a chart (Argo CD) and a chart that won't render (KilnCD) each failed after the grace, reverted to a tree identical to the release before, the provider back on it; a cancel mid-delivery reverted and Argo CD rolled back |
| P2: Argo CD instances as integrations (kind `argocd`: URL, namespace, CA, an account's token sealed), verified by their version and rights; an environment's provider `argocd/<integration>`; each registration one Application written through Argo CD's API (the package at the release's version, the environment's values inline, `kiln.release` as a Helm parameter), refreshed hard, followed to landed by the release it compared; pause and resume; disable (with `keep`), enable, deregister; status per provider | built. On a real Argo CD (v3.5.4, its own minikube profile): a deploy, a second version, a rollback and a config release each landed with the release they carried; pause turned the Application's automated sync off and resume back on; a disable with `keep` deleted the Application and left its objects, which the next enable adopted; a full disable removed both; a server killed mid-release finished it after its restart with one commit; deregistering removed the Application, its objects and the registration's files |
| P2: an Argo CD environment through one ApplicationSet over its release repository (`kiln-<env>`: a git files generator on `<root>/*/*/<env>/release.json`, registrations disabled or without a version left out, each Application built from release.json's keys, its values from the release repository as a second source; Applications created and updated, deleted only by kiln), its AppProject (`kiln-<env>`: its apps' sources, namespaces and cluster-scoped kinds), the release repository registered with a reader token kiln mints for it (90 days, replaced a month early or when Argo CD can't read with it, the one replaced revoked; an empty repository first given its README); Applications followed through Argo CD's stream (asked directly only while it's down); a release landed once Argo CD compared it at a commit holding its files; release.json saying what runs (namespace, what it sets, whether Helm), read alike by both providers; kiln writing each Application kept (`argocd: { applications: true }`); a commit kiln didn't make seen by every environment sharing the repository | built. On the deploy-argo test Argo CD (v3.5.4, its own minikube profile), with a test kiln running in that cluster: `argo-set`'s ApplicationSet made podinfo's Application (owned by it, two sources) in its AppProject, with the release repository registered as `kiln` with a token kiln made for it alone, which Argo CD used as it was put; a deploy (6.11.0), a second version and a rollback each landed, compared at their release's commit and followed by the stream; kiln killed while 6.11.2 was delivering finished it after its restart, with one commit; a values.yaml edited and pushed by hand became a release of origin `git` and ran two replicas. In `argo-apps`, where kiln writes the Application, a release landed in the `default` project as before |
| P2: actions on what runs, through either provider (Argo CD's resource actions, and a pod deleted; on KilnCD restart, pause and resume a rollout, scale, suspend and resume a CronJob or run it now, delete a pod), by API, `kiln deploy action`, `deploy.registrations.actions`/`.action` and the registration's page; moving an environment between Argo CD and KilnCD (§8.4: the new provider in shadow, differences beyond tracking marks, a handover, each object's uid read before and after), by API, `kiln deploy env move`, `deploy.environments.move*` and the environment's page | built. On the deploy-argo test Argo CD: Argo CD offered restart, pause, scale and resume (disabled while it runs) on podinfo's Deployment, a restart set its `restartedAt`, and a pod deleted through kiln was made again; `argo-set` moved to KilnCD (compared, agreeing, handed over in seconds; its ApplicationSet and Application gone, KilnCD running it Synced and Healthy) and back to Argo CD's ApplicationSet with every object's uid the same throughout (the move to KilnCD rolled no pods), then released 6.14.1 there; on KilnCD meanwhile, its actions restarted the Deployment and deleted a pod |
| The `deploy.*` operations (P8's MCP half, early, for agents: §12.3), the environment's `agents` policy, idempotency keys, the `deploy` event | built. Over MCP on the test server: a release with a key asked for twice was one release; `deploy.releases.wait` answered as it moved and when it landed; a missing version refused; a rollback with its key landed |
| P4: gates (`after`, `soak`, approvals with reviewers and `self_approve`, allow and deny windows, the checks `no-snapshots` and `owners-set`, freezes), `waiting_approval`, approve and reject (HTTP, `kiln`, `deploy.releases.approve`/`reject`), promotion | built. On the test servers (upstream on Argo CD, downstream on KilnCD): a version not upstream refused; one upstream held for its soak, then for approval; the requester's own approval refused; a named reviewer's approval let it go and it landed; a rejection cancelled one; a freeze and a deny window each held one and released it the moment they were lifted. `promote: auto` (§9.2) since, end to end with KilnCD on the kilncd-e2e and deploy-argo test clusters: v5 landed in `promo-a` (release 16), and `promo-b` (`after: [promo-a]`, `soak: 1m`, `promote: auto`) got release 17 by itself, promoted from 16 in the name of who asked for it (key `promote:16:promo-b`), held for its soak and landed a minute after 16 did |
| What follows a failure (§8.1), end to end: a failed first release's revert, a failed rollback, a provider that stops answering | built. On the kilncd-e2e test cluster: an app's first release (v3, which can't start) failed Degraded, and its revert removed the Deployment, Service and pods it made (nothing had run before it; its namespace stayed). A rollback whose new pod a ResourceQuota refused failed after its timeout and was held, not reverted: the registration marked degraded, the release it rolled back from running on; with the quota lifted, the next rollback landed and cleared the mark. With the cluster frozen (`docker pause`) under a release, it waited ("the provider can't be asked … it is judged when it answers"), and once the cluster answered, KilnCD synced it again and it landed; frozen past `watch.unreachable` (180 s there), the release was held as unknown, not reverted. On the deploy-argo test Argo CD, with its API server stopped after it took a release: the release waited, and landed when Argo CD answered again; stopped past `watch.unreachable` (150 s), the release was held. (Until then, KilnCD judged a release whose cluster stopped answering failed and reverted it, and a first release's revert left what it made running) |
| P4: external checks (a URL that answers pass, fail or pending, asked with kiln's OIDC token for it; a kiln workflow run by `workflow_dispatch`, given what it declares of the release), asked once the release is recorded and kept with it, and `report_only`; the provenance gate (`require_provenance`, ci.md W18-R04: kiln's signed provenance of each artifact a new version or a rollback deploys, from the workflows and branches allowed; one not there yet waited for half an hour); gate results signed between linked servers (an integration of kind `kiln`; `after: staging@eu`, proven by a statement the linked server signs with its OIDC issuer for this server, checked against the keys it publishes and kept as the evidence); external checks and provenance on the environment's page, each gate's evidence on a release's | built. On the test server (KilnCD on the kilncd-e2e cluster) and a second one linked to it: a release waited while its URL said pending (asked with release 9's facts and kiln's token for that URL), landed once it said pass (its answer the evidence, not asked again), and another was refused when it said fail; a release waited for its workflow run (`acme/checks`' smoke workflow, given the release's app, version and environment), landed when run #2 succeeded, and one whose run #3 failed was refused; a release of an artifact kiln had no provenance of waited, and landed once the build workflow on `main` built and attested it (attestation #1 its evidence), and one built from `feature` was refused at once (W18-AC04). The second server, linking the first with a reader's token, refused `v5` while the first didn't run it; once it did, it took the first server's signed statement (which openssl verifies again with the key the first publishes), held the release for its two-minute soak and landed it; a link whose token the first refused was refused, and one that couldn't be reached held its release |
| P7: platforms under releases through KilnCD: an app's `platform` package (its provider, `aws-lambda`, `aws-ecs`, `cloudflare-workers` or `vercel`, and its definition as `.kiln/deploy.yaml` holds it, `${{ release.* }}`, `${{ env.name }}` and `${{ app.name }}` filled, KilnCD's `${{ git.* }}` and `${{ artifacts.* }}` left to it; the platform's files from a repository at the release's version), an environment's `targets.platforms` (an account that is an integration of the provider's kind, its place), refused on Argo CD | built. KilnCD renders what a Lambda release defines into its Function (a test, against KilnCD's own provider); on the test server a release reached KilnCD for its target, and was cancelled and reverted; no real account yet (cd-targets.md D8) |
| P5, first part: templates (a package with `{app}` in it, a namespace pattern, checks every release passes, properties with their allowed values, the environments allowed), apps made from a template, partial updates (`PATCH`: owners, properties, contacts, template, package), contacts (on call, channels, links), `catalog-info.yaml` for Backstage, whether a name is free, delete everywhere (deregistered first, objects kept or not); `kiln app set`, `kiln app catalog`, `kiln app templates`, the `deploy.templates.*` and `deploy.apps.*` operations | built. On the test server: an app made from a template took its package and namespace; a property and an environment the template doesn't allow were refused; contacts showed in its catalog-info.yaml; delete refused while registered, and deleted everywhere when asked |
| Delivery metrics (§18; DORA's four keys from the releases: changes landed a day, lead time from asked to landed, change failure rate counting rollbacks, time to restore), for an app, an environment or all over N days; the versions a registration has run; releases by version (txst's last by version, deployed versions); `kiln deploy metrics|versions`, `history --version`; `deploy.metrics`, `deploy.registrations.versions`; an app's Delivery panel in the UI | built. From the test servers' releases: 17 changes, 11 landed, lead time 18 s at p50, 36% failed or rolled back, restored in 2 min 14 s at p50 |
| P5: an app's source repository (§3.1, step 4): one linked (`source`), or one its template makes (`repository`: a name pattern, a starter repository at a branch or tag, the default branch) as one of kiln's own repositories, its first commit the starter's files with `{{kiln.app}}` and the rest filled in, in files and in names (links and modes kept, nothing of the starter run), its first build started as a push of it would; a template that makes repositories put only by someone who may add them; `kiln app create --source`, `kiln app set --source`; the source in `catalog-info.yaml` and on the app's page | built. On the test server: a starter served over git:// became `acme/shop-web` with every placeholder filled and Helm's own `{{ }}` left alone, a script still executable and a link still a link, and its `.kiln/ci.yaml` queued a run; podinfo 6.11.0 from GitHub became `demo/pod-src` (336 files, one commit, in a second); a taken name, a missing starter and a `file://` starter were each refused with nothing left behind; with a token of someone who may make apps but not add repositories, a template without a repository was put and one with a repository (or a new starter) refused, and an app made from the admin's template got its repository |
| P5: fleet changes (§3.3) as releases: one version, or values set, across the apps of an environment a fleet selects, each through its gates, followed together; by API, `kiln deploy fleet` and `deploy.fleets.*` | built. On the test server: `--template svc-src --set ui.message=… --set replicaCount=2` made a config release for each of its two apps, both landed with the values merged into their own; a version across both, asked twice with one key, was the same two releases (they then timed out on a rollout the cluster couldn't finish) |
| P5: kiln's base chart and `kiln app init` (§3.4) | built. `helm lint` passes on what `kiln app init` writes; with every part turned on (an Ingress with TLS, autoscaling on CPU and memory, a digest-pinned image, config files and env, a Secret's key as env, a cron job, a migration hook, a volume, a network policy) all twelve objects render as meant; run again it keeps every file, and `--base` rewrites only kiln-base |
| P5: secrets per app (§3.5): kiln's secrets service over its own backend (versions sealed with AES-256-GCM under data keys of its own, wrapped by the master key and bound to scope, name and version; the last ten kept; every read for a delivery recorded; rotation; forgotten with their app or environment; 960 KiB a scope), behind `secrets::Backend` for the JIT secrets service to take over; the API, `kiln app secret set\|import\|list\|rm\|versions\|uses`, `kiln deploy --secrets`, `deploy.secrets.list`, a Secrets tab per registration; each release pins the versions (`release.json`'s `secrets`) and KilnCD renders their values as `<app>-kiln`, rolling the pods that read it; a change is a secrets release, or joins one still waiting; `cd.apps.create` refuses a definition naming secrets; a git package that is a chart renders with Helm as the app, with the environment's values | built. On the test server and the kilncd-e2e cluster: a secret set before the first release was kept, then delivered by it (the Secret, the pod template marked `DB_PASSWORD@1`, the running pod's env); a new value made release 3, which rolled the pod onto it; a dotenv import of two made one release; a deletion made release 5, after which the Secret and the pod held only the other two; the release repository pinned each release's versions; a definition naming another app's secrets was refused (400) and never created; each read for a delivery was recorded |
| P5: owners as groups (§3.1, §15): kiln's groups (`/api/groups`, `kiln groups`, `users.manage`'s) and identity providers' (`github:acme/platform`, from each sign-in), an app's owner, co-owner and ops owner each a group; an app made, or its owners changed, only by someone in its owner team (and the one it had), unless an administrator; releases, cancels, rollbacks and actions a deployer's in their environment (`environments.deploy` on the new `deploy:<env>` scope, or the server) or the app's owner teams' where the environment lets owners release (`owners: { release: true }`, `kiln deploy env add --owners-release`, the environment's page), a token no further than its own grant; fleets a deployer's; the groups a person is in in `/api/me` and on the new-app page; a group that owns apps kept | built. In `deploy::flow`'s tests: owners in kiln's group and in GitHub's release where their environment lets them and not where it doesn't, a stranger and a token held to reading don't, a deployer of production releases there and not in staging, a co-owner cancels what an owner asked for, a fleet is refused to owners; who may give an app its owners (a team they're in, three at most, a provider that isn't one refused, an administrator any). Over HTTP (`tests/deploy_owners.rs`): groups made and filled only with `users.manage` (a name taken in any case, a member who isn't a user refused), `/api/me`'s groups, an app refused to someone outside its owner team and to an unknown kind of group, a group that owns apps kept, a person in no group refused at the door of every deploy route, an owner refused in production and let through in staging, a deployer of staging refused in production; `tests/sso.rs`: a sign-in's groups in `/api/me`, a team left gone with it, each provider's kept |
| P5: kiln-base as an OCI chart (§3.4): each server publishes the kiln-base it ships in its own registry at start (`kiln/charts/kiln-base:<version>`, packaged the same bytes every time, Helm's config and chart media types), pulled by anyone and pushed over or deleted by nobody; `kiln app init` depends on it there (`oci://<kiln>/kiln/charts`, the kiln from `--url`, `KILN_URL` or `--registry`), vendoring it only with `--vendor` or no kiln named; `--base` moves the dependency (the rest of `Chart.yaml` kept) and drops a vendored copy; KilnCD lays the server's own package into a chart that depends on it, never pulling it | built. `tests/kiln_base.rs`: a server published kiln-base 0.1.2 at start; it was pulled without credentials, Helm's media types on its manifest; the server's administrator could neither push over nor delete it; `kiln app init --url` wrote a chart depending on it at `oci://127.0.0.1:<port>/kiln/charts` with nothing vendored; helm 4.3 built that dependency from the server (`--plain-http`), linted the chart and rendered its Deployment. Unit tests: the package is the same bytes twice, its files' modes 0644; the dependency moved between the registry and vendored with the rest of `Chart.yaml` kept; kiln-cd's renderer laid in the server's own kiln-base at an exact version, and asked helm for another version, another repository, or a name that leaves its directory |
| P5: source repositories on GitHub, GitLab or Bitbucket (§3.1, step 4): a template's `repository.host` makes each app's repository there with P3's clients (private), its first commit the starter's files filled in, pushed with credentials that write that one repository; then one of kiln's repositories as an upstream, its first commit building as a push would; a name the host has (or kiln has) refused before anything is made; one made whose first commit didn't reach it deleted again, and no app made | built. In each of `tests/deploy_github.rs`, `deploy_gitlab.rs` and `deploy_bitbucket.rs`, against the fake host: an app made from a template on that host got `acme/<host>-web` there, one commit holding the starter's files, and kiln had it as one of its repositories at the host's URL; a second template naming it was refused (409). On GitLab and Bitbucket the starter was a private repository with a KilnCI workflow, read with kiln's credentials for the host, and the new repository's first commit ran it; on Bitbucket, a repository whose first push the host refused was deleted again and no app was made |
| P5: registries for an app's images (§3.1, step 5): a template's `registry` makes each app's release image, snapshot and chart repositories with it, on kiln's own registry (policies: immutable tags, refused moves and deletes; the newest `keep_snapshots` snapshot tags kept, the collector dropping the rest and the manifests no tag or index names), on ECR through an `aws` integration (immutable or mutable tags, lifecycle policies; one there already takes the rules) or on Artifact Registry through a `gcp` one (immutable tags, cleanup policies); the references on the app, on its page and in its starter's `{{kiln.image}}` and the rest; a template that makes registries put only with `settings.manage`, and only for an integration that makes them | built. `tests/deploy_registries.rs`: an app from a template with `registry: { at: kiln }` got its three repositories; a release image's tag took the same image again, refused another (409) and refused to be deleted, while a snapshot's tag moved; someone without `settings.manage`, or an integration that isn't there, got no such template. Unit tests: a snapshot path kept its newest two tags, a manifest an index names stayed; against a fake ECR (its JSON protocol) the three repositories were made, the one already there given immutable tags, the snapshots' lifecycle keeping five; against a fake Artifact Registry, three repositories with immutable tags or a cleanup policy, one there already kept |
| P5: secrets on Argo CD and platforms (§3.5): on Argo CD each release writes `<app>-kiln` into the app's namespace through the KilnCD cluster that reaches Argo CD's (`targets.kilncd`, set by `kiln deploy env add --kilncd` and shown on the environment's page; or KilnCD's cluster of that name) before its commit, or with `via: pull-request` as kiln sees the merge (what it pins: an open pull request changes nothing), with none of Argo CD's tracking and `compare-options: IgnoreExtraneous`; the release says the versions to the chart (`kiln.secrets`, which kiln-base 0.1.3 stamps on its pods); values read once per set of versions; refused where kiln reaches no such cluster; deleted with the app's last secret. On platforms, the objects KilnCD applies hold references (`kiln-secret:<scope>/<NAME>@<v>`, which a render's own objects can't hold) that the provider resolves as it applies, keeping nothing: Lambda's environment variables, SSM SecureString parameters (a Parameter kind of `aws-ecs`) an ECS task's containers read through `secrets` with the versions a docker label, Workers' `secret_text` bindings, Vercel's sensitive production variables with a new production deployment for new versions; each reads back as kiln's reference while the platform holds what kiln put there, `kiln-secret:changed` otherwise, and one the app no longer has is taken away; a value a reference takes the place of is masked in diffs | built. On the deploy-argo test cluster through test server A (bb-argo: Argo CD, `via: pull-request`, its release repository on the fake Bitbucket; `targets.kilncd: { in-cluster: argo-k }`, deploy-argo added to KilnCD as argo-k): release 17 wrote `bb-web-kiln` (kiln's labels, `IgnoreExtraneous`, `kiln.dev/secrets: DB_PASSWORD@1`, the value set), and after its merge Argo CD synced `kiln.secrets=DB_PASSWORD@1`, tracking only the Deployment and Service, its pruning sync leaving the Secret; with release 18's values podinfo read it as `DB_PASSWORD`, version 1's value in the running pod. Release 20's version 3 left the Secret at version 2 with nothing read while pull request #4 was open; its merge read version 3 once and wrote it, and Argo CD then synced `DB_PASSWORD@3`. A values-only release (22) read nothing, and the pod it rolled read version 3. Deleting the secret, release 24's merge deleted `bb-web-kiln` and its Application dropped `kiln.secrets`. On gl-argo, where kiln reaches no cluster of Argo CD's, release 21 was refused before its commit, saying how to add one, and no merge request was opened. Each read is in the secret's log (`kiln`, `deliver bb-web in bb-argo to Argo CD's in-cluster (through argo-k)`). Unit tests, against fakes of each platform: on Lambda the value reached the function and the function, its record and the views held the reference, a value changed by hand read as `kiln-secret:changed` and was put back, and a new version updated the configuration and published a version; on ECS a SecureString parameter (wave -1) held the value while the task definition read it through `secrets`, a hand change was drift, a new version wrote the parameter and registered a revision, and the parameter went with the app's secret; on Workers the text was Cloudflare's alone, the same secret again uploaded nothing, and one taken out of the app was deleted while kiln's version still served; on Vercel the value was a sensitive production variable a deployment took, a new version made a new deployment, a hand change was drift, and one taken out was deleted; kiln-cd put each platform's references in place (and refused an app whose objects take none); a diff masked the variable a reference takes over; `helm template` of an init'd chart stamps `kiln.secrets` on the pods |
| P5: fleet changes as pull requests (§3.3): a fleet change with `source` makes a kiln-base version (the chart's dependency moved in place; a vendored kiln-base to the version this kiln ships, its files with it) or values set in each selected app's chart in its source repository, one commit per repository on the change's branch (`kiln/fleet/<id>`, `Kiln-Fleet`), proposed as a pull request on GitHub and Bitbucket and a merge request on GitLab, and on kiln's own git server as the branch and its diff, which kiln merges when asked (a fast-forward, or a merge commit where the repository moved; a conflict refused); followed together (open, merged, closed, unchanged, failed), withdrawn on request, the same change asked again with its key; an app without a source skipped; environments.manage's; by API (`POST /fleets` with `source`, `POST /fleets/{id}/merge`, `DELETE /fleets/{id}`), `kiln deploy fleet --base`, `--set … --in-source`, `--merge`, `--withdraw`, and `deploy.fleets.*` | built. In each of `tests/deploy_github.rs`, `deploy_gitlab.rs` and `deploy_bitbucket.rs`, against the fake host (`common::fleet_on_host`): `acme/<host>-web` given a chart on kiln-base 0.1.2, a fleet change moving it to 0.1.9 was one pull request (merge request) of only that line, on `kiln/fleet/<host>-base` with its trailer; asked again it was the same one; merged on the host it read as merged and the change as done; asked once it held 0.1.9, it was unchanged; values set by another were a second pull request, and withdrawing it closed it there. `tests/deploy_fleet_sources.rs`, on kiln's own: a person without environments.manage was refused (403); the change was a branch and its diff, main untouched and the app without a source skipped; after someone pushed to main meanwhile, kiln's merge was a merge commit holding both, its branch gone; a value set on main too was a conflict, refused (409) with main as they left it; withdrawn, its branch went. Unit tests: kiln's branch, diff, fast-forward, merge commit and conflict; kiln-base's version moved in place, vendored only to this kiln's; values in a chart named elsewhere; a key that names a branch |
| P5: one page (§3.1): the new-app page makes an app from a template with its source repository and registries (shown filled in for it), registers it, and releases its first version: straight to each environment that waits on no other, and where an environment takes what lands in another (`after` with `promote: auto`), by that promotion; one that waits on another without promoting is left to release from its page, saying so | built. On test server A, from the page in a browser (recorded): `checkout` (project shop, owner team shop, template svc) got `acme/checkout` made from a starter served over git:// (its first commit by its maker, its README's references to its registries filled in), its release, snapshot and chart repositories on A's registry (`shop/checkout`), and registrations in staging (KilnCD on kilncd-e2e) and production (KilnCD on deploy-argo, `after: staging`, `promote: auto`). 6.10.2 went to staging as release 27, which landed Synced and Healthy, and production took it by itself as release 28, Synced and Healthy: a pod of it Running in each cluster, 39 s from asked to landed |
| P5's chat: with P4's chat approvals and P8's chat commands, in the chat work (Slack first) | not yet |
| Reads per registration, from either provider in KilnCD's shapes (txst's resources, render, diff and logs): resources with sync and health, the manifests it renders (Secrets masked, Helm's test hooks left out on Argo CD), what a sync would change (live, predicted, unified hunks), a workload's pods' logs; on Argo CD, a preview of another version (what it renders, and how that differs from what the deployed version renders). `GET …/resources`, `…/manifests`, `…/diff`, `…/logs`; `kiln deploy resources|manifests|diff|logs`; `deploy.registrations.*` | built. On the test servers each read answered on both providers; previewing podinfo 6.11.0 on Argo CD showed its labels and image change and nothing else. On KilnCD a version is previewed as what runs at that version (its values and artifacts as they are), rendered as KilnCD renders it and compared with what runs renders, Helm's test hooks left out: previewing 6.11.0 where 6.11.2 ran showed the same labels and image change, the running version nothing, and a version that isn't there said so with the newest ones, as a release of it is told |
| P8, first part: the Releases pages under Deployments (`/deployments/releases`): the environments board (apps down, environments across, what runs and what is on its way in each, its provider), what waits for approval with its answer at hand, an app's environments side by side with the release dialog (a version and its image, values, a promotion, a rollback picked from its candidates), pause, resume, enable and disable, its history, and a release's page (its facts, each gate's result and evidence, answers, cancel, what its provider said); live from the `deploy` event, at phone width too; `GET /api/deploy/board` | built. Against the test server in a browser: the board, an app and a release rendered; a reviewer signed in on the board approved a release waiting there and it landed; a refusal (a non-reviewer's) said why |
| P8: a new app from one page (`/deployments/releases/new`, and New app on the board): its name checked as it's left, a template (its package, namespace, checks, allowed environments and the repository it makes, shown filled in for the app) or a package, its source repository (the template's, one kiln has, or none), the environments it's registered in; registering an app from its page | built. In a browser against the test server: an app made from a template got `acme/ui-web` from its starter (its first build queued) and its registration in `e2e`, landed on its page, and its first release, asked for there, landed through KilnCD (Synced, Healthy); at phone width the page fits |
| P8: an app's settings on its page (project, owner teams, on call, chat channels, its source repository; archive and unarchive; delete, deregistered everywhere first, what it runs kept or not, its releases and repository kept); an environment's page from the board (its provider and release repository, clusters and platforms, concurrency, what agents may do, each gate, what runs there) with a freeze put on and lifted there, with why | built. In a browser against the test server: owners, on call and channels saved; an archived app's registration refused; an app deleted from its page; `e2e` frozen from its page held a release ("e2e is frozen: An incident in payments") until the freeze was lifted there, and it landed |
| P8: what runs for an app in an environment, as its provider sees it, on a page of its own (What runs, on each environment of an app's page): its objects with sync and health, the manifests it renders, what a sync would change, a workload's pods' logs; on Argo CD another version previewed (what it renders, what deploying it changes). Both providers answer a diff as `diffs` (Argo CD's said `changes`, so `kiln deploy diff` said "nothing would change" on KilnCD) | built. In a browser against the test servers: `ui-web` in `e2e` (KilnCD) showed its Service and Deployment synced and healthy, their manifests and its pod's log; `podinfo` in `argo-staging` (Argo CD) previewed 6.11.0 as its labels and image changing, side by side, as `kiln deploy diff --version` printed it |
| Release messages (§12.3): a release waiting for approval, landed, failed or cancelled, to its app's channels and through the server's notification rules (`where` reading `release`), on every channel kiln sends to | built. On the test server, through a signed webhook: an approval gate on `e2e` held a release, and "⏸ ui-web 6.11.1 → e2e waits for approval … approvers: ada" came once (a server rule and the app's channel naming the same webhook); ada's approval of another landed it ("✓ ui-web 6.11.0 landed in e2e"); her rejection cancelled one ("◌ … was cancelled … not today"). Loopback was refused until `[ci] allow_private_targets` listed it |
| P8: the skill agents read (`kiln-deploy`, served over MCP and installed for every agent kiln starts): releasing by version with an idempotency key, waiting rather than polling, what each state means, promotion, looking before and after (diff, resources, logs), rollback and cancel, and the rules for agents (their environment's `agents` policy, never their own approval, a 403 handed to a person, a freeze left to people) | built. Its example program, run through code mode on the test server, released `ui-web` 6.11.2 to `e2e` and waited for it to land; run again with the same key, it answered the same release |
| P8: environments made and set in the UI: New environment on the Releases board (its name, its provider, KilnCD or one of kiln's Argo CD integrations, its release repository, made if it isn't there, its clusters), and on its page how releases go there (upstream environments and soak, approvals with reviewers and the requester's own, the built-in checks, allow and deny windows with time zones, whether a second release is refused or waits, what agents may do) | built. In a browser against the test server: `qa` made from the board with a release repository of its own and `e2e-mk`; its gates set on its page held a release of a version `e2e` runs for ada's approval, and refused one `e2e` doesn't run ("6.11.1 doesn't run in e2e (6.11.2 does): release it there first") |
| P8: how an app deploys, changed on its page: its template, and its package (a Helm chart with its repository and a pinned chart version or each release's, or a path in git) with the values each release sets; a platform's definition still by API or `kiln app set` | built. In a browser against the test server: `ui.message=hello from kiln` set on `ui-web`'s page reached its Deployment (`PODINFO_UI_MESSAGE`) with its next release, and was taken off again there |
| P8, the rest: chat commands and a message that updates in place (Slack); a platform's definition edited in the UI | not yet |
| P3, GitHub: release repositories on GitHub through kiln's GitHub App (`release_repo.host: github`, by API, `kiln deploy env add --host github` and the New environment form): made in the owner's organization when they aren't there, read through a mirror of kiln's (KilnCD reads the mirror; Argo CD reads GitHub with an installation token that reads that repository only, which kiln puts in place every half hour), written by a push with a lease on the head the commit was made on; releases as pull requests (`gates.via: pull-request`, on the environment's page too): proposed from a branch of its own (`kiln/release-<id>-<asked at>`, so another kiln's release of the same id is never taken for it) once the gates pass; merged, the merge is delivered and who merged it kept as the approval; closed unmerged, cancelled; cancelled in kiln, closed there saying why (refused beside `approvals`, or on kiln's own git server); each release a deployment of its commit to `<environment>/<app>`, its status following the release (`in_progress`, `success`, `failure`, `inactive`) and linking to it on kiln; the App's `push` and `pull_request` webhooks have a release repository looked at, and a release's pull request followed, at once, with polling once a minute without them; the release page shows its pull request | built. Against a fake GitHub API and a smart HTTP git server that answers only installation tokens (`tests/deploy_github.rs`): the repository made once through the App; registrations and releases pushed with tokens for that one repository; a deployment `in_progress` for a release delivering, `inactive` once it was cancelled and its commit reverted on GitHub; a pull request merged (a merge commit, delivered, its merger the approval; the merge reverted when that release was cancelled), one closed unmerged (cancelled), one cancelled in kiln (closed there, with a comment); signed `pull_request` and `push` webhooks moving it at once, and a commit kiln didn't make recorded as a release of origin `git`. On GitHub itself, with kiln's GitHub App made by its manifest flow for the hyper-light organization (from a browser on another machine, GitHub's answer carried back to the test server) and installed there: kiln made `hyper-light/kiln-p3-releases` through the App, private and empty, and pushed its first commit and each registration through it; on KilnCD (the kilncd-e2e test cluster, reading kiln's mirror) release 2, podinfo 6.10.2, landed; on Argo CD (the deploy-argo test cluster, its ApplicationSet reading GitHub with kiln's installation token) releases 3, 5 and 6 were each a pull request a person merged on GitHub, delivered as the merge (the Application Synced and Healthy at the merge commit), the merger kept as the approval; release 4, cancelled in kiln, had its pull request closed there; release 7's pull request, closed unmerged on GitHub, cancelled it; GitHub took each landed release's deployment status (`success`). The App's webhook was off there (the test server's address isn't one GitHub reaches): kiln saw each merge and close by polling, within a minute |
| P3, GitLab: release repositories on a GitLab (`release_repo.host: gitlab`, `group/…/name`) through the `[[ci.gitlab]]` whose token covers the group, as GitHub's are: made in the group when they aren't there, read through kiln's mirror and written by a push with a lease, releases as merge requests (`via: pull-request`), each release a GitLab deployment to `<environment>/<app>`, GitLab's `Push Hook` and `Merge Request Hook` moving it at once; Argo CD reads with a deploy token of the project kiln makes (`read_repository`); `GET /api/deploy/git-hosts`, and GitLab in the New environment form; CI sources on GitLab: KilnCI's runs of a project there reported as commit statuses (`kiln/<workflow>`) | built. Against a fake GitLab (`tests/deploy_gitlab.rs`: its v4 API, and git that lets into a private project only kiln's token): KilnCI read a private project with the token, ran its workflow on a push hook, and reported `running` then `success` on the commit; the release repository was made once in its group; registrations and releases were pushed; a release delivering was a deployment `running`, `canceled` once cancelled and reverted; a merge request merged was delivered as its merge, its merger the approval, one closed was cancelled, one cancelled in kiln was closed with a note; a push hook brought a commit kiln didn't make in as a release of origin `git`. Through both providers on the test clusters, with that fake GitLab in the deploy-argo cluster (a NodePort both reach): release 8 (podinfo 6.10.2) landed on KilnCD (kilncd-e2e) from kiln's mirror; release 9 was merge request !1, merged, and Argo CD (deploy-argo) read the fake GitLab with kiln's deploy token, Synced and Healthy at the merge commit; !2 closed cancelled release 10, release 11 cancelled in kiln closed !3; the fake GitLab held a `success` deployment for each landed release |
| P3, Bitbucket: release repositories on Bitbucket Cloud (`release_repo.host: bitbucket`, `workspace/name`) through `[ci.bitbucket]`'s access token (or user and app password) where it covers the workspace, as GitLab's are: made in the workspace when they aren't there (private, empty), read through kiln's mirror and written by a push with a lease; releases as pull requests (`via: pull-request`: merged, the merge delivered and its merger the approval; declined, cancelled; cancelled in kiln, declined there with a comment, its branch deleted); each release a build status of its commit (`deploy/<environment>/<app>`: `INPROGRESS`, `SUCCESSFUL`, `FAILED`, `STOPPED`) linking to it on kiln; Bitbucket's `repo:push` and `pullrequest:*` webhooks moving it at once, polling once a minute without them; Argo CD reads over SSH (`[ci.bitbucket] ssh_url`) with a deploy key kiln makes for that one repository, made again, the one before revoked, when Argo CD can't read with it (kiln asks Argo CD to try the repository again, not for its cached answer, which can say a revoked credential still reads); Bitbucket in `GET /api/deploy/git-hosts`, the New environment form and `kiln deploy env add --host bitbucket`; CI sources on Bitbucket: KilnCI's runs of a repository there reported as build statuses (`kiln/<workflow>`) | built. Against a fake Bitbucket (`tests/deploy_bitbucket.rs`: its 2.0 API, and git that lets into a private repository only kiln's access token): KilnCI read a private repository with the token, ran its workflow on a signed push webhook, and reported `SUCCESSFUL` on the commit; the release repository was made once in its workspace (another workspace, or a deeper name, refused); registrations and releases were pushed; a release delivering was a build status `INPROGRESS`, `STOPPED` once cancelled and reverted; a pull request merged was delivered as its merge, its merger the approval, its branch deleted; one declined was cancelled; one cancelled in kiln was declined with a comment; a push webhook brought a commit kiln didn't make in as a release of origin `git`. Through both providers on the test clusters, with a fake Bitbucket in the deploy-argo cluster (its API and git over HTTP, and an sshd whose deploy keys only read their own repository): release 13 (podinfo 6.10.2) landed on KilnCD (kilncd-e2e) from kiln's mirror; release 14 was pull request #1, merged there, its signed `pullrequest:fulfilled` webhook moving it at once, and Argo CD (deploy-argo) read `ssh://…/acme/releases.git` with kiln's deploy key, Synced and Healthy at the merge commit; #2 declined there cancelled release 15 (seen by polling, within a minute); release 16, cancelled in kiln, declined #3 with a comment; Bitbucket held a `SUCCESSFUL` build status for each landed release and `STOPPED` for the one cancelled. With its deploy key gone (the fake Bitbucket made again), kiln gave Argo CD a new one within seconds; with Argo CD's credential removed, it made another at its next look (every five minutes) and revoked the one before |
| P3, the rest: `via: pull-request` on kiln's own git server (its diffs), and Bitbucket Data Center | not yet |
| P6 | not yet |

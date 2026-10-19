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
   kiln group or an identity-provider group. The caller must belong to the owner team, unless an
   admin creates it.
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
   A name already taken is refused before anything is made, and nothing half made is kept. On
   GitHub, GitLab or Bitbucket it comes with their hosts (§7, P3).
5. **Registries.** Create the repositories for images and charts on the chosen registry (any OCI
   registry: ECR, Artifact Registry, ACR, GHCR, Docker Hub, kiln's own later), snapshot and release
   paths, release tags immutable, lifecycle rules.
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
- the source repository each app gets (§3.1): `repository: { name: acme/{app}, starter:
  https://github.com/acme/service-starter, starter_ref: v3, branch: main }`. Each app made from
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
  platform takes them (Kubernetes Secrets, cloud secret managers, platform variables).
- **Fleet changes**: one setting, or a base chart version, changed across many apps as one reviewed
  change. kiln opens one pull request per source repository, or makes one config release per
  registration, and shows their progress together. Built so far: a fleet change in one
  environment (`POST /api/deploy/fleets`, `kiln deploy fleet`, `deploy.fleets.*`) releases the
  apps it selects (named, or by template, project or owner; all registered there when none is),
  each through its gates: one version to each, or values set in each app's own (`path=value`,
  merged into its values file: a config release each). `GET /api/deploy/fleets/{id}` (`--show`)
  says how they stand. Asked again with the same key, it is the same releases.
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
`kiln app init <app>` writes `charts/<app>` (kiln-base vendored in its `charts/`, so no registry
is needed) and `deploy/<env>.yaml` for each environment into a repository, keeping what's
there unless `--force`; `--base` writes only kiln-base, to move an app to the version this kiln
ships.

### 3.5 Secrets per app

An app's secrets for an environment (a database password, an API key) come from kiln's secrets
service and reach what runs without ever passing through git. Agreed 2026-10-19, as the first
piece of a larger one: a just-in-time secrets service (AWS Secrets Manager's scale, Vault's
controls) that will take over the storage and issuing. The code is cut so that cutover swaps one
backend: the API, `kiln`, the UI, delivery and audit stay as they are.

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
  - **The JIT secrets service** (later): the same calls over its API; leases come back with
    `resolve`, and delivery renews them before they expire.
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
- **On Argo CD.** Argo CD renders from git alone, so the Secret can't be added at render: the
  External Secrets Operator with kiln as its webhook provider (the chart carries an
  ExternalSecret; the operator fetches values from kiln with a token scoped to that app and
  environment), or Sealed Secrets (kiln seals each value with the cluster's key and commits the
  SealedSecret: git holds only ciphertext). Not built yet.
- **A change is a release.** Setting or deleting a secret asks for a secrets release (a config
  release that keeps the version and values: "secrets: DB_PASSWORD v4") where something runs on
  KilnCD, so the environment's gates, history and audit apply. A release still waiting takes the
  change on instead, and says so for whoever approves it; one already committing doesn't, and an
  environment that takes one release at a time refuses another (the change is kept, and the
  answer says how to release it). Where nothing runs yet, the first release delivers them. A
  rollback pins the versions current then: it restores a release's configuration, never older
  secret values.
- **Gone with what they belong to.** Deleting an app, or an environment, deletes its secrets
  there and destroys their values; the record of their versions and reads stays.
- **Platforms.** Lambda, ECS, Workers and Vercel take them their own way (their secret stores or
  environment variables), through KilnCD's providers, later.

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
  notify: { chat: ["#deploys"], webhooks: [] }
```

- An environment names one CD provider. Staging may run on KilnCD while production has its own
  Argo CD, and the same app moves between them.
- An environment on KilnCD may name targets of any kind; one on Argo CD names clusters registered
  in that Argo CD.
- `via: pull-request` makes every release in the environment a pull request (a merge request on
  GitLab) on the release repository: the approval is the merge, made on the git host.
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
  "rollback_of": null
}
```

- **One release, one commit.** The message says what changed (`deploy web 1.4.2 to production
  (release 1042)`), and trailers name the release and the people: `Kiln-Release: 1042`,
  `Kiln-Requested-By: ada`, `Kiln-Approved-By: …`.
- **Git says what runs.** A release the provider refused, or never got, and one cancelled while it
  was delivered, is reverted by a commit of kiln's (`Kiln-Reverts: 1042`) that puts its files back
  as they were, unless someone changed them since. A release that ran and then failed stays: roll
  back, or fix it forward. So does a commit kiln didn't make: it fails, and git keeps it.
- **JSON for kiln, YAML for people.** Argo CD's git files generator reads `release.json`'s keys as
  parameters. People edit `values.yaml` and `app.yaml` in pull requests like any file.
- **Safe writes.** kiln commits against the branch head it read and retries on a newer one, keeping
  everything the release didn't touch. Two releases of one registration never race: one is in
  flight at a time (§9).
- **Commits kiln didn't make are releases too.** A merged edit to `values.yaml`, or anything else
  under a registration's path, is seen (webhook or poll) and recorded as a release of origin `git`.
  The records never disagree with git.

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
| Create repositories | yes | from template repositories | from project templates | yes |
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
- **GitLab and Bitbucket** get the same: discovery, webhooks, polling, mirrors, change-request heads
  and statuses for CI, and everything above for release repositories. GitLab.com and self-managed
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
| `action(app, env, object, action)` | restart, scale, suspend, resume, delete a pod: what the provider offers |
| `automation(app, env, on)` | pause or resume the provider's automation (§5) |

**When a release is done.** The provider reports, on every target of the environment, the release's
commit, synced and healthy, steady for `watch.settle` (default 30 s). **When it failed**: a sync
failed after its retries, health stayed Degraded past its grace, or `watch.timeout` passed. A
provider that can't be reached leaves the release `delivering`, retried with backoff until the
timeout. **When it was refused**: the provider can't make anything of it (Argo CD answers `4xx` to
its Application, or reports a `ComparisonError`; KilnCD can't read its sources or render them) for
`watch.refused` (default 60 s, room for a passing trouble to clear). The release fails, its commit
is reverted, and the provider is handed what git says then: what ran before runs on. A release the
provider never held before the timeout is reverted the same way.

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

- **Connect** (`POST /api/deploy/providers`): the URL, a token (an Argo CD account's API key, or an
  AppProject role's token), TLS settings, and the namespace Applications live in. kiln checks the
  version (`GET /api/version`) and its rights (`GET /api/v1/account/can-i/…`) and says what's
  missing.
- **Register**: through Argo CD's API, kiln keeps in place:
  - the release repository (`POST /api/v1/repositories?upsert=true`), with a read token: kiln's own
    server's URL, or the GitHub, GitLab or Bitbucket one;
  - an AppProject per kiln project (`POST /api/v1/projects`): the sources allowed (the release
    repository, the app's chart repositories) and the destinations (the environment's clusters,
    the template's namespace pattern, cluster-scoped kinds only for templates that allow them);
  - one ApplicationSet per environment (`POST /api/v1/applicationsets?upsert=true`), whose git files
    generator reads `<root>/*/*/<env>/release.json` (crossed with the environment's clusters when
    it has several) and makes one Application per app and target: the package at `version`, values
    from the release repository, `kiln.release=<id>` as a Helm parameter (a label otherwise), the
    environment's sync policy, and registrations with `enabled: false` left out.

  Where ApplicationSets aren't allowed, kiln writes each Application itself
  (`POST`/`PUT /api/v1/applications`), with the same spec.
- **Deliver**: a hard refresh (`GET /api/v1/applications/{name}?refresh=hard`), then
  `POST /api/v1/applications/{name}/sync` when the environment syncs on demand.
- **Status**: `GET /api/v1/stream/applications?name=…` streams each Application. The release has
  landed when `status.sync.revision` is its commit, `status.sync.status` is Synced,
  `status.health.status` is Healthy, and the spec's `kiln.release` parameter is its id: txst's check,
  with the commit added.
- **Diff, manifests, resources, logs, actions**: `/server-side-diff`, `/manifests?revision=`,
  `/resource-tree`, `/logs`, `/resource/actions`.
- **Pause**: the Application's automated policy off; the next release turns it back on.
- **Adopting**: an Argo CD Application kiln didn't make becomes a registration. kiln writes its
  path in the release repository, and the ApplicationSet takes the Application over.

### 8.4 Changing an environment's provider

The release repository stays as it is. kiln registers every app with the new provider in shadow
mode (it compares and never writes), shows where the two disagree, and hands over when they agree:
KilnCD already takes objects over from Argo CD by its tracking (cd.md), and Argo CD adopts objects
kiln deployed through its own tracking annotation. Nothing is recreated either way.

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
| `after` | the same version, with the same artifacts, succeeded in each upstream environment and still runs there healthy (txst's deploy-validate) |
| `soak` | it has run there that long |
| `approvals` | enough reviewers approved, from the UI, a phone, chat or `kiln`; agents only where the environment allows them (cd-targets.md D6). With `via: pull-request`, the merge is the approval |
| `windows` | no deny window covers now (Argo CD's sync windows: allow and deny, cron, duration, timezone) |
| `checks` | each check passes: built in (`no-snapshots`, `artifact-exists`, `artifact-signed`, `vulnerabilities`, `owners-set`) or external (a URL that answers pass or fail, a kiln workflow). A check is `required` or `report-only` |
| `freeze` | no freeze holds the environment or the app (incidents, holidays) |

Each gate's result is recorded on the release, with its evidence. A release kiln can't gate is
refused, never let through.

How each one holds or refuses (built: all but the external and signature checks):

- **Refused at once** (`409`): a version that doesn't run upstream (`after`), or runs there with
  other artifacts; a failed check; a window that doesn't read. Nothing waiting would change them.
- **Held** (`waiting`, the reason in its message): a freeze, a deny window (until it closes) or no
  allow window open (until one opens), a soak still running, the same version still on its way
  upstream (an upstream release of it in flight).
- **Held for people** (`waiting_approval`; the event says `needs: approval` and who): too few
  approvals. `POST /releases/{id}/approve` (`runs.approve`, a note optional) counts one per
  person, a token or agent counting as the person it acts for; `…/reject` cancels the release,
  with who and why. Whoever asked doesn't approve their own release unless `self_approve`;
  reviewers, when named, are the only ones who may; agents answer only with `agents.approve`.
- `after` and `soak` hold a new version; a rollback, a config change or an enable runs what passed
  them before, and only freezes, windows, checks and approvals hold it.
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

## 12. Clients

### 12.1 API

| Resource | Calls |
| --- | --- |
| `/api/deploy/git-hosts` | list, add, test, update, remove |
| `/api/deploy/providers` | list, add (Argo CD connections; KilnCD is always there), test, update, remove |
| `/api/deploy/templates` | list, add, update, remove |
| `/api/deploy/environments` | list, add, show, update, remove |
| `/api/deploy/apps` | list (filters: owner, template, name), create, show, update, archive, delete |
| `/api/deploy/apps/{app}/environments/{env}` | register, show (status, history, candidates), update, deregister, `pause`, `resume`, `enable`, `disable`, `diff`, `manifests`, `resources`, `logs`, `actions` |
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
```

Environments are `kiln deploy env …`: `kiln env` is CI's (a repository's environments, ci.md W15)
until the two become one.

### 12.3 UI, chat and MCP

- **UI**: Apps (the list, an app's page with environments side by side), the environments board
  (what runs where, what waits to move), a release's page (its gates, commit and live progress),
  the deploy dialog, the rollback picker, an approvals inbox that works on a phone.
- **Chat**: Slack first. Commands for the calls above, and a message per release that updates in
  place as it moves (txst's deployment messages), in the registration's channels. Built so far: a
  message when a release waits for approval (naming who may approve), lands, fails or is
  cancelled (`release.waiting_approval`, `release.succeeded`, `release.failed`,
  `release.cancelled`), to the app's own channels (`contacts.channels`: `kind:target` as
  ci.md W19-R06 names channels, a bare `#channel` being Slack's) and through the server's
  notification rules (`[[ci.notify]]`, `where` reading `release`: its `app`, `environment`,
  `kind`, `version`, `requested_by`, `approvers`), on any channel kiln has: Slack, Discord, Teams,
  Telegram, PagerDuty, Opsgenie, signed webhooks. Each goes once per release, event and channel.
- **MCP**: one operation per call, in kiln's registry of operations (ci.md W20): MCP tools
  (`deploy_releases_create`) and code mode's `kiln.deploy.*`. Each is one of `/api/deploy`'s routes,
  judged by the route table as that route is, and audited as it is:

  | Operation | |
  | --- | --- |
  | `deploy.environments.list`, `.get`, `.put` | environments |
  | `deploy.apps.list`, `.get`, `.put` | apps |
  | `deploy.registrations.get`, `.create`, `.delete`, `.pause`, `.resume`, `.candidates` | registrations: what runs, register, deregister, the provider's automation, what a rollback can go to |
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

- Ownership is checked on every release: the caller belongs to one of the app's owner teams (txst's
  rule), or holds `deploy.release` on the environment for every app.
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
| The provider can't be reached | the release stays `delivering`, retried with backoff until its timeout; then it fails, and if the provider never held it, its commit is reverted |
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
| P2: ApplicationSets over the release repository, AppProjects, the repository registered for reading, diff, manifests, resources, logs, actions, the stream rather than polling; moving an environment between providers | not yet: ApplicationSets wait on release repositories being served (W14) |
| The `deploy.*` operations (P8's MCP half, early, for agents: §12.3), the environment's `agents` policy, idempotency keys, the `deploy` event | built. Over MCP on the test server: a release with a key asked for twice was one release; `deploy.releases.wait` answered as it moved and when it landed; a missing version refused; a rollback with its key landed |
| P4: gates (`after`, `soak`, approvals with reviewers and `self_approve`, allow and deny windows, the checks `no-snapshots` and `owners-set`, freezes), `waiting_approval`, approve and reject (HTTP, `kiln`, `deploy.releases.approve`/`reject`), promotion | built. On the test servers (upstream on Argo CD, downstream on KilnCD): a version not upstream refused; one upstream held for its soak, then for approval; the requester's own approval refused; a named reviewer's approval let it go and it landed; a rejection cancelled one; a freeze and a deny window each held one and released it the moment they were lifted. `promote: auto` (§9.2) since: unit-tested, end to end once the test kiln deploys to a test cluster again. External checks, signatures and signed results between servers: not yet |
| P7: platforms under releases through KilnCD: an app's `platform` package (its provider, `aws-lambda`, `aws-ecs`, `cloudflare-workers` or `vercel`, and its definition as `.kiln/deploy.yaml` holds it, `${{ release.* }}`, `${{ env.name }}` and `${{ app.name }}` filled, KilnCD's `${{ git.* }}` and `${{ artifacts.* }}` left to it; the platform's files from a repository at the release's version), an environment's `targets.platforms` (an account that is an integration of the provider's kind, its place), refused on Argo CD | built. KilnCD renders what a Lambda release defines into its Function (a test, against KilnCD's own provider); on the test server a release reached KilnCD for its target, and was cancelled and reverted; no real account yet (cd-targets.md D8) |
| P5, first part: templates (a package with `{app}` in it, a namespace pattern, checks every release passes, properties with their allowed values, the environments allowed), apps made from a template, partial updates (`PATCH`: owners, properties, contacts, template, package), contacts (on call, channels, links), `catalog-info.yaml` for Backstage, whether a name is free, delete everywhere (deregistered first, objects kept or not); `kiln app set`, `kiln app catalog`, `kiln app templates`, the `deploy.templates.*` and `deploy.apps.*` operations | built. On the test server: an app made from a template took its package and namespace; a property and an environment the template doesn't allow were refused; contacts showed in its catalog-info.yaml; delete refused while registered, and deleted everywhere when asked |
| Delivery metrics (§18; DORA's four keys from the releases: changes landed a day, lead time from asked to landed, change failure rate counting rollbacks, time to restore), for an app, an environment or all over N days; the versions a registration has run; releases by version (txst's last by version, deployed versions); `kiln deploy metrics|versions`, `history --version`; `deploy.metrics`, `deploy.registrations.versions`; an app's Delivery panel in the UI | built. From the test servers' releases: 17 changes, 11 landed, lead time 18 s at p50, 36% failed or rolled back, restored in 2 min 14 s at p50 |
| P5: an app's source repository (§3.1, step 4): one linked (`source`), or one its template makes (`repository`: a name pattern, a starter repository at a branch or tag, the default branch) as one of kiln's own repositories, its first commit the starter's files with `{{kiln.app}}` and the rest filled in, in files and in names (links and modes kept, nothing of the starter run), its first build started as a push of it would; a template that makes repositories put only by someone who may add them; `kiln app create --source`, `kiln app set --source`; the source in `catalog-info.yaml` and on the app's page | built. On the test server: a starter served over git:// became `acme/shop-web` with every placeholder filled and Helm's own `{{ }}` left alone, a script still executable and a link still a link, and its `.kiln/ci.yaml` queued a run; podinfo 6.11.0 from GitHub became `demo/pod-src` (336 files, one commit, in a second); a taken name, a missing starter and a `file://` starter were each refused with nothing left behind; with a token of someone who may make apps but not add repositories, a template without a repository was put and one with a repository (or a new starter) refused, and an app made from the admin's template got its repository |
| P5: fleet changes (§3.3) as releases: one version, or values set, across the apps of an environment a fleet selects, each through its gates, followed together; by API, `kiln deploy fleet` and `deploy.fleets.*` | built. On the test server: `--template svc-src --set ui.message=… --set replicaCount=2` made a config release for each of its two apps, both landed with the values merged into their own; a version across both, asked twice with one key, was the same two releases (they then timed out on a rollout the cluster couldn't finish) |
| P5: kiln's base chart and `kiln app init` (§3.4) | built. `helm lint` passes on what `kiln app init` writes; with every part turned on (an Ingress with TLS, autoscaling on CPU and memory, a digest-pinned image, config files and env, a Secret's key as env, a cron job, a migration hook, a volume, a network policy) all twelve objects render as meant; run again it keeps every file, and `--base` rewrites only kiln-base |
| P5: secrets per app (§3.5): kiln's secrets service over its own backend (versions sealed with AES-256-GCM under data keys of its own, wrapped by the master key and bound to scope, name and version; the last ten kept; every read for a delivery recorded; rotation; forgotten with their app or environment; 960 KiB a scope), behind `secrets::Backend` for the JIT secrets service to take over; the API, `kiln app secret set\|import\|list\|rm\|versions\|uses`, `kiln deploy --secrets`, `deploy.secrets.list`, a Secrets tab per registration; each release pins the versions (`release.json`'s `secrets`) and KilnCD renders their values as `<app>-kiln`, rolling the pods that read it; a change is a secrets release, or joins one still waiting; `cd.apps.create` refuses a definition naming secrets; a git package that is a chart renders with Helm as the app, with the environment's values | built. On the test server and the kilncd-e2e cluster: a secret set before the first release was kept, then delivered by it (the Secret, the pod template marked `DB_PASSWORD@1`, the running pod's env); a new value made release 3, which rolled the pod onto it; a dotenv import of two made one release; a deletion made release 5, after which the Secret and the pod held only the other two; the release repository pinned each release's versions; a definition naming another app's secrets was refused (400) and never created; each read for a delivery was recorded |
| P5, the rest: owners as groups (kiln has none yet), kiln-base published to a registry (it's vendored for now), source repositories on GitHub, GitLab or Bitbucket (with P3), registries, secrets on Argo CD and platforms (§3.5), fleet changes as pull requests, chat | not yet |
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
| P3, P6 | not yet |

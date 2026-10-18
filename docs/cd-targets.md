# Deploying anywhere

> **Proposal, 2026-10-18, for review. Nothing here is built yet.** It extends [cd.md](cd.md), the
> CD engine's specification, from Kubernetes to every place kiln deploys to. Identity (W10),
> integrations (W11) and environments (W15) are shared with [ci.md](ci.md); §13 records what the
> Kiln CI work and this work each build, as agreed on 2026-10-18.

## At a glance

- **One experience everywhere.** kiln does for every target what it does for Kubernetes, as
  Argo CD would:
  - desired state from git, and a diff before anything changes;
  - sync, health, drift, history and rollback;
  - plans of commits, stacks and pull requests, and previews.

  The targets: AWS, Google Cloud, Azure, Cloudflare, Vercel, Railway, Fly.io, Netlify, Render,
  Heroku, DigitalOcean, Nomad, the hosts kiln's runners run on, and Kubernetes as today.
- **Every target is a cluster of objects.** A provider shows the engine a platform's resources
  (an ECS service, a Lambda alias, a Worker deployment, a Cloud Run service) as objects. It does
  so behind the seam the engine already uses for Kubernetes: render, watch, apply, delete,
  health. Everything above that seam is reused as it is: diffs, plans, waves and hooks, the tree,
  history, rollback, self-heal, shadow mode and take-over, the MCP tools, the UI.
- **Revisions carry artifacts.** A revision is a commit plus what CI built from it: image digests,
  bundles, zips. Environments promote revisions, so production runs the bytes staging ran,
  never a rebuild.
- **Rollouts with verification.** All at once, rolling, blue/green and canary steps, with
  analysis and automatic rollback. kiln uses each platform's own mechanism where one exists.
- **No stored cloud keys.** A sync federates kiln's OIDC identity (W10) through the integrations
  broker (W11) for credentials that last minutes: AWS, Google Cloud and Azure by OIDC.
  Platforms without federation use scoped tokens in kiln's secret store.
- **A builder.** The UI creates and describes apps for any target, in place:
  1. pick the platform and account;
  2. pick what runs there, discovered from the account;
  3. choose how it rolls out;
  4. read the plan;
  5. save it to the repository as a diff.

  Or adopt what already runs: import it from the account or from the platform's own config file,
  watch it in shadow mode, then take it over.
- **kiln deploys itself with kiln, first.** kiln's own apps move from Argo CD and
  `deploy-image.sh` to KilnCD, with your approval as the production gate (§10).

## Decisions for you

| # | Decision | Proposed |
| --- | --- | --- |
| D1 | Model: providers present platform resources to the existing engine as objects (§1) | yes |
| D2 | Order of providers (§12) | kiln itself (Kubernetes); then AWS ECS and Lambda, Cloudflare Workers with Durable Objects, Vercel; then Google Cloud and Azure equivalents; then the rest |
| D3 | Infrastructure as code (CloudFormation, Terraform/OpenTofu, Pulumi) as apps | yes, after the platforms (M4) |
| D4 | Rollouts use each platform's own mechanism where it has one; kiln orchestrates only where not (§5) | yes |
| D5 | Self-heal on cloud targets | off by default (people edit consoles); drift is always shown |
| D6 | kiln's production gate | you approve; agents may plan and request, never approve |
| D7 | Registry for kiln's own image until CI's W12 | minikube's registry addon |
| D8 | Test accounts | real sandbox accounts per platform, created once with you, for each provider's end-to-end tests |

## 1. The model

### 1.1 Targets and providers

Today an app's destination is a Kubernetes cluster and a namespace. It becomes a **target**.

| Term | Meaning |
| --- | --- |
| **Provider** | kiln's support for one kind of platform: `kubernetes`, `aws-ecs`, `aws-lambda`, `cloudflare-workers`, `vercel`, … (§3). It knows the platform's resources, how to read and change them, and how they show health. |
| **Account** | An integration (W11) the provider acts through: an AWS role, a Google service account, an Azure app registration, a Cloudflare or Vercel token. Its credentials never leave kiln (§6). |
| **Target** | A provider, an account, and a place: a region, a project, a cluster, a team. For example: `aws-ecs` in `us-east-1` through `aws-prod`. Kubernetes clusters are targets of the `kubernetes` provider. |
| **Environment** | As ci.md W15 defines it (CI owns the model). An environment names its targets. |

### 1.2 Every target is a cluster of objects

The engine reaches a cluster in five ways (cd.md, Comparing and Syncing). A provider answers the
same five for its platform:

| The engine asks | `kubernetes` | Another provider (`aws-ecs`, say) |
| --- | --- | --- |
| render an app | Helm, Kustomize, Jsonnet, directories, plugins → manifests | the app's definition (kiln's YAML, or the platform's own file) → the provider's objects (`TaskDefinition`, `Service`) |
| watch a kind | list, then watch | list, then poll (§2.3), plus events where the platform sends them |
| apply an object | server-side apply | the platform's calls: register a task definition revision, update the service |
| delete an object | delete with propagation | delete; stateful kinds keep `Delete=false` by default |
| tell health | Argo CD's rules | the provider's rules for its kinds (§3) |

To the engine these objects are ordinary, so everything built on it works on every target:

- diffs before a sync, and plans of commits, stacks and pull requests;
- phases, waves and hooks (an ECS task that migrates the database as a `PreSync` hook);
- the resource tree, history and rollback (with its hold), and self-heal;
- request budgets and restart safety;
- shadow mode and take-over, the MCP tools, and the UI.

The objects keep Kubernetes' shape, so neither the engine nor the UI learns another:

```yaml
apiVersion: ecs.aws.kiln.dev/v1
kind: Service
metadata:
  name: api
  namespace: main            # the provider's scope: an ECS cluster, a Google project, a Cloudflare account
spec: { … }                  # what kiln sets, in the platform's own API field names
status: { … }                # what the platform reports
```

`spec` uses each platform's API field names as its API spells them, so the platform's own
documentation applies and nothing is renamed. Groups are `<service>.<platform>.kiln.dev`.

### 1.3 Revisions carry artifacts

Today a revision is a commit, and an image is named in values (`image.tag: ${{ git.short_sha }}`).
Most platforms deploy built things (an image, a Lambda zip, a Worker bundle, a static site), so an
app declares the **artifacts** it deploys:

- **Resolution.** Each artifact resolves, for a commit, to an immutable reference: a digest or a
  content hash. It comes from the CI run that built that commit (an image it pushed, a file it
  uploaded), or from a registry tag resolved to a digest once.
- **The render key.** A revision is the commit plus those references. The render key includes
  them (content-addressed, as in cd.md), so a rebuild that produces the same digest re-renders
  nothing.
- **Promotion.** `after: staging` and `soak` move the revision, so production gets exactly what
  staging ran.
- **Missing artifacts.** A revision whose artifacts aren't built yet waits (it isn't failed). One
  whose CI run failed is skipped, with the run linked.

### 1.4 What stays as it is

Kubernetes remains the deepest provider: everything in cd.md, Argo CD compatibility included.
It gains what the other providers get:

- artifacts in revisions;
- rollouts (§5) through Argo Rollouts objects (their health, and Promote and Abort);
- later, Gateway API weights.

## 2. Providers

### 2.1 What a provider is

A provider is a Rust module with two halves, split the way the engine is.

- **The pure half (`kiln-cd`).** It holds:
  - its kinds: schemas, scopes, and which kinds own which;
  - rendering its definitions into objects;
  - normalization for diffs: defaults the platform fills in, orders it doesn't keep, names versus
    ARNs;
  - health rules;
  - fields that belong to others: an ECS service's `desiredCount` under autoscaling, like an
    HPA's `replicas`;
  - what a rollout step changes;
  - reading the platform's own config files.

  It does no I/O, so the engine harness can drive a provider's simulated platform at full speed,
  as it does for simulated clusters.
- **The I/O half (`kiln`).** It holds:
  - the platform's API client, on hyper-rt. There are no vendor SDKs: they need tokio
    (prefer-hyper-raft-stack). AWS means SigV4 and its three protocols (JSON, REST-JSON,
    Query/XML), for the operations each provider uses;
  - list, get, apply and delete;
  - children for the tree, logs and events;
  - discovery of what an account holds (for the builder and for import);
  - credentials, through W11's `mint()`.

A provider declares what it can do, and the UI offers only that:

| Capability | Meaning |
| --- | --- |
| `traffic` | weighted rollout steps (§5) |
| `versions` | immutable versions: a rollback moves traffic, uploads nothing |
| `previews` | its own preview URLs per diff, stack or pull request |
| `logs`, `events` | what the object panel can show |
| `drift` | outside edits are readable (otherwise the UI says drift isn't visible) |
| `adopt` | discovery for import and shadow mode |
| `validate` | the platform checks a change without applying it (dry run) |

### 2.2 Where calls run

| From | For |
| --- | --- |
| the server | public control planes: every cloud and hosting API |
| a runner, over its QUIC connection | targets only a private network reaches: Nomad in a VPC, a private Kubernetes API (cd.md's `runner` mode) |
| the runner on the target | `host`, `compose` and `microvm`: the runner there applies (ci.md W15's host, container and microVM kinds) |

### 2.3 Reading what runs

- **Polling.** Few platforms have watches. A provider lists each kind that the target's apps
  contain, then polls:
  - every 2 to 5 seconds for objects a sync or rollout is changing;
  - every 60 seconds at rest, per kind and target, with conditional requests where the API has
    them.
- **Pushes.** Where a platform pushes, kiln reads it as it comes: ECS service events,
  CloudFormation stack events, Beanstalk events. A platform webhook (Vercel, Netlify, Render,
  Railway) refreshes the affected app at once.
- **Budgets.** Each target has a request budget (rate, burst, concurrency), as clusters do. It
  defaults below each platform's documented limits, so kiln never starves the account's other
  users.
- **Restarts.** As in cd.md: until a kind has been read, its objects are *unread*, not missing,
  and automation waits.

### 2.4 Ownership and drift

- **Tracking.** Where the platform has tags or labels, kiln sets `kiln.dev/app` and
  `kiln.dev/tracking` as it does on Kubernetes: AWS tags, Google labels, Azure tags, Cloudflare
  script tags, Fly metadata, Nomad meta. Where it doesn't (Vercel projects, Railway and Render
  services), kiln keeps the resource's id in its state when it creates or adopts the resource.
  kiln never changes a resource it didn't create until that resource is adopted.
- **What kiln owns.** Platforms have no server-side apply, so kiln keeps the last `spec` it
  applied to each object in its own state, like client-side apply's
  `last-applied-configuration`. Drift is:
  - a field kiln sets whose live value differs;
  - or a field kiln no longer sets that still holds kiln's old value.

  Fields kiln never set belong to someone else.
- **Self-heal** is off by default on cloud and hosting targets (D5): people edit consoles, and
  kiln shouldn't fight them unasked. Drift is always shown.

### 2.5 Health, the tree, logs and events

- **Health.** The provider's rules for its kinds come first (§3), then rules from settings (as
  in cd.md), then the generic `status.conditions` rule (Cloud Run's and Azure Container Apps'
  conditions already fit it), then `Healthy` once the object exists. An app's health is still the
  worst of its objects'.
- **The tree.** Each provider says which kinds own which, and lists children when someone asks.
  For example:
  - ECS service → its deployments → tasks → containers;
  - Lambda alias → its versions, with weights;
  - Worker → its deployment → versions, with percentages;
  - Cloud Run service → revisions;
  - Auto Scaling group → instances;
  - Vercel project → deployments.
- **Logs**, in the viewer pods use (follow, since, wrap, copy). The sources include CloudWatch
  Logs (with Live Tail), Cloud Logging, Azure Monitor and Container Apps console logs, Workers
  tail, Vercel runtime logs, Fly, Railway, Render and Heroku logs, Nomad allocations, and journald
  or Docker on runner hosts.
- **Events**, beside kiln's own. Among them: ECS service events, Beanstalk events,
  CloudFormation stack events, Cloud Audit Logs and the Azure Activity Log for the resource.
- **The console.** Each object links to its page in the platform's console.

## 3. The catalog

Each entry rests on the platform's documentation. It is checked again when its provider is built,
and marked verified with a date after an end-to-end test against a real account (as ci.md's
Appendix C does). Every provider gets the same treatment, whether or not it's named here.

### AWS (`aws` integration: OIDC)

| Provider | Kinds | A sync | Rollouts | Health | Reads |
| --- | --- | --- | --- | --- | --- |
| `aws-ecs` | TaskDefinition (immutable revisions), Service | registers a task definition revision, updates the service | ECS's rolling with the circuit breaker and rollback; its blue/green, linear and canary where the account has them; or kiln's steps over weighted ALB target groups | the primary deployment's rollout state; running against desired; target health; CloudWatch alarms | task definition JSON |
| `aws-lambda` | Function, Version, Alias, FunctionUrl, EventSourceMapping, Permission | updates code (S3 zip or image) and configuration, publishes a version, moves the alias | alias weights: canary, linear | `State`, `LastUpdateStatus`; errors and throttles during steps | SAM `template.yaml` functions |
| `aws-beanstalk` | Application, ApplicationVersion, Environment | puts the bundle in S3, makes a version, updates the environment | Beanstalk's own policies: all at once, rolling, rolling with an extra batch, immutable, traffic splitting | status and enhanced health (Green, Yellow, Red) | `Dockerrun.aws.json`, `.ebextensions` |
| `aws-ec2` | LaunchTemplate (versions), AutoScalingGroup | a launch template version with the new image or user data, then an instance refresh | instance refresh: checkpoints, minimum healthy %, skip matching, rollback, alarms | refresh status, instance and target health | — |
| `aws-static` | an S3 prefix, a CloudFront distribution | uploads changed files by content hash, switches the prefix, invalidates | atomic switch | — | — |
| `cloudformation` | Stack; its resources as children | a change set (the plan), executed | CloudFormation's rollback on failure | stack status, resource status, drift detection | templates, including SAM and CDK's synthesized output |

EKS clusters are `kubernetes` targets authenticated through the AWS account.

### Google Cloud (`gcp` integration: OIDC)

| Provider | Kinds | A sync | Rollouts | Health | Reads |
| --- | --- | --- | --- | --- | --- |
| `gcp-run` | Service, Revision, Job | updates the service template (a new revision), sets traffic | traffic percentages per revision; tags give a revision its own URL before it takes traffic (previews, smoke tests) | `Ready` conditions | `service.yaml` |
| `gcp-functions` | Function (Cloud Run functions) | deploys source or a build | as `gcp-run` | state | — |
| `gcp-appengine` | Service, Version | deploys a version, splits traffic | traffic splitting | version serving status | `app.yaml` |
| `gcp-compute` | InstanceTemplate, ManagedInstanceGroup | a new template, a rolling update | canary by two versions with target sizes; surge and unavailable limits | the group's stable status, health checks | — |

GKE clusters are `kubernetes` targets.

### Azure (`azure` integration: OIDC)

| Provider | Kinds | A sync | Rollouts | Health | Reads |
| --- | --- | --- | --- | --- | --- |
| `azure-containerapps` | ContainerApp, Revision, Job | a new revision | revision weights; labels give a revision its own URL | provisioning state; revision health and running state | the app's YAML |
| `azure-functions` | FunctionApp, Slot | deploys a package | slots: traffic % and swap, on plans that have slots | app state | `host.json` |
| `azure-appservice` | WebApp, Slot | deploys to a slot, swaps | traffic % to a slot, swap | state, health check path | — |
| `azure-vmss` | ScaleSet | a new image version | rolling upgrade policy: batches, maximum unhealthy, pauses | instance health | — |
| `azure-static` | StaticWebApp | deploys the built site | preview environments | deployment status | `staticwebapp.config.json` |

AKS clusters are `kubernetes` targets.

### Cloudflare (`cloudflare` integration: token)

| Provider | Kinds | A sync | Rollouts | Health | Reads |
| --- | --- | --- | --- | --- | --- |
| `cloudflare-workers` | Worker, Version, Deployment, routes and custom domains, cron triggers, bindings (KV, R2, D1, Queues, services, Durable Object namespaces), Durable Object migrations | uploads a version (the bundle CI built), deploys it | gradual deployments (two versions by percentage); preview URLs per version | deployment status; errors and CPU from Workers analytics during steps | `wrangler.toml`, `wrangler.jsonc` |
| `cloudflare-pages` | Project, Deployment | uploads the built site | preview deployments per branch | deployment stage | Pages' wrangler config |

**Durable Objects.** Their classes ship inside the Worker. A version that carries a migration
(classes new, renamed, deleted, transferred, or SQLite-backed) is never split: it deploys at
100%. The plan says so, and the migration goes first in its wave. Deleting a class deletes its
data, so it needs `Prune=confirm`.

### Hosting platforms (token integrations)

| Provider | Kinds | A sync | Rollouts | Health | Reads |
| --- | --- | --- | --- | --- | --- |
| `vercel` | Project, Deployment, Domain, EnvVar | a deployment from the prebuilt output CI made (or from git), then promotes it | rolling releases where the plan has them; instant rollback (promote an earlier deployment) | `readyState` | `vercel.json` |
| `railway` | Project, Environment, Service, Deployment, Variable | deploys an image or a commit | — (redeploy) | deployment status, health checks | `railway.json`, `railway.toml` |
| `fly` | App, Machine, Volume, Secret | updates machines through the Machines API, with leases | rolling, blue/green and canary, done by kiln as flyctl does them | machine state and checks | `fly.toml` |
| `netlify` | Site, Deploy | uploads changed files by digest, publishes | deploy previews; restore an earlier deploy | deploy state | `netlify.toml` |
| `render` | Service, Deploy | deploys an image or a commit | rollback to a deploy | deploy status | `render.yaml` |
| `heroku` | App, Build, Release, Pipeline | builds a source tarball, or releases an image | pipeline promotion; rollback to a release | release and dyno state | `app.json`, `heroku.yml` |
| `digitalocean` | App (App Platform), Deployment | updates the app spec | rollback | deployment phase | `.do/app.yaml` |

### Kubernetes, self-hosted and anything else

| Provider | Kinds | A sync | Rollouts | Health | Reads |
| --- | --- | --- | --- | --- | --- |
| `kubernetes` | everything cd.md describes | cd.md | Argo Rollouts objects (health, Promote, Abort); Gateway API weights later | cd.md | Argo CD manifests; Helm, Kustomize, Jsonnet |
| `nomad` | Job, Deployment, Allocation | registers the job | Nomad's canaries, promotion and auto-revert | deployment status, allocation health | job HCL or JSON |
| `host` | a service (systemd, launchd or a Windows service) with its binary or files, on runners matching a selector | the runner on each host installs and restarts | rolling in batches across hosts | the declared check per host | — |
| `compose` | a Compose project on runners with Docker | `docker compose up` per host | rolling across hosts | container health checks | `compose.yaml` |
| `microvm` | a long-lived microVM (Firecracker, or shards with an Agentfile image) on runners | the runner starts the new one, then stops the old | blue/green per host | the guest's check | `Agentfile` |
| `terraform` (and OpenTofu) | Workspace; resources from its plan and state | plan (the diff), then apply, in a job on a runner | — | the apply's result; drift by a scheduled plan | `*.tf` |
| `pulumi` | Stack | preview, then up, in a job | — | the update's result | `Pulumi.yaml` |
| `job` | a list of steps (W15's escape hatch) | runs a workflow job as the sync | — | the job's result and a declared check | a workflow |

`terraform`, `pulumi` and `job` render and apply through a job on a runner. Their objects come
from the tool's own plan output (each resource change), so plans and diffs show them like any
other.

## 4. Defining apps

`.kiln/deploy.yaml` gains a key per provider, `artifacts`, `rollout` and `analysis`:

```yaml
apps:
  api:
    artifacts:
      image: { workflow: ci, image: api }       # the image ci.yaml pushed for this commit, by digest
    aws-ecs:
      cluster: main
      service: "api-${{ env.name }}"
      task: deploy/api/task.json                # the platform's own task definition JSON
      set: { "containerDefinitions[name=api].image": "${{ artifacts.image }}" }
    to: [staging, production]                   # each environment's aws-ecs target
    hooks:
      pre-sync: { aws-ecs-task: { task: deploy/api/migrate.json } }
    rollout:
      canary:
        - { weight: 10, pause: 5m, analysis: error-rate }
        - { weight: 50, pause: 10m }
        - { weight: 100 }

  edge:
    source: workers/edge                        # wrangler.jsonc found here: cloudflare-workers
    artifacts:
      bundle: { workflow: ci, artifact: edge-bundle }
    to: [staging, production]
    rollout:
      canary: [{ weight: 5, pause: 10m, analysis: worker-errors }, { weight: 100 }]

  site:
    vercel:
      project: site
      prebuilt: "${{ artifacts.output }}"
    artifacts:
      output: { workflow: ci, artifact: vercel-output }
    to: production
    previews: { on: [diff, pull_request] }      # Vercel's preview URLs, linked on the diff

  thumbnails:
    aws-lambda: { function: thumbnails, alias: live, code: "${{ artifacts.zip }}" }
    artifacts:
      zip: { workflow: ci, artifact: thumbnails.zip }
    to: [staging, production]
    rollout: { linear: { step: 10%, every: 2m, analysis: lambda-errors } }

analysis:
  error-rate:
    prometheus:
      query: sum(rate(http_requests_total{app="api",code=~"5.."}[2m])) / sum(rate(http_requests_total{app="api"}[2m]))
      below: 0.01
    every: 1m
    count: 5
    failures: 1
  worker-errors:
    cloudflare: { metric: errors-per-request, version: canary, below: 0.005 }
  lambda-errors:
    cloudwatch: { namespace: AWS/Lambda, metric: Errors, dimensions: { FunctionName: thumbnails }, stat: Sum, below: 1 }
```

Environments name their targets in `.kiln/environments.yaml`. That file's model is ci.md W15's;
CD adds `targets`:

```yaml
staging:
  targets:
    aws-ecs: { account: aws-staging, region: us-east-1 }
    aws-lambda: { account: aws-staging, region: us-east-1 }
    cloudflare-workers: { account: cloudflare, worker-suffix: -staging }
production:
  approvals: { reviewers: [ada] }
  after: staging
  soak: 30m
  targets:
    aws-ecs: { account: aws-prod, region: us-east-1 }
    aws-lambda: { account: aws-prod, region: us-east-1 }
    cloudflare-workers: { account: cloudflare }
    vercel: { account: vercel }
```

- **Per environment.** `${{ env.name }}` and the environment's own values cover most differences.
  An app's `environments:` map overrides any key for one environment.
- **The platform's own files.** When `source:` names a directory holding the platform's file,
  kiln reads it as the platform's tool would, so an app moves to kiln without rewriting. The files:
  `wrangler.jsonc`, `fly.toml`, `vercel.json`, `netlify.toml`, `render.yaml`, `railway.json`,
  `app.yaml`, `.do/app.yaml`, task definition JSON, SAM templates, `Dockerrun.aws.json`,
  `compose.yaml`, Nomad jobs. kiln's keys add only what the file can't say: artifacts, rollouts,
  environments.
- **Validation.** Each provider's schema drives validation in the editor and in `kiln cd
  validate`, at exact positions, as CI's workflow validation does (ci.md W02).

## 5. Rollouts and verification

**Strategies.**

| Strategy | What happens |
| --- | --- |
| `all-at-once` | the default |
| `rolling` | batches with a maximum unavailable, for instance-based targets |
| `blue-green` | the new version starts beside the old; traffic switches after its checks pass; the old version stays for a while (`keep`) |
| `canary` | steps |
| `linear` | equal steps, shorthand |

**Steps.**

| Step | What it does |
| --- | --- |
| `weight` | the share of traffic for the new version |
| `pause` | a duration, or until someone promotes |
| `analysis` | a named template passes |
| `check` | an HTTP check, a CI workflow (smoke tests, W15-R06), or an attached agent's verdict (W15-R03 custom gates) |
| `approve` | a person approves before the next step |

**Analysis** reads Prometheus (kiln ships one), CloudWatch, Cloud Monitoring, Azure Monitor,
Cloudflare's Workers analytics and Datadog (W11-R09). HTTP checks run from the server, or from a
runner near the target. A template runs `count` times, `every` interval, and fails after
`failures` misses, as Argo Rollouts' analysis does.

**Failing or aborting** sends traffic straight back to the stable version, the platform's way:
alias weight to 0, the deployment back to 100% of the old version, revision traffic back. kiln
then records the sync as failed. With `auto-rollback: true` (the default for canary and
blue/green), the app holds at its previous deployment, as a rollback does in cd.md, until git
changes it again.

**The rollout panel** shows during a rollout, on the app's page:
- the weights, as a bar per version;
- the steps, with each analysis result;
- **Promote**, **Pause** and **Abort**, in place.

**Native or orchestrated** (D4):

| Platform | Its own mechanism, which kiln drives and shows | kiln orchestrates |
| --- | --- | --- |
| ECS | rolling with the circuit breaker; blue/green, linear, canary (where available) | weights over two services' ALB target groups |
| Lambda | alias weights, moved per step | — |
| Beanstalk | its deployment policies, traffic splitting | — |
| EC2 | Auto Scaling instance refresh with checkpoints | — |
| Cloud Run, App Engine | traffic percentages, traffic splits | — |
| Compute Engine | managed instance group canaries | — |
| Container Apps | revision weights | — |
| App Service, Functions | slot traffic %, swap | — |
| Workers | gradual deployments | — |
| Vercel | rolling releases | promotion and rollback |
| Fly | — | rolling, blue/green, canary over machines |
| Kubernetes | Argo Rollouts, when an app uses it | Gateway API weights (later) |
| hosts, Compose, microVMs | — | batches across hosts |

## 6. Identity and credentials

- **Accounts are integrations.** There is one registry (W11's `ci_integrations`) for jobs and CD.
  Each account is set up once, in Settings → Integrations. kiln generates the provider-side trust
  configuration to paste or apply (W11-R05), and **Verify** runs a real exchange.
- **A sync's credentials, just in time.**
  1. The server describes the sync as a workload (`kiln_ci::oidc::Workload`: repository,
     environment, ref, sha, actor, `source: cd`, app, deployment, trigger).
  2. It mints a kiln ID token (`crate::ci::oidc::mint`).
  3. It exchanges the token through the integration's `mint()`: AWS `AssumeRoleWithWebIdentity`,
     Google Cloud STS then `generateAccessToken`, Azure's `client_assertion`.
- **Subjects.** The subject is `repo:<owner>/<repo>:environment:<env>`, the same as a deploy job's
  in that environment, so one role per environment trusts both. The claims `kiln_source`,
  `kiln_app`, `kiln_deployment` and `kiln_trigger` let a policy tell syncs apart: for example, only
  syncs with `kiln_trigger` other than `self-heal` may delete.
- **Lifetime.** Credentials last minutes. They are cached in memory per account and environment
  until shortly before they expire, and are never written to disk, logged, or sent to a browser.
- **Reachability.** AWS and Azure fetch the issuer's keys over the internet. Either
  `kiln oidc publish --to s3://…` publishes them on every key change (built on this work's AWS
  module), or the public front door serves the issuer's two paths. Google Cloud takes the keys
  inline (`kiln oidc jwks`). Each account's page says what it needs and whether that is met.
- **Platforms without federation.** Cloudflare, Vercel, Railway, Fly, Netlify, Render, Heroku and
  DigitalOcean use scoped tokens. The tokens live in W09's secret store, bound to environments.
  Each account's page links the platform's token page and lists the fewest scopes the provider
  needs. For Cloudflare, a broker Worker in your account can mint short-lived tokens instead (ci.md
  Appendix C).
- **Clusters in clouds.** EKS takes a token from STS; GKE and AKS take access tokens from their
  account. Any cluster with structured authentication can trust kiln's issuer directly.
- **Runtime secrets.** An app's secrets are references into the platform's store: Secrets Manager,
  Secret Manager, Key Vault, Workers secrets, Vercel environment variables. kiln manages them as
  objects whose values come from W09 secrets. They are masked in diffs and plans, and compared by
  hash.
- **Least privilege.** Each provider documents the permissions it needs: read alone for shadow
  mode, write for syncs. Setup generates the policy for each.

## 7. Artifacts

- **From CI.** `artifacts: { name: { workflow, artifact | image } }` names an output of the
  workflow's run for the revision's commit. Images come by digest from the registry the run pushed
  to; files come from W08's artifacts.
- **From a registry.** `image: { ref: ghcr.io/org/api:1.4 }` resolves to a digest when the
  revision is made, then stays pinned.
- **Where the platform can read them.**
  - Lambda zips and Beanstalk bundles go to an S3 bucket kiln keeps per account, under
    content-addressed keys.
  - Worker bundles upload directly.
  - Vercel and Netlify take files by hash, so only missing files upload.
  - An image goes to the platform's registry (ECR, Artifact Registry, ACR) by digest when the
    platform can't pull from where CI pushed it.
- **Provenance.** With `require-provenance` (W15-R03, W18), an environment takes only artifacts
  whose provenance shows kiln built them from that commit.

## 8. The UI

### 8.1 Deployments, for every target

- **The fleet list** holds every provider's apps in one list. Each shows its provider and target
  (`aws-ecs · us-east-1 · production`), sync, health and revision. Filters gain provider, account
  and environment.
- **An app's page** keeps its head and actions:
  - the tree shows the provider's objects by their platform names: "Service api", "Task
    definition api:42", "3 tasks";
  - an object opens with its summary, live and desired forms, diff, events and logs, as pods do,
    plus a link to it in the platform's console;
  - the rollout panel (§5) shows while a rollout runs.
- **The environments board** (W15-R09) has a row per environment. Each app shows its revision,
  health, and what waits: an approval, a window, a soak, an analysis.

### 8.2 The builder

**New app**, on Deployments, opens in place: one column that grows as you answer, with no wizard
dialogs.

1. **Where.** Pick the environments: existing ones, or `dev`, `staging` and `production` made in
   one step (W15-R08). Give each a target: the platform, the account and the region or project.
   Platforms without a connected account show **Connect**, which opens that integration's page
   and returns here.
2. **What runs there.** kiln lists what the account holds: ECS services, Lambda functions, Workers,
   Vercel projects, Cloud Run services and so on. Pick one, or **New**. A picked resource fills the
   definition from what runs now, which is adoption.
3. **What it deploys.** Pick a workflow and one of its outputs (recent runs' artifacts and images
   are listed), or an image in a registry.
4. **How it rolls out.** The strategy defaults to what suits the platform. Steps are a list.
   Analysis comes from templates (error rate, latency, the platform's errors) with their
   thresholds. Hooks: a migration task, a smoke-test workflow.
5. **Automation and gates.** Auto-sync, self-heal and prune. The environment's approvals and windows
   are shown here and edited on the environment's page.
6. **Review.** The YAML it makes, in the editor: highlighting, schema validation and completions
   from the account's own resources. Edits there flow back into the form. Below it is the
   **plan**: per environment, what a sync would do right now.
7. **Save.** **Save to repo** opens a diff (or a pull request upstream) with the
   `.kiln/deploy.yaml` and `.kiln/environments.yaml` changes. **Keep in kiln** keeps the app in
   kiln instead, marked as such. The URL holds a draft, so a half-made app survives a reload.

### 8.3 Import and adopt

- **From an account.** Choose what to import from what runs. kiln writes their definitions as
  they run now and starts them in **shadow mode**: it reads and compares but never writes. It
  shows whether its view agrees with the platform's, then offers **Take over**, as with Argo CD.
- **From a repository.** kiln finds the platform files (`wrangler.jsonc`, `fly.toml`,
  `vercel.json`, …) and proposes apps.
- **From Argo CD**, as cd.md describes.

### 8.4 Phones

Everything works at phone width:
- the tree as an indented list;
- the rollout panel stacked;
- the builder in its one column;
- an approval in one tap. A gate you can pass from your phone is a large part of why approvals
  belong in kiln.

## 9. API, CLI and MCP

| Method | Path | |
| --- | --- | --- |
| GET | `/api/cd/providers` | the catalog: kinds, capabilities, schemas |
| GET | `/api/cd/targets` | targets per environment, with each account's state |
| GET | `/api/cd/discover?integration=&provider=&place=` | what an account holds (for the builder and import) |
| POST | `/api/cd/validate` | a definition → errors at positions, and its plan per environment |
| POST | `/api/cd/import` | from an account, a repository or Argo CD → apps in shadow mode |
| GET | `/api/cd/apps/:app/rollout` | the rollout: weights, step, analysis results |
| POST | `/api/cd/apps/:app/rollout/{promote,pause,abort}` | |

CLI: `kiln cd discover`, `kiln cd import --from aws-ecs --integration aws-prod`, `kiln cd
validate`, `kiln cd rollout promote|pause|abort <app>`. MCP gets the same operations (code mode)
and skills for deploying to a new platform, adopting from an account, and running a canary.

## 10. Moving kiln's own deploys

**Today:**
- Argo CD, in the cluster, syncs four apps (`kiln-root`, `kiln-namespaces`, `kiln`,
  `kube-prometheus-stack`) from the in-cluster git server.
- The kiln image is built by hand, loaded into minikube's nodes with `docker load`, and the
  Deployment restarted (`deploy-image.sh`). Nothing records it.
- The only gate is "a person runs the script".

**Then:**

1. **Registry.** Use minikube's registry addon until CI's W12 (D7). A workflow builds the image on
   every push to main, with `KILN_COMMIT`, pushes it by digest, and leaves the digest as its
   artifact.
2. **Definitions.**
   - `.kiln/deploy.yaml` defines the apps: kiln's chart with `image: ${{ artifacts.image }}`, the
     namespaces, and kube-prometheus-stack pinned. No root app is needed.
   - `.kiln/environments.yaml` defines `production` as the minikube cluster (in-cluster), with
     you as its approver (D6).
3. **Take over.** kiln already reads the four apps in shadow mode and agrees with Argo CD on
   them. It takes them over, then Argo CD and the in-cluster git server are removed: kiln reads
   its own repository.
4. **The flow.**
   1. A push lands.
   2. CI builds the image, and the revision is ready.
   3. Production waits for your approval.
   4. You approve, from the UI or your phone.
   5. kiln syncs, and restarts into the new version.
5. **Self-deploy safety.** A sync that changes kiln's own Deployment can't watch its own restart.
   - Before applying, kiln creates a **guard** Job (a hook, running the *previous* kiln image).
   - The guard waits for the new server to become ready within a deadline. If it doesn't, the
     guard puts the previous image back and records why.
   - When the new server starts, it finds the sync it inherited and finishes it, as restart
     safety already allows.
   - `deploy-image.sh` stays as break-glass, documented.
6. **Runners** keep following the server's build (releases.md). A later `kiln-runners` provider
   could make that a CD rollout (batches, health per runner), outside this work.

**Done when:**
- a push to main reaches production after one approval from a phone, with the deployment, its
  diff and its logs in the UI;
- a broken image (a fixture whose server exits at start) is rolled back by the guard within the
  deadline, with no manual step;
- Argo CD is gone from the cluster.

## 11. Finishing CD's core

What cd.md's status table leaves open that this work needs, in the order it's needed:

1. `.kiln/deploy.yaml` read from kiln's repositories at each revision (it is parsed today but not
   read from them), and environments with targets.
2. Revisions with artifacts (§1.3), resolved from CI runs.
3. Environments: promotion (`after`, `soak`), approvals and windows, built with CI's W15 half.
4. Notifications through W11-R06 (Slack, Discord, email, webhooks): syncs, failures, approvals
   waiting.
5. Projects and capabilities, minimal (W10 principals): who may sync and approve which
   environment.
6. Runner mode, for clusters and targets only a private network reaches.
7. Resource actions (restart, scale, Promote and Abort a rollout), and the tree over a WebSocket.
8. Replication of CD's state across servers: still later (one server runs today).

## 12. Plan of work

| | What | Done when |
| --- | --- | --- |
| **M0** | kiln deploys itself: §11 items 1 to 4; the registry; the self-deploy guard; take-over from Argo CD | §10's criteria |
| **M1** | The provider framework: both halves, polling, budgets, tracking, health, tree, logs, events, discovery. W11's exchanges (AWS, Google Cloud, Azure `mint()`; token kinds). Providers `aws-ecs`, `aws-lambda`, `cloudflare-workers` (with Durable Objects), `vercel`. The builder (§8.2), import and shadow mode (§8.3) | per provider, against a real sandbox account: create, change, roll back, detect drift, shadow then take over |
| **M2** | Rollouts and analysis (§5), with the rollout panel. Providers `gcp-run`, `gcp-functions`, `azure-containerapps`, `azure-functions`, `azure-appservice`, `host`, `compose` | a canary that fails its analysis rolls back, on each platform with traffic weights |
| **M3** | `aws-ec2`, `aws-beanstalk`, `aws-static`, `gcp-appengine`, `gcp-compute`, `azure-vmss`, `azure-static`, `cloudflare-pages`, `netlify`, `fly`, `railway`, `render`, `heroku`, `digitalocean`, `nomad`, `microvm` | per provider, as M1 |
| **M4** | `cloudformation`, `terraform` and OpenTofu, `pulumi`, `job` (D3); Argo Rollouts; Gateway API weights; previews on every provider that has them | plans show each tool's changes; a failed apply is recorded with its output |

## 13. Shared with ci.md

Agreed with the Kiln CI work on 2026-10-18:

| Part | Built by | Notes |
| --- | --- | --- |
| Tokens for CD syncs (W10-R11) | CI, built | `kiln_ci::oidc::Workload`, `workload_claims`, `crate::ci::oidc::mint`, `issuer`. A deploy job's subject, plus `kiln_source`, `kiln_app`, `kiln_deployment`, `kiln_trigger`. To settle: the issuer's documents are served only with `[ci] enabled = true`; they must also be served whenever an OIDC account exists. |
| `kiln oidc jwks`, `kiln oidc publish --to <dir>` (W10-R12) | CI | |
| `kiln oidc publish --to s3://…` | CD | on the AWS module |
| W11: the registry (`ci_integrations`), server-side `mint()` for AWS, Google Cloud and Azure, token kinds through W09's store | CD, now | in `crates/kiln/src/ci/integrations/`; W11 marked in progress on ci.md's board. CI then builds the job side on it: `kiln/auth@v1`, and secrets `from: integration:…` |
| W15: the environment model, protection, approvals, the job binding, deployment records, `kiln/deploy@v1` | CI | `kiln/deploy@v1` asks CD to sync, for every kind |
| W15: targets and providers for every kind, and the `job` provider | CD | |
| W12: the built-in registry | CI, later | CD uses minikube's registry until then |

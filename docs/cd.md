# Deployments

> **Draft spec, being built on the `cd` branch.** The [status](#status) table at the end says
> what runs today. Everything else here is the design it is built to.

kiln deploys what git says. It renders each application from its source at a commit, compares
the result with what is running, and makes the two match, on Kubernetes clusters anywhere, from a
kiln that runs anywhere: bare metal, a VM, a container or a pod.

It does what Argo CD does and reads Argo CD's own `Application`, `ApplicationSet` and
`AppProject` manifests. It is one part of one binary, with no CRDs, no Redis, no repo-server pods
and no database server. Its state is replicated between kiln servers with hyper-raft. A cluster is
reached directly, through a kiln runner inside it, or by a runner that reconciles it itself.

## Concepts

| Term | Meaning |
| --- | --- |
| **Application** (app) | One source rendered into one destination. |
| **Source** | A repository, a path, a revision and an engine: `helm`, `kustomize`, `jsonnet`, `directory` or a `plugin`. An app may have several (Helm chart + values from another repo). |
| **Destination** | A cluster and a default namespace. |
| **Environment** | A named stage (`staging`, `production`) with its clusters, approvals, windows and promotion order, from trunk's `.kiln/environments.yaml` ([review.md](review.md)). |
| **Project** | Who may deploy what, from where, to where. |
| **Fleet** | A generator that makes apps: one per cluster, directory, diff, pull request… (Argo CD's ApplicationSet). |
| **Sync** | An operation that applies an app's rendered state, in order, and prunes what git no longer holds. |
| **Deployment** | The record a finished sync leaves: revision, who or what started it, what changed, the result. |
| **Plan** | What a commit, a diff version or a pull request would change, per app and environment, before it lands. |
| **Preview** | An app made for a diff, a stack or a pull request, and removed when it lands or closes. |

## Defining applications

### kiln's format: `.kiln/deploy.yaml`

App definitions are code: kiln reads them from trunk at the commit being deployed, and changing
them is a reviewed diff whose [plan](#stacks-diffs-and-pull-requests) shows the effect.

```yaml
apps:
  web:
    source: deploy/web                 # a path in this repository; the engine is detected
    helm:
      values: [values.yaml, "values-${{ env.name }}.yaml"]
      set: { image.tag: "${{ git.short_sha }}" }
    to: [staging, production]          # environments; or { cluster, namespace }
    namespace: web
    sync: { auto: true, prune: true, self-heal: true }

  api:
    sources:
      - { repo: hyper-light/charts, path: api, ref: v2.3.1 }
      - { repo: hyper-light/api-config, ref: main, as: values }   # $values/... in value files
    helm: { values: [$values/prod.yaml] }
    to: production

fleets:
  regional:                            # one app per production cluster
    each: { clusters: { labels: { env: prod } } }
    app:
      name: "web-${{ cluster.name }}"
      source: deploy/web
      helm: { values: ["regions/${{ cluster.labels.region }}.yaml"] }
      to: { cluster: "${{ cluster.name }}", namespace: web }
    rollout: { steps: [{ labels: { region: canary } }, { max: 25% }] }

previews:
  web:
    on: [diff, pull_request]           # one preview per stack (top diff), or per: diff
    source: deploy/web
    helm: { set: { image.tag: "${{ git.sha }}" } }
    to: { cluster: dev, namespace: "preview-${{ preview.id }}" }
    ttl: 72h
```

`${{ }}` is the expression language of [pipelines](pipelines.md#expressions), with `env`,
`cluster`, `app`, `diff`, `stack`, `preview` and `git` in scope.

### Argo CD manifests

kiln reads `argoproj.io/v1alpha1` `Application`, `ApplicationSet` and `AppProject` objects:

- **found in a source** (app-of-apps): a rendered `Application` becomes a child app of the app
  that rendered it. It is not applied to the cluster, so no Argo CD CRDs are needed.
- **imported**: `kiln cd import` reads them from files, from a cluster (`--context`), or from a
  running Argo CD (`--argocd https://…`), with their clusters, repositories and projects.

Every field Argo CD documents keeps its meaning. Settings that Argo CD keeps in `argocd-cm`
(resource customizations, exclusions, tracking method) are kiln settings.

### Through the API

`POST /api/cd/apps` creates an app that lives in kiln rather than in git, as `argocd app create`
does. The UI marks such apps and offers "Save to repo", which turns one into a diff.

## Rendering

| Engine | Detected by | Notes |
| --- | --- | --- |
| `helm` | `Chart.yaml` | values files (also `$ref` from another source), `values` object, `set`, `set-file`, release name, namespace, `skip-crds`, kube version and API versions of the destination. `helm.sh/hook` annotations become sync hooks. Charts from git, HTTP repositories and OCI registries. |
| `kustomize` | `kustomization.yaml`, `.yml`, `Kustomization` | images, name prefix and suffix, common labels and annotations, replicas, patches, components, `helmCharts`. |
| `jsonnet` | `*.jsonnet` in a directory source | TLAs, ext vars, library paths. |
| `directory` | anything else | YAML and JSON documents; `recurse`, `include`, `exclude` globs. |
| `plugin` | `plugin:` on the source | Argo CD's config management plugin spec (`init`, `generate`, `discover`, parameters). It runs as a job on a runner with the labels it asks for, so no sidecar is needed. |

Every render is **content-addressed**. Its key is the git tree hash of the source path, the tree
hashes of everything the render reads outside that path (value files from other sources,
Kustomize bases, Helm dependencies), the parameters, the engine version and the destination's
capabilities. A commit that doesn't touch an app's inputs doesn't re-render it: in a monorepo of
10,000 apps, a commit to one app renders one app. The cache is shared, so 500 apps rendered from
one chart and one values file render once. kiln's native engines run in-process, on every core,
without forking `helm` or `kustomize`. A source may pin `engine: { helm: binary }` to use the
`helm` executable instead.

Each step sees Argo CD's build environment (`ARGOCD_APP_NAME`, `ARGOCD_APP_NAMESPACE`,
`ARGOCD_APP_REVISION`, `KUBE_VERSION`, `KUBE_API_VERSIONS`, …) and kiln's own (`KILN_APP`,
`KILN_ENVIRONMENT`, `KILN_SHA`).

## Comparing

**Tracking.** kiln marks every object it applies with the label `kiln.dev/app: <hash of the app id>`
and the annotation `kiln.dev/tracking: <app>:<group>/<kind>:<namespace>/<name>`. The label lets
it watch only its own objects, filtered by the API server; the annotation is checked, so a copied
label claims nothing. It reads Argo CD's tracking (label, annotation, or both, with the
installation id) to adopt what Argo CD deployed.

**Objects two apps deploy.** As in Argo CD, every app compares everything it renders, whatever
other apps render the same objects: the tracking an object carries says which app it belongs
to. An app whose object carries another app's tracking shows it out of sync ("app X's tracking
is on it: a sync takes it over") and counts it as shared, Argo CD's `SharedResourceWarning`; a
sync of it takes the object over, and `FailOnSharedResource=true` fails that sync before it
applies anything. kiln goes further where Argo CD loses objects:
- an app prunes only what carries its tracking, and never while another app deploys it, so one
  app dropping an object can't delete it from under another;
- deleting an app with its objects leaves those another app deploys;
- automatic syncs and self-heal never take an object from another app (two apps would take it
  from each other for ever, as two Argo CD apps do): a person's sync takes it over.

Taking an app over from Argo CD is this case: Argo CD's app (shadowed) and kiln's deploy the
same objects; kiln's shows them as Argo CD's, and says whether a sync changes anything but the
tracking marks.

**Diffs.** By default kiln applies with server-side apply, so the API server records which fields
kiln owns. An object is out of sync when a field kiln sets differs from the live value, a field
kiln sets is missing, or kiln owns a field it no longer sets. Fields other managers own (the HPA's
`replicas`) are never drift unless kiln sets them too. Quantities, ports and empty values are
compared by meaning, not spelling. When that can't decide (mutating webhooks, defaults kiln
doesn't know), kiln asks the API server for a dry-run apply and compares the result. The answer is
cached by the desired object's hash and the live `resourceVersion`, so it costs one call per change,
not one per refresh. Objects applied before kiln (adopted from Argo CD or `kubectl apply`) are
compared three ways against `last-applied-configuration` until kiln first applies them.

Equality is checked on hashes first. Each object's normalized desired form and live form are
hashed; a watch event or a new render recompares only the objects whose hashes moved. A
human-readable diff is computed only when someone asks, and it shows what a sync would change:
kiln predicts the live object after the apply (what it sets laid over what runs, what it no longer
sets dropped) and diffs the two, so defaults the cluster filled in never show as differences.
Secrets are masked alike on both sides, so a changed value still shows as changed.

**Ignoring differences.** `ignore` rules take JSON pointers, a jq-style path subset
(`.spec.template.spec.containers[] | select(.name == "app") | .image`) and field managers, per
app, per project or globally, as Argo CD's `ignoreDifferences` and `resource.customizations` do.
`RespectIgnoreDifferences=true` keeps ignored fields out of the apply too.

**Health.**

| Status | Meaning |
| --- | --- |
| `Healthy` | doing what it should |
| `Progressing` | not yet healthy, still able to become so |
| `Degraded` | failed, or past its progress deadline |
| `Suspended` | paused or waiting (a suspended CronJob, a paused rollout) |
| `Missing` | in git, not in the cluster |
| `Unknown` | no rule could tell |

kiln has built-in rules for Deployments, StatefulSets, DaemonSets, ReplicaSets, Pods, Jobs,
Services, Ingresses, PersistentVolumeClaims, HorizontalPodAutoscalers and APIServices, matching
Argo CD's. Before them, in Argo CD's order, come Lua scripts: a setting's for the kind (Argo CD's
`resource.customizations.health.<group>_<kind>`, `*` allowed in the key), then the script Argo
CD's resource customizations library has for it, then the library's wildcard scripts
(`*.crossplane.io/*`). The library ships inside kiln (294 scripts: cert-manager, Argo Rollouts,
Prometheus Operator, Crossplane, Istio, Strimzi, KEDA and more), and runs as Argo CD runs it, in
Lua with Argo CD's libraries, the object as `obj`, and a second per script; kiln's tests run Argo
CD's own expectations for every script (1,387 objects, status and message alike). A kind with no
rule has no health and doesn't count toward its app's, as in Argo CD. An app's health is the
worst of its resources', in Argo CD's order from worst: Unknown, Degraded, Missing, Progressing,
Suspended, Healthy.

## Syncing

A sync applies the app's objects in **phases** (`PreSync`, `Sync`, `PostSync`; `SyncFail` when
one fails; `PostDelete` when the app is deleted), and in **waves** within a phase
(`argocd.argoproj.io/sync-wave`, or `kiln.dev/wave`). Within a wave, Namespaces, CRDs and the
other kinds Argo CD orders first go first. A wave starts only when every object of the wave
before it is healthy.

**Hooks** are objects annotated `argocd.argoproj.io/hook` (or `helm.sh/hook`), usually Jobs.
They run in their phase and are deleted as `hook-delete-policy` says (`HookSucceeded`,
`HookFailed`, `BeforeHookCreation`, the default).

**Options** use Argo CD's names, on the app, the sync or one object
(`argocd.argoproj.io/sync-options`):

| Option | Effect |
| --- | --- |
| `Prune=true` / `false` / `confirm` | delete what git no longer holds; `false` on an object keeps it; `confirm` waits for a person |
| `PruneLast=true` | prune after every wave has synced |
| `PrunePropagationPolicy=foreground` / `background` / `orphan` | how deletes cascade |
| `CreateNamespace=true` | create the destination namespace (with `managedNamespaceMetadata`) |
| `ServerSideApply=true` / `false` | kiln's default is `true`; `false` uses client-side apply with `last-applied-configuration` |
| `Replace=true`, `Force=true` | replace instead of apply; delete and recreate when replace fails |
| `ApplyOutOfSyncOnly=true` | apply only what differs |
| `RespectIgnoreDifferences=true` | keep ignored fields out of the apply |
| `SkipDryRunOnMissingResource=true` | skip the dry run for kinds whose CRD the same sync creates |
| `Validate=false` | no schema validation |
| `FailOnSharedResource=true` | fail, before applying anything, when an object the app deploys carries another app's tracking |
| `Delete=false` | keep the object when the app is deleted |

**Automation.** `sync.auto` syncs when git moves. `self-heal` syncs again when the cluster drifts,
with backoff. `prune` and `allow-empty` mean what Argo CD's `automated` fields mean. An automatic
sync of a revision that already failed is not repeated until the revision or the spec changes.
`retry: { limit, backoff: { duration, factor, max } }` retries a failed sync.

**Fairness.** Each cluster has a request budget (QPS and burst) and a limit on concurrent syncs.
Syncs queue by priority (people, then rollbacks, then promotions, then automatic syncs, then
self-heal), oldest first within a priority, and one app can never hold more than its share. When
a sync waits, it says why: the cluster's budget, a window, an approval, a concurrency limit.

**Rollback.** "Roll back to deployment N" renders that deployment's commits again and syncs them.
The app then holds there (a pin): automation keeps it at that deployment, self-heal included,
until git changes what the app renders. Then it follows git again by itself, so nobody has to
remember to turn automation back on, as Argo CD makes you. A commit elsewhere in the repository
doesn't count, nor does a cluster upgrade; `follow` lets go sooner. A sync asked for while a
render is in flight waits for it, so a rollback never deploys the render it replaced. In a
repository kiln hosts, rollback will also open the revert as a diff, so trunk catches up with
what runs (not yet).

## Environments and promotion

Environments come from trunk's `.kiln/environments.yaml`, the file pipelines already use for
approvals and secrets. Deployments add a few keys:

```yaml
staging:
  branches: [main]
  clusters: [dev]
production:
  branches: [main]
  approvals: { reviewers: [ada], count: 1 }
  clusters: [prod-us, prod-eu]
  after: staging                      # promote only what is healthy in staging
  soak: 30m                           # …for at least this long
  windows:
    - { deny: "0 0 * * 6,0", duration: 48h, timezone: UTC }   # no weekend deploys
  concurrency: 1
```

An app that deploys `to: [staging, production]` becomes one app per environment. Production
syncs a revision only after staging has run it healthy for `soak`, its approvals are in, and no
window denies it. The environments page shows every app's revision in each stage and what is
waiting to move. **Windows** follow Argo CD's sync windows: `allow` / `deny`, cron, duration,
timezone, which apps, namespaces and clusters, and whether manual syncs may pass.

## Stacks, diffs and pull requests

kiln reviews code as stacks of diffs ([review.md](review.md)) and as GitHub pull requests.
Deployments take part in both.

- **Plans.** Every diff version gets a plan: the apps its files affect, each one's rendered
  manifests diffed against trunk, and against what runs in each environment. A diff's plan shows
  its **own change**, rendered at its commit against the commit of the diff below it, and the
  **stack's change** against trunk. Plans are cheap (renders are cached by content), so a
  100-diff stack is planned in the time its changed apps take to render. The plan is a check
  (`deploy`) that `.kiln/review.yaml` can require. It fails when a render fails, a policy denies
  the change, or it would prune an object marked `Prune=false`.

  What is built plans a commit (against its first parent), a stack (`head`, from where it leaves
  trunk or from `base`, at most 100 commits) or a pull request (its `refs/pull/<n>/head`, or a
  GitLab merge request's, fetched). Finding the apps renders nothing: an app's render key is the
  content address of the paths it reads, so an app is planned only if a commit changes one of
  them, and only its sources that follow trunk move. Each distinct content is rendered once and
  kept by key, so planning again renders nothing. The engine then plans each render against what
  runs, with the sync's own planner: the objects a sync would create, change (live and predicted,
  with `why` and hunks; Secrets masked), prune, and the hooks it runs, in its order (phases,
  waves). Objects of kinds the engine hasn't listed yet are read from the cluster for the plan.
  A diff fails when its sources can't be read or rendered, or when it drops an object that
  `Prune=false` keeps in the cluster. Its notes say what to know: `Prune=confirm`, an object
  another app deploys, a kind the cluster doesn't serve. `brief` leaves out the hunks.
  Plans of kiln's own diffs (`D123`, once its review is built), the land queue's candidates,
  commit statuses and comments on pull requests, and policy checks are not built yet.
- **Land.** The land queue's candidate is planned like a diff. A failure ejects it before trunk
  moves.
- **Previews.** `previews:` makes an app for each stack (by default, at its top diff), for each
  diff (`per: diff`), or for each pull request. A new version updates it. A **rebase-only**
  version whose renders are unchanged doesn't redeploy it. Landing, abandoning or closing removes
  it, and so does its `ttl`.
- **Pull requests.** For repositories that live upstream on GitHub, pull requests get the same
  plans (as a commit status and, with a token, a comment) and previews.
- **Agents.** A failed plan or preview wakes the author's agent like a failed run does
  ([ci-agents.md](ci-agents.md)).

## Fleets

A fleet makes apps from a generator and a template. These are Argo CD's ApplicationSet
generators, plus two for kiln's own review:

| Generator | One app per |
| --- | --- |
| `list` | element of a list |
| `clusters` | cluster matching labels |
| `git: { directories }` / `git: { files }` | directory, or JSON/YAML file, matching globs at a revision |
| `matrix`, `merge` | combination of two generators, or their merge on keys |
| `diffs` | open diff (or stack) in a kiln repository |
| `pull_requests` | open pull request (GitHub, GitLab, Gitea, Bitbucket, Azure DevOps) |
| `scm` | repository in an organization |
| `plugin` | element a plugin job prints |

Templates accept `${{ }}`. Imported ApplicationSets keep Go templates or fasttemplate. The fleet
policy says whether kiln creates, updates and deletes the apps (`create-only`, `create-update`,
`create-delete`, `sync`). A **rollout** moves a change through the fleet in steps, with
label-selected groups, a maximum in flight, and a health gate between steps (Argo CD's progressive
syncs). A fleet's apps are stored as the template plus each app's parameters, so a fleet of
100,000 apps costs little more than its parameters.

## Clusters

| Mode | How kiln reaches the API server | Use when |
| --- | --- | --- |
| `direct` | over the network, with the cluster's credentials | kiln can reach the API server |
| `runner` | through a kiln runner in the cluster, over the runner's QUIC connection | the API server is private; nothing listens for kiln |
| `delegated` | the runner in the cluster watches it, applies, and sends kiln compact changes | many or large clusters; the edge does the work |

Credentials: a kubeconfig context (tokens, client certificates, `exec` plugins such as EKS, GKE and
AKS logins), a service-account token, or the runner's own in-cluster identity. When kiln runs in
a cluster, that cluster is `in-cluster` by default. `kiln cd cluster add <context>` creates a
service account with the role kiln needs, as `argocd cluster add` does. Each cluster has labels
(for fleets and environments), a request budget, and the namespaces kiln may touch.

kiln watches only the kinds its apps contain, filtered to its own label, plus the kinds those
objects own (ReplicaSets, Pods), as metadata, for the resource tree. Watches resume from their
last `resourceVersion` after a disconnect, so a reconnect costs no full list.

## Notifications and webhooks

Triggers (`on-deployed`, `on-sync-failed`, `on-health-degraded`, `on-sync-running`,
`on-sync-succeeded`, `on-created`, `on-deleted`, custom conditions) send templates to services
(webhook, Slack, email, GitHub commit status, Teams, Telegram, PagerDuty, Opsgenie), as Argo CD's
notifications do, and Argo CD's subscription annotations work. Every change is also a kiln event
(`/api/events`) and can feed outgoing webhooks ([access.md](access.md)). Git webhooks from GitHub,
GitLab, Bitbucket and Gitea refresh only the apps whose paths the push touched.

## Agents

Agents use deployments the way people do, through the same API and the same rules.

- **MCP.** kiln's MCP server (`/api/mcp`, and `kiln mcp` over stdio) offers **code mode**:
  `code.search` runs a program over the tool registry, and `code.run` runs a program that calls
  tools (`await kiln.cd.apps.list({ health: "Degraded" })`) in a bounded sandbox inside kiln. Only
  what the program returns enters the agent's context, so "every degraded app in production with
  its failing pods' last events" is one call, whether there are 10 apps or 100,000. Lists are
  async iterators that page on the server. Bulk operations (`kiln.cd.apps.sync({ selector })`)
  are single, idempotent operations. The same tools are offered one by one for clients without
  code mode.
- **Skills.** kiln ships skills for deploying a change, diagnosing a degraded app, rolling back,
  writing app definitions, running fleets and moving from Argo CD. They are served over MCP (the
  skills extension) and installed into every agent kiln starts.
- **Wake events.** Agents attached to an environment or an app ([ci-agents.md](ci-agents.md)) wake
  on `degraded`, `sync-failed`, `drift`, `stuck` (a sync waiting past a limit) and `plan-failed`.
  An environment may forbid agents to sync or roll back it (`agents: { sync: false }`).

## API, CLI and UI

The API lives under `/api/cd`. It follows [access.md](access.md)'s conventions: cursor pagination,
`Idempotency-Key` on POSTs that start things, `{ error, code }` errors.

| Method | Path | |
| --- | --- | --- |
| GET | `/api/cd/apps?project=&env=&cluster=&namespace=&sync=&health=&q=&sort=&cursor=&limit=` | apps, a page at a time; `sort` is `name`, `health` (worst first), `sync` (out of sync first) or `deployed` (newest first) |
| GET | `/api/cd/facets?…` | how many apps each sync, health, project, cluster and namespace would show |
| POST | `/api/cd/apps` | create (or update) an app kept in kiln |
| GET / PATCH / DELETE | `/api/cd/apps/:app` | spec and status; change; delete (`?cascade=foreground\|background\|orphan`) |
| GET | `/api/cd/apps/:app/tree?fresh=` | the app's objects and every live object they own (ReplicaSets, Pods, ControllerRevisions, Jobs, EndpointSlices), joined on owner uids: each node's uid, parents, health, sync, age, ready counts, restarts, images, containers and a few info pairs |
| GET | `/api/cd/apps/:app/diff` | rendered against live, per resource |
| GET | `/api/cd/apps/:app/manifests` | what the app renders now, as kiln applies it (another revision's: not yet) |
| POST | `/api/cd/apps/:app/{refresh,sync,rollback,follow,terminate}` | |
| GET | `/api/cd/apps/:app/history` | deployments |
| GET | `/api/cd/apps/:app/resources/:key` | one object of the tree: its node, live manifest, desired manifest and diff (managed objects), labels, annotations, conditions and owners. `:key` is `group~kind~namespace~name` |
| GET | `/api/cd/apps/:app/resources/:key/events` | its Kubernetes events, newest first |
| GET | `/api/cd/apps/:app/resources/:key/logs?container=&pod=&tail_lines=&since_seconds=&previous=` | the last lines of a pod's log, or of a workload's pods merged by time; `/logs/ws` follows them over a WebSocket |
| POST / DELETE | `/api/cd/apps/:app/resources/:key/actions/:action` | resource actions (restart, delete a pod): not yet |
| POST | `/api/cd/bulk` | `{ selector, action }` over many apps |
| GET / POST | `/api/cd/plans?repo=&commit=\|head=&base=\|pr=&trunk=&brief=` | a plan of a commit, a stack or a pull request (POST takes the same as JSON, with app filters); `diff=` (kiln's own diffs): not yet |
| GET | `/api/cd/{environments,previews,fleets,windows}` | |
| GET / POST / PATCH / DELETE | `/api/cd/{clusters,repos,projects}[/:id]` | |

`/api/events` adds `cd_app` (a coalesced summary), `cd_operation` and `cd_plan`.
`GET /api/cd/apps/:app/ws` will stream one app's resource tree (not yet: the page reads the tree again
every few seconds and when the app's status changes).

The tree and an object's live form are read from the cluster when someone asks, bounded: the
app's own objects are listed by its tracking label where it has one (one list per kind and
namespace, Secrets as metadata alone), children only in the namespaces where something that owns
such children lives, and lists and trees are kept 4 s and shared by every app and viewer that
asks meanwhile. Only objects in the app's tree can be read this way, and Secret values are masked
everywhere (`last-applied-configuration` included).

CLI: `kiln cd apps`, `kiln cd app <app> [--tree|--diff|--history]`, `kiln cd sync <app>…
[--selector] [--prune] [--dry-run] [--wait]`, `kiln cd rollback <app> [n]`, `kiln cd follow <app>`, `kiln cd plan
[<commit>] [--stack] [--base <commit>] [--trunk <branch>] [--pr n] [--diff]` (by default the
branch checked out here, as a stack), `kiln cd cluster add|list|rm`, `kiln cd import`, `kiln cd bench`.

UI: **Deploy plans** (`/deployments/plans`, linked from Deployments as "Plan a deploy"): choose
a repository and a branch, a commit or a pull request; each commit, in the order they land, lists
the apps it changes with its own change, the stack's against trunk, or a sync's steps (one select
chooses), every object's change as a diff, and why a commit fails. The URL holds what is planned.
**Deployments**: the fleet as one list (sync and health per app, filtered by environment,
cluster and project, 100,000 apps scrolling smoothly), an app's resource tree, diffs, history and
rollback, the environments board, previews by stack, fleets and their rollouts. Each diff's page
gets a **Deploy** panel with its plan.

An app's page does what Argo CD's application view does, in the console's own look. Its head says
the sync and health, the revision with its commit message, author and time, the last sync's
result, and where the app comes from and goes to; its actions (Sync with prune, dry run, Argo
CD's sync options, or only chosen objects; Refresh and Hard refresh; Diff; History and rollback;
Delete) open in the page. When it differs from git, **Why it differs from git** says how, by what
a sync does about it: fields that differ, objects missing, objects carrying another app's
tracking (to take over, and whether anything but the marks changes), objects lacking only kiln's
marks, objects git no longer has. Each kind has its diff and the sync that deals with it, those
objects chosen. The list of apps says the same in a line: `14 changed · 92 to take over · 1 to
prune`.
Below, its objects and every live object they own as a tree (a left-to-right graph on wide
screens, an indented list with guide lines on phones, alike siblings folded into "12 pods"), a
list sortable by kind, name, namespace, health, sync and age, or its pods by owner or node, chosen
with a select. The console's one filter field (kind, health, sync, namespace, names) applies to
every view; in the tree a match keeps its owners in view. An object opens in the page: summary,
live and desired manifests, diff, events, and logs for pods and what runs them (container, pod,
since, lines; follow, wrap, times, previous run, copy).

## State, replication and scale

**What is kept.** Hard state is apps, projects, clusters, repositories (credentials sealed),
operations and the deployment history. It is replicated with hyper-raft through hyper-durable,
written to hyper-log, and survives any minority of servers failing. Soft state is renders, live
objects, comparisons and health. It lives in the memory of the server that reconciles the app, is
rebuilt from git and the clusters after a failover, and is never written per change. Argo CD writes
status into every Application object; kiln writes only what it can't recompute.

**Restarts.** A restarted server judges nothing before it has read the cluster: an object of a
kind not yet listed is *unread*, not missing, and automation waits. An app's render waits for its
cluster too, since Helm's `.Capabilities` come from it. Automation resumes from the replicated
history, so the last sync of what git holds counts as attempted: a restart syncs nothing again,
and drift stays where self-heal is off. "What git holds" leaves the cluster out: only Helm and
plugin renders read its version and API versions, and a render whose key moved only with the
cluster (a CRD installed somewhere) is re-rendered but not synced again, as Argo CD syncs only
new revisions. Failed syncs are kept in the history too, but never push out
successful ones.

**Where work happens.** Apps are split into shards by destination cluster, so one server holds
each cluster's watches. Each shard is a raft group. Its leader reconciles it, and shards move
between servers as they join and leave (hyper-swim membership). One server runs every shard.

**How work is done.** The controller is a state machine with no clock, no disk and no network,
like hyper-raft's core. It is told what happened (git moved, a render finished, an object
changed, an apply answered, time passed) and says what to do. That makes it deterministic: the
benchmarks drive the same controller as production, against simulated clusters, at full speed.
Work is incremental. A commit re-renders only the apps whose inputs it changed, a watch event
recompares one object, and a status changes only when an aggregate does. The periodic full
refresh Argo CD relies on (every 3 minutes) becomes a slow safety sweep, by default hourly.

**Targets**, checked by `kiln cd bench` on every push to main
(the [bench pipeline](../.kiln/bench.yaml)):

| Apps | Resources | Memory | Steady CPU | Commit → synced (1% of apps change) |
| --- | --- | --- | --- | --- |
| 10,000 | 100,000 | < 1 GiB | < 0.1 core | < 5 s |
| 100,000 | 1,000,000 | < 4 GiB | < 0.5 core | < 30 s |
| 1,000,000 | 10,000,000 | < 32 GiB across servers | < 4 cores | < 5 min, bounded by the clusters' request budgets |

## Security

Capabilities follow [access.md](access.md): `cd.read`, `cd.sync`, `cd.rollback`, `cd.manage`
(apps), `cd.clusters`, `cd.secrets`, scoped to a project, an environment or a cluster. Argo CD's
RBAC policies (`p, role:dev, applications, sync, team/*, allow`) are imported as kiln grants.
Projects restrict source repositories, destinations, and the kinds an app may create (cluster-
and namespace-scoped allow and deny lists). An app can apply as a destination service account
(impersonation) instead of kiln's own identity. Repository and cluster credentials are sealed with
hyper-seal at rest and are never returned by the API. Secrets in diffs and manifests are masked.
Every change is audited with its principal, and with the person it acted for.

## Moving from Argo CD

1. `kiln cd import --argocd https://argocd.example.com` (or `--context <kube context>`) copies
   apps, ApplicationSets, projects, clusters and repositories.
2. Imported apps start in **shadow mode**. kiln renders, compares and reports, but doesn't write.
   The import page shows, per app, whether kiln's sync and health agree with Argo CD's, and why
   when they don't.
3. When they agree, take over an app, a project or everything. kiln adopts the objects (Argo CD's
   tracking is read) and disables auto-sync in Argo CD for what it took. Nothing is recreated.

### Measured

`cargo run --release -p kiln-cd --example bench -- --apps N --clusters C --shards S` (also
`--features mimalloc`) drives the engine against simulated clusters that admit 500 requests a
second each, answer in 5 ms and roll Deployments out in 3 s. Three objects per app (a
Deployment, a Service, a ConfigMap), every app distinct, automatic sync with prune and
self-heal, dry runs on. On one 32-thread machine (Ryzen 9 9950X3D2, glibc malloc):

| Apps | Shards | Cold start (simulated · wall · engine CPU) | Steady 10 min (engine CPU) | 1% new revision synced | 0.1% drift healed | Engine memory |
| --- | --- | --- | --- | --- | --- | --- |
| 10,000 | 1 | 13 s · 1.9 s · 1.2 s | 0.10 s | 3.0 s | 3.0 s | 48 MiB |
| 100,000 | 1 | 13 s · 24 s · 15 s | 1.5 s | 3.0 s | 3.0 s | 458 MiB |
| 1,000,000 | 16 | 13 s · 45 s · 407 s | 27 s (0.05 cores) | 3.0 s | 3.0 s | 4.4 GiB |

Cold start is bound by the clusters' request budgets (every object is dry-run, then applied),
not by kiln. Status-only events, the bulk of what a quiet fleet sends, cost one hash and no
comparison. With mimalloc, engine CPU drops by a quarter to two fifths under 16 shards. The
simulated clusters are not real ones: watch-stream parsing (about 7 µs per event), rendering
and the network come on top, and are measured separately as they are built.

## Status

| Part | State |
| --- | --- |
| Engine (`crates/kiln-cd`): health, diff (with predicted results), sync planning (phases, waves, hooks, prunes), controller, retries, self-heal, request budgets, restart safety, plans of renders not deployed | built, tested (91 tests), benchmarked to 1M apps |
| Rendering: directories, Helm (binary), Kustomize (binary), plugins; content-addressed render keys | built; Helm tested on kiln's own charts. Jsonnet and kiln's native Helm and Kustomize engines: not yet |
| Git: mirrors, revisions, path hashes, extraction, symlinks | built |
| Durable state on hyper-raft (one voter) | built; replicas across servers: not yet |
| Clusters: kubeconfig (exec plugins included), in-cluster (the chart's `rbac.deployments`: read, write or none), discovery (again when a CRD appears), watches, server-side apply | built; runner and delegated modes: not yet |
| Argo CD Applications (import, app of apps, tracking, shadow mode, take-over) | built; ApplicationSets and AppProjects: not yet |
| `.kiln/deploy.yaml` and environments | read from kiln's repositories at trunk each round (apps registered when they change, removed with the file); artifacts, published images, agents' rights: [cd-targets.md](cd-targets.md) |
| `/api/cd`, `kiln cd` | built |
| MCP at `/api/mcp` (and `kiln mcp` over stdio): code mode in QuickJS, a tool per operation, skills; Claude Code sessions kiln starts get it | built |
| Rollback: to any deployment kept, held until git changes the app (API, CLI, MCP, UI) | built |
| Plans of a commit, a stack or a pull request: the apps it touches, each diff's own change and the stack's, a sync's steps against what runs, the render and `Prune=false` checks (API, CLI, MCP, and Deployments → Plan a deploy) | built; kiln's own diffs, the land queue, previews, statuses and comments on pull requests, policy checks: not yet |
| UI: the fleet list (windowed, filtered and sorted on the server, live; the console's filter field with counts per value, namespace included; the fleet's counts; row checkboxes for syncing or refreshing many) and an app's page (revision, last sync, source; the resource tree as a graph or an indented list, a list, pods by owner or node; one filter over every view; an object's summary, manifests, diff, events and logs, followed live; sync of chosen objects, rollback, refresh and hard refresh, take over, stop, delete, each confirmed in place) | built; environments board, previews, fleets, resource actions, the tree over a WebSocket: not yet |
| Notifications, windows, projects and RBAC, previews | not yet |

Checked against a live cluster: in shadow mode, kiln agrees with Argo CD on kiln's own app of
apps (four apps, the same sync, health and resource counts). On a cluster of its own, a kiln app
synced through its PreSync hook and waves, followed a commit (with a prune), healed hand edits
in 7 s, and was deleted with its objects while the namespace it created stayed. With self-heal
off, a hand edit survived a restart of kiln untouched (no sync ran), and its diff showed only the
edited fields. An app shipping two CRDs (one namespaced, one cluster-scoped) and an object of
each synced in one go. A four-commit stack over three apps (plain manifests and a Helm chart) planned
in under a tenth of a second from cold: each diff's apps and steps came out in the sync's order
(the PreSync hook first), a new Secret showed as a create with its value masked, the diff that
dropped a `Prune=false` ConfigMap failed, a docs-only diff touched no app, and a broken template
failed with Helm's own error.

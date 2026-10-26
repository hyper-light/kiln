# kiln CI: GitHub Actions compatible, and better

> **Spec, version 1 (2026-10-17).** This is the complete specification of kiln's CI. It replaces
> the CI parts of [pipelines.md](pipelines.md) and folds in, adapted, the drafts in
> [review.md](review.md), [ci-agents.md](ci-agents.md), [access.md](access.md) and
> [experiments.md](experiments.md). Where they disagree with this file, this file wins.
> Deployments stay specified in [cd.md](cd.md); §3.W15 says how CI and CD meet.
>
> It is written so that an agent or a person can pick up **any** workstream, at any point, and
> finish it: every workstream lists its requirements, design, interfaces, acceptance criteria and
> test scenarios. The [status board](#05-status-board) says what is done.

## Contents

- [0. Working on this spec](#0-working-on-this-spec)
- [1. What we are building](#1-what-we-are-building)
- [2. Architecture](#2-architecture)
- [3. Workstreams](#3-workstreams)
  - [W01 Expressions](#w01-expressions)
  - [W02 Workflow model, dialects and validation](#w02-workflow-model-dialects-and-validation)
  - [W03 Triggers and events](#w03-triggers-and-events)
  - [W04 Planner: graph, matrices, reusable workflows, outputs](#w04-planner-graph-matrices-reusable-workflows-outputs)
  - [W05 Scheduler: queues, concurrency, priorities, routing, retries](#w05-scheduler-queues-concurrency-priorities-routing-retries)
  - [W06 Step runtime on the runner](#w06-step-runtime-on-the-runner)
  - [W07 Actions](#w07-actions)
  - [W08 Runtime services: store, caches, artifacts, reuse](#w08-runtime-services-store-caches-artifacts-reuse)
  - [W09 Secrets and variables](#w09-secrets-and-variables)
  - [W10 Identity: tokens, principals, OIDC issuer](#w10-identity-tokens-principals-oidc-issuer)
  - [W11 Integrations and the credential broker](#w11-integrations-and-the-credential-broker)
  - [W12 Registries](#w12-registries)
  - [W13 Runners and executors](#w13-runners-and-executors)
  - [W14 Code hosting, stacked diffs, review and landing](#w14-code-hosting-stacked-diffs-review-and-landing)
  - [W15 Environments and deployments](#w15-environments-and-deployments)
  - [W16 Test results, flaky tests and benchmarks](#w16-test-results-flaky-tests-and-benchmarks)
  - [W17 Debugging](#w17-debugging)
  - [W18 Supply chain and untrusted code](#w18-supply-chain-and-untrusted-code)
  - [W19 Insights, audit and notifications](#w19-insights-audit-and-notifications)
  - [W20 Agents: MCP code mode, skills, attachments, experiments](#w20-agents-mcp-code-mode-skills-attachments-experiments)
  - [W21 UI](#w21-ui)
  - [W22 Conformance, end-to-end and performance testing](#w22-conformance-end-to-end-and-performance-testing)
  - [W23 CI providers](#w23-ci-providers)
  - [W24 Incremental CI](#w24-incremental-ci-fingerprints-reuse-warm-state-and-shared-caches)
  - [W28 Demand-driven CI](#w28-demand-driven-ci-adopt-cover-stagger-land-by-conflict-graph)
- [4. API, events, CLI and MCP reference](#4-api-events-cli-and-mcp-reference)
- [5. Milestones and the order of work](#5-milestones-and-the-order-of-work)
- [Appendix A. GitHub Actions compatibility matrix](#appendix-a-github-actions-compatibility-matrix)
- [Appendix B. Where each context is available](#appendix-b-where-each-context-is-available)
- [Appendix C. Integration catalog](#appendix-c-integration-catalog)
- [Appendix D. Registry catalog](#appendix-d-registry-catalog)
- [Appendix E. kiln dialect reference](#appendix-e-kiln-dialect-reference)
- [Appendix F. Test fixtures](#appendix-f-test-fixtures)
- [Appendix G. Sources](#appendix-g-sources)

---

## 0. Working on this spec

### 0.1 Picking up work

1. Read §0, §1 and §2 in full. They hold the rules, the vocabulary and the shared design every
   workstream builds on.
2. Pick a workstream from the [status board](#05-status-board) whose status is `ready`, i.e. its
   dependencies are `done` or the parts it needs are. Several workstreams can run at once: §5
   lists which ones touch which files.
3. Make a worktree for it from `main`:
   `git worktree add -b ci-<wNN>-<topic> .claude/worktrees/ci-<wNN>-<topic> main`.
4. Implement the workstream's requirements. Every requirement has an ID (`W05-R07`). Cite it in
   commit messages and test names (`fn w05_r07_cancel_in_progress_cancels_running()`), so anyone can
   trace a requirement to its code and its test.
5. Make every acceptance criterion pass (`W05-AC07`). Each one says how it is checked: a unit test,
   an integration test, an end-to-end scenario on a live server, or a UI check in Chromium and
   WebKit.
6. Update your row on the status board (one line, so parallel branches rarely conflict) and the
   workstream's **Progress** line, then hand over: commit on your branch; the coordinating session
   merges to `main` and redeploys (§0.3).

If you stop partway, write what is done and what is next in the workstream's **Progress** line.
The next agent starts from there.

### 0.2 Conventions

- **MUST**, **MUST NOT**, **SHOULD** and **MAY** are used as in RFC 2119.
- IDs: `Wnn-Rmm` is a requirement, `Wnn-ACmm` an acceptance criterion, `Wnn-Smm` a test scenario.
  `G-nn` is a goal from §1.1. Fixtures are `F-nn` in Appendix F.
- "GHA" means GitHub Actions as GitHub documents and runs it on 2026-10-17. "GitHub-exact" means:
  the same input gives the same observable result as on GitHub (value, ordering, error or not).
  Where kiln deliberately differs, the text says **Deviation** and gives the reason.
- Paths are relative to the repository root. `crates/kiln-ci` is the new core crate (§2.2).
- Sizes use binary units (`MiB`). Times are UTC. Durations in config are humane strings (`90s`,
  `15m`, `2h`).

### 0.3 Rules of this repository

These hold for every line written under this spec.

- **No panics.** The workspace denies `unwrap`, `expect`, `panic!`, `todo!`, `unimplemented!`,
  `unreachable!`, unchecked indexing and `exit` (see `Cargo.toml`). Malformed input of any kind
  (YAML, expressions, protocol frames, HTTP bodies, archives) becomes an error value that is shown
  to the user, never a crash.
- **hyper-rt, not tokio.** Async code runs on hyper-raft's `hyper-rt` runtime, QUIC on
  `hyper-transport`/`hyper-quic`, TLS on `hyper-tls`. HTTP is kiln's own `kiln-http` (HTTP/1.1 and
  HTTP/2, server and client, on hyper-rt; [docs/http.md](http.md)) behind `crate::http`. Do not add
  tokio, hyper, h2, axum, reqwest, kube-rs or tokio-rustls. If a capability is missing from
  hyper-rt, say so and ask; do not fall back silently. Runtime-agnostic crates (serde, the `http`
  and `http-body` types, httparse, k8s-openapi types, aws-lc-rs, rquickjs) are fine.
- **Deterministic cores.** Logic that decides (expressions, planning, matrices, scheduling,
  routing, retry policy, concurrency, flaky classification) lives in `crates/kiln-ci` as pure code:
  no clock, no disk, no network, no randomness except what is passed in. The kiln crate feeds it
  events and carries out its commands. This is how `kiln-cd`'s engine is built, and it is what makes
  the scheduler testable at full speed and benchmarkable (W22).
- **SQLite is the system of record** for CI state (`crates/kiln/src/store.rs`, WAL). Schema
  changes are numbered migrations (§2.4). Logs, caches and artifacts never go through SQLite.
- **The protocol only grows.** `proto.rs` is postcard-encoded by variant index: new variants go
  **after** every existing one, and new capabilities are new bits (§2.5). Structured payloads
  that will keep changing travel as JSON inside a variant, as the harness messages already do.
- **General hardware.** Never assume or default to a particular machine. Pick machines by what
  they can do (§W05 routing). Examples in docs and UI copy say "a runner with KVM", not a hostname.
- **UI rules** (from the user, binding):
  - clean, coherent, expressive, hyperlight-site style: Geist type, mono eyebrows, hairlines, 7/9 px
    panels, pill buttons, quiet tab underlines;
  - one primary surface per page, and act where the thing is;
  - no popups: no modals, sheets, docked or floating panels; everything sits in the page's flow;
  - standard controls styled alike: native selects (`.select`), the faceted filter combobox
    (`components/ui/facet-box.tsx`), real checkboxes (`.check`);
  - static by default, motion only for real state changes, and the motion toggle and
    reduced-motion are always respected;
  - no hidden horizontal scrollers;
  - change only what the task asks for, and mention any other improvement instead of making it.
  - Every UI acceptance criterion is checked in Chromium **and** WebKit (the Playwright image
    `mcr.microsoft.com/playwright:v1.64.1-noble`), desktop and phone widths.
- **Opt-in automation.** Anything kiln decides on its own (routing preferences, retries beyond
  runner loss, quarantine, cancelling superseded runs, notifications, cache tiers, reuse of earlier
  results) is off until declared. Every such decision is explainable through the API and the UI:
  "routed to X because…", "retried because…", "skipped because…".
- **Tests and checks before handing over:**
  `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, and in `ui/`
  `npm run typecheck && npm run lint && npm run build`. A workstream adds its own scenario suites
  (W22). UI files are formatted with `npx prettier --print-width 200 --write <changed files>`
  (there is no prettier config file; a bare `prettier` rewraps whole files to 80 columns, so
  format only the files you changed).
- **Merging and deploying.** Work happens in a worktree per workstream. When it passes the checks,
  the coordinating session merges it into `main` and builds the server image
  (`docker build -q --build-arg KILN_COMMIT=$(git rev-parse HEAD) -t kiln:<sha> .` from main's
  checkout; with the commit, the server builds every other platform from it, docs/releases.md).
  Rolling the image out to the cluster
  (`deploy-image.sh kiln:<sha>`) is a production deploy: the user runs or approves it, so hand
  them the exact command; never ask another session to deploy for you. Runner restarts kill the
  shells running in kiln terminals, Claude Code sessions included: never restart or swap a runner
  (`kiln-agent`) without listing live terminal sessions first (`GET /api/terminal/sessions`) and
  leaving the timing to the user. Server-only deploys are safe for those shells.
- **Shells in tooling.** The automation shell may be zsh; write multi-line commands as script
  files.

### 0.4 Definition of done

A requirement is done when its code is on `main`, its acceptance criteria pass in CI (kiln's own
`.kiln/ci.yaml`), its scenarios pass on the live server where they say so, and the user-facing
docs (this file, `README.md`, the in-product help) describe what shipped.

The whole of this spec is done when:

1. every row on the status board is `done`;
2. the GHA conformance suite (W22-AC01) runs green on kiln: the workflows of Appendix F's corpus
   and the top-50 actions list, unmodified;
3. the end-to-end suite (W22-AC05) is green on the live cluster, covering every executor type
   available on the cluster's runners;
4. the performance targets of §1.5 hold under `kiln ci bench` (W22-AC08);
5. the UI acceptance criteria pass in Chromium and WebKit at desktop and phone widths.

### 0.5 Status board

Statuses: `ready` (can start), `blocked` (waits on what is in brackets), `in progress (branch)`,
`review (branch)`, `done`. Keep each row on one line.

| WS | Title | Depends on | Status | Progress |
| --- | --- | --- | --- | --- |
| W01 | Expressions | none | done | GitHub's 1,020 vectors pass; v1 corpus identical; 3.2 M evals/s |
| W02 | Workflow model, dialects, validation | W01 (parser) | done (AC09 with W03, W05) | 457 real workflows, no errors; 32 codes at exact positions; v1 plans identical (2,392 jobs); 10⁵ mutations |
| W03 | Triggers and events | W02 | done | matcher, patterns, skip, cron with DST, payloads; polling, webhooks (deduplicated), schedules, dispatches, workflow_run; the whole native table: `pull_request`'s activity types and `pull_request_target`, comments and reviews, the merge queue's `enqueued`/`dequeued`; kiln's `land`, `agent`, `webhook` (inbound hooks), `pipeline` and `deploy`; `chain-depth`; nothing started by a job's token; the App's events; AC01–AC09 |
| W04 | Planner, matrices, reusable workflows, outputs | W01, W02 | in progress | the plan engine, AC01–AC06 (unit, and AC04 and AC06 end to end), reusable workflows local and remote, plan preview, outputs' provenance and readers (R23); fixtures F-10 to F-15 and AC07; `files:` shards in runs, `$KILN_SHARD_ITEMS`, runs' removed combinations; AC08 on the live cluster left |
| W05 | Scheduler, concurrency, routing, retries | W04 (job model) | in progress | core and dispatcher, AC01–AC10 (AC08 at 127k assignments/s; AC09 a server killed under a queue, its running legs re-adopted), reruns as attempts; pools' executors scaled with W13 (R27); reservations (R12); the `scaling` and `reservation` reasons; resource mode from runners' facts (R03); AC11 on the live cluster left |
| W06 | Step runtime on the runner | W01, W02 | in progress | `run:` and `uses:` steps GitHub-exact on the host and in job containers; problem matchers; background steps; debug; kiln's checkout and files; AC01 toolkit conformance (every `@actions/core` API, `getIDToken` included) on the host and in the `docker` executor (W13-AC02); AC04 (each shell, on Linux, macOS and Windows), AC05 (pre, main and post order, `post-if`, cancellation, timeouts) and AC06 (paths in containers); a `k8s` pool's pods (W13-R07); Firecracker microVMs (W13-R05); shards' left with W13 |
| W07 | Actions | W06 | in progress | pins at run start (through reusable workflows), mirror, JS (pre/post), composite, local, `$/`, Docker; policy; catalog; built-ins `kiln/checkout@v1`, `kiln/test-results@v1`, `kiln/notify@v1`, `kiln/auth@v1`, `kiln/upload@v1`, `kiln/download@v1`, `kiln/cache@v1` and `kiln/deploy@v1`; kiln actions' declared secrets, requirements and permissions (R11); `kiln://` actions of the owner's own repositories; tarballs in the store; the run page's pins and a rerun with fresh actions; musl Node for Alpine job containers |
| W08 | Runtime services: store, caches, artifacts | none (store), W06 (wiring) | in progress | store, blobs, cache (GHES protocol), artifacts v4; actions/cache, upload/download-artifact unmodified; job-result reuse (R10, AC06, a rebased diff's); `kiln/cache@v1` and `cache:` on chunks (only new ones move; `ref`, `repo` and `global` scopes; a runner's local tier); called workflows' cache-mode capped; v1's `artifacts:`; the store's chunks in an S3-compatible bucket (R02); BuildKit's `type=gha` and `type=registry` caches (R11, AC03); speed (AC08): a 1 GiB restore at 1,181 MiB/s over HTTP on a 10 GbE link of the bench's own, release build, bench machine |
| W09 | Secrets and variables | W01 | in progress | encrypted store, scopes, least delivery, declarations (KCI4040/4041), variables, API and CLI, rotation; brokered secrets (integrations, Secrets Manager, Secret Manager, Key Vault, Vault, Infisical, Doppler, 1Password), made at leg start, never stored; kiln actions' declared secrets (KCI4040 at start, `with-secrets`, only what's declared inside); environments' secrets only after approval (AC07); Settings → CI's secrets and variables by scope (W21-R11); Vault, Infisical, Doppler and 1Password as stores (R08); AC02 (every read and read-only tool walked, none with a value); AC06 (declarations end to end, an action's undeclared reads reported); AC08 (W18-AC03's); a repository's, a workflow's and an environment's secrets on their pages |
| W10 | Identity, tokens, OIDC issuer | none | in progress | job tokens with permissions (revoked with the attempt, and on the API), OIDC issuer (RS256, rotation, GitHub claims, getIDToken, workload tokens, publishing, subjects per organization and repository), users, roles, grants, the route table and `/api/openapi.json`, PATs and agent tokens; the server's and agents' own workload tokens (R11); `secrets.use:<name>` (each run held to whoever started it), run lists, the event stream and the overview narrowed for scoped callers; passkeys and people's own passwords; Settings → People and → Tokens; the issuer reached from the internet (its own address, or published to a directory, S3 or Cloud Storage: R12); the cloud ACs left |
| W11 | Integrations and credential broker | W10 | in progress (CD) | the registry (`ci_integrations`, /api/ci/integrations), credentials in W09's `integration` scope; built: `oci` (any OCI registry), `aws`, `gcp`, `azure` (kiln's identity exchanged per workload), `cloudflare` and `vercel` tokens, Verify for each; the job side: `integrations:` on jobs and steps and `kiln/auth@v1` (each kind's own federation files and variables, ID tokens kept fresh, stored tokens as their tools' variables, masked), `kiln auth --print-env | --exec` in every executor; and secrets brokered `from: integration:…` and cloud secret stores (W09-R08); notification channels (W11-R06), OpenTelemetry (R09), and Linear and Jira issue trackers (R07) |
| W12 | Registries | W08 (store) | in progress | kiln's OCI registry (Distribution Spec v1.1; the conformance suite passes); Docker's token auth; referrers; collection; `registry_package` events; pull-through caches; caching proxies for every D.1 ecosystem (AC03 with the real tools for eight), vcpkg's binary cache kept by kiln; jobs' registry credentials through `docker-credential-kiln`, federated for every D.2 registry that federates (Alibaba's and Oracle's among them); offline fallbacks |
| W13 | Runners and executors | W06 | in progress | host with job containers, services, Docker actions; the docker executor with kiln-built Ubuntu runner images (GitHub's common toolchains, a slim variant), opt-in Docker inside, host networking; facts (built-in probes on connect, every 10 minutes and on request; declared facts and probes; routing decides over them; on the runner's page; AC01); runner images built in kiln (`kiln/build-image@v1`: Dockerfiles through buildx, Agentfiles through shards, pushed to kiln's registry and registered with their platforms); `docker` pools (`[[ci.pools]]`, `/api/ci/pools`, warm executors, one leg each, AC04); the Runners page's pools (their scaling history) and images, and the image builder (Dockerfiles and Agentfiles made, built and pushed in the UI); `k8s` pools (a pod per leg, each its own runner, a leg's `container:` and `services:` in a pod made for it, an opt-in Docker sidecar for Docker actions, KCI4032 as a leg is queued without one, terminals into its legs); `kiln runner setup`; the `firecracker` executor (a microVM per leg from its OCI image, out only through its egress proxy; its pools; terminals and kept legs in it); each VMM in a jail of its own (namespaces, no root), pools restored from snapshots, services and job containers in microVMs; AC02 on host, docker and firecracker, AC03, AC05 (a cold start 0.64 s, a warm one 35 ms); `shards` left |
| W14 | Code hosting, stacks, review, landing | W03 | in progress | native hosting (clone and push at /git/, push policy, import); stacked diffs (refs/for, versions, stacks, interdiffs, kiln submit/sync/checkout; AC01); change requests; review, policy from trunk, strategies and the land queue with bisection (AC02–AC04); the facade: git (smart HTTP at the root), REST at /api/v3 (repository reads, statuses, checks, releases, artifacts, dispatches, dependency snapshots, deployments, environments, pull requests and issues' comments and labels, workflow runs, code scanning, the dependency graph's compare, search), GraphQL (pull requests, comments, labels, reviews, checks, search; `gh`'s host and token); the GitHub App (R10, R12); the review UI (AC06, in Chromium and WebKit); replies by email and in Slack threads, posted as their people (R05); landing through the queue as GitHub lands where a merge queue is required (`gh pr merge --auto`) |
| W15 | Environments and deployments | W04, W09 | in progress | CI's half: environments from trunk and kiln, protection (reviewers and teams, wait timers, branch and tag policies, deploy windows, custom gates, `after:`/`soak:` promotion), concurrency limits, approvals on the run page and CLI, deployment records with URLs; `kiln/deploy@v1` and the `deploy:` shorthand (CI drives CD through the deploy service); kinds and targets (host, container and microVM jobs routed to their runners, a platform's integration bound), the starter set, an environment's page and the board; container deployments (Compose stacks, healthy, outliving their jobs) and microVM ones (long-lived Firecracker microVMs, health-checked, taking over their ports, booted again with their runner); previews of diffs, stacks and pull requests (AC04; their containers and microVMs taken down as they go); shards' microVMs, CD's preview apps and the live-cluster ACs left |
| W16 | Test results, flaky, benchmarks | W06, W08 | in progress | ten report formats, `test-results:` to the store, results and annotations, flaky and probably flaky, quarantine with `enforce`, timing for shards, `$KILN_METRICS` and seven converters, bootstrap regressions by fingerprint (on every platform), `bench-gate` (on runs, diffs and the land queue), Tests and Benchmarks panels, `kiln/test-results@v1` and opt-in discovery; a repository's tests and its metrics' trends on Insights; AC05 left |
| W17 | Debugging | W06, W13 | in progress | terminals into running and kept legs (host, docker; on the run page and `kiln debug terminal`), keep on failure with extend and release, a step rerun in place, `kiln workflow run` here or on a server, agents through MCP (exec, rerun step, keep, release); log search and "compare with the last green run" (W19); terminals into `k8s` pods' legs; in Firecracker microVMs terminals, keeping, extend and release, and reruns in place (W13-R05); shards' left |
| W18 | Supply chain, untrusted code | W10, W13 | in progress | provenance the server makes from its own records and signs (SLSA v1.0, DSSE, Sigstore bundles; its own P-256 key, a cosign key file, AWS KMS, or keyless through Fulcio and Rekor), images' referrers in kiln's registry, a provenance check CD's environments require before they deploy (R04, AC04), `kiln/attest@v1` and the job's `attest:`, bundles beside files, on the run and through GitHub's attestations API; SBOMs (SPDX 2.3, CycloneDX 1.6) from ten lockfile kinds and `syft`, `kiln sbom`; AC01 with cosign, AC02; untrusted code: trust (fork PRs polled or delivered, marks), approval of outside contributors' runs, untrusted executors (rootless Docker), isolated cache scopes, contained networks through a per-leg egress proxy (SNI checked, blocks annotated), AC03 in containers; the dependency changes a diff makes, on its review page (R03); attestations as referrers in other registries too; Firecracker microVMs contained (no network device: out only through the egress proxy, W13-R05); shards' left |
| W19 | Insights, audit, notifications | W05 | done (AC05's real channels with W11-AC03's tenants) | notification rules from trunk and the server (events, `where`, `throttle`, `run.fixed`, `deploy.*`, `leg.flaky`, `land.ejected`, `runner.offline`, `review.requested`, `comment.reply`, `mention`, `quota.near`, `secret.unused`), Slack, Discord, Teams, Telegram, PagerDuty, Opsgenie, email (SMTP), kiln's inbox and signed webhooks, deliveries recorded, `kiln/notify@v1`, AC05; outgoing webhooks (per-scope subscriptions to kiln's events, signed, in order, retried, logged before sending, resumed after a restart, redelivered); insights (hourly and daily rollups that merge, percentiles within 1 %, waits by reason, attempts, flaky legs, cache, runner utilization, fair share, critical path, AC01, AC02) with Prometheus metrics; compare (differences in actions, variables, environment, images, machines and cache; changed files; steps side by side; "since the last green run", AC03); log search (a trigram index built as legs finish, filters, one leg's search, failure excerpts, AC06 under 1 ms in 10,000 runs); audit v2 (one row per change with principal, on-behalf-of, token, scope, route, result and address; handlers' notes as its detail; secret uses and integration exchanges; filters by every field; AC04 from the route table); OpenTelemetry export (OTLP traces of runs, legs and steps); a person's own rules (inbox, email; Settings → You); Linear and Jira as channels, `diff.landed` and `test.quarantined` |
| W20 | Agents: MCP, skills, attachments, experiments | W02, W05 | in progress | one registry for deployments' and CI's operations, checked against the route table; `kiln.ci.*` and tools for runs, legs (exec, rerun step, keep, release), workflows (edits proposed as diffs), caches, artifacts, tests (quarantine proposed as a diff), metrics, runners, pools, images, routing, secrets' names, variables, integrations, environments, insights and diffs; the seven CI skills (author, diagnose, runners, review, migrate, secrets, performance), their programs run as written (AC05); users' tokens over MCP; every operation over HTTP, in the OpenAPI document and as `kiln op`; writes to routing rules, pools and environments that say what they do (R03); agents asking for runs (R07); attachments and incidents, with every wake R08 lists, Attach agent on each page it names, and daily limits on incidents, tokens and runner minutes (R08 to R10); every §4.1 route an operation on every surface, and §4.3's commands (AC01); bisecting a failure; experiments, their options built by agents too, measured by ordinary runs, with intervals, effects, interactions and a recommendation (R11, AC06); agents' own isolation left |
| W21 | UI | each backend it shows | in progress | the CI API; the run page (attempts, reruns, the graph from the plan with matrix grids, progress and what jobs wait for, critical path, summaries, annotations, artifacts, pins); the leg page (notes, timing bar, attempts, outputs, terminal, Keep); the runs list's facets and sorts; the workflow editor (checks against its repository, the schema's keys, the plan preview and its matrix, the flow view, saving as the repository takes it); Insights (repositories, workflows, runners, tests, benchmarks); Settings → CI, → People, → Tokens and → You; the Runners page's pools (their scaling history), images and executors, and the image builder; comparing runs; log search; environments (board and pages); AC01 in kiln's CI (`ci/ui/run-all.sh`), AC02, AC03 (25–30 ms, 60 fps), AC04, the run's data flow; the editor's Agentfile lints left (shards' own: blocked on shards) |
| W22 | Conformance, e2e, performance | grows with the rest | in progress | local e2e harness; AC01: the corpus plans (804 plans), 39 of 57 runnable starter workflows green, the other 18 failing the same way on GitHub, every miss explained; AC02: 48 of F-21's 50 green, four of them needing accounts and passing in dry configurations against stand-ins (AWS's two, Google's, Codecov's); azure/login waits for a tenant, and attest-build-provenance for GitHub's Sigstore; AC04 (fault injection: drops, delays, OOM kills, full disks and corrupted uploads, each recovered from as specified); AC07 (the security suite: token scope on every route, secrets in no log, API, event or database file, YAML bombs, SSRF guards, archive traversal); AC06 (the UI suite in kiln's own CI: `ci/ui/run-all.sh` in `.kiln/ci.yaml`'s `ui-checks`); AC08 (`kiln ci bench`, every §1.5 target it measures met, regressions gated in kiln's own CI); AC09 (a runner from before the CI work beside this version's) |
| W23 | CI providers: KilnCI, GitHub Actions, GitLab CI/CD, Bitbucket Pipelines | W03, W14, deploy-api.md §7 | in progress | the git-host interface (kiln, GitHub, GitLab, Bitbucket); the provider core; GitHub Actions, GitLab CI/CD and Bitbucket Pipelines behind one contract (runs, jobs, logs, rerun, cancel, dispatch, artifacts, approvals, environments, checks, lint, webhooks; AC01); the hosts' pull requests through one API (R06); events for agents; SSO with GitHub, GitLab and Bitbucket, and acting on the host as the person |
| W24 | Incremental CI: fingerprints, reuse, warm state, shared caches | W04, W05, W06, W08, W13, W18 | ready | specified (2026-10-24): R01–R28, AC01–AC12; milestones M0–M5 are kiln goals with plans |

---

## 1. What we are building

### 1.1 Goals

Each goal is the user's requirement in their own terms, and lists the workstreams that deliver it.

| ID | Goal | Delivered by |
| --- | --- | --- |
| G-01 | **Fully GitHub Actions compatible CI.** `.github/workflows` run unmodified: syntax, expressions, contexts, `uses:` actions (JavaScript, Docker, composite), reusable workflows, workflow commands, environment files, the cache, artifact, tool-cache and OIDC services actions rely on, `GITHUB_TOKEN`, event payloads. | W01–W08, W10, W14, Appendix A |
| G-02 | **Much better concurrency provisioning and handling; faster queuing.** Event-driven dispatch, capacity by resources not just slots, pools, quotas, fair share, priorities, reservations, job-level concurrency groups, explained waits. | W05, W13 |
| G-03 | **Better matrices: filter, split and control work across jobs.** Cross-arch builds that are not painful. | W04, W05, W16 |
| G-04 | **Better passing of outputs and environment.** Between steps, jobs, matrix legs and runs, typed, traceable. | W04, W06 |
| G-05 | **Better secrets.** Secrets scoped to the pipeline; an action can reference a secret knowing it exists in the pipeline it runs in, checked before the run starts. | W09 |
| G-06 | **Workflow and action builder UIs** with real syntax highlighting and checking, visual flow editing, plan previews. | W21, W02 |
| G-07 | **Traditional git workflows and Meta-style stacked diffs**, with the merge type chosen on the fly: merge commit, squash, rebase, fast-forward, and stack landing. | W14 |
| G-08 | **Better review.** Better diffs, online review and comment threads, easy to find and answer reviewers. | W14, W21 |
| G-09 | **Better progress and a more intuitive job DAG.** | W21, W06 |
| G-10 | **Better caching.** | W08, W12, W13 |
| G-11 | **Assemble and set up runners in full from kiln**: bare metal, Docker, Firecracker and shards microVMs (full Agentfile support); build Docker and Agentfile runner images in the UI with highlighting. | W13, W21 |
| G-12 | **Triggers beyond cron**: events of every kind, manual runs, kiln agents. | W03, W20 |
| G-13 | **Thorough integration with kiln's GitOps CD.** | W15 |
| G-14 | **Any kiln-registered agent can run, control, write and manage CI fully and accurately**, through a code-mode MCP server and skills. | W20 |
| G-15 | **Environment awareness**: easy environment creation and management, deploying to CD, runner hosts, containers, microVMs and other platforms. | W15, W11 |
| G-16 | **Better retries.** | W05 |
| G-17 | **Routing to any runner that can do the work, weighing how well each suits it**: opt-in, declared rules with a clear API. | W05, W13 |
| G-18 | **Multi-arch and multi-OS throughout.** | W04, W05, W13 |
| G-19 | **Skip work that has not changed** (input hashing, path- and dependency-aware triggering). | W08, W03 |
| G-20 | **Test results as data**: flaky detection, opt-in quarantine, timing-balanced shards. | W16 |
| G-21 | **Live debugging** of failed jobs: a terminal into the job, debug reruns, `kiln run` locally. | W17 |
| G-22 | **Cancel superseded runs** (opt-in, per workflow). | W05 |
| G-23 | **Land queue** with required checks kept in kiln. | W14 |
| G-24 | **Identity and supply chain**: OIDC tokens for jobs, signed artifacts, provenance, SBOMs. | W10, W18 |
| G-25 | **Untrusted code in microVMs**, with limited egress and no secrets. | W18, W13 |
| G-26 | **Mirrored, SHA-pinned actions.** | W07 |
| G-27 | **Service containers.** | W13 |
| G-28 | **Runner autoscaling** from declared pools. | W13, W05 |
| G-29 | **Benchmark regression tracking and gates.** | W16 |
| G-30 | **Pipeline insights**: critical path, queue time against run time, resource use. | W19 |
| G-31 | **Audit log and rule-based notifications.** | W19 |
| G-32 | **Package registries** of the top ~25 languages and the top ~10 OCI registries: credentials, caching proxies, publishing. | W12 |
| G-33 | **OIDC everywhere it is accepted** (and the best available auth where it is not): clouds, secret managers, registries, model providers, SaaS. | W10, W11, Appendix C |

### 1.2 Principles

1. **Compatibility first, then improvement.** A GHA workflow runs as on GitHub. kiln's own
   dialect (`.kiln/*.yaml`) is a strict superset of GHA's syntax: every improvement is an
   additional key, function or context, never a changed meaning. A GHA workflow can take kiln
   improvements without being edited, through an overlay (W02), so it keeps working on GitHub.
2. **Declared, opt-in, explained.** kiln never makes a "smart" choice the user did not ask for.
   Rules are data with an API; every automatic decision carries its reason.
3. **One route table.** Every operation is defined once (name, input schema, capability, effect).
   The HTTP routes, the OpenAPI document, the CLI, the MCP tools and code mode's `kiln.*` all come
   from it, so they cannot drift (W20).
4. **GitOps.** Pipelines are read at the commit being built. Policy (review rules, owners,
   environments, routing for a repository, quarantine, agent attachments, notifications) is read
   from **trunk**, so a change cannot loosen the rules it is judged by. Everything created in the
   UI can be saved to the repository as a reviewed change.
5. **Isolation follows trust.** Code from people outside a repository's trust runs only where it
   cannot reach secrets or the host: containers or microVMs with limited egress.
6. **Everything observable.** Every wait, retry, skip, route, cache hit and reuse is visible with
   its reason, in the UI and the API.
7. **No paid services.** Everything kiln needs is open source and self-hosted. Integrations with
   paid services are optional.
8. **General hardware.** Any set of machines, any OS and architecture kiln runs on.

### 1.3 Out of scope

- Re-implementing GitHub's hosted runner images byte for byte. kiln provides runner images with
  the commonly used toolset and maps GitHub's labels to them by declared rules (W13).
- GitHub features outside Actions: Issues, Projects, Discussions, Wikis, Pages hosting, Packages
  hosting beyond the registries of W12, Codespaces, Dependabot.
- Billing.
- Windows containers and Windows microVMs (Windows **host** runners are in scope).

### 1.4 Vocabulary

| Term | Meaning |
| --- | --- |
| **Workflow** | One file that defines jobs and when they run: `.github/workflows/*.yml` (GHA dialect) or `.kiln/*.yaml` (kiln dialect). "Pipeline" is the older word for the same thing; both are used. |
| **Dialect** | `gha` (GitHub's syntax exactly) or `kiln` (GHA plus kiln's keys and v1 forms). |
| **Overlay** | `.kiln/overlays/<workflow>.yaml`: kiln keys applied to a GHA workflow without editing it. |
| **Run** | One execution of one workflow for one trigger. Has a number per repository and attempts. |
| **Job** | A job as the workflow defines it (`jobs.<id>`). |
| **Leg** | One matrix combination of a job. A job without a matrix has one leg. |
| **Attempt** | One try of a leg. Reruns and retries add attempts. |
| **Step** | One `run:` or `uses:` in a leg, plus the pre and post steps actions add. |
| **Action** | A reusable unit of steps: JavaScript, Docker or composite (GHA's `action.yml`), or a kiln action. |
| **Runner** | A machine (or a process on one) running `kiln agent`, connected over QUIC. |
| **Executor** | How a runner runs a leg: `host` (a process tree on the machine), `docker` (a container under Docker or Podman), `firecracker` (a Firecracker microVM), `shards` (a shards microVM), `k8s` (a pod). |
| **shards** | hyper-light's microVM system (`github.com/hyper-light/shards`). It is **not Docker** and kiln never treats it as Docker. It has its own machines (one Linux kernel per microVM, many agents per microVM, each isolated like a container), its own image builds (Agentfiles: a Dockerfile's vocabulary plus `AGENT`, `HARNESS`, `SKILL`, `MCP`, `VOLUME`, `NETWORK`, `CONNECT`, `ATTACH` and `EXPOSE` directives), default-deny networks, files and devices, snapshots that restore in microseconds, warm pools kept by `shardsd`, and vsock between host and guest. OCI compatibility means its images and microVMs are OCI objects (pushed, pulled, tagged like any other); its Docker-style commands are a convenience, not the interface kiln builds on (W13). |
| **Pool** | A declared group of runners or of executors that kiln starts and stops on demand. |
| **Fact** | Something a runner reports about itself: OS, architecture, CPUs, memory, free disk, GPUs, executors it supports, tools. |
| **Requirement** | What a leg needs from a runner: labels, facts, an executor, resources. |
| **Routing rule** | A declared mapping from labels or job content to requirements and preferences. |
| **Scope** | Where a secret, variable, rule or grant applies: server, organization, repository, environment, workflow, job. |
| **Integration** | A configured external service (a cloud, a registry, a model provider, a SaaS) and how jobs authenticate to it. |
| **Environment** | A named deployment target with protection rules, secrets, variables, and a kind (Kubernetes, runner host, container, microVM, platform). |
| **Diff, version, stack** | A commit under review, each pushed revision of it, and a chain of dependent diffs (W14). |
| **Land** | Moving reviewed diffs onto trunk through the land queue, with a merge strategy. |
| **Facade** | kiln's GitHub-compatible git and REST endpoints for jobs (W14), so `actions/checkout` and API-calling actions work against kiln. |

### 1.5 Performance targets

Measured by `kiln ci bench` (W22) on one 16-core server, against simulated runners, and confirmed
on the live cluster at smaller scale.

| Measure | Target |
| --- | --- |
| Trigger to run created (webhook or `ls-remote` change seen) | p99 < 500 ms, excluding git fetch time |
| Leg ready to `Assign` sent, with free capacity | p99 < 50 ms; mean < 5 ms |
| Scheduler throughput | ≥ 2,000 assignments/s; 10,000 queued legs; 1,000 runners |
| Queue state after a server restart | rebuilt in < 2 s for 10,000 queued legs |
| Concurrent running legs per server | ≥ 2,000 (today: 49 observed) |
| Log ingest | ≥ 200 MiB/s aggregate; a live log line reaches the browser in p99 < 250 ms |
| Cache restore of a 1 GiB entry from the server on a 10 GbE LAN | ≥ 500 MiB/s |
| Runs list with 1,000,000 runs | first page < 100 ms; scrolls smoothly (windowed) |
| Run graph with 1,000 legs | renders in < 200 ms, interaction at 60 fps |
| Expression evaluation | ≥ 1,000,000 simple evaluations/s per core |

---

## 2. Architecture

### 2.1 Components

```
                                         kiln server
 ┌────────────────────────────────────────────────────────────────────────────────────────────┐
 │ sources ──▶ triggers ──▶ loader ──▶ planner ──▶ scheduler ──▶ dispatcher ──▶ runners (QUIC)  │
 │ (git mirrors,  (W03)     (W02:       (W04)       (W05: queues,   (Assign v2)                  │
 │  native repos,           dialects,               concurrency,                                 │
 │  webhooks,               overlays,               routing,                                     │
 │  agents, API)            validation)             retries)                                     │
 │                                                                                               │
 │ runtime services (W08, W10): blob store · caches (GHA v1, v2) · artifacts (v4) · OIDC issuer  │
 │   · job tokens · action mirror (W07) · tool cache · registries (W12) · GitHub facade (W14)    │
 │ code hosting (W14): native git, diffs, stacks, review, land queue                             │
 │ secrets and variables (W09) · integrations and broker (W11) · environments (W15) ⇄ CD         │
 │ results (W16) · insights, audit, notifications (W19) · MCP code mode and skills (W20)         │
 │ API (one route table) · events (WS, SSE, webhooks) · UI (W21)                                  │
 └────────────────────────────────────────────────────────────────────────────────────────────┘
        ▲ QUIC + mTLS (or the HTTP tunnel)                     ▲ HTTPS: runtime services, facade
        │                                                      │
 ┌──────┴──────────────────────────────────────────────────────┴─────────────────────────────┐
 │ kiln agent (runner): facts and probes (W13) · step runtime (W06) · actions (W07) ·          │
 │ executors: host · docker · firecracker · shards · k8s (pod per leg) · services · job TTY    │
 └─────────────────────────────────────────────────────────────────────────────────────────────┘
```

### 2.2 Code layout

| Where | What | Rules |
| --- | --- | --- |
| `crates/kiln-ci/` (new) | The deterministic core: `expr` (W01), `wf` (workflow model, dialects, overlays, schema, diagnostics; W02), `trigger` (event matching; W03), `plan` (graph, matrix, reusable workflows, outputs; W04), `sched` (scheduler state machine, concurrency, routing, retry policy; W05), `results` (test-result parsing, flaky classification; W16), `stats` (bootstrap intervals for benches and experiments). | Pure: no I/O, clock or randomness except as inputs. `#![forbid(unsafe_code)]`. Depends only on serde, serde_yaml, serde_json, glob, sha2, thiserror and the like. Every module has exhaustive unit tests and property tests. |
| `crates/kiln/src/ci/` (new) | The integration: `load` (read workflows from git), `runs` (persisting runs, jobs, legs, attempts, steps), `dispatch` (drives `kiln_ci::sched`, sends assignments), `services/` (blob, cache, artifacts, twirp; W08), `oidc` (W10), `secrets` (W09), `integrations/` (W11), `registry/` (W12), `facade/` (W14), `review/` (W14), `env` (W15), `results` (W16), `insights` (W19), `api` (routes), `ops` (the route table entries for MCP; W20). | May do I/O. Uses `crate::store`, `crate::http`, `crate::rt`. |
| `crates/kiln/src/runner/` (new, split out of `exec.rs`) | Runner side: `runtime` (W06: env, workflow commands, env files, shells, masking, summaries), `actions` (W07), `executors/{host,docker,firecracker,shards,k8s}` (W13), `facts` (W13), `services` (job and service containers), `terminal` (W17). | `exec.rs` keeps working for v1 assignments until W06 replaces it; both paths are tested. |
| `crates/kiln/src/pipeline.rs` | Kept for v1 compatibility while W02 lands; then becomes a thin shim over `kiln_ci::wf` (the kiln dialect parses every v1 file). | |
| `ui/src/views/ci/` | Runs, run detail, the workflow editor and builder, the action builder, insights (W21). | UI rules (§0.3). |
| `ui/src/views/review/` | Inbox, stack, diff, land queue (W14, W21). | |
| `ui/src/views/runners/` | Runners, pools, images and the image builder (W13, W21). | |
| `ui/src/views/environments/` | Environments and deployments (W15, W21). | |
| `ui/src/views/settings/` | Secrets, variables, integrations, registries, routing, notifications (W09, W11, W12, W05, W19). | |
| `ui/src/components/code/` | The code editor (CodeMirror 6) with YAML, Dockerfile and Agentfile modes, diagnostics and completion (W21). | |

### 2.3 Lifecycle of a run

```
trigger (W03) ─▶ load workflows at the commit (W02) ─▶ match events and filters (W03)
   ─▶ plan the graph (W04): jobs, needs, static matrix legs; dynamic legs wait for their needs
   ─▶ queue ready legs (W05) ─▶ route (W05) ─▶ assign (§2.5) ─▶ execute (W06, W07, W13)
   ─▶ report steps, outputs, annotations, results (W06, W16) ─▶ finish the leg
   ─▶ ready the legs that needed it, evaluate their `if` (W04) ─▶ … ─▶ finish the run
```

**State machines.** Every object below records `status` and, once completed, `conclusion`, as GHA
does. kiln's existing `Status` (store.rs) maps onto them; two variants are appended (`Waiting`,
`Reused`) and are never sent to runners that lack `CAP_CI2` (§2.5).

| Object | `status` | `conclusion` (when `completed`) |
| --- | --- | --- |
| Run | `queued` → `in_progress` → `completed`; `waiting` while a job waits for an approval | `success`, `failure`, `cancelled`, `skipped`, `timed_out`, `action_required`, `startup_failure` (the workflow did not parse or plan) |
| Job (all its legs) | as legs, aggregated | `success` if every leg succeeded or was skipped by a matrix filter; `failure` if any failed (and was not `continue-on-error`); `cancelled`; `skipped` |
| Leg | `pending` (needs not finished) → `queued` → `waiting` (approval, concurrency group, capacity: with a reason) → `in_progress` → `completed` | `success`, `failure`, `cancelled`, `skipped`, `timed_out`, `reused` (W08 job-result reuse; counts as success for `needs`) |
| Attempt | as the leg, per try | as the leg, plus `infra_failure` (runner lost, executor failed to start: retryable, W05) |
| Step | `pending` → `in_progress` → `completed` | `success`, `failure`, `cancelled`, `skipped`; with `outcome` (before `continue-on-error`) and `conclusion` (after), GitHub-exact |

Mapping to the store's `Status` for v1 views: `success`→`Succeeded`, `failure`→`Failed`,
`cancelled`→`Cancelled`, `skipped`→`Skipped`, `timed_out`→`TimedOut`, `reused`→`Reused`,
`waiting`→`Waiting`, `in_progress`→`Running`, `queued`→`Queued`, `pending`→`Pending`.

### 2.4 Data model

New tables come from migrations. The current schema (what `SCHEMA` in `store.rs` creates) is the
baseline. Each migration is one file in `crates/kiln/src/store/migrations/`, named
`<yyyymmdd-hhmm>_<ws>_<topic>.sql` (embedded at build time). They are applied in name order at
start, each in a transaction, recorded by name in a `migrations` table, and never edited once
merged, so parallel branches never compete for a number (§5.2). All ids are UUIDv7 strings
(`crate::util::new_id`). JSON columns hold documented types. The blocks below are numbered for
reading only.

```sql
-- 1: workflows, runs v2, legs, attempts
CREATE TABLE ci_workflows (           -- one row per (repo, path, commit) the loader read
  id TEXT PRIMARY KEY, repo_id TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
  path TEXT NOT NULL, sha TEXT NOT NULL, dialect TEXT NOT NULL,        -- gha | kiln | auto
  name TEXT NOT NULL, def TEXT NOT NULL,                                -- normalized IR (JSON)
  diagnostics TEXT NOT NULL DEFAULT '[]', blob_sha TEXT NOT NULL,       -- git blob of the file
  UNIQUE (repo_id, path, blob_sha));
ALTER TABLE runs ADD COLUMN workflow_id TEXT;          -- ci_workflows.id; NULL for v1 runs
ALTER TABLE runs ADD COLUMN attempt INTEGER NOT NULL DEFAULT 1;
ALTER TABLE runs ADD COLUMN conclusion TEXT;
ALTER TABLE runs ADD COLUMN actor TEXT NOT NULL DEFAULT '';            -- principal (W10)
ALTER TABLE runs ADD COLUMN trigger TEXT NOT NULL DEFAULT '{}';        -- event, ref, before, after, payload digest, diff/version
ALTER TABLE runs ADD COLUMN run_name TEXT;                             -- GHA run-name
ALTER TABLE runs ADD COLUMN trust TEXT NOT NULL DEFAULT 'trusted';     -- trusted | untrusted (W18)
ALTER TABLE runs ADD COLUMN priority INTEGER NOT NULL DEFAULT 50;
CREATE TABLE ci_events (               -- the event payload a run saw (GITHUB_EVENT_PATH)
  run_id TEXT PRIMARY KEY REFERENCES runs(id) ON DELETE CASCADE, payload TEXT NOT NULL);
ALTER TABLE jobs ADD COLUMN job_id TEXT;              -- jobs.<id> in the workflow (jobs rows are legs)
ALTER TABLE jobs ADD COLUMN matrix TEXT;              -- the leg's matrix values (JSON object)
ALTER TABLE jobs ADD COLUMN leg_index INTEGER;        -- strategy.job-index
ALTER TABLE jobs ADD COLUMN outputs TEXT;             -- JSON object (≤ 1 MiB)
ALTER TABLE jobs ADD COLUMN conclusion TEXT;
ALTER TABLE jobs ADD COLUMN requirements TEXT;        -- resolved requirements (W05, JSON)
ALTER TABLE jobs ADD COLUMN routing TEXT;             -- last routing decision with reasons (JSON)
ALTER TABLE jobs ADD COLUMN wait_reason TEXT;         -- why it is waiting (JSON, W05)
ALTER TABLE jobs ADD COLUMN concurrency_group TEXT;
ALTER TABLE jobs ADD COLUMN priority INTEGER NOT NULL DEFAULT 50;
ALTER TABLE jobs ADD COLUMN input_hash TEXT;          -- W08 reuse key
ALTER TABLE jobs ADD COLUMN reused_from TEXT;         -- jobs.id this leg reused
ALTER TABLE jobs ADD COLUMN environment TEXT;         -- W15
CREATE TABLE ci_attempts (
  id TEXT PRIMARY KEY, leg_id TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
  number INTEGER NOT NULL, status TEXT NOT NULL, conclusion TEXT, runner TEXT, executor TEXT,
  queued_at INTEGER, started_at INTEGER, finished_at INTEGER, exit_code INTEGER, error TEXT,
  failure_class TEXT,                                  -- W05 retry classification
  peak_rss INTEGER, cpu_seconds REAL, UNIQUE (leg_id, number));
ALTER TABLE steps ADD COLUMN kind TEXT NOT NULL DEFAULT 'run';   -- run | uses | pre | post | internal
ALTER TABLE steps ADD COLUMN step_id TEXT;                        -- the step's `id:`
ALTER TABLE steps ADD COLUMN action TEXT;                         -- resolved owner/repo@sha or path
ALTER TABLE steps ADD COLUMN outcome TEXT;
ALTER TABLE steps ADD COLUMN conclusion TEXT;
ALTER TABLE steps ADD COLUMN outputs TEXT;
ALTER TABLE steps ADD COLUMN attempt INTEGER NOT NULL DEFAULT 1;
CREATE TABLE ci_annotations (          -- ::error, ::warning, ::notice and test failures
  id INTEGER PRIMARY KEY AUTOINCREMENT, leg_id TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
  step_idx INTEGER, level TEXT NOT NULL, title TEXT, message TEXT NOT NULL,
  file TEXT, line INTEGER, end_line INTEGER, col INTEGER, end_col INTEGER, log_offset INTEGER);
CREATE TABLE ci_summaries (            -- $GITHUB_STEP_SUMMARY, markdown
  leg_id TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE, step_idx INTEGER NOT NULL,
  markdown TEXT NOT NULL, PRIMARY KEY (leg_id, step_idx));

-- 2: blobs, caches, artifacts (W08)
CREATE TABLE cas_objects (             -- content-addressed objects: chunk lists
  digest TEXT PRIMARY KEY, size INTEGER NOT NULL,                   -- SHA-256 of the content
  chunks TEXT NOT NULL,                                             -- JSON [[chunk digest, size]]
  created_at INTEGER NOT NULL, last_used_at INTEGER NOT NULL);     -- references are found at collection
CREATE TABLE cas_chunks (digest TEXT PRIMARY KEY, size INTEGER NOT NULL);  -- <data>/cas/chunks/ab/cd/<digest>
CREATE TABLE ci_caches (
  id INTEGER PRIMARY KEY AUTOINCREMENT, repo_id TEXT NOT NULL, scope TEXT NOT NULL,  -- ref or 'repo' or 'global'
  key TEXT NOT NULL COLLATE NOCASE, version TEXT NOT NULL, object TEXT,  -- cas_objects.digest; NULL while uploading
  size INTEGER, created_at INTEGER NOT NULL, last_used_at INTEGER, created_by_leg TEXT,
  UNIQUE (repo_id, scope, key, version));
CREATE TABLE ci_artifacts (
  id INTEGER PRIMARY KEY AUTOINCREMENT, run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
  leg_id TEXT, name TEXT NOT NULL, object TEXT, size INTEGER, digest TEXT,  -- digest: sha256:<hex>
  mime_type TEXT, created_at INTEGER NOT NULL, expires_at INTEGER, finalized INTEGER NOT NULL DEFAULT 0,
  UNIQUE (run_id, name));
CREATE TABLE ci_reuse (                -- W08 job-result reuse
  input_hash TEXT PRIMARY KEY, leg_id TEXT NOT NULL, outputs TEXT, created_at INTEGER NOT NULL);

-- 3: secrets, variables, integrations, OIDC keys (W09, W10, W11)
CREATE TABLE ci_secrets (
  id TEXT PRIMARY KEY, scope_kind TEXT NOT NULL, scope_id TEXT NOT NULL, name TEXT NOT NULL,
  ciphertext BLOB NOT NULL, nonce BLOB NOT NULL, key_id TEXT NOT NULL,
  created_by TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
  last_used_at INTEGER, UNIQUE (scope_kind, scope_id, name));
CREATE TABLE ci_secret_uses (secret_id TEXT NOT NULL, leg_id TEXT NOT NULL, ts INTEGER NOT NULL,
  PRIMARY KEY (secret_id, leg_id));
CREATE TABLE ci_variables (
  scope_kind TEXT NOT NULL, scope_id TEXT NOT NULL, name TEXT NOT NULL, value TEXT NOT NULL,
  updated_by TEXT NOT NULL, updated_at INTEGER NOT NULL, PRIMARY KEY (scope_kind, scope_id, name));
CREATE TABLE ci_integrations (
  id TEXT PRIMARY KEY, kind TEXT NOT NULL, name TEXT NOT NULL UNIQUE, config TEXT NOT NULL,
  secret_ref TEXT, scope_kind TEXT NOT NULL, scope_id TEXT NOT NULL,
  created_at INTEGER NOT NULL, verified_at INTEGER, last_error TEXT);
CREATE TABLE oidc_keys (kid TEXT PRIMARY KEY, alg TEXT NOT NULL, private_der BLOB NOT NULL,
  public_jwk TEXT NOT NULL, created_at INTEGER NOT NULL, retire_at INTEGER, retired INTEGER NOT NULL DEFAULT 0);
CREATE TABLE tokens (                  -- W10 principals' tokens (hashed)
  id TEXT PRIMARY KEY, kind TEXT NOT NULL, principal TEXT NOT NULL, on_behalf_of TEXT,
  hash TEXT NOT NULL UNIQUE, scopes TEXT NOT NULL, created_at INTEGER NOT NULL,
  expires_at INTEGER, last_used_at INTEGER, revoked_at INTEGER);

-- 4: runners, pools, routing (W05, W13)
ALTER TABLE agents ADD COLUMN facts TEXT NOT NULL DEFAULT '{}';
ALTER TABLE agents ADD COLUMN executors TEXT NOT NULL DEFAULT '["host"]';
ALTER TABLE agents ADD COLUMN pool TEXT;
CREATE TABLE ci_pools (name TEXT PRIMARY KEY, spec TEXT NOT NULL, updated_at INTEGER NOT NULL);
CREATE TABLE ci_routing_rules (id TEXT PRIMARY KEY, scope_kind TEXT NOT NULL, scope_id TEXT NOT NULL,
  ord INTEGER NOT NULL, rule TEXT NOT NULL, updated_by TEXT NOT NULL, updated_at INTEGER NOT NULL);
CREATE TABLE ci_images (               -- runner images built or registered (W13)
  id TEXT PRIMARY KEY, name TEXT NOT NULL, kind TEXT NOT NULL,   -- docker | agentfile | rootfs
  source TEXT NOT NULL, reference TEXT, digest TEXT, platforms TEXT NOT NULL,
  built_by_run TEXT, created_at INTEGER NOT NULL);

-- 5: actions mirror (W07)
CREATE TABLE ci_actions (
  repo TEXT NOT NULL, ref TEXT NOT NULL, sha TEXT NOT NULL, object TEXT,  -- cas digest of the tarball
  metadata TEXT, fetched_at INTEGER NOT NULL, PRIMARY KEY (repo, ref));

-- 6: code hosting and review (W14): see W14 for the full set
-- 7: environments and deployments (W15): see W15
-- 8: test results and metrics (W16): see W16
-- 9: notifications and audit v2 (W19): see W19
```

Each workstream that owns a migration lists its exact SQL in its own section; the above is the
shared core. A migration test (`store::tests::migrations_apply_in_order_and_are_idempotent`) opens a
database at the baseline and after each migration, and migrates it to the latest; applying twice
changes nothing.

### 2.5 Protocol additions

Assignments that use anything new (GHA dialect, actions, services, executors other than `host`
and the k8s pod, runtime services) are sent as `ServerMsg::AssignV2 { json }`: a JSON
`AssignmentV2` (below) that can grow without protocol churn. Runners announce what they can do with
new bits in `AgentMsg::Build.caps`:

| Bit | Name | Means the runner |
| --- | --- | --- |
| `1 << 10` | `CAP_CI2` | takes `AssignV2`, sends `JobEvent`, implements W06 |
| `1 << 11` | `CAP_ACTIONS` | runs `uses:` actions (W07) |
| `1 << 12` | `CAP_DOCKER` | has a working Docker or Podman (job containers, services, Docker actions) |
| `1 << 13` | `CAP_FIRECRACKER` | can boot Firecracker microVMs (KVM, firecracker, jailer present) |
| `1 << 14` | `CAP_SHARDS` | can run shards microVMs (`shards version` works) |
| `1 << 15` | `CAP_FACTS` | sends `Facts` (W13) |
| `1 << 16` | `CAP_JOB_TTY` | can open a terminal into a running or kept leg (W17) |

Bits below `1 << 10` belong to the rest of kiln (1, 2, 4, 8, 16 and 64 are taken on `main` as of
2026-10-17); CI's bits start at `1 << 10` so the two never collide. The variants below are on
`main` (`proto.rs`): `ServerMsg` 23–27 and `AgentMsg` 27–28 on the wire.

Appended variants (after every existing one, in this order):

```rust
// ServerMsg
AssignV2 { json: String },                        // AssignmentV2
SecretsFor { job: String, json: String },         // late-bound secrets (environment approval granted)
JobTerminalOpen { job: String, session: String, cols: u16, rows: u16 },  // W17
KeepRelease { job: String },                      // W17: a kept workspace may go
ProbeFacts,                                       // W13: send Facts now
// AgentMsg
JobEvent { job: String, json: String },           // JobEventV2 (below)
Facts { json: String },                           // W13 RunnerFacts
```

```ts
interface AssignmentV2 {
  job_id: string; run_id: string; attempt: number; repo: string; sha: string; ref: string;
  dialect: "gha" | "kiln";
  leg: { job_id: string; name: string; matrix: Record<string, Json>; index: number; total: number };
  executor: { kind: "host"|"docker"|"firecracker"|"shards"|"k8s"; image?: string; options?: Json };
  container?: ContainerSpec; services: Record<string, ContainerSpec>;    // W13
  steps: StepSpecV2[];                       // unevaluated where GHA evaluates on the runner (W06)
  contexts: { github: Json; env: Record<string,string>; vars: Record<string,string>;
              needs: Json; strategy: Json; matrix: Json; inputs: Json; job: Json };
  secrets: { name: string; value: string }[];  // only those the leg's expressions reference (W09)
  masks: string[];                             // extra values to mask
  defaults: { shell?: string; working_directory?: string };
  timeout_minutes: number; continue_on_error: boolean;
  runtime: { results_url: string; runtime_token: string; cache_url: string;   // W08
             id_token_url?: string; id_token_request_token?: string;          // W10, only with id-token: write
             facade_url: string; api_url: string; graphql_url: string; github_token: string };
  checkout: "none" | "auto";                    // kiln dialect's implicit checkout
  outputs: Record<string, string>;              // job outputs expressions, evaluated after the steps
  cache_hints: Json; reuse?: { input_hash: string };   // W08
  debug: { runner_debug: boolean; step_debug: boolean; keep_on_failure_minutes?: number };  // W17
  network?: { egress: "open"|"none"|{ allow: string[] } };  // W18
  limits: { output_bytes: number; summary_bytes: number; log_bytes: number };
}
type JobEventV2 =
  | { t: "step_started"; idx: number; name: string; kind: "run"|"uses"|"pre"|"post"|"internal"; action?: string }
  | { t: "step_finished"; idx: number; outcome: Conclusion; conclusion: Conclusion; exit_code?: number; outputs: Record<string,string> }
  | { t: "annotation"; idx: number; level: "error"|"warning"|"notice"|"debug"; message: string; title?: string; file?: string; line?: number; end_line?: number; col?: number; end_col?: number; log_offset: number }
  | { t: "group"; idx: number; op: "start"|"end"; title?: string; log_offset: number }
  | { t: "mask"; value_sha256: string }          // a value was masked (never the value itself)
  | { t: "summary"; idx: number; markdown: string }
  | { t: "job_outputs"; outputs: Record<string,string> }
  | { t: "test_report"; idx: number; format: string; object: string }   // W16, uploaded to the blob store
  | { t: "metrics"; idx: number; points: Json[] }                        // W16
  | { t: "failure_class"; class: "infra"|"oom"|"disk_full"|"image_pull"|"timeout"|"failure" };  // W05
```

A runner without `CAP_CI2` keeps getting `Assign` (v1) for v1-shaped kiln jobs, so a fleet can be
upgraded one machine at a time. A leg that needs `CAP_CI2` is never routed to such a runner; its
wait reason says so (W05).

### 2.6 Server endpoints

| Prefix | What | Auth |
| --- | --- | --- |
| `/api/ci/…` | The CI API (§4) | session, PAT, agent token |
| `/twirp/…` (the toolkit keeps only `ACTIONS_RESULTS_URL`'s origin), `/_apis/artifactcache/…`, `/_kiln/blob/…` | GHA runtime services (W08) | the leg's `ACTIONS_RUNTIME_TOKEN`; blob URLs are signed |
| `/_kiln/oidc/token` | ID tokens for jobs (W10) | the leg's `ACTIONS_ID_TOKEN_REQUEST_TOKEN` |
| `<issuer>/.well-known/openid-configuration`, `<issuer>/jwks` (issuer default `<public_url>/oidc`) | The issuer's discovery and keys (W10) | open (the only parts that must be reachable from the internet for some providers; W10-R12) |
| `/_kiln/actions/…` | Action tarballs at pinned SHAs (W07) | job token |
| `/v2/…` | The built-in OCI registry (W12) | registry tokens (W12) |
| `/_kiln/proxy/<ecosystem>/…` | Package registry proxies (W12) | job token or PAT |
| `/<owner>/<repo>[.git]/info/refs` and `…/git-upload-pack`, `/api/v3/…`, `/api/graphql` | The facade: GitHub-compatible git and REST for jobs (W14) | job token as `GITHUB_TOKEN` |
| `/<owner>/<repo>/actions/runs/<id>…` | GitHub-shaped page links that actions print, redirected to kiln's pages (W14) | open (the pages ask for a session) |
| `/git/<owner>/<repo>.git` | Native git hosting (W14): clone, fetch and push | PAT, agent token, job token |

The facade lives at the **root** of the server's origin, because `actions/checkout` builds its
URLs from the origin of `GITHUB_SERVER_URL` and drops any path. Its paths can't collide with
kiln's own:
- the server and the UI never use a path ending in `info/refs` or `git-upload-pack`;
- `/api/v3` and `/api/graphql` are GitHub Enterprise Server's API paths, which kiln's API doesn't use.

So `GITHUB_SERVER_URL` is the server's `public_url`. Jobs reach the facade through the same front
door and certificate as everything else, and no second listener is needed. (The first draft gave
the facade its own origin. Sharing one is simpler and just as safe, because the facade accepts
only a job's token and ignores cookies.)

---

## 3. Workstreams

Every workstream has the same parts: **Scope**, **Requirements** (`Rnn`), **Design**, **Interfaces**
(what it gives and takes from others), **Acceptance criteria** (`ACnn`, each with how it is
checked), **Scenarios** (`Snn`, concrete inputs and expected results) and **Progress**.

### W01 Expressions

**Scope.** The `${{ }}` language, GitHub-exact, with kiln's additions for the kiln dialect. One
implementation (`kiln_ci::expr`) used by the server (job-level keys) and the runner (step-level
keys), so both sides agree on every value.

**Requirements**

- **W01-R01 Lexer.** Tokens: `( ) [ ] . , ! < <= > >= == != && || *`, identifiers
  (`[A-Za-z_][A-Za-z0-9_-]*`: hyphens are allowed, as in `steps.my-step.outputs`), numbers in any
  form GitHub accepts (integers, decimals, exponents, `0x` hex, a leading `-`), single-quoted
  strings with `''` as the escaped quote, and the keywords `true`, `false`, `null`, `NaN` and
  `Infinity` (the last two are numbers; none is a keyword right after a `.`).
- **W01-R02 Grammar and precedence**, highest first: grouping `( )`, index `[ ]`, property `.`,
  `!`, `< <= > >=`, `== !=`, `&&`, `||`. Function calls are `identifier(args)`; names are
  case-insensitive. Object filters: `a.*.b` and `a[*]`.
- **W01-R03 Values.** `null`, booleans, numbers (f64), strings, arrays and objects. Context values
  are JSON. Property lookup on objects is case-insensitive. Lookup on `null` or a non-object gives
  `null`, never an error. An object filter over an array maps over its elements, over an object
  maps over its values, and flattens one level as GitHub does.
- **W01-R04 Coercion.** Equality and relational operators compare same-typed values directly
  (strings **case-insensitively**; arrays and objects only equal themselves). Mixed types are
  coerced to numbers: `null`→0, `true`→1, `false`→0, a string is parsed as a JSON number (the
  empty string is 0; anything unparsable is NaN), arrays and objects are NaN. Any comparison with
  NaN is false, except `!=`, which is true.
- **W01-R05 Logic.** `!x` is the negated truthiness. Falsy: `false`, `0`, `-0`, `""`, `null`,
  NaN; everything else (including empty arrays and objects) is truthy. `&&` and `||`
  short-circuit and return the deciding operand, not a boolean.
- **W01-R06 Functions**, GitHub-exact: `contains(search, item)` (substring for strings, element
  for arrays; case-insensitive; operands cast to strings where GitHub casts),
  `startsWith(s, prefix)`, `endsWith(s, suffix)` (case-insensitive), `format(fmt, …)` (`{N}`
  substitution, `{{` and `}}` escapes, an out-of-range index is an error), `join(array, sep=',')`
  (a non-array is cast to a string), `toJSON(v)` (pretty-printed with two spaces, as GitHub
  prints it), `fromJSON(s)` (an error on invalid JSON), `hashFiles(patterns…)` (runner only;
  W01-R09), `case(pred, value, …, default)` (an odd number of at least three arguments; each
  predicate must be a boolean), and the status functions `success()`, `always()`, `cancelled()`,
  `failure()`. Any function GitHub adds later is implemented to GitHub's semantics, decided by the
  oracle.
- **W01-R07 Strings from values** (interpolation): `null`→`""`, booleans→`true`/`false`, numbers
  as GitHub formats them (integers without a fraction, otherwise the shortest round-trip form),
  arrays→`Array`, objects→`Object`.
- **W01-R08 Templates.** A string holds any number of `${{ … }}`. A `}}` inside a string literal
  does not close the expression. Unclosed `${{` is a parse error with its column.
- **W01-R09 `hashFiles`.** Patterns are relative to `GITHUB_WORKSPACE`; `!` excludes; files
  outside the workspace are ignored; symlinks are followed within the workspace; the result is
  the hex SHA-256 of the concatenated SHA-256 digests of each matched file's contents, or `""`
  when nothing matches. **Deviation:** kiln sorts matches by path before hashing, so the value is
  stable across file systems; it therefore differs from GitHub's for the same files, which
  matters nowhere because cache keys are compared only within kiln.
- **W01-R10 `if:` semantics**, GitHub-exact. An `if:` value is an expression with or without
  `${{ }}`. When the expression calls none of the status functions, `success() &&` is implied. A
  value with literal text around a `${{ }}` is a string, so it is truthy when non-empty (W02 warns
  about this: diagnostic `KCI6001`).
- **W01-R11 Status functions** see a `StatusView` supplied by the caller: for steps, the results
  of the earlier steps of the leg and whether the run is cancelled; for jobs, the results of the
  job's needs, transitively (GitHub's `failure()` is true when any ancestor failed).
- **W01-R12 Introspection.** `Expr::references()` returns every context path an expression reads
  (`secrets.NPM_TOKEN`, `needs.build.outputs.digest`, `matrix.os`, `github.event.pull_request`),
  with wildcards where filters or computed indexes are used. W02 uses it to check context
  availability (Appendix B), W09 to send a leg only the secrets it references, W04 to find
  dependencies, W08 to compute reuse keys.
- **W01-R13 Bounds.** GitHub's: expression text ≤ 21,000 characters (UTF-16 code units), nesting
  depth ≤ 50, and 1 MiB of memory created per evaluation ("The maximum allowed memory size was
  exceeded"). kiln's: at most 100,000 evaluation steps and 1 MiB of result. Exceeding a bound is an
  error naming it.
- **W01-R14 kiln dialect additions** (refused in the GHA dialect with `KCI5001`):
  - double-quoted strings (v1 files use them);
  - the v1 bare names `event`, `branch`, `repo`, `sha` (aliases of `github.event_name`,
    `github.ref_name`, `github.repository`, `github.sha`);
  - contexts `git` (`sha`, `short_sha`, `branch`, `ref`, `message`, `author`), `kiln` (`url`,
    `run_url`), `run` (`id`, `number`, `attempt`, `env`: W04-R17), `diff` and `stack` (W14), and
    `runner.facts` (W13);
  - functions `changed(glob, …)` (whether the trigger changed matching files; W03-R12),
    `matches(s, regex)` (RE2 syntax, linear time), `coalesce(a, b, …)` (the first non-empty
    value), `split(s, sep)`, `replace(s, from, to)`, `lower(s)`, `upper(s)`,
    `trim(s)`, `fromYAML(s)`, `toYAML(v)`.

  (`case` is GitHub's own, available in both dialects; see W01-R06.)

  **Note:** v1 compared strings case-sensitively. Under W01-R04 they compare case-insensitively.
  No v1 condition in the corpus (W01-AC09) changes its result.

**Design.**

```rust
// crates/kiln-ci/src/expr/mod.rs
pub enum Dialect { Gha, Kiln }
pub struct Expr { /* AST + source + flags */ }
impl Expr {
    pub fn parse(src: &str, d: Dialect) -> Result<Expr, ExprError>;     // a bare expression
    pub fn uses_status_function(&self) -> bool;
    pub fn references(&self) -> Vec<ContextPath>;
}
pub struct Template { /* literal and expression parts */ }
impl Template { pub fn parse(src: &str, d: Dialect) -> Result<Template, ExprError>; }
#[derive(Clone)] pub enum Value { Null, Bool(bool), Number(f64), String(Arc<str>),
    Array(Arc<[Value]>), Object(Arc<Map>) }                         // Send + Sync
pub trait Contexts {                                              // lazily supplies contexts
    fn get(&self, name: &str) -> Option<Value>;                    // "github", "matrix", …
    fn status(&self) -> StatusView;
    fn hash_files(&self, patterns: &[String]) -> Result<String, ExprError>;   // runner only
    fn changed(&self, globs: &[String]) -> Result<bool, ExprError>;           // kiln dialect
}
pub fn eval(e: &Expr, cx: &dyn Contexts, b: &mut Budget) -> Result<Value, ExprError>;
pub fn eval_if(e: &Expr, cx: &dyn Contexts, b: &mut Budget) -> Result<bool, ExprError>;
pub fn render(t: &Template, cx: &dyn Contexts, b: &mut Budget) -> Result<String, ExprError>;
pub struct ExprError { pub code: &'static str, pub message: String, pub column: Option<u32> }
```

Parsing is a hand-written Pratt parser (no parser-generator dependency). Compiled expressions are
cached by `(dialect, source)` in the planner and the runner.

**Interfaces.** Gives the above to W02, W04, W06, W09, W15 and the editor (W21, through
`/api/ci/validate`). Takes nothing.

**Acceptance criteria**

- **W01-AC01 Oracle.** GitHub's own expression test vectors (the `expressions` package of
  `github.com/actions/languageservices`, MIT) are vendored under `crates/kiln-ci/tests/oracle/` with
  their licence and a script that refreshes them. Every vector passes: values, errors and error
  positions. *Unit test.*
- **W01-AC02 Coercion table.** A test covers every pair of the six types for each of the six
  comparison operators, with the expected results written out from W01-R04. *Unit test.*
- **W01-AC03 Robustness.** A property test feeds 10⁶ random and mutated inputs to `Expr::parse`,
  `Template::parse` and `eval`: no panic, no stack overflow, every bound reported by name; and
  printing a parsed AST then reparsing it gives the same AST. *Property test.*
- **W01-AC04 Templates.** Multiple expressions, literal `}}` inside string literals, unclosed
  `${{` errors with the right column. *Unit test.*
- **W01-AC05 `if:`.** Implied `success()`, explicit status functions anywhere in the tree, the
  literal-text pitfall, cancelled runs with `always()`. *Unit test.*
- **W01-AC06 `hashFiles`.** Over fixture tree F-01: exclusions, no match → `""`, files outside the
  workspace ignored, symlink inside followed, symlink pointing outside ignored, order-stable.
  *Unit test.*
- **W01-AC07 Speed.** `cargo bench -p kiln-ci --bench expr`: ≥ 1,000,000 evaluations per second
  per core of `github.event_name == 'push' && contains(github.ref, 'refs/heads/')`. *Bench in CI,
  with the threshold asserted.*
- **W01-AC08 Dialects.** Every W01-R14 addition is refused in the GHA dialect with `KCI5001` and
  accepted in the kiln dialect. *Unit test.*
- **W01-AC09 v1 regression.** Every `if:` and `${{ }}` in the v1 corpus (F-02: kiln's own
  `.kiln/*.yaml` and the hyper-light repositories' `.kiln/` files as of 2026-10-17) evaluates to
  what `pipeline::Context` gives, for each event and branch the corpus runs on. *Unit test.*

**Scenarios**

| ID | Expression and contexts | Result |
| --- | --- | --- |
| W01-S01 | `github.event_name == 'PUSH'` with `event_name: push` | `true` |
| W01-S02 | `contains(github.event.pull_request.labels.*.name, 'Bug')` with labels `[{name: bug}]` | `true` |
| W01-S03 | `format('{0}-{{x}}', 'a')` | `a-{x}` |
| W01-S04 | `'1' == 1`, `null == 0`, `'' == 0`, `'abc' < 'abd'`, `fromJSON('[1]') == fromJSON('[1]')` | `true`, `true`, `true`, `true`, `false` |
| W01-S05 | `inputs.flag && 'yes' \|\| 'no'` with `flag: false` | `'no'` |
| W01-S06 | `if: ${{ github.ref }} == 'refs/heads/main'` (literal outside) | truthy string; `KCI6001` warning |
| W01-S07 | `toJSON(matrix)` with `{os: linux}` | `"{\n  \"os\": \"linux\"\n}"` |
| W01-S08 | kiln: `changed('crates/**') && branch == 'main'` on a push touching `crates/a.rs` to main | `true` |

**Progress.** Done (2026-10-17). `crates/kiln-ci/src/expr/` (value, parser, eval, files) and
`crates/kiln/src/runner/hashfiles.rs`. All of GitHub's 1,020 expression test vectors pass, including
the five its TypeScript implementation skips (kiln follows the C# runner: absolute error positions,
the memory bound); the v1 corpus gives identical results for 16,992 templated strings and 40
conditions; 3.2 million evaluations per second (`cargo bench -p kiln-ci --bench expr`). Per-key
context and function limits are `ParseOptions` (for W02's Appendix B checks); `Condition` reads an
`if:` the way GitHub does; `eval_value` keeps a single expression's type.

### W02 Workflow model, dialects and validation

**Scope.** Reading workflow files of both dialects and overlays into one typed model (the IR),
reading action metadata, validating with precise diagnostics, and exporting JSON Schemas for the
editor. Includes `kiln validate` and `POST /api/ci/validate`.

**Requirements**

- **W02-R01 Where workflows are.**

  | Path at the commit | Dialect |
  | --- | --- |
  | `.github/workflows/*.yml`, `*.yaml` (not subdirectories, as GitHub) | `gha` |
  | `.kiln/*.yml`, `*.yaml`, except the reserved policy files `review`, `owners`, `environments`, `agents`, `quarantine`, `routing`, `notify`, `deploy`, `config`, `secrets` (any extension) | `kiln` |
  | `.kiln/overlays/<stem>.yaml` | overlay for `.github/workflows/<stem>.y(a)ml` |
  | `.github/actions/**/action.y(a)ml`, `.kiln/actions/**/action.y(a)ml` | action metadata |

- **W02-R02 Which ones run.** The repository setting `ci.github_workflows` is `auto` (default),
  `on` or `off`. `auto` runs `.github/workflows` when the commit has no kiln-dialect workflow, and
  ignores them otherwise; `on` runs both; `off` never runs them. The repository page and the run
  list say which applies and why ("GitHub workflows not run: this commit has .kiln/ci.yaml"). When
  neither dialect has a workflow, auto-detected pipelines run as today (`gitops.auto_pipelines`).
- **W02-R03 YAML.** Files are parsed with a span-preserving YAML 1.2 parser (core schema), so every
  diagnostic points at a line and column. `on` is a string key (not YAML 1.1's boolean). Anchors
  and aliases resolve as GitHub resolves them. Duplicate keys are an error. Files over 1 MiB, nesting
  deeper than 256, more than 200,000 nodes, or alias expansions over 1 MiB are refused with
  `KCI1003` (billion-laughs safe).
- **W02-R04 The IR.** `kiln_ci::wf::Workflow` holds every key of Appendix A and Appendix E with its
  span, with every expression-bearing value stored parsed (`Expr` or `Template`). It is
  serializable (JSON) and hashed (`def_hash`, SHA-256 of the canonical JSON) for caching and reuse.
- **W02-R05 GHA dialect is strict.** Unknown keys are errors with a "did you mean" suggestion
  (edit distance ≤ 2). Values are type-checked. Job ids match `^[A-Za-z_][A-Za-z0-9_-]*$`.
  `runs-on` is required unless the job `uses` a reusable workflow. A step has exactly one of
  `run` and `uses`. `needs` names existing jobs and forms no cycle. Every `on` form (string, list,
  map) and each event's keys and activity types are checked. `permissions`, `concurrency`,
  `environment`, `container`, `services`, `strategy`, `secrets: inherit`, `with` and reusable
  `uses` forms are checked. Contexts are checked against where they are available (Appendix B):
  a context that is not available at a key is an error, as on GitHub.
- **W02-R06 kiln dialect** is the GHA dialect plus the keys of Appendix E, plus the v1 forms below,
  each normalized to the IR so that v1 files plan exactly as they do today (W02-AC04):

  | v1 form | IR |
  | --- | --- |
  | job `matrix: { axis: [..] }` | `strategy.matrix` |
  | `runs-on` absent | `[ci] default_runs_on` from server config (`[k8s]` when Kubernetes is enabled, otherwise any runner) |
  | `on.manual: true` (default) | `workflow_dispatch: {}`; `false` removes it |
  | `on.tag: { branches: [globs] }` | `push.tags` |
  | `on.schedule: ["cron", …]` | `schedule: [{cron}, …]` |
  | `retries: n` | `retry: { max: n, on: [runner-lost] }` (W05-R14) |
  | `artifacts: [globs]` | an implicit upload step at the end of the leg, artifact name `artifacts` (v1 tar compatibility kept for `GET /api/jobs/:id/artifacts`) |
  | `cache: [dirs]` | kept as written: a string in `cache:` is a v1 directory kept on the runner between legs, which W08-R09 reads as `{ local: [dir] }` |
  | `toolchain`, `image`, `exclusive`, `resources` | kept (Appendix E) |
  | implicit source checkout | `checkout: auto` (GHA dialect: none; workflows use `actions/checkout`) |
  | `timeout-minutes` default 60 | kept for the kiln dialect; GHA dialect default 360 |
  | `$KILN_OUTPUT` | the same file as `$GITHUB_OUTPUT` |

- **W02-R07 Overlays.** An overlay adds kiln keys to a GHA workflow without editing it, so the
  workflow keeps running on GitHub. It may contain the workflow-level kiln keys and
  `jobs.<id>.<kiln key>` and `jobs.<id>.steps.<step id>.<kiln key>` (steps addressed by their
  `id`). It may also replace `runs-on` (to map GitHub's labels to kiln's) and nothing else of
  GHA's. Overlays are read at the same commit as the workflow. Each overlay key a job ignores
  because the workflow has no such job or step is a warning (`KCI4010`).
- **W02-R08 Diagnostics.** `{ code, severity: "error"|"warning"|"info", message, path, range:
  {start:{line,col}, end:{line,col}}, fix?: { title, edits: [{range, text}] } }`. Codes are stable
  and documented: `KCI1xxx` structure, `KCI2xxx` expressions, `KCI3xxx` contexts, `KCI4xxx`
  references (jobs, steps, actions, inputs, secrets), `KCI5xxx` kiln-only in GHA, `KCI6xxx` lints.
  The lints (warnings) include:
  - `KCI6001`: literal text around `${{ }}` in `if:`;
  - `KCI6002`: an action not pinned to a full SHA (info unless the repository requires pins);
  - `KCI6003`: `pull_request_target` that checks out the head ref;
  - `KCI6004`: untrusted input (`github.event.*.title`, `body`, `head_ref`, …) interpolated into
    `run:` (script injection), with a fix that moves each such expression into the step's `env:`
    and reads the variable instead, quoted for the shell (`bash`/`sh`, `pwsh`, `cmd`; no fix for
    other shells, flow-style steps, or text whose position can't be confirmed);
  - `KCI6005`: `secrets.*` interpolated into `run:` rather than passed by `env`;
  - `KCI6006`: a deprecated `::set-output`/`::save-state`;
  - `KCI6007`: an unknown `runs-on` label that no routing rule maps and no runner has (checked
    where routing rules and runner facts are known: on the server, with W05 and W13).
- **W02-R09 Schemas.** JSON Schemas (draft 2020-12) for the GHA dialect, the kiln dialect, overlays
  and action metadata are generated from the IR's definitions (one source), with descriptions taken
  from Appendix A and Appendix E. They are served at `GET /api/ci/schema/{gha,kiln,overlay,action}`
  and drive the editor's completion and hover (W21).
- **W02-R10 Action metadata.** `action.yml` is read into `kiln_ci::wf::Action`: `name`,
  `description`, `author`, `inputs` (`description`, `required`, `default`, `deprecationMessage`),
  `outputs` (`description`, `value` for composites), `runs` (`using`: `node12`, `node16`, `node20`,
  `node24`, `composite`, `docker`; `main`, `pre`, `post`, `pre-if`, `post-if`, `image`,
  `entrypoint`, `pre-entrypoint`, `post-entrypoint`, `args`, `env`, `steps`), `branding` (kept,
  unused). Unknown keys are warnings (GitHub tolerates them). kiln actions add `secrets`,
  `requires` and `permissions` (W07-R11, W09-R07).
- **W02-R11 Loading and caching.** The loader (`crates/kiln/src/ci/load.rs`) lists the paths of
  W02-R01 at the commit (`git ls-tree`), reads each blob, parses, validates and stores the result
  in `ci_workflows` keyed by `(repo, path, content key)` (the blob's id, and the overlay's for a
  GitHub workflow that has one), so an unchanged file is never parsed twice. GitHub workflows that
  don't run (W02-R02) are read too, so pages can show them.
  A file with errors yields a run with conclusion `startup_failure` that shows the diagnostics
  (as v1 shows a failed run today), never a silent skip.
- **W02-R12 CLI and API.** `kiln validate [--dialect gha|kiln] [--json] FILE…` prints diagnostics
  as `file:line:col: severity code: message` and a plan outline; exit status 1 on any error.
  `POST /api/ci/validate { path, content, dialect?, repo? }` answers `{ kind, dialect,
  diagnostics, outline?: { name, dialect, events, jobs: [{ id, name, needs, runs_on, uses,
  legs_estimate, steps }] } }`: the file is read as what its path says (workflow, overlay, action,
  policy file), and `repo` adds the checks that need the repository (routing, actions).
  `GET /api/ci/repos/:repo/workflows?ref=` lists a ref's workflows, each with `runs` and the
  reason GitHub's run or don't; `GET /api/ci/repos/:repo/workflows/:path?ref=` gives one workflow's
  source, model and diagnostics. `:repo` is the repository's id or its name, URL-encoded.
- **W02-R13 Speed.** Parsing and validating a 2,000-line workflow takes < 20 ms.

**Design.** `crates/kiln-ci/src/wf/`: `yaml.rs` (a span-preserving node tree over yaml-rust2's
events), `keys.rs` (every key of both dialects with its type and description: the one source for
checks, schemas and the editor), `ir.rs` (the model), `front.rs` (node tree → IR + diagnostics,
both dialects), `overlay.rs`, `action.rs`, `validate.rs` (needs, cycles, contexts per Appendix B,
references, lints and their fixes), `schema.rs` (JSON Schemas from `keys.rs`), `diag.rs`, and
`check()` (one file, as its path says). The v1 `pipeline::parse` keeps running v1 until W04;
`pipeline::from_ir` maps the new model to v1's, which is how W02-AC04 compares them.

**Interfaces.** Gives the IR to W03 (`on`), W04 (everything else), W07 (action metadata), W20
(validation for agents), W21 (schemas, diagnostics). Takes W01's parser.

**Acceptance criteria**

- **W02-AC01 Corpus, positive.** Every workflow in the GHA corpus (F-03: at least 300 real
  workflow files from popular public repositories and from GitHub's starter workflows, vendored
  with their licences) parses with zero errors, and its IR round-trips through JSON. *Unit test.*
- **W02-AC02 Corpus, negative.** Each file of F-04 (one per diagnostic code) yields exactly its
  expected code at its expected line and column. *Unit test.*
- **W02-AC03 Schemas.** Every positive-corpus file validates against the exported schema of its
  dialect; every negative file whose error is structural fails it. *Unit test.*
- **W02-AC04 v1 compatibility.** For every file of F-02, the kiln front end plus the W04 planner
  produce the same jobs, keys, needs, labels, images, environment, conditions and steps as v1's
  `pipeline::plan` for the same context. *Unit test.*
- **W02-AC05 Overlays.** Fixture F-05: an overlay adds `retry`, a routing preference and a matrix
  `where` to a GHA workflow and maps `ubuntu-latest`; the IR shows them; the same workflow without
  the overlay is unchanged; an overlay naming a missing step warns `KCI4010`. *Unit test.*
- **W02-AC06 Robustness.** A property test mutates corpus files (10⁵ cases): no panic, every
  failure is a diagnostic with a range inside the file. *Property test.*
- **W02-AC07 Speed.** A 2,000-line workflow parses and validates in < 20 ms. *Bench, asserted.*
- **W02-AC08 Precedence.** A commit with `.github/workflows/ci.yml` and `.kiln/ci.yaml` under
  `auto` runs only `.kiln/ci.yaml`, and the run list says why; under `on`, both run; under `off`,
  only `.kiln/ci.yaml`. *Integration test.*
- **W02-AC09 Startup failure.** Pushing a workflow with a syntax error produces one run with
  conclusion `startup_failure` whose page lists the diagnostics with line numbers. *E2E on the
  live server.*

**Scenarios**

- **W02-S01** `on: push` with `jobs.build.runs-on: ubuntu-latest` and a step `uses:
  actions/checkout@v4` → valid; `KCI6002` info (tag, not SHA).
- **W02-S02** A step with both `run` and `uses` → `KCI1012` at the step's line.
- **W02-S03** `jobs.a.if: ${{ secrets.X == '' }}` → `KCI3001` ("secrets is not available in
  jobs.<id>.if"), as GitHub refuses it.
- **W02-S04** `.kiln/ci.yaml` from v1 (F-02, kiln's own file) → same 62 e2e jobs as v1 plans.
- **W02-S05** `run: echo "${{ github.event.pull_request.title }}"` → `KCI6004` with a fix that
  moves the value to `env:`.

**Progress.** Done, except W02-AC09. That criterion needs runs (W03, W05); the loader already
returns a broken file with its diagnostics for the run to show. `[ci] kiln_workflows` (off by
default) lets the engine run `.kiln/` workflows too, v1 standing aside for a commit the engine
took: the kiln dialect's outputs, exports and implicit checkout run on it end to end.

- *Code.*
  - `crates/kiln-ci/src/wf/` (above).
  - `crates/kiln/src/ci/load.rs`: the loader, reading through a `Source` trait. The server's
    source is the repository's mirror and `ci_workflows` (migration `20261017-1200_w02_workflows`);
    tests use a plain repository.
  - `ci/workflows.rs`: the routes of W02-R12.
  - `kiln validate` in `main.rs`.
  - `pipeline::from_ir`.
- *Tests.*
  - AC01, `crates/kiln-ci/tests/wf_corpus.rs`: 457 files (F-03: GitHub's starter workflows and 25
    repositories), zero errors, and the model round-trips through JSON.
  - AC02, `tests/wf_negative.rs`: 32 codes, each at its exact line and column (F-04).
  - AC03, `tests/wf_props.rs`: the corpus conforms to the schema, and structural errors fail it.
  - AC04, `crates/kiln/tests/v1_compat.rs`: 30 v1 files, 240 plans, 2,392 planned jobs, all
    identical, plus S04's 62 jobs.
  - AC05: a unit test, and the loader applying an overlay in `ci_load.rs`.
  - AC06: 10⁵ mutations in 9.5 s in release, run with `KILN_CI_MUTATIONS=100000` (CI runs 1,500
    in debug and 20,000 in release). A watchdog fails any case that runs past 30 s and reports its
    input. That watchdog found the needs-cycle check looping forever on a job that needs itself.
  - AC07: a 2,000-line workflow in under 20 ms in release.
  - AC08, `crates/kiln/tests/ci_load.rs`: real commits under `auto`, `on` and `off`, plus overlays,
    broken files and the cache.
  - S01–S05: unit tests. S05's fix is applied to the file and the result read again.
- *Choices.*
  - YAML goes through yaml-rust2's events, for positions. serde_yaml is a lenient fallback for the
    rare file GitHub accepts that yaml-rust2 refuses (info `KCI1004`, e.g. cpython's `mypy.yml`).
  - GitHub's 2026 keys are in the model and checked:
    - background steps (W06-R17);
    - `concurrency.queue` (W05-R06);
    - `cache-mode` (W08-R14);
    - `environment.deployment` (W15-R04);
    - `$/` references (W07-R01, W04-R24);
    - the `copilot-requests` permission;
    - `snapshot`, kept as written.
  - Where GitHub's context table has no row for a key, the key is checked as the closest key that
    has one (Appendix B).
- *Done.* The checks that need the server, and saving:
  - `KCI6007`. With `repo`, `/api/ci/validate` (and the editor, which sends it) warns of each
    `runs-on` label that no routing rule maps under any of the workflow's events, no runner
    carries (its labels, its name alone, and those GitHub gives every self-hosted runner, from its
    facts) and no pool gives its runners, at the label. Labels written as expressions are the
    run's to know.
  - The repository's action policies (W07-R04): a use they refuse is the error its run would
    meet (`KCI4020`, `KCI4021`, at the step), and a pin they require takes the place of
    `KCI6002`'s hint. Both checks apply only where kiln runs the workflow: KilnCI among the
    repository's providers (W23-R01), and for a GitHub workflow, one kiln runs (W02-R02).
  - `POST /api/ci/repos/:repo/workflows/:path/edit` (W21-R06): checked as `validate` checks it
    with its repository, refused with its errors, then saved as a diff, a change on the
    repository's host, a commit to a branch, or a patch.
  - Tests: `crates/kiln/tests/workflow_edit.rs`; the App's part in `tests/github_app.rs`.

### W03 Triggers and events

**Scope.** Turning everything that happens (pushes, pull requests, diffs, reviews, schedules,
manual and API calls, agents, other runs, deployments, inbound webhooks, registry pushes) into
normalized events, matching them against `on:`, synthesizing GitHub-shaped payloads, and
deduplicating. Builds on today's `gitops.rs` (discovery, `ls-remote` polling, the GitHub webhook
as a hint).

**Requirements**

- **W03-R01 Event model.** `kiln_ci::trigger::Event { name, action, repo, ref, sha, before,
  base_ref, head_ref, actor, trust, source, payload, changed: Changed }`. `name` is a GitHub event
  name or a kiln event (W03-R09). `source` is `poll`, `webhook`, `app`, `native`, `api`,
  `schedule`, `workflow_run`, `agent`, `cd` or `hook`. `changed` is the changed-file set, computed
  lazily (W03-R12).
- **W03-R02 Sources.**

  | Source | Events it can produce |
  | --- | --- |
  | `git ls-remote` polling (no token) | `push` (branches and tags), `create`, `delete`, `pull_request` (`opened`, `synchronize` from `refs/pull/N/head`) |
  | GitHub webhooks (`/api/hooks/github`, HMAC) and GitHub App events | every event GitHub sends, with its payload |
  | native repositories (W14) | `push`, `create`, `delete`; per diff version `pull_request` (`opened` for v1, `synchronize` later, `closed` on land or abandon) plus kiln `diff`; land candidates `merge_group` (`checks_requested`) plus kiln `land`; reviews `pull_request_review`; comments `pull_request_review_comment` and `issue_comment` |
  | API, CLI, UI | `workflow_dispatch`, `repository_dispatch`, reruns |
  | the scheduler | `schedule` |
  | runs finishing | `workflow_run` (`requested`, `in_progress`, `completed`), kiln `pipeline` |
  | agents (W20) | kiln `agent` |
  | deployments (W15) | `deployment`, `deployment_status`, kiln `deploy` |
  | inbound webhooks (`POST /api/ci/hooks/:id`, HMAC-SHA256) | kiln `webhook` |
  | the registry (W12) | `registry_package` (`published`) |

- **W03-R03 Matching**, GitHub-exact: the `on` forms; each event's default activity `types`
  (e.g. `pull_request`: `opened`, `synchronize`, `reopened`); `branches`/`branches-ignore`,
  `tags`/`tags-ignore` (a push of a tag does not run a workflow that filters only branches, and
  vice versa) and `paths`/`paths-ignore` (both must match when both kinds are present).
- **W03-R04 Filter patterns**, GitHub-exact: `*` (any characters but `/`), `**` (any characters),
  `?` and `+` (zero-or-one and one-or-more of the **preceding character**, as GitHub defines them),
  `[]` classes, `!` negation where later patterns override earlier ones, `\` escapes. A module
  `kiln_ci::trigger::pattern` implements exactly the examples of GitHub's filter pattern cheat
  sheet (W03-AC02).
- **W03-R05 Changed paths.** For `push`: the two-dot diff `before..after`; for a new branch, the
  diff against the merge base with the default branch; for `pull_request`: the three-dot diff
  against the base. **Deviation:** kiln evaluates every changed file (GitHub looks at the first
  300).
- **W03-R06 Skipping by message**, GitHub-exact for `push` and `pull_request`: `[skip ci]`,
  `[ci skip]`, `[no ci]`, `[skip actions]`, `[actions skip]` in the head commit message, or a
  `skip-checks: true` trailer. kiln events honour it only when the workflow says
  `kiln: { honour-skip-ci: true }`.
- **W03-R07 Payloads.** `$GITHUB_EVENT_PATH` holds the GitHub-shaped payload. Webhook and App
  events pass GitHub's payload through. For polled and native events kiln synthesizes it: for
  `push` the `ref`, `before`, `after`, `created`, `deleted`, `forced`, `base_ref`, `compare`,
  `commits` (up to 20, each with `id`, `tree_id`, `message`, `timestamp`, `author`, `committer`;
  as in GitHub's payload for Actions, no `added`, `removed` or `modified`), `head_commit`,
  `repository`, `pusher`, `sender`; for
  `pull_request` the `action`, `number`, `pull_request` (`number`, `title`, `body`, `state`,
  `draft`, `user`, `labels`, `head` and `base` with `ref`, `sha`, `repo`, and GitHub's counts:
  `commits` on the head and not the base, `additions`, `deletions` and `changed_files` since their
  merge base), `repository`, `sender`.
  Fields kiln cannot know (labels of a polled PR without a token) are absent, and
  `payload.kiln.partial` is `true`. Every synthesized payload validates against GitHub's published
  webhook schemas (W03-AC04).
- **W03-R08 Trust.** `pull_request` from a fork, or by a user without write access to a native
  repository, is `untrusted` (W18). `pull_request_target` and `workflow_run` run the **base**
  repository's workflow from its default branch and are trusted, GitHub-exact.
- **W03-R09 kiln events** (kiln dialect, or overlays on GHA workflows):

  | Event | Fires on | Filters |
  | --- | --- | --- |
  | `diff` | each new version of a diff | `branches` (trunks), `paths`, `paths-ignore`, `drafts`, `authors: [human, agent]` |
  | `land` | the land queue's candidate (W14) | `branches` |
  | `review` | a review on a diff | `states: [accepted, changes_requested, commented]` |
  | `comment` | a comment on a diff, run or thread | `mentions: [names]`, `commands: ["/rerun", …]` |
  | `pipeline` | another workflow's run finishing (like `workflow_run`, by kiln name or path) | `{ name, conclusion: [success], branches }` |
  | `agent` | a kiln agent asks for a run (W20) | `agents: [names or *]`, `inputs` (typed, as `workflow_dispatch`) |
  | `deploy` | a CD event (W15) | `{ app, environment, states: [synced, degraded, failed] }` |
  | `webhook` | an inbound webhook | `{ hook: <id>, filter: <expression over payload> }` |
  | `schedule` (extended) | cron | each entry may add `timezone`, `branch`, `inputs`, `jitter` (opt-in), `skip-if-unchanged` (opt-in), `catch-up` (opt-in) |

- **W03-R10 Schedules**, GitHub-exact cron: five fields, UTC, with the syntax GitHub accepts,
  run on the latest commit of the default branch. **Deviation:** no five-minute minimum, and no
  automatic disabling after inactivity. kiln additions (opt-in, per entry): `timezone` (IANA,
  with DST handled: a nonexistent local time runs at the next valid minute, a repeated one runs
  once), `jitter: 5m`, `skip-if-unchanged: true` (skip when the branch has not moved since the
  last successful scheduled run with the same inputs), `catch-up: true` (one run for missed
  times after downtime; GitHub drops them, and so does kiln by default).
- **W03-R11 `workflow_dispatch` and `repository_dispatch`**, GitHub-exact: typed inputs (`string`,
  `boolean`, `choice`, `number`, `environment`), required and defaults, the workflow file must
  exist on the chosen ref; `repository_dispatch` with `event_type` and `client_payload`. kiln
  accepts up to 100 inputs (GitHub fewer).
- **W03-R12 `changed()` and reuse.** The changed-file set is computed once per event and shared by
  path filters, `changed()` (W01-R14) and W08's reuse keys. For diffs it is the diff's own files,
  not the stack below it.
- **W03-R13 `workflow_run` chains** are limited to 3 levels (GitHub-exact); the kiln dialect may
  raise the limit per workflow up to 10.
- **W03-R14 Deduplication.** Webhooks, App events and polls feed one ref tracker. A ref
  transition `(ref, before → after)` triggers once, whichever source sees it first; the others are
  recorded as "also seen by". Duplicate deliveries (`X-GitHub-Delivery`) are ignored for 24 h.
- **W03-R15 Latency.** A seen event becomes runs in < 500 ms (excluding fetching the commit), so
  webhook-driven repositories feel instant; polling stays the zero-configuration default.

**Design.** `crates/kiln-ci/src/trigger/` holds the pure matcher (`pattern.rs`, `matcher.rs`,
`payload.rs` building synthesized payloads from a `GitFacts` struct the caller fills).
`crates/kiln/src/gitops.rs` keeps discovery and polling and hands transitions to
`crates/kiln/src/ci/events.rs`, which fetches the commit, loads workflows (W02-R11), matches and
creates runs. The schedule loop moves to `ci/schedule.rs` with a time-zone-aware cron
(`kiln_ci::trigger::cron`, tested against DST tables).

**Interfaces.** Takes the IR (W02). Gives runs to the planner (W04). W14 and W15 call
`ci::events::emit(Event)`. W20 exposes `agent` triggers.

**Acceptance criteria**

- **W03-AC01 Matching table.** A table-driven test per event and filter, with every example from
  GitHub's workflow-syntax and events documentation, gives the documented match. *Unit test.*
- **W03-AC02 Patterns.** Every row of GitHub's filter pattern cheat sheet (branches, tags and paths
  tables) matches and fails to match exactly as listed. *Unit test.*
- **W03-AC03 Skip markers.** All six forms skip `push` and `pull_request`, are case-insensitive as
  on GitHub, and do not skip `workflow_dispatch`. *Unit test.*
- **W03-AC04 Payload schemas.** Synthesized `push`, `pull_request`, `workflow_dispatch`,
  `schedule`, `merge_group`, `workflow_run` and `release` payloads validate against the vendored
  `@octokit/webhooks` JSON schemas. *Unit test.*
- **W03-AC05 Dedup.** A webhook and a poll of the same push create one run; a redelivered webhook
  creates none. *Integration test.*
- **W03-AC06 Schedules.** Cron with time zones across both 2026 DST transitions in
  `Europe/Berlin` and `America/New_York`; `skip-if-unchanged`; `catch-up` after a simulated
  3-hour outage gives exactly one run. *Unit test.*
- **W03-AC07 Native events.** A diff's first and second versions produce `pull_request`
  `opened` and `synchronize` with the diff number, head and base; a GHA workflow `on:
  pull_request` runs on both. *Integration test, after W14.*
- **W03-AC08 Untrusted.** A pull request from a fork runs with `trust: untrusted`, no secrets and a
  read-only `GITHUB_TOKEN` (W18-AC03). *Integration test.*
- **W03-AC09 Latency.** 1,000 synthetic transitions → runs created with p99 < 500 ms (git fetch
  stubbed). *Bench, asserted.*

**Scenarios**

- **W03-S01** `on: { push: { branches: [main], paths: ['crates/**', '!crates/**/*.md'] } }`; a push to
  main touching only `crates/x/README.md` → no run; touching `crates/x/src/lib.rs` → a run.
- **W03-S02** `on: { push: { tags: ['v*'] } }`; pushing branch `v1` → no run; pushing tag `v1.2.0` → a
  run.
- **W03-S03** kiln `on: { diff: { paths: ['ui/**'] } }`; a stack D1 (ui) ← D2 (crates): D1 runs,
  D2 does not (its own files only).
- **W03-S04** `on: { schedule: [{ cron: '0 6 * * *', timezone: Europe/Berlin }] }` → runs at 04:00
  UTC in summer and 05:00 UTC in winter.
- **W03-S05** `on: { workflow_run: { workflows: [ci], types: [completed], branches: [main] } }` → a
  run after each completed `ci` run on main, with the triggering run in `github.event.workflow_run`.

**Progress.** In progress.

- *Done.*
  - `kiln_ci::trigger`, pure code:
    - `event` (R01).
    - `matcher` (R03, R09), with a reason for every "no".
    - `pattern` (R04): every row of GitHub's cheat sheet (AC02).
    - `skip` (R06, AC03).
    - `cron` (R10): time zones and DST, with AC06's two 2026 transitions in Berlin and New York;
      `jitter`; humane durations.
    - `context`: the `github` context, and payloads for push, pull request, dispatch, schedule and
      `workflow_run` (R07).
    - `dispatch`: `workflow_dispatch` inputs, typed and checked, with GitHub's messages (R11).
    - `github`: webhook deliveries as events, with GitHub's ref and commit for each event and
      forks untrusted (R07, R08).
  - `crates/kiln/src/ci/`:
    - `events.rs`, the one path from an event and a workflow to a run:
      - polled pushes, tags, pull requests and manual runs, with changed files (R05);
      - `startup_failure` runs for broken workflows;
      - `workflow_dispatch` and `repository_dispatch` (R11);
      - `workflow_run` (`requested`, `in_progress` and `completed`), with chains stopping at
        three levels (R13).
    - `schedule.rs` (R10): every entry fires once a minute at most, recorded in
      `ci_schedule_fires`, with kiln's opt-in `timezone`, `jitter`, `branch`, `inputs`,
      `skip-if-unchanged` and `catch-up`.
    - `hooks.rs`:
      - GitHub's webhooks with their payloads;
      - one trigger for a move a webhook and a poll both see, via a ref compare-and-set
        (`Store::advance_ref`) that polls use too;
      - redeliveries ignored for 24 hours (R14).
    - The dispatch routes: `/api/ci/repos/:repo/workflows/<path>/dispatches`,
      `/api/ci/repos/:repo/dispatches`, and the facade's `/api/v3/…/dispatches` for a trusted
      run's token.
  - All of it is behind `[ci] enabled`. With it off, a webhook only hints that its repository's
    poll is due.
  - Checked end to end on a local server:
    - push, then matrix legs, needs and outputs;
    - a per-minute schedule firing once a minute;
    - dispatches with typed inputs, and their refusals;
    - `repository_dispatch`;
    - `workflow_run` after each run;
    - an `issue_comment` delivery, and its redelivery doing nothing;
    - a push both delivered and polled, running once.
- *Done.* Native `pull_request_target` (R08, A.6): a diff's or change request's new version, and
  its close, raise it beside `pull_request`. The trunk's own workflows run, as the trunk is now
  (`GITHUB_SHA` its tip, `GITHUB_REF` the trunk), trusted and given the pull request. What the
  pull request's branch changed in its workflows never runs this way. Checked end to end
  (`w03_r08_native_pull_request_target`: a branch that rewrites the workflow, whose rewrite
  doesn't run), and `actions/labeler@v5` labels a change request from it (W22-AC02).
- *Done.* A diff's or change request's conversation, natively (R09, A.6), when a comment is
  published or a review given:
  - a comment on it as a whole raises GitHub's `issue_comment` (`created`), run from the default
    branch as on GitHub, the pull request in the payload as an issue;
  - a comment on its lines raises `pull_request_review_comment`, and a review
    `pull_request_review` (`submitted`, `approved`, `changes_requested` or `commented`), both run
    from its latest version as `pull_request` is;
  - kiln's `comment` raises from the trunk, with whom it mentions (`kiln.mentions`) for the
    `commands` and `mentions` filters, and kiln's `review` with the review's state.

  Checked end to end (`w03_r09_native_conversation_events`): a command, a remark, a line comment
  and an approval, kiln's `comment` running for the command alone.
- *Done.* What a job's `GITHUB_TOKEN` does starts no run, as on GitHub (but `workflow_dispatch`
  and `repository_dispatch`), so a job never starts itself again: its pushes (their moves are
  recorded before the poller looks), the versions and pull requests it makes, and its comments
  and reviews. Checked: a job that pushes a branch and opens a pull request for it, under a
  workflow that runs on both, leaves its one run alone (`w14_r11_pull_requests_jobs_open_and_label`).
- *Done.* The rest of the native table (R02, R09), checked end to end
  (`w03_r09_native_activity_and_kiln_events`, `w14_ac03_queue`):
  - a diff's or change request's `pull_request` activity, `pull_request_target` beside it: made
    ready (`ready_for_review`), edited (`PATCH /api/diffs/:id`: `edited`, the old title and body
    in `changes`), labeled and unlabeled (`/api/diffs/:id/labels`: `labeled`, `unlabeled`, the
    label in `label`), abandoned (`closed`, not merged), reclaimed (`reopened`); queued to land
    and out again (`enqueued`, `dequeued` with GitHub's reason: `MERGE`, `MANUAL`, `CI_FAILURE`,
    `INVALID_MERGE_COMMIT`, `BRANCH_PROTECTIONS`). A landed diff's `closed` is sent by whoever
    asked it to land. Payloads carry the pull request's labels.
  - kiln's `land` and `merge_group` for the land queue's candidates (W14).
  - kiln's `agent`: an agent asks for a run (`POST /api/ci/repos/:repo/agent`, MCP's
    `ci.runs.ask`) of a workflow that takes it, its `agents` naming the caller or `*`, its inputs
    typed as `workflow_dispatch`'s; the agent is the run's actor.
  - kiln's `webhook`: a repository's inbound hooks (`/api/ci/repos/:repo/hooks`, made with
    `secrets.manage`); a call to `POST /api/ci/hooks/:id` signed with HMAC-SHA256 over its body
    (`X-Kiln-Signature-256`, or GitHub's `X-Hub-Signature-256`) runs the default branch's
    workflows that take it, the body as the event, through their `hook` and `filter`. A hook's
    secret is derived from the server's master key and its id, never stored; an unknown hook
    and a bad signature answer alike.
  - kiln's `pipeline`, beside `workflow_run` when a run completes.
  - kiln's `deploy` (W15-R06): as KilnCD publishes its apps' statuses, an app turning synced
    (in sync and healthy, or a sync of it succeeding again), degraded, or whose sync failed
    raises it in the repository whose `.kiln/deploy.yaml` defines it.
  - R13: `kiln: { chain-depth }` lets a workflow take part in `workflow_run` and `pipeline`
    chains up to ten deep (GitHub's three otherwise).
- *Done.* AC05 as an integration test (`w03_ac05_a_webhook_and_a_poll_of_one_push_run_once`): a
  push that GitHub's webhook delivers and the poller also sees, a second later, runs once. A
  redelivery (the same `X-GitHub-Delivery`) does nothing, and a new delivery of the same push
  does nothing either.
- *Done.* AC08 is W18-AC03's end-to-end check: a fork pull request ran untrusted, with no
  secrets, no ID token, and a token whose writes through the facade got 403.
- *Done.* AC09 is measured by `kiln ci bench` (W22-AC08): a run created 1.8 ms after its push at
  the p99 (git stubbed), against 500 ms.
- *Done.* AC04 (`kiln_ci::trigger::payload_schemas`). The part of `@octokit/webhooks-schemas`
  7.6.1 these events need is vendored, with its MIT notice
  (`crates/kiln-ci/src/trigger/schemas/`). A small checker of the keywords it uses (`$ref`,
  `type`, `enum`, `required`, `properties`, `additionalProperties`, `items`, `allOf`, `oneOf`,
  `format`) runs over kiln's synthesized payloads, and they pass: `push`, `pull_request`
  (opened, synchronize with `before` and `after`, reopened, closed), `workflow_dispatch`,
  `merge_group` and `workflow_run` (requested, completed).
  - To pass, the payloads became GitHub's whole shape. Repositories have every field and API
    link (under the facade, or `api.github.com` for GitHub's own), and users are full user
    objects. A pull request has its links, `_links`, counts, states and full labels. A
    workflow run has its repositories as `repository-lite`, its check suite, its workflow and
    `head_commit`'s people. The land queue's `merge_group` is the candidate commit's real tree,
    message and people.
  - Ids kiln has no number for (a login, a label) are stable numbers from their names.
  - Two differences are GitHub's own: a push's commits leave out `added`, `removed` and
    `modified`, as GitHub's payloads for Actions do. kiln's namespace (`kiln`) is set aside.
  - `schedule` has no webhook, so it has no schema.
- *Done.* AC07 (`w03_ac07_a_diffs_versions_are_pull_request_events`): a diff's first version is
  `pull_request` `opened` and its second `synchronize`. Each carries the diff's number, the
  version's commit as the head and the trunk as the base, with GitHub's counts (`commits`,
  `additions`, `deletions`, `changed_files`; cpython's change detection fetches
  `--depth=$(( commits + 1 ))`, so a 0 there lost its merge base). A GitHub workflow
  `on: pull_request` ran on both and said so, and one with `paths: ['docs/**']` ran on neither:
  a hosted repository's diffs are read in its own repository, as their versions
  (`refs/kiln/diffs/…`) are never in its mirror, where their counts and changed files had been
  read and not found (changed files unknown, so every `paths:` filter ran).
- *Done.* AC01 (`kiln_ci::trigger::matcher`): GitHub's documented matching, example by example.
  - `push`: no filters; branches only, tags only and both, each ref by its own kind; the
    "Including", "Excluding" and "Including and excluding" branches and tags examples as written
    (`releases/**`, `!releases/**-alpha`, `v1.*`); a tag push not running a workflow that filters
    only branches, by inclusion or exclusion, and the other way round.
  - Paths: one matching file is enough, `paths-ignore` needs every file ignored, no changed files
    runs nothing, an unknown diff runs, tags skip path filters, branches and paths must both
    pass, and "Including and excluding paths" (`sub-project/**`, `!sub-project/docs/**`). Every
    changed file counts, not GitHub's first 300 (A.7).
  - Pull requests: the default types (opened, synchronize, reopened), branch filters on the base
    in the three forms for `pull_request` and `pull_request_target`, `paths: ['**.js']`.
  - Each event's `types` example in "Events that trigger workflows" (eighteen of them): the
    types listed run it and another doesn't; without `types` every activity runs it, but for the
    pull request events; events without types run for any.
  - `workflow_run`'s workflows, types and branches in the three forms; `merge_group`; each
    `schedule` entry; `repository_dispatch`'s types; `on: [push, fork]` and a map with some
    events filtered.
  - `branches` with `branches-ignore` (and tags', and paths') is an error (KCI1015).
- *Done.* The App's events (W14-R10): its signed webhook's events reach the workflows as GitHub's
  do, and GitHub's Re-run and a check suite's rerun rerun kiln's run (`w14_r10_github_app`).

### W04 Planner: graph, matrices, reusable workflows, outputs

**Scope.** From a workflow, an event and the repository's settings, the plan of a run: jobs,
needs, legs, conditions, outputs and environment flow, concurrency keys, permissions, reusable
workflows, and the incremental expansion of what can only be decided as jobs finish. GitHub-exact
for GHA semantics; kiln's matrix and data-flow additions on top. Pure code in
`kiln_ci::plan`.

**Requirements: the graph**

- **W04-R01 Run creation.** `plan::start(&Workflow, &Event, &Settings) -> Result<RunPlan, Diags>`:
  evaluates `run-name` (contexts `github`, `inputs`, `vars`) and workflow-level `concurrency` and
  `env`, resolves `permissions`, and creates every job in state `pending`. Jobs with no `needs`
  become ready at once.
- **W04-R02 Incremental expansion.** `RunPlan::on_leg_finished(leg, conclusion, outputs) ->
  Vec<PlanAction>` and `RunPlan::on_cancel()` advance the plan: jobs whose needs have all
  finished evaluate their `if` (contexts `github`, `needs`, `vars`, `inputs`, status functions over
  the needs, transitively), then expand their matrix (which may read `needs` outputs) and yield
  `Queue(leg)` actions, or finish as `skipped`. `PlanAction` is `Queue(LegSpec)`,
  `Settle { leg, conclusion, reason }` (a leg decided without running: skipped by `when`, cancelled
  before it was queued, failed to start), `Job { job, conclusion, reason }` (a job that ended with
  no legs), `Cancel { leg, reason, queued_only }`, `Warn { leg, message }` and
  `Finish(conclusion)`.
- **W04-R03 Skips**, GitHub-exact: when a job fails or is skipped, the jobs that need it are
  skipped unless their `if` calls a status function that lets them run (`always()`, `failure()`,
  `!cancelled()`, …).
- **W04-R04 Run conclusion**, GitHub-exact: `failure` if any leg failed without
  `continue-on-error`, `cancelled` if cancelled, `success` otherwise (skipped jobs don't fail a
  run).
- **W04-R05 Timeouts.** Job `timeout-minutes` (default 360 in GHA, 60 in kiln v1 files; at most
  `[ci] max_job_minutes`, default 7 days), step `timeout-minutes`, and a run limit
  (`[ci] max_run_days`, default 35, as GitHub).
- **W04-R06 Permissions.** Workflow `permissions` apply to jobs that don't set their own; values
  are GitHub's scopes (`actions`, `attestations`, `checks`, `contents`, `copilot-requests`,
  `deployments`, `id-token`, `issues`, `discussions`, `models`, `packages`, `pages`,
  `pull-requests`, `repository-projects`, `security-events`, `statuses`) with
  `read`/`write`/`none`, plus `read-all`, `write-all` and `{}`. (`copilot-requests` is accepted;
  the facade has no Copilot API, so it grants nothing there.)
  Unset means the repository's default (`ci.default_token_permissions`: `read` by default, which is
  `contents: read` and `metadata: read`). Untrusted runs are capped at read. The resolved set
  drives `GITHUB_TOKEN` (W14) and the job token (W10).

**Requirements: matrices, GitHub-exact**

- **W04-R07 Expansion algorithm.**
  1. Evaluate `strategy.matrix` (an object, or an expression giving one; each axis an array or an
     expression giving one). An axis with no values is an error, as on GitHub.
  2. Axes are the keys other than `include` and `exclude`, in written order. Take the Cartesian
     product, first axis varying slowest.
  3. Remove every combination that matches an `exclude` entry (all of the entry's keys equal;
     objects compared deeply).
  4. For each `include` entry in order: add its keys to every **original** combination whose
     original axis values it does not contradict, never overwriting an original value but
     overwriting values added by earlier includes; if it matched no combination, append it as a
     new combination.
  5. A matrix of only `include` yields one leg per entry.
  6. More than 256 legs is an error in the GHA dialect; the kiln dialect allows up to
     `[ci] max_legs` (default 1,024).
- **W04-R08 Legs.** Each leg has `strategy.job-index` and `job-total`. Its name is the job's name
  (or id) followed by its values in order in parentheses, unless the job's `name` already
  references `matrix`, GitHub-exact.
- **W04-R09 `fail-fast`** (default `true`): the first failed leg (not `continue-on-error`) cancels
  the job's queued and running legs. **`max-parallel`** limits the job's legs in progress.
- **W04-R10 Dynamic matrices.** A matrix that reads `needs` expands when those needs finish; until
  then the job shows "waiting to expand".

**Requirements: kiln matrices** (kiln dialect and overlays; G-03, G-18)

- **W04-R11 `where:`** — an expression over `matrix`, `github`, `inputs`, `vars` and `needs`
  evaluated per combination after `include`/`exclude`; false removes the combination before any
  leg exists. The plan preview lists removed combinations with "removed by where". This replaces
  most `exclude` lists:

  ```yaml
  strategy:
    matrix:
      os: [linux, macos, windows]
      profile: [dev, release, asan]
      where: matrix.profile != 'asan' || matrix.os == 'linux'
  ```

- **W04-R12 `when:`** — an expression evaluated when the leg becomes ready, with the job-level
  contexts plus `matrix`; false completes the leg as `skipped` ("skipped by when"). For
  conditions that depend on runtime facts such as `changed()`.
- **W04-R13 `platforms:`** — a first-class axis of OCI platforms (`linux/amd64`, `linux/arm64`,
  `linux/arm/v7`, `darwin/arm64`, `darwin/amd64`, `windows/amd64`, `windows/arm64`,
  `linux/riscv64`, `linux/ppc64le`, `linux/s390x`). Each leg gets `matrix.platform`, `matrix.os`,
  `matrix.arch` and the requirement `os == <os> && arch == <arch>` (W05), because the user
  declared the platform. `emulate: [linux/arm64, …]` (opt-in) also allows runners of another
  architecture with QEMU user emulation registered (binfmt_misc fact) for the listed platforms;
  emulated legs are marked in the UI. Combined with routing rules, one line builds for every
  platform:

  ```yaml
  strategy:
    matrix:
      platforms: [linux/amd64, linux/arm64, darwin/arm64, windows/amd64]
      profile: [release]
  runs-on: native            # a label routing rules map to "the leg's platform, natively"
  ```

- **W04-R14 `shard:`** — splits a job into balanced legs over work items:

  ```yaml
  strategy:
    shard:
      items: ${{ fromJSON(needs.plan.outputs.tests) }}   # or files: 'tests/**/test_*.py'
      count: auto            # or a number
      target: 8m             # with auto: as many legs as keep each under the target
      balance: timing        # timing (W16 history; unknown items take the median) or count
  ```

  Each leg gets `matrix.shard` (`index`, `total`) and `$KILN_SHARD_ITEMS` (a file with its items,
  one per line). Balancing is longest-processing-time-first over recorded durations; it is
  deterministic for the same history and items.
- **W04-R15 `chunk:`** — `{ items: <expr>, size: N }` splits a list into legs of N items each.
- **W04-R16 Finer parallelism and failure scope.** `max-parallel: { total: 8, per: { os: { macos:
  2 } } }`; `fail-fast: { scope: job | axis:<name> | run, cancel: queued | running | all }`.
- **W04-R17 Leg-level needs.** `needs: [{ job: build, matrix: same }]` waits only for the legs of
  `build` whose values on the axes both jobs share equal this leg's;
  `needs: [{ job: build, where: "matrix.platform == 'linux/amd64'" }]` waits for the selected
  legs. Plain `needs: [build]` keeps GitHub's meaning (every leg).

**Requirements: outputs and environment** (G-04)

- **W04-R18 Step and job outputs**, GitHub-exact: step outputs from `$GITHUB_OUTPUT` (and the
  deprecated `::set-output`, with a warning annotation), job outputs from `jobs.<id>.outputs`
  evaluated on the runner after the last step, at most 1 MiB per job and 50 MiB per run; an
  output whose value contains a masked secret is not set, with GitHub's warning. For a matrix
  job, `needs.<job>.outputs` holds, per name, the value from the leg that set it last
  (GitHub-exact; confirmed by W22's GitHub comparison suite).
- **W04-R19 Matrix-aware outputs** (kiln): `needs.<job>.legs` is a list of
  `{ name, matrix, result, outputs }` in leg order, so `needs.build.legs.*.outputs.digest` gives
  every leg's digest and a `where`-style selection picks one.
- **W04-R20 Typed outputs** (kiln): a step may write JSON lines `{"name": …, "value": <any JSON>}`
  to `$KILN_OUTPUT_JSON`. Kiln-dialect consumers see the JSON value; GHA consumers see its
  `toJSON`.
- **W04-R21 Exported environment** (kiln): `exports: { env: [NAME, …] }` on a job publishes those
  variables' final values (after `$GITHUB_ENV` updates) as `needs.<job>.env.NAME`; a consuming job
  imports them with `env-from: [build]`. `$KILN_RUN_ENV` (opt-in, `run-env: true` on the
  workflow) collects `name=value` lines into `run.env`, readable by jobs that start later and
  `need` the writer.
- **W04-R22 Secret outputs** (kiln): an output declared `{ value: …, secret: true }` keeps a
  value containing a secret, stored encrypted (W09) and delivered to consumers only as masked
  environment. GitHub's dropping behaviour stays for undeclared outputs.
- **W04-R23 Provenance.** Every output records the step and leg that produced it, and each
  evaluation that read it records the consumer. The run page shows the flow (W21).

**Requirements: reusable workflows**, GitHub-exact

- **W04-R24** `jobs.<id>.uses` names `./.github/workflows/<file>` or `$/.github/workflows/<file>`
  (both the same commit; `$/` reads it without a checkout, GitHub-exact) or
  `owner/repo/.github/workflows/<file>@ref` (fetched through W07's mirror, pinned to the SHA the
  ref resolved to when the run started). A called workflow's own `./` calls are its repository's,
  at that commit, not the caller's. Inputs are typed (`boolean`, `number`, `string`) with
  defaults and `required`; `secrets` are mapped explicitly or `inherit`ed; outputs map
  `jobs.<id>.outputs` of the called workflow. Called jobs appear as `<caller> / <called>`. The
  `github` context is the caller's; the OIDC `job_workflow_ref` is the called workflow's.
  Permissions can only be reduced by the callee. Nesting and count limits are GitHub's (10 levels
  and 50 called workflows per run at the time of writing; the GitHub comparison suite confirms).
- **W04-R25 kiln templates** (the v2 draft's `uses:` for jobs and steps in the kiln dialect) are
  reusable workflows with kiln keys: `uses: ./templates/rust-check.yaml` resolves inside `.kiln/`;
  `kiln://owner/repo/path@sha` names one in another kiln repository.

**Requirements: the rest**

- **W04-R26 Concurrency keys.** Workflow- and job-level `concurrency.group` are evaluated when the
  run is created and when each leg is queued, and scoped to the repository (W05-R07).
- **W04-R27 Environments.** `environment` (string or `{ name, url }`) is evaluated when the leg is
  ready and binds W15's protection rules, secrets and variables; `url` is evaluated after the steps.
- **W04-R28 Defaults.** `defaults.run.shell` and `working-directory` at workflow and job level.
- **W04-R29 Plan preview.** `POST /api/ci/plan-preview { repo, ref | sha, path | content, event }`
  returns the plan without running it: jobs, legs with values, conditions decided now (with their
  value) or later (marked), removed combinations with their reason, each leg's requirements and
  routing candidates (W05-R20 explain), and diagnostics. The builder UI and agents use it.

**Design.** `crates/kiln-ci/src/plan/`: `graph.rs` (jobs, needs, readiness, skips),
`matrix.rs` (W04-R07 to W04-R17), `outputs.rs`, `reusable.rs`, `perms.rs`, `preview.rs`. The
planner never touches I/O: reusable workflows and history (for `shard` timing) are supplied by the
caller through a `PlanSource` trait. `crates/kiln/src/ci/runs.rs` persists plans: one `runs` row,
one `jobs` row per leg (with `job_id`, `matrix`, `leg_index`), created as legs become known.

**Interfaces.** Takes the IR (W02), events (W03), expressions (W01), timing history (W16).
Gives `LegSpec`s to the scheduler (W05) and the runner assignment (W06), outputs and needs
contexts to every later evaluation, and plan previews to W20 and W21.

**Acceptance criteria**

- **W04-AC01 Matrix oracle.** Every matrix example in GitHub's documentation (include, exclude,
  include-only, expanding with include, objects in axes) yields exactly the documented legs in the
  documented order. *Unit test.*
- **W04-AC02 Property tests.** For random matrices (≤ 6 axes, ≤ 6 values, ≤ 8 include and 8
  exclude entries): no panic; no two legs equal; every original combination not excluded appears
  exactly once; legs ≤ the limit or a limit error. *Property test.*
- **W04-AC03 Graph semantics.** Skips, `always()`, `failure()` over transitive needs,
  `continue-on-error`, `fail-fast`, `max-parallel`, cancel, run conclusions: each from a table of
  cases written from GitHub's documentation. *Unit test.*
- **W04-AC04 Reusable workflows.** Inputs of each type with defaults, `secrets: inherit` and
  explicit, outputs, nesting at the limit and one past it (error), permissions reduced and an
  attempt to raise them (error). *Unit test, then E2E.*
- **W04-AC05 kiln matrices.** `where`, `when`, `platforms` (with requirements), `emulate`,
  `shard` (deterministic balance from a recorded history; ±10 % of the target), `chunk`,
  per-axis `max-parallel`, scoped `fail-fast`, leg-level `needs` (`same` and `where`).
  *Unit test.*
- **W04-AC06 Outputs.** Limits (1 MiB per job, 50 MiB per run), secret-containing outputs dropped
  with the warning, matrix last-writer, `legs`, typed outputs, `exports`/`env-from`, `run.env`,
  secret outputs delivered masked. *Unit test, then E2E.*
- **W04-AC07 Preview.** For fixtures F-10 to F-15, the preview's legs, decided conditions and
  removed combinations equal the legs an actual run creates. *Integration test.*
- **W04-AC08 Cross-arch end to end.** F-10 builds a Rust binary for `linux/amd64`, `linux/arm64`
  and `darwin/arm64` on the live cluster's runners (and `windows/amd64` where a Windows runner is
  joined), each leg on a native runner, and a final job collects the three artifacts. *E2E.*

**Scenarios**

- **W04-S01** GitHub's documented example `os: [macos-latest, windows-latest], version: [12, 14,
  16], include: [{os: windows-latest, version: 17}, …]` → the documented legs.
- **W04-S02** `needs.plan.outputs.shards = '["core","cli","ui"]'`, `matrix: { shard:
  ${{ fromJSON(needs.plan.outputs.shards) }} }` → three legs created only after `plan` succeeds.
- **W04-S03** `platforms: [linux/amd64, linux/arm64]` with `needs: [{ job: build, matrix: same }]`
  on `test` → `test (linux/arm64)` starts when `build (linux/arm64)` finishes, without waiting for
  amd64.
- **W04-S04** `shard: { files: 'tests/**/*_test.py', target: 5m, balance: timing }` with history
  totalling 40 minutes → 8 legs, each between 4.5 and 5.5 minutes by history.
- **W04-S05** `exports: { env: [IMAGE_TAG] }` on `build`, `env-from: [build]` on `deploy`; `build`
  appends `IMAGE_TAG=abc` to `$GITHUB_ENV` → `deploy`'s steps see `IMAGE_TAG=abc`.

**Progress.** In progress.

- *Done.* `kiln_ci::plan`:
  - `matrix`: R07, R08, R13 to R15, and AC01's every documented example. An `include` entry that
    fits nothing is appended once.
  - `perms` (R06).
  - The engine (`RunPlan`): GitHub's graph and skips, status functions over transitive needs,
    `continue-on-error`, `fail-fast`, `max-parallel`, dynamic matrices, cancellation (`always()`
    and `cancelled()` jobs run), timeouts.
  - Reusable workflows as scopes: typed inputs, secrets mapped or inherited, outputs,
    permissions only reduced, 10 levels and 50 workflows.
  - kiln's `where`, `when`, `platforms`/`emulate`, `shard` (LPT over history), `chunk`, per-axis
    `max-parallel`, scoped `fail-fast`, leg-level needs (`same`, `where`).
  - Outputs: last writer, `legs`, typed, `exports`/`env-from`, `run.env`, secret outputs kept out
    of contexts, limits with warnings, and provenance (`reads`). On the runner and server too
    (R20 to R22): `$KILN_OUTPUT_JSON`, exports, `$KILN_RUN_ENV` (`run.env` and `KILN_RUN_<X>`),
    and secret outputs as `KILN_NEEDS_<JOB>_<NAME>`, checked end to end with a `.kiln/` workflow.
  - `preview` (R29's core).
  - Tests: 42 unit tests (AC03 to AC06, S01 to S05) and the AC02 property test.
  - `ci/runs.rs` persists runs and legs; `ci_plans` and `ci_plan_log` rebuild a plan by replay.
  - Reusable workflows reach the planner (`ci/reusable.rs`): the commit's own files, and remote
    ones pinned through W07's mirror, nested; checked end to end on a local server.
  - R29: `POST /api/ci/plan-preview` (`crates/kiln/src/ci/preview.rs`). It takes a workflow
    being edited or a commit's, for any event at a ref and commit. It returns:
    - diagnostics;
    - whether the event starts it, and why not, including action-policy refusals;
    - the plan the dispatcher makes with a real run's contexts, variables and reusable
      workflows, each leg with its routing against the runners connected now (W05-R20);
    - the decisions and their reasons;
    - the combinations `exclude` and `where` removed;
    - the jobs decided only once their needs finish.
- *Done.* Fixtures F-10 to F-15 (`crates/kiln/tests/fixtures/plan`, a README says which
  criteria each serves) and AC07 (`tests/plan_fixtures.rs`, `w04_ac07_previews_are_runs`):
  - Every fixture is pushed to one repository. For each, the preview's legs (key, job, name and
    matrix), its decisions (status and reason) and its removed combinations are its run's, and
    the jobs it leaves for later make the rest of the run's legs.
  - The server has three runners of this test build, each posing as a runner of one of F-10's
    platforms (`kiln agent --chaos platform=darwin/arm64`: that platform's `os` and `arch`
    facts, and the labels they give), so each of F-10's legs runs on the runner of its platform,
    whatever machine runs the test.
  - Every run succeeds, which checks the rest end to end on a local server: F-11's forty files
    in one leg with no history, then in eight of five once its first run reported a minute each
    (S04); F-12's legs as GitHub documents them (their order is AC01's unit tests'); F-13's
    legs once `plan` said which, and `docs` skipped from the start; F-14 three levels deep with
    typed inputs, a default, `secrets: inherit` and an output handed back up (AC04); F-15's
    typed, plain and secret outputs, `exports` and `env-from`, `run.env` and a matrix's `legs`
    (AC06).
  - Writing the fixtures found four gaps, now closed:
    - `shard`'s and `chunk`'s `files:` matched nothing in runs and previews. The commit's files
      are listed once when a run starts (for workflows that split files), matched with the
      filter pattern, and saved with its input for replays.
    - `$KILN_SHARD_ITEMS` (R14) wasn't set. A shard's or chunk's leg gets the file, an item a
      line.
    - Only the preview had the removed combinations. A run saves them as they grow
      (`ci_plans.removed`), and `GET /api/ci/runs/:id` shows them.
    - The timing table (W16-R06) was kept for ten minutes, past new reports. A report drops it.
- *Done.* R23 (`kiln_ci::wf::flow`, `GET /api/ci/runs/:id/flow`, `ci.runs.flow`): each job output,
  from the step whose output its value reads to every job that reads it and where (its `if`,
  matrix, `runs-on`, env, a step's `if`, `run` or `with`, its own outputs; read whole, as
  `toJSON(needs.build.outputs)`, counts for each), with the legs that read it and the value each
  of the job's legs gave it (a secret's never, a long one cut); and each job's exported
  environment, the names its legs set, and the jobs that take it with `env-from`. Where a value
  is read comes from the workflow itself, the values from what each leg reported. The run page
  shows it (W21-R04). Checked by `wf::flow`'s unit test and `tests/run_graph.rs`.
- *Done.* R28 as GitHub's runner has it (corrected 2026-10-24, corpus pass 1024b): `defaults.run`
  are the job's own `run:` steps'. An action runs in the workspace (JavaScript, Docker and kiln's
  alike: actions/runner's `NodeScriptActionHandler` and `ContainerActionHandler`), and a
  composite action's steps take no defaults (its `ScriptHandler` applies them outside an action
  only). kiln ran actions in `defaults.run.working-directory`, so react's setup-node, under
  `working-directory: compiler`, looked for `compiler/yarn.lock` in `compiler/` and failed
  ("Some specified paths were not resolved"). Checked by
  `w04_r28_defaults_are_the_jobs_run_steps_own`: a `run:` step in the default directory, a
  composite action's and a JavaScript action's in the workspace.
- *Left.* AC08 on the live cluster's runners: F-10 as it is, on runners of its three platforms.

### W05 Scheduler: queues, concurrency, priorities, routing, retries

**Scope.** Deciding which leg runs where and when: capacity, concurrency groups, priorities,
fairness between repositories, quotas, reservations, routing by requirements and declared
preferences, retries, cancellation and supersession, and saying why anything waits. Replaces
today's `Hub::schedule_once` (a one-second tick that reads up to 2,000 queued jobs from SQLite and
scans agents) with an event-driven state machine in `kiln_ci::sched`, driven by
`crates/kiln/src/ci/dispatch.rs`.

**Requirements: the core**

- **W05-R01 Event-driven.** The scheduler wakes on events (leg queued, attempt finished, runner
  connected, capacity changed, rules changed, approval granted, time reached), never on a
  polling tick, and never reads the database to decide. Its state is in memory and rebuilt from
  SQLite at start (W05-R25).
- **W05-R02 Inputs and outputs.** `Scheduler::handle(Input, now) -> Vec<Command>`. Inputs:
  `RunQueued{ group, supersede }` and `RunFinished` (workflow-level concurrency, W05-R06,
  W05-R08), `LegQueued`, `LegRunning` (a leg adopted after a restart), `LegCancelled`,
  `AttemptStarted`, `AttemptFinished{ outcome }` (success, failed with a class, cancelled),
  `Rejected` (the runner refused it: back to the queue, same attempt, another runner),
  `RunnerUp{ facts, labels, executors, caps, capacity }`, `RunnerChanged`, `RunnerLost`,
  `RunnerDraining`, `PoolChanged`, `RulesChanged`, `ApprovalGranted`, `Tick` (only for timers it
  asked for). Commands: `StartRun`, `CancelRun`, `Assign{ leg, runner, attempt, executor }`,
  `Cancel{ leg, attempt, reason }`, `Retry{ leg, attempt, at }`, `Wait{ leg, reason }`,
  `Settled{ leg, outcome, reason }` (done for good: the plan hears of it), `ScaleUp{ pool, count,
  signature }`, `ScaleDown{ pool, runner }`, `Timer{ at }`.
- **W05-R03 Capacity.** A runner offers `slots` (v1, still the default) or, when it reports
  resources and `[ci.scheduler] mode = "resources"`, CPU, memory, disk and GPUs. A leg requests
  `resources` (default from its pool or `[ci.scheduler] default_request`). In resource mode a leg
  fits when every requested resource fits within capacity × `overcommit` (default 1.0 for memory
  and disk, 1.5 for CPU). `exclusive: true` takes the whole runner.
- **W05-R04 Data structures.** Requirements are interned into **signatures**; each signature keeps
  its eligible runner set (recomputed when a runner or a rule changes) and a priority queue of its
  legs ordered by (effective priority descending, fair-share virtual time ascending, queue order
  ascending). Assigning on new capacity looks only at the signatures that runner is eligible for;
  queuing a leg looks only at that signature's eligible runners. Both are O(log n).
- **W05-R05 Durability.** The dispatcher records an assignment in SQLite (batched transactions,
  flushed at most every 5 ms) before it sends `Assign`, so a crash never runs a leg twice
  unknowingly; runners re-announce running legs on reconnect (today's `Hello.running`), which
  re-adopts them.

**Requirements: concurrency, priorities, fairness**

- **W05-R06 Concurrency groups**, GitHub-exact, at workflow and job level, scoped to the
  repository: at most one member in progress. With `queue: single` (the default) at most one
  member is pending, and a new member makes the previous pending member `cancelled`. With
  `queue: max`, up to 100 are pending and start in order; a member that arrives when 100 are
  pending is cancelled. `cancel-in-progress: true` also cancels the member in progress. It can't be
  combined with `queue: max`: written literally, that is a validation error (`KCI1015`); as an
  expression that evaluates to true, the run fails to start with the same message. The cancelled
  run says "cancelled: superseded in group <name>".
- **W05-R07 Group extensions** (kiln): `limit: N` (N members in progress at once instead of one),
  and `[ci] max_pending` to raise `queue: max`'s bound (default 100, as GitHub). Groups may be
  shared across repositories when declared in server configuration (`[[ci.groups]] name, repos,
  limit`), for shared resources such as one staging cluster. (An earlier draft gave kiln its own
  `queue: fifo | latest | cancel-in-progress`. GitHub's `queue: single | max` now covers them,
  so kiln uses GitHub's.)
- **W05-R08 Superseded runs** (opt-in, G-22): `supersede: { when: [branch, stack, pull_request],
  action: cancel | deprioritize, keep: [main] }` on a workflow cancels or lowers older active runs
  of the same workflow when a new push arrives for the same branch, a new version of a diff in the
  same stack, or the same pull request.
- **W05-R09 Priorities.** Default by event: `workflow_dispatch`/manual 100, `land`/`merge_group`
  90, `diff`/`pull_request` 80, `push` 60, `agent` 60, `workflow_run` 50, `schedule` 20; a job's
  `priority: high | normal | low | <0–100>` overrides. Strict between classes, with **aging**:
  effective priority rises by `[ci.scheduler] aging_per_minute` (default 1) while waiting, so
  nothing starves.
- **W05-R10 Fair share.** Within a priority, start-time fair queuing across repositories (or
  organizations, `[ci.scheduler] fair_by`), weighted by `[ci.fairness] weights`: each leg's virtual
  start is `max(V, finish[repo])`, and `finish[repo] += expected_cost / weight`, with
  `expected_cost` the leg's median runner-seconds from history (W19) or a default. Under
  saturation, two equally weighted repositories each get within 10 % of half the capacity
  (W05-AC04).
- **W05-R11 Quotas** (opt-in): per repository or organization, maximum legs in progress and
  runner-minutes per day, per pool.
- **W05-R12 Reservations** (opt-in): a pool or runner reserved for matching legs during windows
  (`reserve: { for: "workflow == 'bench'", cron: "0 2 * * *", duration: 2h }`); others are not
  assigned there in the window, and legs already running finish.
- **W05-R13 Wait reasons.** Every waiting leg has exactly one current reason, from: `needs`,
  `capacity` (with counts: eligible runners, busy, draining), `no-runner` (none eligible is
  connected; lists the ones offline), `unroutable` (no runner known to kiln could ever run it; lists
  the failing requirements), `concurrency` (group and holder), `quota`, `reservation`,
  `exclusive`, `approval`, `scaling` (a pool is starting runners; how many and since when),
  `max-parallel`, `capability` (eligible runners' builds lack a capability; which),
  `other-runner` (only the runner its retry avoids could take it: R16's `runner: different`, or a
  runner that refused it; which). It is stored (`jobs.wait_reason`), sent as an event, and shown
  on the leg.

**Requirements: retries** (G-16)

- **W05-R14 Retry policy** at leg and step level (kiln dialect and overlays):

  ```yaml
  retry:
    max: 3
    on: [runner-lost, infra, oom, disk-full, image-pull, timeout, failure,
         { exit-code: [137, 143] }, { log: "connection reset by peer" }]
    backoff: { initial: 10s, factor: 2, max: 5m }
    runner: any | same | different       # default: different after runner-lost, otherwise any
  ```

  Step retries rerun only that step (for `run:` steps, and for `uses:` steps that declare
  `retry`). Leg retries make a new attempt.
- **W05-R15 Failure classes.** The runner reports a class with every failed attempt:
  `runner-lost` (the connection lapsed past the reconnect grace), `infra` (the executor failed to
  start, the pod could not be scheduled, a kiln service failed), `image-pull`, `oom` (cgroup
  `oom_kill` count rose, or exit 137 with memory at its limit), `disk-full` (ENOSPC seen, or the
  work volume full), `timeout`, `failure` (anything else).
- **W05-R16 Defaults.** Without a policy: `runner-lost` is retried once on a different runner
  when one can take it, else on the same one (today's requeue, kept, and shown as "retried:
  runner X was lost"); nothing else is retried. A policy's `runner: different` waits for another
  runner, and says so (`other-runner`).
- **W05-R17 Reruns**, GitHub-exact: re-run all jobs, re-run failed jobs (and the jobs that need
  them), re-run one job (and its dependents), optionally with debug logging. A rerun is a new run
  attempt (`github.run_attempt`); automatic leg retries are attempts of the leg
  (`KILN_ATTEMPT`) and leave `github.run_attempt` unchanged.

**Requirements: cancellation**

- **W05-R18 Cancel**, GitHub-exact on the runner: the running step's process group gets SIGINT,
  then SIGTERM after 7.5 s, then SIGKILL after 2.5 s more; then the leg's remaining steps whose
  `if` holds under cancellation (`always()`, `cancelled()`) and its post steps run, bounded by
  `[ci] cancel_cleanup_minutes` (default 5).

**Requirements: routing** (G-17, G-18)

- **W05-R19 Requirements.** A leg's requirement is the conjunction of:
  1. its `runs-on` labels (GitHub-exact: a runner must carry every label, or be named by it;
     `runs-on: { group, labels }` names a pool and labels);
  2. constraints from routing rules that match the leg (W05-R21);
  3. constraints the workflow declares (`requires:`, and `platforms` legs, W04-R13);
  4. capabilities the leg needs to be run correctly at all (`CAP_CI2` for v2 legs, `CAP_ACTIONS`
     for `uses:`, `CAP_DOCKER` for Docker actions, job containers and services on non-k8s
     executors): these are always applied, because a runner without them cannot run the leg;
  5. with `routing: { infer: true }` (opt-in, workflow or repository), executor requirements
     inferred from the job's content, for example `container:` → an executor that runs
     containers.
- **W05-R20 Explain.** `GET /api/ci/legs/:id/routing` returns, for every runner and pool:
  eligible or not, each requirement with the runner's fact value and pass/fail, the preference
  score with its terms, and which one was chosen and why. The leg page shows it ("Why here?",
  "Why waiting?").
- **W05-R21 Routing rules** are declared, ordered data, at server, organization and repository
  scope (`ci_routing_rules`; server config `[[ci.routing]]`; trunk's `.kiln/routing.yaml` for a
  repository). Each rule:

  ```yaml
  - match: { labels: [ubuntu-latest] }          # or job: "<expression>", repos: [globs]
    require: ["os == linux", "arch == x86_64"]
    executor: [docker, firecracker, shards, host]  # allowed executors, in order of preference
    image: kiln/runner-ubuntu-24.04               # image for container and microVM executors (W13)
    prefer: [{ fact: disk.free, higher: true, weight: 2 }]
  - match: { labels: [native] }                   # used with platforms legs: the leg's own platform
    require: ["os == ${{ matrix.os }}", "arch == ${{ matrix.arch }}"]
  ```

  The default rule set maps GitHub's hosted labels (`ubuntu-latest`, `ubuntu-24.04`,
  `ubuntu-22.04`, `ubuntu-24.04-arm`, `ubuntu-22.04-arm`, `macos-latest`, `macos-15`, `macos-14`,
  `windows-latest`, `windows-2025`, `windows-2022`, `windows-11-arm`) to OS and architecture
  requirements and kiln's runner images. It ships **disabled**: an administrator enables it (one
  switch in Settings → Routing, or `[ci.routing] github_labels = true`). Until then such labels
  route only to runners that carry them, GitHub-exact for self-hosted runners.
- **W05-R22 Constraints.** `fact op value` with `==`, `!=`, `<`, `<=`, `>`, `>=`, `in [..]`,
  `has <item>`, `matches <glob>`, units on sizes (`50Gi`) and durations, `label <name>` and
  `!label <name>`, combined with `&&`, `||`, `!` and parentheses. Facts are W13's (`os`, `arch`,
  `os.version`, `cpu.cores`, `cpu.model`, `cpu.flags`, `mem.total`, `mem.free`, `disk.free`,
  `disk.total`, `gpu.count`, `gpu.model`, `gpu.memory`, `virt.kvm`, `virt.hvf`,
  `executors`, `tools.<name>.version`, `binfmt` platforms, custom facts). Unknown facts are false,
  never errors at run time; the editor warns.
- **W05-R23 Preferences** (opt-in: only when a rule or the job declares them): weighted terms over
  facts (`higher`/`lower`), labels, pools, `affinity: { job: <id> }` (the runner that ran that
  job's leg last, for warm caches), `spread: host`. Score = Σ weight × term (each term
  normalized to 0..1 across the eligible set). Ties break by least loaded, then most free
  capacity, then name. **Without preferences** the choice is today's: least loaded, then most free
  slots.
- **W05-R24 Unroutable legs.** When no runner kiln has ever seen, and no pool, could satisfy a
  leg, it waits with reason `unroutable` and the failing requirements, or fails after
  `[ci.routing] unroutable_timeout` if one is set.

**Requirements: recovery and speed**

- **W05-R25 Restart.** At start the dispatcher loads queued and waiting legs and live groups from
  SQLite and rebuilds the state; with 10,000 queued legs this takes < 2 s.
- **W05-R26 Speed.** The targets of §1.5: ready → `Assign` p99 < 50 ms with capacity, ≥ 2,000
  assignments per second, 10,000 queued legs, 1,000 runners.
- **W05-R27 Pools.** The scheduler asks W13's pool manager to scale up when the legs waiting for a
  signature exceed the idle capacity of the pools that can serve it, and to scale down runners idle
  past their pool's `idle_timeout`. Today's Kubernetes pod per job becomes the pool kind `k8s`
  (ephemeral, one leg per pod), keeping its quota handling and launch backoff.

**Design.** `crates/kiln-ci/src/sched/`: `state.rs` (runners, signatures, queues), `fair.rs`,
`groups.rs`, `routing.rs` (constraint parser, rule matching, eligibility, scoring, explain),
`retry.rs`, `sim.rs` (a simulator: runners, legs, durations, failures, all driven by a seeded
clock, used by tests and `kiln ci bench`). `crates/kiln/src/ci/dispatch.rs` owns the loop: it
turns hub and store events into `Input`s, persists, and carries out `Command`s (sending
`Assign`/`AssignV2`, timers, pool calls). `Hub::schedule_once` and `pick_agent` are removed when
the dispatcher is on `main`; the v1 `Assign` path is kept for runners without `CAP_CI2`.

**Interfaces.** Takes legs from W04 and facts and pools from W13. Gives assignments to the
runners (W06), wait reasons and routing explanations to the API and UI (W21), events to W19 and
W20.

**Acceptance criteria**

- **W05-AC01 Groups.** Simulation tests for every GitHub concurrency behaviour (one in progress and
  one pending, replacement of the pending member, `cancel-in-progress`), job-level groups inside
  matrices, `queue: max` (order, the bound of 100, and its refusal with `cancel-in-progress`), and
  the kiln `limit` and cross-repository groups. *Unit test.*
- **W05-AC02 Supersede.** Pushes 1, 2, 3 to a branch with `supersede: { when: [branch], action:
  cancel }`: runs 1 and 2 cancelled with the reason, 3 runs; without it all three run. *Unit test.*
- **W05-AC03 Priorities and aging.** A low-priority leg under a constant stream of high-priority
  legs starts within the bound aging gives. *Simulation test.*
- **W05-AC04 Fairness.** Two repositories, equal weights, each queueing 1,000 one-minute legs on
  20 slots: each gets 50 % ± 10 % of slot-time over any 10-minute window. Weight 3:1 gives 75 % ±
  10 %. *Simulation test.*
- **W05-AC05 Routing.** Tables of runners (facts) × legs (requirements, rules, preferences):
  eligible sets, scores, ties and explanations exactly as written in the test. The default GitHub
  label rules are off until enabled. *Unit test.*
- **W05-AC06 Retries.** Each failure class with and without a policy; backoff timing; `runner:
  different`; step retries; attempts recorded; `github.run_attempt` unchanged by leg retries.
  *Unit test, then E2E with a fault-injecting runner (W22).*
- **W05-AC07 Cancel timing.** A step that traps SIGINT and SIGTERM sees them at 0 s and 7.5 s
  (± 0.5 s) and is killed at 10 s; an `always()` step and a post step run afterwards. *Integration
  test on the host executor.*
- **W05-AC08 Speed.** `kiln ci bench --scheduler`: 1,000 simulated runners, 10,000 queued legs
  over 50 signatures, a stream of completions: ≥ 2,000 assignments per second, p99 ready → assign
  < 50 ms, measured in the simulator with real data structures. *Bench, asserted.*
- **W05-AC09 Restart.** 10,000 queued legs persisted, server restarted: state rebuilt in < 2 s,
  nothing assigned twice, running legs re-adopted. *Integration test.*
- **W05-AC10 Wait reasons.** Every reason of W05-R13 is produced by a scenario and shown on the leg
  page in Chromium and WebKit. *Integration test + UI check.*
- **W05-AC11 Live concurrency.** On the live cluster, a run fanning out 500 legs uses every free
  slot of the eligible runners and pod quota; dispatch never waits on the scheduler (the leg-wait
  histogram shows capacity waits only). *E2E.*

**Scenarios**

- **W05-S01** Runners A (linux/x86_64, 500 GiB free) and B (linux/x86_64, 20 GiB free); a leg with
  `prefer: [{ fact: disk.free, higher: true }]` → A, explained; the same leg without the
  preference → the less loaded one.
- **W05-S02** A leg `runs-on: [self-hosted, linux, arm64]` with only x86_64 runners → waits
  `unroutable: arch == arm64`; joining an arm64 runner with those labels assigns it within 50 ms.
- **W05-S03** `concurrency: deploy-prod` with runs R1 (in progress), R2 (pending), then R3 →
  R2 cancelled "superseded in group deploy-prod", R3 pending.
- **W05-S04** A leg killed by the OOM killer with `retry: { max: 2, on: [oom] }` and
  `resources.memory: 4Gi` → a second attempt; the attempt list shows "oom".

**Progress.** In progress.

- *Done.*
  - `kiln_ci::sched`:
    - `routing`: constraints (R22), GitHub-exact labels with self-hosted runners' implicit
      labels, rules (R21, GitHub's label rules off until enabled), explanations (R20),
      preferences (R23);
    - `groups` (R06, R07);
    - `retry` (R14 to R16);
    - `state`: the event-driven scheduler (R01 to R04, R09 to R11, R13, R19, R24) with
      signatures, aged priority as a time-invariant key, fair share, run and job groups,
      supersede, retries with backoff, `Rejected`, quotas, exclusive legs, slots or resources,
      and restart adoption;
    - `sim`, the simulator.
  - Tests: AC01 to AC06, AC10, S01 to S04, and AC08 at 127,000 assignments per second with a p99
    of 6 µs per input, against a target of 2,000.
  - `[ci.scheduler]` (`aging_per_minute`, `default_cost_seconds`) and `[ci.fairness]`
    (`weights`, `quotas`, by repository name) reach the scheduler. A leg's expected cost (R10)
    is its job's median duration over the last two weeks, from W19's rollups.
  - `crates/kiln/src/ci/dispatch.rs`, the dispatcher, behind `[ci] enabled`:
    - it uses the hub's slot bookkeeping, so v1 and v2 share runners;
    - v1 code reads only v1 rows;
    - legs need `CAP_CI2`;
    - `/api/ci/legs/:id/routing` and the plan preview's per-leg routing (R20).
  - R17, reruns as attempts of the same run (`github.run_attempt` one more): every job, the
    failed and cancelled ones and the jobs that need them, or one job and its dependents
    (`kiln_ci::plan::rerun`). The other legs keep their results as copies (`reused_from`, with
    their steps and logs), and the plan takes their reports as they were.
    - Debug reruns turn on step and runner debug.
    - Legs and the plan log are kept per attempt.
    - Who started each attempt after the first is kept (`ci_run_attempts`), and is its
      `github.triggering_actor`: a person by their name, an agent by the person it acts for,
      GitHub's Re-run by the login that pressed it (`w05_r17_triggering_actor_is_who_reran_it`).
    - Checked end to end on a local server: `kiln rerun --failed` kept `a`'s output for `b`, and
      `--job a --debug` reran the chain with debug lines.
  - Replay (R25) goes on from the run's last log entry across attempts. It hands the plan the
    result of a leg that ended before its report reached the log, so a server stopped in that
    window no longer leaves the run waiting forever. Restored legs are the replayed plan's
    (`RunPlan::spec_of`): environment, permissions, concurrency group, exported environment and
    a called workflow's job all come back. They used to be rebuilt from a few fields, which let
    a leg held for review run unapproved after a restart. A cancel in the log reaches the
    rebuilt scheduler too: a leg it cancelled that hasn't ended is cancelled again, so a leg
    whose runner lost it across the restart ends cancelled. The rebuild used to give the
    cancel to the plan alone. Such a leg went back in the queue as lost, ran again with its
    run still cancelling, and the run never ended (the corpus lab's tokio run, which held its
    one runner for hours; `w05_r25_a_cancel_holds_over_a_restart`).
  - R21's rules by scope (`crates/kiln/src/ci/routing.rs`), in the order they apply:
    - the server's configuration (`[[ci.routing]]`);
    - the database (`ci_routing_rules`: `GET/PUT /api/ci/routing/rules?scope=server|org:<owner>`,
      `routing.manage`);
    - each repository's `.kiln/routing.yaml` on its default branch, read again when one of its
      runs starts;
    - the defaults (native, and GitHub's labels when on).

    An organization's rules reach only its repositories and a repository's only itself; every
    rule a leg matches adds its constraints, and the first that names executors or an image
    decides them, so the server's choices win. `POST /api/ci/routing/explain` (with
    `kiln routing where <labels> [--repo]`) shows where a leg so routed would go and which
    rules applied; `kiln routing rules [--scope] [--set FILE | --clear]`.
- *Done.* R15's classes on the runner, said with each failed attempt (`JobEventV2::FailureClass`),
  from the step that failed the leg:
  - `oom`: the step ended by SIGKILL (exit 137, or no code), and the OOM killer's count rose
    where it ran (the job container's cgroup, or this runner's and the machine's);
  - `disk-full`: it said "No space left on device", or the work volume has under 64 MiB left;
  - `image-pull` or `infra`: the leg's containers couldn't start, for an image that wouldn't
    pull, or anything else.

  R14's `{ log: "..." }` entries now match: the server reads the failed attempt's log for the
  policy's patterns. Checked end to end (`crates/kiln/tests/retries.rs`): a log line, a full
  disk and the OOM killer (a 64 MB job container) are each retried by the policy naming them,
  and a plain failure under `on: [oom]` isn't.
- *Done.* R12, reservations: `[[ci.reservations]]` keeps the runners a constraint names (or a
  pool's `reserve`, its executors) for the legs `for` matches (a condition over `job`,
  `workflow`, `repo` and `event`, as a routing rule's `match.job`). Windows start at `cron`
  (GitHub's syntax; `timezone` reads it in local time, with DST handled) and last `duration`. In a
  window nothing else is assigned there: a leg that could only go there waits with reason
  `reservation` (which one, and until when). Legs already running finish. The scheduler looks at
  the windows when one opens or closes (a timer), not on every decision (`w05_r12_*`).
- *Done.* R13's `scaling` reason for a pool's legs: how many executors it's starting, and since
  when. A pool at its `max` makes the leg wait for `capacity`, its executors counted.
- *Done.* R27 (with W13-R09): the scheduler keeps each pool's executors (starting, idle, busy) and
  asks for more when the legs waiting for a pool outnumber its idle and starting executors (or
  `min_idle`), within `max` and `per_host_max`, the host with the fewest of the pool's first. It
  stops executors idle past `idle_timeout`, down to `min_idle`. A host that fails to start one is
  left alone, 2 s at first, doubling to 5 minutes. A pool's leg runs only in an idle executor of
  its pool; it waits for `scaling` while executors start for it. On the simulator, a
  burst of 200 legs scales a pool to its `max` of 32 and back to `min_idle` within
  `idle_timeout` (`w13_ac04_pool_burst_on_the_simulator`).
- *Done.* AC07, GitHub's cancel sequence timed on the host executor (in W06-AC05's
  `w06_ac05_post_steps_cancellation_and_timeouts`): a step trapping both signals gets SIGINT as the
  run is cancelled, SIGTERM 7.5 s on, and is killed at 10 s, each within half a second; then the
  `always()` step and the post step run.
  - In a job container too (`w05_ac07_cancel_sequence_in_a_job_container`), which is where kiln
    runs `ubuntu-latest` (W13-R04): `docker exec` passes no signal on, so a cancel reached only the
    CLI on the host, and the step ran on, its traps never told, until its container stopped (F-22's
    cancellation probe found it). What a step starts there carries a mark in its environment
    (`KILN_STEP`), and each signal goes to every process that has it, then the CLI goes. Each is a
    `docker exec` of its own, so a little later than on the host. An image without `sh` keeps the
    old way. Not a shell around the step's program: dash drops the variables whose names it can't
    take, GitHub's `INPUT_PERSIST-CREDENTIALS` among them, which `actions/checkout` then reads as
    false (`w07_r09_checkout_v7_persists_its_credentials` caught it).
- *Done.* AC09 (`tests/restart.rs`, `w05_ac09_a_restart_rebuilds_the_queue`): a run of two slow
  legs on a runner of its own and 1,000 legs queued for runners there aren't; the server killed and
  started again on its ports and data while the slow legs' steps run.
  - The dispatcher rebuilt all 1,002 legs within 2 s, saying how many and how long as it started.
  - It took the slow legs back from the runner as the runner reconnected; each finished once, on
    its first attempt.
  - The queued legs waited on, none assigned.
  - A checkout that meets the server away (restarting) tries its fetch again, waiting 1 s and
    doubling to 15 s, for two minutes, as `actions/checkout` retries its own.
  - `kiln ci bench` measures the rebuild at 10,000 queued legs (`ci.bench.restart.rebuild_ms`,
    §1.5's ≤ 2 s; W22-AC08).
- *Done.* R05 across a restart, whichever comes first, a runner's hello or the dispatcher's
  rebuild of its runs:
  - A leg the runner says it still runs before the dispatcher knows its runs is kept for it, and
    re-adopted once it has rebuilt them; the leg's end, if it comes first, waits for it too. Such
    a leg used to be cancelled as a job nobody knew.
  - A leg the runner doesn't say it has was lost with it (`runner-lost`, R16's retry) and frees its
    slot. The rebuild used to put such a leg back on the runner, where it stayed running for good
    and held the slot: a server's own runner, back before the rebuild with none of its legs, hit it
    on every restart that found its legs running.
  - A runner that doesn't come back within its reconnect grace (45 s) loses its legs the same way.
  - Checked: `tests/chaos.rs` (`w05_r05_hellos_before_the_rebuild`, the rebuild made 10 s late by
    the chaos fault `rebuild=`) and `tests/restart.rs`
    (`w05_r05_a_restart_loses_what_its_runner_lost`,
    `w05_r05_a_runner_gone_over_a_restart_loses_its_legs`).
- *Done.* R03's resource mode end to end: `[ci.scheduler] mode = "resources"`, with `overcommit = {
  cpu, memory, disk }` (1.5, 1 and 1 by default) and `default_request = { cpu, memory, disk, gpu
  }`.
  - A runner offers what its facts say it has (W13-R01): its CPUs' threads, its memory, its work
    volume's free disk and its GPUs. Until its facts are in, it offers its slots. Its memory is
    no more than its cgroups let it use (2026-10-25): the least `memory.max` of its own cgroup
    and those above it (v1's `memory.limit_in_bytes`), so a runner in a container, a pod or a
    systemd scope offers that and not the machine's (the corpus lab's, in a 40G scope on a
    124 GB machine, would have taken seven 16 GiB legs at once). Checked by
    `w05_r03_a_runners_memory_is_the_least_limit_above_it` and
    `w05_r03_a_runner_offers_its_cgroups_memory`.
  - A leg takes what its job's `resources:` asks, else the default. CPUs are written as
    Kubernetes writes them (`2`, `0.5`, `500m` for half of one); memory and disk with their units.
    A pool's `resources.cpu` reads `500m` the same way now, where it was 500 minutes.
  - Checked end to end (`tests/capacity.rs`): two legs each asking more than half of the
    machine's CPUs times 1.5 ran one after the other on a runner with two slots, and a small one
    ran beside one of them. Unit-tested: requests as written (`w05_r03_requests_as_written`).
  - The executor holds a leg to the memory it was given (the assignment's `executor.options`): a
    container executor's job container has it as its limit (W13-R04, 2026-10-24).
  - And to its CPUs (2026-10-25): a leg runs on CPUs of its own, as many as it asked for (whole
    ones: half of one is one), a cpuset and not a quota, which stalls a test's many threads
    where a hosted runner's machine wouldn't. The runner keeps a pool of the CPUs it may run on
    itself (its `Cpus_allowed_list`: its affinity within its cgroup's cpuset); each leg takes
    those no other leg holds while there are any, then the least held, and gives them back when
    it ends. A job container and its services get them as `--cpuset-cpus` (a hosted runner's
    services share its machine); a pool's executor, started before its leg came (W13-R09), gets
    them and the leg's memory by `docker update` as the leg takes it; and a host step starts
    within them through `taskset` (Linux; elsewhere a leg sees the machine's). An engine that
    can't hold a container to CPUs (a rootless one without the cpuset controller delegated to
    it) drops them with a warning, as Docker and Podman do, and the leg sees the machine's. So
    `nproc` says the leg's, and what sizes its work by it (`make -j$(nproc)`, cargo's jobs, a test
    runner's workers) does as on a hosted runner's machine of that size, its memory going with it:
    ruff's formatter check and TypeScript's API tests had started a worker per CPU of the lab's 32
    and failed (corpus pass 1024b); at 4 CPUs a leg both pass (pass 1025c, two legs at once on CPUs
    0-3 and 4-7, `nproc` 4). Checked by `w05_r03_legs_take_cpus_of_their_own`,
    `w05_r03_a_host_leg_runs_on_its_cpus` and, with Docker, `w05_r03_a_containers_cpus_are_its_legs`
    and `w05_r03_a_pools_container_is_held_to_its_legs`. Not in a pod: a one-leg runner there
    (`kiln agent --ephemeral`, a job's or a `k8s` pool's) has the CPUs its cluster gives it, its
    node's, which the node's other pods share, so the dispatcher leaves the leg's CPUs out (each
    pod would take the same first ones); a microVM leg has its microVM's vCPUs. *Left*: the
    containers a leg's steps start through its Docker socket see the machine's CPUs, where a
    hosted runner's share its machine's few; two runners on one machine give out the same CPUs,
    each as if alone.
  - With Docker inside its container (W13-R04's `docker = "socket"`), a leg keeps the memory and
    CPUs it was given: the dispatcher had put Docker's option in their place, so such a leg ran
    free of both (corpus pass 1025b, 2026-10-25; `w05_r03_docker_inside_keeps_the_legs_resources`).
- *Left.* AC11 on the live cluster.
- *Done* (2026-10-23), R16's retry where only the runner that lost the leg could take it, which
  waited for good. Of the options put to the user (wait; prefer another runner and take the same
  one when none other can, at once or after a few minutes; don't retry; or the default prefers
  another while an explicit `runner: different` stays strict), the last, recommended, is taken,
  and the user chose it (2026-10-23):
  - The default retry goes to another runner when one could take it (connected, eligible and not
    draining, busy or not), else to the one that lost it, at once. A leg that stayed off it goes
    there first when the others go or drain.
  - `runner: different` waits for another, and its wait reason says which runner it avoids
    (`other-runner`), as a leg a runner refused does.
  - Unit-tested (`w05_r16_a_lost_runners_retry`); a server's own runner, the only one, takes its
    lost legs' retries across a restart (`tests/chaos.rs`, `tests/restart.rs`).

### W06 Step runtime on the runner

**Scope.** Running a leg's steps exactly as GitHub's runner does: workspace layout, environment
variables, contexts, step evaluation, shells, workflow commands, environment files, masking,
annotations, groups, summaries, job outputs, post steps, cancellation and debug logging. The runtime
is executor-agnostic: it asks the executor (W13) to start processes inside the leg's environment.
It replaces `exec.rs` for `AssignV2`.

**Requirements**

- **W06-R01 Layout.** On the host executor: `RUNNER_WORKSPACE = <work>/w/<8 hex>`,
  `GITHUB_WORKSPACE = $RUNNER_WORKSPACE/<repository name>`, `RUNNER_TEMP = $RUNNER_WORKSPACE/_temp`
  (emptied per leg), `RUNNER_TOOL_CACHE = AGENT_TOOLSDIRECTORY = <work>/_tool` (kept between legs),
  actions under `<work>/_actions/<owner>/<repo>/<sha>`, node runtimes under `<work>/_externals`. The
  8-hex prefix keeps paths short for Unix sockets (today's reason). Inside job containers and
  Docker actions the paths are GitHub's: `/__w` (work), `/__e` (externals), `/__t` (tool cache),
  `/github/home`, `/github/workflow`, `/github/file_commands`, `/github/workspace` for Docker
  actions, with the same environment variables pointing at them.
- **W06-R02 Environment variables**, GitHub-exact names and values: `CI=true`,
  `GITHUB_ACTIONS=true`, `GITHUB_ACTION`, `GITHUB_ACTION_PATH`, `GITHUB_ACTION_REPOSITORY`,
  `GITHUB_ACTION_REF`, `GITHUB_ACTOR`, `GITHUB_ACTOR_ID`, `GITHUB_TRIGGERING_ACTOR`,
  `GITHUB_API_URL`, `GITHUB_GRAPHQL_URL`, `GITHUB_SERVER_URL`, `GITHUB_BASE_REF`, `GITHUB_HEAD_REF`,
  `GITHUB_REF`, `GITHUB_REF_NAME`, `GITHUB_REF_PROTECTED`, `GITHUB_REF_TYPE`, `GITHUB_REPOSITORY`,
  `GITHUB_REPOSITORY_ID`, `GITHUB_REPOSITORY_OWNER`, `GITHUB_REPOSITORY_OWNER_ID`,
  `GITHUB_RETENTION_DAYS`, `GITHUB_RUN_ID`, `GITHUB_RUN_NUMBER`, `GITHUB_RUN_ATTEMPT`, `GITHUB_JOB`,
  `GITHUB_SHA`, `GITHUB_WORKFLOW`, `GITHUB_WORKFLOW_REF`, `GITHUB_WORKFLOW_SHA`,
  `GITHUB_EVENT_NAME`, `GITHUB_EVENT_PATH`, `GITHUB_WORKSPACE`, `GITHUB_ENV`, `GITHUB_OUTPUT`,
  `GITHUB_PATH`, `GITHUB_STATE`, `GITHUB_STEP_SUMMARY`, `RUNNER_ARCH` (`X64`, `ARM64`, `ARM`),
  `RUNNER_OS` (`Linux`, `macOS`, `Windows`), `RUNNER_NAME`, `RUNNER_TEMP`, `RUNNER_TOOL_CACHE`,
  `RUNNER_DEBUG` (when debugging), `RUNNER_ENVIRONMENT=self-hosted`, `ACTIONS_RUNTIME_URL`,
  `ACTIONS_RUNTIME_TOKEN`, `ACTIONS_RESULTS_URL`, `ACTIONS_CACHE_URL`,
  `ACTIONS_CACHE_SERVICE_V2=True`, `ACTIONS_ID_TOKEN_REQUEST_URL` and
  `ACTIONS_ID_TOKEN_REQUEST_TOKEN` (only with `id-token: write`), `ImageOS` and `ImageVersion` when
  the runner image declares them. Plus kiln's: every `KILN_*` variable v1 sets, `KILN_ATTEMPT`,
  `KILN_OUTPUT` (= `GITHUB_OUTPUT`), `KILN_OUTPUT_JSON`, `KILN_RUN_ENV`, `KILN_METRICS`,
  `KILN_SHARD_ITEMS`, `KILN_EXECUTOR`, `KILN_TOKEN` (the job token, W10).
- **W06-R03 Contexts on the runner**: `github`, `env`, `vars`, `job` (`status`, `container`,
  `services`, `check_run_id`, and `workflow_ref`, `workflow_sha`, `workflow_repository` and
  `workflow_file_path`: the workflow file that defines the job, a called workflow's for its jobs),
  `steps`, `runner` (`name`, `os`, `arch`, `temp`, `tool_cache`, `debug`, `environment`, and
  kiln's `facts`), `secrets`, `strategy`, `matrix`, `needs`, `inputs`.
  - *Done* (2026-10-24, found in corpus pass 1024b): `job`'s four `workflow_*`. A called workflow
    checks itself out by them (astral-sh/github-actions' security review: `repository: ${{
    job.workflow_repository }}`, `ref: ${{ job.workflow_sha }}`), and without them its checkout
    took the caller's repository (ruff's `security-review`). The plan gives each leg the workflow
    that defines its job (`LegSpec::defined_in`): the run's own (`github.workflow_ref` as it
    reads), a `./` or `$/` one at the run's ref and commit, a remote one's repository, its ref as
    GitHub writes it (`refs/tags/v1`, `refs/heads/main`, or a commit) and the commit pinned at the
    run's start. The assignment carries it (`job_workflow`), and the OIDC token's
    `job_workflow_ref` and `job_workflow_sha` read it (W10) where they read the caller's before.
    Checked by `w06_r03_a_legs_job_knows_its_workflow` (the plan) and
    `w06_r03_the_job_context_names_its_workflow` (the runner). *Left*: `job.check_run_id`.
- **W06-R04 Step evaluation**, GitHub-exact order: `if` (with status functions over earlier steps)
  → display `name` → `env` (step over job over workflow) → `with` → `working-directory` → `shell`
  → `timeout-minutes` → `continue-on-error` → run → process the environment files → record
  `outcome` and `conclusion`.
- **W06-R05 Shells**, GitHub-exact:

  | `shell` | Command (`{0}` is the script file) | Default when |
  | --- | --- | --- |
  | `bash` | `bash --noprofile --norc -eo pipefail {0}` | Linux and macOS with bash (unset `shell`: `bash -e {0}` per GitHub's rule for the implicit default) |
  | `sh` | `sh -e {0}` | Linux and macOS without bash |
  | `pwsh` | `pwsh -command ". '{0}'"` | Windows |
  | `powershell` | `powershell -command ". '{0}'"` | |
  | `cmd` | `%ComSpec% /D /E:ON /V:OFF /S /C "CALL "{0}""` | |
  | `python` | `python {0}` | |
  | custom | the given template, which must contain `{0}` | |

  Scripts are written to `$RUNNER_TEMP/<uuid>.<ext>` with the extension each shell expects
  (`.sh`, `.ps1`, `.cmd`, `.py`), with CRLF for `cmd`.
- **W06-R06 Workflow commands**, GitHub-exact, parsed from stdout line by line:
  `::add-mask::v`, `::group::title` / `::endgroup::`, `::debug::m` (shown only with step debug),
  `::notice`, `::warning`, `::error` with `file`, `line`, `endLine`, `col`, `endColumn`, `title`,
  `::stop-commands::<token>` until `::<token>::`, `::echo::on|off`, `::add-matcher::<file>` and
  `::remove-matcher owner=<o>::` (problem matchers turning matching output lines into annotations),
  the deprecated `::set-output name=` and `::save-state name=` (honoured, with a deprecation
  warning), and `::set-env`/`::add-path` (refused unless
  `ACTIONS_ALLOW_UNSECURE_COMMANDS=true`). Data escapes `%25`, `%0D`, `%0A`; property escapes add
  `%3A` and `%2C`.
- **W06-R07 Environment files**, GitHub-exact syntax and errors: `$GITHUB_ENV` and `$GITHUB_OUTPUT`
  take `NAME=value` and `NAME<<DELIMITER` … `DELIMITER` (the delimiter must not occur inside the
  value; an unterminated block fails the step with GitHub's message); `$GITHUB_PATH` takes one
  directory per line, prepended for later steps in order; `$GITHUB_STATE` feeds `STATE_<name>` to
  the same action's post step; `$GITHUB_STEP_SUMMARY` takes GitHub-flavoured Markdown, at most
  1 MiB per step. Each step gets fresh files.
- **W06-R08 Masking.** Secrets the leg receives, values from `::add-mask::`, and every line of a
  multi-line secret are replaced by `***` in logs, annotations, summaries, step names and outputs
  before anything leaves the runner. Masking works across chunk boundaries (a streaming matcher
  holds back what could be the start of a mask still arriving, and lets the rest go at once, so a
  step's live log keeps up with whole lines). kiln also masks each secret's
  base64, percent-encoded and JSON-escaped forms (secrets of 8 or more characters). Values shorter
  than 4 characters are not masked (that would destroy logs); the secret's settings page warns.
- **W06-R09 Logs.** Raw bytes per step as today (ANSI kept), plus `JobEvent`s for groups and
  annotations with log offsets, and a line-time index (offset → milliseconds) flushed with each
  chunk so the UI shows timestamps and group durations. Limits: 64 MiB per step and 1 GiB per leg
  by default; past them output is dropped with a notice.
- **W06-R10 Job outputs** are evaluated after the last main step, before post steps, then sent as
  `job_outputs`.
- **W06-R11 Pre and post steps.** An action's `pre` runs before the main steps, in step order,
  under `pre-if` (default `always()`); `post` runs after all main steps in reverse order of
  registration, under `post-if` (default `always()`), only for actions whose main step ran (or
  whose pre ran). Job containers and services are stopped after the post steps.
- **W06-R12 Cancellation and timeouts.** W05-R18's signals; a step past its `timeout-minutes`
  fails with "The action has timed out."; a leg past its timeout is cancelled, its conclusion is
  `timed_out`, and dependants see `needs.<job>.result == 'failure'` (GitHub's needs results are
  only `success`, `failure`, `cancelled`, `skipped`).
- **W06-R13 Debug.** Secrets or variables `ACTIONS_STEP_DEBUG` and `ACTIONS_RUNNER_DEBUG` set to
  `true`, or "Re-run with debug logging", enable `::debug::` output and `RUNNER_DEBUG=1`, as on
  GitHub.
- **W06-R14 kiln dialect checkout.** `checkout: auto` (the kiln default) fetches the commit into
  `GITHUB_WORKSPACE` the way v1 does (local git mirror cache, else the server's source tarball);
  `checkout: false` skips it. GHA workflows never get an implicit checkout.
- **W06-R15 Summaries** render as sanitized GitHub-flavoured Markdown (no scripts, no remote
  images unless `[ci] summaries_allow_images = true`), up to 20 per leg in the UI with the rest
  linked, as GitHub shows.
- **W06-R16 Annotations** are stored up to 1,000 per leg (GitHub shows 10 warnings and 10 errors per
  step and 50 per job; kiln shows those first and the rest on demand).
- **W06-R17 Background steps**, GitHub-exact.
  - `background: true` on a `run` or `uses` step that has an `id` starts the step, and the job goes
    on to the next one.
  - At most 10 background steps run at once in a leg. Any more wait their turn.
  - `wait: id | [ids]` pauses until those steps finish; `wait-all:` pauses until every running
    background step does. Their outputs and environment changes (`GITHUB_ENV`, `GITHUB_PATH`)
    apply only after that, in the order the steps were declared.
  - A background step that failed fails the `wait` or `wait-all` that includes it, unless the step
    has `continue-on-error: true`.
  - `cancel: id` stops one background step: SIGTERM, then SIGKILL after a grace period. Use
    GitHub's value, confirmed by W22-AC03; until then, 10 s. The step concludes `cancelled`.
  - `wait`, `wait-all` and `cancel` always run; they take no `if`.
  - `parallel: [steps]` runs its steps as background steps, with an implicit `wait` for all of
    them after the group.
  - An implicit `wait-all` runs before the post steps.
  - A composite action's steps can't be background steps, though the action itself can be one.
  - Each background step keeps its own log; only the live view interleaves them. The leg's
    timeline shows the overlap.

**Design.** (As built so far: the pure parsers are `kiln_ci::runner::{commands, envfiles, mask}`,
tested at kiln-ci's speed; the process side is `crates/kiln/src/runner/runtime.rs`.) Planned:
`crates/kiln/src/runner/runtime/`: `env.rs`, `contexts.rs`, `step.rs` (evaluation and
the step state machine), `shell.rs`, `commands.rs` (the line parser and dispatcher),
`envfiles.rs`, `mask.rs` (Aho–Corasick over a ring buffer), `matchers.rs` (problem matchers),
`summary.rs`, `post.rs`. The parsers are pure functions with exhaustive tests; process control
goes through `trait Exec { fn spawn(&self, Spawn) -> Result<Child>; fn path_map(&self) -> &PathMap; }`
implemented by each executor (W13).

**Interfaces.** Takes `AssignmentV2` (§2.5), W01 for step-level evaluation, W07 for `uses:`, W13
executors, W08/W10 runtime URLs and tokens. Gives `JobEvent`s and logs to the server.

**Acceptance criteria**

- **W06-AC01 Toolkit conformance.** A JavaScript test action (fixture F-20) built on the current
  `@actions/core`, `@actions/exec` and `@actions/io` calls every command and file API (`setOutput`,
  `exportVariable`, `addPath`, `setSecret`, `saveState`/`getState`, `group`, `notice`, `warning`,
  `error` with properties, `summary.addTable(...).write()`, `debug`, `getInput` with every
  `required`/default/trim case, `getIDToken` under W10). Each effect is observed by a later step or
  by the server and matches GitHub's documented behaviour. *Integration test on the host executor,
  repeated per executor in W13.*
- **W06-AC02 Commands and files.** Table tests for each command (escapes, properties,
  `stop-commands`, `echo`), each file (heredoc, CRLF, an unterminated delimiter's error,
  `GITHUB_PATH` order, state into post), problem matchers (setup-node's `tsc` and `eslint`
  matcher files as fixtures). *Unit test.*
- **W06-AC03 Masking.** A property test splits outputs containing secrets at every byte position
  across chunks: no secret byte sequence ever leaves the runner; encoded variants masked; short
  values not masked and flagged. *Property test.*
- **W06-AC04 Shells.** Each shell on each OS that has a runner in CI (Linux, macOS, Windows), with
  failing commands mid-pipeline (`pipefail`), exit codes, `working-directory`, and a custom
  template. *Integration test.*
- **W06-AC05 Post steps and cancellation.** Ordering of pre, main and post across three actions;
  `post-if`; cancellation mid-step runs the `always()` step and the posts; timeouts give the
  documented conclusions and `needs` results. *Integration test.*
- **W06-AC06 Paths in containers.** In a job container, `GITHUB_WORKSPACE`, `RUNNER_TEMP`,
  `RUNNER_TOOL_CACHE` and the file-command paths are GitHub's in-container paths and work; in a
  Docker action, `/github/workspace` and `/github/file_commands`. *Integration test with Docker.*
- **W06-AC07 Corpus jobs.** The jobs of F-03 that need no secrets or external services run green
  unmodified on the host and Docker executors. *E2E (W22-AC01).*
- **W06-AC08 Background steps.** Covers:
  - a server started in the background and read by a later step, with its output collected by
    `wait`;
  - a failing background step failing its `wait`, and not failing it under `continue-on-error`;
  - `cancel` of a step that ignores SIGTERM, which is killed after the grace period;
  - 12 background steps, of which 10 run at once;
  - `parallel` with its outputs used after the group;
  - the implicit `wait-all` before the post steps.

  *Integration test on the host executor.*

**Scenarios**

- **W06-S01** `run: echo "x=1" >> "$GITHUB_OUTPUT"` with `id: a`, then `run: test "${{
  steps.a.outputs.x }}" = 1` → both succeed.
- **W06-S02** A step prints `::error file=src/a.rs,line=3,col=5::bad` → an error annotation on
  `src/a.rs:3:5`, shown on the leg and in the diff view of the commit (W21).
- **W06-S03** A secret `hunter2-long-secret` printed in base64 → `***` in the log.
- **W06-S04** `continue-on-error: true` on a failing step → `steps.s.outcome == 'failure'`,
  `steps.s.conclusion == 'success'`, leg succeeds.
- **W06-S05** `{ id: db, run: ./start-db, background: true }`, then `{ run: ./integration-tests }`,
  then `{ cancel: db }` → the tests run against the database, the leg succeeds, and `db` concludes
  `cancelled`.

**Progress.** In progress.

- *Done.*
  - `kiln_ci::runner`, the pure parsers:
    - `commands`: R06, both syntaxes, AC02;
    - `envfiles`: R07, with the runner's messages;
    - `mask`: R08, streaming and chunk-safe, with every two-cut split tested (AC03).
  - `crates/kiln/src/runner/runtime.rs` runs `AssignV2` on the host executor:
    - R01's layout and R02's variables;
    - R03's contexts;
    - R04's evaluation order;
    - R05's shells;
    - commands and environment files between steps;
    - masking before output leaves;
    - step outcome and conclusion, `continue-on-error` and timeouts;
    - GitHub's cancel sequence (W05-R18);
    - job outputs (R10, one holding a secret is dropped);
    - a step's background processes kept until the leg ends.
  - `uses:` steps (W07), with pre and post steps (R11).
  - Runners announce `CAP_CI2` and `CAP_ACTIONS`.
  - Problem matchers (R06), GitHub-exact, in `kiln_ci::runner::matchers`:
    - matchers last the job; newer ones come first and replace an owner's;
    - multi-line patterns, with `loop`;
    - .NET's regular expressions (lookaround included);
    - GitHub's validation messages.
    A matched line becomes an annotation in its place. Its file is kept when it is one of the
    repository's. setup-node's tsc matcher annotated a real type error end to end.
  - A command that fails fails its step, as on GitHub, with "Unable to process command '…'
    successfully.": an unreadable matcher file, a disabled `set-env`, a bad `echo` value.
  - Background steps (R17, AC08). Each runs on a copy of the leg's state, beside the steps that
    follow, at most 10 at once. `wait`, `wait-all`, `cancel` and `parallel` are steps of their
    own, and an implicit `wait-all` runs before the job's outputs. Their environment, path,
    masks, outputs and post steps reach the leg in declared order. `cancel` sends SIGTERM, then
    SIGKILL after 10 s. Checked end to end on a local server: S05's background server, read by a
    later step and cancelled.
  - R13: debug from a debug rerun or the `ACTIONS_*_DEBUG` secrets and variables.
  - R14: `checkout: auto` fetches the commit through the git facade with the leg's token
    (shallow, `.git` kept), else the facade's tarball, as a step of its own.
  - kiln's files: `$KILN_OUTPUT_JSON` and `$KILN_RUN_ENV` (W04-R20, R21).
  - AC01, toolkit conformance (F-20, `crates/kiln/tests/fixtures/toolkit-action`): a bundled
    JavaScript action on the current `@actions/core`, `@actions/exec` and `@actions/io`, with a
    post step, calls every API (inputs of each kind, outputs, `exportVariable`, `addPath`,
    `setSecret`, state, `debug`, groups, annotations with properties, summaries, `exec`,
    `getExecOutput`, `mkdirP`, `which`, `isDebug`, `getIDToken`). `runtime::tests::toolkit_conformance`
    checks each effect on the host executor: in the step's log, in a later step, in the post
    step and in the leg's events. `getIDToken` asks the leg's ID token endpoint for its
    audience with the request token, and the token is masked from the moment it arrives.
  - R01's workspaces (`crates/kiln/src/runner/workspaces.rs`): a leg's `<work>/w/<8 hex>` is
    removed when the leg ends; until then they were kept forever, 12 GB of them on one runner.
    A failed leg with `keep_on_failure_minutes` (W17-R02) keeps its own until that runs out.
    Each leg's start sweeps what's left behind: workspaces no running leg uses whose keep has
    run out, or that never had one, such as those of a runner stopped mid-leg. Files a Docker
    action wrote as root are removed by a container.
  - R01's tool cache, per running leg. GitHub's self-hosted runner runs one job at a time, so
    its cache is never written twice at once. A kiln runner runs several legs, and two
    `setup-python`s installing 3.10 into one cache broke each other (a starter workflow found
    it). Each running leg now holds a cache of its own: the base directory, then `-1`, `-2` and
    so on, each kept for the legs after it. A busy runner keeps up to one copy per slot.
  - In the `docker` executor's container, `github.workspace`, `github.event_path`,
    `runner.temp` and `runner.tool_cache` are the paths the leg sees, as `$GITHUB_WORKSPACE`
    was; `cmake -S ${{ github.workspace }}` found nothing before. A job's `container:` keeps the
    host's, as on GitHub.
- *Done.* AC05 (`crates/kiln/tests/post_steps.rs`, `w06_ac05_post_steps_cancellation_and_timeouts`),
  on a local server with actions of the test's own: three JavaScript actions' pre steps in step
  order, their main steps, the job's `always()` step after a failed one, then the posts in
  reverse, the one whose `post-if: success()` doesn't hold left out; a job past its
  `timeout-minutes` `timed_out` (it was kept as `failure`, the result its dependants see, and is
  now its own) with "has exceeded the maximum execution time", and its dependant seeing
  `needs.late.result == 'failure'`; a run cancelled mid-step: that step `cancelled`, the
  `always()` step after it run to its end (it was cut short by the cancel it followed, and now
  runs within the post steps' five minutes, W05-R18) and the next not, the action's post step
  run, the leg and the run `cancelled`.
- *Done.* AC06 (`w06_ac06_paths_in_containers`): in an `alpine:3.20` job container,
  `GITHUB_WORKSPACE` is `/__w/<repo>/<repo>`, `RUNNER_TEMP` `/__w/_temp`, `RUNNER_TOOL_CACHE`
  `/__t`, and `GITHUB_OUTPUT`, `GITHUB_ENV`, `GITHUB_PATH` and `GITHUB_STEP_SUMMARY` are in
  `/__w/_temp/_runner_file_commands`, each working (an output, a variable and a path the next
  step has); a Docker action's are `/github/workspace` and `/github/file_commands`, its output
  and what it wrote in the workspace reaching the steps after it.
- *Done.* AC04 (`w06_ac04_each_shell_here`, on whatever machine runs it): each shell the machine
  has, as GitHub runs it. `bash` fails a step on a command failing mid-pipeline (`pipefail`) and
  `sh` doesn't; PowerShell (`$ErrorActionPreference = 'stop'`) and `cmd` on Windows; `python`; a
  custom template (`perl {0}`). Each failing step says its exit code as GitHub does, and
  `working-directory` is where the step runs. Run on Linux (x86-64) and macOS (arm64).
- *Done.* AC04's run on Windows (x86-64, Windows PowerShell 5.1, MSYS2's bash and perl) found
  `shell: cmd` broken there: std's quoting escaped the quotes inside GitHub's `/S /C "CALL "{0}""`,
  which cmd doesn't take escaped, so every `cmd` step failed. Its line goes as written now
  (`raw_arg`). PowerShell's exit codes are `-command`'s, as on GitHub: a failure is 1. Run again
  with the fix, AC04 passed on Windows too: every shell there, `cmd` among them.
- *Left.* AC07 (with W22-AC01).

### W07 Actions

**Scope.** `uses:`: resolving, mirroring, pinning and running actions of every kind, GitHub-exact,
plus kiln's own actions.

**Requirements**

- **W07-R01 Forms.** `./path` (in the workspace, after checkout, GitHub-exact), `$/path` (this
  repository at the running commit, read from the server's mirror so no checkout is needed;
  GitHub-exact), `owner/repo@ref`,
  `owner/repo/path@ref`, `docker://image[:tag|@digest]`, and in the kiln dialect
  `kiln://owner/repo/path@ref` (a kiln-hosted repository) and `kiln/<name>@v1` (built in,
  W07-R10).
- **W07-R02 Resolution at run start.** Every remote action a run may use (including inside
  composite actions and reusable workflows, recursively) is resolved once, when the run starts:
  ref → commit SHA through `git ls-remote` against the action's source (default
  `https://github.com/<owner>/<repo>`; `[[ci.actions.sources]]` maps owners to other hosts or to
  kiln's native repositories). The pins are stored with the run (`run_actions`), shown on the run
  page, and used by every leg and every rerun of that attempt. A rerun re-resolves only when asked
  ("rerun with fresh actions").
- **W07-R03 Mirror.** The server fetches the action repository into a bare mirror (no API quota),
  makes a tarball of the pinned commit, stores it in the blob store (W08) and records it in
  `ci_actions`. Runners download `GET /_kiln/actions/<owner>/<repo>/<sha>.tar.gz` with their job
  token and cache it under `<work>/_actions`. When the source is unreachable, a known pin is served
  from the mirror; an unknown one fails with "action source unreachable and not mirrored".
- **W07-R04 Policy** (opt-in, server and repository scope): `allow` and `deny` lists of
  `owner/repo` globs, `require_sha_pins` (refuse tags and branches: `KCI4021`), and
  `[ci.actions] prefetch = [...]` for air-gapped installs (`kiln actions sync`). Every resolution
  is audited (who, which ref, which SHA).
- **W07-R05 JavaScript actions.** `using: node20` and `node24` run on those versions; `node12` and
  `node16` run on node20, as GitHub forces. kiln provides Node in `<work>/_externals/node<NN>`
  (official builds from nodejs.org, verified against `SHASUMS256.txt`, mirrored in the blob store;
  musl builds for Alpine-based job containers, as GitHub's `node20_alpine`). Inputs become
  `INPUT_<NAME>` with the name upper-cased and spaces replaced by `_` (hyphens kept, GitHub-exact);
  defaults from `action.yml` are evaluated as expressions (`${{ github.token }}`).
  `GITHUB_ACTION_PATH`, `GITHUB_ACTION_REPOSITORY`, `GITHUB_ACTION_REF` are set. `pre`, `post`,
  `pre-if` and `post-if` per W06-R11.
- **W07-R06 Docker actions.** `image: Dockerfile` is built on the runner (`docker build`, tagged by
  the action's SHA and cached) or `image: docker://…` pulled; run with GitHub's mounts and
  environment (`INPUT_*`, `GITHUB_*`, the job's `env`), working directory `/github/workspace`, the
  job's network, `entrypoint`, `pre-entrypoint`, `post-entrypoint` and evaluated `args`. They need
  `CAP_DOCKER`; a leg that uses one is routed to runners with it (W05-R19). On Firecracker
  microVMs they run when the guest image has a container runtime. On shards microVMs they do not
  run: shards is not Docker, and a Docker action's contract (bind mounts, a Docker network, the
  Docker socket) is Docker's. A leg using one on `executor: shards` is a plan-time error
  (`KCI4030`) naming the action.
- **W07-R07 Composite actions.** Steps run in the caller's leg, with the `inputs` context, their
  own `steps` context, `github.action_path`, `shell` required on `run:` steps, `if`, `env`,
  `working-directory`, `continue-on-error` per step, `uses:` nesting up to GitHub's limit, and
  `outputs` mapping `value: ${{ steps.x.outputs.y }}`.
- **W07-R08 Outputs** of an action are its main step's `$GITHUB_OUTPUT`.
- **W07-R09 GitHub API–calling actions** work against the facade (W14): `actions/checkout`,
  `actions/github-script`, release and comment actions, `actions/download-artifact` across runs.
  Setup actions (`actions/setup-node`, `setup-python`, `setup-go`, `setup-java`, …) download from
  their vendors as on a self-hosted runner, and use the tool cache.
- **W07-R10 Built-in kiln actions** (kiln dialect, also usable from GHA workflows since `kiln/*`
  resolves locally first): `kiln/checkout@v1` (fast checkout from the server's mirror, sparse and
  partial), `kiln/cache@v1` (W08-R08 tiers), `kiln/auth@v1` (W11), `kiln/deploy@v1` (W15),
  `kiln/test-results@v1` (W16), `kiln/attest@v1` (W18), `kiln/notify@v1` (W19),
  `kiln/upload@v1`/`kiln/download@v1` (W08 artifacts with kiln's extras). They are implemented by
  the runner, need no Node, and have `action.yml` metadata served by `GET /api/ci/actions/kiln`.
- **W07-R11 kiln action metadata.** A kiln action (`.kiln/actions/<name>/action.yml`,
  `kiln://…`, or `kiln/*`) may declare `secrets:` (W09-R07), `requires:` (W05-R22 constraints its
  steps need) and `permissions:` (the job permissions it needs; the plan fails with `KCI4031` if
  the job grants less).
- **W07-R12 Catalog.** `GET /api/ci/actions?q=` lists actions seen in mirrors and built-ins with
  versions, SHAs and their `action.yml` inputs and outputs, for the editor's completion (W21).

**Interfaces.** Takes W02's metadata parser, W06's runtime, W08's blob store, W14's facade.
Gives step execution for `uses:`.

**Acceptance criteria**

- **W07-AC01 Top actions.** The actions list of F-21 (the 50 most-used actions, among them
  `actions/checkout`, `setup-node`, `setup-python`, `setup-go`, `setup-java`, `cache`,
  `upload-artifact`, `download-artifact`, `github-script`, `docker/setup-buildx-action`,
  `docker/login-action`, `docker/build-push-action`, `docker/metadata-action`,
  `dtolnay/rust-toolchain`, `Swatinem/rust-cache`, `astral-sh/setup-uv`, `pnpm/action-setup`,
  `oven-sh/setup-bun`, `golangci/golangci-lint-action`, `erlef/setup-beam`, `ruby/setup-ruby`,
  `shivammathur/setup-php`, `gradle/actions/setup-gradle`, `aws-actions/configure-aws-credentials`,
  `google-github-actions/auth`, `azure/login`, `hashicorp/setup-terraform`,
  `softprops/action-gh-release`, `peter-evans/create-pull-request`, `codecov/codecov-action`) each
  runs green in a fixture workflow exercising its main inputs. Actions that need a third-party
  account run against that integration's test tenant where W11 has one, and are otherwise
  recorded as "needs account" with a passing dry configuration. *E2E (W22-AC02).*
- **W07-AC02 Pinning.** A run resolves `actions/checkout@v4` once; moving the tag mid-run does not
  change what later legs and reruns get; "rerun with fresh actions" picks the new SHA. *Integration
  test with a local action source.*
- **W07-AC03 Mirror offline.** With the action source blocked, pinned actions still run;
  unpinned unknown ones fail with the message. *Integration test.*
- **W07-AC04 Kinds.** A JS action with pre and post, a Docker action from a Dockerfile and one
  from `docker://`, a three-level composite with outputs, and a local `./` action each behave
  GitHub-exact (inputs, outputs, state, ordering). *Integration test.*
- **W07-AC05 Policy.** `require_sha_pins` refuses a tag with `KCI4021` before the run starts;
  `deny` refuses with the rule named. *Unit test.*
- **W07-AC06 kiln metadata.** A kiln action declaring `secrets: { DEPLOY_KEY: { required: true } }`
  fails the plan with `KCI4040` when the pipeline lacks the secret and runs when it has it (with
  W09). *Integration test.*

**Progress.** In progress.

- *Done.*
  - `crates/kiln/src/ci/actions.rs`, on the server:
    - R02: every remote `uses:` a run can reach is pinned when the run starts, including those of
      composite actions (up to 10 deep). The pins are saved with the run's plan input, so a run
      replayed after a restart keeps them.
    - R03: a bare mirror per action repository (`<data>/actions/<owner>__<repo>.git`). The
      mirror is treeless (`--filter=tree:0`): it holds every commit, and git fetches a commit's
      trees and blobs the first time it's archived. So an action with a large history
      (`github/codeql-action`) mirrors well within the fetch's timeout; a full clone didn't. A
      ref resolves as a tag, then a branch, then a SHA. A known pin still resolves when the
      source is unreachable. Each pinned commit is served as a tarball (`git archive`) at
      `/_kiln/actions/<owner>/<repo>/<sha>.tar.gz`, for a leg's token. An action that was
      never mirrored and can't be fetched fails with "action source unreachable and not
      mirrored". That fails only the jobs that use it, as GitHub fails them at "Set up job": each
      one fails before it reaches a runner, with "Unable to resolve action `…`" on the leg, and
      the run's other jobs go on. A composite's own unresolvable action fails the jobs that use
      the composite. Policy refusals (R04) still fail the whole run as it starts.
    - R02's `[[ci.actions.sources]]`: some owners' actions come from somewhere other than
      GitHub, the first entry that names the owner deciding. That is either another git host
      or one of kiln's own native repositories (W14-R01). The mirror follows a source that
      moves.

      ```toml
      [[ci.actions.sources]]
      owners = ["acme", "acme-*"]
      url = "https://git.example.com/{owner}/{repo}.git"

      [[ci.actions.sources]]
      owners = ["platform"]
      native = true
      ```
  - `crates/kiln/src/runner/runtime.rs`, on the runner:
    - R01: `./path` and `owner/repo[/path]@ref`.
    - R05:
      - Node: `node24` runs on Node 24; `node20`, `node16` and `node12` run on Node 20. Builds
        come from nodejs.org, are checked against `SHASUMS256.txt`, and are kept in
        `<work>/_externals`.
      - Inputs: `INPUT_*`, with defaults evaluated, and GitHub's "Unexpected input(s)" warning.
      - `GITHUB_ACTION_PATH`, `GITHUB_ACTION_REPOSITORY` and `GITHUB_ACTION_REF`.
      - `pre`, `post`, `pre-if`, `post-if` and `STATE_*` (W06-R11). What a pre step saves,
        its main and post steps get, as on GitHub.
    - R07: composite actions, with their own `steps` context, inputs, outputs and nesting.
    - R08: outputs.
    - R06: Docker actions (Dockerfile and `docker://`), on hosts with Docker (W13).
      - A leg using one, or a composite action with one beneath it, is routed to runners with
        `CAP_DOCKER`, as a `docker://` step's is: the server reads each action's metadata as the
        run starts.
      - `runs.args` and `runs.env` are evaluated with the action's own `inputs`, as hadolint's
        `HADOLINT_OUTPUT: ${{ inputs.output-file }}` needs.
      - `pre-entrypoint` runs with the pre steps, before the job's steps, and
        `post-entrypoint` with the post steps, each in a new container of the action's image.
  - `actions/checkout@v4` runs unmodified end to end against the git facade (W14).
  - R01's `$/path`: resolved to the run's own commit and served from the repository's mirror,
    to that repository's legs only. No checkout is needed.
  - R02 through reusable workflows: their calls are followed when the run starts, and the
    actions of the workflows they call are pinned with the rest.
  - An owner's `.github` repository, where `crazy-max/.github` and `docker/.github` keep the
    actions and workflows their repositories share, is a repository like any other. A name may
    start with a dot, but is never dots alone. A workflow there that calls another by `./` calls
    its own repository's, at that commit (W04-R24), not the caller's
    (`w07_r02_an_owners_shared_repository`). The corpus's docker/buildx found both. Its
    annotations step failed with "was not resolved when the run started", and its `binaries`
    call (`docker/github-builder`'s `bake.yml`) with "no such workflow".
  - R02 through the repository's own composite actions (`./path`): each is read from the
    repository at the running commit, where a checkout of it puts it, and its remote actions are
    pinned with the rest (`w07_ac04_kinds`'s local action uses one nothing else does). The
    corpus found them missed: such a job failed with "was not resolved when the run started".
  - R04 (`kiln_ci::policy`, AC05's unit tests):
    - `[ci.actions] allow`, `deny`, `require_sha_pins` (`KCI4021`) and `prefetch`;
    - repositories' own narrower policies (`/api/ci/actions/policy?repo=`, `kiln actions
      policy`);
    - checked on every call and action before the run starts; a refusal is the run's startup
      failure, and the plan preview lists refusals too;
    - each run's resolutions audited: who, which ref, which commit;
    - `kiln actions sync` mirrors `prefetch`.
  - R12: `GET /api/ci/actions?q=` (`kiln actions list`): the mirrored actions, their newest
    versions and their `action.yml`.
- *R10* (`crates/kiln/src/runner/builtins.rs`): `kiln/<name>@v1` runs on the runner itself, in
  either dialect. The server never pins or fetches `kiln/*`, an unknown one fails the run at
  start, and the action policy doesn't apply.
  - `kiln/checkout@v1`: from the server's mirror, with `path`, `ref`, `fetch-depth` (0 for all),
    `sparse-checkout` (cone or not) and `filter` (a partial clone). Its outputs are `ref` and
    `commit`. The kiln dialect's implicit checkout is the same code.
  - `kiln/test-results@v1`: W16's reports as a step of its own, failing on failed tests
    (quarantined ones excepted under `enforce`), with `failed` and `passed` as outputs.
  - `kiln/notify@v1`: a message through the repository's channels (W19-R06), failing only when
    no channel took it.
  - `kiln/auth@v1`: an integration's variables from its step on (W11-R03), with `kind` and `env`
    outputs.
  - Inputs are checked as JavaScript actions' are (required ones, GitHub's "Unexpected
    input(s)" warning). Their metadata is at `GET /api/ci/actions/kiln`, and they lead
    `GET /api/ci/actions?q=` (R12).
  - Verified end to end from a GitHub workflow: a sparse checkout held only what it asked for,
    and `kiln/test-results@v1` failed on a failed test with its counts as outputs.
- *Done.* R11, kiln action metadata:
  - **Recorded.** At run start each action's `secrets:`, `requires:` and `permissions:` are read
    and kept with its pin.
  - **`secrets:`** (W09-R07): checked, delivered, and alone visible inside the action.
  - **`requires:`** joins the leg's routing constraints (W05-R22).
  - **`permissions:`** above what the job grants fail the leg with `KCI4031`, which names the
    scope and the grant to add.
  - Verified end to end: an action needing `deployments: write`, used by a job that grants none.
- *Done.* AC02 (`w07_ac02_pins`): a run's first job holds while the test moves the tag of the
  action it ran. Its second job still gets the pinned commit, and so does a rerun. "Rerun with
  fresh actions" (`fresh_actions` on `POST /api/ci/runs/:id/rerun`, `kiln rerun --fresh-actions`)
  pins it again and gets the commit the tag names now.
- *Done.* AC03 (`w07_ac03_mirror_offline`): a run with two jobs, one using a mirrored action,
  one an action its source never had. The first job passes and the second fails with the
  message, the run conclusion being `failure`, not a startup failure. With the source gone, the
  next run's pin comes from the mirror, and the first job passes again.
- *Done.* AC05: `kiln_ci::policy`'s unit tests (`w07_ac05_*`).
- *Done.* AC06 (`w07_ac06_declared_secrets`): an action declaring a required `DEPLOY_KEY`
  fails its run as it starts with `KCI4040`, naming the job, the scopes searched and the command
  that adds it. Once the secret is set, the next run passes, the action using it masked.
- *Done.* AC04 (`crates/kiln/tests/actions.rs`), on a server with its own runner, with its
  actions from a source of the test's own (`[[ci.actions.sources]]`):
  - a JavaScript action with `pre` and `post`;
  - a Docker action from a Dockerfile, with `pre-entrypoint` and `post-entrypoint`;
  - one from `docker://`;
  - a composite three levels deep, each level calling the next by its remote name;
  - a local `./` action.

  Each gets its inputs (defaults evaluated, `with:`, `runs.args` and `runs.env`) and gives its
  outputs, and the Docker action's file is in the workspace. An image made for GitHub's runner
  user (`USER 1001`, as kubesec's action is) runs as this runner's user, the workspace's owner,
  so it can write there too. Each pre step's state reaches its
  main and post steps, and each main step's reaches its post step. They run in GitHub's order:
  the pre steps first, in step order (`Pre js kind`, `Pre docker kind`); then the job's steps;
  then the post steps, in reverse.
- *Done.* R10's `kiln/upload@v1` and `kiln/download@v1` (`w07_r10_kiln_upload_and_download`),
  run by the runner itself, no Node: a zip each, through the results service and its blob URLs as
  `actions/upload-artifact@v4` keeps them, so either reads the other's. The files' root is the
  deepest directory every path is under; hidden files stay out unless asked for; big ones go in
  8 MiB blocks; the server checks each upload's SHA-256 as it's finalized, and the download checks
  it again as it arrives; entries that would land outside the destination are refused.
- *Done.* R01's `kiln://owner/repo[/path]@ref` (`w07_r01_kiln_actions`): an action in one of
  kiln's own repositories, pinned at run start from kiln's own git, mirrored and served apart
  from GitHub's of the same name (`/_kiln/actions/kiln/…`), run as GitHub's are. It's its owner's
  repositories' to use, as a private action on GitHub is its owner's: another owner's workflow
  fails at the start, saying so, and a leg of another owner's can't download it.
- *Done.* Tarballs in W08's store: each action's tarball (GitHub's, `kiln://` and `$/`), made
  once by `git archive`, is a store object (`ci_action_tarballs`), so an action's versions share
  their chunks, and the store's collection keeps them. One kept as a file under
  `<data>/actions/tar` before moves into the store the first time it's asked for.
- *Done.* The run page: its actions panel lists what the run pinned (each `uses`, its repository
  and commit), and its rerun offers "Fresh actions" when it pinned any, the actions' versions
  looked up again instead of the pinned ones. Checked in Chromium and WebKit, desktop and phone,
  on runs that use their repository's own action (`ci/ui/insights_check.mjs`).
- *Done.* R05's musl Node: a job container whose C library is musl (its dynamic loader is, as
  Alpine's), or a runner on such a machine, runs JavaScript actions on a musl build, kept in
  `<work>/_externals/node<NN>_alpine` as GitHub's runner keeps its own. On x86-64 it is GitHub's
  (`actions/alpine_nodejs`, the newest of the major, needing nothing but musl), checked against
  the SHA-256 digest its release lists; on arm64, which GitHub has none for, the Node project's
  unofficial build, checked against its `SHASUMS256.txt`, which needs the image's `libstdc++`.
  `w07_r05_musl_node_in_an_alpine_container` runs the `pre`/`main`/`post` action in `alpine:3.20`.
- *Done.* R07's `GITHUB_ACTION_PATH` as GitHub's runner has it (corrected 2026-10-24, corpus pass
  1024b): a composite action's, for its steps (its `CompositeActionHandler` sets
  `github.action_path` on each). An action among those steps keeps the composite's, and an action
  on its own (its `pre` too) has none. kiln gave every action its own, so CodSpeed's composite,
  whose github-script step reads `${GITHUB_ACTION_PATH}/.codspeed-runner-installer-hashes.json`,
  looked for it in github-script's directory (ruff's benchmarks). Checked by
  `w07_r07_a_composites_path_is_its_steps_own`. And `${{ github.action_path }}` is where the
  leg's machine has it: in the `docker` executor's container the container's path, as
  `runner.temp` is there; on the host, and for a job's `container:` (as GitHub's runner gives
  it), the host's. next.js's cargo-binstall ran its script by the host's path in the container,
  where nothing was (corpus pass 1024b; `w07_r07_an_actions_path_is_where_the_machine_has_it`).
- *Done.* R09: GitHub's API-calling actions against the facade, unmodified. `github-script`,
  `softprops/action-gh-release` with an asset, and the code-scanning, labeler, pull-request and
  pages actions are W14's and W22-AC02's checks. `tests/hosted_actions.rs`
  (`w14_ac05_github_actions_on_a_native_repository`) adds the rest on a native repository:
  `actions/checkout@v4` at a pull request's head with its submodule (a branch of the repository
  itself, by a relative URL), `actions/download-artifact@v4` of an artifact another run made (its
  `run-id`, with the job's token), and `peter-evans/create-or-update-comment@v4` commenting on the
  pull request as the job. It runs when github.com answers, the actions coming from it.
- *Done.* `actions/checkout` v7, as the corpus pins it, keeps its credentials in a configuration
  file of their own under `RUNNER_TEMP` (`includeIf` in the repository's configuration), and a
  later `git fetch` in the job uses them: on the host, in a job container and in the `docker`
  executor (`w07_r09_checkout_v7_persists_its_credentials`). In a job container (root, over a
  workspace this runner's user owns) git first wants the workspace a safe directory, as on GitHub.
  A job that drops its credentials (`persist-credentials: false`) and fetches again needs them on
  kiln, whose repositories aren't read anonymously as a public one on GitHub is.
- *Done.* AC01 but two (with W22-AC02): F-21's actions green, those that need an account in dry
  configurations against stand-ins for their providers (`tests/account_actions.rs`).
- *Left.* AC01's `azure/login` (a test tenant, W11-AC03) and `actions/attest-build-provenance`,
  which signs only on github.com and GHE.com (W18-R02): what it attests there, kiln verifies.

### W08 Runtime services: store, caches, artifacts, reuse

**Scope.** The content-addressed blob store; GitHub's cache (v2 and the older v1) and artifact
(v4) services, compatible with the toolkit and BuildKit's `type=gha` cache; kiln's cache tiers;
reusing the results of jobs whose inputs have not changed.

**Requirements: the store**

- **W08-R01 Content-addressed store.** Objects are split by content-defined chunking (FastCDC;
  512 KiB min, 2 MiB average, 8 MiB max), each chunk stored once under its SHA-256
  (`<data>/cas/chunks/<2 hex>/<2 hex>/<digest>`), an object being its ordered chunk list
  (`cas_objects`). Writes stream (never buffered whole); reads stream with ranges. Reference counts
  and last-use times drive LRU eviction under `[ci.store] max_size` (default 20 % of the data
  volume) and per-repository quotas (`[ci.store] repo_quota`, default 10 GiB, as GitHub's cache
  limit). `kiln store verify` re-hashes; `kiln store gc` collects.
- **W08-R02 Remote backend** (opt-in): `[ci.store] s3 = { endpoint, bucket, region,
  credentials }` keeps chunks in any S3-compatible store (SigV4 signing implemented in kiln's HTTP
  client), with the local directory as a cache. Built otherwise (CP-T10.1, docs/blobs.md): the
  content-addressed store's objects are keys at `cas/<digest>` in kiln's store of bytes, the local
  store by default (this layout) or any S3-compatible one (`[blobs] store = "s3"`, `[blobs.s3]`),
  with no local cache.

**Requirements: GitHub's services**

- **W08-R03 Signed blob URLs**, a subset of the Azure Blob REST protocol that the toolkit's
  `@azure/storage-blob` client uses, at `/_kiln/blob/<id>?sv=…&se=<expiry>&sp=<r|w>&sig=<hmac>`:
  `PUT` with `x-ms-blob-type: BlockBlob`; `PUT ?comp=block&blockid=<base64>`; `PUT
  ?comp=blocklist` with the XML block list; `GET` and `HEAD` with `Range` or `x-ms-range`; answers
  with the headers the client checks (`x-ms-request-id`, `x-ms-version`, `ETag`, `Last-Modified`,
  `Content-Length`, `Content-Range`, `Accept-Ranges`, `x-ms-blob-type`). Signatures are
  HMAC-SHA256 over method, id, permission and expiry with a server key; tampered or expired URLs
  get 403.
- **W08-R04 Runtime token.** `ACTIONS_RUNTIME_TOKEN` is a JWT signed by kiln (HS256, server key)
  whose `scp` claim carries `Actions.GenericRead:<run> Actions.UploadArtifacts:<run>:<leg>
  Actions.Results:<run>:<leg>`, which the toolkit parses for its backend ids; it expires with the
  leg plus one hour. It also carries GitHub's `ac` claim (the cache scopes, with read and write
  permissions, in lookup order) and the leg's cache mode. Until W10's job tokens, the same token is
  the leg's `GITHUB_TOKEN`.
- **W08-R05 Results service.** `ACTIONS_RESULTS_URL = <server>/`.
  - The toolkit keeps only that URL's origin, so Twirp is served at the origin's root:
    `/twirp/github.actions.results.api.v1.ArtifactService/{CreateArtifact, FinalizeArtifact,
    ListArtifacts, GetSignedArtifactURL, DeleteArtifact}`.
  - The JSON follows protobuf's mapping, as the toolkit uses it:
    - snake_case field names;
    - int64 values as strings;
    - wrapper types as bare values;
    - RFC 3339 times.
  - Errors are Twirp's `{code, msg}`, because the client parses every response as JSON.
  - The toolkit's `CacheService` (cache v2) is used only on github.com, `*.ghe.com` and
    `*.localhost`. It isn't needed for caches on kiln (R07). BuildKit's v2 client may want it
    later (R11).
- **W08-R06 Cache semantics**, GitHub-exact: the client's `version` (a hash of paths and
  compression) must match; lookup tries the exact key, then each restore key as a prefix (newest
  first), in scope order: the current ref, then the pull request's base ref, then the default
  branch; entries are immutable (creating an existing key and version in a scope answers "already
  exists"); unused entries expire after 7 days (configurable); over quota, least recently used
  go first.
- **W08-R07 The cache protocol.** `ACTIONS_CACHE_URL = <server>/` serves `_apis/artifactcache`:
  - `GET cache?keys=&version=`: a hit, or 204 for a miss;
  - `POST caches`, then `PATCH caches/:id` with `Content-Range` (parts arrive in parallel), then
    `POST caches/:id`;
  - `GET caches?key=`.

  The toolkit treats every server but github.com, `*.ghe.com` and `*.localhost` as GitHub
  Enterprise Server, and uses this protocol with it whatever `ACTIONS_CACHE_SERVICE_V2` says. So
  this protocol is what every version of `actions/cache`, the setup actions' caches and BuildKit's
  v1 client talk to on kiln.

  A refusal by the cache mode starts with "cache read denied:" or "cache write denied:", so the
  toolkit warns instead of failing the step. GitHub retired this protocol on github.com only; GHES
  still serves it.
- **W08-R08 Artifacts**, GitHub-exact (v4): unique names per run, immutable after finalize, zip
  uploads, `retention-days` (default `ci.artifact_retention_days`, 90), download by name or id
  within the run, and across runs through the facade's REST endpoints
  (`/repos/{o}/{r}/actions/runs/{id}/artifacts`, `/actions/artifacts/{id}/zip`). v1's `artifacts:`
  tarballs become an artifact named `artifacts`, and `GET /api/jobs/:id/artifacts` keeps working.
  - **GitHub Enterprise Server's refusal.** `@actions/artifact` v2 and later throw
    `GHESNotSupportedError` when `GITHUB_SERVER_URL`'s host is not github.com, `*.ghe.com` or
    `*.localhost`. That covers `upload-artifact@v4` and `download-artifact@v4` and every later
    version.
    - Actions whose scripts carry that refusal get `GITHUB_SERVER_URL=http://kiln.localhost:<port>`
      from the runner, the server's own port (kiln's listener answers plain HTTP on its TLS port
      too).
    - The name reaches the server: on the host `*.localhost` is loopback, and a leg's containers
      have it where the server is (beside `host.docker.internal`), the ones its steps make through
      its Docker socket too (`ExtraHosts`, but a name they give themselves). With a port-less
      `http://kiln.localhost` and no such names, buildx's bake jobs found nothing there.
    - **BuildKit's container.** docker's build-push and bake actions make their git context of
      the name, and BuildKit's own container fetches it with git. git's curl takes every
      `*.localhost` name for its own loopback, whatever `/etc/hosts` says, so the name alone never
      took it to kiln (*corrected 2026-10-24*: corpus pass 1023d, buildx's own validate jobs). A
      BuildKit container a leg makes through its socket gets a loopback relay. That is
      `setup-buildx-action`'s builder, known by its image or buildx's `buildx_buildkit_` name, on
      a network of its own:
      - the runner's static kiln (the egress relay's, W18-R08) is mounted into it and run there
        once it runs, taking connections at `127.0.0.1:<port>`; the name points there too, for
        what reads `/etc/hosts`;
      - the runner dials in through a port published on the host's loopback and splices each
        connection on to the server. Nothing in the container connects to the host, whose firewall
        need not let containers in, and the server may be anywhere the runner reaches;
      - a runner without a static kiln gives none, and such a build's git context still fails;
      - the same way, the builder pulls through kiln's pull-through caches (W12-R02);
      - tests: `w08_r08_buildkit_containers_get_a_loopback_relay`, and
        `w08_r08_a_buildkit_container_reaches_kiln_through_its_relay` with Docker and
        `KILN_EGRESS_BINARY`.
    - Their artifact calls go only to `ACTIONS_RESULTS_URL` and `GITHUB_API_URL`.
    - The runner puts the real server back in what they print and output, so `artifact-url`
      links to kiln.
  - **Unzipped files.** A file uploaded with `archive: false` downloads raw, under its own name
    (`Content-Disposition`).
  - **v1 to v3.** GitHub still takes their protocol, file containers, from an action its version
    check doesn't refuse. bat's `upload-artifact@master` (2.0.1, July 2020) uploads there and its
    run passes, its artifacts kept as v4's are. kiln serves it at `ACTIONS_RUNTIME_URL`
    (`_apis/pipelines/workflows/<run>/artifacts`, `_apis/resources/Containers/<id>`,
    `services/pipelines.rs`):
    - a file at a time, in `Content-Range` chunks, gzipped when that's smaller;
    - another upload by the artifact's name adds to it, as v3 had it (a matrix's legs, one
      artifact); one still uploading waits for the next finish;
    - its size, set, zips what came.

    Such an artifact is one of the run's like any other, read by v4's actions and the REST API,
    and the run's v4 artifacts are read this way too (`w08_r08_artifacts_as_v3_keeps_them`).

**Requirements: kiln's improvements** (G-10, G-19)

- **W08-R09 Cache tiers** (opt-in): `kiln/cache@v1` and the kiln dialect's `cache:` key take
  `{ key, restore-keys, paths, scope: ref | repo | global, policy: pull | push | pull-push, local:
  true }`. With `local: true` the runner keeps chunks it has seen in `<work>/_cache` (LRU,
  `[agent] cache_max`), so a restore downloads only chunks it lacks and a save uploads only new
  ones. Because chunking is content-defined, a `target/` directory that changed by 1 % moves about
  1 % of its bytes.
- **W08-R10 Job-result reuse** (opt-in): `reuse: { paths: [globs], env: [names], max-age: 14d }` on
  a job. The leg's input hash is the SHA-256 of: the job's canonical definition after evaluating
  everything known before it runs, the pinned action SHAs, container and service image digests,
  the leg's requirements and platform, the listed environment and variables, the `needs` outputs
  it reads (W01-R12), the trust level, and the git tree ids of the listed paths at the commit
  (`git rev-parse <sha>:<path>`, so no checkout is needed). When a successful leg with the same
  hash exists within `max-age` and its artifacts still exist, the leg completes at once as
  `reused`, with the earlier leg's outputs and links to its logs and artifacts. "Run anyway"
  (UI, API, `kiln rerun --no-reuse`) forces a real run. A diff that only rebases (W14) reuses
  every leg whose paths it did not touch.
- **W08-R11 BuildKit.** `docker/build-push-action` with `cache-from/cache-to: type=gha` works
  through W08-R05; `type=registry` works against W12's registry.
- **W08-R12 Speed.** Restoring a 1 GiB entry from the server on 10 GbE runs at ≥ 500 MiB/s; uploads
  and downloads use parallel ranges and never hold an object in memory.
- **W08-R13 Insights.** Hits, misses, bytes restored and saved, time saved per key prefix and per
  workflow, for W19.
- **W08-R14 Cache access** (`cache-mode`), GitHub-exact.
  - The modes: `read` (restore only), `write` (both), `write-only` (save only) and `none`.
  - Where it is set: on the workflow and on jobs. A job's mode wins, and it also caps the reusable
    workflows that job calls.
  - When it is unset:
    - `write` for trusted events: `push`, `workflow_dispatch`, `repository_dispatch`, `delete`,
      `registry_package`, `page_build` and `schedule`.
    - `read` for any other event whose run resolves to the default branch (`pull_request_target`,
      `issue_comment`, `workflow_run`, …), so a low-trust run can't poison the default branch's
      caches.
    - `pull_request` runs are not affected: they keep writing only to their merge ref's scope.
  - kiln's own events follow the same rule.
    - Trusted: `land` and `deploy`.
    - Not trusted: `diff`, `review`, `comment`, `agent`, `webhook` and `pipeline`.
    - A repository opts a workflow in with an explicit `cache-mode: write`.
  - Enforcement: the runtime token's cache scopes (W08-R04). Actions see the mode as
    `ACTIONS_CACHE_MODE`, which `actions/cache` and the toolkit read.
  - When the mode stops an operation, the step doesn't fail.
    - A skipped restore is a miss.
    - A skipped save is an informational message.
    - A save the server refuses is a warning.
  - Called workflows: one that asks for more than its caller allows is a validation error, and
    the run doesn't start. Examples are `write` under a `read` caller, or `read` under a
    `write-only` caller (the two don't overlap).

**Design.** `crates/kiln/src/ci/services/`: `cas.rs` (chunker, chunk files, objects, eviction),
`blob.rs` (the Azure-compatible endpoint), `twirp.rs` (the two Twirp services), `cache_v1.rs`,
`artifacts.rs`, `reuse.rs`. The chunker and the hash-input canonicalization are pure functions in
`kiln_ci`.

**Interfaces.** Takes leg identities and tokens (W10). Gives caches and artifacts to W06 and W07,
the store to W07 (actions), W12 (registry blobs), W16 (test reports) and W18 (attestations).

**Acceptance criteria**

- **W08-AC01 Toolkit clients.** A Node test harness using the published `@actions/cache` and
  `@actions/artifact` packages (pinned current versions) against a test server: save, restore by
  key, restore by prefix, miss, scope fallback from a branch to the default branch, "already
  exists"; upload of one file, of 1,000 files, of a 2 GiB file; list; download by name and id;
  delete. *Integration test in CI.*
- **W08-AC02 Actions end to end.** `actions/cache@v4`, `actions/cache@v3` (legacy API),
  `actions/upload-artifact@v4` and `actions/download-artifact@v4` (within and across runs) in
  fixture workflows. *E2E.*
- **W08-AC03 BuildKit.** A Docker build with `cache-to: type=gha,mode=max` then `cache-from:
  type=gha` on a second run reuses every layer. *E2E on a Docker runner.*
- **W08-AC04 Blob protocol.** Block uploads out of order, a block list naming a missing block
  (error), ranged reads at every boundary, expired and tampered signatures (403). *Unit and
  integration tests.*
- **W08-AC05 Dedup.** Saving a 1 GiB directory, changing 1 % of its files, saving again: new chunk
  bytes ≤ 3 % of 1 GiB; a restore with a warm local tier downloads ≤ 3 %. *Integration test.*
- **W08-AC06 Reuse.** The input hash changes when any listed input changes and only then (a table
  of edits); a reused leg shows its source; untrusted never reuses trusted results; "run anyway"
  runs. *Unit and integration tests.*
- **W08-AC07 Eviction and quotas.** Filling past a repository quota evicts least recently used
  entries; expiry after 7 days of disuse (simulated clock). *Unit test.*
- **W08-AC08 Speed.** 1 GiB restore ≥ 500 MiB/s on the CI benchmark host's loopback with a 10 GbE
  shaped link. *Bench.*
- **W08-AC09 Cache access.** For each event and each explicit `cache-mode`, check the effective
  mode, `ACTIONS_CACHE_MODE`, and what `actions/cache`, `actions/cache/restore` and
  `actions/cache/save` do. A `pull_request_target` run restores from the default branch and can't
  save there. A reusable workflow asking for more than its caller allows doesn't start.
  *Integration test.*

**Progress.** In progress.

- *Done.*
  - `kiln_ci::store`, pure code:
    - `cdc`: FastCDC with normalized chunking, streaming (R01);
    - `cache`: modes by event, caps, lookup, immutability, key validation, eviction (R06, R14,
      AC07).
  - `crates/kiln/src/ci/services/`:
    - `cas.rs`: the store, with chunks stored once, ranged reads and collection (R01, AC05 on
      48 MiB), and verification: `kiln store verify` (`ci.store.verify`) re-hashes every chunk
      and moves a corrupt one aside, so its next write stores it again; `kiln store gc`
      (`ci.store.gc`) collects now;
    - `blob.rs`: signed URLs and the Azure Blob subset (R03, AC04);
    - `cache_v1.rs`: the cache protocol, with quotas and idle expiry (R06, R07), recording each
      lookup (hit or miss) and save for R13's insights (hit rates, bytes and time saved by key
      prefix and workflow, in W19's Insights);
    - `twirp.rs`: `ArtifactService` (R05, R08);
    - hourly upkeep.
  - Leg tokens are JWTs carrying GitHub's `scp` and `ac` claims and `ACTIONS_CACHE_MODE` (R04,
    R14).
  - The GHES workaround for `@actions/artifact` (R08). Across runs: W14's REST half.
  - End to end on a local server (part of AC02), with checks that match GitHub's messages:
    - `actions/cache@v4` and `@v3`: save, exact restore, prefix restore, two legs racing for one
      key, and a refused read-only save;
    - `upload-artifact@v4` and `@v7` (zipped and `archive: false`);
    - `download-artifact@v4` and `@v8`: by name, by pattern, raw, and across runs with `run-id`.
- *Job-result reuse (R10, AC06).* A job with `reuse: true` or `reuse: { paths, env, max-age }`
  (kiln) gets an input hash before its leg is queued (`kiln_ci::plan::reuse`). It covers:
  - the leg as planned, with its job and the workflow's `env` and defaults;
  - its actions' pins and its images' digests;
  - runs-on and requirements;
  - the listed variables;
  - its trust;
  - the git objects of the listed paths (globs, files or directories) at the commit, or the
    whole tree when none are listed.

  A leg that succeeded is kept under its hash (`ci_reuse`). A later leg with the same hash,
  within `max-age`, whose source's artifacts are all still there, takes that result over: its
  outputs (which the plan and `needs` see as a success), steps, logs and artifacts, and it
  names its source ("Reused from run #N", linked). Untrusted legs hash differently, so neither
  side takes the other's. `kiln rerun --no-reuse` (and `no_reuse` on the API) runs every leg.
  A leg whose inputs can't all be known (an image's digest, the mirror) runs, and says why in
  the server's log. AC06:
  - a table of edits changes the hash, one input at a time;
  - E2E on a local server: a README-only push reused the build, a change under its `paths` ran
    it, and "run anyway" ran it; now an integration test too
    (`w08_ac06_a_leg_takes_a_result_for_the_same_inputs`: the reused leg names its source run and
    shows its log, and the run-anyway rerun builds again at the reused commit).
  - Where the leg's job is defined (W06-R03's `job.workflow_*`) isn't one of its inputs: for the
    run's own workflow it names the run's commit, as `github.sha` does, and in the hash it made
    every push's inputs new, so nothing was reused (main's CI #1250, 2026-10-25); the job's
    definition is.
- *Done.* R09's `kiln/cache@v1` and the kiln dialect's `cache:` key, on the chunked protocol
  (`crates/kiln/src/ci/services/chunks.rs`, `crates/kiln/src/runner/cache.rs`):
  - `/_kiln/cache` answers for a leg's token:
    - `lookup` returns an entry with its chunk list;
    - `missing` reports which chunks the repository's store lacks (only the repository's own
      chunks count, so no leg can probe another repository's content);
    - `chunks/<sha>` stores a chunk, checked against its digest;
    - `entries` commits an entry once every chunk is there and the sizes and the whole
      digest match.
  - Lookups and saves follow the leg's scopes and cache mode (R06, R14). Two scopes are kiln's
    own:
    - `scope: repo` adds a scope that every ref of the repository reads;
    - `scope: global` adds the owner's: every repository of the owner (`acme/…`) reads it, and
      only a run on its repository's default branch writes it. A save anywhere else is refused
      before any chunk is sent.
  - The runner archives the paths (a tar whose entries name their path), chunks them
    content-defined (`kiln_ci::store::cdc`), and sends only what's missing. A restore unpacks
    into a staging directory before anything is replaced. The step sets `actions/cache`'s
    outputs, and its save runs after the job, unless an exact key hit, under `policy: pull |
    push | pull-push`.
  - `local: true` keeps chunks in `<work>/_cache`, least recently used first out past the
    runner's `[agent] cache_max` (10G by default), so a restore downloads only what the runner
    lacks.
  - A job's `cache: [{ key, restore-keys, paths, scope, policy, local }]` is a `kiln/cache@v1`
    step before its own (`kiln_ci::store::cache::job_steps`).
  - Checked end to end:
    - `w08_r09_kiln_cache_moves_only_new_chunks`: the first run saved 8 MiB, sending all of
      it. The second restored by a restore key with 0 bytes downloaded, changed a file, and sent
      0.5 MiB of the entry's 4 chunks.
    - `w08_r09_kiln_cache_global_scope`: `acme/one`'s main saved a `global` entry, and
      `acme/two` restored it. `other/three` didn't see it, and `acme/one`'s branch was refused
      a save there.
- *Done.* R14's cap for called workflows. A job's `cache-mode` (else its workflow's, else the
  cap it is under) caps the workflow it calls. If the called workflow or one of its jobs asks
  for more, the call doesn't start, saying so ("asks for `cache-mode: write` (its job `inner`),
  more than its caller's `cache-mode: read` allows"). Legs that ask for nothing run with their
  own mode cut down to the cap (`LegSpec::cache_cap`), in their token too. Unit-tested
  (`w08_r14_the_callers_cache_mode_caps_the_called`).
- *Done.* v1's `artifacts: [globs]` on an engine leg: after the job's steps, unless it was
  cancelled (as v1 did), a `kiln/upload@v1` step uploads what the globs match as the run's
  artifact `artifacts`, quietly when nothing matches (`kiln_ci::store::v1_artifacts_step`,
  unit-tested). v1's own jobs keep their tarballs at `GET /api/jobs/:id/artifacts`.
- *Done, otherwise.* R02 through kiln's store of bytes (CP-T10.1, docs/blobs.md): with `[blobs]
  store = "s3"` and `[blobs.s3]`, the content-addressed store's objects are kept in any
  S3-compatible store at `cas/<digest>`, with no local cache, and every other kind of kiln's bytes
  goes there too. The chunk-level tier built first for R02 (`[ci.store.s3]`: the data directory a
  cache of a bucket's chunks) gave way to it when CP-T10.1 was merged in. `kiln/cache@v1`'s
  chunked protocol (R09) moves chunks one by one, which only the local store keeps. With S3 it
  answers 501, which a leg reports as a warning (the cache is neither restored nor saved), while
  `actions/cache` works as on GitHub. Its S3 credentials (an `aws` integration, a key pair, or
  the server's `AWS_*` environment) are what the OIDC issuer's publishing uses
  (`crates/kiln/src/ci/services/s3.rs`).
- *Done.* R11's `type=gha`, and AC03, end to end on a Docker runner
  (`w08_ac03_buildkit_gha_cache`): a build on a BuildKit builder of the run's own exports its
  layers' cache to kiln's (`mode=max`), and the next run's build, on a fresh builder, takes every
  layer from it. It found that BuildKit's cache client (`go-actions-cache`) reads the runtime
  token's `ac` claim only as GitHub writes it, a string holding the scopes' JSON; kiln's was a
  list. It is the string now, and a list minted before is still read.
- *Done.* R11's `type=registry`, end to end (`w08_r11_buildkit_registry_cache`): the cache as an
  image in kiln's registry (W12), the job's own token its credentials (W12-R03), and the next
  run's build, on a fresh builder, takes every layer from it.
- *Done.* AC01 and AC09 with the published clients: `@actions/cache` 6.3.0 and
  `@actions/artifact` 6.3.1, bundled and pinned (`crates/kiln/tests/fixtures/storage-action`),
  in `crates/kiln/tests/storage.rs`:
  - AC01: the cache saved (a second save finds it there), restored by key and by prefix, a miss,
    and a branch restoring its default branch's entry; artifacts of one file, of a thousand and
    (with `KILN_TEST_BIG=1`) of 2 GiB uploaded, listed, downloaded by name and by id, one deleted.
  - AC09: a dispatched run's jobs unset (`write`), `read`, `write-only` and `none`, each with its
    `ACTIONS_CACHE_MODE` and the client restoring and saving as it allows, saying what it skipped;
    a `pull_request_target` run restoring the default branch's entry and unable to save there;
    a called workflow asking for more than its caller allows not starting, and saying why.
- *Done.* AC08: on the bench machine (adacachy), `.kiln/bench.yaml`'s `ci-bench` job, a release
  build, restored a 1 GiB entry at 1,181 MiB/s over HTTP on loopback shaped to 10 Gbit/s, 50 µs
  each way (`cache.restore_10gbe`, the median of its runs; 985262b, 2026-10-25), against 500.
  The run before (a084bb7) measured 1,128 MiB/s.
- *Left.*
  - R12 on a real 10 GbE LAN. The store's part is measured by `kiln ci bench`
    (W22-AC08): a 1 GiB entry restored at 7,962 MiB/s, against 500. R13's insights are done
    (`cache_v1.rs` and the chunked cache record every lookup and save; W19 shows them by key
    prefix, with the time hits saved).
- *Done.* R10's rebased diff (`w08_r10_a_rebased_diff_reuses_what_it_did_not_touch`): a diff's
  first version builds and documents; the trunk moves on, changing only its docs; the diff
  rebased onto it, its own change the same, takes the build's result (its log the first
  version's) and documents again, the listed paths' trees deciding.

### W09 Secrets and variables

**Scope.** Secrets and variables at every scope, encrypted at rest, delivered only where used,
masked; pipeline-scoped secrets; actions that declare the secrets they need and get them, checked
before the run starts; secrets brokered from integrations and external stores. Today every job
receives every `[secrets]` entry of the server configuration; that ends here (v1 config secrets
become server-scope secrets).

**Requirements**

- **W09-R01 Scopes and resolution.** `server`, `organization` (an owner), `repository`,
  `workflow` (one workflow file of one repository: the "pipeline scope"), `environment`. A name
  resolves to the most specific scope that has it: environment, then workflow, then repository,
  then organization, then server. GitHub-exact for the scopes GitHub has (environment over
  repository over organization).
- **W09-R02 Names.** `[A-Za-z_][A-Za-z0-9_]*`, case-insensitive, stored upper-case, not starting
  with `GITHUB_` (GitHub-exact) or `KILN_`. Values up to 64 KiB. Up to 1,000 per scope.
- **W09-R03 Encryption.** AES-256-GCM (aws-lc-rs) with a fresh nonce per write, under a data key
  wrapped by the master key (`[ci.secrets] master_key_file`, default `<data>/secrets/master.key`,
  32 random bytes, mode 0600, created at first start). `kiln secrets rotate-key` re-wraps in the
  background. The database never holds a plaintext value, and the API never returns one: reads give
  names, scopes, sizes, timestamps and last use.
- **W09-R04 Delivery.** A leg receives only the secrets its expressions reference (W01-R12): in the
  workflow, in the reusable workflows it calls, and those its kiln actions declare. `GITHUB_TOKEN`
  is always available. Secrets go in `AssignV2` over the runner's mutually authenticated QUIC
  connection. Environment secrets of a protected environment are sent only after approval
  (`ServerMsg::SecretsFor`). Every delivery is recorded (`ci_secret_uses`), shown as "last used by
  run #N, leg L".
- **W09-R05 GitHub-exact rules.** The `secrets` context's availability (Appendix B), reusable
  workflows getting secrets only explicitly or by `inherit`, untrusted runs (fork pull requests)
  getting no secrets but a read-only `GITHUB_TOKEN`, and secrets referenced in `if:` at job level
  being an error.
- **W09-R06 Pipeline-scoped secrets** (G-05). A workflow-scope secret is visible only to runs of
  that workflow. A kiln-dialect workflow may declare what it uses:

  ```yaml
  secrets:
    NPM_TOKEN: { required: true }
    SENTRY_DSN: { required: false }
  ```

  When declared, the list is exhaustive: referencing another secret is `KCI4041`, and only the
  declared ones are resolved. When the run starts, every required secret must resolve for the
  environments its jobs bind; if one does not, the run ends `startup_failure` with `KCI4040`
  naming each missing secret, the scopes searched, and a link to add it.
- **W09-R07 Actions that declare secrets.** A kiln action's metadata may say:

  ```yaml
  secrets:
    DEPLOY_KEY: { required: true, description: "Key for the deploy host" }
  ```

  Inside the action, `${{ secrets.DEPLOY_KEY }}` is the calling job's secret of that name, or the
  one the caller maps: `with-secrets: { DEPLOY_KEY: PROD_DEPLOY_KEY }`. The action sees only what it
  declares. Before the run starts the plan checks that each required declared secret resolves for
  the calling job ("needs DEPLOY_KEY: found in workflow scope"), failing with `KCI4040` otherwise.
  So an action can rely on `secrets.DEPLOY_KEY` existing in the pipeline it runs in. GitHub's
  JavaScript, Docker and composite actions have no such declaration and keep GitHub's rule: they
  get secrets only through `with` and `env`.
- **W09-R08 Brokered secrets.** A secret may be defined as coming from an integration (W11)
  instead of a stored value: `{ from: "integration:aws-prod" }` yields short-lived credentials at
  leg start; `{ from: "vault:kv/data/ci#npm" }`, `"aws-sm:arn:…"`, `"gcp-sm:projects/…"`,
  `"azure-kv:…"`, `"infisical:…"`, `"doppler:…"`, `"1password:…"` read an external store through
  the broker, authenticated with kiln's own OIDC identity where the store accepts it. Brokered
  values are never stored.
- **W09-R09 Variables**, GitHub-exact (`vars` context; same scopes; plain text; up to 48 KiB each).
- **W09-R10 Secret outputs** (W04-R22) are encrypted with the same keys and deleted with the run.
- **W09-R11 Management.** UI (Settings → Secrets, and on each repository, workflow and environment
  page: a scope select, the list, set and delete in place), CLI (`kiln secrets set NAME --scope
  repo:owner/name [--from-file f | stdin]`, `list`, `delete`, `import --dotenv f`), API.
  Every change is audited (W19).

**Interfaces.** Takes W01's references, W04's plan, W11's broker. Gives values to W06 (delivery
and masking), names to the editor's completion (W21; names only).

**Acceptance criteria**

- **W09-AC01 At rest.** After setting 100 secrets, a scan of the SQLite file, its WAL and the data
  directory finds none of the values. *Integration test.*
- **W09-AC02 API.** No route, MCP tool or event ever contains a value (a test walks the route table
  and calls every read). *Integration test.*
- **W09-AC03 Resolution.** A table of scopes × names gives the documented winner. *Unit test.*
- **W09-AC04 Least delivery.** A leg referencing one of ten secrets receives exactly that one plus
  `GITHUB_TOKEN` (the assignment is inspected). *Integration test.*
- **W09-AC05 Pipeline scope.** A workflow-scope secret is visible to runs of its workflow and absent
  from another workflow of the same repository. *E2E.*
- **W09-AC06 Declarations.** Missing required secrets give `KCI4040` with scopes searched; an
  undeclared reference gives `KCI4041`; an action reading an undeclared secret gets nothing and the
  validator reports it. *Integration test.*
- **W09-AC07 Environments.** Environment secrets reach a leg only after approval, through
  `SecretsFor`. *Integration test.*
- **W09-AC08 Untrusted.** A fork pull request's legs get no secrets and a read-only token
  (W18-AC03). *E2E.*
- **W09-AC09 Rotation.** `rotate-key` re-wraps every value; runs keep working throughout.
  *Integration test.*

**Progress.** In progress.

- *Done.*
  - `kiln_ci::secrets`, pure code:
    - names and scopes, and which scope wins (R01, R02, AC03);
    - which secrets a job reads, and what a called workflow's leg gets (R04, R05);
    - declared secrets (R06): `KCI4041` from the validator, `KCI4040` at run start.
  - `crates/kiln/src/ci/secrets.rs` (R03, R09):
    - AES-256-GCM under a data key wrapped by the master key file, ciphertexts bound to scope and
      name;
    - rotation;
    - the config's `[secrets]` as server scope;
    - uses recorded;
    - variables.
  - Each trusted leg gets exactly the secrets it references, plus `GITHUB_TOKEN`; untrusted runs
    get only the token (R04).
  - The API (names only) and the CLI: `kiln secrets`, `kiln vars` (R11).
  - Checked end to end on a local server:
    - resolution across the repository, server, workflow and organization scopes;
    - AC05, a workflow-scoped secret invisible to another workflow;
    - AC04, each leg receiving only its references, read from the recorded uses;
    - masking;
    - `vars` by scope;
    - AC01, no stored value anywhere in the data directory;
    - AC09, rotation with runs still working.
  - R10: secret outputs (W04-R22) sealed by the master key the moment the server reads them,
    bound to their leg and name, deleted with the run; consumers get them only as masked
    environment; the step outputs that feed them are masked as their step ends.
  - `ACTIONS_STEP_DEBUG` and `ACTIONS_RUNNER_DEBUG` reach a leg whenever they're set, unmasked
    (W06-R13).
- *Done.* R08, brokered secrets (`crates/kiln/src/ci/broker.rs`, migration
  `20261018-2300_w09_brokered.sql`):
  - **Defining one.** `PUT /api/ci/secrets/:name` with `{ "from": … }` (or `kiln secrets set NAME
    --from …`) defines a secret by where its value comes from. The source is checked when it is
    set.
  - **The sources:**
    - `integration:<name>`: a stored token as it is; AWS credentials as `credential_process`
      JSON; a Google or Azure access token (Azure's `?scope=`, Resource Manager by default);
    - `aws-sm:<arn>`: a Secrets Manager secret's string;
    - `gcp-sm:projects/<p>/secrets/<s>[/versions/<v>]`: a Secret Manager version's payload,
      `latest` by default;
    - `azure-kv:<vault>/<secret>`: a Key Vault secret's value.

    The stores are read through the repository's integration of their cloud (`@<integration>`
    names one), which exchanges kiln's identity for the leg's workload (W11).
  - **At leg start.** Its value is made then, never stored, and delivered and masked as any
    secret. A more specific scope's definition wins as usual.
  - **When one can't be made**, the leg goes on without it, with a warning that says why.
  - **Verified end to end:** a secret brokered from a Cloudflare integration reached the leg
    masked. One from Secrets Manager, through an AWS role that doesn't trust this server, left a
    warning naming AWS's refusal. A `vault:` source was refused when set.
- *Done.* R07, kiln actions' declared secrets:
  - **Recorded at run start.** When a run starts, each action's `secrets:` (with W07-R11's
    `requires:` and `permissions:`) is read from its `action.yml` and kept with the pinned
    action.
  - **Checked.** A required one that doesn't resolve for the job using the action fails the run
    then, with `KCI4040`. The message names the action, the job, the name `with-secrets` maps it
    to, the scopes searched, and how to add it.
  - **Delivered.** The leg gets the job's secrets those declarations read.
  - **Seen inside.** Inside the action, `secrets` is exactly its declarations: each the job's
    secret of that name, or the one `with-secrets` maps. The job's are back after it.
  - **Verified end to end:**
    - an action declaring `DEPLOY_KEY`, mapped from `PROD_DEPLOY_KEY`, read it (masked) and saw
      nothing of the job's undeclared `OTHER`, which the job's own step read;
    - a mapping to an unset secret failed the run at start with `KCI4040`.
- *Done.* R04's environment secrets after approval (AC07). The dispatcher holds a leg bound to
  a protected environment out of the scheduler until its protection lets it go (W15-R04). A
  leg's secrets, its environment's included, are resolved only when it is assigned, and they go
  in that `AssignV2`. So nothing of a protected environment's reaches a runner before approval,
  and no separate `SecretsFor` message is needed. Checked end to end
  (`w09_ac07_environment_secrets_after_approval`): while the leg waited for its reviewer, the
  environment's secret was never used. Once approved, the leg read it and the use was
  recorded.
- *Done.* The UI: Settings → CI's secrets and variables, by scope (W21-R11).
- *Done.* R08's other stores (`crates/kiln/src/ci/integrations/stores.rs`), each an integration
  kind, read as a leg starts like the clouds' and never stored:
  - `vault:<path>#<key>`: HashiCorp Vault, kiln's identity for the leg at the JWT auth method
    (`role`, `auth_path`, `namespace`), then the key from KV v2 or v1;
  - `infisical:<environment>[/<path>]/<NAME>`: Infisical, a machine identity by OIDC or universal
    auth;
  - `doppler:<project>/<config>/<NAME>`: Doppler, a service token or a service account identity
    by OIDC, the computed value;
  - `1password:<vault>/<item>/<field>`: 1Password through a Connect server.

  Each store's address keeps to the server's reach, and Verify signs in as a leg would.
  Checked against a fake of all four (`w09_r08_secret_stores`).

- *Done.* AC02. The leak test (`w22_ac07_secrets_never_leak`) runs a workflow that puts a secret
  everywhere a step can (its log, an annotation, the summary, an output, the environment), then
  walks every read of the route table, each `*` the run's ids and each `**` the tails reads have,
  bare and with the scope, repository and words reads take, and calls every read-only operation as
  an MCP tool does. None answers with the value. When written, 66 of the 108 read patterns and 39
  of the 58 read-only tools answered (the rest have nothing to show on that server), the secrets,
  variables, run, leg and log reads among them.
- *Done.* AC06, end to end (`w07_ac06_declared_secrets`). A kiln action's required secret,
  unset, fails the run as it starts with `KCI4040`, naming the scopes it searched and how to add
  it; once set, it's there inside the action, masked. A secret the action reads without
  declaring it gets nothing, though the repository has it, and the validator says so (`KCI4041`,
  a warning for an action: `crates/kiln-ci/src/wf/action.rs`), as it does, an error, for a
  workflow that declares its secrets and uses another.
- *Done.* AC08 is W18-AC03's end-to-end check: a fork pull request ran untrusted, with no
  secrets and a token whose writes through the facade got 403.
- *Done.* R11's pages: a repository's secrets and variables in its details on the Repositories
  page, and a workflow's under its editor, each listed (never a value), set and removed in place,
  for those who may manage them (`ScopeSecrets`, Settings → CI's lists). Checked in Chromium and
  WebKit, desktop and phone (`ci/ui/people_check.mjs`).
- *Done.* R11 on each environment's page (W15-R08): its secrets and variables, set and removed
  there, values never shown again.

### W10 Identity: tokens, principals, OIDC issuer

**Scope.** Who calls kiln and what they may do (adapted from [access.md](access.md)), the tokens
that prove it, and kiln as an OpenID Connect issuer whose tokens jobs exchange for credentials
elsewhere (G-24, G-33).

**Requirements: principals and tokens**

- **W10-R01 Principals.** `owner` (whoever installed kiln: admin token, password, sign-in links),
  `user`, `agent` (always on behalf of a user; its rights are the intersection of its grant and
  that user's), `job` (a leg's attempt), `machine` (a runner: mutual TLS, not an API principal).
  Today's admin token, password sign-in, one-time links and MCP tokens keep working.
- **W10-R02 Capabilities and scopes**, as access.md lists them, extended for CI: `runs.read`,
  `runs.trigger`, `runs.cancel`, `runs.rerun`, `runs.debug`, `runs.approve`, `logs.read`,
  `artifacts.read`, `artifacts.write`, `caches.write`, `workflows.edit` (always as a reviewed
  change), `secrets.use:<name>`, `secrets.manage`, `variables.manage`, `integrations.manage`,
  `runners.read`, `runners.manage`, `runners.terminal`, `pools.manage`, `images.build`,
  `routing.manage`, `environments.deploy`, `environments.manage`, `diffs.read`, `diffs.submit`,
  `diffs.review`, `diffs.land`, `queue.manage`, `agents.run`, `agents.manage` (every agent,
  whoever opened it: docs/agents.md, "Whose agents"), `agents.attach`, `experiments.run`,
  `settings.manage`, `tokens.manage`, `users.manage`, `audit.read`. Scopes: server, organization,
  repository, workflow, environment, runner, pool, and a deploy environment (`deploy:<env>`,
  deploy-api.md §15). Roles `viewer`, `developer`, `maintainer`,
  `admin`, `owner` as access.md defines, plus custom roles. An explicit deny wins.
- **W10-R03 Tokens**, stored as SHA-256 hashes with their last use: personal access tokens
  `kiln_pat_<id>_<secret>` (≤ 1 year), agent tokens `kiln_agt_<id>_<secret>` (rotated hourly while
  the agent lives), job tokens `kiln_job_<id>_<secret>` (one per attempt, random, revoked when the
  attempt ends; replacing today's HMAC of the admin token and job id), registry tokens (W12). The
  prefixes let masking (W06-R08) and secret scanners recognize them.
- **W10-R04 Job permissions.** A job token carries the job's resolved `permissions` (W04-R06) and
  kiln's additions (`permissions: { binaries: publish, environments: deploy, registry: push }`).
  The facade (W14), the registry (W12), the API and runtime services check it.
- **W10-R05 One route table.** Each route declares its capability and scope; authorization, the
  OpenAPI document (`/api/openapi.json`), the CLI and the MCP tools come from it (W20).

**Requirements: the OIDC issuer**

- **W10-R06 Issuer.** `[ci.oidc] issuer` (default `<public_url>/oidc`) serves
  `<issuer>/.well-known/openid-configuration` and `<issuer>/jwks`. Signing keys live in
  `oidc_keys`: RS256 by default (accepted everywhere), ES256 optional. A new key is published 7 days
  before it signs and kept published 7 days after it stops; rotation every 90 days.
- **W10-R07 Job tokens for the toolkit.** With `permissions: { id-token: write }` (GitHub-exact
  requirement), a leg gets `ACTIONS_ID_TOKEN_REQUEST_URL` (`<server>/_kiln/oidc/token?api-version=1`,
  which already has a query, because the toolkit appends `&audience=`) and
  `ACTIONS_ID_TOKEN_REQUEST_TOKEN`. `GET` with that bearer and an optional `audience` answers
  `{"value": "<jwt>"}`, GitHub-exact.
- **W10-R08 Claims**, GitHub-compatible so existing trust policies and actions work: `iss`, `aud`
  (default `[ci.oidc] default_audience`, the issuer URL), `sub`, `jti`, `iat`, `nbf`, `exp`
  (5 minutes by default, at most 1 hour), `repository`, `repository_id`, `repository_owner`,
  `repository_owner_id`, `repository_visibility`, `ref`, `ref_type`, `ref_protected`, `sha`,
  `actor`, `actor_id`, `event_name`, `workflow`, `workflow_ref`, `workflow_sha`,
  `job_workflow_ref`, `job_workflow_sha`, `run_id`, `run_number`, `run_attempt`,
  `runner_environment` (`self-hosted`), `environment`, `head_ref`, `base_ref`; plus kiln's
  `kiln_trust` (`trusted`/`untrusted`), `kiln_executor`, `kiln_runner`, `kiln_pool`, `kiln_job`,
  `kiln_diff`, `kiln_diff_version`, `kiln_stack`.
- **W10-R09 Subjects.** Default GitHub-exact: `repo:<owner>/<repo>:environment:<env>` when the job
  has an environment, `repo:<owner>/<repo>:pull_request` for pull requests, otherwise
  `repo:<owner>/<repo>:ref:<ref>`. Per organization or repository, the subject can be built from a
  list of claim keys (GitHub's "customize subject claims", `include_claim_keys`), e.g.
  `["repo", "context", "job_workflow_ref", "kiln_trust"]`.
- **W10-R10 Untrusted runs** get ID tokens only when the repository allows it, and always with
  `kiln_trust: untrusted`, so cloud trust policies can refuse them.
- **W10-R11 kiln as a workload.** The server mints tokens for itself (`sub:
  kiln:<server-id>:service:<name>`) and for agents it starts (`sub: kiln:agent:<session>`, on
  behalf of a user), so the broker (W11) and agents (W20) can federate into model providers and
  secret stores without static keys.
- **W10-R12 Reachability.** Some providers fetch discovery and keys from the internet (AWS, Azure,
  HashiCorp Cloud Platform, Tailscale, Databricks, Snowflake, JFrog, Cloudsmith, Quay); others
  accept keys uploaded or pasted (Google Cloud, Anthropic, OpenAI). kiln therefore offers three
  ways, chosen in Settings → OIDC: (1) serve the two issuer paths through the public front door
  (only those paths, rate-limited); (2) `kiln oidc publish --to s3://… | gs://… | <static
  host>` writes them to a static host on every key change, and the issuer is that URL; (3)
  `kiln oidc jwks` prints the keys for providers that accept them inline. The OIDC page shows
  each configured provider's needs and whether they are met.

**Acceptance criteria**

- **W10-AC01 Tokens.** Hash-only storage, expiry, revocation, last use, prefixes masked; a revoked
  token is refused within 1 s everywhere. *Integration test.*
- **W10-AC02 Authorization.** For every route in the table, each role and an agent with a reduced
  grant: allowed or 403 exactly as declared. *Integration test generated from the table.*
- **W10-AC03 Toolkit.** `core.getIDToken()` and `core.getIDToken('sts.amazonaws.com')` in a JS action
  return verifiable JWTs with the claims of W10-R08 and the right `aud`. *Integration test.*
- **W10-AC04 Real federation.** With kiln's issuer trusted by an AWS account (role trust policy on
  `sub`), `aws-actions/configure-aws-credentials` (unmodified) gets credentials and `aws sts
  get-caller-identity` shows the role; the same for Google Cloud
  (`google-github-actions/auth`), Azure (`azure/login` with a federated credential) and Vault.
  *E2E against test tenants; user-assisted to create the tenants once.*
- **W10-AC05 Keys.** Rotation publishes the next key early and keeps the old one late; tokens signed
  just before rotation still verify. *Unit test with a simulated clock.*
- **W10-AC06 Subjects.** Default and customized subjects for push, pull request and environment
  jobs. *Unit test.*
- **W10-AC07 Static publishing.** `kiln oidc publish --to <dir>` writes documents that a provider
  reading them over HTTPS accepts (checked by a verifier that fetches only from that location).
  *Integration test.*

**Progress.** In progress.

- *Done.*
  - Job tokens (R03, R04):
    - `GITHUB_TOKEN` is `kiln_job_<leg>_<random>`, stored as its SHA-256 hash with its last use,
      carrying the job's permissions;
    - it is revoked when its attempt ends;
    - the facade checks it with GitHub's mapping (git fetches need `contents: read`, and so on),
      refusing with "Resource not accessible by integration";
    - `ACTIONS_RUNTIME_TOKEN` stays the leg's signed JWT (W08-R04).
  - The OIDC issuer:
    - `kiln_ci::oidc`: rotation, claims, subjects and the discovery document (R06, R08, R09, AC05,
      AC06);
    - `crates/kiln/src/ci/oidc.rs`: RS256 keys sealed by the master key, the open discovery and
      JWKS documents, and `/_kiln/oidc/token` for legs granted `id-token: write` (R07);
    - untrusted runs get no ID tokens unless `[ci.oidc] untrusted` (R10).
  - Workload tokens (R11, the CD half): `kiln_ci::oidc::workload_claims` gives a CD sync a deploy
    job's subject (`repo:<owner>/<repo>:environment:<env>`), so one trust policy per environment
    covers both; `kiln_source`, `kiln_app`, `kiln_deployment` and `kiln_trigger` tell them apart.
    `ci::oidc::mint` signs them for the broker (W11).
  - Publishing (R12): `/api/ci/oidc` (status, `jwks-export`, `rotate`, `publish`); `kiln oidc
    status | jwks | publish --to <dir> | rotate [--now]`; `[ci.oidc] publish_dir`, rewritten by
    the hourly upkeep and at each rotation, each file replaced by rename. AC07: a verifier that
    reads only the published files accepts the signing and next keys' tokens and refuses
    unpublished keys, forged bodies and other issuers.
  - Checked end to end on a local server:
    - AC03: `core.getIDToken('sts.amazonaws.com')` returns a token that verifies against the
      published keys, with GitHub's claims;
    - no ID token variables without the permission;
    - the token permissions;
    - a token refused once its attempt ended;
    - `kiln oidc` publishing to a directory, and a rotation's next key published at once;
    - AC04's Vault case: unmodified `hashicorp/vault-action` logged in to a Vault dev server whose
      JWT auth trusts kiln's issuer by discovery, and read a secret (a role bound to the
      branch's subject).
  - Principals, roles and the one route table (R01, R02, R05; `crates/kiln/src/authz/`):
    - the owner keeps every way in (the admin token, the password, one-time links, MCP tokens)
      and is never held to the table;
    - users (`users`) sign in with a one-time link of their own (`kiln users add | link`,
      `POST /api/users/:id/link`: only the newest link works, once, for a day); a session
      carries the user's epoch, so `kiln users sign-out` and disabling a user end them all;
    - grants (`grants`): a built-in role (`viewer`, `developer`, `maintainer`, `admin`,
      `owner`), a custom role (`roles`, `kiln roles set`) or a capability, in a scope (`server`,
      `org:`, `repo:`, `workflow:`, `environment:`, `runner:`, `pool:`), written
      `developer@org:acme`; a grant reaches what its scope contains, and a deny (`!…`) wins;
    - the route table (`authz::ROUTES`) gives each route its capability and where its scope comes
      from: a path segment's repository, run, job or runner, `?repo=`, or `?scope=`. A route it
      doesn't name needs `settings.manage`. `GET /api/openapi.json` is generated from it. The
      repository list shows what the caller reads, and a plan preview checks the repository its
      body names;
    - every change a non-owner makes is audited under its name (`user:<name>`, `pat:<id> of
      user:<name>`, `agent:<label> for user:<name>`, `job:<leg>`).
  - Personal access and agent tokens (R03): `kiln_pat_<id>_<secret>` (1 to 366 days) and
    `kiln_agt_<id>_<secret>` (an hour; `POST /api/tokens/:id/rotate` gives the next secret and
    the old one stops at once). Each carries its own grant, so it does at most what both it and
    its user allow. Tokens are minted from a session (or with `tokens.manage`), never by a token
    for itself: `kiln tokens create | list | revoke | rotate`. Disabling a user revokes theirs.
    The runner's masker masks all three prefixes by their shape, across chunk boundaries.
  - Job tokens on the API (R04): a job token reaches the API for what its job's permissions
    give. `actions: read` reads its repository's runs, logs and artifacts; `actions: write` also
    triggers, cancels and reruns there; `contents: write` triggers; `environments: deploy`
    deploys; `binaries: publish` publishes binaries, so a release workflow needs no admin token.
    kiln's scopes take their verbs (`publish`, `deploy`, `push`) as `write`.
  - AC02 as a unit test generated from the table: every route, for each built-in role on the
    server and on one repository, for an agent acting for a maintainer whose token is reduced to
    `viewer@repo:…`, and under a deny.
  - Checked end to end on a local server (AC01, AC02), 45 checks:
    - a link signs a user in once;
    - a developer on one repository reads its runs there, and gets 403 for every repository's
      runs, secrets, runners and a route the table doesn't name;
    - another organization's viewer sees an empty repository list and can't preview;
    - a PAT reduced to `viewer` reads but can't cancel, and is refused 3 ms after it is revoked;
    - an agent token rotates and its old secret is refused;
    - a deny wins over a role;
    - disabling the user ends their session and revokes their agent's token;
    - the audit names the user, and the admin token still does everything.

    A job token with `actions: read` read its repository's runs (200), not every run nor the
    runners (403), couldn't cancel (403), got past the check to publish binaries, and was masked
    in its own log.
- *R09, subjects per scope* (`ci_oidc_subjects`; `GET/PUT/DELETE /api/ci/oidc/subject?scope=`,
  `kiln oidc subject --scope repo:acme/app --keys repo,context,kiln_trust | --default |
  --inherit`; the facade's `GET /repos/{owner}/{repo}/actions/oidc/customization/sub`):
  - The subject comes from the repository's setting, else its organization's, else the server's,
    else `[ci.oidc] subject_claims`.
  - A repository's `use_default` is GitHub's default subject, whatever its organization says.
  - Keys must be claims kiln's tokens carry, or GitHub's `repo` and `context`.
  - Setting a repository's needs `settings.manage` there; reading needs `runs.read`.
  - Verified end to end: a leg's token said `repo:acme/e2e:ref:refs/heads/main:kiln_trust:trusted`
    under the organization's keys, and GitHub's default again once the repository chose
    `use_default`. The facade answered both in GitHub's shape.
- *Done.* Run lists narrowed row by row: `/api/runs` and `/api/ci/runs` (and `ci.runs.list`) list
  the runs of the repositories the caller may read, `?repo=` narrowing that further; a person
  of one repository lists without saying which and gets its runs alone, and asking for another
  gets none (`w10_run_lists_narrowed_to_what_the_caller_reads`). The event stream too: such a
  person hears events about those repositories (a run's, a job's run's, any that names one) and
  their own inbox's, the rest being the server's, so the UI's live updates work for them. And the
  overview (`/api/overview`, the home page): theirs is those repositories' runs, jobs and queue,
  with none of the server's (its runners, processes, samples and host name), and the home page
  shows it so, without the fleet and the server.
- *Done.* `secrets.use:<NAME>`: a run's legs are given only the secrets whoever started it may
  use, in the leg's environment (else its workflow).
  - A grant names one secret (`secrets.use:DEPLOY_KEY@repo:acme/app`, names compared as kiln keeps
    them), or any (`secrets.use`, `secrets.use:*`); custom roles take them too. `developer` and
    up use any, so a deny takes one away (`!secrets.use:DEPLOY_KEY@repo:acme/app`), and an agent
    does what its grant and its person's both allow.
  - Who started a run: the pusher of a kiln-hosted repository's branch or tag (kept as the push
    is taken, before git has it; a push kiln can't keep this for is refused), a diff's or change
    request's submitter (its versions keep it; a restack keeps the version before's), whoever
    landed a land queue entry (a batch: each of them), and the caller of a dispatch or an
    `agent` event; a rerun is held to its run's, as GitHub's keep its actor's. Their grants are
    read as each leg starts (a user gone or disabled: none), a token's own as the run started (an
    agent's is rotated while it waits).
  - What the repository starts itself (schedules, another run, `pull_request_target`, a comment's
    `issue_comment`, a host's events) and a job's runs answer to its configuration, as do the
    owner's.
  - A secret withheld is left out, as an untrusted run's are, and the leg says which and why
    (`w10_r02_secrets_use_by_name`, `w10_secrets_use_holds_a_run_to_whoever_started_it`).
- *Done.* R11, kiln as a workload: the server's own tokens (`kiln:<server-id>:service:<name>`,
  the id kept with its data, `kiln_service` naming the service: `ci::oidc::service_token`, for
  W11's broker) and an agent's (`kiln:agent:<session>`, `kiln_on_behalf_of` the person who
  started it, `kiln_harness`). An agent asks for its own with `ci.oidc.token` (an MCP tool, code
  mode's `kiln.ci.oidc.token`, `POST /api/ci/oidc/token`) for the audience it federates into,
  ten minutes by default and an hour at most; nobody but an agent session gets one. Unit-tested
  (`w10_r11_service_and_agent_subjects`, `w10_r11_agent_and_service_tokens`).
- *Done.* Passkeys and people's own passwords (`crates/kiln/src/authz/passkeys.rs`,
  `passwords.rs`, migration `20261019-2300_w10_passkeys.sql`; Settings → Sign-in):
  - Whoever is signed in makes a passkey (WebAuthn) in Settings, the owner or a person, never
    through a token or an agent. It's a discoverable credential with user verification, for the
    domain of the address in use (the public URL's or a front door's): ES256, EdDSA or RS256,
    without attestation.
  - Once any is kept, the sign-in page offers it: the browser lists this domain's passkeys, and
    the one picked signs its person in. Each challenge is used once, within 5 minutes, on its
    origin. Refused: another domain's, another key's signature, a counter gone backwards (a
    copied authenticator), a removed passkey, a disabled person.
  - A person may keep a password of their own (Argon2id), changed and removed with the current
    one, and sign in with their name and it. Guesses are throttled per person, and an unknown
    name takes as long as a known one.
  - The sign-in page offers the hosts SSO is set up with (W23-R08) too.
  - Checked end to end with a software authenticator (`w10_passkeys_and_passwords`), and in
    Chromium with a virtual authenticator: a passkey made in Settings, then signed in with, and
    a name and password; the pages in WebKit too, desktop and phone.
- *Done.* Settings → People and → Tokens (`ui/src/views/settings-people.tsx`, `settings-tokens.tsx`):
  - People, for those with `users.manage`: who signs in here and their grants, someone added
    with a first grant (a role, and where it holds: the server, an organization, a repository)
    and their email for SSO to find them, a sign-in link for each, more grants given and taken
    back, and someone removed (signed out everywhere, their tokens revoked). Roles: the built-in
    ones and their capabilities, and roles of one's own made from capabilities and removed.
  - Tokens, for everyone signed in: one's personal access tokens (everyone's, with
    `tokens.manage`) with what each may do, when it ends and when it was last used; a new one
    shown once; any revoked.
  - Checked in Chromium and WebKit, desktop and phone (`ci/ui/people_check.mjs`): a person
    added, linked, granted and removed; a role made and removed; a token made, shown once and
    revoked; a viewer with Tokens and no People.
- *Done.* R12, the issuer reached from the internet, three ways (`crates/kiln/src/ci/oidc.rs`):
  - An address of its own (`[ci.oidc] listen`, with `listen_cert` and `listen_key` for the
    issuer's public name, else the server's TLS, else plain HTTP behind a TLS proxy or tunnel)
    that serves the two documents and nothing else: any other path is 404 whatever the token,
    any method but GET is refused, and each client gets a burst of 30 requests, then one a
    second (429 past it).
  - Published where the hourly upkeep and each rotation keep them (`[ci.oidc] publish_to`):
    a directory for a static host, `s3://<bucket>[/<prefix>]` (any S3-compatible store,
    `publish_endpoint` and `publish_region`, signed in with `publish_credentials`: a key pair or the
    server's environment, since an `aws` integration can't publish the first keys AWS checks
    its token against) or `gs://<bucket>[/<prefix>]` (through a `gcp` integration: Google Cloud
    takes the keys pasted into its pool). Only what changed is sent again. `kiln oidc publish
    --to <dir | s3://… | gs://…>` does the same by hand, with the machine's own AWS keys or
    Google token.
  - `kiln oidc jwks` and Settings → OIDC's download, for providers that take keys pasted in.

  Settings → OIDC says how it's reached: whether the issuer URL is public (HTTPS, a name or
  address the internet reaches), its own addresses, where it's published, when, and what went
  wrong; and for each federating integration, what it needs (its keys fetched over the internet,
  or pasted in) and whether it trusted kiln when last verified. Unit-tested against a fake S3
  that checks every signature and a fake Cloud Storage (the documents published to the bucket
  verify kiln's tokens, as a provider checks them), and end to end
  (`w10_r12_the_issuers_own_address`).
- *Left.*
  - AC01, AC02, AC04's cloud cases (AWS, Google Cloud, Azure: test tenants, user-assisted).

### W11 Integrations and the credential broker

**Scope.** External services kiln and its jobs work with, each configured once with the best
authentication it accepts: OIDC federation wherever it is accepted, short-lived tokens minted from
an app or client credentials where not, scoped stored tokens as the last resort. Appendix C is the
catalog; this section is the machinery.

**Requirements**

- **W11-R01 Registry.** `ci_integrations` holds each configured integration: `kind`, `name`,
  scope, non-secret `config`, a reference to its stored secret (W09) if any, and its verification
  state. Each kind is a provider module with a config schema, `verify()`, `apply()` (what a job
  gets) and, for minting kinds, `mint()`.
- **W11-R02 Modes.**

  | Mode | Who exchanges | What the job gets |
  | --- | --- | --- |
  | `oidc` | the job, with its kiln ID token (W10) | the provider's native federation files and variables, so its SDKs and CLIs work unchanged (e.g. AWS `AWS_ROLE_ARN` + `AWS_WEB_IDENTITY_TOKEN_FILE`; Google `GOOGLE_APPLICATION_CREDENTIALS` pointing at an `external_account` file whose token file the runner refreshes; Azure `AZURE_CLIENT_ID`, `AZURE_TENANT_ID`, `AZURE_FEDERATED_TOKEN_FILE`); for providers without native file support, `kiln auth` performs the token exchange and exports the short-lived token as that provider's documented variable |
  | `oidc-chain` | the job: kiln token → provider A → provider B | e.g. Datadog through an AWS role, Vertex AI and Gemini through Google Cloud, Bedrock through AWS, Azure OpenAI through Entra |
  | `app` | the server, from an app identity or client credentials | a short-lived token: GitHub App installation tokens limited to the job's permissions and repository; Atlassian OAuth client-credentials tokens (1 hour); Linear client-credentials app tokens (cached until near expiry); Wiz and Snyk service-account tokens |
  | `token` | nobody | a stored token, scoped to the job by W09's rules: Vercel, Cloudflare (or a short-lived token from a broker Worker the user deploys), Docker Hub access tokens, Devin service-user keys, Slack and Discord bot tokens and webhook URLs, registry tokens |

- **W11-R03 Use in jobs.** `integrations: [aws-prod, anthropic]` on a job or step (kiln dialect,
  overlays), or `uses: kiln/auth@v1` with `integration:`. `oidc` modes need `id-token: write`.
  GitHub's own actions (`aws-actions/configure-aws-credentials`, `google-github-actions/auth`,
  `azure/login`, `hashicorp/vault-action`, …) work unchanged with kiln's issuer once the provider
  trusts it.
- **W11-R04 `kiln auth <integration> [--print-env | --exec CMD…]`** exists in every executor (the
  kiln binary is put into containers and microVMs as it is into pods today) and performs the
  exchanges using the leg's ID-token request variables.
- **W11-R05 Setup.** Each kind's page (in place, no dialogs) asks only for what it needs, then
  generates the provider-side trust configuration to paste or apply: the AWS OIDC provider and a
  role trust policy conditioned on `sub` (and `kiln_trust`), the `gcloud` commands for a workload
  identity pool and provider, the Azure federated credential, the Vault JWT role, the Tailscale
  trust credential with a custom issuer, the Anthropic, OpenAI and Mistral console settings, and
  so on. **Verify** runs a real exchange from a short verification leg and shows the result.
- **W11-R06 Chat and paging** (for W19): Slack (bot token or incoming webhook), Discord (bot or
  webhook), Microsoft Teams (workflow webhook), Telegram (bot), email (SMTP), generic webhooks
  (HMAC-signed), PagerDuty and Opsgenie (events APIs). The chats take commands too
  (docs/deploy-api.md §12.3): a kind's second secret (`signing`, set as `signing_secret` beside
  `secret`) is what its requests are checked with, a Slack app's signing secret or a Teams
  outgoing webhook's security token; Discord's `public_key` and Telegram's `commands` are config,
  and each of Discord and Telegram takes an `api` (a test's, a Bot API server of one's own).
- **W11-R07 Issue trackers** (opt-in rules): Linear and Jira (Atlassian) link runs and diffs to
  issues from trailers (`Fixes: LIN-123`, `Refs: ABC-42`), comment when a diff lands or a trunk run
  fails, and open an issue when a test is quarantined (W16).
- **W11-R08 Agent platforms and model providers.** kiln-managed agents (W20) get model access
  through federation: kiln exchanges its agent identity (W10-R11) with Anthropic, OpenAI, Mistral,
  Google (Vertex AI) and AWS (Bedrock) for short-lived tokens, instead of static API keys. Devin
  (Cognition) is offered as an external agent for attachments and experiments through its API with
  a service-user key.
- **W11-R09 Observability.** Datadog CI Visibility (runs, jobs, steps and tests sent to its API;
  API keys, or AWS-chained federation where Datadog accepts it), OpenTelemetry (OTLP traces of runs,
  jobs and steps; vendor-neutral), Grafana (kiln's dashboards already ship).
- **W11-R10 Security scanners.** Wiz (`wizcli` in pipelines; results as annotations), Snyk, and any
  SARIF producer (`kiln/sarif@v1`, and `github/codeql-action/upload-sarif` through the facade's
  code-scanning upload endpoint) feed annotations on runs and diffs.
- **W11-R11 Honest status.** Appendix C marks each kind `verified` only after W11-AC03 has passed
  against a real tenant, with the date. Kinds whose providers trust only their own allow-lists
  (npm, PyPI, crates.io, RubyGems and NuGet trusted publishing; Docker Hub's OIDC, which accepts
  GitHub Actions) use tokens, and the catalog says so.

**Acceptance criteria**

- **W11-AC01 Modes.** Unit tests per mode with a fake provider: files and variables written,
  refresh before expiry, no token in logs.
- **W11-AC02 Setup output.** For AWS, Google Cloud, Azure and Vault, the generated trust
  configuration, applied to a test tenant, makes Verify pass. *E2E, user-assisted once.*
- **W11-AC03 Catalog verification.** Each Appendix C entry gets one fixture workflow that uses it;
  when it passes against a real tenant the entry is marked `verified` with the date. *E2E,
  ongoing.*
- **W11-AC04 Notifications.** Slack, Discord, email and webhook deliveries with retries and HMAC
  signatures. *Integration test with local fakes, then E2E.*
- **W11-AC05 Model providers.** An agent kiln starts calls the Anthropic API with a federated
  token and no stored API key. *E2E, user-assisted.*

**Progress.** In progress. Ownership, agreed 2026-10-18: the Kiln CD lane builds the registry and
the exchanges (`crates/kiln/src/ci/integrations/`, each kind's `mint()`), on W10's
`workload_claims` and `ci::oidc::mint`; this lane wires them into jobs (`integrations:`, `kiln
auth`, W11-R03, R04). Done on this side: R03, R04.

- *R03* (`crates/kiln/src/runner/auth.rs`; the dispatcher's `leg_integrations`):
  - **What names them.** A job's `integrations:` (every step), a step's (that step), and
    `uses: kiln/auth@v1` with `integration:` (its step on).
  - **Server checks.** The server gives a leg only integrations whose scope holds its
    repository, and only to trusted runs. Federating kinds need `permissions: id-token: write`.
    Anything else fails the leg before it starts, with a message that names the integration.
  - **Federating kinds** get the provider's own files and variables, so SDKs and CLIs work
    unchanged:
    - `aws`: `AWS_ROLE_ARN`, `AWS_WEB_IDENTITY_TOKEN_FILE`, `AWS_ROLE_SESSION_NAME`, region;
    - `gcp`: an `external_account` file (`GOOGLE_APPLICATION_CREDENTIALS`, gcloud's override),
      written with the paths a job container sees, and the project;
    - `azure`: `AZURE_CLIENT_ID`, `AZURE_TENANT_ID`, `AZURE_FEDERATED_TOKEN_FILE`, and
      Terraform's `ARM_*`.

    Their ID tokens (mode 0600) are rewritten every four minutes while the leg runs, and masked.
  - **Stored tokens** become the variables their tools read: `CLOUDFLARE_API_TOKEN` and
    `CLOUDFLARE_ACCOUNT_ID`, `VERCEL_TOKEN`, Slack's, Discord's, PagerDuty's, `FLY_API_TOKEN`,
    `TF_TOKEN_<host>`, and so on. An OCI registry becomes a Docker config (`DOCKER_CONFIG`). All
    are masked.
  - **Precedence.** A job's integrations sit under its own `env`; a step's under the step's.
- *R04*: `kiln auth <integration> [--print-env | --exec CMD…]`.
  - It runs in any executor: the runner puts its own binary last on steps' `PATH`
    (`<work>/_externals/kiln`, `/__e/kiln` in containers, which need its libc).
  - It reads the leg's integrations from `$KILN_INTEGRATIONS`.
  - It exchanges the leg's ID token itself, for short-lived credentials: AWS keys
    (`AssumeRoleWithWebIdentity`), a Google access token (STS, then impersonation), an Azure one.
  - It prints `::add-mask::` for every secret before printing or running anything.
- *Verified end to end* on a local server:
  - a Cloudflare integration on a job: its variables set, its token masked, in `kiln auth
    --print-env` too, and in `--exec`'s environment;
  - on one step only: unset before and after it;
  - through `kiln/auth@v1`: set from its step on, with `kind` and `env` outputs;
  - an AWS integration: its role, region and session name, and a token file (0600) whose token
    has `aud: sts.amazonaws.com`;
  - a federating integration without `id-token: write`, and an unknown one, refused by name.
- *Done.* R06's channels are W19-R06's (see its notes): Slack, Discord, Teams, Telegram,
  PagerDuty, Opsgenie, signed webhooks and email (`email`, by SMTP). AC04 with local fakes is
  W19-AC05's test (`w19_ac05_notifications`).
- *Done.* R09's OpenTelemetry half is W19-R02's (see its notes): the `otel` kind, an OTLP/HTTP
  receiver every finished run's trace goes to.
- *Done.* R07, issue trackers (`crates/kiln/src/ci/trackers.rs`): the `linear` and `jira` kinds
  are channels for W19-R06's rules.
  - A run or diff names its issues in its commit's trailers (`Fixes: ENG-123`, `Refs: OPS-42`,
    or an issue's link). Each tracker comments on the issues that are its own (`prefixes`) and
    links the run or diff there, once per issue and link (a Linear attachment, a Jira remote
    link). An event that names none of a tracker's issues is a `skipped` delivery, with why.
  - Two events for them: `diff.landed` (a diff on its trunk, with its latest version's message)
    and `test.quarantined` (a test a trunk push added to `.kiln/quarantine.yaml`). A quarantined
    test gets an issue in the integration's team or project, or the rule's (`linear:ENG`,
    `jira:OPS`), or a comment on the issue its entry names.
  - Linear signs in with a personal API key, or an OAuth app's client credentials (the app
    actor's token, kept until near its expiry). Jira Cloud signs in with an API token and its
    account's email, or a service account's client credentials (Atlassian's token, then the
    site's cloud). Verify says who.
  - Each change is retried on its own, so a retry doesn't comment twice.
  - Checked against fakes of both: `w11_r07_issue_trackers` end to end (a failing trunk run,
    one naming nothing, two quarantined tests, a landed diff), and `w11_r07_trackers_as_apps`
    for the apps' and service accounts' tokens.

### W12 Registries

**Scope.** A built-in OCI registry and pull-through caches; credentials and caching proxies for the
package registries of the top ~25 languages and the top ~10 OCI registries; publishing helpers
(G-32).

**Requirements**

- **W12-R01 Built-in OCI registry**: the OCI Distribution Specification v1.1 (pull, push,
  chunked and monolithic uploads, cross-repository mounts, tags listing, the referrers API for
  signatures and SBOMs, deletion), blobs in the blob store (W08), manifests in SQLite, Docker token
  authentication (`/v2/token`) with PATs, job tokens (`registry: push` permission) and registry
  tokens, anonymous pull opt-in per namespace, multi-platform indexes, garbage collection. Served at
  `/v2/` on the main origin or on `[ci.registry] listen`.
- **W12-R02 Pull-through caches** (opt-in per upstream) for Docker Hub, GHCR, Quay, Google
  Artifact Registry and `gcr.io`, Amazon ECR Public, `mcr.microsoft.com`, Kubernetes' registry.
  kiln configures its runners to use them: the Docker daemon's `registry-mirrors`, containerd's
  `hosts.toml` for the cluster, and the registry settings shards reads; Firecracker root file
  systems are built from images pulled through kiln.
- **W12-R03 OCI credentials for jobs.** `registries: [ghcr, ecr-prod]` (kiln dialect, overlays) or
  `uses: kiln/auth@v1` writes a job-private `DOCKER_CONFIG` with short-lived credentials where the
  registry federates (ECR through AWS, Artifact Registry through Google Cloud, ACR through Entra,
  Quay robot federation, JFrog, Cloudsmith, Chainguard, Alibaba ACR through RAM, Oracle OCIR through
  OCI) and tokens otherwise (Docker Hub, GHCR, GitLab, DigitalOcean, Harbor robot accounts). kiln's
  own registry needs nothing: the job token works.
- **W12-R04 Package registries.** For each ecosystem of Appendix D, three opt-in parts:
  1. **credentials and configuration**: the files and variables its tools read
     (`.npmrc`, `pip.conf` and `UV_INDEX_URL`, `~/.cargo/config.toml` and
     `CARGO_REGISTRIES_*_TOKEN`, `GOPROXY`/`GOPRIVATE`/`.netrc`, Maven `settings.xml`, Gradle
     properties, `NuGet.Config`, Composer `auth.json`, RubyGems credentials, Hex, pub, Conan
     remotes, vcpkg binary sources, …);
  2. **a caching proxy** at `/_kiln/proxy/<ecosystem>/…` that speaks the ecosystem's read protocol
     (npm registry API, PyPI simple API (PEP 503 and 691), the crates.io sparse index, the Go module
     proxy protocol (with checksum-database pass-through), the Maven repository layout, NuGet v3,
     the RubyGems compact index, Packagist v2, Hex, pub, CRAN, CPAN, LuaRocks, Hackage, opam,
     Conan v2, Julia's package server, the Swift package registry, the CocoaPods CDN, Nimble,
     conda channels), with a generic HTTP caching proxy for the rest;
  3. **publishing**: `kiln/publish@v1` with the ecosystem's credentials.
- **W12-R05 Private registries** (Artifactory, Nexus, GitHub Packages, GitLab Packages, AWS
  CodeArtifact, Azure Artifacts, Google Artifact Registry, Cloudsmith) as proxy upstreams and
  publish targets, with credentials from W11.
- **W12-R06 Air-gapped installs.** Proxies and the registry serve what they have cached when
  upstreams are unreachable, and say so.
- **W12-R07 Events.** Pushes to kiln's registry emit `registry_package` (W03).

**Acceptance criteria**

- **W12-AC01 Conformance.** The OCI distribution-spec conformance suite passes against kiln's
  registry (pull, push, content discovery, content management). *Integration test.*
- **W12-AC02 Pull-through.** A second pull of an image from Docker Hub through kiln makes no request
  to Docker Hub for its blobs. *Integration test.*
- **W12-AC03 Proxies.** For npm, PyPI, crates.io, Go, Maven, NuGet and RubyGems, a fixture project
  installs through the proxy, then installs again with the upstream blocked. *E2E.*
- **W12-AC04 Credentials.** Jobs push to ECR, Artifact Registry and ACR with federated credentials,
  and to GHCR and Docker Hub with tokens, and the credentials are gone after the leg. *E2E against
  test tenants, user-assisted.*

**Progress.** In progress.

- *Done.* R01, kiln's OCI registry (`crates/kiln/src/ci/registry/`), at `/v2/` on the main origin
  and on `[ci.registry] listen`:
  - the Distribution Specification v1.1's pull, push (monolithic and chunked uploads, a chunk out
    of order refused, cross-repository mounts), content discovery (tags in pages, the referrers
    API and its `artifactType` filter, the catalog) and content management (deleting tags,
    manifests and blobs);
  - blobs in the content-addressed store (W08), so a layer kiln has in a cache, an artifact or
    another repository is stored once; manifests, tags and uploads in SQLite; a manifest whose
    blobs (an index: whose manifests) aren't there is refused;
  - Docker's token authentication (`/v2/token`): PATs, agents' and jobs' tokens (a job pushes where
    `packages: write` lets it, in its own repository's names); pull is `artifacts.read`, push and
    delete `artifacts.write`, where the name lives; anonymous pull opt-in by namespace;
  - an hourly collection: uploads left a day, blobs no manifest of their repository uses (the
    store then frees what nothing else uses).
  - Checked against the spec's four parts (`w12_ac01_conformance`), and with the OCI
    distribution-spec conformance suite itself, v1.1.1 (`w12_ac01_official_suite`, with
    `KILN_OCI_CONFORMANCE` naming its `go test -c` binary): 76 of its 80 specs run and pass; the
    other four are its alternatives for a registry set up beforehand, for automatic mounts, and for
    a mount that didn't happen (kiln's did).
- *Done.* R07: a tag pushed or moved is a `registry_package` event (`published`, `updated`) on the
  hub, and for the workflows of the repository the name is under, with the package in
  `github.event.registry_package` (`w12_r07_registry_package`).
- *Done.* R02, pull-through caches (`[[ci.registry.mirrors]]`), for any upstream: a manifest by
  digest, or a blob, comes from the upstream once and from kiln after; a tag is asked again after
  `tag_ttl`; under a prefix (`/v2/docker.io/library/alpine/…`, containerd's `hosts.toml` with
  `override_path`) and at the root of a mirror's own `listen` (Docker's `registry-mirrors`);
  anyone signed in pulls (anyone at all with `anonymous`), no one pushes. R06: an upstream out of
  reach is answered with kiln's copy and a `Warning` saying so. Checked against a fake upstream
  with Docker's token challenge and a redirect to storage (`w12_ac02_pull_through`): the second
  pull asks the upstream for no blob.
- *Done.* R04's caching proxies (`[[ci.registry.proxies]]`, `/_kiln/proxy/<name>/…`) for npm, PyPI
  (PEP 503 and 691), crates.io's sparse index, Go's module proxy and checksum database, Maven
  repositories, NuGet v3, RubyGems' compact index, and plain HTTP for the hosts named:
  - every answer kept in the content-addressed store: packages served from kiln once kept, an
    index asked again after `ttl`;
  - an index's links written over to point at the proxy (npm's tarballs, PyPI's files, NuGet's
    resources), crates.io's `config.json` kiln's own;
  - readers: anyone signed in (a PAT, an agent's or a job's token, as Bearer, Basic or bare, as
    Cargo sends it), anyone with `anonymous`;
  - R05's private registries as upstreams (`upstream`, `files`, with a token or a user and
    password);
  - R06: an upstream out of reach answered from kiln's copy, with a `Warning`.
  - Checked against a fake upstream for each (`w12_r04_proxies`), and AC03 with the real tools
    against the public registries (`w12_ac03_installs_through_proxies`, with
    `KILN_E2E_PROXIES=1`): npm, uv, cargo and go here, and Maven, RubyGems and NuGet (`mvn`,
    `gem`, `dotnet restore`) in containers, each installing a fixture through kiln and again with
    every upstream pointed nowhere.
- *Done.* R03, jobs' registry credentials: a leg's Docker config asks `docker-credential-kiln`
  (kiln's own binary, by that name, beside `kiln` on the leg's `PATH`) for each registry its `oci`
  integrations name and for kiln's own; the helper answers when Docker asks, with the job's token
  for kiln's registry, the integration's token for the rest, and short-lived credentials where
  the registry federates (`federation = "aws"`: an AWS role's, then ECR's token;
  `"gcp"`: Google's federated, or impersonated, token for Artifact Registry; `"azure"`: Entra's
  token exchanged at ACR for its refresh token). Nothing is written down, and an ID token can't be
  had after the leg. A leg on the host keeps the runner's own Docker logins; a job container never
  sees them. Checked in a leg (`w12_r03_job_credentials`); the federated exchanges need the
  clouds (AC04, user-assisted).
- *Done.* R04's first part: a `package` integration (an ecosystem, a registry's URL, a token) puts
  the credentials where the ecosystem's tools read them in a leg that names it: npm's userconfig
  and `NODE_AUTH_TOKEN`, twine's and uv's publishing variables and pip's extra index, Cargo's
  registry token (or a named registry's index and token), Go's netrc with `GOPRIVATE`, Maven's
  settings, NuGet's source credentials, RubyGems' and Bundler's, Composer's `COMPOSER_AUTH`. And
  a proxy with `jobs = true` is in every leg's settings (npm's, pip's and uv's, Go's, Maven's,
  RubyGems' and Bundler's, conda's), signed in with the job's token (`w12_r04_jobs_use_proxies`).
- *Done.* R04's third part, `kiln/publish@v1`: `npm publish`, `uv publish` (else twine),
  `cargo publish`, `dotnet nuget push`, `gem push`, `mvn deploy`, with a `package` integration's
  token handed over as each tool reads one, never on its command line; R05's private registries
  as publish targets (the integration's `registry`). Checked with npm against a fake registry
  that wants the token (`w12_r04_publish`).
- *Done.* More of Appendix D behind the proxies: JSR, conda channels, the Gradle Plugin Portal,
  Clojars, pub.dev, Hex, CRAN, CPAN, Hackage, LuaRocks, Julia's package server, opam, CocoaPods'
  CDN, and Zig's packages through the HTTP proxy.
- *Done.* R02's runners: images from the registries kiln mirrors (`container:`, `services:`,
  `docker://` actions) are pulled through kiln, signed in with the job's token, and tagged as
  asked; from their registry when kiln's mirror can't. One pinned to a digest
  (`image:tag@sha256:…`) runs as the mirror's reference: Docker refuses a tag with a digest, and
  finds the image by the digests it was pulled by, which are the mirror's (Home Assistant's
  `docker://hadolint/hadolint:v2.15.1@sha256:…`, corpus pass 1024b;
  `w12_r02_a_digest_pulled_through_kiln_runs_as_the_mirrors`).
- *Done.* More of D.2's federations through `docker-credential-kiln`: Quay's robot federation,
  JFrog's OIDC integration, Cloudsmith's OIDC, Chainguard's assumable identities.
- *Done.* D.2's last two (`crates/kiln/src/ci/integrations/{alibaba,oracle}.rs`), an `oci`
  integration's `federation`:
  - `alibaba`: STS's `AssumeRoleWithOIDC` with the leg's ID token (`role_arn`,
    `oidc_provider_arn`; audience `sts.aliyuncs.com` by default), then Container Registry's
    temporary password: an Enterprise Edition instance's (`instance_id`, RPC style) or the
    Personal Edition's (ROA style), each signed with the role's key and security token. The
    region is the registry host's unless `region` says.
  - `oracle`: the leg's ID token exchanged at an OCI identity domain (`domain`, `client_id`, the
    client secret as the integration's token) for a session token bound to a key made for the one
    login, then Container Registry's token for a request that key signs (OCI's HTTP signatures),
    logged in as `BEARER_TOKEN`. Without a federation, an auth token is the stored token, its
    `username` `<namespace>/<user>`.
  - Unit-tested: Alibaba's own RPC signature example; each exchange against a fake that checks
    what's sent and verifies its signature (a key other than the session's is refused).
- *Done.* The rest of D.1 (`crates/kiln/src/ci/registry/proxy.rs`):
  - Composer: Packagist's v2 metadata, the root's `metadata-url` and each version's archive
    (`dist.url`, on GitHub's, GitLab's or Bitbucket's hosts, or one the proxy names) pointed
    back at kiln, archives kept for good; the root's download counts (`notify-batch`) go nowhere.
  - A Swift Package Registry (SE-0292) the proxy names: its JSON, archives and manifests by the
    client's `Accept`, a release's archive and manifests kept for good, its links through kiln.
  - Conan v2 (ConanCenter by default): `v2/ping` says `revisions`; Conan's sign-in
    (`v2/users/authenticate`, Basic) gives back the kiln token it signed in with, which it then
    sends; a recipe's and a package's files by revision are kept for good, `latest` asked again.
  - vcpkg's binary cache (its `x-http` source): kiln keeps it. A person's token or a trusted run's
    job `PUT`s a package (an untrusted run's would poison every build after), and every build after
    gets it; a miss asks an upstream when the proxy names one, else is a 404.
  - Nimble's package list (nim-lang.org's), its packages through `http`.
  - Unit-tested (`w12_r04_immutables`, `w12_r04_composer_swift_conan_and_vcpkg`), and end to end
    against stand-ins for Packagist and ConanCenter (`w12_r04_composer_conan_and_vcpkg`).
- *Done.* R02 for BuildKit's builders (2026-10-24). A builder a leg makes through its Docker socket
  (buildx's `docker-container` driver, as `setup-buildx-action` makes it) resolves and pulls with
  BuildKit's own resolver, which reads neither Docker's `registry-mirrors` nor the runner's
  settings: in the conformance lab, buildx's builds failed on Docker Hub's `429 Too Many
  Requests`. Where kiln mirrors registries:
  - the builder reads kiln's config naming them, mounted at `/.kiln-buildkitd.toml` and given as
    its `--config`: `[registry."<upstream>"] mirrors = ["kiln.localhost:<port>/<prefix>"]`, and
    that host over plain HTTP;
  - it reaches kiln there through its loopback relay (W08-R08). BuildKit asks the mirror with the
    upstream's name in `ns` (`/v2/<prefix>/…?ns=docker.io`), and goes to the upstream itself when
    the mirror can't answer;
  - a workflow that configured BuildKit itself (buildx's `--config`), or a container whose
    entrypoint isn't BuildKit's, keeps its own;
  - kiln's registry tells what comes by the stand-in's name to get its token there too
    (`realm="http://kiln.localhost:<port>/v2/token"`), not at a public URL the builder may not
    reach or trust;
  - the builder signs in with no credentials of kiln's, so a mirror that isn't `anonymous` sends
    it to the upstream;
  - tests: `w12_r02_buildkit_containers_pull_through_kiln`,
    `w12_r02_the_stand_ins_challenge_names_the_stand_in`, and
    `w12_r02_a_buildkit_builder_pulls_through_kiln` with Docker, buildx, BuildKit's image and
    `KILN_EGRESS_BINARY`. In that test a builder buildx makes builds `FROM` an image that is only
    at a mirror on the runner's loopback.
- *Done.* R02 for a cluster's nodes: `GET /api/ci/registry/mirrors` (`ci.registry.mirrors`)
  says what a machine pulling through kiln is given: Docker's `registry-mirrors` (Docker Hub's
  mirror at its own `listen`) and, for each mirror, containerd's `certs.d/<upstream>/hosts.toml`
  (the upstream's address, then kiln's under the mirror's prefix with `override_path`). `kiln images
  mirrors --containerd /etc/containerd/certs.d [--ca <kiln's CA, from /ca.pem>]` writes them where
  containerd's registry `config_path` reads them; a mirror that isn't `anonymous` is said to ask
  for a sign-in, which a node's containerd doesn't give. Unit-tested (`w12_r02_what_a_node_pulls_through`).
- *Left.* shards' registry settings (with W13-R06); AC04 (user-assisted, against test tenants),
  which is also where D.2's federations meet the real registries.

### W13 Runners and executors

**Scope.** What runners report about themselves, every executor (host, Docker, Firecracker,
shards, Kubernetes), job containers and services, pools and autoscaling, assembling runners from
kiln, runner images (Dockerfiles and Agentfiles) built in kiln, and multi-OS and multi-arch
support (G-11, G-18, G-25, G-27, G-28).

**Requirements: facts**

- **W13-R01 Built-in probes**, run when the runner connects, every 10 minutes, and on
  `ServerMsg::ProbeFacts`, reported as `AgentMsg::Facts` (JSON, `CAP_FACTS`):

  | Fact | Source |
  | --- | --- |
  | `os`, `os.version`, `os.distro`, `kernel`, `arch`, `arch.variant` | uname, `/etc/os-release`, `sw_vers`, Windows version APIs |
  | `cpu.model`, `cpu.cores`, `cpu.threads`, `cpu.flags` (x86 feature flags; arm features) | `/proc/cpuinfo`, sysctl, Windows APIs (sysinfo) |
  | `mem.total`, `mem.free`, `disk.total`, `disk.free` (of the work directory's volume) | as today's host samples |
  | `gpu.count`, `gpu.model`, `gpu.memory`, `gpu.driver` | `nvidia-smi`, `rocm-smi`, Metal device names on macOS |
  | `virt.kvm`, `virt.hvf`, `virt.nested` | `/dev/kvm` access, Hypervisor.framework entitlement check |
  | `executors` | which of `host`, `docker`, `firecracker`, `shards`, `k8s` work right now (each probed for real: a no-op run), with versions (`docker.version`, `docker.rootless`, `podman.version`, `firecracker.version`, `shards.version`, `shards.features`) |
  | `binfmt` | platforms registered for QEMU user emulation |
  | `tools.<name>.version` | `git`, `node`, `python3`, `cargo`, `go`, `java`, `dotnet`, `docker buildx`, `shards`, … found on `PATH` |
  | `net.egress` | whether the internet is reachable directly or only through a proxy |

- **W13-R02 Declared facts.** Administrators add facts in the runner's configuration
  (`[agent.facts] location = "rack-2"`) and opt-in probe commands (`[agent.probes] cuda =
  "nvcc --version"`, output parsed as a version). Nothing else is guessed.

**Requirements: executors**

- **W13-R03 `host`**: today's executor (a process tree killed as a group; job objects on Windows),
  now behind W06's `Exec` trait. Runs `container:` jobs and services through the host's Docker or
  Podman when `CAP_DOCKER`, GitHub-exact for self-hosted runners (steps on the host, containers
  beside them).
- **W13-R04 `docker`**: the whole leg runs in a fresh container of the leg's image (the routing
  rule's `image`, W05-R21), the way a hosted runner gives each job a fresh machine. When the job
  declares `container:`, that container is the leg's container (GitHub-exact). Docker actions and
  services run beside it on a per-leg network. Docker access from inside the leg (for
  `docker build` steps) is opt-in per pool: the host's socket (trusted legs only) or an isolated
  daemon (Docker-in-Docker or sysbox). Docker and Podman (rootless included) both work.
- **W13-R05 `firecracker`**: one Firecracker microVM per leg on Linux hosts with KVM.
  - The root file system is built from the leg's OCI image (pulled through W12, flattened to an
    ext4 image, cached by digest).
  - `kiln guest` runs as PID 1 inside and speaks to the host runner over vsock: process start,
    output, signals, files and the terminal.
  - The workspace and caches live on a scratch block device.
  - Networking is a tap device whose only route out is the leg's egress proxy (W18).
  - The jailer confines each VMM.
  - Warm pools keep booted snapshots per image and restore one per leg.
  - Docker actions and services run when the guest image has a container runtime.
- **W13-R06 `shards`**: one shards microVM per leg, on the machines shards supports (x86_64 Linux
  with KVM, Apple silicon macOS; `shards.features` says what the installed version can do).
  shards is its own system and kiln integrates with it on its terms, **never as Docker**:
  - **Images.** A leg's image is a shards microVM image: built from an Agentfile with
    `shards build` (W13-R11), or an OCI image that shards pulls (`shards pull`) and boots as its
    own microVM. shards' images are OCI objects, so they live in kiln's registry (W12) like any
    other image. kiln does not convert Dockerfiles for shards or run shards images under Docker.
  - **Running.** kiln drives shards through its own interfaces: the `shards` command and its
    background service `shardsd`, and its Rust crates where they offer a library interface
    (kiln is Rust and both are hyper-light projects, so linking them is preferred to scraping
    output). A leg boots a fresh microVM, or restores a copy of a saved one, which shards does in
    microseconds. `shardsd`'s warm pool is used for fast starts. `kiln guest` inside the microVM
    talks to the host runner over shards' vsock.
  - **Grants.** shards denies networks, files and devices by default. The leg's microVM gets
    exactly what the leg's grants say: the egress policy of W18 becomes shards' network grants
    (its `NETWORK`, `EXPOSE` and `CONNECT` declarations, or the equivalent run-time grants), and
    the workspace and caches are the only volumes.
  - **Agents.** An Agentfile's `AGENT`, `HARNESS`, `SKILL`, `MCP`, `VOLUME`, `NETWORK`, `CONNECT`,
    `ATTACH` and `EXPOSE` directives declare agents and what they may reach. kiln uses them to run
    the agents it manages (W20: attachments, experiment variants, review agents) inside shards
    microVMs, many agents per microVM, each isolated as shards isolates them.
  - **Capabilities follow shards.** shards is pre-release (building microVMs from images,
    per-agent isolation, its Docker-style commands and GPUs are in progress or planned, by its
    README of 2026-09-29). kiln reads `shards.features` and routes only what the installed version
    supports; a leg that needs something shards lacks waits with a `capability` reason naming the
    feature. kiln never substitutes Docker for a missing shards feature or the other way round.
  - **Docker actions** do not run on `shards` (W07-R06).
  - **Services** run on `shards` only once shards offers several isolated members in one
    microVM, connected by declared networks. Until then, legs with `services:` route to other
    executors and say why.
- **W13-R07 `k8s`**: a pod per leg, as today (the init container copies `kiln` into the pod; the
  job container runs the leg's image; the PVC cache), extended so that `container:` is the job
  container, `services:` are native sidecar containers (they share `localhost`, and `hostAliases`
  make the service names resolve too), and Docker actions need an opt-in Docker-capable sidecar
  (rootless BuildKit or Podman); without it they are a plan-time error (`KCI4032`) naming the
  executors that can run them.
- **W13-R08 Job containers and services**, GitHub-exact (G-27): `container` and `services` with
  `image`, `credentials`, `env`, `ports`, `volumes`, `options`; services reachable by their ids
  from a job container, or on `localhost` with mapped ports from a job on the host; health checks
  (the image's `HEALTHCHECK`, or `options: --health-cmd …`) awaited before the first step; the
  `job.services.<id>.ports` context; teardown after the post steps.

**Requirements: pools, assembly, images**

- **W13-R09 Pools** (G-28): `[[ci.pools]]` in the server configuration or the Pools page:

  ```toml
  [[ci.pools]]
  name = "fc-linux"
  executor = "firecracker"            # docker | firecracker | shards | k8s
  hosts = "virt.kvm && disk.free >= 200Gi"   # which runners may host its legs
  image = "kiln/runner-ubuntu-24.04"
  labels = ["ubuntu-latest", "linux", "x64"]
  min_idle = 2                        # warm, ready legs' worth of capacity
  max = 64
  per_host_max = 16
  idle_timeout = "10m"
  resources = { cpu = "4", memory = "8Gi", disk = "40Gi" }
  ```

  The pool manager starts and stops ephemeral executors on the matching hosts when W05 asks
  (W05-R27). Each executor takes one leg and is discarded, so every leg starts clean. The `k8s`
  pool kind is today's pod-per-job path.
- **W13-R10 Assembling runners from kiln** (G-11). Runners → Add machines (today's join flow)
  grows, in the page:
  1. choose the executors to enable;
  2. set labels, slots or resources, pools, the work directory, cache sizes and egress policy;
  3. pick images for container and microVM executors;
  4. run `kiln runner setup <executor>` on the machine for the one-time privileged steps:
     - KVM group membership;
     - downloading and verifying Firecracker and its jailer;
     - installing shards;
     - tap networking.

     Each step says what it does and asks for `sudo` itself, never silently.

  The runner's page shows its facts, executors, health (disk low, Docker unhealthy, clock
  skew), pools and legs.
- **W13-R11 Runner images** (G-11): image definitions live in repositories
  (`.kiln/images/<name>/Dockerfile` or `.kiln/images/<name>/Agentfile`) or are made in the UI
  ("Save to repo" turns them into a change). The editor highlights and checks both (W21): for
  Dockerfiles kiln's own parser and lints; for Agentfiles shards' own parser and lints, through
  shards' `agentfile` crate. Images are built by `kiln/build-image@v1`: `docker buildx` for
  Dockerfiles (multi-platform), `shards build` for Agentfiles, each on a runner with that
  executor. Results go to kiln's registry (W12) and are registered (`ci_images`) with their
  platforms, ready for pools and routing rules. kiln ships definitions for
  `kiln/runner-ubuntu-24.04` (the commonly used tools of GitHub's Ubuntu image: build essentials,
  git, curl, jq, Node, Python, Go, Rust, Java, Docker CLI, …), a larger `-full` variant, today's
  toolchain images, and example Agentfiles.
- **W13-R12 Platforms** (G-18): host runners on Linux (x86_64, aarch64), macOS (arm64; kiln
  doesn't run on Intel Macs) and Windows (x86_64, arm64); Docker on Linux, macOS (Docker Desktop, Colima, Podman) and Windows
  with Linux containers; Firecracker on Linux with KVM; shards where shards runs, which gives Apple
  silicon Macs Linux arm64 microVMs; QEMU emulation as a declared fact (W04-R13).

**Interfaces.** Gives facts and pools to W05, executors to W06, terminals to W17, the egress
enforcement points to W18, agent isolation to W20.

**Acceptance criteria**

- **W13-AC01 Facts.** On each CI platform, the reported facts match ground truth gathered by an
  independent script; a declared fact and a probe appear; nothing undeclared appears. *Integration
  test.*
- **W13-AC02 Executor parity.** W06-AC01 (the toolkit conformance action) passes on `host`,
  `docker`, `firecracker`, `shards` and `k8s` wherever the CI fleet has them. *E2E.*
- **W13-AC03 Services.** A Postgres service with a health check, reached by name from a job
  container and by `localhost` from a host job, on `host`, `docker` and `k8s`. *E2E.*
- **W13-AC04 Pools.** A burst of 200 legs on a `docker` pool with `min_idle = 2`, `max = 32`:
  executors scale to 32, the queue drains, they scale back to 2 within `idle_timeout`. *E2E.*
- **W13-AC05 Firecracker.** Cold start of a leg ≤ 1.5 s and warm start from a snapshot ≤ 300 ms
  (p50, on the CI KVM host); the guest has no route out except the proxy. *E2E bench.*
- **W13-AC06 shards.** On a machine with shards: a leg runs in a shards microVM built from an
  Agentfile in kiln's registry; its microVM gets no network but what its grants allow (an
  unlisted host is unreachable); a restore-from-snapshot start is used when shards offers it; a leg
  needing a feature the installed shards lacks waits with a `capability` reason naming it; at no
  point is Docker involved (no Docker process or socket is touched during the leg). *E2E on a
  shards-capable runner.*
- **W13-AC07 Images.** A Dockerfile and an Agentfile edited in the UI build on their executors, are
  pushed, appear in Images with their platforms, and a pool using each runs a leg. *E2E + UI
  check.*
- **W13-AC08 Assembly.** From a fresh Linux VM, the Add machines flow with Docker and Firecracker
  enabled ends with a runner whose facts and executors are correct, without editing files by hand.
  *E2E, user-assisted for the VM.*

**Progress.** In progress.
- **Facts (R01, R02, AC01).** A runner probes when it connects, every 10 minutes and on
  `ServerMsg::ProbeFacts`, one probe at a time. It sends all of `RunnerFacts`:
  - OS, distribution, kernel, architecture;
  - CPU model, cores, threads and feature flags (as the standard library detects them);
  - memory, and the work directory's own volume (`statvfs`, a `tmpfs` included);
  - GPUs (`nvidia-smi`, `system_profiler`);
  - KVM, nesting and Hypervisor.framework, and QEMU's registered platforms;
  - the executors it can run (host; Docker or Podman with its version and whether it's
    rootless), tools' versions, and whether the internet is reachable directly or through which
    proxy.

  `[agent.facts]` and `[agent.probes]` (the server's `kiln.toml` for its own runner, a `kiln
  agent`'s config file) add `custom.*`; nothing else is guessed. The server keeps the last report,
  routes by it (`requires: ["custom.location == rack-2", "gpu.count >= 1"]`, never an executor the
  build didn't announce), and shows it on the runner's page.

  AC01 is an integration test that checks the probe against the machine's own answers:
  `/proc/meminfo`, `uname -r`, `/etc/os-release`, `df`, `/proc/cpuinfo`, `git --version` and
  `docker version`. Exactly the declared fact and the answering probe appear.

  Firecracker and shards are reported as tools until the runner can run legs on them.

  On request, too: `POST /api/agents/:name/probe` (`ci.runners.probe`, `kiln runners probe
  <name>`) has the runner probe again now and answers with its new report once it comes (202,
  without one, after 30 s); `kiln runners facts <name>` shows the last.
- **Executor parity (AC02), on the executors this fleet has.** A workflow on a local server runs
  F-20 from the repository on the host and in the `docker` executor (`ubuntu-latest`, kiln's
  runner image). Every effect is the same in both:
  - inputs of each kind; outputs, `exportVariable` and `addPath` in a later step;
  - the secret masked;
  - `exec` and `io`;
  - annotations with their properties, and the summary;
  - the post step's state;
  - `getIDToken` from kiln's own issuer, for its audience and the repository's subject, masked.

  In a Firecracker microVM of `alpine:3.20` too, its JavaScript on the musl Node W07-R05 fetches
  (`w13_ac02_toolkit_conformance_in_a_microvm`): every effect above, `getIDToken` from the
  server's issuer through the guest's way out. shards and `k8s` follow when theirs do.
- *Done.* AC03 (`tests/services.rs`): a Postgres service with a health check, the leg's steps
  starting once it's healthy, reached at `localhost` on its published port from a job on the
  host, and by its name from a job container and from a leg in the `docker` executor. A `k8s`
  pod's service is a sidecar `hostAliases` names (tests/k8s_pool.rs), and the live cluster's is
  W22-AC05's.
- *Done.* AC05, measured by `kiln ci bench --only firecracker` (alpine's microVM, each timed from
  its start to its guest's word that it's ready, its image and template made first). On this
  host, a debug build (2026-10-21): a cold start's p50 638.7 ms (≤ 1.5 s) and a warm start's,
  restored from the image's snapshot, 34.9 ms (≤ 300 ms). The guest's only way out is the egress
  proxy (W13-R05).

- *Done.* `crates/kiln/src/runner/docker.rs`, on the host executor through Docker or Podman
  (R03, R08), as GitHub's self-hosted runner does it:
  - job containers with GitHub's mounts, steps run through `docker exec` with paths
    translated;
  - services on a per-leg network, health checks awaited, ports published, with the
    `job.services` context;
  - a service that isn't healthy shown as GitHub's runner shows one ("Service container <name>
    failed." and its container's log, both streams, its last 400 lines), so the leg says why:
    laravel's MySQL said only "mysql is unhealthy" (corpus pass 1024b, two legs at once on the
    lab's host network, both publishing 3306; `w13_r08_an_unhealthy_service_shows_its_log`);
  - a step in a job container whose `working-directory` is the container's own path (the
    `docker` executor's `${{ runner.temp }}`, `/home/runner/work/_temp`) is made where its mount
    is on the host, and one only the container has is left to it, where kiln had tried to make it
    on the host and been refused (Home Assistant's pylint step, corpus pass 1024b;
    `w13_r08_a_job_containers_own_path_as_working_directory`);
  - teardown;
  - Docker actions (W07-R06);
  - `CAP_DOCKER` announced when a CLI answers. One probe runs at a time, waited for 10 s from
    its start. A daemon too busy to answer by then (a loaded machine) is asked again at the
    runner's next connection, rather than taken for absent until a restart.
- *Done.* R04's `docker` executor:
  - a leg runs in a fresh container of its routing rule's image, without the host's Docker
    socket;
  - GitHub's Ubuntu labels prefer it where the runner has Docker, and run on the host otherwise;
  - `kiln/runner-ubuntu-24.04` and `-22.04` are built on the runner the first time a leg needs
    one, from a Dockerfile kiln carries. Each build is tagged by its Dockerfile's digest, so a
    changed one is built again;
  - laid out as GitHub's hosted runners are:
    - the tool cache at `/opt/hostedtoolcache`, kept per image on the host;
    - `ImageOS`;
    - steps run as `runner`, a user with the host's uid and gid and passwordless sudo (root on
      a rootless engine), so what they write stays the host user's;
  - container and network names carry the whole leg id and a suffix of their own;
  - `ubuntu-latest` legs ran `setup-python`, `setup-ruby`, `setup-beam`, `setup-php`,
    `setup-r`, `flutter-action` and the rest of W22-AC02's second batch in it end to end.
- *Done.* The image has the toolchains GitHub's hosted runners have and workflows call
  without a `setup-*` action:
  - CMake, Ninja, the autotools and Clang;
  - a JDK (`JAVA_HOME`) with Maven, Ant and sbt;
  - PHP with Composer;
  - Ruby with Bundler;
  - Leiningen, pipx, Rust (rustup, rustfmt, clippy) and Node.js 22;
  - conda at `$CONDA` (`/usr/share/miniconda`, where GitHub's Miniconda is), from conda-forge's
    Miniforge (pinned, its checksum checked), so the image needs no Anaconda license; its base
    environment is the `runner` user's, as the Conda starter needs;
  - PowerShell 7.6 (`pwsh`) and the GitHub CLI (`gh`), pinned and checked against their
    releases' sums;
  - the command-line tools of GitHub's image that scripts call: `python` (Python 3), fakeroot
    (bat's Debian packages need it), Bison and Flex, patchelf, ShellCheck, the SQLite shell,
    tree, lz4, pigz, 7-Zip, GNU parallel, Brotli, zsync, aria2, sshpass, dig, ping, netstat,
    telnet, ftp, the ACL tools, SWIG, Texinfo, rpm, UPX, MediaInfo, Mercurial and Xvfb;
  - git from the git-core PPA, current as GitHub's is (2.55 when added; Ubuntu 24.04's own is
    2.43). Tools go by its version: cargo retries a failed `git fetch` itself only below 2.54,
    and its ssh and https tests count the retries;
  - a `snap` that does what a container can: snapd needs a machine of its own, so
    `snap install NAME` installs Ubuntu's package of each name, as `taiki-e/install-action`'s
    Valgrind asks for (tokio's stress tests), and anything else says why it can't.

  It comes for Ubuntu 26.04 too (`kiln/runner-ubuntu-26.04`, `ImageOS=ubuntu26`, from
  `ubuntu-26.04` and `ubuntu-26.04-arm`; four of the corpus's repositories ask for it), and
  `ubuntu-slim`, GitHub's lightweight runner, gets the full `kiln/runner-ubuntu-24.04`: GitHub's
  smaller set still has Node.js and npm, the Docker CLI, gh and PowerShell, which kiln's `-slim`
  leaves out (TypeScript's format and tidy jobs run on it). On Ubuntu
  24.04 and 26.04, `/etc/pip.conf` lets pip install into the system Python, as GitHub's images'
  does. The `runner` user's `~/.local/bin`, where `pip install` puts scripts, is on `PATH`.

  apt assumes yes, as GitHub's images set it. Cargo's and npm's global directories are the
  `runner` user's, so `cargo install` and `npm install -g` work. `kiln/runner-ubuntu-24.04-slim`
  is the basics alone. A new build removes its user's older ones.
- *Done.* Podman, Buildah and Skopeo in the image, as GitHub's have them. Their containers can't
  run in a container executor's container, so a leg whose `run:` steps call `podman` or
  `buildah` (at a command's start, past `sudo` and `NAME=value`, not in a comment:
  `runs_containers`) leaves the `docker` and `k8s` executors for the others its rules allow, else
  a Firecracker microVM; a rule's `match.nests_containers` sends such legs where it says, as the
  corpus lab's sends them to microVMs, the user's choice (rails' `rails-new-docker`;
  `w13_a_leg_that_runs_podman_leaves_container_executors`). `POST /api/ci/routing/explain` takes
  `nests_containers` too. Corpus pass 1024b ran `rails-new-docker` on both its events in
  Firecracker microVMs, and both passed.
- *Done.* kiln's runner images in Firecracker microVMs, and Podman in them. No registry has
  `kiln/runner-*`, so a microVM leg of one builds it with Docker as the `docker` executor does
  (once per version), saves it (`docker save`, unpacked as it streams), and builds its layers into
  the microVM's ext4 as a pulled image's are, kept by Docker's id for the image: gzip, as Docker's
  containerd image store keeps them, or plain tar (zstd is refused)
  (`w13_saved_images_read_in_order`). Podman's storage there is on the scratch disk, and its
  containers share the guest's own network, the only one it has (out through the egress
  proxy), with cgroupfs and file events. Checked with `kiln workflow run --executor firecracker`
  (Firecracker 1.17, its 6.1 kernel, Docker 29 with the containerd image store): Podman 4.9.3
  built an image whose `RUN apk add` reached the internet through the proxy, ran it, and served
  nginx on its port. On the guest's network a published port is the container's own
  (`-p 3000:3000`, as rails' test has it); one mapped to another (`-p 8080:80`) stays the
  container's.
- *Done.* `kiln/runner-ubuntu-24.04-android` (and `-26.04`, `-22.04`): the full image and
  Android's SDK where GitHub's images have it (`ANDROID_HOME` and `ANDROID_SDK_ROOT`
  `/usr/local/lib/android/sdk`; the command-line tools, latest, checked against the SHA-1 Google's
  repository lists; NDK 27.3.13750724 and the platform tools through `sdkmanager`, which checks
  them against it; `ANDROID_NDK`, `ANDROID_NDK_HOME`, `ANDROID_NDK_ROOT` and
  `ANDROID_NDK_LATEST_HOME` set as GitHub's images set them; the SDK the `runner` user's). Only a
  routing rule that names it chooses it: building it accepts the Android Software Development Kit
  License Agreement (developer.android.com/studio/terms) that `sdkmanager --licenses` asks for,
  so the administrator who names it accepts it, and the image is labelled so
  (`kiln.android-sdk-license=accepted`). There's no slim one (sdkmanager needs the JDK)
  (`runner_images_have_a_dockerfile`). Built on adacachy (2026-10-25), the image has NDK 27's
  LLVM toolchain at `$ANDROID_HOME/ndk/27.3.13750724/toolchains/llvm`
  (`w13_r04_the_android_image_has_ndk_27s_toolchain`, which runs a leg in it once it's built). uv's
  Android job (`build-binary-android-aarch64`), routed to it by a rule that names its job, passed
  with `kiln workflow run`: the NDK's clang linked uv and uvx for `aarch64-linux-android` and the
  artifact uploaded. The corpus routes its Android jobs (uv's, cpython's, bevy's) to the image
  (`ANDROID` in `ci/conformance/corpus.py`).
- *Done.* The leg's machine more as a hosted runner's is (corpus pass 1024b, 2026-10-24):
  - `/tmp` is the machine's own, on the container's file system with its home, as a hosted
    runner's machine has one disk (2026-10-25): a rename from it into the home directory works
    (jetli/wasm-pack-action's of wasm-pack into `~/.cargo/bin`). A `/tmp` volume of its own, as
    kiln gave it on 2026-10-24 for fanotify's watchers, refused that rename across mounts (EXDEV,
    ruff's `cargo-test-wasm`, corpus pass 1025a): such renames are common in setup steps, file
    handles rare in tests. *Left*: the container's overlay makes no file handles, so fanotify's
    watchers fail in it (TypeScript's `fswatch` tests, 126 of them, corpus pass 1024b) where a
    hosted runner's ext4 has them; a microVM leg (W13-R05) has an ext4 of its own;
  - `/opt` is open to all, as GitHub's images leave it (`chmod -R 777 /opt`): steps make their own
    directories there without sudo (neovim's `old-cmake` job);
  - the runner user's `~/.cargo/bin`, `~/.config/composer/vendor/bin` and `~/.dotnet/tools` are
    on `PATH`, as GitHub's are: tools put themselves there without asking (jetli/wasm-pack-action's
    wasm-pack, for ruff's build-wasm), where kiln's Rust is in `/usr/local/cargo`;
  - the full image has the Node.js tools GitHub's install globally (their toolset's
    `node_modules`: Grunt, Gulp, n, Parcel, TypeScript, Newman, webpack and its CLI, Lerna and
    Yarn 1), so `yarn` is on `PATH` whichever Node setup-node puts first (react's
    `yarn install`);
  - in resource mode the job container's memory limit is what the leg was given (W05-R03), swap
    none beyond it, so what sizes itself by its cgroup fits the leg: CodeQL's analysis asked for
    the machine's 118 GB and was killed (neovim's `codeql.yml`);
  - the full image has the libraries a browser or an Electron app loads (GTK 3, ATK, NSS, ALSA,
    GBM and the rest), as GitHub's has them with its Chrome: VS Code's test runner needs them
    (rust-analyzer's editor tests), as do a Chromium a test downloads and winit's X11 backend
    (bevy's examples under Xvfb failed with `XNotSupport`, no libXcursor or libXi). And what GitHub's
    machines have from their apt lists or Ubuntu's server image that kiln's lacked: strace
    (tauri's benchmarks trace their apps), GNU time, lsof, psmisc, ICU's headers, NSS's tools,
    D-Bus, Tk and Noto's emoji font;
  - the full image has the rest of the release's LLVM beside its Clang, as GitHub's images have
    it: LLVM's tools (cpython's JIT build calls `llvm-objdump-21`), clang-format, clang-tidy, and
    the sanitizers' runtimes and headers (cpython's TSan build stopped on
    `sanitizer/tsan_interface.h`, and its UBSan and MSan configure found no function, every link
    failing). 22.04's Clang has the runtimes already. And MySQL's and PostgreSQL's client
    libraries and headers, as GitHub's images have them with their servers: rails' `bundle
    install` builds the `mysql2` and `pg` gems against them;
  - apt's package lists are left in place, as GitHub's images leave them (their cleanup runs
    `apt-get clean` alone), so `apt-get install` and `apt-get remove` know every package without an
    `apt-get update` first: deno's sysroot step removes man-db, which kiln's image hasn't, and
    stopped on apt's "Unable to locate package". The lists are the image build's;
  - apt reads its mirrors from `/etc/apt/apt-mirrors.txt` (`mirror+file:`), as on GitHub's images,
    where workflows read and change the list (ruff's and uv's benchmark jobs); the list holds the
    image's own archive (Ubuntu's, its ports' on arm64), where GitHub's has Azure's first;
  - the full image's `sysctl` does what a container can, as its `snap` does: a container's
    `/proc/sys` is read-only, so a write of what a setting is already succeeds, as `sudo sysctl
    -w` does on a hosted runner's machine, and the rest goes to the real one, which refuses it.
    cpython's and arrow's sanitizer jobs ask for `vm.mmap_rnd_bits=28` (TSan and MSan need
    Ubuntu 24.04's ASLR narrowed), which this machine has already;
  - the full image keeps the Pythons GitHub's images have in their tool cache (their toolsets'
    `toolcache`: the newest 3.11 to 3.15 built for the release, from actions/python-versions as
    setup-python installs them), and a leg's tool cache gets each it hasn't as the leg starts,
    whole, so setup-python resolves a range as it does on GitHub (`3.x` is 3.15): with only
    another leg's 3.11 in the cache, cpython's Emscripten build got a Python without 3.12's
    `delete_on_close`. Left: the rest of GitHub's tool cache (PyPy, Node.js, Go, Ruby, CodeQL),
    whose setup actions download the version asked, so only an open range can resolve otherwise.

  Checked live with Docker (`w13_r04_the_machines_tmp_and_memory`), and by
  `runner_images_have_a_dockerfile`, `w13_r04_sysctl_does_what_a_container_can`,
  `w13_r04_a_legs_tool_cache_gets_the_images_tools` and
  `w05_r03_a_legs_memory_is_its_executors_limit`.
- *Done.* R04's opt-in Docker access: a routing rule's `docker: socket` gives a trusted run's
  `docker` legs the host's Docker socket, reached through the socket's group. The image has the
  Docker CLI with buildx and compose. The leg's machine is a container the daemon doesn't look
  into, so these legs get a socket of their own in the host's place (`runner/docker_sock.rs`). It
  passes every request on and translates the bind mounts of the containers a step creates:
  - a path the leg has from the host (its workspace, temporary and action directories) goes to
    the host's path for it;
  - the kernel's (`/proc`, `/sys`, `/dev`, `/lib/modules`) stay the host's;
  - everything else goes to the leg container's own root as the host has it mounted: the graph
    driver's merged directory, or with containerd's image store, `<root dir>/rootfs/<driver>/<id>`.

  So `docker run -v ~/.cargo:/cargo` mounts the leg's own `~/.cargo`, as on a hosted runner, and
  `cross` runs in its own mode, mounting the image's Rust toolchain:
  `w13_r04_cross_in_a_docker_leg` runs ripgrep's `cross test --manifest-path … -- --nocapture`.
  Before, `cross` ran in its remote mode (`CROSS_REMOTE=1`, which copies into volumes). That mode
  puts `--target-dir` after a `--`, so cargo made `/target` and was denied, and it lost its own
  named container from one step to the next ("is not running"): the corpus's ripgrep and bat.
  Remote mode is left only for a runner that can't listen beside the leg's directory. The socket
  is for fidelity, not containment: a leg given the host's Docker is trusted. For the same reason
  these legs' containers have no seccomp filter, as a hosted runner's machine has none: with the
  host's Docker they could start any container they like, and the filter kept debuggers from
  turning ASLR off (`personality`, which lldb needs to launch a program: cargo's `lldb` tests).

  The daemon is still the host's, one for every leg on that runner, so the socket gives each leg
  names of its own. A hosted runner gives each job a machine and a daemon of its own.
  - What a step makes through the socket gets a suffix of the leg's own on the daemon (`-k` and
    8 hex digits from the leg's id): a container (`--name`), a network, or a volume (made, or
    named by a container's mount, which makes it on a machine of its own).
  - The leg's later requests by that name are taken to it: paths, `rename`, `commit`, a create's
    network, links, `--volumes-from`, `--pid` and `--ipc container:`, a network's connect.
  - A container is found on its networks by the name it was given (an alias).
  - The daemon's answers say the names as the leg gave them. Its lists and events leave out what
    other legs made, and a prune takes only the leg's own.
  - Ids, and names the leg didn't make (kiln's for its services, what was there before), go on
    as they came.

  So two legs at once can both make cargo's `cargo_test_0`, or the same compose project, as two
  hosted jobs can (`w13_r04_two_legs_make_the_same_names`). Before, they collided. What a leg
  made carries the labels `kiln.socket.leg.<tag>` and `kiln.socket.runner.<hash>`, keys of their
  own (a leg's socket within another's, kiln's tests in a kiln leg, keeps both). It goes when the leg ends,
  as the end of its machine would take it, and in the sweep when a runner that stopped short
  starts again. The leg's containers' anonymous volumes go with them (kiln's own services' left
  theirs before). Images and the build cache stay the daemon's, every leg's cache.
- *Done.* In kiln's runner image, a `docker` leg has GitHub's hosted runners' paths:
  `/home/runner/work/<repo>/<repo>` for the workspace, `/home/runner/work/_temp` and
  `/home/runner/work/_actions` beside it, the event file and the file commands under `_temp`, and
  `/home/runner/work` the `runner` user's. The leg's own socket finds what
  `docker run -v $GITHUB_WORKSPACE:/src` mounts wherever it is, so the host's paths aren't needed.
  Those were long enough on some hosts that the Unix sockets tests make in a workspace didn't fit
  (cargo's `ssh-agent` tests, 108 bytes at most). Every such leg has its workspace at the same
  path, so a cache saved relative to it restores in any other
  (`w13_r04_hosted_legs_have_githubs_paths`). A pool's executor, another image, or a runner that
  can't listen beside the leg's directory keeps the host's paths.
- *Done.* What a runner leaves behind goes:
  - A runner's legs' networks and containers and its pools' executors carry its label. The label
    comes from its work directory, so each runner on an engine has its own.
  - A runner killed mid-leg, or crashed, leaves them running. When it next starts, it removes
    everything with its label before it takes a leg, and nothing of another runner's.
  - A start that fails partway removes what it made (a leg's network, a created container).
  - Left behind, a leg's network holds one of the engine's few address pools until no leg on
    the machine can start ("all predefined address pools have been fully subnetted").
  - The engine gets ten minutes, not one, to create or start a container: a busy machine's
    Docker takes minutes, as GitHub's runner allows.
  - Checked: `crates/kiln/tests/leftovers.rs` kills a server mid-leg and starts it again. The
    leg's container and network go, and another runner's network stays. A unit test fails a
    start on a service image there is no pulling.
- *Done.* `KILN_CONTAINER_NETWORK=host` on a runner puts a leg's job container, services and
  Docker actions on the host's network. That is for machines whose firewall drops container
  traffic to the host, such as one of the fleet's, where a container could not reach a server
  on the same machine. Services then resolve by name to 127.0.0.1, and `job.services.<id>.ports`
  are their own.
- *Done.* R11's builds: `kiln/build-image@v1` (`name`, and optionally `context`, `file`,
  `platforms`, `tag`) builds `.kiln/images/<name>/`:
  - a `Dockerfile` with `docker buildx build --push` (multi-platform where the builder can);
  - an `Agentfile` with `shards build`, then `shards push`, on a runner with shards, never with
    Docker (a runner without shards says so).

  Both push to kiln's registry, as `<repo>/images/<name>:<tag>` (the commit's first twelve
  characters by default) and `:latest`, signed in with the job's token through kiln's credential
  helper (`packages: write`). The registry takes OAuth2's form of the token request too, which
  containerd and BuildKit try first. The runner reports the image over its own channel
  (`JobEventV2::ImageBuilt`). The server registers it (`ci_images`) only as its registry holds
  it: under the leg's own repository, the digest one of its manifests there, and the platforms
  read from that manifest (an index's members, else its config's). `GET /api/ci/images?repo=&name=`
  lists them, newest first. The step's outputs are `image` (by digest) and `digest`.
  `w13_r11_build_image` builds a Dockerfile this way and checks the registration against the
  registry. `w13_r11_build_agentfile` does the same for an Agentfile where shards is installed.
  It passed on an Apple silicon Mac with shards 0.1.0: a Linux arm64 microVM image, pushed by
  shards with kiln's helper, and no Docker in the leg's log.
- *Done.* R09's pools, of the `docker` kind. They come from `[[ci.pools]]` in the server
  configuration, or are stored with `PUT /api/ci/pools/:name` (`pools.manage`); a stored pool
  can't take a configured one's name. Each sets `executor`, `hosts` (a constraint over runners'
  facts, as routing rules write them), `image`, `labels`, `min_idle`, `max`, `per_host_max`,
  `idle_timeout` and `resources`. A leg whose `runs-on` labels are all one pool's runs in one of
  its executors (W05-R27 scales them):
  - A runner that announces `CAP_POOLS` starts a `docker` executor when the server asks
    (`ServerMsg::Executor`): a started container of the pool's image, laid out as the `docker`
    executor lays out a leg's, under a directory of its own (`<work>/p/<id>`), with its own lease
    of the image's tools. It says when the executor is ready or why it failed
    (`AgentMsg::Executor`).
  - An image in kiln's registry is pulled with a short-lived token for that image alone.
  - A pool's leg runs at once in the executor the scheduler gave it, and doesn't count against the
    runner's own slots. The executor goes with the leg, so every leg starts clean.
  - A leg the executor can't take as it is (`container:`, `services:`, a contained network) runs
    the usual way, and the executor is discarded all the same.
  - A runner's executors don't outlive its connection: on a reconnect it discards them, and the
    server starts what it wants again.

  `GET /api/ci/pools` and `/:name` show each pool's settings, its executors (state, host,
  since), the legs waiting for it and its last failure to start one. `w13_ac04_docker_pool_burst`
  is AC04 on this machine's Docker: 200 legs on a pool with `min_idle = 2` and `max = 32`; the
  executors reach 32, every leg passes in an executor of its own, and the pool is back to 2 within
  `idle_timeout`.
- *Done.* R11's checks and "Save to repo" (`crates/kiln-ci/src/dockerfile.rs`,
  `crates/kiln/src/ci/images.rs`):
  - kiln's own Dockerfile parser: instructions with continuations (either escape character),
    comment lines inside them, heredocs and parser directives. Its structure errors are `KDF1xxx`
    (an unknown instruction with "did you mean", one before FROM, a continuation into nothing,
    an exec form that isn't JSON, an unterminated heredoc).
  - Lints under hadolint's codes: untagged or `latest` bases (DL3006, DL3007), `apt-get`
    without `-y` or `--no-install-recommends` and with its lists left (DL3014, DL3015, DL3009),
    `apt` (DL3027), `cd` and `sudo` in RUN (DL3003, DL3004), `pip` without `--no-cache-dir`
    unless a cache mount (DL3042), ADD for local files (DL3020), COPY's destination (DL3021),
    relative WORKDIR (DL3000), ports (DL3011), duplicate stages (DL3024), a second CMD,
    ENTRYPOINT or HEALTHCHECK (DL4003, DL4004, DL3012), shell-form CMD and ENTRYPOINT (DL3025),
    MAINTAINER (DL4000), root as the last user (DL3002).
  - `POST /api/ci/images/validate` and `kiln.ci.images.validate` answer with the diagnostics.
    `POST /api/ci/images?repo=` and `kiln.ci.images.propose` propose a definition as a diff
    adding `.kiln/images/<name>/Dockerfile` (or `Agentfile`), refusing a Dockerfile with errors.
  - Unit-tested (`w13_r11_*`).
- *Done.* The Runners page's pools and images (`ui/src/views/runners-pools.tsx`, W21-R08):
  - Pools, for those who read the whole fleet: each one's executor, its executors now (busy,
    idle, starting), the size it's heading for, the legs waiting and its labels. One opened
    shows its limits and image, each executor (on which runner, running which leg, since when)
    and its last failure to start. A stored pool is added, edited in place and removed by those
    with `pools.manage`, and a save says what it does (W20-R03).
  - Images: those kiln built, each one's platforms and digest, and when, linking the run that
    built it.
  - The runners table says what each runner can run legs in (its executors, as it last probed).
  - Checked in Chromium and WebKit, desktop and phone: a pool added, heading for its minimum,
    warned of as no runner can start it, edited in place and removed; a runner's executors.
- *Done.* R09's `k8s` kind, R07's pod per leg (`crates/kiln/src/k8s.rs`, `ci/pools.rs`, the
  scheduler's hosted executors in `crates/kiln-ci/src/sched/state.rs`). A `k8s` pool's executors
  are the server's, not a host runner's:
  - Each is a pod the server starts in its `[kubernetes]` namespace: its job container the
    pool's image (else the toolchain's) running `kiln agent --ephemeral`, copied in by the init
    container as a job's pod has it, with the pool's `resources` as its requests (memory its
    limit too) and the cache claim mounted.
  - The pod's runner enrolls with a one-time token bound to its executor and its name (it can't
    take another), and connects bound to that executor: it takes that pool's one leg, no other
    leg, and leaves. No other runner takes the pool's legs, whatever its labels. Kubernetes places
    the pods, so `hosts` and `per_host_max` don't apply; `min_idle`, `max` and `idle_timeout` do.
  - A pod that can't start (its image can't be pulled, its container made) for two minutes is
    deleted and its executor failed with why, the pool backing off as from a failing host. One
    that ended, or whose executor the scheduler hasn't got (after a restart), goes too.
  - Another ephemeral runner (a v1 job's pod) takes none of the engine's legs.
  - A leg with `container:` or `services:` gets a pod made for it, within `max`, and only it:
    - its `container:` is the pod's job container (its image, else the pool's, and its
      environment), evaluated by the server with the run's `github`, `vars`, `needs`,
      `strategy`, `matrix` and `inputs`, as the runner evaluates it;
    - each of its `services:` is a native sidecar (an init container that keeps running) with its
      ports, on the pod's `localhost`, which `hostAliases` name it by too;
    - the runner inside starts no containers: it runs the steps where it is (the job container),
      awaits each service on its ports, and gives `job.services.<id>.ports` as the pod has them;
    - `credentials` aren't how a pod pulls (its namespace's image pull secrets are): a leg with
      them fails, saying so, as does one whose keys don't evaluate;
    - cancelled while its pod starts, the pod goes.
  - Docker actions (`docker://` images, actions that run in Docker, and composite actions with
    one beneath them) run in a pool's pods when the pool opts into a Docker sidecar (`docker` in
    its `[[ci.pools]]`: `engine`, `podman` by default or `docker` for rootless Docker; `image`;
    `cli_image`, `docker:cli` by default):
    - an init container copies the Docker CLI in beside `kiln`, and the engine is a privileged
      native sidecar whose startup probe waits for its socket, on a volume the job container
      shares (`DOCKER_HOST`);
    - the work directory is at the same path in both, so what a step bind-mounts is there, and
      the engine's images and containers are on a volume of their own;
    - without one, a leg with a Docker step on that pool fails as it's queued with KCI4032,
      naming its Docker steps and the executors that run them, and no pod is made for it (a
      Docker step nested where the server couldn't see it still fails with KCI4032 when it
      comes).
  - A terminal into a pod's leg (W17-R01) is the pod's own shell: the leg is recorded as `k8s`,
    so it needs `runs.debug`, not `runners.terminal`.
  - An executor's pod is the pool's to reap alone: the reaper of v1 jobs' pods leaves a pod that
    carries no job. (It took one for a finished job's, so a leg that ran past the reaper's next
    turn, within 15 s, lost its pod mid-leg, was tried again in another and lost again.)
  - Checked against a stand-in for Kubernetes's API that runs each pod's job container on the
    machine, as a kubelet would, with no container engine but a Docker sidecar's
    (`w13_r07_k8s_pool_runs_each_leg_in_a_pod`: three legs on a pool of at most two pods, each
    leg on a pod's runner of its own, the pods gone after;
    `w13_r07_a_long_leg_in_a_pod_runs_to_its_end`: a 15-second leg runs once, in its one pod;
    `w13_r07_a_leg_with_containers_runs_in_a_pod_made_for_it`: the pod's job container from the
    leg's matrix, its service a sidecar, its steps run in place with the service's ports;
    `w13_r07_docker_actions_in_pods_need_the_pools_sidecar`: KCI4032 on a pool without one, and
    on one with it the pod's CLI, sidecar, socket and shared work directory, and the step run;
    `w17_r01_a_terminal_into_a_leg_in_a_pod`), and on the simulator
    (`w13_r07_k8s_pool_pods_are_their_own_runners`,
    `w13_r07_a_leg_with_containers_gets_a_pod_of_its_own`, `w13_r07_a_pools_docker_sidecar`).
- *Done.* R10's `kiln runner setup docker|firecracker|shards` (`crates/kiln/src/runner/setup.rs`):
  each executor's one-time privileged steps, each said before it's taken and asked for (`--yes`
  takes them, `--dry-run` only says them), `sudo` run for what needs root:
  - `docker`: Docker or Podman answering for this user; else the `docker` group (`usermod`), or
    why not (its daemon down, rootless Podman's subordinate ids), or that one is to be installed.
  - `firecracker`: KVM (`/dev/kvm`, else that virtualization is to be turned on; the `kvm` group);
    then Firecracker and its jailer from their latest release, checked against its SHA-256, into
    `~/.kiln/bin` (no root); a guest kernel (one of Firecracker's own CI kernels, 6.1, pinned by
    its SHA-256 for x86-64 and arm64) to `~/.kiln/firecracker/vmlinux`; and e2fsprogs, said to be
    installed when `mke2fs` isn't here.
  - `shards`: found on `PATH`, else said to be installed as shards' own project says.
  Unit-tested (`w13_r10_setup_says_each_step`); §4.3's commands are all kiln's
  (`w20_ac01_every_command_section_4_3_lists`).
- *Done.* R05's executor: a leg routed to `firecracker` (`executor = ["firecracker"]` in a
  routing rule, with its `image`) runs in a Firecracker microVM of its own
  (`crates/kiln/src/runner/firecracker.rs`, `rootfs.rs`, `guest.rs`):
  - Runners report `executors.firecracker` (its version and kernel) and announce
    `CAP_FIRECRACKER` when KVM, Firecracker (`KILN_FIRECRACKER`, else `~/.kiln/bin`, else
    `PATH`), a guest kernel (`KILN_FIRECRACKER_KERNEL`, else `~/.kiln/firecracker/vmlinux`) and
    `mke2fs` are all here.
  - The microVM's root file system is kiln's own disk: this kiln (and, for a dynamically linked
    build, its loader and libraries) on a small ext4, made once per build of kiln. `kiln guest`
    is its init.
  - The leg's image is pulled through kiln when kiln mirrors its registry (W12-R02), from kiln's
    own registry with the job's token, or straight from its registry. Its layers are kept by
    digest, each checked as it comes.
  - The image becomes an ext4 built by a microVM of its own: `kiln guest build` writes the files
    as root, so owners, modes, device files and whiteouts come out as the image has them, with
    no root on the host. It's kept by the manifest's digest and shared read-only by every leg of
    that image.
  - Each leg's microVM mounts its image under an overlay whose changes, workspace and caches go
    to a sparse scratch disk of its own. The leg runs there as the `host` executor runs one,
    with the image's environment; its events and logs come back over vsock as the host runner's,
    and a cancel goes in the same way.
  - Its network is no network device at all. Its one way out is a vsock tunnel to the leg's
    egress proxy on the host (W18-R08): with the leg's policy, or, for a leg without one, to
    anywhere but the host's own and link-local addresses. kiln's server is reached on the
    guest's loopback when it's on the host's. This is in place of R05's tap device, and needs no
    root.
  - What the proxy refuses is annotated on the step, as elsewhere. The microVM ends with its leg
    (a kept leg's stays: W17-R02), and a leg that outruns its own timeout and cleanup is stopped.
  - Each VMM runs in a jail of its own (`jail.rs`, `kiln vmm-jail`). Firecracker's jailer needs
    root, so kiln confines the VMM without it:
    - namespaces of its own (user, mount, PID, network, IPC and UTS);
    - a root of its own on a tmpfs, read-only, holding only what its microVM needs (Firecracker,
      the guest kernel, its disks, its vsock sockets' directory, `/dev/kvm`, `/dev/urandom` and
      `/dev/null`), with nothing else of the host's and nothing writable but its scratch disk and
      sockets;
    - no network at all, not even the host's loopback;
    - PID 1 of its PID namespace, no new privileges, and gone with its jail.

    Root in its user namespace is the runner's user outside it. Where a machine allows no
    unprivileged user namespaces (or `KILN_FIRECRACKER_JAIL=off`), microVMs run unjailed; the
    runner's facts say which (`executors.firecracker.jail`: `namespaces` or `none`). Checked
    (`tests/jail.rs`): a shell in Firecracker's place sees only its binds and its `/proc`, is
    root there as this user, has only a loopback, is PID 1, and writes only its writable bind.
  - When the leg's image has Docker (`dockerd`), the guest starts its daemon: cgroups mounted, its
    data on the scratch disk, no bridge or iptables of its own. The leg's Docker actions run there
    on the guest's network, their images pulled through the egress proxy
    (`w13_r05_docker_actions_in_a_microvm_with_docker`, with `docker:dind`). `binfmt_misc` is
    mounted too, so emulators a leg registers are the guest kernel's alone
    (`w22_ac02_setup_qemu_in_a_microvm`).
  - A `microvm` environment's deployments run in microVMs of their own that outlive their jobs
    (`kiln guest serve`, W15-R02).
  - R09's `firecracker` pools: each executor is a microVM of the pool's image, ready and waiting,
    so a leg for the pool starts in it at once (its image ready, its microVM up), and the pool
    readies another for the next (`w13_r09_a_firecracker_pool_keeps_a_microvm_ready`).
  - Where VMMs are jailed, a pool's microVMs are restored from a snapshot of its image, not
    booted:
    - A template microVM is booted once per image (and kiln build, guest kernel, Firecracker
      version and size) to where its guest waits for the host, then paused and snapshotted, its
      scratch disk kept as it was.
    - Each of the pool's microVMs restores it in a jail of its own, whose paths are the
      snapshot's, its scratch disk a copy (reflinked where the file system can).
    - The guest takes its leg in PID 1 before its runner starts: it sets its clock from the
      host's and reseeds its kernel's randomness with the host's. Restored microVMs neither keep
      the snapshot's time nor share randomness, and the runner seeds its own after.
    - The pool test checks it: two pooled legs, each in a restored microVM with the host's time
      and randomness of its own.
  - A leg's services and job container in a microVM whose image has Docker run on the guest's
    Docker and network, with a network policy too: the microVM is what contains them, its one way
    out the host's egress proxy (`w13_r05_services_and_a_job_container_in_a_microvm`: a service
    answering the leg's steps at its port, and a job container reaching it by name).
  - Checked end to end (`w13_r05_a_leg_runs_in_a_firecracker_microvm`, with Firecracker 1.17 and
    its 6.1 kernel, when `KILN_TEST_FIRECRACKER` and `KILN_TEST_FIRECRACKER_KERNEL` name them):
    an Alpine job runs on the guest's kernel, `/etc/shadow` keeps Alpine's `shadow` group, the
    server answers on the guest's loopback, and a request out goes through the host's egress
    proxy. Unit-tested: layers' whiteouts, frames, pulls through kiln, `ldd`'s libraries,
    private networks through the guest's proxy.
- *Done.* AC07's Dockerfile half (`tests/image_pool.rs`): an image built from what its editor
  holds (`POST /api/ci/images/build`, as the Images panel's Build and push) on the server's
  runner, pushed to kiln's registry and listed with its platform; then a `docker` pool of it,
  made as the Pools page makes one (`PUT /api/ci/pools/:name`), pulled from kiln's registry with
  the pool's own token, ran a leg in it (the file the Dockerfile wrote, on the image's Ubuntu).
- *Left.*
  - Firecracker's own jailer where the runner is root (kiln's jail needs no root, and confines as
    it does but for its cgroups).
  - The `shards` executor (R06) and its pool kind.
  - R11's other parts: checking Agentfiles by shards' own parser (its `agentfile` crate isn't
    one kiln can build against yet), kiln's shipped definitions beyond the runner image, and
    example Agentfiles. The image builder, its build route and the editor's modes are W21-R08's
    (done).
  - AC02's `shards` and `k8s` parts (W06-AC01 in their legs: a shards microVM, and a pod on the
    live cluster), AC06 (shards), AC07's Agentfile half (shards), and AC08 (a fresh VM through
    Add machines, user-assisted). AC01, AC03 to AC05 and AC07's Dockerfile half are done.

### W14 Code hosting, stacked diffs, review and landing

**Scope.** kiln hosts code, reviews it as stacks of diffs **and** as traditional branches and pull
requests, tests every version, and lands it with the merge strategy chosen at land time
(G-07, G-08, G-23). Repositories that stay on GitHub keep their pull requests, and kiln works with
them through a GitHub App. Jobs reach code and a GitHub-compatible API through the facade. This
adapts [review.md](review.md), which it replaces.

**Requirements: hosting**

- **W14-R01 Modes.** `native` (kiln hosts the repository; clone and push over HTTPS at
  `/git/<owner>/<repo>.git` with a PAT or agent token), `upstream` (kiln polls or receives webhooks
  from GitHub or another forge, as today), and `native + mirror` (kiln pushes trunk to an upstream
  after each land). `kiln repo import <url>` copies an upstream repository into native mode.
- **W14-R02 Git service.** Smart HTTP (`git-upload-pack`, `git-receive-pack`) backed by bare
  repositories in the data directory, served by kiln's HTTP server calling the `git` binary
  (as kiln already does for mirrors). A pre-receive check enforces policy: only the land queue
  writes trunks (unless `.kiln/review.yaml` `direct-push` allows a principal), pushes to
  `refs/for/<trunk>` become diffs, and branch protection rules apply.

**Requirements: two ways to review**

- **W14-R03 Stacked diffs**, as review.md specifies: a **diff** is one commit under review,
  numbered (`D123`), whose identity survives amends and rebases through a `Change-Id: I<40 hex>`
  trailer (Gerrit's format; kiln's commit-msg hook and `kiln submit` add it); each push of a
  changed commit adds a **version** (`D123 v3`), marked **rebase-only** when its patch is
  unchanged; a **stack** is a chain of diffs; an **interdiff** compares any two versions with
  rebase noise removed (`git range-diff` semantics); every version is kept at
  `refs/kiln/diffs/D123/v3`. `kiln submit` (or `git push kiln HEAD:refs/for/main`) creates or
  updates every diff between trunk and `HEAD`, links the stack, starts `diff` (and
  `pull_request`) workflows for each new version, and prints links; push options `-o draft`,
  `-o reviewer=…`, `-o message=…`, `-o topic=…`. `kiln sync` drops landed diffs and rebases the
  rest; `kiln absorb` routes working-copy edits to the commits that last touched those lines.
- **W14-R04 Branches and change requests** (traditional): a **change request** reviews a branch
  against a trunk as a whole, many commits at once, the way a GitHub pull request does. It has the
  same review states, threads, checks and landing as a diff; its versions are the branch's pushes.
  `kiln cr open <branch>` or pushing `refs/for/<trunk>/branch/<name>` opens one. A repository may
  use either way or both.
- **W14-R05 Review**, as review.md specifies (states `draft`, `needs-review`,
  `changes-requested`, `accepted`, `landing`, `landed`, `abandoned`; comments anchored to a
  version, file, side and line range that follow the code to newer versions or become outdated;
  suggestions applied as amends; accept, request changes, comment, resign; mark ready, abandon,
  reclaim, commandeer; drafts published together), plus, for G-08:
  - **Your conversations**: an inbox of threads waiting for **your** reply and threads where you
    wait for others, across all diffs, each one a row you can answer in place;
  - per diff, the unresolved threads listed with "next/previous unresolved" (keys `n`/`p`);
  - mentions (`@name`, `@agent:review`) that notify and enter the inbox;
  - a reviewer's state per diff (not yet looked, reviewing, accepted, asked for changes) shown on
    the stack row;
  - per-file "viewed" checkboxes that reset when the file changes in a new version;
  - replies by email or chat reply where the integration supports it (W11-R06).
- **W14-R06 Better diffs** (G-08): syntax highlighting (the same grammars as the editor, W21),
  word-level intraline changes, moved-block detection, whitespace-insensitive toggle, split and
  unified (unified on phones), generated and vendored files collapsed (`.gitattributes`
  `linguist-generated`, `-diff`), large diffs windowed so 10,000-line diffs scroll smoothly,
  comments on any line including context, multi-line ranges, rename and copy detection, binary and
  image diffs.

**Requirements: landing**

- **W14-R07 Merge strategies chosen at land time** (G-07):

  | Strategy | What lands on trunk |
  | --- | --- |
  | `stack` (default for stacks) | each diff as its own commit, rebased onto trunk's tip in stack order: one diff, one commit |
  | `stack-squash` | the stack as one commit |
  | `squash` (default for change requests) | the change request's commits as one commit |
  | `rebase` | each commit replayed onto trunk's tip (rewritten, linear) |
  | `merge` | a merge commit joining trunk and the branch (`--no-ff`) |
  | `ff-only` | trunk moved to the head only if trunk is its ancestor; refused otherwise |

  `.kiln/review.yaml` lists the allowed strategies per trunk (`land.strategies`) and the default
  (`land.default`). The land control is a select of the allowed ones; the CLI takes
  `kiln land D12 --strategy squash` and the API `{ strategy }`. Landed commits get
  `Differential-Revision: D123` (or `Change-Request: CR45`) and `Reviewed-by:` trailers. Optional
  commit signing of landed commits with a configured SSH or GPG key.
- **W14-R08 The land queue**, as review.md specifies: per trunk; the candidate rebased (or merged,
  by strategy) onto trunk's tip; `land` and `merge_group` workflows run on it; trunk moves
  atomically when they pass; batching up to `batch` with bisection on failure; ejection with a
  reason on conflicts or failure; remaining stacks rebased as rebase-only versions after each land.
- **W14-R09 Policy from trunk**, as review.md specifies (`.kiln/review.yaml`, `.kiln/owners.yaml`):
  approvals, owners, required checks (`land.checks`: workflows or jobs that must pass on the version
  and on the candidate), resolved threads, whether agents' accepts count, sticky approvals, the
  queue, direct pushes, and now `land.strategies` and `land.default`.

**Requirements: GitHub-hosted repositories**

- **W14-R10 GitHub App** (optional, `[ci.github] app_id, private_key_file`, or made in one step by
  GitHub's App manifest flow): the GitHub git host's credential (deploy-api.md §7, W23-P5), which
  KilnCI uses for repositories that stay on GitHub. Installation tokens
  for `GITHUB_TOKEN` when a repository uses `github_api: github` (W14-R12); commit statuses and
  check runs for every kiln run (today's optional token path, generalized); App webhooks as an
  event source (W03); pull request metadata; comments; landing pull requests with the chosen
  strategy through GitHub's merge API (merge, squash, rebase); **stacked pull requests** (chains of
  PRs whose bases are each other's branches): kiln shows them as a stack, lands bottom-up, and
  retargets the next PR's base after each land.

**Requirements: the facade for jobs**

- **W14-R11 Facade.** On its own origin (§2.6), kiln serves git (`/<owner>/<repo>.git`, from native
  repositories and from mirrors of upstream ones) and a GitHub REST subset at `/api/v3`, so
  `GITHUB_SERVER_URL` and `GITHUB_API_URL` point at kiln and `actions/checkout` (which takes
  GitHub Enterprise Server URLs), `actions/github-script`, `gh` and API clients work. The token is
  the job's `GITHUB_TOKEN` (the job token, with the job's permissions). Endpoints, at least:
  `GET /repos/{o}/{r}`, `/git/refs`, `/git/ref/*`, `/commits/{sha}`, `/compare/{base}...{head}`,
  `/contents/{path}`, `POST/GET /statuses/{sha}`, `/check-runs` (create, update, list),
  `/issues/{n}/comments` and `/pulls/{n}` (a pull request is a diff or change request), `/pulls`,
  `/pulls/{n}/files`, `/pulls/{n}/comments`, `/pulls/{n}/reviews`, `/releases` (with asset upload on
  the uploads path), `/actions/runs`, `/actions/runs/{id}`, `/actions/runs/{id}/artifacts`,
  `/actions/artifacts/{id}`, `/actions/artifacts/{id}/zip`, `/deployments` and their statuses
  (W15), `/attestations` (W18), `/code-scanning/sarifs` (W11-R10), `/user`, `/rate_limit`; GraphQL
  for the queries the top actions make. Anything else answers 501 with
  `{"message": "not implemented by kiln: <method> <path>", "documentation_url": …}`, and the
  step gets a warning annotation naming the route, so gaps are visible and get filled.
- *Note.* `gh` without `--repo` takes the repository from the checkout's remotes, and matches them
  to `GH_HOST` by host name alone: with a facade on a port other than 443 (`GH_HOST` names the
  port) it matches none ("none of the git remotes … correspond to the GH_HOST environment
  variable", the corpus lab's, on 42511). A GitHub Enterprise Server is always on 443; so is a
  facade that `gh`'s inference is to work with. `--repo` or `GH_REPO` works on any port.
- **W14-R12 Per repository** (`ci.github_api`): `facade` (default; everything through kiln, fast
  and independent of GitHub) or `github` (upstream repositories with the App: `GITHUB_SERVER_URL`
  is github.com and `GITHUB_TOKEN` an installation token, for workflows that must act on GitHub
  itself).

**Requirements: storage**

- **W14-R13 Schema** (migration 6): `repos` gains `mode` and `default_branch`; tables `diffs`
  (`id`, `repo`, `number`, `kind` diff|change_request, `change_id`, `title`, `summary`, `author`,
  `author_kind`, `state`, `trunk`, `stack_id`, `created_at`, `updated_at`), `diff_versions`
  (`diff`, `n`, `commit`, `parent`, `base`, `rebase_only`, `pushed_by`, `created_at`), `stacks`,
  `review_threads` (`diff`, `version`, `file`, `side`, `start_line`, `end_line`, `state`,
  `created_by`), `review_comments` (`thread`, `author`, `body`, `suggestion`, `created_at`,
  `published`), `reviews` (`diff`, `version`, `reviewer`, `action`, `body`, `created_at`),
  `land_queue` (`trunk`, `entry`, `diffs`, `strategy`, `state`, `batch`, `run`, `reason`),
  `viewed_files` (`diff`, `user`, `file`, `version`). All writes are audited.

**Acceptance criteria**

- **W14-AC01 Stacks.** Submitting three commits creates D1–D3 linked as a stack; amending D2 adds
  D2 v2 only; reordering is detected; an interdiff of D2 v1..v2 shows only the amend; a rebase-only
  version is marked. *Integration test with real git.*
- **W14-AC02 Strategies.** Landing the same stack and the same change request with each allowed
  strategy gives the expected trunk history (commit count, parents, trailers); `ff-only` refuses a
  diverged branch. *Integration test.*
- **W14-AC03 Queue.** Batch of 4 with one failing candidate: the queue bisects, ejects the culprit
  with its reason, lands the other three; conflicts eject as "needs rebase". *Integration test.*
- **W14-AC04 Policy.** Each `.kiln/review.yaml` rule, read from trunk (a diff loosening the policy
  is judged by trunk's). *Integration test.*
- **W14-AC05 Facade.** `actions/checkout@v4` (default settings, `fetch-depth: 0`, `ref:` a diff
  version, submodules), `actions/github-script` creating a commit status and a comment,
  `softprops/action-gh-release` creating a release with assets, and
  `actions/download-artifact` from another run, all against a native repository. *E2E.*
- **W14-AC06 Review UI.** Your conversations inbox, next/previous unresolved, viewed files, word
  diffs, moved blocks, a 10,000-line diff scrolling at 60 fps, unified on phones: checked in
  Chromium and WebKit. *UI check.*
- **W14-AC07 GitHub App.** Statuses and checks on GitHub for kiln runs; a stacked pull request
  landed bottom-up with retargeting. *E2E against a test repository, user-assisted.*

**Progress.** In progress: native hosting, stacked diffs, change requests, review, policy, the
land queue, the GitHub App and the review UI (AC06) are built, and the facade's REST subset and
GraphQL, landing through the queue as a branch that requires a merge queue lands.

- *Done.* Pushing to a new repository (R01, R02), as on GitHub
  (`w14_r01_first_branch_and_refusals_said`): the first branch pushed to an empty repository is
  its default when the branch it was made on isn't among those pushed. A refused push says why:
  kiln reads the rest of the pack before it answers, where git would otherwise see the
  connection close ("RPC failed") and not the reason.
- *Done.* AC06, against a real server: `ci/ui/review_setup.py` makes a diff with a 10,000-line
  change, a moved block, a one-word edit, a generated file and an image, and a reviewer's thread
  waiting for the author. `ci/ui/review_ac06.mjs` checks the review pages (W21-R09) in Chromium and
  WebKit (Playwright 1.64.1), on a desktop and a phone, with the UI server rendering them:
  - the inbox shows the thread waiting for your reply, and `n` brings it into view;
  - a file ticked viewed stays ticked across a reload;
  - word diffs mark only the changed word, and the moved block links to where it went;
  - the generated file is folded, and both sides of the image show;
  - on phones the diff is unified, and nothing is wider than the screen;
  - no script errors, no failed requests.

  The 10,000-line diff scrolls at a median 16.7 ms a frame in Chromium (p95 16.8 ms). WebKit's
  median is 17.0 ms, but its p95 is 82 ms in the container, where it renders without a GPU.

- *Done.* R01 and R02's hosting, in `crates/kiln/src/ci/hosting.rs`:
  - native repositories: bare repositories in `<data_dir>/hosted/<owner>/<name>.git` (apart
    from the mirrors), made empty (`POST /api/repos` with `mode: native`, `kiln repo create`,
    `[[repos]] mode = "native"`) or copied from upstream (`import`, `kiln repo import <url>`,
    from network URLs only); `repos` gains `mode` and `default_branch` (R13's first part);
  - clone and push over HTTPS at `/git/<owner>/<repo>.git` (and at the root, for legs), with a
    PAT or an agent token as git's password, or a leg's `GITHUB_TOKEN`;
  - fetches need `diffs.read` (a leg: `contents: read`, its own repository only), except of a
    public repository (`POST /api/repos` with `public: true`, shown as `public` in its listing),
    which anyone reads without a token, as anyone reads a public GitHub repository. A step's
    `git fetch` after `actions/checkout`'s `persist-credentials: false` relies on it (the
    corpus's elixir; its repositories are public, as on GitHub), and so does a token of kiln's
    that may not read it (a job's whose `permissions` leave `contents` out, as elixir's CodeQL
    jobs' do). A token that isn't kiln's is refused, public or not, and a push always needs one
    that may (`w14_r01_public_repositories_read_without_a_token`). A mirrored repository that's
    public reads so too, and so does the REST facade (R11): a job's read of its public
    repository needs none of its `permissions`, as on GitHub, where CodeQL's `actions: read` is
    "only required for workflows in private repositories". Code scanning alerts aren't public,
    and need `security-events: read` (`w10_r04_a_public_repositorys_reads_need_no_permission`;
    the corpus's prometheus, whose CodeQL job reads its run);
  - a leg's fetch of a repository kiln doesn't have (`actions/checkout`'s `repository:`, ORT's
    configuration) goes on to where kiln takes that owner's actions (`[[ci.actions.sources]]`,
    else GitHub), without the leg's token, so a public repository there is fetched as anyone
    fetches it, and one that needs credentials isn't found. It's passed through, nothing kept,
    so a shallow fetch stays shallow (`w14_r11_a_leg_fetches_another_public_repository`);
  - a push's ref updates are read before git sees the pack, and the push is refused whole, in
    git's own report and as `remote: kiln: …` lines, when one of them breaks policy:
    - only the owner, or a principal `.kiln/review.yaml` `direct-push` names (read from trunk),
      writes a trunk;
    - a trunk is never deleted, nor written in another case;
    - an empty repository takes its first trunk from anyone who may push;
    - other branches need `diffs.submit`, tags `diffs.land`; a leg needs `contents: write`;
    - `refs/kiln/*` and `refs/pull/*` are kiln's;
  - every push is audited (`repo.push`), and wakes the poller, so its workflows start at once;
  - a repository deleted takes its runs with it, as a deleted GitHub repository does: the CI
    engine drops their legs, a running one stopped on its runner and its slot free once the
    runner says it ended, and nothing of them settles or retries
    (`a_deleted_repository_takes_its_legs_with_it`). They used to stay in the engine, a queued
    leg still assigned and run with its records gone.
  - Checked end to end with real git (`crates/kiln/tests/hosting.rs`): clone and push by
    developer, viewer, owner and powerless tokens, trunk refusals, whole-push refusal, and the
    push's run.
- *Done.* R03's stacked diffs, in `crates/kiln/src/ci/diffs.rs` (pure part:
  `crates/kiln-ci/src/review.rs`):
  - a push to `refs/for/<trunk>` (`diffs.submit`) makes each commit between the trunk and the
    one pushed a diff (`D12`), or a new version of the diff with its `Change-Id`;
  - git stores the push, kiln makes the diffs, git's report gives each a link (`remote: kiln:
    D12 v3 (updated)`) or the whole push's refusal: no Change-Id, a merge, a landed or an
    abandoned diff, not a trunk;
  - only the diffs that changed get versions. A diff restacked on an amended one keeps its
    version; one whose patch only moved to a new trunk base gets a rebase-only version, marked;
    a reorder is seen and marked;
  - every version is kept at `refs/kiln/diffs/D12/v3`; the push's `refs/for/*` ref is never kept;
  - push options `draft`, `ready`, `reviewer=`, `message=`, `topic=` (and Gerrit's `%…`
    suffix);
  - each new version starts the `pull_request` (`opened`, then `synchronize`) and `diff`
    workflows on its commit, with the diff in `github.event.kiln.diff`;
  - the API: `/api/repos/:repo/diffs`, `/api/diffs/:id` (versions, reviewers, runs),
    `/versions/:v/files`, `/interdiff?from=&to=` (`git range-diff`), `/api/stacks/:id`, and
    `ready`, `abandon`, `reclaim`, `commandeer`, scoped to the diff's repository in the route
    table;
  - for the review pages (W21): each file of `/versions/:v/files` says whether it's `generated`.
    That is, the repository's `.gitattributes` at that version's commit marks it
    `linguist-generated`, or not to diff (`-diff`, which `binary` implies), and the review folds
    it. `/versions/:v/blob?path=&side=new|old` serves a file as it is on either side (`old` reads
    the parent, a renamed file's `old_path`). It's typed by its name (images and PDFs as
    themselves), `nosniff` and sandboxed, at most 10 MiB (413 past that), and a 404 where the
    side has no such file (`w14_generated_files_and_blobs`);
  - the CLI (`crates/kiln/src/ci/review_cli.rs`):
    - `kiln clone` (kiln's git credential helper, and the commit-msg hook that adds Change-Ids);
    - `kiln submit` (adds the Change-Ids commits lack, then pushes);
    - `kiln sync`, `kiln diffs`, `kiln checkout D12[@v2]`, `kiln hook install`;
    - `kiln absorb [-n]`: each edit of the working copy (to files HEAD has, staged or not) is
      amended into the stack's commit that last touched its lines (blame within the stack), and
      an insertion goes into the commit both its neighbours come from. Edits stay in the working
      copy when they touch the trunk's lines or two commits' lines, or when a commit above moves
      or changes their lines. The commits from the lowest target up are rewritten in place with
      git's plumbing (no rebase to stop half-way): trees with the edits, then authors, dates and
      messages as they were. HEAD moves only once the new top matches the working copy's files.
      Unit-tested on a stack of two commits (`w14_r03_absorb`);
  - tables `diffs`, `diff_versions`, `stacks`, `diff_reviewers`.
  - Checked end to end with real git: W14-AC01 (`w14_ac01_stacks`: three commits as a stack,
    D2 amended, the stack rebased and reordered, an interdiff, the refusals, the options, the
    author's actions, the runs), and the CLI (`w14_r03_cli`).
- *Done.* The rest of R01 and R02, and of R05 and R07:
  - `native + mirror`: a repository's `mirror` (API, `[[repos]] mirror =`) gets its trunk after
    each land and each direct push of it;
  - branch protection: `.kiln/review.yaml` `protected: [globs]`: those branches aren't deleted
    or rewritten (git refuses their non-fast-forwards once it has the commits), and only those who
    may land push them;
  - a suggestion applied as an amend (`POST /api/comments/:id/apply`): a diff's new version (its
    parent, message and author kept), a change request's commit on its branch; the thread is
    resolved, saying which version;
  - landed commits signed when `[ci] land_signing = { key, format }` says so (an SSH key by
    default);
  - a patch series without git (`POST /api/repos/:repo/submit`): `git format-patch` output or
    `{ message, diff }` patches, applied onto the trunk in a temporary index, given Change-Ids,
    and submitted as a push is.
  - Checked end to end (`w14_r05_suggestions_mirror_signing`).
- *Done.* R10 and R12, the GitHub App (`crates/kiln/src/githost/github.rs`, `web.rs`), as the
  GitHub git host's credential (W23-P5):
  - from `[ci.github]` (app id, private key, webhook secret; `api_url` and `web_url` for
    Enterprise Server), or made in one step by GitHub's App manifest flow
    (`POST /api/ci/github/app/manifest`, GitHub's callback at `/api/ci/github/app/callback`; its key
    and secrets sealed in the store; `administration: write` among its permissions, for release
    repositories it makes, and its webhook off where kiln's URL is a private one GitHub can't
    reach); `GET /api/ci/github/app` says which, and where to install it;
  - RS256 JWTs, installation tokens limited to one repository and the permissions each call needs,
    kept until shortly before they expire;
  - a check run for every KilnCI run of a repository on GitHub, made when it's queued, done with its
    conclusion, a new one for each attempt, caught up every minute;
  - the App's webhook (`/api/hooks/github/app`, signed): GitHub's Re-run reruns kiln's run, a
    check suite's reruns its failed legs, the rest reach the workflows as GitHub's events do;
  - `github_api: github` (R12): the job's `GITHUB_TOKEN` is an installation token with the job's
    permissions, and `GITHUB_SERVER_URL`, `GITHUB_API_URL` and the `github` context are GitHub's;
  - the git-host calls deploy-api.md §7 needs: read, commit through the Git Data API (moved only as a
    fast-forward of what it was made on), push with a lease, pull requests (opened, found by their
    branch, followed, closed with why) and who merged one, statuses, deployments with their
    statuses, push webhooks, repositories made in an organization or from templates.
  - Checked against a fake GitHub API and `git daemon` (`w14_r10_github_app`); W14-AC07 against
    GitHub itself waits for an App and a test repository from the user.
- *Done.* R04's change requests, in `crates/kiln/src/ci/cr.rs`: a branch reviewed against a
  trunk as a whole (`CR4`, numbered with the diffs):
  - opened by pushing `refs/for/<trunk>/branch/<name>` (`kiln cr open [branch]`, `-o title=`)
    or `POST /api/repos/:repo/change-requests` for a branch already pushed;
  - each later push of the branch is a version (rebase-only when its combined patch is the same
    on a new base), said in git's report;
  - the same review, threads, checks (`pull_request`), API (`/api/change-requests/:id/…`) and
    queue as diffs; the commits it brings are listed;
  - it lands by `squash` (the default: one commit), `merge` (a merge commit), `rebase` (each
    commit replayed) or `ff-only`, each with a `Change-Request:` trailer where it writes a
    commit;
  - checked end to end (`w14_r04_change_requests`): every strategy's history, and ff-only refused
    once the trunk moved on.
- *Done.* R05's review, in `crates/kiln/src/ci/review.rs`:
  - threads anchored to a version, file, side and lines, that follow the code to the latest
    version (`git diff -U0` hunks) or show as outdated;
  - comments drafted privately and published together by a review (`accept`,
    `request_changes`, `comment`, `resign`); the author doesn't accept their own;
  - each reviewer's state; the diff's review state from its reviews;
  - threads resolved and reopened (`PATCH /api/comments/:id`);
  - files marked viewed until they change;
  - the inbox (`/api/review/inbox`): threads waiting for you (on your diffs, ones you joined, ones
    that mention you), threads where you wait, review requests, your open diffs.
- *Done.* R09's policy from the trunk (`crates/kiln-ci/src/land.rs`): `.kiln/review.yaml` and
  `.kiln/owners.yaml` read from the trunk when they apply:
  - approvals from people other than the author, agents' only with `agents-can-approve`;
  - sticky approvals across rebase-only versions;
  - owners (the last matching rule, optional rules aside);
  - `land.checks` on the version (a skipped or neutral check passes, as on GitHub);
  - open threads; `land.strategies` and `land.default`;
  - what still blocks a diff is listed at `/api/diffs/:id/land`.
- *Done.* R07 and R08, the land queue, in `crates/kiln/src/ci/land.rs`:
  - `POST /api/diffs/:id/land` (`diffs.land`) lands the diff and those below it, by the strategy
    asked for, within `land.strategies`: `stack` (one commit each, with `Differential-Revision:`
    and `Reviewed-by:` trailers), `stack-squash`, or `ff-only` (the commits as they are, refused
    when diverged);
  - per trunk, the next batch (up to `queue.batch`, halved after each failure until the culprit
    is alone) is applied onto the trunk's tip without a working tree (`git merge-tree`; a
    temporary worktree where git is older than 2.40), kept at `refs/kiln/land/<trunk>/<batch>`,
    and tested by its `merge_group` and `land` workflows;
  - a required check that none of them runs ejects the batch;
  - when they pass, the trunk moves in one compare-and-swap update (a moved trunk means a new
    candidate);
  - a failure ejects a lone entry with its reason, or splits the batch; a conflict ejects the
    entry as "needs rebase";
  - landed diffs get `pull_request` `closed` events; the open diffs above them are restacked onto
    the new trunk as rebase-only versions;
  - `GET /api/repos/:repo/queue`, `DELETE /api/queue/:entry` (`queue.manage`);
  - `kiln land D12 [--strategy S]` and `kiln review D12 [--accept | --request-changes | --comment
    | --resign]`.
  - Checked end to end:
    - W14-AC02 and AC04 (`w14_ac02_strategies`): a review lets a stack land; each strategy's
      history; the restack; a policy-loosening diff judged by the trunk's policy;
    - W14-AC03 (`w14_ac03_queue`): a batch of four with one culprit bisected, the culprit
      ejected with its reason and three landed, and a conflict ejected; the checks run on a real
      runner that fetches through kiln's git with the leg's token.
- *Done.* R11's git half, in `crates/kiln/src/ci/facade.rs`:
  - smart HTTP over the server's mirror, served by `git http-backend` (protocols v0 to v2,
    gzipped requests, fetches by SHA, filters);
  - served at the root of the server's origin (§2.6);
  - open to a leg's token only, and each token reads only its own run's repository.
  - `actions/checkout@v4` with default settings runs unmodified.
- *REST so far.* `crates/kiln/src/ci/rest/`, at `/api/v3`, each route checking the job token's
  permissions with GitHub's mapping:
  - the artifact endpoints (`download-artifact` across runs);
  - the dispatches;
  - the repository's reads from the mirror: repository, commits (with files), branches, refs
    (`/git/ref/{ref}`, `/git/matching-refs/{ref}`, and the older `/git/refs[/{ref}]`: the ref
    named exactly, else those it begins), contents, tarballs and zipballs, compare;
  - commit statuses with the combined status (and the older `GET /statuses/{sha}`), and check
    runs;
  - releases, with assets uploaded to `upload_url` (`/api/uploads/…`) and served by
    `browser_download_url`;
  - dependency snapshots (`POST /dependency-graph/snapshots`, validated as GitHub validates them
    and kept for W18), which Gradle's, Maven's and sbt's submission actions send;
  - deployments and their statuses (`/deployments`, `/deployments/{id}/statuses`), and
    `/environments` with GitHub's `protection_rules` (W15);
  - pull requests over diffs and change requests (`crates/kiln/src/ci/rest/pulls.rs`):
    `/pulls`, `/pulls/{n}`, `/files`, `/commits`, line comments (`/pulls/{n}/comments`,
    `/pulls/comments/{id}`), reviews (`APPROVE`, `REQUEST_CHANGES`, `COMMENT`, pending), and
    the issues API's view (`/issues/{n}`, `/issues/{n}/comments`, `/issues/comments/{id}`, edits
    and deletes of jobs' own comments). A job writes as `github-actions[bot]`, an agent: its
    approval counts only where `agents-can-approve`; a comment on the whole pull request doesn't
    hold a land up. Checked with a real run (`w14_r11_pulls`);
  - over HTTPS (§2.6): a server that serves it (`server.https`, or an `https://` address) sends
    its certificate authority with each leg, and the leg's tools trust it beside the machine's
    roots. Node adds it to its own (`NODE_EXTRA_CA_CERTS`); git, curl, Python and OpenSSL take a
    bundle of the machine's roots and it (`GIT_SSL_CAINFO`, `CURL_CA_BUNDLE`,
    `REQUESTS_CA_BUNDLE`, `SSL_CERT_FILE`). A job's own say wins. In kiln's runner image the
    authority goes in the leg's machine's own store instead (`update-ca-certificates`), as a
    hosted runner's machine trusts what it reaches. Then only Node and Python's requests, which
    read bundles of their own, are told, and git says what it says on GitHub (`CAfile: none`,
    which cargo's https tests check). The server's certificate names
    `host.docker.internal` too, which containers on its machine reach it by. GitHub Enterprise
    Server's clients go `https://` to the host they're given whatever its scheme, as
    `create-pull-request` and `actions-gh-pages` do, so this is what lets them run. Checked end to
    end (`w14_r11_facade_over_https`: curl, git and Node over `https://`);
  - pull requests jobs open, as `create-pull-request` does: `POST /pulls` makes a change request
    for a branch the job pushed with its token (one open per branch and trunk, a second refused
    with 422), and `PATCH /pulls/{n}` changes its title or description, or closes it (abandoned)
    and opens it again. Labels, as `actions/labeler` sets them: `GET`, `POST` (add), `PUT` (set)
    and `DELETE` on `/issues/{n}/labels`, and `DELETE /issues/{n}/labels/{name}` (404 when it
    hadn't it); a pull request's JSON lists them. Checked with a real run
    (`w14_r11_pull_requests_jobs_open_and_label`);
  - workflow runs as GitHub shapes them, from kiln's: `/actions/runs` (with GitHub's filters),
    `/actions/runs/{id}`, `/actions/workflows/{id}` and `/actions/workflows/{id}/runs` (by file
    name or number);
  - a person's permission on the repository, `GET /collaborators/{username}/permission`, as
    actions check the run's actor's before they act for it (openai/codex-action: ruff's security
    review, corpus pass 1025a, 2026-10-25). It's GitHub's legacy base permission (`admin`,
    `write`, `read`, `none`; maintain is write) and role, from what kiln grants the person there
    as its git serves them: the server's owner is the repository's admin, then `settings.manage`
    an admin, `diffs.land` a maintainer, `diffs.submit` a writer, `diffs.read` or `git.read` a
    reader. No such person: 404 (`w14_r11_runs_and_code_scanning`,
    `w14_r11_a_collaborators_permission_in_githubs_shape`);
  - code scanning (`crates/kiln/src/ci/rest/scanning.rs`), for W11-R10:
    - `POST /code-scanning/sarifs` (the public API) and `PUT /code-scanning/analysis` (what
      codeql-action uploads with) take SARIF 2.1.0 in base64, gzipped or not, up to 64 MiB
      unpacked, read it into alerts at once and answer 202 with the upload's id;
    - an analysis per tool, ref and commit;
    - alerts by fingerprint on a ref (the result's `partialFingerprints`, else its rule, file,
      line and message): a result seen again keeps its alert's number; one the same tool and
      category no longer find is `fixed`, and open again if it comes back;
    - the leg gets the new alerts as annotations (the first 50);
    - `/sarifs/{id}`, `/analyses` and `/alerts` (with `/alerts/{n}`) read them back, with
      GitHub's filters;
    - codeql-action's feature flags (`/codeql/features`: none) and its status reports
      (`PUT /code-scanning/analysis/status`) are answered, so
      `github/codeql-action/upload-sarif` runs unmodified;
  - the dependency graph's compare (`crates/kiln/src/ci/rest/deps.rs`), as
    `actions/dependency-review-action` asks it: `GET /dependency-graph/compare/{base}...{head}`
    (`base` at its merge base with `head`, and `?name=` for one manifest) lists what the
    lockfiles kiln reads (W18-R03's) pin differently, package by package in GitHub's shape: each
    one removed and each one added, a version changed as both. The added packages'
    vulnerabilities come from the advisory database `[ci.advisories]` names (OSV's API, which
    carries GitHub's advisories: each one's GHSA, from its own id or its alias, with the severity
    GitHub gives it, else its CVSS v3 vector's). With none named, the changes come without
    vulnerabilities and the answer says so in `x-github-dependency-graph-snapshot-warnings`,
    which the action shows as a warning, as an Enterprise Server without GitHub Connect has no
    advisories to give. One that doesn't answer fails the request (503), so no review passes for
    want of it. A scope isn't said, so the action takes each change as `runtime`, as it does an
    Enterprise Server's. Checked with a real run against a stand-in for OSV
    (`w14_r11_dependency_graph_compare`);
  - `/user`, `/rate_limit`, `/meta`;
  - 501 "not implemented by kiln", with a warning on the leg, for everything else;
  - GraphQL at `/api/graphql` (`crates/kiln/src/ci/rest/graphql.rs`, over a parser of GraphQL
    documents in `crates/kiln-ci/src/graphql.rs`: operations, fragments and inline fragments on
    types and interfaces, variables with defaults, aliases, `@skip` and `@include`, nesting
    capped at 64). It answers over the REST half's handlers, so a token's permissions hold as
    they do there, in the shapes of GitHub's schema:
    - `viewer`, `rateLimit`, `node(id:)` and `nodes(ids:)` by the REST half's `node_id`s (so an
      id from a REST answer works in a mutation);
    - `repository(owner:, name:)`, the token's own: its default branch, `ref`, `refs`, `object`,
      `labels`, `pullRequest(number:)`, `issueOrPullRequest(number:)` and `pullRequests` (by
      state, head, base and labels, ordered and paged with GitHub's cursors);
    - a pull request's fields, with its `labels`, `comments`, `commits`, `files`, `reviews`
      (`latestReviews`, `latestOpinionatedReviews`), `reviewRequests` and `statusCheckRollup`
      (its head commit's check runs and statuses, rolled up as GitHub rolls them);
    - mutations `addComment`, `updateIssueComment`, `deleteIssueComment`, `createPullRequest`,
      `updatePullRequest`, `closePullRequest`, `reopenPullRequest`, `addLabelsToLabelable`,
      `removeLabelsFromLabelable` and `addPullRequestReview`; landing's (below);
    - `__type(name:)`'s fields, which `gh` asks an Enterprise Server for before it decides what
      to ask.

    It's lenient where GitHub is strict: a field kiln doesn't keep answers null (a connection,
    empty), so a client asking for more than kiln keeps still gets what it does keep. An unknown
    root field or mutation is an error naming it, with a warning on the leg, as the REST half's
    501s give one. `gh` takes an Enterprise Server's host and token from `GH_HOST` and
    `GH_ENTERPRISE_TOKEN`, and the runner sets both for a step that gives `gh` the job's own
    token (`GH_TOKEN: ${{ github.token }}`, or `GITHUB_TOKEN`), so `gh pr comment` and the rest
    work on the run's repository unmodified; another token is left for github.com, and a step's
    own `GH_HOST` wins. Checked with a real run (`w14_r11_graphql`: a pull request read with a
    fragment, its labels and its head's checks, a comment added and a label removed by mutations
    with ids from the query, the open pull requests, `__type`, and a root field kiln doesn't
    answer);
  - GitHub-shaped run links redirected to kiln's pages.
  - Search (`search.rs`): `GET /search/issues` and GraphQL's `search` over the job's repository's
    pull requests, with issue search's qualifiers (`repo:`, `is:`/`type:` with `pr`, `open`,
    `closed`, `merged`, `unmerged` and `draft`, `head:`, `base:`, `author:`, `label:` (a list is
    either), `-label:`, `no:label`, `sort:`, and words in a title or body) in the advanced syntax
    (`AND`, `OR`, `NOT`, parentheses; `SearchType` says `ISSUE_ADVANCED`). `is:issue` finds
    none, a native repository having no issues besides its pull requests
    (`w14_r11_search_qualifiers`, and `w14_r11_graphql` asking both from a job).
  - `gh` end to end (`w14_r11_gh_against_the_facade`): GitHub's CLI 2.102.0, the release kiln's
    runner image installs, in a pull request's job over HTTPS, given the job's token as workflows
    give it (`GH_TOKEN`; the runtime makes it Enterprise Server's). It reads the repository with
    `gh api`, views and lists the pull request, finds it by search, comments, and adds and
    removes labels. `/meta` says an Enterprise Server version (3.18.0), which `gh` reads to know
    what it may ask (as a body field only, so clients that look for the header still take kiln
    for github.com).
  - Landing, as GitHub lands where a branch requires a merge queue: every kiln trunk lands only
    through its land queue (R08).
    - A pull request says `isMergeQueueEnabled`, `isInMergeQueue` and its `mergeQueueEntry`
      (position, state, who put it there), and its `autoMergeRequest`. `__type` lists them, which
      is how `gh` learns the queue is required, and the REST pull request has `auto_merge`.
    - `enablePullRequestAutoMerge` has a diff wait to land: into the queue as soon as its trunk's
      policy lets it (at once if it does already; `land_when_ready`), by the method asked
      (`SQUASH`; a stack of diffs' `REBASE` is its `stack`, and its `MERGE` the trunk's default).
      `disablePullRequestAutoMerge` ends the wait. `enqueuePullRequest` and `dequeuePullRequest`
      put it in now (if its policy lets it) and take it out. `expectedHeadOid` is checked.
    - `mergePullRequest`, and REST's `PUT /pulls/{n}/merge`, are refused as GitHub refuses them
      there ("Changes must be made through the merge queue", 405); `GET /pulls/{n}/merge` says
      whether it landed.
    - A job may land if its token writes contents and pull requests, as on GitHub, and whoever
      started its run may land here (`diffs.land`): a job never does more than its starter, so a
      developer's workflow can't land their own diff (a run no one started, a schedule's, is the
      repository's). Its land entry is held to that starter, as the candidate's runs are to a
      person's landing (W10-R02's `secrets.use`). `auto_merge_enabled` and `_disabled` are said as
      `pull_request` actions (what a job does starts no run, as on GitHub).
    - Checked with `gh pr merge` (`w14_r11_gh_lands_through_the_queue`): in a maintainer's diff's
      job, `--auto --squash` has it wait (`gh` saw the queue required and asked auto-merge),
      `--disable-auto` ends the wait and a second `--auto` starts it again, and REST's merge is
      refused; accepted, it lands through the queue as one commit. In a developer's diff's job, the
      same is refused: the developer started the run and doesn't land here.
  - Checked end to end:
    - `github-script` (statuses, checks, repository, commit, contents);
    - `docker/metadata-action`;
    - `softprops/action-gh-release` with an asset;
    - `actions/checkout` unpacking the tarball where git is missing;
    - a job reading its own run, its workflow and the runs list, then uploading SARIF twice:
      the first upload opens an alert, the second fixes it, and a bad commit gets a 422
      (`w14_r11_runs_and_code_scanning`).
- *Done.* R05's replies by email or chat (`crates/kiln/src/ci/replies.rs`, `ci/imap.rs`):
  - Email: an `email` integration with `replies` (a mailbox kiln reads: `review@example.com`)
    sends a message about a review thread (`comment.reply`, `mention`) to each address on its
    own, `Reply-To: review+<token>@example.com`, the token that address's for that thread. kiln
    reads the mailbox's unseen messages over IMAP (`imap_host`, `imap_port`, `imap_tls`,
    `imap_user`, `imap_folder`, `imap_every`; the integration's password; a password goes in the
    clear only to this machine). A reply to a token, from the address it went to, is posted to
    the thread by the person with that address, without what it quotes or its signature
    (MIME's parts and encodings read); anything else is left, saying why.
  - Slack: a bot's message about a thread is remembered; a reply in its Slack thread, sent by
    Slack's Events API to the integration's own address (`GET /api/ci/integrations/:name/replies`
    says it, with the bot's scopes and events; its key derived from the server's master key), is
    posted by the person whose Slack email it is (`users.info`). The bot's API is `api`
    (GovSlack's, say). Discord, Teams and Telegram take no replies: their integrations post
    through webhooks or bots kiln doesn't hear from.
  - A reply is that person's comment: they must be able to review the diff (`diffs.review`).
  - Checked end to end (`tests/replies.rs`, against stand-in SMTP, IMAP and Slack servers): a
    reviewer's emailed reply and their Slack reply each posted to the thread as theirs, quoting
    left out; the same reply address from another mailbox, and a reply in a thread kiln didn't
    start, posted nothing.
  - On the way: a repository kiln hosts has its trunk's files (rules, environments, settings)
    read from its own git, not its mirror, which has a push only after the next poll.
- *Left.* Everything else:
  - AC07, which needs a test repository on GitHub (user-assisted). AC06 is done (above), and so
    is AC05: `github-script`, the release with an asset and `actions/checkout` from the facade
    above, and a checkout at a diff's version with its submodules and `download-artifact` across
    runs on a native repository in `w14_ac05_github_actions_on_a_native_repository` (W07-R09).

### W15 Environments and deployments

**Scope.** Environments that are easy to create and manage, of every kind kiln can deploy to (a
Kubernetes cluster through kiln's CD, a runner host, a container host, a microVM, a cloud or
hosting platform), GitHub-exact `environment:` semantics, and CI and CD working as one (G-13,
G-15). [cd.md](cd.md) remains the specification of the CD engine; this section owns the
environment model both use.

**Requirements**

- **W15-R01 The model.** An environment has a `name`, a `kind`, targets, protection rules,
  secrets and variables (W09), a concurrency limit, deployment history and an optional URL.
  Environments come from trunk's `.kiln/environments.yaml` (as cd.md and review.md use it) and
  from kiln itself (made in the UI, CLI or API, marked as such, with "Save to repo" turning them
  into a change).
- **W15-R02 Kinds.**

  | Kind | Targets | How a deployment happens | Health from |
  | --- | --- | --- | --- |
  | `kubernetes` | kiln CD clusters and namespaces | CD syncs the environment's apps at the revision (cd.md) | CD health |
  | `host` | a runner selector (`label deploy-web`, facts) | the deploying job runs on those hosts (`runs-on` is derived from the environment) | a declared health check (HTTP, command) |
  | `container` | runners with Docker | `kiln/deploy@v1` with a Compose file or image list, on those hosts | container health checks |
  | `microvm` | runners with Firecracker or shards | `kiln/deploy@v1` with an image (shards: an Agentfile-built image) run as a long-lived microVM | the guest's health check |
  | `platform` | an integration (W11): AWS, Google Cloud, Azure, Vercel, Cloudflare, Fly.io, … | the job's steps, with that integration's credentials bound to the environment | a declared check, or the platform's status API where the integration supports it |

- **W15-R03 Protection**, GitHub-exact where GitHub has it: required reviewers (people or teams;
  agents only with `agents-can-approve`), a wait timer, deployment branch and tag policies, plus
  kiln's: windows (allow and deny cron, as cd.md's sync windows), custom gates (a workflow or an
  attached agent's check that must pass), `after:` and `soak:` promotion (cd.md), and
  `require-provenance` (W18).
- **W15-R04 Jobs**, GitHub-exact: `environment: name | { name, url }` binds a leg to an environment:
  its protection is enforced before the leg starts (the leg waits with reason `approval`, approved
  on the leg itself in one step), its secrets and variables apply, `concurrency` limits its
  deployments, and a deployment record is created with the leg's result and the evaluated `url`.
  The facade's deployment endpoints reflect them. `environment: { name, deployment: false }`
  applies the environment's secrets, variables and protection without recording a deployment. As
  on GitHub, it can't be combined with custom protection rules (kiln's custom gates, W15-R03): the
  run fails to start, saying why.
- **W15-R05 CI drives CD.** `uses: kiln/deploy@v1` with `{ app?, environment, revision? (default
  the run's commit), wait: synced | healthy, timeout }` asks CD to sync and streams the sync's
  progress (waves, resources, health) into the step's log; the step fails if the sync fails or
  health degrades within the timeout. A kiln-dialect shorthand `deploy: { app, environment }` on a
  job does the same.
- **W15-R06 CD drives CI.** CD events are `deploy` triggers (W03): a workflow can run after an app
  becomes healthy in staging (smoke tests), or when it degrades (diagnostics, rollback).
- **W15-R07 Previews.** Per diff, stack or pull request, an environment made from a template
  (`previews:` in `.kiln/environments.yaml`, cd.md's previews), with a TTL, its own URL, removed on
  land, abandon or close; CI jobs can deploy to it with `environment: preview`.
- **W15-R08 Creating and managing environments is easy.** One page per environment, in place:
  kind, targets (pickers backed by the facts, clusters and integrations kiln knows), protection,
  secrets and variables, history, what is waiting. `kiln env create staging --kind kubernetes
  --cluster dev --namespace web` and the API do the same. A starter set (`dev`, `staging`,
  `production`) can be made in one step for a repository.
- **W15-R09 The environments board**: every environment as a row, with what each app or
  deployment runs (revision, who and what deployed it, health), what is waiting to move (approvals,
  windows, soak), and drift (CD's out-of-sync).

**Acceptance criteria**

- **W15-AC01 Protection.** Each rule blocks and releases as specified; approval on the leg in one
  step; wait timers with a simulated clock; branch policies. *Integration test.*
- **W15-AC02 CI → CD.** A workflow building an image then deploying with `kiln/deploy@v1` to a
  `kubernetes` environment on the live cluster streams the sync into the log and fails when the app
  degrades (a fixture with a crashing container). *E2E.*
- **W15-AC03 Kinds.** One deployment of each kind with a fixture target (`host`: a systemd user
  service on a runner; `container`: a Compose stack; `microvm`: a long-lived shards or Firecracker
  microVM; `platform`: a static site to a test tenant). *E2E, user-assisted for platform tenants.*
- **W15-AC04 Previews.** A diff creates a preview, a new version updates it, landing removes it.
  *E2E.*
- **W15-AC05 Board.** Shows revisions, health, waits and drift for the live cluster's apps, in
  Chromium and WebKit. *UI check.*

**Progress.** In progress: R01, R03, R04, R05, R06, R08 and R09 are done, with R02's kinds on
CI's side and its `container` and `microvm` deployments (Firecracker's; shards' left with W13),
and R07's previews on CI's side; CD's preview apps and the ACs that need the live cluster are left
(with CD).

- *Done.*
  - `kiln_ci::env`: GitHub's protection rules (required reviewers, `prevent-self-review`, a wait
    timer, deployment branch and tag patterns) and the gate a job bound to an environment
    passes. A ref that may not deploy fails the job with GitHub's message; a pull request's merge
    ref is a branch named `refs/pull/<n>/merge`, as on GitHub. A rejection fails it. Otherwise it
    waits for a reviewer, for its timer, or both. AC01's rules are unit-tested with a simulated
    clock.
  - `crates/kiln/src/ci/environments.rs`: environments come from trunk's
    `.kiln/environments.yaml` (CI's keys beside CD's, read with them) and from kiln
    (`/api/ci/environments`, `kiln env list | create | delete | deployments`). A job naming one
    neither has makes it, with no rules, as GitHub does.
  - The dispatcher holds a protected leg out of the scheduler. Its wait reason says
    `approval` (and whom) or `wait-timer` (until when). `POST /api/ci/runs/:id/approve` lets it
    go or fails it, as do `kiln approve <run> [--reject]` and "Approve and deploy" on the run
    page, in one step. A review counts when its author is a listed reviewer (the owner always is;
    an agent only where `agents: { approve: true }`) and isn't the run's actor under
    `prevent-self-review`. A run's cancel ends a held leg, and a restart holds it again.
  - Deployments are recorded when the leg starts (unless `deployment: false`), with the leg's
    last status and the `environment.url` the runner evaluates after the steps. The facade
    serves them (W14-R11).
  - Checked end to end on a local server, with three environments in `.kiln/environments.yaml`:
    - `qa`'s branch pattern refused `main` with GitHub's message;
    - `staging`'s one-minute timer held its job exactly 60 s, and its deployment carried the URL
      a step's output built;
    - `production` waited for its reviewer, refused a viewer (403), deployed once the reviewer
      approved, and read the staging deployment and every environment's protection through the
      facade;
    - a rejection failed the job with the reviewer's comment, and a cancel ended a held one;
    - a held job stayed held across a server restart, then deployed when approved;
    - the run page's review in Chromium and WebKit, on desktop and phone.
- *Done.* R05's `kiln/deploy@v1` (`crates/kiln/src/ci/deploy_step.rs`, the runner's
  `deploy_step`):
  - A leg whose job is bound to the environment (`environment:` on the job, its protection
    passed before it started) asks the deploy service for a release of the app there
    (`POST /_kiln/deploy/releases`, with the leg's token). A job that isn't bound to the
    environment is refused, saying so.
  - It asks as `job:<leg>`, once per attempt (an idempotency key), so the service's own gates
    apply too. Without `app:`, it releases the one app that names the repository as its
    `source` and is registered in the environment; when there is none, or more than one, it
    asks which.
  - The step follows the release, logging each change: its state, what the provider says (sync,
    health, the operation's phase), and the release's message. `wait: synced` ends once the
    provider has it synced and healthy. `wait: healthy` (the default) ends when the release
    succeeds, after staying healthy through the environment's settle time. A failed,
    cancelled or rolled-back release, or health that degrades after the sync, fails the step,
    and so does the `timeout` running out.
  - The kiln dialect's `deploy: { app, environment, revision, wait, timeout }` on a job binds
    it to the environment (a different `environment:` is an error, KCI1021), needs no steps,
    and runs `kiln/deploy@v1` after the job's own steps (and its `attest:`).
  - Checked:
    - `w15_r05_following_a_release`: the step's reading of each state, unit-tested;
    - `w15_r05_the_deploy_shorthand`: the shorthand's binding and its step;
    - end to end (`w15_r05_kiln_deploy_releases_from_the_job`): the bound job's release was
      made as the job and followed, and the unbound job was refused, with no release made.
      The shorthand job, with no steps, released the repository's own app.
- *Done.* R06: KilnCD's app statuses raise kiln's `deploy` event in the app's repository (see
  W03's notes).
- *Done.* Teams as required reviewers (R03). `team:acme/web`, or `team:web` for the
  repository owner's team, counts anyone whose memberships on the hosts they sign in with hold it.
  Those are W23-R08's: GitHub's `org/team`s, GitLab's group paths and Bitbucket's workspaces, as
  of their last sign-in. A team doesn't lift `prevent-self-review`. Unit-tested
  (`w15_r03_who_may_review`).
- *Done.* R01's concurrency limit: `concurrency: N` on an environment (in
  `.kiln/environments.yaml`, CD's key too, or `kiln env create --concurrency N` and the API).
  At most N of its deployments get past its gate at once, counting legs bound to it that are
  queued or running. The rest wait, saying so (`concurrency`, with the environment and the
  limit), and go in the order they got ready as each deployment ends. Checked end to end
  (`w15_r01_an_environments_deployments_one_at_a_time`): with `concurrency: 1`, two jobs
  deploying to staging ran one after the other on a runner with two slots.
- *Done.* R03's windows, as cd.md's sync windows, in the same `windows:` key of
  `.kiln/environments.yaml`: `{ allow | deny: <cron>, duration, timezone }`. A job deploys only
  while an `allow` window is open, and waits while a `deny` window is (`window`, saying until
  when). It goes when the window opens, after its reviewers and wait timer. Windows that don't
  check out leave the environment's rules refused, as other bad rules do. Unit-tested
  (`w15_r03_windows`: a weekend deny window, office hours).
- *Done.* R03's promotion and custom gates, in `.kiln/environments.yaml` (CD's `after` and
  `soak` keys, which its releases already follow):
  - `after: staging` deploys only a commit whose deployment to staging succeeded, and with
    `soak: 30m` only once it has been there that long. The job waits (`promotion`, saying for
    what and until when).
  - `gates: [{ workflow: .kiln/smoke.yaml }, { check: security }]`: a commit deploys once each
    passed for it. A workflow gate is that workflow's run for the commit. A check gate is the
    check run of that name, else the commit status (an attached agent's, say), else a run or job
    kiln names so (`smoke`, `smoke/test`). Skipped and neutral pass. The job waits (`gate`,
    naming what hasn't passed); one that failed fails the job, saying which. A gate can't be the
    workflow that deploys, and, as GitHub's custom protection rules, gates can't take a job's
    `deployment: false`: either fails the job, saying why.
  - Held legs are looked at again every 15 seconds. Unit-tested
    (`w15_r03_promotion_and_custom_gates`), and end to end (the hosting test of that name):
    production took a commit after staging had it for four seconds and its smoke workflow
    passed, the leg waiting `promotion` and `gate` meanwhile; a commit whose smoke run failed
    never deployed there.
- *Done.* R02's kinds, CI's half, and R08 (`crates/kiln/src/ci/environments.rs`):
  - An environment has a kind and where it deploys: `kubernetes` (kiln CD's clusters and a
    namespace: CD syncs its apps there), `host`, `container` and `microvm` (the runners it
    deploys on, a routing constraint: `label deploy-web`, `os == linux && label web`) and
    `platform` (one of kiln's integrations). In `.kiln/environments.yaml` they are `kind`,
    `clusters` (CD's key too), `namespace`, `runners` and `integration`; without `kind`, the
    targets say it. Made in kiln, through the API, `kiln env create staging --kind kubernetes
    --cluster dev --namespace web` and the environment's page; a kind without its targets, a
    constraint that doesn't parse or a kind there isn't is refused, saying why.
  - A host environment's deploying jobs run on its runners; a container one's on those of them
    with Docker, a microVM one's on those that run Firecracker or shards: the constraints join
    the leg's routing as its gate lets it go (and the plan preview's), so a leg no runner of
    them can take waits, unroutable, saying which. A platform environment's jobs get its
    integration's credentials, as `integrations:` would give them; one the repository may not use
    fails the job before it starts, saying so.
  - The starter set: `dev`, `staging` (the trunk only) and `production` (the trunk only, once
    whoever made the set approves) in one step for a repository, each unless it has one of that
    name (`POST /api/ci/environments/starter`, `kiln env starter`, `ci.environments.starter`, and
    on the board).
  - An environment's page (`/deployments/environments/:name?repo=`), in place: its kind and
    where (pickers of kiln CD's clusters, the runners' labels and kiln's integrations), who may
    deploy and when (reviewers, the self-review rule, the wait timer, branches and tags, the
    environment it comes after and for how long, the concurrency limit, the URL; its windows and
    gates kept as written), what a deployment from the trunk would meet once saved, what waits to
    deploy there, kiln CD's apps there, its deployments, and its secrets and variables. One from
    the file is read only, saying where it's changed; one made here, or named by a job, is
    removed here.
- *Done.* R02's `container` deployments (the runner's `deploy_containers`). In a job bound to a
  `container` environment, `kiln/deploy@v1` with `compose:` (a Compose file in the workspace) or
  `images:` (one a line, each a service of its own) deploys on the runner the environment's
  routing chose, as a Compose project named for the repository and the environment (`docker
  compose up -d --wait --remove-orphans`, so the next deployment replaces it; output `project`):
  - The step waits, up to its `timeout`, for every service to run and, where it has a health
    check, to be healthy. Its log lists each service's image, state and health, and on failure
    their last lines of log.
  - The containers outlive the job. A job deploys only to the environment it's bound to.
  - Checked end to end (`tests/deploy_containers.rs`, AC03's `container` part): a Compose
    stack whose health check passes after two seconds came up healthy and ran on after its job;
    a job asking for an environment it isn't bound to was refused.
- *Done.* R02's `microvm` deployments, on Firecracker (the runner's `deploy_microvm`,
  `firecracker::deploy`, `kiln guest serve`). In a job bound to a `microvm` environment,
  `kiln/deploy@v1` with `image:` deploys it on the runner the environment's routing chose, as a
  microVM of its own that outlives the job, named as a container one's project is:
  - Its root file system is the image's, made as a leg's is (W13-R05). In there a manager runs
    the image's service (its entrypoint and command, `command:` in place of the command, as
    Docker's), as the image's user, in its working directory, with its environment and `env:`;
    it runs it again when it ends, waiting longer each time it ends soon after starting.
  - Its health is `health:` (a URL it answers, or a command), else the image's `HEALTHCHECK`, else
    that it has run two seconds. The step waits for it, up to `timeout`, saying each change and
    what the service writes.
  - This runner's `ports:` (`8080:80`, `127.0.0.1:8080:80`) reach it over vsock. Its way out is
    an egress proxy of its own with the job's policy (W18-R08), as a leg's. `vcpus:` and
    `memory:` size it.
  - Once it's healthy it takes the ports from the deployment before it, which ends; one that
    doesn't come up healthy ends, and the one before goes on.
  - Its record is kept in the runner's cache, readable by the runner alone (its environment may
    hold secrets), and the runner boots it again when it starts. It ends with the runner. One
    that ends by itself is booted again, unless it ended within a minute of booting.
  - Checked end to end (`w15_r02_a_microvm_environment_deploys_an_image`, AC03's `microvm` part):
    busybox's `httpd` deployed came up healthy and answered on the runner's port after its job.
    The next deployment took the port over with the first's microVM ended, and the last ended
    with its runner.
- *Done.* R09, the board (`/deployments/environments`; `GET /api/ci/environments/board`,
  `ci.environments.board`): every environment of every repository the caller may read runs of, by
  repository, a row each: what it deploys to and its rules; what runs there, its latest deployment
  (state, commit, ref, who, its run and URL) and kiln CD's apps there that the repository's deploy
  file makes (revision, health, drifted when out of sync, who synced it); and what waits to deploy
  there (each leg held for a review, a wait timer, a window, a promotion, a gate or its turn, with
  why). The deploy API's environments keep their own board (Releases).
- Checked by `tests/hosting.rs` (`w15_r08_kinds_the_starter_set_and_the_board`: the starter set
  made and kept, kinds saved and refused, a kind from the file, a host environment's job on the
  runner its constraint names and another's waiting unroutable, a platform environment's missing
  integration failing its job, the board's deployments and waits, and someone who reads another
  repository seeing none of them) and in Chromium and WebKit, desktop and phone
  (`ci/ui/environments_check.mjs`: the board, a starter set made there, a kind changed in place
  and read again, the file's environment read only, production's wait, history and secrets).
- *Done.* R07, CI's half (`crates/kiln/src/ci/previews.rs`):
  - `previews:` in `.kiln/environments.yaml`, a key of its own (never an environment's name):
    each entry an environment's keys (its kind and targets, protection, `url`, `concurrency`), and
    `per: stack | diff` (one for each stack, at its top diff, by default), `on: [diff,
    pull_request]` (both by default) and `ttl` (72 h by default).
  - A leg naming one, in a run for a diff, a change request or a pull request, is bound to that
    change's preview as it's queued: `preview-D12` (`preview-CR7`, `preview-PR42`; a stack's is
    named for its lowest diff), made then with the entry's rules. `${{ preview.id }}` (and
    `.name` and `.number`) fill its `url`. The job gets `KILN_PREVIEW` and `KILN_PREVIEW_ID`, and
    its `kiln/deploy@v1` takes the entry's name for the preview, deploying as that environment
    (a Compose project or microVM of its own). A stack's lower diffs' legs are skipped, saying
    whose preview it is; a run for no change fails the leg, saying why.
  - A new version's run deploys to the same preview, and each deployment pushes its `ttl` out.
  - It goes when its diff lands or is abandoned (a stack's once none of its diffs is open), when
    its change request or a host's pull request closes, when its `ttl` passes without a
    deployment, or with Remove on its page (`DELETE /api/ci/environments/:name`). Its deployments
    are marked inactive. A `container` or `microvm` one's are taken down on the runners that made
    them (`ServerMsg::Undeploy`, `CAP_UNDEPLOY`), now or when each next connects, until it says it
    has; other kinds' are their jobs' to take down, on the change's closing event. It's kept an
    hour for those closing runs, which bind to it; no other run deploys to it any more, and one
    that was deploying as it went is taken down again after it.
  - The board and the preview's page say what it's for (linked to its review) and when it goes,
    or why it went; its settings are its entry's, changed in the file.
  - Checked end to end (`tests/previews.rs`, AC04 for a `container` preview): D1's job deployed
    `preview-D1`, a Compose project of its own, with its URL; D1 v2 updated it in place; landing
    D1 marked its deployment inactive, and its runner took the containers down. A push's job
    naming the preview failed, saying why. The board and the preview's page in Chromium and
    WebKit, desktop and phone (`ci/ui/environments_check.mjs`).
- *Left.* A `microvm` environment's deployments on shards (W13's shards); cd.md's previews, an
  app of each change in a `kubernetes` preview (CD's); AC02, AC03's other parts and AC05, which
  need the live cluster (W22-AC05). AC03's `container` and `microvm` parts and AC04 are checked
  here.

### W16 Test results, flaky tests and benchmarks

**Scope.** Test results and benchmark metrics as data: ingestion, history, flaky detection,
opt-in quarantine, timing data for shards, regression tracking and opt-in gates (G-20, G-29).

**Requirements**

- **W16-R01 Formats.** JUnit XML in its common dialects (Surefire, pytest, jest-junit,
  go-junit-report, nextest), TAP, `go test -json`, libtest JSON (`cargo test -- -Z
  unstable-options --format json` and nextest's), TRX, xUnit, Mocha JSON, CTRF JSON. Ingest with the
  step key `test-results: { path, format? }` (kiln dialect, overlays), `uses:
  kiln/test-results@v1`, or opt-in discovery of the usual paths. Reports go to the blob store and
  are parsed on the server (`kiln_ci::results`).
- **W16-R02 Storage** (migration 8): `test_cases` (`repo`, `id` = hash of suite + name + file,
  `suite`, `name`, `file`), `test_results` (`case`, `leg`, `attempt`, `outcome` passed | failed |
  skipped | error, `duration_ms`, `message`, `trace` (first 8 KiB), `ts`), rollups per case per
  branch (`runs`, `fails`, `flips`, `p50_ms`, `p90_ms`).
- **W16-R03 In the UI.** A Tests tab on runs and legs (failures first, each with its message,
  trace, history sparkline and flaky mark), failure annotations at their file and line, and a tests
  page per repository (slowest, most failing, most flaky).
- **W16-R04 Flaky classification**, explained on each test: `flaky` when the same test on the same
  input hash (W08-R10) both passed and failed (across attempts or legs); `probably flaky` when its
  flip rate on trunk over the last N runs exceeds a threshold with no relevant change (N and the
  threshold configurable).
- **W16-R05 Quarantine** (opt-in): trunk's `.kiln/quarantine.yaml` lists tests (`{ test, reason,
  issue?, until? }`). A step or job with `quarantine: enforce` whose only failures are quarantined
  tests succeeds with a warning annotation; quarantined tests still run and are still reported.
  Agents may propose quarantine changes as diffs (W20); entries past `until` stop applying and are
  flagged.
- **W16-R06 Timing for shards.** Per-item durations (by test and by file) feed W04-R14.
- **W16-R07 Metrics.** Steps write JSON lines `{ name, value, unit, better: lower | higher, tags }` to
  `$KILN_METRICS`; converters cover criterion (`target/criterion/**/estimates.json`), hyperfine
  JSON, `go test -bench`, pytest-benchmark, Google Benchmark JSON, libtest bench output and k6
  summaries (`kiln metrics <tool> <path>`). Stored per leg with the runner's identity and
  fingerprint (CPU model, kernel, executor).
- **W16-R08 Regressions.** Each metric is compared with its baseline (the median of the last N
  trunk runs on runners with the same fingerprint) by a bootstrap 95 % interval of the ratio
  (`kiln_ci::stats`); a change is reported only when the whole interval clears the threshold,
  otherwise "no clear difference" (the experiments method of W20).
- **W16-R09 Gates** (opt-in): `bench-gate: { metric: "<glob>", max-regression: 5%, same-fingerprint:
  true, action: fail | warn }` on a job; the result shows on the run, the diff and the land queue.

**Acceptance criteria**

- **W16-AC01 Parsers.** Fixture reports for every format, including malformed ones (errors, not
  crashes), produce the expected cases and outcomes. *Unit test.*
- **W16-AC02 Flaky.** A test failing on attempt 1 and passing on attempt 2 is `flaky`; a flip-rate
  fixture yields `probably flaky`; a test that fails consistently after a change is not flaky.
  *Unit test.*
- **W16-AC03 Quarantine.** With `enforce`, quarantined-only failures succeed with a warning; an
  extra unquarantined failure fails; an expired entry is ignored and flagged. *Integration test.*
- **W16-AC04 Benchmarks.** A synthetic 10 % slowdown with low noise is flagged; a 2 % slowdown with
  high noise is "no clear difference"; a gate at 5 % fails the first and passes the second.
  *Unit test with seeded data.*
- **W16-AC05 End to end.** kiln's own `bench.yaml` on an exclusive runner records criterion
  metrics and shows trends per fingerprint. *E2E.*

**Progress.** In progress: R01 (formats and `test-results:`), R02, R03 (the run page and
annotations), R04 to R09; AC01 to AC04.

- *R01* (`kiln_ci::results`): the formats are told apart by their first bytes, and read by a
  small XML reader of kiln's own (no dependency):
  - JUnit XML, in Surefire's, pytest's, jest-junit's, go-junit-report's and nextest's dialects
    (with Surefire's and nextest's retries);
  - TAP 12 to 14, with subtests, YAML diagnostics, `SKIP` and `TODO`, and `Bail out!`;
  - `go test -json` (a package that doesn't build is an error) and libtest JSON (cargo's and
    nextest's);
  - TRX, xUnit v2, Mocha JSON and CTRF.

  The step key `test-results: { path, format? }` (kiln dialect and overlays) takes patterns as
  `@actions/glob` reads them, from the step's working directory. Each report goes to the server
  (`PUT /_kiln/test-results`, with the leg's runtime token), which keeps it in the CAS and parses
  it. The step's log says what it found.
- *R02*: `test_cases`, `test_reports` and `test_results`. A result records the run's attempt, the
  leg's key and the commit, which together are a test's input. Rollups (runs, failures, p50, p90
  per branch) are computed when read.
- *R03*:
  - Failures are annotated at their file and line, with paths made relative to the workspace;
    quarantined ones are warnings.
  - The run page has a Tests panel: failures first, with message, place, trace, and quarantined
    and flaky marks; what passed is folded.
  - `GET /api/ci/runs/:id/tests`, `/api/ci/legs/:id/tests`, and
    `/api/ci/repos/:repo/tests?sort=failing|slowest|flaky`; a test's history on a branch,
    newest first, with `GET /api/ci/repos/:repo/tests/:id/history` (`ci.tests.history`).
- *R04*: a test is `flaky` when it passed and failed on the same input (across attempts) or a
  tool's retry passed it. It is `probably flaky` when it flips in more than 20 % of its last 30
  runs; a change that breaks it for good doesn't count.
- *R05*: `.kiln/quarantine.yaml` entries name a `test` (by id, name, or suite and name, with `*`
  wildcards), a `reason`, an `issue` and an `until`.
  - A job's `quarantine: enforce` passes a step whose failed tests are all quarantined, with a
    warning; its `outcome` stays `failure`.
  - Quarantined tests still run and are reported.
  - An entry past `until` stops applying and is flagged on the step.
- *R06*: each run gets a timing table, which feeds `balance: timing`. It holds test files' and
  tests' median durations over two weeks, cached for ten minutes, and is saved with the run so a
  replay shards alike.
- *R07* (`kiln_ci::results::metrics`):
  - Steps write `{ name, value, unit, better, tags, samples? }` lines to `$KILN_METRICS`.
  - `kiln metrics <tool> <path>` converts criterion (its directory, with `sample.json`'s
    per-iteration times), hyperfine, `go test -bench` (each measure, `-count` runs as samples),
    pytest-benchmark, Google Benchmark, libtest's bench output and k6's summary.
  - The runner sends the points with its fingerprint: OS and architecture, CPU model, kernel and
    executor. They are kept per leg (`metrics`).
- *R08* (`kiln_ci::stats`): each point is compared with its baseline, the default branch's last
  ten runs on runners of the same fingerprint (three at least).
  - The comparison is a seeded bootstrap 95 % interval of the medians' ratio.
  - A change is reported, as an annotation, only when the whole interval clears 5 %; otherwise
    there is "no clear difference".
- *R09*: `bench-gate: { metric, max-regression, same-fingerprint, action }` fails (or warns
  about) a leg whose gated metrics got worse by more than it allows, with the reason.
  - `GET /api/ci/runs/:id/metrics` and `/api/ci/repos/:repo/metrics?name=` (a metric's values
    by fingerprint over time).
  - The run page has a Benchmarks panel.
- *Verified end to end*:
  - three runs at 1,000 ns made a baseline; a fourth at 1,100 ns was "9.7% worse (95% interval
    9.0% to 9.9%)" and its `bench-gate` at 5 % failed the leg; an unchanged metric showed no
    clear difference;
  - a step that exited 1 with one quarantined failure passed, with the run, and showed the test
    at `src/calc.rs:12`;
  - its rerun (the test passing on attempt 2) marked it flaky;
  - one more failure than quarantine covers failed the step, and an expired entry was flagged;
  - the Tests and Benchmarks panels in Chromium and WebKit, at desktop and phone widths.
- *Done.* R01's other two ways in:
  - `uses: kiln/test-results@v1` (`path`, `format`, `fail-on-failure`; W07-R10);
  - opt-in discovery. A kiln job's `test-results: discover` adds a `kiln/test-results@v1` step
    with `discover: true` after its own steps, run unless the job was cancelled. It looks where
    test tools usually write reports (`kiln_ci::results::discover::USUAL`: `TEST-*.xml`,
    `junit*.xml`, `test-results/`, TRX, CTRF; not under `node_modules`, `vendor` or `.git`). It
    takes only reports the job wrote: newer than the leg's start, and not tracked by git, so a
    checkout's fixtures stay out. A discovered failure doesn't fail the step (the tests' own
    step already did).
  - Checked end to end (`w16_r01_discovered_test_reports`): a step wrote nextest's report and
    exited 1. The run failed, and its tests were the report's `adds` (passed) and `divides`
    (failed), without the fixture report the checkout brought.
- *Done.* R03's tests page per repository and R07's trends, on Insights:
  - A repository's Insights page has a Tests panel: its default branch's tests over the window,
    the most failing, the slowest or the flaky first, each with its runs, failures (and their
    share), p50 and p90, and its flaky mark with why.
  - Its Benchmarks panel lists the metrics the default branch recorded. Each opens the metric's
    page: one chart per kind of runner (fingerprint), the latest first, each spanning the window,
    its values in the unit that suits them (criterion's nanoseconds as µs or ms), with a table of
    its points (when, the commit and its run, the value). The run page's Benchmarks panel links
    each metric to its trend.
  - Fingerprints name the CPU model and kernel on every platform, as the runner's facts do
    (`sysinfo`). They were read from `/proc`, so a Mac's, a Windows machine's and an Arm board's
    were `os/arch · executor` alone, and different machines' points shared a baseline.
- *Done.* R09's result on the diff and in the land queue: a diff's runs carry their jobs'
  `bench-gate` verdicts (failing or warning, from each leg's latest attempt), shown beside its
  checks; a land queue entry carries its candidate's, and an entry ejected by a failing gate says
  which metrics got worse (`w16_r09_bench_gate_on_the_diff_and_the_land_queue`).
- *Left*: AC05, kiln's own `bench.yaml` on an exclusive runner.

### W17 Debugging

**Scope.** A terminal into a running or kept leg, keeping a failed leg's environment, debug
reruns, rerunning a failed step in place, and running workflows locally with `kiln run` (G-21).

**Requirements**

- **W17-R01 Job terminal.** "Open terminal" on a running or kept leg opens a terminal inside the
  leg's environment: on `host`, a shell in the workspace with the leg's environment (as the runner's
  user); on `docker`, an exec into the leg's container; on `firecracker` and `shards`, a PTY from
  `kiln guest` over vsock; on `k8s`, an exec into the pod. It rides the existing terminal lane
  (`LANE_TERM`), needs `runs.debug` (and `runners.terminal` for `host`), and every session is
  audited. Secrets stay masked in the terminal's output stream.
- **W17-R02 Keep on failure.** `debug: { keep-on-failure: 30m }` (kiln dialect, overlays) or
  "Re-run and keep if it fails" keeps the environment of a failed leg (processes stopped, files
  and containers or microVM kept) for that long; the leg shows the time left, "Open terminal",
  "Extend" and "Release".
- **W17-R03 Debug reruns**, GitHub-exact: "Re-run with debug logging" (W06-R13).
- **W17-R04 Rerun a step in place.** In a kept environment, `kiln debug rerun-step <leg> <step>`
  (or the step's menu) runs that step again with its original environment, streaming into a new
  section of the log.
- **W17-R05 Agents** reach the same through MCP (W20): run a command in a kept leg and get its
  output, rerun a step, release.
- **W17-R06 `kiln run`.** `kiln run [WORKFLOW] [--job ID] [--event NAME] [--payload FILE]
  [--matrix k=v …] [--secret-file F] [--var k=v] [--executor host|docker|firecracker|shards]` runs a
  workflow on the local machine against the working tree with the same runtime (W06, W07), local
  stand-ins for the runtime services (an in-process cache, artifact and OIDC service), and the same
  log output, without a server. `--server URL` instead runs it on that server's runners from the
  local working tree (uploaded as a temporary commit).
- **W17-R07 Log search** across runs (W19-R04) and "compare with the last green run" for a failed
  leg (W19-R03).

**Acceptance criteria**

- **W17-AC01 Terminals.** On each executor, a terminal opens in the leg's working directory with its
  environment, keystrokes reach it, output is masked, and it closes when the leg ends or is
  released. *E2E.*
- **W17-AC02 Keep.** A failing leg with `keep-on-failure: 5m` stays inspectable for 5 minutes, then
  is cleaned up; Extend and Release work. *E2E.*
- **W17-AC03 Rerun step.** A step fixed by editing a file in the kept workspace succeeds when rerun
  in place. *E2E.*
- **W17-AC04 `kiln run`.** The jobs of F-03 that need no secrets give the same step results with
  `kiln run` as on the server. *Integration test.*

**Progress.** In progress: R01, R02 and R04 on the `host` and `docker` executors and in
Firecracker microVMs, R01 in `k8s` pods, and R06. R03 is W06-R13's debug reruns.

- *R01, terminals* (`runner/kept.rs`, the agent's `JobTerminalOpen`, `GET /api/ci/legs/:id/terminal`):
  - A running or kept leg says what a terminal into it opens. On `host` that is a shell in its
    workspace with its environment, which also needs `runners.terminal`; in a container it is
    `docker exec` into the job container; in a `k8s` pool's pod (W13-R07) it is the pod's own
    shell, in the leg's workspace with its environment, and the leg is recorded as `k8s`, so
    `runs.debug` is enough. In a Firecracker microVM (W13-R05) it opens inside, in the leg's
    workspace with its environment, as on the host: the host runner passes the terminal's
    traffic (and a command run there, R05) to the microVM over vsock.
  - Output is masked with the leg's secrets. What could start a secret is held back until output
    pauses for 25 ms.
  - Sessions are audited, and a terminal closes when its leg ends or is let go.
  - A leg is running once it's assigned, and has its shell once its workspace and environment are
    set up: a terminal asked for in between waits for it, 60 s at most, and opens with what was
    typed meanwhile (`w17_r01_kiln_debug_terminal` asks as soon as the leg runs).
  - Runners announce it with `CAP_JOB_TTY` and offer it when `[terminal]` is enabled.
- *R02, keep on failure*:
  - `debug: { keep-on-failure: 30m }`, or "Keep failed legs" when rerunning
    (`kiln rerun --keep-failed`).
  - A kept leg's processes stop; its workspace and containers stay for that long, at most a day.
  - The run page shows the time left, Open terminal, "Keep 30 more minutes" and Release
    (`kiln debug keep|release`).
  - The server remembers kept legs across a restart. A restarted runner forgets their shells
    (their environments held secrets, and nothing is written to disk) and sweeps their workspaces
    when the keep runs out.
  - A kept leg in a Firecracker microVM keeps its microVM running, with its terminals, Extend and
    reruns in place (R04), until the keep runs out or it's released; then the microVM ends and its
    disks go.
- *R04*: `kiln debug rerun-step <leg> <step>`, `POST /api/ci/legs/:id/rerun-step`, or the failed
  step's "Rerun this step in place" runs the step again with the environment, paths and outputs
  the leg ended with, as a new section of its log ("Rerun: …"). A second rerun while one runs is
  refused.
- *Verified end to end*:
  - AC01 on `host` and `docker`: `pwd` and the environment are the leg's, an echoed secret shows
    `***`, and the terminal closes when the leg ends. In a `k8s` pod
    (`w17_r01_a_terminal_into_a_leg_in_a_pod`, against W13-R07's stand-in for Kubernetes): a
    maintainer without `runners.terminal` opens it, `$PWD` is the workspace, the environment is
    the leg's with its secret masked, and it ends with its shell.
  - A maintainer without `runners.terminal` is refused a host leg's terminal; a viewer is refused
    any.
  - AC02: kept for 5 minutes, then the workspace is removed and an open terminal closes. Extend
    works, and Release stops the containers and removes the workspace.
  - AC03: a step that failed on a missing file passed when rerun after the file was created in the
    kept leg's terminal.
  - AC01 to AC03 in a Firecracker microVM (`w17_a_kept_leg_in_a_microvm`): the failed leg kept, a
    terminal into it in its workspace with its environment and its secret masked, the file made
    there, the step rerun in place and passing, and the release ending the microVM.
  - The run page's controls in Chromium and WebKit, at desktop and phone widths.
- *R06* ships as **`kiln workflow run`**: `kiln run` was taken by reach (a command on another runner,
  `kiln run --on linux/amd64 -- make`), which machines and agents already use. Everything else is
  as written (`crates/kiln/src/ci/local.rs`, `ci/uploads.rs`):
  - The working tree becomes a commit on `HEAD` (`HEAD` itself when nothing changed), made in a
    copy of the index, so the index and the branches don't move.
  - Here, it runs on a server of its own in the same process: CI only, on loopback, with this
    machine as its one runner, and a directory that goes when the run ends. The server reads the
    commit through git's alternates, so nothing is copied, and it has the caches, artifacts and
    OIDC tokens legs talk to.
  - Every leg runs here. `runs-on` keeps only what says which OS and architecture a leg needs, and
    a leg this machine can't run, or an approval nobody here can give, says so and the run stops.
  - `--executor` picks the executor; by default legs are routed as a server routes them.
  - `--server URL` sends the commit as a thin pack over the commits the server has (whole when it
    can't take that). It is kept under `refs/kiln/uploads/` for a week, and the server's runners
    run it. A repository kiln hosts keeps it too, as its legs fetch from it, not from the mirror
    (a leg checking out an upload there was refused before).
  - Runs there are trusted only for the server's owner; anyone else's are a fork's: a read-only
    token, and only the secrets sent with them (`--secret-file`). Those stay in the server's memory,
    never on disk.
  - Logs print as they come, one line per leg; `--report FILE` writes every leg's and step's
    conclusion; the exit code is 0 when every run succeeded.
  - Also: `--job` (and the jobs it needs), `--matrix k=v` (a planner filter kept for replays),
    `--event` and `--payload` (as the plan preview makes events), `--input` and `--var`.
  - Steps record the order they started in, so the CLI and the run page show a job container's
    setup before the step it ran with.
- *R05*, agents, through MCP (W20):
  - `ci.legs.exec` runs a command in a running or kept leg, as its terminal would, and answers
    with its output and exit code (`POST /api/ci/legs/:id/exec`, `kiln debug exec <leg> -- <command>`).
  - A `JobExec` message runs it on the runner, in the leg's shell or by `docker exec` into its
    container, through a terminal session of its own, masked.
  - The rest: `ci.legs.rerun_step`, `keep` and `release`.
  - Each needs what the HTTP route needs: `runs.debug`, and on the host executor
    `runners.terminal` too. Commands are audited.
  - Verified end to end: a command fixed a kept leg, and the step rerun in place passed.
- *AC04*: `crates/kiln/tests/workflow_run.rs` runs the binary on a scratch repository, with
  uncommitted work, a secret, a variable, needs, outputs, a matrix and a failing job.
  `ci/conformance/local_vs_server.py` runs the starter workflows the server ran again here and
  compares every leg's and step's conclusion (`docs/ci-conformance-local.md`).
  - Result: **33 of 33 give the server's step results.** In two, fail-fast cancelled a different
    matrix leg on each side; which leg fails first is a race, on GitHub too.
  - Fixed on the way:
    - legs in containers get this machine's Docker, as GitHub's Ubuntu machines have one;
    - starting runs waits for actions to be pinned, which a fresh server fetches first;
    - a leg cancelled while it runs is never retried (the scheduler had retried one whose
      runner stopped with the cancel);
    - a cancelled job's post steps run to their end on time of their own (5 minutes, GitHub's
      `cancel-timeout-minutes`), on servers too: the cancel used to cut them short;
    - a signal to `kiln workflow run` cancels its runs before its runner stops.
- *R07*: log search across runs and "What changed since the last green run" on a failed leg,
  from W19-R04 and R03.
- *Done.* `kiln debug terminal <leg>`: the terminal of a running or kept leg, in this one. It
  uses the run page's route (`/api/ci/legs/:id/terminal`, as the caller, `runs.debug` and, on
  `host`, `runners.terminal`). On Unix this terminal is raw while the remote one has it, and
  is put back as it was. It follows this window's size, and Ctrl-] leaves it (the session there
  goes on). With its input piped, what was sent runs until the shell exits. Checked end to end
  (`w17_r01_kiln_debug_terminal`): a command piped into a running leg's terminal read its job's
  environment, and the terminal ended with the shell.
- *Left*: R01, R02 and R04 in shards' sandboxes, with the `shards` executor (W13-R06).

### W18 Supply chain and untrusted code

**Scope.** Provenance, signing and SBOMs for what kiln builds; policies that use them; and running
code from outside a repository's trust safely: microVMs or containers, limited egress, no secrets
(G-24, G-25).

**Requirements: supply chain**

- **W18-R01 Provenance.** For artifacts and images a job declares (`attest: { subjects: [paths or
  image refs], provenance: true }` in the kiln dialect, or `uses: kiln/attest@v1`), kiln produces
  SLSA v1.0 provenance as in-toto statements:
  - `builder.id`: `<public_url>/ci@v1`;
  - `buildType`: `https://kiln.dev/ci/workflow@v1`;
  - external parameters: the workflow path and ref, and the inputs;
  - resolved dependencies: the source commit, pinned action SHAs, image digests;
  - run details: the run URL, attempt, runner, executor and isolation.

  kiln produces them on the runner, never from inside the job's own steps, so a job cannot forge
  its own provenance.
- **W18-R02 Signing.** Keyless with Sigstore, using kiln's OIDC identity against a Fulcio and Rekor
  kiln is configured with (self-hosted instances can be deployed in kiln's cluster), or with a key
  (a cosign key file, or a KMS integration). Signatures and attestations attach to images through
  the OCI referrers API (W12) and to files as `.sigstore.json` bundles.
  `actions/attest-build-provenance` (and `actions/attest`, `attest-sbom`) sign only on github.com
  and GHE.com: `@actions/attest` accepts no other server (`GITHUB_SERVER_URL`) and checks the job's
  ID token with `token.actions.<host>`'s keys before it signs, so no Fulcio that trusts kiln's
  issuer makes them work in a kiln leg. `kiln/attest@v1` is the supported path, and the catalog
  and the leg say so; what those actions attest on GitHub, kiln verifies (`kiln attest verify`).
  *Corrected 2026-10-24:* this said such a Fulcio made the action work. @actions/attest's
  `getIssuer` (actions/toolkit `packages/attest/src/oidc.ts`, used by actions/attest v4.2.1 and
  so attest-build-provenance v4.2.2) refuses every other server.
- **W18-R03 SBOMs.** SPDX 2.3 and CycloneDX 1.6 from lockfiles: `Cargo.lock`, `go.mod`/`go.sum`,
  `package-lock.json`, `pnpm-lock.yaml`, `yarn.lock`, `uv.lock`, `poetry.lock`, requirements files,
  `Gemfile.lock` and `composer.lock` built in; others through `syft` when it is on the runner.
  Attached like attestations. Review shows the dependency changes between two versions' SBOMs.
- **W18-R04 Policy.** An environment may `require-provenance: { builder, workflows: [paths],
  branches }`: CD deploys only images whose verified provenance matches (W15-R03).

**Requirements: untrusted code**

- **W18-R05 Trust** (W03-R08): untrusted are fork pull requests, pushes and diffs from principals
  without write access to the repository, and anything an administrator marks so.
- **W18-R06 Approval** (GitHub-exact options): runs from first-time contributors, or from all
  outside contributors, wait for a maintainer's approval (`ci.untrusted.require_approval`:
  `first-time` (default), `all-external`, `none`), approved on the run in one step.
- **W18-R07 Untrusted legs**: run only on executors in `ci.untrusted.executors` (default:
  `firecracker`, `shards`, then rootless `docker`; never `host` unless explicitly allowed); get no
  secrets, a read-only `GITHUB_TOKEN`, no ID token unless allowed (W10-R10); write caches only in
  an isolated scope (they may read the base branch's caches, as on GitHub); never reuse trusted
  results or provide results trusted runs reuse (W08-R10); egress limited by default to kiln's
  registry proxies (W12) and `ci.untrusted.allow_hosts`.
- **W18-R08 Egress enforcement.** Each leg with a network policy (`network: { egress: none | open |
  { allow: [hosts and globs] } }`, kiln dialect; always for untrusted legs) gets a per-leg proxy on
  the runner (HTTP CONNECT, TLS SNI checked against the allow-list, plain HTTP by Host), and the
  executor routes all traffic through it:
  - Firecracker: the tap device's only route is the proxy, enforced with nftables on the host;
  - containers: a per-leg network whose only egress is the proxy;
  - shards: shards' own network grants, which deny everything not declared;
  - `k8s`: a NetworkPolicy plus the proxy;
  - `host`: a network namespace on Linux (opt-in); otherwise the policy cannot be enforced, so
    untrusted legs never route there.

  Blocked attempts are logged as annotations ("blocked egress to example.com:443").

**Acceptance criteria**

- **W18-AC01 Provenance.** A built artifact's provenance verifies (signature, builder,
  dependencies) with `cosign verify-attestation` or `slsa-verifier` configured for kiln; a step
  that writes a fake statement cannot make kiln sign it. *E2E.*
- **W18-AC02 SBOM.** For kiln's own repository, the SBOM lists every crate of `Cargo.lock` and
  every npm package of `ui/package-lock.json`. *Integration test.*
- **W18-AC03 Untrusted.** A fork pull request: waits for approval (first time), then runs on a
  microVM with no secrets, a read-only token (a write through the facade gets 403), no ID token,
  egress only to the allowed hosts (others blocked and annotated), caches written only to its
  isolated scope. *E2E.*
- **W18-AC04 Deploy policy.** An image without matching provenance is refused by an environment
  with `require-provenance`, and one with it deploys. *E2E.*

**Progress.** R01–R08, AC01, AC02, AC03 (in containers) and AC04 done; R04's kiln
half:
- **Attesting.** `kiln/attest@v1` takes GitHub's subjects (`subject-path` patterns,
  `subject-name` with `subject-digest`, `subject-checksums`) and kiln's (`subject-image`, whose
  digests come from the runner's engine or the reference; `subjects`, files where one matches,
  else images), `provenance` (default true) and `sbom` (`spdx`, `cyclonedx`). A kiln job's
  `attest: { subjects, provenance, sbom }` runs it after the job's steps.
- **Who writes what.** The runner reports the subjects' digests over its own channel
  (`JobEventV2::Attest`, which no step can send) and uploads the SBOM under a random name only
  that report carries (`POST /_kiln/attestations/sbom`; anything but SPDX or CycloneDX is
  refused). The server builds the statement from its own records:
  - `builder.id` and `buildType`;
  - the workflow's path and ref, and the inputs;
  - the source commit, the actions as pinned when the run started, and the images its
    containers ran;
  - the run URL, attempt, runner, executor and isolation.

  It signs DSSE envelopes with a P-256 key it made (`<data_dir>/keys/attest-signing.p8`, public
  at `/_kiln/attestation-key.pem`).
- **Where bundles go.** Sigstore v0.3 bundles go to the step (`bundle-path`, `sbom-path`,
  `sbom-bundle-path`, and `<file>.sigstore.json` beside each file) and to the run
  (`GET /api/ci/runs/:id/attestations`). GitHub's attestations API on the facade finds them by
  digest; a job's own `POST` is kept as the job's (`origin: job`), never as kiln's.
- **SBOMs.** The built-in readers cover the ten lockfile kinds, and `syft`, when it is on the
  runner, covers the ecosystems they don't. `kiln sbom [DIR] --format spdx|cyclonedx` prints one.
  Both formats validate against their JSON schemas.
- **AC01**, verified E2E with cosign 2.4.1 (`verify-blob-attestation --key <key>
  --new-bundle-format --insecure-ignore-tlog --type slsaprovenance1`, with an empty
  `--trusted-root` offline): kiln's provenance and SBOM bundles verify. These don't:
  - a changed file;
  - a step's own statement, which is refused as an SBOM upload, and which the facade keeps as
    the job's but kiln's key doesn't verify.
- **AC02** is an integration test over kiln's own `Cargo.lock` and `ui/package-lock.json`.

Untrusted code (R05–R08):
- **Trust.** A webhook's fork pull request is untrusted (W03-R08). A polled one is untrusted
  unless its head is on one of the repository's branches: polling can't see where a head lives.
  Other people's local runs are untrusted too, and so is anything `ci.untrusted.repos` and
  `actors` mark.
- **Approval.** `ci.untrusted.require_approval` is `first-time` (default), `all-external` or
  `none`. Each uses GitHub's `author_association`; for a polled pull request kiln derives it as
  GitHub does, counting the author a contributor once the default branch has a commit of theirs.
  - A run that needs approval doesn't reach the scheduler until a maintainer with
    `runs.approve` decides it, in one step (`POST /api/ci/runs/:id/approve`, `kiln approve`, the
    run page's "Approve and run"). Rejected, it is cancelled.
  - The wait survives a restart (`runs.approval`). `approval.waiting` notifies, and the audit
    log records who decided.
- **Untrusted legs** run only on `ci.untrusted.executors` (firecracker, shards, docker). Docker
  counts only when it is rootless: the runner announces `CAP_DOCKER_ROOTLESS`, unless
  `rootful_docker` allows otherwise. A rule that sends one elsewhere leaves it waiting, with why.
  Its cache saves go to `untrusted/<ref>`, which no trusted run reads; restores also read the
  base's.
- **Egress.** A leg with `network: { egress: none | open | { allow: [...] } }`, and every
  untrusted leg, sits on an `--internal` Docker network of its own.
  - Its one way out is a relay container (a static kiln, any image) that hands each connection
    to the runner's proxy. The runner dials into the relay through a port on loopback, so
    nothing in the leg connects to the host.
  - The proxy allows kiln's server and the policy's hosts (`*.example.com`, `:port`); an
    untrusted leg gets `ci.untrusted.allow_hosts`, which its own list can only narrow. It checks
    `CONNECT` targets and plain HTTP by their URLs, and a TLS client's SNI too.
  - It refuses names that resolve to the host's own or link-local addresses (metadata services)
    unless the policy names the address, and annotates each refusal ("blocked egress to
    github.com:443").
  - Such a leg gets no Docker socket. Its `options:` and volumes may not take it off its network
    (`--network`, `--privileged`, the socket, …).
  - On the host executor the proxy is advisory (`HTTP_PROXY`, with a warning), which is why
    untrusted legs never go there unless the host is listed.
- **AC03**, verified E2E with a polled fork pull request from an author with nothing merged:
  - it waits for approval, then runs on Docker;
  - no secrets, and no ID token; a write through the facade gets 403;
  - `example.com` (allowed) answers; `github.com` and the metadata address are refused and
    annotated; nothing gets out around the proxy (no DNS, no route);
  - its cache lands in `untrusted/refs/pull/1/merge`;
  - a returning contributor's fork pull request runs, contained, without waiting.

  The microVM half waits for W13's executors (shards' network grants, Firecracker's tap device).

Done since:
- attaching to images through the OCI referrers API (W12): kiln's signed provenance and SBOMs
  about an image in kiln's registry (by its public host, or `[ci.registry] hosts`) are referrers
  of it, Sigstore bundles as OCI artifacts (`application/vnd.dev.sigstore.bundle.v0.3+json`), as
  cosign's new bundle format keeps them; cosign verifies the bundle the referrers API returns
  with kiln's key (`w18_attestation_on_image`).
- the same in other registries, where an `oci` integration names the image's registry (its
  credentials kiln's way in): each bundle pushed as an artifact (its blobs, then its manifest by
  digest, the image as its `subject`); where the registry has no referrers API (no `OCI-Subject`
  in its answer), listed in the referrers tag schema's index (`sha256-<hex>`) too, once
  (`w18_r02_an_attestation_attached_in_another_registry`, against a stand-in registry).

- *Done.* R02's other signers, as `[ci.attest] signing` says (`kiln`, its own key, by default):
  - `key`: a cosign key file (`cosign generate-key-pair`'s encrypted key: scrypt and NaCl's
    secretbox, its password in `key_password_file`) or a PKCS#8 or SEC1 PEM (`key_file`);
  - `kms`: an AWS KMS key (`kms = "awskms:///<arn>"`) through an `aws` integration
    (`integration`), its public key and each signature asked of KMS (`GetPublicKey`, `Sign`,
    ECDSA P-256 over SHA-256), the private key never leaving it;
  - `keyless`: an identity token from kiln's own issuer (audience `sigstore`, the workload's
    subject) for a Fulcio (`fulcio_url`) signing certificate of a key made for that one bundle,
    with proof of possession; the DSSE envelope entered in Rekor (`rekor_url`); the bundle
    carries the certificate chain and the log entry, and kiln's key PEM is a 404 that says so.

  Checked with cosign: a key it made signs kiln's bundles, which `cosign verify-blob-attestation`
  accepts (`w18_r02_cosign_key`); and against a fake Fulcio and Rekor that check the token's
  audience and subject, the proof, and that the entry's verifier is the certificate
  (`w18_r02_keyless`).
- *Done.* R04's kiln half: `POST /api/ci/attestations/verify` (`{ digest, builder, workflows,
  branches }`, globs) answers whether kiln's own provenance about a digest, its signature checked
  again, meets the requirement: `{ verified, attestation }`, or `{ verified: false, why }`. CD's
  `require-provenance` asks it before it deploys an image (W15-R03).
- *Done.* Keyless bundles are verified as Sigstore's clients verify them (`ci::sigstore`), so a
  keyless signer's provenance meets R04's requirements too, and attestations other Sigstore users
  made (GitHub's `actions/attest-build-provenance`) can be checked:
  - the certificate chains to a Fulcio of the trusted root, each certificate valid when the log
    took the signature, the leaf for code signing, and a CT log of the trusted root signed its
    SCT (over the precertificate, RFC 6962 §3.2);
  - the DSSE envelope's signature is the certificate's key's;
  - a Rekor entry (`dsse` 0.0.1 or `intoto` 0.0.2) is of this payload, signature and
    certificate; its signed entry timestamp, which gives the signing time, is the log's; and its
    inclusion proof leads to a checkpoint the log signed;
  - the certificate names the identity asked for: its issuer, its subject (exactly or by prefix)
    and Fulcio's claims (`source_repository_uri`, `source_repository_ref`…);
  - the statement names the artifact's digest.

  The trusted root is `[ci.attest] trusted_root` (a `trusted_root.json`: the Sigstore kiln signs
  with, when it's kiln's own), or Sigstore's public instance's through its TUF repository. kiln
  ships that repository's root metadata (version 16, byte for byte as Sigstore publishes it) and
  takes each newer root, then the timestamp, snapshot and targets, as TUF's client workflow says
  (each signed by a threshold of its role's keys, none expired or older than the last taken),
  then `trusted_root.json`, kept for a day. kiln's own keyless bundles verify against it, kiln's
  issuer their identity: against a Sigstore of the test's own (a Fulcio certifying the key for the
  token's issuer, a Rekor logging and signing the entry), CD's check finds kiln's keyless
  provenance verified, and another issuer or another Sigstore's root doesn't verify it
  (`w18_r04_keyless_provenance_verifies`). `kiln attest verify <file | sha256:…> --repo OWNER/NAME
  [--signer-workflow …] [--source-ref …]` checks an artifact's attestations on GitHub (its
  attestations API, whose bundles come snappy-compressed behind `bundle_url`) or in a file
  (`--bundle`). Checked against the GitHub CLI's provenance of its 2.103.2 release (cli/cli's
  `deployment.yml`, on Sigstore's public instance), each part of it changed failing the check
  that covers it (`w18_github_attestation_*`, `w18_the_trusted_root_decides`), and the TUF chain
  against Sigstore's published metadata (`w18_sigstore_*`). Bundles from GitHub's own Sigstore
  (private repositories' attestations and its release attestations: RFC 3161 timestamps, no
  transparency log) and Rekor v2's entries are refused, saying so: kiln doesn't verify
  timestamp authorities' signatures.
- *Done.* R02's GitHub-only actions: the catalog notes `actions/attest`, `attest-build-provenance`
  and `attest-sbom` as signing only on github.com and GHE.com (`note` in `GET /api/ci/actions`),
  naming `kiln/attest@v1`; a leg running one is told so in a warning on its step, by the Node
  action that signs (not twice for a composite around it), and the action runs as asked
  (`w18_r02_actions_that_sign_only_on_github`). Checked with `kiln workflow run` on the host
  executor: `attest-build-provenance` v4.2.2's step shows the warning once, then the action's own
  `Invalid server URL: http://127.0.0.1:…`.
- *Done.* R04's CD half and AC04: an environment's `require_provenance: { builder, workflows,
  branches }` (deploy-api.md §9.1) holds a release until kiln has provenance of each artifact it
  deploys (for half an hour, its build perhaps still attesting it), refuses one whose provenance
  doesn't meet it, and lets one that does go, the attestation its evidence. On the deploy-api test
  server: a release of a file `build.yml` hadn't built yet waited, and landed once the build on
  `main` attested it; one built from `feature` was refused.

- *Done.* R03's review: a diff's page shows what its version changes in the dependencies its
  lockfiles pin, against the commit it was made on, or between two versions when it compares
  them. Each lockfile (the ten kinds kiln reads, a vendored tree's left out) lists its packages
  added, removed, and those whose versions changed (`GET /api/diffs/:id/dependencies?from=&to=`,
  `ci.diffs.dependencies`; `kiln_ci::sbom::changes`). Checked by `w18_r03_dependency_changes`, and
  on a diff's page by `ci/ui/review_ac06.mjs`.

Left:
- attaching to images in other registries;
- microVM and `k8s` containment (W13), and the host's network namespace (opt-in).

### W19 Insights, audit and notifications

**Scope.** Where time and capacity go, what changed between runs, a structured audit log, and
rule-based notifications (G-30, G-31).

**Requirements**

- **W19-R01 Insights** per workflow and job:
  - duration percentiles over time, success and flaky rates;
  - each run's **critical path** (the longest path through `needs` by duration), highlighted on
    the graph;
  - queue time against run time, broken down by wait reason (W05-R13);
  - cache hit rates and time saved (W08-R13);
  - CPU-seconds, peak memory and runner-minutes by pool and executor, and an optional cost per
    runner-minute per pool;
  - the slowest steps.

  Per runner: utilization and failure classes. Per repository: its fair share against its weight.
- **W19-R02 Storage.** Hourly and daily rollups in SQLite (migration 9); Prometheus metrics for
  live values (extending `/metrics`); optional OpenTelemetry export (W11-R09).
- **W19-R03 Compare runs.** For any two runs or legs: duration per step, environment differences,
  action SHAs, image digests, runner facts, cache hits, changed files between their commits. "What
  changed since the last green run" is one click from a failed leg.
- **W19-R04 Log search** across runs with filters (repository, workflow, branch, conclusion,
  time), backed by an index of step logs (trigram index over log segments, built incrementally).
- **W19-R05 Audit v2** (migration 9): `audit2` (`ts`, `principal`, `on_behalf_of`, `token`,
  `scope`, `action`, `target`, `result`, `ip`, `detail`), written for every change, secret use,
  terminal session, approval, land and integration exchange; today's `audit` rows migrate; the
  Audit page filters by every field; retention configurable.
- **W19-R06 Notifications** (opt-in rules): from trunk's `.kiln/notify.yaml`, server rules and
  per-user subscriptions:

  ```yaml
  - on: [run.failed, run.fixed]          # also: run.cancelled, leg.flaky, deploy.failed,
    where: "github.ref_name == 'main'"   #   deploy.succeeded, review.requested, comment.reply,
    to: [slack:#ci, email:ada@example.com]   # mention, land.ejected, runner.offline,
    throttle: 10m                        #   quota.near, approval.waiting, secret.unused
  - on: [review.requested, comment.reply, mention]
    to: [inbox, discord:team-hook]
  ```

  Channels from W11-R06 plus `inbox` (kiln's own per-user inbox page) and issue trackers (W11-R07).
  `run.fixed` is the first success after failures on the same branch and workflow. Deliveries
  retry with backoff, are signed (webhooks), and are recorded.
- **W19-R07 Outgoing webhooks** (access.md): per-scope subscriptions to any kiln event,
  HMAC-SHA256 signed, retried with backoff, with a delivery log and redelivery.

**Acceptance criteria**

- **W19-AC01 Critical path.** For fixture runs with known durations, the computed critical path
  matches the hand-computed one. *Unit test.*
- **W19-AC02 Rollups.** Rollups over a million synthetic legs equal direct computation within
  rounding. *Unit test.*
- **W19-AC03 Compare.** Two runs differing in one action SHA and one cache miss show exactly those
  differences. *Integration test.*
- **W19-AC04 Audit.** Every mutating route in the route table writes exactly one audit row with the
  right principal and on-behalf-of. *Integration test generated from the table.*
- **W19-AC05 Notifications.** Each event fires its rule; `throttle` and `run.fixed` work; Slack,
  Discord, email and webhook channels deliver (local fakes, then real ones). *Integration test.*
- **W19-AC06 Search.** A string printed by one step in 10,000 runs is found in < 500 ms. *Bench.*

**Progress.** Done: R01 to R07, and AC01 to AC06 (AC05 against local fakes; the real channels
with W11-AC03's tenants).

- *R05* (`crates/kiln/src/audit.rs`, migration `20261018-2200_w19_audit2.sql`):
  - **One row per change.** Every request that changes something writes exactly one `audit2`
    row, refused or not:
    - who (`principal`, and `on_behalf_of` for a token or agent acting for a user), with which
      `token`;
    - the rule's `scope` and the route (`action`), the path (`target`);
    - the status (`result`) and the address (`ip`).

    So do operations through MCP and code mode (W20), which run handlers directly.
  - **Handlers' notes.** What a handler records on the way (`Store::audit`) becomes the `detail`
    of its request's row, not a row of its own. The request runs inside a slot, a task-local
    that a wrapper sets around each poll, which collects them.
  - **Rows of their own** come from outside requests:
    - a leg given secrets (`ci.secrets.use`, by name);
    - an integration's credentials exchanged for kiln's ID token (`integration.exchange`: AWS,
      GCP, Azure);
    - terminal sessions, and the other background actions.
  - Today's v1 rows moved in.
  - `GET /api/audit` filters by every field (exact values, comma-separated for any of), `q`
    anywhere, `since`, `until`, and `before` for paging. Rows are kept `[retention] audit_days`
    (400).
  - The System page's audit log filters by window, by facets (who, for whom, action, scope,
    result, address, token) and by words.
  - **AC04** (`crates/kiln/tests/audit.rs`): a real server, generated from the route table.
    Every rule that changes something, called by an agent token acting for a user that may only
    look, wrote exactly one row with that principal and on-behalf-of, the route, the path and the
    status. A change let through wrote one row too, its handler's note as the detail.
  - **Verified end to end:** 356 v1 rows migrated. A variable set through the API wrote one row
    with every field. The page filtered it in Chromium and WebKit, desktop and phone.

- *R04* (`crates/kiln/src/ci/logsearch.rs`, migration `20261018-2100_w19_log_search.sql`):
  - **The index.** Finished legs' step logs go into `ci_log_index`, SQLite's FTS5 with the
    trigram tokenizer: any substring of three characters or more, case-insensitive.
    - Logs go in as 100-line segments, escape codes stripped, a rewritten line as its last
      version.
    - A sweep indexes what isn't in yet, in the order legs finished: every 30 seconds, and as soon
      as a leg is done. So the index builds incrementally and catches up after a restart.
    - `ci_log_indexed` keeps each leg's rowid range, so pruned legs leave the index with their
      logs.
  - **The API:**
    - `GET /api/ci/logs/search?q=&repo=&workflow=&branch=&conclusion=&days=&limit=`: matching
      lines across runs, newest first, each with its run, leg, step, line number and where the
      match is;
    - `GET /api/ci/legs/:id/logs/search?q=`: one leg's lines, read from its files, so a running
      leg's too;
    - `GET /api/ci/legs/:id/logs/failure`: the failed step's log from just before its first line
      that reads like an error (else its end), with its exit code. A line that starts as one
      (`error:`, `error[`, `--- FAIL`, a traceback) or says it panicked comes before one that only
      has an error's words, and a log record at a lower level (`level=info msg="fetch failed"`,
      `[INFO]`, `warning:`) or a test runner's progress (`=== RUN`, `--- PASS`) is neither: an
      info line 3,000 lines before buildx's failed test had been the excerpt;
    - operations `ci.logs.search`, `ci.legs.search`, `ci.legs.failure_excerpt` and
      `ci.runs.compare` (W20), which the `kiln-ci-diagnose` skill now uses.
  - **The page** (`/logs`, from Runs) takes the text, repository, branch, conclusion and window.
    Each hit opens its leg at its step (`?step=`).
  - **AC06** (`cargo test -p kiln -- --ignored w19_ac06`): among 10,000 runs, 20,000 legs and
    60,000 step logs, a string one step printed is found in under a millisecond, against a target
    of 500 ms.
  - **Verified end to end** on a local server:
    - 878 legs (5.8 MB of logs) indexed in two seconds;
    - a search found its line in 3 ms;
    - the conclusion filter narrowed it;
    - two-character queries were refused;
    - a failed leg's excerpt named its step and exit code;
    - the page found the lines and opened a hit's leg at its step, in Chromium and WebKit,
      desktop and phone.

- *R03* (`crates/kiln-ci/src/compare.rs`, `crates/kiln/src/ci/compare.rs`, migration
  `20261018-2000_w19_compare.sql`):
  - **Leg facts.** Each leg reports what it runs on to `POST /_kiln/facts`, kept on its attempt:
    - the machine (OS and architecture, CPU, kernel) and the runner's version;
    - the executor;
    - its containers' images, as named and by content-addressed id;
    - the job's environment, with secrets masked.
  - **What differs.** `kiln_ci::compare::differences` lists only what differs categorically:
    - actions, by name, so a moved ref or a new version is one difference;
    - variables;
    - legs one run lacks;
    - each leg's conclusion, runner, executor, machine, runner version, images, environment and
      cache (hit or miss by key prefix).

    A runner that didn't report its machine adds no differences. Durations aren't differences:
    `steps` pairs each leg's steps by name, an action's ref aside, for reading side by side.
  - **The API:**
    - `GET /api/ci/runs/:id/compare/:other` and `GET /api/ci/legs/:id/compare/:other`, within
      one repository;
    - `:other` is a run (or leg) or `last-green`: the last run of the same workflow and branch
      that succeeded before this one;
    - each returns the differences, the files changed between the commits (from the server's
      mirror) and the legs' steps side by side.
  - **The page** (`/runs/:id/compare`, W21) holds what differs, the changed files and each leg's
    steps with their change. It opens from a run's "Compare with last green", and from a failed
    leg's "What changed since the last green run": one click.
  - **AC03** (a unit test of the comparison): two runs differing in one action SHA and one cache
    miss show exactly those two differences, whatever their durations.
  - **Verified end to end** on a local server. Bumping `actions/cache@v4` to `@v4.1.2`, adding a
    job variable and changing the cache key showed exactly three differences: the action (both
    SHAs), the variable, and the cache's hit then miss. The two changed files were listed. The
    page opened from a failed leg in one click, in Chromium and WebKit, desktop and phone.

- *R01, R02* (`crates/kiln-ci/src/insights/`, `crates/kiln/src/ci/insights.rs`, migration
  `20261018-1900_w19_insights.sql`):
  - **Rollups.** `kiln_ci::insights::Rollup` keeps one bucket's facts and merges exactly:
    - counts by conclusion, flaky (passed after a failed attempt) and retried legs;
    - durations and queue times as a log-bucketed histogram, with quantiles within 1 %;
    - CPU-seconds, peak memory and runner-seconds (every attempt);
    - queue seconds by wait reason, and failure classes.
  - **Rolling up.** Every minute, each hour that has passed is rolled up from the runs, legs and
    steps that finished in it, one row per repository, workflow, job, step, executor and runner.
    - A day is its hours merged. Hours are kept 35 days, days for good.
    - A first start catches up 96 hours a minute.
    - What finished since the last rolled-up hour is read live, so insights are current.
  - **What feeds them:**
    - each leg's queue time by wait reason (`jobs.waits`, closed whenever the reason's kind
      changes, the leg starts or it ends);
    - every attempt in `ci_attempts`, with its runner, conclusion and failure class (nothing
      wrote that table before);
    - cache lookups and saves (W08-R13);
    - each runner's connected, slot and busy seconds per hour, sampled every minute.
  - **The API**, `/api/ci/insights/…`, over `?days=` (30 by default; hours for two days or
    `?grain=hour`):
    - `repos`: each repository's runs, runner time, share of all runner time, and its
      `[ci.fairness]` weight's share;
    - `repos/:repo`: its workflows, with series;
    - `workflows/:repo?workflow=`:
      - its runs and series (p50, p90, p99, success and flaky rates, queue time);
      - each job's legs with waits by reason, failure classes, CPU, peak memory and runner time;
      - the 20 slowest steps;
      - executors and pools (a runner's `pool:` label) with an optional
        `[ci.insights] cost_per_minute` per pool;
      - the cache by key prefix;
      - the latest run's critical path;
    - `runners` and `runners/:name`: utilization (busy over connected slot-time), legs, failure
      classes, and whose legs they ran.
  - **The critical path** is `kiln_ci::insights::critical_path`: the longest path through
    `needs` by the jobs' spans. The run graph uses it.
  - **Prometheus** (`/metrics`):
    - `kiln_ci_cache_total{result}`;
    - `kiln_ci_attempts_total{conclusion,class}`;
    - `kiln_ci_wait_seconds_total{reason}`;
    - `kiln_ci_rollup_lag_seconds`.
  - **Tests.** AC01 checks hand-computed paths: a diamond, a long job against a chain, ties, a
    fan-in, unknown needs, a cycle, 200 jobs in a chain. AC02 checks a million synthetic legs,
    rolled up by the hour, merged into days and then the month, through their stored JSON:
    - counts, failure classes and peak memory exactly;
    - sums to rounding;
    - the 1st to 99.9th percentiles within 1 % of the sorted values.
  - **Verified end to end** on a local server, with a workflow built to produce each fact:
    - capacity, concurrency and retry waits, by reason;
    - the flaky job's two attempts, one failure classed;
    - a cache miss and save, then a hit, with 5 s saved;
    - the runner's utilization (0.92);
    - each repository's share against its weight;
    - the Insights pages (W21-R12) in Chromium and WebKit, desktop and phone.

- *R06* (`crates/kiln/src/ci/notify.rs`):
  - Rules come from trunk's `.kiln/notify.yaml` and the server's `[[ci.notify]]`, as written
    above: `on`, `where` (an expression over `github` and `run`: `repo`, `id`, `number`,
    `workflow`, `name`, `branch`, `sha`, `message`, `actor`, `conclusion` and `url`), `to` and
    `throttle`. The hub's event for each carries the same `run`, for kiln's own subscribers.
  - Events so far: `run.failed`, `run.fixed`, `run.succeeded`, `run.cancelled` and
    `approval.waiting`, announced once per leg; an agent's `agent.needs_you`,
    `agent.finished`, `agent.stopped` and `agent.paused` (docs/notifications.md), which only the
    server's rules take, `where` reading `agent`; and a release's `release.started`,
    `release.waiting_approval`, `release.succeeded`, `release.failed` and `release.cancelled`
    (docs/deploy-api.md §12.3), which only the server's rules take, `where` reading `release`,
    and which go to the app's own channels too: where a channel can change a message afterwards
    (a Slack bot, a Discord webhook, a Telegram bot), as the one message the release has there,
    edited as it moves; elsewhere once per release, event and channel.
  - Channels are W11-R06's integration kinds (`slack`, `discord`, `teams`, `telegram`,
    `pagerduty`, `opsgenie`, `webhook`), named `kind:target`: the integration called `target`,
    or the first the repository may use, with `target` as its channel or chat.
    - Slack takes an incoming webhook or a bot token.
    - PagerDuty triggers an incident on a failure, and resolves it on the fix.
    - Webhooks are signed with `X-Kiln-Signature-256` (HMAC-SHA256 of the body).
  - Deliveries retry after 2 and 15 seconds and are recorded (`GET /api/ci/notify/deliveries`).
    An integration's Verify sends a test message.
  - `kiln/notify@v1` sends a step's own message (W07-R10).
  - Verified end to end, against a local fake:
    - `run.failed` reached Slack and a webhook whose signature checked;
    - the fix sent `run.fixed`;
    - a failure again within the throttle sent nothing;
    - a deployment waiting for a reviewer sent `approval.waiting` to Discord;
    - a step's `kiln/notify@v1` reached both channels.
- *R07* (`crates/kiln/src/ci/webhooks.rs`, migration `20261018-1800_w19_webhooks.sql`):
  - A subscription is a `webhook` integration with `events`:
    - `run`, `job`, `agent`, `queue`, `cd_plan`, `cd_apps` and `cd_app_deleted`, as kiln's own
      clients hear them;
    - R06's `run.failed`, `run.fixed`, `run.succeeded`, `run.cancelled` and `approval.waiting`;
    - `machine.battery_low`, and an agent's `agent.needs_you`, `agent.finished`, `agent.stopped`
      and `agent.paused` (docs/notifications.md), to server-wide subscriptions;
    - or `*`, which is all of them but the busy `queue`.

    Naming anything else is refused. Sign-in flows, terminals and host samples never leave.
  - Its scope decides whose events it gets: the server's (all of them, runners and deployments
    included), an organization's repositories', or one repository's.
  - Each event is POSTed as JSON with `X-Kiln-Event`, `X-Kiln-Delivery` and
    `X-Kiln-Signature-256` (HMAC-SHA256 of the body with the integration's secret).
  - Delivery is in order for each subscriber, by a worker of its own:
    - each attempt uses the integration as it is by then;
    - attempts come after 0, 2, 15 and 60 seconds;
    - 1,000 events may wait; beyond that they are logged as `dropped`.
  - Every delivery is logged, with its body, before it is sent (14 days):
    - a restart resumes the ones still `pending`;
    - `GET /api/ci/webhooks/deliveries[/:id]` lists them;
    - `POST …/:id/redeliver` sends one again, with the same delivery id.
  - Verified end to end, against a local fake:
    - run and job events reached a server-wide subscriber, every signature checked;
    - an `org:other` subscriber got nothing;
    - a subscriber answering 500 was tried four times per event, and logged `failed`;
    - a restart mid-retry resumed the 22 pending deliveries;
    - pointing that subscriber at a working URL sent the rest there;
    - redelivering a failed delivery sent it there with its own delivery id;
    - `run.failed` and `run.succeeded` arrived with their repository.
- *Done.* R06's other events:
  - `deploy.succeeded` and `deploy.failed`: a job's deployment to an environment ended. Its
    environment is named, and a PagerDuty incident is the environment's, resolved by the next
    success.
  - `leg.flaky`: as a run ends, the tests it was flaky on (W16-R04's first rule, on its own
    results: a tool's retry passed them, or one attempt failed them and another passed them),
    named in the message.
  - `land.ejected`: diffs taken out of the land queue, with why, linking the first.
  - `runner.offline`: a runner (not a pod or an outpost) gone two minutes, once each time it
    goes. Only the server's rules take it, `where` reading `runner` (`name`, `labels`,
    `gone_for`), and it reaches the server's webhook subscribers.

  They're webhook subscription events too (R07). Unit-tested
  (`w19_r06_deploy_land_and_runner_events`), and end to end
  (`w19_r06_deploy_failed_and_leg_flaky_reach_a_channel`): a repository's rule sent a failed
  deployment and a report's retried test to a local webhook.
- *Done.* R06's people, email and inbox (`crates/kiln/src/ci/notify.rs`, `smtp.rs`, `inbox.rs`):
  - Events for people, about a diff or change request, done by someone (`actor`) and never to
    them: `review.requested` (the reviewers a push or a change request asks for the first
    time), `comment.reply` (a published comment: the people in its thread and the diff's
    author) and `mention` (whom a comment or a review names, `@ada`). Rules see `run` as the
    diff: `name` it and its title, `workflow` its name, `branch` its trunk, `message` what was
    said, `url`.
  - `quota.near`: a repository's cache past 90 % of its quota after eviction, once until it's
    under 80 % again. `secret.unused`: a secret no run read in 90 days (or never, 90 days after
    it was set), said again 90 days later; a repository's, a workflow's and an environment's
    go through its rules, the server's and an organization's through the server's (`where`
    reading `secret`: `name`, `scope`, `last_used_at`).
  - `email` (W11-R06): an integration of its own (`host`, `from`, `port`, `tls`, `user`, `to`,
    the password as its secret), sent by a small SMTP client on kiln's own stack: STARTTLS on
    587 by default (refused when the server doesn't offer it), TLS from the start on 465, or in
    the clear to a relay that takes mail that way (`tls: none`, never with a password); AUTH
    PLAIN, or LOGIN where that's all a server offers. `email:ada@example.com` sends to that
    address (several, comma-separated), `email` alone to the integration's `to`. A message is
    plain text in base64 lines, its headers one line each (a subject that isn't ASCII is RFC
    2047's encoded word), marked `Auto-Submitted: auto-generated`. It goes where the server may
    reach (W22-AC07), never through an HTTP proxy.
  - `inbox`: kiln's own, each person's, on the review inbox page (its Notifications, newest
    first, the unread marked, "Mark all read"). A rule's `inbox` puts an item there for the
    people an event is about (a run's actor; a release's approvers while it waits, else whoever
    asked for it; the reviewers asked; a thread's people; whom a comment mentions), `inbox:ada`
    for whom it names. `GET /api/review/inbox` has them (`notifications`, `unread`), of
    repositories the caller may read; `POST /api/review/inbox/read` (`ids`, or `all`) marks
    theirs read. A person keeps their last 500.
  - AC05 end to end (`tests/notifications.rs`, `w19_ac05_notifications`), against local fakes:
    `run.failed` reached Slack and a signed webhook; a failure again within the throttle sent
    nothing; the fix sent `run.fixed` to Discord; a diff pushed with `-o reviewer=rev` put
    `review.requested` in rev's inbox; rev's comment naming @dev put `comment.reply` and
    `mention` in dev's, and dev's answer in the thread put `comment.reply` in rev's (nobody
    heard of what they did themselves); rev read them all; each of the four reached an SMTP
    server, from kiln to the rule's address; every delivery was recorded as sent.
- *Done.* R02's OpenTelemetry export (`crates/kiln/src/ci/otel.rs`, W11-R09's `otel`
  integration kind: an OTLP/HTTP receiver's `endpoint`, a `service_name`, and a header such as
  an API key as its secret):
  - Each finished run is a trace, sent to every `otel` integration its repository may use as
    OTLP's JSON (`<endpoint>/v1/traces`): the run the root span, each leg of its latest attempt a
    span under it, each step that ran a span under its leg. Ids come from the run and attempt,
    so a rerun is a trace of its own.
  - Attributes follow OpenTelemetry's CI/CD and VCS conventions (`cicd.pipeline.name`,
    `.run.id`, `.run.url.full`, `.result`; `cicd.pipeline.task.*`, `cicd.worker.name`;
    `vcs.repository.*`, `vcs.ref.head.*`), with kiln's (`kiln.event`, `kiln.actor`,
    `kiln.queued_seconds`, `kiln.executor`, …). A failure's span is an error.
  - It reaches only where requests someone named may go (W22-AC07): a receiver on a private
    network needs `[ci] allow_private_targets`. Verify sends an empty export; each export
    records the integration's last use and error (`w19_r02_a_run_as_a_trace`).
- *Done.* R06's per-user subscriptions: a person's own rules, beside the trunk's and the
  server's (`GET` and `PUT /api/me/notifications`, `{ rules, email }`; Settings → You → CI
  notifications):
  - A rule is `.kiln/notify.yaml`'s (`on`, `where`, `throttle`) for repository events, sent to
    their `inbox` or their `email` (the address they keep there), for every repository they may
    read or those `repos` names (globs), and with `about_me` only what's about them: runs they
    started, reviews asked of them, replies to and mentions of them.
  - It takes nothing of repositories they can't read (their grants as each event comes). The
    owner's rules are the server's (`[[ci.notify]]`), and an agent doesn't change its person's.
  - A run is about whoever in kiln started it (W10's `started_by`, an agent's person), so the
    trunk's `inbox` rules reach them too; without one, its actor.
  - Checked end to end (`w19_r06_a_persons_own_rules`): dev's failing run reached dev's inbox and
    email, the owner's didn't; a channel that isn't a person's and an agent's change were refused.
- *Done.* R06's issue trackers: Linear and Jira as channels, and the `diff.landed` and
  `test.quarantined` events (W11-R07; see its notes).

### W20 Agents: MCP code mode, skills, attachments, experiments

**Scope.** Any agent registered with kiln (sessions kiln starts, attached agents, agents connecting
with an agent token) can run, control, write and manage CI fully and accurately, through the same
operations people use (G-14). Extends the MCP server that exists (`crates/kiln/src/mcp/`: code
mode in QuickJS, a tool per operation, skills) from deployments to all of CI, and adapts
[ci-agents.md](ci-agents.md) and [experiments.md](experiments.md), which it replaces.

**Requirements: the operations**

- **W20-R01 One registry.** CI operations join `cd::ops`'s registry pattern (name, summary,
  input schema, read-only, destructive, capability), generalized into `crate::ops` so that HTTP
  routes, the OpenAPI document, the CLI, the MCP tools and code mode's `kiln.*` are all generated
  from it (W10-R05). Every operation of §4 has an entry.
- **W20-R02 Code mode.** `code_search` and `code_run` gain `kiln.ci.*`:
  - `workflows`: list, get, validate, plan_preview, schema, and edit, which proposes a change as a
    diff or branch, never a direct write;
  - `runs`: list, get, trigger, cancel, rerun, rerun_failed, rerun_job, approve;
  - `legs`: get, logs (search, tail, failure excerpt), routing (explain), terminal_exec,
    rerun_step, keep, release;
  - `artifacts`, `caches`, `tests` (results, flaky, history, propose_quarantine);
  - `runners`, `pools`, `images` (build), `routing` (rules, explain);
  - `secrets` (names and usage; set with `secrets.manage`), `variables`, `integrations` (list,
    verify);
  - `environments` (list, create, deploy, approve where allowed);
  - `diffs` (list, get, comment, reply, resolve, review, submit_patch, land with a strategy);
  - `insights`, `notifications`.

  Lists are async iterators that page on the server. Bulk operations take selectors and are
  idempotent (`Idempotency-Key`). Results are limited as today (heap, time, calls, result size).
- **W20-R03 Accuracy loop.** Every write that changes a workflow, routing rule, environment or pool
  returns diagnostics (W02) and a plan preview (W04-R29) of the effect. The skills require the loop:
  validate → preview → propose → watch the run. Agents never get a "just write" path that skips
  validation.
- **W20-R04 Tools one by one** for clients without code mode (names `ci_runs_list`, …), as today
  for CD.

**Requirements: skills**

- **W20-R05** kiln ships, serves over MCP (the skills extension) and installs into every agent it
  starts:
  - `kiln-ci-author`: writing and porting workflows, both dialects, overlays, expressions, the
    validate-and-preview loop;
  - `kiln-ci-diagnose`: the failure-bundle playbook below;
  - `kiln-ci-runners`: runners, executors, pools, images and routing rules, with explain;
  - `kiln-ci-review`: stacked diffs, change requests, review etiquette, merge strategies, the land
    queue;
  - `kiln-ci-migrate`: from GitHub Actions and other CI systems, step by step;
  - `kiln-ci-secrets`: secrets, integrations and OIDC trust setup per provider;
  - `kiln-ci-performance`: insights to action (critical path, caches, shards, reuse).

  Each skill's text is versioned in the repository (`crates/kiln/src/mcp/skills/`) and tested: a
  scripted agent following it on fixtures reaches the expected result (W20-AC05).

**Requirements: identity and limits**

- **W20-R06** Agents act as `agent:<id>` on behalf of the user who started or attached them, with the
  intersection of their grant and that user's rights (W10-R01). Every call is audited under both.
- **W20-R07 Triggers.** Agents start runs through `agent` events (W03-R09) with typed inputs; a
  workflow lists which agents may.

**Requirements: attachments and incidents** (adapted from ci-agents.md)

- **W20-R08 Attaching.** In the UI (**Watched by** on a repository's panel or a runner's details)
  or in trunk's `.kiln/agents.yaml`, an attachment names:
  - a harness (Claude Code, Codex, any ACP agent, or Devin through W11-R08), or a live session;
  - what wakes it: `failure`, `ejected`, `flaky`, `stuck`, `slow`, `schedule`, `mention`, `degraded`
    (W15), `unroutable` (W05);
  - what it may do;
  - its limits;
  - its playbook;
  - who reviews its changes;
  - its isolation: `worktree`, `container`, `firecracker`, or `shards` with an Agentfile.
- **W20-R09 Capabilities**: `read`, `rerun`, `cancel`, `diagnose` (kept workspaces, terminals,
  bisect), `fix` (submit diffs), `edit-pipelines`, `quarantine`, `comment`, `experiment`,
  `manage-runners` (drain, pool sizes; opt-in), `deploy` (opt-in, per environment, subject to its
  protection) and `land` (opt-in; the land queue still enforces every review rule). Accepting a
  diff counts only where `agents-can-approve` is on. A job opts out with `agents: { rerun: false }`.
- **W20-R10 Incidents**, as ci-agents.md describes:
  1. failures grouped by signature (the failing step plus its normalized error);
  2. a failure bundle: failed steps with log excerpts, parsed test failures with their flakiness,
     changes since the last green run, environment changes, earlier incidents with the same
     signature;
  3. waking an agent with the bundle;
  4. diagnosing, with the agent classifying the failure as flaky, infrastructure, regression with
     the culprit, pipeline error or dependency break;
  5. acting within its capabilities;
  6. reporting on an incident card on the run, linked to the session.

  Limits per day (sessions, tokens, runner-minutes) and reruns per failure; one switch pauses a
  repository's agents.

**Requirements: experiments** (adapted from experiments.md)

- **W20-R11** Experiments keep experiments.md's design:
  - strategies: each alone, all combinations, pairs, greedy stacking, custom;
  - variants built from per-option patches, with glue commits on conflicts;
  - criteria: pipeline, command and metric, each required or an objective;
  - interleaved repetitions;
  - bootstrap intervals and interactions;
  - decisions (bring results back, or pick automatically);
  - budgets.

  Two things change. Measurement is ordinary CI: each variant's runs are workflow runs on routed
  runners, so benchmarks land on exclusive hardware by routing rules. Isolation options now include
  `firecracker` and `shards` (built from an Agentfile that declares the agent and its grants).

**Acceptance criteria**

- **W20-AC01 Coverage.** Every operation in §4 is reachable as an HTTP route, an MCP tool, a
  `kiln.ci.*` function and (where listed) a CLI command, generated from one registry (a test
  compares them). *Unit test.*
- **W20-AC02 Code mode.** One `code_run` program lists the failed legs of the last 100 runs of a
  repository, groups them by failure class, and returns counts, in under 5 s against the live
  server. *E2E.*
- **W20-AC03 Authority.** An agent with `read` only cannot rerun (403 through every surface); an
  agent acting for a viewer cannot exceed the viewer even with a broad grant. *Integration test.*
- **W20-AC04 Incident.** A deliberately broken commit wakes an attached agent, which diagnoses it
  as a regression, opens a revert diff that passes review rules, and reports on the incident card;
  a flaky test fixture is classified flaky and rerun. *E2E with a live harness, user-observed.*
- **W20-AC05 Skills.** Each skill's scripted scenario (fixtures in `tests/skills/`) succeeds with a
  scripted agent driving the MCP tools. *Integration test.*
- **W20-AC06 Experiments.** experiments.md's 3-option, all-combinations scenario completes with
  variants measured by a pipeline on routed runners and a recommendation with intervals. *E2E.*

**Progress.** In progress: R01's registry with CI's first operations, R02's `kiln.ci.*` for them,
R04, R05's skills, and R06's rights.

- *R01* (`crate::ops`, `crates/kiln/src/ci/ops.rs`): deployments' and CI's operations are one
  list, and the MCP tools and code mode's `kiln.*` are generated from it. Each CI operation is
  one of §4.1's routes: it runs that route's handler, and the route table's rule for that route
  decides who may. A unit test checks that every operation has a rule of its own and is read-only
  exactly when its route reads. HTTP routes, the OpenAPI document and the CLI aren't generated
  from it yet.
- *R02*: `kiln.ci.runs.{list,get,trigger,ask,compare,cancel,rerun,rerun_failed,rerun_job,approve}`,
  `kiln.ci.legs.{get,logs,search,failure_excerpt,routing,exec,rerun_step,keep,release}`,
  `kiln.ci.logs.search`, `kiln.ci.workflows.{list,get,validate,plan_preview,edit}`,
  `kiln.ci.caches.list`, and:
  - `artifacts.list`, `tests.{run,leg,repo,propose_quarantine}`, `metrics.{run,repo}`;
  - `runners.list`, `pools.list`, `images.{list,validate,propose}`, `routing.{rules,explain}`;
  - `secrets.{list,set}` (names and last use only; setting needs `secrets.manage`),
    `variables.{list,set}`, `integrations.{list,verify}`;
  - `environments.{list,save}`;
  - `insights.{repos,workflows,runners}`, `notifications.deliveries`;
  - `diffs.{list,get,threads,readiness,comment,reply,resolve,review,submit_patch,land}`, whose
    routes are W14's under `/api` (an operation names its routes' prefix).

  What `workflows.edit`, `tests.propose_quarantine` and `images.propose` change goes up as a diff
  of the file (the workflow, `.kiln/quarantine.yaml`, the image's Dockerfile or Agentfile), for
  review like any other. `workflows.edit` answers with the workflow's diagnostics and, R03's
  loop, the plan a push to the trunk would make of it once it lands.

  An operation's route is checked by its segments as the HTTP layer decodes them, so a
  repository's name (`acme/app`) is one segment, as `acme%2Fapp` is in a path.
- *R04*: the same, as tools one by one (`ci_runs_list`, …).
- *R05*: `kiln-ci-diagnose`, the failure-bundle playbook:
  - one code-mode program for the failed legs of recent runs, with their failing steps' last lines;
  - classifying the failure;
  - keeping a failed leg, running commands in it, and rerunning the step in place;
  - fixing rather than rerunning.

  And the other six (`crates/kiln/assets/skills/`), each with code-mode programs on the
  operations above:
  - `kiln-ci-author`: where workflows live and which dialect runs, kiln's keys, expressions, and
    the validate → preview → propose → watch loop as one program;
  - `kiln-ci-runners`: a leg's routing explained, where a job would go, the facts constraints
    read (by their names), routing rules, executors and pools, images validated and proposed;
  - `kiln-ci-review`: submitting by push and by patch, readiness, answering threads, reviewing
    with suggestions, strategies and the land queue;
  - `kiln-ci-migrate`: the five steps, and GitHub Actions, GitLab CI/CD, Bitbucket Pipelines,
    Jenkins and CircleCI mapped to kiln;
  - `kiln-ci-secrets`: scopes, how a leg gets secrets (and `secrets.use`), integrations, OIDC trust
    for AWS, Google Cloud, Azure and Vault;
  - `kiln-ci-performance`: the critical path, waits by reason, caches, timing shards and reuse,
    measured before and after.

  Writing them found `ci.insights.workflows` without the workflow its route needs: it takes
  `workflow` now.
- *R01's* other surfaces, from the same registry: every operation over HTTP (`GET /api/ops`
  lists each with its summary, whether it changes anything and its input's JSON Schema;
  `POST /api/ops/:name` runs one as its caller, checked as its own route is, a change audited
  once as that route's would be), in the OpenAPI document (`/api/ops/{name}`, each with its
  summary and schema), and as `kiln op` (the list, `--schema`, and a run with a JSON object or
  `key=value` pairs). Checked end to end (`w20_r01_operations_over_http_and_the_cli`).
- *AC05* (`tests/skills.rs`): the skills' programs run as written. The test takes each program
  from its `SKILL.md` and calls `code_run` with it over MCP, against a server with a repository,
  a failing run, a leg no runner can take, a diff and a secret:
  - diagnose's failure bundle names the failing step and its error;
  - author's loop checks a fixed workflow, previews its legs and proposes it as a diff;
  - runners explains the waiting leg (the runner, and the label it lacks) and says where a job
    would go;
  - review reads the diff's readiness and its open threads;
  - secrets lists names and last use, never a value;
  - performance finds the critical path and the slowest test.

  The migrate skill's mappings are prose and YAML, with no program to run. Running the programs
  found that a finished leg's routing is history (`known: false`), so the runners skill
  explains a waiting leg and says where a finished one ran.
- *R06*: `/api/mcp` takes users' sessions and tokens too, and checks each operation as its HTTP
  route is checked. The admin token, the owner's session and kiln's MCP tokens act as the owner,
  as before. Calls no longer need deployments to be on.
- *Verified end to end* (with W17-R05), over MCP:
  - an agent ran commands in a kept leg (in its workspace, with its environment and exit codes);
  - it fixed what failed, reran the step in place and saw it pass;
  - a code-mode program listed runs and read the leg;
  - a developer's token listed runs but was refused `exec` and `release` (`runs.debug`).
- *R03*: every write that changes a workflow, routing rule, environment or pool says what it
  does:
  - a workflow's edit, its diagnostics and the trunk's plan (as before);
  - routing rules (`PUT /api/ci/routing/rules`, the new `ci.routing.set`): a warning for each
    rule no runner connected now meets, and where the last week's legs would go now;
  - a pool (`PUT /api/ci/pools/:name`, the new `ci.pools.set`): a warning when no runner
    connected announces its executor, or a runner carries all its labels itself; and the size it
    heads for;
  - an environment (`POST /api/ci/environments`, `PUT` on one, `ci.environments.save`): a
    warning for a reviewer who isn't anyone here, a lone reviewer who can't approve their own
    runs, and a trunk its branches rule leaves out; and what a deployment from the trunk meets
    now.

  The dispatcher has the change before it answers. Settings → CI's routing rules and the
  Runners page's pools show a save's warnings. Checked end to end
  (`w20_r03_writes_say_what_they_do`), and in Chromium and WebKit.
- *R02's* `images.build` (`ci.images.build`, W21-R08's image builder): a runner image's
  definition, saved or not, built by `kiln/build-image@v1` where `runs_on` names, and with `push`
  pushed and registered (the server owner's only). Checked by `tests/registry.rs`
  (`w21_r08_an_image_built_from_its_editor`).
- *R07* (`POST /api/ci/repos/:repo/agent`, `ci.runs.ask`): an agent asks for a run of a workflow
  whose `on: agent` names it (or `*`), with inputs typed as `workflow_dispatch`'s are; a workflow
  that doesn't name it refuses it (403). Checked with W03-R09's events
  (`w03_r09_native_activity_and_kiln_events`).
- *R08 to R10* (`crate::ci::attach`; docs/agents.md, "Attached agents"): attachments and
  incidents, by API, under Watched by (a repository's panel, a runner's details) and in trunk's
  `.kiln/agents.yaml`. Since:
  - the rest of R08's wakes:
    - `slow`: a green run whose critical path took longer than the median of its workflow's last
      ten green runs on its branch (three at least) by more than `limits.slow_percent` (25%);
    - `unroutable`: a job no runner meets what it asks for, a minute on, one incident for each
      set of unmet requirements;
    - `degraded`: an app kiln CD deploys turning degraded wakes what looks after its environment.
  - **Watched by** on a repository's panel (its own, its workflows' and its environments'
    attachments) and on a runner's details (its own and every runner's): each one's chip, its
    line opened from it (Pause, Remove), and Attach (its name, its agent and what can wake an
    agent there), the rest kiln's defaults.
  - R10's other daily limits:
    - tokens (`tokens_per_day`, 5,000,000: its incidents' sessions together);
    - runner minutes (`runner_minutes_per_day`, 300): the runs its agents asked for, and the
      attempts they reran. kiln now records who started each rerun (`ci_run_attempts`, and
      GitHub's `github.triggering_actor`: the person, for an agent the one it acts for).

    Past either, an incident waits for a person, saying which. `.kiln/agents.yaml`'s `per-day:
    { tokens, runner-minutes }` are held to (tokens as `5M`, `500k` or a number).

  Checked by `w20_r08_unroutable_is_what_no_runner_meets`,
  `w20_r08_slow_is_its_critical_path_past_the_median`, `w20_r10_runner_minutes_a_day` and
  `w05_r17_triggering_actor_is_who_reran_it`. The end-to-end
  `w20_r08_what_happens_wakes_the_agent_attached_to_it` now has a degraded app too. A
  repository's Watched by is checked in Chromium and WebKit (`ci/ui/pages_check.mjs`).
- *AC03* (`tests/ops.rs`, `w20_ac03_authority`), on a server with a failed run. Two agents are
  refused its rerun through every surface: the route, the operation over HTTP, its MCP tool,
  code mode, `kiln rerun` and `kiln op`, each refusal naming `runs.rerun`:
  - one whose grant only reads, acting for a developer who may rerun;
  - one with the owner's grant, acting for a viewer.

  Both read the run through the same surfaces, and nothing reran it. An agent with a
  developer's grant reruns it.
- *AC01*: every route of §4.1 is an operation of the one registry, so each is an MCP tool, a
  function of code mode's `kiln` and `kiln op <name>`.
  - The operations §4.1 named that weren't, now are. Among them: access (`users.*`, `grants.*`,
    `roles.*`, `tokens.*`, `audit.list`, run by the identity routes' own handlers); the hosts'
    pull requests (`host.pulls.*`); a run's graph, summary, annotations, attestations and
    pending approvals; legs compared; working trees' packs and runs (`ci.uploads.*`,
    `ci.runs.local`); OIDC's status, keys, rotation and subjects; integrations made, read,
    changed and deleted; secrets' and variables' deletion; pools, environments, webhooks'
    deliveries, inbound hooks, providers, diffs' versions, files, blobs, interdiffs and
    actions, change requests, the review inbox, the land queue and stacks.
  - Three were missing underneath and are built:
    - a runner probes its facts on request (W13-R01: `POST /api/agents/:name/probe`, which
      waits for its new report);
    - a test's history (`GET /api/ci/repos/:repo/tests/:id/history`);
    - a deployment by hand (`ci.environments.deploy`): the workflow that deploys to the
      environment, dispatched, its job held to the environment's protection.
  - §4.1 names each by its registry name: `ci.runs.trigger` for its dispatch, `ci.legs.exec`,
    `ci.diffs.*` and `ci.change_requests.*`, `attachments.*`.
  - §4.3's commands are kiln's. The ones missing are added: `kiln integrations list|verify`,
    `kiln pools list|set|scale`, `kiln images list|validate|build`, `kiln store verify|gc`,
    `kiln env deploy|approve`, `kiln runners facts|probe`. Each runs the registry's operation
    through `/api/ops`, as `kiln op` does. `kiln store verify` is new underneath too: every
    chunk re-hashed, a corrupt one moved aside.
  - §4.4's resources are served over MCP: `kiln://runs/<id>/logs`,
    `kiln://legs/<id>/logs/<step>`, `kiln://workflows/<owner>/<repo>/<path>`, each read
    through the operations as its caller.

  Checked by `w20_ac01_every_operation_on_every_surface`. It reads §4.1 and compares it with the
  registry, and finds a tool and a `kiln` function for each operation (each a route of the
  table: `w20_r01_each_operation_is_a_route` and kiln's own operations' test). Also by
  `w20_ac01_every_command_section_4_3_lists`, which runs each listed command's `--help`; and the
  new surfaces end to end by `w20_ac01_resources_and_what_was_missing_underneath`. They find
  nothing left: `experiments.*` (R11) and `kiln runner setup` (W13-R10) came last.
- *Bisecting* (`crates/kiln/src/ci/bisect.rs`; `ci.runs.bisect`, `ci.runs.bisects`; W20-R09's
  `diagnose`, so it needs `runs.debug`): a failed run's job, the one that failed unless said, is
  run again on the commits between `good` and `bad`. `good` is the last green run's commit of its
  workflow on its branch, by default; `bad` is the run's. The range halves each time until the
  first commit the job fails on is found.
  - Each try is a run of the job (and the jobs it needs) at that commit, on the run's branch,
    whatever its triggers' filters say, trusted as the failed run was: the commits are its own
    history.
  - A try that couldn't run ends it `inconclusive`, saying which commit and why.
  - A server that restarts mid-bisection says so on it.
  - The diagnose skill says how. Checked by `tests/bisect.rs`: four commits after the last green
    run, the second broke it, found in two tries.
- *R11* (`kiln_ci::experiments`, `crates/kiln/src/ci/experiments.rs`; `experiments.*`,
  `/api/experiments`, `kiln experiment`):
  - **Options** are changes on a base commit (the default branch's head unless said): a commit, a
    branch, a diff (its latest version), each applied as the commits it adds. An option can
    also be implemented by an agent (`implement`: what it is), in the asking agent's harness, on
    its machine (or as `harness` says). Each such agent is a child of the one that asked, so
    Mission Control groups them under it. Each works in a git worktree of its own from the base
    and submits its change as a diff (`ci_diffs_submit_patch`); the experiment measures that
    diff.
  - **Strategies**: each alone, every combination, each alone and every pair, greedy stacking
    (rounds, the best so far with each of the rest), custom; the baseline by default; at most 32
    variants.
  - **Building**: each variant is its options' changes cherry-picked onto the base in the options'
    order, kept as `refs/kiln/experiments/<id>/<variant>` (in a repository kiln hosts too). A
    variant whose options conflict says which option, with which, in which files. With agents to
    ask (the asking agent's, or `harness`), one combines them: in a worktree of its own it
    applies each option's change, resolves the conflicts and submits the combination as a diff
    (the variant's glue commit), and the variant is measured once it's made. Without one, or
    when it can't, the variant isn't measured; nor is one with an option an agent didn't make.
  - **Measuring** is ordinary CI on routed runners:
    - a `pipeline` criterion runs its workflow at the variant's commit, its triggers' filters
      aside;
    - a `command` criterion runs a one-job workflow, on a commit made on the variant's for it, on
      its `runs-on`;
    - a `metric` is read from those runs' `$KILN_METRICS`.

    Repetitions are interleaved: each round runs every variant once, in a shuffled order (seeded
    by the experiment), `max_parallel` at once. A `budget_minutes` that runs out cancels what
    runs and reports what was measured, as a cancel does. Each variant counts its runner
    minutes. Its runs are trusted as the owner's are, and anyone else's as a fork's.
  - **Results**:
    - each variant's required checks (passes of repetitions), and each metric's median and its
      ratio to the baseline's, with a 95% bootstrap interval and a verdict;
    - thresholds: `at_most`, `at_least`, or `better` by a fraction all across the interval;
    - each option's main effect, and each pair's interaction (their gain less the sum of each
      one's: synergistic, antagonistic or additive);
    - the recommendation: the best primary objective among the variants that pass every required
      check and are clearly better than the baseline, a tie going to fewer options.
  - **Deciding**: brought back (the default) or the clear winner taken (`decide: auto`);
    `experiments.decide` chooses one. The asking agent is told the variant's commit and ref,
    which it fetches by its commit from kiln's git.
  - **From a question**: an agent's multiple-choice question offers "Experiment…" beside its
    answers (the choices dialog's experiment form: options, strategy, isolation, criteria and
    thresholds, repetitions, a budget, who decides). Its pipelines are the workflows of the
    repository the agent's folder is a checkout of (`GET /api/experiments/where?agent=`). The
    options become options for agents to implement, and the question is answered "running
    experiment E12"; the results come as the agent's next message. Started from an agent
    without a `repo`, the experiment is in that repository; a command criterion without
    `runs-on` runs on the agent's machine.
  - **Its page**: every variant with its checks and its metrics' intervals on one scale (a dot and
    whiskers against the baseline's line, said in words beside them), the effects and
    interactions, its runs by variant, "Continue with this" on each once it's done, and Cancel.
    Experiments are listed beside Plans and Attached in Mission Control.
  - A server that restarts mid-experiment says so on it.
- *AC06* (`tests/experiments.rs`, `w20_ac06_three_options_every_combination`): three options as
  branches, every combination: eight variants, each measured twice by a bench workflow on the
  local runner (16 runs, interleaved). The recommendation is `a+c`; `a+b`'s interval is about 0.6
  of the baseline; `a` and `b` together are synergistic; `b+c` fails its required check. Choosing
  a variant records its commit. A second experiment's conflicting options make a variant said
  to conflict, and cancelling it stops it. The analysis is checked on its own too
  (`w20_r11_measurements_say_what_helps_and_what_fights`, `w20_r11_strategies_make_their_variants`).
- *AC02*, against a local server (`tests/code_mode.rs`, `w20_ac02_one_program_over_the_last_hundred_runs`):
  a hundred runs of a workflow with two jobs failing each its own way (a plain failure, a full
  disk). One `code_run` program pages through the runs and reads the failed ones' legs, then each
  failed leg's latest attempt's class: 200 legs, 100 `failure` and 100 `disk-full`, in under
  five seconds.
- *Left*:
  - R08's isolation (`container`, `firecracker`, `shards` with an Agentfile): an incident's agent
    works in the folder its attachment names. Workspaces of their own wait on Slates and shards
    (docs/agentic.md);
  - R11's isolation the same way: an option's or a glue's agent works in a git worktree it makes
    (Slates, container and microVM workspaces wait on them);
  - AC02 on the live server, which runs no KilnCI yet (none of its runs are the engine's: it
    waits on `[ci] enabled` there); and AC04, with a live harness, for a person to watch.

### W21 UI

**Scope.** Every page CI needs, under the UI rules of §0.3: runs and the run graph, legs and logs,
the workflow editor and visual builder, the action builder, runners, pools and images (with the
image builder), review, environments, settings, insights (G-06, G-08, G-09, G-11).

**Requirements: shared**

- **W21-R01 Code editor component** (`ui/src/components/code/`): CodeMirror 6 (`@codemirror/state`,
  `view`, `language`, `lang-yaml`, `autocomplete`, `lint`, `search`, and legacy modes for Dockerfile)
  with:
  - an Agentfile mode (the Dockerfile grammar plus `AGENT`, `HARNESS`, `SKILL`, `MCP`, `VOLUME`,
    `NETWORK`, `CONNECT`, `ATTACH` and `EXPOSE`);
  - highlighting inside `${{ }}`;
  - schema-driven completion and hover (W02-R09);
  - context-aware completion: `needs.` offers upstream jobs, `steps.` the step ids before this
    step, `matrix.` the axes, `secrets.` and `vars.` the names in scope (names only), `uses:` the
    action catalog with versions and SHAs (W07-R12), `with:` the chosen action's inputs,
    `runs-on:` labels and routing-rule labels;
  - diagnostics from `POST /api/ci/validate`, debounced at 300 ms, with fixes as quick actions;
  - Dockerfile lints and Agentfile lints (shards' own) served by `POST /api/ci/images/validate`.

  The editor chunk is lazy-loaded and at most 350 KiB gzipped. It follows the hyperlight style
  (Geist Mono, quiet gutters). It works on phones: the text area is first; completion appears in
  the standard dropdown.
- **W21-R02 Live data.** Pages subscribe to `/api/events` for their objects and patch in place;
  lists are windowed (`lib/virtual.ts`); motion only on real state changes.

**Requirements: pages**

- **W21-R03 Runs.** One list across repositories, with the facet-box filter (repository,
  workflow, event, branch, conclusion, actor or agent, runner, executor) and a sort select. Each
  row shows the run's one most important sentence: what triggered it, where it is, what it waits
  for. Windowed to 1,000,000 rows.
- **W21-R04 Run page.** The header holds the status sentence, the trigger (commit, diff or PR),
  the duration, an attempt select, and Rerun (all, failed, one job), Cancel, Approve, Debug. Below
  it, the **graph** (G-09):
  - jobs as nodes in columns by depth;
  - a matrix job's legs as a compact grid inside its node, with axes labelled and each leg one cell
    coloured by state;
  - edges drawn only where needs are;
  - the critical path drawn as a heavier line;
  - each node's progress (steps done of total, elapsed against its typical duration from history),
    its wait reason in words, and reused legs marked;
  - expansions waiting on outputs shown as such.

  Selecting a node opens its detail in the page flow, not in a panel over it. Below the graph sit
  the job summaries (Markdown), annotations, artifacts, the tests tab (W16), the data flow of
  outputs and exported environment (W04-R23), and the pinned actions (W07-R02). Pages for runs of
  1,000 legs stay interactive at 60 fps.
- **W21-R05 Leg page.**
  - Steps with live logs: groups collapse, lines carry timestamps, search, annotations sit inline,
    ANSI is rendered.
  - A step timing bar.
  - Attempts with their failure classes.
  - Why here, or why waiting (W05-R20).
  - Outputs, Open terminal and Keep (W17), and "Compare with last green" (W19-R03).
- **W21-R06 Workflow editor and builder** (G-06). The YAML editor (W21-R01) and the **flow view**
  are two views of one document, and YAML is the source of truth.
  - Editing the flow (adding a job, connecting needs, choosing `runs-on` from a select of labels
    and rules, editing steps as a list) rewrites the YAML through a comment-preserving document
    model (the `yaml` package's Document API), so comments and formatting survive.
  - A **matrix editor** shows axes as a table, `include`, `exclude` and `where` as rows, and the
    resulting legs as a live list with counts and "removed by where".
  - A **plan preview** (W04-R29) for a chosen event and branch shows what would run, in the page.
  - Save commits to a branch or opens a diff (native repositories), pushes through the App
    (GitHub repositories with it), or offers the patch to copy or download.
  - The editor opens from any workflow, run or job ("Edit workflow").
- **W21-R07 Action builder.** The same editor with the action schema, an inputs and outputs editor,
  and "Try it": run the action with chosen inputs on a chosen runner and see its outputs and log.
- **W21-R08 Runners, pools and images** (G-11).
  - Runners: each row shows facts, executors, health, pools and load.
  - The extended Add machines flow (W13-R10), in the page.
  - Pools: edit in place, see current and target size, scaling history.
  - Images: list them with platforms and digests. The **image builder** is the code editor in
    Dockerfile or Agentfile mode with lints, a platform select, Build (a run, with its log here)
    and Push.
- **W21-R09 Review** (W14): the inbox (needs your review, your stacks, your agents' stacks, your
  conversations), the stack view, the diff view (W14-R06), the land control with its strategy
  select, and the land queue.
- **W21-R10 Environments** (W15-R08, W15-R09): the board and the environment page.
- **W21-R11 Settings.** Secrets and variables (a scope select, then the list), integrations (a
  catalog of kinds; each opens its setup in the page with Verify), registries, routing rules (an
  ordered list with a tester that explains a sample leg against the current fleet), actions
  policy, OIDC (issuer, keys, publishing, per-provider readiness), notifications.
- **W21-R12 Insights** (W19): per workflow and per runner, with charts that follow the `dataviz`
  rules already used in kiln's metrics.
- **W21-R13 Phones.** Every page is usable at phone width, with no hidden horizontal scrolling;
  diffs are unified; the graph becomes a vertical list of jobs with leg grids.
- **W21-R14 Style.** Only standard controls (selects, the facet box, checkboxes, the composer
  field, pill buttons); no modals, sheets or floating panels; changes are scoped to what each
  workstream asks for.

**Acceptance criteria**

- **W21-AC01** For each page: a Playwright script runs in Chromium and WebKit at 1440×900 and
  390×844, checks the page's primary task end to end against a live or mock server, asserts that no
  element scrolls horizontally, asserts that no element is `position: fixed` other than the shell's
  own, and saves screenshots for review. *UI check, in CI.*
- **W21-AC02** The editor flags every negative-corpus file (F-04) at the right range, completes
  `needs.`, `steps.`, `matrix.`, `secrets.` and `uses:` correctly in fixtures, and round-trips a
  commented file through the flow view with comments intact. *UI test.*
- **W21-AC03** The run graph of a 1,000-leg run renders in < 200 ms and pans at 60 fps on the CI
  machine (Chromium performance trace). *UI bench.*
- **W21-AC04** The runs list with 1,000,000 runs shows its first page in < 100 ms of server time and
  scrolls without blanking. *UI bench.*

**Progress.** In progress.

- *Done.* The API the pages read (§4.1), in `crates/kiln/src/ci/runs_api.rs`:
  - runs, with filters and cursor paging;
  - a run with its legs, per attempt (`?attempt=`), with the actions it pinned;
  - the graph, with needs edges and the critical path;
  - annotations, summaries and artifacts (a leg a rerun kept shows its original's);
  - cancel, and reruns (all, failed, one job; debug);
  - a leg with its steps and annotations, its logs, and routing explain;
  - caches;
  - the names a workflow sees (`/repos/:repo/names`, names only) for completion.
- *Done.* R04, the run page for the engine's runs (`ui/src/views/run-detail/ci.tsx`):
  - the attempt select, and Rerun (failed jobs, all, one job; debug logging) and Cancel;
  - the graph with the critical path heavier, a matrix job's legs as cells by state, a queued
    leg's wait reason in words, and kept legs marked;
  - job summaries, annotations, artifacts and the pinned actions;
  - "Edit workflow".
- *Done.* R12, Insights (`ui/src/views/ci/insights.tsx`, `/insights`; from Runs, a run's
  "Workflow insights" and a runner's details):
  - the overview: repositories (runs, success, p50 and p90, runner time, share against fair
    share) and runners (utilization, busy time, legs, failure classes, cost);
  - a repository's workflows, with each one's median-duration trend, its tests (the most
    failing, slowest or flaky) and its benchmarks (W16);
  - a metric: its trend on each kind of runner, a chart each (one hue, so no two series share a
    chart), with its points as a table (W16-AC05);
  - a workflow:
    - its numbers, and the latest run's critical path;
    - duration (p50 and p90), success, and waiting charts;
    - why legs waited, as bars by reason;
    - its jobs, slowest steps, executors and pools, and cache;
  - a runner's utilization, failures by class, and the repositories it served.

  The charts are the metrics pages' (`Chart`), one measure each. The p90 line is the darker
  step of the p50 hue, an ordinal pair validated against the chart surface. Kiln's pastel series
  palette fails the categorical checks as a pair, so no chart uses two hues. Every chart spans
  the window, shows lone points ringed, and has its numbers as a table under "As a table". The
  pages were checked in Chromium and WebKit, desktop and phone: no overflow, no errors.
- *Done.* R05's start: a leg's annotations inline, "kept from the last attempt", why a queued
  leg waits with each runner's checks, and its steps in the order they ran.
- *Done.* R01 and R06's start:
  - the code editor (`ui/src/components/code/editor.tsx`): CodeMirror 6, YAML, `${{ }}` marked,
    diagnostics from `/api/ci/validate` 300 ms after typing stops, completion, wrapped lines;
    135 KB gzipped, loaded on demand;
  - `/workflows`: a repository's workflow in the editor beside its plan preview for an event
    (W04-R29), refreshed after each clean check;
  - completion of `needs.`, `steps.`, `matrix.`, `needs.<job>.outputs.`, `inputs.`, `env.`,
    `secrets.` and `vars.` (names in scope), `uses:` (the catalog) and `runs-on:`;
  - Copy and Download until saving to the repository arrives (W14).
- Checked in Chromium and WebKit (WebKit through Playwright's own image) at 1440×900 and 390×844:
  no horizontal scrolling, nothing fixed but the shell's, no page errors.
- *Done.* R11, Settings → CI (`ui/src/views/settings-ci.tsx`, `settings-routing.tsx`,
  `settings-actions.tsx`, `settings-oidc.tsx`), beside Deployments' registries and accounts (the
  integrations' catalog, each kind's setup in the page with Verify):
  - secrets and variables: a scope chosen first (the server, an organization, a repository, or a
    workflow's or environment's written out), then its secrets (names, when set and last used, a
    brokered one's source; saving one again replaces its value) and its variables;
  - routing: every rule in the order it applies with where it comes from, the server's edited as
    an ordered list (earlier, later, remove, add), and a tester that explains a sample leg
    against the rules and the runners connected now;
  - actions: the server's policy as configured, and a repository's set here (allow, deny, pinned
    by SHA) beside its `.kiln/config.yaml`'s; mirroring the prefetched actions;
  - OIDC: the issuer, whether it's served here, the subject's claims, the keys (signing,
    published), rotating in 7 days or now, publishing, the key set to download, and each
    federating integration's last Verify;
  - notifications: what every rule sent, and why any didn't go; and Settings → You, a person's
    own rules (W19-R06).
- *Done.* R05's step timing bar, attempts and outputs, on the leg page:
  - the bar: each step's share of the leg's time, with 2 px gaps, rounded ends, a reading
    each, and the fail tone only for a step that failed;
  - attempts: each attempt's status, failure class, runner, executor, exit code and error, and
    its times;
  - outputs: the leg's, by name.

  With Open terminal and Keep (W17) and "Compare with last green" (W19-R03), R05 is done.
- *Done.* R03's facet box and sort on the runs list: repository, workflow, event, branch and
  author offered from the runs loaded (each with how many it would show), words typed matching
  a run's message, commit, branch, workflow or author; and the sort, newest, oldest, longest or
  shortest first, the server's (`/api/runs?sort=&offset=`), paging through every run. Each is in
  the URL; one repository chosen is the server's to narrow by too.
- *Done.* R03's runner and executor facets: each run in the list says the runners and executors
  its legs ran on (a run may have several of each), and the facet box offers both, a run shown
  when any of its legs ran on the one chosen.
- *Done.* R06's saving, in the editor: an edit opens a diff on the trunk of a repository kiln
  hosts (`ci.workflows.edit`: checked first, refused with its errors), the diff linked from the
  page; and for any repository, the patch of the edit, a unified diff `git apply` takes
  (`ui/src/lib/patch.ts`, checked against `git apply` in ten cases by `ci/ui/patch_check.ts`),
  beside Copy and Download. Checked in Chromium and WebKit, desktop and phone
  (`ci/ui/people_check.mjs`).
- *Done.* AC04. The runs list's first page, with 1,000,000 runs in a store on disk, takes 0.37 ms
  of server time newest first, 0.76 ms for one repository's and 0.33 ms oldest first (a release
  build, `kiln ci bench --only list`: the store's query, the legs' runners and executors, the
  JSON; held to 100 ms). The list renders only the rows near the viewport, at the height a row
  has on that screen (`useWindowedRows`), so it scrolls without blanking. Checked against a mock
  of a million runs in Chromium and WebKit, desktop and phone (`ci/ui/runs_window_check.mjs`):
  grown to 5,000 runs as it scrolled, fewer than 200 rows rendered, and rows under the middle of
  the screen wherever it was scrolled to.
- *Done.* R01's hover and the schema's keys, in the editor: a key hovered says what the file's
  schema says of it (W02-R09's, a GitHub workflow's, a kiln one's or an action's), and a key typed
  at a line's start (or asked for there, Ctrl-Space) is offered from those its mapping may have,
  each with its description. Where a position is comes from its indentation alone
  (`ui/src/lib/yaml-spot.ts`: sequences either way they're written; nothing inside a block
  scalar), as the text is often half typed. Checked in Chromium and WebKit, desktop and phone
  (hover on a desktop).
- *Done.* R06's matrix view, in the plan preview: each job's matrix as written (its axes and
  their values as a table, `include`, `exclude` and `where` as rows, `platforms`), the legs it
  makes counted, and the combinations removed with why. The preview's answer carries each job's
  matrix (`matrices`). Checked in Chromium and WebKit, desktop and phone.
- *Done.* R06's saving, the rest. A workflow comes with what its repository takes (`saves`, from
  where it is kept), and the editor offers just that (`POST …/workflows/:path/edit`, W02-R12):
  - a repository kiln hosts: a diff, or a commit to a branch, made as a push of it would be
    (`hosting::commit_branch`): refused for what a push is refused for (the trunk is the land
    queue's), its pusher kept for its runs' secrets (W10-R02), and the push's run;
  - one on GitHub (through the App), GitLab or Bitbucket: a new branch off the default branch and
    its pull or merge request, or a commit to a branch. GitHub's token for it may write workflows,
    as GitHub refuses a workflow file otherwise;
  - any: the patch, as before.

  Checked by `crates/kiln/tests/workflow_edit.rs` (labels, policies, the patch, a diff, the trunk
  refused, a branch and its run, who may save), `tests/github_app.rs` (the branch and pull request
  through the App, against a fake GitHub) and in Chromium and WebKit, desktop and phone
  (`ci/ui/people_check.mjs`: a commit to a branch, and `runs-on: local` warned of on a server
  with no runner).
- *Done.* R06's flow view, and the matrix edited in its table: the editor's second view of the
  same text (YAML or Flow). The jobs come by stage (a job one past the latest it needs), each with
  where it runs (its labels as chips, and a select of this server's runners', pools' and routing
  rules' labels, `GET /api/ci/repos/:repo/labels`, and GitHub's), what it needs (a select of the
  jobs it can need without going round), its matrix (each axis's values edited in place, axes
  added and removed, `include` and `exclude` rows) and its steps as a list (added, moved,
  removed, named, their commands and actions edited); jobs are added and removed. The plan
  preview follows each edit, its legs counted and the combinations removed said.
  - Each edit is made to the text by `ui/src/lib/workflow-doc.ts`. The `yaml` package's document
    model finds what it touches; only that is rewritten, in the file's own style (its indent, how
    most of its lists sit under their keys, its brackets' spacing), so comments and the rest's
    formatting survive byte for byte. The text read back must be the old data with just that
    change made, or the edit is refused, said, and nothing is written. An expression (`runs-on:
    ${{ … }}`, a matrix from `fromJSON`) is shown and left to the YAML.
  - Checked by `ci/ui/workflow_doc_check.ts` (39 cases, each edit's text exactly, comments and
    blank lines kept, `3.10` kept a string, what can't be edited here refused) and in Chromium
    and WebKit, desktop and phone (`ci/ui/people_check.mjs`: a job added and given a need, an
    axis's values and the preview's count, and the YAML with all of it and its comment).
- *Done.* R07, the action builder. A repository's actions (`.github/actions/**/action.yml`,
  `.kiln/actions/…`) are listed with its workflows and open in the same editor, with the action
  schema's keys and hover; they save as workflows do. Its second view is its inputs and outputs:
  each input's description, default and whether it's required, each output's description (and a
  composite action's value), added and removed, through `ui/src/lib/workflow-doc.ts` as the flow
  view's edits are. "Try it" runs the action as the editor has it, saved or not, with the inputs
  given, on a runner or label chosen (`POST /api/ci/repos/:repo/actions/try`, `ci.actions.try`):
  a commit of it on the ref with a workflow whose one job uses it, each input passed on as a
  dispatch input (data, never YAML or an expression) and its outputs the job's, kept as an
  upload is (W17-R06), trusted for the server's owner and a fork's for anyone else. The pane
  shows the run as it goes, its outputs and the action's log. Checked by
  `tests/workflow_edit.rs` (`w21_r07_an_action_tried`: listed, opened, tried as edited with an
  input, its output, a fork's for a developer, and what it refuses), `ci/ui/workflow_doc_check.ts`
  and in Chromium and WebKit, desktop and phone.
- *Done.* R08's image builder and the pools' scaling history, and R01's Dockerfile and Agentfile
  modes.
  - The code editor reads a Dockerfile as CodeMirror's own mode does, and an Agentfile the same
    way with shards' directives (`AGENT`, `HARNESS`, `SKILL`, `MCP`, `NETWORK`, `CONNECT`,
    `ATTACH`) (`ui/src/components/code/image-modes.ts`).
  - The Images panel's New image: a name, a repository, a Dockerfile (checked as it's typed, by
    kiln's parser and lints) or an Agentfile (shards' to check where it builds), its platforms,
    Build (`POST /api/ci/images/build`, `ci.images.build`: `kiln/build-image@v1` on the runner
    or label chosen, from a commit made for it as "Try it"'s is, its run followed here, `push:
    false`: only built), Build and push (pushed and registered: the server owner's only, as a
    fork's token pushes nothing), and Save to repo (a diff adding `.kiln/images/<name>/`).
    `kiln/build-image@v1` takes `push` (by default `true`).
  - A pool's scaling history: its executors (and the busy among them) and the legs waiting for
    it, taken a minute apart from the server's start and kept a week (`ci_pool_samples`, `GET
    /api/ci/pools/:name/history?hours=`, `ci.pools.history`); charted in the pool's detail over a
    day or a week, with the same as a table by the hour.
  - Checked by `tests/registry.rs` (`w21_r08_an_image_built_from_its_editor`: the owner's build
    pushed and registered with its platform, or only built and nothing registered; a developer's
    a fork's run that waits for an executor untrusted code may use; their push refused; errors
    refused), `tests/pools.rs` (`w21_r08_a_pools_history`) and in Chromium and WebKit, desktop
    and phone.
- *Done.* R04's graph, the rest (`ui/src/views/run-detail/dag.tsx`):
  - every job of the workflow is a node in its column, those whose legs aren't made yet (a job's
    are made once what it needs is done) drawn dashed, as what they wait for ("waits for build-a,
    build-b +2"), and a matrix that comes from what it needs as such ("its matrix waits on the
    outputs of plan"): the run's plan says them (`GET /api/ci/runs/:id/graph`'s `jobs`, from the
    run's saved workflow: each job's name, needs, steps, whether its matrix reads `needs.` and
    whether it calls a workflow);
  - a matrix job's legs as a grid inside its node: across its first axis's values, down the
    other axes' combinations, a leg a cell coloured by state, a combination left out an empty
    cell, and what runs across and down said with it ("x → · y ↓"); one axis of more than 25
    values wraps in order. Each leg's values come with the run's legs (`matrix` on a CI leg),
    so a matrix made while the page is open fills in as its legs arrive;
  - each node's steps done of all, and its time against how long it typically takes ("1m 05s
    of ~2m 10s" while it runs): the median of its spans over the workflow's last 20 runs that
    succeeded (`typical`; migration `20261020-0300_w21_runs_by_workflow`).

  Checked by `tests/run_graph.rs` (`w21_r04_the_run_graph_from_the_plan`: the jobs listed while
  the first runs, a matrix from outputs, the legs' values, typical times on the next run) and the
  graph bench (`ci/ui/graph_bench.mjs`: four 250-leg grids with their axes, the two jobs after
  them as what they wait for, 200 five-leg grids).
- *Done.* AC03. The run graph of a 1,000-leg run renders in 25–30 ms (from the last of its data
  arriving to the first frame with every node in it, the median of three loads in Chromium) and
  pans at 60 fps (no frame over 25 ms in two seconds), both for four 250-leg matrices with two
  jobs after them and for 200 jobs of five legs (`ci/ui/graph_setup.py`, `graph_bench.mjs`, a
  Chromium trace of each pan kept). The cost had been React's own: a section that suspended was
  shown no sooner than 300 ms after its loader (React's throttle on revealing what suspended),
  however soon its data came. `Loadable` now renders again as each key's first data lands
  (`useLanded`, `ui/src/lib/resource.ts`), so what waited shows at once, on every page; the
  loader itself still shows only after a moment.
- *Done.* AC02. The editor flags all 33 of the negative corpus's codes at their line and column
  (`ci/ui/editor_ranges_check.ts`: each file sent as the editor sends it, a workflow of either
  dialect, an action or an overlay with its repository, and placed as the editor places it,
  `ui/src/lib/diagnostics.ts`); completes `needs.`, `steps.`, `matrix.`, `secrets.` and `uses:`
  (`ci/ui/editor_check.mjs`); and the flow view's edits keep comments
  (`ci/ui/workflow_doc_check.ts`, and `people_check.mjs` in the browser).
- *Done.* AC01 in CI. `ci/ui/run-all.sh <kiln>` runs every check against servers of the build
  it's given, each on its fixture, the browsers in Playwright's image, each check's report and
  screenshots under `ci/ui/shots/`; `.kiln/ci.yaml`'s `ui-checks` job runs it on main after the
  build and the UI's checks, on any Linux machine with Docker that builds kiln (`requires:`,
  W05-R22; a v1 pipeline keeps the key and goes by `runs-on`). Each check asserts W21-AC01's two
  page-wide asserts on every page it visits (`ci/ui/layout.mjs`), the review pages' included.
- *Done.* R10: the environments board and an environment's page (W15-R08, R09, under
  Deployments → Environments), checked in Chromium and WebKit, desktop and phone
  (`ci/ui/environments_check.mjs`, in `ci/ui/run-all.sh`). R09 is the review pages W14-AC06
  checks.
- *Done.* R01, the rest: a step's `with:` completes the inputs of the action it uses (from the
  catalog: a mirrored action's `action.yml`, kiln's own as theirs would say them), each with
  whether it's required or its default; and a diagnostic that says what would fix it offers that
  as its quick action (a script's untrusted input moved into `env:`; an unknown key renamed to the
  one it likely means). A fix applies only to the text it was made for: once that has changed,
  the next check's is the one to take. Checked in Chromium and WebKit (`ci/ui/editor_check.mjs`)
  and by `w21_r01_an_unknown_keys_fix`.
- *Done.* R04's data flow (with W04-R23): below the artifacts, each output from the step that made
  it, its value (once, or per leg where they differ; never a secret's) and the jobs that read it
  with where, each to its leg; and each job's exported environment and the jobs that take it. The
  run's sections are fetched at once.
- *Left.* R01's Agentfile lints in the editor: they are shards' own (W13-R06), and the server
  has neither shards' crate nor its command here to ask; an Agentfile is checked where it builds
  (`shards build`, W13-R11). Blocked on shards.

### W22 Conformance, end-to-end and performance testing

**Scope.** The test program that proves the goals: GitHub conformance, end-to-end scenarios on the
live cluster, fault injection, security, performance, and mixed-version fleets.

**Requirements and acceptance criteria**

- **W22-AC01 GHA corpus.** The workflows of F-03 (≥ 300) run on kiln from a test repository
  (`e2e/gha` branches on kiln-git), each with fixtures for the services it needs. A results table
  (`docs/ci-conformance.md`, generated) lists each workflow's result with the reason for every
  non-pass. Target: 100 % of workflows that need no external account; the rest pass with test
  tenants (W11-AC03).
- **W22-AC02 Top actions.** F-21's actions, each in a workflow exercising its main inputs, green.
- **W22-AC03 GitHub comparison suite.** Probe workflows (F-22) for behaviours this spec marks as
  "confirmed by W22" (matrix output last-writer, reusable workflow limits, status-function edge
  cases, `hashFiles` inputs, `toJSON` formatting, step summary limits). They run once on GitHub
  from a public repository the user owns, and their recorded outputs are committed as golden
  files. kiln must reproduce them. The comparison is refreshed when GitHub changes. *User-assisted:
  needs the GitHub account.*
- **W22-AC04 Fault injection.** A test-only runner mode (`kiln agent --chaos <spec>`, compiled
  only with the `chaos` feature) drops connections, kills processes with the OOM signal, fills the
  disk, delays the network and corrupts uploads. W05's retries and classes, W08's integrity checks
  and W06's cleanup behave as specified under each.
- **W22-AC05 Live end-to-end suite.** `.kiln/e2e-ci.yaml` (an extension of today's e2e.yaml) on
  pushes to `e2e/*`. It covers:
  - every executor the cluster's runners have;
  - services;
  - caches and artifacts;
  - OIDC to a local Vault;
  - a stacked diff landed with each strategy;
  - an environment deployment through CD;
  - a fork-style untrusted run;
  - an agent incident.

  It passes before any milestone is called done.
- **W22-AC06 UI suite** (W21-AC01) in CI.
- **W22-AC07 Security suite**:
  - secret-leak scans over logs, the database, the API and events;
  - token scope enforcement on every route;
  - fork isolation;
  - signed URL tampering;
  - archive traversal in tarballs and zips (zip slip);
  - YAML bombs;
  - SSRF guards on integration verify and webhook targets (no private address ranges unless
    allowed).
- **W22-AC08 Performance.** `kiln ci bench` runs:
  - the scheduler simulator (W05-AC08);
  - expressions (W01-AC07) and parsing (W02-AC07);
  - cache throughput (W08-AC08);
  - log ingest (§1.5);
  - run creation (W03-AC09).

  It writes `target/ci-bench.json`, and kiln's own CI fails when a target is missed or a result
  regresses by more than 10 % (with W16's interval rule).
- **W22-AC09 Mixed fleets.** A server at the new version with runners at the version on `main` before
  the CI work (no `CAP_CI2`) and runners at the new version: v1 jobs keep running on old runners, v2
  legs wait on old runners with `capability` and run on new ones, nothing fails because of the mix.

**Progress.** In progress: the local harness, AC01's planning half and its starter workflows,
and AC02.

- *Done.* A throwaway server with the built-in agent (see the W03 to W14 progress notes) runs
  fixture workflows end to end.
- *AC01, planning* (`crates/kiln-ci/tests/plan_corpus.rs`): every one of F-03's 457 workflows
  plans for each event it declares, as its own repository, with that repository's called
  workflows read from the corpus: 804 plans, 3,343 legs, and 568 jobs whose matrix or `if`
  waits for their needs' outputs. 52 calls to workflows outside the corpus are listed, and none
  fail. It found the preview (W04-R29) reporting such jobs as failed, from `fromJSON('')` of
  its empty outputs; they are left for later now.
- *AC01, running* (`ci/conformance/starters.py`, results in `docs/ci-conformance.md`):
  - GitHub's starter workflows run unmodified on `ubuntu-latest` in kiln's runner image against
    a fixture project in every language they build (`crates/kiln/tests/fixtures/starters`).
    The harness gives the reason for each one it doesn't run (accounts, macOS and Windows,
    GitHub Pages, code scanning, framework applications) and for each that doesn't pass.
  - The first run passed 16 of 33. Fixing what kiln got wrong took it to 22:
    - the workspace paths expressions see in docker legs;
    - tool caches shared by legs running at once;
    - dependency snapshots;
    - Docker inside legs;
    - the toolchains the image lacked;
    - `/usr/share/dotnet` for setup-dotnet.
  - Two more (`ci_pylint`, `ci_python-package`) passed once github.com's limit on anonymous API
    reads (below) had reset: 24 of 33.
  - The rest fail the same way on GitHub:
    - pinned actions that predate Ubuntu 24.04 (setup-beam, setup-ruby);
    - Python 3.7, which has no Ubuntu 24.04 build;
    - a container too old for Node 20;
    - cabal 3.2 against Hackage's keys;
    - too few `permissions` to submit a dependency graph.

    Three more are kiln's: the Maven submission action posts to api.github.com whatever
    `GITHUB_API_URL` says, kiln's image has no conda, and there's no Windows runner here.
  - Repeated runs meet github.com's limit on anonymous API reads (setup-python sends its token
    only to github.com), which the harness marks.
  - *2026-10-19*: code scanning on the facade brought the code-scanning starters in, 57 run.
    After five passes, **39 of 57 pass, and each of the other 18 fails the same way on GitHub**
    as written, each with its reason in the table. kiln's fixes on the way:
    - job containers' and Docker actions' engine calls get ten minutes, not one;
    - the runner image's conda, PowerShell, gh, pip and `~/.local/bin`;
    - a Docker action's `runs.env` with its inputs;
    - images made for GitHub's runner user;
    - the workspace's parent (CodeQL's `../results`);
    - codeql-action's newer features path and repository properties;
    - what killed runners left behind, which had used up Docker's address pools.

    The fixture gained what scanners look for: R for lintr, `file.yaml` for kubesec, ESLint's
    `.eslintrc.js`, Conda's `environment.yml`. CodeQL runs on Python, its languages filled in
    as GitHub fills them. The lab keeps its runner's tool cache between passes, as a runner does,
    and gives its legs the machine's Docker, as GitHub's hosted runners have it. 115 aren't run,
    each with its reason. Most need an account (68, the clouds among them) or macOS or Windows
    (10). GitHub Pages, issues and Models are outside kiln (§1.3). SLSA's reusable workflows sign
    through Sigstore's public instance, which trusts GitHub's ID tokens, not kiln's. The
    dependency graph's compare those 5 starters need is now served (W14-R11); they're to be run
    again.
- *AC01, the repositories' own workflows* (`ci/conformance/corpus.py`, results in
  `docs/ci-conformance-corpus.md`), from 2026-10-22:
  - Each of F-03's 24 repositories, at the commit F-03 took its workflows at, is kiln's own
    repository (native and public, as on GitHub). Its whole tree is a pull request against an
    empty trunk, then pushed to it. The trunk has the upstream's version tags, as a checkout of
    the whole history has them.
  - They run in the conformance lab: a throwaway server and a Linux runner of two slots. Its
    `docker` executor runs kiln's runner image, given the host's Docker through each leg's own
    socket. A pacer freezes the lab while the machine's disks are busy, for production's sake.
  - Every leg that misses is classified, and a workflow's miss is explained only if each of its
    legs' is:
    - it waits for macOS or Windows, which the lab hasn't;
    - a reason not kiln's: the repository's secrets, GitHub Pages, a check of the pull
      request's description or base, the lab's host network, a timed wait in the slow lab;
    - a gate's (one job that says how the ones it needs did, a stable name for branch
      protection, as prometheus's `test_go_oldest_status`), which follows their misses: it is
      explained only as they are;
    - or kiln's to fix.
  - Found and fixed on the way, each with its test:
    - `cross`'s mounts, and GitHub's paths, in a `docker` leg (W13-R04);
    - two legs at once on one runner's Docker: their names are their own now (W13-R04);
    - a public repository read with no permission for it, over git and the REST API
      (W10-R04, W14-R01);
    - a leg's fetch of another public repository (W14-R11), and `GH_HOST` in a container;
    - an owner's `.github` repository, and a remote reusable workflow's `./` calls (W07-R02);
    - v1 to v3 artifacts (W08-R08);
    - live logs the masker held back (W06-R08);
    - `USER` and `LOGNAME`, the runner's, in a hosted leg's steps (W13-R04);
    - Go on the runner image's `PATH`, as on GitHub's (prometheus's setup asks `go env` without
      setting Go up).
  - Pass 1022g (six repositories, on builds before most of these): 10 of 25 workflows that ran
    passed, 7 more missed only for reasons not kiln's, and 8 for kiln's, each fixed since.
    Pass 1023b ran ripgrep, bat, cargo and tokio again on a build with all of it; pass 1023c,
    from 2026-10-23, runs the other twenty, each on the newest build when it starts.
- *AC02, fourth batch* (2026-10-19), in the lab, the facade over HTTPS:
  - `github/codeql-action/upload-sarif`: the code-scanning starters above.
  - `peter-evans/create-pull-request@v7`: a branch pushed, a pull request opened, then updated
    by the next run.
  - `actions/labeler@v5`: on the change request's `pull_request_target`, labels from
    `.github/labeler.yml`.
  - `peaceiris/actions-gh-pages@v4`: `gh-pages` pushed.

  That is 43 of F-21's 50. The seven left: five need accounts (codecov and the clouds' logins);
  `attest-build-provenance` signs through a Sigstore (the public one, or GitHub's) that trusts
  only GitHub's ID tokens; `setup-qemu-action` changes the kernel's binfmt and needs a
  disposable runner (W13's microVMs).
- *AC02, fifth batch* (2026-10-20): `docker/setup-qemu-action@v3` in a Firecracker microVM whose
  image has Docker (`docker:dind`), where the kernel is the leg's own to change: the guest mounts
  `binfmt_misc`, the action registered its arm64 emulator, and an arm64 Alpine container ran,
  emulated (`w22_ac02_setup_qemu_in_a_microvm`). That is 44 of F-21's 50; the six left need
  accounts or GitHub's Sigstore.
- *AC02, sixth batch* (2026-10-21): the actions that need an account, recorded so, each in a dry
  configuration that passes on kiln unmodified, against a stand-in for its provider where the
  action lets one take its place (`tests/account_actions.rs`, W07-AC01):
  - `aws-actions/configure-aws-credentials@v4` and `amazon-ecr-login@v2`, the AWS SDK's own
    `AWS_ENDPOINT_URL_STS` and `AWS_ENDPOINT_URL_ECR` naming the stand-in: kiln's ID token
    (audience `sts.amazonaws.com`, the run's subject) assumed the role, and `docker login` signed
    in to the registry ECR named;
  - `google-github-actions/auth@v2`, its `universe` a stand-in for Google's STS over TLS
    (`sts.localhost`, its CA the job's `NODE_EXTRA_CA_CERTS`): kiln's ID token, for the pool's
    provider, exchanged, and the credentials file made;
  - `codecov/codecov-action@v5` with `dry_run`, its `url` a stand-in for Codecov's API.

  That is every one of F-21's 50 but two: `azure/login`, which has no such configuration (it
  signs in with the Azure CLI against Microsoft Entra), waits for a test tenant (W11-AC03); and
  `actions/attest-build-provenance`, which signs only on github.com and GHE.com (W18-R02: its
  toolkit accepts no other server; kiln's own `kiln/attest@v1` and the job's `attest:` are
  W18-R01's).
- *AC02, first batch* (16 actions, 2026-10-18):
  - Green: `setup-node`, `setup-go`, `setup-java`, `setup-uv`, `pnpm/action-setup`,
    `setup-bun`, `dtolnay/rust-toolchain`, `setup-terraform`, `docker/metadata-action`,
    `github-script`, `softprops/action-gh-release`, `actions/cache`, `upload-artifact`,
    `download-artifact` and `checkout`.
  - Failed, and why:
    - `setup-buildx`, `cosign-installer`, `setup-deno` and `setup-zig`: the host's DNS failed
      during the run. All four passed in the second batch.
- *AC02, second batch* (2026-10-18): 20 new actions green.
  - In the `docker` executor: `setup-deno`, `setup-zig`, `cosign-installer`, `setup-dotnet`,
    `actions-rust-lang/setup-rust-toolchain`, `taiki-e/install-action`, `goreleaser-action`,
    `setup-beam`, `setup-ruby`, `setup-php`, `setup-gradle` (with `setup-java`),
    `haskell-actions/setup`, `setup-r`, `setup-julia`, `flutter-action` and `setup-gcloud`.
  - On the host: `setup-buildx-action`, `login-action`, `metadata-action` and
    `build-push-action` pushing to a registry service; `golangci-lint-action`.
- *AC02, third batch* (2026-10-18): `Swatinem/rust-cache` (with its cache), `dorny/paths-filter`
  on a push, and `hashicorp/vault-action`. vault-action logged in to a local Vault dev server whose
  JWT auth trusts kiln's issuer by discovery, with a role bound to `repo:acme/e2e:ref:refs/heads/main`,
  and read a secret. rust-cache's earlier misses were the harness's: no toolchain in the isolated
  home, then a stray `/tmp/Cargo.toml` above the fixture.
  - Fixes found on the way:
    - colliding container names;
    - root-owned files from container steps;
    - images that never rebuilt;
    - the hosted runner's tool cache and `ImageOS`;
    - problem matchers.
  - Left of F-21:
    - `labeler`, `create-pull-request`, `actions-gh-pages` and `upload-sarif`: W14's REST and
      push;
    - `attest-build-provenance`: W18;
    - `setup-qemu-action`: it changes the kernel's binfmt, so it needs a disposable runner;
    - `codecov`, `configure-aws-credentials`, `amazon-ecr-login`, `google-github-actions/auth`
      and `azure/login`: accounts (W11-AC03).
    - `setup-python`: GitHub's `python-versions` manifest has builds for Ubuntu only, and the
      host is not Ubuntu. *Fixed:* `ubuntu-latest` legs now run in kiln's Ubuntu runner image
      (W13-R04), where it installs CPython 3.12.
- *AC04* (`crates/kiln/tests/chaos.rs`: `cargo test -p kiln --features chaos --test chaos`,
  in kiln's gate). `kiln agent --chaos <spec>`, or `KILN_CHAOS` for a server's own runner, is
  compiled only with the `chaos` feature (`crate::runner::chaos`):
  - `drop=<duration>`: the runner drops its connection that often;
  - `delay=<duration>`: what it sends the server arrives that late. A frame due goes back through
    the runner's loop, sent if it's connected and welcomed then and kept for the next connection
    if not, as any frame is (sent straight on, one could reach a connection whose Hello the server
    hadn't taken yet, and be dropped there: the log once lost a step's lines so);
  - `oom=<duration>`: a step on the host is killed that long after it starts, as the OOM killer
    kills, and counted where kiln counts the OOM killer's kills;
  - `disk=<MiB>`: a job container's workspace is a volume that small;
  - `corrupt`: the leg's tools reach the server through a proxy that flips a byte of each upload
    to the blob store;
  - `rebuild=<duration>`, the server's own: the CI engine rebuilds its runs that long after the
    server starts, so its runners say hello before it knows their legs (W05's R05 note).

  A leg's faults hit its first attempt only, so its retry shows the recovery. Under each:
  - dropped every 3 s and 200 ms late, a leg goes on: it's neither lost nor retried, its log
    arrives whole, and its outputs reach the job after it;
  - the OOM kill is `oom`, the full disk `disk-full`, each retried by the policy that names it;
  - a corrupted upload fails FinalizeArtifact's hash check (W08), its log line retries it, and
    only the retry's artifact is there;
  - nothing of a failed attempt is left: the killed step's processes, its containers.

  Found and fixed on the way:
  - A runner reconnecting within the grace was told to cancel its CI legs (the v1 reconcile took
    them for v1 jobs), and their reports were dropped. The hub keeps them now, and the dispatcher
    stops only legs given up while their runner was away.
  - What was in flight as the connection dropped was lost: a leg's outputs (the next job saw
    none), its failure's class, its result (the leg then lost with its runner). The runner now
    numbers each leg's events (`seq`, in their JSON) and keeps them and the leg's result until the
    server says it has the result (`ServerMsg::Received`, `CAP_RECEIVED`), sending them again after
    a reconnect. The dispatcher takes each event once, and a leg's result only from the runner
    it's on.
  - A full disk that the failing tool doesn't name (busybox's `dd`) was a plain `failure`: a job
    container's workspace is measured in the container now.
  - The first of these again, before the dispatcher has rebuilt its runs after a restart (it knows
    none of its legs then): a leg its runner said it still ran was cancelled, and one it didn't was
    put back on it, running for good (W05's R05 note; `w05_r05_hellos_before_the_rebuild`).
- *AC07* (`crates/kiln/tests/security.rs`, against a server of its own):
  - **Token scope.** Every route of the route table refuses a token that may do nothing. That is an
    agent's for a user without a grant: 403 for it, and 401 without a token, on 128 routes.
    Tokens are each caller's own: someone else's is never listed, and is out of reach.
  - **Secrets.** A run uses a secret in its command, environment, annotations, a title, its
    summary, its outputs and `$GITHUB_ENV`. The logs show it masked. It is in no API answer (the
    run, its legs, annotations, summaries and graph), not in the events stream, and in no file
    under the data directory, the database's write-ahead log included.
  - **YAML bombs.** A billion laughs is refused, in well under a second, by everything kiln reads
    from a repository: workflows of both dialects, overlays, actions, `routing.yaml`,
    `notify.yaml`, `config.yaml`, `pnpm-lock.yaml` and `yarn.lock`.
  - **SSRF guards.** A request to an address someone gave kiln never reaches this machine, a
    private network or a link-local address (a cloud's metadata service), unless
    `[ci] allow_private_targets` names that host, address or network. This covers webhook
    subscriptions, channels' webhook URLs and OCI registries' Verify. The name is resolved where
    the request is made and the connection goes to the address checked, so DNS rebinding can't
    get around it.
  - **Archive traversal.** The runner unpacks a commit's tarball with nothing outside the
    workspace (`w22_ac07_tarball_stays_in_the_workspace`):
    - entries with `..` are skipped;
    - a link earlier in the archive can't lead a later entry, or the directories made for it,
      elsewhere;
    - a link where a file goes is replaced, never written through.

    Actions' tarballs unpack through `tar`'s own checks, and local runs arrive as git packs.
  - Fork isolation is W18-AC03, and signed URL tampering W08-AC04.
- *AC08* (`crates/kiln/src/ci/bench.rs`): `kiln ci bench` measures §1.5's targets. Each result is
  the median of `--runs` runs; the hub's measures are single runs.
  - In process: the scheduler (W05-AC08's load), expressions and a 2,000-line workflow's parse.
  - Against a hub of its own (an in-memory store, the CI engine running):
    - run creation: 1,000 pushes, each timed from its event to its run in the store, the
      workflow already read (git stubbed);
    - log ingest: 16 legs' output at once, and a line's way to a live viewer;
    - the cache's store: a 1 GiB entry saved, then restored as a restore streams it;
    - a restart (W05-AC09): 10,000 legs queued by one hub on a store on disk, for runners there
      aren't, rebuilt by a second hub on that store, as the server is when it starts again.
  - With `--only firecracker`, on a machine that runs microVMs: a leg's microVM started cold and
    warm, from its image's snapshot (W13-AC05).

    The cache's restore is measured over HTTP too (W08-AC08), as `@actions/cache` makes one from a
    blob URL that isn't Azure's: its length from a `HEAD`, then 4 MiB ranges, 8 at once, from
    kiln's listener. Once over plain loopback (`cache.restore_http`), once over a 10 GbE link
    (`cache.restore_10gbe`): the bench runs itself again in a user and network namespace of its own
    (`unshare -rn`), whose loopback it shapes to 10 Gbit/s, 50 µs each way (`tc … netem`, a round
    trip of 0.13 ms). Where the machine can't make one (no `unshare` or `tc`, user namespaces off),
    it says so instead. A debug build, on a machine busy with the corpus lab, restored 1,106 MiB/s
    over the link, 93 % of it (1,740 MiB/s over plain loopback): past the target with room. The
    bench machine's release build (`.kiln/bench.yaml`'s `ci-bench` job on adacachy, 985262b,
    2026-10-25) restored 1,181 MiB/s over the link and 5,610 MiB/s over plain loopback. The runs' and logs' network part
    (the runner's link, the browser) is the live cluster's to confirm.

  It writes `target/ci-bench.json`, and, in a leg, `$KILN_METRICS` with the runs as samples. It
  exits 1 on a miss in a release build. `.kiln/bench.yaml`'s `ci-bench` job runs it on the bench
  machine with `bench-gate: { metric: "ci.bench.*", max-regression: 10% }`, so a regression past
  10 % (W16's interval rule) fails it. v1 reads that file too and ignores the gate; a test reads
  kiln's own workflows both ways.

  A release build on a quiet 32-core machine (2026-10-19), against the targets:

  | Measure | Result | Target |
  | --- | --- | --- |
  | Scheduler throughput | 64,942 assignments/s | ≥ 2,000 |
  | An input's p99 / mean | 0.016 / 0.008 ms | < 50 / < 5 ms |
  | Expressions | 2.94 M evaluations/s | ≥ 1 M |
  | A 2,000-line workflow | 2.8 ms | < 20 ms |
  | A run created, p99 (git stubbed) | 1.8 ms | < 500 ms |
  | Log ingest, 16 legs at once | 2,530 MiB/s | ≥ 200 MiB/s |
  | A live line's way, p99 (the server's part) | 0.18 ms | < 250 ms |
  | A 1 GiB entry restored from the store | 7,962 MiB/s | ≥ 500 MiB/s |
  | The same over HTTP on a 10 GbE link (the bench machine, `ci-bench`, 2026-10-25) | 1,181 MiB/s | ≥ 500 MiB/s |

  Measuring run creation found a git read on every run start, for reusable calls the workflow
  didn't make. It's skipped now, which cut the p99 from 44 ms to 9 ms in a debug build.
- *AC09* (`crates/kiln/tests/mixed_fleet.rs`): a server at this version with its own runner, and
  a runner built from `main` before the CI work (`KILN_OLD_BIN`: kiln 0.1.0 of edc9ae8, no
  `CAP_CI2`). The old runner enrolls and connects as it did then. Its v1 job (`runs-on: [old]`)
  runs on it and succeeds. The engine's leg only the old runner's labels match waits with
  `capability`, naming it, and never fails; the leg the new runner can take runs there and
  succeeds. It passed on 2026-10-19.
- *Done.* AC06: the UI suite (W21-AC01) runs in kiln's own CI, `.kiln/ci.yaml`'s `ui-checks`
  (`ci/ui/run-all.sh`).
- *AC03, kiln's half.* F-22's probes are `crates/kiln-ci/tests/fixtures/probes` (A.8's list, and
  the cancel sequence W06-R17's `cancel:` takes its grace from), each saying what it finds as `PROBE
  key=value` lines. `ci/conformance/probes.py` runs them on a provider and keeps what they said,
  each run's conclusion, its jobs' conclusions and their annotations, as JSON. `kiln` runs them on
  a server's native repository, `github` on a public repository the user owns (their token), and
  `compare` lists every difference (kiln's annotations counted as GitHub shows a job's, W06-R16).
  - On kiln (the conformance lab at 860ff42): matrix outputs take the last leg to finish, and
    an empty value written last leaves the one before it; a status function under cancellation
    runs as GitHub's do, and a cancelled step (in its job container) gets SIGINT, then SIGTERM 7 s
    on (7.5); 50 calls of different reusable workflows run, and a 51st fails the run there; 51 of
    the same one run; 11 levels of workflows run (the caller and ten nested); every annotation is
    kept (R16's 1,000); an oversized step summary is a warning. Numbers interpolate as
    `12345678901234567000`, `1000`, `0.1` and `16`.
  - Found on the way, and fixed: a cancelled step in a job container was never signalled (W05's
    AC07 note), and the probes' own matrix one didn't test an empty value written last.
- *Left.* AC01: the rest of pass 1023c; the corpus's macOS and Windows legs, which need such
  runners in the lab (user-assisted: joining a Mac and a Windows machine runs third-party CI code
  on them); the host executor's run of the repositories' workflows (W06-AC07), which needs a
  machine of its own to run them on; and test tenants (W11-AC03); AC03's GitHub half (user-assisted: the probes recorded once on
  a public repository the user owns, then `compare` run against kiln), and AC05; F-21's last two
  (`azure/login` with a tenant, `attest-build-provenance` with GitHub's Sigstore); an
  authenticated way for legs to read github.com's API (W18's egress proxy); scorecard's checks
  of a push (prometheus's, TypeScript's), which read the repository from an Enterprise Server's
  API only when `GH_HOST` names one, over HTTPS (the runner sets it only for a step that gives
  `gh` the job's token), and then sign their results through Sigstore's public instance (W18).
- *Decided with the user* (2026-10-23), for the gaps above and W08's, W13's, W15 to W18's, W20's
  and W23's (the plan "Closing KilnCI's remaining gaps", kept in kiln):
  - the lab's legs go back on networks of their own: the user adds a ufw rule letting Docker's
    address pools (172.16.0.0/12) reach the lab's port, which ends the host network's misses;
  - a Mac, jupiter (Windows) and an arm64 Linux VM on the Mac join the lab, each running the
    corpus's CI code as an unprivileged user of its own;
  - kiln's runner image gets an opt-in Android layer (cmdline-tools and an NDK, where GitHub's
    image has them), the SDK's licence accepted by the user once (uv's Android job; done, and
    the job passes);
  - jobs that run podman themselves go to microVM legs (rails' `rails-new-docker`; done, and the
    job passes);
  - AC03 and F-21's `attest-build-provenance` run on a public repository the user owns, with a
    fine-grained token kept as a kiln secret;
  - test tenants are free-tier AWS, Google Cloud and Azure accounts only, each with a $1 budget
    alert, trusting kiln's issuer (no stored keys);
  - KilnCI goes on in production once AC01's report is in, for W04-AC08, W05-AC11, W15's live
    criteria, W16-AC05, W20-AC02 and AC05 (the user syncs KilnCD);
  - shards and Slates (W13-AC06; W17's, W18's and W20's parts) come after the live cluster's
    criteria;
  - W23's test repositories are on free GitLab and Bitbucket accounts beside GitHub;
  - W08-AC08's release figure is read once from the bench machine's `ci-bench` job.

---

### W23 CI providers

**Scope.** CI is a provider choice, as CD is ([deploy-api.md](deploy-api.md) §8: KilnCD or Argo
CD). A repository's CI runs on **KilnCI**, the engine W01 to W22 build (GitHub Actions compatible,
independent of GitHub), or on **GitHub Actions**, **GitLab CI/CD** or **Bitbucket Pipelines**,
which kiln drives through their APIs. Whatever the provider, kiln's UI, API, CLI and MCP show and
act on the same things:
- runs, jobs, logs, reruns, cancels, dispatches, artifacts, approvals;
- through the repository's git host (deploy-api.md §7: kiln, GitHub, GitLab, Bitbucket), pull
  requests, reviews, comments and checks.

**Decisions.** As the user set them, 2026-10-18:

| # | Decision |
| --- | --- |
| P1 | CI is a provider choice, like CD: KilnCI, GitHub Actions, GitLab CI/CD, Bitbucket Pipelines |
| P2 | KilnCI is independent: full GitHub Actions compatibility (W01 to W22), no dependency on GitHub; GitHub is at most an identity provider (sign in, OIDC) and a git host |
| P3 | GitHub Actions, GitLab and Bitbucket are API integrations: kiln shows and acts on their CI, and on their pull requests, reviews and comments, in its own UI |
| P4 | One contract for every provider (R01): the UI, API, CLI and MCP don't change with the provider |
| P5 | Git hosts are deploy-api.md §7's one interface, shared with CD and built once; GitHub's App is the GitHub host's credential, not KilnCI's (W14-R10) |
| P6 | SSO with GitHub, GitLab and Bitbucket: sign-in, organization limits, and membership mapped to kiln's groups (R08) |

**Requirements**

- **W23-R01 The contract.** Every provider answers the same calls:

  | Call | What it does |
  | --- | --- |
  | `runs(repo, filter)` / `run(id)` | runs (workflow runs, pipelines) with their status, event, ref, commit, actor, times, link |
  | `watch(repo)` | changes as they happen: the provider's webhooks, then polling |
  | `jobs(run)` | jobs with their steps, status, runner, times |
  | `logs(job)` | a job's log, streamed while it runs where the provider allows |
  | `rerun(run, all \| failed \| job)` / `cancel(run)` | as the provider offers them |
  | `dispatch(workflow, ref, inputs)` | a manual run with inputs (`workflow_dispatch`, a pipeline with variables, a custom pipeline) |
  | `artifacts(run)` | list and download |
  | `approve(run, environment, approve \| reject, comment)` | a deployment waiting for review |
  | `workflows(repo)` | the CI files, checked (W02 for GitHub's YAML; GitLab's CI lint) |
  | `checks(sha)` | every provider's results for a commit, together |

- **W23-R02 KilnCI** is the engine of W01 to W22 behind the contract, in process.
- **W23-R03 GitHub Actions**, through GitHub's REST API with the GitHub host's credentials (an
  App installation, or a fine-grained token):
  - workflow runs, their jobs and steps;
  - logs (the job's log, followed while it runs);
  - rerun (all, failed jobs, one job), cancel, `workflow_dispatch`;
  - artifacts;
  - pending deployment reviews;
  - webhooks: `workflow_run`, `workflow_job`, `check_run`, `deployment_review`.

  GitHub.com and GitHub Enterprise Server.
- **W23-R04 GitLab CI/CD**, through GitLab's API with the GitLab host's credentials:
  - pipelines, their jobs and job traces;
  - retry, cancel, play (manual jobs), new pipelines with variables;
  - job artifacts;
  - protected environments' deployment approvals;
  - webhooks: pipeline, job and deployment events;
  - `.gitlab-ci.yml` checked with GitLab's CI lint.

  GitLab.com and self-managed GitLab.
- **W23-R05 Bitbucket Pipelines**, through Bitbucket Cloud's API with the Bitbucket host's
  credentials:
  - pipelines, their steps and step logs;
  - rerun (failed steps or all), stop, custom pipelines with variables;
  - deployments and environments;
  - webhooks for builds and commit statuses.
- **W23-R06 Pull requests on every host** in kiln's review pages (W14-R05, R06, W21): GitHub pull
  requests, GitLab merge requests and Bitbucket pull requests, as kiln's own diffs and change
  requests are shown:
  - the conversation, and threads on lines;
  - reviews: approve, request changes, comment;
  - merging by the host's methods (merge, squash, rebase or fast-forward);
  - every provider's checks;
  - kiln's inbox includes them.
- **W23-R07 Choosing.** Per repository (`PUT /api/ci/repos/:repo/providers`, the repository's settings
  page, `kiln repo provider <repo> <provider…>`). KilnCI is the default, so nothing changes for a
  repository until someone picks its host's CI. Several may run on one repository (KilnCI running
  `.github/workflows` beside GitHub Actions, say) when the user chooses so.
- **W23-R08 SSO** with GitHub, GitLab and Bitbucket (asked 2026-10-18), beside kiln's own sign-in
  (W10):
  - sign in with each (OAuth 2; GitLab's OIDC), GitHub.com and Enterprise Server, GitLab.com and
    self-managed, Bitbucket Cloud;
  - people matched to kiln users by their verified email or a linked account, or made on their
    first sign-in when the server allows it;
  - sign-in limited to chosen GitHub organizations, GitLab groups or Bitbucket workspaces;
  - membership mapped to kiln's groups and grants (W10-R02): a GitHub team, a GitLab group or a
    Bitbucket workspace gives roles in scopes (`developer@org:acme`), checked again on each sign-in
    and as membership changes;
  - one who signs in this way, or connects their account later, acts on the host as themselves:
    comments, reviews and merges made from kiln are theirs, not the App's.
- **W23-R09 One model.** A provider's runs map to kiln's runs, jobs and steps (W05's states), the
  provider's own words kept beside them, with links back to the provider's pages; KilnCI's
  features (insights, compare, log search, notifications) cover every provider's runs where the
  data allows, and say where it doesn't.

**Acceptance criteria**

- **W23-AC01 Contract.** The same API calls list, follow, rerun and cancel a run on each provider,
  against fakes of each API built from their documented shapes. *Integration test.*
- **W23-AC02 Providers.** Against test repositories on GitHub, GitLab and Bitbucket, each
  provider's run in kiln:
  - its jobs and logs, live;
  - rerun failed, cancel, dispatch;
  - an approval given from kiln.

  *E2E, user-assisted.*
- **W23-AC03 Pull requests.** A GitHub pull request, a GitLab merge request and a Bitbucket pull
  request reviewed from kiln's UI: comments on lines, an approval, a merge by each allowed method.
  *E2E, user-assisted.*

**Interfaces.** Uses deploy-api.md §7's git hosts (shared with CD), W10's identity, W11's
credentials, W14's review pages, W19's insights and notifications, and W21.

**Progress.** In progress.

- *Done.* `crate::githost`, one interface over kiln's own git server and GitHub, agreed with the
  deploy-api work, which codes against it; kiln's and GitHub's sides (W14-R10).
- *Done.* The provider core (`crates/kiln/src/ci/providers/`):
  - each repository's providers (`GET`/`PUT /api/ci/repos/:repo/providers`, KilnCI by default);
  - KilnCI stands aside for a repository whose CI is elsewhere;
  - other providers' runs, jobs and steps kept in kiln's tables with their provider, ids and
    pages (R09), so `/api/ci/runs` and the run page show them;
  - rerun, cancel, logs and `workflow_dispatch` routed to the run's provider (R01).
- *Done.* GitHub Actions (R03), through the App:
  - runs synced every 20 seconds and at once on `workflow_run` and `workflow_job` events;
  - job logs read through GitHub's redirect;
  - rerun (all, failed, a job), cancel, dispatch.
  - Checked against the fake GitHub (`w14_r10_github_app`).
- *Done.* For agents (the agentic work's C and K, which builds on it): who an agent's diff is for
  (`author_for`), and events for comments, reviews, a diff version's finished runs (`diff_run`,
  with the failed legs' excerpts) and the land queue (`queue`).
- *Done.* R08's SSO (`crates/kiln/src/authz/sso.rs`, `[sso.github]`, `[sso.gitlab]`,
  `[sso.bitbucket]`):
  - sign-in with OAuth 2's code flow (`/api/auth/:provider/start`, `/callback`; the providers at
    `/api/auth/sso`, each a button on the sign-in page), states used once, the way back always
    kiln's own;
  - the host's API says who signed in (a stable id, a verified email) and what they belong to:
    GitHub's organizations and teams, GitLab's groups, Bitbucket's workspaces;
  - sign-in limited to `allowed` memberships;
  - the kiln user: the linked account's, else the verified email's (users now have one), else a
    new one where `create_users` allows;
  - `grants` by membership, given at each sign-in and taken back when it's gone;
  - the host's token kept sealed with the linked account, for acting on the host as the person;
  - GitHub's falls back to the App's OAuth client when the manifest flow made one.
  - Checked against a fake of all three (`w23_r08_sso`).
- *Done.* GitLab and Bitbucket as git hosts (`crate::githost::gitlab`, `bitbucket`; `[[ci.gitlab]]`,
  `[ci.bitbucket]`), beside kiln's own and GitHub, for CD's release repositories and CI alike:
  - git reads, mirrors and pushes with their credentials (`http.<url>.extraheader` for what they
    cover);
  - commits made here and pushed with a lease on the head they were made on (GitLab's and
    Bitbucket's commit APIs build on whatever a branch holds by then);
  - merge and pull requests and their state, commit statuses (a release's too: Bitbucket has no
    deployments API outside Pipelines), GitLab's deployments;
  - push webhooks;
  - new repositories from a template (GitLab: a fork, then unlinked; Bitbucket: a new repository
    with the template's history);
  - KilnCI's runs of a repository there reported on its commit as a status (`kiln/<workflow>`,
    following the run, linking to it; `crate::githost::statuses`), as GitHub's get check runs;
    checked against a fake GitLab (`p3_gitlab_release_repositories_and_ci`).
- *Done.* GitLab CI/CD (R04) and Bitbucket Pipelines (R05) as providers:
  - pipelines and their jobs (Bitbucket's steps) kept as runs and legs, synced every 20 seconds and
    at once on signed webhooks (`POST /api/hooks/gitlab` with GitLab's `X-Gitlab-Token`;
    `POST /api/hooks/bitbucket` with `X-Hub-Signature`): GitLab's pipeline, job and deployment
    events, Bitbucket's build statuses;
  - a job's trace (a step's log) read when asked for;
  - GitLab: retry (failed jobs, a job), play (a manual job), cancel, a new pipeline with variables,
    `all` as a new pipeline when the branch is still at the run's commit;
  - Bitbucket: `all` as a new pipeline on the same commit and selector, stop, custom pipelines with
    variables. Bitbucket's API has no rerun of failed steps (its web page has); kiln says so.
- *Done.* The rest of R01's contract, for every provider:
  - `artifacts(run)`: `GET /api/ci/runs/:id/artifacts`, `…/artifacts/:artifact/download` (GitHub's
    artifacts, GitLab's job archives; Bitbucket's API serves none);
  - `approve(run, …)`: `GET /api/ci/runs/:id/pending`, `POST /api/ci/runs/:id/approve` (GitHub's
    pending deployment reviews, GitLab's protected environments' approvals), given as the person
    when they signed in with that host;
  - `workflows(repo)`: `GET /api/ci/repos/:repo/providers/workflows` (GitLab's CI lint);
  - `checks(sha)`: `GET /api/ci/repos/:repo/commits/:sha/checks`, every provider's runs of a commit;
  - `watch(repo)`: webhooks, the 20-second sync, and `POST /api/ci/repos/:repo/providers/sync`;
  - environments with their latest deployments on every provider, in `GET /api/ci/environments`.
  - Checked against fakes of all three APIs (`w23_ac01_contract`).
- *Done.* R06's API: the host's pull requests, merge requests and pull requests in one shape
  (`GET /api/ci/repos/:repo/pulls`, `…/pulls/:n` with the conversation, threads on lines, reviews,
  files, merge methods and every provider's checks; `POST …/comments`, `…/reviews`, `…/merge`),
  acted on as the person when they signed in with that host (R08's last part), else with kiln's
  credentials there.
  - Checked against fakes of all three (`w23_r06_pulls`).
- *Done.* Kiln's inbox (`GET /api/review/inbox`) has the hosts' changes asking for the person's
  review (`elsewhere`), through the accounts they linked by signing in; reviews, comments, merges
  and approvals they make from kiln are made with their own token on that host. Checked with a
  person signed in with the fake GitHub (`w23_r08_as_the_person`).
- *Done.* R07's CLI: `kiln repo provider <repo>` says what its CI runs on and what it may;
  `kiln repo provider <repo> <provider…>` chooses (checked in `w23_ac01_contract`).
- *Who builds what.* The review pages (W14-R06, AC06; R06 here) are the agentic work's, under
  W21's rules, against W14's APIs and R06's.
- *Done.* GitHub's review threads' resolved state, which only its GraphQL says: a pull request's
  threads (`reviewThreads`, by their first comments) asked of the App's GraphQL endpoint (an
  Enterprise Server's `/api/graphql` beside its `/api/v3`); when it won't say, a thread's state
  is unknown, as it was (checked against the stand-in GitHub in `w23_r06_pulls`).
- *Done.* R07's page: a repository's panel on the Repositories page says what runs its CI when its
  host makes a choice possible (KilnCI, and GitHub Actions, GitLab CI/CD or Bitbucket Pipelines),
  each a checkbox, saved together (at least one). Checked in Chromium and WebKit, desktop and
  phone, on a repository on a GitLab (`ci/ui/people_check.mjs`).
- *Done.* The review pages show the hosts' pull requests (R06's API): a repository's open ones at
  `/review/hosts/:repo` (linked from its panel on the Repositories page), and one at
  `/review/hosts/:repo/:number` with its checks, conversation, threads on lines and files in
  kiln's diff view, comments on lines and replies going out at once (a host keeps no drafts), a
  review (approve, request changes, comment) and a merge by each method the host allows; the inbox
  lists the ones asking the person's review. Checked in Chromium, desktop and phone, against the
  mock's GitLab merge requests (`ui/e2e/host-pulls.spec.ts`).
- *Left.* AC02 and AC03, which need test repositories on GitHub, GitLab and Bitbucket and their
  credentials (user-assisted).

### W24 Incremental CI: fingerprints, reuse, warm state and shared caches

**Scope.** The least work a change needs, for every kind of work kiln runs: CI legs, `kiln run`
and agents' builds. Every unit of work (a run, a leg, a step, a tool's action, a test) is keyed by
a fingerprint of what it depends on and recorded once in a ledger. A recorded success is reused;
what can't be keyed exactly runs incrementally on kept ("warm") state; what tools produce goes
into one content-addressed cache shared across the fleet; and the planner works out from the
change what it could affect before anything runs. Every reuse, warm start and rebuild says why.
W24 extends W08 (R09's cache tiers, R10's job-result reuse) and W05-R23's preferences, and is
built on the CI engine alone.

**Why.** As of 2026-10-24 (origin/main a34c9139), nothing keeps work between runs:

- v1 deletes each job's workspace (`exec.rs`); the engine deletes each leg's
  (`runner/runtime.rs`); pools discard each executor after one leg (`runner/pool.rs`).
- v1's `cache:` lists and `toolchain_cache()` have no effect in v1 or in the engine.
- `kiln run` deletes the other machine's build output at every sync (`reach/sync.rs`,
  `Mirror::commit`).
- kiln's own `.kiln/` workflows run on v1 (`[ci] kiln_workflows` is off), so W08's reuse and cache
  tiers don't reach them.

The cost: on one runner, 53 separate cargo target directories held 880 GiB, and builds took
production down on 2026-10-19, 2026-10-21 and 2026-10-22. The evidence, every claim graded from
its sources, is kiln research `01a16c5c4b6e760cb96b99da91863dad`.

**Decisions.** These are defaults; the person may change any of them.

1. **The Remote Execution API's shapes are the core types.** Digests, Merkle trees of inputs and
   action results take the shapes of `build.bazel.remote.execution.v2`. Bazel- and Buck2-family
   clients then map one to one, and kiln's own steps are actions whose inputs are observed rather
   than declared.
2. **The CI engine only; v1 frozen.** Nothing of W24 goes into `exec.rs`, which is to become a
   shim over `kiln_ci::wf` (§2.2). kiln's own `.kiln/` workflows move onto the engine first, since
   W24 can reach only what the engine runs.
3. **Person scopes, promotion by reproduction.** Agents' own builds write to their person's scope
   and read trusted entries. An untrusted or person-scoped result becomes trusted only when a
   trusted run reproduces it and writes its own entry, never by copying. Agents' results can't
   poison what release jobs read, yet the work they did is reused once confirmed.
4. **Protected units: verified by two runners, or cold.** Release and deploy legs use only
   results that two independent trusted runners produced identically, else they run cold in a
   hermetic boundary; their provenance says which. A single compromised runner can't slip an
   artifact into a release.
5. **Remote execution last.** It needs Hex's placement (CP-T7) and sandboxes; the data model is
   ready for it from the start, so nothing built earlier has to change.

**Requirements: the model**

- **W24-R01 Fingerprints.** `kiln_ci::work::key`, pure: SHA-256 over a canonical encoding
  prefixed with `kiln/work/v1` and the unit's kind, so granularities never collide and a version
  bump invalidates everything. Its components:
  - the definition: the job or step after evaluation, secrets by name only;
  - the environment: declared `env:`, `memo.env` and each toolchain's known variables; kiln's
    per-run variables only when the unit's text uses them or they're observed being read;
  - the platform: OS, architecture, libc, executor, image digest and toolchain identity (a hash of
    `rustc -vV`, a sysroot digest);
  - the inputs: git tree ids of declared paths, computed from the mirror with no checkout, and
    observed path sets;
  - upstream output digests and the trust domain.

  Keys use tree ids, never commit SHAs. A weak key holds what's known before the unit runs; its
  strong key adds the digests of what it read.
- **W24-R02 The ledger.** Results keyed by strong key: outcome, exit code, outputs (files, trees,
  job and step output values) as content digests, logs and test reports, path set, provenance
  (repository, run, leg, step, attempt, runner, commit, tree, duration, CPU), trust domain,
  lineage, verifications and signatures. Families `work.result`, `work.weak` (a weak key's path
  sets, at most 8), `work.pathset`, `work.snapshot`, `work.lane`, `work.graph`, `work.test` and
  `work.verify`, keyed under `owner/repo|domain|`. They live in SQLite through a numbered migration
  now and in Amphora through the `Records` seam later. Bytes stay in the content-addressed store
  (`cas/<sha256>`, through `Blobs`).
- **W24-R03 Early cutoff.** A unit's key takes its upstream units' output digests, never their
  keys. A unit that reruns and produces byte-identical outputs leaves everything after it
  reusable.
- **W24-R04 Two-phase lookup.** For each weak key the ledger keeps up to 8 path sets of what
  earlier runs read. A lookup computes each candidate's strong key from current digests (tracked
  files' from git's index, at no cost) and takes the first recorded match. Path sets come from,
  best first:
  1. the tools' own records: cargo's dependency files and build-script directives, Go's actions
     through `GOCACHEPROG`, `.tsbuildinfo`, coverage data;
  2. in-process interposition: `LD_PRELOAD`, and `DYLD_INSERT_LIBRARIES` for toolchains macOS
     doesn't harden;
  3. seccomp user notification on Linux, unprivileged once `no_new_privs` is set;
  4. declared inputs;
  5. the whole tree, as a last resort.

  The server adds what it saw a leg fetch (artifacts, package-proxy downloads). A leg the egress
  proxy saw reach anywhere else is marked non-hermetic.
- **W24-R05 Secrets and volatile values.** A unit that receives secrets isn't reused unless it
  says `memo.secrets: hmac`: an HMAC under the server's key, so rotating the secret invalidates
  it. Run ids, attempts, tokens and temporary paths never enter a key unless the unit's text uses
  them.

**Requirements: reuse and caches**

- **W24-R06 Leg reuse.** Dispatch keys every leg that opts in and looks it up in the ledger before
  queueing it. W08-R10's `reuse:` becomes sugar for `work.memo`: its `paths`, `env` and `max-age`
  keep their meaning, and W08-AC06's tests keep passing unchanged. A leg the change can't affect
  finishes `reused`, naming its source, never `skipped`, so required checks stay green with
  provenance.
- **W24-R07 Step reuse.** `memo: check` (nothing to restore: lint, tests) or `memo: { outputs }`
  on a step. The runner looks the step up before running it, and a hit replays its log, exit
  code, outputs and test reports. Outputs are materialized only when a later step misses. Only
  successes are recorded; a failure, a cancellation or a timeout never is.
- **W24-R08 The runner's cache service.** Each runner serves one cache to its legs, its `kiln run`
  commands and its agents:
  - a local store at `<work>/_cas`: whole files, content-defined chunks for large ones;
  - endpoints on loopback and a unix socket, with tokens scoped to repository, domain and
    read or write;
  - writes going up to the server by scope, and existence checks scoped by repository, as
    `ci/services/chunks.rs` already does.

  It speaks the protocols tools already use: sccache over WebDAV, Bazel's HTTP cache (`/ac/`,
  `/cas/`), Gradle's and ccache's HTTP caches, Turborepo's API, and `kiln gocacheprog` for Go.
- **W24-R09 kiln rustc.** A rustc wrapper, sccache-compatible for library crates, that also
  caches proc-macro, bin and test units, which sccache can't because they invoke the linker. They
  are keyed by toolchain and linker digests, extern rlib digests, the dependency files' sources
  and environment variables, and flags with paths remapped. Crates under edit stay incremental in
  their warm workspace.
- **W24-R10 Test results.** Keyed by the test binary's digest, its arguments, the environment and
  files it read (Go's rule) and the platform. Passes are reused, failures never. Selection runs
  the test binaries of affected packages whose digest changed; unchanged ones replay their
  results. nextest archives build once and run across machines, balanced by W16-R06's timings, and
  W16-R04's flaky marks use these keys.

**Requirements: warm state**

- **W24-R11 Warm workspaces.** A runner keeps workspaces per lane at stable paths,
  `<work>/warm/<repo>/<lane>/<n>`, each leased to one leg at a time. A lane is a hash of the
  repository, the job, the matrix values that matter, the toolchain identity and the profile. A
  leg:
  1. takes the workspace closest to its commit;
  2. checks out in place (`git checkout --force`, so only changed files get new mtimes);
  3. cleans everything but its kept folders, and runs.

  A second leg of the lane gets a copy-on-write clone: reflink on btrfs and XFS, clonefile on APFS,
  overlayfs elsewhere on Linux. NTFS keeps workspaces in place, without clones. An untrusted leg
  works on a throwaway clone that never comes back. A workspace whose holder died is reset or
  dropped. W02-R06's v1 `cache: [dirs]` become `work.warm` folders, as that requirement promised.
- **W24-R12 Snapshots.** A trusted warm-up leg on trunk, or a trusted leg's workspace in the
  background, is kept as a snapshot: a Merkle tree, with file modes and mtimes, over objects in
  the content store. Only new objects are uploaded. A runner with no workspace for a lane:
  1. builds one from its local objects, cloned where the filesystem can;
  2. fetches only the missing chunks and sets mtimes from the snapshot;
  3. checks out its commit in place.
- **W24-R13 Warm microVMs.** Firecracker legs get a per-lane ext4 scratch image, cloned per leg and
  discarded, so each starts clean and warm, and a booted guest restored from a diff memory snapshot
  loaded lazily through userfaultfd. Pools (W13-R09) keep warm snapshots. shards uses its
  saved-microVM copies, and Slates volumes once they can clone snapshots.
- **W24-R14 Warm k8s.** Per-node `hostPath` workspaces with node affinity, or per-lane volumes
  cloned through CSI snapshots where the cluster supports them.
- **W24-R15 Budgets.** A byte budget per runner and per lane; least recently used workspaces go
  first; a bloated workspace is reset to trunk's snapshot; below a disk floor nothing new is kept.
  Accounting knows about clones where the filesystem reports shared extents.

**Requirements: planning and scheduling**

- **W24-R16 Package graphs.** `kiln_ci::work::graph` reads package graphs from manifests in the
  mirror as pure code, through `PlanSource::graph()`: Cargo workspaces and `Cargo.lock`; npm, pnpm
  and yarn workspaces; uv workspaces; `go.mod` and `go.work`. Finer graphs (`go list`,
  `bazel query`, `cargo metadata`) come from cached actions on runners.
- **W24-R17 affected().** An expression function: the run's changed files (W03-R12) mapped to
  their packages, plus everything that depends on them. It is usable in dynamic matrices
  (`matrix: { crate: ${{ affected('cargo') }} }`) and in conditions. Every run names its baseline:
  - generally, the nearest ancestor with a complete trusted run of the workflow;
  - for a diff, its trunk base;
  - for a land candidate, its tested version plus trunk's changes since.
- **W24-R18 Warm-aware placement.** W05-R23 gains a `warm` term: expected finish time = queue wait
  + cold duration × (1 − warmth on that runner) + missing bytes ÷ bandwidth, from history and
  workspace distance. `affinity: { job }` gets real workspaces behind it. A leg may wait for a busy
  warm runner up to a share of what it would save, with `WaitReason::Warm { runner, saves_s, until
  }`, explained in `ci.legs.routing`.
- **W24-R19 Admission by load.** Runners report I/O pressure (Linux PSI) and free disk in their
  facts. The scheduler holds heavy legs off a machine past its thresholds, saying why, and honours
  the `Sandbox` seam's I/O weight. Hex's placement takes this over (CP-T7).
- **W24-R20 Warm-up and the land queue.** After each land, a low-priority, preemptible warm-up
  leg per lane and runner refreshes trunk's warm state. A land candidate (W14-R08) reuses trusted
  entries for packages its diff didn't touch, and bisection steps share them; a diff's untrusted
  results are never reused.

**Requirements: trust**

- **W24-R21 Domains and lineage.** The domains:
  - `trusted`: default-branch pushes and the land queue;
  - `protected`: release and deploy legs;
  - `untrusted`: forks and outside contributors, and by default `diff` and `agent` events, as
    W08-R14 has them;
  - `person:<id>`: agents' own builds.

  Reads go down only: untrusted and person scopes read trusted entries; trusted never reads either
  (W18-R07). Every entry and workspace records the domains that wrote it. Promotion is by
  reproduction only. Quotas per domain stop one domain from evicting another's entries.
- **W24-R22 Signatures and provenance.** The producing runner signs each result with its identity
  key; the server countersigns it into a trusted scope; readers check both. Entries are never
  overwritten. A protected unit's provenance (W18) lists every reused unit with its producer and
  its verifiers.
- **W24-R23 Verification.** kiln re-executes a sample of hits: about 1 % of hermetic units, 5 % of
  sandboxed ones, 10 % on the host. Units whose outputs differ are marked nondeterministic (shown,
  with early cutoff turned off for them); a lineage whose outcome differs is quarantined.
  Protected workflows run cold nightly, and `kiln rerun --cold` runs with no reuse, no warm state
  and no cache.

**Requirements: everywhere else**

- **W24-R24 kiln run and agents.** `Mirror::commit` deletes only the paths it sent before and no
  longer lists, so build output survives. Each copy is a warm workspace, and a command that opts
  in returns the recorded result for an identical tree. Agents' harnesses get the cache variables
  (`RUSTC_WRAPPER`, `GOCACHEPROG`, `TURBO_API`, …) under their person's scope, and a new worktree
  gets a copy-on-write clone of the nearest warm build folder.
- **W24-R25 GitHub Actions compatibility.** Overlays (W02-R07) add `work:` keys to GitHub
  workflows without editing them. `actions/cache` keeps its semantics; its restore becomes a no-op
  when the warm workspace's tree already equals the entry's.
- **W24-R26 Remote execution.** kiln serves the Remote Execution API (CAS, action cache,
  `Execute`) over `kiln-http`'s HTTP/2; gRPC needs trailers, which are raised rather than worked
  around if missing. Actions queue in a pure `kiln_ci::work::exec`, are placed through
  `Placement`, and run in `Sandbox` boundaries: host namespaces with no network for trusted
  actions, microVMs for untrusted ones. Their inputs come only from the content store. `kiln
  rustc` gains a remote mode, with toolchains shipped as trees.
- **W24-R27 Peers.** A runner fetches blobs from LAN peers over reach's end-to-end links, each
  checked by digest. The directory says which runner holds what, and local files seed transfers.
- **W24-R28 Reports and why.**
  - Every job and step says whether it ran, was reused (from which run, on which runner) or was
    warm (which workspace, how far behind, files changed, units rebuilt according to cargo's
    `fresh`), with cache hits and time saved.
  - "Why did this rebuild?" compares each fingerprint component with the nearest earlier run.
  - Insights show time saved, hit rates per layer, cold against warm durations, and time saved per
    GiB kept, which drives eviction.
  - Metrics: `kiln_work_lookups_total{layer,outcome}`, `kiln_work_saved_seconds_total`,
    `kiln_warm_slots{state}`, `kiln_work_verify_mismatch_total`.
  - Operations: `kiln work why|status|warm|verify|gc` on the CLI, `ci.work.*` for agents.

**Design.**

- **Pure decisions** in `crates/kiln-ci/src/work/` (`key`, `ledger`, `pathset`, `slots`, `graph`,
  `tests`, `verify`, `exec`), with property tests and simulators. Elsewhere in `kiln-ci`:
  - `plan` gains `affected()` and `LegSpec.work`;
  - `sched` gains the `warm` term, patience, `WaitReason::Warm` and admission by load;
  - `wf` and `expr` gain `work:`, a step's `memo:`, `tests:` and `affected()`;
  - `insights` and `compare` gain time saved and fingerprint differences.
- **The server**: `crates/kiln/src/ci/work/` holds the ledger, `/_kiln/work/v1/*`, the Remote
  Execution API, the cache protocols for runners without the local service, cleanup and
  verification scheduling. `ci/dispatch.rs` gets leg reuse on the ledger (replacing `reuse.rs`),
  placement inputs and the land-queue rules.
- **The runner**: `crates/kiln/src/runner/work/` holds the cache service, the local store, warm
  workspaces, materializing, observation, `kiln rustc` and `kiln gocacheprog`. `runner/runtime.rs`
  takes or builds a workspace when a leg starts, looks steps up, and snapshots and releases the
  workspace when it ends. `reach/sync.rs` and `harness/*` change as R24 says.

```yaml
jobs:
  rust:
    toolchain: rust            # preset: warm [target], actions [rustc], tools [rustc, cargo]
    work:
      memo: { inputs: [crates/**, Cargo.lock], observe: true, max-age: 14d }
      warm: { paths: [target], budget: 60Gi }
      hermetic: sandboxed      # none | sandboxed | strict: who may share its results
      verify: 5%
    steps:
      - run: cargo clippy --locked --all-targets -- -D warnings
        memo: check
      - run: cargo nextest run --locked
        tests: { select: affected, cache: true }
```

**Interfaces.**

- **Wire**, added after what's there: optional `AssignmentV2.work`;
  `JobEventV2::Work { idx, outcome, key, saved_ms, detail }`; `RunnerFacts.work` (clone kind,
  observation methods, store size, I/O pressure); the `CAP_WORK` capability bit and
  `AgentMsg::WorkState` in `proto.rs`.
- **Seams**: the ledger on `Records`; bytes on `Blobs` (Mantle, CP-T10); remote execution on
  `Placement` and `Sandbox` (Hex, CP-T7).
- **Takes** W03's changed files, W05's routing, W08's store and cache scopes, W13's executors,
  W16's test reports and W18's trust.
- **Gives** W14's land queue its reuse, W16 and W19 their keys and reports, and W20's agents
  `ci.work.*`.

**Acceptance criteria**

- **W24-AC01 Fingerprint sensitivity.** Each of 10⁵ random single-component edits changes the key,
  and edits outside the components never do. *Property test.*
- **W24-AC02 Warm equals cold.** On 20 commits from kiln's history, warm and cold builds give the
  same conclusions and test outcomes. *Differential integration test.*
- **W24-AC03 kiln's own CI.** The `rust` job takes under 60 s for a docs-only change, and at most
  30 % of M0's cold baseline for a one-file change in `crates/kiln`, median of 3 runs. *E2E on the
  live server.*
- **W24-AC04 Agents share builds.** A second agent's first build of kiln on the same machine
  recompiles at most 5 % of units, counted from cargo's `fresh`. *Integration test.*
- **W24-AC05 Disk.** Ten agents' target folders on btrfs or APFS hold at most 1.3 × one folder's
  exclusive bytes. *Measurement.*
- **W24-AC06 Poisoning.** A fork's build script that plants a poisoned rlib never reaches a trusted
  leg, and the attempt is audited. *Adversarial E2E.*
- **W24-AC07 Verification.** Corrupted results and injected nondeterminism (the `chaos` feature)
  are each detected, and their lineage quarantined. *Chaos test.*
- **W24-AC08 Tools' clients.** Pinned sccache, Go (`GOCACHEPROG`), Bazel, Gradle, ccache and
  Turborepo clients each build a fixture twice against a runner, the second build served from the
  cache, on Linux and macOS. *Conformance suite.*
- **W24-AC09 Every reason.** 100 % of a run's jobs and steps say why they ran, were reused or were
  warm, and the why-diff names exactly the changed component in a table of edits. *Unit tests and
  E2E.*
- **W24-AC10 Lookup latency.** Ledger lookups take p99 under 5 ms on the server and under 1 ms on
  a runner. *Bench.*
- **W24-AC11 Clone speed.** Cloning a 30 GiB warm workspace takes under 10 s on btrfs or APFS.
  *Bench.*
- **W24-AC12 Eviction.** Under quota pressure, eviction keeps the entries with the most time saved
  per byte. *Simulation.*

**Progress.** Specified (2026-10-24); nothing built yet. The milestones, each a goal with a kiln
plan:

| Milestone | What it delivers | Requirements |
|---|---|---|
| M0 | Baseline measured; `kiln run` keeps build output; v1 `cache:` folders on the engine; `.kiln/` planned on the engine | R11's v1 folders, R24's sync |
| M1 | Ledger, leg and step reuse, warm workspaces on host and Docker, placement, reports | R01–R03, R06, R07, R11, R12, R15, R18, R28 |
| M2 | The runner's cache service, agents, peers | R08, R24, R25, R27 |
| M3 | Graphs, `affected()`, tests, the land queue, admission | R10, R16, R17, R19, R20 |
| M4 | Observation, `kiln rustc`, trust, verification | R04, R05, R09, R21–R23 |
| M5 | Warm microVMs and k8s, remote execution, the seams | R13, R14, R26 |

### W28 Demand-driven CI: adopt, cover, stagger, land by conflict graph

**Scope.** The land queue, builds and deploys of a trunk many agents change at once (kiln plan
`01a17502affc74b3af5a30092f8418bc`). This section holds step 9's requirements, R30–R39 (kiln plan
`01a1751638e773b98b5092662fc06bf5`); the other steps take their ranges as they come. Commits
before this one call this workstream "W25 step N".

**Requirements: the conflict graph**

- **W28-R30 Pending changes.** `kiln_ci::land::conflict`, pure: a change pending in the land queue
  is the files it changes, the units those files affect and the deploy targets it changes. A file
  is an input of the unit whose folder holds it most closely, one per ecosystem, and of every unit
  that reads it (a workspace's root manifest and lockfile). A change affects those units and every
  unit that depends on one of them, transitively (W24-R17). The graph is the union of the trunk's
  and each pending change's own (W24-R16 read with the change applied), so a dependency one change
  adds links it to the changes of what it then depends on.
- **W28-R31 Components.** Two changes conflict when they share a changed file, an affected unit or
  a deploy target. The conflict graph's connected components come from a union-find over the
  queue's changes, so two changes are in one component exactly when a chain of conflicts links
  them. Each component is a queue of its own, and a change that conflicts with nothing ahead of it
  lands at once. W24-R30's e-graph is a union-find over result keys within a trust domain;
  changes aren't keys, so the conflict graph keeps a union-find of its own.
- **W28-R32 Interaction and joint units.** A component's interaction units are those whose inputs
  take files from two or more of its changes: where its changes meet. Its joint units are those
  that two or more of its changes affect: the interaction units, what depends on them, and what
  depends on units that different changes affect. Only joint units get keys that no change gives
  alone, so only they are built and tested together. Every other unit keeps the key it has with
  its own change alone, or the trunk's.
- **W28-R33 Landing classes.** Changes no two of which conflict commute: every order of them gives
  the same tree, and every unit the key it has with its own change alone. They land as one class,
  whose canonical form is the sorted set of their ids, so every order of them is the same class.
- **W28-R34 Every decision says why.** Each change is decided against each change ahead of it in
  the queue. `waits for D9: both affect crates/kiln-ci (unit kiln-ci)` names what the two share:
  files first, then the units where they meet (those none of whose dependencies both affect), then
  deploy targets. `lands beside D12: no shared unit, file or target` says they share nothing.

**Acceptance criteria**

- **W28-AC30 Commuting changes.** Over 10,000 random queues on synthetic package graphs: two
  changes share a component exactly when a chain of shared files, affected units or deploy targets
  links them; interaction units are exactly those whose inputs take files from two or more of a
  component's changes; for every set of changes no two of which conflict, every order gives the
  same tree and every unit the key it has with its own change alone; and every decision names
  only what its two changes share. *Property test* (`cargo test -p kiln-ci land::conflict`).

**Progress.** R30–R34 and AC30 built (2026-10-25) in `kiln_ci::land::conflict`. The land queue
doesn't use them yet: that is step 9's third step, in `crates/kiln/src/ci/land.rs`.

**Requirements: demands and adopt-on-supersede** (step 4's, R40–R49; kiln plan
`01a17607cd1273c5836771efb9d377d7`)

- **W28-R40 Demands.** `kiln_ci::sched::demand`, pure: each run is a demand, with its lane (the
  repository, the workflow, and the branch, stack or pull request W05-R08's `when` names), its
  commit, its intent (gate, waiting, trunk or wip), its actor and its trust domain. A demand that
  arrives under `supersede: { action: adopt }`, its branch not in `keep`, withdraws every older
  live demand of its lane, and cancels nothing itself. Older is by the run's creation, so a run
  approved late (W18-R06) doesn't withdraw a newer one; a rerun's demand is live again and
  withdraws nothing.
- **W28-R41 Domains.** A leg adopts only executions its domain may read: its own domain's, and for
  untrusted and person scopes trusted ones (W24-R21). Protected adopts nothing, as W24 Decision 4
  lets it take only results two trusted runners produced identically.
- **W28-R42 Single-flight.** Work in flight is keyed by its key and domain; the key is W08-R10's
  input hash until W24's strong keys exist. A leg with a key adopts the execution of its key in
  flight that its domain may adopt (its own domain's first, then a trusted one), or executes it:
  one key never has two executions at once within a domain. A leg of a withdrawn run only takes
  what's in flight. An execution's priority is the highest among the live demands waiting on it.
  After a restart, a queued duplicate of a running execution adopts it. Ledger hits (W24-R06) stay
  the dispatcher's, before a leg is queued: the table never reads storage.
- **W28-R43 Work no one wants.** After a withdrawal, or when a waiter leaves, an execution live
  demands still want is kept, at their highest priority. One no live demand wants is dropped if it
  hasn't started (or waits for a retry). Running, it runs on while nothing contends for its
  capacity. Once live work does, it finishes only if its expected remaining time (its cost less its
  attempt's time so far) is within `finish_within` (300 s by default, from W25 step 1's sweep) and
  it seeds trusted entries. Otherwise it is cancelled, naming the warm-workspace lane (W24-R11) its
  workspace goes back to. Nothing is removed from the ledger.
- **W28-R44 Endings.** Adopters take an execution's result over, reused and naming it, when it
  succeeded or failed in class `failure`; never for runner-lost, infra, image-pull, oom, disk-full,
  timeout or a cancel. In those cases the live adopters ask again in the order they adopted: the
  first executes in its place, and the rest adopt it where their domain may. Withdrawn adopters
  take what's in flight or are dropped. An adopter of an execution that read observed path sets
  checks them at its own commit first (W24-R04), and runs itself on a mismatch. Every decision says
  why.
- **W28-R45 The scheduler.** `kiln_ci::sched::state` owns the demand table. A run's demand
  (`Input::Demand`) comes before its `RunQueued`, whose `Supersede::Adopt` withdraws older demands
  on its lane. A leg with work (`LegIn.work`) asks the table as it would be queued. Adopting, it
  holds no runner, slot or quota, and waits (`WaitReason::Adopted`, naming the leg and run).
  Executing, it's queued as ever, at the highest priority its live demands give. A withdrawn
  execution still queued is dropped, settled cancelled with why. Running, it goes on until live
  work that can't be placed contends for a runner or pool it could use. Then, of those, the one with
  the most time left is cancelled through the cancelling path, unless it finishes within
  `finish_within` seeding trusted entries. When an execution settles, its adopters end as
  `Command::Adopted` (reused from it) or carry on as R44 says. Every decision is a
  `Command::Demand`, said once, with why. After a restart, a running leg announced with work takes
  over a queued duplicate of it.

**Acceptance criteria: demands**

- **W28-AC40 Single-flight within domains.** Over 10,000 random sequences of arrivals, asks,
  starts, endings in every class, fates, departures, checks and restarts: no key ever has two
  executions at once within a domain; every adoption is one its domain may make; only a live run's
  leg executes, and only a withdrawn one's is dropped; every leg ends exactly once; and the same
  steps give the same table. *Property test* (`cargo test -p kiln-ci sched::demand`).
- **W28-AC41 Two pushes through the scheduler.** Push A, then B touching only docs/ while A's rust
  leg runs: B's rust leg adopts A's, with one assignment, both settling green, A never cancelled.
  B touching crates/ while A's rust leg is 30% done and both slots are busy: A's rust leg is
  cancelled, saying no live demand wants it and it had 140 s left; B's ui leg adopts A's; B's rust
  leg runs once A's stopped. After a restart that queues B's leg before A's running one is
  announced, the work runs once. *Simulation test* (`cargo test -p kiln-ci sched::tests::w28`).

**Progress (step 4).** R40–R45, AC40 and AC41 built (2026-10-25) in `kiln_ci::sched::demand` and
`kiln_ci::sched::state`. The dispatcher and step 1's replay don't use them yet: those are the
plan's steps 3 and 4.

## 4. API, events, CLI and MCP reference

All routes come from the one route table (W10-R05, W20-R01) and follow access.md's conventions:
cursor pagination (`?cursor=&limit=`), `Idempotency-Key` on POSTs that start things, ETags on reads,
errors as `{ error, code, details? }`. Today's routes (`/api/runs`, `/api/jobs/...`,
`/api/repos/:id/trigger`) keep working; v1 views read the same tables.

### 4.1 Routes

| Method | Path | Op name | Capability | WS |
| --- | --- | --- | --- | --- |
| GET | `/api/ci/repos/:repo/workflows?ref=` | `ci.workflows.list` | runs.read | W02 |
| POST | `/_kiln/work/v1/lookup`, `/_kiln/work/v1/record`, `/_kiln/work/v1/pathsets` | (leg and runner tokens) | work scopes | W24 |
| POST, PUT, GET | `/_kiln/work/v1/missing`, `/_kiln/work/v1/blobs/:sha256`, `/_kiln/work/v1/trees/:sha256` | (leg and runner tokens) | work scopes | W24 |
| GET | `/api/ci/work/why?unit=` | `ci.work.why` | runs.read | W24 |
| GET | `/api/ci/work/status?runner=` | `ci.work.status` | runs.read | W24 |
| POST | `/api/ci/work/warm`, `/api/ci/work/verify` | `ci.work.warm`, `ci.work.verify` | runs.write | W24 |
| POST | `/api/ci/work/gc` | `ci.work.gc` | admin | W24 |
| GET | `/api/ci/repos/:repo/workflows/:path?ref=` | `ci.workflows.get` (source, IR, diagnostics) | runs.read | W02 |
| POST | `/api/ci/validate` | `ci.workflows.validate` | runs.read | W02 |
| POST | `/api/ci/plan-preview` | `ci.workflows.plan_preview` | runs.read | W04 |
| GET | `/api/ci/schema/:kind` | `ci.workflows.schema` | none (public within the API) | W02 |
| POST | `/api/ci/repos/:repo/workflows/:path/edit` | `ci.workflows.edit` (→ a diff, a change on the repository's host, a commit to a branch, or a patch) | workflows.edit | W02, W14, W21 |
| GET | `/api/ci/runs?repo=&workflow=&branch=&event=&conclusion=&actor=&runner=` | `ci.runs.list` | runs.read | W04 |
| GET | `/api/ci/runs/:id` | `ci.runs.get` | runs.read | W04 |
| GET | `/api/ci/runs/:id/graph` | `ci.runs.graph` (with critical path) | runs.read | W04, W19 |
| POST | `/api/ci/repos/:repo/dispatch` | `ci.runs.trigger` (`workflow_dispatch`) | runs.trigger | W03 |
| POST | `/api/ci/repos/:repo/repository-dispatch` | `ci.runs.repository_dispatch` | runs.trigger | W03 |
| POST | `/api/ci/repos/:repo/agent` | `ci.runs.ask` (kiln's `agent` event: an agent asks for a run) | runs.trigger | W03, W20 |
| GET | `/api/ci/repos/:repo/heads` | `ci.uploads.heads` (the commits a pack can leave out) | runs.read | W17 |
| POST | `/api/ci/repos/:repo/packs?sha=` | `ci.uploads.pack` (a working tree's commit, as a thin pack) | runs.trigger | W17 |
| POST | `/api/ci/repos/:repo/local-runs` | `ci.runs.local` (`{ sha, ref, event, payload?, workflows?, job?, matrix?, inputs?, vars?, secrets? }`; untrusted unless the owner's) | runs.trigger | W17 |
| POST | `/api/ci/runs/:id/cancel` | `ci.runs.cancel` | runs.cancel | W05 |
| POST | `/api/ci/runs/:id/rerun` | `ci.runs.rerun` (`{ mode: all\|failed\|job, job?, debug?, fresh_actions?, no_reuse? }`) | runs.rerun | W05 |
| POST | `/api/ci/runs/:id/approve` | `ci.runs.approve` (untrusted run or environment) | runs.approve | W15, W18 |
| GET | `/api/ci/runs/:id/{summary,annotations,artifacts,tests,flow,metrics,attestations}` (built; the actions it pinned come with the run) | `ci.runs.summary`, `ci.runs.annotations`, `ci.artifacts.list`, `ci.tests.run`, `ci.runs.flow`, `ci.metrics.run`, `ci.runs.attestations` | runs.read | W06, W08, W16, W04, W18 |
| GET | `/api/ci/legs/:id/tests` · `/api/ci/repos/:repo/tests?sort=failing\|slowest\|flaky` | `ci.tests.leg`, `ci.tests.repo` | runs.read | W16 |
| PUT | `/_kiln/test-results?step=&format=&path=` | a leg's report (its runtime token) | the leg | W16 |
| GET | `/api/ci/runs/:a/compare/:b` | `ci.runs.compare` | runs.read | W19 |
| GET | `/api/ci/runs/:id/compare/:other` · `/api/ci/legs/:id/compare/:other` (built; `:other` may be `last-green`) · POST `/_kiln/facts` (a leg's) | `ci.runs.compare`, `ci.legs.compare` | runs.read; the leg's token | W19 |
| GET | `/api/ci/legs/:id` | `ci.legs.get` (attempts, steps, outputs, wait reason) | runs.read | W05, W06 |
| GET | `/api/ci/legs/:id/logs?step=&attempt=&from=` · `/logs/ws` | `ci.legs.logs` (`from`: a byte offset, for followers) | logs.read | W06 |
| GET | `/api/ci/legs/:id/logs/search?q=` · `/failure` | `ci.legs.search`, `ci.legs.failure_excerpt` | logs.read | W19, W20 |
| GET | `/api/ci/logs/search?q=&repo=&workflow=&branch=&conclusion=&days=` (built) | `ci.logs.search` | logs.read | W19 |
| GET | `/api/ci/legs/:id/routing` | `ci.legs.routing` (explain) | runs.read | W05 |
| GET | `/api/ci/legs/:id/terminal` (WebSocket) | a terminal (agents run commands with `ci.legs.exec`) | runs.debug | W17 |
| GET · PUT · DELETE | `/api/ci/oidc/subject?scope=` | `ci.oidc.subject`, `ci.oidc.set_subject` (`{ use_default, include_claim_keys }`), `ci.oidc.reset_subject` | runs.read · settings.manage | W10 |
| POST | `/api/ci/legs/:id/exec` | `ci.legs.exec` (command → output; for agents) | runs.debug | W17, W20 |
| POST | `/api/ci/legs/:id/{keep,release,rerun-step}` | `ci.legs.keep`, `.release`, `.rerun_step` | runs.debug | W17 |
| GET | `/api/ci/runs/:id/artifacts/:artifact/download` (built) | `ci.runs.artifact` (as an operation, where to download it: a URL signed for a minute, or kiln's for another provider's run) | artifacts.read | W08, W23 |
| GET/DELETE | `/api/ci/repos/:repo/caches[/:id]` | `ci.caches.list`, `.delete` | runs.read, caches.write | W08 |
| GET/PUT/DELETE | `/api/ci/secrets?scope=` · `/:name` | `ci.secrets.list`, `.set`, `.delete` (values write-only) | secrets.manage | W09 |
| GET/PUT/DELETE | `/api/ci/variables?scope=` · `/:name` | `ci.variables.*` | variables.manage | W09 |
| GET | `/api/ci/integrations/kinds` | `ci.integrations.kinds` (the catalog: fields, how each authenticates, what's built) | runs.read | W11 |
| GET | `/api/ci/integrations` | `ci.integrations.list` (every integration, never its secret, with the kinds) | integrations.manage | W11 |
| POST | `/api/ci/integrations` | `ci.integrations.create` (`{ kind, name, config, secret?, scope? }`; the secret write-only) | integrations.manage | W11 |
| GET/PATCH/DELETE | `/api/ci/integrations/:name` | `ci.integrations.get`, `.update` (`{ config?, secret? }`), `.delete` | integrations.manage | W11 |
| POST | `/api/ci/integrations/:name/verify` | `ci.integrations.verify` (a real exchange: `{ ok, message \| error }`) | integrations.manage | W11 |
| GET | `/api/ci/oidc` · POST `/rotate` · POST `/publish` · GET `/jwks-export` | `ci.oidc.status`, `.rotate`, `.publish`, `.jwks` | settings.manage | W10 |
| GET | `/api/ci/actions?q=` · `/kiln` · POST `/sync` · POST `/api/ci/repos/:repo/actions/try` | `ci.actions.list`, `.kiln`, `.sync`, `.try` | runs.read, settings.manage, runs.trigger | W07, W21 |
| GET · POST | `/api/agents` · POST `/api/agents/:name/probe` (built: runners are kiln's agents) | `ci.runners.list`, `ci.runners.probe` (its facts probed again now) | runners.read, runners.manage | W13 |
| GET/PUT/DELETE | `/api/ci/pools[/:name]` · GET `/:name/history` | `ci.pools.list`, `.get`, `.set`, `.delete`, `.history` | pools.manage | W13, W21 |
| GET/POST | `/api/ci/images` · POST `/validate` · POST `/build` (built) | `ci.images.list`, `.propose`, `.validate`, `.build` | images.build | W13, W21 |
| GET | `/api/ci/registry/mirrors` | `ci.registry.mirrors` (what a machine pulls through kiln with: Docker's registry mirrors, containerd's host files) | runners.read | W12 |
| GET/PUT | `/api/ci/routing/rules?scope=` · POST `/explain` | `ci.routing.rules`, `.set`, `.explain` | routing.manage, runs.read | W05 |
| GET/POST/PATCH/DELETE | `/api/ci/environments[/:name]` · GET `/board` · POST `/starter` | `ci.environments.list`, `.get`, `.save`, `.delete`, `.board`, `.starter` | environments.manage | W15 |
| GET | `/api/ci/environments/:name/deployments` | `ci.environments.deployments` | runs.read | W15 |
| POST | `/api/ci/environments/:name/deploy` | `ci.environments.deploy` (built: the workflow that deploys there, dispatched; its job waits for the environment's protection) | environments.deploy | W15 |
| GET | `/api/ci/repos/:repo/tests/:id/history?branch=&limit=` (built: a test's id is its repository's) | `ci.tests.history` | runs.read | W16 |
| POST | `/api/repos/:repo/submit` (built: a diff of `.kiln/quarantine.yaml`) | `ci.tests.propose_quarantine` (→ diff) | diffs.submit | W16, W20 |
| GET | `/api/ci/repos/:repo/metrics?name=&branch=` | `ci.metrics.repo` | runs.read | W16 |
| GET | `/api/ci/insights/repos[/:repo]` · `/workflows/:repo?workflow=` · `/runners[/:name]` (built; `?days=&grain=`) | `ci.insights.repos`, `.workflows`, `.runners` | runs.read (repos, workflows), runners.read (runners) | W19 |
| GET · PUT | `/api/me/notifications` (built: one's own subscriptions; a repository's rules are its `.kiln/notify.yaml`) | a person's, in Settings | signed in, as oneself | W19 |
| GET · POST | `/api/ci/webhooks/deliveries[/:id]` · `/:id/redeliver` (built: subscriptions are `webhook` integrations with `events`) | `ci.webhooks.deliveries`, `.delivery`, `.redeliver` | integrations.manage | W19 |
| GET | `/api/ci/notify/deliveries?run=` | `ci.notifications.deliveries` | settings.manage | W19 |
| POST | `/api/ci/hooks/:id` | inbound webhook (HMAC) | open, signed | W03 |
| GET · POST · DELETE | `/api/ci/repos/:repo/hooks[/:id]` | `ci.hooks.list`, `.create`, `.delete` (a repository's inbound webhooks) | runs.read · secrets.manage | W03 |
| GET | `/api/audit?principal=&action=&target=&since=` | `audit.list` | audit.read | W19 |
| GET | `/api/audit?principal=&on_behalf_of=&token=&scope=&action=&target=&result=&ip=&q=&since=&until=&before=` (built) | `audit.list` | audit.read | W19 |
| GET/POST · DELETE · POST | `/api/tokens` · `/:id` · `/:id/rotate` | `tokens.list`, `.create`, `.revoke`, `.rotate` (shown once; agent tokens rotate) | your own; anyone's with tokens.manage | W10 |
| GET/POST · DELETE · POST | `/api/users` · `/:id` · `/:id/{link,sign-out}` | `users.list`, `.add`, `.remove`, `.link` (a one-time sign-in link), `.sign_out` | users.manage | W10 |
| GET/POST · DELETE | `/api/grants?user=` · `/:id` | `grants.list`, `.add`, `.remove` (`developer@org:acme`, `!runs.cancel@repo:…`) | users.manage | W10 |
| GET · PUT/DELETE | `/api/roles` · `/:name` | `roles.list`, `.set`, `.remove` (built-in and custom) | signed in · users.manage | W10 |
| GET | `/api/openapi.json` | the route table as OpenAPI 3.1 (`x-kiln-capability`) | signed in | W10 |
| * | `/git/<owner>/<repo>.git/...` | git smart HTTP | diffs.submit (push), diffs.read | W14 |
| GET | `/api/repos/:repo/diffs?state=&author=&reviewer=` | `ci.diffs.list` | diffs.read | W14 |
| GET | `/api/diffs/:id` · `/versions` · `/versions/:v/files` · `/versions/:v/blob?path=&side=` · `/interdiff?from=&to=` | `ci.diffs.get`, `.versions`, `.files`, `.blob`, `.interdiff` | diffs.read | W14 |
| GET | `/api/diffs/:id/dependencies?from=&to=` | `ci.diffs.dependencies` (each lockfile's packages added, removed and changed) | diffs.read | W18 |
| GET/POST | `/api/diffs/:id/comments` · PATCH `/api/comments/:id` | `ci.diffs.threads`, `.comment`, `.reply`, `.resolve` | diffs.review | W14 |
| POST | `/api/diffs/:id/review` | `ci.diffs.review` (`accept\|request_changes\|comment\|resign`) | diffs.review | W14 |
| POST | `/api/diffs/:id/{ready,abandon,reclaim,commandeer}` | `ci.diffs.ready`, `.abandon`, `.reclaim`, `.commandeer` | diffs.submit | W14 |
| GET · POST | `/api/diffs/:id/land` | `ci.diffs.readiness` (what keeps it from landing), `ci.diffs.land` (`{ strategy }`) | diffs.read · diffs.land | W14 |
| GET/POST | `/api/repos/:repo/change-requests` · `/api/change-requests/:id/...` | `ci.change_requests.list`, `.open`, `.get` (the rest as diffs') | diffs.* | W14 |
| GET | `/api/review/inbox` | `ci.review.inbox` (your conversations) | diffs.read | W14 |
| GET · DELETE | `/api/repos/:repo/queue` · `/api/queue/:entry` | `ci.queue.list`, `.remove` | diffs.read, queue.manage | W14 |
| GET | `/api/stacks/:id` | `ci.stacks.get` | diffs.read | W14 |
| POST | `/api/repos/:repo/submit` | `ci.diffs.submit_patch` (no git needed) | diffs.submit | W14 |
| GET/POST · PUT/DELETE · POST | `/api/attachments` · `/:id` · `/pause` (built) | `attachments.list`, `.get`, `.save`, `.remove`, `.pause` | runs.read · agents.attach | W20 |
| GET · POST | `/api/incidents?state=&attachment=` · `/:id` · `/:id/report` (built) | `incidents.list`, `.get`, `.report` | runs.read · its agent, or who may attach there | W20 |
| POST · GET | `/api/ci/runs/:id/bisect` · `/api/ci/runs/:id/bisects[/:bisect]` (built) | `ci.runs.bisect` (`{ job?, good?, bad? }`: the job run on the commits between, halving them), `ci.runs.bisects` | runs.debug (bisecting is diagnosing) · runs.read | W20 |
| POST · GET · POST | `/api/experiments` · `/:id` · `/:id/{decide,cancel}` (built) | `experiments.start`, `.list`, `.get`, `.decide`, `.cancel` | experiments.run · runs.read (reading) | W20 |
| GET · PUT | `/api/ci/repos/:repo/providers` | `ci.providers.get`, `.set` (`{ providers }`: what its CI runs on) | runs.read · settings.manage | W23 |
| POST | `/api/ci/repos/:repo/providers/sync` | `ci.providers.sync` (other providers' runs, looked at now) | runs.read | W23 |
| GET | `/api/ci/repos/:repo/providers/workflows` | `ci.providers.workflows` (each provider's CI files, checked) | runs.read | W23 |
| GET | `/api/ci/repos/:repo/commits/:sha/checks` | `ci.providers.checks` (every provider's runs of a commit) | runs.read | W23 |
| GET | `/api/ci/runs/:id/pending` | `ci.runs.pending` (whichever provider's run) | runs.read | W23 |
| GET · POST | `/api/ci/repos/:repo/pulls[/:n]` · `/:n/{comments,reviews,merge}` | `host.pulls.list`, `.get`, `.comment`, `.review`, `.merge` (GitHub's, GitLab's and Bitbucket's; as the person when they signed in with that host) | diffs.read · diffs.review · diffs.land | W23 |
| POST | `/api/hooks/gitlab` · `/api/hooks/bitbucket` | GitLab's and Bitbucket's webhooks | open, their secret | W23 |
| GET | `/api/auth/sso` · `/api/auth/:provider/{start,callback}` | SSO with GitHub, GitLab and Bitbucket | open | W23 |
| POST | `/api/auth/passkey/options` · `/api/auth/passkey` · `/api/auth/password` (`{ name, password }`) | signing in with a passkey, or a person's name and password | open | W10 |
| GET · POST · DELETE | `/api/me/passkeys` · `/api/me/passkeys/options` · `/api/me/passkeys/:id` | one's own passkeys: listed, made, removed | signed in, as oneself | W10 |
| GET · PUT | `/api/me/password` | a person's own password (`{ password, current }`; empty removes it) | signed in, as oneself | W10 |

GitHub-compatible runtime services (W08, W10), the facade (W14) and the registry (W12) are listed
in §2.6; they are not part of the route table's public API but are authenticated by the same
token model.

### 4.2 Events

`/api/events` (WebSocket) and `/api/events/stream` (SSE) add, each filtered to what the caller may
read and coalesced per object at most every 250 ms: `ci_run`, `ci_leg` (status, wait reason,
progress), `ci_step`, `ci_annotation`, `ci_artifact`, `ci_runner` (facts, health), `ci_pool`,
`ci_routing` (rules changed), `diff`, `diff_version`, `review`, `comment`, `queue`, `deployment`,
`environment`, `incident`, `experiment`, `notification`. Today's `run`, `job`, `agent`, `sample`,
`queue` messages stay.

### 4.3 CLI

```
kiln validate FILE…                     kiln workflow run [WORKFLOW] [--job ID] [--event E] [--executor X] [--server URL]
kiln runs [--repo r] [--follow]         kiln logs <run|leg> [--step n] [--follow]
kiln dispatch <workflow> [--ref r] [--input k=v]   kiln rerun <run> [--failed|--job J] [--debug] [--no-reuse]
kiln cancel <run>                       kiln debug terminal|exec|keep|release|rerun-step <leg> …
kiln secrets set|list|delete|import|rotate-key     kiln vars set|list|delete
kiln integrations list|verify <name>    kiln auth <integration> [--print-env|--exec CMD]   (in jobs)
kiln oidc jwks|publish|rotate|subject   kiln actions list|sync
kiln runners [facts|probe <name>]       kiln runner setup docker|firecracker|shards
kiln pools list|set|scale               kiln images build|list|validate
kiln routing explain <leg|spec>         kiln env create|list|deploy|approve
kiln clone · submit · sync · land [D…] [--strategy S] · diffs · cr open · checkout D123[@v2] · absorb · review D123
kiln metrics criterion|hyperfine|gobench|pytest-benchmark|gbench|k6 PATH
kiln store verify|gc                    kiln ci bench [--scheduler|--expr|--parse|--cache|--logs]
kiln tokens create|list|revoke          kiln login
kiln repo provider <repo> [provider…]
kiln work why <run|leg|step>            kiln work status|warm|verify|gc   kiln rerun <run> --cold
kiln gocacheprog                        kiln rustc   (W24: the runner's cache helpers)
```

### 4.4 MCP names

W24's operations: `ci.work.why`, `ci.work.status`, `ci.work.warm`, `ci.work.verify` and `ci.work.gc` (`kiln.ci.work.*` in code mode).

Code mode: `kiln.ci.<area>.<verb>(input)` (e.g. `kiln.ci.runs.rerun({ run, mode: "failed" })`),
diffs and review among them (`kiln.ci.diffs.*`), the hosts' pull requests (`kiln.host.pulls.*`),
access (`kiln.users.*`, `kiln.grants.*`, `kiln.roles.*`, `kiln.tokens.*`, `kiln.audit.list`),
`kiln.attachments.*`, `kiln.incidents.*`, `kiln.agents.*`, alongside `kiln.cd.*`. One-by-one tools:
the same names with `_` for `.` (`ci_runs_rerun`).
Resources (`resources/templates/list`, read with `resources/read` as the caller): `kiln://runs/<id>/logs`,
`kiln://legs/<id>/logs/<step>`, `kiln://workflows/<owner>/<repo>/<path>`.

---

## 5. Milestones and the order of work

### 5.1 Milestones

| Milestone | Contents | Exit criteria |
| --- | --- | --- |
| **M0 Foundations** | W01; W02 (both dialects, overlays, schemas); W08 store and blob protocol; W10 tokens and the issuer core; W13 facts; W05 core state machine and simulator; W21 code editor component; W22 harnesses | Unit and property suites of each green; `crates/kiln-ci` on `main`; v1 behaviour unchanged (W02-AC04) |
| **M1 GitHub Actions on kiln** | W03 (GHA events from polling and webhooks); W04 (graph, GitHub matrices, outputs, reusable workflows); W05 dispatcher (GitHub concurrency, default retries, wait reasons); W06 on `host` and `docker`; W07 (JavaScript, composite, Docker, mirror and pins); W08 caches (v2, v1) and artifacts; W09 scopes, encryption, least delivery; W10 ID tokens; W14 facade (git, the REST subset for checkout, statuses, artifacts, releases); W21 runs, run graph, leg page, editor with validation | W22-AC01 and W22-AC02 at ≥ 90 % with every miss explained; the e2e suite's GHA part green; kiln's own and hyper-light's v1 pipelines unchanged |
| **M2 Better than GitHub** | W04 kiln matrices and outputs; W05 routing rules, preferences, explain, fairness, quotas, reservations, supersede, retry policies; W08 cache tiers and reuse; W09 pipeline-scoped and action-declared secrets; W16; W17; W19; W21 flow builder, matrix editor, routing settings, insights | Every G-02 to G-04, G-09, G-10, G-16, G-17, G-19 to G-22, G-29 to G-31 acceptance criterion green |
| **M3 Isolation and fleets** | W13 Firecracker, shards, k8s services, pools and autoscaling, assembly, the image builder; W18; W12 | W13-AC*, W18-AC*, W12-AC* green; untrusted runs isolated on the live cluster |
| **M4 Code hosting and review** | W14 native hosting, stacks, change requests, review, strategies, the land queue, the GitHub App; W21 review UI | W14-AC*, W21 review pages green |
| **M5 Environments, integrations, agents** | W15; W11's catalog; W20 operations, skills, attachments, incidents, experiments | W15-AC*, W11-AC01 to AC04, W20-AC* green |
| **M6 Done** | W22 complete, the performance targets, the docs | §0.4 |

### 5.2 Parallel lanes and file ownership

Work proceeds in lanes that rarely touch the same files. Each workstream owns its directories;
shared files are changed only in the ways listed.

| Lane | Workstreams, in order | Owns |
| --- | --- | --- |
| Core | W01 → W02 → W04 → W03 (matcher); W05 core in parallel | `crates/kiln-ci/src/{expr,wf,plan,trigger,sched}` |
| Runner | W06 → W07 → W13 executors | `crates/kiln/src/runner/` |
| Services | W08 → W12; W10 → W11; W09 | `crates/kiln/src/ci/{services,registry,oidc,integrations,secrets}`, `crates/kiln/src/access/` |
| Hosting | W14 | `crates/kiln/src/ci/{facade,review}`, `crates/kiln/src/git/` |
| UI | W21, following each backend | `ui/src/views/{ci,review,runners,environments,settings}`, `ui/src/components/code/` |
| Later | W15, W16, W17, W18, W19, W20 as their dependencies land | `crates/kiln/src/ci/{env,results,debug,supply,insights,notify}`, `crates/kiln/src/ops/`, `crates/kiln/src/mcp/` |

Shared files:

- **Migrations** live one per file in `crates/kiln/src/store/migrations/`, named
  `<yyyymmdd-hhmm>_<ws>_<topic>.sql`. They are applied in name order and recorded by name in the
  `migrations` table, so parallel branches never fight over a number.
- **`proto.rs`** is append-only. When two branches add variants, the second to merge puts its own
  after the first's and fixes the index assertions in the test.
- **The router.** `api.rs` dispatches `/api/ci/*`, `/api/diffs/*` and similar prefixes to each
  workstream's module. Routes are added in their own module's table, not in `api.rs`.
- **Config.** Each workstream adds its own `[ci.<area>]` struct in `crates/kiln/src/config/ci.rs`.
- **UI routes.** Each workstream adds lazy routes to `App.tsx` in one block per area.

### 5.3 What to do first

M0 can start entirely in parallel (W01, W02, W05 core, W08 store, W10, W13 facts, W21 editor,
W22 harness). W02 depends only on W01's public interface (`Expr`, `Template`), so the first commit
of `crates/kiln-ci` fixes that interface (§W01 Design), and W02 builds against it while W01 fills
it in.

---

## Appendix A. GitHub Actions compatibility matrix

Status: **S** supported, GitHub-exact; **S\*** supported with a listed deviation; **K** a kiln
improvement on top. Every row cites the requirement that delivers it.

### A.1 Workflow keys

| Key | Status | Where |
| --- | --- | --- |
| `name`, `run-name` | S | W02-R05, W04-R01 |
| `on` (string, list, map), `on.<event>.types` | S | W03-R03 |
| `on.<push\|pull_request\|pull_request_target>.{branches,branches-ignore,tags,tags-ignore,paths,paths-ignore}` | S\* (all changed files considered) | W03-R04, W03-R05 |
| `on.schedule[].cron` | S\* (no 5-minute minimum, no auto-disable) | W03-R10 |
| `on.workflow_dispatch.inputs` (string, boolean, choice, number, environment) | S | W03-R11 |
| `on.workflow_call.{inputs,outputs,secrets}` | S | W04-R24 |
| `on.workflow_run.{workflows,types,branches,branches-ignore}` | S | W03-R13 |
| `permissions` (workflow, job; every scope, including `models` and `copilot-requests`) | S | W04-R06 |
| `env` (workflow, job, step) | S | W06-R04 |
| `defaults.run.{shell,working-directory}` | S | W04-R28 |
| `concurrency` (string or `{group, cancel-in-progress, queue}`; workflow and job) | S (+K `limit`) | W05-R06, W05-R07 |
| `cache-mode` (workflow, job) | S | W08-R14 |
| `jobs.<id>.name`, `needs`, `if` | S | W04-R02, W04-R03 |
| `jobs.<id>.runs-on` (string, list, `{group, labels}`) | S (+K routing) | W05-R19 |
| `jobs.<id>.environment` (string, `{name, url, deployment}`) | S | W15-R04 |
| `jobs.<id>.outputs` | S (+K typed, legs, secret) | W04-R18 to R22 |
| `jobs.<id>.timeout-minutes`, `continue-on-error` | S | W04-R05, W06-R12 |
| `jobs.<id>.strategy.{matrix,fail-fast,max-parallel}` | S (+K) | W04-R07 to R17 |
| `jobs.<id>.container` (`image`, `credentials`, `env`, `ports`, `volumes`, `options`) | S | W13-R08 |
| `jobs.<id>.services.<id>` | S | W13-R08 |
| `jobs.<id>.uses` (local, `$/` and remote), `with`, `secrets` (`inherit`) | S | W04-R24 |
| `jobs.<id>.snapshot` (custom images) | S\* (read and kept; runner images are built W13's way) | W02-R04, W13 |
| `jobs.<id>.steps[*].{id,if,name,uses,run,shell,with,env,working-directory,continue-on-error,timeout-minutes}` | S | W06-R04 |
| `steps[*].{background,wait,wait-all,cancel,parallel}` | S | W06-R17 |
| `steps[*].uses`: `./path`, `$/path`, `owner/repo[/path]@ref`, `docker://` | S | W07-R01 |
| `steps[*].with.{args,entrypoint}` (Docker actions) | S | W07-R06 |
| YAML anchors and aliases | S | W02-R03 |

### A.2 Contexts

`github` (every documented field, including `event`, `event_path`, `token`, `action*`,
`workflow_ref`, `workflow_sha`, `job`, `run_*`, `ref*`, `head_ref`, `base_ref`, `server_url`,
`api_url`, `graphql_url`, `repository*`, `actor*`, `triggering_actor`, `retention_days`,
`secret_source`, `workspace`, `path`, `env`), `env`, `vars`, `job` (`status`, `container`,
`services`, `check_run_id`), `jobs` (reusable workflow outputs), `steps` (`outputs`, `outcome`,
`conclusion`), `runner` (`name`, `os`, `arch`, `temp`, `tool_cache`, `debug`, `environment`),
`secrets`, `strategy` (`fail-fast`, `job-index`, `job-total`, `max-parallel`), `matrix`, `needs`
(`result`, `outputs`), `inputs`: **S** (W06-R03, W01). kiln adds `git`, `kiln`, `run`, `diff`,
`stack`, `runner.facts`, `needs.<job>.legs`, `needs.<job>.env`: **K** (W01-R14, W04-R19, W04-R21).

### A.3 Functions and operators

All operators and `contains`, `startsWith`, `endsWith`, `format`, `join`, `toJSON`, `fromJSON`,
`hashFiles` (**S\***: sorted order, W01-R09), `success`, `always`, `cancelled`, `failure`: **S**
(W01). kiln adds `changed`, `matches`, `coalesce`, `case`, `split`, `replace`, `lower`, `upper`,
`trim`, `fromYAML`, `toYAML`: **K**.

### A.4 Workflow commands and files

`add-mask`, `group`/`endgroup`, `debug`, `notice`/`warning`/`error` with properties,
`stop-commands`, `echo`, `add-matcher`/`remove-matcher`, deprecated `set-output`/`save-state`
(warned), `set-env`/`add-path` (refused unless allowed); `GITHUB_ENV`, `GITHUB_OUTPUT`,
`GITHUB_PATH`, `GITHUB_STATE`, `GITHUB_STEP_SUMMARY`: **S** (W06-R06, W06-R07). Masking of encoded
forms, `KILN_OUTPUT_JSON`, `KILN_RUN_ENV`, `KILN_METRICS`: **K**.

### A.5 Runtime services

Supported (**S**, from W08, W10 and W14):
- the cache as GitHub Enterprise Server serves it (`_apis/artifactcache`), which the toolkit uses on
  every server but github.com;
- artifacts v4 (Twirp plus Azure-compatible blobs), within and across runs; actions that refuse
  GHES see a `.localhost` server URL (W08-R08);
- `ACTIONS_RUNTIME_TOKEN` scopes;
- OIDC `ACTIONS_ID_TOKEN_REQUEST_*`;
- the tool cache;
- `GITHUB_TOKEN` with `permissions`;
- BuildKit `type=gha`.

The cache service v2 isn't needed: the toolkit uses it only on github.com.

### A.6 Events

| Event | From polling | From webhooks/App | Native (kiln-hosted) |
| --- | --- | --- | --- |
| `push`, `create`, `delete` | yes | yes | yes |
| `pull_request` (`opened`, `synchronize`) | yes | all types | from diffs and change requests |
| `pull_request` (other types), `pull_request_target`, `pull_request_review`, `pull_request_review_comment`, `issue_comment` | no | yes | from reviews and comments |
| `merge_group` | no | yes | from the land queue |
| `workflow_dispatch`, `repository_dispatch`, `schedule`, `workflow_run`, `workflow_call` | kiln itself | kiln itself | kiln itself |
| `release` | no | yes | from kiln releases (facade) |
| `deployment`, `deployment_status` | no | yes | from W15 |
| `registry_package` | no | yes | from W12 |
| `check_run`, `check_suite`, `status`, `label`, `milestone`, `discussion*`, `fork`, `gollum`, `issues`, `page_build`, `public`, `watch`, `branch_protection_rule` and the rest | no | passed through as received | n/a |

### A.7 Limits

| Limit | GitHub | kiln default |
| --- | --- | --- |
| Matrix legs per job | 256 | 256 (GHA dialect), 1,024 (kiln dialect, configurable) |
| Job outputs | 1 MiB per job, 50 MiB per run | same |
| Step summary | 1 MiB per step | same |
| Job execution time | 6 h hosted, 5 days self-hosted | `[ci] max_job_minutes` (default 7 days) |
| Workflow run time | 35 days | 35 days |
| Reusable workflow nesting | GitHub's current limit | the same (W22-AC03 confirms the value): 10 calls deep, and 50 different workflows a run |
| `workflow_run` chain depth | 3 | 3 (kiln dialect up to 10) |
| Cache per repository | 10 GiB | 10 GiB (configurable) |
| `workflow_dispatch` inputs | GitHub's current limit | 100 |
| Path filters | the first 300 changed files | every changed file |
| Background steps running at once | 10 per job | 10 per leg |
| Pending members of a concurrency group | 1 (`queue: single`), 100 (`queue: max`) | the same; `[ci] max_pending` raises `max`'s |
| Cache key length | 512 characters | 512 |
| Unused cache entries kept | 7 days | 7 days (configurable) |

### A.8 Behaviours confirmed by W22-AC03

The last-writer rule for matrix outputs; reusable workflow nesting and count limits; `toJSON`
formatting; number formatting in interpolation; status functions under cancellation; `hashFiles`
input semantics; summary display limits; annotation display limits; `if:` truthiness of mixed
literals.

### A.9 Deviations, collected

`hashFiles` sorted order (W01-R09); all changed files for path filters (W03-R05); no schedule
minimum or auto-disable (W03-R10); the v1 cache protocol kept (W08-R07); 1,000 stored annotations
per leg (W06-R16); kiln dialect's v1 forms and defaults (W02-R06); unmasked short secrets flagged
(W06-R08); a lenient YAML fallback for files GitHub accepts and the strict reader refuses
(`KCI1004`, W02-R03); pull requests land only through the land queue, as on a GitHub branch that
requires a merge queue (a merge of a job's own refused, auto-merge waiting for the queue, a job
landing only what its run's starter may: W14-R11).

## Appendix B. Where each context is available

GitHub-exact, from GitHub's "context availability" table. W02 rejects a context used where it is
not available (`KCI3001`). Special functions: status functions where marked †, `hashFiles` where
marked ‡.

| Key | Contexts |
| --- | --- |
| `run-name` | github, inputs, vars |
| `concurrency` (workflow) | github, inputs, vars |
| `env` (workflow) | github, secrets, inputs, vars |
| `jobs.<id>.concurrency` | github, needs, strategy, matrix, inputs, vars |
| `jobs.<id>.container`, `.container.image` | github, needs, strategy, matrix, vars, inputs |
| `jobs.<id>.container.credentials` | github, needs, strategy, matrix, env, vars, secrets, inputs |
| `jobs.<id>.container.env.<id>` | github, needs, strategy, matrix, job, runner, env, vars, secrets, inputs |
| `jobs.<id>.continue-on-error` | github, needs, strategy, vars, matrix, inputs |
| `jobs.<id>.defaults.run` | github, needs, strategy, matrix, env, vars, inputs |
| `jobs.<id>.env` | github, needs, strategy, matrix, vars, secrets, inputs |
| `jobs.<id>.environment` | github, needs, strategy, matrix, vars, inputs |
| `jobs.<id>.environment.url` | github, needs, strategy, matrix, job, runner, env, vars, steps, inputs |
| `jobs.<id>.if` † | github, needs, vars, inputs |
| `jobs.<id>.name` | github, needs, strategy, matrix, vars, inputs |
| `jobs.<id>.outputs.<id>` | github, needs, strategy, matrix, job, runner, env, vars, secrets, steps, inputs |
| `jobs.<id>.runs-on` | github, needs, strategy, matrix, vars, inputs |
| `jobs.<id>.secrets.<id>` | github, needs, strategy, matrix, secrets, inputs, vars |
| `jobs.<id>.services` (and `.services.<id>.image`, which GitHub's table doesn't list) | github, needs, strategy, matrix, vars, inputs |
| `jobs.<id>.services.<id>.credentials` | github, needs, strategy, matrix, env, vars, secrets, inputs |
| `jobs.<id>.services.<id>.env.<id>` | github, needs, strategy, matrix, job, runner, env, vars, secrets, inputs |
| `jobs.<id>.steps.continue-on-error` ‡ | github, needs, strategy, matrix, job, runner, env, vars, secrets, steps, inputs |
| `jobs.<id>.steps.env` ‡ | github, needs, strategy, matrix, job, runner, env, vars, secrets, steps, inputs |
| `jobs.<id>.steps.if` † ‡ | github, needs, strategy, matrix, job, runner, env, vars, steps, inputs |
| `jobs.<id>.steps.name` ‡ | github, needs, strategy, matrix, job, runner, env, vars, secrets, steps, inputs |
| `jobs.<id>.steps.run` ‡ | github, needs, strategy, matrix, job, runner, env, vars, secrets, steps, inputs |
| `jobs.<id>.steps.timeout-minutes` ‡ | github, needs, strategy, matrix, job, runner, env, vars, secrets, steps, inputs |
| `jobs.<id>.steps.with` ‡ | github, needs, strategy, matrix, job, runner, env, vars, secrets, steps, inputs |
| `jobs.<id>.steps.working-directory` ‡ | github, needs, strategy, matrix, job, runner, env, vars, secrets, steps, inputs |
| `jobs.<id>.strategy` | github, needs, vars, inputs |
| `jobs.<id>.timeout-minutes` | github, needs, strategy, matrix, vars, inputs |
| `jobs.<id>.with.<id>` | github, needs, strategy, matrix, inputs, vars |
| `on.workflow_call.inputs.<id>.default` | github, inputs, vars |
| `on.workflow_call.outputs.<id>.value` | github, jobs, vars, inputs |

Checked on 2026-10-17 against GitHub's table
(<https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#context-availability>,
34 rows; the page carries no revision). It is identical, apart from one row: GitHub has none for
`services.<id>.image`, which kiln reads as `services`.

Keys GitHub's table doesn't list are checked as the closest key that it does:
- `steps[*].background` as `steps.continue-on-error`.
- `environment.deployment` as `jobs.<id>.environment`.
- `wait`, `wait-all` and `cancel` take step ids. `cache-mode` and `concurrency.queue` take
  literals, not expressions.

kiln-dialect keys follow the closest GHA key in the same way. For example, `strategy.where` is
read as `jobs.<id>.strategy` plus `matrix`, and `retry` as `jobs.<id>.continue-on-error`.

## Appendix C. Integration catalog

**Mode** is as in W11-R02. **Basis** is what the entry rests on: `docs` means the provider's own
documentation was checked on 2026-10-17 (sources in Appendix G); `verify` means it must be confirmed
when the provider module is built; `verified <date>` is set by W11-AC03 once a real exchange passes.

| Service | Category | Mode | kiln configures | Basis |
| --- | --- | --- | --- | --- |
| AWS (IAM roles; ECR, S3, EKS, Secrets Manager, CodeArtifact, Bedrock through it) | cloud | oidc | OIDC provider + role trust policy; `AWS_ROLE_ARN` + web-identity token file | docs |
| Google Cloud (Workload Identity Federation; Artifact Registry, GKE, Secret Manager, Vertex AI and Gemini through it) | cloud | oidc | pool and provider; `external_account` credentials file | docs |
| Microsoft Azure / Entra ID (federated credentials; ACR, AKS, Key Vault, Azure OpenAI through it) | cloud | oidc | federated identity credential; Azure SDK variables | docs |
| Alibaba Cloud (RAM OIDC), Oracle Cloud (identity domains token exchange), Tencent Cloud (CAM OIDC), Huawei Cloud (IAM OIDC) | cloud | oidc | provider + role | verify |
| HashiCorp Cloud Platform (service principals' workload identity providers, custom OIDC issuer) | infrastructure | oidc | provider with issuer, audience, conditional access | docs |
| HashiCorp Vault and OpenBao (JWT/OIDC auth method) | secrets | oidc | JWT role bound to claims | verify |
| HCP Terraform / Terraform Cloud API | infrastructure | token (team token); Terraform runs inside kiln use their providers' own federation (AWS, Google, Azure) | token | verify |
| Pulumi Cloud (OIDC issuers) | infrastructure | oidc | issuer registration | verify |
| Infisical, Doppler, Akeyless, CyberArk Conjur | secrets | oidc | machine identity bound to claims | verify |
| 1Password (service accounts), Bitwarden Secrets Manager | secrets | token | service-account token | verify |
| Kubernetes (any cluster: structured authentication / JWT authenticator) | infrastructure | oidc | authenticator config + RBAC binding | verify |
| Snowflake (workload identity federation, OIDC) | data | oidc | user with workload identity | docs |
| Databricks (OAuth token federation policies; public preview) | data | oidc | federation policy (issuer, audiences, subject) | docs |
| MongoDB Atlas, Confluent Cloud (identity pools), Elasticsearch and OpenSearch (JWT realms) | data | oidc | provider config | verify |
| Tailscale (workload identity federation, custom issuer; beta) | network | oidc | trust credential with claim rules → API token, tags | docs |
| JFrog Platform (OIDC integrations, generic provider; Enterprise X and Enterprise+) | registry | oidc | integration + identity mapping | docs |
| Cloudsmith (OIDC providers; Velocity and Ultra plans) | registry | oidc | provider + service account | docs |
| Red Hat Quay (robot account federation) | registry | oidc | robot federation (issuer, subject) | docs |
| Chainguard Registry (assumable identities) | registry | oidc | identity bound to issuer and subject | docs |
| Harbor | registry | token (robot accounts) | robot token | verify |
| Docker Hub | registry | token (organization or personal access tokens; its OIDC accepts GitHub Actions only, from 2026-07-31 per secondary sources) | token | docs (secondary) |
| GitHub (API, GHCR, releases) | forge | app (installation tokens) | GitHub App | docs |
| GitLab (API, registry, packages) | forge | token (project, group or service-account tokens); inbound federation is only a proposal (issue 601894) | token | docs |
| Atlassian: Jira, Confluence | work tracking | app (OAuth 2.0 client credentials for service accounts; 60-minute access tokens), or an API token | client id + secret, or email + API token; the site | docs |
| Linear | work tracking | app (`client_credentials` grant: app-actor token, 30 days), or a personal API key | client id + secret, or an API key | docs |
| Slack | chat | token (bot token) or webhook | bot token / webhook URL | docs (no OIDC for API access) |
| Discord | chat | token (bot) or webhook | bot token / webhook URL | verify |
| Microsoft Teams, Telegram, email (SMTP), PagerDuty, Opsgenie | notifications | token or webhook | as each needs | verify |
| Datadog | observability | oidc-chain through AWS (Datadog's federation accepts AWS identities only), or API and app keys | AWS role mapping, or keys | docs |
| Grafana Cloud | observability | token (access policy tokens) | token | verify |
| OpenTelemetry collectors | observability | none or token | OTLP endpoint | verify |
| Wiz | security | app (service-account client credentials); no OIDC found | client id + secret | verify |
| Snyk | security | token or service account; no OIDC found | token | verify |
| Vercel | hosting | token (Vercel's OIDC is outbound only: it issues tokens for its own deployments) | token | docs |
| Cloudflare (API, Workers, Pages, R2) | hosting | token (no native OIDC; an optional broker Worker in the user's account can mint short-lived scoped tokens) | token | docs |
| Fly.io, Netlify, Render, Railway | hosting | token | token | verify |
| Anthropic (Claude API; any standards-compliant issuer; discovery or inline JWKS) | model provider | oidc | issuer, service account, federation rule | docs |
| OpenAI (workload identity providers: issuer, audience, JWKS source; service-account mapping; tokens ≤ 1 h) | model provider | oidc | provider + mapping | docs |
| Mistral (trusted issuers, service accounts) | model provider | oidc | issuer + credential | docs |
| Google Gemini / Vertex AI | model provider | oidc-chain through Google Cloud | as Google Cloud | docs |
| AWS Bedrock | model provider | oidc-chain through AWS | as AWS | docs |
| Azure OpenAI / AI Foundry | model provider | oidc-chain through Entra | as Azure | verify |
| Cognition Devin (v3 API) | agent platform | token (service-user keys, `cog_…`) | key | docs |
| Sigstore (Fulcio, Rekor) | supply chain | oidc (self-hosted instances trust kiln; the public instance trusts only its allow-listed issuers) | instance URLs | verify |
| npm | package registry | token (trusted publishing accepts GitHub Actions, GitLab CI/CD and CircleCI only) | token | docs |
| PyPI | package registry | token (trusted publishing accepts GitHub, GitLab, Google Cloud, ActiveState) | token | docs |
| crates.io | package registry | token (trusted publishing accepts GitHub Actions and GitLab.com) | token | docs |
| RubyGems, NuGet | package registry | token (trusted publishing for GitHub Actions) | token | docs |
| Maven Central and the other registries of Appendix D | package registry | token | token | verify |

## Appendix D. Registry catalog

### D.1 Package registries (W12-R04)

| Language | Registries | Proxy protocol |
| --- | --- | --- |
| JavaScript, TypeScript | npm, JSR | npm registry API |
| Python | PyPI; conda-forge and Anaconda channels | simple API (PEP 503/691); conda channel |
| Java, Kotlin, Scala, Groovy | Maven Central; Gradle Plugin Portal | Maven layout |
| Clojure | Clojars | Maven layout |
| C#, F#, VB | NuGet | NuGet v3 |
| C, C++ | Conan Center; vcpkg | Conan v2; vcpkg binary cache (HTTP) |
| Go | the Go module proxy | GOPROXY protocol (checksum database passed through) |
| Rust | crates.io | sparse index + downloads |
| PHP | Packagist | Composer v2 metadata |
| Ruby | RubyGems | compact index |
| Swift, Objective-C | Swift Package Registry; CocoaPods | registry API; CDN |
| Dart, Flutter | pub.dev | pub API |
| Elixir, Erlang, Gleam | Hex | Hex API |
| Haskell | Hackage | Hackage index |
| R | CRAN | CRAN layout |
| Perl | CPAN | CPAN mirror layout |
| Lua | LuaRocks | rocks manifest |
| Julia | the General registry | Pkg server protocol |
| OCaml | opam | opam repository |
| Nim | Nimble | package list + generic HTTP |
| Zig | none central (URL-addressed packages) | generic HTTP cache, pinned by hash |
| Multi-language hosts | Artifactory, Nexus, GitHub Packages, GitLab Packages, AWS CodeArtifact, Azure Artifacts, Google Artifact Registry, Cloudsmith | as upstreams |

### D.2 OCI registries (W12-R03)

| Registry | Credentials for jobs |
| --- | --- |
| kiln's own | the job token |
| Docker Hub | token |
| GitHub Container Registry | GitHub App token or PAT |
| Quay | robot federation (OIDC) or robot token |
| Amazon ECR (+ ECR Public) | AWS federation (OIDC) |
| Google Artifact Registry (and `gcr.io`) | Google Cloud federation (OIDC) |
| Azure Container Registry | Entra federation (OIDC) |
| GitLab Container Registry | job or deploy token |
| Harbor | robot token |
| JFrog Artifactory | OIDC integration |
| Sonatype Nexus | token |
| Cloudsmith | OIDC |
| Chainguard | assumable identity (OIDC) |
| DigitalOcean | API token |
| Oracle OCIR | OCI federation or auth token |
| Alibaba ACR | RAM federation (OIDC) |

## Appendix E. kiln dialect reference

Everything here is valid in `.kiln/*.yaml` and, where marked (O), in overlays; none of it is valid
in `.github/workflows`. Each key's semantics are in the requirement cited.

| Level | Key | Ref |
| --- | --- | --- |
| workflow | `secrets: { NAME: { required } }` | W09-R06 |
| workflow | `supersede: { when, action, keep }` (O) | W05-R08 |
| workflow | `routing: { infer: true }` (O) | W05-R19 |
| workflow | `run-env: true` | W04-R21 |
| workflow | `kiln: { honour-skip-ci }` (O) | W03-R06 |
| workflow | `checkout: auto \| false` (default `auto`) | W06-R14 |
| on | `diff`, `land`, `review`, `comment`, `pipeline`, `agent`, `deploy`, `webhook`; v1 `manual`, `tag`, string `schedule` | W03-R09, W02-R06 |
| on | `schedule[].{timezone,branch,inputs,jitter,skip-if-unchanged,catch-up}` (O) | W03-R10 |
| job | `toolchain`, `image`, `exclusive`, `resources: { cpu, memory, disk, gpu }` (O for `resources`) | W02-R06, W05-R03 |
| job | `requires: [constraints]`, `prefer: [preferences]`, `priority` (O) | W05-R22, W05-R23, W05-R09 |
| job, step | `retry: { max, on, backoff, runner }` (O) | W05-R14 |
| workflow, job | `concurrency.limit` (O) | W05-R07 |
| job | `strategy.matrix.{where,platforms,emulate}`, `strategy.{when,shard,chunk}`, `strategy.max-parallel: {total, per}`, `strategy.fail-fast: {scope, cancel}` (O) | W04-R11 to R16 |
| job | `needs: [{ job, matrix: same } \| { job, where }]` (O) | W04-R17 |
| job | `outputs.<name>: { value, secret }`, `exports: { env }`, `env-from` (O) | W04-R20 to R22 |
| job | `reuse: { paths, env, max-age }` (O) | W08-R10 |
| job | `work: { memo, warm, hermetic, verify }` (O); `reuse:` is sugar for `work.memo` | W24 |
| step | `memo: check \| { outputs, inputs, env, secrets: hmac }` (O) | W24 |
| step | `tests: { select: affected \| all, cache }` (O) | W24 |
| job | `cache: [{ key, restore-keys, paths, scope, policy, local }]`; v1 list of directories | W08-R09, W02-R06 |
| job, step | `integrations: [names]`, `registries: [names]` (O) | W11-R03, W12-R03 |
| job | `network: { egress }` (O) | W18-R08 |
| job | `debug: { keep-on-failure }` (O) | W17-R02 |
| job | `attest: { subjects, provenance, sbom }` (O) | W18-R01 |
| job | `quarantine: enforce`, `bench-gate` (O) | W16-R05, W16-R09 |
| job | `deploy: { app, environment }` | W15-R05 |
| job | `agents: { rerun: false, … }` (O) | W20-R09 |
| job | `permissions` extras: `binaries`, `environments`, `registry` (O) | W10-R04 |
| job, step | `uses:` templates (`./templates/...`, `kiln://`), `kiln/*@v1` actions | W04-R25, W07-R10 |
| step | `with-secrets: { NAME: OTHER }` | W09-R07 |
| step | `test-results: { path, format }` (O) | W16-R01 |
| step | `tty: true` (v1) | W06 |
| expressions | the W01-R14 additions | W01-R14 |
| expressions | `affected(ecosystem, …)`: the packages the change affects and everything depending on them | W24 |
| files | `$KILN_OUTPUT_JSON`, `$KILN_RUN_ENV`, `$KILN_METRICS`, `$KILN_SHARD_ITEMS` | W04, W16 |

Policy files read from trunk: `.kiln/review.yaml`, `owners.yaml`, `environments.yaml`,
`agents.yaml`, `quarantine.yaml`, `routing.yaml`, `notify.yaml`, `deploy.yaml` (CD) and
`config.yaml` (repository CI settings: `github_workflows`, `github_api`,
`default_token_permissions`, actions policy, untrusted policy).

A kiln-dialect workflow that uses most of the above:

```yaml
name: ci
on:
  diff: { paths-ignore: ['docs/**'] }
  land: {}
  push: { branches: [main] }
  workflow_dispatch:
    inputs: { profile: { type: choice, options: [dev, release], default: dev } }
secrets: { CRATES_TOKEN: { required: false } }
supersede: { when: [branch, stack], action: cancel, keep: [main] }
concurrency: { group: "ci-${{ github.ref }}", cancel-in-progress: true }

jobs:
  plan:
    runs-on: [linux]
    steps:
      - id: t
        run: echo "tests=$(ci/list-tests.sh --json)" >> "$GITHUB_OUTPUT"
    outputs: { tests: "${{ steps.t.outputs.tests }}" }

  build:
    strategy:
      matrix:
        platforms: [linux/amd64, linux/arm64, darwin/arm64, windows/amd64]
        profile: ["${{ inputs.profile || 'dev' }}"]
    runs-on: native
    reuse: { paths: ['crates/**', 'Cargo.lock'] }
    cache: [{ key: "cargo-${{ matrix.platform }}-${{ hashFiles('Cargo.lock') }}", paths: [target], local: true }]
    retry: { max: 2, on: [runner-lost, infra] }
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo build --profile ${{ matrix.profile }}
      - uses: actions/upload-artifact@v4
        with: { name: "kiln-${{ matrix.platform == 'windows/amd64' && 'win' || matrix.os }}-${{ matrix.arch }}", path: target/${{ matrix.profile }}/kiln* }

  test:
    needs: [plan, { job: build, matrix: same }]
    strategy:
      matrix: { platforms: [linux/amd64, linux/arm64] }
      shard: { items: "${{ fromJSON(needs.plan.outputs.tests) }}", target: 6m, balance: timing }
    runs-on: native
    quarantine: enforce
    steps:
      - uses: actions/checkout@v4
      - run: cargo nextest run $(cat "$KILN_SHARD_ITEMS") --profile ci
        test-results: { path: target/nextest/ci/junit.xml }

  bench:
    needs: [build]
    if: github.event_name == 'push' && changed('crates/**')
    runs-on: [linux]
    requires: ["label bench"]
    exclusive: true
    bench-gate: { metric: "*/p99_ns", max-regression: 5%, action: fail }
    steps:
      - uses: actions/checkout@v4
      - run: cargo bench -- --save-baseline ${{ git.short_sha }} && kiln metrics criterion target/criterion

  deploy-staging:
    needs: [test]
    if: github.ref == 'refs/heads/main'
    environment: { name: staging, url: "https://staging.example.com" }
    runs-on: [linux]
    permissions: { id-token: write, contents: read }
    integrations: [aws-staging]
    steps:
      - uses: kiln/deploy@v1
        with: { app: web, environment: staging, wait: healthy, timeout: 15m }
```

## Appendix F. Test fixtures

Fixtures live under `crates/kiln-ci/tests/fixtures/` (unit and property tests),
`crates/kiln/tests/fixtures/` (integration) and the e2e repository branches (`e2e/*` on kiln-git).

| ID | What | Used by |
| --- | --- | --- |
| F-01 | A file tree for `hashFiles` (nested dirs, exclusions, symlinks in and out of the workspace, empty files) | W01-AC06 |
| F-02 | The v1 corpus: kiln's `.kiln/{ci,bench,e2e}.yaml` and every hyper-light repository's `.kiln/*.yaml` as of 2026-10-17, with the contexts each ran with | W01-AC09, W02-AC04 |
| F-03 | The GHA corpus: ≥ 300 workflow files from GitHub's starter workflows and popular public repositories (licences kept), with service fixtures | W02-AC01, W06-AC07, W17-AC04, W22-AC01 |
| F-04 | The negative corpus: one file per diagnostic code, annotated with the expected code, line and column | W02-AC02, W21-AC02 |
| F-05 | A GHA workflow and its overlay | W02-AC05 |
| F-10 | Cross-arch build: `platforms` legs, `matrix: same` needs, artifact collection | W04-AC07, W04-AC08 |
| F-11 | Timing shards with a recorded history | W04-AC05 |
| F-12 | GitHub's documented matrix examples with their expected legs | W04-AC01 |
| F-13 | A dynamic matrix from a `plan` job | W04-S02 |
| F-14 | A reusable workflow with typed inputs, `secrets: inherit`, outputs, three levels of nesting | W04-AC04 |
| F-15 | `exports` / `env-from`, `run.env`, typed and secret outputs | W04-AC06 |
| F-20 | The toolkit conformance JavaScript action (all `@actions/core` APIs), with pre and post | W06-AC01, W13-AC02 |
| F-21 | Top actions, each in a workflow exercising its main inputs (the list follows) | W07-AC01, W22-AC02 |
| F-22 | GitHub probe workflows and their recorded golden outputs | W22-AC03 |

F-21's list: `actions/checkout`, `actions/setup-node`, `actions/setup-python`, `actions/setup-go`,
`actions/setup-java`, `actions/setup-dotnet`, `actions/cache`, `actions/upload-artifact`,
`actions/download-artifact`, `actions/github-script`, `actions/labeler`,
`actions/attest-build-provenance`, `docker/setup-buildx-action`, `docker/setup-qemu-action`,
`docker/login-action`, `docker/build-push-action`, `docker/metadata-action`,
`dtolnay/rust-toolchain`, `actions-rust-lang/setup-rust-toolchain`, `Swatinem/rust-cache`,
`taiki-e/install-action`, `astral-sh/setup-uv`, `pnpm/action-setup`, `oven-sh/setup-bun`,
`denoland/setup-deno`, `golangci/golangci-lint-action`, `goreleaser/goreleaser-action`,
`erlef/setup-beam`, `ruby/setup-ruby`, `shivammathur/setup-php`, `gradle/actions/setup-gradle`,
`mlugg/setup-zig`, `haskell-actions/setup`, `r-lib/actions/setup-r`, `julia-actions/setup-julia`,
`subosito/flutter-action`, `aws-actions/configure-aws-credentials`,
`aws-actions/amazon-ecr-login`, `google-github-actions/auth`, `google-github-actions/setup-gcloud`,
`azure/login`, `hashicorp/setup-terraform`, `hashicorp/vault-action`,
`softprops/action-gh-release`, `peter-evans/create-pull-request`, `peaceiris/actions-gh-pages`,
`dorny/paths-filter`, `codecov/codecov-action`, `github/codeql-action/upload-sarif`,
`sigstore/cosign-installer`.

## Appendix G. Sources

GitHub Actions behaviour (the reference for every "GitHub-exact"):

- Workflow syntax, contexts, expressions, workflow commands, events, the filter pattern cheat sheet,
  OIDC security hardening, reusable workflows and limits: <https://docs.github.com/en/actions>.
- The expression engine and its test vectors: <https://github.com/actions/languageservices>.
- The toolkit (`@actions/core`, `cache`, `artifact`, `glob`): <https://github.com/actions/toolkit>.
- The runner (workflow commands, file commands, container paths): <https://github.com/actions/runner>.
- Webhook payload schemas: <https://github.com/octokit/webhooks>.
- Independent implementations, useful for the runtime services' wire details: nektos/act
  (<https://github.com/nektos/act>) and Gitea/Forgejo Actions (<https://gitea.com/gitea/act_runner>).
- Pages checked on 2026-10-17 for W02 (none of them carries a revision):
  - context availability:
    <https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#context-availability>;
  - workflow syntax, for background steps, `concurrency.queue`, `cache-mode`,
    `environment.deployment` and `snapshot`:
    <https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax>;
  - dependency caching, for the `cache-mode` defaults, low-trust triggers, `ACTIONS_CACHE_MODE`
    and the limits:
    <https://docs.github.com/en/actions/reference/workflows-and-actions/dependency-caching>;
  - limits: <https://docs.github.com/en/actions/reference/limits>.
- GitHub's changelog (<https://github.blog/changelog/>) announced these 2026 features:
  - concurrency queues (2026-05-07);
  - parallel and background steps (2026-06-25);
  - read-only caches for low-trust triggers (2026-06-26);
  - `$/` self-repository references (2026-07-30);
  - `cache-mode` (2026-09-10).

shards:

- <https://github.com/hyper-light/shards> (README, `docs/design/architecture.md`).
- The `dev` branch: `docs/architecture/AGENTFILE_ARCH.md` and `crates/agentfile`.

Integrations (checked 2026-10-17):

- Anthropic: [Workload Identity Federation](https://platform.claude.com/docs/en/manage-claude/workload-identity-federation)
- OpenAI: [workload identity federation](https://developers.openai.com/api/docs/guides/workload-identity-federation), [token exchange](https://developers.openai.com/api/reference/workload-identity-federation)
- Mistral: [workload identity](https://docs.mistral.ai/admin/identity-access/workload-identity)
- Tailscale: [workload identity federation](https://tailscale.com/kb/1581/workload-identity-federation)
- Datadog: [workload identity federation](https://docs.datadoghq.com/account_management/workload_identity_federation/)
- Databricks: [other providers](https://docs.databricks.com/aws/en/dev-tools/auth/provider-other), [federation policy](https://learn.microsoft.com/azure/databricks/dev-tools/auth/oauth-federation-policy)
- Snowflake: [workload identity federation](https://docs.snowflake.com/en/user-guide/workload-identity-federation)
- HashiCorp Cloud Platform: [workload identity federation](https://developer.hashicorp.com/hcp/docs/hcp/iam/service-principal/workload-identity-federation), [OIDC provider](https://developer.hashicorp.com/hcp/docs/hcp/iam/service-principal/workload-identity-federation/configure-provider/oidc)
- Google Cloud: [workload identity federation with other providers](https://docs.cloud.google.com/iam/docs/workload-identity-federation-with-other-providers)
- Microsoft Entra: [workload identity federation](https://learn.microsoft.com/en-us/entra/workload-id/workload-identity-federation)
- JFrog: [OIDC integration](https://jfrog.com/help/r/jfrog-platform-administration-documentation/configure-an-oidc-integration)
- Cloudsmith: [OpenID Connect](https://docs.cloudsmith.com/authentication/openid-connect)
- Red Hat Quay: [keyless robot accounts](https://docs.redhat.com/en/documentation/red_hat_quay/3/html/manage_red_hat_quay/keyless-authentication-robot-accounts), [push without a password](https://developers.redhat.com/articles/2026/07/22/push-images-to-quay-without-a-password)
- Chainguard: [registry authentication](https://edu.chainguard.dev/chainguard/chainguard-images/chainguard-registry/authenticating/)
- Docker Hub OIDC for GitHub Actions (secondary): [dev.to summary](https://dev.to/leobaniak/docker-hub-gets-oidc-federation-for-github-actions-retiring-the-pat-in-a-secret-pattern-1392)
- Vercel: [OIDC](https://vercel.com/docs/oidc)
- Cloudflare: [GitHub Actions docs](https://developers.cloudflare.com/workers/ci-cd/external-cicd/github-actions/), [OIDC request](https://github.com/cloudflare/workers-sdk/discussions/11434), [community broker](https://github.com/cf-contrib/cf-oidc-auth)
- GitLab: [inbound federation proposal](https://gitlab.com/gitlab-org/gitlab/-/issues/601894)
- Atlassian: [OAuth 2.0 credentials for service accounts](https://support.atlassian.com/user-management/docs/create-oauth-2-0-credential-for-service-accounts/)
- Linear: [OAuth 2.0 authentication](https://linear.app/developers/oauth-2-0-authentication)
- Devin: [API overview](https://docs.devin.ai/api-reference/overview)
- Slack: [Sign in with Slack](https://docs.slack.dev/authentication/sign-in-with-slack/setting-up-sign-in-with-slack-links)
- Grafana: [Google WIF for BigQuery and Cloud Monitoring](https://grafana.com/whats-new/2026-06-24-google-workload-identity-federation-for-bigquery-and-google-cloud-monitoring/)
- npm: [trusted publishers](https://docs.npmjs.com/trusted-publishers), [CircleCI support](https://github.blog/changelog/2026-04-06-npm-trusted-publishing-now-supports-circleci/)
- PyPI: [trusted publishers internals](https://docs.pypi.org/trusted-publishers/internals/), [expanded providers](https://blog.pypi.org/posts/2024-04-17-expanding-trusted-publisher-support/)
- crates.io: [trusted publishing](https://crates.io/docs/trusted-publishing)
- Trusted publishing across RubyGems, NuGet and others: [overview](https://hidekazu-konishi.com/entry/trusted_publishing_beyond_npm_and_pypi.html), [OpenSSF](https://repos.openssf.org/trusted-publishers-for-all-package-repositories.html)

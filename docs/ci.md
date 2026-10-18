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
  `hyper-transport`/`hyper-quic`, TLS on `hyper-tls`. HTTP is `hyper` driven by kiln's own
  `crate::http` client and server. Do not add tokio, axum, reqwest, kube-rs or tokio-rustls. If a
  capability is missing from hyper-rt, say so and ask; do not fall back silently.
  Runtime-agnostic crates (serde, hyper's protocol code, k8s-openapi types, aws-lc-rs, rquickjs)
  are fine.
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
| W03 | Triggers and events | W02 | in progress | matcher, patterns, skip, cron with DST, payloads; polling, webhooks (deduplicated), schedules, dispatches, workflow_run; native events left |
| W04 | Planner, matrices, reusable workflows, outputs | W01, W02 | in progress | the plan engine, AC01–AC06 (unit), reusable workflows local and remote; preview and fixtures left |
| W05 | Scheduler, concurrency, routing, retries | W04 (job model) | in progress | core and dispatcher, AC01–AC06, AC08 (127k assignments/s), AC10 |
| W06 | Step runtime on the runner | W01, W02 | in progress | `run:` and `uses:` steps GitHub-exact on the host and in job containers; background steps, matchers left |
| W07 | Actions | W06 | in progress | pins at run start, mirror, JS (pre/post), composite, local, Docker; `actions/checkout@v4` unmodified; `$/`, policy left |
| W08 | Runtime services: store, caches, artifacts | none (store), W06 (wiring) | in progress | store, blobs, cache (GHES protocol), artifacts v4; actions/cache, upload/download-artifact unmodified; tiers, reuse, BuildKit, speed left |
| W09 | Secrets and variables | W01 | in progress | encrypted store, scopes, least delivery, declarations (KCI4040/4041), variables, API and CLI, rotation; environments, brokered, UI left |
| W10 | Identity, tokens, OIDC issuer | none | in progress | job tokens with permissions (revoked with the attempt), OIDC issuer (RS256, rotation, GitHub claims, getIDToken, workload tokens, publishing); principals and roles left |
| W11 | Integrations and credential broker | W10 | ready (OIDC issuer) | |
| W12 | Registries | W08 (store) | blocked (W08) | |
| W13 | Runners and executors | W06 | in progress | host with job containers, services, Docker actions; the docker executor with kiln-built Ubuntu runner images; probes, pools, microVMs, k8s left |
| W14 | Code hosting, stacks, review, landing | W03 | in progress | the facade: git (smart HTTP at the root), REST at /api/v3 (repository reads, statuses, checks, releases, artifacts, dispatches); hosting, review left |
| W15 | Environments and deployments | W04, W09 | blocked (W04) | |
| W16 | Test results, flaky, benchmarks | W06, W08 | blocked (W06) | |
| W17 | Debugging | W06, W13 | blocked (W06) | |
| W18 | Supply chain, untrusted code | W10, W13 | blocked (W10) | |
| W19 | Insights, audit, notifications | W05 | blocked (W05) | |
| W20 | Agents: MCP, skills, attachments, experiments | W02, W05 | ready (ops registry) | |
| W21 | UI | each backend it shows | in progress | the CI API the pages read (runs, graph, legs, annotations, summaries, artifacts, routing explain); the pages left |
| W22 | Conformance, e2e, performance | grows with the rest | in progress | local e2e harness; AC02 first batch (16 actions: 12 green, 4 hit a DNS outage) |

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
| `/git/<owner>/<repo>.git` | Native git hosting (W14) | PAT, agent token |

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
  policy file), and `repo` is reserved for checks that need the repository (routing, actions).
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
returns a broken file with its diagnostics for the run to show.

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
- *Left to other workstreams.*
  - `KCI6007` needs routing rules and runner facts (W05, W13).
  - `POST /api/ci/repos/:repo/workflows/:path/edit` comes with W14 and W21.
  - Repository-aware checks behind `/api/ci/validate`'s `repo` come with W05 and W07.

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
  `draft`, `user`, `labels`, `head` and `base` with `ref`, `sha`, `repo`), `repository`, `sender`.
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
  - All of it is behind `[ci] enabled`. With it off, a webhook only hints that a poll is due, as
    before.
  - Checked end to end on a local server:
    - push, then matrix legs, needs and outputs;
    - a per-minute schedule firing once a minute;
    - dispatches with typed inputs, and their refusals;
    - `repository_dispatch`;
    - `workflow_run` after each run;
    - an `issue_comment` delivery, and its redelivery doing nothing;
    - a push both delivered and polled, running once.
- *Left.*
  - The App's events (with W14-R10).
  - kiln's native events (with W14, W15 and W20).
  - kiln dialect raising the `workflow_run` depth.
  - AC01's full documentation table, AC04 (the vendored webhook schemas), AC05 as an integration
    test, AC07 to AC09.

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
  ref resolved to when the run started). Inputs are typed (`boolean`, `number`, `string`) with
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
    of contexts, limits with warnings, and provenance (`reads`).
  - `preview` (R29's core).
  - Tests: 42 unit tests (AC03 to AC06, S01 to S05) and the AC02 property test.
  - `ci/runs.rs` persists runs and legs; `ci_plans` and `ci_plan_log` rebuild a plan by replay.
  - Reusable workflows reach the planner (`ci/reusable.rs`): the commit's own files, and remote
    ones pinned through W07's mirror, nested; checked end to end on a local server.
- *Left.*
  - `POST /api/ci/plan-preview`.
  - Fixtures F-10 to F-15, AC07 and AC08 end to end.
  - The live checks of AC04 and AC06.

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
  `max-parallel`, `capability` (eligible runners' builds lack a capability; which). It is stored
  (`jobs.wait_reason`), sent as an event, and shown on the leg.

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
  (today's requeue, kept, and shown as "retried: runner X was lost"); nothing else is retried.
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
  - `crates/kiln/src/ci/dispatch.rs`, the dispatcher, behind `[ci] enabled`:
    - it uses the hub's slot bookkeeping, so v1 and v2 share runners;
    - v1 code reads only v1 rows;
    - legs need `CAP_CI2`.
- *Left.*
  - Pools and scaling (R27, with W13), reservations (R12).
  - `/api/ci/legs/:id/routing`.
  - AC07 (cancel timing on the host executor), AC09 at 10,000 legs, AC11 live.
  - Resource capacity from W13's facts.

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
  `services`, `check_run_id`), `steps`, `runner` (`name`, `os`, `arch`, `temp`, `tool_cache`,
  `debug`, `environment`, and kiln's `facts`), `secrets`, `strategy`, `matrix`, `needs`, `inputs`.
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
  keeps the longest mask's length minus one byte of look-behind). kiln also masks each secret's
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
- *Left.*
  - Background steps (R17). The plan: a background step's process runs in its own task, with
    its own masker and command processor. Its environment, path and outputs are applied at its
    `wait`, in declared order.
  - Problem matchers, `KILN_OUTPUT_JSON` and `exports`.
  - Debug (R13).
  - AC01 (toolkit conformance), AC04 on macOS and Windows, AC05 to AC08.

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
    - R03: a bare mirror per action repository (`<data>/actions/<owner>__<repo>.git`). A ref
      resolves as a tag, then a branch, then a SHA. A known pin still resolves when the source is
      unreachable. Each pinned commit is served as a tarball (`git archive`) at
      `/_kiln/actions/<owner>/<repo>/<sha>.tar.gz`, for a leg's token.
  - `crates/kiln/src/runner/runtime.rs`, on the runner:
    - R01: `./path` and `owner/repo[/path]@ref`.
    - R05:
      - Node: `node24` runs on Node 24; `node20`, `node16` and `node12` run on Node 20. Builds
        come from nodejs.org, are checked against `SHASUMS256.txt`, and are kept in
        `<work>/_externals`.
      - Inputs: `INPUT_*`, with defaults evaluated, and GitHub's "Unexpected input(s)" warning.
      - `GITHUB_ACTION_PATH`, `GITHUB_ACTION_REPOSITORY` and `GITHUB_ACTION_REF`.
      - `pre`, `post`, `pre-if`, `post-if` and `STATE_*` (W06-R11).
    - R07: composite actions, with their own `steps` context, inputs, outputs and nesting.
    - R08: outputs.
    - R06: Docker actions (Dockerfile and `docker://`), on hosts with Docker (W13).
  - `actions/checkout@v4` runs unmodified end to end against the git facade (W14).
- *Left.*
  - R01's `$/path`, `kiln://` and `kiln/*`.
  - The pins on the run page, "rerun with fresh actions", `[[ci.actions.sources]]`, and storing
    tarballs in W08's store (today they are files under `<data>/actions/tar`).
  - R04, R09 beyond checkout (with W14's REST facade), R10, R11, R12.
  - musl Node for Alpine job containers, with W13.
  - AC01 to AC06.

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
  client), with the local directory as a cache.

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
    - Actions whose scripts carry that refusal get `GITHUB_SERVER_URL=http://kiln.localhost`
      from the runner.
    - They call only `ACTIONS_RESULTS_URL` and `GITHUB_API_URL`.
    - The runner puts the real server back in what they print and output, so `artifact-url`
      links to kiln.
  - **Unzipped files.** A file uploaded with `archive: false` downloads raw, under its own name
    (`Content-Disposition`).

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
      48 MiB);
    - `blob.rs`: signed URLs and the Azure Blob subset (R03, AC04);
    - `cache_v1.rs`: the cache protocol, with quotas and idle expiry (R06, R07);
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
- *Left.*
  - R02 (S3), R09 (tiers), R10 (reuse), R11 (BuildKit), R12 and AC08 (speed), R13 (insights).
  - R14's cap for called workflows.
  - v1's `artifacts:` as an artifact.
  - AC01 (the toolkit harness: 1,000 files, 2 GiB), AC03, AC06, AC09.

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
- *Left.*
  - Environment secrets after approval, `SecretsFor` (R04, AC07), with W15.
  - R07 (kiln actions' declared secrets).
  - R08 (brokered, W11).
  - R10 (secret outputs).
  - The UI (W21).
  - AC02 as a route walk, AC06 as an integration test, AC08 (fork pull requests, with W18).

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
  `diffs.review`, `diffs.land`, `queue.manage`, `agents.run`, `agents.attach`, `experiments.run`,
  `settings.manage`, `tokens.manage`, `users.manage`, `audit.read`. Scopes: server, organization,
  repository, workflow, environment, runner, pool. Roles `viewer`, `developer`, `maintainer`,
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
    - `kiln oidc` publishing to a directory, and a rotation's next key published at once.
- *Left.*
  - R01, R02 and R05: principals, capabilities, roles, and the one route table, from access.md.
  - PATs and agent tokens (R03).
  - Kiln's own permission scopes (R04).
  - Per-repository subject customization (R09; server-wide today).
  - R11's service and agent subjects (`kiln:<server-id>:service:<name>`, `kiln:agent:<session>`).
  - R12: `--to s3://… | gs://…` (with W11's cloud modules), the front door serving only the
    issuer paths, and the Settings → OIDC page (W21).
  - AC01, AC02, AC04 (real federation, user-assisted).

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
  (HMAC-signed), PagerDuty and Opsgenie (events APIs).
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

**Progress.** Not started. Ownership, agreed 2026-10-18: the Kiln CD lane builds the registry and
the exchanges (`crates/kiln/src/ci/integrations/`, each kind's `mint()`), on W10's
`workload_claims` and `ci::oidc::mint`; this lane wires them into jobs (`integrations:`, `kiln
auth`, W11-R03, R04).

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

**Progress.** Not started.

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
- **W13-R12 Platforms** (G-18): host runners on Linux (x86_64, aarch64), macOS (arm64, x86_64)
  and Windows (x86_64, arm64); Docker on Linux, macOS (Docker Desktop, Colima, Podman) and Windows
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

- *Done.* `crates/kiln/src/runner/docker.rs`, on the host executor through Docker or Podman
  (R03, R08), as GitHub's self-hosted runner does it:
  - job containers with GitHub's mounts, steps run through `docker exec` with paths
    translated;
  - services on a per-leg network, health checks awaited, ports published, with the
    `job.services` context;
  - teardown;
  - Docker actions (W07-R06);
  - `CAP_DOCKER` announced when a CLI answers.
- *Done.* R04's `docker` executor:
  - a leg runs in a fresh container of its routing rule's image, without the host's Docker
    socket;
  - GitHub's Ubuntu labels prefer it where the runner has Docker, and run on the host otherwise;
  - `kiln/runner-ubuntu-24.04` and `-22.04` are built on the runner the first time a leg needs
    one, from a Dockerfile kiln carries;
  - an `ubuntu-latest` leg ran `setup-python` in it end to end.
- *Left.*
  - R01's probes and R02.
  - The `firecracker`, `shards` and `k8s` executors (R05 to R07).
  - Pools, opt-in Docker access and images built in kiln (R09 to R15).
  - The ACs.

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

- **W14-R10 GitHub App** (optional, `[ci.github] app_id, private_key_file`): installation tokens
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

**Progress.** In progress: the facade's git half, and the start of its REST half.

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
  - the repository's reads from the mirror: repository, commits (with files), branches, refs,
    contents, tarballs and zipballs, compare;
  - commit statuses with the combined status, and check runs;
  - releases, with assets uploaded to `upload_url` (`/api/uploads/…`) and served by
    `browser_download_url`;
  - `/user`, `/rate_limit`;
  - 501 "not implemented by kiln", with a warning on the leg, for everything else;
  - GraphQL answering the same;
  - GitHub-shaped run links redirected to kiln's pages.
  - Checked end to end:
    - `github-script` (statuses, checks, repository, commit, contents);
    - `docker/metadata-action`;
    - `softprops/action-gh-release` with an asset;
    - `actions/checkout` unpacking the tarball where git is missing.
- *Left.* Everything else: the rest of R11's REST subset and GraphQL, hosting (R01, R02), review,
  landing, the App, the schema, and AC01 to AC07.

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

**Progress.** Not started.

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

**Progress.** Not started.

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

**Progress.** Not started.

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
  `actions/attest-build-provenance` works when a Fulcio that trusts kiln's issuer is configured and
  the facade's attestations endpoint is used; otherwise `kiln/attest@v1` is the supported path, and
  the catalog says so.
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

**Progress.** Not started.

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

**Progress.** Not started.

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

- **W20-R08 Attaching.** In the UI ("Attach agent" on a repository, workflow, run, environment or
  runner pool) or in trunk's `.kiln/agents.yaml`, an attachment names:
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

**Progress.** Not started.

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

**Progress.** In progress: the data comes first.

- *Done.* The API the pages read (§4.1), in `crates/kiln/src/ci/runs_api.rs`:
  - runs, with filters and cursor paging;
  - a run with its legs;
  - the graph, with needs edges and the critical path;
  - annotations, summaries and artifacts;
  - cancel, and rerun (mode `all`);
  - a leg with its steps and annotations, its logs, and routing explain;
  - caches.
- Legs of the engine already show on today's run page (steps, logs). Their graph edges come from
  the needed legs' keys.
- *Left.* Every page and component of this workstream: runs, the run graph, the leg page, the
  editor and the rest, with the UI acceptance criteria in Chromium and WebKit.

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

**Progress.** In progress: the local harness only.

- *Done.* A throwaway server with the built-in agent (see the W03 to W14 progress notes) runs
  fixture workflows end to end.
- *AC02, first batch* (16 actions, 2026-10-18):
  - Green: `setup-node`, `setup-go`, `setup-java`, `setup-uv`, `pnpm/action-setup`,
    `setup-bun`, `dtolnay/rust-toolchain`, `setup-terraform`, `docker/metadata-action`,
    `github-script`, `softprops/action-gh-release`, `actions/cache`, `upload-artifact`,
    `download-artifact` and `checkout`.
  - Failed, and why:
    - `setup-buildx`, `cosign-installer`, `setup-deno` and `setup-zig`: the host's DNS failed
      during the run (to rerun).
    - `setup-python`: GitHub's `python-versions` manifest has builds for Ubuntu only, and the
      host is not Ubuntu. *Fixed:* `ubuntu-latest` legs now run in kiln's Ubuntu runner image
      (W13-R04), where it installs CPython 3.12.
- *Left.* Everything else: AC01 and AC03 to AC09, `docs/ci-conformance.md`, and the rest of
  F-21.

---

## 4. API, events, CLI and MCP reference

All routes come from the one route table (W10-R05, W20-R01) and follow access.md's conventions:
cursor pagination (`?cursor=&limit=`), `Idempotency-Key` on POSTs that start things, ETags on reads,
errors as `{ error, code, details? }`. Today's routes (`/api/runs`, `/api/jobs/...`,
`/api/repos/:id/trigger`) keep working; v1 views read the same tables.

### 4.1 Routes

| Method | Path | Op name | Capability | WS |
| --- | --- | --- | --- | --- |
| GET | `/api/ci/repos/:repo/workflows?ref=` | `ci.workflows.list` | runs.read | W02 |
| GET | `/api/ci/repos/:repo/workflows/:path?ref=` | `ci.workflows.get` (source, IR, diagnostics) | runs.read | W02 |
| POST | `/api/ci/validate` | `ci.workflows.validate` | runs.read | W02 |
| POST | `/api/ci/plan-preview` | `ci.workflows.plan_preview` | runs.read | W04 |
| GET | `/api/ci/schema/:kind` | `ci.workflows.schema` | none (public within the API) | W02 |
| POST | `/api/ci/repos/:repo/workflows/:path/edit` | `ci.workflows.edit` (→ a branch, diff or patch) | workflows.edit | W02, W14 |
| GET | `/api/ci/runs?repo=&workflow=&branch=&event=&conclusion=&actor=&runner=` | `ci.runs.list` | runs.read | W04 |
| GET | `/api/ci/runs/:id` | `ci.runs.get` | runs.read | W04 |
| GET | `/api/ci/runs/:id/graph` | `ci.runs.graph` (with critical path) | runs.read | W04, W19 |
| POST | `/api/ci/repos/:repo/dispatch` | `ci.runs.dispatch` (`workflow_dispatch`) | runs.trigger | W03 |
| POST | `/api/ci/repos/:repo/repository-dispatch` | `ci.runs.repository_dispatch` | runs.trigger | W03 |
| POST | `/api/ci/runs/:id/cancel` | `ci.runs.cancel` | runs.cancel | W05 |
| POST | `/api/ci/runs/:id/rerun` | `ci.runs.rerun` (`{ mode: all\|failed\|job, job?, debug?, fresh_actions?, no_reuse? }`) | runs.rerun | W05 |
| POST | `/api/ci/runs/:id/approve` | `ci.runs.approve` (untrusted run or environment) | runs.approve | W15, W18 |
| GET | `/api/ci/runs/:id/{summary,annotations,artifacts,tests,flow,actions}` | `ci.runs.<part>` | runs.read | W06, W08, W16, W04, W07 |
| GET | `/api/ci/runs/:a/compare/:b` | `ci.runs.compare` | runs.read | W19 |
| GET | `/api/ci/legs/:id` | `ci.legs.get` (attempts, steps, outputs, wait reason) | runs.read | W05, W06 |
| GET | `/api/ci/legs/:id/logs?step=&attempt=` · `/logs/ws` | `ci.legs.logs` | logs.read | W06 |
| GET | `/api/ci/legs/:id/logs/search?q=` · `/failure` | `ci.legs.search`, `ci.legs.failure_excerpt` | logs.read | W19, W20 |
| GET | `/api/ci/legs/:id/routing` | `ci.legs.routing` (explain) | runs.read | W05 |
| GET | `/api/ci/legs/:id/terminal` (WebSocket) | `ci.legs.terminal` | runs.debug | W17 |
| POST | `/api/ci/legs/:id/exec` | `ci.legs.terminal_exec` (command → output; for agents) | runs.debug | W17, W20 |
| POST | `/api/ci/legs/:id/{keep,release,rerun-step}` | `ci.legs.keep`, `.release`, `.rerun_step` | runs.debug | W17 |
| GET | `/api/ci/artifacts/:id` | `ci.artifacts.download` | artifacts.read | W08 |
| GET/DELETE | `/api/ci/repos/:repo/caches[/:id]` | `ci.caches.list`, `.delete` | runs.read, caches.write | W08 |
| GET/PUT/DELETE | `/api/ci/secrets?scope=` · `/:name` | `ci.secrets.list`, `.set`, `.delete` (values write-only) | secrets.manage | W09 |
| GET/PUT/DELETE | `/api/ci/variables?scope=` · `/:name` | `ci.variables.*` | variables.manage | W09 |
| GET | `/api/ci/integrations/kinds` | `ci.integrations.kinds` | runs.read | W11 |
| GET/POST/PATCH/DELETE | `/api/ci/integrations[/:id]` | `ci.integrations.*` | integrations.manage | W11 |
| POST | `/api/ci/integrations/:id/verify` | `ci.integrations.verify` | integrations.manage | W11 |
| GET | `/api/ci/oidc` · POST `/rotate` · POST `/publish` · GET `/jwks-export` | `ci.oidc.*` | settings.manage | W10 |
| GET | `/api/ci/actions?q=` · `/kiln` · POST `/sync` | `ci.actions.*` | runs.read, settings.manage | W07 |
| GET | `/api/ci/runners` · `/:name` · POST `/:name/probe` | `ci.runners.*` | runners.read, runners.manage | W13 |
| GET/PUT/DELETE | `/api/ci/pools[/:name]` | `ci.pools.*` | pools.manage | W13 |
| GET/POST | `/api/ci/images` · POST `/validate` · POST `/:id/build` | `ci.images.*` | images.build | W13 |
| GET/PUT | `/api/ci/routing/rules?scope=` · POST `/explain` | `ci.routing.*` | routing.manage, runs.read | W05 |
| GET/POST/PATCH/DELETE | `/api/ci/environments[/:name]` | `ci.environments.*` | environments.manage | W15 |
| GET | `/api/ci/environments/:name/deployments` | `ci.environments.deployments` | runs.read | W15 |
| POST | `/api/ci/environments/:name/deploy` | `ci.environments.deploy` | environments.deploy | W15 |
| GET | `/api/ci/repos/:repo/tests?q=` · `/api/ci/tests/:id/history` | `ci.tests.*` | runs.read | W16 |
| POST | `/api/ci/repos/:repo/quarantine` | `ci.tests.propose_quarantine` (→ diff) | diffs.submit | W16, W20 |
| GET | `/api/ci/repos/:repo/metrics?name=&branch=` | `ci.metrics.query` | runs.read | W16 |
| GET | `/api/ci/insights/{workflows,runners,repos}/:id` | `ci.insights.*` | runs.read | W19 |
| GET/PUT | `/api/ci/notifications/rules?scope=` · GET `/api/inbox` | `ci.notifications.*` | settings.manage, — | W19 |
| GET/POST/DELETE | `/api/webhooks[/:id]` · POST `/:id/redeliver` | `webhooks.*` | settings.manage | W19 |
| POST | `/api/ci/hooks/:id` | inbound webhook (HMAC) | open, signed | W03 |
| GET | `/api/audit?principal=&action=&target=&since=` | `audit.list` | audit.read | W19 |
| GET/POST/PATCH/DELETE | `/api/tokens[/:id]` | `tokens.*` | tokens.manage | W10 |
| * | `/git/<owner>/<repo>.git/...` | git smart HTTP | diffs.submit (push), diffs.read | W14 |
| GET | `/api/repos/:repo/diffs?state=&author=&reviewer=` | `diffs.list` | diffs.read | W14 |
| GET | `/api/diffs/:id` · `/versions/:v/files` · `/interdiff?from=&to=` | `diffs.get`, `.files`, `.interdiff` | diffs.read | W14 |
| GET/POST | `/api/diffs/:id/comments` · PATCH `/api/comments/:id` | `diffs.comments.*` | diffs.review | W14 |
| POST | `/api/diffs/:id/review` | `diffs.review` (`accept\|request_changes\|comment\|resign`) | diffs.review | W14 |
| POST | `/api/diffs/:id/{ready,abandon,reclaim,commandeer}` | `diffs.<action>` | diffs.submit | W14 |
| POST | `/api/diffs/:id/land` | `diffs.land` (`{ strategy }`) | diffs.land | W14 |
| GET/POST | `/api/repos/:repo/change-requests` · `/api/change-requests/:id/...` | `change_requests.*` (as diffs) | diffs.* | W14 |
| GET | `/api/review/inbox` | `review.inbox` (your conversations) | diffs.read | W14 |
| GET · DELETE | `/api/repos/:repo/queue` · `/api/queue/:entry` | `queue.*` | diffs.read, queue.manage | W14 |
| GET | `/api/stacks/:id` | `stacks.get` | diffs.read | W14 |
| POST | `/api/repos/:repo/submit` | `diffs.submit_patch` (no git needed) | diffs.submit | W14 |
| GET/POST · PATCH/DELETE | `/api/repos/:repo/attachments` · `/api/attachments/:id` | `agents.attachments.*` | agents.attach | W20 |
| GET | `/api/incidents?repo=&state=` · `/api/incidents/:id` | `incidents.*` | runs.read | W20 |
| POST | `/api/runs/:id/bisect` | `ci.runs.bisect` | runs.trigger | W20 |
| POST · GET · POST | `/api/experiments` · `/:id` · `/:id/{decide,cancel}` | `experiments.*` | experiments.run | W20 |

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
kiln validate FILE…                     kiln run [WORKFLOW] [--job ID] [--event E] [--executor X]
kiln runs [--repo r] [--follow]         kiln logs <run|leg> [--step n] [--follow]
kiln dispatch <workflow> [--ref r] [--input k=v]   kiln rerun <run> [--failed|--job J] [--debug] [--no-reuse]
kiln cancel <run>                       kiln debug terminal|keep|release|rerun-step <leg> …
kiln secrets set|list|delete|import|rotate-key     kiln vars set|list|delete
kiln integrations list|verify <name>    kiln auth <integration> [--print-env|--exec CMD]   (in jobs)
kiln oidc jwks|publish|rotate           kiln actions list|sync
kiln runners [facts|probe <name>]       kiln runner setup docker|firecracker|shards
kiln pools list|set|scale               kiln images build|list|validate
kiln routing explain <leg|spec>         kiln env create|list|deploy|approve
kiln clone · submit · sync · land [D…] [--strategy S] · diffs · cr open · checkout D123[@v2] · absorb · review D123
kiln metrics criterion|hyperfine|gobench|pytest-benchmark|gbench|k6 PATH
kiln store verify|gc                    kiln ci bench [--scheduler|--expr|--parse|--cache|--logs]
kiln tokens create|list|revoke          kiln login
```

### 4.4 MCP names

Code mode: `kiln.ci.<area>.<verb>(input)` (e.g. `kiln.ci.runs.rerun({ id, mode: "failed" })`),
`kiln.diffs.*`, `kiln.agents.*`, alongside today's `kiln.cd.*`. One-by-one tools: `ci_<area>_<verb>`.
Resources: `kiln://runs/<id>/logs`, `kiln://legs/<id>/logs/<step>`, `kiln://workflows/<repo>/<path>`.

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
| Reusable workflow nesting | GitHub's current limit | the same (W22-AC03 confirms the value) |
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
(`KCI1004`, W02-R03).

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
| Atlassian: Jira, Confluence | work tracking | app (OAuth 2.0 client credentials for service accounts; 60-minute access tokens) | client id + secret | docs |
| Linear | work tracking | app (`client_credentials` grant: app-actor token, 30 days) | client id + secret | docs |
| Slack | chat | token (bot token) or webhook | bot token / webhook URL | docs (no OIDC for API access) |
| Discord | chat | token (bot) or webhook | bot token / webhook URL | verify |
| Microsoft Teams, Telegram, email (SMTP), PagerDuty, Opsgenie | notifications | token or webhook | as each needs | verify |
| Datadog | observability | oidc-chain through AWS (Datadog's federation accepts AWS identities only), or API and app keys | AWS role mapping, or keys | docs |
| Grafana Cloud | observability | token (access policy tokens) | token | verify |
| OpenTelemetry collectors | observability | none or token | OTLP endpoint | — |
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

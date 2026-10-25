# Writing pipelines

> **Superseded for CI by [ci.md](ci.md)** (2026-10-17), which specifies GitHub Actions compatible CI and
> kiln's dialect as its superset; this file still describes v1 `.kiln/*.yaml` files, which keep working.

> **Draft: pipelines v2.** Everything marked ✦ is specified here but not built yet. Every v1
> file stays valid: v2 only adds keys, and v1 conditions keep their meaning.

A repository opts in by adding one or more YAML files under `.kiln/` (`.kiln/ci.yaml`,
`.kiln/bench.yaml`, …). Each file is a pipeline; each pipeline produces one run per trigger.
kiln reads the files **at the commit being built**, so pipeline changes are versioned and reviewed
with the code (GitOps). Check a file locally with `kiln validate .kiln/ci.yaml`.

Policy that must not be changed by the change under test (review rules, owners, environments)
lives in other `.kiln/` files that kiln always reads **from trunk**; see [review.md](review.md).

## No `.kiln/` yet? Auto-detected pipelines

Until a repository adds its own pipeline, kiln reads the files at the root of each commit and
synthesizes a conservative one (`gitops.auto_pipelines = true`, the default):

| Root file | Job (runs in a pod) |
| --- | --- |
| `Cargo.toml` | system deps if missing → `cargo build --workspace --all-targets --locked` → `cargo test --workspace --locked` (honours `rust-toolchain.toml`) |
| `go.mod` / `go.work` | `go build ./...` → `go vet ./...` → `go test ./...` |
| `pyproject.toml` / `setup.py` / `requirements.txt` | `uv sync --all-extras --all-groups` (+ any `requirements*.txt`) → `pytest` if `tests/` exists |
| `package.json` | `npm ci` → `lint` / `typecheck` / `test` / `build` scripts if present |
| `rebar.config` | `rebar3 compile` → `rebar3 eunit` |

The pipeline is named `auto`. It triggers on pushes to `main`/`master`, on diffs ✦ and PRs, and on
manual runs. A repository's own `.kiln/*.yaml` always replaces it.

## A tour

```yaml
name: ci
on:
  diff: {}                                   # ✦ every new version of a diff (stacked review)
  land: {}                                   # ✦ the land queue's candidate: blocks landing
  push: { branches: [main] }
  manual:                                    # ✦ typed inputs for "Run" in the UI, CLI and API
    inputs:
      profile: { type: choice, options: [dev, release], default: dev }

jobs:
  plan:                                      # ✦ a job that decides the shape of the next one
    runs-on: [k8s]
    steps:
      - id: shards
        run: echo "list=$(ci/shards.sh --json)" >> "$KILN_OUTPUT"
    outputs:
      shards: ${{ steps.shards.outputs.list }}

  test:
    needs: [plan]
    runs-on: [linux]
    isolation: microvm                       # ✦ none | container | microvm
    image: ghcr.io/hyper-light/rust:1.90
    matrix:
      shard: ${{ fromJSON(needs.plan.outputs.shards) }}   # ✦ dynamic axis
      profile: [dev, release]
      exclude: [{ profile: release, shard: docs }]        # ✦
      fail-fast: false                                    # ✦
    cache:                                                # ✦ keyed caches
      - key: cargo-${{ hashFiles('Cargo.lock') }}
        restore-keys: [cargo-]
        paths: [~/.cargo/registry, target]
    steps:
      - run: cargo test --profile ${{ matrix.profile }} -p ${{ matrix.shard }}

  bench:
    needs: [test]
    if: event == 'push' && changed('crates/**')           # ✦ changed()
    runs-on: [baremetal, bench]
    exclusive: true
    steps:
      - run: cargo bench -- --save-baseline ${{ git.short_sha }}
    artifacts: ["target/criterion/**"]

  report:
    needs: [test, bench]
    if: always()                                          # ✦ runs whatever happened above
    runs-on: [k8s]
    steps:
      - run: ci/summarize.sh "${{ needs.test.result }}" "${{ needs.bench.result }}"
```

## Triggers (`on`)

| Key | Fires on | Filters |
| --- | --- | --- |
| `push` | a push to a matching branch (trunk pushes come from the land queue in native repos) | `branches`, `paths`, `paths-ignore` ✦ |
| `tag` | a new tag | `branches` (tag-name globs) |
| `diff` ✦ | each new version of a diff targeting a matching trunk ([review.md](review.md)) | `branches` (trunks), `paths`, `paths-ignore`, `drafts: true` |
| `land` ✦ | the land queue's rebased candidate; a failure ejects it from the queue | `branches` (trunks) |
| `pull_request` | a GitHub PR head (repositories that still live upstream on GitHub) | `branches` |
| `schedule` | cron, UTC, on the default branch | a list of crons, or ✦ `[{ cron, branch, inputs }]` |
| `manual` | "Run" in the UI, `kiln run`, `POST …/trigger` | `true` / `false`, or ✦ `{ inputs }` |
| `pipeline` ✦ | another pipeline's run finishing | `{ name, result: [succeeded], branches }` |

Paths compare against the files the trigger changed: for `diff`, the diff's own files (not the
diffs below it in its stack); for `push`, everything since the previous run on that branch.

**Manual inputs** ✦ are typed and validated before a run is created:

```yaml
on:
  manual:
    inputs:
      target:  { type: choice, options: [staging, production], default: staging }
      dry-run: { type: boolean, default: true }
      shards:  { type: number, default: 4 }
      note:    { type: string, required: false, description: "Shown on the run" }
```

They are `${{ inputs.target }}` in expressions. Scheduled runs may pass `inputs` too.

## Jobs

| Key | Meaning |
| --- | --- |
| `name` | display name; expressions allowed (`"test ${{ matrix.profile }}"`) |
| `runs-on` | labels a runner must have, or a runner's name. Default `[k8s]` |
| `requires` | constraints a runner's facts must meet, one or a list (`tools has node`, `disk.free >= 50Gi`, `os == macos`: docs/ci.md W05-R22); see [Runners](#runners) |
| `isolation` ✦ | `none` (on the host), `container`, `microvm`; see [Runners](#runners) |
| `image` | OCI image for `container` / `microvm` / Kubernetes jobs; overrides `toolchain` |
| `toolchain` | `rust` · `go` · `python` · `node` · `bun` · `erlang`: an image plus its caches |
| `needs` | jobs that must finish first; see [Flow](#flow-needs-conditions-outputs) |
| `if` | condition; default `success()` |
| `matrix` | legs to run; see [Matrices](#matrices) |
| `outputs` ✦ | values later jobs read as `needs.<job>.outputs.<name>` |
| `env` | variables for every step |
| `timeout-minutes` | default 60 |
| `retries` | re-run on runner loss (and ✦ `retries: { count, on: [failure, runner-lost] }`) |
| `exclusive` | sole occupant of its runner (benchmarks) |
| `resources` | `cpu`, `memory`, ✦ `disk`, ✦ `gpu`: Kubernetes requests, container and VM limits |
| `network` ✦ | `egress` (default), `none`, or `{ allow: ["crates.io", "*.pkg.dev"] }` |
| `services` ✦ | side containers for the job, e.g. a database |
| `cache` | keyed caches ✦, or (v1) a list of directories kept on the runner |
| `artifacts` | files to keep; ✦ named, and downloadable by later jobs |
| `download` ✦ | artifacts from `needs` jobs to put in the workspace |
| `concurrency` ✦ | job-level limits; see [Concurrency](#concurrency-and-priority) |
| `priority` ✦ | `high` · `normal` · `low` |
| `environment` ✦ | a deploy target from trunk's `.kiln/environments.yaml` (approvals, secrets, limits) |
| `approval` ✦ | turns the job into a gate that waits for people to approve |
| `debug` ✦ | `on-failure`: keep the workspace alive so it can be opened in kiln's terminal |
| `uses` ✦ | build the job from a template; see [Templates](#templates) |
| `permissions` ✦ | what the job's token may do beyond its own run, e.g. `{ binaries: publish }` ([access.md](access.md)) |
| `agents` ✦ | what attached agents may do with this job, e.g. `{ rerun: false }` for deploys ([ci-agents.md](ci-agents.md)) |
| `steps` | what to run, in order |

### Steps

```yaml
steps:
  - name: build
    id: build                          # ✦ lets later steps read steps.build.outputs.*
    run: cargo build --profile ${{ matrix.profile }}
    if: matrix.profile == 'release'    # ✦ step-level condition
    shell: bash                        # default: bash -eo pipefail (sh -e if no bash); Windows: pwsh
    working-directory: crates/core
    env: { CARGO_INCREMENTAL: "0" }
    timeout-minutes: 20
    continue-on-error: false
    tty: false                         # true = run under a PTY (progress bars, colour)
  - uses: ./templates/steps/upload-coverage.yaml   # ✦ a step template
    with: { flags: unit }
```

## Flow: needs, conditions, outputs

`needs` builds the DAG. A job starts when everything it needs has finished, then evaluates `if`.

| `if` | Runs when |
| --- | --- |
| *(absent)* = `success()` | every job it needs succeeded (v1 behaviour: a failure skips dependants) |
| `failure()` ✦ | at least one job it needs failed |
| `always()` ✦ | always, once its needs are done (cleanup, reports) |
| `cancelled()` ✦ | the run was cancelled |
| any expression | `success()` is implied unless the expression calls one of the functions above |

**Outputs** ✦: a step appends `name=value` lines to the file at `$KILN_OUTPUT` (multi-line values
use `name<<EOF` … `EOF`). With an `id`, they are `steps.<id>.outputs.<name>`; the job publishes
the ones it lists under `outputs`. At most 1 MiB per job; outputs are stored with the run.

**Matrix legs and needs** ✦: `needs: [build]` waits for every leg of `build`. To chain legs
one-to-one, pair them on shared axes:

```yaml
test:
  needs: [{ job: build, matrix: same }]   # test (os=linux) waits only for build (os=linux)
```

## Matrices

v1's map of lists still works. v2 adds a few reserved keys, which therefore can't be axis names:

```yaml
matrix:
  os: [linux, macos]
  profile: [dev, release]
  include: [{ os: linux, profile: asan, sanitizer: address }]   # ✦ extra legs (or extra keys on matching legs)
  exclude: [{ os: macos, profile: dev }]                        # ✦
  fail-fast: false        # ✦ default true: the first failed leg cancels the others
  max-parallel: 4         # ✦ legs running at once
```

An axis, or the whole matrix, can come from an expression ✦, usually a `fromJSON` of an earlier
job's output. Such a job is expanded only when its needs have finished. A run is limited to 256 legs.

## Expressions

`${{ … }}` interpolates into any string; `if:` takes a bare expression (braces optional).

**Values:** `'strings'` or `"strings"`, numbers, `true`, `false`, `null`, lists and objects (from
`fromJSON`). Unknown names are empty. Empty, `false`, `0` and `null` are falsy.

**Operators** ✦, highest precedence first: `( )`, `!`, `< <= > >=`, `== !=`, `&&`, `||`.
v1's operators (`==`, `!=`, `&&`, `||`, `!var`) keep their meaning.

| Context | Holds |
| --- | --- |
| `git` | `sha`, `short_sha`, `branch`, `ref`, ✦ `message`, ✦ `author` |
| `branch`, `event`, `repo` | v1 shortcuts; `event` is `push`, `tag`, `diff`, `land`, `pull_request`, `schedule`, `manual` or `pipeline` |
| `matrix` | this leg's values |
| `inputs` ✦ | manual or template inputs |
| `needs` ✦ | `needs.<job>.result` (`success`, `failure`, `cancelled`, `skipped`), `needs.<job>.outputs.<name>` |
| `steps` ✦ | `steps.<id>.result`, `steps.<id>.outputs.<name>` |
| `diff` ✦ | `id` (`D123`), `version`, `title`, `author`, `author_kind` (`human` / `agent`), `base` |
| `stack` ✦ | `position` (1 = closest to trunk), `size`, `bottom`, `top` |
| `env`, `vars` ✦ | environment, and repository variables configured in kiln (not secret) |
| `secrets` ✦ | usable only in `env` and `with`; masked in logs |
| `runner` ✦ | `name`, `os`, `arch`, `isolation` |
| `run` ✦ | `id`, `number`, `attempt` |

**Functions** ✦: `success()`, `failure()`, `always()`, `cancelled()`, `contains(haystack, needle)`
(strings or lists), `startsWith`, `endsWith`, `format('{0}-{1}', a, b)`, `join(list, sep)`,
`toJSON(value)`, `fromJSON(text)`, `hashFiles(glob, …)` (on the runner, after checkout: for cache
keys), `changed(glob, …)` (whether the trigger changed any matching file).

## Runners

| `runs-on` | `isolation` | Executor |
| --- | --- | --- |
| `[k8s]` | (pod) | A fresh pod per job, image from `toolchain` / `image`, shared caches on a PVC. |
| `[baremetal]`, `[bench]`, a name | `none` (default) | A process tree on the runner's host, killed as a group. |
| any runner with `docker` ✦ | `container` | A container per job; steps run in it; caches as volumes; `services` on a private network. Docker or Podman. |
| any runner with `microvm` ✦ | `microvm` | A Firecracker microVM per job, booted from `image` (or a warm snapshot of it). kiln runs as the guest's PID 1 and talks to the host runner over vsock; the network goes through a policy proxy on the host. Shards will be a second backend. |

A job goes to a runner that carries every label of its `runs-on`, in any case: one of the runner's
own, its name, or what its platform is (`linux`, `macos`, `windows`; `x86_64`, `amd64`, `x64`;
`aarch64`, `arm64`; `self-hosted`), so `[linux]` fits every Linux runner, not only those labelled
so; and whose facts, as it last reported them, meet each of its `requires` (one that hasn't
reported any meets none, so the job waits for a runner known to have what it needs). Of those
with a slot free, the one with the most room: its idle cores (by its busy share or its
load average, whichever leaves fewer), then its free memory, the share of its slots in use counting
against it. A runner that's overloaded (under 2 GiB or a twentieth of its memory free, or a load
past twice its cores) takes no job: the job goes to another, or waits for room.

A job that can't run for want of a tool says so on its page: one that failed because a command
wasn't there (its step's log says `command not found`, or a Mac says it has no developer tools), or
one waiting because no runner its labels fit meets its `requires`, where installing tools would
change that (`tools has node`, not `os == macos`). Where an agent can work (a connected runner with
terminals, not a pod, whose image is what lacks the tool), **Install with an agent** starts an agent
on that runner, as whoever asks (who needs `runs.rerun` on the repository and `agents.run`, as for
any agent), to install only what's missing; a step that needs a person there (a password, an
installer's dialog) waits for them in the agent. When it's done, kiln asks the runner what it has
again, then runs the run's failed jobs again, or the waiting job goes there once the runner meets it
([api.md](api.md): `GET /api/jobs/:id/tools`, `POST /api/jobs/:id/tools/fix`). kiln follows the
agent for up to three hours, and only while kiln runs: a restart forgets the fix (the agent goes on,
and Retry runs the job again).

If `isolation` is absent, a job runs as it does in v1: in a pod on `[k8s]`, and on the host
everywhere else (where `image` is ignored, as before). Containers and microVMs are always asked
for explicitly. Runners advertise `docker` / `microvm` themselves when they can run them.

**Services** ✦ run next to the job (containers and microVMs), reachable by name:

```yaml
services:
  db: { image: postgres:17, env: { POSTGRES_PASSWORD: test }, ports: [5432], health: "pg_isready" }
```

## Caches and artifacts ✦

```yaml
cache:
  - key: cargo-${{ runner.os }}-${{ hashFiles('**/Cargo.lock') }}
    restore-keys: [cargo-${{ runner.os }}-]   # prefix fallbacks, newest first
    paths: [~/.cargo/registry, ~/.cargo/git, target]
    scope: branch     # branch (default): read own branch, then trunk; write own branch · repo · global
```

A cache is saved after a successful job if its key wasn't an exact hit. kiln stores caches and
artifacts in its own content-addressed store (deduplicated, evicted least-recently-used under a
configured size), served to every runner type. A list of plain strings (v1) still means
"directories kept on that runner between jobs".

```yaml
build:
  artifacts:
    - { name: binaries, paths: [target/release/kiln], retention-days: 14 }
deploy:
  needs: [build]
  download: [binaries]    # into $KILN_ARTIFACTS/binaries
```

## Concurrency and priority

Run-level (v1): `concurrency: { group, cancel-in-progress }`: one active run per group.

Job-level ✦:

```yaml
deploy:
  concurrency:
    group: deploy-${{ inputs.target }}
    limit: 1            # at most N jobs of the group at once
    queue: latest       # fifo (default) · latest (skip superseded waiting jobs) · cancel-in-progress
  priority: high
```

Default priority by event: `manual` > `land` > `diff` > `push` > `schedule`. Capacity itself
(runner pools, per-repository quotas and fair shares, reserved hardware) is server configuration,
not pipeline YAML, because many repositories share it. When a job waits, the run shows why: no
runner with these labels, pool full, quota used, a concurrency group, an exclusive job.

## Environments and approvals ✦

```yaml
deploy-production:
  needs: [build]
  environment: production     # rules come from trunk's .kiln/environments.yaml
  steps: [{ run: ci/deploy.sh production }]

ship-it:
  needs: [build]
  approval: { reviewers: [owners], message: "Ship ${{ git.short_sha }}?" }   # no steps: waits for approval
```

An environment can require approvals, restrict branches, hold secrets only its jobs see, and limit
concurrent deployments. Every deployment is recorded per environment.

## Templates ✦

```yaml
jobs:
  check:
    uses: ./templates/rust-check.yaml        # relative to .kiln/
    with: { features: "--all-features" }
  lint:
    uses: kiln://hyper-light/ci-templates/.kiln/templates/clippy.yaml@3f2a9c1   # pinned to a commit
```

A template declares `inputs` and a job, or several jobs (a flow template). Step templates
(`uses:` on a step) declare `inputs` and `steps`. Templates may use templates, four deep.

## What every step sees

`CI=true`, `KILN_REPO`, `KILN_SHA`, `KILN_BRANCH`, `KILN_EVENT`, `KILN_RUN_ID`, `KILN_RUN_NUMBER`,
`KILN_JOB`, `KILN_ATTEMPT`, `KILN_WORKSPACE`, `KILN_AGENT`; ✦ `KILN_OUTPUT`, `KILN_ARTIFACTS`,
`KILN_ISOLATION`, and for diffs `KILN_DIFF`, `KILN_DIFF_VERSION`, `KILN_STACK_POSITION`; plus the
server's configured secrets that the job may see (masked in logs).

## Starters for the hyper-light stack

### Rust (hyper-raft, slates, focal, mantle, shards)

```yaml
name: ci
on: { diff: {}, land: {}, push: { branches: [main] } }
concurrency: { group: "ci-${{ branch }}", cancel-in-progress: true }
jobs:
  check:
    runs-on: [k8s]
    toolchain: rust
    resources: { cpu: "8", memory: 16Gi }
    steps:
      - run: rustup show && cargo fmt --all --check
      - run: cargo clippy --workspace --all-targets --locked -- -D warnings
      - run: cargo test --workspace --locked
  bench:
    needs: [check]
    if: event == 'push' && branch == 'main'
    runs-on: [baremetal, bench]
    exclusive: true
    timeout-minutes: 120
    steps:
      - run: cargo bench --workspace -- --save-baseline ${{ git.short_sha }}
    artifacts: ["target/criterion/**"]
```

### Python (hyperscale, mkfst-py, cocoa)

```yaml
name: ci
on: { diff: {}, push: { branches: [main] } }
jobs:
  test:
    runs-on: [k8s]
    toolchain: python
    matrix: { python: ["3.12", "3.13", "3.14"] }
    steps:
      - run: uv python install ${{ matrix.python }}
      - run: uv sync --python ${{ matrix.python }} --all-extras
      - run: uv run --python ${{ matrix.python }} pytest -q
```

### Go (sylk)

```yaml
name: ci
on: { diff: {}, push: { branches: [main] } }
jobs:
  test:
    runs-on: [k8s]
    toolchain: go
    steps:
      - run: go vet ./...
      - run: go test -race ./...
```

### TypeScript (hyperlight-site)

```yaml
name: ci
on: { diff: {}, push: { branches: [main] } }
jobs:
  site:
    runs-on: [k8s]
    toolchain: node
    steps:
      - run: npm ci
      - run: npm run lint && npm run typecheck && npm test
      - run: npm run build
```

### Erlang

```yaml
jobs:
  test:
    runs-on: [k8s]
    toolchain: erlang
    steps:
      - run: rebar3 do compile, eunit, ct
```

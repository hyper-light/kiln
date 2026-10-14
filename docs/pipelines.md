# Writing pipelines

A repository opts in by adding one or more YAML files under `.kiln/` (`.kiln/ci.yaml`,
`.kiln/bench.yaml`, …). Each file is a pipeline; each pipeline produces one run per trigger.
kiln reads the files **at the commit being built**, so pipeline changes are versioned and reviewed
with the code (GitOps). Check a file locally with `kiln validate .kiln/ci.yaml`.

## Reference

```yaml
name: ci                                  # shown in the UI; defaults to the file name
on:
  push: { branches: [main, "release/*"], paths: ["crates/**"] }   # globs; empty = everything
  pull_request: {}                        # builds refs/pull/N/head
  tag: { branches: ["v*"] }               # tag name filter
  schedule: ["0 3 * * *"]                 # cron, UTC, default branch
  manual: true                            # allow "Run" from the UI / API (default true)
env: { RUST_BACKTRACE: "1" }              # every job
concurrency:                              # one active run per group
  group: "ci-${{ branch }}"
  cancel-in-progress: true
jobs:
  <id>:
    name: "test ${{ matrix.profile }}"
    runs-on: [k8s]                        # labels an agent must have; or an agent's name
    needs: [other-job]                    # DAG; a failed dependency skips dependants
    toolchain: rust                       # rust | go | python | node | bun | erlang → image + caches
    image: ghcr.io/org/custom:tag         # k8s only; overrides the toolchain image
    matrix: { profile: [dev, release] }   # cartesian product, ≤ 256 legs
    if: "branch == 'main' && event != 'pull_request'"
    timeout-minutes: 60
    retries: 1                            # re-run on agent loss
    exclusive: true                       # sole occupant of the agent (benchmarks)
    resources: { cpu: "4", memory: 8Gi }  # k8s requests (memory is also the limit)
    env: { PROFILE: "${{ matrix.profile }}" }
    artifacts: ["target/criterion/**"]    # uploaded after the steps, downloadable in the UI
    steps:
      - name: build
        run: cargo build --profile ${{ matrix.profile }}
        shell: bash                       # default: bash -eo pipefail (sh -e if no bash)
        working-directory: crates/core
        env: { CARGO_INCREMENTAL: "0" }
        timeout-minutes: 20
        continue-on-error: false
        tty: false                        # true = run under a PTY (progress bars, colour)
```

Expressions: `${{ git.sha }}`, `${{ git.short_sha }}`, `${{ git.branch }}`, `${{ branch }}`,
`${{ event }}`, `${{ repo }}`, `${{ matrix.<key> }}`. Conditions support `==`, `!=`, `&&`,
`||`, `!var` and quoted literals.

Every step sees `CI=true`, `KILN_REPO`, `KILN_SHA`, `KILN_BRANCH`, `KILN_EVENT`, `KILN_RUN_ID`,
`KILN_RUN_NUMBER`, `KILN_JOB`, `KILN_ATTEMPT`, `KILN_WORKSPACE`, `KILN_AGENT`, plus the server's
configured `[secrets]` (masked in logs).

## Where jobs run

| `runs-on` | Executor |
| --- | --- |
| `[k8s]` | A fresh pod per job in the `kiln-jobs` namespace, image from `toolchain`/`image`, shared build caches on a PVC. Scales with the cluster. |
| `[baremetal]` | Any bare-metal agent (the server's built-in local agent included). |
| `[baremetal, bench]` | Agents you labelled `bench` — typically dedicated hardware. Combine with `exclusive: true` so nothing else runs during a measurement. |
| `[my-box]` | Exactly the agent named `my-box`. |

## Starters for the hyper-light stack

### Rust (hyper-raft, slates, focal, mantle, shards)

```yaml
name: ci
on: { push: { branches: [main] }, pull_request: {} }
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
    if: "branch == 'main'"
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
on: { push: { branches: [main] }, pull_request: {} }
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
on: { push: { branches: [main] }, pull_request: {} }
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
on: { push: { branches: [main] }, pull_request: {} }
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

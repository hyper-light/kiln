/**
 * The files behind the mock's Pipelines rows (docs/ci.md W29; Ada's approved stills of 2026-10-31):
 * each workflow's and action's text at its repository's head, by `repo:path` (a draft's by its id),
 * what its runs were, which tags its repository has, and who used which of its versions.
 */

const lines = (...l: string[]) => `${l.join("\n")}\n`;

/** kiln's own CI (pl-2): its jobs at the lines the stills show them. */
const KILN_CI = lines(
  "name: ci",
  "# kiln's own CI: lint and test what changed, on bare metal unless a job asks otherwise.",
  "on:",
  "  push:",
  "    branches: [main]",
  "  pull_request:",
  "    branches: [main]",
  "    drafts: true",
  "  manual: {}",
  "",
  "concurrency: ci-${{ github.ref }}",
  "env:",
  "  CARGO_TERM_COLOR: always",
  "jobs:",
  "  rust:",
  "    name: rust lint + test",
  "    runs-on: [baremetal, linux]",
  "    resources: { cpu: 2, memory: 16Gi }",
  "    timeout-minutes: 45",
  "    steps:",
  "      - uses: kiln/checkout@v1",
  "      - uses: hyper-light/ci-kit/.github/actions/setup-rust@v2.3.0",
  "        with: { components: clippy }",
  "      - run: cargo clippy --locked --all-targets -- -D warnings",
  "      - run: cargo test --locked --no-fail-fast",
  "",
  "  ui:",
  "    name: ui typecheck + lint + build",
  "    runs-on: [baremetal]",
  "    steps:",
  "      - uses: kiln/checkout@v1",
  "      - uses: actions/cache@v4",
  "        with:",
  "          path: ui/node_modules",
  "          key: ui-${{ hashFiles('ui/package-lock.json') }}",
  "      - run: cd ui && npm ci && npm run typecheck && npm run lint && npm run build",
  "",
  "  vendor:",
  "    runs-on: [baremetal]",
  "    steps:",
  "      - uses: kiln/checkout@v1",
  "      - run: python3 scripts/vendor.py check",
  "",
  "  ui-checks:",
  "    needs: [rust, ui]",
  "    runs-on: [baremetal, docker]",
  "    if: github.ref == 'refs/heads/main'",
  "    steps:",
  "      - uses: kiln/checkout@v1",
  "      - run: ci/ui-checks.sh",
  "        env:",
  "          KILN_BENCH_HOST: ${{ vars.KILN_BENCH_HOST }}",
  "",
  "  release-binary:",
  "    needs: [rust, ui]",
  "    runs-on: [baremetal]",
  "    strategy:",
  "      matrix:",
  "        platform: [linux/amd64, darwin/arm64, windows/amd64]",
  "    steps:",
  "      - uses: kiln/checkout@v1",
  "      - run: cargo build --release --locked",
  "        env:",
  "          CARGO_REGISTRY_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN }}",
);

/** smoke (pl-8, pl-9): a trigger of most kinds, a schedule in local time. */
const KILN_SMOKE = lines(
  "name: smoke",
  "on:",
  "  deploy: { app: kiln-production, states: [synced] }",
  "  schedule:",
  '    - cron: "0 3 * * *"',
  "      timezone: America/Los_Angeles",
  "      skip-if-unchanged: true",
  "      inputs: { target: production }",
  "  pipeline: { name: ci, conclusion: [success], branches: [main] }",
  "  manual:",
  "    inputs:",
  "      target: { type: choice, options: [staging, production], default: staging }",
  "  webhook: { hook: status-page, filter: \"event.status != 'ok'\" }",
  '  comment: { commands: ["/smoke"] }',
  "jobs:",
  "  smoke:",
  "    runs-on: [baremetal, linux]",
  "    steps:",
  "      - uses: kiln/checkout@v1",
  "      - run: ci/smoke.sh --target ${{ inputs.target }}",
  "  report:",
  "    needs: [smoke]",
  "    runs-on: [baremetal]",
  "    steps:",
  "      - uses: kiln/notify@v1",
  '        with: { to: "#ci" }',
);

const KILN_E2E = lines(
  "name: e2e",
  "on:",
  "  push:",
  "    branches: [main]",
  "jobs:",
  "  e2e:",
  "    runs-on: [baremetal, linux]",
  "    strategy:",
  "      matrix:",
  "        spec: [ship-deploys, ship-lanes, ship-sweep, ship-rows, ship-files]",
  "    steps:",
  "      - uses: kiln/checkout@v1",
  "      - run: cd ui && npm ci && npm run build && npx playwright test ${{ matrix.spec }}",
);

const KILN_STRESS = lines(
  "name: stress",
  "# agent:01a19350 for ada: soak the scheduler at 114 jobs",
  "on:",
  "  manual:",
  "    inputs:",
  "      jobs: { type: number, default: 114 }",
  "jobs:",
  "  soak:",
  "    runs-on: [baremetal, linux, bench]",
  "    steps:",
  "      - uses: kiln/checkout@v1",
  "      - run: cargo run -p kiln-sim -- stress --jobs ${{ inputs.jobs }}",
);

const KILN_RELEASE = lines(
  "name: release",
  "on:",
  "  push:",
  "    tags: [v*]",
  "jobs:",
  "  build:",
  "    runs-on: [baremetal]",
  "    strategy:",
  "      matrix:",
  "        os: [linux, windows, macos]",
  "    steps:",
  "      - uses: kiln/checkout@v1",
  "      - run: cargo build --release --locked",
  "  publish:",
  "    needs: [build]",
  "    runs-on: [baremetal, linux]",
  "    steps:",
  "      - run: ci/publish.sh ${{ github.ref_name }}",
  "        env:",
  "          GITHUB_TOKEN: ${{ secrets.RELEASE_TOKEN }}",
);

const KILN_NIGHTLY = lines(
  'import { workflow, job, on, matrix } from "@kiln/sdk";',
  "",
  "// Every platform kiln ships, soaked each night; Windows only when run by hand.",
  'const platforms = ["linux/amd64", "linux/arm64", "darwin/arm64", "windows/amd64"];',
  "",
  'export default workflow("nightly", {',
  '  on: [on.schedule("0 3 * * *"), on.manual({ depth: { type: "number", default: 3 } })],',
  "  jobs: {",
  "    soak: job({",
  '      runsOn: "baremetal",',
  "      matrix: matrix({ platform: platforms })",
  '        .where((m, ev) => m.platform !== "windows/amd64" || ev.name === "manual"),',
  "      steps: (s) => [",
  '        s.uses("kiln/checkout@v1"),',
  '        s.uses("hyper-light/ci-kit/.github/actions/setup-rust@v2.4.0"),',
  "        s.run(`cargo run -p kiln-sim -- soak --depth ${s.inputs.depth}`),",
  "      ],",
  "    }),",
  '    report: job({ needs: ["soak"], steps: (s) => [s.uses("kiln/notify@v1", { to: "#ci" })] }),',
  "  },",
  "});",
);

const KILN_GHA = lines(
  "name: GitHub CI",
  "on: [push, pull_request]",
  "jobs:",
  "  test:",
  "    runs-on: ubuntu-latest",
  "    steps:",
  "      - uses: actions/checkout@v4",
  "      - run: cargo test --locked",
);

const SETUP_RUST = lines(
  "name: setup-rust",
  "description: Install a Rust toolchain, its components and targets, with the registry cached",
  "inputs:",
  "  toolchain:",
  "    description: the toolchain; rust-toolchain.toml's when not given",
  "    required: false",
  "  components:",
  "    description: the components to add",
  "    default: clippy, rustfmt",
  "  targets:",
  "    description: targets to add, for cross builds",
  "    required: false",
  "runs:",
  "  using: composite",
  "  steps:",
  "    - uses: kiln/cache@v1",
  '      with: { path: "~/.cargo, target" }',
  "    - name: rustup toolchain install",
  "      run: rustup toolchain install ${{ inputs.toolchain }} --component ${{ inputs.components }}",
  "      shell: bash",
  "    - name: rustc --version",
  "      run: rustc --version",
  "      shell: bash",
);

const CACHE_CARGO = lines(
  "name: cache-cargo",
  "description: Cache cargo's registry and target by Cargo.lock",
  "inputs:",
  "  key: { description: a key of your own, required: false }",
  "runs:",
  "  using: composite",
  "  steps:",
  "    - uses: kiln/cache@v1",
  '      with: { path: "~/.cargo/registry, target", key: "cargo-${{ hashFiles(\'Cargo.lock\') }}-${{ inputs.key }}" }',
);

const NOTIFY = lines(
  "name: notify",
  "description: Say how a run went, in a channel",
  "inputs:",
  "  to: { description: the channel, required: true }",
  "runs:",
  "  using: node24",
  "  main: dist/index.js",
);

const LINT_DOCKER = lines("name: lint-docker", "description: hadolint over the repository's Dockerfiles", "runs:", "  using: docker", "  image: Dockerfile", "  args: [--format, tty]");

const SANDBOX = lines(
  "name: sandbox",
  "description: Run a command in a Shards microVM of its own",
  "inputs:",
  "  run: { description: the command, required: true }",
  "runs:",
  "  using: shards",
  "  image: Dockerfile",
  "  resources: { cpus: 2, memory: 4Gi }",
  "  egress: []",
);

const FUZZ_TARGET = lines(
  "name: fuzz-target",
  "description: Fuzz one cargo-fuzz target in a microVM of its own",
  "inputs:",
  "  target: { description: the cargo-fuzz target, required: true }",
  '  minutes: { description: how long it runs, default: "10" }',
  "outputs:",
  "  crashes: { description: the crashes it found }",
  "runs:",
  "  using: firecracker        # kiln's: one microVM per call",
  "  image: Dockerfile         # built once per commit, booted as the guest",
  "  resources: { cpus: 4, memory: 8Gi }",
  "  egress: [static.crates.io]   # nothing else leaves the microVM",
  '  args: [--target, "${{ inputs.target }}", --minutes, "${{ inputs.minutes }}"]',
);

const RUST_CHECK = lines(
  "name: rust-check",
  "on:",
  "  workflow_call:",
  "    inputs:",
  "      toolchain: { type: string, default: stable }",
  "      features: { type: string, default: default }",
  "jobs:",
  "  check:",
  "    runs-on: [baremetal, linux]",
  "    steps:",
  "      - uses: kiln/checkout@v1",
  "      - uses: hyper-light/ci-kit/.github/actions/setup-rust@v2.4.0",
  "      - run: cargo clippy --features ${{ inputs.features }} -- -D warnings",
  "  test:",
  "    runs-on: [baremetal, linux]",
  "    steps:",
  "      - uses: kiln/checkout@v1",
  "      - run: cargo test --features ${{ inputs.features }}",
);

const quiverCi = (version: string) =>
  lines(
    "name: ci",
    "on:",
    "  push:",
    "    branches: [main]",
    "  pull_request:",
    "    branches: [main]",
    "permissions:",
    "  contents: read",
    "env:",
    "  RUST_BACKTRACE: 1",
    "",
    "jobs:",
    "  rust:",
    `    uses: hyper-light/ci-kit/.github/workflows/rust-check.yml@${version}`,
    "    with: { toolchain: stable, features: all }",
    "  bench:",
    "    runs-on: [baremetal, bench]",
    "    steps:",
    "      - uses: kiln/checkout@v1",
    "      - run: cargo bench --no-run",
  );

const QUIVER_BENCH = lines(
  "name: bench",
  "on:",
  "  schedule:",
  '    - cron: "0 4 * * *"',
  "jobs:",
  "  bench:",
  "    runs-on: [baremetal, bench]",
  "    steps:",
  "      - uses: actions/checkout@v4",
  "      - run: cargo bench",
);

/** Each file's text, by `repo:path`, or a draft's by `repo:path:draft:id`. */
export const FILES: Record<string, string> = {
  "hyper-light/kiln:.kiln/ci.yaml": KILN_CI,
  "hyper-light/kiln:.kiln/smoke.yaml": KILN_SMOKE,
  "hyper-light/kiln:.kiln/e2e.yaml": KILN_E2E,
  "hyper-light/kiln:.kiln/stress.yaml:draft:d-stress": KILN_STRESS,
  "hyper-light/kiln:.kiln/release.yaml": KILN_RELEASE,
  "hyper-light/kiln:.kiln/nightly.ts": KILN_NIGHTLY,
  "hyper-light/kiln:.github/workflows/ci.yml": KILN_GHA,
  "hyper-light/ci-kit:.github/actions/setup-rust": SETUP_RUST,
  "hyper-light/ci-kit:.github/actions/cache-cargo": CACHE_CARGO,
  "hyper-light/ci-kit:.github/actions/notify": NOTIFY,
  "hyper-light/ci-kit:.github/actions/lint-docker": LINT_DOCKER,
  "hyper-light/ci-kit:.github/actions/sandbox": SANDBOX,
  "hyper-light/ci-kit:.github/actions/fuzz-target:draft:d-fuzz": FUZZ_TARGET,
  "hyper-light/ci-kit:.github/workflows/rust-check.yml": RUST_CHECK,
  "hyper-light/quiver:.github/workflows/ci.yml": quiverCi("v2.3.0"),
  "hyper-light/quiver:.github/workflows/ci.yml:draft:d-update-88": quiverCi("v2.4.0"),
  "hyper-light/quiver:.github/workflows/bench.yml": QUIVER_BENCH,
};

/** A workflow's newest runs on its default branch, newest first: its number, what started it, how it went. */
/** Each workflow's runs, newest first: its commit, how long ago (seconds) and the jobs that failed, as pl-2 and pl-8 show them. */
export const RUNS: Record<string, { n: number; event: string; status: "passed" | "failed" | "under way"; sha?: string; ago?: number; failed?: string[]; subject?: string }[]> = {
  "hyper-light/kiln:.kiln/ci.yaml": [
    { n: 412, event: "push", status: "passed", sha: "a3f2c1d9e8b7", ago: 12 * 60 },
    { n: 411, event: "push", status: "failed", sha: "9e1b2c0f7a6d", ago: 2 * 3600, failed: ["rust"] },
    { n: 409, event: "push", status: "passed", sha: "15878aa3c2b1", ago: 86_400 },
  ],
  "hyper-light/kiln:.kiln/smoke.yaml": [
    { n: 88, event: "deploy", status: "passed", subject: "#153 synced", sha: "a3f2c1d9e8b7", ago: 6 * 3600 },
    { n: 87, event: "schedule", status: "passed", subject: "03:00", sha: "a3f2c1d9e8b7", ago: 14 * 3600 },
    { n: 86, event: "comment", status: "failed", subject: "/smoke on #212", sha: "c71d0e2a9b8f", ago: 86_400 },
  ],
  "hyper-light/kiln:.kiln/e2e.yaml": [{ n: 301, event: "push", status: "failed", sha: "a3f2c1d9e8b7", ago: 3 * 3600, failed: ["e2e"] }],
  "hyper-light/kiln:.kiln/release.yaml": [{ n: 17, event: "push", status: "passed" }],
  "hyper-light/quiver:.github/workflows/ci.yml": [
    { n: 54, event: "push", status: "passed", sha: "1f9b2c4", ago: 3 * 86_400 },
    { n: 53, event: "push", status: "passed", sha: "8d02e7a", ago: 4 * 86_400 },
  ],
  "hyper-light/quiver:.github/workflows/ci.yml:draft:d-update-88": [{ n: 55, event: "pull_request", status: "under way" }],
  "hyper-light/quiver:.github/workflows/bench.yml": [{ n: 30, event: "schedule", status: "passed" }],
};

/** Full releases at the floating tag's exact mirrored commit (W29-R04, pl-2). */
export const RESOLVED: Record<string, string> = { "actions/cache@v4": "4.2.3 · 5a3ec84" };

/** W29-R04, pl-7: display facts at the called tags, which need not be their repository's current file. */
export const CALLED: Record<string, { job?: string; machine?: string; uses: [string, boolean][] }> = Object.fromEntries(
  ["v2.3.0", "v2.4.0"].map((tag) => [
    `hyper-light/ci-kit/.github/workflows/rust-check.yml@${tag}`,
    {
      job: "check",
      machine: "bare metal",
      uses: [
        [`hyper-light/ci-kit/.github/actions/setup-rust@${tag}`, false],
        ["kiln/checkout@v1", false],
      ],
    },
  ]),
);

/** Each repository's tags, newest first, with their commits. */
/** How long ago each tag was made, in seconds, by `repo:tag` (pl-5's `3d`). */
export const TAG_AGO: Record<string, number> = { "hyper-light/ci-kit:v2.4.0": 3 * 86_400, "hyper-light/ci-kit:v2.3.0": 21 * 86_400, "hyper-light/ci-kit:v0.1.0": 70 * 86_400 };

/** The inputs an action added since its newest release (pl-5's `new since v2.4.0`). */
export const ADDED: Record<string, string[]> = { "hyper-light/ci-kit:.github/actions/setup-rust": ["targets"] };

/** Each job's time in its workflow's last finished run, in seconds (pl-2's `18m`). */
export const JOB_SECS: Record<string, Record<string, number>> = {
  "hyper-light/kiln:.kiln/ci.yaml": { rust: 18 * 60 + 40, ui: 4 * 60, vendor: 40 },
};

/** Where each secret and variable is set (pl-2's `repository`, `server`). */
export const SET_IN: Record<string, string> = { CARGO_REGISTRY_TOKEN: "repository", KILN_BENCH_HOST: "server" };

export const TAGS: Record<string, [string, string][]> = {
  "hyper-light/ci-kit": [
    ["v2.4.0", "7ac3e1d0b9a8f7e6d5c4b3a2f1e0d9c8b7a6f5e4"],
    ["v2.3.0", "5b2c9e1a7d6f4e3c2b1a0f9e8d7c6b5a4f3e2d10"],
    ["v2", "7ac3e1d0b9a8f7e6d5c4b3a2f1e0d9c8b7a6f5e4"],
    ["v0.1.0", "d1a0c3f9e8d7c6b5a4f3e2d1c0b9a8f7e6d5c4b3"],
  ],
};

/** Who used which version of a shared action or reusable workflow, by `repo:path` (W29-R20). */
export const USED_BY: Record<string, { repo: string; ref: string; sha: string; runs: number; workflows?: string[] }[]> = {
  "hyper-light/ci-kit:.github/actions/setup-rust": [
    { repo: "hyper-light/kiln", ref: "v2.3.0", sha: "5b2c9e1a7d6f", runs: 412, workflows: ["ci", "release"] },
    { repo: "hyper-light/quiver", ref: "v2.4.0", sha: "7ac3e1d0b9a8", runs: 54, workflows: ["ci"] },
    { repo: "hyper-light/hyperlight-site", ref: "v2.4.0", sha: "7ac3e1d0b9a8", runs: 31, workflows: ["deploy"] },
  ],
  "hyper-light/ci-kit:.github/workflows/rust-check.yml": [{ repo: "hyper-light/quiver", ref: "v2.3.0", sha: "5b2c9e1a7d6f", runs: 54, workflows: ["ci"] }],
};

/**
 * kiln's update drafts (W29-R22), as the server says them: its strip before anything's picked and
 * the words it sets in bold, what it changes for its repository in sentences with its draft's run
 * (pl-7), its upstream's files (after them in its Changes), and the use it moves (marked in Uses).
 */
export const UPDATES: Record<
  string,
  {
    said: string[];
    strong: string[];
    changes: { key: string; kind: string; name: string; value: string; state: string }[];
    upstream: [string, string][];
    update: { uses: string; from: string; to: string };
    /** The use it moves, as it's written without its ref. */
    moves: string;
  }
> = {
  "hyper-light/quiver:.github/workflows/ci.yml:draft:d-update-88": {
    said: ["rust-check v2.4.0 is out", "3 commits since v2.3.0", "a new optional input; no new secrets, permissions or egress", "kiln opened draft pull request #88"],
    strong: ["rust-check v2.4.0", "#88"],
    changes: [
      { key: "changes.input.targets", kind: "change", name: "a new input, targets (optional): quiver passes none, so nothing changes", value: "safe", state: "added" },
      { key: "changes.access", kind: "note", name: "no new secrets, permissions or egress", value: "same", state: "passed" },
      { key: "changes.run", kind: "note", name: "#88's run on its draft: rust / check running, bench queued", value: "4m", state: "running" },
    ],
    upstream: [
      [".github/workflows/rust-check.yml", "1 file changed, 4 insertions(+), 1 deletion(-)"],
      [".github/actions/setup-rust", "2 files changed, 12 insertions(+)"],
    ],
    update: { uses: "rust-check", from: "v2.3.0", to: "v2.4.0" },
    moves: "hyper-light/ci-kit/.github/workflows/rust-check.yml",
  },
};

/** A repository's own checks on its head, by `repo:path` of what's released there, as the server says them in an action's Changes (pl-5); cache-cargo's failed, as its Release says. */
export const CHECKS: Record<string, { name: string; value: string; state: string }> = {
  "hyper-light/ci-kit:.github/actions/setup-rust": { name: "ci-kit's own checks passed on e41a9c2", value: "4 jobs", state: "passed" },
  "hyper-light/ci-kit:.github/actions/cache-cargo": { name: "ci-kit's own checks failed on e41a9c2", value: "4 jobs", state: "failed" },
};

/** W29-R02, pl-1: the commit that last changed each file (an action's folder), independent of the repository tip. */
export const LAST_COMMIT: Record<string, string> = {
  "hyper-light/kiln:.kiln/stress.yaml:draft:d-stress": "8d7e6f5".padEnd(40, "0"),
  "hyper-light/kiln:.kiln/release.yaml": "72cd9e3".padEnd(40, "0"),
  "hyper-light/kiln:.kiln/nightly.ts": "b07d1e9".padEnd(40, "0"),
  "hyper-light/kiln:.github/workflows/ci.yml": "02bbce1".padEnd(40, "0"),
  "hyper-light/ci-kit:.github/actions/sandbox": "d1a0c3f".padEnd(40, "0"),
  "hyper-light/ci-kit:.github/actions/fuzz-target:draft:d-fuzz": "3be09a1".padEnd(40, "0"),
  "hyper-light/quiver:.github/workflows/ci.yml:draft:d-update-88": "4c19e0b".padEnd(40, "0"),
};

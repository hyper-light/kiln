// kiln's own .kiln/ci.yaml in TypeScript (docs/ci.md W29-AC25): the same workflow, made by a
// program. The comments on why each job asks for what it asks are the YAML's.
import { workflow, job, step, type Job, type Step } from "@kiln/sdk";

/** One UI build per ui/ tree for ci, release and image (W28-R77): the same step in every job that builds it. */
const ui: Step = step({
  name: "ui",
  run: 'export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH" && npm ci --prefix ui --no-audit --no-fund && KILN_UI_SERVER=off npm run --prefix ui build',
});

/** The UI server this build carries, for this machine (vendor/bun/README.md). */
const packUiServer: Step = step({ run: "node ui/scripts/pack-ui-server.mjs --require" });

/** What a bare-metal job asks of the machine it runs on. */
const tools = (...names: string[]): string[] => names.map((t) => `tools has ${t}`);

/** Jobs that run on the trunk only. */
const trunk = "branch == 'main'";

const named = (steps: [name: string, run: string][]): Step[] => steps.map(([name, run]) => step({ name, run }));

const rust: Job = job({
  name: "rust lint + test",
  "runs-on": ["k8s"],
  toolchain: "rust",
  image: "docker.io/library/rust:1-trixie",
  resources: { cpu: "2", memory: "24Gi" },
  env: { CARGO_BUILD_JOBS: "4", RUST_TEST_THREADS: "4", CARGO_PROFILE_DEV_DEBUG: "line-tables-only" },
  "timeout-minutes": 90,
  steps: named([
    ["toolchain", "rustup component add clippy && cargo --version"],
    ["clippy (no-panic lint wall)", "cargo clippy --locked --all-targets -- -D warnings"],
    ["build the libraries' tests", "cargo test --locked --lib --no-run"],
    ["test", "cargo test --locked --no-fail-fast"],
  ]),
});

const uiJob: Job = job({
  name: "ui typecheck + lint + build",
  "runs-on": ["k8s"],
  toolchain: "node",
  resources: { cpu: "2", memory: "4Gi" },
  steps: [ui, step({ run: "npm ci --prefix ui --no-audit --no-fund && npm run --prefix ui typecheck && npm run --prefix ui lint && npm run --prefix ui test:ci" })],
});

const uiChecks: Job = job({
  name: "ui checks (Chromium and WebKit, desktop and phone)",
  needs: ["rust", "ui"],
  "runs-on": ["baremetal", "linux"],
  requires: ["os == linux", "executors has docker", ...tools("cargo", "node", "python3", "git")],
  if: trunk,
  "timeout-minutes": 120,
  env: { CARGO_BUILD_JOBS: "4", CARGO_PROFILE_DEV_DEBUG: "0" },
  steps: [
    step({
      name: "what it needs",
      run: `for t in docker python3 git node npm cargo; do command -v "$t" > /dev/null || { echo "this machine has no $t"; exit 1; }; done && docker info --format '{{.ServerVersion}}'`,
    }),
    ui,
    packUiServer,
    step({ run: "cargo build --locked -p kiln --bin kiln" }),
    step({ run: "ci/ui/run-all.sh target/debug/kiln" }),
  ],
  artifacts: ["ci/ui/shots/*/*"],
});

const vendor: Job = job({
  name: "vendored projects (upstream plus kiln's recorded changes)",
  "runs-on": ["k8s"],
  toolchain: "python",
  resources: { cpu: "1", memory: "2Gi" },
  "timeout-minutes": 15,
  steps: named([
    ["git", "command -v git >/dev/null || { apt-get update && apt-get install -y --no-install-recommends git ca-certificates; }"],
    ["the vendor tool's tests", "python3 scripts/test_vendor.py"],
    ["every vendored git project is its upstream plus its recorded changes", "python3 scripts/vendor.py check --kind git"],
  ]),
});

const releaseBinary: Job = job({
  name: "release build (bare metal)",
  needs: ["rust", "ui"],
  "runs-on": ["baremetal"],
  requires: ["os == linux", ...tools("cargo", "node")],
  if: trunk,
  steps: [ui, packUiServer, step({ run: "cargo build --release --locked -p kiln" }), step({ run: "mkdir -p dist && cp target/release/kiln dist/kiln-$(uname -m)" })],
  artifacts: ["dist/*"],
});

export default workflow({
  name: "ci",
  on: { push: { branches: ["main"] }, pull_request: {}, manual: {} },
  // One run per branch at a time, and it finishes: the newest push waits for it.
  concurrency: { group: "kiln-${{ branch }}", "cancel-in-progress": false },
  jobs: {
    rust,
    ui: uiJob,
    "ui-checks": uiChecks,
    vendor,
    "release-binary": releaseBinary,
  },
});

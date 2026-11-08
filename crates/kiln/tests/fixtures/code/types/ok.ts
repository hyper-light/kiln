// W29-R27: a code workflow as @kiln/sdk types it: a function of the event, its jobs made in a loop.
import { workflow, job, step, tree, type Event, type Job, type Program } from "@kiln/sdk";

type Target = { os: "linux" | "macos"; arch: "amd64" | "arm64" };

const targets: Target[] = [
  { os: "linux", arch: "amd64" },
  { os: "macos", arch: "arm64" },
];

const build = (t: Target): Job =>
  job({
    "runs-on": `${t.os}/${t.arch}`,
    "timeout-minutes": 30,
    steps: [step({ uses: "actions/checkout@v4" }), step({ run: "cargo build --release", env: { CARGO_TERM_COLOR: "always" } })],
  });

const program: Program = (e: Event) =>
  workflow({
    name: "ci",
    on: { push: { branches: ["main"] }, pull_request: null },
    jobs: {
      ...Object.fromEntries(targets.map((t) => [`build-${t.os}`, build(t)])),
      ...(tree.read("ui/package.json") === null ? {} : { ui: job({ "runs-on": "linux/amd64", steps: [step({ run: "npm ci && npm test" })] }) }),
      ...(e.ref === "refs/heads/main" ? { release: job({ needs: ["build-linux"], "runs-on": "linux/amd64", "timeout-minutes": "${{ inputs.minutes }}", steps: [step({ run: "echo ${{ github.sha }}" })] }) } : {}),
    },
  });

export default program;

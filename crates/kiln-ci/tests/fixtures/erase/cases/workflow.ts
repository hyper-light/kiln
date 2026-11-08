import { workflow, job, step, type Job } from "@kiln/sdk";

interface Target {
  os: string;
  arch: "amd64" | "arm64";
}

const targets: Target[] = [
  { os: "linux", arch: "amd64" },
  { os: "macos", arch: "arm64" },
];

function build(t: Target): Job {
  return job({
    "runs-on": `${t.os}/${t.arch}`,
    steps: [step({ run: "cargo build --release" })],
  });
}

export default workflow({
  name: "ci",
  on: { push: { branches: ["main"] } },
  jobs: Object.fromEntries(targets.map((t): [string, Job] => [`build-${t.os}`, build(t)])),
}) satisfies object;

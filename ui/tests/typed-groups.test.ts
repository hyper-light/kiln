import assert from "node:assert/strict";
import { test } from "node:test";
import type { ShipPipelineGroup } from "../src/api/ship.ts";
import { addedOf, jobLine, spanExact, stagesOf, typedGroups } from "../src/lib/typed-groups.ts";

const SERVER: ShipPipelineGroup[] = [
  { name: "Triggers", lines: [{ key: "on.push", kind: "trigger", name: "push", value: "branches main" }] },
  {
    name: "Stage 1",
    lines: [
      { key: "jobs.rust", kind: "job", name: "rust", value: "baremetal, linux · bare metal" },
      { key: "jobs.ui", kind: "job", name: "ui", value: "baremetal · bare metal" },
    ],
  },
  { name: "Stage 2", lines: [{ key: "jobs.checks", kind: "job", name: "checks", value: "baremetal · bare metal · needs rust, ui" }] },
  { name: "Uses", lines: [{ key: "uses.kiln/checkout@v1", kind: "use", name: "kiln/checkout@v1", value: "built in · v1" }] },
  { name: "Runs", lines: [] },
];
const job = (id: string, more = "") => `  ${id}:\n    runs-on: [baremetal]\n${more}    steps:\n      - uses: kiln/checkout@v1\n`;
const TEXT = `on: push\njobs:\n${job("rust")}${job("ui")}${job("checks", "    needs: [rust, ui]\n")}`;

test("w29_r05_a_job_and_a_use_typed_in_join_their_groups_marked_new", () => {
  const helm = "  helm-lint:\n    runs-on: [baremetal, linux]\n    steps:\n      - uses: azure/setup-helm@v4\n";
  const out = typedGroups(TEXT + helm, SERVER);
  assert.ok(out);
  assert.deepEqual(
    out.map((g) => g.name),
    ["Triggers", "Stage 1", "Stage 2", "Uses", "Runs"],
  );
  // The server's lines as they were; the new job beside them, worded as the server words a job.
  const stage1 = out.find((g) => g.name === "Stage 1")?.lines ?? [];
  assert.deepEqual(
    stage1.map((l) => l.key),
    ["jobs.rust", "jobs.ui", "jobs.helm-lint"],
  );
  assert.equal(stage1[0], SERVER[1]?.lines[0]);
  assert.deepEqual(stage1[2], { key: "jobs.helm-lint", kind: "job", name: "helm-lint", value: "bare metal · linux", note: "bare metal, linux", state: "added" });
  // A use following a tag is new and asks to be pinned; one pinned to a commit doesn't.
  const uses = out.find((g) => g.name === "Uses")?.lines ?? [];
  assert.deepEqual(
    uses.map((l) => [l.key, l.state ?? null]),
    [
      ["uses.kiln/checkout@v1", null],
      ["uses.azure/setup-helm@v4", "added"],
    ],
  );
  assert.match(uses[1]?.value ?? "", /new · pin it\?$/);
  const pinned = typedGroups(TEXT + helm.replace("@v4", "@1a275c3f0b9e8d7c6b5a4f3e2d1c0b9a8f7e6d5c"), SERVER);
  assert.doesNotMatch(pinned?.find((g) => g.name === "Uses")?.lines[1]?.value ?? "", /pin it/);
  assert.deepEqual(
    addedOf(out).map((l) => l.name),
    ["helm-lint", "azure/setup-helm@v4"],
  );
});

test("w29_r05_a_job_taken_out_is_gone_and_its_stages_follow_needs", () => {
  // `checks` taken out: Stage 2 goes; `ui` now needs `rust`, so it's Stage 2's.
  const text = `on: push\njobs:\n${job("rust")}${job("ui", "    needs: rust\n")}`;
  const out = typedGroups(text, SERVER);
  assert.deepEqual(
    out?.filter((g) => g.name.startsWith("Stage ")).map((g) => [g.name, g.lines.map((l) => l.key)]),
    [
      ["Stage 1", ["jobs.rust"]],
      ["Stage 2", ["jobs.ui"]],
    ],
  );
  assert.deepEqual(addedOf(out ?? []), []);
  // Not YAML yet, or no jobs: as the server gave them.
  assert.equal(typedGroups("jobs: [", SERVER), null);
  assert.equal(typedGroups("on: push\n", SERVER), null);
  // A cycle's jobs stay in the first stage, where its problems say why.
  assert.deepEqual(stagesOf({ a: { needs: "b" }, b: { needs: "a" }, c: {} }), [["a", "b", "c"]]);
});

test("w29_r04_a_picked_jobs_strip_says_what_it_reserves_and_its_last_run_as_the_server_does", () => {
  // As kiln_ci::pipelines::job_line's note (pl-2): where it runs, what it reserves, its last run.
  const rust = { "runs-on": ["baremetal", "linux"], resources: { cpu: 2, memory: "16Gi" } };
  assert.equal(jobLine("rust", rust, 18 * 60 + 40, { outcome: "passed", sha: "a3f2c1d9e8b7" }).note, "bare metal, linux · 2 CPUs, 16 GiB · its last run passed in 18m 40s on a3f2c1d");
  assert.equal(jobLine("rust", rust, 18 * 60 + 40, { outcome: "passed", sha: "a3f2c1d9e8b7" }).value, "bare metal · linux · 18m");
  assert.equal(jobLine("ui", { "runs-on": ["baremetal"] }, 240, { outcome: "failed", sha: "a3f2c1d9e8b7" }).note, "bare metal · its last run failed after 4m on a3f2c1d");
  assert.equal(jobLine("vendor", { "runs-on": ["baremetal"] }).note, "bare metal", "a job the last run didn't time says where it runs alone");
  assert.equal(jobLine("x", { resources: { cpu: "500m", memory: "512Mi", disk: "10Gi", gpu: 1 } }).note, "bare metal · 0.5 CPUs, 512 MiB, 10 GiB of disk, 1 GPU");
  assert.equal(jobLine("x", { resources: { memory: "much" } }).note, "bare metal", "what isn't a quantity says nothing");
  for (const [secs, said] of [
    [40, "40s"],
    [18 * 60 + 40, "18m 40s"],
    [18 * 60, "18m"],
    [2 * 3600 + 5 * 60, "2h 5m"],
    [3 * 86_400 + 4 * 3600, "3d 4h"],
  ] as const)
    assert.equal(spanExact(secs), said);
});

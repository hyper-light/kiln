import assert from "node:assert/strict";
import { test } from "node:test";
import type { Job, KilnBinary, Run, Step, UpdateJob } from "../src/api/types.ts";
import { buildLogOf, findRun, stepToShow } from "../src/views/updates/logs.ts";
import type { PlatformView } from "../src/views/updates/words.ts";

const S = 1_800_000_000;

const run = (number: number, over: Partial<Run> = {}): Run => ({
  id: `run-${number}`,
  repo_id: "r",
  repo_name: "hyper-light/kiln",
  number,
  pipeline: "release",
  event: "push",
  branch: "main",
  sha: `${number}`.padEnd(40, "a"),
  message: "",
  author: "",
  status: "succeeded",
  created_at: S - (1000 - number) * 60,
  started_at: null,
  finished_at: null,
  error: null,
  concurrency_group: null,
  ...over,
});

const bin = (over: Partial<KilnBinary>): KilnBinary => ({ target: "aarch64-unknown-linux-musl", sha256: "ab".repeat(32), size: 1, build: "b1", origin: "built", at: S - 600, ...over });

const view = (over: Partial<PlatformView> = {}): PlatformView => ({
  key: "aarch64-unknown-linux-musl",
  target: "aarch64-unknown-linux-musl",
  os: "linux",
  arch: "aarch64",
  abi: "musl",
  label: "linux · aarch64 · musl",
  binary: null,
  shadowed: [],
  commit: null,
  state: "current",
  tone: "ok",
  job: null,
  lit: false,
  runners: [],
  ...over,
});

const job = (key: string, steps: Partial<Step>[] = []): Job => ({
  id: `job-${key}`,
  run_id: "run-709",
  key,
  display: key,
  status: "running",
  labels: [],
  needs: [],
  agent: null,
  executor: null,
  attempt: 1,
  queued_at: null,
  started_at: null,
  finished_at: null,
  exit_code: null,
  error: null,
  peak_rss: null,
  cpu_seconds: null,
  steps: steps.map((s, i) => ({ idx: i, name: `s${i}`, status: "pending", started_at: null, finished_at: null, exit_code: null, ...s })),
});

const upgrade = (over: Partial<UpdateJob>): UpdateJob => ({
  id: "u1",
  kind: "runner",
  target: "mac",
  method: "build",
  state: "done",
  started_at: S - 900,
  ended_at: S - 600,
  from_build: null,
  to_build: null,
  commit: "c0ffee".padEnd(40, "0"),
  reached: true,
  log: ["built"],
  error: null,
  ...over,
});

test("a platform the release run under way builds opens that job, followed live", () => {
  const last = run(709, { status: "running" });
  const j = job("linux");
  assert.deepEqual(buildLogOf(view({ state: "building", job: j, binary: bin({ run: 701 }) }), last, []), { kind: "job", run: last, job: j });
});

test("a build of the release pipeline opens its run's job of the platform's family, the run found by its number and commit", () => {
  const sha = "df17c34".padEnd(40, "1");
  assert.deepEqual(buildLogOf(view({ binary: bin({ run: 525, sha }) }), run(709), []), { kind: "run", id: null, number: 525, sha, family: "linux" });
  const mac = buildLogOf(view({ target: "aarch64-apple-darwin", binary: bin({ target: "aarch64-apple-darwin", run: 701, sha: undefined }) }), null, []);
  assert.equal(mac.kind === "run" && mac.family, "macos");
  // The last run failed to build it: that run's job says why, read by its id.
  assert.deepEqual(buildLogOf(view({ state: "failed", binary: bin({ run: 700 }) }), run(709, { status: "failed" }), []), {
    kind: "run",
    id: "run-709",
    number: 709,
    sha: run(709).sha,
    family: "linux",
  });
});

test("a machine's build of itself opens that upgrade's output; a build kiln has no log of says why", () => {
  const made = bin({ target: "aarch64-apple-darwin", origin: "machine", by: "mac", sha: "c0ffee".padEnd(40, "0"), run: undefined });
  const jobs = [upgrade({ id: "old", started_at: S - 5000 }), upgrade({ id: "new" }), upgrade({ id: "failed", state: "failed", started_at: S - 100 }), upgrade({ id: "other", target: "pi" })];
  const log = buildLogOf(view({ target: "aarch64-apple-darwin", binary: made }), null, jobs);
  assert.equal(log.kind === "upgrade" && log.job.id, "new", "its newest build that didn't fail");
  assert.equal(buildLogOf(view({ binary: made }), null, []).kind, "none", "its output no longer kept");
  for (const origin of ["server", "upload", "release"] as const) {
    const said = buildLogOf(view({ binary: bin({ origin, run: undefined }) }), null, []);
    assert.equal(said.kind, "none", origin);
    assert.match(said.kind === "none" ? said.why : "", /no build log/, origin);
  }
  assert.equal(buildLogOf(view({ state: "missing" }), null, []).kind, "none");
});

test("the step shown first: the one running, else the one that failed, else the last that ran", () => {
  assert.equal(stepToShow(job("l", [{ status: "succeeded", started_at: 1 }, { status: "running", started_at: 2 }, {}]).steps), 1);
  assert.equal(stepToShow(job("l", [{ status: "succeeded", started_at: 1 }, { status: "failed", started_at: 2 }, { status: "skipped" }]).steps), 1);
  assert.equal(
    stepToShow(
      job("l", [
        { status: "succeeded", started_at: 1 },
        { status: "succeeded", started_at: 2 },
      ]).steps,
    ),
    1,
  );
  assert.equal(stepToShow(job("l", [{ status: "queued" }, { status: "queued" }]).steps), 0, "nothing ran yet: the first");
  assert.equal(stepToShow([]), null);
});

test("a run is found by its number a page at a time back from the newest, and not when it's gone", async () => {
  const all = Array.from({ length: 700 }, (_, i) => run(700 - i));
  const asked: (number | undefined)[] = [];
  const page = async (before: number | undefined) => {
    asked.push(before);
    return all.filter((r) => before === undefined || r.created_at < before).slice(0, 200);
  };
  assert.equal((await findRun(page, 525, run(525).sha.slice(0, 7), 200))?.id, "run-525");
  assert.equal(asked.length, 1, "it was on the first page");
  asked.length = 0;
  assert.equal((await findRun(page, 120, null, 200))?.id, "run-120");
  assert.equal(asked.length, 3);
  // A run pruned away: none, and it stops once the runs read are older than it.
  asked.length = 0;
  assert.equal(await findRun(async (b) => (await page(b)).filter((r) => r.number !== 300), 300, null, 200), null);
  assert.equal(asked.length, 3);
  // The same number of another commit isn't it.
  assert.equal(await findRun(page, 525, "f".repeat(40), 200), null);
});

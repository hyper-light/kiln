import assert from "node:assert/strict";
import { readFileSync, writeFileSync } from "node:fs";
import { test } from "node:test";
import { parse } from "yaml";
import { TRIGGER_KINDS, TRIGGER_GROUPS, triggerLines } from "../src/lib/triggers.ts";
import { addOverlayTrigger, addTrigger, EditRefused, removeTrigger, setOverlayScheduleField, setScheduleField, setTriggerFilter } from "../src/lib/workflow-doc.ts";

const JOBS = "jobs:\n  a:\n    runs-on: [baremetal]\n    steps:\n      - run: echo hi\n";
const EMPTY = `name: x\n${JOBS}`;

test("w29_r06_add_a_trigger_lists_18_kinds_in_six_groups", () => {
  assert.equal(TRIGGER_KINDS.length, 18);
  assert.deepEqual([...new Set(TRIGGER_KINDS.map((k) => k.group))], [...TRIGGER_GROUPS]);
  // GitHub's are the ones a GitHub workflow can say; kiln's only its own dialect.
  for (const k of TRIGGER_KINDS) assert.equal(k.gha === null, k.whose === "kiln", k.id);
});

test("w29_r06_each_kind_added_reads_back_as_itself_the_rest_byte_for_byte", () => {
  for (const kiln of [true, false]) {
    for (const k of TRIGGER_KINDS) {
      const keys = kiln ? k.kiln : k.gha;
      if (!keys) continue;
      const out = addTrigger(EMPTY, keys, k.first(kiln));
      // The file around it as it was: `on:` is new, before `jobs:`.
      assert.ok(out.startsWith("name: x\non:\n"), `${k.id}: ${out}`);
      assert.ok(out.endsWith(JOBS), `${k.id}: ${out}`);
      const events = (triggerLines(out, kiln) ?? []).map((l) => l.key);
      for (const key of keys) assert.ok(events.includes(`on.${kiln && key === "manual" ? "workflow_dispatch" : key}`), `${k.id} (${kiln ? "kiln" : "gha"}): ${events.join(", ")}`);
      // Its lines are what the kind starts with.
      const on = (parse(out) as { on: Record<string, unknown> }).on;
      for (const key of keys) assert.deepEqual(on[key], k.first(kiln), k.id);
    }
  }
});

test("w29_r06_triggers_edited_comments_kept", () => {
  const src = "# smoke, after deploys\nname: smoke\non:\n  # every push to main\n  push:\n    branches: [main]\n  pull_request:\n" + JOBS;
  // Another kind beside them, the comments where they were.
  const added = addTrigger(src, ["schedule"], [{ cron: "0 3 * * *" }]);
  assert.ok(added.includes("# every push to main\n  push:\n    branches: [main]\n"), added);
  assert.match(added, /\n {2}schedule:\n {4}- cron: "?0 3 \* \* \*"?\n/);
  // A filter set on a trigger with none, one changed, one emptied out.
  const pr = setTriggerFilter(added, "pull_request", "branches", ["main", "release/*"]);
  assert.deepEqual((parse(pr) as { on: { pull_request: unknown } }).on.pull_request, { branches: ["main", "release/*"] });
  const tags = setTriggerFilter(pr, "push", "tags", ["v*"]);
  assert.deepEqual((parse(tags) as { on: { push: unknown } }).on.push, { branches: ["main"], tags: ["v*"] });
  const none = setTriggerFilter(tags, "push", "branches", []);
  assert.deepEqual((parse(none) as { on: { push: unknown } }).on.push, { tags: ["v*"] });
  assert.ok(none.includes("# every push to main"), none);
  // A schedule's options, then one taken out.
  const tz = setScheduleField(added, 0, "timezone", "America/Los_Angeles");
  const skip = setScheduleField(tz, 0, "skip-if-unchanged", true);
  assert.deepEqual((parse(skip) as { on: { schedule: unknown } }).on.schedule, [{ cron: "0 3 * * *", timezone: "America/Los_Angeles", "skip-if-unchanged": true }]);
  assert.deepEqual((parse(setScheduleField(skip, 0, "timezone", null)) as { on: { schedule: unknown } }).on.schedule, [{ cron: "0 3 * * *", "skip-if-unchanged": true }]);
  assert.throws(() => setScheduleField(skip, 0, "cron", ""), EditRefused);
  // Taken out, and only it.
  const gone = removeTrigger(added, "pull_request");
  assert.deepEqual(Object.keys((parse(gone) as { on: object }).on), ["push", "schedule"]);
  assert.ok(gone.startsWith("# smoke, after deploys\nname: smoke\non:\n  # every push to main\n"), gone);
});

test("w29_r06_one_line_triggers_become_a_mapping", () => {
  assert.deepEqual((parse(addTrigger(`on: push\n${JOBS}`, ["workflow_dispatch"], {})) as { on: unknown }).on, { push: {}, workflow_dispatch: {} });
  assert.deepEqual((parse(addTrigger(`on: [push, pull_request]\n${JOBS}`, ["release"], { types: ["published"] })) as { on: unknown }).on, {
    push: {},
    pull_request: {},
    release: { types: ["published"] },
  });
  assert.throws(() => addTrigger(`on: [push]\n${JOBS}`, ["push"], {}), EditRefused);
  assert.throws(() => removeTrigger(`on: [push]\n${JOBS}`, "push"), EditRefused);
});

test("w29_r06_kilns_own_triggers_go_in_a_github_workflows_overlay", () => {
  const wf = ".github/workflows/ci.yml";
  // None yet: one begun, saying what it is.
  const begun = addOverlayTrigger("", wf, ["diff"], { branches: ["main"] });
  assert.ok(begun.startsWith("# kiln's own keys for .github/workflows/ci.yml"), begun);
  assert.deepEqual(parse(begun), { on: { diff: { branches: ["main"] } } });
  // Another beside it, the first and its comment kept.
  const two = addOverlayTrigger(begun, wf, ["land"], {});
  assert.ok(two.startsWith(begun.trimEnd()), two);
  assert.deepEqual(Object.keys((parse(two) as { on: object }).on), ["diff", "land"]);
  // An overlay of jobs' keys gets its triggers before them.
  const jobs = "jobs:\n  build:\n    retry: { max: 2 }\n";
  const before = addOverlayTrigger(jobs, wf, ["comment"], { commands: ["/run"] });
  assert.ok(before.endsWith(jobs), before);
  assert.deepEqual((parse(before) as { on: unknown }).on, { comment: { commands: ["/run"] } });
});

test("w29_r06_a_github_schedules_kiln_keys_go_in_its_overlay_by_cron", () => {
  const wf = ".github/workflows/bench.yml";
  const tz = setOverlayScheduleField("", wf, "0 4 * * *", "timezone", "America/Los_Angeles");
  assert.deepEqual((parse(tz) as { on: unknown }).on, { schedule: [{ cron: "0 4 * * *", timezone: "America/Los_Angeles" }] });
  // Its entry again; another cron's entry beside it; one taken out, its cron kept.
  const skip = setOverlayScheduleField(tz, wf, "0 4 * * *", "skip-if-unchanged", true);
  const other = setOverlayScheduleField(skip, wf, "30 2 * * 1", "jitter", "10m");
  assert.deepEqual((parse(other) as { on: unknown }).on, {
    schedule: [
      { cron: "0 4 * * *", timezone: "America/Los_Angeles", "skip-if-unchanged": true },
      { cron: "30 2 * * 1", jitter: "10m" },
    ],
  });
  assert.deepEqual((parse(setOverlayScheduleField(other, wf, "0 4 * * *", "timezone", null)) as { on: { schedule: unknown[] } }).on.schedule[0], { cron: "0 4 * * *", "skip-if-unchanged": true });
  // Nothing to set, nothing begun.
  assert.equal(setOverlayScheduleField("", wf, "0 4 * * *", "timezone", null), "");
  // Beside its triggers.
  const beside = setOverlayScheduleField(addOverlayTrigger("", wf, ["diff"], {}), wf, "0 4 * * *", "catch-up", true);
  assert.deepEqual((parse(beside) as { on: unknown }).on, { diff: {}, schedule: [{ cron: "0 4 * * *", "catch-up": true }] });
});

/**
 * Each kind's first lines as Add a trigger writes them, in a kiln workflow, in a GitHub one, and kiln's own kinds in a GitHub
 * workflow's overlay: kiln-ci's tests/trigger_kinds.rs reads each with no errors, as the server will (W29-R06).
 * `UPDATE_FIXTURES=1` writes the fixture again (then prettier it, as the UI's files are).
 */
test("w29_r06_each_kinds_first_lines_are_the_fixture_kiln_ci_reads", () => {
  const out: { kind: string; path: string; events: string[]; text: string }[] = [];
  for (const kiln of [true, false]) {
    for (const k of TRIGGER_KINDS) {
      const keys = kiln ? k.kiln : k.gha;
      const events = (keys ?? k.kiln).map((key) => (key === "manual" ? "workflow_dispatch" : key));
      if (keys) out.push({ kind: k.id, path: kiln ? ".kiln/x.yaml" : ".github/workflows/x.yml", events, text: addTrigger(EMPTY, keys, k.first(kiln)) });
      else out.push({ kind: k.id, path: ".kiln/overlays/x.yaml", events, text: addOverlayTrigger("", ".github/workflows/x.yml", k.kiln, k.first(true)) });
    }
  }
  assert.equal(out.length, 36);
  // Written plainly: no kind's lines shared through an anchor and its alias.
  for (const e of out) assert.ok(!/&a\d/.test(e.text), `${e.kind}: ${e.text}`);
  const at = new URL("./fixtures/trigger-kinds.json", import.meta.url);
  if (process.env.UPDATE_FIXTURES) writeFileSync(at, `${JSON.stringify(out, null, 2)}\n`);
  assert.deepEqual(JSON.parse(readFileSync(at, "utf8")), out);
});

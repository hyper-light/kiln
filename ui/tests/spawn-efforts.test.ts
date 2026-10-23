import assert from "node:assert/strict";
import { test } from "node:test";
import type { KindInfo } from "../src/api/harness.ts";
import { effortsFor, resumeEffort } from "../src/views/harness/spawn-efforts.ts";

const devin: KindInfo = {
  id: "devin", name: "Devin", verified: true, available: true, efforts: [],
  models: [
    { id: "other", label: "Other", efforts: ["low", "high"] },
    { id: "swe-2-high", label: "SWE-2", default: true, efforts: ["medium", "high", "max"] },
    { id: "adaptive", label: "Adaptive" },
  ],
};
const ids = (kind: KindInfo, model = "") => effortsFor(kind, model).map((e) => e.id);

test("Devin offers its native default model's effort before a model is selected", () => {
  assert.deepEqual(ids(devin), ["medium", "high", "max"]);
  assert.deepEqual(ids(devin, " "), ["medium", "high", "max"]);
});

test("an explicit model uses its own levels and clears incompatible remembered effort", () => {
  assert.deepEqual(ids(devin, " other "), ["low", "high"]);
  assert.equal(effortsFor(devin, "other").some((e) => e.id === "max"), false);
  assert.deepEqual(ids(devin, "adaptive"), []);
  assert.deepEqual(ids({ ...devin, models: [{ id: "none", label: "None", efforts: [] }], efforts: [{ id: "high", label: "High", description: "" }] }, "none"), []);
});

test("a default model's effort is not sent to an incompatible resumed conversation", () => {
  assert.equal(resumeEffort(devin, "swe-2-high", "max"), "max");
  assert.equal(resumeEffort(devin, "other", "max"), undefined);
  assert.equal(resumeEffort(devin, "other", "high"), "high");
  assert.equal(resumeEffort(devin, "adaptive", "max"), undefined);
  assert.equal(resumeEffort(devin, undefined, "max"), undefined);
});

test("old Claude and Codex runners retain their kind and static effort choices", () => {
  for (const id of ["claude", "codex"]) {
    const old: KindInfo = { id, name: id, verified: true, available: true, models: [{ id: "legacy", label: "Legacy" }] };
    assert.equal(ids(old).includes("high"), true);
    assert.deepEqual(ids(old, "legacy"), ids(old));
    assert.equal(resumeEffort(old, "legacy", "high"), "high");
    assert.equal(resumeEffort(old, undefined, "high"), "high");
    const known = { ...old, efforts: [{ id: "medium", label: "Medium", description: "Balanced" }] };
    assert.deepEqual(effortsFor(known, "legacy"), [{ id: "medium", label: "Medium", hint: "Balanced" }]);
  }
});

test("model effort labels retain harness names and readable fallbacks", () => {
  const kind: KindInfo = { ...devin, models: [{ id: "model", label: "Model", default: true, efforts: ["medium", "xhigh"] }], efforts: [{ id: "medium", label: "Balanced", description: "A balance" }] };
  assert.deepEqual(effortsFor(kind), [{ id: "medium", label: "Balanced", hint: "A balance" }, { id: "xhigh", label: "Extra high", hint: undefined }]);
});

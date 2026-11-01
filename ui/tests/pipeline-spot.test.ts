import assert from "node:assert/strict";
import { test } from "node:test";
import { spotOf } from "../src/lib/pipeline-spot.ts";

const CI = `name: ci
on:
  push: { branches: [main] }
  pull_request:
    branches: [main]
  workflow_dispatch:
jobs:
  rust:
    name: rust lint + test
    runs-on: [baremetal, linux]
    steps:
      - uses: kiln/checkout@v1
      - run: cargo test --locked
        env:
          TOKEN: \${{ secrets.CARGO_REGISTRY_TOKEN }}

  ui:
    needs: [rust]
    runs-on: [baremetal]
    uses: hyper-light/ci-kit/.github/workflows/rust-check.yml@v2.3.0
concurrency: ci-\${{ github.ref }}
`;

test("w29_r04_a_line_is_its_part_of_the_file", () => {
  assert.deepEqual(spotOf(CI, { key: "on.push", kind: "trigger" }), { from: 2, to: 2 });
  assert.deepEqual(spotOf(CI, { key: "on.pull_request", kind: "trigger" }), { from: 3, to: 4 });
  // A job: its key's line to its last, not the blank line after it.
  assert.deepEqual(spotOf(CI, { key: "jobs.rust", kind: "job" }), { from: 7, to: 14 });
  assert.deepEqual(spotOf(CI, { key: "jobs.ui", kind: "job" }), { from: 16, to: 19 });
  // A use: the first step or job that uses it.
  assert.deepEqual(spotOf(CI, { key: "uses.kiln/checkout@v1", kind: "use" }), { from: 11, to: 11 });
  assert.deepEqual(spotOf(CI, { key: "uses.hyper-light/ci-kit/.github/workflows/rust-check.yml@v2.3.0", kind: "use" }), { from: 19, to: 19 });
  // A secret: the first line that uses it.
  assert.deepEqual(spotOf(CI, { key: "secrets.CARGO_REGISTRY_TOKEN", kind: "secret" }), { from: 14, to: 14 });
  assert.deepEqual(spotOf(CI, { key: "settings.concurrency", kind: "setting" }), { from: 20, to: 20 });
});

test("w29_r04_what_isnt_in_the_file_has_no_place", () => {
  assert.equal(spotOf(CI, { key: "runs.12", kind: "run" }), null);
  assert.equal(spotOf(CI, { key: "settings.on", kind: "setting" }), null);
  assert.equal(spotOf(CI, { key: "jobs.nope", kind: "job" }), null);
  assert.equal(spotOf("on: [push\n", { key: "on.push", kind: "trigger" }), null, "text that isn't YAML yet has no spot");
});

test("w29_r04_events_without_a_mapping_and_an_actions_parts", () => {
  assert.deepEqual(spotOf("on: push\njobs: {}\n", { key: "on.push", kind: "trigger" }), { from: 0, to: 0 });
  assert.deepEqual(spotOf("on:\n  - push\n  - workflow_dispatch\n", { key: "on.workflow_dispatch", kind: "trigger" }), { from: 2, to: 2 });
  // kiln's dialect's `manual:` is GitHub's workflow_dispatch.
  assert.deepEqual(spotOf("on:\n  push: {}\n  manual:\n    inputs: { target: { type: string } }\n", { key: "on.workflow_dispatch", kind: "trigger" }), { from: 2, to: 3 });
  const action = `name: setup-rust
inputs:
  toolchain:
    description: which
  targets: { description: cross builds }
runs:
  using: composite
  steps:
    - uses: kiln/cache@v1
    - run: rustup toolchain install
      shell: bash
`;
  assert.deepEqual(spotOf(action, { key: "inputs.targets", kind: "input" }), { from: 4, to: 4 });
  assert.deepEqual(spotOf(action, { key: "runs.steps.1", kind: "step" }), { from: 9, to: 10 });
  assert.deepEqual(spotOf(action, { key: "call.inputs.depth", kind: "input" }), null);
});

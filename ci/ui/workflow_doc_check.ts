// The flow view's edits (ui/src/lib/workflow-doc.ts, docs/ci.md W21-R06): each one's text against
// what it should be exactly, so comments and the formatting of what an edit doesn't touch survive,
// and the edits that can't be made here refused.
//
//     bun ci/ui/workflow_doc_check.ts            (PRINT=1 shows each edit's text)
import * as W from "../../ui/src/lib/workflow-doc.ts";

const SRC = `# The build and its checks.
on: [push, pull_request]   # both
jobs:
  build:   # the build
    runs-on: local  # the runner
    strategy:
      matrix:
        os: [linux, mac]
        n: [1, 2]
        exclude:
          - { os: mac, n: 1 }
    steps:
    - uses: actions/checkout@v4   # pinned later
    - name: Test
      run: |
        make test
        make lint

  docs:
    needs: build
    runs-on: [self-hosted, linux]
    steps:
      - run: echo docs
`;

let failed = 0;
const say = (ok: boolean, what: string, detail = "") => {
  console.log(`${ok ? "ok  " : "FAIL"} ${what}${detail && !ok ? `\n${detail}` : ""}`);
  if (!ok) failed++;
};

/** `src` with each `[from, to]` replacement made, once each, in order. */
function change(src: string, ...edits: [string, string][]): string {
  let out = src;
  for (const [from, to] of edits) {
    if (!out.includes(from)) throw new Error(`no ${JSON.stringify(from)} in the source`);
    out = out.replace(from, to);
  }
  return out;
}

function expect(what: string, got: () => string, want: string) {
  try {
    const out = got();
    if (process.env.PRINT) console.log(`--- ${what}\n${out}`);
    say(out === want, what, `got:\n${out}\nwant:\n${want}`);
  } catch (e) {
    say(false, what, String(e));
  }
}

function refused(what: string, f: () => string) {
  try {
    f();
    say(false, what, "it was made");
  } catch (e) {
    say(e instanceof W.EditRefused, what, String(e));
  }
}

// Reading.
const flow = W.readFlow(SRC);
const build = flow.jobs.find((j) => j.id === "build");
const docs = flow.jobs.find((j) => j.id === "docs");
say(flow.error === null && flow.jobs.map((j) => j.id).join() === "build,docs", "the jobs, in order");
say(JSON.stringify(build?.runsOn) === '["local"]' && JSON.stringify(docs?.runsOn) === '["self-hosted","linux"]' && JSON.stringify(docs?.needs) === '["build"]', "labels and needs");
say(
  build?.steps.length === 2 && build.steps[0]?.uses === "actions/checkout@v4" && build.steps[1]?.name === "Test" && build.steps[1]?.run === "make test\nmake lint\n",
  "steps, each its name, uses and run",
);
say(
  JSON.stringify(build?.matrix?.axes) === '[{"name":"os","values":["linux","mac"]},{"name":"n","values":["1","2"]}]' && JSON.stringify(build?.matrix?.exclude) === '["{ os: mac, n: 1 }"]',
  "the matrix's axes and rows",
);
say(
  W.readFlow("on: push\njobs:\n  a:\n    runs-on: ${{ matrix.os }}\n    strategy:\n      matrix: ${{ fromJSON(inputs.m) }}\n").jobs[0]?.runsOnText === "${{ matrix.os }}",
  "an expression is shown, not edited",
);
say(W.readFlow("jobs: [").error !== null, "text that isn't YAML says so");

// Jobs.
expect("a job added, at the end, its list styled as most of the file's are", () => W.addJob(SRC, "lint", "local"), SRC + `  lint:\n    runs-on: local\n    steps:\n      - run: echo "lint"\n`);
expect(
  "or as this file's are",
  () => W.addJob(SRC.replaceAll("      - ", "    - ").replace("          - {", "        - {"), "lint", "local"),
  SRC.replaceAll("      - ", "    - ").replace("          - {", "        - {") + `  lint:\n    runs-on: local\n    steps:\n    - run: echo "lint"\n`,
);
expect(
  "a job removed, and from the needs of those that had it",
  () => W.removeJob(SRC, "build"),
  change(
    SRC,
    [
      "  build:   # the build\n    runs-on: local  # the runner\n    strategy:\n      matrix:\n        os: [linux, mac]\n        n: [1, 2]\n        exclude:\n          - { os: mac, n: 1 }\n    steps:\n    - uses: actions/checkout@v4   # pinned later\n    - name: Test\n      run: |\n        make test\n        make lint\n\n",
      "",
    ],
    ["    needs: build\n", ""],
  ),
);
refused("a job id that isn't one", () => W.addJob(SRC, "2fast", "local"));
refused("a job that's there already", () => W.addJob(SRC, "docs", "local"));

// Needs.
expect("a need added to one", () => W.addNeed(SRC, "docs", "lint"), change(SRC, ["    needs: build\n", "    needs: [build, lint]\n"]));
expect("a first need, before runs-on", () => W.addNeed(SRC, "build", "setup"), change(SRC, ["    runs-on: local  # the runner\n", "    needs: setup\n    runs-on: local  # the runner\n"]));
expect("the last need removed, with its key", () => W.removeNeed(SRC, "docs", "build"), change(SRC, ["    needs: build\n", ""]));
const two = W.addNeed(SRC, "docs", "lint");
expect("one of two needs removed", () => W.removeNeed(two, "docs", "build"), change(SRC, ["    needs: build\n", "    needs: [lint]\n"]));
expect("a need it hasn't, nothing", () => W.removeNeed(SRC, "docs", "nope"), SRC);

// Labels.
expect("a label for a label, its comment kept", () => W.setRunsOn(SRC, "build", ["big"]), change(SRC, ["runs-on: local  # the runner", "runs-on: big  # the runner"]));
expect("labels for labels, on their line", () => W.setRunsOn(SRC, "docs", ["self-hosted", "linux", "x64"]), change(SRC, ["[self-hosted, linux]", "[self-hosted, linux, x64]"]));
expect("one label for a list", () => W.setRunsOn(SRC, "docs", ["local"]), change(SRC, ["    runs-on: [self-hosted, linux]\n", "    runs-on: local\n"]));

// Steps.
expect(
  "a step added after a block scalar, the blank line kept",
  () => W.addStep(SRC, "build", { run: "make dist" }),
  change(SRC, ["        make lint\n\n", "        make lint\n    - run: make dist\n\n"]),
);
expect("an action added, indented as that list is", () => W.addStep(SRC, "docs", { uses: "actions/upload-artifact@v4" }), SRC + "      - uses: actions/upload-artifact@v4\n");
expect("a step removed", () => W.removeStep(SRC, "build", 0), change(SRC, ["    - uses: actions/checkout@v4   # pinned later\n", ""]));
expect(
  "two steps swapped, each with its comment",
  () => W.moveStep(SRC, "build", 1, -1),
  change(SRC, [
    "    - uses: actions/checkout@v4   # pinned later\n    - name: Test\n      run: |\n        make test\n        make lint\n",
    "    - name: Test\n      run: |\n        make test\n        make lint\n    - uses: actions/checkout@v4   # pinned later\n",
  ]),
);
expect("a step's name", () => W.setStepField(SRC, "build", 1, "name", "Tests"), change(SRC, ["- name: Test\n", "- name: Tests\n"]));
expect("a step's run, now one line", () => W.setStepField(SRC, "build", 1, "run", "make check"), change(SRC, ["      run: |\n        make test\n        make lint\n", "      run: make check\n"]));
expect(
  "a step's run, more lines",
  () => W.setStepField(SRC, "docs", 0, "run", "echo docs\necho done"),
  change(SRC, ["      - run: echo docs\n", "      - run: |-\n          echo docs\n          echo done\n"]),
);
expect("a step's first key removed: the step rewritten", () => W.setStepField(SRC, "build", 1, "name", ""), change(SRC, ["    - name: Test\n      run: |\n", "    - run: |\n"]));
refused("the last step removed: the YAML's to say", () => W.removeStep(SRC, "docs", 0));

// The matrix.
expect("an axis's values, on their line", () => W.setAxis(SRC, "build", "os", ["linux", "mac", "windows"]), change(SRC, ["os: [linux, mac]", "os: [linux, mac, windows]"]));
expect("a new axis, before exclude", () => W.setAxis(SRC, "build", "arch", ["x64", "arm64"]), change(SRC, ["        exclude:\n", "        arch: [x64, arm64]\n        exclude:\n"]));
expect(
  "a matrix made, before the steps; 3.10 stays 3.10",
  () => W.setAxis(SRC, "docs", "python", ["3.10", "3.12"]),
  change(SRC, ["    runs-on: [self-hosted, linux]\n    steps:\n", '    runs-on: [self-hosted, linux]\n    strategy:\n      matrix:\n        python: ["3.10", 3.12]\n    steps:\n']),
);
expect("an axis removed", () => W.removeAxis(SRC, "build", "n"), change(SRC, ["        n: [1, 2]\n", ""]));
expect(
  "a row added, spaced as the others",
  () => W.addRow(SRC, "build", "exclude", "os: linux, n: 2"),
  change(SRC, ["          - { os: mac, n: 1 }\n", "          - { os: mac, n: 1 }\n          - { os: linux, n: 2 }\n"]),
);
expect("the last row removed, with its key", () => W.removeRow(SRC, "build", "exclude", 0), change(SRC, ["        exclude:\n          - { os: mac, n: 1 }\n", ""]));
expect(
  "a first include",
  () => W.addRow(SRC, "build", "include", "os: bsd"),
  change(SRC, ["          - { os: mac, n: 1 }\n", "          - { os: mac, n: 1 }\n        include:\n          - os: bsd\n"]),
);
refused("a row that isn't key: value", () => W.addRow(SRC, "build", "include", "nonsense"));
refused("an axis named as a matrix's own key", () => W.setAxis(SRC, "build", "include", ["x"]));

// An action's inputs and outputs (W21-R07).
const ACTION = `name: Greet
description: Says hello.
inputs:
  who:   # whom
    description: Who to greet.
    default: world
runs:
  using: composite
  steps:
    - run: echo "hello"
      shell: bash
`;
const shape = W.readAction(ACTION);
say(
  shape.using === "composite" && shape.inputs.length === 1 && shape.inputs[0]?.default === "world" && shape.inputs[0]?.required === false && shape.outputs.length === 0,
  "an action's inputs and outputs, read",
);
expect("an input added", () => W.addField(ACTION, "inputs", "loud", "Shout it."), change(ACTION, ["    default: world\n", "    default: world\n  loud:\n    description: Shout it.\n"]));
expect(
  "the first output, before runs",
  () => W.addField(ACTION, "outputs", "greeting", "What it said."),
  change(ACTION, ["runs:\n", "outputs:\n  greeting:\n    description: What it said.\nruns:\n"]),
);
expect("an input's description, its comment kept", () => W.setField(ACTION, "inputs", "who", "description", "Whom to greet."), change(ACTION, ["Who to greet.", "Whom to greet."]));
expect("an input made required", () => W.setField(ACTION, "inputs", "who", "required", true), change(ACTION, ["    default: world\n", "    default: world\n    required: true\n"]));
expect("a default removed", () => W.setField(ACTION, "inputs", "who", "default", ""), change(ACTION, ["    default: world\n", ""]));
expect("the last input removed, with its key", () => W.removeField(ACTION, "inputs", "who"), change(ACTION, ["inputs:\n  who:   # whom\n    description: Who to greet.\n    default: world\n", ""]));
refused("an input that's there already", () => W.addField(ACTION, "inputs", "who", "again"));
refused("a name an expression can't take", () => W.addField(ACTION, "outputs", "a b", "x"));

// What isn't edited here.
refused("a flow mapping of jobs", () => W.addJob("on: push\njobs: { a: { runs-on: x, steps: [{ run: y }] } }\n", "b", "x"));
refused("text with errors", () => W.addJob("jobs:\n  a: [\n", "b", "x"));

console.log(failed ? `${failed} failed` : "all passed");
process.exit(failed ? 1 : 0);

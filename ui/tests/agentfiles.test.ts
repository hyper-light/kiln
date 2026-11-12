// Agentfiles in the console (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 31): what a start can be
// made from and how it's kept per harness and project, the spec that names it, and what Shards said,
// in words and on its lines.
import { test } from "node:test";
import assert from "node:assert/strict";
import type { AgentfileCheck, AgentfileDiagnostic, AgentfileListing, AgentfileSaved } from "../src/api/wire/index.ts";
import { builtinImage, choicesFor, chosenFor, keptImage, projectOf, whyNoMicrovm, withChoice } from "../src/views/agentfiles/choose.ts";
import { copyName, named } from "../src/views/agentfiles/names.ts";
import { checkWords, keptWords, marks, refused, restoredWords } from "../src/views/agentfiles/lint.ts";
import { specOf } from "../src/views/harness/spawn-setup.ts";

const listing: Pick<AgentfileListing, "agentfiles" | "builtin"> = {
  agentfiles: [
    { id: "01a1aaaaaaaaaaaaaaaaaaaaaaaaaaaa", owner: "owner", name: "claude-rust", version: 3, created_at: 1, updated_at: 3 },
    { id: "01a1bbbbbbbbbbbbbbbbbbbbbbbbbbbb", owner: "owner", name: "codex-go", version: 1, created_at: 1, updated_at: 2 },
  ],
  builtin: [
    { name: "harness-goose", harness: "goose", text: "FROM alpine\n" },
    { name: "harness-claude", harness: "claude", text: "FROM debian\n" },
  ],
};

test("step31: a start chooses from kiln's Agentfile for its harness, then yours, each named as the start names it", () => {
  assert.deepEqual(choicesFor(listing, "claude"), [
    { image: "harness-claude", name: "harness-claude", whose: "kiln" },
    { image: "agentfile:01a1aaaaaaaaaaaaaaaaaaaaaaaaaaaa", name: "claude-rust", whose: "yours" },
    { image: "agentfile:01a1bbbbbbbbbbbbbbbbbbbbbbbbbbbb", name: "codex-go", whose: "yours" },
  ]);
  // A harness kiln ships none for: yours alone.
  assert.deepEqual(
    choicesFor(listing, "codex").map((c) => c.whose),
    ["yours", "yours"],
  );
  // kiln's by name (an image's, never with a `:`), yours by id: one of yours named as kiln's is still yours.
  assert.equal(builtinImage({ name: "harness-claude" }), "harness-claude");
  assert.equal(keptImage("01a1aaaaaaaaaaaaaaaaaaaaaaaaaaaa"), "agentfile:01a1aaaaaaaaaaaaaaaaaaaaaaaaaaaa");
});

test("step31: the choice is kept per harness and per project, and one no longer there is none", () => {
  const choices = choicesFor(listing, "claude");
  let kept = withChoice({}, "claude", "~/src/kiln", "agentfile:01a1aaaaaaaaaaaaaaaaaaaaaaaaaaaa");
  kept = withChoice(kept, "codex", "~/src/kiln", "agentfile:01a1bbbbbbbbbbbbbbbbbbbbbbbbbbbb");
  kept = withChoice(kept, "claude", "~/src/shards", "harness-claude");
  assert.equal(chosenFor(kept, choices, "claude", "~/src/kiln")?.name, "claude-rust");
  assert.equal(chosenFor(kept, choices, "claude", "~/src/shards")?.name, "harness-claude");
  assert.equal(chosenFor(kept, choicesFor(listing, "codex"), "codex", "~/src/kiln")?.name, "codex-go");
  // Another project, or another harness there: nothing chosen yet, so a process on its machine.
  assert.equal(chosenFor(kept, choices, "claude", "~/src/other"), null);
  assert.equal(chosenFor(kept, choicesFor(listing, "goose"), "goose", "~/src/kiln"), null);
  // Deleted since it was chosen: none, never a name that boots nothing.
  assert.equal(chosenFor(kept, choicesFor({ ...listing, agentfiles: [] }, "claude"), "claude", "~/src/kiln"), null);
  // None again: only that harness's in that project goes.
  const none = withChoice(kept, "claude", "~/src/kiln", null);
  assert.equal(chosenFor(none, choices, "claude", "~/src/kiln"), null);
  assert.equal(chosenFor(none, choices, "claude", "~/src/shards")?.name, "harness-claude");
});

test("step31: a project is the folder it starts in, as typed; its home and a shell's folder are none", () => {
  assert.equal(projectOf(" ~/src/kiln/ ", ""), "~/src/kiln");
  assert.equal(projectOf("/srv/app//", ""), "/srv/app");
  assert.equal(projectOf("/", ""), "/");
  assert.equal(projectOf("~", ""), "");
  assert.equal(projectOf("~/", ""), "");
  assert.equal(projectOf("~/src/kiln", "t1"), "");
});

test("step31: the spec names its Agentfile as a microVM's image, and one without starts as a process", () => {
  const base = { kind: "claude", cwd: "~/src/kiln", shell: "", mode: "", model: "", effort: "", speed: "", name: "", prompt: "go", reach: "", volume: false };
  assert.deepEqual(specOf({ ...base, agentfile: "harness-claude" }).isolation, { kind: "microvm", image: "harness-claude" });
  assert.equal("isolation" in specOf(base), false);
  assert.equal("isolation" in specOf({ ...base, agentfile: "" }), false);
});

test("step31: where its machine would refuse a microVM, the start says why, as the server does", () => {
  const facts = (microvm: boolean) => ({ virt: { kvm: true, hvf: false, nested: false, microvm } }) as NonNullable<Parameters<typeof whyNoMicrovm>[1][number]["facts"]>;
  const machines = [
    { name: "vm-box", facts: facts(true) },
    { name: "plain", facts: facts(false) },
    { name: "quiet", facts: undefined },
  ];
  assert.equal(whyNoMicrovm("agent:vm-box", machines, "vm-box"), null);
  assert.equal(whyNoMicrovm("agent:plain", machines, "plain"), "plain doesn't start microVMs");
  assert.equal(whyNoMicrovm("agent:quiet", machines, "quiet"), "quiet hasn't said yet whether it starts microVMs");
  assert.equal(whyNoMicrovm("agent:gone", machines, "gone"), "gone hasn't said yet whether it starts microVMs");
  assert.equal(whyNoMicrovm("local", machines, "this server"), "kiln's server starts no microVMs itself");
});

test("step31: a name is an image's, and a copy's is its own, else the first number free", () => {
  for (const n of ["claude-rust", "a.b_c__d---e", "x1"]) assert.equal(named(n), true, n);
  for (const n of ["", "Claude", "-x", "x-", "a..b", "a_.b", "a/b", "x".repeat(256)]) assert.equal(named(n), false, n);
  assert.equal(copyName("harness-claude", ["claude-rust"]), "harness-claude");
  assert.equal(copyName("harness-claude", ["harness-claude"]), "harness-claude-2");
  assert.equal(copyName("harness-claude", ["harness-claude", "harness-claude-2", "harness-claude-4"]), "harness-claude-3");
});

const diag = (over: Partial<AgentfileDiagnostic>): AgentfileDiagnostic => ({
  code: "StageNameCasing",
  severity: "warning",
  message: "Stage name 'Build' should be lowercase",
  path: "Agentfile",
  range: { start: { line: 2, col: 1 }, end: { line: 2, col: 1 } },
  ...over,
});
const check = (over: Partial<AgentfileCheck>): AgentfileCheck => ({ checked: true, diagnostics: [], ...over });

test("step31: what Shards said, in words: its problems counted, or why it didn't look", () => {
  assert.equal(checkWords(check({})), "Shards finds no problems.");
  assert.equal(checkWords(check({ diagnostics: [diag({}), diag({ severity: "error", code: "shards" })] })), "1 error, 1 warning.");
  assert.equal(checkWords(check({ diagnostics: [diag({}), diag({})] })), "2 warnings.");
  assert.equal(checkWords(check({ checked: false, unchecked: { reason: "no_shards" } })), "Not checked: this server has no Shards to check it with.");
  assert.equal(checkWords(check({ checked: false, unchecked: { reason: "timed_out" } })), "Not checked: Shards didn't answer in time.");
  assert.equal(checkWords(check({ checked: false, unchecked: { reason: "failed", status: 1, said: "no such file" } })), "Not checked: Shards couldn't check it: no such file.");
  assert.equal(refused(check({ diagnostics: [diag({})] })), false);
  assert.equal(refused(check({ diagnostics: [diag({ severity: "error" })] })), true);
});

test("step31: each problem marks its whole lines, the error that refuses it in Shards' own words", () => {
  const text = "FROM alpine AS base\nFROM base AS Build\nFORM x\n";
  const [warning, error, past] = marks(text, [
    diag({}),
    diag({ code: "shards", severity: "error", message: "unknown instruction: FORM", range: { start: { line: 3, col: 1 }, end: { line: 3, col: 1 } } }),
    diag({ range: { start: { line: 9, col: 1 }, end: { line: 9, col: 1 } } }),
  ]);
  assert.deepEqual(warning, { from: 20, to: 38, severity: "warning", message: "StageNameCasing: Stage name 'Build' should be lowercase" });
  assert.equal(text.slice(warning?.from, warning?.to), "FROM base AS Build");
  assert.deepEqual(error, { from: 39, to: 45, severity: "error", message: "unknown instruction: FORM" });
  // A line past the text's end: its last line (here empty, after the final newline), never outside it.
  assert.ok(past && past.from <= text.length && past.to <= text.length);
  // Several lines: from the first one's start to the last one's end.
  const [both] = marks(text, [diag({ range: { start: { line: 1, col: 1 }, end: { line: 2, col: 1 } } })]);
  assert.equal(text.slice(both?.from, both?.to), "FROM alpine AS base\nFROM base AS Build");
});

test("step31: a save says the version it kept, a rename that it's none, a restore which it made current", () => {
  const saved = (version: number, c: AgentfileCheck = check({})): AgentfileSaved => ({
    agentfile: { id: "a", owner: "owner", name: "claude-rust", text: "FROM alpine\n", version, created_at: 1, updated_at: 2 },
    check: c,
  });
  assert.equal(keptWords(saved(4), 3), "Kept as v4. Shards finds no problems.");
  assert.equal(keptWords(saved(4, check({ checked: false, unchecked: { reason: "no_shards" } })), 3), "Kept as v4. Not checked: this server has no Shards to check it with.");
  assert.equal(keptWords(saved(3), 3), "Renamed: a new name alone is no new version.");
  assert.equal(restoredWords(saved(5), 2), "v2's text is current again, as v5. Shards finds no problems.");
});

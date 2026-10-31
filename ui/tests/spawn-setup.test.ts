import { test } from "node:test";
import assert from "node:assert/strict";
import type { KindInfo } from "../src/api/harness.ts";
import { carryEffort, carryMode, harnessesOf, kindOf, machinesWith, modeOf, modelOf, oneOf, reachChoices, specOf, speedsFor, targetOf, withDir } from "../src/views/harness/spawn-setup.ts";

const claude = (over: Partial<KindInfo> = {}): KindInfo => ({ id: "claude", name: "Claude Code", verified: true, available: true, ...over });
const codex = (over: Partial<KindInfo> = {}): KindInfo => ({ id: "codex", name: "Codex", verified: true, available: true, ...over });

test("every harness once, installed somewhere wins over missing", () => {
  const all = harnessesOf({ "agent:a": [claude({ available: false }), codex()], "agent:b": [claude()] });
  assert.deepEqual(
    all.map((k) => [k.id, k.available]),
    [
      ["claude", true],
      ["codex", true],
    ],
  );
});

test("a harness no machine has gives way to one that some machine has", () => {
  const all = harnessesOf({ "agent:a": [claude({ available: false }), codex()] });
  assert.equal(kindOf(all, "claude"), "codex");
  assert.equal(kindOf(all, "codex"), "codex");
});

test("the machine: picked, then handed over, then used last, else the first that has it", () => {
  const where = machinesWith({ "agent:a": [claude()], "agent:b": [claude()], "agent:c": [codex()] }, "claude");
  assert.deepEqual(where, ["agent:a", "agent:b"]);
  assert.equal(targetOf(where, [null, "agent:b", "agent:a"]), "agent:b");
  assert.equal(targetOf(where, ["agent:c", undefined, ""]), "agent:a");
  assert.equal(targetOf([], ["agent:a"]), "");
});

test("a mode that turns checks off only when confirmed for this start, never remembered", () => {
  const modes = [
    { id: "default", label: "Ask", description: "" },
    { id: "acceptEdits", label: "Edits", description: "" },
    { id: "bypassPermissions", label: "Bypass", description: "", dangerous: true },
  ];
  assert.equal(modeOf(modes, { unchecked: "bypassPermissions" }), "bypassPermissions");
  assert.equal(modeOf(modes, { unchecked: "", remembered: "bypassPermissions", fallback: "default" }), "default");
  assert.equal(modeOf(modes, { unchecked: "", remembered: "acceptEdits" }), "acceptEdits");
  assert.equal(modeOf(modes, { unchecked: "" }), "default");
});

test("the model chosen if offered, its default shown otherwise; the speeds go with the model", () => {
  const k = claude({
    models: [
      { id: "opus", label: "Opus", default: true, speeds: [{ id: "fast", label: "Fast" }] },
      { id: "sonnet", label: "Sonnet" },
    ],
  });
  assert.deepEqual(modelOf(k, "sonnet"), { chosen: "sonnet", shown: "sonnet" });
  assert.deepEqual(modelOf(k, "gone"), { chosen: "", shown: "opus" });
  assert.deepEqual(
    speedsFor(k, "").map((s) => s.id),
    ["fast"],
  );
  assert.deepEqual(speedsFor(k, "sonnet"), []);
  assert.equal(oneOf(speedsFor(k, ""), "fast"), "fast");
  assert.equal(oneOf(speedsFor(k, "sonnet"), "fast"), "");
});

test("the spec: a shell's folder is its own, home is no folder, a volume only over a folder", () => {
  const base = { kind: "claude", cwd: "~/src/kiln", shell: "", mode: "default", model: "", effort: "", speed: "fast", name: " ", prompt: " fix it ", reach: "", volume: true };
  assert.deepEqual(specOf(base), {
    kind: "claude",
    cwd: "~/src/kiln",
    mode: "default",
    model: undefined,
    effort: undefined,
    speed: "fast",
    name: undefined,
    prompt: "fix it",
    shell: undefined,
    reach: undefined,
    workspace: { kind: "volume" },
  });
  const inShell = specOf({ ...base, shell: "t1" });
  assert.equal(inShell.cwd, undefined);
  assert.equal(inShell.workspace, undefined);
  assert.equal(specOf({ ...base, cwd: "~" }).cwd, undefined);
});

test("recent folders: the latest first, eight at most, home left out", () => {
  const dirs = { "agent:a": ["~/x", "~/y"] };
  assert.deepEqual(withDir(dirs, "agent:a", "~/y"), { "agent:a": ["~/y", "~/x"] });
  assert.equal(withDir(dirs, "agent:a", "~"), dirs);
  assert.equal(withDir({ t: Array.from({ length: 8 }, (_, i) => `~/d${i}`) }, "t", "~/new").t?.length, 8);
});

test("where its commands can run: other runners and their platforms", () => {
  const runners = [
    { name: "a", os: "linux", arch: "x86_64", connected: true, reach: true },
    { name: "b", os: "macos", arch: "aarch64", connected: true, reach: true },
  ] as Parameters<typeof reachChoices>[0];
  assert.deepEqual(
    reachChoices(runners, "agent:a").map((o) => o.value),
    ["macos/aarch64", "b"],
  );
});

test("the machine decides the agents: those it runs, its models else another machine's list of them", async () => {
  const { machinesWithAny, onMachine } = await import("../src/views/harness/spawn-setup.ts");
  const sol = { id: "gpt-6.1-sol", label: "GPT-6.1 Sol", default: true };
  const byTarget = {
    "agent:a": [claude(), codex({ models: [] })],
    "agent:b": [codex({ models: [sol] })],
    "agent:c": [claude({ available: false })],
  };
  assert.deepEqual(machinesWithAny(byTarget), ["agent:a", "agent:b"]);
  // a has Codex but never listed its models: b's list stands in.
  assert.deepEqual(onMachine(byTarget, "agent:a", "codex")?.models, [sol]);
  assert.deepEqual(onMachine(byTarget, "agent:b", "codex")?.models, [sol]);
  assert.equal(onMachine(byTarget, "agent:b", "claude"), undefined);
});

test("a machine in the slot, short: no domain, its host's name only when that's another", async () => {
  const { shortMachineLabel } = await import("../src/views/harness/spawn-setup.ts");
  const machines = [
    { name: "kiln-a094a12d.attlocal.net", hostname: "fedora-2.attlocal.net" },
    { name: "adas-mac-studio-3.local", hostname: "Adas-Mac-Studio-3.local" },
    { name: "adacachy" },
  ] as Parameters<typeof shortMachineLabel>[1];
  assert.equal(shortMachineLabel("agent:kiln-a094a12d.attlocal.net", machines), "kiln-a094a12d (fedora-2)");
  assert.equal(shortMachineLabel("agent:adas-mac-studio-3.local", machines), "adas-mac-studio-3");
  assert.equal(shortMachineLabel("agent:adacachy", machines), "adacachy");
  assert.equal(shortMachineLabel("local", machines), "kiln server");
});

test("fast mode: as the model lists it; a machine that lists no model's speeds gives Claude Code's Opus it", () => {
  const fast = { id: "fast", label: "Fast", description: "" };
  const old = claude({
    models: [
      { id: "default", label: "Default (recommended)", description: "Opus 5.5 · Best for everyday, complex tasks" },
      { id: "opus", label: "Opus 5.5" },
      { id: "sonnet", label: "Sonnet 5.5" },
    ],
  });
  assert.deepEqual(
    speedsFor(old, "").map((s) => s.id),
    ["fast"],
  );
  assert.deepEqual(
    speedsFor(old, "opus").map((s) => s.id),
    ["fast"],
  );
  assert.deepEqual(speedsFor(old, "sonnet"), []);
  // A machine that lists speeds says which models have them: none else.
  const listed = claude({
    models: [
      { id: "default", label: "Default", description: "Opus 5.5 · …", speeds: [] },
      { id: "opus", label: "Opus 5.5", speeds: [fast] },
    ],
  });
  assert.deepEqual(speedsFor(listed, "opus"), [fast]);
  assert.deepEqual(speedsFor(listed, ""), []);
  // Another harness gets none it doesn't list.
  assert.deepEqual(speedsFor(codex({ models: [{ id: "gpt-6.1-sol", label: "GPT-6.1 Sol", default: true }] }), ""), []);
});

// The modes each harness offers, as their runners list them.
const CLAUDE = ["default", "acceptEdits", "plan", "auto", "dontAsk", "bypassPermissions"];
const CODEX = ["auto", "read-only", "untrusted", "full-access"];
const DEVIN = ["accept-edits", "smart", "ask", "plan", "bypass"];
const AGY = ["default", "accept-edits", "plan", "bypass"];

test("a harness switch carries the permissions at the same latitude, never more, as an agent's switch does (handoff.rs)", () => {
  // Read only stays read only; ask stays ask; Claude Code's accept edits is Codex's auto, and back.
  assert.equal(carryMode("claude", "plan", "codex", CODEX), "read-only");
  assert.equal(carryMode("claude", "dontAsk", "codex", CODEX), "read-only");
  assert.equal(carryMode("claude", "default", "codex", CODEX), "untrusted");
  assert.equal(carryMode("claude", "acceptEdits", "codex", CODEX), "auto");
  assert.equal(carryMode("codex", "auto", "claude", CLAUDE), "acceptEdits");
  // Codex has nothing at Claude Code's auto: the next one down.
  assert.equal(carryMode("claude", "auto", "codex", CODEX), "auto");
  // Skipping every check carries only where it's offered; else the most there is below it.
  assert.equal(carryMode("codex", "full-access", "claude", CLAUDE), "bypassPermissions");
  assert.equal(
    carryMode(
      "codex",
      "full-access",
      "claude",
      CLAUDE.filter((m) => m !== "bypassPermissions"),
    ),
    "auto",
  );
  // Into Devin and Antigravity by their own modes; out of them as well.
  assert.equal(carryMode("claude", "auto", "devin", DEVIN), "smart");
  assert.equal(carryMode("devin", "smart", "antigravity", AGY), "accept-edits");
  assert.equal(carryMode("devin", "plan", "claude", CLAUDE), "plan");
  // Nothing that low there (an agent that names no modes before it starts): its own default.
  assert.equal(carryMode("claude", "plan", "gemini", []), "");
  // A mode kiln doesn't know asks; in its own harness a mode is itself.
  assert.equal(carryMode("gemini", "", "codex", CODEX), "untrusted");
  assert.equal(carryMode("claude", "dontAsk", "claude", CLAUDE), "dontAsk");
  assert.equal(
    carryMode(
      "claude",
      "bypassPermissions",
      "claude",
      CLAUDE.filter((m) => m !== "bypassPermissions"),
    ),
    "auto",
  );
});

test("a harness switch carries the effort to the nearest level the model takes (handoff.rs)", () => {
  const all = ["low", "medium", "high", "xhigh", "max", "ultra"];
  assert.equal(carryEffort("max", all), "max");
  assert.equal(carryEffort("max", ["low", "medium", "high", "xhigh"]), "xhigh");
  assert.equal(carryEffort("ultra", ["low", "medium", "high", "xhigh", "max"]), "max");
  assert.equal(carryEffort("minimal", ["low", "high"]), "low");
  // The lower of two as near; none chosen is none; none to take is none.
  assert.equal(carryEffort("xhigh", ["high", "max"]), "high");
  assert.equal(carryEffort("", ["low"]), "");
  assert.equal(carryEffort("max", []), "");
  assert.equal(carryEffort("thorough", ["low", "high"]), "");
});

test("a mode carried from another harness that turns checks off still waits for a confirmation for this start", () => {
  const codex = [
    { id: "auto", label: "Auto", description: "" },
    { id: "untrusted", label: "Ask", description: "" },
    { id: "full-access", label: "Full access", description: "", dangerous: true },
  ];
  const ids = codex.map((m) => m.id);
  // Confirmed for this start in Claude Code: confirmed in Codex too.
  assert.equal(modeOf(codex, { unchecked: carryMode("claude", "bypassPermissions", "codex", ids) }), "full-access");
  // Kept (never a mode that turns checks off) or handed over: never carried into one.
  assert.equal(modeOf(codex, { unchecked: "", remembered: carryMode("claude", "bypassPermissions", "codex", ids), fallback: "auto" }), "auto");
  assert.equal(modeOf(codex, { unchecked: "", remembered: carryMode("claude", "default", "codex", ids), fallback: "auto" }), "untrusted");
});

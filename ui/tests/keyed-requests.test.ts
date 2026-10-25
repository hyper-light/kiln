import assert from "node:assert/strict";
import { readFileSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";
import { test } from "node:test";

// CP-T3.2 (the ninth adversarial check's): every request of the UI's that reaches an agent as a
// prompt carries an idempotency key, kept across a retry (`Tries`): asked again after its answer was
// lost, it's the same request, never a second prompt. Each call names its key as its last argument:
// `plansApi.run(id, target, mode, pick, key)`, `plansApi.runAgents(id, to, pick, key)`,
// `harnessApi.addGoals(id, goals, start, key)` and (the twelfth's: a resume or a skip sends the next
// goal) `harnessApi.goalAction(id, action, goal, key)`.
const KEYED: [string, number][] = [
  ["plansApi.run", 5],
  ["plansApi.runAgents", 4],
  ["harnessApi.addGoals", 4],
  ["harnessApi.goalAction", 4],
];

function sources(dir: string): string[] {
  return readdirSync(dir).flatMap((n) => {
    const p = join(dir, n);
    return statSync(p).isDirectory() ? sources(p) : /\.tsx?$/.test(n) ? [p] : [];
  });
}

/** The arguments of each call of `name(` in `text`, split at the top level. */
function calls(text: string, name: string): string[][] {
  const out: string[][] = [];
  const at = (from: number) => {
    // `name(`, not a longer name ending in it (`plansApi.runAgents(` isn't `plansApi.run(`).
    let i = text.indexOf(name + "(", from);
    while (i > 0 && /[\w.]/.test(text[i - 1]!)) i = text.indexOf(name + "(", i + 1);
    return i;
  };
  let start = at(0);
  while (start >= 0) {
    let i = start + name.length + 1;
    let depth = 0;
    let arg = "";
    const args: string[] = [];
    for (; i < text.length; i++) {
      const c = text[i]!;
      if ("([{".includes(c)) depth++;
      if (")]}".includes(c)) {
        if (depth === 0) break;
        depth--;
      }
      if (c === "," && depth === 0) {
        args.push(arg.trim());
        arg = "";
      } else arg += c;
    }
    if (arg.trim()) args.push(arg.trim());
    out.push(args);
    start = at(i);
  }
  return out;
}

/** Whether `call`, of a request whose key is its argument `args`, carries one: not left out, nor said to be none (the thirteenth adversarial check's). */
function keyed(call: string[], args: number): boolean {
  const key = call[args - 1];
  return call.length >= args && key !== undefined && !/^(undefined|null|""|''|``)$/.test(key);
}

test("every request the UI makes that reaches an agent as a prompt carries a key", () => {
  const unkeyed: string[] = [];
  const files = sources(join(import.meta.dirname, "..", "src")).filter((f) => !f.includes(join("src", "api") + "/"));
  let seen = 0;
  for (const file of files) {
    const text = readFileSync(file, "utf8");
    for (const [name, args] of KEYED) {
      for (const call of calls(text, name)) {
        seen++;
        if (!keyed(call, args)) unkeyed.push(`${file}: ${name}(${call.join(", ")})`);
      }
    }
  }
  assert.ok(seen >= 7, `the calls were found (${seen})`);
  assert.deepEqual(unkeyed, [], "sent without a key: asked again after a lost answer, each is a second prompt");
});

test("a key left out, or said to be none, is no key", () => {
  const call = (key: string) => calls(`harnessApi.goalAction(id, "start", undefined${key})`, "harnessApi.goalAction")[0] ?? [];
  for (const none of ["", ", undefined", ", null", ', ""']) assert.equal(keyed(call(none), 4), false, `goalAction(id, "start", undefined${none})`);
  assert.equal(keyed(call(", tries.keyFor(id)"), 4), true);
});

test("a call's arguments are read at the top level, and a longer name isn't the shorter", () => {
  assert.deepEqual(calls("plansApi.run(a, { b, c }, f(d, e), {}, key)", "plansApi.run"), [["a", "{ b, c }", "f(d, e)", "{}", "key"]]);
  assert.deepEqual(calls("plansApi.runAgents(a, b, {}, key)", "plansApi.run"), []);
});

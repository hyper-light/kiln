import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { test } from "node:test";
import ts from "typescript";

// W29-R27: code workflows type-check against @kiln/sdk's declarations, the copy kiln-ci holds to its
// generator (crates/kiln/tests/fixtures/code/kiln.d.ts), and what's written wrong is refused at its line.
const dir = join(import.meta.dirname, "..", "..", "crates", "kiln", "tests", "fixtures", "code");

function problems(file: string): number[] {
  const options = { strict: true, noEmit: true, target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext, moduleResolution: ts.ModuleResolutionKind.Bundler, types: [], lib: ["lib.es2022.d.ts"] };
  const program = ts.createProgram([join(dir, "kiln.d.ts"), file], options);
  return ts.getPreEmitDiagnostics(program).map((d) => (d.file && d.start !== undefined ? d.file.getLineAndCharacterOfPosition(d.start).line + 1 : 0));
}

const programs = (sub: string) =>
  readdirSync(join(dir, sub))
    .filter((f) => f.endsWith(".ts") && !f.endsWith(".d.ts"))
    .map((f) => join(dir, sub, f));

test("W29-R27: code workflows type-check against @kiln/sdk", () => {
  const right = [...programs("."), ...programs("types").filter((f) => !f.includes("wrong-"))];
  assert.ok(right.length >= 1);
  for (const f of right) assert.deepEqual(problems(f), [], f);
});

test("W29-R27: @kiln/sdk refuses a misspelled key, a wrong type and an unknown job key, each at its line", () => {
  const wrong = programs("types").filter((f) => f.includes("wrong-"));
  assert.equal(wrong.length, 3);
  for (const f of wrong) {
    const at = Number(/refused at (\d+)/.exec(readFileSync(f, "utf8"))?.[1]);
    assert.deepEqual(problems(f), [at], f);
  }
});

// The workflow editor's patch (ui/src/lib/patch.ts, docs/ci.md W21-R06): each case's patch
// applies with `git apply` and gives the new text, missing final newlines included.
//
//     bun ci/ui/patch_check.ts
import { unifiedPatch } from "../../ui/src/lib/patch.ts";
import { execFileSync } from "node:child_process";
import { mkdtempSync, writeFileSync, readFileSync, mkdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const cases: [string, string, string][] = [
  ["middle", "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n", "a\nb\nc\nD\ne\nf\ng\nh\ni\nj\n"],
  ["top", "x\ny\n", "new\nx\ny\n"],
  ["end", "x\ny\nz\n", "x\ny\n"],
  ["two hunks", Array.from({ length: 30 }, (_, i) => `l${i}`).join("\n") + "\n", Array.from({ length: 30 }, (_, i) => (i === 2 || i === 25 ? `L${i}` : `l${i}`)).join("\n") + "\n"],
  ["no eol before", "a\nb", "a\nb\nc"],
  ["no eol after", "a\nb\n", "a\nb"],
  ["both no eol", "a\nb", "a\nc"],
  ["empty to text", "", "one\ntwo\n"],
  ["text to empty", "one\ntwo\n", ""],
  [
    "yaml",
    "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: make\n",
    "on: [push, pull_request]\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - run: make test\n",
  ],
];
let failed = 0;
for (const [name, before, after] of cases) {
  const dir = mkdtempSync(join(tmpdir(), "kiln-patch-"));
  const env = { ...process.env, GIT_CONFIG_GLOBAL: "/dev/null", GIT_CONFIG_NOSYSTEM: "1" };
  execFileSync("git", ["init", "-q", dir], { env });
  mkdirSync(join(dir, ".github/workflows"), { recursive: true });
  const rel = ".github/workflows/ci.yml";
  writeFileSync(join(dir, rel), before);
  const patch = unifiedPatch(rel, before, after);
  writeFileSync(join(dir, "p.patch"), patch);
  try {
    execFileSync("git", ["-C", dir, "apply", "p.patch"], { env, stdio: "pipe" });
    const got = readFileSync(join(dir, rel), "utf8");
    if (got !== after) throw new Error(`gave ${JSON.stringify(got)}`);
    console.log(`ok   ${name}`);
  } catch (e) {
    failed++;
    console.log(`FAIL ${name}: ${String(e).split("\n")[0]}\n${patch}`);
  }
}
console.log(unifiedPatch("x", "same\n", "same\n") === "" ? "ok   unchanged is empty" : "FAIL unchanged");
process.exit(failed ? 1 : 0);

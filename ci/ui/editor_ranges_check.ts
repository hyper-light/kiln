// W21-AC02's first part (docs/ci.md): the editor flags every negative-corpus file (F-04,
// crates/kiln-ci/tests/fixtures/negative) at the right range. Each file goes to the server as the
// editor sends it (POST /api/ci/validate): a workflow of either dialect, an action, or an overlay
// (with its repository, whose workflow it overlays: the one wf_negative.rs applies it to), as
// wf_negative.rs reads them. Its diagnostics are placed in the text as the editor places them
// (ui/src/lib/diagnostics.ts): the code expected must start exactly at its line and column (a mark
// at the file's very end has no width, as CodeMirror shows one).
//
//     SIGNIN="$(cat <signin_setup.py's output>)" bun ci/ui/editor_ranges_check.ts
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { placed } from "../../ui/src/lib/diagnostics.ts";

const cfg = JSON.parse(process.env.SIGNIN ?? "{}") as { base: string; admin: string };
const dir = join(import.meta.dir, "../../crates/kiln-ci/tests/fixtures/negative");
const expected = readFileSync(join(dir, "expect.txt"), "utf8")
  .split("\n")
  .filter((l) => l.trim() && !l.startsWith("#"))
  .map((l) => {
    const [file, code, at] = l.trim().split(/\s+/);
    const [line, col] = (at ?? "0:0").split(":").map(Number);
    return { file: file ?? "", code: code ?? "", line: line ?? 0, col: col ?? 0 };
  });

/** Where line `line`, column `col` (both from 1) is in `text`. */
function offset(text: string, line: number, col: number): number {
  let at = 0;
  for (let l = 1; l < line; l++) at = text.indexOf("\n", at) + 1;
  return at + col - 1;
}

const api = (method: string, path: string, body?: unknown) =>
  fetch(`${cfg.base}${path}`, { method, headers: { Authorization: `Bearer ${cfg.admin}`, "Content-Type": "application/json" }, body: body === undefined ? undefined : JSON.stringify(body) });

// The repository an overlay is checked against: the workflow wf_negative.rs overlays.
const OVERLAID = "on: push\njobs:\n  a:\n    runs-on: x\n    steps: [{run: echo}]\n";
await api("POST", "/api/repos", { name: "acme/overlaid", mode: "native" });
const src = mkdtempSync(join(tmpdir(), "overlaid-"));
mkdirSync(join(src, ".github", "workflows"), { recursive: true });
writeFileSync(join(src, ".github", "workflows", "x.yml"), OVERLAID);
const env = { ...process.env, GIT_CONFIG_GLOBAL: "/dev/null", GIT_CONFIG_NOSYSTEM: "1", GIT_AUTHOR_NAME: "t", GIT_AUTHOR_EMAIL: "t@example.invalid", GIT_COMMITTER_NAME: "t", GIT_COMMITTER_EMAIL: "t@example.invalid" };
for (const args of [["init", "-q", "-b", "main"], ["add", "."], ["commit", "-qm", "overlaid"], ["push", "-q", `http://x:${cfg.admin}@${cfg.base.split("://")[1]}/git/acme/overlaid.git`, "main"]]) {
  const p = Bun.spawnSync(["git", "-C", src, ...args], { env });
  if (p.exitCode !== 0) throw new Error(`git ${args.join(" ")}: ${p.stderr.toString()}`);
}

/** Where the editor would have the file, and what it sends with it. */
function asSent(file: string): { path: string; repo?: string } {
  if (file.endsWith(".action.yml")) return { path: ".github/actions/negative/action.yml" };
  if (file.endsWith(".overlay.yaml")) return { path: ".kiln/overlays/x.yaml", repo: "acme/overlaid" };
  return { path: file.includes(".kiln.") ? `.kiln/${file}` : `.github/workflows/${file}` };
}

let failed = 0;
for (const e of expected) {
  const content = readFileSync(join(dir, e.file), "utf8");
  const r = await api("POST", "/api/ci/validate", { ...asSent(e.file), content });
  const checked = (await r.json()) as { diagnostics: Parameters<typeof placed>[1] };
  const marks = placed(content, checked.diagnostics ?? []);
  const want = offset(content, e.line, e.col);
  const hit = marks.find((m) => m.message.startsWith(`${e.code}:`) && m.from === want && (m.to > m.from || m.from === content.length));
  const near = marks.filter((m) => m.message.startsWith(`${e.code}:`)).map((m) => `${m.from}-${m.to}`);
  console.log(`${hit ? "ok  " : "FAIL"} ${e.file}: ${e.code} at ${e.line}:${e.col}${hit ? "" : ` (offset ${want}; ${e.code} marked at ${near.join(", ") || "nowhere"})`}`);
  if (!hit) failed++;
}
console.log(failed ? `${failed} of ${expected.length} misplaced` : `all ${expected.length} at their places`);
process.exit(failed ? 1 : 0);

// W29-AC25: kiln's own workflows (.kiln/*.yaml, copied beside this at the commit the fixtures were
// made at) written as TypeScript programs, each the YAML's workflow as one object for `workflow()`.
// ci.ts is written by hand, idiomatically, and isn't remade. Run from ui/, whose `yaml` it uses:
// node ../crates/kiln/tests/fixtures/code/kiln/translate.mjs
import { readFileSync, readdirSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const { parse } = createRequire(join(process.cwd(), "package.json"))("yaml");

const here = dirname(fileURLToPath(import.meta.url));
for (const file of readdirSync(here).filter((f) => f.endsWith(".yaml") && f !== "ci.yaml").sort()) {
  const name = file.replace(/\.yaml$/, "");
  const w = parse(readFileSync(join(here, file), "utf8"));
  // W02's shorthands, written as the objects the SDK's types (the kiln dialect's schema) say: an
  // event that's just on (`manual: true`), and a schedule's bare crons.
  for (const [k, v] of Object.entries(w.on ?? {})) if (v === true) w.on[k] = {};
  if (Array.isArray(w.on?.schedule)) w.on.schedule = w.on.schedule.map((s) => (typeof s === "string" ? { cron: s } : s));
  const body = JSON.stringify(w, null, 2);
  writeFileSync(join(here, `${name}.ts`), `// kiln's own .kiln/${file} in TypeScript (docs/ci.md W29-AC25), written by translate.mjs.\nimport { workflow } from "@kiln/sdk";\n\nexport default workflow(${body});\n`);
}

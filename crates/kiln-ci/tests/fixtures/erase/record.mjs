// W29-R27: Node's own type stripping of each case under cases/, written to node.json beside it:
// what kiln's erasure (crates/kiln-ci/src/wf/code.rs) is held to, byte for byte. Run it with
// Node 24: `node record.mjs`.
import { stripTypeScriptTypes } from "node:module";
import { readdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const cases = {};
for (const name of readdirSync(join(here, "cases")).filter((f) => f.endsWith(".ts")).sort()) {
  const ts = readFileSync(join(here, "cases", name), "utf8");
  try {
    cases[name] = { js: stripTypeScriptTypes(ts, { mode: "strip" }) };
  } catch (e) {
    cases[name] = { refused: e.code, message: e.message };
  }
}
writeFileSync(join(here, "node.json"), JSON.stringify({ node: process.version, cases }, null, 2) + "\n");

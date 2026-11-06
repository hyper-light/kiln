// Bundles the shared socket's lab page and worker into OUT (argv[2]) for a lab server's KILN_LIVE_LAB.
import { build } from "esbuild";
import { writeFileSync } from "node:fs";
import { join } from "node:path";

const out = process.argv[2];
const here = new URL(".", import.meta.url).pathname;
// NODE_PATH: where react resolves from (the UI's node_modules), the lab page's one package.
const nodePaths = (process.env.NODE_PATH ?? "").split(":").filter(Boolean);
await build({
  entryPoints: [join(here, "lab.ts")],
  bundle: true,
  outfile: join(out, "lab.js"),
  format: "iife",
  target: "es2022",
  nodePaths,
  define: { "import.meta.url": '""', "process.env.NODE_ENV": '"production"' },
});
await build({ entryPoints: [join(here, "../../src/api/bp/worker.ts")], bundle: true, outfile: join(out, "worker.js"), format: "esm", target: "es2022" });
writeFileSync(
  join(out, "lab.html"),
  '<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width"><title>kiln-bp shared socket</title><body><script src="/__live/lab.js"></script></body>',
);
console.log(`built into ${out}`);

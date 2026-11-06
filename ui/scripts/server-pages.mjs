#!/usr/bin/env node
/**
 * Lists where the pages rendered on the server start, for kiln (goal 8, plan 01a1a832ebfd step 3):
 * `dist/client/_server-pages.json`, e.g. `["/shared/plans/"]`. Every signed-in page (under
 * routes/_app.tsx, `ssr: false`) renders only in the browser, so what the server sends for it is the
 * app's shell, which kiln sends itself, packed by its build and revalidated (crates/kiln/src/ui.rs); a
 * page of any other route goes to the UI server. A route's prefix is its path up to its first
 * parameter.
 *
 *   node scripts/server-pages.mjs
 */
import { readdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const UI = join(dirname(fileURLToPath(import.meta.url)), "..");

/** The path prefix of a flat route file (`shared.plans.$token.tsx` -> `/shared/plans/`). */
export function prefixOf(file) {
  const parts = file.replace(/\.tsx?$/, "").split(".");
  const fixed = [];
  for (const p of parts) {
    if (p.startsWith("$")) break;
    if (p === "index" || p === "route" || (p.startsWith("_") && !p.endsWith("_"))) continue;
    fixed.push(p.replace(/_$/, ""));
  }
  return `/${fixed.map((p) => `${p}/`).join("")}`;
}

/** Every route file outside the browser-only `_app` layout and the document (`__root`). */
export function serverPages(files) {
  const routes = files.filter((f) => /\.tsx?$/.test(f) && f !== "__root.tsx" && !/^_app(\.|$)/.test(f.replace(/\.tsx?$/, "")));
  return [...new Set(routes.map(prefixOf))].sort();
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const pages = serverPages(readdirSync(join(UI, "src", "routes")));
  writeFileSync(join(UI, "dist", "client", "_server-pages.json"), `${JSON.stringify(pages)}\n`);
  console.log(`server-pages: ${pages.join(" ") || "(none)"}`);
}

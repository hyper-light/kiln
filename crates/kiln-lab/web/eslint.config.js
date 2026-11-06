// The browser lab's JavaScript (docs/browser-lab.md, "Running it"): ESLint's recommended rules,
// Node's globals for the driver, the origin and their tests, a browser's for the probe page.
import js from "@eslint/js";
import globals from "globals";

export default [
  { ignores: ["node_modules/**"] },
  js.configs.recommended,
  { files: ["*.mjs", "test/*.mjs"], languageOptions: { globals: { ...globals.node } } },
  // The driver's and the recorder's page.evaluate callbacks run in the page.
  { files: ["job.mjs", "record.mjs"], languageOptions: { globals: { ...globals.node, ...globals.browser } } },
  { files: ["probe/*.js"], languageOptions: { sourceType: "script", globals: { ...globals.browser } } },
];

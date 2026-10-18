import js from "@eslint/js";
import globals from "globals";
import reactHooks from "eslint-plugin-react-hooks";
import tseslint from "typescript-eslint";

export default tseslint.config(
  { ignores: ["dist", "node_modules"] },
  {
    extends: [js.configs.recommended, ...tseslint.configs.recommended],
    files: ["**/*.{ts,tsx}"],
    languageOptions: {
      ecmaVersion: 2023,
      globals: { ...globals.browser, ...globals.node },
    },
    plugins: { "react-hooks": reactHooks },
    rules: {
      "react-hooks/rules-of-hooks": "error",
      "react-hooks/exhaustive-deps": "warn",
      "@typescript-eslint/no-unused-vars": ["warn", { argsIgnorePattern: "^_" }],
      // No effects in kiln's UI.
      "no-restricted-syntax": [
        "error",
        {
          selector: "CallExpression[callee.name=/^use(Layout)?Effect$/], CallExpression[callee.object.name='React'][callee.property.name=/^use(Layout)?Effect$/]",
          message:
            "No effects: load data with useSuspenseResource (lib/resource.ts) inside Loadable, subscribe to outside state with useSyncExternalStore or a zustand store, act in event handlers, and wire DOM objects with a ref callback (it can return its cleanup).",
        },
      ],
    },
  },
);

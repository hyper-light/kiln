// The eslint starter's configuration (it runs `eslint . --config .eslintrc.js`).
module.exports = {
  root: true,
  env: { node: true, browser: true, es2022: true },
  parserOptions: { ecmaVersion: 2022, sourceType: "module" },
  extends: "eslint:recommended",
  ignorePatterns: ["node_modules/", "deno/", "dist/", "target/", "build/", "_site/", "deps/", "_build/", "vendor/"],
};

/**
 * The browser protocol's own browser checks (e2e/bp-*.spec.ts): a blank page each, no server.
 * Chromium (the machine's Chrome unless `E2E_CHANNEL` names another) and WebKit.
 *
 *   npx playwright test -c playwright.bp.config.ts
 */
import { defineConfig, devices } from "@playwright/test";

const CHANNEL = process.env.E2E_CHANNEL ?? "chrome";

export default defineConfig({
  testDir: "e2e",
  testMatch: /bp-.*\.spec\.ts/,
  outputDir: process.env.E2E_OUT ?? "test-results",
  workers: 1,
  reporter: [["list"]],
  timeout: 120_000,
  projects: [
    { name: "chromium", use: { browserName: "chromium", channel: CHANNEL } },
    { name: "webkit", use: { browserName: "webkit" } },
    { name: "phone-webkit", use: { ...devices["iPhone 15"], browserName: "webkit" } },
  ],
});

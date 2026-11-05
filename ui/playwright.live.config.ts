/**
 * The shared protocol socket's browser checks (e2e/bp-shared.spec.ts, goal 3 of the browser protocol
 * program, step 6): against a lab server (`LIVE_ORIGIN`, its admin token `LIVE_TOKEN`; started with
 * `KILN_LIVE=1` and `KILN_LIVE_LAB` holding `e2e/bp-shared/build.mjs`'s bundles). Chromium and WebKit,
 * desktop (1280) and phone (390) widths.
 *
 *   LIVE_ORIGIN=https://host:port LIVE_TOKEN=... npx playwright test -c playwright.live.config.ts
 */
import { defineConfig } from "@playwright/test";

const CHANNEL = process.env.E2E_CHANNEL ?? "chrome";
const desktop = { width: 1280, height: 800 };
const phone = { width: 390, height: 844 };

export default defineConfig({
  testDir: "e2e",
  testMatch: /bp-shared\.spec\.ts/,
  outputDir: process.env.E2E_OUT ?? "test-results",
  workers: 1,
  reporter: [["list"]],
  timeout: 120_000,
  use: { ignoreHTTPSErrors: true },
  projects: [
    { name: "chromium-desktop", use: { browserName: "chromium", channel: CHANNEL, viewport: desktop } },
    { name: "chromium-phone", use: { browserName: "chromium", channel: CHANNEL, viewport: phone, isMobile: true, hasTouch: true } },
    { name: "webkit-desktop", use: { browserName: "webkit", viewport: desktop } },
    { name: "webkit-phone", use: { browserName: "webkit", viewport: phone, isMobile: true, hasTouch: true } },
  ],
});

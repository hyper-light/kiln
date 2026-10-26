/**
 * The UI's browser tests (e2e/): the production build (`npm run build`, its client in dist/client)
 * served by the mock server (mock/server.ts, MOCK_NO_AUTH=1, on 17073: never 7070, production's),
 * in the machine's own Chrome (`channel: "chrome"`: no browser to download). A desktop and a phone.
 * With `E2E_BASE_URL` they run against that kiln server instead (no mock): `E2E_TOKEN` signs in
 * (its admin token), `E2E_PROGRAM` names the program they read.
 *
 *   npm run build && npm run e2e
 */
import { defineConfig, devices } from "@playwright/test";

const PORT = Number(process.env.E2E_PORT ?? 17073);
/** A real kiln server to run against, instead of the mock. */
const REAL = process.env.E2E_BASE_URL;

export default defineConfig({
  testDir: "e2e",
  outputDir: process.env.E2E_OUT ?? "test-results",
  fullyParallel: false,
  workers: 1,
  reporter: [["list"]],
  timeout: 120_000,
  use: { baseURL: REAL ?? `http://127.0.0.1:${PORT}`, channel: process.env.E2E_CHANNEL ?? "chrome", trace: "off" },
  webServer: REAL
    ? undefined
    : {
        command: "npx tsx mock/server.ts",
        url: `http://127.0.0.1:${PORT}/agents/programs`,
        env: { MOCK_NO_AUTH: "1", PORT: String(PORT), HOST: "127.0.0.1" },
        reuseExistingServer: false,
        timeout: 60_000,
      },
  projects: [
    { name: "desktop", use: { browserName: "chromium", viewport: { width: 1440, height: 900 } } },
    { name: "phone", use: { ...devices["Pixel 7"], browserName: "chromium" } },
    // WebKit too (E2E_WEBKIT=1, with Playwright's WebKit installed: `npx playwright install webkit`).
    ...(process.env.E2E_WEBKIT
      ? [
          { name: "desktop-webkit", use: { browserName: "webkit" as const, channel: undefined, viewport: { width: 1440, height: 900 } } },
          { name: "phone-webkit", use: { ...devices["iPhone 15"], browserName: "webkit" as const, channel: undefined } },
        ]
      : []),
  ],
});

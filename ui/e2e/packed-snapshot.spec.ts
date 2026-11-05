/**
 * Goal 10, plan 01a1a87b2dae step 3: an agent's packed snapshot (`?z=gzip`) is one gzip member kiln
 * puts together from pieces deflated apart (crates/kiln/src/harness/packed.rs): each chunk of events
 * deflated once, ending on a sync flush, the bytes between them in stored blocks, an empty final
 * block last. The browser's own decoder, which the agent page unpacks it with (api/harness.ts,
 * `DecompressionStream("gzip")`), reads it whole. The fixture is what kiln's packer makes
 * (`g10_s3_the_browsers_fixture_is_what_the_packer_makes` holds it so).
 */
import { readFileSync } from "node:fs";

import { expect, test } from "@playwright/test";

test("a snapshot put together from pieces deflated apart unpacks in the browser as one", async ({ page }) => {
  const packed = readFileSync(new URL("./fixtures/packed-snapshot.gz", import.meta.url)).toString("base64");
  const want = readFileSync(new URL("./fixtures/packed-snapshot.txt", import.meta.url), "utf8");
  await page.goto("about:blank");
  const got = await page.evaluate(async (b64) => {
    const bytes = Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
    return await new Response(new Blob([bytes]).stream().pipeThrough(new DecompressionStream("gzip"))).text();
  }, packed);
  expect(got).toBe(want);
});

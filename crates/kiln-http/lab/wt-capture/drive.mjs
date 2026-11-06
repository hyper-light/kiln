// Lab driver (goal 4 steps 1 and 7): opens the page in Playwright's Chromium, Firefox and WebKit
// at desktop and phone widths and prints each browser's version and what the page reported.
//   node drive.mjs <page url> [chromium,firefox,webkit] [desktop,phone]
import { chromium, firefox, webkit } from "playwright";
const [url, which = "chromium,firefox,webkit", widths = "desktop"] = process.argv.slice(2);
const kinds = { chromium, firefox, webkit };
const sizes = { desktop: { width: 1280, height: 800 }, phone: { width: 390, height: 844 } };
for (const name of which.split(",")) {
  for (const w of widths.split(",")) {
    let browser;
    try {
      browser = await kinds[name].launch({
        firefoxUserPrefs: { "network.webtransport.enabled": true, "network.webtransport.datagrams.enabled": true, "network.http.http3.enable": true },
      });
      const page = await browser.newPage({ viewport: sizes[w], isMobile: w === "phone" && name !== "firefox" });
      await page.goto(url);
      await page.waitForFunction(() => window.__result && window.__result.done, null, { timeout: 30000 });
      const r = await page.evaluate(() => window.__result);
      console.log(JSON.stringify({ browser: name, version: browser.version(), width: w, ...r }));
    } catch (e) {
      console.log(JSON.stringify({ browser: name, version: browser ? browser.version() : null, width: w, error: String(e).slice(0, 400) }));
    } finally {
      if (browser) await browser.close();
    }
  }
}

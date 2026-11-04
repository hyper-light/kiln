// Records the real UI (docs/browser-lab.md, "The catalogue"; plan 01a1a832e9cc step 3): Chromium
// opens kiln's own pages on the lab kiln for each journey, and the browser's own network events
// (Chrome DevTools Protocol: every request with its sizes, every socket and each of its frames)
// are written down with their payloads.
//   node record.mjs SPEC.json
// SPEC: { origin, token, out, journeys: [{ name, path, seconds }] }. Writes OUT/<journey>.jsonl,
// one event a line, and OUT/<journey>.bodies/ with each response body.
import { readFileSync, writeFileSync, appendFileSync, mkdirSync } from "node:fs";
import { join } from "node:path";
import { chromium } from "playwright";

const spec = JSON.parse(readFileSync(process.argv[2], "utf8"));
const browser = await chromium.launch();
const context = await browser.newContext({ ignoreHTTPSErrors: true, viewport: { width: 1440, height: 900 } });
const login = await context.request.post(`${spec.origin}/api/auth/login`, { data: { token: spec.token } });
if (!login.ok()) throw new Error(`login: ${login.status()}`);
for (const j of spec.journeys) {
  const file = join(spec.out, `${j.name}.jsonl`);
  const bodies = join(spec.out, `${j.name}.bodies`);
  mkdirSync(bodies, { recursive: true });
  writeFileSync(file, "");
  const page = await context.newPage();
  const cdp = await context.newCDPSession(page);
  await cdp.send("Network.enable", { maxPostDataSize: 65536 });
  await cdp.send("Network.setCacheDisabled", { cacheDisabled: false });
  const put = (o) => appendFileSync(file, JSON.stringify(o) + "\n");
  const pending = [];
  for (const ev of ["Network.requestWillBeSent", "Network.responseReceived", "Network.loadingFailed", "Network.webSocketCreated", "Network.webSocketClosed", "Network.webSocketFrameReceived", "Network.webSocketFrameSent", "Network.eventSourceMessageReceived", "Network.dataReceived"]) {
    cdp.on(ev, (p) => {
      if (ev === "Network.dataReceived") return;
      const o = { ev: ev.slice(8), wall_us: Math.round((performance.timeOrigin + performance.now()) * 1000), ...p };
      if (ev === "Network.requestWillBeSent") o.request = { url: p.request.url, method: p.request.method, postData: p.request.postData, headers: p.request.headers };
      if (ev === "Network.responseReceived") o.response = { url: p.response.url, status: p.response.status, mimeType: p.response.mimeType, protocol: p.response.protocol, encodedDataLength: p.response.encodedDataLength, headers: p.response.headers };
      delete o.initiator;
      put(o);
    });
  }
  cdp.on("Network.loadingFinished", (p) => {
    put({ ev: "loadingFinished", wall_us: Math.round((performance.timeOrigin + performance.now()) * 1000), requestId: p.requestId, encodedDataLength: p.encodedDataLength });
    pending.push(
      cdp
        .send("Network.getResponseBody", { requestId: p.requestId })
        .then((b) => writeFileSync(join(bodies, p.requestId.replace(/[^\w.-]/g, "_")), b.base64Encoded ? Buffer.from(b.body, "base64") : b.body))
        .catch(() => {}),
    );
  });
  const started = Date.now();
  put({ ev: "journey", name: j.name, path: j.path, wall_us: started * 1000 });
  await page.goto(`${spec.origin}${j.path}`, { waitUntil: "domcontentloaded" });
  if (j.type) {
    await page.waitForTimeout(3000);
    await page.keyboard.type(j.type);
  }
  await page.waitForTimeout(j.seconds * 1000);
  await Promise.all(pending);
  put({ ev: "end", wall_us: Date.now() * 1000 });
  await page.close();
}
await browser.close();

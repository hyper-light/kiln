/** Coordinator-owned native browser check. No production origin, credentials or application server. */
import assert from "node:assert/strict";
import http from "node:http";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
const ui = createRequire(new URL("../../../../ui/package.json", import.meta.url));
const { build } = ui("esbuild");
const { chromium, webkit } = ui("playwright");
const engine = process.env.HELD_BROWSER ?? "chromium";
if (!["chromium", "webkit"].includes(engine)) throw new Error("HELD_BROWSER must be chromium or webkit");
const bundled = await build({ entryPoints: [fileURLToPath(new URL("held-browser-entry.ts", import.meta.url))], bundle: true, format: "esm", platform: "browser", write: false, tsconfig: fileURLToPath(new URL("../../../../ui/tsconfig.app.json", import.meta.url)) });
const script = bundled.outputFiles[0]?.text;
assert.ok(script);
const legacy = JSON.stringify({ state: { held: { legacy: [{ id: "old", text: "old original", at: 1 }, { invalid: "preserve this exact raw value" }] } }, version: 0 });
const server = http.createServer((request, response) => {
  if (request.url === "/fixture.js") { response.setHeader("content-type", "text/javascript"); response.end(script); return; }
  response.setHeader("content-type", "text/html");
  response.end(`<script>if(!localStorage.getItem('kiln.agents.held'))localStorage.setItem('kiln.agents.held',${JSON.stringify(legacy)})</script><script type="module" src="/fixture.js"></script>`);
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const address = server.address();
assert.ok(address && typeof address !== "string");
let browser;
try {
  browser = await (engine === "webkit" ? webkit : chromium).launch(engine === "chromium" ? { channel: process.env.E2E_CHANNEL ?? "chrome" } : {});
  const context = await browser.newContext();
  const a = await context.newPage();
  const b = await context.newPage();
  const origin = `http://127.0.0.1:${address.port}`;
  await Promise.all([a.goto(origin), b.goto(origin)]);
  for (const page of [a, b]) { await page.waitForFunction(() => window.heldFixture); await page.evaluate(() => window.heldFixture.ready); }
  const outcomes = await Promise.all([a.evaluate(() => window.heldFixture.useHeld.getState().hold("agent", { id: "tab-a", text: "first original", at: 1 })), b.evaluate(() => window.heldFixture.useHeld.getState().hold("agent", { id: "tab-b", text: "second original", at: 1 }))]);
  assert.deepEqual(outcomes, [true, true]);
  assert.deepEqual(await a.evaluate(async () => (await window.heldFixture.readHeld()).agent.map((message) => message.id).sort()), ["tab-a", "tab-b"]);
  await a.evaluate(() => {
    window.fixtureStorageErrors = [];
    const put = IDBObjectStore.prototype.put;
    IDBObjectStore.prototype.put = function(...args) {
      try { return put.apply(this,args); }
      catch(error) { window.fixtureStorageErrors.push({place:"put",name:error.name,message:error.message}); throw error; }
    };
    const transaction = IDBDatabase.prototype.transaction;
    IDBDatabase.prototype.transaction = function(...args) {
      const tx = transaction.apply(this,args);
      tx.addEventListener("abort", () => window.fixtureStorageErrors.push({place:"abort",name:tx.error?.name,message:tx.error?.message}));
      return tx;
    };
  });
  const fileHeld = await a.evaluate(() => window.heldFixture.useHeld.getState().hold("agent", { id: "file", text: "with pending file", at: 1, uploading: ["proof.txt"], originals: [{ name: "proof.txt", mime: "text/plain", blob: new Blob(["exact complete bytes"]) }] }));
  assert.equal(fileHeld, true, JSON.stringify(await a.evaluate(() => window.fixtureStorageErrors)));

  await a.reload();
  await a.waitForFunction(() => window.heldFixture);
  await a.evaluate(() => window.heldFixture.ready);
  assert.equal(await a.evaluate(async () => (await window.heldFixture.readHeld()).agent.find((message) => message.id === "file").originals[0].blob.text()), "exact complete bytes");
  for (const fault of ["abort", "quota"]) {
    assert.equal(await a.evaluate(async (kind) => {
      const put = IDBObjectStore.prototype.put;
      IDBObjectStore.prototype.put = function (...args) {
        if (args[1] !== "originals") return put.apply(this, args);
        if (kind === "quota") throw new DOMException("fixture quota", "QuotaExceededError");
        const request = put.apply(this, args);
        this.transaction.abort();
        return request;
      };
      try { return await window.heldFixture.useHeld.getState().hold("agent", { id: kind, text: "caller-owned", at: 1 }); }
      finally { IDBObjectStore.prototype.put = put; }
    }, fault), false);
  }
  const accounts = await b.evaluate(async () => {
    window.heldFixture.setMailScope("browser-account-b");
    const held = window.heldFixture.useHeld.getState();
    await held.hold("agent", { id: "tab-a", text: "other account original", at: 1 });
    await held.settled("agent", "tab-a");
    return (await window.heldFixture.readHeld()).agent.map((message) => [message.id, message.owner, message.text]);
  });
  assert.ok(accounts.some(([id, owner, text]) => id === "tab-a" && owner === "browser-account-a" && text === "first original"));
  assert.ok(!accounts.some(([id]) => id === "abort" || id === "quota"));
  assert.equal(await a.evaluate(() => localStorage.getItem("kiln.agents.held")), legacy);
  assert.equal(await a.evaluate(async () => (await window.heldFixture.readHeld()).legacy[0].owner ?? null), null);
  await context.close();
  process.stdout.write(JSON.stringify({ engine, result: "pass", checks: ["two native tab writers", "reload with original Blob", "abort", "quota refusal", "account isolation", "legacy bytes preserved"] }) + "\n");
} finally {
  await browser?.close();
  await new Promise((resolve) => server.close(resolve));
}

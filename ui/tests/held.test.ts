import assert from "node:assert/strict";
import { test } from "node:test";
import { IDBFactory, IDBObjectStore } from "fake-indexeddb";

// Simulated IndexedDB transactions, not browser/crash durability evidence.
const legacy = JSON.stringify({ state: { held: { old: [{ id: "unbound", text: "original legacy", at: 1 }, { broken: "preserve these raw bytes" }] } }, version: 0 });
const local = new Map([["kiln.agents.held", legacy]]);
Object.assign(globalThis, {
  indexedDB: new IDBFactory(),
  localStorage: { getItem: (key: string) => local.get(key) ?? null },
  window: new EventTarget(),
  BroadcastChannel: class {
    postMessage() {}
  },
});
const { setMailScope } = await import("../src/api/client.ts");
setMailScope("account-a");
const { useHeld, readHeld } = await import("../src/api/held.ts");
await useHeld.getState().ready();
const original = (id: string) => ({ id, text: `original ${id}`, at: 1, mode: "queue" as const });

await test("W20: legacy migration commits valid originals, quarantines their unbound scope and retains invalid raw bytes", async () => {
  const held = await readHeld();
  assert.equal(held.old?.[0]?.text, "original legacy");
  assert.equal(held.old?.[0]?.owner, undefined);
  assert.equal(local.get("kiln.agents.held"), legacy, "the source is never erased before or after the transaction");
  const backup = await new Promise<unknown[]>((resolve, reject) => {
    const request = indexedDB.open("kiln.agents.held");
    request.onsuccess = () => {
      const db = request.result;
      const tx = db.transaction("legacy", "readonly");
      const rows = tx.objectStore("legacy").getAll();
      tx.oncomplete = () => {
        db.close();
        resolve(rows.result);
      };
      tx.onabort = () => reject(tx.error);
    };
  });
  assert.deepEqual(backup, [legacy], "invalid rows survive as the exact raw legacy backup");
});

await test("W20: overlapping simulated readwrite transactions preserve both writers instead of replacing another original", async () => {
  assert.deepEqual(await Promise.all([useHeld.getState().hold("parallel", original("first")), useHeld.getState().hold("parallel", original("second"))]), [true, true]);
  assert.deepEqual(
    (await readHeld()).parallel?.map((message) => message.id),
    ["first", "second"],
  );
  assert.equal(await useHeld.getState().hold("parallel", { ...original("first"), text: "changed under same identity" }), false);
  assert.equal((await readHeld()).parallel?.[0]?.text, "original first");
});

await test("W20: a multi-recipient original commits for every recipient or none", async () => {
  assert.equal(
    await useHeld.getState().holdMany([
      { agent: "batch-a", message: original("a") },
      { agent: "batch-b", message: original("b") },
    ]),
    true,
  );
  assert.equal(
    await useHeld.getState().holdMany([
      { agent: "batch-c", message: original("c") },
      { agent: "batch-a", message: { ...original("a"), text: "changed" } },
    ]),
    false,
  );
  const held = await readHeld();
  assert.equal(held["batch-c"], undefined, "the earlier write in a refused batch was rolled back");
  assert.equal(held["batch-a"]?.[0]?.text, "original a");
  assert.equal(held["batch-b"]?.[0]?.text, "original b");
});

await test("W20: original Blob bytes survive a committed pending upload and a fresh projection read", async () => {
  const blob = new Blob(["complete original bytes"], { type: "text/plain" });
  assert.equal(await useHeld.getState().hold("blobs", { ...original("file"), uploading: ["proof.txt"], originals: [{ name: "proof.txt", mime: "text/plain", blob }] }), true);
  const message = (await readHeld()).blobs?.[0];
  assert.equal(await message?.originals?.[0]?.blob.text(), "complete original bytes");
  await useHeld.getState().uploaded("blobs", "file", []);
  const failed = (await readHeld()).blobs?.[0];
  assert.deepEqual(failed?.uploading, ["proof.txt"], "failed upload does not authorize dropping the file or sending without it");
  assert.equal(await failed?.originals?.[0]?.blob.text(), "complete original bytes");
});

await test("W20: an aborted write transfers no ownership and leaves all earlier originals intact", async () => {
  const put = IDBObjectStore.prototype.put;
  IDBObjectStore.prototype.put = function (...args) {
    const request = put.apply(this, args);
    if (args[1] === "originals") this.transaction.abort();
    return request;
  };
  try {
    assert.equal(await useHeld.getState().hold("aborted", original("not-committed")), false);
    assert.match(useHeld.getState().error ?? "", /Nothing new was sent/);
  } finally {
    IDBObjectStore.prototype.put = put;
  }
  const held = await readHeld();
  assert.equal(held.aborted, undefined);
  assert.equal(held.parallel?.length, 2);
  assert.equal(await held.blobs?.[0]?.originals?.[0]?.blob.text(), "complete original bytes");
});

await test("W20: account changes preserve separate originals and cannot edit or settle another account's body", async () => {
  assert.equal(await useHeld.getState().hold("scope", original("same-id")), true);
  setMailScope("account-b");
  assert.equal(await useHeld.getState().hold("scope", { ...original("same-id"), text: "account b original" }), true);
  await useHeld.getState().settled("scope", "same-id");
  const held = (await readHeld()).scope;
  assert.deepEqual(
    held?.map((message) => [message.owner, message.text]),
    [["account-a", "original same-id"]],
  );
  setMailScope("account-a");
});

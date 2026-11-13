import assert from "node:assert/strict";
import { test } from "node:test";
import { IDBFactory, IDBObjectStore } from "fake-indexeddb";

// Simulated IndexedDB, not browser/crash durability evidence. The browser's copy is a backup of
// what the server hasn't taken (2026-11-13): one record per message, written after memory has it.
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

/** W20's database as an older build left it: one set of every original, with rows no build reads. */
await new Promise<void>((resolve, reject) => {
  const request = indexedDB.open("kiln.agents.held", 1);
  request.onupgradeneeded = () => {
    request.result.createObjectStore("held");
    request.result.createObjectStore("legacy", { autoIncrement: true });
  };
  request.onsuccess = () => {
    const db = request.result;
    const tx = db.transaction("held", "readwrite");
    tx.objectStore("held").put(
      {
        kept: [{ id: "w20-kept", text: "kept by W20", at: 2, owner: "account-a", custody: "offered" }],
        worn: [{ id: "no-bytes", text: "a file without bytes", at: 3, owner: "account-a", originals: [{ name: "a.txt", mime: "text/plain" }] }],
        other: "not a list",
      },
      "originals",
    );
    tx.oncomplete = () => {
      db.close();
      resolve();
    };
    tx.onabort = () => reject(tx.error);
  };
});

const { setMailScope } = await import("../src/api/client.ts");
setMailScope("account-a");
const { useHeld, readHeld, flushHeld } = await import("../src/api/held.ts");
await useHeld.getState().ready();
const original = (id: string) => ({ id, text: `original ${id}`, at: 1, mode: "queue" as const });

/** Every row of `store` in database `name`. */
async function rows(name: string, store: string): Promise<unknown[]> {
  return new Promise((resolve, reject) => {
    const request = indexedDB.open(name);
    request.onsuccess = () => {
      const db = request.result;
      const tx = db.transaction(store, "readonly");
      const all = tx.objectStore(store).getAll();
      tx.oncomplete = () => {
        db.close();
        resolve(all.result);
      };
      tx.onabort = () => reject(tx.error);
    };
  });
}

await test("composer_send: W20's one set and the box's localStorage become one record per message, once, their unreadable rows kept aside", async () => {
  const held = await readHeld();
  assert.equal(held.kept?.[0]?.text, "kept by W20");
  assert.equal(held.worn?.[0]?.text, "a file without bytes", "a message whose file had no bytes comes over without that file");
  assert.equal(held.worn?.[0]?.originals?.length ?? 0, 0);
  assert.equal(held.other, undefined, "a row that isn't a list isn't a message");
  assert.equal(held.old?.[0]?.text, "original legacy", "the box's localStorage before W20 comes over too");
  assert.equal(held.old?.length, 1, "its unreadable row doesn't");
  const records = await rows("kiln.agents.backup", "messages");
  assert.equal(records.length, 3, "one record each");
  const aside = await rows("kiln.agents.backup", "legacy");
  assert.ok(
    aside.some((row) => typeof row === "object" && row !== null && (row as { other?: unknown }).other === "not a list"),
    "W20's set as it was is kept",
  );
  assert.ok(aside.includes(legacy), "the localStorage as it was is kept");
  assert.equal(local.get("kiln.agents.held"), legacy, "localStorage is never erased");
  assert.equal((await rows("kiln.agents.held", "held")).length, 1, "W20's database is left as it was");
});

await test("composer_send: a message is in memory before anything is written, and its record follows", async () => {
  const write = useHeld.getState().hold("fast", original("now"));
  assert.equal(useHeld.getState().held.fast?.[0]?.text, "original now", "memory has it in the same moment");
  assert.equal(await write, true);
  await flushHeld();
  assert.equal((await readHeld()).fast?.[0]?.text, "original now");
});

await test("composer_send: holding a message writes its record alone, never the others", async () => {
  for (let i = 0; i < 20; i += 1) await useHeld.getState().hold("many", original(`m${i}`));
  await flushHeld();
  const put = IDBObjectStore.prototype.put;
  let puts = 0;
  IDBObjectStore.prototype.put = function (...args) {
    puts += 1;
    return put.apply(this, args);
  };
  try {
    await useHeld.getState().hold("many", original("one-more"));
    await flushHeld();
  } finally {
    IDBObjectStore.prototype.put = put;
  }
  assert.equal(puts, 1, "one record written, however many are kept");
  assert.equal((await readHeld()).many?.length, 21);
});

await test("composer_send: a record the server's receipt lets go is deleted alone", async () => {
  await useHeld.getState().settled("many", "m3");
  await flushHeld();
  const ids = (await readHeld()).many?.map((m) => m.id) ?? [];
  assert.equal(ids.length, 20);
  assert.ok(!ids.includes("m3"));
});

await test("W20: overlapping writers each keep their own original", async () => {
  assert.deepEqual(await Promise.all([useHeld.getState().hold("parallel", original("first")), useHeld.getState().hold("parallel", original("second"))]), [true, true]);
  await flushHeld();
  assert.deepEqual((await readHeld()).parallel?.map((message) => message.id).sort(), ["first", "second"]);
  assert.equal(await useHeld.getState().hold("parallel", { ...original("first"), text: "changed under same identity" }), false);
  await flushHeld();
  assert.equal((await readHeld()).parallel?.find((m) => m.id === "first")?.text, "original first");
});

await test("W20: a multi-recipient original is kept for every recipient or none", async () => {
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
  await flushHeld();
  const held = await readHeld();
  assert.equal(held["batch-c"], undefined, "the earlier one in a refused batch wasn't kept");
  assert.equal(held["batch-a"]?.[0]?.text, "original a");
  assert.equal(held["batch-b"]?.[0]?.text, "original b");
});

await test("W20: a file's bytes survive a pending upload and a fresh read", async () => {
  const blob = new Blob(["complete original bytes"], { type: "text/plain" });
  assert.equal(await useHeld.getState().hold("blobs", { ...original("file"), uploading: ["proof.txt"], originals: [{ name: "proof.txt", mime: "text/plain", blob }] }), true);
  await flushHeld();
  const message = (await readHeld()).blobs?.[0];
  assert.equal(await message?.originals?.[0]?.blob.text(), "complete original bytes");
  await useHeld.getState().uploaded("blobs", "file", []);
  await flushHeld();
  const failed = (await readHeld()).blobs?.[0];
  assert.deepEqual(failed?.uploading, ["proof.txt"], "a failed upload doesn't drop the file or send without it");
  assert.equal(await failed?.originals?.[0]?.blob.text(), "complete original bytes");
});

await test("composer_send: a write that fails keeps the message in memory, says so, and leaves the other records as they were", async () => {
  const put = IDBObjectStore.prototype.put;
  IDBObjectStore.prototype.put = function (...args) {
    const request = put.apply(this, args);
    this.transaction.abort();
    return request;
  };
  try {
    assert.equal(await useHeld.getState().hold("aborted", original("not-written")), true);
    await flushHeld();
  } finally {
    IDBObjectStore.prototype.put = put;
  }
  assert.equal(useHeld.getState().held.aborted?.[0]?.text, "original not-written", "memory keeps it");
  assert.match(useHeld.getState().error ?? "", /couldn't keep a copy/);
  const held = await readHeld();
  assert.equal(held.aborted, undefined);
  assert.equal(held.parallel?.length, 2);
  assert.equal(await held.blobs?.[0]?.originals?.[0]?.blob.text(), "complete original bytes");
});

await test("W20: account changes keep separate originals and can't settle another account's", async () => {
  assert.equal(await useHeld.getState().hold("scope", original("same-id")), true);
  setMailScope("account-b");
  assert.equal(await useHeld.getState().hold("scope", { ...original("same-id"), text: "account b original" }), true);
  await useHeld.getState().settled("scope", "same-id");
  await flushHeld();
  const held = (await readHeld()).scope;
  assert.deepEqual(
    held?.map((message) => [message.owner, message.text]),
    [["account-a", "original same-id"]],
  );
  setMailScope("account-a");
});

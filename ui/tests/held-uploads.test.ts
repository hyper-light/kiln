import assert from "node:assert/strict";
import { test } from "node:test";
import { IDBFactory, IDBObjectStore } from "fake-indexeddb";

const values = new Map<string, string>();
const storage = {
  getItem: (key: string) => values.get(key) ?? null,
  setItem: (key: string, value: string) => void values.set(key, value),
  removeItem: (key: string) => void values.delete(key),
};
Object.assign(globalThis, {
  indexedDB: new IDBFactory(),
  BroadcastChannel: class {
    postMessage() {}
  },
  window: Object.assign(new EventTarget(), { setTimeout, clearTimeout }),
  localStorage: storage,
  sessionStorage: storage,
});
const { setMailScope } = await import("../src/api/client.ts");
setMailScope("upload-test-scope");
const { flushHeld, readHeld } = await import("../src/api/held.ts");
const { holdWith, uploadingHere, useHeld } = await import("../src/views/harness/state.ts");

const tick = () => new Promise<void>((resolve) => setImmediate(resolve));
const kept = (agent: string) => useHeld.getState().held[agent] ?? [];
const file = (name: string) => ({ name, mime: "text/plain", file_id: `up-${name}` });

/** Files taken from a box as a message is sent, those still uploading answered when the test says. */
function taken(up: string[], uploading: string[]) {
  let answer: (files: string[]) => void = () => {};
  const done = new Promise<ReturnType<typeof file>[]>((resolve) => (answer = (names) => resolve(names.map(file))));
  return { taken: { files: up.map(file), uploading, up: done, back: () => {} }, answer };
}

await test("W21-R15: a message sent while its files upload waits for them, then carries each that made it", async () => {
  const { taken: t, answer } = taken(["a.txt"], ["b.png"]);
  await holdWith("agent-1", { id: "m1", text: "Read these", at: 1, mode: "queue" }, t);
  assert.deepEqual(
    kept("agent-1").map((m) => [m.id, m.uploading, m.attachments?.map((a) => a.name)]),
    [["m1", ["b.png"], ["a.txt"]]],
  );
  assert.ok(uploadingHere.has("m1"), "this page is uploading its files");
  // Kept across a reload with what it still waits for (the page that uploads them says it's this one).
  assert.deepEqual((await readHeld())["agent-1"]?.[0]?.uploading, ["b.png"]);
  answer(["a.txt", "b.png"]);
  await tick();
  await useHeld.getState().ready();
  assert.deepEqual(
    kept("agent-1").map((m) => [m.id, m.uploading, m.attachments?.map((a) => a.name)]),
    [["m1", undefined, ["a.txt", "b.png"]]],
  );
  assert.ok(!uploadingHere.has("m1"));
  await useHeld.getState().drop("agent-1", "m1");
});

await test("W21-R15: one whose files all failed goes with its words, and one with neither is dropped", async () => {
  const words = taken([], ["big.mov"]);
  await holdWith("agent-2", { id: "w", text: "And this", at: 1, mode: "queue" }, words.taken);
  const bare = taken([], ["big.mov"]);
  await holdWith("agent-2", { id: "bare", text: "", at: 2, mode: "queue" }, bare.taken);
  words.answer([]);
  bare.answer([]);
  await tick();
  await useHeld.getState().ready();
  assert.deepEqual(
    kept("agent-2").map((m) => [m.id, m.text, m.uploading, m.attachments]),
    [["w", "And this", undefined, undefined]],
  );
  await useHeld.getState().drop("agent-2", "w");
});

await test("W21-R15: one sent without files is kept as it was, waiting for nothing", async () => {
  await holdWith("agent-3", { id: "plain", text: "Hello", at: 1, mode: "queue" }, { files: [], uploading: [], up: Promise.resolve([]), back: () => {} });
  assert.deepEqual(
    kept("agent-3").map((m) => [m.id, m.uploading, m.attachments]),
    [["plain", undefined, undefined]],
  );
  assert.ok(!uploadingHere.has("plain"));
  await useHeld.getState().drop("agent-3", "plain");
});

await test("W21-R15: files whose uploads went with an earlier page are let go one by one, and then it goes", async () => {
  // As a reload leaves it: kept, waiting for two files no page is uploading.
  await useHeld.getState().hold("agent-4", { id: "old", text: "", at: 1, mode: "queue", attachments: [file("up.txt")], uploading: ["x.png", "y.png"] });
  await useHeld.getState().without("agent-4", "old", 0);
  assert.deepEqual(kept("agent-4")[0]?.uploading, ["y.png"]);
  await useHeld.getState().without("agent-4", "old", 0);
  assert.deepEqual(
    kept("agent-4").map((m) => [m.uploading, m.attachments?.map((a) => a.name)]),
    [[undefined, ["up.txt"]]],
  );
  // With nothing else to carry, letting its last file go drops it.
  await useHeld.getState().hold("agent-4", { id: "empty", text: "", at: 2, mode: "queue", uploading: ["z.png"] });
  await useHeld.getState().without("agent-4", "empty", 0);
  assert.deepEqual(
    kept("agent-4").map((m) => m.id),
    ["old"],
  );
  await useHeld.getState().drop("agent-4", "old");
  assert.equal(useHeld.getState().held["agent-4"], undefined);
});

await test("composer_send: a write this browser can't make still keeps the message here and goes on with its upload", async () => {
  const put = IDBObjectStore.prototype.put;
  let restored = 0;
  let cleared = 0;
  IDBObjectStore.prototype.put = function (...args) {
    const request = put.apply(this, args);
    this.transaction.abort();
    return request;
  };
  try {
    assert.equal(
      await holdWith(
        "failed-original",
        { id: "keep-file", text: "keep words", at: 1 },
        {
          files: [],
          uploading: ["proof.txt"],
          originals: [{ name: "proof.txt", mime: "text/plain", blob: new Blob(["all original bytes"]) }],
          up: Promise.resolve([]),
          back: () => {
            restored += 1;
          },
          commit: () => {
            cleared += 1;
          },
        },
      ),
      true,
      "kept in this page's memory: storage that refuses never stops it",
    );
    assert.equal(restored, 0);
    assert.equal(cleared, 1, "the box lets go of what it handed on");
    assert.equal(useHeld.getState().held["failed-original"]?.[0]?.text, "keep words");
    await flushHeld();
  } finally {
    IDBObjectStore.prototype.put = put;
  }
  assert.equal((await readHeld())["failed-original"], undefined, "IndexedDB couldn't take it");
});

import assert from "node:assert/strict";
import { test } from "node:test";

const values = new Map<string, string>();
const storage = {
  getItem: (key: string) => values.get(key) ?? null,
  setItem: (key: string, value: string) => void values.set(key, value),
  removeItem: (key: string) => void values.delete(key),
};
Object.assign(globalThis, { window: Object.assign(new EventTarget(), { setTimeout, clearTimeout }), localStorage: storage, sessionStorage: storage });
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
  holdWith("agent-1", { id: "m1", text: "Read these", at: 1, mode: "queue" }, t);
  assert.deepEqual(
    kept("agent-1").map((m) => [m.id, m.uploading, m.attachments?.map((a) => a.name)]),
    [["m1", ["b.png"], ["a.txt"]]],
  );
  assert.ok(uploadingHere.has("m1"), "this page is uploading its files");
  // Kept across a reload with what it still waits for (the page that uploads them says it's this one).
  assert.deepEqual(JSON.parse(values.get("kiln.agents.held") ?? "{}").state.held["agent-1"][0].uploading, ["b.png"]);
  answer(["a.txt", "b.png"]);
  await tick();
  assert.deepEqual(
    kept("agent-1").map((m) => [m.id, m.uploading, m.attachments?.map((a) => a.name)]),
    [["m1", undefined, ["a.txt", "b.png"]]],
  );
  assert.ok(!uploadingHere.has("m1"));
  useHeld.getState().drop("agent-1", "m1");
});

await test("W21-R15: one whose files all failed goes with its words, and one with neither is dropped", async () => {
  const words = taken([], ["big.mov"]);
  holdWith("agent-2", { id: "w", text: "And this", at: 1, mode: "queue" }, words.taken);
  const bare = taken([], ["big.mov"]);
  holdWith("agent-2", { id: "bare", text: "", at: 2, mode: "queue" }, bare.taken);
  words.answer([]);
  bare.answer([]);
  await tick();
  assert.deepEqual(
    kept("agent-2").map((m) => [m.id, m.text, m.uploading, m.attachments]),
    [["w", "And this", undefined, undefined]],
  );
  useHeld.getState().drop("agent-2", "w");
});

await test("W21-R15: one sent without files is kept as it was, waiting for nothing", () => {
  holdWith("agent-3", { id: "plain", text: "Hello", at: 1, mode: "queue" }, { files: [], uploading: [], up: Promise.resolve([]), back: () => {} });
  assert.deepEqual(
    kept("agent-3").map((m) => [m.id, m.uploading, m.attachments]),
    [["plain", undefined, undefined]],
  );
  assert.ok(!uploadingHere.has("plain"));
  useHeld.getState().drop("agent-3", "plain");
});

await test("W21-R15: files whose uploads went with an earlier page are let go one by one, and then it goes", () => {
  // As a reload leaves it: kept, waiting for two files no page is uploading.
  useHeld.getState().hold("agent-4", { id: "old", text: "", at: 1, mode: "queue", attachments: [file("up.txt")], uploading: ["x.png", "y.png"] });
  useHeld.getState().without("agent-4", "old", 0);
  assert.deepEqual(kept("agent-4")[0]?.uploading, ["y.png"]);
  useHeld.getState().without("agent-4", "old", 0);
  assert.deepEqual(
    kept("agent-4").map((m) => [m.uploading, m.attachments?.map((a) => a.name)]),
    [[undefined, ["up.txt"]]],
  );
  // With nothing else to carry, letting its last file go drops it.
  useHeld.getState().hold("agent-4", { id: "empty", text: "", at: 2, mode: "queue", uploading: ["z.png"] });
  useHeld.getState().without("agent-4", "empty", 0);
  assert.deepEqual(
    kept("agent-4").map((m) => m.id),
    ["old"],
  );
  useHeld.getState().drop("agent-4", "old");
  assert.equal(useHeld.getState().held["agent-4"], undefined);
});

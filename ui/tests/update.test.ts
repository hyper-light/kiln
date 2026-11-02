/**
 * A part of the page that can't load reloads it onto the new build, once in 10 s (lib/update.ts);
 * but not while the page is being left: a navigation away cancels what it was still loading, and
 * WebKit fails those imports, so a reload then took the page back from where it was going.
 */
import assert from "node:assert/strict";
import { test } from "node:test";

const values = new Map<string, string>();
const storage = {
  getItem: (key: string) => values.get(key) ?? null,
  setItem: (key: string, value: string) => void values.set(key, value),
  removeItem: (key: string) => void values.delete(key),
};
const timers: (() => void)[] = [];
let reloads = 0;
const browser = Object.assign(new EventTarget(), {
  setTimeout: (fn: () => void) => timers.push(fn),
  clearTimeout: () => {},
  location: { reload: () => void reloads++ },
});
Object.assign(globalThis, { window: browser, sessionStorage: storage, localStorage: storage, document: new EventTarget(), location: { protocol: "http:", host: "localhost" } });
const { watchForUpdates } = await import("../src/lib/update.ts");
watchForUpdates();

/** A lazily loaded part fails, as Vite says it; whether the page took Vite's error. */
const fails = () => {
  const e = new Event("vite:preloadError", { cancelable: true });
  window.dispatchEvent(e);
  return e.defaultPrevented;
};

await test("a part that can't load reloads the page, and not twice in 10 s", () => {
  values.clear();
  reloads = 0;
  assert.equal(fails(), true);
  assert.equal(reloads, 1);
  assert.equal(fails(), true);
  assert.equal(reloads, 1);
});

await test("leaving the page, what its leaving cancels reloads nothing; still there 5 s on, it does", () => {
  values.clear();
  reloads = 0;
  window.dispatchEvent(new Event("beforeunload"));
  assert.equal(fails(), true, "Vite's error is taken all the same");
  assert.equal(reloads, 0);
  for (const fn of timers.splice(0)) fn();
  assert.equal(fails(), true);
  assert.equal(reloads, 1);
});

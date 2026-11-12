import assert from "node:assert/strict";
import { getEventListeners } from "node:events";
import { test, type TestContext } from "node:test";
import { fitTheWindow } from "../src/components/shell/window-fit.ts";

/** A viewport whose geometry and frames are controlled, while listeners use real EventTargets. */
function viewport(t: TestContext, visual = true) {
  const readsAndWrites: string[] = [];
  const geometry = { height: 800, scale: 1, offset: 0, innerHeight: 800, x: 0, y: 0 };
  const classes = new Set<string>();
  const styles = new Map<string, string>();
  const data: Record<string, string> = {};
  const root = {
    classList: {
      add(name: string) {
        readsAndWrites.push(`write:class:${name}`);
        classes.add(name);
      },
      remove(name: string) {
        readsAndWrites.push(`write:unclass:${name}`);
        classes.delete(name);
      },
    },
    style: {
      setProperty(name: string, value: string) {
        readsAndWrites.push(`write:style:${name}`);
        styles.set(name, value);
      },
      removeProperty(name: string) {
        readsAndWrites.push(`write:unstyle:${name}`);
        styles.delete(name);
      },
    },
    dataset: new Proxy(data, {
      set(target, name, value) {
        readsAndWrites.push(`write:data:${String(name)}`);
        return Reflect.set(target, name, value);
      },
      deleteProperty(target, name) {
        readsAndWrites.push(`write:undata:${String(name)}`);
        return Reflect.deleteProperty(target, name);
      },
    }),
  };
  const read = (name: keyof typeof geometry) => {
    readsAndWrites.push(`read:${name}`);
    return geometry[name];
  };
  const view = Object.defineProperties(new EventTarget(), {
    height: { get: () => read("height") },
    scale: { get: () => read("scale") },
    offsetTop: { get: () => read("offset") },
  });
  const frames = new Map<number, FrameRequestCallback>();
  let next = 0;
  let pans = 0;
  const win = Object.assign(
    Object.defineProperties(new EventTarget(), {
      visualViewport: { value: visual ? view : null },
      innerHeight: { get: () => read("innerHeight") },
      scrollX: { get: () => read("x") },
      scrollY: { get: () => read("y") },
    }),
    {
      requestAnimationFrame(callback: FrameRequestCallback) {
        const id = next++;
        frames.set(id, callback);
        return id;
      },
      cancelAnimationFrame(id: number) {
        frames.delete(id);
      },
      scrollTo(options: ScrollToOptions) {
        assert.deepEqual(options, { top: 0, left: 0, behavior: "instant" });
        readsAndWrites.push("write:pan");
        geometry.x = geometry.y = 0;
        pans++;
      },
    },
  );
  const doc = { documentElement: root, activeElement: { text: "an unfinished draft", selectionStart: 4 } };
  for (const [name, value] of Object.entries({ window: win, document: doc })) {
    const previous = Object.getOwnPropertyDescriptor(globalThis, name);
    Object.defineProperty(globalThis, name, { value, configurable: true, writable: true });
    t.after(() => {
      if (previous) Object.defineProperty(globalThis, name, previous);
      else Reflect.deleteProperty(globalThis, name);
    });
  }
  const mount = () => {
    const stop = fitTheWindow(root as unknown as HTMLElement);
    t.after(() => stop?.());
    return stop;
  };
  const flush = () => {
    const due = [...frames.values()];
    frames.clear();
    for (const callback of due) callback(0);
  };
  const listeners = () =>
    [getEventListeners(view, "resize"), getEventListeners(view, "scroll"), ...["resize", "pageshow", "scroll"].map((event) => getEventListeners(win, event))].map((list) => list.length);
  return { root, view, win, doc, geometry, classes, styles, data, frames, mount, flush, listeners, readsAndWrites, pans: () => pans };
}

await test("W21-R13 entry fits synchronously in its original overflow mode before returning", (t) => {
  const v = viewport(t);
  v.geometry.height = 637.4;
  v.mount();
  assert.equal(v.frames.size, 0, "initial geometry must not depend on another animation frame");
  assert.equal(v.styles.get("--window-h"), "637px", "fit the current viewport within the mounting commit");
  assert.equal(v.classes.has("fills-window"), true);
  assert.equal(v.readsAndWrites[0], "write:class:fills-window", "measure after entering the original overflow mode");
  const firstWrite = v.readsAndWrites.findIndex((op) => op.startsWith("write:") && op !== "write:class:fills-window");
  assert.ok(firstWrite > 0);
  assert.ok(
    v.readsAndWrites.slice(firstWrite).every((op) => !op.startsWith("read:")),
    "no read after a style invalidates layout",
  );
  assert.deepEqual(v.listeners(), [1, 1, 1, 1, 1]);
});

await test("W21-R13 repeated viewport events do not rewrite unchanged geometry", (t) => {
  const v = viewport(t);
  v.mount();
  v.flush();
  v.readsAndWrites.length = 0;
  for (let i = 0; i < 20; i++) {
    v.view.dispatchEvent(new Event("resize"));
    v.view.dispatchEvent(new Event("scroll"));
    for (const event of ["resize", "pageshow", "scroll"]) v.win.dispatchEvent(new Event(event));
  }
  assert.ok(v.readsAndWrites.every((op) => op.startsWith("read:")));
  assert.equal(v.frames.size, 0, "subsequent events are immediate, without another queued fit");
});

await test("W21-R13 keyboard opening and closing fit immediately without queuing a frame", (t) => {
  const v = viewport(t);
  v.mount();
  assert.equal(v.styles.get("--window-h"), "800px");
  v.geometry.height = 400;
  v.geometry.offset = 20;
  v.view.dispatchEvent(new Event("resize"));
  assert.equal(v.styles.get("--window-h"), "400px");
  assert.equal(v.data.window, "short");
  assert.equal(v.pans(), 1);
  assert.equal(v.frames.size, 0);
  v.geometry.offset = 0;
  v.geometry.height = 720;
  v.view.dispatchEvent(new Event("resize"));
  assert.equal(v.styles.get("--window-h"), "720px", "closing the keyboard is immediate too");
  assert.equal(v.data.window, undefined);
  const field = v.doc.activeElement;
  v.flush();
  assert.equal(v.doc.activeElement, field);
  assert.equal(field.text, "an unfinished draft");
  assert.equal(field.selectionStart, 4);
});

await test("W21-R13 zoom keeps unzoomed height while keyboard geometry changes without forcing a pan", (t) => {
  const v = viewport(t);
  v.geometry.scale = 2;
  v.geometry.height = 300;
  v.geometry.offset = 50;
  v.geometry.y = 100;
  v.mount();
  assert.equal(v.styles.get("--window-h"), "600px");
  assert.equal(v.data.window, undefined);
  assert.equal(v.pans(), 0);
  v.geometry.height = 220;
  v.view.dispatchEvent(new Event("resize"));
  assert.equal(v.styles.get("--window-h"), "440px");
  assert.equal(v.data.window, "short");
  assert.equal(v.pans(), 0, "deliberate zoom and panning are left alone");
});

await test("W21-R13 a same-height native keyboard pan still corrects the unzoomed window", (t) => {
  const v = viewport(t);
  v.mount();
  v.flush();
  v.readsAndWrites.length = 0;
  v.geometry.offset = 10;
  v.view.dispatchEvent(new Event("scroll"));
  assert.equal(v.pans(), 1);
  assert.deepEqual(
    v.readsAndWrites.filter((op) => op.startsWith("write:")),
    ["write:pan"],
  );
  v.geometry.offset = 0;
  v.geometry.x = 5;
  v.win.dispatchEvent(new Event("scroll"));
  assert.equal(v.pans(), 2, "horizontal page drift is corrected as before");
});

await test("W21-R13 short-height threshold still uses rounded unzoomed pixels", (t) => {
  const v = viewport(t);
  v.geometry.height = 559.4;
  v.mount();
  assert.equal(v.styles.get("--window-h"), "559px");
  assert.equal(v.data.window, "short");
  v.geometry.height = 559.6;
  v.view.dispatchEvent(new Event("resize"));
  assert.equal(v.styles.get("--window-h"), "560px");
  assert.equal(v.data.window, undefined);
});

await test("W21-R13 without visualViewport the inner height fits again on pageshow", (t) => {
  const v = viewport(t, false);
  v.geometry.innerHeight = 500;
  v.mount();
  assert.equal(v.styles.get("--window-h"), "500px");
  v.geometry.innerHeight = 900;
  v.win.dispatchEvent(new Event("pageshow"));
  assert.equal(v.styles.get("--window-h"), "900px");
  assert.equal(v.data.window, undefined);
  assert.deepEqual(v.listeners(), [0, 0, 1, 1, 1]);
});

await test("W21-R13 leaving immediately restores root styles without a pending callback", (t) => {
  const v = viewport(t);
  const stop = v.mount();
  assert.equal(v.classes.has("fills-window"), true);
  stop?.();
  assert.equal(v.frames.size, 0);
  assert.deepEqual(v.listeners(), [0, 0, 0, 0, 0]);
  assert.equal(v.classes.has("fills-window"), false);
  assert.equal(v.styles.has("--window-h"), false);
  v.readsAndWrites.length = 0;
  v.flush();
  assert.deepEqual(v.readsAndWrites, [], "no later callback can fit an ordinary page");
});

await test("W21-R13 leaving after fitting restores ordinary page scrolling and removes listeners", (t) => {
  const v = viewport(t);
  v.geometry.height = 400;
  const stop = v.mount();
  stop?.();
  assert.equal(v.classes.has("fills-window"), false);
  assert.equal(v.styles.has("--window-h"), false);
  assert.equal(v.data.window, undefined);
  assert.deepEqual(v.listeners(), [0, 0, 0, 0, 0]);
  v.readsAndWrites.length = 0;
  v.geometry.height = 900;
  v.win.dispatchEvent(new Event("resize"));
  v.view.dispatchEvent(new Event("resize"));
  assert.deepEqual(v.readsAndWrites, []);
});

await test("W21-R13 a disposed controller cannot change the next full-height page", (t) => {
  const v = viewport(t);
  const stopOld = v.mount();
  const oldResize = getEventListeners(v.view, "resize")[0] as () => void;
  stopOld?.();
  v.geometry.height = 480;
  v.mount();
  v.readsAndWrites.length = 0;
  oldResize();
  stopOld?.();
  assert.deepEqual(v.readsAndWrites, [], "a stale event callback or repeated old cleanup cannot undo the current fit");
  assert.equal(v.classes.has("fills-window"), true);
  assert.equal(v.styles.get("--window-h"), "480px");
  assert.deepEqual(v.listeners(), [1, 1, 1, 1, 1]);
});

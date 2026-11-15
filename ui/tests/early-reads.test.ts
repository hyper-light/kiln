import assert from "node:assert/strict";
import { test } from "node:test";

// Mission control's time to interactive: a page's first reads are asked by the document as it's
// parsed (lib/early-script.ts) and handed to the app's own reads (lib/early.ts), and two reads of a
// URL at once share one request.

type Call = { url: string; init?: RequestInit };

/** A window whose fetch answers each URL with its name and counts the calls, and lib/early.ts installed in it (a fresh copy each time). */
async function page(early: Record<string, string> = {}) {
  const calls: Call[] = [];
  const timers: (() => void)[] = [];
  const fetch = (url: string, init?: RequestInit) => {
    calls.push({ url, init });
    return Promise.resolve(new Response(JSON.stringify({ url, n: calls.length })));
  };
  const win = {
    fetch,
    setTimeout: (fn: () => void) => timers.push(fn),
    __kilnEarly: Object.fromEntries(Object.entries(early).map(([u, body]) => [u, Promise.resolve(new Response(body))])),
  };
  Object.assign(globalThis, { window: win });
  const mod = (await import(`../src/lib/early.ts?${Math.random()}`)) as typeof import("../src/lib/early.ts");
  const read = async (url: string, init?: RequestInit) => (await (win.fetch as typeof fetch)(url, init)).text();
  return { win, calls, read, expire: () => timers.splice(0).forEach((t) => t()), earlyAnswer: mod.earlyAnswer };
}

test("the first render's reads take what the document asked, each its own copy; later reads go to the server", async () => {
  const p = await page({ "/api/agents": "early" });
  assert.equal(await p.read("/api/agents"), "early");
  assert.equal(await p.read("/api/agents", { method: "GET", credentials: "same-origin" }), "early");
  assert.equal(p.calls.length, 0);
  p.expire();
  assert.match(await p.read("/api/agents"), /"n":1/);
  assert.equal(p.calls.length, 1);
});

test("a change drops what the document asked: what was read before it may be out of date", async () => {
  const p = await page({ "/api/harness/sessions": "early" });
  await p.win.fetch("/api/harness/sessions/x/stop", { method: "POST", body: "{}" });
  assert.match(await p.read("/api/harness/sessions"), /"url":"\/api\/harness\/sessions"/);
  assert.equal(p.calls.length, 2);
});

test("two reads of a URL at once share one request; one given up leaves the other its answer", async () => {
  const p = await page();
  const gone = new AbortController();
  const a = p.win.fetch("/api/reach/runners", { signal: gone.signal });
  const b = p.win.fetch("/api/reach/runners");
  gone.abort();
  await assert.rejects(a);
  assert.match(await (await b).text(), /reach\/runners/);
  assert.equal(p.calls.length, 1);
});

test("anything but a plain read goes straight through", async () => {
  const p = await page({ "/api/me": "early" });
  await p.win.fetch("/api/me", { headers: { "x-kiln": "1" } });
  await p.win.fetch("/api/me", { cache: "no-cache" });
  await p.win.fetch("/healthz");
  assert.equal(p.calls.length, 3);
  assert.equal(await p.read("/api/me"), "early");
});

test("the sign-in check is handed the document's own answer, and a plain read of it still has one; a change takes it back", async () => {
  const p = await page({ "/api/me": "who" });
  const asked = p.earlyAnswer("/api/me");
  assert.ok(asked, "the document asked it");
  assert.equal(await (await asked).text(), "who");
  // Its own copy: the first render's plain read of the same URL is answered too, with no request.
  assert.equal(await p.read("/api/me"), "who");
  assert.equal(p.earlyAnswer("/api/agents"), null, "what the document didn't ask has none");
  assert.equal(p.calls.length, 0);
  // A change the page makes empties what the document asked: who this is may have changed with it.
  await p.win.fetch("/api/auth/logout", { method: "POST" });
  assert.equal(p.earlyAnswer("/api/me"), null);
});

test("the document asks every page's first reads and its own page's, under a peer's base; signed out, only who this is", async () => {
  const { EARLY, EARLY_SCRIPT, SIGNED } = await import("../src/lib/early-script.ts");
  const run = (pathname: string, signedOut: string | null) => {
    const asked: string[] = [];
    const scope = { location: { pathname }, localStorage: { getItem: () => signedOut }, fetch: (u: string) => (asked.push(u), Promise.resolve()) };
    const win: Record<string, unknown> = {};
    new Function("window", "location", "localStorage", "fetch", EARLY_SCRIPT)(win, scope.location, scope.localStorage, scope.fetch);
    assert.deepEqual(
      Object.keys(win.__kilnEarly as object),
      asked.map((u) => u.replace(/^\/\.kiln-peer\/[^/]+/, "")),
    );
    return asked;
  };
  // Signed in, every page asks the agents its shell shows (once: Mission control's list reads the same).
  assert.deepEqual(SIGNED, ["/api/harness/sessions"]);
  assert.ok(!EARLY["/agents"]!.includes("/api/harness/sessions"));
  assert.deepEqual(run("/agents/", null), ["/api/me", "/api/peers/open", ...SIGNED, ...EARLY["/agents"]!]);
  assert.deepEqual(run("/ship", null), ["/api/me", "/api/peers/open", ...SIGNED, "/api/ship/commits?by=repo&limit=100"]);
  assert.deepEqual(run("/ship/deploys", null), ["/api/me", "/api/peers/open", ...SIGNED]);
  assert.deepEqual(run("/agents/0123abcd", null), ["/api/me", "/api/peers/open", ...SIGNED], "an agent's page: its agents asked with the document");
  assert.deepEqual(run("/.kiln-peer/lab/agents", "1"), ["/.kiln-peer/lab/api/me", "/.kiln-peer/lab/api/peers/open"]);
});

test("W21-R02 Mission control's early reads are the URLs its composer asks for: the kinds written once, the runners and the machines lean", async () => {
  const { EARLY } = await import("../src/lib/early-script.ts");
  const { AGENTS_READ, KINDS_READ, REACH_READ } = await import("../src/api/launch-reads.ts");
  assert.ok(EARLY["/agents"]!.includes(KINDS_READ), KINDS_READ);
  assert.ok(EARLY["/agents"]!.includes(REACH_READ), REACH_READ);
  assert.ok(EARLY["/agents"]!.includes(AGENTS_READ), AGENTS_READ);
  assert.ok(!EARLY["/agents"]!.includes("/api/agents"), "the machines are asked for once, lean: not whole beside");
  assert.equal(KINDS_READ, "/api/harness/kinds?shared=1");
  assert.equal(REACH_READ, "/api/reach/runners?lean=1");
  assert.equal(AGENTS_READ, "/api/agents?lean=1");
});

test("Ship's early read is the URL its list asks for", async () => {
  const { SHIP_FIRST_READ, SHIP_ROWS } = await import("../src/api/ship-first.ts");
  const { shipSearch } = await import("../src/api/ship.ts");
  assert.equal(SHIP_FIRST_READ, `/api/ship/commits${shipSearch({ by: "repo", limit: SHIP_ROWS })}`);
});

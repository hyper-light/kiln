import assert from "node:assert/strict";
import { test } from "node:test";
import { readPrivateBootstrap } from "../src/server/private-bootstrap-data.server.ts";
import { bootstrapForPage, needsSessions } from "../src/lib/private-bootstrap.ts";
import { privateResponse } from "../server/private-response.ts";

const me = {
  authenticated: true,
  version: "test",
  hostname: "my kiln",
  kind: "user",
  groups: ["shop"],
  can: { "agents.view": "some" },
};
const request = (cookie = "session=one") =>
  new Request("https://untrusted-host.example/agents/one", {
    headers: { cookie, "x-invented": "never forwarded" },
  });
const json = (value: unknown) =>
  new Response(JSON.stringify(value), {
    headers: { "content-type": "application/json" },
  });

test("W21-R02: bootstrap credentials and identity stay request-local and the API origin is configured", async () => {
  const seen: { url: string; headers: Headers }[] = [];
  const fetcher: typeof fetch = async (input, init) => {
    const url = String(input),
      headers = new Headers(init?.headers);
    seen.push({ url, headers });
    assert.equal(init?.redirect, "error");
    assert.equal(init?.cache, "no-store");
    assert.equal(headers.get("x-invented"), null);
    return url.endsWith("/api/me")
      ? json({
          ...me,
          hostname: headers.get("cookie"),
          secret: "not serialized",
          grants: ["not carried"],
        })
      : json([{ id: "one", name: headers.get("cookie") }]);
  };
  const [one, two] = await Promise.all([
    readPrivateBootstrap(request("session=one"), "/agents/one", {
      apiOrigin: "http://127.0.0.1:7070",
      fetch: fetcher,
    }),
    readPrivateBootstrap(request("session=two"), "/agents/one", {
      apiOrigin: "http://127.0.0.1:7070",
      fetch: fetcher,
    }),
  ]);
  assert.equal(one?.me.hostname, "session=one");
  assert.equal(two?.me.hostname, "session=two");
  assert.equal(one?.sessions?.[0].name, "session=one");
  assert.equal(two?.sessions?.[0].name, "session=two");
  assert.ok(seen.every((r) => r.url.startsWith("http://127.0.0.1:7070/api/")));
  assert.equal("secret" in one!.me, false);
  assert.equal("grants" in one!.me, false);
  assert.equal(bootstrapForPage(one, "/agents/other"), false);
  assert.equal(bootstrapForPage(one, "/agents/one"), true);
});

test("W21-R02: anonymous, public and peered pages make no bootstrap API requests", async () => {
  let calls = 0;
  const options = {
    apiOrigin: "http://127.0.0.1:7070",
    fetch: (async () => {
      calls++;
      return json(me);
    }) as typeof fetch,
  };
  assert.equal(await readPrivateBootstrap(new Request("https://kiln.example/agents"), "/agents", options), null);
  assert.equal(await readPrivateBootstrap(request(), "/shared/plans/token", options), null);
  assert.equal(
    await readPrivateBootstrap(
      new Request("https://kiln.example/.kiln-peer/other/agents", {
        headers: { cookie: "session=one" },
      }),
      "/agents",
      options,
    ),
    null,
  );
  assert.equal(calls, 0);
  assert.equal(
    await readPrivateBootstrap(request(), "/agents", {
      ...options,
      apiOrigin: "not a URL",
    }),
    null,
  );
  assert.equal(needsSessions("/agents/plans/one"), false);
  assert.equal(needsSessions("/agents/research/one"), false);
  assert.equal(needsSessions("/agents/sessions"), false);
  assert.equal(needsSessions("/agents/one"), true);
});

test("W21-R02: refused/anonymous identity never fetches private route resources", async () => {
  let calls = 0;
  const fetcher: typeof fetch = async () => {
    calls++;
    return new Response("denied", { status: 401 });
  };
  assert.equal(
    await readPrivateBootstrap(request(), "/agents/one", {
      apiOrigin: "http://127.0.0.1:7070",
      fetch: fetcher,
    }),
    null,
  );
  assert.equal(calls, 1);
  const anon = await readPrivateBootstrap(request(), "/agents/one", {
    apiOrigin: "http://127.0.0.1:7070",
    fetch: async () => {
      calls++;
      return json({ ...me, authenticated: false });
    },
  });
  assert.equal(anon?.me.authenticated, false);
  assert.equal(anon?.sessions, undefined);
  assert.equal(calls, 2);
});

test("W21-R02: decoded byte budget cancels oversized route data but preserves verified identity", async () => {
  let cancelled = false,
    calls = 0;
  const fetcher: typeof fetch = async () => {
    if (++calls === 1) return json(me);
    return new Response(
      new ReadableStream({
        pull(controller) {
          controller.enqueue(new TextEncoder().encode("x".repeat(128)));
        },
        cancel() {
          cancelled = true;
        },
      }),
    );
  };
  const value = await readPrivateBootstrap(request(), "/agents/one", {
    apiOrigin: "http://127.0.0.1:7070",
    fetch: fetcher,
    maxBytes: 256,
  });
  assert.equal(value?.me.authenticated, true);
  assert.equal(value?.sessions, undefined);
  assert.equal(cancelled, true);
  assert.equal(calls, 2);
});

test("W21-R02: one deadline aborts a slow API read and falls back to browser authentication", async () => {
  let aborted = false;
  const fetcher: typeof fetch = (_input, init) =>
    new Promise((_resolve, reject) => {
      const guard = setTimeout(() => reject(new Error("deadline failed")), 1000);
      const abort = () => {
        aborted = true;
        clearTimeout(guard);
        reject(new Error("aborted"));
      };
      if (init?.signal?.aborted) abort();
      else init?.signal?.addEventListener("abort", abort, { once: true });
    });
  assert.equal(
    await readPrivateBootstrap(request(), "/agents/one", {
      apiOrigin: "http://127.0.0.1:7070",
      fetch: fetcher,
      timeoutMs: 5,
    }),
    null,
  );
  assert.equal(aborted, true);
});

test("W21-R02: dynamic response is private without dropping stream, transport or cookie headers", async () => {
  const response = privateResponse(
    new Response("private content", {
      headers: {
        "cache-control": "public,max-age=60",
        vary: "Accept",
        "set-cookie": "session=x; HttpOnly",
        "x-start-content-type": "test",
      },
    }),
  );
  assert.equal(response.headers.get("cache-control"), "private, no-store");
  assert.match(response.headers.get("vary")!, /Accept/);
  assert.match(response.headers.get("vary")!, /Cookie/);
  assert.match(response.headers.get("vary")!, /Authorization/);
  assert.equal(response.headers.get("set-cookie"), "session=x; HttpOnly");
  assert.equal(response.headers.get("x-start-content-type"), "test");
  assert.equal(await response.text(), "private content");
});

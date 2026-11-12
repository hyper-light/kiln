import { strict as assert } from "node:assert";
import { test } from "node:test";
import { agentModules } from "../scripts/entry-preloads";
import { agentEntry, preloadLinks, withEntryPreloads } from "../server/entry-preloads";
import { privateResponse } from "../server/private-response";

test("W21-R02 entry hints discover split agent code, omit shell imports and tolerate graph cycles", () => {
  const chunk = (fileName: string, imports: string[], facadeModuleId = "/ui/src/other.ts", isEntry = false, moduleIds = [facadeModuleId]) => ({
    fileName,
    imports,
    facadeModuleId,
    isEntry,
    moduleIds,
  });
  const shell = chunk("assets/index.js", ["assets/react.js"], "/ui/src/client.tsx", true, ["/ui/src/routes/_app.agents.$id.tsx"]);
  const route = chunk("assets/agent.js", ["assets/react.js", "assets/thread.js"], "/ui/src/routes/_app.agents.$id.tsx?tsr-split=component");
  assert.deepEqual(agentModules([shell, chunk("assets/react.js", []), route, chunk("assets/thread.js", ["assets/agent.js", "assets/external.js"])]), ["/assets/agent.js", "/assets/thread.js"]);
  assert.deepEqual(agentModules([shell]), []);
  const stub = chunk("assets/stub.js", [], "/ui/src/routes/_app.agents.$id.tsx");
  const implementation = chunk("assets/implementation.js", [], "/ui/src/virtual.ts", false, ["/ui/src/views/harness/agent.tsx"]);
  assert.deepEqual(agentModules([shell, stub, implementation]), ["/assets/implementation.js"]);
  const app = chunk("assets/app.js", ["assets/hooks.js"], "/ui/src/routes/_app.tsx?tsr-split=component");
  assert.deepEqual(agentModules([shell, app, { ...route, imports: [...route.imports, "assets/hooks.js"] }, chunk("assets/hooks.js", [])]), ["/assets/agent.js"]);
});

test("W21-R02 entry hints support Windows component facades without matching other agent pages", () => {
  const chunk = { fileName: "assets/agent.js", imports: [], moduleIds: [], isEntry: false, facadeModuleId: "C:\\ui\\src\\routes\\_app.agents.$id.tsx?tsr-split=component" };
  assert.deepEqual(agentModules([chunk]), ["/assets/agent.js"]);
  assert.deepEqual(agentModules([{ ...chunk, facadeModuleId: "/ui/src/routes/_app.agents.index.tsx?tsr-split=component" }]), []);
});

test("W21-R02 entry hints apply only to an agent document and accept bounded local JavaScript paths", () => {
  for (const path of ["/agents/a", "/agents/a/", "/agents/plan-123"]) assert.equal(agentEntry(path), true, path);
  for (const path of ["/agents", "/ship", "/agents/plans", "/agents/plans/a", "/agents/research", "/agents/sessions", "/agents/agentfiles", "/.kiln-peer/a/agents/b", "/shared/agents/a"])
    assert.equal(agentEntry(path), false, path);
  assert.equal(preloadLinks({ version: 1, agent: ["/assets/agent-123.js", "/assets/agent-123.js"] }), '<link rel="modulepreload" crossorigin="anonymous" href="/assets/agent-123.js">');
  assert.equal(
    preloadLinks({ version: 1, agent: ["/assets/icons~_app._~_app.agents._id-Csdk4V5P.js"] }),
    '<link rel="modulepreload" crossorigin="anonymous" href="/assets/icons~_app._~_app.agents._id-Csdk4V5P.js">',
  );
  for (const agent of [["https://elsewhere.test/code.js"], ["/assets/../private.js"], ['x" onload="alert(1)'], ["/assets/x.css"], Array(129).fill("/assets/x.js")])
    assert.equal(preloadLinks({ version: 1, agent }), "");
  assert.equal(preloadLinks({ version: 2, agent: ["/assets/x.js"] }), "");
});

function streamed(text: string, cuts: number[]) {
  const bytes = new TextEncoder().encode(text);
  return new Response(
    new ReadableStream<Uint8Array>({
      start(controller) {
        let at = 0;
        for (const end of [...cuts, bytes.length]) {
          controller.enqueue(bytes.slice(at, end));
          at = end;
        }
        controller.close();
      },
    }),
    { headers: { "content-type": "text/html; charset=utf-8", "content-length": String(bytes.length), etag: '"old-body"', "x-preserve": "yes" } },
  );
}

test("W21-R02 streaming hints preserve UTF-8 across every chunk boundary and follow existing shell hints", async () => {
  const html = '<html><head><title>kiln é 🔥</title><link rel="modulepreload" href="/assets/shell.js"></head><body>漢字 🪵</body></html>';
  const links = preloadLinks({ version: 1, agent: ["/assets/agent.js"] });
  const length = new TextEncoder().encode(html).length;
  for (let cut = 1; cut < length; cut++) {
    const response = privateResponse(withEntryPreloads(streamed(html, [cut]), links));
    assert.equal(await response.text(), html.replace("</head>", links + "</head>"), "byte split " + cut);
    assert.equal(response.headers.get("content-length"), null);
    assert.equal(response.headers.get("etag"), null);
    assert.equal(response.headers.get("cache-control"), "private, no-store");
    assert.match(response.headers.get("vary") ?? "", /Cookie/);
    assert.equal(response.headers.get("x-preserve"), "yes");
  }
});

test("W21-R02 an absent or oversized head passes through intact without inventing markup", async () => {
  for (const html of ["not a head é 🔥", "<head>" + "x".repeat(65540) + "é</head><body>🪵</body>"]) {
    assert.equal(await withEntryPreloads(streamed(html, [65539]), "<link>").text(), html);
    assert.equal(await withEntryPreloads(streamed(html, []), "<link>").text(), html);
  }
});

test("W21-R02 entry hints forward existing shell preloads before the head finishes and preserve auth cookies", async () => {
  let writer: ReadableStreamDefaultController<Uint8Array>;
  const encode = new TextEncoder();
  const shell = '<html><head><link rel="modulepreload" href="/assets/shell.js">' + " ".repeat(128);
  const headers = new Headers({ "content-type": "text/html" });
  headers.append("set-cookie", "one=fixture; HttpOnly");
  headers.append("set-cookie", "two=fixture; Secure");
  const response = new Response(
    new ReadableStream<Uint8Array>({
      start(controller) {
        writer = controller;
        controller.enqueue(encode.encode(shell));
      },
    }),
    { headers },
  );
  const transformed = withEntryPreloads(response, '<link href="/assets/agent.js">');
  assert.deepEqual(transformed.headers.getSetCookie(), ["one=fixture; HttpOnly", "two=fixture; Secure"]);
  const reader = transformed.body!.getReader();
  const first = await reader.read();
  assert.match(new TextDecoder().decode(first.value), /href="\/assets\/shell.js"/);
  writer!.enqueue(encode.encode("</head><body>🔥</body></html>"));
  writer!.close();
  let rest = "";
  for (;;) {
    const next = await reader.read();
    if (next.done) break;
    rest += new TextDecoder().decode(next.value);
  }
  assert.equal(new TextDecoder().decode(first.value) + rest, shell + '<link href="/assets/agent.js"></head><body>🔥</body></html>');
});

test("W21-R02 non-HTML, encoded and bodyless responses retain their original response", () => {
  for (const response of [
    new Response("x", { headers: { "content-type": "application/json" } }),
    new Response("packed", { headers: { "content-type": "text/html", "content-encoding": "br" } }),
    new Response(null, { status: 204 }),
    new Response("<head></head>", { headers: { "content-type": "text/html" } }),
  ]) {
    const links = response.status === 200 && !response.headers.has("content-encoding") && response.headers.get("content-type") === "text/html" ? "" : "<link>";
    assert.equal(withEntryPreloads(response, links), response);
  }
});

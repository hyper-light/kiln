import assert from "node:assert/strict";
import { test } from "node:test";
import type { HEvent } from "../src/api/harness.ts";

const { enginesOf, heightAt, heightsFor, isShowPage, ownEngine, pageOf, shownTitle } = await import("../src/views/harness/page-of.ts");
const { isShowLive, liveOf } = await import("../src/views/harness/live-of.ts");
const { turnsOf } = await import("../src/views/harness/derive.ts");

const SAFARI = "Mozilla/5.0 (iPhone; CPU iPhone OS 26_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Mobile/15E148 Safari/604.1";
const CHROME = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36";
const FIREFOX = "Mozilla/5.0 (X11; Linux x86_64; rv:150.0) Gecko/20100101 Firefox/150.0";

test("a browser's own engine, as pages are measured in them", () => {
  assert.equal(ownEngine(SAFARI), "webkit");
  assert.equal(ownEngine(CHROME), "chromium");
  assert.equal(ownEngine(FIREFOX), "firefox");
});

test("a frame takes its own engine's height where one reaches its width, else Chromium's", () => {
  const heights: [number, number][] = [[320, 900], [390, 800], [640, 600], [728, 560], [1000, 400]];
  // A viewer in Safari measured it at 390; its machine's Firefox at every width.
  const page = { heights, engines: { webkit: [[390, 812]] as [number, number][], firefox: heights.map(([w, h]) => [w, h + 10]) as [number, number][] } };
  // At about 390, Safari's own; far from it, Chromium's.
  assert.equal(heightAt(heightsFor(page, SAFARI, 393), 393), 812);
  assert.equal(heightAt(heightsFor(page, SAFARI, 700), 700), 600);
  // Firefox's reach every width; Chromium's own are Chromium's.
  assert.equal(heightAt(heightsFor(page, FIREFOX, 700), 700), 610);
  assert.equal(heightAt(heightsFor(page, CHROME, 393), 393), 800);
  // Two of Safari's around it, near enough: theirs.
  const two = { heights, engines: { webkit: [[640, 610], [728, 570]] as [number, number][] } };
  assert.equal(heightAt(heightsFor(two, SAFARI, 700), 700), 610);
  // Nothing of its own: Chromium's.
  assert.equal(heightAt(heightsFor({ heights }, SAFARI, 393), 393), 800);
});

test("heights as kiln answers them, and none that aren't", () => {
  assert.deepEqual(enginesOf({ webkit: [[390, 812], [0, 4], "x", [1, 2, 3]], firefox: "no" }), { webkit: [[390, 812]], firefox: [] });
  assert.deepEqual(enginesOf(null), {});
});

const ID = "44eaaab55d5cefed526c485d51db5016";
const PATH = `/v/${ID}/home/ada/.cache/review/7-try-it.html`;
/** kiln's answer to `show_page` (`crate::reach::mcp`), on one line, as Devin's person was shown it. */
const ANSWER = JSON.stringify({
  kiln_page: {
    id: ID,
    title: "Try it: Review, end to end (clickable)",
    path: PATH,
    url: `http://192.168.1.77:7070${PATH}`,
    files: 4,
    heights: [
      [320, 1458],
      [390, 1184],
      [1144, 1004],
    ],
    engines: { firefox: [[390, 1184]] },
  },
  seen: { picture_path: "/home/ada/.local/share/kiln/agent/browser/pictures/preview-1.png", width: 390, height: 844, content_height: 1184, said: [], layout_shift: 0.0003, long_tasks: 0 },
});
/** Devin's call of it, as kiln keeps an ACP agent's (`crate::harness::acp`): no kind said, the call in its words. */
const DEVIN = {
  name: "tool",
  tool: "other",
  title: "Called show_page from kiln",
  input: { path: "/home/ada/.cache/review/7-try-it.html", title: "Try it: Review, end to end (clickable)" },
  status: "done",
  output: ANSWER,
} as const;
/** The same while kiln keeps the page: not answered yet. */
const KEEPING = { ...DEVIN, status: "running", output: undefined } as const;

test("a page an agent showed, whatever its harness calls kiln's show_page", () => {
  // Claude Code and Codex: kiln's MCP tool by its name (kiln reads its server and the tool's own name from it).
  assert.ok(isShowPage({ name: "mcp__kiln__show_page", tool: "mcp", title: "Try it", server: "kiln", tool_short: "show_page", output: ANSWER }));
  assert.ok(isShowPage({ name: "mcp__kiln_reach__show_page", tool: "mcp", title: "Try it" }));
  // Devin: in its words, a page from the moment it's called, and once kiln has answered.
  assert.ok(isShowPage(DEVIN));
  assert.ok(isShowPage(KEEPING));
  assert.equal(pageOf(DEVIN)?.path, PATH);
  // An ACP agent that words it otherwise: what kiln answered says it's a page.
  assert.ok(isShowPage({ name: "other", tool: "other", title: '{"title":"Try it"}', output: ANSWER }));
  // Not a page: kiln's other tools, another server's, a command that printed kiln's answer.
  assert.equal(isShowPage({ ...DEVIN, title: "Called preview_page from kiln" }), false);
  assert.equal(isShowPage({ name: "mcp__github__show_page", tool: "mcp", title: "github · show_page", server: "github", tool_short: "show_page", output: ANSWER }), false);
  assert.equal(isShowPage({ name: "execute", tool: "shell", title: "cat answer.json", output: ANSWER }), false);
});

test("every harness's show_page call is a page, from the moment it's called where its harness says so", () => {
  /** As kiln keeps an ACP agent's call (`crate::harness::acp`): its kind for a name, "tool" when it gave none. */
  const acp = (title: string, output?: string, name = "other") => ({ name, tool: "other", title, output }) as const;
  const kept = { claude: { name: "mcp__kiln__show_page", tool: "mcp", title: "Try it", server: "kiln", tool_short: "show_page", output: ANSWER } } as const;
  const calls = {
    // Claude Code and Codex: by name, kiln's answer on one line (`crate::reach::shown`).
    "Claude Code": [kept.claude],
    Codex: [{ ...kept.claude, title: "Database replication — interactive design preview" }],
    // Devin: in its words.
    Devin: [KEEPING, DEVIN],
    // Gemini CLI: its display name for the tool; while it runs, its "explanation" (the call's input as JSON) is
    // all its output; then its answer's text, and its picture in words.
    "Gemini CLI": [acp("show_page (kiln MCP Server)", '{"title":"Try it","html":"<p>kiln_page</p>"}'), acp("show_page (kiln MCP Server)", `${ANSWER}\n[Image: image/png]`)],
    // opencode: the tool's own id while it runs; done, its title is empty, so kiln keeps its kind as its title.
    opencode: [acp("kiln_show_page"), acp("other", ANSWER)],
    // Amp (amp-acp): the tool's name and its first argument.
    Amp: [acp("mcp__kiln__show_page: Try it"), acp("mcp__kiln__show_page: Try it", ANSWER)],
  };
  for (const [harness, shown] of Object.entries(calls)) for (const c of shown) assert.ok(isShowPage(c), `${harness}: ${c.title}`);
  assert.equal(pageOf(calls["Gemini CLI"][0]!), null, "Gemini CLI: its input isn't kiln's answer");
  assert.equal(pageOf(calls["Gemini CLI"][1]!)?.id, ID);
  // Goose has a model title each call ("kiln: show page" until it does): its calls are pages by kiln's answer.
  assert.equal(isShowPage(acp("showing the harness check page", undefined, "tool")), false);
  assert.ok(isShowPage(acp("showing the harness check page", ANSWER, "tool")));
  // Words like these on a call of the harness's own tools, or another server's, aren't kiln's.
  assert.equal(isShowPage({ name: "execute", tool: "shell", title: "kiln_show_page" }), false);
  assert.equal(isShowPage(acp("show_page (github MCP Server)")), false);
  assert.equal(isShowPage(acp("Called show_page from kilnx")), false);
});

test("kiln's page answer, however a harness passes it on", () => {
  const at = (output: string) => pageOf({ output })?.id ?? null;
  assert.equal(at(ANSWER), ID);
  // Quoted as text again (an ACP agent's raw output, as Devin's can be).
  assert.equal(at(JSON.stringify(ANSWER)), ID);
  // Its picture said in words on a line of its own; laid out in a fence.
  assert.equal(at(`${ANSWER}\n[image/png]`), ID);
  assert.equal(at("```json\n" + JSON.stringify(JSON.parse(ANSWER), null, 2) + "\n```"), ID);
  // MCP's whole result: the answer quoted in its text, and structured after it, even with its middle cut.
  const whole = JSON.stringify({
    content: [
      { type: "text", text: ANSWER },
      { type: "image", data: "iVBORw0KGgo", mimeType: "image/png" },
    ],
    structuredContent: JSON.parse(ANSWER),
    isError: false,
  });
  assert.equal(at(whole), ID);
  assert.equal(at(`${whole.slice(0, 60)}\n…\n${whole.slice(whole.indexOf('"structuredContent"'))}`), ID);
  // Nothing else: an error, an answer cut short, an id that isn't a page's.
  assert.equal(at('{"error": "kiln couldn\'t keep the page"}'), null);
  assert.equal(at(ANSWER.slice(0, 60)), null);
  assert.equal(at(ANSWER.replace(`"id":"${ID}"`, '"id":"../../etc"')), null);
});

test("a page or live page being shown goes by its own title, not its harness's words for the call", () => {
  assert.equal(shownTitle(KEEPING, "A page"), "Try it: Review, end to end (clickable)");
  assert.equal(shownTitle({ title: "Called show_page from kiln", input: '{"title": "Try it", "path": "/w/a.html"}' }, "A page"), "Try it");
  assert.equal(shownTitle({ title: "Retry latency by attempt", input: { html: "<p>" } }, "A page"), "Retry latency by attempt");
  assert.equal(shownTitle({ title: "", input: null }, "A live page"), "A live page");
  // A harness's words for the call aren't the page's title.
  assert.equal(shownTitle({ title: "Called show_page from kiln", input: { html: "<p>" } }, "A page"), "A page");
  assert.equal(shownTitle({ title: "show_page (kiln MCP Server)", input: null }, "A page"), "A page");
});

test("a live page an agent showed, whatever its harness calls kiln's show_live", () => {
  const stream = "9d6c1f0a2b3e4d5c6b7a8f9e0d1c2b3a";
  const answer = JSON.stringify({ kiln_stream: { id: stream, session: "01a1751c85587445b9454ce71b676cc0", url: "http://localhost:5173/", title: "The app" } });
  const devin = { name: "tool", tool: "other", title: "Called show_live from kiln", input: { url: "http://localhost:5173/", title: "The app" }, status: "done", output: answer } as const;
  const opening = { ...devin, status: "running", output: undefined } as const;
  assert.ok(isShowLive({ name: "mcp__kiln__show_live", tool: "web", title: "The app", server: "kiln", tool_short: "show_live" }));
  assert.ok(isShowLive(devin));
  assert.ok(isShowLive(opening));
  assert.equal(liveOf(devin)?.id, stream);
  assert.equal(liveOf({ output: JSON.stringify(answer) })?.id, stream);
  assert.ok(isShowLive({ name: "other", tool: "other", title: "show_live", output: answer }));
  assert.equal(isShowLive({ ...devin, title: "Called use_live from kiln" }), false);
  assert.equal(isShowLive({ name: "execute", tool: "shell", title: "cat stream.json", output: answer }), false);
  assert.equal(isShowPage(devin), false);
});

test("Devin's page is drawn in its turn, not a step that prints kiln's answer", () => {
  const events: HEvent[] = [
    { id: "u1", seq: 1, at: 1, kind: "user", text: "Show me Review, clickable." },
    { id: "t1", seq: 2, at: 2, by: "Devin", kind: "tool", tool_id: "toolu_015U3YPKjuXoYg3V4f2Z9TAV", started: 2, ended: 3, ...DEVIN },
    { id: "a1", seq: 3, at: 4, by: "Devin", kind: "assistant", text: "It's above: try the buttons." },
  ];
  assert.deepEqual(
    turnsOf(events).map((t) => t.items.map((i) => i.type)),
    [["page"]],
  );
});

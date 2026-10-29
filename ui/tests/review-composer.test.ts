import assert from "node:assert/strict";
import { registerHooks } from "node:module";
import { test } from "node:test";
import * as React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import type { Thread, ThreadKind } from "../src/api/review.ts";

// This tab's storage and timers, as a browser has them (drafts.test.ts).
const values = new Map<string, string>();
const timers = new Map<number, () => void>();
let nextTimer = 0;
const storage = {
  getItem: (key: string) => values.get(key) ?? null,
  setItem: (key: string, value: string) => void values.set(key, value),
  removeItem: (key: string) => void values.delete(key),
};
const browser = Object.assign(new EventTarget(), {
  setTimeout: (fn: () => void) => {
    const id = ++nextTimer;
    timers.set(id, fn);
    return id;
  },
  clearTimeout: (id: number) => timers.delete(id),
});
// A document too, as the other tests' browsers have: api/events.ts listens on it once it has a window.
Object.assign(globalThis, { window: browser, document: new EventTarget(), localStorage: storage, sessionStorage: storage });
const flush = () => {
  const queued = [...timers.values()];
  timers.clear();
  queued.forEach((fn) => fn());
};

// The real composer, on the agent chat's drafts store; its stylesheets need no CSS in Node.
const loader = registerHooks({
  load(url, context, nextLoad) {
    return url.endsWith(".css") ? { format: "module", source: "export {};", shortCircuit: true } : nextLoad(url, context);
  },
});
const { useDrafts } = await import("../src/views/harness/state.ts");
const { draftKey, fenceFor, insertSuggestion, replyAt, useDraftKinds } = await import("../src/views/ship/review/draft.ts");
const { ReviewComposer } = await import("../src/views/ship/review/composer.tsx");
const { CommentText } = await import("../src/views/ship/review/comment.tsx");
loader.deregister();

const ITEM = "kiln/D41";
const FILE = "crates/kiln-ci/src/matrix.rs";
const ON_42 = { start: 42, text: ["    let first = m.axes[0].values[0];"] };
const sent: { body: string; kind?: ThreadKind }[] = [];
type Props = Partial<React.ComponentProps<typeof ReviewComposer>> & Pick<React.ComponentProps<typeof ReviewComposer>, "at">;
/** The composer as it's drawn where it's written. */
const composer = (p: Props) =>
  renderToStaticMarkup(
    React.createElement(ReviewComposer, {
      placeholder: "Comment on line 42",
      onSend: async (body: string, kind?: ThreadKind) => void sent.push({ body, kind }),
      ...p,
    }),
  );
/** What its field holds, as drawn. */
const field = (html: string) => {
  const m = /<textarea[^>]*>([^<]*)<\/textarea>/.exec(html);
  assert.ok(m, html);
  return (m[1] ?? "").replaceAll("&lt;", "<").replaceAll("&gt;", ">").replaceAll("&quot;", '"').replaceAll("&#x27;", "'").replaceAll("&amp;", "&");
};
const has = (html: string, ...parts: string[]) => parts.forEach((p) => assert.ok(html.includes(p), `expected ${JSON.stringify(p)} in\n${html}`));
const lacks = (html: string, ...parts: string[]) => parts.forEach((p) => assert.ok(!html.includes(p), `did not expect ${JSON.stringify(p)} in\n${html}`));
const thread = (rest: Partial<Thread> = {}): Thread => ({
  id: "t1",
  version: 5,
  file: FILE,
  side: "new",
  start_line: 42,
  end_line: 42,
  state: "open",
  created_by: "user:kim",
  created_at: 1,
  outdated: false,
  now: null,
  kind: "question",
  comments: [{ id: "c1", author: "user:kim", body: "This panics on an empty matrix.", suggestion: null, created_at: 1, draft: false }],
  ...rest,
});

test("w26_r08 the composer offers a preview, Insert a suggestion, and a kind for a new thread (a question by default)", () => {
  const at = { item: ITEM, file: FILE, line: 42 };
  const fresh = composer({ at, lines: ON_42 });
  has(fresh, 'class="rvc cp"', 'class="cp-field"', 'class="hx-compose-input cp-mirror"', 'class="hx-compose-input cp-input"', 'placeholder="Comment on line 42 · markdown"');
  has(fresh, 'aria-label="Preview" aria-pressed="false"', 'aria-label="Insert a suggestion"', 'role="radiogroup" aria-label="Kind of comment"');
  has(fresh, 'data-k="question" aria-checked="true"', 'data-k="nit" aria-checked="false"', 'data-k="issue" aria-checked="false"', 'data-k="blocking" aria-checked="false"');
  // Nothing written: nothing to send.
  has(fresh, 'class="cp-send" disabled=""');
  // A reply has no kind (its thread has one); the Conversation has no lines to suggest for; nor has the old side.
  const reply = composer({ at: replyAt(ITEM, thread()), lines: ON_42, placeholder: "Reply" });
  lacks(reply, "radiogroup");
  has(reply, 'aria-label="Insert a suggestion"');
  const convo = composer({ at: { item: ITEM }, placeholder: "Leave a comment" });
  lacks(convo, "radiogroup", "Insert a suggestion");
  has(convo, 'aria-label="Preview"');
  lacks(composer({ at: { ...at, side: "old" }, lines: ON_42 }), "Insert a suggestion");
  // Closing is offered where the caller can close it.
  has(composer({ at, onCancel: () => undefined }), 'aria-label="Close"');
  lacks(fresh, 'aria-label="Close"');
});

test("w26_r15 drafts are kept by item, file and line, in the agent chat's drafts store, and typing changes only its own", () => {
  const at = { item: ITEM, file: FILE, line: 42 };
  const keys = [
    draftKey(at),
    draftKey({ ...at, item: "kiln/D42" }),
    draftKey({ ...at, file: "crates/kiln-ci/src/expand.rs" }),
    draftKey({ ...at, line: 43 }),
    draftKey({ ...at, side: "old" }),
    draftKey({ ...at, thread: "t1" }),
    draftKey({ item: ITEM }),
  ];
  assert.equal(new Set(keys).size, keys.length, "each place its own draft");
  assert.equal(draftKey({ ...at, side: "new" }), draftKey(at), "the new side is the default");
  useDrafts.getState().setDraft("agent-1", "a message to an agent");
  useDrafts.getState().setDraft(draftKey(at), "This panics on an empty matrix.");
  useDrafts.getState().setDraft(draftKey({ ...at, line: 43 }), "And here.");
  assert.equal(field(composer({ at })), "This panics on an empty matrix.");
  assert.equal(field(composer({ at: { ...at, line: 43 } })), "And here.");
  assert.equal(field(composer({ at: { ...at, line: 44 } })), "");
  // Typing on line 42: every other draft is what it was, the same value, so nothing that reads one renders.
  const before = { ...useDrafts.getState().drafts };
  for (const ch of " Return early.") useDrafts.getState().setDraft(draftKey(at), (useDrafts.getState().drafts[draftKey(at)] ?? "") + ch);
  const after = useDrafts.getState().drafts;
  for (const [k, v] of Object.entries(before)) if (k !== draftKey(at)) assert.equal(after[k], v, k);
  assert.equal(after[draftKey(at)], "This panics on an empty matrix. Return early.");
  assert.equal(after["agent-1"], "a message to an agent", "the agent chat's own draft is untouched");
  has(composer({ at }), 'class="cp-send" aria-label="Comment"');
});

test("w26_r15 a live update arriving while typing leaves the draft untouched", () => {
  const t = thread();
  const at = replyAt(ITEM, t);
  const typing = "Half a thought about `m.is_empty()` and <Legs>";
  useDrafts.getState().setDraft(draftKey(at), typing);
  assert.equal(field(composer({ at, lines: ON_42, placeholder: "Reply" })), typing);
  // A live update: a new revision moved the thread's lines, another reviewer answered, and it became blocking.
  const live = thread({
    now: { version: 6, start_line: 44, end_line: 44 },
    kind: "blocking",
    comments: [...t.comments, { id: "c2", author: "agent:review", body: "Agreed: return `Legs::skipped`.", suggestion: null, created_at: 2, draft: false }],
  });
  assert.equal(draftKey(replyAt(ITEM, live)), draftKey(at), "nothing a live update changes is in its key");
  const state = useDrafts.getState();
  const drawn = composer({ at: replyAt(ITEM, live), lines: { start: 44, text: ON_42.text }, placeholder: "Reply" });
  assert.equal(field(drawn), typing, "what's typed is what was typed");
  assert.equal(useDrafts.getState(), state, "drawing it never writes a draft");
  // The same for a new thread on a line, as a new revision of the item comes.
  const line = { item: ITEM, file: FILE, line: 42 };
  useDrafts.getState().setDraft(draftKey(line), "Mid-sentence");
  assert.equal(field(composer({ at: { ...line }, lines: { start: 42, text: ["    let first = m.axes.first();"] } })), "Mid-sentence");
});

test("w26_r15 a draft and its kind survive a reload", async () => {
  const at = { item: ITEM, file: FILE, line: 90 };
  useDrafts.getState().setDraft(draftKey(at), "sort_unstable is enough here.");
  useDraftKinds.getState().setKind(draftKey(at), "nit");
  has(composer({ at }), 'data-k="nit" aria-checked="true"', 'data-k="question" aria-checked="false"');
  flush();
  const drafts = await import("../src/views/harness/state.ts?reload");
  const kinds = await import("../src/views/ship/review/draft.ts?reload");
  assert.equal(drafts.useDrafts.getState().drafts[draftKey(at)], "sort_unstable is enough here.");
  assert.equal(kinds.useDraftKinds.getState().kinds[draftKey(at)], "nit");
  // A question is the default, so it isn't kept.
  useDraftKinds.getState().setKind(draftKey(at), "question");
  assert.equal(useDraftKinds.getState().kinds[draftKey(at)], undefined);
});

test("w26_r10 Insert a suggestion puts the lines picked in, on lines of their own, to edit", () => {
  assert.deepEqual(insertSuggestion("", 0, ["a"]), { text: "```suggestion\na\n```\n", caret: "```suggestion\na".length });
  assert.equal(insertSuggestion("Looks off:\n\nThanks", 10, ["x"]).text, "Looks off:\n```suggestion\nx\n```\n\nThanks");
  assert.equal(insertSuggestion("ab", 1, ["x", "y"]).text, "a\n```suggestion\nx\ny\n```\nb");
  assert.equal(insertSuggestion("end\n", 99, ["x"]).text, "end\n```suggestion\nx\n```\n");
  // A markdown file's own fences stay inside a longer one.
  assert.equal(fenceFor(["plain"]), "```");
  assert.equal(fenceFor(["```sh", "kiln run", "```"]), "````");
  assert.equal(fenceFor(["`````"]), "``````");
  // What it puts in reads back as one suggestion of those lines, shown against them.
  const lines = ["Run it:", "", "```sh", "kiln run", "```"];
  const { text } = insertSuggestion("Say how:", 8, lines);
  const html = renderToStaticMarkup(React.createElement(CommentText, { body: text, old: { start: 7, text: ["Run it."] } }));
  has(html, '<span class="md-ln" data-n="7" data-sign="−">Run it.</span>', ...lines.map((l, i) => `<span class="md-ln" data-n="${7 + i}" data-sign="+">${l}</span>`));
  // The preview of a draft holding one: the old line and the new by the file's numbers, nothing to apply.
  const draft = insertSuggestion("Return early:", 13, ["if m.is_empty() { return Ok(vec![]) }"]).text;
  const preview = renderToStaticMarkup(React.createElement(CommentText, { body: draft, old: ON_42 }));
  has(preview, 'data-n="42" data-sign="−"', 'data-n="42" data-sign="+">if m.is_empty() { return Ok(vec![]) }</span>');
  lacks(preview, "md-apply");
});

import assert from "node:assert/strict";
import { registerHooks } from "node:module";
import { test } from "node:test";
import * as React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import type { Suggestion } from "../src/api/review.ts";

// The real components; their stylesheets need no CSS in Node.
const loader = registerHooks({
  load(url, context, nextLoad) {
    return url.endsWith(".css") ? { format: "module", source: "export {};", shortCircuit: true } : nextLoad(url, context);
  },
});
const { CommentBody } = await import("../src/lib/markdown.tsx");
const { CommentText } = await import("../src/views/ship/review/comment.tsx");
const { serverSuggestion } = await import("../src/views/ship/review/suggestion.tsx");
loader.deregister();

/** A comment's markdown as it's drawn (docs/ci.md W26-R09). */
const md = (text: string) => renderToStaticMarkup(React.createElement(CommentBody, { text }));
/** A comment with its suggestions, as a thread on lines shows it (W26-R10). */
const comment = (props: { body: string; suggestions?: Suggestion[]; old?: { start: number; text: string[] } | null; onApply?: (i: number) => Promise<{ version: number }> }) =>
  renderToStaticMarkup(React.createElement(CommentText, props));
const has = (html: string, ...parts: string[]) => parts.forEach((p) => assert.ok(html.includes(p), `expected ${JSON.stringify(p)} in\n${html}`));
const lacks = (html: string, ...parts: string[]) => parts.forEach((p) => assert.ok(!html.includes(p), `did not expect ${JSON.stringify(p)} in\n${html}`));
/** Links open apart from kiln, as every link in kiln's markdown does. */
const a = (href: string, text: string, title?: string) => `<a href="${href}"${title ? ` title="${title}"` : ""} target="_blank" rel="noopener noreferrer">${text}</a>`;
const mention = (name: string) => `<span class="md-mention" data-mention="${name}">@${name}</span>`;
const suggestion = (s: Partial<Suggestion> & { lines: string[] }): Suggestion => ({ index: 0, field: false, applied: null, outdated: false, ...s });

// W26-AC05: the GitHub-flavoured cases render as GitHub renders them (the GFM spec's examples where it
// has them), kiln's own parts aside: links open apart, tables keep their columns' names, fenced code
// is numbered under its language and a copy icon.

test("w26_ac05 emphasis and strikethrough", () => {
  has(md("*em* _em_ **strong** __strong__ ***both***"), "<p><em>em</em> <em>em</em> <strong>strong</strong> <strong>strong</strong> <em><strong>both</strong></em></p>");
  has(md("snake_case_name stays"), "<p>snake_case_name stays</p>");
  // GFM spec, strikethrough (extension) examples 491 and 493.
  has(md("~~Hi~~ Hello, ~there~ world!"), "<p><del>Hi</del> Hello, <del>there</del> world!</p>");
  has(md("This will ~~~not~~~ strike."), "<p>This will ~~~not~~~ strike.</p>");
});

test("w26_ac05 code, escaped", () => {
  has(md("`a < b && c`"), "<p><code>a &lt; b &amp;&amp; c</code></p>");
  has(md("`` a`b ``"), "<p><code>a`b</code></p>");
});

test("w26_ac05 links and autolinks", () => {
  has(md('[kiln](https://example.com/kiln "Kiln")'), `<p>${a("https://example.com/kiln", "kiln", "Kiln")}</p>`);
  has(md("[kiln][k]\n\n[k]: https://example.com/k"), `<p>${a("https://example.com/k", "kiln")}</p>`);
  has(md("<https://example.com>"), a("https://example.com", "https://example.com"));
  // GFM spec, autolinks (extension) examples 621, 625 and 631.
  has(md("www.commonmark.org"), `<p>${a("http://www.commonmark.org", "www.commonmark.org")}</p>`);
  has(
    md("Visit https://encrypted.google.com/search?q=Markup+(business)"),
    `<p>Visit ${a("https://encrypted.google.com/search?q=Markup+(business)", "https://encrypted.google.com/search?q=Markup+(business)")}</p>`,
  );
  has(md("foo@bar.baz"), `<p>${a("mailto:foo@bar.baz", "foo@bar.baz")}</p>`);
});

test("w26_r09 mentions, never in code, a link or an address", () => {
  has(md("@kim, please look (cc @ada)"), `<p>${mention("kim")}, please look (cc ${mention("ada")})</p>`);
  has(md("@kim's call, then @kim.lee."), `${mention("kim")}&#x27;s call, then ${mention("kim.lee")}.`);
  has(md("# Over to @kim"), `<h1>Over to ${mention("kim")}</h1>`);
  has(md("- [ ] @ada checks it"), `${mention("ada")} checks it`);
  const not = md("ada@example.com `@kim` [@kim](https://example.com) @kim/repo a@@b");
  lacks(not, "md-mention");
  has(not, a("mailto:ada@example.com", "ada@example.com"), "<code>@kim</code>", a("https://example.com", "@kim"), "@kim/repo");
});

test("w26_ac05 headings", () => {
  const h = md("# One\n\n## Two\n\n### Three\n\n#### Four\n\n##### Five\n\n###### Six");
  ["One", "Two", "Three", "Four", "Five", "Six"].forEach((w, i) => has(h, `<h${i + 1}>${w}</h${i + 1}>`));
  has(md("Setext\n===\n\nAnd two\n---"), "<h1>Setext</h1>", "<h2>And two</h2>");
  has(md("#7 is no heading\n\n####### nor seven"), "<p>#7 is no heading</p>", "<p>####### nor seven</p>");
});

test("w26_ac05 lists", () => {
  has(md("- a\n- b"), "<ul>\n<li>a</li>\n<li>b</li>\n</ul>");
  has(md("3. c\n4. d"), '<ol start="3">\n<li>c</li>\n<li>d</li>\n</ol>');
  has(md("- a\n  - b"), "<ul>\n<li>a\n<ul>\n<li>b</li>\n</ul>\n</li>\n</ul>");
  has(md("- loose\n\n- list"), "<li>\n<p>loose</p>\n</li>");
});

test("w26_ac05 task lists", () => {
  // GFM spec, task list items (extension) example 279.
  has(
    md("- [ ] foo\n- [x] bar"),
    '<ul class="contains-task-list">\n<li class="task-list-item"><input type="checkbox" disabled=""/> foo</li>\n<li class="task-list-item"><input type="checkbox" disabled="" checked=""/> bar</li>\n</ul>',
  );
});

test("w26_ac05 tables", () => {
  // GFM spec, tables (extension) examples 198, 199 and 200.
  has(
    md("| foo | bar |\n| --- | --- |\n| baz | bim |"),
    '<table><thead><tr><th>foo</th><th>bar</th></tr></thead><tbody><tr><td data-label="foo">baz</td><td data-label="bar">bim</td></tr></tbody></table>',
  );
  has(
    md("| abc | defghi |\n:-: | -----------:\nbar | baz"),
    '<th style="text-align:center">abc</th><th style="text-align:right">defghi</th>',
    '<td data-label="abc" style="text-align:center">bar</td>',
  );
  const escaped = md("| f\\|oo  |\n| ------ |\n| b `\\|` az |\n| b **\\|** im |");
  has(escaped, "<th>f|oo</th>", '<td data-label="f|oo">b <code>|</code> az</td>', '<td data-label="f|oo">b <strong>|</strong> im</td>');
});

test("w26_ac05 quotes and rules", () => {
  has(md("> quoted\n> on two lines"), "<blockquote>\n<p>quoted\non two lines</p>\n</blockquote>");
  has(md("> outer\n>\n>> inner"), "<blockquote>\n<p>outer</p>\n<blockquote>\n<p>inner</p>\n</blockquote>\n</blockquote>");
  for (const rule of ["***", "---", "___", "- - -"]) has(md(`a\n\n${rule}\n\nb`), "<hr/>");
  lacks(md("a\n\n--\n\nb"), "<hr");
});

test("w26_r09 fenced code: its language and a copy icon at its top right, its lines numbered", () => {
  const html = md('```rust\nfn main() {\n    println!("<hi>");\n}\n```');
  has(html, '<div class="md-code" data-lang="rust">', '<span class="md-code-lang">rust</span>', 'aria-label="Copy code"');
  // The head comes first, the language then the copy icon: the icon is at its top right.
  const lang = html.indexOf("md-code-lang");
  const copy = html.indexOf('aria-label="Copy code"');
  const pre = html.indexOf("<pre");
  assert.ok(lang < copy && copy < pre, html);
  // Each line its number, its text escaped.
  has(html, '<span class="md-ln" data-n="1">fn main() {</span>', '<span class="md-ln" data-n="2">    println!(&quot;&lt;hi&gt;&quot;);</span>', '<span class="md-ln" data-n="3">}</span>');
  assert.equal(html.match(/class="md-ln"/g)?.length, 3);
  // No language: plain text; an empty last line is a line; tildes; the first word of the info string.
  has(md("```\nplain\n\n```"), '<span class="md-code-lang">text</span>', 'data-n="1">plain</span>', 'data-n="2"></span>');
  has(md("~~~python extra words\nprint(1)\n~~~"), '<span class="md-code-lang">python</span>', 'data-n="1">print(1)</span>');
  // A longer fence holds a shorter one as its text; indented code is numbered too.
  const outer = md("````md\n```sh\nkiln run\n```\n````");
  has(outer, '<span class="md-code-lang">md</span>', 'data-n="1">```sh</span>', 'data-n="3">```</span>');
  has(md("    indented\n    code"), 'data-n="1">indented</span>', 'data-n="2">code</span>');
});

/* ------------------------------------------------------------------ suggestions (W26-R10) */

const ON_42 = { start: 42, text: ["    let first = m.axes[0].values[0];"] };
const EMPTY_FIRST = "if m.is_empty() { return Ok(vec![]) }";
const apply = async () => ({ version: 6 });

test("w26_r10 a suggestion on matrix.rs:42 shows 42 − and 42 + with copy and Apply", () => {
  const html = comment({ body: `Return early:\n\n\`\`\`suggestion\n${EMPTY_FIRST}\n\`\`\``, suggestions: [suggestion({ lines: [EMPTY_FIRST] })], old: ON_42, onApply: apply });
  has(html, "<p>Return early:</p>", "suggested change", 'aria-label="Copy the suggestion"', '<button type="button" class="md-apply">');
  const minus = html.indexOf(`<span class="md-ln" data-n="42" data-sign="−">    let first = m.axes[0].values[0];</span>`);
  const plus = html.indexOf(`<span class="md-ln" data-n="42" data-sign="+">${EMPTY_FIRST}</span>`);
  assert.ok(minus > 0 && plus > minus, html);
  has(html, "Apply</button>");
});

test("w26_r10 then it says which revision applied it, or that it's outdated, and Apply goes", () => {
  const applied = comment({
    body: `\`\`\`suggestion\n${EMPTY_FIRST}\n\`\`\``,
    suggestions: [suggestion({ lines: [EMPTY_FIRST], applied: { version: 6, by: "user:ada", at: 1 } })],
    old: ON_42,
    onApply: apply,
  });
  has(applied, "applied in v6</span>", 'data-state="applied"');
  lacks(applied, "md-apply");
  const outdated = comment({ body: `\`\`\`suggestion\n${EMPTY_FIRST}\n\`\`\``, suggestions: [suggestion({ lines: [EMPTY_FIRST], outdated: true })], old: ON_42, onApply: apply });
  has(outdated, "outdated</span>", 'data-state="outdated"');
  lacks(outdated, "md-apply");
  // A draft or a preview: shown against its lines, nothing to apply.
  const draft = comment({ body: `\`\`\`suggestion\n${EMPTY_FIRST}\n\`\`\``, old: ON_42 });
  has(draft, 'data-n="42" data-sign="−"', 'data-n="42" data-sign="+"');
  lacks(draft, "md-apply");
});

test("w26_r10 several lines, several blocks, each by its own place", () => {
  const body = "Either\n\n```suggestion\nlet n = m.len();\n```\n\nor, keeping the loop:\n\n```rust\nfor r in m {}\n```\n\n```suggestion\nlet n = m.iter().count();\nlet _ = n;\nlet _ = 0;\n```\n";
  const all = [suggestion({ lines: ["let n = m.len();"] }), suggestion({ index: 1, lines: ["let n = m.iter().count();", "let _ = n;", "let _ = 0;"], applied: { version: 3, by: "user:kim", at: 1 } })];
  const html = comment({ body, suggestions: all, old: { start: 40, text: ["let mut n = 0;", "for _ in m { n += 1; }"] }, onApply: apply });
  // Old lines by the file's numbers, the new ones from the first of them.
  for (const [n, sign] of [
    [40, "−"],
    [41, "−"],
    [40, "+"],
    [41, "+"],
    [42, "+"],
  ] as const)
    has(html, `data-n="${n}" data-sign="${sign}"`);
  // The first block applies; the second says v3 applied it; the rust block between them is code.
  assert.ok(html.indexOf("md-apply") < html.indexOf("applied in v"), html);
  assert.equal(html.match(/md-apply/g)?.length, 1);
  has(html, '<span class="md-code-lang">rust</span>');
  // Matched by place and lines: a block the server reads otherwise doesn't apply.
  assert.equal(serverSuggestion(all, 1, all[1]!.lines)?.index, 1);
  assert.equal(serverSuggestion(all, 0, all[1]!.lines)?.index, 1, "by its lines when its place says otherwise");
  assert.equal(serverSuggestion(all, 0, ["something else"]), undefined);
  assert.equal(serverSuggestion([suggestion({ lines: ["x"], field: true })], 0, ["x"]), undefined, "the old field is no block of the body");
});

test("w26_r10 what isn't a suggestion of this comment shows as code", () => {
  // Quoting someone's suggestion suggests nothing; a suggestion written inside another block is its text.
  const quoted = comment({ body: "> ```suggestion\n> TWO\n> ```\n\nAgreed.", suggestions: [], old: ON_42, onApply: apply });
  has(quoted, '<span class="md-code-lang">suggestion</span>', 'data-n="1">TWO</span>');
  lacks(quoted, "data-sign", "md-apply");
  const howto = comment({ body: "Write it so:\n\n````markdown\n```suggestion\nnew line\n```\n````", suggestions: [], old: ON_42, onApply: apply });
  has(howto, '<span class="md-code-lang">markdown</span>', 'data-n="2">new line</span>');
  lacks(howto, "data-sign");
  // Without the lines it's on (the Conversation): numbered code.
  has(comment({ body: "```suggestion\nx\n```" }), '<span class="md-code-lang">suggestion</span>', 'data-n="1">x</span>');
});

test("w26_r10 an empty block takes the lines out; the old field shows as one block more", () => {
  const gone = comment({ body: "Drop it:\n\n```suggestion\n```", suggestions: [suggestion({ lines: [] })], old: ON_42, onApply: apply });
  has(gone, 'data-n="42" data-sign="−"', "md-apply");
  lacks(gone, 'data-sign="+"');
  const emptied = comment({ body: "```suggestion\n\n```", suggestions: [suggestion({ lines: [""] })], old: ON_42, onApply: apply });
  has(emptied, '<span class="md-ln" data-n="42" data-sign="+"></span>', "md-apply");
  const field = comment({ body: "shout it", suggestions: [suggestion({ lines: ["TWO"], field: true })], old: { start: 2, text: ["two"] }, onApply: apply });
  assert.ok(field.indexOf("<p>shout it</p>") < field.indexOf('data-n="2" data-sign="+">TWO</span>'), field);
  has(field, "md-apply");
});

/* ------------------------------------------------------------------ no raw HTML ever runs (W26-AC05) */

/** What kiln itself draws in a comment: anything else in the markup came from the comment. */
const OURS = new Set([
  "p",
  "em",
  "strong",
  "del",
  "code",
  "pre",
  "a",
  "span",
  "div",
  "h1",
  "h2",
  "h3",
  "h4",
  "h5",
  "h6",
  "ul",
  "ol",
  "li",
  "input",
  "table",
  "thead",
  "tbody",
  "tr",
  "th",
  "td",
  "blockquote",
  "hr",
  "br",
  "button",
  "svg",
  "path",
  "rect",
  "wbr",
]);

/** A tag as React writes it: every attribute's value in double quotes, its own quotes escaped. */
const TAG = /<([a-zA-Z][\w-]*)((?:\s+[^\s"'>/=]+(?:="[^"]*")?)*)\s*\/?>/g;

function inert(html: string) {
  const tags = [...html.matchAll(TAG)];
  // Every tag opened is read here: text the comment wrote can't open one (its `<` is `&lt;`).
  assert.equal(tags.length, html.match(/<[a-zA-Z]/g)?.length ?? 0, html);
  for (const [, tag = "", attrs = ""] of tags) {
    assert.ok(OURS.has(tag.toLowerCase()), `<${tag}> in\n${html}`);
    for (const [, name = "", value = ""] of attrs.matchAll(/\s([^\s"'>/=]+)(?:="([^"]*)")?/g)) {
      assert.doesNotMatch(name, /^on/i, `an event handler (${name}) on <${tag}> in\n${html}`);
      if (/^(?:href|src|action|formaction|xlink:href)$/i.test(name)) assert.doesNotMatch(value, /^\s*(?:javascript|vbscript|data):/i, `${name}="${value}" in\n${html}`);
    }
  }
}

test("w26_ac05 script and iframe injections are shown as written, never run", () => {
  const attacks = [
    "<script>alert(1)</script>",
    "Hello <script>alert(1)</script> there",
    '<iframe src="https://evil.example/"></iframe>',
    'Before <iframe srcdoc="<script>alert(1)</script>"></iframe> after',
    '<img src="x" onerror="alert(1)">',
    '<svg onload="alert(1)"><circle r="1"/></svg>',
    '<a href="javascript:alert(1)">x</a>',
    "<style>body { display: none }</style>",
    '<object data="x.swf"></object> <embed src="x.swf">',
    '<form action="https://evil.example"><input name="q"><button formaction="javascript:alert(1)">go</button></form>',
    '<details open ontoggle="alert(1)">x</details>',
    '<math><mi xlink:href="javascript:alert(1)">x</mi></math>',
    "[x](javascript:alert(1))",
    "[x](JaVaScRiPt:alert(1))",
    "[x](vbscript:msgbox(1))",
    "[x](data:text/html;base64,PHNjcmlwdD5hbGVydCgxKTwvc2NyaXB0Pg==)",
    "<javascript:alert(1)>",
    "![y](javascript:alert(2))",
    '[t](https://example.com "\\" onmouseover=\\"alert(1)")',
    "| a |\n|---|\n| <script>alert(1)</script> |",
    "- <iframe src=x></iframe>",
    "# <img src=x onerror=alert(1)>",
    "> <script>alert(1)</script>",
    "@<script>alert(1)</script>",
    "```html\n</code></pre><script>alert(1)</script>\n```",
  ];
  for (const text of attacks) inert(md(text));
  // What was written shows as written.
  has(md("<script>alert(1)</script>"), "&lt;script&gt;alert(1)&lt;/script&gt;");
  has(md('<iframe src="https://evil.example/"></iframe>'), "&lt;iframe src=&quot;https://evil.example/&quot;&gt;&lt;/iframe&gt;");
  has(md("```html\n</code></pre><script>alert(1)</script>\n```"), 'data-n="1">&lt;/code&gt;&lt;/pre&gt;&lt;script&gt;alert(1)&lt;/script&gt;</span>');
  // A link to a script goes nowhere.
  has(md("[x](javascript:alert(1))"), '<a href="" target="_blank" rel="noopener noreferrer">x</a>');
  // In a suggestion, against its lines, too.
  const sug = comment({ body: "```suggestion\n<script>alert(1)</script>\n```", suggestions: [suggestion({ lines: ["<script>alert(1)</script>"] })], old: ON_42, onApply: apply });
  inert(sug);
  has(sug, 'data-sign="+">&lt;script&gt;alert(1)&lt;/script&gt;</span>');
});

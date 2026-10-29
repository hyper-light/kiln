/**
 * kiln's markdown: GitHub-flavored (tables, lists, task lists, quotes, code), drawn the console's
 * way, a fenced block's code in its language's colours (components/code/highlighted.tsx). Text
 * only: HTML in it is never injected. No hooks, so it renders in the browser (the agent's live
 * conversation) and in server components (pages that send no markdown code at all).
 */
import * as React from "react";
import type { Element, ElementContent, Root, RootContent } from "hast";
import Markdown, { defaultUrlTransform, type Components } from "react-markdown";
import remarkGfm from "remark-gfm";
import { CopyBlock, CopyIcon } from "@/components/ui/copy-block";
import { CodeBlock } from "@/components/code/highlighted";

/** A table cell keeps its words whole (harness.css), so a long run in one (a path, a URL, a digest)
 *  gets places to wrap: after its slashes, then (where a piece is still long) its other separators,
 *  then every 20 characters. */
const LONG_RUN = /(\S{21,})/;
const AFTER_SLASHES = /[^/\\]*[/\\]+|[^/\\]+/g;
const AFTER_SEPARATORS = /[^._:?&=#@+,;|~-]*[._:?&=#@+,;|~-]+|[^._:?&=#@+,;|~-]+/g;
const splitLong = (s: string, at: RegExp) => (s.length > 20 ? (s.match(at) ?? [s]) : [s]);
const pieces = (run: string) =>
  splitLong(run, AFTER_SLASHES)
    .flatMap((p) => splitLong(p, AFTER_SEPARATORS))
    .flatMap((p) => p.match(/.{1,20}/gu) ?? [p]);
function breakRuns(n: Element) {
  n.children = n.children.flatMap((c): ElementContent[] => {
    if (c.type === "element") breakRuns(c);
    if (c.type !== "text" || !LONG_RUN.test(c.value)) return [c];
    const out: ElementContent[] = [];
    c.value.split(LONG_RUN).forEach((part, i) => {
      (i % 2 ? pieces(part) : [part]).forEach((value, j) => {
        if (j) out.push({ type: "element", tagName: "wbr", properties: {}, children: [] });
        if (value) out.push({ type: "text", value });
      });
    });
    return out;
  });
}

/** Marks tables for the narrow layout: each body cell carries its column's heading
 *  (`data-label`), and a table of three or more columns is `data-wide`. Long runs in its cells
 *  get places to wrap. */
function labelTables() {
  const text = (n: RootContent): string => (n.type === "text" ? n.value : "children" in n ? n.children.map(text).join("") : "");
  const kids = (n: Element | undefined, tag: string): Element[] => (n?.children ?? []).filter((c): c is Element => c.type === "element" && c.tagName === tag);
  const walk = (n: Root | Element) => {
    if (n.type === "element" && n.tagName === "table") {
      const labels = kids(kids(kids(n, "thead")[0], "tr")[0], "th").map(text);
      if (labels.length >= 3) n.properties = { ...n.properties, dataWide: "" };
      for (const row of kids(kids(n, "tbody")[0], "tr")) {
        kids(row, "td").forEach((td, i) => {
          td.properties = { ...td.properties, dataLabel: labels[i] ?? "" };
        });
      }
      breakRuns(n);
    }
    for (const c of n.children) if (c.type === "element") walk(c);
  };
  return (tree: Root) => walk(tree);
}

const REMARK = [remarkGfm];
const REHYPE = [labelTables];

/* ------------------------------------------------------------------ files on the agent's machine */

/** How a conversation shows a file one of its messages mentions (`views/harness/files.tsx`):
 *  `image` when the message shows it (`![…](…)`), else a link or a path in its words. */
export type LocalFile = React.ComponentType<{ path: string; image: boolean; children?: React.ReactNode }>;

/** Whether a link points at a file (a path, a `file:` address) rather than the web. */
export function isLocalHref(href: string): boolean {
  if (!href || href.startsWith("#") || href.startsWith("?") || href.startsWith("//")) return false;
  return /^file:/i.test(href) || !/^[a-z][a-z0-9+.-]*:/i.test(href);
}

/** A file link's path, decoded. */
export function hrefPath(href: string): string {
  const raw = href.replace(/^file:\/\/(localhost)?/i, "").split(/[?#]/)[0] ?? "";
  try {
    return decodeURIComponent(raw);
  } catch {
    return raw;
  }
}

/** Files of the kinds people show (pictures, recordings, sounds, documents): a full path to one in
 *  the words or in code is taken for a mention (as the server's `crate::harness::files::mentioned`),
 *  so prose about `/api/...` routes isn't. */
const SHOWN = "png|jpe?g|gif|webp|avif|heic|svg|bmp|mp4|webm|mov|mkv|mp3|wav|ogg|oga|flac|m4a|aac|pdf|log|md|csv";
const SHOWN_PATH = new RegExp(`^(?:~/|/(?!/))\\S*\\.(?:${SHOWN})$`, "i");
const PATH_IN_WORDS = new RegExp(`(^|[\\s(\\[{"'<*_])((?:~/|/(?!/))[^\\s()\\[\\]{}"'<>\`]*\\.(?:${SHOWN}))(?=$|[\\s)\\]}"'>,.;:!?*_])`, "gi");

type MdPoint = { line: number; column: number; offset?: number };
type MdNode = {
  type: string;
  value?: string;
  url?: string;
  lang?: string | null;
  position?: { start: MdPoint; end: MdPoint };
  data?: { hName?: string; hProperties?: Record<string, unknown> };
  children?: MdNode[];
};

function linksIn(value: string): MdNode[] {
  const out: MdNode[] = [];
  let last = 0;
  for (const m of value.matchAll(PATH_IN_WORDS)) {
    const start = (m.index ?? 0) + (m[1] ?? "").length;
    const path = m[2] ?? "";
    if (start > last) out.push({ type: "text", value: value.slice(last, start) });
    out.push({ type: "link", url: path, children: [{ type: "text", value: path }] });
    last = start + path.length;
  }
  if (out.length === 0) return [{ type: "text", value }];
  if (last < value.length) out.push({ type: "text", value: value.slice(last) });
  return out;
}

/** Paths to files people show, in the words or as code, become links (to the file). */
function remarkPaths() {
  const walk = (n: MdNode) => {
    if (!n.children) return;
    n.children = n.children.flatMap((c): MdNode[] => {
      if (["link", "linkReference", "code", "html", "definition", "image", "imageReference"].includes(c.type)) return [c];
      if (c.type === "inlineCode" && c.value && SHOWN_PATH.test(c.value)) return [{ type: "link", url: c.value, children: [c] }];
      if (c.type === "text" && c.value) return linksIn(c.value);
      walk(c);
      return [c];
    });
  };
  return (tree: MdNode) => walk(tree);
}

const REMARK_FILES = [remarkGfm, remarkPaths];
/** `file:` addresses stay (to be shown as files); everything else as react-markdown has it. */
const keepFiles = (url: string) => (/^file:/i.test(url) ? url : defaultUrlTransform(url));

const heading =
  (level: number) =>
  ({ children }: { children?: React.ReactNode }) => (
    <p className="hx-h" data-level={level}>
      {children}
    </p>
  );

const WebLink: NonNullable<Components["a"]> = ({ href, children }) => (
  <a href={href} target="_blank" rel="noopener noreferrer">
    {children}
  </a>
);

const MARKDOWN: Components = {
  a: WebLink,
  h1: heading(1),
  h2: heading(2),
  h3: heading(3),
  h4: heading(4),
  h5: heading(5),
  h6: heading(6),
  pre: ({ children }) => {
    const code = React.isValidElement<{ className?: string; children?: React.ReactNode }>(children) ? children : null;
    const body = String(code?.props.children ?? "").replace(/\n$/, "");
    const lang = /language-(\S+)/.exec(code?.props.className ?? "")?.[1];
    return (
      <div className="hx-pre-wrap">
        <div className="hx-block-head">
          <span className="hx-block-tag">{lang ?? "code"}</span>
          <CopyBlock text={body} />
        </div>
        <CodeBlock text={body} lang={lang} className={code?.props.className} />
      </div>
    );
  },
  table: ({ children, ...props }) => (
    <div className="hx-table">
      <table data-wide={"data-wide" in props || undefined}>{children}</table>
    </div>
  ),
  // Shown as a link: nothing loads from elsewhere on its own.
  img: ({ src, alt }) => (
    <a href={typeof src === "string" ? src : undefined} target="_blank" rel="noopener noreferrer">
      {alt || "image"}
    </a>
  ),
};

/** The components with files shown by `File` (one set per `File`, so they stay the same). */
const withFiles = new WeakMap<LocalFile, Components>();
function componentsFor(File: LocalFile): Components {
  let c = withFiles.get(File);
  if (!c) {
    c = {
      ...MARKDOWN,
      a: (props) =>
        typeof props.href === "string" && isLocalHref(props.href) ? (
          <File path={hrefPath(props.href)} image={false}>
            {props.children}
          </File>
        ) : (
          <a href={props.href} target="_blank" rel="noopener noreferrer">
            {props.children}
          </a>
        ),
      img: ({ src, alt }) =>
        typeof src === "string" && isLocalHref(src) ? (
          <File path={hrefPath(src)} image>
            {alt}
          </File>
        ) : (
          <a href={typeof src === "string" ? src : undefined} target="_blank" rel="noopener noreferrer">
            {alt || "image"}
          </a>
        ),
    };
    withFiles.set(File, c);
  }
  return c;
}

/** `files`: how a conversation shows the files its message mentions (none: paths stay words). */
export function MarkdownBody({ text, files }: { text: string; files?: LocalFile }) {
  if (files) {
    return (
      <Markdown remarkPlugins={REMARK_FILES} rehypePlugins={REHYPE} components={componentsFor(files)} urlTransform={keepFiles}>
        {text}
      </Markdown>
    );
  }
  return (
    <Markdown remarkPlugins={REMARK} rehypePlugins={REHYPE} components={MARKDOWN}>
      {text}
    </Markdown>
  );
}

/* ------------------------------------------------------------------ review comments (W26-R09) */

/** `@name`, GitHub's way: at the start or after a character no name or address has, ending on a
 *  letter or a digit, and not followed by a slash (a path) or another `@`. */
const MENTION = /(^|[^\w@])@([A-Za-z0-9](?:[A-Za-z0-9._-]*[A-Za-z0-9])?)(?![\w@/])/g;
/** Where a mention isn't one: code, links (an address is one already), and what isn't shown as words. */
const NOT_WORDS = ["link", "linkReference", "code", "inlineCode", "html", "definition", "image", "imageReference"];

function mentionsIn(value: string): MdNode[] {
  const out: MdNode[] = [];
  let last = 0;
  for (const m of value.matchAll(MENTION)) {
    const start = (m.index ?? 0) + (m[1] ?? "").length;
    const name = m[2] ?? "";
    if (start > last) out.push({ type: "text", value: value.slice(last, start) });
    out.push({ type: "text", value: `@${name}`, data: { hName: "span", hProperties: { className: ["md-mention"], dataMention: name } } });
    last = start + name.length + 1;
  }
  if (out.length === 0) return [{ type: "text", value }];
  if (last < value.length) out.push({ type: "text", value: value.slice(last) });
  return out;
}

/** Mentions in a comment's words are marked as such (W26-R09). */
function remarkMentions() {
  const walk = (n: MdNode) => {
    if (!n.children) return;
    n.children = n.children.flatMap((c): MdNode[] => {
      if (NOT_WORDS.includes(c.type)) return [c];
      if (c.type === "text" && c.value) return mentionsIn(c.value);
      walk(c);
      return [c];
    });
  };
  return (tree: MdNode) => walk(tree);
}

/** A suggestion block's lines (W26-R10), as kiln_ci::suggest reads them: none when the block is empty
 *  (its lines go), one empty line when it holds one (the line is emptied). */
function blockLines(node: MdNode, src: string): string[] {
  if (node.value) return node.value.split(/\r?\n/);
  const { start, end } = node.position ?? {};
  // Line by line as kiln_ci::suggest reads it (Rust's `lines`): a last line ending isn't one more line.
  const raw =
    start?.offset != null && end?.offset != null
      ? src
          .slice(start.offset, end.offset)
          .replace(/\r?\n$/, "")
          .split(/\r?\n/)
      : [];
  const fence = /^\s*(`{3,}|~{3,})/.exec(raw[0] ?? "")?.[1] ?? "```";
  const closed = raw.length > 1 && new RegExp(`^\\s*[${fence.charAt(0)}]{${fence.length},}\\s*$`).test(raw[raw.length - 1] ?? "");
  return raw.length - (closed ? 2 : 1) > 0 ? [""] : [];
}

/** A comment's `suggestion` blocks (W26-R10), numbered in order with their lines; a quoted one is
 *  another comment's suggestion, shown as code. */
function remarkSuggestions() {
  return (tree: MdNode, file: { value?: unknown }) => {
    const src = typeof file.value === "string" ? file.value : "";
    let n = 0;
    const walk = (node: MdNode) => {
      for (const c of node.children ?? []) {
        if (c.type === "blockquote") continue;
        if (c.type === "code" && c.lang === "suggestion") c.data = { ...c.data, hProperties: { ...c.data?.hProperties, dataSuggestion: n++, dataLines: JSON.stringify(blockLines(c, src)) } };
        walk(c);
      }
    };
    walk(tree);
  };
}

/** A fenced block of code in a comment (W26-R09): its language and a copy icon at its top right, and its
 *  lines numbered (the numbers are drawn, so they're never copied with the code). */
export function CommentCode({ body, lang }: { body: string; lang?: string }) {
  return (
    <div className="md-code" data-lang={lang}>
      <div className="md-code-head">
        <span className="md-code-lang">{lang ?? "text"}</span>
        <CopyIcon text={body} label="Copy code" />
      </div>
      <pre className="md-code-body">
        <code>
          {body.split("\n").map((line, i) => (
            <span key={i} className="md-ln" data-n={i + 1}>
              {line}
            </span>
          ))}
        </code>
      </pre>
    </div>
  );
}

/** How a comment shows one of its suggestion blocks (W26-R10): which of them it is, from 0, and its lines. */
export type SuggestionView = React.ComponentType<{ n: number; lines: string[] }>;

type CodeProps = { className?: string; children?: React.ReactNode; "data-suggestion"?: number | string; "data-lines"?: string };

const commentPre =
  (Suggestion: SuggestionView | undefined): NonNullable<Components["pre"]> =>
  ({ children }) => {
    const code: CodeProps = React.isValidElement<CodeProps>(children) ? children.props : {};
    const n = code["data-suggestion"];
    if (Suggestion && n != null) return <Suggestion n={Number(n)} lines={JSON.parse(code["data-lines"] ?? "[]") as string[]} />;
    return <CommentCode body={String(code.children ?? "").replace(/\n$/, "")} lang={/language-(\S+)/.exec(code.className ?? "")?.[1]} />;
  };

/** A comment's link keeps its title, as GitHub's do. */
const CommentLink: NonNullable<Components["a"]> = ({ href, title, children }) => (
  <a href={href} title={title} target="_blank" rel="noopener noreferrer">
    {children}
  </a>
);

const COMMENT_REMARK = [remarkGfm, remarkMentions, remarkSuggestions];
const commentComponents = (Suggestion?: SuggestionView): Components => ({ a: CommentLink, table: MARKDOWN.table, img: MARKDOWN.img, pre: commentPre(Suggestion) });
const COMMENT = commentComponents();
/** The components with suggestions shown by each view (one set per view, so they stay the same). */
const withSuggestion = new WeakMap<SuggestionView, Components>();

/**
 * A review comment (W26-R09): GitHub-flavoured markdown (emphasis, strikethrough, code, links and
 * autolinks, mentions, headings, lists, task lists, tables, quotes, rules), its fenced code numbered
 * with its language and a copy icon. Text only: HTML in it shows as it was written and never runs.
 * `suggestion`: how its suggestion blocks show (none: as code). Drawn by views/ship/review/review.css.
 */
export function CommentBody({ text, suggestion }: { text: string; suggestion?: SuggestionView }) {
  let components = COMMENT;
  if (suggestion) {
    components = withSuggestion.get(suggestion) ?? commentComponents(suggestion);
    withSuggestion.set(suggestion, components);
  }
  return (
    <Markdown remarkPlugins={COMMENT_REMARK} rehypePlugins={REHYPE} components={components}>
      {text}
    </Markdown>
  );
}

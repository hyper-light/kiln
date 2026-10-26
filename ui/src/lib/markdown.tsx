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
import { CopyBlock } from "@/components/ui/copy-block";
import { Highlighted } from "@/components/code/highlighted";

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

type MdNode = { type: string; value?: string; url?: string; children?: MdNode[] };

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
        <pre className="hx-pre" data-lang={lang}>
          <code className={code?.props.className}>
            <Highlighted text={body} lang={lang} />
          </code>
        </pre>
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

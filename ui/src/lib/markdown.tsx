/**
 * kiln's markdown: GitHub-flavored (tables, lists, task lists, quotes, code), drawn the console's
 * way. Text only: HTML in it is never injected. No hooks, so it renders in the browser (the agent's
 * live conversation) and in server components (pages that send no markdown code at all).
 */
import * as React from "react";
import type { Element, ElementContent, Root, RootContent } from "hast";
import Markdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";
import { CopyBlock } from "@/components/ui/copy-block";

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
  const kids = (n: Element | undefined, tag: string): Element[] =>
    (n?.children ?? []).filter((c): c is Element => c.type === "element" && c.tagName === tag);
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

const heading =
  (level: number) =>
  ({ children }: { children?: React.ReactNode }) => (
    <p className="hx-h" data-level={level}>
      {children}
    </p>
  );

const MARKDOWN: Components = {
  a: ({ href, children }) => (
    <a href={href} target="_blank" rel="noopener noreferrer">
      {children}
    </a>
  ),
  h1: heading(1),
  h2: heading(2),
  h3: heading(3),
  h4: heading(4),
  h5: heading(5),
  h6: heading(6),
  pre: ({ children }) => {
    const code = React.isValidElement<{ className?: string; children?: React.ReactNode }>(children) ? children : null;
    const body = String(code?.props.children ?? "").replace(/\n$/, "");
    return (
      <div className="hx-pre-wrap">
        <CopyBlock text={body} />
        <pre className="hx-pre" data-lang={/language-(\S+)/.exec(code?.props.className ?? "")?.[1]}>
          {children}
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

export function MarkdownBody({ text }: { text: string }) {
  return (
    <Markdown remarkPlugins={REMARK} rehypePlugins={REHYPE} components={MARKDOWN}>
      {text}
    </Markdown>
  );
}

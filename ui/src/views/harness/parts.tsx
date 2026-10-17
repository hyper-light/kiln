/** Small shared pieces of the agent views: harness marks, status, tool icons, prose. */
import * as React from "react";
import "./harness.css";
import "./console.css";
import type { Element, Root, RootContent } from "hast";
import { Bot, FilePen, FileText, Globe, ListChecks, Map as MapIcon, MessageCircleQuestion, Plug, Search, Sparkles, SquareTerminal, Wrench, type LucideIcon } from "lucide-react";
import type { Status, ToolKind } from "@/api/harness";
import Markdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";
import { relativeTime } from "@/lib/format";
import { useNow } from "@/lib/hooks";
import { cn } from "@/lib/utils";
import { copyText } from "@/views/terminal/clipboard";

/** "12s ago" (`ts` in unix seconds), kept current by the shared clock: only this text re-renders as it counts, not the row around it. */
export function Ago({ ts, className }: { ts: number; className?: string }) {
  const now = useNow();
  return <span className={className}>{relativeTime(ts, now)}</span>;
}

/** A neutral two-letter mark per harness (never a vendor's logo). */
const MARKS: Record<string, { code: string; hue: string }> = {
  claude: { code: "CC", hue: "#d6b3a1" },
  codex: { code: "CX", hue: "#a9c7e8" },
  gemini: { code: "GM", hue: "#b7c4ef" },
  amp: { code: "AM", hue: "#e3b9c9" },
  antigravity: { code: "AG", hue: "#b9dccb" },
  opencode: { code: "OC", hue: "#d9d3a6" },
};

export function HarnessMark({ kind, name, size = 22, className }: { kind: string; name?: string; size?: number; className?: string }) {
  const m = MARKS[kind] ?? { code: kind.slice(0, 2).toUpperCase(), hue: "#b4b7c0" };
  return (
    <span
      className={cn("hx-mark", className)}
      style={{ width: size, height: size, fontSize: Math.round(size * 0.42), color: m.hue, borderColor: `${m.hue}55`, background: `${m.hue}14` }}
      title={name ?? kind}
      aria-label={name ?? kind}
    >
      {m.code}
    </span>
  );
}

const STATUS: Record<Status, { label: string; dot: string }> = {
  starting: { label: "Starting", dot: "queued" },
  idle: { label: "Ready", dot: "succeeded" },
  running: { label: "Working", dot: "running" },
  waiting: { label: "Needs you", dot: "timed_out" },
  exited: { label: "Stopped", dot: "cancelled" },
  disconnected: { label: "Offline", dot: "skipped" },
};

export function statusLabel(s: Status) {
  return STATUS[s]?.label ?? s;
}

export function StatusPill({ status, activity, className }: { status: Status; activity?: string | null; className?: string }) {
  const s = STATUS[status] ?? { label: status, dot: "skipped" };
  return (
    <span className={cn("hx-pill", className)} data-status={status}>
      <i className="dot" data-status={s.dot} aria-hidden="true" />
      <span>{s.label}</span>
      {activity && status === "running" && <span className="hx-pill-act">· {activity}</span>}
    </span>
  );
}

export const machineName = (target: string) => (target === "local" ? "server" : target.replace(/^agent:/, ""));

/** Status as the site shows a project's: a dot and a word. Someone waiting on you outranks the rest. */
export function StateWord({ a }: { a: { status: Status; approvals: readonly unknown[] } }) {
  const s = a.approvals.length > 0 ? "waiting" : a.status;
  const word = { waiting: "Needs you", running: "Working", starting: "Starting", idle: "Ready", exited: "Ended", disconnected: "Offline" }[s];
  return (
    <span className="st-word" data-state={s}>
      <i aria-hidden="true" />
      {word}
    </span>
  );
}

const ICONS: Record<ToolKind, LucideIcon> = {
  shell: SquareTerminal,
  edit: FilePen,
  read: FileText,
  search: Search,
  web: Globe,
  agent: Bot,
  skill: Sparkles,
  todo: ListChecks,
  question: MessageCircleQuestion,
  plan: MapIcon,
  mcp: Plug,
  other: Wrench,
};

export function ToolIcon({ kind, size = 14, className }: { kind: ToolKind; size?: number; className?: string }) {
  const I = ICONS[kind] ?? Wrench;
  return <I size={size} className={className} aria-hidden="true" />;
}

/** A small Copy button for a block of code or output. */
export function CopyBlock({ text, className }: { text: string; className?: string }) {
  const [done, setDone] = React.useState(false);
  return (
    <button
      type="button"
      className={cn("hx-copy", className)}
      onClick={() =>
        void copyText(text).then((ok) => {
          setDone(ok);
          if (ok) window.setTimeout(() => setDone(false), 1500);
        })
      }
    >
      {done ? "Copied" : "Copy"}
    </button>
  );
}

/* ------------------------------------------------------------------ prose */

/** Marks tables for the narrow layout: each body cell carries its column's heading
 *  (`data-label`), and a table of three or more columns is `data-wide`. */
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

/** The agent's prose: GitHub-flavored markdown (tables, lists, task lists, quotes, code), drawn the
 *  console's way. Text only: HTML in it is never injected. */
export const Prose = React.memo(function Prose({ text, className, ev }: { text: string; className?: string; /** The event it says (for find). */ ev?: string }) {
  return (
    <div className={cn("hx-prose", className)} data-ev={ev}>
      <Markdown remarkPlugins={REMARK} rehypePlugins={REHYPE} components={MARKDOWN}>
        {text}
      </Markdown>
    </div>
  );
});

/** Output a command printed: the end matters most, so long output opens at its tail. */
export function Output({ text, truncated, error }: { text: string; truncated?: boolean; error?: boolean }) {
  const ref = React.useRef<HTMLPreElement>(null);
  React.useLayoutEffect(() => {
    const el = ref.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [text]);
  return (
    <div className="hx-pre-wrap">
      {text && <CopyBlock text={text} />}
      <pre ref={ref} className="hx-out" data-error={error || undefined}>
        {truncated && <span className="hx-out-cut">… earlier output omitted …{"\n"}</span>}
        {text || " "}
      </pre>
    </div>
  );
}

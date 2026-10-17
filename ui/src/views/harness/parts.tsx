/** Small shared pieces of the agent views: harness marks, status, tool icons, prose. */
import * as React from "react";
import "./harness.css";
import { Bot, FilePen, FileText, Globe, ListChecks, Map as MapIcon, MessageCircleQuestion, Plug, Search, Sparkles, SquareTerminal, Wrench, type LucideIcon } from "lucide-react";
import type { Status, ToolKind } from "@/api/harness";
import { cn } from "@/lib/utils";
import { copyText } from "@/views/terminal/clipboard";

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

/** Inline markdown: `code`, **bold**, *em*, [links](url). Text only; nothing is injected as HTML. */
function inline(text: string, key: string): React.ReactNode[] {
  const out: React.ReactNode[] = [];
  const re = /(`[^`]+`|\*\*[^*]+\*\*|\*[^*\s][^*]*\*|\[[^\]]+\]\((https?:\/\/[^)\s]+)\))/g;
  let last = 0;
  let m: RegExpExecArray | null;
  let i = 0;
  while ((m = re.exec(text))) {
    if (m.index > last) out.push(text.slice(last, m.index));
    const t = m[0];
    const k = `${key}-${i++}`;
    if (t.startsWith("`")) out.push(<code key={k}>{t.slice(1, -1)}</code>);
    else if (t.startsWith("**")) out.push(<strong key={k}>{t.slice(2, -2)}</strong>);
    else if (t.startsWith("[")) {
      const label = t.slice(1, t.indexOf("]"));
      out.push(
        <a key={k} href={m[2]} target="_blank" rel="noopener noreferrer">
          {label}
        </a>,
      );
    } else out.push(<em key={k}>{t.slice(1, -1)}</em>);
    last = m.index + t.length;
  }
  if (last < text.length) out.push(text.slice(last));
  return out;
}

/** The agent's prose: paragraphs, headings, lists, quotes and code blocks. */
export const Prose = React.memo(function Prose({ text, className }: { text: string; className?: string }) {
  const blocks = React.useMemo(() => {
    const lines = text.replace(/\r\n/g, "\n").split("\n");
    const out: React.ReactNode[] = [];
    let i = 0;
    let n = 0;
    while (i < lines.length) {
      const l = lines[i] ?? "";
      const key = `b${n++}`;
      const fence = l.match(/^\s*```(\S*)/);
      if (fence) {
        const body: string[] = [];
        i++;
        while (i < lines.length && !/^\s*```/.test(lines[i] ?? "")) body.push(lines[i++] ?? "");
        i++;
        out.push(
          <div key={key} className="hx-pre-wrap">
            <CopyBlock text={body.join("\n")} />
            <pre className="hx-pre" data-lang={fence[1] || undefined}>
              <code>{body.join("\n")}</code>
            </pre>
          </div>,
        );
        continue;
      }
      if (/^\s*$/.test(l)) {
        i++;
        continue;
      }
      const h = l.match(/^(#{1,4})\s+(.*)$/);
      if (h) {
        out.push(
          <p key={key} className="hx-h">
            {inline(h[2] ?? "", key)}
          </p>,
        );
        i++;
        continue;
      }
      if (/^\s*([-*]|\d+[.)])\s+/.test(l)) {
        const ordered = /^\s*\d/.test(l);
        const items: string[] = [];
        while (i < lines.length && /^\s*([-*]|\d+[.)])\s+/.test(lines[i] ?? "")) items.push((lines[i++] ?? "").replace(/^\s*([-*]|\d+[.)])\s+/, ""));
        const List = ordered ? "ol" : "ul";
        out.push(
          <List key={key}>
            {items.map((it, j) => (
              <li key={j}>{inline(it, `${key}-${j}`)}</li>
            ))}
          </List>,
        );
        continue;
      }
      if (/^\s*>/.test(l)) {
        const q: string[] = [];
        while (i < lines.length && /^\s*>/.test(lines[i] ?? "")) q.push((lines[i++] ?? "").replace(/^\s*>\s?/, ""));
        out.push(<blockquote key={key}>{inline(q.join(" "), key)}</blockquote>);
        continue;
      }
      const para: string[] = [];
      while (i < lines.length && !/^\s*$/.test(lines[i] ?? "") && !/^\s*(```|#{1,4}\s|[-*]\s|\d+[.)]\s|>)/.test(lines[i] ?? "")) para.push(lines[i++] ?? "");
      out.push(<p key={key}>{inline(para.join(" "), key)}</p>);
    }
    return out;
  }, [text]);
  return <div className={cn("hx-prose", className)}>{blocks}</div>;
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

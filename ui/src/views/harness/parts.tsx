/** Small shared pieces of the agent views: harness marks, status, tool icons, prose. */
import * as React from "react";
import "./harness.css";
import "./console.css";
import { Bot, FilePen, FileText, Globe, ListChecks, Map as MapIcon, MessageCircleQuestion, Plug, Search, Sparkles, SquareTerminal, Wrench, type LucideIcon } from "lucide-react";
import type { Status, ToolKind } from "@/api/harness";
import { relativeTime } from "@/lib/format";
import { useNow } from "@/lib/hooks";
import { cn } from "@/lib/utils";
import { MarkdownBody } from "@/lib/markdown";
import { MessageFile, MessageSeq } from "./files";
import { CopyBlock } from "@/components/ui/copy-block";

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

/** Status as the site shows a project's: a dot and a word. Someone waiting on you outranks the rest.
 *  A conversation that already exists is resuming, not starting, while its harness comes up. */
export function StateWord({ a }: { a: { status: Status; approvals: readonly unknown[]; pause?: unknown; harness_session?: string } }) {
  // Paused for a usage limit: once a turn under way ends, it waits for the reset.
  const s = a.approvals.length > 0 ? "waiting" : a.pause && a.status !== "running" && a.status !== "exited" && a.status !== "disconnected" ? "paused" : a.status;
  const word = { waiting: "Needs you", running: "Working", starting: a.harness_session ? "Resuming" : "Starting", idle: "Ready", exited: "Ended", disconnected: "Offline", paused: "Paused" }[s];
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

export { CopyBlock };

/* ------------------------------------------------------------------ prose */


/** The agent's prose: GitHub-flavored markdown (tables, lists, task lists, quotes, code), drawn the
 *  console's way. Text only: HTML in it is never injected. */
export const Prose = React.memo(function Prose({
  text,
  className,
  ev,
  seq,
}: {
  text: string;
  className?: string;
  /** The event it says (for find). */
  ev?: string;
  /** A message's `seq`: the files it mentions show in it, as they were then (files.tsx). */
  seq?: number;
}) {
  return (
    <div className={cn("hx-prose", className)} data-ev={ev}>
      {seq == null ? (
        <MarkdownBody text={text} />
      ) : (
        <MessageSeq.Provider value={seq}>
          <MarkdownBody text={text} files={MessageFile} />
        </MessageSeq.Provider>
      )}
    </div>
  );
});

/** Output a command printed: the end matters most, so long output opens at its tail. */
export function Output({ text, truncated, error }: { text: string; truncated?: boolean; error?: boolean }) {
  // At its tail when it shows, and again as more comes.
  const ref = React.useCallback(
    (el: HTMLPreElement | null) => {
      void text;
      if (el) el.scrollTop = el.scrollHeight;
    },
    [text],
  );
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

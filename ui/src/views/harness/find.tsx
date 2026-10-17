/**
 * Find in an agent's conversation: what you asked, what it answered and thought, the commands it
 * ran and what they printed, the files it read and changed, its notices. The matches are found in
 * the conversation itself (folded steps included), counted, and shown one at a time where they are:
 * the step that holds one opens, it scrolls into view, and every match on the page is marked in
 * place. Newest first, as a conversation is read from its end; ↑ goes back in time.
 */
import * as React from "react";
import { ChevronDown, ChevronUp, Search, X } from "lucide-react";
import type { HEvent } from "@/api/harness";

/** The match being shown: its event, and how many times one has been shown (to open it again). */
export interface FindNow {
  id: string;
  n: number;
}

export const FindNowContext = React.createContext<FindNow | null>(null);

/**
 * Opens a fold when the match being shown is inside it, and keeps it open after (as a click would);
 * a click that closes it again wins until another match is shown there.
 */
export function useRevealed(holds: (id: string) => boolean, open: () => void) {
  const now = React.useContext(FindNowContext);
  const [seen, setSeen] = React.useState(0);
  if (now && now.n !== seen && holds(now.id)) {
    setSeen(now.n);
    open();
  }
}

/* ------------------------------------------------------------------ what an event says */

const said = new WeakMap<HEvent, string>();

/** An event's words as the conversation shows them, lower-cased once. */
function textOf(e: HEvent): string {
  let t = said.get(e);
  if (t !== undefined) return t;
  switch (e.kind) {
    case "user":
      t = [e.text, ...(e.attachments ?? [])].join("\n");
      break;
    case "assistant":
    case "notice":
      t = e.text;
      break;
    case "thinking":
      t = e.redacted ? "" : e.text;
      break;
    case "tool": {
      // What each kind of step shows (and so what can be found): a read's file and a search's
      // pattern, not what they returned; a command and its output; an edit's diff; a tool's
      // arguments and answer; a plan, a to-do list, a question and its answer.
      const input = e.input && typeof e.input === "object" ? (e.input as Record<string, unknown>) : null;
      const parts: (string | undefined)[] = [e.title, e.detail];
      if (e.tool === "shell" || e.tool === "agent" || e.tool === "question" || e.tool === "skill" || e.tool === "mcp" || e.tool === "other") parts.push(e.output);
      if (e.tool === "edit") {
        parts.push(...(e.diffs ?? []).map((d) => [d.path, ...d.hunks.flatMap((h) => h.lines.map((l) => l.slice(1)))].join("\n")));
        if (e.status === "error") parts.push(e.output);
      }
      if (e.server || e.tool === "other" || e.tool === "mcp") parts.push(JSON.stringify(e.input ?? null));
      if (typeof input?.plan === "string") parts.push(input.plan);
      if (Array.isArray(input?.questions)) parts.push(...(input.questions as { question?: string }[]).map((q) => q.question));
      parts.push(...(e.todos ?? []).map((x) => x.text));
      t = parts.filter(Boolean).join("\n");
      break;
    }
  }
  t = (t ?? "").toLowerCase();
  said.set(e, t);
  return t;
}

/** The events of one thread (the main one, or a subagent's) whose words hold `q`, in order. */
export function findIn(events: HEvent[], parent: string | undefined, q: string): string[] {
  const needle = q.trim().toLowerCase();
  if (!needle) return [];
  const out: string[] = [];
  for (const e of events) if ((e.parent ?? undefined) === parent && textOf(e).includes(needle)) out.push(e.id);
  return out;
}

/* ------------------------------------------------------------------ marking them in place */

const MAX_MARKS = 4000;

interface HighlightRegistry {
  set: (name: string, h: unknown) => void;
  delete: (name: string) => void;
}
const registry = (): HighlightRegistry | null => (typeof CSS !== "undefined" && "highlights" in CSS ? (CSS as unknown as { highlights: HighlightRegistry }).highlights : null);
const Highlight = (): (new (...r: Range[]) => unknown) | null => (globalThis as { Highlight?: new (...r: Range[]) => unknown }).Highlight ?? null;

/** The element of an event: the innermost one that names it. */
export function elementOf(root: HTMLElement, id: string): HTMLElement | null {
  const all = root.querySelectorAll<HTMLElement>(`[data-ev~="${CSS.escape(id)}"]`);
  return all.length ? (all[all.length - 1] ?? null) : null;
}

/**
 * Marks every place `q` shows on the page (and those in `current` as the one shown), with the CSS
 * Custom Highlight API: nothing in the page changes but how its text is painted. Returns the
 * first place in `current`, to bring into view.
 */
export function mark(root: HTMLElement, q: string, current: HTMLElement | null): Range | null {
  const needle = q.trim().toLowerCase();
  const reg = registry();
  const H = Highlight();
  if (!needle) {
    reg?.delete("kiln-find");
    reg?.delete("kiln-find-now");
    for (const el of root.querySelectorAll("[data-find-here]")) el.removeAttribute("data-find-here");
    return null;
  }
  const all: Range[] = [];
  const now: Range[] = [];
  const walk = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  for (let n = walk.nextNode(); n && all.length < MAX_MARKS; n = walk.nextNode()) {
    const text = n.nodeValue?.toLowerCase();
    if (!text) continue;
    const inside = current?.contains(n) ?? false;
    for (let i = text.indexOf(needle); i >= 0 && all.length < MAX_MARKS; i = text.indexOf(needle, i + needle.length)) {
      const r = new Range();
      r.setStart(n, i);
      r.setEnd(n, i + needle.length);
      all.push(r);
      if (inside) now.push(r);
    }
  }
  if (reg && H) {
    reg.set("kiln-find", new H(...all));
    reg.set("kiln-find-now", new H(...now));
  }
  // The words found aren't drawn as typed (a link's target, markdown's marks): its whole place is.
  for (const el of root.querySelectorAll("[data-find-here]")) if (el !== current || now.length) el.removeAttribute("data-find-here");
  if (current && now.length === 0) current.setAttribute("data-find-here", "");
  return now[0] ?? null;
}

/**
 * Brings a place into view: every scrolling box between it and `root` (a long command output, a
 * wide diff) moves it to its middle, then `root` does.
 */
export function bringIntoView(target: Range | HTMLElement, root: HTMLElement) {
  const rect = () => target.getBoundingClientRect();
  let el = target instanceof Range ? target.startContainer.parentElement : target.parentElement;
  while (el && el !== root && root.contains(el)) {
    const s = getComputedStyle(el);
    const y = el.scrollHeight > el.clientHeight + 1 && /(auto|scroll)/.test(s.overflowY);
    const x = el.scrollWidth > el.clientWidth + 1 && /(auto|scroll)/.test(s.overflowX);
    if (y || x) {
      const r = rect();
      const b = el.getBoundingClientRect();
      if (y) el.scrollTop += r.top - b.top - (el.clientHeight - r.height) / 2;
      if (x) el.scrollLeft += r.left - b.left - (el.clientWidth - r.width) / 2;
    }
    el = el.parentElement;
  }
  const r = rect();
  const b = root.getBoundingClientRect();
  if (root.scrollHeight > root.clientHeight + 1) root.scrollTop += r.top - b.top - (root.clientHeight - r.height) / 2;
  else window.scrollBy({ top: r.top - window.innerHeight / 2 });
}

/* ------------------------------------------------------------------ the bar */

/**
 * The find bar, over the conversation (in its flow, like the subagent crumbs): the words to find,
 * where you are among the matches, earlier and later, close.
 */
export function FindBar({
  q,
  onQ,
  index,
  total,
  onStep,
  onClose,
  partial,
  inputRef,
}: {
  q: string;
  onQ: (q: string) => void;
  /** Which match is shown (0 is the oldest), of `total`. */
  index: number;
  total: number;
  /** -1: an earlier match; 1: a later one. */
  onStep: (by: -1 | 1) => void;
  onClose: () => void;
  /** Only the latest events are loaded (earlier ones aren't searched). */
  partial?: number;
  inputRef: React.RefObject<HTMLInputElement | null>;
}) {
  const where = !q.trim() ? "" : total === 0 ? "No matches" : `${index + 1} of ${total}`;
  return (
    <div className="hx-find" role="search">
      <Search size={14} aria-hidden="true" className="hx-find-icon" />
      <input
        ref={inputRef}
        type="text"
        value={q}
        onChange={(e) => onQ(e.target.value)}
        placeholder="Find in this conversation"
        aria-label="Find in this conversation"
        autoComplete="off"
        spellCheck={false}
        enterKeyHint="search"
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === "ArrowUp" || e.key === "ArrowDown") {
            e.preventDefault();
            // Newest first: Enter and ↑ go back in time; Shift+Enter and ↓ come forward.
            onStep((e.key === "Enter" && !e.shiftKey) || e.key === "ArrowUp" ? -1 : 1);
          } else if (e.key === "Escape") {
            e.preventDefault();
            onClose();
          }
        }}
      />
      <span className="hx-find-where" aria-live="polite">
        {where}
        {where && partial ? <span className="hx-find-partial"> · latest {partial.toLocaleString()} events</span> : null}
      </span>
      <button type="button" className="hx-icon" aria-label="Earlier match" disabled={total === 0} onClick={() => onStep(-1)}>
        <ChevronUp size={15} />
      </button>
      <button type="button" className="hx-icon" aria-label="Later match" disabled={total === 0} onClick={() => onStep(1)}>
        <ChevronDown size={15} />
      </button>
      <button type="button" className="hx-icon" aria-label="Close find" onClick={onClose}>
        <X size={14} />
      </button>
    </div>
  );
}

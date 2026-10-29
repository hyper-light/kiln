/**
 * Where a review comment is written, and what's kept of it (docs/ci.md W26-R08, R15): its words in
 * the drafts store the agent chat shares (harness/state.ts), by the item, the file and the line it's
 * on (and the thread a reply answers), so a live update never touches a draft and a reload puts it
 * back; the kind chosen for a new thread beside it.
 */
import * as React from "react";
import { create } from "zustand";
import { createJSONStorage, persist } from "zustand/middleware";
import type { Thread, ThreadKind } from "@/api/review";
import { useDrafts } from "@/views/harness/state";

/** Where a comment is being written. */
export interface DraftAt {
  /** The Ship item it's on: `${repo}/${change}` (`kiln/D41`, `kiln/CR4`), or `${repo}/${sha}` for a commit with no change. */
  item: string;
  /** The file it's on (none: the change as a whole, its Conversation). */
  file?: string | null;
  /** The line it's on, by the file's own numbers. */
  line?: number | null;
  side?: "old" | "new";
  /** The thread it answers (none: a new thread). */
  thread?: string | null;
}

/** Its key in the drafts store. Nothing a live update changes is in it: no revision, no comment, not where a later revision moved the thread. */
export function draftKey(at: DraftAt): string {
  return `review:${JSON.stringify([at.item, at.file ?? null, at.line ?? null, at.side === "old" ? "old" : "new", at.thread ?? null])}`;
}

/** Where a reply to a thread is written: on the thread's own file and line, as it was made. */
export function replyAt(item: string, t: Pick<Thread, "id" | "file" | "side" | "start_line" | "end_line">): DraftAt {
  return { item, file: t.file, line: t.end_line ?? t.start_line, side: t.side, thread: t.id };
}

interface KindsState {
  kinds: Record<string, ThreadKind>;
  setKind: (key: string, kind: ThreadKind) => void;
}

/** The kind chosen for each new thread being written, by its draft's key: for this tab, across reloads. */
export const useDraftKinds = create<KindsState>()(
  persist(
    (set) => ({
      kinds: {},
      setKind: (key, kind) =>
        set((s) => {
          const kinds = { ...s.kinds };
          if (kind === "question") delete kinds[key];
          else kinds[key] = kind;
          return { kinds };
        }),
    }),
    {
      name: "kiln.review.kinds",
      storage: createJSONStorage(() => sessionStorage),
      partialize: (s) => ({ kinds: s.kinds }),
    },
  ),
);

/**
 * A draft's setter alone (W26-R15): no subscription, so the keystrokes that fill it never render
 * who holds this. For parts that write a draft they don't show (the ⋯ menu's Quote).
 */
export function useDraftSet(key: string): (text: string | ((was: string) => string)) => void {
  return React.useCallback((t: string | ((was: string) => string)) => useDrafts.getState().setDraft(key, typeof t === "function" ? t(useDrafts.getState().drafts[key] ?? "") : t), [key]);
}

/** A new thread's kind, by its draft's key: a question until another is chosen. */
export function useDraftKind(key: string): [ThreadKind, (kind: ThreadKind) => void] {
  const read = () => useDraftKinds.getState().kinds[key] ?? "question";
  const kind = React.useSyncExternalStore(useDraftKinds.subscribe, read, read);
  const set = React.useCallback((k: ThreadKind) => useDraftKinds.getState().setKind(key, k), [key]);
  return [kind, set];
}

/** The fence a suggestion of `lines` needs: longer than any run of backticks in them (a markdown file's own fences stay inside it), three at least. */
export function fenceFor(lines: readonly string[]): string {
  const longest = lines.reduce((n, l) => Math.max(n, ...Array.from(l.matchAll(/`+/g), (m) => m[0].length)), 0);
  return "`".repeat(Math.max(3, longest + 1));
}

/**
 * `text` with a suggestion of `lines` (the lines picked, to edit into what they should be: W26-R10)
 * put in at `caret` on lines of its own, and where the caret goes then: the end of its last line.
 */
export function insertSuggestion(text: string, caret: number, lines: readonly string[]): { text: string; caret: number } {
  const at = Math.max(0, Math.min(caret, text.length));
  const before = text.slice(0, at);
  const after = text.slice(at);
  const fence = fenceFor(lines);
  const head = `${before}${before === "" || before.endsWith("\n") ? "" : "\n"}${fence}suggestion\n${lines.join("\n")}`;
  return { text: `${head}\n${fence}\n${after.startsWith("\n") ? after.slice(1) : after}`, caret: head.length };
}

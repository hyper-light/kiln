/**
 * Plan reviews as this browser holds them (review.tsx): an answer being written (picked and typed,
 * kept across tabs, pages and reloads until it's sent), and answers given here a while ago, for the
 * receipt the Plan tab shows once its review goes, and, for a plan proposed in plan mode, what you
 * asked when you answered Other… (its harness keeps no record of a denial's words kiln can read
 * back, so its next proposal says what changed from what was kept here). One `kiln.*` preference
 * each (lib/prefs.ts), so another tab follows.
 */
import { getPref, setPref, usePref } from "@/lib/prefs";

export type Answer = "approve" | "reject" | "other";

export interface Draft {
  pick: Answer | null;
  words: string;
}

const DRAFTS = "kiln.plan.review.drafts";
const ANSWERED = "kiln.plan.review.answered";

const NO_DRAFTS: Record<string, Draft> = {};
const BLANK: Draft = { pick: null, words: "" };

/** The answer being written to review `key` (a plan's version, or its harness's request), and how to change it. */
export function useDraft(key: string): [Draft, (d: Draft) => void] {
  const [all] = usePref(DRAFTS, NO_DRAFTS);
  const set = (d: Draft) => setPref(DRAFTS, { ...getPref(DRAFTS, NO_DRAFTS), [key]: d });
  return [all[key] ?? BLANK, set];
}

function dropDraft(key: string) {
  setPref(DRAFTS, Object.fromEntries(Object.entries(getPref(DRAFTS, NO_DRAFTS)).filter(([k]) => k !== key)));
}

/** An answer given here. */
export interface Answered {
  kind: "goal" | "proposed";
  answer: Answer;
  /** Other…: what you asked. */
  words: string;
  title: string;
  /** The version answered (a plan kiln keeps), if known. */
  version: number | null;
  /** Its plan, as kiln keeps it. */
  plan: string | null;
  steps: number;
  /** A goal's plan: the goal's number on its stack. */
  goal?: number;
  /** Unix ms. */
  at: number;
}

const NO_ANSWERS: Record<string, Answered> = {};

/** The answer given here to agent `agent`'s last review. */
export function useAnswered(agent: string): Answered | undefined {
  const [all] = usePref(ANSWERED, NO_ANSWERS);
  return all[agent];
}

/** Agent `agent`'s review answered here (its draft `draft` sent). Only each agent's last is kept. */
export function noteAnswered(agent: string, draft: string, a: Answered) {
  dropDraft(draft);
  setPref(ANSWERED, { ...getPref(ANSWERED, NO_ANSWERS), [agent]: a });
}

/** How long the Plan tab says what you answered, once its review has gone. */
export const RECEIPT_MS = 10 * 60_000;

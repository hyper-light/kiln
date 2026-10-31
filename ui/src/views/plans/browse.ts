/**
 * Finding kept plans and research in an agent's Plan tab (browser.tsx, research.tsx): the words
 * typed go to the server once typing pauses (`useAsked`), and what comes back narrows by facets,
 * each option with how many it would show: within a facet any option chosen, across facets all.
 */
import * as React from "react";
import type { PlanBrief } from "@/api/plans";
import type { ResearchBrief } from "@/api/research";
import type { Chosen, Facet } from "@/components/ui/facet-box";
import { relativeTime } from "@/lib/format";

/** How long typing pauses before its words are asked for. */
const PAUSE_MS = 250;

/** The words typed, those asked for (the same once typing pauses), and what types them: the handler's own timer, no effect. */
export function useAsked(): [typed: string, asked: string, type: (words: string) => void] {
  const [typed, setTyped] = React.useState("");
  const [asked, setAsked] = React.useState("");
  const timer = React.useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const type = (words: string) => {
    setTyped(words);
    clearTimeout(timer.current);
    // Cleared, at once.
    if (!words.trim()) setAsked("");
    else timer.current = setTimeout(() => setAsked(words.trim()), PAUSE_MS);
  };
  return [typed, asked, type];
}

/** A facet: what it's called, its options, and which of them a row has. */
export interface Dimension<T> {
  id: string;
  label: string;
  options: { id: string; label: string }[];
  of: (r: T) => string[];
}

/** Whether row `r` has every facet chosen (any of a facet's options); `except` leaves one facet out. */
export const hasChosen = <T>(dims: Dimension<T>[], chosen: Chosen, r: T, except?: string) =>
  dims.every((d) => d.id === except || !chosen[d.id]?.length || d.of(r).some((v) => chosen[d.id]!.includes(v)));

/** Each facet's options with how many rows each would show, the other facets' choices as they are; an option no row has is left out unless chosen. */
export function facetsOf<T>(dims: Dimension<T>[], rows: T[], chosen: Chosen): Facet[] {
  return dims.flatMap((d) => {
    const options = d.options
      .filter((o) => chosen[d.id]?.includes(o.id) || rows.some((r) => d.of(r).includes(o.id)))
      .map((o) => ({ ...o, count: rows.filter((r) => d.of(r).includes(o.id) && hasChosen(dims, chosen, r, d.id)).length }));
    return options.length ? [{ id: d.id, label: d.label, options }] : [];
  });
}

/** A kept plan as a row: whose it is (this agent's, another's, no agent's) and where it stands. */
export interface PlanRow {
  p: PlanBrief;
  whose: "here" | "agent" | "none";
  stand: "planning" | "new" | "ran";
}

export const planRow = (p: PlanBrief, session: string): PlanRow => ({
  p,
  whose: p.source?.session === session ? "here" : p.source?.session || p.source?.agent ? "agent" : "none",
  stand: p.phase === "planning" ? "planning" : p.runs ? "ran" : "new",
});

/** What plans narrow by: whose, where they stand, and their tags. */
export function planDimensions(rows: PlanRow[]): Dimension<PlanRow>[] {
  return [
    {
      id: "whose",
      label: "Kept with",
      options: [
        { id: "here", label: "This agent" },
        { id: "agent", label: "Other agents" },
        { id: "none", label: "No agent" },
      ],
      of: (r) => [r.whose],
    },
    {
      id: "stand",
      label: "State",
      options: [
        { id: "planning", label: "Being planned" },
        { id: "new", label: "Not run yet" },
        { id: "ran", label: "Run before" },
      ],
      of: (r) => [r.stand],
    },
    { id: "tags", label: "Tags", options: [...new Set(rows.flatMap((r) => r.p.tags))].sort().map((t) => ({ id: t, label: t })), of: (r) => r.p.tags },
  ];
}

/** What a plan's row says under its title: its steps, when it last changed, whose it is, where it stands. */
export function planMeta(r: PlanRow, now = Date.now()): string[] {
  const p = r.p;
  const n = p.step_count;
  const whose = r.whose === "here" ? (p.source?.proposed ? "proposed here" : p.source?.goal ? "from its goal" : "this agent") : r.whose === "agent" ? (p.source?.agent ?? "another agent") : "";
  const stand = r.stand === "planning" ? "being planned" : r.stand === "ran" ? `run ${p.runs}×` : "not run";
  return [n ? `${n} step${n === 1 ? "" : "s"}` : "", relativeTime(p.updated_at, now), whose, stand].filter(Boolean);
}

const GRADES = ["A", "B", "C", "D", "F"] as const;

/** What research narrows by: whose (agent `agent`'s, another agent's, a person's), its findings' grades, and its plan. */
export function researchDimensions(agent: string): Dimension<ResearchBrief>[] {
  return [
    {
      id: "whose",
      label: "Whose",
      options: [
        { id: "here", label: "This agent" },
        { id: "agent", label: "Other agents" },
        { id: "person", label: "People" },
      ],
      of: (r) => [r.agent === agent ? "here" : r.agent ? "agent" : "person"],
    },
    { id: "grade", label: "Has a finding graded", options: GRADES.map((g) => ({ id: g, label: g })), of: (r) => GRADES.filter((g) => r.grades.includes(g)) },
    {
      id: "plan",
      label: "Plan",
      options: [
        { id: "plan", label: "A plan's" },
        { id: "none", label: "On its own" },
      ],
      of: (r) => [r.plan ? "plan" : "none"],
    },
  ];
}

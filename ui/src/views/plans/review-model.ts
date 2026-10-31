/**
 * A plan under review (review.tsx), as a person weighs it before answering: which findings of its
 * research to look at first (weak, contested, or quoted where kiln couldn't find the words), how each
 * step stands on them, and what changed since they last answered "Other…". However much research a
 * plan has, what's shown up front is a fixed few lines: counts, and the few findings that need a look;
 * the rest is there on asking. Pure: no clock, no I/O.
 */
import type { PlanItem } from "@/api/plans";
import type { Finding, Grade, Research } from "@/api/research";

export const GRADES: readonly Grade[] = ["A", "B", "C", "D", "F"];

/** Why a finding a step rests on is worth a look before answering. */
export type Flag = "weak" | "contested" | "unfound";

export const FLAG_WORDS: Record<Flag, string> = {
  weak: "weak",
  contested: "contested",
  unfound: "quote not found",
};

/** Counterclaims weighing this much contest a finding, as a research's page marks them (research.tsx). */
const AGAINST = 0.25;

/**
 * Why `f` is worth a look: graded D or F (what `plans_ready` warns of), its counterclaims weighing a
 * quarter or more, or a quote kiln didn't find where it's cited (or its source gone). None: it isn't.
 */
export function flagsOf(f: Finding): Flag[] {
  const out: Flag[] = [];
  if (f.confidence.grade === "D" || f.confidence.grade === "F") out.push("weak");
  if (f.confidence.against >= AGAINST) out.push("contested");
  if (f.confidence.failed > 0 || f.cites.some((c) => c.check?.state === "not_found" || c.check?.state === "gone")) out.push("unfound");
  return out;
}

/** A finding as the review shows it: why it's worth a look (if it is), and the steps resting on it. */
export interface Look {
  f: Finding;
  flags: Flag[];
  steps: number[];
}

/** How many of each grade there are, A first, only those there are. */
export function gradeCounts(fs: readonly Finding[]): [Grade, number][] {
  return GRADES.map((g): [Grade, number] => [g, fs.filter((f) => f.confidence.grade === g).length]).filter(([, n]) => n > 0);
}

/** Which steps (by number, from 1) rest on each finding. */
export function restsOn(items: readonly PlanItem[]): Map<string, number[]> {
  const out = new Map<string, number[]>();
  items.forEach((it, i) => {
    for (const b of new Set(it.basis ?? [])) out.set(b, [...(out.get(b) ?? []), i + 1]);
  });
  return out;
}

/** Worst first: weak, then contested, then a quote not found; within each, the lower score, then the more steps resting on it. */
function worstFirst(a: Look, b: Look): number {
  const tier = (l: Look) => (l.flags.includes("weak") ? 0 : l.flags.includes("contested") ? 1 : l.flags.length ? 2 : 3);
  return tier(a) - tier(b) || a.f.confidence.score - b.f.confidence.score || b.steps.length - a.steps.length || a.f.id.localeCompare(b.f.id, undefined, { numeric: true });
}

/** What a plan's research comes to for its review, in a fixed few lines whatever its size. */
export interface Digest {
  findings: number;
  sources: number;
  inside: number;
  outside: number;
  grades: [Grade, number][];
  /** Findings a step rests on that are worth a look, worst first. */
  look: Look[];
  /** Steps resting on no finding, by number. */
  bare: number[];
  /** Quotes kiln is still looking for: grades may change. */
  checking: number;
  /** The least sure finding a step rests on, said when none is worth a look. */
  least: Look | null;
  /** Findings that no step rests on (there on asking). */
  unused: number;
}

export function digest(items: readonly PlanItem[], r: Research): Digest {
  const rests = restsOn(items);
  const used = r.findings.filter((f) => rests.has(f.id)).map((f): Look => ({ f, flags: flagsOf(f), steps: rests.get(f.id) ?? [] }));
  const look = used.filter((l) => l.flags.length > 0).sort(worstFirst);
  const least = look.length ? null : ([...used].sort(worstFirst)[0] ?? null);
  return {
    findings: r.summary.findings,
    sources: r.summary.sources,
    inside: r.summary.internal,
    outside: r.summary.external,
    grades: gradeCounts(r.findings),
    look,
    bare: items.flatMap((it, i) => ((it.basis ?? []).length === 0 ? [i + 1] : [])),
    checking: r.summary.checking,
    least,
    unused: r.findings.length - used.length,
  };
}

/** Every finding as the review lists them on asking: those a step rests on, worst first, then the rest. */
export function allLooks(items: readonly PlanItem[], r: Research): { used: Look[]; unused: Look[] } {
  const rests = restsOn(items);
  const looks = r.findings.map((f): Look => ({ f, flags: flagsOf(f), steps: rests.get(f.id) ?? [] }));
  return { used: looks.filter((l) => l.steps.length > 0).sort(worstFirst), unused: looks.filter((l) => l.steps.length === 0).sort(worstFirst) };
}

/** What one step rests on, for its review. */
export interface StepBasis {
  /** Its findings: those worth a look first (worst first), then the rest as the agent named them. */
  all: Look[];
  flagged: Look[];
  grades: [Grade, number][];
  /** Ids it names that its research doesn't hold. */
  missing: string[];
}

export function stepBasis(item: PlanItem, r: Research | null, rests: Map<string, number[]>): StepBasis {
  const ids = [...new Set(item.basis ?? [])];
  const found = ids.map((id) => r?.findings.find((f) => f.id === id)).filter((f): f is Finding => !!f);
  const looks = found.map((f): Look => ({ f, flags: flagsOf(f), steps: rests.get(f.id) ?? [] }));
  const flagged = looks.filter((l) => l.flags.length > 0).sort(worstFirst);
  return {
    all: [...flagged, ...looks.filter((l) => l.flags.length === 0)],
    flagged,
    grades: gradeCounts(found),
    missing: r ? ids.filter((id) => !r.findings.some((f) => f.id === id)) : [],
  };
}

/** A plan's steps as items: its items, or its markdown's steps as titles alone. */
export function asItems(items: readonly PlanItem[] | undefined, steps: readonly string[]): PlanItem[] {
  if (items?.length) return [...items];
  return steps.map((title) => ({ title, description: "", examples: "", acceptance: [], testing: [] }));
}

/** How one step changed from one version of a plan to the next. */
export interface StepChange {
  /** Its number: in the new version (in the old one, for a step removed). */
  n: number;
  kind: "added" | "rewritten" | "changed" | "removed";
  title: string;
  /** Rewritten: its title before. */
  was?: string;
  /** Changed: what of it, in words. */
  parts?: string[];
}

export interface Changes {
  list: StepChange[];
  /** The new version's steps that didn't change, by number. */
  same: number[];
}

const same = (a: unknown, b: unknown) => JSON.stringify(a ?? null) === JSON.stringify(b ?? null);
const listOf = <T>(v: readonly T[] | undefined): readonly T[] => v ?? [];

/** What of a step changed, in words (`moved`: it changed places among the others, from step `moved`). */
function partsChanged(a: PlanItem, b: PlanItem, moved: number | null): string[] {
  const out: string[] = [];
  if (a.description.trim() !== b.description.trim()) out.push("what to do");
  if (a.examples.trim() !== b.examples.trim()) out.push("its examples");
  if (!same(a.acceptance, b.acceptance)) out.push("when it's done");
  if (!same(a.testing, b.testing)) out.push("how it's tested");
  if (!same(listOf(a.needs), listOf(b.needs))) out.push("what it waits for");
  if (!same(listOf(a.basis), listOf(b.basis))) out.push("what it rests on");
  if (moved !== null) out.push(`its place (was step ${moved})`);
  return out;
}

/**
 * Of steps kept from one version to the next (each its index then and now, in today's order), those
 * that changed places among the others: off the longest run still in their old order. A step added
 * or taken out moves none of the rest.
 */
function movedOf(kept: readonly { now: number; was: number }[]): Set<number> {
  const best = kept.map(() => 1);
  const prev = kept.map(() => -1);
  kept.forEach((k, i) => {
    for (let j = 0; j < i; j++) {
      const o = kept[j];
      if (o && o.was < k.was && (best[j] ?? 0) + 1 > (best[i] ?? 0)) {
        best[i] = (best[j] ?? 0) + 1;
        prev[i] = j;
      }
    }
  });
  const inOrder = new Set<number>();
  let at = best.indexOf(Math.max(0, ...best));
  while (at >= 0) {
    const k = kept[at];
    if (k) inOrder.add(k.now);
    at = prev[at] ?? -1;
  }
  return new Set(kept.filter((k) => !inOrder.has(k.now)).map((k) => k.now));
}

/**
 * What changed between two versions of a plan's steps: steps matched by title (each once), a step
 * whose title changed in the same place rewritten, and the rest added or removed.
 */
export function changesSince(before: readonly PlanItem[], after: readonly PlanItem[]): Changes {
  const matched = new Map<number, number>(); // new index → old index
  const taken = new Set<number>();
  after.forEach((it, i) => {
    const j = before.findIndex((b, k) => !taken.has(k) && b.title.trim() === it.title.trim());
    if (j >= 0) {
      matched.set(i, j);
      taken.add(j);
    }
  });
  // A step in the same place whose title changed is that step, rewritten.
  after.forEach((_, i) => {
    if (!matched.has(i) && i < before.length && !taken.has(i)) {
      matched.set(i, i);
      taken.add(i);
    }
  });
  const list: StepChange[] = [];
  const kept: number[] = [];
  const sameTitled = after.flatMap((it, i) => {
    const j = matched.get(i);
    return j !== undefined && before[j]?.title.trim() === it.title.trim() ? [{ now: i, was: j }] : [];
  });
  const moved = movedOf(sameTitled);
  after.forEach((it, i) => {
    const j = matched.get(i);
    const b = j === undefined ? undefined : before[j];
    if (j === undefined || !b) return void list.push({ n: i + 1, kind: "added", title: it.title });
    if (b.title.trim() !== it.title.trim()) return void list.push({ n: i + 1, kind: "rewritten", title: it.title, was: b.title });
    const parts = partsChanged(b, it, moved.has(i) ? j + 1 : null);
    if (parts.length) list.push({ n: i + 1, kind: "changed", title: it.title, parts });
    else kept.push(i + 1);
  });
  before.forEach((b, j) => {
    if (!taken.has(j)) list.push({ n: j + 1, kind: "removed", title: b.title });
  });
  return { list, same: kept };
}

/** Numbers in words, three or more in a row as a run: "3–5", "1, 3 and 4", "2, 6–8 and 10". */
export function runsOf(ns: readonly number[]): string {
  const sorted = [...new Set(ns)].sort((a, b) => a - b);
  const runs: string[] = [];
  for (let i = 0; i < sorted.length;) {
    let j = i;
    while (j + 1 < sorted.length && sorted[j + 1] === (sorted[j] ?? 0) + 1) j++;
    if (j - i >= 2) runs.push(`${sorted[i]}–${sorted[j]}`);
    else for (let k = i; k <= j; k++) runs.push(String(sorted[k]));
    i = j + 1;
  }
  return runs.length > 1 ? `${runs.slice(0, -1).join(", ")} and ${runs.at(-1)}` : (runs[0] ?? "");
}

/** "step 2", "steps 2 and 4": the steps resting on a finding. */
export function stepsWord(ns: readonly number[]): string {
  return ns.length === 1 ? `step ${ns[0]}` : `steps ${runsOf(ns)}`;
}

/** A change in a line, as "Changed since you looked" lists them. */
export function changeWords(c: StepChange): string {
  switch (c.kind) {
    case "added":
      return `Step ${c.n} added: ${c.title}`;
    case "removed":
      return `Step ${c.n} taken out: ${c.title}`;
    case "rewritten":
      return `Step ${c.n} rewritten: ${c.title} (was: ${c.was ?? ""})`;
    case "changed":
      return `Step ${c.n} changed: ${(c.parts ?? []).join(", ")}`;
  }
}

/** The same steps, in a line ("Steps 3–5 unchanged"); none, nothing. */
export function sameWords(ns: readonly number[]): string {
  if (ns.length === 0) return "";
  return ns.length === 1 ? `Step ${ns[0]} unchanged` : `Steps ${runsOf(ns)} unchanged`;
}

/** Text this long shows its first lines, the rest on asking. */
export const LONG = 480;

/** "Comment on step n" starts Other… with the step named, after what's written already. */
export function commentLine(words: string, n: number, title: string): string {
  const line = `Step ${n} (${title.trim()}): `;
  return words.trim() ? `${words.replace(/\s+$/, "")}\n${line}` : line;
}

/** A plan's title as its words have it: its first heading (none: undefined). */
export function headingOf(md: string): string | undefined {
  const m = /^ {0,3}#{1,6}\s+(.+?)(?:\s+#+)?\s*$/m.exec(md);
  return m?.[1]?.trim() || undefined;
}

/** A plan's words before its steps: what it's for, its headings left out, up to its first list or the heading after its words (none: ""). */
export function introOf(md: string): string {
  const out: string[] = [];
  for (const line of md.split(/\r?\n/)) {
    const t = line.trim();
    const said = out.some((l) => l.trim());
    if (/^#{1,6}\s/.test(t)) {
      if (said) break;
      continue;
    }
    if (line === line.trimStart() && /^([-*+]|\d+[.)])\s/.test(t)) break;
    out.push(line);
  }
  return out.join("\n").trim();
}

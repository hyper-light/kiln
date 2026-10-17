/**
 * Choices: an agent's multiple-choice questions (Claude Code's AskUserQuestion, Codex's
 * requestUserInput), asked one at a time. Several get a stepper (their header chips wrap, never
 * scroll sideways), Back and Next, and a review of every answer before it goes; one is just the
 * question. Options are full-width rows with radio or checkbox semantics, "Other…" takes your own
 * words, a note can ride along, and an option's preview sits beside the list (under the option on
 * phones). Or, instead of picking one, an experiment: the options tried alone, stacked, in pairs or
 * greedily, each in an isolated workspace, judged against criteria you set.
 *
 * Three frames: a bottom sheet (phones), a centered dialog (desktops), or inline (an approval card,
 * the timeline); left unset, the screen picks between the first two. Minimizing keeps the question
 * waiting (your picks survive a remount); declining answers the agent.
 */
import * as React from "react";
import { Dialog as D } from "radix-ui";
import { ArrowLeft, ArrowRight, Check, ChevronDown, FlaskConical, Minus, Plus, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { useMediaQuery } from "@/lib/hooks";
import { cn } from "@/lib/utils";
import { Optic, PrismLabel } from "./signals";
import "./choices.css";

/* ------------------------------------------------------------------ model */

export interface ChoiceOption {
  label: string;
  description?: string;
  /** Shown while the option is focused or picked (a mockup, a snippet), as monospace text. */
  preview?: string;
}

export interface ChoiceQuestion {
  /** The harness's own id for the question (Codex has them). */
  id?: string;
  question: string;
  /** A short label: the question's chip in the stepper. */
  header?: string;
  multi?: boolean;
  /** None at all: a free-text question. */
  options: ChoiceOption[];
  /** Offer "Other…", an answer in your own words (default true). */
  other?: boolean;
  /** Mask the words you type. */
  secret?: boolean;
}

export interface ChoiceAnswer {
  question: string;
  id?: string;
  /** The labels picked, exactly as the agent wrote them, in its order. */
  selected: string[];
  /** Your own words: "Other…", or the answer to a free-text question. */
  other?: string;
  note?: string;
}

export type ExperimentStrategy = "each" | "all" | "pairs" | "greedy" | "custom";

export interface Criterion {
  kind: "pipeline" | "command" | "metric";
  pipeline?: string;
  command?: string;
  metric?: string;
  /** Metrics: which way is better. */
  better?: "lower" | "higher";
  /** A number to beat: absolute, or relative (%) against the baseline. */
  threshold?: { type: "absolute" | "relative"; value: number };
  /** It must pass (or meet its threshold), rather than be optimized. */
  required?: boolean;
}

export interface ExperimentSpec {
  question: string;
  questionId?: string;
  /** The options in play, as the agent wrote them. */
  options: string[];
  strategy: ExperimentStrategy;
  /** Explicit option sets: every variant, except under greedy stacking, where it's the first round only. */
  variants: string[][];
  /** Also run with none of the options. */
  baseline: boolean;
  /** Which isolation each variant runs in (an `IsolationOption` id). */
  isolation: string;
  criteria: Criterion[];
  repetitions: number;
  maxParallel: number;
  budgetMinutes?: number;
  decide: "auto" | "ask";
  notes?: string;
}

/** Somewhere a variant can run on its own: a git worktree, a slate, a container, a microVM … */
export interface IsolationOption {
  id: string;
  label: string;
  /** What it is; for one that isn't available, why not. */
  description?: string;
  available: boolean;
}

export interface ChoicesProps {
  questions: ChoiceQuestion[];
  agentName?: string;
  /** The harness, as people know it ("Claude Code"). */
  kind?: string;
  busy?: boolean;
  canDecline?: boolean;
  /** Unset: a sheet on phones, a dialog elsewhere. */
  layout?: "sheet" | "dialog" | "inline";
  onSubmit: (answers: ChoiceAnswer[]) => void;
  onDecline?: () => void;
  onMinimize?: () => void;
  /** Offers "Experiment…" on questions with two or more options. `answers`: the other questions' answers so far. */
  onExperiment?: (spec: ExperimentSpec, answers: ChoiceAnswer[]) => void;
  isolation?: IsolationOption[];
  defaultIsolation?: string;
  pipelines?: string[];
  className?: string;
}

/* ------------------------------------------------------------------ answers, in each harness's terms */

const said = (a: ChoiceAnswer) => (a.other !== undefined ? [...a.selected, a.other] : a.selected);

/** Claude Code's AskUserQuestion answers: by question text; several picks joined by ", ", your own words as typed. */
export function answersToClaude(answers: ChoiceAnswer[]): Record<string, string> {
  return Object.fromEntries(answers.map((a) => [a.question, said(a).join(", ")]));
}

/** Codex's requestUserInput answers: by question id (its text, failing one); a note rides as "user_note: …", as Codex's own TUI sends it. */
export function answersToCodex(answers: ChoiceAnswer[]): Record<string, { answers: string[] }> {
  return Object.fromEntries(answers.map((a) => [a.id ?? a.question, { answers: a.note ? [...said(a), `user_note: ${a.note}`] : said(a) }]));
}

/* ------------------------------------------------------------------ experiments (pure) */

/** Past this many variants the list stops (2^12 − 1 is already a long night). */
const MAX_VARIANTS = 4096;
/** More variants than this earns a warning. */
const MANY_VARIANTS = 32;

/** Every k-subset of 0…n−1, lexicographically. */
function* subsets(n: number, k: number): Generator<number[]> {
  if (k < 1 || k > n) return;
  const c = Array.from({ length: k }, (_, i) => i);
  for (;;) {
    yield [...c];
    let i = k - 1;
    while (i >= 0 && c[i] === n - k + i) i--;
    if (i < 0) return;
    c[i] = (c[i] ?? 0) + 1;
    for (let j = i + 1; j < k; j++) c[j] = (c[j - 1] ?? 0) + 1;
  }
}

/** Singles, then pairs, then triples …; within a size, in the options' order. */
function bySize(a: number[], b: number[]): number {
  if (a.length !== b.length) return a.length - b.length;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return (a[i] ?? 0) - (b[i] ?? 0);
  return 0;
}

/**
 * The option sets an experiment runs, in a stable order: singles, then pairs, then triples ….
 * Greedy stacking lists its first round (each option alone); the rounds after it depend on results.
 * Custom sets keep only known options, each once, and drop empty and repeated sets.
 */
export function variantsFor(options: string[], strategy: ExperimentStrategy, custom?: string[][]): string[][] {
  const label = (i: number) => options[i] ?? "";
  if (strategy === "custom") {
    const at = new Map<string, number>();
    options.forEach((o, i) => {
      if (!at.has(o)) at.set(o, i);
    });
    const seen = new Set<string>();
    const sets: number[][] = [];
    for (const set of custom ?? []) {
      const ix = [...new Set(set.flatMap((o) => at.get(o) ?? []))].sort((a, b) => a - b);
      const key = ix.join("+");
      if (ix.length > 0 && !seen.has(key)) {
        seen.add(key);
        sets.push(ix);
      }
    }
    return sets.sort(bySize).map((s) => s.map(label));
  }
  const most = strategy === "all" ? options.length : strategy === "pairs" ? 2 : 1;
  const out: string[][] = [];
  for (let k = 1; k <= most; k++)
    for (const s of subsets(options.length, k)) {
      if (out.length >= MAX_VARIANTS) return out;
      out.push(s.map(label));
    }
  return out;
}

/** How many variants a strategy runs over n options (`custom`: how many sets were picked). Greedy's is at most. */
export function variantCount(n: number, strategy: ExperimentStrategy, custom = 0): { count: number; atMost: boolean } {
  switch (strategy) {
    case "each":
      return { count: n, atMost: false };
    case "all":
      return { count: n > 0 ? 2 ** n - 1 : 0, atMost: false };
    case "pairs":
      return { count: n + (n * (n - 1)) / 2, atMost: false };
    case "greedy":
      // n alone, then n − 1 stacked on the best, then n − 2 …
      return { count: (n * (n + 1)) / 2, atMost: true };
    case "custom":
      return { count: custom, atMost: false };
  }
}

/** "7 variants + baseline × 3 runs = 24 runs" (repetitions that aren't a count yet read "?"). */
export function describeRuns(variants: { count: number; atMost: boolean }, baseline: boolean, repetitions: number): string {
  const up = variants.atMost ? "up to " : "";
  const v = variants.count;
  const ok = Number.isInteger(repetitions) && repetitions > 0;
  const runs = (v + (baseline ? 1 : 0)) * repetitions;
  const each = ok ? `${repetitions} run${repetitions === 1 ? "" : "s"}` : "? runs";
  return `${up}${v} variant${v === 1 ? "" : "s"}${baseline ? " + baseline" : ""} × ${each} = ${ok ? `${up}${runs} run${runs === 1 ? "" : "s"}` : "?"}`;
}

/** What stops an experiment from starting, in a few words; null when it's ready. */
export function experimentProblem(spec: ExperimentSpec): string | null {
  if (spec.options.length < 2) return "Keep at least two options in play.";
  if (spec.variants.length === 0) return "Pick at least one set to try.";
  if (!spec.isolation) return "Nowhere to run them: no isolation is available.";
  if (spec.criteria.length === 0) return "Add a criterion to judge them by.";
  for (const c of spec.criteria) {
    if (c.kind === "pipeline" && !c.pipeline?.trim()) return "Name the pipeline to run.";
    if (c.kind === "command" && !c.command?.trim()) return "Give the command to run.";
    if (c.kind === "metric" && !c.metric?.trim()) return "Name the metric to measure.";
    if (c.threshold && !Number.isFinite(c.threshold.value)) return "A threshold has to be a number.";
  }
  if (!Number.isInteger(spec.repetitions) || spec.repetitions < 1) return "Repetitions: a whole number, at least 1.";
  if (!Number.isInteger(spec.maxParallel) || spec.maxParallel < 1) return "Max parallel: a whole number, at least 1.";
  if (spec.budgetMinutes !== undefined && !(spec.budgetMinutes > 0)) return "The time budget is a number of minutes.";
  return null;
}

/* ------------------------------------------------------------------ drafts */

/** Where you are in one question. */
interface Draft {
  /** The options picked, by index, in the order you picked them. */
  picked: number[];
  /** "Other…" is chosen (in a single choice, instead of the options). */
  other: boolean;
  /** Your own words: "Other…", or a free-text answer. */
  text: string;
  note: string;
  noteOpen: boolean;
}

const BLANK: Draft = { picked: [], other: false, text: "", note: "", noteOpen: false };

const offersOther = (q: ChoiceQuestion) => q.options.length > 0 && q.other !== false;

/** The words you typed, as they'll be sent: a secret exactly as typed, anything else trimmed. */
function typed(q: ChoiceQuestion, d: Draft): string | undefined {
  if (q.options.length > 0 && !d.other) return undefined;
  if (!d.text.trim()) return undefined;
  return q.secret ? d.text : d.text.trim();
}

const isAnswered = (q: ChoiceQuestion, d: Draft) => d.picked.length > 0 || typed(q, d) !== undefined;

function answerOf(q: ChoiceQuestion, d: Draft): ChoiceAnswer {
  const other = typed(q, d);
  const note = d.note.trim();
  const selected = [...d.picked]
    .sort((a, b) => a - b)
    .flatMap((i) => {
      const o = q.options[i];
      return o ? [o.label] : [];
    });
  return { question: q.question, ...(q.id ? { id: q.id } : {}), selected, ...(other !== undefined ? { other } : {}), ...(note ? { note } : {}) };
}

/** Option `o` picked (the one past the last option is "Other…"): a single choice moves to it, a multiple one toggles it. */
function withPick(ds: Draft[], i: number, q: ChoiceQuestion, o: number, on?: boolean): Draft[] {
  return ds.map((d, j) => {
    if (j !== i) return d;
    const other = o >= q.options.length;
    if (!q.multi) return other ? { ...d, other: true, picked: [] } : { ...d, other: false, picked: [o] };
    if (other) return { ...d, other: on ?? !d.other };
    const has = d.picked.includes(o);
    return (on ?? !has) ? (has ? d : { ...d, picked: [...d.picked, o] }) : { ...d, picked: d.picked.filter((x) => x !== o) };
  });
}

const RECOMMENDED = /\s*\(recommended\)\s*$/i;

/** An option's label as shown: "(Recommended)" becomes a tag. */
function shown(label: string): { text: string; recommended: boolean } {
  return RECOMMENDED.test(label) ? { text: label.replace(RECOMMENDED, ""), recommended: true } : { text: label, recommended: false };
}

/** A preview's widest line, in columns (a tab as four), so a mockup can shrink to fit instead of wrapping. */
function columns(text: string): number {
  return text.split("\n").reduce((w, line) => Math.max(w, line.replace(/\t/g, "    ").length), 0);
}

/** Lines past this are prose or code that should wrap rather than shrink. */
const FIT_COLUMNS = 100;

/* ------------------------------------------------------------------ setup (the experiment, as you build it) */

interface CriterionDraft {
  key: number;
  kind: Criterion["kind"];
  pipeline: string;
  command: string;
  metric: string;
  better: "lower" | "higher";
  thresholdType: "absolute" | "relative";
  /** As typed; empty is no threshold. */
  threshold: string;
  required: boolean;
}

interface Setup {
  /** The question it tries. */
  q: number;
  /** Options in play, by index. */
  on: boolean[];
  strategy: ExperimentStrategy;
  /** Custom sets, as option indices ("0+2"). */
  sets: string[];
  baseline: boolean;
  isolation: string;
  criteria: CriterionDraft[];
  /** Numbers as typed. */
  repetitions: string;
  maxParallel: string;
  budget: string;
  decide: "auto" | "ask";
  notes: string;
}

const STRATEGIES: { id: ExperimentStrategy; label: string; hint: string }[] = [
  { id: "each", label: "Each alone", hint: "Every option on its own." },
  { id: "all", label: "All combinations", hint: "Every option alone, and every way of stacking them." },
  { id: "pairs", label: "Pairs", hint: "Every option alone, and every two together." },
  { id: "greedy", label: "Greedy stacking", hint: "Start with the best single option, then keep adding the option that improves most." },
  { id: "custom", label: "Custom", hint: "Only the sets you pick." },
];

const DECISIONS: { id: Setup["decide"]; label: string }[] = [
  { id: "auto", label: "Pick automatically when one variant clearly wins" },
  { id: "ask", label: "Bring the results back to me" },
];

/** Until the server says where variants can run, a git worktree is somewhere they always can. */
const FALLBACK_ISOLATION: IsolationOption[] = [{ id: "worktree", label: "Git worktree", description: "Each variant in its own worktree of the repository.", available: true }];

let criterionSeq = 0;

function newCriterion(kind: Criterion["kind"], pipelines: string[]): CriterionDraft {
  criterionSeq += 1;
  return { key: criterionSeq, kind, pipeline: pipelines[0] ?? "", command: "", metric: "", better: "lower", thresholdType: "relative", threshold: "", required: kind !== "metric" };
}

/** What depends on the question: its options, and a strategy that suits them (options you'd pick several of can stack). */
function optionPart(q: number, question: ChoiceQuestion): Pick<Setup, "q" | "on" | "strategy" | "sets"> {
  const n = question.options.length;
  return { q, on: question.options.map(() => true), strategy: question.multi ? (2 ** n - 1 <= MANY_VARIANTS ? "all" : "pairs") : "each", sets: [] };
}

function newSetup(q: number, question: ChoiceQuestion, isolation: string, pipelines: string[]): Setup {
  return {
    ...optionPart(q, question),
    baseline: true,
    isolation,
    criteria: [newCriterion(pipelines.length > 0 ? "pipeline" : "command", pipelines)],
    repetitions: "3",
    maxParallel: "4",
    budget: "",
    decide: "ask",
    notes: "",
  };
}

/** The isolation to use: the one asked for if it's available, else the first available one. */
function pickIsolation(list: IsolationOption[], wanted?: string): string {
  return list.find((o) => o.id === wanted && o.available)?.id ?? list.find((o) => o.available)?.id ?? "";
}

/** The sets Custom offers: every one for up to five options, past that the singles, the pairs and all of them. */
function customSets(n: number): number[][] {
  const out: number[][] = [];
  for (let k = 1; k <= n; k++) if (n <= 5 || k <= 2 || k === n) for (const s of subsets(n, k)) out.push(s);
  return out;
}

function criterionOf(c: CriterionDraft): Criterion {
  switch (c.kind) {
    case "pipeline":
      return { kind: "pipeline", pipeline: c.pipeline.trim(), required: c.required };
    case "command":
      return { kind: "command", command: c.command.trim(), required: c.required };
    case "metric": {
      const t = c.threshold.trim();
      return { kind: "metric", metric: c.metric.trim(), better: c.better, ...(t ? { threshold: { type: c.thresholdType, value: Number(t) } } : {}), required: c.required };
    }
  }
}

const count = (s: string) => (s.trim() ? Number(s) : NaN);

function specOf(question: ChoiceQuestion, s: Setup, isolation: string): ExperimentSpec {
  const labels = question.options.map((o) => o.label);
  const options = labels.filter((_, i) => s.on[i]);
  const custom = s.sets
    .map((k) => k.split("+").map(Number))
    .filter((ix) => ix.every((i) => s.on[i]))
    .map((ix) => ix.map((i) => labels[i] ?? ""));
  const budget = s.budget.trim();
  const notes = s.notes.trim();
  return {
    question: question.question,
    ...(question.id ? { questionId: question.id } : {}),
    options,
    strategy: s.strategy,
    variants: variantsFor(options, s.strategy, custom),
    baseline: s.baseline,
    isolation,
    criteria: s.criteria.map(criterionOf),
    repetitions: count(s.repetitions),
    maxParallel: count(s.maxParallel),
    ...(budget ? { budgetMinutes: Number(budget) } : {}),
    decide: s.decide,
    ...(notes ? { notes } : {}),
  };
}

/* ------------------------------------------------------------------ memory */

interface Saved {
  drafts: Draft[];
  step: number;
  toReview: boolean;
  setup: Setup | null;
  trying: boolean;
}

/** Unfinished answers, by question set: most hosts unmount a minimized question, and reopening it should find your picks. */
const saved = new Map<string, Saved>();

function remember(sig: string, s: Saved) {
  saved.delete(sig);
  saved.set(sig, s);
  const oldest = saved.keys().next().value;
  if (saved.size > 20 && oldest !== undefined) saved.delete(oldest);
}

/** Who asked what: the same questions from two agents are two drafts. */
const signature = (qs: ChoiceQuestion[], agent?: string) => JSON.stringify([agent ?? "", qs.map((q) => [q.id ?? "", q.question, q.multi ? 1 : 0, q.options.map((o) => o.label)])]);

/* ------------------------------------------------------------------ the dialog */

export function Choices(props: ChoicesProps) {
  const sig = React.useMemo(() => signature(props.questions, props.agentName), [props.questions, props.agentName]);
  if (props.questions.length === 0) return null;
  // New questions start over.
  return <Ask key={sig} sig={sig} {...props} />;
}

function Ask({
  sig,
  questions,
  agentName,
  kind,
  busy = false,
  canDecline = false,
  layout,
  onSubmit,
  onDecline,
  onMinimize,
  onExperiment,
  isolation,
  defaultIsolation,
  pipelines = [],
  className,
}: ChoicesProps & { sig: string }) {
  const uid = React.useId();
  const phone = useMediaQuery("(max-width: 639px)");
  const coarse = useMediaQuery("(pointer: coarse)");
  const frame = layout ?? (phone ? "sheet" : "dialog");
  const n = questions.length;

  const [restored] = React.useState(() => saved.get(sig));
  const [drafts, setDrafts] = React.useState<Draft[]>(() => restored?.drafts ?? questions.map(() => BLANK));
  const [step, setStep] = React.useState(() => Math.min(restored?.step ?? 0, n === 1 ? 0 : n));
  /** Editing from the review: Next goes straight back to it. */
  const [toReview, setToReview] = React.useState(() => restored?.toReview ?? false);
  const [setup, setSetup] = React.useState<Setup | null>(() => restored?.setup ?? null);
  /** The experiment setup is open. */
  const [trying, setTrying] = React.useState(() => Boolean(restored?.trying && restored.setup));
  const [focused, setFocused] = React.useState<number | null>(null);
  const [hint, setHint] = React.useState("");

  const rootRef = React.useRef<HTMLElement>(null);
  const setRoot = React.useCallback((el: HTMLElement | null) => {
    rootRef.current = el;
  }, []);
  const bodyRef = React.useRef<HTMLDivElement>(null);
  const otherRef = React.useRef<HTMLInputElement>(null);
  const wantOther = React.useRef(false);

  React.useEffect(() => {
    remember(sig, { drafts, step, toReview, setup, trying });
  }, [sig, drafts, step, toReview, setup, trying]);

  // Each view starts at its top, focus on what it asks of you (what it marks data-autofocus).
  const focusView = React.useCallback(() => {
    if (bodyRef.current) bodyRef.current.scrollTop = 0;
    (rootRef.current?.querySelector<HTMLElement>("[data-autofocus]") ?? rootRef.current)?.focus();
  }, []);
  const view = trying ? "x" : String(step);
  const lastView = React.useRef(view);
  React.useEffect(() => {
    if (lastView.current === view) return;
    lastView.current = view;
    focusView();
  }, [view, focusView]);
  // "Other…" was just chosen: type.
  React.useEffect(() => {
    if (!wantOther.current) return;
    wantOther.current = false;
    otherRef.current?.focus();
  });

  const q = step < n ? questions[step] : undefined;
  const d = drafts[step] ?? BLANK;
  const ready = q ? isAnswered(q, d) : questions.every((x, i) => isAnswered(x, drafts[i] ?? BLANK));

  const isolations = isolation && isolation.length > 0 ? isolation : FALLBACK_ISOLATION;
  const eligible = questions.flatMap((x, i) => (x.options.length >= 2 ? [i] : []));
  const canTry = Boolean(onExperiment) && (q ? q.options.length >= 2 : eligible.length > 0);
  const tried = setup ? questions[setup.q] : undefined;
  const isolationId = setup && isolations.some((o) => o.id === setup.isolation && o.available) ? setup.isolation : pickIsolation(isolations, defaultIsolation);
  const spec = trying && setup && tried ? specOf(tried, setup, isolationId) : null;
  const problem = spec ? experimentProblem(spec) : null;

  const go = (s: number) => {
    setStep(s);
    setTrying(false);
    setHint("");
    setFocused(null);
    if (s >= n) setToReview(false);
  };

  const submit = (ds: Draft[]) => {
    if (busy) return;
    const missing = questions.findIndex((x, i) => !isAnswered(x, ds[i] ?? BLANK));
    if (missing >= 0) {
      go(missing);
      setHint("That one still needs an answer.");
      return;
    }
    saved.delete(sig);
    onSubmit(questions.map((x, i) => answerOf(x, ds[i] ?? BLANK)));
  };

  const start = () => {
    if (busy || !onExperiment || !spec || problem || !setup) return;
    saved.delete(sig);
    onExperiment(
      spec,
      questions.flatMap((x, i) => (i !== setup.q && isAnswered(x, drafts[i] ?? BLANK) ? [answerOf(x, drafts[i] ?? BLANK)] : [])),
    );
  };

  /** The primary action: Next, Review, Submit, or Start experiment. */
  const advance = (ds: Draft[]) => {
    if (busy) return;
    if (trying) return start();
    if (!q) return submit(ds);
    if (!isAnswered(q, ds[step] ?? BLANK)) {
      setHint(q.options.length > 0 ? "Pick an option first." : "Type an answer first.");
      return;
    }
    if (n === 1) return submit(ds);
    go(toReview || step === n - 1 ? n : step + 1);
  };

  const back = () => {
    if (trying) {
      setTrying(false);
      setHint("");
      return;
    }
    if (step > 0) go(step - 1);
  };

  const choose = (o: number, on?: boolean) => {
    if (!q) return;
    const next = withPick(drafts, step, q, o, on);
    setDrafts(next);
    setHint("");
    if (o >= q.options.length && next[step]?.other) wantOther.current = true;
  };

  const patch = (p: Partial<Draft>) => setDrafts((ds) => ds.map((x, j) => (j === step ? { ...x, ...p } : x)));

  const openSetup = () => {
    const at = q ? step : setup && eligible.includes(setup.q) ? setup.q : eligible[0];
    const question = at === undefined ? undefined : questions[at];
    if (at === undefined || !question) return;
    setSetup((s) => (s && s.q === at ? s : newSetup(at, question, pickIsolation(isolations, defaultIsolation), pipelines)));
    setTrying(true);
    setHint("");
  };

  const decline = () => {
    if (busy || !onDecline) return;
    saved.delete(sig);
    onDecline();
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLElement>) => {
    if (e.defaultPrevented || e.nativeEvent.isComposing) return;
    const t = e.target as HTMLElement;
    const mod = e.metaKey || e.ctrlKey;
    if (e.key === "Escape") {
      // The modal frames hear Escape through the dialog (onOpenChange).
      if (frame === "inline" && onMinimize) {
        e.preventDefault();
        onMinimize();
      }
      return;
    }
    if (e.key === "Enter") {
      if (trying) {
        if (mod) {
          e.preventDefault();
          start();
        }
        return;
      }
      if (t instanceof HTMLTextAreaElement) {
        if (e.shiftKey || mod) {
          e.preventDefault();
          advance(drafts);
        }
        return;
      }
      if (t instanceof HTMLSelectElement) return;
      if (t instanceof HTMLInputElement) {
        e.preventDefault();
        advance(drafts);
        return;
      }
      const row = t.closest<HTMLElement>("[data-opt]");
      if (row === t && q) {
        // Enter on an option: a single choice takes the one you're on and moves on; "Other…" opens for typing.
        e.preventDefault();
        const o = Number(row.dataset.opt);
        const isOther = o >= q.options.length;
        if (isOther && !d.other) return choose(o, true);
        if (q.multi || isOther) return advance(drafts);
        const next = withPick(drafts, step, q, o);
        setDrafts(next);
        advance(next);
        return;
      }
      if (t.closest("button, a, input, textarea, select")) return;
      e.preventDefault();
      advance(drafts);
      return;
    }
    const typing = t instanceof HTMLInputElement || t instanceof HTMLTextAreaElement || t instanceof HTMLSelectElement || t.isContentEditable;
    if (!trying && q && q.options.length > 0 && !typing && !mod && !e.altKey && /^[1-9]$/.test(e.key)) {
      const o = Number(e.key) - 1;
      if (o < q.options.length + (offersOther(q) ? 1 : 0)) {
        e.preventDefault();
        choose(o);
        if (o < q.options.length || q.multi) rootRef.current?.querySelector<HTMLElement>(`[data-opt="${o}"]`)?.focus();
      }
      return;
    }
    // Arrows move between a group's items (focus only: Space or a click picks).
    const keys = ["ArrowDown", "ArrowUp", "ArrowLeft", "ArrowRight", "Home", "End"];
    const group = t.closest<HTMLElement>("[data-roving]");
    if (keys.includes(e.key) && group && t.matches("[data-item]")) {
      const items = Array.from(group.querySelectorAll<HTMLElement>("[data-item]")).filter((el) => el.closest("[data-roving]") === group && !el.matches(":disabled"));
      const i = items.indexOf(t);
      const to = e.key === "Home" ? 0 : e.key === "End" ? items.length - 1 : e.key === "ArrowDown" || e.key === "ArrowRight" ? Math.min(i + 1, items.length - 1) : Math.max(i - 1, 0);
      e.preventDefault();
      items[to]?.focus();
    }
  };

  const modal = frame !== "inline";
  const title = agentName ?? kind ?? "The agent";
  const titleInner = (
    <>
      <span className="ch-agent">{title}</span> <PrismLabel className="ch-ask">asks you</PrismLabel>
    </>
  );
  const describedBy = trying ? `${uid}-x` : q ? `${uid}-q` : `${uid}-r`;
  const announce = trying
    ? `Experiment setup: ${tried?.header || tried?.question || ""}`
    : q
      ? n > 1
        ? `Question ${step + 1} of ${n}${q.header ? `, ${q.header}` : ""}`
        : q.question
      : "Review your answers";
  const primary = busy ? "Sending…" : trying ? "Start experiment" : !q || n === 1 ? "Submit" : toReview || step === n - 1 ? "Review" : "Next";

  const content = (
    <>
      <header className="ch-head">
        <div className="ch-who">
          <Optic state="waiting" size={16} />
          {modal ? (
            <D.Title className="ch-title">{titleInner}</D.Title>
          ) : (
            <h2 className="ch-title" id={`${uid}-t`}>
              {titleInner}
            </h2>
          )}
          {agentName && kind && <span className="ch-kind">{kind}</span>}
        </div>
        <div className="ch-head-actions">
          {canDecline && onDecline && (
            <button type="button" className="ch-decline" onClick={decline} disabled={busy}>
              Decline
            </button>
          )}
          {onMinimize && (
            <button type="button" className="ch-icon" onClick={onMinimize} aria-label="Minimize" title="Minimize: it keeps waiting for you">
              {frame === "sheet" ? <ChevronDown size={18} /> : <Minus size={16} />}
            </button>
          )}
        </div>
      </header>

      {n > 1 && !trying && (
        <div className="ch-steps">
          <span className="ch-count">{q ? `${step + 1} of ${n}` : "Review"}</span>
          <ol className="ch-chips-row" aria-label="Questions">
            {questions.map((x, i) => {
              const done = isAnswered(x, drafts[i] ?? BLANK);
              const current = i === step;
              return (
                <li key={i} className="ch-step" data-state={current ? "current" : done ? "done" : "todo"} aria-current={current ? "step" : undefined}>
                  {done && !current && <Check size={10} strokeWidth={2.5} aria-hidden="true" />}
                  {x.header || `Q${i + 1}`}
                  {done && <span className="sr-only"> (answered)</span>}
                </li>
              );
            })}
          </ol>
        </div>
      )}

      <div className="ch-body" ref={bodyRef}>
        <div className="ch-view" key={view}>
          {trying && setup && tried ? (
            <ExperimentSetup
              uid={uid}
              questions={questions}
              eligible={q ? [] : eligible}
              setup={setup}
              onChange={(p) => setSetup((s) => (s ? { ...s, ...p } : s))}
              onRetarget={(at) => {
                const question = questions[at];
                if (question) setSetup((s) => (s ? { ...s, ...optionPart(at, question) } : s));
              }}
              isolations={isolations}
              isolationId={isolationId}
              pipelines={pipelines}
            />
          ) : q ? (
            <QuestionStep
              uid={uid}
              q={q}
              d={d}
              single={n === 1}
              focused={focused}
              coarse={coarse}
              otherRef={otherRef}
              onPick={choose}
              onFocusRow={setFocused}
              onChange={patch}
            />
          ) : (
            <Review
              uid={uid}
              questions={questions}
              drafts={drafts}
              onEdit={(i) => {
                setToReview(true);
                go(i);
              }}
            />
          )}
        </div>
      </div>

      <footer className="ch-foot">
        {trying && spec ? (
          <p className="ch-runs" id={`${uid}-runs`}>
            <span>{describeRuns(variantCount(spec.options.length, spec.strategy, spec.variants.length), spec.baseline, spec.repetitions)}</span>
            {problem && <span className="ch-problem">{problem}</span>}
          </p>
        ) : (
          <Keys q={q} canMinimize={Boolean(onMinimize)} label={primary} />
        )}
        <div className="ch-actions">
          {!trying && canTry && (
            <Button variant="secondary" size="sm" onClick={openSetup} disabled={busy}>
              <FlaskConical /> Experiment…
            </Button>
          )}
          {(trying || step > 0) && (
            <Button variant="ghost" size="sm" onClick={back} aria-label="Back" className="ch-back">
              <ArrowLeft /> <span className="ch-back-label">Back</span>
            </Button>
          )}
          <Button
            size="sm"
            onClick={() => advance(drafts)}
            disabled={busy || (trying ? Boolean(problem) : !ready)}
            aria-describedby={trying ? `${uid}-runs` : undefined}
            className="ch-primary"
          >
            {trying && <FlaskConical />}
            {primary}
            {!busy && !trying && primary !== "Submit" && <ArrowRight />}
          </Button>
        </div>
      </footer>

      <p className="sr-only" aria-live="polite">
        {hint || announce}
      </p>
    </>
  );

  if (!modal)
    return (
      <section
        ref={setRoot}
        className={cn("ch", className)}
        data-layout="inline"
        data-wide={hasPreviews(questions) || undefined}
        aria-labelledby={`${uid}-t`}
        aria-describedby={describedBy}
        aria-busy={busy || undefined}
        onKeyDown={onKeyDown}
      >
        {content}
      </section>
    );

  return (
    <D.Root
      open
      onOpenChange={(open) => {
        if (!open) onMinimize?.();
      }}
    >
      <D.Portal>
        <D.Overlay className="ch-overlay" data-layout={frame} />
        <D.Content
          ref={setRoot}
          className={cn("ch", className)}
          data-layout={frame}
          data-wide={hasPreviews(questions) || undefined}
          aria-modal="true"
          aria-describedby={describedBy}
          aria-busy={busy || undefined}
          onOpenAutoFocus={(e) => {
            e.preventDefault();
            focusView();
          }}
          onKeyDown={onKeyDown}
        >
          {content}
        </D.Content>
      </D.Portal>
    </D.Root>
  );
}

const hasPreviews = (qs: ChoiceQuestion[]) => qs.some((q) => q.options.some((o) => o.preview));

/* ------------------------------------------------------------------ a question */

function QuestionStep({
  uid,
  q,
  d,
  single,
  focused,
  coarse,
  otherRef,
  onPick,
  onFocusRow,
  onChange,
}: {
  uid: string;
  q: ChoiceQuestion;
  d: Draft;
  /** The only question: its header has no stepper to sit in. */
  single: boolean;
  focused: number | null;
  coarse: boolean;
  otherRef: React.RefObject<HTMLInputElement | null>;
  onPick: (o: number) => void;
  onFocusRow: (o: number | null) => void;
  onChange: (p: Partial<Draft>) => void;
}) {
  const noteRef = React.useRef<HTMLTextAreaElement>(null);
  const noteWasOpen = React.useRef(d.noteOpen);
  const free = q.options.length === 0;
  const other = offersOther(q);
  const rows = q.options.length + (other ? 1 : 0);
  const previews = q.options.some((o) => o.preview);
  // The preview follows focus, else the latest pick that has one.
  const latest = [...d.picked].reverse().find((i) => q.options[i]?.preview);
  const pv = focused !== null && q.options[focused]?.preview ? focused : latest;
  const pvOption = pv === undefined ? undefined : q.options[pv];
  const first = d.other ? q.options.length : (d.picked.length > 0 ? Math.min(...d.picked) : 0);
  const stop = focused ?? first;
  const role = q.multi ? "checkbox" : "radio";
  const kind = free ? "In your words" : q.multi ? "Pick any" : "Pick one";

  // Opening the note puts you in it (coming back to an open one doesn't).
  React.useEffect(() => {
    if (d.noteOpen && !noteWasOpen.current) noteRef.current?.focus();
    noteWasOpen.current = d.noteOpen;
  }, [d.noteOpen]);

  const field = q.secret ? (
    <input
      ref={free ? undefined : otherRef}
      className="ch-input"
      type="password"
      autoComplete="off"
      value={d.text}
      onChange={(e) => onChange({ text: e.target.value })}
      placeholder={free ? "Type your answer" : "Your answer"}
      aria-label={free ? undefined : "Your answer"}
      aria-labelledby={free ? `${uid}-q` : undefined}
      data-autofocus={free && !coarse ? "" : undefined}
    />
  ) : free ? (
    <textarea
      className="ch-input ch-free"
      rows={3}
      value={d.text}
      onChange={(e) => onChange({ text: e.target.value })}
      placeholder="Type your answer"
      aria-labelledby={`${uid}-q`}
      data-autofocus={coarse ? undefined : ""}
    />
  ) : (
    <input
      ref={otherRef}
      className="ch-input"
      value={d.text}
      onChange={(e) => onChange({ text: e.target.value })}
      placeholder="Your answer"
      aria-label="Your answer"
      enterKeyHint="next"
    />
  );

  return (
    <div className="ch-q" data-previews={previews || undefined}>
      <div className="ch-q-head">
        <p className="ch-q-kind">
          {single && q.header && <span className="ch-q-header">{q.header}</span>}
          {kind}
        </p>
        <h3 className="ch-q-text" id={`${uid}-q`} tabIndex={-1} data-autofocus={free && coarse ? "" : undefined}>
          {q.question}
        </h3>
      </div>

      <div className="ch-q-main">
        {free ? (
          field
        ) : (
          <div className="ch-opts" role={q.multi ? "group" : "radiogroup"} aria-labelledby={`${uid}-q`} data-roving="">
            {q.options.map((o, i) => {
              const { text, recommended } = shown(o.label);
              const on = d.picked.includes(i);
              return (
                <div key={i} className="ch-opt" data-checked={on || undefined}>
                  <button
                    type="button"
                    role={role}
                    aria-checked={on}
                    aria-labelledby={`${uid}-o${i}`}
                    aria-describedby={o.description ? `${uid}-d${i}` : undefined}
                    aria-keyshortcuts={i < 9 ? String(i + 1) : undefined}
                    tabIndex={i === stop ? 0 : -1}
                    className="ch-row"
                    data-item=""
                    data-opt={i}
                    data-autofocus={i === first ? "" : undefined}
                    onClick={() => onPick(i)}
                    onFocus={() => onFocusRow(i)}
                    onBlur={() => onFocusRow(null)}
                  >
                    <span className="ch-mark" aria-hidden="true">
                      <Check size={11} strokeWidth={3} />
                    </span>
                    <span className="ch-text">
                      <span className="ch-label" id={`${uid}-o${i}`}>
                        {text}
                        {recommended && (
                          <>
                            {" "}
                            <span className="ch-rec">Recommended</span>
                          </>
                        )}
                      </span>
                      {o.description && (
                        <span className="ch-desc" id={`${uid}-d${i}`}>
                          {o.description}
                        </span>
                      )}
                    </span>
                    {i < 9 && (
                      <kbd className="ch-key" aria-hidden="true">
                        {i + 1}
                      </kbd>
                    )}
                  </button>
                  {pv === i && pvOption?.preview && <Preview text={pvOption.preview} className="ch-pv-under" />}
                </div>
              );
            })}
            {other && (
              <div className="ch-opt" data-checked={d.other || undefined}>
                <button
                  type="button"
                  role={role}
                  aria-checked={d.other}
                  aria-keyshortcuts={rows <= 9 ? String(rows) : undefined}
                  tabIndex={q.options.length === stop ? 0 : -1}
                  className="ch-row ch-row-other"
                  data-item=""
                  data-opt={q.options.length}
                  data-autofocus={first === q.options.length ? "" : undefined}
                  onClick={() => onPick(q.options.length)}
                  onFocus={() => onFocusRow(q.options.length)}
                  onBlur={() => onFocusRow(null)}
                >
                  <span className="ch-mark" aria-hidden="true">
                    <Check size={11} strokeWidth={3} />
                  </span>
                  <span className="ch-text">
                    <span className="ch-label">Other…</span>
                    {!d.other && <span className="ch-desc">An answer in your own words</span>}
                  </span>
                  {rows <= 9 && (
                    <kbd className="ch-key" aria-hidden="true">
                      {rows}
                    </kbd>
                  )}
                </button>
                {d.other && <div className="ch-other">{field}</div>}
              </div>
            )}
          </div>
        )}

        <div className="ch-note">
          <button type="button" className="ch-link" aria-expanded={d.noteOpen} aria-controls={`${uid}-note`} onClick={() => onChange({ noteOpen: !d.noteOpen })}>
            {d.noteOpen ? <Minus size={13} /> : <Plus size={13} />}
            {d.note.trim() ? "Note" : "Add a note"}
            {!d.noteOpen && d.note.trim() && <span className="ch-note-peek">{d.note.trim()}</span>}
          </button>
          {d.noteOpen && (
            <textarea
              ref={noteRef}
              id={`${uid}-note`}
              className="ch-input ch-note-field"
              rows={2}
              value={d.note}
              onChange={(e) => onChange({ note: e.target.value })}
              placeholder="Anything the agent should know with this answer"
              aria-label="Note for the agent"
            />
          )}
        </div>
      </div>

      {previews && (
        <aside className="ch-side" aria-label="Preview">
          {pvOption?.preview ? (
            <Preview text={pvOption.preview} caption={shown(pvOption.label).text} />
          ) : (
            <p className="ch-pv-empty">{focused !== null && focused < q.options.length ? "No preview for this option." : "Move to an option to preview it."}</p>
          )}
        </aside>
      )}
    </div>
  );
}

/** A preview, as the site's code blocks: it wraps (or, for a mockup, shrinks to fit) and only ever scrolls down. */
function Preview({ text, caption, className }: { text: string; caption?: string; className?: string }) {
  const cols = columns(text);
  return (
    <figure className={cn("ch-pv", className)}>
      {caption && <figcaption className="ch-pv-cap">Preview · {caption}</figcaption>}
      <pre className="ch-pv-pre" data-fit={cols > 0 && cols <= FIT_COLUMNS ? "" : undefined} style={{ "--ch-cols": cols } as React.CSSProperties} tabIndex={caption ? 0 : undefined}>
        {text}
      </pre>
    </figure>
  );
}

/** The footer's quiet keyboard legend (desktops only). */
function Keys({ q, canMinimize, label }: { q?: ChoiceQuestion; canMinimize: boolean; label: string }) {
  const rows = q ? q.options.length + (offersOther(q) ? 1 : 0) : 0;
  return (
    <p className="ch-keys" aria-hidden="true">
      {q && rows > 0 && (
        <span>
          <kbd>↑</kbd>
          <kbd>↓</kbd> move
        </span>
      )}
      {q && rows > 0 && (
        <span>
          <kbd>1</kbd>–<kbd>{Math.min(rows, 9)}</kbd> {q.multi ? "toggle" : "pick"}
        </span>
      )}
      <span>
        {q && q.options.length === 0 && !q.secret && <kbd>⇧</kbd>}
        <kbd>↵</kbd> {label.toLowerCase()}
      </span>
      {canMinimize && (
        <span>
          <kbd>esc</kbd> later
        </span>
      )}
    </p>
  );
}

/* ------------------------------------------------------------------ the review */

function Review({ uid, questions, drafts, onEdit }: { uid: string; questions: ChoiceQuestion[]; drafts: Draft[]; onEdit: (i: number) => void }) {
  return (
    <div className="ch-review">
      <h3 className="ch-q-text" id={`${uid}-r`} tabIndex={-1} data-autofocus="">
        Check your answers
      </h3>
      <ol className="ch-review-list">
        {questions.map((q, i) => {
          const a = answerOf(q, drafts[i] ?? BLANK);
          const picks = a.selected.map((l) => shown(l).text);
          return (
            <li key={i} className="ch-review-item">
              <p className="ch-review-q">
                <span className="ch-review-h">{q.header || `Q${i + 1}`}</span>
                <span>{q.question}</span>
              </p>
              <p className="ch-review-a">
                {picks.join(", ")}
                {picks.length > 0 && a.other !== undefined && ", "}
                {a.other !== undefined && (q.secret ? <span className="ch-secret">••••••••</span> : <q className="ch-own">{a.other}</q>)}
                {picks.length === 0 && a.other === undefined && <span className="ch-missing">Not answered</span>}
              </p>
              {a.note && (
                <p className="ch-review-note">
                  <span className="ch-review-h">Note</span> {a.note}
                </p>
              )}
              <button type="button" className="ch-link ch-edit" onClick={() => onEdit(i)} aria-label={`Edit your answer to ${q.header || q.question}`}>
                edit
              </button>
            </li>
          );
        })}
      </ol>
    </div>
  );
}

/* ------------------------------------------------------------------ the experiment setup */

function ExperimentSetup({
  uid,
  questions,
  eligible,
  setup,
  onChange,
  onRetarget,
  isolations,
  isolationId,
  pipelines,
}: {
  uid: string;
  questions: ChoiceQuestion[];
  /** From the review: the questions it could try (a picker when there are several). */
  eligible: number[];
  setup: Setup;
  onChange: (p: Partial<Setup>) => void;
  onRetarget: (q: number) => void;
  isolations: IsolationOption[];
  isolationId: string;
  pipelines: string[];
}) {
  const q = questions[setup.q];
  if (!q) return null;
  const labels = q.options.map((o) => shown(o.label).text);
  const inPlay = setup.on.filter(Boolean).length;
  const sets = customSets(q.options.length).filter((s) => s.every((i) => setup.on[i]));
  const picked = sets.filter((s) => setup.sets.includes(s.join("+"))).length;
  const size = variantCount(inPlay, setup.strategy, picked);
  const strategy = STRATEGIES.find((s) => s.id === setup.strategy);
  const setCriterion = (key: number, p: Partial<CriterionDraft>) => onChange({ criteria: setup.criteria.map((c) => (c.key === key ? { ...c, ...p } : c)) });
  const firstFree = isolations.findIndex((o) => o.available);

  return (
    <div className="ch-x">
      <div className="ch-x-head">
        <div className="ch-x-eyebrow">
          <span className="ch-rule" aria-hidden="true" />
          <PrismLabel>Experiment</PrismLabel>
        </div>
        <h3 className="ch-q-text" id={`${uid}-x`} tabIndex={-1} data-autofocus="">
          Try them instead of picking one
        </h3>
        <p className="ch-x-sub">Each variant runs in its own isolated workspace and is tested against your criteria; then they’re compared.</p>
        <p className="ch-x-of">{q.question}</p>
      </div>

      {eligible.length > 1 && (
        <Section id={`${uid}-xq`} title="Question">
          <div className="ch-chips" role="radiogroup" aria-labelledby={`${uid}-xq`} data-roving="">
            {eligible.map((i) => (
              <button
                key={i}
                type="button"
                role="radio"
                aria-checked={setup.q === i}
                tabIndex={setup.q === i ? 0 : -1}
                className="ch-chip"
                data-item=""
                onClick={() => setup.q !== i && onRetarget(i)}
              >
                {questions[i]?.header || `Q${i + 1}`}
              </button>
            ))}
          </div>
        </Section>
      )}

      <Section id={`${uid}-xo`} title="Options in play" aside={inPlay < 2 ? <span className="ch-warn">at least two</span> : `${inPlay} of ${q.options.length}`}>
        <div className="ch-chips" role="group" aria-labelledby={`${uid}-xo`} data-roving="">
          {q.options.map((_, i) => (
            <button
              key={i}
              type="button"
              role="checkbox"
              aria-checked={Boolean(setup.on[i])}
              className="ch-chip"
              data-item=""
              onClick={() => onChange({ on: setup.on.map((v, j) => (j === i ? !v : v)) })}
            >
              <span className="ch-chip-n">{i + 1}</span>
              {labels[i]}
            </button>
          ))}
        </div>
      </Section>

      <Section id={`${uid}-xs`} title="Strategy">
        <div className="ch-tiles" role="radiogroup" aria-labelledby={`${uid}-xs`} data-roving="">
          {STRATEGIES.map((s) => {
            const c = variantCount(inPlay, s.id, picked);
            return (
              <button
                key={s.id}
                type="button"
                role="radio"
                aria-checked={setup.strategy === s.id}
                tabIndex={setup.strategy === s.id ? 0 : -1}
                className="ch-tile"
                data-item=""
                onClick={() => onChange({ strategy: s.id })}
              >
                <span className="ch-tile-top">
                  <span className="ch-tile-label">{s.label}</span>
                  <span className="ch-tile-n" aria-label={`${c.atMost ? "up to " : ""}${c.count} variants`}>
                    {c.atMost ? "≤" : ""}
                    {c.count}
                  </span>
                </span>
              </button>
            );
          })}
        </div>
        <p className="ch-x-hint">{strategy?.hint}</p>
        {setup.strategy === "custom" && (
          <div className="ch-chips" role="group" aria-label="Sets to try" data-roving="">
            {sets.map((s, i) => {
              const key = s.join("+");
              const on = setup.sets.includes(key);
              const names = s.map((x) => labels[x]).join(" + ");
              return (
                <button
                  key={key}
                  type="button"
                  role="checkbox"
                  aria-checked={on}
                  aria-label={names}
                  title={names}
                  tabIndex={i === 0 ? 0 : -1}
                  className="ch-chip ch-set"
                  data-item=""
                  onClick={() => onChange({ sets: on ? setup.sets.filter((k) => k !== key) : [...setup.sets, key] })}
                >
                  {s.map((x) => x + 1).join("+")}
                </button>
              );
            })}
          </div>
        )}
        {size.count > MANY_VARIANTS && (
          <p className="ch-warn">
            {size.atMost ? "Up to " : ""}
            {size.count} variants is a long experiment; Pairs or Greedy stacking try fewer.
          </p>
        )}
        <button type="button" role="switch" aria-checked={setup.baseline} className="ch-switch" onClick={() => onChange({ baseline: !setup.baseline })}>
          <span className="ch-switch-track" aria-hidden="true" />
          <span>
            Include baseline <span className="ch-dim">(none of them)</span>
          </span>
        </button>
      </Section>

      <Section id={`${uid}-xi`} title="Isolation">
        <div className="ch-tiles" role="radiogroup" aria-labelledby={`${uid}-xi`} data-roving="">
          {isolations.map((o, i) => {
            const on = o.id === isolationId;
            return (
              <button
                key={o.id}
                type="button"
                role="radio"
                aria-checked={on}
                disabled={!o.available}
                tabIndex={on || (!isolationId && i === firstFree) ? 0 : -1}
                className="ch-tile"
                data-item=""
                onClick={() => onChange({ isolation: o.id })}
              >
                <span className="ch-tile-top">
                  <span className="ch-tile-label">{o.label}</span>
                  {!o.available && <span className="ch-tile-n">unavailable</span>}
                </span>
                {(o.description || !o.available) && <span className="ch-tile-desc">{o.description || "Not available here."}</span>}
              </button>
            );
          })}
        </div>
      </Section>

      <Section
        id={`${uid}-xc`}
        title="Criteria"
        aside={
          <button
            type="button"
            className="ch-link ch-add"
            onClick={() => onChange({ criteria: [...setup.criteria, newCriterion("metric", pipelines)] })}
          >
            <Plus size={13} /> Add
          </button>
        }
      >
        <ol className="ch-crits">
          {setup.criteria.map((c, i) => (
            <CriterionRow
              key={c.key}
              n={i + 1}
              c={c}
              pipelines={pipelines}
              canRemove={setup.criteria.length > 1}
              onChange={(p) => setCriterion(c.key, p)}
              onRemove={() => onChange({ criteria: setup.criteria.filter((x) => x.key !== c.key) })}
            />
          ))}
        </ol>
      </Section>

      <div className="ch-nums">
        <NumberField label="Repetitions" value={setup.repetitions} min={1} onChange={(repetitions) => onChange({ repetitions })} />
        <NumberField label="Max parallel" value={setup.maxParallel} min={1} onChange={(maxParallel) => onChange({ maxParallel })} />
        <NumberField label="Budget, min" value={setup.budget} min={1} placeholder="none" onChange={(budget) => onChange({ budget })} />
      </div>

      <Section id={`${uid}-xd`} title="Decision">
        <div className="ch-radios" role="radiogroup" aria-labelledby={`${uid}-xd`} data-roving="">
          {DECISIONS.map((o) => (
            <button
              key={o.id}
              type="button"
              role="radio"
              aria-checked={setup.decide === o.id}
              tabIndex={setup.decide === o.id ? 0 : -1}
              className="ch-radio"
              data-item=""
              onClick={() => onChange({ decide: o.id })}
            >
              <span className="ch-mark" aria-hidden="true" />
              <span>{o.label}</span>
            </button>
          ))}
        </div>
      </Section>

      <label className="ch-x-notes">
        <span className="ch-x-label">Notes</span>
        <textarea className="ch-input" rows={2} value={setup.notes} onChange={(e) => onChange({ notes: e.target.value })} placeholder="Anything the agent should know (optional)" />
      </label>
    </div>
  );
}

function Section({ id, title, aside, children }: { id: string; title: string; aside?: React.ReactNode; children: React.ReactNode }) {
  return (
    <section className="ch-x-sec" aria-labelledby={id}>
      <div className="ch-x-label">
        <h4 id={id}>{title}</h4>
        {aside && <span className="ch-x-aside">{aside}</span>}
      </div>
      {children}
    </section>
  );
}

/** Two or three choices side by side (radio semantics). */
function Seg<T extends string>({ label, value, options, onChange }: { label: string; value: T; options: { id: T; label: string }[]; onChange: (v: T) => void }) {
  return (
    <div className="ch-seg" role="radiogroup" aria-label={label} data-roving="">
      {options.map((o) => (
        <button key={o.id} type="button" role="radio" aria-checked={value === o.id} tabIndex={value === o.id ? 0 : -1} data-item="" onClick={() => onChange(o.id)}>
          {o.label}
        </button>
      ))}
    </div>
  );
}

function CriterionRow({
  n,
  c,
  pipelines,
  canRemove,
  onChange,
  onRemove,
}: {
  n: number;
  c: CriterionDraft;
  pipelines: string[];
  canRemove: boolean;
  onChange: (p: Partial<CriterionDraft>) => void;
  onRemove: () => void;
}) {
  const name = `Criterion ${n}`;
  return (
    <li className="ch-crit">
      <div className="ch-crit-main">
        <select className="ch-select" aria-label={`${name}: kind`} value={c.kind} onChange={(e) => onChange({ kind: e.target.value as Criterion["kind"] })}>
          <option value="pipeline">Pipeline</option>
          <option value="command">Command</option>
          <option value="metric">Metric</option>
        </select>
        {c.kind === "pipeline" && pipelines.length > 0 ? (
          <select className="ch-select ch-grow" aria-label={`${name}: pipeline`} value={c.pipeline} onChange={(e) => onChange({ pipeline: e.target.value })}>
            {!pipelines.includes(c.pipeline) && <option value={c.pipeline}>{c.pipeline || "Choose a pipeline"}</option>}
            {pipelines.map((p) => (
              <option key={p} value={p}>
                {p}
              </option>
            ))}
          </select>
        ) : (
          <input
            className="ch-input ch-grow ch-mono"
            aria-label={`${name}: ${c.kind === "pipeline" ? "pipeline name" : c.kind === "command" ? "command" : "metric name"}`}
            value={c.kind === "pipeline" ? c.pipeline : c.kind === "command" ? c.command : c.metric}
            onChange={(e) => onChange(c.kind === "pipeline" ? { pipeline: e.target.value } : c.kind === "command" ? { command: e.target.value } : { metric: e.target.value })}
            placeholder={c.kind === "pipeline" ? "pipeline name" : c.kind === "command" ? "cargo bench --bench parse" : "p95_ms"}
            spellCheck={false}
            autoCapitalize="off"
            autoComplete="off"
          />
        )}
        <button type="button" className="ch-icon" aria-label={`Remove ${name.toLowerCase()}`} disabled={!canRemove} onClick={onRemove}>
          <X size={15} />
        </button>
      </div>
      <div className="ch-crit-more">
        {c.kind === "metric" && (
          <>
            <Seg
              label={`${name}: better`}
              value={c.better}
              options={[
                { id: "lower", label: "Lower wins" },
                { id: "higher", label: "Higher wins" },
              ]}
              onChange={(better) => onChange({ better })}
            />
            <span className="ch-thr">
              <input
                className="ch-input"
                type="number"
                inputMode="decimal"
                step="any"
                aria-label={`${name}: threshold`}
                value={c.threshold}
                onChange={(e) => onChange({ threshold: e.target.value })}
                placeholder="threshold"
              />
              <select
                className="ch-select"
                aria-label={`${name}: threshold is`}
                value={c.thresholdType}
                onChange={(e) => onChange({ thresholdType: e.target.value as CriterionDraft["thresholdType"] })}
              >
                <option value="relative">% vs baseline</option>
                <option value="absolute">absolute</option>
              </select>
            </span>
          </>
        )}
        <Seg
          label={`${name}: role`}
          value={c.required ? "must" : "optimize"}
          options={[
            { id: "must", label: "Must pass" },
            { id: "optimize", label: "Optimize" },
          ]}
          onChange={(v) => onChange({ required: v === "must" })}
        />
      </div>
    </li>
  );
}

function NumberField({ label, value, min, placeholder, onChange }: { label: string; value: string; min: number; placeholder?: string; onChange: (v: string) => void }) {
  return (
    <label className="ch-num">
      <span className="ch-x-label">{label}</span>
      <input className="ch-input" type="number" inputMode="numeric" min={min} step={1} value={value} placeholder={placeholder} onChange={(e) => onChange(e.target.value)} />
    </label>
  );
}

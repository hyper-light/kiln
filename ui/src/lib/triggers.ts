/**
 * A workflow's triggers as Ship's Pipelines edits them (docs/ci.md W29-R06): Add a trigger's 18
 * kinds in six groups, each GitHub's, kiln's or both, with the key each dialect writes it under and
 * the lines it starts with; and a text's triggers as the pane's lines, worded as the server words
 * them (`kiln_ci::pipelines::trigger_line`), so the Triggers group follows what's typed.
 */
import { parseDocument } from "yaml";
import type { ShipPipelineLine } from "@/api/ship";

export interface TriggerKind {
  id: string;
  words: string;
  /** What it is and what it filters, as Add a trigger says it. */
  say: string;
  group: "Code" | "Time" | "People and agents" | "Other workflows" | "Deploys and releases" | "Outside kiln";
  whose: "github" | "kiln" | "both";
  /** The keys each dialect writes it under (none: a GitHub workflow can't say it). */
  gha: string[] | null;
  kiln: string[];
  /** Its first lines, by dialect. */
  first: (kiln: boolean) => unknown;
}

export const TRIGGER_KINDS: readonly TriggerKind[] = [
  { id: "push", words: "push", say: "a push to a branch or tag; branches, tags, paths", group: "Code", whose: "github", gha: ["push"], kiln: ["push"], first: () => ({ branches: ["main"] }) },
  {
    id: "pull_request",
    words: "pull request",
    say: "opened, updated, labelled, closed and 12 more; branches, paths",
    group: "Code",
    whose: "github",
    gha: ["pull_request"],
    kiln: ["pull_request"],
    first: () => ({ branches: ["main"] }),
  },
  {
    id: "diff",
    words: "diff",
    say: "each new version of a stacked diff; trunks, paths, drafts, human or agent",
    group: "Code",
    whose: "kiln",
    gha: null,
    kiln: ["diff"],
    first: () => ({ branches: ["main"] }),
  },
  { id: "land", words: "land", say: "the land queue's candidate, before it lands", group: "Code", whose: "kiln", gha: null, kiln: ["land"], first: () => ({}) },
  { id: "merge_group", words: "merge group", say: "a pull request in GitHub's merge queue", group: "Code", whose: "github", gha: ["merge_group"], kiln: ["merge_group"], first: () => ({}) },
  { id: "create", words: "create, delete", say: "a branch or tag made or removed", group: "Code", whose: "github", gha: ["create", "delete"], kiln: ["create", "delete"], first: () => ({}) },
  {
    id: "schedule",
    words: "schedule",
    say: "cron, with a time zone, skip if unchanged, catch up, jitter",
    group: "Time",
    whose: "both",
    gha: ["schedule"],
    kiln: ["schedule"],
    first: () => [{ cron: "0 3 * * *" }],
  },
  { id: "manual", words: "manual", say: "Run, kiln run or the API, with typed inputs", group: "People and agents", whose: "both", gha: ["workflow_dispatch"], kiln: ["manual"], first: () => ({}) },
  { id: "agent", words: "agent", say: "a kiln agent asks for a run; which agents, its inputs", group: "People and agents", whose: "kiln", gha: null, kiln: ["agent"], first: () => ({}) },
  {
    id: "comment",
    words: "comment",
    say: "a /command or mention on a diff, run or thread",
    group: "People and agents",
    whose: "kiln",
    gha: null,
    kiln: ["comment"],
    first: () => ({ commands: ["/run"] }),
  },
  { id: "review", words: "review", say: "accepted, changes asked, commented", group: "People and agents", whose: "both", gha: ["pull_request_review"], kiln: ["review"], first: () => ({}) },
  {
    id: "pipeline",
    words: "pipeline",
    say: "another workflow's run finishing; by name, result, branch",
    group: "Other workflows",
    whose: "both",
    gha: ["workflow_run"],
    kiln: ["pipeline"],
    first: (kiln) => (kiln ? { name: "ci", conclusion: ["success"] } : { workflows: ["ci"], types: ["completed"] }),
  },
  {
    id: "called",
    words: "called",
    say: "used by another workflow, with inputs, secrets, outputs",
    group: "Other workflows",
    whose: "github",
    gha: ["workflow_call"],
    kiln: ["workflow_call"],
    first: () => ({}),
  },
  {
    id: "deploy",
    words: "deploy",
    say: "an app synced, degraded or failed, in an environment",
    group: "Deploys and releases",
    whose: "kiln",
    gha: null,
    kiln: ["deploy"],
    first: () => ({ states: ["synced"] }),
  },
  { id: "release", words: "release", say: "a release published", group: "Deploys and releases", whose: "github", gha: ["release"], kiln: ["release"], first: () => ({ types: ["published"] }) },
  {
    id: "package",
    words: "package",
    say: "an image or package pushed to kiln's registry",
    group: "Deploys and releases",
    whose: "github",
    gha: ["registry_package"],
    kiln: ["registry_package"],
    first: () => ({}),
  },
  {
    id: "webhook",
    words: "webhook",
    say: "an inbound hook, filtered by an expression over its payload",
    group: "Outside kiln",
    whose: "kiln",
    gha: null,
    kiln: ["webhook"],
    first: () => ({ hook: "status-page" }),
  },
  {
    id: "dispatch",
    words: "dispatch",
    say: "an API call with an event type and a payload",
    group: "Outside kiln",
    whose: "github",
    gha: ["repository_dispatch"],
    kiln: ["repository_dispatch"],
    first: () => ({ types: ["deploy"] }),
  },
];

export const TRIGGER_GROUPS = ["Code", "Time", "People and agents", "Other workflows", "Deploys and releases", "Outside kiln"] as const;

/** An event's words, group and whose, as the server's lines say them (its EVENTS). */
const EVENT_WORDS: Record<string, [string, string, "both" | "github" | "kiln"]> = {
  push: ["push", "Code", "both"],
  pull_request: ["pull request", "Code", "both"],
  diff: ["diff", "Code", "kiln"],
  land: ["land", "Code", "kiln"],
  merge_group: ["merge group", "Code", "both"],
  create: ["create", "Code", "both"],
  delete: ["delete", "Code", "both"],
  schedule: ["schedule", "Time", "both"],
  workflow_dispatch: ["manual", "People and agents", "both"],
  agent: ["agent", "People and agents", "kiln"],
  comment: ["comment", "People and agents", "kiln"],
  review: ["review", "People and agents", "kiln"],
  pull_request_review: ["pull request review", "People and agents", "both"],
  workflow_run: ["pipeline", "Other workflows", "both"],
  pipeline: ["pipeline", "Other workflows", "kiln"],
  workflow_call: ["called", "Other workflows", "both"],
  deploy: ["deploy", "Deploys and releases", "kiln"],
  release: ["release", "Deploys and releases", "both"],
  registry_package: ["package", "Deploys and releases", "both"],
  webhook: ["webhook", "Outside kiln", "kiln"],
  repository_dispatch: ["dispatch", "Outside kiln", "both"],
};

type Js = Record<string, unknown>;
const obj = (v: unknown): Js => (v && typeof v === "object" && !Array.isArray(v) ? (v as Js) : {});
const strs = (v: unknown): string[] => (Array.isArray(v) ? v.map(String) : v === undefined || v === null ? [] : [String(v)]);

/** A trigger's filters in a line, in the server's order: types, branches, tags, paths, schedules, workflows, inputs, the rest's names. */
function filters(event: string, v: unknown): string {
  const o = obj(v);
  const parts: string[] = [];
  if (o.types) parts.push(strs(o.types).join(", "));
  for (const [k, w] of [
    ["branches", "branches"],
    ["branches-ignore", "not branches"],
    ["tags", "tags"],
    ["tags-ignore", "not tags"],
    ["paths", "paths"],
    ["paths-ignore", "not paths"],
  ] as const)
    if (strs(o[k]).length) parts.push(`${w} ${strs(o[k]).join(", ")}`);
  if (event === "schedule") for (const s of Array.isArray(v) ? v : []) parts.push(`${String(obj(s).cron)} ${String(obj(s).timezone ?? "UTC")}`);
  if (o.workflows || (event === "pipeline" && o.name)) parts.push(`after ${strs(o.workflows ?? o.name).join(", ")}`);
  if (o.inputs && event !== "schedule") parts.push(`inputs: ${Object.keys(obj(o.inputs)).join(", ")}`);
  const known = new Set(["types", "branches", "branches-ignore", "tags", "tags-ignore", "paths", "paths-ignore", "workflows", "inputs", ...(event === "pipeline" ? ["name"] : [])]);
  for (const k of Object.keys(o)) if (!known.has(k)) parts.push(k);
  return parts.join(" · ");
}

/** The YAML key each of a text's triggers is written under, by its event (kiln's `manual:` is GitHub's workflow_dispatch). */
export function triggerKeys(text: string, kiln: boolean): Map<string, string> {
  const out = new Map<string, string>();
  for (const [key] of entriesOf(text)) out.set(kiln && key === "manual" ? "workflow_dispatch" : key, key);
  return out;
}

function entriesOf(text: string): [string, unknown][] {
  const d = parseDocument(text);
  if (d.errors.length) return [];
  const on = obj(d.toJS()).on;
  return typeof on === "string" ? [[on, null]] : Array.isArray(on) ? on.map((e): [string, unknown] => [String(e), null]) : Object.entries(obj(on));
}

/** A text's triggers as the pane's lines, in its order, worded as the server words them; null where the text isn't YAML yet. */
export function triggerLines(text: string, kiln: boolean): ShipPipelineLine[] | null {
  const d = parseDocument(text);
  if (d.errors.length) return null;
  const on = obj(d.toJS()).on;
  const out: ShipPipelineLine[] = entriesOf(text)
    .filter(([e, v]) => !(kiln && e === "manual" && v === false))
    .map(([e, v]) => {
      const event = kiln && e === "manual" ? "workflow_dispatch" : e;
      const [words, group, whose] = EVENT_WORDS[event] ?? [event.replace(/_/g, " "), "Repository", "github"];
      return { key: `on.${event}`, kind: "trigger", name: words, value: filters(event, v), group, whose };
    });
  // kiln's dialect runs by hand unless `manual: false` says it doesn't (W02-R06).
  if (kiln && !out.some((l) => l.key === "on.workflow_dispatch") && obj(on).manual !== false)
    out.push({ key: "on.workflow_dispatch", kind: "trigger", name: "manual", value: "", group: "People and agents", whose: "both" });
  return out;
}

/**
 * Ship (plan 01a175b3a21174bc8769f5e784b90ecb; docs/ci.md W26-R01, R02): each commit of a
 * repository and where it stands, from `GET /api/ship/commits`. A change (a kiln diff or change
 * request, a host's pull request) is its commits: its tip carries its review, the rest fold under
 * it (`fold_under`). Every field is new on the wire, so each is optional here but the basics.
 */
import { apiPath } from "@/lib/peer-base";
import { request } from "./client";

export type ShipStand = "needs_you" | "failing" | "landing" | "checking" | "building" | "in_staging" | "waiting" | "shipping" | "replanting" | "shipped" | "fixed" | "rolled_back" | "superseded";

export interface ShipPip {
  stage: string;
  state: string;
  p?: number;
}

/** What a person can do from the row: one endpoint kiln has. */
export interface ShipAction {
  name: string;
  method: string;
  path: string;
  body: unknown;
}

export interface ShipChange {
  /** `D41`, `CR12`, `#5` (GitHub, Bitbucket), `!5` (GitLab). */
  name: string;
  kind: "diff" | "request";
  host: string;
  title: string;
  revision: number;
  latest: number;
  state: string | null;
  turn: string[];
  open: { question: number; nit: number; issue: number; blocking: number } | null;
  superseded_by: string | null;
}

export interface ShipCommit {
  repo: string;
  sha: string;
  short: string;
  message: string;
  author: string;
  branch: string;
  /** Unix seconds. */
  at: number;
  stand: ShipStand | string;
  line: string;
  pips: ShipPip[];
  environments?: { environment: string; production: boolean }[];
  tags?: string[];
  runs?: { id: string; pipeline: string }[];
  cursor: number;
  kind?: "diff" | "request" | "commit";
  host?: string | null;
  change?: ShipChange | null;
  /** The change's tip this commit folds under. */
  fold_under?: string | null;
  /**
   * Who made it, as far as kiln knows (ship.rs's made_by, W26-R18): the agent session that made it,
   * and the person it traces back to. Hand off goes to `agent`, and Files' ask asks it. Today only a
   * kiln change says it; Chain of Ownership fills it for every commit in time.
   */
  made_by?: { agent?: string; person?: string } | null;
  actions?: ShipAction[];
  /** A hash of what the row shows (W26-R06): the same row, the same `v`. */
  v?: string;
}

/**
 * The pace line's words (plan 01a1856fe81375b1bd1d1d86f270a45b, step 5): the commits in flight
 * (landing, checking, building) and the agents behind them; what each environment of each
 * repository in view runs now, production with how many commits it's behind; and how many of the
 * connected runners run production's commit (null when none does).
 */
export interface ShipPace {
  in_flight: number;
  agents: number;
  envs: { repo: string; environment: string; short: string; behind: number | null }[];
  runners: { on: number; of: number } | null;
}

export interface ShipPage {
  commits: ShipCommit[];
  next_cursor: number | null;
  /** With a page the server read from memory. */
  pace?: ShipPace;
}

/** A build part's state (`Part` in kiln_cd::journey): `none` when nothing built it. */
export type ShipPart = "queued" | "running" | "passed" | "failed" | "superseded" | "none";

/**
 * A mark of a lane group (W26-R14): a push, replay, comment, fix, resolution or approval with its
 * time (`end` null while under way, = `at` for a point, unix seconds), who did it, its revision and
 * its signer — null until W27-R03 signs kiln's records, so the seal says unsigned, never a false
 * one. `state` is a GlyphState name.
 */
export interface ShipMark {
  id: string;
  kind: "push" | "replay" | "comment" | "fix" | "resolution" | "approval";
  at: number;
  end: number | null;
  by: string;
  signer: string | null;
  revision: number | null;
  state: string;
  thread?: string;
  note?: string;
}

/** A commit with every record its stand came from: `GET /api/ship/commits/{repo}/{sha}`. */
export interface ShipDetail extends ShipCommit {
  /** The Review lane group's marks, oldest first; a commit with no change has the one, its push. */
  review?: ShipMark[];
  checks?: { pipeline: string; legs: { name: string; state: string; attempts: number; note: string; job?: string; run?: string; start?: number; end?: number }[] }[];
  build?: { platforms: { name: string; state: ShipPart }[]; image: { state: ShipPart; digest: string | null } };
  deployments?: { environment: string; production: boolean; state: string | null; description: string | null; at: number }[];
  releases?: { state: string; message: string | null; rolled_back: boolean }[];
  replant?: { name: string; state: string; why: string }[];
}

/** What a list asks for: a repository, the facets (W26-R02), a page. */
export interface ShipQuery {
  repo?: string;
  host?: string;
  kind?: string;
  /** `review`: the changes' commits only. */
  lens?: string;
  limit?: number;
  cursor?: number;
}

/** What a Pipelines row is (docs/ci.md W29-R02): a workflow, a reusable one, or an action by its `runs.using`. */
export type ShipPipelineKind = "workflow" | "reusable" | "composite" | "javascript" | "docker" | "firecracker" | "shards" | "unknown";

/**
 * A draft of a workflow or action (W29-R10): how far it has come (`unpublished`, or `review` when
 * it asks someone else to review it; its author sees it unpublished still), whose it is, and the
 * change it's saved as: a kiln diff (`D12`), or a host's pull or merge request (`#5`, `!5`).
 */
export interface ShipPipelineDraft {
  id: string;
  state: "not_saved" | "unpublished" | "review" | "update" | string;
  author: string;
  change: { kind: "diff" | "change"; name: string; url: string | null } | null;
}

/**
 * A workflow or action of a repository, as Ship's Pipelines lists it (W29-R02), at its default
 * branch's head: its stand and word (`Can't run`, `Failing`, `Released`, …), its line, and its pips
 * as glyph names, Checks, Runs on and Latest in that order.
 */
export interface ShipPipelineRow {
  /** `owner/name:path`. */
  key: string;
  kind: ShipPipelineKind | string;
  /** A workflow's: `gha` or `kiln`; null for an action. */
  dialect: "gha" | "kiln" | null;
  name: string;
  path: string;
  stand: ShipStand | string;
  word: string;
  line: string;
  pips: [string, string, string];
  /** Who changed its file last, and when (unix seconds). */
  who: string | null;
  when: number | null;
  ref: string;
  commit: string;
  /** The repository's newest full-version tag, for an action or reusable workflow. */
  released: string | null;
  /** Its draft (W29-R10), marked DRAFT beside its title; null for what's published. */
  draft: ShipPipelineDraft | null;
  /** A hash of what the row shows: the same row, the same `v`. */
  v: string;
}

/** A repository's rows, with its heading's words (`3 actions · 1 workflow`) and how many need someone. */
export interface ShipPipelineRepo {
  repo: string;
  say: string;
  needs: number;
  /** W29-R17's Access: `none` until sharing is built. */
  access: string;
  ref: string;
  commit: string;
  rows: ShipPipelineRow[];
}

/** A line of the pane's groups (W29-R04): a trigger, job, use, secret's or variable's name, run, setting, input, output, step, … */
export interface ShipPipelineLine {
  key: string;
  kind: string;
  name: string;
  value: string;
  /** A glyph's name, where the line has a state. */
  state?: string;
  /** A trigger's group (`Code`, `Time`, …) and whose event it is (W29-R06). */
  group?: string;
  whose?: "both" | "github" | "kiln";
}

export interface ShipPipelineGroup {
  name: string;
  lines: ShipPipelineLine[];
}

/** A verb of the pane's strip: dimmed with why while it isn't built, never hidden. */
export interface ShipPipelineVerb {
  verb: "run" | "save_draft" | "ready" | "discard" | "release" | string;
  label: string;
  enabled: boolean;
  why_not?: string;
  /** What it asks before it acts: Release's Keep · Release, saying what it changes (W29-R21). */
  asks?: string;
}

export interface ShipPipelineStrip {
  said: string[];
  pick: string[];
  verbs: ShipPipelineVerb[];
}

/** The file the pane opens, and the views it has (`YAML`, `Flow`, `Plan`; an action's `Changes`, `YAML`, `Try it`). */
export interface ShipPipelineDetail {
  name: string;
  views: string[];
  text: string;
}

/** A workflow or action opened (W29-R04): `GET /api/ship/pipelines/{owner}/{name}?path=&ref=`. */
export interface ShipPipelinePane {
  head: {
    title: string;
    path: string;
    kind: ShipPipelineKind | string;
    dialect: "gha" | "kiln" | null;
    ref: string;
    commit: string;
    draft: ShipPipelineDraft | null;
    stand: ShipStand | string;
    word: string;
    pips: [string, string, string];
  };
  strip: ShipPipelineStrip;
  groups: ShipPipelineGroup[];
  detail: ShipPipelineDetail;
  repo: string;
}

/**
 * A strip's verb (W29-R04), as `POST /api/ship/pipelines` takes it: on your own work, each as its
 * own operation and with its right. Save draft takes `content` and `title`; Run takes `event` (a
 * draft's), `jobs`, `inputs`, and a published workflow's `ref` or `sha`.
 */
export interface ShipPipelineAct {
  repo: string;
  path: string;
  verb: "save_draft" | "ready" | "discard" | "run";
  content?: string;
  title?: string;
  event?: string;
  jobs?: string[];
  inputs?: Record<string, unknown>;
  ref?: string;
  sha?: string;
}

export function shipSearch(q: ShipQuery): string {
  const p = new URLSearchParams();
  for (const [k, v] of Object.entries(q)) if (v !== undefined && v !== "") p.set(k, String(v));
  const s = p.toString();
  return s ? `?${s}` : "";
}

export const shipApi = {
  commits: (q: ShipQuery, signal?: AbortSignal) => request<ShipPage>("GET", apiPath(`/api/ship/commits${shipSearch(q)}`), undefined, { signal }),
  /** One commit and its records; `repo` is `owner/name` or its id, `sha` seven characters or more. */
  commit: (repo: string, sha: string, signal?: AbortSignal) =>
    request<ShipDetail>("GET", apiPath(`/api/ship/commits/${repo.split("/").map(encodeURIComponent).join("/")}/${encodeURIComponent(sha)}`), undefined, { signal }),
  /** A row's action, through the endpoint it names. */
  act: (a: ShipAction) => request<unknown>(a.method, apiPath(a.path), a.method === "GET" ? undefined : (a.body ?? {})),
};

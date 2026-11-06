/**
 * Ship (plan 01a175b3a21174bc8769f5e784b90ecb; docs/ci.md W26-R01, R02): each commit of a
 * repository and where it stands, from `GET /api/ship/commits`. A change (a kiln diff or change
 * request, a host's pull request) is its commits: its tip carries its review, the rest fold under
 * it (`fold_under`). Every field is new on the wire, so each is optional here but the basics.
 */
import { apiPath } from "@/lib/peer-base";
import { browserSigns, signed } from "@/lib/signing";
import { request } from "./client";
import type { CdPlan } from "./types";

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
  /**
   * When its jobs ran (unix seconds): the first one's start to the last one's end, `to` null while
   * one runs; null when none has started (docs/ci.md W26-R29).
   */
  ran?: { from: number; to: number | null } | null;
  /** The machines its jobs ran on, in the order they started. */
  machines?: string[];
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
  /** Whom its rows were picked for (a `ship` frame's `whose` is held against it); a server that
   * sends `ship` frames says it (W21-R15, views/ship/live.ts). */
  viewer?: string[];
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
  build?: { platforms: { name: string; state: ShipPart; job?: string; run?: string; start?: number; end?: number }[]; image: { state: ShipPart; digest: string | null } };
  deployments?: { environment: string; production: boolean; state: string | null; description: string | null; at: number }[];
  releases?: { state: string; message: string | null; rolled_back: boolean }[];
  replant?: { name: string; state: string; why: string }[];
  /** Every job of the commit, either engine's, as the jobs pane acts on them (docs/ci.md W26-R21, R22). */
  jobs?: ShipJob[];
  /** The version of what it shows beyond its row: a `ship` frame of the same `dv` changes only the row's part (W21-R15). */
  dv?: string;
  /** As a page's. */
  viewer?: string[];
}

/** What can be done to a commit's jobs together (`POST /api/ship/jobs`). */
export type ShipVerb = "retry" | "pause" | "resume" | "cancel";

/** A commit's job: its run, its lane's pipeline and name, where it runs, whether it's paused, and
 * the verbs that apply now (and why each other doesn't). */
export interface ShipJob {
  id: string;
  run: string;
  pipeline: string;
  name: string;
  runner?: string;
  paused_at?: number;
  verbs: ShipVerb[];
  why_not: Partial<Record<ShipVerb, string>>;
}

/** What one job said to a verb: done, or why not. */
export interface ShipJobDone {
  job: string;
  run?: string;
  done: boolean;
  why?: string;
}

/** What a list asks for: a repository, the facets (W26-R02), a page. */
export interface ShipQuery {
  /** `repo`: each repository's newest commit, Ship's rows (docs/ci.md W26-R29). */
  by?: "repo";
  repo?: string;
  host?: string;
  kind?: string;
  /** `review`: the changes' commits only. */
  lens?: string;
  limit?: number;
  cursor?: number;
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
  /**
   * A row's Hand off, the person's side signed in this browser (docs/ci.md W27-R08): kiln says what
   * to sign (`…/statement`: their hand-on to kiln's server, which passes it on to the agent that
   * takes it), and it goes with this browser's own key (lib/signing). Where this browser can't sign,
   * it says why (`unsigned`), and kiln's take for them gives the same reason.
   */
  handOff: async (a: ShipAction): Promise<{ answer: unknown; unsigned?: string }> => {
    const body = (a.body && typeof a.body === "object" ? a.body : {}) as Record<string, unknown>;
    let why = "this page isn't secure (https), so this browser has no key";
    if (browserSigns()) {
      try {
        const hand_on = await signed(apiPath(`${a.path}/statement`), body);
        if (hand_on) return { answer: await request<unknown>("POST", apiPath(a.path), { ...body, hand_on }) };
        why = "kiln is from before a browser signed its hand-offs";
      } catch (e) {
        why = `this browser couldn't sign: ${e instanceof Error ? e.message : String(e)}`;
      }
    }
    return { answer: await request<unknown>("POST", apiPath(a.path), { ...body, unsigned: why }), unsigned: why };
  },
  /** A commit's jobs run again, paused, resumed or cancelled together: those named, or those a word picks. */
  jobs: (body: { repo: string; sha: string; verb: ShipVerb; jobs?: string[]; scope?: ShipScope }) => request<{ verb: ShipVerb; jobs: ShipJobDone[] }>("POST", apiPath("/api/ship/jobs"), body),
  /** Every KilnCD app by project, needs-you first (docs/ci.md W26-R24); `q` finds by project, app, environment, cluster or namespace. */
  deploys: (q: string, signal?: AbortSignal) => request<ShipDeploys>("GET", apiPath(`/api/ship/deploys${q ? `?q=${encodeURIComponent(q)}` : ""}`), undefined, { signal }),
  /** An opened app's pane: its id is `project/name`. */
  deploy: (app: string, signal?: AbortSignal) => request<ShipDeployPane>("GET", apiPath(`/api/ship/deploys/${app.split("/").map(encodeURIComponent).join("/")}`), undefined, { signal }),
  /** What syncing a commit of `repo` would do to its apps against what runs (`cd.plans.get`, counts and keys only). */
  plan: (repo: string, commit: string, signal?: AbortSignal) =>
    request<CdPlan>("GET", apiPath(`/api/cd/plans?repo=${encodeURIComponent(repo)}&commit=${encodeURIComponent(commit)}&brief=true`), undefined, { signal }),
  /** One of the strip's verbs on an app, as its CD operation. */
  deployAct: (body: ShipDeployAct) => request<{ app: string; verb: ShipDeployVerb; op: string; done: unknown }>("POST", apiPath("/api/ship/deploys"), body),
};

/**
 * Ship's Deploys (docs/ci.md W26-R24, `crates/kiln/src/ship/deploys.rs`): every KilnCD app as Ship's
 * rows, by project, where each stands as `kiln_cd::stands` says; an opened app's pane; and the strip's
 * verbs. Every field is new on the wire, so each is optional here but the basics.
 */
export interface ShipDeployRow {
  /** Its id, `project/name`: what the pane and the verbs take. */
  app: string;
  name: string;
  /** The repository it deploys from (`owner/name`), else its Argo CD project. */
  project: string;
  environment: string | null;
  cluster: string;
  namespace: string;
  stand: ShipStand | string;
  /** The stand as a word (`Out of sync`, `Rolled back`). */
  word: string;
  line: string;
  /** Its latest deploy's Sync, Health and Soak. */
  pips: ShipPip[];
  /** Who did what it shows (a person, `agent:…`, `auto-sync`, `auto-rollback`), and when (unix seconds; 0 when nothing has). */
  by: string | null;
  at: number;
  /** What runs: its commit, short, or its chart's version. */
  runs: string | null;
  sync: string;
  health: string;
  auto_sync: boolean;
  pinned: { to: number; revision: string | null; by: string | null; at_ms: number } | null;
  v?: string;
}

export interface ShipDeployProject {
  project: string;
  /** Its heading's words: `2 apps · 1 waits on you`. */
  say: string;
  /** Its most pressing app needs someone. */
  needs: boolean;
  apps: ShipDeployRow[];
}

export interface ShipDeploys {
  projects: ShipDeployProject[];
  total: number;
  at_ms: number;
}

/** One of an app's objects as a line; `key` is the `:key` of `/api/cd/apps/:app/resources/:key`. */
export interface ShipDeployItem {
  key: string;
  group: string;
  kind: string;
  namespace: string;
  name: string;
  state: string;
  v: string;
  logs: boolean;
  differs: boolean;
  /** A pod's node. */
  node?: string;
  /** The container a pod's logs read. */
  container?: string;
}

/** Its objects of one owner (`Deployment kiln`) or one sort (`Network`, `Config`, …). */
export interface ShipDeployGroup {
  name: string;
  note: string;
  owner: ShipDeployItem | null;
  items: ShipDeployItem[];
}

/** A line of its history: a deploy (`deploy`), or a commit of its source none ran. */
export interface ShipDeployLine {
  deploy: number | null;
  sha: string | null;
  short: string | null;
  subject: string;
  by: string | null;
  at_ms: number;
  state: string;
  /** It runs now. */
  now: boolean;
  /** An older deploy than what runs. */
  dim: boolean;
  /** Where Roll back takes the app to go to it. */
  to: number | null;
  revision: string | null;
  same_as: number | null;
}

export interface ShipDeployStep {
  n: number;
  state: string;
  what: string;
  v: string;
  /** The step under way's analysis as measured: each check's latest measurement. */
  detail?: string;
}

export interface ShipDeployRollout {
  id: string;
  revision: string | null;
  state: string;
  note: string;
  steps: ShipDeployStep[];
  under_way: boolean;
  waits: string | null;
  approvals?: string[];
  /** The deploy it rolls out, the one it goes back to, and the new one's share of traffic (%). */
  deploy?: number | null;
  back_to?: number | null;
  weight?: number;
  /** `canary` or `blue/green`; how it's going in a few words; the share Promote moves it to now. */
  strategy?: string;
  said?: string;
  promote_to?: number | null;
  /** On a cluster: its workload's ReplicaSet that ran (`stable`) and the newest (`canary`). */
  sides?: { side: "stable" | "canary"; name: string; pods: number; share: number; up: string | null }[];
}

export interface ShipDeploySetting {
  key: "auto_sync" | "auto_rollback" | "prune" | "self_heal" | string;
  on: boolean;
  /** Whose word it is: `app` (a person's), `deploy-file`, `repo`, `default`. */
  from: string;
  what: string;
  /** A person may change it here. */
  can: boolean;
}

export interface ShipDeployPane {
  app: ShipDeployRow;
  /** Where it deploys, how many objects it has in git, where they come from, and its source's repository (to plan a commit of). */
  about: { cluster: string; namespace: string; objects: number; source: string; repo?: string | null };
  groups: ShipDeployGroup[];
  tree: { live: boolean; errors: string[] };
  rollout: ShipDeployRollout | null;
  history: ShipDeployLine[];
  settings: ShipDeploySetting[];
  /** Why each verb can't apply now; a verb missing from it can. */
  why_not: Partial<Record<ShipDeployVerb, string>>;
}

export type ShipDeployVerb = "sync" | "refresh" | "rollback" | "stop" | "follow" | "promote" | "abort" | "approve" | "deny" | "auto_sync" | "auto_rollback" | "delete";

/** A verb's body: the app, the verb, and only what that verb takes. */
export interface ShipDeployAct {
  app: string;
  verb: ShipDeployVerb;
  /** Sync: the picked objects' keys. */
  resources?: string[];
  prune?: boolean;
  /** Refresh: render again too. */
  hard?: boolean;
  /** Roll back: to a deployment, or to any commit. */
  to?: number;
  revision?: string;
  full?: boolean;
  reason?: string;
  check?: string;
  rollout?: string;
  /** auto_sync, auto_rollback. */
  on?: boolean | null;
  /** Delete: `foreground` deletes its objects too, `orphan` leaves them in the cluster. */
  cascade?: "foreground" | "orphan";
}

/** The words the jobs pane picks by. */
export type ShipScope = "failed" | "running" | "waiting" | "all";

/**
 * Agents attached to what they look after, and their incidents (docs/ci.md W20-R08 to R10): a
 * repository, a workflow, an environment, a runner pool or the runners; kiln wakes the one
 * attached when something happens there, and it reports back on the incident.
 */
import type { Res } from "@/lib/data";
import { request } from "./client";

/** The agent an attachment starts for each incident. */
export interface AttachHarness {
  kind: string;
  /** A machine's name, or what it needs (`linux/amd64`, `+gpu`). */
  on?: string;
  /** The folder it starts in there. */
  cwd?: string;
  model?: string;
  effort?: string;
  mode?: string;
}

export interface AttachLimits {
  /** Incidents it takes on in a day; past them, an incident waits for a person. */
  sessions_per_day: number;
  reruns_per_failure: number;
  /** How long a job waits for a runner before `stuck` wakes it. */
  stuck_minutes: number;
  /** How much slower than its recent median a run's critical path gets, in percent, before `slow` wakes it. */
  slow_percent?: number;
  /** Tokens its agents spend in a day, and runner minutes their runs take: past either, an incident waits for a person. */
  tokens_per_day?: number;
  runner_minutes_per_day?: number;
}

export interface Attachment {
  id: string;
  name: string;
  /** `repo:<owner>/<repo>`, `workflow:<owner>/<repo>:<path>`, `environment:<owner>/<repo>:<env>`, `pool:<name>`, `runner:<name>` or `runners`. */
  on: string;
  branches: string[];
  /** On a repository: only these workflows wake it. */
  workflows?: string[];
  /** On a repository: only deployments to these environments wake it. */
  environments?: string[];
  /** Kept in a repository (`<owner>/<repo>:.kiln/agents.yaml`), changed there; none: kept here. */
  source?: string;
  harness?: AttachHarness;
  /** A live agent it tells instead. */
  session?: string;
  wake: string[];
  schedule?: string;
  can: string[];
  limits: AttachLimits;
  playbook: string;
  reviewers: string[];
  /** Whom its agents act for. */
  by: string;
  paused: boolean;
  created_at: number;
  updated_at: number;
}

/** One time an incident happened. */
export interface IncidentSeen {
  at: number;
  what: string;
  url?: string;
  run?: string;
}

/** An incident as lists show it. */
export interface IncidentBrief {
  id: string;
  attachment: string;
  name: string;
  on: string;
  wake: string;
  /** The repository and the runner it is about, when it is about one. */
  repo?: string | null;
  runner?: string | null;
  title: string;
  /** `open` (its agent is on it), `reported`, `resolved`, `waiting` (for a person: `why`), `dismissed`. */
  state: "open" | "reported" | "resolved" | "waiting" | "dismissed";
  agent?: string | null;
  /** flaky, infrastructure, regression, pipeline, dependency or other. */
  class?: string | null;
  report?: string | null;
  links: string[];
  why?: string | null;
  times: number;
  last?: IncidentSeen | null;
  at: number;
  updated_at: number;
}

/** An attachment, with its open incidents and how many it took on today. */
export interface AttachmentRow {
  attachment: Attachment;
  open: number;
  today: number;
  latest: IncidentBrief | null;
}

/** What wakes one that looks after code; one that looks after runners. */
export const WAKES_CODE = ["failure", "flaky", "ejected", "stuck", "slow", "unroutable", "degraded", "mention", "schedule"] as const;
export const WAKES_RUNNERS = ["offline", "upgrade", "check", "schedule"] as const;
/** What its agent may do beyond reading what it looks after (W20-R09). */
export const CAN = ["rerun", "cancel", "diagnose", "fix", "edit-pipelines", "quarantine", "comment", "experiment", "manage-runners", "deploy", "land"] as const;

const enc = encodeURIComponent;

/** The attachments page, its form open on what `on` names (`repo:acme/app`, `environment:acme/app:production`, …): a page's "Attach agent". */
export const attachHref = (on: string) => `/agents/attached?on=${enc(on)}`;

export type AttachmentInput = Omit<Attachment, "id" | "by" | "created_at" | "updated_at" | "paused"> & { paused?: boolean; attachment?: string };

export const attachApi = {
  list: (signal?: AbortSignal) => request<{ attachments: AttachmentRow[] }>("GET", "/api/attachments", undefined, { signal }).then((r) => r.attachments),
  save: (a: AttachmentInput) =>
    request<{ attachment: Attachment }>(a.attachment ? "PUT" : "POST", a.attachment ? `/api/attachments/${enc(a.attachment)}` : "/api/attachments", a).then((r) => r.attachment),
  remove: (id: string) => request<{ removed: string }>("DELETE", `/api/attachments/${enc(id)}`),
  pause: (body: { attachment?: string; on?: string; paused: boolean }) => request<{ paused: boolean; attachments: string[] }>("POST", "/api/attachments/pause", body),
  incidents: (signal?: AbortSignal) => request<{ incidents: IncidentBrief[] }>("GET", "/api/incidents?limit=100", undefined, { signal }).then((r) => r.incidents),
  /** The incidents a run is part of (its incident card). */
  ofRun: (run: string, signal?: AbortSignal) => request<{ incidents: IncidentBrief[] }>("GET", `/api/incidents?run=${enc(run)}`, undefined, { signal }).then((r) => r.incidents),
  report: (id: string, body: { class?: string; summary?: string; links?: string[]; state?: string }) => request<{ incident: IncidentBrief }>("POST", `/api/incidents/${enc(id)}/report`, body),
};

export const attachmentsRes: Res<AttachmentRow[]> = { key: "attachments", fetch: (s) => attachApi.list(s), opts: { on: ["attachment", "incident"] } };
export const incidentsRes: Res<IncidentBrief[]> = { key: "incidents", fetch: (s) => attachApi.incidents(s), opts: { on: ["incident"] } };

/** What an attachment looks after, in words: `acme/app`, `ci.yml of acme/app`, `every runner`. */
export function onWords(on: string): string {
  if (on === "runners") return "every runner";
  const [kind, rest = ""] = on.split(/:(.*)/s);
  switch (kind) {
    case "repo":
      return rest;
    case "workflow": {
      const [repo, path = ""] = rest.split(/:(.*)/s);
      return `${path.split("/").pop() || path} of ${repo}`;
    }
    case "environment": {
      const [repo, env = ""] = rest.split(/:(.*)/s);
      return `${env} of ${repo}`;
    }
    case "pool":
      return `the ${rest} pool`;
    case "runner":
      return rest;
    default:
      return on;
  }
}

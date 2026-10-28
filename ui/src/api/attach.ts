/**
 * Agents attached to what they look after, and their incidents (docs/ci.md W20-R08 to R10): a
 * repository, a workflow, an environment, a runner pool or the runners; kiln wakes the one
 * attached when something happens there, and it reports back on the incident.
 */
import type { Res } from "@/lib/data";
import { request } from "./client";
import type { AgentAttachment, IncidentBrief } from "./wire";
export type { AgentAttachment, AttachHarness, AttachLimits, IncidentBrief, IncidentSeen } from "./wire";

/** An attachment, with its open incidents and how many it took on today. */
export interface AttachmentRow {
  attachment: AgentAttachment;
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

export type AttachmentInput = Omit<AgentAttachment, "id" | "by" | "created_at" | "updated_at" | "paused"> & { paused?: boolean; attachment?: string };

export const attachApi = {
  list: (signal?: AbortSignal) => request<{ attachments: AttachmentRow[] }>("GET", "/api/attachments", undefined, { signal }).then((r) => r.attachments),
  save: (a: AttachmentInput) =>
    request<{ attachment: AgentAttachment }>(a.attachment ? "PUT" : "POST", a.attachment ? `/api/attachments/${enc(a.attachment)}` : "/api/attachments", a).then((r) => r.attachment),
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

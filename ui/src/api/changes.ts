/**
 * Agents' change requests (docs/agentic.md C1–C4): an agent's work pushed for review, and where it
 * stands — in kiln's own review, or as a pull request on the repository's own host.
 */
import type { Res } from "@/lib/data";
import { request } from "./client";

export interface AgentChange {
  agent: string;
  /** The repository as kiln knows it (`owner/name`). */
  repo?: string;
  /** `kiln`, `github`, `gitlab`, `bitbucket`, or `git` (a remote kiln has no integration with). */
  provider: string;
  /** `CR12`, `#34`, or none: the branch is pushed, the pull request not opened yet. */
  change?: string;
  /** Its page, or where to open it. */
  url: string;
  branch: string;
  trunk: string;
  sha: string;
  version: number;
  files: string[];
  title: string;
  /** `open`, `landed` or `closed`. */
  state: string;
  /** kiln's own review state (`needs-review`, `approved`, `changes-requested`, …). */
  review?: string;
  /** How its newest version's checks went (`passed`, `failed`, …). */
  checks?: string;
  /** Failing checks go to the agent to fix. */
  fix_checks: boolean;
  opened_at: number;
  updated_at: number;
}

export interface OpenChange {
  title?: string;
  body?: string;
  draft?: boolean;
  fix_checks?: boolean;
}

const enc = encodeURIComponent;

export const changesApi = {
  list: (signal?: AbortSignal) => request<{ changes: AgentChange[] }>("GET", "/api/harness/changes", undefined, { signal }).then((r) => r.changes),
  /** Open the agent's change request from its work, or push a new version of it. */
  open: (agent: string, body: OpenChange) => request<{ change_request: AgentChange }>("POST", `/api/harness/agents/${enc(agent)}/change`, body).then((r) => r.change_request),
};

/** Every agent's change request; an agent's is found by its id. */
export const changesRes: Res<AgentChange[]> = { key: "agents/changes", fetch: (s) => changesApi.list(s), opts: { on: ["agent_change"] } };

/** Where a change stands, in a few words: `CR12 · approved · checks failed`, `landed`. */
export function changeWords(c: AgentChange): string {
  if (c.state === "landed") return `${c.change ?? c.branch} · landed`;
  if (c.state === "closed") return `${c.change ?? c.branch} · closed`;
  const parts = [c.change ?? `${c.branch} pushed`];
  if (c.review) parts.push(c.review.replace(/-/g, " "));
  if (c.checks) parts.push(`checks ${c.checks}`);
  return parts.join(" · ");
}

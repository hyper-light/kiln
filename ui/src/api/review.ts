/**
 * Review (docs/ci.md W14, W21-R09): diffs and change requests, their versions and files, threads,
 * reviews, viewed files, landing, stacks, the inbox and the land queue.
 */
import type { Res } from "@/lib/data";
import { apiPath } from "@/lib/peer-base";
import { request } from "./client";

export type DiffState = "draft" | "needs-review" | "changes-requested" | "accepted" | "landing" | "landed" | "abandoned";

/** A diff (`D12`) or a change request (`CR4`). */
export interface Diff {
  id: string;
  /** `D12` or `CR4`. */
  name: string;
  number: number;
  kind: "diff" | "change_request";
  repo: string;
  change_id: string | null;
  title: string;
  summary: string;
  /** `user:…`, `agent:…`, `admin`. */
  author: string;
  author_kind: string;
  /** The person an agent's change is for (`user:<id>`), when it's an agent's. */
  author_for?: string | null;
  state: DiffState | string;
  trunk: string;
  stack: string | null;
  /** The diff below it in its stack (`D11`). */
  parent: string | null;
  topic: string | null;
  created_at: number;
  updated_at: number;
  /** A change request's branch. */
  branch: string | null;
}

export interface DiffVersion {
  n: number;
  commit: string;
  parent: string;
  base: string;
  rebase_only: boolean;
  pushed_by: string;
  note: string | null;
  created_at: number;
  ref: string;
}

export interface DiffRun {
  id: string;
  number: number | null;
  workflow: string | null;
  name: string | null;
  event: string;
  status: string;
  conclusion: string | null;
  /** What its jobs' `bench-gate`s said (W16-R09). */
  gates?: BenchGate[];
}

/** A job's `bench-gate` result on a commit (W16-R09): failing (`error`) or a warning, and why. */
export interface BenchGate {
  run: string;
  leg: string;
  job: string;
  level: string;
  message: string;
}

export interface Reviewer {
  reviewer: string;
  state: string;
  [k: string]: unknown;
}

/** A diff whole: it, its versions, its reviewers, the runs on its latest version (and a change request's commits). */
export interface DiffDetail extends Diff {
  versions: DiffVersion[];
  reviewers: Reviewer[];
  runs: DiffRun[];
  commits?: { sha: string; title?: string; subject?: string; author?: string }[];
}

export interface ChangedFile {
  path: string;
  old_path: string | null;
  added: number | null;
  deleted: number | null;
  binary: boolean;
  /** `M`, `A`, `D`, `R100`, `C75`, … as git names it. */
  status: string;
  viewed: boolean;
  /** Marked generated or not to be diffed (`.gitattributes`): folded until opened. */
  generated?: boolean;
}

export interface VersionFiles {
  diff: string;
  version: number;
  commit: string;
  parent: string;
  files: ChangedFile[];
  /** The whole patch (`git diff -M`), cut at 8 MiB (`truncated`). */
  patch: string;
  truncated: boolean;
}

export interface Interdiff {
  diff: string;
  from: number;
  to: number;
  /** The two versions' patches are the same: only the base moved. */
  same_patch: boolean;
  range_diff: string;
}

export interface ThreadComment {
  id: string;
  author: string;
  body: string;
  suggestion: string | null;
  created_at: number;
  /** Yours, not published yet: it goes out with your review. */
  draft: boolean;
}

export interface Thread {
  id: string;
  version: number;
  /** Null: on the diff as a whole. */
  file: string | null;
  side: "old" | "new";
  start_line: number | null;
  end_line: number | null;
  state: "open" | "resolved" | string;
  created_by: string;
  created_at: number;
  /** The code it was on is gone in the latest version. */
  outdated: boolean;
  /** Where it sits in the latest version. */
  now: { version: number; start_line: number; end_line: number } | null;
  comments: ThreadComment[];
}

export interface Readiness {
  /** What keeps it from landing, in words. */
  blockers: string[];
  accepting: string[] | number | unknown;
  requesting: string[] | unknown;
  /** Required checks and how they stand (`null`: not run yet). */
  checks: Record<string, string | null>;
  required: string[];
  strategies: string[];
  default: string | null;
}

export interface InboxThread {
  thread: string;
  repo: string;
  diff: string;
  title: string;
  version: number;
  file: string | null;
  line: number | null;
  last_author: string;
  last: string;
}

/** What a notification rule put in your inbox (W19-R06). */
export interface InboxNote {
  id: string;
  event: string;
  title: string;
  line: string;
  url: string;
  repo: string | null;
  at: number;
  read: boolean;
}

export interface Inbox {
  you: string;
  /** What notification rules put here, newest first. */
  notifications: InboxNote[];
  unread: number;
  /** Threads whose last word isn't yours. */
  waiting_for_you: InboxThread[];
  /** Threads whose last word is yours. */
  waiting_for_others: InboxThread[];
  review_requested: { repo: string; diff: string; title: string; author: string; your_state: string }[];
  your_diffs: { repo: string; diff: string; title: string; state: string }[];
}

export interface Stack {
  id: string | null;
  trunk: string | null;
  repo: string | null;
  /** Bottom to top. */
  diffs: Diff[];
}

export interface QueueEntry {
  id: string;
  trunk: string;
  /** The diffs it lands, bottom first (`D12`). */
  diffs: string[];
  strategy: string;
  /** `queued`, `testing`, `landed`, `ejected`, `cancelled`. */
  state: string;
  batch: string | null;
  candidate: string | null;
  reason: string | null;
  enqueued_by: string;
  created_at: number;
  updated_at: number;
  /** What the candidate's `bench-gate`s said (W16-R09). */
  gates?: BenchGate[];
}

export type ReviewAction = "accept" | "request_changes" | "comment" | "resign";

const enc = encodeURIComponent;

export const reviewApi = {
  inbox: (signal?: AbortSignal) => request<Inbox>("GET", "/api/review/inbox", undefined, { signal }),
  readAll: () => request<{ read: number }>("POST", "/api/review/inbox/read", { all: true }),
  diff: (key: string, signal?: AbortSignal) => request<DiffDetail>("GET", `/api/diffs/${enc(key)}`, undefined, { signal }),
  files: (key: string, version: number, signal?: AbortSignal) => request<VersionFiles>("GET", `/api/diffs/${enc(key)}/versions/${version}/files`, undefined, { signal }),
  interdiff: (key: string, from: number, to: number, signal?: AbortSignal) => request<Interdiff>("GET", `/api/diffs/${enc(key)}/interdiff?from=${from}&to=${to}`, undefined, { signal }),
  dependencies: (key: string, to: number, from: number | null, signal?: AbortSignal) =>
    request<Dependencies>("GET", `/api/diffs/${enc(key)}/dependencies?to=${to}${from != null ? `&from=${from}` : ""}`, undefined, { signal }),
  threads: (key: string, signal?: AbortSignal) => request<{ threads: Thread[] }>("GET", `/api/diffs/${enc(key)}/comments`, undefined, { signal }).then((r) => r.threads),
  comment: (
    key: string,
    body: { body: string; suggestion?: string; publish?: boolean; thread?: string; version?: number; file?: string; side?: "old" | "new"; start_line?: number; end_line?: number },
  ) => request<{ id: string; thread: string; draft: boolean }>("POST", `/api/diffs/${enc(key)}/comments`, body),
  editComment: (id: string, body: { body?: string; suggestion?: string; state?: "resolved" | "open" }) =>
    request<{ id: string; thread: string; state: string }>("PATCH", `/api/comments/${enc(id)}`, body),
  applySuggestion: (id: string) => request<unknown>("POST", `/api/comments/${enc(id)}/apply`, {}),
  review: (key: string, action: ReviewAction, body: string) =>
    request<{ diff: string; action: string; version: number; published: number; state: string }>("POST", `/api/diffs/${enc(key)}/review`, { action, body }),
  viewed: (key: string, file: string, viewed: boolean) => request<{ file: string; viewed: boolean; version: number }>("POST", `/api/diffs/${enc(key)}/viewed`, { file, viewed }),
  readiness: (key: string, signal?: AbortSignal) => request<Readiness>("GET", `/api/diffs/${enc(key)}/land`, undefined, { signal }),
  land: (key: string, strategy?: string) => request<unknown>("POST", `/api/diffs/${enc(key)}/land`, strategy ? { strategy } : {}),
  act: (key: string, action: "ready" | "abandon" | "reclaim" | "commandeer") => request<unknown>("POST", `/api/diffs/${enc(key)}/${action}`, {}),
  stack: (id: string, signal?: AbortSignal) => request<Stack>("GET", `/api/stacks/${enc(id)}`, undefined, { signal }),
  queue: (repo: string, signal?: AbortSignal) => request<{ entries: QueueEntry[] }>("GET", `/api/repos/${enc(repo)}/queue`, undefined, { signal }).then((r) => r.entries),
  dequeue: (entry: string) => request<unknown>("DELETE", `/api/queue/${enc(entry)}`),
  /** A file's bytes at a version, old or new side (images). */
  blobUrl: (key: string, version: number, path: string, side: "old" | "new") => apiPath(`/api/diffs/${enc(key)}/versions/${version}/blob?path=${enc(path)}&side=${side}`),
};

/** Events that change what review pages show. */
const REVIEW_EVENTS = ["diff", "diff_version", "review", "comment", "queue", "landed", "diff_run", "run", "inbox"];

export const inboxRes: Res<Inbox> = { key: "review/inbox", fetch: (s) => reviewApi.inbox(s), opts: { on: REVIEW_EVENTS } };
export const diffRes = (key: string): Res<DiffDetail> => ({ key: `review/diff/${key}`, fetch: (s) => reviewApi.diff(key, s), opts: { on: REVIEW_EVENTS } });
export const threadsRes = (key: string): Res<Thread[]> => ({ key: `review/threads/${key}`, fetch: (s) => reviewApi.threads(key, s), opts: { on: ["comment", "review", "diff_version"] } });
export const filesRes = (key: string, version: number): Res<VersionFiles> => ({ key: `review/files/${key}/${version}`, fetch: (s) => reviewApi.files(key, version, s) });

/** What a version changes in the dependencies its lockfiles pin (docs/ci.md W18-R03). */
export interface Dependencies {
  diff: string;
  from: string;
  to: string;
  lockfiles: { path: string; added: number; removed: number; changed: number; changes: { ecosystem: string; name: string; from: string[]; to: string[] }[] }[];
}

export const dependenciesRes = (key: string, to: number, from: number | null): Res<Dependencies> => ({
  key: `review/dependencies/${key}/${from ?? "base"}/${to}`,
  fetch: (s) => reviewApi.dependencies(key, to, from, s),
});
export const readinessRes = (key: string): Res<Readiness> => ({ key: `review/land/${key}`, fetch: (s) => reviewApi.readiness(key, s), opts: { on: REVIEW_EVENTS } });

/** Who someone is, in words: `agent:a1b2` → "agent a1b2", `user:ada` → "ada". */
export function whoSaid(principal: string): string {
  if (principal.startsWith("user:")) return principal.slice(5);
  if (principal.startsWith("agent:")) return `agent ${principal.slice(6).split(/\s/)[0] ?? ""}`;
  if (principal === "admin") return "the owner";
  return principal;
}

/** A state in words. */
export const STATE_WORD: Record<string, string> = {
  draft: "Draft",
  "needs-review": "Needs review",
  "changes-requested": "Changes requested",
  accepted: "Accepted",
  landing: "Landing",
  landed: "Landed",
  abandoned: "Abandoned",
};

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
  /** Why it exists (W26-R04) when it isn't a push: `suggestion applied`, `conflict resolved`. */
  why?: string | null;
  /** The thread it answers (a suggestion's). */
  thread?: string | null;
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
  /** Its author deleted it: a thread's first comment stays, saying `deleted`, while replies remain (W26-R08). */
  deleted?: boolean;
  /** Its suggestions (W26-R10): its body's ```suggestion blocks, then its old `suggestion` field as one more. */
  suggestions?: Suggestion[];
}

/** A comment's suggestion (W26-R10): the lines it puts where its thread's lines are (none: they go), and which revision applied it. */
export interface Suggestion {
  /** Which of the comment's suggestions it is, from 0: what Apply's `suggestion` names. */
  index: number;
  lines: string[];
  /** The comment's old `suggestion` field, not a block of its body. */
  field: boolean;
  applied: { version: number; by: string; at: number } | null;
  /** Its lines changed since it was written: it doesn't apply. */
  outdated: boolean;
}

/** A thread's kind (W26-R11); threads from before kinds are questions. */
export type ThreadKind = "question" | "nit" | "issue" | "blocking";

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
  kind?: ThreadKind;
  /** Who resolved it, when, and why (an owner resolving someone else's blocking thread says why). */
  resolved_by?: string | null;
  resolved_at?: number | null;
  resolution?: string | null;
}

/** The gate (W26-R11) as it stands for you approving: a blocking thread or a conflict holds it, open issues warn, open nits inform. */
export interface ReviewGate {
  verdict: "hold" | "warn" | "inform" | "clear";
  /** What holds it, each naming where it is, whose it is to resolve (a conflict: no one's), and why. */
  holds: { at: string; by: string | null; reason: string }[];
  open: { question: number; nit: number; issue: number; blocking: number };
  conflicts: number;
  /** `2 issues open`: approving asks to go ahead anyway. */
  warning: string | null;
  /** `1 nit open`: approving says so. */
  note: string | null;
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
  /** Pull requests on the hosts kiln's repositories are on that ask your review there (W23-R06). */
  elsewhere?: { repo: string; host: HostName; number: number; title: string; author: string; url: string; updated_at: number | null }[];
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
  threads: (key: string, signal?: AbortSignal) => request<{ threads: Thread[]; gate?: ReviewGate }>("GET", `/api/diffs/${enc(key)}/comments`, undefined, { signal }).then((r) => r.threads),
  comment: (
    key: string,
    body: { body: string; suggestion?: string; publish?: boolean; thread?: string; version?: number; file?: string; side?: "old" | "new"; start_line?: number; end_line?: number; kind?: ThreadKind },
  ) => request<{ id: string; thread: string; draft: boolean }>("POST", `/api/diffs/${enc(key)}/comments`, body),
  editComment: (id: string, body: { body?: string; suggestion?: string; state?: "resolved" | "open"; kind?: ThreadKind; reason?: string }) =>
    request<{ id: string; thread: string; state: string; kind?: ThreadKind }>("PATCH", `/api/comments/${enc(id)}`, body),
  deleteComment: (id: string) => request<{ id: string; thread: string; deleted: boolean; thread_deleted: boolean }>("DELETE", `/api/comments/${enc(id)}`),
  applySuggestion: (id: string, suggestion?: number) =>
    request<{ diff: string; version: number; thread: string; state: string; commit?: string; suggestion?: number; signer?: string | null }>(
      "POST",
      `/api/comments/${enc(id)}/apply`,
      suggestion == null ? {} : { suggestion },
    ),
  review: (key: string, action: ReviewAction, body: string, force?: boolean) =>
    request<{ diff: string; action: string; version: number; published: number; state: string; note?: string | null }>(
      "POST",
      `/api/diffs/${enc(key)}/review`,
      force ? { action, body, force } : { action, body },
    ),
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

export type HostName = "github" | "gitlab" | "bitbucket";

/** A pull request (GitLab's merge request) on a repository's host, in one shape (W23-R06). */
export interface HostPull {
  host: HostName;
  number: number;
  title: string;
  body: string;
  author: string;
  state: "open" | "merged" | "closed" | string;
  draft: boolean;
  head: string;
  sha: string;
  base: string;
  base_sha: string;
  url: string;
  updated_at: number | null;
  mergeable: boolean | null;
  reviewers: string[];
}

export interface HostCheck {
  run: string;
  provider: string;
  name: string | null;
  status: string;
  conclusion: string | null;
  url: string | null;
  number: number;
}

export interface HostComment {
  id: string;
  author: string;
  body: string;
  at: number | null;
}

export interface HostThread {
  id: string;
  path: string | null;
  line: number | null;
  side: "left" | "right" | null;
  resolved: boolean | null;
  comments: HostComment[];
}

export interface HostPullDetail extends HostPull {
  files: { path: string; old_path: string | null; status: string; additions: number | null; deletions: number | null; patch: string | null }[];
  conversation: HostComment[];
  threads: HostThread[];
  reviews: { author: string; state: string; body: string; at: number | null }[];
  merge_methods: string[];
  /** Every provider's runs of its head commit, from kiln's own tables. */
  checks: { state?: "success" | "failure" | "pending" | "none"; checks?: HostCheck[]; error?: string };
}

/** Who acted on the host: you, with the account you signed in with, or kiln's credentials there. */
type Acted = { as: "person" | "kiln" };

export const hostPullsApi = {
  list: (repo: string, signal?: AbortSignal) => request<{ repo: string; pulls: HostPull[] }>("GET", `/api/ci/repos/${enc(repo)}/pulls`, undefined, { signal }).then((r) => r.pulls),
  get: (repo: string, n: number, signal?: AbortSignal) => request<HostPullDetail>("GET", `/api/ci/repos/${enc(repo)}/pulls/${n}`, undefined, { signal }),
  comment: (repo: string, n: number, body: { body: string; path?: string; line?: number; side?: "left" | "right"; reply_to?: string }) =>
    request<Acted>("POST", `/api/ci/repos/${enc(repo)}/pulls/${n}/comments`, body),
  review: (repo: string, n: number, verdict: "approve" | "request_changes" | "comment", body: string) => request<Acted>("POST", `/api/ci/repos/${enc(repo)}/pulls/${n}/reviews`, { verdict, body }),
  merge: (repo: string, n: number, method: string) => request<Acted & { merged: boolean; commit: string | null }>("POST", `/api/ci/repos/${enc(repo)}/pulls/${n}/merge`, { method }),
};

export const hostPullsRes = (repo: string): Res<HostPull[]> => ({ key: `review/host/${repo}`, fetch: (s) => hostPullsApi.list(repo, s) });
export const hostPullRes = (repo: string, n: number): Res<HostPullDetail> => ({ key: `review/host/${repo}/${n}`, fetch: (s) => hostPullsApi.get(repo, n, s) });

export const HOST_WORD: Record<HostName, string> = { github: "GitHub", gitlab: "GitLab", bitbucket: "Bitbucket" };
/** How each host writes a change's number. */
export const hostNumber = (host: HostName, n: number) => (host === "gitlab" ? `!${n}` : `#${n}`);

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

/** A conflict's kind (W26-R12): a text file's conflicts sit where they fall and resolve line by line; the others take a side. */
export type ConflictKind = "text" | "binary" | "modify_delete" | "delete_modify" | "other";

/** How a conflict is resolved (W26-R12): main's side, this change's, both (main's then this change's), an agent's resolution, or an edit. */
export type ConflictTake = "main" | "this" | "both" | "agent" | "edit";

/** A text conflict's side (its lines, and where they start in that side's own file); a path taken whole: what that side has there, or null where it deleted it. */
export type ConflictSide = { start?: number | null; lines: string[] } | { mode: string; blob: string } | null;

/** A conflict of a change's replay onto where it lands, where it falls (W26-R12). */
export interface Conflict {
  id: string;
  file: string;
  kind: ConflictKind;
  /** A text conflict's place among its file's, from 0. */
  hunk: number | null;
  /** How many conflicts its file has. */
  of: number;
  /** Where it falls in this change's own file (a text conflict). */
  line: number | null;
  end_line: number | null;
  main: ConflictSide;
  this: ConflictSide;
  base: ConflictSide;
  /** What it is, in words, for a conflict resolved by taking a side. */
  says: string | null;
  /** git's own words on it. */
  git: string | null;
  /** What resolves it. */
  takes: ConflictTake[];
  /** An agent's resolution, for a person to take. */
  agent: { by: string; take: ConflictTake | null; lines: string[] | null; at: number | null } | null;
  /** The resolution taken: what it took, the lines a text conflict resolves to, by whom and when. */
  resolution: { take: ConflictTake; lines: string[] | null; by: string | null; at: number | null } | null;
}

/** A change's latest revision replayed onto where it lands, and its conflicts (W26-R12). */
export interface Conflicts {
  diff: string;
  version: number | null;
  /** None when there's nothing to replay; once Commit resolution made the latest revision, the replay it resolved. */
  replay: {
    id: string;
    version: number;
    commit: string;
    onto: { name: string; commit: string };
    state: "clean" | "conflicted" | "resolved";
    by: string;
    at: number;
    resolved_in: number | null;
    resolved_by: string | null;
    resolved_at: number | null;
  } | null;
  conflicts: Conflict[];
  unresolved: number;
  /** Each conflict resolved: Commit resolution may go. */
  ready: boolean;
}

/** What Commit resolution made: one revision of whoever committed it, its why `conflict resolved` (`signer` null: unsigned until W27-R03). */
export interface ConflictsCommitted {
  diff: string;
  version: number;
  why: string;
  commit: string;
  by: string;
  signer: string | null;
  replay: string;
  conflicts: number;
}

export const conflictsApi = {
  list: (key: string, signal?: AbortSignal) => request<Conflicts>("GET", `/api/diffs/${enc(key)}/conflicts`, undefined, { signal }),
  resolve: (key: string, id: string, body: { take: ConflictTake; lines?: string[]; text?: string }) => request<Conflicts>("POST", `/api/diffs/${enc(key)}/conflicts/${enc(id)}/resolution`, body),
  undo: (key: string, id: string) => request<Conflicts>("DELETE", `/api/diffs/${enc(key)}/conflicts/${enc(id)}/resolution`),
  commit: (key: string) => request<ConflictsCommitted>("POST", `/api/diffs/${enc(key)}/conflicts/commit`, {}),
};

export const conflictsRes = (key: string): Res<Conflicts> => ({ key: `review/conflicts/${key}`, fetch: (s) => conflictsApi.list(key, s), opts: { on: ["conflict", "diff_version", "diff"] } });

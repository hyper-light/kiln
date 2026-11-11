import type { Approval, QueueItem, StateDescriptor, StateMetadata, StateRead, StateStep, Task } from "./wire";
export type { StateDescriptor, StateMetadata, StateRead, StateStep } from "./wire";
export interface StateEntry {
  key: StateStep;
  value?: unknown;
  node?: StateDescriptor | null;
}
export type StateReply =
  | { type: "page"; revision: string; entries: StateEntry[]; next?: number | null }
  | { type: "text"; revision: string; text: string; next?: number | null }
  | { type: "value"; revision: string; value: unknown }
  | { type: "node"; revision: string; node: StateDescriptor };
export interface StatePage {
  phase: "loading" | "ready" | "error";
  read: StateRead;
  reply?: StateReply;
  error?: string;
}

const object = (v: unknown): v is Record<string, unknown> => typeof v === "object" && v !== null && !Array.isArray(v);
const count = (v: unknown): v is number => typeof v === "number" && Number.isSafeInteger(v) && v >= 0;
export const descriptor = (v: unknown): v is StateDescriptor => object(v) && ["array", "object", "text", "value"].includes(String(v.kind)) && count(v.len);
const step = (v: unknown): v is StateStep => object(v) && ((v.type === "index" && count(v.index)) || (v.type === "key" && typeof v.key === "string") || (v.type === "id" && typeof v.id === "string"));

export function stateMetadata(state: unknown): StateMetadata | undefined {
  if (!object(state) || !object(state.view)) return undefined;
  const v = state.view;
  if (
    typeof v.revision !== "string" ||
    !object(v.revisions) ||
    !Object.values(v.revisions).every((r) => typeof r === "string") ||
    !object(v.deferred) ||
    !Object.values(v.deferred).every(descriptor) ||
    !object(v.counts)
  )
    return undefined;
  const counts = v.counts;
  if (!["queue", "approvals", "tasks", "tasks_running", "todos", "todos_done"].every((key) => count(counts[key]))) return undefined;
  if (v.queue_index !== undefined) {
    const index = v.queue_index;
    if (!object(index) || !Array.isArray(index.entries) || typeof index.complete !== "boolean" || (index.next != null && !count(index.next))) return undefined;
    if (!index.entries.every((e) => object(e) && typeof e.id === "string" && typeof e.digest === "string" && count(e.len))) return undefined;
  }
  return v as unknown as StateMetadata;
}

export const deferredState = (state: unknown, field: string): StateDescriptor | undefined => stateMetadata(state)?.deferred[field];
export const stateCount = (state: unknown, field: keyof StateMetadata["counts"], fallback: number): number => stateMetadata(state)?.counts[field] ?? fallback;
export const stateFieldRevision = (state: unknown, field: string): string | undefined => stateMetadata(state)?.revisions[field === "$queue_index" ? "queue" : field];

/** Only positive owner evidence counts; absence from a partial index says nothing. */
export function stateMessageIds(state: unknown): Set<string> {
  const ids = new Set<string>();
  const view = stateMetadata(state);
  for (const entry of view?.queue_index?.entries ?? []) ids.add(entry.id);
  if (!object(state)) return ids;
  const add = (list: unknown) => {
    if (Array.isArray(list)) for (const entry of list) if (object(entry) && typeof entry.id === "string") ids.add(entry.id);
  };
  if (!view?.deferred.queue) add(state.queue);
  if (!view?.deferred.given) add(state.given);
  if (!view?.deferred.tasks && Array.isArray(state.tasks)) for (const task of state.tasks) if (object(task)) add(task.queue);
  if (!view?.deferred.delivery && object(state.delivery))
    for (const [id, status] of Object.entries(state.delivery)) if (["sent", "queued", "started", "completed", "cancelled"].includes(String(status))) ids.add(id);
  return ids;
}

export function statePageMessageIds(page: StatePage): string[] {
  if (page.phase !== "ready" || !["queue", "$queue_index"].includes(page.read.field)) return [];
  if (page.read.field === "queue") return queuePageRows(page).flatMap((row) => (row.item ? [row.item.id] : []));
  const reply = page.reply;
  const values = reply?.type === "value" ? [reply.value] : reply?.type === "page" ? reply.entries.map((entry) => entry.value) : [];
  return values.flatMap((value) => (object(value) && typeof value.id === "string" && typeof value.digest === "string" && count(value.len) ? [value.id] : []));
}

/** Replies are bounded by the state wire contract; they replace one field's previous page. */
export function stateReply(value: unknown, revision: string): { reply?: StateReply; error?: string } {
  if (!object(value)) return { error: "The agent did not answer this read. Try again." };
  if (object(value.error)) {
    const code = value.error.code;
    return {
      error:
        code === "stale"
          ? "The agent changed while this page was loading. Read its current data again."
          : code === "budget"
            ? "This value needs to be read in smaller parts."
            : "The agent could not read this part of its data. Try again.",
    };
  }
  const r = value.result;
  if (!object(r) || r.revision !== revision) return { error: "This page belongs to an older state. Read its current data again." };
  if ((r.type === "page" || r.type === "text") && r.next != null && !count(r.next)) return { error: "The agent returned an invalid page cursor." };
  const valid =
    (r.type === "page" && Array.isArray(r.entries) && r.entries.every((e) => object(e) && step(e.key) && (Object.hasOwn(e, "value") || descriptor(e.node)))) ||
    (r.type === "text" && typeof r.text === "string") ||
    (r.type === "value" && Object.hasOwn(r, "value")) ||
    (r.type === "node" && descriptor(r.node));
  return valid ? { reply: r as unknown as StateReply } : { error: "The agent returned an unreadable page." };
}

export function stateMutationError(value: unknown): string | undefined {
  if (object(value) && object(value.result) && value.result.type === "applied") return undefined;
  if (object(value) && object(value.error) && value.error.code === "stale") return "This message changed before the edit reached the agent. Read its current queue again.";
  return "The agent did not apply this change. Read its current queue before trying again.";
}

export interface QueuePageRow {
  index: number;
  item?: QueueItem;
  node?: StateDescriptor;
}

/** A reference or partial row never becomes editable queue text. */
export function queuePageRows(page: StatePage | undefined): QueuePageRow[] {
  if (page?.phase !== "ready" || page.read.path.length || page.reply?.type !== "page") return [];
  return page.reply.entries.flatMap<QueuePageRow>((entry) => {
    if (entry.key.type !== "index") return [];
    if (entry.node) return [{ index: entry.key.index, node: entry.node }];
    const q = entry.value;
    if (!object(q) || typeof q.id !== "string" || typeof q.text !== "string" || typeof q.at !== "number") return [{ index: entry.key.index, node: { kind: "value" as const, len: 0 } }];
    if (q.attachments != null && (!Array.isArray(q.attachments) || !q.attachments.every((a) => object(a) && typeof a.name === "string" && typeof a.mime === "string")))
      return [{ index: entry.key.index, node: { kind: "value" as const, len: 0 } }];
    return [{ index: entry.key.index, item: q as unknown as QueueItem }];
  });
}

const optionalStrings = (value: Record<string, unknown>, keys: string[]) => keys.every((key) => value[key] === undefined || typeof value[key] === "string");
const optionalList = (value: unknown, valid: (entry: unknown) => boolean) => value === undefined || (Array.isArray(value) && value.every(valid));
const itemPath = (path: StateStep[]) => path.length === 1 && (path[0]?.type === "index" || path[0]?.type === "id");
const matchesItemPath = (value: Record<string, unknown>, path: StateStep[]) => itemPath(path) && (path[0]?.type !== "id" || path[0].id === value.id);
const hunk = (value: unknown) =>
  object(value) && ["old_start", "old_lines", "new_start", "new_lines"].every((key) => count(value[key])) && Array.isArray(value.lines) && value.lines.every((line) => typeof line === "string");
const diff = (value: unknown) =>
  object(value) && typeof value.path === "string" && (value.created === undefined || typeof value.created === "boolean") && Array.isArray(value.hunks) && value.hunks.every(hunk);
const question = (value: unknown) =>
  object(value) &&
  typeof value.question === "string" &&
  typeof value.header === "string" &&
  ["multi", "other", "secret"].every((key) => typeof value[key] === "boolean") &&
  optionalStrings(value, ["id"]) &&
  Array.isArray(value.options) &&
  value.options.every((option) => object(option) && typeof option.label === "string" && typeof option.description === "string" && optionalStrings(option, ["preview"]));

/** Only whole, schema-shaped source items may acquire their existing typed actions. */
export function approvalItem(value: unknown, path: StateStep[]): Approval | undefined {
  if (!object(value) || !matchesItemPath(value, path) || typeof value.id !== "string" || !value.id || !Object.hasOwn(value, "tool_id") || (value.tool_id !== null && typeof value.tool_id !== "string"))
    return undefined;
  if (
    !["tool_name", "title", "cwd"].every((key) => typeof value[key] === "string") ||
    !["shell", "edit", "read", "search", "web", "agent", "skill", "todo", "question", "plan", "mcp", "other"].includes(String(value.tool))
  )
    return undefined;
  if (
    !Object.hasOwn(value, "input") ||
    !count(value.at) ||
    !optionalStrings(value, ["detail", "command", "reason", "plan", "agent"]) ||
    (value.default_to_no !== undefined && typeof value.default_to_no !== "boolean")
  )
    return undefined;
  if (
    !optionalList(value.diffs, diff) ||
    !optionalList(value.questions, question) ||
    !optionalList(value.always, (entry) => object(entry) && typeof entry.label === "string" && typeof entry.scope === "string" && Object.hasOwn(entry, "value"))
  )
    return undefined;
  return value as unknown as Approval;
}

export function taskItem(value: unknown, path: StateStep[]): Task | undefined {
  if (!object(value) || !matchesItemPath(value, path) || typeof value.id !== "string" || !value.id || !["description", "kind", "status"].every((key) => typeof value[key] === "string"))
    return undefined;
  if (!["background", "stoppable"].every((key) => typeof value[key] === "boolean") || !["depth", "tokens", "tool_uses", "duration_ms", "started"].every((key) => count(value[key]))) return undefined;
  if (!optionalStrings(value, ["tool_id", "parent_task", "prompt", "summary", "last_tool", "class", "command", "cwd", "output", "process", "last_event", "wakes", "via"])) return undefined;
  if (value.ended !== undefined && !count(value.ended)) return undefined;
  return value as unknown as Task;
}

/** A stale render cannot act after its one retained page or source field has changed. */
export function pageHasItem(state: unknown, page: StatePage | undefined, field: "approvals" | "tasks", id: string): boolean {
  if (page?.phase !== "ready" || page.read.field !== field || page.reply?.revision !== page.read.revision || page.read.revision !== stateFieldRevision(state, field)) return false;
  const decode = field === "approvals" ? approvalItem : taskItem;
  if (page.reply?.type === "value") return decode(page.reply.value, page.read.path)?.id === id;
  return page.reply?.type === "page" && page.reply.entries.some((entry) => !entry.node && decode(entry.value, [...page.read.path, entry.key])?.id === id);
}

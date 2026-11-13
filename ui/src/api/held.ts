/**
 * This browser's backup of what it sent that the server hasn't taken yet (2026-11-13: it was the
 * gate every send went through, and any failure there sent nothing). A message goes to the server
 * first; its copy here is kept in memory at once and written to IndexedDB after, one record per
 * message (keyed by sign-in, agent and id), never a combined set read and rewritten, and without a
 * strict commit. The server's receipt (W20 custody, docs/wire.md "mail-flow") lets the copy go;
 * what's still here when a page opens is offered again. Zustand is its in-memory projection.
 */
import { create } from "zustand";
import type { Attachment, Command, SendMode, SessionDown } from "./wire";
import { mailScope } from "./client";

export type PublicationReceipt = NonNullable<Extract<SessionDown, { t: "ack" }>["publication"]>;
export type SendCommand = Extract<Command, { type: "send" }>;
export type OriginalFile = { name: string; mime: string; blob: Blob; bytes?: ArrayBuffer };
export interface Held {
  id: string;
  text: string;
  at: number;
  attachments?: Attachment[];
  originals?: OriginalFile[];
  overrides?: { model?: string; effort?: string };
  mode?: SendMode;
  reply_to?: string;
  task?: string;
  uploading?: string[];
  owner?: string;
  custody?: "offered" | "waiting" | "retained" | "failed";
  publication?: PublicationReceipt;
  wait?: string;
  why?: string;
}
const DB = "kiln.agents.backup";
/** The store W20 kept every original in, as one set: read once, into per-message records. */
const OLD_DB = "kiln.agents.held";
const LEGACY = "kiln.agents.held";
const NOT_KEPT = "This browser couldn't keep a copy; keep this page open until it's sent.";
type RecordSet = Record<string, Held[]>;
type Key = [string, string, string];
interface HeldState {
  held: RecordSet;
  error: string | null;
  ready(): Promise<boolean>;
  hold(agent: string, message: Held): Promise<boolean>;
  holdMany(messages: { agent: string; message: Held }[]): Promise<boolean>;
  attempt(agent: string, id: string, stamp: string): Promise<boolean>;
  edit(agent: string, id: string, text: string): Promise<boolean>;
  uploaded(agent: string, id: string, files: Attachment[], owner?: string, originals?: OriginalFile[]): Promise<boolean>;
  without(agent: string, id: string, index: number): Promise<boolean>;
  drop(agent: string, ...ids: string[]): Promise<boolean>;
  settled(agent: string, ...ids: string[]): Promise<boolean>;
}
const object = (value: unknown): value is Record<string, unknown> => typeof value === "object" && value !== null && !Array.isArray(value);
const optionalString = (value: unknown) => value === undefined || typeof value === "string";
export function publicationReceipt(value: unknown): value is PublicationReceipt {
  if (!object(value) || typeof value.id !== "string" || !Number.isSafeInteger(value.revision) || !Number.isSafeInteger(value.generation) || typeof value.repeat !== "boolean") return false;
  if (typeof value.revision !== "number" || value.revision <= 0 || typeof value.generation !== "number" || value.generation < 0) return false;
  return ["waiting", "dispatched", "admitted", "consumed", "superseded", "cancelled", "transferred"].includes(String(value.disposition));
}
function validMessage(value: unknown): value is Held {
  if (!object(value) || typeof value.id !== "string" || typeof value.text !== "string" || typeof value.at !== "number") return false;
  if (![value.owner, value.reply_to, value.task, value.wait, value.why].every(optionalString)) return false;
  if (value.mode !== undefined && !["queue", "steer", "now"].includes(String(value.mode))) return false;
  if (value.custody !== undefined && !["offered", "waiting", "retained", "failed"].includes(String(value.custody))) return false;
  if (value.uploading !== undefined && (!Array.isArray(value.uploading) || !value.uploading.every((name) => typeof name === "string"))) return false;
  if (value.overrides !== undefined && (!object(value.overrides) || !optionalString(value.overrides.model) || !optionalString(value.overrides.effort))) return false;
  if (
    value.attachments !== undefined &&
    (!Array.isArray(value.attachments) ||
      !value.attachments.every((f) => object(f) && typeof f.name === "string" && typeof f.mime === "string" && [f.file_id, f.data_base64, f.path].every(optionalString)))
  )
    return false;
  return value.publication === undefined || publicationReceipt(value.publication);
}
function legacyHeld(raw: string | null): RecordSet {
  if (!raw) return {};
  try {
    const value: unknown = JSON.parse(raw);
    if (!object(value) || !object(value.state) || !object(value.state.held)) return {};
    return Object.fromEntries(Object.entries(value.state.held).flatMap(([agent, rows]) => (Array.isArray(rows) ? [[agent, rows.filter(validMessage)]] : [])));
  } catch {
    return {};
  }
}
// WebKit can reject IndexedDB Blob/File serialization: a file is written as its bytes.
async function preparedFiles(files?: OriginalFile[]): Promise<OriginalFile[] | undefined> {
  return files ? Promise.all(files.map(async (file) => ({ ...file, bytes: file.bytes ?? (await file.blob.arrayBuffer()) }))) : undefined;
}
/** How many rows (messages and their files) `value` holds, as the store or `restored` has them. */
function rowsIn(value: unknown): number {
  if (!object(value)) return 0;
  return Object.values(value).reduce<number>(
    (n, rows) => n + (Array.isArray(rows) ? rows.reduce<number>((m, row) => m + 1 + (object(row) && Array.isArray(row.originals) ? row.originals.length : 0), 0) : 1),
    0,
  );
}
/** A file kept with its bytes, as `stored` writes one. */
const keptFile = (file: unknown): file is OriginalFile =>
  object(file) && typeof file.name === "string" && typeof file.mime === "string" && (file.bytes instanceof ArrayBuffer || file.blob instanceof Blob);
/**
 * What this browser kept, as the store holds it, whatever wrote it (2026-11-13: one row an older
 * build kept — a file without its bytes, an agent's rows not a list — failed every send after it,
 * for every agent, as every save reads and writes the whole set). A row that isn't a message is
 * left out, and a file kept without bytes (none to send again) is left out of its message.
 */
export function restored(held: unknown): RecordSet {
  if (!object(held)) return {};
  return Object.fromEntries(
    Object.entries(held).flatMap(([agent, rows]) =>
      Array.isArray(rows)
        ? [
            [
              agent,
              rows.filter(validMessage).map((message) => {
                const files = Array.isArray(message.originals) ? message.originals.filter(keptFile) : undefined;
                return { ...message, originals: files?.map((file) => (file.bytes ? { ...file, blob: new Blob([file.bytes], { type: file.mime }) } : file)) };
              }),
            ],
          ]
        : [],
    ),
  );
}
function sameFiles(previous?: OriginalFile[], next?: OriginalFile[]): boolean {
  if (!next || !previous) return true;
  return (
    previous.length === next.length &&
    previous.every((file, index) => {
      const other = next[index];
      if (!other || file.name !== other.name || file.mime !== other.mime || !file.bytes || !other.bytes || file.bytes.byteLength !== other.bytes.byteLength) return false;
      const a = new Uint8Array(file.bytes),
        b = new Uint8Array(other.bytes);
      return a.every((byte, at) => byte === b[at]);
    })
  );
}

const keyOf = (agent: string, message: Held): Key => [message.owner ?? "", agent, message.id];

function request<T>(r: IDBRequest<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => reject(r.error ?? new Error("storage request failed"));
  });
}
function done(tx: IDBTransaction): Promise<void> {
  return new Promise((resolve, reject) => {
    tx.oncomplete = () => resolve();
    tx.onabort = () => reject(tx.error ?? new Error("storage transaction aborted"));
    tx.onerror = () => {};
  });
}
function open(name: string, version: number, upgrade: (db: IDBDatabase) => void): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    if (typeof indexedDB === "undefined") return reject(new Error("storage unavailable"));
    let r: IDBOpenDBRequest;
    try {
      r = indexedDB.open(name, version);
    } catch (error) {
      return reject(error instanceof Error ? error : new Error("storage unavailable"));
    }
    r.onupgradeneeded = () => upgrade(r.result);
    r.onerror = () => reject(new Error("storage unavailable"));
    r.onblocked = () => reject(new Error("storage upgrade blocked"));
    r.onsuccess = () => resolve(r.result);
  });
}

let opening: Promise<IDBDatabase> | undefined;
function database(): Promise<IDBDatabase> {
  opening ??= open(DB, 1, (db) => {
    db.createObjectStore("messages");
    db.createObjectStore("legacy", { autoIncrement: true });
    db.createObjectStore("meta");
  })
    .then((db) => {
      db.onversionchange = () => {
        db.close();
        opening = undefined;
      };
      return db;
    })
    .catch((error: unknown) => {
      opening = undefined;
      throw error;
    });
  return opening;
}

/** A message as written: its files as bytes alone (a file whose bytes weren't read isn't written). */
const written = (agent: string, message: Held) => ({
  agent,
  message: { ...message, originals: message.originals?.flatMap((file) => (file.bytes ? [{ name: file.name, mime: file.mime, bytes: file.bytes }] : [])) },
});

/** The records kept, read whole (on opening, and when another tab or a reload changed them). */
async function readAll(db: IDBDatabase): Promise<RecordSet> {
  const tx = db.transaction("messages", "readonly");
  const rows = request(tx.objectStore("messages").getAll());
  await done(tx);
  const set: Record<string, unknown[]> = {};
  for (const row of await rows) {
    if (typeof row !== "object" || row === null || typeof (row as { agent?: unknown }).agent !== "string") continue;
    const { agent, message } = row as { agent: string; message: unknown };
    (set[agent] ??= []).push(message);
  }
  return restored(set);
}

/** The old database's set, if it has one (opening it at its version never creates a newer one). */
async function oldSet(): Promise<unknown> {
  const exists = typeof indexedDB.databases === "function" ? (await indexedDB.databases()).some((d) => d.name === OLD_DB) : true;
  if (!exists) return undefined;
  const db = await open(OLD_DB, 1, (d) => {
    d.createObjectStore("held");
    d.createObjectStore("legacy", { autoIncrement: true });
  });
  try {
    if (!db.objectStoreNames.contains("held")) return undefined;
    const tx = db.transaction("held", "readonly");
    const value = request(tx.objectStore("held").get("originals"));
    await done(tx);
    return await value;
  } finally {
    db.close();
  }
}

/**
 * Once: what W20 kept as one set, and the box's localStorage before it, become one record per
 * message. Rows that aren't messages are kept as they were in `legacy`, never dropped unseen, and
 * never stop the rest; the old database and localStorage are left as they are.
 */
async function migrate(db: IDBDatabase) {
  const tx0 = db.transaction("meta", "readonly");
  const marked = request(tx0.objectStore("meta").get("migrated"));
  await done(tx0);
  if (await marked) return;
  let raw: string | null = null;
  try {
    raw = typeof localStorage === "undefined" ? null : localStorage.getItem(LEGACY);
  } catch {
    /* no localStorage to read */
  }
  let old: unknown;
  try {
    old = await oldSet();
  } catch {
    old = undefined;
  }
  const set = restored(old);
  for (const [agent, rows] of Object.entries(legacyHeld(raw))) {
    const existing = set[agent] ?? [];
    set[agent] = [...existing, ...rows.filter((row) => !existing.some((item) => item.id === row.id && item.owner === row.owner))];
  }
  const tx = db.transaction(["messages", "legacy", "meta"], "readwrite");
  const messages = tx.objectStore("messages");
  for (const [agent, rows] of Object.entries(set)) for (const message of rows) messages.put(written(agent, message), keyOf(agent, message));
  if (old !== undefined && rowsIn(old) !== rowsIn(restored(old))) tx.objectStore("legacy").add(old);
  if (raw) tx.objectStore("legacy").add(raw);
  tx.objectStore("meta").put(Date.now(), "migrated");
  await done(tx);
}

let channel: BroadcastChannel | undefined;
/** The writes under way (`flushHeld` waits for them). */
const writing = new Set<Promise<boolean>>();
/** Waits until every record written so far is in IndexedDB (tests; nothing on a send waits for it). */
export async function flushHeld(): Promise<void> {
  while (writing.size) await Promise.all([...writing]);
}
/** Messages whose record is being written or deleted, by key: a reread keeps memory's word on them. */
const pending = new Map<string, { agent: string; message: Held; gone: boolean }>();
const keyText = (agent: string, message: Held) => keyOf(agent, message).join("\u0000");
/** What IndexedDB has, with what's still being written taken from memory (never lost to a reread). */
function merged(stored: RecordSet): RecordSet {
  let held = stored;
  for (const { agent, message, gone } of pending.values()) {
    const rows = (held[agent] ?? []).filter((row) => !(row.id === message.id && row.owner === message.owner));
    held = keptAs(held, agent, gone ? rows : [...rows, message]);
  }
  return held;
}
const keptAs = (held: RecordSet, agent: string, messages: Held[]): RecordSet => {
  const next = { ...held };
  if (messages.length) next[agent] = messages;
  else delete next[agent];
  return next;
};
const empty = (message: Held) => !message.text.trim() && !message.attachments?.length && !message.uploading?.length;

export const useHeld = create<HeldState>()((set, get) => {
  let loaded: Promise<boolean> | undefined;
  /** The records as IndexedDB has them, into memory: once, then on another tab's word. */
  const load = async (): Promise<boolean> => {
    try {
      const db = await database();
      await migrate(db);
      set({ held: merged(await readAll(db)) });
      return true;
    } catch {
      return false;
    }
  };
  /** Writes one message's record, or deletes it, after memory has it: a backup, never a gate. */
  const persist = async (agent: string, message: Held, gone = false): Promise<boolean> => {
    const key = keyText(agent, message);
    const mine = { agent, message, gone };
    pending.set(key, mine);
    try {
      const db = await database();
      const tx = db.transaction("messages", "readwrite");
      const store = tx.objectStore("messages");
      if (gone) store.delete(keyOf(agent, message));
      else store.put(written(agent, { ...message, originals: await preparedFiles(message.originals) }), keyOf(agent, message));
      await done(tx);
      channel?.postMessage("changed");
      return true;
    } catch {
      set({ error: NOT_KEPT });
      return false;
    } finally {
      if (pending.get(key) === mine) pending.delete(key);
    }
  };
  /** Writes in the background: what calls it never waits on IndexedDB. */
  const background = (agent: string, message: Held, gone = false) => {
    const write = persist(agent, message, gone);
    writing.add(write);
    void write.finally(() => writing.delete(write));
  };
  /** `agent`'s messages now `next`, in memory at once, and each changed one written after. */
  const change = (agent: string, next: Held[], changed: Held[], gone: Held[]) => {
    set({ held: keptAs(get().held, agent, next), error: null });
    for (const m of changed) background(agent, m);
    for (const m of gone) background(agent, m, true);
    return Promise.resolve(true);
  };
  const alter = (agent: string, id: string, update: (message: Held) => Held | null, settled = false, owner = mailScope()) => {
    const rows = get().held[agent] ?? [];
    const target = rows.find((message) => message.id === id && message.owner === owner && (settled || !message.custody));
    if (!target) return Promise.resolve(false);
    const next = update(target);
    return change(
      agent,
      rows.flatMap((message) => (message === target ? (next ? [next] : []) : [message])),
      next ? [next] : [],
      next ? [] : [target],
    ).then(() => true);
  };
  return {
    held: {},
    error: null,
    ready: () => (loaded ??= load()),
    hold: (agent, message) => get().holdMany([{ agent, message }]),
    // Memory takes it before anything is awaited (a send that follows finds it there); the
    // records are written after, and what this browser can't write is still sent and kept here.
    holdMany: (messages) => {
      const owner = mailScope();
      if (!owner) {
        set({ error: NOT_KEPT });
        return Promise.resolve(false);
      }
      const held = get().held;
      const writes: { agent: string; message: Held }[] = [];
      let next = held;
      for (const { agent, message: original } of messages) {
        const message = { ...original, owner };
        const previous = next[agent] ?? [];
        const exists = previous.find((item) => item.id === message.id && item.owner === owner);
        if (exists && (JSON.stringify(heldCommand(exists)) !== JSON.stringify(heldCommand(message)) || !sameFiles(exists.originals, message.originals))) {
          set({ error: "That saved identity already has different original words or files. Send the edit as a new message." });
          return Promise.resolve(false);
        }
        if (!exists && message.custody && message.custody !== "offered") return Promise.resolve(false);
        const kept = exists ? { ...message, wait: exists.custody ? exists.wait : message.wait, originals: message.originals ?? exists.originals } : message;
        next = keptAs(next, agent, exists ? previous.map((item) => (item === exists ? kept : item)) : [...previous, kept]);
        writes.push({ agent, message: kept });
      }
      set({ held: next, error: null });
      for (const { agent, message } of writes) background(agent, message);
      return Promise.resolve(true);
    },
    attempt: (agent, id, stamp) => {
      const owner = mailScope();
      const rows = get().held[agent] ?? [];
      const original = rows.find((message) => message.owner === owner && message.id === id);
      if (!original || original.wait === stamp || original.custody === "failed") return Promise.resolve(false);
      const next = { ...original, wait: stamp };
      return change(
        agent,
        rows.map((message) => (message === original ? next : message)),
        [next],
        [],
      ).then(() => true);
    },
    edit: (agent, id, text) => alter(agent, id, (message) => ({ ...message, text })),
    uploaded: async (agent, id, attachments, owner, originals) => {
      try {
        originals = await preparedFiles(originals);
      } catch {
        set({ error: NOT_KEPT });
      }
      return alter(
        agent,
        id,
        (message) => {
          const kept = originals ?? message.originals;
          const missing = kept?.filter((file) => !attachments.some((attachment) => attachment.name === file.name)).map((file) => file.name);
          const next = {
            ...message,
            originals: kept,
            attachments: attachments.length ? attachments : undefined,
            uploading: missing?.length ? missing : undefined,
            why: missing?.length ? "Some files are not uploaded. Their original bytes remain saved here; retry the files or remove them explicitly." : undefined,
          };
          return empty(next) ? null : next;
        },
        false,
        owner ?? mailScope(),
      );
    },
    without: (agent, id, index) =>
      alter(agent, id, (message) => {
        const name = message.uploading?.[index];
        const original = message.originals?.findIndex((file) => file.name === name);
        const left = (message.uploading ?? []).filter((_, at) => at !== index);
        const next = { ...message, uploading: left.length ? left : undefined, originals: message.originals?.filter((_, at) => at !== original) };
        return empty(next) ? null : next;
      }),
    drop: (agent, ...ids) => {
      const owner = mailScope();
      const rows = get().held[agent] ?? [];
      const gone = rows.filter((message) => message.owner === owner && ids.includes(message.id) && (message.custody === undefined || message.custody === "failed"));
      return change(
        agent,
        rows.filter((message) => !gone.includes(message)),
        [],
        gone,
      ).then(() => true);
    },
    settled: (agent, ...ids) => {
      const owner = mailScope();
      const rows = get().held[agent] ?? [];
      const gone = rows.filter((message) => message.owner === owner && ids.includes(message.id));
      return change(
        agent,
        rows.filter((message) => !gone.includes(message)),
        [],
        gone,
      ).then(() => true);
    },
  };
});
/** What another tab wrote, into this one's memory (a word on the channel carries nothing else). */
async function reread() {
  try {
    useHeld.setState({ held: merged(await readAll(await database())) });
  } catch {
    /* this tab keeps what it has */
  }
}
// A bodyless word only rereads the records; it grants no send.
if (typeof window !== "undefined" && typeof indexedDB !== "undefined") {
  try {
    channel = new BroadcastChannel(DB);
    channel.onmessage = () => void reread();
  } catch {
    /* other tabs' changes show on the next visibility change */
  }
  window.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "visible") void reread();
  });
  void useHeld.getState().ready();
}
/** The records as IndexedDB has them now (tests; a page reads `useHeld`). */
export async function readHeld(): Promise<RecordSet> {
  return readAll(await database());
}
export function heldCommand(message: Held): SendCommand {
  return {
    type: "send",
    id: message.id,
    text: message.text,
    mode: message.mode ?? "queue",
    attachments: message.attachments,
    overrides: message.overrides,
    reply_to: message.reply_to,
    ...(message.task ? { task: message.task } : {}),
  };
}
export function heldFrom(command: SendCommand, at: number, rest: Partial<Held> = {}): Held {
  return { id: command.id, text: command.text, at, mode: command.mode, attachments: command.attachments, overrides: command.overrides, reply_to: command.reply_to, task: command.task, ...rest };
}

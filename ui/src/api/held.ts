/** One transactional owner for browser-held originals; Zustand is its published projection. */
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
const KEY = "kiln.agents.held";
const FAILURE = "The original message and files could not be saved. Nothing new was sent; keep this page and its text and files open.";
type RecordSet = Record<string, Held[]>;
type Change = (previous: RecordSet) => RecordSet | null;
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
// WebKit can reject IndexedDB Blob/File serialization. Persist standard ArrayBuffer data;
// the in-memory interface remains Blob-based for upload and preview callers.
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
function stored(held: RecordSet) {
  return Object.fromEntries(
    Object.entries(held).map(([agent, rows]) => [
      agent,
      rows.map((message) => ({
        ...message,
        // Only a file's bytes can be kept; one without them (none were read) isn't written, so it
        // never stops the rest from being kept.
        originals: message.originals?.flatMap((file) => (file.bytes ? [{ name: file.name, mime: file.mime, bytes: file.bytes }] : [])),
      })),
    ]),
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
let opening: Promise<IDBDatabase> | undefined;
let revision = 0;
let channel: BroadcastChannel | undefined;
function database(): Promise<IDBDatabase> {
  if (opening) return opening;
  opening = new Promise<IDBDatabase>((resolve, reject) => {
    if (typeof indexedDB === "undefined") {
      reject(new Error("storage unavailable"));
      return;
    }
    const request = indexedDB.open(KEY, 1);
    request.onupgradeneeded = () => {
      request.result.createObjectStore("held");
      request.result.createObjectStore("legacy", { autoIncrement: true });
    };
    request.onerror = () => reject(new Error("storage unavailable"));
    request.onblocked = () => reject(new Error("storage upgrade blocked"));
    request.onsuccess = () => {
      const db = request.result;
      db.onversionchange = () => {
        db.close();
        opening = undefined;
      };
      resolve(db);
    };
  }).catch((error: unknown) => {
    opening = undefined;
    throw error;
  });
  return opening;
}
/** Overlapping readwrite transactions serialize across tabs. Strict completion precedes
 * ownership transfer; raw legacy bytes are backed up without deleting the old localStorage.
 */
async function transaction(update: Change): Promise<boolean> {
  const db = await database();
  let raw: string | null;
  try {
    raw = typeof localStorage === "undefined" ? null : localStorage.getItem(KEY);
  } catch {
    throw new Error("legacy originals cannot be read");
  }
  return new Promise((resolve, reject) => {
    const tx = db.transaction(["held", "legacy"], "readwrite", { durability: "strict" });
    const store = tx.objectStore("held");
    let changed = false;
    let refused = false;
    tx.onabort = () => (refused ? resolve(false) : reject(new Error("storage transaction aborted")));
    tx.onerror = () => {};
    tx.oncomplete = () => resolve(changed);
    // A browser that can't promise a strict commit (no `durability`, or one it lowered) still keeps
    // what it can: it never stops a send (the server's waiting holds the message either way).
    const old = store.get("originals");
    old.onsuccess = () => {
      const previous = restored(old.result);
      // Rows left out as unreadable are kept as they were, beside the set, never dropped unseen.
      const repaired = rowsIn(old.result) !== rowsIn(previous);
      if (repaired) tx.objectStore("legacy").add(old.result);
      const migration = store.get("legacy-source");
      migration.onsuccess = () => {
        try {
          let migrated = false;
          if (raw && migration.result !== raw) {
            tx.objectStore("legacy").add(raw);
            for (const [agent, rows] of Object.entries(legacyHeld(raw))) {
              const existing = previous[agent] ?? [];
              previous[agent] = [...existing, ...rows.filter((row) => !existing.some((item) => item.id === row.id && item.owner === row.owner))];
            }
            store.put(raw, "legacy-source");
            migrated = true;
          }
          const next = update(previous);
          if (!next) {
            refused = true;
            tx.abort();
            return;
          }
          if (migrated || repaired || next !== previous) store.put(stored(next), "originals");
          changed = true;
        } catch {
          tx.abort();
        }
      };
    };
  });
}
export async function readHeld(): Promise<RecordSet> {
  const db = await database();
  return new Promise((resolve, reject) => {
    const tx = db.transaction("held", "readonly");
    const request = tx.objectStore("held").get("originals");
    tx.oncomplete = () => {
      try {
        resolve(restored(request.result));
      } catch {
        reject(new Error("saved originals could not be decoded"));
      }
    };
    tx.onabort = () => reject(new Error("saved originals could not be read"));
  });
}
const keptAs = (held: RecordSet, agent: string, messages: Held[]): RecordSet => {
  const next = { ...held };
  if (messages.length) next[agent] = messages;
  else delete next[agent];
  return next;
};
const empty = (message: Held) => !message.text.trim() && !message.attachments?.length && !message.uploading?.length;
export const useHeld = create<HeldState>()((set, get) => {
  const refresh = async () => {
    const observed = ++revision;
    const held = await readHeld();
    if (revision === observed) set({ held, error: null });
  };
  const change = async (update: Change): Promise<boolean> => {
    try {
      if (!(await transaction(update))) return false;
      await refresh();
      channel?.postMessage("committed");
      return true;
    } catch {
      set({ error: FAILURE });
      return false;
    }
  };
  const alter = (agent: string, id: string, update: (message: Held) => Held | null, settled = false, owner = mailScope()) =>
    change((held) =>
      keptAs(
        held,
        agent,
        (held[agent] ?? []).flatMap((message) => {
          if (message.id !== id || message.owner !== owner || (!settled && message.custody)) return [message];
          const next = update(message);
          return next ? [next] : [];
        }),
      ),
    );
  return {
    held: {},
    error: null,
    ready: async () => {
      try {
        await transaction((held) => held);
        await refresh();
        return true;
      } catch {
        set({ error: FAILURE });
        return false;
      }
    },
    hold: (agent, message) => get().holdMany([{ agent, message }]),
    holdMany: async (messages) => {
      const owner = mailScope();
      if (!owner) {
        set({ error: "The current sign-in is not identified yet. Keep the original text and files." });
        return false;
      }
      try {
        messages = await Promise.all(messages.map(async (entry) => ({ ...entry, message: { ...entry.message, originals: await preparedFiles(entry.message.originals) } })));
      } catch {
        set({ error: FAILURE });
        return false;
      }
      let conflict = false;
      const accepted = await change((held) => {
        for (const { agent, message: original } of messages) {
          const message = { ...original, owner };
          const previous = held[agent] ?? [];
          const exists = previous.find((item) => item.id === message.id && item.owner === owner);
          if (exists && (JSON.stringify(heldCommand(exists)) !== JSON.stringify(heldCommand(message)) || !sameFiles(exists.originals, message.originals))) {
            conflict = true;
            return null;
          }
          if (!exists && message.custody && message.custody !== "offered") return null;
          held = keptAs(
            held,
            agent,
            exists
              ? previous.map((item) =>
                  item.id === message.id && item.owner === owner ? { ...message, wait: item.custody ? item.wait : message.wait, originals: message.originals ?? item.originals } : item,
                )
              : [...previous, message],
          );
        }
        return held;
      });
      if (conflict) set({ error: "That saved identity already has different original words or files. Send the edit as a new message." });
      return accepted;
    },
    attempt: (agent, id, stamp) => {
      const owner = mailScope();
      return change((held) => {
        const previous = held[agent] ?? [];
        const original = previous.find((message) => message.owner === owner && message.id === id);
        if (!original || original.wait === stamp || original.custody === "failed") return null;
        return keptAs(
          held,
          agent,
          previous.map((message) => (message === original ? { ...message, wait: stamp } : message)),
        );
      });
    },
    edit: (agent, id, text) => alter(agent, id, (message) => ({ ...message, text })),
    uploaded: async (agent, id, attachments, owner, originals) => {
      try {
        originals = await preparedFiles(originals);
      } catch {
        set({ error: FAILURE });
        return false;
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
      return change((held) =>
        keptAs(
          held,
          agent,
          (held[agent] ?? []).filter((message) => message.owner !== owner || !ids.includes(message.id) || (message.custody !== undefined && message.custody !== "failed")),
        ),
      );
    },
    settled: (agent, ...ids) => {
      const owner = mailScope();
      return change((held) =>
        keptAs(
          held,
          agent,
          (held[agent] ?? []).filter((message) => message.owner !== owner || !ids.includes(message.id)),
        ),
      );
    },
  };
});
// A bodyless commit event only refreshes this tab's projection; it grants no send.
if (typeof window !== "undefined" && typeof indexedDB !== "undefined") {
  try {
    channel = new BroadcastChannel(KEY);
    channel.onmessage = () => {
      void useHeld.getState().ready();
    };
  } catch {
    /* Visibility and real source events also read the authoritative transaction store. */
  }
  window.addEventListener("visibilitychange", () => {
    void useHeld.getState().ready();
  });
  void useHeld.getState().ready();
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

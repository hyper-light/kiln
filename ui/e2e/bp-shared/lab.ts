/**
 * The shared socket's lab page (goal 3 of the browser protocol program, step 6): what
 * `bp-shared.spec.ts` drives in each tab, bundled by esbuild and served on the server's own origin
 * (`KILN_LIVE_LAB`, `crate::live::lab_file`). `?mode=lock` forces the lock fallback.
 */
import { Live } from "../../src/api/bp/shared";
import type { Topic } from "../../src/api/bp/gen";

const mode = new URLSearchParams(location.search).get("mode") === "lock" ? "lock" : "worker";
const live = new Live({ mode, worker: "/__live/worker.js" });
const opened = new Map<string, number>();

/** What the test reads and does through the page. */
const lab = {
  mode,
  open(kind: number, key: string): number {
    const topic: Topic = { kind, key };
    const id = live.open(topic);
    opened.set(`${kind}/${key}`, id);
    return id;
  },
  /** A subscription's copy: its record's field count, its items, its cursor, the changes applied. */
  read(id: number) {
    const s = live.sub(id);
    if (!s) return null;
    const hex = (b: Uint8Array) => [...b].map((x) => x.toString(16).padStart(2, "0")).join("");
    const items = Object.fromEntries([...s.state.items].map(([k, f]) => [k, f.map(hex).join(",")]));
    return { items, cursor: s.cursor, applied: s.applied, snapshots: s.snapshots, version: s.version };
  },
  conn: () => ({ conn: live.conn, owner: live.owner, owns: live.owns(), sockets: live.sockets, errors: live.errors }),
  kill: () => live.killWorker(),
  async get(path: string) {
    const o = await live.request({ t: "Http", method: { t: "Get" }, path, body: new Uint8Array() });
    return o.t === "Reply" ? { status: o.status, body: new TextDecoder().decode(o.body) } : { status: 0, body: o.t };
  },
};
(globalThis as unknown as { lab: typeof lab }).lab = lab;
document.body.dataset.ready = "1";

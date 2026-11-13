/**
 * The agents this page is starting (docs/ci.md W21-R15). Start doesn't wait for the server: the
 * agents are listed at once, starting, everywhere agents are, under ids made here that the server
 * starts them under (`POST /api/harness/sessions`' `ids`), and one agent's page opens at once,
 * showing it starting with what it was asked until its machine has it. One that couldn't start says
 * why there, or in a toast where nothing would: which of them did start the server says (some may
 * have before another couldn't); Try again asks again for the rest, under the same ids, so nothing
 * starts twice; Back puts its message and choices back in Mission control's composer.
 */
import { create } from "zustand";
import { harnessApi, sessionStore, type AgentInfo, type Spec } from "@/api/harness";
import { toast } from "@/components/ui/toast";
import { harnessSessionsRes } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { PEER_BASE } from "@/lib/peer-base";
import { invalidate, setData } from "@/lib/resource";
import { MISSION_DRAFT, usePrefill } from "./new-agent-draft";
import { useDrafts, useHeld } from "./state";

export interface Start {
  /** Its agents' ids, made here, in order. */
  ids: string[];
  target: string;
  spec: Spec;
  /** Said while it starts: its harness's name, and its machine's. */
  kindName: string;
  machine: string;
  /** Its first message (in its spec, or held to go with its files), and those files' names. */
  text: string;
  files: string[];
  /** Its first message's id when it waits here with its files (`useHeld`), its page sending it. */
  heldFirst?: string;
  at: number;
  state: "starting" | "started" | "failed";
  why?: string;
  /** Its agents that started, as the server has them (those of an earlier try too; ids of its own, from a server that makes them itself). */
  started?: AgentInfo[];
  /** What's done once they've all started: what's kept as Mission control's choices, several agents' files sent to them all. */
  after?: () => Promise<void> | void;
}

interface Starting {
  /** Each start under each of its agents' ids. */
  starts: Record<string, Start>;
}

export const useStarting = create<Starting>()(() => ({ starts: {} }));

/** How long a start that's done is kept, its page reading what it was asked meanwhile. */
const KEPT_MS = 120_000;

/** The start of agent `id` this page made, while it goes, failed, or a moment after it's done. */
export function useStart(id: string): Start | undefined {
  return useStarting((s) => s.starts[id]);
}

const settling = new WeakMap<Start, Promise<void>>();

/** Settles once start `s` (one under way) has started or couldn't: what its page waits on, in Suspense. */
export function settled(s: Start): Promise<void> {
  const kept = settling.get(s);
  if (kept) return kept;
  const id = s.ids[0] ?? "";
  const p = new Promise<void>((resolve) => {
    const off = useStarting.subscribe((st) => {
      if (st.starts[id] === s) return;
      off();
      resolve();
    });
    if (useStarting.getState().starts[id] !== s) {
      off();
      resolve();
    }
  });
  settling.set(s, p);
  return p;
}

function put(s: Start) {
  useStarting.setState((st) => ({ starts: { ...st.starts, ...Object.fromEntries(s.ids.map((id) => [id, s])) } }));
}

/** The entries listed here for agents still starting (`listing`): what the server says of them takes their place. */
const stood = new WeakSet<AgentInfo>();

/** Agents `ids` of `s` as they're listed while they start: as the server lists one it hasn't heard from yet. */
function listing(s: Start, ids: string[]): AgentInfo[] {
  const name = s.spec.name?.trim() || s.kindName;
  const shown = ids.map((id): AgentInfo => ({
    id,
    kind: s.spec.kind,
    kind_name: s.kindName,
    target: s.target,
    name: s.ids.length > 1 ? `${name} ${s.ids.indexOf(id) + 1}` : name,
    cwd: s.spec.cwd ?? "",
    created_at: s.at,
    status: "starting",
    title: "",
    model: s.spec.model ?? "",
    mode: s.spec.mode ?? "",
    harness_session: s.spec.resume,
    approvals: [],
    queued: 0,
    tasks_running: 0,
    cost_usd: 0,
    last_activity: s.at,
    goal: s.text.slice(0, 200),
    summary: "",
    todos_done: 0,
    todos_total: 0,
    // Not started yet: kiln has no start of its harness to record.
    starts: [],
    files_changed: 0,
    lines_added: 0,
    lines_removed: 0,
  }));
  for (const a of shown) stood.add(a);
  return shown;
}

/** The lists of agents the pages read: every page's (lib/data.ts) and Mission control's (use-agents.ts). */
const LISTS = [harnessSessionsRes.key, "harness/agents"];

/**
 * `fresh` in the lists of agents, added, or in place of what had their ids: a listing's; and, unless
 * `over` is only those, the server's own (its events may have said more of them by now); `drop`'s
 * taken out.
 */
function list(fresh: AgentInfo[], drop: string[] = [], over: "any" | "listed" = "any") {
  for (const key of LISTS)
    setData<AgentInfo[]>(key, (all) => {
      const by = new Map(fresh.map((a) => [a.id, a]));
      const kept = all.filter((a) => !drop.includes(a.id)).map((a) => (over === "any" || stood.has(a) ? (by.get(a.id) ?? a) : a));
      return [...kept, ...fresh.filter((a) => !all.some((b) => b.id === a.id))];
    });
}

/** The lines under Mission control's box saying how a start of several goes, by its first agent's id, while they show. */
const lines = new Set<string>();

/** The ref of the line that says how start `id` goes: while it shows, a failure isn't said in a toast as well. */
export function showsLine(id: string) {
  return (el: HTMLElement | null) => {
    if (!el) return;
    lines.add(id);
    return () => void lines.delete(id);
  };
}

/** Said in a toast when nothing that would say it shows: its own page, or for several the line under Mission control's box. */
function sayElsewhere(s: Start, why: string) {
  const first = s.ids[0] ?? "";
  const shown = s.ids.length === 1 ? location.pathname.slice(PEER_BASE.length).replace(/\/$/, "") === `/agents/${first}` : lines.has(first);
  if (shown) return;
  const what = s.ids.length === 1 ? `${s.kindName} on ${s.machine}` : `${s.ids.length} ${s.kindName} agents on ${s.machine}`;
  toast({ title: `Couldn't start ${what}`, description: why, tone: "failed", href: s.ids.length === 1 ? `/agents/${first}` : "/agents" });
}

/** Its ids still to start: all of them, but those an earlier try started. */
const toStart = (s: Start) => s.ids.filter((id) => !(s.started ?? []).some((a) => a.id === id));

async function go(s: Start) {
  const asked = toStart(s);
  try {
    const made = await harnessApi.start(s.target, s.spec, asked.length, asked);
    done(s, [...(s.started ?? []), ...made]);
  } catch (e) {
    // Refused, or its answer lost: which of them are there, as the server has them now. Some may
    // have started before another couldn't, and one asked again may have got there the first time
    // (refused then as there already).
    const there = (await Promise.all(asked.map((id) => harnessApi.session(id).catch(() => null)))).filter((a): a is AgentInfo => a !== null);
    const started = [...(s.started ?? []), ...there];
    if (there.length === asked.length) done(s, started);
    else failed(s, started, errorMessage(e));
  }
}

function failed(s: Start, started: AgentInfo[], why: string) {
  const no: Start = { ...s, state: "failed", why, started };
  put(no);
  // Those that did start, as the server has them; the rest leave the lists.
  list(started, toStart(no), "listed");
  sayElsewhere(s, why);
}

function done(s: Start, started: AgentInfo[]) {
  const ok: Start = { ...s, state: "started", why: undefined, started };
  put(ok);
  void Promise.resolve()
    .then(() => s.after?.())
    .catch((e: unknown) => put({ ...ok, why: `Started, but the files didn't go: ${errorMessage(e)}` }));
  // The server's word for them (a server that makes its own ids lists those: its listing goes).
  list(
    started,
    s.ids.filter((id) => !started.some((a) => a.id === id)),
    "listed",
  );
  for (const id of s.ids) sessionStore(id).wake();
  invalidate("harness/agents");
  window.setTimeout(() => {
    useStarting.setState((st) => ({ starts: Object.fromEntries(Object.entries(st.starts).filter(([id, kept]) => !(s.ids.includes(id) && kept.state === "started"))) }));
  }, KEPT_MS);
}

/** Start `s`'s agents: listed at once, starting; its page says how it goes. */
export function startAgents(s: Omit<Start, "state" | "at" | "why" | "started">) {
  const start: Start = { ...s, at: Date.now(), state: "starting" };
  put(start);
  list(listing(start, start.ids));
  void go(start);
}

/** Ask again for those that didn't start, under their ids (one that got there after all isn't started twice). */
export function startAgain(id: string) {
  const s = useStarting.getState().starts[id];
  if (!s || s.state !== "failed") return;
  const again: Start = { ...s, state: "starting", why: undefined };
  put(again);
  list(listing(again, toStart(again)));
  void go(again);
}

/**
 * What a start that couldn't asked, back in Mission control's box: its words (unless something's
 * been written there since) and its choices (its machine, folder, harness, model, effort and
 * permissions, said to come from it); the start and what it kept to send are let go.
 */
export function putBack(s: Start) {
  if (s.files.length || s.ids.some((id) => (useHeld.getState().held[id] ?? []).some((message) => message.custody))) {
    toast.error("The original stays with this start", "Its files or delivery are still held. Retry this start; returning only its text would leave those originals behind.");
    return;
  }
  const d = useDrafts.getState();
  if (s.text && !(d.drafts[MISSION_DRAFT] ?? "").trim()) d.setDraft(MISSION_DRAFT, s.text);
  const { kind, cwd, shell, mode, model, effort } = s.spec;
  usePrefill.getState().hand({ from: "As the start that couldn't asked", kind, target: s.target, cwd, shell, mode, model, effort });
  for (const id of s.ids) void useHeld.getState().drop(id, ...(useHeld.getState().held[id] ?? []).map((m) => m.id));
  dropStart(s.ids[0] ?? "");
}

/** Let go of a start that failed: what it asked is gone from here. */
export function dropStart(id: string) {
  const s = useStarting.getState().starts[id];
  if (!s) return;
  useStarting.setState((st) => ({ starts: Object.fromEntries(Object.entries(st.starts).filter(([k]) => !s.ids.includes(k))) }));
}

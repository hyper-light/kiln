/**
 * Repositories on machines (GET /api/places, docs/sync.md): what each mock machine holds, as its
 * runner would say, and clones, syncs, moves and removals that go through their states over a few
 * seconds, as the server's tasks do, then change what the machines hold.
 */
import type http from "node:http";
import type { Agent, GitState, Place, PlaceAsk, PlaceMachine, PlacesView, PlaceTask, Repo, RepoRow, ServerEvent, TaskKind } from "../src/api/types.ts";
import { hashStr, mulberry32 } from "./data.ts";
import { syncView } from "./pages.ts";

export interface PlacesCtx {
  agents: () => Agent[];
  repos: () => Repo[];
  emit: (ev: ServerEvent) => void;
  json: (res: http.ServerResponse, status: number, body: unknown) => void;
  err: (res: http.ServerResponse, status: number, message: string) => void;
  body: <T>(req: http.IncomingMessage) => Promise<T>;
}

const now = () => Math.floor(Date.now() / 1000);
const hex = (r: () => number, n: number) => Array.from({ length: n }, () => Math.floor(r() * 16).toString(16)).join("");
const homeOf = (a: Agent) => (a.os === "macos" ? "/Users/ada" : a.os === "windows" ? "C:\\Users\\ada" : "/home/ada");
/** Runners too old to clone or move repositories (the mock's 0.0.9). */
const old = (a: Agent) => a.version.startsWith("0.0.");

/** What each machine holds, path → what git says of it. */
const held = new Map<string, Map<string, GitState>>();
const tasks: PlaceTask[] = [];
let seeded = false;

function key(origin: string | null, machine: string, path: string) {
  if (!origin) return `here:${machine}:${path}`;
  return origin
    .replace(/^[a-z]+:\/\//i, "")
    .replace(/^[^@/]+@/, "")
    .replace(":", "/")
    .replace(/\.git$/, "")
    .toLowerCase();
}

function git(r: () => number, path: string, origin: string | null, kiln = false): GitState {
  const changed = r() < 0.3 ? Math.floor(1 + r() * 6) : 0;
  return { path, origin, branch: r() < 0.85 ? "main" : "feature/places", head: hex(r, 40), upstream: origin ? "origin/main" : null, ahead: r() < 0.2 ? 2 : 0, behind: r() < 0.25 ? Math.floor(1 + r() * 9) : 0, changed, active: now() - Math.floor(r() * 86_400 * 9), kiln };
}

function seed(ctx: PlacesCtx) {
  if (seeded) return;
  seeded = true;
  const machines = ctx.agents().filter((a) => !a.ephemeral);
  for (const a of machines) {
    const r = mulberry32(hashStr(a.name));
    const mine = new Map<string, GitState>();
    const home = homeOf(a);
    const dir = a.os === "macos" ? "Projects" : "src";
    for (const repo of ctx.repos()) {
      if (r() < 0.45) continue;
      const name = repo.name.split("/")[1]!;
      const path = `${home}/${dir}/${name}`;
      mine.set(path, git(r, path, r() < 0.5 ? repo.clone_url : `git@github.com:${repo.name}.git`));
    }
    if (r() < 0.6) mine.set(`${home}/dotfiles`, git(r, `${home}/dotfiles`, "https://github.com/ada/dotfiles.git"));
    if (r() < 0.4) mine.set(`${home}/${dir}/scratch`, git(r, `${home}/${dir}/scratch`, null));
    if (a.name.includes("epyc")) mine.set(`/srv/bench/hyper-raft`, git(r, `/srv/bench/hyper-raft`, "https://github.com/hyper-light/hyper-raft.git", true));
    held.set(a.name, mine);
  }
}

function machinesOf(ctx: PlacesCtx): PlaceMachine[] {
  return ctx
    .agents()
    .filter((a) => !a.ephemeral)
    .map((a) => ({ name: a.name, os: a.os, arch: a.arch, labels: a.labels, connected: a.connected, home: homeOf(a), looked: now() - 40, error: null, can_clone: a.connected && !old(a), can_sync: a.connected && !old(a) }));
}

function view(ctx: PlacesCtx): PlacesView {
  seed(ctx);
  const online = new Map(ctx.agents().map((a) => [a.name, a.connected]));
  const rows = new Map<string, RepoRow>();
  for (const b of ctx.repos()) {
    const k = key(b.clone_url, "", "");
    rows.set(k, { key: k, name: b.name, origin: b.clone_url, built: b.id, synced: null, places: [] });
  }
  // Synced folders: with the repository kiln builds of the same name (as the server joins them).
  const placed = new Set<string>();
  for (const s of syncView(ctx).repos) {
    const b = ctx.repos().find((r) => r.name === s.name || r.name.split("/").pop() === s.name);
    const k = b ? key(b.clone_url, "", "") : `synced:${s.id}`;
    const row = rows.get(k) ?? { key: k, name: s.name, origin: null, built: null, synced: null, places: [] };
    row.synced = s.id;
    for (const c of s.copies) {
      const g = held.get(c.machine)?.get(c.path) ?? null;
      placed.add(`${c.machine}\n${c.path}`);
      row.places.push({ machine: c.machine, path: c.path, kind: "copy", git: g, copy: { repo: s.id, member: c.member, state: c.state, moving: false }, online: online.get(c.machine) ?? false });
    }
    rows.set(k, row);
  }
  for (const [machine, mine] of held) {
    for (const g of mine.values()) {
      if (placed.has(`${machine}\n${g.path}`)) continue;
      const k = key(g.origin, machine, g.path);
      const row = rows.get(k) ?? { key: k, name: g.origin ? k.split("/").slice(1).join("/") : g.path.split("/").pop()!, origin: g.origin, built: null, synced: null, places: [] };
      const place: Place = { machine, path: g.path, kind: g.kiln ? "clone" : "found", git: g, copy: null, online: online.get(machine) ?? false };
      row.places.push(place);
      rows.set(k, row);
    }
  }
  const repos = [...rows.values()].map((r) => ({ ...r, places: r.places.sort((a, b) => a.machine.localeCompare(b.machine) || a.path.localeCompare(b.path)) }));
  repos.sort((a, b) => a.name.localeCompare(b.name));
  return { repos, machines: machinesOf(ctx), tasks: [...tasks].sort((a, b) => b.created - a.created) };
}

/** The states a task of each kind goes through, with what each says. */
const STEPS: Record<TaskKind, [string, string][]> = {
  clone: [
    ["bundling", "kiln bundles its mirror"],
    ["fetching", "fetched 38.0 MB of 61.2 MB"],
    ["cloning", "cloning"],
  ],
  sync: [
    ["bundling", "bundling its commits"],
    ["fetching", "fetched 12.4 MB of 61.2 MB"],
    ["cloning", "cloning"],
    ["joining", "joins the sync"],
    ["catching up", "takes the files (212 to take, fetching 30)"],
  ],
  move: [
    ["bundling", "bundling its commits"],
    ["cloning", "cloning"],
    ["catching up", "takes the files (40 to take)"],
    ["leaving", "the old copy stops syncing"],
  ],
  remove: [["leaving", "stops syncing it"]],
};

function run(ctx: PlacesCtx, t: PlaceTask, origin: string | null) {
  const steps = STEPS[t.kind];
  steps.forEach(([state, detail], i) => {
    setTimeout(() => {
      Object.assign(t, { state, detail, updated: now() });
      t.notes.push({ at: now(), text: `${state}: ${detail}` });
      ctx.emit({ type: "places" });
    }, 1200 * (i + 1));
  });
  setTimeout(() => {
    const fail = t.machine.includes("pi");
    if (fail) {
      Object.assign(t, { state: "failed", error: `${t.machine} stopped saying how it goes`, updated: now() });
    } else {
      const r = mulberry32(hashStr(t.id));
      const from = t.from.kind === "machine" ? t.from : null;
      if (t.kind === "remove") {
        if (from) held.get(from.machine)?.delete(from.path);
      } else {
        held.get(t.machine)?.set(t.path, { ...git(r, t.path, origin, true), changed: 0, ahead: 0, behind: 0, active: now() });
        if (t.kind === "move" && t.delete && from) held.get(from.machine)?.delete(from.path);
      }
      Object.assign(t, { state: "done", detail: t.kind === "remove" ? "given up" : `${t.kind === "move" ? "moved" : t.kind === "sync" ? "in step" : "cloned"} into ${t.path} on ${t.machine}`, updated: now() });
    }
    t.notes.push({ at: now(), text: t.state === "failed" ? `failed: ${t.error}` : `done: ${t.detail}` });
    ctx.emit({ type: "places" });
  }, 1200 * (steps.length + 1));
}

function start(ctx: PlacesCtx, kind: TaskKind, ask: PlaceAsk): PlaceTask[] | string {
  const v = view(ctx);
  const row = v.repos.find((r) => r.key === ask.key);
  if (!row) return "no such repository";
  const batch = `b${hex(Math.random, 8)}`;
  const targets = kind === "remove" ? [{ machine: ask.from?.kind === "machine" ? ask.from.machine : "", path: ask.from?.kind === "machine" ? ask.from.path : "" }] : (ask.to ?? []);
  if (targets.length === 0) return "choose a machine for it to go to";
  const out: PlaceTask[] = [];
  for (const to of targets) {
    const m = v.machines.find((x) => x.name === to.machine);
    if (!m) return `no machine named ${to.machine}`;
    if (!m.connected) return `${to.machine} is offline`;
    const path = to.path.startsWith("~") ? `${m.home}${to.path.slice(1)}` : to.path;
    if (kind !== "remove" && row.places.some((p) => p.machine === to.machine && p.path === path)) return `${row.name} is at ${path} on ${to.machine} already`;
    const t: PlaceTask = {
      id: `t${hex(Math.random, 10)}`,
      batch,
      kind,
      key: row.key,
      name: row.name,
      from: ask.from ?? (row.built ? { kind: "kiln", repo: row.built } : { kind: "url", url: row.origin ?? "" }),
      branch: ask.branch,
      machine: to.machine,
      path,
      delete: Boolean(ask.delete),
      state: "queued",
      detail: "starting",
      notes: [],
      created: now(),
      updated: now(),
    };
    tasks.push(t);
    out.push(t);
    run(ctx, t, row.origin);
  }
  ctx.emit({ type: "places" });
  return out;
}

/** Handles /api/places; false for any other route. */
export async function handlePlaces(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: PlacesCtx): Promise<boolean> {
  const p = url.pathname;
  const m = req.method ?? "GET";
  if (!p.startsWith("/api/places")) return false;
  let mm: RegExpMatchArray | null;
  if (p === "/api/places" && m === "GET") return (void ctx.json(res, 200, view(ctx)), true);
  if (p === "/api/places/look" && m === "POST") return (void ctx.json(res, 202, { asked: machinesOf(ctx).filter((x) => x.can_clone).length }), true);
  if ((mm = p.match(/^\/api\/places\/(clone|sync|move|remove)$/)) && m === "POST") {
    const out = start(ctx, mm[1] as TaskKind, await ctx.body<PlaceAsk>(req));
    return (typeof out === "string" ? ctx.err(res, 409, out) : ctx.json(res, 201, out), true);
  }
  if ((mm = p.match(/^\/api\/places\/tasks\/([^/]+)\/retry$/)) && m === "POST") {
    const t = tasks.find((x) => x.id === decodeURIComponent(mm![1]!));
    if (!t || t.state !== "failed") return (void ctx.err(res, 409, "only a failed task is started again"), true);
    tasks.splice(tasks.indexOf(t), 1);
    const out = start(ctx, t.kind, { key: t.key, from: t.from, branch: t.branch, to: t.kind === "remove" ? undefined : [{ machine: t.machine, path: t.path }], delete: t.delete });
    return (typeof out === "string" ? ctx.err(res, 409, out) : ctx.json(res, 201, out), true);
  }
  if ((mm = p.match(/^\/api\/places\/tasks\/([^/]+)$/)) && m === "DELETE") {
    const i = tasks.findIndex((x) => x.id === decodeURIComponent(mm![1]!) && (x.state === "done" || x.state === "failed"));
    if (i < 0) return (void ctx.err(res, 409, "no such task, or it is still under way"), true);
    tasks.splice(i, 1);
    ctx.emit({ type: "places" });
    return (void (res.writeHead(204), res.end()), true);
  }
  return false;
}

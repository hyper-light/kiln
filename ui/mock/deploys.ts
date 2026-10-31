/**
 * Ship's Deploys in the mock (docs/ci.md W26-R24; Ada's approved stills of 2026-10-30, v5 and v6):
 * three projects' apps as the server answers `GET /api/ship/deploys` (each one's stand, line and
 * pips as `kiln_cd::stands` says them, needs-you first), each app's pane, and the strip's verbs
 * (`POST /api/ship/deploys`), answered as the server answers them. What a picked object shows comes
 * from the CD routes the pane reads (its logs, events, manifest and diff, a commit's plan), for these
 * apps only. What each browser did is kept by its user agent, as the jobs pane's is, so the e2e
 * projects running side by side don't see each other's.
 */
import type http from "node:http";
import type { ShipDeployGroup, ShipDeployLine, ShipDeployPane, ShipDeployProject, ShipDeployRow, ShipDeploySetting, ShipDeploys, ShipDeployVerb, ShipPip } from "../src/api/ship.ts";

type Ctx = { json: (res: http.ServerResponse, status: number, body: unknown) => void };

let emit: (ev: unknown) => void = () => {};
/** How the mock tells the pages something changed (`cd_apps`, `cd_rollout`). */
export function setDeploysEmit(fn: (ev: unknown) => void) {
  emit = fn;
}

async function readBody(req: http.IncomingMessage): Promise<string> {
  const chunks: Buffer[] = [];
  for await (const c of req) chunks.push(c as Buffer);
  return Buffer.concat(chunks).toString("utf8");
}

const MIN = 60;
const HOUR = 3600;
const DAY = 86_400;
const now = () => Math.floor(Date.now() / 1000);

const pips = (sync: string, health: string, soak: string): ShipPip[] => [
  { stage: "sync", state: sync },
  { stage: "health", state: health },
  { stage: "soak", state: soak },
];

/** The commits the stills name, whole. */
const SHA: Record<string, string> = {
  "15878aa": "15878aaf0b6c2d1e3f4a5b6c7d8e9f0a1b2c3d4e",
  "021a29f": "021a29f57301b6a1c2d3e4f5a6b7c8d9e0f1a2b3",
  b1ef50d: "b1ef50d5ce4a3b2c1d0e9f8a7b6c5d4e3f2a1b0c",
  d90465f: "d90465ff49aa11bb22cc33dd44ee55ff66aa77bb",
  b626724: "b62672456d0e1f2a3b4c5d6e7f8091a2b3c4d5e6",
  a946ea5: "a946ea5c3b2a19f8e7d6c5b4a3928170f6e5d4c3",
  "1f9b2c4": "1f9b2c4e8d7c6b5a49382716f5e4d3c2b1a09f8e",
  "6b0d2e1": "6b0d2e1a9f8e7d6c5b4a39281706f5e4d3c2b1a0",
  "7ac3e1d": "7ac3e1d2c3b4a5968778695a4b3c2d1e0f9a8b7c",
  "9d1e0a7": "9d1e0a7b6c5d4e3f2a1b0c9d8e7f6a5b4c3d2e1f",
  c2f81aa: "c2f81aa0b9c8d7e6f5a4b3c2d1e0f9a8b7c6d5e4",
};

interface App {
  row: Omit<ShipDeployRow, "at"> & { ago: number };
  pane: Omit<ShipDeployPane, "app">;
}

const settings = (autoSync: boolean, from = "deploy-file"): ShipDeploySetting[] => [
  { key: "auto_sync", on: autoSync, from, what: autoSync ? "syncs each time git changes what it renders" : "syncs when you say", can: true },
  { key: "auto_rollback", on: true, from: "default", what: "goes back on a failed deploy", can: true },
  { key: "prune", on: false, from: "deploy-file", what: "keeps what git dropped", can: false },
  { key: "self_heal", on: false, from: "deploy-file", what: "leaves changes made by hand", can: false },
];
const item = (kind: string, name: string, state: string, v: string, o: { group?: string; ns: string; logs?: boolean; differs?: boolean; node?: string; container?: string }) => ({
  key: `${o.group ?? ""}~${kind}~${o.ns}~${name}`,
  group: o.group ?? "",
  kind,
  namespace: o.ns,
  name,
  state,
  v,
  logs: o.logs ?? false,
  differs: o.differs ?? false,
  ...(o.node ? { node: o.node } : {}),
  ...(o.container ? { container: o.container } : {}),
});
const deploy = (n: number, short: string, subject: string, ago: number, o: { now?: boolean; dim?: boolean; state?: string; by?: string; to?: boolean } = {}): ShipDeployLine => ({
  deploy: n,
  sha: SHA[short] ?? short,
  short,
  subject,
  by: o.by ?? "ada",
  at_ms: (now() - ago) * 1000,
  state: o.state ?? (o.now ? "healthy" : "superseded"),
  now: !!o.now,
  dim: !!o.dim,
  to: o.to === false || o.now ? null : n,
  revision: null,
  same_as: null,
});
const commit = (short: string, subject: string, ago: number, by = "user.email", same_as: number | null = null): ShipDeployLine => ({
  deploy: null,
  sha: SHA[short] ?? short,
  short,
  subject,
  by,
  at_ms: (now() - ago) * 1000,
  state: "none",
  now: false,
  dim: false,
  to: null,
  revision: SHA[short] ?? short,
  same_as,
});
const NO_ROLLOUT = { promote: "no rollout is under way", abort: "no rollout is under way", approve: "nothing waits for an approval", deny: "nothing waits for an approval" };
/** Deleted here, an app its repository's deploy file declares would come back. */
const DECLARED = { delete: "its repository's .kiln/deploy.yaml declares it: take it out there" };

/** kiln-production's objects, as the stills draw them. */
const KILN_GROUPS: ShipDeployGroup[] = [
  {
    name: "Deployment kiln",
    note: "1/1 ready · out of sync",
    owner: item("Deployment", "kiln", "changes", "differs from git", { group: "apps", ns: "kiln", logs: true, differs: true }),
    items: [
      item("Pod", "kiln-cd9dc7b7-rsmwz", "running", "up 3m · 4 restarts", { ns: "kiln", logs: true, node: "kiln-m02", container: "kiln" }),
      item("ReplicaSet", "kiln-cd9dc7b7", "healthy", "1 pod", { group: "apps", ns: "kiln", logs: true }),
    ],
  },
  {
    name: "Network",
    note: "synced",
    owner: null,
    items: [item("Service", "kiln", "healthy", "ClusterIP · 3 ports", { ns: "kiln" }), item("Ingress", "kiln", "healthy", "kiln.lan", { group: "networking.k8s.io", ns: "kiln" })],
  },
  {
    name: "Config",
    note: "1 differs from git",
    owner: null,
    items: [item("ConfigMap", "kiln-config", "changes", "differs from git", { ns: "kiln", differs: true }), item("Secret", "kiln", "healthy", "synced", { ns: "kiln" })],
  },
  { name: "Storage", note: "synced", owner: null, items: [item("PersistentVolumeClaim", "kiln-data", "healthy", "Bound · 200Gi", { ns: "kiln" })] },
  { name: "Jobs", note: "", owner: null, items: [item("Job", "kiln-guard-94lwq", "passed", "completed 12h ago", { group: "batch", ns: "kiln", logs: true })] },
];

/** A workload and its config, for the others' panes. */
const small = (name: string, ns: string, pods: number, state = "running"): ShipDeployGroup[] => [
  {
    name: `Deployment ${name}`,
    note: `${pods}/${pods} ready`,
    owner: item("Deployment", name, "healthy", `${pods}/${pods} ready`, { group: "apps", ns, logs: true }),
    items: Array.from({ length: pods }, (_, i) =>
      item("Pod", `${name}-5c7d9b-${["x2k4f", "p8r1q", "m3n7v"][i] ?? "z0z0z"}`, state, `up ${2 + i}h`, { ns, logs: true, node: i % 2 ? "kiln" : "kiln-m02", container: name }),
    ),
  },
  { name: "Network", note: "synced", owner: null, items: [item("Service", name, "healthy", "ClusterIP · 1 port", { ns })] },
  { name: "Config", note: "synced", owner: null, items: [item("ConfigMap", `${name}-config`, "healthy", "synced", { ns })] },
];

const APPS: App[] = [
  {
    row: {
      app: "default/kiln-production",
      name: "kiln-production",
      project: "hyper-light/kiln",
      environment: "production",
      cluster: "in-cluster",
      namespace: "kiln",
      stand: "needs_you",
      word: "Out of sync",
      line: "#153 runs 15878aa · trunk is 2 ahead · healthy",
      pips: pips("passed", "healthy", "passed"),
      by: "agent:01a18b08 for user:admin",
      ago: 36 * MIN,
      runs: "15878aa",
      sync: "OutOfSync",
      health: "Healthy",
      auto_sync: false,
      pinned: null,
    },
    pane: {
      about: { cluster: "in-cluster", namespace: "kiln", objects: 22, source: "deploy/helm/kiln @ main", repo: "https://github.com/hyper-light/kiln" },
      groups: KILN_GROUPS,
      tree: { live: true, errors: [] },
      rollout: null,
      history: [
        commit("021a29f", "The agent page's composer keeps Mission Control's grey border", HOUR),
        commit("b1ef50d", "Merge ship-made-by: Files colours and ask about a commit's lines", 2 * HOUR),
        deploy(153, "15878aa", "Merge main (eecbd0decf) into fix/frozen-front-door", 36 * MIN, { now: true, by: "agent:01a18b08 for user:admin" }),
        commit("d90465f", "Runner upgrades: progress never waits on the uplink", DAY),
        deploy(151, "b626724", "Merge main (f381c4220f) into observe-merge-main", DAY, { dim: true }),
        deploy(149, "a946ea5", "Mission control: an offline machine's notice can be put away", 2 * DAY, { dim: true }),
      ],
      settings: settings(false),
      why_not: { stop: "no sync is running", follow: "it follows git already", ...NO_ROLLOUT, ...DECLARED },
    },
  },
  {
    row: {
      app: "default/kiln-staging",
      name: "kiln-staging",
      project: "hyper-light/kiln",
      environment: "staging",
      cluster: "in-cluster",
      namespace: "kiln-staging",
      stand: "shipped",
      word: "Synced",
      line: "#88 runs 021a29f · healthy",
      pips: pips("passed", "healthy", "passed"),
      by: "auto-sync",
      ago: HOUR,
      runs: "021a29f",
      sync: "Synced",
      health: "Healthy",
      auto_sync: true,
      pinned: null,
    },
    pane: {
      about: { cluster: "in-cluster", namespace: "kiln-staging", objects: 22, source: "deploy/helm/kiln @ main", repo: "https://github.com/hyper-light/kiln" },
      groups: small("kiln", "kiln-staging", 1),
      tree: { live: true, errors: [] },
      rollout: null,
      history: [
        deploy(88, "021a29f", "The agent page's composer keeps Mission Control's grey border", HOUR, { now: true, by: "auto-sync" }),
        deploy(87, "b1ef50d", "Merge ship-made-by: Files colours and ask about a commit's lines", 2 * HOUR, { dim: true }),
      ],
      settings: settings(true),
      why_not: { stop: "no sync is running", follow: "it follows git already", ...NO_ROLLOUT, ...DECLARED },
    },
  },
  {
    row: {
      app: "default/kube-prometheus-stack-production",
      name: "kube-prometheus-stack-production",
      project: "hyper-light/kiln",
      environment: "production",
      cluster: "in-cluster",
      namespace: "monitoring",
      stand: "shipped",
      word: "Synced",
      line: "chart 92.2.0 · healthy",
      pips: pips("passed", "healthy", "passed"),
      by: "admin",
      ago: 6 * DAY,
      runs: "92.2.0",
      sync: "Synced",
      health: "Healthy",
      auto_sync: false,
      pinned: null,
    },
    pane: {
      about: { cluster: "in-cluster", namespace: "monitoring", objects: 140, source: "chart kube-prometheus-stack 92.2.0", repo: null },
      groups: small("prometheus", "monitoring", 2),
      tree: { live: true, errors: [] },
      rollout: null,
      history: [
        { ...deploy(4, "92.2.0", "successfully synced (all tasks run)", 6 * DAY, { now: true, by: "admin" }), sha: "92.2.0", revision: null },
        { ...deploy(3, "91.0.1", "successfully synced (all tasks run)", 30 * DAY, { dim: true, by: "admin" }), sha: "91.0.1", revision: null },
      ],
      settings: settings(false, "default"),
      why_not: { stop: "no sync is running", follow: "it follows git already", ...NO_ROLLOUT },
    },
  },
  {
    row: {
      app: "default/quiver-production",
      name: "quiver-production",
      project: "hyper-light/quiver",
      environment: "production",
      cluster: "in-cluster",
      namespace: "quiver",
      stand: "needs_you",
      word: "Needs you",
      line: "#39 waits for your approval · #38 runs",
      pips: pips("requested", "none", "none"),
      by: "ada",
      ago: 2 * DAY,
      runs: "1f9b2c4",
      sync: "Synced",
      health: "Progressing",
      auto_sync: false,
      pinned: null,
    },
    pane: {
      about: { cluster: "in-cluster", namespace: "quiver", objects: 9, source: "deploy @ main", repo: "https://github.com/hyper-light/quiver" },
      groups: small("quiver", "quiver", 3),
      tree: { live: true, errors: [] },
      rollout: {
        id: "in-cluster|apps/Rollout/quiver/quiver",
        revision: "1f9b2c4",
        state: "requested",
        note: "canary · 20% · waiting for approval",
        steps: [
          { n: 1, state: "passed", what: "weight 20%", v: "done 4m" },
          { n: 2, state: "requested", what: "a person approves", v: "waiting for approval" },
          { n: 3, state: "none", what: "weight 100%", v: "next" },
        ],
        under_way: true,
        waits: "waiting for approval",
        approvals: ["release"],
        deploy: 39,
        back_to: 38,
        weight: 20,
        strategy: "canary",
        said: "waiting for approval",
        promote_to: 100,
        sides: [
          { side: "stable", name: "quiver-5c7d9b", pods: 3, share: 80, up: null },
          { side: "canary", name: "quiver-6f1c02", pods: 1, share: 20, up: "4m" },
        ],
      },
      history: [
        deploy(39, "1f9b2c4", "Quiver: a follower's vote waits for its grant", 2 * DAY, { now: true }),
        deploy(38, "6b0d2e1", "Quiver: elections settle within one term", 9 * DAY, { dim: true }),
      ],
      settings: settings(false),
      why_not: { stop: "no sync is running", follow: "it follows git already", ...DECLARED },
    },
  },
  {
    row: {
      app: "default/quiver-staging",
      name: "quiver-staging",
      project: "hyper-light/quiver",
      environment: "staging",
      cluster: "in-cluster",
      namespace: "quiver-staging",
      stand: "shipping",
      word: "Syncing",
      line: "#42 · syncing 7ac3e1d",
      pips: pips("running", "none", "none"),
      by: "auto-sync",
      ago: 2,
      runs: "1f9b2c4",
      sync: "OutOfSync",
      health: "Progressing",
      auto_sync: true,
      pinned: null,
    },
    pane: {
      about: { cluster: "in-cluster", namespace: "quiver-staging", objects: 9, source: "deploy @ main", repo: "https://github.com/hyper-light/quiver" },
      groups: small("quiver", "quiver-staging", 1, "queued"),
      tree: { live: true, errors: [] },
      rollout: null,
      history: [
        commit("7ac3e1d", "Quiver: the leader steps down on a slow disk", 20 * MIN, "ada"),
        deploy(41, "1f9b2c4", "Quiver: a follower's vote waits for its grant", 2 * DAY, { now: true, by: "auto-sync" }),
      ],
      settings: settings(true),
      why_not: { sync: "a sync is running: stop it first", rollback: "a sync is running: stop it first", follow: "it follows git already", ...NO_ROLLOUT, ...DECLARED },
    },
  },
  {
    row: {
      app: "default/site-production",
      name: "site-production",
      project: "hyper-light/hyperlight-site",
      environment: "production",
      cluster: "in-cluster",
      namespace: "site",
      stand: "rolled_back",
      word: "Rolled back",
      line: "#19 failed its health check · back on #18",
      pips: pips("passed", "failed", "none"),
      by: "auto-rollback",
      ago: 3 * HOUR,
      runs: "9d1e0a7",
      sync: "Synced",
      health: "Healthy",
      auto_sync: true,
      pinned: { to: 18, revision: SHA["9d1e0a7"] ?? null, by: "auto-rollback", at_ms: (now() - 3 * HOUR) * 1000 },
    },
    pane: {
      about: { cluster: "in-cluster", namespace: "site", objects: 6, source: "deploy @ main", repo: "https://github.com/hyper-light/hyperlight-site" },
      groups: small("site", "site", 2),
      tree: { live: true, errors: [] },
      rollout: null,
      history: [
        deploy(20, "9d1e0a7", "Hyperlight site: the docs' search keeps its place", 3 * HOUR, { now: true, by: "auto-rollback" }),
        {
          ...deploy(19, "c2f81aa", "Hyperlight site: the landing page loads its fonts first", 3 * HOUR + 10 * MIN, { state: "failed", to: false }),
          revision: SHA.c2f81aa ?? null,
          subject: "failed its health check",
        },
        deploy(18, "9d1e0a7", "Hyperlight site: the docs' search keeps its place", 2 * DAY, { dim: true }),
      ],
      settings: settings(true),
      why_not: { stop: "no sync is running", ...NO_ROLLOUT, ...DECLARED },
    },
  },
];

/** What each browser did, by its user agent: each body it sent, and the settings it changed. */
interface Acts {
  bodies: unknown[];
  autoSync: Map<string, boolean>;
  /** quiver-staging's sync, moved on by `/api/mock/ship/deploys/tick`. */
  tick: number;
  /** The apps it deleted. */
  deleted: Set<string>;
}
const acts = new Map<string, Acts>();
function actsOf(ua: string): Acts {
  let a = acts.get(ua);
  if (!a) {
    a = { bodies: [], autoSync: new Map(), tick: 0, deleted: new Set() };
    acts.set(ua, a);
  }
  return a;
}

/** An app as this browser sees it: what it set, and how far quiver-staging's sync has come. */
function seen(a: App, mine: Acts): { row: ShipDeployRow; pane: Omit<ShipDeployPane, "app"> } {
  const auto = mine.autoSync.get(a.row.app);
  const row: ShipDeployRow = { ...a.row, at: now() - a.row.ago };
  let pane = a.pane;
  if (auto !== undefined) {
    row.auto_sync = auto;
    pane = {
      ...pane,
      settings: pane.settings.map((s) => (s.key === "auto_sync" ? { ...s, on: auto, from: "app", what: auto ? "syncs each time git changes what it renders" : "syncs when you say" } : s)),
    };
    // Out of sync with auto-sync on, kiln syncs it: it waits for that, no one else.
    if (row.word === "Out of sync") {
      row.stand = auto ? "waiting" : "needs_you";
      row.line = auto ? "#153 runs 15878aa · trunk is 2 ahead · auto-sync syncs it" : a.row.line;
    }
  }
  if (a.row.app === "default/quiver-staging" && mine.tick > 0) row.line = `#42 · wave ${Math.min(mine.tick + 1, 3)} of 3`;
  delete (row as Partial<App["row"]>).ago;
  return { row, pane };
}

/** Where an app sorts, as `kiln_cd::stands::rank` has it: what needs someone first, out of sync after what failed or went back. */
const RANK: Record<string, number> = { needs_you: 0, failing: 1, rolled_back: 2, shipping: 4, waiting: 5, shipped: 6 };
const rank = (r: ShipDeployRow) => (r.stand === "needs_you" && r.word === "Out of sync" ? 3 : (RANK[r.stand] ?? 7));

/** The list as the server answers it: by project, each project's apps and the projects most pressing first. */
function list(mine: Acts, q: string): ShipDeploys {
  const want = q.trim().toLowerCase();
  const rows = APPS.filter((a) => !mine.deleted.has(a.row.app))
    .map((a) => seen(a, mine).row)
    .filter((r) => !want || [r.project, r.app, r.environment ?? "", r.cluster, r.namespace].some((v) => v.toLowerCase().includes(want)));
  const by = new Map<string, ShipDeployRow[]>();
  for (const r of rows) by.set(r.project, [...(by.get(r.project) ?? []), r]);
  const projects: ShipDeployProject[] = [...by.entries()].map(([project, apps]) => {
    apps.sort((x, y) => rank(x) - rank(y) || x.name.localeCompare(y.name));
    return { project, apps, ...say(apps) };
  });
  projects.sort((x, y) => Math.min(...x.apps.map(rank)) - Math.min(...y.apps.map(rank)) || x.project.localeCompare(y.project));
  return { projects, total: rows.length, at_ms: Date.now() };
}

/** A project's heading, as `kiln_cd::stands::project_say` says it. */
function say(apps: ShipDeployRow[]): { say: string; needs: boolean } {
  const n = apps.length;
  const count = n === 1 ? "1 app" : `${n} apps`;
  const top = apps[0];
  if (!top) return { say: count, needs: false };
  const k = apps.filter((a) => a.stand === top.stand && a.word === top.word).length;
  const phrase = top.word === "Needs you" ? (k === 1 ? "waits on you" : "wait on you") : top.word.toLowerCase();
  const text = n === 1 ? `${count} · ${phrase}` : n === k ? `${count} · all ${phrase}` : `${count} · ${k} ${phrase}`;
  return { say: text, needs: ["needs_you", "failing", "rolled_back"].includes(top.stand) };
}

const OPS: Record<ShipDeployVerb, string> = {
  sync: "cd.apps.sync",
  refresh: "cd.apps.refresh",
  rollback: "cd.apps.rollback",
  stop: "cd.apps.terminate",
  follow: "cd.apps.follow",
  promote: "cd.apps.rollout_act",
  abort: "cd.apps.rollout_act",
  approve: "cd.apps.rollout_act",
  deny: "cd.apps.rollout_act",
  auto_sync: "cd.apps.set_auto_sync",
  auto_rollback: "cd.apps.set_auto_rollback",
  delete: "cd.apps.delete",
};

/** What a verb's operation answers, as CD's do. */
function done(app: App, verb: ShipDeployVerb, body: Record<string, unknown>): { status: number; body: unknown } {
  const id = app.row.app;
  const refused = app.pane.why_not[verb];
  if (refused) return { status: 409, body: { error: `${id}: ${refused}` } };
  // site-production's #19 failed its health check: CD won't hold it there.
  if (verb === "rollback" && body.revision === SHA.c2f81aa) return { status: 409, body: { error: `${id}: c2f81aa failed its health check; roll back to a deploy that held` } };
  switch (verb) {
    case "sync":
      return { status: 200, body: { syncing: id } };
    case "refresh":
      return { status: 200, body: { refreshing: id } };
    case "stop":
      return { status: 200, body: { terminating: id } };
    case "follow":
      return { status: 200, body: { following: id } };
    case "rollback":
      return { status: 200, body: { rolling_back: id, to: body.to ?? null, revision: body.revision ?? null } };
    case "auto_sync":
    case "auto_rollback":
      return { status: 200, body: { on: body.on, from: body.on === null ? "deploy-file" : "app" } };
    case "delete":
      return { status: 200, body: { deleted: id, cascade: body.cascade !== "orphan" } };
    default:
      return { status: 200, body: { app: id, rollout: app.pane.rollout?.id ?? null, action: verb } };
  }
}

/** A picked object's logs, events and manifest, and the app's diff, for these apps' objects. */
const LOG = [
  ["05:31:02", "INFO kiln server started version=0.2.0"],
  ["05:31:02", "INFO HTTP listening (hyper-rt) addr=0.0.0.0:7070"],
  ["05:31:03", "INFO QUIC listening (hyper-transport on hyper-rt, mTLS)"],
  ["05:31:04", "INFO agent connected agent=adacachy labels=[baremetal] slots=32"],
  ["05:31:09", "WARN store: a write waited 2.4 s on the disk (fsync)"],
  ["05:31:21", "WARN readiness: /readyz answered in 5.1 s"],
  ["05:31:40", "INFO conversation feed caught up store=01a18bdc… from=1847211"],
];
function logsOf(name: string) {
  const day = new Date().toISOString().slice(0, 10);
  return { pods: [name], more_pods: 0, container: "kiln", containers: ["kiln"], lines: LOG.map(([t, text]) => ({ pod: name, ts: `${day}T${t}Z`, text })), errors: [] };
}
function eventsOf() {
  const at = (m: number) => new Date(Date.now() - m * 60_000).toISOString();
  return {
    events: [
      { type: "Normal", reason: "Started", message: "Started container kiln", count: 1, first: at(3), last: at(3), source: "kubelet", object: { kind: "Pod", name: null } },
      {
        type: "Normal",
        reason: "Pulled",
        message: "Container image kiln:15878aaf0b already present on machine",
        count: 1,
        first: at(3),
        last: at(3),
        source: "kubelet",
        object: { kind: "Pod", name: null },
      },
      {
        type: "Warning",
        reason: "Unhealthy",
        message: "Liveness probe failed: GET /healthz: context deadline exceeded",
        count: 4,
        first: at(14),
        last: at(4),
        source: "kubelet",
        object: { kind: "Pod", name: null },
      },
      {
        type: "Normal",
        reason: "Killing",
        message: "Container kiln failed liveness probe, will be restarted",
        count: 4,
        first: at(14),
        last: at(4),
        source: "kubelet",
        object: { kind: "Pod", name: null },
      },
      {
        type: "Warning",
        reason: "Unhealthy",
        message: "Readiness probe failed: GET /readyz: context deadline exceeded",
        count: 591,
        first: at(240),
        last: at(15),
        source: "kubelet",
        object: { kind: "Pod", name: null },
      },
    ],
  };
}
function manifestOf(key: string) {
  const [group, kind, namespace, name] = key.split("~");
  const api = group ? `${group}/v1` : "v1";
  const live = [
    `apiVersion: ${api}`,
    `kind: ${kind}`,
    "metadata:",
    `  name: ${name}`,
    `  namespace: ${namespace}`,
    "  labels:",
    "    app.kubernetes.io/name: kiln",
    "    kiln.dev/app: 3f2a9c41d0b7e6a5",
  ].join("\n");
  return {
    key: { group: group ?? "", kind: kind ?? "", namespace: namespace ?? "", name: name ?? "" },
    managed: true,
    node: null,
    live,
    desired: live,
    diff: null,
    labels: {},
    annotations: {},
    conditions: [],
    finalizers: [],
    owners: [],
    errors: [],
  };
}
const CONFIG_DIFF = {
  key: { group: "", kind: "ConfigMap", namespace: "kiln", name: "kiln-config" },
  why: ".data differs",
  live: "",
  predicted: "",
  hunks: [{ old_start: 1, old_lines: 4, new_start: 1, new_lines: 4, lines: [" data:", "-  log_level: info", "+  log_level: debug", "   guard_seconds: 600", "-  headroom: v1", "+  headroom: v2"] }],
};
const DEPLOYMENT_DIFF = {
  key: { group: "apps", kind: "Deployment", namespace: "kiln", name: "kiln" },
  why: ".spec.template.spec.containers[0].image differs",
  live: "",
  predicted: "",
  hunks: [{ old_start: 21, old_lines: 1, new_start: 21, new_lines: 1, lines: ["-        image: kiln:15878aaf0b", "+        image: kiln:021a29f573"] }],
};
/** What syncing a commit of kiln's would do against what runs (#153), as `cd.plans.get` answers. */
function planOf(sha: string) {
  const short = sha.slice(0, 7);
  const step = (key: { group: string; kind: string; namespace: string; name: string }, why: string) => ({ key, step: "update", why, hook: false, added_lines: 1, removed_lines: 1 });
  return {
    repo: "https://github.com/hyper-light/kiln",
    trunk: { branch: "main", commit: SHA["021a29f"] },
    base: null,
    ok: true,
    apps: { affected: 1, planned: 1, unaffected: 0, unplanned: [] },
    notes: [],
    diffs: [
      {
        commit: { sha, subject: "Runner upgrades: progress never waits on the uplink", author: "user.email", at_ms: (now() - DAY) * 1000 },
        ok: true,
        apps: [
          {
            app: "default/kiln-production",
            cluster: "in-cluster",
            namespace: "kiln",
            auto: false,
            auto_prune: false,
            shadow: false,
            touched: true,
            ok: true,
            error: null,
            problems: [],
            notes: [],
            own: null,
            stack: null,
            sync: {
              create: 0,
              update: 2,
              prune: 0,
              hooks: 0,
              unchanged: 20,
              unread: 0,
              held: [],
              shared: [],
              steps: [step(DEPLOYMENT_DIFF.key, `image kiln:15878aa → kiln:${short}`), step(CONFIG_DIFF.key, "2 keys")],
            },
          },
        ],
      },
    ],
  };
}

/** Ship's Deploys' routes, and the CD routes its panes read for its apps. True when it answered. */
export async function handleDeploys(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: Ctx): Promise<boolean> {
  const ua = String(req.headers["user-agent"] ?? "");
  const p = url.pathname;
  const m = req.method ?? "GET";
  if (p === "/api/ship/deploys" && m === "GET") {
    ctx.json(res, 200, list(actsOf(ua), url.searchParams.get("q") ?? ""));
    return true;
  }
  const one = m === "GET" ? p.match(/^\/api\/ship\/deploys\/([^/]+)\/([^/]+)$/) : null;
  if (one) {
    const id = `${decodeURIComponent(one[1] ?? "")}/${decodeURIComponent(one[2] ?? "")}`;
    const a = APPS.find((x) => x.row.app === id && !actsOf(ua).deleted.has(id));
    if (!a) ctx.json(res, 404, { error: `no app ${id}` });
    else {
      const s = seen(a, actsOf(ua));
      ctx.json(res, 200, { app: s.row, ...s.pane } satisfies ShipDeployPane);
    }
    return true;
  }
  if (p === "/api/ship/deploys" && m === "POST") {
    let body: Record<string, unknown> = {};
    try {
      body = JSON.parse((await readBody(req)) || "{}") as Record<string, unknown>;
    } catch {
      body = {};
    }
    const verb = body.verb as ShipDeployVerb;
    const a = APPS.find((x) => x.row.app === body.app && !actsOf(ua).deleted.has(x.row.app));
    if (!a || !(verb in OPS)) {
      ctx.json(res, 400, { error: "`app` and `verb`: the app, and what to do to it" });
      return true;
    }
    const mine = actsOf(ua);
    mine.bodies.push(body);
    const r = done(a, verb, body);
    if (r.status === 200 && verb === "auto_sync") {
      if (body.on === null) mine.autoSync.delete(a.row.app);
      else mine.autoSync.set(a.row.app, body.on === true);
    }
    if (r.status === 200 && verb === "delete") {
      mine.deleted.add(a.row.app);
      emit({ type: "cd_app_deleted", app: a.row.app });
    }
    ctx.json(res, r.status, r.status === 200 ? { app: a.row.app, verb, op: OPS[verb], done: r.body } : r.body);
    return true;
  }
  // What this browser sent the strip's endpoint, for the e2e to read; `reset` starts it over.
  if (p === "/api/mock/ship/deploys") {
    if (url.searchParams.get("reset")) acts.delete(ua);
    ctx.json(res, 200, { bodies: acts.get(ua)?.bodies ?? [] });
    return true;
  }
  // quiver-staging's sync moves on a wave, and CD says so: the pages ask again.
  if (p === "/api/mock/ship/deploys/tick" && m === "POST") {
    const mine = actsOf(ua);
    mine.tick += 1;
    emit({ type: "cd_apps", apps: [] });
    emit({ type: "cd_rollout", app: "default/quiver-production", id: "in-cluster|apps/Rollout/quiver/quiver" });
    ctx.json(res, 200, { tick: mine.tick });
    return true;
  }
  // A commit's plan, for kiln's: what syncing it would do against what runs.
  const commit = url.searchParams.get("commit");
  if (p === "/api/cd/plans" && m === "GET" && url.searchParams.get("repo") === "https://github.com/hyper-light/kiln" && commit) {
    ctx.json(res, 200, planOf(commit));
    return true;
  }
  const cd = p.match(/^\/api\/cd\/apps\/([^/]+)\/([^/]+)(\/.*)?$/);
  const app = cd ? APPS.find((x) => x.row.app === `${decodeURIComponent(cd[1] ?? "")}/${decodeURIComponent(cd[2] ?? "")}`) : undefined;
  if (!cd || !app || m !== "GET") return false;
  const rest = cd[3] ?? "";
  if (rest === "/diff") {
    ctx.json(res, 200, { diffs: app.row.app === "default/kiln-production" ? [DEPLOYMENT_DIFF, CONFIG_DIFF] : [] });
    return true;
  }
  const r = rest.match(/^\/resources\/([^/]+)(\/events|\/logs)?$/);
  if (!r) return false;
  const key = decodeURIComponent(r[1] ?? "");
  if (r[2] === "/logs") ctx.json(res, 200, logsOf(key.split("~")[3] ?? "pod"));
  else if (r[2] === "/events") ctx.json(res, 200, eventsOf());
  else ctx.json(res, 200, manifestOf(key));
  return true;
}

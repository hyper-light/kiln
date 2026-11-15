/**
 * Runners' engines and the sandboxes their sessions run in (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d,
 * P2.3, P3.2), every harness session of the mock's own given one, so each row and each agent page has
 * its sandbox:
 *
 *   GET  /api/microvm/runners/:name               its microVMs, its warm pools, the images built there, its Hex node's room
 *                                                 (a proposal: a runner's engine is the server's GET /api/runners/:name/engine, engine.ts)
 *   GET  /api/microvm/sessions                    every session's sandbox, in short: engine, runner, start, state, CPU and memory now
 *   GET  /api/microvm/sessions/:id                one session's sandbox: its Hex box, its Shards microVM, grants, volumes, secret names, pool, telemetry
 *   POST /api/microvm/sessions/:id/:act           pause | resume | restart | stop its microVM
 *   POST/DELETE /api/microvm/sessions/:id/ingress {host, guest, proto, to}: publish a port, or unpublish it (microVMs only)
 *   POST/DELETE /api/microvm/sessions/:id/egress  {to, ports, dns, mcp, who}: grant a destination, or revoke it (microVMs only)
 *   PUT  /api/microvm/sessions/:id/networks/:net  {member, connected}: connect a member to a network or disconnect it (microVMs only)
 *   POST/DELETE /api/microvm/sessions/:id/volumes {path, mode, members}: add a volume, or remove it (every engine)
 *   POST /api/microvm/sessions/:id/components/:name {source, pin}: swap a harness, agent or MCP server at its path, the rest running on
 *   DELETE /api/microvm/sessions/:id/components/:name: put back what it was built with
 */
import type http from "node:http";
import { MAC_MINI } from "./data.ts";
import { sessionDef } from "./agentdefs.ts";

interface Ctx {
  json: (res: http.ServerResponse, status: number, body: unknown) => void;
  body: <T>(req: http.IncomingMessage) => Promise<T>;
}

const G = 2 ** 30;
/**
 * The Hex node's room on each runner whose agents run in microVMs: what its keeper says is free of its
 * memory and vCPUs. Not a route of the server's yet (a proposal, as `GET /api/microvm/runners/:name`
 * carries it): a runner's engine is `GET /api/runners/:name/engine` (engine.ts).
 */
const ROOMS: Record<string, { memory_free: number; memory_total: number; vcpus_free: number; vcpus_total: number }> = {
  [MAC_MINI]: { memory_free: 21 * G, memory_total: 32 * G, vcpus_free: 7, vcpus_total: 10 },
  "metal-ryzen-01": { memory_free: 41 * G, memory_total: 64 * G, vcpus_free: 7, vcpus_total: 16 },
  "metal-epyc-01": { memory_free: 388 * G, memory_total: 512 * G, vcpus_free: 52, vcpus_total: 64 },
  local: { memory_free: 30 * G, memory_total: 64 * G, vcpus_free: 9, vcpus_total: 24 },
};

type Kind = "warm" | "template" | "cold";
type VmState = "running" | "idle" | "paused" | "failed" | "draining";

/** What an image is: the Agentfile it was built from, at which version, its digest (linux/amd64; the Mac's are arm64). */
const IMAGES: Record<string, { version: number; digest: string; arm64: string; size: number }> = {
  "rust-dev": { version: 8, digest: "sha256:7c1e0b9a44d2", arm64: "sha256:5d2a1e9c07f3", size: 1.42 * G },
  "docs-site": { version: 3, digest: "sha256:91fe72b04c3d", arm64: "sha256:e04b7a1c9d25", size: 0.88 * G },
  "harness-claude": { version: 1, digest: "sha256:3a90d1c2fe07", arm64: "sha256:b1c4e8a20d6f", size: 0.61 * G },
  "harness-codex": { version: 1, digest: "sha256:6b2f90e1d4a8", arm64: "sha256:2c8e5f71a0b3", size: 0.58 * G },
  "harness-devin": { version: 1, digest: "sha256:0e7d3c5b9a12", arm64: "sha256:9a3d0c6e1f47", size: 0.66 * G },
};
/** Each harness's own hosts and secret, and each Agentfile's: what a session is granted, by name, nothing else. */
const HARNESS: Record<string, { egress: string[]; secrets: { name: string; scope: string }[] }> = {
  claude: { egress: ["api.anthropic.com:443"], secrets: [] },
  codex: { egress: ["api.openai.com:443"], secrets: [{ name: "OPENAI_API_KEY", scope: "user" }] },
  devin: { egress: ["api.devin.ai:443"], secrets: [{ name: "DEVIN_API_KEY", scope: "user" }] },
};
const AGENTFILE: Record<string, { egress: string[]; secrets: { name: string; scope: string }[]; volumes: { path: string; mode: "ro" | "rw" }[]; mcp: { name: string; to: string } | null }> = {
  "rust-dev": {
    egress: ["github.com:443", "index.crates.io:443", "static.crates.io:443"],
    secrets: [{ name: "GITHUB_TOKEN", scope: "repo" }],
    volumes: [{ path: "/root/.cargo/registry", mode: "rw" }],
    mcp: { name: "github", to: "api.githubcopilot.com:443" },
  },
  "docs-site": { egress: ["github.com:443", "registry.npmjs.org:443"], secrets: [{ name: "GITHUB_TOKEN", scope: "repo" }], volumes: [], mcp: null },
};

/** Who in a session can be granted a flow or a path: its harness, its agent, its MCP servers. */
type Member = { name: string; kind: "harness" | "agent" | "mcp" };
type Ingress = { host: string; guest: number; proto: "tcp" | "udp"; to: string };
type Egress = { to: string; ports: string; dns: boolean; mcp: string | null; who: string };
type Network = { name: string; members: (Member & { connected: boolean })[] };
type Volume = { path: string; mode: "ro" | "rw"; members: string[] };
/** Where a component comes from: Git over https or ssh, an S3 object, an https download. */
type From = { kind: "git-https" | "git-ssh" | "s3" | "https"; url: string; pin: string };
/** A harness, agent or MCP server installed at a path in the session; swapped in place, `built` is what it was built with. */
type Component = { kind: Member["kind"]; name: string; path: string; from: From; version: string; built: (From & { version: string }) | null };
/** A session's flows and paths, as granted at its start and changed since: everything else is denied. */
type Reach = { members: Member[]; ingress: Ingress[]; egress: Egress[]; networks: Network[]; volumes: Volume[]; components: Component[] };

/** What each harness, agent and MCP server is installed from, as its Agentfile names it (agentfiles.ts's rust-dev). */
const FROM: Record<string, From & { version: string }> = {
  claude: {
    kind: "https",
    url: "https://downloads.claude.ai/claude-code/2.1.311/claude-linux.tar.gz",
    pin: "sha256:2b7d4f0e9c1a8e3f5d6b0a7c9e2f4d1b8a6c3e5f7d9b1a0c2e4f6a8b0d2c4e6f",
    version: "2.1.311",
  },
  codex: {
    kind: "https",
    url: "https://github.com/openai/codex/releases/download/rust-v0.68.0/codex-linux.tar.gz",
    pin: "sha256:9f2c55d0b7a1e4c8f3d6a9b2e5c8f1a4d7b0e3c6f9a2d5b8e1c4f7a0d3b6e9c2",
    version: "0.68.0",
  },
  devin: { kind: "https", url: "https://downloads.devin.ai/cli/1.4.2/devin-linux.tar.gz", pin: "sha256:5c0e8a2f4b6d1e3a7c9f0b2d4e6a8c1f3b5d7e9a0c2f4b6d8e1a3c5f7b9d0e2a", version: "1.4.2" },
  reviewer: { kind: "git-ssh", url: "git@github.com:hyper-light/agents.git", pin: "v4 → 3f9c2e1d7a0b", version: "v4" },
  "pr-triage": { kind: "s3", url: "s3://kiln-agents/pr-triage/v2.tar.gz", pin: "version 3HL4kqtJlcpXroDTDmJ.rmSpXd3dIbrHY", version: "v2" },
  "release-notes": { kind: "git-https", url: "https://github.com/hyper-light/notes-agent.git", pin: "main → 8e21c4b0f9d3", version: "main" },
  "ci-diagnose": { kind: "git-https", url: "https://github.com/hyper-light/kiln.git", pin: "v0.68.0 → 1d7f3a9c2e40", version: "0.68.0" },
  github: { kind: "git-https", url: "https://github.com/github/github-mcp-server.git", pin: "v0.9.1 → a41c0e2b7d58", version: "0.9.1" },
};
/** A pin's version in short: a release's numbers, a ref, an object's tag. */
const versionOf = (f: From) => /(\d+\.\d+\.\d+)/.exec(f.url)?.[1] ?? /(\d+\.\d+\.\d+)/.exec(f.pin)?.[1] ?? f.pin.split(/\s|→/)[0]!.replace(/^version$/, "object");
const REACH = new Map<string, Reach>();

/** 60 readings, the last hour, each a step from the one before (0–100). */
const series = (base: number, spread: number, seed: number) => {
  let s = seed;
  let v = base;
  return Array.from({ length: 60 }, (_, i) => {
    s = (s * 16807) % 2147483647;
    v = i === 59 ? base : Math.max(0, Math.min(100, v + (s / 2147483647 - 0.5) * spread));
    return Math.round(v);
  });
};
const hex6 = (id: string) => {
  let h = 2166136261;
  for (const c of id) h = Math.imul(h ^ c.charCodeAt(0), 16777619);
  return (h >>> 0).toString(16).padStart(8, "0").slice(0, 6);
};

/** A microVM: the harness session it holds (the mock's own, so it opens), what it boots, how it started and what it uses now. */
interface Vm {
  id: string;
  session: string;
  /** The definition it started from: one of yours, or kiln's own for its harness. */
  agent: string;
  harness: string;
  /** `rust-dev v8`: its Agentfile and that Agentfile's version. */
  image: string;
  agentfile: string;
  version: number;
  digest: string;
  vcpus: number;
  start: { kind: Kind; ms: number };
  up_s: number;
  state: VmState;
  cpu: number[];
  /** Memory used, % of its own, the last hour. */
  mem: number[];
  /** Network, MB/s × 10, the last hour. */
  net: number[];
  memory: { used: number; total: number };
  disk: { used: number; total: number };
  footprint: { peak: number; steady: number };
  box: string;
}

// session · runner · harness · start kind · start ms · up (s) · state · CPU % now · memory used (GiB), one a row.
const AT: [string, string, string, Kind, number, number, VmState, number, number][] = [
  ["demo-claude", "local", "claude", "template", 598, 7200, "idle", 2, 0.9],
  ["demo-long", "local", "claude", "template", 640, 3600, "running", 41, 2.6],
  ["fam-orch", "local", "claude", "warm", 214, 5400, "running", 38, 2.1],
  ["fan-lead", "local", "claude", "warm", 219, 4800, "idle", 3, 2.9],
  ["fan-sign", "local", "claude", "cold", 1884, 900, "running", 22, 1.4],
  ["ask-lead", "local", "claude", "template", 612, 3000, "idle", 1, 1.1],
  ["ask-s2", "local", "claude", "template", 605, 1500, "idle", 1, 1.2],
  ["ask-perm", "local", "claude", "template", 618, 2800, "idle", 1, 0.8],
  ["ask-perm-s1", "local", "claude", "warm", 226, 1200, "idle", 2, 1.0],
  ["ask-own", "local", "claude", "template", 601, 600, "idle", 1, 0.9],
  ["relay-lead", "local", "claude", "template", 622, 6400, "idle", 1, 1.3],
  ["tree-lead", "local", "claude", "template", 609, 8200, "idle", 2, 1.5],
  ["rev-cicd", "local", "devin", "cold", 2104, 9000, "idle", 1, 1.7],
  ["demo-codex", "metal-ryzen-01", "codex", "template", 640, 7200, "idle", 2, 1.0],
  ["fam-tests", "metal-ryzen-01", "codex", "warm", 205, 180, "running", 44, 2.8],
  ["fan-s2", "metal-ryzen-01", "claude", "template", 431, 1100, "running", 31, 2.3],
  ["sub-busy", "metal-ryzen-01", "claude", "warm", 198, 2460, "idle", 4, 1.1],
  ["tree-s1", "metal-ryzen-01", "claude", "template", 615, 2200, "running", 36, 1.9],
  ["att-ci", "metal-ryzen-01", "claude", "cold", 1912, 840, "running", 62, 3.4],
  ["fam-migr", "metal-epyc-01", "claude", "warm", 212, 360, "idle", 8, 2.3],
  ["fan-s3", "metal-epyc-01", "codex", "template", 455, 1080, "paused", 0, 1.9],
  ["run-bench", "metal-epyc-01", "codex", "cold", 1930, 2600, "running", 88, 3.1],
  ["sub-codex", "metal-epyc-01", "codex", "warm", 207, 5200, "idle", 1, 1.2],
  ["tree-s2", "metal-epyc-01", "codex", "template", 652, 2200, "running", 47, 1.8],
  ["rev-big", "metal-epyc-01", "claude", "warm", 221, 4100, "idle", 2, 2.0],
];
/** Sessions on the host, and why: the runner's engine, or a project that asks for the host there. */
const ON_HOST: [string, string, string, number, number, string | null][] = [
  // session · runner · harness · CPU % now · memory used (GiB) · the project that asked for the host
  ["fam-docs", "metal-ryzen-02", "claude", 3, 0.7, null],
  ["fan-s4", "metal-ryzen-02", "claude", 29, 1.6, null],
  ["sub-done", "metal-ryzen-02", "claude", 1, 0.5, null],
  ["tree-s3", "metal-ryzen-02", "claude", 33, 1.8, null],
  ["tree-s3a", "metal-ryzen-02", "claude", 2, 0.6, null],
  ["run-freeze", "metal-ryzen-01", "claude", 1, 0.6, "hyper-light/kiln"],
  ["rev-mode", "adas-macbook-pro-2.local", "claude", 6, 0.9, null],
];
/** Ended: its microVM stopped with it. */
const ENDED: Record<string, { runner: string; harness: string; kind: Kind; ms: number }> = { "tree-s5": { runner: "metal-ryzen-01", harness: "claude", kind: "template", ms: 620 } };

function makeVm([session, runner, harness, kind, ms, up_s, state, cpu, used]: (typeof AT)[number], i: number): Vm {
  const def = sessionDef(session);
  const agent = def?.agent ?? harness;
  const agentfile = def?.agentfile ?? `harness-${harness}`;
  const memory = def?.memory ?? (harness === "devin" ? 8 * G : 4 * G);
  const img = IMAGES[agentfile] ?? IMAGES["harness-claude"]!;
  const seed = 41 + i * 6;
  const pct = Math.round(((used * G) / memory) * 100);
  return {
    id: `vm-${hex6(session)}`,
    session,
    agent,
    harness,
    image: `${agentfile} v${img.version}`,
    agentfile,
    version: img.version,
    digest: runner === MAC_MINI ? img.arm64 : img.digest,
    vcpus: def?.cpus ?? (harness === "devin" ? 4 : 2),
    start: { kind, ms },
    up_s,
    state,
    cpu: series(cpu, cpu / 2 + 4, seed),
    mem: series(pct, 6, seed + 1),
    net: series(state === "running" ? 12 : 1, state === "running" ? 10 : 1, seed + 2),
    memory: { used: used * G, total: memory },
    disk: { used: (0.4 + (i % 5) * 0.45) * G, total: 8 * G },
    footprint: { peak: used * 1.18 * G, steady: used * 0.86 * G },
    box: `box-${hex6(`${session}/box`)}`,
  };
}

type Pool = { agent: string; image: string; cpus: number; memory: number; warm: number; bound: number; hits: number; starts: number; idle_s: number; leaving_s: number | null };
type Image = { name: string; from: string; digest: string; size: number; built_s: number; state: "built" | "building" | "failed"; why?: string };
type Runner = { vms: Vm[]; pools: Pool[]; images: Image[]; starts: { warm: number; template: number; cold: number }; net: number[] };

const vms = AT.map(makeVm);
const on = (runner: string) => vms.filter((v) => AT.find((a) => a[0] === v.session)?.[1] === runner);
const RUNNERS: Record<string, Runner> = {
  "metal-ryzen-01": {
    vms: on("metal-ryzen-01"),
    pools: [
      { agent: "reviewer", image: "rust-dev v8", cpus: 2, memory: 4 * G, warm: 2, bound: 3, hits: 41, starts: 47, idle_s: 40, leaving_s: null },
      { agent: "ci-diagnose", image: "harness-claude v1", cpus: 2, memory: 4 * G, warm: 1, bound: 1, hits: 9, starts: 12, idle_s: 120, leaving_s: null },
      { agent: "pr-triage", image: "rust-dev v8", cpus: 1, memory: 2 * G, warm: 0, bound: 1, hits: 0, starts: 1, idle_s: 1860, leaving_s: 420 },
    ],
    images: [
      { name: "rust-dev", from: "rust-dev v8", digest: "sha256:7c1e0b9a44d2", size: 1.42 * G, built_s: 240, state: "built" },
      { name: "rust-dev", from: "rust-dev v9", digest: "", size: 0, built_s: 0, state: "building" },
      { name: "harness-claude", from: "kiln's", digest: "sha256:3a90d1c2fe07", size: 0.61 * G, built_s: 172800, state: "built" },
      { name: "harness-codex", from: "kiln's", digest: "sha256:6b2f90e1d4a8", size: 0.58 * G, built_s: 172800, state: "built" },
      { name: "docs-site", from: "docs-site v3", digest: "sha256:91fe72b04c3d", size: 0.88 * G, built_s: 86400, state: "built" },
    ],
    starts: { warm: 64, template: 12, cold: 6 },
    net: series(30, 18, 59),
  },
  "metal-epyc-01": {
    vms: on("metal-epyc-01"),
    pools: [{ agent: "reviewer", image: "rust-dev v8", cpus: 4, memory: 8 * G, warm: 1, bound: 2, hits: 12, starts: 15, idle_s: 300, leaving_s: null }],
    images: [
      { name: "rust-dev", from: "rust-dev v8", digest: "sha256:7c1e0b9a44d2", size: 1.42 * G, built_s: 360, state: "built" },
      {
        name: "rust-dev",
        from: "rust-dev v9",
        digest: "",
        size: 0,
        built_s: 0,
        state: "failed",
        why: "Shards refused the build at line 13 (HARNESS codex): the download for linux/amd64 has SHA-256 4b07e1a9c3d5…, and --checksum=sha256:9f2c55d0b7a1… names another.",
      },
      { name: "harness-claude", from: "kiln's", digest: "sha256:3a90d1c2fe07", size: 0.61 * G, built_s: 172800, state: "built" },
      { name: "harness-codex", from: "kiln's", digest: "sha256:6b2f90e1d4a8", size: 0.58 * G, built_s: 172800, state: "built" },
    ],
    starts: { warm: 22, template: 6, cold: 3 },
    net: series(22, 14, 73),
  },
  [MAC_MINI]: {
    vms: [],
    pools: [{ agent: "reviewer", image: "rust-dev v8", cpus: 2, memory: 4 * G, warm: 1, bound: 2, hits: 7, starts: 9, idle_s: 60, leaving_s: null }],
    images: [
      { name: "rust-dev", from: "rust-dev v8", digest: "sha256:5d2a1e9c07f3", size: 1.39 * G, built_s: 300, state: "built" },
      { name: "harness-claude", from: "kiln's", digest: "sha256:b1c4e8a20d6f", size: 0.6 * G, built_s: 172800, state: "built" },
    ],
    starts: { warm: 7, template: 2, cold: 1 },
    net: series(8, 6, 79),
  },
  local: {
    vms: on("local"),
    pools: [{ agent: "reviewer", image: "rust-dev v8", cpus: 2, memory: 4 * G, warm: 1, bound: 2, hits: 18, starts: 21, idle_s: 90, leaving_s: null }],
    images: [
      { name: "rust-dev", from: "rust-dev v8", digest: "sha256:7c1e0b9a44d2", size: 1.42 * G, built_s: 600, state: "built" },
      { name: "harness-claude", from: "kiln's", digest: "sha256:3a90d1c2fe07", size: 0.61 * G, built_s: 172800, state: "built" },
      { name: "harness-devin", from: "kiln's", digest: "sha256:0e7d3c5b9a12", size: 0.66 * G, built_s: 172800, state: "built" },
    ],
    starts: { warm: 31, template: 44, cold: 5 },
    net: series(16, 10, 83),
  },
};

const runnerOf = (session: string) => Object.entries(RUNNERS).find(([, r]) => r.vms.some((v) => v.session === session));

/** Readings move as a real VM's do: each read at least 2 s after the last one steps every running microVM's hour on by a reading. */
let ticked = Date.now();
const step = (xs: number[], spread: number, lo = 0, hi = 100) => {
  const last = xs.at(-1) ?? 0;
  xs.push(Math.round(Math.max(lo, Math.min(hi, last + (Math.random() - 0.5) * spread))));
  xs.shift();
};
function tick() {
  if (Date.now() - ticked < 2000) return;
  ticked = Date.now();
  for (const r of Object.values(RUNNERS))
    for (const v of r.vms) {
      if (v.state !== "running" && v.state !== "idle") continue;
      step(v.cpu, v.state === "running" ? 14 : 2, v.state === "running" ? 8 : 0, v.state === "running" ? 97 : 9);
      step(v.mem, 3, 8, 96);
      step(v.net, v.state === "running" ? 8 : 1);
      v.memory.used = Math.round(((v.mem.at(-1) ?? 0) / 100) * v.memory.total);
    }
}

/** A session's sandbox in short, as a row shows it. */
function brief(session: string) {
  tick();
  const found = runnerOf(session);
  if (found) {
    const [runner, r] = found;
    const v = r.vms.find((x) => x.session === session)!;
    return { engine: "microvm" as const, runner, state: v.state, start: v.start, cpu_pct: v.state === "paused" ? 0 : (v.cpu.at(-1) ?? 0), memory: v.memory };
  }
  const h = ON_HOST.find((x) => x[0] === session);
  if (h) return { engine: "host" as const, runner: h[1], state: "running" as const, start: null, cpu_pct: h[3], memory: { used: h[4] * G, total: 64 * G } };
  const e = ENDED[session];
  if (e) return { engine: "microvm" as const, runner: e.runner, state: "ended" as const, start: { kind: e.kind, ms: e.ms }, cpu_pct: 0, memory: { used: 0, total: 4 * G } };
  // A session no row above names (a new one): on the host where kiln itself runs, until it says.
  return null;
}

/** The project a session works on: what its scope is made of, beside its volumes. */
const projectOf = (session: string) => (/^(tree|rev|sub|relay)-/.test(session) ? "hyper-light/shards" : "hyper-light/kiln");

/**
 * What a session may touch, by member (its allowed directories), and its scope (its project, its
 * branch and the paths in it): both made from its volumes, so a volume added shows in each.
 */
function reachWords(session: string, reach: Reach) {
  const dirs = reach.members
    .filter((m) => m.kind !== "mcp")
    .map((m) => ({ member: m.name, kind: m.kind, paths: reach.volumes.filter((v) => v.members.includes(m.name)).map((v) => ({ path: v.path, mode: v.mode })) }));
  const scope = { project: projectOf(session), branch: `kiln/${session}`, paths: reach.volumes.filter((v) => v.members.length > 0).map((v) => v.path) };
  return { members: reach.members, volumes: reach.volumes, dirs, scope, components: reach.components };
}

/**
 * One session's sandbox in full. On a microVM: its Hex box, its Shards image and footprint, how it
 * started (its warm pool), its network (ports published, egress granted, networks and who is on
 * each) and the secret names its image was built with. On the host: the harness process it runs as,
 * and none of those. Both: what it uses now and over the last hour, its volumes, allowed directories
 * and scope.
 */
function detail(session: string) {
  const b = brief(session);
  if (!b) return null;
  const found = runnerOf(session);
  if (found) {
    const [runner, r] = found;
    const v = r.vms.find((x) => x.session === session)!;
    const af = AGENTFILE[v.agentfile];
    const h = HARNESS[v.harness] ?? HARNESS.claude!;
    const reach = reachOf(session, v.harness, v.agent, v.agentfile, false);
    const pool = v.start.kind === "warm" ? (r.pools.find((p) => p.agent === v.agent && p.cpus === v.vcpus) ?? r.pools.find((p) => p.agent === v.agent) ?? null) : null;
    return {
      ...b,
      why: null,
      up_s: v.up_s,
      process: null,
      hex: { box: v.box, node: runner, state: v.state },
      vm: { id: v.id, agent: v.agent, image: v.agentfile, version: v.version, digest: v.digest, vcpus: v.vcpus, memory: v.memory.total, footprint: v.footprint },
      now: { cpu_pct: b.cpu_pct, memory: v.memory, disk: v.disk },
      series: { cpu: v.cpu, memory: v.mem, net: v.net },
      network: { ingress: reach.ingress, egress: reach.egress, networks: reach.networks },
      ...reachWords(session, reach),
      secrets: [...h.secrets, ...(af?.secrets ?? [])],
      pool: pool ? { agent: pool.agent, image: pool.image, cpus: pool.cpus, memory: pool.memory, warm: pool.warm, bound: pool.bound } : null,
    };
  }
  const h = ON_HOST.find((x) => x[0] === session);
  if (h) {
    const [, , harness, cpu, used, project] = h;
    const seed = 97 + session.length * 7;
    const reach = reachOf(session, harness, sessionDef(session)?.agent ?? harness, sessionDef(session)?.agentfile ?? `harness-${harness}`, true);
    return {
      ...b,
      why: project ? `${project} asks for the host here` : "its owner's choice",
      up_s: 3000,
      process: { harness, pid: 40000 + (parseInt(hex6(session), 16) % 20000), user: "kiln" },
      hex: null,
      vm: null,
      now: { cpu_pct: cpu, memory: b.memory, disk: { used: 0.9 * G, total: 2048 * G } },
      series: { cpu: series(cpu, cpu / 2 + 4, seed), memory: series(Math.round(((used * G) / (64 * G)) * 100), 1, seed + 1), net: series(6, 6, seed + 2) },
      network: null,
      ...reachWords(session, reach),
      secrets: null,
      pool: null,
    };
  }
  // Ended: its microVM stopped with it; nothing runs, nothing is held.
  return {
    ...b,
    why: null,
    up_s: 0,
    process: null,
    hex: null,
    vm: null,
    now: null,
    series: { cpu: [], memory: [], net: [] },
    network: null,
    members: [],
    volumes: [],
    dirs: [],
    scope: null,
    components: [],
    secrets: null,
    pool: null,
  };
}

/**
 * A session's reach, made once from its harness and Agentfile and changed by the routes below. On the
 * host there is no network of its own to grant (the host's is the host's), so only its volumes.
 */
function reachOf(session: string, harness: string, agent: string, agentfile: string, host: boolean): Reach {
  const had = REACH.get(session);
  if (had) return had;
  const af = AGENTFILE[agentfile];
  const h = HARNESS[harness] ?? HARNESS.claude!;
  const members: Member[] = [
    { name: harness, kind: "harness" },
    ...(agent !== harness ? [{ name: agent, kind: "agent" as const }] : []),
    ...(af?.mcp ? [{ name: af.mcp.name, kind: "mcp" as const }] : []),
  ];
  const egress: Egress[] = host
    ? []
    : [
        ...h.egress.map((d) => ({ to: d.split(":")[0]!, ports: d.split(":")[1]!, dns: true, mcp: null, who: harness })),
        ...(af?.egress ?? []).map((d) => ({ to: d.split(":")[0]!, ports: d.split(":")[1]!, dns: true, mcp: null, who: agent })),
        ...(af?.mcp ? [{ to: af.mcp.to.split(":")[0]!, ports: af.mcp.to.split(":")[1]!, dns: true, mcp: af.mcp.name, who: af.mcp.name }] : []),
      ];
  const networks: Network[] = host
    ? []
    : [
        { name: `${harness}-out`, members: members.map((m) => ({ ...m, connected: m.kind === "harness" })) },
        ...(af ? [{ name: `${agentfile}-net`, members: members.map((m) => ({ ...m, connected: m.kind !== "harness" })) }] : []),
      ];
  const everyone = members.filter((m) => m.kind !== "mcp").map((m) => m.name);
  const volumes: Volume[] = host
    ? [{ path: `/srv/kiln/work/${session}`, mode: "rw", members: everyone }]
    : [
        { path: "/work", mode: "rw", members: everyone },
        { path: `/home/${harness}`, mode: "rw", members: [harness] },
        ...(af?.volumes ?? []).map((v) => ({ ...v, members: everyone })),
        { path: "/opt/kiln/skills", mode: "ro", members: everyone },
      ];
  // A session that serves something of its own: fam-orch's preview, published on its runner's loopback.
  const ingress: Ingress[] = host ? [] : session === "fam-orch" ? [{ host: "127.0.0.1:4173", guest: 4173, proto: "tcp", to: agent }] : [];
  // Each member at its own path: in the microVM under /opt/kiln, on the host inside this session's scope.
  const base = host ? `/srv/kiln/work/${session}/.kiln` : "/opt/kiln";
  const dir: Record<Member["kind"], string> = { harness: "harness", agent: "agents", mcp: "mcp" };
  const components: Component[] = members.flatMap((m) => {
    const f = FROM[m.name];
    return f ? [{ kind: m.kind, name: m.name, path: `${base}/${dir[m.kind]}/${m.name}`, from: { kind: f.kind, url: f.url, pin: f.pin }, version: f.version, built: null }] : [];
  });
  const reach = { members, ingress, egress, networks, volumes, components };
  REACH.set(session, reach);
  return reach;
}

/** The sessions on runner `name`, each in a microVM or not, running or not: its engine's view counts them (engine.ts). */
export function sessionsOn(name: string): { microvm: boolean; running: boolean }[] {
  const all = [...AT.map((a) => a[0]), ...ON_HOST.map((h) => h[0])];
  return all.flatMap((id) => {
    const b = brief(id);
    return b && b.runner === name ? [{ microvm: b.engine === "microvm", running: b.state !== "ended" }] : [];
  });
}

/** Handles the routes above; false when it doesn't know the route. */
export async function handleMicrovms(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: Ctx): Promise<boolean> {
  const p = url.pathname;
  const m = req.method ?? "GET";
  if (p === "/api/microvm/sessions" && m === "GET") {
    const all = [...AT.map((a) => a[0]), ...ON_HOST.map((h) => h[0]), ...Object.keys(ENDED)];
    return (ctx.json(res, 200, Object.fromEntries(all.flatMap((id) => (brief(id) ? [[id, brief(id)]] : [])))), true);
  }
  const ses = p.match(/^\/api\/microvm\/sessions\/([^/]+)$/);
  if (ses && m === "GET") {
    const d = detail(decodeURIComponent(ses[1]!));
    return (d ? ctx.json(res, 200, d) : ctx.json(res, 404, { error: "no sandbox for that session" }), true);
  }
  const swap = p.match(/^\/api\/microvm\/sessions\/([^/]+)\/components\/([^/]+)$/);
  if (swap && (m === "POST" || m === "DELETE")) {
    const id = decodeURIComponent(swap[1]!);
    const d = detail(id);
    const c = REACH.get(id)?.components.find((x) => x.name === decodeURIComponent(swap[2]!));
    if (!d || !c || d.state === "ended") return (ctx.json(res, 404, { error: "no such component in a running session" }), true);
    if (m === "DELETE") {
      if (c.built) {
        const { version, ...from } = c.built;
        c.from = from;
        c.version = version;
        c.built = null;
      }
      return (ctx.json(res, 200, detail(id)), true);
    }
    const b = await ctx.body<{ source?: string; pin?: string }>(req);
    const url = String(b.source ?? "").trim();
    const pin = String(b.pin ?? "").trim();
    const kind: From["kind"] | null = url.startsWith("s3://")
      ? "s3"
      : /^(git@|ssh:\/\/)/.test(url)
        ? "git-ssh"
        : /^https:\/\/[^#\s]+\.git$/.test(url)
          ? "git-https"
          : /^https:\/\/\S+$/.test(url)
            ? "https"
            : null;
    if (!kind) return (ctx.json(res, 400, { error: "a source is Git (https://….git or git@host:….git), s3://BUCKET/KEY or an https URL" }), true);
    const pinned = kind === "https" ? /^sha256:[0-9a-f]{64}$/.test(pin) : kind === "s3" ? /^sha256:[0-9a-f]{64}$/.test(pin) || /^version \S+$/.test(pin) : /^\S+$/.test(pin);
    if (!pinned)
      return (
        ctx.json(res, 400, {
          error:
            kind === "https"
              ? "an https source is pinned by sha256: and 64 hex digits"
              : kind === "s3"
                ? "an S3 source is pinned by sha256:… or `version <id>`"
                : "a Git source is pinned by a ref: a tag, branch or commit",
        }),
        true
      );
    const from: From = { kind, url, pin: kind.startsWith("git") ? `${pin} → ${hex6(url + pin)}${hex6(pin + url)}` : pin };
    c.built ??= { ...c.from, version: c.version };
    c.from = from;
    c.version = versionOf(from);
    return (ctx.json(res, 200, detail(id)), true);
  }
  const change = p.match(/^\/api\/microvm\/sessions\/([^/]+)\/(ingress|egress|volumes|networks\/([^/]+))$/);
  if (change && (m === "POST" || m === "DELETE" || m === "PUT")) {
    const id = decodeURIComponent(change[1]!);
    const d = detail(id);
    const reach = REACH.get(id);
    if (!d || !reach) return (ctx.json(res, 404, { error: "no sandbox for that session" }), true);
    const what = change[2]!;
    const microvm = d.engine === "microvm" && d.state !== "ended";
    if (what !== "volumes" && !microvm) return (ctx.json(res, 409, { error: "it runs on the host: its network is the host's, with nothing of its own to grant" }), true);
    const b = await ctx.body<Record<string, unknown>>(req);
    const known = (who: unknown) => typeof who === "string" && reach.members.some((x) => x.name === who);
    if (what === "ingress") {
      const i = { host: String(b.host ?? ""), guest: Number(b.guest), proto: b.proto === "udp" ? ("udp" as const) : ("tcp" as const), to: String(b.to ?? "") };
      if (m === "DELETE") reach.ingress = reach.ingress.filter((x) => !(x.host === i.host && x.guest === i.guest && x.proto === i.proto));
      else {
        if (!/^[0-9.]+:\d{1,5}$|^\[[0-9a-f:]+\]:\d{1,5}$/i.test(i.host) || !(i.guest > 0 && i.guest < 65536) || !known(i.to))
          return (ctx.json(res, 400, { error: "a port is published as ADDRESS:PORT on the runner → a guest port, to one of its members" }), true);
        if (reach.ingress.some((x) => x.host === i.host && x.proto === i.proto)) return (ctx.json(res, 409, { error: `${i.host}/${i.proto} is published already` }), true);
        reach.ingress.push(i);
      }
    } else if (what === "egress") {
      const e = { to: String(b.to ?? "").toLowerCase(), ports: String(b.ports ?? ""), dns: b.dns !== false, mcp: typeof b.mcp === "string" && b.mcp ? b.mcp : null, who: String(b.who ?? "") };
      if (m === "DELETE") reach.egress = reach.egress.filter((x) => !(x.to === e.to && x.ports === e.ports && x.who === e.who));
      else {
        if (!/^[a-z0-9.-]+$/.test(e.to) || !/^\d{1,5}(-\d{1,5})?(,\d{1,5}(-\d{1,5})?)*$/.test(e.ports) || !known(e.who))
          return (ctx.json(res, 400, { error: "a grant names a host, its ports (443, or 8000-8010) and who it is for" }), true);
        reach.egress.push(e);
      }
    } else if (what === "volumes") {
      const v = { path: String(b.path ?? ""), mode: b.mode === "ro" ? ("ro" as const) : ("rw" as const), members: Array.isArray(b.members) ? b.members.filter(known).map(String) : [] };
      if (m === "DELETE") reach.volumes = reach.volumes.filter((x) => x.path !== v.path);
      else {
        if (!v.path.startsWith("/") || v.members.length === 0) return (ctx.json(res, 400, { error: "a volume is an absolute path, given to at least one member" }), true);
        if (reach.volumes.some((x) => x.path === v.path)) return (ctx.json(res, 409, { error: `${v.path} is a volume already` }), true);
        reach.volumes.push(v);
      }
    } else {
      const net = reach.networks.find((n) => n.name === decodeURIComponent(change[3] ?? ""));
      const member = net?.members.find((x) => x.name === b.member);
      if (!net || !member) return (ctx.json(res, 404, { error: "no such network or member" }), true);
      member.connected = b.connected === true;
    }
    return (ctx.json(res, 200, detail(id)), true);
  }
  const act = p.match(/^\/api\/microvm\/sessions\/([^/]+)\/(pause|resume|restart|stop)$/);
  if (act && m === "POST") {
    const id = decodeURIComponent(act[1]!);
    const found = runnerOf(id);
    if (!found) return (ctx.json(res, 409, { error: "it runs on the host: there is no microVM to control" }), true);
    const [runner, r] = found;
    const v = r.vms.find((x) => x.session === id)!;
    if (act[2] === "pause") v.state = "paused";
    else if (act[2] === "resume") v.state = "running";
    else if (act[2] === "restart") {
      const warm = r.pools.find((x) => x.agent === v.agent && x.warm > 0);
      if (warm) warm.warm -= 1;
      v.state = "running";
      v.up_s = 0;
      v.start = warm ? { kind: "warm", ms: 209 } : { kind: "template", ms: 447 };
    } else {
      r.vms = r.vms.filter((x) => x !== v);
      ENDED[id] = { runner, harness: v.harness, kind: v.start.kind, ms: v.start.ms };
    }
    return (ctx.json(res, 200, detail(id)), true);
  }
  const one = p.match(/^\/api\/microvm\/runners\/([^/]+)$/);
  if (one && m === "GET") {
    const name = decodeURIComponent(one[1]!);
    const r = RUNNERS[name] ?? { vms: [], pools: [], images: [], starts: { warm: 0, template: 0, cold: 0 }, net: [] };
    return (ctx.json(res, 200, { ...r, room: ROOMS[name] ?? null, vms: r.vms.map(({ footprint: _f, box: _b, ...v }) => v) }), true);
  }
  return false;
}

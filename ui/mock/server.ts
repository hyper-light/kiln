/**
 * kiln mock server — implements docs/api.md with a live simulation so the UI can be
 * developed without the Rust server.
 *
 *   npm run mock            # http://127.0.0.1:7070, token printed on start
 *   MOCK_NO_AUTH=1 npm run mock
 *
 * If ../dist/client exists (the built UI) it is served at /, with the app's shell for every page,
 * as kiln serves it when its UI server isn't running.
 */
import http from "node:http";
import { randomBytes } from "node:crypto";
import { existsSync, readFileSync, statSync } from "node:fs";
import { extname, join, normalize, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import os from "node:os";
import { WebSocketServer, WebSocket } from "ws";
import type { Agent, AuditEntry, Child, HostSample, Job, JobTools, PlannedJob, Repo, Run, ServerEvent, Status, Step } from "../src/api/types.ts";
import type { DiscoveredInstance, DiscoveryInfo, DiscoveryMethod, DiscoveryProblem, DiscoveryVia, JoinInfo, JoinRequest, ScanState } from "../src/api/types.ts";
import { defaultRunner } from "./defaults.ts";
import { AUTHORS, REPOS, commitMessage, hashStr, mockDirList, mulberry32, pick, pipelineFor, sha, stepLog, type JobTpl, type LogPlan, type Toolchain } from "./data.ts";
import { handleAccounts, handleDemoSession, setDemoEmit } from "./accounts.ts";
import { handleFamily, handleFamilySession, setFamilyEmit } from "./agents.ts";
import { handleGoals } from "./goals.ts";
import { handlePrograms } from "./programs.ts";
import { handlePages } from "./pages.ts";
import { handleSettings } from "./settings.ts";
import { handleMachines } from "./machines.ts";
import { handleKeep } from "./keep.ts";
import { handleReap } from "./reap.ts";
import { handlePlaces } from "./places.ts";
import { handleLongFiles, handleLongSession, handleShownPages, longInfo, searchSaid, sendAway, setLongEmit } from "./conversation.ts";
import { fastInfos, handleFastSession } from "./fast.ts";
import { handlePush, pushHeard } from "./push.ts";
import { handleReview } from "./review.ts";
import { handleShip } from "./ship.ts";
import { handleDoctor } from "./doctor.ts";
import { handleStores } from "./stores.ts";
import {
  AUTOJOIN,
  DNS_SD_DOMAINS,
  INSTANCE_SEEDS,
  PROBE_FIND,
  SCAN_FIND,
  SERVER_ADDR,
  SERVER_CA,
  SERVER_INSTANCE_ID,
  SERVER_URL,
  SILENT_NETWORK_FIXES,
  SUBNETS,
  dnsSdRecords,
  mdnsInterfaces,
  type InstanceSeed,
  type MockOs,
} from "./data.ts";

const PORT = Number(process.env.PORT ?? 7070);
const HOST = process.env.HOST ?? "127.0.0.1";
const TOKEN = process.env.KILN_TOKEN ?? "kiln-dev-token";
const NO_AUTH = process.env.MOCK_NO_AUTH === "1";
const OTP = randomBytes(4).toString("hex");
const VERSION = "0.1.0";
const STARTED = Date.now();
const HOSTNAME = "kiln-01";
const __dirname = dirname(fileURLToPath(import.meta.url));
// MOCK_DIST: serve another build's client files (a scratch one, so builds in progress elsewhere don't clash).
const DIST = process.env.MOCK_DIST ?? join(__dirname, "..", "dist", "client");

const now = () => Math.floor(Date.now() / 1000);
const rid = (p: string) => `${p}${randomBytes(6).toString("hex")}`;
const rnd = Math.random;
const between = (a: number, b: number) => a + rnd() * (b - a);

/* ------------------------------------------------------------------ */
/* State                                                               */
/* ------------------------------------------------------------------ */

interface SimStep extends Step {
  dur: number;
  kind: string;
  run: string;
  plan?: LogPlan;
  emitted: number;
}
interface SimJob {
  job: Job;
  tpl: JobTpl;
  steps: SimStep[];
  failStep: number | null; // step index that will fail
  stepStartedMs: number | null;
  planned: PlannedJob;
  /** Queued, it waits for a machine until then (unix seconds): one machine builds kiln's platforms in turn. */
  holdUntil?: number;
}
interface LogStream {
  data: string;
  done: boolean;
  subs: Set<WebSocket>;
}

const repos: Repo[] = [];
const repoTc = new Map<string, Toolchain>();
const runs: Run[] = []; // newest first
const jobsByRun = new Map<string, SimJob[]>();
const jobsById = new Map<string, SimJob>();
/** A job for Ship's fixed rows to stream (ship.ts): one running, else the first. */
function liveJob() {
  const all = [...jobsById.values()];
  const sj = all.find((j) => j.job.status === "running") ?? all[0];
  return sj ? { run: sj.job.run_id, job: sj.job.id, name: sj.job.display || sj.job.key, state: sj.job.status } : null;
}
const logs = new Map<string, LogStream>();
const agents = new Map<string, Agent>();
const agentState = new Map<string, HostState>();
const history = new Map<string, HostSample[]>();
const audit: AuditEntry[] = [];
const runNumbers = new Map<string, number>();
const eventClients = new Set<WebSocket>();
const crashLog: string[] = [
  `2026-10-11T03:12:44Z generation 3 exited: signal SIGKILL (oom?) — restarted in 1.6s`,
  `2026-10-12T19:40:02Z generation 4 exited: code 101 — panicked at 'quic listener: address in use' — restarted in 3.2s`,
];
const generation = 5;

const children: Child[] = [
  { name: "http", state: "running", restarts: 0, last_error: null, since_unix: now() - 86_000 },
  { name: "gitops-poller", state: "running", restarts: 2, last_error: "GET api.github.com: 502 Bad Gateway", since_unix: now() - 4_100 },
  { name: "scheduler", state: "running", restarts: 0, last_error: null, since_unix: now() - 86_000 },
  { name: "quic-listener", state: "running", restarts: 0, last_error: null, since_unix: now() - 86_000 },
  { name: "k8s-launcher", state: "running", restarts: 1, last_error: "pods is forbidden: exceeded quota (transient)", since_unix: now() - 12_000 },
  { name: "log-store", state: "running", restarts: 0, last_error: null, since_unix: now() - 86_000 },
  { name: "metrics-sampler", state: "running", restarts: 0, last_error: null, since_unix: now() - 86_000 },
  { name: "terminal", state: "running", restarts: 0, last_error: null, since_unix: now() - 86_000 },
  { name: "raft", state: "finished", restarts: 0, last_error: null, since_unix: now() - 86_000 },
];

let auditSeq = 0;
function addAudit(actor: string, action: string, detail: string) {
  audit.unshift({ id: ++auditSeq, ts: now(), principal: actor, on_behalf_of: null, token: null, scope: null, action, target: null, result: "ok", ip: null, detail, actor });
  if (audit.length > 500) audit.length = 500;
}

/* ------------------------------------------------------------------ */
/* Hosts / samples                                                     */
/* ------------------------------------------------------------------ */

interface HostState {
  cpus: number;
  memTotal: number;
  memBase: number;
  diskTotal: number;
  diskUsed: number;
  cpu: number;
  rx: number;
  tx: number;
  load: number;
}

function newHost(cpus: number, memGiB: number, diskGiB: number): HostState {
  return {
    cpus,
    memTotal: memGiB * 1024 ** 3,
    memBase: memGiB * 1024 ** 3 * between(0.15, 0.35),
    diskTotal: diskGiB * 1024 ** 3,
    diskUsed: diskGiB * 1024 ** 3 * between(0.3, 0.7),
    cpu: between(3, 10),
    rx: Math.floor(between(1e9, 9e10)),
    tx: Math.floor(between(1e9, 5e10)),
    load: between(0.1, 1),
  };
}

function stepHost(h: HostState, busyFrac: number, tsMs: number): HostSample {
  const target = 4 + busyFrac * 78 + between(-4, 4);
  h.cpu = Math.max(0.5, Math.min(100, h.cpu + (target - h.cpu) * 0.35 + between(-3, 3)));
  h.load = Math.max(0.02, h.load + ((h.cpu / 100) * h.cpus - h.load) * 0.15);
  const memUsed = Math.min(h.memTotal * 0.97, h.memBase + busyFrac * h.memTotal * 0.45 + between(-1, 1) * h.memTotal * 0.01);
  h.diskUsed = Math.min(h.diskTotal * 0.96, h.diskUsed + busyFrac * between(0, 4e6));
  h.rx += Math.floor((between(20e3, 300e3) + busyFrac * between(1e6, 40e6)) * 5);
  h.tx += Math.floor((between(10e3, 200e3) + busyFrac * between(0.2e6, 8e6)) * 5);
  return {
    ts_ms: tsMs,
    cpu_pct: Math.round(h.cpu * 10) / 10,
    cpus: h.cpus,
    load1: Math.round(h.load * 100) / 100,
    mem_total: h.memTotal,
    mem_used: Math.floor(memUsed),
    disk_total: h.diskTotal,
    disk_used: Math.floor(h.diskUsed),
    net_rx: h.rx,
    net_tx: h.tx,
    procs: Math.floor(180 + busyFrac * 220 + between(0, 30)),
  };
}

function prefillHistory(name: string, h: HostState, busyish: number) {
  const out: HostSample[] = [];
  const t0 = Date.now() - 720 * 5000;
  let wave = rnd() * 10;
  for (let i = 0; i < 720; i++) {
    wave += 0.04;
    const busy = Math.max(0, Math.min(1, busyish * (0.5 + 0.5 * Math.sin(wave)) + between(-0.1, 0.1)));
    out.push(stepHost(h, busy, t0 + i * 5000));
  }
  history.set(name, out);
}

function pushSample(name: string, s: HostSample) {
  const arr = history.get(name) ?? [];
  arr.push(s);
  if (arr.length > 720) arr.splice(0, arr.length - 720);
  history.set(name, arr);
}

const serverHost = newHost(16, 64, 1800);
prefillHistory("server", serverHost, 0.25);

function addAgent(a: Omit<Agent, keyof ReturnType<typeof defaultRunner>> & Partial<ReturnType<typeof defaultRunner>>, host: HostState, prefill = true) {
  const agent: Agent = { ...defaultRunner(), ...a };
  agents.set(a.name, agent);
  agentState.set(a.name, host);
  if (prefill) {
    prefillHistory(a.name, host, a.connected ? 0.5 : 0);
    agent.sample = a.connected ? (history.get(a.name)?.at(-1) ?? null) : null;
  }
  return agent;
}

const t = now();
addAgent({ name: "metal-ryzen-01", labels: ["baremetal", "bench", "linux", "x86_64"], slots: 2, hostname: "ryzen-01.lan", os: "linux", arch: "x86_64", version: VERSION, ephemeral: false, connected: true, last_seen: t, enrolled_at: t - 86400 * 40 }, newHost(16, 64, 2000));
addAgent({ name: "metal-ryzen-02", labels: ["baremetal", "linux", "x86_64"], slots: 4, hostname: "ryzen-02.lan", os: "linux", arch: "x86_64", version: VERSION, ephemeral: false, connected: true, last_seen: t, enrolled_at: t - 86400 * 38 }, newHost(24, 128, 4000));
addAgent({ name: "metal-epyc-01", labels: ["baremetal", "bench", "gpu", "linux", "x86_64"], slots: 8, hostname: "epyc-01.rack2", os: "linux", arch: "x86_64", version: VERSION, ephemeral: false, connected: true, last_seen: t, enrolled_at: t - 86400 * 12 }, newHost(64, 512, 8000));
addAgent({ name: "metal-mac-mini", labels: ["baremetal", "macos", "arm64"], slots: 2, hostname: "mac-mini.lan", os: "macos", arch: "aarch64", version: VERSION, ephemeral: false, connected: true, last_seen: t, enrolled_at: t - 86400 * 9 }, newHost(10, 32, 1000));
addAgent({ name: "metal-pi-04", labels: ["baremetal", "arm64", "linux"], slots: 1, hostname: "pi-04.lan", os: "linux", arch: "aarch64", version: "0.0.9", ephemeral: false, connected: true, last_seen: t, enrolled_at: t - 86400 * 60, draining: true }, newHost(4, 8, 120));
addAgent({ name: "metal-nuc-old", labels: ["baremetal", "linux", "x86_64"], slots: 2, hostname: "nuc.lan", os: "linux", arch: "x86_64", version: "0.0.8", ephemeral: false, connected: false, last_seen: t - 86400 * 2 - 3600, enrolled_at: t - 86400 * 120 }, newHost(8, 16, 500));

/* ------------------------------------------------------------------ */
/* Repos & runs                                                        */
/* ------------------------------------------------------------------ */

for (const [i, r] of REPOS.entries()) {
  const id = `repo_${r.name.split("/")[1]!.replace(/[^a-z0-9]/g, "")}`;
  repos.push({
    id,
    name: r.name,
    clone_url: `https://github.com/${r.name}.git`,
    branches: r.branches,
    poll_seconds: 60,
    enabled: true,
    created_at: t - 86400 * (30 + i * 3),
    last_polled_at: t - Math.floor(between(2, 58)),
    last_error: r.name.endsWith("mantle") ? "GET /repos/hyper-light/mantle/commits: 403 rate limit exceeded (resets in 4m)" : null,
  });
  repoTc.set(id, r.toolchain);
  runNumbers.set(id, Math.floor(between(80, 900)));
}

function plannedFor(tpl: JobTpl): PlannedJob {
  return {
    key: tpl.key,
    spec_id: tpl.key.split(" ")[0]!,
    display: tpl.key,
    needs: tpl.needs,
    labels: tpl.labels,
    image: tpl.toolchain === "rust" ? "ghcr.io/hyper-light/kiln-rust:1.90" : tpl.toolchain === "node" ? "node:24-slim" : tpl.toolchain === "python" ? "ghcr.io/astral-sh/uv:python3.13" : null,
    toolchain: tpl.toolchain,
    env: { CARGO_TERM_COLOR: "always", RUST_BACKTRACE: "1" },
    timeout_minutes: tpl.labels.includes("bench") ? 120 : 60,
    retries: tpl.labels.includes("k8s") ? 1 : 0,
    exclusive: tpl.labels.includes("bench"),
    resources: { cpu: tpl.labels.includes("k8s") ? "4" : null, memory: tpl.labels.includes("k8s") ? "8Gi" : null },
    artifacts: tpl.key === "bench" ? ["target/criterion/**"] : [],
    cache: tpl.toolchain === "rust" ? ["~/.cargo/registry", "target"] : [],
    condition: null,
    requires: [],
    steps: tpl.steps.map((s) => ({ name: s.name, run: s.run, shell: null, env: {}, "working-directory": null, "timeout-minutes": null, "continue-on-error": false, tty: true })),
  };
}

function weightedRepo(r: () => number): Repo {
  const total = REPOS.reduce((s, x) => s + x.weight, 0);
  let x = r() * total;
  for (const [i, s] of REPOS.entries()) {
    x -= s.weight;
    if (x <= 0) return repos[i]!;
  }
  return repos[0]!;
}

function makeRun(repo: Repo, opts: { created: number; branch?: string; event?: string; pipeline?: string; r?: () => number }): { run: Run; tpls: JobTpl[] } {
  const r = opts.r ?? rnd;
  const tc = repoTc.get(repo.id) ?? "rust";
  const concrete = repo.branches.filter((b) => !b.includes("*"));
  const prBranch = pick(r, ["feat/zero-copy", "fix/election-race", "perf/batch-append", "deps/tokio-1.47", "docs/membership"]);
  const event = opts.event ?? (r() < 0.7 ? "push" : r() < 0.85 ? "pull_request" : r() < 0.5 ? "schedule" : "tag");
  const branch = opts.branch ?? (event === "pull_request" ? prBranch : event === "tag" ? `v0.${Math.floor(r() * 9)}.${Math.floor(r() * 20)}` : pick(r, concrete.length ? concrete : ["main"]));
  const n = (runNumbers.get(repo.id) ?? 1) + 1;
  runNumbers.set(repo.id, n);
  const pipe = pipelineFor(tc, branch)[0]!;
  const run: Run = {
    id: rid("run_"),
    repo_id: repo.id,
    repo_name: repo.name,
    number: n,
    pipeline: opts.pipeline ?? pipe.name,
    event,
    branch,
    sha: sha(r),
    message: event === "schedule" ? "nightly" : commitMessage(r, tc),
    author: event === "schedule" ? "kiln" : pick(r, AUTHORS),
    status: "pending",
    created_at: opts.created,
    started_at: null,
    finished_at: null,
    error: null,
    concurrency_group: event === "push" && branch === "main" ? `${repo.name}/main` : null,
  };
  return { run, tpls: pipe.jobs };
}

function newSimJob(run: Run, tpl: JobTpl, idx: number): SimJob {
  const id = `${run.id}_j${idx}`;
  const steps: SimStep[] = tpl.steps.map((s, i) => ({
    idx: i,
    name: s.name,
    status: "pending",
    started_at: null,
    finished_at: null,
    exit_code: null,
    dur: between(s.dur[0], s.dur[1]),
    kind: s.kind,
    run: s.run,
    emitted: 0,
  }));
  const planned = plannedFor(tpl);
  return {
    job: {
      id,
      run_id: run.id,
      key: tpl.key,
      display: tpl.key,
      status: "pending",
      labels: tpl.labels,
      needs: tpl.needs,
      agent: null,
      executor: null,
      attempt: 1,
      queued_at: null,
      started_at: null,
      finished_at: null,
      exit_code: null,
      error: null,
      peak_rss: null,
      cpu_seconds: null,
      steps: steps.map(stripStep),
    },
    tpl,
    steps,
    failStep: null,
    stepStartedMs: null,
    planned,
  };
}

function stripStep(s: SimStep): Step {
  return { idx: s.idx, name: s.name, status: s.status, started_at: s.started_at, finished_at: s.finished_at, exit_code: s.exit_code };
}
function syncJob(sj: SimJob) {
  sj.job.steps = sj.steps.map(stripStep);
}

/** Fully resolve a historical run (all jobs finished, deterministic per seed). */
function historicalRun(created: number, seed: number) {
  const r = mulberry32(seed);
  const repo = weightedRepo(r);
  const { run, tpls } = makeRun(repo, { created, r });
  const outcome = r();
  const runStatus: Status = outcome < 0.84 ? "succeeded" : outcome < 0.95 ? "failed" : outcome < 0.98 ? "cancelled" : "timed_out";
  const sjobs = tpls.map((tpl, i) => newSimJob(run, tpl, i));
  const failIdx = runStatus === "failed" || runStatus === "timed_out" ? 1 + Math.floor(r() * (sjobs.length - 1)) : -1;
  const finish = new Map<string, number>();
  const statusOf = new Map<string, Status>();
  let runStart = Infinity;
  let runEnd = created;
  const fleet = [...agents.values()].filter((a) => !a.ephemeral);
  for (const [i, sj] of sjobs.entries()) {
    const deps = sj.tpl.needs;
    const depFailed = deps.some((d) => statusOf.get(d) !== "succeeded");
    const ready = Math.max(created + 3, ...deps.map((d) => finish.get(d) ?? created));
    if (depFailed || runStatus === "cancelled" && i > sjobs.length / 2) {
      sj.job.status = runStatus === "cancelled" ? "cancelled" : "skipped";
      sj.steps.forEach((s) => (s.status = sj.job.status));
      statusOf.set(sj.tpl.key, sj.job.status);
      finish.set(sj.tpl.key, ready);
      syncJob(sj);
      continue;
    }
    const wait = Math.floor(r() * 25 + 1);
    const start = ready + wait;
    let tcur = start;
    const failing = i === failIdx;
    const failStep = failing ? Math.max(1, sj.steps.length - 1 - Math.floor(r() * 2)) : -1;
    let jobStatus: Status = "succeeded";
    for (const s of sj.steps) {
      if (jobStatus !== "succeeded") {
        s.status = "skipped";
        continue;
      }
      s.started_at = Math.floor(tcur);
      tcur += s.dur;
      s.finished_at = Math.floor(tcur);
      if (s.idx === failStep) {
        s.status = runStatus === "timed_out" ? "timed_out" : "failed";
        s.exit_code = runStatus === "timed_out" ? null : 101;
        jobStatus = s.status;
      } else {
        s.status = "succeeded";
        s.exit_code = 0;
      }
    }
    const isK8s = sj.tpl.labels.includes("k8s");
    const agent = isK8s ? `kiln-pod-${seed.toString(36).slice(0, 5)}${i}` : pick(r, fleet.filter((a) => sj.tpl.labels.every((l) => a.labels.includes(l))).map((a) => a.name).concat(["metal-ryzen-02"]));
    Object.assign(sj.job, {
      status: jobStatus,
      agent,
      executor: isK8s ? "k8s" : "baremetal",
      queued_at: ready,
      started_at: start,
      finished_at: Math.floor(tcur),
      exit_code: jobStatus === "succeeded" ? 0 : jobStatus === "failed" ? 101 : null,
      error: jobStatus === "timed_out" ? "timed out after 60m" : jobStatus === "failed" ? `step "${sj.steps[failStep]?.name}" exited with code 101` : null,
      peak_rss: Math.floor(between(80, 6000) * 1024 * 1024),
      cpu_seconds: Math.round((tcur - start) * between(0.6, 6) * 10) / 10,
      attempt: r() < 0.05 ? 2 : 1,
    });
    sj.failStep = failStep >= 0 ? failStep : null;
    syncJob(sj);
    statusOf.set(sj.tpl.key, jobStatus);
    finish.set(sj.tpl.key, Math.floor(tcur));
    runStart = Math.min(runStart, start);
    runEnd = Math.max(runEnd, Math.floor(tcur));
  }
  run.status = runStatus;
  run.started_at = Number.isFinite(runStart) ? runStart : created + 2;
  run.finished_at = runEnd;
  run.error = runStatus === "timed_out" ? "run exceeded its timeout" : runStatus === "cancelled" ? "cancelled by ada" : null;
  return { run, sjobs };
}

// ~3 days of history
{
  let tc = now() - 1800;
  let seed = 1;
  const hist: { run: Run; sjobs: SimJob[] }[] = [];
  while (tc > now() - 86400 * 3) {
    hist.push(historicalRun(tc, seed++ * 7919));
    const hour = new Date(tc * 1000).getHours();
    const busy = hour >= 9 && hour <= 20;
    tc -= Math.floor(between(busy ? 180 : 600, busy ? 900 : 2400));
  }
  // assign numbers in chronological order
  hist.reverse();
  const counters = new Map<string, number>();
  for (const h of hist) {
    const n = (counters.get(h.run.repo_id) ?? Math.floor(hashStr(h.run.repo_id) % 400) + 20) + 1;
    counters.set(h.run.repo_id, n);
    h.run.number = n;
  }
  for (const [k, v] of counters) runNumbers.set(k, v);
  hist.reverse();
  for (const h of hist) {
    runs.push(h.run);
    jobsByRun.set(h.run.id, h.sjobs);
    h.sjobs.forEach((s) => jobsById.set(s.job.id, s));
  }
}

/* ------------------------------------------------------------------ */
/* Events                                                              */
/* ------------------------------------------------------------------ */

function emit(ev: ServerEvent) {
  pushHeard(ev);
  const msg = JSON.stringify(ev);
  for (const ws of eventClients) if (ws.readyState === WebSocket.OPEN) ws.send(msg);
}
setLongEmit((ev) => emit(ev as ServerEvent));
setDemoEmit((ev) => emit(ev as ServerEvent));
setFamilyEmit((ev) => emit(ev as ServerEvent));

function queueCounts() {
  let queued = 0;
  let running = 0;
  for (const run of runs.slice(0, 80)) {
    if (run.status !== "running" && run.status !== "pending" && run.status !== "queued") continue;
    for (const sj of jobsByRun.get(run.id) ?? []) {
      if (sj.job.status === "queued") queued++;
      if (sj.job.status === "running") running++;
    }
  }
  return { queued, running };
}

/* ------------------------------------------------------------------ */
/* Logs                                                                */
/* ------------------------------------------------------------------ */

function logKey(jobId: string, step: number) {
  return `${jobId}:${step}`;
}

function planFor(sj: SimJob, step: SimStep): LogPlan {
  if (!step.plan) {
    const run = runs.find((r) => r.id === sj.job.run_id);
    step.plan = stepLog(step.kind, step.run, hashStr(`${sj.job.id}:${step.idx}`), {
      repo: run?.repo_name ?? "hyper-light/x",
      sha: run?.sha ?? "0".repeat(40),
      branch: run?.branch ?? "main",
      job: sj.job.key,
    });
  }
  return step.plan;
}

function stepFooter(step: SimStep): string {
  const d = step.finished_at && step.started_at ? step.finished_at - step.started_at : 0;
  if (step.status === "succeeded") return `\x1b[2m── step finished in ${d}s · exit 0\x1b[0m\n`;
  if (step.status === "timed_out") return `\x1b[1;33m── step timed out after ${d}s\x1b[0m\n`;
  if (step.status === "failed") return `\x1b[1;31m── step failed after ${d}s · exit ${step.exit_code ?? 1}\x1b[0m\n`;
  return "";
}

/** Full log for a finished step (lazily generated). */
function completedLog(sj: SimJob, step: SimStep): string {
  if (step.status === "skipped" || step.status === "cancelled" || step.status === "pending") return "";
  const p = planFor(sj, step);
  const failed = step.status === "failed" || step.status === "timed_out";
  const lines = failed ? [...p.lines.slice(0, Math.max(2, Math.floor(p.lines.length * 0.7))), ...p.failLines] : p.lines;
  return lines.join("\n") + "\n" + stepFooter(step);
}

function getStream(sj: SimJob, idx: number): LogStream {
  const k = logKey(sj.job.id, idx);
  let s = logs.get(k);
  if (!s) {
    const step = sj.steps[idx];
    const done = !step || !["running", "pending", "queued"].includes(step.status);
    s = { data: step && done ? completedLog(sj, step) : "", done, subs: new Set() };
    logs.set(k, s);
  }
  return s;
}

function appendLog(sj: SimJob, idx: number, text: string) {
  const s = getStream(sj, idx);
  s.data += text;
  const buf = Buffer.from(text, "utf8");
  for (const ws of s.subs) if (ws.readyState === WebSocket.OPEN) ws.send(buf, { binary: true });
}

function endLog(sj: SimJob, idx: number) {
  const s = getStream(sj, idx);
  s.done = true;
  const status = sj.steps[idx]?.status ?? "succeeded";
  for (const ws of s.subs) {
    if (ws.readyState !== WebSocket.OPEN) continue;
    ws.send(JSON.stringify({ type: "step", idx, status }));
    ws.send(JSON.stringify({ type: "end" }));
    ws.close(1000);
  }
  s.subs.clear();
}

/* ------------------------------------------------------------------ */
/* Simulation                                                          */
/* ------------------------------------------------------------------ */

function deriveRunStatus(run: Run) {
  const sjobs = jobsByRun.get(run.id) ?? [];
  const st = sjobs.map((s) => s.job.status);
  const prev = run.status;
  if (run.status === "cancelled") return;
  if (st.some((s) => s === "running")) run.status = "running";
  else if (st.every((s) => ["succeeded", "failed", "cancelled", "skipped", "timed_out"].includes(s))) {
    run.status = st.some((s) => s === "timed_out") ? "timed_out" : st.some((s) => s === "failed") ? "failed" : st.some((s) => s === "cancelled") ? "cancelled" : "succeeded";
    run.finished_at ??= now();
    if (run.status === "failed") run.error = null;
  } else if (st.some((s) => s === "queued")) run.status = run.started_at ? "running" : "queued";
  if (run.status === "running" && !run.started_at) run.started_at = now();
  if (prev !== run.status) emit({ type: "run", run });
}

function freeAgent(name: string | null, jobId: string) {
  if (!name) return;
  const a = agents.get(name);
  if (!a) return;
  a.running = a.running.filter((j) => j !== jobId);
  a.busy = a.running.length;
  a.last_seen = now();
  if (a.ephemeral) {
    a.connected = false;
    setTimeout(() => {
      agents.delete(name);
      agentState.delete(name);
      history.delete(name);
    }, 90_000);
  }
  emit({ type: "agent", agent: a });
}

function assign(sj: SimJob): boolean {
  const labels = sj.tpl.labels;
  if (labels.includes("k8s")) {
    const pods = [...agents.values()].filter((a) => a.ephemeral && a.connected).length;
    if (pods >= 6) return false;
    const name = `kiln-pod-${randomBytes(3).toString("hex")}`;
    const host = newHost(4, 8, 50);
    const a = addAgent({ name, labels: ["k8s", "linux", "x86_64"], slots: 1, hostname: name, os: "linux", arch: "x86_64", version: VERSION, ephemeral: true, connected: true, last_seen: now(), enrolled_at: now() }, host, false);
    history.set(name, []);
    sj.job.agent = name;
    sj.job.executor = "k8s";
    a.running = [sj.job.id];
    a.busy = 1;
    emit({ type: "agent", agent: a });
    return true;
  }
  const candidates = [...agents.values()].filter((a) => !a.ephemeral && a.connected && !a.draining && a.busy < a.slots && labels.every((l) => a.labels.includes(l)));
  const a = candidates.sort((x, y) => x.busy / x.slots - y.busy / y.slots)[0];
  if (!a) return false;
  sj.job.agent = a.name;
  sj.job.executor = "baremetal";
  a.running = [...a.running, sj.job.id];
  a.busy = a.running.length;
  emit({ type: "agent", agent: a });
  return true;
}

function startRun(repo: Repo, opts: { branch?: string; event?: string; pipeline?: string } = {}) {
  const { run, tpls } = makeRun(repo, { created: now(), ...opts });
  const sjobs = tpls.map((tpl, i) => newSimJob(run, tpl, i));
  // ~18% of live runs fail somewhere
  if (rnd() < 0.18) {
    const victim = sjobs[1 + Math.floor(rnd() * (sjobs.length - 1))];
    if (victim) victim.failStep = victim.steps.length - 1;
  }
  runs.unshift(run);
  jobsByRun.set(run.id, sjobs);
  sjobs.forEach((s) => jobsById.set(s.job.id, s));
  emit({ type: "run", run });
  sjobs.forEach((s) => emit({ type: "job", job: s.job }));
  return run;
}

/** kiln's release pipeline (`.kiln/release.yaml`), as System → Builds' Build every platform runs it: a job per platform family, each on a machine of its own. */
const RELEASE_UI = { name: "ui", run: "npm ci --prefix ui --no-audit --no-fund && npm run --prefix ui build", dur: [5, 8] as [number, number], kind: "npm" };
const RELEASE_JOBS: JobTpl[] = [
  {
    key: "linux",
    needs: [],
    labels: ["baremetal", "linux"],
    toolchain: "rust",
    steps: [RELEASE_UI, { name: "build", run: "nice -n 10 ci/release/build.sh x86_64-unknown-linux-musl aarch64-unknown-linux-musl", dur: [14, 20], kind: "compile" }],
  },
  {
    key: "windows",
    needs: [],
    labels: ["baremetal", "linux"],
    toolchain: "rust",
    steps: [RELEASE_UI, { name: "build", run: "nice -n 10 ci/release/build.sh x86_64-pc-windows-msvc", dur: [18, 26], kind: "compile" }],
  },
  {
    key: "macos",
    needs: [],
    labels: ["baremetal", "macos"],
    toolchain: "rust",
    steps: [RELEASE_UI, { name: "build", run: "nice -n 10 ci/release/build.sh aarch64-apple-darwin", dur: [24, 32], kind: "compile" }],
  },
];
let releaseRun: Run | null = null;

/**
 * A release run: Build every platform's (a manual run of `commit`), or a push to main's. `hold`
 * keeps a job queued that many seconds (one machine builds Linux and Windows in turn), `slow`
 * draws its steps out, so a page shows it building for a while.
 */
function startRelease(commit: string, opts: { event?: string; message?: string; hold?: Record<string, number>; slow?: number } = {}): Run {
  const repo = repos.find((r) => r.name === "hyper-light/kiln") ?? repos[0]!;
  const { run } = makeRun(repo, { created: now(), branch: "main", event: opts.event ?? "manual", pipeline: "release" });
  Object.assign(run, { sha: commit, message: opts.message ?? "Build every platform", author: opts.event === "push" ? "ada" : "admin" });
  const sjobs = RELEASE_JOBS.map((tpl, i) => newSimJob(run, tpl, i));
  for (const sj of sjobs) {
    if (opts.slow) sj.steps.forEach((s) => (s.dur *= opts.slow!));
    if (opts.hold?.[sj.tpl.key]) sj.holdUntil = now() + opts.hold[sj.tpl.key]!;
  }
  runs.unshift(run);
  jobsByRun.set(run.id, sjobs);
  sjobs.forEach((s) => jobsById.set(s.job.id, s));
  emit({ type: "run", run });
  sjobs.forEach((s) => emit({ type: "job", job: s.job }));
  releaseRun = run;
  return run;
}

/**
 * The release runs the kept binaries came from, newest first: two of kiln's past runs, made release
 * runs of `sha`, every job done, so a binary's run number opens its job's log. The older one built
 * the server's own platform, which the server's own build shadows.
 */
const pastReleases: Run[] = [
  { ago: 80_000, sha: "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678" },
  { ago: 205_000, sha: "df17c34639ecbf9d5c9caefb6af28aa67ee01257" },
].flatMap(({ ago, sha }) => {
  const kiln = runs.filter((r) => r.repo_name === "hyper-light/kiln");
  const run = kiln.reduce<Run | null>((a, r) => (a === null || Math.abs(r.created_at - (now() - ago)) < Math.abs(a.created_at - (now() - ago)) ? r : a), null);
  if (!run) return [];
  for (const s of jobsByRun.get(run.id) ?? []) jobsById.delete(s.job.id);
  Object.assign(run, { pipeline: "release", event: "push", branch: "main", sha, message: "Release builds", status: "succeeded", error: null });
  let end = run.created_at;
  const sjobs = RELEASE_JOBS.map((tpl, i) => {
    const sj = newSimJob(run, tpl, i);
    const start = run.created_at + 4 + i * 3;
    let t = start;
    for (const s of sj.steps) {
      s.dur *= 12;
      Object.assign(s, { status: "succeeded", exit_code: 0, started_at: Math.floor(t), finished_at: Math.floor((t += s.dur)) });
    }
    const agent = tpl.labels.includes("macos") ? "metal-mac-mini" : "metal-ryzen-01";
    Object.assign(sj.job, {
      status: "succeeded",
      agent,
      executor: "baremetal",
      queued_at: run.created_at + 2,
      started_at: start,
      finished_at: Math.floor(t),
      exit_code: 0,
      peak_rss: 3_100_000_000,
      cpu_seconds: Math.round((t - start) * 5),
    });
    syncJob(sj);
    end = Math.max(end, Math.floor(t));
    return sj;
  });
  Object.assign(run, { started_at: run.created_at + 4, finished_at: end });
  jobsByRun.set(run.id, sjobs);
  sjobs.forEach((s) => jobsById.set(s.job.id, s));
  return [run];
});

// A push to main builds a commit newer than the server's (main moved on since it was built): its
// Mac builds first while Linux and Windows wait their turn on one machine. kiln keeps none of
// it (it takes builds of the server's own commit only); the page shows it building.
startRelease("25159ff8b2d4c6e0a1f3b5d7c9e1a3f5b7d9c1e3", { event: "push", message: "Builds open their logs under the platforms", hold: { linux: 45, windows: 100 }, slow: 4 });

function finishJob(sj: SimJob, status: Status, error: string | null) {
  const j = sj.job;
  j.status = status;
  j.finished_at = now();
  j.exit_code = status === "succeeded" ? 0 : status === "failed" ? 101 : null;
  j.error = error;
  j.cpu_seconds = Math.round(((j.finished_at - (j.started_at ?? j.finished_at)) * between(0.5, 5)) * 10) / 10;
  freeAgent(j.agent, j.id);
  syncJob(sj);
  emit({ type: "job", job: j });
}

function tick() {
  const nowMs = Date.now();
  const nowS = Math.floor(nowMs / 1000);
  for (const run of runs.slice(0, 60)) {
    if (!["pending", "queued", "running"].includes(run.status)) continue;
    const sjobs = jobsByRun.get(run.id) ?? [];
    const byKey = new Map(sjobs.map((s) => [s.tpl.key, s]));
    for (const sj of sjobs) {
      const j = sj.job;
      if (j.status === "pending") {
        const deps = sj.tpl.needs.map((k) => byKey.get(k)?.job.status);
        if (deps.some((d) => d && ["failed", "cancelled", "skipped", "timed_out"].includes(d))) {
          j.status = "skipped";
          sj.steps.forEach((s) => (s.status = "skipped"));
          j.finished_at = nowS;
          syncJob(sj);
          emit({ type: "job", job: j });
        } else if (deps.every((d) => d === "succeeded")) {
          j.status = "queued";
          j.queued_at = nowS;
          sj.steps.forEach((s) => (s.status = "queued"));
          syncJob(sj);
          emit({ type: "job", job: j });
        }
      } else if (j.status === "queued") {
        if ((sj.holdUntil ?? 0) <= nowS && nowS - (j.queued_at ?? nowS) >= 1 && rnd() < 0.7 && assign(sj)) {
          j.status = "running";
          j.started_at = nowS;
          const s0 = sj.steps[0]!;
          s0.status = "running";
          s0.started_at = nowS;
          sj.stepStartedMs = nowMs;
          sj.steps.slice(1).forEach((s) => (s.status = "pending"));
          appendLog(sj, 0, "");
          syncJob(sj);
          emit({ type: "job", job: j });
        }
      } else if (j.status === "running") {
        const cur = sj.steps.find((s) => s.status === "running");
        if (!cur) continue;
        const plan = planFor(sj, cur);
        const elapsed = (nowMs - (sj.stepStartedMs ?? nowMs)) / 1000;
        const frac = Math.min(1, elapsed / cur.dur);
        const target = Math.floor(frac * plan.lines.length);
        if (target > cur.emitted) {
          appendLog(sj, cur.idx, plan.lines.slice(cur.emitted, target).join("\n") + "\n");
          cur.emitted = target;
        }
        j.peak_rss = Math.max(j.peak_rss ?? 0, Math.floor(between(60, 4000) * 1024 * 1024 * frac));
        if (frac >= 1) {
          if (cur.emitted < plan.lines.length) appendLog(sj, cur.idx, plan.lines.slice(cur.emitted).join("\n") + "\n");
          cur.finished_at = nowS;
          const fails = sj.failStep === cur.idx;
          if (fails) appendLog(sj, cur.idx, plan.failLines.join("\n") + "\n");
          cur.status = fails ? "failed" : "succeeded";
          cur.exit_code = fails ? 101 : 0;
          appendLog(sj, cur.idx, stepFooter(cur));
          endLog(sj, cur.idx);
          const next = sj.steps[cur.idx + 1];
          if (fails) {
            sj.steps.slice(cur.idx + 1).forEach((s) => (s.status = "skipped"));
            finishJob(sj, "failed", `step "${cur.name}" exited with code 101`);
          } else if (next) {
            next.status = "running";
            next.started_at = nowS;
            sj.stepStartedMs = nowMs;
            appendLog(sj, next.idx, "");
            syncJob(sj);
            emit({ type: "job", job: j });
          } else {
            finishJob(sj, "succeeded", null);
          }
        }
      }
    }
    deriveRunStatus(run);
  }
}

let lastQueue = "";
function emitQueue(force = false) {
  const q = queueCounts();
  const k = JSON.stringify(q);
  if (force || k !== lastQueue) {
    lastQueue = k;
    emit({ type: "queue", ...q });
  }
}

function sampleAll() {
  const ts = Date.now();
  const totalBusy = [...agents.values()].reduce((s, a) => s + a.busy, 0);
  const srv = stepHost(serverHost, Math.min(1, 0.1 + totalBusy * 0.04), ts);
  pushSample("server", srv);
  emit({ type: "sample", agent: "server", sample: srv });
  for (const a of agents.values()) {
    if (!a.connected) continue;
    const h = agentState.get(a.name);
    if (!h) continue;
    const s = stepHost(h, a.slots ? a.busy / a.slots : 0, ts);
    a.sample = s;
    a.last_seen = Math.floor(ts / 1000);
    pushSample(a.name, s);
    emit({ type: "sample", agent: a.name, sample: s });
  }
}

// Seed a couple of in-flight runs so the UI is lively immediately.
for (let i = 0; i < 3; i++) startRun(weightedRepo(rnd));
for (let i = 0; i < 25; i++) tick();

setInterval(tick, 1000);
setInterval(() => emitQueue(), 1000);
setInterval(() => emitQueue(true), 5000);
setInterval(sampleAll, 5000);
(function scheduleNewRun() {
  setTimeout(() => {
    const repo = weightedRepo(rnd);
    startRun(repo);
    repo.last_polled_at = now();
    scheduleNewRun();
  }, between(12_000, 35_000));
})();
setInterval(() => {
  for (const r of repos) if (rnd() < 0.3) r.last_polled_at = now();
}, 10_000);

/* ------------------------------------------------------------------ */
/* Discovery and joining (Runners → Add machines)                      */
/*                                                                     */
/*   MOCK_SERVER_OS=macos|linux|windows  the server's OS (default macos: */
/*                                       Bonjour is a discovery method)  */
/*   MOCK_NETWORK=empty                  nothing else on the network     */
/*   MOCK_AUTOJOIN=0                     no machine runs the one-liner   */
/*   MOCK_AUTOJOIN_SECONDS=30            how long after the page asked   */
/* ------------------------------------------------------------------ */

const SERVER_OS: MockOs = process.env.MOCK_SERVER_OS === "linux" || process.env.MOCK_SERVER_OS === "windows" ? process.env.MOCK_SERVER_OS : "macos";
const SERVER_ARCH = SERVER_OS === "macos" ? "aarch64" : "x86_64";
/** The OS's own DNS-SD (Bonjour, Windows DNS-SD); Linux has none kiln uses. */
const NATIVE = SERVER_OS !== "linux";
const EMPTY_NETWORK = process.env.MOCK_NETWORK === "empty";
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

interface MockInstance {
  id: string;
  instance: string;
  name: string;
  role: string;
  version: string;
  os: string;
  arch: string;
  port: number;
  quic: number;
  ca: string | null;
  enrolled: string | null;
  addresses: string[];
  first_seen: number;
  last_seen: number;
  via: DiscoveryVia[];
  invited: boolean;
  request: string | null;
}

const instances = new Map<string, MockInstance>();
const joinRequests = new Map<string, JoinRequest>();

function seedInstance(s: InstanceSeed, at = now()): MockInstance {
  return {
    id: s.id,
    instance: `kiln ${s.role} on ${s.name} (${s.id.slice(0, 8)})`,
    name: s.name,
    role: s.role,
    version: s.version || VERSION,
    os: s.os,
    arch: s.arch,
    port: s.port,
    quic: s.quic,
    ca: s.ca,
    enrolled: s.enrolled,
    addresses: [...s.addresses],
    first_seen: at - s.firstAgo,
    last_seen: at - s.lastAgo,
    via: NATIVE ? [...s.via] : s.via.filter((v) => v !== "native"),
    invited: s.invited ?? false,
    request: null,
  };
}

// This server shows up too: through its own DNS-SD zone and the last scan.
instances.set(
  SERVER_INSTANCE_ID,
  seedInstance({ id: SERVER_INSTANCE_ID, name: HOSTNAME, role: "server", version: "", os: SERVER_OS, arch: SERVER_ARCH, port: 7070, quic: 7071, ca: SERVER_CA, enrolled: null, addresses: [SERVER_ADDR], via: ["dns-sd", "scan"], firstAgo: 86_000, lastAgo: 4 }),
);
if (!EMPTY_NETWORK) for (const s of INSTANCE_SEEDS) instances.set(s.id, seedInstance(s));

/** The agent an instance already is here: one of our runners on that host, or a runner enrolled with our CA. */
function joinedAs(i: MockInstance): string | null {
  const host = [...agents.values()].find((a) => !a.ephemeral && a.hostname.toLowerCase() === i.name.toLowerCase());
  if (host) return host.name;
  return i.role === "agent" && i.enrolled === SERVER_CA ? i.name.replace(/\.lan$/, "") : null;
}

function addressOf(i: MockInstance) {
  return i.addresses.find((a) => !a.includes(":")) ?? i.addresses[0] ?? null;
}

function presentInstance(i: MockInstance): DiscoveredInstance {
  const agent = joinedAs(i);
  const addr = addressOf(i);
  return {
    ...i,
    url: i.port && addr ? `http://${addr.includes(":") ? `[${addr}]` : addr}:${i.port}` : null,
    joined: agent !== null,
    agent,
    same_ca: i.ca === SERVER_CA,
    join_command: `kiln join --server ${SERVER_URL}`,
  };
}

const sixDigits = () => String(Math.floor(rnd() * 1e6)).padStart(6, "0");

/** A machine asking to join (`kiln join`). `code: false`: it has not confirmed yet (it does after `confirmMs`). */
function addJoinRequest(
  r: Pick<JoinRequest, "name" | "hostname" | "os" | "arch" | "labels" | "version" | "address">,
  opts: { code?: string | false; ago?: number; instance?: string; confirmMs?: number } = {},
): JoinRequest {
  const at = now() - (opts.ago ?? 0);
  const jr: JoinRequest = { ...r, id: rid("jr_"), status: "pending", code: opts.code === false ? null : (opts.code ?? sixDigits()), created_at: at, expires_at: at + 600, agent: null };
  joinRequests.set(jr.id, jr);
  const inst = opts.instance ? instances.get(opts.instance) : undefined;
  if (inst) inst.request = jr.id;
  if (jr.code === null) {
    setTimeout(() => {
      if (jr.status !== "pending" || jr.code) return;
      jr.code = sixDigits();
      emit({ type: "join_requests" });
    }, opts.confirmMs ?? 3000);
  }
  return jr;
}

function requestFrom(i: MockInstance): Pick<JoinRequest, "name" | "hostname" | "os" | "arch" | "labels" | "version" | "address"> {
  const name = i.name.replace(/\.lan$/, "");
  return { name, hostname: i.name, os: i.os, arch: i.arch, labels: ["baremetal", i.os, i.arch], version: i.version, address: addressOf(i) ?? "" };
}

if (!EMPTY_NETWORK) {
  const studio = INSTANCE_SEEDS.find((s) => s.name === "studio-mbp.lan")!;
  addJoinRequest(requestFrom(instances.get(studio.id)!), { code: "482913", ago: 95, instance: studio.id });
  // Two machines that are not on this network: one over the internet, one over Tailscale.
  addJoinRequest({ name: "ci-win-02", hostname: "CI-WIN-02", os: "windows", arch: "x86_64", labels: ["baremetal", "windows", "x86_64"], version: VERSION, address: "203.0.113.24" }, { code: "731064", ago: 40 });
  addJoinRequest({ name: "gpu-box", hostname: "gpu-box", os: "linux", arch: "x86_64", labels: ["baremetal", "gpu", "linux", "x86_64"], version: "0.0.9", address: "100.70.3.9" }, { code: false, ago: 3, confirmMs: 9000 });
}

// Requests live 10 minutes; a machine still waiting asks again, with a new code. Decided ones go.
setInterval(() => {
  const t = now();
  let changed = false;
  for (const jr of [...joinRequests.values()]) {
    if (jr.expires_at > t) continue;
    joinRequests.delete(jr.id);
    changed = true;
    if (jr.status !== "pending") continue;
    const inst = [...instances.values()].find((i) => i.request === jr.id);
    if (inst) inst.request = null;
    addJoinRequest({ name: jr.name, hostname: jr.hostname, os: jr.os, arch: jr.arch, labels: jr.labels, version: jr.version, address: jr.address }, { code: false, instance: inst?.id, confirmMs: 2000 });
  }
  if (changed) {
    emit({ type: "join_requests" });
    emit({ type: "discovery" });
  }
}, 5000);

// What announces itself is heard again every 30 s or so; what a scan or a probe found is not.
setInterval(() => {
  const t = now();
  for (const i of instances.values()) if (i.via.some((v) => v === "mdns" || v === "native" || v === "tailscale" || v === "dns-sd")) i.last_seen = t - Math.floor(rnd() * 8);
}, 30_000);

/* ---- scans ---- */

let scan: ScanState | null = { running: false, cidrs: [SUBNETS[0]!], started_at: now() - 760, finished_at: now() - 731, probed: 254, total: 254, found: 2 };

function ipv4(s: string): number | null {
  const m = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/.exec(s);
  if (!m) return null;
  const o = m.slice(1, 5).map(Number);
  return o.some((x) => x > 255) ? null : ((o[0]! << 24) | (o[1]! << 16) | (o[2]! << 8) | o[3]!) >>> 0;
}

/** Addresses in a CIDR, or why it can't be scanned. */
function cidrSize(c: string): number | string {
  const [addr = "", bits = ""] = c.split("/");
  const n = Number(bits);
  if (ipv4(addr) === null || !/^\d{1,2}$/.test(bits) || n > 32) return `${c} is not an IPv4 subnet like 192.168.1.0/24`;
  if (n < 22) return `${c} is too big to scan: at most a /22 (1022 addresses) at a time`;
  return n >= 31 ? 2 ** (32 - n) : 2 ** (32 - n) - 2;
}

function inCidr(addr: string, c: string): boolean {
  const [base = "", bits = "32"] = c.split("/");
  const a = ipv4(addr);
  const b = ipv4(base);
  if (a === null || b === null) return false;
  const mask = Number(bits) === 0 ? 0 : (~0 << (32 - Number(bits))) >>> 0;
  return ((a & mask) >>> 0) === ((b & mask) >>> 0);
}

function startScan(cidrs: string[], total: number): ScanState {
  const s: ScanState = { running: true, cidrs, started_at: now(), finished_at: null, probed: 0, total, found: 0 };
  scan = s;
  const step = Math.max(1, Math.ceil(total / 40));
  let emitted = 0;
  const timer = setInterval(() => {
    s.probed = Math.min(total, s.probed + step);
    const find = SCAN_FIND.addresses[0]!;
    if (s.probed >= total * 0.6 && !instances.has(SCAN_FIND.id) && cidrs.some((c) => inCidr(find, c))) {
      instances.set(SCAN_FIND.id, seedInstance(SCAN_FIND));
      s.found++;
    }
    if (s.probed >= total) {
      clearInterval(timer);
      s.running = false;
      s.finished_at = now();
      for (const i of instances.values()) {
        if (i.role === "agent" && i.id !== AUTOJOIN.id && i.addresses.some((a) => cidrs.some((c) => inCidr(a, c))) && !i.via.includes("scan")) i.via.push("scan");
      }
      emit({ type: "discovery" });
      return;
    }
    // Progress events at most twice a second, as the server sends them.
    if (Date.now() - emitted >= 500) {
      emitted = Date.now();
      emit({ type: "discovery" });
    }
  }, 250);
  return s;
}

/* ---- the join token, and a machine running the one-liner ---- */

let joinToken = { token: `kj_${randomBytes(18).toString("base64url")}`, expires_at: now() + 86_400 };
let autojoinPlanned = process.env.MOCK_AUTOJOIN === "0";

function joinInfo(): JoinInfo & { quic: string; manual: string } {
  if (joinToken.expires_at <= now()) joinToken = { token: `kj_${randomBytes(18).toString("base64url")}`, expires_at: now() + 86_400 };
  const { token, expires_at } = joinToken;
  const install = {
    linux: `curl -fsSL ${SERVER_URL}/install.sh | sh -s -- --token ${token} --labels baremetal,linux`,
    macos: `curl -fsSL ${SERVER_URL}/install.sh | sh -s -- --token ${token} --labels baremetal,macos`,
    windows: `& ([scriptblock]::Create((irm '${SERVER_URL}/install.ps1'))) -Token '${token}' -Labels 'baremetal,windows'`,
  };
  return {
    token,
    server: SERVER_URL,
    quic: `${SERVER_ADDR}:7071`,
    command: install.linux,
    macos: install.macos,
    windows: install.windows,
    manual: `kiln agent --server ${SERVER_URL} --token ${token} --labels baremetal`,
    join: `curl -fsSL ${SERVER_URL}/install.sh | sh -s -- --join`,
    join_manual: `kiln join --server ${SERVER_URL}`,
    install,
    expires_at,
  };
}

/** Once per mock run: a while after someone asked for the one-liner, a machine runs it and connects. */
function planAutojoin() {
  if (autojoinPlanned) return;
  autojoinPlanned = true;
  setTimeout(() => {
    if (agents.has(AUTOJOIN.name)) return;
    const a = addAgent({ name: AUTOJOIN.name, labels: ["baremetal", "linux", "aarch64"], slots: 1, hostname: AUTOJOIN.hostname, os: AUTOJOIN.os, arch: AUTOJOIN.arch, version: VERSION, ephemeral: false, connected: true, last_seen: now(), enrolled_at: now() }, newHost(4, 8, 250));
    instances.set(AUTOJOIN.id, seedInstance({ id: AUTOJOIN.id, name: AUTOJOIN.hostname, role: "agent", version: "", os: AUTOJOIN.os, arch: AUTOJOIN.arch, port: 0, quic: 0, ca: null, enrolled: SERVER_CA, addresses: [AUTOJOIN.address], via: ["mdns"], firstAgo: 0, lastAgo: 0 }));
    addAudit(AUTOJOIN.name, "agent.enroll", "");
    emit({ type: "agent", agent: a });
    emit({ type: "discovery" });
  }, Number(process.env.MOCK_AUTOJOIN_SECONDS ?? 30) * 1000);
}

function discoveryInfo(): DiscoveryInfo {
  const list = [...instances.values()];
  const by = (v: DiscoveryVia) => list.filter((i) => i.id !== SERVER_INSTANCE_ID && i.via.includes(v));
  const last = (xs: MockInstance[]) => (xs.length ? Math.max(...xs.map((i) => i.last_seen)) : null);
  const method = (id: DiscoveryMethod["id"], label: string, state: DiscoveryMethod["state"], detail: string, v?: DiscoveryVia): DiscoveryMethod => {
    const xs = v && state === "active" ? by(v) : [];
    return { id, label, state, detail, heard: xs.length, last_heard: last(xs) };
  };
  const publish = { configured: true, zone: DNS_SD_DOMAINS[0]!, ok: false, error: "update refused by 192.168.1.1 (REFUSED)", at: Math.floor(STARTED / 1000) + 12 };
  const methods: DiscoveryMethod[] = [
    method("mdns", "Multicast DNS", "active", `${mdnsInterfaces(SERVER_OS)} · announcing kiln server on ${HOSTNAME}`, "mdns"),
    NATIVE
      ? method("native", SERVER_OS === "macos" ? "Bonjour (macOS)" : "Windows DNS-SD", "active", SERVER_OS === "macos" ? "mDNSResponder · browsing _kiln._tcp in local." : "DnsServiceBrowse · browsing _kiln._tcp.local", "native")
      : method("native", "Bonjour (macOS)", "unavailable", "only on macOS and Windows; multicast DNS covers Linux"),
    method("dns-sd", `DNS-SD (${DNS_SD_DOMAINS[0]})`, "active", `browsing _kiln._tcp.${DNS_SD_DOMAINS[0]} through 192.168.1.1`, "dns-sd"),
    method("dns-sd", `DNS-SD (${DNS_SD_DOMAINS[1]})`, "error", `SERVFAIL from 10.10.0.2 for _kiln._tcp.${DNS_SD_DOMAINS[1]}`),
    method("scan", "Network scan", "active", scan?.running ? `scanning ${scan.cidrs.join(", ")}` : scan ? `on demand · last ${scan.cidrs.join(", ")}` : "on demand · not run yet", "scan"),
    method("tailscale", "Tailscale", "active", "tailnet tail4a2c.ts.net · 6 peers online, asked every 60 s", "tailscale"),
    method("seeds", "Seed addresses", "off", "none configured"),
  ];
  const problems: DiscoveryProblem[] = [];
  const silent = list.filter((i) => i.id !== SERVER_INSTANCE_ID && !i.via.some((v) => v === "mdns" || v === "native") && i.addresses.some((a) => inCidr(a, SUBNETS[0]!)));
  if (silent.length) {
    const names = silent.map((i) => i.name).join(", ");
    const one = silent.length === 1;
    problems.push({
      id: "silent-network",
      text: `${names} ${one ? "answers" : "answer"} a scan of ${SUBNETS[0]} but ${one ? "has" : "have"} never been heard over multicast DNS. Something between ${one ? "it" : "them"} and this server drops multicast: a firewall, or Wi-Fi client isolation.`,
      fixes: SILENT_NETWORK_FIXES,
    });
  }
  if (publish.ok === false) {
    problems.push({
      id: "dns-sd-publish",
      text: `Publishing this server to ${publish.zone} failed: ${publish.error}. Machines on other networks can't find it there until it is published.`,
      fixes: [{ text: `Allow dynamic updates (RFC 2136) from ${SERVER_ADDR} for ${publish.zone}, or add the records under DNS-SD below to the zone by hand.` }],
    });
  }
  return {
    enabled: true,
    announce: true,
    browse: true,
    join_requests: true,
    self: { id: SERVER_INSTANCE_ID, name: HOSTNAME, instance: `kiln server on ${HOSTNAME} (${SERVER_INSTANCE_ID.slice(0, 8)})`, ca: SERVER_CA, port: 7070, quic: 7071, version: VERSION },
    instances: list.map(presentInstance),
    methods,
    problems,
    subnets: SUBNETS,
    scan,
    dns_sd: { domains: DNS_SD_DOMAINS, publish, records: dnsSdRecords(HOSTNAME, VERSION, SERVER_OS, SERVER_ARCH, publish.zone) },
  };
}

/* ------------------------------------------------------------------ */
/* HTTP                                                                */
/* ------------------------------------------------------------------ */

const sessions = new Set<string>();

function parseCookies(h: string | undefined): Record<string, string> {
  const out: Record<string, string> = {};
  for (const part of (h ?? "").split(";")) {
    const i = part.indexOf("=");
    if (i > 0) out[part.slice(0, i).trim()] = decodeURIComponent(part.slice(i + 1).trim());
  }
  return out;
}

function authed(req: http.IncomingMessage): boolean {
  if (NO_AUTH) return true;
  const auth = req.headers.authorization;
  if (auth?.startsWith("Bearer ") && auth.slice(7) === TOKEN) return true;
  const sid = parseCookies(req.headers.cookie).kiln_session;
  return Boolean(sid && sessions.has(sid));
}

function newSession(res: http.ServerResponse) {
  const sid = randomBytes(18).toString("hex");
  sessions.add(sid);
  res.setHeader("set-cookie", `kiln_session=${sid}; Path=/; HttpOnly; SameSite=Lax; Max-Age=2592000`);
}

/** A log line without its terminal colours (`ESC [ … letter`), as toolfix.rs reads it. */
function plain(s: string): string {
  let out = "";
  for (let i = 0; i < s.length; i++) {
    if (s.charCodeAt(i) === 27 && s[i + 1] === "[") {
      i += 2;
      while (i < s.length && !/[A-Za-z]/.test(s[i]!)) i++;
      continue;
    }
    out += s[i];
  }
  return out;
}

/** The command a shell's line says wasn't there (toolfix.rs `tool_of`, its commonest ways). */
function notFound(line: string): string | null {
  if (line.includes("No developer tools were found")) return "xcode-command-line-tools";
  const m = line.match(/(?:^|: )([\w.+-]+): command not found$/) ?? line.match(/command not found: ([\w.+-]+)$/) ?? line.match(/^(?:\/bin\/)?(?:sh|dash): \d+: ([\w.+-]+): not found$/);
  return m?.[1] ?? null;
}

/** What a failed job lacks (`GET /api/jobs/:id/tools`): the commands its failed steps' logs say weren't there. The mock's runners meet every job's `requires`, and it starts no agent to install anything. */
function lacks(sj: SimJob): JobTools["missing"] {
  const out: JobTools["missing"] = { tools: [], on: null, why: null, no_agent: null, runners: [] };
  if (sj.job.status !== "failed") return out;
  for (const st of sj.steps.filter((x) => x.status === "failed")) {
    for (const raw of getStream(sj, st.idx).data.split("\n").slice(-400)) {
      const line = plain(raw).trim();
      const t = notFound(line);
      if (t && !out.tools.includes(t)) {
        out.tools.push(t);
        out.why = line;
      }
    }
  }
  if (out.tools.length) {
    out.on = sj.job.agent;
    // A pod's tools are its image's; the mock's other runners host no agent it could start.
    out.no_agent = sj.job.executor === "k8s" ? `it ran in a pod: add ${out.tools.join(", ")} to the job's image or toolchain` : "the mock starts no agents";
  }
  return out;
}

function json(res: http.ServerResponse, status: number, body: unknown) {
  const s = JSON.stringify(body);
  res.writeHead(status, { "content-type": "application/json", "cache-control": "no-store" });
  res.end(s);
}
function noContent(res: http.ServerResponse) {
  res.writeHead(204);
  res.end();
}
function err(res: http.ServerResponse, status: number, message: string) {
  json(res, status, { error: message });
}

async function body<T>(req: http.IncomingMessage): Promise<T> {
  const chunks: Buffer[] = [];
  for await (const c of req) chunks.push(c as Buffer);
  const s = Buffer.concat(chunks).toString("utf8");
  return (s ? JSON.parse(s) : {}) as T;
}

function median(xs: number[]) {
  if (!xs.length) return 0;
  const s = [...xs].sort((a, b) => a - b);
  return s[Math.floor(s.length / 2)]!;
}

function overview() {
  const since = now() - 86400;
  const recent = runs.filter((r) => r.created_at >= since);
  const jobs = recent.flatMap((r) => jobsByRun.get(r.id) ?? []).map((s) => s.job);
  const jobsByStatus: Partial<Record<Status, number>> = {};
  for (const j of jobs) jobsByStatus[j.status] = (jobsByStatus[j.status] ?? 0) + 1;
  const hourStart = Math.floor(now() / 3600) * 3600;
  const hourly = Array.from({ length: 24 }, (_, i) => {
    const ts = hourStart - (23 - i) * 3600;
    const inHour = recent.filter((r) => r.created_at >= ts && r.created_at < ts + 3600);
    const done = inHour.filter((r) => r.finished_at && r.started_at);
    return {
      ts,
      succeeded: inHour.filter((r) => r.status === "succeeded").length,
      failed: inHour.filter((r) => r.status === "failed" || r.status === "timed_out").length,
      avg_seconds: done.length ? Math.round(done.reduce((s, r) => s + (r.finished_at! - r.started_at!), 0) / done.length) : 0,
    };
  });
  const connected = [...agents.values()].filter((a) => a.connected);
  return {
    stats: {
      runs_24h: recent.length,
      runs_failed_24h: recent.filter((r) => r.status === "failed" || r.status === "timed_out").length,
      runs_succeeded_24h: recent.filter((r) => r.status === "succeeded").length,
      median_job_seconds: median(jobs.filter((j) => j.started_at && j.finished_at).map((j) => j.finished_at! - j.started_at!)),
      jobs_by_status: jobsByStatus,
      hourly,
    },
    queue: queueCounts(),
    agents: { connected: connected.length, total: agents.size, slots: connected.reduce((s, a) => s + a.slots, 0), busy: connected.reduce((s, a) => s + a.busy, 0) },
    system: { version: VERSION, uptime_s: Math.floor((Date.now() - STARTED) / 1000) + 86_000, hostname: HOSTNAME, generation, children, sample: history.get("server")?.at(-1) ?? null },
  };
}

const MIME: Record<string, string> = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript",
  ".css": "text/css",
  ".svg": "image/svg+xml",
  ".woff2": "font/woff2",
  ".woff": "font/woff",
  ".wasm": "application/wasm",
  ".png": "image/png",
  ".json": "application/json",
  ".webmanifest": "application/manifest+json",
  ".ico": "image/x-icon",
};

function serveStatic(pathname: string, res: http.ServerResponse) {
  if (!existsSync(DIST)) {
    res.writeHead(200, { "content-type": "text/plain" });
    res.end(`kiln mock API on :${PORT}. Run the UI with \`npm run dev\` (or build to serve dist/ here).\n`);
    return;
  }
  const safe = normalize(pathname).replace(/^(\.\.[/\\])+/, "");
  let file = join(DIST, safe);
  if (!file.startsWith(DIST) || !existsSync(file) || statSync(file).isDirectory()) file = join(DIST, "_shell.html");
  const ext = extname(file);
  res.writeHead(200, {
    "content-type": MIME[ext] ?? "application/octet-stream",
    "cache-control": file.includes(`${DIST}/assets/`) ? "public, max-age=31536000, immutable" : "no-cache",
  });
  res.end(readFileSync(file));
}

const server = http.createServer(async (req, res) => {
  try {
    const url = new URL(req.url ?? "/", `http://${req.headers.host ?? "localhost"}`);
    const p = url.pathname;
    const m = req.method ?? "GET";

    if (p === "/healthz" || p === "/readyz") return void res.end("ok\n");
    if (p === "/metrics") {
      const q = queueCounts();
      res.writeHead(200, { "content-type": "text/plain; version=0.0.4" });
      return void res.end(
        [
          "# HELP kiln_queue_depth Jobs waiting for a slot.",
          "# TYPE kiln_queue_depth gauge",
          `kiln_queue_depth ${q.queued}`,
          "# TYPE kiln_jobs_running gauge",
          `kiln_jobs_running ${q.running}`,
          "# TYPE kiln_agents_connected gauge",
          `kiln_agents_connected ${[...agents.values()].filter((a) => a.connected).length}`,
          "",
        ].join("\n"),
      );
    }
    // Pages agents show: open, as the server's are (their address is their key).
    if (handleShownPages(p, res)) return;
    if (!p.startsWith("/api/")) return serveStatic(p, res);

    // ---- auth (open) ----
    if (p === "/api/auth/login" && m === "POST") {
      const b = await body<{ token?: string }>(req);
      if (b.token !== TOKEN) return err(res, 401, "invalid token");
      newSession(res);
      addAudit("admin", "auth.login", `token login from ${req.socket.remoteAddress}`);
      return noContent(res);
    }
    if (p === "/api/auth/otp" && m === "GET") {
      if (url.searchParams.get("code") !== OTP) return err(res, 401, "invalid or expired code");
      newSession(res);
      addAudit("admin", "auth.otp", "one-time login URL used");
      res.writeHead(302, { location: "/" });
      return void res.end();
    }
    if (p === "/api/auth/logout" && m === "POST") {
      const sid = parseCookies(req.headers.cookie).kiln_session;
      if (sid) sessions.delete(sid);
      res.setHeader("set-cookie", "kiln_session=; Path=/; Max-Age=0");
      return noContent(res);
    }
    // Open (rate-limited on the server): what an invited machine polls before it asks to join.
    if (p === "/api/join/invited" && m === "GET") {
      const i = instances.get(url.searchParams.get("instance") ?? "");
      return json(res, 200, { invited: Boolean(i?.invited), ca: SERVER_CA });
    }
    if (!authed(req)) return err(res, 401, "unauthorized");

    if (p === "/api/me") return json(res, 200, { authenticated: true, version: VERSION, hostname: HOSTNAME });
    // The browser checks send the long session's machine away for a while (`?ms=`).
    if (p === "/api/mock/away" && m === "POST") {
      sendAway(Number(url.searchParams.get("ms") ?? 10_000));
      return json(res, 200, { ok: true });
    }
    // ---- files the long session's messages mention ----
    if (handleLongFiles(p, url, req, res)) return;
    // ---- live pages a person opens (the Tools tab): what its machine serves; no streams here ----
    const live = /^\/api\/harness\/sessions\/([^/]+)\/(preview|preview\/stop|streams)$/.exec(p);
    if (live) {
      if (live[2] === "preview" && m === "GET")
        return json(res, 200, {
          ports: [
            { port: 5173, address: "127.0.0.1", pid: 4242, command: "node", agents: true },
            { port: 8080, address: "0.0.0.0", pid: 977, command: "python3", agents: false },
          ],
          app: { configured: true, start: "npm run dev", port: 5173, path: "/", running: null },
        });
      if (live[2] === "preview/stop" && m === "POST") return json(res, 200, { stopped: false });
      if (live[2] === "streams" && m === "GET") return json(res, 200, []);
      if (live[2] === "streams" && m === "POST") return err(res, 409, "the mock doesn't stream pages: run a kiln server and a runner for live pages");
    }
    if (/^\/api\/harness\/sessions\/[^/]+\/recordings$/.test(p) && m === "GET") return json(res, 200, []);
    const opened = /^\/api\/harness\/sessions\/([^/]+)\/opened$/.exec(p);
    if (opened && m === "GET") return json(res, 200, []);
    if (opened && m === "POST") return err(res, 409, "opening an app on your device needs pages' own site: set [server] pages_url (docs/previews.md)");
    // ---- goal stacks ----
    if (await handleGoals(req, res, url)) return;
    if (await handlePrograms(req, res, url)) return;
    // ---- agents that run agents ----
    if (await handleShip(req, res, url, { json, liveJob })) return;
    if (await handleFamily(req, res, url, () => [longInfo(), ...fastInfos()])) return;
    // ---- harness accounts (and the demo agents to see them from) ----
    if (await handleMachines(req, res, url, { json, body })) return;
    // ---- copies runners keep of other runners' trees (docs/reach.md §5.1) ----
    if (await handleKeep(req, res, url, { json, body })) return;
    // ---- the reaper's policies (W24-R31) ----
    if (await handleReap(req, res, url, { json, body })) return;
    if (await handleAccounts(req, res, url)) return;
    if (p === "/api/overview") return json(res, 200, overview());

    // ---- repos ----
    if (p === "/api/repos" && m === "GET") return json(res, 200, repos);
    if (p === "/api/repos" && m === "POST") {
      const b = await body<{ name?: string; clone_url?: string; branches?: string[]; poll_seconds?: number }>(req);
      if (!b.name || !/^[\w.-]+\/[\w.-]+$/.test(b.name)) return err(res, 400, "name must be owner/repo");
      if (repos.some((r) => r.name === b.name)) return err(res, 409, `${b.name} already exists`);
      const repo: Repo = {
        id: rid("repo_"),
        name: b.name,
        clone_url: b.clone_url || `https://github.com/${b.name}.git`,
        branches: b.branches?.length ? b.branches : ["main"],
        poll_seconds: b.poll_seconds ?? 60,
        enabled: true,
        created_at: now(),
        last_polled_at: null,
        last_error: null,
      };
      repos.push(repo);
      repoTc.set(repo.id, "rust");
      addAudit("admin", "repo.add", repo.name);
      setTimeout(() => (repo.last_polled_at = now()), 2000);
      return json(res, 200, repo);
    }
    let mm: RegExpMatchArray | null;
    if ((mm = p.match(/^\/api\/repos\/([^/]+)$/)) && m === "DELETE") {
      const i = repos.findIndex((r) => r.id === decodeURIComponent(mm![1]!));
      if (i === -1) return err(res, 404, "no such repo");
      addAudit("admin", "repo.delete", repos[i]!.name);
      repos.splice(i, 1);
      return noContent(res);
    }
    if ((mm = p.match(/^\/api\/repos\/([^/]+)\/poll$/)) && m === "POST") {
      const r = repos.find((x) => x.id === decodeURIComponent(mm![1]!));
      if (!r) return err(res, 404, "no such repo");
      r.last_polled_at = now();
      if (r.last_error && rnd() < 0.5) r.last_error = null;
      addAudit("admin", "repo.poll", r.name);
      return noContent(res);
    }
    if ((mm = p.match(/^\/api\/repos\/([^/]+)\/trigger$/)) && m === "POST") {
      const r = repos.find((x) => x.id === decodeURIComponent(mm![1]!));
      if (!r) return err(res, 404, "no such repo");
      const b = await body<{ branch?: string; pipeline?: string }>(req);
      const run = startRun(r, { branch: b.branch || undefined, event: "manual", pipeline: b.pipeline || undefined });
      addAudit("admin", "run.trigger", `${r.name}@${run.branch} #${run.number}`);
      return json(res, 200, [run]);
    }

    // ---- runs ----
    if (p === "/api/runs" && m === "GET") {
      const repo = url.searchParams.get("repo");
      const limit = Math.min(500, Number(url.searchParams.get("limit") ?? 50) || 50);
      const before = url.searchParams.get("before");
      let list = runs;
      if (repo) list = list.filter((r) => r.repo_id === repo || r.repo_name === repo);
      if (before) list = list.filter((r) => r.created_at < Number(before));
      return json(res, 200, list.slice(0, limit));
    }
    if ((mm = p.match(/^\/api\/runs\/([^/]+)$/)) && m === "GET") {
      const run = runs.find((r) => r.id === decodeURIComponent(mm![1]!));
      if (!run) return err(res, 404, "no such run");
      return json(res, 200, { run, jobs: (jobsByRun.get(run.id) ?? []).map((s) => s.job) });
    }
    if ((mm = p.match(/^\/api\/runs\/([^/]+)\/cancel$/)) && m === "POST") {
      const run = runs.find((r) => r.id === decodeURIComponent(mm![1]!));
      if (!run) return err(res, 404, "no such run");
      for (const sj of jobsByRun.get(run.id) ?? []) {
        if (["pending", "queued", "running"].includes(sj.job.status)) {
          sj.steps.forEach((s) => {
            if (["pending", "queued", "running"].includes(s.status)) {
              if (s.status === "running") {
                s.finished_at = now();
                appendLog(sj, s.idx, "\n\x1b[1;33m── cancelled (SIGTERM to process group)\x1b[0m\n");
                endLog(sj, s.idx);
              }
              s.status = "cancelled";
            }
          });
          finishJob(sj, "cancelled", "cancelled by admin");
        }
      }
      run.status = "cancelled";
      run.finished_at = now();
      run.error = "cancelled by admin";
      emit({ type: "run", run });
      addAudit("admin", "run.cancel", `${run.repo_name} #${run.number}`);
      return noContent(res);
    }
    if ((mm = p.match(/^\/api\/runs\/([^/]+)\/retry$/)) && m === "POST") {
      const run = runs.find((r) => r.id === decodeURIComponent(mm![1]!));
      if (!run) return err(res, 404, "no such run");
      const b = await body<{ only_failed?: boolean }>(req);
      const sjobs = jobsByRun.get(run.id) ?? [];
      const reset = (sj: SimJob) => {
        sj.job.status = "pending";
        sj.job.attempt += 1;
        Object.assign(sj.job, { agent: null, executor: null, queued_at: null, started_at: null, finished_at: null, exit_code: null, error: null, peak_rss: null, cpu_seconds: null });
        sj.failStep = rnd() < 0.3 ? sj.steps.length - 1 : null;
        sj.steps.forEach((s) => {
          Object.assign(s, { status: "pending", started_at: null, finished_at: null, exit_code: null, emitted: 0, plan: undefined });
          logs.delete(logKey(sj.job.id, s.idx));
        });
        syncJob(sj);
        emit({ type: "job", job: sj.job });
      };
      const bad = new Set(sjobs.filter((s) => ["failed", "timed_out", "cancelled"].includes(s.job.status)).map((s) => s.tpl.key));
      // also reset dependents of failed jobs
      let grew = true;
      while (grew) {
        grew = false;
        for (const s of sjobs) {
          if (!bad.has(s.tpl.key) && s.tpl.needs.some((n) => bad.has(n))) {
            bad.add(s.tpl.key);
            grew = true;
          }
        }
      }
      for (const sj of sjobs) if (!b.only_failed || bad.has(sj.tpl.key)) reset(sj);
      run.status = "pending";
      run.finished_at = null;
      run.error = null;
      if (!b.only_failed) run.started_at = null;
      emit({ type: "run", run });
      addAudit("admin", b.only_failed ? "run.retry_failed" : "run.retry", `${run.repo_name} #${run.number}`);
      return noContent(res);
    }

    // ---- jobs ----
    if ((mm = p.match(/^\/api\/jobs\/([^/]+)$/)) && m === "GET") {
      const sj = jobsById.get(decodeURIComponent(mm[1]!));
      if (!sj) return err(res, 404, "no such job");
      return json(res, 200, { ...sj.job, spec: sj.planned });
    }
    if ((mm = p.match(/^\/api\/jobs\/([^/]+)\/logs$/)) && m === "GET") {
      const sj = jobsById.get(decodeURIComponent(mm[1]!));
      if (!sj) return err(res, 404, "no such job");
      const idx = Number(url.searchParams.get("step") ?? 0);
      if (!sj.steps[idx]) return err(res, 404, "no such step");
      res.writeHead(200, { "content-type": "text/plain; charset=utf-8" });
      return void res.end(getStream(sj, idx).data);
    }
    if ((mm = p.match(/^\/api\/jobs\/([^/]+)\/tools$/)) && m === "GET") {
      const sj = jobsById.get(decodeURIComponent(mm[1]!));
      if (!sj) return err(res, 404, "no such job");
      return json(res, 200, { job: sj.job.id, missing: lacks(sj), fix: null } satisfies JobTools);
    }

    // ---- agents ----
    if (p === "/api/conversations/search" && m === "GET") {
      const q = url.searchParams;
      return json(res, 200, { hits: searchSaid(q.get("q") ?? "", q.get("agent"), q.has("until") ? Number(q.get("until")) : null, Number(q.get("limit") ?? 100)) });
    }
    if (p === "/api/agents" && m === "GET") return json(res, 200, [...agents.values()]);
    if (p === "/api/agents/join" && m === "GET") {
      planAutojoin();
      return json(res, 200, joinInfo());
    }

    // ---- joining and discovery ----
    if (p === "/api/join-requests" && m === "GET") {
      return json(res, 200, [...joinRequests.values()].sort((a, b) => a.created_at - b.created_at));
    }
    if ((mm = p.match(/^\/api\/join-requests\/([^/]+)\/(approve|deny)$/)) && m === "POST") {
      const jr = joinRequests.get(decodeURIComponent(mm[1]!));
      if (!jr || jr.expires_at <= now()) return err(res, 404, "no such join request (it may have expired)");
      if (jr.status !== "pending") return err(res, 409, `that request was already ${jr.status}`);
      const inst = [...instances.values()].find((i) => i.request === jr.id);
      if (mm[2] === "deny") {
        jr.status = "denied";
        if (inst) Object.assign(inst, { request: null, invited: false });
        addAudit("admin", "join.deny", `${jr.name} from ${jr.address}`);
        emit({ type: "join_requests" });
        emit({ type: "discovery" });
        return json(res, 200, { id: jr.id, status: "denied", name: jr.name });
      }
      if (!jr.code) return err(res, 409, "the machine has not confirmed its request yet");
      const b = await body<{ name?: string; labels?: string[] }>(req);
      const base = (b.name || jr.name).toLowerCase().replace(/[^a-z0-9-]/g, "-");
      let name = base;
      for (let n = 2; agents.has(name); n++) name = `${base}-${n}`;
      const labels = b.labels?.length ? b.labels : jr.labels;
      jr.status = "approved";
      jr.agent = name;
      if (inst) Object.assign(inst, { request: null, invited: false, enrolled: SERVER_CA });
      addAudit("admin", "join.approve", `${name} from ${jr.address}`);
      emit({ type: "join_requests" });
      emit({ type: "discovery" });
      // The machine picks up its certificate on its next poll, installs its runner and connects.
      setTimeout(() => {
        const a = addAgent({ name, labels, slots: 2, hostname: jr.hostname, os: jr.os, arch: jr.arch, version: jr.version || VERSION, ephemeral: false, connected: true, last_seen: now(), enrolled_at: now() }, newHost(8, 32, 1000));
        emit({ type: "agent", agent: a });
        emit({ type: "discovery" });
      }, 2500);
      return json(res, 200, { id: jr.id, status: "approved", name, labels });
    }
    if (p === "/api/discovery" && m === "GET") return json(res, 200, discoveryInfo());
    if (p === "/api/discovery/scan" && m === "POST") {
      if (scan?.running) return err(res, 409, `a scan of ${scan.cidrs.join(", ")} is running`);
      const b = await body<{ cidrs?: string[] }>(req);
      const cidrs = b.cidrs?.length ? b.cidrs.map((c) => c.trim()).filter(Boolean) : SUBNETS;
      let total = 0;
      for (const c of cidrs) {
        const n = cidrSize(c);
        if (typeof n === "string") return err(res, 400, n);
        total += n;
      }
      addAudit("admin", "discovery.scan", cidrs.join(", "));
      const s = startScan(cidrs, total);
      emit({ type: "discovery" });
      return json(res, 202, s);
    }
    if (p === "/api/discovery/probe" && m === "POST") {
      const b = await body<{ address?: string }>(req);
      const address = (b.address ?? "").trim();
      if (!address) return err(res, 400, "address is required");
      const host = address.replace(/^\[(.*)\](:\d+)?$/, "$1").replace(/:\d+$/, "").toLowerCase();
      if (host.endsWith(".invalid")) return err(res, 400, `cannot resolve ${host}`);
      await sleep(600 + rnd() * 900);
      let found = [...instances.values()].filter((i) => i.addresses.includes(host) || [i.name, i.name.replace(/\.lan$/, "")].includes(host));
      if (!found.length && (PROBE_FIND.addresses.includes(host) || host === PROBE_FIND.name)) {
        const i = seedInstance(PROBE_FIND);
        instances.set(i.id, i);
        found = [i];
      }
      for (const i of found) {
        if (!i.via.includes("probe")) i.via.push("probe");
        i.last_seen = now();
      }
      if (found.length) emit({ type: "discovery" });
      return json(res, 200, { found: found.map(presentInstance) });
    }
    if (p === "/api/discovery/invite" && m === "POST") {
      const b = await body<{ instance?: string }>(req);
      const inst = instances.get(b.instance ?? "");
      if (!inst) return err(res, 404, "no such instance");
      if (inst.role !== "agent" || inst.enrolled) return err(res, 409, `${inst.name} is already set up`);
      inst.invited = true;
      addAudit("admin", "discovery.invite", inst.name);
      emit({ type: "discovery" });
      // It polls GET /api/join/invited, sees the invitation and asks to join like `kiln join`.
      setTimeout(() => {
        if (!inst.invited || inst.request || inst.enrolled) return;
        addJoinRequest(requestFrom(inst), { code: false, instance: inst.id, confirmMs: 2500 });
        emit({ type: "join_requests" });
        emit({ type: "discovery" });
      }, 6000);
      return json(res, 200, { invited: true });
    }
    if ((mm = p.match(/^\/api\/discovery\/invite\/([^/]+)$/)) && m === "DELETE") {
      const inst = instances.get(decodeURIComponent(mm[1]!));
      if (!inst) return err(res, 404, "no such instance");
      inst.invited = false;
      emit({ type: "discovery" });
      return noContent(res);
    }
    if ((mm = p.match(/^\/api\/agents\/([^/]+)$/)) && m === "DELETE") {
      const name = decodeURIComponent(mm[1]!);
      if (!agents.delete(name)) return err(res, 404, "no such agent");
      addAudit("admin", "agent.forget", name);
      return noContent(res);
    }
    if ((mm = p.match(/^\/api\/agents\/([^/]+)\/drain$/)) && m === "POST") {
      const a = agents.get(decodeURIComponent(mm[1]!));
      if (!a) return err(res, 404, "no such agent");
      const b = await body<{ draining?: boolean }>(req);
      a.draining = Boolean(b.draining);
      emit({ type: "agent", agent: a });
      addAudit("admin", a.draining ? "agent.drain" : "agent.undrain", a.name);
      return noContent(res);
    }

    // ---- folders on machines (the folder picker) ----
    if ((mm = p.match(/^\/api\/machines\/([^/]+)\/dirs$/)) && m === "GET") {
      const target = decodeURIComponent(mm[1]!);
      const path = url.searchParams.get("path") ?? "";
      let os: "linux" | "macos" = "linux";
      let wait = between(20, 60);
      if (target !== "local") {
        const name = target.replace(/^agent:/, "");
        const a = agents.get(name);
        if (!a || !a.connected) return err(res, 409, `${name} is not connected`);
        if (a.ephemeral) return err(res, 403, `${name} has terminals (and so agents) turned off`);
        // Runners older than this server never announced that they list folders.
        if (a.version !== VERSION) return err(res, 409, `${name}'s runner is too old to list folders: upgrade it`);
        if (a.os === "macos") os = "macos";
        // A machine answers over its QUIC session; its network mount answers slowly.
        wait = /^\/mnt\/nfs(\/|$)/.test(path.trim()) ? 2500 : between(90, 280);
      }
      await new Promise((r) => setTimeout(r, wait));
      return json(res, 200, mockDirList(os, path));
    }

    // ---- metrics / system ----
    if (p === "/api/metrics/history" && m === "GET") {
      const agent = url.searchParams.get("agent");
      if (agent) return json(res, 200, { [agent]: history.get(agent) ?? [] });
      return json(res, 200, Object.fromEntries(history));
    }
    if (p === "/api/system" && m === "GET") {
      return json(res, 200, {
        version: VERSION,
        uptime_s: Math.floor((Date.now() - STARTED) / 1000) + 86_000,
        generation,
        children,
        crash_log: crashLog,
        config: {
          listen: { http: "0.0.0.0:7070", quic: "0.0.0.0:7443" },
          data_dir: "/var/lib/kiln",
          admin_token: "<redacted>",
          github: { token: "<redacted>", webhook_secret: "<redacted>", poll_default_seconds: 60 },
          scheduler: { max_queue: 10000, requeue_on_agent_loss: true, default_timeout_minutes: 60 },
          kubernetes: { enabled: true, namespace: "kiln-jobs", max_pods: 6, image: "ghcr.io/hyper-light/kiln:0.1.0" },
          telemetry: { sample_interval_ms: 5000, history_minutes: 60 },
          raft: { enabled: false, peers: [] },
          tls: { cert: "/etc/kiln/tls/server.crt", key: "<redacted>" },
        },
      });
    }
    if (p === "/api/audit" && m === "GET") {
      const limit = Number(url.searchParams.get("limit") ?? 200) || 200;
      return json(res, 200, audit.slice(0, limit));
    }
    if (p.startsWith("/api/hooks/github")) return noContent(res);
    if (await handleReview(req, res, url, { json, err, body, emit: (ev) => emit(ev as never) })) return;
    if (await handlePush(req, res, url, { json, err, body, emit, noContent })) return;
    if (await handleDoctor(req, res, url, { json, err, body, runners: () => [...agents.values()].map((a) => ({ name: a.name, connected: a.connected, os: a.os })) })) return;
    if (await handleStores(req, res, url, { json, err, runners: () => [...agents.values()].filter((a) => !a.ephemeral).map((a) => ({ name: a.name, connected: a.connected, version: a.version, last_seen: a.last_seen })) })) return;
    if (await handlePlaces(req, res, url, { agents: () => [...agents.values()], repos: () => repos, emit, json, err, body })) return;
    if (await handleSettings(req, res, url, { json, err, body })) return;
    if (
      await handlePages(req, res, url, {
        agents: () => [...agents.values()],
        runs: () => runs,
        version: VERSION,
        hostname: HOSTNAME,
        json,
        err,
        body,
        emit,
        release: {
          start: (commit) => startRelease(commit),
          latest: () => (releaseRun ? { run: releaseRun, jobs: (jobsByRun.get(releaseRun.id) ?? []).map((s) => s.job) } : null),
          past: () => pastReleases,
        },
      })
    )
      return;

    return err(res, 404, `no route for ${m} ${p}`);
  } catch (e) {
    console.error(e);
    if (!res.headersSent) err(res, 500, String(e));
  }
});

/* ------------------------------------------------------------------ */
/* WebSockets                                                          */
/* ------------------------------------------------------------------ */

const wss = new WebSocketServer({ noServer: true });

type PtyMod = typeof import("node-pty");
let ptyMod: PtyMod | null = null;
try {
  const mod = (await import("node-pty")) as PtyMod & { default?: PtyMod };
  ptyMod = mod.default ?? mod;
} catch (e) {
  console.warn("[mock] node-pty unavailable, terminal falls back to echo:", (e as Error).message);
}

server.on("upgrade", (req, socket, head) => {
  const url = new URL(req.url ?? "/", "http://x");
  if (!authed(req)) {
    socket.write("HTTP/1.1 401 Unauthorized\r\nconnection: close\r\n\r\n");
    socket.destroy();
    return;
  }
  wss.handleUpgrade(req, socket, head, (ws) => {
    const p = url.pathname;
    if (p === "/api/events") return handleEvents(ws);
    const lm = p.match(/^\/api\/jobs\/([^/]+)\/logs\/ws$/);
    if (lm) return handleLogs(ws, decodeURIComponent(lm[1]!), Number(url.searchParams.get("step") ?? 0));
    if (p === "/api/terminal/ws") return handleTerminal(ws, url.searchParams);
    const hm = p.match(/^\/api\/harness\/sessions\/([^/]+)\/ws$/);
    if (
      hm &&
      (handleDemoSession(ws, decodeURIComponent(hm[1]!)) ||
        handleFamilySession(ws, decodeURIComponent(hm[1]!)) ||
        handleFastSession(ws, decodeURIComponent(hm[1]!), url.searchParams.get("patch") === "state") ||
        handleLongSession(ws, decodeURIComponent(hm[1]!), url.searchParams.get("z") === "gzip", url.searchParams.get("patch") === "state"))
    )
      return;
    ws.close(1008, "no such socket");
  });
});

function handleEvents(ws: WebSocket) {
  eventClients.add(ws);
  ws.send(JSON.stringify({ type: "queue", ...queueCounts() }));
  ws.on("close", () => eventClients.delete(ws));
}

function handleLogs(ws: WebSocket, jobId: string, step: number) {
  const sj = jobsById.get(jobId);
  if (!sj || !sj.steps[step]) {
    ws.send(JSON.stringify({ type: "end" }));
    ws.close(1000);
    return;
  }
  const s = getStream(sj, step);
  if (s.data) ws.send(Buffer.from(s.data, "utf8"), { binary: true });
  if (s.done) {
    ws.send(JSON.stringify({ type: "step", idx: step, status: sj.steps[step]!.status }));
    ws.send(JSON.stringify({ type: "end" }));
    ws.close(1000);
    return;
  }
  s.subs.add(ws);
  ws.on("close", () => s.subs.delete(ws));
}

function handleTerminal(ws: WebSocket, params: URLSearchParams) {
  const target = params.get("target") ?? "local";
  const cols = Math.max(2, Math.min(1000, Number(params.get("cols")) || 120));
  const rows = Math.max(1, Math.min(500, Number(params.get("rows")) || 40));
  if (target !== "local") {
    const name = target.replace(/^agent:/, "");
    const a = agents.get(name);
    if (!a || !a.connected) {
      ws.send(JSON.stringify({ type: "error", message: `agent ${name} is not connected` }));
      ws.close(1000);
      return;
    }
  }
  addAudit("admin", "terminal.open", target);
  const banner = target === "local" ? "" : `\x1b[2m[mock] ${target} → local shell on ${os.hostname()}\x1b[0m\r\n`;
  // Which session this socket is on (the server's first frame on every attach).
  ws.send(
    JSON.stringify({
      type: "session",
      session: {
        id: params.get("session") ?? rid("ts_"),
        target,
        command: null,
        title: "",
        name: null,
        created_at: now(),
        attached: 1,
        cols,
        rows,
        idle_seconds: 0,
        exited: false,
        exit_code: null,
        error: null,
      },
    }),
  );

  if (!ptyMod) {
    // Echo terminal fallback
    ws.send(Buffer.from(`${banner}\x1b[2mkiln mock echo terminal (node-pty unavailable)\x1b[0m\r\n$ `), { binary: true });
    ws.on("message", (data, isBinary) => {
      if (!isBinary) return;
      const s = data.toString();
      if (s === "\x04") {
        ws.send(JSON.stringify({ type: "exit", code: 0 }));
        return ws.close(1000);
      }
      ws.send(Buffer.from(s.replace(/\r/g, "\r\n$ ")), { binary: true });
    });
    return;
  }

  const shell = process.env.SHELL && existsSync(process.env.SHELL) ? process.env.SHELL : "/bin/bash";
  const term = ptyMod.spawn(shell, [], {
    name: "xterm-256color",
    cols,
    rows,
    cwd: os.homedir(),
    env: { ...process.env, TERM: "xterm-256color", COLORTERM: "truecolor", KILN_TARGET: target } as Record<string, string>,
  });
  if (banner) ws.send(Buffer.from(banner), { binary: true });
  term.onData((d) => {
    if (ws.readyState === WebSocket.OPEN) ws.send(Buffer.from(d, "utf8"), { binary: true });
  });
  term.onExit(({ exitCode }) => {
    if (ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify({ type: "exit", code: exitCode }));
      ws.close(1000);
    }
  });
  ws.on("message", (data, isBinary) => {
    if (isBinary) {
      term.write((data as Buffer).toString("utf8"));
      return;
    }
    try {
      const msg = JSON.parse(data.toString()) as { type: string; cols?: number; rows?: number };
      if (msg.type === "resize" && msg.cols && msg.rows) term.resize(Math.max(2, msg.cols), Math.max(1, msg.rows));
    } catch {
      /* ignore */
    }
  });
  ws.on("close", () => {
    try {
      term.kill();
    } catch {
      /* already gone */
    }
  });
}

server.listen(PORT, HOST, () => {
  console.log(`\n  kiln mock server  http://${HOST}:${PORT}`);
  console.log(`  admin token       ${NO_AUTH ? "(auth disabled)" : TOKEN}`);
  console.log(`  one-time login    http://localhost:5173/api/auth/otp?code=${OTP}  (dev server)`);
  console.log(`                    http://${HOST}:${PORT}/api/auth/otp?code=${OTP}  (dist served here)`);
  console.log(`  terminal          ${ptyMod ? "node-pty (real PTY)" : "echo fallback"}`);
  console.log(`  runs              ${runs.length} historical, ${agents.size} agents, ${repos.length} repos\n`);
});


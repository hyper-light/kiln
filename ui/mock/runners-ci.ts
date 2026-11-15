/**
 * The Runners page's other panels in the mock, answered as the server's routes answer: CI's executor
 * pools and the runner images kiln built (`GET /api/ci/pools`, `GET /api/ci/images`; W13-R09, W13-R11),
 * and each runner's headroom (`GET|PUT /api/runners/:name/headroom`, `GET /api/headroom`;
 * docs/headroom.md), its settings kiln's defaults until a person saves their own.
 */
import type http from "node:http";
import type { CiImage, CiPool } from "../src/api/ci.ts";
import type { HeadroomSettings, HeadroomView } from "../src/api/types.ts";
import { MAC_MINI } from "./data.ts";

interface Ctx {
  json: (res: http.ServerResponse, status: number, body: unknown) => void;
  body: <T>(req: http.IncomingMessage) => Promise<T>;
}

const now = () => Math.floor(Date.now() / 1000);

const POOLS: CiPool[] = [
  {
    name: "linux-docker",
    executor: "docker",
    image: "ghcr.io/hyper-light/ci-rust:1.91",
    labels: ["linux", "x86_64"],
    hosts: null,
    min_idle: 1,
    max: 8,
    per_host_max: 4,
    idle_timeout_ms: 600_000,
    starting: 0,
    idle: 1,
    busy: 2,
    waiting: 0,
    executors: [
      { id: "ex-7d21", pool: "linux-docker", runner: "metal-ryzen-01", state: "busy", leg: "test (linux, x86_64)", since: now() - 240 },
      { id: "ex-9a03", pool: "linux-docker", runner: "metal-epyc-01", state: "busy", leg: "clippy", since: now() - 95 },
      { id: "ex-1c5e", pool: "linux-docker", runner: "metal-epyc-01", state: "idle", since: now() - 610 },
    ],
    last_error: null,
    source: "config",
  },
  {
    name: "mac-shards",
    executor: "shards",
    image: "kiln/ci-macos-arm64:2026.11",
    labels: ["macos", "arm64"],
    hosts: MAC_MINI,
    min_idle: 0,
    max: 2,
    per_host_max: 2,
    idle_timeout_ms: 300_000,
    starting: 0,
    idle: 0,
    busy: 1,
    waiting: 0,
    executors: [{ id: "ex-4f60", pool: "mac-shards", runner: MAC_MINI, state: "busy", leg: "test (macos, arm64)", since: now() - 130 }],
    last_error: null,
    source: "stored",
  },
];

const IMAGES: CiImage[] = [
  {
    id: "img-ci-rust-191",
    repo: "hyper-light/kiln",
    name: "ci-rust",
    reference: "ghcr.io/hyper-light/ci-rust:1.91",
    digest: "sha256:4e8c1f0a9b27d3e6",
    image: "ghcr.io/hyper-light/ci-rust@sha256:4e8c1f0a9b27d3e6",
    platforms: ["linux/amd64", "linux/arm64"],
    run_id: "run-1842",
    created_at: now() - 3 * 86_400,
  },
  {
    id: "img-ci-macos",
    repo: null,
    name: "ci-macos-arm64",
    reference: "kiln/ci-macos-arm64:2026.11",
    digest: "sha256:91d0e7c3a5b84f12",
    image: "kiln/ci-macos-arm64@sha256:91d0e7c3a5b84f12",
    platforms: ["linux/arm64"],
    run_id: null,
    created_at: now() - 9 * 86_400,
  },
];

/** kiln's defaults (crates/kiln/src/headroom/settings.rs): on, kiln's own targets, one false alarm a day, no cap of yours on the checks' share, escaped work paced. */
const DEFAULTS: HeadroomSettings = { enabled: true, targets: [], automatic: true, false_alarms_per_day: 1, probe_load_share: 1, escapes: true };
const saved = new Map<string, HeadroomSettings>();
const view = (runner: string): HeadroomView => ({
  runner,
  connected: runner !== "metal-nuc-old",
  supported: runner === "metal-nuc-old" ? false : true,
  settings: saved.get(runner) ?? DEFAULTS,
  customized: saved.has(runner),
  applied: saved.has(runner) ? true : null,
  status: null,
  status_at: null,
});
const RUNNERS = [MAC_MINI, "metal-ryzen-01", "metal-ryzen-02", "metal-epyc-01", "metal-pi-04", "metal-nuc-old"];

/** Handles the routes above; false when it doesn't know the route. */
export async function handleRunnersCi(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: Ctx): Promise<boolean> {
  const p = url.pathname;
  const m = req.method ?? "GET";
  if (p === "/api/ci/pools" && m === "GET") return (ctx.json(res, 200, { pools: POOLS }), true);
  if (p === "/api/ci/images" && m === "GET") return (ctx.json(res, 200, { images: IMAGES }), true);
  if (p === "/api/headroom" && m === "GET") return (ctx.json(res, 200, RUNNERS.map(view)), true);
  const one = p.match(/^\/api\/runners\/([^/]+)\/headroom$/);
  if (one) {
    const runner = decodeURIComponent(one[1]!);
    if (m === "GET") return (ctx.json(res, 200, view(runner)), true);
    if (m === "PUT") {
      saved.set(runner, await ctx.body<HeadroomSettings>(req));
      return (ctx.json(res, 200, view(runner)), true);
    }
  }
  return false;
}

/** The cluster picker's list (`GET /api/peers/open`; docs/peers.md): this cluster, peered with none in the mock. */
export function handlePeersOpen(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: Pick<Ctx, "json">): boolean {
  if (url.pathname !== "/api/peers/open" || (req.method ?? "GET") !== "GET") return false;
  ctx.json(res, 200, { name: "kiln", peers: [] });
  return true;
}

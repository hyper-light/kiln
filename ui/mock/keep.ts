/**
 * The copies runners keep of other runners' trees (GET /api/reach/keep, docs/reach.md §5.1): every
 * scope's rules, and two runners with copies, one of them offline, with what each let go.
 */
import type http from "node:http";
import type { AgentKeep, KeepCopy, KeepRules, KeepView } from "../src/api/types.ts";

interface Ctx {
  json: (res: http.ServerResponse, status: number, body: unknown) => void;
  body: (req: http.IncomingMessage) => Promise<unknown>;
}

const now = () => Math.floor(Date.now() / 1000);
const GiB = 1 << 30;

let system: KeepRules = { newest: 5, hours: 168, schedule: "0 3 * * *", free_percent: 10 };
const runners: Record<string, KeepRules> = { "fedora.attlocal.net": { free_gib: 200, beyond: true } };
const repos: Record<string, KeepRules> = { "github.com/hyper-light/kiln": { newest: 3 } };
const agents: Record<string, KeepRules> = {};

const copy = (from: string, key: string, daysAgo: number, gib: number | undefined, kept: boolean, why: string, extra: Partial<KeepCopy> = {}): KeepCopy => ({
  from,
  key,
  repo: "github.com/hyper-light/kiln",
  agents: ["a-mission"],
  created: now() - daysAgo * 86_400 - 3600,
  used: now() - daysAgo * 86_400,
  bytes: gib === undefined ? undefined : Math.round(gib * GiB),
  kept,
  why,
  ...extra,
});

function view(): KeepView {
  const t = now();
  return {
    now: t,
    rules: system,
    repos,
    runners: [
      {
        name: "adacachy",
        connected: true,
        rules: {},
        heard: t - 8,
        base: "/home/ada/.kiln/reach",
        disk: { total: 7300 * GiB, free: 3500 * GiB },
        copies: [
          copy("adas-macbook-pro-2.local", "slates-caf0fad34b", 0.02, 0.45, true, "in use now", { busy: true }),
          copy("adas-macbook-pro-2.local", "serve-cell-643fe04f09", 0.2, 1.6, true, "one of the newest 3 of github.com/hyper-light/kiln (github.com/hyper-light/kiln's rule)"),
          copy("fedora.attlocal.net", "kiln-devin-effort-23edca5702", 3, 11, true, "used 3 days ago, within 7 days (every runner's rule)"),
          copy("adas-macbook-pro-2.local", "data-layer-gate-9a49a6e6d1", 4, 8.5, true, "used 4 days ago, within 7 days (every runner's rule)"),
          copy(
            "adas-macbook-pro-2.local",
            "replicate-clean-58a932cbdf",
            9,
            undefined,
            false,
            "not one of the newest 3 of github.com/hyper-light/kiln (github.com/hyper-light/kiln's rule), and not used for 9 days (kept 7 days, every runner's rule)",
          ),
        ],
        gone: [
          {
            from: "adas-macbook-pro-2.local",
            key: "engine-cap-454e5daafc",
            repo: "github.com/hyper-light/kiln",
            bytes: 183 * 1024 * 1024,
            at: t - 5 * 3600,
            trigger: "schedule",
            why: "its schedule came round (0 3 * * * UTC, every runner's rule): not one of the newest 3 of github.com/hyper-light/kiln, and not used for 8 days (kept 7 days, every runner's rule)",
          },
        ],
        next: t + 13 * 3600,
        asked: 0,
        done: 0,
        now: { copies: 1, bytes: 0 },
        error: null,
      },
      {
        name: "fedora.attlocal.net",
        connected: false,
        rules: runners["fedora.attlocal.net"] ?? {},
        heard: t - 3 * 3600,
        base: "/home/ada/.kiln/reach",
        disk: { total: 7300 * GiB, free: 4300 * GiB },
        copies: [copy("adacachy", "reach-keep-src-ced59c34f7", 0.1, 0.4, true, "used 2 hours ago, within 7 days (every runner's rule)")],
        gone: [],
        next: t + 13 * 3600,
        asked: 0,
        done: 0,
        now: { copies: 0, bytes: 0 },
        error: null,
      },
      { name: "jupiter", connected: true, rules: {}, heard: null, base: null, disk: null, copies: [], gone: [], next: t + 13 * 3600, asked: 0, done: 0, now: { copies: 0, bytes: 0 }, error: null },
    ],
  };
}

/** Handles the routes above; false when it doesn't know the route. */
export async function handleKeep(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: Ctx): Promise<boolean> {
  const p = url.pathname;
  const m = req.method ?? "GET";
  let mm: RegExpMatchArray | null;
  const rules = async () => (m === "PUT" ? ((await ctx.body(req)) as KeepRules) : null);
  if (p === "/api/reach/keep" && m === "GET") return (ctx.json(res, 200, view()), true);
  if (p === "/api/reach/keep/rules" && (m === "PUT" || m === "DELETE")) {
    system = (await rules()) ?? {};
    return (ctx.json(res, 204, null), true);
  }
  if ((mm = p.match(/^\/api\/reach\/keep\/runners\/([^/]+)$/)) && (m === "PUT" || m === "DELETE")) {
    const r = await rules();
    if (r) runners[decodeURIComponent(mm[1])] = r;
    else delete runners[decodeURIComponent(mm[1])];
    return (ctx.json(res, 204, null), true);
  }
  if ((mm = p.match(/^\/api\/reach\/keep\/repos\/(.+)$/)) && (m === "PUT" || m === "DELETE")) {
    const r = await rules();
    if (r) repos[decodeURIComponent(mm[1])] = r;
    else delete repos[decodeURIComponent(mm[1])];
    return (ctx.json(res, 204, null), true);
  }
  if ((mm = p.match(/^\/api\/reach\/keep\/(?:runners\/([^/]+)\/)?now$/)) && m === "POST") return (ctx.json(res, 202, { asked: mm[1] ? [decodeURIComponent(mm[1])] : ["adacachy"] }), true);
  if ((mm = p.match(/^\/api\/harness\/sessions\/([^/]+)\/keep$/))) {
    const id = decodeURIComponent(mm[1]);
    if (m === "GET") {
      const k: AgentKeep = {
        rules: agents[id] ?? {},
        copies: view()
          .runners.flatMap((r) => r.copies.map((c) => ({ ...c, runner: r.name })))
          .slice(0, 2),
      };
      return (ctx.json(res, 200, k), true);
    }
    const r = await rules();
    if (r) agents[id] = r;
    else delete agents[id];
    return (ctx.json(res, 204, null), true);
  }
  return false;
}

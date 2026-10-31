/**
 * Experiments from an agent's question (docs/ci.md W20-R11): where one from `ask-s2`'s question
 * ("Which goes first, observe's gate or mine?") runs, CI's engine off (so no pipelines), with the
 * checks and metrics that agent suggested; and a start that answers as the server does, keeping
 * what it was sent for a browser test to read (`GET /api/mock/experiments`; `POST` forgets them).
 * Other agents' questions offer no experiment here.
 */
import type http from "node:http";

type Ctx = { json: (res: http.ServerResponse, status: number, body: unknown) => void; body: (req: http.IncomingMessage) => Promise<unknown> };

const ASKS = "ask-s2";
let started: unknown[] = [];

const where = () => ({
  repo: "hyper-light/kiln",
  branch: "main",
  machine: "adacachy",
  ci: false,
  workflows: [],
  may: true,
  suggested: [
    {
      question: "Which goes first, observe's gate or mine?",
      criteria: [
        { name: "gate", kind: "command", command: "ci/gate.sh --quick", required: true },
        { name: "wall_s", kind: "metric", metric: "wall_s", better: "lower", required: false, threshold: { better: 0.1 } },
      ],
      at: Math.floor(Date.now() / 1000) - 120,
    },
  ],
});

export async function handleExperiments(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: Ctx): Promise<boolean> {
  const p = url.pathname;
  if (p === "/api/experiments/where" && req.method === "GET") {
    if (url.searchParams.get("agent") !== ASKS) ctx.json(res, 404, { error: "its folder isn't a checkout of a repository kiln has" });
    else ctx.json(res, 200, where());
    return true;
  }
  if (p === "/api/experiments" && req.method === "POST") {
    const ask = (await ctx.body(req)) as Record<string, unknown>;
    started.push(ask);
    const id = `E${started.length}`;
    ctx.json(res, 202, { experiment: { id, repo: ask.repo, state: "building", variants: [], runs: [], criteria: ask.criteria ?? [], by: "ada", created_at: Math.floor(Date.now() / 1000) } });
    return true;
  }
  if (p === "/api/mock/experiments") {
    if (req.method === "POST") started = [];
    ctx.json(res, 200, started);
    return true;
  }
  return false;
}

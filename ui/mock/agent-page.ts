/**
 * The agent pages' own small reads in the mock, answered as the server answers them: voice input's
 * settings (`GET /api/voice`, off: no transcriber here), the terminals beside a session
 * (`GET /api/harness/sessions/:id/terminals`, none until one is opened from its composer), and the
 * machines whose AI sessions can't be read (`GET /api/ai-sessions/silent`, none).
 */
import type http from "node:http";

export function handleAgentPage(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: { json: (res: http.ServerResponse, status: number, body: unknown) => void }): boolean {
  if ((req.method ?? "GET") !== "GET") return false;
  if (url.pathname === "/api/voice") return (ctx.json(res, 200, { transcribe: false, language: "en" }), true);
  if (url.pathname === "/api/ai-sessions/silent") return (ctx.json(res, 200, { machines: [] }), true);
  if (/^\/api\/harness\/sessions\/[^/]+\/terminals$/.test(url.pathname)) return (ctx.json(res, 200, []), true);
  return false;
}

/**
 * A person's own secrets in the mock, as the server's routes answer (crates/kiln/src/secrets/mine.rs,
 * `/api/me/secrets`): names, sizes and times, never a value.
 *
 *   GET    /api/me/secrets         {secrets: [{name, size, created_at, updated_at, last_used_at}]}, by name
 *   PUT    /api/me/secrets/:name   {value}: set it (kept sealed; never shown again) → {set, name}
 *   DELETE /api/me/secrets/:name   → {deleted}
 */
import type http from "node:http";

interface Ctx {
  json: (res: http.ServerResponse, status: number, body: unknown) => void;
  body: <T>(req: http.IncomingMessage) => Promise<T>;
}
const now = () => Math.floor(Date.now() / 1000);
const NAME = /^[A-Za-z_][A-Za-z0-9_]*$/;
/** What's kept of each: its size and times (the mock keeps no value). */
const kept = new Map<string, { size: number; created_at: number; updated_at: number; last_used_at: number | null }>([
  ["GH_TOKEN", { size: 40, created_at: now() - 20 * 86400, updated_at: now() - 3 * 86400, last_used_at: now() - 3600 }],
  ["AWS_AGENTS_KEY_ID", { size: 20, created_at: now() - 9 * 86400, updated_at: now() - 9 * 86400, last_used_at: now() - 7200 }],
  ["AWS_AGENTS_SECRET", { size: 40, created_at: now() - 9 * 86400, updated_at: now() - 9 * 86400, last_used_at: now() - 7200 }],
  ["OPENAI_API_KEY", { size: 164, created_at: now() - 30 * 86400, updated_at: now() - 30 * 86400, last_used_at: null }],
]);

export async function handleMySecrets(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: Ctx): Promise<boolean> {
  const p = url.pathname;
  const m = req.method ?? "GET";
  if (p === "/api/me/secrets" && m === "GET") {
    const secrets = [...kept.entries()].sort(([a], [b]) => a.localeCompare(b)).map(([name, s]) => ({ name, ...s }));
    return (ctx.json(res, 200, { secrets }), true);
  }
  const one = p.match(/^\/api\/me\/secrets\/([^/]+)$/);
  if (!one) return false;
  const typed = decodeURIComponent(one[1]!).trim();
  const name = typed.toUpperCase();
  if (m === "PUT") {
    const b = await ctx.body<{ value?: unknown }>(req);
    if (typeof b.value !== "string") return (ctx.json(res, 400, { error: "`value` is required: the secret's value, kept sealed and never shown again" }), true);
    if (!NAME.test(typed)) return (ctx.json(res, 422, { error: `\`${typed}\` is not a valid name: alphanumeric characters or underscores only, and not starting with a number` }), true);
    if (name.startsWith("GITHUB_")) return (ctx.json(res, 422, { error: "Names must not start with GITHUB_." }), true);
    if (name.startsWith("KILN_")) return (ctx.json(res, 422, { error: "Names must not start with KILN_." }), true);
    const size = new TextEncoder().encode(b.value).length;
    if (size > 64 * 1024) return (ctx.json(res, 422, { error: "a secret is at most 64 KiB" }), true);
    const was = kept.get(name);
    kept.set(name, { size, created_at: was?.created_at ?? now(), updated_at: now(), last_used_at: was?.last_used_at ?? null });
    return (ctx.json(res, 200, { set: true, name }), true);
  }
  if (m === "DELETE") {
    const key = [...kept.keys()].find((k) => k === name);
    if (!key) return (ctx.json(res, 404, { error: `you have no secret ${name}` }), true);
    kept.delete(key);
    return (ctx.json(res, 200, { deleted: true }), true);
  }
  return false;
}

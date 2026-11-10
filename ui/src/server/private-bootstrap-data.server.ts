/** Server-only request-local reads: no application stores, credentials, or responses are cached. */
import type { AgentInfo } from "@/api/harness";
import type { Me } from "@/api/types";
import { needsSessions, privatePage, type PrivateBootstrap } from "@/lib/private-bootstrap";

const MAX_BYTES = 256 * 1024;
const DEADLINE_MS = 250;
const SECONDARY_MS = 75;

function identity(value: unknown): Me | null {
  if (!value || typeof value !== "object") return null;
  const v = value as Record<string, unknown>;
  if (typeof v.authenticated !== "boolean" || typeof v.version !== "string" || typeof v.hostname !== "string") return null;
  // Carry display/capability fields, never credentials or unneeded account/grant internals.
  const me: Me = {
    authenticated: v.authenticated,
    version: v.version,
    hostname: v.hostname,
  };
  if (v.kind === null || ["owner", "user", "pat", "agent", "job", "peer"].includes(String(v.kind))) me.kind = v.kind as Me["kind"];
  if (typeof v.public_url === "string") me.public_url = v.public_url;
  if (v.pages_url === null || typeof v.pages_url === "string") me.pages_url = v.pages_url;
  if (Array.isArray(v.origins) && v.origins.every((s) => typeof s === "string")) me.origins = v.origins;
  if (Array.isArray(v.groups) && v.groups.every((s) => typeof s === "string")) me.groups = v.groups;
  if (v.can && typeof v.can === "object" && !Array.isArray(v.can)) {
    me.can = Object.fromEntries(Object.entries(v.can).filter(([, scope]) => scope === "server" || scope === "some")) as Me["can"];
  }
  return me;
}

async function boundedJson(response: Response, budget: { left: number }): Promise<unknown> {
  if (!response.ok) {
    await response.body?.cancel();
    return null;
  }
  if (!response.body) return null;
  const length = Number(response.headers.get("content-length"));
  if (Number.isFinite(length) && length > budget.left) {
    await response.body.cancel();
    return null;
  }
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let text = "";
  try {
    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      budget.left -= value.byteLength;
      if (budget.left < 0) return null;
      text += decoder.decode(value, { stream: true });
    }
    return JSON.parse(text + decoder.decode()) as unknown;
  } finally {
    await reader.cancel().catch(() => {});
    reader.releaseLock();
  }
}

export async function readPrivateBootstrap(
  request: Request,
  pathname: string,
  options: {
    apiOrigin: string;
    fetch?: typeof fetch;
    timeoutMs?: number;
    maxBytes?: number;
  },
): Promise<PrivateBootstrap | null> {
  if (!privatePage(pathname) || new URL(request.url).pathname.startsWith("/.kiln-peer/")) return null;
  const headers = new Headers();
  for (const name of ["cookie", "authorization"]) {
    const value = request.headers.get(name);
    if (value) headers.set(name, value);
  }
  // A static prerender and an anonymous request make no private API requests. Client auth remains
  // the fallback, including servers configured to allow a browser without a session cookie.
  if (!headers.has("cookie") && !headers.has("authorization")) return null;
  let api: URL;
  try {
    api = new URL(options.apiOrigin);
  } catch {
    return null;
  }
  if (api.protocol !== "http:" && api.protocol !== "https:") return null;
  const signal = AbortSignal.any([request.signal, AbortSignal.timeout(options.timeoutMs ?? DEADLINE_MS)]);
  const budget = { left: options.maxBytes ?? MAX_BYTES };
  const fetcher = options.fetch ?? fetch;
  const read = async (path: string, readSignal = signal) => {
    const response = await fetcher(new URL(path, api), {
      headers,
      signal: readSignal,
      cache: "no-store",
      redirect: "error",
    });
    return boundedJson(response, budget);
  };
  try {
    const me = identity(await read("/api/me"));
    if (!me) return null;
    const bootstrap: PrivateBootstrap = { version: 1, pathname, me };
    if (me.authenticated && needsSessions(pathname)) {
      try {
        const sessions = await read("/api/harness/sessions", AbortSignal.any([signal, AbortSignal.timeout(SECONDARY_MS)]));
        if (Array.isArray(sessions) && sessions.every((s: unknown) => !!s && typeof s === "object" && typeof (s as { id?: unknown }).id === "string")) bootstrap.sessions = sessions as AgentInfo[];
      } catch {
        // A bounded/failed secondary read never loses an identity already verified for this page.
      }
    }
    return bootstrap;
  } catch {
    return null;
  }
}

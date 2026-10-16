import type {
  Agent,
  AuditEntry,
  DiscoveryInfo,
  Job,
  JoinInfo,
  JoinRequest,
  Me,
  MetricsHistory,
  NewRepo,
  Overview,
  Repo,
  Run,
  RunDetail,
  SystemInfo,
  TerminalSessionInfo,
} from "./types";

export class ApiError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.status = status;
    this.name = "ApiError";
  }
}

/* ---------------------------- auth state ---------------------------- */

type AuthListener = (authed: boolean) => void;
const authListeners = new Set<AuthListener>();
let authed: boolean | null = null;

export function onAuthChange(fn: AuthListener): () => void {
  authListeners.add(fn);
  return () => authListeners.delete(fn);
}
export function setAuthed(value: boolean) {
  if (authed === value) return;
  authed = value;
  authListeners.forEach((fn) => fn(value));
}
export function isAuthed() {
  return authed;
}

/* ---------------------------- core fetch ---------------------------- */

async function request<T>(
  method: string,
  path: string,
  body?: unknown,
  opts: { raw?: boolean; signal?: AbortSignal } = {},
): Promise<T> {
  const init: RequestInit = {
    method,
    credentials: "same-origin",
    headers: body !== undefined ? { "content-type": "application/json" } : undefined,
    body: body !== undefined ? JSON.stringify(body) : undefined,
    signal: opts.signal,
  };
  const res = await fetch(path, init);
  if (res.status === 401) {
    setAuthed(false);
    throw new ApiError(401, "Not signed in");
  }
  if (!res.ok) {
    let msg = res.statusText || `HTTP ${res.status}`;
    try {
      const text = await res.text();
      if (text) {
        try {
          const j = JSON.parse(text) as { error?: string; message?: string };
          msg = j.error ?? j.message ?? text;
        } catch {
          msg = text.slice(0, 300);
        }
      }
    } catch {
      /* ignore */
    }
    throw new ApiError(res.status, msg);
  }
  if (opts.raw) return (await res.text()) as T;
  if (res.status === 204) return undefined as T;
  const text = await res.text();
  return (text ? JSON.parse(text) : undefined) as T;
}

const get = <T>(path: string, signal?: AbortSignal) => request<T>("GET", path, undefined, { signal });
const post = <T>(path: string, body?: unknown) => request<T>("POST", path, body ?? {});
const del = <T>(path: string) => request<T>("DELETE", path);

function qs(params: Record<string, string | number | undefined | null>) {
  const s = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) {
    if (v !== undefined && v !== null && v !== "") s.set(k, String(v));
  }
  const str = s.toString();
  return str ? `?${str}` : "";
}

const enc = encodeURIComponent;

/* ---------------------------- endpoints ---------------------------- */

export const api = {
  login: async (token: string) => {
    await post<void>("/api/auth/login", { token });
    setAuthed(true);
  },
  /** Which sign-in methods this server offers (public). */
  authMethods: (signal?: AbortSignal) => get<{ password: boolean; token: boolean }>("/api/auth/methods", signal),
  loginPassword: async (password: string) => {
    await post<void>("/api/auth/password", { password });
    setAuthed(true);
  },
  setPassword: (password: string) => request<void>("PUT", "/api/settings/password", { password }),
  clearPassword: () => del<void>("/api/settings/password"),
  logout: async () => {
    try {
      await post<void>("/api/auth/logout");
    } finally {
      setAuthed(false);
    }
  },
  me: async (signal?: AbortSignal) => {
    const me = await get<Me>("/api/me", signal);
    setAuthed(Boolean(me.authenticated));
    return me;
  },

  overview: (signal?: AbortSignal) => get<Overview>("/api/overview", signal),

  repos: (signal?: AbortSignal) => get<Repo[]>("/api/repos", signal),
  addRepo: (repo: NewRepo) => post<Repo>("/api/repos", repo),
  deleteRepo: (id: string) => del<void>(`/api/repos/${enc(id)}`),
  pollRepo: (id: string) => post<void>(`/api/repos/${enc(id)}/poll`),
  triggerRepo: (id: string, body: { branch?: string; pipeline?: string }) =>
    post<Run[]>(`/api/repos/${enc(id)}/trigger`, body),

  runs: (params: { repo?: string; limit?: number; before?: string | number }, signal?: AbortSignal) =>
    get<Run[]>(`/api/runs${qs({ repo: params.repo, limit: params.limit ?? 50, before: params.before })}`, signal),
  run: (id: string, signal?: AbortSignal) => get<RunDetail>(`/api/runs/${enc(id)}`, signal),
  cancelRun: (id: string) => post<void>(`/api/runs/${enc(id)}/cancel`),
  retryRun: (id: string, onlyFailed: boolean) => post<void>(`/api/runs/${enc(id)}/retry`, { only_failed: onlyFailed }),

  job: (id: string, signal?: AbortSignal) => get<Job>(`/api/jobs/${enc(id)}`, signal),
  jobLogsUrl: (id: string, step: number) => `/api/jobs/${enc(id)}/logs${qs({ step })}`,
  jobLogs: (id: string, step: number) =>
    request<string>("GET", `/api/jobs/${enc(id)}/logs${qs({ step })}`, undefined, { raw: true }),

  agents: (signal?: AbortSignal) => get<Agent[]>("/api/agents", signal),
  forgetAgent: (name: string) => del<void>(`/api/agents/${enc(name)}`),
  drainAgent: (name: string, draining: boolean) => post<void>(`/api/agents/${enc(name)}/drain`, { draining }),
  join: (signal?: AbortSignal) => get<JoinInfo>("/api/agents/join", signal),
  joinRequests: (signal?: AbortSignal) => get<JoinRequest[]>("/api/join-requests", signal),
  approveJoin: (id: string, body: { name?: string; labels?: string[] } = {}) =>
    post<{ id: string; status: string; name: string; labels: string[] }>(`/api/join-requests/${enc(id)}/approve`, body),
  denyJoin: (id: string) => post<{ id: string; status: string; name: string }>(`/api/join-requests/${enc(id)}/deny`),
  discovery: (signal?: AbortSignal) => get<DiscoveryInfo>("/api/discovery", signal),

  metricsHistory: (agent?: string, signal?: AbortSignal) =>
    get<MetricsHistory>(`/api/metrics/history${qs({ agent })}`, signal),

  system: (signal?: AbortSignal) => get<SystemInfo>("/api/system", signal),
  terminalSessions: (signal?: AbortSignal) => get<TerminalSessionInfo[]>("/api/terminal/sessions", signal),
  /** End a session: its shell stops on every device. */
  closeTerminalSession: (id: string) => del<void>(`/api/terminal/sessions/${enc(id)}`),
  audit: (limit = 200, signal?: AbortSignal) => get<AuditEntry[]>(`/api/audit${qs({ limit })}`, signal),
};

/** Build a ws:// or wss:// URL for a same-origin path. */
export function wsUrl(path: string): string {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  return `${proto}//${location.host}${path}`;
}

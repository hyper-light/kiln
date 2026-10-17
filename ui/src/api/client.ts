import type {
  Agent,
  AiSession,
  AiTranscriptEntry,
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
  UpdateJob,
  UpdatesOverview,
  KilnBinary,
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
  opts: { raw?: boolean; signal?: AbortSignal; file?: Blob } = {},
): Promise<T> {
  const init: RequestInit = {
    method,
    credentials: "same-origin",
    headers: opts.file ? { "content-type": "application/octet-stream" } : body !== undefined ? { "content-type": "application/json" } : undefined,
    body: opts.file ?? (body !== undefined ? JSON.stringify(body) : undefined),
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
  /** Start a session without attaching (any device attaches by its id). */
  createTerminalSession: (body: { target: string; cmd?: string; cols?: number; rows?: number }) =>
    post<TerminalSessionInfo>("/api/terminal/sessions", body),

  aiSessions: (signal?: AbortSignal) => get<AiSession[]>("/api/ai-sessions", signal),
  /** Connected runners too old to report AI sessions. */
  aiSilent: (signal?: AbortSignal) => get<{ machines: string[] }>("/api/ai-sessions/silent", signal),
  aiTranscript: (machine: string, pid: number, signal?: AbortSignal) =>
    get<AiTranscriptEntry[]>(`/api/ai-sessions/${enc(machine)}/${pid}/transcript`, signal),
  audit: (limit = 200, signal?: AbortSignal) => get<AuditEntry[]>(`/api/audit${qs({ limit })}`, signal),

  /** Settings → Updates: the server, every runner, recent upgrades and the binaries kiln has. */
  updates: (signal?: AbortSignal) => get<UpdatesOverview>("/api/updates", signal),
  /** Ask GitHub (and, in Kubernetes, the image on the node) again; an `updates` event follows. */
  checkUpdates: () => post<UpdatesOverview>("/api/updates/check"),
  upgradeRunner: (name: string) => post<UpdateJob>(`/api/updates/runners/${enc(name)}`),
  upgradeAllRunners: () => post<{ jobs: UpdateJob[]; errors: string[] }>("/api/updates/runners"),
  upgradeServer: (action: "restart" | "replace") => post<UpdateJob>("/api/updates/server", { action }),
  uploadBinary: (target: string, file: Blob) => request<KilnBinary>("POST", `/api/updates/binaries${qs({ target })}`, undefined, { file }),
  deleteBinary: (target: string) => del<void>(`/api/updates/binaries/${enc(target)}`),
};

/** Build a ws:// or wss:// URL for a same-origin path. */
export function wsUrl(path: string): string {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  return `${proto}//${location.host}${path}`;
}

/* ---------------------------- discovery v2 + Add machines ---------------------------- */
// The routes of .claude/contracts/discovery.md, kept apart from `api` above. Join requests are
// decided with `api.approveJoin` / `api.denyJoin`.

import type { DiscoveredInstance, ScanState } from "./types";

export const discoveryApi = {
  /** Scan subnets for kiln (each at most a /22; none: the detected `subnets`). 202; progress arrives as `discovery` events. */
  scan: (cidrs?: string[]) => post<ScanState>("/api/discovery/scan", cidrs?.length ? { cidrs } : {}),
  /** Ask one address (IP or host name, optional `:port`) whether kiln runs there; answers within ~3 s. */
  probe: (address: string) => post<{ found: DiscoveredInstance[] }>("/api/discovery/probe", { address }),
  /** Invite a machine that is not set up yet: it asks to join, and its request waits for approval as usual. */
  invite: (instance: string) => post<{ invited: true }>("/api/discovery/invite", { instance }),
  uninvite: (instance: string) => del<void>(`/api/discovery/invite/${enc(instance)}`),
  /** Open (rate-limited): what an invited machine polls. */
  invited: (instance: string, signal?: AbortSignal) => get<{ invited: boolean; ca: string }>(`/api/join/invited${qs({ instance })}`, signal),
};

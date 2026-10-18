import type {
  Agent,
  AiSession,
  AiTranscriptEntry,
  AuditEntry,
  DirListing,
  DiscoveryInfo,
  Job,
  JoinInfo,
  JoinRequest,
  KilnBinary,
  Me,
  MetricsHistory,
  NewRepo,
  Overview,
  PlaceAsk,
  PlacesView,
  PlaceTask,
  ReachRunner,
  Repo,
  Run,
  RunDetail,
  SyncKeep,
  SyncView,
  SystemInfo,
  TaskKind,
  TerminalSessionInfo,
  UpdateJob,
  UpdatesOverview,
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

export async function request<T>(method: string, path: string, body?: unknown, opts: { raw?: boolean; signal?: AbortSignal; file?: Blob; type?: string } = {}): Promise<T> {
  const init: RequestInit = {
    method,
    credentials: "same-origin",
    headers: opts.file ? { "content-type": opts.type ?? "application/octet-stream" } : body !== undefined ? { "content-type": "application/json" } : undefined,
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

/** Where a published image lives: the local Docker that builds it, or a registry repository. */
export type ImageHome = { kind: "local" } | { kind: "registry"; repository: string; integration?: string };

export interface CdImages {
  images: Record<string, ImageHome>;
  /** The images apps publish, with the apps that do. */
  used: Record<string, string[]>;
}

export interface IntegrationKind {
  kind: string;
  title: string;
  category: string;
  mode: "oidc" | "token" | "optional";
  fields: { name: string; required: boolean; help: string }[];
  secret: string | null;
  built: boolean;
}

export interface Integration {
  id: string;
  kind: string;
  name: string;
  config: Record<string, unknown>;
  has_secret: boolean;
  scope: string;
  created_at: number;
  verified_at: number | null;
  last_error: string | null;
  last_used_at: number | null;
}

export interface Integrations {
  kinds: IntegrationKind[];
  integrations: Integration[];
}

export interface OidcIssuer {
  issuer: string;
  discovery: string;
  jwks_uri: string;
  /** Whether this server serves the issuer's documents now. */
  serving: boolean;
}

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
  /** Where each image kiln publishes lives, and the apps that publish it (docs/cd-targets.md §7.1). */
  cdImages: (signal?: AbortSignal) => get<CdImages>("/api/cd/images", signal),
  setCdImage: (name: string, home: ImageHome) => request<CdImages>("PUT", `/api/cd/images/${encodeURIComponent(name)}`, home),
  /** Integrations (docs/ci.md W11): the catalog of kinds and the ones set up. */
  integrations: (signal?: AbortSignal) => get<Integrations>("/api/ci/integrations", signal),
  createIntegration: (body: { kind: string; name: string; config: Record<string, string>; secret?: string }) => post<Integration>("/api/ci/integrations", body),
  /** A real exchange with the integration; a federated one as `repository`'s `environment` (docs/cd-targets.md §6). */
  verifyIntegration: (name: string, as?: { repository?: string; environment?: string }) =>
    post<{ ok: boolean; message?: string; error?: string }>(`/api/ci/integrations/${encodeURIComponent(name)}/verify`, as ?? {}),
  /** kiln's OIDC issuer (docs/ci.md W10): its URL, whether its documents are served, its keys. */
  oidc: (signal?: AbortSignal) => get<OidcIssuer>("/api/ci/oidc", signal),
  deleteIntegration: (name: string) => del<void>(`/api/ci/integrations/${encodeURIComponent(name)}`),

  repos: (signal?: AbortSignal) => get<Repo[]>("/api/repos", signal),
  sync: (signal?: AbortSignal) => get<SyncView>("/api/sync", signal),
  syncCreate: (body: { name: string; copies: { machine: string; path: string }[]; exclude: string[] }) => post<{ id: string }>("/api/sync", body),
  syncAddCopy: (id: string, copy: { machine: string; path: string }) => post<{ member: number }>(`/api/sync/${enc(id)}/copies`, copy),
  syncRemoveCopy: (id: string, member: number) => del<void>(`/api/sync/${enc(id)}/copies/${member}`),
  syncDelete: (id: string) => del<void>(`/api/sync/${enc(id)}`),
  syncResolve: (id: string, path: string, keep: SyncKeep) => post<void>(`/api/sync/${enc(id)}/conflicts/resolve`, { path, keep }),
  /** Every repository kiln knows of, the places each is, the machines and the tasks. */
  places: (signal?: AbortSignal) => get<PlacesView>("/api/places", signal),
  /** Asks every machine to look at its repositories again now. */
  placesLook: () => post<{ asked: number }>("/api/places/look"),
  /** Starts a clone, sync, move or removal: its tasks, one per machine. */
  placesStart: (kind: TaskKind, ask: PlaceAsk) => post<PlaceTask[]>(`/api/places/${kind}`, ask),
  placesRetry: (id: string) => post<PlaceTask[]>(`/api/places/tasks/${enc(id)}/retry`),
  placesDismiss: (id: string) => del<void>(`/api/places/tasks/${enc(id)}`),
  addRepo: (repo: NewRepo) => post<Repo>("/api/repos", repo),
  deleteRepo: (id: string) => del<void>(`/api/repos/${enc(id)}`),
  pollRepo: (id: string) => post<void>(`/api/repos/${enc(id)}/poll`),
  triggerRepo: (id: string, body: { branch?: string; pipeline?: string }) => post<Run[]>(`/api/repos/${enc(id)}/trigger`, body),

  runs: (params: { repo?: string; limit?: number; before?: string | number }, signal?: AbortSignal) =>
    get<Run[]>(`/api/runs${qs({ repo: params.repo, limit: params.limit ?? 50, before: params.before })}`, signal),
  run: (id: string, signal?: AbortSignal, attempt?: number) => get<RunDetail>(`/api/runs/${enc(id)}${attempt ? `?attempt=${attempt}` : ""}`, signal),
  cancelRun: (id: string) => post<void>(`/api/runs/${enc(id)}/cancel`),
  retryRun: (id: string, onlyFailed: boolean) => post<void>(`/api/runs/${enc(id)}/retry`, { only_failed: onlyFailed }),

  job: (id: string, signal?: AbortSignal) => get<Job>(`/api/jobs/${enc(id)}`, signal),
  jobLogsUrl: (id: string, step: number) => `/api/jobs/${enc(id)}/logs${qs({ step })}`,
  jobLogs: (id: string, step: number) => request<string>("GET", `/api/jobs/${enc(id)}/logs${qs({ step })}`, undefined, { raw: true }),

  agents: (signal?: AbortSignal) => get<Agent[]>("/api/agents", signal),
  forgetAgent: (name: string) => del<void>(`/api/agents/${enc(name)}`),
  drainAgent: (name: string, draining: boolean) => post<void>(`/api/agents/${enc(name)}/drain`, { draining }),
  join: (signal?: AbortSignal) => get<JoinInfo>("/api/agents/join", signal),
  joinRequests: (signal?: AbortSignal) => get<JoinRequest[]>("/api/join-requests", signal),
  approveJoin: (id: string, body: { name?: string; labels?: string[] } = {}) => post<{ id: string; status: string; name: string; labels: string[] }>(`/api/join-requests/${enc(id)}/approve`, body),
  denyJoin: (id: string) => post<{ id: string; status: string; name: string }>(`/api/join-requests/${enc(id)}/deny`),
  discovery: (signal?: AbortSignal) => get<DiscoveryInfo>("/api/discovery", signal),

  metricsHistory: (agent?: string, signal?: AbortSignal) => get<MetricsHistory>(`/api/metrics/history${qs({ agent })}`, signal),

  system: (signal?: AbortSignal) => get<SystemInfo>("/api/system", signal),
  terminalSessions: (signal?: AbortSignal) => get<TerminalSessionInfo[]>("/api/terminal/sessions", signal),
  /** Reach's directory: every runner, whether it takes commands from other runners, how busy it is. */
  reachRunners: (signal?: AbortSignal) => get<{ runners: ReachRunner[] }>("/api/reach/runners", signal),
  /** End a session: its shell stops on every device. */
  closeTerminalSession: (id: string) => del<void>(`/api/terminal/sessions/${enc(id)}`),
  /** Names a terminal session ("" takes its name away). */
  renameTerminalSession: (id: string, name: string) => request<void>("PUT", `/api/terminal/sessions/${enc(id)}/name`, { name }),
  /** Start a session without attaching (any device attaches by its id). */
  createTerminalSession: (body: { target: string; cmd?: string; cols?: number; rows?: number }) => post<TerminalSessionInfo>("/api/terminal/sessions", body),

  aiSessions: (signal?: AbortSignal) => get<AiSession[]>("/api/ai-sessions", signal),
  /** Connected runners too old to report AI sessions. */
  aiSilent: (signal?: AbortSignal) => get<{ machines: string[] }>("/api/ai-sessions/silent", signal),
  aiTranscript: (machine: string, pid: number, signal?: AbortSignal) => get<AiTranscriptEntry[]>(`/api/ai-sessions/${enc(machine)}/${pid}/transcript`, signal),
  audit: (limit = 200, signal?: AbortSignal) => get<AuditEntry[]>(`/api/audit${qs({ limit })}`, signal),

  /** Settings → Updates: the server, every runner, recent upgrades and the binaries kiln has. */
  updates: (signal?: AbortSignal) => get<UpdatesOverview>("/api/updates", signal),
  /** Ask GitHub (and, in Kubernetes, the image on the node) again; an `updates` event follows. */
  checkUpdates: () => post<UpdatesOverview>("/api/updates/check"),
  upgradeRunner: (name: string) => post<UpdateJob>(`/api/updates/runners/${enc(name)}`),
  upgradeAllRunners: () => post<{ jobs: UpdateJob[]; errors: string[] }>("/api/updates/runners"),
  /** Run the pipeline that builds kiln for every platform; its binaries are kept when it succeeds. */
  buildEveryPlatform: () => post<Run>("/api/updates/build"),
  upgradeServer: (action: "restart" | "replace") => post<UpdateJob>("/api/updates/server", { action }),
  uploadBinary: (target: string, file: Blob) => request<KilnBinary>("POST", `/api/updates/binaries${qs({ target })}`, undefined, { file }),
  deleteBinary: (target: string) => del<void>(`/api/updates/binaries/${enc(target)}`),

  /** Whether kiln turns recordings into text (for browsers without speech recognition of their own). */
  voice: (signal?: AbortSignal) => get<{ transcribe: boolean; language: string }>("/api/voice", signal),
  /** A recording's words; sent as recorded (its own audio type), up to 25 MiB. */
  transcribe: (audio: Blob) => request<{ text: string }>("POST", "/api/voice/transcribe", undefined, { file: audio, type: audio.type || "audio/webm" }),
};

/* ---------------------------- folders on a machine ---------------------------- */

/**
 * The folders in a folder on a machine, for the folder picker. `target` is "local" (the kiln
 * server), "agent:<name>" or a machine's name; `path` as typed (`~` is home; "" is home too).
 * What can't be listed (no such folder, permission denied) comes back in `error` with a 200; a
 * runner too old to list folders is a 409.
 */
export const machineDirs = (target: string, path: string, signal?: AbortSignal) => get<DirListing>(`/api/machines/${enc(target)}/dirs${qs({ path })}`, signal);

/* ---------------------------- front doors (Runners → Domain & HTTPS) ---------------------------- */

import type { CertInfo, FrontDoorSettings, FrontDoorView, TailscaleView } from "./types";

export const frontDoorApi = {
  /** Every runner with front-door settings. */
  list: (signal?: AbortSignal) => get<FrontDoorView[]>("/api/front-doors", signal),
  get: (runner: string, signal?: AbortSignal) => get<FrontDoorView>(`/api/runners/${enc(runner)}/front-door`, signal),
  save: (runner: string, settings: FrontDoorSettings) => request<FrontDoorView>("PUT", `/api/runners/${enc(runner)}/front-door`, settings),
  /** A certificate chain and its key for the runner (which keeps them; the key is never sent back). */
  uploadCert: (runner: string, cert_pem: string, key_pem: string) => post<{ cert: CertInfo; front_door: FrontDoorView }>(`/api/runners/${enc(runner)}/front-door/cert`, { cert_pem, key_pem }),
  /** A new key and self-signed certificate now (409 for your own certificate, or with HTTPS off). */
  renewCert: (runner: string) => post<FrontDoorView>(`/api/runners/${enc(runner)}/front-door/cert/renew`, {}),
  /** The certificate chain the runner serves, as a PEM download (certificates only, never the key). */
  certUrl: (runner: string) => `/api/runners/${enc(runner)}/front-door/cert`,
};

/** The tailnet's split DNS for the front doors (one Tailscale credential for the server). */
export const tailscaleApi = {
  get: (signal?: AbortSignal) => get<TailscaleView>("/api/tailscale", signal),
  /** An OAuth client secret (with its ID when it doesn't carry it) or an API access token; tried first. */
  connect: (secret: string, client_id: string) => request<TailscaleView>("PUT", "/api/tailscale", { secret, client_id }),
  forget: () => del<TailscaleView>("/api/tailscale"),
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

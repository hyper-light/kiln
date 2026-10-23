/**
 * Deploying by API (docs/deploy-api.md): apps, environments, their registrations and releases,
 * over `/api/deploy`. A release is a commit to an environment's release repository that its CD
 * provider (KilnCD or Argo CD) makes run; it waits on the environment's gates first.
 */
import { ApiError, request } from "./client";
import type { CdDiff, CdResourceKey } from "./types";

export type ReleaseState = "waiting" | "waiting_approval" | "committing" | "delivering" | "succeeded" | "failed" | "cancelled" | "rolled_back" | "superseded";
export type ReleaseKind = "deploy" | "config" | "rollback" | "enable" | "disable" | "revert";

export interface Artifact {
  repository: string;
  tag: string;
  digest: string;
}

/** A gate's result on a release, with its evidence. */
export interface GateResult {
  gate: string;
  passed: boolean;
  detail: string;
  evidence?: Record<string, unknown> | null;
}

/** A person's answer to a release waiting for approval. */
export interface Approval {
  by: string;
  at: number;
  approved: boolean;
  note?: string;
}

export interface Release {
  id: number;
  app: string;
  environment: string;
  kind: ReleaseKind;
  state: ReleaseState;
  origin: "api" | "git" | "kiln";
  version?: string;
  artifacts?: Record<string, Artifact>;
  values?: string;
  commit?: string;
  requested_by: string;
  message?: string;
  rollback_of?: number;
  rolled_back_by?: number;
  keep?: boolean;
  provider?: Record<string, unknown> | null;
  gates?: GateResult[];
  approvals?: Approval[];
  promoted_from?: number;
  created_at: number;
  committed_at?: number;
  ended_at?: number;
  /** Secrets' changes it delivers (docs/deploy-api.md §3.5): a secrets release's reason (`DB_PASSWORD v4`), or ones it took on while it waited. */
  secrets?: string | null;
  /** Failed after its commit, it isn't reverted: it's held for a person (§8.1). */
  no_revert?: boolean;
  /** A revert: the release it puts back what ran before. */
  reverts?: number | null;
  /** The revert that puts back what ran before it. */
  reverted_by?: number | null;
  /** What it runs on its way in that a revert couldn't undo (a migration's hook): failed, it's held rather than reverted. */
  irreversible?: string | null;
}

export interface Registration {
  app: string;
  environment: string;
  enabled: boolean;
  paused: boolean;
  running?: number;
  created_at: number;
  /** Why what runs here needs a person (docs/deploy-api.md §8.1): a release failed and wasn't put back, or putting it back didn't land. The next release that lands clears it. */
  degraded?: string | null;
}

/** A check a release passes (§9.1): one built in, by its name, or one described: a URL kiln asks, or a workflow it runs. */
export type Check = string | CheckSpec;

export interface CheckSpec {
  name: string;
  /** A URL asked about each release: it answers pass, fail or pending. */
  http?: { url: string; timeout?: string };
  /** A workflow of one of kiln's repositories, run for each release: it passes when the run succeeds. */
  workflow?: { repository: string; workflow: string; ref?: string };
  /** Recorded with the release, never holding or refusing it. */
  report_only?: boolean;
}

export const checkName = (c: Check) => (typeof c === "string" ? c : c.name);

/** What an environment asks of what it deploys (docs/ci.md W18-R04): kiln's signed provenance, built by its CI. */
export interface ProvenanceRequirement {
  builder?: string;
  workflows?: string[];
  branches?: string[];
}

export interface Gates {
  /** `staging@eu`: `staging` on the linked kiln server `eu` (§11). */
  after?: string[];
  soak?: string;
  approvals?: { count: number; reviewers?: string[]; self_approve?: boolean };
  windows?: { allow?: string; deny?: string; duration: string; timezone?: string }[];
  checks?: Check[];
  require_provenance?: ProvenanceRequirement;
  freeze?: string;
  /** `auto`: a version that lands in one of `after` is released here by itself (§9.2). */
  promote?: "auto" | "manual";
  /** `pull-request`: each release is a pull request on the release repository (a merge request on GitLab); its merge is the approval, and closing it cancels the release (§4). */
  via?: "commit" | "pull-request";
}

/** A git host release repositories may be on (docs/deploy-api.md §7): kiln's own, GitHub through kiln's App, a GitLab with the groups its token covers, Bitbucket with its workspaces. */
export interface GitHost {
  host: "kiln" | "github" | "gitlab" | "bitbucket";
  name: string;
  /** Its web origin. */
  web?: string;
  /** A GitLab's: the groups its token covers; Bitbucket's: the workspaces kiln's credentials cover (none: every one). */
  groups?: string[];
}

/** The GitHub App kiln acts through, if it has one (docs/ci.md W14-R10). */
export interface GithubApp {
  configured: boolean;
  /** GitHub's web origin. */
  web_url: string;
  slug?: string | null;
  html_url?: string | null;
  install_url?: string | null;
}

export interface Environment {
  name: string;
  /** `kilncd`, or `argocd/<integration>`. */
  provider: string;
  release_repo: { host: string; repo: string; branch: string; root: string };
  /** `kilncd`: on Argo CD, the KilnCD cluster that reaches each of Argo CD's, where kiln writes an app's secrets. */
  targets: { kubernetes: string[]; platforms?: Record<string, Record<string, unknown>>; kilncd?: Record<string, string> };
  gates: Gates;
  concurrency: "reject" | "queue";
  agents: { deploy: boolean; approve: boolean };
  /** What an app's owner teams may do here without environments.deploy on it (docs/deploy-api.md §15). */
  owners?: { release: boolean };
  /** On Argo CD: kiln writes each Application itself, rather than through the environment's ApplicationSet. */
  argocd?: { applications: boolean };
}

export type MovePhase = "comparing" | "compared" | "handing_over" | "moved" | "failed" | "cancelled";

/** One app's part in an environment's move between providers (docs/deploy-api.md §8.4). */
export interface MovedRegistration {
  app: string;
  /** The new provider would change nothing of what runs but its own tracking marks. */
  agrees?: boolean;
  /** Where it disagrees: each object it would change. */
  differences?: CdDiff[];
  /** Each object's uid as the handover began. */
  uids?: Record<string, string>;
  /** Objects made again by the handover (their uid changed): none, if it went as it should. */
  remade?: string[];
  message?: string;
}

/** An environment's move between providers: compared in shadow, then handed over with nothing made again. */
export interface Move {
  environment: string;
  from: string;
  to: string;
  argocd: { applications: boolean };
  phase: MovePhase;
  by: string;
  started_at: number;
  ended_at?: number;
  handover: boolean;
  force: boolean;
  handing_over: boolean;
  registrations: MovedRegistration[];
  message?: string;
}

/** What can be done to an object a registration runs: its provider's action, whether it can be now, what it takes. */
export interface ObjectAction {
  name: string;
  disabled: boolean;
  params?: { name: string; default?: string }[] | null;
}

/** An object an action is on. */
export interface ActionTarget {
  group?: string;
  kind: string;
  namespace?: string;
  name: string;
}

export type Package =
  | { kind: "helm"; repo: string; chart: string; chart_version?: string; set?: Record<string, string> }
  | { kind: "git"; repo: string; path: string; set?: Record<string, string> }
  | { kind: "platform"; provider: string; spec: unknown; repo?: string; path?: string };

export interface App {
  name: string;
  project: string;
  owners: string[];
  package: Package;
  namespace?: string;
  properties?: Record<string, string>;
  template?: string;
  contacts?: { oncall?: string; channels?: string[]; links?: Record<string, string> };
  /** Its source repository, as kiln names it (`acme/web`). */
  source?: string;
  archived: boolean;
  /** The repositories kiln made for its images and chart (docs/deploy-api.md §3.1, step 5). */
  registries?: Registries;
}

/** Where an app's images and chart go: release images (tags immutable), snapshots (the newest kept), its chart. */
export interface Registries {
  /** `kiln`, or the integration they were made through. */
  at: string;
  images: string;
  snapshots: string;
  chart: string;
}

/** The registry a template's apps get their image and chart repositories on (§3.1, step 5). */
export interface RegistryTemplate {
  /** `kiln`, or an `aws` (ECR) or `gcp` (Artifact Registry) integration. */
  at: string;
  /** `{project}/{app}`. */
  name: string;
  keep_snapshots: number;
  location?: string;
  project?: string;
}

/** The source repository a template makes for each app (§3.1): one of kiln's own, or on another host. */
export interface RepoTemplate {
  /** `acme/{app}`: `{app}` and `{project}` filled in. */
  name: string;
  starter?: string;
  starter_ref?: string;
  branch: string;
  /** `kiln` (the default), `github`, `gitlab` or `bitbucket`. */
  host?: string;
}

/** What apps are made from (§3.2). */
export interface Template {
  name: string;
  description?: string;
  package?: Package;
  namespace?: string;
  checks?: Check[];
  properties?: Record<string, string[]>;
  environments?: string[];
  repository?: RepoTemplate;
  registry?: RegistryTemplate;
}

/** An app as asked for: a template fills what isn't given. `source: ""` makes none. */
export interface AppAsk {
  name: string;
  project?: string;
  owners?: string[];
  template?: string;
  package?: Package;
  namespace?: string;
  properties?: Record<string, string>;
  source?: string;
}

/** What changes in an app: only what is given. A property set to `null` is removed; `source: ""` unlinks. */
export interface AppPatch {
  project?: string;
  owners?: string[];
  package?: Package;
  namespace?: string;
  properties?: Record<string, string | null>;
  template?: string;
  contacts?: App["contacts"];
  source?: string;
  archived?: boolean;
}

/** One registration on the board: what runs, and what's on its way. */
export interface Cell {
  registration: Registration;
  running: Release | null;
  in_flight: Release | null;
}

export interface Board {
  environments: Environment[];
  apps: App[];
  cells: Cell[];
  waiting_approval: Release[];
}

export interface RegistrationDetail {
  registration: Registration;
  running: Release | null;
  in_flight: Release | null;
  releases: Release[];
  provider: { kind: "kilncd" | "argocd"; app: string; integration?: string; status: Record<string, unknown> | null };
}

export interface AppDetail {
  app: App;
  environments: { registration: Registration; running: Release | null }[];
}

/** One object of what runs, as the provider sees it (KilnCD's shape, from either provider). */
export interface RunningObject extends CdResourceKey {
  sync: string;
  health: string | null;
  message: string | null;
  missing?: boolean;
  requires_pruning?: boolean;
  hook?: boolean;
  why?: string | null;
}

/** A workload's pods' logs, the last lines of each (Argo CD gives only the lines). */
export interface LogRead {
  lines: { pod: string; ts: string; text: string }[];
  pods?: string[];
  more_pods?: number;
  container?: string | null;
  containers?: string[];
  errors?: string[];
}

/** DORA's four keys, from the releases (durations in seconds). */
export interface Metrics {
  days: number;
  changes: number;
  landed: number;
  per_day: number;
  lead_time_secs: { count: number; p50: number | null; p90: number | null };
  failed: number;
  change_failure_rate: number | null;
  time_to_restore_secs: { count: number; p50: number | null; p90: number | null };
  by_day: { day: string; landed: number; failed: number }[];
}

/** One of an app's secrets in an environment, as listed: never its value (docs/deploy-api.md §3.5). */
export interface SecretMeta {
  name: string;
  version: number;
  size: number;
  set_by: string;
  set_at: number;
}

export interface SecretVersion {
  version: number;
  set_by: string;
  set_at: number;
  /** A deletion: the secret was gone from this version on. */
  deleted: boolean;
  /** Its value is no longer kept (older than the last ten), or it never had one. */
  destroyed: boolean;
}

/** A secret's change on its way to what runs: the release that delivers it (made, or a waiting one it joined), and what happens next. */
export interface SecretReleased {
  release: Release | null;
  next: string;
}

/** A read of an app's secrets for a delivery: who, for what, which versions (`DB_PASSWORD@3,API_KEY@1`). */
export interface SecretRead {
  by: string;
  purpose: string;
  versions: string;
  at: number;
}

/** A version a registration has run: when it first and last landed, how often, and whether it runs now. */
export interface VersionRun {
  version: string;
  runs_now: boolean;
  releases: number;
  last_release: number | null;
  first_landed: number | null;
  last_landed: number | null;
}

/** This server as the servers linked to it see it (docs/deploy-api.md §11): where it is, and the issuer its statements are signed by. */
export interface LinkInfo {
  server: string;
  issuer: string;
}

/** A fleet change as asked for (docs/deploy-api.md §3.3): a version or values released to many apps of an environment, or (`source`) a change in each app's source repository. */
export interface FleetAsk {
  environment: string;
  apps?: string[];
  template?: string;
  project?: string;
  owner?: string;
  version?: string;
  set?: Record<string, string>;
  source?: { base?: string; set?: Record<string, string>; chart?: string; values?: string };
  idempotency_key?: string;
}

/** A source change's proposal in one repository: a pull request on its host, or a branch on kiln's own git server. */
export interface Proposal {
  fleet: string;
  /** kiln's name for the repository. */
  repo: string;
  host: string;
  name: string;
  into: string;
  branch: string;
  apps: string[];
  files: string[];
  number?: number;
  url?: string;
  commit?: string;
  parent?: string;
  state: "open" | "merged" | "closed" | "unchanged" | "failed";
  note?: string;
  by: string;
  at: number;
  /** On kiln's own git server, while it's open: its diff. */
  diff?: string;
}

/** A fleet change as it stands: its releases, or (one in apps' sources) each repository's proposal. */
export interface Fleet {
  fleet: string;
  source?: boolean;
  done: boolean;
  states: Record<string, number>;
  releases?: Release[];
  proposals?: Proposal[];
}

/** What a release asks for: exactly one of a version, values, enabled, or a promotion. */
export interface ReleaseAsk {
  app: string;
  environment: string;
  version?: string;
  artifacts?: Record<string, Artifact>;
  values?: string;
  enabled?: boolean;
  keep?: boolean;
  promote_from?: string;
  rollback?: { to?: number };
  idempotency_key?: string;
}

const enc = encodeURIComponent;
const reg = (app: string, env: string) => `/api/deploy/apps/${enc(app)}/environments/${enc(env)}`;

export const deployApi = {
  board: (signal?: AbortSignal) => request<Board>("GET", "/api/deploy/board", undefined, { signal }),
  apps: (signal?: AbortSignal) => request<{ apps: App[] }>("GET", "/api/deploy/apps", undefined, { signal }).then((r) => r.apps),
  app: (name: string, signal?: AbortSignal) => request<AppDetail>("GET", `/api/deploy/apps/${enc(name)}`, undefined, { signal }),
  /** The app replaced, all of it, by what's given. */
  putApp: (a: App) => request<App>("PUT", `/api/deploy/apps/${enc(a.name)}`, a),
  /** The app as Backstage's catalog reads it. */
  catalogInfo: (name: string, signal?: AbortSignal) => request<string>("GET", `/api/deploy/apps/${enc(name)}/catalog-info.yaml`, undefined, { raw: true, signal }),
  registration: (app: string, env: string, signal?: AbortSignal) => request<RegistrationDetail>("GET", reg(app, env), undefined, { signal }),
  candidates: (app: string, env: string) => request<{ candidates: Release[] }>("GET", `${reg(app, env)}/candidates`).then((r) => r.candidates),
  releases: (q: { app?: string; environment?: string; state?: ReleaseState; limit?: number }, signal?: AbortSignal) => {
    const qs = Object.entries(q)
      .filter(([, v]) => v !== undefined && v !== "")
      .map(([k, v]) => `${enc(k)}=${enc(String(v))}`)
      .join("&");
    return request<{ releases: Release[] }>("GET", `/api/deploy/releases${qs ? `?${qs}` : ""}`, undefined, { signal }).then((r) => r.releases);
  },
  release: (id: number, signal?: AbortSignal) => request<Release>("GET", `/api/deploy/releases/${id}`, undefined, { signal }),
  metrics: (q: { app?: string; environment?: string; days?: number }, signal?: AbortSignal) => {
    const qs = Object.entries(q)
      .filter(([, v]) => v !== undefined && v !== "")
      .map(([k, v]) => `${enc(k)}=${enc(String(v))}`)
      .join("&");
    return request<Metrics>("GET", `/api/deploy/metrics${qs ? `?${qs}` : ""}`, undefined, { signal });
  },
  create: (ask: ReleaseAsk) => request<Release>("POST", "/api/deploy/releases", ask),
  /** A rollback of release `id`'s app in its environment: to `to`, or the newest release that succeeded before what runs. */
  rollbackRelease: (id: number, to?: number) => request<Release>("POST", `/api/deploy/releases/${id}/rollback`, to === undefined ? {} : { to }),
  cancel: (id: number) => request<Release>("POST", `/api/deploy/releases/${id}/cancel`, {}),
  approve: (id: number, note?: string) => request<Release>("POST", `/api/deploy/releases/${id}/approve`, { note }),
  reject: (id: number, note?: string) => request<Release>("POST", `/api/deploy/releases/${id}/reject`, { note }),
  templates: (signal?: AbortSignal) => request<{ templates: Template[] }>("GET", "/api/deploy/templates", undefined, { signal }).then((r) => r.templates),
  template: (name: string, signal?: AbortSignal) => request<Template>("GET", `/api/deploy/templates/${enc(name)}`, undefined, { signal }),
  createTemplate: (t: Template) => request<Template>("POST", "/api/deploy/templates", t),
  putTemplate: (t: Template) => request<Template>("PUT", `/api/deploy/templates/${enc(t.name)}`, t),
  deleteTemplate: (name: string) => request<void>("DELETE", `/api/deploy/templates/${enc(name)}`),
  environments: (signal?: AbortSignal) => request<{ environments: Environment[] }>("GET", "/api/deploy/environments", undefined, { signal }).then((r) => r.environments),
  nameFree: (name: string) => request<{ name: string; available: boolean; why?: string | null }>("GET", `/api/deploy/app-names/${enc(name)}`),
  createApp: (ask: AppAsk) => request<App>("POST", "/api/deploy/apps", ask),
  patchApp: (name: string, patch: AppPatch) => request<App>("PATCH", `/api/deploy/apps/${enc(name)}`, patch),
  deleteApp: (name: string, o: { everywhere: boolean; keep: boolean }) => request<void>("DELETE", `/api/deploy/apps/${enc(name)}?everywhere=${o.everywhere}&keep=${o.keep}`),
  environment: (name: string, signal?: AbortSignal) => request<Environment>("GET", `/api/deploy/environments/${enc(name)}`, undefined, { signal }),
  putEnvironment: (e: Environment) => request<Environment>("PUT", `/api/deploy/environments/${enc(e.name)}`, e),
  /** Refused while apps are registered there. */
  deleteEnvironment: (name: string) => request<void>("DELETE", `/api/deploy/environments/${enc(name)}`),
  /** A new environment: what isn't given is the server's default (no gates, one release at a time, no agents). */
  createEnvironment: (e: { name: string; provider: string; release_repo: { host?: string; repo: string; branch: string; root: string }; targets: { kubernetes: string[] }; gates?: Gates; argocd?: { applications: boolean } }) =>
    request<Environment>("POST", "/api/deploy/environments", e),
  /** kiln's GitHub App, through which release repositories on GitHub are made and written. */
  githubApp: (signal?: AbortSignal) => request<GithubApp>("GET", "/api/ci/github/app", undefined, { signal }),
  /** The git hosts release repositories may be on. */
  gitHosts: (signal?: AbortSignal) => request<{ hosts: GitHost[] }>("GET", "/api/deploy/git-hosts", undefined, { signal }).then((r) => r.hosts),
  /** This server, for the servers linked to it. */
  link: (signal?: AbortSignal) => request<LinkInfo>("GET", "/api/deploy/link", undefined, { signal }),
  /** How an environment's move between providers stands; `null` when it hasn't moved. */
  move: (env: string, signal?: AbortSignal) =>
    request<Move>("GET", `/api/deploy/environments/${enc(env)}/move`, undefined, { signal }).catch((err: unknown) => {
      if (err instanceof ApiError && err.status === 404) return null;
      throw err;
    }),
  startMove: (env: string, ask: { to: string; handover: boolean; force: boolean; argocd?: { applications: boolean } }) => request<Move>("POST", `/api/deploy/environments/${enc(env)}/move`, ask),
  cancelMove: (env: string) => request<Move>("DELETE", `/api/deploy/environments/${enc(env)}/move`),
  actions: (app: string, env: string, t: ActionTarget, signal?: AbortSignal) => {
    const qs = Object.entries(t)
      .filter(([, v]) => v !== undefined && v !== "")
      .map(([k, v]) => `${enc(k)}=${enc(String(v))}`)
      .join("&");
    return request<{ actions: ObjectAction[] }>("GET", `${reg(app, env)}/actions?${qs}`, undefined, { signal }).then((r) => r.actions);
  },
  runAction: (app: string, env: string, t: ActionTarget, action: string, params: Record<string, string> = {}) =>
    request<{ done: string }>("POST", `${reg(app, env)}/actions`, { ...t, action, params }),
  register: (app: string, env: string) => request<Registration>("POST", reg(app, env), {}),
  /** What it runs there goes with it, unless `keep`. */
  deregister: (app: string, env: string, keep: boolean) => request<void>("DELETE", `${reg(app, env)}?keep=${keep}`),
  versions: (app: string, env: string, signal?: AbortSignal) => request<{ versions: VersionRun[] }>("GET", `${reg(app, env)}/versions`, undefined, { signal }).then((r) => r.versions),
  /** What runs of it there, signed for the linked server whose issuer is `audience`. */
  statement: (app: string, env: string, audience: string) => request<{ statement: string }>("GET", `${reg(app, env)}/statement?audience=${enc(audience)}`).then((r) => r.statement),
  resources: (app: string, env: string, signal?: AbortSignal) => request<{ resources: RunningObject[] }>("GET", `${reg(app, env)}/resources`, undefined, { signal }).then((r) => r.resources),
  manifests: (app: string, env: string, version?: string, signal?: AbortSignal) =>
    request<{ manifests: { key: CdResourceKey; yaml: string }[] }>("GET", `${reg(app, env)}/manifests${version ? `?version=${enc(version)}` : ""}`, undefined, { signal }).then((r) => r.manifests),
  diff: (app: string, env: string, version?: string, signal?: AbortSignal) =>
    request<{ diffs: CdDiff[] }>("GET", `${reg(app, env)}/diff${version ? `?version=${enc(version)}` : ""}`, undefined, { signal }).then((r) => r.diffs),
  logs: (app: string, env: string, q: { kind: string; name: string; namespace?: string; container?: string; tail: number }, signal?: AbortSignal) => {
    const qs = Object.entries(q)
      .filter(([, v]) => v !== undefined && v !== "")
      .map(([k, v]) => `${enc(k)}=${enc(String(v))}`)
      .join("&");
    return request<LogRead>("GET", `${reg(app, env)}/logs?${qs}`, undefined, { signal });
  },
  pause: (app: string, env: string) => request<Registration>("POST", `${reg(app, env)}/pause`, {}),
  resume: (app: string, env: string) => request<Registration>("POST", `${reg(app, env)}/resume`, {}),
  secrets: (app: string, env: string, signal?: AbortSignal) => request<{ backend: string; secrets: SecretMeta[] }>("GET", `${reg(app, env)}/secrets`, undefined, { signal }),
  /** A secret's next version. Its value is never read back. */
  setSecret: (app: string, env: string, name: string, value: string) => request<SecretMeta & SecretReleased>("PUT", `${reg(app, env)}/secrets/${enc(name)}`, { value }),
  deleteSecret: (app: string, env: string, name: string) => request<SecretReleased>("DELETE", `${reg(app, env)}/secrets/${enc(name)}`),
  secretVersions: (app: string, env: string, name: string, signal?: AbortSignal) =>
    request<{ versions: SecretVersion[] }>("GET", `${reg(app, env)}/secrets/${enc(name)}/versions`, undefined, { signal }).then((r) => r.versions),
  secretUses: (app: string, env: string, signal?: AbortSignal) => request<{ reads: SecretRead[] }>("GET", `${reg(app, env)}/secret-uses?limit=20`, undefined, { signal }).then((r) => r.reads),
  /** Every app's secrets sealed again under new keys: how many values were. */
  rotateSecretKeys: () => request<{ resealed: number }>("POST", "/api/deploy/secret-keys/rotate", {}),
  createFleet: (f: FleetAsk) => request<{ fleet: string }>("POST", "/api/deploy/fleets", f),
  fleet: (id: string, signal?: AbortSignal) => request<Fleet>("GET", `/api/deploy/fleets/${enc(id)}`, undefined, { signal }),
  /** A source change's proposals on kiln's own git server merged: `repo`'s, or each open one. */
  mergeFleet: (id: string, repo?: string) => request<Fleet>("POST", `/api/deploy/fleets/${enc(id)}/merge`, repo ? { repo } : {}),
  /** A source change's open proposals withdrawn. */
  withdrawFleet: (id: string) => request<Fleet>("DELETE", `/api/deploy/fleets/${enc(id)}`),
};

export const boardHref = "/deployments/releases";
export const newAppHref = "/deployments/releases/new";
export const releaseHref = (id: number) => `/deployments/releases/${id}`;
export const releaseAppHref = (app: string) => `/deployments/releases/apps/${enc(app)}`;
export const releaseEnvHref = (env: string) => `/deployments/releases/environments/${enc(env)}`;
export const registrationHref = (app: string, env: string) => `/deployments/releases/apps/${enc(app)}/environments/${enc(env)}`;
export const templatesHref = "/deployments/releases/templates";
export const templateHref = (name: string) => `/deployments/releases/templates/${enc(name)}`;
export const newFleetHref = "/deployments/releases/fleets/new";
export const fleetHref = (id: string) => `/deployments/releases/fleets/${enc(id)}`;

/** The status dot a release's state shows as. */
export const STATE_DOT: Record<ReleaseState, string> = {
  waiting: "queued",
  waiting_approval: "pending",
  committing: "running",
  delivering: "running",
  succeeded: "succeeded",
  failed: "failed",
  cancelled: "cancelled",
  rolled_back: "skipped",
  superseded: "skipped",
};

export const STATE_WORDS: Record<ReleaseState, string> = {
  waiting: "waiting",
  waiting_approval: "waiting for approval",
  committing: "committing",
  delivering: "delivering",
  succeeded: "succeeded",
  failed: "failed",
  cancelled: "cancelled",
  rolled_back: "rolled back",
  superseded: "superseded",
};

/** Still moving: a registration has at most one release in flight. */
export const inFlight = (s: ReleaseState) => s === "waiting" || s === "waiting_approval" || s === "committing" || s === "delivering";

/** What a release does, in a few words: `deploy 1.4.2`, `rollback to 1.4.1 (release 40)`. */
export function describe(r: Release): string {
  switch (r.kind) {
    case "deploy":
      return r.promoted_from ? `promote ${r.version ?? ""} (from release ${r.promoted_from})` : `deploy ${r.version ?? ""}`;
    case "rollback":
      return `rollback to ${r.version ?? ""}${r.rollback_of ? ` (release ${r.rollback_of})` : ""}`;
    case "revert":
      return `revert of release ${r.reverts ?? ""}`;
    case "config":
      return r.secrets && !r.values ? `secrets: ${r.secrets}` : r.secrets ? `config (and secrets: ${r.secrets})` : "config";
    default:
      return r.kind;
  }
}

/** How a package deploys, in a few words: `oci://ghcr.io/acme/charts web`. */
export function packageWords(p: Package): string {
  switch (p.kind) {
    case "helm":
      return `${p.repo} ${p.chart}`;
    case "git":
      return `${p.repo}${p.path ? ` ${p.path}` : ""}`;
    case "platform":
      return p.provider;
  }
}

/** The host a release repository is on (a GitLab's: the one whose token covers its group), if it's another's. */
export function repoHost(e: Environment, hosts: GitHost[]): GitHost | null {
  const { host, repo } = e.release_repo;
  if (host === "kiln") return null;
  return hosts.find((h) => h.host === host && (!h.groups?.length || h.groups.some((g) => repo.startsWith(`${g.replace(/\/$/, "")}/`)))) ?? null;
}

/** Where a release repository is seen: its page on GitHub, GitLab or Bitbucket, none for kiln's own. */
export function releaseRepoUrl(e: Environment, hosts: GitHost[]): string | null {
  const h = repoHost(e, hosts);
  return h?.web ? `${h.web.replace(/\/$/, "")}/${e.release_repo.repo}` : null;
}

/** A release proposed as a pull request (`via: pull-request`, a merge request on GitLab): its number, where it is, and what the host calls it. */
export function pullRequestOf(r: Release): { number: number; url: string; name: string } | null {
  const pr = r.provider?.pull_request as { number?: unknown; url?: unknown } | undefined;
  if (!pr || typeof pr.number !== "number" || typeof pr.url !== "string") return null;
  const gitlab = pr.url.includes("/-/merge_requests/");
  return { number: pr.number, url: pr.url, name: gitlab ? `merge request !${pr.number}` : `pull request #${pr.number}` };
}

/** The provider's name, as people say it. */
export function providerName(provider: string): string {
  return provider.startsWith("argocd/") ? `Argo CD ${provider.slice("argocd/".length)}` : "KilnCD";
}

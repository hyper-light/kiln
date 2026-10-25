/**
 * Mock data for the pages the simulation in server.ts doesn't cover: deployments (apps, their
 * trees, history, diffs, objects, events, logs, deploy plans), kept plans, System → Updates, the
 * runners' front doors and the tailnet, synced folders, terminal and AI sessions, and the sign-in
 * methods. Deterministic (seeded), so screenshots and layout measurements repeat.
 */
import type http from "node:http";
import type {
  Agent,
  CdApp,
  CdAppDetail,
  CdAutoRollback,
  CdAutoRollbackSetting,
  CdDeployment,
  CdDiff,
  CdEvent,
  CdHealth,
  CdNode,
  CdOperation,
  CdPlan,
  CdPlanRepo,
  CdResourceDetail,
  CdRollout,
  CdSummary,
  CdSync,
  CdTree,
  FrontDoorView,
  Job,
  KilnBinary,
  Run,
  ServerEvent,
  SyncView,
  TailscaleView,
  UpdateJob,
  UpdatePolicy,
  UpdatesOverview,
  UpgradeWaits,
} from "../src/api/types.ts";
import { hashStr, mulberry32, pick } from "./data.ts";

export interface PagesCtx {
  agents: () => Agent[];
  runs: () => Run[];
  version: string;
  hostname: string;
  json: (res: http.ServerResponse, status: number, body: unknown) => void;
  err: (res: http.ServerResponse, status: number, message: string) => void;
  body: <T>(req: http.IncomingMessage) => Promise<T>;
  emit?: (ev: ServerEvent) => void;
  /** kiln's release pipeline in the simulation: Build every platform starts it; `past`, the runs the kept builds came from, newest first. */
  release?: { start: (commit: string) => Run; latest: () => { run: Run; jobs: Job[] } | null; past: () => Run[] };
}

const now = () => Math.floor(Date.now() / 1000);
const T0 = now();
const hex = (r: () => number, n: number) => Array.from({ length: n }, () => Math.floor(r() * 16).toString(16)).join("");

/* ------------------------------------------------------------------ deployments */

const CLUSTERS = ["prod-eu", "prod-us", "staging"];
const PROJECTS = ["platform", "payments", "search", "web"];
const NAMES = ["api", "gateway", "worker", "scheduler", "indexer", "frontend", "billing", "ledger", "auth", "notifier", "ingest", "cache", "exporter", "reports", "checkout"];
const HEALTHS: CdHealth[] = ["Healthy", "Healthy", "Healthy", "Healthy", "Healthy", "Progressing", "Degraded", "Suspended", "Missing"];
const SYNCS: CdSync[] = ["Synced", "Synced", "Synced", "Synced", "OutOfSync", "Unknown"];

interface MockApp {
  app: CdApp;
  detail: CdAppDetail;
  history: CdDeployment[];
  rollouts?: CdRollout[];
  /** Auto-rollback set for the app. */
  autoRollback?: CdAutoRollbackSetting | null;
}

/** Auto-rollback set for every app of a repository, by its URL. */
const repoAutoRollback = new Map<string, CdAutoRollbackSetting>();

const COMMIT_SUBJECTS = [
  "Raise worker memory limit to 1Gi",
  "Roll the api image forward to 1.42.0",
  "Add a canary step at 25% for checkout",
  "Tune HPA thresholds after the Friday spike",
  "Pin the ingress controller's chart to 4.11",
  "Move the session cache to its own namespace and give it a PodDisruptionBudget so node drains stop taking every replica at once",
  'Revert "Enable HTTP/3 on the edge"',
];
const COMMIT_BODIES = [
  "",
  "The worker was OOM-killed twice an hour under the nightly import.\nA heap profile shows the parser holding whole batches; 1Gi fits\nthe largest batch we've seen with room to spare.\n\nRefs: OPS-2211",
  "1.42.0 fixes the slow /search queries (an index the planner skipped).\n\n- api: 1.41.3 → 1.42.0\n- migrations: none\n- config: unchanged\n\nRolled out to staging yesterday; p99 dropped from 840 ms to 190 ms.",
  "Checkout takes 25% for 10 minutes, measured against the error rate\nand p95 latency, before the rest.",
  "Scale-up reacted too late on Friday: CPU sat at 92% for six minutes\nbefore the first new pod. Target 60% instead of 80%, and a shorter\nstabilization window up.\n\nCo-Authored-By: Mira K <mira@example.com>",
  "This reverts commit 4be7e1c0. HTTP/3 broke clients behind two corporate proxies that drop UDP 443 without an ICMP reply, so they waited out the whole handshake timeout before falling back. We'll try again with Alt-Svc only on a canary hostname.",
  "",
];

/** An app's auto-rollback, as the server works it out: its own, its repository's, its deploy file's, else on. */
function autoRollbackOf(a: MockApp): CdAutoRollback {
  const repo = a.detail.origin.kind === "deploy" ? a.detail.origin.repo : (a.detail.definition?.sources[0]?.repo ?? null);
  const repoSetting = repo ? (repoAutoRollback.get(repo) ?? null) : null;
  const app = a.autoRollback ?? null;
  const name = (u: string) => u.replace(/^https:\/\/(github\.com\/)?/, "");
  const by = (s: CdAutoRollbackSetting | null) => (s?.by ? ` by ${s.by}` : "");
  if (app) return { on: app.on, from: "app", why: `set for this app${by(app)}`, app, repo, repo_setting: repoSetting, deploy_file: null };
  if (repoSetting) return { on: repoSetting.on, from: "repo", why: `set for ${name(repo ?? "")}${by(repoSetting)}`, app, repo, repo_setting: repoSetting, deploy_file: null };
  return { on: true, from: "default", why: "kiln's default", app, repo, repo_setting: null, deploy_file: null };
}

/** What a rollback can go to: what succeeded, newest first, what runs now and repeats left out. */
function rollbackTargets(a: MockApp) {
  const current = a.history.find((d) => d.succeeded) ?? null;
  const seen = new Set<string>(current?.revision ? [current.revision] : []);
  const targets = a.history.filter((d) => {
    if (!d.succeeded || !d.revision || seen.has(d.revision)) return false;
    seen.add(d.revision);
    return true;
  });
  return { app: a.detail.app, current, targets, kept: 10, shadow: a.detail.shadow };
}

const apps = new Map<string, MockApp>();
{
  const r = mulberry32(4242);
  let n = 0;
  for (const project of PROJECTS) {
    for (const name of NAMES) {
      if (r() < 0.25) continue;
      const cluster = CLUSTERS[n % CLUSTERS.length]!;
      n++;
      const id = `${project}/${name}`;
      const health = pick(r, HEALTHS);
      const sync = health === "Missing" ? "OutOfSync" : pick(r, SYNCS);
      const resources = 4 + Math.floor(r() * 9);
      const revision = hex(r, 40);
      const running = r() < 0.06;
      const op: CdOperation | null = running
        ? { id: 7, phase: "Running", trigger: "auto", attempt: 1, message: "waiting for healthy state of apps/Deployment/" + name, started_ms: Date.now() - 40_000, finished_ms: null }
        : {
            id: 6,
            phase: health === "Degraded" && r() < 0.5 ? "Failed" : "Succeeded",
            trigger: pick(r, ["manual", "auto", "self-heal"] as const),
            attempt: 1,
            message: "successfully synced (all tasks run)",
            by: r() < 0.5 ? "ada" : null,
            started_ms: Date.now() - Math.floor(r() * 86_400_000),
            finished_ms: Date.now() - Math.floor(r() * 80_000_000),
          };
      const app: CdApp = {
        app: id,
        name,
        project,
        cluster,
        namespace: `${project}-${name}`.slice(0, 24),
        environment: cluster.startsWith("prod") ? "production" : "staging",
        auto: r() < 0.7,
        labels: { team: project },
        // Every third one is kept in kiln (its sync policy is kiln's to change); the rest come from a deploy file.
        origin: n % 3 === 0 ? { kind: "api" } : { kind: "deploy", repo: "https://github.com/hyper-light/deploy" },
        shadow: r() < 0.08,
        sync,
        health,
        revision,
        resources,
        // Out of sync, every kind of difference: a change, one missing, one another app's
        // tracking is on, one lacking kiln's marks, one to prune.
        out_of_sync: sync === "OutOfSync" ? 5 : 0,
        shared: sync === "OutOfSync" ? 1 : 0,
        missing: sync === "OutOfSync" ? 1 : 0,
        unmarked: sync === "OutOfSync" ? 1 : 0,
        prune: sync === "OutOfSync" ? 1 : 0,
        message: health === "Degraded" ? "Deployment " + name + " exceeded its progress deadline" : null,
        operation: op,
        pinned: null,
      };
      // Only an app that syncs by itself is synced by auto-sync or self-heal.
      if (op && !app.auto) op.trigger = "manual";
      const detail: CdAppDetail = {
        ...app,
        definition: {
          name,
          project,
          sources: [{ repo: "https://github.com/hyper-light/deploy", path: `apps/${project}/${name}`, revision: "main", chart: null, as_ref: null }],
          sync: { auto: app.auto, prune: true, self_heal: app.auto, allow_empty: false },
        },
        commits: [revision],
        children: [],
        revision_info: {
          sha: revision,
          author: pick(r, ["ada", "mira.k", "jonas", "ines"]),
          email: "dev@example.com",
          at_ms: Date.now() - Math.floor(r() * 5 * 86_400_000),
          subject: pick(r, ["Bump api to 1.42.0", "Raise worker memory limit", "Add canary for checkout", "Roll indexer back to v3 settings", "Tune HPA thresholds"]),
          body: "",
        },
      };
      // Twelve deployments, newest first: tagged releases among them, messages of every length,
      // one failure, and one render deployed twice.
      const history: CdDeployment[] = Array.from({ length: 12 }, (_, i) => {
        const rev = i === 0 ? revision : hex(r, 40);
        const at = Date.now() - (i + 1) * 9 * 3_600_000;
        const subject = i === 0 ? (detail.revision_info?.subject ?? "") : (COMMIT_SUBJECTS[(i + n) % COMMIT_SUBJECTS.length] ?? `Change ${12 - i} to ${name}`);
        return {
          app: id,
          id: 12 - i,
          op: 12 - i,
          revision: rev,
          commits: [rev],
          trigger: pick(r, ["manual", "auto", "self-heal"]),
          by: r() < 0.4 ? "ada" : null,
          succeeded: i !== 2,
          message: i === 2 ? "one or more objects failed to apply" : "",
          applied: resources,
          pruned: i === 1 ? 1 : 0,
          at_ms: at,
          commit: {
            sha: rev,
            author: pick(r, ["ada", "mira.k", "jonas", "ines"]),
            email: "dev@example.com",
            at_ms: at - 1_800_000,
            subject,
            body: COMMIT_BODIES[(i + n) % COMMIT_BODIES.length] ?? "",
          },
          tags: i % 3 === 1 ? [`v1.${40 - i}.0`, ...(i === 4 ? [`release-${40 - i}`] : [])] : [],
        };
      });
      // Deployment 6 deployed again what 7 did.
      const twin = history[6];
      const again = history[5];
      if (twin && again) Object.assign(again, { revision: twin.revision, commits: twin.commits, commit: twin.commit, tags: twin.tags });
      apps.set(id, { app, detail, history });
    }
  }
}

// Two Lambda apps with rollouts: one ramping with an approval waiting, one aborted by a failing check.
{
  const at = Date.now();
  const lambda = (name: string, health: CdHealth, rollouts: CdRollout[], message: string | null) => {
    const id = `default/${name}`;
    const revision = hashStr(name).toString(16).padStart(8, "0").repeat(5);
    const app: CdApp = {
      app: id,
      name,
      project: "default",
      cluster: "aws-lambda/aws-prod/us-east-1",
      namespace: "",
      environment: "production",
      auto: true,
      labels: {},
      origin: { kind: "deploy", repo: "https://github.com/hyper-light/thumbs" },
      shadow: false,
      sync: "Synced",
      health,
      revision,
      resources: 3,
      out_of_sync: 0,
      shared: 0,
      missing: 0,
      unmarked: 0,
      prune: 0,
      message,
      operation: { id: 12, phase: "Succeeded", trigger: "auto", attempt: 1, message: "successfully synced (all tasks run)", by: null, started_ms: at - 300_000, finished_ms: at - 290_000 },
      pinned: null,
    };
    const detail: CdAppDetail = {
      ...app,
      definition: {
        sources: [{ repo: "https://github.com/hyper-light/thumbs", path: ".kiln/deploy.yaml", revision: "main", chart: null, as_ref: null }],
        sync: { auto: true, prune: true, self_heal: true, allow_empty: false },
      },
      commits: [revision],
      children: [],
      revision_info: { sha: revision, author: "ada", email: "dev@example.com", at_ms: at - 360_000, subject: "Resize thumbnails in WebP", body: "" },
    };
    apps.set(id, { app, detail, history: [], rollouts });
  };
  const run = (name: string, against: "preview" | "production", passed: number, failed: number, verdict: CdRollout["checks"][number]["verdict"], why = "", asked = false) => ({
    name,
    against,
    measurements: [...Array(passed).fill("pass"), ...Array(failed).fill("fail")].map((v, i) => ({
      at_ms: at - (passed + failed - i) * 20_000,
      verdict: v,
      value: { status: v === "pass" ? 200 : 503 },
      message: v === "pass" ? "" : "503 from /healthz",
    })),
    tally: { passed, failed, inconclusive: 0, errors: 0, streak: passed, errors_in_a_row: 0 },
    asked: asked ? { id: 9, at_ms: at - 2_000 } : null,
    next_ms: at + 20_000,
    verdict,
    why,
    timing: { count: 5, every_ms: 20_000, dry_run: false },
  });
  const steps: CdRollout["steps"] = [
    { step: "checks", names: ["smoke"], against: "preview", parallel: true },
    { step: "weight", weight: 10 },
    { step: "checks", names: ["errors", "release-ok"], against: "production", parallel: true },
    { step: "weight", weight: 50 },
    { step: "pause", ms: 300_000 },
    { step: "weight", weight: 100 },
    { step: "watch", names: ["errors"], window_ms: 600_000 },
  ];
  lambda(
    "thumbs-production",
    "Progressing",
    [
      {
        id: "aws-lambda/aws-prod/us-east-1|lambda.aws.kiln.dev/Alias//thumbs:live",
        target: "aws-lambda/aws-prod/us-east-1",
        kind: "Alias",
        name: "thumbs:live",
        revision: "9c1e4f2a7b3d",
        previous_revision: "41d0b7e3c2aa",
        back_to: 11,
        blue: "41",
        green: "42",
        preview_url: "https://thumbs-live-preview.lambda-url.us-east-1.on.aws/",
        started_ms: at - 240_000,
        updated_ms: at - 2_000,
        status: { state: "running", why: "", held: false, weight: 10, target: 10, step: 3, steps: 7, waiting: "" },
        steps,
        step: 2,
        auto_rollback: true,
        ready: true,
        checks: [run("errors", "production", 3, 0, null, "", true), run("release-ok", "production", 0, 0, null, "", true)],
        watching: [],
        log: [
          { event: "read", at_ms: at - 238_000, from: 0, to: 0 },
          { event: "ready", at_ms: at - 236_000 },
          { event: "checked", at_ms: at - 200_000, step: 0, background: false, check: { name: "smoke", verdict: "pass" } },
          { event: "moved", at_ms: at - 190_000, weight: 10 },
        ],
        dropped: 0,
        waiting: ["release-ok"],
      },
    ],
    null,
  );
  lambda(
    "refunds-production",
    "Degraded",
    [
      {
        id: "aws-lambda/aws-prod/us-east-1|lambda.aws.kiln.dev/Alias//refunds:live",
        target: "aws-lambda/aws-prod/us-east-1",
        kind: "Alias",
        name: "refunds:live",
        revision: "77ab90de1c43",
        previous_revision: "5e2f0a9b8c11",
        back_to: 7,
        blue: "17",
        green: "18",
        preview_url: null,
        started_ms: at - 900_000,
        updated_ms: at - 600_000,
        status: { state: "aborted", why: "check errors failed 2 times (limit 1)", held: false, weight: 0, target: 0, step: 3, steps: 7, waiting: "" },
        steps,
        step: 2,
        auto_rollback: false,
        ready: true,
        checks: [],
        watching: [],
        log: [
          { event: "moved", at_ms: at - 700_000, weight: 10 },
          { event: "measured", at_ms: at - 660_000, check: "errors", n: 2, verdict: "fail", message: "503 from /healthz", decided: "fail" },
          { event: "aborted", at_ms: at - 660_000, why: "check errors failed 2 times (limit 1)", step: 2 },
          { event: "moved", at_ms: at - 655_000, weight: 0 },
          { event: "reverted", at_ms: at - 655_000 },
          { event: "released", at_ms: at - 600_000, version: "green" },
        ],
        dropped: 0,
        waiting: [],
      },
    ],
    "rollout aborted: check errors failed 2 times (limit 1)",
  );
}

function listApps(q: URLSearchParams) {
  const want = (k: string) => (q.get(k) ?? "").split(",").filter(Boolean);
  const f = { sync: want("sync"), health: want("health"), cluster: want("cluster"), project: want("project"), namespace: want("namespace") };
  const text = (q.get("q") ?? "").toLowerCase();
  const match = (a: CdApp, skip?: keyof typeof f) =>
    (skip === "sync" || !f.sync.length || f.sync.includes(a.sync)) &&
    (skip === "health" || !f.health.length || f.health.includes(a.health)) &&
    (skip === "cluster" || !f.cluster.length || f.cluster.includes(a.cluster)) &&
    (skip === "project" || !f.project.length || f.project.includes(a.project)) &&
    (skip === "namespace" || !f.namespace.length || f.namespace.includes(a.namespace)) &&
    (!text || a.app.toLowerCase().includes(text));
  const all = [...apps.values()].map((x) => x.app);
  return { all, match, f };
}

function facets(q: URLSearchParams) {
  const { all, match } = listApps(q);
  const count = (key: "sync" | "health" | "cluster" | "project" | "namespace") => {
    const m: Record<string, number> = {};
    for (const a of all) if (match(a, key)) m[a[key]] = (m[a[key]] ?? 0) + 1;
    return m;
  };
  return { total: all.filter((a) => match(a)).length, facets: { sync: count("sync"), health: count("health"), cluster: count("cluster"), project: count("project"), namespace: count("namespace") } };
}

function summary(): CdSummary {
  const s: CdSummary = { apps: apps.size, sync: {}, health: {}, clusters: CLUSTERS.length };
  for (const { app } of apps.values()) {
    s.sync[app.sync] = (s.sync[app.sync] ?? 0) + 1;
    s.health[app.health] = (s.health[app.health] ?? 0) + 1;
  }
  return s;
}

const HRANK: Record<string, number> = { Degraded: 0, Missing: 1, Progressing: 2, Suspended: 3, Unknown: 4, Healthy: 5 };
const SRANK: Record<string, number> = { OutOfSync: 0, Unknown: 1, Synced: 2 };

function node(r: () => number, p: Partial<CdNode> & Pick<CdNode, "kind" | "name" | "namespace">): CdNode {
  return {
    id: p.uid ?? `${p.group ?? ""}/${p.kind}:${p.namespace}/${p.name}`,
    uid: null,
    group: "",
    version: "v1",
    parents: [],
    managed: false,
    health: "Healthy",
    message: null,
    sync: null,
    hook: false,
    missing: false,
    requires_pruning: false,
    why: null,
    created_at: new Date(Date.now() - Math.floor(r() * 9 * 86_400_000)).toISOString(),
    info: [],
    status: null,
    ready: null,
    restarts: null,
    images: [],
    containers: [],
    node: null,
    ...p,
  };
}

function tree(id: string): CdTree | null {
  const a = apps.get(id)?.app;
  if (!a) return null;
  const r = mulberry32(hashStr(id));
  const ns = a.namespace;
  const nodes: CdNode[] = [];
  const managed = (kind: string, name: string, group = "", extra: Partial<CdNode> = {}) => {
    const uid = `uid-${kind}-${name}`;
    const n = node(r, {
      kind,
      name,
      namespace: ns,
      group,
      uid,
      id: uid,
      managed: true,
      sync: a.sync === "OutOfSync" && kind === "Deployment" ? "OutOfSync" : "Synced",
      why: a.sync === "OutOfSync" && kind === "Deployment" ? ".spec.replicas differs" : null,
      ...extra,
    });
    nodes.push(n);
    return n;
  };
  const dep = managed("Deployment", a.name, "apps", {
    health: a.health === "Missing" ? "Missing" : a.health,
    message: a.health === "Degraded" ? `Deployment "${a.name}" exceeded its progress deadline` : null,
    ready: "3/3",
    info: [{ name: "Ready", value: "3/3" }],
  });
  const off = a.sync === "OutOfSync";
  managed(
    "Service",
    a.name,
    "",
    off
      ? {
          sync: "OutOfSync",
          tracked_by: `default/${a.name}-argo`,
          shared_with: [`default/${a.name}-argo`],
          marks_only: true,
          why: `app default/${a.name}-argo's tracking is on it: a sync takes it over`,
        }
      : {},
  );
  managed("ConfigMap", `${a.name}-config`, "", {
    health: null,
    info: [{ name: "Keys", value: "4" }],
    ...(off ? { sync: "OutOfSync" as const, marks_only: true, why: ".metadata.labels.kiln.dev/app is missing live" } : {}),
  });
  if (off) {
    managed("PodDisruptionBudget", a.name, "policy", { health: "Missing", sync: "OutOfSync", missing: true, why: "not in the cluster" });
    managed("ConfigMap", `${a.name}-old`, "", { health: null, sync: "OutOfSync", requires_pruning: true, why: "not in git: requires pruning" });
  }
  managed("ServiceAccount", a.name, "", { health: null });
  if (r() < 0.6) managed("Ingress", a.name, "networking.k8s.io");
  if (r() < 0.5) managed("HorizontalPodAutoscaler", a.name, "autoscaling");
  for (let rs = 0; rs < 2; rs++) {
    const rsUid = `uid-rs-${a.name}-${rs}`;
    const hash = hex(r, 9);
    nodes.push(
      node(r, {
        kind: "ReplicaSet",
        name: `${a.name}-${hash}`,
        namespace: ns,
        group: "apps",
        uid: rsUid,
        id: rsUid,
        parents: [dep.uid!],
        info: [{ name: "Revision", value: String(7 - rs) }],
        ready: rs === 0 ? "3/3" : "0/0",
      }),
    );
    if (rs > 0) continue;
    for (let p = 0; p < 3; p++) {
      const podUid = `uid-pod-${a.name}-${p}`;
      const bad = a.health === "Degraded" && p === 2;
      nodes.push(
        node(r, {
          kind: "Pod",
          name: `${a.name}-${hash}-${hex(r, 5)}`,
          namespace: ns,
          uid: podUid,
          id: podUid,
          parents: [rsUid],
          health: bad ? "Degraded" : "Healthy",
          message: bad ? "back-off 5m0s restarting failed container" : null,
          status: bad ? "CrashLoopBackOff" : "Running",
          ready: bad ? "0/1" : "1/1",
          restarts: bad ? 14 : 0,
          node: pick(r, ["node-a1", "node-a2", "node-b1"]),
          images: [`ghcr.io/hyper-light/${a.name}:1.42.0`],
          containers: [
            {
              name: a.name,
              image: `ghcr.io/hyper-light/${a.name}:1.42.0`,
              init: false,
              ready: !bad,
              restarts: bad ? 14 : 0,
              state: bad ? "waiting" : "running",
              reason: bad ? "CrashLoopBackOff" : null,
              last: bad ? "Error (exit 1)" : null,
            },
          ],
        }),
      );
    }
  }
  nodes.push(
    node(r, {
      kind: "EndpointSlice",
      name: `${a.name}-${hex(r, 5)}`,
      namespace: ns,
      group: "discovery.k8s.io",
      uid: `uid-eps-${a.name}`,
      id: `uid-eps-${a.name}`,
      parents: [`uid-Service-${a.name}`],
      health: null,
      ready: "3",
    }),
  );
  return { app: id, cluster: a.cluster, namespace: ns, nodes, truncated: false, errors: [], at_ms: Date.now() };
}

function yamlOf(n: CdNode, desired: boolean) {
  return [
    `apiVersion: ${n.group ? `${n.group}/` : ""}${n.version}`,
    `kind: ${n.kind}`,
    "metadata:",
    `  name: ${n.name}`,
    `  namespace: ${n.namespace}`,
    "  labels:",
    `    app.kubernetes.io/name: ${n.name}`,
    ...(desired ? [] : [`  uid: ${n.uid ?? "?"}`, `  creationTimestamp: ${n.created_at}`]),
    "spec:",
    `  replicas: ${desired ? 3 : 2}`,
  ].join("\n");
}

function resourceDetail(id: string, key: string): CdResourceDetail | null {
  const t = tree(id);
  if (!t) return null;
  const [group, kind, namespace, name] = key.split("~");
  const n = t.nodes.find((x) => x.kind === kind && x.name === name && x.group === (group ?? ""));
  if (!n) return null;
  const owners = n.parents.map((u) => t.nodes.find((x) => x.uid === u)).filter((x): x is CdNode => Boolean(x));
  return {
    key: { group: group ?? "", kind: kind ?? "", namespace: namespace ?? "", name: name ?? "" },
    managed: n.managed,
    node: n,
    live: yamlOf(n, false),
    desired: n.managed ? yamlOf(n, true) : null,
    diff: n.managed
      ? {
          key: { group: n.group, kind: n.kind, namespace: n.namespace, name: n.name },
          why: n.why,
          live: "",
          predicted: "",
          hunks: n.sync === "OutOfSync" ? [{ old_start: 9, old_lines: 1, new_start: 9, new_lines: 1, lines: ["-  replicas: 2", "+  replicas: 3"] }] : [],
        }
      : null,
    labels: { "app.kubernetes.io/name": n.name, "app.kubernetes.io/part-of": t.app.split("/")[0] ?? "" },
    annotations: n.kind === "Deployment" ? { "deployment.kubernetes.io/revision": "7" } : {},
    conditions:
      n.kind === "Deployment" || n.kind === "Pod"
        ? [
            {
              type: "Available",
              status: n.health === "Degraded" ? "False" : "True",
              reason: "MinimumReplicasAvailable",
              message: "Deployment has minimum availability.",
              last: new Date(Date.now() - 3_600_000).toISOString(),
            },
          ]
        : [],
    finalizers: [],
    owners: owners.map((o) => ({ group: o.group, kind: o.kind, namespace: o.namespace, name: o.name, id: o.id, uid: o.uid })),
    errors: [],
  };
}

function events(): CdEvent[] {
  const at = (m: number) => new Date(Date.now() - m * 60_000).toISOString();
  return [
    { type: "Normal", reason: "Scheduled", message: "Successfully assigned to node-a1", count: 1, first: at(40), last: at(40), source: "default-scheduler", object: { kind: "Pod", name: null } },
    { type: "Normal", reason: "Pulled", message: "Container image already present on machine", count: 3, first: at(38), last: at(12), source: "kubelet", object: { kind: "Pod", name: null } },
    { type: "Warning", reason: "BackOff", message: "Back-off restarting failed container", count: 14, first: at(30), last: at(2), source: "kubelet", object: { kind: "Pod", name: null } },
  ];
}

function logs(name: string) {
  const r = mulberry32(hashStr(name));
  const lines = Array.from({ length: 80 }, (_, i) => ({
    pod: `${name}-pod`,
    ts: new Date(Date.now() - (80 - i) * 4000).toISOString(),
    text: `${pick(r, ["INFO", "INFO", "INFO", "WARN", "DEBUG"])} ${pick(r, ["request served", "cache hit", "connected to db", "flushed 120 rows", "health check ok", "retrying upstream"])} id=${hex(r, 8)} took=${Math.floor(r() * 90)}ms`,
  }));
  return { pods: [`${name}-pod`], more_pods: 0, container: null, containers: [name], lines, errors: [] };
}

function diffOf(id: string): CdDiff[] {
  const a = apps.get(id)?.app;
  if (!a || a.sync !== "OutOfSync") return [];
  const ns = a.namespace;
  const marks = (app: string) => [
    { old_start: 4, old_lines: 3, new_start: 4, new_lines: 3, lines: ["   labels:", `-    app.kubernetes.io/instance: ${app}`, `+    kiln.dev/app: 0123456789abcdef`, `   name: ${a.name}`] },
  ];
  return [
    {
      key: { group: "apps", kind: "Deployment", namespace: ns, name: a.name },
      why: ".spec.replicas differs",
      live: "",
      predicted: "",
      hunks: [{ old_start: 7, old_lines: 3, new_start: 7, new_lines: 3, lines: [" spec:", "-  replicas: 2", "+  replicas: 3", "   revisionHistoryLimit: 5"] }],
    },
    {
      key: { group: "", kind: "Service", namespace: ns, name: a.name },
      why: `app default/${a.name}-argo's tracking is on it: a sync takes it over`,
      live: "",
      predicted: "",
      hunks: marks(`${a.name}-argo`),
      tracked_by: `default/${a.name}-argo`,
      marks_only: true,
    },
    {
      key: { group: "", kind: "ConfigMap", namespace: ns, name: `${a.name}-config` },
      why: ".metadata.labels.kiln.dev/app is missing live",
      live: "",
      predicted: "",
      hunks: marks(""),
      marks_only: true,
    },
    {
      key: { group: "policy", kind: "PodDisruptionBudget", namespace: ns, name: a.name },
      why: "not in the cluster",
      live: "",
      predicted: "",
      hunks: [{ old_start: 0, old_lines: 0, new_start: 1, new_lines: 2, lines: ["+kind: PodDisruptionBudget", `+name: ${a.name}`] }],
      missing: true,
    },
    {
      key: { group: "", kind: "ConfigMap", namespace: ns, name: `${a.name}-old` },
      why: "not in git: requires pruning",
      live: "",
      predicted: "",
      hunks: [{ old_start: 1, old_lines: 2, new_start: 0, new_lines: 0, lines: ["-kind: ConfigMap", `-name: ${a.name}-old`] }],
      requires_pruning: true,
    },
  ];
}

const PLAN_REPOS: CdPlanRepo[] = [
  {
    url: "https://github.com/hyper-light/deploy",
    urls: ["https://github.com/hyper-light/deploy", "git@github.com:hyper-light/deploy.git"],
    apps: apps.size,
    default_branch: "main",
    branches: [
      { name: "main", commit: "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678", at_ms: Date.now() - 3_600_000 },
      { name: "feature/checkout", commit: "b2c3d4e5f60718293a4b5c6d7e8f901234567890", at_ms: Date.now() - 7_200_000 },
      { name: "fix/worker-memory", commit: "c3d4e5f60718293a4b5c6d7e8f90123456789012", at_ms: Date.now() - 86_400_000 },
    ],
  },
];

function plan(ask: { head?: string; commit?: string; pr?: number }): CdPlan {
  const target = ask.head ?? ask.commit ?? `#${ask.pr ?? ""}`;
  const app = (name: string, ok = true) => ({
    app: `payments/${name}`,
    cluster: "prod-eu",
    namespace: `payments-${name}`,
    auto: true,
    auto_prune: false,
    shadow: false,
    touched: true,
    ok,
    error: null,
    problems: ok ? [] : ["Deployment payments-checkout/checkout: spec.template.spec.containers[0].image: Required value"],
    notes: [],
    own: {
      added: 0,
      changed: 1,
      removed: 0,
      objects: [
        {
          key: { group: "apps", kind: "Deployment", namespace: `payments-${name}`, name },
          change: "changed" as const,
          why: ".spec.replicas differs",
          hook: false,
          added_lines: 1,
          removed_lines: 1,
          hunks: [{ old_start: 7, old_lines: 2, new_start: 7, new_lines: 2, lines: [" spec:", "-  replicas: 2", "+  replicas: 3"] }],
        },
      ],
    },
    stack: { added: 0, changed: 1, removed: 0, objects: [] },
    sync: { create: 0, update: 1, prune: 0, hooks: 0, unchanged: 6, unread: 0, held: [], shared: [], steps: [] },
  });
  return {
    repo: PLAN_REPOS[0]!.url,
    trunk: { branch: "main", commit: PLAN_REPOS[0]!.branches[0]!.commit },
    base: PLAN_REPOS[0]!.branches[0]!.commit,
    ok: !target.includes("fail"),
    apps: { affected: 2, planned: 2, unaffected: apps.size - 2, unplanned: [] },
    notes: [],
    diffs: [
      { commit: { sha: "d4e5f60718293a4b5c6d7e8f90123456789012ab", author: "ada", at_ms: Date.now() - 5_400_000, subject: "Raise checkout replicas" }, ok: true, apps: [app("checkout")] },
      {
        commit: { sha: "e5f60718293a4b5c6d7e8f90123456789012abcd", author: "ada", at_ms: Date.now() - 3_600_000, subject: `Ledger on ${target}` },
        ok: !target.includes("fail"),
        apps: [app("ledger", !target.includes("fail"))],
      },
    ],
  };
}

/* ------------------------------------------------------------------ plans */

/** A goal turned into a plan here (Make a plan): a plan being planned, its words the goal's. */
export function planning(goal: { agent: string; id: string; text: string }): string {
  const id = `pl_${Math.random().toString(36).slice(2, 8)}`;
  const now = Math.floor(Date.now() / 1000);
  const title = goal.text.split(/[.:—]/)[0]!.trim().slice(0, 80);
  PLANS.unshift({
    ...PLANS[0]!,
    id,
    title,
    body: goal.text,
    items: [],
    steps: [],
    tags: [],
    version: 1,
    runs: 0,
    updated_at: now,
    share: undefined,
    phase: "planning",
    research: undefined,
    source: { goal },
  } as unknown as (typeof PLANS)[number]);
  return id;
}

/** The plan Release prep proposed (plan mode), as its conversation has it and as kiln keeps it (`pl_proposed`). */
export const PROPOSED = `# Fix: a mount taken over by a restarted daemon dropped a store

The successor's handle store starts fresh, so a page re-fetched after the takeover was never tracked, and its write-back was judged fresh.

1. Reproduce it: the CLI test that writes through a mount taken over by a restarted daemon fails 3 of 3.
2. Track handles the successor never saw opened: \`WritableOpens\` keyed by handle and inode (coherence.rs).
3. Adopt an unseen handle on a restored mount before serving its write (channel.rs).
4. Tests, red first: the CLI test and the coherence unit tests, then the guest tests on the box.
5. Verify: workspace clippy, the doc gate, and CI.`;

const PLANS = [
  {
    id: "pl_search",
    title: "Search across agents' conversations",
    body: "Agents can search every conversation they may read.",
    items: [
      { title: "Store: search by agent and time", description: "", examples: "", acceptance: [], testing: [], needs: [] },
      { title: "Op: conversations.search", description: "", examples: "", acceptance: [], testing: [], needs: [1] },
      { title: "Agents use it in code mode", description: "", examples: "", acceptance: [], testing: [], needs: [1] },
      { title: "UI: search in Mission Control", description: "", examples: "", acceptance: [], testing: [], needs: [2, 3] },
      { title: "Docs and the kiln-agents skill", description: "", examples: "", acceptance: [], testing: [], needs: [2] },
      { title: "Bench: a million events under 50 ms", description: "", examples: "", acceptance: [], testing: [], needs: [2] },
      { title: "WebKit and phone check", description: "", examples: "", acceptance: [], testing: [], needs: [4] },
      { title: "Ship it", description: "", examples: "", acceptance: [], testing: [], needs: [4, 5, 6, 7] },
    ],
    tags: [],
    source: null,
    share: null,
    version: 1,
    created_at: T0 - 7_200,
    updated_at: T0 - 3_600,
    steps: [
      "Store: search by agent and time",
      "Op: conversations.search",
      "Agents use it in code mode",
      "UI: search in Mission Control",
      "Docs and the kiln-agents skill",
      "Bench: a million events under 50 ms",
      "WebKit and phone check",
      "Ship it",
    ],
    runs: 0,
    phase: "ready",
    research: null,
  },
  {
    id: "pl_proposed",
    title: "Fix: a mount taken over by a restarted daemon dropped a store",
    body: PROPOSED,
    items: [],
    tags: [],
    source: { session: "fam-orch", agent: "Release prep", harness: "claude", proposed: true },
    share: null,
    version: 2,
    created_at: T0 - 40 * 60,
    updated_at: T0 - 20 * 60,
    steps: [
      "Reproduce it: the CLI test that writes through a mount taken over by a restarted daemon fails 3 of 3.",
      "Track handles the successor never saw opened: `WritableOpens` keyed by handle and inode (coherence.rs).",
      "Adopt an unseen handle on a restored mount before serving its write (channel.rs).",
      "Tests, red first: the CLI test and the coherence unit tests, then the guest tests on the box.",
      "Verify: workspace clippy, the doc gate, and CI.",
    ],
    runs: 0,
    phase: "ready",
    research: null,
  },
  {
    id: "pl_release",
    title: "Cut the 0.2 release",
    body: "Everything that has to happen before 0.2 ships.",
    items: [
      {
        title: "Freeze main",
        description: "Stop merging features; only fixes from here.",
        examples: "",
        acceptance: [{ kind: "qualitative", text: "Branch protection requires a release label." }],
        testing: [],
      },
      {
        title: "Run the full bench",
        description: "Compare against 0.1 on the metal runners.",
        examples: "",
        acceptance: [{ kind: "quantitative", target: "p99 latency", op: "rises_at_most", value: "3", unit: "%", baseline: "0.1", measured: "kiln run bench --all" }],
        testing: [{ scenario: "The full bench on metal-epyc-01", criteria: [{ kind: "qualitative", text: "Every case runs to the end." }] }],
        needs: [1],
      },
      { title: "Tag and publish", description: "", examples: "", acceptance: [], testing: [], needs: [1, 2] },
    ],
    tags: ["release"],
    source: null,
    share: "shr_7f3a91",
    version: 4,
    created_at: T0 - 86_400 * 6,
    updated_at: T0 - 3_600 * 5,
    steps: ["Freeze main", "Run the full bench", "Tag and publish"],
    runs: 2,
    phase: "ready",
    research: null,
  },
  {
    id: "pl_quic",
    title: "QUIC replication",
    body: "Goal: Add full hyperaraft dual UDP/QUIC stack replication to our metadata store without impacting the 4-5x lead on Splinter and RocksDB",
    items: [
      {
        title: "Replicate over QUIC",
        description: "Carry the metadata store's raft log over hyper-quic.",
        examples: "",
        acceptance: [{ kind: "qualitative", text: "The replication suite passes on three nodes." }],
        testing: [],
        basis: ["f1", "f2"],
      },
      {
        title: "Bench with replication on",
        description: "",
        examples: "",
        acceptance: [
          { kind: "qualitative", text: "Fills stay at least 4x ahead of RocksDB." },
          { kind: "qualitative", text: "Gets and seeks stay at least 4x ahead of Splinter." },
        ],
        testing: [],
        needs: [1],
        basis: ["f3"],
      },
      { title: "Document the replication protocol", description: "", examples: "", acceptance: [], testing: [], needs: [1], basis: ["f1"] },
    ],
    tags: ["from a goal"],
    source: {
      session: "fam-orch",
      agent: "Release prep",
      goal: { agent: "fam-orch", id: "g-quic", text: "Add full hyperaraft dual UDP/QUIC stack replication to our metadata store without impacting the 4-5x lead on Splinter and RocksDB" },
    },
    share: null,
    version: 1,
    created_at: T0 - 3 * 3600,
    updated_at: T0 - 2 * 3600,
    steps: ["Replicate over QUIC", "Bench with replication on", "Document the replication protocol"],
    runs: 1,
    phase: "planning",
    research: "rs_quic",
  },
  // Release prep's goals being planned now, each by a subagent of its own (agents.ts): a subagent at
  // "The planner", one asked for "Store and transfer", one that stopped before "Optimize" was ready.
  ...(
    [
      ["pl_planner", "The planner, researched and audited", "rs_planner", "g-planner"],
      ["pl_store", "Build the data storage/transfer layer and replication using hyper-raft and its crates", "rs_store", "g-store"],
      ["pl_opt", "Optimize the QUIC API and s3 HTTP API using the same benchmarking and profiling we have in steps 1-3", "rs_opt", "g-opt"],
    ] as const
  ).map(([id, title, research, goal]) => ({
    id,
    title,
    body: "",
    items: [],
    tags: ["from a goal"],
    source: { session: "fam-orch", agent: "Release prep", goal: { agent: "fam-orch", id: goal, text: title } },
    share: null,
    version: 1,
    created_at: T0 - 600,
    updated_at: T0 - 60,
    steps: [],
    runs: 0,
    phase: "planning",
    research,
  })),
  {
    id: "pl_parser",
    title: "Fix the YAML anchor parser",
    body: "1. Read the failing test\n2. Fix the parser\n3. Run the suite",
    items: [],
    tags: ["parser", "bug"],
    source: { harness: "claude", agent: "claude-7", machine: "metal-ryzen-01" },
    share: null,
    version: 1,
    created_at: T0 - 86_400 * 2,
    updated_at: T0 - 86_400 * 2,
    steps: ["Read the failing test", "Fix the parser", "Run the suite"],
    runs: 0,
    phase: "ready",
    research: null,
  },
  {
    id: "pl_docs",
    title: "Docs: front door and certificates",
    body: "Write up the one-time setup each platform needs.",
    items: [],
    tags: ["docs"],
    source: null,
    share: null,
    version: 2,
    created_at: T0 - 86_400 * 12,
    updated_at: T0 - 86_400 * 3,
    steps: [],
    runs: 1,
    phase: "ready",
    research: null,
  },
];

const conf = (score: number, why: string, extra: Record<string, number> = {}) => {
  const grade = score >= 85 ? "A" : score >= 70 ? "B" : score >= 50 ? "C" : score >= 30 ? "D" : "F";
  const words = { A: "high", B: "good", C: "moderate", D: "low", F: "very low" }[grade];
  return { score, grade, words, support: score / 100, against: 0, reliability: 0.9, citation: 1, independent: 2, cites: 2, found: 2, failed: 0, why, ...extra };
};

/** Research (docs/research.md), as the server answers it: a plan's planning, its findings graded. */
const RESEARCH = {
  rs_quic: {
    id: "rs_quic",
    question: "Add full hyperaraft dual UDP/QUIC stack replication to our metadata store without impacting the 4-5x lead on Splinter and RocksDB",
    agent: "fam-orch",
    by: "agent:Release prep",
    plan: "pl_quic",
    sources: [
      { id: "s1", type: "code", title: "hyper-raft's log transport", locator: "crates/hyper-raft/src/transport.rs:88-140", retrieved_at: Date.now() - 3 * 3600_000 },
      { id: "s2", type: "measurement", title: "the replication suite", locator: "cargo test -p meta-store --test replication", retrieved_at: Date.now() - 3 * 3600_000 },
      { id: "s3", type: "standard", title: "RFC 9000: QUIC", locator: "https://www.rfc-editor.org/rfc/rfc9000#section-2.2", published: "2021-05", retrieved_at: Date.now() - 3 * 3600_000 },
      { id: "s4", type: "blog", title: "QUIC under load", locator: "https://example.dev/quic-under-load", published: "2023", retrieved_at: Date.now() - 3 * 3600_000 },
      { id: "s5", type: "forum", title: "a thread on QUIC and head-of-line blocking", locator: "https://forum.example.org/t/4812", published: "2022", retrieved_at: Date.now() - 3 * 3600_000 },
    ],
    findings: [
      {
        id: "f1",
        claim: "hyper-raft's log transport takes any stream with ordered delivery, so a QUIC stream carries it as it is.",
        cites: [
          { source: "s1", quote: "pub trait LogStream: AsyncRead + AsyncWrite + Unpin + Send", at: "line 88", check: { state: "found", at: Date.now() - 3 * 3600_000 } },
          { source: "s3", quote: "Streams in QUIC provide a lightweight, ordered byte-stream abstraction", check: { state: "found", at: Date.now() - 3 * 3600_000 } },
        ],
        against: [],
        at: Date.now() - 3 * 3600_000,
        confidence: conf(98, "2 independent sources (code, a standard); 2 of 2 quotes found where cited"),
        against_weights: [],
      },
      {
        id: "f2",
        claim: "The replication suite passes over QUIC on three nodes today.",
        cites: [{ source: "s2", quote: "test result: ok. 48 passed; 0 failed", check: { state: "found", at: Date.now() - 2 * 3600_000, said: "in the agent's own tool output" } }],
        against: [],
        at: Date.now() - 2 * 3600_000,
        confidence: conf(95, "1 independent source (a measurement); 1 of 1 quotes found where cited", { independent: 1, cites: 1, found: 1, reliability: 0.95 }),
        against_weights: [],
      },
      {
        id: "f3",
        claim: "QUIC's per-stream ordering keeps tail latency within 10% of TCP under load.",
        cites: [{ source: "s4", quote: "p99 stayed within 8% of TCP at 40k req/s", check: { state: "not_found", at: Date.now() - 3 * 3600_000, said: "the page doesn't hold those words" } }],
        against: [{ claim: "Head-of-line blocking returns when one stream carries everything.", cites: [{ source: "s5", check: { state: "found", at: Date.now() - 3 * 3600_000 } }] }],
        at: Date.now() - 3 * 3600_000,
        confidence: conf(13, "1 independent source (a blog); 0 of 1 quotes found where cited, 1 not; 1 counterclaim, the strongest moderate", {
          independent: 1,
          cites: 1,
          found: 0,
          failed: 1,
          reliability: 0.5,
          citation: 0.3,
          against: 0.2,
          support: 0.15,
        }),
        against_weights: [0.2],
      },
    ],
    skipped: {},
    summary: { findings: 3, grades: { A: 2, F: 1 }, sources: 5, internal: 2, external: 3, checking: 0 },
    created_at: Date.now() - 3 * 3600_000,
    updated_at: Date.now() - 2 * 3600_000,
  },
  // The research of the goals being planned now (agents.ts): what their subagents found so far.
  ...Object.fromEntries(
    (
      [
        [
          "rs_planner",
          "pl_planner",
          "The planner, researched and audited",
          [
            "DuckDB enumerates join orders with DPhyp and falls back to a greedy order past 12 relations.",
            "Umbra's adaptive compilation picks its plan before it compiles, from the same cost model.",
            "Our planner costs a join by its inputs' row counts alone, with no distinct counts.",
          ],
          { A: 2, B: 1 },
        ],
        ["rs_store", "pl_store", "Build the data storage/transfer layer", [], {}],
        [
          "rs_opt",
          "pl_opt",
          "Optimize the QUIC API and s3 HTTP API",
          ["The s3 API's PUT path copies each part twice before it reaches the store.", "hyper-quic's send path allocates a frame per write."],
          { B: 1, C: 1 },
        ],
      ] as const
    ).map(([id, plan, question, claims, grades]) => [
      id,
      {
        id,
        question,
        agent: "fam-orch",
        by: "agent:Release prep",
        plan,
        sources: [],
        findings: claims.map((claim, i) => ({
          id: `${id}-f${i + 1}`,
          claim,
          cites: [],
          against: [],
          at: Date.now() - 60_000,
          confidence: conf(80, "1 independent source; 1 of 1 quotes found where cited"),
          against_weights: [],
        })),
        skipped: {},
        summary: { findings: claims.length, grades, sources: claims.length, internal: claims.length, external: 0, checking: 0 },
        created_at: Date.now() - 10 * 60_000,
        updated_at: Date.now() - 60_000,
      },
    ]),
  ),
} as Record<string, unknown>;

/* ------------------------------------------------------------------ the rest */

const NEWEST = "94f0847a1b2c3d4e5f60718293a4b5c6d7e8f901";

/** How long upgrades are waited on: kiln.toml's, unless changed in System → Updates. */
const WAITS_CONFIG: UpgradeWaits = { upgrade_minutes: 15, wait_for_jobs_minutes: 360, build_minutes: 60 };
let waitsChosen: UpgradeWaits | null = null;
/** Upgrades started from the page, by runner. */
const started = new Map<string, UpdateJob>();
/** Runners an upgrade from the page brought to the newest kiln. */
const upgraded = new Set<string>();
/** Update policies chosen on the page, by runner (else automatic). */
const policies = new Map<string, UpdatePolicy>();
/** When each platform's kept binary was removed on the page (unix seconds), and what was uploaded, or a machine built of itself, for a platform since. */
const removedAt = new Map<string, number>();
const uploaded = new Map<string, KilnBinary>();
const machineBuilt = new Map<string, KilnBinary>();
/** The platforms each job of the release pipeline builds (`.kiln/release.yaml`), as the mock's targets name them. */
const RELEASE_TARGETS: Record<string, string[]> = { linux: ["aarch64-unknown-linux-gnu"], windows: ["x86_64-pc-windows-msvc"], macos: ["aarch64-apple-darwin"] };

/** What an upgrade started on the page says as it goes, by seconds in; done at the last. */
function upgradeLines(name: string, build: boolean): [number, string][] {
  const s = NEWEST.slice(0, 12);
  return build
    ? [
        [3, `fetching the source of ${s} from https://kiln.lan`],
        [6, `   Compiling kiln v0.2.0 (~/.cache/kiln/self-build/src/crates/kiln)`],
        [11, "new runner started beside the old one; it takes new work"],
        [16, `the old runner ended · ${name} runs ${NEWEST.slice(0, 7)}`],
      ]
    : [
        [3, "downloading build 0.1.0+a1b2c3d (36.4 MiB) from the server"],
        [5, "checksum matches"],
        [8, "new runner started beside the old one; it takes new work"],
        [14, `the old runner ended · ${name} runs ${NEWEST.slice(0, 7)}`],
      ];
}

let ticking: ReturnType<typeof setInterval> | null = null;
/** Upgrades started on the page go on by themselves: a line every few seconds, then done. */
function tickUpgrades(ctx: PagesCtx) {
  if (ticking) return;
  ticking = setInterval(() => {
    let running = 0;
    for (const job of started.values()) {
      if (job.state !== "running") continue;
      running++;
      const lines = upgradeLines(job.target, job.method === "build");
      const t = now() - job.started_at;
      const said = lines.filter(([at]) => at <= t).map(([, l]) => l);
      if (said.length + 1 === job.log.length && t < lines[lines.length - 1]![0] + 2) continue;
      job.log = [job.log[0]!, ...said];
      if (t >= lines[lines.length - 1]![0] + 2) {
        Object.assign(job, { state: "done", ended_at: now(), reached: true });
        upgraded.add(job.target);
        // A machine that built kiln of itself hands the binary back: others of its platform download it.
        const a = job.method === "build" ? ctx.agents().find((x) => x.name === job.target) : undefined;
        if (a) {
          const target = `${a.arch}-${a.os === "macos" ? "apple-darwin" : "unknown-linux-gnu"}`;
          const sha256 = hex(mulberry32(hashStr(`${target}:${job.id}`)), 64);
          machineBuilt.set(target, { target, sha256, size: 36_400_000, build: sha256.slice(0, 12), origin: "machine", at: now(), sha: NEWEST, by: a.name });
        }
        ctx.emit?.({ type: "updates" });
      }
      ctx.emit?.({ type: "update_job", job: { ...job, log: [...job.log] } });
    }
    if (running === 0 && ticking) {
      clearInterval(ticking);
      ticking = null;
    }
  }, 1000);
}

function startUpgrade(ctx: PagesCtx, name: string, way?: "download" | "build"): UpdateJob {
  const build = way === "build" || (way === undefined && ctx.agents().find((a) => a.name === name)?.os === "macos");
  const job: UpdateJob = {
    id: hex(mulberry32(hashStr(name + Date.now())), 16),
    kind: "runner",
    target: name,
    method: build ? "build" : "native",
    state: "running",
    started_at: now(),
    ended_at: null,
    from_build: "0.0.9+0f1e2d3",
    to_build: build ? null : "0.1.0+a1b2c3d",
    commit: NEWEST,
    reached: false,
    log: [build ? `upgrading ${name}: it builds ${NEWEST.slice(0, 12)} itself; nothing it runs is stopped` : `upgrading ${name} to build 0.1.0+a1b2c3d; nothing it runs is stopped`],
    error: null,
  };
  started.set(name, job);
  tickUpgrades(ctx);
  return job;
}

/** The Mac's build that failed, and a Linux runner's upgrade kiln stopped waiting for: what System → Updates offers to remedy. */
function failedJobs(ctx: PagesCtx): UpdateJob[] {
  const agents = ctx.agents().filter((a) => !a.ephemeral && a.connected);
  const mac = agents.find((a) => a.os === "macos");
  const old = agents.find((a) => a.os !== "macos" && a.version !== ctx.version);
  const out: UpdateJob[] = [];
  if (mac)
    out.push({
      id: "9d2f61c08b7e4a55",
      kind: "runner",
      target: mac.name,
      method: "build",
      state: "failed",
      started_at: T0 - 1500,
      ended_at: T0 - 1180,
      from_build: "0.0.9+0f1e2d3",
      to_build: null,
      commit: NEWEST,
      reached: false,
      log: [
        `upgrading ${mac.name}: it builds ${NEWEST.slice(0, 12)} itself; nothing it runs is stopped`,
        "fetching the source of 94f0847a1b2c from https://kiln.lan",
        "   Compiling kiln-cd v0.2.0 (/Users/ada/.cache/kiln/self-build/src-9d2f61c08b7e4a55/crates/kiln-cd)",
        "   Compiling kiln v0.2.0 (/Users/ada/.cache/kiln/self-build/src-9d2f61c08b7e4a55/crates/kiln)",
        "cargo exited with 101. What it said that went wrong:",
        "error: linking with `cc` failed: exit status: 1",
        'error: could not compile `kiln` (bin "kiln") due to 1 previous error',
        "failed: kiln did not build (its full output is /Users/ada/.cache/kiln/self-build/build-9d2f61c08b7e4a55.log on this machine)",
      ],
      error: "kiln did not build (its full output is /Users/ada/.cache/kiln/self-build/build-9d2f61c08b7e4a55.log on this machine)",
    });
  if (old)
    out.push({
      id: "4b1e0c9a7d6f3e21",
      kind: "runner",
      target: old.name,
      method: "native",
      state: "failed",
      started_at: T0 - 4200,
      ended_at: T0 - 2300,
      from_build: "0.0.9+0f1e2d3",
      to_build: "0.1.0+a1b2c3d",
      commit: NEWEST,
      reached: false,
      log: [
        `upgrading ${old.name} to build 0.1.0+a1b2c3d (x86_64-unknown-linux-gnu); nothing it runs is stopped`,
        "asked the runner to upgrade itself: the new build starts beside it and takes over",
        `${old.name} waits for its running job before it switches`,
        "failed: the new runner didn't report in within 15 minutes of its last job ending; see the runner's log",
      ],
      error: "the new runner didn't report in within 15 minutes of its last job ending; see the runner's log",
      timed_out: "upgrade",
    });
  return out;
}

/**
 * The binaries kept here: the server's own, then one for each platform, the newest of what kiln's
 * release pipeline built, a machine built of itself or was uploaded (crate::updates::stored). An
 * older release build of the server's own platform stays kept beside the server's own, which
 * runners of it get instead. The page's release runs, uploads and removals change them.
 */
function binariesOf(ctx: PagesCtx): KilnBinary[] {
  const [recent, older] = ctx.release?.past() ?? [];
  const own: KilnBinary = { target: "x86_64-unknown-linux-gnu", sha256: "ab".repeat(32), size: 38_200_000, build: "0.1.0+a1b2c3d", origin: "server", at: T0 - 86_400 };
  const kept: KilnBinary[] = [
    {
      target: "aarch64-unknown-linux-gnu",
      sha256: "cd".repeat(32),
      size: 36_900_000,
      build: "0.1.0+a1b2c3d",
      origin: "built",
      at: recent?.finished_at ?? T0 - 80_000,
      run: recent?.number ?? 412,
      sha: recent?.sha ?? "a1b2c3d4e5",
    },
    {
      target: "x86_64-unknown-linux-gnu",
      sha256: "ef".repeat(32),
      size: 37_500_000,
      build: "0.0.9+df17c34",
      origin: "built",
      at: older?.finished_at ?? T0 - 205_000,
      run: older?.number ?? 380,
      sha: older?.sha ?? "df17c34639ecbf9d5c9caefb6af28aa67ee01257",
    },
  ];
  // A release run of the server's own commit keeps its builds of the other platforms (crate::updates::adopts); one of any other commit, none.
  const rel = ctx.release?.latest();
  for (const j of rel && rel.run.sha === NEWEST ? rel.jobs : []) {
    if (j.status !== "succeeded") continue;
    for (const t of RELEASE_TARGETS[j.key] ?? []) {
      kept.push({
        target: t,
        sha256: hex(mulberry32(hashStr(t)), 64),
        size: t.includes("windows") ? 43_000_000 : t.includes("darwin") ? 36_100_000 : 39_600_000,
        build: `0.1.0+${hex(mulberry32(hashStr(t + "b")), 7)}`,
        origin: "built",
        at: j.finished_at ?? now(),
        run: rel!.run.number,
        sha: rel!.run.sha,
      });
    }
  }
  kept.push(...machineBuilt.values(), ...uploaded.values());
  const newest = new Map<string, KilnBinary>();
  for (const b of kept) if (b.at > (removedAt.get(b.target) ?? 0) && b.at >= (newest.get(b.target)?.at ?? 0)) newest.set(b.target, b);
  return [own, ...newest.values()];
}

function updates(ctx: PagesCtx): UpdatesOverview {
  const binaries = binariesOf(ctx);
  const rel = ctx.release?.latest() ?? null;
  const darwin = binaries.find((b) => b.target === "aarch64-apple-darwin" && b.sha === NEWEST);
  // While a release run of the server's commit goes, what it builds and isn't built from it yet; else the Mac's platform, until a build of it is kept.
  const releaseTargets = Object.values(RELEASE_TARGETS).flat();
  const behind = (rel && rel.run.sha === NEWEST ? releaseTargets : ["aarch64-apple-darwin"]).filter((t) => !binaries.some((b) => b.target === t && b.sha === NEWEST));
  const recent = ctx.release?.past()[0];
  const runners = ctx
    .agents()
    .filter((a) => !a.ephemeral)
    .map((a) => {
      const target = `${a.arch}-${a.os === "macos" ? "apple-darwin" : "unknown-linux-gnu"}`;
      // The Mac is behind and builds the newest kiln itself (crate::fleet), until the release run built its platform.
      const mac = a.os === "macos" && !darwin;
      const up = upgraded.has(a.name) || (a.version === ctx.version && a.os !== "macos");
      return {
        name: a.name,
        policy: policies.get(a.name) ?? ("auto" as const),
        locked: false,
        from: up || mac ? null : `built by release run #${recent?.number ?? 412}`,
        hostname: a.hostname,
        os: a.os,
        arch: a.arch,
        connected: a.connected,
        version: a.version,
        target,
        target_build: mac ? null : a.os === "macos" ? (darwin?.build ?? null) : "0.1.0+a1b2c3d",
        build: up ? "0.1.0+a1b2c3d" : "0.0.9+0f1e2d3",
        up_to_date: up,
        method: up ? ("native" as const) : ("terminal" as const),
        terminal: true,
        instances: [],
        commit: up ? NEWEST : mac ? "24e80a198f19253bd115906660f719b06b4d0016" : null,
        standing: up ? ("current" as const) : mac ? ("behind" as const) : ("unknown" as const),
        next: up ? null : mac ? `builds ${NEWEST.slice(0, 12)} itself` : `downloads build 0.1.0+a1b2c3d (${NEWEST.slice(0, 12)})`,
        how: up ? null : mac ? ("build" as const) : ("download" as const),
        checkout: mac ? "/Users/ada/Projects/kiln" : null,
        ways:
          up || !a.connected
            ? []
            : mac
              ? [
                  { way: "download" as const, ok: false, why: `no binary of ${NEWEST.slice(0, 12)} for aarch64-apple-darwin yet` },
                  { way: "build" as const, ok: true, why: `builds ${NEWEST.slice(0, 12)} on the machine, from this server's source` },
                ]
              : [
                  { way: "download" as const, ok: true, why: "downloads build 0.1.0+a1b2c3d" },
                  { way: "build" as const, ok: false, why: "it has no cargo or node to build kiln with" },
                ],
      };
    });
  return {
    server: {
      version: ctx.version,
      build: "0.1.0+a1b2c3d",
      target: "x86_64-unknown-linux-gnu",
      mode: "service",
      deployment: null,
      namespace: null,
      image: null,
      checking: false,
      newer: false,
      action: null,
      replacement: null,
      release: { latest: "0.1.0", newer: false, url: null, error: null, checked_at: T0 - 600 },
      note: "This is the newest build: nothing to upgrade.",
    },
    runners,
    jobs: [...started.values(), ...failedJobs(ctx).filter((j) => !started.has(j.target))],
    binaries,
    targets: ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu", "aarch64-apple-darwin", "x86_64-pc-windows-msvc"],
    build: { repo: "hyper-light/kiln", pipeline: "release", last: rel?.run ?? null, commit: NEWEST, in_step: behind.length === 0, behind, keeps_in_step: true },
    newest: { commit: NEWEST, from: "server", since: T0 - 3600 },
    follow: true,
    waits: { now: waitsChosen ?? WAITS_CONFIG, config: WAITS_CONFIG, changed: waitsChosen !== null },
  };
}

function frontDoor(a: Agent): FrontDoorView {
  const on = a.name === "metal-ryzen-01";
  const listener = (state: "on" | "off", on: string[], to: string[] = []) => ({ state, on, error: null, to, served: on.length ? 1284 : 0, fix: null });
  return {
    runner: a.name,
    connected: a.connected,
    supported: a.connected ? true : null,
    settings: {
      domain: on ? "kiln.dev" : "",
      aliases: on ? ["ci.home"] : [],
      dns: { enabled: on, port: 53, mode: "resolver", upstream: [], addresses: [] },
      https: { enabled: on, port: 443, http_port: 80, upstream: "" },
      cert: { mode: "kiln" },
      mdns: { enabled: false, name: "kiln" },
    },
    applied: on ? true : null,
    status: a.connected
      ? {
          settings: "f00",
          standby: false,
          os: a.os,
          server: "http://192.168.1.50:7070",
          addresses: ["192.168.1.77", "fe80::1"],
          dns: on ? listener("on", ["192.168.1.77:53"], ["1.1.1.1"]) : listener("off", []),
          https: on ? listener("on", ["[::]:443"], ["http://192.168.1.50:7070"]) : listener("off", []),
          http: on ? listener("on", ["[::]:80"]) : listener("off", []),
          mdns: listener("off", []),
          cert: on
            ? {
                mode: "kiln",
                state: "on",
                info: {
                  subject: "kiln.dev",
                  issuer: "kiln CA",
                  names: ["kiln.dev", "*.kiln.dev", "ci.home"],
                  not_before: T0 - 86_400 * 20,
                  not_after: T0 + 86_400 * 70,
                  sha256: "3f".repeat(32),
                  server_auth: true,
                },
                kiln: true,
                missing: [],
                error: null,
                renewing: false,
              }
            : { mode: "kiln", state: "off", info: null, kiln: true, missing: [], error: null, renewing: false },
          want: [],
          asked: T0 - 30,
          fixes: [],
        }
      : null,
    status_at: a.connected ? T0 - 30 : null,
    origins: on ? ["https://kiln.dev"] : [],
    ca: { subject: "kiln CA", sha256: "9a".repeat(32), not_after: T0 + 86_400 * 3650 },
  };
}

const TAILSCALE: TailscaleView = {
  configured: false,
  kind: null,
  id: null,
  mode: null,
  exit_nodes: null,
  want: {},
  pending: [],
  owned: {},
  synced_at: null,
  ok: true,
  error: null,
  routes: {},
  replaced: {},
};

export function syncView(ctx: Pick<PagesCtx, "agents">): SyncView {
  const names = ctx
    .agents()
    .filter((a) => !a.ephemeral)
    .map((a) => a.name);
  const copy = (member: number, machine: string, path: string, state: SyncView["repos"][number]["copies"][number]["state"]) => ({
    member,
    machine,
    path,
    added: T0 - 86_400 * 3,
    state,
    status: { applied: 1820, behind: state === "catching up" ? 12 : 0, unsent: 0, waiting: 0, conflicts: 0, skipped: [], error: null, at: T0 - 5 },
  });
  return {
    error: null,
    repos: [
      {
        id: "sy_kiln",
        error: null,
        name: "kiln",
        created: T0 - 86_400 * 9,
        exclude: ["dist"],
        files: 1834,
        index: 41,
        conflicts: [],
        copies: [copy(1, names[0] ?? "server", "/home/ada/src/kiln", "in step"), copy(2, names[1] ?? "server", "/home/ada/src/kiln", "catching up")],
      },
      {
        id: "sy_notes",
        error: null,
        name: "notes",
        created: T0 - 86_400 * 30,
        exclude: [],
        files: 212,
        index: 9,
        conflicts: [{ path: "todo.sync-conflict-ryzen.md", beside: "todo.md" }],
        copies: [copy(1, names[0] ?? "server", "/home/ada/notes", "in step"), copy(2, names[3] ?? "server", "/Users/ada/notes", "offline")],
      },
    ],
  };
}

/** Handles the routes above; false when it doesn't know the route. */
export async function handlePages(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: PagesCtx): Promise<boolean> {
  const p = url.pathname;
  const m = req.method ?? "GET";
  const { json, err } = ctx;
  let mm: RegExpMatchArray | null;

  if (p === "/api/auth/methods") return (void json(res, 200, { password: false, token: true }), true);
  if (p === "/api/sync" && m === "GET") return (void json(res, 200, syncView(ctx)), true);
  if (p === "/api/terminal/sessions" && m === "GET") return (void json(res, 200, []), true);
  if (p === "/api/ai-sessions" && m === "GET") return (void json(res, 200, []), true);
  if (p === "/api/updates" && m === "GET") return (void json(res, 200, updates(ctx)), true);
  if (p === "/api/updates/check" && m === "POST") return (void json(res, 200, updates(ctx)), true);
  if (p === "/api/updates/waits" && (m === "PUT" || m === "DELETE")) {
    waitsChosen = m === "PUT" ? await ctx.body<UpgradeWaits>(req) : null;
    res.statusCode = 204;
    res.end();
    return true;
  }
  if ((mm = p.match(/^\/api\/updates\/runners\/([^/]+)$/)) && m === "POST") {
    const ask = await ctx.body<{ way?: "download" | "build" }>(req);
    return (void json(res, 202, startUpgrade(ctx, decodeURIComponent(mm[1] ?? ""), ask.way)), true);
  }
  if ((mm = p.match(/^\/api\/updates\/runners\/([^/]+)$/)) && m === "PUT") {
    const { policy } = await ctx.body<{ policy: UpdatePolicy }>(req);
    policies.set(decodeURIComponent(mm[1] ?? ""), policy);
    ctx.emit?.({ type: "updates" });
    res.statusCode = 204;
    res.end();
    return true;
  }
  if (p === "/api/updates/policy" && m === "PUT") {
    const ask = await ctx.body<{ policy: UpdatePolicy; runners?: string[]; all?: boolean }>(req);
    const names = ask.all ? updates(ctx).runners.map((r) => r.name) : (ask.runners ?? []);
    for (const n of names) policies.set(n, ask.policy);
    ctx.emit?.({ type: "updates" });
    res.statusCode = 204;
    res.end();
    return true;
  }
  if (p === "/api/updates/runners" && m === "POST") {
    const behind = updates(ctx).runners.filter((r) => r.connected && !r.up_to_date && !r.locked && started.get(r.name)?.state !== "running");
    return (void json(res, 202, { jobs: behind.map((r) => startUpgrade(ctx, r.name)), errors: [] }), true);
  }
  if (p === "/api/updates/build" && m === "POST") {
    if (!ctx.release) return (void err(res, 404, "no release pipeline"), true);
    const run = ctx.release.start(NEWEST);
    ctx.emit?.({ type: "updates" });
    return (void json(res, 202, run), true);
  }
  if (p === "/api/updates/binaries" && m === "POST") {
    const target = url.searchParams.get("target") ?? "";
    const chunks: Buffer[] = [];
    for await (const c of req) chunks.push(c as Buffer);
    const size = chunks.reduce((n, c) => n + c.length, 0);
    const bin: KilnBinary = { target, sha256: hex(mulberry32(hashStr(target + size + Date.now())), 64), size, build: "0.1.0+a1b2c3d", origin: "upload", at: now() };
    uploaded.set(target, bin);
    ctx.emit?.({ type: "updates" });
    return (void json(res, 201, bin), true);
  }
  if ((mm = p.match(/^\/api\/updates\/binaries\/([^/]+)$/)) && m === "DELETE") {
    const target = decodeURIComponent(mm[1] ?? "");
    uploaded.delete(target);
    machineBuilt.delete(target);
    removedAt.set(target, now());
    ctx.emit?.({ type: "updates" });
    res.statusCode = 204;
    res.end();
    return true;
  }
  if (p === "/api/front-doors" && m === "GET")
    return (
      void json(
        res,
        200,
        ctx
          .agents()
          .filter((a) => !a.ephemeral)
          .map(frontDoor),
      ),
      true
    );
  if ((mm = p.match(/^\/api\/runners\/([^/]+)\/front-door$/)) && m === "GET") {
    const a = ctx.agents().find((x) => x.name === decodeURIComponent(mm![1]!));
    return (a ? json(res, 200, frontDoor(a)) : err(res, 404, "no such runner"), true);
  }
  if (p === "/api/tailscale" && m === "GET") return (void json(res, 200, TAILSCALE), true);
  if (p === "/api/harness/kinds" && m === "GET") {
    const kinds = [
      { id: "claude", name: "Claude Code", verified: true, available: true },
      { id: "codex", name: "Codex", verified: true, available: true },
    ];
    return (
      void json(res, 200, {
        local: kinds,
        ...Object.fromEntries(
          ctx
            .agents()
            .filter((a) => a.connected && !a.ephemeral)
            .map((a) => [`agent:${a.name}`, kinds]),
        ),
      }),
      true
    );
  }

  // Deployments
  if (p === "/api/cd/summary") return (void json(res, 200, summary()), true);
  if (p === "/api/cd/facets") return (void json(res, 200, facets(url.searchParams)), true);
  if (p === "/api/cd/apps" && m === "GET") {
    const { all, match } = listApps(url.searchParams);
    const sort = url.searchParams.get("sort") ?? "name";
    const list = all
      .filter((a) => match(a))
      .sort((x, y) =>
        sort === "health"
          ? (HRANK[x.health] ?? 9) - (HRANK[y.health] ?? 9) || x.app.localeCompare(y.app)
          : sort === "sync"
            ? (SRANK[x.sync] ?? 9) - (SRANK[y.sync] ?? 9) || x.app.localeCompare(y.app)
            : sort === "deployed"
              ? (y.operation?.finished_ms ?? 0) - (x.operation?.finished_ms ?? 0)
              : x.app.localeCompare(y.app),
      );
    return (void json(res, 200, { apps: list, next: null }), true);
  }
  // An app kept in kiln, put again (its sync policy changed): what it says now.
  if (p === "/api/cd/apps" && m === "POST") {
    const def = await ctx.body<{ name?: string; project?: string; sync?: { auto: boolean; prune: boolean; self_heal: boolean; allow_empty: boolean } }>(req);
    const a = apps.get(`${def.project}/${def.name}`);
    if (!a || !def.sync) return (void json(res, 400, { error: "no such app, or no sync policy" }), true);
    if (a.detail.definition) a.detail.definition.sync = def.sync;
    a.detail.auto = def.sync.auto;
    a.app.auto = def.sync.auto;
    return (void json(res, 200, { app: a.detail.app }), true);
  }
  if (p === "/api/cd/repos/auto-rollback" && m === "PUT") {
    const { repo, on } = await ctx.body<{ repo: string; on: boolean | null }>(req);
    if (on === null) repoAutoRollback.delete(repo);
    else repoAutoRollback.set(repo, { on, by: "ada", at_ms: Date.now() });
    return (void json(res, 200, { repo, setting: on === null ? null : { on, by: "ada" } }), true);
  }
  if (p === "/api/cd/plans/repos") return (void json(res, 200, { repos: PLAN_REPOS }), true);
  if (p === "/api/cd/plans" && m === "POST") {
    const ask = await ctx.body<{ head?: string; commit?: string; pr?: number }>(req);
    await new Promise((r) => setTimeout(r, 400));
    return (void json(res, 200, plan(ask)), true);
  }
  if ((mm = p.match(/^\/api\/cd\/apps\/([^/]+)\/([^/]+)(\/.*)?$/))) {
    const id = `${decodeURIComponent(mm[1]!)}/${decodeURIComponent(mm[2]!)}`;
    const rest = mm[3] ?? "";
    const a = apps.get(id);
    if (!a) return (err(res, 404, `no app ${id}`), true);
    if (rest === "/auto-rollback" && m === "PUT") {
      const { on } = await ctx.body<{ on: boolean | null }>(req);
      a.autoRollback = on === null ? null : { on, by: "ada", at_ms: Date.now() };
      return (void json(res, 200, autoRollbackOf(a)), true);
    }
    if (rest === "/rollback" && m === "POST") {
      const { to } = await ctx.body<{ to?: number }>(req);
      const d = rollbackTargets(a).targets.find((t) => t.id === to);
      if (!d) return (err(res, 400, `${id} has no deployment ${to} to roll back to`), true);
      const pin = { to: d.id, revision: d.revision, by: "ada", at_ms: Date.now() };
      a.detail.pinned = pin;
      a.app.pinned = pin;
      return (void json(res, 200, { rolling_back: id, to: d.id, revision: d.revision, commits: d.commits, tag: null }), true);
    }
    if (m !== "GET") return (void json(res, 200, {}), true);
    if (rest === "") return (void json(res, 200, { ...a.detail, auto_rollback: autoRollbackOf(a) }), true);
    if (rest === "/tree") return (void json(res, 200, tree(id)), true);
    if (rest === "/history") return (void json(res, 200, { history: a.history }), true);
    if (rest === "/rollback-targets") return (void json(res, 200, rollbackTargets(a)), true);
    if (rest === "/auto-rollback") return (void json(res, 200, autoRollbackOf(a)), true);
    if (rest === "/rollout") return (void json(res, 200, { app: id, rollouts: a.rollouts ?? [] }), true);
    if (rest === "/diff") return (void json(res, 200, { diffs: diffOf(id) }), true);
    if (rest === "/resources") return (void json(res, 200, { resources: [] }), true);
    let rm: RegExpMatchArray | null;
    if ((rm = rest.match(/^\/resources\/([^/]+)(\/events|\/logs)?$/))) {
      const key = decodeURIComponent(rm[1]!);
      if (rm[2] === "/events") return (void json(res, 200, { events: events() }), true);
      if (rm[2] === "/logs") return (void json(res, 200, logs(key.split("~")[3] ?? "pod")), true);
      const d = resourceDetail(id, key);
      return (d ? json(res, 200, d) : err(res, 404, "no such object"), true);
    }
    return (err(res, 404, `no route for ${p}`), true);
  }

  // Research
  if (p === "/api/research" && m === "GET") {
    // One agent's, one plan's, or all, as the server answers.
    const agent = url.searchParams.get("agent");
    const plan = url.searchParams.get("plan");
    const all = Object.values(RESEARCH) as { agent?: string | null; plan?: string | null }[];
    return (void json(res, 200, { research: all.filter((r) => (!agent || r.agent === agent) && (!plan || r.plan === plan)) }), true);
  }
  if ((mm = p.match(/^\/api\/research\/([^/]+)$/)) && m === "GET") {
    const r = RESEARCH[decodeURIComponent(mm[1]!)];
    return (r ? json(res, 200, r) : err(res, 404, "no such research"), true);
  }
  // Plans
  if (p === "/api/plans" && m === "GET") {
    // An agent's (`session`), and the plan each agent proposed (`proposed`), when asked.
    const session = url.searchParams.get("session");
    const proposed = url.searchParams.get("proposed") === "1";
    const of = (pl: (typeof PLANS)[number]) => (pl as { source?: { session?: string; proposed?: boolean } | null }).source;
    return (
      void json(
        res,
        200,
        PLANS.filter((pl) => (!session || of(pl)?.session === session) && (!proposed || of(pl)?.proposed)),
      ),
      true
    );
  }
  if ((mm = p.match(/^\/api\/plans\/([^/]+)(\/versions|\/runs)?$/)) && m === "GET") {
    const pl = PLANS.find((x) => x.id === decodeURIComponent(mm![1]!));
    if (!pl) return (err(res, 404, "no such plan"), true);
    if (mm[2] === "/versions")
      return (
        void json(
          res,
          200,
          Array.from({ length: pl.version }, (_, i) => ({ version: pl.version - i, title: pl.title, body: pl.body, at: pl.updated_at - i * 86_400, by: "ada" })),
        ),
        true
      );
    if (mm[2] === "/runs")
      return (
        void json(
          res,
          200,
          Array.from({ length: pl.runs }, (_, i) => ({ id: `pr_${i}`, version: pl.version, session: `h_${i}abcdef`, mode: i ? "steps" : "message", at: pl.updated_at - i * 3_600, by: "ada" })),
        ),
        true
      );
    return (void json(res, 200, pl), true);
  }
  // A plan run (`plans.run`): some of its steps (`only`), as goals split into their criteria and tests (`split`), or an agent each.
  if ((mm = p.match(/^\/api\/plans\/([^/]+)\/run$/)) && m === "POST") {
    const pl = PLANS.find((x) => x.id === decodeURIComponent(mm![1]!));
    if (!pl) return (err(res, 404, "no such plan"), true);
    const chunks: Buffer[] = [];
    for await (const c of req) chunks.push(c as Buffer);
    const b = JSON.parse(Buffer.concat(chunks).toString("utf8") || "{}") as { mode?: string; only?: number[]; split?: boolean; session?: string };
    const n = b.only?.length ?? (pl.items?.length || pl.steps?.length || 0);
    if (b.mode === "agents")
      return (void json(res, 200, { run: "pr_agents1", steps: n, note: b.only?.length ? `Not in this run, taken as done: the steps step ${b.only[0]} waits for.` : null }), true);
    return (void json(res, 200, { session: b.session ?? "fam-orch", sent: n, run: "pr_goals1", split: b.mode === "goals" && b.split ? n : 0 }), true);
  }
  if ((mm = p.match(/^\/api\/shared\/plans\/([^/]+)$/))) {
    const pl = PLANS.find((x) => x.share === decodeURIComponent(mm![1]!));
    return (pl ? json(res, 200, { title: pl.title, body: pl.body, items: pl.items, steps: pl.steps, version: pl.version, updated_at: pl.updated_at }) : err(res, 404, "not shared"), true);
  }
  return false;
}

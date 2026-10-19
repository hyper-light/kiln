/**
 * Mock data for the pages the simulation in server.ts doesn't cover: deployments (apps, their
 * trees, history, diffs, objects, events, logs, deploy plans), kept plans, Settings → Updates, the
 * runners' front doors and the tailnet, synced folders, terminal and AI sessions, and the sign-in
 * methods. Deterministic (seeded), so screenshots and layout measurements repeat.
 */
import type http from "node:http";
import type {
  Agent,
  CdApp,
  CdAppDetail,
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
  Run,
  SyncView,
  TailscaleView,
  UpdateJob,
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
      const history: CdDeployment[] = Array.from({ length: 6 }, (_, i) => ({
        app: id,
        id: 6 - i,
        op: 6 - i,
        revision: i === 0 ? revision : hex(r, 40),
        trigger: pick(r, ["manual", "auto", "self-heal"]),
        by: r() < 0.4 ? "ada" : null,
        succeeded: !(i === 2 && r() < 0.4),
        message: i === 2 ? "one or more objects failed to apply" : "",
        applied: resources,
        pruned: i === 1 ? 1 : 0,
        at_ms: Date.now() - (i + 1) * 9 * 3_600_000,
        commit: { sha: revision, author: "ada", email: "dev@example.com", at_ms: Date.now() - (i + 1) * 9 * 3_600_000, subject: `Change ${6 - i} to ${name}`, body: "" },
      }));
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
      definition: { sources: [{ repo: "https://github.com/hyper-light/thumbs", path: ".kiln/deploy.yaml", revision: "main", chart: null, as_ref: null }], sync: { auto: true, prune: true, self_heal: true, allow_empty: false } },
      commits: [revision],
      children: [],
      revision_info: { sha: revision, author: "ada", email: "dev@example.com", at_ms: at - 360_000, subject: "Resize thumbnails in WebP", body: "" },
    };
    apps.set(id, { app, detail, history: [], rollouts });
  };
  const run = (name: string, against: "preview" | "production", passed: number, failed: number, verdict: CdRollout["checks"][number]["verdict"], why = "", asked = false) => ({
    name,
    against,
    measurements: [...Array(passed).fill("pass"), ...Array(failed).fill("fail")].map((v, i) => ({ at_ms: at - (passed + failed - i) * 20_000, verdict: v, value: { status: v === "pass" ? 200 : 503 }, message: v === "pass" ? "" : "503 from /healthz" })),
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
  managed("Service", a.name, "", off ? { sync: "OutOfSync", tracked_by: `default/${a.name}-argo`, shared_with: [`default/${a.name}-argo`], marks_only: true, why: `app default/${a.name}-argo's tracking is on it: a sync takes it over` } : {});
  managed("ConfigMap", `${a.name}-config`, "", { health: null, info: [{ name: "Keys", value: "4" }], ...(off ? { sync: "OutOfSync" as const, marks_only: true, why: ".metadata.labels.kiln.dev/app is missing live" } : {}) });
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
  const marks = (app: string) => [{ old_start: 4, old_lines: 3, new_start: 4, new_lines: 3, lines: ["   labels:", `-    app.kubernetes.io/instance: ${app}`, `+    kiln.dev/app: 0123456789abcdef`, `   name: ${a.name}`] }];
  return [
    {
      key: { group: "apps", kind: "Deployment", namespace: ns, name: a.name },
      why: ".spec.replicas differs",
      live: "",
      predicted: "",
      hunks: [{ old_start: 7, old_lines: 3, new_start: 7, new_lines: 3, lines: [" spec:", "-  replicas: 2", "+  replicas: 3", "   revisionHistoryLimit: 5"] }],
    },
    { key: { group: "", kind: "Service", namespace: ns, name: a.name }, why: `app default/${a.name}-argo's tracking is on it: a sync takes it over`, live: "", predicted: "", hunks: marks(`${a.name}-argo`), tracked_by: `default/${a.name}-argo`, marks_only: true },
    { key: { group: "", kind: "ConfigMap", namespace: ns, name: `${a.name}-config` }, why: ".metadata.labels.kiln.dev/app is missing live", live: "", predicted: "", hunks: marks(""), marks_only: true },
    { key: { group: "policy", kind: "PodDisruptionBudget", namespace: ns, name: a.name }, why: "not in the cluster", live: "", predicted: "", hunks: [{ old_start: 0, old_lines: 0, new_start: 1, new_lines: 2, lines: ["+kind: PodDisruptionBudget", `+name: ${a.name}`] }], missing: true },
    { key: { group: "", kind: "ConfigMap", namespace: ns, name: `${a.name}-old` }, why: "not in git: requires pruning", live: "", predicted: "", hunks: [{ old_start: 1, old_lines: 2, new_start: 0, new_lines: 0, lines: ["-kind: ConfigMap", `-name: ${a.name}-old`] }], requires_pruning: true },
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

const PLANS = [
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
      },
      { title: "Tag and publish", description: "", examples: "", acceptance: [], testing: [] },
    ],
    tags: ["release"],
    source: null,
    share: "shr_7f3a91",
    version: 4,
    created_at: T0 - 86_400 * 6,
    updated_at: T0 - 3_600 * 5,
    steps: ["Freeze main", "Run the full bench", "Tag and publish"],
    runs: 2,
  },
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
  },
];

/* ------------------------------------------------------------------ the rest */

const NEWEST = "94f0847a1b2c3d4e5f60718293a4b5c6d7e8f901";

/** How long upgrades are waited on: kiln.toml's, unless changed in Settings → Updates. */
const WAITS_CONFIG: UpgradeWaits = { upgrade_minutes: 15, wait_for_jobs_minutes: 360, build_minutes: 60 };
let waitsChosen: UpgradeWaits | null = null;
/** Upgrades started from the page, by runner. */
const started = new Map<string, UpdateJob>();

/** The Mac's build that failed, and a Linux runner's upgrade kiln stopped waiting for: what Settings → Updates offers to remedy. */
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
        "kiln checkout: /Users/ada/Projects/kiln (left as it is: the build is of 94f0847a1b2c, from the server)",
        "fetching the source of 94f0847a1b2c from https://kiln.lan",
        "   Compiling kiln-cd v0.2.0 (/Users/ada/.cache/kiln/self-build/src-9d2f61c08b7e4a55/crates/kiln-cd)",
        "   Compiling kiln v0.2.0 (/Users/ada/.cache/kiln/self-build/src-9d2f61c08b7e4a55/crates/kiln)",
        "cargo exited with 101. What it said that went wrong:",
        "error: linking with `cc` failed: exit status: 1",
        "error: could not compile `kiln` (bin \"kiln\") due to 1 previous error",
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

function updates(ctx: PagesCtx): UpdatesOverview {
  const runners = ctx
    .agents()
    .filter((a) => !a.ephemeral)
    .map((a) => {
      const target = `${a.arch}-${a.os === "macos" ? "apple-darwin" : "unknown-linux-gnu"}`;
      const mac = a.os === "macos";
      // The Mac is behind and builds the newest kiln itself (crate::fleet).
      const up = a.version === ctx.version && !mac;
      return {
        name: a.name,
        policy: "auto" as const,
        locked: false,
        from: up || mac ? null : "built by release run #412",
        hostname: a.hostname,
        os: a.os,
        arch: a.arch,
        connected: a.connected,
        version: a.version,
        target,
        target_build: a.os === "macos" ? null : "0.1.0+a1b2c3d",
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
                  { way: "build" as const, ok: false, why: "no kiln checkout on it to build from" },
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
    binaries: [
      { target: "x86_64-unknown-linux-gnu", sha256: "ab".repeat(32), size: 38_200_000, build: "0.1.0+a1b2c3d", origin: "server", at: T0 - 86_400 },
      { target: "aarch64-unknown-linux-gnu", sha256: "cd".repeat(32), size: 36_900_000, build: "0.1.0+a1b2c3d", origin: "built", at: T0 - 80_000, run: 412, sha: "a1b2c3d4e5" },
    ],
    targets: ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu", "aarch64-apple-darwin", "x86_64-pc-windows-msvc"],
    build: { repo: "hyper-light/kiln", pipeline: "release", last: null, commit: NEWEST, in_step: false, behind: ["aarch64-apple-darwin"], keeps_in_step: true },
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
    const name = decodeURIComponent(mm[1] ?? "");
    const ask = await ctx.body<{ way?: "download" | "build" }>(req);
    const build = ask.way === "build";
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
    return (void json(res, 202, job), true);
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
    if (m !== "GET") return (void json(res, 200, {}), true);
    if (rest === "") return (void json(res, 200, a.detail), true);
    if (rest === "/tree") return (void json(res, 200, tree(id)), true);
    if (rest === "/history") return (void json(res, 200, { history: a.history }), true);
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

  // Plans
  if (p === "/api/plans" && m === "GET") return (void json(res, 200, PLANS), true);
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
  if ((mm = p.match(/^\/api\/shared\/plans\/([^/]+)$/))) {
    const pl = PLANS.find((x) => x.share === decodeURIComponent(mm![1]!));
    return (pl ? json(res, 200, { title: pl.title, body: pl.body, items: pl.items, steps: pl.steps, version: pl.version, updated_at: pl.updated_at }) : err(res, 404, "not shared"), true);
  }
  return false;
}

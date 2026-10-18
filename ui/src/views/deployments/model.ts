/**
 * An app's resource tree as the page shows it: what to call each kind, the order kinds come in,
 * what a filter keeps (matching nodes, and their owners for context), the counts the filter
 * field shows, and the forest the tree and the list draw — with large sets of alike siblings
 * folded into one node ("12 pods") until someone opens it.
 */
import type { CdNode } from "@/api/types";
import type { Chosen, Facet } from "@/components/ui/facet-box";

/** kubectl's short names where it has them; the console's own words otherwise. */
const SHORT: Record<string, string> = {
  Deployment: "deploy",
  ReplicaSet: "rs",
  Pod: "pod",
  StatefulSet: "sts",
  DaemonSet: "ds",
  Job: "job",
  CronJob: "cj",
  Service: "svc",
  Ingress: "ing",
  IngressClass: "ingc",
  EndpointSlice: "eps",
  Endpoints: "ep",
  ConfigMap: "cm",
  Secret: "secret",
  ServiceAccount: "sa",
  PersistentVolumeClaim: "pvc",
  PersistentVolume: "pv",
  StorageClass: "sc",
  Namespace: "ns",
  ControllerRevision: "rev",
  HorizontalPodAutoscaler: "hpa",
  PodDisruptionBudget: "pdb",
  NetworkPolicy: "netpol",
  Role: "role",
  RoleBinding: "rb",
  ClusterRole: "crole",
  ClusterRoleBinding: "crb",
  PriorityClass: "pc",
  CustomResourceDefinition: "crd",
  MutatingWebhookConfiguration: "mwc",
  ValidatingWebhookConfiguration: "vwc",
  APIService: "apisvc",
  LimitRange: "limits",
  ResourceQuota: "quota",
  Application: "app",
};

/** A kind's badge: short, lower case. */
export function kindShort(kind: string): string {
  const known = SHORT[kind];
  if (known) return known;
  const caps = kind.replace(/[^A-Z]/g, "");
  return (kind.length <= 7 || caps.length < 2 ? kind.slice(0, 7) : caps).toLowerCase();
}

/**
 * Kinds in the order an app is read: what runs first (workloads and what they run), then how it is
 * reached, then what it is configured and stored with, then what it is allowed to do.
 */
const ORDER = [
  "Deployment",
  "StatefulSet",
  "DaemonSet",
  "CronJob",
  "Job",
  "ReplicaSet",
  "Pod",
  "ControllerRevision",
  "HorizontalPodAutoscaler",
  "PodDisruptionBudget",
  "Service",
  "EndpointSlice",
  "Ingress",
  "NetworkPolicy",
  "ConfigMap",
  "Secret",
  "PersistentVolumeClaim",
  "PersistentVolume",
  "StorageClass",
  "ServiceAccount",
  "Role",
  "RoleBinding",
  "ClusterRole",
  "ClusterRoleBinding",
  "PriorityClass",
  "Namespace",
  "CustomResourceDefinition",
];
const RANK = new Map(ORDER.map((k, i) => [k, i]));
export const kindRank = (kind: string) => RANK.get(kind) ?? ORDER.length;

const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: "base" });
export const compareText = (a: string, b: string) => collator.compare(a, b);

const revisionOf = (n: CdNode) => Number(n.info.find((i) => i.name === "Revision")?.value ?? NaN);

/** Siblings in reading order: by kind, newest revision first, then by name. */
export function compareNodes(a: CdNode, b: CdNode): number {
  const k = kindRank(a.kind) - kindRank(b.kind) || compareText(a.kind, b.kind);
  if (k) return k;
  const ra = revisionOf(a);
  const rb = revisionOf(b);
  if (!Number.isNaN(ra) && !Number.isNaN(rb) && ra !== rb) return rb - ra;
  return compareText(a.name, b.name) || compareText(a.namespace, b.namespace);
}

/** Age in one short word: `45s`, `12m`, `3h`, `5d`. */
export function age(created: string | null, now: number): string {
  if (!created) return "";
  const t = Date.parse(created);
  if (Number.isNaN(t)) return "";
  const s = Math.max(0, Math.floor((now - t) / 1000));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86_400) return `${Math.floor(s / 3600)}h`;
  return `${Math.floor(s / 86_400)}d`;
}

/** Whether a node is in trouble (it is drawn and listed so). */
export const troubled = (n: CdNode) =>
  n.health === "Degraded" || n.health === "Missing" || (n.managed && !n.hook && n.sync === "OutOfSync");

/* ------------------------------------------------------------------ filtering */

export const NO_HEALTH = "(none)";
export const NOT_MANAGED = "Live only";
export const CLUSTER_SCOPED = "(cluster)";

/** What a node is for each facet. */
const DIMENSIONS: { id: string; label: string; of: (n: CdNode) => string; order?: string[] }[] = [
  { id: "kind", label: "Kind", of: (n) => n.kind },
  {
    id: "health",
    label: "Health",
    of: (n) => n.health ?? NO_HEALTH,
    order: ["Degraded", "Missing", "Progressing", "Suspended", "Unknown", "Healthy", NO_HEALTH],
  },
  {
    id: "sync",
    label: "Sync",
    of: (n) => (n.managed ? (n.sync ?? "Unknown") : NOT_MANAGED),
    order: ["OutOfSync", "Unknown", "Synced", "Hook", NOT_MANAGED],
  },
  { id: "namespace", label: "Namespace", of: (n) => n.namespace || CLUSTER_SCOPED },
];

const WORDS: Record<string, string> = { OutOfSync: "Out of sync" };

/** Matches the typed words (part of its name, or `kind/name`) and every facet chosen. */
export function matches(n: CdNode, q: string, chosen: Chosen, except?: string): boolean {
  const words = q.trim().toLowerCase();
  if (words && !n.name.toLowerCase().includes(words) && !`${n.kind}/${n.name}`.toLowerCase().includes(words)) return false;
  return DIMENSIONS.every((d) => d.id === except || !chosen[d.id]?.length || chosen[d.id]!.includes(d.of(n)));
}

export const isFiltered = (q: string, chosen: Chosen) => Boolean(q.trim()) || Object.values(chosen).some((v) => v.length > 0);

/** Each facet's options, with how many nodes each would show (the other choices as they are). */
export function facetsOf(nodes: CdNode[], q: string, chosen: Chosen): Facet[] {
  return DIMENSIONS.flatMap((d) => {
    const counts = new Map<string, number>();
    for (const n of nodes) {
      const v = d.of(n);
      counts.set(v, (counts.get(v) ?? 0) + (matches(n, q, chosen, d.id) ? 1 : 0));
    }
    for (const v of chosen[d.id] ?? []) if (!counts.has(v)) counts.set(v, 0);
    // A facet all nodes share says nothing, unless something of it is chosen.
    if (counts.size < 2 && !(chosen[d.id]?.length ?? 0)) return [];
    const rank = (v: string) => (d.order ? (d.order.indexOf(v) + 1 || d.order.length + 1) : 0);
    const options = [...counts.entries()]
      .sort(([a], [b]) => rank(a) - rank(b) || (d.id === "kind" ? kindRank(a) - kindRank(b) : 0) || compareText(a, b))
      .map(([id, count]) => ({ id, label: WORDS[id] ?? id, count }));
    return [{ id: d.id, label: d.label, options }];
  });
}

/**
 * What a filter keeps: the nodes that match, and (for the tree) every owner of one, shown as
 * context so a pod is never seen without its ReplicaSet and Deployment.
 */
export function keep(nodes: CdNode[], q: string, chosen: Chosen, withOwners: boolean): { shown: Set<string>; matched: Set<string> } {
  const matched = new Set(nodes.filter((n) => matches(n, q, chosen)).map((n) => n.id));
  const shown = new Set(matched);
  if (withOwners && matched.size < nodes.length) {
    const byId = new Map(nodes.map((n) => [n.id, n]));
    const todo = [...matched];
    while (todo.length) {
      const n = byId.get(todo.pop()!);
      for (const p of n?.parents ?? []) {
        if (!shown.has(p) && byId.has(p)) {
          shown.add(p);
          todo.push(p);
        }
      }
    }
  }
  return { shown, matched };
}

/* ------------------------------------------------------------------ the forest */

export interface NodeItem {
  type: "node";
  id: string;
  node: CdNode;
  /** Shown only because something it owns matched the filter. */
  context: boolean;
  children: Item[];
}

/** Alike siblings folded into one node, or the header of the same siblings opened. */
export interface GroupItem {
  type: "group";
  id: string;
  kind: string;
  /** "old replica sets", "pods". */
  noun: string;
  members: CdNode[];
  open: boolean;
  /** Opened: the members, as nodes. */
  children: Item[];
}

export type Item = NodeItem | GroupItem;

const NOUN: Record<string, [string, string]> = {
  Pod: ["pod", "pods"],
  ReplicaSet: ["old replica set", "old replica sets"],
  ControllerRevision: ["revision", "revisions"],
  Job: ["job", "jobs"],
  EndpointSlice: ["endpoint slice", "endpoint slices"],
};

export function noun(kind: string, n: number): string {
  const w = NOUN[kind];
  if (w) return n === 1 ? w[0] : w[1];
  return n === 1 ? kind : `${kind}s`;
}

/** Siblings folded when there are more than this many of a kind (healthy ones; trouble stays out). */
const FOLD_AT: Record<string, number> = { ControllerRevision: 2 };
const FOLD_DEFAULT = 5;

/** Which of a parent's children of one kind fold together, if any. */
function foldable(kind: string, kids: CdNode[]): CdNode[] {
  const quiet = (n: CdNode) => n.health === null || n.health === "Healthy";
  if (kind === "ReplicaSet") {
    // Old revisions scaled to nothing; the ones that run stay in view.
    const idle = kids.filter((n) => quiet(n) && n.ready === "0/0");
    return idle.length >= 2 ? idle : [];
  }
  if (kids.length <= (FOLD_AT[kind] ?? FOLD_DEFAULT)) return [];
  const calm = kids.filter(quiet);
  return calm.length >= 2 ? calm : [];
}

/**
 * The forest of the nodes shown: each node under its first owner that is shown (the app's own
 * objects at the top), siblings in reading order, alike siblings folded unless `open` holds
 * their group. `context` marks owners shown only for what they own.
 */
export function forest(nodes: CdNode[], shown: Set<string>, matched: Set<string>, open: ReadonlySet<string>, reveal?: string | null): Item[] {
  const byId = new Map(nodes.filter((n) => shown.has(n.id)).map((n) => [n.id, n]));
  const kids = new Map<string, CdNode[]>();
  const roots: CdNode[] = [];
  for (const n of byId.values()) {
    const parent = n.parents.find((p) => byId.has(p));
    if (parent) {
      const list = kids.get(parent);
      if (list) list.push(n);
      else kids.set(parent, [n]);
    } else roots.push(n);
  }
  const seen = new Set<string>();
  const items = (list: CdNode[], parentId: string): Item[] => {
    const sorted = [...list].sort(compareNodes);
    const byKind = new Map<string, CdNode[]>();
    for (const n of sorted) {
      const l = byKind.get(n.kind);
      if (l) l.push(n);
      else byKind.set(n.kind, [n]);
    }
    const folded = new Map<string, Set<string>>();
    for (const [kind, ks] of byKind) {
      const f = foldable(kind, ks);
      if (f.length) folded.set(kind, new Set(f.map((n) => n.id)));
    }
    const out: Item[] = [];
    const placedGroups = new Set<string>();
    for (const n of sorted) {
      const fold = folded.get(n.kind);
      if (fold?.has(n.id)) {
        const gid = `${parentId}|${n.kind}`;
        if (placedGroups.has(gid)) continue;
        placedGroups.add(gid);
        const members = sorted.filter((x) => fold.has(x.id));
        const isOpen = open.has(gid) || (!!reveal && members.some((m) => m.id === reveal));
        out.push({
          type: "group",
          id: gid,
          kind: n.kind,
          noun: noun(n.kind, members.length),
          members,
          open: isOpen,
          children: isOpen ? members.map((m) => node(m)) : [],
        });
        continue;
      }
      out.push(node(n));
    }
    return out;
  };
  const node = (n: CdNode): NodeItem => {
    // A cycle the server didn't cut can't loop here: each node is placed once.
    const fresh = !seen.has(n.id);
    seen.add(n.id);
    return { type: "node", id: n.id, node: n, context: !matched.has(n.id), children: fresh ? items(kids.get(n.id) ?? [], n.id) : [] };
  };
  return items(roots, "app");
}

/** Every item of a forest, depth first, with its depth (for the indented list). */
export function flatten(list: Item[], closed: ReadonlySet<string>, depth = 0, out: { item: Item; depth: number }[] = []) {
  for (const it of list) {
    out.push({ item: it, depth });
    if (it.type === "group") {
      if (it.open) flatten(it.children, closed, depth, out);
    } else if (!closed.has(it.id)) flatten(it.children, closed, depth + 1, out);
  }
  return out;
}

/** How many nodes of each health a set has: "10 healthy · 2 degraded". */
export function healthCounts(list: CdNode[]): string {
  const counts = new Map<string, number>();
  for (const n of list) counts.set(n.health ?? "no health", (counts.get(n.health ?? "no health") ?? 0) + 1);
  return [...counts.entries()].map(([h, c]) => `${c} ${h.toLowerCase()}`).join(" · ");
}

/** What a sync does about an object that differs: the fix it needs. */
export type Fix = "changed" | "takeover" | "unmarked" | "missing" | "prune";

/** The fixes, in the order people deal with them. */
export const FIXES: Fix[] = ["changed", "missing", "takeover", "unmarked", "prune"];

/** The fix an app's object needs, if it differs. */
export function fixOf(n: { sync?: string | null; hook?: boolean; missing?: boolean; requires_pruning?: boolean; tracked_by?: string | null; marks_only?: boolean }): Fix | null {
  if (n.hook || n.sync === "Synced" || n.sync === "Hook") return null;
  if (n.requires_pruning) return "prune";
  if (n.missing) return "missing";
  if (n.tracked_by) return "takeover";
  if (n.marks_only) return "unmarked";
  return n.sync === "OutOfSync" || n.sync === undefined ? "changed" : null;
}

/** What differs in an app, by what a sync does about it ("14 changed · 92 to take over · 3 to prune"), or how many objects it has when nothing does. */
export function whatDiffers(a: { out_of_sync?: number; resources?: number; missing?: number; prune?: number; shared?: number; unmarked?: number }): string {
  const differ = a.out_of_sync ?? 0;
  if (!differ) return a.resources === undefined ? "" : `${a.resources} ${a.resources === 1 ? "object" : "objects"}`;
  const [missing, prune, shared, unmarked] = [a.missing ?? 0, a.prune ?? 0, a.shared ?? 0, a.unmarked ?? 0];
  const changed = Math.max(0, differ - missing - prune - shared - unmarked);
  const parts: [number, string][] = [
    [changed, "changed"],
    [missing, "missing"],
    [shared, "to take over"],
    [unmarked, "unmarked"],
    [prune, "to prune"],
  ];
  return parts
    .filter(([n]) => n > 0)
    .map(([n, w]) => `${n} ${w}`)
    .join(" · ");
}

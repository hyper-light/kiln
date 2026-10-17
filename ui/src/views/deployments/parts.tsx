/** Deployments' small pieces: sync and health as a dot and a word, in kiln's eight tones. */
import type { CdApp, CdNode, CdOperation, CdResourceKey } from "@/api/types";
import { cn } from "@/lib/utils";
import { kindShort } from "./model";

/** Health in the run tones: healthy is a success, progressing runs, degraded failed. */
export const HEALTH_TONE: Record<string, string> = {
  Healthy: "succeeded",
  Progressing: "running",
  Degraded: "failed",
  Missing: "timed_out",
  Suspended: "cancelled",
  Unknown: "skipped",
};

/** Sync in the run tones: out of sync waits in amber. */
export const SYNC_TONE: Record<string, string> = { Synced: "succeeded", OutOfSync: "timed_out", Unknown: "skipped", Hook: "skipped" };

const WORD: Record<string, string> = { OutOfSync: "Out of sync" };

/** Why a sync ran, in words. */
export const TRIGGER_WORD: Record<string, string> = {
  manual: "Manual",
  auto: "Auto-sync",
  "self-heal": "Self-heal",
  rollback: "Rollback",
  promotion: "Promotion",
};

/** A dot and a word. */
export function StatePair({ tone, word, className }: { tone: string; word: string; className?: string }) {
  return (
    <span className={cn("status cd-pair", className)}>
      <i className="dot" data-status={tone} aria-hidden="true" />
      <span className="status-text" data-status={tone}>
        {WORD[word] ?? word}
      </span>
    </span>
  );
}

export function SyncPair({ sync }: { sync: string }) {
  return <StatePair tone={SYNC_TONE[sync] ?? "skipped"} word={sync} />;
}

export function HealthPair({ health }: { health: string | null | undefined }) {
  if (!health) return <span className="cd-none">No health</span>;
  return <StatePair tone={HEALTH_TONE[health] ?? "skipped"} word={health} />;
}

/** Whether a sync is under way. */
export function running(op: CdOperation | null | undefined) {
  return !!op && (op.phase === "Running" || op.phase === "DryRun" || op.phase === "Failing" || op.phase === "Terminating");
}

/** The operation's tone: running, succeeded or failed. */
export function opTone(op: CdOperation) {
  return running(op) ? "running" : op.phase === "Succeeded" ? "succeeded" : op.phase === "Failed" ? "failed" : "skipped";
}

/** The operation in a few words: "Syncing", "Synced by self-heal", "Sync failed". */
export function opWords(op: CdOperation | null | undefined) {
  if (!op) return "";
  const by = op.trigger === "manual" ? "" : (TRIGGER_WORD[op.trigger] ?? op.trigger).toLowerCase();
  switch (op.phase) {
    case "Running":
    case "DryRun":
      return by ? `Syncing (${by})` : "Syncing";
    case "Failing":
      return "Sync failing";
    case "Terminating":
      return "Stopping";
    case "Succeeded":
      return by ? `Synced by ${by}` : "Synced";
    case "Failed":
      return "Sync failed";
    default:
      return op.phase;
  }
}

export const shortRev = (r: string | null | undefined) => (r ? (/^[0-9a-f]{12,}$/.test(r) ? r.slice(0, 7) : r) : "");

/** An app's place: cluster/namespace. */
export const placeOf = (a: Pick<CdApp, "cluster" | "namespace">) => [a.cluster, a.namespace].filter(Boolean).join("/");

/** An object as people say it: `Deployment web/api`. */
export const objectLabel = (k: CdResourceKey) => `${k.kind} ${k.namespace ? `${k.namespace}/` : ""}${k.name}`;

/** An object's identity, unique within an app. */
export const objectKey = (k: CdResourceKey) => `${k.group}/${k.kind}/${k.namespace}/${k.name}`;

/** A kind as a small mono badge (`deploy`, `rs`, `pod`); the full kind on hover. */
export function KindBadge({ kind, group, className }: { kind: string; group?: string; className?: string }) {
  return (
    <span className={cn("cd-kind", className)} title={group ? `${kind} (${group})` : kind}>
      {kindShort(kind)}
    </span>
  );
}

/**
 * A node's health and sync, compact: a dot each, and a word only where it says something
 * (not healthy, or out of sync). Children have no sync.
 */
export function NodeMarks({ n, words = true }: { n: Pick<CdNode, "health" | "sync" | "managed" | "hook">; words?: boolean }) {
  const h = n.health;
  const htone = h ? (HEALTH_TONE[h] ?? "skipped") : null;
  const s = n.managed ? n.sync : null;
  const stone = s ? (SYNC_TONE[s] ?? "skipped") : null;
  return (
    <span className="cd-marks">
      {htone ? (
        <span className="status" title={`Health: ${h}`}>
          <i className="dot" data-status={htone} aria-hidden="true" />
          {words && h !== "Healthy" ? <span className="status-text" data-status={htone}>{h}</span> : <span className="sr-only">{h}</span>}
        </span>
      ) : null}
      {stone && !n.hook ? (
        <span className="status" title={`Sync: ${WORD[s ?? ""] ?? s}`}>
          <i className="dot cd-sync-dot" data-status={stone} aria-hidden="true" />
          {words && s === "OutOfSync" ? <span className="status-text" data-status={stone}>Out of sync</span> : <span className="sr-only">{WORD[s ?? ""] ?? s}</span>}
        </span>
      ) : null}
      {n.hook ? <span className="cd-hook">hook</span> : null}
    </span>
  );
}

/** What a node says in one short line: ready, restarts, the pod's state, its age. */
export function nodeLine(n: CdNode, ageText: string): string {
  const parts: string[] = [];
  if (n.kind === "Pod" && n.status && n.status !== "Running") parts.push(n.status);
  // A finished pod's containers are done, not unready.
  if (n.ready && !(n.kind === "Pod" && n.status === "Completed")) parts.push(n.kind === "Pod" || n.kind === "EndpointSlice" ? `${n.ready} ready` : n.ready);
  if (n.restarts) parts.push(`${n.restarts} restart${n.restarts === 1 ? "" : "s"}`);
  const rev = n.kind === "ReplicaSet" || n.kind === "ControllerRevision" ? n.info.find((i) => i.name === "Revision")?.value : undefined;
  if (rev) parts.push(`rev ${rev}`);
  if (!n.ready && !n.restarts) {
    const first = n.info.find((i) => i.name !== "Revision" && i.name !== "Status");
    if (first) parts.push(first.name === "Keys" ? `${first.value} key${first.value === "1" ? "" : "s"}` : first.value);
  }
  if (n.missing) parts.push("not in the cluster");
  else if (n.requires_pruning) parts.push("to prune");
  if (ageText) parts.push(ageText);
  return parts.join(" · ");
}

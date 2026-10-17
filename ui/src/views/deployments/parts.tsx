/** Deployments' small pieces: sync and health as a dot and a word, in kiln's eight tones. */
import type { CdApp, CdOperation, CdResourceKey } from "@/api/types";
import { cn } from "@/lib/utils";

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

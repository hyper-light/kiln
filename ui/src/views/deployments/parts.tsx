/** Deployments' small pieces: sync and health as Argo CD's signs and a word, in kiln's eight tones;
 *  what's under way turns, and a change plays in. */
import { CircleArrowUp, CircleCheck, CircleHelp, CirclePause, CircleX, Ghost, Heart, HeartCrack, LoaderCircle, RefreshCw, type LucideIcon } from "lucide-react";
import type { CdApp, CdNode, CdOperation, CdResourceKey } from "@/api/types";
import { cn } from "@/lib/utils";
import { kindShort } from "./model";

/** Keys of one app's data (lib/resource.ts): `cd/app/<id>/…`, so `invalidate(appKey(id))` reads all of it again. */
export const appKey = (id: string) => `cd/app/${id}/`;

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

/** Argo CD's signs: sync (synced, out of sync), health (a heart, broken when degraded, turning while
 *  it progresses, paused, a ghost when missing). */
const SYNC_ICON: Record<string, LucideIcon> = { Synced: CircleCheck, OutOfSync: CircleArrowUp, Unknown: CircleHelp, Hook: CircleHelp };
const HEALTH_ICON: Record<string, LucideIcon> = { Healthy: Heart, Progressing: LoaderCircle, Degraded: HeartCrack, Suspended: CirclePause, Missing: Ghost, Unknown: CircleHelp };

/**
 * A state as a sign and a word. Keyed by what it says, so a change plays in (it fades and settles)
 * rather than snapping; `spin` turns the sign while something is under way.
 */
export function StateGlyph({
  icon: Icon,
  tone,
  word,
  spin = false,
  words = true,
  size = 14,
  title,
  className,
}: {
  icon: LucideIcon;
  tone: string;
  word: string;
  spin?: boolean;
  words?: boolean;
  size?: number;
  title?: string;
  className?: string;
}) {
  const said = WORD[word] ?? word;
  return (
    <span className={cn("status cd-glyph", className)} title={title}>
      <span key={said} className="cd-glyph-in">
        <Icon size={size} className={cn("cd-glyph-i", spin && "cd-spin")} data-status={tone} aria-hidden="true" />
        {words ? (
          <span className="status-text" data-status={tone}>
            {said}
          </span>
        ) : (
          <span className="sr-only">{said}</span>
        )}
      </span>
    </span>
  );
}

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

/** Sync, or while a sync is under way, the sync: its sign turning. */
export function SyncPair({ sync, op }: { sync: string; op?: CdOperation | null }) {
  if (running(op)) return <StateGlyph icon={RefreshCw} tone="running" word={op?.phase === "Terminating" ? "Stopping" : "Syncing"} spin title={opWords(op)} />;
  return <StateGlyph icon={SYNC_ICON[sync] ?? CircleHelp} tone={SYNC_TONE[sync] ?? "skipped"} word={sync} />;
}

export function HealthPair({ health }: { health: string | null | undefined }) {
  if (!health) return <span className="cd-none">No health</span>;
  return <StateGlyph icon={HEALTH_ICON[health] ?? CircleHelp} tone={HEALTH_TONE[health] ?? "skipped"} word={health} spin={health === "Progressing"} />;
}

/** A sync, under way or done: its sign (turning while it runs) and what it is in words. */
export function OpPair({ op }: { op: CdOperation }) {
  const icon = running(op) ? RefreshCw : op.phase === "Succeeded" ? CircleCheck : op.phase === "Failed" ? CircleX : CircleHelp;
  return <StateGlyph icon={icon} tone={opTone(op)} word={opWords(op)} spin={running(op)} />;
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
/** Providers' names as people know them, for targets beyond Kubernetes (docs/cd-targets.md §3). */
const PROVIDER_WORDS: Record<string, string> = {
  "aws-ecs": "ECS",
  "aws-lambda": "Lambda",
  "aws-beanstalk": "Elastic Beanstalk",
  "aws-ec2": "EC2",
  "aws-static": "S3 + CloudFront",
  cloudformation: "CloudFormation",
  "gcp-run": "Cloud Run",
  "gcp-functions": "Cloud Run functions",
  "gcp-appengine": "App Engine",
  "gcp-compute": "Compute Engine",
  "azure-containerapps": "Container Apps",
  "azure-functions": "Azure Functions",
  "azure-appservice": "App Service",
  "azure-vmss": "Scale Sets",
  "azure-static": "Static Web Apps",
  "cloudflare-workers": "Workers",
  "cloudflare-pages": "Pages",
  vercel: "Vercel",
  railway: "Railway",
  fly: "Fly.io",
  netlify: "Netlify",
  render: "Render",
  heroku: "Heroku",
  digitalocean: "App Platform",
  nomad: "Nomad",
  host: "hosts",
  compose: "Compose",
  microvm: "microVMs",
  terraform: "Terraform",
  pulumi: "Pulumi",
  job: "a job",
};

/**
 * Where an app runs: `cluster/namespace`, or for a target beyond Kubernetes (its cluster named
 * `provider/account/place…`) the platform, its place and the account: `ECS · us-east-1 · aws-prod`,
 * with the provider's scope (an ECS cluster) when it has one.
 */
export const placeOf = (a: Pick<CdApp, "cluster" | "namespace">) => {
  const [provider, account, ...place] = (a.cluster ?? "").split("/");
  const words = provider ? PROVIDER_WORDS[provider] : undefined;
  if (words && account) return [words, ...place, account, a.namespace].filter(Boolean).join(" · ");
  return [a.cluster, a.namespace].filter(Boolean).join("/");
};

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
export function NodeMarks({ n, words = true }: { n: Pick<CdNode, "health" | "sync" | "managed" | "hook" | "tracked_by">; words?: boolean }) {
  const h = n.health;
  const htone = h ? (HEALTH_TONE[h] ?? "skipped") : null;
  const s = n.managed ? n.sync : null;
  const stone = s ? (SYNC_TONE[s] ?? "skipped") : null;
  return (
    <span className="cd-marks">
      {htone && h ? (
        <StateGlyph icon={HEALTH_ICON[h] ?? CircleHelp} tone={htone} word={h} words={words && h !== "Healthy"} spin={h === "Progressing"} size={12} title={`Health: ${h}`} />
      ) : null}
      {stone && s && !n.hook ? (
        <StateGlyph icon={SYNC_ICON[s] ?? CircleHelp} tone={stone} word={s} words={words && s === "OutOfSync"} size={12} title={`Sync: ${WORD[s] ?? s}`} className="cd-sync-glyph" />
      ) : null}
      {n.hook ? <span className="cd-hook">hook</span> : null}
      {n.tracked_by ? (
        <span className="cd-owner" title={`${n.tracked_by}'s tracking is on it: a sync of this app takes it over`}>
          {n.tracked_by.slice(n.tracked_by.indexOf("/") + 1)}'s
        </span>
      ) : null}
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

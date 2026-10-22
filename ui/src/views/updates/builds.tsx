/**
 * System → Builds: what runners download, one build per platform, each from kiln's release
 * pipeline, a machine of its own or an upload. Each platform says which build it has, where it came
 * from, how big it is and when it was made; while the release pipeline builds them, how far each
 * is, its tile moving while its job runs. Picking a platform opens its build's log under the tiles.
 * Build every platform runs that pipeline; a build is uploaded or removed in place.
 */
import * as React from "react";
import { ChevronDown, Upload } from "lucide-react";
import { api } from "@/api/client";
import type { Job, KilnBinary, Run, UpdateJob, UpdatesOverview } from "@/api/types";
import { Button } from "@/components/ui/button";
import { Confirm } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { formatBytes, relativeTime, shortSha } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { PEER_BASE } from "@/lib/peer-base";
import { invalidate } from "@/lib/resource";
import { Link } from "@/lib/router";
import { BuildLogPanel } from "./build-log";
import { pick, readAgainSoon } from "./state";
import { archWords, buildOf, buildStatus, isBuilding, originWords, platformLine, plural, type PlatformView, type Tone } from "./words";

const DIE: Record<Tone, string> = { ok: "#a8c9bc", warn: "#d0b591", bad: "#d6a2aa", run: "#b8b0d8", off: "#5c6470" };

/** As many columns as fill the rows evenly, four or five at most. */
function columns(n: number) {
  return n <= 5 ? Math.max(1, n) : n % 3 === 0 ? 3 : 4;
}

export function PlatformsPanel({
  data,
  platforms,
  releaseJobs,
  updateJobs,
  picked,
  nowMs,
  uploading,
  onChoose,
}: {
  data: UpdatesOverview;
  platforms: PlatformView[];
  /** The jobs of the release run under way, when read. */
  releaseJobs: Job[] | null;
  /** The upgrades kiln knows, live: a machine's build of itself is one. */
  updateJobs: UpdateJob[];
  picked: string | null;
  nowMs: number;
  uploading: string | null;
  /** Pick a binary to upload for a platform. */
  onChoose: (target: string) => void;
}) {
  const build = data.build;
  const last = build?.last ?? null;
  const building = isBuilding(last);
  const status = build ? buildStatus(build, platforms, nowMs, releaseJobs) : null;
  const [busy, setBusy] = React.useState(false);
  const [uploadOpen, setUploadOpen] = React.useState(false);
  const [target, setTarget] = React.useState(data.targets[0] ?? "");
  const [removing, setRemoving] = React.useState<string | null>(null);
  const uploadId = React.useId();
  const logId = React.useId();
  const shown = platforms.find((p) => picked === `build:${p.key}`) ?? null;
  const start = async () => {
    if (!build) return;
    setBusy(true);
    try {
      const run = await api.buildEveryPlatform();
      toast({ title: "Building every platform", description: `${build.repo} ${build.pipeline} run #${run.number}`, tone: "running" });
      readAgainSoon();
    } catch (e) {
      toast.error("Couldn't start the build", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  const remove = async (p: PlatformView) => {
    try {
      await api.deleteBinary(p.target);
      invalidate("updates");
    } catch (e) {
      toast.error("Couldn't remove it", errorMessage(e));
    } finally {
      setRemoving(null);
    }
  };
  // A tile picked opens its log under the tiles: brought into view where that's below the screen.
  const choose = (p: PlatformView) => {
    const key = `build:${p.key}`;
    pick(key);
    if (picked !== key) requestAnimationFrame(() => document.getElementById(logId)?.scrollIntoView({ block: "nearest" }));
  };
  return (
    <section className="up-panel up-panel-flush" aria-labelledby="up-platforms-title">
      <header className="up-panel-head">
        <div className="up-panel-words">
          <h3 id="up-platforms-title">Platforms</h3>
          <p>{plural(platforms.length, "platform")} · runners download theirs from the server</p>
          {status?.more && <p>{status.more}</p>}
        </div>
        <div className="up-panel-actions">
          {status && (
            <span className="up-instep" data-tone={status.tone}>
              <span>
                {status.before}
                {status.run && <Link to={`/runs/${status.run.id}`}>run #{status.run.number}</Link>}
                {status.after}
              </span>
            </span>
          )}
          <Button size="xs" variant="ghost" aria-expanded={uploadOpen} onClick={() => setUploadOpen(!uploadOpen)}>
            <Upload /> Upload a build
          </Button>
          {build && (
            <Button size="xs" variant="secondary" disabled={busy || building || Boolean(build.error)} onClick={() => void start()}>
              {building ? "Building…" : "Build every platform"}
            </Button>
          )}
        </div>
      </header>
      {uploadOpen && (
        <div className="up-upload">
          <label htmlFor={uploadId}>Platform</label>
          <div className="select up-select up-upload-select">
            <select id={uploadId} value={target} onChange={(e) => setTarget(e.target.value)}>
              {data.targets.map((t) => (
                <option key={t} value={t}>
                  {t}
                </option>
              ))}
            </select>
            <ChevronDown size={13} aria-hidden="true" />
          </div>
          <Button size="xs" variant="secondary" disabled={uploading !== null || !target} onClick={() => onChoose(target)}>
            {uploading ? "Uploading…" : "Choose a file"}
          </Button>
          <Button size="xs" variant="ghost" onClick={() => setUploadOpen(false)}>
            Cancel
          </Button>
        </div>
      )}
      {platforms.length > 0 ? (
        <div className="up-platforms" data-cols={columns(platforms.length)}>
          {platforms.map((p) => (
            <PlatformTile
              key={p.key}
              p={p}
              commit={build?.commit ?? null}
              last={last}
              picked={shown === p}
              logId={logId}
              nowMs={nowMs}
              removing={removing === p.key}
              uploading={uploading}
              onPick={() => choose(p)}
              onRemove={() => setRemoving(p.key)}
              onKeep={() => setRemoving(null)}
              onRemoved={() => void remove(p)}
              onChoose={onChoose}
            />
          ))}
        </div>
      ) : (
        <p className="up-none">No builds here yet: Build every platform, or upload one.</p>
      )}
      {shown && <BuildLogPanel key={shown.key} id={logId} p={shown} build={build} updateJobs={updateJobs} />}
    </section>
  );
}

/** A platform's chip, its die drawn as it goes while its job builds it, as the picture's chip is. */
function ChipIcon({ tone, building }: { tone: Tone; building: boolean }) {
  const id = "up-chip" + React.useId().replace(/[^a-zA-Z0-9_-]/g, "");
  return (
    <svg viewBox="0 0 38 30" fill="none" aria-hidden="true">
      <defs>
        <linearGradient id={id} x1="0" y1="0" x2="1" y2=".8">
          <stop stopColor="#a8d2c7" />
          <stop offset=".5" stopColor="#a6bfdc" />
          <stop offset="1" stopColor="#b5a5cf" />
        </linearGradient>
      </defs>
      <path d="M19 5 34 13.5 19 22 4 13.5Z" fill="#7c94a51c" stroke={`url(#${id})`} strokeWidth=".8" />
      <path d="M4 13.5v3L19 25v-3M34 13.5v3L19 25" stroke="#8aa0b4" strokeOpacity=".45" strokeWidth=".8" />
      <path d="m19 9.5 7 4-7 4-7-4Z" pathLength={42} className="up-chip-die" data-building={building || undefined} stroke={DIE[tone]} strokeWidth=".9" strokeOpacity=".9" />
    </svg>
  );
}

function PlatformTile({
  p,
  commit,
  last,
  picked,
  logId,
  nowMs,
  removing,
  uploading,
  onPick,
  onRemove,
  onKeep,
  onRemoved,
  onChoose,
}: {
  p: PlatformView;
  /** The commit the server runs: the release pipeline's builds of it are what runners get. */
  commit: string | null;
  last: Run | null;
  picked: boolean;
  /** The log under the tiles, which picking this one opens. */
  logId: string;
  nowMs: number;
  removing: boolean;
  uploading: string | null;
  onPick: () => void;
  onRemove: () => void;
  onKeep: () => void;
  onRemoved: () => void;
  onChoose: (target: string) => void;
}) {
  const b = p.binary;
  const run = last ? `release run #${last.number}` : "the release run";
  const toward = p.state === "building" || p.state === "queued";
  // While its job builds it, what the picture says: the commit the run builds, and the run.
  const from = toward ? platformLine(p, last) : p.state === "failed" && !b ? `${run} · failed` : b ? originWords(b) : "no build yet";
  // What runners get changes only for a build of the server's own commit (crate::updates::adopts), and never the server's own.
  const adopted = toward && last !== null && commit !== null && last.sha.toLowerCase() === commit.toLowerCase() && b?.origin !== "server";
  const made = adopted ? (p.state === "building" ? "now" : "—") : b ? relativeTime(b.at, nowMs) : "—";
  const [shadow] = p.shadowed;
  return (
    <div className="up-platform" data-tone={p.tone} data-state={p.state} data-hl={picked || undefined}>
      <button type="button" className="up-p-pick" aria-pressed={picked} aria-controls={picked ? logId : undefined} aria-label={`${p.os} ${archWords(p)}: its build and log`} onClick={onPick} />
      <div className="up-p-head">
        <ChipIcon tone={p.tone} building={p.state === "building"} />
        <div>
          <span className="up-p-os">{p.os}</span>
          <span className="up-p-arch" title={p.target}>
            {archWords(p)}
          </span>
        </div>
      </div>
      <dl className="up-p-facts">
        <dt>Build</dt>
        <dd data-tone={p.state === "current" || (toward && !adopted) ? undefined : p.tone} title={b ? `build ${b.build} · ${p.target}` : p.target}>
          {adopted && last ? `${b ? buildOf(p) : "none"} → ${last.sha.slice(0, 7)}` : buildOf(p)}
        </dd>
        <dt>From</dt>
        <dd title={from}>{from}</dd>
        <dt>Size</dt>
        <dd>{b ? formatBytes(b.size) : "—"}</dd>
        <dt>Made</dt>
        <dd>{made}</dd>
      </dl>
      {toward && (
        <span className="up-track up-track-wide" data-state={p.state} aria-hidden="true">
          <i />
        </span>
      )}
      {shadow && !removing && (
        <p className="up-p-shadow">
          <span>
            Also kept: {shadow.sha ? shortSha(shadow.sha) : shadow.build} from {originWords(shadow)}, not handed out
          </span>
          <Button size="xs" variant="ghost" onClick={onRemove} aria-label={`Remove the kept ${p.target} build ${shadow.build}`}>
            Remove
          </Button>
        </p>
      )}
      {removing && (shadow ?? b) && <RemoveConfirm target={p.target} binary={(shadow ?? b) as KilnBinary} shadowed={Boolean(shadow)} onRemoved={onRemoved} onKeep={onKeep} />}
      <div className="up-p-acts">
        {b && !PEER_BASE && (
          <Button asChild size="xs" variant="ghost">
            <a href={`/dl/kiln/${encodeURIComponent(b.target)}/${encodeURIComponent(b.build)}`} download>
              Download
            </a>
          </Button>
        )}
        {b && b.origin !== "server" && !removing && (
          <Button size="xs" variant="ghost" onClick={onRemove} aria-label={`Remove the ${p.target} binary`}>
            Remove
          </Button>
        )}
        {!b && (
          <Button size="xs" variant="ghost" disabled={uploading !== null} onClick={() => onChoose(p.target)}>
            {uploading === p.target ? "Uploading…" : "Upload a build"}
          </Button>
        )}
      </div>
    </div>
  );
}

/** Removing a platform's binary, confirmed in place: what runners get instead. */
function RemoveConfirm({ target, binary, shadowed, onRemoved, onKeep }: { target: string; binary: KilnBinary; shadowed: boolean; onRemoved: () => void; onKeep: () => void }) {
  return (
    <Confirm
      className="up-confirm"
      label={`Remove the ${target} binary`}
      tone="danger"
      actions={
        <>
          <Button size="xs" variant="danger" onClick={onRemoved}>
            Remove
          </Button>
          <Button size="xs" variant="ghost" onClick={onKeep}>
            Keep it
          </Button>
        </>
      }
    >
      <p>
        {shadowed
          ? `Remove build ${binary.build}, kept beside the server's own? Runners of ${target} get the server's own build either way.`
          : `Remove the ${target} binary? Build ${binary.build} is no longer handed to machines of that platform until kiln builds every platform again or one is uploaded.`}
      </p>
    </Confirm>
  );
}

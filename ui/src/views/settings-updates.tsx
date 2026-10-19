/**
 * Settings → Updates: which build the server runs and how it is upgraded, each machine's runner
 * and its processes, Upgrade per runner and for all of them with live output, and the binaries
 * kiln hands out. Runners are upgraded without a restart: the old runner keeps its terminals,
 * agents and jobs. Every action is confirmed in place first and says what will happen. Binaries for
 * other platforms come from kiln's own release pipeline (Build every platform) or an upload.
 */
import * as React from "react";
import { Link, useNavigate } from "@/lib/router";
import { create } from "zustand";
import { Bot, ChevronDown, RefreshCw, Trash2, Upload } from "lucide-react";
import { api } from "@/api/client";
import { events, useServerEvents } from "@/api/events";
import type { FleetTarget, KilnBinary, ReleaseBuild, ReleaseWait, RunnerInstance, RunnerUpdate, ServerUpdate, UpdateJob, UpdatePolicy, UpdatesOverview, UpgradeWaits, UpgradeWay, UpgradeWayOpen } from "@/api/types";
import { Button } from "@/components/ui/button";
import { Hold } from "@/components/ui/hold";
import { Confirm, Facts, SelectField } from "@/components/ui/misc";
import { SettingRow, SettingsGroup, openSettings } from "@/components/ui/settings";
import { StatusDot } from "@/components/ui/status";
import { toast } from "@/components/ui/toast";
import { fuller, mergeJobs, updatesRes, useRes, useUpdateJobs } from "@/lib/data";
import { formatBytes, formatDuration, relativeTime, shortSha } from "@/lib/format";
import { errorMessage, useNow } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { cn } from "@/lib/utils";
import { SpawnDialog } from "@/views/harness/spawn";
import "./settings-updates.css";

/** A server upgrade in flight: the page reconnects to the new server and says what changed. */
const PENDING_KEY = "kiln.serverUpgrade";
/** Finished upgrades stay shown under their runner this long. */
const RECENT_S = 3600;
/** How long a server upgrade may take before the page says it didn't change the build. */
const PATIENCE_MS = 4 * 60_000;

interface Confirming {
  /** Where it is asked: "server", "all", or "runner:<name>". */
  scope: string;
  title: string;
  /** One sentence; the plan below says step by step what will happen. */
  lead: string;
  plan: React.ReactNode;
  confirmLabel: string;
  run: () => Promise<void>;
}

const plural = (n: number, one: string) => `${n} ${one}${n === 1 ? "" : "s"}`;

function holding(i: RunnerInstance) {
  const parts = [i.terminals ? plural(i.terminals, "terminal") : "", i.agents ? plural(i.agents, "agent session") : "", i.jobs ? plural(i.jobs, "job") : ""].filter(Boolean);
  return parts.length ? parts.join(", ") : "nothing";
}

type Pending = { from: string; at: number } | null;

function readPending(): Pending {
  try {
    const v = sessionStorage.getItem(PENDING_KEY);
    return v ? (JSON.parse(v) as { from: string; at: number }) : null;
  } catch {
    return null;
  }
}

/** The server upgrade this tab started and waits for (kept for the tab, across the restart). */
const usePending = create<{ pending: Pending }>(() => ({ pending: readPending() }));

function setPending(v: Pending) {
  try {
    if (v) sessionStorage.setItem(PENDING_KEY, JSON.stringify(v));
    else sessionStorage.removeItem(PENDING_KEY);
  } catch {
    /* private mode */
  }
  usePending.setState({ pending: v });
}

/** Back from the restart (or out of patience): say what the server runs now. */
async function settlePending() {
  const p = usePending.getState().pending;
  if (!p) return;
  try {
    const d = await api.updates();
    if (d.server.build !== p.from) {
      toast.success("Server upgraded", `build ${p.from} → ${d.server.build}`);
      setPending(null);
    } else if (Date.now() - p.at > PATIENCE_MS) {
      toast({ title: "The server is still on the same build", description: `build ${d.server.build}`, tone: "info" });
      setPending(null);
    }
  } catch {
    /* still restarting: the next reconnection asks again */
  }
}
events.onReopen(() => void settlePending());

/** Runner events come in bursts: Updates is read again once they settle. */
let soon: number | null = null;
function readAgainSoon() {
  if (soon !== null) window.clearTimeout(soon);
  soon = window.setTimeout(() => {
    soon = null;
    invalidate("updates");
  }, 350);
}

/** What finds each group in Settings' search, besides its title. */
const SERVER_WORDS = "server upgrade restart install build platform version image node release binary upload check";
const RUNNERS_WORDS = "runners machines upgrade all update policy automatic notify off select diagnose retry build";
const WAITS_ABOUT = "How long an upgrade may take before it reads as failed";
const WAITS_WORDS = "timeout minutes upgrade running jobs build wait kiln.toml";
const BINARIES_ABOUT = "What runners download, a build per platform";
const BINARIES_WORDS = "binaries platform upload build every platform download release remove";

/** Updates while they load: its groups, and their space. */
export function UpdatesHold() {
  return (
    <>
      <SettingsGroup id="updates.server" title="Server" words={SERVER_WORDS}>
        <Hold height={110} label="Loading updates" />
      </SettingsGroup>
      <SettingsGroup id="updates.runners" title="Runners" words={RUNNERS_WORDS}>
        <Hold height={160} />
      </SettingsGroup>
      <SettingsGroup id="updates.waits" title="How long kiln waits" about={WAITS_ABOUT} words={WAITS_WORDS} collapsible />
      <SettingsGroup id="updates.binaries" title="Binaries" about={BINARIES_ABOUT} words={BINARIES_WORDS} collapsible />
    </>
  );
}

/** What Updates says in its header, closed or open: what's waiting to be upgraded. */
export function UpdatesStatus() {
  const data = useRes(updatesRes).data;
  const behind = data.runners.filter((r) => r.connected && !r.up_to_date && r.how !== "left").length;
  if (data.server.newer) return <span data-tone="queued">Server upgrade ready</span>;
  if (behind > 0) return <span data-tone="queued">{plural(behind, "runner")} behind</span>;
  return <span data-tone="succeeded">Up to date</span>;
}

export function UpdatesPanel() {
  const data = useRes(updatesRes).data;
  const live = useUpdateJobs();
  const [confirm, setConfirm] = React.useState<Confirming | null>(null);
  // Runners chosen for one update policy at once, and that policy.
  const [selected, setSelected] = React.useState<Set<string>>(() => new Set());
  const [bulk, setBulk] = React.useState<UpdatePolicy>("notify");
  const select = (name: string, on: boolean) =>
    setSelected((cur) => {
      const next = new Set(cur);
      if (on) next.add(name);
      else next.delete(name);
      return next;
    });
  const [checking, setChecking] = React.useState(false);
  // The runner an agent is being started for, to find out why its upgrade failed.
  const [diagnosing, setDiagnosing] = React.useState<{ r: RunnerUpdate; job: UpdateJob | undefined } | null>(null);
  const navigate = useNavigate();
  const pending = usePending((s) => s.pending);
  const fileRef = React.useRef<HTMLInputElement>(null);
  const uploadTarget = React.useRef<string>("");
  const [uploading, setUploading] = React.useState<string | null>(null);

  useServerEvents((ev) => {
    if (ev.type === "agent") readAgainSoon();
  });

  const jobs = React.useMemo(() => mergeJobs(data, live), [data, live]);
  const nowS = useNow() / 1000;
  const latest = (kind: UpdateJob["kind"], target: string) => jobs.find((j) => j.kind === kind && j.target === target && (j.state === "running" || nowS - (j.ended_at ?? j.started_at) < RECENT_S));
  /** A job just started here: shown at once, before its first event. */
  const started = (job: UpdateJob) => {
    useUpdateJobs.setState((m) => ({ ...m, [job.id]: fuller(m[job.id], job) }), true);
  };

  const check = async () => {
    setChecking(true);
    try {
      await api.checkUpdates();
      invalidate("updates");
    } catch (e) {
      toast.error("Couldn't check for updates", errorMessage(e));
    } finally {
      setChecking(false);
    }
  };

  const startRunner = async (r: RunnerUpdate, way?: UpgradeWay) => {
    try {
      started(await api.upgradeRunner(r.name, way));
      toast({ title: way === "build" ? `Building kiln on ${r.name}` : `Upgrading ${r.name}`, description: "Its terminals, agents and jobs keep running.", tone: "running" });
    } catch (e) {
      toast.error(`Couldn't upgrade ${r.name}`, errorMessage(e));
    }
    readAgainSoon();
  };

  const setPolicy = async (names: string[] | "all", policy: UpdatePolicy) => {
    try {
      const [one] = names === "all" ? [] : names;
      if (names !== "all" && names.length === 1 && one) await api.setRunnerPolicy(one, policy);
      else await api.setPolicies(policy, names);
    } catch (e) {
      toast.error("Couldn't change the update policy", errorMessage(e));
    }
    invalidate("updates");
  };

  const startAll = async () => {
    try {
      const out = await api.upgradeAllRunners();
      for (const j of out.jobs) started(j);
      if (out.jobs.length) toast({ title: `Upgrading ${plural(out.jobs.length, "runner")}`, description: "Nothing they run is stopped.", tone: "running" });
      for (const e of out.errors) toast.error("A runner wasn't upgraded", e);
    } catch (e) {
      toast.error("Couldn't upgrade the runners", errorMessage(e));
    }
    readAgainSoon();
  };

  const startServer = async (s: ServerUpdate, action: "restart" | "replace") => {
    try {
      const job = await api.upgradeServer(action);
      started(job);
      if (job.state === "failed") {
        toast.error("The server wasn't upgraded", job.error ?? undefined);
        return;
      }
      setPending({ from: s.build, at: Date.now() });
      // A restart that never comes still gets an answer.
      window.setTimeout(() => void settlePending(), PATIENCE_MS + 1_000);
    } catch (e) {
      toast.error("Couldn't upgrade the server", errorMessage(e));
    }
  };

  const pickUpload = (target: string) => {
    uploadTarget.current = target;
    fileRef.current?.click();
  };
  const upload = async (file: File) => {
    const target = uploadTarget.current;
    setUploading(target);
    try {
      const b = await api.uploadBinary(target, file);
      toast.success("Binary uploaded", `build ${b.build} for ${target}`);
      invalidate("updates");
    } catch (e) {
      toast.error("Upload failed", errorMessage(e));
    } finally {
      setUploading(null);
    }
  };

  // Runners on a branch or a local build run it on purpose: "Upgrade all" leaves them be.
  const outdated = data.runners.filter((r) => r.connected && !r.up_to_date && r.how !== "left" && r.how !== "wait" && r.policy !== "off" && !r.locked);
  const byPolicy = (p: UpdatePolicy) => data.runners.filter((r) => r.policy === p).length;
  const newest = data.newest ?? null;
  const current = data.runners.filter((r) => r.up_to_date).length;
  const runningRunners = new Set(jobs.filter((j) => j.kind === "runner" && j.state === "running").map((j) => j.target));

  const summary =
    data.runners.length === 0
      ? "No machines have joined yet."
      : newest
        ? `${current} of ${data.runners.length} on ${shortSha(newest.commit)}, the newest kiln${newest.from === "server" ? " (the server's)" : ` (from ${newest.from})`} · ${
            data.follow ? `${byPolicy("auto")} automatic, ${byPolicy("notify")} notify me, ${byPolicy("off")} off` : "automatic upgrades are off ([updates] follow)"
          }`
        : `${current} of ${data.runners.length} on the server's build`;

  return (
    <>
      <input
        ref={fileRef}
        type="file"
        className="sr-only"
        tabIndex={-1}
        aria-hidden="true"
        onChange={(e) => {
          const f = e.target.files?.[0];
          e.target.value = "";
          if (f) void upload(f);
        }}
      />
      <ServerSection
        s={data.server}
        job={latest("server", "server")}
        pending={pending !== null && data.server.build === pending.from}
        uploading={uploading}
        checking={checking || data.server.checking}
        onCheck={() => void check()}
        onUpload={() => pickUpload(data.server.target)}
        onAction={(action) =>
          setConfirm({
            scope: "server",
            title: action === "restart" ? "Restart the server onto the new image?" : "Install the new server build?",
            lead: "What will happen:",
            plan: <ServerPlan s={data.server} action={action} />,
            confirmLabel: action === "restart" ? "Restart server" : "Install and restart",
            run: () => startServer(data.server, action),
          })
        }
        plan={confirm?.scope === "server" ? <InlinePlan confirm={confirm} onClose={() => setConfirm(null)} /> : null}
      />
      <SettingsGroup
        id="updates.runners"
        title="Runners"
        about={summary}
        words={RUNNERS_WORDS}
        actions={
          <Button
            variant="secondary"
            size="xs"
            disabled={outdated.length === 0 || outdated.every((r) => runningRunners.has(r.name))}
            onClick={() =>
              setConfirm({
                scope: "all",
                title: `Upgrade ${plural(outdated.length, "runner")}?`,
                lead: "What will happen:",
                plan: <AllPlan runners={outdated} />,
                confirmLabel: "Upgrade all",
                run: startAll,
              })
            }
          >
            Upgrade all{outdated.length ? ` (${outdated.length})` : ""}
          </Button>
        }
      >
        {confirm?.scope === "all" && <InlinePlan confirm={confirm} onClose={() => setConfirm(null)} />}
        {data.runners.length > 0 && (
          <div className="upd-bulk">
            <label className="upd-bulk-all">
              <input
                type="checkbox"
                className="check"
                checked={selected.size > 0 && selected.size === data.runners.length}
                onChange={(e) => setSelected(e.target.checked ? new Set(data.runners.map((r) => r.name)) : new Set())}
                aria-label="Select every runner"
              />
              <span>{selected.size ? `${plural(selected.size, "runner")} selected` : "Select runners"}</span>
            </label>
            <SelectField label="Updates for the selected runners" hideLabel value={bulk} onChange={(v) => setBulk(v as UpdatePolicy)} options={POLICIES} className="upd-policy" />
            <Button
              size="xs"
              variant="secondary"
              disabled={selected.size === 0}
              onClick={() => {
                void setPolicy(selected.size === data.runners.length ? "all" : [...selected], bulk);
                setSelected(new Set());
              }}
            >
              {selected.size === 0 ? "Set" : `Set for ${selected.size === data.runners.length ? "all" : plural(selected.size, "runner")}`}
            </Button>
          </div>
        )}
        <ul className="upd-runners">
          {data.runners.map((r) => {
            const job = latest("runner", r.name);
            const again = job?.state === "failed";
            return (
              <RunnerRow
                key={r.name}
                r={r}
                newest={newest}
                job={job}
                uploading={uploading}
                onUpload={() => r.target && pickUpload(r.target)}
                selected={selected.has(r.name)}
                onSelect={(on) => select(r.name, on)}
                onPolicy={(p) => void setPolicy([r.name], p)}
                plan={confirm?.scope === `runner:${r.name}` ? <InlinePlan confirm={confirm} onClose={() => setConfirm(null)} /> : null}
                onUpgrade={(way) => {
                  const build = way ? way === "build" : r.how === "build";
                  setConfirm({
                    scope: `runner:${r.name}`,
                    title: again ? (build ? `Try building kiln on ${r.name} again?` : `Try upgrading ${r.name} again?`) : build ? `Build the newest kiln on ${r.name}?` : `Upgrade ${r.name}?`,
                    lead: "What will happen:",
                    plan: <RunnerPlan r={r} newest={newest} build={build} />,
                    confirmLabel: again ? "Try again" : build ? "Build and upgrade" : "Upgrade",
                    run: () => startRunner(r, way),
                  });
                }}
                onDiagnose={() => setDiagnosing({ r, job })}
              />
            );
          })}
        </ul>
      </SettingsGroup>
      {data.waits && <WaitsSection waits={data.waits} />}
      <Binaries data={data} uploading={uploading} onUpload={pickUpload} onRemoved={() => invalidate("updates")} onBuilt={readAgainSoon} />
      {diagnosing && (
        <SpawnDialog
          open
          onOpenChange={(o) => {
            if (!o) setDiagnosing(null);
          }}
          defaults={{ target: `agent:${diagnosing.r.name}`, cwd: diagnosing.r.checkout ?? undefined, prompt: diagnosisPrompt(diagnosing.r, diagnosing.job, newest) }}
          onStarted={(agents) => {
            setDiagnosing(null);
            navigate(agents.length === 1 && agents[0] ? `/agents/${agents[0].id}` : "/agents");
          }}
        />
      )}
    </>
  );
}

/** Confirm an upgrade in place, showing step by step what it will do. */
function InlinePlan({ confirm, onClose }: { confirm: Confirming; onClose: () => void }) {
  const [busy, setBusy] = React.useState(false);
  return (
    <Confirm
      className="upd-confirm"
      label={confirm.title}
      actions={
        <>
          <Button
            size="xs"
            disabled={busy}
            onClick={async () => {
              setBusy(true);
              try {
                await confirm.run();
              } finally {
                setBusy(false);
                onClose();
              }
            }}
          >
            {confirm.confirmLabel}
          </Button>
          <Button size="xs" variant="ghost" disabled={busy} onClick={onClose}>
            Cancel
          </Button>
        </>
      }
    >
      <p className="upd-confirm-title">{confirm.title}</p>
      <p className="upd-sub">{confirm.lead}</p>
      {confirm.plan}
    </Confirm>
  );
}

/* ---------------------------- the server ---------------------------- */

function ServerSection({
  s,
  job,
  pending,
  uploading,
  checking,
  onCheck,
  onUpload,
  onAction,
  plan,
}: {
  s: ServerUpdate;
  job: UpdateJob | undefined;
  pending: boolean;
  uploading: string | null;
  checking: boolean;
  onCheck: () => void;
  onUpload: () => void;
  onAction: (a: "restart" | "replace") => void;
  /** Its upgrade, being confirmed. */
  plan: React.ReactNode;
}) {
  const runsAs = s.mode === "kubernetes" ? `Deployment ${s.namespace}/${s.deployment}` : s.mode === "service" ? "a service (kiln supervise)" : "a standalone process";
  const release = s.release.latest
    ? `v${s.release.latest}${s.release.newer ? " · newer" : ""}`
    : s.release.checked_at === 0
      ? "checking…"
      : s.release.error
        ? "couldn't ask GitHub"
        : "none published";
  const busy = job?.state === "running" || pending;
  const tone = pending ? "running" : s.newer ? "queued" : s.image?.error || (s.mode !== "kubernetes" && s.release.error) ? "timed_out" : "succeeded";
  return (
    <SettingsGroup
      id="updates.server"
      title="Server"
      about={`kiln ${s.version}, run as ${runsAs}`}
      words={SERVER_WORDS}
      actions={
        <>
          {s.action === "restart" && (s.newer || !!s.image?.error) && (
            <Button size="xs" variant={s.newer ? "primary" : "secondary"} disabled={busy} onClick={() => onAction("restart")}>
              {s.newer ? "Upgrade server" : "Restart anyway"}
            </Button>
          )}
          {s.action === "replace" && s.replacement && (
            <Button size="xs" disabled={busy} onClick={() => onAction("replace")}>
              {s.replacement.origin === "upload" ? `Install build ${s.replacement.build ?? ""}` : `Install kiln ${s.replacement.version ?? ""}`}
            </Button>
          )}
          {s.mode !== "kubernetes" && (
            <Button size="xs" variant="secondary" disabled={uploading !== null} onClick={onUpload}>
              <Upload /> {uploading === s.target ? "Uploading…" : "Upload binary"}
            </Button>
          )}
          <Button size="xs" variant="ghost" disabled={checking} onClick={onCheck} aria-label="Check for updates" title="Check for updates">
            <RefreshCw className={cn(checking && "animate-spin motion-reduce:animate-none")} />
          </Button>
        </>
      }
    >
      <Facts
        items={[
          { label: "Build", value: s.build },
          { label: "Platform", value: s.target },
          s.mode === "kubernetes"
            ? { label: "Image on node", value: s.image?.build ? `${s.image.build}${s.image.newer ? " · newer" : " · same"}` : s.image?.error ? "unknown" : "checking…", tone: s.image?.newer ? "pending" : undefined }
            : { label: "Latest release", value: release, tone: s.release.newer ? "pending" : undefined },
        ]}
      />
      <p className="upd-note" data-tone={tone}>
        <StatusDot status={tone} title={s.newer ? "A newer build is ready" : "Status"} />
        <span>{pending ? "Restarting… this page reconnects when the server is back." : s.note}</span>
      </p>
      {plan}
      {job && <JobLog job={job} />}
    </SettingsGroup>
  );
}

function ServerPlan({ s, action }: { s: ServerUpdate; action: "restart" | "replace" }) {
  if (action === "restart") {
    return (
      <ul className="upd-plan">
        <li>
          kiln asks Kubernetes to restart Deployment {s.namespace}/{s.deployment}: a rollout restart, as <code>kubectl rollout restart</code> does.
        </li>
        <li>
          The new pod runs {s.image?.build ? `build ${s.image.build}` : "whatever build is"} from the {s.image?.image || "kiln"} image on {s.image?.node ?? "the node"}
          {s.image?.build ? "" : " (kiln couldn't check which)"}; this one runs {s.build}.
        </li>
        <li>This page loses its connection for a few seconds and reconnects. Terminals and agents on runners keep running; terminals on the server itself end.</li>
      </ul>
    );
  }
  const r = s.replacement;
  return (
    <ul className="upd-plan">
      <li>
        {r?.origin === "upload" ? `kiln installs the uploaded build ${r.build ?? ""}` : `kiln downloads kiln ${r?.version ?? ""} for ${s.target} from GitHub and installs it`} over its own
        binary (build {s.build}).
      </li>
      <li>Its supervisor restarts the server into it within seconds, and this page reconnects.</li>
      <li>Runners keep their terminals, agents and jobs; terminals on the server itself end.</li>
    </ul>
  );
}

/* ---------------------------- runners ---------------------------- */

/** What a runner does with a newer kiln. */
const POLICIES: { value: UpdatePolicy; label: string }[] = [
  { value: "auto", label: "Updates: automatic" },
  { value: "notify", label: "Updates: notify me" },
  { value: "off", label: "Updates: off" },
];

function runnerState(r: RunnerUpdate, job: UpdateJob | undefined): { tone: string; text: string } {
  if (job?.state === "running") return { tone: "running", text: job.method === "build" ? "building itself" : "upgrading" };
  if (!r.connected) return { tone: "cancelled", text: "offline" };
  if (r.up_to_date) return { tone: "succeeded", text: "up to date" };
  // Its last upgrade failed, and it hasn't got there since: what it reads until it does, or is tried again.
  if (job?.state === "failed") return { tone: "failed", text: job.method === "build" ? "build failed" : "upgrade failed" };
  if (r.how === "left") return { tone: "timed_out", text: r.standing === "local" ? "local build" : "branch build" };
  if (r.how === "off") return { tone: "timed_out", text: "updates off" };
  if (r.how === "wait") return { tone: "queued", text: "waiting" };
  if (r.standing) return { tone: "queued", text: "behind" };
  return { tone: "queued", text: r.build ? "upgrade available" : "older runner" };
}

/** "builds 94f0847a1b2c itself" → "Builds 94f0847a1b2c itself". */
function sentence(s: string) {
  return s.charAt(0).toUpperCase() + s.slice(1);
}

/** The way an upgrade went: a build on the machine, or a binary downloaded. */
function wayOf(j: UpdateJob): UpgradeWay {
  return j.method === "build" ? "build" : "download";
}

/** The ways a runner can be upgraded now. An older server doesn't say: the way it would take, when it would. */
function upgradeWays(r: RunnerUpdate): UpgradeWayOpen[] {
  if (r.ways) return r.ways;
  if (!r.how || r.how === "wait" || r.how === "upgrading" || r.how === "left" || r.how === "off") return [];
  const build = r.how === "build";
  return [{ way: build ? "build" : "download", ok: (r.method === "native" && !build) || r.terminal, why: "" }];
}

/** A way's button: the first is the upgrade itself, the one that failed is tried again, any other is the alternative. */
function wayLabel(way: UpgradeWay, first: boolean, failedBefore: boolean) {
  if (failedBefore) return way === "build" ? "Retry build" : "Retry";
  if (first) return way === "build" ? "Build and upgrade" : "Upgrade";
  return way === "build" ? "Build it instead" : "Download instead";
}

/** "Waits for the release build of 31c0bba: run #232, its build step, on adacachy, 11m 04s in",
 * the run a link and its time ticking. */
function ReleaseWaitWords({ w, newest }: { w: ReleaseWait; newest: FleetTarget | null }) {
  const nowS = useNow() / 1000;
  const parts = [w.step ? `its ${w.step} step` : null, w.on ? `on ${w.on}` : null, w.since ? `${formatDuration(nowS - w.since)} in` : null].filter(Boolean);
  return (
    <>
      Waits for the release build{newest ? ` of ${shortSha(newest.commit)}` : ""}: <Link to={`/runs/${w.run}`}>run #{w.number}</Link>
      {parts.length > 0 ? `, ${parts.join(", ")}` : ""}
    </>
  );
}

function RunnerRow({
  r,
  newest,
  job,
  uploading,
  plan,
  onUpgrade,
  onUpload,
  selected,
  onSelect,
  onPolicy,
  onDiagnose,
}: {
  r: RunnerUpdate;
  /** The newest kiln, when kiln knows it. */
  newest: FleetTarget | null;
  job: UpdateJob | undefined;
  uploading: string | null;
  /** Its upgrade, being confirmed. */
  plan: React.ReactNode;
  /** Upgrade it that way (none: the way kiln picks). */
  onUpgrade: (way?: UpgradeWay) => void;
  onUpload: () => void;
  selected: boolean;
  onSelect: (on: boolean) => void;
  onPolicy: (p: UpdatePolicy) => void;
  /** Start an agent on it to find out why it isn't upgrading. */
  onDiagnose: () => void;
}) {
  const st = runnerState(r, job);
  const missing = Boolean(r.connected && !r.up_to_date && !r.target_build && r.target);
  const failed = job?.state === "failed" ? job : undefined;
  const behind = r.connected && !r.up_to_date && r.how !== "left" && r.policy !== "off" && !r.locked;
  const actionable = behind && job?.state !== "running" && r.how !== "upgrading" && !plan;
  const ways = upgradeWays(r);
  const open = ways.filter((w) => w.ok);
  // The way that failed comes first (to try it again), else the way kiln would take.
  const first = failed ? wayOf(failed) : r.how === "build" ? "build" : "download";
  open.sort((a, b) => Number(b.way === first) - Number(a.way === first));
  // Stuck: behind, and no way to upgrade it now.
  const stuck = behind && open.length === 0 && ways.length > 0;
  const build = r.commit ? shortSha(r.commit) : (r.build ?? "build not reported");
  // On its way to another build: that has a line of its own, with what happens next.
  const moving = r.connected && !r.up_to_date;
  return (
    <li className="upd-runner">
      <div className="upd-runner-top">
        <input type="checkbox" className="check" checked={selected} onChange={(e) => onSelect(e.target.checked)} aria-label={`Select ${r.name}`} />
        <StatusDot status={st.tone} title={st.text} />
        <div className="min-w-0 flex-1">
          <div className="upd-name">{r.name}</div>
          <div className="upd-meta">
            {r.hostname} · {r.os}/{r.arch} · v{r.version}
            {moving ? "" : ` · ${build}`}
          </div>
        </div>
        <span className="upd-state" data-tone={st.tone}>
          {st.text}
        </span>
      </div>
      <div className="upd-runner-body">
        {moving && (
          <p className="upd-runner-build">
            <span className="upd-build">
              {build}
              {r.how !== "left" && (
                <>
                  {" → "}
                  {newest ? shortSha(newest.commit) : (r.target_build ?? `${r.target ?? "?"} build`)}
                </>
              )}
            </span>
            <span className="upd-sub">
              {r.waits_on ? (
                <ReleaseWaitWords w={r.waits_on} newest={newest} />
              ) : r.next ? (
                sentence(r.next)
              ) : r.method === "native" ? (
                "Upgrades itself"
              ) : r.terminal ? (
                "Upgraded through a terminal on it"
              ) : (
                "Terminals are off: upgrade it on the machine"
              )}
              {r.from && r.how !== "off" ? ` · ${r.from}` : ""}
            </span>
          </p>
        )}
        <div className="upd-acts">
          {r.locked ? (
            <span className="upd-sub">Updates off on the machine</span>
          ) : (
            <SelectField label={`Updates for ${r.name}`} hideLabel value={r.policy} onChange={(v) => onPolicy(v as UpdatePolicy)} options={POLICIES} className="upd-policy" />
          )}
          {missing && (
            <Button size="xs" variant="ghost" disabled={uploading !== null} onClick={onUpload}>
              <Upload /> {uploading === r.target ? "Uploading…" : `Upload ${r.target}`}
            </Button>
          )}
          {actionable &&
            open.map((w, i) => (
              <Button key={w.way} size="xs" variant={i === 0 ? "secondary" : "ghost"} title={sentence(w.why)} onClick={() => onUpgrade(w.way)}>
                {wayLabel(w.way, i === 0, failed !== undefined && wayOf(failed) === w.way)}
              </Button>
            ))}
          {r.connected && (failed || stuck) && !plan && (
            <Button size="xs" variant="ghost" onClick={onDiagnose}>
              <Bot /> Diagnose
            </Button>
          )}
        </div>
        {actionable && open.length === 0 && ways.length > 0 && (
          <p className="upd-sub upd-ways">
            {ways.map((w) => (
              <span key={w.way}>
                {w.way === "build" ? "Can't build it there now" : "Can't download a build now"}: {w.why}
              </span>
            ))}
          </p>
        )}
        {plan}
        {r.instances.length > 1 && (
          <ul className="upd-procs">
            {r.instances.map((i) => (
              <li key={i.id}>
                <StatusDot status={i.newest ? "succeeded" : "timed_out"} />
                <span>{i.newest ? "new runner" : "old runner"}</span>
                <span className="mono upd-dim">
                  {i.build ?? (i.legacy ? "legacy" : i.id.slice(0, 8))}
                  {i.pid ? ` · pid ${i.pid}` : ""}
                </span>
                <span className="upd-dim">{i.newest ? "takes new work" : `keeps ${holding(i)}; retires once that ends`}</span>
              </li>
            ))}
          </ul>
        )}
        {job && <JobLog job={job} />}
      </div>
    </li>
  );
}

function RunnerPlan({ r, newest: target, build }: { r: RunnerUpdate; newest: FleetTarget | null; build: boolean }) {
  const newest = r.instances.find((i) => i.newest);
  const inPlace = r.method === "native" && newest !== undefined && newest.id !== "main" && !newest.legacy;
  const of = target ? ` (${shortSha(target.commit)})` : "";
  const to = r.target_build ? `build ${r.target_build}${of}` : `the ${r.target ?? ""} build (this version's GitHub release, if one is published)`;
  return (
    <ul className="upd-plan">
      {build ? (
        <>
          <li>
            {r.how === "build" ? (
              <>
                No binary of {target ? shortSha(target.commit) : "the newest kiln"} exists for {r.target ?? "its platform"} yet, so {r.name} builds it
              </>
            ) : (
              <>
                {r.name} builds {target ? shortSha(target.commit) : "the newest kiln"} itself rather than downloading it
              </>
            )}
            : kiln opens a terminal on it (it won&rsquo;t appear among your terminals) that fetches the source from this server, builds it in <code>~/.cache/kiln/self-build</code>{" "}
            and runs <code>kiln install --agent --upgrade</code>.
          </li>
          <li>
            {r.checkout ? (
              <>
                Its kiln checkout at <code>{r.checkout}</code> is left as it is.
              </>
            ) : (
              "Its kiln checkout is left as it is."
            )}{" "}
            It brings its stable Rust toolchain up to date first (kiln needs a recent one), and the build takes its CPU for a few minutes.
          </li>
          {r.os === "macos" && <li>The binary comes back to this server, so other Macs download it instead of building.</li>}
        </>
      ) : r.method === "native" ? (
        <li>
          {r.name} downloads {to} from this server, checks its SHA-256 and runs <code>kiln install --agent --upgrade</code>.
        </li>
      ) : (
        <li>
          {r.name}&rsquo;s runner predates upgrade commands, so kiln opens a terminal on it (it won&rsquo;t appear among your terminals), downloads {to} from this server,
          checks its SHA-256 and runs <code>kiln install --agent --upgrade</code> there.
        </li>
      )}
      {inPlace ? (
        <li>The runner takes the new binary in place: the same process, with its terminals and agent sessions handed across. If a job is running, it switches once the job ends.</li>
      ) : (
        <li>
          A new runner process starts beside the current one and takes all new terminals, agents and jobs. The current one keeps {newest ? holding(newest) : "what it runs"} until
          that ends, then retires.
        </li>
      )}
      <li>Nothing running on {r.name} is stopped or restarted.</li>
    </ul>
  );
}

function AllPlan({ runners }: { runners: RunnerUpdate[] }) {
  return (
    <div>
      <ul className="upd-plan">
        {runners.map((r) => (
          <li key={r.name}>
            <strong className="font-medium text-[#d6dbe2]">{r.name}</strong>:{" "}
            {r.next ?? (
              <>
                {r.target_build ? `to build ${r.target_build}` : `to the ${r.target ?? ""} build`},{" "}
                {r.method === "native" ? "by its own upgrade command" : "through a terminal on it (it predates upgrade commands)"}
              </>
            )}
          </li>
        ))}
      </ul>
      <p className="mt-3">Each gets a new runner beside the current one (or the new binary in place); nothing they run is stopped.</p>
    </div>
  );
}

/* ---------------------------- jobs ---------------------------- */

function jobTitle(j: UpdateJob) {
  if (j.kind === "server") return j.method === "restart" ? "Rollout restart" : `Install build ${j.to_build ?? ""}`;
  if (j.method === "build") return `Build ${j.commit ? shortSha(j.commit) : "the newest kiln"} and upgrade to it`;
  const to = j.to_build ? `build ${j.to_build}` : "the new build";
  return `Upgrade to ${to}${j.method === "terminal" ? " · through a terminal" : ""}`;
}

function JobLog({ job }: { job: UpdateJob }) {
  const running = job.state === "running";
  // Open while it runs; one that failed says its error, its log a tap away.
  const [open, setOpen] = React.useState(running);
  // An upgrade that starts running opens its log.
  const [runningSeen, setRunningSeen] = React.useState(running);
  if (running !== runningSeen) {
    setRunningSeen(running);
    if (running) setOpen(true);
  }
  // Its newest lines stay in view as they come.
  const follow = React.useCallback(
    (el: HTMLPreElement | null) => {
      if (el) el.scrollTop = el.scrollHeight;
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [job.log.length],
  );
  const tone = running ? "running" : job.state === "done" ? "succeeded" : "failed";
  return (
    <div className="upd-job" data-state={job.state}>
      <button type="button" className="upd-job-head" onClick={() => setOpen((o) => !o)} aria-expanded={open}>
        <StatusDot status={tone} />
        <span className="min-w-0 flex-1 truncate">{jobTitle(job)}</span>
        <span className="mono upd-dim shrink-0">
          {running ? "running" : job.state === "done" ? "done" : "failed"} · {relativeTime(job.ended_at ?? job.started_at)}
        </span>
        <ChevronDown size={14} className={cn("shrink-0 transition-transform", open && "rotate-180")} />
      </button>
      {open && (
        <pre ref={follow} className="upd-log" aria-live={running ? "polite" : undefined}>
          {job.log.join("\n") || "starting…"}
        </pre>
      )}
      {!open && job.error && <p className="upd-err">{job.error}</p>}
      {job.state === "failed" && job.timed_out && (
        <p className="upd-sub upd-job-note">
          kiln stopped waiting; the runner may still get there, and the upgrade then reads as done.{" "}
          <button type="button" className="upd-link" onClick={() => toWaits(job.timed_out)}>
            Change how long kiln waits
          </button>
        </p>
      )}
    </div>
  );
}

/** Bring the waits into view, the one that ran out focused: their section and group open first. */
function toWaits(which: UpdateJob["timed_out"]) {
  openSettings("updates", "updates.waits");
  requestAnimationFrame(() =>
    requestAnimationFrame(() => {
      const field = document.getElementById(`upd-wait-${which ?? "upgrade"}`);
      field?.scrollIntoView({ behavior: "smooth", block: "center" });
      field?.focus({ preventScroll: true });
    }),
  );
}

/* ---------------------------- diagnosing with an agent ---------------------------- */

/** The first message of an agent started on a runner to find out why it isn't upgrading, and fix that. */
function diagnosisPrompt(r: RunnerUpdate, job: UpdateJob | undefined, newest: FleetTarget | null): string {
  const failed = job?.state === "failed" ? job : undefined;
  const at = (t: number) => new Date(t * 1000).toISOString().replace(/\.\d+Z$/, "Z");
  const tail = failed ? failed.log.slice(-40) : [];
  const stuck = (r.ways ?? []).filter((w) => !w.ok);
  const lines = [
    failed
      ? `kiln couldn't upgrade its runner on ${r.name} (${r.hostname}, ${r.os}/${r.arch}). Find out why, and fix what's wrong on the machine so the next upgrade goes through.`
      : `kiln can't upgrade its runner on ${r.name} (${r.hostname}, ${r.os}/${r.arch}). Find out what's in the way, and fix it if it's on the machine.`,
    "",
    "What kiln knows:",
    `- ${r.name} runs kiln ${r.version}${r.commit ? `, commit ${r.commit}` : ""}${r.build ? `, build ${r.build}` : ""}.`,
    newest ? `- The newest kiln is commit ${newest.commit}.` : "",
    failed
      ? `- Its upgrade, ${failed.method === "build" ? `a build of ${failed.commit ? shortSha(failed.commit) : "the newest kiln"} on the machine` : `to build ${failed.to_build ?? "the new build"}`}, started ${at(failed.started_at)} and failed: ${failed.error ?? "kiln wasn't told why"}`
      : "",
    ...stuck.map((w) => `- ${w.way === "build" ? "It can't build kiln itself now" : "It can't download a build now"}: ${w.why}.`),
    r.next ? `- What kiln does next: it ${r.next}.` : "",
    tail.length ? `- Its last lines, as kiln kept them:\n\n${tail.map((l) => `    ${l}`).join("\n")}` : "",
    "",
    "Where to look:",
    `- You should be on ${r.name}. If you aren't, run commands there with kiln reach (on: "${r.name}").`,
    r.os === "linux"
      ? `- The runner's log: journalctl --user -u 'kiln-agent*'${failed ? ` --since '${at(failed.started_at).replace("T", " ").replace("Z", "")} UTC'` : ""} (without --user for a system install).`
      : r.os === "macos"
        ? "- The runner's log: kiln.log in its state directory, usually ~/.local/share/kiln/agent/kiln.log."
        : "- The runner's log, in its state directory.",
    failed?.method === "build" ? `- The build's whole output: ~/.cache/kiln/self-build/build-${failed.id}.log (the build works in ~/.cache/kiln/self-build).` : "",
    "- Free disk and memory, and for a build the Rust toolchain (rustup show, cargo --version) and npm.",
    "",
    "Rules:",
    "- Don't stop, restart or reinstall the kiln runner: everything on this machine runs through it, you included.",
    "- Don't upgrade kiln yourself. Fix the cause, then say what was wrong and what you changed; the upgrade is then tried again from Settings → Updates.",
    "- Ask before anything you can't undo, like deleting files you didn't make or changing system settings.",
  ];
  return lines
    .filter((l, i, all) => l !== "" || (i > 0 && all[i - 1] !== ""))
    .join("\n")
    .trim();
}

/* ---------------------------- how long kiln waits ---------------------------- */

const WAITS: { key: keyof UpgradeWaits; which: NonNullable<UpdateJob["timed_out"]>; label: string; hint: string; min: number; max: number }[] = [
  { key: "upgrade_minutes", which: "upgrade", label: "Upgrade", hint: "for a runner to switch to its new build and report in, once it has no jobs running", min: 1, max: 24 * 60 },
  { key: "wait_for_jobs_minutes", which: "jobs", label: "Running jobs", hint: "while a runner finishes the jobs it is running; it switches once they end", min: 1, max: 7 * 24 * 60 },
  { key: "build_minutes", which: "build", label: "Build", hint: "for a machine to build kiln itself, install included", min: 5, max: 24 * 60 },
];

/** "an hour", "6 hours", "90 minutes". */
function minutesSaid(m: number) {
  if (m === 1) return "a minute";
  if (m === 60) return "an hour";
  return m >= 120 && m % 60 === 0 ? `${m / 60} hours` : `${m} minutes`;
}

type WaitsDraft = Record<keyof UpgradeWaits, string>;

const draftOf = (w: UpgradeWaits): WaitsDraft => ({ upgrade_minutes: String(w.upgrade_minutes), wait_for_jobs_minutes: String(w.wait_for_jobs_minutes), build_minutes: String(w.build_minutes) });

/** The waits a draft says, or what's wrong with it. */
function readDraft(d: WaitsDraft): UpgradeWaits | string {
  const out: UpgradeWaits = { upgrade_minutes: 0, wait_for_jobs_minutes: 0, build_minutes: 0 };
  for (const f of WAITS) {
    const v = Number(d[f.key]);
    if (!Number.isInteger(v) || v < f.min || v > f.max) return `${f.label}: a whole number of minutes from ${f.min} to ${f.max}`;
    out[f.key] = v;
  }
  return out;
}

function WaitsSection({ waits }: { waits: NonNullable<UpdatesOverview["waits"]> }) {
  const [draft, setDraft] = React.useState<WaitsDraft | null>(null);
  const [busy, setBusy] = React.useState(false);
  const shown = draft ?? draftOf(waits.now);
  const read = readDraft(shown);
  const changed = typeof read !== "string" && WAITS.some((f) => read[f.key] !== waits.now[f.key]);
  const act = async (what: () => Promise<void>, done: string) => {
    setBusy(true);
    try {
      await what();
      setDraft(null);
      toast.success(done);
      invalidate("updates");
    } catch (e) {
      toast.error("Couldn't change how long kiln waits", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  const config = WAITS.map((f) => `${f.label.toLowerCase()} ${minutesSaid(waits.config[f.key])}`).join(", ");
  return (
    <SettingsGroup id="updates.waits" title="How long kiln waits" about={WAITS_ABOUT} words={WAITS_WORDS} collapsible>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          if (typeof read !== "string" && changed) void act(() => api.setUpgradeWaits(read), "kiln waits that long from now on");
        }}
      >
        {WAITS.map((f) => {
          const n = Number(shown[f.key]);
          const id = `upd-wait-${f.which}`;
          return (
            <SettingRow key={f.key} inline htmlFor={id} label={f.label} help={`${Number.isInteger(n) && n >= f.min ? `${sentence(minutesSaid(n))} ` : ""}${f.hint}`}>
              <div className="field st-num">
                <input
                  id={id}
                  type="number"
                  inputMode="numeric"
                  min={f.min}
                  max={f.max}
                  step={1}
                  value={shown[f.key]}
                  onChange={(e) => setDraft({ ...shown, [f.key]: e.target.value })}
                />
                <span aria-hidden="true">min</span>
              </div>
            </SettingRow>
          );
        })}
        <p className="st-note">
          An upgrade that takes longer reads as failed, and as done if the runner gets there later. A change applies to upgrades under way too.{" "}
          {waits.changed ? `Changed here; kiln.toml says ${config}.` : "As kiln.toml's [updates] says."}
        </p>
        <div className="st-form-acts upd-waits-acts">
          <Button type="submit" size="xs" variant="secondary" disabled={busy || !changed}>
            Save
          </Button>
          {draft && (
            <Button type="button" size="xs" variant="ghost" disabled={busy} onClick={() => setDraft(null)}>
              Cancel
            </Button>
          )}
          {waits.changed && !draft && (
            <Button type="button" size="xs" variant="ghost" disabled={busy} onClick={() => void act(() => api.resetUpgradeWaits(), "kiln waits as kiln.toml says")}>
              Use kiln.toml&rsquo;s
            </Button>
          )}
          {draft && typeof read === "string" && <span className="upd-err-inline">{read}</span>}
        </div>
      </form>
    </SettingsGroup>
  );
}

/* ---------------------------- binaries ---------------------------- */

function Binaries({
  data,
  uploading,
  onUpload,
  onRemoved,
  onBuilt,
}: {
  data: UpdatesOverview;
  uploading: string | null;
  onUpload: (t: string) => void;
  onRemoved: () => void;
  onBuilt: () => void;
}) {
  const [target, setTarget] = React.useState(data.targets[0] ?? "");
  const [confirm, setConfirm] = React.useState<KilnBinary | null>(null);
  const origin = (b: KilnBinary) =>
    b.origin === "server"
      ? "this server's own"
      : b.origin === "upload"
        ? `uploaded ${relativeTime(b.at)}`
        : b.origin === "built"
          ? `built by run #${b.run ?? "?"} from ${(b.sha ?? "").slice(0, 7)}, ${relativeTime(b.at)}`
          : b.origin === "machine"
            ? `built by ${b.by ?? "a machine"} from ${(b.sha ?? "").slice(0, 7)}, ${relativeTime(b.at)}`
            : `GitHub release, fetched ${relativeTime(b.at)}`;
  // Not built from the newest kiln: its machines are behind until one is.
  const behind = (b: KilnBinary) => b.origin !== "server" && (data.build?.behind ?? []).includes(b.target);
  return (
    <SettingsGroup id="updates.binaries" title="Binaries" about={BINARIES_ABOUT} words={BINARIES_WORDS} collapsible>
      <p className="st-note upd-bins-lead">Machines of another platform get the build kiln made of every platform, or an uploaded one.</p>
      {data.build && <BuildEvery build={data.build} onStarted={onBuilt} />}
      <ul className="st-list upd-bins">
        {data.binaries.map((b) => (
          <li key={`${b.target}-${b.origin}`}>
            <div className="st-item">
              <span className="st-item-name upd-bin-target">{b.target}</span>
              <span className="st-item-meta mono">
                {b.build} · {formatBytes(b.size)}
              </span>
              {b.origin !== "server" && confirm?.target !== b.target && (
                <span className="st-item-acts">
                  <Button size="xs" variant="ghost" aria-label={`Remove the ${b.target} binary`} onClick={() => setConfirm(b)}>
                    <Trash2 />
                  </Button>
                </span>
              )}
            </div>
            <p className="st-note">
              {sentence(origin(b))}
              {behind(b) && <span className="upd-behind"> · behind the newest kiln</span>}
            </p>
            {confirm?.target === b.target && (
              <Confirm
                className="upd-confirm"
                label={`Remove the ${b.target} binary`}
                tone="danger"
                actions={
                  <>
                    <Button
                      size="xs"
                      variant="danger"
                      onClick={async () => {
                        try {
                          await api.deleteBinary(b.target);
                          onRemoved();
                        } catch (e) {
                          toast.error("Couldn't remove it", errorMessage(e));
                        } finally {
                          setConfirm(null);
                        }
                      }}
                    >
                      Remove
                    </Button>
                    <Button size="xs" variant="ghost" onClick={() => setConfirm(null)}>
                      Keep it
                    </Button>
                  </>
                }
              >
                <p>
                  Remove the {b.target} binary? Build {b.build} is no longer handed to machines of that platform until kiln builds every platform again or one is uploaded.
                </p>
              </Confirm>
            )}
          </li>
        ))}
      </ul>
      <div className="upd-upload">
        <SelectField label="Platform" hideLabel value={target} onChange={setTarget} options={data.targets.map((t) => ({ value: t, label: t }))} className="upd-platform" />
        <Button size="xs" variant="secondary" disabled={uploading !== null || !target} onClick={() => onUpload(target)}>
          <Upload /> {uploading ? "Uploading…" : "Upload binary"}
        </Button>
      </div>
    </SettingsGroup>
  );
}

/** Building kiln for every platform with its own release pipeline, and how the last build went. */
function BuildEvery({ build, onStarted }: { build: ReleaseBuild; onStarted: () => void }) {
  const [busy, setBusy] = React.useState(false);
  const last = build.last ?? null;
  const building = last !== null && (last.status === "queued" || last.status === "running" || last.status === "pending");
  const start = async () => {
    setBusy(true);
    try {
      const run = await api.buildEveryPlatform();
      toast({ title: "Building every platform", description: `${build.repo} ${build.pipeline} run #${run.number}`, tone: "running" });
      onStarted();
    } catch (e) {
      toast.error("Couldn't start the build", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  const runLink = last ? <Link to={`/runs/${last.id}`}>run #{last.number}</Link> : null;
  // The newest kiln's commit, when kiln knows it: every platform should be built from it.
  const commit = build.commit ? build.commit.slice(0, 7) : null;
  const behind = build.behind ?? [];
  const lastOfIt = last !== null && build.commit != null && last.sha.toLowerCase() === build.commit.toLowerCase();
  const tone = build.error
    ? "timed_out"
    : building
      ? "running"
      : build.in_step
        ? "succeeded"
        : commit
          ? lastOfIt && last?.status === "failed"
            ? "failed"
            : "queued"
          : last?.status === "failed"
            ? "failed"
            : last
              ? "succeeded"
              : "queued";
  return (
    <div className="upd-build-every">
      <p className="upd-note" data-tone={tone}>
        <StatusDot status={tone} />
        <span>
          {build.error ??
            (building ? (
              <>
                Building every platform from {last?.sha.slice(0, 7)}
                {lastOfIt ? ", the newest kiln" : ""} · {runLink}
              </>
            ) : build.in_step ? (
              <>
                Every platform is built from {commit}, the newest kiln{runLink ? " · " : ""}
                {runLink}
              </>
            ) : commit ? (
              <span title={behind.join(", ")}>
                {behind.length} platform{behind.length === 1 ? " isn't" : "s aren't"} built from {commit}, the newest kiln
                {lastOfIt && last?.status === "failed" ? (
                  <>
                    : the build failed · {runLink}
                    {build.keeps_in_step ? ". kiln tries again in a while" : ""}
                  </>
                ) : build.keeps_in_step ? (
                  ": kiln builds them on its own"
                ) : (
                  ""
                )}
              </span>
            ) : last?.status === "succeeded" ? (
              <>
                Built every platform {relativeTime(last.finished_at ?? last.created_at)} from {last.branch} {last.sha.slice(0, 7)} · {runLink}. The server doesn&rsquo;t know which commit it runs
                (built without KILN_COMMIT), so kiln can&rsquo;t keep them in step with it.
              </>
            ) : last ? (
              <>The last build {last.status === "failed" ? "failed" : "didn't finish"} · {runLink}</>
            ) : (
              <>kiln hasn't built every platform here yet: {build.repo}&rsquo;s {build.pipeline} pipeline does, on the server&rsquo;s own runner.</>
            ))}
        </span>
      </p>
      <Button size="xs" variant="secondary" disabled={busy || building || Boolean(build.error)} onClick={() => void start()}>
        {building ? "Building…" : "Build every platform"}
      </Button>
    </div>
  );
}

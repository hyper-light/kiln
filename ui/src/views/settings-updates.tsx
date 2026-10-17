/**
 * Settings → Updates: which build the server runs and how it is upgraded, each machine's runner
 * and its processes, Upgrade per runner and for all of them with live output, and the binaries
 * kiln hands out. Runners are upgraded without a restart: the old runner keeps its terminals,
 * agents and jobs. Every action is confirmed first and says what will happen.
 */
import * as React from "react";
import { ChevronDown, RefreshCw, Trash2, Upload } from "lucide-react";
import { api } from "@/api/client";
import { useConnection, useServerEvents } from "@/api/events";
import type { KilnBinary, RunnerInstance, RunnerUpdate, ServerUpdate, UpdateJob, UpdatesOverview } from "@/api/types";
import { Button } from "@/components/ui/button";
import { ConfirmDialog, Dialog, DialogClose, DialogContent } from "@/components/ui/dialog";
import { Facts, Panel } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { toast } from "@/components/ui/toast";
import { formatBytes, relativeTime } from "@/lib/format";
import { errorMessage, useNow, useResource } from "@/lib/hooks";
import { cn } from "@/lib/utils";

/** A server upgrade in flight: the page reconnects to the new server and says what changed. */
const PENDING_KEY = "kiln.serverUpgrade";
/** Finished upgrades stay shown under their runner this long. */
const RECENT_S = 3600;

interface Confirm {
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

/** The fuller of two snapshots of one job (events and refetches can cross). */
function fuller(a: UpdateJob | undefined, b: UpdateJob): UpdateJob {
  if (!a) return b;
  const rank = (j: UpdateJob) => (j.state === "running" ? 0 : 1) * 1e6 + j.log.length;
  return rank(b) >= rank(a) ? b : a;
}

function readPending(): { from: string; at: number } | null {
  try {
    const v = sessionStorage.getItem(PENDING_KEY);
    return v ? (JSON.parse(v) as { from: string; at: number }) : null;
  } catch {
    return null;
  }
}

function writePending(v: { from: string; at: number } | null) {
  try {
    if (v) sessionStorage.setItem(PENDING_KEY, JSON.stringify(v));
    else sessionStorage.removeItem(PENDING_KEY);
  } catch {
    /* private mode */
  }
}

export function UpdatesPanel() {
  const res = useResource((s) => api.updates(s), []);
  const conn = useConnection();
  const [live, setLive] = React.useState<Record<string, UpdateJob>>({});
  const [confirm, setConfirm] = React.useState<Confirm | null>(null);
  const [checking, setChecking] = React.useState(false);
  const [pending, setPending] = React.useState(readPending);
  const fileRef = React.useRef<HTMLInputElement>(null);
  const uploadTarget = React.useRef<string>("");
  const [uploading, setUploading] = React.useState<string | null>(null);

  // Refetch on changes, a little coalesced (runner events come in bursts).
  const timer = React.useRef<number | null>(null);
  const reload = res.reload;
  const reloadSoon = React.useCallback(() => {
    if (timer.current !== null) window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => {
      timer.current = null;
      reload();
    }, 350);
  }, [reload]);
  React.useEffect(() => () => {
    if (timer.current !== null) window.clearTimeout(timer.current);
  }, []);

  useServerEvents((ev) => {
    if (ev.type === "update_job") setLive((prev) => ({ ...prev, [ev.job.id]: fuller(prev[ev.job.id], ev.job) }));
    else if (ev.type === "updates" || ev.type === "agent") reloadSoon();
  });

  // Back from a restart: refetch, and say what the server runs now.
  const wasClosed = React.useRef(false);
  React.useEffect(() => {
    if (conn.state === "closed") wasClosed.current = true;
    if (conn.state === "open" && wasClosed.current) {
      wasClosed.current = false;
      reload();
    }
  }, [conn.state, reload]);
  const data = res.data;
  React.useEffect(() => {
    if (!pending || !data) return;
    if (data.server.build !== pending.from) {
      toast.success("Server upgraded", `build ${pending.from} → ${data.server.build}`);
      writePending(null);
      setPending(null);
    } else if (Date.now() - pending.at > 4 * 60_000) {
      toast({ title: "The server is still on the same build", description: `build ${data.server.build}`, tone: "info" });
      writePending(null);
      setPending(null);
    }
  }, [data, pending]);

  const jobs = React.useMemo(() => {
    const all: Record<string, UpdateJob> = { ...live };
    for (const j of data?.jobs ?? []) all[j.id] = fuller(all[j.id], j);
    return Object.values(all).sort((a, b) => b.started_at - a.started_at);
  }, [data, live]);
  const nowS = useNow() / 1000;
  const latest = (kind: UpdateJob["kind"], target: string) =>
    jobs.find((j) => j.kind === kind && j.target === target && (j.state === "running" || nowS - (j.ended_at ?? j.started_at) < RECENT_S));

  const check = async () => {
    setChecking(true);
    try {
      await api.checkUpdates();
      reload();
    } catch (e) {
      toast.error("Couldn't check for updates", errorMessage(e));
    } finally {
      setChecking(false);
    }
  };

  const startRunner = async (r: RunnerUpdate) => {
    try {
      const job = await api.upgradeRunner(r.name);
      setLive((prev) => ({ ...prev, [job.id]: fuller(prev[job.id], job) }));
      toast({ title: `Upgrading ${r.name}`, description: "Its terminals, agents and jobs keep running.", tone: "running" });
    } catch (e) {
      toast.error(`Couldn't upgrade ${r.name}`, errorMessage(e));
    }
    reloadSoon();
  };

  const startAll = async () => {
    try {
      const out = await api.upgradeAllRunners();
      setLive((prev) => {
        const next = { ...prev };
        for (const j of out.jobs) next[j.id] = fuller(next[j.id], j);
        return next;
      });
      if (out.jobs.length) toast({ title: `Upgrading ${plural(out.jobs.length, "runner")}`, description: "Nothing they run is stopped.", tone: "running" });
      for (const e of out.errors) toast.error("A runner wasn't upgraded", e);
    } catch (e) {
      toast.error("Couldn't upgrade the runners", errorMessage(e));
    }
    reloadSoon();
  };

  const startServer = async (s: ServerUpdate, action: "restart" | "replace") => {
    try {
      const job = await api.upgradeServer(action);
      setLive((prev) => ({ ...prev, [job.id]: fuller(prev[job.id], job) }));
      if (job.state === "failed") {
        toast.error("The server wasn't upgraded", job.error ?? undefined);
        return;
      }
      const p = { from: s.build, at: Date.now() };
      writePending(p);
      setPending(p);
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
      reload();
    } catch (e) {
      toast.error("Upload failed", errorMessage(e));
    } finally {
      setUploading(null);
    }
  };

  const outdated = (data?.runners ?? []).filter((r) => r.connected && !r.up_to_date);
  const runningRunners = new Set(jobs.filter((j) => j.kind === "runner" && j.state === "running").map((j) => j.target));

  return (
    <Panel
      eyebrow="Maintenance"
      title="Updates"
      description="Upgrade kiln and its runners from here. A runner keeps every terminal, agent and job it runs while it is upgraded."
      actions={
        <Button variant="ghost" size="sm" disabled={checking || !data} onClick={() => void check()} aria-label="Check for updates">
          <RefreshCw className={cn((checking || data?.server.checking) && "animate-spin motion-reduce:animate-none")} /> <span className="max-sm:sr-only">Check</span>
        </Button>
      }
    >
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
      {res.error && !data && <p className="upd-note" data-tone="error">{errorMessage(res.error)}</p>}
      {!data && !res.error && <p className="upd-note">Loading…</p>}
      {data && (
        <div className="upd">
          <ServerSection
            s={data.server}
            job={latest("server", "server")}
            pending={pending !== null && data.server.build === pending.from}
            uploading={uploading}
            onUpload={() => pickUpload(data.server.target)}
            onAction={(action) =>
              setConfirm({
                title: action === "restart" ? "Restart the server onto the new image?" : "Install the new server build?",
                lead: "What will happen:",
                plan: <ServerPlan s={data.server} action={action} />,
                confirmLabel: action === "restart" ? "Restart server" : "Install and restart",
                run: () => startServer(data.server, action),
              })
            }
          />
          <section className="upd-section" aria-labelledby="upd-runners">
            <div className="upd-section-head">
              <div className="min-w-0">
                <h4 id="upd-runners" className="upd-h">
                  Runners
                </h4>
                <p className="upd-sub">
                  {data.runners.length === 0
                    ? "No machines have joined yet."
                    : `${data.runners.filter((r) => r.up_to_date).length} of ${data.runners.length} on the server's build · runners are brought to the build this server runs`}
                </p>
              </div>
              <Button
                variant="secondary"
                size="sm"
                disabled={outdated.length === 0 || outdated.every((r) => runningRunners.has(r.name))}
                onClick={() =>
                  setConfirm({
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
            </div>
            <ul className="upd-runners">
              {data.runners.map((r) => (
                <RunnerRow
                  key={r.name}
                  r={r}
                  job={latest("runner", r.name)}
                  uploading={uploading}
                  onUpload={() => r.target && pickUpload(r.target)}
                  onUpgrade={() =>
                    setConfirm({
                      title: `Upgrade ${r.name}?`,
                      lead: "What will happen:",
                      plan: <RunnerPlan r={r} />,
                      confirmLabel: "Upgrade",
                      run: () => startRunner(r),
                    })
                  }
                />
              ))}
            </ul>
          </section>
          <Binaries data={data} uploading={uploading} onUpload={pickUpload} onRemoved={reload} />
        </div>
      )}
      <PlanDialog confirm={confirm} onClose={() => setConfirm(null)} />
    </Panel>
  );
}

/** Confirm an upgrade, showing step by step what it will do. */
function PlanDialog({ confirm, onClose }: { confirm: Confirm | null; onClose: () => void }) {
  const [busy, setBusy] = React.useState(false);
  // Keep the content while the dialog animates closed.
  const [shown, setShown] = React.useState(confirm);
  React.useEffect(() => {
    if (confirm) setShown(confirm);
  }, [confirm]);
  return (
    <Dialog open={confirm !== null} onOpenChange={(o) => !o && !busy && onClose()}>
      {shown && (
        <DialogContent title={shown.title} eyebrow="Confirm" description={shown.lead}>
          {shown.plan}
          <div className="flex flex-wrap justify-end gap-2 pt-5">
            <DialogClose asChild>
              <Button variant="ghost" disabled={busy}>
                Cancel
              </Button>
            </DialogClose>
            <Button
              disabled={busy}
              onClick={async () => {
                setBusy(true);
                try {
                  await shown.run();
                } finally {
                  setBusy(false);
                  onClose();
                }
              }}
            >
              {shown.confirmLabel}
            </Button>
          </div>
        </DialogContent>
      )}
    </Dialog>
  );
}

/* ---------------------------- the server ---------------------------- */

function ServerSection({
  s,
  job,
  pending,
  uploading,
  onUpload,
  onAction,
}: {
  s: ServerUpdate;
  job: UpdateJob | undefined;
  pending: boolean;
  uploading: string | null;
  onUpload: () => void;
  onAction: (a: "restart" | "replace") => void;
}) {
  const runsAs = s.mode === "kubernetes" ? `Deployment ${s.namespace}/${s.deployment}` : s.mode === "service" ? "service (kiln supervise)" : "standalone process";
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
    <section className="upd-section" aria-labelledby="upd-server">
      <div className="upd-section-head">
        <div className="min-w-0">
          <h4 id="upd-server" className="upd-h">
            Server
          </h4>
          <p className="upd-sub">kiln {s.version}</p>
        </div>
        <div className="upd-actions">
          {s.action === "restart" && (s.newer || !!s.image?.error) && (
            <Button size="sm" variant={s.newer ? "primary" : "secondary"} disabled={busy} onClick={() => onAction("restart")}>
              {s.newer ? "Upgrade server" : "Restart anyway"}
            </Button>
          )}
          {s.action === "replace" && s.replacement && (
            <Button size="sm" disabled={busy} onClick={() => onAction("replace")}>
              {s.replacement.origin === "upload" ? `Install build ${s.replacement.build ?? ""}` : `Install kiln ${s.replacement.version ?? ""}`}
            </Button>
          )}
          {s.mode !== "kubernetes" && (
            <Button size="sm" variant="secondary" disabled={uploading !== null} onClick={onUpload}>
              <Upload /> {uploading === s.target ? "Uploading…" : "Upload binary"}
            </Button>
          )}
        </div>
      </div>
      <Facts
        className="upd-facts"
        items={[
          { label: "Build", value: s.build },
          { label: "Platform", value: s.target },
          { label: "Runs as", value: runsAs },
          s.mode === "kubernetes"
            ? { label: "Image on node", value: s.image?.build ? `${s.image.build}${s.image.newer ? " · newer" : " · same"}` : s.image?.error ? "unknown" : "checking…", tone: s.image?.newer ? "pending" : undefined }
            : { label: "Latest release", value: release, tone: s.release.newer ? "pending" : undefined },
        ]}
      />
      <p className="upd-note" data-tone={tone}>
        <StatusDot status={tone} title={s.newer ? "A newer build is ready" : "Status"} />
        <span>{pending ? "Restarting… this page reconnects when the server is back." : s.note}</span>
      </p>
      {job && <JobLog job={job} />}
    </section>
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

function runnerState(r: RunnerUpdate, job: UpdateJob | undefined): { tone: string; text: string } {
  if (job?.state === "running") return { tone: "running", text: "upgrading" };
  if (!r.connected) return { tone: "cancelled", text: "offline" };
  if (r.up_to_date) return { tone: "succeeded", text: "up to date" };
  return { tone: "queued", text: r.build ? "upgrade available" : "older runner" };
}

function RunnerRow({
  r,
  job,
  uploading,
  onUpgrade,
  onUpload,
}: {
  r: RunnerUpdate;
  job: UpdateJob | undefined;
  uploading: string | null;
  onUpgrade: () => void;
  onUpload: () => void;
}) {
  const st = runnerState(r, job);
  const missing = Boolean(r.connected && !r.up_to_date && !r.target_build && r.target);
  return (
    <li className="upd-runner">
      <div className="upd-runner-top">
        <StatusDot status={st.tone} title={st.text} />
        <div className="min-w-0 flex-1">
          <div className="upd-name">{r.name}</div>
          <div className="upd-sub mono">
            {r.hostname} · {r.os}/{r.arch} · v{r.version}
          </div>
        </div>
        <span className="upd-state" data-tone={st.tone}>
          {st.text}
        </span>
      </div>
      <div className="upd-runner-detail">
        <span className="mono upd-build">
          {r.build ?? "build not reported"}
          {!r.up_to_date && r.connected && (
            <>
              {" → "}
              {r.target_build ?? `${r.target ?? "?"} build`}
            </>
          )}
        </span>
        {r.connected && !r.up_to_date && (
          <span className="upd-sub">
            {r.method === "native" ? "upgrades itself" : r.terminal ? "upgraded through a terminal on it" : "terminals are off: upgrade it on the machine"}
          </span>
        )}
        <div className="upd-actions">
          {missing && (
            <Button size="xs" variant="ghost" disabled={uploading !== null} onClick={onUpload}>
              <Upload /> {uploading === r.target ? "Uploading…" : `Upload ${r.target}`}
            </Button>
          )}
          {r.connected && !r.up_to_date && (
            <Button size="xs" variant="secondary" disabled={job?.state === "running" || (r.method === "terminal" && !r.terminal)} onClick={onUpgrade}>
              Upgrade
            </Button>
          )}
        </div>
      </div>
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
    </li>
  );
}

function RunnerPlan({ r }: { r: RunnerUpdate }) {
  const newest = r.instances.find((i) => i.newest);
  const inPlace = r.method === "native" && newest !== undefined && newest.id !== "main" && !newest.legacy;
  const to = r.target_build ? `build ${r.target_build}` : `the ${r.target ?? ""} build (this version's GitHub release, if one is published)`;
  return (
    <ul className="upd-plan">
      {r.method === "native" ? (
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
            <strong className="font-medium text-[#d6dbe2]">{r.name}</strong>: {r.target_build ? `to build ${r.target_build}` : `to the ${r.target ?? ""} build`},{" "}
            {r.method === "native" ? "by its own upgrade command" : "through a terminal on it (it predates upgrade commands)"}
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
  const to = j.to_build ? `build ${j.to_build}` : "the new build";
  return `Upgrade to ${to}${j.method === "terminal" ? " · through a terminal" : ""}`;
}

function JobLog({ job }: { job: UpdateJob }) {
  const running = job.state === "running";
  const [open, setOpen] = React.useState(running || job.state === "failed");
  React.useEffect(() => {
    if (running) setOpen(true);
  }, [running]);
  const ref = React.useRef<HTMLPreElement>(null);
  React.useEffect(() => {
    const el = ref.current;
    if (el && open) el.scrollTop = el.scrollHeight;
  }, [job.log.length, open]);
  const tone = running ? "running" : job.state === "done" ? "succeeded" : "failed";
  return (
    <div className="upd-job" data-state={job.state}>
      <button type="button" className="upd-job-head" onClick={() => setOpen((o) => !o)} aria-expanded={open}>
        <StatusDot status={tone} />
        <span className="min-w-0 flex-1 truncate">{jobTitle(job)}</span>
        <span className="mono upd-dim shrink-0">{running ? "running" : job.state === "done" ? "done" : "failed"} · {relativeTime(job.ended_at ?? job.started_at)}</span>
        <ChevronDown size={14} className={cn("shrink-0 transition-transform", open && "rotate-180")} />
      </button>
      {open && (
        <pre ref={ref} className="upd-log" aria-live={running ? "polite" : undefined}>
          {job.log.join("\n") || "starting…"}
        </pre>
      )}
      {!open && job.error && <p className="upd-err">{job.error}</p>}
    </div>
  );
}

/* ---------------------------- binaries ---------------------------- */

function Binaries({ data, uploading, onUpload, onRemoved }: { data: UpdatesOverview; uploading: string | null; onUpload: (t: string) => void; onRemoved: () => void }) {
  const [target, setTarget] = React.useState(data.targets[0] ?? "");
  const [confirm, setConfirm] = React.useState<KilnBinary | null>(null);
  const origin = (b: KilnBinary) => (b.origin === "server" ? "this server's own" : b.origin === "upload" ? `uploaded ${relativeTime(b.at)}` : `GitHub release, fetched ${relativeTime(b.at)}`);
  return (
    <section className="upd-section" aria-labelledby="upd-binaries">
      <div className="upd-section-head">
        <div className="min-w-0">
          <h4 id="upd-binaries" className="upd-h">
            Binaries
          </h4>
          <p className="upd-sub">What runners download. Machines of another platform get an uploaded build, else this version&rsquo;s GitHub release.</p>
        </div>
      </div>
      <ul className="upd-bins">
        {data.binaries.map((b) => (
          <li key={`${b.target}-${b.origin}`}>
            <span className="mono upd-bin-target">{b.target}</span>
            <span className="mono upd-dim">
              {b.build} · {formatBytes(b.size)}
            </span>
            <span className="upd-dim">{origin(b)}</span>
            {b.origin !== "server" && (
              <Button size="xs" variant="ghost" aria-label={`Remove the ${b.target} binary`} onClick={() => setConfirm(b)}>
                <Trash2 />
              </Button>
            )}
          </li>
        ))}
      </ul>
      <div className="upd-upload">
        <label className="upd-select">
          <span className="sr-only">Platform</span>
          <select value={target} onChange={(e) => setTarget(e.target.value)}>
            {data.targets.map((t) => (
              <option key={t} value={t}>
                {t}
              </option>
            ))}
          </select>
          <ChevronDown size={13} aria-hidden="true" />
        </label>
        <Button size="sm" variant="secondary" disabled={uploading !== null || !target} onClick={() => onUpload(target)}>
          <Upload /> {uploading ? "Uploading…" : "Upload binary"}
        </Button>
      </div>
      <ConfirmDialog
        open={confirm !== null}
        onOpenChange={(o) => !o && setConfirm(null)}
        title={`Remove the ${confirm?.target ?? ""} binary?`}
        description={`Build ${confirm?.build ?? ""} is no longer handed to machines of that platform; an upgrade then fetches this version's GitHub release, if one is published.`}
        confirmLabel="Remove"
        danger
        onConfirm={async () => {
          if (!confirm) return;
          try {
            await api.deleteBinary(confirm.target);
            onRemoved();
          } catch (e) {
            toast.error("Couldn't remove it", errorMessage(e));
          }
        }}
      />
    </section>
  );
}

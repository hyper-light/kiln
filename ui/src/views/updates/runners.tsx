/**
 * System → Updates' table of machines: the server and every runner, the build each runs, its
 * state, its update policy and what can be done about it, and under each row what it needs said:
 * the plan of an upgrade to confirm, its live output, a diagnosis, the release run it waits for.
 * Runners are upgraded without a restart: the old runner keeps its terminals, agents and jobs.
 * Every action is confirmed in place first and says what will happen. Under the table, how long
 * kiln waits on an upgrade before it calls it failed.
 */
import * as React from "react";
import { Bot, ChevronDown, CircleArrowUp, Hammer, RefreshCw, RotateCcw, Stethoscope, Terminal, Upload } from "lucide-react";
import { attachApi, type IncidentBrief } from "@/api/attach";
import { api } from "@/api/client";
import type { FleetTarget, ReleaseWait, RunnerDiagnosis, RunnerFinding, RunnerUpdate, ServerUpdate, UpdateJob, UpdatePolicy, UpdatesOverview, UpgradeWaits, UpgradeWay } from "@/api/types";
import { Button } from "@/components/ui/button";
import { Confirm } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { toast } from "@/components/ui/toast";
import { useAction, useActions, useHold } from "@/lib/action";
import { fuller, labelOf, useLabels, useUpdateJobs } from "@/lib/data";
import { formatDuration, shortSha } from "@/lib/format";
import { errorMessage, useNow } from "@/lib/hooks";
import { invalidate, useSuspenseResource, type SuspenseResource } from "@/lib/resource";
import { Link, useNavigate } from "@/lib/router";
import { cn } from "@/lib/utils";
import { startFrom } from "@/views/harness/new-agent-draft";
import { PATIENCE_MS, readAgainSoon, setPending, settlePending, useUploadPicker, useUploading } from "./state";
import { UpgradeLog } from "./upgrade-log";
import {
  POLICIES,
  WAITS,
  diagnosisPrompt,
  draftOf,
  holding,
  minutesSaid,
  platformOf,
  plural,
  readDraft,
  sentence,
  upgradeOffer,
  wayLabel,
  wayOf,
  type Fleet,
  type RunnerEntry,
  type ServerView,
  type Tone,
  type WaitsDraft,
} from "./words";

interface Confirming {
  /** Where it is asked: "server", "all", or "runner:<name>". */
  scope: string;
  title: string;
  /** One sentence; the plan below says step by step what will happen. */
  lead: string;
  plan: React.ReactNode;
  confirmLabel: string;
  run: () => Promise<void>;
  /** The other ways it can be done, each its own plan. */
  instead?: { label: string; ask: () => void }[];
}

type Which = NonNullable<UpdateJob["timed_out"]>;

export interface MachinesProps {
  resource: SuspenseResource<UpdatesOverview>;
  fleet: Fleet;
  server: ServerView;
  serverJob: UpdateJob | undefined;
  /** The server restarts onto the upgrade this tab started. */
  serverPending: boolean;
  summary: string;
  picked: string | null;
}

export function MachinesPanel({ resource, fleet, server, serverJob, serverPending, summary, picked }: MachinesProps) {
  useLabels();
  const data = resource.data;
  const newest = data.newest ?? null;
  const [confirm, setConfirm] = React.useState<Confirming | null>(null);
  // Runners chosen for one update policy at once, and that policy.
  const [selected, setSelected] = React.useState<Set<string>>(() => new Set());
  const [bulk, setBulk] = React.useState<UpdatePolicy>("notify");
  const [policyPending, setPolicyPending] = React.useState<Record<string, UpdatePolicy>>({});
  const [policyErrors, setPolicyErrors] = React.useState<Record<string, string>>({});
  const [bulkBusy, setBulkBusy] = React.useState(false);
  const policyRequests = React.useRef(new Set<string>());
  // The machines whose upgrade output is open: "server" and runners' names.
  const [outputs, setOutputs] = React.useState<Set<string>>(() => new Set());
  // How long kiln waits, being changed: the field to start in.
  const [waitsEditing, setWaitsEditing] = React.useState<{ focus: Which } | null>(null);
  // The runner an agent is being started for, to find out why its upgrade failed.
  const navigate = useNavigate();
  const uploads = useUploadPicker();
  const uploading = useUploading((s) => s.targets);

  const selectable = data.runners.filter((r) => !r.locked);
  const selectedNames = selectable.filter((r) => selected.has(r.name)).map((r) => r.name);
  const select = (name: string, on: boolean) =>
    setSelected((cur) => {
      const next = new Set(cur);
      if (on) next.add(name);
      else next.delete(name);
      return next;
    });
  const toggleOutput = (key: string, open?: boolean) =>
    setOutputs((cur) => {
      const next = new Set(cur);
      if (open ?? !next.has(key)) next.add(key);
      else next.delete(key);
      return next;
    });

  /** A job just started here: shown at once, before its first event. */
  const started = (job: UpdateJob) => {
    useUpdateJobs.setState((m) => ({ ...m, [job.id]: fuller(m[job.id], job) }), true);
  };

  const [check, checking] = useAction(async () => {
    await api.checkUpdates();
    invalidate("updates");
  }, "Couldn't check for updates");

  const startRunner = async (r: RunnerUpdate, way?: UpgradeWay) => {
    try {
      const job = await api.upgradeRunner(r.name, way);
      started(job);
      if (job.state === "failed") throw new Error(job.error ?? "The upgrade couldn't start.");
      toggleOutput(r.name, true);
      toast({
        title: way === "build" ? (job.on ? `Building kiln for ${labelOf(r.name)} on ${labelOf(job.on)}` : `Building kiln on ${labelOf(r.name)}`) : `Upgrading ${labelOf(r.name)}`,
        description: "Its terminals, agents and jobs keep running.",
        tone: "running",
      });
    } catch (e) {
      toast.error(`Couldn't upgrade ${labelOf(r.name)}`, errorMessage(e));
      throw e;
    } finally {
      readAgainSoon();
    }
  };

  const setPolicy = async (names: string[], policy: UpdatePolicy, isBulk = false) => {
    if (!names.length || names.some((name) => policyRequests.current.has(name))) return;
    for (const name of names) policyRequests.current.add(name);
    const scope = isBulk ? "bulk" : `runner:${names[0]}`;
    setPolicyErrors((prev) => ({ ...prev, [scope]: "" }));
    setPolicyPending((prev) => ({ ...prev, ...Object.fromEntries(names.map((name) => [name, policy])) }));
    if (isBulk) setBulkBusy(true);
    try {
      const [one] = names;
      if (names.length === 1 && one) await api.setRunnerPolicy(one, policy);
      else await api.setPolicies(policy, names);
      resource.setData((prev) => ({ ...prev, runners: prev.runners.map((r) => (names.includes(r.name) ? { ...r, policy } : r)) }));
      setPolicyErrors((prev) => Object.fromEntries(Object.entries(prev).filter(([key]) => key !== scope && !names.some((name) => key === `runner:${name}`))));
      if (isBulk) setSelected((prev) => new Set([...prev].filter((name) => !names.includes(name))));
      toast.success("Update policy saved", `${names.length === 1 ? labelOf(one ?? "") : plural(names.length, "runner")}: ${POLICIES.find((p) => p.value === policy)?.label}`);
    } catch (e) {
      setPolicyErrors((prev) => ({ ...prev, [scope]: `Couldn't save the update policy. ${errorMessage(e)}` }));
    } finally {
      for (const name of names) policyRequests.current.delete(name);
      setPolicyPending((prev) => Object.fromEntries(Object.entries(prev).filter(([name]) => !names.includes(name))));
      if (isBulk) setBulkBusy(false);
      invalidate("updates");
    }
  };

  const startAll = async () => {
    try {
      const out = await api.upgradeAllRunners();
      for (const j of out.jobs) started(j);
      setOutputs((cur) => new Set([...cur, ...out.jobs.map((j) => j.target)]));
      if (out.jobs.length) toast({ title: `Upgrading ${plural(out.jobs.length, "runner")}`, description: "Nothing they run is stopped.", tone: "running" });
      for (const e of out.errors) toast.error("A runner wasn't upgraded", e);
    } catch (e) {
      toast.error("Couldn't upgrade the runners", errorMessage(e));
      throw e;
    } finally {
      readAgainSoon();
    }
  };

  const startServer = async (s: ServerUpdate, action: "restart" | "replace") => {
    try {
      const job = await api.upgradeServer(action);
      started(job);
      if (job.state === "failed") {
        throw new Error(job.error ?? "The server couldn't be upgraded.");
      }
      toggleOutput("server", true);
      setPending({ from: s.build, at: Date.now() });
      // A restart that never comes still gets an answer.
      window.setTimeout(() => void settlePending(), PATIENCE_MS + 1_000);
    } catch (e) {
      toast.error("Couldn't upgrade the server", errorMessage(e));
      throw e;
    }
  };

  const askRunner = (e: RunnerEntry, way: UpgradeWay) => {
    const { r, job } = e;
    const offer = upgradeOffer(r, job);
    const again = job?.state === "failed";
    const build = way === "build";
    const failedWay = offer.failed ? wayOf(offer.failed) : null;
    const chosen = offer.open.find((w) => w.way === way);
    const shown = labelOf(r.name);
    const where = build && chosen?.on ? `for ${shown} on ${labelOf(chosen.on)}` : `on ${shown}`;
    setConfirm({
      scope: `runner:${r.name}`,
      title: again ? (build ? `Try building kiln ${where} again?` : `Try upgrading ${shown} again?`) : build ? `Build the newest kiln ${where}?` : `Upgrade ${shown}?`,
      lead: "What will happen:",
      plan: <RunnerPlan r={r} newest={newest} build={build} on={build ? chosen?.on : undefined} why={chosen?.why} />,
      confirmLabel: again ? "Try again" : build ? "Build and upgrade" : "Upgrade",
      run: () => startRunner(r, way),
      instead: offer.open.filter((w) => w.way !== way).map((w) => ({ label: wayLabel(w.way, false, failedWay === w.way), ask: () => askRunner(e, w.way) })),
    });
  };

  const runningRunners = new Set(fleet.upgrading.map((e) => e.r.name));
  // One start at a time for each thing (the server, every runner at once, a runner), by its
  // confirm's scope: the confirm pressed says it's on its way, what would start that thing again is
  // held until it's answered (Cancel still closes the confirm), and the answer closes, or says why
  // in, only the confirm it was pressed in (W21-R15).
  const [failed, setFailed] = React.useState<{ confirm: Confirming; why: string } | null>(null);
  const [start, starting] = useActions(async (_scope: string, c: Confirming) => {
    setFailed((f) => (f?.confirm === c ? null : f));
    try {
      await c.run();
      setConfirm((cur) => (cur === c ? null : cur));
    } catch (e) {
      setFailed({ confirm: c, why: errorMessage(e) });
    }
  });
  const runnerStarting = fleet.runners.some((e) => starting(`runner:${e.r.name}`));
  const plan = (scope: string) =>
    confirm?.scope === scope ? (
      <InlinePlan
        key={confirm.title}
        confirm={confirm}
        starting={starting(scope)}
        problem={failed?.confirm === confirm ? failed.why : null}
        onStart={() => start(scope, confirm)}
        onClose={() => setConfirm(null)}
      />
    ) : null;

  return (
    <section className="up-panel" aria-labelledby="up-machines-title">
      {uploads.input}
      <header className="up-panel-head">
        <div className="up-panel-words">
          <h3 id="up-machines-title">Server and runners</h3>
          <p>{summary}</p>
          {data.follow === false && <p>Automatic upgrades are disabled on this server. Runners can still be upgraded here.</p>}
        </div>
        <div className="up-panel-actions up-fleet-actions">
          <Button size="xs" variant="ghost" className="up-check-updates" pending={checking} disabled={data.server.checking} onClick={check}>
            <RefreshCw className={cn(data.server.checking && "animate-spin motion-reduce:animate-none")} /> Check for updates
          </Button>
          <Button
            size="xs"
            variant="secondary"
            className="up-upgrade-all"
            pending={starting("all") && confirm?.scope !== "all"}
            disabled={fleet.ready.length === 0 || fleet.ready.every((r) => runningRunners.has(r.name)) || (starting("all") && confirm?.scope === "all") || runnerStarting}
            onClick={() =>
              setConfirm({
                scope: "all",
                title: `Upgrade ${plural(fleet.ready.length, "runner")}?`,
                lead: "What will happen:",
                plan: <AllPlan runners={fleet.ready} />,
                confirmLabel: "Upgrade all",
                run: startAll,
              })
            }
          >
            Upgrade all{fleet.ready.length ? ` (${fleet.ready.length})` : ""}
          </Button>
        </div>
      </header>
      {confirm?.scope === "all" && <div className="up-wide">{plan("all")}</div>}
      {selectedNames.length > 0 && (
        <div className="up-bulk" aria-label="Selected runner actions">
          <span className="up-bulk-count">{selectedNames.length} selected</span>
          <PolicySelect label="Updates for the selected runners" value={bulk} onChange={setBulk} />
          <Button
            size="xs"
            variant="secondary"
            pending={bulkBusy}
            disabled={!bulkBusy && selectedNames.some((name) => policyPending[name] !== undefined)}
            onClick={() => void setPolicy(selectedNames, bulk, true)}
          >
            {bulkBusy ? "Saving…" : "Apply"}
          </Button>
          <span className="up-spacer" />
          <Button
            size="xs"
            variant="ghost"
            disabled={bulkBusy}
            onClick={() => {
              setSelected(new Set());
              setPolicyErrors((prev) => ({ ...prev, bulk: "" }));
            }}
          >
            Clear
          </Button>
          {policyErrors.bulk && (
            <p className="up-error" role="alert">
              {policyErrors.bulk}
            </p>
          )}
        </div>
      )}
      <table className="up-table">
        <MachinesHead
          box={
            <input
              type="checkbox"
              className="check"
              ref={(el) => {
                if (el) el.indeterminate = selectedNames.length > 0 && selectedNames.length < selectable.length;
              }}
              checked={selectedNames.length > 0 && selectedNames.length === selectable.length}
              disabled={bulkBusy || selectable.length === 0}
              onChange={(e) => setSelected(e.target.checked ? new Set(selectable.map((r) => r.name)) : new Set())}
              aria-label="Select all editable runners"
            />
          }
        />
        <ServerRows
          s={data.server}
          view={server}
          job={serverJob}
          pending={serverPending}
          starting={starting("server")}
          uploading={uploading}
          picked={picked === "server"}
          output={outputs.has("server")}
          onOutput={() => toggleOutput("server")}
          onUpload={() => uploads.choose(data.server.target)}
          onChangeWaits={(w) => setWaitsEditing({ focus: w ?? "upgrade" })}
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
          plan={plan("server")}
        />
        {fleet.runners.map((e) => (
          <RunnerRows
            key={e.r.name}
            e={e}
            newest={newest}
            picked={picked === `runner:${e.r.name}`}
            selected={selectedNames.includes(e.r.name)}
            bulkSaving={bulkBusy}
            starting={starting(`runner:${e.r.name}`)}
            held={starting("all")}
            uploading={uploading}
            plan={plan(`runner:${e.r.name}`)}
            output={outputs.has(e.r.name)}
            policyPending={policyPending[e.r.name]}
            policyError={policyErrors[`runner:${e.r.name}`]}
            onSelect={(on) => select(e.r.name, on)}
            onPolicy={(p) => void setPolicy([e.r.name], p)}
            onUpgrade={(way) => askRunner(e, way)}
            onOutput={() => toggleOutput(e.r.name)}
            onUpload={() => e.r.target && uploads.choose(e.r.target)}
            onAskAgent={(findings) =>
              startFrom(navigate, {
                from: `From System → Updates: ${labelOf(e.r.name)}`,
                target: `agent:${e.r.name}`,
                cwd: e.r.checkout ?? undefined,
                prompt: diagnosisPrompt(e.r, e.job, newest, findings),
              })
            }
            onChangeWaits={(w) => setWaitsEditing({ focus: w ?? "upgrade" })}
          />
        ))}
      </table>
      {data.waits && <WaitsLine waits={data.waits} editing={waitsEditing} onEditing={setWaitsEditing} />}
    </section>
  );
}

/** The table's head: its select-all box (`box`, none while it loads), then its columns. */
export function MachinesHead({ box }: { box?: React.ReactNode }) {
  return (
    <thead>
      <tr>
        <th className="up-c-check" scope="col">
          {box}
        </th>
        <th className="up-c-name" scope="col">
          Machine
        </th>
        <th className="up-c-build" scope="col">
          Build
        </th>
        <th className="up-c-state" scope="col">
          State
        </th>
        <th className="up-c-policy" scope="col">
          Updates
        </th>
        <th className="up-c-act" scope="col">
          Actions
        </th>
      </tr>
    </thead>
  );
}

/** A machine's state: its word in its tone, what that means now, and while it is upgraded a moving line under it. */
function StateCell({ tone, word, sub, moving, title }: { tone: Tone; word: string; sub: string; moving: boolean; title?: string }) {
  return (
    <>
      <span className="up-state" data-tone={tone}>
        {word}
      </span>
      {sub && (
        <span className="up-state-sub" title={title ?? sub}>
          {sub}
        </span>
      )}
      {moving && (
        <span className="up-track" aria-hidden="true">
          <i />
        </span>
      )}
    </>
  );
}

/**
 * Confirm an upgrade in place, showing step by step what it will do. `starting`: what it confirms is
 * on its way (from here, or from this thing's confirm closed meanwhile): the other ways wait for it.
 */
function InlinePlan({ confirm, starting, problem, onStart, onClose }: { confirm: Confirming; starting: boolean; problem: string | null; onStart: () => void; onClose: () => void }) {
  return (
    <Confirm
      className="up-confirm"
      label={confirm.title}
      actions={
        <>
          <Button size="xs" pending={starting} onClick={onStart}>
            {starting ? "Starting…" : confirm.confirmLabel}
          </Button>
          {confirm.instead?.map((w) => (
            <Button key={w.label} size="xs" variant="ghost" disabled={starting} onClick={w.ask}>
              {w.label}
            </Button>
          ))}
          <Button size="xs" variant="ghost" onClick={onClose}>
            Cancel
          </Button>
        </>
      }
    >
      <p className="up-confirm-title">{confirm.title}</p>
      <p className="up-confirm-lead">{confirm.lead}</p>
      {confirm.plan}
      {problem && (
        <p className="up-error" role="alert">
          {problem}
        </p>
      )}
    </Confirm>
  );
}

/* ---------------------------- the server ---------------------------- */

function ServerRows({
  s,
  view,
  job,
  pending,
  starting,
  uploading,
  picked,
  output,
  onOutput,
  onUpload,
  onAction,
  onChangeWaits,
  plan,
}: {
  s: ServerUpdate;
  view: ServerView;
  job: UpdateJob | undefined;
  pending: boolean;
  /** Its upgrade, confirmed, is on its way: its confirm says so, and once that's closed its button does. */
  starting: boolean;
  /** The platforms whose binary is being uploaded. */
  uploading: string[];
  picked: boolean;
  output: boolean;
  onOutput: () => void;
  onUpload: () => void;
  onAction: (a: "restart" | "replace") => void;
  onChangeWaits: (which: UpdateJob["timed_out"]) => void;
  /** Its upgrade, being confirmed. */
  plan: React.ReactNode;
}) {
  const busy = job?.state === "running" || pending;
  const held = starting && plan !== null;
  const { os, arch } = platformOf(s.target);
  return (
    <tbody className="up-machine" data-hl={picked || undefined}>
      <tr className="up-row">
        <td className="up-c-check" />
        <td className="up-c-name">
          <span className="up-name">
            kiln server<span className="up-tag">server</span>
          </span>
          <span className="up-sub" title={view.runsAs}>
            {os} · {arch} · {s.mode}
          </span>
        </td>
        <td className="up-c-build">
          <span className="up-build" title={`build ${s.build} · ${s.target}`}>
            {view.build}
          </span>
          <span className="up-sub">{s.version}</span>
        </td>
        <td className="up-c-state">
          <StateCell tone={view.tone} word={view.word} sub={view.sub} moving={view.tone === "run"} title={s.note} />
        </td>
        <td className="up-c-policy">
          <span className="up-sub up-policy-words">{view.policy}</span>
        </td>
        <td className="up-c-act">
          <span className="up-acts" data-lead={(s.action === "restart" && s.newer) || (s.action === "replace" && s.replacement) ? "pill" : undefined}>
            {s.action === "restart" && (s.newer || !!s.image?.error) && (
              <Button size="xs" variant={s.newer ? "secondary" : "ghost"} pending={starting && !held} disabled={busy || held} onClick={() => onAction("restart")}>
                {s.newer ? <CircleArrowUp /> : <RotateCcw />} {s.newer ? "Upgrade server" : "Restart anyway"}
              </Button>
            )}
            {s.action === "replace" && s.replacement && (
              <Button size="xs" variant="secondary" pending={starting && !held} disabled={busy || held} onClick={() => onAction("replace")}>
                <CircleArrowUp /> {s.replacement.origin === "upload" ? `Install build ${s.replacement.build ?? ""}` : `Install kiln ${s.replacement.version ?? ""}`}
              </Button>
            )}
            {s.mode !== "kubernetes" && (
              <Button size="xs" variant="ghost" pending={uploading.includes(s.target)} onClick={onUpload}>
                <Upload /> {uploading.includes(s.target) ? "Uploading…" : "Upload a build"}
              </Button>
            )}
            {job && (
              <Button size="xs" variant="ghost" aria-expanded={output} onClick={onOutput}>
                <Terminal /> Output
              </Button>
            )}
          </span>
        </td>
      </tr>
      <tr className="up-more">
        <td colSpan={6}>
          <div className="up-more-in">
            {view.notable && <p className="up-note">{pending ? "Restarting… this page reconnects when the server is back." : s.note}</p>}
            {plan}
            {job && output && <UpgradeLog key={job.id} job={job} onChangeWaits={() => onChangeWaits(job.timed_out)} />}
          </div>
        </td>
      </tr>
    </tbody>
  );
}

function ServerPlan({ s, action }: { s: ServerUpdate; action: "restart" | "replace" }) {
  if (action === "restart") {
    return (
      <ul className="up-plan">
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
    <ul className="up-plan">
      <li>
        {r?.origin === "upload" ? `kiln installs the uploaded build ${r.build ?? ""}` : `kiln downloads kiln ${r?.version ?? ""} for ${s.target} from GitHub and installs it`} over its own binary
        (build {s.build}).
      </li>
      <li>Its supervisor restarts the server into it within seconds, and this page reconnects.</li>
      <li>Runners keep their terminals, agents and jobs; terminals on the server itself end.</li>
    </ul>
  );
}

/* ---------------------------- runners ---------------------------- */

function PolicySelect({ label, value, onChange, disabled }: { label: string; value: UpdatePolicy; onChange: (value: UpdatePolicy) => void; disabled?: boolean }) {
  return (
    <div className="select up-select">
      <select aria-label={label} value={value} disabled={disabled} onChange={(e) => onChange(e.target.value as UpdatePolicy)}>
        {POLICIES.map((p) => (
          <option key={p.value} value={p.value}>
            {p.label}
          </option>
        ))}
      </select>
      <ChevronDown size={13} aria-hidden="true" />
    </div>
  );
}

/** "Waits for the release build of 31c0bba: run #232, its build step, on adacachy, 11m 04s in",
 * the run a link and its time ticking. */
function ReleaseWaitWords({ w, newest }: { w: ReleaseWait; newest: FleetTarget | null }) {
  useLabels();
  const nowS = useNow() / 1000;
  const parts = [w.step ? `its ${w.step} step` : null, w.on ? `on ${labelOf(w.on)}` : null, w.since ? `${formatDuration(nowS - w.since)} in` : null].filter(Boolean);
  return (
    <>
      Waits for the release build{newest ? ` of ${shortSha(newest.commit)}` : ""}: <Link to={`/runs/${w.run}`}>run #{w.number}</Link>
      {parts.length > 0 ? `, ${parts.join(", ")}` : ""}
    </>
  );
}

function RunnerRows({
  e,
  newest,
  picked,
  selected,
  bulkSaving,
  starting,
  held,
  uploading,
  plan,
  output,
  policyPending,
  policyError,
  onSelect,
  onPolicy,
  onUpgrade,
  onOutput,
  onUpload,
  onAskAgent,
  onChangeWaits,
}: {
  e: RunnerEntry;
  newest: FleetTarget | null;
  picked: boolean;
  selected: boolean;
  /** An update policy is being applied to the selected runners: the selection stays as it is meanwhile. */
  bulkSaving: boolean;
  /** Its upgrade, confirmed, is on its way (its Upgrade says so once its confirm is closed); `held`: every runner's is. */
  starting: boolean;
  held: boolean;
  /** The platforms whose binary is being uploaded. */
  uploading: string[];
  plan: React.ReactNode;
  output: boolean;
  policyPending: UpdatePolicy | undefined;
  policyError: string | undefined;
  onSelect: (on: boolean) => void;
  onPolicy: (p: UpdatePolicy) => void;
  onUpgrade: (way: UpgradeWay) => void;
  onOutput: () => void;
  onUpload: () => void;
  /** Start an agent on it to find out what's wrong, told what kiln's checks found. */
  onAskAgent: (findings: RunnerFinding[]) => void;
  onChangeWaits: (which: UpdateJob["timed_out"]) => void;
}) {
  useLabels();
  const { r, job, view } = e;
  const offer = upgradeOffer(r, job);
  const failedWay = offer.failed ? wayOf(offer.failed) : null;
  const primary = offer.actionable && !plan ? offer.open[0] : undefined;
  const saving = policyPending !== undefined;
  // Named as people know it (its label; its name, the machine id, on hover), its host name beside that when it's another.
  const shown = labelOf(r.name);
  const hostDiffers = r.hostname.toLowerCase() !== shown.toLowerCase();
  const builds = [`installed build ${r.build ?? "not reported"}`, r.target_build ? `available build ${r.target_build}` : null, r.from ? `source: ${r.from}` : null].filter(Boolean).join(" · ");
  const [diag, setDiag] = React.useState<Diagnosing | null>(null);
  const look = async () => {
    setDiag((s) => ({ ...(s ?? {}), looking: true, error: undefined }));
    try {
      const d = await api.diagnoseRunner(r.name);
      setDiag((s) => ({ ...(s ?? {}), looking: false, d }));
    } catch (err) {
      setDiag((s) => ({ ...(s ?? {}), looking: false, error: errorMessage(err) }));
    }
  };
  // One repair at a time for a runner (W21-R15): the one pressed says so and the others wait, even
  // across its diagnosis being closed and opened again.
  const [working, setWorking] = React.useState<string | null>(null);
  const [repair, repairing] = useAction(async (f: RunnerFinding) => {
    const fix = f.fix;
    if (!fix) return;
    setWorking(fix.action);
    setDiag((s) => s && { ...s, said: undefined });
    try {
      const out = await api.repairRunner(r.name, fix.action, fix.way);
      const text = out.job ? "The upgrade started: its output is in its row." : (out.message ?? "Done.");
      setDiag((s) => s && { ...s, said: { ok: true, text } });
      invalidate("updates");
      if (!out.job) void look();
    } catch (err) {
      setDiag((s) => s && { ...s, said: { ok: false, text: errorMessage(err) } });
    }
  });
  return (
    <tbody className="up-machine" data-hl={picked || undefined} data-picked={selected || undefined}>
      <tr className="up-row">
        <td className="up-c-check">
          <input
            type="checkbox"
            className="check"
            checked={selected}
            disabled={bulkSaving || r.locked}
            onChange={(ev) => onSelect(ev.target.checked)}
            aria-label={`Select ${shown}`}
            title={r.locked ? "Update policy is locked on this machine" : undefined}
          />
        </td>
        <td className="up-c-name">
          <span className="up-name" title={r.name}>
            {shown}
          </span>
          <span className="up-sub" title={hostDiffers ? `hostname ${r.hostname}` : undefined}>
            {r.os} · {r.arch}
            {hostDiffers ? ` · ${r.hostname}` : ""}
          </span>
        </td>
        <td className="up-c-build">
          <span className="up-build" data-tone={view.moving ? view.tone : undefined} title={builds}>
            {view.moving ? (
              <>
                <span className="up-from">{view.from}</span>
                <span className="up-goes">→</span>
                <span className="up-to">{view.to ?? "pending build"}</span>
              </>
            ) : (
              view.from
            )}
          </span>
          <span className="up-sub">v{r.version}</span>
        </td>
        <td className="up-c-state">
          <StateCell tone={view.tone} word={view.word} sub={view.sub} moving={view.lit} />
        </td>
        <td className="up-c-policy" aria-busy={saving}>
          {r.locked ? (
            <span className="up-sub up-policy-words">Locked on its machine</span>
          ) : (
            <PolicySelect label={`Updates for ${shown}`} value={policyPending ?? r.policy} onChange={onPolicy} disabled={bulkSaving && selected} />
          )}
          {saving && (
            <span className="up-saving" role="status">
              Saving…
            </span>
          )}
        </td>
        <td className="up-c-act">
          {/* In one order on every row: its upgrade first, then a build to upload, its output, Diagnose. */}
          <span className="up-acts" data-lead={primary ? "pill" : undefined}>
            {primary && (
              <Button size="xs" variant="secondary" pending={starting} disabled={held} title={sentence(primary.why)} onClick={() => onUpgrade(primary.way)}>
                {failedWay === primary.way ? <RotateCcw /> : primary.way === "build" ? <Hammer /> : <CircleArrowUp />} {wayLabel(primary.way, true, failedWay === primary.way)}
              </Button>
            )}
            {offer.missing && (
              <Button size="xs" variant="ghost" pending={uploading.includes(r.target ?? "")} onClick={onUpload}>
                <Upload /> {uploading.includes(r.target ?? "") ? "Uploading…" : "Upload a build"}
              </Button>
            )}
            {job && (
              <Button size="xs" variant="ghost" aria-expanded={output} onClick={onOutput}>
                <Terminal /> Output
              </Button>
            )}
            {r.connected && !plan && (
              <Button size="xs" variant="ghost" aria-expanded={diag !== null} onClick={() => (diag ? setDiag(null) : void look())}>
                <Stethoscope /> Diagnose
              </Button>
            )}
          </span>
        </td>
      </tr>
      <tr className="up-more">
        <td colSpan={6}>
          <div className="up-more-in">
            {r.waits_on && (
              <p className="up-note">
                <ReleaseWaitWords w={r.waits_on} newest={newest} />
              </p>
            )}
            {offer.stuck.map((w) => (
              <p key={w.way} className="up-note">
                {w.way === "build" ? "Build unavailable" : "Download unavailable"}: {w.why}
              </p>
            ))}
            {r.instances.length > 1 && (
              <ul className="up-procs">
                {r.instances.map((i) => (
                  <li key={i.id}>
                    <StatusDot status={i.newest ? "succeeded" : "timed_out"} />
                    <span>{i.newest ? "New runner" : "Previous runner"}</span>
                    <span className="up-dim up-mono">
                      {i.build ?? (i.legacy ? "legacy" : i.id.slice(0, 8))}
                      {i.pid ? ` · pid ${i.pid}` : ""}
                    </span>
                    <span className="up-dim">{i.newest ? "Takes new work" : `Keeps ${holding(i)}; retires once that ends`}</span>
                  </li>
                ))}
              </ul>
            )}
            {policyError && (
              <p className="up-error" role="alert">
                {policyError}
              </p>
            )}
            <React.Suspense fallback={null}>
              <DoctorLine runner={r.name} />
            </React.Suspense>
            {plan}
            {diag && <DiagnosisPanel name={shown} s={diag} working={repairing ? working : null} onRepair={repair} onAgain={() => void look()} onAskAgent={onAskAgent} />}
            {job && output && <UpgradeLog key={job.id} job={job} onChangeWaits={() => onChangeWaits(job.timed_out)} />}
          </div>
        </td>
      </tr>
    </tbody>
  );
}

function RunnerPlan({ r, newest: target, build, on, why }: { r: RunnerUpdate; newest: FleetTarget | null; build: boolean; on?: string; why?: string }) {
  useLabels();
  const newest = r.instances.find((i) => i.newest);
  const inPlace = r.method === "native" && newest !== undefined && newest.id !== "main" && !newest.legacy;
  const of = target ? ` (${shortSha(target.commit)})` : "";
  const to = r.target_build ? `build ${r.target_build}${of}` : `the ${r.target ?? ""} build (this version's GitHub release, if one is published)`;
  const shown = labelOf(r.name);
  const builder = on ? labelOf(on) : undefined;
  return (
    <ul className="up-plan">
      {build && on ? (
        <>
          <li>
            {why ?? `${builder} builds ${target ? shortSha(target.commit) : "the newest kiln"} for ${r.target ?? "its platform"}`}. The build runs in a terminal kiln opens on {builder} (it won&rsquo;t
            appear among your terminals): it fetches the source from this server, builds it in <code>~/.cache/kiln/self-build</code> and hands the binary back to this server. Neither machine needs a
            checkout of kiln.
          </li>
          <li>
            Then {shown} downloads it from this server, checks its SHA-256 and runs <code>kiln install --agent --upgrade</code>. The other machines of its platform download the same build.
          </li>
        </>
      ) : build ? (
        <>
          <li>
            {r.how === "build" ? (
              <>
                No binary of {target ? shortSha(target.commit) : "the newest kiln"} exists for {r.target ?? "its platform"} yet, so {shown} builds it
              </>
            ) : (
              <>
                {shown} builds {target ? shortSha(target.commit) : "the newest kiln"} itself rather than downloading it
              </>
            )}
            : kiln opens a terminal on it (it won&rsquo;t appear among your terminals) that fetches the source from this server, builds it in <code>~/.cache/kiln/self-build</code> and runs{" "}
            <code>kiln install --agent --upgrade</code>.
          </li>
          <li>It needs no checkout of kiln. It brings its stable Rust toolchain up to date first (kiln needs a recent one), and the build takes its CPU for a few minutes.</li>
          {r.os === "macos" && <li>The binary comes back to this server, so other Macs download it instead of building.</li>}
        </>
      ) : r.method === "native" ? (
        <li>
          {shown} downloads {to} from this server, checks its SHA-256 and runs <code>kiln install --agent --upgrade</code>.
        </li>
      ) : (
        <li>
          {shown}&rsquo;s runner predates upgrade commands, so kiln opens a terminal on it (it won&rsquo;t appear among your terminals), downloads {to} from this server, checks its SHA-256 and runs{" "}
          <code>kiln install --agent --upgrade</code> there.
        </li>
      )}
      {inPlace ? (
        <li>The runner takes the new binary in place: the same process, with its terminals and agent sessions handed across. If a job is running, it switches once the job ends.</li>
      ) : (
        <li>
          A new runner process starts beside the current one and takes all new terminals, agents and jobs. The current one keeps {newest ? holding(newest) : "what it runs"} until that ends, then
          retires.
        </li>
      )}
      <li>Nothing running on {shown} is stopped or restarted.</li>
    </ul>
  );
}

function AllPlan({ runners }: { runners: RunnerUpdate[] }) {
  useLabels();
  return (
    <div>
      <ul className="up-plan">
        {runners.map((r) => (
          <li key={r.name}>
            <strong className="font-medium text-[#d6dbe2]">{labelOf(r.name)}</strong>:{" "}
            {r.next ?? (
              <>
                {r.target_build ? `to build ${r.target_build}` : `to the ${r.target ?? ""} build`},{" "}
                {r.method === "native" ? "by its own upgrade command" : "through a terminal on it (it predates upgrade commands)"}
              </>
            )}
          </li>
        ))}
      </ul>
      <p className="up-plan-after">Each gets a new runner beside the current one (or the new binary in place); nothing they run is stopped.</p>
    </div>
  );
}

/* ---------------------------- a runner doctor ---------------------------- */

/** What an agent attached to the runners (docs/agentic.md R4) is doing about this one, while it does it. */
function DoctorLine({ runner }: { runner: string }) {
  const incidents = useSuspenseResource("incidents/agents", (s) => attachApi.incidents(s).catch(() => [] as IncidentBrief[]), { on: ["incident"], pollMs: 30_000 }).data;
  const i = incidents.find((x) => x.runner === runner && (x.state === "open" || x.state === "waiting"));
  if (!i) return null;
  return (
    <p className="up-note">
      {i.name}: {i.title} · {i.state === "open" ? (i.report ?? "its agent is on it") : (i.why ?? "waiting for you")}
      {i.agent && (
        <>
          {" · "}
          <Link to={`/agents/${encodeURIComponent(i.agent)}`}>its agent</Link>
        </>
      )}
    </p>
  );
}

/* ---------------------------- diagnosing ---------------------------- */

/** A row's diagnosis as it goes: being looked at, what was found, and what its last repair said. */
interface Diagnosing {
  looking?: boolean;
  d?: RunnerDiagnosis;
  error?: string;
  said?: { ok: boolean; text: string };
}

const FINDING_TONE: Record<RunnerFinding["level"], string> = { fail: "failed", warn: "timed_out", info: "skipped", ok: "succeeded" };

/**
 * What kiln's checks found about a runner, in its row: what's wrong first, each with the repair
 * that fixes it when kiln can make it; then asking an agent (told all of it) and looking again.
 */
function DiagnosisPanel({
  name,
  s,
  working,
  onRepair,
  onAgain,
  onAskAgent,
}: {
  name: string;
  s: Diagnosing;
  /** The repair on its way, if one is: the others wait for it. */
  working: string | null;
  onRepair: (f: RunnerFinding) => void;
  onAgain: () => void;
  onAskAgent: (findings: RunnerFinding[]) => void;
}) {
  if (!s.d) {
    return s.error ? (
      <p className="up-note" data-tone="error">
        Couldn&rsquo;t diagnose {name}: {s.error}
      </p>
    ) : (
      <p className="up-note">Looking at {name}…</p>
    );
  }
  const d = s.d;
  const wrong = d.findings.filter((f) => f.level !== "ok");
  const fine = d.findings.length - wrong.length;
  // A repair is offered once, on the worst finding it fixes (the list is worst first).
  const offered = new Set<string>();
  const fixOf = (f: RunnerFinding) => {
    const key = f.fix ? `${f.fix.action}:${f.fix.way ?? ""}` : "";
    if (!f.fix || offered.has(key)) return undefined;
    offered.add(key);
    return f.fix;
  };
  return (
    <div className="up-diag" aria-live="polite">
      {wrong.length === 0 ? (
        <p className="up-diag-title">Nothing wrong found{fine ? `: ${fine} check${fine === 1 ? "" : "s"} fine` : ""}.</p>
      ) : (
        <ul className="up-diag-list">
          {wrong.map((f) => {
            const fix = fixOf(f);
            return (
              <li key={f.id} className="up-diag-item">
                <StatusDot status={FINDING_TONE[f.level]} title={f.level} />
                <div className="min-w-0 flex-1">
                  <p className="up-diag-title">{f.title}</p>
                  {f.detail && (f.detail.includes("\n") ? <pre className="up-diag-log">{f.detail}</pre> : <p className="up-note">{f.detail}</p>)}
                </div>
                {fix && (
                  <Button size="xs" variant="secondary" pending={working === fix.action} disabled={working !== null && working !== fix.action} onClick={() => onRepair(f)}>
                    {working === fix.action ? "Working…" : fix.label}
                  </Button>
                )}
              </li>
            );
          })}
        </ul>
      )}
      {!d.machine_checked && d.why && (
        <p className="up-note">
          Only what the server knows: {name} itself wasn&rsquo;t looked at ({d.why}).
        </p>
      )}
      {s.said && (
        <p className="up-note" data-tone={s.said.ok ? undefined : "error"}>
          {s.said.text}
        </p>
      )}
      <div className="up-diag-acts">
        <Button size="xs" variant="ghost" onClick={() => onAskAgent(d.findings)}>
          <Bot /> Ask an agent
        </Button>
        <Button size="xs" variant="ghost" pending={s.looking} onClick={onAgain}>
          <RefreshCw /> {s.looking ? "Looking…" : "Look again"}
        </Button>
        {wrong.length > 0 && fine > 0 && (
          <span className="up-note">
            {fine} other check{fine === 1 ? "" : "s"} fine
          </span>
        )}
      </div>
    </div>
  );
}

/* ---------------------------- how long kiln waits ---------------------------- */

/**
 * How long kiln waits on an upgrade, in a line under the table, changed in place: each wait's
 * field, then Save. A link from an upgrade that ran out of time opens it on the wait that did.
 */
function WaitsLine({ waits, editing, onEditing }: { waits: NonNullable<UpdatesOverview["waits"]>; editing: { focus: Which } | null; onEditing: (v: { focus: Which } | null) => void }) {
  const [draft, setDraft] = React.useState<WaitsDraft | null>(null);
  const shown = draft ?? draftOf(waits.now);
  const read = readDraft(shown);
  const changed = typeof read !== "string" && WAITS.some((f) => read[f.key] !== waits.now[f.key]);
  // Opened (or asked for again from an upgrade's output): into view, on the field asked for.
  const opened = React.useCallback(
    (el: HTMLFormElement | null) => {
      if (!el || !editing) return;
      const field = el.querySelector<HTMLInputElement>(`#up-wait-${editing.focus}`);
      field?.scrollIntoView({ block: "center" });
      field?.focus({ preventScroll: true });
    },
    [editing],
  );
  const close = () => {
    setDraft(null);
    onEditing(null);
  };
  // The editor open now: an answer closes only the one it was asked from, so one opened since (from
  // here, or from an upgrade's output) stays as it is, what's typed in it too (W21-R15).
  const open = React.useRef(editing);
  open.current = editing;
  const act = async (what: () => Promise<void>, done: string) => {
    const at = editing;
    await what();
    if (open.current === at) close();
    toast.success(done);
    invalidate("updates");
  };
  const hold = useHold("Couldn't change how long kiln waits");
  const reset = () => hold.change("reset", () => act(() => api.resetUpgradeWaits(), "kiln waits as kiln.toml says"));
  const save = (to: UpgradeWaits) => hold.change("save", () => act(() => api.setUpgradeWaits(to), "kiln waits that long from now on"));
  const config = WAITS.map((f) => `${f.label.toLowerCase()} ${minutesSaid(waits.config[f.key])}`).join(", ");
  const now = (key: keyof UpgradeWaits) => <b>{waits.now[key]}</b>;
  if (!editing) {
    return (
      <div className="up-waits">
        <span className="up-waits-words">
          kiln waits {now("upgrade_minutes")} min for an upgrade, {now("wait_for_jobs_minutes")} min for running jobs and {now("build_minutes")} min for a build before it calls one failed.
        </span>
        {waits.changed && (
          <Button size="xs" variant="ghost" pending={hold.doing("reset")} disabled={hold.held("reset")} onClick={reset}>
            Use kiln.toml&rsquo;s
          </Button>
        )}
        <Button size="xs" variant="ghost" onClick={() => onEditing({ focus: "upgrade" })}>
          Change
        </Button>
      </div>
    );
  }
  return (
    <form
      ref={opened}
      className="up-waits"
      data-editing
      aria-label="How long kiln waits"
      onSubmit={(e) => {
        e.preventDefault();
        if (typeof read !== "string" && changed) save(read);
      }}
    >
      <div className="up-waits-fields">
        {WAITS.map((f) => {
          const id = `up-wait-${f.which}`;
          const n = Number(shown[f.key]);
          return (
            <label key={f.key} htmlFor={id} title={`${Number.isInteger(n) && n >= f.min ? `${sentence(minutesSaid(n))} ` : ""}${f.hint}`}>
              {f.label}
              <input
                id={id}
                type="number"
                inputMode="numeric"
                min={f.min}
                max={f.max}
                step={1}
                value={shown[f.key]}
                aria-describedby={`${id}-hint`}
                onChange={(e) => setDraft({ ...shown, [f.key]: e.target.value })}
              />
              min
              <span id={`${id}-hint`} className="sr-only">
                {f.hint}
              </span>
            </label>
          );
        })}
        <span className="up-spacer" />
        <Button type="button" size="xs" variant="ghost" onClick={close}>
          Cancel
        </Button>
        <Button type="submit" size="xs" variant="secondary" pending={hold.doing("save")} disabled={!changed || hold.held("save")}>
          Save
        </Button>
      </div>
      <p className="up-waits-note">
        An upgrade that takes longer reads as failed, and as done if the runner gets there later. A change applies to upgrades under way too.{" "}
        {waits.changed ? `Changed here; kiln.toml says ${config}.` : "As kiln.toml's [updates] says."}
      </p>
      {draft && typeof read === "string" && (
        <p className="up-error" role="alert">
          {read}
        </p>
      )}
    </form>
  );
}

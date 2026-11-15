/**
 * A runner's engine and its microVMs on the Runners page (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d,
 * P2.3, P3.2): the table's "Agents in" cell, from its facts; in a runner's details, its engine as the
 * server says it (`GET|PUT /api/runners/:name/engine`: switched by those who manage runners, what runs
 * on the other engine finishing there), and then the microVMs it runs, each with its controls, its
 * warm pools and the images built there.
 */
import * as React from "react";
import { Ban, ChartLine, Hammer, Pause, Play, RotateCcw, Square, Trash2, X, Check } from "lucide-react";
import { engineApi, engineOf, runnerEngineRes, runnerVmsRes, sandboxApi, type Engine, type MicroVm, type RunnerImage, type WarmPool } from "@/api/microvms";
import type { HostSample } from "@/api/wire";
import { Button } from "@/components/ui/button";
import { Confirm, Meter, Sparkline } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { Segmented } from "@/components/ui/tabs";
import { toast } from "@/components/ui/toast";
import { useRes } from "@/lib/data";
import { formatBytes } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { Link } from "@/lib/router";
import { LimitMeter } from "./harness/limits";
import { ENGINE_ICON, START, StartMark, WarmDots } from "./sandbox-marks";
import "./runners-microvms.css";

const VM_STATUS: Record<MicroVm["state"], string> = { running: "running", idle: "succeeded", paused: "queued", failed: "failed", draining: "timed_out" };
const VM_WORD: Record<MicroVm["state"], string> = { running: "Running", idle: "Idle", paused: "Paused", failed: "Failed", draining: "Draining" };
const ago = (s: number) => (s < 60 ? `${s}s` : s < 3600 ? `${Math.round(s / 60)}m` : s < 86400 ? `${Math.round(s / 3600)}h` : `${Math.round(s / 86400)}d`);
const G = 2 ** 30;
const gib = (n: number) => Math.round((n / G) * 100) / 100;

/** The table's cell, from its facts: its engine as an icon and a word; one that can't run microVMs carries ⊘, its reason in the tooltip; a runner from before engines, none. */
export function EngineCell({ facts }: { facts: Parameters<typeof engineOf>[0] }) {
  const e = engineOf(facts);
  if (!e)
    return (
      <span className="cell-dim" title="Its runner says nothing of microVMs: its kiln is from before them">
        —
      </span>
    );
  const Icon = ENGINE_ICON[e.engine];
  return (
    <span className="rv-engine" title={e.why ?? (e.engine === "microvm" ? `In microVMs (${facts?.isolation?.hypervisor ?? "its hypervisor"})` : "On the host")}>
      <Icon size={13} aria-hidden="true" />
      <span className="rv-engine-word">{e.engine}</span>
      {!e.can && <Ban size={12} className="rv-ban" role="img" aria-label={e.why ?? "Can't run microVMs"} />}
    </span>
  );
}

/** Bytes in a bar's words: GiB, or TiB from a TiB up, to two places. */
function size(used: number, total: number): { used: number; limit: number; unit: string } {
  const unit = total >= 2 ** 40 ? 2 ** 40 : G;
  return { used: Math.round((used / unit) * 100) / 100, limit: Math.round((total / unit) * 100) / 100, unit: unit === G ? "GiB" : "TiB" };
}

/**
 * Where a runner is now, first in its details (under its name): its CPU, memory and disk as live
 * bars, from its own samples, and on a runner whose agents run in microVMs, the vCPUs its Hex node
 * holds for them, of its room.
 */
export function RunnerNow({ name, sample }: { name: string; sample: HostSample | null }) {
  const room = useRes(runnerVmsRes(name)).data.room;
  if (!sample && !room) return null;
  const held = room ? room.vcpus_total - room.vcpus_free : 0;
  return (
    <div className="rv-now" role="group" aria-label="Now">
      {sample && (
        <>
          <LimitMeter live limit={{ id: "cpu", label: "CPU", used_percent: sample.cpu_pct, used: Math.round((sample.cpu_pct / 100) * sample.cpus * 100) / 100, limit: sample.cpus, unit: "cores" }} />
          <LimitMeter live limit={{ id: "memory", label: "Memory", used_percent: (sample.mem_used / sample.mem_total) * 100, ...size(sample.mem_used, sample.mem_total) }} />
          <LimitMeter live limit={{ id: "disk", label: "Disk", used_percent: (sample.disk_used / sample.disk_total) * 100, ...size(sample.disk_used, sample.disk_total) }} />
        </>
      )}
      {room && <LimitMeter live limit={{ id: "vcpus", label: "vCPUs held", used_percent: (held / room.vcpus_total) * 100, used: held, limit: room.vcpus_total, unit: "vCPU" }} />}
    </div>
  );
}

/**
 * A runner's engine, in its details, as the server says it: the switch (those who manage runners;
 * microvm only where it can hold them), its checks (its facts' microVM, its isolation, whether it can
 * hold microVMs now), where its engine comes from, and its sessions still on the other engine, which
 * finish there: listed in place, ended now (destructive, confirmed inline), or kept by taking the switch
 * back (`runners.engine.end_draining`, `runners.engine.cancel`).
 */
export function EngineBlock({ name, manage }: { name: string; manage: boolean }) {
  const res = useRes(runnerEngineRes(name));
  const e = res.data;
  const [asking, setAsking] = React.useState<Engine | null>(null);
  const [sending, setSending] = React.useState(false);
  const [drains, setDrains] = React.useState(false);
  const [ending, setEnding] = React.useState(false);
  if (!e.engine) return null;
  const drained = (call: Promise<typeof e>, said: (v: typeof e) => string, failed: string) => {
    setSending(true);
    call
      .then((next) => {
        res.setData(() => next);
        setEnding(false);
        toast({ title: said(next), tone: "info" });
      })
      .catch((err: unknown) => toast.error(failed, errorMessage(err)))
      .finally(() => setSending(false));
  };
  const endNow = () => drained(engineApi.endDraining(name), (v) => `Ended ${(v as typeof v & { ended?: string[] }).ended?.length ?? 0}`, "Couldn't end them");
  const undo = () => drained(engineApi.cancel(name), (v) => `Back on ${v.engine === "microvm" ? "microVMs" : "its host"}`, "Couldn't take the switch back");
  const can = e.microvm?.usable ?? false;
  const send = (engine: Engine | null) => {
    setSending(true);
    engineApi
      .set(name, engine)
      .then((next) => {
        res.setData(() => next);
        setAsking(null);
        toast({ title: engine ? `Its new sessions run in ${engine === "microvm" ? "microVMs" : "its host"}` : "Its engine is what it can do", tone: "info" });
      })
      .catch((err: unknown) => toast.error("Couldn't set its engine", errorMessage(err)))
      .finally(() => setSending(false));
  };
  const checks: [string, string, string][] = [
    ["microVM", e.virt?.microvm ? "succeeded" : "skipped", e.virt?.microvm ? "virt.microvm: yes" : "virt.microvm: no (its engine is host)"],
    [
      "Isolation",
      e.isolation?.available ? "succeeded" : "failed",
      e.isolation?.available ? `isolation.available: yes · ${e.isolation.hypervisor ?? ""}` : `isolation.why_not: ${e.isolation?.why_not ?? "not said"}`,
    ],
    ["Can hold", can ? "succeeded" : "failed", can ? "It can hold agents in microVMs now" : (e.microvm?.why ?? "It can't hold agents in microVMs")],
  ];
  return (
    <section className="rv-block" aria-label="Engine">
      <div className="rv-head">
        <span className="label" title="Where agents' sessions run here (CI's jobs run on its executors: the facts' CI in)">
          Agents in
        </span>
        <Segmented
          size="xs"
          label={`Engine of ${name}`}
          value={e.engine}
          pending={sending ? (asking ?? null) : null}
          onChange={(v) => v !== e.engine && setAsking(v)}
          options={(["microvm", "host"] as Engine[]).map((x) => {
            const Icon = ENGINE_ICON[x];
            const disabled = !manage || (x === "microvm" && !can);
            return {
              value: x,
              disabled,
              title: !manage ? "Only those who manage runners set its engine" : x === "microvm" && !can ? (e.microvm?.why ?? undefined) : undefined,
              label: (
                <>
                  <Icon size={13} aria-hidden="true" />
                  {x === "microvm" ? "microVM" : "host"}
                </>
              ),
            };
          })}
        />
        <span className="rv-checks">
          {checks.map(([word, status, tip]) => (
            <span key={word} title={tip}>
              <StatusDot status={status} title={tip} />
              {word}
            </span>
          ))}
        </span>
        <span className="rv-default" title={e.from === "server" ? "Its owner set it on the server" : "What it can do decides: microvm where it can hold them, else host"}>
          {e.from === "server" ? "set" : "default"}
          {e.draining > 0 && (
            <button
              type="button"
              className="rv-override rv-drain-toggle"
              aria-expanded={drains}
              onClick={() => setDrains((o) => !o)}
              title={`${e.draining} of its sessions run on the other engine, and finish there`}
            >
              {e.draining} draining
            </button>
          )}
          {e.asked && manage && (
            <button type="button" className="ms-link" disabled={sending} onClick={() => send(null)} title="Take what was set away: what it can do decides">
              use default
            </button>
          )}
        </span>
      </div>
      {e.why && <p className="rv-why">{e.why}</p>}
      {drains && e.draining_sessions.length > 0 && (
        <div className="rv-drains">
          <ul aria-label="Draining">
            {e.draining_sessions.map((d) => {
              const Icon = ENGINE_ICON[d.engine];
              return (
                <li key={d.id}>
                  <StatusDot status={d.state === "working" ? "running" : "queued"} title={d.state} />
                  <Link to={`/agents/${encodeURIComponent(d.id)}`}>{d.name}</Link>
                  <span className="rv-drain-kind">{d.kind}</span>
                  <Icon size={12} aria-label={d.engine} />
                </li>
              );
            })}
          </ul>
          {manage && (
            <span className="rv-drain-acts">
              <Button size="xs" variant="ghost" disabled={sending} onClick={() => setEnding(true)} title="End each now, rather than letting it finish where it runs">
                <Square size={12} aria-hidden="true" />
                End now
              </Button>
              {e.switch && (
                <Button size="xs" variant="ghost" disabled={sending} onClick={undo} title="Put back the engine it had: these run on it again">
                  <RotateCcw size={12} aria-hidden="true" />
                  Undo switch
                </Button>
              )}
            </span>
          )}
        </div>
      )}
      {ending && (
        <Confirm
          label={`End ${e.draining_sessions.length} now`}
          actions={
            <>
              <Button size="xs" pending={sending} onClick={endNow}>
                End them
              </Button>
              <Button size="xs" variant="ghost" onClick={() => setEnding(false)}>
                Keep
              </Button>
            </>
          }
        >
          <p>Each ends for good; its changes stay on disk.</p>
        </Confirm>
      )}
      {asking && (
        <Confirm
          label={`Run its new sessions in ${asking === "microvm" ? "microVMs" : "its host"}`}
          actions={
            <>
              <Button size="xs" pending={sending} onClick={() => send(asking)}>
                {e.sessions > 0 ? "Switch, they finish" : "Switch"}
              </Button>
              <Button size="xs" variant="ghost" onClick={() => setAsking(null)}>
                Cancel
              </Button>
            </>
          }
        >
          <p>{e.sessions > 0 ? `Its ${e.sessions} running finish on ${e.engine}.` : `New sessions on ${asking}.`}</p>
        </Confirm>
      )}
    </section>
  );
}

/** What a runner's microVMs hold: its room, then each microVM, its warm pools and the images built there. */
export function MicrovmsSection({ name, manage }: { name: string; manage: boolean }) {
  const e = useRes(runnerEngineRes(name)).data;
  const res = useRes(runnerVmsRes(name));
  const r = res.data;
  // One microVM's telemetry open at a time (kiln's principle 1).
  const [chart, setChart] = React.useState<string | null>(null);
  const [building, setBuilding] = React.useState(false);
  const can = e.microvm?.usable ?? false;
  if (!can && r.vms.length === 0 && r.images.length === 0) return null;
  const warm = r.pools.reduce((a, p) => a + p.warm, 0);
  const bound = r.pools.reduce((a, p) => a + p.bound, 0);
  const total = r.starts.warm + r.starts.template + r.starts.cold;
  return (
    <section className="rv-block" aria-label="MicroVMs">
      {/* How its microVMs start, and its warm hosts (where it is now, its bars, leads its details). */}
      {(total > 0 || bound > 0) && (
        <div className="rv-room">
          {total > 0 && (
            <span className="rv-cell">
              <span className="label" title="Flame: warm · layers: from its template · snowflake: cold">
                Starts
              </span>
              <span className="rv-meter rv-starts">
                {(["warm", "template", "cold"] as const).map((k) => {
                  const Icon = START[k].icon;
                  return (
                    <span key={k} className="sm-start" data-kind={k} title={`${START[k].word}: ${r.starts[k]}`}>
                      <Icon size={12} aria-hidden="true" />
                      {r.starts[k]}
                    </span>
                  );
                })}
              </span>
            </span>
          )}
          {bound > 0 && (
            <span className="rv-cell">
              <span className="label">Warm</span>
              <span className="rv-meter">
                <WarmDots n={warm} of={bound} />
              </span>
            </span>
          )}
        </div>
      )}

      {r.vms.length > 0 && (
        <>
          <div className="label rv-label">MicroVMs · {r.vms.length}</div>
          <ul className="rv-rows" aria-label={`${name}'s microVMs`}>
            {r.vms.map((v) => (
              <VmRow key={v.id} v={v} open={chart === v.id} onCharts={() => setChart(chart === v.id ? null : v.id)} onChanged={() => res.reload()} />
            ))}
          </ul>
        </>
      )}

      {r.pools.length > 0 && (
        <>
          <div className="label rv-label">Warm pools · {r.pools.length}</div>
          <ul className="rv-rows" aria-label={`${name}'s warm pools`}>
            {r.pools.map((p) => (
              <PoolRow key={p.agent + p.image} p={p} manage={manage} />
            ))}
          </ul>
        </>
      )}

      {can && (
        <>
          <div className="rv-labelrow">
            <span className="label rv-label">Images{r.images.length ? ` · ${r.images.length}` : ""}</span>
            {!building && (
              <Button size="xs" variant="ghost" onClick={() => setBuilding(true)}>
                <Hammer aria-hidden="true" /> Build image
              </Button>
            )}
          </div>
          {building && <BuildForm name={name} onDone={() => setBuilding(false)} />}
          {r.images.length > 0 && (
            <ul className="rv-rows" aria-label={`Images on ${name}`}>
              {r.images.map((im, i) => (
                <ImageRow key={im.from + i} im={im} />
              ))}
            </ul>
          )}
        </>
      )}
    </section>
  );
}

/** One microVM: its stand, the session it holds, how it started, CPU and memory now, and its controls in fixed places (pause, restart, stop, telemetry). */
function VmRow({ v, open, onCharts, onChanged }: { v: MicroVm; open: boolean; onCharts: () => void; onChanged: () => void }) {
  const [asking, setAsking] = React.useState(false);
  const [busy, setBusy] = React.useState(false);
  const paused = v.state === "paused";
  const act = (a: "pause" | "resume" | "restart" | "stop") => {
    setBusy(true);
    sandboxApi
      .act(v.session, a)
      .then(() => {
        onChanged();
        if (a === "pause") toast({ title: `Paused ${v.agent}'s vCPUs`, tone: "info", action: { label: "Undo", run: () => void sandboxApi.act(v.session, "resume").then(onChanged) } });
        else if (a === "restart") toast({ title: `Restarting ${v.agent}`, tone: "running" });
        else if (a === "stop") toast({ title: `Stopped ${v.agent}`, tone: "info" });
      })
      .catch((err: unknown) => toast.error(`Couldn't ${a}`, errorMessage(err)))
      .finally(() => setBusy(false));
  };
  const cpu = paused ? 0 : (v.cpu.at(-1) ?? 0);
  return (
    <li className="rv-row" data-state={v.state}>
      <StatusDot status={VM_STATUS[v.state]} title={VM_WORD[v.state]} />
      <Link to={`/agents/${v.session}?tab=sandbox`} className="rv-name" title={`${v.agent} · ${v.session}`}>
        {v.agent}
      </Link>
      <span className="rv-dim rv-image" title={v.image}>
        {v.image}
      </span>
      <span className="rv-start">
        <StartMark kind={v.start.kind} ms={v.start.ms} />
      </span>
      <span className="rv-dim rv-up" title="Up">
        {ago(v.up_s)}
      </span>
      <span className="rv-cpu" title={`CPU ${cpu}% of its ${v.state === "paused" ? "paused " : ""}vCPUs`}>
        <LimitMeter compact live limit={{ id: "cpu", label: "CPU", used_percent: cpu }} />
      </span>
      <span className="rv-mem" title={`Memory ${gib(v.memory.used)} of ${gib(v.memory.total)} GiB`}>
        <LimitMeter compact live limit={{ id: "memory", label: "Mem", used_percent: (v.memory.used / v.memory.total) * 100 }} />
      </span>
      <span className="rv-acts">
        {asking ? (
          <span className="rv-confirm" role="group" aria-label={`Stop ${v.agent}?`}>
            Stop?
            <button
              type="button"
              className="icon-btn"
              data-variant="bare"
              data-size="sm"
              aria-label={`Stop ${v.agent}`}
              onClick={() => {
                setAsking(false);
                act("stop");
              }}
            >
              <Check size={14} />
            </button>
            <button type="button" className="icon-btn" data-variant="bare" data-size="sm" aria-label="Keep it" onClick={() => setAsking(false)}>
              <X size={14} />
            </button>
          </span>
        ) : (
          <>
            <button
              type="button"
              className="icon-btn"
              data-variant="bare"
              data-size="sm"
              disabled={busy || v.state === "failed" || v.state === "draining"}
              title={paused ? "Resume its vCPUs (P)" : "Pause its vCPUs (P)"}
              aria-label={`${paused ? "Resume" : "Pause"} ${v.agent}`}
              onClick={() => act(paused ? "resume" : "pause")}
            >
              {paused ? <Play size={14} /> : <Pause size={14} />}
            </button>
            <button type="button" className="icon-btn" data-variant="bare" data-size="sm" disabled={busy} title="Restart (R)" aria-label={`Restart ${v.agent}`} onClick={() => act("restart")}>
              <RotateCcw size={14} />
            </button>
            <button
              type="button"
              className="icon-btn"
              data-variant="bare"
              data-size="sm"
              disabled={v.state === "failed"}
              title="Stop (S)"
              aria-label={`Stop ${v.agent}`}
              onClick={() => setAsking(true)}
            >
              <Square size={13} />
            </button>
          </>
        )}
        <button type="button" className="icon-btn" data-variant="bare" data-size="sm" aria-expanded={open} title="Telemetry (T)" aria-label={`${v.agent}'s telemetry`} onClick={onCharts}>
          <ChartLine size={14} />
        </button>
      </span>
      {/* Its telemetry: the last hour, each plot the row's full width. */}
      {open && (
        <div className="rv-charts">
          {(
            [
              ["CPU", v.cpu, 100, `${cpu}%`],
              ["Memory", v.mem ?? [], 100, `${Math.round((v.memory.used / v.memory.total) * 100)}%`],
              ["Network", v.net ?? [], undefined, `${(((v.net ?? []).at(-1) ?? 0) / 10).toFixed(1)} MB/s`],
            ] as const
          ).map(([label, values, max, now]) => (
            <span key={label} className="rv-plot">
              <span className="rv-plot-head">
                <span className="label">{label} · last hour</span>
                <span className="rv-dim">{now}</span>
              </span>
              <Sparkline values={[...values]} max={max} width={480} height={40} className="rv-wide" />
            </span>
          ))}
        </div>
      )}
    </li>
  );
}

/** A warm pool: kiln names it, Shards sizes it; its warm hosts as dots, its hits as a bar, its idle time; released in place. */
function PoolRow({ p, manage }: { p: WarmPool; manage: boolean }) {
  const [asking, setAsking] = React.useState(false);
  return (
    <li className="rv-row" data-state={p.leaving_s !== null ? "leaving" : "kept"}>
      <StatusDot status={p.leaving_s !== null ? "timed_out" : "succeeded"} title={p.leaving_s !== null ? `Released in ${ago(p.leaving_s)}: no starts for its keep window` : "Kept"} />
      <span className="rv-name" title={p.agent}>
        {p.agent}
      </span>
      <span className="rv-dim rv-image" title={`${p.image} · ${p.cpus} vCPU · ${formatBytes(p.memory)}`}>
        {p.cpus}c · {Math.round(p.memory / 2 ** 30)}G
      </span>
      <span className="rv-meter rv-start">
        <WarmDots n={p.warm} of={p.bound} />
      </span>
      <span className="rv-dim rv-up" title="Since its last start">
        {ago(p.idle_s)}
      </span>
      <span className="rv-meter rv-cpu" title={`${p.hits} of ${p.starts} starts warm, the last day`}>
        <Meter value={p.hits} max={Math.max(1, p.starts)} label="Starts warm" />
        <span>{p.starts ? Math.round((p.hits / p.starts) * 100) : 0}%</span>
      </span>
      <span className="rv-mem" />
      <span className="rv-acts">
        {manage &&
          (asking ? (
            <span className="rv-confirm" role="group" aria-label={`Release ${p.agent}'s pool?`}>
              Release?
              <button
                type="button"
                className="icon-btn"
                data-variant="bare"
                data-size="sm"
                aria-label="Release it"
                onClick={() => {
                  setAsking(false);
                  toast({ title: `Released ${p.agent}'s pool`, tone: "info" });
                }}
              >
                <Check size={14} />
              </button>
              <button type="button" className="icon-btn" data-variant="bare" data-size="sm" aria-label="Keep it" onClick={() => setAsking(false)}>
                <X size={14} />
              </button>
            </span>
          ) : (
            <button type="button" className="icon-btn" data-variant="bare" data-size="sm" title="Release" aria-label={`Release ${p.agent}'s pool`} onClick={() => setAsking(true)}>
              <X size={14} />
            </button>
          ))}
      </span>
    </li>
  );
}

function ImageRow({ im }: { im: RunnerImage }) {
  const [asking, setAsking] = React.useState(false);
  const status = im.state === "built" ? "succeeded" : im.state === "building" ? "running" : "failed";
  return (
    <li className="rv-row" data-state={im.state}>
      <StatusDot status={status} title={im.state === "built" ? `Built · ${im.digest}` : im.state === "building" ? "Building" : "Failed"} />
      <span className="rv-name" title={im.digest || im.name}>
        {im.name}
      </span>
      <span className="rv-dim rv-image">{im.from}</span>
      <span className="rv-dim rv-start">{im.state === "building" ? "now" : ago(im.built_s)}</span>
      <span className="rv-up" />
      <span className="rv-meter rv-cpu">
        {im.size > 0 ? (
          <>
            <Meter value={im.size} max={2 * 2 ** 30} label="Size" />
            <span>{(im.size / 2 ** 30).toFixed(2)}G</span>
          </>
        ) : (
          <span className="rv-dim">—</span>
        )}
      </span>
      <span className="rv-mem" />
      <span className="rv-acts">
        {asking ? (
          <span className="rv-confirm" role="group" aria-label={`Remove ${im.name}?`}>
            Remove?
            <button
              type="button"
              className="icon-btn"
              data-variant="bare"
              data-size="sm"
              aria-label="Remove it"
              onClick={() => {
                setAsking(false);
                toast({ title: `Removed ${im.from}`, tone: "info" });
              }}
            >
              <Check size={14} />
            </button>
            <button type="button" className="icon-btn" data-variant="bare" data-size="sm" aria-label="Keep it" onClick={() => setAsking(false)}>
              <X size={14} />
            </button>
          </span>
        ) : (
          <>
            <button
              type="button"
              className="icon-btn"
              data-variant="bare"
              data-size="sm"
              title="Build again"
              aria-label={`Build ${im.from} again`}
              onClick={() => toast({ title: `Building ${im.from}`, tone: "running" })}
            >
              <RotateCcw size={14} />
            </button>
            <button type="button" className="icon-btn" data-variant="bare" data-size="sm" title="Remove" aria-label={`Remove ${im.from}`} onClick={() => setAsking(true)}>
              <Trash2 size={13} />
            </button>
          </>
        )}
      </span>
      {im.state === "failed" && im.why && <p className="rv-why">{im.why}</p>}
    </li>
  );
}

/** Build an image here, from one of your Agentfiles, for this runner's platform. */
function BuildForm({ name, onDone }: { name: string; onDone: () => void }) {
  const [from, setFrom] = React.useState("rust-dev");
  return (
    <div className="rv-build">
      <select className="rv-select" value={from} aria-label="Agentfile" onChange={(e) => setFrom(e.target.value)}>
        {["rust-dev", "docs-site"].map((a) => (
          <option key={a} value={a}>
            {a}
          </option>
        ))}
      </select>
      <Button
        size="xs"
        onClick={() => {
          onDone();
          toast({ title: `Building ${from} on ${name}`, tone: "running" });
        }}
      >
        Build
      </Button>
      <Button size="xs" variant="ghost" onClick={onDone}>
        Cancel
      </Button>
    </div>
  );
}

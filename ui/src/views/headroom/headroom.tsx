/**
 * Runners → Headroom (docs/headroom.md): what a runner keeps healthy on its machine, how much heavy
 * work it lets run at full speed, and how it slows the rest. The live part says each target's
 * latency against its goal, the window, what waits and what is paced; the form below edits the
 * runner's settings, which the server keeps and sends it (it keeps them too, server or not).
 */
import * as React from "react";
import { headroomApi } from "@/api/client";
import { useServerEvents } from "@/api/events";
import type { Agent, HeadroomSettings, HeadroomTarget, HeadroomTargetStatus, HeadroomView } from "@/api/types";
import { Button } from "@/components/ui/button";
import { Hold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { SectionHeading } from "@/components/ui/misc";
import { errorMessage } from "@/lib/hooks";
import { getPref, setPref } from "@/lib/prefs";
import { useSuspenseResource } from "@/lib/resource";
import { RelTime } from "@/views/shared";
import { Field, Toggle } from "../front-door/front-door";
import "../front-door/front-door.css";

const PICKED = "kiln.headroom.runner";

export function HeadroomSection({ agents, index }: { agents: Agent[]; index: string }) {
  const runners = React.useMemo(() => agents.filter((a) => !a.ephemeral).sort((a, b) => a.name.localeCompare(b.name)), [agents]);
  const [picked, setPicked] = React.useState(() => getPref(PICKED, ""));
  const runner = runners.some((a) => a.name === picked) ? picked : (runners.find((a) => a.connected)?.name ?? runners[0]?.name ?? "");
  const selectId = React.useId();
  return (
    <section className="fd" aria-labelledby="hr-title">
      <SectionHeading index={index} label="Headroom" title={<span id="hr-title">Keep machines responsive</span>} />
      <p className="fd-lead">
        Each runner checks what must stay healthy on its machine (its disk&apos;s syncs, the kiln server, anything you add) and keeps a window of how much heavy work runs at full speed: it grows
        while every target meets its goal and halves when one misses. New work past the window waits to start; work already running past it is paced, stopped part of every second. A message you
        send &quot;now&quot; never waits.
      </p>
      {runners.length === 0 ? (
        <p className="empty-line">No runners yet: add a machine first.</p>
      ) : (
        <>
          <div className="fd-runner">
            <label className="fd-key" htmlFor={selectId}>
              Runner
            </label>
            <select
              id={selectId}
              className="fd-select"
              value={runner}
              onChange={(e) => {
                const v = e.target.value;
                setPref(PICKED, v);
                React.startTransition(() => setPicked(v));
              }}
            >
              {runners.map((a) => (
                <option key={a.name} value={a.name}>
                  {a.name}
                  {a.connected ? "" : " (offline)"}
                </option>
              ))}
            </select>
          </div>
          {runner && (
            <Loadable label={`${runner}'s headroom`} fallback={<Hold height={420} className="fd-body" />}>
              <HeadroomPanel key={runner} runner={runner} />
            </Loadable>
          )}
        </>
      )}
    </section>
  );
}

function HeadroomPanel({ runner }: { runner: string }) {
  const view = useSuspenseResource(`headroom/${runner}`, (s) => headroomApi.get(runner, s));
  useServerEvents((ev) => {
    if ((ev.type === "headroom" && ev.runner === runner) || (ev.type === "agent" && ev.agent.name === runner && ev.agent.connected !== view.data.connected)) view.reload();
  });
  const v = view.data;
  const [draft, setDraft] = React.useState<HeadroomSettings | null>(null);
  const [saving, setSaving] = React.useState(false);
  const [msg, setMsg] = React.useState<{ tone: "ok" | "error" | "quiet"; text: string } | null>(null);
  const d = draft ?? v.settings;
  const dirty = Boolean(draft && JSON.stringify(draft) !== JSON.stringify(v.settings));
  const set = (p: Partial<HeadroomSettings>) => {
    setDraft({ ...d, ...p });
    setMsg(null);
  };
  const save = async () => {
    setSaving(true);
    try {
      const next = await headroomApi.save(runner, d);
      view.setData(() => next);
      setDraft(null);
      setMsg({ tone: "ok", text: next.connected ? `Saved. ${runner} applies it now.` : `Saved. ${runner} applies it when it connects.` });
    } catch (e) {
      setMsg({ tone: "error", text: errorMessage(e) });
    } finally {
      setSaving(false);
    }
  };
  const num = (s: string) => (s.trim() === "" ? NaN : Number(s));

  return (
    <div className="fd-body">
      <Live v={v} />
      <form
        className="fd-form"
        onSubmit={(e) => {
          e.preventDefault();
          void save();
        }}
      >
        <fieldset className="fd-group">
          <legend className="fd-key">On</legend>
          <Toggle checked={d.enabled} onChange={(enabled) => set({ enabled })} label="Headroom on">
            Hold and pace work on this machine to keep its targets healthy (off: targets are still checked and shown)
          </Toggle>
          <Toggle checked={d.automatic} onChange={(automatic) => set({ automatic })} label="kiln's own targets">
            Also keep kiln&apos;s own targets healthy: a small sync on the runner&apos;s disk, and the kiln server&apos;s health
          </Toggle>
          <Toggle checked={d.escapes} onChange={(escapes) => set({ escapes })} label="Pace escaped work">
            Pace work started outside kiln&apos;s groups too (a build an agent ran with <code>systemd-run --user</code>)
          </Toggle>
        </fieldset>
        <Targets targets={d.targets} onChange={(targets) => set({ targets })} />
        <fieldset className="fd-group">
          <legend className="fd-key">Window</legend>
          <div className="fd-grid">
            <NumField label="Least" hint="Heavy groups always let run at full speed." value={d.window.min} onChange={(min) => set({ window: { ...d.window, min } })} />
            <NumField label="Most" value={d.window.max} onChange={(max) => set({ window: { ...d.window, max } })} />
            <NumField label="Start" hint="Where a restart starts: its work resumes a few at a time." value={d.window.start} onChange={(start) => set({ window: { ...d.window, start } })} />
            <NumField label="Grow every (s)" hint="One more while every target is healthy." value={d.window.grow_secs} onChange={(grow_secs) => set({ window: { ...d.window, grow_secs } })} />
            <NumField label="Cut at most every (ms)" value={d.window.cooldown_ms} onChange={(cooldown_ms) => set({ window: { ...d.window, cooldown_ms } })} />
            <NumField label="Hold a place (s)" hint="How long work let in has to show before its place goes." value={d.admit_hold_secs} onChange={(admit_hold_secs) => set({ admit_hold_secs })} />
          </div>
        </fieldset>
        <fieldset className="fd-group">
          <legend className="fd-key">Pacing</legend>
          <Toggle checked={d.pacing.enabled} onChange={(enabled) => set({ pacing: { ...d.pacing, enabled } })} label="Pace running work">
            Slow work already running past the window (off: it only waits to start)
          </Toggle>
          <div className="fd-grid">
            <NumField label="Period (ms)" value={d.pacing.period_ms} onChange={(period_ms) => set({ pacing: { ...d.pacing, period_ms } })} />
            <NumField label="Least share (%)" value={d.pacing.min_duty_pct} onChange={(min_duty_pct) => set({ pacing: { ...d.pacing, min_duty_pct } })} />
            <NumField label="Stop after (s)" hint="Missing this long at the least window: paced work stops outright." value={d.pacing.freeze_after_secs} onChange={(freeze_after_secs) => set({ pacing: { ...d.pacing, freeze_after_secs } })} />
            <NumField label="Release after (s)" hint="Healthy this long: paced work runs at full speed again." value={d.pacing.release_after_secs} onChange={(release_after_secs) => set({ pacing: { ...d.pacing, release_after_secs } })} />
          </div>
        </fieldset>
        <fieldset className="fd-group">
          <legend className="fd-key">Heavy work</legend>
          <div className="fd-grid">
            <NumField label="CPU (% of one)" value={d.heavy.cpu_pct} onChange={(cpu_pct) => set({ heavy: { ...d.heavy, cpu_pct } })} />
            <NumField label="Writes (MB/s)" value={d.heavy.write_mb_s} onChange={(write_mb_s) => set({ heavy: { ...d.heavy, write_mb_s } })} />
            <Field label="Most agent sessions" hint="Empty: the runner's own limit. Sessions a restart continues are never turned away.">
              {(id) => <input id={id} className="fd-input" inputMode="numeric" value={d.max_sessions ?? ""} onChange={(e) => set({ max_sessions: e.target.value.trim() === "" ? null : num(e.target.value) })} />}
            </Field>
          </div>
        </fieldset>
        <div className="fd-actions">
          <Button type="submit" size="xs" className="fd-btn" disabled={saving || !dirty}>
            {saving ? "Saving…" : "Save"}
          </Button>
          <Button type="button" size="xs" variant="ghost" className="fd-btn" disabled={saving || !dirty} onClick={() => setDraft(null)}>
            Revert
          </Button>
          <span className="fd-msg" role="status" data-tone={msg?.tone ?? "quiet"}>
            {msg?.text ?? (dirty ? "Not saved yet." : appliedWords(v))}
          </span>
        </div>
      </form>
    </div>
  );
}

function appliedWords(v: HeadroomView): string {
  if (!v.connected) return "The runner is offline: it keeps the settings it last had.";
  if (v.supported === false) return "This runner's kiln is too old for headroom: upgrade it.";
  if (v.applied === false) return "Waiting for the runner to apply these settings.";
  return v.customized ? "In use." : "Defaults, in use.";
}

function NumField({ label, hint, value, onChange }: { label: string; hint?: string; value: number; onChange: (v: number) => void }) {
  return (
    <Field label={label} hint={hint}>
      {(id) => <input id={id} className="fd-input" inputMode="numeric" value={Number.isFinite(value) ? String(value) : ""} onChange={(e) => onChange(e.target.value.trim() === "" ? NaN : Number(e.target.value))} />}
    </Field>
  );
}

const probeValue = (t: HeadroomTarget) => (t.probe.kind === "fsync" ? t.probe.path : t.probe.kind === "http" ? t.probe.url : t.probe.addr);

function Targets({ targets, onChange }: { targets: HeadroomTarget[]; onChange: (t: HeadroomTarget[]) => void }) {
  const put = (i: number, t: HeadroomTarget) => onChange(targets.map((x, j) => (j === i ? t : x)));
  return (
    <fieldset className="fd-group">
      <legend className="fd-key">Targets</legend>
      <p className="fd-hint">What must stay healthy beyond kiln&apos;s own: a folder another service writes to (a small sync there), an HTTP health URL, or a TCP port, each with how fast it must answer.</p>
      {targets.map((t, i) => (
        <div className="fd-grid" key={i}>
          <Field label="Name">{(id) => <input id={id} className="fd-input" value={t.name} onChange={(e) => put(i, { ...t, name: e.target.value })} />}</Field>
          <Field label="Kind">
            {(id) => (
              <select
                id={id}
                className="fd-select"
                value={t.probe.kind}
                onChange={(e) => {
                  const kind = e.target.value as HeadroomTarget["probe"]["kind"];
                  const value = probeValue(t);
                  put(i, { ...t, probe: kind === "fsync" ? { kind, path: value } : kind === "http" ? { kind, url: value } : { kind, addr: value } });
                }}
              >
                <option value="fsync">sync in a folder</option>
                <option value="http">HTTP health</option>
                <option value="tcp">TCP port</option>
              </select>
            )}
          </Field>
          <Field label={t.probe.kind === "fsync" ? "Folder" : t.probe.kind === "http" ? "URL (http://)" : "host:port"}>
            {(id) => (
              <input
                id={id}
                className="fd-input"
                value={probeValue(t)}
                spellCheck={false}
                onChange={(e) => {
                  const value = e.target.value;
                  put(i, { ...t, probe: t.probe.kind === "fsync" ? { kind: "fsync", path: value } : t.probe.kind === "http" ? { kind: "http", url: value } : { kind: "tcp", addr: value } });
                }}
              />
            )}
          </Field>
          <NumField label="Goal (ms)" value={t.goal_ms} onChange={(goal_ms) => put(i, { ...t, goal_ms })} />
          <Button type="button" size="xs" variant="ghost" className="fd-btn" onClick={() => onChange(targets.filter((_, j) => j !== i))}>
            Remove
          </Button>
        </div>
      ))}
      <Button type="button" size="xs" variant="ghost" className="fd-btn" onClick={() => onChange([...targets, { name: "", probe: { kind: "http", url: "http://" }, goal_ms: 1000 }])}>
        Add a target
      </Button>
    </fieldset>
  );
}

function targetLine(t: HeadroomTargetStatus): { state: string; text: string } {
  switch (t.state) {
    case "met":
      return { state: "on", text: `${t.ms} ms (goal ${t.goal_ms} ms)` };
    case "missed":
      return { state: "error", text: `${t.ms} ms: over its goal of ${t.goal_ms} ms` };
    case "unavailable":
      return { state: "warn", text: `not answering (${t.why}): not counted` };
    default:
      return { state: "waiting", text: "not checked yet" };
  }
}

function Live({ v }: { v: HeadroomView }) {
  const s = v.status;
  const paced = s?.groups.filter((g) => g.share < 100) ?? [];
  return (
    <div className="fd-live">
      <div className="fd-live-head">
        <span className="fd-key">Live</span>
        <span className="fd-when">{v.status_at ? <>reported <RelTime ts={v.status_at} /></> : !v.connected ? "runner offline" : " "}</span>
      </div>
      {!s ? (
        <p className="fd-hint">{v.supported === false ? "This runner's kiln is too old for headroom." : "Nothing reported yet."}</p>
      ) : (
        <ul className="fd-lines" aria-label="How headroom is doing">
          <li className="fd-line" data-state={!s.enabled ? "off" : s.healthy ? "on" : "error"}>
            <i className="fd-dot" data-state={!s.enabled ? "off" : s.healthy ? "on" : "error"} aria-hidden="true" />
            <span className="fd-name">Window</span>
            <span className="fd-text">
              {!s.enabled ? "off" : `${s.allowed} heavy at full speed (${s.window}); ${s.held} waiting, ${s.starting} starting${s.pacing === "none" ? "; this machine can't pace, only hold" : ""}`}
            </span>
          </li>
          {s.targets.map((t) => {
            const l = targetLine(t);
            return (
              <li key={t.name} className="fd-line" data-state={l.state}>
                <i className="fd-dot" data-state={l.state} aria-hidden="true" />
                <span className="fd-name">{t.name}</span>
                <span className="fd-text">{l.text}</span>
              </li>
            );
          })}
          {s.groups.map((g) => (
            <li key={g.label} className="fd-line" data-state={g.share < 100 ? "warn" : "on"}>
              <i className="fd-dot" data-state={g.share < 100 ? "warn" : "on"} aria-hidden="true" />
              <span className="fd-name">{g.session ? `session ${g.label.slice(0, 8)}` : g.label}</span>
              <span className="fd-text">
                {g.cpu_pct}% CPU, {g.write_mb_s} MB/s written{g.share < 100 ? (g.share === 0 ? ": stopped" : `: paced to ${g.share}%`) : ""}
              </span>
            </li>
          ))}
          {s.enabled && s.groups.length === 0 && paced.length === 0 && (
            <li className="fd-line" data-state="off">
              <i className="fd-dot" data-state="off" aria-hidden="true" />
              <span className="fd-name">Work</span>
              <span className="fd-text">nothing heavy running</span>
            </li>
          )}
        </ul>
      )}
    </div>
  );
}

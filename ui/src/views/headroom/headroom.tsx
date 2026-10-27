/**
 * Runners → Headroom (docs/headroom.md): what a runner keeps healthy on its machine, what it
 * learned of it, and what it is doing about it. The live part shows each target's calm latency
 * against its tolerance, the write and CPU budgets between the levels learned safe and unsafe,
 * the work waiting to start and the work slowed, each with its reason. The form edits only what a
 * person decides (the false-alarm budget, tolerances, what headroom may touch): everything else
 * is measured.
 */
import * as React from "react";
import { headroomApi } from "@/api/client";
import { useServerEvents } from "@/api/events";
import type { Agent, HeadroomSettings, HeadroomStatus, HeadroomTarget, HeadroomView } from "@/api/types";
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
        Each runner checks what must stay healthy on its machine and learns how much of kiln&apos;s own work the machine takes before that suffers: how many writes a second, how many CPU cores. New
        work waits to start when it wouldn&apos;t fit, shortest first; work already running is slowed only for the resource that&apos;s short. Every number is measured; you only decide how many false
        alarms a day you&apos;ll accept and how slow each target may get. A message you send &quot;now&quot; never waits.
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
            Hold and slow kiln&apos;s work on this machine to keep its targets healthy (off: targets are still checked and shown)
          </Toggle>
          <Toggle checked={d.automatic} onChange={(automatic) => set({ automatic })} label="kiln's own targets">
            Also keep kiln&apos;s own targets healthy: a small sync in the runner&apos;s data folder, and the kiln server&apos;s health
          </Toggle>
          <Toggle checked={d.escapes} onChange={(escapes) => set({ escapes })} label="Pace escaped work">
            Slow work an agent started outside kiln&apos;s groups too (a build run with <code>systemd-run --user</code>); your own apps and terminals are never touched
          </Toggle>
        </fieldset>
        <fieldset className="fd-group">
          <legend className="fd-key">What you decide</legend>
          <div className="fd-grid">
            <NumField
              label="False alarms a day"
              hint="How often headroom may hold or slow work when nothing would have gone wrong. Fewer means it waits for clearer trouble."
              value={d.false_alarms_per_day}
              onChange={(false_alarms_per_day) => set({ false_alarms_per_day })}
            />
            <NumField
              label="Probe share (%)"
              hint="How much of a served target's time its checks may take. The platform's own default is 10%."
              value={Math.round(d.probe_load_share * 1000) / 10}
              onChange={(pct) => set({ probe_load_share: pct / 100 })}
            />
          </div>
        </fieldset>
        <Targets targets={d.targets} onChange={(targets) => set({ targets })} />
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
      {(id) => (
        <input
          id={id}
          className="fd-input"
          inputMode="decimal"
          value={Number.isFinite(value) ? String(value) : ""}
          onChange={(e) => onChange(e.target.value.trim() === "" ? NaN : Number(e.target.value))}
        />
      )}
    </Field>
  );
}

const probeValue = (t: HeadroomTarget) => (t.probe.kind === "fsync" ? t.probe.path : t.probe.kind === "http" ? t.probe.url : t.probe.addr);

function Targets({ targets, onChange }: { targets: HeadroomTarget[]; onChange: (t: HeadroomTarget[]) => void }) {
  const put = (i: number, t: HeadroomTarget) => onChange(targets.map((x, j) => (j === i ? t : x)));
  return (
    <fieldset className="fd-group">
      <legend className="fd-key">Targets</legend>
      <p className="fd-hint">
        What must stay healthy beyond kiln&apos;s own: a folder another service writes to (a small sync there), an HTTP health URL, or a TCP port, and how slow it may get before it&apos;s harmed
        (empty: the platform&apos;s default, 1 s).
      </p>
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
          <Field label="Tolerance (ms)">
            {(id) => (
              <input
                id={id}
                className="fd-input"
                inputMode="numeric"
                placeholder="1000"
                value={t.tolerance_ms ?? ""}
                onChange={(e) => put(i, { ...t, tolerance_ms: e.target.value.trim() === "" ? null : Number(e.target.value) })}
              />
            )}
          </Field>
          <Button type="button" size="xs" variant="ghost" className="fd-btn" onClick={() => onChange(targets.filter((_, j) => j !== i))}>
            Remove
          </Button>
        </div>
      ))}
      <Button type="button" size="xs" variant="ghost" className="fd-btn" onClick={() => onChange([...targets, { name: "", probe: { kind: "http", url: "http://" }, tolerance_ms: null }])}>
        Add a target
      </Button>
    </fieldset>
  );
}

const ms = (us: number | null | undefined) => (us == null ? "?" : us >= 1e6 ? `${(us / 1e6).toFixed(1)} s` : `${(us / 1e3).toFixed(us < 1e4 ? 1 : 0)} ms`);
const mbs = (b: number | null | undefined) => (b == null ? "no limit" : `${Math.round(b / 1e6)} MB/s`);
const cores = (c: number | null | undefined) => (c == null ? "no limit" : `${c.toFixed(1)} cores`);

function budgetLine(label: string, budget: string, safe: string | null, bad: string | null, wanted: string) {
  return `${budget} (wanted ${wanted}${safe ? `; held calm at ${safe}` : ""}${bad ? `; trouble at ${bad}` : ""})${label}`;
}

function Live({ v }: { v: HeadroomView }) {
  const s: HeadroomStatus | null = v.status;
  return (
    <div className="fd-live">
      <div className="fd-live-head">
        <span className="fd-key">Live</span>
        <span className="fd-when">
          {v.status_at ? (
            <>
              reported <RelTime ts={v.status_at} />
            </>
          ) : !v.connected ? (
            "runner offline"
          ) : (
            " "
          )}
        </span>
      </div>
      {!s ? (
        <p className="fd-hint">{v.supported === false ? "This runner's kiln is too old for headroom." : "Nothing reported yet."}</p>
      ) : (
        <ul className="fd-lines" aria-label="How headroom is doing">
          {s.targets.map((t) => {
            const state = t.alarm ? (t.alarm.harm ? "error" : "warn") : t.calm_us == null ? "waiting" : "on";
            const text =
              t.calm_us == null
                ? "learning its calm"
                : `calm ${ms(t.calm_us)}, tolerance ${ms(t.tolerance_us)}${t.alarm ? (t.alarm.harm ? ": harmed" : ": rising toward its tolerance") : ""}; checked every ${s.probe_every_ms[t.name] ?? "?"} ms`;
            return (
              <li key={t.name} className="fd-line" data-state={state}>
                <i className="fd-dot" data-state={state} aria-hidden="true" />
                <span className="fd-name">{t.name}</span>
                <span className="fd-text">{text}</span>
              </li>
            );
          })}
          <li className="fd-line" data-state={!s.enabled ? "off" : "on"}>
            <i className="fd-dot" data-state={!s.enabled ? "off" : "on"} aria-hidden="true" />
            <span className="fd-name">Writes</span>
            <span className="fd-text">{budgetLine("", mbs(s.budget_bps), s.safe_bps != null ? mbs(s.safe_bps) : null, s.unsafe_bps != null ? mbs(s.unsafe_bps) : null, mbs(s.wanted_bps))}</span>
          </li>
          <li className="fd-line" data-state={!s.enabled ? "off" : "on"}>
            <i className="fd-dot" data-state={!s.enabled ? "off" : "on"} aria-hidden="true" />
            <span className="fd-name">CPU</span>
            <span className="fd-text">
              {budgetLine(
                s.pacing === "none" ? "; this machine can't slow work, only hold it" : "",
                cores(s.cpu_budget),
                s.cpu_safe != null ? cores(s.cpu_safe) : null,
                s.cpu_unsafe != null ? cores(s.cpu_unsafe) : null,
                cores(s.wanted_cpu),
              )}
            </span>
          </li>
          {s.held.map((h) => (
            <li key={h.session} className="fd-line" data-state="waiting">
              <i className="fd-dot" data-state="waiting" aria-hidden="true" />
              <span className="fd-name">waits: {h.session.slice(0, 8)}</span>
              <span className="fd-text" title={h.why}>
                {h.why}
              </span>
            </li>
          ))}
          {s.groups
            .filter((g) => g.share < 100)
            .map((g) => (
              <li key={g.label} className="fd-line" data-state="warn">
                <i className="fd-dot" data-state="warn" aria-hidden="true" />
                <span className="fd-name">{g.session ? `session ${g.label.slice(0, 8)}` : g.label}</span>
                <span className="fd-text" title={g.why}>
                  {g.why}
                </span>
              </li>
            ))}
          {s.events.map((e, i) => (
            <li key={`e${i}`} className="fd-line" data-state="off">
              <i className="fd-dot" data-state="off" aria-hidden="true" />
              <span className="fd-name">last change</span>
              <span className="fd-text" title={e}>
                {e}
              </span>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

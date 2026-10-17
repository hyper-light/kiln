/**
 * The agent page's head, in hyper-light.dev's figure-header voice: a mono eyebrow (where you are),
 * the agent's name, a line of state, round controls. How it runs — permissions, model, effort,
 * settings — is a row of quiet chips on wide screens, and one tappable line on a phone that opens
 * a sheet, so the head stays a few lines tall.
 */
import * as React from "react";
import { Link, useNavigate } from "react-router";
import { ArrowLeft, ChevronDown, ChevronRight, MoreHorizontal, Pause, Power, RotateCcw, SlidersHorizontal, SquareTerminal } from "lucide-react";
import type { AgentInfo, Command, Meta, State, Status } from "@/api/harness";
import { Dialog, DialogContent } from "@/components/ui/dialog";
import { cn } from "@/lib/utils";
import { costText, modelLabel, tildify } from "./derive";
import { RoundButton } from "./figure/figure";
import { LimitBadge } from "./limits";
import { machineName, StateWord } from "./parts";
import type { useAgentActions } from "./actions";
import "./console.css";

interface Option {
  id: string;
  label: string;
  dangerous?: boolean;
  description?: string;
}

/** "bypassPermissions" → "Bypass permissions": a mode the harness didn't list, in words. */
const modeWords = (v: string) => {
  const w = v.replace(/([a-z])([A-Z])/g, "$1 $2").replace(/[-_]+/g, " ").toLowerCase();
  return w.charAt(0).toUpperCase() + w.slice(1);
};

const labelOf = (options: Option[], value: string, pretty: (v: string) => string = (v) => v) => options.find((o) => o.id === value)?.label ?? (value ? pretty(value) : "");

/** A chip that shows a setting and opens the native picker to change it (wide screens). */
function Chip({
  label,
  value,
  options,
  onPick,
  empty,
  pretty = (v) => v,
}: {
  label: string;
  value: string;
  options: Option[];
  onPick: (id: string, dangerous: boolean) => void;
  empty?: string;
  pretty?: (v: string) => string;
}) {
  const disabled = options.length === 0;
  return (
    <label className={cn("ag-chip", disabled && "ag-chip-off")} title={disabled ? empty : options.find((o) => o.id === value)?.description}>
      <span className="ag-chip-label">{label}</span>
      <span className="ag-chip-value">{labelOf(options, value, pretty) || "—"}</span>
      {!disabled && <ChevronDown size={12} aria-hidden="true" />}
      <select
        aria-label={label}
        value={value}
        disabled={disabled}
        onChange={(e) => {
          const o = options.find((x) => x.id === e.target.value);
          if (o) onPick(o.id, Boolean(o.dangerous));
        }}
      >
        {!options.some((o) => o.id === value) && <option value={value}>{value ? pretty(value) : "—"}</option>}
        {options.map((o) => (
          <option key={o.id} value={o.id}>
            {o.label}
            {o.dangerous ? " (no checks)" : ""}
          </option>
        ))}
      </select>
    </label>
  );
}

/** A full-width row in the phone sheet: the setting, its value, and the native picker over it. */
function SheetRow({ label, value, options, onPick, pretty = (v) => v }: { label: string; value: string; options: Option[]; onPick: (id: string) => void; pretty?: (v: string) => string }) {
  return (
    <label className={cn("rs-row", options.length === 0 && "rs-row-off")}>
      <span className="rs-label">{label}</span>
      <span className="rs-value">{labelOf(options, value, pretty) || "—"}</span>
      <ChevronDown size={14} aria-hidden="true" />
      <select aria-label={label} value={value} disabled={options.length === 0} onChange={(e) => onPick(e.target.value)}>
        {!options.some((o) => o.id === value) && <option value={value}>{value ? pretty(value) : "—"}</option>}
        {options.map((o) => (
          <option key={o.id} value={o.id}>
            {o.label}
          </option>
        ))}
      </select>
    </label>
  );
}

export function AgentHeader({
  info,
  state,
  meta,
  status,
  conn,
  cwd,
  actions,
  send,
  onUsage,
  onSettings,
  onRisky,
  onBackground,
}: {
  info: AgentInfo | null;
  state: State | null;
  meta: Meta | null;
  status: Status;
  conn: string;
  cwd: string;
  actions: ReturnType<typeof useAgentActions>;
  send: (c: Command) => void;
  onUsage: () => void;
  onSettings: () => void;
  onRisky: (mode: string) => void;
  /** Show what it left running in the background. */
  onBackground?: () => void;
}) {
  const navigate = useNavigate();
  const [sheet, setSheet] = React.useState(false);
  const features = meta?.features ?? [];
  const sessionControls = (meta?.controls ?? []).filter((c) => (c.scope ?? "session") === "session").length;
  const running = status === "running" || status === "waiting";
  const activity = state?.activity ?? info?.activity;
  const cost = state?.usage?.cost_usd ?? info?.cost_usd ?? 0;
  const plan = state?.plan ?? info?.plan;
  const approvals = state?.approvals ?? [];
  const openShell = info?.shell ? () => navigate(`/terminal?session=${encodeURIComponent(info.shell ?? "")}`) : null;
  const extra = [...(openShell ? [{ label: "Open its shell", onSelect: openShell }] : []), ...(sessionControls > 0 ? [{ label: "Settings…", onSelect: onSettings }] : [])];
  const canMode = features.includes("set_mode");
  const canModel = features.includes("set_model");
  const canEffort = features.includes("set_effort") || (meta?.efforts?.length ?? 0) > 0;
  const mode = state?.mode ?? info?.mode ?? "";
  const model = state?.model ?? info?.model ?? "";
  const effort = state?.effort ?? "";
  const modes = meta?.modes ?? [];
  const pickMode = (m: string, dangerous: boolean) => (dangerous ? onRisky(m) : send({ type: "set_mode", mode: m }));
  // The kind already names an agent that has no name of its own.
  const named = Boolean(info && info.name.trim().toLowerCase() !== info.kind_name.trim().toLowerCase());
  const summary = [canMode && labelOf(modes, mode, modeWords), canModel && labelOf(meta?.models ?? [], model, modelLabel), canEffort && labelOf(meta?.efforts ?? [], effort)].filter(Boolean).join(" · ");
  const where = [tildify(cwd), cost > 0 ? costText(cost, plan).text : ""].filter(Boolean).join(" · ");
  const hasControls = canMode || canModel || canEffort || sessionControls > 0;

  return (
    <header className="ag-head">
      <div className="ag-top">
        <div className="ag-id">
          <p className="ag-eyebrow">
            <Link to="/agents" className="ag-back">
              <ArrowLeft size={12} aria-hidden="true" /> Agents
            </Link>
            <span aria-hidden="true">/</span>
            {named && (
              <>
                <span>{info?.kind_name}</span>
                <span aria-hidden="true">·</span>
              </>
            )}
            {info && <span>{machineName(info.target)}</span>}
          </p>
          <h1 className="ag-title">{info?.name ?? "Agent"}</h1>
          <div className="ag-state">
            <StateWord a={{ status, approvals }} />
            {status === "running" && activity && <span className="ag-activity">{activity}</span>}
            <span className="ag-cwd ag-wide-only" title={cwd}>
              {tildify(cwd)}
            </span>
            {cost > 0 && (
              <span className="ag-cost ag-wide-only" title={costText(cost, plan).title}>
                {costText(cost, plan).text}
              </span>
            )}
            {(state?.left_behind ?? info?.left_behind ?? 0) > 0 && (
              <button type="button" className="ag-left" onClick={onBackground}>
                {state?.left_behind ?? info?.left_behind} left running
              </button>
            )}
            <LimitBadge limits={state?.limits ?? info?.limits} onClick={onUsage} />
            {conn === "reconnecting" && <span className="ag-warn">reconnecting…</span>}
          </div>
          {hasControls && (
            <button type="button" className="ag-summary" onClick={() => setSheet(true)} aria-label="How it runs: permissions, model, effort, settings">
              <span className="ag-summary-text">{summary || "Settings"}</span>
              <ChevronDown size={12} aria-hidden="true" />
            </button>
          )}
        </div>
        <div className="ag-actions">
          {running && features.includes("interrupt") && (
            <RoundButton label="Stop the current turn" onClick={() => send({ type: "interrupt" })}>
              <Pause size={15} aria-hidden="true" />
            </RoundButton>
          )}
          {status === "exited" && features.includes("resume") && (
            <RoundButton label="Restart" onClick={() => send({ type: "restart" })}>
              <RotateCcw size={15} aria-hidden="true" />
            </RoundButton>
          )}
          {openShell && (
            <RoundButton label="Open its shell" className="ag-wide" onClick={openShell}>
              <SquareTerminal size={15} aria-hidden="true" />
            </RoundButton>
          )}
          <RoundButton label={actions.live ? "End the agent" : "Remove the agent"} tone="danger" className="ag-wide" onClick={actions.end}>
            <Power size={15} aria-hidden="true" />
          </RoundButton>
          {actions.menu(
            <RoundButton label={`More for ${info?.name ?? "this agent"}`}>
              <MoreHorizontal size={16} aria-hidden="true" />
            </RoundButton>,
            extra,
          )}
        </div>
      </div>
      {hasControls && (
        <div className="ag-controls" role="group" aria-label="How it runs">
          {canMode && <Chip label="Permissions" value={mode} options={modes} pretty={modeWords} onPick={pickMode} />}
          {canModel && <Chip label="Model" value={model} options={meta?.models ?? []} pretty={modelLabel} onPick={(m) => send({ type: "set_model", model: m })} />}
          {canEffort && (
            <Chip
              label="Effort"
              value={effort}
              options={meta?.efforts ?? []}
              onPick={(e) => send({ type: "set_effort", effort: e })}
              empty="This agent's harness hasn't said which effort levels it supports yet."
            />
          )}
          {sessionControls > 0 && (
            <button type="button" className="ag-chip ag-chip-btn" onClick={onSettings}>
              <SlidersHorizontal size={13} aria-hidden="true" />
              <span className="ag-chip-value">Settings</span>
              <span className="ag-chip-n">{sessionControls}</span>
            </button>
          )}
        </div>
      )}

      <Dialog open={sheet} onOpenChange={setSheet}>
        {sheet && (
          <DialogContent title="How it runs" eyebrow={info?.name ?? "Agent"} description={where || undefined} className="ag-sheet">
            <div className="rs">
              {canMode && modes.length > 0 && (
                <section>
                  <h3 className="rs-head">Permissions</h3>
                  <div role="radiogroup" aria-label="Permissions" className="rs-opts">
                    {modes.map((m) => (
                      <button
                        key={m.id}
                        type="button"
                        role="radio"
                        aria-checked={m.id === mode}
                        className="rs-opt"
                        data-danger={m.dangerous || undefined}
                        onClick={() => {
                          setSheet(false);
                          if (m.id !== mode) pickMode(m.id, Boolean(m.dangerous));
                        }}
                      >
                        <span className="rs-opt-label">
                          {m.label}
                          {m.dangerous && <span className="rs-danger">no checks</span>}
                        </span>
                        {m.description && <span className="rs-opt-desc">{m.description}</span>}
                      </button>
                    ))}
                  </div>
                </section>
              )}
              {(canModel || canEffort) && (
                <section className="rs-rows">
                  {canModel && <SheetRow label="Model" value={model} options={meta?.models ?? []} pretty={modelLabel} onPick={(m) => send({ type: "set_model", model: m })} />}
                  {canEffort && <SheetRow label="Effort" value={effort} options={meta?.efforts ?? []} onPick={(e) => send({ type: "set_effort", effort: e })} />}
                </section>
              )}
              {sessionControls > 0 && (
                <button
                  type="button"
                  className="rs-row rs-link"
                  onClick={() => {
                    setSheet(false);
                    onSettings();
                  }}
                >
                  <span className="rs-label">Settings</span>
                  <span className="rs-value">{sessionControls}</span>
                  <ChevronRight size={14} aria-hidden="true" />
                </button>
              )}
            </div>
          </DialogContent>
        )}
      </Dialog>
    </header>
  );
}

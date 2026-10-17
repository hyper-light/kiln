/**
 * The agent page's head, in hyper-light.dev's figure-header voice: a mono eyebrow (where you are),
 * the agent's name, a line of state, round controls; and beneath, how it runs — permissions, model,
 * effort, settings — as quiet chips that open the device's own picker. The same on a phone: the
 * name wraps instead of truncating and the chip row scrolls; End and the shell move into ⋯.
 */
import { Link, useNavigate } from "react-router";
import { ArrowLeft, ChevronDown, MoreHorizontal, Pause, Power, RotateCcw, SlidersHorizontal, SquareTerminal } from "lucide-react";
import type { AgentInfo, Command, Meta, State, Status } from "@/api/harness";
import { cn } from "@/lib/utils";
import { money, tildify } from "./derive";
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

/** A chip that shows a setting and opens the native picker to change it. */
function Chip({ label, value, options, onPick, empty }: { label: string; value: string; options: Option[]; onPick: (id: string, dangerous: boolean) => void; empty?: string }) {
  const cur = options.find((o) => o.id === value);
  const disabled = options.length === 0;
  return (
    <label className={cn("ag-chip", disabled && "ag-chip-off")} title={disabled ? empty : cur?.description}>
      <span className="ag-chip-label">{label}</span>
      <span className="ag-chip-value">{cur?.label ?? (value || "—")}</span>
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
        {!options.some((o) => o.id === value) && <option value={value}>{value || "—"}</option>}
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
}) {
  const navigate = useNavigate();
  const features = meta?.features ?? [];
  const sessionControls = (meta?.controls ?? []).filter((c) => (c.scope ?? "session") === "session").length;
  const running = status === "running" || status === "waiting";
  const activity = state?.activity ?? info?.activity;
  const cost = state?.usage?.cost_usd ?? info?.cost_usd ?? 0;
  const approvals = state?.approvals ?? [];
  const openShell = info?.shell ? () => navigate(`/terminal?session=${encodeURIComponent(info.shell ?? "")}`) : null;
  const extra = [...(openShell ? [{ label: "Open its shell", onSelect: openShell }] : []), ...(sessionControls > 0 ? [{ label: "Settings…", onSelect: onSettings }] : [])];
  const showEffort = features.includes("set_effort") || (meta?.efforts?.length ?? 0) > 0;
  return (
    <header className="ag-head">
      <div className="ag-top">
        <div className="ag-id">
          <p className="ag-eyebrow">
            <Link to="/agents" className="ag-back">
              <ArrowLeft size={12} aria-hidden="true" /> Agents
            </Link>
            <span aria-hidden="true">/</span>
            <span>{info?.kind_name ?? "Agent"}</span>
            {info && (
              <>
                <span aria-hidden="true">·</span>
                <span>{machineName(info.target)}</span>
              </>
            )}
          </p>
          <h1 className="ag-title">{info?.name ?? "Agent"}</h1>
          <div className="ag-state">
            <StateWord a={{ status, approvals }} />
            {status === "running" && activity && <span className="ag-activity">{activity}</span>}
            <span className="ag-cwd" title={cwd}>
              {tildify(cwd)}
            </span>
            {cost > 0 && <span className="ag-cost">{money(cost)}</span>}
            <LimitBadge limits={state?.limits ?? info?.limits} onClick={onUsage} />
            {conn === "reconnecting" && <span className="ag-warn">reconnecting…</span>}
          </div>
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
      {(features.includes("set_mode") || features.includes("set_model") || showEffort || sessionControls > 0) && (
        <div className="ag-controls" role="group" aria-label="How it runs">
          {features.includes("set_mode") && (
            <Chip label="Permissions" value={state?.mode ?? info?.mode ?? ""} options={meta?.modes ?? []} onPick={(m, dangerous) => (dangerous ? onRisky(m) : send({ type: "set_mode", mode: m }))} />
          )}
          {features.includes("set_model") && <Chip label="Model" value={state?.model ?? info?.model ?? ""} options={meta?.models ?? []} onPick={(m) => send({ type: "set_model", model: m })} />}
          {showEffort && (
            <Chip
              label="Effort"
              value={state?.effort ?? ""}
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
    </header>
  );
}

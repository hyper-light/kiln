/**
 * The agent page's head, in hyper-light.dev's figure-header voice: a mono eyebrow (where you are),
 * the agent's name, a line of state, round controls; then how it runs — permissions, model, effort —
 * as one line of words that opens in place to change them (no popups). Settings live in ⋯.
 */
import * as React from "react";
import { Link, useNavigate } from "@/lib/router";
import { ArrowLeft, MoreHorizontal, Pause, Power, RotateCcw, Search, SquareTerminal } from "lucide-react";
import { FORCE_STOP_AFTER_MS, harnessApi, type AgentInfo, type Command, type KindInfo, type Meta, type State, type Status } from "@/api/harness";
import { useSuspenseResource } from "@/lib/resource";
import { useNow } from "@/lib/hooks";
import { Button } from "@/components/ui/button";
import { agentLabel, costText, hasOwnName, modelLabel, tildify } from "./derive";
import { useListener } from "./dom";
import { RoundButton } from "./figure/figure";
import { LimitBadge, UsagePanel } from "./limits";
import { machineName, StateWord } from "./parts";
import { RunSettings, type RunSettingsProps } from "./run-settings";
import { NameField, type useAgentActions } from "./actions";
import { Optic, PrismLabel, formatThought } from "./signals";
import "./console.css";

/** A turn under way, said plainly: what it's doing, at what effort while it thinks, and for how
 *  long, so a long think doesn't look like nothing happening. Once a stop is asked it says so, and
 *  if the turn goes on regardless it offers to force it. */
function TurnLine({ state, activity, stopAt, onForce }: { state: State | null; activity?: string; stopAt: number | null; onForce?: () => void }) {
  const now = useNow();
  const doing = state?.doing || activity || "Working";
  const thinking = /^think/i.test(doing);
  const started = state?.turn_started;
  const stuck = stopAt != null && now - stopAt > FORCE_STOP_AFTER_MS;
  return (
    <span className="ag-turn">
      <Optic state={state?.status === "waiting" ? "waiting" : thinking ? "thinking" : "tool"} />
      <PrismLabel live className="ag-turn-what">
        {stopAt == null ? doing : stuck ? "Still running" : "Stopping…"}
      </PrismLabel>
      {thinking && stopAt == null && state?.effort && <span className="ag-turn-meta">{state.effort} effort</span>}
      {started != null && <span className="ag-turn-meta">{formatThought((now - started) / 1000)}</span>}
      {stuck && onForce && (
        <Button size="xs" variant="ghost-danger" onClick={onForce}>
          Force stop
        </Button>
      )}
    </span>
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
  onBackground,
  onFind,
  stopAt,
  onStop,
  onForceStop,
}: {
  /** Find in the conversation. */
  onFind?: () => void;
  /** When a stop was asked of the turn under way (`useStopAt`), or null. */
  stopAt: number | null;
  onStop: () => void;
  /** Force a stop that didn't take. */
  onForceStop: () => void;
  info: AgentInfo | null;
  state: State | null;
  meta: Meta | null;
  status: Status;
  conn: string;
  cwd: string;
  actions: ReturnType<typeof useAgentActions>;
  send: (c: Command) => void;
  /** Everything about the session's usage and status (the inspector's Tools). */
  onUsage: () => void;
  onSettings: () => void;
  /** Show what it left running in the background. */
  onBackground?: () => void;
}) {
  const navigate = useNavigate();
  const features = meta?.features ?? [];
  const sessionControls = (meta?.controls ?? []).filter((c) => (c.scope ?? "session") === "session").length;
  const running = status === "running" || status === "waiting";
  const activity = state?.activity ?? info?.activity;
  const cost = state?.usage?.cost_usd ?? info?.cost_usd ?? 0;
  const plan = state?.plan ?? info?.plan;
  const approvals = state?.approvals ?? [];
  const left = state?.left_behind ?? info?.left_behind ?? 0;
  const openShell = info?.shell ? () => navigate(`/terminal?session=${encodeURIComponent(info.shell ?? "")}`) : null;
  const extra = [...(openShell ? [{ label: "Open its shell", onSelect: openShell }] : []), ...(sessionControls > 0 ? [{ label: "Settings…", onSelect: onSettings }] : [])];
  // Codex takes a new permission mode, model or effort from its next turn.
  const nextTurn = info?.kind === "codex" ? { mode: true, model: true, effort: true } : undefined;
  // How it runs, as one line; with the other harnesses' models on its machine where it can go on in them.
  const line: RunSettingsProps = {
    className: "ag-run",
    modes: meta?.modes ?? [],
    mode: state?.mode ?? info?.mode ?? "",
    onMode: features.includes("set_mode") ? (m) => send({ type: "set_mode", mode: m }) : undefined,
    models: meta?.models ?? [],
    model: state?.switching?.model ?? state?.model ?? info?.model ?? "",
    onModel: features.includes("set_model") ? (m, kind) => send(kind && kind !== info?.kind ? { type: "set_model", model: m, kind } : { type: "set_model", model: m }) : undefined,
    harness: info?.kind,
    modelLabel,
    efforts: meta?.efforts ?? [],
    effort: state?.effort ?? "",
    onEffort: features.includes("set_effort") || (meta?.efforts?.length ?? 0) > 0 ? (e) => send({ type: "set_effort", effort: e }) : undefined,
    appliesNextTurn: nextTurn,
    running,
  };
  const switchTarget = features.includes("switch_harness") ? info?.target : undefined;
  // The plan's limits open in place under the line, from the usage badge.
  const limits = state?.limits ?? info?.limits ?? [];
  const [usage, setUsage] = React.useState(false);
  const usageId = React.useId();
  useListener<KeyboardEvent>(
    document,
    "keydown",
    (e) => {
      if (e.key === "Escape") setUsage(false);
    },
    { on: usage },
  );
  // The kind is said beside a label that isn't the kind's own name.
  const label = info ? agentLabel(info) : "Agent";
  const named = Boolean(info && label.toLowerCase() !== info.kind_name.trim().toLowerCase());

  return (
    <header className="ag-head">
      <p className="ag-eyebrow">
        <Link to="/agents" className="ag-back">
          <ArrowLeft size={12} aria-hidden="true" /> Agents
        </Link>
        <span aria-hidden="true">/</span>
        {named && info?.kind_name && (
          <>
            <span>{info.kind_name}</span>
            <span aria-hidden="true">·</span>
          </>
        )}
        {info && <span>{machineName(info.target)}</span>}
      </p>
      {/* The name and the buttons on one row, centred on each other. */}
      <div className="ag-top">
        <div className="ag-id">
          {actions.renaming && info ? (
            <NameField value={hasOwnName(info) ? info.name : ""} placeholder={label} onSave={actions.rename} onDone={actions.stopRenaming} className="ag-title-field" label="Agent name" />
          ) : (
            <h1 className="ag-title">
              <button
                type="button"
                className="ag-title-btn"
                onClick={actions.startRenaming}
                disabled={!info}
                aria-label={info && hasOwnName(info) ? `Rename ${label}` : `Name this agent (shown as ${label})`}
              >
                <span className={info && !hasOwnName(info) ? "ag-title-unnamed" : undefined}>{label}</span>
              </button>
            </h1>
          )}
        </div>
        <div className="ag-actions">
          {running && features.includes("interrupt") && (
            <RoundButton
              label={stopAt != null ? "Stopping…" : "Stop the current turn"}
              disabled={stopAt != null}
              onClick={onStop}
            >
              <Pause size={15} aria-hidden="true" />
            </RoundButton>
          )}
          {status === "exited" && features.includes("resume") && (
            <RoundButton label="Restart" onClick={() => send({ type: "restart" })}>
              <RotateCcw size={15} aria-hidden="true" />
            </RoundButton>
          )}
          {onFind && (
            <RoundButton label="Find in the conversation (Ctrl F)" onClick={onFind}>
              <Search size={15} aria-hidden="true" />
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
      <div className="ag-state">
        <StateWord a={{ status, approvals, pause: state?.pause ?? info?.pause }} />
        {running && <TurnLine state={state} activity={activity} stopAt={stopAt} onForce={features.includes("interrupt") ? onForceStop : undefined} />}
        <span className="ag-cwd ag-wide-only" title={cwd}>
          {tildify(cwd)}
        </span>
        {cost > 0 && (
          <span className="ag-cost ag-wide-only" title={costText(cost, plan).title}>
            {costText(cost, plan).text}
          </span>
        )}
        {left > 0 && (
          <button type="button" className="ag-left" onClick={onBackground}>
            {left} left running
          </button>
        )}
        <LimitBadge limits={limits} open={usage} controls={usageId} onClick={() => setUsage(!usage)} />
        {conn === "reconnecting" && <span className="ag-warn">reconnecting…</span>}
      </div>
      {usage && limits.length > 0 && (
        <UsagePanel
          id={usageId}
          limits={limits}
          plan={plan}
          stopAt={state?.pause_at ?? null}
          onStopAt={(percent) => send({ type: "pause_at", percent })}
          onMore={() => {
            setUsage(false);
            onUsage();
          }}
        />
      )}
      {switchTarget ? (
        <React.Suspense fallback={<RunSettings {...line} />}>
          <RunSettingsElsewhere {...line} target={switchTarget} />
        </React.Suspense>
      ) : (
        <RunSettings {...line} />
      )}
    </header>
  );
}

/** The line with the models of the other harnesses on the agent's machine it can go on in (its conversation carried over). */
function RunSettingsElsewhere({ target, ...line }: RunSettingsProps & { target: string }) {
  const kinds = useSuspenseResource(`harness-kinds/${target}`, () => harnessApi.kinds(target).catch((): KindInfo[] => []), { pollMs: 120_000 });
  const elsewhere = kinds.data.filter((k) => k.switch && k.available && k.id !== line.harness).map((k) => ({ kind: k.id, name: k.name, models: k.models ?? [] }));
  return <RunSettings {...line} elsewhere={elsewhere} />;
}

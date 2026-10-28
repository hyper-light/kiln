/**
 * The agent page's head, in hyper-light.dev's figure-header voice: a mono eyebrow (where you are),
 * the agent's name, state on the stop control's border; then how it runs — permissions, model, effort,
 * and its speed where the model goes faster — as one line that changes them in place (no popups).
 * Settings live in ⋯.
 */
import * as React from "react";
import { Link, useNavigate } from "@/lib/router";
import { ArrowLeft, MoreHorizontal, Pause, Power, RotateCcw, Search, SquareTerminal } from "lucide-react";
import { FORCE_STOP_AFTER_MS, harnessApi, type AgentInfo, type Command, type KindInfo, type Meta, type State, type Status } from "@/api/harness";
import { useSuspenseResource } from "@/lib/resource";
import { agentLabel, hasOwnName, modelLabel } from "./derive";
import { useClock, useListener } from "./dom";
import { RoundButton } from "./figure/figure";
import { AtLimit } from "./elsewhere";
import { LimitBadge, topLimit, UsagePanel } from "./limits";
import { machineName } from "./parts";
import { modelIn, RunSettings, type RunSettingsProps } from "./run-settings";
import { NameField, type useAgentActions } from "./actions";
import { formatThought } from "./signals";
import { kilnAtWork, kilnSubagentsOf, subagentsAtWork, subagentTasksAtWork, workingThrough } from "./tasks";
import { harnessSessionsRes, useRes } from "@/lib/data";
import "./console.css";

type BorderState = "active" | "stopped" | "finished" | "ready" | "disconnected";

function turnSignal(status: Status, conn: string, state: State | null, info: AgentInfo | null, kiln: number): { tone: BorderState; label: string } {
  if (status === "disconnected" || (conn !== "open" && status !== "exited")) return { tone: "disconnected", label: conn === "reconnecting" ? "Reconnecting…" : "Disconnected" };
  if (state?.pause ?? info?.pause) return { tone: "stopped", label: "Paused at a usage limit" };
  if (status === "exited") return { tone: "stopped", label: "Stopped" };
  if (status === "waiting" || (state?.approvals.length ?? info?.approvals.length ?? 0) > 0) return { tone: "ready", label: "Waiting for you" };
  if (status === "starting") return { tone: "active", label: state?.harness_session ? "Resuming…" : "Starting…" };
  if (status === "running") return { tone: "active", label: "Working" };
  // Its turn over, a subagent it sent off still works (its harness's own, or one kiln started for its goals: `kiln`).
  const subs = (state ? subagentTasksAtWork(state.tasks).length : subagentsAtWork(info?.subagents).length) + kiln;
  if (workingThrough(status, subs)) return { tone: "active", label: `Working · ${subs} subagent${subs === 1 ? "" : "s"}` };
  if (state?.last_turn) return state.last_turn.how === "done" ? { tone: "finished", label: "Finished" } : { tone: "stopped", label: "Stopped" };
  return { tone: "ready", label: "Ready" };
}

/** State lives on the existing button's border. Its shape, icon and hit area never animate. */
function TurnControl({ tone, label, action, onClick, restart = false }: { tone: BorderState; label: string; action?: string; onClick?: () => void; restart?: boolean }) {
  return (
    <>
      <RoundButton className="ag-turn-control" data-state={tone} label={action ? `${label} · ${action}` : label} disabled={!onClick} onClick={onClick}>
        {restart ? <RotateCcw size={15} aria-hidden="true" /> : <Pause size={15} aria-hidden="true" />}
      </RoundButton>
      <span className="sr-only" role="status">
        {label}
      </span>
    </>
  );
}

/** Only a pending stop needs to tick; once it stalls, the same button offers a force stop. */
function StoppingControl({ stopAt, onForce, unavailable }: { stopAt: number; onForce: () => void; unavailable: boolean }) {
  const stuck = useClock(1000) - stopAt > FORCE_STOP_AFTER_MS;
  return <TurnControl tone={unavailable ? "disconnected" : "stopped"} label={stuck ? "Still running" : "Stopping…"} action={stuck ? "Force stop" : undefined} onClick={stuck ? onForce : undefined} />;
}

/** The turn's clock, a stopwatch: it ticks on its own (the header doesn't render for it) while a
 *  turn runs, and keeps room for hours. */
function TurnTime({ started }: { started: number }) {
  const now = useClock(1000);
  return (
    <span className="ag-turn-time" title="Time in this turn" aria-label={`Time in this turn: ${formatThought((now - started) / 1000)}`}>
      {formatThought((now - started) / 1000)}
    </span>
  );
}

/** Between turns the clock stops where the last one ended. */
function TurnTook({ ms }: { ms: number }) {
  const took = formatThought(Math.max(0, ms) / 1000);
  return (
    <span className="ag-turn-time" data-stopped="" title="How long the last turn took" aria-label={`The last turn took ${took}`}>
      {took}
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
  onSettings,
  onBackground,
  onFind,
  stopAt,
  onStop,
  onForceStop,
  lastTurnFrom,
}: {
  /** When the last turn began (`lastTurnFrom`), for the stopped clock between turns. */
  lastTurnFrom?: number | null;
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
  onSettings: () => void;
  /** Show what it left running in the background. */
  onBackground?: () => void;
}) {
  const navigate = useNavigate();
  const features = meta?.features ?? [];
  const running = status === "running" || status === "waiting";
  const plan = state?.plan ?? info?.plan;
  const unavailable = status === "disconnected" || conn !== "open";
  // The subagents kiln started for its goals or plan steps at work, beside its harness's own.
  const everyone = useRes(harnessSessionsRes).data;
  const kiln = info ? kilnAtWork(kilnSubagentsOf(everyone, info.id)).length : 0;
  const signal = turnSignal(status, conn, state, info, kiln);
  // Stop can use HTTP while its socket reconnects; keep that escape available.
  const canStop = running && features.includes("interrupt");
  const canRestart = status === "exited" && features.includes("resume");
  const left = state?.left_behind ?? info?.left_behind ?? 0;
  const openShell = info?.shell ? () => navigate(`/terminal?session=${encodeURIComponent(info.shell ?? "")}`) : null;
  // Settings always: kiln's own (usage limits) are there whatever the harness offers.
  const extra = [
    ...(left > 0 && onBackground ? [{ label: `${left} left running…`, onSelect: onBackground }] : []),
    ...(openShell ? [{ label: "Open its shell", onSelect: openShell }] : []),
    { label: "Settings…", onSelect: onSettings },
  ];
  // Newer Codex adapters can interrupt and continue a turn to apply its permissions immediately.
  const permissionsLive = features.includes("permissions_live");
  const nextTurn = info?.kind === "codex" ? { mode: !permissionsLive, model: true, effort: true } : undefined;
  // How it runs, as one line; with the other harnesses' models on its machine where it can go on in them.
  // While a switch to another harness waits, how it will run there: the permissions and effort it
  // carries over, named by that harness, set once it's there; the old harness's limits aren't its.
  const switching = state?.switching;
  const line: RunSettingsProps = {
    className: "ag-run",
    modes: switching ? (switching.modes ?? []) : (meta?.modes ?? []),
    mode: switching ? (switching.mode ?? "") : (state?.mode ?? info?.mode ?? ""),
    onMode: !switching && features.includes("set_mode") ? (m) => send({ type: "set_mode", mode: m }) : undefined,
    modeCanReapply: permissionsLive && running,
    models: meta?.models ?? [],
    // A switch waiting shows as that harness's model (its own default, for none).
    model: switching ? modelIn(switching.kind, switching.model) : (state?.model ?? info?.model ?? ""),
    onModel:
      features.includes("set_model") || features.includes("switch_harness")
        ? (m, kind) => send(kind && kind !== info?.kind ? { type: "set_model", model: m, kind } : { type: "set_model", model: m })
        : undefined,
    harness: switching?.kind ?? info?.kind,
    modelLabel,
    efforts: switching ? (switching.efforts ?? []) : (meta?.efforts ?? []),
    effort: switching ? (switching.effort ?? "") : (state?.effort ?? ""),
    onEffort: !switching && (features.includes("set_effort") || (meta?.efforts?.length ?? 0) > 0) ? (e) => send({ type: "set_effort", effort: e }) : undefined,
    // The bolt, where the model goes faster: a speed is its harness's own, so none while a switch waits.
    speeds: switching ? [] : (meta?.speeds ?? []),
    speed: state?.speed ?? "",
    onSpeed: features.includes("set_speed") ? (s) => send({ type: "set_speed", speed: s }) : undefined,
    speedBlocked: state?.speed_blocked,
    speedCooldown: state?.speed_cooldown,
    appliesNextTurn: nextTurn,
    running,
  };
  const switchTarget = features.includes("switch_harness") ? info?.target : undefined;
  // The plan's limits open in place under the line, from the usage badge.
  const limits = switching ? [] : (state?.limits ?? info?.limits ?? []);
  const [usage, setUsage] = React.useState(false);
  const usageId = React.useId();
  const top = topLimit(limits);
  line.trailing = top?.used_percent != null ? <LimitBadge className="rs-v" limits={limits} open={usage} controls={usageId} onClick={() => setUsage(!usage)} /> : undefined;
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
        {info && <span title={cwd}>{machineName(info.target)}</span>}
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
          {canStop && stopAt != null ? (
            <StoppingControl stopAt={stopAt} onForce={onForceStop} unavailable={unavailable} />
          ) : (
            <TurnControl
              {...signal}
              action={canStop ? "Stop the current turn" : canRestart ? "Restart" : undefined}
              onClick={canStop ? onStop : canRestart ? () => send({ type: "restart" }) : undefined}
              restart={canRestart}
            />
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
      <div className="ag-clock-row">
        {running && state?.turn_started != null ? <TurnTime started={state.turn_started} /> : state?.last_turn && lastTurnFrom != null ? <TurnTook ms={state.last_turn.at - lastTurnFrom} /> : null}
      </div>
      {switchTarget ? (
        <React.Suspense fallback={<RunSettings {...line} />}>
          <RunSettingsElsewhere {...line} target={switchTarget} own={info?.kind} />
        </React.Suspense>
      ) : (
        <RunSettings {...line} />
      )}
      {usage && limits.length > 0 && (
        <UsagePanel
          id={usageId}
          limits={limits}
          plan={plan}
          cost={state?.usage?.cost_usd ?? info?.cost_usd}
          stopAt={state?.pause_at ?? null}
          onStopAt={(percent) => send({ type: "pause_at", percent })}
          elsewhere={
            info && (
              <React.Suspense fallback={null}>
                <AtLimit agent={info} state={state} send={send} />
              </React.Suspense>
            )
          }
        />
      )}
    </header>
  );
}

/** The line with the models of the other harnesses on the agent's machine it can go on in (its conversation carried over), the one a switch waits for among them; `own`: the harness it runs in. */
function RunSettingsElsewhere({ target, own, ...line }: RunSettingsProps & { target: string; own?: string }) {
  const kinds = useSuspenseResource(`harness-kinds/${target}`, () => harnessApi.kinds(target).catch((): KindInfo[] => []), { pollMs: 120_000 });
  const elsewhere = kinds.data.filter((k) => k.switch && k.available && k.id !== own).map((k) => ({ kind: k.id, name: k.name, models: k.models ?? [] }));
  return <RunSettings {...line} elsewhere={elsewhere} />;
}

/**
 * Attention plan 01a199e1b177752e84dad5dc990e6a15.
 * The agent page's head, in hyper-light.dev's figure-header voice: a mono eyebrow (where you are),
 * the agent's name, state on the stop control's border; then how it runs — permissions, model, effort,
 * and its speed where the model goes faster — as one line that changes them in place (no popups).
 * Settings live in ⋯. A flag says when any of its messages was refused, and opens their list in place.
 */
import * as React from "react";
import { Link, useNavigate } from "@/lib/router";
import { ArrowLeft, Flag, MoreHorizontal, Pause, Power, RotateCcw, Search, SquareTerminal } from "lucide-react";
import { FORCE_STOP_AFTER_MS, harnessApi, type AgentInfo, type Command, type KindInfo, type Meta, type State, type Status } from "@/api/harness";
import type { RefusedMessages } from "@/api/types";
import { sendingProps } from "@/lib/action";
import { useSuspenseResource } from "@/lib/resource";
import { newestFirst, refusedCounts, refusedFlag, refusedWhen, whatWhy } from "@/views/refused";
import { agentLabel, hasOwnName, modelLabel } from "./derive";
import { useClock, useListener } from "./dom";
import { RoundButton } from "./figure/figure";
import { AtLimit } from "./elsewhere";
import { LimitBadge, topLimit, UsagePanel } from "./limits";
import { machineShown } from "./parts";
import { modelIn, RunSettings, type RunSettingsProps } from "./run-settings";
import { NameField, type useAgentActions } from "./actions";
import { formatThought } from "./signals";
import { kilnAtWork, kilnDescendantsOf, withKilnSubagents } from "./tasks";
import { turnSignal, type BorderState } from "./chat-state";
import { harnessSessionsRes, useLabels, useRes } from "@/lib/data";
import "./console.css";

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

/** How long a setting picked here shows as picked before its agent says it back (most do at once). */
const SAID_BACK_MS = 3500;

/**
 * A setting as the head shows it: picked here, at once (W21-R15), until its agent says it back, or
 * hasn't within a while (then as the agent has it; a command still waiting says so, asked.tsx).
 */
function usePicked(said: string): [string, (v: string) => void] {
  const [picked, setPicked] = React.useState<{ v: string; at: number } | null>(null);
  const now = useClock(picked ? 1000 : 60_000);
  if (picked && (picked.v === said || now - picked.at > SAID_BACK_MS)) setPicked(null);
  return [picked ? picked.v : said, (v) => setPicked({ v, at: Date.now() })];
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
  useLabels();
  const navigate = useNavigate();
  const features = meta?.features ?? [];
  const running = status === "running" || status === "waiting";
  const plan = state?.plan ?? info?.plan;
  const unavailable = status === "disconnected" || conn !== "open";
  // The subagents kiln started for its goals or plan steps at work, beside its harness's own.
  const everyone = useRes(harnessSessionsRes).data;
  const descendants = info ? kilnDescendantsOf(everyone, info.id) : [];
  const kiln = kilnAtWork(descendants).length;
  // The roster carries fresh ownership updates; the session socket's initial Info can be older.
  const ownership = info ? (everyone.find((a) => a.id === info.id) ?? info) : null;
  const family = ownership ? withKilnSubagents(ownership, descendants) : null;
  const signal = turnSignal(status, conn, state, ownership, kiln, family);
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
  // Each setting picked shows at once, before its agent says it back.
  const [mode, pickMode] = usePicked(switching ? (switching.mode ?? "") : (state?.mode ?? info?.mode ?? ""));
  // A switch waiting shows as that harness's model (its own default, for none).
  const [model, pickModel] = usePicked(switching ? modelIn(switching.kind, switching.model) : (state?.model ?? info?.model ?? ""));
  const [effort, pickEffort] = usePicked(switching ? (switching.effort ?? "") : (state?.effort ?? ""));
  const [speed, pickSpeed] = usePicked(state?.speed ?? "");
  const line: RunSettingsProps = {
    className: "ag-run",
    modes: switching ? (switching.modes ?? []) : (meta?.modes ?? []),
    mode,
    onMode:
      !switching && features.includes("set_mode")
        ? (m) => {
            pickMode(m);
            send({ type: "set_mode", mode: m });
          }
        : undefined,
    modeCanReapply: permissionsLive && running,
    models: meta?.models ?? [],
    model,
    onModel:
      features.includes("set_model") || features.includes("switch_harness")
        ? (m, kind) => {
            // One in another harness is a switch, which says itself how it goes (`switching`).
            if (kind && kind !== info?.kind) {
              send({ type: "set_model", model: m, kind });
              return;
            }
            pickModel(m);
            send({ type: "set_model", model: m });
          }
        : undefined,
    harness: switching?.kind ?? info?.kind,
    modelLabel,
    efforts: switching ? (switching.efforts ?? []) : (meta?.efforts ?? []),
    effort,
    onEffort:
      !switching && (features.includes("set_effort") || (meta?.efforts?.length ?? 0) > 0)
        ? (e) => {
            pickEffort(e);
            send({ type: "set_effort", effort: e });
          }
        : undefined,
    // The bolt, where the model goes faster: a speed is its harness's own, so none while a switch waits.
    speeds: switching ? [] : (meta?.speeds ?? []),
    speed,
    onSpeed: features.includes("set_speed")
      ? (v) => {
          pickSpeed(v);
          send({ type: "set_speed", speed: v });
        }
      : undefined,
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
  // What of its messages couldn't be read (on its sockets, in its MCP calls, in its runner's updates
  // of it): the flag opens their list in place, under it.
  const refused = info?.refused;
  const flag = refusedFlag(refused);
  const [refusals, setRefusals] = React.useState(false);
  const refusalsId = React.useId();
  useListener<KeyboardEvent>(
    document,
    "keydown",
    (e) => {
      if (e.key !== "Escape") return;
      setUsage(false);
      setRefusals(false);
    },
    { on: usage || refusals },
  );
  // The kind is said beside a label that isn't the kind's own name. A name given here shows at once.
  const own = info && { ...info, name: actions.name };
  const label = own ? agentLabel(own) : "Agent";
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
        {info && <span title={cwd}>{machineShown(info.target)}</span>}
      </p>
      {/* The name and the buttons on one row, centred on each other. */}
      <div className="ag-top">
        <div className="ag-id">
          {actions.renaming && own ? (
            <NameField value={hasOwnName(own) ? own.name : ""} placeholder={label} onSave={actions.rename} onDone={actions.stopRenaming} className="ag-title-field" label="Agent name" />
          ) : (
            <h1 className="ag-title">
              <button
                type="button"
                className="ag-title-btn"
                onClick={actions.startRenaming}
                disabled={!info}
                {...sendingProps(actions.naming)}
                aria-label={own && hasOwnName(own) ? `Rename ${label}` : `Name this agent (shown as ${label})`}
              >
                <span className={own && !hasOwnName(own) ? "ag-title-unnamed" : undefined}>{label}</span>
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
          <RoundButton label={actions.live ? "End the agent" : "Remove the agent"} tone="danger" className="ag-wide" {...sendingProps(actions.removing)} onClick={actions.end}>
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
        {flag && (
          <button type="button" className="ag-refused" aria-expanded={refusals} aria-controls={refusals ? refusalsId : undefined} onClick={() => setRefusals(!refusals)}>
            <Flag size={11} aria-hidden="true" />
            {flag}
          </button>
        )}
      </div>
      {refusals && refused && flag && <RefusedList id={refusalsId} refused={refused} />}
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

/** Its messages kiln couldn't read, opened from the flag: how many by wire and code, then the newest first, each with when, what and why. */
function RefusedList({ id, refused }: { id: string; refused: RefusedMessages }) {
  const now = useClock(1000);
  return (
    <section id={id} className="ag-refused-list" aria-label="Refused messages">
      <p className="ag-refused-counts">{refusedCounts(refused)}</p>
      <ul className="ag-refused-rows">
        {newestFirst(refused).map((m, i) => (
          <li key={i} className="ag-refused-row">
            <span className="ag-refused-at">{refusedWhen(m, now)}</span>
            <span className="ag-refused-wire">
              {m.unread.wire} {m.unread.code}
            </span>
            <span className="ag-refused-why">{whatWhy(m)}</span>
          </li>
        ))}
      </ul>
    </section>
  );
}

/** The line with the models of the other harnesses on the agent's machine it can go on in (its conversation carried over), the one a switch waits for among them; `own`: the harness it runs in. */
function RunSettingsElsewhere({ target, own, ...line }: RunSettingsProps & { target: string; own?: string }) {
  const kinds = useSuspenseResource(`harness-kinds/${target}`, () => harnessApi.kinds(target).catch((): KindInfo[] => []), { pollMs: 120_000 });
  const elsewhere = kinds.data.filter((k) => k.switch && k.available && k.id !== own).map((k) => ({ kind: k.id, name: k.name, models: k.models ?? [] }));
  return <RunSettings {...line} elsewhere={elsewhere} />;
}

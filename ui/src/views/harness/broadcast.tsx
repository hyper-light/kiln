/**
 * Message many agents at once, in the page, drawn like an agent's own composer: to the agents
 * picked (checked in the list, or found by typing @ and part of a name, machine or harness) or,
 * with none picked, every one the list shows. A prompt, with files and what you say into the mic,
 * queued after the current turn, steered into it, or sent now; a slash command read in their own
 * catalogs; or one of kiln's own, typed after / like theirs and listed first (interrupt, restart,
 * allow or deny everything waiting on you, end those picked), or Stop beside Send. Their mode,
 * model and effort are the agent page's line of settings, above the box; who it goes to is the To
 * line under it. The rows are the composer's that starts an agent (new-agent.tsx), so Mission control
 * turns from one to the other with nothing moving. Commands are asked once, in a line under those;
 * the same line then says how it went, and opens to each agent's outcome.
 */
import * as React from "react";
import "./console.css";
import { ArrowUp, ChevronRight, CornerDownLeft, Paperclip, Square, X } from "lucide-react";
import { clientId, harnessApi, type AgentInfo, type BroadcastBody, type BroadcastPreview, type BroadcastReport, type Command, type SendMode, type SlashCommand } from "@/api/harness";
import { Button } from "@/components/ui/button";
import { foldChips, sameFold, UNFOLDED, type Fold } from "@/components/ui/facet-fold";
import { Loadable } from "@/components/ui/loading";
import { toast } from "@/components/ui/toast";
import { Tip } from "@/components/ui/tooltip";
import { sendingProps, useActions } from "@/lib/action";
import { labelOf, useLabels } from "@/lib/data";
import { errorMessage, useMediaQuery } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";
import { cn } from "@/lib/utils";
import { CommandList, matchCommands, parseSlash } from "./palette";
import { MODES, SendModes } from "./composer";
import { ComposeField } from "./compose-field";
import { commandNames } from "./marks";
import { useAttachments } from "./controls";
import { agentLabel } from "./derive";
import { machineName, machineShown } from "./parts";
import { MicButton } from "./voice";
import { RunSettings } from "./run-settings";
import { markAnswered, unmark } from "./answered";
import { Slide } from "./bulk";
import { throughSubagents } from "./tasks";

const machineLabel = (m: string) => (m === "local" ? "server" : labelOf(m));
/** Enter sends with a keyboard and pointer; on a touch screen it is a new line (as in the composer). */
const fine = () => window.matchMedia?.("(pointer: fine)").matches;
const count = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;

interface Ask {
  ask: string;
  label: string;
  danger?: boolean;
  /** Who it's for: their ids, joined. */
  to: string;
  run: () => Promise<unknown>;
}

/** Who the message goes to, in words: "2 selected", "5 shown", "all 7". */
export type Reach = "selected" | "shown" | "all";

type BarProps = {
  /** Who it goes to: those picked, or (none picked) every one the list shows. */
  agents: AgentInfo[];
  reach: Reach;
  onEnded?: () => void;
  /** Every agent, to find by typing @ (picking one adds it to those picked). */
  all?: AgentInfo[];
  /** Pick (or put back) agents: the list's own selection. */
  onPick?: (ids: string[], on: boolean) => void;
  /** What's written, when Mission control keeps it (it stays as the composer turns to starting an agent and back). */
  text?: string;
  onText?: (t: string | ((was: string) => string)) => void;
  /** Unselect them all: the composer turns to starting an agent. */
  onClear?: () => void;
  autoFocus?: boolean;
  /** What's said under the To line: Mission control's word about its machines, as under the box that starts an agent. */
  under?: React.ReactNode;
};

/**
 * Those picked, on the To line's one line, in the order they were picked: as many as fit, the rest
 * folded into "+N more" after them (facet-fold.ts, as a filter field's chips). The line never wraps,
 * so the composer keeps its height however many are picked, and picking more moves no chip already there.
 */
function ToChips({ picked, onPick }: { picked: AgentInfo[]; onPick?: (ids: string[], on: boolean) => void }) {
  const [fold, setFold] = React.useState<Fold>(UNFOLDED);
  // A new ref when those picked change: it measures once they're laid out, then whenever the line
  // or a chip changes size (the window, a font arriving).
  const key = picked.map((a) => `${a.id}:${agentLabel(a)}`).join("\n");
  const fit = React.useCallback(
    (line: HTMLDivElement | null) => {
      const copy = line?.querySelector<HTMLElement>(".ms-to-measure");
      if (!line || !copy) return;
      const update = () => {
        const widths = Array.from(copy.children, (el) => el.getBoundingClientRect().width);
        const all = widths.pop() ?? 0;
        const more = widths.pop() ?? 0;
        const gap = parseFloat(getComputedStyle(line).columnGap) || 0;
        const next = foldChips({ line: line.clientWidth, chips: widths, more, count: all, typed: 0, gap });
        setFold((was) => (sameFold(was, next) ? was : next));
      };
      // First as its observer hears, once they're laid out (before they're painted): measured as
      // they're put in, the whole page would be laid out there and then.
      const watch = new ResizeObserver(update);
      watch.observe(line);
      for (const el of copy.children) watch.observe(el);
      return () => watch.disconnect();
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [key],
  );
  const shown = Math.min(fold.shown, picked.length);
  const folded = picked.length - shown;
  const chip = (a: AgentInfo, cut?: number) => (
    <span key={a.id} className="ms-to-chip" style={cut ? { maxWidth: cut } : undefined}>
      <span className="hx-trunc">{agentLabel(a)}</span>
      {onPick && (
        <button type="button" aria-label={`Not to ${agentLabel(a)}`} onClick={() => onPick([a.id], false)}>
          <X size={11} />
        </button>
      )}
    </span>
  );
  return (
    <div className="ms-to-chips" ref={fit}>
      {picked.slice(0, shown).map((a, i) => chip(a, i === 0 ? fold.cut : undefined))}
      {folded > 0 && <span className="ms-to-more">{shown === 0 && fold.count === "words" ? count(picked.length, "agent") : `+${folded} more`}</span>}
      {/* Unseen: each chip at its own width, then "+N more" at its widest and the words for them all, what the fold is measured from. */}
      <div className="ms-to-measure" aria-hidden="true">
        {picked.map((a) => (
          <span key={a.id} className="ms-to-chip">
            <span className="hx-trunc">{agentLabel(a)}</span>
            {onPick && <span className="ms-to-x" />}
          </span>
        ))}
        <span className="ms-to-more">+{picked.length} more</span>
        <span className="ms-to-more">{count(picked.length, "agent")}</span>
      </div>
    </div>
  );
}

/** "@words" being typed at the end: the words. */
export const atOf = (text: string) => text.match(/(?:^|\s)@([^\s@]*)$/)?.[1];

/** Agents a few typed letters may mean: by name, then machine, harness or what it's on. */
export function findAgents(all: AgentInfo[], q: string, not: ReadonlySet<string>): AgentInfo[] {
  const w = q.toLowerCase();
  const score = (a: AgentInfo) => {
    const name = agentLabel(a).toLowerCase();
    if (!w) return 3;
    if (name.startsWith(w)) return 0;
    if (name.split(/[\s\-_/:]+/).some((p) => p.startsWith(w))) return 1;
    if (name.includes(w)) return 2;
    if ([machineShown(a.target), machineName(a.target), a.kind_name, a.goal ?? "", a.cwd].some((x) => x.toLowerCase().includes(w))) return 4;
    return -1;
  };
  return all
    .filter((a) => !not.has(a.id) && a.status !== "exited")
    .map((a) => [a, score(a)] as const)
    .filter(([, sc]) => sc >= 0)
    .sort((x, y) => x[1] - y[1] || y[0].last_activity - x[0].last_activity)
    .slice(0, 6)
    .map(([a]) => a);
}

/** What an agent is doing, in a word, beside its name in the finder. */
export const doingOf = (a: AgentInfo) => (a.approvals.length > 0 ? "needs you" : a.status === "running" || throughSubagents(a) ? "working" : a.status === "idle" ? "idle" : a.status);

/** What the agents of a roster ("id:status,…") understand; nothing for no agents, or when it can't be said. */
const previewOf = (roster: string): Promise<BroadcastPreview | null> =>
  roster ? harnessApi.broadcastPreview({ ids: roster.split(",").map((r) => r.slice(0, r.lastIndexOf(":"))) }).catch(() => null) : Promise.resolve(null);

/** One of the settings the agents are held to: Any, or a value some or all of them have. */
export function MessageBar(props: BarProps) {
  // What they understand (commands, modes, models, efforts, and how many have each): again as
  // the agents picked change, come or go. Meanwhile the bar keeps what it knew; before the first
  // answer its line of settings is there, empty, so nothing moves when the answer comes.
  const roster = React.useDeferredValue(props.agents.map((a) => `${a.id}:${a.status}`).join(","));
  return (
    <Loadable label="Loading what the agents understand" fallback={<Bar {...props} preview={null} previewing />}>
      <Previewed roster={roster} {...props} />
    </Loadable>
  );
}

function Previewed({ roster, ...bar }: BarProps & { roster: string }) {
  const { data } = useSuspenseResource(`harness/broadcast/${roster}`, () => previewOf(roster));
  return <Bar {...bar} preview={data} />;
}

/** A setting the agents can be held to: Any, or one of their catalog's (with which agents have it). */
type Tune = { mode: string; model: string; effort: string; speed: string };
const ANY: Tune = { mode: "", model: "", effort: "", speed: "" };

/** The agents that have every setting chosen (Any leaves all of them). */
function tuned(agents: AgentInfo[], tune: Tune, preview: BroadcastPreview | null): AgentInfo[] {
  const lists = { mode: preview?.modes ?? [], model: preview?.models ?? [], effort: preview?.efforts ?? [], speed: preview?.speeds ?? [] };
  const sets = (Object.keys(tune) as (keyof Tune)[]).filter((k) => tune[k]).map((k) => new Set(lists[k].find((o) => o.id === tune[k])?.agents ?? []));
  return sets.length === 0 ? agents : agents.filter((a) => sets.every((ids) => ids.has(a.id)));
}

/** The line of settings before the agents have said what they understand: its room, as the line will take it. */
const TunerLoading = () => (
  <div className="rs-tuner ms-tuner" aria-hidden="true">
    <div className="rs-top">
      <p className="rs-line" />
    </div>
  </div>
);

function Bar({
  agents: everyone,
  reach,
  onEnded,
  all = [],
  onPick,
  preview,
  previewing,
  text: given,
  onText,
  onClear,
  autoFocus,
  under,
}: BarProps & { preview: BroadcastPreview | null; previewing?: boolean }) {
  useLabels();
  const [tune, setTune] = React.useState<Tune>(ANY);
  // Who gets what's sent from here: those the selectors leave (all of them while each is Any).
  const agents = tuned(everyone, tune, preview);
  const [typed, setTyped] = React.useState("");
  const text = given ?? typed;
  const setText = onText ?? setTyped;
  const [mode, setMode] = React.useState<SendMode>("queue");
  const [found, setFound] = React.useState(0);
  const field = React.useRef<HTMLTextAreaElement | null>(null);
  const [note, setNote] = React.useState<string | null>(null);
  const attachments = useAttachments("any");
  const { files, pending } = attachments;
  const picker = React.useRef<HTMLInputElement>(null);
  const [ask, setAsk] = React.useState<Ask | null>(null);
  /** The last thing sent to each set of agents and its key, until it comes back sent. */
  const lastTry = React.useRef(new Map<string, { said: string; key: string }>());
  const [report, setReport] = React.useState<BroadcastReport | null>(null);
  /** On its way to this many agents (0: nothing is). */
  const [sending, setSending] = React.useState(0);
  const [reportOpen, setReportOpen] = React.useState(false);
  const [index, setIndex] = React.useState(0);
  const [focused, setFocused] = React.useState(false);
  // The lines under the To line slide open and shut (bulk.tsx's Slide), so the list under the
  // composer moves smoothly: while one slides shut, it says what it last said.
  const [shownAsk, setShownAsk] = React.useState<Ask | null>(null);
  if (ask && ask !== shownAsk) setShownAsk(ask);
  const askGone = React.useCallback(() => setShownAsk(null), []);
  const [shownSending, setShownSending] = React.useState(0);
  if (sending > 0 && sending !== shownSending) setShownSending(sending);
  const sendingGone = React.useCallback(() => setShownSending(0), []);
  const roomy = useMediaQuery("(min-width: 640px)");

  const ids = agents.map((a) => a.id);
  const to = ids.join(",");
  const key = everyone.map((a) => a.id).join(",");
  // A different set of agents: whatever was being asked, and the settings held, are for the old one.
  const [askedOf, setAskedOf] = React.useState(key);
  if (askedOf !== key) {
    setAskedOf(key);
    setAsk(null);
    setTune(ANY);
  }

  const n = agents.length;
  const total = everyone.length;
  const held = n < total;
  const who = held ? `${n} of ${total}` : reach === "selected" ? `${n} selected` : reach === "shown" ? `${n} shown` : n === 1 ? "the one agent" : `all ${n}`;
  const working = agents.filter((a) => a.status === "running").length;
  const waiting = agents.reduce((s, a) => s + a.approvals.filter((p) => p.tool !== "question" && p.tool !== "plan").length, 0);

  const send = async (body: Omit<BroadcastBody, "to">) => {
    // The same thing sent again to the same agents after a try that didn't come back goes under the
    // same key: each agent gets it once (CP-T3.2), its id made from the key and the agent. Anything
    // else is new. The key is for what was said, not for the list's order or whose turn it shows,
    // which change as agents start working.
    const said = JSON.stringify(body);
    const tried = lastTry.current.get(to);
    const key = tried?.said === said ? tried.key : clientId();
    lastTry.current.set(to, { said, key });
    try {
      setReport(await harnessApi.broadcastTo({ to: { ids }, ...body, key }));
      if (lastTry.current.get(to)?.key === key) lastTry.current.delete(to);
      setReportOpen(false);
      return true;
    } catch (e) {
      toast.error("Couldn't send", errorMessage(e));
      return false;
    }
  };
  // One send at a time to the same agents (W21-R15): the control pressed says it's on its way and
  // the others that would send to them wait, so each try keeps its key (CP-T3.2); a send to other
  // agents doesn't wait. Cancel still puts a line away, and what was confirmed goes on.
  const [out, setOut] = React.useState<ReadonlyMap<string, Ask | null>>(() => new Map());
  const [deliver] = useActions((to: string, a: Ask | null, go: () => Promise<unknown>) => {
    setOut((o) => new Map(o).set(to, a));
    return go().finally(() => {
      setOut((o) => {
        const left = new Map(o);
        left.delete(to);
        return left;
      });
      // Its line goes once it's done, unless another was asked since.
      if (a) setAsk((cur) => (cur === a ? null : cur));
    });
  });
  /** What's on its way to these agents: the line it was confirmed in, null for a message from the box (undefined: nothing). */
  const going = out.get(to);
  /** The same, for the agents of the line on show. */
  const lineGoing = shownAsk ? out.get(shownAsk.to) : undefined;
  const asking = (a: Omit<Ask, "to">) => setAsk({ ...a, to });
  const command = (c: Command) => send({ command: c });
  // Every tool call waiting on them, answered: shown so at once (answered.ts), back if it didn't go.
  const answerAll = async (decision: "allow" | "deny") => {
    const asked = agents.flatMap((a) => a.approvals.filter((p) => p.tool !== "question" && p.tool !== "plan").map((p) => p.id));
    markAnswered(asked);
    if (!(await send({ approvals: decision }))) unmark(asked);
  };

  // kiln's own, for all of them at once: asked in the line under the field, as any command. They
  // take no words, and their route is a command's default (they're run here, never routed).
  const own = (name: string, description: string): SlashCommand => ({ name, group: "Kiln", args: "none", description, hint: "", route: { kind: "prompt" } });
  const kiln: SlashCommand[] = [
    own("interrupt", working > 0 ? `Stop the turn of ${count(working, "working agent")}` : "Stop their turns (none is working)"),
    own("restart", `Restart ${count(n, "agent")}`),
    own("allow", waiting > 0 ? `Allow ${count(waiting, "tool call")} waiting on you` : "Allow what waits on you (nothing does)"),
    own("deny", waiting > 0 ? `Deny ${count(waiting, "tool call")} waiting on you` : "Deny what waits on you (nothing does)"),
    ...(reach === "selected" ? [own("end", `End ${count(n, "agent")}: their processes stop`)] : []),
  ];
  const kilns = (name: string) => kiln.some((c) => c.name === name);
  // Theirs and kiln's own, marked wherever they're typed, as an agent's composer marks its own. Their
  // names change only with who it goes to (End is for those picked), not with kiln's counts.
  const names = React.useMemo(
    () => commandNames([...kiln, ...(preview?.commands ?? [])]),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [reach, preview],
  );
  // The slash palette: kiln's own first, then what the agents understand, marked where only some do.
  const slash = parseSlash(text);
  const items = focused && slash?.typingName ? [...matchCommands(kiln, slash.name), ...matchCommands(preview?.commands ?? [], slash.name).filter((c) => !kilns(c.name))] : [];
  // The finder: agents to add, for "@" and what's typed after it.
  const at = focused && onPick ? atOf(text) : undefined;
  const pickedIds = React.useMemo(() => new Set(reach === "selected" ? ids : []), [reach, ids]);
  const finds = at === undefined ? [] : findAgents(all, at, pickedIds);
  const add = (a: AgentInfo) => {
    onPick?.([a.id], true);
    setText((t) => t.replace(/(^|\s)@[^\s@]*$/, "$1"));
    setFound(0);
    field.current?.focus();
  };
  const noteOf = (c: SlashCommand) => {
    const k = (c as SlashCommand & { count?: number }).count;
    return k !== undefined && k < n ? `${k} of ${n}` : undefined;
  };
  const pick = (c: SlashCommand) => {
    setIndex(0);
    if (kilns(c.name)) {
      setText("");
      choose(c.name);
      return;
    }
    setText(`/${c.name} `);
  };
  const sendText = () => {
    const body = text.trim();
    if ((!body && files.length === 0 && pending === 0) || n === 0 || going !== undefined) return;
    // One of kiln's own: asked in the line under the field, not sent.
    const typed = parseSlash(body);
    if (typed && kilns(typed.name) && !typed.args && files.length === 0 && pending === 0) {
      setText("");
      choose(typed.name);
      return;
    }
    // The field empties at once, if it still says what's sent (the line under it says it's on its
    // way), and its files go with it, those still uploading too (W21-R15); the words come back if it
    // didn't go and nothing new was typed meanwhile, and the files if nothing new was attached.
    const go = async () => {
      setText((t) => (t.trim() === body ? "" : t));
      const taken = attachments.take();
      setSending(n);
      // The settings chosen, applied to them first (each a command to every one of them).
      const settings: Command[] = [];
      if (tune.mode) settings.push({ type: "set_mode", mode: tune.mode });
      if (tune.model) settings.push({ type: "set_model", model: tune.model });
      if (tune.effort) settings.push({ type: "set_effort", effort: tune.effort });
      if (tune.speed) settings.push({ type: "set_speed", speed: tune.speed });
      let ok = true;
      for (const c of settings) ok = ok && (await send({ command: c }));
      const up = ok ? await taken.up : [];
      ok = ok && (body || up.length ? await send({ text: body, mode, attachments: up.length > 0 ? up : undefined }) : true);
      setSending(0);
      if (!ok) {
        setText((t) => t || body);
        taken.back();
      }
    };
    const risky = tune.mode ? modes.find((m) => m.id === tune.mode)?.dangerous : false;
    if (risky)
      asking({
        ask: `Switch ${count(n, "agent")} to ${modes.find((m) => m.id === tune.mode)?.label ?? tune.mode} and send? It skips permission checks.`,
        label: "Send",
        danger: true,
        run: go,
      });
    else if (mode === "now" && working > 0)
      asking({
        ask: `Stop the turn of ${count(working, "working agent")} and send this now?`,
        label: "Send now",
        run: go,
      });
    else {
      // Sent from the box instead of answering what's asked: that line goes.
      setAsk(null);
      deliver(to, null, go);
    }
  };

  const modes = preview?.modes ?? [];
  const models = preview?.models ?? [];
  const efforts = preview?.efforts ?? [];
  const speeds = preview?.speeds ?? [];
  const counted = <T extends { description?: string; count: number }>(o: T): T =>
    o.count < total ? { ...o, description: [o.description, `${o.count} of ${total} agents`].filter(Boolean).join(" · ") } : o;
  /** One of kiln's own commands (typed, picked, or Stop): asked in the line under the field, run from there. */
  const choose = (what: string) => {
    const many = count(n, "agent");
    if ((what === "interrupt" && working === 0) || ((what === "allow" || what === "deny") && waiting === 0)) {
      setNote(what === "interrupt" ? "None of them is working: there's no turn to stop." : "Nothing of theirs waits on you.");
      return;
    }
    if (what === "interrupt")
      asking({
        ask: `Stop the turn of ${count(working, "working agent")}?`,
        label: "Interrupt",
        run: () => command({ type: "interrupt" }),
      });
    else if (what === "restart")
      asking({
        ask: `Restart ${many}?`,
        label: "Restart",
        run: () => command({ type: "restart" }),
      });
    else if (what === "allow")
      asking({
        ask: `Allow ${count(waiting, "tool call")} waiting on you?`,
        label: "Allow",
        run: () => answerAll("allow"),
      });
    else if (what === "deny")
      asking({
        ask: `Deny ${count(waiting, "tool call")} waiting on you?`,
        label: "Deny",
        run: () => answerAll("deny"),
      });
    else if (what === "end")
      asking({
        ask: `End ${many}? Their processes stop on their machines.`,
        label: "End",
        danger: true,
        run: async () => {
          await Promise.allSettled(ids.map((id) => harnessApi.stop(id)));
          onEnded?.();
        },
      });
  };

  // A phone has room for who it goes to, not the hints.
  const placeholder =
    n === 0
      ? "No agent to message"
      : mode === "steer"
        ? `Steer ${who}${roomy ? ": each reads this at its next step" : "…"}`
        : mode === "now"
          ? `Stop ${who} and say…`
          : `Message ${who}${roomy ? `: a prompt, / for a command${onPick ? ", @ to pick agents" : ""}` : "…"}`;
  const outcome = report
    ? [
        report.sent ? `Sent to ${report.sent}` : "",
        report.retained ? `${report.retained} retained, waiting for receiver` : "",
        report.skipped ? `${report.skipped} skipped` : "",
        report.failed ? `${report.failed} failed` : "",
      ]
        .filter(Boolean)
        .join(" · ") || "No agent to send to"
    : "";

  const picked = reach === "selected" ? agents : [];
  const sendLabel = working > 0 ? (mode === "queue" ? "Queue" : mode === "steer" ? "Steer" : "Send now") : "Send";

  return (
    <section className="ms-msg cp" aria-label="Message agents">
      {/* What they're held to, as the agent page's line: each its own (unset) or one chosen, with which have it. Always
          there, as it is over the box that starts an agent: its room before they've said, a dash for what none of them has. */}
      {previewing ? (
        <TunerLoading />
      ) : (
        <RunSettings
          className="ms-tuner"
          modes={modes.map(counted)}
          mode={tune.mode}
          onMode={(mode) => setTune((t) => ({ ...t, mode }))}
          models={models.map(counted)}
          model={tune.model}
          onModel={(model) => setTune((t) => ({ ...t, model }))}
          efforts={efforts.map(counted)}
          effort={tune.effort}
          onEffort={(effort) => setTune((t) => ({ ...t, effort }))}
          speeds={speeds.map(counted)}
          speed={tune.speed}
          onSpeed={(speed) => setTune((t) => ({ ...t, speed }))}
          trailing={
            tune.mode || tune.model || tune.effort || tune.speed ? (
              <button type="button" className="rs-more" aria-label="Leave each agent's own permissions, model and effort" onClick={() => setTune(ANY)}>
                Reset
              </button>
            ) : undefined
          }
        />
      )}
      <div
        className="ms-msg-field"
        onDragOver={(e) => {
          if (e.dataTransfer.types.includes("Files")) e.preventDefault();
        }}
        onDrop={(e) => {
          if (e.dataTransfer.files.length === 0) return;
          e.preventDefault();
          void attachments.add(e.dataTransfer.files);
        }}
      >
        <div className="hx-compose-box">
          {attachments.chips}
          {/* The agent page's field: it grows with the text, and what's typed is marked as it is there. */}
          <ComposeField
            text={text}
            names={names}
            field={field}
            disabled={n === 0}
            autoFocus={autoFocus}
            placeholder={placeholder}
            aria-label={`Message ${who}`}
            enterKeyHint="send"
            autoCapitalize="sentences"
            onFocus={() => setFocused(true)}
            onBlur={() => setFocused(false)}
            onPaste={(e) => {
              if (e.clipboardData.files.length === 0) return;
              e.preventDefault();
              void attachments.add(e.clipboardData.files);
            }}
            onChange={(e) => {
              setText(e.target.value);
              setIndex(0);
              setFound(0);
            }}
            onKeyDown={(e) => {
              if (finds.length > 0 && (e.key === "ArrowDown" || e.key === "ArrowUp")) {
                e.preventDefault();
                setFound((i) => (i + (e.key === "ArrowDown" ? 1 : finds.length - 1)) % finds.length);
              } else if (finds.length > 0 && (e.key === "Tab" || e.key === "Enter")) {
                e.preventDefault();
                const a = finds[Math.min(found, finds.length - 1)];
                if (a) add(a);
              } else if (items.length > 0 && (e.key === "ArrowDown" || e.key === "ArrowUp")) {
                e.preventDefault();
                setIndex((i) => (i + (e.key === "ArrowDown" ? 1 : items.length - 1)) % items.length);
              } else if (items.length > 0 && (e.key === "Tab" || e.key === "Enter")) {
                e.preventDefault();
                const c = items[Math.min(index, items.length - 1)];
                if (c) pick(c);
              } else if (e.key === "Enter" && !e.nativeEvent.isComposing && (e.metaKey || e.ctrlKey || (!e.shiftKey && fine()))) {
                e.preventDefault();
                sendText();
              }
            }}
          />
          <div className="cp-bar">
            <input
              ref={picker}
              type="file"
              multiple
              hidden
              onChange={(e) => {
                if (e.target.files) void attachments.add(e.target.files);
                e.target.value = "";
              }}
            />
            <Tip content="Attach files or images">
              <button type="button" className="cp-icon" aria-label="Attach files" disabled={n === 0} onClick={() => picker.current?.click()}>
                <Paperclip size={15} />
                {files.length > 0 && <span className="cp-badge">{files.length}</span>}
              </button>
            </Tip>
            <MicButton field={field} setText={(t) => setText(t)} onNote={setNote} disabled={n === 0} />
            {working > 0 && <SendModes modes={MODES} mode={mode} onMode={setMode} />}
            <span className="cp-gap" />
            {working > 0 && (
              <Tip content={`Stop the turn of ${count(working, "working agent")}`}>
                <button type="button" className="cp-stop" disabled={n === 0 || going !== undefined} onClick={() => choose("interrupt")} aria-label={`Stop ${count(working, "working agent")}`}>
                  <Square size={10} fill="currentColor" />
                  <span className="cp-label">Stop</span>
                </button>
              </Tip>
            )}
            <button
              type="button"
              className="cp-send"
              data-mode={working > 0 ? mode : "send"}
              disabled={(!text.trim() && files.length === 0 && pending === 0) || n === 0 || !!going}
              {...sendingProps(going === null)}
              onClick={sendText}
              aria-label={`${sendLabel} to ${n}`}
            >
              {working > 0 && mode === "queue" ? <CornerDownLeft size={15} /> : <ArrowUp size={15} />}
              <span>
                {sendLabel} to {n}
              </span>
            </button>
          </div>
        </div>
        {finds.length > 0 && (
          <div className="ms-msg-pal">
            <ul className="pl-list" role="listbox" aria-label="Agents to add">
              {finds.map((a, i) => (
                <li key={a.id}>
                  <button
                    type="button"
                    role="option"
                    aria-selected={i === Math.min(found, finds.length - 1)}
                    className="pl-item"
                    onPointerDown={(e) => {
                      e.preventDefault();
                      add(a);
                    }}
                    onPointerEnter={() => setFound(i)}
                  >
                    <span className="pl-name">
                      {agentLabel(a)}
                      <span className="pl-note">{doingOf(a)}</span>
                    </span>
                    <span className="pl-desc">
                      {machineShown(a.target)} · {a.kind_name}
                    </span>
                  </button>
                </li>
              ))}
            </ul>
          </div>
        )}
        {finds.length === 0 && items.length > 0 && (
          <div className="ms-msg-pal">
            <CommandList items={items} index={Math.min(index, items.length - 1)} onPick={pick} onHover={setIndex} note={noteOf} />
          </div>
        )}
        {note && (
          <div className="pl-note ms-msg-note" role="status">
            <span>{note}</span>
            <button type="button" className="hx-icon" aria-label="Dismiss" onClick={() => setNote(null)}>
              <X size={12} />
            </button>
          </div>
        )}
      </div>
      {/* Who it goes to, under the box where More options is under the box that starts an agent: those picked
          (each put back with its ×), or everyone the list shows. */}
      <div className="ms-to">
        <span className="ms-to-label">To</span>
        {picked.length > 0 ? (
          <ToChips picked={picked} onPick={onPick} />
        ) : (
          <span className="ms-to-all">{held ? `${n} of ${total} agents` : reach === "shown" ? `the ${n} shown` : n === 1 ? "the one agent" : `all ${n} agents`}</span>
        )}
        {onClear && picked.length > 0 && (
          <Button size="xs" variant="secondary" className="mc-clear" onClick={onClear}>
            <X aria-hidden="true" />
            Clear
          </Button>
        )}
      </div>
      {under}
      {/* What's asked, then how it went, each sliding open in turn (one shuts as the other opens). */}
      {shownAsk && (
        <Slide open={ask !== null} onGone={askGone}>
          <div className="ms-msg-line" role="group" aria-label="Confirm">
            <span className="ms-msg-ask">{shownAsk.ask}</span>
            <Button
              size="xs"
              variant={shownAsk.danger ? "danger" : "primary"}
              pending={lineGoing === shownAsk}
              disabled={lineGoing !== undefined && lineGoing !== shownAsk}
              onClick={() => deliver(shownAsk.to, shownAsk, shownAsk.run)}
            >
              {shownAsk.label}
            </Button>
            <Button size="xs" variant="ghost" onClick={() => setAsk(null)}>
              Cancel
            </Button>
          </div>
        </Slide>
      )}
      {(shownSending > 0 || report) && (
        <Slide open={!ask && (sending > 0 || report !== null)} onGone={sendingGone}>
          {sending > 0 || !report ? (
            <div className="ms-msg-line" role="status">
              <span className="ms-msg-ask">Sending to {count(sending || shownSending, "agent")}…</span>
            </div>
          ) : (
            <div className="ms-msg-line">
              <button type="button" className="ms-msg-result" aria-expanded={reportOpen} onClick={() => setReportOpen(!reportOpen)}>
                <ChevronRight size={12} className={cn("ms-chev", reportOpen && "rotate-90")} aria-hidden="true" />
                {outcome}
              </button>
            </div>
          )}
        </Slide>
      )}
      {report && reportOpen && !ask && (
        <ul className="ms-msg-outcomes">
          {report.results.map((r) => (
            <li key={r.id} data-result={r.result}>
              <span className="ms-msg-o-name">{r.name}</span>
              <span className="ms-msg-o-where">{machineLabel(r.machine)}</span>
              <span className="ms-msg-o-result">{r.result}</span>
              {r.reason && <span className="ms-msg-o-why">{r.reason}</span>}
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}

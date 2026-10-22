/**
 * Message many agents at once, in the page, drawn like an agent's own composer: to the agents
 * picked (checked in the list, or found by typing @ and part of a name, machine or harness) or,
 * with none picked, every one the list shows. A prompt, with files and what you say into the mic,
 * queued after the current turn, steered into it, or sent now; a slash command read in their own
 * catalogs; or a command: interrupt, restart, a mode, model or effort, an answer to everything
 * waiting on you. Commands are asked once, in a line under the field; the same line then says how
 * it went, and opens to each agent's outcome.
 */
import * as React from "react";
import "./console.css";
import { ArrowUp, ChevronRight, CornerDownLeft, Paperclip, SlidersHorizontal, X } from "lucide-react";
import { harnessApi, type AgentInfo, type BroadcastBody, type BroadcastPreview, type BroadcastReport, type Command, type SendMode, type SlashCommand } from "@/api/harness";
import { Button } from "@/components/ui/button";
import { Loadable } from "@/components/ui/loading";
import { toast } from "@/components/ui/toast";
import { Tip } from "@/components/ui/tooltip";
import { errorMessage, useMediaQuery } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";
import { cn } from "@/lib/utils";
import { CommandList, matchCommands, parseSlash } from "./palette";
import { MODES, SendModes } from "./composer";
import { useAttachments } from "./controls";
import { agentLabel } from "./derive";
import { machineName } from "./parts";
import { MicButton } from "./voice";
import { markAnswered, unmark } from "./answered";
import { throughSubagents } from "./tasks";

const machineLabel = (m: string) => (m === "local" ? "server" : m);
/** Enter sends with a keyboard and pointer; on a touch screen it is a new line (as in the composer). */
const fine = () => window.matchMedia?.("(pointer: fine)").matches;
const count = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;

interface Ask {
  ask: string;
  label: string;
  danger?: boolean;
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
};

/** Chips for those picked, at most; past these, how many more. */
const CHIPS = 4;

/** "@words" being typed at the end: the words. */
const atOf = (text: string) => text.match(/(?:^|\s)@([^\s@]*)$/)?.[1];

/** Agents a few typed letters may mean: by name, then machine, harness or what it's on. */
function findAgents(all: AgentInfo[], q: string, not: ReadonlySet<string>): AgentInfo[] {
  const w = q.toLowerCase();
  const score = (a: AgentInfo) => {
    const name = agentLabel(a).toLowerCase();
    if (!w) return 3;
    if (name.startsWith(w)) return 0;
    if (name.split(/[\s\-_/:]+/).some((p) => p.startsWith(w))) return 1;
    if (name.includes(w)) return 2;
    if ([machineName(a.target), a.kind_name, a.goal ?? "", a.cwd].some((x) => x.toLowerCase().includes(w))) return 4;
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
const doingOf = (a: AgentInfo) => (a.approvals.length > 0 ? "needs you" : a.status === "running" || throughSubagents(a) ? "working" : a.status === "idle" ? "idle" : a.status);

/** What the agents of a roster ("id:status,…") understand; nothing for no agents, or when it can't be said. */
const previewOf = (roster: string): Promise<BroadcastPreview | null> =>
  roster ? harnessApi.broadcastPreview({ ids: roster.split(",").map((r) => r.slice(0, r.lastIndexOf(":"))) }).catch(() => null) : Promise.resolve(null);

/** One of the settings the agents are held to: Any, or a value some or all of them have. */
function TuneSelect({
  label,
  any,
  value,
  options,
  total,
  onChange,
}: {
  label: string;
  any: string;
  value: string;
  options: { id: string; label: string; count: number }[];
  total: number;
  onChange: (v: string) => void;
}) {
  if (options.length === 0) return null;
  return (
    <select className="ms-tune-select" aria-label={label} value={value} data-set={value ? "" : undefined} onChange={(e) => onChange(e.target.value)}>
      <option value="">{any}</option>
      {options.map((o) => (
        <option key={o.id} value={o.id}>
          {o.label}
          {o.count < total ? ` (${o.count} of ${total})` : ""}
        </option>
      ))}
    </select>
  );
}

export function MessageBar(props: BarProps) {
  // What they understand (commands, modes, models, efforts, and how many have each): again as
  // the agents picked change, come or go. Meanwhile the bar keeps what it knew; before the first
  // answer it shows as it does without one.
  const roster = React.useDeferredValue(props.agents.map((a) => `${a.id}:${a.status}`).join(","));
  return (
    <Loadable label="Loading what the agents understand" fallback={<Bar {...props} preview={null} />}>
      <Previewed roster={roster} {...props} />
    </Loadable>
  );
}

function Previewed({ roster, ...bar }: BarProps & { roster: string }) {
  const { data } = useSuspenseResource(`harness/broadcast/${roster}`, () => previewOf(roster));
  return <Bar {...bar} preview={data} />;
}

/** A setting the agents can be held to: Any, or one of their catalog's (with which agents have it). */
type Tune = { mode: string; model: string; effort: string };
const ANY: Tune = { mode: "", model: "", effort: "" };

/** The agents that have every setting chosen (Any leaves all of them). */
function tuned(agents: AgentInfo[], tune: Tune, preview: BroadcastPreview | null): AgentInfo[] {
  const lists = { mode: preview?.modes ?? [], model: preview?.models ?? [], effort: preview?.efforts ?? [] };
  const sets = (Object.keys(tune) as (keyof Tune)[]).filter((k) => tune[k]).map((k) => new Set(lists[k].find((o) => o.id === tune[k])?.agents ?? []));
  return sets.length === 0 ? agents : agents.filter((a) => sets.every((ids) => ids.has(a.id)));
}

function Bar({ agents: everyone, reach, onEnded, all = [], onPick, preview }: BarProps & { preview: BroadcastPreview | null }) {
  const [tune, setTune] = React.useState<Tune>(ANY);
  // Who gets what's sent from here: those the selectors leave (all of them while each is Any).
  const agents = tuned(everyone, tune, preview);
  const [text, setText] = React.useState("");
  const [mode, setMode] = React.useState<SendMode>("queue");
  const [found, setFound] = React.useState(0);
  const field = React.useRef<HTMLTextAreaElement | null>(null);
  const [note, setNote] = React.useState<string | null>(null);
  const attachments = useAttachments("any");
  const { files, pending } = attachments;
  const picker = React.useRef<HTMLInputElement>(null);
  const [choice, setChoice] = React.useState("");
  const [ask, setAsk] = React.useState<Ask | null>(null);
  const [busy, setBusy] = React.useState(false);
  const [report, setReport] = React.useState<BroadcastReport | null>(null);
  /** On its way to this many agents (0: nothing is). */
  const [sending, setSending] = React.useState(0);
  const [reportOpen, setReportOpen] = React.useState(false);
  const [index, setIndex] = React.useState(0);
  const [focused, setFocused] = React.useState(false);
  const roomy = useMediaQuery("(min-width: 640px)");

  const ids = agents.map((a) => a.id);
  const key = everyone.map((a) => a.id).join(",");
  // A different set of agents: whatever was being asked, and the settings held, are for the old one.
  const [askedOf, setAskedOf] = React.useState(key);
  if (askedOf !== key) {
    setAskedOf(key);
    setAsk(null);
    setChoice("");
    setTune(ANY);
  }

  const n = agents.length;
  const total = everyone.length;
  const held = n < total;
  const who = held ? `${n} of ${total}` : reach === "selected" ? `${n} selected` : reach === "shown" ? `${n} shown` : n === 1 ? "the one agent" : `all ${n}`;
  const working = agents.filter((a) => a.status === "running").length;
  const waiting = agents.reduce((s, a) => s + a.approvals.filter((p) => p.tool !== "question" && p.tool !== "plan").length, 0);

  const send = async (body: Omit<BroadcastBody, "to">) => {
    setBusy(true);
    try {
      setReport(await harnessApi.broadcastTo({ to: { ids }, ...body }));
      setReportOpen(false);
      if (body.attachments) attachments.clear();
      return true;
    } catch (e) {
      toast.error("Couldn't send", errorMessage(e));
      return false;
    } finally {
      setBusy(false);
      setAsk(null);
      setChoice("");
    }
  };
  const command = (c: Command) => send({ command: c });
  // Every tool call waiting on them, answered: shown so at once (answered.ts), back if it didn't go.
  const answerAll = async (decision: "allow" | "deny") => {
    const asked = agents.flatMap((a) => a.approvals.filter((p) => p.tool !== "question" && p.tool !== "plan").map((p) => p.id));
    markAnswered(asked);
    if (!(await send({ approvals: decision }))) unmark(asked);
  };

  // The slash palette: what the agents understand, marked where only some do.
  const slash = parseSlash(text);
  const items = focused && slash?.typingName ? matchCommands(preview?.commands ?? [], slash.name) : [];
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
    setText(`/${c.name} `);
    setIndex(0);
  };
  const sendText = () => {
    const body = text.trim();
    if ((!body && files.length === 0) || pending > 0 || n === 0 || busy) return;
    // The field empties at once (the line under it says it's on its way); the words come back
    // if it didn't go and nothing new was typed meanwhile.
    const go = async () => {
      setText("");
      setSending(n);
      // The settings chosen, applied to them first (each a command to every one of them).
      const settings: Command[] = [];
      if (tune.mode) settings.push({ type: "set_mode", mode: tune.mode });
      if (tune.model) settings.push({ type: "set_model", model: tune.model });
      if (tune.effort) settings.push({ type: "set_effort", effort: tune.effort });
      let ok = true;
      for (const c of settings) ok = ok && (await send({ command: c }));
      ok = ok && (await send({ text: body, mode, attachments: files.length > 0 ? attachments.payload() : undefined }));
      setSending(0);
      if (!ok) setText((t) => t || body);
    };
    const risky = tune.mode ? modes.find((m) => m.id === tune.mode)?.dangerous : false;
    if (risky)
      setAsk({
        ask: `Switch ${count(n, "agent")} to ${modes.find((m) => m.id === tune.mode)?.label ?? tune.mode} and send? It skips permission checks.`,
        label: "Send",
        danger: true,
        run: go,
      });
    else if (mode === "now" && working > 0)
      setAsk({
        ask: `Stop the turn of ${count(working, "working agent")} and send this now?`,
        label: "Send now",
        run: go,
      });
    else void go();
  };

  const modes = preview?.modes ?? [];
  const models = preview?.models ?? [];
  const efforts = preview?.efforts ?? [];
  /** A command picked from the select: asked in the line under the field, run from there. */
  const choose = (v: string) => {
    setChoice(v);
    if (!v) {
      setAsk(null);
      return;
    }
    const [what] = v.split(":");
    const many = count(n, "agent");
    if (what === "interrupt")
      setAsk({
        ask: `Stop the turn of ${count(working, "working agent")}?`,
        label: "Interrupt",
        run: () => command({ type: "interrupt" }),
      });
    else if (what === "restart")
      setAsk({
        ask: `Restart ${many}?`,
        label: "Restart",
        run: () => command({ type: "restart" }),
      });
    else if (what === "allow")
      setAsk({
        ask: `Allow ${count(waiting, "tool call")} waiting on you?`,
        label: "Allow",
        run: () => answerAll("allow"),
      });
    else if (what === "deny")
      setAsk({
        ask: `Deny ${count(waiting, "tool call")} waiting on you?`,
        label: "Deny",
        run: () => answerAll("deny"),
      });
    else if (what === "end")
      setAsk({
        ask: `End ${many}? Their processes stop on their machines.`,
        label: "End",
        danger: true,
        run: async () => {
          await Promise.allSettled(ids.map((id) => harnessApi.stop(id)));
          setAsk(null);
          setChoice("");
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
    ? [report.sent ? `Sent to ${report.sent}` : "", report.skipped ? `${report.skipped} skipped` : "", report.failed ? `${report.failed} failed` : ""].filter(Boolean).join(" · ") ||
      "No agent to send to"
    : "";

  // Grows with the text up to the stylesheet's most lines, then scrolls inside (as the composer).
  const lengthWas = React.useRef(0);
  const grow = React.useCallback(
    (el: HTMLTextAreaElement | null) => {
      field.current = el;
      if (!el) return;
      if (text.length < lengthWas.current || !text) el.style.height = "auto";
      lengthWas.current = text.length;
      const h = `${el.scrollHeight}px`;
      if (el.style.height !== h) el.style.height = h;
    },
    [text],
  );
  const picked = reach === "selected" ? agents : [];
  const sendLabel = working > 0 ? (mode === "queue" ? "Queue" : mode === "steer" ? "Steer" : "Send now") : "Send";

  return (
    <section className="ms-msg cp" aria-label="Message agents">
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
        <div className={cn("hx-compose-box", working > 0 && "hx-compose-live")}>
          {/* Who it goes to: those picked (each put back with its ×), or everyone the list shows. */}
          <div className="ms-to">
            <span className="ms-to-label">To</span>
            {picked.length > 0 ? (
              <>
                {picked.slice(0, CHIPS).map((a) => (
                  <span key={a.id} className="ms-to-chip">
                    <span className="hx-trunc">{agentLabel(a)}</span>
                    {onPick && (
                      <button type="button" aria-label={`Not to ${agentLabel(a)}`} onClick={() => onPick([a.id], false)}>
                        <X size={11} />
                      </button>
                    )}
                  </span>
                ))}
                {picked.length > CHIPS && <span className="ms-to-more">+{picked.length - CHIPS} more</span>}
              </>
            ) : (
              <span className="ms-to-all">{held ? `${n} of ${total} agents` : reach === "shown" ? `the ${n} shown` : n === 1 ? "the one agent" : `all ${n} agents`}</span>
            )}
            {(modes.length > 0 || models.length > 0 || efforts.length > 0) && (
              <span className="ms-tune">
                <TuneSelect label="Permissions" any="Any permissions" value={tune.mode} options={modes} total={total} onChange={(mode) => setTune((t) => ({ ...t, mode }))} />
                <TuneSelect label="Model" any="Any model" value={tune.model} options={models} total={total} onChange={(model) => setTune((t) => ({ ...t, model }))} />
                <TuneSelect label="Effort" any="Any effort" value={tune.effort} options={efforts} total={total} onChange={(effort) => setTune((t) => ({ ...t, effort }))} />
              </span>
            )}
          </div>
          {attachments.chips}
          <textarea
            ref={grow}
            className="hx-compose-input"
            rows={1}
            value={text}
            disabled={n === 0}
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
            {/* On a phone, an icon with the select over it: a tap opens the phone's own list. */}
            <label className="ms-cmd">
              <SlidersHorizontal size={15} className="ms-cmd-icon" aria-hidden="true" />
              <select className="ms-msg-select" aria-label="Command" value={choice} disabled={n === 0 || busy} onChange={(e) => choose(e.target.value)}>
                <option value="">Command…</option>
                <option value="interrupt" disabled={working === 0}>
                  Interrupt{working > 0 ? ` (${working} working)` : ""}
                </option>
                <option value="restart">Restart</option>
                <optgroup label="Waiting on you">
                  <option value="allow" disabled={waiting === 0}>
                    Allow {waiting > 0 ? `${waiting} ` : ""}waiting
                  </option>
                  <option value="deny" disabled={waiting === 0}>
                    Deny {waiting > 0 ? `${waiting} ` : ""}waiting
                  </option>
                </optgroup>
                {reach === "selected" && <option value="end">End {count(n, "agent")}</option>}
              </select>
            </label>
            <span className="cp-gap" />
            <button
              type="button"
              className="cp-send"
              data-mode={working > 0 ? mode : "send"}
              disabled={busy || (!text.trim() && files.length === 0) || pending > 0 || n === 0}
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
                      {machineName(a.target)} · {a.kind_name}
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
      {ask ? (
        <div className="ms-msg-line" role="group" aria-label="Confirm">
          <span className="ms-msg-ask">{ask.ask}</span>
          <Button size="xs" variant={ask.danger ? "danger" : "primary"} disabled={busy} onClick={() => void ask.run()}>
            {ask.label}
          </Button>
          <Button
            size="xs"
            variant="ghost"
            disabled={busy}
            onClick={() => {
              setAsk(null);
              setChoice("");
            }}
          >
            Cancel
          </Button>
        </div>
      ) : sending > 0 ? (
        <div className="ms-msg-line" role="status">
          <span className="ms-msg-ask">Sending to {count(sending, "agent")}…</span>
        </div>
      ) : report ? (
        <div className="ms-msg-line">
          <button type="button" className="ms-msg-result" aria-expanded={reportOpen} onClick={() => setReportOpen(!reportOpen)}>
            <ChevronRight size={12} className={cn("ms-chev", reportOpen && "rotate-90")} aria-hidden="true" />
            {outcome}
          </button>
        </div>
      ) : null}
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

/**
 * Message many agents at once, in the page, drawn like an agent's own composer: to the agents
 * checked in the list or, with none checked, every one it shows. A prompt (queued after the current
 * turn, steered into it, or sent now), a slash command read in their own catalogs, or a command:
 * interrupt, restart, a mode, model or effort, an answer to everything waiting on you. Commands are
 * asked once, in a line under the field; the same line then says how it went, and opens to each
 * agent's outcome.
 */
import * as React from "react";
import "./console.css";
import { ArrowUp, ChevronRight, CornerDownLeft } from "lucide-react";
import { harnessApi, type AgentInfo, type BroadcastBody, type BroadcastPreview, type BroadcastReport, type Command, type SendMode, type SlashCommand } from "@/api/harness";
import { Button } from "@/components/ui/button";
import { toast } from "@/components/ui/toast";
import { errorMessage, useMediaQuery } from "@/lib/hooks";
import { cn } from "@/lib/utils";
import { CommandList, matchCommands, parseSlash } from "./palette";

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

export function MessageBar({ agents, reach, onEnded }: { agents: AgentInfo[]; reach: Reach; onEnded?: () => void }) {
  const [text, setText] = React.useState("");
  const [mode, setMode] = React.useState<SendMode>("queue");
  const [picked, setPicked] = React.useState("");
  const [ask, setAsk] = React.useState<Ask | null>(null);
  const [busy, setBusy] = React.useState(false);
  const [report, setReport] = React.useState<BroadcastReport | null>(null);
  const [reportOpen, setReportOpen] = React.useState(false);
  const [preview, setPreview] = React.useState<BroadcastPreview | null>(null);
  const [index, setIndex] = React.useState(0);
  const [focused, setFocused] = React.useState(false);
  const roomy = useMediaQuery("(min-width: 640px)");

  const ids = agents.map((a) => a.id);
  const key = ids.join(",");
  // What they understand (commands, modes, models, efforts, and how many have each): again as
  // the agents picked change, come or go.
  const roster = agents.map((a) => `${a.id}:${a.status}`).join(",");
  React.useEffect(() => {
    if (!key) {
      setPreview(null);
      return;
    }
    let stop = false;
    const t = window.setTimeout(() => {
      harnessApi.broadcastPreview({ ids: key.split(",") }).then(
        (p) => {
          if (!stop) setPreview(p);
        },
        () => undefined,
      );
    }, 150);
    return () => {
      stop = true;
      window.clearTimeout(t);
    };
  }, [key, roster]);
  // A different set of agents: whatever was being asked is for the old one.
  React.useEffect(() => {
    setAsk(null);
    setPicked("");
  }, [key]);

  const n = agents.length;
  const who = reach === "selected" ? `${n} selected` : reach === "shown" ? `${n} shown` : n === 1 ? "the one agent" : `all ${n}`;
  const working = agents.filter((a) => a.status === "running").length;
  const waiting = agents.reduce((s, a) => s + a.approvals.filter((p) => p.tool !== "question" && p.tool !== "plan").length, 0);

  const send = async (body: Omit<BroadcastBody, "to">) => {
    setBusy(true);
    try {
      setReport(await harnessApi.broadcastTo({ to: { ids }, ...body }));
      setReportOpen(false);
      return true;
    } catch (e) {
      toast.error("Couldn't send", errorMessage(e));
      return false;
    } finally {
      setBusy(false);
      setAsk(null);
      setPicked("");
    }
  };
  const command = (c: Command) => send({ command: c });

  // The slash palette: what the agents understand, marked where only some do.
  const slash = parseSlash(text);
  const items = focused && slash?.typingName ? matchCommands(preview?.commands ?? [], slash.name) : [];
  const note = (c: SlashCommand) => {
    const k = (c as SlashCommand & { count?: number }).count;
    return k !== undefined && k < n ? `${k} of ${n}` : undefined;
  };
  const pick = (c: SlashCommand) => {
    setText(`/${c.name} `);
    setIndex(0);
  };
  const sendText = () => {
    const body = text.trim();
    if (!body || n === 0 || busy) return;
    const go = () => send({ text: body, mode }).then((ok) => ok && setText(""));
    if (mode === "now" && working > 0)
      setAsk({
        ask: `Stop the turn of ${count(working, "working agent")} and send this now?`,
        label: "Send now",
        run: go,
      });
    else void go();
  };

  const some = (k: number) => (k < n ? ` (${k} of ${n})` : "");
  const modes = preview?.modes ?? [];
  const models = preview?.models ?? [];
  const efforts = preview?.efforts ?? [];
  /** A command picked from the select: asked in the line under the field, run from there. */
  const choose = (v: string) => {
    setPicked(v);
    if (!v) {
      setAsk(null);
      return;
    }
    const [what, id = ""] = v.split(":");
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
    else if (what === "mode") {
      const m = modes.find((x) => x.id === id);
      setAsk({
        ask: `Switch ${many} to ${m?.label ?? id}?${m?.dangerous ? " It skips permission checks." : ""}`,
        label: "Switch",
        danger: m?.dangerous,
        run: () => command({ type: "set_mode", mode: id }),
      });
    } else if (what === "model")
      setAsk({
        ask: `Switch ${many} to ${models.find((x) => x.id === id)?.label ?? id}?`,
        label: "Switch",
        run: () => command({ type: "set_model", model: id }),
      });
    else if (what === "effort")
      setAsk({
        ask: `Set the effort of ${many} to ${efforts.find((x) => x.id === id)?.label ?? id}?`,
        label: "Set",
        run: () => command({ type: "set_effort", effort: id }),
      });
    else if (what === "allow")
      setAsk({
        ask: `Allow ${count(waiting, "tool call")} waiting on you?`,
        label: "Allow",
        run: () => send({ approvals: "allow" }),
      });
    else if (what === "deny")
      setAsk({
        ask: `Deny ${count(waiting, "tool call")} waiting on you?`,
        label: "Deny",
        run: () => send({ approvals: "deny" }),
      });
    else if (what === "end")
      setAsk({
        ask: `End ${many}? Their processes stop on their machines.`,
        label: "End",
        danger: true,
        run: async () => {
          await Promise.allSettled(ids.map((id) => harnessApi.stop(id)));
          setAsk(null);
          setPicked("");
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
          : `Message ${who}${roomy ? ": a prompt, or / for a command" : "…"}`;
  const outcome = report
    ? [report.sent ? `Sent to ${report.sent}` : "", report.skipped ? `${report.skipped} skipped` : "", report.failed ? `${report.failed} failed` : ""].filter(Boolean).join(" · ") ||
      "No agent to send to"
    : "";

  return (
    <section className="ms-msg cp" aria-label="Message agents">
      <div className="ms-msg-field">
        <div className={cn("hx-compose-box", working > 0 && "hx-compose-live")}>
          <textarea
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
            onChange={(e) => {
              setText(e.target.value);
              setIndex(0);
            }}
            onKeyDown={(e) => {
              if (items.length > 0 && (e.key === "ArrowDown" || e.key === "ArrowUp")) {
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
            <select className="ms-msg-select ms-msg-mode" aria-label="How to send to agents that are working" value={mode} onChange={(e) => setMode(e.target.value as SendMode)}>
              <option value="queue">Queue</option>
              <option value="steer">Steer</option>
              <option value="now">Now</option>
            </select>
            <select className="ms-msg-select" aria-label="Command" value={picked} disabled={n === 0 || busy} onChange={(e) => choose(e.target.value)}>
              <option value="">Command…</option>
              <option value="interrupt" disabled={working === 0}>
                Interrupt{working > 0 ? ` (${working} working)` : ""}
              </option>
              <option value="restart">Restart</option>
              {modes.length > 0 && (
                <optgroup label="Permissions">
                  {modes.map((m) => (
                    <option key={m.id} value={`mode:${m.id}`}>
                      {m.label}
                      {some(m.count)}
                    </option>
                  ))}
                </optgroup>
              )}
              {models.length > 0 && (
                <optgroup label="Model">
                  {models.map((m) => (
                    <option key={m.id} value={`model:${m.id}`}>
                      {m.label}
                      {some(m.count)}
                    </option>
                  ))}
                </optgroup>
              )}
              {efforts.length > 0 && (
                <optgroup label="Effort">
                  {efforts.map((m) => (
                    <option key={m.id} value={`effort:${m.id}`}>
                      {m.label}
                      {some(m.count)}
                    </option>
                  ))}
                </optgroup>
              )}
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
            <span className="cp-gap" />
            <button type="button" className="cp-send" data-mode={working > 0 ? mode : "send"} disabled={busy || !text.trim() || n === 0} onClick={sendText}>
              {working > 0 && mode === "queue" ? <CornerDownLeft size={15} /> : <ArrowUp size={15} />}
              <span>Send to {n}</span>
            </button>
          </div>
        </div>
        {items.length > 0 && (
          <div className="ms-msg-pal">
            <CommandList items={items} index={Math.min(index, items.length - 1)} onPick={pick} onHover={setIndex} note={note} />
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
              setPicked("");
            }}
          >
            Cancel
          </Button>
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

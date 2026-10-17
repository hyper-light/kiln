/**
 * Message many agents at once, on every machine. To: the ones selected, every agent, or a filter
 * (machine · harness · state). What: a prompt (queued after the current turn, steered into it, or
 * now), a slash command read in each agent's own catalog, a quick command (interrupt, restart,
 * mode, model, effort), or an answer to every tool call waiting on you. One line says how it went;
 * open it for each agent's outcome. Everything happens in the panel; nothing pops up.
 */
import * as React from "react";
import { ChevronRight, Send, X } from "lucide-react";
import { harnessApi, type AgentInfo, type BroadcastBody, type BroadcastPreview, type BroadcastReport, type BroadcastTo, type Command, type SendMode, type SlashCommand } from "@/api/harness";
import { Button } from "@/components/ui/button";
import { toast } from "@/components/ui/toast";
import { errorMessage } from "@/lib/hooks";
import { cn } from "@/lib/utils";
import { CommandList, matchCommands, parseSlash } from "./palette";

type Who = "selected" | "shown" | "all" | "filter";
type Choose = "mode" | "model" | "effort";

interface Filter {
  machines: string[];
  kinds: string[];
  status: string[];
}

/** The states a filter can ask for, as Mission control words them. */
const STATES = [
  { id: "running", label: "Working" },
  { id: "waiting", label: "Needs you" },
  { id: "idle", label: "Ready" },
  { id: "starting", label: "Starting" },
  { id: "exited", label: "Ended" },
];

const machineOf = (target: string) => target.replace(/^agent:/, "");
const machineLabel = (m: string) => (m === "local" ? "server" : m);
const count = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;

interface Ask {
  ask: string;
  label: string;
  danger?: boolean;
  run: () => Promise<unknown>;
}

/** One facet of the filter: its options as toggles; none chosen means any. */
export function Facet({ label, options, chosen, onChange }: { label: string; options: { id: string; label: string }[]; chosen: string[]; onChange: (next: string[]) => void }) {
  return (
    <div className="ms-bc-facet" role="group" aria-label={label}>
      <span className="ms-bc-facet-label">{label}</span>
      {options.map((o) => {
        const on = chosen.includes(o.id);
        return (
          <button key={o.id} type="button" className="ms-bc-chip" aria-pressed={on} onClick={() => onChange(on ? chosen.filter((c) => c !== o.id) : [...chosen, o.id])}>
            {o.label}
          </button>
        );
      })}
    </div>
  );
}

export function MessageMany({
  selected,
  shown,
  agents,
  onClose,
}: {
  selected: string[];
  /** The agents Mission control's filters leave, when it is filtered: "message these". */
  shown?: string[];
  agents: AgentInfo[];
  onClose: () => void;
}) {
  const [who, setWho] = React.useState<Who>(selected.length ? "selected" : "all");
  const [filter, setFilter] = React.useState<Filter>({ machines: [], kinds: [], status: [] });
  const [filterOpen, setFilterOpen] = React.useState(false);
  const [text, setText] = React.useState("");
  const [mode, setMode] = React.useState<SendMode>("queue");
  const [choose, setChoose] = React.useState<Choose | null>(null);
  const [ask, setAsk] = React.useState<Ask | null>(null);
  const [busy, setBusy] = React.useState(false);
  const [report, setReport] = React.useState<BroadcastReport | null>(null);
  const [reportOpen, setReportOpen] = React.useState(false);
  const [preview, setPreview] = React.useState<BroadcastPreview | null>(null);
  const [index, setIndex] = React.useState(0);

  // A cleared selection leaves the panel writing to everything, never to nobody.
  React.useEffect(() => {
    if ((who === "selected" && selected.length === 0) || (who === "shown" && !shown)) setWho("all");
  }, [who, selected.length, shown]);
  React.useEffect(() => {
    if (selected.length > 0) setWho("selected");
  }, [selected.length]);

  const to: BroadcastTo = React.useMemo(() => {
    if (who === "selected") return { ids: selected };
    if (who === "shown" && shown) return { ids: shown };
    if (who === "filter" && (filter.machines.length || filter.kinds.length || filter.status.length)) return { machines: filter.machines, kinds: filter.kinds, status: filter.status };
    return { all: true };
  }, [who, selected, shown, filter]);
  // Who that reaches, by the server's own picking: again as agents come, go or change state.
  const key = JSON.stringify(to);
  const roster = agents.map((a) => `${a.id}:${a.status}`).join(",");
  React.useEffect(() => {
    let stop = false;
    const t = window.setTimeout(() => {
      harnessApi.broadcastPreview(JSON.parse(key) as BroadcastTo).then(
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

  const picked = preview?.agents ?? [];
  const n = picked.length;
  const ids = new Set(picked.map((p) => p.id));
  const waiting = agents.filter((a) => ids.has(a.id)).reduce((s, a) => s + a.approvals.filter((p) => p.tool !== "question").length, 0);
  const working = picked.filter((p) => p.status === "running").length;
  const machines = [...new Set(agents.map((a) => machineOf(a.target)))];
  const kinds = [...new Map(agents.map((a) => [a.kind, a.kind_name])).entries()];
  const reach = preview ? `${count(n, "agent")} on ${count(preview.machines.length, "machine")}` : "…";

  const send = async (body: Omit<BroadcastBody, "to">) => {
    setBusy(true);
    try {
      setReport(await harnessApi.broadcastTo({ to, ...body }));
      setReportOpen(false);
      return true;
    } catch (e) {
      toast.error("Couldn't send", errorMessage(e));
      return false;
    } finally {
      setBusy(false);
      setAsk(null);
      setChoose(null);
    }
  };
  const command = (c: Command) => send({ command: c });

  // The slash palette: what the agents picked understand, marked where only some do.
  const slash = parseSlash(text);
  const items = slash?.typingName ? matchCommands(preview?.commands ?? [], slash.name) : [];
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
    if (mode === "now" && working > 0) setAsk({ ask: `Stop the turn of ${count(working, "working agent")} and send this now?`, label: "Send now", run: go });
    else void go();
  };

  const options = choose === "mode" ? (preview?.modes ?? []) : choose === "model" ? (preview?.models ?? []) : choose === "effort" ? (preview?.efforts ?? []) : [];
  const set = (id: string, dangerous?: boolean) => {
    const c: Command = choose === "mode" ? { type: "set_mode", mode: id } : choose === "model" ? { type: "set_model", model: id } : { type: "set_effort", effort: id };
    if (dangerous) setAsk({ ask: `Switch ${count(n, "agent")} to a mode that skips permission checks?`, label: "Switch", danger: true, run: () => command(c) });
    else void command(c);
  };
  const summary = [
    filter.machines.length ? filter.machines.map(machineLabel).join(" + ") : "all machines",
    filter.kinds.length ? filter.kinds.map((k) => kinds.find(([id]) => id === k)?.[1] ?? k).join(" + ") : "any harness",
    filter.status.length ? filter.status.map((s) => STATES.find((x) => x.id === s)?.label ?? s).join(" + ") : "not ended",
  ].join(" · ");

  return (
    <section className="ms-broadcast" aria-label="Message agents">
      <div className="ms-bc-head">
        <span className="ms-eyebrow">Message agents</span>
        <span className="ms-bc-reach">{reach}</span>
        <button type="button" className="hx-icon" aria-label="Close" onClick={onClose}>
          <X size={14} />
        </button>
      </div>

      <div className="ms-bc-to" role="radiogroup" aria-label="To">
        {selected.length > 0 && (
          <button type="button" role="radio" aria-checked={who === "selected"} onClick={() => setWho("selected")}>
            Selected · {selected.length}
          </button>
        )}
        {shown && (
          <button type="button" role="radio" aria-checked={who === "shown"} onClick={() => setWho("shown")}>
            Shown · {shown.length}
          </button>
        )}
        <button type="button" role="radio" aria-checked={who === "all"} onClick={() => setWho("all")}>
          Everything
        </button>
        <button
          type="button"
          role="radio"
          aria-checked={who === "filter"}
          onClick={() => {
            setWho("filter");
            setFilterOpen(true);
          }}
        >
          Filter
        </button>
      </div>
      {who === "filter" && (
        <div className="ms-bc-filter">
          <button type="button" className="ms-bc-summary" aria-expanded={filterOpen} onClick={() => setFilterOpen(!filterOpen)}>
            <ChevronRight size={12} className={cn("ms-chev", filterOpen && "rotate-90")} aria-hidden="true" />
            {summary}
          </button>
          {filterOpen && (
            <div className="ms-bc-facets">
              <Facet label="Machine" options={machines.map((m) => ({ id: m, label: machineLabel(m) }))} chosen={filter.machines} onChange={(machines) => setFilter({ ...filter, machines })} />
              <Facet label="Harness" options={kinds.map(([id, label]) => ({ id, label }))} chosen={filter.kinds} onChange={(kinds) => setFilter({ ...filter, kinds })} />
              <Facet label="State" options={STATES} chosen={filter.status} onChange={(status) => setFilter({ ...filter, status })} />
            </div>
          )}
        </div>
      )}

      <div className="ms-bc-compose">
        {items.length > 0 && <CommandList items={items} index={Math.min(index, items.length - 1)} onPick={pick} onHover={setIndex} note={note} />}
        <textarea
          className="ms-broadcast-input"
          rows={2}
          value={text}
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
            } else if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              sendText();
            }
          }}
          placeholder="A prompt, or a /command, to every agent picked"
          aria-label="Message"
        />
      </div>
      <div className="ms-broadcast-bar">
        <div className="ms-modes" role="radiogroup" aria-label="How to send">
          {(["queue", "steer", "now"] as SendMode[]).map((m) => (
            <button key={m} type="button" role="radio" aria-checked={mode === m} onClick={() => setMode(m)}>
              {m === "queue" ? "Queue" : m === "steer" ? "Steer" : "Now"}
            </button>
          ))}
        </div>
        <Button size="sm" onClick={sendText} disabled={busy || !text.trim() || n === 0}>
          <Send /> Send to {n}
        </Button>
      </div>

      {ask ? (
        <div className="ms-bc-ask" role="group" aria-label="Confirm">
          <span>{ask.ask}</span>
          <span className="ms-bc-ask-actions">
            <Button size="xs" variant={ask.danger ? "danger" : "primary"} disabled={busy} onClick={() => void ask.run()}>
              {ask.label}
            </Button>
            <Button size="xs" variant="ghost" disabled={busy} onClick={() => setAsk(null)}>
              Cancel
            </Button>
          </span>
        </div>
      ) : (
        <div className="ms-bc-quick" role="group" aria-label="Quick commands">
          <button type="button" className="ms-bc-q" disabled={busy || working === 0} onClick={() => setAsk({ ask: `Stop the turn of ${count(working, "working agent")}?`, label: "Interrupt", run: () => command({ type: "interrupt" }) })}>
            Interrupt
          </button>
          <button type="button" className="ms-bc-q" disabled={busy || n === 0} onClick={() => setAsk({ ask: `Restart ${count(n, "agent")}?`, label: "Restart", run: () => command({ type: "restart" }) })}>
            Restart
          </button>
          {(["mode", "model", "effort"] as Choose[]).map((c) =>
            (c === "mode" ? preview?.modes : c === "model" ? preview?.models : preview?.efforts)?.length ? (
              <button key={c} type="button" className="ms-bc-q" aria-expanded={choose === c} disabled={busy || n === 0} onClick={() => setChoose(choose === c ? null : c)}>
                {c === "mode" ? "Mode" : c === "model" ? "Model" : "Effort"}
              </button>
            ) : null,
          )}
          <span className="ms-bc-gap" aria-hidden="true" />
          <button type="button" className="ms-bc-q" disabled={busy || waiting === 0} onClick={() => setAsk({ ask: `Allow ${count(waiting, "tool call")} waiting on you?`, label: "Allow", run: () => send({ approvals: "allow" }) })}>
            Allow {waiting > 0 ? waiting : ""} waiting
          </button>
          <button type="button" className="ms-bc-q" disabled={busy || waiting === 0} onClick={() => setAsk({ ask: `Deny ${count(waiting, "tool call")} waiting on you?`, label: "Deny", run: () => send({ approvals: "deny" }) })}>
            Deny {waiting > 0 ? waiting : ""} waiting
          </button>
          {who === "selected" && (
            <button
              type="button"
              className="ms-bc-q ms-bc-danger"
              disabled={busy}
              onClick={() =>
                setAsk({
                  ask: `End ${count(selected.length, "agent")}? Their processes stop on their machines.`,
                  label: "End",
                  danger: true,
                  run: async () => {
                    await Promise.allSettled(selected.map((id) => harnessApi.stop(id)));
                    onClose();
                  },
                })
              }
            >
              End {selected.length}
            </button>
          )}
        </div>
      )}
      {choose && !ask && (
        <div className="ms-bc-choices" role="group" aria-label={choose}>
          {options.map((o) => (
            <button key={o.id} type="button" className="ms-bc-chip" disabled={busy} onClick={() => set(o.id, (o as { dangerous?: boolean }).dangerous === true)}>
              {o.label}
              {o.count < n && <i>{o.count}/{n}</i>}
            </button>
          ))}
        </div>
      )}

      <div className="ms-bc-result">
        {report && (
          <button type="button" className="ms-bc-result-line" aria-expanded={reportOpen} onClick={() => setReportOpen(!reportOpen)}>
            <ChevronRight size={12} className={cn("ms-chev", reportOpen && "rotate-90")} aria-hidden="true" />
            {[report.sent ? `Sent to ${report.sent}` : "", report.skipped ? `${report.skipped} skipped` : "", report.failed ? `${report.failed} failed` : ""].filter(Boolean).join(" · ") || "No agent to send to"}
          </button>
        )}
      </div>
      {report && reportOpen && (
        <ul className="ms-bc-outcomes">
          {report.results.map((r) => (
            <li key={r.id} data-result={r.result}>
              <span className="ms-bc-o-name">{r.name}</span>
              <span className="ms-bc-o-where">{machineLabel(r.machine)}</span>
              <span className="ms-bc-o-result">{r.result}</span>
              {r.reason && <span className="ms-bc-o-why">{r.reason}</span>}
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}

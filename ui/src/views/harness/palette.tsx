/**
 * Slash commands, as each harness offers them and kiln can run them: found by name, alias or what
 * they do; sent as a prompt, run as a control (an answer comes back to read), opened as a kiln
 * panel, or shown as unavailable with the reason.
 */
import * as React from "react";
import { Copy, Download, X } from "lucide-react";
import type { Command, SessionStore, SlashCommand } from "@/api/harness";
import { copyText } from "@/views/terminal/clipboard";
import { Prose } from "./parts";

const GROUPS = ["Session", "Model & mode", "Context", "Tools & MCP", "Skills", "Plugins", "Kiln"];
const groupRank = (g?: string) => {
  const i = GROUPS.indexOf(g ?? "");
  return i === -1 ? GROUPS.length : i;
};

/** "/name words" at the start of the field: the name typed so far and anything after it. */
export function parseSlash(text: string): { name: string; args: string; typingName: boolean } | null {
  const m = text.match(/^\/(\S*)(?:(\s+)([\s\S]*))?$/);
  if (!m) return null;
  return { name: m[1] ?? "", args: (m[3] ?? "").trim(), typingName: m[2] === undefined };
}

/** The entry a typed name means: names first, then aliases. */
export function findCommand(commands: SlashCommand[], name: string): SlashCommand | undefined {
  const n = name.toLowerCase();
  return commands.find((c) => c.name.toLowerCase() === n) ?? commands.find((c) => (c.aliases ?? []).some((a) => a.toLowerCase() === n));
}

/** Matches for what's typed after "/": by name, alias, then what it does; everything (grouped) for "/" alone. */
export function matchCommands(commands: SlashCommand[], query: string): SlashCommand[] {
  const q = query.toLowerCase();
  if (!q) return [...commands].sort((a, b) => groupRank(a.group) - groupRank(b.group) || a.name.localeCompare(b.name));
  const score = (c: SlashCommand) => {
    const n = c.name.toLowerCase();
    if (n === q) return 0;
    if (n.startsWith(q)) return 1;
    if ((c.aliases ?? []).some((a) => a.toLowerCase().startsWith(q))) return 2;
    if (n.includes(q)) return 3;
    if ((c.description ?? "").toLowerCase().includes(q)) return 4;
    return -1;
  };
  return commands
    .map((c) => [c, score(c)] as const)
    .filter(([, s]) => s >= 0)
    .sort((a, b) => a[1] - b[1] || groupRank(a[0].group) - groupRank(b[0].group) || a[0].name.localeCompare(b[0].name))
    .map(([c]) => c);
}

/** A control's command with "{args}" and "{client}" filled in its strings. */
function fill(v: unknown, args: string, client: string): unknown {
  if (typeof v === "string") return v.replaceAll("{args}", args).replaceAll("{client}", client);
  if (Array.isArray(v)) return v.map((x) => fill(x, args, client));
  if (v && typeof v === "object") return Object.fromEntries(Object.entries(v).map(([k, x]) => [k, fill(x, args, client)]));
  return v;
}

export interface Reply {
  title: string;
  text: string;
  format?: string;
  filename?: string;
  path?: string;
  error?: string;
}

export type Outcome =
  | { kind: "send"; text: string }
  | { kind: "sent" }
  | { kind: "view"; view: string }
  | { kind: "reply"; promise: Promise<Reply> }
  | { kind: "unsupported"; reason: string }
  | { kind: "terminal"; command: string };

/** What running an entry with these words does. The caller sends "send" text the usual way. */
export function runCommand(c: SlashCommand, args: string, store: SessionStore): Outcome {
  const r = c.route;
  if (!r || r.kind === "prompt") {
    if (r?.skill) {
      store.send({ type: "skill_run", name: c.name, args: args || undefined });
      return { kind: "sent" };
    }
    return { kind: "send", text: r?.text ? r.text.replaceAll("{args}", args) : `/${c.name}${args ? ` ${args}` : ""}` };
  }
  if (r.kind === "view") return { kind: "view", view: r.view };
  if (r.kind === "unsupported") return { kind: "unsupported", reason: r.reason };
  if (r.kind === "terminal") return { kind: "terminal", command: r.command };
  const asks = JSON.stringify(r.command).includes("{client}");
  if (!asks) {
    store.send(fill(r.command, args, "") as Command);
    return { kind: "sent" };
  }
  const promise = store.request((client) => fill(r.command, args, client) as Command, 60_000).then((v) => {
    const o = (v && typeof v === "object" ? v : {}) as Partial<Reply>;
    return { title: o.title ?? `/${c.name}`, text: o.text ?? "", format: o.format, filename: o.filename, path: o.path, error: o.error };
  });
  return { kind: "reply", promise };
}

/** An answer to a command (/cost, /export, /btw…): read it here, copy or save it, close it. */
export function ReplyPanel({ reply, onClose }: { reply: Reply | "loading"; onClose: () => void }) {
  if (reply === "loading")
    return (
      <div className="pl-reply" aria-busy="true">
        <div className="pl-reply-head">
          <span className="pl-reply-title">Asking…</span>
          <button type="button" className="hx-icon" aria-label="Close" onClick={onClose}>
            <X size={13} />
          </button>
        </div>
      </div>
    );
  const download = () => {
    const url = URL.createObjectURL(new Blob([reply.text], { type: reply.format === "markdown" ? "text/markdown" : "text/plain" }));
    const a = document.createElement("a");
    a.href = url;
    a.download = reply.filename ?? "kiln.txt";
    a.click();
    URL.revokeObjectURL(url);
  };
  return (
    <div className="pl-reply" role="region" aria-label={reply.title}>
      <div className="pl-reply-head">
        <span className="pl-reply-title">{reply.title}</span>
        {reply.path && <span className="pl-reply-path">{reply.path}</span>}
        <span className="flex-1" />
        {!reply.error && reply.text && (
          <>
            <button type="button" className="hx-icon" aria-label="Copy" onClick={() => void copyText(reply.text)}>
              <Copy size={13} />
            </button>
            {reply.filename && (
              <button type="button" className="hx-icon" aria-label={`Save ${reply.filename}`} onClick={download}>
                <Download size={13} />
              </button>
            )}
          </>
        )}
        <button type="button" className="hx-icon" aria-label="Close" onClick={onClose}>
          <X size={13} />
        </button>
      </div>
      <div className="pl-reply-body">{reply.error ? <p className="pl-reply-error">{reply.error}</p> : reply.format === "markdown" ? <Prose text={reply.text} /> : <pre>{reply.text}</pre>}</div>
    </div>
  );
}

/** The list over the composer while a command name is typed. */
export function CommandList({
  items,
  index,
  onPick,
  onHover,
  note,
}: {
  items: SlashCommand[];
  index: number;
  onPick: (c: SlashCommand) => void;
  onHover: (i: number) => void;
  /** A word beside a name, e.g. how many of the agents written to have it. */
  note?: (c: SlashCommand) => string | undefined;
}) {
  // The highlighted command stays in view (again whenever the highlight moves).
  const follow = React.useCallback(
    (ul: HTMLUListElement | null) => {
      ul?.querySelectorAll<HTMLElement>('[role="option"]')[index]?.scrollIntoView({ block: "nearest" });
    },
    [index],
  );
  let lastGroup: string | undefined;
  return (
    <ul ref={follow} className="pl-list" role="listbox" aria-label="Commands">
      {items.map((c, i) => {
        const head = c.group !== lastGroup ? c.group : undefined;
        lastGroup = c.group;
        const off = c.route?.kind === "unsupported";
        return (
          <React.Fragment key={c.name}>
            {head && <li className="pl-group">{head}</li>}
            <li>
              <button
                type="button"
                role="option"
                aria-selected={i === index}
                aria-disabled={off || undefined}
                className="pl-item"
                onPointerDown={(e) => {
                  e.preventDefault();
                  onPick(c);
                }}
                onPointerEnter={() => onHover(i)}
              >
                <span className="pl-name">
                  /{c.name}
                  {c.hint && <span className="pl-hint"> {c.hint}</span>}
                  {note?.(c) && <span className="pl-note">{note(c)}</span>}
                  {c.route?.kind === "terminal" && <span className="pl-note">its own screen</span>}
                </span>
                <span className="pl-desc">{off && c.route?.kind === "unsupported" ? c.route.reason : c.description}</span>
              </button>
            </li>
          </React.Fragment>
        );
      })}
    </ul>
  );
}

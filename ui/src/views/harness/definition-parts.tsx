/**
 * What Mission control's rows and an agent's head show of its definition, apart from the definitions'
 * dropdown (definitions.tsx, which re-exports these): a variant's swatch, a session's sandbox in
 * short, and the heads and counts of a row's dropdown. Those pages load this much with their first
 * view; the dropdown's groups, editors and experiments load with what opens them (goal 8).
 */
import * as React from "react";
import { ChevronRight } from "lucide-react";
import { variantIn, type AgentDef } from "@/api/agentdefs";
import { sandboxesRes, type SandboxBrief } from "@/api/microvms";
import { labelOf, useRes } from "@/lib/data";
import { EngineGlyph, StartMark } from "@/views/sandbox-marks";
import { startedIn } from "./engine-of";
import { variantName, variantTone } from "./variants";
import "./mission.css";
import "./definition.css";

/** A variant's swatch: the colour of its slice of the split, a short bar as that slice is. */
export function Swatch({ tone, title, current }: { tone: string; title?: string; current?: boolean }) {
  return (
    <i
      className="df-swatch"
      style={{ background: tone }}
      title={title}
      data-current={current || undefined}
      aria-hidden={title ? undefined : true}
      role={title ? "img" : undefined}
      aria-label={title}
    />
  );
}

/** The variant a session started in, as its swatch and name (its Agentfiles by their `names`); none outside an experiment. */
export function sessionVariant(d: AgentDef | undefined, id: string, names?: ReadonlyMap<string, string>): { tone: string; name: string } | null {
  const at = variantIn(d, d?.sessions.find((x) => x.id === id)?.variant);
  return at ? { tone: variantTone(at.i, at.e.variants.length), name: variantName(at.e.variants, at.i, names) } : null;
}

/** A session's sandbox in short: its engine, its runner, how it started. `isolation`: its isolation's kind, none for a process. */
export function SandboxLine({ id, target, isolation }: { id: string; target: string; isolation?: string }) {
  return (
    <React.Suspense fallback={<span>{labelOf(target.replace(/^agent:/, ""))}</span>}>
      <SandboxLineRead id={id} target={target} isolation={isolation} />
    </React.Suspense>
  );
}
function SandboxLineRead({ id, target, isolation }: { id: string; target: string; isolation?: string }) {
  const b: SandboxBrief | undefined = useRes(sandboxesRes).data[id];
  const machine = target.replace(/^agent:/, "");
  // A session the sandboxes don't list yet (just started): where it runs, as its start chose.
  if (!b)
    return (
      <span className="df-sandbox">
        <EngineGlyph engine={startedIn(isolation)} />
        <span className="df-runner">{labelOf(machine)}</span>
      </span>
    );
  return (
    <span className="df-sandbox">
      <EngineGlyph engine={b.engine} />
      <span className="df-runner">{labelOf(b.runner)}</span>
      {b.start && <StartMark kind={b.start.kind} ms={b.start.ms} />}
    </span>
  );
}

let unseen: IntersectionObserver | null = null;
/**
 * A row whose dropdown is open says when it's out of sight (`data-unseen`; under kiln's header, or
 * scrolled past in its panel), so its dropdown's head stands in for it (kiln's principle 1). One
 * observer for every row, as signals.tsx's.
 */
export function watchUnseen(el: HTMLElement | null) {
  if (!el) return;
  const top = getComputedStyle(document.documentElement).getPropertyValue("--header-h").trim() || "0px";
  unseen ??= new IntersectionObserver(
    (entries) => {
      for (const e of entries) e.target.toggleAttribute("data-unseen", !e.isIntersecting);
    },
    { rootMargin: `-${top} 0px 0px 0px` },
  );
  unseen.observe(el);
  return () => {
    unseen?.unobserve(el);
    el.removeAttribute("data-unseen");
  };
}

/**
 * An open dropdown's head: its row's name and a way to close it, sticking at the top while you're in
 * the dropdown and its row is out of sight; nothing at all while its row is in sight. It takes no room
 * (it overlies the dropdown), so nothing moves as it comes and goes.
 */
export function OpenHead({ label, onClose }: { label: string; onClose: () => void }) {
  return (
    <div className="ms-open-head">
      <div>
        <span className="ms-open-name">{label}</span>
        <button type="button" className="ms-go" tabIndex={-1} aria-label={`Close ${label}'s dropdown`} title="Close (Esc)" onClick={onClose}>
          <ChevronRight size={14} className="-rotate-90" aria-hidden="true" />
        </button>
      </div>
    </div>
  );
}

export const NODE_WORD: Record<string, string> = { needs: "Needs you", failed: "Stopped", paused: "Paused", working: "Working", done: "Done", offline: "Offline", stopped: "Ended" };
/**
 * Its sessions' stands, as Mission's tree colours them: a dot each stand that has any, and how many
 * stand there, in the order they need you (needs you, failed, at work, paused, done, ended). As many
 * sessions as it has, the line stays one stand wide each: dots and numbers, never words beside them.
 */
const STAND_ORDER = ["needs", "failed", "working", "paused", "done", "offline", "stopped"] as const;
export function StandCounts({ stands, words: named = NODE_WORD }: { stands: readonly string[]; words?: Record<string, string> }) {
  const counts = STAND_ORDER.map((s) => [s, stands.filter((x) => x === s).length] as const).filter(([, n]) => n > 0);
  const words = counts.map(([s, n]) => `${n} ${named[s]?.toLowerCase() ?? s}`).join(", ");
  return (
    <span className="df-counts" role="img" aria-label={words} title={words}>
      {counts.map(([s, n]) => (
        <span key={s} className="df-count" data-stand={s}>
          <i aria-hidden="true" />
          {n}
        </span>
      ))}
    </span>
  );
}

/**
 * A group's head: one line, one height for every group, its parts centred on it: its chevron and
 * label (the press that opens it), what it says in short at its right end, then its controls.
 * Nothing in its middle: what its dropdown shows, it doesn't repeat.
 */
export function GroupHead({
  label,
  open,
  onOpen,
  summary,
  actions,
  stand = "done",
  fixed,
}: {
  label: string;
  open: boolean;
  onOpen: () => void;
  summary?: React.ReactNode;
  actions?: React.ReactNode;
  stand?: string;
  /** A section shown whole (inside something that folds): its label alone, nothing to press. */
  fixed?: boolean;
}) {
  return (
    <li className="ms-node df-group df-head" data-stand={stand} data-depth={1} data-open={open || undefined} data-fixed={fixed || undefined}>
      {fixed ? (
        <span className="df-head-main">
          <span className="ms-sep-h df-group-k">{label}</span>
        </span>
      ) : (
        <button type="button" className="df-head-main" aria-expanded={open} onClick={onOpen}>
          <span className="df-chev">
            <ChevronRight size={12} aria-hidden="true" className={open ? "rotate-90" : undefined} />
          </span>
          <span className="ms-sep-h df-group-k">{label}</span>
        </button>
      )}
      <span className="df-head-end">{summary}</span>
      {actions && <span className="df-head-acts">{actions}</span>}
    </li>
  );
}

/** A kiln MCP call about an agent definition (agentdefs_get, _update, _run …): the definition's id it names, so chat shows its rows. */
export function defCall(c: { server?: string; tool_short?: string; input?: unknown; status?: string }): string | null {
  if (c.server !== "kiln" || !c.tool_short?.startsWith("agentdefs_") || c.status === "error") return null;
  const id = (c.input as { id?: unknown } | null)?.id;
  return typeof id === "string" ? id : null;
}

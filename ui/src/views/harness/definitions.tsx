/**
 * Mission control's definitions (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, P5.2, P7.2): each agent as
 * you define it is a row in Mission's own style (its sessions' stand, its harness and model, Run),
 * and opens in place, as an agent's subagents do, to the sessions started from it and to the live
 * experiment splitting its new sessions between microVM deployments. kiln's own are in the same
 * list, read only.
 */
import * as React from "react";
import { DropdownMenu } from "radix-ui";
import { Check, ChevronDown, ChevronUp, ArrowUpRight, Lock, MoreHorizontal, Pause, Pencil, Play, Repeat2, Square, X } from "lucide-react";
import { harnessApi } from "@/api/harness";
import { errorMessage } from "@/lib/hooks";
import { agentdefsRes, type AgentDef, type Experiment, type Variant } from "@/api/agentdefs";
import type { AgentInfo } from "@/api/harness";
import { Button } from "@/components/ui/button";
import { Segmented } from "@/components/ui/tabs";
import { toast } from "@/components/ui/toast";
import { labelOf, useRes } from "@/lib/data";
import { useNavigate } from "@/lib/router";
import { chatStand, type ChatStand } from "./chat-state";
import { agentLabel, modelLabel } from "./derive";
import { KindGlyph } from "./kind-glyph";
import { Shimmer } from "./signals";

const WORD: Record<ChatStand, string> = { needs: "Needs you", crashed: "Stopped", paused: "Paused", working: "Working", finished: "Finished", ready: "Ready", offline: "Offline", ended: "Ended" };
/** A session's stand as the tree's nodes draw it (mission.css .ms-node[data-stand]). */
const NODE: Record<ChatStand, string> = { needs: "needs", crashed: "failed", paused: "paused", working: "working", finished: "done", ready: "done", offline: "offline", ended: "stopped" };
const NODE_WORD: Record<string, string> = { needs: "Needs you", failed: "Stopped", paused: "Paused", working: "Working", done: "Done", offline: "Offline", stopped: "Ended" };
/** The stand a definition's row shows: its sessions', the one that most needs you first. */
const ORDER: ChatStand[] = ["needs", "crashed", "working", "paused", "finished", "ready", "offline", "ended"];

export function Definitions({ list }: { list: readonly AgentInfo[] }) {
  const defs = useRes(agentdefsRes).data;
  const [show, setShow] = React.useState<"yours" | "kiln">("yours");
  const [open, setOpen] = React.useState<string | null>("reviewer");
  const shown = defs.filter((d) => (show === "yours" ? d.own : !d.own));
  return (
    <section className="ms-defs" aria-label="Definitions">
      <div className="ms-listhead">
        <span className="ms-all">
          <span className="ms-defs-title">Definitions</span>
          <span className="ms-swap">{shown.length}</span>
        </span>
        <Segmented
          size="xs"
          label="Whose"
          value={show}
          onChange={setShow}
          options={[
            { value: "yours", label: "Yours", count: defs.filter((d) => d.own).length },
            { value: "kiln", label: "kiln's", count: defs.filter((d) => !d.own).length },
          ]}
        />
      </div>
      <ul className="ms-list" data-list="defs">
        {shown.map((d) => (
          <DefRow key={d.id} d={d} list={list} open={open === d.id} onOpen={() => setOpen(open === d.id ? null : d.id)} />
        ))}
      </ul>
    </section>
  );
}

function DefRow({ d, list, open, onOpen }: { d: AgentDef; list: readonly AgentInfo[]; open: boolean; onOpen: () => void }) {
  const sessions = d.sessions.map((s) => ({ ...s, a: list.find((x) => x.id === s.id) })).filter((s): s is typeof s & { a: AgentInfo } => s.a !== undefined);
  const stands = sessions.map((s) => chatStand(s.a));
  const stand = ORDER.find((o) => stands.includes(o)) ?? "ready";
  const at = stands.filter((s) => s === "working" || s === "needs").length;
  const line = [sessions.length ? `${sessions.length} session${sessions.length === 1 ? "" : "s"}${at ? `, ${at} at work` : ""}` : "No sessions", d.experiment && `split ${d.experiment.variants.map((v) => v.weight).join("/")}`, d.conflict && `${d.conflict.from} differs`]
    .filter(Boolean)
    .join(" · ");
  const canOpen = d.own || sessions.length > 0 || d.experiment !== undefined;
  return (
    <>
      <li className="ms-row" data-stand={sessions.length ? stand : "ready"} data-def={d.id}>
        <span className="ms-check" aria-hidden="true" />
        <button type="button" className="ms-main" onClick={onOpen} aria-expanded={canOpen ? open : undefined}>
          <span className="ms-name">
            <span>{d.name}</span>
            {!d.own && <Lock size={12} className="ms-flag-pin" role="img" aria-label="kiln's: copy it to change it" />}
            {(d.replaces || d.replaced_by) && <Repeat2 size={12} className="ms-flag-pin" role="img" aria-label={d.replaces ? `Stands in for kiln's ${d.replaces}` : `You use ${d.replaced_by}`} />}
          </span>
          <span className="ms-line">
            <Shimmer on={stand === "working" && sessions.length > 0} flow={d.id} className="ms-word">
              <i aria-hidden="true" />
              {sessions.length ? WORD[stand] : `v${d.version}`}
            </Shimmer>
            {d.conflict && <span className="ms-flag" data-flag="stale" title={`${d.conflict.field}: ${d.conflict.here} here, ${d.conflict.there} in ${d.conflict.from}`} />}
            <span className="ms-text">{line}</span>
          </span>
        </button>
        <span className="ms-meta">
          <span>
            {d.agentfile} · v{d.version}
          </span>
          <span className="ms-meta-2">
            <span className="ms-kind">{d.harness}</span>
            {d.model && <span>{modelLabel(d.model)}</span>}
          </span>
        </span>
        <span className="ms-actions">
          <span className="ms-decide">
            <Button size="xs" variant="secondary" onClick={() => toast({ title: `Starting ${d.name}`, tone: "running" })}>
              Run
            </Button>
          </span>
          <span className="ms-tail">
            {canOpen && (
              <button type="button" className="ms-subs" aria-expanded={open} aria-label={open ? `Hide ${d.name}'s sessions` : `Show ${d.name}'s sessions`} data-on={open || undefined} onClick={onOpen}>
                <KindGlyph kind="agent" live={at > 0} />
                {open ? <ChevronUp size={12} aria-hidden="true" /> : <ChevronDown size={12} aria-hidden="true" />}
              </button>
            )}
            <DefMenu d={d} />
          </span>
        </span>
      </li>
      {open && canOpen && (
        <li className="ms-tree">
          <ul className="ms-tree-rows" aria-label={`${d.name}'s sessions`}>
            {d.own && <DefNode d={d} />}
            {sessions.map((s) => (
              <SessionNode key={s.id} a={s.a} version={s.version} current={d.version} variant={s.variant} />
            ))}
            {d.experiment && <ExperimentNodes e={d.experiment} />}
          </ul>
        </li>
      )}
    </>
  );
}

/** A session started from a definition, in the tree's own columns: the version it started on and its variant beside its stand. */
function SessionNode({ a, version, current, variant }: { a: AgentInfo; version: number; current: number; variant?: string }) {
  const navigate = useNavigate();
  const stand = NODE[chatStand(a)];
  const to = `/agents/${a.id}`;
  return (
    <li className="ms-node" data-stand={stand} data-depth={1}>
      <span className="ms-node-path" />
      <button type="button" className="ms-node-main" onClick={() => navigate(to)}>
        <span className="ms-node-name">{agentLabel(a)}</span>
        <span className="ms-node-line">
          <Shimmer on={stand === "working"} flow={a.id} className="ms-word">
            <i aria-hidden="true" />
            {NODE_WORD[stand]}
          </Shimmer>
          <span className="ms-node-text" title={version === current ? "Current version" : `Started on v${version}; new sessions get v${current}`}>
            v{version}
            {variant ? ` · variant ${variant}` : ""}
          </span>
        </span>
      </button>
      <span className="ms-node-meta">
        <span>{labelOf(a.target.replace(/^agent:/, ""))}</span>
        <span>{a.kind_name}</span>
      </span>
      <span className="ms-node-actions">
        <span className="ms-decide">
          <SessionControls a={a} stand={stand} />
        </span>
        <span className="ms-tail">
          <button type="button" className="ms-go" onClick={() => navigate(to)} aria-label={`Open ${agentLabel(a)}`}>
            <ArrowUpRight size={16} />
          </button>
        </span>
      </span>
    </li>
  );
}

/** A live experiment under its agent: the split, then each variant with its weight and its measures as intervals. */
function ExperimentNodes({ e }: { e: Experiment }) {
  return (
    <>
      <li className="ms-node" data-stand={e.state === "live" ? "working" : e.state === "paused" ? "paused" : "done"} data-depth={1}>
        <span className="ms-node-path" />
        <span className="ms-node-main">
          <span className="ms-node-name">{e.id}</span>
          <span className="ms-node-line">
            <Shimmer on={e.state === "live"} flow={e.id} className="ms-word">
              <i aria-hidden="true" />
              {e.state === "live" ? "Splitting" : e.state === "paused" ? "Paused" : "Promoted"}
            </Shimmer>
            <span className="ms-node-text">
              {e.sessions} sessions · split v{e.version}
            </span>
          </span>
        </span>
        <span className="ms-node-meta">
          <span className="ms-split" aria-label={`Split ${e.variants.map((v) => `${v.key} ${v.weight}%`).join(", ")}`}>
            {e.variants.map((v, i) => (
              <i key={v.key} data-i={i} style={{ flexGrow: v.weight }} />
            ))}
          </span>
          <span>{e.variants.map((v) => v.weight).join("/")}</span>
        </span>
        <span className="ms-node-actions" />
      </li>
      {e.variants.map((v, i) => (
        <VariantNode key={v.key} v={v} i={i} all={e.variants} />
      ))}
    </>
  );
}

function VariantNode({ v, i, all }: { v: Variant; i: number; all: Variant[] }) {
  const win = (k: keyof Variant["measures"]) => all.every((o) => o === v || v.measures[k][2] < o.measures[k][0]);
  const gib = Math.round(v.memory / 2 ** 30);
  return (
    <li className="ms-node ms-variant" data-stand="done" data-depth={2}>
      <span className="ms-node-path">
        <span className="ms-vkey" data-i={i} title={v.current ? `Variant ${v.key}, what it runs on now` : `Variant ${v.key}`} data-current={v.current || undefined}>
          {v.key}
        </span>
      </span>
      <span className="ms-node-main">
        <span className="ms-node-name">
          {v.agentfile} · {v.cpus} vCPU · {gib} GiB
        </span>
        <span className="ms-node-line">
          <span className="ms-node-text">
            <Measure label="warm" iv={v.measures.warm_ms} f={(x) => `${Math.round(x)}ms`} win={win("warm_ms")} />
            <Measure label="peak" iv={v.measures.peak_gib} f={(x) => `${x.toFixed(2)}G`} win={win("peak_gib")} />
            <Measure label="failed" iv={v.measures.failed_pct} f={(x) => `${Math.round(x)}%`} win={win("failed_pct")} />
          </span>
        </span>
      </span>
      <span className="ms-node-meta">
        <span className="meter ms-weight" role="meter" aria-valuenow={v.weight} aria-valuemin={0} aria-valuemax={100} aria-label={`Weight ${v.weight}%`}>
          <span style={{ width: `${v.weight}%` }} />
        </span>
        <span>
          {v.weight}% · {v.sessions} sessions
        </span>
      </span>
      <span className="ms-node-actions">
        <span className="ms-decide">
          {!v.current && (
            <Button size="xs" variant="secondary" onClick={() => toast({ title: `Promote ${v.key}? Confirm on its row`, tone: "info" })}>
              Promote
            </Button>
          )}
        </span>
      </span>
    </li>
  );
}

/** A measure as its median, with a check where its 95% interval clears every other variant's. */
function Measure({ label, iv, f, win }: { label: string; iv: [number, number, number]; f: (x: number) => string; win: boolean }) {
  return (
    <span className="ms-measure" data-win={win || undefined} title={`${label}: median ${f(iv[1])}, 95% ${f(iv[0])}–${f(iv[2])}`}>
      {label} {f(iv[1])}
      {win ? " ✓" : ""}
    </span>
  );
}

/** A definition's rarer acts, behind one ⋯, as a Mission row's are (actions.tsx): its history, an experiment, a copy, delete. */
function DefMenu({ d }: { d: AgentDef }) {
  const items: { label: string; onSelect: () => void; danger?: boolean }[] = d.own
    ? [
        { label: `History (v${d.version})`, onSelect: () => toast({ title: `${d.name}: v1 to v${d.version}`, tone: "info" }) },
        { label: d.experiment ? "Experiment settings" : "New experiment…", onSelect: () => toast({ title: "Experiment setup opens on its row", tone: "info" }) },
        { label: "Duplicate", onSelect: () => toast({ title: `Copied ${d.name}`, tone: "info" }) },
        { label: "Delete…", onSelect: () => toast({ title: `Delete ${d.name}? Confirm on its row`, tone: "info" }), danger: true },
      ]
    : [{ label: "Copy as yours", onSelect: () => toast({ title: `${d.name} copied as yours`, tone: "info" }) }];
  return (
    <DropdownMenu.Root modal={false}>
      <DropdownMenu.Trigger asChild>
        <button type="button" className="hx-icon" aria-label={`Actions for ${d.name}`}>
          <MoreHorizontal size={16} />
        </button>
      </DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content className="popover w-56" sideOffset={6} align="end" collisionPadding={12}>
          {items.map((i) => (
            <DropdownMenu.Item key={i.label} className="menu-item" onSelect={i.onSelect} style={i.danger ? { color: "var(--color-fail)" } : undefined}>
              <span className="flex-1">{i.label}</span>
            </DropdownMenu.Item>
          ))}
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  );
}

/** A session's own controls on its row: pause or resume (undone from its toast), and stop, asked in place. */
function SessionControls({ a, stand }: { a: AgentInfo; stand: string }) {
  const [asking, setAsking] = React.useState(false);
  const [busy, setBusy] = React.useState(false);
  const paused = stand === "paused";
  const live = stand !== "stopped" && stand !== "failed";
  const pause = (on: boolean) => {
    setBusy(true);
    harnessApi
      .command(a.id, { type: "pause", on })
      .then(() => toast({ title: on ? `Paused ${agentLabel(a)}` : `Resumed ${agentLabel(a)}`, tone: "info", description: on ? "Resume from its row (P)" : undefined }))
      .catch((e: unknown) => toast.error(on ? "Couldn't pause" : "Couldn't resume", errorMessage(e)))
      .finally(() => setBusy(false));
  };
  if (!live) return null;
  if (asking)
    return (
      <span className="ms-confirm" role="group" aria-label={`Stop ${agentLabel(a)}?`}>
        <span>Stop?</span>
        <button type="button" className="ms-go" aria-label="Stop it" onClick={() => { setAsking(false); toast({ title: `Stopping ${agentLabel(a)}`, tone: "info" }); }}>
          <Check size={15} />
        </button>
        <button type="button" className="ms-go" aria-label="Keep it" onClick={() => setAsking(false)}>
          <X size={15} />
        </button>
      </span>
    );
  return (
    <span className="ms-ctl">
      <button type="button" className="ms-go" disabled={busy} title={paused ? "Resume (P)" : "Pause (P)"} aria-label={`${paused ? "Resume" : "Pause"} ${agentLabel(a)}`} onClick={() => pause(!paused)}>
        {paused ? <Play size={15} /> : <Pause size={15} />}
      </button>
      <button type="button" className="ms-go" title="Stop (S)" aria-label={`Stop ${agentLabel(a)}`} onClick={() => setAsking(true)}>
        <Square size={14} />
      </button>
    </span>
  );
}

/** A definition's own values, first under its row: each a value until pressed, then its control, in place; a save is a new version. */
function DefNode({ d }: { d: AgentDef }) {
  const [editing, setEditing] = React.useState<string | null>(null);
  const [vals, setVals] = React.useState({ model: d.model, limit: d.limit, instructions: d.instructions });
  const [version, setVersion] = React.useState(d.version);
  const field = (k: keyof typeof vals, label: string, shown: string) =>
    editing === k ? (
      <span className="ms-edit">
        <span className="ms-edit-k">{label}</span>
        <input className="ms-edit-in" autoFocus defaultValue={vals[k]} aria-label={label} onKeyDown={(e) => { if (e.key === "Enter") save(k, e.currentTarget.value); if (e.key === "Escape") setEditing(null); }} />
        <button type="button" className="ms-go" aria-label={`Save ${label}`} onClick={(e) => save(k, (e.currentTarget.previousElementSibling as HTMLInputElement).value)}>
          <Check size={15} />
        </button>
      </span>
    ) : (
      <button type="button" className="ms-field" title={`Change ${label}`} onClick={() => setEditing(k)}>
        <span className="ms-edit-k">{label}</span>
        <span className="ms-field-v">{shown || "none"}</span>
        <Pencil size={12} aria-hidden="true" />
      </button>
    );
  const save = (k: keyof typeof vals, v: string) => {
    setVals({ ...vals, [k]: v });
    setEditing(null);
    setVersion((n) => n + 1);
    toast({ title: `Saved v${version + 1}; running sessions keep theirs`, tone: "info" });
  };
  return (
    <li className="ms-node ms-defnode" data-stand="done" data-depth={1}>
      <span className="ms-node-path" />
      <span className="ms-node-main">
        <span className="ms-fields">
          {field("model", "model", vals.model ? modelLabel(vals.model) : "default")}
          {field("limit", "limit", vals.limit)}
          <span className="ms-field" aria-label={`Version ${version}`}>
            <span className="ms-edit-k">version</span>
            <span className="ms-field-v">v{version}</span>
          </span>
        </span>
        <span className="ms-fields">{field("instructions", "instructions", vals.instructions)}</span>
      </span>
      <span className="ms-node-meta">
        <span>
          {d.tools} kiln · {d.mcp} MCP
        </span>
        <span>{d.skills} skills</span>
      </span>
      <span className="ms-node-actions" />
    </li>
  );
}

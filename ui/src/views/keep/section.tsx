/**
 * The System page's copies of other runners' trees (docs/reach.md §5.1): every runner's rules,
 * then each runner with what it holds, the room on its disk, what it let go and why, when a schedule
 * next comes round, its own rules and Let go now.
 */
import * as React from "react";
import { api } from "@/api/client";
import type { KeepCopy, KeepGone, KeepRunner, KeepView } from "@/api/types";
import { Button } from "@/components/ui/button";
import { Hold, TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { EmptyState, FoldSection, Panel } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { toast } from "@/components/ui/toast";
import { useRes, type Res } from "@/lib/data";
import { formatBytes } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { RelTime } from "../shared";
import { RulesForm } from "./rules";
import { triggerSaid } from "./words";
import "./keep.css";

/** Every scope's rules and what each runner holds: read again when the server says they changed. */
export const keepRes: Res<KeepView> = { key: "reach/keep", fetch: (s) => api.keep(s), opts: { pollMs: 15_000, on: ["keep"] } };

const LEAD =
  "A runner keeps a copy of each tree another runner runs commands on (~/.kiln/reach), so the next sync sends only what changed. Nothing goes until a schedule or a room limit says when; a copy in use, or used in the last ten minutes, always stays. Rules set for a runner, a repository (on its row in Repositories) or an agent (its settings) win over these, rule by rule.";

export function KeepSection({ index, hold = false }: { index: string; hold?: boolean }) {
  return (
    <FoldSection id="system.keep" index={index} label="Reach" title="Copies of other runners' trees">
      <p className="section-lead">{LEAD}</p>
      {hold ? (
        <KeepHold />
      ) : (
        <Loadable label="the copies runners keep" fallback={<KeepHold />}>
          <KeepBody />
        </Loadable>
      )}
    </FoldSection>
  );
}

/** What a runner's copies take, as measured so far; and whether some aren't yet. */
export function taking(copies: KeepCopy[]): { bytes: number; unmeasured: number } {
  return { bytes: copies.reduce((a, c) => a + (c.bytes ?? 0), 0), unmeasured: copies.filter((c) => c.bytes === undefined).length };
}

function KeepBody() {
  const view = useRes(keepRes).data;
  const [open, setOpen] = React.useState<string | null>(null);
  const shown = view.runners.find((r) => r.name === open) ?? null;
  return (
    <>
      <Panel eyebrow="Every runner" title="Rules">
        <RulesForm
          rules={view.rules}
          top
          label="Every runner's rules"
          onSave={async (r) => {
            await api.setKeepRules(r);
            invalidate("reach/keep");
          }}
        />
      </Panel>
      {view.runners.length === 0 ? (
        <EmptyState title="No runner yet" />
      ) : (
        <div className="table-wrap mt-6">
          <table className="data-table">
            <RunnersHead />
            <tbody>
              {view.runners.map((r) => (
                <RunnerRow key={r.name} r={r} open={open === r.name} onOpen={() => setOpen(open === r.name ? null : r.name)} />
              ))}
            </tbody>
          </table>
        </div>
      )}
      {shown && <RunnerPanel key={shown.name} view={view} r={shown} />}
    </>
  );
}

function RunnersHead() {
  return (
    <thead>
      <tr>
        <th className="w-7" aria-label="State" />
        <th>Runner</th>
        <th className="num">Copies</th>
        <th className="num hidden sm:table-cell">Taking</th>
        <th className="num hidden md:table-cell">Free</th>
        <th className="hidden lg:table-cell">Rules</th>
        <th className="hidden lg:table-cell">Last let go</th>
        <th className="hidden xl:table-cell">Next</th>
        <th className="w-px" aria-label="Actions" />
      </tr>
    </thead>
  );
}

function runnerDot(r: KeepRunner): { status: string; title: string } {
  if (r.error) return { status: "failed", title: r.error };
  if (r.heard === null) return { status: "skipped", title: "not heard from: a runner from before kiln kept copies, or one that never connected" };
  if (!r.connected) return { status: "cancelled", title: "offline: what it last said" };
  return { status: "succeeded", title: "keeping its copies" };
}

function RunnerRow({ r, open, onOpen }: { r: KeepRunner; open: boolean; onOpen: () => void }) {
  const dot = runnerDot(r);
  const t = taking(r.copies);
  const last = r.gone[0];
  const waiting = r.asked > r.done;
  const [busy, setBusy] = React.useState(false);
  const letGo = async () => {
    setBusy(true);
    try {
      await api.keepNow(r.name);
      toast.success(`${r.name} lets go now`, `${r.now.copies} ${r.now.copies === 1 ? "copy" : "copies"} its rules don't keep, within 20 seconds.`);
      invalidate("reach/keep");
    } catch (e) {
      toast.error("Couldn't ask it", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <tr data-selected={open} className="cursor-pointer" onClick={(ev) => !(ev.target as HTMLElement).closest("button") && onOpen()}>
      <td>
        <StatusDot status={dot.status} title={dot.title} />
      </td>
      <td className="whitespace-nowrap text-[#e4e5e7]">
        <button type="button" className="text-left" aria-expanded={open} onClick={onOpen}>
          {r.name}
        </button>
      </td>
      <td className="num cell-mono">{r.copies.length}</td>
      <td className="num cell-mono hidden sm:table-cell" title={t.unmeasured ? `${t.unmeasured} not measured yet` : undefined}>
        {r.copies.length ? `${formatBytes(t.bytes)}${t.unmeasured ? "+" : ""}` : "—"}
      </td>
      <td className="num cell-mono hidden md:table-cell">{r.disk ? `${formatBytes(r.disk.free)} · ${Math.round((r.disk.free / Math.max(1, r.disk.total)) * 100)}%` : "—"}</td>
      <td className="cell-mono hidden lg:table-cell">{Object.keys(r.rules).length ? "its own" : "every runner's"}</td>
      <td className="cell-mono hidden lg:table-cell">
        {last ? (
          <>
            <RelTime ts={last.at} /> · {triggerSaid(last.trigger)}
          </>
        ) : (
          "—"
        )}
      </td>
      <td className="cell-mono hidden xl:table-cell">{r.next ? <RelTime ts={r.next} /> : "—"}</td>
      <td className="text-right whitespace-nowrap">
        <Button
          size="xs"
          variant="secondary"
          disabled={busy || waiting || !r.connected || r.now.copies === 0}
          title={r.now.copies ? `${r.now.copies} ${r.now.copies === 1 ? "copy" : "copies"} its rules don't keep (${formatBytes(r.now.bytes)} measured)` : "Its rules keep every copy it has"}
          onClick={() => void letGo()}
        >
          {waiting ? "Letting go…" : "Let go now"}
        </Button>
      </td>
    </tr>
  );
}

function RunnerPanel({ view, r }: { view: KeepView; r: KeepRunner }) {
  return (
    <div className="kp-runner">
      <Panel eyebrow={r.name} title="Its rules" description={r.base ? `Its copies are in ${r.base}.` : undefined}>
        <RulesForm
          rules={r.rules}
          above={view.rules}
          aboveName="every runner's"
          label={`${r.name}'s rules`}
          onSave={async (rules) => {
            await api.setRunnerKeep(r.name, rules);
            invalidate("reach/keep");
          }}
        />
      </Panel>
      <Panel eyebrow={r.name} title="What it holds" description={r.error ? r.error : undefined}>
        <CopyList copies={r.copies} empty={r.heard === null ? "It hasn't said what it holds: an older kiln, or one that never connected." : "No copies of other runners' trees."} />
        <h4 className="field-label mt-6">What it let go</h4>
        <GoneList gone={r.gone} />
      </Panel>
    </div>
  );
}

/** Copies, each with what the rules say of it now. */
export function CopyList({ copies, empty, where = false }: { copies: KeepCopy[]; empty: string; where?: boolean }) {
  if (!copies.length) return <p className="kp-summary">{empty}</p>;
  const sorted = [...copies].sort((a, b) => b.used - a.used);
  return (
    <ul className="kp-list">
      {sorted.map((c) => (
        <li key={`${c.runner ?? ""}/${c.from}/${c.key}`}>
          <StatusDot status={c.busy ? "running" : c.kept ? "succeeded" : "timed_out"} title={c.busy ? "in use" : c.kept ? "kept" : "goes when a trigger comes"} />
          <span className="kp-copy-name" title={`${c.from}/${c.key}`}>
            {where && c.runner ? `${c.runner}: ` : ""}
            {c.from}/{c.key}
          </span>
          <span className="kp-copy-meta">
            {c.bytes !== undefined ? formatBytes(c.bytes) : "measuring"} · <RelTime ts={c.used} />
          </span>
          <span className="kp-copy-why">
            {c.repo ? `${c.repo} · ` : ""}
            {c.kept ? "stays: " : "goes: "}
            {c.why}
          </span>
        </li>
      ))}
    </ul>
  );
}

/** Copies let go, newest first, with why. */
export function GoneList({ gone }: { gone: KeepGone[] }) {
  if (!gone.length) return <p className="kp-summary">Nothing yet.</p>;
  return (
    <ul className="kp-list">
      {gone.slice(0, 20).map((g, i) => (
        <li key={`${g.at}-${g.from}-${g.key}-${i}`}>
          <StatusDot status={g.error ? "failed" : "skipped"} title={g.error ?? triggerSaid(g.trigger)} />
          <span className="kp-copy-name" title={`${g.from}/${g.key}`}>
            {g.from}/{g.key}
          </span>
          <span className="kp-copy-meta">
            {g.bytes !== undefined ? `${formatBytes(g.bytes)} · ` : ""}
            <RelTime ts={g.at} /> · {triggerSaid(g.trigger)}
          </span>
          <span className="kp-copy-why">
            {g.why}
            {g.error ? ` (${g.error})` : ""}
          </span>
        </li>
      ))}
    </ul>
  );
}

/** The section while it loads: the rules' panel and the table's head. */
function KeepHold() {
  return (
    <>
      <Panel eyebrow="Every runner" title="Rules">
        <Hold height={300} label="Loading the copies runners keep" />
      </Panel>
      <div className="mt-6">
        <TableHold head={<RunnersHead />} rows={4} rowHeight={41} />
      </div>
    </>
  );
}

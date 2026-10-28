/**
 * The Files section's one row (docs/ci.md W26-R07, R08, f22): the open comments by kind (each
 * icon with its count) and Approve, its words the gate's — a blocking thread or a conflict holds
 * it disabled saying whose to resolve, open issues ask "Approve anyway", open nits are said on
 * approval; while conflicts remain, "Commit resolution" stands in Approve's place (W26-R12).
 * A host's pull request approves through the forge seam; a commit with no change shows no row.
 */
import * as React from "react";
import { BadgeCheck, Check, CircleQuestionMark, Feather, GitMerge, OctagonMinus, TriangleAlert, X } from "lucide-react";
import { conflictsApi, reviewApi, type ReviewGate } from "@/api/review";
import { invalidate } from "@/lib/resource";
import "./review.css";

const said = (e: unknown) => (e instanceof Error ? e.message : String(e));

const KIND_ICON = { question: CircleQuestionMark, nit: Feather, issue: TriangleAlert, blocking: OctagonMinus } as const;
const ORDER = ["blocking", "issue", "nit", "question"] as const;

export function GateRow({
  gate,
  conflicts,
  diffKey,
  superseded,
  onApprove,
}: {
  gate: ReviewGate | null;
  /** Unresolved conflicts left, and whether every one is taken (commit may go). */
  conflicts: { unresolved: number; ready: boolean; total: number };
  diffKey: string;
  /** The revision that replaced this one: Approve is off, naming it. */
  superseded?: string | null;
  onApprove?: () => void;
}) {
  const [ask, setAsk] = React.useState(false);
  const [note, setNote] = React.useState<{ k: string; t: string } | null>(null);
  const [busy, start] = React.useTransition();
  const [err, setErr] = React.useState<string | null>(null);

  const chips = (
    <span className="rv-ks">
      {conflicts.total > 0 && (
        <span className="rv-k" data-k="conflict" title={`${conflicts.unresolved} conflict${conflicts.unresolved === 1 ? "" : "s"} open`} aria-label={`${conflicts.unresolved} conflicts`}>
          <GitMerge size={12} aria-hidden="true" />
          <b>{conflicts.unresolved}</b>
        </span>
      )}
      {ORDER.map((k) => {
        const n = gate?.open[k] ?? 0;
        if (!n) return null;
        const Icon = KIND_ICON[k];
        return (
          <span key={k} className="rv-k" data-k={k} title={`${n} ${k}${n === 1 ? "" : "s"} open`} aria-label={`${n} ${k}s`}>
            <Icon size={12} aria-hidden="true" />
            <b>{n}</b>
          </span>
        );
      })}
      {!conflicts.total && !ORDER.some((k) => gate?.open[k]) && (
        <span className="rv-k" data-k="ok" title="No open comments" aria-label="No open comments">
          <Check size={12} aria-hidden="true" />
        </span>
      )}
    </span>
  );

  const commit = () =>
    start(async () => {
      setErr(null);
      try {
        const done = await conflictsApi.commit(diffKey);
        invalidate("review/");
        invalidate("ship/");
        setNote({ k: "ok", t: `v${done.version} · conflicts resolved` });
        onApprove?.();
      } catch (e) {
        setErr(said(e));
      }
    });

  const approve = (force: boolean) =>
    start(async () => {
      setErr(null);
      try {
        const r = await reviewApi.review(diffKey, "accept", "", force);
        invalidate("review/");
        invalidate("ship/");
        setAsk(false);
        setNote({ k: gate?.note ? "nit" : "ok", t: r.note ?? "Approved" });
        onApprove?.();
      } catch (e) {
        setErr(said(e));
      }
    });

  const held = gate?.verdict === "hold" || conflicts.unresolved > 0;
  const holdWhy =
    conflicts.unresolved > 0
      ? `Resolve the conflict${conflicts.unresolved === 1 ? "" : "s"} first`
      : (gate?.holds.map((h) => (h.by ? `${h.at}: ${h.by.replace(/^user:/, "")} resolves ${h.reason}` : `${h.at}: ${h.reason}`)).join(" · ") ?? "held");

  let action: React.ReactNode;
  if (conflicts.total > 0 && conflicts.unresolved === 0 && conflicts.ready)
    action = (
      <button type="button" className="rv-btn rv-btn-go" disabled={busy} onClick={commit}>
        <Check size={13} aria-hidden="true" />
        <span>Commit resolution</span>
      </button>
    );
  else
    action = (
      <button
        type="button"
        className="rv-btn rv-btn-go"
        disabled={busy || held || !!superseded}
        title={superseded ? `Superseded by ${superseded}` : held ? holdWhy : undefined}
        onClick={() => (gate?.verdict === "warn" && !ask ? setAsk(true) : approve(gate?.verdict === "warn"))}
      >
        {held ? <OctagonMinus size={13} aria-hidden="true" /> : <Check size={13} aria-hidden="true" />}
        <span>Approve</span>
      </button>
    );

  return (
    <>
      <div className="rv-bar">
        {chips}
        {action}
      </div>
      {ask && gate?.warning && (
        <div className="rv-note" data-k="issue">
          <TriangleAlert size={13} aria-hidden="true" />
          <span>{gate.warning}</span>
          <button type="button" className="rv-btn rv-btn-go" disabled={busy} onClick={() => approve(true)}>
            Approve anyway
          </button>
          <button type="button" className="rv-ib" aria-label="Cancel" onClick={() => setAsk(false)}>
            <X size={14} aria-hidden="true" />
          </button>
        </div>
      )}
      {note && (
        <div className="rv-note" data-k={note.k}>
          {note.k === "ok" ? <Check size={13} aria-hidden="true" /> : note.k === "nit" ? <Feather size={13} aria-hidden="true" /> : <BadgeCheck size={13} aria-hidden="true" />}
          <span>{note.t}</span>
        </div>
      )}
      {err && (
        <p className="rvc-error" role="alert">
          {err}
        </p>
      )}
    </>
  );
}

/** A host's pull request's row: Approve through the forge seam, the kinds kept by kiln (none read back). */
export function HostGateRow({ approved, onApprove, busy }: { approved: boolean; onApprove: () => Promise<void>; busy?: boolean }) {
  const [err, setErr] = React.useState<string | null>(null);
  const [done, setDone] = React.useState(approved);
  const [go, start] = React.useTransition();
  return (
    <>
      <div className="rv-bar">
        <span className="rv-ks" />
        {done ? (
          <span className="rv-k" data-k="ok">
            <Check size={12} aria-hidden="true" />
            <b>approved</b>
          </span>
        ) : (
          <button
            type="button"
            className="rv-btn rv-btn-go"
            disabled={busy || go}
            onClick={() =>
              start(async () => {
                setErr(null);
                try {
                  await onApprove();
                  invalidate("review/");
                  invalidate("ship/");
                  setDone(true);
                } catch (e) {
                  setErr(said(e));
                }
              })
            }
          >
            <Check size={13} aria-hidden="true" />
            <span>Approve</span>
          </button>
        )}
      </div>
      {err && (
        <p className="rvc-error" role="alert">
          {err}
        </p>
      )}
    </>
  );
}

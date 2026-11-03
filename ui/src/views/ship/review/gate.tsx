/**
 * The Files section's one row (docs/ci.md W26-R07, R08, f22): the open comments by kind (each
 * icon with its count) and Approve, its words the gate's — a blocking thread or a conflict holds
 * it disabled saying whose to resolve, open issues ask "Approve anyway", open nits are said on
 * approval; while conflicts remain, "Commit resolution" stands in Approve's place (W26-R12).
 * A host's pull request approves through the forge seam; a commit with no change shows no row.
 * Each approval carried to this version from an earlier one, a rebase-only version with its verbatim
 * patch-id, is said under the row in kiln's words: "kim's approval carried from v2: rebase only, …"
 * (docs/ci.md W27-R08: kiln signs the carry as a transfer).
 */
import * as React from "react";
import { BadgeCheck, Check, CircleQuestionMark, Feather, GitMerge, OctagonMinus, TriangleAlert, X } from "lucide-react";
import { conflictsApi, reviewApi, verdictsRes, type Readiness, type ReviewGate } from "@/api/review";
import { useRes } from "@/lib/data";
import { invalidate } from "@/lib/resource";
import "./review.css";

const said = (e: unknown) => (e instanceof Error ? e.message : String(e));

/** Where the queue has a change, in Land's words (docs/ci.md W26-R19): by its entry's state, and by its Land pip. */
const QUEUE_WORD: Record<string, string> = { queued: "In the queue", testing: "Testing", held: "Held", returned: "Back with Review", landed: "Landed", ejected: "Didn't land" };
const PIP_WORD: Record<string, string> = { queued: "In the queue", running: "Testing", needs: "Back with Review", passed: "Landed" };

/**
 * Land, beside Approve (docs/ci.md W26-R19): while the gate is clear it hands the change to the
 * queue; while it holds, Land when ready has Review hand it over once it clears; once asked, or in
 * the queue, it says where the queue has it, disabled. It keeps its place and width whatever it says.
 */
function LandButton({ word, why, can, onLand, onError }: { word: string; why?: string; can: boolean; onLand: () => Promise<unknown>; onError: (e: string | null) => void }) {
  const [busy, start] = React.useTransition();
  const land = () =>
    start(async () => {
      onError(null);
      try {
        await onLand();
        invalidate("review/");
        invalidate("ship/");
      } catch (e) {
        onError(said(e));
      }
    });
  return (
    <button type="button" className="rv-btn rv-land" disabled={busy || !can} title={why} onClick={land}>
      <GitMerge size={13} aria-hidden="true" />
      <span>{word}</span>
    </button>
  );
}

const KIND_ICON = { question: CircleQuestionMark, nit: Feather, issue: TriangleAlert, blocking: OctagonMinus } as const;
const ORDER = ["blocking", "issue", "nit", "question"] as const;

export function GateRow({
  gate,
  conflicts,
  diffKey,
  superseded,
  ready,
  onApprove,
}: {
  gate: ReviewGate | null;
  /** What Review says of it for landing now, and where the queue has it. */
  ready?: Readiness | null;
  /** Unresolved conflicts left, and whether every one is taken (commit may go). */
  conflicts: { unresolved: number; ready: boolean; total: number };
  diffKey: string;
  /** The revision that replaced this one: Approve is off, naming it. */
  superseded?: string | null;
  onApprove?: () => void;
}) {
  const [ask, setAsk] = React.useState(false);
  const [note, setNote] = React.useState<{ k: string; t: string } | null>(null);
  const carried = useRes(verdictsRes(diffKey)).data.verdicts.filter((v) => v.action === "accept" && v.standing === "carried" && v.carried?.said);
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

  const q = ready?.queue ?? null;
  const clear = !!ready && ready.blockers.length === 0;
  const land = (
    <LandButton
      word={q ? (QUEUE_WORD[q.state] ?? q.state) : ready?.when_ready ? "Lands when ready" : clear ? "Land" : "Land when ready"}
      why={q?.reason ?? (ready?.blockers.join(" · ") || undefined)}
      can={!!ready && !q && !ready.when_ready && !superseded}
      onLand={() => reviewApi.land(diffKey, undefined, !clear)}
      onError={setErr}
    />
  );

  return (
    <>
      <div className="rv-bar">
        {chips}
        {action}
        {land}
      </div>
      {carried.map((v) => (
        <p key={v.id} className="rv-carried">
          <BadgeCheck size={13} aria-hidden="true" />
          <span>
            {v.reviewer.replace(/^user:/, "")}'s approval {v.carried?.said}
          </span>
        </p>
      ))}
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

/**
 * A host's pull request's row: Approve through the forge seam, the kinds kept by kiln (none read
 * back), and Land into its host's queue (docs/ci.md W26-R19), which Review's gate refuses, saying
 * why, until it's clear; `land.state` is its Land pip's.
 */
export function HostGateRow({ approved, onApprove, busy, land }: { approved: boolean; onApprove: () => Promise<void>; busy?: boolean; land?: { state: string; onLand: () => Promise<unknown> } }) {
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
        {land && <LandButton word={PIP_WORD[land.state] ?? "Land"} can={!PIP_WORD[land.state]} onLand={land.onLand} onError={setErr} />}
      </div>
      {err && (
        <p className="rvc-error" role="alert">
          {err}
        </p>
      )}
    </>
  );
}

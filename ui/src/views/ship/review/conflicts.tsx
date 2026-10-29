/**
 * A merge conflict where it falls in a Ship item's Files (docs/ci.md W26-R12, AC04): a text
 * conflict's three sides (main's, this change's, and the agent's own resolution marked as the
 * agent's for a person to take), resolved by taking a side — main, this, both, the agent's — or an
 * edit; binary, modify/delete and the rest named and resolved by taking a side, never markers.
 * Taking a side is a React Action on the conflicts endpoints; once each is taken, the gate row's
 * Approve place holds "Commit resolution", which makes the revision.
 */
import * as React from "react";
import { BadgeCheck, Check, GitMerge, Pencil, RotateCcw, X } from "lucide-react";
import { conflictsApi, type Conflict, type ConflictTake } from "@/api/review";
import { invalidate } from "@/lib/resource";
import "./review.css";

const said = (e: unknown) => (e instanceof Error ? e.message : String(e));

/** How a take reads ("main's", "this change's", …) once the conflict is resolved. */
const TOOK: Record<ConflictTake, string> = { main: "main's", this: "this change's", both: "both", agent: "the agent's", edit: "edited" };

const KIND_SAYS: Record<Conflict["kind"], string> = {
  text: "conflict",
  binary: "binary conflict",
  modify_delete: "modify/delete conflict",
  delete_modify: "delete/modify conflict",
  other: "conflict",
};

function sideLines(s: Conflict["main"]): string[] {
  return s && "lines" in s ? (s.lines ?? []) : [];
}

function Lines({ lines, k }: { lines: string[]; k: "a" | "d" }) {
  if (!lines.length) return null;
  return (
    <div className="rv-code">
      {lines.map((t, i) => (
        <div key={i} className={`rv-l ${k}`}>
          <i />
          <code>{t}</code>
        </div>
      ))}
    </div>
  );
}

export function ConflictBlock({ cf, n, diffKey, onDone }: { cf: Conflict; n: number; diffKey: string; onDone?: () => void }) {
  const [edit, setEdit] = React.useState(false);
  const [text, setText] = React.useState("");
  const [err, setErr] = React.useState<string | null>(null);
  const [busy, start] = React.useTransition();
  const run = (f: () => Promise<unknown>) =>
    start(async () => {
      setErr(null);
      try {
        await f();
        invalidate("review/");
        invalidate("ship/");
        onDone?.();
      } catch (e) {
        setErr(said(e));
      }
    });
  const take = (t: ConflictTake) => run(() => conflictsApi.resolve(diffKey, cf.id, { take: t }));
  const text_ = cf.kind === "text";
  const res = cf.resolution;

  if (res) {
    const resolved =
      res.take === "both"
        ? [...sideLines(cf.main), ...sideLines(cf.this)]
        : (res.lines ?? (res.take === "main" ? sideLines(cf.main) : res.take === "this" ? sideLines(cf.this) : res.take === "agent" ? (cf.agent?.lines ?? []) : []));
    return (
      <div className="rv-cf" data-done="1">
        <div className="rv-cfh">
          <Check size={13} aria-hidden="true" />
          <span>
            {cf.hunk != null ? `${cf.hunk + 1}/${n} · ` : ""}
            {TOOK[res.take]}
            {res.by ? ` · ${res.by.replace(/^user:/, "")}` : ""}
          </span>
          <button type="button" className="rv-ib" aria-label="Undo" title="Undo" disabled={busy} onClick={() => run(() => conflictsApi.undo(diffKey, cf.id))}>
            <RotateCcw size={13} aria-hidden="true" />
          </button>
        </div>
        {text_ && <Lines lines={resolved} k="a" />}
        {!text_ && <p className="rv-cf-says">{sayOf(cf)}</p>}
      </div>
    );
  }

  const says_ = cf.says ?? cf.git ?? KIND_SAYS[cf.kind];
  return (
    <div className="rv-cf">
      <div className="rv-cfh">
        <GitMerge size={13} aria-hidden="true" />
        <span>{cf.hunk != null ? `conflict ${cf.hunk + 1}/${n}` : KIND_SAYS[cf.kind]}</span>
      </div>
      {text_ ? (
        <>
          <div className="rv-cfs" data-side="main">
            <span className="rv-cfl">main{cf.base && "lines" in cf.base && cf.base.lines.length ? ` · its base` : ""}</span>
            <Lines lines={sideLines(cf.main)} k="d" />
          </div>
          <div className="rv-cfs" data-side="this">
            <span className="rv-cfl">this change</span>
            <Lines lines={sideLines(cf.this)} k="a" />
          </div>
          {cf.agent?.lines && (
            <div className="rv-cfs" data-side="agent">
              <span className="rv-cfl">
                <BadgeCheck size={11} aria-hidden="true" /> {cf.agent.by ?? "an agent"}'s resolution — a person takes it
              </span>
              <Lines lines={cf.agent.lines} k="a" />
            </div>
          )}
        </>
      ) : (
        <p className="rv-cf-says">{says_}</p>
      )}
      {edit ? (
        <div className="rv-edit">
          <textarea className="rv-ta rv-cta" rows={Math.max(4, sideLines(cf.this).length + 1)} value={text} onChange={(e) => setText(e.target.value)} aria-label="The resolution" />
          <div className="rv-btns">
            <button type="button" className="rv-btn rv-btn-go" disabled={busy} onClick={() => run(() => conflictsApi.resolve(diffKey, cf.id, { take: "edit", text }))}>
              <Check size={13} aria-hidden="true" />
              <span>Use</span>
            </button>
            <button type="button" className="rv-ib" aria-label="Cancel" onClick={() => setEdit(false)}>
              <X size={14} aria-hidden="true" />
            </button>
          </div>
        </div>
      ) : (
        <div className="rv-btns">
          <span className="rv-seg">
            {cf.takes
              .filter((t) => t !== "edit")
              .map((t) => (
                <button
                  key={t}
                  type="button"
                  data-rec={t === "agent" || undefined}
                  disabled={busy}
                  title={t === "agent" ? "the agent's resolution — a person takes it" : undefined}
                  onClick={() => take(t)}
                >
                  {t === "main" ? "main" : t === "this" ? "this" : t === "both" ? "both" : "agent's"}
                </button>
              ))}
          </span>
          {text_ && (
            <button
              type="button"
              className="rv-ib"
              aria-label="Edit"
              title="Edit"
              disabled={busy}
              onClick={() => {
                setText(sideLines(cf.this).join("\n"));
                setEdit(true);
              }}
            >
              <Pencil size={13} aria-hidden="true" />
            </button>
          )}
        </div>
      )}
      {err && (
        <p className="rvc-error" role="alert">
          {err}
        </p>
      )}
    </div>
  );
}

function sayOf(cf: Conflict): string {
  return cf.says ?? cf.git ?? "resolved by taking a side";
}

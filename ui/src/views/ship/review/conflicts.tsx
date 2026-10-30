/**
 * A merge conflict where it falls in a Ship item's Files (docs/ci.md W26-R12, AC04): a text
 * conflict's three sides (main's, this change's, and the agent's own resolution marked as the
 * agent's for a person to take), resolved by taking a side — main, this, both, the agent's — or an
 * edit; binary, modify/delete and the rest named and resolved by taking a side, never markers.
 * Taking a side is a React Action on the conflicts endpoints; once each is taken, the gate row's
 * Approve place holds "Commit resolution", which makes the revision.
 *
 * The conflict ladder (W26-R17) says on one line of its own, at one height from the first paint,
 * what its rungs made of the conflict (kiln's structural merge, a remembered resolution, an agent):
 * at work, kept and why, or a resolution under test, passed or failed. A tested resolution shows
 * with its explanation, and "Use the agent's" (or kiln's) takes it; drawn but disabled, saying why,
 * until there's one whose tests passed. The buttons sit in a grid of fixed rows.
 */
import * as React from "react";
import { BadgeCheck, Check, GitMerge, Pencil, RotateCcw, X } from "lucide-react";
import { conflictsApi, type Conflict, type ConflictTake, type LadderTry } from "@/api/review";
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

/** Who made a rung's try, in words. */
function whoOf(o: LadderTry): string {
  if (o.rung === "structural") return "kiln's structural merge";
  if (o.rung === "remembered") return "a remembered resolution";
  const name = o.by.replace(/^agent:/, "").split(" ")[0];
  return name && name !== "kiln" ? `agent ${name}` : "an agent";
}

/** A try's resolution, tested: whether a person may use it. */
const usable = (o: LadderTry | null | undefined): o is LadderTry => !!o && o.state === "resolved" && (o.tests === "passed" || o.tests === "none_to_run") && !!o.lines;

/** The ladder's line for a conflict: where its rungs are, in words (one line, always drawn). */
function rungLine(o: LadderTry | null | undefined): { k: string; t: string } {
  if (!o) return { k: "none", t: "kiln's merge, then what it remembers, then an agent try it first" };
  const who = whoOf(o);
  if (o.state === "working") return { k: "working", t: `${who} · resolving…` };
  if (o.state === "kept") return { k: "kept", t: `${who} kept the markers${o.why ? `: ${o.why}` : ""}` };
  if (o.tests === "pending" || o.tests == null) return { k: "testing", t: `${who} · testing…` };
  if (o.tests === "failed") return { k: "failed", t: `${who} · ✗ its tests failed` };
  if (o.tests === "none_to_run") return { k: "passed", t: `${who} · no test touches it` };
  return { k: "passed", t: `${who} · ✓ its tests passed` };
}

/** "Take D41" for a change named so; else "Take this change". */
const thisName = (key: string) => (/^(D|CR)\d+$/.test(key) ? key : "this change");

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
  const rung = rungLine(cf.offered);
  // "Use the agent's": the ladder's tested resolution, else an agent's own a person may take.
  const offered = cf.offered;
  const useIt: { ok: boolean; why: string; go?: () => void } = usable(offered)
    ? { ok: true, why: `${whoOf(offered)}'s resolution, its tests passed`, go: () => run(() => conflictsApi.resolve(diffKey, cf.id, { take: "edit", lines: offered.lines ?? [] })) }
    : cf.takes.includes("agent")
      ? { ok: true, why: "the agent's resolution — a person takes it", go: () => take("agent") }
      : { ok: false, why: offered ? rung.t : "no resolution of the ladder's has passed its tests yet" };

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
          {usable(cf.offered) ? (
            <div className="rv-cfs" data-side="offered">
              <span className="rv-cfl">
                <BadgeCheck size={11} aria-hidden="true" /> {whoOf(cf.offered)}'s resolution — a person takes it
              </span>
              <Lines lines={cf.offered.lines ?? []} k="a" />
              {cf.offered.explanation && <p className="rv-cf-says">{cf.offered.explanation}</p>}
            </div>
          ) : (
            cf.agent?.lines && (
              <div className="rv-cfs" data-side="agent">
                <span className="rv-cfl">
                  <BadgeCheck size={11} aria-hidden="true" /> {cf.agent.by ?? "an agent"}'s resolution — a person takes it
                </span>
                <Lines lines={cf.agent.lines} k="a" />
              </div>
            )
          )}
        </>
      ) : (
        <p className="rv-cf-says">{says_}</p>
      )}
      <p className="rv-rung" data-k={rung.k} title={rung.t}>
        {rung.t}
      </p>
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
        <div className="rv-cfb">
          {cf.takes.includes("main") && (
            <button type="button" className="rv-btn" disabled={busy} onClick={() => take("main")}>
              Take main
            </button>
          )}
          {cf.takes.includes("this") && (
            <button type="button" className="rv-btn" disabled={busy} onClick={() => take("this")}>
              Take {thisName(diffKey)}
            </button>
          )}
          {cf.takes.includes("both") && (
            <button type="button" className="rv-btn" disabled={busy} onClick={() => take("both")}>
              Both
            </button>
          )}
          <button type="button" className="rv-btn" data-rec={useIt.ok || undefined} disabled={busy || !useIt.ok} title={useIt.why} onClick={() => useIt.go?.()}>
            {cf.offered && cf.offered.rung !== "agent" ? "Use kiln's" : "Use the agent's"}
          </button>
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

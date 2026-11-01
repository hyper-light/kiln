/**
 * A repository's commits, the first section of its opened row (docs/ci.md W26-R29; Ada, 2026-11-01:
 * "each item maps to a repo, and commits are browsable from within there"): newest first, a page at
 * a time (5, 10, 25, 50 or 100), each line its checks' glyph, sha, message, author, machine, run time
 * and when, as Runs lists them; two lines on a phone. Picking a line fills the sections below with
 * its commit. Paging moves the list only: the page before stays until the next is in (a transition),
 * the press saying meanwhile that it's on its way (W21-R15), and the list keeps a page's height, so
 * nothing beside it moves. A page keeps the commits it showed: one that arrives meanwhile waits for
 * Newer.
 */
import * as React from "react";
import { whoSaid } from "@/api/review";
import { shipApi, type ShipCommit, type ShipQuery } from "@/api/ship";
import { Loading } from "@/components/ui/loading";
import { StatusGlyph } from "@/components/ui/status-glyph";
import { STATES, type GlyphState } from "@/components/ui/status-glyph-spec";
import { formatDuration, spanSeconds } from "@/lib/format";
import { useNow } from "@/lib/hooks";
import { sendingProps } from "@/lib/action";
import { useSuspenseResource } from "@/lib/resource";
import { RelTime } from "../shared";
import { SectionHead } from "./head";

/** The facets the server narrows a repository's commits by: the list's own (host, kind). */
export type Narrow = Pick<ShipQuery, "host" | "kind">;

/** A page's sizes, as the strip offers them. */
export const PER_PAGE = [5, 10, 25, 50, 100] as const;
export type PerPage = (typeof PER_PAGE)[number];

/** What turns the page: Newer, Older, or a page's size. */
type Turn = "newer" | "older" | PerPage;

/** A commit's checks, as its line's glyph says them: its journey's checks pip. */
export function glyphOf(c: ShipCommit): GlyphState {
  const s = c.pips.find((p) => p.stage === "checks")?.state ?? "none";
  return (STATES as readonly string[]).includes(s) ? (s as GlyphState) : "none";
}

/** The machines its jobs ran on, as one cell says them. */
const machinesOf = (c: ShipCommit) => (c.machines ?? []).join(", ");

/** A run still going: its time so far, as the clock goes. */
function Ticking({ from }: { from: number }) {
  const now = useNow();
  return <>{formatDuration(spanSeconds(from, null, now))}…</>;
}

/** How long its jobs ran: first start to last end, or so far. */
function RunTime({ ran }: { ran: ShipCommit["ran"] }) {
  if (!ran) return <>—</>;
  if (ran.to === null) return <Ticking from={ran.from} />;
  return <>{formatDuration(ran.to - ran.from)}</>;
}

export function ShipCommits({ repo, narrow, picked, onPick }: { repo: string; narrow: Narrow; picked: string; onPick: (c: ShipCommit) => void }) {
  const [per, setPer] = React.useState<PerPage>(10);
  // The cursors of the pages before this one, newest first: Older adds one, Newer takes one off.
  const [before, setBefore] = React.useState<number[]>([]);
  const [paging, startTransition] = React.useTransition();
  const [pressed, setPressed] = React.useState<Turn | null>(null);
  const turn = (by: Turn, to: () => void) => {
    setPressed(by);
    startTransition(to);
  };
  // Other facets are other commits: the first page again.
  const facets = `${narrow.host ?? ""}|${narrow.kind ?? ""}`;
  const [seen, setSeen] = React.useState(facets);
  if (seen !== facets) {
    setSeen(facets);
    setBefore([]);
  }
  const cursor = seen === facets ? before.at(-1) : undefined;
  const q: ShipQuery = cursor === undefined ? { repo, ...narrow, limit: per } : { repo, ...narrow, limit: per, cursor };
  const key = `ship/commits/${JSON.stringify(q)}`;
  // A line says its checks, run time and machines: runs' and jobs' events move them, not a review's.
  const page = useSuspenseResource(key, (s) => shipApi.commits(q, s), { pollMs: 15_000, on: ["run", "job"] });
  const fetched = page.data.commits;
  // A page keeps the commits it showed first, each with its latest data, as the list keeps its rows:
  // a commit that arrives meanwhile waits for Newer, so no line moves under the one you're reading.
  const [held, setHeld] = React.useState(() => ({ key, lines: fetched }));
  if (held.key !== key) setHeld({ key, lines: fetched });
  const latest = new Map(fetched.map((c) => [c.sha, c]));
  const lines = (held.key === key ? held.lines : fetched).map((c) => latest.get(c.sha) ?? c);
  // Newer than the first it shows: in what came since, above it.
  const arrived = before.length === 0 && lines.length > 0 && fetched.findIndex((c) => c.sha === lines[0]?.sha) !== 0;
  const last = lines.at(-1)?.cursor;
  const next = page.data.next_cursor === null || last === undefined ? null : last;
  const first = before.length * per + 1;
  // A press acts on the page shown: pressed again before the next is in, it does nothing more.
  const depth = before.length;
  return (
    <Frame
      repo={repo}
      per={per}
      onPer={(n) =>
        turn(n, () => {
          setPer(n);
          setBefore([]);
        })
      }
      newer={depth === 0 && !arrived ? null : () => (depth === 0 ? setHeld({ key, lines: fetched }) : turn("newer", () => setBefore((b) => (b.length === depth ? b.slice(0, -1) : b))))}
      range={lines.length > 0 ? `${first}–${first + lines.length - 1}` : "—"}
      older={next === null ? null : () => turn("older", () => setBefore((b) => (b.length !== depth ? b : [...b, next])))}
      turning={paging ? pressed : null}
    >
      {lines.map((c) => {
        const on = machinesOf(c) || "—";
        return (
          <li key={c.sha} className="sc-c" data-sel={c.sha === picked || undefined}>
            <button type="button" className="sc-b" aria-pressed={c.sha === picked} onClick={() => onPick(c)}>
              <StatusGlyph state={glyphOf(c)} size={14} />
              <span className="sc-sha">{c.short}</span>
              <span className="sc-msg">{c.message || c.change?.title || c.short}</span>
              <span className="sc-who">{whoSaid(c.author)}</span>
              <span className="sc-on" title={on}>
                {on}
              </span>
              <span className="sc-took">
                <RunTime ran={c.ran} />
              </span>
              <RelTime className="sc-ago" ts={c.at} />
              <span className="sc-l2">
                <b>{c.short}</b> · {whoSaid(c.author)} · {on} · <RunTime ran={c.ran} />
              </span>
            </button>
          </li>
        );
      })}
    </Frame>
  );
}

/**
 * The commits while they load: the section itself, its controls dimmed and its list a page tall
 * and empty, so nothing below moves when they come (Loadable's `fallback`).
 */
export function ShipCommitsSlot({ repo }: { repo: string }) {
  return (
    <Frame repo={repo} per={10} onPer={null} newer={null} range="—" older={null}>
      <li className="sc-wait">
        <Loading label="Loading its commits" />
      </li>
    </Frame>
  );
}

/**
 * The section, loaded or not: its head with the page sizes, the columns' names, the list (at least
 * a page tall: `--sc-per`) and the pager. A null action's control is dimmed, never gone; the one
 * whose page is on its way (`turning`) says so.
 */
function Frame({
  repo,
  per,
  onPer,
  newer,
  range,
  older,
  turning = null,
  children,
}: {
  repo: string;
  per: PerPage;
  onPer: ((n: PerPage) => void) | null;
  newer: (() => void) | null;
  range: string;
  older: (() => void) | null;
  turning?: Turn | null;
  children: React.ReactNode;
}) {
  return (
    <section aria-label="Commits" className="ship-sec ship-commits">
      <SectionHead label="Commits">
        <span className="sc-per" role="group" aria-label="Commits a page">
          <span>per page</span>
          {PER_PAGE.map((n, i) => (
            <React.Fragment key={n}>
              {i > 0 && <span aria-hidden="true">·</span>}
              <button type="button" aria-pressed={per === n} aria-label={`${n} a page`} disabled={onPer === null} {...sendingProps(turning === n)} onClick={() => onPer?.(n)}>
                {n}
              </button>
            </React.Fragment>
          ))}
        </span>
      </SectionHead>
      <div className="sc-body">
        <div className="sc-h" aria-hidden="true">
          <span />
          <span>commit</span>
          <span>message</span>
          <span>author</span>
          <span>machine</span>
          <span>run time</span>
          <span>when</span>
        </div>
        <ol className="sc-list" aria-label={`${repo}'s commits`} style={{ "--sc-per": per } as React.CSSProperties}>
          {children}
        </ol>
        <div className="sc-pager">
          <button type="button" disabled={newer === null} {...sendingProps(turning === "newer")} onClick={() => newer?.()}>
            Newer
          </button>
          <span>{range}</span>
          <button type="button" disabled={older === null} {...sendingProps(turning === "older")} onClick={() => older?.()}>
            Older
          </button>
        </div>
      </div>
    </section>
  );
}

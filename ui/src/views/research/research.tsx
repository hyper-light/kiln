/**
 * Research (docs/research.md), as a person weighs it: what was asked, each finding with kiln's grade
 * (A to F, out of 100) and what makes it (its sources' kinds, how far its citations held up when
 * kiln looked for each quote where it says it is, how many independent sources agree, what's
 * against it), each quote under it with the source it's from, and the sources, numbered: inside
 * (the machines and systems the agent works in) or outside. Kept live while quotes are being looked
 * for.
 *
 * Where it shows: its plan's page; its agent's Plan tab (every piece kept, to search, a card each); its agent's
 * conversation, where the agent records or reads it (in brief, in full on asking); a planned goal
 * on its stack (in a line); and a page of its own, its way back to its agent's plans.
 */
import * as React from "react";
import { ChevronRight } from "lucide-react";
import { Fold } from "@/views/harness/fold";
import { useParams, Link } from "@/lib/router";
import { ApiError } from "@/api/client";
import { planHref } from "@/api/plans";
import { INSIDE, SOURCE_WORDS, researchApi, researchHref, type Cite, type CheckState, type Finding, type Research, type ResearchBrief, type ResearchSource } from "@/api/research";
import { Loadable } from "@/components/ui/loading";
import { RowsHold } from "@/components/ui/hold";
import { Button } from "@/components/ui/button";
import { FacetBox, type Chosen } from "@/components/ui/facet-box";
import { EmptyState, FilterBar } from "@/components/ui/misc";
import { relativeTime } from "@/lib/format";
import { useSuspenseResource } from "@/lib/resource";
import { cn } from "@/lib/utils";
import { Shimmer } from "@/views/harness/signals";
import { SessionCrumbs } from "@/views/plans/crumbs";
import { facetsOf, hasChosen, researchDimensions, useAsked } from "@/views/plans/browse";
import "./research.css";

const CHECK_WORDS: Record<CheckState, string> = {
  found: "quote found",
  not_found: "quote not found there",
  gone: "not there",
  exists: "there",
  pending: "checking",
  unchecked: "not checked",
};

const internal = (s: ResearchSource | undefined) => !!s && INSIDE.includes(s.type);

const isUrl = (l: string) => /^https?:\/\//.test(l);

/** Where a source is: a link when it's a page. */
function Locator({ s }: { s: ResearchSource }) {
  const l = s.locator;
  if (isUrl(l))
    return (
      <a href={l} target="_blank" rel="noreferrer noopener" className="rs-loc">
        {l.replace(/^https?:\/\//, "")}
      </a>
    );
  if (l.startsWith("doi:"))
    return (
      <a href={`https://doi.org/${l.slice(4).trim()}`} target="_blank" rel="noreferrer noopener" className="rs-loc">
        {l}
      </a>
    );
  return <span className="rs-loc">{l}</span>;
}

function Meter({ value, against }: { value: number; against?: boolean }) {
  return (
    <span className="rs-meter">
      <i data-against={against ? "" : undefined} style={{ width: `${Math.round(Math.max(0, Math.min(1, value)) * 100)}%` }} />
    </span>
  );
}

function CiteLine({ c, number, sources }: { c: Cite; number: (id: string) => number; sources: ResearchSource[] }) {
  const s = sources.find((x) => x.id === c.source);
  const state = c.check?.state;
  return (
    <li className="rs-cite">
      <span className="rs-num">[{number(c.source)}]</span>
      <span>
        {c.quote ? <q className="rs-quote">{c.quote}</q> : <span className="rs-dim">{s?.title ?? c.source}</span>}
        {c.at && <span className="rs-dim"> · {c.at}</span>}
        {state && (
          <span className="rs-check" data-state={state} title={c.check?.said}>
            {" "}
            · {CHECK_WORDS[state]}
          </span>
        )}
      </span>
    </li>
  );
}

function FindingCard({ f, sources }: { f: Finding; sources: ResearchSource[] }) {
  const c = f.confidence;
  const number = (id: string) => sources.findIndex((s) => s.id === id) + 1;
  const kinds = [
    ...new Set(
      f.cites
        .map((x) => sources.find((s) => s.id === x.source))
        .filter((s): s is ResearchSource => !!s)
        .map((s) => SOURCE_WORDS[s.type]),
    ),
  ];
  const looked = f.cites.filter((x) => x.check && x.check.state !== "pending" && x.check.state !== "unchecked").length;
  return (
    <li className="rs-finding" data-grade={c.grade}>
      <div className="rs-claim-row">
        <span className="rs-claim">
          {f.claim}
          {f.inferred && <span className="rs-dim"> (inferred)</span>}
        </span>
        <span className="rs-grade" title={c.why}>
          <b data-g={c.grade}>{c.grade}</b>
          <span className="rs-score">
            {c.score} · {c.words}
          </span>
        </span>
      </div>
      <div className="rs-factors">
        <span>Sources</span>
        <Meter value={c.reliability} />
        <span>{kinds.length ? kinds.join(", ") : "none"}</span>
        <span>Citations</span>
        <Meter value={c.citation} />
        <span data-bad={c.failed ? "" : undefined}>
          {looked ? `${c.found} of ${f.cites.length} quotes found${c.failed ? `, ${c.failed} not` : ""}` : f.cites.some((x) => x.check?.state === "pending") ? "checking quotes" : "not checked"}
        </span>
        <span>Agree</span>
        <Meter value={Math.min(1, c.independent / 3)} />
        <span>
          {c.independent} independent source{c.independent === 1 ? "" : "s"}
        </span>
        <span>Against</span>
        <Meter value={c.against} against />
        <span data-bad={c.against >= 0.25 ? "" : undefined}>{f.against.length ? `${f.against.length} counterclaim${f.against.length === 1 ? "" : "s"}` : "none found"}</span>
      </div>
      {f.cites.length > 0 && (
        <ol className="rs-cites">
          {f.cites.map((x, i) => (
            <CiteLine key={i} c={x} number={number} sources={sources} />
          ))}
        </ol>
      )}
      {f.against.map((k, i) => (
        <p key={i} className="rs-against">
          Against: {k.claim} {k.cites.map((x) => `[${number(x.source)}]`).join("")}
          <span className="rs-dim"> · weighs {Math.round((f.against_weights[i] ?? 0) * 100)}%</span>
        </p>
      ))}
      {f.note && <p className="rs-dim rs-note">{f.note}</p>}
    </li>
  );
}

/** Its findings' grades, as many of each: "A 2 · B 3 · C 1". */
const gradeWords = (r: { summary: Research["summary"] }) =>
  (["A", "B", "C", "D", "F"] as const)
    .filter((g) => r.summary.grades[g])
    .map((g) => `${g} ${r.summary.grades[g]}`)
    .join(" · ");

/** What it comes to: its findings and sources (inside, outside), and their grades. */
function ResearchHead({ r, live }: { r: Research; live?: boolean }) {
  const s = r.summary;
  return (
    <div className="rs-head">
      <Shimmer on={!!live} className="sg-label">
        {live ? "Researching" : "Research"} · {s.findings} finding{s.findings === 1 ? "" : "s"} · {s.sources} source{s.sources === 1 ? "" : "s"} ({s.internal} inside, {s.external} outside)
      </Shimmer>
      {gradeWords(r) && <span className="rs-grades">{gradeWords(r)}</span>}
    </div>
  );
}

/** Research, graded, its sources numbered. `live`: it's still being done. */
export function ResearchView({ r, live }: { r: Research; live?: boolean }) {
  return (
    <div className="rs">
      <ResearchHead r={r} live={live} />
      <p className="rs-question">{r.question}</p>
      <ResearchBody r={r} />
    </div>
  );
}

/** What research found: each finding with its grade and quotes, what had no sources, the sources. */
function ResearchBody({ r }: { r: Research }) {
  const s = r.summary;
  return (
    <>
      {s.checking > 0 && (
        <p className="rs-dim">
          kiln is looking for {s.checking} quote{s.checking === 1 ? "" : "s"} where they're cited…
        </p>
      )}
      {r.findings.length === 0 ? (
        <p className="rs-dim">No findings yet.</p>
      ) : (
        <ol className="rs-findings">
          {r.findings.map((f) => (
            <FindingCard key={f.id} f={f} sources={r.sources} />
          ))}
        </ol>
      )}
      {(r.skipped.internal || r.skipped.external) && (
        <div className="rs-skipped">
          {r.skipped.internal && <p>No sources inside: {r.skipped.internal}</p>}
          {r.skipped.external && <p>No sources outside: {r.skipped.external}</p>}
        </div>
      )}
      {r.sources.length > 0 && (
        <>
          <h3 className="rs-sub">Sources</h3>
          <ol className="rs-sources">
            {r.sources.map((src, i) => (
              <li key={src.id}>
                <span className="rs-num">[{i + 1}]</span>
                <span>
                  <span className="rs-kind" data-inside={internal(src) ? "" : undefined}>
                    {SOURCE_WORDS[src.type]}
                  </span>
                  {src.title !== src.locator && <span className="rs-title">{src.title}</span>} <Locator s={src} />
                  {src.published && <span className="rs-dim"> · {src.published}</span>}
                  {src.dated && <span className="rs-bad"> · out of date</span>}
                  {src.machine && <span className="rs-dim"> · on {src.machine}</span>}
                </span>
              </li>
            ))}
          </ol>
        </>
      )}
    </>
  );
}

/** Research in brief: what it comes to, what was asked, its first findings with their grades. */
export function ResearchBrief({ r, live }: { r: Research; live?: boolean }) {
  return (
    <div className="rs">
      <ResearchHead r={r} live={live} />
      <p className="rs-question">{r.question}</p>
      {r.findings.length > 0 && (
        <ol className="rs-claims">
          {r.findings.slice(0, 3).map((f) => (
            <li key={f.id}>
              <span className="rs-grade rs-grade-sm" title={f.confidence.why}>
                <b data-g={f.confidence.grade}>{f.confidence.grade}</b>
              </span>
              <span>{f.claim}</span>
            </li>
          ))}
        </ol>
      )}
      {r.findings.length > 3 && <p className="rs-dim rs-more">and {r.findings.length - 3} more</p>}
    </div>
  );
}

/** Where to open research: its page, and its plan's. */
function ResearchLinks({ r, children }: { r: Research; children?: React.ReactNode }) {
  return (
    <p className="rs-acts">
      {children}
      <Link to={researchHref(r.id)} className="rs-link">
        Open it
      </Link>
      {r.plan && (
        <Link to={planHref(r.plan)} className="rs-link">
          Its plan
        </Link>
      )}
    </p>
  );
}

const unlessGone = (e: unknown): "gone" => {
  if (e instanceof ApiError && (e.status === 404 || e.status === 400)) return "gone";
  throw e;
};

/** Research `id`, read again every few seconds while quotes are being looked for or it's `live`. */
export function ResearchPanel({ id, live }: { id: string; live?: boolean }) {
  return (
    <Loadable label="its research" fallback={<RowsHold rows={3} rowHeight={64} />}>
      <ResearchLoaded id={id} live={live} />
    </Loadable>
  );
}

function ResearchLoaded({ id, live }: { id: string; live?: boolean }) {
  const res = useSuspenseResource(`research/${id}`, (s) => researchApi.get(id, s).catch(unlessGone), { pollMs: 4000 });
  if (res.data === "gone") return <p className="rs-dim">Its research is gone.</p>;
  return <ResearchView r={res.data} live={live} />;
}

/** Research `id` where its agent records or reads it (its conversation): in brief, in full on asking. */
export function ResearchCard({ id }: { id: string }) {
  return (
    <Loadable label="its research" fallback={<RowsHold rows={1} rowHeight={56} />}>
      <ResearchCardLoaded id={id} />
    </Loadable>
  );
}

function ResearchCardLoaded({ id }: { id: string }) {
  const [open, setOpen] = React.useState(false);
  const res = useSuspenseResource(`research/${id}`, (s) => researchApi.get(id, s).catch(unlessGone), { pollMs: 4000 });
  if (res.data === "gone") return <p className="rs-dim">Its research is gone.</p>;
  const r = res.data;
  return (
    <div className="rs-card">
      {open ? <ResearchView r={r} live={r.summary.checking > 0} /> : <ResearchBrief r={r} live={r.summary.checking > 0} />}
      <ResearchLinks r={r}>
        <button type="button" className="rs-link" aria-expanded={open} onClick={() => setOpen(!open)}>
          {open ? "Less" : "Each finding, its grade and sources"}
        </button>
      </ResearchLinks>
    </div>
  );
}

/** Research in an agent's Plan tab: the newest kiln keeps, in brief, found by their words and facets (whose, their grades, their plan), a card each, opened in place, read in full, to each finding, its grade and sources. */
export function ResearchSection({ agent }: { agent: string }) {
  const [typed, asked, type] = useAsked();
  const [chosen, setChosen] = React.useState<Chosen>({});
  // While the words asked for are read, what was found before stays.
  const words = React.useDeferredValue(asked);
  return (
    <Loadable
      label="research"
      fallback={
        <Fold id="research" title="Research">
          <RowsHold rows={2} rowHeight={96} />
        </Fold>
      }
    >
      <ResearchFound agent={agent} words={words} typed={typed} type={type} chosen={chosen} onChosen={setChosen} />
    </Loadable>
  );
}

/** Cards shown before "Show more", and how many more each time. */
const CARDS = 4;
const MORE_CARDS = 10;

function ResearchFound({ agent, words, typed, type, chosen, onChosen }: { agent: string; words: string; typed: string; type: (w: string) => void; chosen: Chosen; onChosen: (c: Chosen) => void }) {
  const all = useSuspenseResource(`research/briefs/${words}`, (s) => researchApi.briefs(words, s), { pollMs: 15_000 }).data.research;
  const [open, setOpen] = React.useState<ReadonlySet<string>>(new Set());
  const [more, setMore] = React.useState(0);
  const dims = React.useMemo(() => researchDimensions(agent), [agent]);
  const facets = React.useMemo(() => facetsOf(dims, all, chosen), [dims, all, chosen]);
  const shown = all.filter((r) => hasChosen(dims, chosen, r));
  const limit = CARDS + more;
  const rest = shown.length - limit;
  const toggle = (id: string) => {
    const next = new Set(open);
    if (!next.delete(id)) next.add(id);
    setOpen(next);
  };
  return (
    <Fold id="research" title="Research" count={all.length && !words ? all.length : undefined}>
      <div className="rb">
        <FilterBar>
          <FacetBox label="Search research" placeholder="Search research by question, finding or source…" query={typed} onQuery={type} facets={facets} chosen={chosen} onChosen={onChosen} />
        </FilterBar>
        {shown.length === 0 ? (
          <p className="in-none">{words || Object.values(chosen).some((c) => c.length) ? "No research matches." : "No research yet: what agents find out, graded by kiln, shows here."}</p>
        ) : (
          <ul className="rb-list">
            {shown.slice(0, limit).map((r) => (
              <ResearchItem key={r.id} r={r} agent={agent} open={open.has(r.id)} onOpen={() => toggle(r.id)} />
            ))}
          </ul>
        )}
        {rest > 0 && (
          <div className="plan-actions">
            <Button size="xs" variant="ghost" onClick={() => setMore(more + MORE_CARDS)}>
              {rest > MORE_CARDS ? `Show ${MORE_CARDS} more of ${rest}` : `Show ${rest} more`}
            </Button>
          </div>
        )}
      </div>
    </Fold>
  );
}

/** Research as a card: whose and when, what was asked, a segment a finding in its grade's colour and the strongest one; opened, each finding, its grade and sources. */
function ResearchItem({ r, agent, open, onOpen }: { r: ResearchBrief; agent: string; open: boolean; onOpen: () => void }) {
  const s = r.summary;
  const live = s.checking > 0;
  const whose = r.agent === agent ? "this agent" : r.agent ? "another agent" : r.by;
  const tally = gradeWords(r);
  return (
    <li className="rb-card" data-open={open || undefined}>
      <button type="button" className="rb-head" aria-expanded={open} onClick={onOpen}>
        <span className="rb-eyebrow">
          <Shimmer on={live}>{[live ? "checking quotes" : "", whose, relativeTime(r.updated_at / 1000)].filter(Boolean).join(" · ")}</Shimmer>
          {tally && <span className="rs-grades">{tally}</span>}
        </span>
        <span className="rb-q">{r.question}</span>
        {r.grades.length > 0 && (
          <span className="rb-strip" aria-hidden="true">
            {r.grades.map((g, i) => (
              <i key={i} data-g={g} />
            ))}
          </span>
        )}
        {r.best && !open && (
          <span className="rb-best">
            <span className="rs-grade rs-grade-sm">
              <b data-g={r.grades[0]}>{r.grades[0]}</b>
            </span>
            <span>{r.best}</span>
          </span>
        )}
        <span className="rb-foot">
          {s.findings ? `${s.findings} finding${s.findings === 1 ? "" : "s"}` : "No findings yet"} · {s.sources} source{s.sources === 1 ? "" : "s"}
          {s.sources > 0 && ` (${s.internal} inside, ${s.external} outside)`}
        </span>
        <ChevronRight size={14} className={cn("rb-chev hx-chev", open && "rotate-90")} aria-hidden="true" />
      </button>
      {open && (
        <div className="rb-body">
          <Loadable label="the research" fallback={<RowsHold rows={2} rowHeight={56} />}>
            <ResearchDetails id={r.id} />
          </Loadable>
        </div>
      )}
    </li>
  );
}

/** Research `id` read in full, opened in its card: each finding, its grade and quotes, its sources; read again every few seconds. */
function ResearchDetails({ id }: { id: string }) {
  const res = useSuspenseResource(`research/${id}`, (s) => researchApi.get(id, s).catch(unlessGone), { pollMs: 4000 });
  if (res.data === "gone") return <p className="rs-dim">Its research is gone.</p>;
  return (
    <div className="rs">
      <ResearchBody r={res.data} />
      <ResearchLinks r={res.data} />
    </div>
  );
}

/** A plan's research in a line: to open, with what it comes to. */
export function ResearchLine({ id }: { id: string }) {
  const res = useSuspenseResource(`research/${id}`, (s) => researchApi.get(id, s).catch(unlessGone), { pollMs: 4000 });
  if (res.data === "gone") return null;
  const r = res.data;
  return (
    <>
      <Link to={researchHref(id)}>research</Link>: {r.summary.findings} finding{r.summary.findings === 1 ? "" : "s"}
      {gradeWords(r) && ` (${gradeWords(r)})`}
    </>
  );
}

/** Research on a page of its own. */
export default function ResearchPage() {
  const { id = "" } = useParams();
  return (
    <div className="container-console page plan-page">
      <Loadable label="the research" fallback={<RowsHold rows={4} rowHeight={72} />}>
        <ResearchOrNone key={id} id={id} />
      </Loadable>
    </div>
  );
}

function ResearchOrNone({ id }: { id: string }) {
  const res = useSuspenseResource(`research/${id}`, (s) => researchApi.get(id, s).catch(unlessGone), { pollMs: 4000 });
  const r = res.data;
  if (r === "gone")
    return (
      <>
        <SessionCrumbs />
        <EmptyState title="No such research">It may have been made on another server.</EmptyState>
      </>
    );
  return (
    <>
      <title>{`Research: ${r.question.slice(0, 60)} · kiln`}</title>
      <SessionCrumbs session={r.agent} />
      <p className="plan-dim plan-meta">
        Research {r.agent ? <>by an agent</> : <>by {r.by}</>} · changed {relativeTime(r.updated_at / 1000)}
        {r.plan && (
          <>
            {" "}
            · the planning of <Link to={planHref(r.plan)}>a plan</Link>
          </>
        )}
      </p>
      <ResearchView r={r} live={r.summary.checking > 0} />
    </>
  );
}

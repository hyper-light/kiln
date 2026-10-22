/**
 * Research (docs/research.md), as a person weighs it: what was asked, each finding with kiln's grade
 * (A to F, out of 100) and what makes it (its sources' kinds, how far its citations held up when
 * kiln looked for each quote where it says it is, how many independent sources agree, what's
 * against it), each quote under it with the source it's from, and the sources, numbered: inside
 * (the machines and systems the agent works in) or outside. On a plan's page while it's planned, and
 * on a page of its own; kept live while quotes are being looked for.
 */
import { useParams, Link } from "@/lib/router";
import { ApiError } from "@/api/client";
import { planHref } from "@/api/plans";
import { INSIDE, SOURCE_WORDS, researchApi, type Cite, type CheckState, type Finding, type Research, type ResearchSource } from "@/api/research";
import { Loadable } from "@/components/ui/loading";
import { RowsHold } from "@/components/ui/hold";
import { Crumbs, EmptyState } from "@/components/ui/misc";
import { relativeTime } from "@/lib/format";
import { useSuspenseResource } from "@/lib/resource";
import { Shimmer } from "@/views/harness/signals";
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

/** Research, graded, its sources numbered. `live`: it's still being done. */
export function ResearchView({ r, live }: { r: Research; live?: boolean }) {
  const s = r.summary;
  const grades = (["A", "B", "C", "D", "F"] as const).filter((g) => s.grades[g]).map((g) => `${g} ${s.grades[g]}`);
  return (
    <div className="rs">
      <div className="rs-head">
        <Shimmer on={!!live} className="sg-label">
          {live ? "Researching" : "Research"} · {s.findings} finding{s.findings === 1 ? "" : "s"} · {s.sources} source{s.sources === 1 ? "" : "s"} ({s.internal} inside, {s.external} outside)
        </Shimmer>
        {grades.length > 0 && <span className="rs-grades">{grades.join(" · ")}</span>}
      </div>
      <p className="rs-question">{r.question}</p>
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
    </div>
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
        <Crumbs
          index={6}
          items={[
            { label: "Agents", to: "/agents" },
            { label: "Plans", to: "/agents/plans" },
          ]}
        />
        <EmptyState title="No such research">It may have been made on another server.</EmptyState>
      </>
    );
  return (
    <>
      <title>{`Research: ${r.question.slice(0, 60)} · kiln`}</title>
      <Crumbs
        index={6}
        items={[
          { label: "Agents", to: "/agents" },
          { label: "Plans", to: "/agents/plans" },
        ]}
      />
      <p className="plan-dim plan-meta">
        Research {r.agent ? <>by an agent</> : <>by {r.by}</>} · changed {relativeTime(r.updated_at / 1000)}
        {r.plan && (
          <>
            {" "}
            · the planning of <Link to={planHref(r.plan)}>its plan</Link>
          </>
        )}
      </p>
      <ResearchView r={r} live={r.summary.checking > 0} />
    </>
  );
}

/**
 * Runners → Offers (docs/reach.md §2.1): what each runner can take now, and what waits and why.
 * One surface: the runners, each with its answer for the ask sizes in question, then the waiting
 * work by what it needs and asks, a page as long as the fleet. Static: it changes only when the
 * server says something decided by did (an `offers` event of a newer version).
 */
import type { OfferAnswer, OfferGroup, OfferRunner, OffersView } from "@/api/wire";
import { Hold, RowsHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, EmptyState, SectionHeading } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { useRes } from "@/lib/data";
import { absoluteTime, clockTime } from "@/lib/format";
import { Link, useSearchParams } from "@/lib/router";
import { offersRes } from "./data";
import { amountsSaid, answerSaid, countsSaid, groupSaid, healthSaid, healthStatus, lineSaid, needSaid } from "./words";
import "./offers.css";

const LEAD = "What each runner can take now, and what waits for room and why, as each runner's admission last said it. This page changes only when something you'd decide by does.";

export default function Offers() {
  const [params] = useSearchParams();
  const after = params.get("after");
  return (
    <div className="container-console page offers">
      <title>Offers · Runners · kiln</title>
      <header className="page-intro">
        <div className="min-w-0">
          <Crumbs index={4} items={[{ label: "Runners", to: "/runners" }, { label: "Offers" }]} />
          <h1>Offers</h1>
          <p className="lead">{LEAD}</p>
        </div>
      </header>
      <Loadable label="what runners can take" fallback={<OffersHold />}>
        <OffersBody after={after} />
      </Loadable>
    </div>
  );
}

function OffersHold() {
  return (
    <>
      <Hold height={28} label="Runners" />
      <RowsHold rows={4} rowHeight={96} label="Runners" />
    </>
  );
}

function OffersBody({ after }: { after: string | null }) {
  const view = useRes(offersRes(after)).data;
  return (
    <>
      <section className="offers-section" aria-labelledby="offers-runners">
        <SectionHeading index="01" label="Runners" title={<span id="offers-runners">What each runner can take</span>} aside={<span className="offers-aside">{taking(view)}</span>} />
        {view.runners.length === 0 ? (
          <EmptyState title="No runner yet">
            <Link to="/runners/add">Add machines</Link> to see what they can take.
          </EmptyState>
        ) : (
          <ul className="offers-runners" aria-label="Runners">
            {view.runners.map((r) => (
              <RunnerItem key={r.name} r={r} />
            ))}
          </ul>
        )}
      </section>
      <section className="offers-section" aria-labelledby="offers-waiting">
        <SectionHeading index="02" label="Waiting" title={<span id="offers-waiting">What waits, and why</span>} aside={<span className="offers-aside">{waitingAside(view)}</span>} />
        {view.waiting.groups.length === 0 ? (
          <EmptyState title={after ? "Nothing more waits" : "Nothing waits"}>
            {after ? <Link to="/runners/offers">Back to the first page</Link> : "Every run and leg asked for has started."}
          </EmptyState>
        ) : (
          <>
            <ul className="offers-groups" aria-label="Waiting work">
              {view.waiting.groups.map((g) => (
                <GroupItem key={g.cursor} g={g} />
              ))}
            </ul>
            <nav className="offers-pages" aria-label="Pages of waiting work">
              {after && <Link to="/runners/offers">First page</Link>}
              {view.waiting.next && <Link to={`/runners/offers?after=${encodeURIComponent(view.waiting.next)}`}>Next {view.waiting.limit}</Link>}
            </nav>
          </>
        )}
      </section>
    </>
  );
}

/** How many take work, of how many. */
function taking(v: OffersView): string {
  const n = v.runners.filter((r) => r.connected && r.health.taking).length;
  return `${n} of ${v.runners.length} taking work`;
}

function waitingAside(v: OffersView): string {
  const w = v.waiting;
  if (w.runs === 0) return "none";
  return `${w.runs} ${w.runs === 1 ? "run" : "runs"} in ${w.groups_total} ${w.groups_total === 1 ? "group" : "groups"}`;
}

function RunnerItem({ r }: { r: OfferRunner }) {
  const name = r.label ?? r.name;
  return (
    <li className="offer-runner" aria-label={name} data-taking={r.health.taking || undefined}>
      <div className="offer-head">
        <StatusDot status={healthStatus(r)} title={healthSaid(r)} />
        <Link className="offer-name" to={`/runners?agent=${encodeURIComponent(r.name)}`}>
          {name}
        </Link>
        <span className="offer-platform">{r.platform}</span>
        <span className="offer-health">{healthSaid(r)}</span>
      </div>
      {r.said && (
        <dl className="offer-facts">
          <div>
            <dt>Budget</dt>
            <dd>{r.budget ? amountsSaid(r.budget) : "—"}</dd>
          </div>
          <div>
            <dt>Free</dt>
            <dd>{r.room ? amountsSaid(r.room) : "—"}</dd>
          </div>
          <div>
            <dt>Line</dt>
            <dd>{lineSaid(r)}</dd>
          </div>
          {r.heard !== undefined && (
            <div>
              <dt>As of</dt>
              <dd>
                <time dateTime={new Date(r.heard).toISOString()}>{clockTime(r.heard)}</time>
              </dd>
            </div>
          )}
        </dl>
      )}
      {r.answers.length > 0 && (
        <ul className="offer-answers" aria-label={`What ${name} would do`}>
          {r.answers.map((a) => (
            <AnswerItem key={`${a.ask.cpu_milli}-${a.ask.mem}-${a.ask.disk}-${a.ask.gpu}`} a={a} />
          ))}
        </ul>
      )}
    </li>
  );
}

function AnswerItem({ a }: { a: OfferAnswer }) {
  return (
    <li className="offer-answer" data-answer={a.answer}>
      <span className="offer-ask">{amountsSaid(a.ask)}</span>
      <span className="offer-said">
        {answerSaid(a)}
        {a.derived && <span className="offer-derived"> (from its room)</span>}
      </span>
    </li>
  );
}

function GroupItem({ g }: { g: OfferGroup }) {
  return (
    <li className="offer-group" aria-label={`${needSaid(g)}, ${amountsSaid(g.ask)}`}>
      <div className="offer-head">
        <span className="offer-count">{g.runs}</span>
        <span className="offer-name">{needSaid(g)}</span>
        <span className="offer-platform">{amountsSaid(g.ask)}</span>
      </div>
      <p className="offer-why">{groupSaid(g)}</p>
      <p className="offer-meta">
        {countsSaid(g.by_class)}
        {g.since !== undefined && (
          <>
            {" · the longest since "}
            <time dateTime={new Date(g.since).toISOString()}>{absoluteTime(Math.floor(g.since / 1000))}</time>
          </>
        )}
      </p>
    </li>
  );
}

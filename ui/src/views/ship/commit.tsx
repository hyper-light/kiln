/**
 * A commit's own page (Commit page plan 01a175b7f26e724d9159b4120a514fee):
 * /ship/{owner}/{name}/{sha}, reached from Commit on an opened row (step 1). Every commit has one,
 * read from its detail (GET /api/ship/commits/{owner}/{name}/{sha}, the same resource the opened row
 * reads); a commit kiln doesn't know says so. What failed comes first, then the six stage cards,
 * then one trace of every span on a time axis, failures first (step 2).
 */
import { shipApi } from "@/api/ship";
import { Loadable } from "@/components/ui/loading";
import { Crumbs } from "@/components/ui/misc";
import { JourneyStrip, StatusGlyph } from "@/components/ui/status-glyph";
import { GLYPHS, STAGE_WORD } from "@/components/ui/status-glyph-spec";
import { clockTime } from "@/lib/format";
import { useSuspenseResource } from "@/lib/resource";
import { Link, useParams } from "@/lib/router";
import { cardsOf } from "./jobs";
import { failuresOf, shipGroups, standWord, strip, traceOf } from "./model";
import { horizonOf, LaneBar, reviewGroup, type ShipLaneGroup } from "./review/lanes";
import "./ship.css";

export default function CommitPage() {
  const { owner = "", name = "", sha = "" } = useParams();
  const repo = `${owner}/${name}`;
  return (
    <div className="container-console page ship">
      <title>{`${sha.slice(0, 7)} · Ship · kiln`}</title>
      <Crumbs index={2} items={[{ label: "Ship", to: "/ship" }, { label: repo, to: `/ship?repo=${encodeURIComponent(repo)}` }, { label: sha.slice(0, 7) }]} />
      <Loadable label="Loading the commit">
        <Commit repo={repo} sha={sha} />
      </Loadable>
    </div>
  );
}

function Commit({ repo, sha }: { repo: string; sha: string }) {
  // Every stage it shows moves on its own event, as Ship's list follows them.
  const c = useSuspenseResource(`ship/commit/${repo}/${sha}`, (s) => shipApi.commit(repo, sha, s), {
    pollMs: 15_000,
    on: ["run", "job", "review", "comment", "conflict", "cd_apps", "cd_rollout", "updates", "update_job"],
  }).data;
  const failed = failuresOf(c);
  const groups = [reviewGroup(c), ...shipGroups(c)].filter((g): g is ShipLaneGroup => g != null && g.lanes.length > 0);
  const now = Math.floor(Date.now() / 1000);
  const window = horizonOf(groups, now);
  const spans = traceOf(groups);
  return (
    <section className="ship-commit" aria-label="Commit">
      <h1 className="ship-commit__title">{c.message || c.change?.title || c.short}</h1>
      <p className="ship-commit__meta">
        <span className="ship-stand" data-stand={c.stand}>
          {standWord(c.stand)}
        </span>
        <span className="ship-commit__line">{c.line}</span>
        <span>{c.author}</span>
        <span className="ship-sha">{c.short}</span>
      </p>
      <JourneyStrip pips={strip(c.pips)} />
      {/* What failed, before anything else: each failing job with its words and its log. */}
      {failed.length > 0 && (
        <section className="ship-sec ship-commit__errors" aria-label="Errors">
          <h2>Errors</h2>
          <ul>
            {failed.map((f) => (
              <li key={`${f.stage}/${f.name}`} className="ship-commit__error">
                <StatusGlyph state="failed" size={14} />
                <span className="ship-commit__error-name">{f.name}</span>
                <span className="ship-commit__error-stage">{STAGE_WORD[f.stage]}</span>
                {f.note ? <span className="ship-commit__error-note">{f.note}</span> : null}
                {f.href ? <Link to={f.href}>Its log</Link> : null}
              </li>
            ))}
          </ul>
        </section>
      )}
      <section className="ship-sec" aria-label="Stages">
        <h2>Stages</h2>
        <ol className="ship-commit__stages">
          {cardsOf(c).map((p) => (
            <li key={p.stage} className="ship-job" data-stage={p.stage} data-state={p.state}>
              <span className="ship-job__head">
                <StatusGlyph state={p.state} size={16} p={p.p} />
                {STAGE_WORD[p.stage]}
              </span>
              <span className="ship-job__state">{GLYPHS[p.state].word}</span>
            </li>
          ))}
        </ol>
      </section>
      <section className="ship-sec" aria-label="Trace">
        <h2>Trace</h2>
        {spans.length === 0 ? (
          <p>Nothing has run for it yet.</p>
        ) : (
          <div className="ship-lanesbox">
            <p className="ship-lanesbox__nav">
              <span className="ship-lanesbox__span">
                {clockTime(window[0] * 1000)} – {clockTime(window[1] * 1000)}
              </span>
            </p>
            <ul className="ship-lanes">
              {spans.map((l) => (
                <LaneBar key={l.id} lane={l} window={window} now={now} />
              ))}
            </ul>
          </div>
        )}
      </section>
    </section>
  );
}

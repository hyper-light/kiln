/**
 * A commit's own page (Commit page plan 01a175b7f26e724d9159b4120a514fee):
 * /ship/{owner}/{name}/{sha}, reached from Commit on an opened row (step 1). Every commit has one,
 * read from its detail (GET /api/ship/commits/{owner}/{name}/{sha}, the same resource the opened row
 * reads); a commit kiln doesn't know says so. What failed comes first, then the six stage cards,
 * then one trace of every span on a time axis, failures first (step 2); what ships with it, where
 * each environment stands from it with the one image they run, and links to its runs (step 3). Its
 * Build names each platform its release built, the image, and builds a newer push superseded, and for
 * kiln's own commits, from System's builds, which platforms' runners get its binary (Build and
 * replant plan 01a175b7f27570ee9f5b248d7bb40c0f, step 1).
 */
import * as React from "react";
import { shipApi } from "@/api/ship";
import { Loadable } from "@/components/ui/loading";
import { Crumbs } from "@/components/ui/misc";
import { JourneyStrip, StatusGlyph } from "@/components/ui/status-glyph";
import { GLYPHS, STAGE_WORD } from "@/components/ui/status-glyph-spec";
import { clockTime } from "@/lib/format";
import { updatesRes, useRes } from "@/lib/data";
import { useSuspenseResource } from "@/lib/resource";
import { Link, useParams } from "@/lib/router";
import { cardsOf } from "./jobs";
import { platformsOf } from "@/views/updates/words";
import { buildWord, environmentsOf, failuresOf, keptBuildsOf, ridesOf, shipGroups, standWord, strip, traceOf } from "./model";
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
  // What ships with it is how Ship's list folds it: the repository's commits, read as the list reads them.
  const listed = useSuspenseResource(`ship/commits/${JSON.stringify({ repo, limit: 200 })}`, (s) => shipApi.commits({ repo, limit: 200 }, s), { pollMs: 15_000, on: ["run", "review"] }).data;
  const rides = ridesOf(listed.commits, c.sha);
  const envs = environmentsOf(c);
  const image = c.build?.image.digest ?? null;
  const runsIt = envs.filter((e) => e.now || e.state === "passed").map((e) => e.environment);
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
      <section className="ship-sec" aria-label="Build">
        <h2>Build</h2>
        {c.build ? (
          <>
            <ul className="ship-commit__builds">
              {c.build.platforms.map((p) => (
                <li key={p.name} data-state={p.state}>
                  <StatusGlyph state={p.state} size={14} />
                  <span className="ship-commit__env">{p.name}</span>
                  <span>{buildWord(p.state)}</span>
                </li>
              ))}
            </ul>
            <p className="ship-commit__image">
              The image
              {c.build.image.digest ? (
                <>
                  {" "}
                  <code>{c.build.image.digest.slice(0, 19)}</code>
                </>
              ) : null}
              : {buildWord(c.build.image.state)}.
            </p>
          </>
        ) : (
          <p>No build for it yet.</p>
        )}
        <Quiet>
          <React.Suspense fallback={null}>
            <KeptBuilds repo={repo} sha={c.sha} />
          </React.Suspense>
        </Quiet>
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
      <section className="ship-sec" aria-label="Ships with">
        <h2>Ships with</h2>
        {rides.as === "alone" ? (
          <p>It ships on its own.</p>
        ) : (
          <>
            {rides.head && (
              <p>
                It rides with <Link to={`/ship/${rides.head.repo}/${rides.head.sha}`}>{rides.head.message || rides.head.short}</Link> (<span className="ship-sha">{rides.head.short}</span>).
              </p>
            )}
            {rides.with.length > 0 && (
              <ul className="ship-commit__rides">
                {rides.with.map((r) => (
                  <li key={r.sha}>
                    <Link to={`/ship/${r.repo}/${r.sha}`}>{r.message || r.short}</Link> <span className="ship-sha">{r.short}</span> · {standWord(r.stand)}
                  </li>
                ))}
              </ul>
            )}
          </>
        )}
      </section>
      <section className="ship-sec" aria-label="Environments">
        <h2>Environments</h2>
        {envs.length === 0 ? (
          <p>Not in an environment yet.</p>
        ) : (
          <ul className="ship-commit__envs">
            {envs.map((e) => (
              <li key={e.environment} data-production={e.production || undefined}>
                <StatusGlyph state={e.state as keyof typeof GLYPHS} size={14} />
                <span className="ship-commit__env">{e.environment}</span>
                <span>{e.now ? "runs it now" : (GLYPHS[e.state as keyof typeof GLYPHS]?.word ?? e.state)}</span>
                {e.description ? <span className="ship-commit__env-note">{e.description}</span> : null}
                {e.at ? <span className="ship-commit__env-note">{clockTime(e.at * 1000)}</span> : null}
              </li>
            ))}
          </ul>
        )}
        <p className="ship-commit__image">
          {image ? (
            <>
              Built once as <code>{image.slice(0, 19)}</code>
              {runsIt.length > 1 ? `: ${runsIt.join(" and ")} run the same image.` : runsIt.length === 1 ? `: ${runsIt[0]} runs it.` : "."}
            </>
          ) : (
            "No image built for it yet."
          )}
        </p>
      </section>
      <section className="ship-sec" aria-label="Runs">
        <h2>Runs</h2>
        {(c.runs ?? []).length === 0 ? (
          <p>No runs yet.</p>
        ) : (
          <ul className="ship-commit__runs">
            {(c.runs ?? []).map((r) => (
              <li key={r.id}>
                <Link to={`/runs/${r.id}`}>{r.pipeline}</Link> <span className="ship-sha">{r.id}</span>
              </li>
            ))}
          </ul>
        )}
      </section>
    </section>
  );
}

/** For a commit of kiln's own, from System's builds: which platforms' runners get its binary, and which a newer build replaced. */
function KeptBuilds({ repo, sha }: { repo: string; sha: string }) {
  const data = useRes(updatesRes).data;
  if (data.build?.repo !== repo) return null;
  const kept = keptBuildsOf(platformsOf(data, null), sha);
  return kept.length === 0 ? (
    <p className="ship-commit__kept">No binary runners get is built from it.</p>
  ) : (
    <ul className="ship-commit__kept" aria-label="Runners' binaries">
      {kept.map((k) => (
        <li key={k.target}>
          {k.label}: {k.state === "current" ? "runners get its binary" : "its binary is kept; a newer build serves runners"}
        </li>
      ))}
    </ul>
  );
}

/** What a viewer may not read (System's builds) leaves the page as it is, saying nothing. */
class Quiet extends React.Component<{ children: React.ReactNode }, { failed: boolean }> {
  state = { failed: false };
  static getDerivedStateFromError() {
    return { failed: true };
  }
  render() {
    return this.state.failed ? null : this.props.children;
  }
}

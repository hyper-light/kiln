/**
 * A commit's own page (Commit page plan 01a175b7f26e724d9159b4120a514fee, step 1):
 * /ship/{owner}/{name}/{sha}, reached from Commit on an opened row. Every commit has one, read from
 * its detail (GET /api/ship/commits/{owner}/{name}/{sha}, the same resource the opened row reads);
 * a commit kiln doesn't know says so.
 */
import { shipApi } from "@/api/ship";
import { Loadable } from "@/components/ui/loading";
import { Crumbs } from "@/components/ui/misc";
import { JourneyStrip } from "@/components/ui/status-glyph";
import { useSuspenseResource } from "@/lib/resource";
import { useParams } from "@/lib/router";
import { standWord, strip } from "./model";
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
    </section>
  );
}

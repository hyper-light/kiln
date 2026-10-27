/**
 * Ship (docs/ci.md W26-R01, R02; the CI/CD Integration agent's page, after its f-ship prototype):
 * every commit of a repository on its way from push to production, changes among them. A change is
 * its commits: its tip's row carries its review, the rest fold under it. Each row is Ship's own:
 * the commit, its stand and the one line saying what it's doing, its journey strip, who and when,
 * its paired actions, and the dropdown (Files with the Conversation, Output, Jobs, Lanes). The
 * facets narrow by host and kind; Review is the lens on changes. Data through Suspense, changes
 * through the endpoints the row names; no effects.
 */
import * as React from "react";
import { ChevronDown } from "lucide-react";
import { shipApi, type ShipAction, type ShipCommit, type ShipQuery } from "@/api/ship";
import { Button } from "@/components/ui/button";
import { FacetBox, type Chosen } from "@/components/ui/facet-box";
import { Loadable } from "@/components/ui/loading";
import { EmptyState, PageIntro } from "@/components/ui/misc";
import { JourneyStrip } from "@/components/ui/status-glyph";
import { Segmented } from "@/components/ui/tabs";
import { toast } from "@/components/ui/toast";
import { errorMessage } from "@/lib/hooks";
import { Link, useNavigate, useSearchParams } from "@/lib/router";
import { invalidate, useSuspenseResource } from "@/lib/resource";
import { RelTime } from "../shared";
import { ACTION_WORD, fold, HOSTS, KINDS, order, standWord, strip, type Item, type Sort } from "./model";
import "./ship.css";
import { ShipViews } from "./views";

const LIMIT = 100;

export default function ShipPage() {
  return (
    <div className="container-console page ship">
      <title>Ship · kiln</title>
      <PageIntro index={2} label="Ship" title="Ship." lead="Every commit and change on its way from review to production, and what it needs of you." />
      <ShipViews />
      <Loadable label="Loading commits">
        <ShipList />
      </Loadable>
    </div>
  );
}

function ShipList() {
  const [params, setParams] = useSearchParams();
  const lens = params.get("lens") === "review" ? "review" : "all";
  const sort: Sort = params.get("sort") === "newest" ? "newest" : "needs";
  const chosen: Chosen = Object.fromEntries((["host", "kind"] as const).map((k): [string, string[]] => [k, (params.get(k) ?? "").split(",").filter(Boolean)]).filter(([, v]) => v.length > 0));
  // One value of a facet is the server's to narrow by; several are narrowed here.
  const one = (k: string) => (chosen[k]?.length === 1 ? chosen[k][0] : undefined);
  const q: ShipQuery = { repo: params.get("repo") ?? undefined, host: one("host"), kind: one("kind"), lens: lens === "review" ? "review" : undefined, limit: LIMIT };
  const key = `ship/commits/${JSON.stringify(q)}`;
  // Every stage it shows moves on its own event: a run or a leg (checks, build), review and comments,
  // an app's sync and a rollout (staging, production), a runner's update (replant).
  const page = useSuspenseResource(key, (s) => shipApi.commits(q, s), {
    pollMs: 15_000,
    on: ["run", "job", "review", "comment", "cd_apps", "cd_rollout", "updates", "update_job"],
  });
  const commits = page.data.commits.filter((c) => (chosen.host ?? [c.host ?? ""]).includes(c.host ?? "") && (chosen.kind ?? [c.kind ?? "commit"]).includes(c.kind ?? "commit"));
  const items = order(fold(commits), sort);

  const set = (k: string, v: string | null) =>
    setParams(
      (prev) => {
        const next = new URLSearchParams(prev);
        if (v) next.set(k, v);
        else next.delete(k);
        return next;
      },
      { replace: true },
    );

  return (
    <>
      <div className="ship-tools">
        <FacetBox
          className="ship-facet"
          label="Filter commits"
          placeholder="Host, kind…"
          facets={[
            { id: "host", label: "Host", options: HOSTS },
            { id: "kind", label: "Kind", options: KINDS },
          ]}
          chosen={chosen}
          onChosen={(next) =>
            setParams(
              (prev) => {
                const p = new URLSearchParams(prev);
                for (const k of ["host", "kind"]) {
                  const v = next[k] ?? [];
                  if (v.length) p.set(k, v.join(","));
                  else p.delete(k);
                }
                return p;
              },
              { replace: true },
            )
          }
        />
        <Segmented
          label="Lens"
          value={lens}
          onChange={(v) => set("lens", v === "review" ? "review" : null)}
          options={[
            { value: "all", label: "All" },
            { value: "review", label: "Review" },
          ]}
        />
        <Segmented
          label="Order"
          value={sort}
          onChange={(v) => set("sort", v === "newest" ? "newest" : null)}
          options={[
            { value: "needs", label: "Needs you first" },
            { value: "newest", label: "Newest" },
          ]}
        />
      </div>
      {items.length === 0 ? (
        <EmptyState title={lens === "review" ? "No changes under review" : "No commits yet"}>Commits show here from their first push: their runs, their review, their way to production.</EmptyState>
      ) : (
        <ul className="ship-list" aria-label="Commits">
          {items.map((it) => (
            <Row key={`${it.head.repo}/${it.head.sha}`} item={it} reload={page.reload} />
          ))}
        </ul>
      )}
    </>
  );
}

function Row({ item, reload }: { item: Item; reload: () => void }) {
  const c = item.head;
  const [open, setOpen] = React.useState(false);
  const [riders, setRiders] = React.useState(false);
  return (
    <li className="ship-row" data-stand={c.stand}>
      <div className="ship-r">
        <button type="button" className="ship-main" aria-expanded={open} onClick={() => setOpen(!open)}>
          <span className="ship-name">{c.message || c.change?.title || c.short}</span>
          <span className="ship-line">
            <span className="ship-stand" data-stand={c.stand}>
              {standWord(c.stand)}
            </span>
            <span className="ship-text">{c.line}</span>
          </span>
        </button>
        <span className="ship-pips">
          <JourneyStrip pips={strip(c.pips)} labels={false} />
        </span>
        <span className="ship-metas">
          <span>
            {c.author} · <RelTime ts={c.at} />
          </span>
          <span className="ship-m2">
            {c.change && <span className="ship-change">{c.change.name}</span>}
            {c.tags?.map((t) => (
              <span key={t} className="ship-tag">
                {t}
              </span>
            ))}
            <span className="ship-sha">{c.short}</span>
          </span>
        </span>
        <Actions commit={c} reload={reload} onReview={() => setOpen(true)} />
        <button type="button" className="ship-go" aria-expanded={open} aria-label={`${open ? "Close" : "Open"} ${c.short}`} onClick={() => setOpen(!open)}>
          <ChevronDown size={16} />
        </button>
      </div>
      {item.riders.length > 0 && (
        <button type="button" className="ship-fold" aria-expanded={riders} onClick={() => setRiders(!riders)}>
          {riders ? "Hide" : "Show"} {item.riders.length} more commit{item.riders.length === 1 ? "" : "s"} of {c.change?.name ?? "this change"}
        </button>
      )}
      {riders && (
        <ul className="ship-riders" aria-label={`The other commits of ${c.change?.name ?? c.short}`}>
          {item.riders.map((r) => (
            <li key={r.sha} className="ship-rider" data-stand={r.stand}>
              <span className="ship-name">{r.message || r.short}</span>
              <span className="ship-text">
                {standWord(r.stand)} · {r.line}
              </span>
              <span className="ship-sha">{r.short}</span>
            </li>
          ))}
        </ul>
      )}
      {open && <Dropdown commit={c} />}
    </li>
  );
}

/** The row's paired actions, each through the endpoint it names; Review opens the item at its files. */
function Actions({ commit, reload, onReview }: { commit: ShipCommit; reload: () => void; onReview: () => void }) {
  const [busy, setBusy] = React.useState<string | null>(null);
  const navigate = useNavigate();
  const act = async (a: ShipAction) => {
    if (a.name === "review" && commit.change) {
      onReview();
      return;
    }
    setBusy(a.name);
    try {
      await shipApi.act(a);
      toast({ title: `${ACTION_WORD[a.name] ?? a.name}: ${commit.change?.name ?? commit.short}`, tone: "succeeded" });
      invalidate("ship/");
      reload();
    } catch (e) {
      toast({ title: `${ACTION_WORD[a.name] ?? a.name} didn't go`, description: errorMessage(e), tone: "failed" });
    } finally {
      setBusy(null);
    }
  };
  const acts = (commit.actions ?? []).slice(0, 2);
  if (acts.length === 0) return <span className="ship-acts" />;
  return (
    <span className="ship-acts">
      {acts.map((a, i) => (
        <Button
          key={a.name}
          size="xs"
          variant={i === 0 ? "primary" : "secondary"}
          disabled={busy !== null}
          onClick={() => (a.method === "GET" && !commit.change ? navigate(a.path.replace(/^\/api/, "")) : void act(a))}
        >
          {ACTION_WORD[a.name] ?? a.name}
        </Button>
      ))}
    </span>
  );
}

/** An item's dropdown, in its order: Files (with the Conversation), Output, Jobs, Lanes. */
function Dropdown({ commit }: { commit: ShipCommit }) {
  const ch = commit.change;
  return (
    <div className="ship-open">
      <section aria-label="Files" className="ship-sec">
        <h3>Files</h3>
        {ch ? (
          <p>
            {ch.name} · revision {ch.revision} of {ch.latest} · {ch.title}
            {ch.open && ch.open.question + ch.open.nit + ch.open.issue + ch.open.blocking > 0 && (
              <span className="ship-kinds">
                {" "}
                · {ch.open.blocking} blocking, {ch.open.issue} issues, {ch.open.nit} nits, {ch.open.question} questions
              </span>
            )}{" "}
            {ch.host === "kiln" && <Link to={`/review/${ch.name}`}>Open its files and conversation</Link>}
          </p>
        ) : (
          <p>
            Commit <span className="ship-sha">{commit.short}</span> on {commit.branch || "its branch"}.
          </p>
        )}
      </section>
      <section aria-label="Output" className="ship-sec">
        <h3>Output</h3>
        {commit.runs?.length ? <p>Its runs' output: {commit.runs.map((r) => r.pipeline).join(", ")}.</p> : <p>No runs yet.</p>}
      </section>
      <section aria-label="Jobs" className="ship-sec">
        <h3>Jobs</h3>
        <ul>
          {(commit.runs ?? []).map((r) => (
            <li key={r.id}>
              <Link to={`/runs/${r.id}`}>{r.pipeline}</Link>
            </li>
          ))}
        </ul>
      </section>
      <section aria-label="Lanes" className="ship-sec">
        <h3>Lanes</h3>
        {commit.environments?.length ? <p>Runs in {commit.environments.map((e) => e.environment).join(", ")}.</p> : <p>Not in an environment yet.</p>}
      </section>
    </div>
  );
}

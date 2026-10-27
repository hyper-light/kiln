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
import { ACTION_WORD, FACET_KEYS, fold, keyOf, HOSTS, KINDS, optionsOf, order, projectOf, SHIP_STANDS, SORTS, sortOf, standCounts, standWord, strip, type Item } from "./model";
import "./ship.css";
import { ShipViews } from "./views";
import { Stands } from "@/views/harness/stands";
import { SelectField } from "@/components/ui/misc";
import { useGlide, useListHold } from "@/views/harness/list-tools";
import { firstStage, JobCards } from "./jobs";
import { ShipOutput } from "./output";

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
  const sort = sortOf(params.get("sort"));
  const chosen: Chosen = Object.fromEntries(FACET_KEYS.map((k): [string, string[]] => [k, (params.get(k) ?? "").split(",").filter(Boolean)]).filter(([, v]) => v.length > 0));
  // One value of a facet is the server's to narrow by; several are narrowed here.
  const one = (k: string) => (chosen[k]?.length === 1 ? chosen[k][0] : undefined);
  const q: ShipQuery = { repo: one("repo"), host: one("host"), kind: one("kind"), lens: lens === "review" ? "review" : undefined, limit: LIMIT };
  const key = `ship/commits/${JSON.stringify(q)}`;
  // Every stage it shows moves on its own event: a run or a leg (checks, build), review and comments,
  // an app's sync and a rollout (staging, production), a runner's update (replant).
  const page = useSuspenseResource(key, (s) => shipApi.commits(q, s), {
    pollMs: 15_000,
    on: ["run", "job", "review", "comment", "cd_apps", "cd_rollout", "updates", "update_job"],
  });
  const all = page.data.commits;
  // What each facet reads of a commit; every facet chosen must hold.
  const value: Record<string, (c: (typeof all)[number]) => string> = {
    repo: (c) => c.repo,
    project: (c) => projectOf(c.repo),
    author: (c) => c.author,
    stand: (c) => String(c.stand),
    host: (c) => c.host ?? "",
    kind: (c) => c.kind ?? "commit",
  };
  const commits = all.filter((c) => Object.entries(chosen).every(([k, vs]) => vs.length === 0 || vs.includes(value[k]?.(c) ?? "")));
  const items = order(fold(commits), sort);
  // Holding still (step 6): while the list is in use or a row is open, the rows keep their places,
  // each with its latest data, and what arrives waits for the release; then the rows glide.
  const [opened, setOpened] = React.useState<ReadonlySet<string>>(() => new Set());
  const toggle = (k: string, o: boolean) =>
    setOpened((was) => {
      const next = new Set(was);
      if (o) next.add(k);
      else next.delete(k);
      return next;
    });
  const hold = useListHold(opened.size > 0);
  const { ref: holdRef, ...holdProps } = hold.bind;
  // Your own change of view (lens, sort, filter) applies at once; only what arrives waits.
  const view = `${lens}|${sort}|${JSON.stringify(chosen)}`;
  const [kept] = React.useState(() => ({ order: [] as string[], last: new Map<string, Item>(), view }));
  for (const it of items) kept.last.set(keyOf(it), it);
  if (!hold.held || kept.order.length === 0 || kept.view !== view) {
    kept.order = items.map(keyOf);
    kept.view = view;
  }
  const shown = hold.held ? kept.order.flatMap((k) => kept.last.get(k) ?? []) : items;
  if (!hold.held) kept.last = new Map(items.map((it) => [keyOf(it), it]));
  const glide = useGlide(shown.map(keyOf).join("\n"));

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
      <Stands counts={standCounts(commits)} every={SHIP_STANDS} />
      <div className="ship-tools">
        <FacetBox
          className="ship-facet"
          label="Filter commits"
          placeholder="Repository, project, author, stand…"
          facets={[
            { id: "repo", label: "Repository", options: optionsOf(all.map((c) => c.repo)) },
            { id: "project", label: "Project", options: optionsOf(all.map((c) => projectOf(c.repo))) },
            { id: "author", label: "Author", options: optionsOf(all.map((c) => c.author)) },
            {
              id: "stand",
              label: "Stand",
              options: optionsOf(
                all.map((c) => String(c.stand)),
                standWord,
              ),
            },
            { id: "host", label: "Host", options: HOSTS },
            { id: "kind", label: "Kind", options: KINDS },
          ]}
          chosen={chosen}
          onChosen={(next) =>
            setParams(
              (prev) => {
                const p = new URLSearchParams(prev);
                for (const k of FACET_KEYS) {
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
        <SelectField className="ship-sort" label="Sort commits" hideLabel value={sort} onChange={(v) => set("sort", v === "needs" ? null : v)} options={SORTS} />
      </div>
      {items.length === 0 ? (
        <EmptyState title={lens === "review" ? "No changes under review" : "No commits yet"}>Commits show here from their first push: their runs, their review, their way to production.</EmptyState>
      ) : (
        <ul
          className="ship-list"
          aria-label="Commits"
          {...holdProps}
          ref={(el) => {
            glide(el);
            holdRef(el);
          }}
        >
          {shown.map((it) => (
            <Row key={keyOf(it)} item={it} reload={page.reload} open={opened.has(keyOf(it))} onOpen={(o) => toggle(keyOf(it), o)} />
          ))}
        </ul>
      )}
    </>
  );
}

function Row({ item, reload, open, onOpen }: { item: Item; reload: () => void; open: boolean; onOpen: (open: boolean) => void }) {
  const c = item.head;
  const [riders, setRiders] = React.useState(false);
  // One environment flag: production when it runs there, else the first it runs in.
  const env = c.environments?.find((e) => e.production) ?? c.environments?.[0];
  const n = item.riders.length;
  const what =
    item.folds === "rides"
      ? `${n} commit${n === 1 ? "" : "s"} riding with it`
      : item.folds === "shipped"
        ? `${n} more shipped`
        : `${n} more commit${n === 1 ? "" : "s"} of ${c.change?.name ?? "this change"}`;
  return (
    <li className="ship-row" data-stand={c.stand} data-glide={keyOf(item)}>
      <div className="ship-r">
        <button type="button" className="ship-main" aria-expanded={open} onClick={() => onOpen(!open)}>
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
            {env && (
              <span className="ship-env" data-prod={env.production || undefined}>
                {env.environment}
              </span>
            )}
            <span className="ship-sha">{c.short}</span>
          </span>
        </span>
        <Actions commit={c} reload={reload} onReview={() => onOpen(true)} />
        <button type="button" className="ship-go" aria-expanded={open} aria-label={`${open ? "Close" : "Open"} ${c.short}`} onClick={() => onOpen(!open)}>
          <ChevronDown size={16} />
        </button>
      </div>
      {item.riders.length > 0 && (
        <button type="button" className="ship-fold" aria-expanded={riders} onClick={() => setRiders(!riders)}>
          {riders ? "Hide" : "Show"} {what}
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
  // Always a pair of one width: a slot no action fills keeps its place, unseen.
  const slot = (k: number) => (
    <span key={`slot-${k}`} className="ship-act-slot" aria-hidden="true">
      <Button size="xs" variant="secondary" tabIndex={-1} disabled>
        ·
      </Button>
    </span>
  );
  if (acts.length === 0) return <span className="ship-acts">{[slot(0), slot(1)]}</span>;
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
      {acts.length === 1 && slot(1)}
    </span>
  );
}

/** An item's dropdown, in its order: Files (with the Conversation), Output, Jobs, Lanes. */
function Dropdown({ commit }: { commit: ShipCommit }) {
  // The picked job card: yours once you click one, else the cards' rule; Output shows its job.
  const [stage, setStage] = React.useState<string | null>(null);
  const shown = stage ?? firstStage(commit);
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
        <Loadable label="Loading the output" className="ship-output-slot">
          <ShipOutput commit={commit} stage={shown} />
        </Loadable>
      </section>
      <section aria-label="Jobs" className="ship-sec">
        <h3>Jobs</h3>
        <JobCards commit={commit} onPick={setStage} />
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

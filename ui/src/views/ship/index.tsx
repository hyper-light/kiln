/**
 * Ship (docs/ci.md W26-R01, R02; the CI/CD Integration agent's page, after its f-ship prototype):
 * every commit of a repository on its way from push to production, changes among them. A change is
 * its commits: its tip's row carries its review, the rest fold under it. Each row is Ship's own:
 * the commit, its stand and the one line saying what it's doing, its journey strip, who and when,
 * its paired actions, and the dropdown (Files with the Conversation, Output, Jobs, Lanes). The
 * facets narrow by host and kind, and a change's kind is how to see only changes: no toggle of its
 * own (Ada, 2026-10-28). Data through Suspense, changes through the endpoints the row names; no
 * effects.
 */
import * as React from "react";
import { ChevronDown } from "lucide-react";
import { shipApi, type ShipAction, type ShipCommit, type ShipPace, type ShipQuery } from "@/api/ship";
import { Button } from "@/components/ui/button";
import { FacetBox, type Chosen } from "@/components/ui/facet-box";
import { Loadable, Loading } from "@/components/ui/loading";
import { EmptyState, PageIntro } from "@/components/ui/misc";
import { JourneyStrip, StatusGlyph } from "@/components/ui/status-glyph";
import { toast } from "@/components/ui/toast";
import { errorMessage } from "@/lib/hooks";
import { Link, useNavigate, useSearchParams } from "@/lib/router";
import { invalidate, useSuspenseResource } from "@/lib/resource";
import { RelTime } from "../shared";
import { ACTION_WORD, FACET_KEYS, fold, keyOf, HOSTS, KINDS, optionsOf, order, projectOf, SHIP_STANDS, SORTS, sortOf, standCounts, standWord, strip, type Item } from "./model";
import "./ship.css";
import { ShipViews } from "./views";
import { Stands } from "@/views/harness/stands";
import { Shimmer } from "@/views/harness/signals";
import { whoSaid } from "@/api/review";
import { SelectField } from "@/components/ui/misc";
import { useGlide, useListHold } from "@/views/harness/list-tools";
import { JobCards } from "./jobs";
import { ShipOutput } from "./output";
import { ReviewFiles, ShipLanes, warmReview } from "./review";
import { SectionHead } from "./head";
import { counted } from "./review/perf";

const LIMIT = 100;

/** The stands under way, whose word shimmers as Mission control's working agents' do (f-ship's). */
const UNDER_WAY = new Set(["landing", "checking", "building", "shipping", "replanting"]);

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
  const sort = sortOf(params.get("sort"));
  const chosen: Chosen = Object.fromEntries(FACET_KEYS.map((k): [string, string[]] => [k, (params.get(k) ?? "").split(",").filter(Boolean)]).filter(([, v]) => v.length > 0));
  // One value of a facet is the server's to narrow by; several are narrowed here.
  const one = (k: string) => (chosen[k]?.length === 1 ? chosen[k][0] : undefined);
  const q: ShipQuery = { repo: one("repo"), host: one("host"), kind: one("kind"), limit: LIMIT };
  const key = `ship/commits/${JSON.stringify(q)}`;
  // Every stage it shows moves on its own event: a run or a leg (checks, build), review, comments and
  // conflicts, an app's sync and a rollout (staging, production), a runner's update (replant).
  const page = useSuspenseResource(key, (s) => shipApi.commits(q, s), {
    pollMs: 15_000,
    on: ["run", "job", "review", "comment", "conflict", "cd_apps", "cd_rollout", "updates", "update_job"],
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
  // An open row never folds under another, so it keeps taking its latest data.
  const items = order(fold(commits, opened), sort);
  const hold = useListHold(opened.size > 0);
  const { ref: holdRef, ...holdProps } = hold.bind;
  // Your own change of view (sort, filter) applies at once; only what arrives waits.
  const view = `${sort}|${JSON.stringify(chosen)}`;
  const [kept] = React.useState(() => ({ order: [] as string[], last: new Map<string, { it: Item; sig: string }>(), view, pace: undefined as ShipPace | undefined }));
  // What a refetch brings that reads the same isn't a change (W26-R15): the item keeps its last
  // reference, so its memoized row doesn't render again; a changed item keeps its key and is
  // patched in place, never remounted.
  const stable = items.map((it) => {
    const k = keyOf(it);
    const sig = JSON.stringify(it);
    const was = kept.last.get(k);
    const same = was && was.sig === sig ? was.it : it;
    kept.last.set(k, { it: same, sig });
    return same;
  });
  if (!hold.held || kept.order.length === 0 || kept.view !== view) {
    kept.order = items.map(keyOf);
    kept.view = view;
  }
  const shown = hold.held ? kept.order.flatMap((k) => kept.last.get(k)?.it ?? []) : stable;
  if (!hold.held) kept.last = new Map(stable.map((it) => [keyOf(it), { it, sig: JSON.stringify(it) }]));
  // The pace line holds with the rows: what it counts waits for the release too, so nothing beside
  // the row you're reading moves (plan 01a1856fe81375b1bd1d1d86f270a45b, step 5).
  if (!hold.held || kept.pace === undefined) kept.pace = page.data.pace;
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
      <Stands counts={standCounts(shown.map((it) => it.head))} every={SHIP_STANDS} />
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
        <SelectField className="ship-sort" label="Sort commits" hideLabel value={sort} onChange={(v) => set("sort", v === "needs" ? null : v)} options={SORTS} />
      </div>
      <Pace pace={kept.pace} />
      {items.length === 0 ? (
        <EmptyState title="No commits yet">Commits show here from their first push: their runs, their review, their way to production.</EmptyState>
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

/**
 * The pace line under the list bar, as f-ship draws it (plan 01a1856fe81375b1bd1d1d86f270a45b, step
 * 5): what's in flight and the agents behind it, what each environment runs and how far production
 * is behind, and the runners on production's commit. One line, scrolling sideways when it doesn't
 * fit; it keeps its height when the server sends none.
 */
function Pace({ pace }: { pace?: ShipPace }) {
  const repos = new Set(pace?.envs.map((e) => e.repo) ?? []);
  return (
    <p className="ship-pace" aria-label="Pace">
      {pace ? (
        <>
          <span>
            <b>{pace.in_flight}</b> in flight · <b>{pace.agents}</b> agent{pace.agents === 1 ? "" : "s"}
          </span>
          {pace.envs.map((e) => (
            <span key={`${e.repo} ${e.environment}`}>
              {repos.size > 1 ? `${e.repo.split("/").pop() ?? e.repo} ` : ""}
              {e.environment} <span className="ship-pace__sha">{e.short}</span>
              {e.behind !== null && (
                <>
                  {" "}
                  · <b>{e.behind}</b> behind
                </>
              )}
            </span>
          ))}
          {pace.runners && (
            <span>
              runners <b>{pace.runners.on}</b>/{pace.runners.of}
            </span>
          )}
        </>
      ) : (
        "\u00a0"
      )}
    </p>
  );
}

const Row = React.memo(
  function Row({ item, reload, open, onOpen }: { item: Item; reload: () => void; open: boolean; onOpen: (open: boolean) => void }) {
    counted("ship/row");
    const c = item.head;
    const [riders, setRiders] = React.useState(false);
    // Each press of the Diff (Review) button, counted, so a press with the row open asks again.
    const [reveal, setReveal] = React.useState(0);
    // One environment flag: production when it runs there, else the first it runs in.
    const env = c.environments?.find((e) => e.production) ?? c.environments?.[0];
    const n = item.riders.length;
    // Riders say themselves as f-ship does (below); the rest of a fold says what it holds.
    const what = item.folds === "shipped" ? `${n} more shipped` : `${n} more commit${n === 1 ? "" : "s"} of ${c.change?.name ?? "this change"}`;
    return (
      // Its review starts loading as the pointer comes onto the row or the focus into it (W26-R15).
      <li className="ship-row" data-stand={c.stand} data-glide={keyOf(item)} onPointerEnter={() => warmReview(c)} onFocus={() => warmReview(c)}>
        <div className="ship-r">
          <button type="button" className="ship-main" aria-expanded={open} onClick={() => onOpen(!open)}>
            <span className="ship-name">{c.message || c.change?.title || c.short}</span>
            <span className="ship-line">
              <span className="ship-stand" data-stand={c.stand}>
                <Shimmer on={UNDER_WAY.has(c.stand)} flow={keyOf(item)}>
                  {standWord(c.stand)}
                </Shimmer>
              </span>
              <span className="ship-text">{c.line}</span>
            </span>
          </button>
          <span className="ship-pips">
            <JourneyStrip pips={strip(c.pips)} />
          </span>
          <span className="ship-metas">
            <span>
              <span className="ship-who">
                <StatusGlyph state={c.author.startsWith("agent:") ? "agent" : "person"} size={12} />
                <span>{whoSaid(c.author)}</span>
              </span>{" "}
              ·{" "}
              <span className="ship-ago">
                <RelTime ts={c.at} />
              </span>
            </span>
            <span className="ship-m2">
              {c.change && <span className="ship-change">{c.change.name}</span>}
              {c.tags?.map((t) => (
                <span key={t} className="ship-tag">
                  {t}
                </span>
              ))}
              {env && (
                <span className="ship-env" data-env={env.production ? "production" : "staging"}>
                  <StatusGlyph state="flag" size={12} />
                  {env.environment}
                </span>
              )}
              <span className="ship-sha">{c.short}</span>
            </span>
          </span>
          <Actions
            commit={c}
            reload={reload}
            onReview={() => {
              onOpen(true);
              setReveal((n) => n + 1);
            }}
          />
          <button type="button" className="ship-go" aria-expanded={open} aria-label={`${open ? "Close" : "Open"} ${c.short}`} onClick={() => onOpen(!open)}>
            <ChevronDown size={16} />
          </button>
        </div>
        {item.riders.length > 0 && (
          <button type="button" className="ship-fold" aria-expanded={riders} onClick={() => setRiders(!riders)}>
            {item.folds === "rides" ? (
              // f-ship's riders: how many ride with the approval, and the first of them.
              <>
                <StatusGlyph state="queued" size={14} />
                <span>
                  +{n} riding ·{" "}
                  <span className="ship-fold__sha">
                    {item.riders
                      .slice(0, 3)
                      .map((r) => r.short)
                      .join(" ")}
                  </span>
                  {n > 3 ? " …" : ""}
                </span>
              </>
            ) : (
              <span>
                {riders ? "Hide" : "Show"} {what}
              </span>
            )}
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
        {open && <Dropdown commit={c} reveal={reveal} />}
      </li>
    );
  },
  // A row renders only when its item does (a stable reference while it reads the same), its open
  // state flips, or its own state does (the Diff button's count); reload and onOpen are made each
  // render but always do the same thing (W26-R15, AC06).
  (a, b) => a.item === b.item && a.open === b.open,
);

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

/**
 * Brings the dropdown's Files into view and focuses it (the Diff button, Changes plan
 * 01a17bd748ff73d98f09996ecdc6873d step 4). The item's diff lives in one place, Review's Files
 * (Ada, 2026-10-27). While its review loads, Files is a placeholder that the loaded section then
 * replaces: it's scrolled to once, and each Files that comes takes the focus, until the loaded one
 * has it (Review marks it with its summary, `.rv-sum`), the reader moves the focus elsewhere, or
 * 15 s pass.
 */
function showFiles(open: HTMLElement): (() => void) | undefined {
  let shown: HTMLElement | null = null;
  /** Whether there's nothing more to do. */
  const show = () => {
    const now = document.activeElement;
    // The reader has moved on (the focus is somewhere, and not in what was shown): leave it there.
    if (shown && now && now !== document.body && !shown.contains(now)) return true;
    const files = open.querySelector<HTMLElement>('section[aria-label="Files"]');
    if (!files) return false;
    if (files !== shown) {
      // Review's Files is focusable (tabIndex -1) since e538638e56; one that isn't is made so.
      if (!files.hasAttribute("tabindex")) files.tabIndex = -1;
      if (!shown) files.scrollIntoView({ block: "start", behavior: document.documentElement.dataset.motion === "paused" ? "auto" : "smooth" });
      files.focus({ preventScroll: true });
      shown = files;
    }
    return files.querySelector(".rv-sum") !== null;
  };
  if (show()) return undefined;
  const watch = new MutationObserver(() => {
    if (show()) stop();
  });
  const timer = window.setTimeout(() => stop(), 15_000);
  const stop = () => {
    watch.disconnect();
    window.clearTimeout(timer);
  };
  watch.observe(open, { childList: true, subtree: true });
  return stop;
}

/** An item's dropdown, in its order: Files (with the Conversation), Output, Jobs, Lanes. */
function Dropdown({ commit, reveal }: { commit: ShipCommit; reveal: number }) {
  // The picked job (its stage and lane): yours once you click one, else the cards' rule; Output shows it.
  const [picked, setPicked] = React.useState<string | null>(null);
  // A Diff press (reveal counts them) brings Files into view; opening the row any other way doesn't.
  const files = React.useCallback((el: HTMLDivElement | null) => (el && reveal > 0 ? showFiles(el) : undefined), [reveal]);
  return (
    <div className="ship-open" ref={files}>
      {/* Dropdown boundary agreed with the Ship agent 01a1748c, 2026-10-26 20:28 PDT (NOTES.md):
          Files (with the Conversation) first, owned by Review. One loader for them all: every
          section waits for Files' review, so none is drawn first and then pushed down (2026-10-28). */}
      <Loadable label="Loading its review">
        <ReviewFiles item={commit} />
        <section aria-label="Output" className="ship-sec">
          <SectionHead label="Output" />
          <Loadable label="Loading the output" className="ship-output-slot">
            <ShipOutput commit={commit} picked={picked} />
          </Loadable>
        </section>
        <section aria-label="Jobs" className="ship-sec">
          <JobCards commit={commit} picked={picked} onPick={setPicked} />
          <ul>
            {(commit.runs ?? []).map((r) => (
              <li key={r.id}>
                <Link to={`/runs/${r.id}`}>{r.pipeline}</Link>
              </li>
            ))}
          </ul>
        </section>
        <section aria-label="Lanes" className="ship-sec">
          <Loadable
            label="Loading the lanes"
            fallback={
              <>
                <SectionHead label="Lanes" />
                <Loading label="Loading the lanes" block className="ship-lanes-slot" />
              </>
            }
          >
            <ShipLanes commit={commit} />
          </Loadable>
          {commit.environments?.length ? <p>Runs in {commit.environments.map((e) => e.environment).join(", ")}.</p> : <p>Not in an environment yet.</p>}
        </section>
        <p className="ship-open__page">
          <Link to={`/ship/${commit.repo}/${commit.sha}`}>Commit {commit.short}</Link>, its stages and trace on a page of its own
        </p>
      </Loadable>
    </div>
  );
}

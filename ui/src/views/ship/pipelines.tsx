/**
 * Ship's Pipelines (docs/ci.md W29-R01 to R08; Ada's approved stills of 2026-10-31, pl-1 to pl-9):
 * every workflow, reusable workflow and action of the repositories you may read, as Ship's rows by
 * repository (the stand's word and edge, its line, the Checks · Runs on · Latest pips; DRAFT beside a
 * draft's title), each repository's heading saying what's there and what needs someone, and a New
 * row last. An opened row is the jobs pane's anatomy, as Deploys draws it. Flat throughout; every
 * height held, so nothing moves as you pick, act or the pipelines go on. Data through Suspense; no
 * effects.
 */
import * as React from "react";
import { ChevronDown, FileText, Plus, Tag } from "lucide-react";
import { shipApi, type ShipPipelineRepo, type ShipPipelineRow } from "@/api/ship";
import { FacetBox, type Chosen } from "@/components/ui/facet-box";
import { Loadable, Loading } from "@/components/ui/loading";
import { EmptyState, PageIntro } from "@/components/ui/misc";
import { JourneyStrip, StatusGlyph } from "@/components/ui/status-glyph";
import { GLYPHS, PIPELINE_STAGES, type GlyphState, type Pip } from "@/components/ui/status-glyph-spec";
import { useSearchParams } from "@/lib/router";
import { useSuspenseResource } from "@/lib/resource";
import { relativeTime } from "@/lib/format";
import { optionsOf } from "./model";
import "./ship.css";
import { ShipViews } from "./views";

/** The event after which Pipelines asks again (W29-R03): a repository's rows changed. */
const PIPELINE_EVENTS = ["pipelines"] as const;
/** The facets rows are found by, beside the words typed (W29-R02). */
const FACETS = ["repo", "kind", "dialect", "stand", "shared"] as const;
type Facet = (typeof FACETS)[number];
const FACET_LABEL: Record<Facet, string> = { repo: "Repository", kind: "Kind", dialect: "Dialect", stand: "Stand", shared: "Shared" };
/** A kind as the facet and the pane say it. */
const KIND_WORD: Record<string, string> = {
  workflow: "Workflow",
  reusable: "Reusable workflow",
  composite: "Composite",
  javascript: "JavaScript",
  docker: "Docker",
  firecracker: "Firecracker",
  shards: "Shards",
  unknown: "Unknown",
};
const DIALECT_WORD: Record<string, string> = { gha: "GitHub's", kiln: "kiln's" };

export default function ShipPipelinesPage() {
  return (
    <div className="container-console page ship">
      <title>Pipelines · Ship · kiln</title>
      <PageIntro index={2} label="Ship" title="Ship." lead="Every commit and change on its way from review to production, and what it needs of you." />
      <ShipViews />
      <Loadable label="Loading pipelines">
        <PipelinesList />
      </Loadable>
    </div>
  );
}

/** What a row is found by under a facet: its repository, kind, dialect, stand's word, and whether its repository shares. */
const fieldOf = (repo: ShipPipelineRepo, r: ShipPipelineRow, k: Facet): string =>
  k === "repo"
    ? repo.repo
    : k === "kind"
      ? (KIND_WORD[r.kind] ?? r.kind)
      : k === "dialect"
        ? r.dialect
          ? (DIALECT_WORD[r.dialect] ?? r.dialect)
          : ""
        : k === "stand"
          ? r.word
          : repo.access && repo.access !== "none"
            ? "Shared"
            : "Not shared";

/** A row's place in the address (`?repo=&path=`): a draft's is its file's, with its id. */
const rowKey = (r: ShipPipelineRow) => r.key;

function PipelinesList() {
  const [params, setParams] = useSearchParams();
  const list = useSuspenseResource("ship/pipelines", (s) => shipApi.pipelines("", s), { on: PIPELINE_EVENTS, pollMs: 30_000 });
  const q = params.get("q") ?? "";
  const openRepo = params.get("repo");
  const openPath = params.get("path");
  const openDraft = params.get("draft");
  // The row a link opened the page on is brought into view, once; one opened here leaves the page where it is.
  const [landed] = React.useState(openRepo && openPath ? `${openRepo}:${openPath}` : null);
  const chosen: Chosen = Object.fromEntries(FACETS.map((k): [string, string[]] => [k, (params.get(k) ?? "").split(",").filter(Boolean)]).filter(([, v]) => v.length > 0));
  const set = (patch: Record<string, string | null>) => {
    const next = new URLSearchParams(params);
    for (const [k, v] of Object.entries(patch)) {
      if (v) next.set(k, v);
      else next.delete(k);
    }
    setParams(next, { replace: true });
  };
  const repos = list.data.repos;
  const pairs = repos.flatMap((p) => p.rows.map((r) => [p, r] as const));
  // Found as the server finds: a repository, a name, a path, a kind, a word or its line.
  const words = q.trim().toLowerCase();
  const shows = (p: ShipPipelineRepo, r: ShipPipelineRow) =>
    (!words || [p.repo, r.name, r.path, KIND_WORD[r.kind] ?? r.kind, r.word, r.line].some((v) => v.toLowerCase().includes(words))) &&
    FACETS.every((k) => !chosen[k]?.length || chosen[k].includes(fieldOf(p, r, k)));
  const shown = repos.map((p) => ({ ...p, rows: p.rows.filter((r) => shows(p, r)) })).filter((p) => p.rows.length > 0 || (!words && !Object.keys(chosen).length));
  const isOpen = (p: ShipPipelineRepo, r: ShipPipelineRow) => openRepo === p.repo && openPath === r.path && (openDraft ?? null) === (r.draft?.id ?? null);
  const open = (p: ShipPipelineRepo, r: ShipPipelineRow | null, o: boolean) =>
    set(o && r ? { repo: p.repo, path: r.path, draft: r.draft?.id ?? null, new: null } : o ? { repo: p.repo, path: null, draft: null, new: "1" } : { repo: null, path: null, draft: null, new: null });
  return (
    <>
      <FacetBox
        className="ship-facet"
        label="Find workflows and actions"
        placeholder="Repository, workflow, action, kind, machine…"
        query={q}
        onQuery={(v) => set({ q: v || null })}
        facets={FACETS.map((k) => ({ id: k, label: FACET_LABEL[k], options: optionsOf(pairs.map(([p, r]) => fieldOf(p, r, k))) }))}
        chosen={chosen}
        onChosen={(next) => set(Object.fromEntries(FACETS.map((k) => [k, next[k]?.join(",") || null])))}
      />
      {shown.length ? (
        <ul className="ship-list dp-list ppl-list" aria-label="Workflows and actions by repository">
          {shown.map((p) => (
            <React.Fragment key={p.repo}>
              <li className="dp-proj">
                <span>{p.repo}</span>
                <span data-needs={p.needs || undefined}>{p.say}</span>
              </li>
              {p.rows.map((r) => (
                <PipelineRow key={rowKey(r)} repo={p} row={r} open={isOpen(p, r)} landed={landed === `${p.repo}:${r.path}`} onOpen={(o) => open(p, r, o)} />
              ))}
              <NewRow repo={p} open={openRepo === p.repo && params.get("new") === "1"} onOpen={(o) => open(p, null, o)} />
            </React.Fragment>
          ))}
        </ul>
      ) : (
        <EmptyState title={pairs.length ? "No workflow or action is one of those" : "No repository you may read has a workflow or action yet"}>
          {pairs.length ? "Find by another repository, name, kind or stand." : "A workflow is a file in .github/workflows or .kiln; an action a folder with its action.yml."}
        </EmptyState>
      )}
    </>
  );
}

/** Who changed it last, as the stills name them: a person by name, an agent as one, kiln's update drafts as kiln. */
const byWord = (by: string) => (by.startsWith("kiln-updates:") ? "kiln" : by.startsWith("agent:") ? "agent" : by.replace(/^user:/, ""));

/** How long ago, in the fewest words (`12m`). */
const since = (s: number) => relativeTime(s).replace(/ ago$/, "");

/** A row a link opened the page on, brought into view as it's first drawn. */
const intoView = (el: HTMLLIElement | null) => el?.scrollIntoView({ block: "start" });

/** A row's three pips (Checks, Runs on, Latest) as the journey strip draws them; a glyph the page doesn't know shows as not yet. */
const pipsOf = (pips: readonly string[]): Pip[] => PIPELINE_STAGES.map((stage, i) => ({ stage, state: (pips[i] && pips[i] in GLYPHS ? pips[i] : "none") as GlyphState }));

/** Where a row is (W29-R02): a draft's change (`#214`), else an action's or reusable workflow's newest tag, else its branch. */
function Where({ row }: { row: ShipPipelineRow }) {
  if (row.draft?.change)
    return (
      <span className="ppl-ref" data-ref="change">
        {row.draft.change.name}
      </span>
    );
  if (row.released)
    return (
      <span className="ppl-ref" data-ref="tag">
        <Tag size={11} aria-hidden="true" />
        {row.released}
      </span>
    );
  return <span className="ppl-ref">{row.ref}</span>;
}

/** DRAFT, beside a draft's title (Ada, 2026-10-31: "except for the draft label location"). */
export function DraftMark() {
  return (
    <span className="ppl-draft">
      <FileText size={12} aria-hidden="true" />
      DRAFT
    </span>
  );
}

/** A workflow or action as Ship's row: its name, its stand and line, its pips, who changed it and when, where it is. No buttons but to open it. */
function PipelineRow({ repo, row, open, landed, onOpen }: { repo: ShipPipelineRepo; row: ShipPipelineRow; open: boolean; landed: boolean; onOpen: (open: boolean) => void }) {
  return (
    <li className="ship-row dp-row ppl-row" data-stand={row.stand} data-key={row.key} ref={landed ? intoView : undefined}>
      <div className="ship-r">
        <button type="button" className="ship-main" aria-expanded={open} onClick={() => onOpen(!open)}>
          <span className="ship-name ppl-name">
            <span className="ppl-title">{row.name}</span>
            {row.draft ? <DraftMark /> : null}
          </span>
          <span className="ship-line">
            <span className="ship-stand" data-stand={row.stand}>
              {row.word}
            </span>
            <span className="ship-text">{row.line}</span>
          </span>
        </button>
        <span className="ship-pips">
          <JourneyStrip pips={pipsOf(row.pips)} />
        </span>
        <span className="ship-metas">
          <span className="dp-by">
            {row.who ? (
              <>
                <span className="ship-who">
                  <StatusGlyph state={row.who.startsWith("agent:") || row.who.startsWith("kiln-updates:") ? "agent" : "person"} size={12} />
                  <span>{byWord(row.who)}</span>
                </span>
                <span>·</span>
              </>
            ) : null}
            <span className="dp-ago">{row.when ? since(row.when) : "never"}</span>
          </span>
          <span className="ship-m2">
            <Where row={row} />
            <span className="ship-sha">{row.commit.slice(0, 7)}</span>
          </span>
        </span>
        <span className="ship-acts" aria-hidden="true" />
        <button type="button" className="ship-go" aria-expanded={open} aria-label={`${open ? "Close" : "Open"} ${row.name}`} onClick={() => onOpen(!open)}>
          <ChevronDown size={16} />
        </button>
      </div>
      {open ? (
        <div className="ship-open dp-open">
          <Loadable label={`Loading ${row.name}`} fallback={<Loading label={`Loading ${row.name}`} block className="dp-slot" />}>
            <PipelinePane repo={repo} row={row} />
          </Loadable>
        </div>
      ) : null}
    </li>
  );
}

/** A repository's last row (W29-R08): a new workflow or action, opened in place. */
function NewRow({ repo, open, onOpen }: { repo: ShipPipelineRepo; open: boolean; onOpen: (open: boolean) => void }) {
  return (
    <li className="ship-row dp-row ppl-row ppl-new" data-key={`${repo.repo}:new`}>
      <div className="ship-r">
        <button type="button" className="ship-main" aria-expanded={open} onClick={() => onOpen(!open)}>
          <span className="ship-name ppl-name">
            <Plus size={14} aria-hidden="true" />
            <span className="ppl-title">New workflow or action</span>
          </span>
          <span className="ship-line">
            <span className="ship-text">in {repo.repo} · starts as a draft, unpublished until it lands</span>
          </span>
        </button>
        <span className="ship-pips" />
        <span className="ship-metas" />
        <span className="ship-acts" aria-hidden="true" />
        <button type="button" className="ship-go" aria-expanded={open} aria-label={`${open ? "Close" : "Open"} a new workflow or action in ${repo.repo}`} onClick={() => onOpen(!open)}>
          <ChevronDown size={16} />
        </button>
      </div>
      {open ? (
        <div className="ship-open dp-open">
          <div className="jp ppl-pane">
            <div className="ppl-kicker">
              <span>New in {repo.repo}</span>
              <span>starts as a draft: unpublished until it lands</span>
            </div>
          </div>
        </div>
      ) : null}
    </li>
  );
}

/** An opened workflow or action (W29-R04): its head, then the jobs pane's anatomy. */
function PipelinePane({ repo, row }: { repo: ShipPipelineRepo; row: ShipPipelineRow }) {
  const draft = row.draft?.id ?? null;
  const pane = useSuspenseResource(`ship/pipeline/${repo.repo}/${row.path}/${draft ?? ""}`, (s) => shipApi.pipeline(repo.repo, row.path, { draft }, s), { on: PIPELINE_EVENTS, pollMs: 15_000 }).data;
  const h = pane.head;
  const where = [h.path, h.dialect ? `${DIALECT_WORD[h.dialect] ?? h.dialect} dialect` : (KIND_WORD[h.kind] ?? h.kind), h.ref, h.commit.slice(0, 7)].filter(Boolean).join(" · ");
  return (
    <div className="jp ppl-pane">
      <div className="ppl-kicker">
        <span className="ppl-name">
          <span className="ppl-title">{h.title}</span>
          {h.draft ? <DraftMark /> : null}
        </span>
        <span>{where}</span>
      </div>
    </div>
  );
}

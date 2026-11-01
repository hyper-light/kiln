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
import { ci } from "@/api/ci";
import { shipApi, type ShipPipelineAct, type ShipPipelineGroup, type ShipPipelineLine, type ShipPipelinePane, type ShipPipelineRepo, type ShipPipelineRow, type ShipPipelineVerb } from "@/api/ship";
import { Toks } from "@/components/code/highlighted";
import { FacetBox, type Chosen } from "@/components/ui/facet-box";
import { Loadable, Loading } from "@/components/ui/loading";
import { EmptyState, PageIntro } from "@/components/ui/misc";
import { JourneyStrip, StatusGlyph } from "@/components/ui/status-glyph";
import { GLYPHS, PIPELINE_STAGES, type GlyphState, type Pip } from "@/components/ui/status-glyph-spec";
import { errorMessage, useMediaQuery } from "@/lib/hooks";
import { spotOf, type Spot } from "@/lib/pipeline-spot";
import { Link, useSearchParams } from "@/lib/router";
import { invalidate, useSuspenseResource } from "@/lib/resource";
import { relativeTime } from "@/lib/format";
import { resolveLanguage, useHighlighter } from "@/lib/syntax/browser";
import { TryPane } from "@/views/ci/action-view";
import { FlowView } from "@/views/ci/flow-view";
import { glyphOf, optionsOf } from "./model";
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

/** A pick's key: a pane line's (`on.push`, `jobs.rust`, `uses.kiln/checkout@v1`, `versions.head`). */
type Key = string;

/** A verb as the strip draws it: dimmed (with its why) where it doesn't apply; Keep and the asked verb while it asks again. */
interface StripVerb {
  key: string;
  label: string;
  enabled: boolean;
  dim?: boolean;
  keep?: boolean;
  go?: boolean;
  run: () => void;
}

/** The detail's code, held to this many lines, scrolling inside itself: beside the list, and under its line on a phone. */
const SIDE_ROWS = 18;
const PHONE_ROWS = 14;

/** What's shown before anything's picked: the update's change, a workflow's first job, an action's tip, else the first line. */
function firstPick(p: ShipPipelinePane): Key | null {
  const first = (name: string | ((n: string) => boolean)) => p.groups.find((g) => (typeof name === "string" ? g.name === name : name(g.name)))?.lines[0]?.key;
  return first("What changes for it") ?? first((n) => n.startsWith("Stage ")) ?? first("Versions") ?? p.groups.find((g) => g.lines.length)?.lines[0]?.key ?? null;
}

/** A group's words on its right, as the stills put them: how many, what a stage waits on, where runs are from. */
function noteOf(g: ShipPipelineGroup, p: ShipPipelinePane): string {
  const n = g.lines.length;
  if (g.name.startsWith("Stage ")) {
    if (g.name === "Stage 1") return n === 1 ? "1 job" : `${n} jobs at once`;
    const after = [...new Set(g.lines.flatMap((l) => /· needs (.+)$/.exec(l.value)?.[1]?.split(", ") ?? []))];
    return after.length ? `after ${after.join(" and ")}` : `${n} jobs`;
  }
  switch (g.name) {
    case "Secrets and variables":
      return "names only";
    case "Runs":
      return `on ${p.head.ref}`;
    case "Versions":
      return "tags";
    case "Used by":
      return g.lines.some((l) => l.kind === "consumer") ? `${new Set(g.lines.map((l) => l.name)).size} repositories` : "";
    case "Settings":
    case "What changes for it":
    case "Upstream":
      return "";
    default:
      return n ? String(n) : "none";
  }
}

/** A use's source, as its line's kind word says it: built in, this repository, an image, GitHub's, or the repository's name. */
function sourceOf(uses: string): string {
  if (uses.startsWith("kiln/")) return "built in";
  if (uses.startsWith("./")) return "this repository";
  if (uses.startsWith("docker://")) return "image";
  const [owner, repo] = uses.replace(/^kiln:\/\//, "").split("/");
  return owner === "actions" || owner === "github" ? "GitHub" : (repo?.split("@")[0] ?? owner ?? "");
}

/** A use as its line names it: an action or workflow in another repository by its own name (`setup-rust@v2.3.0`), the kind word saying whose; the rest as written. */
function shortUse(uses: string): string {
  const u = uses.replace(/^kiln:\/\//, "");
  const at = u.lastIndexOf("@");
  const [path, ref] = at > 0 ? [u.slice(0, at), u.slice(at + 1)] : [u, ""];
  const parts = path.split("/");
  if (parts.length <= 2 || u.startsWith("./") || u.startsWith("docker://")) return u;
  const own = (parts.at(-1) ?? path).replace(/\.ya?ml$/, "");
  return ref ? `${own}@${ref}` : own;
}

/** A line's kind word, its name and its value, as the pane draws them. */
function wordsOf(l: ShipPipelineLine): { kind: string | null; name: string; value: string } {
  switch (l.kind) {
    case "trigger":
      return { kind: l.name, name: l.value, value: "" };
    case "use":
      return { kind: sourceOf(l.name), name: shortUse(l.name), value: l.value.split(" · ").slice(1).join(" · ") };
    case "version":
      return { kind: l.key === "versions.head" ? "main" : "tag", name: l.key === "versions.head" ? l.value.slice(0, 7) : l.name, value: l.key === "versions.head" ? "" : l.value.slice(0, 7) };
    case "job":
    case "secret":
    case "variable":
    case "input":
    case "output":
    case "step":
    case "image":
    case "entry":
      return { kind: l.kind, name: l.name, value: l.value };
    default:
      return { kind: null, name: l.name, value: l.value };
  }
}

/** A line's glyph: its own state where it has one; a setting off or a note as not yet; the rest as there. */
const glyphFor = (l: ShipPipelineLine): string => l.state ?? (l.kind === "note" || (l.kind === "setting" && l.key !== "settings.on") ? "none" : "passed");

/** What a picked line's detail shows (W29-R04): a job's YAML · Plan · Last run, a trigger's Times or Filters, a use's own, a run's jobs; an action's Changes · YAML · Try it. */
function viewsFor(p: ShipPipelinePane, l: ShipPipelineLine | null): string[] {
  const action = !["workflow", "reusable"].includes(p.head.kind);
  if (action) return p.detail.views.length ? p.detail.views : ["YAML"];
  if (!l) return p.detail.views.length ? p.detail.views : ["YAML"];
  switch (l.kind) {
    case "job":
      return ["YAML", "Plan", "Last run"];
    case "trigger":
      return [l.key === "on.schedule" ? "Times" : "Filters", "YAML", "Plan"];
    case "use":
      return ["Use", "YAML"];
    case "run":
      return ["Run"];
    case "change":
    case "upstream":
    case "note":
      return ["Changes", "YAML", "Plan"];
    default:
      return ["YAML", "Flow", "Plan"];
  }
}

/** The event a workflow's plan is for: its picked trigger's, else its first. */
function eventOf(p: ShipPipelinePane, l: ShipPipelineLine | null): string | null {
  const triggers = p.groups.find((g) => g.name === "Triggers")?.lines ?? [];
  const t = l?.kind === "trigger" ? l : triggers[0];
  return t ? t.key.replace(/^on\./, "") : null;
}

/** An opened workflow or action (W29-R04): its head, the strip, its groups, and the picked line's detail beside them (under its line on a phone). */
function PipelinePane({ repo, row }: { repo: ShipPipelineRepo; row: ShipPipelineRow }) {
  const draft = row.draft?.id ?? null;
  const pane = useSuspenseResource(`ship/pipeline/${repo.repo}/${row.path}/${draft ?? ""}`, (s) => shipApi.pipeline(repo.repo, row.path, { draft }, s), { on: PIPELINE_EVENTS, pollMs: 15_000 });
  const p = pane.data;
  const h = p.head;
  const phone = useMediaQuery("(max-width: 700px)");
  const [picked, setPicked] = React.useState<Key | null>(null);
  const [said, setSaid] = React.useState<{ text: React.ReactNode; refused: boolean } | null>(null);
  const [armed, setArmed] = React.useState<ShipPipelineVerb | null>(null);
  const [busy, setBusy] = React.useState(false);
  // The file as it's being written: typing (or the Flow view) starts a draft, Not saved until it's saved (W29-R05, R10).
  const [text, setText] = React.useState(p.detail.text);
  const changed = text !== p.detail.text;
  const lines = p.groups.flatMap((g) => g.lines);
  const at = picked && lines.some((l) => l.key === picked) ? picked : firstPick(p);
  const line = lines.find((l) => l.key === at) ?? null;
  const groupOf = (k: Key) => p.groups.find((g) => g.lines.some((l) => l.key === k))?.name ?? "";
  const where = [h.path, h.dialect ? `${DIALECT_WORD[h.dialect] ?? h.dialect} dialect` : `${KIND_WORD[h.kind] ?? h.kind} action`, h.ref, h.commit.slice(0, 7)].filter(Boolean).join(" · ");
  const act = async (body: Omit<ShipPipelineAct, "repo" | "path">, done: React.ReactNode) => {
    setBusy(true);
    setArmed(null);
    try {
      await shipApi.pipelinesAct({ repo: repo.repo, path: h.path, ...body });
      setSaid({ text: done, refused: false });
    } catch (e) {
      setSaid({ text: errorMessage(e), refused: true });
    } finally {
      setBusy(false);
      pane.reload();
      invalidate("ship/pipelines");
    }
  };
  const onVerb = (v: ShipPipelineVerb) => {
    switch (v.verb) {
      case "run":
        return void act(
          { verb: "run", ...(eventOf(p, line) ? { event: eventOf(p, line) ?? undefined } : {}) },
          <>
            Running <b>{h.title}</b>.
          </>,
        );
      case "save_draft":
        return void act(
          { verb: "save_draft", content: text },
          <>
            Saved <b>{h.title}</b> as a draft: unpublished until it lands.
          </>,
        );
      case "ready":
        return void act(
          { verb: "ready" },
          <>
            Asked for <b>{h.title}</b>&apos;s review.
          </>,
        );
      case "discard":
        return void act(
          { verb: "discard" },
          <>
            Discarded the draft of <b>{h.title}</b>.
          </>,
        );
      default:
        return setArmed(v);
    }
  };
  const say: React.ReactNode = armed ? (
    (armed.asks ?? `${armed.label}?`)
  ) : said ? (
    said.text
  ) : changed ? (
    <>Not saved yet · a draft runs on no trigger until it lands</>
  ) : line ? (
    <>
      <b>{wordsOf(line).name || line.name}</b> picked · {groupOf(line.key).toLowerCase()}
      {wordsOf(line).value ? ` · ${wordsOf(line).value}` : ""}
    </>
  ) : (
    p.strip.said.join(" · ")
  );
  // A verb that doesn't apply is dimmed, never hidden; picking it says why (W29-R04).
  const whyNot = (v: ShipPipelineVerb): string | null =>
    !v.enabled ? (v.why_not ?? `${v.label} doesn't apply here`) : v.verb === "save_draft" && !changed ? "Nothing to save yet: typing in its YAML starts a draft (W29-R05)" : null;
  const verbs: StripVerb[] = armed
    ? [
        { key: "keep", label: "Keep", enabled: true, keep: true, run: () => setArmed(null) },
        {
          key: armed.verb,
          label: armed.label,
          enabled: !busy,
          go: true,
          run: () => void act({ verb: armed.verb as ShipPipelineAct["verb"], sha: h.commit }, <>{armed.label}: asked of the server.</>),
        },
      ]
    : p.strip.verbs.map((v) => {
        const why = whyNot(v);
        return { key: v.verb, label: v.label, enabled: !why && !busy, dim: Boolean(why), run: () => (why ? setSaid({ text: why, refused: true }) : onVerb(v)) };
      });
  const pick = (k: Key) => {
    setPicked(k);
    setSaid(null);
    setArmed(null);
  };
  const detail = <Detail key={at ?? "file"} pane={p} line={line} text={text} onText={setText} rows={phone ? PHONE_ROWS : SIDE_ROWS} />;
  // On a phone, the picked line's detail is right under it, as the approved stills draw it.
  const below = (k: Key) => (phone && k === at ? <li className="jp-detail">{detail}</li> : null);
  return (
    <section aria-label="Pipeline" className="ppl">
      <div className="ppl-kicker">
        <span className="ppl-name">
          <span className="ppl-title">{h.title}</span>
          {h.draft || changed ? <DraftMark /> : null}
        </span>
        <span>{where}</span>
      </div>
      <div className="jp">
        <div className="jp-strip">
          <div className="jp-say" data-tone={armed ? "ask" : said?.refused ? "warn" : undefined}>
            <div className="jp-said" aria-live="polite">
              {say}
            </div>
            <div className="jp-q">
              <span>pick:</span>
              {p.strip.pick.map((w, i) => (
                <React.Fragment key={w}>
                  {i ? <span aria-hidden>·</span> : null}
                  <button type="button" aria-pressed={w === "all"}>
                    {w}
                  </button>
                </React.Fragment>
              ))}
            </div>
          </div>
          <div className="jp-verbs ppl-verbs" data-armed={armed ? true : undefined}>
            {verbs.map((v) => (
              <button key={v.key} type="button" aria-disabled={!v.enabled || undefined} data-dim={v.dim || undefined} data-keep={v.keep || undefined} data-go={v.go || undefined} onClick={v.run}>
                {v.label}
              </button>
            ))}
          </div>
        </div>
        <div className="jp-body ppl-body">
          <ol className="jp-list ppl-list" aria-label={`${h.title}'s parts`}>
            {p.groups.map((g) => (
              <React.Fragment key={g.name}>
                <li className="jp-g" data-group={g.name}>
                  <span>{g.name}</span>
                  <span>{noteOf(g, p)}</span>
                </li>
                {g.lines.map((l) => (
                  <React.Fragment key={l.key}>
                    <PaneLine line={l} at={at === l.key} onLine={pick} />
                    {below(l.key)}
                  </React.Fragment>
                ))}
              </React.Fragment>
            ))}
          </ol>
          {phone ? null : <div className="jp-side ppl-side">{detail}</div>}
        </div>
      </div>
    </section>
  );
}

/** One line: its glyph, its kind word and name, its value; picking it shows its detail. */
function PaneLine({ line, at, onLine }: { line: ShipPipelineLine; at: boolean; onLine: (k: Key) => void }) {
  const w = wordsOf(line);
  return (
    <li className="jp-j" data-key={line.key} data-sel={at || undefined} data-at={at || undefined} data-state={line.state}>
      <span className="jp-glyph" aria-hidden="true">
        <StatusGlyph state={glyphOf(glyphFor(line))} size={12} />
      </span>
      <button type="button" className="jp-line" aria-current={at || undefined} onClick={() => onLine(line.key)}>
        <span className="n">
          {w.kind ? <em>{w.kind}</em> : null}
          {w.name}
        </span>
        <span className="v">{w.value}</span>
      </button>
    </li>
  );
}

/** The picked line's detail (W29-R04): its name and the file's, its views, and the view shown. */
function Detail({ pane, line, text, onText, rows }: { pane: ShipPipelinePane; line: ShipPipelineLine | null; text: string; onText: (t: string) => void; rows: number }) {
  const views = viewsFor(pane, line);
  const [view, setView] = React.useState(views[0] ?? "YAML");
  const shown = views.includes(view) ? view : (views[0] ?? "YAML");
  const name = line ? `${wordsOf(line).name || line.name} · ${pane.head.path}` : pane.head.path;
  return (
    <div className="ppl-detail">
      <div className="ppl-dhead">
        <span className="ppl-dname">{name}</span>
        <span className="dp-w ppl-views" role="tablist" aria-label="Views">
          {views.map((v) => (
            <button key={v} type="button" role="tab" aria-selected={v === shown} onClick={() => setView(v)}>
              {v}
            </button>
          ))}
        </span>
      </div>
      <div className="ppl-dbody" style={{ "--rows": rows } as React.CSSProperties}>
        <Loadable label={`Loading ${shown}`} fallback={<Loading label={`Loading ${shown}`} block className="ppl-slot" />}>
          <View view={shown} pane={pane} line={line} text={text} onText={onText} rows={rows} />
        </Loadable>
      </div>
    </div>
  );
}

function View({ view, pane, line, text, onText, rows }: { view: string; pane: ShipPipelinePane; line: ShipPipelineLine | null; text: string; onText: (t: string) => void; rows: number }) {
  const h = pane.head;
  switch (view) {
    case "YAML":
      return <CodeView text={text} path={h.path} spot={line ? spotOf(text, line) : null} rows={rows} />;
    case "Flow":
      return <FlowView repo={pane.repo} text={text} onChange={onText} />;
    case "Plan":
      return <PlanView pane={pane} text={text} event={eventOf(pane, line)} />;
    case "Try it":
      return <TryPane repo={pane.repo} path={h.path} gitRef={h.ref} text={text} blocked={text !== pane.detail.text} />;
    case "Last run":
    case "Run":
      return <Lines lines={(pane.groups.find((g) => g.name === "Runs")?.lines ?? []).filter((l) => view === "Last run" || l.key === line?.key)} empty="No run yet on its default branch." />;
    case "Times":
    case "Filters":
      return (
        <Lines
          lines={(line?.value ?? "")
            .split(" · ")
            .filter(Boolean)
            .map((v, i) => ({ key: `${line?.key}.${i}`, kind: "filter", name: v, value: "" }))}
          empty="It has no filters: every one of its events starts it."
        />
      );
    case "Use":
      return line ? <UseView line={line} /> : null;
    case "Changes":
      return <Lines lines={pane.groups.filter((g) => ["What changes for it", "Upstream", "Versions"].includes(g.name)).flatMap((g) => g.lines)} empty="Nothing has changed since its newest tag." />;
    default:
      return null;
  }
}

/** Lines as the detail lists them: a glyph, the words, a value. */
function Lines({ lines, empty }: { lines: ShipPipelineLine[]; empty: string }) {
  if (!lines.length) return <p className="ppl-empty">{empty}</p>;
  return (
    <ul className="ppl-lines">
      {lines.map((l) => {
        const w = wordsOf(l);
        return (
          <li key={l.key}>
            <StatusGlyph state={glyphOf(glyphFor(l))} size={12} />
            <span className="n">
              {w.kind ? <em>{w.kind}</em> : null}
              {w.name}
            </span>
            <span className="v">{w.value}</span>
          </li>
        );
      })}
    </ul>
  );
}

/** A use's own detail: where it's from and the version it pins, and its row here when kiln hosts it. */
function UseView({ line }: { line: ShipPipelineLine }) {
  const at = /^(?:kiln:\/\/)?([^/@]+\/[^/@]+)(\/[^@]*)?@(.+)$/.exec(line.name);
  const local = at && !line.name.startsWith("kiln/") && !line.name.startsWith("actions/");
  return (
    <ul className="ppl-lines">
      <li>
        <StatusGlyph state="passed" size={12} />
        <span className="n">
          <em>{sourceOf(line.name)}</em>
          {line.name}
        </span>
        <span className="v">{line.value}</span>
      </li>
      {local && at ? (
        <li>
          <StatusGlyph state="none" size={12} />
          <span className="n">
            <Link to={`/ship/pipelines?repo=${encodeURIComponent(at[1] ?? "")}&path=${encodeURIComponent((at[2] ?? "").replace(/^\//, ""))}`}>Open it in Pipelines</Link>
          </span>
          <span className="v">{at[3]}</span>
        </li>
      ) : null}
    </ul>
  );
}

/** The file in its language's colours (kiln's tree-sitter engine), its lines numbered, the picked part's lines tinted and opened at; held to `rows` lines, scrolling inside itself. */
function CodeView({ text, path, spot, rows }: { text: string; path: string; spot: Spot | null; rows: number }) {
  const hl = useHighlighter(resolveLanguage(null, path, text));
  const toks = React.useMemo(() => hl?.(text) ?? null, [hl, text]);
  // Drawn anew for each pick (its key), so it opens there once; scrolling after that is the reader's.
  return (
    <ol key={spot?.from ?? -1} className="ppl-code" ref={openAtFirst} tabIndex={0} aria-label={`${path}'s code`} style={{ "--rows": rows } as React.CSSProperties}>
      {text.split("\n").map((l, i) => (
        <li key={i} data-at={(spot && i >= spot.from && i <= spot.to) || undefined} data-first={(spot && i === spot.from) || undefined}>
          <span className="ppl-ln" aria-hidden="true">
            {i + 1}
          </span>
          <code>{l ? <Toks text={l} toks={toks?.[i]} /> : " "}</code>
        </li>
      ))}
    </ol>
  );
}

/** A code list opened at its picked part's first line, a line above it. */
const openAtFirst = (el: HTMLOListElement | null) => {
  const first = el?.querySelector<HTMLElement>("[data-first]");
  if (el && first) el.scrollTop = Math.max(0, first.offsetTop - el.offsetTop - first.offsetHeight);
};

/** A text's hash (FNV-1a), for what's asked of it: the same text, the same answer. */
function hashOf(text: string): string {
  let h = 0x811c9dc5;
  for (let i = 0; i < text.length; i++) h = Math.imul(h ^ text.charCodeAt(i), 0x01000193) >>> 0;
  return h.toString(16);
}

/** The plan for an event (W04-R29's preview of the text): each job, its legs and where they'd run. */
function PlanView({ pane, text, event }: { pane: ShipPipelinePane; text: string; event: string | null }) {
  const plan = useSuspenseResource(`ci/plan-preview/${pane.repo}/${pane.head.path}/${event ?? ""}/${hashOf(text)}`, () =>
    ci.preview({ repo: pane.repo, path: pane.head.path, content: text, ...(event ? { event } : {}) }),
  ).data;
  if (!plan.plan) return <p className="ppl-empty">{plan.why ?? "It plans nothing for this event."}</p>;
  const jobs = [...new Set(plan.plan.legs.map((l) => l.job))];
  return (
    <ul className="ppl-lines" aria-label={`Its plan for ${event ?? "its event"}`}>
      {jobs.map((j) => {
        const legs = plan.plan?.legs.filter((l) => l.job === j) ?? [];
        const can = [...new Set(legs.flatMap((l) => (l.routing?.runners ?? []).filter((r) => r.eligible && r.connected).map((r) => r.name)))];
        return (
          <li key={j}>
            <StatusGlyph state="passed" size={12} />
            <span className="n">
              <em>job</em>
              {j}
            </span>
            <span className="v">
              {legs.length === 1 ? "1 leg" : `${legs.length} legs`}
              {can.length ? ` · ${can.join(", ")}` : ""}
            </span>
          </li>
        );
      })}
    </ul>
  );
}

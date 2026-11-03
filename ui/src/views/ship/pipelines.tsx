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
import { ChevronDown, FileText, Minus, Plus, Tag } from "lucide-react";
import { ci, type CiDiagnostic, type CiPreview, type CiSchedule } from "@/api/ci";
import { ApiError } from "@/api/client";
import {
  shipApi,
  type ShipPipelineAct,
  type ShipPipelineGroup,
  type ShipPipelineLine,
  type ShipPipelineNew,
  type ShipPipelinePane,
  type ShipPipelineRepo,
  type ShipPipelineRow,
  type ShipPipelineVerb,
  type ShipCodeChange,
  type ShipLockReview,
} from "@/api/ship";
import type { Diagnostic } from "@/components/code/editor";
import { Toks } from "@/components/code/highlighted";
import { FacetBox, type Chosen } from "@/components/ui/facet-box";
import { Loadable, Loading } from "@/components/ui/loading";
import { EmptyState, PageIntro } from "@/components/ui/misc";
import { JourneyStrip, StatusGlyph } from "@/components/ui/status-glyph";
import { GLYPHS, PIPELINE_STAGES, type GlyphState, type Pip } from "@/components/ui/status-glyph-spec";
import { errorMessage, useMediaQuery } from "@/lib/hooks";
import { changesIn, lineDiff } from "@/lib/line-diff";
import { jobsOf, planDiff, planLines, whereWords, type PlanDiff, type PlanJob, type PlanLine } from "@/lib/plan-diff";
import { manualInputs, spotOf, type ManualInput, type Spot } from "@/lib/pipeline-spot";
import { TRIGGER_GROUPS, TRIGGER_KINDS, triggerKeys, triggerLines, type TriggerKind } from "@/lib/triggers";
import { addedOf, jobResources, typedGroups } from "@/lib/typed-groups";
import { addOverlayTrigger, addTrigger, EditRefused, removeTrigger, setOverlayScheduleField, setScheduleField, setTriggerFilter } from "@/lib/workflow-doc";
import { parse as parseYaml } from "yaml";
import { Link, useSearchParams } from "@/lib/router";
import { invalidate, useSuspenseResource } from "@/lib/resource";
import { relativeTime } from "@/lib/format";
import { resolveLanguage, useHighlighter } from "@/lib/syntax/browser";
import { TryPane } from "@/views/ci/action-view";
import { FlowView } from "@/views/ci/flow-view";
import { glyphOf, optionsOf } from "./model";
import "./ship.css";
import { ShipViews } from "./views";

const CodeEditor = React.lazy(() => import("@/components/code/editor"));
type Editor = Parameters<NonNullable<React.ComponentProps<typeof CodeEditor>["onView"]>>[0];

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
  // Typed in and not saved: the row says so, as a draft does (W29-R10's Not saved), with how much it changes and its plan once checked (pl-3).
  const [notSaved, setNotSaved] = React.useState(() => unsaved.has(row.key) || unsaved.has(overlayKey(row.key)));
  const [typedLine, setTypedLine] = React.useState<string | null>(null);
  return (
    <li className="ship-row dp-row ppl-row" data-stand={row.stand} data-key={row.key} ref={landed ? intoView : undefined}>
      <div className="ship-r">
        <button type="button" className="ship-main" aria-expanded={open} onClick={() => onOpen(!open)}>
          <span className="ship-name ppl-name">
            <span className="ppl-title">{row.name}</span>
            {row.draft || notSaved ? <DraftMark /> : null}
          </span>
          <span className="ship-line">
            <span className="ship-stand" data-stand={notSaved ? "waiting" : row.stand}>
              {notSaved ? "Not saved" : row.word}
            </span>
            <span className="ship-text">{notSaved ? (typedLine ?? "yours · changed here, not saved yet") : row.line}</span>
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
            <PipelinePane repo={repo} row={row} onChanged={setNotSaved} onLine={setTypedLine} />
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
          <NewPane repo={repo} onClose={() => onOpen(false)} />
        </div>
      ) : null}
    </li>
  );
}

/** New's seven kinds (W29-R08), as Make lists them: what each is, and what it runs on. */
interface Make {
  kind: string;
  label: string;
  say: string;
  strip: string;
}
const MAKE_WORKFLOW: Make = { kind: "workflow", label: "Workflow", say: "jobs on triggers", strip: "jobs that run on its triggers, on bare metal unless a job says" };
const MAKE: readonly Make[] = [
  MAKE_WORKFLOW,
  { kind: "reusable", label: "Reusable workflow", say: "jobs others call", strip: "jobs other workflows call, with inputs, secrets and outputs" },
  { kind: "composite", label: "Composite action", say: "steps others use", strip: "steps other workflows use as one" },
  { kind: "javascript", label: "JavaScript action", say: "node24, in TypeScript", strip: "a Node program, written in TypeScript and built to dist/index.js" },
  { kind: "docker", label: "Docker action", say: "a container per call", strip: "a container per call, from the image its Dockerfile builds" },
  { kind: "firecracker", label: "Firecracker action", say: "a microVM per call", strip: "a microVM per call, booted from the image its Dockerfile builds, on machines with KVM" },
  { kind: "shards", label: "Shards action", say: "a Shards microVM per call", strip: "a Shards microVM per call, booted from the image its Dockerfile builds" },
];
/** Where New starts (W29-R08). */
type From = "blank" | "recommended" | "starter" | "copy";

/** A scaffold's file's language, for the editor's colours. */
const syntaxOfFile = (path: string): string | undefined => (/action\.ya?ml$/.test(path) || /\.ya?ml$/.test(path) ? "workflow" : resolveLanguage(null, path)?.id);

/** New, opened in place (W29-R08): what to make, in what, from where, its name; the files it makes, edited where they're shown; Create draft or Cancel. */
function NewPane({ repo, onClose }: { repo: ShipPipelineRepo; onClose: () => void }) {
  const [kind, setKind] = React.useState("workflow");
  const [kiln, setKiln] = React.useState(false);
  const [from, setFrom] = React.useState<From>("blank");
  const [name, setName] = React.useState("new-workflow");
  const [said, setSaid] = React.useState<{ text: React.ReactNode; refused: boolean } | null>(null);
  const [busy, setBusy] = React.useState(false);
  // The files as edited, by the dry run they came from: another kind, name or start makes them anew.
  const [edited, setEdited] = React.useState<{ key: string; files: Record<string, string> }>({ key: "", files: {} });
  const action = !["workflow", "reusable"].includes(kind);
  const make = MAKE.find((m) => m.kind === kind) ?? MAKE_WORKFLOW;
  const copyOf = repo.rows.find((r) => ["workflow", "reusable"].includes(r.kind) && !r.draft)?.path ?? null;
  const ask: ShipPipelineAct = {
    repo: repo.repo,
    path: "",
    verb: "new",
    kind,
    name,
    language: kiln && !action ? "kiln" : "gha",
    from: from === "starter" ? { starter: "rust" } : from === "copy" && copyOf ? { copy: copyOf } : from,
  };
  const key = JSON.stringify(ask);
  const create = async (files: { path: string; text: string }[]) => {
    setBusy(true);
    try {
      const out = (await shipApi.pipelinesAct({ ...ask, files: files.map((f) => ({ path: f.path, text: edited.key === key ? (edited.files[f.path] ?? f.text) : f.text })) })) as {
        draft?: { name?: string | null };
      } | null;
      setSaid({
        text: (
          <>
            Saved <b>{name}</b> as a draft{out?.draft?.name ? `, ${out.draft.name}` : ""}: unpublished until it lands.
          </>
        ),
        refused: false,
      });
      invalidate("ship/pipelines");
    } catch (e) {
      setSaid({ text: errorMessage(e), refused: true });
    } finally {
      setBusy(false);
    }
  };
  return (
    <section aria-label="New" className="ppl">
      <div className="ppl-kicker">
        <span>New in {repo.repo}</span>
        <span>starts as a draft: unpublished until it lands</span>
      </div>
      <Loadable label="Making its files" fallback={<Loading label="Making its files" block className="ppl-slot" />}>
        <NewBody
          key={key}
          ask={ask}
          askKey={key}
          make={make}
          action={action}
          from={from}
          name={name}
          said={said}
          busy={busy}
          edited={edited.key === key ? edited.files : {}}
          copyOf={copyOf}
          acts={{
            onKind: (k: string) => {
              setKind(k);
              setName(["workflow", "reusable"].includes(k) ? `new-${k === "reusable" ? "reusable" : "workflow"}` : `new-${k}`);
              setSaid(null);
            },
            onDialect: (k: boolean) => setKiln(k),
            onFrom: (f: From) => setFrom(f),
            onName: (n: string) => setName(n),
            onEdit: (path: string, text: string) => setEdited((e) => ({ key, files: { ...(e.key === key ? e.files : {}), [path]: text } })),
            onCreate: create,
            onCancel: onClose,
            onSay: (why: string) => setSaid({ text: why, refused: true }),
            kiln,
          }}
        />
      </Loadable>
    </section>
  );
}

interface NewActs {
  onKind: (k: string) => void;
  onDialect: (kiln: boolean) => void;
  onFrom: (f: From) => void;
  onName: (n: string) => void;
  onEdit: (path: string, text: string) => void;
  onCreate: (files: { path: string; text: string }[]) => void;
  onCancel: () => void;
  onSay: (why: string) => void;
  kiln: boolean;
}

/** New's strip, its choices and its files: the files a dry run of New makes (the server's `kiln_ci::scaffold`). */
function NewBody({
  ask,
  askKey,
  make,
  action,
  from,
  name,
  said,
  busy,
  edited,
  copyOf,
  acts,
}: {
  ask: ShipPipelineAct;
  askKey: string;
  make: (typeof MAKE)[number];
  action: boolean;
  from: From;
  name: string;
  said: { text: React.ReactNode; refused: boolean } | null;
  busy: boolean;
  edited: Record<string, string>;
  copyOf: string | null;
  acts: NewActs;
}) {
  const dry = useSuspenseResource(`ship/pipelines/new/${askKey}`, () => shipApi.pipelinesAct({ ...ask, dry: true }) as Promise<ShipPipelineNew>).data;
  const [file, setFile] = React.useState(0);
  const phone = useMediaQuery("(max-width: 700px)");
  const files = dry.files;
  const shown = files[Math.min(file, files.length - 1)];
  // W29-R08, pl-6: what connected machines run, from their facts, never their names.
  const whyKind = (m: Make) =>
    (m.kind === "shards" || m.kind === "firecracker") && dry.machines?.[m.kind].length === 0 ? `no machine runs ${m.kind === "shards" ? "shards" : "Firecracker"} yet` : null;
  const machines = make.kind === "firecracker" || make.kind === "shards" ? dry.machines?.[make.kind] : null;
  const own = String(ask.repo).split("/").pop() ?? String(ask.repo);
  const actionPath = dry.path.replace(/\/action\.ya?ml$/, "");
  // A file's tab: its path in the action's folder, or a workflow's own name.
  const folder = action ? dry.path.replace(/[^/]+$/, "") : "";
  const tab = (path: string) => (action ? path.slice(folder.length) : (path.split("/").pop() ?? path));
  const starts: [From, string, string | null][] = [
    ["blank", "blank", null],
    ["recommended", `recommended for ${String(ask.repo).split("/").pop() ?? ""}`, action ? "an action starts blank: recommended is for workflows" : null],
    ["starter", "a starter", action || acts.kiln || make.kind === "reusable" ? "GitHub's starter workflows are GitHub's dialect, run on push" : null],
    ["copy", "a copy", copyOf ? null : "this repository has no workflow to copy yet"],
  ];
  const detail = (
    <div className="ppl-detail">
      <div className="ppl-dhead">
        <span className="ppl-dname">
          <StatusGlyph state="running" size={12} label="Preview" className="ppl-preview" />
          {name} · {files.length === 1 ? "1 file" : `${files.length} files`}
        </span>
      </div>
      <ul className="ppl-lines ppl-fields" aria-label="Its name and path">
        <li data-field="name">
          <span className="n">Name</span>
          <Field value={name} label="Name" onSet={(v) => acts.onName(v.trim())} />
        </li>
        <li data-field="path">
          <span className="n">Path</span>
          <span className="v">{action ? actionPath : dry.path}</span>
        </li>
      </ul>
      <span className="dp-w ppl-views ppl-files" role="tablist" aria-label="Its files">
        {files.map((f, i) => (
          <button key={f.path} type="button" role="tab" aria-selected={f === shown} onClick={() => setFile(i)}>
            {tab(f.path)}
          </button>
        ))}
      </span>
      <div className="ppl-dbody ppl-newcode" style={{ "--rows": phone ? 14 : 16 } as React.CSSProperties}>
        {shown ? (
          <CodeEditor
            key={shown.path}
            value={edited[shown.path] ?? shown.text}
            onChange={(t) => acts.onEdit(shown.path, t)}
            label={`${shown.path}, as New makes it`}
            syntax={syntaxOfFile(shown.path)}
            inline
          />
        ) : null}
      </div>
      <div className="jp-g">
        <span>What happens</span>
        <span />
      </div>
      <ul className="ppl-lines">
        <li>
          <StatusGlyph state="draft" size={12} />
          <span className="n">
            Create draft keeps it unpublished on its own branch
            {dry.draft_name ? (
              <>
                , as <b>{dry.draft_name}</b> in {own}
              </>
            ) : (
              <> in {own}</>
            )}
            : no trigger runs it and nobody can use it
          </span>
        </li>
        <li>
          <StatusGlyph state="tag" size={12} />
          <span className="n">
            {action ? (
              <>
                others use it once a tag has it
                {dry.next_version ? (
                  <>
                    :{" "}
                    <b>
                      uses: {String(ask.repo)}/{actionPath}@{dry.next_version}
                    </b>
                  </>
                ) : null}
              </>
            ) : (
              "it runs on its triggers once it lands"
            )}
          </span>
        </li>
      </ul>
    </div>
  );
  return (
    <div className="jp">
      <div className="jp-strip">
        <div className="jp-say" data-tone={said?.refused ? "warn" : undefined}>
          <div className="jp-said" aria-live="polite">
            {said ? (
              said.text
            ) : (
              <>
                <b>{make.label}</b> · {make.strip.replace(/, on machines with KVM$/, "")}
                {machines ? (
                  <>
                    {" "}
                    · {make.kind === "firecracker" ? "runs on machines with KVM" : "runs on machines with shards"}: {machines.length ? <b>{machines.join(", ")}</b> : "none connected"}
                  </>
                ) : null}
              </>
            )}
          </div>
          <div className="jp-q">
            <span>start from:</span>
            {starts.map(([f, label, why], i) => (
              <React.Fragment key={f}>
                {i ? <span aria-hidden>·</span> : null}
                <button type="button" aria-pressed={from === f} aria-disabled={why ? true : undefined} onClick={() => (why ? acts.onSay(why) : acts.onFrom(f))}>
                  {label}
                </button>
              </React.Fragment>
            ))}
          </div>
        </div>
        <div className="jp-verbs ppl-verbs">
          <button type="button" className="ppl-wide" aria-disabled={busy || undefined} onClick={() => (busy ? undefined : acts.onCreate(files))}>
            Create draft
          </button>
          <button type="button" onClick={acts.onCancel}>
            Cancel
          </button>
        </div>
      </div>
      <div className="jp-body ppl-body">
        <ol className="jp-list ppl-list" aria-label="What to make">
          <li className="jp-g">
            <span>Make</span>
            <span />
          </li>
          {MAKE.map((m) => {
            const why = whyKind(m);
            return (
              <React.Fragment key={m.kind}>
                <li className="jp-j" data-key={`make.${m.kind}`} data-sel={m === make || undefined} data-at={m === make || undefined} data-dim={Boolean(why) || undefined}>
                  <span className="jp-glyph" aria-hidden="true">
                    <StatusGlyph state={why ? "blocked" : m === make ? "passed" : "none"} size={12} />
                  </span>
                  <button
                    type="button"
                    className="jp-line"
                    aria-current={m === make || undefined}
                    aria-disabled={Boolean(why) || undefined}
                    onClick={() => (why ? acts.onSay(why) : acts.onKind(m.kind))}
                  >
                    <span className="n">{m.label}</span>
                    <span className="v">{why ?? m.say}</span>
                  </button>
                </li>
                {phone && m === make ? <li className="jp-detail">{detail}</li> : null}
              </React.Fragment>
            );
          })}
          <li className="jp-g">
            <span>Write it in</span>
            <span>workflows</span>
          </li>
          {(
            [
              ["yaml", "YAML", "GitHub's or kiln's dialect"],
              ["ts", "TypeScript", "or Python, Go, Rust …"],
            ] as const
          ).map(([id, label, say]) => {
            const on = !action && id === "yaml";
            const dim = action || id === "ts";
            return (
              <React.Fragment key={id}>
                <li className="jp-j" data-key={`lang.${id}`} data-sel={on || undefined} data-dim={dim || undefined}>
                  <span className="jp-glyph" aria-hidden="true">
                    <StatusGlyph state={on ? "passed" : "none"} size={12} />
                  </span>
                  <button
                    type="button"
                    className="jp-line"
                    aria-disabled={dim || undefined}
                    onClick={() => (dim ? acts.onSay(action ? "An action is its action.yml: a dialect is a workflow's" : "Workflows in code come with W29-R27") : undefined)}
                  >
                    <span className="n">{label}</span>
                    <span className="v">{say}</span>
                  </button>
                </li>
                {id === "yaml" && !action ? (
                  <li className="jp-q ppl-dialect" aria-label="YAML dialect">
                    <span>dialect:</span>
                    {([false, true] as const).map((k) => (
                      <button key={String(k)} type="button" aria-pressed={acts.kiln === k} onClick={() => acts.onDialect(k)}>
                        {k ? "kiln's" : "GitHub's"}
                      </button>
                    ))}
                  </li>
                ) : null}
              </React.Fragment>
            );
          })}
        </ol>
        {phone ? null : <div className="jp-side ppl-side">{detail}</div>}
      </div>
    </div>
  );
}

/** Each row's text as it's being written, by its key, kept while the page is open (W29-R05: typing starts a draft); a GitHub workflow's overlay by its row's {@link overlayKey}. */
const unsaved = new Map<string, string>();
const overlayKey = (key: string) => `${key}#overlay`;

/** A pick's key: a pane line's (`on.push`, `jobs.rust`, `uses.kiln/checkout@v1`, `versions.head`). */
type Key = string;

/** A verb as the strip draws it: dimmed (with its why) where it doesn't apply; Keep and the asked verb while it asks again. */
interface StripVerb {
  key: string;
  label: string;
  enabled: boolean;
  dim?: boolean;
  keep?: boolean;
  /** The verb that answers an ask: `ask` where it takes nothing back (Release), else in a warning's tone. */
  go?: boolean | "ask";
  run: () => void;
}

/** The detail's code, held to this many lines, scrolling inside itself: beside the list, and under its line on a phone. */
const SIDE_ROWS = 28;
// W29-R06: the same phone frame for every workflow view, tall enough for all 18 wrapped kinds.
const PHONE_ROWS = 42;

/** What's shown before anything's picked: the use kiln's update draft moves (pl-7), a workflow's first job, an action's tip, else the first line. */
function firstPick(p: ShipPipelinePane): Key | null {
  const first = (name: string | ((n: string) => boolean)) => p.groups.find((g) => (typeof name === "string" ? g.name === name : name(g.name)))?.lines[0]?.key;
  const moved = p.groups.find((g) => g.name === "Uses")?.lines.find((l) => l.state === "changes")?.key;
  return moved ?? first((n) => n === "Jobs" || n.startsWith("Stage ")) ?? first("Versions") ?? p.groups.find((g) => g.lines.length)?.lines[0]?.key ?? null;
}

/** A group's words on its right, as the stills put them: how many, what a stage waits on, where runs are from. */
function noteOf(g: ShipPipelineGroup, p: ShipPipelinePane): string {
  // Add a trigger is the group's last line, not one of its triggers.
  const n = g.lines.filter((l) => l.kind !== "add").length;
  // What's typed in says how much is new (pl-3); an action's inputs mark theirs on their lines (pl-5).
  const added = g.name.startsWith("Stage ") || g.name === "Jobs" || g.name === "Uses" ? g.lines.filter((l) => l.state === "added").length : 0;
  // The uses kiln's update draft moves (pl-7's `3 · 1 update`).
  const moved = g.name === "Uses" ? g.lines.filter((l) => l.state === "changes").length : 0;
  const more = (added ? ` · ${added} new` : "") + (moved ? ` · ${moved} ${moved === 1 ? "update" : "updates"}` : "");
  if (g.name.startsWith("Stage ")) {
    if (g.name === "Stage 1") return (n === 1 ? "1 job" : `${n} jobs at once`) + more;
    const after = [...new Set(g.lines.flatMap((l) => /· needs (.+)$/.exec(l.value)?.[1]?.split(", ") ?? []))];
    return (after.length ? `after ${after.join(" and ")}` : `${n} jobs`) + more;
  }
  switch (g.name) {
    case "Jobs": {
      const from = [...new Set(g.lines.flatMap((l) => (l.value.startsWith("calls ") ? [sourceOf(l.value.slice(6), "a repository kiln hosts")] : [])))];
      return `${n}${from.length ? ` · called from ${from.join(", ")}` : ""}${more}`;
    }
    case "Secrets and variables":
      return "names only";
    case "Runs":
      // Runs of more than one event say so (pl-8); a push's alone, its branch (pl-2).
      return new Set(g.lines.map((l) => l.note ?? "")).size > 1 ? "by what started them" : `on ${p.head.ref}`;
    case "Versions":
      return "tags";
    case "Used by":
      return g.lines.some((l) => l.kind === "consumer") ? `${new Set(g.lines.map((l) => l.name)).size} repositories` : "";
    case "Steps":
      return `${KIND_WORD[p.head.kind]?.toLowerCase() ?? p.head.kind} · ${n}`;
    case "Settings":
      // Access is the whole repository's (pl-5's `all of ci-kit`).
      return g.lines.some((l) => l.key === "settings.access") ? `all of ${p.repo.split("/").pop() ?? p.repo}` : "";
    default:
      return n ? `${n}${more}` : "none";
  }
}

/** A use's source, as its line's kind word says it (pl-2): built in, this repository, an image, the name of a repository kiln hosts, else GitHub; by the server's words for where it's from, else by its name. */
function sourceOf(uses: string, from = ""): string {
  const f = from.replace(/^a reusable workflow, /, "");
  if (f === "a repository kiln hosts")
    return (
      uses
        .replace(/^kiln:\/\//, "")
        .split("/")[1]
        ?.split("@")[0] ?? "kiln"
    );
  if (f === "fetched by name") return "GitHub";
  if (uses.startsWith("kiln/") || f === "kiln's own") return "built in";
  if (uses.startsWith("./") || f === "this repository") return "this repository";
  if (uses.startsWith("docker://") || f === "a container image") return "image";
  if (uses.startsWith("kiln://")) return uses.slice("kiln://".length).split("/")[1]?.split("@")[0] ?? "kiln";
  return "GitHub";
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
      return { kind: l.name, name: l.value, value: l.note ?? "" };
    case "use":
      return { kind: sourceOf(l.name, l.value.split(" · ")[0]), name: shortUse(l.name), value: l.value.split(" · ").slice(1).join(" · ") };
    case "version":
      return { kind: l.key === "versions.head" ? "main" : "tag", name: l.name, value: l.value };
    case "step":
      // Numbered, as pl-5 draws them.
      return { kind: String(Number(l.key.split(".").pop() ?? "0") + 1), name: l.name, value: l.value };
    case "run":
      // A push is what most runs are: other events say so (pl-8's DEPLOY, SCHEDULE, COMMENT).
      return { kind: l.note && l.note !== "push" ? l.note.replace(/_/g, " ") : null, name: l.name, value: l.value };
    case "setting":
      return { kind: null, name: l.name, value: l.key === "settings.access" ? l.value.replace(/^shared with /, "") : l.value };
    case "job":
      // What it needs is its stage's note (`after rust and ui`), not its own.
      return {
        kind: l.state === "added" ? "new job" : l.value.startsWith("calls ") ? "calls" : "job",
        name: l.name,
        value: l.value.startsWith("calls ") ? shortUse(l.value.slice(6)).replace(/@[^ ·]+/, "") : l.value.replace(/(^| · )needs .*$/, ""),
      };
    case "secret":
    case "variable":
    case "input":
    case "output":
    case "image":
    case "entry":
      return { kind: l.kind, name: l.name, value: l.value };
    default:
      return { kind: null, name: l.name, value: l.value };
  }
}

/** A line's glyph: its own state where it has one; a setting off or a note as not yet; the rest as there. */
const glyphFor = (l: ShipPipelineLine): string =>
  // Access shared is as it should be (pl-5's ✓); not shared, not yet.
  l.key === "settings.access"
    ? l.state === "none"
      ? "none"
      : "passed"
    : l.state === "flag" && (l.kind === "use" || l.kind === "consumer")
      ? "changes"
      : (l.state ?? (l.kind === "version" && l.key !== "versions.head" ? "tag" : l.kind === "note" || (l.kind === "setting" && l.key !== "settings.on") ? "none" : "passed"));

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
      // The use kiln's update draft moves: what the update changes, its YAML, its plan (pl-7).
      return l.state === "changes" && (p.detail.update || p.lock_review) ? ["Changes", "YAML", "Plan"] : ["Use", "YAML"];
    case "run":
      return ["Run"];
    case "add":
      return ["Add a trigger"];
    case "note":
      return ["Changes", "YAML", "Plan"];
    default:
      return ["YAML", "Flow", "Plan"];
  }
}

/** What a plan is asked for (W29-R07): its event, in the strip's words, and the text it's set against (a draft's published file, or the text typed over); a GitHub workflow's overlay with each (W29-R06). */
interface PlanAsk {
  event: string | null;
  words: string | null;
  base: string | null;
  overlay: string | null;
  baseOverlay: string | null;
}

/** Events the preview can't plan without what they carry (a webhook's body, a comment's words): "plan for:" leaves them out (pl-8). */
const UNPLANNED = new Set(["webhook", "comment"]);

/** A trigger's event, as the preview takes it (kiln's `manual:` is GitHub's workflow_dispatch). */
const eventKey = (t: ShipPipelineLine) => t.key.replace(/^on\./, "");

/** A trigger as "plan for:" says it: a push to its branch, else its event's words. */
function eventWords(t: ShipPipelineLine): string {
  const branch = t.value.split(",")[0]?.trim();
  return t.name === "push" && branch && branch !== "any branch" && !branch.startsWith("tags ") ? `push to ${branch}` : t.name;
}

/** The plan of `text` for `event` (W04-R29's preview), with a GitHub workflow's overlay applied; a manual run's with its inputs' defaults. */
function planOf(pane: ShipPipelinePane, text: string, event: string | null, overlay: string | null = null): Promise<CiPreview> {
  const inputs = Object.fromEntries(manualInputs(text).flatMap((i) => (i.default === null ? [] : [[i.name, i.default]])));
  const ev = event === "workflow_dispatch" ? { name: event, inputs } : event;
  return ci.preview({ repo: pane.repo, path: pane.head.path, content: text, ...(overlay?.trim() ? { overlay } : {}), ...(ev ? { event: ev } : {}) });
}

/** The published text's plans, asked once each while the page is open: they don't change as you type. */
const basePlans = new Map<string, Promise<CiPreview>>();
function basePlanOf(pane: ShipPipelinePane, base: string, event: string | null, overlay: string | null = null): Promise<CiPreview> {
  const key = `${pane.repo}\0${pane.head.path}\0${event ?? ""}\0${hashOf(base)}\0${hashOf(overlay ?? "")}`;
  let had = basePlans.get(key);
  if (!had) {
    had = planOf(pane, base, event, overlay);
    had.catch(() => basePlans.delete(key));
    basePlans.set(key, had);
  }
  return had;
}

/** An opened workflow or action (W29-R04): its head, the strip, its groups, and the picked line's detail beside them (under its line on a phone). */
function PipelinePane({ repo, row, onChanged, onLine }: { repo: ShipPipelineRepo; row: ShipPipelineRow; onChanged: (changed: boolean) => void; onLine: (line: string | null) => void }) {
  const draft = row.draft?.id ?? null;
  const pane = useSuspenseResource(`ship/pipeline/${repo.repo}/${row.path}/${draft ?? ""}`, (s) => shipApi.pipeline(repo.repo, row.path, { draft }, s), { on: PIPELINE_EVENTS, pollMs: 15_000 });
  const p = pane.data;
  const h = p.head;
  const phone = useMediaQuery("(max-width: 700px)");
  const [picked, setPicked] = React.useState<Key | null>(null);
  const [said, setSaid] = React.useState<{ text: React.ReactNode; refused: boolean } | null>(null);
  // Release asked again in the strip (W29-R21): the tag picked, and, after a refusal, its why and Release anyway.
  const [release, setRelease] = React.useState<{ verb: ShipPipelineVerb; tag: string; confirm: boolean; why: string | null } | null>(null);
  const [busy, setBusy] = React.useState(false);
  // Run's inputs as they're asked in the strip (W29-R14): each by its name, as typed or picked.
  const [asking, setAsking] = React.useState<Record<string, string> | null>(null);
  // The file as it's being written: typing (or the Flow view) starts a draft, Not saved until it's saved (W29-R05, R10).
  const [text, setText] = React.useState(() => unsaved.get(row.key) ?? p.detail.text);
  // A GitHub workflow's overlay, where kiln's own triggers and its schedules' kiln keys go, so the file keeps working on GitHub (W02-R07, W29-R06).
  const ov = p.detail.overlay ?? null;
  const [overlay, setOverlay] = React.useState(() => unsaved.get(overlayKey(row.key)) ?? ov?.text ?? "");
  const textChanged = text !== p.detail.text;
  const overlayChanged = ov !== null && overlay !== ov.text;
  const changed = textChanged || overlayChanged;
  // What its YAML's check found, for the strip's `checks clean but for 1 warning` and the row's plan (pl-3).
  const [checked, setChecked] = React.useState<Checked | null>(null);
  // The row's words while it's typed in (pl-3): how many places it changes, and its plan's jobs against the published one's once checked.
  const lineFor = (t: string, o: string, c: Checked | null): string | null => {
    const n = changesIn(p.detail.text, t) + (ov ? changesIn(ov.text, o) : 0);
    if (!n) return null;
    const jobs = (k: number) => (k === 1 ? "1 job" : `${k} jobs`);
    const plan =
      c?.text === t && c.jobs
        ? c.jobs.was === null
          ? `plans ${jobs(c.jobs.now)}`
          : c.jobs.was === c.jobs.now
            ? `plans the same ${jobs(c.jobs.now)}`
            : `plans ${jobs(c.jobs.now)} (was ${c.jobs.was})`
        : null;
    return ["yours", n === 1 ? "1 change" : `${n} changes`, ...(plan ? [plan] : [])].join(" · ");
  };
  const edit = (t: string) => {
    setText(t);
    if (t === p.detail.text) unsaved.delete(row.key);
    else unsaved.set(row.key, t);
    onChanged(t !== p.detail.text || overlayChanged);
    onLine(lineFor(t, overlay, checked));
    setSaid(null);
  };
  const editOverlay = (t: string) => {
    setOverlay(t);
    if (t === (ov?.text ?? "")) unsaved.delete(overlayKey(row.key));
    else unsaved.set(overlayKey(row.key), t);
    onChanged(textChanged || t !== (ov?.text ?? ""));
    onLine(lineFor(text, t, checked));
    setSaid(null);
  };
  // The Triggers group follows what's typed, and ends with Add a trigger where the file can be edited (W29-R06).
  const workflow = ["workflow", "reusable"].includes(h.kind);
  const kiln = h.dialect === "kiln";
  const editable = p.strip.verbs.some((v) => v.verb === "save_draft" && v.enabled);
  // kiln's own triggers of a GitHub workflow come from its overlay, each saying so.
  const overlaid = ov ? (triggerLines(overlay, true) ?? []).filter((l) => l.whose === "kiln").map((l) => ({ ...l, note: l.note ? `${l.note} · in its overlay` : "in its overlay" })) : [];
  const inOverlay = (event: string) => overlaid.some((l) => eventKey(l) === event);
  const written = workflow && (changed || overlaid.length) ? triggerLines(text, kiln) : null;
  const typed = written ? [...written, ...overlaid] : null;
  // A job or use typed in is a line of its own, marked new; one taken out is gone (pl-3).
  const typedAll = workflow && textChanged ? typedGroups(text, p.groups) : null;
  const added = typedAll ? addedOf(typedAll) : [];
  // What the strip says it adds: its new jobs, else its new uses (pl-3's `Adds helm-lint`); and what its check found.
  const adds = added.some((l) => l.kind === "job") ? added.filter((l) => l.kind === "job") : added.filter((l) => l.kind === "use");
  const checks = checked?.text === text ? checksWords(checked) : null;
  // W29-R04, pl-7/8: implicit workflow defaults need no Settings group. An override, an off
  // workflow or sharing remains explicit; the read model still carries all the default facts.
  const groups = (typedAll ?? p.groups)
    .filter(
      (g) =>
        !(
          workflow &&
          g.name === "Settings" &&
          g.lines.every(
            (l) => (l.key === "settings.on" && l.name === "Turned on" && l.value === "runs on its triggers") || (l.key === "settings.machines" && l.value === "bare metal unless a job says"),
          )
        ),
    )
    .map((g) => (g.name === "Triggers" && workflow ? { ...g, lines: [...(typed ?? g.lines), ...(editable ? [ADD_TRIGGER] : [])] } : g));
  const lines = groups.flatMap((g) => g.lines);
  const at = picked && lines.some((l) => l.key === picked) ? picked : firstPick(p);
  const line = lines.find((l) => l.key === at) ?? null;
  const groupOf = (k: Key) => groups.find((g) => g.lines.some((l) => l.key === k))?.name ?? "";
  // The plan is for one of its triggers' events (W29-R07): a picked trigger's, else the one "plan for:" picks, else its first.
  const triggers = (groups.find((g) => g.name === "Triggers")?.lines ?? []).filter((l) => l.kind === "trigger");
  // What "plan for:" lists: the events the preview plans without a payload (pl-8: not a webhook's or a comment's).
  const plannable = triggers.filter((t) => !UNPLANNED.has(eventKey(t)));
  const [show, setShow] = React.useState<Shown>("all");
  const [planFor, setPlanFor] = React.useState<string | null>(null);
  const event = line?.kind === "trigger" ? eventKey(line) : planFor && plannable.some((t) => eventKey(t) === planFor) ? planFor : plannable[0] ? eventKey(plannable[0]) : null;
  // R04's pick words (pl-2): on a published workflow with nothing typed and no trigger picked; the lines each narrows away are dimmed in place.
  const picking = workflow && !h.draft && !changed && line?.kind !== "trigger";
  const [narrowed, setNarrowed] = React.useState("all");
  const kept = picking && narrowed !== "all" ? new Set(p.strip.narrow?.[narrowed] ?? []) : null;
  const narrowTo = (w: string) => {
    setNarrowed(w);
    // Narrowed to nothing, the strip says why every line is dimmed.
    const none = w !== "all" && !(p.strip.narrow?.[w] ?? []).length;
    setSaid(none ? { text: w === "failed last run" ? "Nothing failed in its last run." : "No open draft changes it.", refused: false } : null);
  };
  const pickEvent = (e: string) => {
    setPlanFor(e);
    if (line?.kind === "trigger") setPicked(`on.${e}`);
    setSaid(null);
  };
  // What its plan is set against: a draft's published file (none for a new one), or the published text typed over.
  const published = useSuspenseResource(
    `ship/pipeline-base/${repo.repo}/${row.path}/${draft ?? ""}`,
    (s) => (draft ? shipApi.pipeline(repo.repo, row.path, {}, s).catch(() => null) : Promise.resolve(null)),
    {
      on: PIPELINE_EVENTS,
    },
  ).data;
  const ask: PlanAsk = {
    event,
    words: triggers.find((t) => eventKey(t) === event) ? eventWords(triggers.find((t) => eventKey(t) === event) as ShipPipelineLine) : null,
    base: draft ? (published?.detail.text ?? null) : changed ? p.detail.text : null,
    overlay: ov ? overlay : null,
    baseOverlay: draft ? (published?.detail.overlay?.text ?? null) : (ov?.text ?? null),
  };
  // Who gets a tag: a tag is the whole repository's (W29-R21).
  const sharing = (() => {
    const published = repo.rows.filter((r) => !r.draft);
    const actions = published.filter((r) => !["workflow", "reusable"].includes(r.kind)).length;
    const calls = published.filter((r) => r.kind === "reusable").length;
    const n = (k: number, one: string, many: string) => `${k === 1 ? "its" : `its ${k}`} ${k === 1 ? one : many}`;
    return [actions ? n(actions, "action", "actions") : "", calls ? n(calls, "workflow", "workflows") : ""].filter(Boolean).join(" and ") || "its files";
  })();
  // W29-R04, pl-2/3: a private published workflow is relative to its repository's heading.
  // Shared resources and saved drafts keep the repository, distinguishing publisher and consumer (pl-5/7).
  const qualified = !h.dialect || h.kind === "reusable" || Boolean(h.draft) || repo.access !== "none";
  const where = (
    h.dialect
      ? [qualified ? repo.repo : null, h.path, changed && !h.draft ? null : `${DIALECT_WORD[h.dialect] ?? h.dialect} dialect`, changed && !h.draft ? `from ${h.ref}` : h.ref, h.commit.slice(0, 7)]
      : [repo.repo, h.path, `${(KIND_WORD[h.kind] ?? h.kind).toLowerCase()} action`]
  )
    .filter(Boolean)
    .join(" · ");
  const act = async (body: Omit<ShipPipelineAct, "repo" | "path">, done: React.ReactNode): Promise<boolean> => {
    setBusy(true);
    setRelease(null);
    try {
      await shipApi.pipelinesAct({ repo: repo.repo, path: h.path, ...body });
      setSaid({ text: done, refused: false });
      return true;
    } catch (e) {
      setSaid({ text: errorMessage(e), refused: true });
      return false;
    } finally {
      setBusy(false);
      pane.reload();
      invalidate("ship/pipelines");
    }
  };
  // A draft's verbs name it by its id: kiln's update draft is the repository's, not an author's (W29-R22).
  const draftId = h.draft ? { draft: h.draft.id } : {};
  // Release: its tag, moving its major's floating tag where the repository has one (W29-R21); a
  // refusal over failing checks asks again, as Release anyway.
  const floating = (tag: string) => p.groups.find((g) => g.name === "Versions")?.lines.some((l) => l.key === `versions.${tag.split(".")[0] ?? ""}`) ?? false;
  const releaseNow = async () => {
    if (!release) return;
    setBusy(true);
    try {
      await shipApi.pipelinesAct({
        repo: repo.repo,
        path: h.path,
        verb: "release",
        sha: h.commit,
        tag: release.tag,
        ...(floating(release.tag) ? { major: true } : {}),
        ...(release.confirm ? { confirm: true } : {}),
      });
      setRelease(null);
      setSaid({
        text: (
          <>
            Released <b>{release.tag}</b> at {h.commit.slice(0, 7)}: {sharing} have it.
          </>
        ),
        refused: false,
      });
    } catch (e) {
      if (e instanceof ApiError && e.status === 409 && !release.confirm) setRelease({ ...release, confirm: true, why: e.message });
      else {
        setRelease(null);
        setSaid({ text: errorMessage(e), refused: true });
      }
    } finally {
      setBusy(false);
      pane.reload();
      invalidate("ship/pipelines");
    }
  };
  // Run runs what's saved: a published workflow by hand (workflow_dispatch, with its inputs), a draft as the event "plan for:" picks (W29-R14).
  const runEvent = h.draft ? event : "workflow_dispatch";
  const runInputs = runEvent === "workflow_dispatch" ? manualInputs(p.detail.text) : [];
  const runNow = (values: Record<string, string>) => {
    const missing = runInputs.find((i) => i.required && !values[i.name]);
    if (missing) return setSaid({ text: `${missing.name} is needed to run it`, refused: true });
    const inputs = Object.fromEntries(
      runInputs.map((i) => [i.name, i.type === "boolean" ? values[i.name] === "true" : i.type === "number" && values[i.name] ? Number(values[i.name]) : (values[i.name] ?? "")]),
    );
    setAsking(null);
    void act(
      { verb: "run", ...draftId, ...(h.draft && event ? { event } : {}), ...(runInputs.length ? { inputs } : {}) },
      <>
        Running <b>{h.title}</b> by hand{runInputs.length ? ` with ${runInputs.map((i) => `${i.name} ${String(inputs[i.name])}`).join(", ")}` : ""}:{" "}
        <Link to={`/ship/${repo.repo}/${h.commit}`}>its jobs</Link> show as it starts.
      </>,
    );
  };
  // Save draft, and Lock it (W29-R25): the file as typed saved as a draft, whose locks take what it uses. Refused, the text stays as typed and the strip says why.
  const saveAs = (verb: "save_draft" | "lock", done: React.ReactNode) => {
    // A GitHub workflow's overlay goes in the same draft where it isn't what its trunk has (W29-R06).
    const trunkOverlay = draft ? (published?.detail.overlay?.text ?? "") : (ov?.text ?? "");
    const files = ov && overlay !== trunkOverlay ? [{ path: ov.path, text: overlay }] : [];
    void act({ verb, content: text, ...(files.length ? { files } : {}) }, done).then((ok) => {
      if (!ok) return;
      // A draft's pane holds what's saved now; a published file's reads as published again, its draft a row of its own.
      if (!draft) {
        setText(p.detail.text);
        setOverlay(ov?.text ?? "");
      }
      unsaved.delete(row.key);
      unsaved.delete(overlayKey(row.key));
      onChanged(false);
    });
  };
  const onVerb = (v: ShipPipelineVerb) => {
    switch (v.verb) {
      case "run":
        if (runInputs.length) return setAsking(Object.fromEntries(runInputs.map((i) => [i.name, i.default ?? (i.type === "boolean" ? "false" : (i.options[0] ?? ""))])));
        return runNow({});
      case "save_draft":
        return saveAs(
          "save_draft",
          <>
            Saved <b>{h.title}</b> as a draft: unpublished until it lands.
          </>,
        );
      case "ready":
        return void act(
          { verb: "ready", ...draftId },
          <>
            Asked for <b>{h.title}</b>&apos;s review.
          </>,
        );
      case "discard":
        return void act(
          { verb: "discard", ...draftId },
          <>
            Discarded the draft of <b>{h.title}</b>.
          </>,
        );
      case "release":
        return setRelease({ verb: v, tag: v.tags?.[0] ?? "", confirm: false, why: null });
      default:
        return setSaid({ text: `${v.label} isn't one this page knows yet`, refused: true });
    }
  };
  const say: React.ReactNode = asking ? (
    <>
      Run <b>{h.title}</b> by hand, with its inputs:
    </>
  ) : release ? (
    release.why ? (
      <>
        {release.why} Release <b>{release.tag}</b> anyway?
      </>
    ) : (
      <>
        Release <b>{release.tag}</b> at {h.commit.slice(0, 7)}, {h.ref}&apos;s tip? The tag is {repo.repo.split("/").pop()}&apos;s: {sharing} all get {release.tag}.
      </>
    )
  ) : said ? (
    said.text
  ) : line?.kind === "add" ? (
    <>
      <b>Add a trigger</b> · pick what starts it; its filters open under it and the YAML gains its lines, as a draft
    </>
  ) : changed ? (
    // What it adds (its jobs, else its uses), what its checks found, and that it isn't saved (pl-3).
    <>
      {adds.length ? (
        <>
          Adds{" "}
          {adds.map((l, i) => (
            <React.Fragment key={l.key}>
              {i ? ", " : ""}
              <b>{l.kind === "use" ? shortUse(l.name) : l.name}</b>
            </React.Fragment>
          ))}{" "}
          ·{" "}
        </>
      ) : null}
      {checks ? `${checks} · ` : ""}
      {adds.length || checks ? "not saved yet" : "Not saved yet"} · a draft runs on no trigger until it lands
    </>
  ) : line && (picked !== null || (h.draft?.state !== "update" && !p.lock_review)) ? (
    <PickedSaid line={line} group={groupOf(line.key)} pane={p} text={text} ask={ask} />
  ) : (
    // Before anything's picked, kiln's update draft says what it is (pl-7), and so does a lock change (W29-R25).
    <Strong text={p.strip.said.join(" · ")} words={p.strip.strong ?? []} />
  );
  // A verb that doesn't apply is dimmed, never hidden; picking it says why (W29-R04).
  const whyNot = (v: ShipPipelineVerb): string | null =>
    !v.enabled ? (v.why_not ?? `${v.label} doesn't apply here`) : v.verb === "save_draft" && !changed ? "Nothing to save yet: typing in its YAML starts a draft (W29-R05)" : null;
  const verbs: StripVerb[] = asking
    ? [
        { key: "keep", label: "Keep", enabled: true, keep: true, run: () => setAsking(null) },
        { key: "run", label: "Run", enabled: !busy, go: true, run: () => runNow(asking) },
      ]
    : release
      ? [
          { key: "keep", label: "Keep", enabled: true, keep: true, run: () => setRelease(null) },
          { key: "release", label: release.confirm ? "Release anyway" : "Release", enabled: !busy && Boolean(release.tag), go: release.confirm ? true : "ask", run: () => void releaseNow() },
        ]
      : p.strip.verbs.map((v) => {
          const why = whyNot(v);
          return { key: v.verb, label: v.label, enabled: !why && !busy, dim: Boolean(why), run: () => (why ? setSaid({ text: why, refused: true }) : onVerb(v)) };
        });
  const pick = (k: Key) => {
    setPicked(k);
    setSaid(null);
    setRelease(null);
    setAsking(null);
  };
  // What the detail edits the file with: a change made (or refused, saying why), a kind added and picked.
  const refusal = (e: unknown) => setSaid({ text: e instanceof EditRefused ? e.message : errorMessage(e), refused: true });
  const tools: Tools = {
    kiln,
    show,
    overlaid: ov !== null,
    written: new Set([...triggerKeys(text, kiln).values(), ...(ov ? triggerKeys(overlay, true).values() : [])]),
    change: (make) => {
      try {
        edit(make(text));
      } catch (e) {
        refusal(e);
      }
    },
    // A GitHub workflow keeps its cron alone; kiln's keys for it go in its overlay, on the entry with its cron.
    setSchedule: (i, cron, field, v) => {
      try {
        if (ov && !kiln) editOverlay(setOverlayScheduleField(overlay, h.path, cron, field, v));
        else edit(setScheduleField(text, i, field, v));
      } catch (e) {
        refusal(e);
      }
    },
    say: (why) => setSaid({ text: why, refused: true }),
    checked: (c) => {
      setChecked(c);
      onLine(lineFor(c.text, overlay, c));
    },
    pick: (k) => pick(k),
    lock: (uses) =>
      saveAs(
        "lock",
        <>
          Locked <b>{uses ?? h.title}</b> at {uses ? "its commit" : "the commits it uses"} in a draft: unpublished until it lands.
        </>,
      ),
    accept: () => {
      const r = p.lock_review;
      if (!r || !h.draft) return;
      void act(
        { verb: "accept", draft: h.draft.id, head: h.commit, evidence_sha256: r.evidence_sha256 },
        <>
          Accepted the new code of <b>{Object.keys(r.differences).join(", ")}</b> for {repo.repo.split("/").pop()}: it runs in this draft, and on its trunk once it lands.
        </>,
      );
    },
    addKind: (k) => {
      const keys = kiln ? k.kiln : k.gha;
      try {
        if (keys) edit(addTrigger(text, keys, k.first(kiln)));
        // kiln's own, in a GitHub workflow: written to its overlay.
        else if (ov) editOverlay(addOverlayTrigger(overlay, h.path, k.kiln, k.first(true)));
        else return setSaid({ text: overlayWhy(h.path), refused: true });
        const first = (keys ?? k.kiln)[0] ?? "";
        setPicked(`on.${kiln && first === "manual" ? "workflow_dispatch" : first}`);
      } catch (e) {
        refusal(e);
      }
    },
  };
  // A trigger the overlay holds is edited there: its filters and its YAML are the overlay's, its plan the workflow's with it.
  const inOwn = line?.kind === "trigger" && ov !== null && inOverlay(eventKey(line));
  // W29-R06: schedule options of a GitHub workflow always live in its overlay. Show its cron
  // there as an editing starting point; merely opening this view saves and changes nothing.
  const scheduleOwn = line?.key === "on.schedule" && ov !== null;
  const ownText = scheduleOwn ? scheduleOverlay(overlay, h.path, text) : overlay;
  const own: OwnFile | null =
    (inOwn || scheduleOwn) && ov
      ? {
          path: ov.path,
          text: ownText,
          onText: editOverlay,
          tools: {
            ...tools,
            kiln: true,
            change: (make) => {
              try {
                editOverlay(make(ownText));
              } catch (e) {
                refusal(e);
              }
            },
          },
        }
      : null;
  const detail = (
    <Detail
      key={`${at ?? "file"}|${event ?? ""}`}
      pane={p}
      line={line}
      text={text}
      onText={edit}
      own={own}
      editable={editable}
      ask={ask}
      tools={tools}
      rows={phone && workflow ? PHONE_ROWS : SIDE_ROWS}
    />
  );
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
          <div className="jp-say" data-tone={release ? (release.why ? "ask" : undefined) : said?.refused ? "warn" : undefined}>
            <div className="jp-said" aria-live="polite">
              {say}
            </div>
            {release ? (
              <div className="jp-q">
                <span>as:</span>
                {(release.verb.tags ?? []).map((t, i) => (
                  <React.Fragment key={t}>
                    {i ? <span aria-hidden>·</span> : null}
                    <button type="button" aria-pressed={release.tag === t} onClick={() => setRelease({ ...release, tag: t, confirm: false, why: null })}>
                      {t}
                    </button>
                  </React.Fragment>
                ))}
              </div>
            ) : asking ? (
              <RunInputs inputs={runInputs} values={asking} onSet={(name, v) => setAsking({ ...asking, [name]: v })} />
            ) : line?.kind === "add" ? (
              <div className="jp-q">
                <span>show:</span>
                {(["all", "github", "kiln"] as const).map((w, i) => (
                  <React.Fragment key={w}>
                    {i ? <span aria-hidden>·</span> : null}
                    <button type="button" aria-pressed={show === w} onClick={() => setShow(w)}>
                      {w === "all" ? "all" : w === "github" ? "GitHub's" : "kiln's"}
                    </button>
                  </React.Fragment>
                ))}
              </div>
            ) : workflow && plannable.length && !picking ? (
              <div className="jp-q">
                <span>plan for:</span>
                {plannable.map((t, i) => (
                  <React.Fragment key={t.key}>
                    {i ? <span aria-hidden>·</span> : null}
                    <button type="button" aria-pressed={eventKey(t) === event} onClick={() => pickEvent(eventKey(t))}>
                      {eventWords(t)}
                    </button>
                  </React.Fragment>
                ))}
              </div>
            ) : (
              <div className="jp-q">
                <span>pick:</span>
                {p.strip.pick.map((w, i) => (
                  <React.Fragment key={w}>
                    {i ? <span aria-hidden>·</span> : null}
                    <button type="button" aria-pressed={w === (kept ? narrowed : "all")} onClick={() => narrowTo(w)}>
                      {w}
                    </button>
                  </React.Fragment>
                ))}
              </div>
            )}
          </div>
          <div className="jp-verbs ppl-verbs" data-armed={release ? true : undefined}>
            {verbs.map((v) => (
              <button key={v.key} type="button" aria-disabled={!v.enabled || undefined} data-dim={v.dim || undefined} data-keep={v.keep || undefined} data-go={v.go || undefined} onClick={v.run}>
                {v.label}
              </button>
            ))}
          </div>
        </div>
        <div className="jp-body ppl-body">
          <ol className="jp-list ppl-list" aria-label={`${h.title}'s parts`}>
            {groups.map((g) => (
              <React.Fragment key={g.name}>
                <li className="jp-g" data-group={g.name}>
                  <span>{g.name}</span>
                  <span>{noteOf(g, p)}</span>
                </li>
                {g.lines.map((l) => (
                  <React.Fragment key={l.key}>
                    <PaneLine line={l} at={at === l.key} dim={kept !== null && !kept.has(l.key)} onLine={pick} />
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

/** Add a trigger's line, last in Triggers where the file can be edited (W29-R06). */
const ADD_TRIGGER: ShipPipelineLine = { key: "add-trigger", kind: "add", name: "Add a trigger", value: `${TRIGGER_KINDS.length} kinds` };

/** Which kinds Add a trigger lists: all, or what GitHub's or kiln's dialect says. */
type Shown = "all" | "github" | "kiln";

/** What the detail edits the file with (W29-R06): a change made in its text, a schedule's field set, a kind added, why something can't be. */
interface Tools {
  kiln: boolean;
  show: Shown;
  /** A GitHub workflow with its overlay: kiln's own kinds are added there. */
  overlaid: boolean;
  /** The trigger keys the file and its overlay have. */
  written: Set<string>;
  change: (make: (text: string) => string) => void;
  setSchedule: (index: number, cron: string, field: string, value: string | boolean | null) => void;
  addKind: (k: TriggerKind) => void;
  say: (why: string) => void;
  /** What the pane's own file's check found, as it's checked. */
  checked: (c: Checked) => void;
  /** Picks a line, as picking it in the list does. */
  pick: (k: Key) => void;
  /** W29-R25's "lock it": the file as typed saved with its locks in a draft, `uses` the one asked for. */
  lock: (uses: string | null) => void;
  /** A person's word on a lock-change draft's upstream code, as its head and evidence stand (W29-R25). */
  accept: () => void;
}

/** What the YAML view's check found of the pane's own file (pl-3): the text it checked, its problems, and its plan's jobs against what's published (none before it's typed in). */
interface Checked {
  text: string;
  errors: number;
  warnings: number;
  jobs: { now: number; was: number | null } | null;
}

/** A check's findings in the strip's words (pl-3): `checks clean`, `checks clean but for 1 warning`, `checks find 2 errors and 1 warning`. */
function checksWords(c: Checked): string {
  const n = (k: number, one: string) => `${k} ${one}${k === 1 ? "" : "s"}`;
  if (c.errors) return `checks find ${n(c.errors, "error")}${c.warnings ? ` and ${n(c.warnings, "warning")}` : ""}`;
  return c.warnings ? `checks clean but for ${n(c.warnings, "warning")}` : "checks clean";
}

/** The file a picked line lives in where it isn't the pane's own: a GitHub workflow's overlay, for kiln's own triggers (W29-R06). */
interface OwnFile {
  path: string;
  text: string;
  onText: (t: string) => void;
  tools: Tools;
}

/** A GitHub schedule's overlay editing text, with its cron present even before any kiln options are set (W29-R06). Pure: displaying it makes no edit. */
function scheduleOverlay(overlay: string, path: string, text: string): string {
  try {
    const doc = parseYaml(text) as { on?: { schedule?: { cron?: unknown }[] } } | null;
    const schedules = doc?.on?.schedule;
    return Array.isArray(schedules) ? schedules.reduce((o, s) => (typeof s.cron === "string" ? setOverlayScheduleField(o, path, s.cron, "cron", s.cron) : o), overlay) : overlay;
  } catch {
    return overlay;
  }
}

/** A workflow file's stem (`ci` of `.github/workflows/ci.yml`), as its overlay is named. */
const stemOf = (path: string) => (path.split("/").pop() ?? path).replace(/\.ya?ml$/, "");

/** Why a kiln-only trigger isn't added to a GitHub workflow whose overlay the pane wasn't given (W29-R06: it goes there). */
const overlayWhy = (path: string) => `kiln's own: it goes in .kiln/overlays/${stemOf(path)}.yaml, so the workflow keeps working on GitHub, and this pane wasn't given that file`;

/** Add a trigger (W29-R06): the 18 kinds in six groups, each GitHub's, kiln's or both; picking one adds its lines, a draft, and opens its filters. */
function AddTrigger({ pane, tools }: { pane: ShipPipelinePane; tools: Tools }) {
  const shown = TRIGGER_KINDS.filter((k) => tools.show === "all" || k.whose === "both" || k.whose === tools.show);
  return (
    <div className="ppl-kinds" aria-label="Add a trigger">
      {TRIGGER_GROUPS.map((g) => {
        const kinds = shown.filter((k) => k.group === g);
        if (!kinds.length) return null;
        return (
          <React.Fragment key={g}>
            <div className="jp-g">
              <span>{g}</span>
              <span />
            </div>
            <ul className="ppl-lines">
              {kinds.map((k) => {
                // In a GitHub workflow, kiln's own kinds go in its overlay.
                const keys = (tools.kiln ? k.kiln : k.gha) ?? (tools.overlaid ? k.kiln : null);
                const why = !keys ? overlayWhy(pane.head.path) : null;
                // A kind the file has already is as plain as the rest (pl-9): picking it picks its trigger's line.
                const had = keys?.find((key) => tools.written.has(key));
                const go = () => (why ? tools.say(why) : had ? tools.pick(`on.${had === "manual" ? "workflow_dispatch" : had}`) : tools.addKind(k));
                return (
                  <li key={k.id} data-kind={k.id}>
                    <button type="button" className="ppl-kind" aria-disabled={why ? true : undefined} onClick={go}>
                      <StatusGlyph state="none" size={12} />
                      <span className="n">
                        <b>{k.words}</b> · {k.say}
                      </span>
                      <span className="v">{k.whose === "both" ? "both" : k.whose === "kiln" ? "kiln" : "GitHub"}</span>
                    </button>
                  </li>
                );
              })}
            </ul>
          </React.Fragment>
        );
      })}
    </div>
  );
}

/** The filters each event takes (W03, W29-R06), offered where a trigger has none of them yet. */
const FILTERS: Record<string, string[]> = {
  push: ["branches", "branches-ignore", "tags", "tags-ignore", "paths", "paths-ignore"],
  pull_request: ["types", "branches", "branches-ignore", "paths", "paths-ignore"],
  pull_request_review: ["types"],
  review: ["states"],
  merge_group: ["types"],
  workflow_run: ["workflows", "types", "branches"],
  pipeline: ["name", "conclusion", "branches"],
  release: ["types"],
  registry_package: ["types"],
  repository_dispatch: ["types"],
  diff: ["branches", "paths", "paths-ignore", "drafts", "authors"],
  land: ["branches"],
  deploy: ["app", "environment", "states"],
  webhook: ["hook", "filter"],
  comment: ["mentions", "commands"],
  agent: ["agents"],
};
/** Filters that are lists: written as words split by commas. */
const LISTS = new Set(["branches", "branches-ignore", "tags", "tags-ignore", "paths", "paths-ignore", "types", "workflows", "conclusion", "states", "commands", "mentions", "agents", "authors"]);
/** Filters that are switches, off until set (W03-R09's `drafts`). */
const SWITCHES = new Set(["drafts"]);

/** A trigger's value in the text, by its key (a mapping, or nothing under it). */
function triggerValue(text: string, key: string): Record<string, unknown> {
  try {
    const on = (parseYaml(text) as { on?: unknown } | null)?.on;
    const v = on && typeof on === "object" && !Array.isArray(on) ? (on as Record<string, unknown>)[key] : null;
    return v && typeof v === "object" && !Array.isArray(v) ? (v as Record<string, unknown>) : {};
  } catch {
    return {};
  }
}

/** Words split by commas, as a list filter is typed. */
const wordsIn = (s: string) =>
  s
    .split(",")
    .map((w) => w.trim())
    .filter(Boolean);

/** A field that writes when it's left or Enter is pressed, not at every key: the YAML changes once per edit. */
function Field({ value, label, onSet }: { value: string; label: string; onSet: (v: string) => void }) {
  return (
    <input
      key={value}
      className="ppl-input"
      aria-label={label}
      defaultValue={value}
      spellCheck={false}
      onBlur={(e) => {
        if (e.currentTarget.value !== value) onSet(e.currentTarget.value);
      }}
      onKeyDown={(e) => {
        if (e.key === "Enter") e.currentTarget.blur();
      }}
    />
  );
}

function Switch({ on, label, onSet }: { on: boolean; label: string; onSet: (v: boolean) => void }) {
  return (
    <button type="button" role="switch" className="ppl-switch" aria-checked={on} aria-label={label} onClick={() => onSet(!on)}>
      {on ? "on" : "off"}
    </button>
  );
}

/** A trigger's filters, edited where they're shown (W29-R06): a list as words split by commas, a word, a switch; what's deeper in the YAML. */
function TriggerFilters({ text, line, tools }: { text: string; line: ShipPipelineLine; tools: Tools }) {
  const event = eventKey(line);
  const key = triggerKeys(text, tools.kiln).get(event) ?? event;
  const value = triggerValue(text, key);
  const fields = [...new Set([...Object.keys(value), ...(FILTERS[key] ?? FILTERS[event] ?? [])])];
  const set = (f: string, v: string[] | string | boolean | null) => tools.change((t) => setTriggerFilter(t, key, f, v));
  return (
    <div className="ppl-fields-view">
      <ul className="ppl-lines ppl-fields" aria-label={`${line.name}'s filters`}>
        {fields.map((f) => {
          const v = value[f];
          return (
            <li key={f} data-field={f}>
              <span className="n">{f}</span>
              {typeof v === "boolean" || (v === undefined && SWITCHES.has(f)) ? (
                <Switch on={v === true} label={f} onSet={(on) => set(f, on || null)} />
              ) : v !== undefined && v !== null && typeof v === "object" && !Array.isArray(v) ? (
                <span className="v">{Object.keys(v).join(", ")} · edit it in the YAML</span>
              ) : (
                <Field
                  value={Array.isArray(v) ? v.map(String).join(", ") : v === undefined || v === null ? "" : String(v)}
                  label={f}
                  onSet={(s) => set(f, LISTS.has(f) || Array.isArray(v) ? wordsIn(s) : s.trim() || null)}
                />
              )}
            </li>
          );
        })}
      </ul>
      <button type="button" className="ppl-fix ppl-out" onClick={() => tools.change((t) => removeTrigger(t, key))}>
        Take this trigger out
      </button>
    </div>
  );
}

/** What the strip says of a picked line (pl-2, pl-8): a trigger by its kind, a schedule by when it runs next; a job by where it runs, what it reserves and its last run (its note); the rest by their words. */
function PickedSaid({ line, group, pane, text, ask }: { line: ShipPipelineLine; group: string; pane: ShipPipelinePane; text: string; ask: PlanAsk }) {
  const w = wordsOf(line);
  if (line.kind === "trigger") {
    const words = [w.name, w.value].filter(Boolean).join(" · ");
    return (
      <>
        <b>{w.kind ?? line.name}</b> picked ·{" "}
        {eventKey(line) === "schedule" ? (
          <React.Suspense fallback={words}>
            <ScheduleSaid pane={pane} text={text} ask={ask} or={words} />
          </React.Suspense>
        ) : (
          words
        )}
      </>
    );
  }
  const value = line.kind === "job" ? (line.note ?? w.value) : w.value;
  return (
    <>
      <b>{w.name || line.name}</b> picked · {group.toLowerCase()}
      {value ? ` · ${value}` : ""}
    </>
  );
}

/** A picked schedule in the strip (pl-8): `every day at 03:00 Los Angeles time · next tomorrow, 11:00 UTC: daylight saving ends tonight`; its line's words where the preview can't say. */
function ScheduleSaid({ pane, text, ask, or }: { pane: ShipPipelinePane; text: string; ask: PlanAsk; or: string }) {
  const overlay = ask.overlay?.trim() ? ask.overlay : null;
  const preview = useSuspenseResource(`ci/schedule-said/${pane.repo}/${pane.head.path}/${hashOf(text)}/${hashOf(overlay ?? "")}`, () =>
    ci.preview({ repo: pane.repo, path: pane.head.path, content: text, ...(overlay ? { overlay } : {}), event: "schedule" }).catch(() => null),
  ).data;
  const s = preview?.schedules?.[0];
  if (!s || s.error) return <>{or}</>;
  const next = s.next?.[0];
  const utc = next ? new Intl.DateTimeFormat("en-US", { timeZone: "UTC", hour: "2-digit", minute: "2-digit", hourCycle: "h23" }).format(new Date(next.at * 1000)) : null;
  const shift = next?.shift ? `: daylight saving ${next.shift.ends ? "ends" : "starts"} ${shiftWhen(next.shift, s.timezone)}` : "";
  return (
    <>
      {cronWords(s.cron, s.timezone)}
      {next && utc ? ` · next ${fromNow(next.at, s.timezone)}, ${utc} UTC${shift}` : ""}
    </>
  );
}

/** When a zone's clocks change, from now (pl-8's `tonight`): the coming night's small hours are tonight, else its day. */
function shiftWhen(shift: { at: number; local: string }, tz: string | null): string {
  const day = fromNow(shift.at, tz);
  const hour = Number(shift.local.split(":")[0]);
  return day === "today" || (day === "tomorrow" && hour < 6) ? "tonight" : day;
}

const DAY_NAMES = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];

/** A cron in words, with its zone (pl-8's strip): `every day at 03:00 Los Angeles time`, `every weekday at 09:30 UTC`, `every 30 minutes`; else as written. */
function cronWords(cron: string, tz: string | null): string {
  const f = cron.trim().split(/\s+/);
  const [m, h, dom, mon, dow] = f;
  const zone = !tz || tz === "UTC" || tz === "Etc/UTC" ? "UTC" : `${(tz.split("/").pop() ?? tz).replace(/_/g, " ")} time`;
  const num = (x: string | undefined) => (x !== undefined && /^\d+$/.test(x) ? Number(x) : null);
  const pad = (n: number) => String(n).padStart(2, "0");
  const [mm, hh] = [num(m), num(h)];
  const at = mm !== null && hh !== null ? `${pad(hh)}:${pad(mm)} ${zone}` : null;
  const every = /^\*\/(\d+)$/.exec(m ?? "");
  if (f.length !== 5 || mon !== "*") return `on cron ${cron.trim()}`;
  if (at && dom === "*" && dow === "*") return `every day at ${at}`;
  if (at && dom === "*" && dow === "1-5") return `every weekday at ${at}`;
  if (at && dom === "*" && /^[0-6](,[0-6])*$/.test(dow ?? ""))
    return `every ${(dow ?? "")
      .split(",")
      .map((d) => DAY_NAMES[Number(d)])
      .join(", ")} at ${at}`;
  if (at && num(dom) !== null && dow === "*") return `on day ${dom} of each month at ${at}`;
  if (mm !== null && h === "*" && dom === "*" && dow === "*") return `every hour at :${pad(mm)}`;
  if (every && h === "*" && dom === "*" && dow === "*") return `every ${every[1]} minutes`;
  return `on cron ${cron.trim()}`;
}

/** Where `w` first stands as a word of its own in `text` (not inside another), else where it first appears; -1 where it doesn't. */
function wholeAt(text: string, w: string): number {
  const word = /\w/;
  for (let i = text.indexOf(w); i >= 0; i = text.indexOf(w, i + 1)) if (!word.test(text[i - 1] ?? " ") && !word.test(text[i + w.length] ?? " ")) return i;
  return text.indexOf(w);
}

/** Words with some set in bold, each where it first stands (an update draft's `rust-check v2.4.0`, `#88`; a change's name in its sentence). */
function Strong({ text, words }: { text: string; words: readonly string[] }) {
  const out: React.ReactNode[] = [];
  let rest = text;
  let left = words.filter(Boolean);
  for (;;) {
    const first = left
      .map((w) => ({ w, at: wholeAt(rest, w) }))
      .filter((x) => x.at >= 0)
      .sort((a, b) => a.at - b.at)[0];
    if (!first) break;
    out.push(rest.slice(0, first.at), <b key={out.length}>{first.w}</b>);
    rest = rest.slice(first.at + first.w.length);
    left = left.filter((w) => w !== first.w);
  }
  out.push(rest);
  return <>{out}</>;
}

/** A run's minute in its zone and in UTC, with daylight saving's change said: `Sun, Nov 1, 03:00 PST · 11:00 UTC (daylight saving ends at 02:00)`. */
function runWords(r: NonNullable<CiSchedule["next"]>[number], tz: string | null): string {
  const at = new Date(r.at * 1000);
  const local = new Intl.DateTimeFormat("en-US", {
    timeZone: tz ?? "UTC",
    weekday: "short",
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    hourCycle: "h23",
    timeZoneName: "short",
  }).format(at);
  const utc = new Intl.DateTimeFormat("en-US", { timeZone: "UTC", hour: "2-digit", minute: "2-digit", hourCycle: "h23" }).format(at);
  const shift = r.shift ? ` (daylight saving ${r.shift.ends ? "ends" : "starts"} at ${r.shift.local})` : "";
  return `${local}${tz && tz !== "UTC" ? ` · ${utc} UTC` : ""}${shift}`;
}

/** How far off a run is, in its zone's calendar days: today, tomorrow, in 3 days. */
function fromNow(at: number, tz: string | null): string {
  let day: (ms: number) => number;
  try {
    const f = new Intl.DateTimeFormat("en-CA", { timeZone: tz ?? "UTC", year: "numeric", month: "2-digit", day: "2-digit" });
    day = (ms) => Date.parse(f.format(ms));
  } catch {
    day = (ms) => Math.floor(ms / 86_400_000) * 86_400_000;
  }
  const days = Math.round((day(at * 1000) - day(Date.now())) / 86_400_000);
  return days <= 0 ? "today" : days === 1 ? "tomorrow" : `in ${days} days`;
}

/** A schedule's next runs, from W03's cron as the server reads them, and its options, each edited where it's shown (W29-R06, W03-R10). */
function ScheduleTimes({
  pane,
  line,
  text,
  onText,
  own,
  ask,
  tools,
  editable,
}: {
  pane: ShipPipelinePane;
  line: ShipPipelineLine;
  text: string;
  onText: (t: string) => void;
  own: OwnFile | null;
  ask: PlanAsk;
  tools: Tools;
  editable: boolean;
}) {
  const overlay = ask.overlay?.trim() ? ask.overlay : null;
  const preview = useSuspenseResource(`ci/schedules/${pane.repo}/${pane.head.path}/${hashOf(text)}/${hashOf(overlay ?? "")}`, () =>
    ci.preview({ repo: pane.repo, path: pane.head.path, content: text, ...(overlay ? { overlay } : {}), event: "schedule" }),
  ).data;
  const schedules = preview.schedules ?? [];
  const set = (i: number, field: string, v: string | boolean | null) => tools.setSchedule(i, schedules[i]?.cron ?? "", field, v);
  if (!schedules.length) return <p className="ppl-empty">{preview.why ?? "Its schedule can't be read yet: fix the YAML first."}</p>;
  return (
    <div className="ppl-times">
      {schedules.map((s, i) => (
        <React.Fragment key={`${i}-${s.cron}`}>
          <div className="jp-g">
            <span>Its next runs{schedules.length > 1 ? ` · ${s.cron}` : ""}</span>
            <span />
          </div>
          <ul className="ppl-lines" aria-label={`${s.cron}'s next runs`}>
            {s.error ? (
              <li>
                <StatusGlyph state="failed" size={12} />
                <span className="n">{s.error}</span>
              </li>
            ) : (
              (s.next ?? []).map((r) => (
                <li key={r.at}>
                  <StatusGlyph state="none" size={12} />
                  <span className="n">{runWords(r, s.timezone)}</span>
                  <span className="v">{fromNow(r.at, s.timezone)}</span>
                </li>
              ))
            )}
          </ul>
          <div className="jp-g">
            <span>Its options</span>
            <span />
          </div>
          {/* W29-R06, pl-8: options as sentences. Toggle the two booleans here; other values live in the YAML below. */}
          <ul className="ppl-lines ppl-options" aria-label={`${s.cron}'s options`}>
            <li data-field="timezone">
              <StatusGlyph state="passed" size={12} />
              <span className="n">
                <b>Time zone</b> {s.timezone ?? "UTC"}: a missing local time runs at the next minute, a repeated one once
              </span>
              <span className="v">kiln</span>
            </li>
            <li data-field="skip-if-unchanged">
              <button
                type="button"
                className="ppl-option"
                aria-label="Skip if unchanged"
                aria-pressed={s.skip_if_unchanged}
                aria-disabled={!editable || undefined}
                onClick={() => (editable ? set(i, "skip-if-unchanged", !s.skip_if_unchanged) : undefined)}
              >
                <StatusGlyph state={s.skip_if_unchanged ? "passed" : "none"} size={12} />
                <span className="n">
                  <b>Skip if unchanged</b>: {s.skip_if_unchanged ? `no run when ${pane.head.ref} hasn't moved since the last one passed` : "off: run at every scheduled time"}
                </span>
                <span className="v">kiln</span>
              </button>
            </li>
            <li data-field="catch-up">
              <button
                type="button"
                className="ppl-option"
                aria-label="Catch up"
                aria-pressed={s.catch_up}
                aria-disabled={!editable || undefined}
                onClick={() => (editable ? set(i, "catch-up", !s.catch_up) : undefined)}
              >
                <StatusGlyph state={s.catch_up ? "passed" : "none"} size={12} />
                <span className="n">
                  <b>Catch up</b> after downtime: {s.catch_up ? "on: run the latest missed time" : "off, as GitHub (missed times are dropped)"}
                </span>
                <span className="v">kiln</span>
              </button>
            </li>
            <li data-field="jitter">
              <StatusGlyph state={s.jitter ? "passed" : "none"} size={12} />
              <span className="n">
                <b>Jitter</b>: {s.jitter ?? "off"}
              </span>
              <span className="v">kiln</span>
            </li>
            <li data-field="inputs">
              <StatusGlyph state={Object.keys(s.inputs).length ? "passed" : "none"} size={12} />
              <span className="n">
                <b>Inputs</b>:{" "}
                {Object.entries(s.inputs)
                  .map(([k, v]) => `${k} = ${String(v)}`)
                  .join(", ") || "none"}
              </span>
              <span className="v">kiln</span>
            </li>
          </ul>
        </React.Fragment>
      ))}
      {editable ? (
        <CodeEditor
          value={own?.text ?? text}
          onChange={own?.onText ?? onText}
          label={`${own?.path ?? pane.head.path}, schedule options in YAML`}
          syntax="workflow"
          inline
          tint={spotOf(own?.text ?? text, line)}
        />
      ) : (
        <CodeView text={own?.text ?? text} path={own?.path ?? pane.head.path} spot={spotOf(own?.text ?? text, line)} rows={SIDE_ROWS} />
      )}
    </div>
  );
}

/** Run's inputs asked in the strip, never in a dialog (W29-R14): a choice as its words, a switch, a field; each its default first. */
function RunInputs({ inputs, values, onSet }: { inputs: ManualInput[]; values: Record<string, string>; onSet: (name: string, v: string) => void }) {
  return (
    <div className="jp-q ppl-inputs" aria-label="Its inputs">
      {inputs.map((i) => (
        <span key={i.name} className="ppl-in" data-input={i.name}>
          <span>{i.name}:</span>
          {i.type === "choice" && i.options.length ? (
            i.options.map((o, n) => (
              <React.Fragment key={o}>
                {n ? <span aria-hidden>·</span> : null}
                <button type="button" aria-pressed={values[i.name] === o} onClick={() => onSet(i.name, o)}>
                  {o}
                </button>
              </React.Fragment>
            ))
          ) : i.type === "boolean" ? (
            <Switch on={values[i.name] === "true"} label={i.name} onSet={(on) => onSet(i.name, on ? "true" : "false")} />
          ) : (
            <input
              className="ppl-input"
              aria-label={i.name}
              inputMode={i.type === "number" ? "numeric" : undefined}
              value={values[i.name] ?? ""}
              placeholder={i.required ? "needed" : ""}
              onChange={(e) => onSet(i.name, e.currentTarget.value)}
            />
          )}
        </span>
      ))}
    </div>
  );
}

/** One line: its glyph, its kind word and name, its value; picking it shows its detail. */
function PaneLine({ line, at, dim, onLine }: { line: ShipPipelineLine; at: boolean; dim: boolean; onLine: (k: Key) => void }) {
  const w = wordsOf(line);
  return (
    <li className="jp-j" data-key={line.key} data-sel={at || undefined} data-at={at || undefined} data-dim={dim || undefined} data-state={line.state}>
      <span className="jp-glyph" aria-hidden="true">
        {line.kind === "add" || (line.state === "added" && !line.value.includes("pin it?")) ? (
          <Plus size={12} />
        ) : (
          <StatusGlyph state={line.state === "added" ? "flag" : glyphOf(glyphFor(line))} size={12} />
        )}
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
function Detail({
  pane,
  line,
  text,
  onText,
  own,
  editable,
  ask,
  tools,
  rows,
}: {
  pane: ShipPipelinePane;
  line: ShipPipelineLine | null;
  text: string;
  onText: (t: string) => void;
  own: OwnFile | null;
  editable: boolean;
  ask: PlanAsk;
  tools: Tools;
  rows: number;
}) {
  const views = viewsFor(pane, line);
  const [view, setView] = React.useState(views[0] ?? "YAML");
  const shown = views.includes(view) ? view : (views[0] ?? "YAML");
  const file = own?.path ?? pane.head.path;
  // The use kiln's update draft moves names the draft and the move (pl-7's `#88 · rust-check v2.3.0 → v2.4.0`).
  const u = pane.detail.update;
  const name =
    line?.state === "changes" && line.kind === "use" && u
      ? `${pane.head.draft?.change?.name ?? "the draft"} · ${u.uses} ${u.from} → ${u.to}`
      : line?.key === "versions.head"
        ? `${line.name} · ${line.value}`
        : line?.key === "on.schedule"
          ? `schedule · ${file}`
          : line
            ? `${wordsOf(line).name || line.name} · ${file}`
            : file;
  return (
    <div className="ppl-detail">
      <div className="ppl-dhead">
        <span className="ppl-dname">
          <StatusGlyph state="running" size={12} label="Preview" className="ppl-preview" />
          {name}
        </span>
        <span className="dp-w ppl-views" role="tablist" aria-label="Views" hidden={views.length < 2}>
          {views.map((v) => (
            <button key={v} type="button" role="tab" aria-selected={v === shown} onClick={() => setView(v)}>
              {v}
            </button>
          ))}
        </span>
      </div>
      <div className="ppl-dbody" style={{ "--rows": rows } as React.CSSProperties}>
        <Loadable label={`Loading ${shown}`} fallback={<Loading label={`Loading ${shown}`} block className="ppl-slot" />}>
          <View view={shown} pane={pane} line={line} text={text} onText={onText} own={own} editable={editable} ask={ask} tools={tools} rows={rows} />
        </Loadable>
      </div>
    </div>
  );
}

function View({
  view,
  pane,
  line,
  text,
  onText,
  own,
  editable,
  ask,
  tools,
  rows,
}: {
  view: string;
  pane: ShipPipelinePane;
  line: ShipPipelineLine | null;
  text: string;
  onText: (t: string) => void;
  own: OwnFile | null;
  editable: boolean;
  ask: PlanAsk;
  tools: Tools;
  rows: number;
}) {
  const h = pane.head;
  switch (view) {
    case "YAML":
      // A trigger the overlay holds: the overlay's lines, its own Problems.
      if (own)
        return editable ? (
          <YamlView pane={pane} line={line} text={own.text} onText={own.onText} ask={ask} file={own.path} />
        ) : (
          <CodeView text={own.text} path={own.path} spot={line ? spotOf(own.text, line) : null} rows={rows} />
        );
      return editable ? (
        <YamlView pane={pane} line={line} text={text} onText={onText} ask={ask} onChecked={tools.checked} onLock={tools.lock} />
      ) : (
        <CodeView text={text} path={h.path} spot={line ? spotOf(text, line) : null} rows={rows} />
      );
    case "Flow":
      return <FlowView repo={pane.repo} text={text} onChange={onText} />;
    case "Plan":
      return <PlanView pane={pane} text={text} ask={ask} />;
    case "Try it":
      return <TryPane repo={pane.repo} path={h.path} gitRef={h.ref} text={text} blocked={text !== pane.detail.text} />;
    case "Last run":
    case "Run":
      return <Lines lines={(pane.groups.find((g) => g.name === "Runs")?.lines ?? []).filter((l) => view === "Last run" || l.key === line?.key)} empty="No run yet on its default branch." />;
    case "Add a trigger":
      return <AddTrigger pane={pane} tools={tools} />;
    case "Times":
      return line ? <ScheduleTimes pane={pane} line={line} text={text} onText={onText} own={own} ask={ask} tools={tools} editable={editable} /> : null;
    case "Filters":
      if (editable && line) return <TriggerFilters text={own?.text ?? text} line={line} tools={own?.tools ?? tools} />;
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
      return <ChangesView pane={pane} text={text} ask={ask} tools={tools} />;
    default:
      return null;
  }
}

/** What a file reads as: an action's action.yml and a workflow's YAML as a workflow (`run:` as its shell, W29-R05), the rest by its path. */
const syntaxOf = (h: ShipPipelinePane["head"]): string | undefined => (!["workflow", "reusable"].includes(h.kind) || /\.ya?ml$/i.test(h.path) ? "workflow" : resolveLanguage(null, h.path)?.id);

/** The file a pane's path names, as the validator reads it: an action's action.yml. */
const fileOf = (h: ShipPipelinePane["head"]) => (["workflow", "reusable"].includes(h.kind) || /action\.ya?ml$/.test(h.path) ? h.path : `${h.path}/action.yml`);

/** An offset of `text` at a line and column, both from 1. */
function offsetOf(text: string, at: { line: number; col: number }): number {
  let off = 0;
  for (let i = 1; i < at.line; i++) {
    const nl = text.indexOf("\n", off);
    if (nl < 0) return text.length;
    off = nl + 1;
  }
  return Math.min(text.length, off + Math.max(0, at.col - 1));
}

/**
 * The file where it's shown, edited in place (W29-R05): typing starts the draft, Not saved until
 * it's saved. Checked 300 ms after typing stops; its errors and warnings sit under it, each with its
 * fix, never in a popup.
 */
function YamlView({
  pane,
  line,
  text,
  onText,
  ask,
  file,
  onChecked,
  onLock,
}: {
  pane: ShipPipelinePane;
  line: ShipPipelineLine | null;
  text: string;
  onText: (t: string) => void;
  ask: PlanAsk;
  file?: string;
  onChecked?: (c: Checked) => void;
  onLock?: (uses: string | null) => void;
}) {
  const h = pane.head;
  const syntax = syntaxOf(h);
  const [problems, setProblems] = React.useState<CiDiagnostic[]>([]);
  // The text's plan for the strip's event, with the base's when it differs: asked as the text is checked.
  const [plan, setPlan] = React.useState<{ text: string; now: PlanJob[]; was: PlanJob[] | null } | null>(null);
  // Another file (a GitHub workflow's overlay) is checked as itself, and plans nothing of its own.
  const workflow = ["workflow", "reusable"].includes(h.kind) && !file;
  const editor = React.useRef<Editor>(null);
  const lint = async (t: string): Promise<Diagnostic[]> => {
    if (syntax !== "workflow") return [];
    try {
      const [r, now, was] = await Promise.all([
        ci.validate(file ?? fileOf(h), t, pane.repo, undefined, h.commit),
        workflow ? planOf(pane, t, ask.event, ask.overlay).catch(() => null) : null,
        workflow && ask.base !== null ? basePlanOf(pane, ask.base, ask.event, ask.baseOverlay).catch(() => null) : null,
      ]);
      if (editor.current && editor.current.state.doc.toString() !== t) return [];
      const shown = r.diagnostics.filter((d) => d.severity === "error" || d.severity === "warning");
      setProblems(shown);
      const planned = now?.plan ? { text: t, now: jobsOf(now.plan), was: ask.base === null ? null : was?.plan ? jobsOf(was.plan) : [] } : null;
      setPlan(planned);
      onChecked?.({
        text: t,
        errors: shown.filter((d) => d.severity === "error").length,
        warnings: shown.filter((d) => d.severity === "warning").length,
        jobs: planned ? { now: planned.now.length, was: planned.was?.length ?? null } : null,
      });
      return shown.map((d) => {
        const from = offsetOf(t, d.range.start);
        return { from, to: Math.max(from + 1, offsetOf(t, d.range.end)), severity: d.severity === "error" ? "error" : "warning", message: d.message };
      });
    } catch {
      if (editor.current && editor.current.state.doc.toString() !== t) return [];
      setProblems([]);
      setPlan(null);
      return [];
    }
  };
  // Under the code: a picked job's machines, and a draft's plan against the published one's (pl-2, pl-3).
  const job = line?.kind === "job" ? line.key.replace(/^jobs\./, "") : null;
  const currentPlan = plan?.text.replace(/\r\n?/g, "\n") === text.replace(/\r\n?/g, "\n") ? plan : null;
  const where = job ? (currentPlan?.now.find((j) => j.job === job || j.job === line?.name.replaceAll(" / ", "/")) ?? null) : null;
  const reserved = job ? jobResources(text, job) : null;
  const needs = where ? [...where.labels.filter((l) => !["baremetal", "bare-metal"].includes(l)), ...(reserved ? [`${reserved.replace(/, ([^,]+)$/, " and $1")} free`] : [])].join(", ") : "";
  const changes = currentPlan?.was ? planLines(currentPlan.now, currentPlan.was, h.ref) : null;
  const fix = (d: CiDiagnostic) => {
    const v = editor.current;
    if (!v || !d.fix) return;
    const t = v.state.doc.toString();
    v.dispatch({ changes: d.fix.edits.map((e) => ({ from: offsetOf(t, e.range.start), to: offsetOf(t, e.range.end), insert: e.text })) });
  };
  return (
    <div className="ppl-yaml">
      <CodeEditor
        value={text}
        onChange={onText}
        lint={lint}
        label={`${file ?? h.path}, edited in place: typing starts a draft`}
        syntax={syntax}
        inline
        tint={line ? spotOf(text, line) : null}
        onView={(v) => {
          editor.current = v;
        }}
      />
      <div className="ppl-problems">
        {problems.length ? (
          <>
            <div className="jp-g">
              <span>Problems</span>
              <span>{problems.length}</span>
            </div>
            <ul className="ppl-lines" aria-label="Problems">
              {problems.map((d, i) => (
                <li key={`${d.code}-${d.range.start.line}-${i}`} data-severity={d.severity}>
                  <StatusGlyph state={d.severity === "error" ? "failed" : "flag"} size={12} />
                  <span className="n">
                    <em>{d.code}</em>
                    line {d.range.start.line}: {d.message}
                  </span>
                  {d.fix || (d.lock && onLock) ? (
                    <span className="ppl-fixes">
                      {d.fix ? (
                        <button type="button" className="ppl-fix" onClick={() => fix(d)}>
                          {d.fix.title}
                        </button>
                      ) : null}
                      {d.lock && onLock ? (
                        <button type="button" className="ppl-fix" onClick={() => onLock(d.lock?.uses ?? null)}>
                          Lock it
                        </button>
                      ) : null}
                    </span>
                  ) : (
                    <span className="v">{d.severity}</span>
                  )}
                </li>
              ))}
            </ul>
          </>
        ) : null}
        {where && !changes ? (
          <>
            <div className="jp-g">
              <span>Where it would run</span>
              <span />
            </div>
            <ul className="ppl-lines">
              <li>
                <StatusGlyph state={where.runners.length ? "passed" : "blocked"} size={12} />
                <span className="n">{where.runners.length ? `${where.runners.join(", ")} can take it now${needs ? `: ${needs}` : ""}` : whereWords(where)}</span>
                <span className="v">{where.of ? `${where.runners.length} of ${where.of}` : ""}</span>
              </li>
            </ul>
          </>
        ) : null}
        {changes ? (
          <>
            <div className="jp-g">
              <span>Plan for {ask.words ? `a ${ask.words}` : "its event"}</span>
              <span />
            </div>
            <PlanList lines={changes} />
          </>
        ) : null}
      </div>
    </div>
  );
}

/**
 * What changed (pl-5, pl-7): an action's commits since its newest release, and what releasing its
 * head changes for those who use it; a draft's lines against what's published, and what it changes
 * for its repository.
 */
function ChangesView({ pane, text, ask, tools }: { pane: ShipPipelinePane; text: string; ask: PlanAsk; tools: Tools }) {
  const d = pane.detail;
  if (pane.lock_review) return <LockReview review={pane.lock_review} tools={tools} />;
  const next = pane.strip.verbs.find((v) => v.verb === "release")?.tags?.[0];
  if (d.since) {
    return (
      <div className="ppl-changes">
        <ul className="ppl-lines" aria-label={`Commits since ${d.since.tag}`}>
          {d.since.commits.map((c) => (
            <li key={c.sha}>
              <span className="ppl-sha">{c.sha}</span>
              <span className="n">{c.subject}</span>
              <span className="v">
                {c.author} · {since(c.at)}
              </span>
            </li>
          ))}
        </ul>
        {d.changes ? (
          <>
            <div className="jp-g">
              <span>What {next ?? "a release"} changes for those who use it</span>
              <span />
            </div>
            <Lines lines={d.changes} empty="Nothing they pass changes." />
          </>
        ) : null}
      </div>
    );
  }
  if (ask.base !== null) {
    return (
      <div className="ppl-changes">
        <ol className="ppl-code ppl-diff" aria-label="Its lines against what's published">
          {lineDiff(ask.base, text, 1).map((l, i) =>
            l.kind === "gap" ? (
              <li key={i} data-kind="gap">
                <span className="ppl-ln" aria-hidden="true">
                  ⋯
                </span>
                <code />
              </li>
            ) : (
              <li key={i} data-kind={l.kind}>
                <span className="ppl-ln">{l.now ?? l.old}</span>
                <code>{l.text || " "}</code>
              </li>
            ),
          )}
        </ol>
        {d.changes?.length ? (
          // In sentences (pl-7), with its plan against the published one's before the draft's run.
          <>
            <div className="jp-g">
              <span>
                What {d.update?.to ?? "it"} changes for {pane.repo.split("/").pop()}
              </span>
              <span />
            </div>
            <React.Suspense fallback={<Lines lines={d.changes.filter((l) => l.kind !== "upstream")} empty="" />}>
              <UpdateLines pane={pane} text={text} ask={ask} lines={d.changes.filter((l) => l.kind !== "upstream")} />
            </React.Suspense>
            {/* The upstream code's diff between its two versions (W29-R22), under its own head. */}
            {d.changes.some((l) => l.kind === "upstream") ? (
              <>
                <div className="jp-g">
                  <span>Upstream{d.update ? `, ${d.update.from} → ${d.update.to}` : ""}</span>
                  <span />
                </div>
                <Lines lines={d.changes.filter((l) => l.kind === "upstream")} empty="" />
              </>
            ) : null}
          </>
        ) : null}
      </div>
    );
  }
  return <Lines lines={pane.groups.filter((g) => g.name === "Versions").flatMap((g) => g.lines)} empty="Nothing has changed since its newest tag." />;
}

/** A changed file's side as text, or why it isn't: a link's target, a submodule's commit, or a binary's size. */
function sideOf(f: ShipCodeChange["before"]): { text: string | null; words: string } {
  if (!f) return { text: "", words: "" };
  const bytes = Uint8Array.from(atob(f.base64), (c) => c.charCodeAt(0));
  let text: string | null;
  try {
    text = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  } catch {
    text = null;
  }
  if (f.mode === 0o120000) return { text: null, words: `a link to ${text ?? "a path that isn't text"}` };
  if (f.mode === 0o160000) return { text: null, words: `a submodule at ${(text ?? "").slice(0, 7)}` };
  if (text === null || text.includes("\0")) return { text: null, words: `${bytes.length} bytes, not text` };
  return { text, words: f.mode === 0o100755 ? "executable" : "" };
}

/** What changed of a file between the accepted and the new commit, in words. */
function changeWords(c: ShipCodeChange, old: { words: string }, now: { words: string }): string {
  if (!c.before) return `new${now.words ? `, ${now.words}` : ""}`;
  if (!c.after) return "removed";
  if (old.words !== now.words) return `${old.words || "a file"} → ${now.words || "a file"}`;
  return now.words ? `changed, ${now.words}` : "changed";
}

/**
 * A lock-change draft's upstream code (W29-R25): every file that changed between the commit its
 * lock had and the one it proposes, each source under its own head, then whose word it waits on.
 * Accept stays where it is, dimmed once a person's word stands.
 */
function LockReview({ review, tools }: { review: ShipLockReview; tools: Tools }) {
  return (
    <div className="ppl-changes">
      {Object.entries(review.differences).map(([key, d]) => (
        <React.Fragment key={key}>
          <div className="jp-g">
            <span>
              Upstream code, {key}: {d.before.commit.slice(0, 7)} → {d.after.commit.slice(0, 7)}
            </span>
            <span>{d.files.length === 1 ? "1 file" : `${d.files.length} files`}</span>
          </div>
          {d.files.map((c) => {
            const [old, now] = [sideOf(c.before), sideOf(c.after)];
            return (
              <React.Fragment key={c.path}>
                <ul className="ppl-lines">
                  <li>
                    <StatusGlyph state={c.after ? "changes" : "failed"} size={12} />
                    <span className="n ppl-sha">{c.path}</span>
                    <span className="v">{changeWords(c, old, now)}</span>
                  </li>
                </ul>
                {old.text !== null && now.text !== null ? (
                  <ol className="ppl-code ppl-diff" aria-label={`${c.path}, its lines at each commit`}>
                    {lineDiff(old.text, now.text, 2).map((l, i) =>
                      l.kind === "gap" ? (
                        <li key={i} data-kind="gap">
                          <span className="ppl-ln" aria-hidden="true">
                            ⋯
                          </span>
                          <code />
                        </li>
                      ) : (
                        <li key={i} data-kind={l.kind}>
                          <span className="ppl-ln">{l.now ?? l.old}</span>
                          <code>{l.text || " "}</code>
                        </li>
                      ),
                    )}
                  </ol>
                ) : null}
              </React.Fragment>
            );
          })}
        </React.Fragment>
      ))}
      <ul className="ppl-lines" aria-label="Its word">
        <li>
          <StatusGlyph state={review.accepted_by ? "passed" : "flag"} size={12} />
          <span className="n">
            {review.accepted_by ? `Accepted by ${byWord(review.accepted_by)}: it runs in this draft, and on its trunk once it lands` : "It runs nowhere until a person accepts this code"}
          </span>
          <button type="button" className="ppl-fix" disabled={Boolean(review.accepted_by)} data-dim={review.accepted_by ? true : undefined} onClick={tools.accept}>
            Accept
          </button>
        </li>
      </ul>
    </div>
  );
}

/** An update's lines (pl-7), its plan against the published one's for the strip's event put before its draft's run: `the plan for a push to main: the same 2 jobs, 4 legs · same`. */
function UpdateLines({ pane, text, ask, lines }: { pane: ShipPipelinePane; text: string; ask: PlanAsk; lines: ShipPipelineLine[] }) {
  const diff = useSuspenseResource(`ship/update-plan/${pane.repo}/${pane.head.path}/${ask.event ?? ""}/${hashOf(text)}/${hashOf(ask.base ?? "")}`, () =>
    Promise.all([planOf(pane, text, ask.event, ask.overlay), basePlanOf(pane, ask.base ?? "", ask.event, ask.baseOverlay)])
      .then(([now, was]) => (now.plan && was.plan ? planDiff(jobsOf(was.plan), jobsOf(now.plan)) : null))
      .catch(() => null),
  ).data;
  const at = lines.findIndex((l) => l.key === "changes.run");
  const plan = diff ? planSaid(diff, ask.words ? `the plan for a ${ask.words}` : "its plan") : null;
  return <Lines lines={plan ? (at < 0 ? [...lines, plan] : [...lines.slice(0, at), plan, ...lines.slice(at)]) : lines} empty="" />;
}

/** A draft's plan against the published one's in a line (pl-7): the same jobs and legs, or how many now and before. */
function planSaid(d: PlanDiff, of: string): ShipPipelineLine {
  const n = (k: number, one: string) => `${k} ${one}${k === 1 ? "" : "s"}`;
  const legsOf = (js: readonly PlanJob[]) => js.reduce((x, j) => x + j.legs, 0);
  if (!d.added.length && !d.removed.length && !d.changed.length)
    return { key: "changes.plan", kind: "note", name: `${of}: the same ${n(d.same.jobs, "job")}, ${n(d.same.legs, "leg")}`, value: "same", state: "passed" };
  const jobs = [d.same.jobs + d.added.length + d.changed.length, d.same.jobs + d.removed.length + d.changed.length];
  const legs = [d.same.legs + legsOf(d.added) + legsOf(d.changed.map((c) => c.now)), d.same.legs + legsOf(d.removed) + legsOf(d.changed.map((c) => c.was))];
  return { key: "changes.plan", kind: "note", name: `${of}: ${n(jobs[0] ?? 0, "job")} (was ${jobs[1] ?? 0}), ${n(legs[0] ?? 0, "leg")} (was ${legs[1] ?? 0})`, value: "changes", state: "changes" };
}

/** Lines as the detail lists them: a glyph, the words, a value. */
function Lines({ lines, empty }: { lines: ShipPipelineLine[]; empty: string }) {
  if (!lines.length) return <p className="ppl-empty">{empty}</p>;
  return (
    <ul className="ppl-lines">
      {lines.map((l) => {
        const w = wordsOf(l);
        // A change's sentence sets what changed in bold (pl-5, pl-7): its key's last part.
        const strong = l.kind === "change" ? l.key.split(".").slice(2).join(".") : "";
        return (
          <li key={l.key}>
            {/* What's added is a +, as the list and the plan draw it (pl-5, pl-7). */}
            {l.state === "added" ? <Plus size={12} aria-hidden="true" /> : <StatusGlyph state={glyphOf(glyphFor(l))} size={12} />}
            <span className="n">
              {w.kind ? <em>{w.kind}</em> : null}
              {strong ? <Strong text={w.name} words={[strong]} /> : w.name}
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
          <em>{sourceOf(line.name, line.value.split(" · ")[0])}</em>
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

/** The plan for the strip's event (W04-R29's preview of the text): each job, its legs and where they'd run; a draft's against what's published. */
function PlanView({ pane, text, ask }: { pane: ShipPipelinePane; text: string; ask: PlanAsk }) {
  const key = `${pane.repo}/${pane.head.path}/${ask.event ?? ""}`;
  const now = useSuspenseResource(`ci/plan-preview/${key}/${hashOf(text)}/${hashOf(ask.overlay ?? "")}`, () => planOf(pane, text, ask.event, ask.overlay)).data;
  const was = useSuspenseResource(`ci/plan-base/${key}/${ask.base === null ? "-" : hashOf(ask.base)}/${hashOf(ask.baseOverlay ?? "")}`, () =>
    ask.base === null ? Promise.resolve(null) : basePlanOf(pane, ask.base, ask.event, ask.baseOverlay),
  ).data;
  if (!now.plan) return <p className="ppl-empty">{now.why ?? "It plans nothing for this event."}</p>;
  return <PlanList lines={planLines(jobsOf(now.plan), ask.base === null ? null : was?.plan ? jobsOf(was.plan) : [], pane.head.ref)} label={`Its plan for ${ask.words ?? "its event"}`} />;
}

/** A plan's lines: a job new, gone, changed or as it was, each with its words and its count. */
function PlanList({ lines, label }: { lines: PlanLine[]; label?: string }) {
  return (
    <ul className="ppl-lines" aria-label={label}>
      {lines.map((l) => (
        <li key={l.key} data-mark={l.mark}>
          {l.mark === "added" ? (
            <Plus size={12} aria-hidden="true" />
          ) : l.mark === "removed" ? (
            <Minus size={12} aria-hidden="true" />
          ) : (
            <StatusGlyph state={l.mark === "changed" ? "changes" : "passed"} size={12} />
          )}
          <span className="n">{l.text}</span>
          <span className="v">{l.value}</span>
        </li>
      ))}
    </ul>
  );
}

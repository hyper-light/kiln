/**
 * A notebook (docs/observe.md §11.1): its cells over its time, Markdown and graphs (drawn as
 * dashboards draw Datadog's widgets), each graph its size and, if it says, a time of its own;
 * edited in place (a cell's text, a graph's query or its definition, its size; cells added, moved
 * and removed; its name, its type, whether it is a template), its time picked; a postmortem says
 * its incident.
 */
import * as React from "react";
import { ArrowDown, ArrowUp, X } from "lucide-react";
import { observeApi, type GraphSize, type Notebook, type NotebookCell, type NotebookCellAttributes, type NotebookTime, type NotebookType } from "@/api/observe";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/dialog";
import { Hold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, Panel, SelectField } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/tabs";
import { toast } from "@/components/ui/toast";
import { useRes, type Res } from "@/lib/data";
import { relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { Link, useNavigate, useParams } from "@/lib/router";
import { TYPE_WORDS } from "./notebooks";
import { RANGES, rangeSaid, useParam, type Range, type Span } from "./parts";
import { KIND_WORDS, ROW, WidgetBody } from "./widgets";
import "./observe.css";

// Markdown's renderer is big: loaded with the first note.
const MarkdownBody = React.lazy(() => import("@/lib/markdown").then((m) => ({ default: m.MarkdownBody })));

const notebookRes = (id: string): Res<Notebook> => ({ key: `observe/notebooks/one/${id}`, fetch: (s) => observeApi.notebook(id, s) });

/** Datadog's spans that are kiln's ranges, and back. */
const SPAN_RANGES: Record<string, Range> = { "15m": "15m", "1h": "1h", "4h": "4h", "1d": "1d", "1w": "7d", "1mo": "30d" };
const RANGE_SPANS: Record<Range, string> = { "15m": "15m", "1h": "1h", "4h": "4h", "1d": "1d", "7d": "1w", "30d": "1mo" };
/** The rest of Datadog's spans, each its length (one not fixed, a day). */
const SPAN_MS: Record<string, number> = { "1m": 60_000, "5m": 300_000, "10m": 600_000, "30m": 1_800_000, "2d": 172_800_000, "3mo": 7_776_000_000, "6mo": 15_552_000_000, "1y": 31_536_000_000 };
/** A graph's rows, by Datadog's size. */
const SIZE_ROWS: Record<GraphSize, number> = { xs: 2, s: 3, m: 4, l: 5, xl: 6 };
const SIZES = (Object.keys(SIZE_ROWS) as GraphSize[]).map((s) => ({ value: s, label: { xs: "Extra small", s: "Small", m: "Medium", l: "Large", xl: "Extra large" }[s] }));
const TYPES = Object.keys(TYPE_WORDS) as NotebookType[];

/** A notebook's (or a cell's) time as the charts read it: a range of kiln's, or a fixed time (a
 * span kiln has no range of, back from now to the minute). */
function spanOfTime(t: NotebookTime | null | undefined, fallback: Span): Span {
  if (!t) return fallback;
  if ("live_span" in t) {
    const r = SPAN_RANGES[t.live_span];
    if (r) return r;
    const now = Math.floor(Date.now() / 60_000) * 60_000;
    return { from: now - (SPAN_MS[t.live_span] ?? 86_400_000), to: now };
  }
  return { from: Date.parse(t.start), to: Date.parse(t.end) };
}

const same = (q: string) => q;
const seconds = (rfc3339: string) => Date.parse(rfc3339) / 1000;

export default function NotebookPage() {
  const { id = "" } = useParams();
  return (
    <div className="container-console page">
      <Loadable key={id} label="the notebook" fallback={<Hold height={480} />}>
        <NotebookView id={id} />
      </Loadable>
    </div>
  );
}

function NotebookView({ id }: { id: string }) {
  const n = useRes(notebookRes(id)).data;
  const [edit, setEdit] = useParam("edit", "");
  return edit === "1" ? <Editor key={n.attributes.modified} n={n} onDone={() => setEdit("")} /> : <Viewer n={n} onEdit={() => setEdit("1")} />;
}

/** Its name, type and who wrote it; its incident when a postmortem. */
function Head({ n, actions }: { n: Notebook; actions: React.ReactNode }) {
  const a = n.attributes;
  return (
    <header className="page-intro">
      <div className="min-w-0">
        <Crumbs index={8} items={[{ label: "Observe", to: "/observe" }, { label: "Notebooks", to: "/observe/notebooks" }, { label: a.name || "Untitled" }]} />
        <h1 className="[overflow-wrap:anywhere]">{a.name || "Untitled"}</h1>
        <p className="lead max-w-3xl text-[13px]">
          {a.metadata.type && <span className="chip mr-2">{TYPE_WORDS[a.metadata.type].toLowerCase()}</span>}
          {a.metadata.is_template && <span className="chip mr-2">template</span>}
          <span className="text-[#8c8f98]">
            By {a.author.handle}, changed {relativeTime(seconds(a.modified))}
          </span>
          {a.kiln.incident && (
            <span className="block text-[13px] text-[#8c8f98]">
              The postmortem of{" "}
              <Link to={`/observe/incidents/${encodeURIComponent(a.kiln.incident)}`} className="text-link">
                its incident
              </Link>
            </span>
          )}
        </p>
      </div>
      <div className="flex flex-wrap gap-2">{actions}</div>
    </header>
  );
}

/** What a notebook says, as it is to be sent back: its attributes and its cells' ids. */
function bodyOf(n: Notebook, change: { time?: NotebookTime }) {
  const a = n.attributes;
  return {
    name: a.name,
    cells: (a.cells ?? []).map((c) => ({ id: c.id, type: "notebook_cells" as const, attributes: c.attributes })),
    time: change.time ?? a.time ?? { live_span: "1h" },
    metadata: a.metadata,
  };
}

function Viewer({ n, onEdit }: { n: Notebook; onEdit: () => void }) {
  const a = n.attributes;
  const navigate = useNavigate();
  const [removing, setRemoving] = React.useState(false);
  const span = spanOfTime(a.time, "1h");
  const setTime = async (r: Range) => {
    try {
      await observeApi.updateNotebook(n.id, bodyOf(n, { time: { live_span: RANGE_SPANS[r] } }));
      invalidate(`observe/notebooks/one/${n.id}`);
    } catch (e) {
      toast({ title: "Couldn't change its time", description: errorMessage(e), tone: "failed" });
    }
  };
  const fixed = typeof span === "string" ? null : span;
  return (
    <>
      <title>{`${a.name || "Untitled"} · Notebooks · Observe · kiln`}</title>
      <Head
        n={n}
        actions={
          <>
            <Button variant="secondary" onClick={onEdit}>
              Edit
            </Button>
            <Button variant="ghost-danger" onClick={() => setRemoving(true)}>
              Delete
            </Button>
          </>
        }
      />
      <div className="mb-6 flex flex-wrap items-center gap-3">
        <Segmented<Range | "fixed">
          label="Time"
          value={fixed ? "fixed" : (span as Range)}
          onChange={(v) => v !== "fixed" && void setTime(v)}
          options={[...(fixed ? [{ value: "fixed" as const, label: rangeSaid(fixed.from, fixed.to) }] : []), ...(Object.keys(RANGES) as Range[]).map((r) => ({ value: r, label: r }))]}
        />
      </div>
      {(a.cells ?? []).length === 0 ? (
        <p className="empty-line py-14 text-center">
          Nothing in it yet.{" "}
          <button type="button" className="text-link" onClick={onEdit}>
            Write in it
          </button>
          .
        </p>
      ) : (
        <div className="flex min-w-0 flex-col gap-4">
          {(a.cells ?? []).map((c) => (
            <CellView key={c.id} c={c} span={span} />
          ))}
        </div>
      )}
      <ConfirmDialog
        open={removing}
        onOpenChange={setRemoving}
        title={`Delete ${a.name || "this notebook"}?`}
        description={a.kiln.incident ? "Its incident keeps its link to it, which then leads nowhere: remove the link there too." : "Its cells go with it."}
        confirmLabel="Delete"
        danger
        onConfirm={async () => {
          try {
            await observeApi.removeNotebook(n.id);
            invalidate("observe/notebooks");
            navigate("/observe/notebooks");
          } catch (e) {
            toast({ title: "Couldn't delete it", description: errorMessage(e), tone: "failed" });
          }
        }}
      />
    </>
  );
}

/** A cell drawn: Markdown as Markdown; a graph as kiln draws Datadog's widget, on its own time if it has one. */
function CellView({ c, span }: { c: NotebookCell; span: Span }) {
  const def = c.attributes.definition;
  if (def.type === "markdown")
    return (
      <section className="obs-note obs-doc min-w-0 [overflow-wrap:anywhere]">
        <React.Suspense fallback={<p className="whitespace-pre-wrap">{def.text}</p>}>
          <MarkdownBody text={def.text ?? ""} />
        </React.Suspense>
      </section>
    );
  const w = c.kiln?.widget;
  const own = spanOfTime(c.attributes.time, span);
  const rows = SIZE_ROWS[c.attributes.graph_size ?? "m"];
  const title = def.title?.trim() || (w ? KIND_WORDS[w.kind] : "A graph");
  return (
    <Panel title={title} description={c.attributes.time ? `On a time of its own: ${typeof own === "string" ? `the last ${own}` : rangeSaid(own.from, own.to)}` : undefined}>
      {w ? (
        <Loadable label="the graph" fallback={<Hold height={rows * ROW - 58} />}>
          <WidgetBody w={{ ...w, layout: { ...w.layout, h: rows } }} range={own} fill={same} fillSearch={same} />
        </Loadable>
      ) : (
        <p className="empty-line">kiln does not draw this cell.</p>
      )}
      {c.kiln?.said?.map((s, i) => (
        <p key={i} className="mt-2 text-[11.5px] text-[#8c8f98]">
          {s.why}
        </p>
      ))}
    </Panel>
  );
}

/** A cell as it is edited: its id when it is one kept, its attributes, and its definition's JSON
 * when it is edited as JSON (a graph of more than one query). */
interface Editing {
  key: number;
  id?: string;
  attributes: NotebookCellAttributes;
  json?: string;
}

/** Whether a graph's definition is one query kiln edits as such (else its JSON is edited). */
const oneQuery = (a: NotebookCellAttributes) =>
  a.definition.type !== "log_stream" && a.definition.type !== "markdown" && a.definition.requests?.length === 1 && typeof a.definition.requests[0]?.q === "string";

function editing(c: NotebookCell, key: number): Editing {
  const a = c.attributes;
  const raw = a.definition.type !== "markdown" && a.definition.type !== "log_stream" && !oneQuery(a);
  return { key, id: c.id, attributes: a, json: raw ? JSON.stringify(a.definition, null, 2) : undefined };
}

const NEW: Record<"text" | "graph" | "logs", NotebookCellAttributes> = {
  text: { definition: { type: "markdown", text: "" } },
  graph: { definition: { type: "timeseries", requests: [{ q: "", display_type: "line" }] }, graph_size: "m" },
  logs: { definition: { type: "log_stream", query: "", columns: [] }, graph_size: "m" },
};

function Editor({ n, onDone }: { n: Notebook; onDone: () => void }) {
  const a = n.attributes;
  const [name, setName] = React.useState(a.name);
  const [type, setType] = React.useState<string>(a.metadata.type ?? "");
  const [template, setTemplate] = React.useState(a.metadata.is_template);
  const [cells, setCells] = React.useState<Editing[]>(() => (a.cells ?? []).map((c, i) => editing(c, i)));
  const [next, setNext] = React.useState(cells.length);
  const [busy, setBusy] = React.useState(false);
  const change = (key: number, f: (e: Editing) => Editing) => setCells((cs) => cs.map((c) => (c.key === key ? f(c) : c)));
  const move = (at: number, by: number) =>
    setCells((cs) => {
      const to = at + by;
      if (to < 0 || to >= cs.length) return cs;
      const out = [...cs];
      const [c] = out.splice(at, 1);
      if (c) out.splice(to, 0, c);
      return out;
    });
  const add = (kind: keyof typeof NEW) => {
    setCells((cs) => [...cs, { key: next, attributes: structuredClone(NEW[kind]) }]);
    setNext((k) => k + 1);
  };
  const save = async () => {
    const out = [];
    for (const [i, c] of cells.entries()) {
      let attributes = c.attributes;
      if (c.json !== undefined) {
        try {
          attributes = { ...attributes, definition: JSON.parse(c.json) };
        } catch {
          toast({ title: `Cell ${i + 1}'s definition does not read`, description: "It is JSON: Datadog's widget definition.", tone: "failed" });
          return;
        }
      }
      out.push({ ...(c.id ? { id: c.id } : {}), type: "notebook_cells" as const, attributes });
    }
    setBusy(true);
    try {
      await observeApi.updateNotebook(n.id, {
        name: name.trim(),
        cells: out,
        time: a.time ?? { live_span: "1h" },
        metadata: { ...a.metadata, is_template: template, type: (type || null) as NotebookType | null },
      });
      invalidate("observe/notebooks");
      onDone();
    } catch (e) {
      toast({ title: "Couldn't save it", description: errorMessage(e), tone: "failed" });
      setBusy(false);
    }
  };
  return (
    <>
      <title>{`Editing ${a.name || "Untitled"} · Notebooks · Observe · kiln`}</title>
      <header className="page-intro">
        <div className="min-w-0 flex-1">
          <Crumbs index={8} items={[{ label: "Observe", to: "/observe" }, { label: "Notebooks", to: "/observe/notebooks" }, { label: a.name || "Untitled" }]} />
          <div className="field max-w-2xl">
            <input aria-label="Its name" value={name} onChange={(e) => setName(e.target.value)} maxLength={80} placeholder="Untitled notebook" />
          </div>
          <div className="mt-3 flex flex-wrap items-center gap-4">
            <SelectField
              label="Type"
              hideLabel
              value={type}
              onChange={setType}
              options={[{ value: "", label: "No type" }, ...TYPES.map((t) => ({ value: t, label: TYPE_WORDS[t] }))]}
              className="min-w-[10rem]"
            />
            <label className="flex items-center gap-2 text-[13px]">
              <input type="checkbox" className="check" checked={template} onChange={(e) => setTemplate(e.target.checked)} /> A template
            </label>
          </div>
        </div>
        <div className="flex flex-wrap gap-2">
          <Button variant="ghost" onClick={onDone}>
            Cancel
          </Button>
          <Button onClick={() => void save()} disabled={busy}>
            Save
          </Button>
        </div>
      </header>
      <div className="flex min-w-0 flex-col gap-4">
        {cells.map((c, i) => (
          <CellEditor
            key={c.key}
            c={c}
            at={i}
            last={i === cells.length - 1}
            onChange={(f) => change(c.key, f)}
            onMove={(by) => move(i, by)}
            onRemove={() => setCells((cs) => cs.filter((x) => x.key !== c.key))}
          />
        ))}
      </div>
      <div className="mt-4 flex flex-wrap gap-2">
        <Button variant="secondary" size="sm" onClick={() => add("text")}>
          Add text
        </Button>
        <Button variant="secondary" size="sm" onClick={() => add("graph")}>
          Add a graph
        </Button>
        <Button variant="secondary" size="sm" onClick={() => add("logs")}>
          Add logs
        </Button>
      </div>
    </>
  );
}

function CellEditor({
  c,
  at,
  last,
  onChange,
  onMove,
  onRemove,
}: {
  c: Editing;
  at: number;
  last: boolean;
  onChange: (f: (e: Editing) => Editing) => void;
  onMove: (by: number) => void;
  onRemove: () => void;
}) {
  const def = c.attributes.definition;
  const set = (d: Partial<typeof def>) => onChange((e) => ({ ...e, attributes: { ...e.attributes, definition: { ...e.attributes.definition, ...d } } }));
  const what = def.type === "markdown" ? "Text" : def.type === "log_stream" ? "Logs" : KIND_WORDS[def.type];
  const n = at + 1;
  return (
    <Panel
      title={`${n}. ${what}`}
      actions={
        <>
          <button type="button" className="icon-btn" data-variant="bare" aria-label={`Move cell ${n} up`} disabled={at === 0} onClick={() => onMove(-1)}>
            <ArrowUp size={15} />
          </button>
          <button type="button" className="icon-btn" data-variant="bare" aria-label={`Move cell ${n} down`} disabled={last} onClick={() => onMove(1)}>
            <ArrowDown size={15} />
          </button>
          <button type="button" className="icon-btn" data-variant="bare" aria-label={`Remove cell ${n}`} onClick={onRemove}>
            <X size={15} />
          </button>
        </>
      }
    >
      {def.type === "markdown" ? (
        <div className="field">
          <textarea
            aria-label={`Cell ${n}'s text`}
            rows={Math.min(24, Math.max(4, (def.text ?? "").split("\n").length + 1))}
            value={def.text ?? ""}
            onChange={(e) => set({ text: e.target.value })}
            placeholder="Markdown"
          />
        </div>
      ) : c.json !== undefined ? (
        <div className="field">
          <textarea aria-label={`Cell ${n}'s definition`} className="font-mono" rows={12} value={c.json} onChange={(e) => onChange((x) => ({ ...x, json: e.target.value }))} spellCheck={false} />
        </div>
      ) : (
        <div className="grid grid-cols-1 gap-3 md:grid-cols-[minmax(0,1fr)_10rem_10rem]">
          <div className="field">
            {def.type === "log_stream" ? (
              <input aria-label={`Cell ${n}'s search`} value={def.query ?? ""} onChange={(e) => set({ query: e.target.value })} placeholder="service:checkout status:error" spellCheck={false} />
            ) : (
              <input
                aria-label={`Cell ${n}'s query`}
                className="font-mono"
                value={def.requests?.[0]?.q ?? ""}
                onChange={(e) => set({ requests: [{ ...(def.requests?.[0] ?? {}), q: e.target.value }] })}
                placeholder="avg:app.latency.p95{service:checkout} by {host}"
                spellCheck={false}
              />
            )}
          </div>
          {def.type === "timeseries" ? (
            <SelectField
              label={`Cell ${n}'s display`}
              hideLabel
              value={String(def.requests?.[0]?.display_type ?? "line")}
              onChange={(v) => set({ requests: [{ ...(def.requests?.[0] ?? {}), display_type: v }] })}
              options={[
                { value: "line", label: "Lines" },
                { value: "bars", label: "Bars" },
                { value: "area", label: "Areas" },
              ]}
            />
          ) : (
            <span className="hidden md:block" />
          )}
          <SelectField
            label={`Cell ${n}'s size`}
            hideLabel
            value={c.attributes.graph_size ?? "m"}
            onChange={(v) => onChange((e) => ({ ...e, attributes: { ...e.attributes, graph_size: v as GraphSize } }))}
            options={SIZES}
          />
        </div>
      )}
    </Panel>
  );
}

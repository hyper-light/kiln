/**
 * A dashboard: its widgets on a grid of twelve columns over one time range, its template variables
 * above them (each a tag, chosen from the values it has); a group's widgets on a grid of its own,
 * under its title. A dashboard of your own is edited in place (add, change, move, resize or remove
 * widgets, a group's within it, then save); kiln's own are copied first. Any is given as Datadog's
 * dashboard JSON.
 */
import * as React from "react";
import { ArrowDown, ArrowUp, Copy, Download, Pencil, Plus, Trash2 } from "lucide-react";
import { useNavigate, useParams, useSearchParams } from "@/lib/router";
import { fillVariables, observeApi, type Dashboard, type TemplateVariable, type Widget, type WidgetKind } from "@/api/observe";
import { Button } from "@/components/ui/button";
import { ConfirmDialog, Dialog, DialogContent } from "@/components/ui/dialog";
import { Hold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, Panel, SelectField, TextField } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { useRes } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { RangePicker, dashboardRes, tagValuesRes, useRange } from "./parts";
import { KIND_WORDS, ROW, WidgetBody } from "./widgets";
import "./observe.css";

export default function DashboardPage() {
  const { id = "" } = useParams();
  return (
    <div className="container-console page">
      <Loadable label="the dashboard" fallback={<Hold height={640} label="Loading the dashboard" />}>
        <DashboardView key={id} id={id} />
      </Loadable>
    </div>
  );
}

/** Widgets in reading order: by row, then column; a group's within it too. */
const ordered = (ws: Widget[]): Widget[] =>
  [...ws].sort((a, b) => a.layout.y - b.layout.y || a.layout.x - b.layout.x).map((w) => (w.kind === "group" ? { ...w, widgets: ordered(w.widgets ?? []) } : w));

/** A widget's columns on its grid. */
const span = (w: Widget) => Math.min(12, Math.max(1, w.layout.w));

/** Widgets changed by `fn`: the dashboard's own, or (`group`) a group's. */
const within = (ws: Widget[], fn: (ws: Widget[]) => Widget[], group?: string): Widget[] => (group ? ws.map((g) => (g.id === group ? { ...g, widgets: fn(g.widgets ?? []) } : g)) : fn(ws));

/** Widget `id` made `w`, wherever it is. */
const replaced = (ws: Widget[], id: string, w: Widget): Widget[] => ws.map((o) => (o.id === id ? w : o.kind === "group" ? { ...o, widgets: replaced(o.widgets ?? [], id, w) } : o));

/** Widgets laid out in their order: each placed after the last, a new row when a row is full. */
function reflow(ws: Widget[]): Widget[] {
  let x = 0;
  let y = 0;
  let rowH = 0;
  return ws.map((w) => {
    const width = Math.min(12, Math.max(1, w.layout.w));
    if (x + width > 12) {
      y += rowH;
      x = 0;
      rowH = 0;
    }
    const placed = { ...w, layout: { x, y, w: width, h: Math.max(1, w.layout.h) }, ...(w.kind === "group" ? { widgets: reflow(w.widgets ?? []) } : {}) };
    x += width;
    rowH = Math.max(rowH, placed.layout.h);
    return placed;
  });
}

const freshId = () => `w${Math.random().toString(16).slice(2, 12)}`;

function DashboardView({ id }: { id: string }) {
  const saved = useRes(dashboardRes(id));
  const [draft, setDraft] = React.useState<Dashboard | null>(null);
  const d = draft ?? saved.data;
  const [range, setRange] = useRange();
  const [params, setParams] = useSearchParams();
  const [editing, setEditing] = React.useState<Widget | null>(null);
  const [vars, setVars] = React.useState(false);
  const [removing, setRemoving] = React.useState(false);
  const [busy, setBusy] = React.useState(false);
  const navigate = useNavigate();
  const values = Object.fromEntries(d.template_variables.map((v) => [v.name, params.get(`var.${v.name}`) ?? v.default]));
  const fill = (q: string) => fillVariables(q, d.template_variables, values);
  const fillSearch = (q: string) => fillVariables(q, d.template_variables, values, "");
  const widgets = draft ? draft.widgets : ordered(d.widgets);
  const change = (fn: (d: Dashboard) => Dashboard) => setDraft((x) => fn(x ?? saved.data));
  const move = (i: number, by: number, group?: string) =>
    change((x) => ({
      ...x,
      widgets: within(
        x.widgets,
        (ws) => {
          const moved = [...ws];
          const [w] = moved.splice(i, 1);
          if (w) moved.splice(Math.max(0, Math.min(moved.length, i + by)), 0, w);
          return moved;
        },
        group,
      ),
    }));
  const remove = (id: string, group?: string) => change((x) => ({ ...x, widgets: within(x.widgets, (ws) => ws.filter((o) => o.id !== id), group) }));
  /** A widget on its grid: its title, in editing its moves (a group's within it), change and
   * removal; its body, or a group's widgets under its title. */
  const frame = (w: Widget, i: number, siblings: Widget[], group?: string): React.ReactNode => {
    const actions = draft ? (
      <>
        <Button variant="ghost" size="xs" aria-label="Move earlier" disabled={i === 0} onClick={() => move(i, -1, group)}>
          <ArrowUp />
        </Button>
        <Button variant="ghost" size="xs" aria-label="Move later" disabled={i === siblings.length - 1} onClick={() => move(i, 1, group)}>
          <ArrowDown />
        </Button>
        <Button variant="ghost" size="xs" aria-label={`Change ${w.title || "the widget"}`} onClick={() => setEditing(w)}>
          <Pencil />
        </Button>
        <Button variant="ghost-danger" size="xs" aria-label={`Remove ${w.title || "the widget"}`} onClick={() => remove(w.id, group)}>
          <Trash2 />
        </Button>
      </>
    ) : undefined;
    if (w.kind === "group") {
      const inner = w.widgets ?? [];
      return (
        <section key={w.id || i} className="obs-widget obs-group" style={{ "--w": span(w) } as React.CSSProperties} aria-label={w.title || KIND_WORDS.group}>
          <div className="obs-group-head">
            <h2>{w.title || KIND_WORDS.group}</h2>
            {actions && <div className="flex items-center gap-1">{actions}</div>}
          </div>
          {inner.length ? <div className="obs-grid">{inner.map((c, j) => frame(c, j, inner, w.id))}</div> : <p className="empty-line py-6 text-center">No widgets in this group.</p>}
        </section>
      );
    }
    return (
      <div key={w.id || i} className="obs-widget" style={{ "--w": span(w) } as React.CSSProperties}>
        <Panel title={w.title || KIND_WORDS[w.kind]} actions={actions}>
          <div style={{ minHeight: Math.max(ROW, w.layout.h * ROW - 58) }}>
            <Loadable key={`${w.kind}/${w.queries.join("\n")}/${w.search}`} label="the widget" fallback={<Hold height={Math.max(ROW, w.layout.h * ROW - 58)} />}>
              <WidgetBody w={w} range={range} fill={fill} fillSearch={fillSearch} />
            </Loadable>
          </div>
        </Panel>
      </div>
    );
  };

  const save = async () => {
    if (!draft) return;
    setBusy(true);
    try {
      const after = await observeApi.updateDashboard({ ...draft, widgets: reflow(draft.widgets) });
      saved.setData(() => after);
      setDraft(null);
      invalidate("observe/dashboards");
    } catch (e) {
      toast({ title: "Couldn't save the dashboard", description: errorMessage(e), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };
  const copy = async () => {
    setBusy(true);
    try {
      const made = await observeApi.createDashboard({ title: `${d.title} (copy)`, description: d.description, template_variables: d.template_variables, widgets: d.widgets });
      invalidate("observe/dashboards");
      navigate(`/observe/dashboards/${encodeURIComponent(made.id)}`);
    } catch (e) {
      toast({ title: "Couldn't copy the dashboard", description: errorMessage(e), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <title>{`${d.title} · Dashboards · kiln`}</title>
      <header className="page-intro">
        <div className="min-w-0 flex-1">
          <Crumbs index={8} items={[{ label: "Observe", to: "/observe" }, { label: "Dashboards", to: "/observe/dashboards" }, { label: d.builtin ? "kiln's" : "Yours" }]} />
          {draft ? (
            <div className="mt-3 grid max-w-[720px] gap-3">
              <TextField label="Title" value={draft.title} onChange={(e) => change((x) => ({ ...x, title: e.target.value }))} />
              <TextField label="Description" value={draft.description} onChange={(e) => change((x) => ({ ...x, description: e.target.value }))} />
            </div>
          ) : (
            <>
              <h1>{d.title}</h1>
              {d.description && <p className="lead">{d.description}</p>}
            </>
          )}
        </div>
        <div className="flex flex-wrap items-center gap-2">
          {d.builtin ? (
            <Button variant="secondary" onClick={() => void copy()} disabled={busy}>
              <Copy /> Copy to change
            </Button>
          ) : draft ? (
            <>
              <Button variant="ghost" onClick={() => setDraft(null)} disabled={busy}>
                Cancel
              </Button>
              <Button onClick={() => void save()} disabled={busy || !draft.title.trim()}>
                Save
              </Button>
            </>
          ) : (
            <>
              <Button variant="ghost" asChild>
                <a href={observeApi.datadogUrl(d.id)} download={`${d.title}.json`} title="This dashboard as Datadog's dashboard JSON">
                  <Download /> Datadog JSON
                </a>
              </Button>
              <Button variant="secondary" onClick={() => setDraft({ ...saved.data, widgets: ordered(saved.data.widgets) })}>
                <Pencil /> Edit
              </Button>
              <Button variant="ghost-danger" size="icon" aria-label="Delete the dashboard" onClick={() => setRemoving(true)}>
                <Trash2 />
              </Button>
            </>
          )}
        </div>
      </header>

      <div className="mb-6 flex flex-wrap items-end gap-3">
        {d.template_variables.map((v) => (
          <React.Suspense key={v.name} fallback={<SelectField label={`$${v.name}`} value="*" onChange={() => undefined} options={[{ value: "*", label: "*" }]} className="w-44" />}>
            <VariablePicker
              v={v}
              value={values[v.name] ?? v.default}
              onChange={(value) =>
                React.startTransition(() =>
                  setParams(
                    (p) => {
                      if (value === v.default) p.delete(`var.${v.name}`);
                      else p.set(`var.${v.name}`, value);
                      return p;
                    },
                    { replace: true },
                  ),
                )
              }
            />
          </React.Suspense>
        ))}
        {draft && (
          <Button variant="ghost" onClick={() => setVars(true)}>
            Variables
          </Button>
        )}
        <div className="sm:ml-auto">
          <RangePicker value={range} onChange={setRange} />
        </div>
      </div>

      {widgets.length === 0 && !draft ? (
        <p className="empty-line py-16 text-center">This dashboard has no widgets yet. Edit it to add some.</p>
      ) : (
        <div className="obs-grid">
          {widgets.map((w, i) => frame(w, i, widgets))}
          {draft && (
            <div className="obs-widget" style={{ "--w": 12 } as React.CSSProperties}>
              <button
                type="button"
                onClick={() => setEditing({ id: "", title: "", kind: "timeseries", queries: [""], search: "", text: "", layout: { x: 0, y: 0, w: 6, h: 4 }, options: null })}
                className="flex min-h-[88px] w-full items-center justify-center gap-2 rounded-[10px] border border-dashed border-white/15 text-[13px] text-[#b4b5ba] hover:border-white/30 hover:text-white"
              >
                <Plus size={15} /> Add a widget
              </button>
            </div>
          )}
        </div>
      )}

      {editing && (
        <WidgetDialog
          w={editing}
          onClose={() => setEditing(null)}
          onSave={(w) => {
            change((x) => ({ ...x, widgets: editing.id ? replaced(x.widgets, editing.id, w) : [...x.widgets, { ...w, id: freshId() }] }));
            setEditing(null);
          }}
        />
      )}
      {vars && draft && (
        <VariablesDialog
          vars={draft.template_variables}
          onClose={() => setVars(false)}
          onSave={(tv) => {
            change((x) => ({ ...x, template_variables: tv }));
            setVars(false);
          }}
        />
      )}
      <ConfirmDialog
        open={removing}
        onOpenChange={setRemoving}
        title={`Delete ${d.title}?`}
        description="Its widgets go with it. Monitors and the data they show stay."
        confirmLabel="Delete"
        danger
        onConfirm={async () => {
          try {
            await observeApi.deleteDashboard(d.id);
            invalidate("observe/dashboards");
            navigate("/observe/dashboards");
          } catch (e) {
            toast({ title: "Couldn't delete the dashboard", description: errorMessage(e), tone: "failed" });
          }
        }}
      />
    </>
  );
}

/** A template variable's value: any, or one of the values its tag has. */
function VariablePicker({ v, value, onChange }: { v: TemplateVariable; value: string; onChange: (value: string) => void }) {
  const values = useRes(tagValuesRes(v.tag)).data;
  const options = ["*", ...values.filter((x) => x !== "*")];
  if (!options.includes(value)) options.push(value);
  return <SelectField label={`$${v.name}`} value={value} onChange={onChange} options={options.map((o) => ({ value: o, label: o === "*" ? `${v.tag}: any` : o }))} className="w-44" />;
}

/** The kinds a widget is made of here: a group comes of Datadog's dashboards (`Import`). */
const KINDS = (Object.keys(KIND_WORDS) as WidgetKind[]).filter((k) => k !== "group");
const UNITS = [
  { value: "", label: "None" },
  { value: "%", label: "Percent" },
  { value: "fraction", label: "Fraction as percent" },
  { value: "B", label: "Bytes" },
  { value: "B/s", label: "Bytes a second" },
  { value: "s", label: "Seconds" },
];
const isMetric = (k: WidgetKind) => k === "timeseries" || k === "query_value" || k === "toplist" || k === "table" || k === "heatmap" || k === "distribution" || k === "pie";

/** A widget's kind, title, what it shows, and its size. */
function WidgetDialog({ w, onClose, onSave }: { w: Widget; onClose: () => void; onSave: (w: Widget) => void }) {
  const [x, setX] = React.useState(w);
  const options = x.options ?? {};
  const setOption = (k: string, v: string) => setX((o) => ({ ...o, options: { ...(o.options ?? {}), [k]: v || undefined } }));
  const [queries, setQueries] = React.useState(w.queries.join("\n"));
  // A check widget's tags as typed, made lists as it is kept.
  const [checkTags, setCheckTags] = React.useState((w.options?.tags ?? []).join(", "));
  const [checkBy, setCheckBy] = React.useState((w.options?.group_by ?? []).join(", "));
  const list = (v: string) =>
    v
      .split(",")
      .map((t) => t.trim())
      .filter(Boolean);
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent title={w.id ? "Change the widget" : "Add a widget"}>
        <form
          className="space-y-4"
          onSubmit={(e) => {
            e.preventDefault();
            onSave({
              ...x,
              queries: isMetric(x.kind)
                ? queries
                    .split("\n")
                    .map((q) => q.trim())
                    .filter(Boolean)
                : [],
              options: x.kind === "check" ? { ...options, tags: list(checkTags), group_by: list(checkBy), grouping: options.grouping ?? "cluster" } : x.options,
            });
          }}
        >
          <div className="grid gap-3 sm:grid-cols-2">
            {x.kind !== "group" && (
              <SelectField label="Kind" value={x.kind} onChange={(k) => setX((o) => ({ ...o, kind: k as WidgetKind }))} options={KINDS.map((k) => ({ value: k, label: KIND_WORDS[k] }))} />
            )}
            <TextField label="Title" value={x.title} onChange={(e) => setX((o) => ({ ...o, title: e.target.value }))} />
          </div>
          {isMetric(x.kind) && (
            <div>
              <label htmlFor="widget-queries" className="field-label">
                Queries, one a line
              </label>
              <div className="field">
                <textarea
                  id="widget-queries"
                  rows={3}
                  className="font-mono"
                  value={queries}
                  onChange={(e) => setQueries(e.target.value)}
                  placeholder="avg:system.cpu.user{$host} by {host}"
                  spellCheck={false}
                />
              </div>
              <p className="field-hint">A template variable ($host) stands for its tag&apos;s value, or any.</p>
            </div>
          )}
          {(x.kind === "logs" || x.kind === "events" || x.kind === "monitors") && (
            <TextField
              label={x.kind === "monitors" ? "Monitors whose name or tags hold" : "Search"}
              className="font-mono"
              value={x.search}
              onChange={(e) => setX((o) => ({ ...o, search: e.target.value }))}
              placeholder={x.kind === "monitors" ? "team:ci" : "service:api status:error"}
            />
          )}
          {x.kind === "check" && (
            <div className="grid gap-3 sm:grid-cols-2">
              <TextField label="Service check" className="font-mono" required value={options.check ?? ""} onChange={(e) => setOption("check", e.target.value)} placeholder="datadog.agent.up" />
              <TextField label="Of the runs tagged" className="font-mono" value={checkTags} onChange={(e) => setCheckTags(e.target.value)} placeholder="env:prod, $host" />
              <SelectField
                label="Shows"
                value={options.grouping ?? "cluster"}
                onChange={(v) => setOption("grouping", v)}
                options={[
                  { value: "cluster", label: "Every group's status" },
                  { value: "check", label: "One group's status" },
                ]}
              />
              {options.grouping === "check" ? (
                <TextField label="The group" className="font-mono" value={options.group ?? ""} onChange={(e) => setOption("group", e.target.value)} placeholder="host:db-1" />
              ) : (
                <TextField label="A group is each" className="font-mono" value={checkBy} onChange={(e) => setCheckBy(e.target.value)} placeholder="host (none: each check)" />
              )}
            </div>
          )}
          {x.kind === "note" && (
            <div>
              <label htmlFor="widget-text" className="field-label">
                Text (Markdown)
              </label>
              <div className="field">
                <textarea id="widget-text" rows={5} value={x.text} onChange={(e) => setX((o) => ({ ...o, text: e.target.value }))} />
              </div>
            </div>
          )}
          <div className="grid gap-3 sm:grid-cols-2">
            <SelectField
              label="Width"
              value={String(x.layout.w)}
              onChange={(v) => setX((o) => ({ ...o, layout: { ...o.layout, w: Number(v) } }))}
              options={[...new Set([3, 4, 6, 8, 12, x.layout.w])].sort((a, b) => a - b).map((n) => ({ value: String(n), label: n === 12 ? "Whole row" : `${n} of 12 columns` }))}
            />
            {x.kind !== "group" && (
              <SelectField
                label="Height"
                value={String(x.layout.h)}
                onChange={(v) => setX((o) => ({ ...o, layout: { ...o.layout, h: Number(v) } }))}
                options={[...new Set([2, 3, 4, 5, 6, 8, x.layout.h])].sort((a, b) => a - b).map((n) => ({ value: String(n), label: `${n} rows` }))}
              />
            )}
            {isMetric(x.kind) && <SelectField label="Unit" value={options.unit ?? ""} onChange={(v) => setOption("unit", v)} options={UNITS} />}
            {x.kind === "timeseries" && (
              <SelectField
                label="Drawn as"
                value={options.display ?? "line"}
                onChange={(v) => setOption("display", v)}
                options={[
                  { value: "line", label: "Lines" },
                  { value: "area", label: "Areas" },
                  { value: "bars", label: "Bars" },
                ]}
              />
            )}
            {(x.kind === "query_value" || x.kind === "toplist" || x.kind === "pie") && (
              <SelectField
                label="Over the range, each series'"
                value={options.aggregator ?? "avg"}
                onChange={(v) => setOption("aggregator", v)}
                options={[
                  { value: "avg", label: "Average" },
                  { value: "sum", label: "Sum" },
                  { value: "max", label: "Most" },
                  { value: "min", label: "Least" },
                  { value: "last", label: "Last" },
                ]}
              />
            )}
          </div>
          <div className="flex justify-end gap-2">
            <Button variant="ghost" onClick={onClose}>
              Cancel
            </Button>
            <Button type="submit">{w.id ? "Change" : "Add"}</Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}

/** The dashboard's template variables: each a name its queries say (`$host`) and the tag it filters. */
function VariablesDialog({ vars, onClose, onSave }: { vars: TemplateVariable[]; onClose: () => void; onSave: (v: TemplateVariable[]) => void }) {
  const [list, setList] = React.useState<TemplateVariable[]>(vars.length ? vars : [{ name: "host", tag: "host", default: "*" }]);
  const set = (i: number, patch: Partial<TemplateVariable>) => setList((l) => l.map((v, j) => (j === i ? { ...v, ...patch } : v)));
  const valid = list.every((v) => /^\w+$/.test(v.name) && v.tag.trim()) && new Set(list.map((v) => v.name)).size === list.length;
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent title="Template variables" description="Each one is a choice above the widgets: $name in a query stands for name's tag at the value chosen, or any.">
        <form
          className="space-y-3"
          onSubmit={(e) => {
            e.preventDefault();
            if (valid) onSave(list.map((v) => ({ ...v, tag: v.tag.trim(), default: v.default.trim() || "*" })));
          }}
        >
          {list.map((v, i) => (
            <div key={i} className="grid grid-cols-[1fr_1fr_1fr_auto] items-end gap-2">
              <TextField label="Name" value={v.name} onChange={(e) => set(i, { name: e.target.value })} />
              <TextField label="Tag" value={v.tag} onChange={(e) => set(i, { tag: e.target.value })} />
              <TextField label="Default" value={v.default} onChange={(e) => set(i, { default: e.target.value })} />
              <Button variant="ghost-danger" size="icon" aria-label={`Remove $${v.name}`} onClick={() => setList((l) => l.filter((_, j) => j !== i))}>
                <Trash2 />
              </Button>
            </div>
          ))}
          <Button variant="ghost" onClick={() => setList((l) => [...l, { name: "", tag: "", default: "*" }])}>
            <Plus /> Add a variable
          </Button>
          <div className="flex justify-end gap-2">
            <Button variant="ghost" onClick={onClose}>
              Cancel
            </Button>
            <Button type="submit" disabled={!valid}>
              Done
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}

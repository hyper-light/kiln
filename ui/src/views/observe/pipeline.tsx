/**
 * A log pipeline (docs/observe.md §8.2): its filter, its processors in order (each edited as its
 * type has it: a grok parser's rules tried on its samples as they are written), and a record tried
 * on it whole before it is kept. kiln's own are read-only: copied to be changed.
 */
import * as React from "react";
import { ArrowDown, ArrowUp, Copy, Pencil, Plus, Trash2 } from "lucide-react";
import { Link, useNavigate, useParams } from "@/lib/router";
import { observeApi, type PipelineIn, type PipelineResult, type Processor, type ProcessorType } from "@/api/observe";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent } from "@/components/ui/dialog";
import { Hold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, Panel, SelectField, TextField } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { useRes } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { swapped } from "@/lib/utils";
import { pipelineRes } from "./parts";

const TYPES: { value: ProcessorType; label: string }[] = [
  { value: "grok-parser", label: "Grok parser" },
  { value: "date-remapper", label: "Date remapper" },
  { value: "status-remapper", label: "Status remapper" },
  { value: "service-remapper", label: "Service remapper" },
  { value: "message-remapper", label: "Message remapper" },
  { value: "trace-id-remapper", label: "Trace ID remapper" },
  { value: "attribute-remapper", label: "Attribute remapper" },
  { value: "url-parser", label: "URL parser" },
  { value: "user-agent-parser", label: "User-agent parser" },
  { value: "category-processor", label: "Category processor" },
  { value: "arithmetic-processor", label: "Arithmetic processor" },
  { value: "string-builder-processor", label: "String builder" },
  { value: "lookup-processor", label: "Lookup" },
  { value: "pipeline", label: "Nested pipeline" },
];
const TYPE = Object.fromEntries(TYPES.map((t) => [t.value, t.label])) as Record<ProcessorType, string>;
const REMAPS: Partial<Record<ProcessorType, string>> = {
  "date-remapper": "its time",
  "status-remapper": "its status",
  "service-remapper": "its service",
  "message-remapper": "its message",
  "trace-id-remapper": "its trace",
};

const BLANK: PipelineIn = { name: "", is_enabled: true, filter: { query: "" }, processors: [] };

const list = (s: string) =>
  s
    .split(/[\s,]+/)
    .map((x) => x.trim())
    .filter(Boolean);
const lines = (s: string) =>
  s
    .split("\n")
    .map((x) => x.trim())
    .filter(Boolean);

export default function PipelinePage() {
  const { id = "" } = useParams();
  return (
    <div className="container-console page max-w-[1100px]">
      {id === "new" ? (
        <>
          <title>New pipeline · Observe · kiln</title>
          <Header crumb="New" />
          <h1 className="sr-only">New pipeline</h1>
          <Editor initial={BLANK} />
        </>
      ) : (
        <Loadable key={id} label="the pipeline" fallback={<Hold height={480} label="Loading the pipeline" />}>
          <Existing id={id} />
        </Loadable>
      )}
    </div>
  );
}

function Header({ crumb, title, children }: { crumb: string; title?: string; children?: React.ReactNode }) {
  return (
    <header className="page-intro">
      <div className="min-w-0">
        <Crumbs index={8} items={[{ label: "Observe", to: "/observe" }, { label: "Pipelines", to: "/observe/pipelines" }, { label: crumb }]} />
        <h1>{title ?? "New pipeline"}</h1>
      </div>
      {children}
    </header>
  );
}

function Existing({ id }: { id: string }) {
  const p = useRes(pipelineRes(id)).data;
  const navigate = useNavigate();
  if (!p.builtin)
    return (
      <>
        <title>{`${p.name} · Pipelines · kiln`}</title>
        <Header crumb={p.name} title={p.name} />
        <Editor initial={p} id={p.id} />
      </>
    );
  const copy = async () => {
    try {
      const made = await observeApi.createPipeline({ name: `${p.name} (copy)`, is_enabled: true, filter: p.filter, processors: p.processors });
      invalidate("observe/pipelines");
      navigate(`/observe/pipelines/${encodeURIComponent(made.id)}`);
    } catch (e) {
      toast({ title: "Couldn't copy it", description: errorMessage(e), tone: "failed" });
    }
  };
  return (
    <>
      <title>{`${p.name} · Pipelines · kiln`}</title>
      <Header crumb={p.name} title={p.name}>
        <Button variant="secondary" onClick={() => void copy()}>
          <Copy /> Copy to change
        </Button>
      </Header>
      <p className="lead mb-8">
        kiln&apos;s own, for <code className="font-mono text-[12px]">{p.filter.query}</code>: {p.is_enabled ? "on" : "off"}. Read-only; copy it to change it.
      </p>
      <div className="space-y-8">
        <Processors processors={p.processors} readOnly onChange={() => undefined} />
        <TryIt pipeline={p} />
      </div>
    </>
  );
}

function Editor({ initial, id }: { initial: PipelineIn; id?: string }) {
  const [p, setP] = React.useState<PipelineIn>(() => ({ name: initial.name, is_enabled: initial.is_enabled, filter: initial.filter, processors: initial.processors }));
  const [busy, setBusy] = React.useState(false);
  const navigate = useNavigate();
  const save = async () => {
    setBusy(true);
    try {
      const saved = id ? await observeApi.updatePipeline(id, p) : await observeApi.createPipeline(p);
      invalidate("observe/pipelines");
      invalidate(`observe/pipeline/${saved.id}`);
      toast({ title: id ? "Saved" : "Made", description: `${saved.name}: records taken from now are read by it.`, tone: "succeeded" });
      if (!id) navigate(`/observe/pipelines/${encodeURIComponent(saved.id)}`);
    } catch (e) {
      toast({ title: "Couldn't save it", description: errorMessage(e), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };
  return (
    <form
      className="space-y-8"
      onSubmit={(e) => {
        e.preventDefault();
        if (p.name.trim()) void save();
      }}
    >
      <div className="grid max-w-[860px] grid-cols-1 gap-3 sm:grid-cols-2">
        <TextField label="Name" value={p.name} onChange={(e) => setP({ ...p, name: e.target.value })} placeholder="checkout's access log" required />
        <TextField
          label="Filter"
          value={p.filter.query}
          onChange={(e) => setP({ ...p, filter: { query: e.target.value } })}
          placeholder="source:checkout"
          hint="A log search: the records it reads. Empty, every record."
          className="font-mono"
          spellCheck={false}
        />
      </div>
      <Processors processors={p.processors} onChange={(processors) => setP({ ...p, processors })} />
      <TryIt pipeline={p} />
      <div className="flex justify-end gap-2">
        <Button variant="ghost" asChild>
          <Link to="/observe/pipelines">Cancel</Link>
        </Button>
        <Button type="submit" disabled={busy || !p.name.trim()}>
          {id ? "Save" : "Make the pipeline"}
        </Button>
      </div>
    </form>
  );
}

/** What a processor does, in a line. */
function summary(x: Processor): string {
  const from = (x.sources ?? []).join(", ");
  switch (x.type) {
    case "grok-parser": {
      const n = lines(x.grok?.match_rules ?? "").filter((l) => !l.startsWith("#")).length;
      return `${n} ${n === 1 ? "rule" : "rules"} on ${x.source ?? "message"}`;
    }
    case "attribute-remapper":
    case "url-parser":
    case "user-agent-parser":
      return `${from} → ${x.target ?? ""}`;
    case "category-processor":
      return `${x.categories?.length ?? 0} categories → ${x.target ?? ""}`;
    case "arithmetic-processor":
      return `${x.expression ?? ""} → ${x.target ?? ""}`;
    case "string-builder-processor":
      return `${x.template ?? ""} → ${x.target ?? ""}`;
    case "lookup-processor":
      return `${x.source ?? ""} → ${x.target ?? ""}, ${x.lookup_table?.length ?? 0} rows`;
    case "pipeline":
      return `${x.filter?.query || "every record"}, ${x.processors?.length ?? 0} processors`;
    default:
      return `${from} → ${REMAPS[x.type] ?? ""}`;
  }
}

function Processors({ processors, onChange, readOnly, nested }: { processors: Processor[]; onChange: (p: Processor[]) => void; readOnly?: boolean; nested?: boolean }) {
  const [editing, setEditing] = React.useState<{ at: number | null; p: Processor } | null>(null);
  const [adding, setAdding] = React.useState<ProcessorType>("grok-parser");
  const move = (i: number, by: -1 | 1) => {
    const j = i + by;
    if (j < 0 || j >= processors.length) return;
    onChange(swapped(processors, i, j));
  };
  return (
    <Panel eyebrow="Processors" title={nested ? "Its processors" : "In order"} description={nested ? undefined : "Each reads what the ones before it made."}>
      {processors.length === 0 ? (
        <p className="empty-line">None yet.</p>
      ) : (
        <ol className="divide-y divide-white/[0.06]">
          {processors.map((x, i) => (
            <li key={i} className="flex min-w-0 items-center gap-3 py-2">
              <span className="cell-mono w-5 shrink-0 text-[#8c8f98]">{i + 1}</span>
              <span className="min-w-0 flex-1">
                <span className="block truncate text-[13px] text-[#e4e5e7]">
                  {x.name || TYPE[x.type]}
                  {x.is_enabled === false && <span className="ml-2 text-[11px] text-[#8c8f98]">off</span>}
                </span>
                <span className="block truncate font-mono text-[10.5px] text-[#8c8f98]">
                  {x.name ? `${TYPE[x.type]} · ` : ""}
                  {summary(x)}
                </span>
              </span>
              {readOnly ? (
                <Button variant="ghost" size="xs" aria-label={`Read processor ${i + 1}`} onClick={() => setEditing({ at: i, p: x })}>
                  Read
                </Button>
              ) : (
                <span className="flex shrink-0 items-center">
                  <Button variant="ghost" size="xs" aria-label={`Move processor ${i + 1} up`} disabled={i === 0} onClick={() => move(i, -1)}>
                    <ArrowUp />
                  </Button>
                  <Button variant="ghost" size="xs" aria-label={`Move processor ${i + 1} down`} disabled={i === processors.length - 1} onClick={() => move(i, 1)}>
                    <ArrowDown />
                  </Button>
                  <Button variant="ghost" size="xs" aria-label={`Edit processor ${i + 1}`} onClick={() => setEditing({ at: i, p: x })}>
                    <Pencil />
                  </Button>
                  <Button variant="ghost-danger" size="xs" aria-label={`Remove processor ${i + 1}`} onClick={() => onChange(processors.filter((_, j) => j !== i))}>
                    <Trash2 />
                  </Button>
                </span>
              )}
            </li>
          ))}
        </ol>
      )}
      {!readOnly && (
        <div className="mt-4 flex flex-wrap items-end gap-2">
          <SelectField
            label="A processor to add"
            value={adding}
            onChange={(v) => setAdding(v as ProcessorType)}
            options={nested ? TYPES.filter((t) => t.value !== "pipeline") : TYPES}
            className="w-56"
          />
          <Button variant="secondary" onClick={() => setEditing({ at: null, p: blank(adding) })}>
            <Plus /> Add
          </Button>
        </div>
      )}
      {editing && (
        <ProcessorDialog
          processor={editing.p}
          readOnly={readOnly}
          onClose={() => setEditing(null)}
          onSave={(x) => {
            onChange(editing.at === null ? [...processors, x] : processors.map((p, j) => (j === editing.at ? x : p)));
            setEditing(null);
          }}
        />
      )}
    </Panel>
  );
}

function blank(type: ProcessorType): Processor {
  switch (type) {
    case "grok-parser":
      return { type, name: "", is_enabled: true, source: "message", samples: [], grok: { match_rules: "", support_rules: "" } };
    case "attribute-remapper":
      return { type, name: "", is_enabled: true, sources: [], target: "", target_format: "auto", preserve_source: false, override_on_conflict: false };
    case "url-parser":
      return { type, name: "", is_enabled: true, sources: ["http.url"], target: "http.url_details", normalize_ending_slashes: false };
    case "user-agent-parser":
      return { type, name: "", is_enabled: true, sources: ["http.useragent"], target: "http.useragent_details", is_encoded: false };
    case "category-processor":
      return { type, name: "", is_enabled: true, target: "", categories: [{ filter: { query: "" }, name: "" }] };
    case "arithmetic-processor":
      return { type, name: "", is_enabled: true, expression: "", target: "", is_replace_missing: false };
    case "string-builder-processor":
      return { type, name: "", is_enabled: true, template: "", target: "", is_replace_missing: false };
    case "lookup-processor":
      return { type, name: "", is_enabled: true, source: "", target: "", lookup_table: [], default_lookup: null };
    case "pipeline":
      return { type, name: "", is_enabled: true, filter: { query: "" }, processors: [] };
    default:
      return { type, name: "", is_enabled: true, sources: [] };
  }
}

function Area({
  id,
  label,
  value,
  onChange,
  rows = 4,
  placeholder,
  hint,
  readOnly,
}: {
  id: string;
  label: string;
  value: string;
  onChange: (v: string) => void;
  rows?: number;
  placeholder?: string;
  hint?: React.ReactNode;
  readOnly?: boolean;
}) {
  return (
    <div>
      <label htmlFor={id} className="field-label">
        {label}
      </label>
      <div className="field">
        <textarea id={id} rows={rows} className="font-mono" value={value} onChange={(e) => onChange(e.target.value)} placeholder={placeholder} spellCheck={false} readOnly={readOnly} />
      </div>
      {hint && <p className="field-hint">{hint}</p>}
    </div>
  );
}

function Check({ label, checked, onChange, disabled }: { label: string; checked: boolean; onChange: (v: boolean) => void; disabled?: boolean }) {
  return (
    <label className="inline-flex items-center gap-2 text-[13px] text-[#c6c8ce]">
      <input type="checkbox" checked={checked} onChange={(e) => onChange(e.target.checked)} disabled={disabled} />
      {label}
    </label>
  );
}

function ProcessorDialog({ processor, onSave, onClose, readOnly }: { processor: Processor; onSave: (p: Processor) => void; onClose: () => void; readOnly?: boolean }) {
  const [x, setX] = React.useState<Processor>(processor);
  const set = (patch: Partial<Processor>) => setX((old) => ({ ...old, ...patch }));
  const t = x.type;
  const sourcesField = (label: string, hint?: string) => (
    <TextField
      label={label}
      value={(x.sources ?? []).join(", ")}
      onChange={(e) => set({ sources: list(e.target.value) })}
      placeholder="level, severity"
      hint={hint ?? "The first of them a record has is read."}
      spellCheck={false}
      readOnly={readOnly}
    />
  );
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent title={TYPE[t]} description={x.name || undefined}>
        <form
          className="space-y-4"
          onSubmit={(e) => {
            e.preventDefault();
            // The dialog sits in the pipeline's form as React sees it (its DOM is elsewhere): its
            // submit would reach that form too, and save the pipeline before this is in it.
            e.stopPropagation();
            if (!readOnly) onSave(x);
          }}
        >
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-[1fr_auto] sm:items-end">
            <TextField label="Name (optional)" value={x.name ?? ""} onChange={(e) => set({ name: e.target.value })} readOnly={readOnly} />
            <Check label="On" checked={x.is_enabled !== false} onChange={(v) => set({ is_enabled: v })} disabled={readOnly} />
          </div>
          {t === "grok-parser" && <Grok x={x} set={set} readOnly={readOnly} />}
          {REMAPS[t] && sourcesField(`Read ${REMAPS[t]} from`)}
          {t === "attribute-remapper" && (
            <>
              {sourcesField("From")}
              <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
                <TextField label="To" value={x.target ?? ""} onChange={(e) => set({ target: e.target.value })} placeholder="usr.id" spellCheck={false} readOnly={readOnly} />
                <SelectField
                  label="As"
                  value={x.target_format ?? "auto"}
                  onChange={(v) => set({ target_format: v as Processor["target_format"] })}
                  options={[
                    { value: "auto", label: "It is" },
                    { value: "string", label: "Text" },
                    { value: "integer", label: "An integer" },
                    { value: "double", label: "A number" },
                  ]}
                />
              </div>
              <div className="flex flex-wrap gap-5">
                <Check label="Keep the source" checked={!!x.preserve_source} onChange={(v) => set({ preserve_source: v })} disabled={readOnly} />
                <Check label="Replace what is there" checked={!!x.override_on_conflict} onChange={(v) => set({ override_on_conflict: v })} disabled={readOnly} />
              </div>
            </>
          )}
          {(t === "url-parser" || t === "user-agent-parser") && (
            <>
              <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
                {sourcesField("From")}
                <TextField label="Into" value={x.target ?? ""} onChange={(e) => set({ target: e.target.value })} spellCheck={false} readOnly={readOnly} />
              </div>
              {t === "url-parser" ? (
                <Check label="Its path without a trailing /" checked={!!x.normalize_ending_slashes} onChange={(v) => set({ normalize_ending_slashes: v })} disabled={readOnly} />
              ) : (
                <Check label="Encoded (%20 and the like)" checked={!!x.is_encoded} onChange={(v) => set({ is_encoded: v })} disabled={readOnly} />
              )}
            </>
          )}
          {t === "category-processor" && <Categories x={x} set={set} readOnly={readOnly} />}
          {t === "arithmetic-processor" && (
            <>
              <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
                <TextField
                  label="Expression"
                  value={x.expression ?? ""}
                  onChange={(e) => set({ expression: e.target.value })}
                  placeholder="http.duration / 1000"
                  className="font-mono"
                  spellCheck={false}
                  readOnly={readOnly}
                />
                <TextField label="Into" value={x.target ?? ""} onChange={(e) => set({ target: e.target.value })} placeholder="duration_s" spellCheck={false} readOnly={readOnly} />
              </div>
              <Check label="What is missing counts as 0" checked={!!x.is_replace_missing} onChange={(v) => set({ is_replace_missing: v })} disabled={readOnly} />
            </>
          )}
          {t === "string-builder-processor" && (
            <>
              <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
                <TextField
                  label="Template"
                  value={x.template ?? ""}
                  onChange={(e) => set({ template: e.target.value })}
                  placeholder="%{http.method} %{http.url_details.path}"
                  className="font-mono"
                  spellCheck={false}
                  readOnly={readOnly}
                />
                <TextField label="Into" value={x.target ?? ""} onChange={(e) => set({ target: e.target.value })} placeholder="http.route" spellCheck={false} readOnly={readOnly} />
              </div>
              <Check label="What is missing is left empty" checked={!!x.is_replace_missing} onChange={(v) => set({ is_replace_missing: v })} disabled={readOnly} />
            </>
          )}
          {t === "lookup-processor" && (
            <>
              <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
                <TextField label="From" value={x.source ?? ""} onChange={(e) => set({ source: e.target.value })} spellCheck={false} readOnly={readOnly} />
                <TextField label="Into" value={x.target ?? ""} onChange={(e) => set({ target: e.target.value })} spellCheck={false} readOnly={readOnly} />
                <TextField label="Else (optional)" value={x.default_lookup ?? ""} onChange={(e) => set({ default_lookup: e.target.value || null })} spellCheck={false} readOnly={readOnly} />
              </div>
              <Area
                id="lookup-table"
                label="Table"
                value={(x.lookup_table ?? []).join("\n")}
                onChange={(v) => set({ lookup_table: lines(v) })}
                rows={5}
                placeholder={"/pay,payments\n/cart,checkout"}
                hint="key,value a line."
                readOnly={readOnly}
              />
            </>
          )}
          {t === "pipeline" && (
            <>
              <TextField
                label="Filter"
                value={x.filter?.query ?? ""}
                onChange={(e) => set({ filter: { query: e.target.value } })}
                placeholder="@http.status_code:>=500"
                className="font-mono"
                spellCheck={false}
                readOnly={readOnly}
              />
              <Processors processors={x.processors ?? []} onChange={(processors) => set({ processors })} readOnly={readOnly} nested />
            </>
          )}
          <div className="flex justify-end gap-2">
            <Button variant="ghost" onClick={onClose}>
              {readOnly ? "Close" : "Cancel"}
            </Button>
            {!readOnly && <Button type="submit">Keep</Button>}
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}

/** A grok parser: its rules, and its samples read by them as they are written. */
function Grok({ x, set, readOnly }: { x: Processor; set: (p: Partial<Processor>) => void; readOnly?: boolean }) {
  const [tried, setTried] = React.useState<{ sample: string; out: PipelineResult | null; error?: string }[] | null>(null);
  const samples = x.samples ?? [];
  const tryIt = async () => {
    try {
      const out = await observeApi.tryPipeline(
        { name: "try", is_enabled: true, filter: { query: "" }, processors: [{ ...x, is_enabled: true }] },
        samples.map((s) => ({ message: s })),
      );
      setTried(samples.map((s, i) => ({ sample: s, out: out[i] ?? null })));
    } catch (e) {
      setTried(samples.map((s) => ({ sample: s, out: null, error: errorMessage(e) })));
    }
  };
  return (
    <>
      <TextField
        label="Parse"
        value={x.source ?? "message"}
        onChange={(e) => set({ source: e.target.value })}
        hint="The attribute read: its message unless said."
        spellCheck={false}
        readOnly={readOnly}
      />
      <Area
        id="grok-match"
        label="Rules"
        value={x.grok?.match_rules ?? ""}
        onChange={(v) => set({ grok: { ...x.grok, match_rules: v } })}
        rows={5}
        placeholder={"access %{ipOrHost:network.client.ip} %{word:http.method} %{notSpace:http.url} %{integer:http.status_code}"}
        hint="A name and a pattern a line, the first that matches a value whole extracting its attributes."
        readOnly={readOnly}
      />
      <Area
        id="grok-support"
        label="Helper rules (optional)"
        value={x.grok?.support_rules ?? ""}
        onChange={(v) => set({ grok: { match_rules: x.grok?.match_rules ?? "", support_rules: v } })}
        rows={3}
        readOnly={readOnly}
      />
      <Area id="grok-samples" label="Samples" value={samples.join("\n")} onChange={(v) => set({ samples: lines(v) })} rows={3} placeholder="A line as it comes, a line each" readOnly={readOnly} />
      {samples.length > 0 && (
        <div>
          <Button variant="ghost" size="xs" onClick={() => void tryIt()}>
            Try the rules on the samples
          </Button>
          {tried && (
            <ul className="mt-2 space-y-2" role="status">
              {tried.map((t, i) => {
                const attrs = Object.entries(t.out?.attributes ?? {});
                return (
                  <li key={i} className="rounded-md border border-white/[0.06] p-2">
                    <p className="truncate font-mono text-[11px] text-[#8c8f98]">{t.sample}</p>
                    {t.error ? (
                      <p className="text-[12px] text-[#e2c4c6]">{t.error}</p>
                    ) : attrs.length === 0 ? (
                      <p className="text-[12px] text-[#e2c4c6]">No rule matches it.</p>
                    ) : (
                      <dl className="mt-1 grid grid-cols-1 gap-x-3 font-mono text-[11px] sm:grid-cols-[minmax(0,auto)_minmax(0,1fr)]">
                        {attrs.map(([k, v]) => (
                          <React.Fragment key={k}>
                            <dt className="truncate text-[#8c8f98]">{k}</dt>
                            <dd className="mb-1 break-all text-[#e4e5e7] sm:mb-0 sm:truncate">{v}</dd>
                          </React.Fragment>
                        ))}
                      </dl>
                    )}
                  </li>
                );
              })}
            </ul>
          )}
        </div>
      )}
    </>
  );
}

function Categories({ x, set, readOnly }: { x: Processor; set: (p: Partial<Processor>) => void; readOnly?: boolean }) {
  const cats = x.categories ?? [];
  const setCat = (i: number, patch: Partial<{ query: string; name: string }>) =>
    set({ categories: cats.map((c, j) => (j === i ? { filter: { query: patch.query ?? c.filter.query }, name: patch.name ?? c.name } : c)) });
  return (
    <>
      <TextField label="Into" value={x.target ?? ""} onChange={(e) => set({ target: e.target.value })} placeholder="http.status_category" spellCheck={false} readOnly={readOnly} />
      <div>
        <p className="field-label">Categories</p>
        <p className="field-hint mb-2">The first whose search matches names a record.</p>
        <ul className="space-y-2">
          {cats.map((c, i) => (
            <li key={i} className="grid grid-cols-[1fr_9rem_auto] items-center gap-2">
              <div className="field">
                <input
                  aria-label={`Category ${i + 1}'s search`}
                  className="font-mono"
                  value={c.filter.query}
                  onChange={(e) => setCat(i, { query: e.target.value })}
                  placeholder="@http.status_code:[500 TO 599]"
                  spellCheck={false}
                  readOnly={readOnly}
                />
              </div>
              <div className="field">
                <input aria-label={`Category ${i + 1}'s name`} value={c.name} onChange={(e) => setCat(i, { name: e.target.value })} placeholder="error" readOnly={readOnly} />
              </div>
              {!readOnly && (
                <Button variant="ghost" size="icon" aria-label={`Remove category ${i + 1}`} onClick={() => set({ categories: cats.filter((_, j) => j !== i) })}>
                  <Trash2 />
                </Button>
              )}
            </li>
          ))}
        </ul>
        {!readOnly && (
          <Button variant="ghost" size="xs" className="mt-2" onClick={() => set({ categories: [...cats, { filter: { query: "" }, name: "" }] })}>
            <Plus /> Add a category
          </Button>
        )}
      </div>
    </>
  );
}

/** A record tried on the pipeline as it is now (saved or not): what it makes of it. */
function TryIt({ pipeline }: { pipeline: PipelineIn }) {
  const guess = /source:(\S+)/.exec(pipeline.filter.query)?.[1] ?? "";
  const [message, setMessage] = React.useState("");
  // The filter's source, until one is typed here.
  const [typed, setTyped] = React.useState<string | null>(null);
  const source = typed ?? guess;
  const [out, setOut] = React.useState<{ r: PipelineResult; matched: boolean } | { error: string } | null>(null);
  const run = async () => {
    try {
      const [r] = await observeApi.tryPipeline({ ...pipeline, is_enabled: true }, [{ message, source }]);
      const changed = !!r && (Object.keys(r.attributes).length > 0 || r.message !== message || r.status !== "info" || r.service !== "");
      setOut(r ? { r, matched: changed } : { error: "no record came back" });
    } catch (e) {
      setOut({ error: errorMessage(e) });
    }
  };
  return (
    <Panel eyebrow="Try it" title="A record through the pipeline" description="As it is now, saved or not: nothing is kept.">
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-[1fr_12rem_auto] sm:items-end">
        <TextField label="Message" value={message} onChange={(e) => setMessage(e.target.value)} placeholder="A line as it comes" className="font-mono" spellCheck={false} />
        <TextField label="Source" value={source} onChange={(e) => setTyped(e.target.value)} placeholder="nginx" spellCheck={false} />
        <Button variant="secondary" disabled={!message.trim()} onClick={() => void run()}>
          Try it
        </Button>
      </div>
      {out && (
        <div className="mt-4" role="status">
          {"error" in out ? (
            <p className="text-[13px] text-[#e2c4c6]">{out.error}</p>
          ) : !out.matched ? (
            <p className="empty-line">Left as it was: its filter does not pick it, or no processor reads it.</p>
          ) : (
            <dl className="grid grid-cols-1 gap-x-4 font-mono text-[12px] sm:grid-cols-[minmax(0,auto)_minmax(0,1fr)] sm:gap-y-1">
              {(
                [
                  ["status", out.r.status],
                  ["service", out.r.service],
                  ["time", new Date(out.r.ts_ns / 1e6).toISOString()],
                  ...(out.r.message !== message ? [["message", out.r.message]] : []),
                  ...Object.entries(out.r.attributes),
                ] as [string, string][]
              )
                .filter(([, v]) => v !== "")
                .map(([k, v]) => (
                  <React.Fragment key={k}>
                    <dt className="truncate text-[#8c8f98]">{k}</dt>
                    <dd className="mb-1.5 break-all text-[#e4e5e7] sm:mb-0 sm:truncate" title={v}>
                      {v}
                    </dd>
                  </React.Fragment>
                ))}
            </dl>
          )}
        </div>
      )}
    </Panel>
  );
}

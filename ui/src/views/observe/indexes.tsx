/**
 * Log indexes (docs/observe.md §8.10), Datadog's: which records are kept for search, how many a day
 * and how long. Each record goes into the first index whose filter it matches; there, the first of
 * its enabled exclusion filters that matches decides; past an index's daily quota its records are
 * not kept for search until its day turns. Records not kept for search are still seen live,
 * archived and counted by metrics from logs.
 */
import * as React from "react";
import { ArrowDown, ArrowUp, Pencil, Plus, Trash2, X } from "lucide-react";
import { observeApi, type ExclusionFilter, type LogsIndex, type LogsIndexIn } from "@/api/observe";
import { Button } from "@/components/ui/button";
import { ConfirmDialog, Dialog, DialogContent } from "@/components/ui/dialog";
import { TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { PageIntro, SelectField, TextField } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { useRes, type Res } from "@/lib/data";
import { absoluteDate } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { swapped } from "@/lib/utils";
import { ObserveNav } from "./parts";

export const indexesRes: Res<LogsIndex[]> = { key: "observe/indexes", fetch: (s) => observeApi.indexes(s), opts: { pollMs: 15_000 } };

const count = (n: number) => n.toLocaleString();
/** An index's name, Datadog's: a lowercase letter, then lowercase letters, digits and hyphens. */
const NAME = /^[a-z][a-z0-9-]{0,99}$/;

export default function Indexes() {
  const [editing, setEditing] = React.useState<LogsIndex | "new" | null>(null);
  return (
    <div className="container-console page">
      <title>Indexes · Observe · kiln</title>
      <PageIntro
        index={8}
        label="Observe"
        title="Indexes"
        lead="Which records are kept for search, how many a day and how long. Each record goes into the first index whose filter it matches; there, the first exclusion filter that matches it decides. Records not kept for search are still seen live, archived and counted by metrics from logs."
        actions={
          <Button onClick={() => setEditing("new")}>
            <Plus /> New index
          </Button>
        }
      />
      <ObserveNav />
      <Loadable label="the indexes" fallback={<TableHold head={<Head />} rows={2} />}>
        <List onEdit={setEditing} />
      </Loadable>
      {editing && <IndexDialog index={editing === "new" ? null : editing} onClose={() => setEditing(null)} />}
    </div>
  );
}

function Head() {
  return (
    <tr>
      <th className="num w-1">#</th>
      <th>Index</th>
      <th className="hidden sm:table-cell">Today</th>
      <th className="hidden md:table-cell">Kept for</th>
      <th className="hidden lg:table-cell">Excluded</th>
      <th className="hidden sm:table-cell" />
    </tr>
  );
}

/** Its records kept today against its quota: a count, a meter, and its word once over it. */
function Today({ i }: { i: LogsIndex }) {
  const t = i.kiln.today;
  if (!t) return <span className="text-[#8c8f98]">No quota</span>;
  const share = Math.min(100, (100 * t.count) / Math.max(1, t.limit));
  return (
    <div className="min-w-[9rem]">
      <div className="font-mono text-[12px]">
        {count(t.count)} <span className="text-[#8c8f98]">of {count(t.limit)}</span>
      </div>
      <div
        className="mt-1 h-1 w-full overflow-hidden rounded-full bg-white/[0.06]"
        role="meter"
        aria-label={`${i.name}'s quota today`}
        aria-valuemin={0}
        aria-valuemax={t.limit}
        aria-valuenow={t.count}
      >
        <div className={i.is_rate_limited ? "h-full bg-[#f27a7a]" : t.warned ? "h-full bg-[#e5c07b]" : "h-full bg-[#8ab4f8]"} style={{ width: `${share}%` }} />
      </div>
      {i.is_rate_limited ? (
        <p className="mt-1 text-[11px] text-[#f27a7a]">Over its quota until {absoluteDate(t.next_reset / 1000)}</p>
      ) : (
        t.warned && <p className="mt-1 text-[11px] text-[#e5c07b]">Past its warning</p>
      )}
    </div>
  );
}

/** Its day against its quota in words, as a phone's row says it: told apart past its warning or over it. */
function TodaySaid({ i }: { i: LogsIndex }) {
  const t = i.kiln.today;
  if (!t) return <>No quota</>;
  const said = `${count(t.count)} of ${count(t.limit)} today`;
  if (i.is_rate_limited) return <span className="text-[#f27a7a]">{said}, over its quota</span>;
  if (t.warned) return <span className="text-[#e5c07b]">{said}, past its warning</span>;
  return <>{said}</>;
}

const keptFor = (i: LogsIndex) => (i.num_retention_days ? `${i.num_retention_days} ${i.num_retention_days === 1 ? "day" : "days"}` : "The store's time");
const excludedSaid = (i: LogsIndex) => {
  const on = i.exclusion_filters.filter((e) => e.is_enabled);
  if (i.exclusion_filters.length === 0) return "Nothing";
  return `${on.length} of ${i.exclusion_filters.length} ${i.exclusion_filters.length === 1 ? "filter" : "filters"} on`;
};

function List({ onEdit }: { onEdit: (i: LogsIndex) => void }) {
  const all = useRes(indexesRes).data;
  const [removing, setRemoving] = React.useState<LogsIndex | null>(null);
  const act = async (what: string, f: () => Promise<unknown>) => {
    try {
      await f();
      invalidate("observe/indexes");
    } catch (e) {
      toast({ title: `Couldn't ${what}`, description: errorMessage(e), tone: "failed" });
    }
  };
  const move = (i: number, by: -1 | 1) => {
    const names = all.map((x) => x.name);
    const j = i + by;
    if (j < 0 || j >= names.length) return;
    void act("move it", () => observeApi.orderIndexes(swapped(names, i, j)));
  };
  return (
    <>
      <div className="table-wrap">
        <table className="data-table">
          <thead>
            <Head />
          </thead>
          <tbody>
            {all.map((x, i) => {
              // On a phone they go beside its name, its day and time on a line of their own below.
              const controls = (
                <>
                  <Button variant="ghost" size="xs" aria-label={`Move ${x.name} up`} disabled={i === 0} onClick={() => move(i, -1)}>
                    <ArrowUp />
                  </Button>
                  <Button variant="ghost" size="xs" aria-label={`Move ${x.name} down`} disabled={i === all.length - 1} onClick={() => move(i, 1)}>
                    <ArrowDown />
                  </Button>
                  <Button variant="ghost" size="xs" aria-label={`Edit ${x.name}`} onClick={() => onEdit(x)}>
                    <Pencil />
                  </Button>
                  <Button variant="ghost-danger" size="xs" aria-label={`Remove ${x.name}`} onClick={() => setRemoving(x)}>
                    <Trash2 />
                  </Button>
                </>
              );
              return (
                <tr key={x.name}>
                  <td className="num cell-mono w-1 text-[#8c8f98]">{i + 1}</td>
                  <td className="truncate-cell">
                    <div className="flex items-center justify-between gap-2">
                      <button type="button" className="min-w-0 truncate text-left text-[13px] text-[#e4e5e7] hover:text-white" onClick={() => onEdit(x)}>
                        {x.name}
                      </button>
                      <span className="-my-1 flex shrink-0 items-center sm:hidden">{controls}</span>
                    </div>
                    <p className="truncate font-mono text-[11px] text-[#8c8f98]">{x.filter.query.trim() || "*"}</p>
                    <p className="mt-1 whitespace-normal text-[11px] text-[#8c8f98] sm:hidden">
                      <TodaySaid i={x} /> · {keptFor(x).toLowerCase()}
                    </p>
                  </td>
                  <td className="hidden sm:table-cell">
                    <Today i={x} />
                  </td>
                  <td className="hidden whitespace-nowrap text-[12.5px] md:table-cell">{keptFor(x)}</td>
                  <td className="hidden whitespace-nowrap text-[12.5px] lg:table-cell">{excludedSaid(x)}</td>
                  <td className="hidden w-1 whitespace-nowrap text-right sm:table-cell">{controls}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
      {all.length === 0 && <p className="empty-line py-10 text-center">No index: no record is kept for search. Make one to keep them.</p>}
      <ConfirmDialog
        open={removing !== null}
        onOpenChange={(o) => !o && setRemoving(null)}
        title={`Remove ${removing?.name ?? ""}?`}
        description="Its records are no longer found by a search, and its files go. Its name can't be used again."
        confirmLabel="Remove"
        danger
        onConfirm={async () => {
          if (removing) await act("remove it", () => observeApi.deleteIndex(removing.name));
        }}
      />
    </>
  );
}

/** An exclusion filter as its form holds it: its share excluded in percent. */
interface Exclusion {
  name: string;
  enabled: boolean;
  query: string;
  percent: string;
  attribute: string;
}

interface Form {
  name: string;
  query: string;
  retention: string;
  limit: string;
  hour: string;
  offset: string;
  warn: string;
  exclusions: Exclusion[];
  tags: string;
}

const HOURS = Array.from({ length: 24 }, (_, h) => `${String(h).padStart(2, "0")}:00`);
const OFFSETS = Array.from({ length: 27 }, (_, i) => i - 12).map((h) => `${h < 0 ? "-" : "+"}${String(Math.abs(h)).padStart(2, "0")}:00`);

const formOf = (i: LogsIndex | null): Form => ({
  name: i?.name ?? "",
  query: i?.filter.query ?? "*",
  retention: i?.num_retention_days ? String(i.num_retention_days) : "",
  limit: i?.daily_limit ? String(i.daily_limit) : "",
  hour: i?.daily_limit_reset?.reset_time ?? "14:00",
  offset: i?.daily_limit_reset?.reset_utc_offset ?? "+00:00",
  warn: i?.daily_limit_warning_threshold_percentage ? String(i.daily_limit_warning_threshold_percentage) : "",
  exclusions: (i?.exclusion_filters ?? []).map((e) => ({
    name: e.name,
    enabled: e.is_enabled,
    query: e.filter.query,
    percent: String(Math.round(e.filter.sample_rate * 10_000) / 100),
    attribute: e.filter.sample_attribute ?? "",
  })),
  tags: (i?.tags ?? []).join(", "),
});

const whole = (v: string) => /^\d+$/.test(v.trim()) && Number(v) >= 1;
const percent = (v: string) => v.trim() !== "" && Number.isFinite(Number(v)) && Number(v) >= 0 && Number(v) <= 100;

function IndexDialog({ index, onClose }: { index: LogsIndex | null; onClose: () => void }) {
  const [f, setF] = React.useState<Form>(() => formOf(index));
  const [busy, setBusy] = React.useState(false);
  const set = (patch: Partial<Form>) => setF((old) => ({ ...old, ...patch }));
  const setEx = (k: number, patch: Partial<Exclusion>) => set({ exclusions: f.exclusions.map((e, i) => (i === k ? { ...e, ...patch } : e)) });
  const tags = f.tags
    .split(",")
    .map((t) => t.trim())
    .filter(Boolean);
  const problem =
    !index && !NAME.test(f.name.trim())
      ? "A name: a lowercase letter, then lowercase letters, digits and hyphens."
      : f.retention.trim() && !whole(f.retention)
        ? "Kept for: a whole number of days."
        : f.limit.trim() && !whole(f.limit)
          ? "A daily quota: a whole number of records."
          : f.warn.trim() && !(Number(f.warn) >= 50 && Number(f.warn) <= 99.99)
            ? "A warning at 50 to 99.99 percent of the quota."
            : f.exclusions.some((e) => !e.name.trim())
              ? "Each exclusion filter a name."
              : f.exclusions.some((e) => !percent(e.percent))
                ? "Each exclusion filter's share: 0 to 100 percent."
                : tags.some((t) => !t.includes(":"))
                  ? "Tags: key:value."
                  : null;
  const save = async () => {
    setBusy(true);
    try {
      const body: Omit<LogsIndexIn, "name"> = {
        filter: { query: f.query.trim() || "*" },
        exclusion_filters: f.exclusions.map((e): ExclusionFilter => ({
          name: e.name.trim(),
          is_enabled: e.enabled,
          filter: { query: e.query.trim() || "*", sample_rate: Number(e.percent) / 100, ...(e.attribute.trim() ? { sample_attribute: e.attribute.trim() } : {}) },
        })),
        daily_limit: f.limit.trim() ? Number(f.limit) : null,
        ...(f.limit.trim() ? { daily_limit_reset: { reset_time: f.hour, reset_utc_offset: f.offset } } : {}),
        ...(f.limit.trim() && f.warn.trim() ? { daily_limit_warning_threshold_percentage: Number(f.warn) } : {}),
        num_retention_days: f.retention.trim() ? Number(f.retention) : null,
        tags,
      };
      const saved = index ? await observeApi.updateIndex(index.name, { ...body, disable_daily_limit: !f.limit.trim() }) : await observeApi.createIndex({ name: f.name.trim(), ...body });
      invalidate("observe/indexes");
      toast({ title: index ? "Saved" : "Made", description: `${saved.name}: records are routed by it from now on.`, tone: "succeeded" });
      onClose();
    } catch (err) {
      toast({ title: "Couldn't save it", description: errorMessage(err), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent
        title={index ? `Edit ${index.name}` : "New index"}
        description={index ? "Records taken from now are routed by it as changed; those it kept stay." : "It goes last in the order: move it up for the records an index before it takes."}
      >
        <form
          className="space-y-4"
          onSubmit={(e) => {
            e.preventDefault();
            if (!problem) void save();
          }}
        >
          {!index && <TextField label="Name" value={f.name} onChange={(e) => set({ name: e.target.value })} placeholder="audit" spellCheck={false} required />}
          <TextField
            label="Of the records"
            value={f.query}
            onChange={(e) => set({ query: e.target.value })}
            placeholder="service:payments"
            className="font-mono"
            spellCheck={false}
            hint="A log search; *, every record an index before it did not take."
          />
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
            <TextField label="Kept for (days)" value={f.retention} onChange={(e) => set({ retention: e.target.value })} placeholder="the store's time" inputMode="numeric" />
            <TextField label="Daily quota (records)" value={f.limit} onChange={(e) => set({ limit: e.target.value })} placeholder="none" inputMode="numeric" />
          </div>
          {f.limit.trim() !== "" && (
            <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
              <SelectField label="Its day begins" value={f.hour} onChange={(v) => set({ hour: v })} options={HOURS.map((h) => ({ value: h, label: h }))} />
              <SelectField label="At UTC" value={f.offset} onChange={(v) => set({ offset: v })} options={OFFSETS.map((o) => ({ value: o, label: o }))} />
              <TextField label="Warn at (%)" value={f.warn} onChange={(e) => set({ warn: e.target.value })} placeholder="no warning" inputMode="decimal" />
            </div>
          )}
          <fieldset className="space-y-3">
            <legend className="field-label">Exclusion filters</legend>
            <p className="field-hint">
              In order: the first enabled one that matches a record decides alone. A share is decided by each value of an attribute where one is named (a trace&apos;s records together).
            </p>
            {f.exclusions.map((e, k) => (
              <div key={k} className="space-y-2 rounded-[10px] border border-white/10 p-3">
                <div className="flex items-end gap-2">
                  <TextField label="Name" value={e.name} onChange={(ev) => setEx(k, { name: ev.target.value })} placeholder="health checks" className="flex-1" />
                  <Button variant="ghost" size="xs" aria-label={`Move ${e.name || "this filter"} up`} disabled={k === 0} onClick={() => set({ exclusions: swapped(f.exclusions, k, k - 1) })}>
                    <ArrowUp />
                  </Button>
                  <Button
                    variant="ghost"
                    size="xs"
                    aria-label={`Move ${e.name || "this filter"} down`}
                    disabled={k === f.exclusions.length - 1}
                    onClick={() => set({ exclusions: swapped(f.exclusions, k, k + 1) })}
                  >
                    <ArrowDown />
                  </Button>
                  <Button variant="ghost-danger" size="xs" aria-label={`Remove ${e.name || "this filter"}`} onClick={() => set({ exclusions: f.exclusions.filter((_, i) => i !== k) })}>
                    <X />
                  </Button>
                </div>
                <TextField label="Of the records" value={e.query} onChange={(ev) => setEx(k, { query: ev.target.value })} placeholder="status:debug" className="font-mono" spellCheck={false} />
                <div className="grid grid-cols-1 gap-2 sm:grid-cols-2">
                  <TextField label="Excluded (%)" value={e.percent} onChange={(ev) => setEx(k, { percent: ev.target.value })} inputMode="decimal" />
                  <TextField label="By (optional)" value={e.attribute} onChange={(ev) => setEx(k, { attribute: ev.target.value })} placeholder="@trace_id" className="font-mono" spellCheck={false} />
                </div>
                <label className="flex items-center gap-2 text-[13px] text-[#c8cad0]">
                  <input type="checkbox" className="check" checked={e.enabled} onChange={(ev) => setEx(k, { enabled: ev.target.checked })} /> On
                </label>
              </div>
            ))}
            <Button variant="ghost" onClick={() => set({ exclusions: [...f.exclusions, { name: "", enabled: true, query: "*", percent: "100", attribute: "" }] })}>
              <Plus /> Exclusion filter
            </Button>
          </fieldset>
          <TextField label="Tags" value={f.tags} onChange={(e) => set({ tags: e.target.value })} placeholder="team:payments, env:prod" spellCheck={false} />
          {problem && (index || f.name !== "") && <p className="field-hint text-[#f2b8b5]">{problem}</p>}
          <div className="flex justify-end gap-2">
            <Button variant="ghost" onClick={onClose}>
              Cancel
            </Button>
            <Button type="submit" disabled={problem !== null || busy}>
              {index ? "Save" : "Make it"}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}

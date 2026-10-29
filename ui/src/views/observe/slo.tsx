/**
 * An SLO: its standing (its SLI over the timeframe, the error budget left, its burn rates), each day
 * of it, and the monitors that alert on it; made and changed in place (docs/observe.md §9.2).
 */
import * as React from "react";
import { BellPlus, Pencil, Trash2 } from "lucide-react";
import { Link, useNavigate, useParams } from "@/lib/router";
import { observeApi, type Slo, type SloIn } from "@/api/observe";
import { Chart } from "@/components/charts/chart";
import { SERIES } from "@/components/charts/types";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/dialog";
import { Hold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, Facts, Panel, TextField } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/tabs";
import { toast } from "@/components/ui/toast";
import { useRes } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { StateBadge, sloRes } from "./parts";
import { Budget, SloBadge, burn, measures, sliText, targetText } from "./slos";

export default function SloPage() {
  const { id = "" } = useParams();
  return (
    <div className="container-console page max-w-[1100px]">
      {id === "new" ? (
        <>
          <title>New SLO · Observe · kiln</title>
          <header className="page-intro">
            <div className="min-w-0">
              <Crumbs index={8} items={[{ label: "Observe", to: "/observe" }, { label: "SLOs", to: "/observe/slos" }, { label: "New" }]} />
              <h1>New SLO</h1>
            </div>
          </header>
          <SloForm initial={BLANK} />
        </>
      ) : (
        <Loadable key={id} label="the SLO" fallback={<Hold height={560} label="Loading the SLO" />}>
          <SloView id={id} />
        </Loadable>
      )}
    </div>
  );
}

const BLANK: SloIn = { name: "", type: "ratio", good: "", total: "", target: 99.9, timeframe: "30d", tags: [] };

/** Hours a timeframe holds. */
const HOURS: Record<SloIn["timeframe"], number> = { "7d": 168, "30d": 720, "90d": 2160 };

/**
 * The workbook's two paging alerts (Beyer et al., The Site Reliability Workbook, ch. 5), scaled to
 * the SLO's timeframe: 2% of the budget spent in an hour (14.4× for 30 days), 5% in six hours (6×);
 * each over its window and a twelfth of it at once.
 */
function burnAlerts(s: Slo) {
  const hours = HOURS[s.timeframe];
  const round = (v: number) => Math.round(v * 100) / 100;
  return [
    { what: "fast", long: "1h", short: "5m", rate: round((0.02 * hours) / 1), spent: "2% of its budget in an hour" },
    { what: "steady", long: "6h", short: "30m", rate: round((0.05 * hours) / 6), spent: "5% of its budget in six hours" },
  ];
}

function SloView({ id }: { id: string }) {
  const res = useRes(sloRes(id));
  const s = res.data;
  const [editing, setEditing] = React.useState(false);
  const [removing, setRemoving] = React.useState(false);
  const [adding, setAdding] = React.useState(false);
  const navigate = useNavigate();
  const st = s.standing;
  const alerts = s.alerts ?? [];
  const addAlerts = async () => {
    setAdding(true);
    try {
      for (const a of burnAlerts(s)) {
        await observeApi.createMonitor({
          name: `${s.name}: error budget burning ${a.what}`,
          query: `slo("${s.id}").burn_rate("${a.long}", "${a.short}") > ${a.rate}`,
          message: `{{#is_alert}}${s.name} is spending ${a.spent}: {{value}}× the pace that spends it in ${s.timeframe}{{/is_alert}}\n{{#is_recovery}}${s.name}'s burn rate is back under ${a.rate}×{{/is_recovery}}`,
          tags: s.tags ?? [],
          priority: null,
          options: { thresholds: {}, notify_no_data: false, every: 60 },
        });
      }
    } catch (e) {
      toast({ title: "Couldn't add the alerts", description: errorMessage(e), tone: "failed" });
    } finally {
      // Those made, shown even if one failed: they are not made twice.
      invalidate("observe/monitors");
      const fresh = await observeApi.slo(s.id).catch(() => null);
      if (fresh) res.setData(() => fresh);
      setAdding(false);
    }
  };
  return (
    <>
      <title>{`${s.name} · SLOs · kiln`}</title>
      <header className="page-intro">
        <div className="min-w-0">
          <Crumbs index={8} items={[{ label: "Observe", to: "/observe" }, { label: "SLOs", to: "/observe/slos" }, { label: s.id }]} />
          <h1>{s.name}</h1>
          <p className="lead flex flex-wrap items-center gap-3">
            <SloBadge state={st.state} />
            <code className="min-w-0 font-mono text-[12px] [overflow-wrap:anywhere] text-[#a7aab3]">{measures(s)}</code>
          </p>
        </div>
        {!editing && (
          <div className="flex flex-wrap items-center gap-2">
            <Button variant="secondary" onClick={() => setEditing(true)}>
              <Pencil /> Edit
            </Button>
            <Button variant="ghost-danger" size="icon" aria-label="Delete the SLO" onClick={() => setRemoving(true)}>
              <Trash2 />
            </Button>
          </div>
        )}
      </header>
      {editing ? (
        <SloForm
          initial={s}
          id={s.id}
          onDone={async (saved) => {
            if (saved) {
              const fresh = await observeApi.slo(s.id);
              res.setData(() => fresh);
            }
            setEditing(false);
          }}
        />
      ) : (
        <div className="space-y-10">
          {st.error && <p className="empty-line text-[#e2c4c6]">{st.error}</p>}
          <Facts
            items={[
              { label: `SLI, last ${s.timeframe}`, value: sliText(st.sli), tone: st.state === "BREACHED" ? "fail" : undefined },
              { label: "Target", value: `${targetText(s.target)}${s.warning ? `, warn under ${targetText(s.warning)}` : ""}` },
              { label: "Error budget left", value: <Budget left={st.budget_left} state={st.state} /> },
              { label: "Burn rate, 1 h · 6 h · 1 d · 3 d", value: ["1h", "6h", "1d", "3d"].map((w) => burn(st.burn_rates?.[w])).join(" · ") },
            ]}
          />
          <Days s={s} />
          <Panel
            eyebrow="Alerts"
            title="Monitors of its burn rate"
            description="The long window says the rate is real, the short one that it still is: an alert pages while both pass, and resolves soon after the burning stops."
          >
            <div className="space-y-4">
              {alerts.length > 0 && (
                <ul className="divide-y divide-white/[0.06]">
                  {alerts.map((a) => (
                    <li key={a.id} className="flex min-w-0 items-center gap-3 py-2">
                      <StateBadge state={a.state} />
                      <span className="min-w-0">
                        <Link to={`/observe/monitors/${encodeURIComponent(a.id)}`} className="block truncate text-[13px] text-[#e4e5e7] hover:text-white">
                          {a.name}
                        </Link>
                        <code className="block truncate font-mono text-[10.5px] text-[#8c8f98]">{a.query}</code>
                      </span>
                    </li>
                  ))}
                </ul>
              )}
              {!alerts.some((a) => a.query.includes(".burn_rate(")) && (
                <>
                  <p className="empty-line">
                    {alerts.length === 0 ? "None yet. " : ""}The workbook's two for a {s.timeframe} SLO page at{" "}
                    {burnAlerts(s)
                      .map((a) => `${a.rate}× over ${a.long} and ${a.short}`)
                      .join(", and at ")}
                    .
                  </p>
                  <Button variant="secondary" disabled={adding} onClick={() => void addAlerts()}>
                    <BellPlus /> Add burn-rate alerts
                  </Button>
                </>
              )}
            </div>
          </Panel>
        </div>
      )}
      <ConfirmDialog
        open={removing}
        onOpenChange={setRemoving}
        title={`Delete ${s.name}?`}
        description="Its standing is read no more. A monitor of it must be deleted first."
        confirmLabel="Delete"
        danger
        onConfirm={async () => {
          try {
            await observeApi.deleteSlo(s.id);
            invalidate("observe/slos");
            navigate("/observe/slos");
          } catch (e) {
            toast({ title: "Couldn't delete the SLO", description: errorMessage(e), tone: "failed" });
          }
        }}
      />
    </>
  );
}

/**
 * Each day of the timeframe: its SLI against the target, and the budget left through it. Lines, not
 * bars: an SLI's days differ in the last places, so its axis is drawn from just under the lowest of
 * them and the target, which bars (measured from zero) cannot be.
 */
function Days({ s }: { s: Slo }) {
  const days = s.standing.days ?? [];
  const slis = days.flatMap((d) => (d.sli === null ? [] : [d.sli]));
  const lefts = days.flatMap((d) => (d.budget_left === null ? [] : [d.budget_left]));
  if (slis.length === 0) return null;
  const xs = days.map((d) => d.day / 1000);
  const low = Math.min(s.target, ...slis);
  const sliRange: [number, number] = [low - Math.max(100 - low, 0.01) * 0.25, 100 + Math.max(100 - low, 0.01) * 0.04];
  const span = sliRange[1] - sliRange[0];
  const places = span < 0.5 ? 3 : span < 5 ? 2 : 1;
  const bottom = Math.min(0, ...lefts);
  const leftRange: [number, number] = [bottom - (100 - bottom) * 0.06, 100 + (100 - bottom) * 0.06];
  return (
    <section aria-label="Day by day" className="grid grid-cols-1 gap-8 lg:grid-cols-2">
      <div>
        <h2 className="field-label">Each day's SLI</h2>
        <Chart
          ariaLabel={`Each day's SLI against the target of ${targetText(s.target)}`}
          data={[xs, days.map((d) => d.sli), xs.map(() => s.target)]}
          height={180}
          timeFormat="day"
          yRange={sliRange}
          yFormat={(v) => `${v.toFixed(places)}%`}
          series={[
            { label: "SLI", color: SERIES.blue, width: 2, points: true },
            { label: "Target", color: SERIES.grey, dash: [4, 4], width: 1 },
          ]}
        />
      </div>
      <div>
        <h2 className="field-label">Error budget left</h2>
        <Chart
          ariaLabel="The error budget left through each day"
          data={[xs, days.map((d) => d.budget_left)]}
          height={180}
          timeFormat="day"
          yRange={leftRange}
          yFormat={(v) => `${Number(v.toFixed(1))}%`}
          legend={false}
          series={[{ label: "Budget left", color: SERIES.violet, width: 2, points: true }]}
        />
      </div>
    </section>
  );
}

const num = (s: string): number | undefined => (s.trim() === "" || !Number.isFinite(Number(s)) ? undefined : Number(s));

/** An SLO's indicator, target and timeframe; made or changed on save. */
function SloForm({ initial, id, onDone }: { initial: SloIn; id?: string; onDone?: (saved: boolean) => void | Promise<void> }) {
  const [name, setName] = React.useState(initial.name);
  const [type, setType] = React.useState<SloIn["type"]>(initial.type);
  const [good, setGood] = React.useState(initial.good ?? "");
  const [total, setTotal] = React.useState(initial.total ?? "");
  const [query, setQuery] = React.useState(initial.query ?? "");
  const [condition, setCondition] = React.useState(initial.condition ?? "< 0.3");
  const [slice, setSlice] = React.useState<"60" | "300">(initial.slice_s === 60 ? "60" : "300");
  const [target, setTarget] = React.useState(String(initial.target));
  const [warning, setWarning] = React.useState(initial.warning === undefined || initial.warning === null ? "" : String(initial.warning));
  const [timeframe, setTimeframe] = React.useState<SloIn["timeframe"]>(initial.timeframe);
  const [tags, setTags] = React.useState((initial.tags ?? []).join(", "));
  const [busy, setBusy] = React.useState(false);
  const navigate = useNavigate();
  const body = (): SloIn => ({
    name: name.trim(),
    type,
    ...(type === "ratio" ? { good: good.trim(), total: total.trim() } : { query: query.trim(), condition: condition.trim(), slice_s: slice === "60" ? 60 : 300 }),
    target: num(target) ?? Number.NaN,
    ...(num(warning) !== undefined ? { warning: num(warning) } : {}),
    timeframe,
    tags: tags
      .split(",")
      .map((t) => t.trim())
      .filter(Boolean),
  });
  const queryField = (fid: string, label: string, value: string, set: (v: string) => void, placeholder: string, hint?: string) => (
    <div>
      <label htmlFor={fid} className="field-label">
        {label}
      </label>
      <div className="field">
        <textarea id={fid} rows={2} className="font-mono" value={value} onChange={(e) => set(e.target.value)} placeholder={placeholder} spellCheck={false} required />
      </div>
      {hint && <p className="field-hint">{hint}</p>}
    </div>
  );
  return (
    <form
      className="max-w-[720px] space-y-5"
      onSubmit={async (e) => {
        e.preventDefault();
        setBusy(true);
        try {
          const saved = id ? await observeApi.updateSlo(id, body()) : await observeApi.createSlo(body());
          invalidate("observe/slos");
          if (onDone) await onDone(true);
          else navigate(`/observe/slos/${encodeURIComponent(saved.id)}`);
        } catch (err) {
          toast({ title: "Couldn't save the SLO", description: errorMessage(err), tone: "failed" });
        } finally {
          setBusy(false);
        }
      }}
    >
      <TextField label="Name" value={name} onChange={(e) => setName(e.target.value)} placeholder="Checkout availability" required />
      <Segmented
        label="Indicator"
        value={type}
        onChange={setType}
        options={[
          { value: "ratio", label: "Good events over all" },
          { value: "slices", label: "Time slices" },
        ]}
      />
      {type === "ratio" ? (
        <>
          {queryField("slo-good", "Good events", good, setGood, "sum:http.requests{service:checkout,status:2xx}.as_count()", "A metric query of counts.")}
          {queryField("slo-total", "All events", total, setTotal, "sum:http.requests{service:checkout}.as_count()")}
        </>
      ) : (
        <>
          {queryField("slo-query", "Query", query, setQuery, "max:http.latency.p99{service:checkout}", "Each slice is good when its value meets the condition.")}
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
            <TextField label="Condition" value={condition} onChange={(e) => setCondition(e.target.value)} placeholder="< 0.3" spellCheck={false} required />
            <div>
              <p className="field-label">Slice</p>
              <Segmented
                label="Slice"
                value={slice}
                onChange={setSlice}
                options={[
                  { value: "60", label: "1 min" },
                  { value: "300", label: "5 min" },
                ]}
              />
            </div>
          </div>
        </>
      )}
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        <TextField label="Target (%)" inputMode="decimal" value={target} onChange={(e) => setTarget(e.target.value)} required />
        <TextField label="Warning (%, optional)" inputMode="decimal" value={warning} onChange={(e) => setWarning(e.target.value)} placeholder="above the target" />
      </div>
      <div>
        <p className="field-label">Timeframe</p>
        <Segmented
          label="Timeframe"
          value={timeframe}
          onChange={setTimeframe}
          options={[
            { value: "7d", label: "7 days" },
            { value: "30d", label: "30 days" },
            { value: "90d", label: "90 days" },
          ]}
        />
      </div>
      <TextField label="Tags" value={tags} onChange={(e) => setTags(e.target.value)} placeholder="team:checkout, tier:1" spellCheck={false} />
      <div className="flex justify-end gap-2">
        {onDone && (
          <Button variant="ghost" onClick={() => void onDone(false)}>
            Cancel
          </Button>
        )}
        <Button type="submit" disabled={busy || !name.trim()}>
          {id ? "Save" : "Make the SLO"}
        </Button>
      </div>
    </form>
  );
}

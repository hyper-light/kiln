/**
 * Downtimes (docs/observe.md §9.3), Datadog's: when monitors' notifications are held, once or
 * recurring, for the monitors named (one, or every one with some tags) and the groups a scope holds.
 * A group held still changes state (an event, marked so); as a downtime ends, what is still wrong
 * is told.
 */
import * as React from "react";
import { Pencil, Plus } from "lucide-react";
import { Link } from "@/lib/router";
import { observeApi, type Downtime, type DowntimeIn, type DowntimeStatus } from "@/api/observe";
import { Button } from "@/components/ui/button";
import { ConfirmDialog, Dialog, DialogContent } from "@/components/ui/dialog";
import { TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { PageIntro, SelectField, TextField } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/tabs";
import { toast } from "@/components/ui/toast";
import { useRes } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { downtimesRes, monitorsRes, ObserveNav } from "./parts";

const TONE: Record<DowntimeStatus, string> = { active: "paused", scheduled: "pending", ended: "skipped", canceled: "cancelled" };
const STATUS: Record<DowntimeStatus, string> = { active: "Holding", scheduled: "Scheduled", ended: "Ended", canceled: "Canceled" };
const DAYS = ["MO", "TU", "WE", "TH", "FR", "SA", "SU"] as const;
const DAY_NAME: Record<string, string> = { MO: "Mon", TU: "Tue", WE: "Wed", TH: "Thu", FR: "Fri", SA: "Sat", SU: "Sun" };

const when = (ms: number) => new Date(ms).toLocaleString(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
const list = (s: string) =>
  s
    .split(/[\s,]+/)
    .map((x) => x.trim())
    .filter(Boolean);

/** A recurrence rule, in words where it is a plain one; else as it is. */
function ruleText(rrule: string): string {
  const parts = Object.fromEntries(rrule.split(";").map((p) => p.split("=") as [string, string]));
  const every = Number(parts.INTERVAL ?? "1");
  const until = parts.UNTIL ? `, until ${parts.UNTIL.slice(0, 4)}-${parts.UNTIL.slice(4, 6)}-${parts.UNTIL.slice(6, 8)}` : parts.COUNT ? `, ${parts.COUNT} times` : "";
  const known = Object.keys(parts).every((k) => ["FREQ", "INTERVAL", "BYDAY", "UNTIL", "COUNT"].includes(k));
  const days = (parts.BYDAY ?? "").split(",").filter(Boolean);
  if (!known || days.some((d) => !(d in DAY_NAME))) return rrule;
  switch (parts.FREQ) {
    case "DAILY":
      return (every === 1 ? "Daily" : `Every ${every} days`) + until;
    case "WEEKLY":
      return (every === 1 ? "Weekly" : `Every ${every} weeks`) + (days.length ? ` on ${days.map((d) => DAY_NAME[d]).join(", ")}` : "") + until;
    case "MONTHLY":
      return (every === 1 ? "Monthly" : `Every ${every} months`) + until;
    case "YEARLY":
      return "Yearly" + until;
    default:
      return rrule;
  }
}

/** When a downtime holds, in words. */
function scheduleText(d: Downtime): string {
  const s = d.schedule;
  if ("recurrences" in s) {
    return s.recurrences.map((r) => `${ruleText(r.rrule)} at ${r.start.slice(11, 16)} for ${r.duration}`).join("; ") + ` (${s.timezone ?? "UTC"})`;
  }
  const start = s.start ?? d.created;
  return s.end ? `${when(start)} to ${when(s.end)}` : `From ${when(start)}, until canceled`;
}

export default function Downtimes() {
  const [editing, setEditing] = React.useState<Downtime | "new" | null>(null);
  return (
    <div className="container-console page">
      <title>Downtimes · Observe · kiln</title>
      <PageIntro
        index={8}
        label="Observe"
        title="Downtimes"
        lead="When monitors' notifications are held: once or recurring, for the monitors named and the groups a scope holds. A group held still changes state, an event marked so; as a downtime ends, what is still wrong is told."
        actions={
          <Button onClick={() => setEditing("new")}>
            <Plus /> New downtime
          </Button>
        }
      />
      <ObserveNav />
      <Loadable label="the downtimes" fallback={<TableHold head={<Head />} rows={3} />}>
        <List onAdd={() => setEditing("new")} onEdit={setEditing} />
      </Loadable>
      {editing && (
        <Loadable label="the monitors" fallback={null}>
          <DowntimeDialog downtime={editing === "new" ? null : editing} onClose={() => setEditing(null)} />
        </Loadable>
      )}
    </div>
  );
}

function Head() {
  return (
    <tr>
      <th>Status</th>
      <th>Holds</th>
      <th className="hidden md:table-cell">When</th>
      <th className="hidden sm:table-cell">Window</th>
      <th />
    </tr>
  );
}

function List({ onAdd, onEdit }: { onAdd: () => void; onEdit: (d: Downtime) => void }) {
  const all = useRes(downtimesRes).data;
  const monitors = useRes(monitorsRes).data;
  const [canceling, setCanceling] = React.useState<Downtime | null>(null);
  const monitorOf = (id: string) => monitors.find((m) => m.id === id);
  const name = (id: string) => monitorOf(id)?.name ?? `${id}, removed`;
  if (all.length === 0)
    return (
      <p className="empty-line py-12 text-center">
        None yet.{" "}
        <button type="button" className="text-link" onClick={onAdd}>
          Make one
        </button>{" "}
        to hold a monitor&apos;s notifications through a maintenance window.
      </p>
    );
  return (
    <>
      <div className="table-wrap">
        <table className="data-table">
          <thead>
            <Head />
          </thead>
          <tbody>
            {all.map((d) => {
              const id = d.monitor_identifier.monitor_id;
              const live = d.status === "active" || d.status === "scheduled";
              return (
                <tr key={d.id}>
                  <td className="w-1 whitespace-nowrap">
                    <span className="status">
                      <i className="dot" data-status={TONE[d.status]} aria-hidden="true" />
                      <span className="status-text" data-status={TONE[d.status]}>
                        {STATUS[d.status]}
                      </span>
                    </span>
                  </td>
                  <td className="truncate-cell">
                    <div className="truncate text-[13px] text-[#e4e5e7]">
                      {id && monitorOf(id) ? (
                        <Link to={`/observe/monitors/${encodeURIComponent(id)}`} className="hover:text-white">
                          {name(id)}
                        </Link>
                      ) : id ? (
                        <span className="text-[#8c8f98]">{name(id)}</span>
                      ) : (
                        <span className="font-mono">monitors tagged {(d.monitor_identifier.monitor_tags ?? []).join(", ")}</span>
                      )}
                    </div>
                    <p className="truncate font-mono text-[11px] text-[#8c8f98]">{d.scope && d.scope !== "*" ? d.scope : "every group"}</p>
                    <p className="text-[12px] whitespace-normal text-[#8c8f98] md:hidden">{scheduleText(d)}</p>
                  </td>
                  <td className="hidden md:table-cell">
                    <div className="max-w-[22rem] truncate" title={scheduleText(d)}>
                      {scheduleText(d)}
                    </div>
                    {d.message && <p className="truncate text-[12px] text-[#8c8f98]">{d.message}</p>}
                  </td>
                  <td className="hidden whitespace-nowrap sm:table-cell">
                    {d.window ? (
                      <>
                        {when(d.window.start)}
                        <div className="text-[12px] text-[#8c8f98]">{d.window.end ? `to ${when(d.window.end)}` : "until canceled"}</div>
                      </>
                    ) : (
                      "—"
                    )}
                  </td>
                  <td className="w-1 whitespace-nowrap">
                    {live && (
                      <>
                        <Button variant="ghost" size="xs" aria-label={`Edit the downtime of ${id ? name(id) : "tagged monitors"}`} onClick={() => onEdit(d)}>
                          <Pencil />
                        </Button>
                        <Button variant="ghost-danger" size="xs" onClick={() => setCanceling(d)}>
                          Cancel
                        </Button>
                      </>
                    )}
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
      <ConfirmDialog
        open={canceling !== null}
        onOpenChange={(o) => !o && setCanceling(null)}
        title="Cancel this downtime?"
        description="Its monitors say what they find again from now; it is kept, canceled."
        confirmLabel="Cancel it"
        danger
        onConfirm={async () => {
          if (!canceling) return;
          try {
            await observeApi.cancelDowntime(canceling.id);
            invalidate("observe/downtimes");
          } catch (e) {
            toast({ title: "Couldn't cancel it", description: errorMessage(e), tone: "failed" });
          }
        }}
      />
    </>
  );
}

interface Form {
  by: "monitor" | "tags";
  monitor: string;
  tags: string;
  scope: string;
  kind: "once" | "recurring";
  start: string;
  end: string;
  freq: "DAILY" | "WEEKLY" | "MONTHLY" | "custom";
  days: string[];
  rrule: string;
  at: string;
  duration: string;
  timezone: string;
  until: string;
  message: string;
  endStates: string[];
  onCancel: boolean;
  quietRecovery: boolean;
}

const localInput = (ms: number | undefined) => {
  if (ms === undefined) return "";
  const d = new Date(ms - new Date(ms).getTimezoneOffset() * 60_000);
  return d.toISOString().slice(0, 16);
};

function formOf(d: Downtime | null, firstMonitor: string): Form {
  const s = d?.schedule;
  const rec = s && "recurrences" in s ? s.recurrences[0] : undefined;
  const parts = Object.fromEntries((rec?.rrule ?? "").split(";").map((p) => p.split("=") as [string, string]));
  const plain = !rec || (["DAILY", "WEEKLY", "MONTHLY"].includes(parts.FREQ ?? "") && Object.keys(parts).every((k) => ["FREQ", "BYDAY", "UNTIL"].includes(k)));
  return {
    by: d && !d.monitor_identifier.monitor_id ? "tags" : "monitor",
    monitor: d?.monitor_identifier.monitor_id ?? firstMonitor,
    tags: (d?.monitor_identifier.monitor_tags ?? []).join(", "),
    scope: d?.scope && d.scope !== "*" ? d.scope : "",
    kind: rec ? "recurring" : "once",
    start: s && !("recurrences" in s) ? localInput(s.start) : "",
    end: s && !("recurrences" in s) ? localInput(s.end) : "",
    freq: rec ? (plain ? (parts.FREQ as Form["freq"]) : "custom") : "WEEKLY",
    days: (parts.BYDAY ?? "SA,SU").split(",").filter(Boolean),
    rrule: rec?.rrule ?? "",
    at: rec ? rec.start.slice(0, 16) : `${new Date().toISOString().slice(0, 10)}T02:00`,
    duration: rec?.duration ?? "2h",
    timezone: s && "recurrences" in s ? (s.timezone ?? "UTC") : Intl.DateTimeFormat().resolvedOptions().timeZone,
    until: parts.UNTIL ? `${parts.UNTIL.slice(0, 4)}-${parts.UNTIL.slice(4, 6)}-${parts.UNTIL.slice(6, 8)}` : "",
    message: d?.message ?? "",
    endStates: d?.notify_end_states ?? ["alert", "no data", "warn"],
    onCancel: d?.notify_end_types?.includes("canceled") ?? false,
    quietRecovery: d?.mute_first_recovery_notification ?? false,
  };
}

/** The rule the form's builder makes. */
function ruleOf(f: Form): string {
  if (f.freq === "custom") return f.rrule.trim();
  const parts = [`FREQ=${f.freq}`];
  if (f.freq === "WEEKLY" && f.days.length) parts.push(`BYDAY=${DAYS.filter((d) => f.days.includes(d)).join(",")}`);
  if (f.until) parts.push(`UNTIL=${f.until.replaceAll("-", "")}`);
  return parts.join(";");
}

function bodyOf(f: Form): DowntimeIn {
  return {
    message: f.message.trim(),
    monitor_identifier: f.by === "monitor" ? { monitor_id: f.monitor } : { monitor_tags: list(f.tags) },
    scope: f.scope.trim() || "*",
    schedule:
      f.kind === "once"
        ? { start: f.start ? new Date(f.start).getTime() : undefined, end: f.end ? new Date(f.end).getTime() : undefined }
        : { recurrences: [{ rrule: ruleOf(f), start: `${f.at}:00`, duration: f.duration.trim() }], timezone: f.timezone.trim() || "UTC" },
    notify_end_states: f.endStates as DowntimeIn["notify_end_states"],
    notify_end_types: f.onCancel ? ["expired", "canceled"] : ["expired"],
    mute_first_recovery_notification: f.quietRecovery,
  };
}

function Check({ label, checked, onChange }: { label: string; checked: boolean; onChange: (v: boolean) => void }) {
  return (
    <label className="flex items-center gap-2 text-[13px] text-[#c8cad0]">
      <input type="checkbox" className="check" checked={checked} onChange={(e) => onChange(e.target.checked)} /> {label}
    </label>
  );
}

function DowntimeDialog({ downtime, onClose }: { downtime: Downtime | null; onClose: () => void }) {
  const monitors = useRes(monitorsRes).data;
  const [f, setF] = React.useState<Form>(() => formOf(downtime, monitors[0]?.id ?? ""));
  const [busy, setBusy] = React.useState(false);
  const set = (patch: Partial<Form>) => setF((old) => ({ ...old, ...patch }));
  const whoOk = f.by === "monitor" ? f.monitor !== "" : list(f.tags).length > 0;
  const onceOk = f.kind !== "once" || !f.start || !f.end || new Date(f.end) > new Date(f.start);
  const recurringOk = f.kind !== "recurring" || (ruleOf(f) !== "" && /^\d+[wdhms](\d+[wdhms])*$/.test(f.duration.trim()) && f.at.length >= 16 && (f.freq !== "WEEKLY" || f.days.length > 0));
  const ok = whoOk && onceOk && recurringOk;
  const save = async () => {
    setBusy(true);
    try {
      if (downtime) await observeApi.updateDowntime(downtime.id, bodyOf(f));
      else await observeApi.createDowntime(bodyOf(f));
      invalidate("observe/downtimes");
      invalidate("observe/monitors");
      onClose();
    } catch (err) {
      toast({ title: "Couldn't save it", description: errorMessage(err), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent title={downtime ? "Edit the downtime" : "New downtime"} description="While it holds, the groups it names say nothing; their changes are still events.">
        <form
          className="space-y-4"
          onSubmit={(e) => {
            e.preventDefault();
            if (ok) void save();
          }}
        >
          <div className="space-y-2">
            <div className="field-label">Holds</div>
            <Segmented
              label="Which monitors"
              value={f.by}
              onChange={(v) => set({ by: v as Form["by"] })}
              options={[
                { value: "monitor", label: "A monitor" },
                { value: "tags", label: "Monitors tagged" },
              ]}
            />
            {f.by === "monitor" ? (
              <SelectField label="Monitor" hideLabel value={f.monitor} onChange={(v) => set({ monitor: v })} options={monitors.map((m) => ({ value: m.id, label: m.name }))} />
            ) : (
              <TextField
                label="Tags"
                value={f.tags}
                onChange={(e) => set({ tags: e.target.value })}
                placeholder="team:pay, env:prod"
                hint="Every monitor that carries all of them."
                spellCheck={false}
              />
            )}
          </div>
          <TextField
            label="Scope (optional)"
            value={f.scope}
            onChange={(e) => set({ scope: e.target.value })}
            placeholder="host:(web-1 OR web-2) -env:dev"
            hint="The groups held; every one when none."
            spellCheck={false}
          />
          <div className="space-y-3">
            <div className="field-label">When</div>
            <Segmented
              label="When"
              value={f.kind}
              onChange={(v) => set({ kind: v as Form["kind"] })}
              options={[
                { value: "once", label: "Once" },
                { value: "recurring", label: "Recurring" },
              ]}
            />
            {f.kind === "once" ? (
              <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
                <TextField label="From" type="datetime-local" value={f.start} onChange={(e) => set({ start: e.target.value })} hint="Now when empty." />
                <TextField label="To" type="datetime-local" value={f.end} onChange={(e) => set({ end: e.target.value })} hint={!onceOk ? "After it starts" : "Until canceled when empty."} />
              </div>
            ) : (
              <>
                <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
                  <SelectField
                    label="Repeats"
                    value={f.freq}
                    onChange={(v) => set({ freq: v as Form["freq"], rrule: v === "custom" && !f.rrule ? ruleOf(f) : f.rrule })}
                    options={[
                      { value: "DAILY", label: "Daily" },
                      { value: "WEEKLY", label: "Weekly" },
                      { value: "MONTHLY", label: "Monthly, on the day it starts" },
                      { value: "custom", label: "A rule of my own (RFC 5545)" },
                    ]}
                  />
                  <TextField label="Starting" type="datetime-local" value={f.at} onChange={(e) => set({ at: e.target.value })} hint="Its first, in the timezone." />
                </div>
                {f.freq === "WEEKLY" && (
                  <div className="flex flex-wrap gap-1" role="group" aria-label="On">
                    {DAYS.map((d) => (
                      <Button
                        key={d}
                        variant={f.days.includes(d) ? "secondary" : "ghost"}
                        size="xs"
                        aria-pressed={f.days.includes(d)}
                        onClick={() => set({ days: f.days.includes(d) ? f.days.filter((x) => x !== d) : [...f.days, d] })}
                      >
                        {DAY_NAME[d]}
                      </Button>
                    ))}
                  </div>
                )}
                {f.freq === "custom" && <TextField label="Rule" value={f.rrule} onChange={(e) => set({ rrule: e.target.value })} placeholder="FREQ=MONTHLY;BYDAY=2TU" spellCheck={false} />}
                <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
                  <TextField label="For" value={f.duration} onChange={(e) => set({ duration: e.target.value })} placeholder="2h" hint="1h30m, 4h, 1d" spellCheck={false} />
                  <TextField label="Timezone" value={f.timezone} onChange={(e) => set({ timezone: e.target.value })} placeholder="UTC" spellCheck={false} />
                  {f.freq !== "custom" && <TextField label="Until (optional)" type="date" value={f.until} onChange={(e) => set({ until: e.target.value })} />}
                </div>
              </>
            )}
          </div>
          <TextField label="Message (optional)" value={f.message} onChange={(e) => set({ message: e.target.value })} placeholder="Patching the web hosts" />
          <div className="space-y-2">
            <div className="field-label">As it ends, tell what is</div>
            <div className="flex flex-wrap gap-4">
              {(
                [
                  ["alert", "Alert"],
                  ["warn", "Warn"],
                  ["no data", "No data"],
                ] as const
              ).map(([s, label]) => (
                <Check key={s} label={label} checked={f.endStates.includes(s)} onChange={(v) => set({ endStates: v ? [...f.endStates, s] : f.endStates.filter((x) => x !== s) })} />
              ))}
            </div>
            <Check label="Tell it when canceled too" checked={f.onCancel} onChange={(v) => set({ onCancel: v })} />
            <Check label="Keep each group's first recovery after quiet" checked={f.quietRecovery} onChange={(v) => set({ quietRecovery: v })} />
          </div>
          <div className="flex justify-end gap-2">
            <Button variant="ghost" onClick={onClose}>
              Cancel
            </Button>
            <Button type="submit" disabled={!ok || busy}>
              {downtime ? "Save" : "Make it"}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}

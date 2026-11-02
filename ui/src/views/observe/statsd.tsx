/**
 * DogStatsD: listeners the server, a runner, or every machine opens (docs/observe.md §8.4), taking
 * the datagrams Datadog's DogStatsD takes, over UDP or a Unix socket, aggregated as its Agent
 * aggregates them: counts, gauges, sets, histograms and distributions a bucket of ten seconds at a
 * time, events and service checks as they come.
 */
import * as React from "react";
import { Pencil, Plus, Trash2 } from "lucide-react";
import { observeApi, type HistogramAggregate, type Statsd, type StatsdIn } from "@/api/observe";
import { Button } from "@/components/ui/button";
import { ConfirmDialog, Dialog, DialogContent } from "@/components/ui/dialog";
import { TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { SectionHeading, TextField } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { useRes } from "@/lib/data";
import { relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { statsdRes } from "./parts";
import { ReadBy, ReadByMachines } from "./scrapes";

const SERVER = "server";
const EVERY = "*";
const AGGREGATES: HistogramAggregate[] = ["max", "median", "avg", "count", "min", "sum"];
const DEFAULT_AGGREGATES = "max, median, avg, count";
const DEFAULT_PERCENTILES = "0.95";
const TONE = { listening: "succeeded", failing: "failed", starting: "pending" } as const;
const STATE = { listening: "Listening", failing: "Failing", starting: "Starting" } as const;

const list = (s: string) =>
  s
    .split(/[\s,]+/)
    .map((x) => x.trim())
    .filter(Boolean);
const count = (n: number) => n.toLocaleString();
const readBy = (on: string) => (on === SERVER ? "The server" : on === EVERY ? "Every machine" : on);

/** A listener's state over its machines: failing if any could not listen, listening once one does. */
function stateOf(l: Statsd): keyof typeof STATE {
  if (l.machines.some((m) => m.state.error)) return "failing";
  return l.machines.some((m) => m.state.listening.length > 0) ? "listening" : "starting";
}

export function StatsdListeners() {
  const [editing, setEditing] = React.useState<Statsd | "new" | null>(null);
  return (
    <section className="mt-12">
      <SectionHeading
        index="05"
        label="DogStatsD"
        title="Where kiln takes DogStatsD"
        aside={
          <Button variant="ghost" onClick={() => setEditing("new")}>
            <Plus /> Add listener
          </Button>
        }
      />
      <Loadable label="the DogStatsD listeners" fallback={<TableHold head={<Head />} rows={1} />}>
        <List onAdd={() => setEditing("new")} onEdit={setEditing} />
      </Loadable>
      {editing && <StatsdDialog listener={editing === "new" ? null : editing} onClose={() => setEditing(null)} />}
    </section>
  );
}

function Head() {
  return (
    <tr>
      <th>State</th>
      <th>Name</th>
      <th className="hidden md:table-cell">Listens on</th>
      <th className="hidden md:table-cell">Opened by</th>
      <th className="hidden sm:table-cell">Last datagram</th>
      <th />
    </tr>
  );
}

function List({ onAdd, onEdit }: { onAdd: () => void; onEdit: (l: Statsd) => void }) {
  const all = useRes(statsdRes).data;
  const [removing, setRemoving] = React.useState<Statsd | null>(null);
  if (all.length === 0)
    return (
      <p className="empty-line py-12 text-center">
        None yet.{" "}
        <button type="button" className="text-link" onClick={onAdd}>
          Add one
        </button>{" "}
        to take what applications send a DogStatsD client.
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
            {all.map((l) => {
              const state = stateOf(l);
              const error = l.machines.find((m) => m.state.error)?.state.error;
              const last = Math.max(0, ...l.machines.map((m) => m.state.last_ms ?? 0));
              const metrics = l.machines.reduce((a, m) => a + m.state.metrics, 0);
              const dropped = l.machines.reduce((a, m) => a + m.state.dropped + m.state.refused, 0);
              return (
                <tr key={l.name}>
                  <td className="whitespace-nowrap">
                    <span className="status">
                      <i className="dot" data-status={TONE[state]} aria-hidden="true" />
                      <span className="status-text" data-status={TONE[state]}>
                        {STATE[state]}
                      </span>
                    </span>
                  </td>
                  <td className="truncate-cell">
                    <div className="truncate text-[13px] text-[#e4e5e7]">{l.name}</div>
                    <div className="truncate font-mono text-[12px] text-[#8c8f98] md:hidden">{l.listen.join(", ")}</div>
                    {error && (
                      <div className="truncate text-[12px] text-[#e2c4c6]" title={error}>
                        {error}
                      </div>
                    )}
                  </td>
                  <td className="hidden md:table-cell">
                    <div className="max-w-[20rem] truncate font-mono text-[12px]" title={l.listen.join(", ")}>
                      {l.listen.join(", ")}
                    </div>
                  </td>
                  <td className="hidden whitespace-nowrap md:table-cell">{readBy(l.on)}</td>
                  <td className="hidden whitespace-nowrap sm:table-cell">
                    {last > 0 ? (
                      <>
                        {relativeTime(last / 1000)}
                        <div className="text-[12px] text-[#8c8f98]">
                          {count(metrics)} metrics{dropped > 0 ? `, ${count(dropped)} not kept` : ""}
                        </div>
                      </>
                    ) : (
                      "—"
                    )}
                  </td>
                  <td className="w-1 whitespace-nowrap">
                    <Button variant="ghost" size="xs" aria-label={`Edit ${l.name}`} onClick={() => onEdit(l)}>
                      <Pencil />
                    </Button>
                    <Button variant="ghost-danger" size="xs" aria-label={`Remove ${l.name}`} onClick={() => setRemoving(l)}>
                      <Trash2 />
                    </Button>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
      <ConfirmDialog
        open={removing !== null}
        onOpenChange={(o) => !o && setRemoving(null)}
        title={`Remove ${removing?.name ?? ""}?`}
        description="Its machines close it; what it took stays, until its retention."
        confirmLabel="Remove"
        danger
        onConfirm={async () => {
          if (!removing) return;
          try {
            await observeApi.deleteStatsd(removing.name);
            invalidate("observe/statsd");
          } catch (e) {
            toast({ title: "Couldn't remove it", description: errorMessage(e), tone: "failed" });
          }
        }}
      />
    </>
  );
}

interface Form {
  name: string;
  listen: string;
  on: string;
  tags: string;
  aggregates: string;
  percentiles: string;
}

const formOf = (l: Statsd | null): Form => ({
  name: l?.name ?? "",
  listen: (l?.listen ?? ["udp://127.0.0.1:8125"]).join(", "),
  on: l?.on ?? EVERY,
  tags: (l?.tags ?? []).join(", "),
  aggregates: (l?.histogram_aggregates ?? []).join(", ") || DEFAULT_AGGREGATES,
  percentiles: (l?.histogram_percentiles ?? []).join(", ") || DEFAULT_PERCENTILES,
});

/** The listener as it is sent: the Agent's defaults left out. */
function bodyOf(f: Form): StatsdIn {
  const aggregates = list(f.aggregates) as HistogramAggregate[];
  const percentiles = list(f.percentiles).map(Number);
  return {
    listen: list(f.listen),
    on: f.on,
    tags: list(f.tags),
    histogram_aggregates: aggregates.join(", ") === DEFAULT_AGGREGATES ? undefined : aggregates,
    histogram_percentiles: f.percentiles.trim() === DEFAULT_PERCENTILES ? undefined : percentiles,
  };
}

function StatsdDialog({ listener, onClose }: { listener: Statsd | null; onClose: () => void }) {
  const [f, setF] = React.useState<Form>(() => formOf(listener));
  const [busy, setBusy] = React.useState(false);
  const set = (patch: Partial<Form>) => setF((old) => ({ ...old, ...patch }));
  const validName = /^[A-Za-z0-9_.-]{1,64}$/.test(f.name);
  const addresses = list(f.listen);
  const validListen = addresses.length >= 1 && addresses.length <= 4 && addresses.every((a) => /^udp:\/\/\S+:\d+$/.test(a) || /^unix:\/\/\/\S+$/.test(a));
  const validAggregates = list(f.aggregates).every((a) => AGGREGATES.includes(a as HistogramAggregate));
  const validPercentiles = list(f.percentiles).every((p) => /^0?\.\d{1,2}$/.test(p) && Number(p) >= 0.01);
  const ok = validName && validListen && validAggregates && validPercentiles;
  const save = async () => {
    setBusy(true);
    try {
      await observeApi.putStatsd(f.name, bodyOf(f));
      invalidate("observe/statsd");
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
        title={listener ? `Edit ${listener.name}` : "Add a DogStatsD listener"}
        description="Metrics are of the machine that takes them (a host: tag says otherwise), aggregated each ten seconds as Datadog's Agent aggregates them."
      >
        <form
          className="space-y-4"
          onSubmit={(e) => {
            e.preventDefault();
            if (ok) void save();
          }}
        >
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
            <TextField
              label="Name"
              value={f.name}
              readOnly={listener !== null}
              onChange={(e) => set({ name: e.target.value })}
              placeholder="dogstatsd"
              hint={f.name && !validName ? "Letters, digits, - _ and . only" : undefined}
            />
            <React.Suspense fallback={<ReadBy value={f.on} onChange={(v) => set({ on: v })} machines={[]} />}>
              <ReadByMachines value={f.on} onChange={(v) => set({ on: v })} />
            </React.Suspense>
          </div>
          <TextField
            label="Listen on"
            value={f.listen}
            onChange={(e) => set({ listen: e.target.value })}
            placeholder="udp://127.0.0.1:8125"
            hint={!validListen ? "udp://host:port or unix:///path, 4 at the most" : "udp://0.0.0.0:8125 takes other machines' too; a Unix socket anyone on the machine may send to."}
            spellCheck={false}
          />
          <TextField label="Tags" value={f.tags} onChange={(e) => set({ tags: e.target.value })} placeholder="env:prod" hint="Given to every metric, event and check." spellCheck={false} />
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
            <TextField
              label="Histogram aggregates"
              value={f.aggregates}
              onChange={(e) => set({ aggregates: e.target.value })}
              hint={!validAggregates ? "max, median, avg, count, min, sum" : undefined}
              spellCheck={false}
            />
            <TextField
              label="Histogram percentiles"
              value={f.percentiles}
              onChange={(e) => set({ percentiles: e.target.value })}
              hint={!validPercentiles ? "0.01 to 0.99, in hundredths" : undefined}
              spellCheck={false}
            />
          </div>
          <div className="flex justify-end gap-2">
            <Button variant="ghost" onClick={onClose}>
              Cancel
            </Button>
            <Button type="submit" disabled={!ok || busy}>
              {listener ? "Save" : "Add"}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}

/**
 * Prometheus: the endpoints kiln scrapes (docs/observe.md §8), each read by the server, a runner,
 * or every machine its own, on its interval, its samples tagged `job` and `instance` as Prometheus
 * tags them. A bearer token is kept by the server, sealed, never shown again; a test reads the
 * endpoint once, from the server.
 */
import * as React from "react";
import { Pencil, Plus, Trash2 } from "lucide-react";
import { observeApi, type Scrape, type ScrapeIn, type ScrapeTest } from "@/api/observe";
import { Button } from "@/components/ui/button";
import { ConfirmDialog, Dialog, DialogContent } from "@/components/ui/dialog";
import { TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { SectionHeading, SelectField, TextField } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { useRes } from "@/lib/data";
import { relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { scrapesRes, statusRes } from "./parts";

const SERVER = "server";
const EVERY = "*";
const TONE = { up: "succeeded", down: "failed", starting: "pending" } as const;
const STATE = { up: "Up", down: "Down", starting: "Starting" } as const;
const KEPT = "Kept. Type a new one to replace it.";

const list = (s: string) =>
  s
    .split(/[\s,]+/)
    .map((x) => x.trim())
    .filter(Boolean);
const count = (n: number) => n.toLocaleString();

/** Who reads a target, in words. */
const readBy = (on: string) => (on === SERVER ? "The server" : on === EVERY ? "Every machine" : on);

/** A target's state over the machines that read it: down if any is, up once all are. */
function stateOf(t: Scrape): keyof typeof STATE {
  const seen = t.machines.filter((m) => m.state.up !== null);
  if (seen.some((m) => m.state.up === false)) return "down";
  return seen.length > 0 ? "up" : "starting";
}

export function Scrapes() {
  const [editing, setEditing] = React.useState<Scrape | "new" | null>(null);
  return (
    <section className="mt-12">
      <SectionHeading
        index="03"
        label="Prometheus"
        title="Endpoints kiln scrapes"
        aside={
          <Button variant="ghost" onClick={() => setEditing("new")}>
            <Plus /> Add target
          </Button>
        }
      />
      <Loadable label="the scrape targets" fallback={<TableHold head={<Head />} rows={2} />}>
        <List onAdd={() => setEditing("new")} onEdit={setEditing} />
      </Loadable>
      {editing && <ScrapeDialog target={editing === "new" ? null : editing} onClose={() => setEditing(null)} />}
    </section>
  );
}

/** As the clouds' list: the state first, then the name, what went wrong beneath it. */
function Head() {
  return (
    <tr>
      <th>State</th>
      <th>Name</th>
      <th className="hidden md:table-cell">Endpoint</th>
      <th className="hidden md:table-cell">Read by</th>
      <th className="hidden sm:table-cell">Last scrape</th>
      <th />
    </tr>
  );
}

function List({ onAdd, onEdit }: { onAdd: () => void; onEdit: (t: Scrape) => void }) {
  const all = useRes(scrapesRes).data;
  const [removing, setRemoving] = React.useState<Scrape | null>(null);
  if (all.length === 0)
    return (
      <p className="empty-line py-12 text-center">
        None yet.{" "}
        <button type="button" className="text-link" onClick={onAdd}>
          Add one
        </button>{" "}
        to read a Prometheus exporter&apos;s /metrics into kiln&apos;s metrics.
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
            {all.map((t) => {
              const state = stateOf(t);
              const error = t.machines.find((m) => m.state.error)?.state.error;
              const last = Math.max(0, ...t.machines.map((m) => m.state.last_ms ?? 0));
              const samples = t.machines.reduce((a, m) => a + m.state.samples, 0);
              return (
                <tr key={t.name}>
                  <td className="whitespace-nowrap">
                    <span className="status">
                      <i className="dot" data-status={TONE[state]} aria-hidden="true" />
                      <span className="status-text" data-status={TONE[state]}>
                        {STATE[state]}
                      </span>
                    </span>
                  </td>
                  <td className="truncate-cell">
                    <div className="truncate text-[13px] text-[#e4e5e7]">{t.name}</div>
                    <div className="truncate font-mono text-[12px] text-[#8c8f98] md:hidden">{t.url}</div>
                    {error && (
                      <div className="truncate text-[12px] text-[#e2c4c6]" title={error}>
                        {error}
                      </div>
                    )}
                  </td>
                  <td className="hidden md:table-cell">
                    <div className="max-w-[20rem] truncate font-mono text-[12px]" title={t.url}>
                      {t.url}
                    </div>
                  </td>
                  <td className="hidden whitespace-nowrap md:table-cell">
                    {readBy(t.on)}
                    <div className="text-[12px] text-[#8c8f98]">every {t.every_s} s</div>
                  </td>
                  <td className="hidden whitespace-nowrap sm:table-cell">
                    {last > 0 ? (
                      <>
                        {relativeTime(last / 1000)}
                        <div className="text-[12px] text-[#8c8f98]">
                          {count(samples)} samples{t.machines.length > 1 ? ` on ${t.machines.length} machines` : ""}
                        </div>
                      </>
                    ) : (
                      "—"
                    )}
                  </td>
                  <td className="w-1 whitespace-nowrap">
                    <Button variant="ghost" size="xs" aria-label={`Edit ${t.name}`} onClick={() => onEdit(t)}>
                      <Pencil />
                    </Button>
                    <Button variant="ghost-danger" size="xs" aria-label={`Remove ${t.name}`} onClick={() => setRemoving(t)}>
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
        description="Its endpoint is scraped no more; what was read stays, until its retention."
        confirmLabel="Remove"
        danger
        onConfirm={async () => {
          if (!removing) return;
          try {
            await observeApi.deleteScrape(removing.name);
            invalidate("observe/scrapes");
          } catch (e) {
            toast({ title: "Couldn't remove it", description: errorMessage(e), tone: "failed" });
          }
        }}
      />
    </>
  );
}

/** Who reads a target (or a log source): the server, every machine its own, or one of the machines. */
export function ReadBy({ value, onChange, machines }: { value: string; onChange: (v: string) => void; machines: string[] }) {
  const options = [{ value: SERVER, label: "The server" }, { value: EVERY, label: "Every machine, each its own" }, ...machines.map((m) => ({ value: m, label: m }))];
  if (!options.some((o) => o.value === value)) options.push({ value, label: value });
  return <SelectField label="Read by" value={value} onChange={onChange} options={options} />;
}

/** [`ReadBy`] with the runners the group knows, once they are read. */
export function ReadByMachines({ value, onChange }: { value: string; onChange: (v: string) => void }) {
  const machines = useRes(statusRes).data.machines.map((m) => m.machine);
  return <ReadBy value={value} onChange={onChange} machines={machines} />;
}

interface Form {
  name: string;
  url: string;
  on: string;
  every: string;
  timeout: string;
  tags: string;
  bearer: string;
}

const formOf = (t: Scrape | null): Form => ({
  name: t?.name ?? "",
  url: t?.url ?? "",
  on: t?.on ?? SERVER,
  every: String(t?.every_s ?? 15),
  timeout: String(t?.timeout_s ?? 10),
  tags: (t?.tags ?? []).join(", "),
  bearer: "",
});

/** The target as it is sent: a token left empty is the kept one (left out). */
const bodyOf = (f: Form): ScrapeIn => ({ url: f.url.trim(), on: f.on, every_s: Number(f.every), timeout_s: Number(f.timeout), tags: list(f.tags), bearer: f.bearer.trim() || undefined });

function ScrapeDialog({ target, onClose }: { target: Scrape | null; onClose: () => void }) {
  const [f, setF] = React.useState<Form>(() => formOf(target));
  const [busy, setBusy] = React.useState(false);
  const [tested, setTested] = React.useState<ScrapeTest | null>(null);
  const set = (patch: Partial<Form>) => {
    setF((old) => ({ ...old, ...patch }));
    setTested(null);
  };
  const validName = /^[A-Za-z0-9_.-]{1,64}$/.test(f.name);
  const validUrl = /^https?:\/\/[^/\s]+/.test(f.url.trim());
  const every = Number(f.every);
  const timeout = Number(f.timeout);
  const timesOk = Number.isInteger(every) && every >= 1 && every <= 86_400 && Number.isInteger(timeout) && timeout >= 1 && timeout <= every;
  const ok = validName && validUrl && timesOk;
  const save = async () => {
    setBusy(true);
    try {
      await observeApi.putScrape(f.name, bodyOf(f));
      invalidate("observe/scrapes");
      onClose();
    } catch (err) {
      toast({ title: "Couldn't save it", description: errorMessage(err), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };
  const test = async () => {
    setBusy(true);
    try {
      setTested(await observeApi.testScrape(f.name || "new", bodyOf(f)));
    } catch (err) {
      setTested({ ok: false, error: errorMessage(err) });
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent
        title={target ? `Edit ${target.name}` : "Add a scrape target"}
        description="Its samples are tagged job (its name) and instance (the endpoint's host and port), as Prometheus tags them."
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
              readOnly={target !== null}
              onChange={(e) => set({ name: e.target.value })}
              placeholder="node"
              hint={f.name && !validName ? "Letters, digits, - _ and . only" : undefined}
            />
            <React.Suspense fallback={<ReadBy value={f.on} onChange={(v) => set({ on: v })} machines={[]} />}>
              <ReadByMachines value={f.on} onChange={(v) => set({ on: v })} />
            </React.Suspense>
          </div>
          <TextField
            label="Endpoint"
            value={f.url}
            onChange={(e) => set({ url: e.target.value })}
            placeholder="http://127.0.0.1:9100/metrics"
            hint={f.url && !validUrl ? "An http:// or https:// URL" : f.on === EVERY ? "Each machine reads its own: 127.0.0.1 is each one's." : undefined}
            spellCheck={false}
          />
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
            <TextField label="Every (seconds)" inputMode="numeric" value={f.every} onChange={(e) => set({ every: e.target.value })} />
            <TextField
              label="Timeout (seconds)"
              inputMode="numeric"
              value={f.timeout}
              onChange={(e) => set({ timeout: e.target.value })}
              hint={!timesOk ? "1 at the least, and no longer than the interval" : undefined}
            />
          </div>
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
            <TextField label="Tags" value={f.tags} onChange={(e) => set({ tags: e.target.value })} placeholder="env:prod" hint="Given to every sample." spellCheck={false} />
            <TextField
              label="Bearer token (optional)"
              type="password"
              value={f.bearer}
              onChange={(e) => set({ bearer: e.target.value })}
              autoComplete="new-password"
              hint={target?.bearer ? KEPT : undefined}
            />
          </div>
          {tested && (
            <div className="field-hint" role="status">
              {tested.ok ? (
                <span>
                  Reads {count(tested.samples ?? 0)} samples of {count(tested.metrics ?? 0)} metrics ({tested.format === "openmetrics" ? "OpenMetrics" : "Prometheus text"}) in{" "}
                  {count(tested.took_ms ?? 0)} ms
                  {tested.skipped ? `; ${count(tested.skipped)} lines skipped` : ""}.
                  {tested.sample && tested.sample.length > 0 && <span className="font-mono"> {tested.sample.slice(0, 4).join(", ")}…</span>}
                </span>
              ) : (
                <span className="text-[#e2c4c6]">{tested.error}</span>
              )}
            </div>
          )}
          <div className="flex justify-end gap-2">
            <Button variant="ghost" onClick={onClose}>
              Cancel
            </Button>
            <Button variant="ghost" disabled={!ok || busy} onClick={() => void test()}>
              Test
            </Button>
            <Button type="submit" disabled={!ok || busy}>
              {target ? "Save" : "Add"}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}

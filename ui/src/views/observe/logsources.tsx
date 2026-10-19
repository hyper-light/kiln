/**
 * Logs kiln collects (docs/observe.md §8.1): files by their globs (a Kubernetes node's pods' and
 * Docker's containers' logs among them) and the systemd journal, each read by the server, a runner,
 * or every machine its own. A line's place is kept once its record is written, so a machine that
 * stops reads on where it left off. A test reads on the server: the files a source matches and the
 * records of the newest one's last lines.
 */
import * as React from "react";
import { Pencil, Plus, Trash2, X } from "lucide-react";
import { observeApi, type LogRule, type LogRuleKind, type LogSource, type LogSourceIn, type LogSourceKind, type LogSourceTest } from "@/api/observe";
import { Button } from "@/components/ui/button";
import { ConfirmDialog, Dialog, DialogContent } from "@/components/ui/dialog";
import { TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { SectionHeading, SelectField, TextField } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { useRes } from "@/lib/data";
import { formatBytes, relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { logSourcesRes } from "./parts";
import { ReadBy, ReadByMachines } from "./scrapes";

const SERVER = "server";
const TYPES: { value: LogSourceKind; label: string }[] = [
  { value: "file", label: "Files" },
  { value: "kubernetes", label: "Kubernetes pods" },
  { value: "docker", label: "Docker containers" },
  { value: "journald", label: "systemd journal" },
];
const TYPE = Object.fromEntries(TYPES.map((t) => [t.value, t.label])) as Record<LogSourceKind, string>;
const DEFAULT_PATHS: Partial<Record<LogSourceKind, string>> = { kubernetes: "/var/log/pods/*/*/*.log", docker: "/var/lib/docker/containers/*/*-json.log" };
const RULES: { value: LogRuleKind; label: string }[] = [
  { value: "exclude_at_match", label: "Leave out lines matching" },
  { value: "include_at_match", label: "Keep only lines matching" },
  { value: "mask_sequences", label: "Mask what matches" },
  { value: "multi_line", label: "A record starts at" },
];
const TONE = { reading: "succeeded", failing: "failed", none: "skipped", starting: "pending" } as const;
const STATE = { reading: "Reading", failing: "Failing", none: "No files", starting: "Starting" } as const;

const count = (n: number) => n.toLocaleString();
const lines = (s: string) =>
  s
    .split("\n")
    .map((x) => x.trim())
    .filter(Boolean);
const list = (s: string) =>
  s
    .split(/[\s,]+/)
    .map((x) => x.trim())
    .filter(Boolean);
const absolute = (p: string) => /^(\/|[A-Za-z]:[\\/]|\\\\)/.test(p);

/** A source's state over the machines that read it: failing if any does, no files if none matched any. */
function stateOf(s: LogSource): keyof typeof STATE {
  if (s.machines.length === 0) return "starting";
  if (s.machines.some((m) => m.state.error)) return "failing";
  if (s.type !== "journald" && s.machines.every((m) => m.state.files === 0)) return "none";
  return "reading";
}

/** What a source reads, in a line. */
function reads(s: LogSource): string {
  if (s.type === "journald") return s.units?.length ? s.units.join(", ") : "every unit";
  return (s.paths?.length ? s.paths : [DEFAULT_PATHS[s.type] ?? ""]).join(", ");
}

export function LogSources() {
  const [editing, setEditing] = React.useState<LogSource | "new" | null>(null);
  return (
    <section className="mt-12">
      <SectionHeading
        index="04"
        label="Logs"
        title="Logs kiln collects"
        aside={
          <Button variant="ghost" onClick={() => setEditing("new")}>
            <Plus /> Add log source
          </Button>
        }
      />
      <Loadable label="the log sources" fallback={<TableHold head={<Head />} rows={2} />}>
        <List onAdd={() => setEditing("new")} onEdit={setEditing} />
      </Loadable>
      {editing && <SourceDialog source={editing === "new" ? null : editing} onClose={() => setEditing(null)} />}
    </section>
  );
}

function Head() {
  return (
    <tr>
      <th>State</th>
      <th>Name</th>
      <th className="hidden md:table-cell">Type</th>
      <th className="hidden md:table-cell">Read by</th>
      <th className="hidden sm:table-cell">Records</th>
      <th />
    </tr>
  );
}

function List({ onAdd, onEdit }: { onAdd: () => void; onEdit: (s: LogSource) => void }) {
  const all = useRes(logSourcesRes).data;
  const [removing, setRemoving] = React.useState<LogSource | null>(null);
  if (all.length === 0)
    return (
      <p className="empty-line py-12 text-center">
        None yet.{" "}
        <button type="button" className="text-link" onClick={onAdd}>
          Add one
        </button>{" "}
        to collect a machine&apos;s log files, its containers&apos; logs or its journal.
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
            {all.map((s) => {
              const state = stateOf(s);
              const error = s.machines.find((m) => m.state.error)?.state.error;
              const records = s.machines.reduce((a, m) => a + m.state.records, 0);
              const last = Math.max(0, ...s.machines.map((m) => m.state.last_ms ?? 0));
              const lag = s.machines.reduce((a, m) => a + m.state.lag_bytes, 0);
              return (
                <tr key={s.name}>
                  <td className="whitespace-nowrap">
                    <span className="status">
                      <i className="dot" data-status={TONE[state]} aria-hidden="true" />
                      <span className="status-text" data-status={TONE[state]}>
                        {STATE[state]}
                      </span>
                    </span>
                  </td>
                  <td className="truncate-cell">
                    <div className="truncate text-[13px] text-[#e4e5e7]">{s.name}</div>
                    <div className="truncate font-mono text-[12px] text-[#8c8f98]" title={reads(s)}>
                      {reads(s)}
                    </div>
                    {error && (
                      <div className="truncate text-[12px] text-[#e2c4c6]" title={error}>
                        {error}
                      </div>
                    )}
                  </td>
                  <td className="hidden whitespace-nowrap md:table-cell">{TYPE[s.type]}</td>
                  <td className="hidden whitespace-nowrap md:table-cell">{s.on === SERVER ? "The server" : s.on === "*" ? "Every machine" : s.on}</td>
                  <td className="hidden whitespace-nowrap sm:table-cell">
                    {count(records)}
                    <div className="text-[12px] text-[#8c8f98]">
                      {last > 0 ? `last ${relativeTime(last / 1000)}` : "none yet"}
                      {lag > 0 ? ` · ${formatBytes(lag)} behind` : ""}
                    </div>
                  </td>
                  <td className="w-1 whitespace-nowrap">
                    <Button variant="ghost" size="xs" aria-label={`Edit ${s.name}`} onClick={() => onEdit(s)}>
                      <Pencil />
                    </Button>
                    <Button variant="ghost-danger" size="xs" aria-label={`Remove ${s.name}`} onClick={() => setRemoving(s)}>
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
        description="Its files and journal are read no more; what was collected stays, until its retention."
        confirmLabel="Remove"
        danger
        onConfirm={async () => {
          if (!removing) return;
          try {
            await observeApi.deleteLogSource(removing.name);
            invalidate("observe/log_sources");
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
  type: LogSourceKind;
  on: string;
  paths: string;
  exclude: string;
  units: string;
  format: NonNullable<LogSourceIn["format"]>;
  start: NonNullable<LogSourceIn["start"]>;
  encoding: NonNullable<LogSourceIn["encoding"]>;
  service: string;
  source: string;
  tags: string;
  rules: LogRule[];
}

const formOf = (s: LogSource | null): Form => ({
  name: s?.name ?? "",
  type: s?.type ?? "file",
  on: s?.on ?? SERVER,
  paths: (s?.paths ?? []).join("\n"),
  exclude: (s?.exclude ?? []).join("\n"),
  units: (s?.units ?? []).join(", "),
  format: s?.format ?? "auto",
  start: s?.start ?? "end",
  encoding: s?.encoding ?? "utf-8",
  service: s?.service ?? "",
  source: s?.source ?? "",
  tags: (s?.tags ?? []).join(", "),
  rules: s?.rules ?? [],
});

/** The source as it is sent: what its type reads, the rest as given. */
function bodyOf(f: Form): LogSourceIn {
  const journal = f.type === "journald";
  return {
    type: f.type,
    ...(journal ? { units: list(f.units) } : { paths: lines(f.paths), exclude: lines(f.exclude), format: f.format, encoding: f.encoding }),
    start: f.start,
    service: f.service.trim() || undefined,
    source: f.source.trim() || undefined,
    tags: list(f.tags),
    rules: f.rules.map((r) => ({ ...r, pattern: r.pattern.trim(), replace_placeholder: r.type === "mask_sequences" ? (r.replace_placeholder ?? "") : undefined })),
    on: f.on,
  };
}

function Field({ id, label, hint, children }: { id: string; label: string; hint?: React.ReactNode; children: React.ReactNode }) {
  return (
    <div>
      <label htmlFor={id} className="field-label">
        {label}
      </label>
      <div className="field">{children}</div>
      {hint && <p className="field-hint">{hint}</p>}
    </div>
  );
}

/** An input of a row whose label is its row's (a rule's pattern): named for the screen reader alone. */
function Bare({ label, className, ...props }: React.InputHTMLAttributes<HTMLInputElement> & { label: string }) {
  return (
    <div className={`field ${className ?? ""}`}>
      <input aria-label={label} {...props} />
    </div>
  );
}

function SourceDialog({ source, onClose }: { source: LogSource | null; onClose: () => void }) {
  const [f, setF] = React.useState<Form>(() => formOf(source));
  const [busy, setBusy] = React.useState(false);
  const [tested, setTested] = React.useState<LogSourceTest | null>(null);
  const set = (patch: Partial<Form>) => {
    setF((old) => ({ ...old, ...patch }));
    setTested(null);
  };
  const journal = f.type === "journald";
  const validName = /^[A-Za-z0-9_-][A-Za-z0-9_.-]{0,63}$/.test(f.name);
  const paths = lines(f.paths);
  const pathsOk = journal || ((f.type !== "file" || paths.length > 0) && [...paths, ...lines(f.exclude)].every(absolute));
  const rulesOk = f.rules.every((r) => r.pattern.trim() !== "") && f.rules.filter((r) => r.type === "multi_line").length <= 1 && !(journal && f.rules.some((r) => r.type === "multi_line"));
  const ok = validName && pathsOk && rulesOk;
  const save = async () => {
    setBusy(true);
    try {
      await observeApi.putLogSource(f.name, bodyOf(f));
      invalidate("observe/log_sources");
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
      setTested(await observeApi.testLogSource(f.name || "new", bodyOf(f)));
    } catch (err) {
      setTested({ ok: false, files: [], matched: 0, records: [], excluded: 0, error: errorMessage(err), took_ms: 0 });
    } finally {
      setBusy(false);
    }
  };
  const setRule = (i: number, patch: Partial<LogRule>) => set({ rules: f.rules.map((r, j) => (j === i ? { ...r, ...patch } : r)) });
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent
        title={source ? `Edit ${source.name}` : "Add a log source"}
        description="Each line a record, its place kept once it is written. A source reads what kiln may read on each machine that reads it."
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
              readOnly={source !== null}
              onChange={(e) => set({ name: e.target.value })}
              placeholder="nginx"
              hint={f.name && !validName ? "Letters, digits, - _ and . only" : undefined}
            />
            <React.Suspense fallback={<ReadBy value={f.on} onChange={(v) => set({ on: v })} machines={[]} />}>
              <ReadByMachines value={f.on} onChange={(v) => set({ on: v })} />
            </React.Suspense>
          </div>
          <SelectField label="Type" value={f.type} onChange={(v) => set({ type: v as LogSourceKind })} options={TYPES} />
          {journal ? (
            <TextField
              label="Units (optional)"
              value={f.units}
              onChange={(e) => set({ units: e.target.value })}
              placeholder="nginx.service, sshd.service"
              hint="Every unit's when none."
              spellCheck={false}
            />
          ) : (
            <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
              <Field
                id="log-paths"
                label={f.type === "file" ? "Paths" : "Paths (optional)"}
                hint={f.paths && !pathsOk ? "Absolute globs, one a line" : "One glob a line: * within a folder, ** across them."}
              >
                <textarea
                  id="log-paths"
                  rows={3}
                  className="font-mono"
                  value={f.paths}
                  onChange={(e) => set({ paths: e.target.value })}
                  placeholder={DEFAULT_PATHS[f.type] ?? "/var/log/nginx/*.log"}
                  spellCheck={false}
                />
              </Field>
              <Field id="log-exclude" label="Leave out (optional)">
                <textarea id="log-exclude" rows={3} className="font-mono" value={f.exclude} onChange={(e) => set({ exclude: e.target.value })} placeholder="/var/log/nginx/*.gz" spellCheck={false} />
              </Field>
            </div>
          )}
          <div className={journal ? "grid grid-cols-1 gap-3" : "grid grid-cols-1 gap-3 sm:grid-cols-3"}>
            {!journal && (
              <SelectField
                label="Lines are"
                value={f.format}
                onChange={(v) => set({ format: v as Form["format"] })}
                options={[
                  { value: "auto", label: "Auto (JSON or text)" },
                  { value: "text", label: "Text" },
                  { value: "json", label: "JSON" },
                  { value: "cri", label: "CRI (Kubernetes)" },
                  { value: "docker", label: "Docker json-file" },
                ]}
              />
            )}
            <SelectField
              label="Start"
              value={f.start}
              onChange={(v) => set({ start: v as Form["start"] })}
              options={[
                { value: "end", label: "New lines only" },
                { value: "beginning", label: "From the beginning" },
              ]}
            />
            {!journal && (
              <SelectField
                label="Encoding"
                value={f.encoding}
                onChange={(v) => set({ encoding: v as Form["encoding"] })}
                options={[
                  { value: "utf-8", label: "UTF-8" },
                  { value: "utf-16le", label: "UTF-16 LE" },
                  { value: "utf-16be", label: "UTF-16 BE" },
                ]}
              />
            )}
          </div>
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
            <TextField
              label="Service (optional)"
              value={f.service}
              onChange={(e) => set({ service: e.target.value })}
              placeholder={f.type === "file" ? "the source's name" : "each container's"}
              spellCheck={false}
            />
            <TextField label="Source (optional)" value={f.source} onChange={(e) => set({ source: e.target.value })} placeholder={f.type} spellCheck={false} />
            <TextField label="Tags" value={f.tags} onChange={(e) => set({ tags: e.target.value })} placeholder="env:prod" spellCheck={false} />
          </div>
          <div>
            <p className="field-label">Rules</p>
            {f.rules.length === 0 && <p className="field-hint">None: every line kept as it is.</p>}
            <ul className="space-y-2">
              {f.rules.map((r, i) => (
                <li key={i} className="grid grid-cols-[1fr_auto] items-start gap-2 sm:grid-cols-[11rem_1fr_auto]">
                  <SelectField label={`Rule ${i + 1}`} hideLabel value={r.type} onChange={(v) => setRule(i, { type: v as LogRuleKind })} options={RULES} className="col-span-2 sm:col-span-1" />
                  <div className={r.type === "mask_sequences" ? "grid grid-cols-[2fr_1fr] gap-2" : ""}>
                    <Bare
                      label={`Rule ${i + 1}'s pattern`}
                      value={r.pattern}
                      onChange={(e) => setRule(i, { pattern: e.target.value })}
                      placeholder={r.type === "multi_line" ? "^\\d{4}-\\d{2}-\\d{2}" : "a regular expression"}
                      className="font-mono"
                      spellCheck={false}
                    />
                    {r.type === "mask_sequences" && (
                      <Bare
                        label={`Rule ${i + 1}'s mask`}
                        value={r.replace_placeholder ?? ""}
                        onChange={(e) => setRule(i, { replace_placeholder: e.target.value })}
                        placeholder="[masked]"
                        spellCheck={false}
                      />
                    )}
                  </div>
                  <Button variant="ghost" size="icon" aria-label={`Remove rule ${i + 1}`} onClick={() => set({ rules: f.rules.filter((_, j) => j !== i) })}>
                    <X />
                  </Button>
                </li>
              ))}
            </ul>
            <Button variant="ghost" size="xs" className="mt-2" onClick={() => set({ rules: [...f.rules, { type: "exclude_at_match", pattern: "" }] })}>
              <Plus /> Add a rule
            </Button>
          </div>
          {tested && <Tested t={tested} journal={journal} />}
          <div className="flex justify-end gap-2">
            <Button variant="ghost" onClick={onClose}>
              Cancel
            </Button>
            <Button variant="ghost" disabled={!ok || busy} onClick={() => void test()}>
              Test
            </Button>
            <Button type="submit" disabled={!ok || busy}>
              {source ? "Save" : "Add"}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}

/** A test's answer: the files matched on the server and how their last lines read. */
function Tested({ t, journal }: { t: LogSourceTest; journal: boolean }) {
  return (
    <div className="field-hint space-y-1" role="status">
      {t.error && <p className="text-[#e2c4c6]">{t.error}</p>}
      {!journal && t.matched > 0 && (
        <p>
          On the server it matches {count(t.matched)} {t.matched === 1 ? "file" : "files"}:{" "}
          <span className="font-mono">
            {t.files
              .slice(0, 3)
              .map((f) => `${f.path} (${formatBytes(f.bytes)})`)
              .join(", ")}
            {t.matched > 3 ? "…" : ""}
          </span>
        </p>
      )}
      {t.records.length > 0 && (
        <>
          <p>
            {journal ? "The journal's last entries" : "The newest file's last lines"} read as{t.excluded > 0 ? ` (${count(t.excluded)} left out by the rules)` : ""}:
          </p>
          <ul className="space-y-0.5 font-mono text-[11.5px]">
            {t.records.slice(-5).map((r, i) => (
              <li key={i} className="truncate" title={r.message}>
                <span className="text-[#8c8f98]">{r.status}</span> {r.service}: {r.message}
              </li>
            ))}
          </ul>
        </>
      )}
      {t.ok && t.records.length === 0 && <p>Nothing to read there yet.</p>}
    </div>
  );
}

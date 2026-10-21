/**
 * Log archives (docs/observe.md §10.5), Datadog's: the records kiln takes written to a bucket as
 * Datadog's Log Archives hold them (gzip'd JSON lines, a folder an hour), each record to the first
 * archive in order whose search takes it, every one once; each archive written with a cloud
 * integration's credentials. And their rehydrations (§10.6): an archive's records of a range read
 * back into a view of their own, searched in the logs explorer as the live logs are.
 */
import * as React from "react";
import { ArrowDown, ArrowUp, History, Pencil, Plus, Trash2 } from "lucide-react";
import { Link } from "@/lib/router";
import { observeApi, type Archive, type ArchiveDestination, type ArchiveIn, type ArchiveState, type CloudKind, type Rehydration, type RehydrationState } from "@/api/observe";
import { Button } from "@/components/ui/button";
import { ConfirmDialog, Dialog, DialogContent } from "@/components/ui/dialog";
import { TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { PageIntro, SectionHeading, SelectField, TextField } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { useRes } from "@/lib/data";
import { absoluteDate, formatBytes, relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { swapped } from "@/lib/utils";
import { archivesRes, cloudsRes, ObserveNav, rangeSaid, rehydrationSaid, rehydrationsRes } from "./parts";

const count = (n: number) => n.toLocaleString();
const TONE: Record<ArchiveState, string> = { WORKING: "succeeded", FAILING: "failed", UNKNOWN: "skipped" };
const STATE: Record<ArchiveState, string> = { WORKING: "Writing", FAILING: "Failing", UNKNOWN: "Waiting" };
const STORES = { s3: "S3", gcs: "Cloud Storage", azure: "Azure Storage" } as const;
type Store = keyof typeof STORES;
const KIND: Record<Store, CloudKind> = { s3: "aws", gcs: "gcp", azure: "azure" };
const CLOUD: Record<CloudKind, string> = { aws: "AWS", gcp: "Google Cloud", azure: "Azure" };
const STORAGE_CLASSES = ["STANDARD", "STANDARD_IA", "ONEZONE_IA", "INTELLIGENT_TIERING", "GLACIER_IR", "GLACIER", "DEEP_ARCHIVE"];
/** A bucket's, a container's or an account's name as object stores name them. */
const storeName = (v: string) => /^[A-Za-z0-9._-]{1,255}$/.test(v);

/** Where an archive writes, as its store's URLs say it. */
function where(d: ArchiveDestination): string {
  const path = (d.path ?? "").replace(/^\/+|\/+$/g, "");
  const tail = path ? `/${path}` : "";
  if (d.type === "s3") return `s3://${d.bucket}${tail}`;
  if (d.type === "gcs") return `gs://${d.bucket}${tail}`;
  return `${d.storage_account}/${d.container}${tail}`;
}

export default function Archives() {
  const [editing, setEditing] = React.useState<Archive | "new" | null>(null);
  // The archive a rehydration is asked of: its id, or "" for one chosen in the dialog.
  const [rehydrating, setRehydrating] = React.useState<string | null>(null);
  return (
    <div className="container-console page">
      <title>Archives · Observe · kiln</title>
      <PageIntro
        index={8}
        label="Observe"
        title="Archives"
        lead="The records kiln takes, written to your bucket as Datadog's Log Archives hold them: gzip'd JSON, a folder an hour, each record to the first archive whose search takes it, every one once. Each writes with a cloud integration's credentials."
        actions={
          <Button onClick={() => setEditing("new")}>
            <Plus /> New archive
          </Button>
        }
      />
      <ObserveNav />
      <Loadable label="the archives" fallback={<TableHold head={<Head />} rows={2} />}>
        <List onAdd={() => setEditing("new")} onEdit={setEditing} onRehydrate={(a) => setRehydrating(a.id)} />
      </Loadable>
      <section className="mt-12">
        <SectionHeading
          label="Rehydrations"
          title="Archived logs read back"
          aside={
            <Button variant="ghost" onClick={() => setRehydrating("")}>
              <History /> Rehydrate
            </Button>
          }
        />
        <Loadable label="the rehydrations" fallback={<TableHold head={<RehydrationsHead />} rows={1} />}>
          <Rehydrations onAdd={() => setRehydrating("")} />
        </Loadable>
      </section>
      {editing && (
        <Loadable label="the cloud integrations" fallback={null}>
          <ArchiveDialog archive={editing === "new" ? null : editing} onClose={() => setEditing(null)} />
        </Loadable>
      )}
      {rehydrating !== null && (
        <Loadable label="the archives" fallback={null}>
          <RehydrateDialog archiveId={rehydrating} onClose={() => setRehydrating(null)} />
        </Loadable>
      )}
    </div>
  );
}

function Head() {
  return (
    <tr>
      <th className="num w-1">#</th>
      <th>Archive</th>
      <th className="hidden md:table-cell">Writes to</th>
      <th>State</th>
      <th className="hidden sm:table-cell">Written</th>
      <th />
    </tr>
  );
}

function List({ onAdd, onEdit, onRehydrate }: { onAdd: () => void; onEdit: (a: Archive) => void; onRehydrate: (a: Archive) => void }) {
  const all = useRes(archivesRes).data;
  const [removing, setRemoving] = React.useState<Archive | null>(null);
  const act = async (what: string, f: () => Promise<unknown>) => {
    try {
      await f();
      invalidate("observe/archives");
    } catch (e) {
      toast({ title: `Couldn't ${what}`, description: errorMessage(e), tone: "failed" });
    }
  };
  const move = (i: number, by: -1 | 1) => {
    const j = i + by;
    if (j < 0 || j >= all.length) return;
    void act("move it", () =>
      observeApi.orderArchives(
        swapped(
          all.map((a) => a.id),
          i,
          j,
        ),
      ),
    );
  };
  if (all.length === 0)
    return (
      <p className="empty-line py-12 text-center">
        None yet.{" "}
        <button type="button" className="text-link" onClick={onAdd}>
          Make one
        </button>{" "}
        to keep your logs past their retention, in S3, Cloud Storage or Azure Storage.
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
            {all.map((a, i) => (
              <tr key={a.id}>
                <td className="num cell-mono w-1 text-[#8c8f98]">{i + 1}</td>
                <td className="truncate-cell">
                  <div className="truncate text-[13px] text-[#e4e5e7]">{a.name}</div>
                  <p className="truncate font-mono text-[11px] text-[#8c8f98]">{a.query?.trim() && a.query.trim() !== "*" ? a.query : "every record"}</p>
                  <p className="truncate font-mono text-[11px] text-[#8c8f98] md:hidden">{where(a.destination)}</p>
                </td>
                <td className="hidden md:table-cell">
                  <div className="max-w-[22rem] truncate font-mono text-[12px]" title={where(a.destination)}>
                    {where(a.destination)}
                  </div>
                  <p className="text-[12px] text-[#8c8f98]">
                    {STORES[a.destination.type]} · {a.destination.integration.cloud}
                    {a.include_tags ? " · with tags" : ""}
                  </p>
                </td>
                <td className="max-w-[20rem] sm:min-w-[13rem]">
                  <span className="status">
                    <i className="dot" data-status={TONE[a.status.state]} aria-hidden="true" />
                    <span className="status-text" data-status={TONE[a.status.state]}>
                      {STATE[a.status.state]}
                    </span>
                  </span>
                  {a.status.error && <p className="line-clamp-2 text-[12px] text-[#e2c4c6] [overflow-wrap:anywhere]">{a.status.error}</p>}
                  {a.status.pending > 0 && (
                    <p className="text-[12px] text-[#8c8f98]">
                      {count(a.status.pending)} {a.status.pending === 1 ? "file" : "files"} to write
                    </p>
                  )}
                </td>
                <td className="hidden whitespace-nowrap sm:table-cell">
                  {a.status.objects > 0 ? (
                    <>
                      {count(a.status.events)} {a.status.events === 1 ? "record" : "records"}
                      <div className="text-[12px] text-[#8c8f98]">
                        {count(a.status.objects)} {a.status.objects === 1 ? "object" : "objects"}, {formatBytes(a.status.bytes)}
                        {a.status.last_written ? `, ${relativeTime(a.status.last_written / 1000)}` : ""}
                      </div>
                    </>
                  ) : (
                    <span className="text-[#8c8f98]">None yet</span>
                  )}
                </td>
                <td className="w-1 whitespace-nowrap text-right">
                  <Button variant="ghost" size="xs" aria-label={`Move ${a.name} up`} disabled={i === 0} onClick={() => move(i, -1)}>
                    <ArrowUp />
                  </Button>
                  <Button variant="ghost" size="xs" aria-label={`Move ${a.name} down`} disabled={i === all.length - 1} onClick={() => move(i, 1)}>
                    <ArrowDown />
                  </Button>
                  <Button variant="ghost" size="xs" aria-label={`Rehydrate from ${a.name}`} title="Rehydrate: read a range of it back" onClick={() => onRehydrate(a)}>
                    <History />
                  </Button>
                  <Button variant="ghost" size="xs" aria-label={`Edit ${a.name}`} onClick={() => onEdit(a)}>
                    <Pencil />
                  </Button>
                  <Button variant="ghost-danger" size="xs" aria-label={`Remove ${a.name}`} onClick={() => setRemoving(a)}>
                    <Trash2 />
                  </Button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <p className="mt-3 text-[12.5px] text-[#8c8f98]">
        In this order, each record goes to the first archive whose search takes it; a change of order or of a search applies to the log files cut after it.
      </p>
      <ConfirmDialog
        open={removing !== null}
        onOpenChange={(o) => !o && setRemoving(null)}
        title={`Remove ${removing?.name ?? ""}?`}
        description="Records taken from now are not written to it; what it wrote stays in its bucket."
        confirmLabel="Remove"
        danger
        onConfirm={async () => {
          if (removing) await act("remove it", () => observeApi.deleteArchive(removing.id));
        }}
      />
    </>
  );
}

interface Form {
  name: string;
  query: string;
  store: Store;
  cloud: string;
  bucket: string;
  account: string;
  container: string;
  path: string;
  region: string;
  storageClass: string;
  encryption: "" | "NO_OVERRIDE" | "SSE_S3" | "SSE_KMS";
  key: string;
  endpoint: string;
  tags: boolean;
}

const formOf = (a: Archive | null): Form => {
  const d = a?.destination;
  return {
    name: a?.name ?? "",
    query: a?.query && a.query !== "*" ? a.query : "",
    store: d?.type ?? "s3",
    cloud: d?.integration.cloud ?? "",
    bucket: d && d.type !== "azure" ? d.bucket : "",
    account: d?.type === "azure" ? d.storage_account : "",
    container: d?.type === "azure" ? d.container : "",
    path: d?.path ?? "",
    region: d?.type === "s3" ? (d.region ?? "") : "",
    storageClass: d?.type === "s3" ? (d.storage_class ?? "") : "",
    encryption: d?.type === "s3" ? (d.encryption?.type ?? "") : "",
    key: d?.type === "s3" ? (d.encryption?.key ?? "") : "",
    endpoint: d?.endpoint ?? "",
    tags: a?.include_tags ?? false,
  };
};

/** The archive the form says, what it had besides (Datadog's own fields) kept. */
const bodyOf = (f: Form, was: Archive | null): ArchiveIn => {
  const integration = { ...(was?.destination.integration ?? {}), cloud: f.cloud };
  const opt = (v: string) => (v.trim() ? v.trim() : undefined);
  const common = { path: f.path.trim(), integration, endpoint: opt(f.endpoint) };
  const destination: ArchiveDestination =
    f.store === "s3"
      ? {
          type: "s3",
          bucket: f.bucket.trim(),
          ...common,
          region: opt(f.region),
          storage_class: opt(f.storageClass),
          encryption: f.encryption ? { type: f.encryption, ...(f.encryption === "SSE_KMS" ? { key: f.key.trim() } : {}) } : undefined,
        }
      : f.store === "gcs"
        ? { type: "gcs", bucket: f.bucket.trim(), ...common }
        : { type: "azure", storage_account: f.account.trim(), container: f.container.trim(), ...common };
  return {
    name: f.name.trim(),
    query: f.query.trim() || "*",
    destination,
    include_tags: f.tags,
    rehydration_tags: was?.rehydration_tags ?? [],
    ...(was?.rehydration_max_scan_size_in_gb !== undefined ? { rehydration_max_scan_size_in_gb: was.rehydration_max_scan_size_in_gb } : {}),
  };
};

function ArchiveDialog({ archive, onClose }: { archive: Archive | null; onClose: () => void }) {
  const clouds = useRes(cloudsRes).data;
  const [f, setF] = React.useState<Form>(() => formOf(archive));
  const [busy, setBusy] = React.useState(false);
  const set = (patch: Partial<Form>) => setF((old) => ({ ...old, ...patch }));
  const kind = KIND[f.store];
  const fitting = clouds.filter((c) => c.kind === kind);
  // The cloud chosen, or the only one of its kind there is.
  const cloud = fitting.some((c) => c.name === f.cloud) ? f.cloud : fitting.length === 1 ? (fitting[0]?.name ?? "") : "";
  const placed = f.store === "azure" ? storeName(f.account.trim()) && storeName(f.container.trim()) : storeName(f.bucket.trim());
  const prefix = f.path.replace(/^\/+|\/+$/g, "");
  const ok = f.name.trim() !== "" && cloud !== "" && placed && !(f.encryption === "SSE_KMS" && !f.key.trim()) && !f.path.split("/").includes("..");
  const save = async () => {
    setBusy(true);
    try {
      const body = bodyOf({ ...f, cloud }, archive);
      const saved = archive ? await observeApi.updateArchive(archive.id, body) : await observeApi.createArchive(body);
      invalidate("observe/archives");
      toast({ title: archive ? "Saved" : "Made", description: `${saved.name}: the log files cut from now are written to ${where(saved.destination)}.`, tone: "succeeded" });
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
        title={archive ? `Edit ${archive.name}` : "New archive"}
        description={
          archive ? "A change of its search or tags applies to the log files cut after it; of where it writes, to its next object." : "It takes the records of the log files cut from now on."
        }
      >
        <form
          className="space-y-4"
          onSubmit={(e) => {
            e.preventDefault();
            if (ok) void save();
          }}
        >
          <TextField label="Name" value={f.name} onChange={(e) => set({ name: e.target.value })} placeholder="everything, a year" required />
          <TextField
            label="Of the records"
            value={f.query}
            onChange={(e) => set({ query: e.target.value })}
            placeholder="service:checkout"
            className="font-mono"
            spellCheck={false}
            hint="A log search; empty, every record (kiln's own events only if it names source:kiln.event)."
          />
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
            <SelectField
              label="Store"
              value={f.store}
              onChange={(v) => set({ store: v === "gcs" || v === "azure" ? v : "s3" })}
              options={Object.entries(STORES).map(([value, label]) => ({ value, label }))}
            />
            {fitting.length > 0 ? (
              <SelectField
                label={`${CLOUD[kind]} integration`}
                value={cloud}
                onChange={(v) => set({ cloud: v })}
                options={[...(cloud ? [] : [{ value: "", label: "Choose one" }]), ...fitting.map((c) => ({ value: c.name, label: c.name }))]}
              />
            ) : (
              <div>
                <div className="field-label">{CLOUD[kind]} integration</div>
                <p className="field-hint">
                  None yet: <Link to="/observe/integrations">make one in Integrations</Link>, paused if only archives write with it.
                </p>
              </div>
            )}
          </div>
          {f.store === "azure" ? (
            <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
              <TextField label="Storage account" value={f.account} onChange={(e) => set({ account: e.target.value })} placeholder="mylogs" spellCheck={false} />
              <TextField label="Container" value={f.container} onChange={(e) => set({ container: e.target.value })} placeholder="archives" spellCheck={false} />
            </div>
          ) : (
            <TextField label="Bucket" value={f.bucket} onChange={(e) => set({ bucket: e.target.value })} placeholder="my-logs-archive" spellCheck={false} />
          )}
          <TextField
            label="Path"
            value={f.path}
            onChange={(e) => set({ path: e.target.value })}
            placeholder="/kiln"
            className="font-mono"
            spellCheck={false}
            hint={`Its objects: ${prefix ? `${prefix}/` : ""}dt=YYYYMMDD/hour=HH/archive_….json.gz`}
          />
          {f.store === "s3" && (
            <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
              <TextField label="Region" value={f.region} onChange={(e) => set({ region: e.target.value })} placeholder="the integration's" spellCheck={false} />
              <SelectField
                label="Storage class"
                value={f.storageClass}
                onChange={(v) => set({ storageClass: v })}
                options={[{ value: "", label: "The bucket's" }, ...STORAGE_CLASSES.map((c) => ({ value: c, label: c }))]}
              />
              <SelectField
                label="Encryption"
                value={f.encryption}
                onChange={(v) => set({ encryption: v === "NO_OVERRIDE" || v === "SSE_S3" || v === "SSE_KMS" ? v : "" })}
                options={[
                  { value: "", label: "The bucket's" },
                  { value: "SSE_S3", label: "S3's keys (SSE-S3)" },
                  { value: "SSE_KMS", label: "A KMS key (SSE-KMS)" },
                ]}
              />
              {f.encryption === "SSE_KMS" && <TextField label="KMS key" value={f.key} onChange={(e) => set({ key: e.target.value })} placeholder="arn:aws:kms:…" spellCheck={false} />}
            </div>
          )}
          <TextField
            label="Endpoint"
            value={f.endpoint}
            onChange={(e) => set({ endpoint: e.target.value })}
            placeholder={f.store === "s3" ? "AWS's" : f.store === "gcs" ? "Google's" : `https://${f.account.trim() || "<account>"}.blob.core.windows.net`}
            spellCheck={false}
            hint={f.store === "s3" ? "Another S3-compatible store's address (MinIO, R2, …): objects at <endpoint>/<bucket>/<key>." : "Another address of the store's (an emulator's)."}
          />
          <label className="flex items-center gap-2 text-[13px] text-[#c8cad0]">
            <input type="checkbox" className="check" checked={f.tags} onChange={(e) => set({ tags: e.target.checked })} /> Events carry their tags (service, host, source, env, version)
          </label>
          <div className="flex justify-end gap-2">
            <Button variant="ghost" onClick={onClose}>
              Cancel
            </Button>
            <Button type="submit" disabled={!ok || busy}>
              {archive ? "Save" : "Make it"}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}

const R_TONE: Record<RehydrationState, string> = { queued: "queued", reading: "running", done: "succeeded", failed: "failed" };
const R_STATE: Record<RehydrationState, string> = { queued: "Waiting", reading: "Reading", done: "Read", failed: "Failed" };

/** What a rehydration read, for its row: the records it kept once read (on wider screens, of how many read, from how much), else how its reading goes. */
function ReadSaid({ r }: { r: Rehydration }) {
  if (r.state !== "done") return <>{rehydrationSaid(r)}</>;
  const n = (x: number) => x.toLocaleString();
  const records = r.records === 1 ? "record" : "records";
  return (
    <>
      <span className="whitespace-nowrap sm:hidden">
        {n(r.records)} {records}
      </span>
      <span className="hidden sm:inline">
        {n(r.records)} of {n(r.events_read)} {r.events_read === 1 ? "record" : "records"} ·{" "}
        <span className="whitespace-nowrap">
          {n(r.objects_read)} {r.objects_read === 1 ? "object" : "objects"}, {formatBytes(r.bytes_read)}
        </span>
      </span>
      {r.limited && " · stopped at its bound"}
    </>
  );
}

function RehydrationsHead() {
  return (
    <tr>
      <th>Rehydration</th>
      <th className="hidden md:table-cell">Range</th>
      <th>State</th>
      <th className="hidden sm:table-cell">Kept</th>
      <th />
    </tr>
  );
}

/** Each rehydration: what it reads, how its reading goes, until when its view is kept; its view a click from the logs explorer. */
function Rehydrations({ onAdd }: { onAdd: () => void }) {
  const all = useRes(rehydrationsRes).data;
  const [removing, setRemoving] = React.useState<Rehydration | null>(null);
  if (all.length === 0)
    return (
      <p className="empty-line py-10 text-center">
        None yet.{" "}
        <button type="button" className="text-link" onClick={onAdd}>
          Rehydrate
        </button>{" "}
        an archive's records of a time range to search them as the live logs are, for as long as you keep them.
      </p>
    );
  return (
    <>
      <div className="table-wrap">
        <table className="data-table">
          <thead>
            <RehydrationsHead />
          </thead>
          <tbody>
            {all.map((r) => (
              <tr key={r.id}>
                <td className="truncate-cell">
                  <div className="truncate text-[13px] text-[#e4e5e7]">{r.name}</div>
                  <p className="truncate font-mono text-[11px] text-[#8c8f98]">
                    {r.query.trim() && r.query.trim() !== "*" ? r.query : "every record"} · {r.archive}
                  </p>
                  <p className="truncate text-[11px] text-[#8c8f98] md:hidden">{rangeSaid(r.from, r.to)}</p>
                </td>
                <td className="hidden text-[12.5px] whitespace-nowrap md:table-cell">{rangeSaid(r.from, r.to)}</td>
                <td className="max-w-[22rem] sm:min-w-[13rem] md:min-w-[17rem]">
                  <span className="status">
                    <i className="dot" data-status={R_TONE[r.state]} aria-hidden="true" />
                    <span className="status-text" data-status={R_TONE[r.state]}>
                      {R_STATE[r.state]}
                    </span>
                  </span>
                  {r.state === "failed" ? (
                    <p className="line-clamp-2 text-[12px] text-[#e2c4c6] [overflow-wrap:anywhere]">{r.error ?? ""}</p>
                  ) : (
                    <p className="text-[12px] text-[#8c8f98]">
                      <ReadSaid r={r} />
                    </p>
                  )}
                </td>
                <td className="hidden text-[12.5px] whitespace-nowrap text-[#8c8f98] sm:table-cell">
                  {r.state === "done" || r.state === "failed" ? `until ${absoluteDate(r.expires / 1000)}` : `${r.retention_days} days once read`}
                </td>
                <td className="w-1 whitespace-nowrap text-right">
                  {(r.state === "done" || r.state === "reading") && (
                    <Button variant="ghost" size="xs" asChild>
                      <Link to={`/observe/logs?rehydration=${encodeURIComponent(r.id)}`} aria-label={`Search ${r.name}`}>
                        Search
                      </Link>
                    </Button>
                  )}
                  <Button variant="ghost-danger" size="xs" aria-label={`Remove ${r.name}`} onClick={() => setRemoving(r)}>
                    <Trash2 />
                  </Button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <ConfirmDialog
        open={removing !== null}
        onOpenChange={(o) => !o && setRemoving(null)}
        title={`Remove ${removing?.name ?? ""}?`}
        description="Its view goes with it (its reading stops, if it is read); the archive keeps every record."
        confirmLabel="Remove"
        danger
        onConfirm={async () => {
          if (!removing) return;
          try {
            await observeApi.deleteRehydration(removing.id);
            invalidate("observe/rehydrations");
          } catch (e) {
            toast({ title: "Couldn't remove it", description: errorMessage(e), tone: "failed" });
          }
        }}
      />
    </>
  );
}

/** `ms` as a datetime-local input's value, in this browser's timezone. */
const localInput = (ms: number) => new Date(ms - new Date(ms).getTimezoneOffset() * 60_000).toISOString().slice(0, 16);

/** A rehydration asked of an archive: which records (a search), of which range, kept how long, read how far at the most. */
function RehydrateDialog({ archiveId, onClose }: { archiveId: string; onClose: () => void }) {
  const archives = useRes(archivesRes).data;
  const [f, setF] = React.useState(() => {
    const now = Date.now();
    return { name: "", archive: archiveId || (archives[0]?.id ?? ""), query: "", from: localInput(now - 86_400_000), to: localInput(now), days: "15", gb: "" };
  });
  const [busy, setBusy] = React.useState(false);
  const set = (patch: Partial<typeof f>) => setF((old) => ({ ...old, ...patch }));
  // A time picked to the minute: "to 15:54" holds 15:54's records too.
  const from = new Date(f.from).getTime();
  const to = new Date(f.to).getTime() + 60_000;
  const days = Number(f.days);
  const gb = f.gb.trim() ? Number(f.gb) : undefined;
  // The archive's own bound, which bounds what is asked too.
  const bound = archives.find((a) => a.id === f.archive)?.rehydration_max_scan_size_in_gb;
  const ranged = Number.isFinite(from) && Number.isFinite(to) && to > from;
  const ok = f.name.trim() !== "" && f.archive !== "" && ranged && Number.isInteger(days) && days >= 1 && days <= 180 && (gb === undefined || (Number.isFinite(gb) && gb > 0));
  const save = async () => {
    setBusy(true);
    try {
      const made = await observeApi.createRehydration({
        name: f.name.trim(),
        archive_id: f.archive,
        query: f.query.trim() || "*",
        from,
        to,
        retention_days: days,
        ...(gb !== undefined ? { max_scan_size_in_gb: gb } : {}),
      });
      invalidate("observe/rehydrations");
      toast({ title: "Asked", description: `${made.name}: its records are read from ${made.archive}, and searched in the logs explorer as they come.`, tone: "succeeded" });
      onClose();
    } catch (err) {
      toast({ title: "Couldn't rehydrate", description: errorMessage(err), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent
        title="Rehydrate"
        description="An archive's records of a time range, read back into a view of their own: searched, counted and faceted as the live logs are, until it is removed or its days are up."
      >
        {archives.length === 0 ? (
          <p className="field-hint">No archive yet: make one first, and its records can be read back once it has written them.</p>
        ) : (
          <form
            className="space-y-4"
            onSubmit={(e) => {
              e.preventDefault();
              if (ok) void save();
            }}
          >
            <TextField label="Name" value={f.name} onChange={(e) => set({ name: e.target.value })} placeholder="checkout errors, last March" required />
            <SelectField label="From the archive" value={f.archive} onChange={(v) => set({ archive: v })} options={archives.map((a) => ({ value: a.id, label: a.name }))} />
            <TextField
              label="Of the records"
              value={f.query}
              onChange={(e) => set({ query: e.target.value })}
              placeholder="service:checkout status:error"
              className="font-mono"
              spellCheck={false}
              hint="A log search; empty, every record of the range."
            />
            <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
              <TextField label="From" type="datetime-local" value={f.from} onChange={(e) => set({ from: e.target.value })} />
              <TextField label="To" type="datetime-local" value={f.to} onChange={(e) => set({ to: e.target.value })} hint={ranged ? undefined : "After it starts"} />
            </div>
            <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
              <TextField label="Kept for (days)" type="number" min={1} max={180} value={f.days} onChange={(e) => set({ days: e.target.value })} hint="1 to 180." />
              <TextField
                label="Read at most (GB)"
                type="number"
                min={0}
                step="any"
                value={f.gb}
                onChange={(e) => set({ gb: e.target.value })}
                placeholder={bound === undefined ? "no bound" : `${bound} (the archive's)`}
                hint="Of its objects, gzip'd: past it, it stops."
              />
            </div>
            <div className="flex justify-end gap-2">
              <Button variant="ghost" onClick={onClose}>
                Cancel
              </Button>
              <Button type="submit" disabled={!ok || busy}>
                Rehydrate
              </Button>
            </div>
          </form>
        )}
      </DialogContent>
    </Dialog>
  );
}

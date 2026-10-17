/**
 * One app: where it deploys, whether it matches git, what runs and what differs, and its
 * deployments. Sync, refresh, take over (for apps adopted from Argo CD), stop and delete are
 * here; each says what it will do before it does it.
 */
import * as React from "react";
import { Link, useNavigate, useParams } from "react-router";
import { RefreshCw, Rocket } from "lucide-react";

import { cdApi } from "@/api/cd";
import { useServerEvents } from "@/api/events";
import type { CdAppDetail, CdResource } from "@/api/types";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/dialog";
import { EmptyState, ErrorState, SkeletonRows } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { errorMessage, useResource } from "@/lib/hooks";
import { useWindowedRows } from "@/lib/virtual";
import { FileDiffView } from "@/views/harness/diff";
import { RelTime } from "@/views/shared";
import {
  HealthPair,
  StatePair,
  SyncPair,
  TRIGGER_WORD,
  objectKey,
  objectLabel,
  opTone,
  opWords,
  placeOf,
  running,
  shortRev,
} from "./parts";
import "./deployments.css";

const RES_ROW = 56;

type Tab = "resources" | "diff" | "history";

/** A fresh page per app, so nothing of one app shows while the next one loads. */
export default function DeploymentAppRoute() {
  const { project = "", name = "" } = useParams();
  return <DeploymentApp key={`${project}/${name}`} project={project} name={name} />;
}

function DeploymentApp({ project, name }: { project: string; name: string }) {
  const id = `${project}/${name}`;
  const navigate = useNavigate();
  const app = useResource((s) => cdApi.app(id, s), [id], { pollMs: 10_000 });
  const res = useResource((s) => cdApi.resources(id, s), [id], { pollMs: 10_000 });
  const [tab, setTab] = React.useState<Tab>("resources");
  // Bumped when this app's status changes, so the open tab reads it again.
  const [tick, setTick] = React.useState(0);
  const [confirm, setConfirm] = React.useState<null | "sync" | "takeover" | "delete">(null);
  const [prune, setPrune] = React.useState(false);
  const [dryRun, setDryRun] = React.useState(false);
  const [orphan, setOrphan] = React.useState(false);
  const [busy, setBusy] = React.useState(false);

  // Live: this app's status changed, so read it again (at most twice a second).
  const soon = React.useRef<number | null>(null);
  useServerEvents((ev) => {
    if (ev.type === "cd_apps" && soon.current === null && ev.apps.some((a) => a.app === id)) {
      soon.current = window.setTimeout(() => {
        soon.current = null;
        app.reload();
        res.reload();
        setTick((t) => t + 1);
      }, 500);
    } else if (ev.type === "cd_app_deleted" && ev.app === id) {
      navigate("/deployments", { replace: true });
    }
  });
  React.useEffect(
    () => () => {
      if (soon.current !== null) clearTimeout(soon.current);
    },
    [],
  );

  const act = async (done: string, f: () => Promise<unknown>) => {
    setBusy(true);
    try {
      await f();
      toast({ title: done, tone: "info" });
      app.reload();
    } catch (e) {
      toast({ title: "That didn't work", description: errorMessage(e), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };

  if (app.error && !app.data) {
    return (
      <div className="container-console page cd-page">
        <EmptyState icon={<Rocket size={20} />} title="No such app." action={<Link to="/deployments">All deployments</Link>}>
          {app.error.message}
        </EmptyState>
      </div>
    );
  }

  const a = app.data;
  const op = a?.operation ?? null;
  const live = running(op);
  const resources = res.data?.resources ?? [];
  const objects = resources.filter((r) => !r.hook).length;
  const differ = resources.filter((r) => !r.hook && r.sync !== "Synced").length;
  const tabs: [Tab, string][] = [
    ["resources", objects ? `Resources ${objects}` : "Resources"],
    ["diff", differ ? `Diff ${differ}` : "Diff"],
    ["history", "History"],
  ];

  return (
    <div className="container-console page cd-page">
      <header className="cd-head">
        <nav className="eyebrow cd-crumbs" aria-label="Breadcrumb">
          <span className="spectrum-rule" />
          <Link to="/deployments">03 / Deployments</Link>
          <span aria-hidden="true">/</span>
          <span>{project}</span>
        </nav>
        <h1 className="cd-title">{name}</h1>
        {a ? (
          <>
            <div className="cd-state">
              <SyncPair sync={a.sync} />
              <HealthPair health={a.health} />
              <span className="cd-meta">{[placeOf(a), shortRev(a.revision)].filter(Boolean).join(" · ")}</span>
            </div>
            <p className="cd-from">{fromLine(a)}</p>
            {a.shadow && (
              <p className="cd-note">
                Shadowed from Argo CD: kiln compares it and never writes to it. Take it over to deploy it with kiln.
              </p>
            )}
            <div className="cd-actions">
              {a.shadow ? (
                <Button size="sm" onClick={() => setConfirm("takeover")} disabled={busy}>
                  Take over
                </Button>
              ) : (
                <Button size="sm" onClick={() => setConfirm("sync")} disabled={busy || live}>
                  Sync
                </Button>
              )}
              <Button size="sm" variant="secondary" onClick={() => act("Reading git again", () => cdApi.refresh(id, false))} disabled={busy}>
                <RefreshCw aria-hidden="true" /> Refresh
              </Button>
              {live && (
                <Button size="sm" variant="secondary" onClick={() => act("Stopping the sync", () => cdApi.terminate(id))} disabled={busy}>
                  Stop
                </Button>
              )}
              <Button size="sm" variant="ghost" onClick={() => setConfirm("delete")} disabled={busy}>
                Delete
              </Button>
            </div>
          </>
        ) : (
          <SkeletonRows rows={2} cols={3} />
        )}
      </header>

      {op && (
        <section className="cd-op" data-tone={opTone(op)} aria-live="polite">
          <span className="cd-op-label">{live ? "Now" : "Last sync"}</span>
          <StatePair tone={opTone(op)} word={opWords(op)} />
          {op.message && <span className="cd-op-msg">{op.message}</span>}
          {op.attempt > 1 && <span className="cd-meta">attempt {op.attempt}</span>}
        </section>
      )}
      {a?.source_error && <p className="cd-warn">kiln can't read its sources: {a.source_error}</p>}
      {a?.message && <p className="cd-warn">{a.message}</p>}

      <div className="cd-tabs" role="tablist" aria-label="App">
        {tabs.map(([key, label]) => (
          <button
            key={key}
            id={`cd-tab-${key}`}
            type="button"
            role="tab"
            aria-selected={tab === key}
            aria-controls="cd-tab-panel"
            className="cd-tab"
            onClick={() => setTab(key)}
          >
            {label}
          </button>
        ))}
      </div>

      <div id="cd-tab-panel" role="tabpanel" aria-labelledby={`cd-tab-${tab}`} className="cd-panel">
        {tab === "resources" &&
          (res.error && !res.data ? (
            <ErrorState error={res.error} onRetry={res.reload} compact />
          ) : !res.data ? (
            <SkeletonRows rows={6} cols={3} />
          ) : resources.length === 0 ? (
            <p className="cd-empty">This app renders no resources.</p>
          ) : (
            <ResourceList list={resources} />
          ))}
        {tab === "diff" && <DiffTab id={id} tick={tick} />}
        {tab === "history" && <HistoryTab id={id} tick={tick} />}
      </div>

      <ConfirmDialog
        open={confirm === "sync"}
        onOpenChange={(o) => !o && setConfirm(null)}
        title={`Sync ${name}?`}
        description={
          <>
            {dryRun
              ? "Checks every object against the cluster without changing anything."
              : `Applies what git says to ${a ? placeOf(a) : "its cluster"}, wave by wave, running its hooks.`}
            <label className="cd-check">
              <input type="checkbox" checked={prune} onChange={(e) => setPrune(e.target.checked)} />
              Also delete what git no longer holds (prune)
            </label>
            <label className="cd-check">
              <input type="checkbox" checked={dryRun} onChange={(e) => setDryRun(e.target.checked)} />
              Only check (dry run)
            </label>
          </>
        }
        confirmLabel={dryRun ? "Check" : "Sync"}
        onConfirm={() => act(dryRun ? `Checking ${name}` : `Syncing ${name}`, () => cdApi.sync(id, { prune, dry_run: dryRun }))}
      />
      <ConfirmDialog
        open={confirm === "takeover"}
        onOpenChange={(o) => !o && setConfirm(null)}
        title={`Take over ${name}?`}
        description="kiln stops shadowing this app and deploys it from now on. Turn off auto-sync for it in Argo CD first, so the two don't both apply it. Nothing is recreated."
        confirmLabel="Take over"
        onConfirm={() => act(`kiln deploys ${name} now`, () => cdApi.takeOver(id))}
      />
      <ConfirmDialog
        open={confirm === "delete"}
        onOpenChange={(o) => !o && setConfirm(null)}
        title={`Delete ${name}?`}
        description={
          a?.shadow ? (
            "kiln forgets it. Argo CD still deploys it, and nothing in the cluster changes."
          ) : (
            <>
              {orphan
                ? "kiln forgets it; its objects keep running in the cluster."
                : "Its objects are deleted from the cluster too, except those marked Delete=false and namespaces it created."}
              <label className="cd-check">
                <input type="checkbox" checked={orphan} onChange={(e) => setOrphan(e.target.checked)} />
                Leave its objects running
              </label>
            </>
          )
        }
        confirmLabel="Delete"
        danger
        onConfirm={() =>
          act(`Deleting ${name}`, async () => {
            await cdApi.remove(id, a?.shadow || orphan ? "orphan" : "foreground");
            navigate("/deployments", { replace: true });
          })
        }
      />
    </div>
  );
}

/** Where an app comes from and how it syncs, in one line. */
function fromLine(a: CdAppDetail) {
  const src = a.definition?.sources?.[0];
  const where = src ? [src.chart ? `${src.repo} ${src.chart}` : src.repo, src.path].filter(Boolean).join(" · ") : "";
  const at = src?.revision ? ` @ ${src.revision}` : "";
  const more = (a.definition?.sources?.length ?? 0) > 1 ? ` (+${(a.definition?.sources?.length ?? 1) - 1} more)` : "";
  const sync = a.definition?.sync;
  const how = sync?.auto ? ["Auto-sync", sync.prune && "prune", sync.self_heal && "self-heal"].filter(Boolean).join(", ") : "Synced by hand";
  return [where && `From ${where}${at}${more}`, how].filter(Boolean).join(" · ");
}

/** What differs first, then what is unhealthy, then the rest by kind and name; hooks last. */
function sortResources(list: CdResource[]) {
  const rank = (r: CdResource) => (r.hook ? 3 : r.sync !== "Synced" ? 0 : r.health && r.health !== "Healthy" ? 1 : 2);
  return [...list].sort((x, y) => rank(x) - rank(y) || x.kind.localeCompare(y.kind) || x.name.localeCompare(y.name));
}

/** The app's objects, windowed: an app of ten thousand objects draws what fits on screen. */
function ResourceList({ list }: { list: CdResource[] }) {
  const ref = React.useRef<HTMLDivElement>(null);
  const sorted = React.useMemo(() => sortResources(list), [list]);
  const win = useWindowedRows(ref, sorted.length, RES_ROW);
  return (
    <div ref={ref} className="cd-resources" role="list">
      <div style={{ height: win.start * RES_ROW }} aria-hidden="true" />
      {sorted.slice(win.start, win.end).map((r, i) => (
        <ResourceRow key={objectKey(r)} r={r} pos={win.start + i + 1} size={sorted.length} />
      ))}
      <div style={{ height: (sorted.length - win.end) * RES_ROW }} aria-hidden="true" />
    </div>
  );
}

function ResourceRow({ r, pos, size }: { r: CdResource; pos: number; size: number }) {
  const note = r.why ?? r.message ?? (r.requires_pruning ? "Not in git any more: a sync with prune deletes it" : r.missing ? "Not in the cluster yet" : "");
  return (
    <div
      className="cd-res"
      role="listitem"
      aria-posinset={pos}
      aria-setsize={size}
      data-tone={r.health === "Degraded" ? "fail" : !r.hook && r.sync === "OutOfSync" ? "warn" : undefined}
    >
      <div className="cd-res-main">
        <span className="cd-res-name">{r.name}</span>
        <span className="cd-res-kind">
          {r.kind}
          {r.namespace ? ` · ${r.namespace}` : ""}
          {r.hook ? " · hook" : ""}
        </span>
      </div>
      <div className="cd-res-why" title={note}>
        {note}
      </div>
      <div className="cd-row-states">
        {!r.hook && <SyncPair sync={r.sync} />}
        <HealthPair health={r.health} />
      </div>
    </div>
  );
}

function DiffTab({ id, tick }: { id: string; tick: number }) {
  const diff = useResource((s) => cdApi.diff(id, s), [id, tick]);
  if (diff.error && !diff.data) return <ErrorState error={diff.error} onRetry={diff.reload} compact />;
  if (!diff.data) return <SkeletonRows rows={4} cols={2} />;
  const list = diff.data.diffs;
  if (list.length === 0) return <p className="cd-empty">Nothing differs: the cluster runs what git says.</p>;
  return (
    <div className="cd-diffs">
      <p className="cd-meta">The cluster on the left, git on the right. Fields the cluster fills in itself are left out.</p>
      {list.map((d) => (
        <div key={objectKey(d.key)} className="cd-diff">
          {d.why && <p className="cd-why">{d.why}</p>}
          <FileDiffView diff={{ path: objectLabel(d.key), hunks: d.hunks }} label={objectLabel(d.key)} defaultOpen={list.length <= 4} />
        </div>
      ))}
    </div>
  );
}

function HistoryTab({ id, tick }: { id: string; tick: number }) {
  const history = useResource((s) => cdApi.history(id, s), [id, tick]);
  if (history.error && !history.data) return <ErrorState error={history.error} onRetry={history.reload} compact />;
  if (!history.data) return <SkeletonRows rows={4} cols={3} />;
  if (history.data.history.length === 0) return <p className="cd-empty">No deployments yet.</p>;
  return (
    <div className="cd-history" role="list">
      {history.data.history.map((h) => (
        <div key={`${h.at_ms}-${h.op}`} className="cd-history-row" role="listitem">
          <StatePair tone={h.succeeded ? "succeeded" : "failed"} word={h.succeeded ? "Deployed" : "Failed"} />
          <span className="cd-mono">{shortRev(h.revision) || "no revision"}</span>
          <span className="cd-history-what">
            {TRIGGER_WORD[h.trigger] ?? h.trigger}
            {h.by ? ` by ${h.by}` : ""} · {h.applied} applied{h.pruned ? ` · ${h.pruned} pruned` : ""}
          </span>
          <RelTime ts={Math.floor(h.at_ms / 1000)} className="cd-meta cd-history-at" />
          {!h.succeeded && h.message && <p className="cd-history-msg">{h.message}</p>}
        </div>
      ))}
    </div>
  );
}

/**
 * One app: where it deploys, whether it matches git, what runs and what differs, and its
 * deployments. Sync, refresh, rollback, take over (for apps adopted from Argo CD), stop and
 * delete are here, all in the page: an action that can't be undone first says, in place, what it
 * will do; what happened shows below the actions. A select chooses what the page lists.
 */
import * as React from "react";
import { Link, useNavigate, useParams } from "react-router";
import { RefreshCw, Rocket } from "lucide-react";

import { cdApi } from "@/api/cd";
import { useServerEvents } from "@/api/events";
import type { CdAppDetail, CdDeployment, CdResource } from "@/api/types";
import { Button } from "@/components/ui/button";
import { EmptyState, ErrorState, SelectField, SkeletonRows } from "@/components/ui/misc";
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

type Show = "resources" | "diff" | "history";
type Ask = null | "takeover" | "delete" | "follow";
type Notice = { tone: "ok" | "fail"; text: string } | null;

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
  const [show, setShow] = React.useState<Show>("resources");
  // Bumped when this app's status changes, so what is shown is read again.
  const [tick, setTick] = React.useState(0);
  const [ask, setAsk] = React.useState<Ask>(null);
  const [prune, setPrune] = React.useState(false);
  const [dryRun, setDryRun] = React.useState(false);
  const [orphan, setOrphan] = React.useState(false);
  const [busy, setBusy] = React.useState(false);
  const [notice, setNotice] = React.useState<Notice>(null);

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
  // A success says so for a while; a failure stays until the next action.
  React.useEffect(() => {
    if (notice?.tone !== "ok") return;
    const t = setTimeout(() => setNotice(null), 8_000);
    return () => clearTimeout(t);
  }, [notice]);

  const act = async (done: string, f: () => Promise<unknown>) => {
    setBusy(true);
    setNotice(null);
    try {
      await f();
      setNotice({ tone: "ok", text: done });
      setAsk(null);
      app.reload();
      setTick((t) => t + 1);
    } catch (e) {
      setNotice({ tone: "fail", text: errorMessage(e) });
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
  const shows = [
    { value: "resources", label: objects ? `Resources (${objects})` : "Resources" },
    { value: "diff", label: differ ? `What differs (${differ})` : "What differs" },
    { value: "history", label: "History" },
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
            {a.pinned && (
              <p className="cd-note">
                Rolled back to deployment {a.pinned.to}
                {a.pinned.revision ? ` (${shortRev(a.pinned.revision)})` : ""}
                {a.pinned.by ? ` by ${a.pinned.by}` : ""}, <RelTime ts={Math.floor(a.pinned.at_ms / 1000)} />. kiln holds it
                there until git changes this app, then follows git again.
              </p>
            )}
            <div className="cd-actions">
              {a.shadow ? (
                <Button size="xs" onClick={() => setAsk(ask === "takeover" ? null : "takeover")} aria-expanded={ask === "takeover"} disabled={busy}>
                  Take over…
                </Button>
              ) : (
                <>
                  <Button
                    size="xs"
                    onClick={() => act(dryRun ? "Checking every object against the cluster." : "Syncing.", () => cdApi.sync(id, { prune, dry_run: dryRun }))}
                    disabled={busy || live}
                  >
                    {dryRun ? "Check" : "Sync"}
                  </Button>
                  <label className="cd-opt">
                    <input type="checkbox" checked={prune} onChange={(e) => setPrune(e.target.checked)} /> Prune
                  </label>
                  <label className="cd-opt">
                    <input type="checkbox" checked={dryRun} onChange={(e) => setDryRun(e.target.checked)} /> Dry run
                  </label>
                </>
              )}
              <Button size="xs" variant="secondary" onClick={() => act("Reading git again.", () => cdApi.refresh(id, false))} disabled={busy}>
                <RefreshCw aria-hidden="true" /> Refresh
              </Button>
              {live && (
                <Button size="xs" variant="secondary" onClick={() => act("Stopping the sync.", () => cdApi.terminate(id))} disabled={busy}>
                  Stop
                </Button>
              )}
              {a.pinned && (
                <Button size="xs" variant="secondary" onClick={() => setAsk(ask === "follow" ? null : "follow")} aria-expanded={ask === "follow"} disabled={busy}>
                  Follow git…
                </Button>
              )}
              <Button size="xs" variant="ghost" onClick={() => setAsk(ask === "delete" ? null : "delete")} aria-expanded={ask === "delete"} disabled={busy}>
                Delete…
              </Button>
            </div>
            {ask && (
              <div className="cd-confirm" role="group" aria-label="Confirm">
                {ask === "takeover" && (
                  <>
                    <p>
                      kiln stops shadowing {name} and deploys it from now on. Turn off auto-sync for it in Argo CD first, so the two
                      don't both apply it. Nothing is recreated.
                    </p>
                    <div className="cd-confirm-actions">
                      <Button size="xs" onClick={() => act(`kiln deploys ${name} now.`, () => cdApi.takeOver(id))} disabled={busy}>
                        Take over {name}
                      </Button>
                      <Button size="xs" variant="ghost" onClick={() => setAsk(null)}>
                        Cancel
                      </Button>
                    </div>
                  </>
                )}
                {ask === "follow" && (
                  <>
                    <p>
                      kiln renders what git says now
                      {a.auto ? " and deploys it: this app syncs automatically." : ". Sync to deploy it."}
                    </p>
                    <div className="cd-confirm-actions">
                      <Button size="xs" onClick={() => act(`${name} follows git again.`, () => cdApi.follow(id))} disabled={busy}>
                        Follow git
                      </Button>
                      <Button size="xs" variant="ghost" onClick={() => setAsk(null)}>
                        Cancel
                      </Button>
                    </div>
                  </>
                )}
                {ask === "delete" && (
                  <>
                    <p>
                      {a.shadow
                        ? "kiln forgets it. Argo CD still deploys it, and nothing in the cluster changes."
                        : orphan
                          ? "kiln forgets it; its objects keep running in the cluster."
                          : `Its objects are deleted from ${placeOf(a)} too, except those marked Delete=false and namespaces it created.`}
                    </p>
                    {!a.shadow && (
                      <label className="cd-opt">
                        <input type="checkbox" checked={orphan} onChange={(e) => setOrphan(e.target.checked)} /> Leave its objects running
                      </label>
                    )}
                    <div className="cd-confirm-actions">
                      <Button
                        size="xs"
                        variant="danger"
                        onClick={() =>
                          act(`Deleting ${name}.`, async () => {
                            await cdApi.remove(id, a.shadow || orphan ? "orphan" : "foreground");
                            navigate("/deployments", { replace: true });
                          })
                        }
                        disabled={busy}
                      >
                        Delete {name}
                      </Button>
                      <Button size="xs" variant="ghost" onClick={() => setAsk(null)}>
                        Cancel
                      </Button>
                    </div>
                  </>
                )}
              </div>
            )}
            {notice && (
              <p className="cd-notice" data-tone={notice.tone} aria-live="polite">
                {notice.text}
              </p>
            )}
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

      <div className="cd-show">
        <SelectField label="Show" value={show} onChange={(v) => setShow(v as Show)} options={shows} className="cd-show-select" />
      </div>

      <div className="cd-panel">
        {show === "resources" &&
          (res.error && !res.data ? (
            <ErrorState error={res.error} onRetry={res.reload} compact />
          ) : !res.data ? (
            <SkeletonRows rows={6} cols={3} />
          ) : resources.length === 0 ? (
            <p className="cd-empty">This app renders no resources.</p>
          ) : (
            <ResourceList list={resources} />
          ))}
        {show === "diff" && <DiffList id={id} tick={tick} />}
        {show === "history" && (
          <HistoryList
            id={id}
            tick={tick}
            busy={busy}
            onRollback={
              a && !a.shadow
                ? (d, prune) => act(`Rolling ${name} back to deployment ${d.id}.`, () => cdApi.rollback(id, { to: d.id, prune }))
                : undefined
            }
          />
        )}
      </div>
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
    <div ref={ref} className="cd-resources" role="list" aria-label="Resources">
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

function DiffList({ id, tick }: { id: string; tick: number }) {
  const diff = useResource((s) => cdApi.diff(id, s), [id, tick]);
  if (diff.error && !diff.data) return <ErrorState error={diff.error} onRetry={diff.reload} compact />;
  if (!diff.data) return <SkeletonRows rows={4} cols={2} />;
  const list = diff.data.diffs;
  if (list.length === 0) return <p className="cd-empty">Nothing differs: the cluster runs what git says.</p>;
  return (
    <div className="cd-diffs">
      <p className="cd-meta">What a sync changes: − is what runs, + what it becomes. Fields the cluster fills in itself are left out.</p>
      {list.map((d) => (
        <div key={objectKey(d.key)} className="cd-diff">
          {d.why && <p className="cd-why">{d.why}</p>}
          <FileDiffView diff={{ path: objectLabel(d.key), hunks: d.hunks }} label={objectLabel(d.key)} defaultOpen={list.length <= 4} />
        </div>
      ))}
    </div>
  );
}

function HistoryList({
  id,
  tick,
  busy,
  onRollback,
}: {
  id: string;
  tick: number;
  busy: boolean;
  onRollback?: (d: CdDeployment, prune: boolean) => Promise<void>;
}) {
  const history = useResource((s) => cdApi.history(id, s), [id, tick]);
  const [asking, setAsking] = React.useState<number | null>(null);
  const [prune, setPrune] = React.useState(false);
  if (history.error && !history.data) return <ErrorState error={history.error} onRetry={history.reload} compact />;
  if (!history.data) return <SkeletonRows rows={4} cols={3} />;
  const list = history.data.history;
  if (list.length === 0) return <p className="cd-empty">No deployments yet.</p>;
  // What runs now is the newest that succeeded; any other success can be gone back to.
  const current = list.find((h) => h.succeeded);
  const same = (x: CdDeployment, y: CdDeployment | undefined) =>
    !!y && (x.commits?.length && y.commits?.length ? x.commits.join() === y.commits.join() : x.revision === y.revision);
  return (
    <div className="cd-history" role="list" aria-label="Deployments">
      {list.map((h) => {
        const canRollBack = onRollback && h.succeeded && !same(h, current) && h.id > 0;
        return (
          <div key={`${h.at_ms}-${h.op}`} className="cd-history-row" role="listitem">
            <StatePair tone={h.succeeded ? "succeeded" : "failed"} word={h.succeeded ? "Deployed" : "Failed"} />
            <span className="cd-mono">
              #{h.id} · {shortRev(h.revision) || "no revision"}
            </span>
            <span className="cd-history-what">
              {TRIGGER_WORD[h.trigger] ?? h.trigger}
              {h.by ? ` by ${h.by}` : ""} · {h.applied} applied{h.pruned ? ` · ${h.pruned} pruned` : ""}
              {h === current ? " · what runs now" : ""}
            </span>
            <RelTime ts={Math.floor(h.at_ms / 1000)} className="cd-meta cd-history-at" />
            {canRollBack ? (
              <Button
                size="xs"
                variant="ghost"
                className="cd-history-act"
                aria-expanded={asking === h.id}
                onClick={() => {
                  setAsking(asking === h.id ? null : h.id);
                  setPrune(false);
                }}
              >
                Roll back…
              </Button>
            ) : (
              <span className="cd-history-act" aria-hidden="true" />
            )}
            {!h.succeeded && h.message && <p className="cd-history-msg">{h.message}</p>}
            {canRollBack && asking === h.id && (
              <div className="cd-confirm cd-history-confirm" role="group" aria-label={`Roll back to deployment ${h.id}`}>
                <p>
                  kiln renders deployment {h.id}
                  {h.revision ? ` (${shortRev(h.revision)})` : ""} again and syncs it. The app holds there until git changes it, then
                  follows git again.
                </p>
                <label className="cd-opt">
                  <input type="checkbox" checked={prune} onChange={(e) => setPrune(e.target.checked)} /> Also delete what it didn't have
                  (prune)
                </label>
                <div className="cd-confirm-actions">
                  <Button
                    size="xs"
                    onClick={async () => {
                      await onRollback(h, prune);
                      setAsking(null);
                    }}
                    disabled={busy}
                  >
                    Roll back to #{h.id}
                  </Button>
                  <Button size="xs" variant="ghost" onClick={() => setAsking(null)}>
                    Cancel
                  </Button>
                </div>
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}

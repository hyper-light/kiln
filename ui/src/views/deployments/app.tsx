/**
 * One app: everything Argo CD's application view does, in kiln's words. The head says how it
 * stands (sync, health), what it runs (the revision, its message, author and time), how its last
 * sync went, and where it comes from and goes to; its actions (sync, refresh, diff, history and
 * rollback, delete) open in the page, under them, and what can't be undone asks there first.
 *
 * Below, its objects and every live object they own, as a tree (a graph on wide screens, an
 * indented list on phones), a sortable list, or its pods by node or owner, chosen with a select.
 * One filter field (kind, health, sync, namespace, and names as you type) applies to every view;
 * in the tree a match keeps its owners in view. Choosing an object opens it in the page: under
 * the graph on wide screens, under its row everywhere else.
 */
import * as React from "react";
import { Link, useNavigate, useParams } from "react-router";
import { RefreshCw, Rocket } from "lucide-react";

import { cdApi } from "@/api/cd";
import { useServerEvents } from "@/api/events";
import type { CdAppDetail, CdDeployment, CdNode, CdResource, CdTree } from "@/api/types";
import { Button } from "@/components/ui/button";
import { FacetBox, type Chosen } from "@/components/ui/facet-box";
import { Hold, RowsHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Confirm, Crumbs, EmptyState, SelectField } from "@/components/ui/misc";
import { errorMessage, useElementWidth, useMediaQuery, useNow } from "@/lib/hooks";
import { usePref } from "@/lib/prefs";
import { useRes, warm } from "@/lib/data";
import { invalidate, useSuspenseResource } from "@/lib/resource";
import { FileDiffView } from "@/views/harness/diff";
import { RelTime } from "@/views/shared";
import { ResourceDetails } from "./details";
import { age, compareNodes, compareText, facetsOf, forest, isFiltered, keep, kindRank, troubled, type GroupItem } from "./model";
import {
  HealthPair,
  KindBadge,
  appKey,
  NodeMarks,
  StatePair,
  SyncPair,
  TRIGGER_WORD,
  nodeLine,
  objectKey,
  objectLabel,
  opTone,
  opWords,
  placeOf,
  running,
  shortRev,
} from "./parts";
import { TreeGraph, TreeList, graphFits } from "./tree";
import "./deployments.css";

type View = "tree" | "list" | "pods";
type Ask = null | "sync" | "diff" | "history" | "takeover" | "delete" | "follow";
type Notice = { tone: "ok" | "fail"; text: string } | null;
type ListSort = "kind" | "name" | "namespace" | "health" | "sync" | "age";
type PodGroup = "owner" | "node";

const VIEWS = [
  { value: "tree", label: "Tree" },
  { value: "list", label: "List" },
  { value: "pods", label: "Pods" },
];
const SORTS = [
  { value: "kind", label: "Kind" },
  { value: "name", label: "Name" },
  { value: "namespace", label: "Namespace" },
  { value: "health", label: "Health, worst first" },
  { value: "sync", label: "Sync, out of sync first" },
  { value: "age", label: "Age, newest first" },
];
const GROUPS = [
  { value: "owner", label: "Owner" },
  { value: "node", label: "Node" },
];
const HEALTH_RANK: Record<string, number> = { Degraded: 0, Missing: 1, Progressing: 2, Suspended: 3, Unknown: 4, Healthy: 5 };
const SYNC_RANK: Record<string, number> = { OutOfSync: 0, Unknown: 1, Synced: 2, Hook: 3 };

/** One app's page: a fresh page per app, so nothing of one shows as another's. */
export default function DeploymentAppRoute() {
  const { project = "", name = "" } = useParams();
  return (
    <div className="container-console page cd-page">
      <title>{`${name} · kiln`}</title>
      <Loadable label="the app" fallback={<AppHold project={project} name={name} />}>
        <DeploymentApp key={`${project}/${name}`} project={project} name={name} />
      </Loadable>
    </div>
  );
}

/** The app's objects as the engine knows them, for when its cluster can't be read. */
function managedOnly(list: CdResource[]): CdNode[] {
  return list.map((r) => ({
    id: `${r.group}/${r.kind}:${r.namespace}/${r.name}`,
    uid: null,
    group: r.group,
    version: "",
    kind: r.kind,
    namespace: r.namespace,
    name: r.name,
    parents: [],
    managed: true,
    health: r.health,
    message: r.message,
    sync: r.sync,
    hook: r.hook,
    missing: r.missing,
    requires_pruning: r.requires_pruning,
    why: r.why,
    created_at: null,
    info: [],
    status: null,
    ready: null,
    restarts: null,
    images: [],
    containers: [],
    node: null,
  }));
}

/** What the app runs: its tree; or, when its cluster can't be read, the objects kiln compares. */
interface Objects {
  tree: CdTree | null;
  /** Why the tree couldn't be read. */
  error: string | null;
  nodes: CdNode[];
}

async function readObjects(id: string, signal: AbortSignal): Promise<Objects> {
  try {
    const tree = await cdApi.tree(id, signal);
    return { tree, error: null, nodes: tree.nodes };
  } catch (e) {
    if (signal.aborted) throw e;
    const { resources } = await cdApi.resources(id, signal);
    return { tree: null, error: errorMessage(e), nodes: managedOnly(resources) };
  }
}

/** The app, or why there is none (404: deleted, or a wrong link). */
async function readApp(id: string, signal: AbortSignal): Promise<CdAppDetail | { missing: string }> {
  try {
    return await cdApi.app(id, signal);
  } catch (e) {
    if ((e as { status?: number }).status === 404) return { missing: errorMessage(e) };
    throw e;
  }
}

/** One app's data: its detail, its objects and its history, loaded together. */
const appRes = (id: string) => ({
  detail: { key: `${appKey(id)}detail`, fetch: (s: AbortSignal) => readApp(id, s), opts: { pollMs: 10_000 } },
  objects: { key: `${appKey(id)}tree`, fetch: (s: AbortSignal) => readObjects(id, s), opts: { pollMs: 5_000 } },
  history: { key: `${appKey(id)}history`, fetch: (s: AbortSignal) => cdApi.history(id, s) },
});

function DeploymentApp({ project, name }: { project: string; name: string }) {
  const id = `${project}/${name}`;
  const r = appRes(id);
  warm(r.detail, r.objects, r.history);
  const app = useRes(r.detail).data;
  if ("missing" in app) {
    return (
      <EmptyState icon={<Rocket size={20} />} title="No such app." action={<Link to="/deployments">All deployments</Link>}>
        {app.missing}
      </EmptyState>
    );
  }
  return <AppPage id={id} project={project} name={name} a={app} />;
}

function AppPage({ id, project, name, a }: { id: string; project: string; name: string; a: CdAppDetail }) {
  const navigate = useNavigate();
  const r = appRes(id);
  const objects = useRes(r.objects).data;
  const history = useRes(r.history).data.history;
  const [ask, setAsk] = React.useState<Ask>(null);
  const [orphan, setOrphan] = React.useState(false);
  const [busy, setBusy] = React.useState(false);
  const [notice, setNotice] = React.useState<Notice>(null);

  // Live: this app's status changed, so read it again (at most twice a second).
  const soon = React.useRef<number | null>(null);
  useServerEvents((ev) => {
    if (ev.type === "cd_apps" && soon.current === null && ev.apps.some((x) => x.app === id)) {
      soon.current = window.setTimeout(() => {
        soon.current = null;
        invalidate(appKey(id));
      }, 500);
    } else if (ev.type === "cd_app_deleted" && ev.app === id) {
      navigate("/deployments", { replace: true });
    }
  });

  const act = async (done: string, f: () => Promise<unknown>, keepOpen = false) => {
    setBusy(true);
    setNotice(null);
    try {
      await f();
      const said: Notice = { tone: "ok", text: done };
      setNotice(said);
      // A success says so for a while; a failure stays until the next action.
      window.setTimeout(() => setNotice((n) => (n === said ? null : n)), 8_000);
      if (!keepOpen) setAsk(null);
      invalidate(appKey(id));
      invalidate("cd/summary");
    } catch (e) {
      setNotice({ tone: "fail", text: errorMessage(e) });
    } finally {
      setBusy(false);
    }
  };
  const toggle = (x: Exclude<Ask, null>) => setAsk(ask === x ? null : x);

  const op = a.operation ?? null;
  const live = running(op);
  const nodes = objects.nodes;
  const managed = nodes.filter((n) => n.managed);
  const childCount = nodes.length - managed.length;
  const last = history[0];

  return (
    <>
      <header className="cd-head">
        <AppCrumbs project={project} />
        <h1 className="cd-title">{name}</h1>
        <div className="cd-state">
          <SyncPair sync={a.sync} />
          <HealthPair health={a.health} />
          <span className="cd-meta">{[placeOf(a), `${managed.length || a.resources || 0} objects`, childCount ? `${childCount} live children` : ""].filter(Boolean).join(" · ")}</span>
        </div>
        <dl className="cd-lines">
          <div className="cd-lines-rev">
            <dt>Revision</dt>
            <dd>
              <RevisionLine a={a} />
            </dd>
          </div>
          <div>
            <dt>{live ? "Now" : "Last sync"}</dt>
            <dd>
              <OpLine a={a} last={last} />
            </dd>
          </div>
          <div className="cd-lines-from">
            <dt>From</dt>
            <dd>{fromLine(a)}</dd>
          </div>
        </dl>
        {a.shadow && <p className="cd-note">Shadowed from Argo CD: kiln compares it and never writes to it. Take it over to deploy it with kiln.</p>}
        {a.pinned && (
          <p className="cd-note">
            Rolled back to deployment {a.pinned.to}
            {a.pinned.revision ? ` (${shortRev(a.pinned.revision)})` : ""}
            {a.pinned.by ? ` by ${a.pinned.by}` : ""}, <RelTime ts={Math.floor(a.pinned.at_ms / 1000)} />. kiln holds it there until git changes this app, then follows git again.
          </p>
        )}
        <div className="cd-actions">
          {a.shadow ? (
            <Button size="xs" onClick={() => toggle("takeover")} aria-expanded={ask === "takeover"} disabled={busy}>
              Take over…
            </Button>
          ) : (
            <Button size="xs" onClick={() => toggle("sync")} aria-expanded={ask === "sync"} disabled={busy || live}>
              Sync…
            </Button>
          )}
          <Button size="xs" variant="secondary" onClick={() => act("Comparing it with git again.", () => cdApi.refresh(id, false))} disabled={busy}>
            <RefreshCw aria-hidden="true" /> Refresh
          </Button>
          <Button size="xs" variant="secondary" onClick={() => act("Rendering it from git again, then comparing.", () => cdApi.refresh(id, true))} disabled={busy} title="Render the app from git again, not only compare">
            Hard refresh
          </Button>
          <Button size="xs" variant="secondary" onClick={() => toggle("diff")} aria-expanded={ask === "diff"}>
            Diff{a.out_of_sync ? ` (${a.out_of_sync})` : ""}
          </Button>
          <Button size="xs" variant="secondary" onClick={() => toggle("history")} aria-expanded={ask === "history"}>
            History
          </Button>
          {live && (
            <Button size="xs" variant="secondary" onClick={() => act("Stopping the sync.", () => cdApi.terminate(id))} disabled={busy}>
              Stop
            </Button>
          )}
          {a.pinned && (
            <Button size="xs" variant="secondary" onClick={() => toggle("follow")} aria-expanded={ask === "follow"} disabled={busy}>
              Follow git…
            </Button>
          )}
          <Button size="xs" variant="ghost" onClick={() => toggle("delete")} aria-expanded={ask === "delete"} disabled={busy}>
            Delete…
          </Button>
        </div>
        {ask === "sync" && !a.shadow && <SyncPanel a={a} managed={managed} busy={busy} onCancel={() => setAsk(null)} onSync={(body, done) => act(done, () => cdApi.sync(id, body))} />}
        {ask === "takeover" && (
          <Confirm
            label="Confirm"
            actions={
              <>
                <Button size="xs" onClick={() => act(`kiln deploys ${name} now.`, () => cdApi.takeOver(id))} disabled={busy}>
                  Take over {name}
                </Button>
                <Button size="xs" variant="ghost" onClick={() => setAsk(null)}>
                  Cancel
                </Button>
              </>
            }
          >
            <p>kiln stops shadowing {name} and deploys it from now on. Turn off auto-sync for it in Argo CD first, so the two don't both apply it. Nothing is recreated.</p>
          </Confirm>
        )}
        {ask === "follow" && (
          <Confirm
            label="Confirm"
            actions={
              <>
                <Button size="xs" onClick={() => act(`${name} follows git again.`, () => cdApi.follow(id))} disabled={busy}>
                  Follow git
                </Button>
                <Button size="xs" variant="ghost" onClick={() => setAsk(null)}>
                  Cancel
                </Button>
              </>
            }
          >
            <p>
              kiln renders what git says now
              {a.auto ? " and deploys it: this app syncs automatically." : ". Sync to deploy it."}
            </p>
          </Confirm>
        )}
        {ask === "delete" && (
          <Confirm
            label="Confirm"
            tone="danger"
            actions={
              <>
                <Button
                  size="xs"
                  variant="danger"
                  onClick={() =>
                    act(`Deleting ${name}.`, async () => {
                      await cdApi.remove(id, a.shadow || orphan ? "orphan" : "foreground");
                      invalidate("cd/");
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
              </>
            }
          >
            <p>
              {a.shadow
                ? "kiln forgets it. Argo CD still deploys it, and nothing in the cluster changes."
                : orphan
                  ? "kiln forgets it; its objects keep running in the cluster."
                  : `Its objects are deleted from ${placeOf(a)} too, except those marked Delete=false and namespaces it created.`}
            </p>
            {!a.shadow && (
              <label className="cd-opt">
                <input type="checkbox" className="check" checked={orphan} onChange={(e) => setOrphan(e.target.checked)} /> Leave its objects running
              </label>
            )}
          </Confirm>
        )}
        {notice && (
          <p className="cd-notice" data-tone={notice.tone} aria-live="polite">
            {notice.text}
          </p>
        )}
      </header>

      {a.source_error && <p className="cd-warn">kiln can't read its sources: {a.source_error}</p>}
      {a.message && <p className="cd-warn">{a.message}</p>}

      {ask === "diff" && (
        <section className="cd-panel-flow" aria-label="What differs">
          <h2 className="cd-flow-title">What differs</h2>
          <Loadable label="what differs" fallback={<RowsHold rows={4} rowHeight={48} />}>
            <DiffList id={id} />
          </Loadable>
        </section>
      )}
      {ask === "history" && (
        <section className="cd-panel-flow" aria-label="History">
          <h2 className="cd-flow-title">History</h2>
          <HistoryList
            list={history}
            busy={busy}
            onRollback={!a.shadow ? (d, prune) => act(`Rolling ${name} back to deployment ${d.id}.`, () => cdApi.rollback(id, { to: d.id, prune }), true) : undefined}
          />
        </section>
      )}

      <Resources
        appId={id}
        appMark={{ name, health: a.health ?? "Unknown", sync: a.sync ?? "Unknown" }}
        namespace={a.namespace ?? ""}
        nodes={nodes}
        treeError={objects.error}
        errors={objects.tree?.errors ?? []}
        truncated={objects.tree?.truncated ?? false}
      />
    </>
  );
}

function AppCrumbs({ project }: { project: string }) {
  return <Crumbs index={3} items={[{ label: "Deployments", to: "/deployments" }, { label: project }]} />;
}

/** The app while it loads: its crumbs and name (both from the address), then its head's and its objects' space. */
function AppHold({ project, name }: { project: string; name: string }) {
  return (
    <>
      <header className="cd-head">
        <AppCrumbs project={project} />
        <h1 className="cd-title">{name}</h1>
        <Hold height={170} label="Loading the app" />
      </header>
      <section className="cd-resources-section" aria-label="Resources">
        <RowsHold rows={6} rowHeight={54} />
      </section>
    </>
  );
}

/** The revision: its commit, what it says, who wrote it and when. */
function RevisionLine({ a }: { a: CdAppDetail }) {
  const c = a.revision_info;
  if (!a.revision) return <span className="cd-dim">Not rendered yet</span>;
  return (
    <>
      <span className="cd-mono" title={a.revision}>
        {shortRev(a.revision)}
      </span>
      {c ? (
        <>
          <span className="cd-line-text" title={c.body ? `${c.subject}\n\n${c.body}` : c.subject}>
            {c.subject}
          </span>
          <span className="cd-dim cd-line-by">
            {c.author} · <RelTime ts={Math.floor(c.at_ms / 1000)} />
          </span>
        </>
      ) : null}
    </>
  );
}

/** The sync under way, or the last one: its result, when, by whom, and what it said. */
function OpLine({ a, last }: { a: CdAppDetail; last: CdDeployment | undefined }) {
  const op = a.operation;
  if (op) {
    const at = op.finished_ms ?? op.started_ms;
    // opWords already says what started it when it wasn't a person.
    const by = op.by;
    return (
      <>
        <StatePair tone={opTone(op)} word={opWords(op)} />
        {at ? (
          <span className="cd-dim">
            {running(op) ? "started " : ""}
            <RelTime ts={Math.floor(at / 1000)} />
            {by ? ` by ${by}` : ""}
            {op.attempt > 1 ? ` · attempt ${op.attempt}` : ""}
          </span>
        ) : null}
        {op.message && (
          <span className="cd-line-text" title={op.message}>
            {op.message}
          </span>
        )}
      </>
    );
  }
  if (last) {
    return (
      <>
        <StatePair tone={last.succeeded ? "succeeded" : "failed"} word={last.succeeded ? "Deployed" : "Failed"} />
        <span className="cd-dim">
          <RelTime ts={Math.floor(last.at_ms / 1000)} />
          {last.by ? ` by ${last.by}` : ` · ${(TRIGGER_WORD[last.trigger] ?? last.trigger).toLowerCase()}`}
          {` · ${last.applied} applied`}
        </span>
        {last.message && (
          <span className="cd-line-text" title={last.message}>
            {last.message}
          </span>
        )}
      </>
    );
  }
  return <span className="cd-dim">Never synced</span>;
}

/** Where an app comes from, where it goes, and how it syncs, in one line. */
function fromLine(a: CdAppDetail) {
  const src = a.definition?.sources?.[0];
  const where = src ? [src.chart ? `${src.repo} ${src.chart}` : src.repo, src.path].filter(Boolean).join(" · ") : "";
  const at = src?.revision ? ` @ ${src.revision}` : "";
  const more = (a.definition?.sources?.length ?? 0) > 1 ? ` (+${(a.definition?.sources?.length ?? 1) - 1} more)` : "";
  const sync = a.definition?.sync;
  const how = sync?.auto ? ["Auto-sync", sync.prune && "prune", sync.self_heal && "self-heal"].filter(Boolean).join(", ") : "Synced by hand";
  return [where && `${where}${at}${more} → ${a.cluster} / ${a.namespace || "default"}`, how].filter(Boolean).join(" · ");
}

/**
 * Sync, asked in place: prune, dry run, and optionally only some of the app's objects, chosen
 * from a list (what differs first).
 */
function SyncPanel({
  a,
  managed,
  busy,
  onSync,
  onCancel,
}: {
  a: CdAppDetail;
  managed: CdNode[];
  busy: boolean;
  onSync: (body: { prune: boolean; dry_run: boolean; resources?: { group: string; kind: string; namespace: string; name: string }[] }, done: string) => void;
  onCancel: () => void;
}) {
  const [prune, setPrune] = React.useState(false);
  const [dryRun, setDryRun] = React.useState(false);
  const [some, setSome] = React.useState(false);
  const list = React.useMemo(
    () => managed.filter((n) => !n.hook).sort((x, y) => Number(y.sync === "OutOfSync") - Number(x.sync === "OutOfSync") || compareNodes(x, y)),
    [managed],
  );
  const [picked, setPicked] = React.useState<ReadonlySet<string>>(() => new Set(list.filter((n) => n.sync === "OutOfSync").map((n) => n.id)));
  const prunable = list.filter((n) => n.requires_pruning).length;
  const pick = (ids: string[]) => setPicked(new Set(ids));
  const chosen = list.filter((n) => picked.has(n.id));
  const go = () => {
    const resources = some ? chosen.map((n) => ({ group: n.group, kind: n.kind, namespace: n.namespace, name: n.name })) : undefined;
    const what = some ? `${chosen.length} of its objects` : "it";
    onSync({ prune, dry_run: dryRun, resources }, dryRun ? `Checking ${what} against the cluster.` : `Syncing ${what}.`);
  };
  return (
    <Confirm
      className="cd-sync"
      label="Sync"
      actions={
        <>
          <Button size="xs" onClick={go} disabled={busy || (some && chosen.length === 0)}>
            {dryRun ? "Check" : some ? `Sync ${chosen.length}` : "Sync"}
          </Button>
          <Button size="xs" variant="ghost" onClick={onCancel}>
            Cancel
          </Button>
        </>
      }
    >
      <p>
        Apply what git holds{a.revision ? ` at ${shortRev(a.revision)}` : ""} to {placeOf(a)}
        {a.out_of_sync ? `: ${a.out_of_sync} ${a.out_of_sync === 1 ? "object differs" : "objects differ"}.` : ". Nothing differs now."}
      </p>
      <label className="cd-opt">
        <input type="checkbox" className="check" checked={prune} onChange={(e) => setPrune(e.target.checked)} /> Prune: delete what git no longer holds
        {prunable ? ` (${prunable})` : ""}
      </label>
      <label className="cd-opt">
        <input type="checkbox" className="check" checked={dryRun} onChange={(e) => setDryRun(e.target.checked)} /> Dry run: check every object with
        the cluster, change nothing
      </label>
      <label className="cd-opt">
        <input type="checkbox" className="check" checked={some} onChange={(e) => setSome(e.target.checked)} /> Only the objects chosen below
      </label>
      {some && (
        <div className="cd-pickset">
          <div className="cd-pickset-bar">
            <span className="cd-meta">
              {chosen.length} of {list.length} chosen
            </span>
            <button type="button" className="cd-link" onClick={() => pick(list.map((n) => n.id))}>
              All
            </button>
            <button type="button" className="cd-link" onClick={() => pick(list.filter((n) => n.sync === "OutOfSync").map((n) => n.id))}>
              What differs
            </button>
            <button type="button" className="cd-link" onClick={() => pick([])}>
              None
            </button>
          </div>
          <ul className="cd-picklist">
            {list.map((n) => (
              <li key={n.id}>
                <label className="cd-pickrow">
                  <input
                    type="checkbox"
                    className="check"
                    checked={picked.has(n.id)}
                    onChange={(e) => {
                      const next = new Set(picked);
                      if (e.target.checked) next.add(n.id);
                      else next.delete(n.id);
                      setPicked(next);
                    }}
                  />
                  <KindBadge kind={n.kind} group={n.group} />
                  <span className="cd-pickrow-name">{n.namespace && n.namespace !== a.namespace ? `${n.namespace}/${n.name}` : n.name}</span>
                  <NodeMarks n={n} />
                </label>
              </li>
            ))}
          </ul>
        </div>
      )}
    </Confirm>
  );
}

/* ------------------------------------------------------------------ resources */

function Resources({
  appId,
  appMark,
  namespace,
  nodes,
  treeError,
  errors,
  truncated,
}: {
  appId: string;
  appMark: { name: string; health: string; sync: string };
  namespace: string;
  nodes: CdNode[];
  treeError: string | null;
  errors: string[];
  truncated: boolean;
}) {
  const now = useNow();
  const wide = useMediaQuery("(min-width: 1024px)");
  const [stored, setStored] = usePref<{ view?: View; sort?: ListSort; group?: PodGroup; children?: boolean }>("kiln.cd.app.view", {});
  const view: View = stored.view ?? "tree";
  const sort: ListSort = stored.sort ?? "kind";
  const group: PodGroup = stored.group ?? "owner";
  const children = stored.children ?? true;
  const set = (patch: Partial<typeof stored>) => setStored({ ...stored, ...patch });
  const [q, setQ] = React.useState("");
  const [chosen, setChosen] = React.useState<Chosen>({});
  const [selected, setSelected] = React.useState<string | null>(null);
  const [open, setOpen] = React.useState<ReadonlySet<string>>(() => new Set());
  const [box, width] = useElementWidth<HTMLDivElement>();

  // What the view is over: the pods for Pods; for the others, children unless left out.
  const base = React.useMemo(
    () => (view === "pods" ? nodes.filter((n) => n.kind === "Pod" && n.group === "") : children ? nodes : nodes.filter((n) => n.managed)),
    [nodes, view, children],
  );
  const facets = React.useMemo(() => facetsOf(base, q, chosen), [base, q, chosen]);
  const filtered = isFiltered(q, chosen);
  const { shown, matched } = React.useMemo(() => keep(base, q, chosen, view === "tree"), [base, q, chosen, view]);
  const byId = React.useMemo(() => new Map(nodes.map((n) => [n.id, n])), [nodes]);
  const sel = selected ? (byId.get(selected) ?? null) : null;
  const roots = React.useMemo(() => forest(base, shown, matched, open, selected), [base, shown, matched, open, selected]);
  const graph = view === "tree" && wide && graphFits(roots, width);

  const onGroup = (g: GroupItem) =>
    setOpen((o) => {
      const next = new Set(o);
      if (next.has(g.id)) next.delete(g.id);
      else next.add(g.id);
      return next;
    });
  const choose = (n: CdNode) => setSelected(selected === n.id ? null : n.id);
  const details = sel ? (
    <ResourceDetails key={sel.id} appId={appId} node={sel} onClose={() => setSelected(null)} onSelect={(id) => setSelected(id)} />
  ) : selected ? (
    <p className="cd-empty">
      That object is gone from the cluster.{" "}
      <button type="button" className="cd-link" onClick={() => setSelected(null)}>
        Close
      </button>
    </p>
  ) : null;

  const counts = [
    filtered ? `${matched.size} of ${base.length} shown` : `${base.length} ${view === "pods" ? (base.length === 1 ? "pod" : "pods") : base.length === 1 ? "object" : "objects"}`,
    ...(!filtered
      ? [
          base.filter((n) => n.managed && !n.hook && n.sync === "OutOfSync").length && `${base.filter((n) => n.managed && !n.hook && n.sync === "OutOfSync").length} out of sync`,
          base.filter((n) => n.health === "Degraded").length && `${base.filter((n) => n.health === "Degraded").length} degraded`,
          base.filter((n) => n.health === "Progressing").length && `${base.filter((n) => n.health === "Progressing").length} progressing`,
          base.filter((n) => n.health === "Missing").length && `${base.filter((n) => n.health === "Missing").length} missing`,
        ]
      : []),
  ]
    .filter(Boolean)
    .join(" · ");

  return (
    <section className="cd-resources-section" aria-label="Resources">
      <div className="cd-viewbar" data-view={view}>
        <SelectField label="View" hideLabel value={view} onChange={(v) => set({ view: v as View })} options={VIEWS} className="cd-view-select" />
        <FacetBox
          className="cd-view-filter"
          label="Filter objects"
          placeholder="Filter by name, kind, health, sync, namespace…"
          query={q}
          onQuery={setQ}
          facets={facets}
          chosen={chosen}
          onChosen={setChosen}
        />
        {view === "list" && <SelectField label="Sort by" hideLabel value={sort} onChange={(v) => set({ sort: v as ListSort })} options={SORTS} className="cd-view-sort" />}
        {view === "pods" && <SelectField label="Group by" hideLabel value={group} onChange={(v) => set({ group: v as PodGroup })} options={GROUPS} className="cd-view-sort" />}
        {view !== "pods" && (
          <label className="cd-opt cd-view-opt" title="The live objects the app's objects own: ReplicaSets, Pods, Jobs, EndpointSlices…">
            <input type="checkbox" className="check" checked={children} onChange={(e) => set({ children: e.target.checked })} /> Live children
          </label>
        )}
      </div>
      <div className="cd-viewhead">
        <span className="cd-count" aria-live="polite">
          {counts}
        </span>
        {filtered && (
          <button
            type="button"
            className="cd-link"
            onClick={() => {
              setQ("");
              setChosen({});
            }}
          >
            Clear the filter
          </button>
        )}
      </div>
      {treeError && (
        <p className="cd-warn">
          The cluster couldn't be read ({treeError}); showing the objects kiln compares, without what they own.
        </p>
      )}
      {errors.length > 0 && <p className="cd-warn">Not everything could be read: {errors.slice(0, 3).join("; ")}</p>}
      {truncated && <p className="cd-warn">The tree is cut at 5,000 objects.</p>}

      <div ref={box} className="cd-view">
        {base.length === 0 ? (
          <p className="cd-empty">{view === "pods" ? "It runs no pods." : "This app renders no resources."}</p>
        ) : matched.size === 0 ? (
          <p className="cd-empty">
            Nothing matches.{" "}
            <button
              type="button"
              className="cd-link"
              onClick={() => {
                setQ("");
                setChosen({});
              }}
            >
              Clear the filter
            </button>
          </p>
        ) : view === "tree" ? (
          graph ? (
            <>
              <TreeGraph roots={roots} app={appMark} width={width} selected={selected} onSelect={choose} onGroup={onGroup} />
              {details && <div className="cd-graph-details">{details}</div>}
            </>
          ) : (
            <TreeList roots={roots} namespace={namespace} selected={selected} onSelect={choose} onGroup={onGroup} details={details} />
          )
        ) : view === "list" ? (
          <FlatList nodes={base.filter((n) => matched.has(n.id))} sort={sort} now={now} selected={selected} onSelect={choose} details={details} />
        ) : (
          <PodList pods={base.filter((n) => matched.has(n.id))} group={group} byId={byId} now={now} selected={selected} onSelect={choose} details={details} />
        )}
      </div>
    </section>
  );
}

function sortNodes(list: CdNode[], sort: ListSort): CdNode[] {
  const by: Record<ListSort, (x: CdNode, y: CdNode) => number> = {
    kind: compareNodes,
    name: (x, y) => compareText(x.name, y.name),
    namespace: (x, y) => compareText(x.namespace, y.namespace),
    health: (x, y) => (HEALTH_RANK[x.health ?? ""] ?? 6) - (HEALTH_RANK[y.health ?? ""] ?? 6),
    sync: (x, y) => (x.managed ? (SYNC_RANK[x.sync ?? ""] ?? 4) : 5) - (y.managed ? (SYNC_RANK[y.sync ?? ""] ?? 4) : 5),
    age: (x, y) => (Date.parse(y.created_at ?? "") || 0) - (Date.parse(x.created_at ?? "") || 0),
  };
  return [...list].sort((x, y) => by[sort](x, y) || kindRank(x.kind) - kindRank(y.kind) || compareText(x.name, y.name));
}

/** One object in a list: its kind, name, place, marks, what it says, its age. */
function Row({ n, now, selected, onSelect, sub }: { n: CdNode; now: number; selected: boolean; onSelect: (n: CdNode) => void; sub?: string }) {
  const note = n.why ?? (n.health && n.health !== "Healthy" ? n.message : null) ?? nodeLine(n, "");
  return (
    <button
      type="button"
      className="cd-lrow"
      data-selected={selected || undefined}
      data-tone={troubled(n) ? (n.health === "Degraded" || n.health === "Missing" ? "fail" : "warn") : undefined}
      aria-expanded={selected}
      onClick={() => onSelect(n)}
    >
      <KindBadge kind={n.kind} group={n.group} />
      <span className="cd-lrow-main">
        <span className="cd-lrow-name">{n.name}</span>
        <span className="cd-lrow-sub">{sub ?? [n.namespace || "cluster", n.managed ? "" : "live"].filter(Boolean).join(" · ")}</span>
      </span>
      <span className="cd-lrow-marks">
        <NodeMarks n={n} />
      </span>
      <span className="cd-lrow-note" title={note || undefined}>
        {note}
      </span>
      <span className="cd-lrow-age">{age(n.created_at, now)}</span>
    </button>
  );
}

function FlatList({
  nodes,
  sort,
  now,
  selected,
  onSelect,
  details,
}: {
  nodes: CdNode[];
  sort: ListSort;
  now: number;
  selected: string | null;
  onSelect: (n: CdNode) => void;
  details: React.ReactNode;
}) {
  const sorted = React.useMemo(() => sortNodes(nodes, sort), [nodes, sort]);
  const inView = sorted.some((n) => n.id === selected);
  return (
    <ul className="cd-llist" aria-label="Objects">
      {!inView && details ? <li>{details}</li> : null}
      {sorted.map((n) => (
        <li key={n.id}>
          <Row n={n} now={now} selected={selected === n.id} onSelect={onSelect} />
          {selected === n.id && details}
        </li>
      ))}
    </ul>
  );
}

/** Pods, grouped by the node they run on or by what owns them. */
function PodList({
  pods,
  group,
  byId,
  now,
  selected,
  onSelect,
  details,
}: {
  pods: CdNode[];
  group: PodGroup;
  byId: Map<string, CdNode>;
  now: number;
  selected: string | null;
  onSelect: (n: CdNode) => void;
  details: React.ReactNode;
}) {
  const groups = React.useMemo(() => {
    const m = new Map<string, { label: string; owner: CdNode | null; pods: CdNode[] }>();
    for (const p of pods) {
      // The app's own object that runs it (a Deployment, not its ReplicaSet of the day).
      let owner: CdNode | null = p.parents.map((u) => byId.get(u)).find(Boolean) ?? null;
      for (let hops = 0; owner && !owner.managed && hops < 8; hops++) {
        owner = owner.parents.map((u) => byId.get(u)).find(Boolean) ?? owner;
        if (owner.managed || !owner.parents.length) break;
      }
      const key = group === "node" ? (p.node ?? "") : (owner?.id ?? "");
      const label = group === "node" ? (p.node ?? "Not scheduled") : owner ? `${owner.kind} ${owner.name}` : "No owner";
      const g = m.get(key);
      if (g) g.pods.push(p);
      else m.set(key, { label, owner: group === "owner" ? owner : null, pods: [p] });
    }
    return [...m.values()].sort((x, y) => compareText(x.label, y.label)).map((g) => ({ ...g, pods: [...g.pods].sort((x, y) => compareText(x.name, y.name)) }));
  }, [pods, group, byId]);
  // An owner chosen from a pod's details isn't a pod: its details open on top.
  const inView = pods.some((p) => p.id === selected);
  return (
    <div className="cd-pods">
      {!inView && details}
      {groups.map((g) => {
        const bad = g.pods.filter((p) => p.health === "Degraded").length;
        return (
          <section key={g.label} className="cd-podgroup" aria-label={g.label}>
            <h3 className="cd-podgroup-head">
              <span className="cd-podgroup-name">{g.label}</span>
              <span className="cd-dim">
                {g.pods.length} {g.pods.length === 1 ? "pod" : "pods"}
                {bad ? ` · ${bad} degraded` : ""}
              </span>
            </h3>
            <ul className="cd-llist">
              {g.pods.map((p) => (
                <li key={p.id}>
                  <Row n={p} now={now} selected={selected === p.id} onSelect={onSelect} sub={group === "node" ? (p.parents.map((u) => byId.get(u)?.name).find(Boolean) ?? "") : (p.node ?? "Not scheduled")} />
                  {selected === p.id && details}
                </li>
              ))}
            </ul>
          </section>
        );
      })}
    </div>
  );
}

/* ------------------------------------------------------------------ diff and history */

function DiffList({ id }: { id: string }) {
  const diff = useSuspenseResource(`${appKey(id)}diff`, (s) => cdApi.diff(id, s));
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
  list,
  busy,
  onRollback,
}: {
  list: CdDeployment[];
  busy: boolean;
  onRollback?: (d: CdDeployment, prune: boolean) => Promise<void>;
}) {
  const [asking, setAsking] = React.useState<number | null>(null);
  const [prune, setPrune] = React.useState(false);
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
              {h.commit?.subject ? <span className="cd-history-subject">{h.commit.subject}</span> : null}
              <span>
                {TRIGGER_WORD[h.trigger] ?? h.trigger}
                {h.by ? ` by ${h.by}` : ""} · {h.applied} applied{h.pruned ? ` · ${h.pruned} pruned` : ""}
                {h === current ? " · what runs now" : ""}
              </span>
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
              <Confirm
                className="cd-history-confirm"
                label={`Roll back to deployment ${h.id}`}
                actions={
                  <>
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
                  </>
                }
              >
                <p>
                  kiln renders deployment {h.id}
                  {h.revision ? ` (${shortRev(h.revision)})` : ""} again and syncs it. The app holds there until git changes it, then follows git again.
                </p>
                <label className="cd-opt">
                  <input type="checkbox" className="check" checked={prune} onChange={(e) => setPrune(e.target.checked)} /> Also delete what it didn't have (prune)
                </label>
              </Confirm>
            )}
          </div>
        );
      })}
    </div>
  );
}

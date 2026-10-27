/**
 * Deployments: every app kiln deploys, its sync and health, live. One list however large the
 * fleet: filters and sorts run on the server, pages arrive as you scroll, only the rows on screen
 * are drawn, and live updates are merged in once a frame. Above it, the fleet's counts, then the
 * console's one filter field (names as you type; sync, health, project, cluster and namespace as
 * checkboxes, each value with how many it would show) and a sort select. Rows have checkboxes, so
 * a selection, or everything a filter shows, syncs or refreshes at once.
 */
import * as React from "react";
import { Link, useSearchParams } from "@/lib/router";
import { Rocket } from "lucide-react";

import { appHref, cdApi, newAppHref, plansHref, type CdFilters, type CdSort } from "@/api/cd";
import { boardHref } from "@/api/deploy";
import { useServerEvents } from "@/api/events";
import type { CdApp, CdFacets, CdSummary } from "@/api/types";
import { Button } from "@/components/ui/button";
import { FacetBox, type Chosen, type Facet } from "@/components/ui/facet-box";
import { RowsHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { EmptyState, PageIntro, SelectField } from "@/components/ui/misc";
import { errorMessage } from "@/lib/hooks";
import { useRes, warm, type Res } from "@/lib/data";
import { invalidate } from "@/lib/resource";
import { useWindowedRows } from "@/lib/virtual";
import { environmentsHref } from "@/views/environments/shared";
import { whatDiffers } from "./model";
import { HEALTH_TONE, HealthPair, StatePair, SYNC_TONE, SyncPair, opWords, placeOf, running, shortRev } from "./parts";
import "./deployments.css";
import { ShipViews } from "@/views/ship/views";

const ROW = 68;
const PAGE = 500;

const FACETS = [
  { id: "sync", label: "Sync" },
  { id: "health", label: "Health" },
  { id: "project", label: "Project" },
  { id: "cluster", label: "Cluster" },
  { id: "namespace", label: "Namespace" },
] as const;

const SORTS: { value: CdSort; label: string }[] = [
  { value: "name", label: "Name, A to Z" },
  { value: "health", label: "Health, worst first" },
  { value: "sync", label: "Sync, out of sync first" },
  { value: "deployed", label: "Recently deployed" },
];
type FacetId = (typeof FACETS)[number]["id"];

const WORDS: Record<string, string> = { OutOfSync: "Out of sync" };
/** Worst first, as people look for trouble. */
const ORDER: Partial<Record<FacetId, string[]>> = {
  sync: ["OutOfSync", "Unknown", "Synced"],
  health: ["Degraded", "Missing", "Progressing", "Suspended", "Unknown", "Healthy"],
};

const n = (x: number) => x.toLocaleString();

/** The fleet's counts follow its statuses, read again at most every two seconds (statuses come in bursts). */
let countsSoon: number | null = null;
function countsLater() {
  if (countsSoon !== null) return;
  countsSoon = window.setTimeout(() => {
    countsSoon = null;
    invalidate("cd/summary");
    invalidate("cd/facets");
  }, 2_000);
}

/** The fleet at a glance: how many apps, and how many in each sync and health. */
function Summary({ s }: { s: CdSummary }) {
  if (s.apps === 0) return null;
  const sync = ORDER.sync?.filter((k) => s.sync[k]) ?? [];
  const health = ORDER.health?.filter((k) => s.health[k]) ?? [];
  return (
    <p className="cd-summary-line" aria-label="Every app">
      <span className="cd-count">
        {n(s.apps)} {s.apps === 1 ? "app" : "apps"} · {n(s.clusters)} {s.clusters === 1 ? "cluster" : "clusters"}
      </span>
      {sync.map((k) => (
        <StatePair key={`s-${k}`} tone={SYNC_TONE[k] ?? "skipped"} word={`${n(s.sync[k] ?? 0)} ${WORDS[k] ?? k}`.toLowerCase()} />
      ))}
      {health.map((k) => (
        <StatePair key={`h-${k}`} tone={HEALTH_TONE[k] ?? "skipped"} word={`${n(s.health[k] ?? 0)} ${k}`.toLowerCase()} />
      ))}
    </p>
  );
}

/** "6 apps · 1 out of sync · 1 degraded", or "2 of 6 apps" when filtered. */
function countLine(f: CdFacets, all: number, filters: CdFilters) {
  const filtered = isFiltered(filters);
  const parts = [filtered ? `${n(f.total)} of ${n(all)} apps` : `${n(f.total)} ${f.total === 1 ? "app" : "apps"}`];
  // A facet's counts describe what is shown only while it isn't filtered itself.
  if (!filters.sync?.length && f.facets.sync.OutOfSync) parts.push(`${n(f.facets.sync.OutOfSync)} out of sync`);
  if (!filters.health?.length) {
    for (const h of ["Degraded", "Missing", "Progressing"]) {
      const c = f.facets.health[h];
      if (c) parts.push(`${n(c)} ${h.toLowerCase()}`);
    }
  }
  return parts.join(" · ");
}

const isFiltered = (filters: CdFilters) => Object.values(filters).some((v) => (Array.isArray(v) ? v.length > 0 : Boolean(v)));

/** Runs `f` over `items`, `limit` at a time; every outcome, in order. */
async function settleAll<T>(items: string[], limit: number, f: (x: string) => Promise<T>): Promise<PromiseSettledResult<T>[]> {
  const out: PromiseSettledResult<T>[] = [];
  let next = 0;
  const worker = async () => {
    while (next < items.length) {
      const i = next++;
      const item = items[i];
      if (item === undefined) continue;
      try {
        out[i] = { status: "fulfilled", value: await f(item) };
      } catch (reason) {
        out[i] = { status: "rejected", reason };
      }
    }
  };
  await Promise.all(Array.from({ length: Math.min(limit, items.length) }, worker));
  return out;
}

/** The filters the URL says, and the query string the server takes for them. */
function useFilters() {
  const [params, setParams] = useSearchParams();
  const filters = React.useMemo<CdFilters>(() => {
    const list = (k: string) => (params.get(k) ?? "").split(",").filter(Boolean);
    return { sync: list("sync"), health: list("health"), cluster: list("cluster"), project: list("project"), namespace: list("namespace"), q: params.get("q") ?? "" };
  }, [params]);
  const sort = (SORTS.find((x) => x.value === params.get("sort"))?.value ?? "name") as CdSort;
  const key = [...FACETS.map((f) => `${f.id}=${(filters[f.id] ?? []).join(",")}`), `q=${filters.q ?? ""}`].join("&");
  return { params, setParams, filters, sort, key };
}

export default function Deployments() {
  return (
    <div className="container-console page cd-page">
      <title>Deployments · kiln</title>
      <PageIntro
        index={2}
        label="Deployments"
        title="Deployments."
        lead="What git says, and what runs: every app's sync and health, live."
        actions={
          <>
            <Button asChild size="sm" variant="secondary">
              <Link to={environmentsHref}>Environments</Link>
            </Button>
            <Button asChild size="sm" variant="secondary">
              <Link to={boardHref}>Releases</Link>
            </Button>
            <Button asChild size="sm" variant="secondary">
              <Link to={plansHref}>Plan a deploy</Link>
            </Button>
            <Button asChild size="sm">
              <Link to={newAppHref}>New app</Link>
            </Button>
          </>
        }
      />
      <ShipViews />
      <Loadable label="the apps" fallback={<DeploymentsHold />}>
        <DeploymentsBody />
      </Loadable>
    </div>
  );
}

/** The list as loaded: its apps by id (live statuses merged in place), their order, the next page. */
interface List {
  apps: Map<string, CdApp>;
  order: string[];
  next: string | null;
}

function DeploymentsBody() {
  const { params, setParams, filters, sort, key } = useFilters();
  const choice: Chosen = {
    sync: filters.sync ?? [],
    health: filters.health ?? [],
    cluster: filters.cluster ?? [],
    project: filters.project ?? [],
    namespace: filters.namespace ?? [],
  };
  const setChoice = (c: Chosen) => {
    const next = new URLSearchParams(params);
    for (const f of FACETS) {
      const v = c[f.id] ?? [];
      if (v.length) next.set(f.id, v.join(","));
      else next.delete(f.id);
    }
    setParams(next, { replace: true });
  };
  const setSort = (v: string) => {
    const next = new URLSearchParams(params);
    if (v && v !== "name") next.set("sort", v);
    else next.delete("sort");
    setParams(next, { replace: true });
  };
  // What's typed filters at once here and reaches the URL (and the server) a moment later.
  const [query, setQuery] = React.useState(filters.q ?? "");
  const typing = React.useRef<number | null>(null);
  const onQuery = (q: string) => {
    setQuery(q);
    if (typing.current !== null) window.clearTimeout(typing.current);
    typing.current = window.setTimeout(() => {
      typing.current = null;
      const next = new URLSearchParams(window.location.search);
      if (q) next.set("q", q);
      else next.delete("q");
      setParams(next, { replace: true });
    }, 250);
  };

  const summaryRes: Res<CdSummary> = { key: "cd/summary", fetch: (s) => cdApi.summary(s), opts: { pollMs: 30_000 } };
  const facetsRes: Res<CdFacets> = { key: `cd/facets?${key}`, fetch: (s) => cdApi.facets(filters, s), opts: { pollMs: 30_000 } };
  const appsRes: Res<{ apps: CdApp[]; next: string | null }> = { key: `cd/apps?${key}&sort=${sort}`, fetch: (s) => cdApi.apps({ ...filters, sort, limit: PAGE }, s) };
  warm(summaryRes, facetsRes, appsRes);
  const summary = useRes(summaryRes).data;
  const facets = useRes(facetsRes).data;
  const first = useRes(appsRes).data;
  const groups: Facet[] = FACETS.flatMap((f) => {
    const counts = facets.facets[f.id] ?? {};
    const order = ORDER[f.id];
    const rank = (v: string) => (order ? order.indexOf(v) + 1 || order.length + 1 : 0);
    const ids = [...new Set([...Object.keys(counts), ...(choice[f.id] ?? [])])].sort((a, b) => rank(a) - rank(b) || a.localeCompare(b));
    // A facet every app shares says nothing, unless something of it is chosen.
    if (ids.length < 2 && !(choice[f.id]?.length ?? 0) && f.id !== "sync" && f.id !== "health") return [];
    return [{ id: f.id, label: f.label, options: ids.map((id) => ({ id, label: WORDS[id] ?? id, count: counts[id] ?? 0 })) }];
  });

  // The list: the first page as loaded (again on every refresh), the pages after it as you
  // scroll, and the statuses events bring, merged in place and drawn once a frame.
  const list = React.useMemo<List>(() => ({ apps: new Map(first.apps.map((a) => [a.app, a])), order: first.apps.map((a) => a.app), next: first.next }), [first]);
  const [, setVersion] = React.useState(0);
  const [moreError, setMoreError] = React.useState<string | null>(null);
  const loadingMore = React.useRef(false);
  const failedAt = React.useRef(0);
  const frame = React.useRef(0);
  const redraw = () => {
    if (!frame.current)
      frame.current = requestAnimationFrame(() => {
        frame.current = 0;
        setVersion((v) => v + 1);
      });
  };

  const loadMore = React.useCallback(() => {
    if (!list.next || loadingMore.current || Date.now() - failedAt.current < 5_000) return;
    loadingMore.current = true;
    cdApi
      .apps({ ...filters, sort, limit: PAGE, cursor: list.next })
      .then((page) => {
        for (const a of page.apps) {
          if (!list.apps.has(a.app)) list.order.push(a.app);
          list.apps.set(a.app, a);
        }
        list.next = page.next;
        setMoreError(null);
        setVersion((v) => v + 1);
      })
      .catch((e: unknown) => {
        failedAt.current = Date.now();
        setMoreError(`More apps didn't load: ${errorMessage(e)}`);
      })
      .finally(() => {
        loadingMore.current = false;
      });
  }, [list, filters, sort]);

  useServerEvents((ev) => {
    if (ev.type === "cd_apps") {
      for (const s of ev.apps) {
        const cur = list.apps.get(s.app);
        if (cur) list.apps.set(s.app, { ...cur, ...s });
      }
      redraw();
      countsLater();
    } else if (ev.type === "cd_app_deleted") {
      if (list.apps.delete(ev.app)) {
        list.order = list.order.filter((id) => id !== ev.app);
        redraw();
      }
      countsLater();
    }
  });

  const count = list.order.length;
  const [listRef, win] = useWindowedRows(count, ROW, { onEnd: loadMore });

  // Selection: some rows, or everything the filters show (which the server acts on, loaded or
  // not). Another filter starts another selection.
  const [sel, setSel] = React.useState<{ key: string; ids: ReadonlySet<string>; everything: boolean }>({ key, ids: new Set(), everything: false });
  const selection = sel.key === key ? sel : { key, ids: new Set<string>(), everything: false };
  const [busy, setBusy] = React.useState(false);
  const [note, setNote] = React.useState<{ key: string; tone: "ok" | "fail"; text: string } | null>(null);
  const check = React.useCallback(
    (id: string, on: boolean) =>
      setSel((s) => {
        const ids = new Set(s.key === key ? s.ids : []);
        if (on) ids.add(id);
        else ids.delete(id);
        return { key, ids, everything: false };
      }),
    [key],
  );
  const selectAll = (on: boolean) => setSel({ key, ids: on ? new Set(list.order) : new Set(), everything: false });
  const allLoaded = count > 0 && selection.ids.size >= count && list.order.every((id) => selection.ids.has(id));
  const total = facets.total;

  const bulk = async (what: "sync" | "refresh") => {
    setBusy(true);
    setNote(null);
    try {
      if (selection.everything) {
        if (what === "sync") {
          const r = await cdApi.syncMany(filters);
          setNote({ key, tone: "ok", text: `Syncing ${n(r.started)} apps.` });
        } else {
          const r = await cdApi.refreshMany(filters);
          setNote({ key, tone: "ok", text: `Comparing ${n(r.refreshing)} apps again.` });
        }
      } else {
        const ids = [...selection.ids];
        const results = await settleAll(ids, 8, (id) => (what === "sync" ? cdApi.sync(id, {}) : cdApi.refresh(id, false)));
        const failed = results.flatMap((r, i) => (r.status === "rejected" ? [`${ids[i] ?? "?"}: ${errorMessage(r.reason)}`] : []));
        const done = ids.length - failed.length;
        const verb = what === "sync" ? "Syncing" : "Comparing again";
        setNote(
          failed.length
            ? { key, tone: "fail", text: `${verb} ${n(done)} of ${n(ids.length)}. ${failed.slice(0, 3).join("; ")}${failed.length > 3 ? "; …" : ""}` }
            : { key, tone: "ok", text: `${verb} ${n(done)} ${done === 1 ? "app" : "apps"}.` },
        );
      }
      setSel({ key, ids: new Set(), everything: false });
      invalidate("cd/");
    } catch (e) {
      setNote({ key, tone: "fail", text: errorMessage(e) });
    } finally {
      setBusy(false);
    }
  };

  const filtered = isFiltered(filters);
  const some = selection.everything || selection.ids.size > 0;
  // A note says what the last action did, in the count's place, until the next one or another filter.
  const said = note?.key === key ? note : null;

  return (
    <>
      <Summary s={summary} />
      <div className="cd-toolbar">
        <FacetBox
          label="Filter apps"
          placeholder="Filter by name, sync, health, project, cluster…"
          query={query}
          onQuery={onQuery}
          facets={groups}
          chosen={choice}
          onChosen={setChoice}
          className="cd-filter"
        />
        <SelectField label="Sort apps" hideLabel value={sort} onChange={setSort} options={SORTS} className="cd-sort" />
      </div>

      <div className="cd-list-head">
        <label className="cd-pick">
          <input
            type="checkbox"
            className="check"
            checked={selection.everything || allLoaded}
            ref={(el) => {
              if (el) el.indeterminate = !selection.everything && !allLoaded && selection.ids.size > 0;
            }}
            onChange={(e) => selectAll(e.target.checked)}
            disabled={count === 0}
            aria-label="Select every app shown"
          />
        </label>
        {some ? (
          <div className="cd-bulk">
            <span className="cd-count">{selection.everything ? `All ${n(total)} matching` : `${n(selection.ids.size)} selected`}</span>
            {!selection.everything && allLoaded && list.next && (
              <button type="button" className="cd-link" onClick={() => setSel({ key, ids: selection.ids, everything: true })}>
                Select all {n(total)} matching
              </button>
            )}
            <Button size="xs" onClick={() => bulk("sync")} disabled={busy}>
              Sync
            </Button>
            <Button size="xs" variant="secondary" onClick={() => bulk("refresh")} disabled={busy}>
              Refresh
            </Button>
            <button type="button" className="cd-link" onClick={() => selectAll(false)}>
              Clear
            </button>
          </div>
        ) : said ? (
          <p className="cd-notice cd-notice-inline" data-tone={said.tone} aria-live="polite">
            {said.text}
          </p>
        ) : (
          <span className="cd-count" aria-live="polite">
            {countLine(facets, summary.apps, filters)}
          </span>
        )}
      </div>

      {count === 0 ? (
        <EmptyState icon={<Rocket size={20} />} title={filtered ? "No apps match." : "No apps yet."}>
          {filtered ? (
            "Clear the filters to see every app."
          ) : (
            <>
              <Link to={newAppHref}>Build one</Link>, write it in a repository's <code>.kiln/deploy.yaml</code>, or import Argo CD Applications with <code>kiln cd import app.yaml</code>.
            </>
          )}
        </EmptyState>
      ) : (
        <div ref={listRef} className="cd-list" role="list">
          <div style={{ height: win.start * ROW }} aria-hidden="true" />
          {list.order.slice(win.start, win.end).map((id, i) => {
            const a = list.apps.get(id);
            return a ? <AppRow key={id} app={a} pos={win.start + i + 1} size={list.next ? -1 : count} checked={selection.everything || selection.ids.has(id)} onCheck={check} /> : null;
          })}
          <div style={{ height: Math.max(0, count - win.end) * ROW }} aria-hidden="true" />
        </div>
      )}
      {moreError ? (
        <p className="cd-notice" data-tone="fail">
          {moreError}
        </p>
      ) : (
        list.next && <p className="cd-more">Loading more as you scroll…</p>
      )}
    </>
  );
}

/** The list while it loads: the toolbar it will have, the head, the rows' space. */
function DeploymentsHold() {
  const { sort } = useFilters();
  const noop = () => undefined;
  return (
    <>
      <p className="cd-summary-line" aria-hidden="true">
        <span className="cd-count">{" "}</span>
      </p>
      <div className="cd-toolbar">
        <FacetBox label="Filter apps" placeholder="Filter by name, sync, health, project, cluster…" facets={[]} chosen={{}} onChosen={noop} className="cd-filter" />
        <SelectField label="Sort apps" hideLabel value={sort} onChange={noop} options={SORTS} className="cd-sort" />
      </div>
      <div className="cd-list-head">
        <label className="cd-pick">
          <input type="checkbox" className="check" disabled aria-label="Select every app shown" />
        </label>
      </div>
      <RowsHold rows={8} rowHeight={ROW} className="cd-list" label="Loading the apps" />
    </>
  );
}

const AppRow = React.memo(function AppRow({
  app,
  pos,
  size,
  checked,
  onCheck,
}: {
  app: CdApp;
  pos: number;
  size: number;
  checked: boolean;
  onCheck: (id: string, on: boolean) => void;
}) {
  const live = running(app.operation);
  const failed = app.operation?.phase === "Failed";
  const note = app.source_error ?? (failed ? app.operation?.message : app.message) ?? "";
  const tone = app.health === "Degraded" || failed || app.source_error ? "fail" : app.sync === "OutOfSync" ? "warn" : undefined;
  const extent = whatDiffers(app);
  // While it syncs, what the sync is doing.
  const doing = live ? [opWords(app.operation), app.operation?.message].filter(Boolean).join(" · ") : "";
  return (
    <div role="listitem" aria-posinset={pos} aria-setsize={size} className="cd-item" data-checked={checked || undefined} data-live={live || undefined} data-tone={tone}>
      <label className="cd-pick">
        <input type="checkbox" className="check" checked={checked} onChange={(e) => onCheck(app.app, e.target.checked)} aria-label={`Select ${app.name}`} />
      </label>
      <Link to={appHref(app.app)} className="cd-row">
        <span className="cd-row-main">
          <span className="cd-row-name">{app.name}</span>
          <span className="cd-row-meta">
            {[app.project, placeOf(app), shortRev(app.revision)].filter(Boolean).join(" · ")}
            {app.shadow ? " · shadow" : ""}
            {app.pinned ? " · rolled back" : ""}
          </span>
        </span>
        <span className="cd-row-states">
          <SyncPair sync={app.sync} op={app.operation} />
          <HealthPair health={app.health} />
        </span>
        <span className="cd-row-note" title={(live ? doing : note) || undefined}>
          {live ? <span className="cd-live-text">{doing}</span> : note || extent}
        </span>
      </Link>
    </div>
  );
});

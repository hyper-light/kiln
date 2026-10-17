/**
 * Deployments: every app kiln deploys, its sync and health, live. One list however large the
 * fleet: filters and sorts run on the server, pages arrive as you scroll, only the rows on screen
 * are drawn, and live updates are merged in once a frame. Above it, the fleet's counts, then the
 * console's one filter field (names as you type; sync, health, project, cluster and namespace as
 * checkboxes, each value with how many it would show) and a sort select. Rows have checkboxes, so
 * a selection, or everything a filter shows, syncs or refreshes at once.
 */
import * as React from "react";
import { Link, useSearchParams } from "react-router";
import { Rocket } from "lucide-react";

import { appHref, cdApi, plansHref, type CdFilters, type CdSort } from "@/api/cd";
import { useServerEvents } from "@/api/events";
import type { CdApp, CdFacets, CdSummary } from "@/api/types";
import { Button } from "@/components/ui/button";
import { FacetBox, type Chosen, type Facet } from "@/components/ui/facet-box";
import { EmptyState, ErrorState, PageIntro, SelectField, SkeletonRows } from "@/components/ui/misc";
import { errorMessage, useResource } from "@/lib/hooks";
import { useWindowedRows } from "@/lib/virtual";
import { HEALTH_TONE, HealthPair, StatePair, SYNC_TONE, SyncPair, opWords, placeOf, running, shortRev } from "./parts";
import "./deployments.css";

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

/** The fleet at a glance: how many apps, and how many in each sync and health. */
function Summary({ s }: { s: CdSummary | undefined }) {
  if (!s || s.apps === 0) return null;
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
function countLine(f: CdFacets | undefined, all: number | undefined, filters: CdFilters) {
  if (!f) return "";
  const filtered = Object.values(filters).some((v) => (Array.isArray(v) ? v.length > 0 : Boolean(v)));
  const parts = [filtered && all !== undefined ? `${n(f.total)} of ${n(all)} apps` : `${n(f.total)} ${f.total === 1 ? "app" : "apps"}`];
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

export default function Deployments() {
  const [params, setParams] = useSearchParams();
  const filters = React.useMemo<CdFilters>(() => {
    const list = (k: string) => (params.get(k) ?? "").split(",").filter(Boolean);
    return {
      sync: list("sync"),
      health: list("health"),
      cluster: list("cluster"),
      project: list("project"),
      namespace: list("namespace"),
      q: params.get("q") ?? "",
    };
  }, [params]);
  const sort = (SORTS.find((x) => x.value === params.get("sort"))?.value ?? "name") as CdSort;
  const setSort = (v: string) => {
    const next = new URLSearchParams(params);
    if (v && v !== "name") next.set("sort", v);
    else next.delete("sort");
    setParams(next, { replace: true });
  };
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
  const [query, setQuery] = React.useState(filters.q ?? "");
  React.useEffect(() => {
    const t = setTimeout(() => {
      if (query === (filters.q ?? "")) return;
      const next = new URLSearchParams(params);
      if (query) next.set("q", query);
      else next.delete("q");
      setParams(next, { replace: true });
    }, 250);
    return () => clearTimeout(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [query]);

  const summary = useResource((s) => cdApi.summary(s), [], { pollMs: 30_000 });
  const facets = useResource((s) => cdApi.facets(filters, s), [filters], { pollMs: 30_000 });
  const groups: Facet[] = FACETS.flatMap((f) => {
    const counts = facets.data?.facets[f.id] ?? {};
    const order = ORDER[f.id];
    const rank = (v: string) => (order ? order.indexOf(v) + 1 || order.length + 1 : 0);
    const ids = [...new Set([...Object.keys(counts), ...(choice[f.id] ?? [])])].sort((a, b) => rank(a) - rank(b) || a.localeCompare(b));
    // A facet every app shares says nothing, unless something of it is chosen.
    if (ids.length < 2 && !(choice[f.id]?.length ?? 0) && f.id !== "sync" && f.id !== "health") return [];
    return [{ id: f.id, label: f.label, options: ids.map((id) => ({ id, label: WORDS[id] ?? id, count: counts[id] ?? 0 })) }];
  });

  // The list: pages in id order, kept in a map that live events update in place. `gen` drops
  // pages of a list the filters have since replaced.
  const apps = React.useRef(new Map<string, CdApp>());
  const order = React.useRef<string[]>([]);
  const gen = React.useRef(0);
  const [version, setVersion] = React.useState(0);
  const [next, setNext] = React.useState<string | null>(null);
  const [loading, setLoading] = React.useState(true);
  const [error, setError] = React.useState<Error | null>(null);
  const [moreError, setMoreError] = React.useState<string | null>(null);
  const loadingMore = React.useRef(false);
  const failedAt = React.useRef(0);

  React.useEffect(() => {
    const ctl = new AbortController();
    const g = ++gen.current;
    setLoading(true);
    setError(null);
    cdApi
      .apps({ ...filters, sort, limit: PAGE }, ctl.signal)
      .then((page) => {
        if (g !== gen.current) return;
        apps.current = new Map(page.apps.map((a) => [a.app, a]));
        order.current = page.apps.map((a) => a.app);
        setNext(page.next);
        setLoading(false);
        setVersion((v) => v + 1);
      })
      .catch((e: unknown) => {
        if (ctl.signal.aborted) return;
        setError(e instanceof Error ? e : new Error(String(e)));
        setLoading(false);
      });
    return () => ctl.abort();
  }, [filters, sort]);

  const loadMore = React.useCallback(() => {
    if (!next || loadingMore.current || Date.now() - failedAt.current < 5_000) return;
    loadingMore.current = true;
    const g = gen.current;
    cdApi
      .apps({ ...filters, sort, limit: PAGE, cursor: next })
      .then((page) => {
        if (g !== gen.current) return;
        for (const a of page.apps) {
          if (!apps.current.has(a.app)) order.current.push(a.app);
          apps.current.set(a.app, a);
        }
        setNext(page.next);
        setMoreError(null);
        setVersion((v) => v + 1);
      })
      .catch((e: unknown) => {
        if (g !== gen.current) return;
        failedAt.current = Date.now();
        setMoreError(`More apps didn't load: ${errorMessage(e)}`);
      })
      .finally(() => {
        loadingMore.current = false;
      });
  }, [filters, sort, next]);

  // Live: statuses arrive in batches; merge them and draw once a frame.
  const frame = React.useRef(0);
  const countsSoon = React.useRef<number | null>(null);
  useServerEvents((ev) => {
    if (ev.type === "cd_apps") {
      for (const s of ev.apps) {
        const cur = apps.current.get(s.app);
        if (cur) apps.current.set(s.app, { ...cur, ...s });
      }
      if (!frame.current) {
        frame.current = requestAnimationFrame(() => {
          frame.current = 0;
          setVersion((v) => v + 1);
        });
      }
      if (countsSoon.current === null) {
        countsSoon.current = window.setTimeout(() => {
          countsSoon.current = null;
          summary.reload();
          facets.reload();
        }, 2_000);
      }
    } else if (ev.type === "cd_app_deleted") {
      if (apps.current.delete(ev.app)) {
        order.current = order.current.filter((id) => id !== ev.app);
        setVersion((v) => v + 1);
      }
      summary.reload();
      facets.reload();
    }
  });
  React.useEffect(
    () => () => {
      if (frame.current) cancelAnimationFrame(frame.current);
      if (countsSoon.current !== null) clearTimeout(countsSoon.current);
    },
    [],
  );

  const listRef = React.useRef<HTMLDivElement>(null);
  const count = order.current.length;
  const win = useWindowedRows(listRef, count, ROW);
  React.useEffect(() => {
    if (win.end >= count - 50) loadMore();
  }, [win.end, count, loadMore]);

  // Selection: some rows, or everything the filters show (which the server acts on, loaded or
  // not). A new filter starts a new selection.
  const [selected, setSelected] = React.useState<ReadonlySet<string>>(() => new Set());
  const [everything, setEverything] = React.useState(false);
  const [busy, setBusy] = React.useState(false);
  const [note, setNote] = React.useState<{ tone: "ok" | "fail"; text: string } | null>(null);
  React.useEffect(() => {
    setSelected(new Set());
    setEverything(false);
  }, [filters]);
  const check = React.useCallback((id: string, on: boolean) => {
    setEverything(false);
    setSelected((s) => {
      const next = new Set(s);
      if (on) next.add(id);
      else next.delete(id);
      return next;
    });
  }, []);
  const selectAll = (on: boolean) => {
    setEverything(false);
    setSelected(on ? new Set(order.current) : new Set());
  };
  const allLoaded = count > 0 && selected.size >= count && order.current.every((id) => selected.has(id));
  const total = facets.data?.total;

  const bulk = async (what: "sync" | "refresh") => {
    setBusy(true);
    setNote(null);
    try {
      if (everything) {
        if (what === "sync") {
          const r = await cdApi.syncMany(filters);
          setNote({ tone: "ok", text: `Syncing ${n(r.started)} apps.` });
        } else {
          const r = await cdApi.refreshMany(filters);
          setNote({ tone: "ok", text: `Comparing ${n(r.refreshing)} apps again.` });
        }
      } else {
        const ids = [...selected];
        const results = await settleAll(ids, 8, (id) => (what === "sync" ? cdApi.sync(id, {}) : cdApi.refresh(id, false)));
        const failed = results.flatMap((r, i) => (r.status === "rejected" ? [`${ids[i] ?? "?"}: ${errorMessage(r.reason)}`] : []));
        const done = ids.length - failed.length;
        const verb = what === "sync" ? "Syncing" : "Comparing again";
        setNote(
          failed.length
            ? { tone: "fail", text: `${verb} ${n(done)} of ${n(ids.length)}. ${failed.slice(0, 3).join("; ")}${failed.length > 3 ? "; …" : ""}` }
            : { tone: "ok", text: `${verb} ${n(done)} ${done === 1 ? "app" : "apps"}.` },
        );
      }
      setSelected(new Set());
      setEverything(false);
    } catch (e) {
      setNote({ tone: "fail", text: errorMessage(e) });
    } finally {
      setBusy(false);
    }
  };

  const filtered = Object.values(filters).some((v) => (Array.isArray(v) ? v.length > 0 : Boolean(v)));
  const some = everything || selected.size > 0;

  return (
    <div className="container-console page cd-page">
      <PageIntro
        index={3}
        label="Deployments"
        title="Deployments."
        lead="What git says, and what runs: every app's sync and health, live."
        actions={
          <Button asChild size="sm" variant="secondary">
            <Link to={plansHref}>Plan a deploy</Link>
          </Button>
        }
      />
      <Summary s={summary.data} />
      <div className="cd-toolbar">
        <FacetBox
          label="Filter apps"
          placeholder="Filter by name, sync, health, project, cluster…"
          query={query}
          onQuery={setQuery}
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
            checked={everything || allLoaded}
            ref={(el) => {
              if (el) el.indeterminate = !everything && !allLoaded && selected.size > 0;
            }}
            onChange={(e) => selectAll(e.target.checked)}
            disabled={count === 0}
            aria-label="Select every app shown"
          />
        </label>
        {some ? (
          <div className="cd-bulk">
            <span className="cd-count">{everything && total !== undefined ? `All ${n(total)} matching` : `${n(selected.size)} selected`}</span>
            {!everything && allLoaded && next && total !== undefined && (
              <button type="button" className="cd-link" onClick={() => setEverything(true)}>
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
        ) : (
          <span className="cd-count" aria-live="polite">
            {countLine(facets.data, summary.data?.apps, filters)}
          </span>
        )}
      </div>
      {note && (
        <p className="cd-notice" data-tone={note.tone} aria-live="polite">
          {note.text}
        </p>
      )}

      {error ? (
        <ErrorState error={error} onRetry={() => setParams(new URLSearchParams(params), { replace: true })} />
      ) : loading ? (
        <SkeletonRows rows={8} cols={4} />
      ) : count === 0 ? (
        <EmptyState icon={<Rocket size={20} />} title={filtered ? "No apps match." : "No apps yet."}>
          {filtered ? (
            "Clear the filters to see every app."
          ) : (
            <>
              Add one with <code>kiln cd import app.yaml</code> (Argo CD Applications), the API, or an agent through kiln's MCP
              server.
            </>
          )}
        </EmptyState>
      ) : (
        <div ref={listRef} className="cd-list" role="list" data-version={version}>
          <div style={{ height: win.start * ROW }} aria-hidden="true" />
          {order.current.slice(win.start, win.end).map((id, i) => {
            const a = apps.current.get(id);
            return a ? (
              <AppRow key={id} app={a} pos={win.start + i + 1} size={next ? -1 : count} checked={everything || selected.has(id)} onCheck={check} />
            ) : null;
          })}
          <div style={{ height: Math.max(0, count - win.end) * ROW }} aria-hidden="true" />
        </div>
      )}
      {moreError ? <p className="cd-notice" data-tone="fail">{moreError}</p> : next && !loading && <p className="cd-more">Loading more as you scroll…</p>}
    </div>
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
  const extent = app.out_of_sync ? `${app.out_of_sync} of ${app.resources ?? 0} differ` : app.resources !== undefined ? `${app.resources} resources` : "";
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
          <SyncPair sync={app.sync} />
          <HealthPair health={app.health} />
        </span>
        <span className="cd-row-note" title={note || undefined}>
          {live ? opWords(app.operation) : note || extent}
        </span>
      </Link>
    </div>
  );
});

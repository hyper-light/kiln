/**
 * Deployments: every app kiln deploys, its sync and health, live. One list however large the
 * fleet: filters run on the server, pages arrive as you scroll, only the rows on screen are
 * drawn, and live updates are merged in once a frame. On phones the filters fold into one line
 * that opens a sheet.
 */
import * as React from "react";
import { Link, useSearchParams } from "react-router";
import { ChevronDown, Rocket } from "lucide-react";

import { appHref, cdApi } from "@/api/cd";
import { useServerEvents } from "@/api/events";
import type { CdApp, CdSummary } from "@/api/types";
import { Dialog, DialogContent } from "@/components/ui/dialog";
import { EmptyState, ErrorState, PageIntro, SearchField, SelectField, SkeletonRows } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { errorMessage, useResource } from "@/lib/hooks";
import { useWindowedRows } from "@/lib/virtual";
import { HealthPair, SyncPair, opWords, placeOf, running, shortRev } from "./parts";
import "./deployments.css";

const ROW = 68;
const PAGE = 500;

const SYNC_OPTIONS = [
  { value: "", label: "Any sync" },
  { value: "Synced", label: "Synced" },
  { value: "OutOfSync", label: "Out of sync" },
  { value: "Unknown", label: "Unknown" },
];
const HEALTH_OPTIONS = [
  { value: "", label: "Any health" },
  { value: "Healthy", label: "Healthy" },
  { value: "Progressing", label: "Progressing" },
  { value: "Degraded", label: "Degraded" },
  { value: "Missing", label: "Missing" },
  { value: "Suspended", label: "Suspended" },
  { value: "Unknown", label: "Unknown" },
];

function summaryLine(s: CdSummary | undefined) {
  if (!s) return "Deployments";
  const parts = [`${s.apps.toLocaleString()} ${s.apps === 1 ? "app" : "apps"}`];
  const out = s.sync.OutOfSync ?? 0;
  if (out) parts.push(`${out.toLocaleString()} out of sync`);
  for (const h of ["Degraded", "Missing", "Progressing"]) {
    const n = s.health[h] ?? 0;
    if (n) parts.push(`${n.toLocaleString()} ${h.toLowerCase()}`);
  }
  return parts.join(" · ");
}

export default function Deployments() {
  const [params, setParams] = useSearchParams();
  const filters = React.useMemo(
    () => ({
      sync: params.get("sync") ?? "",
      health: params.get("health") ?? "",
      cluster: params.get("cluster") ?? "",
      q: params.get("q") ?? "",
    }),
    [params],
  );
  const setFilter = (k: keyof typeof filters, v: string) => {
    const next = new URLSearchParams(params);
    if (v) next.set(k, v);
    else next.delete(k);
    setParams(next, { replace: true });
  };
  const [query, setQuery] = React.useState(filters.q);
  React.useEffect(() => {
    const t = setTimeout(() => {
      if (query !== filters.q) setFilter("q", query);
    }, 250);
    return () => clearTimeout(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [query]);

  const summary = useResource((s) => cdApi.summary(s), [], { pollMs: 30_000 });
  const clusters = useResource((s) => cdApi.clusters(s), [], { pollMs: 60_000 });

  // The list: pages in id order, kept in a map that live events update in place. `gen` drops
  // pages of a list the filters have since replaced.
  const apps = React.useRef(new Map<string, CdApp>());
  const order = React.useRef<string[]>([]);
  const gen = React.useRef(0);
  const [version, setVersion] = React.useState(0);
  const [next, setNext] = React.useState<string | null>(null);
  const [loading, setLoading] = React.useState(true);
  const [error, setError] = React.useState<Error | null>(null);
  const loadingMore = React.useRef(false);
  const failedAt = React.useRef(0);

  React.useEffect(() => {
    const ctl = new AbortController();
    const g = ++gen.current;
    setLoading(true);
    setError(null);
    cdApi
      .apps({ ...filters, limit: PAGE }, ctl.signal)
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
  }, [filters]);

  const loadMore = React.useCallback(() => {
    if (!next || loadingMore.current || Date.now() - failedAt.current < 5_000) return;
    loadingMore.current = true;
    const g = gen.current;
    cdApi
      .apps({ ...filters, limit: PAGE, cursor: next })
      .then((page) => {
        if (g !== gen.current) return;
        for (const a of page.apps) {
          if (!apps.current.has(a.app)) order.current.push(a.app);
          apps.current.set(a.app, a);
        }
        setNext(page.next);
        setVersion((v) => v + 1);
      })
      .catch((e: unknown) => {
        if (g !== gen.current) return;
        failedAt.current = Date.now();
        toast({ title: "More apps didn't load", description: errorMessage(e), tone: "failed" });
      })
      .finally(() => {
        loadingMore.current = false;
      });
  }, [filters, next]);

  // Live: statuses arrive in batches; merge them and draw once a frame.
  const frame = React.useRef(0);
  const reloadSummarySoon = React.useRef<number | null>(null);
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
      if (reloadSummarySoon.current === null) {
        reloadSummarySoon.current = window.setTimeout(() => {
          reloadSummarySoon.current = null;
          summary.reload();
        }, 2_000);
      }
    } else if (ev.type === "cd_app_deleted") {
      if (apps.current.delete(ev.app)) {
        order.current = order.current.filter((id) => id !== ev.app);
        setVersion((v) => v + 1);
      }
      summary.reload();
    }
  });
  React.useEffect(
    () => () => {
      if (frame.current) cancelAnimationFrame(frame.current);
      if (reloadSummarySoon.current !== null) clearTimeout(reloadSummarySoon.current);
    },
    [],
  );

  const listRef = React.useRef<HTMLDivElement>(null);
  const count = order.current.length;
  const win = useWindowedRows(listRef, count, ROW);
  React.useEffect(() => {
    if (win.end >= count - 50) loadMore();
  }, [win.end, count, loadMore]);

  const [sheet, setSheet] = React.useState(false);
  const clusterOptions = [
    { value: "", label: "Any cluster" },
    ...(clusters.data?.clusters ?? []).map((c) => ({ value: c.name, label: c.name })),
  ];
  const filtered = Object.values(filters).some(Boolean);
  const line = summaryLine(summary.data);

  const controls = (
    <>
      <SearchField label="Search apps" placeholder="Search apps" value={query} onChange={setQuery} className="cd-search" />
      <SelectField label="Sync" hideLabel value={filters.sync} onChange={(v) => setFilter("sync", v)} options={SYNC_OPTIONS} />
      <SelectField label="Health" hideLabel value={filters.health} onChange={(v) => setFilter("health", v)} options={HEALTH_OPTIONS} />
      <SelectField label="Cluster" hideLabel value={filters.cluster} onChange={(v) => setFilter("cluster", v)} options={clusterOptions} />
    </>
  );

  return (
    <div className="container-console page cd-page">
      <PageIntro index={3} label="Deployments" title="Deployments." lead="What git says, and what runs: every app's sync and health, live." />
      <div className="cd-toolbar">{controls}</div>
      <button type="button" className="cd-summary" onClick={() => setSheet(true)} aria-haspopup="dialog">
        <span className="sr-only">Filter apps: </span>
        <span className="cd-summary-text">{line}</span>
        <ChevronDown size={12} aria-hidden="true" />
      </button>
      <Dialog open={sheet} onOpenChange={setSheet}>
        {sheet && (
          <DialogContent title="Filter apps" eyebrow={line} className="cd-sheet">
            <div className="cd-sheet-body">{controls}</div>
          </DialogContent>
        )}
      </Dialog>
      <p className="cd-count" aria-live="polite">
        {line}
      </p>

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
            return a ? <AppRow key={id} app={a} pos={win.start + i + 1} size={next ? -1 : count} /> : null;
          })}
          <div style={{ height: Math.max(0, count - win.end) * ROW }} aria-hidden="true" />
        </div>
      )}
      {next && !loading && <p className="cd-more">Loading more as you scroll…</p>}
    </div>
  );
}

const AppRow = React.memo(function AppRow({ app, pos, size }: { app: CdApp; pos: number; size: number }) {
  const live = running(app.operation);
  const failed = app.operation?.phase === "Failed";
  const note = app.source_error ?? (failed ? app.operation?.message : app.message) ?? "";
  const tone = app.health === "Degraded" || failed || app.source_error ? "fail" : app.sync === "OutOfSync" ? "warn" : undefined;
  const extent = app.out_of_sync ? `${app.out_of_sync} of ${app.resources ?? 0} differ` : app.resources !== undefined ? `${app.resources} resources` : "";
  return (
    <div role="listitem" aria-posinset={pos} aria-setsize={size}>
      <Link to={appHref(app.app)} className="cd-row" data-live={live || undefined} data-tone={tone}>
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

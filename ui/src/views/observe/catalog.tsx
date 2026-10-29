/**
 * The Service Catalog (docs/observe.md §10.8), Datadog's: every service, who owns it and how it is
 * doing. Defined in Datadog's service.datadog.yaml (in a repository kiln mirrors, or sent to its
 * API), or seen by its traces and deployments; each scored against Datadog's three scorecards.
 */
import * as React from "react";
import { Plus } from "lucide-react";
import { observeApi, type CatalogService } from "@/api/observe";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent } from "@/components/ui/dialog";
import { TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { PageIntro, SelectField } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { useRes, type Res } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { Link, useNavigate } from "@/lib/router";
import { ObserveNav, compact, formatSeconds, useParam } from "./parts";

export const catalogRes: Res<CatalogService[]> = { key: "observe/catalog", fetch: (s) => observeApi.catalog(s), opts: { pollMs: 60_000 } };

const SOURCE: Record<CatalogService["source"], string> = { repository: "repository", api: "API", seen: "seen" };

/** A template of a definition, Datadog's v2.2. */
export const TEMPLATE = `schema-version: v2.2
dd-service: my-service
team: my-team
description: What it does.
tier: "2"
lifecycle: production
contacts:
  - type: email
    contact: my-team@example.com
links:
  - name: Runbook
    type: runbook
    url: https://wiki.example.com/my-service/runbook
`;

export default function Catalog() {
  const [team, setTeam] = useParam("team", "");
  const [q, setQ] = useParam("q", "");
  const [defining, setDefining] = React.useState(false);
  return (
    <div className="container-console page">
      <title>Catalog · Observe · kiln</title>
      <PageIntro
        index={8}
        label="Observe"
        title="Catalog"
        lead="Every service, who owns it and how it is doing: defined in a service.datadog.yaml (in a repository kiln mirrors, or sent to Datadog's API), or seen by its traces and deployments; each scored against Datadog's three scorecards."
        actions={
          <Button onClick={() => setDefining(true)}>
            <Plus /> Define a service
          </Button>
        }
      />
      <ObserveNav />
      <Loadable label="the catalog" fallback={<TableHold head={<Head />} rows={6} />}>
        <List team={team} q={q} onTeam={setTeam} onQ={setQ} />
      </Loadable>
      {defining && <DefineDialog initial={TEMPLATE} title="Define a service" onClose={() => setDefining(false)} />}
    </div>
  );
}

function Head() {
  return (
    <tr>
      <th>Service</th>
      <th className="hidden text-right md:table-cell">Requests</th>
      <th className="hidden text-right sm:table-cell">Issues</th>
      <th className="hidden sm:table-cell">Monitors</th>
      <th className="hidden text-right lg:table-cell">Deploys</th>
      <th className="hidden sm:table-cell">Score</th>
    </tr>
  );
}

/** A service's score: its rules passed of all, as a short bar and words. */
export function Score({ passed, total }: { passed: number; total: number }) {
  const share = total > 0 ? passed / total : 0;
  return (
    <span className="inline-flex items-center gap-2 whitespace-nowrap" title={`${passed} of ${total} rules passed`}>
      <span className="relative inline-block h-1.5 w-14 overflow-hidden rounded-full bg-white/[0.08]" aria-hidden="true">
        <span className="absolute inset-y-0 left-0 rounded-full bg-[#a8c9bc]" style={{ width: `${Math.round(share * 100)}%` }} />
      </span>
      <span className="font-mono text-[12px]">
        {passed}/{total}
      </span>
    </span>
  );
}

function List({ team, q, onTeam, onQ }: { team: string; q: string; onTeam: (t: string) => void; onQ: (q: string) => void }) {
  const all = useRes(catalogRes).data;
  const teams = [...new Set(all.flatMap((s) => (s.team ? [s.team] : [])))].sort();
  const needle = q.trim().toLowerCase();
  const shown = all.filter((s) => (!team || s.team === team) && (!needle || s.service.toLowerCase().includes(needle) || (s.description ?? "").toLowerCase().includes(needle)));
  if (all.length === 0)
    return (
      <p className="empty-line py-12 text-center">
        No services yet. A service is listed once a <code>service.datadog.yaml</code> defines it, or it sends traces or is deployed.
      </p>
    );
  return (
    <>
      <div className="mb-4 flex flex-wrap items-end gap-3">
        <div className="field min-w-[14rem]">
          <input aria-label="Search services" value={q} onChange={(e) => onQ(e.target.value)} placeholder="Search services" spellCheck={false} />
        </div>
        {teams.length > 0 && (
          <SelectField label="Team" hideLabel value={team} onChange={onTeam} options={[{ value: "", label: "Every team" }, ...teams.map((t) => ({ value: t, label: t }))]} className="min-w-[10rem]" />
        )}
      </div>
      <div className="table-wrap">
        <table className="data-table">
          <thead>
            <Head />
          </thead>
          <tbody>
            {shown.map((s) => (
              <tr key={s.service}>
                <td className="truncate-cell">
                  <Link to={`/observe/catalog/${encodeURIComponent(s.service)}`} className="text-[13px] text-[#e4e5e7] hover:text-white">
                    {s.service}
                  </Link>
                  <p className="truncate text-[11px] text-[#8c8f98]">{[s.team ?? "no team", s.tier ? `tier ${s.tier}` : "", s.lifecycle ?? "", SOURCE[s.source]].filter(Boolean).join(" · ")}</p>
                  {s.description && <p className="truncate text-[11.5px] text-[#a7aab3]">{s.description}</p>}
                  <p className="truncate text-[11px] text-[#8c8f98] sm:hidden">
                    {s.score.passed}/{s.score.total} rules
                    {s.alerting > 0 ? ` · ${s.alerting} alerting` : ""}
                    {s.open_issues > 0 ? ` · ${s.open_issues} open ${s.open_issues === 1 ? "issue" : "issues"}` : ""}
                  </p>
                </td>
                <td className="hidden text-right font-mono text-[12px] whitespace-nowrap md:table-cell">
                  {s.requests > 0 ? (
                    <>
                      {compact(s.requests)}
                      <div className={s.error_rate ? "text-[#f27a7a]" : "text-[#8c8f98]"}>{s.error_rate ? `${compact(s.error_rate * 100)}% errors` : "no errors"}</div>
                    </>
                  ) : (
                    <span className="text-[#8c8f98]">—</span>
                  )}
                </td>
                <td className="hidden text-right font-mono text-[12px] sm:table-cell">{s.open_issues > 0 ? s.open_issues : <span className="text-[#8c8f98]">—</span>}</td>
                <td className="hidden text-[12px] whitespace-nowrap sm:table-cell">
                  {s.monitors === 0 ? (
                    <span className="text-[#8c8f98]">none</span>
                  ) : s.alerting > 0 ? (
                    <span className="status">
                      <i className="dot" data-status="failed" aria-hidden="true" />
                      <span className="status-text" data-status="failed">
                        {s.alerting} of {s.monitors} alerting
                      </span>
                    </span>
                  ) : (
                    <span className="status">
                      <i className="dot" data-status="succeeded" aria-hidden="true" />
                      <span className="status-text" data-status="succeeded">
                        {s.monitors} OK
                      </span>
                    </span>
                  )}
                </td>
                <td className="hidden text-right font-mono text-[12px] lg:table-cell">{s.deployments_30d > 0 ? s.deployments_30d : <span className="text-[#8c8f98]">—</span>}</td>
                <td className="hidden sm:table-cell">
                  <Score passed={s.score.passed} total={s.score.total} />
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {shown.length === 0 && <p className="empty-line py-8 text-center">No service matches.</p>}
      <p className="mt-3 text-[12.5px] text-[#8c8f98]">Requests are the last hour's; deploys the last 30 days'. {latencyNote(all)}</p>
    </>
  );
}

function latencyNote(all: CatalogService[]) {
  const slow = all.filter((s) => s.p95_s !== null).sort((a, b) => (b.p95_s ?? 0) - (a.p95_s ?? 0))[0];
  return slow?.p95_s ? `The slowest, ${slow.service}: p95 ${formatSeconds(slow.p95_s)}.` : "";
}

/** Definitions made or changed: Datadog's v2.2, YAML (several `---` apart) or JSON. */
export function DefineDialog({ initial, title, onClose }: { initial: string; title: string; onClose: () => void }) {
  const [text, setText] = React.useState(initial);
  const [busy, setBusy] = React.useState(false);
  const navigate = useNavigate();
  const save = async () => {
    setBusy(true);
    try {
      const r = await observeApi.defineServices(text);
      invalidate("observe/catalog");
      onClose();
      const first = (r.data[0] as { id?: string } | undefined)?.id;
      if (first) navigate(`/observe/catalog/${encodeURIComponent(first)}`);
    } catch (e) {
      toast({ title: "Couldn't define it", description: errorMessage(e), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent title={title} description="Datadog's service definition (v2.2), as a service.datadog.yaml says it: YAML, several --- apart, or JSON.">
        <div className="space-y-4">
          <div>
            <label htmlFor="service-definition" className="field-label">
              Definition
            </label>
            <textarea id="service-definition" rows={14} className="font-mono" value={text} onChange={(e) => setText(e.target.value)} spellCheck={false} />
          </div>
          <div className="flex justify-end gap-2">
            <Button variant="ghost" onClick={onClose}>
              Cancel
            </Button>
            <Button disabled={busy || !text.trim()} onClick={() => void save()}>
              Save
            </Button>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}

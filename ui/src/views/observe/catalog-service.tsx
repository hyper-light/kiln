/**
 * A service in the catalog (docs/observe.md §10.8): who owns it and how to reach them, how it is
 * doing (its requests, errors, issues, monitors, SLOs and deployments), and each of Datadog's
 * scorecards' rules, passed or not and why.
 */
import * as React from "react";
import { Check, X } from "lucide-react";
import { observeApi, type CatalogServiceDetail } from "@/api/observe";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/dialog";
import { Hold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, Facts, Panel } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { useRes, type Res } from "@/lib/data";
import { relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { Link, useNavigate, useParams } from "@/lib/router";
import { DefineDialog, Score } from "./catalog";
import { StateBadge, compact, formatSeconds } from "./parts";

const serviceRes = (name: string): Res<CatalogServiceDetail> => ({ key: `observe/catalog/${name}`, fetch: (s) => observeApi.catalogService(name, s), opts: { pollMs: 60_000 } });

export default function CatalogServicePage() {
  const { name = "" } = useParams();
  return (
    <div className="container-console page">
      <title>{`${name} · Catalog · Observe · kiln`}</title>
      <Loadable key={name} label="the service" fallback={<Hold height={480} />}>
        <Service name={name} />
      </Loadable>
    </div>
  );
}

function Service({ name }: { name: string }) {
  const s = useRes(serviceRes(name)).data;
  const [editing, setEditing] = React.useState(false);
  const [removing, setRemoving] = React.useState(false);
  const navigate = useNavigate();
  const d = s.definition?.attributes.schema;
  const where =
    s.source === "repository" ? (
      <>
        defined in <span className="font-mono">{s.repo}</span>'s service.datadog.yaml
      </>
    ) : s.source === "api" ? (
      "defined through the API"
    ) : (
      "seen by its traces or deployments: no definition yet"
    );
  const byCard = new Map<string, CatalogServiceDetail["rules"]>();
  for (const r of s.rules) byCard.set(r.scorecard, [...(byCard.get(r.scorecard) ?? []), r]);
  return (
    <>
      <header className="page-intro">
        <div className="min-w-0">
          <Crumbs index={8} items={[{ label: "Observe", to: "/observe" }, { label: "Catalog", to: "/observe/catalog" }, { label: name }]} />
          <h1>{name}</h1>
          <p className="lead max-w-3xl">
            {s.description ? `${s.description} ` : ""}
            <span className="text-[#8c8f98]">{where}.</span>
          </p>
        </div>
      </header>
      <Facts
        className="mb-8"
        items={[
          { label: "Team", value: s.team ?? "—" },
          { label: "Tier", value: s.tier ?? "—" },
          { label: "Lifecycle", value: s.lifecycle ?? "—" },
          { label: "Requests (an hour)", value: s.requests > 0 ? compact(s.requests) : "—" },
          { label: "Errors", value: s.error_rate !== null ? `${compact(s.error_rate * 100)}%` : "—", tone: s.error_rate ? "fail" : undefined },
          { label: "p95", value: s.p95_s !== null ? formatSeconds(s.p95_s) : "—" },
          { label: "Open issues", value: String(s.open_issues), tone: s.open_issues > 0 ? "fail" : undefined },
          { label: "Deploys (30 days)", value: String(s.deployments_30d) },
        ]}
      />
      <section aria-label="Scorecards" className="mb-8">
        <div className="mb-3 flex items-center gap-3">
          <h2 className="field-label">Scorecards</h2>
          <Score passed={s.score.passed} total={s.score.total} />
        </div>
        <div className="grid grid-cols-1 gap-4 lg:grid-cols-3">
          {[...byCard.entries()].map(([card, rules]) => (
            <Panel key={card} title={card}>
              <ul className="space-y-2">
                {rules.map((r) => (
                  <li key={r.id} className="flex items-start gap-2 text-[12.5px]">
                    {r.passed ? <Check size={14} className="mt-0.5 shrink-0 text-[#a8c9bc]" aria-hidden="true" /> : <X size={14} className="mt-0.5 shrink-0 text-[#d1aaad]" aria-hidden="true" />}
                    <span className="min-w-0">
                      <span className="text-[#e4e5e7]">{r.name}</span>
                      <span className="sr-only">{r.passed ? ": passed" : ": failed"}</span>
                      <span className="block text-[11.5px] text-[#8c8f98]">{r.why}</span>
                    </span>
                  </li>
                ))}
              </ul>
            </Panel>
          ))}
        </div>
      </section>
      <div className="mb-8 grid grid-cols-1 gap-4 lg:grid-cols-3">
        <Panel title="Owners and links">
          {d && ((d.contacts?.length ?? 0) > 0 || (d.links?.length ?? 0) > 0 || d.integrations) ? (
            <ul className="space-y-1.5 text-[12.5px]">
              {(d.contacts ?? []).map((c) => (
                <li key={`${c.type}:${c.contact}`} className="truncate">
                  <span className="text-[#8c8f98]">{c.name ?? c.type}</span> {c.type === "email" ? <a href={`mailto:${c.contact}`}>{c.contact}</a> : <a href={c.contact}>{c.contact}</a>}
                </li>
              ))}
              {(d.links ?? []).map((l) => (
                <li key={l.url} className="truncate">
                  <span className="text-[#8c8f98]">{l.type}</span> <a href={l.url}>{l.name}</a>
                </li>
              ))}
              {d.integrations?.pagerduty && (
                <li className="truncate">
                  <span className="text-[#8c8f98]">on-call</span> <a href={d.integrations.pagerduty["service-url"]}>PagerDuty</a>
                </li>
              )}
              {d.integrations?.opsgenie && (
                <li className="truncate">
                  <span className="text-[#8c8f98]">on-call</span> <a href={d.integrations.opsgenie["service-url"]}>Opsgenie</a>
                </li>
              )}
            </ul>
          ) : (
            <p className="empty-line">None in its definition.</p>
          )}
        </Panel>
        <Panel title="What reads it">
          {s.monitor_list.length === 0 && s.slo_list.length === 0 ? (
            <p className="empty-line">No monitor or SLO reads service:{name}.</p>
          ) : (
            <ul className="space-y-1.5 text-[12.5px]">
              {s.monitor_list.map((m) => (
                <li key={m.id} className="flex min-w-0 items-center justify-between gap-2">
                  <Link to={`/observe/monitors/${encodeURIComponent(m.id)}`} className="truncate">
                    {m.name}
                  </Link>
                  <StateBadge state={m.state} />
                </li>
              ))}
              {s.slo_list.map((x) => (
                <li key={x.id} className="truncate">
                  <span className="text-[#8c8f98]">SLO</span> <Link to={`/observe/slos/${encodeURIComponent(x.id)}`}>{x.name}</Link>
                </li>
              ))}
            </ul>
          )}
        </Panel>
        <Panel
          title="Deployments"
          actions={
            s.deployments.length > 0 ? (
              <Link to={`/observe/delivery?service=${encodeURIComponent(name)}`} className="text-[12px]">
                DORA
              </Link>
            ) : undefined
          }
        >
          {s.deployments.length === 0 ? (
            <p className="empty-line">None tracked.</p>
          ) : (
            <ul className="space-y-1.5 text-[12.5px]">
              {s.deployments.map((x) => (
                <li key={x.id} className="flex min-w-0 items-center justify-between gap-2">
                  <span className="truncate font-mono">{x.version || x.id}</span>
                  <span className="shrink-0 text-[#8c8f98]">
                    {x.failed ? <span className="text-[#d1aaad]">failed · </span> : null}
                    {relativeTime(x.finished_at / 1000)}
                  </span>
                </li>
              ))}
            </ul>
          )}
        </Panel>
      </div>
      <Panel
        title="Its definition"
        actions={
          s.source !== "repository" ? (
            <div className="flex gap-2">
              <Button variant="secondary" onClick={() => setEditing(true)}>
                {d ? "Edit" : "Define it"}
              </Button>
              {s.source === "api" && (
                <Button variant="ghost-danger" onClick={() => setRemoving(true)}>
                  Remove
                </Button>
              )}
            </div>
          ) : undefined
        }
      >
        {d ? (
          <pre className="max-h-[24rem] overflow-auto font-mono text-[12px] leading-relaxed text-[#cdd0d6]">{JSON.stringify(d, null, 2)}</pre>
        ) : (
          <p className="empty-line">None: it is listed as it is seen. Define it to name its team, contacts and links.</p>
        )}
      </Panel>
      {editing && (
        <DefineDialog
          title={d ? `Edit ${name}` : `Define ${name}`}
          initial={d ? JSON.stringify(d, null, 2) : `schema-version: v2.2\ndd-service: ${name}\nteam: \n`}
          onClose={() => {
            setEditing(false);
            invalidate(`observe/catalog/${name}`);
          }}
        />
      )}
      <ConfirmDialog
        open={removing}
        onOpenChange={setRemoving}
        title={`Remove ${name}'s definition?`}
        description="It stays listed while it is seen by its traces or deployments."
        confirmLabel="Remove"
        danger
        onConfirm={async () => {
          try {
            await observeApi.undefineService(name);
            invalidate("observe/catalog");
            navigate("/observe/catalog");
          } catch (e) {
            toast({ title: "Couldn't remove it", description: errorMessage(e), tone: "failed" });
          }
        }}
      />
    </>
  );
}

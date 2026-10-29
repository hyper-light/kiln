/**
 * A host (docs/observe.md §10.9): whether it is reporting and muted, its platform from the best of
 * what says it, its CPU, load, memory, disks and network over a range, its tags by source and its
 * apps, its containers, what Datadog's Agent says of it; muted (a downtime of every monitor's
 * groups of it, as Datadog's mute makes) and unmuted.
 */
import * as React from "react";
import { observeApi, type HostDetail } from "@/api/observe";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent } from "@/components/ui/dialog";
import { Hold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, Facts, Panel, SelectField } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { useRes, type Res } from "@/lib/data";
import { formatBytes, relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { Link, useParams } from "@/lib/router";
import { coresText, imageOf, nameOf } from "./containers";
import { Percent, Status } from "./hosts";
import { QueryChart, RangePicker, compact, useRange, type Range } from "./parts";

// The charts' series, named.
const CPU = ["User", "System", "I/O wait"];
const LOAD = ["1 minute", "5 minutes", "15 minutes"];
const MEMORY = ["Used", "Total"];

const hostRes = (name: string): Res<HostDetail> => ({ key: `observe/hosts/${name}`, fetch: (s) => observeApi.host(name, s), opts: { pollMs: 30_000 } });

/** A host's name as a query's tag value: quoted where it is more than a plain word. */
function tagValue(v: string) {
  return /^[\w.\-/:]+$/.test(v) ? v : `"${v.replace(/["\\]/g, "\\$&")}"`;
}

export default function HostPage() {
  const { name = "" } = useParams();
  const [range, setRange] = useRange();
  return (
    <div className="container-console page">
      <title>{`${name} · Hosts · Observe · kiln`}</title>
      <Loadable key={name} label="the host" fallback={<Hold height={480} />}>
        <HostView name={name} range={range} onRange={setRange} />
      </Loadable>
    </div>
  );
}

function HostView({ name, range, onRange }: { name: string; range: Range; onRange: (r: Range) => void }) {
  const h = useRes(hostRes(name)).data;
  const [muting, setMuting] = React.useState(false);
  const p = h.kiln.platform;
  const f = h.kiln.figures;
  const last = h.last_reported_time !== null ? `its last sample ${relativeTime(h.last_reported_time)}` : "no sample yet";
  const mute = h.kiln.mute;
  const unmute = async () => {
    try {
      await observeApi.unmuteHost(name);
      invalidate(`observe/hosts/${name}`);
      invalidate("observe/hosts");
    } catch (e) {
      toast({ title: "Couldn't unmute it", description: errorMessage(e), tone: "failed" });
    }
  };
  const q = `host:${tagValue(name)}`;
  return (
    <>
      <header className="page-intro">
        <div className="min-w-0">
          <Crumbs index={8} items={[{ label: "Observe", to: "/observe" }, { label: "Hosts", to: "/observe/hosts" }, { label: name }]} />
          <h1 className="truncate font-mono">{name}</h1>
          <p className="lead max-w-3xl">
            <Status h={h} /> <span className="text-[#8c8f98]">— {last}</span>
            {mute && (
              <span className="block text-[13px] text-[#8c8f98]">
                Muted {mute.end ? `until ${new Date(mute.end).toLocaleString()}` : "until unmuted"}
                {mute.message ? `: ${mute.message}` : ""}
              </span>
            )}
          </p>
        </div>
        <div className="flex flex-wrap gap-2">
          <Button variant="secondary" asChild>
            <Link to={`/observe/logs?q=${encodeURIComponent(q)}`}>Its logs</Link>
          </Button>
          {mute ? (
            <Button variant="secondary" onClick={() => void unmute()}>
              Unmute
            </Button>
          ) : (
            <Button variant="secondary" onClick={() => setMuting(true)}>
              Mute…
            </Button>
          )}
        </div>
      </header>
      <Facts
        className="mb-8"
        items={[
          { label: "CPU", value: <Percent v={f.cpu} />, tone: (f.cpu ?? 0) >= 90 ? "fail" : undefined },
          { label: "I/O wait", value: <Percent v={f.iowait} /> },
          { label: "Load (15 min)", value: f.load !== null ? `${compact(f.load)}${f.load_norm !== null ? ` · ${compact(f.load_norm)} a core` : ""}` : "—" },
          { label: "Memory", value: <Percent v={f.memory} />, tone: (f.memory ?? 0) >= 90 ? "fail" : undefined },
          { label: "Fullest disk", value: <Percent v={f.disk} />, tone: (f.disk ?? 0) >= 90 ? "fail" : undefined },
          { label: "OS", value: [p.os, p.arch].filter(Boolean).join(" · ") || "—" },
          { label: "CPUs", value: p.cores !== null ? `${p.cores} cores${p.threads ? `, ${p.threads} threads` : ""}` : "—" },
          { label: "Memory installed", value: p.memory_bytes !== null ? formatBytes(p.memory_bytes) : "—" },
        ]}
      />
      <div className="mb-5">
        <RangePicker value={range} onChange={onRange} />
      </div>
      <div className="mb-8 grid grid-cols-1 gap-4 lg:grid-cols-2">
        <Panel title="CPU" description="Of all its cores: user, system and waiting on I/O (%).">
          <Loadable label="its CPU" fallback={<Hold height={180} />}>
            <QueryChart queries={[`avg:system.cpu.user{${q}}`, `avg:system.cpu.system{${q}}`, `avg:system.cpu.iowait{${q}}`]} names={CPU} range={range} unit="%" />
          </Loadable>
        </Panel>
        <Panel title="Load" description="Its load over 1, 5 and 15 minutes.">
          <Loadable label="its load" fallback={<Hold height={180} />}>
            <QueryChart queries={[`avg:system.load.1{${q}}`, `avg:system.load.5{${q}}`, `avg:system.load.15{${q}}`]} names={LOAD} range={range} />
          </Loadable>
        </Panel>
        <Panel title="Memory" description="In use, of all it has.">
          <Loadable label="its memory" fallback={<Hold height={180} />}>
            <QueryChart queries={[`avg:system.mem.used{${q}}`, `avg:system.mem.total{${q}}`]} names={MEMORY} range={range} unit="MiB" />
          </Loadable>
        </Panel>
        <Panel title="Disks" description="Each disk's share in use.">
          <Loadable label="its disks" fallback={<Hold height={180} />}>
            <QueryChart queries={[`max:system.disk.in_use{${q}} by {device}`]} range={range} unit="fraction" />
          </Loadable>
        </Panel>
        <Panel title="Network" description="Bytes in and out, a second.">
          <Loadable label="its network" fallback={<Hold height={180} />}>
            <QueryChart queries={[`sum:system.net.bytes_rcvd{${q}}`, `sum:system.net.bytes_sent{${q}}`]} range={range} unit="B/s" />
          </Loadable>
        </Panel>
        <Panel title="Platform">
          <dl className="space-y-1.5 text-[12.5px]">
            {[
              ["CPU", p.cpu_model],
              ["Kernel", p.kernel],
              ["Cloud", [p.cloud, p.region, p.instance_type].filter(Boolean).join(" · ") || null],
              ["Datadog Agent", p.agent_version],
              ["kiln", p.kiln_version ? `${p.kiln_version}${h.kiln.runner ? ` (runner ${h.kiln.runner.name}, ${h.kiln.runner.connected ? "connected" : "not connected"})` : ""}` : null],
              ["Reported by", h.sources.join(", ") || "its series alone"],
              ["Series", String(h.kiln.series)],
              ["Also called", h.aliases.join(", ") || null],
            ]
              .filter(([, v]) => v)
              .map(([k, v]) => (
                <div key={k} className="flex min-w-0 gap-3">
                  <dt className="w-28 shrink-0 text-[#8c8f98]">{k}</dt>
                  <dd className="min-w-0 [overflow-wrap:anywhere]">{v}</dd>
                </div>
              ))}
          </dl>
        </Panel>
      </div>
      <div className="mb-8 grid grid-cols-1 gap-4 lg:grid-cols-2">
        <Panel title="Tags" description="By where they come from.">
          {Object.keys(h.tags_by_source).length === 0 ? (
            <p className="empty-line">None.</p>
          ) : (
            <dl className="space-y-3 text-[12.5px]">
              {Object.entries(h.tags_by_source).map(([source, tags]) => (
                <div key={source}>
                  <dt className="mb-1 text-[#8c8f98]">{source}</dt>
                  <dd className="flex flex-wrap gap-1.5">
                    {tags.map((t) => (
                      <span key={t} className="chip font-mono">
                        {t}
                      </span>
                    ))}
                  </dd>
                </div>
              ))}
            </dl>
          )}
        </Panel>
        <Panel title="Apps" description="What its metrics are of: each its namespace.">
          {h.apps.length === 0 ? (
            <p className="empty-line">None.</p>
          ) : (
            <div className="flex flex-wrap gap-1.5">
              {h.apps.map((a) => (
                <span key={a} className="chip font-mono">
                  {a}
                </span>
              ))}
            </div>
          )}
        </Panel>
      </div>
      <Panel title="Containers" description="Those its series say run on it, and those stopped within the hour: each one's CPU (cores) and memory in use (its working set)." className="mb-8">
        {h.containers.length === 0 ? (
          <p className="empty-line">None reported.</p>
        ) : (
          <div className="table-wrap">
            <table className="data-table">
              <thead>
                <tr>
                  <th>Container</th>
                  <th className="hidden md:table-cell">Pod</th>
                  <th className="text-right">CPU</th>
                  <th className="hidden text-right sm:table-cell">Memory</th>
                </tr>
              </thead>
              <tbody>
                {h.containers.map((c) => {
                  const f = c.figures;
                  const memory = f.working_set_bytes ?? f.memory_bytes;
                  return (
                    <tr key={c.id}>
                      <td className="truncate-cell">
                        <Link to={`/observe/containers/${c.id}`} className="font-mono text-[12.5px] text-[#e4e5e7] hover:text-white">
                          {nameOf(c)}
                        </Link>
                        <p className="truncate text-[11px] text-[#8c8f98]">
                          {c.state === "stopped" && <span className="text-[#d1aaad]">stopped · </span>}
                          {[imageOf(c) || null, relativeTime(c.last_reported / 1000)].filter(Boolean).join(" · ")}
                        </p>
                      </td>
                      <td className="hidden text-[12px] md:table-cell">{c.kube.pod ? `${c.kube.namespace ? `${c.kube.namespace}/` : ""}${c.kube.pod}` : <span className="text-[#8c8f98]">—</span>}</td>
                      <td className="text-right font-mono text-[12px]">{f.cpu_nanocores !== null ? coresText(f.cpu_nanocores / 1e9) : <span className="text-[#8c8f98]">—</span>}</td>
                      <td className="hidden text-right font-mono text-[12px] sm:table-cell">{memory !== null ? formatBytes(memory) : <span className="text-[#8c8f98]">—</span>}</td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </Panel>
      {h.meta && <AgentPanel h={h} />}
      {muting && <MuteDialog name={name} onClose={() => setMuting(false)} />}
    </>
  );
}

/** What Datadog's Agent says: its version and how it was installed, its checks and theirs. */
function AgentPanel({ h }: { h: HostDetail }) {
  const meta = h.meta ?? {};
  const checks = (Array.isArray(meta.agent_checks) ? meta.agent_checks : []) as unknown[][];
  const install = meta.install_method as { tool?: string; tool_version?: string } | undefined;
  return (
    <Panel
      title="Datadog Agent"
      description={[
        meta.agent_version ? `Version ${String(meta.agent_version)}` : null,
        install?.tool ? `installed by ${install.tool}` : null,
        h.kiln.agent_updated ? `said ${relativeTime(h.kiln.agent_updated / 1000)}` : null,
      ]
        .filter(Boolean)
        .join(", ")}
      className="mb-8"
    >
      {checks.length === 0 ? (
        <p className="empty-line">No checks said.</p>
      ) : (
        <ul className="grid grid-cols-1 gap-x-6 gap-y-1.5 text-[12.5px] sm:grid-cols-2 lg:grid-cols-3">
          {checks.map((c, i) => {
            const status = String(c[3] ?? "");
            const ok = status === "OK";
            return (
              <li key={`${String(c[2] ?? i)}`} className="flex min-w-0 items-center gap-2">
                <i className="dot" data-status={ok ? "succeeded" : "failed"} aria-hidden="true" />
                <span className="truncate font-mono">{String(c[0] ?? "")}</span>
                <span className="sr-only">{ok ? "OK" : status}</span>
                {!ok && <span className="truncate text-[#d1aaad]">{String(c[4] || status)}</span>}
              </li>
            );
          })}
        </ul>
      )}
    </Panel>
  );
}

const MUTES = [
  { value: "3600", label: "For an hour" },
  { value: "14400", label: "For four hours" },
  { value: "86400", label: "For a day" },
  { value: "604800", label: "For a week" },
  { value: "", label: "Until unmuted" },
];

/** A host muted: a downtime of every monitor's groups of it, for a while or until unmuted. */
function MuteDialog({ name, onClose }: { name: string; onClose: () => void }) {
  const [forS, setFor] = React.useState("3600");
  const [message, setMessage] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const save = async () => {
    setBusy(true);
    try {
      await observeApi.muteHost(name, { end: forS ? Math.floor(Date.now() / 1000) + Number(forS) : undefined, message: message.trim() || undefined });
      invalidate(`observe/hosts/${name}`);
      invalidate("observe/hosts");
      onClose();
    } catch (e) {
      toast({ title: "Couldn't mute it", description: errorMessage(e), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent title={`Mute ${name}`} description="Every monitor's groups of this host say nothing while it is muted: a downtime, as Datadog's mute makes.">
        <div className="space-y-4">
          <SelectField label="How long" value={forS} onChange={setFor} options={MUTES} />
          <div>
            <label htmlFor="mute-message" className="field-label">
              Why (optional)
            </label>
            <textarea id="mute-message" rows={3} value={message} onChange={(e) => setMessage(e.target.value)} placeholder="Patching the kernel" />
          </div>
          <div className="flex justify-end gap-2">
            <Button variant="ghost" onClick={onClose}>
              Cancel
            </Button>
            <Button disabled={busy} onClick={() => void save()}>
              Mute
            </Button>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}

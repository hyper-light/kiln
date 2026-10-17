import { Link } from "react-router";
import { Clock, GitCommitHorizontal, GitPullRequest, Hand, Tag } from "lucide-react";
import type { Agent, Child, HostSample, Run } from "@/api/types";
import { StatusDot, Chips } from "@/components/ui/status";
import { Meter, Sparkline } from "@/components/ui/misc";
import { formatBytes, formatDuration, firstLine, relativeTime, shortSha, spanSeconds, formatPct } from "@/lib/format";
import { useNow } from "@/lib/hooks";
import { cn } from "@/lib/utils";

export { upsert } from "@/lib/data";

export function childStatus(c: Child): string {
  switch (c.state) {
    case "running":
      return "succeeded";
    case "starting":
      return "queued";
    case "restarting":
      return "timed_out";
    case "stopped":
      return c.last_error ? "failed" : "cancelled";
    case "finished":
      return "skipped";
  }
}

/** Live-ticking duration for a run/job/step. */
export function LiveDuration({ start, end, className }: { start: number | null; end: number | null; className?: string }) {
  const now = useNow();
  const live = start !== null && end === null;
  return (
    <span className={cn("tabular-nums", live && "text-[#d8d0f2]", className)}>{formatDuration(spanSeconds(start, end, now))}</span>
  );
}

export function RelTime({ ts, className }: { ts: number | null | undefined; className?: string }) {
  const now = useNow();
  return (
    <time className={className} dateTime={ts ? new Date(ts * 1000).toISOString() : undefined} title={ts ? new Date(ts * 1000).toLocaleString() : undefined}>
      {relativeTime(ts, now)}
    </time>
  );
}

export function RunActivityRow({ run }: { run: Run }) {
  return (
    <Link
      to={`/runs/${run.id}`}
      className="nudge grid grid-cols-[auto_minmax(0,1fr)_auto] items-center gap-x-3 gap-y-0.5 px-1 py-2.5 transition-colors hover:bg-[#101114]"
    >
      <StatusDot status={run.status} className="row-span-2" />
      <span className="min-w-0 truncate text-[13px] text-[#e4e5e7]">
        {run.repo_name.split("/").pop()} <span className="text-[#8c8f98]">#{run.number}</span>
        <span className="ml-2 text-[12px] text-[#9a9da6]">{firstLine(run.message)}</span>
      </span>
      <span className="text-right font-mono text-[10px] text-[#8c8f98]">
        <LiveDuration start={run.started_at} end={run.finished_at} />
      </span>
      <span className="min-w-0 truncate font-mono text-[10px] text-[#7b7e88]">
        {run.branch} · {shortSha(run.sha)} · {run.pipeline}
      </span>
      <span className="text-right font-mono text-[10px] text-[#7b7e88]">
        <RelTime ts={run.created_at} />
      </span>
    </Link>
  );
}

export function memPct(s: HostSample | null | undefined) {
  return s && s.mem_total > 0 ? (s.mem_used / s.mem_total) * 100 : 0;
}
export function diskPct(s: HostSample | null | undefined) {
  return s && s.disk_total > 0 ? (s.disk_used / s.disk_total) * 100 : 0;
}

export function AgentMiniCard({ agent, history, onOpen }: { agent: Agent; history: HostSample[]; onOpen?: () => void }) {
  const s = agent.sample;
  const cpu = history.map((h) => h.cpu_pct);
  const mem = history.map((h) => memPct(h));
  const status = !agent.connected ? "cancelled" : agent.draining ? "timed_out" : agent.busy > 0 ? "running" : "succeeded";
  return (
    <article className="card flex flex-col" data-interactive="true" data-live={agent.busy > 0 ? "true" : undefined}>
      <button onClick={onOpen} className="flex flex-1 flex-col p-4 text-left outline-offset-[-3px]">
        <div className="flex items-center gap-2.5">
          <StatusDot status={status} title={!agent.connected ? "disconnected" : agent.draining ? "draining" : agent.busy ? "busy" : "idle"} />
          <h3 className="min-w-0 flex-1 truncate text-[15px] font-[480] tracking-[-0.3px]">{agent.name}</h3>
          <span className="font-mono text-[10px] text-[#8c8f98] tabular-nums">
            {agent.busy}/{agent.slots}
          </span>
        </div>
        <div className="mt-1 truncate font-mono text-[10px] text-[#7b7e88]">
          {agent.ephemeral ? "pod" : agent.hostname} · {agent.os}/{agent.arch}
        </div>
        <div className="mt-3 grid grid-cols-2 gap-3">
          <div>
            <div className="flex justify-between font-mono text-[9px] tracking-[0.08em] text-[#7f838d] uppercase">
              <span>CPU</span>
              <span className="text-[#c8cad0]">{s ? formatPct(s.cpu_pct) : "—"}</span>
            </div>
            <Sparkline values={cpu} max={100} width={140} height={26} className="mt-1 w-full" stroke="#b8a6d5" />
          </div>
          <div>
            <div className="flex justify-between font-mono text-[9px] tracking-[0.08em] text-[#7f838d] uppercase">
              <span>MEM</span>
              <span className="text-[#c8cad0]">{s ? formatPct(memPct(s)) : "—"}</span>
            </div>
            <Sparkline values={mem} max={100} width={140} height={26} className="mt-1 w-full" stroke="#a6c4ed" />
          </div>
        </div>
        <div className="mt-3">
          <Meter value={agent.busy} max={Math.max(1, agent.slots)} label="Slots busy" />
        </div>
      </button>
      <div className="card-footer">
        <Chips labels={agent.labels} max={3} />
        <span className="font-mono text-[9px] text-[#7b7e88]">{s ? formatBytes(s.mem_total, 0) : agent.connected ? "" : <RelTime ts={agent.last_seen} />}</span>
      </div>
    </article>
  );
}

export function EventIcon({ event }: { event: string }) {
  const p = { size: 12, "aria-hidden": true } as const;
  switch (event) {
    case "pull_request":
      return <GitPullRequest {...p} />;
    case "tag":
      return <Tag {...p} />;
    case "manual":
      return <Hand {...p} />;
    case "schedule":
      return <Clock {...p} />;
    default:
      return <GitCommitHorizontal {...p} />;
  }
}


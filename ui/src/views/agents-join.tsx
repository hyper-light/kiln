import * as React from "react";
import { Check, Radar, ShieldQuestion, X } from "lucide-react";
import { api } from "@/api/client";
import { useServerEvents } from "@/api/events";
import type { DiscoveredInstance, DiscoveryInfo, JoinRequest } from "@/api/types";
import { Button } from "@/components/ui/button";
import { CopyButton, ErrorState, SectionHeading } from "@/components/ui/misc";
import { Chips } from "@/components/ui/status";
import { toast } from "@/components/ui/toast";
import { errorMessage, useNow, useResource } from "@/lib/hooks";
import { RelTime } from "./shared";

/** `123456` → `123 456`. */
function formatCode(code: string) {
  return code.length === 6 ? `${code.slice(0, 3)} ${code.slice(3)}` : code;
}

/**
 * Machines asking to join (kiln join) and kiln instances heard on the local network. Refreshed by
 * the `join_requests` / `discovery` events, with slow polling as a fallback.
 */
export function useJoinData() {
  const requests = useResource((s) => api.joinRequests(s), [], { pollMs: 15_000 });
  const discovery = useResource((s) => api.discovery(s), [], { pollMs: 30_000 });
  const { reload: reloadRequests } = requests;
  const { reload: reloadDiscovery } = discovery;
  useServerEvents((ev) => {
    if (ev.type === "join_requests") reloadRequests();
    if (ev.type === "discovery") reloadDiscovery();
    // A machine that just joined connects as an agent: it leaves "On this network".
    if (ev.type === "agent") reloadDiscovery();
  });
  return { requests, discovery };
}

/* ------------------------------------------------------------------ waiting to join */

export function WaitingToJoin({
  requests,
  onDecided,
}: {
  requests: JoinRequest[];
  onDecided: () => void;
}) {
  const pending = requests.filter((r) => r.status === "pending");
  if (pending.length === 0) return null;
  return (
    <section className="mb-10" aria-labelledby="waiting-to-join">
      <SectionHeading
        label="Join requests"
        title={<span id="waiting-to-join">Waiting to join</span>}
        aside={<span className="font-mono text-[10px] text-[#8c8f98]">{pending.length} waiting</span>}
      />
      <p className="mb-4 max-w-[640px] text-[12.5px] leading-relaxed text-[#9a9da6]">
        Approve a machine only if the verification code below matches the one <code className="font-mono text-[#cdc4e0]">kiln join</code> shows on
        it. Approval gives it a certificate from this server; it then connects as a runner.
      </p>
      <div className="grid grid-cols-1 gap-4 md:grid-cols-2 2xl:grid-cols-3">
        {pending.map((r) => (
          <JoinRequestCard key={r.id} request={r} onDecided={onDecided} />
        ))}
      </div>
    </section>
  );
}

function JoinRequestCard({ request: r, onDecided }: { request: JoinRequest; onDecided: () => void }) {
  const now = useNow();
  const [busy, setBusy] = React.useState<"approve" | "deny" | null>(null);
  const minutes = Math.max(0, Math.ceil((r.expires_at * 1000 - now) / 60_000));

  const decide = async (what: "approve" | "deny") => {
    setBusy(what);
    try {
      if (what === "approve") {
        const res = await api.approveJoin(r.id);
        toast.success(`${res.name} approved`, "It connects as a runner in a moment.");
      } else {
        await api.denyJoin(r.id);
        toast({ title: `${r.name} denied`, tone: "info" });
      }
      onDecided();
    } catch (e) {
      toast.error(what === "approve" ? "Approval failed" : "Deny failed", errorMessage(e));
    } finally {
      setBusy(null);
    }
  };

  return (
    <article className="card flex flex-col" data-live="true" aria-label={`Join request from ${r.name}`}>
      <div className="flex flex-1 flex-col gap-3 p-4">
        <div className="flex items-start gap-2.5">
          <ShieldQuestion size={16} className="mt-0.5 shrink-0 text-[#cbb69a]" aria-hidden="true" />
          <div className="min-w-0 flex-1">
            <h3 className="truncate text-[15px] font-[480] tracking-[-0.3px]">{r.name}</h3>
            <div className="truncate font-mono text-[10px] text-[#7b7e88]">
              {r.hostname || "unknown host"} · {r.os}/{r.arch}
              {r.version ? ` · kiln ${r.version}` : ""}
            </div>
          </div>
        </div>
        <div>
          <div className="label mb-1">Verification code</div>
          {r.code ? (
            <div className="font-mono text-[26px] leading-none tracking-[0.16em] text-[#e4e5e7] tabular-nums" aria-label={`Verification code ${r.code.split("").join(" ")}`}>
              {formatCode(r.code)}
            </div>
          ) : (
            <div className="text-[12.5px] text-[#9a9da6]">Waiting for the machine to confirm…</div>
          )}
        </div>
        <dl className="grid grid-cols-[auto_minmax(0,1fr)] gap-x-3 gap-y-1 font-mono text-[10.5px]">
          <dt className="text-[#7b7e88]">From</dt>
          <dd className="truncate text-[#c8cad0]">{r.address}</dd>
          <dt className="text-[#7b7e88]">Asked</dt>
          <dd className="text-[#c8cad0]">
            <RelTime ts={r.created_at} /> · expires in {minutes} min
          </dd>
        </dl>
        <Chips labels={r.labels} max={6} />
      </div>
      <div className="card-footer flex-wrap py-2">
        <Button size="sm" className="flex-1 sm:flex-none" disabled={!r.code || busy !== null} onClick={() => decide("approve")}>
          <Check /> {busy === "approve" ? "Approving…" : "Approve"}
        </Button>
        <Button size="sm" variant="danger" className="flex-1 sm:flex-none" disabled={busy !== null} onClick={() => decide("deny")}>
          <X /> {busy === "deny" ? "Denying…" : "Deny"}
        </Button>
      </div>
    </article>
  );
}

/* ------------------------------------------------------------------ on this network */

export function OnThisNetwork({ discovery, error, onRetry }: { discovery: DiscoveryInfo | undefined; error: Error | null; onRetry: () => void }) {
  const nearby = (discovery?.instances ?? []).filter((i) => !i.joined);
  return (
    <section className="mt-14" aria-labelledby="on-this-network">
      <SectionHeading label="Discovery" title={<span id="on-this-network">On this network</span>} />
      {error && !discovery ? (
        <ErrorState error={error} onRetry={onRetry} compact />
      ) : !discovery ? null : !discovery.enabled || !discovery.browse ? (
        <p className="text-[12.5px] leading-relaxed text-[#9a9da6]">
          Discovery is turned off in this server&apos;s config (<code className="font-mono text-[#cdc4e0]">[discovery]</code>). Other machines can
          still join with <code className="font-mono text-[#cdc4e0]">kiln join --server &lt;this server&apos;s URL&gt;</code>.
        </p>
      ) : nearby.length === 0 ? (
        <div className="flex items-start gap-3 rounded-md border border-[#222428] px-4 py-3 text-[12.5px] leading-relaxed text-[#9a9da6]">
          <Radar size={16} className="mt-0.5 shrink-0 text-[#8c8f98]" aria-hidden="true" />
          <p className="min-w-0">
            No other kiln machines heard on this network. Run <code className="font-mono text-[#cdc4e0]">kiln join</code> on another computer to add it
            here. Discovery needs UDP 5353 (multicast DNS) open between the machines, e.g.{" "}
            <code className="font-mono break-all text-[#cdc4e0]">sudo ufw allow from 192.168.1.0/24 to any port 5353 proto udp</code>.
          </p>
        </div>
      ) : (
        <>
          <p className="mb-4 max-w-[640px] text-[12.5px] leading-relaxed text-[#9a9da6]">
            kiln machines heard on this network that are not part of this fleet yet. Run the command on one to ask to join; its request shows up
            above for approval.
          </p>
          <div className="grid grid-cols-1 gap-4 md:grid-cols-2 2xl:grid-cols-3">
            {nearby.map((i) => (
              <NearbyCard key={i.id} instance={i} />
            ))}
          </div>
        </>
      )}
    </section>
  );
}

function NearbyCard({ instance: i }: { instance: DiscoveredInstance }) {
  const address = i.addresses.find((a) => !a.includes(":")) ?? i.addresses[0];
  return (
    <article className="card flex flex-col">
      <div className="flex flex-1 flex-col gap-3 p-4">
        <div className="flex items-center gap-2.5">
          <Radar size={15} className="shrink-0 text-[#b8a6d5]" aria-hidden="true" />
          <h3 className="min-w-0 flex-1 truncate text-[15px] font-[480] tracking-[-0.3px]">{i.name || i.instance}</h3>
          <span className="chip" data-tone={i.role === "server" ? undefined : "baremetal"}>
            {i.role}
          </span>
        </div>
        <div className="truncate font-mono text-[10px] text-[#7b7e88]">
          {address ?? "no address"} · {i.os}/{i.arch} · kiln {i.version} · seen <RelTime ts={i.last_seen} />
        </div>
        <pre className="overflow-x-auto rounded-md border border-[#2b2e39] bg-[#101217] p-3 font-mono text-[11.5px] leading-relaxed whitespace-pre-wrap break-all text-[#cdc4e0]">
          <span className="text-[#6d7078] select-none">$ </span>
          {i.join_command}
        </pre>
      </div>
      <div className="card-footer">
        <span className="min-w-0 truncate font-mono text-[9px] text-[#7b7e88]">{i.url ?? (i.role === "agent" ? "runner · not enrolled here" : "")}</span>
        <CopyButton text={i.join_command} label="Copy command" />
      </div>
    </article>
  );
}

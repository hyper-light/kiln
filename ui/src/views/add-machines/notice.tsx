import { Link } from "react-router";
import { ArrowRight } from "lucide-react";
import type { DiscoveryInfo, JoinRequest } from "@/api/types";
import { cn } from "@/lib/utils";

/** On Runners: one quiet line when machines are asking to join or wait to be set up, leading to Add machines. */
export function JoinNotice({ requests, discovery }: { requests: JoinRequest[] | undefined; discovery: DiscoveryInfo | undefined }) {
  const asking = (requests ?? []).filter((r) => r.status === "pending").length;
  const fresh = (discovery?.instances ?? []).filter((i) => i.role === "agent" && !i.joined && !i.enrolled && !i.request).length;
  if (!asking && !fresh) return null;
  const words = [asking ? `${asking} asking to join` : "", fresh ? `${fresh} not set up yet on this network` : ""].filter(Boolean).join(" · ");
  return (
    <Link
      to="/runners/add"
      className="mb-6 flex min-h-11 min-w-0 items-center gap-3 rounded-[7px] border border-[#2a2c33] bg-[#0d0e11] px-3.5 py-2 text-[12.5px] text-[#c9cbd1] transition-colors hover:border-[#47464f] hover:text-[#ededee]"
    >
      <i className={cn("size-1.5 shrink-0 rounded-full", asking ? "bg-[#d8bb95]" : "bg-[#a6b2c6]")} aria-hidden="true" />
      <span className="min-w-0 flex-1 truncate">{words}</span>
      <span className="hidden shrink-0 font-mono text-[10px] tracking-[0.08em] text-[#8f929b] uppercase sm:inline">Add machines</span>
      <ArrowRight size={14} className="shrink-0 text-[#8f929b]" aria-hidden="true" />
    </Link>
  );
}

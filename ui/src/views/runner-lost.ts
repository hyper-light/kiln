import type { Agent, NetDrop } from "@/api/types";

/** Frames the runner's and the server's QUIC layers let go between them, the most first (docs/wire.md, "Loss below the channel"): the channel sends again what of them it carried. */
export function lostLine(agent: Pick<Agent, "dropped" | "dropped_toward">): string {
  const said = (drops: NetDrop[] | undefined, where: string) => {
    const total = (drops ?? []).reduce((n, d) => n + d.frames, 0);
    if (total === 0) return null;
    const most = [...(drops ?? [])].sort((a, b) => b.frames - a.frames).slice(0, 3);
    return `${total} ${where} (${most.map((d) => `${d.lane} ${d.reason} ${d.frames}`).join(", ")})`;
  };
  const parts = [said(agent.dropped, "here"), said(agent.dropped_toward, "at the server")].filter((p): p is string => p !== null);
  return parts.length ? parts.join(" · ") : "none";
}

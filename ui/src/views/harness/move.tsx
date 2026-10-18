/**
 * Move an agent to another machine: the runners that have its harness and take reach links, one
 * to pick, and what goes along. It moves once its turn is over; how it goes shows in its timeline.
 */
import * as React from "react";
import { api } from "@/api/client";
import { harnessApi, type AgentInfo, type KindInfo } from "@/api/harness";
import type { ReachRunner } from "@/api/types";
import { Dialog, DialogClose, DialogContent } from "@/components/ui/dialog";
import { Loadable } from "@/components/ui/loading";
import { toast } from "@/components/ui/toast";
import { errorMessage } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";
import { cn } from "@/lib/utils";

interface Places {
  byTarget: Record<string, KindInfo[]>;
  runners: ReachRunner[];
}

const askPlaces = (signal: AbortSignal): Promise<Places> =>
  Promise.all([
    harnessApi.kindsAll().catch((): Record<string, KindInfo[]> => ({})),
    api.reachRunners(signal).then(
      (r) => r.runners,
      (): ReachRunner[] => [],
    ),
  ]).then(([byTarget, runners]) => ({ byTarget, runners }));

type Agent = Pick<AgentInfo, "id" | "name" | "kind" | "kind_name" | "target" | "status">;

function Choices({ agent, picked, onPick }: { agent: Agent; picked: string; onPick: (t: string) => void }) {
  const { byTarget, runners } = useSuspenseResource("harness/move", askPlaces).data;
  const fit = runners.filter((r) => r.connected && r.reach && `agent:${r.name}` !== agent.target && (byTarget[`agent:${r.name}`] ?? []).some((k) => k.id === agent.kind && k.available));
  if (fit.length === 0) return <p className="mv-none">No other connected machine has {agent.kind_name} installed and takes agents from other runners.</p>;
  return (
    <div className="mv-list" role="radiogroup" aria-label="Machines">
      {fit.map((r) => {
        const t = `agent:${r.name}`;
        return (
          <button key={t} type="button" role="radio" aria-checked={picked === t} className={cn("mv-choice", picked === t && "mv-choice-on")} onClick={() => onPick(t)}>
            <span className="mv-name">{r.name}</span>
            <span className="mv-meta">
              {r.os}/{r.arch}
              {r.hostname && r.hostname !== r.name ? ` · ${r.hostname}` : ""}
            </span>
          </button>
        );
      })}
    </div>
  );
}

export function MoveDialog({ agent, onClose }: { agent: Agent; onClose: () => void }) {
  const [picked, setPicked] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const running = agent.status === "running" || agent.status === "waiting";
  const go = async () => {
    if (!picked) return;
    setBusy(true);
    try {
      await harnessApi.move(agent.id, picked);
      toast.success(`Moving ${agent.name}`, running ? "It goes on there once this turn is over." : "It goes on there in a moment.");
      onClose();
    } catch (e) {
      toast.error("Couldn't move it", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent
        title={`Move ${agent.name} to…`}
        description="Its conversation, timeline and queue go along. A git repository it works in goes too, kept in step between both machines; other folders stay where they are."
      >
        <Loadable label="Loading your machines" fallback={<p className="mv-none">Looking at your machines…</p>}>
          <Choices agent={agent} picked={picked} onPick={setPicked} />
        </Loadable>
        <div className="mt-5 flex flex-wrap justify-end gap-2">
          <DialogClose asChild>
            <button className="inline-flex min-h-11 items-center rounded-full px-5 text-sm text-[#b4b5ba] hover:bg-white/[0.06] hover:text-white">Cancel</button>
          </DialogClose>
          <button
            disabled={busy || !picked}
            onClick={() => void go()}
            className="inline-flex min-h-11 items-center rounded-full border border-[#e4e5e7] bg-[#e4e5e7] px-5 text-sm font-medium text-[#111214] transition-colors hover:bg-white disabled:opacity-40"
          >
            {busy ? "Moving…" : running ? "Move after this turn" : "Move"}
          </button>
        </div>
      </DialogContent>
    </Dialog>
  );
}

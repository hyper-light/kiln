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

/** Names in words: "a", "a and b", "a, b and c". */
const inWords = (ns: string[]) => (ns.length < 2 ? ns.join("") : `${ns.slice(0, -1).join(", ")} and ${ns[ns.length - 1]}`);

function Choices({ agent, picked, onPick }: { agent: Agent; picked: string; onPick: (t: string) => void }) {
  const { byTarget, runners } = useSuspenseResource("harness/move", askPlaces).data;
  // A runner without a reach identity runs a kiln from before agents could move.
  const here = runners.find((r) => `agent:${r.name}` === agent.target);
  if (here && !here.fingerprint) return <p className="mv-none">{here.name} runs an older kiln that can't hand agents over: upgrade it in Settings → Updates.</p>;
  const others = runners.filter((r) => r.connected && `agent:${r.name}` !== agent.target && (byTarget[`agent:${r.name}`] ?? []).some((k) => k.id === agent.kind && k.available));
  const fit = others.filter((r) => r.reach);
  const older = others.filter((r) => !r.fingerprint).map((r) => r.name);
  const closed = others.filter((r) => r.fingerprint && !r.reach).map((r) => r.name);
  const why = [
    older.length > 0 && `${inWords(older)} ${older.length === 1 ? "runs an older kiln: upgrade it" : "run an older kiln: upgrade them"} in Settings → Updates to move agents there.`,
    closed.length > 0 && `${inWords(closed)} ${closed.length === 1 ? "is" : "are"} set not to take work from other runners.`,
  ]
    .filter(Boolean)
    .join(" ");
  if (fit.length === 0) return <p className="mv-none">{why || `No other connected machine has ${agent.kind_name} installed.`}</p>;
  return (
    <>
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
      {why && <p className="mv-why">{why}</p>}
    </>
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

/**
 * Move an agent to another machine: the runners that have its harness and take reach links, one
 * to pick, and what goes along. It moves once its turn is over, or now (the turn stops here and is
 * picked up there); how it goes shows in its timeline.
 */
import * as React from "react";
import { api } from "@/api/client";
import { harnessApi, type AgentInfo, type KindInfo } from "@/api/harness";
import type { ReachRunner } from "@/api/types";
import { Button } from "@/components/ui/button";
import { Dialog, DialogClose, DialogContent } from "@/components/ui/dialog";
import { Loadable } from "@/components/ui/loading";
import { toast } from "@/components/ui/toast";
import { errorMessage } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";
import { cn } from "@/lib/utils";

export interface Places {
  byTarget: Record<string, KindInfo[]>;
  runners: ReachRunner[];
}

export const askPlaces = (signal: AbortSignal): Promise<Places> =>
  Promise.all([
    harnessApi.kindsAll().catch((): Record<string, KindInfo[]> => ({})),
    api.reachRunners(signal).then(
      (r) => r.runners,
      (): ReachRunner[] => [],
    ),
  ]).then(([byTarget, runners]) => ({ byTarget, runners }));

type Agent = Pick<AgentInfo, "id" | "name" | "kind" | "kind_name" | "target" | "status">;

export const movable = (a: Pick<AgentInfo, "target" | "status" | "harness_session">) => a.target.startsWith("agent:") || (a.status === "exited" && !a.harness_session);

/** Names in words: "a", "a and b", "a, b and c". */
export const inWords = (ns: string[]) => (ns.length < 2 ? ns.join("") : `${ns.slice(0, -1).join(", ")} and ${ns[ns.length - 1]}`);

function Choices({ agent, picked, onPick }: { agent: Agent; picked: string; onPick: (t: string) => void }) {
  const { byTarget, runners } = useSuspenseResource("harness/move", askPlaces).data;
  // A runner without a reach identity runs a kiln from before agents could move.
  const here = runners.find((r) => `agent:${r.name}` === agent.target);
  if (here && !here.fingerprint) return <p className="mv-none">{here.name} runs an older kiln that can't hand agents over: upgrade it in System → Updates.</p>;
  const others = runners.filter((r) => r.connected && `agent:${r.name}` !== agent.target && (byTarget[`agent:${r.name}`] ?? []).some((k) => k.id === agent.kind && k.available));
  const fit = others.filter((r) => r.reach);
  const older = others.filter((r) => !r.fingerprint).map((r) => r.name);
  const closed = others.filter((r) => r.fingerprint && !r.reach).map((r) => r.name);
  const why = [
    older.length > 0 && `${inWords(older)} ${older.length === 1 ? "runs an older kiln: upgrade it" : "run an older kiln: upgrade them"} in System → Updates to move agents there.`,
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
  const [busy, setBusy] = React.useState<"" | "now" | "after">("");
  const running = agent.status === "running" || agent.status === "waiting";
  const go = async (now: boolean) => {
    if (!picked) return;
    setBusy(now ? "now" : "after");
    try {
      await harnessApi.move(agent.id, picked, now);
      toast.success(`Moving ${agent.name}`, !running ? "It goes on there in a moment." : now ? "This turn stops here and goes on there." : "It goes on there once this turn is over.");
      onClose();
    } catch (e) {
      toast.error("Couldn't move it", errorMessage(e));
    } finally {
      setBusy("");
    }
  };
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent
        title={`Move ${agent.name} to…`}
        description={
          agent.status === "disconnected"
            ? "Its machine is away, so it goes on from the kiln server's copy: its conversation, timeline and queue. Its files stay on that machine, unless its repository is kept in step where it goes."
            : agent.target === "local"
              ? "It never started on the kiln server, so it starts on the machine you pick instead, with what it was asked to do: in the same folder where that machine has it, else in its home folder."
              : "Its conversation, timeline and queue go along. A git repository it works in goes too, kept in step between both machines; other folders stay where they are."
        }
      >
        <Loadable label="Loading your machines" fallback={<p className="mv-none">Looking at your machines…</p>}>
          <Choices agent={agent} picked={picked} onPick={setPicked} />
        </Loadable>
        {/* On a phone, stacked: the move first, then the other way to move, then Cancel. */}
        <div className="mt-5 flex flex-col-reverse gap-2 sm:flex-row sm:flex-wrap sm:justify-end">
          <DialogClose asChild>
            <Button variant="ghost">Cancel</Button>
          </DialogClose>
          {running && (
            <Button variant="secondary" disabled={busy !== "" || !picked} onClick={() => void go(true)}>
              {busy === "now" ? "Moving…" : "Move now"}
            </Button>
          )}
          <Button disabled={busy !== "" || !picked} onClick={() => void go(false)}>
            {busy === "after" ? "Moving…" : running ? "Move after this turn" : "Move"}
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}

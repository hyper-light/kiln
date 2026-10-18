/**
 * Where an agent's commands run besides its own machine — any runner it needs, one it prefers, or
 * only its own — changed while it lives. Its harness takes the setting when it starts, so it starts
 * again once nothing is under way, the conversation going on (the timeline says so).
 */
import * as React from "react";
import { api } from "@/api/client";
import { harnessApi, type AgentInfo } from "@/api/harness";
import type { ReachRunner } from "@/api/types";
import { Button } from "@/components/ui/button";
import { Dialog, DialogClose, DialogContent } from "@/components/ui/dialog";
import { Loadable } from "@/components/ui/loading";
import { toast } from "@/components/ui/toast";
import { errorMessage } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";
import { cn } from "@/lib/utils";
import { reachChoices } from "./spawn";

type Agent = Pick<AgentInfo, "id" | "name" | "kind_name" | "target" | "status" | "reach">;

const askRunners = (signal: AbortSignal): Promise<ReachRunner[]> =>
  api.reachRunners(signal).then(
    (r) => r.runners,
    (): ReachRunner[] => [],
  );

function Choices({ agent, value, onPick }: { agent: Agent; value: string; onPick: (v: string) => void }) {
  const runners = useSuspenseResource("harness/reach", askRunners).data;
  // A runner without a reach identity runs a kiln from before commands could run elsewhere.
  const here = runners.find((r) => `agent:${r.name}` === agent.target);
  if (here && !here.fingerprint) return <p className="mv-none">{here.name} runs an older kiln, without commands on other runners: upgrade it in Settings → Updates.</p>;
  const others = runners.filter((r) => r.connected && r.reach);
  const choices = [{ value: "", label: "Any runner it needs" }, ...reachChoices(others, agent.target), { value: "off", label: "Only its own machine" }];
  // The one it has stays a choice, whether or not that runner is around now.
  const now = agent.reach ?? "";
  if (!choices.some((c) => c.value === now)) choices.splice(1, 0, { value: now, label: `Prefer ${now}` });
  return (
    <div className="mv-list" role="radiogroup" aria-label="Where its commands run">
      {choices.map((c) => (
        <button key={c.value || "any"} type="button" role="radio" aria-checked={value === c.value} className={cn("mv-choice", value === c.value && "mv-choice-on")} onClick={() => onPick(c.value)}>
          <span className="mv-name">{c.label}</span>
          {c.value === now && <span className="mv-meta">now</span>}
        </button>
      ))}
    </div>
  );
}

export function ReachDialog({ agent, onClose }: { agent: Agent; onClose: () => void }) {
  const now = agent.reach ?? "";
  const [picked, setPicked] = React.useState(now);
  const [busy, setBusy] = React.useState(false);
  const running = agent.status === "running" || agent.status === "waiting";
  const save = async () => {
    setBusy(true);
    try {
      await harnessApi.command(agent.id, { type: "control", id: "reach", value: picked });
      const when =
        agent.status === "exited" ? "It takes it when it next starts." : running ? `${agent.kind_name} starts again with it once this turn ends.` : `${agent.kind_name} starts again with it now.`;
      toast.success("Changed", `${when} The conversation goes on.`);
      onClose();
    } catch (e) {
      toast.error("Couldn't change it", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent
        title={`Where ${agent.name}'s commands run`}
        description="On its own machine, and on other runners when it needs one: a platform its machine isn't, or one with room. A change starts it again once its turn ends, the conversation going on."
      >
        <Loadable label="Loading your machines" fallback={<p className="mv-none">Looking at your machines…</p>}>
          <Choices agent={agent} value={picked} onPick={setPicked} />
        </Loadable>
        {/* On a phone, stacked: the change first, then Cancel. */}
        <div className="mt-5 flex flex-col-reverse gap-2 sm:flex-row sm:flex-wrap sm:justify-end">
          <DialogClose asChild>
            <Button variant="ghost">Cancel</Button>
          </DialogClose>
          <Button disabled={busy || picked === now} onClick={() => void save()}>
            {busy ? "Changing…" : "Change"}
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}

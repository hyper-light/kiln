/** What you can do to an agent, from its card or its page: open, interrupt, restart, rename, duplicate, move, where its commands run, end, remove, mute its notifications. */
import * as React from "react";
import { useNavigate } from "@/lib/router";
import { DropdownMenu } from "radix-ui";
import { Check, MoreHorizontal } from "lucide-react";
import { harnessApi, type AgentInfo, type Command } from "@/api/harness";
import { pushApi, pushStateRes } from "@/api/push";
import { ConfirmDialog } from "@/components/ui/dialog";
import { toast } from "@/components/ui/toast";
import { useRes } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { copyText } from "@/views/terminal/clipboard";
import { MoveDialog } from "./move";
import { ReachDialog } from "./reach";
import { SpawnDialog } from "./spawn";

type Act = "end" | "remove" | "rename" | "duplicate" | "move" | "reach" | null;

export function useAgentActions(
  a: Pick<AgentInfo, "id" | "name" | "status" | "kind" | "kind_name" | "target" | "cwd" | "harness_session" | "left_behind" | "reach" | "keep_off">,
  after?: { onGone?: () => void },
) {
  const navigate = useNavigate();
  const [act, setAct] = React.useState<Act>(null);
  // Kiln keeps it going when it stops part-way unless switched off here (shown at once, kept on the server).
  const [keepSet, setKeepSet] = React.useState<boolean | null>(null);
  const keeping = keepSet ?? !a.keep_off;
  // Renaming from the menu: the menu mustn't hand focus back to its button, the name field takes it.
  const renameNext = React.useRef(false);
  const live = a.status !== "exited";
  const command = async (c: Command, what: string) => {
    try {
      await harnessApi.command(a.id, c);
    } catch (e) {
      toast.error(`Couldn't ${what}`, errorMessage(e));
    }
  };
  const end = async () => {
    try {
      await harnessApi.stop(a.id);
      after?.onGone?.();
    } catch (e) {
      toast.error(live ? "Couldn't end the agent" : "Couldn't remove the agent", errorMessage(e));
    }
  };
  const items: { label: string; onSelect: () => void; danger?: boolean; hidden?: boolean }[] = [
    {
      label: "Rename…",
      onSelect: () => {
        renameNext.current = true;
        setAct("rename");
      },
    },
    { label: "Stop the current turn", onSelect: () => void command({ type: "interrupt" }, "interrupt it"), hidden: a.status !== "running" && a.status !== "waiting" },
    { label: "Restart", onSelect: () => void command({ type: "restart" }, "restart it"), hidden: live },
    { label: "Duplicate…", onSelect: () => setAct("duplicate") },
    // Agents the kiln server itself runs stay there.
    { label: "Move to…", onSelect: () => setAct("move"), hidden: !live || !a.target.startsWith("agent:") },
    { label: "Where commands run…", onSelect: () => setAct("reach"), hidden: !a.target.startsWith("agent:") },
    { label: "Copy session id", onSelect: () => void copyText(a.harness_session ?? a.id).then((ok) => ok && toast.success("Copied")) },
    { label: live ? "End agent…" : "Remove", onSelect: () => (live ? setAct("end") : void end()), danger: true },
  ];
  const menu = (trigger?: React.ReactNode, extra: { label: string; onSelect: () => void }[] = []) => (
    <DropdownMenu.Root>
      <DropdownMenu.Trigger asChild>
        {trigger ?? (
          <button type="button" className="hx-icon" aria-label={`Actions for ${a.name}`}>
            <MoreHorizontal size={16} />
          </button>
        )}
      </DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content
          className="popover w-56"
          sideOffset={6}
          align="end"
          collisionPadding={12}
          onCloseAutoFocus={(e) => {
            if (renameNext.current) {
              renameNext.current = false;
              e.preventDefault();
            }
          }}
        >
          {[...extra, ...items]
            .filter((i) => !("hidden" in i && i.hidden))
            .map((i) => (
              <DropdownMenu.Item key={i.label} className="menu-item" onSelect={i.onSelect} style={"danger" in i && i.danger ? { color: "var(--color-fail)" } : undefined}>
                <span className="flex-1">{i.label}</span>
              </DropdownMenu.Item>
            ))}
          <DropdownMenu.Separator className="my-1 h-px bg-white/10" />
          <React.Suspense fallback={null}>
            <MuteItem id={a.id} />
          </React.Suspense>
          <DropdownMenu.CheckboxItem
            className="menu-item"
            checked={keeping}
            onSelect={(e) => e.preventDefault()}
            onCheckedChange={(on) => {
              setKeepSet(on);
              void command({ type: "control", id: "keep_going", value: on }, on ? "keep it going" : "stop keeping it going");
            }}
          >
            <span className="flex-1">Keep it going when it stops part-way</span>
            <DropdownMenu.ItemIndicator>
              <Check size={14} aria-hidden="true" />
            </DropdownMenu.ItemIndicator>
          </DropdownMenu.CheckboxItem>
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  );
  const dialogs = (
    <>
      <ConfirmDialog
        open={act === "end"}
        onOpenChange={(o) => !o && setAct(null)}
        title={`End ${a.name}?`}
        description={`Its process stops on the machine and it leaves the board${a.left_behind ? `, along with the ${a.left_behind === 1 ? "shell or monitor" : `${a.left_behind} shells and monitors`} it left running` : ""}. Its changes stay on disk, and its conversation can be resumed from the harness later.`}
        confirmLabel="End agent"
        danger
        onConfirm={end}
      />
      {act === "move" && <MoveDialog agent={a} onClose={() => setAct(null)} />}
      {act === "reach" && <ReachDialog agent={a} onClose={() => setAct(null)} />}
      {act === "duplicate" && (
        <SpawnDialog
          open
          onOpenChange={(o) => !o && setAct(null)}
          defaults={{ target: a.target, cwd: a.cwd, kind: a.kind }}
          onStarted={(started) => {
            setAct(null);
            if (started.length === 1 && started[0]) navigate(`/agents/${started[0].id}`);
          }}
        />
      )}
    </>
  );
  return {
    menu,
    dialogs,
    end: () => (live ? setAct("end") : void end()),
    live,
    /** Its name is being edited where it shows (`NameField`). */
    renaming: act === "rename",
    startRenaming: () => setAct("rename"),
    stopRenaming: () => setAct(null),
    /** Names it in kiln (and in its harness, which names conversations too); "" gives it back its harness's name. */
    rename: (n: string) => command({ type: "control", id: "rename", value: n }, "rename it"),
  };
}

/** Mute an agent's notifications for you, on every device (and unmute): where push is on here. */
function MuteItem({ id }: { id: string }) {
  const push = useRes(pushStateRes);
  const muted = push.data.muted.includes(id);
  if (!push.data.available && !muted) return null;
  const toggle = () => {
    push.setData((s) => ({ ...s, muted: muted ? s.muted.filter((m) => m !== id) : [...s.muted, id] }));
    pushApi.mute(id, !muted).catch((e: unknown) => {
      toast.error(muted ? "Couldn't unmute it" : "Couldn't mute it", errorMessage(e));
      invalidate("push/state");
    });
  };
  return (
    <DropdownMenu.Item className="menu-item" onSelect={toggle}>
      <span className="flex-1">{muted ? "Unmute notifications" : "Mute notifications"}</span>
    </DropdownMenu.Item>
  );
}

export { NameField } from "@/components/ui/name-field";

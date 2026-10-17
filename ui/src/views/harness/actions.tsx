/** What you can do to an agent, from its card or its page: open, interrupt, restart, rename, duplicate, end, remove. */
import * as React from "react";
import { useNavigate } from "react-router";
import { DropdownMenu } from "radix-ui";
import { MoreHorizontal } from "lucide-react";
import { harnessApi, type AgentInfo, type Command } from "@/api/harness";
import { Button } from "@/components/ui/button";
import { ConfirmDialog, Dialog, DialogContent } from "@/components/ui/dialog";
import { toast } from "@/components/ui/toast";
import { errorMessage } from "@/lib/hooks";
import { copyText } from "@/views/terminal/clipboard";
import { SpawnDialog } from "./spawn";

type Act = "end" | "remove" | "rename" | "duplicate" | null;

export function useAgentActions(a: Pick<AgentInfo, "id" | "name" | "status" | "kind" | "target" | "cwd" | "harness_session">, after?: { onGone?: () => void }) {
  const navigate = useNavigate();
  const [act, setAct] = React.useState<Act>(null);
  const [name, setName] = React.useState(a.name);
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
    { label: "Stop the current turn", onSelect: () => void command({ type: "interrupt" }, "interrupt it"), hidden: a.status !== "running" && a.status !== "waiting" },
    { label: "Restart", onSelect: () => void command({ type: "restart" }, "restart it"), hidden: live },
    { label: "Rename…", onSelect: () => setAct("rename") },
    { label: "Duplicate…", onSelect: () => setAct("duplicate") },
    { label: "Copy session id", onSelect: () => void copyText(a.harness_session ?? a.id).then((ok) => ok && toast.success("Copied")) },
    { label: live ? "End agent…" : "Remove", onSelect: () => (live ? setAct("end") : void end()), danger: true },
  ];
  const menu = (trigger?: React.ReactNode) => (
    <DropdownMenu.Root>
      <DropdownMenu.Trigger asChild>
        {trigger ?? (
          <button type="button" className="hx-icon" aria-label={`Actions for ${a.name}`}>
            <MoreHorizontal size={16} />
          </button>
        )}
      </DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content className="popover w-56" sideOffset={6} align="end" collisionPadding={12}>
          {items
            .filter((i) => !i.hidden)
            .map((i) => (
              <DropdownMenu.Item key={i.label} className="menu-item" onSelect={i.onSelect} style={i.danger ? { color: "var(--color-fail)" } : undefined}>
                <span className="flex-1">{i.label}</span>
              </DropdownMenu.Item>
            ))}
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
        description="Its process stops on the machine and it leaves the board. Its changes stay on disk, and its conversation can be resumed from the harness later."
        confirmLabel="End agent"
        danger
        onConfirm={end}
      />
      <Dialog open={act === "rename"} onOpenChange={(o) => !o && setAct(null)}>
        {act === "rename" && (
          <DialogContent title="Rename agent" eyebrow="Agent">
            <form
              className="flex gap-2"
              onSubmit={(e) => {
                e.preventDefault();
                const n = name.trim();
                if (n) void command({ type: "control", id: "rename", value: n }, "rename it");
                setAct(null);
              }}
            >
              <input className="hx-input" autoFocus value={name} onChange={(e) => setName(e.target.value)} aria-label="Name" />
              <Button size="sm" type="submit">
                Rename
              </Button>
            </form>
          </DialogContent>
        )}
      </Dialog>
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
  return { menu, dialogs, end: () => (live ? setAct("end") : void end()), live };
}

/** The ⋯ menu on an agent's card. */
export function AgentMenu({ a }: { a: AgentInfo }) {
  const { menu, dialogs } = useAgentActions(a);
  return (
    <>
      {menu()}
      {dialogs}
    </>
  );
}

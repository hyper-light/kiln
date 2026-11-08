/**
 * What you can do to an agent, from its card or its page: open, interrupt, restart, rename, duplicate,
 * move, where its commands run, end, remove, mute its notifications; and from its row in Mission
 * control, where it's kept for you (`organize`: pin, park, archive; attention plan 01a199e1b177752e84dad5dc990e6a15).
 */
import * as React from "react";
import { DropdownMenu } from "radix-ui";
import { Check, MoreHorizontal } from "lucide-react";
import { harnessApi, sessionStore, type AgentInfo, type Command } from "@/api/harness";
import { pushApi, pushStateRes } from "@/api/push";
import { ConfirmDialog } from "@/components/ui/dialog";
import { toast } from "@/components/ui/toast";
import { sendingProps, useAction } from "@/lib/action";
import { harnessSessionsRes, useRes } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { change, invalidate } from "@/lib/resource";
import { copyText } from "@/views/terminal/clipboard";
import { useNavigate } from "@/lib/router";
import { agentLabel } from "./derive";
import { MoveDialog, movable } from "./move";
import { startFrom } from "./new-agent-draft";
import { useEndWithChildren } from "./family";

const ReachDialog = React.lazy(() => import("./reach").then((m) => ({ default: m.ReachDialog })));

/**
 * The lists of agents the pages read, every page's (lib/data.ts) and Mission control's
 * (use-agents.ts), as starting.ts has them: what's done to one here shows in both at once, over a
 * load already on its way too (`change`).
 */
const LISTS = [harnessSessionsRes.key, "harness/agents"];
const inLists = (f: (l: AgentInfo[]) => AgentInfo[]) => LISTS.forEach((key) => change<AgentInfo[]>(key, f));
/** Both read again, as the server has them: what was shown at once and didn't go goes back. */
const readLists = () => LISTS.forEach((key) => invalidate(key));

type Act = "end" | "remove" | "rename" | "move" | "reach" | null;

/** One of the menu's ways to keep a row (Mission control's): `opens` a line of its own under the row, which takes the focus. */
export interface OrganizeItem {
  label: string;
  onSelect: () => void;
  disabled?: boolean;
  opens?: boolean;
}

/** `after.onGone`: it's gone (ended or removed); `after.onFailed`: its Remove didn't go (the server kept it, why given), said where it's shown instead of in a toast. */
export function useAgentActions(
  a: Pick<AgentInfo, "id" | "name" | "status" | "kind" | "kind_name" | "target" | "cwd" | "harness_session" | "left_behind" | "reach" | "keep_off" | "mode" | "model">,
  after?: { onGone?: () => void; onFailed?: (why: string) => void },
) {
  const navigate = useNavigate();
  const [act, setAct] = React.useState<Act>(null);
  /** A command that didn't go says so, and `back` puts back what it showed at once. */
  const command = (c: Command, what: string, back?: () => void) =>
    harnessApi.command(a.id, c).catch((e: unknown) => {
      back?.();
      toast.error(`Couldn't ${what}`, errorMessage(e));
    });
  // Each of its changes goes one at a time (W21-R15): while one is on its way, its item says so and
  // a press on it does nothing, so no answer lands out of order or puts back a later change.
  // Kiln keeps it going when it stops part-way unless switched off here (shown at once, kept on the server).
  const [keepSet, setKeepSet] = React.useState<boolean | null>(null);
  const keeping = keepSet ?? !a.keep_off;
  const [keep, keepGoing] = useAction((on: boolean) => {
    setKeepSet(on);
    inLists((l) => l.map((x) => (x.id === a.id ? { ...x, keep_off: !on } : x)));
    return command({ type: "control", id: "keep_going", value: on }, on ? "keep it going" : "stop keeping it going", () => {
      setKeepSet(!on);
      readLists();
    });
  });
  // Once the agent says so too, its own word shows (changed elsewhere later, that shows).
  if (keepSet !== null && !keepGoing && keepSet === !a.keep_off) setKeepSet(null);
  // A name given here shows at once in both lists, and here while the agent still has the one it
  // had; back if it couldn't be given. Once it went and the agent's name is another, that one shows
  // (given again elsewhere too).
  const [named, setNamed] = React.useState<{ to: string; from: string; went?: boolean } | null>(null);
  if (named?.went && a.name !== named.from) setNamed(null);
  const name = named && a.name === named.from ? named.to : a.name;
  const [rename, naming] = useAction((n: string) => {
    setNamed({ to: n, from: a.name });
    inLists((l) => l.map((x) => (x.id === a.id ? { ...x, name: n } : x)));
    return command({ type: "control", id: "rename", value: n }, "rename it", () => {
      setNamed(null);
      readLists();
    }).then(() => setNamed((cur) => (cur?.to === n ? { ...cur, went: true } : cur)));
  });
  // Renaming from the menu (or a line it opens under the row): the menu mustn't hand focus back to its button, the field takes it.
  const renameNext = React.useRef(false);
  // A row moved by what was chosen (pinned, unpinned…): focus goes back to its button where the row now is, the page not scrolling to it.
  const stayNext = React.useRef(false);
  const button = React.useRef<HTMLButtonElement>(null);
  const live = a.status !== "exited";
  // Stopped through its session where that shows (its head then says Stopping…, as its own Stop
  // does); from elsewhere over HTTP, saying so if it couldn't.
  const [stopTurn, stoppingTurn] = useAction(() => {
    const s = sessionStore(a.id);
    if (!s.idle()) return s.stop();
    return command({ type: "interrupt" }, "interrupt it");
  });
  // The menu has closed: a restart says it's on its way.
  const [restart, restarting] = useAction(() => {
    toast({ title: `Restarting ${name}`, tone: "running" });
    return command({ type: "restart" }, "restart it");
  });
  // Ending it ends the agents it started too, unless the dialog says otherwise; the dialog says
  // meanwhile that it's on its way, and why if it couldn't.
  const ending = useEndWithChildren(a.id);
  const end = async () => {
    await ending.end();
    after?.onGone?.();
  };
  // An ended one leaves both lists at once, and is back if it couldn't go (its machine kept it: an archive or restore under way).
  const [remove, removing] = useAction(
    async () => {
      inLists((l) => l.filter((x) => x.id !== a.id));
      try {
        await harnessApi.stop(a.id);
      } catch (e) {
        readLists();
        throw e;
      }
      after?.onGone?.();
    },
    after?.onFailed ? (e: unknown) => after.onFailed?.(errorMessage(e)) : "Couldn't remove the agent",
  );
  const items: { label: string; onSelect: () => void; danger?: boolean; hidden?: boolean; going?: boolean }[] = [
    {
      label: "Rename…",
      onSelect: () => {
        renameNext.current = true;
        setAct("rename");
      },
      going: naming,
    },
    { label: "Stop the current turn", onSelect: stopTurn, hidden: a.status !== "running" && a.status !== "waiting", going: stoppingTurn },
    { label: "Restart", onSelect: restart, hidden: live, going: restarting },
    // Mission control's composer, filled in as it runs: its harness, machine, folder, permissions and model (new-agent-draft.ts).
    {
      label: "Duplicate",
      onSelect: () => startFrom(navigate, { from: `Like ${agentLabel(a)}`, kind: a.kind, target: a.target, cwd: a.cwd || undefined, mode: a.mode || undefined, model: a.model || undefined }),
    },
    // Agents the kiln server itself runs stay there once they've started; an ended one goes nowhere.
    { label: "Move to…", onSelect: () => setAct("move"), hidden: !live || !movable(a) },
    { label: "Where commands run…", onSelect: () => setAct("reach"), hidden: !a.target.startsWith("agent:") },
    { label: "Copy session id", onSelect: () => void copyText(a.harness_session ?? a.id).then((ok) => ok && toast.success("Copied")) },
    { label: live ? "End agent…" : "Remove", onSelect: () => (live ? setAct("end") : remove()), danger: true, going: removing },
  ];
  const menu = (trigger?: React.ReactNode, extra: { label: string; onSelect: () => void }[] = [], organize: OrganizeItem[] = []) => (
    // A row's menu in Mission control (with where it's kept) doesn't lock the page's scroll: in WebKit that lock moved the list it opens over.
    <DropdownMenu.Root modal={organize.length === 0}>
      <DropdownMenu.Trigger asChild>
        {trigger ?? (
          <button ref={button} type="button" className="hx-icon" aria-label={`Actions for ${a.name}`}>
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
          // Never taller than the room it has (a phone's row mid-screen): what doesn't fit scrolls within it.
          style={{ maxHeight: "var(--radix-dropdown-menu-content-available-height)", overflowY: "auto" }}
          onCloseAutoFocus={(e) => {
            if (renameNext.current) {
              renameNext.current = false;
              e.preventDefault();
            } else if (stayNext.current) {
              stayNext.current = false;
              e.preventDefault();
              button.current?.focus({ preventScroll: true });
            }
          }}
        >
          {[...extra, ...items]
            .filter((i) => !("hidden" in i && i.hidden))
            .map((i) => {
              const going = "going" in i && Boolean(i.going);
              return (
                <DropdownMenu.Item
                  key={i.label}
                  className="menu-item"
                  {...sendingProps(going)}
                  onSelect={(e) => (going ? e.preventDefault() : i.onSelect())}
                  style={"danger" in i && i.danger ? { color: "var(--color-fail)" } : undefined}
                >
                  <span className="flex-1">{i.label}</span>
                </DropdownMenu.Item>
              );
            })}
          {organize.length > 0 && <DropdownMenu.Separator className="my-1 h-px bg-white/10" />}
          {organize.map((i) => (
            <DropdownMenu.Item
              key={i.label}
              className="menu-item"
              disabled={i.disabled}
              onSelect={() => {
                if (i.opens) renameNext.current = true;
                else stayNext.current = true;
                i.onSelect();
              }}
            >
              <span className="flex-1">{i.label}</span>
            </DropdownMenu.Item>
          ))}
          <DropdownMenu.Separator className="my-1 h-px bg-white/10" />
          <React.Suspense fallback={null}>
            <MuteItem id={a.id} />
          </React.Suspense>
          <DropdownMenu.CheckboxItem className="menu-item" checked={keeping} {...sendingProps(keepGoing)} onSelect={(e) => e.preventDefault()} onCheckedChange={keep}>
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
        description={
          <>
            {`Its process stops on the machine and it leaves the board${a.left_behind ? `, along with the ${a.left_behind === 1 ? "shell or monitor" : `${a.left_behind} shells and monitors`} it left running` : ""}. Its changes stay on disk, and its conversation can be resumed from the harness later.`}
            {act === "end" && ending.note}
          </>
        }
        confirmLabel="End agent"
        danger
        onConfirm={end}
      />
      {act === "move" && <MoveDialog agent={a} onClose={() => setAct(null)} />}
      <React.Suspense fallback={null}>{act === "reach" && <ReachDialog agent={a} onClose={() => setAct(null)} />}</React.Suspense>
    </>
  );
  return {
    menu,
    dialogs,
    end: () => (live ? setAct("end") : remove()),
    live,
    /** An ended one's removal is on its way. */
    removing,
    /** Its name as shown: one given here, at once. */
    name,
    /** A name given here is on its way: the next waits for it. */
    naming,
    /** Its name is being edited where it shows (`NameField`). */
    renaming: act === "rename",
    startRenaming: () => !naming && setAct("rename"),
    stopRenaming: () => setAct(null),
    /** Names it in kiln (and in its harness, which names conversations too); "" gives it back its harness's name. */
    rename,
  };
}

/** Mute an agent's notifications for you, on every device (and unmute): where push is on here. */
function MuteItem({ id }: { id: string }) {
  const push = useRes(pushStateRes);
  const muted = push.data.muted.includes(id);
  if (!push.data.available && !muted) return null;
  const toggle = () => {
    push.change((s) => ({ ...s, muted: muted ? s.muted.filter((m) => m !== id) : [...s.muted, id] }));
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

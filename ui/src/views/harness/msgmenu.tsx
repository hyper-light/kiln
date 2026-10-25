"use client";
/**
 * The ⋯ on a message: one menu for prompts and for what the agent said, with the message's own
 * actions (reply, copy, attachments) and, on yours, where to take the conversation from there
 * (restore files, rewind, branch). What can't run stays visible with why.
 */
import * as React from "react";
import { DropdownMenu } from "radix-ui";
import { MoreHorizontal } from "lucide-react";
import { cn } from "@/lib/utils";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent } from "@/components/ui/dialog";

/** A row in a message's menu. `disabled` is the reason it can't run now (shown, not hidden). */
export interface MenuRow {
  icon?: React.ReactNode;
  label: string;
  /** A quiet note at the row's right (a count, what it does). */
  hint?: string;
  danger?: boolean;
  /** Why it can't run now; the row stays, greyed, saying so. */
  disabled?: string;
  /** Asks before running. */
  confirm?: { title: string; body: string };
  onSelect?: () => void;
}

export interface MenuGroup {
  /** A labeled section ("Go back"), no label for the first, message-local one. */
  label?: string;
  rows: MenuRow[];
}

function Row({ r, onPick }: { r: MenuRow; onPick: (r: MenuRow) => void }) {
  if (r.disabled !== undefined) {
    return (
      <div className="menu-item hx-menu-dead" title={r.disabled} aria-disabled="true">
        {r.icon}
        <span className="flex-1">{r.label}</span>
        <span className="hx-menu-why">{r.disabled}</span>
      </div>
    );
  }
  return (
    <DropdownMenu.Item className={cn("menu-item", r.danger && "text-[var(--color-fail)]")} onSelect={() => onPick(r)}>
      {r.icon}
      <span className="flex-1">{r.label}</span>
      {r.hint && <span className="hx-menu-hint">{r.hint}</span>}
    </DropdownMenu.Item>
  );
}

/** The small ⋯ at a message's corner, opening its actions. Always there, for touch too. */
export function MsgMenu({ groups, label, className }: { groups: MenuGroup[]; label: string; className?: string }) {
  const [ask, setAsk] = React.useState<{ title: string; body: string; run: () => void } | null>(null);
  const pick = (r: MenuRow) => {
    if (r.confirm) setAsk({ title: r.confirm.title, body: r.confirm.body, run: () => r.onSelect?.() });
    else r.onSelect?.();
  };
  return (
    <>
      <DropdownMenu.Root>
        <DropdownMenu.Trigger asChild>
          <button type="button" className={cn("hx-icon hx-msg-menu", className)} aria-label={label} onClick={(e) => e.stopPropagation()}>
            <MoreHorizontal size={14} />
          </button>
        </DropdownMenu.Trigger>
        <DropdownMenu.Portal>
          <DropdownMenu.Content className="popover w-64" sideOffset={4} align="end">
            {groups.map((g, i) => (
              <React.Fragment key={i}>
                {i > 0 && <DropdownMenu.Separator className="hx-menu-sep" />}
                {g.label && <div className="hx-eyebrow hx-menu-group">{g.label}</div>}
                {g.rows.map((r, j) => (
                  <Row key={j} r={r} onPick={pick} />
                ))}
              </React.Fragment>
            ))}
          </DropdownMenu.Content>
        </DropdownMenu.Portal>
      </DropdownMenu.Root>
      <Dialog open={ask !== null} onOpenChange={(o) => !o && setAsk(null)}>
        {ask && (
          <DialogContent title={ask.title} eyebrow="Confirm" description={ask.body}>
            <div className="flex justify-end gap-2">
              <Button variant="ghost" onClick={() => setAsk(null)}>
                Cancel
              </Button>
              <Button
                variant="danger"
                onClick={() => {
                  ask.run();
                  setAsk(null);
                }}
              >
                Go ahead
              </Button>
            </div>
          </DialogContent>
        )}
      </Dialog>
    </>
  );
}

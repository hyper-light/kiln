import * as React from "react";
import { Dialog as D } from "radix-ui";
import { X } from "lucide-react";
import { sendingProps } from "@/lib/action";
import { toast } from "./toast";
import { errorMessage } from "@/lib/hooks";
import { cn } from "@/lib/utils";

export const Dialog = D.Root;
export const DialogTrigger = D.Trigger;
export const DialogClose = D.Close;

export function DialogContent({
  title,
  eyebrow,
  description,
  children,
  className,
  variant = "dialog",
  onOpenAutoFocus,
  onCloseAutoFocus,
}: {
  title: React.ReactNode;
  eyebrow?: React.ReactNode;
  description?: React.ReactNode;
  children?: React.ReactNode;
  className?: string;
  variant?: "dialog" | "drawer";
  onOpenAutoFocus?: (e: Event) => void;
  onCloseAutoFocus?: (e: Event) => void;
}) {
  return (
    <D.Portal>
      <D.Overlay className="overlay" />
      <D.Content
        className={cn(variant === "drawer" ? "drawer" : "dialog", className)}
        onOpenAutoFocus={onOpenAutoFocus}
        onCloseAutoFocus={onCloseAutoFocus}
        aria-describedby={description ? undefined : undefined}
      >
        <div className="flex items-start justify-between gap-4 px-6 pt-5 pb-1">
          <div className="min-w-0">
            {eyebrow && <span className="panel-eyebrow">{eyebrow}</span>}
            <D.Title className="text-[19px] font-[450] leading-snug tracking-[-0.025em] text-[#e1e5e9]">{title}</D.Title>
            {description ? (
              <D.Description className="mt-1.5 text-[12.5px] leading-relaxed text-[#aab3c1]">{description}</D.Description>
            ) : (
              <D.Description className="sr-only">{typeof title === "string" ? title : "Dialog"}</D.Description>
            )}
          </div>
          <D.Close asChild>
            <button className="icon-btn -mr-2 -mt-1" data-variant="bare" aria-label="Close">
              <X size={17} />
            </button>
          </D.Close>
        </div>
        <div className="px-6 pt-4 pb-6">{children}</div>
      </D.Content>
    </D.Portal>
  );
}

/**
 * Simple confirmation dialog. Confirmed, it closes once `onConfirm` has done, its button saying
 * meanwhile that it's on its way (W21-R15); Cancel still closes it (what was confirmed goes on), and
 * one that failed says why here (or in a toast, once it's closed). One confirm at a time: while one
 * is on its way, the button says so in every opening, and a press does nothing; an earlier opening's
 * answer (cancelled, the dialog then opened again, maybe for another row) never closes this one.
 */
export function ConfirmDialog({
  open,
  onOpenChange,
  title,
  description,
  confirmLabel,
  danger,
  onConfirm,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  title: string;
  description: React.ReactNode;
  confirmLabel: string;
  danger?: boolean;
  onConfirm: () => void | Promise<void>;
}) {
  const [shown, setShown] = React.useState({ open, n: 0 });
  if (shown.open !== open) setShown({ open, n: open ? shown.n + 1 : shown.n });
  const opening = React.useRef(shown.n);
  opening.current = shown.n;
  const openNow = React.useRef(open);
  openNow.current = open;
  /** The opening whose confirm is on its way, and why one's failed. */
  const [going, setGoing] = React.useState<number | null>(null);
  const [failed, setFailed] = React.useState<{ n: number; why: string } | null>(null);
  const confirm = () => {
    const n = shown.n;
    if (going !== null) return;
    let done: unknown;
    try {
      done = onConfirm();
    } catch (e) {
      setFailed({ n, why: errorMessage(e) });
      return;
    }
    setGoing(n);
    setFailed(null);
    void Promise.resolve(done)
      .then(
        () => {
          if (opening.current === n) onOpenChange(false);
        },
        (e: unknown) => {
          // Closed since: said where it can still be read.
          if (opening.current === n && openNow.current) setFailed({ n, why: errorMessage(e) });
          else toast.error(`${confirmLabel}: it didn't go through`, errorMessage(e));
        },
      )
      .finally(() => setGoing((g) => (g === n ? null : g)));
  };
  const why = failed?.n === shown.n ? failed.why : null;
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent title={title} eyebrow="Confirm" description={description}>
        {why && (
          <p role="alert" className="mb-4 text-[12.5px] leading-[1.55] text-[var(--color-fail)] [overflow-wrap:anywhere]">
            {why}
          </p>
        )}
        <div className="flex flex-wrap justify-end gap-2">
          <DialogClose asChild>
            <button className="inline-flex min-h-11 items-center rounded-full px-5 text-sm text-[#b4b5ba] hover:bg-white/[0.06] hover:text-white">Cancel</button>
          </DialogClose>
          <button
            {...sendingProps(going !== null)}
            onClick={confirm}
            className={cn(
              "inline-flex min-h-11 items-center rounded-full border px-5 text-sm font-medium transition-colors disabled:opacity-40",
              danger ? "border-[#d1aaad]/40 bg-[#d1aaad]/[0.06] text-[#e7c9cb] hover:border-[#d1aaad]/70 hover:bg-[#d1aaad]/[0.12]" : "border-[#e4e5e7] bg-[#e4e5e7] text-[#111214] hover:bg-white",
            )}
          >
            {confirmLabel}
          </button>
        </div>
      </DialogContent>
    </Dialog>
  );
}

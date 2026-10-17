import * as React from "react";
import { Dialog as D } from "radix-ui";
import { X } from "lucide-react";
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

/** Simple confirmation dialog. */
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
  const [busy, setBusy] = React.useState(false);
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent title={title} eyebrow="Confirm" description={description}>
        <div className="flex flex-wrap justify-end gap-2">
          <DialogClose asChild>
            <button className="inline-flex min-h-11 items-center rounded-full px-5 text-sm text-[#b4b5ba] hover:bg-white/[0.06] hover:text-white">
              Cancel
            </button>
          </DialogClose>
          <button
            disabled={busy}
            onClick={async () => {
              setBusy(true);
              try {
                await onConfirm();
                onOpenChange(false);
              } finally {
                setBusy(false);
              }
            }}
            className={cn(
              "inline-flex min-h-11 items-center rounded-full border px-5 text-sm font-medium transition-colors disabled:opacity-40",
              danger
                ? "border-[#d1aaad]/40 bg-[#d1aaad]/[0.06] text-[#e7c9cb] hover:border-[#d1aaad]/70 hover:bg-[#d1aaad]/[0.12]"
                : "border-[#e4e5e7] bg-[#e4e5e7] text-[#111214] hover:bg-white",
            )}
          >
            {confirmLabel}
          </button>
        </div>
      </DialogContent>
    </Dialog>
  );
}

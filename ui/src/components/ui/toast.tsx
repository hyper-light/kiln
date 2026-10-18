import * as React from "react";
import { Toast as T } from "radix-ui";
import { X } from "lucide-react";
import { useNavigate } from "@/lib/router";
import { StatusDot } from "./status";

export interface ToastItem {
  id: number;
  title: string;
  description?: string;
  tone?: "succeeded" | "failed" | "timed_out" | "running" | "info";
  href?: string;
  duration?: number;
}

let seq = 0;
let items: ToastItem[] = [];
const listeners = new Set<() => void>();
const emit = () => listeners.forEach((l) => l());

export function toast(t: Omit<ToastItem, "id">) {
  const item = { ...t, id: ++seq };
  items = [...items.slice(-4), item];
  emit();
  return item.id;
}
toast.error = (title: string, description?: string) => toast({ title, description, tone: "failed" });
toast.success = (title: string, description?: string) => toast({ title, description, tone: "succeeded" });

function dismiss(id: number) {
  items = items.filter((i) => i.id !== id);
  emit();
}

function useToasts() {
  return React.useSyncExternalStore(
    (cb) => {
      listeners.add(cb);
      return () => listeners.delete(cb);
    },
    () => items,
    () => items,
  );
}

export function Toaster() {
  const list = useToasts();
  const navigate = useNavigate();
  return (
    <T.Provider swipeDirection="right" duration={6000}>
      {list.map((t) => (
        <T.Root
          key={t.id}
          className="toast"
          duration={t.duration}
          onOpenChange={(open) => {
            if (!open) window.setTimeout(() => dismiss(t.id), 200);
          }}
        >
          <span className="mt-[7px]">
            <StatusDot status={t.tone === "info" || !t.tone ? "skipped" : t.tone} />
          </span>
          <div className="min-w-0 flex-1">
            <T.Title className="text-[13px] font-[450] leading-snug text-[#eceef2]">
              {t.href ? (
                <button
                  className="text-left hover:underline hover:underline-offset-4"
                  onClick={() => {
                    navigate(t.href!);
                    dismiss(t.id);
                  }}
                >
                  {t.title}
                </button>
              ) : (
                t.title
              )}
            </T.Title>
            {t.description && (
              <T.Description className="mt-1 break-words font-mono text-[10.5px] leading-relaxed text-[#9a9da6]">
                {t.description}
              </T.Description>
            )}
          </div>
          <T.Close aria-label="Dismiss" className="icon-btn -my-2 -mr-1" data-variant="bare" data-size="sm">
            <X size={14} />
          </T.Close>
        </T.Root>
      ))}
      <T.Viewport className="toast-viewport" />
    </T.Provider>
  );
}

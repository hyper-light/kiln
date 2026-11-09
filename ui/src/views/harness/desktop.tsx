/** The desktop conversation and its inspector: a remembered width, or a small reopen rail. */
import * as React from "react";
import { PanelRightClose, PanelRightOpen } from "lucide-react";
import { useElementWidth, useLocalStorage } from "@/lib/hooks";
import { Tip } from "@/components/ui/tooltip";

export const INSPECTOR_COLLAPSED = "kiln.agents.inspector.collapsed";

export function DesktopLayout({
  children,
  side,
  collapsed,
  onCollapsed,
  frame,
}: {
  children: React.ReactNode;
  side: (toggle: React.ReactNode) => React.ReactNode;
  collapsed: boolean;
  onCollapsed: (collapsed: boolean) => void;
  /** W21-R02: the geometry owner can surround keyed session content without being remounted. */
  frame?: (body: React.ReactElement<React.HTMLAttributes<HTMLDivElement>>, actions: React.ReactNode, resetDrag: () => void) => React.ReactNode;
}) {
  const [measure, available] = useElementWidth<HTMLDivElement>();
  const [saved, save] = useLocalStorage<number | null>("kiln.agents.inspector.width", null);
  const [dragWidth, setDragWidth] = React.useState<number | null>(null);
  const drag = React.useRef<{ x: number; start: number; width: number; pointer: number } | null>(null);
  const resizeRef = React.useRef<HTMLDivElement>(null);
  const focusFrame = React.useRef<number | null>(null);
  const cancelDrag = React.useCallback(() => {
    const pointer = drag.current?.pointer;
    drag.current = null;
    if (pointer !== undefined && resizeRef.current?.hasPointerCapture(pointer)) resizeRef.current.releasePointerCapture(pointer);
    if (document.activeElement === resizeRef.current) resizeRef.current?.blur();
    setDragWidth(null);
  }, []);
  const resetDrag = React.useCallback(() => {
    cancelDrag();
    if (focusFrame.current !== null) cancelAnimationFrame(focusFrame.current);
    focusFrame.current = null;
  }, [cancelDrag]);
  const resizeHandle = React.useCallback(
    (node: HTMLDivElement | null) => {
      if (!node) return;
      resizeRef.current = node;
      return () => {
        // A breakpoint/collapse removes the handle while its geometry owner survives. Cancel
        // before losing the node, so lost capture cannot save a previous view's unfinished drag.
        cancelDrag();
        if (resizeRef.current === node) resizeRef.current = null;
      };
    },
    [cancelDrag],
  );
  const toggleRef = React.useRef<HTMLButtonElement>(null);
  const id = React.useId();
  const total = available || 1100;
  const min = 320;
  // Keep both the conversation and the inspector usable, including after the window shrinks.
  const max = Math.max(min, Math.min(total - 480, Math.floor(total * 0.65)));
  const clamp = (value: number) => Math.round(Math.max(min, Math.min(max, value)));
  const preferred = typeof saved === "number" && Number.isFinite(saved) ? saved : Math.max(380, total * 0.34);
  const width = clamp(dragWidth ?? preferred);
  const toggle = () => {
    onCollapsed(!collapsed);
    if (focusFrame.current !== null) cancelAnimationFrame(focusFrame.current);
    focusFrame.current = requestAnimationFrame(() => {
      focusFrame.current = null;
      toggleRef.current?.focus({ preventScroll: true });
    });
  };
  const finish = (event: React.PointerEvent<HTMLDivElement>) => {
    if (!drag.current || drag.current.pointer !== event.pointerId) return;
    // A native viewport change can release capture before React removes the desktop handle.
    // Only a completed desktop drag is a preference; an interrupted drag is never saved.
    if (!matchMedia("(min-width: 1100px)").matches || collapsed || !event.currentTarget.isConnected) {
      cancelDrag();
      return;
    }
    save(clamp(drag.current.width));
    drag.current = null;
    setDragWidth(null);
  };
  const label = collapsed ? "Show side tray" : "Collapse side tray";
  const control = (
    <Tip content={label} side="left">
      <button ref={toggleRef} type="button" className="hx-tray-toggle" aria-label={label} aria-expanded={!collapsed} aria-controls={id} onClick={toggle}>
        {collapsed ? <PanelRightOpen size={16} aria-hidden="true" /> : <PanelRightClose size={16} aria-hidden="true" />}
      </button>
    </Tip>
  );
  const body = (
    <div ref={measure} className="hx-agent-body hx-desktop" data-resizing={dragWidth !== null || undefined} style={{ gridTemplateColumns: `minmax(0, 1fr) ${collapsed ? 44 : width}px` }}>
      {children}
      <div id={id} className="hx-desktop-tray" hidden={collapsed}>
        {side(collapsed ? null : control)}
      </div>
      {collapsed ? (
        <div className="hx-tray-rail">{control}</div>
      ) : (
        <div
          ref={resizeHandle}
          className="hx-tray-resize"
          style={{ right: width - 5 }}
          role="separator"
          tabIndex={0}
          aria-label="Resize side tray"
          aria-orientation="vertical"
          aria-controls={id}
          aria-valuemin={min}
          aria-valuemax={max}
          aria-valuenow={width}
          aria-valuetext={`${width} pixels wide`}
          title="Drag or use the arrow keys to resize. Double-click to reset."
          onPointerDown={(e) => {
            if (e.button !== 0 || !e.isPrimary) return;
            e.preventDefault();
            e.currentTarget.focus({ preventScroll: true });
            e.currentTarget.setPointerCapture(e.pointerId);
            drag.current = { x: e.clientX, start: width, width, pointer: e.pointerId };
            setDragWidth(width);
          }}
          onPointerMove={(e) => {
            if (!drag.current) return;
            const next = clamp(drag.current.start + drag.current.x - e.clientX);
            drag.current.width = next;
            setDragWidth(next);
          }}
          onPointerUp={finish}
          onLostPointerCapture={() => {
            // Pointerup already saved and cleared a completed drag. Unexpected capture loss
            // (resize, removed surface or browser cancellation) must discard pending width.
            if (drag.current) cancelDrag();
          }}
          onPointerCancel={() => {
            drag.current = null;
            setDragWidth(null);
          }}
          onDoubleClick={() => save(null)}
          onKeyDown={(e) => {
            const step = e.shiftKey ? 64 : 16;
            const next = e.key === "ArrowLeft" ? width + step : e.key === "ArrowRight" ? width - step : e.key === "Home" ? min : e.key === "End" ? max : null;
            if (next !== null) {
              e.preventDefault();
              save(clamp(next));
            } else if (e.key === "Enter") {
              e.preventDefault();
              toggle();
            }
          }}
        />
      )}
    </div>
  );
  return frame ? frame(body, control, resetDrag) : body;
}

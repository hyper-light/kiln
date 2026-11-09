/** W21-R02: persistent console geometry; only one keyed session owns its visible content.
 * Portals keep session hooks, handlers and context together while the desktop measurement,
 * columns and outer window-sized surface survive direct agent-to-agent navigation.
 */
import * as React from "react";
import { createPortal } from "react-dom";
import { useLocalStorage, useMediaQuery } from "@/lib/hooks";
import { DesktopLayout, INSPECTOR_COLLAPSED } from "./desktop";
import { takeTyping } from "./early";

type Slot = "head" | "work" | "side" | "tabs" | "after" | "controller";
type Slots = Partial<Record<Slot, HTMLDivElement>>;
type DragHandlers = Pick<React.HTMLAttributes<HTMLDivElement>, "onDragEnter" | "onDragOver" | "onDragLeave" | "onDrop">;
interface SessionView {
  id: string;
  ready: boolean;
  busy?: boolean;
  gone: boolean;
  work: boolean;
  handlers?: DragHandlers;
}
interface FrameContext {
  slots: Slots;
  wide: boolean;
  actions: React.ReactNode;
  setSlot: (name: Slot, node: HTMLDivElement | null) => void;
  attach: (owner: symbol, read: () => SessionView) => () => void;
}
const Context = React.createContext<FrameContext | null>(null);

/** The same native CSS animation, including its name/events and reduced-motion rules.
 * Restarting its local CSSAnimation avoids replacing the measured outer element or forcing a
 * geometry read merely to remove/re-add a class. No animation is started under paused motion.
 */
function entrance(node: HTMLDivElement, ready: boolean, id: string, state: { id: string | null; animation: Animation | null }) {
  if (!ready) {
    state.animation?.cancel();
    state.id = null;
    return;
  }
  if (state.id === id) return;
  state.id = id;
  const style = getComputedStyle(node);
  if (!style.animationName.split(",").some((name) => name.trim() === "ag-ready")) {
    state.animation = null;
    return;
  }
  const current = node.getAnimations().find((animation) => "animationName" in animation && animation.animationName === "ag-ready");
  const animation = current ?? state.animation;
  if (!animation) return;
  state.animation = animation;
  animation.currentTime = 0;
  animation.play();
}

function FrameSlot({ name, className, hidden }: { name: Slot; className?: string; hidden?: boolean }) {
  const context = React.use(Context);
  const setSlot = context?.setSlot;
  const ref = React.useCallback(
    (node: HTMLDivElement | null) => {
      if (!node || !setSlot) return;
      setSlot(name, node);
      return () => setSlot(name, null);
    },
    [name, setSlot],
  );
  return <div ref={ref} data-agent-slot={name} className={className} style={{ display: hidden ? "none" : className ? undefined : "contents" }} />;
}

function FrameRoot({
  id,
  body,
  actions,
  resetDrag,
  children,
}: {
  id: string;
  body: React.ReactElement<React.HTMLAttributes<HTMLDivElement>>;
  actions: React.ReactNode;
  resetDrag: () => void;
  children: React.ReactNode;
}) {
  const wide = useMediaQuery("(min-width: 1100px)");
  const [slots, setSlots] = React.useState<Slots>({});
  const [view, setView] = React.useState<SessionView & { attached: boolean }>({ id, ready: false, gone: false, work: true, attached: false });
  const owner = React.useRef<{ token: symbol; read: () => SessionView } | null>(null);
  const lastSession = React.useRef<string | null>(null);
  const live = React.useRef(false);
  const animation = React.useRef<{ id: string | null; animation: Animation | null }>({ id: null, animation: null });
  const setSlot = React.useCallback((name: Slot, node: HTMLDivElement | null) => {
    setSlots((previous) => {
      if (previous[name] === (node ?? undefined)) return previous;
      const next = { ...previous };
      if (node) next[name] = node;
      else delete next[name];
      return next;
    });
  }, []);
  const attach = React.useCallback(
    (token: symbol, read: () => SessionView) => {
      const next = read();
      if (next.id !== id) return () => {};
      owner.current = { token, read };
      if (lastSession.current !== id) {
        lastSession.current = id;
        resetDrag();
      }
      setView((previous) =>
        previous.id === id && previous.ready === next.ready && previous.busy === next.busy && previous.gone === next.gone && previous.work === next.work && previous.attached
          ? previous
          : { ...next, attached: true },
      );
      return () => {
        if (owner.current?.token !== token) return;
        owner.current = null;
        // A changed ref or a fallback attaches its replacement in this commit. Only a genuinely
        // absent controller (an error/unmount) clears its view; no obsolete session may clear a new one.
        queueMicrotask(() => {
          if (live.current && owner.current === null) setView((previous) => (previous.id === id ? { id, ready: false, gone: false, work: true, attached: false } : previous));
        });
      };
    },
    [id, resetDrag],
  );
  const current = view.id === id ? view : { id, ready: false, gone: false, work: true, attached: false };
  const ready = current.ready;
  const root = React.useCallback(
    (node: HTMLDivElement | null) => {
      if (!node) return;
      live.current = true;
      entrance(node, ready, id, animation.current);
      if (ready) takeTyping(node);
      return () => {
        live.current = false;
      };
    },
    [id, ready],
  );
  const context = React.useMemo(() => ({ slots, wide, actions, setSlot, attach }), [slots, wide, actions, setSlot, attach]);
  const dispatch = (name: keyof DragHandlers, event: React.DragEvent<HTMLDivElement>) => owner.current?.read().handlers?.[name]?.(event);
  return (
    <Context.Provider value={context}>
      <div
        ref={root}
        data-agent-frame=""
        data-agent-session={id}
        className={ready ? "hx-agent ag-ready" : "hx-agent"}
        aria-busy={current.attached && (current.busy ?? !ready) ? true : undefined}
        onDragEnter={(event) => dispatch("onDragEnter", event)}
        onDragOver={(event) => dispatch("onDragOver", event)}
        onDragLeave={(event) => dispatch("onDragLeave", event)}
        onDrop={(event) => dispatch("onDrop", event)}
      >
        <FrameSlot name="head" />
        {/* The controller's portal target never changes at the desktop/phone breakpoint.
            Its work portal may remount a timeline, as before; its session hooks must not. */}
        <FrameSlot name="controller" />
        {wide ? (
          React.cloneElement(body, { style: { ...body.props.style, display: current.gone ? "none" : undefined } })
        ) : (
          <div className="hx-agent-phone" style={{ display: current.gone ? "none" : undefined }}>
            <FrameSlot name="tabs" />
            <FrameSlot name="work" className="hx-main" hidden={!current.work} />
            <FrameSlot name="side" />
          </div>
        )}
        <FrameSlot name="after" />
        {slots.controller && createPortal(children, slots.controller)}
      </div>
    </Context.Provider>
  );
}

export function AgentFrame({ id, children }: { id: string; children: React.ReactNode }) {
  const [collapsed, onCollapsed] = useLocalStorage(INSPECTOR_COLLAPSED, false);
  return (
    <DesktopLayout
      collapsed={collapsed}
      onCollapsed={onCollapsed}
      side={() => <FrameSlot name="side" />}
      frame={(body, actions, resetDrag) => (
        <FrameRoot id={id} body={body} actions={collapsed ? null : actions} resetDrag={resetDrag}>
          {children}
        </FrameRoot>
      )}
    >
      <FrameSlot name="work" className="hx-main" />
    </DesktopLayout>
  );
}

/** The keyed controller publishes all these portals in the same React commit. No previous
 * session is mounted in a hidden view; the old controller's state and handlers are discarded.
 */
export function AgentFrameContent({
  id,
  ready,
  busy,
  gone = false,
  workVisible = true,
  handlers,
  head,
  work,
  side,
  tabs,
  after,
}: {
  id: string;
  ready: boolean;
  busy?: boolean;
  gone?: boolean;
  workVisible?: boolean;
  handlers?: DragHandlers;
  head: React.ReactNode;
  work: React.ReactNode;
  side: (actions: React.ReactNode) => React.ReactNode;
  tabs: React.ReactNode;
  after?: React.ReactNode;
}) {
  const context = React.use(Context);
  const [token] = React.useState(() => Symbol());
  const attach = context?.attach;
  const mark = React.useCallback(
    (node: HTMLSpanElement | null) => {
      if (!node || !attach) return;
      // Capture handlers in this commit's ref, rather than publishing an interrupted render.
      return attach(token, () => ({ id, ready, busy, gone, work: workVisible, handlers }));
    },
    [attach, token, id, ready, busy, gone, workVisible, handlers],
  );
  if (!context?.slots.head || !context.slots.work || !context.slots.side || !context.slots.after) return null;
  const { slots, wide, actions } = context;
  return (
    <>
      {createPortal(head, slots.head!)}
      {createPortal(!gone && (wide || workVisible) ? work : null, slots.work!)}
      {createPortal(!gone && (wide || !workVisible) ? side(wide ? actions : null) : null, slots.side!)}
      {!wide && slots.tabs && createPortal(!gone ? tabs : null, slots.tabs)}
      {createPortal(
        <>
          <span hidden ref={mark} />
          {after}
        </>,
        slots.after!,
      )}
    </>
  );
}

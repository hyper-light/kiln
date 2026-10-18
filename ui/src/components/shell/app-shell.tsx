import * as React from "react";
import { Link, NavLink, Outlet, useLocation } from "react-router";
import { UpdateNotice } from "./update-notice";
import { ArrowUpRight, LogOut, Menu, Search, Settings, X } from "lucide-react";
import { api } from "@/api/client";
import { useServerEvents } from "@/api/events";
import type { Me, Status } from "@/api/types";
import { Brand } from "./brand";
import { ConnectionIndicator } from "./connection";
import { CommandPalette, setPaletteOpen, usePalette } from "./command-palette";
import { MotionToggle } from "./motion";
import { Button } from "@/components/ui/button";
import { Kbd } from "@/components/ui/misc";
import { ErrorBoundary } from "@/components/ui/error-boundary";
import { Loading } from "@/components/ui/loading";
import { toast } from "@/components/ui/toast";
import { Tip } from "@/components/ui/tooltip";
import { pad2, shortSha } from "@/lib/format";

const TerminalWorkspace = React.lazy(() => import("@/views/terminal/workspace"));

export const NAV = [
  { to: "/", label: "Overview", end: true },
  { to: "/runs", label: "Runs" },
  { to: "/deployments", label: "Deployments" },
  { to: "/repos", label: "Repos" },
  { to: "/runners", label: "Runners" },
  { to: "/agents", label: "Agents" },
  { to: "/terminal", label: "Terminal" },
  { to: "/telemetry", label: "Metrics" },
  { to: "/system", label: "System" },
];

const isMac = typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform);

function useRunFailureToasts() {
  const seen = React.useRef(new Map<string, Status>());
  useServerEvents((ev) => {
    if (ev.type !== "run") return;
    const { run } = ev;
    const prev = seen.current.get(run.id);
    seen.current.set(run.id, run.status);
    if (seen.current.size > 500) {
      const first = seen.current.keys().next().value;
      if (first !== undefined) seen.current.delete(first);
    }
    if (prev === run.status) return;
    if (run.status === "failed" || run.status === "timed_out") {
      toast({
        tone: run.status,
        title: `${run.repo_name} #${run.number} ${run.status === "failed" ? "failed" : "timed out"}`,
        description: `${run.pipeline} · ${run.branch} · ${shortSha(run.sha)}${run.error ? ` — ${run.error}` : ""}`,
        href: `/runs/${run.id}`,
        duration: 9000,
      });
    }
  });
}

/** A press or tap outside the header closes what is open in it. */
function closeOnPressAway(close: () => void) {
  return (el: HTMLElement | null) => {
    if (!el) return;
    const header = el.closest(".site-header") ?? el;
    const onDown = (e: PointerEvent) => {
      if (!header.contains(e.target as Node)) close();
    };
    document.addEventListener("pointerdown", onDown);
    return () => document.removeEventListener("pointerdown", onDown);
  };
}

const NOT_TYPED = new Set(["button", "checkbox", "color", "file", "image", "radio", "range", "reset", "submit"]);

/** A field a phone opens its keyboard for. */
function typingInto(el: Element | null) {
  if (el instanceof HTMLInputElement) return !NOT_TYPED.has(el.type);
  return el instanceof HTMLTextAreaElement || el instanceof HTMLSelectElement || (el instanceof HTMLElement && el.isContentEditable);
}

/**
 * A page that fills the window never scrolls, but a phone pans it up to keep a field clear of its
 * keyboard, and iOS can leave it there once the keyboard has gone: the composer stranded mid-screen
 * over an empty band. Once nothing is being typed into (and the page isn't zoomed), it goes back.
 */
function settleWhenTyped(el: HTMLElement | null) {
  if (!el) return;
  const view = window.visualViewport;
  let timer = 0;
  const settle = () => {
    window.clearTimeout(timer);
    timer = window.setTimeout(() => {
      const zoomed = view ? Math.abs(view.scale - 1) > 0.01 : false;
      if (window.scrollY !== 0 && !zoomed && !typingInto(document.activeElement)) window.scrollTo({ top: 0, behavior: "instant" });
    }, 150);
  };
  view?.addEventListener("resize", settle);
  window.addEventListener("scroll", settle, { passive: true });
  document.addEventListener("focusout", settle);
  return () => {
    window.clearTimeout(timer);
    view?.removeEventListener("resize", settle);
    window.removeEventListener("scroll", settle);
    document.removeEventListener("focusout", settle);
  };
}

export function AppShell({ me }: { me: Me | null }) {
  const location = useLocation();
  const onTerminal = location.pathname.startsWith("/terminal");
  // An agent's workspace fills the window like the terminal does (no footer, no page scroll).
  const fullHeight = onTerminal || /^\/agents\/(?!sessions(?:\/|$))[^/]+/.test(location.pathname);
  // The terminal stays mounted once opened, so its sessions live on while you're elsewhere.
  const [termMounted, setTermMounted] = React.useState(onTerminal);
  if (onTerminal && !termMounted) setTermMounted(true);
  // The phones' menu, open on the page it was opened on: going anywhere closes it.
  const [menuAt, setMenuAt] = React.useState<string | null>(null);
  const menuOpen = menuAt === location.pathname;
  const closeMenu = React.useCallback(() => setMenuAt(null), []);
  const menuAway = React.useMemo(() => closeOnPressAway(closeMenu), [closeMenu]);
  const paletteOpen = usePalette((s) => s.open);
  useRunFailureToasts();

  return (
    <div className="flex min-h-dvh flex-col">
      <a className="skip-link" href="#main">
        Skip to content
      </a>
      <header className="site-header">
        {/* The same on every page (the terminal's too), so nothing in it moves between pages. */}
        <div className="header-inner container-console">
          <Link
            to="/"
            aria-label="kiln overview"
            className="inline-flex min-h-11 items-center rounded-sm outline-none focus-visible:ring-2 focus-visible:ring-white/80 focus-visible:ring-offset-4 focus-visible:ring-offset-[#08090a]"
          >
            <Brand />
          </Link>
          <nav className="desktop-nav hidden xl:flex" aria-label="Main navigation">
            {NAV.map((n, i) => (
              <NavLink key={n.to} to={n.to} end={n.end}>
                <span className="nav-index" aria-hidden="true">
                  {pad2(i + 1)}
                </span>
                {n.label}
              </NavLink>
            ))}
          </nav>
          <div className="ml-auto flex items-center gap-1.5">
            <UpdateNotice />
            <ConnectionIndicator />
            <button
              type="button"
              onClick={() => setPaletteOpen(!paletteOpen)}
              aria-expanded={paletteOpen}
              className="hidden min-h-11 items-center gap-2.5 rounded-full px-3 text-[12px] text-[#a5a5ac] transition-colors hover:bg-white/[0.05] hover:text-white sm:inline-flex"
              aria-label="Open command palette"
            >
              <Search size={14} />
              <span className="hidden xl:inline">Search</span>
              <span className="flex gap-1">
                <Kbd>{isMac ? "⌘" : "Ctrl"}</Kbd>
                <Kbd>K</Kbd>
              </span>
            </button>
            <Tip content="Settings">
              <Link to="/settings" className="icon-btn hidden sm:inline-grid" data-variant="bare" aria-label="Settings">
                <Settings size={16} />
              </Link>
            </Tip>
            <Tip content={me?.hostname ? `Sign out of ${me.hostname}` : "Sign out"}>
              <button className="icon-btn hidden sm:inline-grid" data-variant="bare" aria-label="Sign out" onClick={() => void api.logout()}>
                <LogOut size={16} />
              </button>
            </Tip>
            <Button
              variant="ghost"
              size="icon"
              className="xl:hidden [&_svg]:size-5"
              aria-label={menuOpen ? "Close navigation" : "Open navigation"}
              aria-expanded={menuOpen}
              aria-controls="site-menu"
              onClick={() => setMenuAt(menuOpen ? null : location.pathname)}
            >
              {menuOpen ? <X aria-hidden="true" /> : <Menu aria-hidden="true" />}
            </Button>
          </div>
        </div>
        {menuOpen && (
          <div
            ref={menuAway}
            id="site-menu"
            className="header-band xl:hidden"
            onKeyDown={(e) => {
              if (e.key === "Escape") closeMenu();
            }}
          >
            <nav className="container-console pb-6" aria-label="Mobile navigation">
              {[...NAV, { to: "/settings", label: "Settings", end: false }].map((n, i) => (
                <NavLink
                  key={n.to}
                  to={n.to}
                  end={n.end}
                  onClick={closeMenu}
                  className="flex min-h-14 items-center justify-between gap-4 border-b border-white/10 px-1 py-3 text-xl tracking-tight text-[#a3a5ad] transition-colors hover:text-white aria-[current=page]:text-white"
                >
                  <span className="flex items-baseline gap-3">
                    <span className="font-mono text-[9px] tracking-[0.08em] text-[#6d7078]">{pad2(i + 1)}</span>
                    {n.label}
                  </span>
                  <ArrowUpRight className="size-5" aria-hidden="true" />
                </NavLink>
              ))}
              <div className="mt-5 flex items-center justify-between">
                <MotionToggle />
                <button className="motion-toggle" onClick={() => void api.logout()}>
                  <LogOut size={11} /> Sign out
                </button>
              </div>
            </nav>
          </div>
        )}
        <CommandPalette />
      </header>

      <main id="main" ref={fullHeight ? settleWhenTyped : undefined} className={fullHeight ? "relative flex min-h-0 flex-1 flex-col" : "page-main flex-1"}>
        {!onTerminal && (
          <ErrorBoundary resetKey={location.pathname}>
            <React.Suspense fallback={<RouteFallback />}>
              <Outlet />
            </React.Suspense>
          </ErrorBoundary>
        )}
        {termMounted && (
          <div className={onTerminal ? "flex min-h-0 flex-1 flex-col" : "hidden"} aria-hidden={!onTerminal}>
            <ErrorBoundary label="Terminal crashed">
              <React.Suspense fallback={<RouteFallback />}>
                <TerminalWorkspace visible={onTerminal} />
              </React.Suspense>
            </ErrorBoundary>
          </div>
        )}
      </main>

      {!fullHeight && (
        <footer className="site-footer">
          <div className="container-console flex flex-wrap items-center justify-between gap-x-6 gap-y-1">
            <span className="flex items-center gap-2.5">
              <span className="spectral-dot" aria-hidden="true" />
              KILN {me?.version ? `v${me.version}` : ""} {me?.hostname ? `· ${me.hostname.toUpperCase()}` : ""}
            </span>
            <span className="flex items-center gap-5">
              <a className="inline-flex min-h-11 items-center gap-1 hover:text-white" href="/metrics" target="_blank" rel="noreferrer">
                /METRICS <ArrowUpRight size={10} />
              </a>
              <MotionToggle />
            </span>
          </div>
        </footer>
      )}
    </div>
  );
}

/** A page whose code is still on its way: the header and the menu stay, the loader takes the page's place. */
export function RouteFallback() {
  return (
    <div className="container-console page" aria-busy="true">
      <Loading label="Loading the page" block />
    </div>
  );
}

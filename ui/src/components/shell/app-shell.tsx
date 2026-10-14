import * as React from "react";
import { Link, NavLink, Outlet, useLocation } from "react-router";
import { Dialog } from "radix-ui";
import { ArrowUpRight, LogOut, Menu, Search, Settings, X } from "lucide-react";
import { api } from "@/api/client";
import { useServerEvents } from "@/api/events";
import type { Me, Status } from "@/api/types";
import { Brand } from "./brand";
import { ConnectionIndicator } from "./connection";
import { CommandPalette, setPaletteOpen } from "./command-palette";
import { MotionToggle } from "./motion";
import { Button } from "@/components/ui/button";
import { Kbd } from "@/components/ui/misc";
import { ErrorBoundary } from "@/components/ui/error-boundary";
import { toast } from "@/components/ui/toast";
import { Tip } from "@/components/ui/tooltip";
import { pad2, shortSha } from "@/lib/format";

const TerminalWorkspace = React.lazy(() => import("@/views/terminal/workspace"));

export const NAV = [
  { to: "/", label: "Overview", end: true },
  { to: "/runs", label: "Runs" },
  { to: "/repos", label: "Repos" },
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

export function AppShell({ me }: { me: Me | null }) {
  const location = useLocation();
  const onTerminal = location.pathname.startsWith("/terminal");
  const [termMounted, setTermMounted] = React.useState(onTerminal);
  const [menuOpen, setMenuOpen] = React.useState(false);
  React.useEffect(() => {
    if (onTerminal) setTermMounted(true);
  }, [onTerminal]);
  React.useEffect(() => setMenuOpen(false), [location.pathname]);
  useRunFailureToasts();

  return (
    <div className="flex min-h-dvh flex-col">
      <a className="skip-link" href="#main">
        Skip to content
      </a>
      <Dialog.Root open={menuOpen} onOpenChange={setMenuOpen}>
        <header className="site-header">
          <div className={onTerminal ? "header-inner px-4 sm:px-6" : "header-inner container-console"}>
            <Link
              to="/"
              aria-label="kiln overview"
              className="inline-flex min-h-11 items-center rounded-sm outline-none focus-visible:ring-2 focus-visible:ring-white/80 focus-visible:ring-offset-4 focus-visible:ring-offset-[#08090a]"
            >
              <Brand />
            </Link>
            <nav className="desktop-nav hidden lg:flex" aria-label="Main navigation">
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
              <ConnectionIndicator />
              <button
                type="button"
                onClick={() => setPaletteOpen(true)}
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
              <Dialog.Trigger asChild>
                <Button variant="ghost" size="icon" className="lg:hidden [&_svg]:size-5" aria-label="Open navigation">
                  <Menu aria-hidden="true" />
                </Button>
              </Dialog.Trigger>
            </div>
          </div>
        </header>
        <Dialog.Portal>
          <Dialog.Overlay className="fixed inset-0 z-[80] bg-black/65 backdrop-blur-sm" />
          <Dialog.Content
            className="fixed inset-x-0 top-0 z-[90] max-h-[100dvh] overflow-y-auto border-b border-white/10 bg-[#08090a] px-5 pt-[max(0.5rem,env(safe-area-inset-top))] pb-8 text-[#e4e5e7] shadow-2xl outline-none sm:px-8"
            style={{ animation: "dialog-in-top .3s var(--ease)" }}
          >
            <Dialog.Title className="sr-only">Navigation</Dialog.Title>
            <Dialog.Description className="sr-only">kiln console sections</Dialog.Description>
            <div className="flex min-h-16 items-center justify-between gap-4">
              <Brand />
              <Dialog.Close asChild>
                <Button variant="ghost" size="icon" aria-label="Close navigation">
                  <X aria-hidden="true" />
                </Button>
              </Dialog.Close>
            </div>
            <nav className="mt-4 flex flex-col" aria-label="Mobile navigation">
              {[...NAV, { to: "/settings", label: "Settings", end: false }].map((n, i) => (
                <NavLink
                  key={n.to}
                  to={n.to}
                  end={n.end}
                  className="flex min-h-14 items-center justify-between gap-4 border-b border-white/10 px-1 py-3 text-xl tracking-tight text-[#a3a5ad] transition-colors hover:text-white aria-[current=page]:text-white"
                >
                  <span className="flex items-baseline gap-3">
                    <span className="font-mono text-[9px] tracking-[0.08em] text-[#6d7078]">{pad2(i + 1)}</span>
                    {n.label}
                  </span>
                  <ArrowUpRight className="size-5" aria-hidden="true" />
                </NavLink>
              ))}
            </nav>
            <div className="mt-6 flex items-center justify-between">
              <MotionToggle />
              <button className="motion-toggle" onClick={() => void api.logout()}>
                <LogOut size={11} /> Sign out
              </button>
            </div>
          </Dialog.Content>
        </Dialog.Portal>
      </Dialog.Root>

      <main id="main" className={onTerminal ? "relative flex min-h-0 flex-1 flex-col" : "flex-1"}>
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

      {!onTerminal && (
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
      <CommandPalette />
    </div>
  );
}

export function RouteFallback() {
  return (
    <div className="container-console page" aria-busy="true">
      <div className="skeleton h-3 w-40" />
      <div className="skeleton mt-6 h-10 w-80 max-w-full" />
      <div className="skeleton mt-10 h-48 w-full" />
    </div>
  );
}

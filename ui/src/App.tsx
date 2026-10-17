import * as React from "react";
import { BrowserRouter, Navigate, Route, Routes } from "react-router";
import { api, onAuthChange } from "@/api/client";
import { events } from "@/api/events";
import type { Me } from "@/api/types";
import { AppShell, RouteFallback } from "@/components/shell/app-shell";
import { MotionProvider } from "@/components/shell/motion";
import { ErrorBoundary } from "@/components/ui/error-boundary";
import { Toaster } from "@/components/ui/toast";
import { TooltipProvider } from "@/components/ui/tooltip";
import { Login } from "@/views/login";
import { BrandMark } from "@/components/shell/brand";

const Overview = React.lazy(() => import("@/views/overview"));
const Runs = React.lazy(() => import("@/views/runs"));
const RunDetail = React.lazy(() => import("@/views/run-detail/index"));
const Deployments = React.lazy(() => import("@/views/deployments/index"));
const DeploymentApp = React.lazy(() => import("@/views/deployments/app"));
const DeployPlans = React.lazy(() => import("@/views/deployments/plans"));
const Repos = React.lazy(() => import("@/views/repos"));
const Plans = React.lazy(() => import("@/views/plans/index"));
const PlanPage = React.lazy(() => import("@/views/plans/plan"));
const SharedPlan = React.lazy(() => import("@/views/plans/shared"));
const Runners = React.lazy(() => import("@/views/runners"));
const AddMachines = React.lazy(() => import("@/views/add-machines"));
// Found sessions live in Mission control now; their old page sends you there.
const FoundRedirect = React.lazy(() => import("@/views/harness/found"));
const Mission = React.lazy(() => import("@/views/harness/mission"));
const AgentPage = React.lazy(() => import("@/views/harness/agent"));
const Metrics = React.lazy(() => import("@/views/metrics"));
const System = React.lazy(() => import("@/views/system"));
const Settings = React.lazy(() => import("@/views/settings"));
const NotFound = React.lazy(() => import("@/views/not-found"));

type AuthState = { kind: "loading" } | { kind: "anon"; error?: string } | { kind: "authed"; me: Me | null };

function useAuth() {
  const [state, setState] = React.useState<AuthState>({ kind: "loading" });

  const check = React.useCallback(() => {
    api
      .me()
      .then((me) => setState(me.authenticated ? { kind: "authed", me } : { kind: "anon" }))
      .catch((e: unknown) => {
        const status = (e as { status?: number }).status;
        setState(status === 401 ? { kind: "anon" } : { kind: "anon", error: e instanceof Error ? e.message : String(e) });
      });
  }, []);

  React.useEffect(() => {
    check();
    return onAuthChange((authed) => {
      if (!authed) setState({ kind: "anon" });
      else check();
    });
  }, [check]);

  React.useEffect(() => {
    if (state.kind === "authed") events.start();
    else events.stop();
  }, [state.kind]);

  return { state, recheck: check };
}

export function App() {
  const { state, recheck } = useAuth();

  return (
    <MotionProvider>
      <TooltipProvider>
        <BrowserRouter>
          <ErrorBoundary label="kiln crashed">
            {window.location.pathname.startsWith("/shared/plans/") ? (
              // A shared plan opens for anyone with its link, signed in or not.
              <React.Suspense fallback={<Splash />}>
                <Routes>
                  <Route path="shared/plans/:token" element={<SharedPlan />} />
                </Routes>
              </React.Suspense>
            ) : state.kind === "loading" ? (
              <Splash />
            ) : state.kind === "anon" ? (
              <Login error={state.error} onRetry={recheck} />
            ) : (
              <Routes>
                <Route element={<AppShell me={state.me} />}>
                  <Route index element={<Overview />} />
                  <Route path="runs" element={<Runs />} />
                  <Route path="runs/:id" element={<RunDetail />} />
                  <Route path="runs/:id/jobs/:jobId" element={<RunDetail />} />
                  <Route path="deployments" element={<Deployments />} />
                  <Route path="deployments/plans" element={<DeployPlans />} />
                  <Route path="deployments/apps/:project/:name" element={<DeploymentApp />} />
                  <Route path="repos" element={<Repos />} />
                  <Route path="sync" element={<Navigate to="/repos?show=synced" replace />} />
                  <Route path="runners" element={<Runners />} />
                  <Route path="runners/add" element={<AddMachines />} />
                  <Route path="agents" element={<Mission />} />
                  <Route path="agents/sessions" element={<FoundRedirect />} />
                  <Route path="agents/plans" element={<Plans />} />
                  <Route path="agents/plans/:id" element={<PlanPage />} />
                  <Route path="agents/:id" element={<AgentPage />} />
                  <Route path="terminal" element={null} />
                  <Route path="telemetry" element={<Metrics />} />
                  <Route path="system" element={<System />} />
                  <Route path="settings" element={<Settings />} />
                  <Route
                    path="*"
                    element={
                      <React.Suspense fallback={<RouteFallback />}>
                        <NotFound />
                      </React.Suspense>
                    }
                  />
                </Route>
              </Routes>
            )}
            <Toaster />
          </ErrorBoundary>
        </BrowserRouter>
      </TooltipProvider>
    </MotionProvider>
  );
}

function Splash() {
  return (
    <div className="grid min-h-dvh place-items-center" aria-busy="true" aria-label="Loading kiln">
      <div className="flex flex-col items-center gap-5 text-[#ededee]">
        <BrandMark className="h-9 w-8 opacity-90" />
        <span className="spectrum-rule" />
      </div>
    </div>
  );
}

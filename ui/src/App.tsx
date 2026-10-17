import * as React from "react";
import { BrowserRouter, Navigate, Route, Routes } from "react-router";
import { AppShell, RouteFallback } from "@/components/shell/app-shell";
import { ErrorBoundary } from "@/components/ui/error-boundary";
import { Loading } from "@/components/ui/loading";
import { Toaster } from "@/components/ui/toast";
import { TooltipProvider } from "@/components/ui/tooltip";
import { checkAuth, useAuth } from "@/lib/auth";
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

// Who this is, asked as the app starts (not after its first render).
void checkAuth();

export function App() {
  const auth = useAuth();

  return (
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
          ) : auth.kind === "loading" ? (
            <Splash />
          ) : auth.kind === "anon" ? (
            <Login error={auth.error} onRetry={() => void checkAuth()} />
          ) : (
            <Routes>
              <Route element={<AppShell me={auth.me} />}>
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
  );
}

/** The first moment, while the app asks who this is: kiln's mark and the loader. */
function Splash() {
  return (
    <div className="grid min-h-dvh place-items-center" aria-busy="true">
      <div className="flex flex-col items-center gap-5 text-[#ededee]">
        <BrandMark className="h-9 w-8 opacity-90" />
        <Loading label="Loading kiln" />
      </div>
    </div>
  );
}

/**
 * What's around every signed-in page: who this is (sign-in until it's known), then kiln's shell with
 * the page inside (routes/_app.tsx). Rendered in the browser.
 */
import * as React from "react";
import { AppShell } from "@/components/shell/app-shell";
import { Splash } from "@/components/shell/splash";
import { ErrorBoundary } from "@/components/ui/error-boundary";
import { Toaster } from "@/components/ui/toast";
import { checkAuth, startAuth, useAuth } from "@/lib/auth";
import { PrivateBootstrapContext } from "@/lib/private-bootstrap-context";
import { harnessSessionsRes } from "@/lib/data";
import { listed } from "@/api/harness";
import { seedResource } from "@/lib/resource";

/** Sign-in, loaded only when someone isn't signed in. */
const Login = React.lazy(() => import("@/views/login").then((m) => ({ default: m.Login })));

/** Sign-in until it's known who this is, then kiln's shell with the page in it. */
export function SignedIn() {
  const bootstrap = React.use(PrivateBootstrapContext);
  // The root loader has hydrated before this ClientOnly page mounts. Initialize before reading
  // the hook: no duplicate /api/me and no private store mutation during a server render.
  startAuth(bootstrap, typeof location === "undefined" ? "" : location.pathname, () => {
    if (bootstrap?.sessions) seedResource(harnessSessionsRes.key, bootstrap.sessions.filter(listed), harnessSessionsRes.fetch, harnessSessionsRes.opts);
  });
  const auth = useAuth();
  return (
    <Providers>
      {auth.kind === "loading" ? (
        <Splash />
      ) : auth.kind === "anon" ? (
        <React.Suspense fallback={<Splash />}>
          <Login error={auth.error} onRetry={checkAuth} />
        </React.Suspense>
      ) : (
        <AppShell me={auth.me} />
      )}
    </Providers>
  );
}

/** What every page has around it: toasts, and a last word if it crashes. */
export function Providers({ children }: { children: React.ReactNode }) {
  return (
    <ErrorBoundary label="kiln crashed">
      {children}
      <Toaster />
    </ErrorBoundary>
  );
}

/** The first moment, while the app asks who this is (components/shell/splash.tsx). */
export { Splash };

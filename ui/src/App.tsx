/**
 * What's around every signed-in page: who this is (sign-in until it's known), then kiln's shell with
 * the page inside (routes/_app.tsx). Rendered in the browser.
 */
import * as React from "react";
import { AppShell } from "@/components/shell/app-shell";
import { Splash } from "@/components/shell/splash";
import { ErrorBoundary } from "@/components/ui/error-boundary";
import { Toaster } from "@/components/ui/toast";
import { checkAuth, useAuth } from "@/lib/auth";

/** Sign-in, loaded only when someone isn't signed in. */
const Login = React.lazy(() => import("@/views/login").then((m) => ({ default: m.Login })));

// Who this is, asked as the app starts (not after its first render).
if (typeof window !== "undefined") void checkAuth();

/** Sign-in until it's known who this is, then kiln's shell with the page in it. */
export function SignedIn() {
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

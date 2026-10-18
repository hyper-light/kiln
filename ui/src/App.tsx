/**
 * What's around every signed-in page: who this is (sign-in until it's known), then kiln's shell with
 * the page inside (routes/_app.tsx). Rendered in the browser.
 */
import * as React from "react";
import { AppShell } from "@/components/shell/app-shell";
import { BrandMark } from "@/components/shell/brand";
import { ErrorBoundary } from "@/components/ui/error-boundary";
import { Loading } from "@/components/ui/loading";
import { Toaster } from "@/components/ui/toast";
import { TooltipProvider } from "@/components/ui/tooltip";
import { checkAuth, useAuth } from "@/lib/auth";
import { Login } from "@/views/login";

// Who this is, asked as the app starts (not after its first render).
if (typeof window !== "undefined") void checkAuth();

/** Sign-in until it's known who this is, then kiln's shell with the page in it. */
export function SignedIn() {
  const auth = useAuth();
  return (
    <Providers>
      {auth.kind === "loading" ? <Splash /> : auth.kind === "anon" ? <Login error={auth.error} onRetry={() => void checkAuth()} /> : <AppShell me={auth.me} />}
    </Providers>
  );
}

/** What every page has around it: tooltips, toasts, and a last word if it crashes. */
export function Providers({ children }: { children: React.ReactNode }) {
  return (
    <TooltipProvider>
      <ErrorBoundary label="kiln crashed">
        {children}
        <Toaster />
      </ErrorBoundary>
    </TooltipProvider>
  );
}

/** The first moment, while the app asks who this is: kiln's mark and the loader. */
export function Splash() {
  return (
    <div className="grid min-h-dvh place-items-center" aria-busy="true">
      <div className="flex flex-col items-center gap-5 text-[#ededee]">
        <BrandMark className="h-9 w-8 opacity-90" />
        <Loading label="Loading kiln" />
      </div>
    </div>
  );
}

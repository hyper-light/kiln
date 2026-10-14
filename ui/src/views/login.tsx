import * as React from "react";
import { ArrowRight, KeyRound } from "lucide-react";
import { api } from "@/api/client";
import { Brand } from "@/components/shell/brand";
import { Button } from "@/components/ui/button";
import { ErrorState, Optic } from "@/components/ui/misc";
import { errorMessage } from "@/lib/hooks";
import { MotionToggle } from "@/components/shell/motion";

export function Login({ error, onRetry }: { error?: string; onRetry: () => void }) {
  const [token, setToken] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const [err, setErr] = React.useState<string | null>(null);

  React.useEffect(() => {
    document.title = "Sign in · kiln";
  }, []);

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!token.trim()) return;
    setBusy(true);
    setErr(null);
    try {
      await api.login(token.trim());
    } catch (e2) {
      setErr((e2 as { status?: number }).status === 401 ? "That token was not accepted." : errorMessage(e2));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex min-h-dvh flex-col">
      <header className="site-header">
        <div className="header-inner container-console">
          <Brand />
        </div>
      </header>
      <main id="main" className="container-console grid flex-1 items-center gap-10 py-16 md:grid-cols-[1.05fr_1fr]">
        <div data-reveal>
          <div className="eyebrow">
            <span className="spectrum-rule" /> 00 / Sign in
          </div>
          <h1 className="mt-6 text-[clamp(40px,5.4vw,68px)] leading-[1.06] font-[450] tracking-[-0.055em]">
            Your fleet,
            <br />
            <span className="bg-[linear-gradient(110deg,#f0f0ef_5%,#e5e5e8_40%,#a7a6b2_95%)] bg-clip-text text-transparent">one console.</span>
          </h1>
          <p className="mt-6 max-w-[440px] text-[15px] leading-[1.85] text-[#a1a4ad]">
            Run <code className="rounded border border-[#2a2d38] bg-[#191b22] px-1.5 py-0.5 text-[12.5px] text-[#cdc4e0]">kiln login</code>{" "}
            on the server and open the one-time URL it prints — no typing needed. Or paste the admin token below.
          </p>

          <form onSubmit={submit} className="mt-9 max-w-[440px]">
            <label htmlFor="token" className="field-label">
              Admin token
            </label>
            <div className="field">
              <KeyRound size={15} aria-hidden="true" />
              <input
                id="token"
                type="password"
                autoComplete="current-password"
                spellCheck={false}
                placeholder="kiln_…"
                value={token}
                onChange={(e) => setToken(e.target.value)}
                autoFocus
              />
            </div>
            <div className="mt-5 flex flex-wrap items-center gap-3">
              <Button type="submit" disabled={busy || !token.trim()}>
                {busy ? "Signing in…" : "Sign in"} <ArrowRight size={16} />
              </Button>
              {error && (
                <Button variant="ghost" onClick={onRetry}>
                  Retry connection
                </Button>
              )}
            </div>
            {(err ?? error) && (
              <div className="mt-5">
                <ErrorState error={err ?? `Could not reach the server: ${error}`} compact />
              </div>
            )}
          </form>
          <div className="mt-12 flex items-center gap-2.5 font-mono text-[9px] text-[#858891]">
            <span className="tiny-cross">+</span>Self-hosted. Session cookie only; tokens never leave this origin.
          </div>
        </div>
        <div className="hidden items-center justify-center md:flex" aria-hidden="true">
          <div className="relative">
            <Optic size={380} />
            <div className="absolute inset-x-0 -bottom-6 text-center font-mono text-[8px] tracking-[0.11em] text-[#7b7e88]">
              KILN / CONTINUOUS INTEGRATION
            </div>
          </div>
        </div>
      </main>
      <footer className="site-footer">
        <div className="container-console flex items-center justify-between">
          <span className="flex items-center gap-2.5">
            <span className="spectral-dot" /> HYPERLIGHT / KILN
          </span>
          <MotionToggle />
        </div>
      </footer>
    </div>
  );
}

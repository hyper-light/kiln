import * as React from "react";
import { ArrowRight, KeyRound, LockKeyhole } from "lucide-react";
import { ApiError, api } from "@/api/client";
import { Brand } from "@/components/shell/brand";
import { Button } from "@/components/ui/button";
import { ErrorState, Optic } from "@/components/ui/misc";
import { errorMessage } from "@/lib/hooks";
import { MotionToggle } from "@/components/shell/motion";

type Mode = "password" | "token";

/** `?signin=…` from a refused one-time link, explained in words. */
function linkProblem(): string | null {
  const why = new URLSearchParams(window.location.search).get("signin");
  if (why === "expired") return "That sign-in link has expired (links last 5 minutes). Sign in below instead.";
  if (why === "foreign")
    return "That sign-in link was made for a different kiln server (signed with another admin token). Sign in below instead.";
  return null;
}

function clearLinkProblem() {
  const url = new URL(window.location.href);
  if (!url.searchParams.has("signin")) return;
  url.searchParams.delete("signin");
  window.history.replaceState(null, "", url.pathname + url.search + url.hash);
}

export function Login({ error, onRetry }: { error?: string; onRetry: () => void }) {
  const [hasPassword, setHasPassword] = React.useState<boolean | null>(null);
  const [mode, setMode] = React.useState<Mode>("password");
  const [secret, setSecret] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const [err, setErr] = React.useState<string | null>(null);
  const [notice] = React.useState(linkProblem);
  const inputRef = React.useRef<HTMLInputElement>(null);

  React.useEffect(() => {
    document.title = "Sign in · kiln";
  }, []);

  React.useEffect(() => {
    const ctl = new AbortController();
    api
      .authMethods(ctl.signal)
      .then((m) => {
        setHasPassword(m.password);
        setMode(m.password ? "password" : "token");
      })
      .catch(() => {
        // An older server, or unreachable: the token always works.
        setHasPassword(false);
        setMode("token");
      });
    return () => ctl.abort();
  }, [error]);

  const switchMode = (m: Mode) => {
    setMode(m);
    setSecret("");
    setErr(null);
    requestAnimationFrame(() => inputRef.current?.focus());
  };

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    const value = mode === "password" ? secret : secret.trim();
    if (!value) return;
    setBusy(true);
    setErr(null);
    try {
      if (mode === "password") await api.loginPassword(value);
      else await api.login(value);
      clearLinkProblem();
    } catch (e2) {
      const status = e2 instanceof ApiError ? e2.status : (e2 as { status?: number }).status;
      if (status === 401) setErr(mode === "password" ? "Wrong password." : "That token was not accepted.");
      else setErr(errorMessage(e2));
    } finally {
      setBusy(false);
    }
  };

  const loading = hasPassword === null;

  return (
    <div className="flex min-h-dvh flex-col">
      <header className="site-header">
        <div className="header-inner container-console">
          <Brand />
        </div>
      </header>
      <main id="main" className="container-console grid flex-1 items-center gap-10 py-10 md:grid-cols-[1.05fr_1fr] md:py-16">
        {/* The rings sit above the form on phones and beside it on wider screens. */}
        <div className="flex justify-center md:hidden" aria-hidden="true">
          <Optic size={216} />
        </div>
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
            {loading
              ? "Checking how this server signs you in…"
              : mode === "password"
                ? "Sign in with your password. It works from any device that can reach this server."
                : hasPassword
                  ? "Paste the admin token. It never expires and always works, even if the password is forgotten."
                  : "Paste the admin token to sign in. Then choose a password in Settings, and every other device can sign in with that instead."}
          </p>

          {notice && (
            <div className="mt-6 max-w-[440px]">
              <ErrorState error={notice} compact />
            </div>
          )}

          <form onSubmit={submit} className="mt-9 max-w-[440px]" autoComplete="on">
            {/* Lets password managers file the password under a username. */}
            <input type="text" name="username" autoComplete="username" value="admin" readOnly tabIndex={-1} aria-hidden="true" className="sr-only" />
            <label htmlFor="secret" className="field-label">
              {mode === "password" ? "Password" : "Admin token"}
            </label>
            <div className="field">
              {mode === "password" ? <LockKeyhole size={15} aria-hidden="true" /> : <KeyRound size={15} aria-hidden="true" />}
              <input
                ref={inputRef}
                id="secret"
                name={mode === "password" ? "password" : "token"}
                type="password"
                autoComplete={mode === "password" ? "current-password" : "off"}
                autoCapitalize="none"
                autoCorrect="off"
                spellCheck={false}
                placeholder={mode === "password" ? "Your password" : "48 hexadecimal characters"}
                value={secret}
                onChange={(e) => setSecret(e.target.value)}
                disabled={loading}
                autoFocus
              />
            </div>
            <div className="mt-5 flex flex-wrap items-center gap-3">
              <Button type="submit" disabled={busy || loading || !secret.trim()}>
                {busy ? "Signing in…" : "Sign in"} <ArrowRight size={16} />
              </Button>
              {hasPassword && (
                <Button type="button" variant="ghost" onClick={() => switchMode(mode === "password" ? "token" : "password")}>
                  {mode === "password" ? "Use the admin token" : "Use your password"}
                </Button>
              )}
              {error && (
                <Button type="button" variant="ghost" onClick={onRetry}>
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

          {mode === "token" && !loading && (
            <details className="mt-8 max-w-[440px] text-[13px] leading-[1.75] text-[#a1a4ad]">
              <summary className="cursor-pointer text-[#cdc4e0] select-none">Where is the admin token?</summary>
              <ul className="mt-3 list-disc space-y-2 pl-5">
                <li>
                  On the machine running kiln: <code className="rounded border border-[#2a2d38] bg-[#191b22] px-1.5 py-0.5 text-[12px] text-[#cdc4e0]">kiln token</code>
                </li>
                <li>
                  Cluster installs also keep it as <code className="rounded border border-[#2a2d38] bg-[#191b22] px-1.5 py-0.5 text-[12px] text-[#cdc4e0]">KILN_ADMIN_TOKEN</code> in{" "}
                  <code className="rounded border border-[#2a2d38] bg-[#191b22] px-1.5 py-0.5 text-[12px] text-[#cdc4e0] break-all">~/.local/state/kiln/cluster/credentials.env</code>
                </li>
                <li>
                  <code className="rounded border border-[#2a2d38] bg-[#191b22] px-1.5 py-0.5 text-[12px] text-[#cdc4e0]">kiln login</code> on that machine prints one-time sign-in links, including one for other devices on the network.
                </li>
              </ul>
            </details>
          )}

          <div className="mt-12 flex items-center gap-2.5 font-mono text-[9px] text-[#858891]">
            <span className="tiny-cross">+</span>Self-hosted. Session cookie only; secrets never leave this origin.
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

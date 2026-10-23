import * as React from "react";
import { ArrowRight, Fingerprint, KeyRound, LockKeyhole, UserRound } from "lucide-react";
import { ApiError, api } from "@/api/client";
import { passkeysHere, signin, type SsoProvider } from "@/api/signin";
import { Brand } from "@/components/shell/brand";
import { Button } from "@/components/ui/button";
import { Loadable } from "@/components/ui/loading";
import { ErrorState, Optic } from "@/components/ui/misc";
import { errorMessage } from "@/lib/hooks";
import { invalidate, useSuspenseResource } from "@/lib/resource";
import { MotionToggle } from "@/components/shell/motion";

type Mode = "password" | "token";

/** `?signin=…` from a refused one-time link, explained in words. */
function linkProblem(): string | null {
  const why = new URLSearchParams(window.location.search).get("signin");
  if (why === "expired") return "That sign-in link has expired (links last 5 minutes). Sign in below instead.";
  if (why === "foreign") return "That sign-in link was made for a different kiln server (signed with another admin token). Sign in below instead.";
  return null;
}

function clearLinkProblem() {
  const url = new URL(window.location.href);
  if (!url.searchParams.has("signin")) return;
  url.searchParams.delete("signin");
  window.history.replaceState(null, "", url.pathname + url.search + url.hash);
}

type Methods = { password: boolean; token: boolean; passkey: boolean; people: boolean; sso: SsoProvider[] };

/** How this server signs people in. An older server, or one out of reach: the token always works. */
const readMethods = async (signal: AbortSignal): Promise<Methods> => {
  const [m, sso] = await Promise.all([api.authMethods(signal).catch(() => ({ password: false, token: true })), signin.sso(signal).catch(() => [])]);
  return { passkey: false, people: false, ...m, sso };
};

export function Login({ error, onRetry }: { error?: string; onRetry: () => void }) {
  const retry = () => {
    invalidate("auth/signin");
    onRetry();
  };
  return (
    <div className="flex min-h-dvh flex-col">
      <title>Sign in · kiln</title>
      <header className="site-header">
        <div className="header-inner container-console">
          <Brand />
        </div>
      </header>
      <main id="main" className="container-console grid flex-1 items-center gap-10 py-10 md:grid-cols-[1.05fr_1fr] md:py-16">
        <Loadable label="how this server signs you in" fallback={<SignIn methods={null} error={error} onRetry={retry} />}>
          <SignInLoaded error={error} onRetry={retry} />
        </Loadable>
        {/* Beside the form on wide screens; below it on phones, as on hyperlight-site's hero. */}
        <div className="login-art" aria-hidden="true">
          <Optic size="var(--optic-size)" />
          <div className="login-art-caption">KILN / CONTINUOUS INTEGRATION</div>
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

function SignInLoaded({ error, onRetry }: { error?: string; onRetry: () => void }) {
  const methods = useSuspenseResource("auth/signin", readMethods).data;
  return <SignIn methods={methods} error={error} onRetry={onRetry} />;
}

/** The sign-in form; `methods` null while the server is asked how it signs people in. */
function SignIn({ methods, error, onRetry }: { methods: Methods | null; error?: string; onRetry: () => void }) {
  const loading = methods === null;
  const hasPassword = methods?.password ?? null;
  // A password signs in when the owner keeps one or any person does (with their name).
  const anyPassword = Boolean(methods?.password || methods?.people);
  const [mode, setMode] = React.useState<Mode>(methods && !anyPassword ? "token" : "password");
  const [secret, setSecret] = React.useState("");
  const [name, setName] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const [err, setErr] = React.useState<string | null>(null);
  const [notice] = React.useState(linkProblem);
  const inputRef = React.useRef<HTMLInputElement>(null);
  // People with passwords of their own sign in with their name (W10); passkeys and hosts beside.
  const people = mode === "password" && (methods?.people ?? false);
  const byPasskey = (methods?.passkey ?? false) && passkeysHere();
  const sso = methods?.sso ?? [];

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
      if (people && name.trim()) await signin.withName(name.trim(), value);
      else if (mode === "password") await api.loginPassword(value);
      else await api.login(value);
      clearLinkProblem();
    } catch (e2) {
      const status = e2 instanceof ApiError ? e2.status : (e2 as { status?: number }).status;
      if (status === 401) setErr(mode === "password" ? (people && name.trim() ? "Wrong name or password." : "Wrong password.") : "That token was not accepted.");
      else setErr(errorMessage(e2));
    } finally {
      setBusy(false);
    }
  };

  const withPasskey = async () => {
    setBusy(true);
    setErr(null);
    try {
      await signin.withPasskey();
      clearLinkProblem();
    } catch (e2) {
      setErr(e2 instanceof DOMException && e2.name === "NotAllowedError" ? "No passkey was used: it was cancelled, or this device has none of this server's." : errorMessage(e2));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div>
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
        {people ? (
          <div className="mb-4">
            <label htmlFor="who" className="field-label">
              Name
            </label>
            <div className="field">
              <UserRound size={15} aria-hidden="true" />
              <input
                id="who"
                name="username"
                type="text"
                autoComplete="username"
                autoCapitalize="none"
                autoCorrect="off"
                spellCheck={false}
                placeholder={hasPassword ? "Yours in kiln; empty for the owner" : "Yours in kiln"}
                value={name}
                onChange={(e) => setName(e.target.value)}
              />
            </div>
          </div>
        ) : (
          /* Lets password managers file the password under a username. */
          <input type="text" name="username" autoComplete="username" value="admin" readOnly tabIndex={-1} aria-hidden="true" className="sr-only" />
        )}
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
            placeholder={mode === "password" ? "Your password" : "kiln_…"}
            value={secret}
            onChange={(e) => setSecret(e.target.value)}
            disabled={loading}
            autoFocus
          />
        </div>
        <div className="mt-5 flex flex-wrap items-center gap-3">
          <Button type="submit" disabled={busy || loading || !secret.trim() || (people && !hasPassword && !name.trim())}>
            {busy ? "Signing in…" : "Sign in"} <ArrowRight size={16} />
          </Button>
          {anyPassword && (
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

      {!loading && (byPasskey || sso.length > 0) && (
        <div className="mt-8 max-w-[440px]">
          <div className="font-mono text-[10px] tracking-[0.14em] text-[#858891] uppercase">Or</div>
          <div className="mt-3 flex flex-wrap gap-3">
            {byPasskey && (
              <Button type="button" variant="secondary" disabled={busy} onClick={() => void withPasskey()}>
                <Fingerprint size={16} aria-hidden="true" /> Sign in with a passkey
              </Button>
            )}
            {sso.map((p) => (
              <Button key={p.provider} asChild variant="secondary">
                <a href={p.start}>Continue with {p.name}</a>
              </Button>
            ))}
          </div>
        </div>
      )}

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
              <code className="rounded border border-[#2a2d38] bg-[#191b22] px-1.5 py-0.5 text-[12px] text-[#cdc4e0]">kiln login</code> on that machine prints one-time sign-in links, including one for
              other devices on the network.
            </li>
          </ul>
        </details>
      )}

      <div className="mt-12 flex items-center gap-2.5 font-mono text-[9px] text-[#858891]">
        <span className="tiny-cross">+</span>Self-hosted. Session cookie only; secrets never leave this origin.
      </div>
    </div>
  );
}

/**
 * Runners → Domain & HTTPS → Tailnet: the front door on the tailnet (Tailscale). Every device on
 * the tailnet finds the front door's names through the runner's DNS, wherever it is: the tailnet's
 * split DNS sends those names to the runner, set by kiln with a Tailscale key given here once, or
 * by hand in Tailscale's admin console (the exact values shown, with copy buttons). The runner
 * checks it the way every tailnet device asks: through Tailscale's own resolver.
 */
import * as React from "react";
import { Check, Copy } from "lucide-react";
import { tailscaleApi } from "@/api/client";
import type { FrontDoorFix, FrontDoorSettings, FrontDoorState, FrontDoorStatus, TailnetStatus } from "@/api/types";
import { Button } from "@/components/ui/button";
import { errorMessage } from "@/lib/hooks";
import { invalidate, useSuspenseResource } from "@/lib/resource";
import { copyText } from "@/views/terminal/clipboard";
import { RelTime } from "@/views/shared";
import "./tailnet.css";

/** One live line, as the section draws them. */
export interface TailnetLine {
  name: string;
  state: FrontDoorState;
  text: string;
  detail?: string;
  fix?: FrontDoorFix | null;
  muted?: boolean;
}

/** The front door's names DNS answers (the tailnet's split DNS sends these). */
function dnsNames(s: FrontDoorSettings): string[] {
  return [s.domain, ...s.aliases].filter(Boolean);
}

/** The Live line for the tailnet. */
export function tailnetLine(s: FrontDoorSettings, st: FrontDoorStatus | null): TailnetLine {
  if (!s.tailscale?.enabled) return { name: "Tailnet", state: "off", text: "off", muted: true };
  const t = st?.tailscale;
  if (!t) return { name: "Tailnet", state: "waiting", text: "waiting for the runner", detail: "asking Tailscale about this machine" };
  if (t.state === "error") return { name: "Tailnet", state: "error", text: t.error ?? "Tailscale is not ready", detail: t.fix ? undefined : t.name || undefined, fix: t.fix };
  if (t.state === "waiting") return { name: "Tailnet", state: "waiting", text: t.error ?? "starting", detail: t.name || undefined };
  const where = [t.name, t.addresses[0]].filter(Boolean).join(" · ");
  if (!s.dns.enabled || !s.domain) return { name: "Tailnet", state: "on", text: `${where} ✓`, detail: "HTTPS answers here on the tailnet" };
  const route = t.routes.find((r) => r.name === s.domain);
  if (route?.here) {
    const resolved = t.resolved?.answers.length ? "resolves through Tailscale" : t.resolved?.error ? `Tailscale's resolver: ${t.resolved.error}` : "";
    return {
      name: "Tailnet",
      state: "on",
      text: `${s.domain} → ${t.nameserver ?? route.to[0] ?? ""} ✓`,
      detail: [t.name, route.exit_node ? "with exit nodes too" : "not with exit nodes", resolved].filter(Boolean).join(" · "),
    };
  }
  return {
    name: "Tailnet",
    state: "waiting",
    text: route?.to.length ? `${s.domain} goes to ${route.to.join(", ")}, not here` : `the tailnet does not send ${s.domain} here yet`,
    detail: t.nameserver ? `point it at ${t.nameserver} below` : where,
  };
}

function CopyButton({ text, label }: { text: string; label: string }) {
  const [done, setDone] = React.useState(false);
  return (
    <button
      type="button"
      className="fd-copy"
      aria-label={done ? "Copied" : label}
      title={done ? "Copied" : label}
      onClick={() =>
        void copyText(text).then((ok) => {
          setDone(ok);
          if (ok) window.setTimeout(() => setDone(false), 1500);
        })
      }
    >
      {done ? <Check size={13} aria-hidden="true" /> : <Copy size={13} aria-hidden="true" />}
    </button>
  );
}

/** A value to type into Tailscale's admin console, with its copy button. */
function Value({ text, label }: { text: string; label: string }) {
  return (
    <span className="tn-value">
      <code>{text}</code>
      <CopyButton text={text} label={label} />
    </span>
  );
}

/**
 * Below the Tailnet switch: where the runner is on the tailnet, and what the tailnet's DNS says for
 * each name, with the one way to make it say "here" that suits: kiln sets it (a Tailscale key, once
 * for every runner), or the values for the admin console.
 */
export function TailnetDetails({ settings, status, dnsOn }: { settings: FrontDoorSettings; status: FrontDoorStatus | null; dnsOn: boolean }) {
  const ts = useSuspenseResource("tailscale", (s) => tailscaleApi.get(s), { on: ["tailscale"] });
  const reload = () => invalidate("tailscale");
  const t: TailnetStatus | null | undefined = status?.tailscale;
  const names = dnsNames(settings);
  const nameserver = t?.nameserver ?? t?.addresses.find((a) => !a.includes(":")) ?? null;
  const k = ts.data;
  const unrouted = names.filter((n) => !t?.routes.find((r) => r.name === n)?.here);

  return (
    <div className="tn">
      <p className="fd-hint">
        {t?.name ? (
          <>
            This runner on the tailnet: <code>{t.name}</code>
            {t.addresses.length > 0 && <> · {t.addresses.join(" · ")}</>}
            {t.tailnet && <> · {t.tailnet}</>}
          </>
        ) : (
          "Devices on your tailnet reach this runner at its Tailscale address; its DNS answers them there with that address."
        )}
      </p>
      {dnsOn && names.length > 0 && (
        <>
          <ul className="tn-routes" aria-label="Where the tailnet sends these names">
            {names.map((n) => {
              const r = t?.routes.find((x) => x.name === n);
              const state: FrontDoorState = !t ? "waiting" : r?.here ? "on" : "waiting";
              return (
                <li key={n} className="tn-route" data-state={state}>
                  <i className="fd-dot" data-state={state} aria-hidden="true" />
                  <code className="tn-name">{n}</code>
                  <span className="tn-to">
                    {r?.here ? `→ this runner${r.exit_node ? ", with exit nodes too" : ""}` : r?.to.length ? `→ ${r.to.join(", ")} (not this runner)` : "→ nowhere yet"}
                  </span>
                </li>
              );
            })}
          </ul>
          {k?.configured ? (
            <div className="fd-actions">
              <span className="fd-msg" role="status" data-tone={k.error ? "error" : "quiet"}>
                {k.error ? (
                  k.error
                ) : (
                  <>
                    kiln keeps the tailnet&apos;s DNS on these names ({k.kind === "oauth" ? "OAuth client" : "API token"} <code>{k.id}</code>)
                    {k.exit_nodes === false && ": devices using an exit node ask the exit node instead (this key can't change that)"}
                    {k.synced_at ? (
                      <>
                        {" "}
                        · checked <RelTime ts={k.synced_at} />
                      </>
                    ) : null}
                  </>
                )}
              </span>
              <Forget onDone={reload} />
            </div>
          ) : (
            unrouted.length > 0 && <Setup names={unrouted} nameserver={nameserver} onConnected={reload} />
          )}
        </>
      )}
    </div>
  );
}

function Forget({ onDone }: { onDone: () => void }) {
  const [busy, setBusy] = React.useState(false);
  return (
    <Button
      type="button"
      size="xs"
      variant="ghost"
      className="fd-btn"
      disabled={busy}
      onClick={() => {
        setBusy(true);
        void tailscaleApi
          .forget()
          .then(onDone)
          .finally(() => setBusy(false));
      }}
    >
      Forget the key
    </Button>
  );
}

/** Not routed here yet: let kiln set it with a key, or type the values into the admin console. */
function Setup({ names, nameserver, onConnected }: { names: string[]; nameserver: string | null; onConnected: () => void }) {
  const [secret, setSecret] = React.useState("");
  const [clientId, setClientId] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const [note, setNote] = React.useState<{ tone: "ok" | "error"; text: string } | null>(null);
  const secretId = React.useId();
  const idId = React.useId();
  const connect = async () => {
    setBusy(true);
    setNote(null);
    try {
      const v = await tailscaleApi.connect(secret, clientId);
      setSecret("");
      setClientId("");
      setNote({ tone: v.error ? "error" : "ok", text: v.error ?? "Connected: kiln sets the tailnet's DNS now." });
      onConnected();
    } catch (e) {
      setNote({ tone: "error", text: errorMessage(e) });
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="tn-setup">
      <div className="tn-way">
        <h4 className="fd-key">Let kiln set it</h4>
        <p className="fd-hint">
          One key covers every runner: in Tailscale&apos;s admin console, Settings → Trust credentials, make an OAuth client with the <b>DNS</b> write scope and paste its secret
          here (an API access token works too, until it expires).
        </p>
        <div className="fd-grid">
          <div className="fd-field">
            <label className="fd-key" htmlFor={secretId}>
              Client secret
            </label>
            <input
              id={secretId}
              className="fd-input"
              type="password"
              value={secret}
              placeholder="tskey-client-…"
              autoComplete="off"
              spellCheck={false}
              onChange={(e) => setSecret(e.target.value)}
            />
          </div>
          <div className="fd-field">
            <label className="fd-key" htmlFor={idId}>
              Client ID
            </label>
            <input id={idId} className="fd-input" value={clientId} placeholder="optional: read from the secret" autoComplete="off" spellCheck={false} onChange={(e) => setClientId(e.target.value)} />
          </div>
        </div>
        <div className="fd-actions">
          <Button type="button" size="xs" variant="secondary" className="fd-btn" disabled={busy || !secret.trim()} onClick={() => void connect()}>
            {busy ? "Checking…" : "Connect"}
          </Button>
          <span className="fd-msg" role="status" data-tone={note?.tone ?? "quiet"}>
            {note?.text ?? "Tried with Tailscale before it is kept; never shown again."}
          </span>
        </div>
      </div>
      <div className="tn-way">
        <h4 className="fd-key">Or by hand</h4>
        {nameserver ? (
          <p className="fd-hint tn-steps">
            Tailscale admin console → DNS → Nameservers → Add nameserver → Custom: <Value text={nameserver} label="Copy the nameserver" />, turn on Restrict to domain with{" "}
            {names.map((n, i) => (
              <React.Fragment key={n}>
                {i > 0 && (i === names.length - 1 ? " and then " : ", then ")}
                <Value text={n} label={`Copy ${n}`} />
              </React.Fragment>
            ))}
            {names.length > 1 ? " (one nameserver entry each)" : ""}, and turn on Use with exit node.
          </p>
        ) : (
          <p className="fd-hint">The address to give Tailscale shows here once the runner&apos;s DNS listens on the tailnet.</p>
        )}
      </div>
    </div>
  );
}

/**
 * Runners → Domain & HTTPS: a runner made into kiln's address on the network (its "front door"):
 * DNS for a domain of any name (kiln.dev), HTTPS in front of the server with a self-signed
 * certificate kiln makes, installs and renews, or your own; a `.local` name as a quiet extra. All
 * in the page's flow, from standard controls: a runner select, text fields, switches, one Save.
 * The certificate in use is shown where it is managed (renew, download, upload, how devices trust
 * it), each action done in place. Live status lines keep their place and height whatever they say
 * (a failing line shows its fix with a copy button in its second line); the one-time setup a
 * runner needs follows.
 */
import * as React from "react";
import { Switch } from "radix-ui";
import { Check, Copy, Download, Minus, Plus } from "lucide-react";
import { frontDoorApi } from "@/api/client";
import { useServerEvents } from "@/api/events";
import type { Agent, CertInfo, CertMode, DnsMode, FrontDoorFix, FrontDoorListener, FrontDoorSettings, FrontDoorState, FrontDoorStatus, FrontDoorView } from "@/api/types";
import { Button } from "@/components/ui/button";
import { SectionHeading } from "@/components/ui/misc";
import { errorMessage, useLocalStorage, useResource } from "@/lib/hooks";
import { cn } from "@/lib/utils";
import { copyText } from "@/views/terminal/clipboard";
import { RelTime } from "@/views/shared";
import "./front-door.css";

/* ------------------------------------------------------------------ the section */

export function FrontDoorSection({ agents, index }: { agents: Agent[]; index: string }) {
  const runners = React.useMemo(() => agents.filter((a) => !a.ephemeral).sort((a, b) => a.name.localeCompare(b.name)), [agents]);
  const doors = useResource((s) => frontDoorApi.list(s), []);
  const [picked, setPicked] = useLocalStorage<string>("kiln.frontdoor.runner", "");
  useServerEvents((ev) => {
    if (ev.type === "front_door") doors.reload();
  });
  // The runner chosen last; else the one whose front door is on; else the first connected one.
  const on = doors.data?.find((d) => d.settings.dns.enabled || d.settings.https.enabled || d.settings.mdns.enabled)?.runner;
  const fallback = on ?? runners.find((a) => a.connected)?.name ?? runners[0]?.name ?? "";
  const runner = runners.some((a) => a.name === picked) ? picked : fallback;
  const agent = runners.find((a) => a.name === runner);
  const selectId = React.useId();
  const summary = doors.data?.find((d) => d.runner === runner);

  return (
    <section className="fd" aria-labelledby="fd-title">
      <SectionHeading
        index={index}
        label="Front door"
        title={<span id="fd-title">Domain &amp; HTTPS</span>}
        aside={summary && summary.origins.length > 0 ? <span className="fd-aside">{summary.origins[0]}</span> : undefined}
      />
      <p className="fd-lead">
        Give kiln any name you like, such as <code>kiln.dev</code>, and make a runner its address on your network: DNS on the runner answers the name, and HTTPS serves kiln under it with a self-signed
        certificate kiln makes and renews, or with your own. Phones need HTTPS for voice input.
      </p>
      {runners.length === 0 ? (
        <p className="fd-quiet">No runners yet: add a machine first.</p>
      ) : (
        <>
          <div className="fd-runner">
            <label className="fd-key" htmlFor={selectId}>
              Runner
            </label>
            <select id={selectId} className="fd-select" value={runner} onChange={(e) => setPicked(e.target.value)}>
              {runners.map((a) => (
                <option key={a.name} value={a.name}>
                  {a.name}
                  {a.connected ? "" : " (offline)"}
                </option>
              ))}
            </select>
            {agent && (
              <span className="fd-runner-state">
                <i className="fd-dot" data-state={agent.connected ? "on" : "off"} aria-hidden="true" />
                {agent.connected ? `${agent.hostname} · ${agent.os}` : "offline"}
              </span>
            )}
          </div>
          {runner && <FrontDoorPanel key={runner} runner={runner} onSaved={doors.reload} />}
        </>
      )}
    </section>
  );
}

/* ------------------------------------------------------------------ one runner */

/** The settings as the form edits them: lists as the text typed, ports as text. */
interface Draft {
  domain: string;
  aliases: string;
  dnsEnabled: boolean;
  dnsMode: DnsMode;
  dnsPort: string;
  upstream: string;
  addresses: string;
  httpsEnabled: boolean;
  httpsPort: string;
  httpPort: string;
  httpsUpstream: string;
  certMode: CertMode;
  mdnsEnabled: boolean;
  mdnsName: string;
}

const list = (v: string[]) => v.join(", ");
const words = (v: string) =>
  v
    .split(/[\s,]+/)
    .map((w) => w.trim())
    .filter(Boolean);

function draftOf(s: FrontDoorSettings): Draft {
  return {
    domain: s.domain,
    aliases: list(s.aliases),
    dnsEnabled: s.dns.enabled,
    dnsMode: s.dns.mode,
    dnsPort: String(s.dns.port),
    upstream: list(s.dns.upstream),
    addresses: list(s.dns.addresses),
    httpsEnabled: s.https.enabled,
    httpsPort: String(s.https.port),
    httpPort: String(s.https.http_port),
    httpsUpstream: s.https.upstream,
    certMode: s.cert.mode,
    mdnsEnabled: s.mdns.enabled,
    mdnsName: s.mdns.name,
  };
}

/** The settings a draft says, on top of the stored ones (so a save keeps what the form does not
 * show), or the field that is not a number. */
function settingsOf(d: Draft, base: FrontDoorSettings): FrontDoorSettings | string {
  const port = (v: string, what: string, zero = false) => {
    const n = Number(v.trim());
    return Number.isInteger(n) && n >= (zero ? 0 : 1) && n <= 65535 ? n : `${what}: ${v.trim() || "nothing"} is not a port`;
  };
  const dnsPort = port(d.dnsPort, "DNS port");
  const httpsPort = port(d.httpsPort, "HTTPS port");
  const httpPort = port(d.httpPort, "HTTP redirect port", true);
  for (const p of [dnsPort, httpsPort, httpPort]) if (typeof p === "string") return p;
  return {
    ...base,
    domain: d.domain.trim(),
    aliases: words(d.aliases),
    dns: { ...base.dns, enabled: d.dnsEnabled, port: dnsPort as number, mode: d.dnsMode, upstream: words(d.upstream), addresses: words(d.addresses) },
    https: { ...base.https, enabled: d.httpsEnabled, port: httpsPort as number, http_port: httpPort as number, upstream: d.httpsUpstream.trim() },
    cert: { ...base.cert, mode: d.certMode },
    mdns: { ...base.mdns, enabled: d.mdnsEnabled, name: d.mdnsName.trim() },
  };
}

function FrontDoorPanel({ runner, onSaved }: { runner: string; onSaved: () => void }) {
  const view = useResource((s) => frontDoorApi.get(runner, s), [runner]);
  useServerEvents((ev) => {
    if ((ev.type === "front_door" && ev.runner === runner) || (ev.type === "agent" && ev.agent.name === runner)) view.reload();
  });
  const v = view.data?.runner === runner ? view.data : undefined;
  const saved = React.useMemo(() => (v ? draftOf(v.settings) : null), [v]);
  const [draft, setDraft] = React.useState<Draft | null>(null);
  const [saving, setSaving] = React.useState(false);
  const [msg, setMsg] = React.useState<{ tone: "ok" | "error" | "quiet"; text: string } | null>(null);
  // Until something is typed, the form follows what is stored (another tab, another admin).
  const d = draft ?? saved;
  const dirty = Boolean(draft && saved && JSON.stringify(draft) !== JSON.stringify(saved));
  const set = (p: Partial<Draft>) => {
    if (d) setDraft({ ...d, ...p });
    setMsg(null);
  };

  const save = async () => {
    if (!d || !v) return;
    const s = settingsOf(d, v.settings);
    if (typeof s === "string") {
      setMsg({ tone: "error", text: s });
      return;
    }
    setSaving(true);
    try {
      const next = await frontDoorApi.save(runner, s);
      view.setData(() => next);
      setDraft(null);
      setMsg({ tone: "ok", text: next.connected ? `Saved. ${runner} applies it now.` : `Saved. ${runner} applies it when it connects.` });
      onSaved();
    } catch (e) {
      setMsg({ tone: "error", text: errorMessage(e) });
    } finally {
      setSaving(false);
    }
  };

  if (view.error && !v) return <p className="fd-msg" data-tone="error">{errorMessage(view.error)}</p>;
  return (
    <div className="fd-body" aria-busy={!v}>
      <StatusLines v={v} />
      {d && v ? (
        <form
          className="fd-form"
          onSubmit={(e) => {
            e.preventDefault();
            void save();
          }}
        >
          <NamesGroup d={d} set={set} />
          <DnsGroup d={d} set={set} v={v} />
          <HttpsGroup d={d} set={set} v={v} onView={(next) => view.setData(() => next)} />
          <div className="fd-actions">
            <Button type="submit" size="xs" className="fd-btn" disabled={saving || !dirty}>
              {saving ? "Saving…" : "Save"}
            </Button>
            <Button type="button" size="xs" variant="ghost" className="fd-btn" disabled={saving || !dirty} onClick={() => setDraft(null)}>
              Revert
            </Button>
            <span className="fd-msg" role="status" data-tone={msg?.tone ?? "quiet"}>
              {msg?.text ?? (dirty ? "Not saved yet." : appliedWords(v))}
            </span>
          </div>
        </form>
      ) : (
        <div className="fd-form fd-loading" aria-hidden="true" />
      )}
      {v?.status && v.status.fixes.length > 0 && <Setup runner={runner} fixes={v.status.fixes} />}
    </div>
  );
}

function appliedWords(v: FrontDoorView): string {
  if (!v.connected) return `${v.runner} is offline: it applies these when it connects.`;
  if (v.supported === false) return `${v.runner} runs a kiln too old for a front door: upgrade it in Settings → Updates.`;
  if (v.applied === false) return `${v.runner} is applying these…`;
  if (v.applied) return `In use on ${v.runner}.`;
  return "";
}

/* ------------------------------------------------------------------ status lines */

/** A line's state: the runner's, or `warn` when it works but needs you soon (amber). */
type Line = { name: string; state: FrontDoorState | "warn"; text: string; detail?: string; fix?: FrontDoorFix | null; muted?: boolean };

const date = (unix: number) => new Date(unix * 1000).toISOString().slice(0, 10);
/** A certificate this close to its end is a problem (the runner renews a self-signed one then). */
const SOON_DAYS = 30;
const daysLeft = (unix: number) => Math.round((unix - Date.now() / 1000) / 86_400);
const days = (n: number) => `${n} day${n === 1 ? "" : "s"}`;

/** The address to show for a listener: a LAN one before loopback, IPv4 before IPv6. */
function where(on: string[]): string {
  const lan = on.filter((a) => !a.startsWith("127.") && !a.startsWith("[::1]"));
  const shown = lan.find((a) => !a.startsWith("[")) ?? lan[0] ?? on[0] ?? "";
  return lan.length > 1 ? `${shown} +${lan.length - 1}` : shown;
}

function port(addr: string): string {
  const i = addr.lastIndexOf(":");
  return i >= 0 ? addr.slice(i) : addr;
}

function linesOf(v: FrontDoorView | undefined): Line[] {
  const s = v?.settings;
  const st = v?.status ?? null;
  const offline = (name: string, on: boolean, detail: string): Line =>
    on ? { name, state: "waiting", text: !v ? "…" : !v.connected ? "runner offline" : v.supported === false ? "this runner's kiln is too old" : "waiting for the runner", detail } : { name, state: "off", text: "off", muted: true };
  const dnsOn = Boolean(s?.dns.enabled);
  const httpsOn = Boolean(s?.https.enabled);
  const httpOn = httpsOn && (s?.https.http_port ?? 0) !== 0;
  const localOn = Boolean(s?.mdns.enabled);
  const names = s ? [s.domain, s.domain ? `*.${s.domain}` : "", ...s.aliases].filter(Boolean).join(", ") : "";
  if (!v || !s || !st || !v.connected || v.supported === false) {
    const lines = [offline("DNS", dnsOn, names), offline("HTTPS", httpsOn, names)];
    if (httpOn) lines.push(offline("HTTP", true, "plain HTTP redirects to HTTPS"));
    if (localOn) lines.push(offline(".local", true, `${s?.mdns.name ?? "kiln"}.local`));
    return lines;
  }
  const standby = (name: string, l: FrontDoorListener): Line | null =>
    st.standby && l.state !== "off" ? { name, state: "waiting", text: "standing by", detail: "another runner process of this machine runs it" } : null;
  const failed = (name: string, l: FrontDoorListener, fallback: string): Line => ({
    name,
    state: "error",
    text: l.error ?? fallback,
    detail: l.fix ? undefined : l.error ?? undefined,
    fix: l.fix,
  });

  const dns = st.dns;
  let dnsLine: Line;
  if (!dnsOn) dnsLine = { name: "DNS", state: "off", text: "off", muted: true };
  else if (standby("DNS", dns)) dnsLine = standby("DNS", dns) as Line;
  else if (dns.state === "on")
    dnsLine = {
      name: "DNS",
      state: dns.error ? "error" : "on",
      text: `${where(dns.on)} ✓ · ${s.dns.mode}`,
      detail: [
        `answers ${names}`,
        s.dns.mode === "resolver" ? (dns.to.length ? `forwards to ${dns.to.join(", ")}` : "no upstream resolver found") : "refuses the rest",
        dns.served ? `${dns.served.toLocaleString()} answered` : "",
      ]
        .filter(Boolean)
        .join(" · "),
      fix: dns.error ? dns.fix : null,
    };
  else dnsLine = failed("DNS", dns, "not listening");

  const h = st.https;
  const cert = st.cert;
  const kind = cert.mode === "kiln" ? "self-signed" : "your own";
  let httpsLine: Line;
  if (!httpsOn) httpsLine = { name: "HTTPS", state: "off", text: "off", muted: true };
  else if (standby("HTTPS", h)) httpsLine = standby("HTTPS", h) as Line;
  else if (h.state === "on" && cert.state === "error") httpsLine = { name: "HTTPS", state: "error", text: cert.error ?? "its certificate cannot be used", detail: "upload a new certificate below" };
  else if (h.state === "on") {
    // What is wrong with the certificate it serves: names it does not cover, its end near.
    const left = cert.info ? daysLeft(cert.info.not_after) : null;
    const problem = [cert.missing.length ? `not valid for ${cert.missing.join(", ")}` : "", left !== null && left < SOON_DAYS ? (left < 1 ? "expires today" : `expires in ${days(left)}`) : ""]
      .filter(Boolean)
      .join(" · ");
    const at = port(h.on[0] ?? "");
    const usual = [cert.info?.names.join(", "), h.to[0] ? `→ ${h.to[0]}` : "", h.served ? `${h.served.toLocaleString()} connection${h.served === 1 ? "" : "s"}` : ""].filter(Boolean).join(" · ");
    if (cert.mode === "kiln" && (cert.renewing || problem))
      httpsLine = { name: "HTTPS", state: "on", text: `${at} ✓ · self-signed · renewing`, detail: problem ? `${problem}: kiln is renewing it` : "a new key and certificate are on their way" };
    else if (problem)
      httpsLine = {
        name: "HTTPS",
        state: "warn",
        text: `${at} ✓ · your own · ${problem}`,
        detail: cert.missing.length
          ? `upload one that covers them${left !== null && left < SOON_DAYS ? `, before ${date(cert.info?.not_after ?? 0)}` : ""}`
          : `upload a new one before ${date(cert.info?.not_after ?? 0)}`,
      };
    else httpsLine = { name: "HTTPS", state: "on", text: `${at} ✓ · ${kind}${cert.info ? `, until ${date(cert.info.not_after)}` : ""}`, detail: usual };
  } else if (h.state === "waiting" || cert.state === "waiting")
    httpsLine = { name: "HTTPS", state: "waiting", text: cert.state === "waiting" ? "waiting for its self-signed certificate" : (h.error ?? "waiting"), detail: cert.error ?? names };
  else if (cert.state === "error") httpsLine = { name: "HTTPS", state: "error", text: cert.error ?? "no certificate", detail: cert.mode === "custom" ? "upload a certificate and its key below" : undefined };
  else httpsLine = failed("HTTPS", h, "not listening");

  const lines = [dnsLine, httpsLine];
  if (httpOn) {
    const x = st.http;
    if (standby("HTTP", x)) lines.push(standby("HTTP", x) as Line);
    else if (x.state === "on") lines.push({ name: "HTTP", state: "on", text: `${port(x.on[0] ?? "")} ✓ → HTTPS`, detail: "plain http:// redirects to https://" });
    else if (x.state === "waiting") lines.push({ name: "HTTP", state: "waiting", text: "waits for HTTPS", detail: "plain http:// redirects to https:// once HTTPS is up" });
    else lines.push(failed("HTTP", x, "not listening"));
  }
  const m = st.mdns;
  if (!localOn) return lines;
  if (standby(".local", m)) lines.push(standby(".local", m) as Line);
  else if (m.state === "on") lines.push({ name: ".local", state: "on", text: `${m.on[0] ?? `${s.mdns.name}.local`} ✓`, detail: "multicast DNS on this network: nothing to set up" });
  else lines.push({ name: ".local", state: m.state === "error" ? "error" : "waiting", text: m.error ?? "starting", detail: `${s.mdns.name}.local` });
  return lines;
}

function StatusLines({ v }: { v: FrontDoorView | undefined }) {
  const lines = linesOf(v);
  return (
    <div className="fd-live">
      <div className="fd-live-head">
        <span className="fd-key">Live</span>
        <span className="fd-when">{v?.status_at ? <>reported <RelTime ts={v.status_at} /></> : v && !v.connected ? "runner offline" : " "}</span>
      </div>
      <ul className="fd-lines" aria-label="How the front door is doing">
        {lines.map((l) => (
          <li key={l.name} className="fd-line" data-state={l.state} data-two={l.muted ? undefined : ""}>
            <i className="fd-dot" data-state={l.state} aria-hidden="true" />
            <span className="fd-name">{l.name}</span>
            <span className="fd-text" title={l.text}>
              {l.text}
            </span>
            {!l.muted &&
              (l.fix ? (
                <span className="fd-detail fd-fixline">
                  <code title={l.fix.commands.join("\n")}>$ {l.fix.commands[0]}</code>
                  <CopyIcon text={l.fix.commands.join("\n")} label={`Copy the fix for ${l.name}`} />
                </span>
              ) : (
                <span className="fd-detail" title={l.detail}>
                  {l.detail || " "}
                </span>
              ))}
          </li>
        ))}
      </ul>
    </div>
  );
}

function CopyIcon({ text, label }: { text: string; label: string }) {
  const [done, setDone] = React.useState(false);
  React.useEffect(() => {
    if (!done) return;
    const t = window.setTimeout(() => setDone(false), 1500);
    return () => window.clearTimeout(t);
  }, [done]);
  return (
    <button type="button" className="fd-copy" aria-label={done ? "Copied" : label} title={done ? "Copied" : label} onClick={() => void copyText(text).then(setDone)}>
      {done ? <Check size={13} aria-hidden="true" /> : <Copy size={13} aria-hidden="true" />}
    </button>
  );
}

/* ------------------------------------------------------------------ the form */

type GroupProps = { d: Draft; set: (p: Partial<Draft>) => void };

function Field({ label, hint, children, wide }: { label: string; hint?: React.ReactNode; children: (id: string) => React.ReactNode; wide?: boolean }) {
  const id = React.useId();
  return (
    <div className={cn("fd-field", wide && "fd-wide")}>
      <label className="fd-key" htmlFor={id}>
        {label}
      </label>
      {children(id)}
      {hint && <p className="fd-hint">{hint}</p>}
    </div>
  );
}

function Toggle({ checked, onChange, label, children }: { checked: boolean; onChange: (v: boolean) => void; label: string; children: React.ReactNode }) {
  return (
    <label className="fd-toggle">
      <Switch.Root
        checked={checked}
        onCheckedChange={onChange}
        aria-label={label}
        className="relative h-[18px] w-8 shrink-0 rounded-full border border-white/15 bg-white/[0.04] transition-colors outline-none focus-visible:ring-2 focus-visible:ring-white/80 focus-visible:ring-offset-2 focus-visible:ring-offset-[#08090a] data-[state=checked]:border-[#8f87b3]/70 data-[state=checked]:bg-[#8f87b3]/25"
      >
        <Switch.Thumb className="block size-3 translate-x-[2px] rounded-full bg-[#a1a4ad] transition-transform duration-200 data-[state=checked]:translate-x-[15px] data-[state=checked]:bg-[#e6e1f6]" />
      </Switch.Root>
      <span>{children}</span>
    </label>
  );
}

function NamesGroup({ d, set }: GroupProps) {
  return (
    <fieldset className="fd-group">
      <legend className="fd-key">Names</legend>
      <div className="fd-grid">
        <Field
          label="Domain"
          hint={
            <>
              Any name you choose. Every name under it answers too (<code>*.{d.domain || "kiln.dev"}</code>).
            </>
          }
        >
          {(id) => (
            <input id={id} className="fd-input" value={d.domain} placeholder="kiln.dev" autoCapitalize="off" autoCorrect="off" spellCheck={false} onChange={(e) => set({ domain: e.target.value })} />
          )}
        </Field>
        <Field label="Aliases" hint="More names for the same door, separated by commas.">
          {(id) => <input id={id} className="fd-input" value={d.aliases} placeholder="ci.home" autoCapitalize="off" autoCorrect="off" spellCheck={false} onChange={(e) => set({ aliases: e.target.value })} />}
        </Field>
      </div>
      {/* A .local name: an extra for networks whose DNS you can't set, quieter than the names. */}
      <div className="fd-extra">
        <Toggle checked={d.mdnsEnabled} onChange={(mdnsEnabled) => set({ mdnsEnabled })} label="Also answer a .local name">
          Also answer <code>{d.mdnsName || "kiln"}.local</code> on this network (multicast DNS, no setup)
        </Toggle>
        {d.mdnsEnabled && (
          <Field label=".local name">
            {(id) => (
              <span className="fd-suffix">
                <input id={id} className="fd-input" value={d.mdnsName} placeholder="kiln" autoCapitalize="off" spellCheck={false} onChange={(e) => set({ mdnsName: e.target.value })} />
                <span aria-hidden="true">.local</span>
              </span>
            )}
          </Field>
        )}
      </div>
    </fieldset>
  );
}

function DnsGroup({ d, set, v }: GroupProps & { v: FrontDoorView }) {
  const st = v.status;
  const lan = st?.addresses.find((a) => !a.includes(":")) ?? st?.addresses[0];
  const system = st?.dns.to.length ? st.dns.to.join(", ") : null;
  return (
    <fieldset className="fd-group">
      <legend className="fd-key">DNS</legend>
      <Toggle checked={d.dnsEnabled} onChange={(dnsEnabled) => set({ dnsEnabled })} label="Answer DNS">
        Answer DNS for these names
      </Toggle>
      {d.dnsEnabled && (
        <>
          <div className="fd-grid">
            <Field label="Mode">
              {(id) => (
                <select id={id} className="fd-select" value={d.dnsMode} onChange={(e) => set({ dnsMode: e.target.value as DnsMode })}>
                  <option value="resolver">Resolver: answers everything, forwards the rest</option>
                  <option value="authoritative">Authoritative: only these names, refuses the rest</option>
                </select>
              )}
            </Field>
            <Field label="Port">
              {(id) => <input id={id} className="fd-input fd-port" inputMode="numeric" value={d.dnsPort} onChange={(e) => set({ dnsPort: e.target.value })} />}
            </Field>
            {d.dnsMode === "resolver" && (
              <Field label="Upstream resolvers" hint="Empty: the runner's own resolvers.">
                {(id) => (
                  <input id={id} className="fd-input" value={d.upstream} placeholder={system ?? "the system's"} autoCapitalize="off" spellCheck={false} onChange={(e) => set({ upstream: e.target.value })} />
                )}
              </Field>
            )}
            <Field label="Answer with" hint="Empty: the runner's address on the network that asks.">
              {(id) => <input id={id} className="fd-input" value={d.addresses} placeholder={lan ?? "192.168.1.77"} autoCapitalize="off" spellCheck={false} onChange={(e) => set({ addresses: e.target.value })} />}
            </Field>
          </div>
          <p className="fd-hint">
            {d.dnsMode === "resolver" ? (
              <>
                Devices use it once their DNS server is <code>{lan ?? "this runner"}</code>: set that in your router&apos;s DHCP settings for the whole network, or on one device.
              </>
            ) : (
              <>
                Have your router forward just <code>{d.domain || "kiln.dev"}</code> to <code>{lan ?? "this runner"}</code> (conditional forwarding); it keeps answering the rest.
              </>
            )}
          </p>
        </>
      )}
    </fieldset>
  );
}

/** Read chosen files into the certificate and key fields, by what each holds. */
async function readPems(files: FileList | null): Promise<{ cert?: string; key?: string }> {
  const out: { cert?: string; key?: string } = {};
  for (const f of Array.from(files ?? []).slice(0, 4)) {
    if (f.size > 256 * 1024) continue;
    const text = await f.text();
    if (/-----BEGIN [A-Z ]*PRIVATE KEY-----/.test(text)) out.key = text;
    if (/-----BEGIN CERTIFICATE-----/.test(text)) out.cert = text.replace(/-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----\s*/g, "");
  }
  return out;
}

function HttpsGroup({ d, set, v, onView }: GroupProps & { v: FrontDoorView; onView: (v: FrontDoorView) => void }) {
  const st = v.status;
  return (
    <fieldset className="fd-group">
      <legend className="fd-key">HTTPS</legend>
      <Toggle checked={d.httpsEnabled} onChange={(httpsEnabled) => set({ httpsEnabled })} label="Serve HTTPS">
        Serve HTTPS in front of kiln
      </Toggle>
      {d.httpsEnabled && (
        <>
          <div className="fd-grid">
            <Field label="Port">
              {(id) => <input id={id} className="fd-input fd-port" inputMode="numeric" value={d.httpsPort} onChange={(e) => set({ httpsPort: e.target.value })} />}
            </Field>
            <Field label="HTTP redirect port" hint="0: no plain-HTTP redirect.">
              {(id) => <input id={id} className="fd-input fd-port" inputMode="numeric" value={d.httpPort} onChange={(e) => set({ httpPort: e.target.value })} />}
            </Field>
            <Field label="Forward to" hint="Empty: the runner's own server URL." wide>
              {(id) => (
                <input
                  id={id}
                  className="fd-input"
                  value={d.httpsUpstream}
                  placeholder={st?.server || "http://192.168.1.77:7070"}
                  autoCapitalize="off"
                  spellCheck={false}
                  onChange={(e) => set({ httpsUpstream: e.target.value })}
                />
              )}
            </Field>
          </div>
          <CertPart d={d} set={set} v={v} onView={onView} />
        </>
      )}
    </fieldset>
  );
}

/* ------------------------------------------------------------------ the certificate */

/** Top-level domains every browser treats as HTTPS-only (the HSTS preload list): nobody can click
 * through a certificate warning under them, so the certificate must be trusted first. */
const PRELOADED = new Set("android app bank boo chrome dad day dev esq foo gle gmail google hangout ing insurance meet meme mov new nexus page phd play prof rsvp search youtube zip".split(" "));
const tldOf = (name: string) => name.trim().toLowerCase().replace(/\.$/, "").split(".").pop() ?? "";

/** The file a certificate downloads as (the server names it the same way): its first name. */
function pemName(info: CertInfo, runner: string): string {
  const first = info.names.find((n) => !/^[\d.]+$/.test(n) && !n.includes(":"))?.replace(/^\*\./, "") ?? "";
  return `${first.replace(/[^a-z0-9.-]/gi, "") || runner}.pem`;
}

/** A SHA-256 fingerprint as two lines of 16 bytes, the way devices show it. */
function Fingerprint({ sha256 }: { sha256: string }) {
  const bytes = sha256.toUpperCase().match(/.{2}/g) ?? [];
  return (
    <span className="fd-fp">
      <span>{bytes.slice(0, 16).join(":")}</span>
      <span>{bytes.slice(16).join(":")}</span>
    </span>
  );
}

/** The certificate installed on the runner now (what it reports), the same four rows whether
 * there is one or not, so nothing moves when it arrives. */
function CertFacts({ cert, runner }: { cert: FrontDoorStatus["cert"] | null; runner: string }) {
  const id = React.useId();
  const info = cert?.info ?? null;
  const left = info ? daysLeft(info.not_after) : 0;
  return (
    <div className="fd-facts">
      <span id={`${id}-in-use`} className="fd-key">
        In use on {runner}
      </span>
      <dl aria-labelledby={`${id}-in-use`}>
        <div className="fd-fact">
          <dt className="fd-key">Names</dt>
          <dd>{info ? info.names.join(", ") || "—" : <span className="fd-none">none installed yet</span>}</dd>
        </div>
        <div className="fd-fact">
          <dt className="fd-key">Valid</dt>
          <dd>
            {info ? (
              <>
                {date(info.not_before)} → {date(info.not_after)}
                <span className="fd-none"> · {left < 1 ? (info.not_after * 1000 < Date.now() ? "expired" : "ends today") : `${days(left)} left`}</span>
              </>
            ) : (
              <span className="fd-none">—</span>
            )}
          </dd>
        </div>
        <div className="fd-fact">
          <dt className="fd-key">Issuer</dt>
          <dd>{info ? cert?.kiln ? `${info.issuer || "kiln CA"}, kiln's own root` : info.issuer || "—" : <span className="fd-none">—</span>}</dd>
        </div>
        <div className="fd-fact">
          <dt className="fd-key">SHA-256</dt>
          <dd>
            {info ? (
              <Fingerprint sha256={info.sha256} />
            ) : (
              <span className="fd-fp fd-none">
                <span>—</span>
                <span> </span>
              </span>
            )}
          </dd>
        </div>
      </dl>
    </div>
  );
}

/** HTTPS's certificate, managed where it is shown: which kind (self-signed or your own), the one
 * installed now, and what can be done with it in place: renew, download, upload, trust. */
function CertPart({ d, set, v, onView }: GroupProps & { v: FrontDoorView; onView: (v: FrontDoorView) => void }) {
  const cert = v.status?.cert ?? null;
  const info = cert?.info ?? null;
  const self = d.certMode === "kiln";
  const online = v.connected && v.supported !== false;
  const savedSelf = v.settings.https.enabled && v.settings.cert.mode === "kiln";
  const tld = PRELOADED.has(tldOf(d.domain)) ? tldOf(d.domain) : null;
  const ownInstalled = cert?.mode === "custom" && Boolean(info);
  const [uploading, setUploading] = React.useState(false);

  // Renew now: asked, then done once the runner serves a certificate with another fingerprint.
  const [renew, setRenew] = React.useState<{ from: string; asking?: boolean; error?: string } | null>(null);
  const renewed = renew && !renew.asking && !renew.error && info && info.sha256 !== renew.from ? info : null;
  const renewing = !renewed && (Boolean(cert?.renewing) || Boolean(renew && !renew.error));
  const startRenew = async () => {
    const from = info?.sha256 ?? "";
    setRenew({ from, asking: true });
    try {
      onView(await frontDoorApi.renewCert(v.runner));
      setRenew({ from });
    } catch (e) {
      setRenew({ from, error: errorMessage(e) });
    }
  };
  const note: { tone: "ok" | "error" | "quiet"; text: string } | null = renew?.error
    ? { tone: "error", text: renew.error }
    : renewed
      ? { tone: "ok", text: `Renewed: a new key and certificate, until ${date(renewed.not_after)}.` }
      : renewing
        ? { tone: "quiet", text: `Renewing: ${v.runner} makes a new key and kiln's root signs it.` }
        : self && !savedSelf
          ? { tone: "quiet", text: "Save first; then it can be renewed here." }
          : !v.connected
            ? { tone: "quiet", text: `${v.runner} is offline.` }
            : !online
              ? { tone: "quiet", text: `${v.runner} runs a kiln too old to renew it.` }
              : null;

  return (
    <div className="fd-cert">
      <Field
        label="Certificate"
        hint={
          self ? (
            <>
              kiln makes it from its own root, installs it on {v.runner} and renews it before it ends. Trust that root once on a device and the device trusts every runner&apos;s certificate, renewals
              too.
            </>
          ) : (
            <>A certificate for these names and its key, from your own CA or a public one. The key goes to {v.runner} once and stays there; kiln never shows it again.</>
          )
        }
      >
        {(id) => (
          <select id={id} className="fd-select" value={d.certMode} onChange={(e) => set({ certMode: e.target.value as CertMode })}>
            <option value="kiln">Self-signed: kiln makes and renews it</option>
            <option value="custom">Your own: upload it with its key</option>
          </select>
        )}
      </Field>
      <CertFacts cert={cert} runner={v.runner} />
      <div className="fd-actions">
        {self && (
          <Button
            type="button"
            size="xs"
            variant="secondary"
            className="fd-btn"
            disabled={!savedSelf || !online || renewing}
            title="A new key and certificate now; the one in use serves until it is in"
            onClick={() => void startRenew()}
          >
            {renewing ? "Renewing…" : "Renew now"}
          </Button>
        )}
        {info ? (
          <a className="fd-download" href={frontDoorApi.certUrl(v.runner)} download title="The certificate chain it serves (PEM), never its key">
            <Download size={13} aria-hidden="true" /> {pemName(info, v.runner)}
          </a>
        ) : (
          <span className="fd-download" aria-disabled="true">
            <Download size={13} aria-hidden="true" /> Download
          </span>
        )}
        {!self && ownInstalled && !uploading && (
          <Button type="button" size="xs" variant="secondary" className="fd-btn" onClick={() => setUploading(true)}>
            Upload a new one
          </Button>
        )}
        {self && note && (
          <span className="fd-msg" role="status" data-tone={note.tone}>
            {note.text}
          </span>
        )}
      </div>
      {!self && (uploading || !ownInstalled) && <Upload v={v} onView={onView} onDone={() => setUploading(false)} />}
      {self && tld && (
        <p className="fd-hint">
          Browsers never let anyone click through a certificate warning on <code>.{tld}</code> (it is HSTS-preloaded): trust kiln&apos;s root on each device first.
        </p>
      )}
      {self && <TrustDevices v={v} />}
    </div>
  );
}

/** Your own certificate: its chain and key, pasted or read from files, sent to the runner. */
function Upload({ v, onView, onDone }: { v: FrontDoorView; onView: (v: FrontDoorView) => void; onDone: () => void }) {
  const [certPem, setCertPem] = React.useState("");
  const [keyPem, setKeyPem] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const [note, setNote] = React.useState<{ tone: "ok" | "error"; text: string } | null>(null);
  const fileId = React.useId();
  const upload = async () => {
    setBusy(true);
    setNote(null);
    try {
      const r = await frontDoorApi.uploadCert(v.runner, certPem, keyPem);
      onView(r.front_door);
      setCertPem("");
      setKeyPem("");
      setNote({ tone: "ok", text: `Sent to ${v.runner}: ${r.cert.names.join(", ")}, until ${date(r.cert.not_after)}. The key stays on the runner.` });
      onDone();
    } catch (e) {
      setNote({ tone: "error", text: errorMessage(e) });
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="fd-custom">
      <div className="fd-grid">
        <Field label="Certificate chain (PEM)" wide>
          {(id) => <textarea id={id} className="fd-input fd-area" value={certPem} placeholder="-----BEGIN CERTIFICATE-----" spellCheck={false} onChange={(e) => setCertPem(e.target.value)} />}
        </Field>
        <Field label="Private key (PEM)" hint="Sent to the runner once and kept there (mode 0600); never shown again." wide>
          {(id) => (
            <textarea id={id} className="fd-input fd-area" value={keyPem} placeholder="-----BEGIN PRIVATE KEY-----" spellCheck={false} autoComplete="off" onChange={(e) => setKeyPem(e.target.value)} />
          )}
        </Field>
      </div>
      <div className="fd-actions">
        <label className="fd-file" htmlFor={fileId}>
          Choose files…
        </label>
        <input
          id={fileId}
          className="fd-sr"
          type="file"
          multiple
          accept=".pem,.crt,.cer,.key,.txt"
          onChange={(e) => {
            void readPems(e.target.files).then((r) => {
              if (r.cert) setCertPem(r.cert);
              if (r.key) setKeyPem(r.key);
            });
            e.target.value = "";
          }}
        />
        <Button type="button" size="xs" variant="secondary" className="fd-btn" disabled={busy || !certPem.trim() || !keyPem.trim()} onClick={() => void upload()}>
          {busy ? "Uploading…" : "Upload certificate"}
        </Button>
        <span className="fd-msg" role="status" data-tone={note?.tone ?? "quiet"}>
          {note?.text ?? (v.connected ? "" : `${v.runner} must be online to take it.`)}
        </span>
      </div>
    </div>
  );
}

/* ------------------------------------------------------------------ devices and setup */

const DEVICES: { os: string; steps: React.ReactNode }[] = [
  {
    os: "iPhone, iPad",
    steps: (
      <ol className="fd-steps">
        <li>
          Open <a href="/ca.mobileconfig">ca.mobileconfig</a> in Safari and allow it, then install the profile: Settings → General → VPN &amp; Device Management → kiln CA → Install.
        </li>
        <li>
          Settings → General → About → Certificate Trust Settings: turn on <b>kiln CA</b> (Enable full trust for root certificates).
        </li>
      </ol>
    ),
  },
  {
    os: "Android",
    steps: (
      <>
        Download <a href="/ca.crt">kiln-ca.crt</a>, then Settings → Security &amp; privacy → More security settings → Encryption &amp; credentials → Install a certificate → CA
        certificate.
      </>
    ),
  },
  {
    os: "macOS",
    steps: (
      <>
        Open <a href="/ca.mobileconfig">ca.mobileconfig</a>, then System Settings → Privacy &amp; Security → Profiles → kiln CA → Install (or{" "}
        <code>sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain kiln-ca.pem</code>).
      </>
    ),
  },
  {
    os: "Windows",
    steps: (
      <>
        Open <a href="/ca.crt">kiln-ca.crt</a> → Install Certificate → Local Machine → Trusted Root Certification Authorities (or, as administrator,{" "}
        <code>certutil -addstore -f Root kiln-ca.crt</code>).
      </>
    ),
  },
  {
    os: "Linux",
    steps: (
      <>
        <code>sudo trust anchor --store kiln-ca.pem</code> (Arch, Fedora), or copy it to <code>/usr/local/share/ca-certificates/kiln-ca.crt</code> and run{" "}
        <code>sudo update-ca-certificates</code> (Debian, Ubuntu). Firefox keeps its own list: Settings → Certificates → Import.
      </>
    ),
  },
];

/** How a device trusts kiln's root (and so every self-signed certificate kiln makes): a quiet
 * disclosure inside the certificate part, closed until asked for. */
function TrustDevices({ v }: { v: FrontDoorView }) {
  const [open, setOpen] = React.useState(false);
  const id = React.useId();
  return (
    <div className="fd-trust">
      <button type="button" className="fd-disclose" aria-expanded={open} aria-controls={id} onClick={() => setOpen(!open)}>
        {open ? <Minus size={13} aria-hidden="true" /> : <Plus size={13} aria-hidden="true" />}
        Trust it on your devices
      </button>
      {open && (
        <div id={id} className="fd-trust-body">
          <p className="fd-hint">
            Once on each device: trust kiln&apos;s root, and the device trusts every certificate it signs, on every runner, renewals included. On the device, check that it is this root:
          </p>
          {v.ca && (
            <p className="fd-root">
              <span className="fd-key">SHA-256</span>
              <Fingerprint sha256={v.ca.sha256} />
            </p>
          )}
          <div className="fd-downloads">
            <a className="fd-download" href="/ca.mobileconfig" download>
              <Download size={13} aria-hidden="true" /> ca.mobileconfig
            </a>
            <a className="fd-download" href="/ca.crt" download>
              <Download size={13} aria-hidden="true" /> kiln-ca.crt
            </a>
            <a className="fd-download" href="/ca.pem" download>
              <Download size={13} aria-hidden="true" /> kiln-ca.pem
            </a>
          </div>
          <dl className="fd-devices">
            {DEVICES.map((x) => (
              <div key={x.os} className="fd-device">
                <dt className="fd-key">{x.os}</dt>
                <dd>{x.steps}</dd>
              </div>
            ))}
          </dl>
        </div>
      )}
    </div>
  );
}

function Setup({ runner, fixes }: { runner: string; fixes: FrontDoorFix[] }) {
  return (
    <div className="fd-setup">
      <h3 className="fd-key">One-time setup on {runner}</h3>
      {fixes.map((f) => (
        <div key={f.commands.join("\n")} className="fd-fix">
          <p className="fd-hint">{f.why}</p>
          {f.commands.map((c) => (
            <div key={c} className="fd-code">
              <code>
                <span className="fd-prompt" aria-hidden="true">
                  ${" "}
                </span>
                {c}
              </code>
              <CopyIcon text={c} label="Copy the command" />
            </div>
          ))}
        </div>
      ))}
    </div>
  );
}

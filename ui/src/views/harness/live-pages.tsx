/**
 * Live pages you open yourself, in an agent's Tools tab (docs/previews.md, "Opening a page"): what
 * its machine is serving (its agent's servers first), the project's app (its `.kiln/preview.yaml`:
 * started for you), or any address, streamed live from the agent's machine in the browser profile
 * you pick. The ones you open are here, live; the agent's own are in the conversation. A port of
 * its machine also opens on the person's own device, in its own browser (`crate::harness::opened`).
 */
import * as React from "react";
import { ChevronDown } from "lucide-react";
import { harnessApi, type Listening, type Opened, type PreviewLook, type StreamView } from "@/api/harness";
import { Button } from "@/components/ui/button";
import { Loadable } from "@/components/ui/loading";
import { SelectField } from "@/components/ui/misc";
import { errorMessage } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";
import { Live } from "./live";
import type { LiveStream } from "./live-of";
import "./live.css";

type Profile = "conversation" | "clean" | "personal";

const PROFILES: { value: Profile; label: string }[] = [
  { value: "conversation", label: "This conversation's" },
  { value: "clean", label: "Clean" },
  { value: "personal", label: "Your own" },
];

const PROFILE_SAYS: Record<Profile, string> = {
  conversation: "Its cookies and sign-ins are this conversation's, shared with the pages its agent shows, and removed when it closes.",
  clean: "A new profile each time: nothing from before, nothing kept after.",
  personal: "Yours alone, kept on the agent's machine across conversations, so a site you sign in to stays signed in. Agents running on that machine can read its files.",
};

/** What it may open, or why it can't (a session on the server itself, a runner too old). */
type Look = { look: PreviewLook; error?: undefined } | { look?: undefined; error: string };

export function LivePages({ session }: { session: string }) {
  return (
    <Loadable label="what it can open live">
      <Pages session={session} />
    </Loadable>
  );
}

/** A port's address as its machine's browser reaches it: loopback and every-address ones at localhost. */
function addressOf(p: Listening): string {
  const local = ["0.0.0.0", "::", "127.0.0.1", "::1", "*", ""].includes(p.address);
  const host = local ? "localhost" : p.address.includes(":") ? `[${p.address}]` : p.address;
  return `http://${host}:${p.port}/`;
}

/** A command, in a word or two: its program, and what an interpreter runs (`node vite`,
 *  `python3 http.server`). */
function programOf(command: string): string {
  const words = command.split(/\s+/).filter(Boolean);
  const base = (w: string) => w.split(/[\\/]/).pop() ?? w;
  const program = base(words[0] ?? "");
  if (!/^(node|nodejs|bun|deno|python[0-9.]*|ruby|php|perl|java)$/.test(program)) return program || "unknown";
  for (let i = 1; i < words.length; i++) {
    const w = words[i] ?? "";
    if (w === "-m" || w === "-jar") return `${program} ${base(words[i + 1] ?? "")}`.trim();
    // An option's own argument isn't what it runs.
    if (["--require", "-r", "--import", "--loader", "-c", "-e", "--eval", "-cp", "--config"].includes(w)) i++;
    else if (!w.startsWith("-")) return `${program} ${base(w)}`;
  }
  return program;
}

/** A typed address, as one: a scheme when it has none (`localhost:3000` is http). */
function urlOf(typed: string): string {
  const t = typed.trim();
  return /^[a-z][a-z0-9+.-]*:\/\//i.test(t) ? t : `http://${t}`;
}

/** A typed address on the agent's machine itself (`localhost:3000/x`): its port and page. */
function localOf(typed: string): { port: number; path: string } | undefined {
  try {
    const u = new URL(urlOf(typed));
    if (!["localhost", "127.0.0.1", "[::1]"].includes(u.hostname) || !u.port) return undefined;
    return { port: Number(u.port), path: `${u.pathname}${u.search}` };
  } catch {
    return undefined;
  }
}

/** What the server says of an open stream, as the live view shows it. */
function streamOf(v: StreamView): LiveStream {
  const url = v.said.navigated?.url ?? v.spec.url ?? "";
  const profile = v.spec.profile === "clean" ? "clean" : v.spec.profile?.startsWith("personal:") ? "personal" : undefined;
  return { id: v.id, session: v.session, url, title: v.said.loaded?.title || v.spec.title || url || "A live page", profile, engine: v.spec.engine, device: v.spec.device };
}

function Pages({ session }: { session: string }) {
  // Its servers come and go as its agent works: read again while this is shown.
  const look = useSuspenseResource<Look>(
    `harness/preview/${session}`,
    () =>
      harnessApi.preview(session).then(
        (look) => ({ look }),
        (e: unknown) => ({ error: errorMessage(e) }),
      ),
    { pollMs: 10_000 },
  );
  const listed = useSuspenseResource(`harness/streams/${session}`, () => harnessApi.streams(session).catch((): StreamView[] => []), { pollMs: 15_000 });
  const openings = useSuspenseResource(`harness/opened/${session}`, () => harnessApi.opened(session).catch((): Opened[] => []), { pollMs: 30_000 });
  const [deviceBusy, setDeviceBusy] = React.useState(false);
  const [note, setNote] = React.useState<string | null>(null);
  const [pick, setPick] = React.useState("");
  const [address, setAddress] = React.useState("");
  const [profile, setProfile] = React.useState<Profile>("conversation");
  const [engine, setEngine] = React.useState("chromium");
  const [layout, setLayout] = React.useState("");
  const [busy, setBusy] = React.useState<"" | "open" | "start" | "stop">("");
  const [error, setError] = React.useState<string | null>(null);
  // Opened here: shown until closed, ended or not (what it said when it ended stays to read).
  const [here, setHere] = React.useState<StreamView[]>([]);
  const [closed, setClosed] = React.useState<string[]>([]);
  // One just opened is brought into view (it goes live once it's seen).
  const [opened, setOpened] = React.useState<string | null>(null);
  const into = React.useCallback((el: HTMLDivElement | null) => el?.scrollIntoView({ block: "nearest", behavior: "smooth" }), []);

  if (!look.data.look) return <p className="in-none">{look.data.error}</p>;
  const { ports, app } = look.data.look;
  const appPort = app.running ? app.port : undefined;
  const startable = app.configured && !app.running && !app.error;
  const option = (p: Listening) => ({ value: `port:${p.port}`, label: `localhost:${p.port} · ${p.port === appPort ? "the app" : programOf(p.command)}` });
  // The agent's servers (and the app) first; the rest of what its machine serves after.
  const theirs = ports.filter((p) => p.agents || p.port === appPort).map(option);
  const others = ports.filter((p) => !p.agents && p.port !== appPort).map(option);
  const values = [...(startable ? ["app"] : []), ...theirs.map((o) => o.value), ...others.map((o) => o.value), "address"];
  const chosen = values.includes(pick) ? pick : (values[0] ?? "address");
  const port = ports.find((p) => `port:${p.port}` === chosen);
  // The engines its machine has, and the devices a page can be laid out as.
  const has = look.data.look.engines ?? { chromium: true };
  const engines = [
    { value: "chromium", label: "Chromium" },
    ...(has.firefox ? [{ value: "firefox", label: "Firefox" }] : []),
    ...(has.webkit ? [{ value: "webkit", label: "WebKit (Safari)" }] : []),
  ];
  const devices = [{ value: "", label: "This screen" }, ...(look.data.look.devices ?? []).map((d) => ({ value: d.name, label: `${d.name} · ${d.width}×${d.height}` }))];
  const runIn = engines.some((e) => e.value === engine) ? engine : "chromium";
  // What WebKit is there: an iPhone or iPad a Mac's iOS Simulator (the real Mobile Safari), else
  // Safari's own window on that Mac, or WebKitGTK elsewhere.
  const mac = look.data.look.platform === "macos";
  // A profile of your own is yours alone: signed in as yourself (the server says when not).
  const personal = look.data.look.personal ?? { ok: true };
  const profiles = PROFILES.map((p) => (p.value === "personal" && !personal.ok ? { ...p, label: "Your own (sign in as yourself)", disabled: true } : p));
  const profileNow: Profile = profile === "personal" && !personal.ok ? "conversation" : profile;
  const ios = (look.data.look.devices ?? []).some((d) => d.name === layout && d.mobile && /iPhone|iPad/.test(d.agent ?? ""));
  const webkitSays =
    runIn !== "webkit"
      ? ""
      : mac && ios
        ? has.ios
          ? "It runs in that Mac's iOS Simulator: the real Mobile Safari, with no window on its screen."
          : "An iPhone or iPad runs in that Mac's iOS Simulator, which needs a runtime first: install one there with xcodebuild -downloadPlatform iOS."
        : mac
          ? "WebKit is Safari on that Mac: its window opens on its screen, and keeps nothing."
          : "WebKit is WebKitGTK on that machine.";
  // What opens on the person's own device: a port of its machine (the app's once it runs).
  const device = port ? { port: port.port, path: chosen === `port:${appPort}` ? (app.path ?? "/") : "/" } : chosen === "address" ? localOf(address) : undefined;
  // Yours: the ones opened here, then any you opened elsewhere (the agent's are in the conversation).
  const yours = [...here, ...listed.data.filter((v) => !v.by.startsWith("agent:") && !here.some((h) => h.id === v.id))].filter((v) => !closed.includes(v.id));

  const open = async () => {
    const starting = chosen === "app";
    const url = port ? addressOf(port) : urlOf(address);
    if (!starting && !port && !address.trim()) {
      setError("Which address? Type one, like localhost:3000.");
      return;
    }
    setBusy(starting ? "start" : "open");
    setError(null);
    try {
      const v = await harnessApi.openStream(session, { ...(starting ? { start: true } : { url }), profile: profileNow, engine: runIn === "chromium" ? undefined : runIn, device: layout || undefined, title: starting ? "The app" : port ? `localhost:${port.port}` : undefined });
      setHere((h) => [v, ...h]);
      setOpened(v.id);
      listed.setData((l) => [v, ...l.filter((x) => x.id !== v.id)]);
      if (starting) look.reload();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy("");
    }
  };
  // On the person's own device: a link made by kiln, followed in a window opened now (in the tap's
  // own gesture, which a phone wants for one) or copied for another device.
  const onDevice = async (how: "here" | "copy") => {
    if (!device) return;
    const w = how === "here" ? window.open("", "_blank") : null;
    let made: (url: string) => void = () => undefined;
    const link = new Promise<string>((r) => (made = r));
    if (how === "copy") {
      const later = () => void link.then((u) => (u ? navigator.clipboard?.writeText(u) : undefined)).catch(() => undefined);
      try {
        navigator.clipboard.write([new ClipboardItem({ "text/plain": link.then((u) => (u ? new Blob([u], { type: "text/plain" }) : Promise.reject(new Error("no link")))) })]).catch(later);
      } catch {
        later();
      }
    }
    setDeviceBusy(true);
    setError(null);
    setNote(null);
    try {
      const o = await harnessApi.openOnDevice(session, device);
      openings.setData((l) => [o, ...l.filter((x) => x.id !== o.id)]);
      made(o.url ?? "");
      if (w && o.url) {
        w.opener = null;
        w.location.href = o.url;
      }
      if (how === "copy") setNote("Link copied. Open it on your other device within ten minutes: it works once.");
    } catch (e) {
      made("");
      w?.close();
      setError(errorMessage(e));
    } finally {
      setDeviceBusy(false);
    }
  };
  const endOpened = (id: string) => {
    openings.setData((l) => l.filter((o) => o.id !== id));
    void harnessApi.endOpened(session, id).catch(() => undefined);
  };
  const close = (id: string) => {
    setClosed((c) => [...c, id]);
    setHere((h) => h.filter((v) => v.id !== id));
    listed.setData((l) => l.filter((v) => v.id !== id));
    // Ended already, it's gone there too.
    void harnessApi.closeStream(session, id).catch(() => undefined);
  };
  const stopApp = async () => {
    setBusy("stop");
    setError(null);
    try {
      await harnessApi.stopApp(session);
      look.reload();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy("");
    }
  };

  return (
    <div className="lp">
      <form
        className="lp-open"
        onSubmit={(e) => {
          e.preventDefault();
          void open();
        }}
      >
        <div className="lp-what">
          <label className="field-label" htmlFor={`lp-page-${session}`}>
            Page
          </label>
          <div className="select">
            <select id={`lp-page-${session}`} value={chosen} onChange={(e) => setPick(e.target.value)}>
              {startable && <option value="app">Start the app: {app.start}</option>}
              {theirs.length > 0 && (
                <optgroup label="The agent's">
                  {theirs.map((o) => (
                    <option key={o.value} value={o.value}>
                      {o.label}
                    </option>
                  ))}
                </optgroup>
              )}
              {others.length > 0 && (
                <optgroup label="Also on its machine">
                  {others.map((o) => (
                    <option key={o.value} value={o.value}>
                      {o.label}
                    </option>
                  ))}
                </optgroup>
              )}
              <option value="address">Another address</option>
            </select>
            <ChevronDown size={14} aria-hidden="true" />
          </div>
        </div>
        {chosen === "address" && (
          <div className="lp-what">
            <label className="field-label" htmlFor={`lp-address-${session}`}>
              Address
            </label>
            <div className="field">
              <input id={`lp-address-${session}`} value={address} placeholder="localhost:3000" spellCheck={false} autoCapitalize="off" autoCorrect="off" inputMode="url" onChange={(e) => setAddress(e.target.value)} />
            </div>
          </div>
        )}
        <SelectField label="Engine" value={runIn} onChange={setEngine} options={engines} className="lp-half" />
        <SelectField label="Device" value={layout} onChange={setLayout} options={devices} className="lp-half" />
        <SelectField label="Browser profile" value={profileNow} onChange={(v) => setProfile(v as Profile)} options={profiles} className="lp-profile" />
        <Button size="sm" type="submit" className="lp-go" disabled={busy !== ""}>
          {busy === "start" ? "Starting the app" : busy === "open" ? "Opening" : chosen === "app" ? "Start and open" : "Open"}
        </Button>
      </form>
      <p className="field-hint lp-says">
        {PROFILE_SAYS[profileNow]} {webkitSays}
        {runIn !== "chromium" ? " It streams without sound." : ""}
      </p>
      {!personal.ok && personal.why && <p className="field-hint lp-says">{personal.why}</p>}
      <div className="lp-device">
        <span className="lp-device-what">On your device</span>
        <Button size="sm" variant="secondary" disabled={!device || deviceBusy} onClick={() => void onDevice("here")}>
          Open on this device
        </Button>
        <Button size="sm" variant="ghost" disabled={!device || deviceBusy} onClick={() => void onDevice("copy")}>
          Copy a link
        </Button>
      </div>
      {chosen === "app" && <p className="field-hint lp-says">Start the app first; then it opens on your device too.</p>}
      {openings.data.length > 0 && (
        <ul className="lp-opened" aria-label="Open on your devices">
          {openings.data.map((o) => (
            <li key={o.id}>
              <code>
                localhost:{o.port}
                {o.path !== "/" ? o.path : ""}
              </code>
              <span>until {new Date(o.until).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}</span>
              <Button size="sm" variant="ghost" onClick={() => endOpened(o.id)}>
                End
              </Button>
            </li>
          ))}
        </ul>
      )}
      {note && <p className="field-hint lp-says">{note}</p>}
      {app.configured &&
        (app.error ? (
          <p className="field-hint lp-says">.kiln/preview.yaml: {app.error}</p>
        ) : app.running ? (
          <div className="lp-app">
            <span>
              The app is running at <code>{app.running}</code>
            </span>
            <Button size="sm" variant="ghost" disabled={busy !== ""} onClick={() => void stopApp()}>
              {busy === "stop" ? "Stopping" : "Stop the app"}
            </Button>
          </div>
        ) : null)}
      {!app.configured && ports.length === 0 && <p className="field-hint lp-says">Nothing is serving on its machine yet. A project&rsquo;s .kiln/preview.yaml says how its app starts, so it can be started from here.</p>}
      {error && (
        <p className="lp-error" role="alert">
          {error}
        </p>
      )}
      {yours.map((v) => (
        <div key={v.id} ref={v.id === opened ? into : undefined}>
          <Live stream={streamOf(v)} ev={`lp-${v.id}`} onClose={() => close(v.id)} />
        </div>
      ))}
    </div>
  );
}

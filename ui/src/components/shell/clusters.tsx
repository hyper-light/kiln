/**
 * Peered clusters in kiln's shell (docs/peers.md): a picker in the header for this cluster and
 * each one peered with it. A peer opens in a frame that fills the page and stays loaded once
 * opened (as the terminal does), so flipping back finds it where it was.
 *
 * One header at a time: the peer is kiln too, so its page in the frame says so to this one
 * (`{kiln: "framed"}`), is told the clusters (`{kiln: "clusters"}`), and shows this same picker
 * in its own header; a pick there comes back (`{kiln: "pick"}`) and this page switches. This
 * page's header steps aside while such a peer is shown. A peer that says nothing (an older kiln)
 * keeps this header above it, picker and all.
 */
import * as React from "react";
import { create } from "zustand";
import { ChevronDown } from "lucide-react";
import { ApiError } from "@/api/client";
import { peersApi, type Frame, type Openable } from "@/api/peers";
import { toast } from "@/components/ui/toast";
import { useRes, type Res } from "@/lib/data";
import { useNavigate } from "@/lib/router";
import "./clusters.css";

/** The picker's list: this cluster and each it may open (none from a server without peering). */
export const openRes: Res<Openable> = {
  key: "peers/open",
  fetch: (s) => peersApi.open(s).catch(() => ({ name: "", peers: [] })),
  opts: { on: ["peers"] },
};

interface Opened {
  id: string;
  name: string;
  frame: Frame;
  /** A new grant posted into its frame makes a new one. */
  key: number;
  /** Its address gave this browser no answer (it says why, not a blank frame). */
  unreached?: boolean;
}

/** Which cluster is shown (`null`: this one), every one opened, and those whose page speaks kiln. */
export const useClusters = create<{ shown: string | null; opened: Opened[]; framed: string[] }>(() => ({ shown: null, opened: [], framed: [] }));

/** A peer is shown whose own header has the picker: this page's header steps aside. */
export const useImmersed = () => useClusters((s) => s.shown !== null && s.framed.includes(s.shown));

/** Within this of its grant ending, a frame is opened anew with a new one. */
const FRESH_MS = 60_000;
/** How long a peer's address has to answer this browser at all. */
const REACH_MS = 8_000;

/** Whether this browser gets any answer from `origin` (the forwarder there answers anything). */
async function reaches(origin: string): Promise<boolean> {
  try {
    await fetch(`${origin}/.kiln-peer/enter`, { mode: "no-cors", cache: "no-store", signal: AbortSignal.timeout(REACH_MS) });
    return true;
  } catch {
    return false;
  }
}

const mark = (id: string, unreached: boolean) => useClusters.setState((s) => ({ opened: s.opened.map((o) => (o.id === id ? { ...o, unreached } : o)) }));

/** Show a peered cluster: opened (with a grant of its own) when it isn't, or its grant is ending. */
export async function openCluster(id: string, name: string) {
  const was = useClusters.getState().opened.find((o) => o.id === id);
  if (was && was.frame.expires_at * 1000 - Date.now() > FRESH_MS) {
    useClusters.setState({ shown: id });
    return;
  }
  try {
    const frame = await peersApi.frame(id);
    const next: Opened = { id, name, frame, key: (was?.key ?? 0) + 1 };
    useClusters.setState((s) => ({
      shown: id,
      opened: was ? s.opened.map((o) => (o.id === id ? next : o)) : [...s.opened, next],
      // A new page in its frame says again whether it speaks kiln.
      framed: s.framed.filter((f) => f !== id),
    }));
    if (!(await reaches(frame.origin))) mark(id, true);
  } catch (e) {
    toast.error(`Couldn't open ${name}`, e instanceof ApiError ? e.message : String(e));
  }
}

export const showHome = () => useClusters.setState({ shown: null });

// ------------------------------------------------------------------ this page frames peers

/** The frames' windows, by peer: whose messages count, and where to answer. */
const frames = new Map<string, { win: Window | null; origin: string }>();

/** Tell a peer's page the clusters, so its header's picker lists them (it is the one shown). */
async function tell(id: string) {
  const f = frames.get(id);
  if (!f?.win) return;
  const list = await peersApi.open().catch(() => null);
  if (!list) return;
  f.win.postMessage({ kiln: "clusters", name: list.name, peers: list.peers.map((p) => ({ id: p.id, name: p.name })), current: id }, f.origin);
}

const framing = typeof window !== "undefined" && window.parent === window;
if (framing) {
  window.addEventListener("message", (e: MessageEvent) => {
    const id = [...frames].find(([, f]) => f.win === e.source && f.origin === e.origin)?.[0];
    const d = e.data as { kiln?: unknown; id?: unknown } | null;
    if (!id || !d || typeof d.kiln !== "string") return;
    if (d.kiln === "framed") {
      useClusters.setState((s) => ({ framed: s.framed.includes(id) ? s.framed : [...s.framed, id] }));
      void tell(id);
    } else if (d.kiln === "pick") {
      const to = typeof d.id === "string" ? d.id : "";
      const o = useClusters.getState().opened.find((x) => x.id === to);
      if (!to) showHome();
      else void peersApi.open().then((l) => openCluster(to, l.peers.find((p) => p.id === to)?.name ?? o?.name ?? to));
    }
  });
}

// ------------------------------------------------------------------ this page in a peer's frame

/** The clusters of the page this one is framed in, when that page is a kiln that peers with it. */
interface Embed {
  name: string;
  peers: { id: string; name: string }[];
  current: string;
  /** Where picks go back to. */
  origin: string;
}

const useEmbed = create<{ embed: Embed | null }>(() => ({ embed: null }));

if (typeof window !== "undefined" && window.parent !== window) {
  window.addEventListener("message", (e: MessageEvent) => {
    if (e.source !== window.parent) return;
    const d = e.data as { kiln?: unknown; name?: unknown; peers?: unknown; current?: unknown } | null;
    if (!d || d.kiln !== "clusters" || !Array.isArray(d.peers)) return;
    const peers = (d.peers as { id?: unknown; name?: unknown }[]).filter((p) => typeof p?.id === "string" && typeof p?.name === "string").map((p) => ({ id: String(p.id), name: String(p.name) }));
    useEmbed.setState({ embed: { name: typeof d.name === "string" ? d.name : "", peers, current: typeof d.current === "string" ? d.current : "", origin: e.origin } });
  });
  // To whatever framed this page: a kiln that peers with this one answers with its clusters.
  window.parent.postMessage({ kiln: "framed" }, "*");
}

// ------------------------------------------------------------------ the picker

const MANAGE = "\u0000manage";

/** The header's cluster picker: the clusters of the page framing this one, else this cluster's. */
export function ClusterPicker() {
  const embed = useEmbed((s) => s.embed);
  return embed ? <FramedPicker embed={embed} /> : <OwnPicker />;
}

function Pick({ label, away, value, onPick, children }: { label: string; away: boolean; value: string; onPick: (v: string) => void; children: React.ReactNode }) {
  return (
    <label className="cluster-pick" data-away={away ? "" : undefined}>
      <span className="cluster-pick-name">{label}</span>
      <ChevronDown size={12} aria-hidden="true" />
      <select aria-label="Cluster" value={value} onChange={(e) => onPick(e.target.value)}>
        {children}
      </select>
    </label>
  );
}

/** This cluster's: nothing until it's peered with another. */
function OwnPicker() {
  const { data } = useRes(openRes);
  const shown = useClusters((s) => s.shown);
  const navigate = useNavigate();
  if (data.peers.length === 0) return null;
  const current = data.peers.find((p) => p.id === shown)?.name ?? data.name;
  return (
    <Pick
      label={current}
      away={shown !== null}
      value={shown ?? ""}
      onPick={(v) => {
        if (v === "") showHome();
        else if (v === MANAGE) {
          showHome();
          navigate("/clusters");
        } else {
          const p = data.peers.find((x) => x.id === v);
          if (p) void openCluster(p.id, p.name);
        }
      }}
    >
      <option value="">{data.name} · this cluster</option>
      {data.peers.map((p) => (
        <option key={p.id} value={p.id}>
          {p.name}
        </option>
      ))}
      <option value={MANAGE}>Manage clusters…</option>
    </Pick>
  );
}

/** In a peer's frame: the clusters of the page framing it; a pick goes back to that page. */
function FramedPicker({ embed }: { embed: Embed }) {
  const current = embed.peers.find((p) => p.id === embed.current)?.name ?? embed.name;
  return (
    <Pick label={current} away value={embed.current} onPick={(v) => window.parent.postMessage({ kiln: "pick", id: v }, embed.origin)}>
      <option value="">{embed.name}</option>
      {embed.peers.map((p) => (
        <option key={p.id} value={p.id}>
          {p.name}
        </option>
      ))}
    </Pick>
  );
}

// ------------------------------------------------------------------ the frames

/** Every opened cluster's frame; the one shown fills the page. */
export function ClusterFrames() {
  const shown = useClusters((s) => s.shown);
  const opened = useClusters((s) => s.opened);
  const immersed = useImmersed();
  return (
    <>
      {opened.map((o) => (
        <PeerFrame key={o.id} o={o} visible={shown === o.id} whole={immersed} />
      ))}
    </>
  );
}

function PeerFrame({ o, visible, whole }: { o: Opened; visible: boolean; whole: boolean }) {
  const target = `kiln-peer-${o.id}`;
  // Its window, so what its page says is known to come from it (and where to answer).
  const known = React.useCallback(
    (el: HTMLIFrameElement | null) => {
      if (!el) return;
      frames.set(o.id, { win: el.contentWindow, origin: o.frame.origin });
      return () => {
        frames.delete(o.id);
      };
    },
    [o.id, o.frame.origin],
  );
  // The grant goes in by a form posted into the frame (so it's in no address or history), and
  // again whenever the cluster is opened anew.
  const post = React.useCallback(
    (form: HTMLFormElement | null) => {
      void o.key;
      form?.submit();
    },
    [o.key],
  );
  return (
    <div className={visible ? "cluster-frame-wrap" : "hidden"} data-whole={whole ? "" : undefined} aria-hidden={!visible}>
      {o.unreached && <Unreached o={o} />}
      <iframe ref={known} name={target} title={o.name} className={o.unreached ? "hidden" : "cluster-frame"} allow="clipboard-read; clipboard-write; fullscreen" />
      <form key={o.key} ref={post} method="post" action={o.frame.action} target={target} hidden>
        <input type="hidden" name="g" value={o.frame.grant} />
        <input type="hidden" name="to" value="/" />
      </form>
    </div>
  );
}

/** A peer whose address this browser got no answer from: which address, and what lets it through. */
function Unreached({ o }: { o: Opened }) {
  const url = new URL(o.frame.origin);
  const port = url.port || (url.protocol === "https:" ? "443" : "80");
  const byPort = url.protocol === "http:" && url.port !== "";
  return (
    <div className="cluster-unreached" role="alert">
      <p className="cluster-unreached-title">This browser can't reach {o.name} at {o.frame.origin}</p>
      {byPort ? (
        <p>
          Opened at an address, a peered cluster loads on its own port there ({port}), served by the kiln server or a runner's front door on {url.hostname}. Let port {port} through that machine's firewall (with ufw: <code>sudo ufw allow {port}/tcp</code>), or open kiln at a front door's domain.
        </p>
      ) : (
        <p>The front door at {url.hostname} didn't answer: check that this device resolves that name and reaches the machine it names.</p>
      )}
      <button
        type="button"
        className="cluster-unreached-retry"
        onClick={() => {
          mark(o.id, false);
          void reaches(o.frame.origin).then((ok) => {
            if (!ok) mark(o.id, true);
            else useClusters.setState((s) => ({ opened: s.opened.map((x) => (x.id === o.id ? { ...x, key: x.key + 1 } : x)) }));
          });
        }}
      >
        Try again
      </button>
    </div>
  );
}

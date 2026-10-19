/**
 * Peered clusters in kiln's shell (docs/peers.md): a picker in the header for this cluster and
 * each one peered with it, and a band under the header with the clusters opened, to flip between
 * them. A peer opens in a frame that fills the page and stays loaded once opened (as the terminal
 * does), so flipping back finds it where it was, without going back to its address.
 */
import * as React from "react";
import { create } from "zustand";
import { ChevronDown, X } from "lucide-react";
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
}

/** Which cluster is shown (`null`: this one), and every one opened. */
export const useClusters = create<{ shown: string | null; opened: Opened[] }>(() => ({ shown: null, opened: [] }));

/** Within this of its grant ending, a frame is opened anew with a new one. */
const FRESH_MS = 60_000;

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
    useClusters.setState((s) => ({ shown: id, opened: was ? s.opened.map((o) => (o.id === id ? next : o)) : [...s.opened, next] }));
  } catch (e) {
    toast.error(`Couldn't open ${name}`, e instanceof ApiError ? e.message : String(e));
  }
}

export const showHome = () => useClusters.setState({ shown: null });

export function closeCluster(id: string) {
  useClusters.setState((s) => ({ shown: s.shown === id ? null : s.shown, opened: s.opened.filter((o) => o.id !== id) }));
}

const MANAGE = "\u0000manage";

/** The header's cluster picker: nothing until this cluster is peered with another. */
export function ClusterPicker() {
  const { data } = useRes(openRes);
  const shown = useClusters((s) => s.shown);
  const navigate = useNavigate();
  if (data.peers.length === 0) return null;
  const current = data.peers.find((p) => p.id === shown)?.name ?? data.name;
  return (
    <label className="cluster-pick" data-away={shown ? "" : undefined}>
      <span className="cluster-pick-name">{current}</span>
      <ChevronDown size={12} aria-hidden="true" />
      <select
        aria-label="Cluster"
        value={shown ?? ""}
        onChange={(e) => {
          const v = e.target.value;
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
      </select>
    </label>
  );
}

/** Under the header once a peered cluster is open: this one and each opened, to flip between. */
export function ClusterBar() {
  const { data } = useRes(openRes);
  const shown = useClusters((s) => s.shown);
  const opened = useClusters((s) => s.opened);
  if (opened.length === 0) return null;
  return (
    <div className="cluster-bar">
      <div className="container-console cluster-bar-inner" role="tablist" aria-label="Open clusters">
        <button type="button" role="tab" className="cluster-tab" aria-selected={shown === null} onClick={showHome}>
          {data.name || "This cluster"}
        </button>
        {opened.map((o) => (
          <span key={o.id} className="cluster-tab-pair">
            <button type="button" role="tab" className="cluster-tab" aria-selected={shown === o.id} onClick={() => void openCluster(o.id, o.name)}>
              {o.name}
            </button>
            <button type="button" className="cluster-close" aria-label={`Close ${o.name}`} onClick={() => closeCluster(o.id)}>
              <X size={12} />
            </button>
          </span>
        ))}
      </div>
    </div>
  );
}

/** Every opened cluster's frame; the one shown fills the page. */
export function ClusterFrames() {
  const shown = useClusters((s) => s.shown);
  const opened = useClusters((s) => s.opened);
  return (
    <>
      {opened.map((o) => (
        <PeerFrame key={o.id} o={o} visible={shown === o.id} />
      ))}
    </>
  );
}

function PeerFrame({ o, visible }: { o: Opened; visible: boolean }) {
  const target = `kiln-peer-${o.id}`;
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
    <div className={visible ? "cluster-frame-wrap" : "hidden"} aria-hidden={!visible}>
      <iframe name={target} title={o.name} className="cluster-frame" allow="clipboard-read; clipboard-write; fullscreen" />
      <form key={o.key} ref={post} method="post" action={o.frame.action} target={target} hidden>
        <input type="hidden" name="g" value={o.frame.grant} />
        <input type="hidden" name="to" value="/" />
      </form>
    </div>
  );
}

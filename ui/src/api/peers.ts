/** Peered clusters (docs/peers.md): this cluster's peerings, the codes that make them, and frames. */
import { request } from "./client";

export type PeerDirection = "out" | "in";
export type PeerState = "peered" | "waiting" | "expired";

export interface Peer {
  id: string;
  name: string;
  /** "out": our people open it; "in": its people open this cluster. */
  direction: PeerDirection;
  /** Out: its address, as reached from here. In: the address its challenge said. */
  url: string;
  /** In: what its people may do here. Out: what it said we may do there. */
  role: string;
  slug: string;
  port: number;
  state: PeerState;
  /** Its key, as people compare it (once known). */
  fingerprint: string | null;
  created_at: number;
  created_by: string;
  peered_at: number | null;
  /** While waiting: when its challenge stops working. */
  expires_at: number | null;
}

export interface Peers {
  name: string;
  fingerprint: string;
  roles: string[];
  peers: Peer[];
}

/** What the cluster picker lists: this cluster, and each it may open. */
export interface Openable {
  name: string;
  peers: { id: string; name: string; slug: string }[];
}

/** Where a peer opens for this person, and the grant to post there. */
export interface Frame {
  origin: string;
  action: string;
  grant: string;
  expires_at: number;
}

const at = (id: string, rest = "") => `/api/peers/${encodeURIComponent(id)}${rest}`;

export const peersApi = {
  list: (signal?: AbortSignal) => request<Peers>("GET", "/api/peers", undefined, { signal }),
  open: (signal?: AbortSignal) => request<Openable>("GET", "/api/peers/open", undefined, { signal }),
  register: (name: string, url: string) => request<{ peer: Peer; challenge: string }>("POST", "/api/peers", { name, url }),
  rechallenge: (id: string) => request<{ peer: Peer; challenge: string }>("POST", at(id, "/challenge"), {}),
  complete: (id: string, response: string) => request<Peer>("POST", at(id, "/complete"), { response }),
  authorize: (challenge: string, role: string) => request<{ peer: Peer; response: string }>("POST", "/api/peers/authorize", { challenge, role }),
  remove: (id: string) => request<void>("DELETE", at(id)),
  frame: (id: string) => request<Frame>("POST", at(id, "/frame"), {}),
};

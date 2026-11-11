import type { AgentInfo, Conn, State, Status } from "./harness";

type Head = { info: AgentInfo | null; state: State | null };

/** Info is refreshed by owner Open and each live State; connection alone says no lifecycle. */
export function sessionStatus(s: Head): Status {
  return s.info?.status ?? s.state?.status ?? "starting";
}

/** An owner header refreshes lifecycle and capacity without manufacturing the pending details. */
export function stateAtOpen(state: State | null, info: AgentInfo | null): State | null {
  if (!state || !info) return state;
  const sameAdmission = state.admission?.epoch === info.admission?.epoch && state.admission?.generation === info.admission?.generation;
  return state.status === info.status && sameAdmission ? state : { ...state, status: info.status, admission: info.admission };
}

export type SessionAvailability = "connecting" | "available" | "offline" | "gone";

/** Browser command transport and the owner's lifecycle are separate facts; neither is an ACK. */
export function sessionAvailability(s: Head & { conn: Conn }): SessionAvailability {
  if (s.conn === "gone") return "gone";
  if (s.conn !== "open" || (!s.info && !s.state)) return "connecting";
  return sessionStatus(s) === "disconnected" ? "offline" : "available";
}

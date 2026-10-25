import type { ControlOption } from "@/api/harness";

/**
 * The speed an agent runs at, as the head's bolt shows it: off (standard, its outline), on (filled,
 * one bolt for each step up: two for Codex's Ultrafast), cooling down (filled, grey and still), or
 * blocked (sketched: the account can't go faster, and it says why).
 */
export type BoltState = "off" | "on" | "cooldown" | "blocked";

export interface SpeedLine {
  /** The speeds faster than standard the model in use runs at, slowest first (`Meta.speeds`). */
  speeds: ControlOption[];
  /** The speed it runs at: "" standard. */
  speed: string;
  /** Why it can't go faster now, as its harness said. */
  blocked?: string;
  /** Fast mode's own rate limit is cooling down: standard speed for now. */
  cooldown?: boolean;
}

/** How many bolts are filled: none at standard (or a speed the model doesn't list), one for its first speed, two for the next. */
export const lit = (speeds: ControlOption[], speed: string): number => speeds.findIndex((s) => s.id === speed) + 1;

export function boltState({ speeds, speed, blocked, cooldown }: SpeedLine): BoltState {
  if (blocked) return "blocked";
  if (lit(speeds, speed) === 0) return "off";
  return cooldown ? "cooldown" : "on";
}

/** Where a click goes: the model's next speed up, and from its fastest back to standard. */
export function nextSpeed(speeds: ControlOption[], speed: string): string {
  const cycle = ["", ...speeds.map((s) => s.id)];
  return cycle[(cycle.indexOf(speed) + 1) % cycle.length] ?? "";
}

const capital = (v: string) => v.charAt(0).toUpperCase() + v.slice(1);

/** A speed's name: "Standard", else as its harness names it ("Fast", "Ultrafast"). */
export const speedName = (speeds: ControlOption[], id: string): string => (id ? speeds.find((s) => s.id === id)?.label || capital(id) : "Standard");

/** What the bolt says it is and what a click does: its label for assistive tech, and its tooltip. */
export function boltLabel(line: SpeedLine): string {
  const { speeds, speed, blocked } = line;
  const state = boltState(line);
  if (state === "blocked") return blocked ?? "";
  const next = nextSpeed(speeds, speed);
  const click = `Click for ${speedName(speeds, next)}`;
  if (state === "cooldown") return `${speedName(speeds, speed)}, cooling down: standard speed until its rate limit resets, then ${speedName(speeds, speed).toLowerCase()} again by itself. ${click}`;
  const said = state === "on" ? speeds.find((s) => s.id === speed)?.description : undefined;
  const upto = state === "off" ? speeds.find((s) => s.id === next)?.description : undefined;
  return `Speed: ${speedName(speeds, state === "on" ? speed : "")}${said ? ` (${said})` : ""}. ${click}${upto ? `: ${upto}` : ""}`;
}

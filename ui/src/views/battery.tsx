/**
 * A laptop runner's battery: on its row (how full, charging or how long it lasts), and a line at
 * the top of Runners and Mission Control while a machine on battery is low. A machine that runs
 * out sleeps, and its runner, agents and jobs with it.
 */
import type { Agent, Battery } from "@/api/types";
import { cn } from "@/lib/utils";

/** On battery at this or less, a machine is low: the server's `os::battery::LOW`. */
const LOW = 20;

/** It's on battery, and low: it sleeps soon. */
export const isLow = (b: Battery | null | undefined) => Boolean(b && !b.ac && b.percent <= LOW);

/** "1h 05m", "25 min". */
function left(minutes: number): string {
  return minutes >= 60 ? `${Math.floor(minutes / 60)}h ${String(minutes % 60).padStart(2, "0")}m` : `${minutes} min`;
}

/** "battery 34% · 2h 15m left", "battery 80% · charging", "battery 100% · on its charger". */
export function batteryWords(b: Battery): string {
  const how = b.charging ? "charging" : b.ac ? "on its charger" : b.minutes ? `${left(b.minutes)} left` : "on battery";
  return `battery ${b.percent}% · ${how}`;
}

/** A runner's battery, under its name: amber when it's low. */
export function BatteryLine({ battery }: { battery: Battery | null | undefined }) {
  if (!battery) return null;
  return <div className={cn("truncate font-mono text-[10px]", isLow(battery) ? "text-[#e0b25c]" : "text-[#7b7e88]")}>{batteryWords(battery)}</div>;
}

/**
 * Connected machines on battery that are low, each said with what stops when it sleeps:
 * `what(machine)` names it ("its 3 agents", "1 job").
 */
export function LowBattery({ agents, what }: { agents: Agent[]; what?: (machine: string) => string }) {
  const low = agents.filter((a) => a.connected && isLow(a.battery));
  if (low.length === 0) return null;
  return (
    <>
      {low.map((a) => {
        const b = a.battery;
        if (!b) return null;
        const stops = what?.(a.name) ?? "";
        return (
          <p key={a.name} role="status" className="mb-3 rounded-lg border border-[#3a3326] bg-[#1b1915] px-3 py-2.5 text-[13px] text-[#e0c98f]">
            {a.name} is on battery at {b.percent}%{b.minutes ? `, about ${left(b.minutes)} left` : ""}. Plug it in, or it sleeps and its runner with it
            {stops ? `: ${stops} stop` : ""}.
          </p>
        );
      })}
    </>
  );
}

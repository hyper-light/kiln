/**
 * Which Agentfile an agent's microVM is made from, chosen as it starts (kiln plan
 * 01a1cd8e862e7169a074db3d5fa33f3d, step 31): kiln's for its harness or one of the person's own, kept
 * per harness and per project (the folder it starts in), so the next agent of that harness there
 * starts the same way. None is a process on its machine, as agents start without one. Pure but for
 * the list it reads, so the composer (views/harness/new-agent.tsx) and its tests share one reading;
 * apart from the Agentfiles pages' own code (api/agentfiles.ts), so Mission control loads none of it.
 */
import { request } from "@/api/client";
import type { Agent } from "@/api/types";
import type { AgentfileBuiltin, AgentfileListing } from "@/api/wire";
import type { Res } from "@/lib/data";

/** What a start chooses from: yours and kiln's. */
type Listed = Pick<AgentfileListing, "agentfiles" | "builtin">;

/**
 * The list as a start chooses from it (`GET /api/agentfiles`): yours and kiln's, or none where it
 * can't be had (a server from before Agentfiles), so a start never waits on it failing.
 */
export const agentfileChoicesRes: Res<Listed> = {
  key: "agentfiles/choices",
  fetch: (signal) =>
    request<AgentfileListing>("GET", "/api/agentfiles", undefined, { signal }).catch((e: unknown) => {
      if (signal.aborted) throw e;
      return { agentfiles: [], builtin: [] };
    }),
  opts: { on: ["agentfile"] },
};

/**
 * The Agentfile an agent's microVM is made from, as its start names it (`Spec.isolation.image`,
 * docs/api.md "Agentfiles"): one kiln ships by its name (`harness-claude`: an image's, so it never
 * has a `:`), or one of the person's own by its id (`agentfile:<id>`), at the version it has when
 * the microVM is made.
 */
export const builtinImage = (b: Pick<AgentfileBuiltin, "name">): string => b.name;
export const keptImage = (id: string): string => `agentfile:${id}`;

/** One to choose: the image the start names (`Spec.isolation.image`), its name, and whose it is. */
export interface AgentfileChoice {
  image: string;
  name: string;
  /** `kiln`: one kiln ships, for this harness; `yours`: one of the person's own. */
  whose: "kiln" | "yours";
}

/** What harness `kind` can start from: kiln's for it, then yours, newest change first. */
export function choicesFor(listing: Listed, kind: string): AgentfileChoice[] {
  return [
    ...listing.builtin.filter((b) => b.harness === kind).map((b): AgentfileChoice => ({ image: builtinImage(b), name: b.name, whose: "kiln" })),
    ...listing.agentfiles.map((a): AgentfileChoice => ({ image: keptImage(a.id), name: a.name, whose: "yours" })),
  ];
}

/** The project an agent starts in, as its choice is kept: its folder as typed, without a trailing `/`; none (its home, or a shell's folder) is "". */
export function projectOf(cwd: string, shell: string): string {
  const d = cwd.trim();
  const folder = d.length > 1 ? d.replace(/\/+$/, "") : d;
  return shell || folder === "~" ? "" : folder;
}

/** Where a choice is kept: per harness and per project. */
const keyOf = (kind: string, project: string) => `${kind}\n${project}`;

/** The choice kept for harness `kind` in `project`, while it's still one to choose; else none. */
export function chosenFor(kept: Readonly<Record<string, string>>, choices: AgentfileChoice[], kind: string, project: string): AgentfileChoice | null {
  const image = kept[keyOf(kind, project)];
  return choices.find((c) => c.image === image) ?? null;
}

/** The choices kept, with `image` (none: a process) now harness `kind`'s in `project`. */
export function withChoice(kept: Readonly<Record<string, string>>, kind: string, project: string, image: string | null): Record<string, string> {
  const key = keyOf(kind, project);
  const rest = Object.fromEntries(Object.entries(kept).filter(([k]) => k !== key));
  return image ? { ...rest, [key]: image } : rest;
}

/**
 * Why an agent can't be held in a microVM on `target` (`local`, `agent:<name>`), as the server
 * refuses one there (`harness::lineage::isolation_on`): a machine starts them once its runner says
 * so; kiln's server starts none itself. Null where it can.
 */
export function whyNoMicrovm(target: string, machines: readonly Pick<Agent, "name" | "facts">[], shown: string): string | null {
  const name = target.startsWith("agent:") ? target.slice("agent:".length) : null;
  if (!name) return "kiln's server starts no microVMs itself";
  const said = machines.find((m) => m.name === name)?.facts?.virt.microvm;
  if (said) return null;
  return said === false ? `${shown} doesn't start microVMs` : `${shown} hasn't said yet whether it starts microVMs`;
}

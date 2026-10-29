/**
 * Where an agent can go on: here in another harness, or on another of your machines that takes
 * agents over, in its own harness or one its conversation can be carried into (any other: Claude
 * Code, Codex, Devin, Antigravity, the other ACP agents). Two selects: the machine, then the harness
 * and model there (its own default, for a harness that lists none yet). At a usage limit it can go
 * on so by itself (`AtLimit`; the agent keeps the choice, `State.move_plan`), or a person sends it
 * now (`GoOnNow`, above the composer while it's paused).
 */
import * as React from "react";
import { ChevronDown } from "lucide-react";
import { harnessApi, type AgentInfo, type Command, type KindInfo, type MovePlan, type State } from "@/api/harness";
import { Button } from "@/components/ui/button";
import { toast } from "@/components/ui/toast";
import { errorMessage } from "@/lib/hooks";
import { Link } from "@/lib/router";
import { useSuspenseResource } from "@/lib/resource";
import { useClock } from "./dom";
import { askPlaces } from "./move";

type Agent = Pick<AgentInfo, "id" | "name" | "target" | "kind" | "kind_name">;

/** A machine it can go on on, and the harnesses there it can go on in, its own first. */
interface Place {
  target: string;
  name: string;
  kinds: KindInfo[];
}

/** Where `agent` can go on: the other harnesses here, the machines that take it over, and its machine's runner being too old to hand it over. */
function usePlaces(agent: Agent) {
  const { byTarget, runners } = useSuspenseResource("harness/move", askPlaces).data;
  // Carried into another harness only out of one that's carried too.
  const carries = (byTarget[agent.target] ?? []).find((k) => k.id === agent.kind)?.switch ?? false;
  const goesIn = (k: KindInfo) => k.available && (k.id === agent.kind || (carries && Boolean(k.switch)));
  const here = (byTarget[agent.target] ?? []).filter((k) => k.id !== agent.kind && goesIn(k));
  const machine = agent.target.startsWith("agent:");
  const places: Place[] = machine
    ? runners
        .filter((r) => r.connected && r.reach && r.fingerprint && `agent:${r.name}` !== agent.target)
        .map((r) => ({
          target: `agent:${r.name}`,
          name: r.name,
          kinds: (byTarget[`agent:${r.name}`] ?? []).filter(goesIn).sort((a, b) => Number(b.id === agent.kind) - Number(a.id === agent.kind)),
        }))
        .filter((p) => p.kinds.length > 0)
    : [];
  const mine = runners.find((r) => `agent:${r.name}` === agent.target);
  return { here, places, older: Boolean(mine && !mine.fingerprint) };
}

/** `kind:model`, a choice of the second select ("" its own harness, as it is; `kind:` that harness's own default model). */
const pick = (kind?: string, model?: string) => (kind ? `${kind}:${model ?? ""}` : "");
const unpick = (v: string): { kind?: string; model?: string } => {
  const [kind, ...rest] = v.split(":");
  const model = rest.join(":");
  return kind ? { kind, ...(model ? { model } : {}) } : {};
};

/** The harnesses and their models, as options: its own first ("as it is"), then each other one's models (its default, for one that lists none yet). */
function HarnessOptions({ agent, kinds, own }: { agent: Agent; kinds: KindInfo[]; own: boolean }) {
  return (
    <>
      {own && <option value="">{agent.kind_name}, as it is</option>}
      {kinds
        .filter((k) => k.id !== agent.kind)
        .map((k) => (
          <optgroup key={k.id} label={k.name}>
            {k.models?.length ? (
              k.models.map((m) => (
                <option key={m.id} value={pick(k.id, m.id)}>
                  {k.name} · {m.label || m.id}
                </option>
              ))
            ) : (
              <option value={pick(k.id)}>{k.name} · its default model</option>
            )}
          </optgroup>
        ))}
    </>
  );
}

function Select({ label, value, onChange, disabled, children }: { label: string; value: string; onChange: (v: string) => void; disabled?: boolean; children: React.ReactNode }) {
  return (
    <span className="select hx-pick">
      <select aria-label={label} value={value} disabled={disabled} onChange={(e) => onChange(e.target.value)}>
        {children}
      </select>
      <ChevronDown size={12} aria-hidden="true" />
    </span>
  );
}

/** What's chosen, in a word: "" waiting, else where and in what. */
const chosenOf = (on: boolean, plan: MovePlan) => (on ? `${plan.to ?? "*"}|${pick(plan.kind, plan.model)}` : "");

/**
 * What it does at a usage limit: waits for the reset, or goes on elsewhere, on whichever machine has
 * room or the one chosen, in its own harness or the one chosen there. Kept by the agent: what's
 * shown is what it says back. A runner that doesn't say it back in a few seconds runs a kiln from
 * before this, and that's said, with the way to upgrade it.
 */
export function AtLimit({ agent, state, send }: { agent: Agent; state: State | null; send: (c: Command) => void }) {
  const { places } = usePlaces(agent);
  const [asked, setAsked] = React.useState<{ at: number; want: string } | null>(null);
  const now = useClock(asked ? 1000 : 60_000);
  const on = state?.move_on_limit === true;
  // A runner from before a machine and harness could be chosen sends no plan: anywhere, its own.
  const plans = state?.move_plan !== undefined;
  const plan: MovePlan = state?.move_plan ?? {};
  const where = !on ? "" : (plan.to ?? "*");
  const unanswered = asked !== null && asked.want !== chosenOf(on, plan) && now - asked.at > 3500;
  const there = places.find((p) => p.target === where);
  // Anywhere: the harnesses some machine has; there: those it has.
  const kinds = there ? there.kinds : [...new Map(places.flatMap((p) => p.kinds).map((k) => [k.id, k])).values()];
  const set = (next: { on: boolean } & MovePlan) => {
    setAsked({ at: Date.now(), want: chosenOf(next.on, next) });
    send({ type: "move_on_limit", ...next });
  };
  return (
    <span className="hx-atlimit">
      <Select label="At a limit" value={where} onChange={(v) => set(v === "" ? { on: false } : { on: true, ...(v === "*" ? {} : { to: v }) })}>
        <option value="">Wait for the reset</option>
        <option value="*">Go on wherever has room</option>
        {plans &&
          places.map((p) => (
            <option key={p.target} value={p.target}>
              Go on on {p.name}
            </option>
          ))}
      </Select>
      {on && plans && kinds.some((k) => k.id !== agent.kind) && (
        <Select label="In" value={pick(plan.kind, plan.model)} onChange={(v) => set({ on: true, ...(plan.to ? { to: plan.to } : {}), ...unpick(v) })}>
          <HarnessOptions agent={agent} kinds={kinds} own={!there || there.kinds.some((k) => k.id === agent.kind)} />
        </Select>
      )}
      {unanswered ? (
        <span className="hx-pick-why">
          Its runner didn&rsquo;t take this: it runs an older kiln. <Link to="/system#system.updates">Upgrade it</Link>.
        </span>
      ) : (
        on && !plans && <span className="hx-pick-why">Its runner picks the machine: upgrade it to choose one.</span>
      )}
    </span>
  );
}

/**
 * Go on now, from a pause: here in another harness, or on another machine (the turn the limit cut
 * off goes on there), then the harness and model there.
 */
export function GoOnNow({ agent, canSwitch, send }: { agent: Agent; canSwitch: boolean; send: (c: Command) => void }) {
  const { here, places } = usePlaces(agent);
  const switches = canSwitch ? here : [];
  const [where, setWhere] = React.useState<string | null>(null);
  const [what, setWhat] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  if (switches.length === 0 && places.length === 0) return null;
  // Here first when it can change harness here; else the first machine.
  const shown = where ?? (switches.length ? "" : (places[0]?.target ?? ""));
  const there = places.find((p) => p.target === shown);
  const kinds = there ? there.kinds : switches;
  const own = Boolean(there?.kinds.some((k) => k.id === agent.kind));
  // Here it changes harness: one must be chosen (its own default model, when it lists none). There it may go as it is.
  const chosen = what || (own ? "" : pick(kinds.find((k) => k.id !== agent.kind)?.id, kinds.find((k) => k.id !== agent.kind)?.models?.[0]?.id));
  const go = async () => {
    const { kind, model } = unpick(chosen);
    if (!there) {
      if (kind) send({ type: "set_model", model: model ?? "", kind });
      return;
    }
    setBusy(true);
    try {
      await harnessApi.move(agent.id, there.target, true, kind, model);
      toast.success(`Moving ${agent.name} to ${there.name}`, "The turn the limit cut off goes on there.");
    } catch (e) {
      toast.error("Couldn't move it", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="hx-goon">
      <span className="hx-goon-label">Go on now</span>
      <Select
        label="Where it goes on now"
        value={shown}
        onChange={(v) => {
          setWhere(v);
          setWhat("");
        }}
      >
        {switches.length > 0 && <option value="">Here, in another harness</option>}
        {places.map((p) => (
          <option key={p.target} value={p.target}>
            On {p.name}
          </option>
        ))}
      </Select>
      <Select label="In" value={chosen} onChange={setWhat}>
        <HarnessOptions agent={agent} kinds={kinds} own={own} />
      </Select>
      <Button size="xs" variant="secondary" disabled={busy || (!there && !chosen)} onClick={() => void go()}>
        {busy ? "Moving…" : "Go"}
      </Button>
    </div>
  );
}

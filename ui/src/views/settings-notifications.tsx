/**
 * Settings → Notifications (docs/notifications.md): this device on or off (the browser asks for
 * permission only on that click), what it hears, a test, and your devices, each with when it last
 * got something and why it didn't. When this device can't have them, it says why and what to do.
 */
import * as React from "react";
import { Link } from "@/lib/router";
import { Trash2 } from "lucide-react";
import { pushApi, pushDevicesRes, pushStateRes, setHears, thisDeviceRes, turnOff, turnOn, type PushDevice, type PushKind, type PushState, type PushTest, type ThisDevice } from "@/api/push";
import { Button } from "@/components/ui/button";
import { Hold } from "@/components/ui/hold";
import { Panel } from "@/components/ui/misc";
import { relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { useRes } from "@/lib/data";
import { invalidate } from "@/lib/resource";

const DESCRIPTION = "When an agent needs you, finishes, stops or pauses, kiln tells your devices, even with kiln closed. Tapping one opens the agent.";

export function NotificationsHold() {
  return (
    <Panel eyebrow="Agents" title="Notifications" description={DESCRIPTION}>
      <Hold height={96} />
    </Panel>
  );
}

/** Why this device can't have notifications, and what to do about it; nothing when it can. */
export function blockedWhy(state: PushState, device: ThisDevice): React.ReactNode {
  if (!state.available) return state.why ?? "Notifications aren't available on this server.";
  switch (device.block) {
    case "insecure":
      return (
        <>
          Notifications need a secure page: open kiln over HTTPS, through a front door (<Link to="/runners">Runners → Domain &amp; HTTPS</Link>), or at localhost on this machine.
        </>
      );
    case "home-screen":
      return "On an iPhone or iPad, notifications need kiln on the Home Screen: tap Share, then Add to Home Screen, and open kiln from there. Then turn them on here.";
    case "unsupported":
      return "This browser can't receive push notifications.";
    case null:
      return device.permission === "denied" ? "Notifications are blocked for this site in this browser's settings. Allow them there, then turn them on here." : null;
  }
}

const hearsWords = (hears: PushKind[], state: PushState) =>
  hears.length === state.kinds.length && state.kinds.length > 0 ? "Everything" : hears.length === 0 ? "Nothing" : hears.map((h) => state.kinds.find((k) => k.id === h)?.label ?? h).join(", ");

export function NotificationsPanel() {
  const state = useRes(pushStateRes).data;
  const devices = useRes(pushDevicesRes).data;
  const device = useRes(thisDeviceRes).data;
  const mine = device.endpoint ? devices.find((d) => d.endpoint === device.endpoint) : undefined;
  const [busy, setBusy] = React.useState(false);
  const [problem, setProblem] = React.useState<string | null>(null);
  const [tested, setTested] = React.useState<PushTest | null>(null);
  const blocked = blockedWhy(state, device);

  const act = async (what: () => Promise<unknown>) => {
    setBusy(true);
    setProblem(null);
    try {
      await what();
    } catch (e) {
      setProblem(errorMessage(e));
    } finally {
      setBusy(false);
      invalidate("push");
    }
  };
  const toggle = (on: boolean) => act(() => (on ? turnOn(state.public_key ?? "") : turnOff(mine?.id)));
  const hear = (kind: PushKind, on: boolean) => {
    const now = new Set(mine?.hears ?? []);
    if (on) now.add(kind);
    else now.delete(kind);
    void act(() => setHears(state.kinds.map((k) => k.id).filter((k) => now.has(k))));
  };
  const test = () =>
    act(async () => {
      setTested(null);
      setTested(await pushApi.test());
    });

  return (
    <Panel
      eyebrow="Agents"
      title="Notifications"
      description={DESCRIPTION}
      actions={
        devices.length > 0 && state.available ? (
          <Button variant="ghost" size="sm" disabled={busy} onClick={() => void test()}>
            Send a test
          </Button>
        ) : undefined
      }
    >
      <div className="space-y-4">
        {blocked ? (
          <p className="text-[13px] leading-relaxed text-[#c9c3b6]">{blocked}</p>
        ) : (
          <div className="space-y-3">
            <label className="inline-flex min-h-8 cursor-pointer items-center gap-3 text-[13.5px] text-[#ededee]">
              <input type="checkbox" className="check" checked={Boolean(mine)} disabled={busy} onChange={(e) => void toggle(e.target.checked)} />
              Notify this device
            </label>
            {mine && (
              <fieldset className="flex flex-wrap items-center gap-x-5 gap-y-2" disabled={busy}>
                <legend className="sr-only">What this device hears</legend>
                {state.kinds.map((k) => (
                  <label key={k.id} className="inline-flex cursor-pointer items-center gap-2 text-[13px] text-[#c8cad0]" title={k.means}>
                    <input type="checkbox" className="check" checked={mine.hears.includes(k.id)} onChange={(e) => hear(k.id, e.target.checked)} />
                    {k.label}
                  </label>
                ))}
              </fieldset>
            )}
          </div>
        )}
        {state.available && state.reached?.ok === false && (
          <p className="text-[12.5px] text-[#d8c8b0]">This server can&rsquo;t reach the push services ({state.reached.error}). It needs outbound HTTPS to them; until then no device hears anything.</p>
        )}
        {problem && <p className="text-[12.5px] text-[#d8c8b0]">{problem}</p>}
        {tested && (
          <p className="text-[12.5px] text-[#9a9da6]">
            {tested.sent > 0 ? `A test went to ${tested.sent} device${tested.sent === 1 ? "" : "s"}.` : "The test reached no device."}
            {tested.results
              .filter((r) => !r.ok)
              .map((r) => (
                <span key={r.id} className="block text-[#d8c8b0]">
                  {r.device}: {r.error}
                </span>
              ))}
          </p>
        )}
        <Devices devices={devices} state={state} mine={mine} />
      </div>
    </Panel>
  );
}

/** Your devices: what each hears, how its last delivery went, and remove. */
function Devices({ devices, state, mine }: { devices: PushDevice[]; state: PushState; mine: PushDevice | undefined }) {
  if (devices.length === 0) return <p className="text-[13px] text-[#9a9da6]">No device of yours gets notifications yet.</p>;
  return (
    <ul className="divide-y divide-white/[0.06] border-t border-white/[0.06]">
      {devices.map((d) => (
        <DeviceRow key={d.id} d={d} state={state} here={d.id === mine?.id} />
      ))}
    </ul>
  );
}

function DeviceRow({ d, state, here }: { d: PushDevice; state: PushState; here: boolean }) {
  const [busy, setBusy] = React.useState(false);
  const [problem, setProblem] = React.useState<string | null>(null);
  const remove = async () => {
    setBusy(true);
    setProblem(null);
    try {
      if (here) await turnOff(d.id);
      else await pushApi.remove(d.id);
    } catch (e) {
      setProblem(errorMessage(e));
    } finally {
      setBusy(false);
      invalidate("push");
    }
  };
  const failed = d.last_error && (!d.last_ok_at || (d.last_error_at ?? 0) >= d.last_ok_at);
  return (
    <li className="flex items-start gap-3 py-3">
      <div className="min-w-0 flex-1 space-y-1">
        <p className="text-[13.5px] text-[#eceef2]">
          {d.device}
          {here && <span className="ml-2 text-[12px] text-[#8b8f99]">this device</span>}
        </p>
        <p className="text-[12px] text-[#8b8f99]">Hears: {hearsWords(d.hears, state).toLowerCase()}</p>
        <p className={failed ? "text-[12px] text-[#d1aaad]" : "text-[12px] text-[#8b8f99]"}>
          {failed ? `Last delivery failed ${relativeTime(d.last_error_at)}: ${d.last_error}` : d.last_ok_at ? `Last delivered ${relativeTime(d.last_ok_at)}` : "Nothing sent yet"}
        </p>
        {problem && <p className="text-[12px] text-[#d8c8b0]">{problem}</p>}
      </div>
      <Button size="xs" variant="ghost-danger" disabled={busy} onClick={() => void remove()} aria-label={`Remove ${d.device}`}>
        <Trash2 /> Remove
      </Button>
    </li>
  );
}

/**
 * Settings → This device → Notifications (docs/notifications.md): this device on or off (the
 * browser asks for permission only on that click), what it hears, a test, and your devices, each
 * with when it last got something and why it didn't. When this device can't have them, it says why
 * and what to do. `/settings#device.notifications` opens it (Mission control's offer links here).
 */
import * as React from "react";
import "./settings-notifications.css";
import { Link } from "@/lib/router";
import { pushApi, pushDevicesRes, pushStateRes, setHears, thisDeviceRes, turnOff, turnOn, type PushDevice, type PushKind, type PushState, type PushTest, type ThisDevice } from "@/api/push";
import { Button } from "@/components/ui/button";
import { Hold } from "@/components/ui/hold";
import { SettingRow, SettingsGroup } from "@/components/ui/settings";
import { relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { useRes } from "@/lib/data";
import { invalidate } from "@/lib/resource";

const ID = "device.notifications";
const TITLE = "Notifications";
const ABOUT = "When an agent needs you, finishes, stops or pauses, kiln tells your devices, even with kiln closed; tapping one opens the agent.";
const WORDS = "push alerts notifications phone mute devices test";

export function NotificationsHold() {
  return (
    <SettingsGroup id={ID} title={TITLE} about={ABOUT} words={WORDS}>
      <Hold height={64} />
    </SettingsGroup>
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
  hears.length === state.kinds.length && state.kinds.length > 0
    ? "everything"
    : hears.length === 0
      ? "nothing"
      : hears.map((h) => (state.kinds.find((k) => k.id === h)?.label ?? h).toLowerCase()).join(", ");

export function NotificationsGroup() {
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
    <SettingsGroup
      id={ID}
      title={TITLE}
      about={ABOUT}
      words={WORDS}
      actions={
        devices.length > 0 && state.available ? (
          <Button size="xs" variant="secondary" disabled={busy} onClick={() => void test()}>
            Send a test
          </Button>
        ) : undefined
      }
    >
      {blocked ? (
        <p className="st-note">{blocked}</p>
      ) : (
        <>
          <SettingRow inline htmlFor="st-push" label="Notify this device" help="The browser asks for permission first, on this click.">
            <input id="st-push" type="checkbox" className="check" checked={Boolean(mine)} disabled={busy} onChange={(e) => void toggle(e.target.checked)} />
          </SettingRow>
          {mine && (
            <SettingRow label="What it hears" help="Each agent can be muted from its ⋯ menu, on every device.">
              <fieldset className="st-hears" disabled={busy}>
                <legend className="sr-only">What this device hears</legend>
                {state.kinds.map((k) => (
                  <label key={k.id} title={k.means}>
                    <input type="checkbox" className="check" checked={mine.hears.includes(k.id)} onChange={(e) => hear(k.id, e.target.checked)} />
                    {k.label}
                  </label>
                ))}
              </fieldset>
            </SettingRow>
          )}
        </>
      )}
      {state.available && state.reached?.ok === false && (
        <p className="st-note" data-tone="error">
          This server can&rsquo;t reach the push services ({state.reached.error}). It needs outbound HTTPS to them; until then no device hears anything.
        </p>
      )}
      {problem && (
        <p className="st-note" data-tone="error">
          {problem}
        </p>
      )}
      {tested && (
        <p className="st-note" data-tone={tested.results.some((r) => !r.ok) ? "error" : undefined}>
          {tested.sent > 0 ? `A test went to ${tested.sent} device${tested.sent === 1 ? "" : "s"}.` : "The test reached no device."}
          {tested.results
            .filter((r) => !r.ok)
            .map((r) => ` ${r.device}: ${r.error}`)
            .join("")}
        </p>
      )}
      <Devices devices={devices} state={state} mine={mine} />
    </SettingsGroup>
  );
}

/** Your devices: what each hears, how its last delivery went, and remove. */
function Devices({ devices, state, mine }: { devices: PushDevice[]; state: PushState; mine: PushDevice | undefined }) {
  return (
    <div className="st-push-devices">
      <span className="st-label">Your devices</span>
      {devices.length === 0 ? (
        <p className="st-note">None gets notifications yet.</p>
      ) : (
        <ul className="st-list">
          {devices.map((d) => (
            <DeviceItem key={d.id} d={d} state={state} here={d.id === mine?.id} />
          ))}
        </ul>
      )}
    </div>
  );
}

function DeviceItem({ d, state, here }: { d: PushDevice; state: PushState; here: boolean }) {
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
    <li>
      <div className="st-item">
        <span className="st-item-name">
          {d.device}
          {here && " (this device)"}
        </span>
        <span className="st-item-meta">Hears {hearsWords(d.hears, state)}</span>
        <span className="st-item-acts">
          <Button size="xs" variant="ghost-danger" disabled={busy} onClick={() => void remove()} aria-label={`Remove ${d.device}`}>
            Remove
          </Button>
        </span>
      </div>
      <p className="st-note" data-tone={failed ? "error" : undefined}>
        {failed ? `Last delivery failed ${relativeTime(d.last_error_at)}: ${d.last_error}` : d.last_ok_at ? `Last delivered ${relativeTime(d.last_ok_at)}` : "Nothing sent yet"}
      </p>
      {problem && (
        <p className="st-note" data-tone="error">
          {problem}
        </p>
      )}
    </li>
  );
}

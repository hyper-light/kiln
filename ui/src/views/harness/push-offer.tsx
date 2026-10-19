/**
 * Mission control's one quiet line about notifications (docs/notifications.md): when this device
 * could have them and doesn't, it offers to turn them on, right here, in one step (the browser asks
 * for permission on that click). When that fails, it says why and links to Settings. "Not now" puts
 * it away in this browser.
 */
import * as React from "react";
import { Link } from "@/lib/router";
import { pushDevicesRes, pushStateRes, thisDeviceRes, turnOn } from "@/api/push";
import { useRes } from "@/lib/data";
import { errorMessage, useLocalStorage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";

export function PushOffer() {
  const state = useRes(pushStateRes).data;
  const device = useRes(thisDeviceRes).data;
  const devices = useRes(pushDevicesRes).data;
  const [dismissed, setDismissed] = useLocalStorage<boolean>("kiln.push.offer.dismissed", false);
  const [busy, setBusy] = React.useState(false);
  const [problem, setProblem] = React.useState<string | null>(null);
  const on = device.endpoint !== null && devices.some((d) => d.endpoint === device.endpoint);
  if (dismissed || on || !state.available || !state.public_key || device.block !== null || device.permission === "denied") return null;
  const key = state.public_key;
  const go = async () => {
    setBusy(true);
    setProblem(null);
    try {
      await turnOn(key);
    } catch (e) {
      setProblem(errorMessage(e));
    } finally {
      setBusy(false);
      invalidate("push");
    }
  };
  return (
    <p className="ms-offer" role="status">
      <span className="ms-offer-text" title="Turned on, this device hears when an agent needs you, finishes, stops or pauses.">
        {problem ?? "Notifications are off here."}
      </span>
      {problem ? (
        <Link to="/settings#device.notifications" className="ms-link">
          Settings
        </Link>
      ) : (
        <button type="button" className="ms-link" disabled={busy} onClick={() => void go()}>
          Turn them on
        </button>
      )}
      <button type="button" className="ms-link ms-offer-not" onClick={() => setDismissed(true)}>
        Not now
      </button>
    </p>
  );
}

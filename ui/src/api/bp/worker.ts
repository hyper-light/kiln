/**
 * The browser's protocol socket in a SharedWorker (goal 3 of the browser protocol program, plan
 * 01a1a832eb2f711f9c6f51d88effe89d step 6; `shared.ts`): it takes the owner's Web Lock, holds it while
 * it lives (so its tabs learn the moment it dies), and runs the `Hub` for every tab's port.
 */
import { Hub, OWNER_LOCK, type Down, type Up } from "./hub";

/** What a SharedWorker's global scope gives (the app is typed for windows). */
type Scope = { onconnect: ((e: MessageEvent) => void) | null; close: () => void; navigator: Navigator };
const scope = globalThis as unknown as Scope;

const ports = new Set<MessagePort>();
const send = (d: Down): void => {
  for (const p of ports) p.postMessage(d);
};
const hub = new Hub(`worker-${crypto.randomUUID()}`, send);
let owning = false;

scope.onconnect = (e: MessageEvent) => {
  const port = e.ports[0];
  if (!port) return;
  ports.add(port);
  port.onmessage = (m: MessageEvent<Up>) => {
    if (m.data.t === "die") {
      // The lab's: end as Android may end a worker, its socket with it.
      hub.stop();
      scope.close();
      return;
    }
    if (m.data.t === "leave") ports.delete(port);
    hub.take(m.data);
  };
  port.start();
  if (owning) port.postMessage({ t: "owner", owner: hub.name } satisfies Down);
};

// Owns the socket once it holds the lock, and holds it while it lives.
void scope.navigator.locks.request(
  OWNER_LOCK,
  () =>
    new Promise<void>(() => {
      owning = true;
      hub.start();
    }),
);

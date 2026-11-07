// What the lab's pages hold for its driver (typecheck only): the probe page's handle
// (probe/probe.js) and the sockets the recorder sees the UI open (record.mjs).
interface Window {
  __lab: any;
  __labSockets: WebSocket[];
}

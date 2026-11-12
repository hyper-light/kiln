/** W26-R15: grammar loading, parsing and injection convergence stay off the input thread. */
import { assets } from "./assets";
import { Highlighter } from "./engine";
import { HighlightQueue } from "./worker-queue";
import type { HighlightRequest, HighlightResponse } from "./worker-protocol";

// The application's DOM types exclude WebWorker's conflicting global declarations.
const port = globalThis as unknown as { onmessage: (event: MessageEvent<HighlightRequest>) => void; postMessage: (response: HighlightResponse) => void };
const queue = new HighlightQueue(new Highlighter(assets), (response) => port.postMessage(response));
port.onmessage = (event) => queue.receive(event.data);

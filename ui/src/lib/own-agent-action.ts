/**
 * Human agent commands, plan 01a199e1b177752e84dad5dc990e6a15: the rows a press acts on, told as it's
 * pressed (`start`, so the row it acted on can move at once) and once its request is done (`done`).
 * Concurrent recipients form one completed action; one that failed alone moves nothing.
 */
type Listener = { done: (ids: readonly string[]) => void; start?: (ids: readonly string[]) => void };
const listeners = new Set<Listener>();
let pending = 0;
let succeeded = false;
let acted = new Set<string>();

function tell(which: "start" | "done", ids: readonly string[]) {
  for (const listener of listeners) {
    try {
      listener[which]?.(ids);
    } catch {
      /* Attention telemetry cannot fail a completed human command. */
    }
  }
}

/** `ids`: the agents it acts on, as far as they're known when it's pressed; `resultIds`: those its answer names (a broadcast's). */
export async function ownAgentAction<T>(operation: Promise<T>, ids: readonly string[] = [], resultIds?: (result: T) => readonly string[]): Promise<T> {
  pending++;
  for (const id of ids) acted.add(id);
  if (ids.length > 0) tell("start", ids);
  try {
    const result = await operation;
    succeeded = true;
    for (const id of resultIds?.(result) ?? []) acted.add(id);
    return result;
  } finally {
    pending--;
    if (pending === 0) {
      const all = [...acted];
      acted = new Set();
      if (succeeded) {
        succeeded = false;
        tell("done", all);
      }
    }
  }
}

export function onOwnAgentAction(done: (ids: readonly string[]) => void, start?: (ids: readonly string[]) => void) {
  const listener: Listener = { done, start };
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

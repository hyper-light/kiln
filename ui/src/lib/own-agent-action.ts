/** Human agent commands, plan 01a199e1b177752e84dad5dc990e6a15. Concurrent recipients form one completed action. */
const listeners = new Set<() => void>();
let pending = 0;
let succeeded = false;
export async function ownAgentAction<T>(operation: Promise<T>): Promise<T> {
  pending++;
  try {
    const result = await operation;
    succeeded = true;
    return result;
  } finally {
    pending--;
    if (pending === 0 && succeeded) {
      succeeded = false;
      for (const listener of listeners) {
        try {
          listener();
        } catch {
          /* Attention telemetry cannot fail a completed human command. */
        }
      }
    }
  }
}
export function onOwnAgentAction(listener: () => void) {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

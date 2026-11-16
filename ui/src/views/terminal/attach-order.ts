/**
 * The order terminals attach in (W21-R02): the one in view first, the others after it, one at a time.
 *
 * Every tab's view attaches to its session, and a session's replay is up to 256 KiB. Attached all at
 * once, ten tabs' replays share the link, and the one in view has its scrollback, and then what its
 * shell says next, behind the others' (docs/wire.md, "A terminal's screen first"). So a view asks for
 * its turn before it opens its socket. One in view never waits: it attaches at once, whatever else is
 * attaching. The others go one at a time, in the order they asked, each once nothing is attaching:
 * the one before it has its replay drawn, or its socket has closed.
 */
export interface AttachOrder {
  /**
   * `go` is called when it's `id`'s turn: now when it's in view, else once nothing is attaching.
   * What it returns is `id` leaving: it gives up its place, or its turn if it has it.
   */
  ask(id: string, inView: () => boolean, go: () => void): () => void;
  /** `id` has its replay drawn, or its socket closed: its turn is over. */
  done(id: string): void;
  /** `id` came into view: it goes now, if it's still waiting. */
  shown(id: string): void;
}

export function attachOrder(): AttachOrder {
  const waiting: { id: string; go: () => void }[] = [];
  const attaching = new Set<string>();
  let looking = false;
  const start = (id: string, go: () => void) => {
    attaching.add(id);
    go();
  };
  /** The first waiting goes, if nothing is attaching. Looked at once the task's asks are all in: views made together ask together. */
  const next = () => {
    if (looking) return;
    looking = true;
    queueMicrotask(() => {
      looking = false;
      if (attaching.size > 0) return;
      const first = waiting.shift();
      if (first) start(first.id, first.go);
    });
  };
  const leave = (id: string) => {
    const at = waiting.findIndex((w) => w.id === id);
    if (at >= 0) waiting.splice(at, 1);
    if (attaching.delete(id)) next();
  };
  return {
    ask(id, inView, go) {
      leave(id);
      if (inView()) start(id, go);
      else {
        waiting.push({ id, go });
        next();
      }
      return () => leave(id);
    },
    done(id) {
      if (attaching.delete(id)) next();
    },
    shown(id) {
      const at = waiting.findIndex((w) => w.id === id);
      const mine = at >= 0 ? waiting.splice(at, 1)[0] : undefined;
      if (mine) start(mine.id, mine.go);
    },
  };
}

/** The page's terminals' one order. */
export const terminalsAttach = attachOrder();

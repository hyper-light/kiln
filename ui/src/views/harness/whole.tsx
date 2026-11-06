/**
 * What a lean event held back (docs/wire.md, "Windows"), asked for when it's shown: a command's
 * output, an edit's lines, once a person opens them. The page's first load carries a settled tool
 * call's head alone (its title, status, exit code, sizes); its bodies come here, one call at a time,
 * and take its place in the session's store, so everything that shows it shows it whole from then on.
 */
import * as React from "react";
import { heldOf, type HEvent, type SessionStore } from "@/api/harness";

/** The session whose events the views under it show: where a lean one's whole self is asked for. */
export const BodiesContext = React.createContext<SessionStore | null>(null);

/**
 * `children` with `e` whole: at once when it is, else once its body comes (`fallback` meanwhile, in
 * the place it will take). Without a session to ask (a view outside one), `e` as it is.
 */
export function Whole<E extends HEvent>({ e, fallback, children }: { e: E; fallback: React.ReactNode; children: (whole: E) => React.ReactNode }) {
  const store = React.useContext(BodiesContext);
  if (!store || !heldOf(e)) return <>{children(e)}</>;
  return (
    <React.Suspense fallback={fallback}>
      <Fetched store={store} e={e}>
        {children}
      </Fetched>
    </React.Suspense>
  );
}

function Fetched<E extends HEvent>({ store, e, children }: { store: SessionStore; e: E; children: (whole: E) => React.ReactNode }) {
  const got = React.use(store.body(e));
  if (typeof got === "string") return <div className="hx-dim text-[11px]">Couldn&apos;t load it: {got}</div>;
  return <>{children(got as E)}</>;
}

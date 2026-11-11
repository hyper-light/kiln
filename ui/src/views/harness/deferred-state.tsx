import type { ReactNode } from "react";
import { useSession, type SessionStore } from "@/api/harness";
import { deferredState, stateMetadata, type StateEntry, type StateStep } from "@/api/state-pages";
import { Button } from "@/components/ui/button";

const LABELS: Record<string, string> = {
  queue: "Queued messages",
  approvals: "Requests waiting on you",
  tasks: "Tasks",
  todos: "To-do list",
  mcp: "MCP servers",
  skills: "Skills",
  plugins: "Plugins",
  harness_status: "Harness status",
  control_values: "Settings",
  delivery: "Message delivery",
  limits: "Usage limits",
};
const labelFor = (field: string) => LABELS[field] ?? field.replaceAll("_", " ");
const stepName = (step: StateStep) => (step.type === "index" ? String(step.index + 1) : step.type === "key" ? step.key : step.id);

/** A deferred field remains explicitly unread, rather than inheriting its projection's empty default. */
export function DeferredState({
  store,
  field,
  label = labelFor(field),
  children,
  renderValue,
}: {
  store: SessionStore;
  field: string;
  label?: string;
  children?: ReactNode;
  renderValue?: (value: unknown, path: StateStep[]) => ReactNode;
}) {
  const state = useSession(store, (s) => s.state);
  const page = useSession(store, (s) => s.statePages[field]);
  const deferred = deferredState(state, field);
  if (!deferred) return children;
  const path = page?.read.path ?? [];
  const offset = page?.read.offset ?? 0;
  const read = (next: number, at: StateStep[] = path) => void store.readState(field, at, next);
  const entry = (e: StateEntry) => (
    <li key={JSON.stringify(e.key)}>
      <span>{stepName(e.key)}</span>{" "}
      {e.node ? (
        <>
          <span>Available in parts; actions need this complete item.</span>{" "}
          <Button size="xs" variant="secondary" onClick={() => read(0, [...path, e.key])}>
            Read
          </Button>
        </>
      ) : (
        (renderValue?.(e.value, [...path, e.key]) ?? <pre className="hx-output">{typeof e.value === "string" ? e.value : JSON.stringify(e.value, null, 2)}</pre>)
      )}
    </li>
  );
  const reply = page?.reply;
  const next = reply?.type === "page" || reply?.type === "text" ? reply.next : undefined;
  return (
    <section className="hx-stack" aria-label={label} aria-busy={page?.phase === "loading"}>
      <div className="hx-section-head">
        <span className="hx-eyebrow">
          {label} · {deferred.len} {deferred.kind === "text" ? "bytes" : "items"}
        </span>
        <Button size="xs" variant="secondary" disabled={page?.phase === "loading"} onClick={() => read(offset)}>
          {page?.phase === "error" ? "Retry" : page ? "Read again" : "Read"}
        </Button>
      </div>
      {!page && <p className="in-none">Available from the agent. Read one part at a time.</p>}
      {page?.phase === "loading" && <p className="in-none">Reading {label.toLowerCase()}…</p>}
      {page?.error && (
        <p className="hx-bad" role="alert">
          {page.error}
        </p>
      )}
      {path.length > 0 && (
        <div className="hx-section-head">
          <span>{path.map(stepName).join(" / ")}</span>
          <Button size="xs" variant="ghost" onClick={() => read(0, path.slice(0, -1))}>
            Back
          </Button>
        </div>
      )}
      {page?.phase === "ready" && reply?.type === "page" && <ul className="hx-stack">{reply.entries.map(entry)}</ul>}
      {page?.phase === "ready" && reply?.type === "text" && <pre className="hx-output">{reply.text}</pre>}
      {page?.phase === "ready" &&
        reply?.type === "value" &&
        (renderValue?.(reply.value, path) ?? <pre className="hx-output">{typeof reply.value === "string" ? reply.value : JSON.stringify(reply.value, null, 2)}</pre>)}
      {page?.phase === "ready" && reply?.type === "node" && <p className="in-none">This value must be read through its individual parts.</p>}
      {(offset > 0 || next != null) && (
        <div className="hx-section-head">
          <Button size="xs" variant="ghost" disabled={offset === 0 || page?.phase === "loading"} onClick={() => read(0)}>
            First part
          </Button>
          {next != null && (
            <Button size="xs" variant="secondary" disabled={page?.phase === "loading"} onClick={() => read(next)}>
              Next part
            </Button>
          )}
        </div>
      )}
    </section>
  );
}

/** Every omitted field has a bounded read affordance, including fields without a dedicated panel. */
export function DeferredFields({ store, except = [] }: { store: SessionStore; except?: string[] }) {
  const state = useSession(store, (s) => s.state);
  const fields = Object.keys(stateMetadata(state)?.deferred ?? {}).filter((field) => !except.includes(field));
  if (!fields.length) return null;
  return (
    <div className="hx-stack">
      {fields.map((field) => (
        <details key={field}>
          <summary>{labelFor(field)} · not loaded</summary>
          <DeferredState store={store} field={field} />
        </details>
      ))}
    </div>
  );
}

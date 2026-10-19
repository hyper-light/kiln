/**
 * Synthetics (docs/observe.md §10.4), Datadog's Synthetic API tests: an endpoint tried on a schedule
 * from the server and runners (its locations), HTTP, TCP, DNS or TLS, each run judged by its
 * assertions; a test that alerts owns a monitor. Each test's state at each location, a test tried
 * once from the server before it is kept, and run now from everywhere.
 */
import * as React from "react";
import { Pencil, Play, Plus, Trash2, X } from "lucide-react";
import { observeApi, type Synthetic, type SyntheticAssertion, type SyntheticAssertionType, type SyntheticIn, type SyntheticOperator, type SyntheticOutcome, type SyntheticType } from "@/api/observe";
import { Button } from "@/components/ui/button";
import { ConfirmDialog, Dialog, DialogContent } from "@/components/ui/dialog";
import { TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { PageIntro, SelectField, TextField } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { useRes } from "@/lib/data";
import { relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { Link } from "@/lib/router";
import { ObserveNav, syntheticsRes } from "./parts";

const TYPES: { value: SyntheticType; label: string }[] = [
  { value: "http", label: "HTTP" },
  { value: "tcp", label: "TCP" },
  { value: "dns", label: "DNS" },
  { value: "tls", label: "TLS certificate" },
];
const TYPE_WORD = Object.fromEntries(TYPES.map((t) => [t.value, t.label])) as Record<SyntheticType, string>;
/** The assertions each kind of test takes, and their words. */
const ASSERTS: Record<SyntheticType, SyntheticAssertionType[]> = {
  http: ["status_code", "response_time", "header", "body", "certificate_days"],
  tcp: ["response_time"],
  dns: ["response_time", "address"],
  tls: ["response_time", "certificate_days"],
};
const ASSERT_WORD: Record<SyntheticAssertionType, string> = {
  status_code: "Status code",
  response_time: "Response time (ms)",
  header: "Header",
  body: "Body",
  certificate_days: "Certificate's days left",
  address: "An address",
};
const NUMERIC: SyntheticAssertionType[] = ["status_code", "response_time", "certificate_days"];
const OPS_NUMBER: SyntheticOperator[] = ["is", "is_not", "less_than", "more_than"];
const OPS_TEXT: SyntheticOperator[] = ["is", "is_not", "contains", "does_not_contain", "matches", "does_not_match"];
const OP_WORD: Record<SyntheticOperator, string> = {
  is: "is",
  is_not: "is not",
  less_than: "less than",
  more_than: "more than",
  contains: "contains",
  does_not_contain: "does not contain",
  matches: "matches",
  does_not_match: "does not match",
};
const METHODS = ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];

const list = (s: string) =>
  s
    .split(/[\s,]+/)
    .map((x) => x.trim())
    .filter(Boolean);

/** Where a test points. */
function target(t: SyntheticIn): string {
  if (t.type === "http") return `${t.method ?? "GET"} ${t.url ?? ""}`;
  if (t.type === "dns") return t.host ?? "";
  return `${t.host ?? ""}:${t.port ?? (t.type === "tls" ? 443 : "")}`;
}

/** A test's state over its locations: passing, failing (how many), pending, paused. */
function overall(t: Synthetic): { tone: string; word: string } {
  if (t.paused) return { tone: "skipped", word: "Paused" };
  const ran = t.machines.filter((m) => m.state.passed !== null && m.state.passed !== undefined);
  const failing = ran.filter((m) => m.state.passed === false).length;
  if (ran.length === 0) return { tone: "pending", word: "Pending" };
  if (failing === 0) return { tone: "succeeded", word: "Passing" };
  return { tone: "failed", word: failing === t.machines.length ? "Failing" : `${failing} of ${t.machines.length} failing` };
}

export default function Synthetics() {
  const [editing, setEditing] = React.useState<Synthetic | "new" | null>(null);
  return (
    <div className="container-console page">
      <title>Synthetics · Observe · kiln</title>
      <PageIntro
        index={8}
        label="Observe"
        title="Synthetics"
        lead="Endpoints tried on a schedule from the server and runners, HTTP, TCP, DNS or a TLS certificate, each run judged by its assertions and written as series. A test that alerts owns a monitor that tells you when enough locations fail."
        actions={
          <Button onClick={() => setEditing("new")}>
            <Plus /> New test
          </Button>
        }
      />
      <ObserveNav />
      <Loadable label="the synthetic tests" fallback={<TableHold head={<Head />} rows={3} />}>
        <List onAdd={() => setEditing("new")} onEdit={setEditing} />
      </Loadable>
      {editing && <TestDialog test={editing === "new" ? null : editing} onClose={() => setEditing(null)} />}
    </div>
  );
}

function Head() {
  return (
    <tr>
      <th>State</th>
      <th>Test</th>
      <th className="hidden md:table-cell">Locations</th>
      <th className="hidden sm:table-cell">Last run</th>
      <th />
    </tr>
  );
}

function List({ onAdd, onEdit }: { onAdd: () => void; onEdit: (t: Synthetic) => void }) {
  const all = useRes(syntheticsRes).data;
  const [removing, setRemoving] = React.useState<Synthetic | null>(null);
  if (all.length === 0)
    return (
      <p className="empty-line py-12 text-center">
        None yet.{" "}
        <button type="button" className="text-link" onClick={onAdd}>
          Make one
        </button>{" "}
        to try an endpoint from the server and runners on a schedule.
      </p>
    );
  return (
    <>
      <div className="table-wrap">
        <table className="data-table">
          <thead>
            <Head />
          </thead>
          <tbody>
            {all.map((t) => {
              const o = overall(t);
              const failing = t.machines.find((m) => m.state.passed === false);
              const newest = [...t.machines].sort((a, b) => (b.state.last_ms ?? 0) - (a.state.last_ms ?? 0))[0];
              return (
                <tr key={t.name}>
                  <td className="w-1 whitespace-nowrap">
                    <span className="status">
                      <i className="dot" data-status={o.tone} aria-hidden="true" />
                      <span className="status-text" data-status={o.tone}>
                        {o.word}
                      </span>
                    </span>
                  </td>
                  <td className="truncate-cell">
                    <div className="truncate text-[13px] text-[#e4e5e7]">
                      {t.name}
                      {t.monitor_id && (
                        <Link to={`/observe/monitors/${encodeURIComponent(t.monitor_id)}`} className="ml-2 text-[12px] text-[#8c8f98] hover:text-white">
                          its monitor
                        </Link>
                      )}
                    </div>
                    <p className="truncate font-mono text-[11px] text-[#8c8f98]" title={target(t)}>
                      {TYPE_WORD[t.type]} · {target(t)}
                    </p>
                    {failing && (
                      <p className="truncate text-[12px] text-[#e2c4c6]" title={failing.state.failed.join("\n")}>
                        {failing.machine}: {failing.state.failed[0] ?? "failing"}
                      </p>
                    )}
                  </td>
                  <td className="hidden md:table-cell">
                    <div className="flex flex-wrap gap-x-3 gap-y-1">
                      {t.machines.map((m) => (
                        <span key={m.machine} className="status" title={m.state.failed.join("\n") || undefined}>
                          <i className="dot" data-status={m.state.passed === false ? "failed" : m.state.passed ? "succeeded" : "pending"} aria-hidden="true" />
                          <span className="text-[12px]">{m.machine}</span>
                        </span>
                      ))}
                      {t.machines.length === 0 && <span className="text-[12px] text-[#8c8f98]">{(t.locations ?? []).join(", ")}</span>}
                    </div>
                  </td>
                  <td className="hidden whitespace-nowrap sm:table-cell">
                    {newest?.state.last_ms ? (
                      <>
                        {relativeTime(newest.state.last_ms / 1000)}
                        <div className="text-[12px] text-[#8c8f98]">
                          {(newest.state.took_us / 1000).toFixed(1)} ms{newest.state.status ? ` · ${newest.state.status}` : ""}
                        </div>
                      </>
                    ) : (
                      "—"
                    )}
                  </td>
                  <td className="w-1 whitespace-nowrap">
                    <Button
                      variant="ghost"
                      size="xs"
                      aria-label={`Run ${t.name} now`}
                      onClick={async () => {
                        try {
                          await observeApi.runSynthetic(t.name);
                          toast({ title: `Running ${t.name} from every location`, tone: "info" });
                          window.setTimeout(() => invalidate("observe/synthetics"), 3000);
                        } catch (e) {
                          toast({ title: `Couldn't run ${t.name}`, description: errorMessage(e), tone: "failed" });
                        }
                      }}
                    >
                      <Play /> <span className="hidden sm:inline">Run now</span>
                    </Button>
                    <Button variant="ghost" size="xs" aria-label={`Edit ${t.name}`} onClick={() => onEdit(t)}>
                      <Pencil />
                    </Button>
                    <Button variant="ghost-danger" size="xs" aria-label={`Remove ${t.name}`} onClick={() => setRemoving(t)}>
                      <Trash2 />
                    </Button>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
      <ConfirmDialog
        open={removing !== null}
        onOpenChange={(o) => !o && setRemoving(null)}
        title={`Remove ${removing?.name ?? ""}?`}
        description="It runs no more, from anywhere, and the monitor it owns goes with it; its series stay, until their retention."
        confirmLabel="Remove"
        danger
        onConfirm={async () => {
          if (!removing) return;
          try {
            await observeApi.deleteSynthetic(removing.name);
            invalidate("observe/synthetics");
          } catch (e) {
            toast({ title: "Couldn't remove it", description: errorMessage(e), tone: "failed" });
          }
        }}
      />
    </>
  );
}

/** An assertion as the form holds it: its target as text. */
interface AssertRow {
  type: SyntheticAssertionType;
  operator: SyntheticOperator;
  property: string;
  target: string;
}

interface Form {
  name: string;
  type: SyntheticType;
  url: string;
  method: string;
  body: string;
  follow: string;
  host: string;
  port: string;
  ca: string;
  asserts: AssertRow[];
  locations: string;
  every: string;
  timeout: string;
  retries: string;
  /** "no" or "yes": a monitor of its own. */
  alert: string;
  message: string;
  minLocations: string;
  forS: string;
}

function rowOf(a: SyntheticAssertion): AssertRow {
  return { type: a.type, operator: a.operator, property: a.property ?? "", target: String(a.target) };
}

function formOf(t: Synthetic | null): Form {
  return {
    name: t?.name ?? "",
    type: t?.type ?? "http",
    url: t?.url ?? "",
    method: t?.method ?? "GET",
    body: t?.body ?? "",
    follow: t?.follow_redirects === false ? "no" : "yes",
    host: t?.host ?? "",
    port: t?.port ? String(t.port) : "",
    ca: t?.ca ?? "",
    asserts: (t?.assertions ?? (t ? [] : [{ type: "status_code", operator: "is", target: 200 }])).map(rowOf),
    locations: (t?.locations ?? ["server"]).join(", "),
    every: String(t?.every_s ?? 60),
    timeout: String(t?.timeout_s ?? 30),
    retries: String(t?.retries ?? 0),
    alert: t?.alert ? "yes" : "no",
    message: t?.alert?.message ?? "",
    minLocations: String(t?.alert?.min_locations ?? 1),
    forS: String(t?.alert?.for_s ?? 0),
  };
}

/** The test as it is sent; settings only the API says (headers, a token, tags) kept as they were. */
function bodyOf(f: Form, was: Synthetic | null): SyntheticIn {
  const assertions: SyntheticAssertion[] = f.asserts
    .filter((a) => ASSERTS[f.type].includes(a.type))
    .map((a) => ({ type: a.type, operator: a.operator, ...(a.type === "header" ? { property: a.property.trim() } : {}), target: NUMERIC.includes(a.type) ? Number(a.target) : a.target }));
  const common = {
    assertions,
    locations: list(f.locations),
    every_s: Number(f.every) || 60,
    timeout_s: Number(f.timeout) || 30,
    retries: Number(f.retries) || 0,
    retry_ms: was?.retry_ms,
    tags: was?.tags,
    paused: was?.paused,
    alert: f.alert === "yes" ? { message: f.message, min_locations: Number(f.minLocations) || 1, for_s: Number(f.forS) || 0, priority: was?.alert?.priority } : undefined,
  };
  switch (f.type) {
    case "http":
      return {
        ...common,
        type: "http",
        url: f.url.trim(),
        method: f.method,
        body: f.body ? f.body : undefined,
        follow_redirects: f.follow === "yes",
        headers: was?.type === "http" ? was.headers : undefined,
        bearer: was?.type === "http" && was.bearer ? "…" : undefined,
        ca: f.ca.trim() || undefined,
      };
    case "tls":
      return { ...common, type: "tls", host: f.host.trim(), port: Number(f.port) || 443, ca: f.ca.trim() || undefined };
    case "tcp":
      return { ...common, type: "tcp", host: f.host.trim(), port: Number(f.port) };
    case "dns":
      return { ...common, type: "dns", host: f.host.trim() };
  }
}

function outcomeText(o: SyntheticOutcome): string {
  const parts = (
    [
      ["DNS", o.dns_ms],
      ["connect", o.connect_ms],
      ["TLS", o.tls_ms],
      ["first byte", o.firstbyte_ms],
      ["download", o.download_ms],
    ] as [string, number | null][]
  )
    .filter(([, v]) => v !== null && v !== undefined)
    .map(([k, v]) => `${k} ${(v ?? 0).toFixed(1)}`);
  const what = [
    o.status ? `${o.status}` : "",
    o.certificate_days !== null && o.certificate_days !== undefined ? `certificate ${Math.floor(o.certificate_days)} days left` : "",
    o.addresses.length && !o.status ? o.addresses.join(", ") : "",
  ]
    .filter(Boolean)
    .join(" · ");
  return `${o.passed ? "Passed" : "Failed"} in ${o.total_ms.toFixed(1)} ms${parts.length ? ` (${parts.join(", ")})` : ""}${what ? ` · ${what}` : ""}`;
}

function TestDialog({ test, onClose }: { test: Synthetic | null; onClose: () => void }) {
  const [f, setF] = React.useState<Form>(() => formOf(test));
  const [busy, setBusy] = React.useState(false);
  const [tried, setTried] = React.useState<SyntheticOutcome | { error: string } | null>(null);
  const set = (patch: Partial<Form>) => {
    setF((old) => ({ ...old, ...patch }));
    setTried(null);
  };
  const setAssert = (i: number, patch: Partial<AssertRow>) => set({ asserts: f.asserts.map((a, j) => (j === i ? { ...a, ...patch } : a)) });
  const validName = /^[A-Za-z0-9_.-]{1,64}$/.test(f.name);
  const where = f.type === "http" ? /^https?:\/\/\S+$/.test(f.url.trim()) : f.host.trim() !== "" && (f.type !== "tcp" || /^\d+$/.test(f.port));
  const assertsOk = f.asserts.every(
    (a) => !ASSERTS[f.type].includes(a.type) || (a.target.trim() !== "" && (!NUMERIC.includes(a.type) || !Number.isNaN(Number(a.target))) && (a.type !== "header" || a.property.trim() !== "")),
  );
  const ok = validName && where && assertsOk && list(f.locations).length > 0 && Number(f.every) >= 10 && Number(f.timeout) >= 1 && Number(f.timeout) <= 60;
  const save = async () => {
    setBusy(true);
    try {
      await observeApi.putSynthetic(f.name, bodyOf(f, test));
      invalidate("observe/synthetics");
      onClose();
    } catch (err) {
      toast({ title: "Couldn't save it", description: errorMessage(err), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };
  const tryIt = async () => {
    setBusy(true);
    try {
      setTried(await observeApi.testSynthetic(f.name || "new", bodyOf(f, test)));
    } catch (err) {
      setTried({ error: errorMessage(err) });
    } finally {
      setBusy(false);
    }
  };
  const kinds = ASSERTS[f.type];
  const firstKind: SyntheticAssertionType = kinds[0] ?? "response_time";
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent title={test ? `Edit ${test.name}` : "A new synthetic test"} description="Tried from each location on its schedule; Test tries it once from the server, nothing kept.">
        <form
          className="space-y-4"
          onSubmit={(e) => {
            e.preventDefault();
            if (ok) void save();
          }}
        >
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
            <TextField
              label="Name"
              value={f.name}
              readOnly={test !== null}
              onChange={(e) => set({ name: e.target.value })}
              placeholder="api-health"
              hint={f.name && !validName ? "Letters, digits, - _ and . only" : undefined}
            />
            <SelectField label="Type" value={f.type} onChange={(v) => set({ type: v as SyntheticType })} options={TYPES} />
          </div>
          {f.type === "http" ? (
            <>
              <div className="grid grid-cols-1 gap-3 sm:grid-cols-[8rem_1fr]">
                <SelectField label="Method" value={f.method} onChange={(v) => set({ method: v })} options={METHODS.map((m) => ({ value: m, label: m }))} />
                <TextField label="URL" value={f.url} onChange={(e) => set({ url: e.target.value })} placeholder="https://api.example.com/health" spellCheck={false} />
              </div>
              {["POST", "PUT", "PATCH"].includes(f.method) && (
                <div>
                  <label htmlFor="synthetic-body" className="field-label">
                    Body
                  </label>
                  <div className="field">
                    <textarea id="synthetic-body" rows={3} className="font-mono" value={f.body} onChange={(e) => set({ body: e.target.value })} spellCheck={false} />
                  </div>
                </div>
              )}
              <SelectField
                label="Redirects"
                value={f.follow}
                onChange={(v) => set({ follow: v })}
                options={[
                  { value: "yes", label: "Followed" },
                  { value: "no", label: "Not followed" },
                ]}
              />
            </>
          ) : (
            <div className="grid grid-cols-1 gap-3 sm:grid-cols-[1fr_8rem]">
              <TextField label="Host" value={f.host} onChange={(e) => set({ host: e.target.value })} placeholder={f.type === "dns" ? "api.example.com" : "db.internal"} spellCheck={false} />
              {f.type !== "dns" && (
                <TextField
                  label={f.type === "tls" ? "Port (optional)" : "Port"}
                  inputMode="numeric"
                  value={f.port}
                  onChange={(e) => set({ port: e.target.value })}
                  placeholder={f.type === "tls" ? "443" : "5432"}
                />
              )}
            </div>
          )}
          {(f.type === "tls" || (f.type === "http" && f.url.startsWith("https://"))) && (
            <div>
              <label htmlFor="synthetic-ca" className="field-label">
                Its CA (optional)
              </label>
              <div className="field">
                <textarea id="synthetic-ca" rows={2} className="font-mono" value={f.ca} onChange={(e) => set({ ca: e.target.value })} placeholder="-----BEGIN CERTIFICATE-----" spellCheck={false} />
              </div>
              <div className="field-hint">When its certificate is not a public one.</div>
            </div>
          )}
          <div>
            <div className="field-label">Assertions</div>
            <div className="space-y-2">
              {f.asserts.map((a, i) =>
                kinds.includes(a.type) ? (
                  <div key={i} className="grid grid-cols-[1fr_auto] gap-2 sm:grid-cols-[11rem_9rem_1fr_auto]">
                    <SelectField
                      label={`Assertion ${i + 1}`}
                      value={a.type}
                      onChange={(v) => setAssert(i, { type: v as SyntheticAssertionType, operator: NUMERIC.includes(v as SyntheticAssertionType) ? "less_than" : "contains" })}
                      options={kinds.map((k) => ({ value: k, label: ASSERT_WORD[k] }))}
                    />
                    <Button variant="ghost" size="xs" className="self-end sm:order-last" aria-label={`Remove assertion ${i + 1}`} onClick={() => set({ asserts: f.asserts.filter((_, j) => j !== i) })}>
                      <X />
                    </Button>
                    <SelectField
                      label="Operator"
                      value={a.operator}
                      onChange={(v) => setAssert(i, { operator: v as SyntheticOperator })}
                      options={(NUMERIC.includes(a.type) ? OPS_NUMBER : OPS_TEXT).map((o) => ({ value: o, label: OP_WORD[o] }))}
                    />
                    <div className="col-span-2 grid grid-cols-1 gap-2 sm:col-span-1">
                      {a.type === "header" && (
                        <TextField label="Header" value={a.property} onChange={(e) => setAssert(i, { property: e.target.value })} placeholder="content-type" spellCheck={false} />
                      )}
                      <TextField
                        label="Target"
                        value={a.target}
                        inputMode={NUMERIC.includes(a.type) ? "numeric" : undefined}
                        onChange={(e) => setAssert(i, { target: e.target.value })}
                        spellCheck={false}
                      />
                    </div>
                  </div>
                ) : null,
              )}
              <Button
                variant="ghost"
                size="xs"
                onClick={() => set({ asserts: [...f.asserts, { type: firstKind, operator: NUMERIC.includes(firstKind) ? "less_than" : "contains", property: "", target: "" }] })}
              >
                <Plus /> Add assertion
              </Button>
              {f.type === "http" && f.asserts.length === 0 && <p className="field-hint">None: a status under 400 passes.</p>}
            </div>
          </div>
          <TextField label="Locations" value={f.locations} onChange={(e) => set({ locations: e.target.value })} hint="server, runners by name, or * for every machine." spellCheck={false} />
          <div className="grid grid-cols-3 gap-3">
            <TextField label="Every (s)" inputMode="numeric" value={f.every} onChange={(e) => set({ every: e.target.value })} hint={Number(f.every) < 10 ? "10 at the least" : undefined} />
            <TextField label="Timeout (s)" inputMode="numeric" value={f.timeout} onChange={(e) => set({ timeout: e.target.value })} />
            <TextField label="Retries" inputMode="numeric" value={f.retries} onChange={(e) => set({ retries: e.target.value })} />
          </div>
          <SelectField
            label="Alert"
            value={f.alert}
            onChange={(v) => set({ alert: v })}
            options={[
              { value: "no", label: "No monitor" },
              { value: "yes", label: "A monitor of its own" },
            ]}
          />
          {f.alert === "yes" && (
            <>
              <div className="grid grid-cols-2 gap-3">
                <TextField label="When locations failing" inputMode="numeric" value={f.minLocations} onChange={(e) => set({ minLocations: e.target.value })} hint="At least this many" />
                <TextField label="For (s)" inputMode="numeric" value={f.forS} onChange={(e) => set({ forS: e.target.value })} hint="0: at once" />
              </div>
              <div>
                <label htmlFor="synthetic-message" className="field-label">
                  Message
                </label>
                <div className="field">
                  <textarea id="synthetic-message" rows={2} value={f.message} onChange={(e) => set({ message: e.target.value })} placeholder="The API is down @slack-oncall" />
                </div>
              </div>
            </>
          )}
          {tried && (
            <div className="field-hint" role="status">
              {"error" in tried ? (
                <span className="text-[#e2c4c6]">{tried.error}</span>
              ) : (
                <>
                  <span className={tried.passed ? undefined : "text-[#e2c4c6]"}>{outcomeText(tried)}</span>
                  {tried.failures.map((x) => (
                    <span key={x} className="block truncate font-mono text-[#e2c4c6]" title={x}>
                      {x}
                    </span>
                  ))}
                </>
              )}
            </div>
          )}
          <div className="flex justify-end gap-2">
            <Button variant="ghost" onClick={onClose}>
              Cancel
            </Button>
            <Button variant="ghost" disabled={!where || busy} onClick={() => void tryIt()}>
              Test
            </Button>
            <Button type="submit" disabled={!ok || busy}>
              {test ? "Save" : "Make it"}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}

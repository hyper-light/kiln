/**
 * Found on your network: every kiln machine discovery knows, the ones not set up here first, each
 * with the one thing to do about it (invite it, approve it, open it). Scanning and asking one address
 * happen in place above the list. Waiting to join: requests from machines discovery doesn't hear.
 */
import * as React from "react";
import { Link } from "react-router";
import { ArrowDown, ArrowRight, ArrowUpRight, Check, Pencil, Plus, Radar, Send, X } from "lucide-react";
import { api, discoveryApi } from "@/api/client";
import type { DiscoveredInstance, DiscoveryInfo, JoinRequest, ScanState } from "@/api/types";
import { Button } from "@/components/ui/button";
import { SectionHeading } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { errorMessage, useNow } from "@/lib/hooks";
import type { SuspenseResource } from "@/lib/resource";
import { Beam } from "@/views/harness/signals";
import { RelTime } from "@/views/shared";
import { STAND_RANK, STAND_WORD, VIA_TITLE, addressOf, cidrProblem, displayName, formatCode, jumpTo, roleWord, sortInstances, viaWord, type Stand } from "./data";
import { CopyText, Fold } from "./parts";

type Probe = { address: string; state: "asking" } | { address: string; state: "done"; found: DiscoveredInstance[] } | { address: string; state: "error"; error: string };

export function FoundSection({ index, discovery, requests, onRequestsChanged }: { index: string; discovery: SuspenseResource<DiscoveryInfo>; requests: JoinRequest[]; onRequestsChanged: () => void }) {
  const d = discovery.data;
  const [probe, setProbe] = React.useState<Probe | null>(null);
  const { setData, reload } = discovery;

  const patch = React.useCallback(
    (id: string, p: Partial<DiscoveredInstance>) => setData((prev) => ({ ...prev, instances: prev.instances.map((i) => (i.id === id ? { ...i, ...p } : i)) })),
    [setData],
  );
  const changed = React.useCallback(() => {
    reload();
    onRequestsChanged();
  }, [reload, onRequestsChanged]);

  const selfId = d.self.id;
  const nativeLabel = d.methods?.find((m) => m.id === "native")?.label;
  const byId = new Map(d.instances.map((i) => [i.id, i]));
  const reqById = new Map(requests.map((r) => [r.id, r]));
  // What a probe found is shown under the address field; the list below leaves it out meanwhile.
  const probed = probe?.state === "done" ? probe.found.map((i) => byId.get(i.id) ?? i) : [];
  const probedIds = new Set(probed.map((i) => i.id));
  const sorted = sortInstances(d.instances.filter((i) => !probedIds.has(i.id)), selfId);
  const fresh = sorted.filter((x) => STAND_RANK[x.stand] < STAND_RANK.here);
  const here = sorted.filter((x) => STAND_RANK[x.stand] >= STAND_RANK.here);
  const heard = d.instances.filter((i) => i.id !== selfId).length;
  const asking = fresh.filter((x) => x.stand === "asking").length + sortInstances(probed, selfId).filter((x) => x.stand === "asking").length;
  const problems = d.problems?.length ?? 0;

  const row = ({ i, stand }: { i: DiscoveredInstance; stand: Stand }) => (
    <InstanceRow
      key={i.id}
      inst={i}
      stand={stand}
      request={i.request ? reqById.get(i.request) : undefined}
      nativeLabel={nativeLabel}
      canInvite={d.join_requests !== false}
      onPatch={patch}
      onChanged={changed}
    />
  );

  return (
    <section className="am-section" aria-labelledby="am-found">
      <SectionHeading
        index={index}
        label="Nearby"
        title={<span id="am-found">Found on your network</span>}
        aside={
          <span className="am-aside">
            {heard} heard{asking > 0 && <span className="am-aside-warn"> · {asking} asking</span>}
          </span>
        }
      />
      {
        <>
          {d.subnets !== undefined && (
            <div className="am-tools">
              <ScanForm d={d} onStarted={(s) => setData((prev) => ({ ...prev, scan: s }))} />
              <ProbeForm probe={probe} onProbe={setProbe} />
            </div>
          )}
          <ScanLine scan={d.scan} />
          <span className="sr-only" role="status">
            {scanNews(d.scan)}
          </span>
          {probe && <ProbeResult probe={probe} found={sortInstances(probed, selfId)} row={row} onClose={() => setProbe(null)} />}
          {(!d.enabled || !d.browse) && (
            <p className="am-hint">
              Discovery is off in this server&apos;s config (<code>[discovery]</code>): it doesn&apos;t listen for other machines. A scan or an address still finds them.
            </p>
          )}
          {asking > 0 && <p className="am-hint">Approve a machine only when its code matches the one it shows.</p>}
          {fresh.length > 0 ? (
            <ul className="am-list">{fresh.map(row)}</ul>
          ) : (
            <div className="am-empty">
              <span>{here.some((x) => x.stand !== "self") ? "Nothing new heard on this network yet" : "Nothing else heard on this network yet"}</span>
              {(d.methods || d.dns_sd) && <JumpLink to="am-health">Discovery health</JumpLink>}
              {problems > 0 && (
                <JumpLink to="am-problems" tone="warn">
                  {problems} problem{problems === 1 ? "" : "s"}
                </JumpLink>
              )}
            </div>
          )}
          {here.length > 0 && (
            <Fold
              className="am-here"
              label={
                <>
                  Already here <span className="am-count">{here.length}</span>
                </>
              }
            >
              <ul className="am-list">{here.map(row)}</ul>
            </Fold>
          )}
        </>
      }
    </section>
  );
}

function JumpLink({ to, tone, children }: { to: string; tone?: "warn"; children: React.ReactNode }) {
  return (
    <a
      className="am-act"
      data-tone={tone}
      href={`#${to}`}
      onClick={(e) => {
        e.preventDefault();
        jumpTo(to, "start");
      }}
    >
      {children} <ArrowDown aria-hidden="true" />
    </a>
  );
}

/* ------------------------------------------------------------------ scan */

/** Scan network, on the subnets shown beside it as mono text; tapping that text edits it in place. */
function ScanForm({ d, onStarted }: { d: DiscoveryInfo; onStarted: (s: ScanState) => void }) {
  const [text, setText] = React.useState<string | null>(null);
  const value = text ?? (d.subnets ?? []).join(", ");
  const [editing, setEditing] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  const [starting, setStarting] = React.useState(false);
  const running = Boolean(d.scan?.running);
  const id = React.useId();
  const edit = React.useRef<HTMLButtonElement>(null);

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (running || starting) return;
    const cidrs = value.split(/[\s,]+/).filter(Boolean);
    const bad = cidrs.map(cidrProblem).find((p) => p !== null);
    if (bad) {
      setError(bad);
      setEditing(true);
      return;
    }
    setError(null);
    setEditing(false);
    setStarting(true);
    try {
      onStarted(await discoveryApi.scan(cidrs));
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setStarting(false);
    }
  };

  return (
    <form className="am-scan" onSubmit={submit}>
      <Button type="submit" variant="secondary" size="xs" disabled={running || starting}>
        <Radar aria-hidden="true" /> {running ? "Scanning…" : "Scan network"}
      </Button>
      {editing ? (
        <>
          <label htmlFor={`${id}-cidrs`} className="sr-only">
            Subnets to scan, at most a /22 each
          </label>
          <input
            id={`${id}-cidrs`}
            className="am-inline-input"
            autoFocus
            value={value}
            size={Math.max(15, value.length + 1)}
            placeholder="192.168.1.0/24"
            spellCheck={false}
            autoComplete="off"
            autoCapitalize="off"
            aria-invalid={error ? true : undefined}
            aria-describedby={error ? `${id}-err` : undefined}
            onChange={(e) => {
              setText(e.target.value);
              setError(null);
            }}
            onBlur={() => !error && setEditing(false)}
            onKeyDown={(e) => {
              if (e.key !== "Escape") return;
              e.preventDefault();
              setText(null);
              setError(null);
              setEditing(false);
              window.setTimeout(() => edit.current?.focus(), 0);
            }}
          />
        </>
      ) : (
        <button ref={edit} type="button" className="am-subnets" onClick={() => setEditing(true)} aria-label={`Subnets to scan: ${value || "the detected ones"}. Change`}>
          {value || "detected subnets"}
          <Pencil aria-hidden="true" />
        </button>
      )}
      {error && (
        <p id={`${id}-err`} className="am-error" role="alert">
          {error}
        </p>
      )}
    </form>
  );
}

/** The latest scan as one line: a beam and its count while it runs, what it found once it's done. */
function ScanLine({ scan }: { scan: ScanState | null | undefined }) {
  const now = useNow();
  if (!scan) return null;
  const recent = scan.finished_at !== null && now / 1000 - scan.finished_at < 300;
  if (!scan.running && !recent) return null;
  const pct = scan.total ? Math.min(100, (scan.probed / scan.total) * 100) : 0;
  const cidrs = scan.cidrs.join(", ");
  return (
    <div className="am-progress" data-running={scan.running || undefined}>
      <div className="am-progress-line">
        {scan.running ? (
          <span aria-hidden="true">
            <Beam status="running" />
          </span>
        ) : (
          <i className="am-dot" data-state="active" aria-hidden="true" />
        )}
        {scan.running ? (
          <span>
            scanning {cidrs} · {scan.probed}/{scan.total} · {scan.found} found
          </span>
        ) : (
          <span>
            scanned {cidrs} · {scan.total} addresses · {scan.found} found · <RelTime ts={scan.finished_at} />
          </span>
        )}
      </div>
      {scan.running && (
        <div className="am-progress-track" role="progressbar" aria-label={`Scanning ${cidrs}`} aria-valuemin={0} aria-valuemax={scan.total} aria-valuenow={scan.probed}>
          <span style={{ width: `${pct}%` }} />
        </div>
      )}
    </div>
  );
}

/** What a screen reader hears about a scan: its start, every quarter, its end. Not every address. */
function scanNews(scan: ScanState | null | undefined): string {
  if (!scan) return "";
  const found = `${scan.found} found`;
  if (scan.running) {
    const quarter = scan.total ? Math.floor((scan.probed / scan.total) * 4) * 25 : 0;
    return `Scanning ${scan.cidrs.join(", ")}: ${quarter}% done, ${found}.`;
  }
  return scan.finished_at && Date.now() / 1000 - scan.finished_at < 300 ? `Scan finished: ${found}.` : "";
}

/* ------------------------------------------------------------------ one address */

function ProbeForm({ probe, onProbe }: { probe: Probe | null; onProbe: (p: Probe | null) => void }) {
  const [open, setOpen] = React.useState(false);
  const [address, setAddress] = React.useState("");
  const toggle = React.useRef<HTMLButtonElement>(null);
  // An answer that comes back after the field was closed (or asked again) is dropped.
  const seq = React.useRef(0);
  const id = React.useId();
  const asking = probe?.state === "asking";

  const close = () => {
    seq.current++;
    setOpen(false);
    onProbe(null);
    window.setTimeout(() => toggle.current?.focus(), 0);
  };

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    const a = address.trim();
    if (!a || asking) return;
    const n = ++seq.current;
    onProbe({ address: a, state: "asking" });
    try {
      const { found } = await discoveryApi.probe(a);
      if (n === seq.current) onProbe({ address: a, state: "done", found });
    } catch (err) {
      if (n === seq.current) onProbe({ address: a, state: "error", error: errorMessage(err) });
    }
  };

  if (!open) {
    return (
      <button ref={toggle} type="button" className="am-act" onClick={() => setOpen(true)}>
        <Plus aria-hidden="true" /> Add by address
      </button>
    );
  }
  return (
    <form className="am-probe" onSubmit={submit}>
      <label htmlFor={`${id}-addr`} className="sr-only">
        Address or host name, with :port if it isn&apos;t 7070
      </label>
      <input
        id={`${id}-addr`}
        className="am-inline-input am-probe-input"
        autoFocus
        value={address}
        placeholder="10.0.4.12 or build-box:7070"
        spellCheck={false}
        autoComplete="off"
        autoCapitalize="off"
        onChange={(e) => setAddress(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Escape") {
            e.preventDefault();
            close();
          }
        }}
      />
      <button type="submit" className="am-act" disabled={!address.trim() || asking}>
        {asking ? "Asking…" : "Probe"} <ArrowRight aria-hidden="true" />
      </button>
      <button type="button" className="am-act" data-tone="quiet" aria-label="Close add by address" onClick={close}>
        <X aria-hidden="true" />
      </button>
    </form>
  );
}

function ProbeResult({ probe, found, row, onClose }: { probe: Probe; found: { i: DiscoveredInstance; stand: Stand }[]; row: (x: { i: DiscoveredInstance; stand: Stand }) => React.ReactNode; onClose: () => void }) {
  return (
    <div className="am-probed" aria-live="polite">
      {probe.state === "asking" ? (
        <p className="am-probe-line">
          <span aria-hidden="true">
            <Beam status="running" />
          </span>
          Asking <code>{probe.address}</code>…
        </p>
      ) : probe.state === "error" ? (
        <p className="am-probe-line am-error">
          {probe.error}
          <button type="button" className="am-act" data-tone="quiet" onClick={onClose}>
            Dismiss
          </button>
        </p>
      ) : found.length === 0 ? (
        <p className="am-probe-line">
          No kiln answered at <code>{probe.address}</code>. It listens on 7070: add <code>:port</code> if that one is different.
        </p>
      ) : (
        <>
          <p className="am-probe-line">
            At <code>{probe.address}</code>:
          </p>
          <ul className="am-list">{found.map(row)}</ul>
        </>
      )}
    </div>
  );
}

/* ------------------------------------------------------------------ rows */

function InstanceRow({
  inst: i,
  stand,
  request,
  nativeLabel,
  canInvite,
  onPatch,
  onChanged,
}: {
  inst: DiscoveredInstance;
  stand: Stand;
  request: JoinRequest | undefined;
  nativeLabel: string | undefined;
  canInvite: boolean;
  onPatch: (id: string, p: Partial<DiscoveredInstance>) => void;
  onChanged: () => void;
}) {
  const address = addressOf(i);
  const via = i.via ?? [];
  return (
    <li id={`di-${i.id}`} tabIndex={-1} className="am-row" data-stand={stand} data-wide={stand === "asking" || stand === "invited" || undefined}>
      <div className="am-main">
        <span className="am-name" title={i.instance}>
          {displayName(i)}
        </span>
        <span className="am-stand">
          <i aria-hidden="true" />
          {STAND_WORD[stand]}
        </span>
      </div>
      <div className="am-meta">
        <span className="am-facts">
          {roleWord(i.role)} · {i.os}/{i.arch} · v{i.version}
          {address ? ` · ${address}` : ""}
        </span>
        <span className="am-seen">
          {via.length > 0 && (
            <span className="am-tags">
              <span className="sr-only">found through </span>
              {via.map((v) => (
                <span key={v} className="am-tag" title={VIA_TITLE[v]}>
                  {viaWord(v, nativeLabel)}
                </span>
              ))}
            </span>
          )}
          <span className="am-when">
            seen <RelTime ts={i.last_seen} />
          </span>
        </span>
      </div>
      <div className="am-do">
        <InstanceAction inst={i} stand={stand} request={request} canInvite={canInvite} onPatch={onPatch} onChanged={onChanged} />
      </div>
    </li>
  );
}

function InstanceAction({
  inst: i,
  stand,
  request,
  canInvite,
  onPatch,
  onChanged,
}: {
  inst: DiscoveredInstance;
  stand: Stand;
  request: JoinRequest | undefined;
  canInvite: boolean;
  onPatch: (id: string, p: Partial<DiscoveredInstance>) => void;
  onChanged: () => void;
}) {
  const name = displayName(i);
  const [busy, setBusy] = React.useState(false);

  const invite = async (yes: boolean) => {
    setBusy(true);
    try {
      if (yes) await discoveryApi.invite(i.id);
      else await discoveryApi.uninvite(i.id);
      onPatch(i.id, { invited: yes });
      onChanged();
    } catch (e) {
      toast.error(yes ? `Couldn't invite ${name}` : "Couldn't cancel the invitation", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  switch (stand) {
    case "asking":
      return request ? <Decide request={request} onDecided={onChanged} /> : <span className="am-quiet">asking…</span>;
    case "fresh":
      return canInvite ? (
        <button type="button" className="am-act" disabled={busy} onClick={() => void invite(true)} aria-label={`Invite ${name}`} title="It asks to join; you approve it here with its code">
          <Send aria-hidden="true" /> {busy ? "Inviting…" : "Invite"}
        </button>
      ) : (
        <span className="am-quiet">join requests are off</span>
      );
    case "invited":
      return (
        <>
          <span className="am-quiet">Invited · waiting for it to ask</span>
          <button type="button" className="am-act" data-tone="quiet" disabled={busy} onClick={() => void invite(false)} aria-label={`Cancel the invitation to ${name}`}>
            Cancel
          </button>
        </>
      );
    case "server":
      return i.url ? (
        <a className="am-act" href={i.url} target="_blank" rel="noreferrer" aria-label={`Open ${name} (another kiln server)`}>
          Open <ArrowUpRight aria-hidden="true" />
        </a>
      ) : null;
    case "here":
      return i.agent ? (
        <Link className="am-act" to={`/runners?agent=${encodeURIComponent(i.agent)}`} aria-label={`Open runner ${i.agent}`}>
          {i.agent} <ArrowRight aria-hidden="true" />
        </Link>
      ) : null;
    case "self":
      return null;
    default:
      return <CopyText text={i.join_command} label="Copy join command" name={`Copy the join command for ${name}`} />;
  }
}

/** The verification code and the decision. The code is the check: approve only when the machine shows the same. */
function Decide({ request: r, onDecided }: { request: JoinRequest; onDecided: () => void }) {
  const [busy, setBusy] = React.useState<"approve" | "deny" | null>(null);
  const decide = async (what: "approve" | "deny") => {
    setBusy(what);
    try {
      if (what === "approve") {
        const res = await api.approveJoin(r.id);
        toast.success(`${res.name} approved`, "It connects as a runner in a moment.");
      } else {
        await api.denyJoin(r.id);
        toast({ title: `${r.name} denied`, tone: "info" });
      }
      onDecided();
    } catch (e) {
      toast.error(what === "approve" ? "Approval failed" : "Deny failed", errorMessage(e));
    } finally {
      setBusy(null);
    }
  };
  return (
    <>
      {r.code ? (
        <span className="am-code6">
          <span className="sr-only">Verification code {r.code.split("").join(" ")}</span>
          <small aria-hidden="true">code</small>
          <span aria-hidden="true">{formatCode(r.code)}</span>
        </span>
      ) : (
        <span className="am-quiet">waiting for it to confirm…</span>
      )}
      <button type="button" className="am-act" data-tone="ok" disabled={!r.code || busy !== null} onClick={() => void decide("approve")} aria-label={`Approve ${r.name}`}>
        <Check aria-hidden="true" /> {busy === "approve" ? "Approving…" : "Approve"}
      </button>
      <button type="button" className="am-act" data-tone="danger" disabled={busy !== null} onClick={() => void decide("deny")} aria-label={`Deny ${r.name}`}>
        {busy === "deny" ? "Denying…" : "Deny"}
      </button>
    </>
  );
}

/* ------------------------------------------------------------------ waiting to join */

/** Join requests from machines discovery doesn't hear (the ones it hears are asking in the list above). */
export function WaitingSection({ index, requests, onDecided }: { index: string; requests: JoinRequest[]; onDecided: () => void }) {
  return (
    <section className="am-section" aria-labelledby="am-waiting">
      <SectionHeading
        index={index}
        label="Requests"
        title={<span id="am-waiting">Waiting to join</span>}
        aside={
          <span className="am-aside">
            {requests.length} waiting
          </span>
        }
      />
      <p className="am-hint">
        Asking to join, but not heard on this network. Approve one only when its code matches the one <code>kiln join</code> shows on it.
      </p>
      <ul className="am-list">
        {requests.map((r) => (
          <RequestRow key={r.id} request={r} onDecided={onDecided} />
        ))}
      </ul>
    </section>
  );
}

function RequestRow({ request: r, onDecided }: { request: JoinRequest; onDecided: () => void }) {
  const now = useNow();
  const left = Math.floor((r.expires_at * 1000 - now) / 60_000);
  // The labels it asked for, less the ones that only repeat its system.
  const labels = r.labels.filter((l) => l !== r.os && l !== r.arch);
  return (
    <li id={`jr-${r.id}`} tabIndex={-1} className="am-row" data-stand="asking" data-wide="true">
      <div className="am-main">
        <span className="am-name">{r.name}</span>
        <span className="am-stand">
          <i aria-hidden="true" />
          asking to join
        </span>
      </div>
      <div className="am-meta">
        <span className="am-facts">
          {r.hostname && r.hostname !== r.name ? `${r.hostname} · ` : ""}
          {r.os}/{r.arch}
          {r.version ? ` · v${r.version}` : ""} · from {r.address}
          {labels.length > 0 && <span className="am-labels"> · {labels.join(" ")}</span>}
        </span>
        <span className="am-seen">
          <span className="am-when">
            asked <RelTime ts={r.created_at} /> · {left >= 1 ? `${left} min left` : "under a minute left"}
          </span>
        </span>
      </div>
      <div className="am-do">
        <Decide request={r} onDecided={onDecided} />
      </div>
    </li>
  );
}

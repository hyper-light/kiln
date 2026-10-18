/**
 * Plan usage and status: how much of each account's 5-hour and weekly limits are used and when
 * they reset, and the harness's own status (version, account, model, sandbox, MCP servers …).
 */
import * as React from "react";
import type { AgentInfo, Limit, StatusItem } from "@/api/harness";
import { SelectField } from "@/components/ui/misc";
import { cn } from "@/lib/utils";
import { useClock } from "./dom";
import { HarnessMark } from "./parts";

const tone = (pct: number | null) => (pct === null ? "none" : pct >= 90 ? "fail" : pct >= 70 ? "warn" : "ok");

const DAY = 86_400_000;

/** How long until then, in short words: "45m", "2h 5m", "3d 4h". */
function leftWords(ms: number): string {
  const m = Math.max(1, Math.round(ms / 60000));
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 48) return `${h}h ${m % 60}m`;
  return `${Math.floor(h / 24)}d ${h % 24}h`;
}

/** When a window starts over, in local time: "4:30 PM PDT", "tomorrow 9:00 AM PDT", "Tue, Oct 21, 9:00 AM PDT". */
export function resetWhen(at: number, now: number): string {
  const d = new Date(at);
  const time = d.toLocaleTimeString([], { hour: "numeric", minute: "2-digit", timeZoneName: "short" });
  const day = (ms: number) => new Date(ms).toDateString();
  if (day(at) === day(now)) return time;
  if (day(at) === day(now + DAY)) return `tomorrow ${time}`;
  return `${d.toLocaleDateString([], { weekday: "short", month: "short", day: "numeric" })}, ${time}`;
}

/** When it starts over and how long until then, as pieces that wrap whole. */
function resetsIn(at: number | undefined, now: number): string[] {
  if (!at) return [];
  const ms = at - now;
  if (ms <= 0) return ["Starting over now"];
  return [`Resets ${resetWhen(at, now)}`, `in ${leftWords(ms)}`];
}

const num = (n: number) => (Number.isInteger(n) ? String(n) : n.toFixed(2));

/** How much is used: "42%", "12.40 / 50 USD · 25%". */
function amount(l: Limit): string {
  const pct = l.used_percent !== null ? `${Math.round(l.used_percent)}%` : null;
  const of = l.used !== undefined && l.limit ? `${num(l.used)} / ${num(l.limit)}${l.unit ? ` ${l.unit}` : ""}` : null;
  return [of, pct].filter(Boolean).join(" · ") || "—";
}

export function LimitMeter({ limit, compact }: { limit: Limit; compact?: boolean }) {
  const now = useClock(30_000);
  const pct = limit.used_percent;
  const reset = resetsIn(limit.resets_at, now);
  // "5h": the session window, which its label ("Session") doesn't say the length of.
  const hours = /^(\d+)h$/.exec(limit.window ?? "")?.[1];
  const said = limit.status === "rejected" ? "Limit reached" : limit.status === "warning" ? "Close to the limit" : null;
  return (
    <div className={cn("hx-limit", compact && "hx-limit-compact")} data-tone={limit.status === "rejected" ? "fail" : tone(pct)}>
      <div className="hx-limit-top">
        <span className="hx-limit-label">
          {limit.label}
          {hours && !compact && <span className="hx-limit-window"> · {hours} hours</span>}
        </span>
        <span className="hx-meta-num">
          {said && <span className="hx-limit-said">{said} · </span>}
          {amount(limit)}
        </span>
      </div>
      <div className="hx-limit-bar" role="meter" aria-label={limit.label} aria-valuemin={0} aria-valuemax={100} aria-valuenow={pct ?? undefined}>
        <span style={{ width: `${Math.min(100, Math.max(0, pct ?? 0))}%` }} />
      </div>
      {reset.length > 0 && (
        <div className="hx-limit-reset">
          {reset.map((r) => (
            <span key={r}>{r}</span>
          ))}
        </div>
      )}
    </div>
  );
}

/**
 * The plan's limits in full, opened in place from the head's usage badge (no popup): how much of
 * each window is used, and when it starts over, in local time.
 */
export function UsagePanel({
  id,
  limits,
  plan,
  onMore,
  stopAt,
  onStopAt,
}: {
  id: string;
  limits: Limit[];
  plan?: string;
  onMore?: () => void;
  /** The agent stops between turns once a limit is this full (null: only when one is reached). */
  stopAt?: number | null;
  onStopAt?: (percent: number | null) => void;
}) {
  return (
    <section id={id} className="hx-usage-panel" aria-label="Plan usage">
      <div className="hx-usage-head">
        <span className="hx-eyebrow">Plan usage</span>
        {plan && <span className="hx-tag">{plan}</span>}
      </div>
      <div className="hx-usage-limits">
        {limits.map((l) => (
          <LimitMeter key={l.id} limit={l} />
        ))}
      </div>
      {onStopAt && (
        <SelectField
          className="hx-usage-stop"
          label="Stop between turns"
          value={stopAt ? String(stopAt) : ""}
          onChange={(v) => onStopAt(v ? Number(v) : null)}
          options={[{ value: "", label: "Only when a limit is reached" }, ...[80, 90, 95].map((p) => ({ value: String(p), label: `Once a limit is ${p}% used` }))]}
        />
      )}
      {onMore && (
        <button type="button" className="hx-usage-more" onClick={onMore}>
          This session&apos;s cost, context and status
        </button>
      )}
    </section>
  );
}

/** The most-used limit, for a header badge. */
export function topLimit(limits: Limit[] | undefined): Limit | null {
  return (limits ?? []).reduce<Limit | null>((best, l) => ((l.used_percent ?? -1) > (best?.used_percent ?? -1) ? l : best), null);
}

/** The most-used limit; it opens the rest (`controls`: the panel it opens, `open`: whether it is). */
export function LimitBadge({ limits, onClick, open, controls }: { limits: Limit[] | undefined; onClick?: () => void; open?: boolean; controls?: string }) {
  const top = topLimit(limits);
  if (!top || top.used_percent === null) return null;
  return (
    <button
      type="button"
      className="hx-limit-badge"
      data-tone={tone(top.used_percent)}
      onClick={onClick}
      aria-expanded={open}
      aria-controls={open ? controls : undefined}
      title={(limits ?? []).map((l) => `${l.label}: ${l.used_percent ?? "?"}%`).join(" · ")}
    >
      {top.label} {Math.round(top.used_percent)}%
    </button>
  );
}

/** The harness's status, by group. */
export function StatusList({ items }: { items: StatusItem[] }) {
  const groups = React.useMemo(() => {
    const m = new Map<string, StatusItem[]>();
    for (const it of items) m.set(it.group || "Harness", [...(m.get(it.group || "Harness") ?? []), it]);
    return [...m.entries()];
  }, [items]);
  return (
    <div className="hx-status">
      {groups.map(([g, its]) => (
        <section key={g}>
          <div className="hx-eyebrow">{g}</div>
          <dl>
            {its.map((it, i) => (
              <div key={i} className="hx-status-row" data-tone={it.tone}>
                <dt>{it.label}</dt>
                <dd>{it.value}</dd>
              </div>
            ))}
          </dl>
        </section>
      ))}
    </div>
  );
}

/**
 * Mission control's account strip: every account your agents run as, with its limits. Agents that
 * share an account share its limits, so each account shows once (its freshest report).
 */
export function AccountLimits({ agents }: { agents: AgentInfo[] }) {
  const accounts = React.useMemo(() => {
    const m = new Map<string, { kind: string; kindName: string; plan?: string; limits: Map<string, Limit>; agents: number; at: number }>();
    for (const a of agents) {
      if (!a.limits?.length) continue;
      const key = a.account ?? a.limits[0]?.account ?? `${a.kind}:${a.target}`;
      const cur = m.get(key) ?? { kind: a.kind, kindName: a.kind_name, plan: a.plan, limits: new Map<string, Limit>(), agents: 0, at: 0 };
      cur.agents += 1;
      if (a.last_activity >= cur.at) {
        cur.at = a.last_activity;
        for (const l of a.limits) cur.limits.set(l.id, l);
        cur.plan = a.plan ?? cur.plan;
      }
      m.set(key, cur);
    }
    return [...m.entries()];
  }, [agents]);
  if (accounts.length === 0) return null;
  return (
    <section className="hx-accounts" aria-label="Plan usage">
      {accounts.map(([key, acc]) => (
        <div key={key} className="hx-account">
          <div className="hx-account-head">
            <HarnessMark kind={acc.kind} name={acc.kindName} size={20} />
            <span>{acc.kindName}</span>
            {acc.plan && <span className="hx-tag">{acc.plan}</span>}
            <span className="hx-dim ml-auto text-[11px]">
              {acc.agents} agent{acc.agents === 1 ? "" : "s"}
            </span>
          </div>
          <div className="hx-account-limits">
            {[...acc.limits.values()].map((l) => (
              <LimitMeter key={l.id} limit={l} compact />
            ))}
          </div>
        </div>
      ))}
    </section>
  );
}

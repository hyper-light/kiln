/**
 * Plan usage and status: how much of each account's 5-hour and weekly limits are used and when
 * they reset, and the harness's own status (version, account, model, sandbox, MCP servers …).
 */
import * as React from "react";
import type { AgentInfo, Limit, StatusItem } from "@/api/harness";
import { cn } from "@/lib/utils";
import { HarnessMark } from "./parts";

const tone = (pct: number | null) => (pct === null ? "none" : pct >= 90 ? "fail" : pct >= 70 ? "warn" : "ok");

function resetsIn(at: number | undefined, now: number): string | null {
  if (!at) return null;
  const ms = at - now;
  if (ms <= 0) return "resetting";
  const m = Math.round(ms / 60000);
  if (m < 60) return `resets in ${m}m`;
  const h = Math.floor(m / 60);
  if (h < 48) return `resets in ${h}h ${m % 60}m`;
  return `resets ${new Date(at).toLocaleDateString([], { weekday: "short", hour: "2-digit", minute: "2-digit" })}`;
}

function useMinuteClock() {
  const [now, setNow] = React.useState(() => Date.now());
  React.useEffect(() => {
    const t = window.setInterval(() => setNow(Date.now()), 30_000);
    return () => window.clearInterval(t);
  }, []);
  return now;
}

export function LimitMeter({ limit, compact }: { limit: Limit; compact?: boolean }) {
  const now = useMinuteClock();
  const pct = limit.used_percent;
  const reset = resetsIn(limit.resets_at, now);
  return (
    <div className={cn("hx-limit", compact && "hx-limit-compact")} data-tone={tone(pct)}>
      <div className="hx-limit-top">
        <span className="hx-limit-label">{limit.label}</span>
        <span className="hx-meta-num">
          {pct !== null ? `${Math.round(pct)}%` : limit.used !== undefined && limit.limit ? `${limit.used} / ${limit.limit}${limit.unit ? ` ${limit.unit}` : ""}` : "—"}
        </span>
      </div>
      <div className="hx-limit-bar" role="meter" aria-label={limit.label} aria-valuemin={0} aria-valuemax={100} aria-valuenow={pct ?? undefined}>
        <span style={{ width: `${Math.min(100, Math.max(0, pct ?? 0))}%` }} />
      </div>
      {reset && <div className="hx-limit-reset">{reset}</div>}
    </div>
  );
}

/** The most-used limit, for a header badge. */
export function topLimit(limits: Limit[] | undefined): Limit | null {
  return (limits ?? []).reduce<Limit | null>((best, l) => ((l.used_percent ?? -1) > (best?.used_percent ?? -1) ? l : best), null);
}

export function LimitBadge({ limits, onClick }: { limits: Limit[] | undefined; onClick?: () => void }) {
  const top = topLimit(limits);
  if (!top || top.used_percent === null) return null;
  return (
    <button type="button" className="hx-limit-badge" data-tone={tone(top.used_percent)} onClick={onClick} title={(limits ?? []).map((l) => `${l.label}: ${l.used_percent ?? "?"}%`).join(" · ")}>
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

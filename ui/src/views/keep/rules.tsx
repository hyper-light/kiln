/**
 * One scope's rules for the copies runners keep of other runners' trees (docs/reach.md §5.1),
 * edited in place: each rule as the scope above says, none here, or set here; what they come to,
 * said under them; Save, and back to the scope above's.
 */
import * as React from "react";
import type { KeepRules } from "@/api/types";
import { Button } from "@/components/ui/button";
import { toast } from "@/components/ui/toast";
import { errorMessage } from "@/lib/hooks";
import { draftOf, hoursSaid, merge, rulesOf, rulesSaid, SCHEDULES, scheduleSaid, type Draft, type Mode } from "./words";
import "./keep.css";

interface Props {
  /** This scope's own rules. */
  rules: KeepRules;
  /** What the scopes above come to (none: it differs from copy to copy). */
  above?: KeepRules;
  /** Whose they are, as the "As …" choices say: "every runner's", "each runner's". */
  aboveName?: string;
  /** Every runner's: there's nothing above. */
  top?: boolean;
  /** Save these (none: the scope above's apply). */
  onSave: (rules: KeepRules | null) => Promise<void>;
  label: string;
}

/** The form, made again whenever the rules it shows were saved (its draft starts from them). */
export function RulesForm(props: Props) {
  return <Form key={JSON.stringify(props.rules)} {...props} />;
}

function Form({ rules, above, aboveName = "the scope above's", top = false, onSave, label }: Props) {
  const [draft, setDraft] = React.useState<Draft>(() => draftOf(rules, top));
  const [busy, setBusy] = React.useState(false);
  const made = rulesOf(draft, top);
  const changed = "rules" in made && JSON.stringify(made.rules) !== JSON.stringify(rules);
  const set = <K extends keyof Draft>(k: K, v: Draft[K]) => setDraft((d) => ({ ...d, [k]: v }));
  const save = async (r: KeepRules | null) => {
    setBusy(true);
    try {
      await onSave(r && Object.keys(r).length ? r : null);
      toast.success("Saved", "Runners have it within 20 seconds.");
    } catch (e) {
      toast.error("Couldn't save the rules", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  const as = (said: string | null) => (top ? null : above ? `As ${aboveName}: ${said ?? "none"}` : `As ${aboveName}`);
  const settled = "rules" in made ? (above ? merge(made.rules, above) : made.rules) : null;
  const zone = typeof Intl !== "undefined" ? Intl.DateTimeFormat().resolvedOptions().timeZone : "";
  const preset = draft.schedule.mode === "set" ? (SCHEDULES.some((s) => s.cron === draft.schedule.value) ? `cron:${draft.schedule.value}` : "custom") : draft.schedule.mode;
  return (
    <form
      className="kp-form"
      aria-label={label}
      onSubmit={(e) => {
        e.preventDefault();
        if ("rules" in made) void save(made.rules);
      }}
    >
      <Row label="Keep the newest" hint="Copies, by when each was last used: counted among this scope's own.">
        <Count field={draft.newest} unit="copies" none="No such rule" inherit={as(above?.newest ? `the newest ${above.newest}` : null)} onChange={(f) => set("newest", f)} />
      </Row>
      <Row label="Keep those used within" hint="How long since a sync into it or a run in it.">
        <Count
          field={draft.hours}
          unit={
            <select className="kp-select kp-unit" aria-label="Hours or days" value={draft.hours.unit} onChange={(e) => set("hours", { ...draft.hours, unit: e.target.value as "hours" | "days" })}>
              <option value="hours">hours</option>
              <option value="days">days</option>
            </select>
          }
          none="No such rule"
          inherit={as(above?.hours ? hoursSaid(above.hours) : null)}
          onChange={(f) => set("hours", { ...draft.hours, ...f })}
        />
      </Row>
      <Row label="A copy stays when" hint="With both rules set.">
        <select className="kp-select" aria-label="A copy stays when" value={draft.both} onChange={(e) => set("both", e.target.value as Draft["both"])}>
          {!top && <option value="inherit">{as(above ? (above.both ? "both keep it" : "either keeps it") : null)}</option>}
          <option value="either">Either rule keeps it</option>
          <option value="both">Both rules keep it</option>
        </select>
      </Row>
      <Row label="Let go on a schedule" hint="What the rules don't keep goes when it comes round.">
        <div className="kp-inputs">
          <select
            className="kp-select"
            aria-label="Schedule"
            value={preset}
            onChange={(e) => {
              const v = e.target.value;
              if (v === "inherit" || v === "none") set("schedule", { mode: v, value: draft.schedule.value });
              else if (v === "custom") set("schedule", { mode: "set", value: SCHEDULES.some((s) => s.cron === draft.schedule.value) ? "" : draft.schedule.value });
              else set("schedule", { mode: "set", value: v.slice("cron:".length) });
            }}
          >
            {!top && <option value="inherit">{as(above?.schedule?.trim() ? scheduleSaid(above.schedule, above.timezone) : null)}</option>}
            <option value="none">Never on a schedule</option>
            {SCHEDULES.map((s) => (
              <option key={s.cron} value={`cron:${s.cron}`}>
                {s.label}
              </option>
            ))}
            <option value="custom">Cron…</option>
          </select>
          {preset === "custom" && (
            <input
              className="kp-input kp-cron"
              aria-label="Cron schedule"
              placeholder="0 3 * * *"
              value={draft.schedule.value}
              onChange={(e) => set("schedule", { mode: "set", value: e.target.value })}
            />
          )}
          {draft.schedule.mode === "set" && (
            <>
              <input className="kp-input kp-zone" aria-label="Time zone" placeholder={above?.timezone || "UTC"} value={draft.timezone} onChange={(e) => set("timezone", e.target.value)} />
              {zone && zone !== draft.timezone && (
                <Button type="button" size="xs" variant="ghost" onClick={() => set("timezone", zone)}>
                  Use {zone}
                </Button>
              )}
            </>
          )}
        </div>
      </Row>
      <Row label="Let go when free space falls under" hint="Of the disk the copies are on: what the rules don't keep goes, least recently used first, until there's room.">
        <div className="kp-inputs">
          <Count field={draft.free_percent} unit="%" none="No share" inherit={as(above?.free_percent ? `${above.free_percent}%` : null)} onChange={(f) => set("free_percent", f)} />
          <Count field={draft.free_gib} unit="GiB" none="No size" inherit={as(above?.free_gib ? `${above.free_gib} GiB` : null)} onChange={(f) => set("free_gib", f)} />
        </div>
      </Row>
      <Row label="With too little room still" hint="Once what the rules let go is gone.">
        <select className="kp-select" aria-label="With too little room still" value={draft.beyond} onChange={(e) => set("beyond", e.target.value as Draft["beyond"])}>
          {!top && <option value="inherit">{as(above ? (above.beyond ? "the kept go too" : "stop there") : null)}</option>}
          <option value="no">Stop at what the rules keep</option>
          <option value="yes">Let the kept go too, least recently used first</option>
        </select>
      </Row>
      <p className="kp-said" aria-live="polite">
        {"error" in made ? <span className="kp-error">{made.error}</span> : settled ? rulesSaid(settled) : null}
        {!above && !top && "rules" in made && <span className="kp-dim"> Where this sets nothing, each copy's runner and repository rules apply.</span>}
      </p>
      <div className="kp-actions">
        <Button type="submit" size="sm" disabled={busy || !changed}>
          {busy ? "Saving…" : "Save"}
        </Button>
        {Object.keys(rules).length > 0 && (
          <Button type="button" size="sm" variant="ghost" disabled={busy} onClick={() => void save(null)}>
            {top ? "Clear" : `Back to ${aboveName}`}
          </Button>
        )}
        {changed && (
          <Button type="button" size="sm" variant="ghost" disabled={busy} onClick={() => setDraft(draftOf(rules, top))}>
            Undo
          </Button>
        )}
      </div>
    </form>
  );
}

function Row({ label, hint, children }: { label: string; hint: string; children: React.ReactNode }) {
  return (
    <div className="kp-row">
      <div className="kp-row-text">
        <span className="kp-row-label">{label}</span>
        <span className="kp-row-hint">{hint}</span>
      </div>
      <div className="kp-row-input">{children}</div>
    </div>
  );
}

/** A number rule: as the scope above says, none, or a number of `unit`. */
function Count({
  field,
  unit,
  none,
  inherit,
  onChange,
}: {
  field: { mode: Mode; value: string };
  unit: React.ReactNode;
  none: string;
  inherit: string | null;
  onChange: (f: { mode: Mode; value: string }) => void;
}) {
  return (
    <span className="kp-inputs">
      <select className="kp-select" aria-label={typeof unit === "string" ? `How: ${unit}` : "How"} value={field.mode} onChange={(e) => onChange({ ...field, mode: e.target.value as Mode })}>
        {inherit !== null && <option value="inherit">{inherit}</option>}
        <option value="none">{none}</option>
        <option value="set">Set…</option>
      </select>
      {field.mode === "set" && (
        <>
          <input
            className="kp-input kp-number"
            inputMode="numeric"
            aria-label={typeof unit === "string" ? unit : "How many"}
            value={field.value}
            onChange={(e) => onChange({ ...field, value: e.target.value })}
          />
          {typeof unit === "string" ? <span className="kp-unit-said">{unit}</span> : unit}
        </>
      )}
    </span>
  );
}

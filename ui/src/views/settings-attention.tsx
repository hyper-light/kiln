/**
 * Settings → You → Attention (attention plan 01a199e1b177752e84dad5dc990e6a15): how Mission control
 * keeps your agents. What it parks and archives by itself (parking quiet chats is off until you turn
 * it on; archiving parked chats is on), the park menu's times, how its learned order does against
 * simpler orders, and a replay of where agents would have sat. Every value shown is the server's.
 * `/settings#you.attention` opens it (Mission control links here).
 */
import * as React from "react";
import "./settings-attention.css";
import { ChevronDown } from "lucide-react";
import { attentionModelRes, attentionReplay, attentionSettingsRes, saveAttentionSettings, type AttentionReplay, type AttentionSettings } from "@/api/attention";
import { Button } from "@/components/ui/button";
import { Hold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { SettingRow, SettingsGroup } from "@/components/ui/settings";
import { useAction } from "@/lib/action";
import { harnessSessionsRes, useRes } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { agentLabel } from "@/views/harness/derive";
import { comparisonLabel, hourWords, parameterLabel } from "@/views/harness/attention-state";

const ID = "you.attention";
const TITLE = "Attention";
const ABOUT = "How Mission control keeps your agents: what it parks and archives by itself, the park menu's times, and how its learned order is doing.";
const WORDS = "attention mission control park parked archive archived pin order learned sort replay model quiet";

export function AttentionHold() {
  return (
    <SettingsGroup id={ID} title={TITLE} about={ABOUT} words={WORDS}>
      <Hold height={320} />
    </SettingsGroup>
  );
}

/** The form's own copy, numbers as typed: what the server has until you change it. */
interface Draft {
  autoPark: boolean;
  parkDays: string;
  autoArchive: boolean;
  archiveDays: string;
  later: string;
  evening: number;
  morning: number;
}
const draftOf = (s: AttentionSettings): Draft => ({
  autoPark: s.auto_park_days !== null,
  parkDays: s.auto_park_days === null ? "" : String(s.auto_park_days),
  autoArchive: s.auto_archive,
  archiveDays: String(s.archive_after_days),
  later: String(s.park_later_minutes),
  evening: s.park_evening_hour,
  morning: s.park_morning_hour,
});
/** A whole number of at least one, or nothing. */
const whole = (v: string) => (/^\d+$/.test(v.trim()) && Number(v) >= 1 ? Number(v) : null);
/** The draft as settings, when every number in it is one. */
function settingsOf(d: Draft, sort: AttentionSettings["sort"]): AttentionSettings | null {
  const parkDays = d.autoPark ? whole(d.parkDays) : null;
  const archiveDays = whole(d.archiveDays);
  const later = whole(d.later);
  if ((d.autoPark && parkDays === null) || archiveDays === null || later === null) return null;
  return { sort, auto_park_days: parkDays, auto_archive: d.autoArchive, archive_after_days: archiveDays, park_later_minutes: later, park_evening_hour: d.evening, park_morning_hour: d.morning };
}
const same = (a: AttentionSettings, b: AttentionSettings) => (Object.keys(a) as (keyof AttentionSettings)[]).every((k) => a[k] === b[k]);

const HOURS = Array.from({ length: 24 }, (_, h) => h);
function HourPick({ id, value, onChange }: { id: string; value: number; onChange: (h: number) => void }) {
  return (
    <div className="select">
      <select id={id} value={value} onChange={(e) => onChange(Number(e.target.value))}>
        {HOURS.map((h) => (
          <option key={h} value={h}>
            {hourWords(h)}
          </option>
        ))}
      </select>
      <ChevronDown size={14} aria-hidden="true" />
    </div>
  );
}

export function AttentionGroup() {
  const res = useRes(attentionSettingsRes);
  const settings = res.data.settings;
  // Turned on, parking starts at the server's days (it keeps none while it's off).
  const startDays = res.data.settings_defaults?.auto_park_days;
  const [draft, setDraft] = React.useState(() => draftOf(settings));
  // What the server says now stands over the form (after a save, or a change elsewhere).
  const [from, setFrom] = React.useState(settings);
  const [saved, setSaved] = React.useState(false);
  const [problem, setProblem] = React.useState<string | null>(null);
  if (from !== settings) {
    setFrom(settings);
    setDraft(draftOf(settings));
  }
  const change = (d: Partial<Draft>) => {
    setDraft({ ...draft, ...d });
    setSaved(false);
    setProblem(null);
  };
  const next = settingsOf(draft, settings.sort);
  // Saved whole; Save says it's on its way, and the form is as the server has it after (W21-R15).
  const [save, saving] = useAction(
    async (to: AttentionSettings) => {
      const snapshot = await saveAttentionSettings(to);
      res.change(() => snapshot);
      setSaved(true);
    },
    (e) => setProblem(errorMessage(e)),
  );
  return (
    <SettingsGroup id={ID} title={TITLE} about={ABOUT} words={WORDS}>
      <form
        noValidate
        onSubmit={(e) => {
          e.preventDefault();
          if (next && !same(next, settings)) save(next);
        }}
      >
        <SettingRow
          inline
          htmlFor="st-att-park"
          label="Park quiet chats by themselves"
          help="Off until you turn it on. A chat with nothing running that you haven't used in that many days goes to Parked, saying so."
        >
          <label className="st-check-control" htmlFor="st-att-park">
            <input
              id="st-att-park"
              type="checkbox"
              className="check"
              checked={draft.autoPark}
              onChange={(e) => change({ autoPark: e.target.checked, parkDays: e.target.checked && !draft.parkDays && startDays ? String(startDays) : draft.parkDays })}
            />
          </label>
        </SettingRow>
        {draft.autoPark && (
          <SettingRow label="Days quiet before it parks" htmlFor="st-att-park-days">
            <div className="field st-num">
              <input
                id="st-att-park-days"
                type="number"
                inputMode="numeric"
                min={1}
                step={1}
                value={draft.parkDays}
                aria-invalid={whole(draft.parkDays) === null || undefined}
                onChange={(e) => change({ parkDays: e.target.value })}
              />
              <span>days</span>
            </div>
          </SettingRow>
        )}
        <SettingRow
          inline
          htmlFor="st-att-archive"
          label="Archive parked chats"
          help="Archiving stops the agent for everyone and keeps its conversation; Restore, under Mission control's State filter (Archived), brings it back."
        >
          <label className="st-check-control" htmlFor="st-att-archive">
            <input id="st-att-archive" type="checkbox" className="check" checked={draft.autoArchive} onChange={(e) => change({ autoArchive: e.target.checked })} />
          </label>
        </SettingRow>
        <SettingRow label="Days parked before it's archived" htmlFor="st-att-archive-days" help="Also how long a park kiln makes by itself lasts.">
          <div className="field st-num">
            <input
              id="st-att-archive-days"
              type="number"
              inputMode="numeric"
              min={1}
              step={1}
              value={draft.archiveDays}
              aria-invalid={whole(draft.archiveDays) === null || undefined}
              onChange={(e) => change({ archiveDays: e.target.value })}
            />
            <span>days</span>
          </div>
        </SettingRow>
        <SettingRow label="Later today" htmlFor="st-att-later" help="Park until… Later today: this long from when you park.">
          <div className="field st-num">
            <input
              id="st-att-later"
              type="number"
              inputMode="numeric"
              min={1}
              step={1}
              value={draft.later}
              aria-invalid={whole(draft.later) === null || undefined}
              onChange={(e) => change({ later: e.target.value })}
            />
            <span>min</span>
          </div>
        </SettingRow>
        <SettingRow label="This evening" htmlFor="st-att-evening" help="Park until… This evening: at this hour today.">
          <HourPick id="st-att-evening" value={draft.evening} onChange={(evening) => change({ evening })} />
        </SettingRow>
        <SettingRow label="Tomorrow morning" htmlFor="st-att-morning" help="Park until… Tomorrow morning: at this hour tomorrow.">
          <HourPick id="st-att-morning" value={draft.morning} onChange={(morning) => change({ morning })} />
        </SettingRow>
        <div className="st-form-acts st-att-acts">
          {(problem || saved || !next) && (
            <p className="st-note st-feedback" data-tone={problem || !next ? "error" : "success"} role="status">
              {problem ?? (!next ? "Each number is a whole number of at least 1." : "Saved")}
            </p>
          )}
          <Button type="submit" size="xs" variant="secondary" pending={saving} disabled={!next || same(next, settings)}>
            Save
          </Button>
        </div>
      </form>
      <Loadable label="the learned order" fallback={<Hold height={160} />}>
        <LearnedOrder />
      </Loadable>
    </SettingsGroup>
  );
}

/** How the learned order does: why it's used or not, how well it predicts your picks against simpler orders, its parameters, and a replay. */
function LearnedOrder() {
  const model = useRes(attentionModelRes).data.model;
  return (
    <div className="st-att-model">
      <SettingRow label="How the learned order does" help={model.reason}>
        <p className="st-value">
          {model.observations} {model.observations === 1 ? "pick" : "picks"} learned from, {model.position_corrected} corrected for where {model.position_corrected === 1 ? "it was" : "they were"} in
          the list
        </p>
      </SettingRow>
      {model.comparisons.length > 0 && (
        <div className="st-att-table-wrap">
          <table className="st-att-table">
            <caption>How each order predicted your picks before learning from them, beside the learned order on the same picks: fewer given no chance is better, then the higher log score</caption>
            <thead>
              <tr>
                <th scope="col">Order</th>
                <th scope="col">Picks</th>
                <th scope="col">Given no chance</th>
                <th scope="col">Log score</th>
                <th scope="col">Learned order on them</th>
              </tr>
            </thead>
            <tbody>
              {model.comparisons.map((c) => (
                <tr key={c.name}>
                  <th scope="row">{comparisonLabel(c.name)}</th>
                  <td>{c.fitted ? c.choices : "not learned yet"}</td>
                  <td>{c.fitted ? c.refuted : ""}</td>
                  <td>{c.fitted && c.choices > 0 ? c.log_likelihood.toPrecision(4) : ""}</td>
                  <td>{c.learned ? `${c.learned.log_likelihood.toPrecision(4)}${c.learned.refuted > 0 ? `, ${c.learned.refuted} given no chance` : ""}` : ""}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {model.parameters.length > 0 && (
        <div className="st-att-table-wrap">
          <table className="st-att-table">
            <caption>What it learned</caption>
            <thead>
              <tr>
                <th scope="col">Parameter</th>
                <th scope="col">Value</th>
                <th scope="col">± standard error</th>
              </tr>
            </thead>
            <tbody>
              {model.parameters.map((p) => (
                <tr key={p.name}>
                  <th scope="row">{parameterLabel(p.name)}</th>
                  <td>{p.value.toPrecision(3)}</td>
                  <td>{p.standard_error === null ? "not known" : p.standard_error.toPrecision(2)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      <Replay />
    </div>
  );
}

/** Where your agents would have sat at a time you choose, each with why. */
function Replay() {
  const agents = useRes(harnessSessionsRes).data;
  const archived = useRes(attentionSettingsRes).data.archived;
  const names = new Map([...agents, ...archived].map((a) => [a.id, agentLabel(a)]));
  const [at, setAt] = React.useState("");
  const [replay, setReplay] = React.useState<AttentionReplay | null>(null);
  const [problem, setProblem] = React.useState<string | null>(null);
  const [go, going] = useAction(
    async (when: number) => {
      setProblem(null);
      setReplay(await attentionReplay(when));
    },
    (e) => setProblem(errorMessage(e)),
  );
  const when = at ? new Date(at).getTime() : Number.NaN;
  return (
    <>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          if (Number.isFinite(when)) go(when);
        }}
      >
        <SettingRow label="Replay" htmlFor="st-att-replay" help="Where your agents would have sat in the learned order at a time you choose, and why.">
          <div className="st-control-line">
            <div className="field st-wide">
              <input id="st-att-replay" type="datetime-local" value={at} onChange={(e) => setAt(e.target.value)} />
            </div>
            <Button type="submit" size="xs" variant="secondary" pending={going} disabled={!Number.isFinite(when)}>
              Replay
            </Button>
          </div>
        </SettingRow>
      </form>
      {problem && (
        <p className="st-note" data-tone="error" role="alert">
          {problem}
        </p>
      )}
      {replay && (
        <p className="st-note">
          {replay.learned ? "By the learned order" : "By your latest interaction (the learned order isn't used yet)"}
          {replay.pick && `. Your pick then, ${names.get(replay.pick.agent) ?? "an agent no longer listed"}, sat ${replay.pick.position} of ${replay.pick.of}`}. {replay.limitation}
        </p>
      )}
      {replay && (
        <ol className="st-list st-att-replay" aria-label="Where they would have sat">
          {replay.order.map((r, i) => (
            <li key={r.agent}>
              <div className="st-item">
                <span className="st-item-name">
                  {i + 1}. {names.get(r.agent) ?? "An agent no longer listed"}
                </span>
                <span className="st-item-meta">{r.reason}</span>
              </div>
            </li>
          ))}
        </ol>
      )}
    </>
  );
}

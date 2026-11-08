/** Pin, Park and Archive in the page's flow (plan 01a199e1b177752e84dad5dc990e6a15). */
import * as React from "react";
import { attentionReplay, type AttentionCommand, type AttentionReplay, type AttentionSnapshot } from "@/api/attention";
import { Button } from "@/components/ui/button";
import { parkChoices } from "./attention-state";
import type { AgentInfo } from "@/api/harness";

export type AttentionAct = (command: AttentionCommand | AttentionCommand[]) => Promise<void>;

export function AttentionControls({
  agents,
  snapshot,
  act,
  parked = false,
  archived = false,
}: {
  agents: readonly AgentInfo[];
  snapshot: AttentionSnapshot;
  act: AttentionAct;
  parked?: boolean;
  archived?: boolean;
}) {
  const [until, setUntil] = React.useState("tomorrow");
  const [custom, setCustom] = React.useState("");
  const [confirmArchive, setConfirmArchive] = React.useState(false);
  const [confirmCopy, setConfirmCopy] = React.useState(false);
  const [busy, setBusy] = React.useState(false);
  const choices = parkChoices(Date.now(), snapshot.settings);
  const selected = choices.find((c) => c.value === until);
  const at = until === "custom" ? new Date(custom).getTime() : selected?.at;
  const one = agents.length === 1 ? agents[0] : undefined;
  const archiving = agents.some((a) => snapshot.agents[a.id]?.archive_status === "archiving");
  const pinned = one ? snapshot.agents[one.id]?.pinned !== undefined : false;
  const run = async (action: AttentionCommand["action"], extra: Partial<AttentionCommand> = {}) => {
    setBusy(true);
    try {
      await act(agents.map((a) => ({ action, agent: a.id, ...extra })));
    } finally {
      setBusy(false);
    }
  };
  const pins = Object.entries(snapshot.agents)
    .filter(([, mark]) => mark.pinned !== undefined)
    .sort((x, y) => (x[1].pinned ?? 0) - (y[1].pinned ?? 0))
    .map(([id]) => id);
  const move = (offset: number) => {
    if (!one) return;
    const index = pins.indexOf(one.id);
    const other = pins[index + offset];
    if (!other) return;
    const order = [...pins];
    order[index] = other;
    order[index + offset] = one.id;
    void act({ action: "reorder", order });
  };
  return (
    <div className="ms-attention-controls" aria-label="Organize chats">
      {archived ? (
        <>
          <span>{archiving ? "Waiting for confirmation that the agent stopped." : ""}</span>
          <Button size="xs" disabled={busy} onClick={() => void run("restore")}>
            Restore
          </Button>
          <Button size="xs" variant="secondary" disabled={busy} onClick={() => setConfirmCopy(!confirmCopy)}>
            Continue from saved conversation…
          </Button>
          {confirmCopy && (
            <span className="ms-attention-confirm">
              Continue using a new harness session and kiln's saved conversation? Your saved history stays available.
              <Button size="xs" disabled={busy} onClick={() => void run("restore_copy")}>
                Continue from saved conversation
              </Button>
              <Button size="xs" variant="secondary" disabled={busy} onClick={() => setConfirmCopy(false)}>
                Cancel
              </Button>
            </span>
          )}
        </>
      ) : (
        <>
          <Button size="xs" variant="secondary" disabled={busy || agents.length === 0} onClick={() => void run(pinned ? "unpin" : "pin")}>
            {pinned ? "Unpin" : "Pin"}
          </Button>
          {agents.length > 1 && agents.some((a) => snapshot.agents[a.id]?.pinned !== undefined) && (
            <Button size="xs" variant="secondary" disabled={busy} onClick={() => void run("unpin")}>
              Unpin
            </Button>
          )}
          {pinned && (
            <>
              <Button size="xs" variant="secondary" disabled={busy || pins.indexOf(one?.id ?? "") <= 0} onClick={() => move(-1)}>
                Move pin up
              </Button>
              <Button size="xs" variant="secondary" disabled={busy || pins.indexOf(one?.id ?? "") === pins.length - 1} onClick={() => move(1)}>
                Move pin down
              </Button>
            </>
          )}
          {parked ? (
            <Button size="xs" disabled={busy} onClick={() => void run("unpark")}>
              Unpark
            </Button>
          ) : (
            <>
              <label className="ms-attention-field">
                Park until{" "}
                <select className="select" aria-label="Park until" value={until} onChange={(e) => setUntil(e.target.value)}>
                  {choices.map((c) => (
                    <option value={c.value} key={c.value}>
                      {c.label}
                    </option>
                  ))}
                  <option value="custom">Choose a time…</option>
                </select>
              </label>
              {until === "custom" && <input type="datetime-local" aria-label="Custom park time" value={custom} onChange={(e) => setCustom(e.target.value)} className="input" />}
              <Button size="xs" disabled={busy || !Number.isFinite(at) || (at ?? 0) <= Date.now()} onClick={() => void run("park", { until: at })}>
                Park
              </Button>
            </>
          )}
          <Button size="xs" variant="secondary" disabled={busy} onClick={() => setConfirmArchive(!confirmArchive)}>
            Archive…
          </Button>
          {confirmArchive && (
            <span className="ms-attention-confirm">
              Stop {agents.length === 1 ? "this agent" : `these ${agents.length} agents`} for everyone and keep the conversation for Restore?{" "}
              <Button size="xs" disabled={busy} onClick={() => void run("archive")}>
                Archive
              </Button>{" "}
              <Button size="xs" variant="secondary" onClick={() => setConfirmArchive(false)}>
                Cancel
              </Button>
            </span>
          )}
        </>
      )}
    </div>
  );
}

export function AttentionSettingsForm({ snapshot, act, agents }: { snapshot: AttentionSnapshot; act: AttentionAct; agents: readonly AgentInfo[] }) {
  const names = new Map([...agents, ...snapshot.archived].map((a) => [a.id, a.name || a.title || a.id]));
  const settings = snapshot.settings;
  const [replayAt, setReplayAt] = React.useState("");
  const [replay, setReplay] = React.useState<AttentionReplay | null>(null);
  const [replayError, setReplayError] = React.useState("");
  const fit = snapshot.model;
  const [open, setOpen] = React.useState(false);
  const [draft, setDraft] = React.useState(settings);
  return (
    <div className="ms-attention-settings">
      <button
        type="button"
        className="ms-link"
        aria-expanded={open}
        onClick={() => {
          setDraft(settings);
          setOpen(!open);
        }}
      >
        Attention settings
      </button>
      {open && fit && (
        <div className="ms-attention-diagnostics">
          <p>{fit.reason}</p>
          <p>
            {fit.observations} choices recorded. {fit.position_corrected} with position information. Historical choices without exposure information cannot establish position bias.
          </p>
          {snapshot.examination && (
            <div>
              <p>
                Completed visits used: {snapshot.examination.exposures}. Open or incomplete visits: {snapshot.examination.censored}. Position comparisons: {snapshot.examination.opportunities}.
              </p>
              {snapshot.examination.curves.map((curve) => (
                <p key={curve.layout}>
                  {curve.layout === "mission/phone" ? "Phone" : curve.layout === "mission/desktop" ? "Desktop" : curve.layout}: {curve.reason}
                </p>
              ))}
              {snapshot.examination.unavailable.map((item, i) => (
                <p key={`${item.layout}/${i}`}>
                  {item.layout === "mission/phone" ? "Phone" : item.layout === "mission/desktop" ? "Desktop" : item.layout}: {item.reason}
                </p>
              ))}
            </div>
          )}
          {fit.unavailable_comparisons?.map((c) => (
            <p key={c.name}>
              {c.name}: {c.reason}
            </p>
          ))}
          {fit.comparisons.length > 0 && (
            <table>
              <caption>Prediction on later choices</caption>
              <thead>
                <tr>
                  <th>Order</th>
                  <th>Choices</th>
                  <th>Log likelihood (higher is better)</th>
                </tr>
              </thead>
              <tbody>
                {fit.comparisons.map((c) => (
                  <tr key={c.name}>
                    <td>{c.name}</td>
                    <td>{c.choices}</td>
                    <td>{c.log_likelihood.toPrecision(4)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
          <form
            onSubmit={(e) => {
              e.preventDefault();
              setReplayError("");
              void attentionReplay(new Date(replayAt).getTime()).then(setReplay, (e: unknown) => setReplayError(e instanceof Error ? e.message : String(e)));
            }}
          >
            <label className="ms-attention-field">
              Replay this order at <input className="input" type="datetime-local" aria-label="Replay time" value={replayAt} onChange={(e) => setReplayAt(e.target.value)} required />
            </label>
            <Button size="xs" type="submit">
              Replay
            </Button>
          </form>
          {replayError && <p role="alert">{replayError}</p>}
          {replay && (
            <>
              <p>{replay.model.reason}</p>
              {replay.model.unavailable_comparisons?.map((c) => (
                <p key={c.name}>
                  {c.name}: {c.reason}
                </p>
              ))}
              <ol>
                {replay.order.map((r) => (
                  <li key={r.agent}>
                    {names.get(r.agent) ?? r.agent} · {r.reason}
                  </li>
                ))}
              </ol>
            </>
          )}
        </div>
      )}
      {open && (
        <form
          className="ms-attention-controls"
          onSubmit={(e) => {
            e.preventDefault();
            void act({
              action: "settings",
              settings: {
                auto_park_days: draft.auto_park_days,
                auto_archive: draft.auto_archive,
                archive_after_days: draft.archive_after_days,
                park_later_minutes: draft.park_later_minutes ?? 60,
                park_evening_hour: draft.park_evening_hour ?? 18,
                park_morning_hour: draft.park_morning_hour ?? 9,
              },
            }).then(() => setOpen(false));
          }}
        >
          <label>
            <input type="checkbox" className="check" checked={draft.auto_park_days !== null} onChange={(e) => setDraft({ ...draft, auto_park_days: e.target.checked ? 3 : null })} /> Automatically park
            quiet chats
          </label>
          {draft.auto_park_days !== null && (
            <label className="ms-attention-field">
              After{" "}
              <input
                className="input"
                aria-label="Days before automatic parking"
                type="number"
                min="1"
                step="1"
                required
                value={draft.auto_park_days}
                onChange={(e) => setDraft({ ...draft, auto_park_days: Number(e.target.value) })}
              />{" "}
              days with nothing running
            </label>
          )}
          <label>
            <input type="checkbox" className="check" checked={draft.auto_archive} onChange={(e) => setDraft({ ...draft, auto_archive: e.target.checked })} /> Automatically archive parked chats
          </label>
          <label className="ms-attention-field">
            After{" "}
            <input
              className="input"
              aria-label="Days before archiving"
              disabled={!draft.auto_archive}
              type="number"
              min="1"
              step="1"
              required
              value={draft.archive_after_days}
              onChange={(e) => setDraft({ ...draft, archive_after_days: Number(e.target.value) })}
            />{" "}
            days
          </label>
          <label className="ms-attention-field">
            Later today, after{" "}
            <input
              className="input"
              aria-label="Later park delay in minutes"
              type="number"
              min="1"
              step="1"
              required
              value={draft.park_later_minutes ?? 60}
              onChange={(e) => setDraft({ ...draft, park_later_minutes: Number(e.target.value) })}
            />{" "}
            minutes
          </label>
          <label className="ms-attention-field">
            This evening, hour{" "}
            <input
              className="input"
              aria-label="Evening park hour"
              type="number"
              min="0"
              max="23"
              step="1"
              required
              value={draft.park_evening_hour ?? 18}
              onChange={(e) => setDraft({ ...draft, park_evening_hour: Number(e.target.value) })}
            />{" "}
            (local time, 0–23)
          </label>
          <label className="ms-attention-field">
            Tomorrow morning, hour{" "}
            <input
              className="input"
              aria-label="Morning park hour"
              type="number"
              min="0"
              max="23"
              step="1"
              required
              value={draft.park_morning_hour ?? 9}
              onChange={(e) => setDraft({ ...draft, park_morning_hour: Number(e.target.value) })}
            />{" "}
            (local time, 0–23)
          </label>
          <span>Automatic parking and archiving are off until you enable them. Archiving stops the agent for everyone.</span>
          <Button size="xs" type="submit">
            Save
          </Button>
          <Button size="xs" variant="secondary" onClick={() => setOpen(false)}>
            Cancel
          </Button>
        </form>
      )}
    </div>
  );
}

/**
 * An environment deployed to by API (docs/deploy-api.md §4): its provider and release repository,
 * where it deploys, the gates each release passes there and who may release; what runs there; and
 * a freeze, put on and lifted here with why (an incident, a holiday): nothing goes until it's lifted.
 * Its gates, concurrency and what agents may do are changed here too.
 */
import * as React from "react";
import { Link, useParams } from "@/lib/router";
import { Plus, Snowflake, Sun, Trash2 } from "lucide-react";

import { boardHref, deployApi, describe, providerName, releaseAppHref, releaseHref, type Environment, type Gates } from "@/api/deploy";
import { Button } from "@/components/ui/button";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, Facts, Panel, SelectField, TextField } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/tabs";
import { toast } from "@/components/ui/toast";
import { useRes } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { invalidate, useSuspenseResource } from "@/lib/resource";
import { boardRes, DEPLOY_EVENTS, ReleaseState } from "./index";
import "@/views/deployments/deployments.css";
import "./releases.css";

/** Each gate, in a line. */
function gateLines(g: Gates): { label: string; value: string }[] {
  const out: { label: string; value: string }[] = [];
  if (g.after?.length) out.push({ label: "After", value: `${g.after.join(", ")}${g.soak ? `, for ${g.soak}` : ""}${g.promote === "auto" ? "; moves on by itself" : ""}` });
  if (g.approvals) {
    const who = g.approvals.reviewers?.length ? ` from ${g.approvals.reviewers.join(", ")}` : "";
    out.push({ label: "Approvals", value: `${g.approvals.count}${who}${g.approvals.self_approve ? ", the requester's own counting" : ""}` });
  }
  for (const w of g.windows ?? []) {
    out.push({ label: w.allow ? "Allowed" : "Denied", value: `${w.allow ?? w.deny} for ${w.duration}${w.timezone ? ` (${w.timezone})` : ""}` });
  }
  if (g.checks?.length) out.push({ label: "Checks", value: g.checks.join(", ") });
  return out;
}

/** Names in a field, separated by commas. */
const names = (s: string) =>
  s
    .split(",")
    .map((x) => x.trim())
    .filter(Boolean);

type WindowRow = { kind: "allow" | "deny"; cron: string; duration: string; timezone: string };

const CHECKS: { name: string; words: string }[] = [
  { name: "no-snapshots", words: "No snapshot versions" },
  { name: "owners-set", words: "The app has an owner team" },
];

/** What every release here passes, how a second one waits, and what agents may do; saved whole. */
function Settings({ e }: { e: Environment }) {
  const g = e.gates;
  const [after, setAfter] = React.useState((g.after ?? []).join(", "));
  const [soak, setSoak] = React.useState(g.soak ?? "");
  const [promote, setPromote] = React.useState<"auto" | "manual">(g.promote === "auto" ? "auto" : "manual");
  const [approvals, setApprovals] = React.useState(String(g.approvals?.count ?? 0));
  const [reviewers, setReviewers] = React.useState((g.approvals?.reviewers ?? []).join(", "));
  const [selfApprove, setSelfApprove] = React.useState(!!g.approvals?.self_approve);
  const [checks, setChecks] = React.useState<string[]>(g.checks ?? []);
  const [windows, setWindows] = React.useState<WindowRow[]>(
    (g.windows ?? []).map((w) => ({ kind: w.allow ? "allow" : "deny", cron: w.allow ?? w.deny ?? "", duration: w.duration, timezone: w.timezone ?? "" })),
  );
  const [concurrency, setConcurrency] = React.useState(e.concurrency);
  const [agents, setAgents] = React.useState(e.agents);
  const [busy, setBusy] = React.useState(false);
  const count = Number(approvals);
  const wrong =
    !Number.isInteger(count) || count < 0
      ? "Approvals: a whole number, 0 for none."
      : windows.some((w) => !w.cron.trim() || !w.duration.trim())
        ? "Each window needs its schedule and how long it lasts."
        : null;
  const setWindow = (i: number, patch: Partial<WindowRow>) => setWindows((ws) => ws.map((w, j) => (j === i ? { ...w, ...patch } : w)));
  const save = async () => {
    const gates: Gates = {
      after: names(after),
      soak: soak.trim() || undefined,
      promote: promote === "auto" && names(after).length ? "auto" : undefined,
      approvals: count > 0 ? { count, reviewers: names(reviewers), self_approve: selfApprove } : undefined,
      windows: windows.map((w) => {
        const when = { duration: w.duration.trim(), timezone: w.timezone.trim() || undefined };
        return w.kind === "allow" ? { allow: w.cron.trim(), ...when } : { deny: w.cron.trim(), ...when };
      }),
      checks,
      freeze: g.freeze,
    };
    setBusy(true);
    try {
      await deployApi.putEnvironment({ ...e, gates, concurrency, agents });
      toast({ title: `${e.name} is saved`, description: "Releases asked for from now on pass its gates as they are.", tone: "succeeded" });
      invalidate("deploy/");
    } catch (err) {
      toast({ title: "Not saved", description: errorMessage(err), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };
  return (
    <Panel eyebrow="Settings" title="How releases go here" description="Each release passes these gates in order; a person changes what agents may do.">
      <div className="cd-new-grid">
        <TextField label="After" placeholder="staging" value={after} spellCheck={false} onChange={(ev) => setAfter(ev.target.value)} hint="Environments a version runs in first." />
        <TextField label="Soak" placeholder="30m" value={soak} spellCheck={false} onChange={(ev) => setSoak(ev.target.value)} hint="How long it runs there first." />
        <TextField label="Approvals" inputMode="numeric" value={approvals} onChange={(ev) => setApprovals(ev.target.value)} hint="How many people approve; 0 for none." />
        <TextField label="Reviewers" placeholder="ada, grace" value={reviewers} onChange={(ev) => setReviewers(ev.target.value)} hint="Who may approve; anyone who approves releases when empty." />
      </div>
      <div className="cd-new-checks">
        <label className="cd-new-check">
          <input type="checkbox" className="check" checked={selfApprove} onChange={(ev) => setSelfApprove(ev.target.checked)} /> Whoever asked may approve it too
        </label>
        {CHECKS.map((c) => (
          <label key={c.name} className="cd-new-check">
            <input type="checkbox" className="check" checked={checks.includes(c.name)} onChange={(ev) => setChecks((xs) => (ev.target.checked ? [...xs, c.name] : xs.filter((x) => x !== c.name)))} />{" "}
            {c.words}
          </label>
        ))}
      </div>
      <div className="rel-windows">
        <span className="field-label">Windows</span>
        {windows.map((w, i) => (
          <div key={i} className="rel-window">
            <SelectField
              label="Kind"
              hideLabel
              value={w.kind}
              onChange={(v) => setWindow(i, { kind: v === "allow" ? "allow" : "deny" })}
              options={[
                { value: "deny", label: "No releases" },
                { value: "allow", label: "Releases only" },
              ]}
            />
            <TextField label="From (cron)" placeholder="0 17 * * 5" value={w.cron} spellCheck={false} onChange={(ev) => setWindow(i, { cron: ev.target.value })} />
            <TextField label="For" placeholder="64h" value={w.duration} spellCheck={false} onChange={(ev) => setWindow(i, { duration: ev.target.value })} />
            <TextField label="Time zone" placeholder="UTC" value={w.timezone} spellCheck={false} onChange={(ev) => setWindow(i, { timezone: ev.target.value })} />
            <Button size="xs" variant="ghost-danger" aria-label="Remove the window" onClick={() => setWindows((ws) => ws.filter((_, j) => j !== i))}>
              <Trash2 />
            </Button>
          </div>
        ))}
        <Button size="xs" variant="ghost" onClick={() => setWindows((ws) => [...ws, { kind: "deny", cron: "", duration: "", timezone: "" }])}>
          <Plus /> A window
        </Button>
      </div>
      {names(after).length > 0 && (
        <div className="rel-settings-row">
          <Segmented
            label="A version that lands there comes here"
            value={promote}
            onChange={setPromote}
            options={[
              { value: "manual", label: "When someone releases it" },
              { value: "auto", label: "By itself" },
            ]}
          />
        </div>
      )}
      <div className="rel-settings-row">
        <Segmented
          label="A release asked for while one is on its way"
          value={concurrency}
          onChange={setConcurrency}
          options={[
            { value: "reject", label: "Refused" },
            { value: "queue", label: "Waits its turn" },
          ]}
        />
        <div className="cd-new-checks">
          <label className="cd-new-check">
            <input type="checkbox" className="check" checked={agents.deploy} onChange={(ev) => setAgents({ ...agents, deploy: ev.target.checked })} /> Agents may release here
          </label>
          <label className="cd-new-check">
            <input type="checkbox" className="check" checked={agents.approve} onChange={(ev) => setAgents({ ...agents, approve: ev.target.checked })} /> Agents may approve here
          </label>
        </div>
      </div>
      {wrong && <p className="rel-msg">{wrong}</p>}
      <div className="rel-settings-actions">
        <Button size="sm" disabled={busy || !!wrong} onClick={() => void save()}>
          Save
        </Button>
      </div>
    </Panel>
  );
}

/** A freeze, put on with why, or lifted. */
function Freeze({ e }: { e: Environment }) {
  const [why, setWhy] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const frozen = e.gates.freeze;
  const put = async (freeze: string | undefined) => {
    setBusy(true);
    try {
      await deployApi.putEnvironment({ ...e, gates: { ...e.gates, freeze } });
      toast({
        title: freeze ? `${e.name} is frozen` : `${e.name} is thawed`,
        description: freeze ? "Releases wait here until it's lifted." : "Releases held here go on.",
        tone: freeze ? "info" : "succeeded",
      });
      setWhy("");
      invalidate("deploy/");
    } catch (err) {
      toast({ title: freeze ? "Not frozen" : "Not thawed", description: errorMessage(err), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };
  return (
    <Panel eyebrow="Freeze" title={frozen ? "Frozen" : "Releases go"} description={frozen ? `Why: ${frozen}` : "A freeze holds every release here until it's lifted; each waits, and goes once it is."}>
      {frozen ? (
        <Button size="sm" variant="secondary" disabled={busy} onClick={() => void put(undefined)}>
          <Sun /> Lift the freeze
        </Button>
      ) : (
        <div className="rel-register">
          <TextField label="Why" placeholder="An incident in payments" value={why} onChange={(ev) => setWhy(ev.target.value)} />
          <Button size="sm" variant="ghost-danger" disabled={busy || !why.trim()} onClick={() => void put(why.trim())}>
            <Snowflake /> Freeze {e.name}
          </Button>
        </div>
      )}
    </Panel>
  );
}

/** What runs here, app by app, and what's on its way. */
function Here({ env }: { env: string }) {
  const board = useRes(boardRes).data;
  const cells = board.cells.filter((c) => c.registration.environment === env);
  if (!cells.length) return <p className="rel-quiet">No app is registered here yet.</p>;
  return (
    <Panel eyebrow="Apps" title="What runs here">
      <ol className="rel-history">
        {cells.map((c) => (
          <li key={c.registration.app}>
            <Link to={releaseAppHref(c.registration.app)}>{c.registration.app}</Link>
            <span className="rel-what">{c.running ? <Link to={releaseHref(c.running.id)}>{describe(c.running)}</Link> : "nothing yet"}</span>
            {c.in_flight ? <ReleaseState r={c.in_flight} /> : <span />}
            <span className="rel-who">{c.registration.paused ? "paused" : c.registration.enabled ? "" : "disabled"}</span>
          </li>
        ))}
      </ol>
    </Panel>
  );
}

function EnvView({ name }: { name: string }) {
  const e = useSuspenseResource(`deploy/environment/${name}`, (s) => deployApi.environment(name, s), { on: DEPLOY_EVENTS }).data;
  const gates = gateLines(e.gates);
  const clusters = e.targets.kubernetes.length ? e.targets.kubernetes.join(", ") : "the cluster kiln runs in";
  const platforms = Object.keys(e.targets.platforms ?? {});
  return (
    <>
      <header className="cd-head">
        <Crumbs index={3} items={[{ label: "Deployments", to: "/deployments" }, { label: "Releases", to: boardHref }, { label: e.name }]} />
        <h1 className="cd-title">{e.name}</h1>
        <p className="cd-note">
          {providerName(e.provider)} · releases committed to <code>{`${e.release_repo.repo}@${e.release_repo.branch}${e.release_repo.root ? `/${e.release_repo.root}` : ""}`}</code>
          {e.gates.freeze && " · frozen"}
        </p>
      </header>
      <div className="rel-env-page">
        <Panel eyebrow="Where" title="What it deploys to">
          <Facts
            items={[
              { label: "Kubernetes", value: clusters },
              ...(platforms.length ? [{ label: "Platforms", value: platforms.join(", ") }] : []),
              { label: "Concurrency", value: e.concurrency === "queue" ? "a release waits for the one before" : "one release at a time; another is refused" },
              { label: "Agents", value: e.agents.deploy ? (e.agents.approve ? "release and approve here" : "release here") : "may not release here" },
            ]}
          />
        </Panel>
        <Panel eyebrow="Gates" title={gates.length ? "Each release passes" : "No gates"} description={gates.length ? undefined : "Releases go as soon as they're asked for."}>
          {gates.length > 0 && <Facts items={gates} />}
        </Panel>
        <Freeze e={e} />
      </div>
      <Loadable label="what runs here">
        <Here env={e.name} />
      </Loadable>
      <div className="rel-stack">
        <Settings key={JSON.stringify(e)} e={e} />
      </div>
    </>
  );
}

export default function ReleaseEnvironment() {
  const env = useParams().env ?? "";
  return (
    <div className="container-console page cd-page">
      <title>{`${env} · Releases · kiln`}</title>
      <Loadable label={env}>
        <EnvView name={env} />
      </Loadable>
    </div>
  );
}

/**
 * An environment deployed to by API (docs/deploy-api.md §4): its provider and release repository,
 * where it deploys, the gates each release passes there and who may release; what runs there; and
 * a freeze, put on and lifted here with why (an incident, a holiday): nothing goes until it's lifted.
 * Its gates, concurrency and what agents may do are changed here too, and it moves to another
 * provider here (§8.4): the new one compares in shadow, then takes over with nothing made again.
 */
import * as React from "react";
import { Link, useParams } from "@/lib/router";
import { ArrowRightLeft, Plus, Snowflake, Sun, Trash2 } from "lucide-react";

import { api } from "@/api/client";
import { boardHref, checkName, deployApi, describe, providerName, releaseAppHref, releaseHref, releaseRepoUrl, type Check, type CheckSpec, type Environment, type Gates, type Move, type ProvenanceRequirement } from "@/api/deploy";
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

/** A check, as a person reads it. */
function checkWords(c: Check): string {
  if (typeof c === "string") return c;
  const what = c.http ? `asks ${c.http.url}` : c.workflow ? `runs ${c.workflow.workflow} of ${c.workflow.repository}${c.workflow.ref ? `@${c.workflow.ref}` : ""}` : "";
  return `${c.name}${what ? `: ${what}` : ""}${c.report_only ? " (report only)" : ""}`;
}

/** The provenance asked of what an environment deploys, as a person reads it. */
function provenanceWords(p: ProvenanceRequirement): string {
  const from = p.workflows?.length ? `, built by ${p.workflows.join(", ")}` : "";
  const on = p.branches?.length ? ` on ${p.branches.join(", ")}` : "";
  return `kiln's signed provenance of each artifact${from}${on}${p.builder ? ` (builder ${p.builder})` : ""}`;
}

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
  if (g.checks?.length) out.push({ label: "Checks", value: g.checks.map(checkWords).join("; ") });
  if (g.require_provenance) out.push({ label: "Provenance", value: provenanceWords(g.require_provenance) });
  if (g.via === "pull-request") out.push({ label: "Pull requests", value: "each release is a pull request (a merge request on GitLab): merging it is the approval, closing it cancels the release" });
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

/** An external check, as edited: a URL kiln asks, or a workflow it runs. */
type CheckRow = { name: string; kind: "http" | "workflow"; url: string; timeout: string; repository: string; workflow: string; ref: string; reportOnly: boolean };

const isExternal = (c: Check): c is CheckSpec => typeof c !== "string" && (!!c.http || !!c.workflow);

function checkRow(c: CheckSpec): CheckRow {
  return {
    name: c.name,
    kind: c.workflow ? "workflow" : "http",
    url: c.http?.url ?? "",
    timeout: c.http?.timeout ?? "",
    repository: c.workflow?.repository ?? "",
    workflow: c.workflow?.workflow ?? "",
    ref: c.workflow?.ref ?? "",
    reportOnly: !!c.report_only,
  };
}

function checkSpec(r: CheckRow): CheckSpec {
  const how =
    r.kind === "http"
      ? { http: { url: r.url.trim(), timeout: r.timeout.trim() || undefined } }
      : { workflow: { repository: r.repository.trim(), workflow: r.workflow.trim(), ref: r.ref.trim() || undefined } };
  return { name: r.name.trim(), ...how, report_only: r.reportOnly || undefined };
}

/** What every release here passes, how a second one waits, and what agents may do; saved whole. */
function Settings({ e }: { e: Environment }) {
  const g = e.gates;
  const [after, setAfter] = React.useState((g.after ?? []).join(", "));
  const [soak, setSoak] = React.useState(g.soak ?? "");
  const [promote, setPromote] = React.useState<"auto" | "manual">(g.promote === "auto" ? "auto" : "manual");
  const [approvals, setApprovals] = React.useState(String(g.approvals?.count ?? 0));
  const [reviewers, setReviewers] = React.useState((g.approvals?.reviewers ?? []).join(", "));
  const [selfApprove, setSelfApprove] = React.useState(!!g.approvals?.self_approve);
  // Built-in checks by name (one given as a description, report only, stays as it is), and the
  // environment's own: a URL asked, or a workflow run.
  const builtInSpecs = (g.checks ?? []).filter((c): c is CheckSpec => typeof c !== "string" && !isExternal(c));
  const [checks, setChecks] = React.useState<string[]>((g.checks ?? []).filter((c) => !isExternal(c)).map(checkName));
  const [external, setExternal] = React.useState<CheckRow[]>((g.checks ?? []).filter(isExternal).map(checkRow));
  const [provenance, setProvenance] = React.useState(!!g.require_provenance);
  const [provWorkflows, setProvWorkflows] = React.useState((g.require_provenance?.workflows ?? []).join(", "));
  const [provBranches, setProvBranches] = React.useState((g.require_provenance?.branches ?? []).join(", "));
  const [windows, setWindows] = React.useState<WindowRow[]>(
    (g.windows ?? []).map((w) => ({ kind: w.allow ? "allow" : "deny", cron: w.allow ?? w.deny ?? "", duration: w.duration, timezone: w.timezone ?? "" })),
  );
  const [concurrency, setConcurrency] = React.useState(e.concurrency);
  const [agents, setAgents] = React.useState(e.agents);
  const [ownersRelease, setOwnersRelease] = React.useState(e.owners?.release ?? false);
  const [pulls, setPulls] = React.useState(g.via === "pull-request");
  const [busy, setBusy] = React.useState(false);
  // Pull requests (GitLab's merge requests) where the release repository is on another host.
  const onHost = e.release_repo.host !== "kiln";
  const count = Number(approvals);
  const wrong =
    !Number.isInteger(count) || count < 0
      ? "Approvals: a whole number, 0 for none."
      : pulls && count > 0
        ? "With pull requests, merging one is the approval: set approvals to 0 (who may merge is the branch's protection on its host)."
      : windows.some((w) => !w.cron.trim() || !w.duration.trim())
        ? "Each window needs its schedule and how long it lasts."
        : external.some((r) => !r.name.trim() || (r.kind === "http" ? !r.url.trim() : !r.repository.trim() || !r.workflow.trim()))
          ? "Each check needs its name, and its URL, or its repository and workflow."
          : null;
  const setWindow = (i: number, patch: Partial<WindowRow>) => setWindows((ws) => ws.map((w, j) => (j === i ? { ...w, ...patch } : w)));
  const setCheck = (i: number, patch: Partial<CheckRow>) => setExternal((xs) => xs.map((x, j) => (j === i ? { ...x, ...patch } : x)));
  const save = async () => {
    const builtIn: Check[] = checks.map((n) => builtInSpecs.find((s) => s.name === n) ?? n);
    const gates: Gates = {
      // What this form doesn't show is kept as it is.
      ...g,
      after: names(after),
      soak: soak.trim() || undefined,
      promote: promote === "auto" && names(after).length ? "auto" : undefined,
      approvals: count > 0 ? { count, reviewers: names(reviewers), self_approve: selfApprove } : undefined,
      windows: windows.map((w) => {
        const when = { duration: w.duration.trim(), timezone: w.timezone.trim() || undefined };
        return w.kind === "allow" ? { allow: w.cron.trim(), ...when } : { deny: w.cron.trim(), ...when };
      }),
      checks: [...builtIn, ...external.map(checkSpec)],
      require_provenance: provenance ? { builder: g.require_provenance?.builder, workflows: names(provWorkflows), branches: names(provBranches) } : undefined,
      freeze: g.freeze,
      via: onHost && pulls ? "pull-request" : undefined,
    };
    setBusy(true);
    try {
      await deployApi.putEnvironment({ ...e, gates, concurrency, agents, owners: { release: ownersRelease } });
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
        <label className="cd-new-check">
          <input type="checkbox" className="check" checked={provenance} onChange={(ev) => setProvenance(ev.target.checked)} /> Only what kiln's CI built and signed (provenance)
        </label>
        {onHost && (
          <label className="cd-new-check">
            <input type="checkbox" className="check" checked={pulls} onChange={(ev) => setPulls(ev.target.checked)} /> Releases as pull requests: merging one releases it, closing it cancels it
          </label>
        )}
      </div>
      {provenance && (
        <div className="cd-new-grid">
          <TextField label="Built by" placeholder=".kiln/build.yaml" value={provWorkflows} spellCheck={false} onChange={(ev) => setProvWorkflows(ev.target.value)} hint="The workflows it may come from; any when empty." />
          <TextField label="From branches" placeholder="main" value={provBranches} spellCheck={false} onChange={(ev) => setProvBranches(ev.target.value)} hint="The branches it may be built from; any when empty." />
        </div>
      )}
      <div className="rel-windows">
        <span className="field-label">Checks of its own</span>
        {external.map((r, i) => (
          <div key={i} className="rel-ext-check">
            <TextField label="Name" placeholder="smoke" value={r.name} spellCheck={false} onChange={(ev) => setCheck(i, { name: ev.target.value })} />
            <SelectField
              label="Kind"
              value={r.kind}
              onChange={(v) => setCheck(i, { kind: v === "workflow" ? "workflow" : "http" })}
              options={[
                { value: "http", label: "A URL answers" },
                { value: "workflow", label: "A workflow runs" },
              ]}
            />
            {r.kind === "http" ? (
              <>
                <TextField label="URL" placeholder="https://checks.example.com/smoke" value={r.url} spellCheck={false} onChange={(ev) => setCheck(i, { url: ev.target.value })} />
                <TextField label="Timeout" placeholder="10s" value={r.timeout} spellCheck={false} onChange={(ev) => setCheck(i, { timeout: ev.target.value })} />
              </>
            ) : (
              <>
                <TextField label="Repository" placeholder="acme/web" value={r.repository} spellCheck={false} onChange={(ev) => setCheck(i, { repository: ev.target.value })} />
                <TextField label="Workflow" placeholder=".kiln/smoke.yaml" value={r.workflow} spellCheck={false} onChange={(ev) => setCheck(i, { workflow: ev.target.value })} />
                <TextField label="At" placeholder="main" value={r.ref} spellCheck={false} onChange={(ev) => setCheck(i, { ref: ev.target.value })} />
              </>
            )}
            <label className="rel-check">
              <input type="checkbox" className="check" checked={r.reportOnly} onChange={(ev) => setCheck(i, { reportOnly: ev.target.checked })} />
              <span>Report only</span>
            </label>
            <Button size="xs" variant="ghost-danger" aria-label="Remove the check" onClick={() => setExternal((xs) => xs.filter((_, j) => j !== i))}>
              <Trash2 />
            </Button>
          </div>
        ))}
        <Button size="xs" variant="ghost" onClick={() => setExternal((xs) => [...xs, { name: "", kind: "http", url: "", timeout: "", repository: "", workflow: "", ref: "", reportOnly: false }])}>
          <Plus /> A check
        </Button>
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
          <label className="cd-new-check">
            <input type="checkbox" className="check" checked={ownersRelease} onChange={(ev) => setOwnersRelease(ev.target.checked)} /> An app's owner teams may release it here
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

const PHASE_WORDS: Record<Move["phase"], string> = {
  comparing: "comparing in shadow",
  compared: "compared",
  handing_over: "handing over",
  moved: "moved",
  failed: "stopped",
  cancelled: "cancelled",
};

/** How a move stands: each app, whether the new provider agrees with what runs, and, handed over, whether anything was made again. */
function MoveReport({ m }: { m: Move }) {
  return (
    <div className="rel-move">
      <p className="cd-meta">
        To {providerName(m.to)}: {PHASE_WORDS[m.phase]}
        {m.message ? ` · ${m.message}` : ""}
      </p>
      {m.registrations.length > 0 && (
        <ol className="rel-history">
          {m.registrations.map((r) => (
            <li key={r.app}>
              <Link to={releaseAppHref(r.app)}>{r.app}</Link>
              <span className="rel-what">
                {r.agrees === undefined ? "not compared yet" : r.agrees ? "agrees with what runs" : `would change ${r.differences?.length ?? 0} object${r.differences?.length === 1 ? "" : "s"}`}
              </span>
              <span className="rel-who">
                {r.remade === undefined ? "" : r.remade.length ? `made again: ${r.remade.join(", ")}` : `nothing made again (${Object.keys(r.uids ?? {}).length} objects)`}
              </span>
              {!!r.differences?.length && (
                <details className="rel-manifest">
                  <summary>Where it disagrees</summary>
                  {r.differences.map((d) => (
                    <pre key={`${d.key.kind}/${d.key.namespace}/${d.key.name}`} className="rel-log">
                      {`${d.key.kind} ${d.key.namespace}/${d.key.name}\n`}
                      {d.hunks.flatMap((h) => h.lines).join("\n")}
                    </pre>
                  ))}
                </details>
              )}
            </li>
          ))}
        </ol>
      )}
    </div>
  );
}

/** Its provider, and moving it to another (§8.4): compared in shadow first, then handed over. */
function Provider({ e }: { e: Environment }) {
  const move = useSuspenseResource(`deploy/move/${e.name}`, (s) => deployApi.move(e.name, s), { on: DEPLOY_EVENTS, pollMs: 10_000 }).data;
  const integrations = useSuspenseResource("deploy/integrations", (s) => api.integrations(s)).data.integrations.filter((i) => i.kind === "argocd");
  const options = [{ value: "kilncd", label: "KilnCD (kiln's own)" }, ...integrations.map((i) => ({ value: `argocd/${i.name}`, label: `Argo CD ${i.name}` }))].filter((o) => o.value !== e.provider);
  const [to, setTo] = React.useState(options[0]?.value ?? "");
  const [applications, setApplications] = React.useState(false);
  const [busy, setBusy] = React.useState(false);
  const open = !!move && (["comparing", "compared", "handing_over"].includes(move.phase) || (move.phase === "failed" && move.handing_over));
  const agree = !!move && move.registrations.every((r) => r.agrees);
  const argo = e.provider.startsWith("argocd/");
  const ask = async (handover: boolean, force = false) => {
    setBusy(true);
    try {
      const target = open && move ? move.to : to;
      const m = await deployApi.startMove(e.name, { to: target, handover, force, argocd: { applications: open && move ? move.argocd.applications : applications } });
      toast({
        title: handover ? `Handing ${e.name} over to ${providerName(m.to)}` : `Comparing ${e.name} on ${providerName(m.to)}`,
        description: "It goes on by itself; this page follows it.",
        tone: "info",
      });
      invalidate(`deploy/move/${e.name}`);
    } catch (err) {
      toast({ title: "Not moved", description: errorMessage(err), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };
  const cancel = async () => {
    setBusy(true);
    try {
      await deployApi.cancelMove(e.name);
      toast({ title: "Move cancelled", description: `${e.name} stays on ${providerName(e.provider)}.`, tone: "info" });
      invalidate(`deploy/move/${e.name}`);
    } catch (err) {
      toast({ title: "Not cancelled", description: errorMessage(err), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };
  return (
    <Panel
      eyebrow="Provider"
      title={providerName(e.provider)}
      description={
        argo
          ? e.argocd?.applications
            ? "kiln writes each app's Application itself."
            : "One ApplicationSet makes each app's Application from its release.json."
          : "kiln's own engine runs each app from the release repository."
      }
    >
      {move && <MoveReport m={move} />}
      {!open ? (
        options.length > 0 && (
          <div className="rel-register">
            <SelectField label="Move it to" value={to} onChange={setTo} options={options} />
            {to.startsWith("argocd/") && (
              <label className="rel-check">
                <input type="checkbox" className="check" checked={applications} onChange={(ev) => setApplications(ev.target.checked)} />
                <span>kiln writes each Application (no ApplicationSet)</span>
              </label>
            )}
            <Button size="sm" variant="secondary" disabled={busy || !to} onClick={() => void ask(false)}>
              <ArrowRightLeft /> Compare there
            </Button>
          </div>
        )
      ) : (
        <div className="rel-register">
          {move?.phase === "compared" && (
            <Button size="sm" disabled={busy || !agree} onClick={() => void ask(true)}>
              Hand over to {providerName(move.to)}
            </Button>
          )}
          {move?.phase === "compared" && !agree && (
            <Button size="sm" variant="ghost-danger" disabled={busy} onClick={() => void ask(true, true)}>
              Hand over anyway
            </Button>
          )}
          {move?.phase === "failed" && move.handing_over && (
            <Button size="sm" disabled={busy} onClick={() => void ask(true)}>
              Finish the handover
            </Button>
          )}
          {move?.phase === "compared" && (
            <Button size="sm" variant="ghost" disabled={busy} onClick={() => void cancel()}>
              Cancel the move
            </Button>
          )}
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

/** Another git host's name, as its repositories are called. */
const HOSTS_OF: Record<string, string> = { github: "GitHub's", gitlab: "GitLab's", bitbucket: "Bitbucket's" };

/** Where an environment's releases go: its release repository, linked where it's on another host. */
function RepoLine({ e }: { e: Environment }) {
  const hosts = useSuspenseResource("deploy/git-hosts", (s) => deployApi.gitHosts(s)).data;
  const where = `${e.release_repo.repo}@${e.release_repo.branch}${e.release_repo.root ? `/${e.release_repo.root}` : ""}`;
  const url = releaseRepoUrl(e, hosts);
  const proposals = e.release_repo.host === "gitlab" ? "merge requests" : "pull requests";
  return (
    <>
      releases {e.gates.via === "pull-request" ? `proposed as ${proposals} on` : "committed to"}{" "}
      {url ? (
        <>
          {HOSTS_OF[e.release_repo.host] ?? "its host's"}{" "}
          <a href={url} target="_blank" rel="noreferrer">
            <code>{where}</code>
          </a>
        </>
      ) : (
        <code>{where}</code>
      )}
    </>
  );
}

function EnvView({ name }: { name: string }) {
  const e = useSuspenseResource(`deploy/environment/${name}`, (s) => deployApi.environment(name, s), { on: DEPLOY_EVENTS }).data;
  const gates = gateLines(e.gates);
  const clusters = e.targets.kubernetes.length ? e.targets.kubernetes.join(", ") : "the cluster kiln runs in";
  const platforms = Object.keys(e.targets.platforms ?? {});
  const reaches = Object.entries(e.targets.kilncd ?? {});
  const secrets = reaches.length ? reaches.map(([argo, k]) => `into ${argo} through ${k}`).join(", ") : "through KilnCD's cluster of Argo CD's name for it";
  return (
    <>
      <header className="cd-head">
        <Crumbs index={3} items={[{ label: "Deployments", to: "/deployments" }, { label: "Releases", to: boardHref }, { label: e.name }]} />
        <h1 className="cd-title">{e.name}</h1>
        <p className="cd-note">
          {providerName(e.provider)} · <RepoLine e={e} />
          {e.gates.freeze && " · frozen"}
        </p>
      </header>
      <div className="rel-env-page">
        <Panel eyebrow="Where" title="What it deploys to">
          <Facts
            items={[
              { label: "Kubernetes", value: clusters },
              ...(platforms.length ? [{ label: "Platforms", value: platforms.join(", ") }] : []),
              ...(e.provider.startsWith("argocd/") ? [{ label: "Secrets", value: `kiln writes them ${secrets}` }] : []),
              { label: "Concurrency", value: e.concurrency === "queue" ? "a release waits for the one before" : "one release at a time; another is refused" },
              { label: "Agents", value: e.agents.deploy ? (e.agents.approve ? "release and approve here" : "release here") : "may not release here" },
              { label: "Owners", value: e.owners?.release ? "an app's owner teams release it here" : "release here with environments.deploy on it" },
            ]}
          />
        </Panel>
        <Panel eyebrow="Gates" title={gates.length ? "Each release passes" : "No gates"} description={gates.length ? undefined : "Releases go as soon as they're asked for."}>
          {gates.length > 0 && <Facts items={gates} />}
        </Panel>
        <Freeze e={e} />
        <Loadable label="its provider">
          <Provider e={e} />
        </Loadable>
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

/**
 * An environment deployed to by API (docs/deploy-api.md §4): its provider and release repository,
 * where it deploys, the gates each release passes there and who may release; what runs there; and
 * a freeze, put on and lifted here with why (an incident, a holiday): nothing goes until it's lifted.
 */
import * as React from "react";
import { Link, useParams } from "@/lib/router";
import { Snowflake, Sun } from "lucide-react";

import { boardHref, deployApi, describe, providerName, releaseAppHref, releaseHref, type Environment, type Gates } from "@/api/deploy";
import { Button } from "@/components/ui/button";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, Facts, Panel, TextField } from "@/components/ui/misc";
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
  if (g.after?.length) out.push({ label: "After", value: `${g.after.join(", ")}${g.soak ? `, for ${g.soak}` : ""}` });
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

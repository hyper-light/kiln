/**
 * Releases (docs/deploy-api.md §12.3): every app across its environments, what runs in each and
 * what is on its way, and what waits for people, answered here, on a phone too. Each release that
 * moves reloads the board.
 */
import * as React from "react";
import { Link } from "@/lib/router";
import { Check, Plus, X } from "lucide-react";

import { deployApi, describe, newAppHref, providerName, releaseAppHref, releaseEnvHref, releaseHref, STATE_DOT, STATE_WORDS, type Board, type Cell, type Environment, type Release, type Template } from "@/api/deploy";
import { Button } from "@/components/ui/button";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, EmptyState, Panel } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { toast } from "@/components/ui/toast";
import { useRes, type Res } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { RelTime } from "@/views/shared";
import { NewEnvironment } from "./new-environment";
import "@/views/deployments/deployments.css";
import "./releases.css";

/** Server events after which what this section shows is out of date. */
export const DEPLOY_EVENTS = ["deploy", "deploy_registration", "deploy_move"] as const;

export const boardRes: Res<Board> = { key: "deploy/board", fetch: (s) => deployApi.board(s), opts: { on: DEPLOY_EVENTS, pollMs: 30_000 } };
export const environmentsRes: Res<Environment[]> = { key: "deploy/environments", fetch: (s) => deployApi.environments(s), opts: { on: DEPLOY_EVENTS } };
export const templatesRes: Res<Template[]> = { key: "deploy/templates", fetch: (s) => deployApi.templates(s), opts: { on: DEPLOY_EVENTS } };

/** A release's state, as a dot and its words. */
export function ReleaseState({ r }: { r: Release }) {
  return (
    <span className="rel-state">
      <StatusDot status={STATE_DOT[r.state]} title={STATE_WORDS[r.state]} />
      {STATE_WORDS[r.state]}
    </span>
  );
}

/** Approve or reject a release waiting for people; the server says who may. */
export function Answer({ r, compact }: { r: Release; compact?: boolean }) {
  const [busy, setBusy] = React.useState(false);
  const act = async (yes: boolean) => {
    setBusy(true);
    try {
      const next = yes ? await deployApi.approve(r.id) : await deployApi.reject(r.id);
      // The answer is in before the release moves on: say what comes next.
      const description = !yes ? "It is cancelled." : next.state === "waiting_approval" ? "It goes once its gates let it." : STATE_WORDS[next.state];
      toast({ title: yes ? `Approved release ${r.id}` : `Rejected release ${r.id}`, description, tone: yes ? "succeeded" : "info", href: releaseHref(r.id) });
      invalidate("deploy/");
    } catch (e) {
      toast({ title: yes ? "Not approved" : "Not rejected", description: errorMessage(e), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };
  const size = compact ? "xs" : "sm";
  return (
    <div className="rel-answer">
      <Button size={size} disabled={busy} onClick={() => void act(true)}>
        <Check /> Approve
      </Button>
      <Button size={size} variant="ghost-danger" disabled={busy} onClick={() => void act(false)}>
        <X /> Reject
      </Button>
    </div>
  );
}

/** What waits for people, newest first, with its answer at hand. */
function Approvals({ list }: { list: Release[] }) {
  if (!list.length) return null;
  return (
    <Panel eyebrow="Waiting for approval" title={`${list.length} ${list.length === 1 ? "release waits" : "releases wait"} for an answer`} className="rel-approvals-panel">
      <ul className="rel-approvals">
        {list.map((r) => (
          <li key={r.id}>
            <div className="min-w-0">
              <Link to={releaseHref(r.id)} className="rel-link">
                {r.app} → {r.environment}
              </Link>
              <span className="rel-what">{describe(r)}</span>
              <span className="rel-who">
                asked by {r.requested_by} · <RelTime ts={r.created_at} />
              </span>
              {r.message && <p className="rel-msg">{r.message}</p>}
            </div>
            <Answer r={r} compact />
          </li>
        ))}
      </ul>
    </Panel>
  );
}

/** One app in one environment: the release that runs, and the one on its way. */
function CellView({ cell }: { cell: Cell | undefined }) {
  if (!cell) {
    return (
      <td className="rel-cell rel-none">
        <span className="sr-only">not registered</span>
        <span aria-hidden="true">·</span>
      </td>
    );
  }
  const { registration: reg, running, in_flight } = cell;
  return (
    <td className="rel-cell">
      {running ? (
        <Link to={releaseHref(running.id)} className="rel-running">
          <span className="rel-version">{running.version ?? "—"}</span>
          {!reg.enabled && <span className="rel-tag">disabled</span>}
          {reg.paused && <span className="rel-tag">paused</span>}
        </Link>
      ) : (
        <span className="rel-quiet">nothing runs</span>
      )}
      {reg.degraded && (
        <span className="rel-tag rel-tag-alert" title={reg.degraded}>
          needs a person
        </span>
      )}
      {in_flight && (
        <Link to={releaseHref(in_flight.id)} className="rel-flight">
          <StatusDot status={STATE_DOT[in_flight.state]} title={STATE_WORDS[in_flight.state]} />
          <span>
            {describe(in_flight)} · {STATE_WORDS[in_flight.state]}
          </span>
        </Link>
      )}
    </td>
  );
}

function BoardView() {
  const board = useRes(boardRes).data;
  if (!board.apps.length) {
    return (
      <EmptyState
        title="Nothing released by API yet"
        action={
          <Button asChild size="sm">
            <Link to={newAppHref}>
              <Plus /> New app
            </Link>
          </Button>
        }
      >
        Apps, environments and their releases appear here once they exist: make an app here or with <code>kiln app create</code>, add environments with <code>kiln deploy env add</code>, then release
        from the app's page.
      </EmptyState>
    );
  }
  const cells = new Map(board.cells.map((c) => [`${c.registration.app}/${c.registration.environment}`, c]));
  return (
    <>
      <Approvals list={board.waiting_approval} />
      <div className="rel-board-wrap">
        <table className="rel-board">
          <thead>
            <tr>
              <th scope="col">App</th>
              {board.environments.map((e) => (
                <th key={e.name} scope="col">
                  <Link to={releaseEnvHref(e.name)}>{e.name}</Link>
                  <small>
                    {providerName(e.provider)}
                    {e.gates.freeze ? " · frozen" : ""}
                  </small>
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {board.apps.map((a) => (
              <tr key={a.name}>
                <th scope="row">
                  <Link to={releaseAppHref(a.name)}>{a.name}</Link>
                  <small>
                    {a.project}
                    {a.archived ? " · archived" : ""}
                  </small>
                </th>
                {board.environments.map((e) => (
                  <CellView key={e.name} cell={cells.get(`${a.name}/${e.name}`)} />
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </>
  );
}

export default function Releases() {
  return (
    <div className="container-console page cd-page">
      <title>Releases · kiln</title>
      <header className="cd-head">
        <Crumbs index={3} items={[{ label: "Deployments", to: "/deployments" }, { label: "Releases" }]} />
        <div className="rel-head-row">
          <h1 className="cd-title">Releases</h1>
          <div className="rel-head-actions">
            <NewEnvironment />
            <Button asChild size="sm">
              <Link to={newAppHref}>
                <Plus /> New app
              </Link>
            </Button>
          </div>
        </div>
        <p className="cd-note">
          Every app across its environments: what runs in each, what is on its way, and what waits for you. A release is a commit to an environment's release repository, made to run by its
          provider, KilnCD or Argo CD.
        </p>
      </header>
      <Loadable label="the releases">
        <BoardView />
      </Loadable>
    </div>
  );
}

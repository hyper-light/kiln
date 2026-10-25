/**
 * The System page's Updates and Builds (docs/releases.md), moved here from Settings: the fleet's
 * rollout as a figure with the table of machines under it, then the builds runners download. Both
 * read the updates overview and live upgrade jobs; while kiln's release pipeline runs, its jobs
 * too, so each platform says how far its build is.
 */
import * as React from "react";
import { api } from "@/api/client";
import { useServerEvents } from "@/api/events";
import type { Job } from "@/api/types";
import { Hold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { FoldSection } from "@/components/ui/misc";
import { mergeJobs, updatesRes, upsert, useRes, useUpdateJobs } from "@/lib/data";
import { relativeTime, shortSha } from "@/lib/format";
import { useSuspenseResource } from "@/lib/resource";
import { PlatformsPanel } from "./builds";
import { RolloutFigure } from "./figure";
import { MachinesHead, MachinesPanel } from "./runners";
import { readAgainSoon, useMinute, usePending, usePicked, useUploadPicker, useUploading } from "./state";
import {
  archWords,
  fleetOf,
  fleetSummary,
  headline,
  isBuilding,
  latestJob,
  platformCable,
  platformLine,
  platformsOf,
  readoutAll,
  readoutPlatform,
  readoutRunner,
  readoutServer,
  serverView,
} from "./words";
import "./updates.css";

const UPDATES_LEAD = "Upgrade kiln and its runners. A runner keeps what it runs while it's upgraded.";
const BUILDS_LEAD = "What runners download: one build per platform, from kiln's release pipeline, a machine of its own or an upload.";
const FINISHED = new Set(["succeeded", "failed", "cancelled", "skipped", "timed_out"]);

/** 02 / Fleet · Updates (`hold`: as it stands while the page loads). */
export function UpdatesSection({ hold = false }: { hold?: boolean }) {
  return (
    <FoldSection id="system.updates" index="02" label="Fleet" title="Updates">
      <p className="section-lead">{UPDATES_LEAD}</p>
      {hold ? (
        <UpdatesHold />
      ) : (
        <Loadable label="the updates" fallback={<UpdatesHold />}>
          <UpdatesBody />
        </Loadable>
      )}
    </FoldSection>
  );
}

/** 03 / Binaries · Builds (`hold`: as it stands while the page loads). */
export function BuildsSection({ hold = false }: { hold?: boolean }) {
  return (
    <FoldSection id="system.builds" index="03" label="Binaries" title="Builds">
      <p className="section-lead">{BUILDS_LEAD}</p>
      {hold ? (
        <BuildsHold />
      ) : (
        <Loadable label="the builds" fallback={<BuildsHold />}>
          <BuildsBody />
        </Loadable>
      )}
    </FoldSection>
  );
}

/**
 * The jobs of the release run under way (`id`), kept up to date by their events: what each
 * platform's build is doing. The run is read in the background when one starts (the id is
 * deferred), so the sections keep what they show until it is in.
 */
function useReleaseJobs(id: string | null): Job[] | null {
  return useSuspenseResource<Job[] | null>(`updates/release/${id ?? "none"}`, (s) => (id ? api.run(id, s).then((d) => d.jobs) : Promise.resolve(null)), {
    on: id ? ["job"] : [],
    apply: (jobs, ev) => (ev.type === "job" && jobs && ev.job.run_id === id ? upsert(jobs, ev.job, (j) => j.id, false) : jobs),
  }).data;
}

function UpdatesBody() {
  const resource = useRes(updatesRes);
  const data = resource.data;
  const live = useUpdateJobs();
  const nowMs = useMinute();
  const last = data.build?.last ?? null;
  const releaseId = React.useDeferredValue(last && isBuilding(last) ? last.id : null);
  const releaseJobs = useReleaseJobs(releaseId);
  const pending = usePending((s) => s.pending);
  const picked = usePicked((s) => s.picked);
  useServerEvents((ev) => {
    if (ev.type === "agent") readAgainSoon();
    // A release job that ends keeps its binaries: the builds, and the runners waiting on them, change.
    if (ev.type === "job" && releaseId !== null && ev.job.run_id === releaseId && FINISHED.has(ev.job.status)) readAgainSoon();
  });
  const jobs = React.useMemo(() => mergeJobs(data, live), [data, live]);
  const fleet = fleetOf(data, jobs, nowMs);
  const serverJob = latestJob(jobs, "server", "server", nowMs / 1000);
  const serverPending = pending !== null && data.server.build === pending.from;
  const server = serverView(data.server, serverJob, serverPending, data.newest ?? null, nowMs);
  const platforms = platformsOf(data, releaseJobs);
  const entry = picked?.startsWith("runner:") ? fleet.runners.find((e) => `runner:${e.r.name}` === picked) : undefined;
  const platform = picked?.startsWith("build:") ? platforms.find((p) => `build:${p.key}` === picked) : undefined;
  const shown = entry || platform || picked === "server" ? picked : null;
  const readout =
    picked === "server"
      ? readoutServer(data.server, server)
      : entry
        ? readoutRunner(entry, data.waits?.now)
        : platform
          ? readoutPlatform(platform, data.build, nowMs)
          : readoutAll(data, fleet, platforms);
  return (
    <>
      <RolloutFigure
        input={{
          builds: platforms.map((p) => ({
            key: `build:${p.key}`,
            os: p.os,
            arch: archWords(p),
            line: platformLine(p, last),
            tone: p.tone,
            cable: platformCable(p),
            lit: p.lit,
            building: p.state === "building",
          })),
          server: { line: server.scene, tone: server.tone },
          runners: fleet.runners.map(({ r, view }) => ({
            key: `runner:${r.name}`,
            name: r.name,
            line: view.scene,
            tone: view.tone,
            cable: view.cable,
            lit: view.lit,
            switching: view.switching,
            offline: !r.connected,
          })),
        }}
        status={headline(data, fleet, server, platforms)}
        readout={readout}
        picked={shown}
        metrics={{
          current: fleet.current,
          total: data.runners.length,
          upgrading: fleet.upgrading.length,
          newest: data.newest ? shortSha(data.newest.commit) : null,
          since: data.newest ? relativeTime(data.newest.since, nowMs) : null,
        }}
      />
      <MachinesPanel resource={resource} fleet={fleet} server={server} serverJob={serverJob} serverPending={serverPending} summary={fleetSummary(data, fleet)} picked={shown} />
    </>
  );
}

function BuildsBody() {
  const data = useRes(updatesRes).data;
  const live = useUpdateJobs();
  const nowMs = useMinute();
  const last = data.build?.last ?? null;
  const releaseId = React.useDeferredValue(last && isBuilding(last) ? last.id : null);
  const releaseJobs = useReleaseJobs(releaseId);
  const picked = usePicked((s) => s.picked);
  const uploads = useUploadPicker();
  const uploading = useUploading((s) => s.target);
  const jobs = React.useMemo(() => mergeJobs(data, live), [data, live]);
  return (
    <>
      {uploads.input}
      <PlatformsPanel
        data={data}
        platforms={platformsOf(data, releaseJobs)}
        releaseJobs={releaseJobs}
        updateJobs={jobs}
        picked={picked}
        nowMs={nowMs}
        uploading={uploading}
        onChoose={uploads.choose}
      />
      <p className="up-foot">
        <span>Select a machine or a build in the picture to see it here; its row lights below.</span>
        <span>Light follows downloads and new builds · a faint pulse marks a live link</span>
      </p>
    </>
  );
}

/** Updates while they load: the figure's frame and the table's head, at their size. */
function UpdatesHold() {
  return (
    <>
      <section className="up-figure" aria-busy="true">
        <header className="up-head">
          <div>
            <p className="up-eyebrow">Builds · server · runners</p>
            <h3 className="up-fig-title">Rollout</h3>
          </div>
        </header>
        <Hold className="up-stage-hold" label="Loading updates" />
        <dl className="up-metrics" aria-hidden="true">
          <div>
            <dt>Runners current</dt>
            <dd>&nbsp;</dd>
          </div>
          <div>
            <dt>Upgrading</dt>
            <dd>&nbsp;</dd>
          </div>
          <div>
            <dt>Newest kiln</dt>
            <dd>&nbsp;</dd>
          </div>
        </dl>
      </section>
      <section className="up-panel" aria-busy="true">
        <header className="up-panel-head">
          <div className="up-panel-words">
            <h3>Server and runners</h3>
            <p>&nbsp;</p>
          </div>
        </header>
        <table className="up-table up-table-hold">
          <MachinesHead />
          <tbody aria-hidden="true">
            {Array.from({ length: 4 }, (_, i) => (
              <tr key={i}>
                <td colSpan={6} />
              </tr>
            ))}
          </tbody>
        </table>
      </section>
    </>
  );
}

/** Builds while they load: the panel's head, and the platforms' space. */
function BuildsHold() {
  return (
    <section className="up-panel up-panel-flush" aria-busy="true">
      <header className="up-panel-head">
        <div className="up-panel-words">
          <h3>Platforms</h3>
          <p>&nbsp;</p>
        </div>
      </header>
      <Hold className="up-platforms-hold" label="Loading the builds" />
    </section>
  );
}

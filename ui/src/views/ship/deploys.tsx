/**
 * Ship's Deploys (docs/ci.md W26-R24; Ada's approved stills of 2026-10-30, v5 and v6): every app
 * KilnCD deploys, as Ship's rows by project (the stand word, the edge, the pips; no buttons), each
 * project's heading saying what it needs. An opened app is the jobs pane's anatomy: one strip says
 * what's picked and does what's done (Sync, Refresh, Roll back and Stop; Promote and Abort while a
 * rollout runs; a setting's change when one is picked), asking again where the stills do; under it
 * the app's objects grouped by what owns them, its rollout, its history (any commit a rollback can
 * go to) and its settings; the picked one's detail beside them (Logs · Events · Manifest, or Diff
 * for an object that differs; a commit against what runs), or under its line on a phone, where the
 * strip comes again. Its log is the commits' Output, the run page's LogViewer. Flat throughout;
 * every height held, so nothing moves as you pick, act or the apps go on. Data through Suspense; no
 * effects.
 */
import * as React from "react";
import { ChevronDown } from "lucide-react";
import { cdApi } from "@/api/cd";
import { whoSaid } from "@/api/review";
import {
  shipApi,
  type ShipDeployAct,
  type ShipDeployGroup,
  type ShipDeployItem,
  type ShipDeployLine,
  type ShipDeployPane,
  type ShipDeployRollout,
  type ShipDeployRow,
  type ShipDeploySetting,
  type ShipDeployVerb,
} from "@/api/ship";
import type { CdResourceKey } from "@/api/types";
import { FacetBox, type Chosen } from "@/components/ui/facet-box";
import { Loadable, Loading } from "@/components/ui/loading";
import { EmptyState, PageIntro } from "@/components/ui/misc";
import { JourneyStrip, StatusGlyph } from "@/components/ui/status-glyph";
import { GLYPHS } from "@/components/ui/status-glyph-spec";
import { sendingProps, useAction } from "@/lib/action";
import { errorMessage, useMediaQuery } from "@/lib/hooks";
import { useSearchParams } from "@/lib/router";
import { invalidate, useSuspenseResource } from "@/lib/resource";
import { relativeTime } from "@/lib/format";
import LogViewer from "@/views/run-detail/log-viewer";
import { Shimmer } from "@/views/harness/signals";
import { SectionHead } from "./head";
import { glyphOf, optionsOf, strip } from "./model";
import "./ship.css";
import { ShipViews } from "./views";

/** The events after which Ship's Deploys asks again (W26-R24): CD's own. */
const CD_EVENTS = ["cd_apps", "cd_rollout", "cd_app_deleted"] as const;
/** The facets apps are found by, beside the words typed. */
const FACETS = ["project", "environment", "cluster"] as const;
/** A detail's log's height in terminal rows, whatever it holds: the commits' Output's on a phone, the side's on a desktop (ship.css's `.dp-op` heights). */
const PHONE_ROWS = 14;
const SIDE_ROWS = 32;

export default function ShipDeploysPage() {
  return (
    <div className="container-console page ship">
      <title>Deploys · Ship · kiln</title>
      <PageIntro index={2} label="Ship" title="Ship." lead="Every commit and change on its way from review to production, and what it needs of you." />
      <ShipViews />
      <Loadable label="Loading deploys">
        <DeploysList />
      </Loadable>
    </div>
  );
}

const fieldOf = (r: ShipDeployRow, k: (typeof FACETS)[number]) => (k === "project" ? r.project : k === "environment" ? (r.environment ?? "") : r.cluster);

function DeploysList() {
  const [params, setParams] = useSearchParams();
  const list = useSuspenseResource("ship/deploys", (s) => shipApi.deploys("", s), { on: CD_EVENTS, pollMs: 30_000 });
  const q = params.get("q") ?? "";
  const open = params.get("app");
  // The app a link opened the page on is brought into view, once; one opened here leaves the page where it is.
  const [landed] = React.useState(open);
  const chosen: Chosen = Object.fromEntries(FACETS.map((k): [string, string[]] => [k, (params.get(k) ?? "").split(",").filter(Boolean)]).filter(([, v]) => v.length > 0));
  const set = (patch: Record<string, string | null>) => {
    const next = new URLSearchParams(params);
    for (const [k, v] of Object.entries(patch)) {
      if (v) next.set(k, v);
      else next.delete(k);
    }
    setParams(next, { replace: true });
  };
  const all = list.data.projects.flatMap((p) => p.apps);
  // Found as the server finds: a project, an app, an environment, a cluster or a namespace.
  const words = q.trim().toLowerCase();
  const shows = (r: ShipDeployRow) =>
    (!words || [r.project, r.app, r.environment ?? "", r.cluster, r.namespace].some((v) => v.toLowerCase().includes(words))) &&
    FACETS.every((k) => !chosen[k]?.length || chosen[k].includes(fieldOf(r, k)));
  const projects = list.data.projects.map((p) => ({ ...p, apps: p.apps.filter(shows) })).filter((p) => p.apps.length > 0);
  return (
    <>
      <FacetBox
        className="ship-facet"
        label="Find apps"
        placeholder="Project, app, environment, cluster…"
        query={q}
        onQuery={(v) => set({ q: v || null })}
        facets={FACETS.map((k) => ({ id: k, label: k === "project" ? "Project" : k === "environment" ? "Environment" : "Cluster", options: optionsOf(all.map((r) => fieldOf(r, k))) }))}
        chosen={chosen}
        onChosen={(next) => set(Object.fromEntries(FACETS.map((k) => [k, next[k]?.join(",") || null])))}
      />
      {projects.length ? (
        <ul className="ship-list dp-list" aria-label="Apps by project">
          {projects.map((p) => (
            <React.Fragment key={p.project}>
              <li className="dp-proj">
                <span>{p.project}</span>
                <span data-needs={p.needs || undefined}>{p.say}</span>
              </li>
              {p.apps.map((r) => (
                <DeployRow key={r.app} row={r} open={open === r.app} landed={landed === r.app} onOpen={(o) => set({ app: o ? r.app : null })} />
              ))}
            </React.Fragment>
          ))}
        </ul>
      ) : (
        <EmptyState title={all.length ? "No app is one of those" : "KilnCD deploys no app yet"}>
          {all.length ? "Find by another project, app, environment or cluster." : "An app comes from a repository's .kiln/deploy.yaml, or from the API."}
        </EmptyState>
      )}
    </>
  );
}

/** Who did it, as the stills name them: a person or kiln by name, an agent by its id's first eight, and whom it acted for (`agent:01a18b08 for admin`). */
const byWord = (by: string) => by.replace(/\bagent:([0-9a-z]{8})[0-9a-z]*/gi, "agent:$1").replace(/\buser:/g, "");

/** A row a link opened the page on, brought into view as it's first drawn. */
const intoView = (el: HTMLLIElement | null) => el?.scrollIntoView({ block: "start" });

/** An app as Ship's row: its name, its stand and line, its latest deploy's pips, who and when, where it deploys and what runs. No buttons but to open it. */
function DeployRow({ row, open, landed, onOpen }: { row: ShipDeployRow; open: boolean; landed: boolean; onOpen: (open: boolean) => void }) {
  return (
    <li className="ship-row dp-row" data-stand={row.stand} data-app={row.app} ref={landed ? intoView : undefined}>
      <div className="ship-r">
        <button type="button" className="ship-main" aria-expanded={open} onClick={() => onOpen(!open)}>
          <span className="ship-name">{row.name}</span>
          <span className="ship-line">
            <span className="ship-stand" data-stand={row.stand}>
              <Shimmer on={row.stand === "shipping"} flow={row.app}>
                {row.word}
              </Shimmer>
            </span>
            <span className="ship-text">{row.line}</span>
          </span>
        </button>
        <span className="ship-pips">
          <JourneyStrip pips={strip(row.pips)} />
        </span>
        <span className="ship-metas">
          {/* Who and when, as the stills draw them: the who whole, the when in the fewest words where the column has room for it. */}
          <span className="dp-by">
            {row.by ? (
              <>
                <span className="ship-who">
                  <StatusGlyph state={row.by.startsWith("agent:") ? "agent" : "person"} size={12} />
                  <span>{byWord(row.by)}</span>
                </span>
                <span>·</span>
              </>
            ) : null}
            <span className="dp-ago">{row.at ? since(row.at * 1000) : "never"}</span>
          </span>
          <span className="ship-m2">
            {row.environment ? (
              <span className="ship-env" data-env={row.environment === "production" ? "production" : "staging"}>
                <StatusGlyph state="flag" size={12} />
                {row.environment}
              </span>
            ) : null}
            <span className="ship-sha">{row.runs ?? "—"}</span>
          </span>
        </span>
        <span className="ship-acts" aria-hidden="true" />
        <button type="button" className="ship-go" aria-expanded={open} aria-label={`${open ? "Close" : "Open"} ${row.name}`} onClick={() => onOpen(!open)}>
          <ChevronDown size={16} />
        </button>
      </div>
      {open ? (
        <div className="ship-open dp-open">
          <Loadable label={`Loading ${row.name}`} fallback={<Loading label={`Loading ${row.name}`} block className="dp-slot" />}>
            <DeployPane row={row} />
          </Loadable>
        </div>
      ) : null}
    </li>
  );
}

/** The words that pick an app's objects for Sync. */
type Word = "out of sync" | "unhealthy" | "all";
const WORDS: Word[] = ["out of sync", "unhealthy", "all"];
const UNHEALTHY = new Set(["degraded", "failed", "error", "queued"]);
/** What asks again before it's done (the stills: Refresh's choice, Roll back's, and what can't be taken back). */
type Armed = "refresh" | "rollback" | "stop" | "abort" | "delete";
/** A setting's label on its line. */
const SETTING_WORD: Record<string, string> = { auto_sync: "Auto-sync", auto_rollback: "Auto-rollback", prune: "Prune", self_heal: "Self-heal" };
/** The settings the pane shows, in its order. */
const SETTINGS_SHOWN = ["auto_sync", "auto_rollback", "prune"];
/** A kind's word on its line, where it's long. */
const KIND_WORD: Record<string, string> = { PersistentVolumeClaim: "PVC", HorizontalPodAutoscaler: "HPA", PodDisruptionBudget: "PDB" };

/** A pick's key: an object (`o:` its key), a workload's head (`w:`), a history line (`h:` its index), a rollout step (`s:` its number), a setting (`c:`), deleting the app (`c:delete`). */
type Key = string;

interface Pick {
  at: Key | null;
  sel: Key[];
  word: Word | null;
}

/** An object of the pane's, with its key. */
type Obj = { k: Key; item: ShipDeployItem; owner: boolean };

/** Every object, in the list's order: each workload's head, then its objects, then the rest. */
function objectsOf(groups: readonly ShipDeployGroup[]): Obj[] {
  return groups.flatMap((g) => [...(g.owner ? [{ k: `w:${g.owner.key}`, item: g.owner, owner: true }] : []), ...g.items.map((item) => ({ k: `o:${item.key}`, item, owner: false }))]);
}

/** What's shown before anything's picked: the rollout's step under way, else a pod's log, else what differs, else the first. */
function firstPick(pane: ShipDeployPane, objs: readonly Obj[]): Key | null {
  const step = pane.rollout?.under_way ? pane.rollout.steps.find((s) => s.state !== "passed" && s.state !== "none") : undefined;
  if (step) return `s:${step.n}`;
  return (objs.find((o) => o.item.kind === "Pod") ?? objs.find((o) => o.item.differs) ?? objs[0])?.k ?? (pane.history.length ? "h:0" : null);
}

const keyOf = (i: ShipDeployItem): CdResourceKey => ({ group: i.group, kind: i.kind, namespace: i.namespace, name: i.name });

/** How long ago, in the fewest words (`36m`), as a line's value says it. */
const since = (ms: number) => relativeTime(ms / 1000).replace(/ ago$/, "");

/** The deploy line's head: `#153 · 15878aa`, or a commit's short hash. */
const lineHead = (l: ShipDeployLine) => (l.deploy != null ? `#${l.deploy} · ${l.short ?? ""}` : (l.short ?? ""));

/** A rollout by the deploy it rolls out (`Rollout #39`), else its commit. */
const rolloutName = (r: ShipDeployRollout) => (r.deploy != null ? `Rollout #${r.deploy}` : r.revision ? `Rollout ${r.revision}` : "Rollout");

/** An opened app: the strip, its objects, rollout, history and settings, and the picked one's detail. */
function DeployPane({ row }: { row: ShipDeployRow }) {
  const pane = useSuspenseResource(`ship/deploy/${row.app}`, (s) => shipApi.deploy(row.app, s), { on: CD_EVENTS, pollMs: 15_000 });
  const p = pane.data;
  const phone = useMediaQuery("(max-width: 700px)");
  const objs = React.useMemo(() => objectsOf(p.groups), [p.groups]);
  const [pick, setPick] = React.useState<Pick>({ at: null, sel: [], word: null });
  const [armed, setArmed] = React.useState<Armed | null>(null);
  const [said, setSaid] = React.useState<{ text: React.ReactNode; refused: boolean } | null>(null);
  const at = pick.at ?? firstPick(p, objs);
  const sel = pick.sel.length ? pick.sel : at ? [at] : [];
  const choose = (next: Pick) => {
    setPick(next);
    setArmed(null);
    setSaid(null);
  };
  const onLine = (k: Key) => choose({ at: k, sel: [k], word: null });
  // An object's glyph adds it to what Sync takes, or takes it out; the line shown stays.
  const onGlyph = (k: Key) => {
    const has = sel.includes(k);
    const rest = has ? sel.filter((x) => x !== k) : [...sel.filter((x) => x.startsWith("o:") || x.startsWith("w:")), k];
    choose({ at: has && k === at ? (rest[0] ?? null) : k.startsWith("o:") || k.startsWith("w:") ? (at ?? k) : k, sel: rest, word: null });
  };
  const onWord = (w: Word) => {
    if (pick.word === w) return choose({ at, sel: at ? [at] : [], word: null });
    const keys = objs.filter((o) => w === "all" || (w === "out of sync" ? o.item.differs : UNHEALTHY.has(o.item.state))).map((o) => o.k);
    choose({ at: keys.includes(at ?? "") ? at : (keys[0] ?? at), sel: keys, word: w });
  };
  const shown = at ? resolve(p, objs, at) : null;
  const picked = sel.map((k) => objs.find((o) => o.k === k)).filter((o): o is Obj => Boolean(o));
  const rollout = p.rollout?.under_way ? p.rollout : null;
  const no = (v: ShipDeployVerb) => p.why_not[v];
  // One change to the app at a time (W21-R15): the verb pressed says it's on its way, and so does its
  // own once the strip is back to its four (Roll back for Roll back's answer); the others are held
  // until it's back, and Keep still takes a question back.
  const [sent, setSent] = React.useState<ShipDeployVerb | null>(null);
  const [act, acting] = useAction(async (body: Omit<ShipDeployAct, "app">, done: (r: unknown) => React.ReactNode) => {
    setSent(body.verb);
    setArmed(null);
    try {
      const r = await shipApi.deployAct({ app: row.app, ...body });
      setSaid({ text: done(r.done), refused: false });
    } catch (e) {
      setSaid({ text: errorMessage(e), refused: true });
    } finally {
      // A deleted app has no pane to read again: the list drops its row.
      if (body.verb !== "delete") pane.reload();
      invalidate("ship/deploys");
    }
  });
  const sending = (v: ShipDeployVerb) => acting && sent === v;
  const held = (v: ShipDeployVerb) => acting && sent !== v;
  // Sync takes what's picked of what differs; with none of it, all that differs.
  const differing = picked.filter((o) => o.item.differs);
  const sync = () =>
    act(differing.length ? { verb: "sync", resources: differing.map((o) => o.item.key) } : { verb: "sync" }, () =>
      differing.length ? (
        <>
          Syncing <b>{differing.length === 1 ? `${differing[0]?.item.kind} ${differing[0]?.item.name}` : `${differing.length} objects`}</b> of {row.name}.
        </>
      ) : (
        <>
          Syncing <b>{row.name}</b> to what git renders.
        </>
      ),
    );
  const line = shown?.kind === "line" ? shown.line : null;
  const setting = shown?.kind === "setting" ? shown.setting : null;
  const deleting = shown?.kind === "delete";
  const del = (cascade: "foreground" | "orphan") =>
    act({ verb: "delete", cascade }, (r) =>
      (r as { cascade?: boolean } | null)?.cascade ? (
        <>
          Deleting <b>{row.name}</b> and its objects.
        </>
      ) : (
        <>
          Deleting <b>{row.name}</b>; its objects stay in {p.about.cluster}/{p.about.namespace}.
        </>
      ),
    );
  const back = line && !line.now && (line.to != null || line.revision) ? line : null;
  const rollBack = () =>
    act(back ? (back.to != null ? { verb: "rollback", to: back.to } : { verb: "rollback", revision: back.revision ?? undefined }) : { verb: "rollback" }, () => (
      <>
        Rolling {row.name} back{back ? ` to ${lineHead(back)}` : ""}.
      </>
    ));
  const verbs: Verb[] = armed
    ? armedVerbs(armed, sending, held, {
        keep: () => setArmed(null),
        refresh: (hard) => act({ verb: "refresh", hard }, () => (hard ? <>Rendering {row.name} again and comparing it.</> : <>Comparing {row.name} again.</>)),
        rollback: rollBack,
        stop: () => act({ verb: "stop" }, () => <>Stopping {row.name}&apos;s sync.</>),
        abort: () => act({ verb: "abort", rollout: rollout?.id }, () => <>Aborting the rollout: its traffic goes back.</>),
        delete: del,
      })
    : deleting
      ? [
          { label: "Delete", on: !no("delete") && !held("delete"), sending: sending("delete"), run: () => setArmed("delete") },
          { label: "Refresh", on: false, run: () => undefined },
          { label: "Roll back", on: false, run: () => undefined },
          { label: rollout ? "Abort" : "Stop", on: false, run: () => undefined },
        ]
      : setting
        ? [
            {
              label: setting.on ? "Turn off" : "Turn on",
              on: setting.can && (setting.key === "auto_sync" || setting.key === "auto_rollback") && !held(setting.key === "auto_rollback" ? "auto_rollback" : "auto_sync"),
              sending: sending(setting.key === "auto_rollback" ? "auto_rollback" : "auto_sync"),
              run: () =>
                act({ verb: setting.key === "auto_rollback" ? "auto_rollback" : "auto_sync", on: !setting.on }, () => (
                  <>
                    {SETTING_WORD[setting.key]} is {setting.on ? "off" : "on"} for {row.name}.
                  </>
                )),
            },
            { label: "Refresh", on: false, run: () => undefined },
            { label: "Roll back", on: false, run: () => undefined },
            { label: rollout ? "Abort" : "Stop", on: false, run: () => undefined },
          ]
        : rollout
          ? [
              {
                label: "Promote",
                on: !no("promote") && !held("promote"),
                sending: sending("promote"),
                run: () => act({ verb: "promote", rollout: rollout.id }, () => <>Promoting the rollout past its step.</>),
              },
              { label: "Refresh", on: !held("refresh"), sending: sending("refresh"), run: () => setArmed("refresh") },
              { label: "Roll back", on: !no("rollback") && (!line || Boolean(back)) && !held("rollback"), sending: sending("rollback"), run: () => setArmed("rollback") },
              { label: "Abort", on: !no("abort") && !held("abort"), sending: sending("abort"), run: () => setArmed("abort") },
            ]
          : [
              { label: "Sync", on: !no("sync") && !held("sync"), sending: sending("sync"), run: sync },
              { label: "Refresh", on: !held("refresh"), sending: sending("refresh"), run: () => setArmed("refresh") },
              { label: "Roll back", on: !no("rollback") && (!line || Boolean(back)) && !held("rollback"), sending: sending("rollback"), run: () => setArmed("rollback") },
              { label: "Stop", on: !no("stop") && !held("stop"), sending: sending("stop"), run: () => setArmed("stop") },
            ];
  const say = armed ? armedSay(armed, row, p, back) : said ? said.text : pickSay(p, row, shown, picked, pick.word, rollout);
  // The words pick objects: they show while an object (or nothing) is picked, a picked one lighting its own.
  const words = !armed && !setting && !line && !deleting && !shown?.kind.startsWith("step");
  const one = !pick.word && picked.length === 1 ? picked[0] : undefined;
  const lit = (w: Word): boolean | "mixed" => (pick.word === w ? true : one && w !== "all" && (w === "out of sync" ? one.item.differs : UNHEALTHY.has(one.item.state)) ? "mixed" : false);
  const detail = shown ? <Detail pane={p} row={row} shown={shown} rows={phone ? PHONE_ROWS : SIDE_ROWS} /> : null;
  // On a phone, the picked one's detail is right under its line, as the approved stills draw it.
  const below = (k: Key) => (phone && k === at ? <li className="jp-detail">{detail}</li> : null);
  const settingsFrom = p.settings.some((s) => s.from === "deploy-file") ? "from .kiln/deploy.yaml" : "set here";
  return (
    <section aria-label="Deploys" className="ship-sec dp">
      <SectionHead label={row.name}>
        {p.about.cluster}/{p.about.namespace} · {p.about.objects} objects · {p.about.source}
      </SectionHead>
      <div className="jp">
        <Strip say={say} tone={armed === "refresh" || armed === "rollback" ? "ask" : armed || said?.refused ? "warn" : undefined} words={words} lit={lit} onWord={onWord} verbs={verbs} />
        <div className="jp-body dp-body">
          <ol className="jp-list" aria-label={`${row.name}'s objects, history and settings`}>
            {p.rollout ? (
              <>
                <li className="jp-g" data-group="rollout">
                  <span>{rolloutName(p.rollout)}</span>
                  <span>{p.rollout.note}</span>
                </li>
                {p.rollout.steps.map((s) => (
                  <React.Fragment key={s.n}>
                    <Line k={`s:${s.n}`} state={s.state} kind={`Step ${s.n}`} name={s.what} v={s.v} picked={sel.includes(`s:${s.n}`)} at={at === `s:${s.n}`} onLine={onLine} />
                    {below(`s:${s.n}`)}
                  </React.Fragment>
                ))}
              </>
            ) : null}
            {p.groups.map((g) => (
              <React.Fragment key={g.name}>
                {g.owner ? (
                  <li className="jp-g dp-head" data-sel={sel.includes(`w:${g.owner.key}`) || undefined} data-at={at === `w:${g.owner.key}` || undefined} data-key={`w:${g.owner.key}`}>
                    <button type="button" className="jp-line" aria-current={at === `w:${g.owner.key}` || undefined} onClick={() => onLine(`w:${g.owner?.key ?? ""}`)}>
                      <span>{g.name}</span>
                      <span>{g.note}</span>
                    </button>
                  </li>
                ) : (
                  <li className="jp-g" data-group={g.name}>
                    <span>{g.name}</span>
                    <span>{g.note}</span>
                  </li>
                )}
                {g.owner ? below(`w:${g.owner.key}`) : null}
                {g.items.map((i) => (
                  <React.Fragment key={i.key}>
                    <Line
                      k={`o:${i.key}`}
                      state={i.state}
                      quiet={Boolean(g.owner) && i.group === "apps" && i.kind === "ReplicaSet"}
                      kind={KIND_WORD[i.kind] ?? i.kind}
                      name={i.name}
                      v={i.v}
                      picked={sel.includes(`o:${i.key}`)}
                      at={at === `o:${i.key}`}
                      onLine={onLine}
                      onGlyph={onGlyph}
                    />
                    {below(`o:${i.key}`)}
                  </React.Fragment>
                ))}
              </React.Fragment>
            ))}
            <li className="jp-g" data-group="history">
              <span>History</span>
              <span>deploys and main&apos;s commits</span>
            </li>
            {p.history.map((l, i) => (
              <React.Fragment key={`${l.deploy ?? ""}-${l.sha ?? i}`}>
                <Line
                  k={`h:${i}`}
                  state={l.state}
                  head={lineHead(l)}
                  name={l.subject}
                  v={l.now ? `runs now · ${since(l.at_ms)}` : since(l.at_ms)}
                  dim={l.dim}
                  picked={sel.includes(`h:${i}`)}
                  at={at === `h:${i}`}
                  onLine={onLine}
                />
                {below(`h:${i}`)}
              </React.Fragment>
            ))}
            <li className="jp-g" data-group="settings">
              <span>Settings</span>
              <span>{settingsFrom}</span>
            </li>
            {p.settings
              .filter((s) => SETTINGS_SHOWN.includes(s.key))
              .map((s) => (
                <React.Fragment key={s.key}>
                  <Line
                    k={`c:${s.key}`}
                    state={s.on ? "passed" : "none"}
                    kind={SETTING_WORD[s.key] ?? s.key}
                    name={s.on ? "on" : "off"}
                    v={s.what}
                    picked={sel.includes(`c:${s.key}`)}
                    at={at === `c:${s.key}`}
                    onLine={onLine}
                  />
                  {below(`c:${s.key}`)}
                </React.Fragment>
              ))}
            <Line k="c:delete" state="none" kind="App" name={`delete ${row.name}`} v="" picked={sel.includes("c:delete")} at={at === "c:delete"} onLine={onLine} />
            {below("c:delete")}
          </ol>
          {phone ? null : <div className="jp-side dp-side">{detail}</div>}
        </div>
      </div>
    </section>
  );
}

/** What a pick's key shows. */
type Shown =
  | { kind: "object"; obj: Obj }
  | { kind: "line"; line: ShipDeployLine; i: number }
  | { kind: "step"; rollout: ShipDeployRollout; n: number }
  | { kind: "setting"; setting: ShipDeploySetting }
  | { kind: "delete" };

function resolve(p: ShipDeployPane, objs: readonly Obj[], k: Key): Shown | null {
  const [tag, rest] = [k.slice(0, 2), k.slice(2)];
  if (tag === "o:" || tag === "w:") {
    const obj = objs.find((o) => o.k === k);
    return obj ? { kind: "object", obj } : null;
  }
  if (tag === "h:") {
    const i = Number(rest);
    const line = p.history[i];
    return line ? { kind: "line", line, i } : null;
  }
  if (tag === "s:") return p.rollout ? { kind: "step", rollout: p.rollout, n: Number(rest) } : null;
  if (k === "c:delete") return { kind: "delete" };
  if (tag === "c:") {
    const setting = p.settings.find((s) => s.key === rest);
    return setting ? { kind: "setting", setting } : null;
  }
  return null;
}

/** One line: its glyph (an object's adds it to what Sync takes), its kind or head, its name, its value. */
function Line({
  k,
  state,
  kind,
  head,
  name,
  v,
  dim,
  quiet,
  picked,
  at,
  onLine,
  onGlyph,
}: {
  k: Key;
  state: string;
  kind?: string;
  head?: string;
  name: string;
  v: string;
  dim?: boolean;
  /** Drawn quiet (the slashed ring), its own state its glyph's name: a workload's ReplicaSet, whose pods carry how it runs. */
  quiet?: boolean;
  picked: boolean;
  at: boolean;
  onLine: (k: Key) => void;
  onGlyph?: (k: Key) => void;
}) {
  const glyph = quiet ? <StatusGlyph state="superseded" size={12} label={GLYPHS[glyphOf(state)].word} /> : <StatusGlyph state={glyphOf(state)} size={12} />;
  return (
    <li className="jp-j" data-key={k} data-sel={picked || undefined} data-at={at || undefined} data-dim={dim || undefined} data-state={state}>
      {onGlyph ? (
        <button type="button" className="jp-glyph" aria-pressed={picked} aria-label={`${picked ? "Take" : "Add"} ${name} ${picked ? "out of" : "to"} what Sync takes`} onClick={() => onGlyph(k)}>
          {glyph}
        </button>
      ) : (
        <span className="jp-glyph" aria-hidden="true">
          {glyph}
        </span>
      )}
      <button type="button" className="jp-line" aria-current={at || undefined} onClick={() => onLine(k)}>
        <span className="n">
          {kind ? <em>{kind}</em> : null}
          {head ? <s>{head}</s> : null}
          {name}
        </span>
        <span className="v">{v}</span>
      </button>
    </li>
  );
}

interface Verb {
  label: string;
  on: boolean;
  run: () => void;
  /** Its verb's request is on its way: it says so, and a press does nothing. */
  sending?: boolean;
  keep?: boolean;
  /** An answer to what was asked: amber where it can be gone back on, else pink. */
  go?: "ask" | "warn";
  wide?: number;
}

/** The answers to what's asked again: Keep, and the action (Refresh's two, Delete's two), held while another change to the app is on its way. */
function armedVerbs(
  armed: Armed,
  sending: (verb: ShipDeployVerb) => boolean,
  held: (verb: ShipDeployVerb) => boolean,
  run: { keep: () => void; refresh: (hard: boolean) => void; rollback: () => void; stop: () => void; abort: () => void; delete: (cascade: "foreground" | "orphan") => void },
): Verb[] {
  const keep: Verb = { label: "Keep", on: true, keep: true, run: run.keep };
  switch (armed) {
    case "refresh":
      return [
        keep,
        { label: "Refresh", on: !held("refresh"), sending: sending("refresh"), go: "ask", run: () => run.refresh(false) },
        { label: "Hard refresh", on: !held("refresh"), sending: sending("refresh"), go: "ask", wide: 2, run: () => run.refresh(true) },
      ];
    case "rollback":
      return [keep, { label: "Roll back", on: !held("rollback"), sending: sending("rollback"), go: "ask", wide: 3, run: run.rollback }];
    case "stop":
      return [keep, { label: "Stop the sync", on: !held("stop"), sending: sending("stop"), go: "warn", wide: 3, run: run.stop }];
    case "abort":
      return [keep, { label: "Abort it", on: !held("abort"), sending: sending("abort"), go: "warn", wide: 3, run: run.abort }];
    case "delete":
      return [
        keep,
        { label: "Delete all", on: !held("delete"), sending: sending("delete"), go: "warn", run: () => run.delete("foreground") },
        { label: "Delete, leave objects", on: !held("delete"), sending: sending("delete"), go: "warn", wide: 2, run: () => run.delete("orphan") },
      ];
  }
}

/** The strip: what's picked and what an action would do, the words that pick objects, the four actions. */
function Strip({
  say,
  tone,
  words,
  lit,
  onWord,
  verbs,
}: {
  say: React.ReactNode;
  tone: "warn" | "ask" | undefined;
  /** The words pick objects: where they don't apply (a question, a setting, a commit, a step), what's said has their room, the strip its height. */
  words: boolean;
  /** A word's state: picked by it, or the one picked object's own. */
  lit: (w: Word) => boolean | "mixed";
  onWord: (w: Word) => void;
  verbs: Verb[];
}) {
  return (
    <div className="jp-strip">
      <div className="jp-say" data-tone={tone} data-room={!words || undefined}>
        <div className="jp-said" aria-live="polite">
          {say}
        </div>
        <div className="jp-q" aria-hidden={!words || undefined}>
          <span>pick:</span>
          {WORDS.map((w, i) => (
            <React.Fragment key={w}>
              {i ? <span aria-hidden>·</span> : null}
              <button type="button" aria-pressed={lit(w)} disabled={!words} onClick={() => onWord(w)}>
                {w}
              </button>
            </React.Fragment>
          ))}
        </div>
      </div>
      <div className="jp-verbs dp-verbs" data-armed={verbs.some((v) => v.go) || undefined}>
        {verbs.map((v) => (
          <button
            key={v.label}
            type="button"
            disabled={!v.on}
            data-keep={v.keep || undefined}
            data-go={v.go}
            style={v.wide ? { gridColumn: `span ${v.wide}` } : undefined}
            {...sendingProps(Boolean(v.sending))}
            onClick={v.sending ? undefined : v.run}
          >
            {v.label}
          </button>
        ))}
      </div>
    </div>
  );
}

/** Where a setting's word comes from, in a few words. */
function fromWord(s: ShipDeploySetting): string {
  switch (s.from) {
    case "deploy-file":
      return "as .kiln/deploy.yaml says";
    case "app":
      return "as a person set it here";
    case "repo":
      return "as its repository's setting says";
    default:
      return "kiln's default";
  }
}

/** The strip's words for what's picked. */
function pickSay(p: ShipDeployPane, row: ShipDeployRow, shown: Shown | null, picked: readonly Obj[], word: Word | null, rollout: ShipDeployRollout | null): React.ReactNode {
  if (word)
    return picked.length ? (
      <>
        <b>
          {picked.length} {word === "all" ? (picked.length === 1 ? "object" : "objects") : word}
        </b>{" "}
        picked · Sync applies just what of {picked.length === 1 ? "it" : "them"} differs from git
      </>
    ) : (
      <>Nothing {word === "all" ? "here" : word} to pick</>
    );
  if (picked.length > 1)
    return (
      <>
        <b>{picked.length} objects</b> picked · Sync applies just what of them differs from git
      </>
    );
  if (!shown) return <>Pick an object, a commit or a setting</>;
  switch (shown.kind) {
    case "object": {
      const i = shown.obj.item;
      return (
        <>
          <b>
            {i.kind} {i.name}
          </b>{" "}
          picked · {i.node ? `running on ${i.node} · ` : ""}
          {i.v}
          {i.differs ? (
            <Loadable label="Reading what differs" fallback={<></>}>
              <Keys app={row.app} item={i} />
            </Loadable>
          ) : null}
          {i.differs ? " · Sync applies just this object" : ""}
        </>
      );
    }
    case "line": {
      const l = shown.line;
      const what = l.now ? "what runs now" : l.deploy != null ? `deploy #${l.deploy}, ${relativeTime(l.at_ms / 1000)}` : "never deployed";
      return (
        <>
          <b>{lineHead(l)}</b> picked · {what}
          {l.now ? "" : l.to != null || l.revision ? ` · Roll back takes ${row.name} to it` : ""}
        </>
      );
    }
    case "step": {
      const r = shown.rollout;
      return (
        <>
          <b>{rolloutName(r)}</b> picked · {r.strategy ? `${r.strategy} at ${r.weight ?? 0}%, ${r.said ?? ""}` : r.note}
          {rollout ? (r.promote_to != null ? ` · Promote moves it to ${r.promote_to}% now` : " · Promote moves it on now") : ""}
        </>
      );
    }
    case "setting": {
      const s = shown.setting;
      return (
        <>
          <b>{SETTING_WORD[s.key] ?? s.key}</b> picked · {s.on ? "on" : "off"}, {fromWord(s)}
          {s.can ? "" : ` · ${p.about.source.includes("@") ? ".kiln/deploy.yaml" : "its definition"} sets it`}
        </>
      );
    }
    case "delete":
      return (
        <>
          <b>Delete {row.name}</b> picked · {p.why_not.delete ?? `its ${p.about.objects} objects go too, unless you leave them`}
        </>
      );
  }
}

/** What's asked again, in the strip, before it's done. */
function armedSay(armed: Armed, row: ShipDeployRow, p: ShipDeployPane, back: ShipDeployLine | null): React.ReactNode {
  switch (armed) {
    case "refresh":
      return (
        <>
          <b>Refresh</b> reads git and the cluster again and compares them. <b>Hard refresh</b> also renders {p.about.source.split(" @ ")[0]} from scratch, its cache dropped.
        </>
      );
    case "rollback":
      if (back && back.deploy == null)
        return (
          <>
            Roll <b>{row.name}</b> back to <b>{back.short}</b>? It was never deployed: kiln renders {p.about.source.split(" @ ")[0]} at {back.short}, syncs it, and holds it there until trunk changes
            what it renders.
          </>
        );
      return (
        <>
          Roll <b>{row.name}</b> back to <b>{back ? lineHead(back) : "the deploy before"}</b>? kiln renders it again, syncs it, and holds it there until trunk changes what it renders.
        </>
      );
    case "stop":
      return (
        <>
          Stop <b>{row.name}</b>&apos;s sync? What it applied stays; the rest waits for the next sync.
        </>
      );
    case "abort":
      return (
        <>
          Abort the rollout of <b>{row.name}</b>? Its traffic goes back to what ran before{p.settings.find((s) => s.key === "auto_rollback")?.on ? ", and kiln goes back to the deploy before it" : ""}.
        </>
      );
    case "delete":
      return (
        <>
          Delete <b>{row.name}</b>? kiln stops deploying it. <b>Delete all</b> deletes its {p.about.objects} objects from {p.about.cluster}/{p.about.namespace} too; <b>Delete, leave objects</b> leaves
          them running.
        </>
      );
  }
}

/** The picked one's detail: an object's Logs · Events · Manifest (or Diff), a commit against what runs, a rollout's steps, a setting. */
function Detail({ pane, row, shown, rows }: { pane: ShipDeployPane; row: ShipDeployRow; shown: Shown; rows: number }) {
  switch (shown.kind) {
    case "object":
      return <ObjectDetail key={shown.obj.item.key} app={row.app} item={shown.obj.item} trunk={pane.history.find((l) => l.deploy == null)?.short ?? null} rows={rows} />;
    case "line":
      return <CommitDetail pane={pane} row={row} line={shown.line} i={shown.i} />;
    case "step":
      return <RolloutDetail rollout={shown.rollout} />;
    case "setting":
      return <SettingDetail setting={shown.setting} row={row} />;
    case "delete":
      return <DeleteDetail pane={pane} row={row} />;
  }
}

type View = "Logs" | "Diff" | "Events" | "Manifest";

/** An object's detail, in the commits' Output's frame: its glyph and name, the views, and the view's body at one height. */
function ObjectDetail({ app, item, trunk, rows }: { app: string; item: ShipDeployItem; trunk: string | null; rows: number }) {
  const views: View[] = item.differs ? ["Diff", "Events", "Manifest"] : item.logs ? ["Logs", "Events", "Manifest"] : ["Events", "Manifest"];
  const [view, setView] = React.useState<View>(views[0] ?? "Events");
  const [loading, start] = React.useTransition();
  const [asked, setAsked] = React.useState<View | null>(null);
  const k = keyOf(item);
  return (
    <div className="ship-output dp-o" data-key={item.key} data-view={view}>
      <div className="ship-output__bar">
        <StatusGlyph state={glyphOf(item.state)} size={12} />
        <span className="ship-output__name">
          {item.kind} · {item.name}
          {item.container ? ` · ${item.container}` : ""}
        </span>
        <span className="dp-w" role="group" aria-label="Views">
          {views.map((v) => (
            // The view there is stays until the next has loaded: nothing flashes, nothing moves, and
            // the one pressed says meanwhile that it's on its way (W21-R15).
            <button
              key={v}
              type="button"
              aria-pressed={v === view}
              {...sendingProps(loading && asked === v)}
              onClick={() => {
                setAsked(v);
                start(() => setView(v));
              }}
            >
              {v}
            </button>
          ))}
        </span>
      </div>
      {view === "Logs" ? (
        <LogViewer cd={{ app, key: k }} live rows={rows} bare />
      ) : (
        <Loadable label={`Loading its ${view.toLowerCase()}`} fallback={<pre className="dp-op" aria-busy="true" />}>
          {view === "Events" ? <EventsBody app={app} k={k} /> : view === "Diff" ? <DiffBody app={app} item={item} trunk={trunk} /> : <ManifestBody app={app} k={k} />}
        </Loadable>
      )}
    </div>
  );
}

const resKey = (k: CdResourceKey) => `${k.group}~${k.kind}~${k.namespace}~${k.name}`;
const hms = (iso: string) => {
  const t = new Date(iso);
  return Number.isNaN(t.getTime()) ? "        " : t.toISOString().slice(11, 19);
};

function EventsBody({ app, k }: { app: string; k: CdResourceKey }) {
  const ev = useSuspenseResource(`ship/deploy/${app}/events/${resKey(k)}`, (s) => cdApi.events(app, k, s), { pollMs: 15_000 }).data.events;
  return (
    <pre className="dp-op">
      {ev.length
        ? ev.map((e, i) => (
            <span key={i} className={e.type === "Warning" ? "wn" : undefined}>
              <i>{hms(e.last)}</i>
              {`  ${e.type.padEnd(8)} ${e.reason.padEnd(10)} ${e.message}${e.count > 1 ? ` (x${e.count})` : ""}\n`}
            </span>
          ))
        : "No events."}
    </pre>
  );
}

function ManifestBody({ app, k }: { app: string; k: CdResourceKey }) {
  const r = useSuspenseResource(`ship/deploy/${app}/manifest/${resKey(k)}`, (s) => cdApi.resource(app, k, s)).data;
  return <pre className="dp-op">{r.live ?? r.desired ?? "Not in the cluster."}</pre>;
}

/** An app's diff, as CD says it: what differs in each of its objects. */
const useDiffs = (app: string) => useSuspenseResource(`ship/deploy/${app}/diff`, (s) => cdApi.diff(app, s), { on: CD_EVENTS }).data.diffs;
type Diff = ReturnType<typeof useDiffs>[number];
const diffOf = (diffs: readonly Diff[], i: ShipDeployItem) => diffs.find((x) => x.key.kind === i.kind && x.key.name === i.name && x.key.namespace === i.namespace && x.key.group === i.group);
/** How many keys differ: each a line taken out, put in, or both. */
const keysOf = (d: Diff | undefined) => {
  const lines = d?.hunks.flatMap((h) => h.lines) ?? [];
  return Math.max(lines.filter((l) => l.startsWith("+")).length, lines.filter((l) => l.startsWith("-")).length);
};

/** What a picked object that differs differs in, in the strip: ` in 2 keys`. */
function Keys({ app, item }: { app: string; item: ShipDeployItem }) {
  const n = keysOf(diffOf(useDiffs(app), item));
  return n ? <>{` in ${n} key${n === 1 ? "" : "s"}`}</> : null;
}

function DiffBody({ app, item, trunk }: { app: string; item: ShipDeployItem; trunk: string | null }) {
  const d = diffOf(useDiffs(app), item);
  const lines = d?.hunks.flatMap((h) => h.lines) ?? [];
  const changed = keysOf(d);
  return (
    <pre className="dp-op">
      {lines.map((l, i) => (
        <span key={i} className={l.startsWith("-") ? "rm" : l.startsWith("+") ? "ad" : undefined}>
          {`${l.startsWith("-") || l.startsWith("+") ? l.slice(0, 1) : " "}  ${l.slice(1)}\n`}
        </span>
      ))}
      {"\n"}
      <i>
        live (left) against git{trunk ? ` at ${trunk}` : ""} (right) · {changed ? `${changed} key${changed === 1 ? " differs" : "s differ"}` : (d?.why ?? "nothing differs")}
      </i>
    </pre>
  );
}

/** A commit or a deploy of the app's history: what it is, where it sits among the deploys, and what syncing it would do against what runs. */
function CommitDetail({ pane, row, line, i }: { pane: ShipDeployPane; row: ShipDeployRow; line: ShipDeployLine; i: number }) {
  const newer = pane.history
    .slice(0, i)
    .reverse()
    .find((l) => l.deploy != null);
  const older = pane.history.slice(i + 1).find((l) => l.deploy != null);
  const runs = pane.history.find((l) => l.now);
  const between = line.deploy == null && (newer || older) ? (newer && older ? `between #${older.deploy} and #${newer.deploy}` : newer ? `before #${newer.deploy}` : `after #${older?.deploy}`) : null;
  const plannable = !line.now && pane.about.repo && line.sha && /^[0-9a-f]{7,64}$/.test(line.sha);
  return (
    <div className="jp-cm" data-key={`h:${i}`}>
      <h4>
        {lineHead(line)} · {line.now ? "runs now" : line.deploy != null ? (line.state === "failed" ? "failed" : "deployed") : "never deployed"}
      </h4>
      <div>{line.subject}</div>
      <div className="dim">
        {[line.by ? whoSaid(line.by) : null, relativeTime(line.at_ms / 1000), between, line.same_as != null ? `renders as #${line.same_as}` : null].filter(Boolean).join(" · ")}
      </div>
      {plannable && pane.about.repo && line.sha ? (
        <>
          <h4>Against what runs{runs?.deploy != null ? ` (#${runs.deploy})` : ""}</h4>
          <Loadable label="Planning it" fallback={<div className="dim">Reading what it would change…</div>}>
            <Against repo={pane.about.repo} sha={line.sha} app={row.app} />
          </Loadable>
        </>
      ) : null}
    </div>
  );
}

/** What syncing a commit would change in the app against what runs (`cd.plans.get`, counts and keys). */
function Against({ repo, sha, app }: { repo: string; sha: string; app: string }) {
  const plan = useSuspenseResource(`ship/plan/${repo}/${sha}`, (s) => shipApi.plan(repo, sha, s)).data;
  const mine = plan.diffs.flatMap((d) => d.apps).find((a) => a.app === app);
  const sync = mine?.sync;
  if (!mine) return <div className="dim">It changes nothing {app.split("/")[1]} renders.</div>;
  if (!sync) return <div className="dim">{mine.error ?? mine.problems[0] ?? "It can't be planned against what runs."}</div>;
  return (
    <ul>
      {sync.steps.map((s) => (
        <li key={resKey(s.key)}>
          {s.key.kind} {s.key.name}: {s.step === "create" ? "new" : s.step === "prune" ? "deleted" : (s.why ?? "changes")}
        </li>
      ))}
      <li>
        {sync.unchanged} object{sync.unchanged === 1 ? "" : "s"} the same
      </li>
    </ul>
  );
}

function RolloutDetail({ rollout }: { rollout: ShipDeployRollout }) {
  const weight = rollout.weight ?? null;
  const sides = rollout.sides ?? [];
  return (
    <div className="jp-cm">
      <h4>
        {rolloutName(rollout)}
        {rollout.strategy ? ` · ${rollout.strategy}` : ""}
      </h4>
      {rollout.steps.map((s) => (
        <div key={s.n} className="row">
          <StatusGlyph state={glyphOf(s.state)} size={12} />
          <span>
            Step {s.n} · {s.detail ?? s.what}
          </span>
          {/* A step done says when: its line says it's done. */}
          <span className="dim">{s.v.replace(/^done /, "")}</span>
        </div>
      ))}
      {rollout.strategy ? null : <div className="dim">{rollout.note}</div>}
      {rollout.approvals?.length ? <div>Waits for: {rollout.approvals.join(", ")}</div> : null}
      {sides.length ? (
        <>
          <h4>Pods</h4>
          {sides.map((s) => (
            <div key={s.side}>
              {s.side} {s.name} · {s.pods} pod{s.pods === 1 ? "" : "s"} · {s.share}%{s.up ? ` · up ${s.up}` : ""}
            </div>
          ))}
        </>
      ) : weight != null && rollout.under_way ? (
        <>
          <h4>Traffic</h4>
          {rollout.back_to != null ? (
            <div>
              #{rollout.back_to} · what ran before · {100 - weight}%
            </div>
          ) : null}
          <div>
            {rollout.deploy != null ? `#${rollout.deploy}` : (rollout.revision ?? "it")} · rolling out · {weight}%
          </div>
        </>
      ) : null}
    </div>
  );
}

/** Deleting the app: what goes, and what stays unless it's left. */
function DeleteDetail({ pane, row }: { pane: ShipDeployPane; row: ShipDeployRow }) {
  return (
    <div className="jp-cm">
      <h4>App · delete</h4>
      <div>
        Deleting {row.name} stops kiln deploying it. Its {pane.about.objects} objects in {pane.about.cluster}/{pane.about.namespace} go too, unless you leave them running.
      </div>
      <div className="dim">{pane.why_not.delete ?? `Its deploy history goes with it; ${row.project}'s commits stay.`}</div>
    </div>
  );
}

/** What a setting does, on and off, and whose word it is. */
function SettingDetail({ setting, row }: { setting: ShipDeploySetting; row: ShipDeployRow }) {
  const n = row.name;
  const text: Record<string, [string, string]> = {
    auto_sync: [
      `${n} syncs each time trunk changes what it renders. When off, it syncs when someone says so.`,
      `${n} syncs when someone says so. When on, it syncs each time trunk changes what it renders.`,
    ],
    auto_rollback: [`When a deploy of ${n} fails, kiln goes back to the deploy before it.`, `When a deploy of ${n} fails, it stays as it is until someone acts.`],
    prune: [`A sync of ${n} deletes what git dropped.`, `A sync of ${n} keeps what git dropped; pruning deletes it.`],
    self_heal: [`kiln undoes changes made to ${n} by hand.`, `Changes made to ${n} by hand stay until the next sync.`],
  };
  const [on, off] = text[setting.key] ?? ["On.", "Off."];
  const word = setting.on ? "on" : "off";
  const from =
    setting.from === "deploy-file"
      ? `Set ${word} in ${row.project}'s .kiln/deploy.yaml.${setting.can ? ` Turning it ${setting.on ? "off" : "on"} here holds until that file changes it.` : ""}`
      : setting.from === "app"
        ? `Set ${word} here. ${row.project}'s .kiln/deploy.yaml has its word again once that file changes it.`
        : setting.from === "repo"
          ? `Set ${word} for every app of ${row.project}.`
          : "kiln's default.";
  return (
    <div className="jp-cm">
      <h4>
        {SETTING_WORD[setting.key] ?? setting.key} · {word}
      </h4>
      <div>{setting.on ? on : off}</div>
      <div className="dim">{from}</div>
    </div>
  );
}

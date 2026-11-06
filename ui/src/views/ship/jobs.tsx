/**
 * An opened commit's jobs as one pane (docs/ci.md W26-R23; Ada's approved stills, 2026-10-30): a strip
 * over its jobs by stage, the picked job's output beside them, or under its line on a phone. Nothing
 * on a line is a button but the line and its glyph; what's done is done from the strip.
 */
import * as React from "react";
import { shipApi, type ShipCommit, type ShipDetail, type ShipJob, type ShipJobDone, type ShipScope, type ShipVerb } from "@/api/ship";
import { DETAIL_OPTS, shipCommit } from "./live";
import { whoSaid } from "@/api/review";
import { StatusGlyph } from "@/components/ui/status-glyph";
import { GLYPHS } from "@/components/ui/status-glyph-spec";
import { sendingProps, useHolds } from "@/lib/action";
import { glyphOf, pickCard, shipGroups, strip } from "./model";
import { reviewGroup, type ShipLane, type ShipLaneGroup } from "./review/lanes";
import { labelOf, useLabels } from "@/lib/data";
import { formatDuration } from "@/lib/format";
import { errorMessage, useMediaQuery } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";
import { SectionHead } from "./head";

/** The cards: the journey's six stages after Review (Review is Files' to show). */
export const cardsOf = (commit: ShipCommit) => strip(commit.pips).filter((p) => p.stage !== "review");

/** The stage picked first, by the cards' rule. */
export const firstStage = (commit: ShipCommit) => {
  const cards = cardsOf(commit);
  return cards[pickCard(cards)]?.stage ?? null;
};

/** States of a job under way, the one shown before any is picked. */
const UNDER_WAY_PART = new Set(["running", "replanting", "guard", "soaking"]);

/** A job's time beside it: who for a review's mark, how long for one that ran, else why. */
function valueOf(l: ShipLane): string {
  if (l.by) return whoSaid(l.by);
  if (l.start != null && l.end != null && l.end > l.start) return formatDuration(l.end - l.start);
  return l.note ?? "";
}

/** The job shown until you pick one, as f-ship picks it: the one under way, else the one that failed, else the last begun. */
export function pickPart(groups: readonly ShipLaneGroup[]): string | null {
  const all = groups.flatMap((g) => g.lanes.map((l) => ({ key: `${g.group}|${l.id}`, l })));
  const begun = all.filter((x) => x.l.start != null);
  return (
    begun.find((x) => UNDER_WAY_PART.has(x.l.state))?.key ??
    begun.find((x) => x.l.state === "failed" || x.l.state === "rolledback")?.key ??
    begun.slice(-1)[0]?.key ??
    all.find((x) => x.l.job)?.key ??
    null
  );
}

/** A picked job's stage and lane, by its key. */
export function partOf(groups: readonly ShipLaneGroup[], key: string | null): { group: ShipLaneGroup; lane: ShipLane } | null {
  for (const group of groups) for (const lane of group.lanes) if (`${group.group}|${lane.id}` === key) return { group, lane };
  return null;
}

/** An opened commit's stations: Review first (its marks), then each of Ship's stages. */
export const stationsOf = (detail: ShipDetail): ShipLaneGroup[] => {
  const review = reviewGroup(detail);
  if (!review) return shipGroups(detail);
  // Its head says how many threads it has, as f-ship's does; with none, how it stands.
  const threads = new Set((detail.review ?? []).filter((m) => m.kind === "comment" && m.thread).map((m) => m.thread)).size;
  const note = threads ? `${threads} thread${threads === 1 ? "" : "s"}` : GLYPHS[glyphOf(review.state)].word;
  return [{ ...review, note }, ...shipGroups(detail)];
};

/** A lane's job, by its id: a Checks leg's `pipeline/name`, a platform's `platform/name`. */
function jobsByLane(jobs: readonly ShipJob[]): Map<string, ShipJob> {
  const m = new Map<string, ShipJob>();
  for (const j of jobs) {
    m.set(`${j.pipeline}/${j.name}`, j);
    if (!m.has(`platform/${j.name}`)) m.set(`platform/${j.name}`, j);
  }
  return m;
}

/** Which word picks a lane: its job's state, by the lane's glyph. */
const WORD_OF: Record<string, ShipScope> = { failed: "failed", error: "failed", running: "running", queued: "waiting", waiting: "waiting" };
const WORDS: ShipScope[] = ["failed", "running", "waiting", "all"];
/** A job's word on its line while it has no time to show. */
const JOB_WORD: Record<string, string> = { queued: "waiting", waiting: "waiting", running: "running" };
const VERB_WORD: Record<ShipVerb, string> = { retry: "Retry", pause: "Pause", resume: "Resume", cancel: "Cancel" };
const DONE_WORD: Record<ShipVerb, string> = { retry: "Run again", pause: "Paused", resume: "Resumed", cancel: "Cancelled" };

/** What's picked: the line shown (`at`), the lines picked, and the word that picked them, if one did. */
interface Pick {
  at: string | null;
  sel: string[];
  word: ShipScope | null;
}

/** One line: a stage's job, its glyph (which adds it to the pick or takes it out), name and time. */
function Line({
  k,
  lane,
  job,
  picked,
  at,
  onLine,
  onGlyph,
}: {
  k: string;
  lane: ShipLane;
  job: ShipJob | undefined;
  picked: boolean;
  at: boolean;
  onLine: (k: string) => void;
  onGlyph: (k: string) => void;
}) {
  const paused = job?.paused_at != null;
  return (
    <li className="jp-j" data-stage={k.split("|")[0]} data-sel={picked || undefined} data-at={at || undefined} data-job={job?.id} data-state={paused ? "paused" : lane.state}>
      <button type="button" className="jp-glyph" aria-pressed={picked} aria-label={`${picked ? "Take" : "Add"} ${lane.name} ${picked ? "out of" : "to"} the pick`} onClick={() => onGlyph(k)}>
        <StatusGlyph state={glyphOf(paused ? "waiting" : lane.state)} size={12} />
      </button>
      <button type="button" className="jp-line" aria-current={at || undefined} onClick={() => onLine(k)}>
        <span className="n">{lane.name}</span>
        <span className="v">{paused ? "paused" : valueOf(lane) || (job ? (JOB_WORD[lane.state] ?? "") : "")}</span>
      </button>
    </li>
  );
}

/**
 * An opened commit's Jobs as one pane (docs/ci.md W26-R23, as Ada approved it on 2026-10-30): the
 * strip says what's picked and what each action would do, Retry, Pause (Resume when what's picked is
 * paused) and Cancel one width each, dimmed where they don't apply and never hidden, and the words
 * pick failed, running, waiting or all. Under it every stage's jobs, flat: a line picks its job
 * alone, its glyph adds it to the pick or takes it out. The picked job's output is beside them, or
 * under its line on a phone, where the strip comes again. Cancel asks again in the strip. Each
 * action goes to `POST /api/ship/jobs`, and the strip says what came of it.
 */
export function JobsPane({ commit, output }: { commit: ShipCommit; output: (picked: string) => React.ReactNode }) {
  const detail = useSuspenseResource(`ship/commit/${commit.repo}/${commit.sha}`, (s) => shipCommit(commit.repo, commit.sha, s), DETAIL_OPTS);
  const stations = React.useMemo(() => stationsOf(detail.data), [detail.data]);
  const byLane = React.useMemo(() => jobsByLane(detail.data.jobs ?? []), [detail.data.jobs]);
  const phone = useMediaQuery("(max-width: 700px)");
  // A job's runner is shown by its label (docs/machines.md).
  useLabels();
  const [pick, setPick] = React.useState<Pick>({ at: null, sel: [], word: null });
  const [armed, setArmed] = React.useState(false);
  const [said, setSaid] = React.useState<{ text: string; refused: boolean } | null>(null);
  const lanes = React.useMemo(() => stations.flatMap((g) => g.lanes.map((l) => ({ key: `${g.group}|${l.id}`, g, l, job: byLane.get(l.id) }))), [stations, byLane]);
  const at = pick.at ?? pickPart(stations);
  const sel = pick.sel.length ? pick.sel : at ? [at] : [];
  const picked = lanes.filter((x) => sel.includes(x.key));
  const jobs = picked.flatMap((x) => (x.job ? [x.job] : []));
  const can = (v: ShipVerb) => jobs.some((j) => j.verbs.includes(v));
  // Pause becomes Resume when what's picked is paused (none of it can pause, some can resume).
  const hold: ShipVerb = !can("pause") && can("resume") ? "resume" : "pause";
  const choose = (next: Pick) => {
    setPick(next);
    setArmed(false);
    setSaid(null);
  };
  const onLine = (k: string) => choose({ at: k, sel: [k], word: null });
  const onGlyph = (k: string) => {
    const has = sel.includes(k);
    const rest = has ? sel.filter((x) => x !== k) : [...sel, k];
    // The line shown stays as it was; taken out, the next picked one is shown.
    choose({ at: has && k === at ? (rest[0] ?? null) : (at ?? k), sel: rest, word: null });
  };
  const onWord = (w: ShipScope) => {
    if (pick.word === w) return choose({ at, sel: at ? [at] : [], word: null });
    const keys = lanes.filter((x) => x.job && (w === "all" || WORD_OF[x.l.state] === w)).map((x) => x.key);
    choose({ at: keys.includes(at ?? "") ? at : (keys[0] ?? at), sel: keys, word: w });
  };
  // One change to the jobs picked at a time (W21-R15): the verb pressed says it's on its way (Cancel
  // for Cancel them, once the strip is back to its three) and the others are held until it's back;
  // other jobs picked meanwhile are free.
  const target = pick.word
    ? `scope:${pick.word}`
    : `jobs:${jobs
        .map((j) => j.id)
        .sort()
        .join(",")}`;
  const holds = useHolds();
  const these = holds(target);
  const act = (verb: ShipVerb) =>
    these.change(verb, async () => {
      const ids = jobs.filter((j) => j.verbs.includes(verb)).map((j) => j.id);
      const body = pick.word ? { repo: commit.repo, sha: commit.sha, verb, scope: pick.word } : { repo: commit.repo, sha: commit.sha, verb, jobs: ids };
      setArmed(false);
      try {
        const r = await shipApi.jobs(body);
        setSaid(sayDone(verb, r.jobs, detail.data.jobs ?? []));
      } catch (e) {
        setSaid({ text: `${VERB_WORD[verb]}: ${errorMessage(e)}`, refused: true });
      } finally {
        detail.reload();
      }
    });
  const { doing: sending, held } = these;
  const strip = (live: boolean) => (
    <Strip
      live={live}
      say={armed ? armedSay(picked) : said ? said.text : pickSay(picked, pick.word)}
      tone={armed || said?.refused ? "warn" : undefined}
      word={pick.word}
      onWord={onWord}
      verbs={
        armed
          ? [
              { label: "Keep", on: true, keep: true, run: () => setArmed(false) },
              { label: "Cancel them", on: !held("cancel"), sending: sending("cancel"), go: true, run: () => act("cancel") },
            ]
          : [
              { label: VERB_WORD.retry, on: can("retry") && !held("retry"), sending: sending("retry"), run: () => act("retry") },
              { label: VERB_WORD[hold], on: can(hold) && !held(hold), sending: sending(hold), run: () => act(hold) },
              { label: VERB_WORD.cancel, on: can("cancel") && !held("cancel"), sending: sending("cancel"), run: () => setArmed(true) },
            ]
      }
    />
  );
  const shown = partOf(stations, at);
  return (
    <>
      <SectionHead label="Jobs" />
      <div className="jp">
        {strip(true)}
        <div className="jp-body">
          <ol className="jp-list" aria-label="Jobs by stage">
            {stations.map((g) => (
              <React.Fragment key={g.group}>
                <li className="jp-g" data-stage={g.group}>
                  <span>{g.label}</span>
                  <span>{g.note}</span>
                </li>
                {g.lanes.map((l) => {
                  const k = `${g.group}|${l.id}`;
                  return (
                    <React.Fragment key={l.id}>
                      <Line k={k} lane={l} job={byLane.get(l.id)} picked={sel.includes(k)} at={k === at} onLine={onLine} onGlyph={onGlyph} />
                      {phone && k === at ? (
                        <li className="jp-detail">
                          {strip(false)}
                          {output(k)}
                        </li>
                      ) : null}
                    </React.Fragment>
                  );
                })}
              </React.Fragment>
            ))}
          </ol>
          {phone ? null : <div className="jp-side">{shown && at ? output(at) : null}</div>}
        </div>
      </div>
    </>
  );
}

interface Verb {
  label: string;
  on: boolean;
  run: () => void;
  /** Its verb's request is on its way: it says so, and a press does nothing. */
  sending?: boolean;
  keep?: boolean;
  go?: boolean;
}

/** The strip: what's picked and what an action would do, the words that pick, the actions. */
function Strip({ live, say, tone, word, onWord, verbs }: { live: boolean; say: React.ReactNode; tone: "warn" | undefined; word: ShipScope | null; onWord: (w: ShipScope) => void; verbs: Verb[] }) {
  return (
    <div className="jp-strip">
      <div className="jp-say" data-tone={tone}>
        <div className="jp-said" aria-live={live ? "polite" : undefined}>
          {say}
        </div>
        <div className="jp-q">
          <span>pick:</span>
          {WORDS.map((w, i) => (
            <React.Fragment key={w}>
              {i ? <span aria-hidden>·</span> : null}
              <button type="button" aria-pressed={word === w} onClick={() => onWord(w)}>
                {w}
              </button>
            </React.Fragment>
          ))}
        </div>
      </div>
      <div className="jp-verbs" data-armed={verbs.some((v) => v.go) || undefined}>
        {verbs.map((v) => (
          <button
            key={v.label}
            type="button"
            disabled={!v.on}
            data-keep={v.keep || undefined}
            data-go={v.go || undefined}
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

type Picked = { key: string; l: ShipLane; job: ShipJob | undefined };

/** The strip's words for what's picked: one job and how it stands, or how many a word picked. */
function pickSay(picked: readonly Picked[], word: ShipScope | null): React.ReactNode {
  const jobs = picked.filter((x) => x.job);
  if (word) {
    const n = jobs.length;
    const what: Record<ShipScope, string> = {
      failed: "Retry runs them again, and what needs them",
      running: "Pause freezes them where they run; Cancel stops them",
      waiting: "Pause holds them before they start; Cancel stops them",
      all: "Retry runs again those that ended; Pause and Cancel the rest",
    };
    return n ? (
      <>
        <b>
          {n} {word === "all" ? (n === 1 ? "job" : "jobs") : word}
        </b>{" "}
        picked · {what[word]}
      </>
    ) : (
      <>Nothing {word === "all" ? "here" : word} to pick</>
    );
  }
  if (picked.length > 1)
    return (
      <>
        <b>{picked.length} jobs</b> picked · {jobs.length < picked.length ? "the actions are for those that are jobs" : "the actions are for each that they apply to"}
      </>
    );
  const one = picked[0];
  if (!one) return <>Pick a job, or a word: failed, running, waiting or all</>;
  return (
    <>
      <b>{one.l.name}</b> picked · {one.job ? standOf(one.l, one.job) : "nothing here to run again, pause or cancel"}
    </>
  );
}

/** How one job stands, and what an action would do to it. */
function standOf(l: ShipLane, j: ShipJob): string {
  if (j.paused_at != null) return "paused · Resume lets it go on from where it was";
  switch (l.state) {
    case "running":
      return j.runner ? `running on ${labelOf(j.runner)}` : "running";
    case "failed":
    case "error":
      return "failed · Retry runs it again, and what needs it";
    case "queued":
    case "waiting":
      return "waiting · Pause holds it before it starts";
    default:
      return `${GLYPHS[glyphOf(l.state)].word} · Retry runs it again`;
  }
}

/** What Cancel would do to what's picked, asked again before it does. */
function armedSay(picked: readonly Picked[]): React.ReactNode {
  const jobs = picked.filter((x) => x.job?.verbs.includes("cancel"));
  const running = jobs.filter((x) => x.l.state === "running").map((x) => x.l.name);
  const waiting = jobs.length - running.length;
  const runs = running.length === 1 ? <b>{running[0]}</b> : running.length ? <b>{running.length} running</b> : null;
  const waits = waiting ? `${runs ? " and " : ""}the ${waiting === 1 ? "one" : waiting} waiting before ${waiting === 1 ? "it starts" : "they start"}` : "";
  return (
    <>
      Cancel stops {runs}
      {waits}; what needs {jobs.length === 1 ? "it" : "them"} is skipped.
    </>
  );
}

/** What came of an action, job by job: done, or why not. */
function sayDone(verb: ShipVerb, done: readonly ShipJobDone[], jobs: readonly ShipJob[]): { text: string; refused: boolean } {
  // As its line names it: a Checks leg's pipeline and name.
  const name = (id: string) => {
    const j = jobs.find((x) => x.id === id);
    return j ? `${j.pipeline} · ${j.name}` : id;
  };
  const ok = done.filter((d) => d.done).map((d) => name(d.job));
  const no = done.filter((d) => !d.done);
  const parts = [ok.length ? `${DONE_WORD[verb]}: ${ok.join(", ")}.` : "", ...no.map((d) => `${name(d.job)}: ${d.why ?? "not done"}.`)].filter(Boolean);
  return { text: parts.join(" ") || `Nothing to ${VERB_WORD[verb].toLowerCase()}.`, refused: no.length > 0 };
}

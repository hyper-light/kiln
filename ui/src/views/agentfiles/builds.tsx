/**
 * An Agentfile's builds and install sources (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, P4.5, P4.6):
 * its image on each runner first, where it is now (built, building step by step, queued, refused in
 * Shards' words), built again in place; then what it installs from, a row a `HARNESS` or `AGENT`
 * source (its kind, its pin, the credential its fetch is handed by name, the origin that fetch alone
 * is granted), each opened at its line in the editor and changed there through the source picker;
 * and, beside its versions, each version's image.
 */
import * as React from "react";
import { Box, Check, ChevronDown, Database, Folder, GitBranch, Globe, Hammer, KeyRound, Package, Pencil, RotateCcw, Server } from "lucide-react";
import {
  agentfileBuildingRes,
  agentfileBuildsApi,
  agentfileBuildsRes,
  moving,
  type AgentfileBuild,
  type AgentfileBuilds,
  type AgentfileImage,
  type AgentfileSource,
  type AgentfileSourceKind,
} from "@/api/agentfiles";
import { Button } from "@/components/ui/button";
import { Meter } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { Segmented } from "@/components/ui/tabs";
import { toast } from "@/components/ui/toast";
import { useActions } from "@/lib/action";
import { agentsRes, labelOf, useRes } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { mySecretsRes } from "@/api/secrets";
import { Link } from "@/lib/router";
import { chosenIn, formOf, lineOf, pinWords, problemOf, slotsOf, type SecretChoice, type SecretSlot, type SourceForm } from "./sources";

const ago = (s: number) => (s < 60 ? `${Math.max(0, Math.round(s))}s` : s < 3600 ? `${Math.round(s / 60)}m` : s < 86400 ? `${Math.round(s / 3600)}h` : `${Math.round(s / 86400)}d`);
const gib = (n: number) => `${(n / 2 ** 30).toFixed(n >= 10 * 2 ** 30 ? 0 : 1)}G`;
const short = (digest: string) => digest.replace(/^sha256:/, "").slice(0, 8);

const STATUS: Record<AgentfileBuild["state"], string> = { built: "succeeded", building: "running", queued: "queued", failed: "failed", none: "skipped" };
const WORD: Record<AgentfileBuild["state"], string> = { built: "Built", building: "Building", queued: "Queued", failed: "Refused", none: "Not built here" };
/** Why a runner doesn't build, in a word or two (its whole reason in the title). */
const whyShort = (why: string | null) => (!why ? "not built" : /host/.test(why) ? "host engine" : /microVM/i.test(why) ? "no microVMs" : /old|upgrade/.test(why) ? "kiln too old" : "not built");

type Change = (fn: (prev: AgentfileBuilds) => AgentfileBuilds) => void;

/** Its image on each runner, read every 2 s while one is queued or building, and once every build has settled no more. */
export function Builds({ id }: { id: string }) {
  const calm = useRes(agentfileBuildsRes(id));
  // Runners by the names people see (their labels come with the runners' list).
  useRes(agentsRes);
  const [open, setOpen] = React.useState<ReadonlySet<string>>(new Set());
  const toggle = (runner: string) =>
    setOpen((s) => {
      const n = new Set(s);
      if (n.has(runner)) n.delete(runner);
      else n.add(runner);
      return n;
    });
  const body = (b: AgentfileBuilds) => <BuildsBody id={id} b={b} open={open} onOpen={toggle} change={calm.change} onBuilt={(next) => calm.setData(() => next)} />;
  if (!moving(calm.data)) return body(calm.data);
  return (
    <React.Suspense fallback={body(calm.data)}>
      <Moving id={id} calm={calm.data} body={body} />
    </React.Suspense>
  );
}

function Moving({ id, calm, body }: { id: string; calm: AgentfileBuilds; body: (b: AgentfileBuilds) => React.ReactNode }) {
  const live = useRes(agentfileBuildingRes(id)).data;
  // A reading kept from an earlier build stands aside for the newer one until it's read again.
  return body(moving(live) ? live : calm);
}

function BuildsBody({
  id,
  b,
  open,
  onOpen,
  change,
  onBuilt,
}: {
  id: string;
  b: AgentfileBuilds;
  open: ReadonlySet<string>;
  onOpen: (runner: string) => void;
  change: Change;
  onBuilt: (next: AgentfileBuilds) => void;
}) {
  // A press shows at once: its runners queued here, then as kiln says (W21-R15).
  const [build, building] = useActions(
    async (_key: string, runner?: string) => {
      change((prev) => ({
        ...prev,
        builds: prev.builds.map((x) => (x.platform && (!runner || x.runner === runner) ? { ...x, state: "queued" as const, step: null, why: null, version: prev.version } : x)),
      }));
      onBuilt(await agentfileBuildsApi.build(id, runner));
    },
    (e) => toast.error("Couldn't build", errorMessage(e)),
  );
  const here = b.builds.filter((x) => x.platform);
  const built = here.filter((x) => x.state === "built" && x.version === b.version).length;
  const refused = here.filter((x) => x.state === "failed").length;
  const now = Date.now() / 1000;
  return (
    <section className="af-section afb" aria-label="Builds">
      <div className="afb-head">
        <h2 className="af-h">Builds</h2>
        <span className="afb-sum" aria-label={`${built} of ${here.length} runners built v${b.version}`}>
          <span className="afb-tally" aria-hidden="true">
            {here.map((x) => (
              <i key={x.runner} data-state={x.state} title={`${labelOf(x.runner)}: ${WORD[x.state]}`} />
            ))}
          </span>
          {built}/{here.length} built
          {refused > 0 && <span className="afb-bad"> · {refused} refused</span>}
        </span>
        <Button size="xs" variant="secondary" className="afb-build" pending={building("all")} disabled={here.length === 0} onClick={() => build("all")}>
          <Hammer aria-hidden="true" /> Build v{b.version}
        </Button>
      </div>
      <ul className="afb-rows" aria-label="Its image on each runner">
        {b.builds.map((x) => (
          <BuildRow key={x.runner} x={x} now={now} open={open.has(x.runner)} onOpen={() => onOpen(x.runner)} pending={building(x.runner)} onBuild={() => build(x.runner, x.runner)} />
        ))}
      </ul>
    </section>
  );
}

function BuildRow({ x, now, open, onOpen, pending, onBuild }: { x: AgentfileBuild; now: number; open: boolean; onOpen: () => void; pending: boolean; onBuild: () => void }) {
  const name = labelOf(x.runner);
  const moving = x.state === "queued" || x.state === "building";
  return (
    <li className="afb-row" data-state={x.state}>
      <StatusDot status={STATUS[x.state]} title={WORD[x.state]} />
      <span className="afb-name" title={name}>
        {name}
      </span>
      <span className="afb-dim afb-plat" title={x.platform ?? undefined}>
        {x.platform ? x.platform.split("/")[1] : "—"}
      </span>
      <span className="afb-state">
        {x.state === "building" && x.step ? (
          <>
            <Meter value={x.step[0]} max={x.step[1]} label={`Step ${x.step[0]} of ${x.step[1]}`} />
            <span className="afb-num">
              {x.step[0]}/{x.step[1]}
            </span>
          </>
        ) : x.state === "none" ? (
          <span className="afb-dim" title={x.why ?? undefined}>
            {whyShort(x.why)}
          </span>
        ) : (
          <span className="status-text" data-status={STATUS[x.state]}>
            {x.state === "failed" ? "refused" : x.state}
          </span>
        )}
      </span>
      <span className="afb-ver" title={x.serving ? `Sessions here start from v${x.serving} until v${x.version} is built` : undefined}>
        {x.platform ? (x.state === "built" ? `v${x.version}` : x.serving ? `serving v${x.serving}` : `v${x.version}`) : ""}
      </span>
      <span className="afb-dim afb-digest" title={x.digest ?? undefined}>
        {x.digest ? short(x.digest) : ""}
      </span>
      <span className="afb-dim afb-size">{x.size ? gib(x.size) : ""}</span>
      <span className="afb-dim afb-when" title={x.took_s ? `took ${ago(x.took_s)}` : undefined}>
        {x.at ? ago(now - x.at) : ""}
      </span>
      <span className="afb-acts">
        {x.state === "failed" && (
          <button type="button" className="icon-btn" data-variant="bare" data-size="sm" aria-expanded={open} title="Shards' words" aria-label={`Why ${name} refused it`} onClick={onOpen}>
            <ChevronDown size={14} className="afb-chev" />
          </button>
        )}
        {x.platform && (
          <button
            type="button"
            className="icon-btn"
            data-variant="bare"
            data-size="sm"
            disabled={pending || moving}
            title={x.state === "failed" ? "Build again" : "Build here"}
            aria-label={`Build on ${name}`}
            onClick={onBuild}
          >
            <RotateCcw size={14} />
          </button>
        )}
      </span>
      {open && x.why && x.state === "failed" && (
        <div className="afb-why">
          <p>{x.why}</p>
          <Button size="xs" variant="secondary" pending={pending} onClick={onBuild}>
            <RotateCcw aria-hidden="true" /> Build again
          </Button>
        </div>
      )}
    </li>
  );
}

/* ------------------------------------------------ install sources */

const KIND: Record<AgentfileSourceKind, { icon: typeof GitBranch; word: string; title: string }> = {
  "git-https": { icon: GitBranch, word: "git", title: "Git over https" },
  "git-ssh": { icon: GitBranch, word: "ssh", title: "Git over ssh" },
  s3: { icon: Database, word: "s3", title: "An S3 object" },
  https: { icon: Package, word: "https", title: "An https tarball" },
  path: { icon: Folder, word: "path", title: "A path kiln fills" },
  oci: { icon: Box, word: "oci", title: "An OCI image" },
};

/**
 * Each install source as a row, opened at its line in the editor (`onJump`), and changed there
 * through the source picker (`onWrite`: the line written again, in the editor, for Save).
 */
export function Sources({
  id,
  text,
  onJump,
  onWrite,
}: {
  id: string;
  text: () => string;
  onJump: (s: AgentfileSource) => void;
  onWrite: (s: AgentfileSource, f: SourceForm, chosen: SecretChoice) => boolean;
}) {
  const b = useRes(agentfileBuildsRes(id)).data;
  const [editing, setEditing] = React.useState<string | null>(null);
  const loose = b.sources.filter((s) => !s.pin).length;
  return (
    <section className="af-section afs" aria-label="Sources">
      <div className="afb-head">
        <h2 className="af-h">Sources</h2>
        <span className="afb-sum">
          {b.sources.length} · {loose ? <span className="afs-loose">{loose} unpinned</span> : "all pinned"}
        </span>
      </div>
      {b.sources.length === 0 ? (
        <p className="af-dim">None: no HARNESS or AGENT line</p>
      ) : (
        <ul className="afs-rows" aria-label="What it installs from">
          {b.sources.map((s) => {
            const key = `${s.directive} ${s.name}`;
            return (
              <SourceRow
                key={key}
                s={s}
                editing={editing === key}
                onJump={() => onJump(s)}
                onEdit={() => setEditing(editing === key ? null : key)}
                text={text}
                onWrite={(f, chosen) => {
                  if (onWrite(s, f, chosen)) setEditing(null);
                }}
              />
            );
          })}
        </ul>
      )}
    </section>
  );
}

function SourceRow({
  s,
  editing,
  onJump,
  onEdit,
  text,
  onWrite,
}: {
  s: AgentfileSource;
  editing: boolean;
  onJump: () => void;
  onEdit: () => void;
  text: () => string;
  onWrite: (f: SourceForm, chosen: SecretChoice) => void;
}) {
  const K = KIND[s.kind];
  const pin = pinWords(s.pin);
  return (
    <li className="afs-row" data-editing={editing || undefined}>
      <button type="button" className="afs-main" title={`Line ${s.line}: ${s.source}`} aria-label={`${s.directive} ${s.name}: line ${s.line}`} onClick={onJump}>
        <span className="afs-kind" data-kind={s.kind} title={K.title}>
          <K.icon size={13} aria-hidden="true" />
          {K.word}
        </span>
        <span className="afs-name">
          <i>{s.directive.toLowerCase()}</i> {s.name}
        </span>
        <span className="afs-src">{s.source}</span>
        <span className="afs-pin" data-none={!s.pin || undefined} title={s.pin ? `${s.pin.by}: ${s.pin.value}${s.pin.resolved ? ` → ${s.pin.resolved}` : ""}` : "Not pinned"}>
          {s.pin?.by === "ref" ? <GitBranch size={12} aria-hidden="true" /> : <Check size={12} aria-hidden="true" />}
          <span>{pin}</span>
        </span>
        <span className="afs-cred" title={secretsTitle(s)}>
          {s.secrets.length > 0 ? (
            <>
              <KeyRound size={12} aria-hidden="true" />
              <span>{s.secrets.map((x) => x.name).join(" + ")}</span>
            </>
          ) : (
            <span className="afb-dim">{s.kind === "git-ssh" ? "ssh agent" : "no key"}</span>
          )}
        </span>
        <span className="afs-grant" title={s.grant ? `Its fetch alone may reach ${s.grant}` : undefined}>
          {s.grant ? (
            <>
              <Globe size={12} aria-hidden="true" />
              <span>{s.grant}</span>
            </>
          ) : (
            <span className="afb-dim">local</span>
          )}
        </span>
      </button>
      {s.kind !== "oci" && (
        <button type="button" className="icon-btn" data-variant="bare" data-size="sm" aria-expanded={editing} title="Change its source" aria-label={`Change ${s.name}'s source`} onClick={onEdit}>
          <Pencil size={13} />
        </button>
      )}
      {editing && <SourcePicker s={s} text={text()} onWrite={onWrite} onCancel={onEdit} />}
    </li>
  );
}

/** Git over ssh signs in with an SSH agent, and kiln gives a build secrets alone (`--secret`, sandbox/image.rs). */
const SSH = "Git over ssh signs in with an SSH agent, and kiln gives builds secrets alone: its https URL takes a token";
/** Each secret its fetch is given, as its label names it. */
const secretsTitle = (s: AgentfileSource) => (s.secrets.length ? s.secrets.map((x) => `${x.id} = user:${x.name}`).join(", ") : s.kind === "git-ssh" ? SSH : "Its fetch is given no secret");

const KINDS = [
  { value: "git", label: "Git" },
  { value: "s3", label: "S3" },
  { value: "https", label: "HTTPS" },
  { value: "path", label: "Path" },
] as const;
const PIN: Record<SourceForm["kind"], { label: string; hint: string }> = {
  git: { label: "Ref", hint: "v4:dir" },
  s3: { label: "Version", hint: "version id or sha256" },
  https: { label: "SHA-256", hint: "64 hex digits" },
  path: { label: "", hint: "" },
};

/** The source picker, in place under its row: kind, source and pin, and the secrets its fetch is given (yours, by name), written into the editor in Shards' syntax. */
function SourcePicker({ s, text, onWrite, onCancel }: { s: AgentfileSource; text: string; onWrite: (f: SourceForm, chosen: SecretChoice) => void; onCancel: () => void }) {
  const [f, setF] = React.useState<SourceForm>(() => formOf(s, text.split("\n")[lineOf(text, s) - 1] ?? ""));
  const [chosen, setChosen] = React.useState<SecretChoice>(() => chosenIn(text, slotsOf(f)));
  const slots = slotsOf(f);
  // Shards refuses half an S3 key (build/s3.rs): both halves, or neither.
  const half = f.kind === "s3" && slots.length === 2 && Boolean(chosen.key_id) !== Boolean(chosen.secret_key);
  const problem = problemOf(f) ?? (half ? "both halves of the key" : null);
  const set = (p: Partial<SourceForm>) => setF((x) => ({ ...x, ...p }));
  return (
    <form
      className="afs-pick"
      aria-label={`${s.name}'s source`}
      onSubmit={(e) => {
        e.preventDefault();
        if (!problem) onWrite(f, chosen);
      }}
    >
      <Segmented size="xs" label="Kind" value={f.kind} onChange={(kind) => set({ kind })} options={KINDS.map((k) => ({ value: k.value, label: k.label }))} />
      <label className="field afs-field afs-wide">
        <span className="sr-only">Source</span>
        <input
          value={f.source}
          spellCheck={false}
          placeholder={f.kind === "git" ? "https://host/org/repo.git" : f.kind === "s3" ? "s3://bucket/key" : f.kind === "https" ? "https://…/x.tar.gz" : "./dir"}
          onChange={(e) => set({ source: e.target.value })}
        />
      </label>
      {f.kind !== "path" && (
        <label className="field afs-field">
          <span className="afs-k">{PIN[f.kind].label}</span>
          <input value={f.pin} spellCheck={false} placeholder={PIN[f.kind].hint} onChange={(e) => set({ pin: e.target.value })} />
        </label>
      )}
      {f.kind === "s3" && (
        <label className="field afs-field afs-narrow">
          <span className="afs-k">Region</span>
          <input value={f.region} spellCheck={false} placeholder="us-east-1" onChange={(e) => set({ region: e.target.value })} />
        </label>
      )}
      {slots.length > 0 ? (
        <React.Suspense fallback={slots.map((x) => <SecretPickHold key={x.role} slot={x} />)}>
          <SecretPicks slots={slots} chosen={chosen} onChoose={(role, name) => setChosen((c) => ({ ...c, [role]: name }))} />
        </React.Suspense>
      ) : (
        f.kind === "git" &&
        /^(git@|ssh:\/\/)/.test(f.source.trim()) && (
          <span className="afs-cred afs-key afb-dim" title={SSH}>
            ssh agent
          </span>
        )
      )}
      <span className="afs-acts">
        {problem && <span className="afs-problem">{problem}</span>}
        <Button size="xs" type="submit" disabled={Boolean(problem)}>
          Write line
        </Button>
        <Button size="xs" variant="ghost" onClick={onCancel}>
          Cancel
        </Button>
      </span>
    </form>
  );
}

/**
 * Its fetch's secrets, each one of yours by name (`/api/me/secrets`): a name written that isn't yours
 * says so, as kiln refuses its session's build until you set one.
 */
function SecretPicks({ slots, chosen, onChoose }: { slots: SecretSlot[]; chosen: SecretChoice; onChoose: (role: SecretSlot["role"], name: string | null) => void }) {
  const mine = useRes(mySecretsRes).data;
  return (
    <>
      {slots.map((x) => {
        const name = chosen[x.role] ?? "";
        const yours = !name || mine.some((m) => m.name === name);
        return (
          <label key={x.role} className="field afs-field afs-secret" data-missing={!yours || undefined} title={`${x.id}: one of your secrets, by name, given to this fetch alone`}>
            <KeyRound size={12} aria-hidden="true" />
            <span className="afs-k">{x.label}</span>
            <select value={name} onChange={(e) => onChoose(x.role, e.target.value || null)}>
              <option value="">none</option>
              {mine.map((m) => (
                <option key={m.name} value={m.name}>
                  {m.name}
                </option>
              ))}
              {!yours && <option value={name}>{name} · not yours</option>}
            </select>
          </label>
        );
      })}
      {mine.length === 0 && (
        <Link to="/settings#secrets.mine" className="afs-set">
          Set one of yours
        </Link>
      )}
    </>
  );
}

function SecretPickHold({ slot }: { slot: SecretSlot }) {
  return (
    <span className="field afs-field afs-secret" aria-hidden="true">
      <KeyRound size={12} />
      <span className="afs-k">{slot.label}</span>
    </span>
  );
}

/* ------------------------------------------------ each version's image */

/** Its versions' images by version, for the versions' rows. */
export function useImages(id: string): ReadonlyMap<number, AgentfileImage> {
  const images = useRes(agentfileBuildsRes(id)).data.images;
  return React.useMemo(() => new Map(images.map((i) => [i.version, i])), [images]);
}

/** A version's image, in a row's few cells: built or not, its digest per platform, its size, and the runners whose sessions start from it. */
export function ImageCells({ i }: { i: AgentfileImage | undefined }) {
  if (!i) return <span className="afv-image" aria-hidden="true" />;
  const digests = Object.entries(i.digests);
  return (
    <span className="afv-image">
      <StatusDot status={i.built ? "succeeded" : "queued"} title={i.built ? "Image built" : "Not built yet"} />
      {digests.length > 0 ? (
        digests.map(([platform, d]) => (
          <span key={platform} className="afv-digest" title={`${platform} ${d}`}>
            <i>{platform.split("/")[1]}</i>
            {short(d)}
          </span>
        ))
      ) : (
        <span className="afb-dim">not built</span>
      )}
      {i.size !== null && <span className="afb-dim afv-size">{gib(i.size)}</span>}
      <span
        className="afv-runners"
        data-none={i.runners.length === 0 || undefined}
        title={i.runners.length ? `Sessions start from it on ${i.runners.map((r) => labelOf(r)).join(", ")}` : "No runner starts sessions from it"}
      >
        <Server size={12} aria-hidden="true" />
        {i.runners.length}
      </span>
    </span>
  );
}

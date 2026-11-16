/**
 * An agent's Sandbox tab (its Host tab on the host; kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, P2,
 * P3): where the session runs and what it may reach, top to bottom as the person asked:
 *
 *   - now: CPU, memory and disk as live bars, then its controls;
 *   - what it is: in a microVM its Hex box, Shards image, footprint, start and secret names; on the
 *     host its runner and the harness process it runs as;
 *   - its components (harness, agents, MCP servers) swapped at their paths, the rest running on;
 *   - in a microVM only, its network: ports published in, egress granted out, and who is on which
 *     network, everything else denied;
 *   - its volumes, allowed directories and scope, on both engines;
 *   - the last hour as plots, full width.
 *
 * What only a microVM has is absent on the host, not greyed: one line says it runs on the host.
 * Under its line in a Mission row's dropdown (engine-rows.tsx, `inRow`), that line says where it is
 * and holds its controls: the tab's whole is the rest.
 */
import * as React from "react";
import { ArrowRight, Check, Cloud, Download, FolderOpen, GitBranch, KeyRound, Pause, Play, Plus, RotateCcw, Square, Undo2, X } from "lucide-react";
import {
  memberWord,
  overridesApi,
  sandboxApi,
  sandboxRes,
  grantsRes,
  type SessionGrants,
  type ComponentSource,
  type MemberKind,
  type OverrideAnswer,
  type SandboxComponent,
  type SandboxDetail,
  type SandboxMember,
} from "@/api/microvms";
import { useSession, type SessionStore } from "@/api/harness";
import type { LiveMember, LiveOverrides, LiveTook } from "@/api/wire";
import { agentdefsRes } from "@/api/agentdefs";
import { Button } from "@/components/ui/button";
import { Sparkline } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { Segmented } from "@/components/ui/tabs";
import { toast } from "@/components/ui/toast";
import { labelOf, useRes } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { EngineGlyph, StartMark, WarmDots } from "@/views/sandbox-marks";
import { Fold } from "./fold";
import { KindGlyph } from "./kind-glyph";
import { LimitMeter } from "./limits";
import { HarnessMark } from "./parts";
import "./sandbox.css";

const G = 2 ** 30;
const gib = (n: number) => Math.round((n / G) * 100) / 100;
const STATE: Record<SandboxDetail["state"], { status: string; word: string }> = {
  running: { status: "running", word: "Running" },
  idle: { status: "succeeded", word: "Idle" },
  paused: { status: "queued", word: "Paused" },
  failed: { status: "failed", word: "Failed" },
  draining: { status: "timed_out", word: "Draining" },
  ended: { status: "skipped", word: "Ended" },
};
const up = (s: number) => (s < 60 ? `${s}s` : s < 3600 ? `${Math.round(s / 60)}m` : `${Math.floor(s / 3600)}h ${Math.round((s % 3600) / 60)}m`);
const SOURCE: Record<ComponentSource["kind"], { icon: typeof GitBranch; word: string }> = {
  "git-https": { icon: GitBranch, word: "git" },
  "git-ssh": { icon: GitBranch, word: "git ssh" },
  s3: { icon: Cloud, word: "s3" },
  https: { icon: Download, word: "https" },
};
/** A pin, short: a SHA-256's first 12, an object version's first 12, a ref and its commit as they are. */
const pinShort = (pin: string) => (pin.startsWith("sha256:") ? pin.slice(7, 19) : pin.startsWith("version ") ? pin.slice(8, 20) : pin);

/** The session's definition version, and the current one: behind, it can restart on the current. */
function useVersions(agentId: string): { started: number; current: number } | null {
  const defs = useRes(agentdefsRes).data;
  for (const d of defs) {
    const s = d.sessions.find((x) => x.id === agentId);
    if (s) return { started: s.version, current: d.version };
  }
  return null;
}

/** Press the head's own harness switch (its model chip, which lists the other harnesses' models): the switch stays where it is. */
function openHarnessSwitch() {
  const chip = [...document.querySelectorAll<HTMLButtonElement>('.rs-v[data-kind="model"]')].find((b) => b.offsetParent !== null);
  if (!chip) return toast({ title: "This harness can't switch here", tone: "info" });
  chip.scrollIntoView({ block: "nearest" });
  chip.focus();
  if (chip.getAttribute("aria-expanded") !== "true") chip.click();
}

export function SandboxTab({ agentId, store, inRow }: { agentId: string; store: SessionStore; inRow?: boolean }) {
  // What's been changed of it live, and how each place took it: its state, as the server sends it.
  const o = useSession(store, (s) => s.state?.overrides);
  const res = useRes(sandboxRes(agentId));
  const d = res.data;
  // What its launch gave it, the base its live overrides change (`sessions.grants`).
  const grants = useRes(grantsRes(agentId)).data;
  const host = d.engine === "host";
  const ended = d.state === "ended";
  /** An act's answer is the sandbox as it is now. */
  const done = (next: SandboxDetail) => res.setData(() => next);
  const fail = (what: string) => (e: unknown) => {
    toast.error(what, errorMessage(e));
    res.reload();
  };
  return (
    <div className="in-secs sb">
      <Now d={d} agentId={agentId} done={done} fail={fail} change={res.change} bare={inRow} />
      {!ended && (host ? <HostFacts d={d} /> : <MicrovmFacts d={d} />)}
      {!ended && d.components.length > 0 && <Components d={d} agentId={agentId} done={done} fail={fail} />}
      {!ended && !host && d.network && <Network d={d} o={o} agentId={agentId} />}
      {!ended && <Volumes d={d} o={o} agentId={agentId} base={grants.base} />}
      {!ended && d.scope && <Reach d={d} o={o} />}
      {!ended && <Plots d={d} />}
    </div>
  );
}

type Acts = { agentId: string; done: (d: SandboxDetail) => void; fail: (what: string) => (e: unknown) => void };

/** Where we are now: its engine and state, CPU, memory and disk as live bars, then its controls. */
function Now({ d, agentId, done, fail, change, bare }: { d: SandboxDetail; change: (fn: (prev: SandboxDetail) => SandboxDetail) => void; bare?: boolean } & Acts) {
  const versions = useVersions(agentId);
  const [asking, setAsking] = React.useState(false);
  const host = d.engine === "host";
  const st = STATE[d.state];
  const behind = versions && versions.started < versions.current ? versions.current : null;
  const act = (a: "pause" | "resume" | "restart" | "stop", body: { version?: number } = {}) => {
    const before = d;
    if (a === "pause" || a === "resume") change((x) => ({ ...x, state: a === "pause" ? "paused" : "running" }));
    sandboxApi
      .act(agentId, a, body)
      .then((next) => {
        done(next);
        if (a === "pause") toast({ title: "Paused its vCPUs", tone: "info", action: { label: "Undo", run: () => void sandboxApi.act(agentId, "resume").then(done) } });
        else if (a === "restart") toast({ title: body.version ? `Restarting on v${body.version}` : "Restarting", tone: "running" });
        else if (a === "stop") toast({ title: "Stopped", tone: "info" });
      })
      .catch((e: unknown) => {
        done(before);
        fail(`Couldn't ${a}`)(e);
      });
  };
  const now = d.now;
  return (
    <section className="sb-now" aria-label="Now">
      {!bare && (
        <h3 className="in-sec-head">
          Now
          <span className="sb-where">
            <EngineGlyph engine={d.engine} word />
            <span>{labelOf(d.runner)}</span>
            <StatusDot status={st.status} title={st.word} />
            <span>{st.word}</span>
            {!host && d.state !== "ended" && <span>· up {up(d.up_s)}</span>}
          </span>
        </h3>
      )}
      {host && !bare && (
        <p className="sb-hostline">
          <EngineGlyph engine="host" />
          Runs on the host, without a microVM
        </p>
      )}
      {now && (
        <div className="sb-bars">
          <LimitMeter
            live
            limit={{ id: "cpu", label: "CPU", used_percent: now.cpu_pct, ...(d.vm ? { used: Math.round((now.cpu_pct / 100) * d.vm.vcpus * 100) / 100, limit: d.vm.vcpus, unit: "vCPU" } : {}) }}
          />
          <LimitMeter live limit={{ id: "memory", label: "Memory", used_percent: (now.memory.used / now.memory.total) * 100, used: gib(now.memory.used), limit: gib(now.memory.total), unit: "GiB" }} />
          <LimitMeter live limit={{ id: "disk", label: "Disk", used_percent: (now.disk.used / now.disk.total) * 100, used: gib(now.disk.used), limit: gib(now.disk.total), unit: "GiB" }} />
        </div>
      )}
      {d.state !== "ended" && !bare && (
        <div className="sb-ctl">
          {asking ? (
            <span className="sb-confirm" role="group" aria-label="Stop it?">
              <span>Stop it?</span>
              <button
                type="button"
                className="ms-go"
                aria-label="Stop it"
                onClick={() => {
                  setAsking(false);
                  act("stop");
                }}
              >
                <Check size={15} />
              </button>
              <button type="button" className="ms-go" aria-label="Keep it running" onClick={() => setAsking(false)}>
                <X size={15} />
              </button>
            </span>
          ) : (
            <>
              {!host && (
                <Button size="xs" variant="secondary" onClick={() => act(d.state === "paused" ? "resume" : "pause")} title="A vCPU pause: memory and disk stay as they are">
                  {d.state === "paused" ? <Play aria-hidden="true" /> : <Pause aria-hidden="true" />}
                  {d.state === "paused" ? "Resume" : "Pause"}
                </Button>
              )}
              <Button size="xs" variant="secondary" onClick={() => act("restart")}>
                <RotateCcw aria-hidden="true" />
                Restart
              </Button>
              {behind && (
                <Button size="xs" onClick={() => act("restart", { version: behind })} title={`Started on v${versions!.started}; v${behind} is current`}>
                  <RotateCcw aria-hidden="true" />
                  Restart on v{behind}
                </Button>
              )}
              <Button size="xs" variant="ghost-danger" onClick={() => setAsking(true)}>
                <Square aria-hidden="true" />
                Stop
              </Button>
            </>
          )}
        </div>
      )}
    </section>
  );
}

/** A key and its value, one line, as the rows below read. */
function Kv({ k, children, title }: { k: string; children: React.ReactNode; title?: string }) {
  return (
    <li className="sb-kv" title={title}>
      <span className="sb-k">{k}</span>
      <span className="sb-v">{children}</span>
    </li>
  );
}

/** In a microVM: its Hex box, its Shards image and size, its footprint, how it started, the secret names it was built with. */
function MicrovmFacts({ d }: { d: SandboxDetail }) {
  const vm = d.vm;
  if (!vm) return null;
  return (
    <Fold id="sb-microvm" title="MicroVM">
      <ul className="sb-list">
        {d.hex && (
          <Kv k="Hex box" title={`Hex box ${d.hex.box} on node ${d.hex.node}: ${d.hex.state}`}>
            <span className="sb-mono">{d.hex.box}</span>
            <span className="sb-dim">{labelOf(d.hex.node)}</span>
            <span className="sb-dim">{d.hex.state}</span>
          </Kv>
        )}
        <Kv k="Image" title={`${vm.image} v${vm.version} · ${vm.digest}`}>
          <span>
            {vm.image} v{vm.version}
          </span>
          <span className="sb-mono sb-dim">{vm.digest.replace(/^sha256:/, "").slice(0, 12)}</span>
        </Kv>
        <Kv k="Size">
          <span className="sb-mono">
            {vm.vcpus} vCPU · {gib(vm.memory)} GiB
          </span>
        </Kv>
        <Kv k="Footprint" title="Peak and steady memory, measured">
          <span className="sb-mono">
            peak {gib(vm.footprint.peak).toFixed(2)} · steady {gib(vm.footprint.steady).toFixed(2)} GiB
          </span>
        </Kv>
        {d.start && (
          <Kv k="Start">
            <StartMark kind={d.start.kind} ms={d.start.ms} />
            {d.pool ? (
              <span className="sb-pool" title={`${d.pool.agent}'s pool · ${d.pool.image} · ${d.pool.cpus} vCPU · ${gib(d.pool.memory)} GiB`}>
                <span className="sb-dim">{d.pool.agent} pool</span>
                <WarmDots n={d.pool.warm} of={d.pool.bound} />
              </span>
            ) : (
              <span className="sb-dim">{d.start.kind === "template" ? "its template" : "booted"}</span>
            )}
          </Kv>
        )}
        {d.secrets && (
          <Kv k="Secrets" title="Names only: the values never leave kiln">
            {d.secrets.length === 0 ? (
              <span className="sb-dim">none</span>
            ) : (
              d.secrets.map((s) => (
                <span key={s.name} className="sb-chip">
                  <KeyRound size={11} aria-hidden="true" />
                  {s.name}
                  <i>{s.scope}</i>
                </span>
              ))
            )}
          </Kv>
        )}
      </ul>
    </Fold>
  );
}

/** On the host: its runner, the harness process it runs as, and why it's here. */
function HostFacts({ d }: { d: SandboxDetail }) {
  return (
    <Fold id="sb-host" title="Host">
      <ul className="sb-list">
        <Kv k="Runner">
          <span>{labelOf(d.runner)}</span>
        </Kv>
        {d.process && (
          <Kv k="Process">
            <span>{d.process.harness}</span>
            <span className="sb-mono sb-dim">pid {d.process.pid}</span>
            <span className="sb-mono sb-dim">{d.process.user}</span>
          </Kv>
        )}
        {d.why && (
          <Kv k="Why" title={d.why}>
            <span className="sb-dim sb-clip">{d.why}</span>
          </Kv>
        )}
      </ul>
    </Fold>
  );
}

function MemberGlyph({ kind, name }: { kind: MemberKind; name: string }) {
  return kind === "harness" ? <HarnessMark kind={name} size={14} className="sb-mark" /> : <KindGlyph kind={kind === "agent" ? "agent" : "mcp"} />;
}

/** Its harness, agents and MCP servers, each at its path: swapped in place, put back as built. */
function Components({ d, agentId, done, fail }: { d: SandboxDetail } & Acts) {
  const [swapping, setSwapping] = React.useState<string | null>(null);
  const host = d.engine === "host";
  return (
    <Fold id="sb-components" title="Components" count={d.components.length}>
      <p className="sb-note">{host ? "Installs into this session's scope" : "Swaps in place: the microVM and the rest keep running"}</p>
      <ul className="sb-rows">
        {d.components.map((c) => (
          <React.Fragment key={c.name}>
            <ComponentRow
              c={c}
              open={swapping === c.name}
              onSwap={() => setSwapping(swapping === c.name ? null : c.name)}
              onRestore={() =>
                sandboxApi.restore(agentId, c.name).then(
                  (next) => {
                    done(next);
                    toast({ title: `${c.name} is back to ${c.built?.version ?? "as built"}`, tone: "info" });
                  },
                  fail(`Couldn't put ${c.name} back`),
                )
              }
            />
            {swapping === c.name && (
              <SwapForm
                c={c}
                agentId={agentId}
                done={(next) => {
                  done(next);
                  setSwapping(null);
                }}
                fail={fail}
              />
            )}
          </React.Fragment>
        ))}
      </ul>
    </Fold>
  );
}

function ComponentRow({ c, open, onSwap, onRestore }: { c: SandboxComponent; open: boolean; onSwap: () => void; onRestore: () => void }) {
  const S = SOURCE[c.from.kind];
  return (
    <li className="sb-row sb-comp" data-swapped={c.built ? "" : undefined}>
      <MemberGlyph kind={c.kind} name={c.name} />
      {/* Two lines, each whole: its name and version; its path, where it comes from and its pin. */}
      <span className="sb-two">
        <span className="sb-two-top">
          <span className="sb-name" title={`${c.kind} ${c.name}`}>
            {c.name}
          </span>
          <span className="sb-mono sb-ver" title={c.built ? `Swapped from ${c.built.version}` : "As built"}>
            {c.version}
          </span>
        </span>
        <span className="sb-two-sub">
          <span className="sb-mono sb-dim sb-clip" title={c.path}>
            {c.path}
          </span>
          <span className="sb-src" title={`${c.from.url} · ${c.from.pin}`}>
            <S.icon size={11} aria-hidden="true" />
            <span className="sb-mono">{pinShort(c.from.pin)}</span>
          </span>
        </span>
      </span>
      <span className="sb-acts">
        {c.kind === "harness" ? (
          <Button size="xs" variant="ghost" onClick={openHarnessSwitch} title="kiln's harness switch, in the head">
            Switch
          </Button>
        ) : c.built ? (
          <Button size="xs" variant="ghost" onClick={onRestore} title={`Back to ${c.built.version}, as built`}>
            <Undo2 aria-hidden="true" />
            Restore
          </Button>
        ) : (
          <Button size="xs" variant="ghost" aria-expanded={open} onClick={onSwap}>
            Swap
          </Button>
        )}
      </span>
    </li>
  );
}

/** A new source and pin for a component, swapped at its path when pressed. */
function SwapForm({ c, agentId, done }: { c: SandboxComponent } & Acts) {
  const [source, setSource] = React.useState(c.from.url);
  const [pin, setPin] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const [why, setWhy] = React.useState<string | null>(null);
  const hint = /^s3:\/\//.test(source) ? "sha256:… or version ID" : /\.git$|^git@/.test(source) ? "tag, branch or commit" : "sha256:…";
  return (
    <li className="sb-form" aria-label={`Swap ${c.name}`}>
      <input className="sb-in sb-grow" value={source} aria-label="Source" spellCheck={false} onChange={(e) => setSource(e.target.value)} />
      <input className="sb-in sb-pin" value={pin} placeholder={hint} aria-label="Pin" spellCheck={false} onChange={(e) => setPin(e.target.value)} />
      <Button
        size="xs"
        pending={busy}
        disabled={!source.trim() || !pin.trim()}
        onClick={() => {
          setBusy(true);
          setWhy(null);
          sandboxApi
            .swap(agentId, c.name, source.trim(), pin.trim())
            .then((next) => {
              done(next);
              toast({ title: `Swapped ${c.name}`, tone: "info" });
            })
            .catch((e: unknown) => setWhy(errorMessage(e)))
            .finally(() => setBusy(false));
        }}
      >
        Swap
      </Button>
      {why && <span className="sb-why">{why}</span>}
    </li>
  );
}

/** How one place (its microVM, its harness, kiln's scope) took a change: a dot in kiln's stand colours, its words in its title. */
const TOOK: Record<LiveTook["took"], { status: string; word: string }> = {
  now: { status: "succeeded", word: "took it now" },
  next_turn: { status: "queued", word: "takes it from its next turn" },
  next_start: { status: "queued", word: "takes it as it next starts" },
  nothing: { status: "skipped", word: "has nothing to change" },
  refused: { status: "failed", word: "refused it" },
};
function Took({ took, place }: { took: LiveTook; place: string }) {
  const t = TOOK[took.took];
  const words = "how" in took ? took.how : took.why;
  return <StatusDot status={t.status} title={`${place} ${t.word}: ${words}`} />;
}
/** A member in a few words: its name, an agent's or an MCP server's marked. */
const said = (m: LiveMember) => (m.kind === "harness" ? m.name : `${m.kind} ${m.name}`);

/** An act on the session's live overrides: it goes to its machine, and its state brings back how each place took it. */
function useLive(agentId: string) {
  const [busy, setBusy] = React.useState(false);
  const [why, setWhy] = React.useState<string | null>(null);
  const run = (what: string, act: () => Promise<OverrideAnswer>, after?: () => void) => {
    setBusy(true);
    setWhy(null);
    act()
      .then(() => after?.())
      .catch((e: unknown) => {
        setWhy(errorMessage(e));
        toast.error(what, errorMessage(e));
      })
      .finally(() => setBusy(false));
  };
  return { agentId, busy, why, run };
}

/**
 * In a microVM only: ports published in, egress granted out, and who is on which network, its Agentfile's
 * (its base grants, read only here) and what's been changed live (each with how its microVM took it).
 * Nothing else goes in or out.
 */
function Network({ d, o, agentId }: { d: SandboxDetail; o: LiveOverrides | undefined; agentId: string }) {
  const base = d.network!;
  const live = useLive(agentId);
  const [adding, setAdding] = React.useState<"ingress" | "egress" | "network" | null>(null);
  const ports = o?.ports ?? [];
  const egress = o?.egress ?? [];
  const networks = o?.networks ?? [];
  return (
    <Fold id="sb-network" title="Network" count={`${base.ingress.length + ports.length} in · ${base.egress.length + egress.length} out`}>
      <div className="sb-sub">
        <span className="sb-k">Ingress</span>
        {adding !== "ingress" && (
          <Button size="xs" variant="ghost" onClick={() => setAdding("ingress")}>
            <Plus aria-hidden="true" />
            Publish
          </Button>
        )}
      </div>
      <ul className="sb-rows">
        {base.ingress.map((i) => (
          <li key={`base:${i.host}/${i.proto}`} className="sb-row sb-flow" data-base="">
            <ArrowRight size={12} className="sb-in-glyph" aria-hidden="true" />
            <span className="sb-mono sb-clip" title={`From its Agentfile: ${i.host} → ${i.guest}/${i.proto}, to ${i.to}`}>
              {i.host} → {i.guest}/{i.proto}
            </span>
            <span className="sb-dim sb-clip">{i.to}</span>
            <span className="sb-tags">
              <i title="From its Agentfile">built</i>
            </span>
            <span />
          </li>
        ))}
        {ports.map((h) => (
          <li key={`live:${h.port.bind}:${h.port.host_port}/${h.port.protocol}`} className="sb-row sb-flow">
            <Took took={h.microvm} place="Its microVM" />
            <span className="sb-mono sb-clip" title={`${h.port.bind}:${h.port.host_port} on its machine → ${h.port.guest_port}/${h.port.protocol}, to ${said(h.port.member)}`}>
              {h.port.bind}:{h.port.host_port} → {h.port.guest_port}/{h.port.protocol}
            </span>
            <span className="sb-dim sb-clip">{said(h.port.member)}</span>
            <span className="sb-tags" />
            <span className="sb-acts">
              <button
                type="button"
                className="ms-go"
                disabled={live.busy}
                aria-label={`Unpublish ${h.port.bind}:${h.port.host_port}`}
                title="Unpublish"
                onClick={() => live.run("Couldn't unpublish", () => overridesApi.unpublish(agentId, { bind: h.port.bind, host_port: h.port.host_port, protocol: h.port.protocol }))}
              >
                <X size={14} />
              </button>
            </span>
          </li>
        ))}
        {base.ingress.length + ports.length === 0 && <li className="sb-deny">nothing published</li>}
        {adding === "ingress" && <PublishForm d={d} live={live} onDone={() => setAdding(null)} />}
      </ul>
      <div className="sb-sub">
        <span className="sb-k">Egress</span>
        {adding !== "egress" && (
          <Button size="xs" variant="ghost" onClick={() => setAdding("egress")}>
            <Plus aria-hidden="true" />
            Grant
          </Button>
        )}
      </div>
      <ul className="sb-rows">
        {base.egress.map((e) => (
          <li key={`base:${e.to}:${e.ports}/${e.who}`} className="sb-row sb-flow" data-base="">
            <ArrowRight size={12} className="sb-out-glyph" aria-hidden="true" />
            <span className="sb-mono sb-clip" title={`From its Agentfile: ${e.who} → ${e.to} on ${e.ports}`}>
              {e.to}:{e.ports}
            </span>
            <span className="sb-dim sb-clip">{e.who}</span>
            <span className="sb-tags">
              <i title="From its Agentfile">built</i>
            </span>
            <span />
          </li>
        ))}
        {egress.map((h) => (
          <li key={`live:${h.egress.host}:${h.egress.port}/${memberWord(h.egress.member)}`} className="sb-row sb-flow">
            <Took took={h.microvm} place="Its microVM" />
            <span className="sb-mono sb-clip" title={`${said(h.egress.member)} → ${h.egress.host} on ${h.egress.port}`}>
              {h.egress.host}:{h.egress.port}
            </span>
            <span className="sb-dim sb-clip">{said(h.egress.member)}</span>
            <span className="sb-tags" />
            <span className="sb-acts">
              <button
                type="button"
                className="ms-go"
                disabled={live.busy}
                aria-label={`Revoke ${h.egress.host}:${h.egress.port}`}
                title="Revoke"
                onClick={() => live.run("Couldn't revoke", () => overridesApi.revoke(agentId, { member: memberWord(h.egress.member), host: h.egress.host, port: h.egress.port }))}
              >
                <X size={14} />
              </button>
            </span>
          </li>
        ))}
        <li className="sb-deny">{base.egress.length + egress.length === 0 ? "nothing granted" : "nothing else"}</li>
        {adding === "egress" && <GrantForm d={d} live={live} onDone={() => setAdding(null)} />}
      </ul>
      <div className="sb-sub">
        <span className="sb-k">Networks</span>
        {adding !== "network" && (
          <Button size="xs" variant="ghost" onClick={() => setAdding("network")}>
            <Plus aria-hidden="true" />
            Connect
          </Button>
        )}
      </div>
      <ul className="sb-rows">
        {base.networks.map((n) => (
          <li key={`base:${n.name}`} className="sb-row sb-net" data-base="">
            <span className="sb-mono sb-clip" title={`${n.name}, from its Agentfile`}>
              {n.name}
            </span>
            <span className="sb-members">
              {n.members
                .filter((m) => m.connected)
                .map((m) => (
                  <span key={m.name} className="sb-member" aria-pressed="true" title={`${m.kind} ${m.name}, from its Agentfile`}>
                    <i aria-hidden="true" />
                    {m.name}
                  </span>
                ))}
            </span>
          </li>
        ))}
        {networks.map((h) => (
          <li key={`live:${h.network.name}/${memberWord(h.network.member)}`} className="sb-row sb-flow">
            <Took took={h.microvm} place="Its microVM" />
            <span className="sb-mono sb-clip" title={`${said(h.network.member)} on ${h.network.name}, shared with the runs connected to it`}>
              {h.network.name}
            </span>
            <span className="sb-dim sb-clip">{said(h.network.member)}</span>
            <span className="sb-tags" />
            <span className="sb-acts">
              <button
                type="button"
                className="ms-go"
                disabled={live.busy}
                aria-label={`Disconnect ${said(h.network.member)} from ${h.network.name}`}
                title="Disconnect"
                onClick={() => live.run("Couldn't disconnect", () => overridesApi.disconnect(agentId, { member: memberWord(h.network.member), network: h.network.name }))}
              >
                <X size={14} />
              </button>
            </span>
          </li>
        ))}
        {base.networks.length + networks.length === 0 && <li className="sb-deny">no networks</li>}
        {adding === "network" && <ConnectForm d={d} live={live} onDone={() => setAdding(null)} />}
      </ul>
    </Fold>
  );
}

function MemberSelect({ members, value, onChange, label }: { members: SandboxMember[]; value: string; onChange: (v: string) => void; label: string }) {
  return (
    <select className="sb-in" value={value} aria-label={label} onChange={(e) => onChange(e.target.value)}>
      {members.map((m) => (
        <option key={memberWord(m)} value={memberWord(m)}>
          {m.kind === "harness" ? m.name : `${m.kind} ${m.name}`}
        </option>
      ))}
    </select>
  );
}

type Live = ReturnType<typeof useLive>;
const firstAgent = (d: SandboxDetail) => memberWord(d.members.find((m) => m.kind === "agent") ?? d.members[0] ?? { kind: "harness", name: "" });

function PublishForm({ d, live, onDone }: { d: SandboxDetail; live: Live; onDone: () => void }) {
  const [bind, setBind] = React.useState("127.0.0.1");
  const [hostPort, setHostPort] = React.useState("8080");
  const [guest, setGuest] = React.useState("8080");
  const [protocol, setProtocol] = React.useState<"tcp" | "udp">("tcp");
  const [member, setMember] = React.useState(firstAgent(d));
  return (
    <li className="sb-form" aria-label="Publish a port">
      <input
        className="sb-in sb-addr"
        value={bind}
        aria-label="Address on its machine"
        title="Who may reach it: 127.0.0.1 this machine alone"
        spellCheck={false}
        onChange={(e) => setBind(e.target.value)}
      />
      <input className="sb-in sb-port" value={hostPort} inputMode="numeric" aria-label="Port on its machine" onChange={(e) => setHostPort(e.target.value)} />
      <ArrowRight size={12} aria-hidden="true" />
      <input className="sb-in sb-port" value={guest} inputMode="numeric" aria-label="Port in its microVM" onChange={(e) => setGuest(e.target.value)} />
      <Segmented
        size="xs"
        label="Protocol"
        value={protocol}
        onChange={setProtocol}
        options={[
          { value: "tcp", label: "tcp" },
          { value: "udp", label: "udp" },
        ]}
      />
      <MemberSelect members={d.members} value={member} onChange={setMember} label="To" />
      <Button
        size="xs"
        pending={live.busy}
        onClick={() =>
          live.run("Couldn't publish it", () => overridesApi.publish(live.agentId, { member, bind: bind.trim(), host_port: Number(hostPort), guest_port: Number(guest), protocol }), onDone)
        }
      >
        Publish
      </Button>
      <button type="button" className="ms-go" aria-label="Cancel" onClick={onDone}>
        <X size={14} />
      </button>
      {live.why && <span className="sb-why">{live.why}</span>}
    </li>
  );
}

function GrantForm({ d, live, onDone }: { d: SandboxDetail; live: Live; onDone: () => void }) {
  const [host, setHost] = React.useState("");
  const [port, setPort] = React.useState("443");
  const [member, setMember] = React.useState(firstAgent(d));
  return (
    <li className="sb-form" aria-label="Grant egress">
      <input className="sb-in sb-grow" value={host} placeholder="host" aria-label="Host: a DNS name or an address" spellCheck={false} onChange={(e) => setHost(e.target.value)} />
      <input className="sb-in sb-port" value={port} inputMode="numeric" aria-label="Port" onChange={(e) => setPort(e.target.value)} />
      <MemberSelect members={d.members} value={member} onChange={setMember} label="For" />
      <Button
        size="xs"
        pending={live.busy}
        disabled={!host.trim()}
        onClick={() => live.run("Couldn't grant it", () => overridesApi.grant(live.agentId, { member, host: host.trim(), port: Number(port) }), onDone)}
      >
        Grant
      </Button>
      <button type="button" className="ms-go" aria-label="Cancel" onClick={onDone}>
        <X size={14} />
      </button>
      {live.why && <span className="sb-why">{live.why}</span>}
    </li>
  );
}

function ConnectForm({ d, live, onDone }: { d: SandboxDetail; live: Live; onDone: () => void }) {
  const [network, setNetwork] = React.useState("");
  const [member, setMember] = React.useState(firstAgent(d));
  return (
    <li className="sb-form" aria-label="Connect to a network">
      <MemberSelect members={d.members} value={member} onChange={setMember} label="Member" />
      <input className="sb-in sb-grow" value={network} placeholder="network" aria-label="Network's name" spellCheck={false} onChange={(e) => setNetwork(e.target.value)} />
      <Button
        size="xs"
        pending={live.busy}
        disabled={!network.trim()}
        onClick={() => live.run("Couldn't connect it", () => overridesApi.connect(live.agentId, { member, network: network.trim() }), onDone)}
      >
        Connect
      </Button>
      <button type="button" className="ms-go" aria-label="Cancel" onClick={onDone}>
        <X size={14} />
      </button>
      {live.why && <span className="sb-why">{live.why}</span>}
    </li>
  );
}

/**
 * Its folders, on either engine: its Agentfile's (read only here), then those given live, each with how
 * its harness, kiln's scope and (a microVM's) its microVM took it; taken back when confirmed.
 */
function Volumes({ d, o, agentId, base }: { d: SandboxDetail; o: LiveOverrides | undefined; agentId: string; base: SessionGrants["base"] }) {
  const live = useLive(agentId);
  const [adding, setAdding] = React.useState(false);
  const [asking, setAsking] = React.useState<string | null>(null);
  const host = d.engine === "host";
  const given = o?.volumes ?? [];
  // Its base, as its launch gave it: on its host, the folders its harness is allowed; in a microVM, the
  // machine's folders it sees, each where and whether writable.
  const built = host
    ? base.allowed_dirs.map((p) => ({ at: p, from: "its folder", mode: "rw" as const }))
    : base.volumes.map((v) => ({ at: v.at, from: v.host, mode: v.writable ? ("rw" as const) : ("ro" as const) }));
  return (
    <Fold
      id="sb-volumes"
      title="Volumes"
      count={built.length + given.length}
      aside={
        !adding && (
          <Button size="xs" variant="ghost" onClick={() => setAdding(true)}>
            <Plus aria-hidden="true" />
            Add volume
          </Button>
        )
      }
    >
      <p className="sb-note">{host ? "Adds to the agent's allowed directories and this session's scope" : "Mounts into this microVM as you set, then allowed directories and scope"}</p>
      <ul className="sb-rows">
        {built.map((v) => (
          <li key={`base:${v.at}`} className="sb-row sb-vol" data-base="">
            <FolderOpen size={12} className="sb-dim" aria-hidden="true" />
            <span className="sb-two">
              <span className="sb-two-top">
                <span className="sb-mono sb-clip" title={`${v.at}, as it started`}>
                  {v.at}
                </span>
                <span className="sb-mode" data-mode={v.mode}>
                  {v.mode}
                </span>
              </span>
              <span className="sb-two-sub">
                <span className="sb-dim sb-clip" title={v.from}>
                  {v.from}
                </span>
              </span>
            </span>
            <span className="sb-tags">
              <i title="Given as it started">base</i>
            </span>
          </li>
        ))}
        {given.map((h) => (
          <li key={`live:${h.volume.path}`} className="sb-row sb-vol">
            <span className="sb-took">
              <Took took={h.harness} place="Its harness" />
              <Took took={h.scope} place="kiln's scope" />
              {h.microvm && <Took took={h.microvm} place="Its microVM" />}
            </span>
            <span className="sb-two">
              <span className="sb-two-top">
                <span className="sb-mono sb-clip" title={h.volume.at === h.volume.path ? h.volume.path : `${h.volume.path}, seen at ${h.volume.at}`}>
                  {h.volume.path}
                </span>
                <span className="sb-mode" data-mode={h.volume.access}>
                  {h.volume.access}
                </span>
              </span>
              <span className="sb-two-sub">
                <span className="sb-dim sb-clip">{h.volume.members.map(said).join(", ")}</span>
                {h.volume.at !== h.volume.path && <span className="sb-mono sb-dim">at {h.volume.at}</span>}
              </span>
            </span>
            <span className="sb-acts">
              {asking === h.volume.path ? (
                <span className="sb-confirm" role="group" aria-label={`Take ${h.volume.path} back?`}>
                  <span>Remove?</span>
                  <button
                    type="button"
                    className="ms-go"
                    aria-label={`Take ${h.volume.path} back`}
                    onClick={() => {
                      setAsking(null);
                      live.run("Couldn't take it back", () => overridesApi.removeVolume(agentId, h.volume.path));
                    }}
                  >
                    <Check size={14} />
                  </button>
                  <button type="button" className="ms-go" aria-label="Keep it" onClick={() => setAsking(null)}>
                    <X size={14} />
                  </button>
                </span>
              ) : (
                <button type="button" className="ms-go" aria-label={`Take ${h.volume.path} back`} title="Remove" onClick={() => setAsking(h.volume.path)}>
                  <X size={14} />
                </button>
              )}
            </span>
          </li>
        ))}
        {adding && <VolumeForm d={d} live={live} onDone={() => setAdding(false)} />}
      </ul>
    </Fold>
  );
}

function VolumeForm({ d, live, onDone }: { d: SandboxDetail; live: Live; onDone: () => void }) {
  const people = d.members.filter((m) => m.kind !== "mcp");
  const [path, setPath] = React.useState("");
  const [at, setAt] = React.useState("");
  const [access, setAccess] = React.useState<"ro" | "rw">("ro");
  const [members, setMembers] = React.useState<string[]>(people.map(memberWord));
  const microvm = d.engine === "microvm";
  return (
    <li className="sb-form" aria-label="Add a volume">
      <input className="sb-in sb-grow" value={path} placeholder="/path on its machine" aria-label="Folder on its machine" spellCheck={false} onChange={(e) => setPath(e.target.value)} />
      {microvm && (
        <input className="sb-in sb-pin" value={at} placeholder="at (in its microVM)" aria-label="Where its members see it in the microVM" spellCheck={false} onChange={(e) => setAt(e.target.value)} />
      )}
      <Segmented
        size="xs"
        label="Access"
        value={access}
        onChange={setAccess}
        options={[
          { value: "ro", label: "ro" },
          { value: "rw", label: "rw" },
        ]}
      />
      <span className="sb-members">
        {people.map((m) => {
          const w = memberWord(m);
          return (
            <button key={w} type="button" className="sb-member" aria-pressed={members.includes(w)} onClick={() => setMembers(members.includes(w) ? members.filter((x) => x !== w) : [...members, w])}>
              <i aria-hidden="true" />
              {m.name}
            </button>
          );
        })}
      </span>
      <Button
        size="xs"
        pending={live.busy}
        disabled={!path.trim() || members.length === 0}
        onClick={() => live.run("Couldn't give it the folder", () => overridesApi.addVolume(live.agentId, { path: path.trim(), access, ...(at.trim() ? { at: at.trim() } : {}), members }), onDone)}
      >
        Add
      </Button>
      <button type="button" className="ms-go" aria-label="Cancel" onClick={onDone}>
        <X size={14} />
      </button>
      {live.why && <span className="sb-why">{live.why}</span>}
    </li>
  );
}

/** What each member may touch (its allowed directories: its Agentfile's and those given live), and the session's scope. */
function Reach({ d, o }: { d: SandboxDetail; o: LiveOverrides | undefined }) {
  const scope = d.scope!;
  const given = o?.volumes ?? [];
  const dirs = d.dirs.map((m) => ({
    ...m,
    paths: [...m.paths, ...given.filter((h) => h.volume.members.some((x) => x.name === m.member)).map((h) => ({ path: h.volume.at, mode: h.volume.access }))],
  }));
  const paths = [...scope.paths, ...given.map((h) => h.volume.at)];
  return (
    <Fold id="sb-reach" title="Directories" count={dirs.length}>
      <ul className="sb-list">
        {dirs.map((m) => (
          // Each path whole, one a line: a directory is never cut.
          <li key={m.member} className="sb-kv sb-stacked">
            <span className="sb-k">{m.member}</span>
            <span className="sb-paths">
              {m.paths.map((p) => (
                <span key={p.path} className="sb-mono sb-path" data-mode={p.mode}>
                  {p.path}
                </span>
              ))}
            </span>
          </li>
        ))}
        <Kv k="Scope" title={`${scope.project} · ${scope.branch} · ${paths.join(", ")}`}>
          <span>{scope.project}</span>
          <span className="sb-mono sb-dim">{scope.branch}</span>
          <span className="sb-dim">
            {paths.length} path{paths.length === 1 ? "" : "s"}
          </span>
        </Kv>
      </ul>
    </Fold>
  );
}

/** The last hour, full width: CPU, memory, disk and network as plots. */
function Plots({ d }: { d: SandboxDetail }) {
  const s = d.series;
  const rows: [string, number[], number | undefined, string][] = [
    ["CPU", s.cpu, 100, `${s.cpu.at(-1) ?? 0}%`],
    ["Memory", s.memory, 100, `${s.memory.at(-1) ?? 0}%`],
    ["Disk", s.disk, 100, `${s.disk.at(-1) ?? 0}%`],
    ["Network", s.net, undefined, `${((s.net.at(-1) ?? 0) / 10).toFixed(1)} MB/s`],
  ];
  return (
    <Fold id="sb-plots" title="Last hour">
      <div className="sb-plots">
        {rows.map(([label, values, max, now]) => (
          <div key={label} className="sb-plot">
            <span className="sb-plot-head">
              <span className="sb-k">{label}</span>
              <span className="sb-mono">{now}</span>
            </span>
            <Sparkline values={values} max={max} width={480} height={44} className="sb-wide" />
          </div>
        ))}
      </div>
    </Fold>
  );
}

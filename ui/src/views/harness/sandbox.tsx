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
 */
import * as React from "react";
import { ArrowRight, Check, Cloud, Download, FolderOpen, GitBranch, KeyRound, Pause, Play, Plus, RotateCcw, Square, Undo2, X } from "lucide-react";
import { sandboxApi, sandboxRes, type ComponentSource, type MemberKind, type SandboxComponent, type SandboxDetail, type SandboxMember } from "@/api/microvms";
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

export function SandboxTab({ agentId }: { agentId: string }) {
  const res = useRes(sandboxRes(agentId));
  const d = res.data;
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
      <Now d={d} agentId={agentId} done={done} fail={fail} change={res.change} />
      {!ended && (host ? <HostFacts d={d} /> : <MicrovmFacts d={d} />)}
      {!ended && d.components.length > 0 && <Components d={d} agentId={agentId} done={done} fail={fail} />}
      {!ended && !host && d.network && <Network d={d} agentId={agentId} done={done} fail={fail} />}
      {!ended && <Volumes d={d} agentId={agentId} done={done} fail={fail} />}
      {!ended && d.scope && <Reach d={d} />}
      {!ended && <Plots d={d} />}
    </div>
  );
}

type Acts = { agentId: string; done: (d: SandboxDetail) => void; fail: (what: string) => (e: unknown) => void };

/** Where we are now: its engine and state, CPU, memory and disk as live bars, then its controls. */
function Now({ d, agentId, done, fail, change }: { d: SandboxDetail; change: (fn: (prev: SandboxDetail) => SandboxDetail) => void } & Acts) {
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
      {host && (
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
      {d.state !== "ended" && (
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

/** In a microVM only: ports published in, egress granted out, and who is on which network. Nothing else goes in or out. */
function Network({ d, agentId, done, fail }: { d: SandboxDetail } & Acts) {
  const net = d.network!;
  const [adding, setAdding] = React.useState<"ingress" | "egress" | null>(null);
  const toggle = (network: string, member: string, connected: boolean) =>
    sandboxApi.connect(agentId, network, member, connected).then(done, fail(`Couldn't ${connected ? "connect" : "disconnect"} ${member}`));
  return (
    <Fold id="sb-network" title="Network" count={`${net.ingress.length} in · ${net.egress.length} out`}>
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
        {net.ingress.map((i) => (
          <li key={`${i.host}/${i.proto}`} className="sb-row sb-flow">
            <ArrowRight size={12} className="sb-in-glyph" aria-hidden="true" />
            <span className="sb-mono sb-clip" title={`${i.host} on the runner → port ${i.guest}/${i.proto} in the microVM, to ${i.to}`}>
              {i.host} → {i.guest}/{i.proto}
            </span>
            <span className="sb-dim sb-clip">{i.to}</span>
            <span className="sb-acts">
              <button type="button" className="ms-go" aria-label={`Unpublish ${i.host}`} title="Unpublish" onClick={() => sandboxApi.publish(agentId, i, true).then(done, fail("Couldn't unpublish"))}>
                <X size={14} />
              </button>
            </span>
          </li>
        ))}
        {net.ingress.length === 0 && <li className="sb-deny">nothing published</li>}
        {adding === "ingress" && (
          <PublishForm
            d={d}
            agentId={agentId}
            done={(n) => {
              done(n);
              setAdding(null);
            }}
            fail={fail}
            onCancel={() => setAdding(null)}
          />
        )}
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
        {net.egress.map((e) => (
          <li key={`${e.to}:${e.ports}/${e.who}`} className="sb-row sb-flow">
            <ArrowRight size={12} className="sb-out-glyph" aria-hidden="true" />
            <span className="sb-mono sb-clip" title={`${e.who} → ${e.to} on ${e.ports}${e.dns ? ", its name resolved" : ""}${e.mcp ? `, for the ${e.mcp} MCP server` : ""}`}>
              {e.to}:{e.ports}
            </span>
            <span className="sb-dim sb-clip">{e.who}</span>
            <span className="sb-tags">
              {e.dns && <i title="Its name resolves">dns</i>}
              {e.mcp && <i title={`For the ${e.mcp} MCP server`}>mcp</i>}
            </span>
            <span className="sb-acts">
              <button type="button" className="ms-go" aria-label={`Revoke ${e.to}`} title="Revoke" onClick={() => sandboxApi.grant(agentId, e, true).then(done, fail("Couldn't revoke"))}>
                <X size={14} />
              </button>
            </span>
          </li>
        ))}
        <li className="sb-deny">{net.egress.length === 0 ? "nothing granted" : "nothing else"}</li>
        {adding === "egress" && (
          <GrantForm
            d={d}
            agentId={agentId}
            done={(n) => {
              done(n);
              setAdding(null);
            }}
            fail={fail}
            onCancel={() => setAdding(null)}
          />
        )}
      </ul>
      <div className="sb-sub">
        <span className="sb-k">Networks</span>
      </div>
      <ul className="sb-rows">
        {net.networks.map((n) => (
          <li key={n.name} className="sb-row sb-net">
            <span className="sb-mono sb-clip" title={n.name}>
              {n.name}
            </span>
            <span className="sb-members">
              {n.members.map((m) => (
                <button
                  key={m.name}
                  type="button"
                  className="sb-member"
                  aria-pressed={m.connected}
                  aria-label={`${m.connected ? "Disconnect" : "Connect"} ${m.kind} ${m.name}`}
                  title={`${m.connected ? "Disconnect" : "Connect"} ${m.kind} ${m.name} (${n.name})`}
                  onClick={() => void toggle(n.name, m.name, !m.connected)}
                >
                  <i aria-hidden="true" />
                  {m.name}
                </button>
              ))}
            </span>
          </li>
        ))}
        {net.networks.length === 0 && <li className="sb-deny">no networks</li>}
      </ul>
    </Fold>
  );
}

function MemberSelect({ members, value, onChange, label }: { members: SandboxMember[]; value: string; onChange: (v: string) => void; label: string }) {
  return (
    <select className="sb-in" value={value} aria-label={label} onChange={(e) => onChange(e.target.value)}>
      {members.map((m) => (
        <option key={m.name} value={m.name}>
          {m.name}
        </option>
      ))}
    </select>
  );
}

function PublishForm({ d, agentId, done, onCancel }: { d: SandboxDetail; onCancel: () => void } & Acts) {
  const [host, setHost] = React.useState("127.0.0.1:8080");
  const [guest, setGuest] = React.useState("8080");
  const [proto, setProto] = React.useState<"tcp" | "udp">("tcp");
  const [to, setTo] = React.useState(d.members.find((m) => m.kind === "agent")?.name ?? d.members[0]?.name ?? "");
  const [why, setWhy] = React.useState<string | null>(null);
  return (
    <li className="sb-form" aria-label="Publish a port">
      <input className="sb-in sb-addr" value={host} aria-label="Runner address and port" spellCheck={false} onChange={(e) => setHost(e.target.value)} />
      <ArrowRight size={12} aria-hidden="true" />
      <input className="sb-in sb-port" value={guest} inputMode="numeric" aria-label="Guest port" onChange={(e) => setGuest(e.target.value)} />
      <Segmented
        size="xs"
        label="Protocol"
        value={proto}
        onChange={setProto}
        options={[
          { value: "tcp", label: "tcp" },
          { value: "udp", label: "udp" },
        ]}
      />
      <MemberSelect members={d.members} value={to} onChange={setTo} label="To" />
      <Button size="xs" onClick={() => sandboxApi.publish(agentId, { host, guest: Number(guest), proto, to }).then(done, (e: unknown) => setWhy(errorMessage(e)))}>
        Publish
      </Button>
      <button type="button" className="ms-go" aria-label="Cancel" onClick={onCancel}>
        <X size={14} />
      </button>
      {why && <span className="sb-why">{why}</span>}
    </li>
  );
}

function GrantForm({ d, agentId, done, onCancel }: { d: SandboxDetail; onCancel: () => void } & Acts) {
  const [to, setTo] = React.useState("");
  const [ports, setPorts] = React.useState("443");
  const [who, setWho] = React.useState(d.members.find((m) => m.kind === "agent")?.name ?? d.members[0]?.name ?? "");
  const [dns, setDns] = React.useState(true);
  const [why, setWhy] = React.useState<string | null>(null);
  const mcp = d.members.find((m) => m.name === who && m.kind === "mcp")?.name ?? null;
  return (
    <li className="sb-form" aria-label="Grant egress">
      <input className="sb-in sb-grow" value={to} placeholder="host" aria-label="Destination host" spellCheck={false} onChange={(e) => setTo(e.target.value)} />
      <input className="sb-in sb-port" value={ports} aria-label="Ports" spellCheck={false} onChange={(e) => setPorts(e.target.value)} />
      <MemberSelect members={d.members} value={who} onChange={setWho} label="For" />
      <label className="sb-check">
        <input type="checkbox" checked={dns} onChange={(e) => setDns(e.target.checked)} />
        dns
      </label>
      <Button size="xs" disabled={!to.trim()} onClick={() => sandboxApi.grant(agentId, { to: to.trim(), ports: ports.trim(), dns, mcp, who }).then(done, (e: unknown) => setWhy(errorMessage(e)))}>
        Grant
      </Button>
      <button type="button" className="ms-go" aria-label="Cancel" onClick={onCancel}>
        <X size={14} />
      </button>
      {why && <span className="sb-why">{why}</span>}
    </li>
  );
}

/** Its volumes on either engine: mounted into the microVM, or added to the host's allowed directories; removed when confirmed. */
function Volumes({ d, agentId, done, fail }: { d: SandboxDetail } & Acts) {
  const [adding, setAdding] = React.useState(false);
  const [asking, setAsking] = React.useState<string | null>(null);
  const host = d.engine === "host";
  return (
    <Fold
      id="sb-volumes"
      title="Volumes"
      count={d.volumes.length}
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
        {d.volumes.map((v) => (
          <li key={v.path} className="sb-row sb-vol">
            <FolderOpen size={12} className="sb-dim" aria-hidden="true" />
            <span className="sb-two">
              <span className="sb-two-top">
                <span className="sb-mono sb-clip" title={v.path}>
                  {v.path}
                </span>
                <span className="sb-mode" data-mode={v.mode}>
                  {v.mode}
                </span>
              </span>
              <span className="sb-two-sub">
                <span className="sb-dim sb-clip" title={v.members.join(", ")}>
                  {v.members.join(", ")}
                </span>
              </span>
            </span>
            <span className="sb-acts">
              {asking === v.path ? (
                <span className="sb-confirm" role="group" aria-label={`Remove ${v.path}?`}>
                  <span>Remove?</span>
                  <button
                    type="button"
                    className="ms-go"
                    aria-label={`Remove ${v.path}`}
                    onClick={() => {
                      setAsking(null);
                      sandboxApi.volume(agentId, v, true).then(done, fail("Couldn't remove it"));
                    }}
                  >
                    <Check size={14} />
                  </button>
                  <button type="button" className="ms-go" aria-label="Keep it" onClick={() => setAsking(null)}>
                    <X size={14} />
                  </button>
                </span>
              ) : (
                <button type="button" className="ms-go" aria-label={`Remove ${v.path}`} title="Remove" onClick={() => setAsking(v.path)}>
                  <X size={14} />
                </button>
              )}
            </span>
          </li>
        ))}
        {adding && (
          <VolumeForm
            d={d}
            agentId={agentId}
            done={(n) => {
              done(n);
              setAdding(false);
            }}
            fail={fail}
            onCancel={() => setAdding(false)}
          />
        )}
      </ul>
    </Fold>
  );
}

function VolumeForm({ d, agentId, done, onCancel }: { d: SandboxDetail; onCancel: () => void } & Acts) {
  const people = d.members.filter((m) => m.kind !== "mcp");
  const [path, setPath] = React.useState("");
  const [mode, setMode] = React.useState<"ro" | "rw">("ro");
  const [members, setMembers] = React.useState<string[]>(people.map((m) => m.name));
  const [why, setWhy] = React.useState<string | null>(null);
  return (
    <li className="sb-form" aria-label="Add a volume">
      <input className="sb-in sb-grow" value={path} placeholder="/path" aria-label="Path" spellCheck={false} onChange={(e) => setPath(e.target.value)} />
      <Segmented
        size="xs"
        label="Access"
        value={mode}
        onChange={setMode}
        options={[
          { value: "ro", label: "ro" },
          { value: "rw", label: "rw" },
        ]}
      />
      <span className="sb-members">
        {people.map((m) => (
          <button
            key={m.name}
            type="button"
            className="sb-member"
            aria-pressed={members.includes(m.name)}
            onClick={() => setMembers(members.includes(m.name) ? members.filter((x) => x !== m.name) : [...members, m.name])}
          >
            <i aria-hidden="true" />
            {m.name}
          </button>
        ))}
      </span>
      <Button
        size="xs"
        disabled={!path.trim() || members.length === 0}
        onClick={() => sandboxApi.volume(agentId, { path: path.trim(), mode, members }).then(done, (e: unknown) => setWhy(errorMessage(e)))}
      >
        Add
      </Button>
      <button type="button" className="ms-go" aria-label="Cancel" onClick={onCancel}>
        <X size={14} />
      </button>
      {why && <span className="sb-why">{why}</span>}
    </li>
  );
}

/** What each member may touch (its allowed directories), and the session's scope. */
function Reach({ d }: { d: SandboxDetail }) {
  const scope = d.scope!;
  return (
    <Fold id="sb-reach" title="Directories" count={d.dirs.length}>
      <ul className="sb-list">
        {d.dirs.map((m) => (
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
        <Kv k="Scope" title={`${scope.project} · ${scope.branch} · ${scope.paths.join(", ")}`}>
          <span>{scope.project}</span>
          <span className="sb-mono sb-dim">{scope.branch}</span>
          <span className="sb-dim">
            {scope.paths.length} path{scope.paths.length === 1 ? "" : "s"}
          </span>
        </Kv>
      </ul>
    </Fold>
  );
}

/** The last hour, full width: CPU, memory and network as plots. */
function Plots({ d }: { d: SandboxDetail }) {
  const s = d.series;
  const rows: [string, number[], number | undefined, string][] = [
    ["CPU", s.cpu, 100, `${s.cpu.at(-1) ?? 0}%`],
    ["Memory", s.memory, 100, `${s.memory.at(-1) ?? 0}%`],
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

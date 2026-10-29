/**
 * Clusters (docs/peers.md): the kiln clusters this one is peered with. A peering is authorized by
 * hand, with codes: a challenge made here, taken to the other cluster, whose admin authorizes it
 * (picking what our people may do there) and gives back a response, pasted here. Nothing secret
 * crosses, and the other cluster never has to reach this one. Once peered, it opens from the
 * cluster picker in the header, through every runner of this cluster. Letting the other cluster's
 * people in here is the same steps the other way: "Let a cluster in" with its challenge.
 */
import * as React from "react";
import { ApiError } from "@/api/client";
import { peersApi, type Peer, type PeerState } from "@/api/peers";
import { Button } from "@/components/ui/button";
import { CopyBlock } from "@/components/ui/copy-block";
import { Loadable } from "@/components/ui/loading";
import { Confirm, Facts, PageIntro, Panel, SelectField, TextField } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { openCluster, openRes } from "@/components/shell/clusters";
import { useRes, warm, type Res } from "@/lib/data";
import { invalidate } from "@/lib/resource";
import "./clusters.css";

const peersRes: Res<Awaited<ReturnType<typeof peersApi.list>>> = { key: "peers", fetch: (s) => peersApi.list(s), opts: { on: ["peers"] } };

const STATE: Record<PeerState, string> = { peered: "Peered", waiting: "Waiting for its response", expired: "Challenge expired" };
const ROLE: Record<string, string> = {
  viewer: "Look (viewer)",
  developer: "Run and review (developer)",
  maintainer: "Land and deploy (maintainer)",
  admin: "Everything (admin)",
};

const problem = (e: unknown) => (e instanceof ApiError ? e.message : String(e));

/** The page and the picker read peerings again after a change. */
function changed() {
  invalidate("peers");
  invalidate(openRes.key);
}

export default function Clusters() {
  warm(peersRes);
  return (
    <div className="container-console page max-w-[880px]">
      <title>Clusters · kiln</title>
      <PageIntro
        index={10}
        label="Clusters"
        title="Clusters"
        lead="Other kiln clusters, opened through this one from the picker in the header, from every runner of this cluster. Each peering is authorized by hand with codes, so the other cluster never has to reach this one."
      />
      <Loadable label="the clusters">
        <Body />
      </Loadable>
    </div>
  );
}

function Body() {
  const d = useRes(peersRes).data;
  const out = d.peers.filter((p) => p.direction === "out");
  const inbound = d.peers.filter((p) => p.direction === "in");
  return (
    <div className="space-y-4">
      <Panel eyebrow="This cluster" title={d.name} description="Its key, as the other cluster's admin sees it in the codes this one makes: compare them.">
        <Facts items={[{ label: "Key", value: <code className="cl-fp">{d.fingerprint}</code> }]} />
      </Panel>
      <Panel eyebrow="Opened from here" title="Peered clusters" description={out.length ? "Open one from the cluster picker in the header, or here." : "None yet: peer with one below."}>
        {out.length > 0 && (
          <ul className="cl-list">
            {out.map((p) => (
              <OutRow key={p.id} p={p} />
            ))}
          </ul>
        )}
      </Panel>
      <Register />
      {inbound.length > 0 && (
        <Panel eyebrow="Let in here" title="Clusters that open this one" description="Their people come in with at most the role given here, and with no more than they have at home.">
          <ul className="cl-list">
            {inbound.map((p) => (
              <InRow key={p.id} p={p} />
            ))}
          </ul>
        </Panel>
      )}
      <Authorize roles={d.roles} />
    </div>
  );
}

/** A code to copy, and what to do with it. */
function Code({ label, text, note }: { label: string; text: string; note: React.ReactNode }) {
  return (
    <div className="cl-made">
      <div className="cl-made-head">
        <span className="field-label">{label}</span>
        <CopyBlock text={text} />
      </div>
      <pre className="cl-code" tabIndex={0}>
        {text}
      </pre>
      <p className="cl-note">{note}</p>
    </div>
  );
}

/** A code to paste. */
function CodeField({ label, value, onChange, placeholder }: { label: string; value: string; onChange: (v: string) => void; placeholder: string }) {
  const id = React.useId();
  return (
    <div>
      <label htmlFor={id} className="field-label">
        {label}
      </label>
      <textarea id={id} className="cl-code cl-paste" value={value} placeholder={placeholder} spellCheck={false} autoComplete="off" rows={3} onChange={(e) => onChange(e.target.value)} />
    </div>
  );
}

/** Asks before removing a peering. */
function Removal({ p, question, onDone }: { p: Peer; question: string; onDone: () => void }) {
  const [busy, setBusy] = React.useState(false);
  const remove = async () => {
    setBusy(true);
    try {
      await peersApi.remove(p.id);
      toast({ title: `${p.name} removed`, tone: "info" });
      changed();
    } catch (e) {
      toast.error(`Couldn't remove ${p.name}`, problem(e));
    } finally {
      setBusy(false);
      onDone();
    }
  };
  return (
    <Confirm
      className="mt-3"
      label={`Remove ${p.name}`}
      tone="danger"
      actions={
        <>
          <Button size="xs" variant="danger" disabled={busy} onClick={() => void remove()}>
            Remove
          </Button>
          <Button size="xs" variant="ghost" onClick={onDone}>
            Cancel
          </Button>
        </>
      }
    >
      <p>{question}</p>
    </Confirm>
  );
}

/** A cluster our people open: open it, or finish peering, or start again. */
function OutRow({ p }: { p: Peer }) {
  const [removing, setRemoving] = React.useState(false);
  const [moving, setMoving] = React.useState(false);
  const [challenge, setChallenge] = React.useState<string | null>(null);
  const [busy, setBusy] = React.useState(false);
  const rechallenge = async () => {
    setBusy(true);
    try {
      setChallenge((await peersApi.rechallenge(p.id)).challenge);
      changed();
    } catch (e) {
      toast.error("Couldn't make a new challenge", problem(e));
    } finally {
      setBusy(false);
    }
  };
  const peered = p.state === "peered";
  return (
    <li className="cl-row">
      <div className="cl-head">
        <span className="cl-name">{p.name}</span>
        <span className="cl-state" data-state={p.state}>
          {STATE[p.state]}
        </span>
        <span className="flex-1" />
        {peered ? (
          <Button size="xs" variant="secondary" onClick={() => void openCluster(p.id, p.name)}>
            Open
          </Button>
        ) : (
          <Button size="xs" variant="ghost" disabled={busy} onClick={() => void rechallenge()}>
            New challenge
          </Button>
        )}
        <Button size="xs" variant="ghost" aria-expanded={moving} onClick={() => setMoving(!moving)}>
          Change address
        </Button>
        <Button size="xs" variant="ghost" aria-expanded={removing} onClick={() => setRemoving(!removing)}>
          Remove
        </Button>
      </div>
      <div className="cl-meta">
        {p.url}
        {peered && p.role ? ` · ${ROLE[p.role] ?? p.role} there` : ""}
        {p.fingerprint ? (
          <>
            {" · key "}
            <code className="cl-fp">{p.fingerprint}</code>
          </>
        ) : null}
      </div>
      {moving && <Readdress p={p} onDone={() => setMoving(false)} />}
      {removing && <Removal p={p} question={`Stop opening ${p.name} from here? To open it again takes new codes.`} onDone={() => setRemoving(false)} />}
      {challenge && <Code label="Its new challenge" text={challenge} note={<>Take it to {p.name}: its admin pastes it under Clusters → Let a cluster in, within 30 minutes. The one before no longer works.</>} />}
      {!peered && <Complete p={p} />}
    </li>
  );
}

/** Where a cluster is reached from here, changed: its key is what's trusted, so no new codes. */
function Readdress({ p, onDone }: { p: Peer; onDone: () => void }) {
  const [url, setUrl] = React.useState(p.url);
  const [busy, setBusy] = React.useState(false);
  const [err, setErr] = React.useState<string | null>(null);
  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setErr(null);
    try {
      await peersApi.readdress(p.id, url.trim());
      toast({ title: `${p.name}'s address changed`, description: "Open it again from the cluster picker.", tone: "info" });
      changed();
      onDone();
    } catch (e2) {
      setErr(problem(e2));
    } finally {
      setBusy(false);
    }
  };
  return (
    <form className="cl-form grid gap-3 sm:grid-cols-[1fr_auto] sm:items-end" onSubmit={(e) => void submit(e)}>
      <TextField label="Its address, as this cluster reaches it" value={url} inputMode="url" onChange={(e) => setUrl(e.target.value)} />
      <div className="flex gap-2">
        <Button type="submit" size="sm" variant="secondary" disabled={busy || !url.trim() || url.trim() === p.url}>
          Save
        </Button>
        <Button type="button" size="sm" variant="ghost" onClick={onDone}>
          Cancel
        </Button>
      </div>
      {err && <p className="cl-err sm:col-span-2">{err}</p>}
    </form>
  );
}

/** Paste the other cluster's response: checked against our challenge, it peers them. */
function Complete({ p }: { p: Peer }) {
  const [text, setText] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const [err, setErr] = React.useState<string | null>(null);
  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setErr(null);
    try {
      await peersApi.complete(p.id, text.trim());
      toast({ title: `Peered with ${p.name}`, description: "Open it from the cluster picker in the header.", tone: "succeeded" });
      changed();
    } catch (e2) {
      setErr(problem(e2));
    } finally {
      setBusy(false);
    }
  };
  return (
    <form className="cl-form" onSubmit={(e) => void submit(e)}>
      <CodeField label={`${p.name}'s response`} value={text} onChange={setText} placeholder="kiln_pr1_…" />
      <div className="cl-actions">
        <Button type="submit" size="sm" variant="secondary" disabled={busy || !text.trim()}>
          Peer
        </Button>
        {err && <span className="cl-err">{err}</span>}
      </div>
    </form>
  );
}

/** Name a cluster and its address: the challenge to take to it. */
function Register() {
  const [name, setName] = React.useState("");
  const [url, setUrl] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const [err, setErr] = React.useState<string | null>(null);
  const [made, setMade] = React.useState<{ peer: Peer; challenge: string } | null>(null);
  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setErr(null);
    try {
      setMade(await peersApi.register(name.trim(), url.trim()));
      changed();
    } catch (e2) {
      setErr(problem(e2));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Panel eyebrow="Peer with a cluster" title="Open another cluster from here" description="Its name, and its address as this cluster reaches it: its front door, or its server's address.">
      {made ? (
        <>
          <Code
            label="The challenge"
            text={made.challenge}
            note={
              <>
                Take it to {made.peer.name}: its admin pastes it under Clusters → Let a cluster in, within 30 minutes, and gives you a response to paste under {made.peer.name}{" "}
                above.
              </>
            }
          />
          <Button
            size="xs"
            variant="ghost"
            onClick={() => {
              setMade(null);
              setName("");
              setUrl("");
            }}
          >
            Peer with another
          </Button>
        </>
      ) : (
        <form className="grid gap-3 sm:grid-cols-[1fr_1.4fr_auto] sm:items-end" onSubmit={(e) => void submit(e)}>
          <TextField label="Name" value={name} placeholder="work" onChange={(e) => setName(e.target.value)} />
          <TextField label="Its address" value={url} placeholder="https://kiln.work" inputMode="url" onChange={(e) => setUrl(e.target.value)} />
          <Button type="submit" variant="secondary" disabled={busy || !name.trim() || !url.trim()}>
            Make a challenge
          </Button>
        </form>
      )}
      {err && <p className="cl-err mt-3">{err}</p>}
    </Panel>
  );
}

/** A cluster whose people open this one. */
function InRow({ p }: { p: Peer }) {
  const [removing, setRemoving] = React.useState(false);
  return (
    <li className="cl-row">
      <div className="cl-head">
        <span className="cl-name">{p.name}</span>
        <span className="cl-state" data-state="peered">
          {ROLE[p.role] ?? p.role}
        </span>
        <span className="flex-1" />
        <Button size="xs" variant="ghost" aria-expanded={removing} onClick={() => setRemoving(!removing)}>
          Remove
        </Button>
      </div>
      <div className="cl-meta">
        {p.url || "its address wasn't said"}
        {p.fingerprint ? (
          <>
            {" · key "}
            <code className="cl-fp">{p.fingerprint}</code>
          </>
        ) : null}
      </div>
      {removing && <Removal p={p} question={`Stop letting ${p.name}'s people in? What they have open here stops working.`} onDone={() => setRemoving(false)} />}
    </li>
  );
}

/** Paste another cluster's challenge and pick what its people may do: the response to give back. */
function Authorize({ roles }: { roles: string[] }) {
  const [code, setCode] = React.useState("");
  const [role, setRole] = React.useState("developer");
  const [busy, setBusy] = React.useState(false);
  const [err, setErr] = React.useState<string | null>(null);
  const [made, setMade] = React.useState<{ peer: Peer; response: string } | null>(null);
  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setErr(null);
    try {
      setMade(await peersApi.authorize(code.trim(), role));
      changed();
    } catch (e2) {
      setErr(problem(e2));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Panel eyebrow="Let a cluster in" title="Let another cluster open this one" description="Paste the challenge its admin made, pick the most its people may do here, and give back the response.">
      {made ? (
        <>
          <p className="cl-ok">
            {made.peer.name} may open this cluster, its people as {ROLE[made.peer.role] ?? made.peer.role} at most. Its key is <code className="cl-fp">{made.peer.fingerprint}</code>:
            compare it with what its admin sees.
          </p>
          <Code label="The response" text={made.response} note={<>Take it back to {made.peer.name}: its admin pastes it under Clusters, at the cluster it made the challenge for.</>} />
          <Button
            size="xs"
            variant="ghost"
            onClick={() => {
              setMade(null);
              setCode("");
            }}
          >
            Let another in
          </Button>
        </>
      ) : (
        <form className="space-y-3" onSubmit={(e) => void submit(e)}>
          <CodeField label="Its challenge" value={code} onChange={setCode} placeholder="kiln_pc1_…" />
          <div className="grid gap-3 sm:grid-cols-[260px_auto] sm:items-end sm:justify-start">
            <SelectField label="Its people may" value={role} onChange={setRole} options={roles.map((r) => ({ value: r, label: ROLE[r] ?? r }))} />
            <Button type="submit" variant="secondary" disabled={busy || !code.trim()}>
              Let it in
            </Button>
          </div>
        </form>
      )}
      {err && <p className="cl-err mt-3">{err}</p>}
    </Panel>
  );
}

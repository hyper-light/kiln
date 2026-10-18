/** Add a machine: the one-liner for its system, what it does, and a live line that says when the machine is here. */
import * as React from "react";
import { Link } from "@/lib/router";
import { ArrowDown, ArrowRight, ArrowUpRight, Check, ChevronRight, Copy, RotateCcw } from "lucide-react";
import type { InstallOs, JoinInfo, JoinRequest } from "@/api/types";
import { Button } from "@/components/ui/button";
import { SectionHeading } from "@/components/ui/misc";
import { relativeTime } from "@/lib/format";
import { useNow } from "@/lib/hooks";
import type { SuspenseResource } from "@/lib/resource";
import { Optic } from "@/views/harness/signals";
import { installCommand, jumpTo, listWords, type Arrival } from "./data";
import { CodeLine, OsWords, Wrapped, useCopy } from "./parts";

const SERVICE: Record<InstallOs, string> = {
  linux: "a service (systemd)",
  macos: "a service (launchd)",
  windows: "a scheduled task (no admin needed)",
};

const K8S_GUIDE = "https://github.com/hyper-light/kiln/blob/main/deploy/README.md#mode-2--kubernetes";

export function InstallSection({
  index,
  os,
  onOs,
  info,
  arrived,
  asking,
  rowOf,
}: {
  index: string;
  os: InstallOs;
  onOs: (os: InstallOs) => void;
  info: SuspenseResource<JoinInfo>;
  /** Runners that connected since the page opened. */
  arrived: Arrival[];
  /** Join requests that arrived since the page opened. */
  asking: JoinRequest[];
  /** The id of the row a join request is shown in. */
  rowOf: (r: JoinRequest) => string;
}) {
  const data = info.data;
  const command = installCommand(data, os);
  const [copied, copy] = useCopy();

  return (
    <section className="am-section am-install" aria-labelledby="am-add">
      <SectionHeading index={index} label="One command" title={<span id="am-add">Add a machine</span>} />
      <div className="am-cmd">
        <div className="am-cmd-bar">
          <OsWords value={os} onChange={onOs} label="The machine's system" />
          <Button size="xs" className="shrink-0" disabled={!command} onClick={() => copy(command)}>
            {copied ? <Check aria-hidden="true" /> : <Copy aria-hidden="true" />}
            <span aria-live="polite">{copied ? "Copied" : "Copy"}</span>
          </Button>
        </div>
        <pre className="am-cmd-text">
          <span className="am-prompt" aria-hidden="true">
            {os === "windows" ? "PS> " : "$ "}
          </span>
          <Wrapped text={command} />
        </pre>
      </div>
      <p className="am-note">
        {os === "windows" ? "In PowerShell on the machine: downloads" : "On the machine: downloads"} kiln from this server, installs it as {SERVICE[os]} and joins as a runner.
        <Expiry at={data.expires_at} onRenew={info.reload} />
      </p>
      <Status arrived={arrived} asking={asking} rowOf={rowOf} />
      <Alternatives info={data} os={os} />
    </section>
  );
}

/** Other ways in, as two disclosures sharing one line (one open at a time): containers, and no token. */
function Alternatives({ info, os }: { info: JoinInfo | undefined; os: InstallOs }) {
  const [open, setOpen] = React.useState<"k8s" | "ask" | null>(null);
  const id = React.useId();
  const ask = info ? [os === "windows" ? info.join_windows : info.join, info.join_manual].filter((c): c is string => Boolean(c)) : [];
  const trigger = (key: "k8s" | "ask", label: string) => (
    <button type="button" className="am-fold" aria-expanded={open === key} aria-controls={`${id}-${key}`} onClick={() => setOpen(open === key ? null : key)}>
      <ChevronRight size={13} aria-hidden="true" />
      {label}
    </button>
  );
  return (
    <div className="am-alts">
      <div className="am-alts-line">
        {trigger("k8s", "Kubernetes or Docker instead?")}
        {ask.length > 0 && trigger("ask", "Without the token?")}
      </div>
      <div id={`${id}-k8s`} hidden={open !== "k8s"} className="am-fold-body">
        {open === "k8s" && (
          <div className="am-alt">
            <div>
              <h3 className="am-h3">Kubernetes</h3>
              <p>
                No machine to add: with <code>[kubernetes] enabled = true</code> in the server&apos;s config, each job that <code>runs-on: [k8s]</code> gets a fresh pod in{" "}
                <code>kiln-jobs</code>.{" "}
                <a className="am-act am-inline-act" href={K8S_GUIDE} target="_blank" rel="noreferrer">
                  Kubernetes guide <ArrowUpRight aria-hidden="true" />
                </a>
              </p>
            </div>
            {info && (
              <div>
                <h3 className="am-h3">Docker</h3>
                <p>
                  A runner in a container, from the image the kiln repository builds (<code>docker build -t kiln:dev .</code>):
                </p>
                <CodeLine
                  command={`docker run -d --name kiln-runner --restart unless-stopped -v kiln-runner:/var/lib/kiln kiln:dev agent --server ${info.server} --token ${info.token} --labels docker`}
                  copyName="Copy the docker command"
                />
              </div>
            )}
          </div>
        )}
      </div>
      <div id={`${id}-ask`} hidden={open !== "ask"} className="am-fold-body">
        {open === "ask" && (
          <div className="am-alt">
            <p>The machine asks to join instead, and shows a code: approve it on this page when the codes match.</p>
            {ask.map((c) => (
              <CodeLine key={c} command={c} prompt={os === "windows" ? "PS>" : "$"} copyName="Copy the command" />
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

/** " Its token expires in 23h." — live, and a way to a fresh command once it has expired. */
function Expiry({ at, onRenew }: { at: number | null | undefined; onRenew: () => void }) {
  const now = useNow();
  if (!at) return null;
  if (at * 1000 <= now) {
    return (
      <>
        {" "}
        <span className="am-warn">Its token has expired.</span>{" "}
        <button type="button" className="am-act am-inline-act" onClick={onRenew}>
          <RotateCcw aria-hidden="true" /> New command
        </button>
      </>
    );
  }
  return (
    <>
      {" "}
      Its token expires{" "}
      <time dateTime={new Date(at * 1000).toISOString()} title={new Date(at * 1000).toLocaleString()}>
        {relativeTime(at, now)}
      </time>
      .
    </>
  );
}

/** Waiting → someone asking to join → connected: whichever happened last. Announced as it changes. */
function Status({ arrived, asking, rowOf }: { arrived: Arrival[]; asking: JoinRequest[]; rowOf: (r: JoinRequest) => string }) {
  const lastArrival = arrived[arrived.length - 1];
  const ask = asking[asking.length - 1];
  const last = lastArrival && !(ask && ask.created_at > lastArrival.at) ? lastArrival.agent : undefined;
  return (
    <div className="am-status" role="status" data-state={last ? "connected" : ask ? "asking" : "waiting"}>
      {last ? (
        <>
          <Check size={15} className="am-status-ok" aria-hidden="true" />
          <span>
            <b>{listWords(arrived.map((a) => a.agent.name))}</b> connected
          </span>
          <Link className="am-act" to={`/runners?agent=${encodeURIComponent(last.name)}`}>
            Open {arrived.length > 1 ? last.name : "it"} <ArrowRight aria-hidden="true" />
          </Link>
        </>
      ) : ask ? (
        <>
          <span className="am-status-glyph" aria-hidden="true">
            <Optic state="waiting" size={16} />
          </span>
          <span>
            <b>{ask.name}</b> is asking to join
          </span>
          <a
            className="am-act"
            href={`#${rowOf(ask)}`}
            onClick={(e) => {
              e.preventDefault();
              jumpTo(rowOf(ask));
            }}
          >
            Check its code <ArrowDown aria-hidden="true" />
          </a>
        </>
      ) : (
        <>
          <span className="am-status-glyph" aria-hidden="true">
            <Optic state="waiting" size={16} />
          </span>
          <span>Waiting for a machine to connect…</span>
        </>
      )}
    </div>
  );
}

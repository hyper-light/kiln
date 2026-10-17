/** Discovery health: how each way of finding machines is doing, what is wrong and the fix for your system, and DNS-SD. */
import { AlertTriangle } from "lucide-react";
import type { DiscoveryFix, DiscoveryInfo, DiscoveryMethod, DnsSdInfo, InstallOs } from "@/api/types";
import { SectionHeading } from "@/components/ui/misc";
import { RelTime } from "@/views/shared";
import { OS_NAME } from "./data";
import { CodeLine, CopyText, Fold, Wrapped } from "./parts";

const STATE_WORD: Record<DiscoveryMethod["state"], string> = { active: "active", off: "off", unavailable: "unavailable", error: "error" };

export function HealthSection({ index, discovery: d, os }: { index: string; discovery: DiscoveryInfo; os: InstallOs }) {
  const methods = d.methods ?? [];
  const problems = d.problems ?? [];
  const active = methods.filter((m) => m.state === "active").length;
  return (
    <section className="am-section" id="am-health" tabIndex={-1} aria-labelledby="am-health-title">
      <SectionHeading
        index={index}
        label="Discovery"
        title={<span id="am-health-title">Discovery health</span>}
        aside={
          methods.length > 0 && (
            <span className="am-aside">
              {active} of {methods.length} active
              {problems.length > 0 && (
                <span className="am-aside-warn">
                  {" "}
                  · {problems.length} problem{problems.length === 1 ? "" : "s"}
                </span>
              )}
            </span>
          )
        }
      />
      {methods.length > 0 && (
        <ul className="am-methods" aria-label="Ways of finding machines">
          {methods.map((m) => (
            <li key={`${m.id}:${m.label}`} className="am-method" data-state={m.state}>
              <span className="am-method-label">
                <i className="am-dot" data-state={m.state} role="img" aria-label={STATE_WORD[m.state]} title={STATE_WORD[m.state]} />
                {m.label}
              </span>
              <span className="am-method-detail">{m.detail}</span>
              <span className="am-method-count">
                {m.state === "active" || m.heard > 0 ? (
                  <>
                    {m.heard} heard
                    {m.last_heard ? (
                      <>
                        {" "}
                        · <RelTime ts={m.last_heard} />
                      </>
                    ) : null}
                  </>
                ) : (
                  STATE_WORD[m.state]
                )}
              </span>
            </li>
          ))}
        </ul>
      )}

      {problems.length > 0 && (
        <div className="am-block" id="am-problems" tabIndex={-1}>
          <h3 className="am-h3">
            {problems.length === 1 ? "A problem" : `${problems.length} problems`}
          </h3>
          <ul className="am-problems">
            {problems.map((p) => (
              <li key={p.id} className="am-problem">
                <AlertTriangle size={14} aria-hidden="true" />
                <div className="min-w-0">
                  <p className="am-problem-text">{p.text}</p>
                  <Fixes fixes={p.fixes} os={os} />
                </div>
              </li>
            ))}
          </ul>
        </div>
      )}

      {d.dns_sd && <DnsSd info={d.dns_sd} />}
    </section>
  );
}

/** Fixes for your system (and those for any system) first; the other systems' behind a fold. */
function Fixes({ fixes, os }: { fixes: DiscoveryFix[]; os: InstallOs }) {
  const mine = fixes.filter((f) => f.os === os);
  const any = fixes.filter((f) => !f.os);
  const others = fixes.filter((f) => f.os && f.os !== os);
  // With nothing for this system, every system's fix is shown: hiding them all would leave nothing.
  const shown = mine.length ? [...mine, ...any] : [...others, ...any];
  const folded = mine.length ? others : [];
  const names = [...new Set(folded.map((f) => OS_NAME[f.os!]))];
  return (
    <>
      {shown.map((f, i) => (
        <Fix key={i} fix={f} />
      ))}
      {folded.length > 0 && (
        <Fold className="am-fixes-fold" label={`Other systems (${names.join(", ")})`}>
          {folded.map((f, i) => (
            <Fix key={i} fix={f} />
          ))}
        </Fold>
      )}
    </>
  );
}

function Fix({ fix: f }: { fix: DiscoveryFix }) {
  return (
    <div className="am-fix">
      {(f.text || f.os) && (
        <p className="am-fix-text">
          {f.os && <span className="am-tag am-fix-os">{OS_NAME[f.os]}</span>}
          {f.text}
        </p>
      )}
      {f.command && <CodeLine command={f.command} prompt={f.os === "windows" ? "PS>" : "$"} copyName="Copy the command" />}
    </div>
  );
}

function DnsSd({ info }: { info: DnsSdInfo }) {
  const p = info.publish;
  return (
    <div className="am-block" aria-labelledby="am-dnssd">
      <h3 className="am-h3" id="am-dnssd">
        DNS-SD
      </h3>
      <dl className="am-dl">
        <dt>Browsing</dt>
        <dd className="am-mono">{info.domains.length ? info.domains.join(" · ") : "no unicast domains"}</dd>
        <dt>Publishing</dt>
        <dd>
          {!p.configured ? (
            <span className="am-quiet">Not set up: this server isn&apos;t published to any zone.</span>
          ) : p.ok === false ? (
            <span className="am-publish" data-state="error">
              <i className="am-dot" data-state="error" aria-hidden="true" />
              <span>
                Failed to publish to <span className="am-mono">{p.zone}</span>
                {p.error ? `: ${p.error}` : ""}
                {p.at ? (
                  <>
                    {" "}
                    · <RelTime ts={p.at} />
                  </>
                ) : null}
              </span>
            </span>
          ) : p.ok ? (
            <span className="am-publish" data-state="active">
              <i className="am-dot" data-state="active" aria-hidden="true" />
              <span>
                Published to <span className="am-mono">{p.zone}</span>
                {p.at ? (
                  <>
                    {" "}
                    · <RelTime ts={p.at} />
                  </>
                ) : null}
              </span>
            </span>
          ) : (
            <span className="am-quiet">
              Publishing to <span className="am-mono">{p.zone}</span>…
            </span>
          )}
        </dd>
        {info.records.length > 0 && (
          <>
            <dt>Records</dt>
            <dd>
              <p className="am-records-note">
                Add these to your DNS to make this server discoverable across networks.{" "}
                <CopyText text={info.records.join("\n")} label="Copy all" name="Copy all the records" className="am-inline-act" />
              </p>
              <ul className="am-records" aria-label="Zone-file lines">
                {info.records.map((r, i) => (
                  <li key={i}>
                    <code>
                      <Wrapped text={r} />
                    </code>
                    <CopyText text={r} iconOnly name={`Copy line ${i + 1}`} />
                  </li>
                ))}
              </ul>
            </dd>
          </>
        )}
      </dl>
    </div>
  );
}

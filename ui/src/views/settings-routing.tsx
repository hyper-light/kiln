/**
 * Settings → CI → Routing (docs/ci.md W21-R11, W05-R21): the routing rules that send legs to
 * runners, in the order they apply, each with where it comes from; the server's edited in place
 * as an ordered list (a repository's are its trunk's `.kiln/routing.yaml`, changed by a diff); and
 * a tester that explains a sample leg against the runners connected now.
 */
import * as React from "react";
import { ci, type CiRouteExplain, type CiRoutingRule } from "@/api/ci";
import { Button } from "@/components/ui/button";
import { Hold } from "@/components/ui/hold";
import { SettingRow, SettingsGroup } from "@/components/ui/settings";
import { reposRes, useRes } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";

const list = (s: string, by: RegExp = /,/) =>
  s
    .split(by)
    .map((x) => x.trim())
    .filter(Boolean);

/** A rule in a line: what it matches, then what it adds. */
function describe(r: CiRoutingRule): { matches: string; adds: string } {
  const m = r.match ?? {};
  const matches =
    [m.labels?.length ? `labels ${m.labels.join(", ")}` : "", m.repos?.length ? `repositories ${m.repos.join(", ")}` : "", m.job ? `jobs where ${m.job}` : ""].filter(Boolean).join(" · ") ||
    "every leg";
  const adds = [r.require?.length ? r.require.join(" && ") : "", r.executor?.length ? `on ${r.executor.join(" or ")}` : "", r.image ? `image ${r.image}` : "", r.docker ? `docker ${r.docker}` : ""]
    .filter(Boolean)
    .join(" · ");
  return { matches, adds: adds || "nothing more" };
}

export function RoutingHold() {
  return (
    <SettingsGroup id="ci.routing" title="Routing" words="routing rules runners labels">
      <Hold height={120} />
    </SettingsGroup>
  );
}

export function RoutingGroups() {
  const res = useSuspenseResource("ci/routing/rules", (s) => ci.routingRules(s));
  const d = res.data;
  const server = d.rules.filter((r) => r.source === "server").map((r) => r.rule);
  return (
    <>
      <SettingsGroup
        id="ci.routing"
        title="Routing"
        about="The rules that send legs to runners, in the order they apply: each adds requirements, executors and an image to the legs it matches."
        words="routing rules runners labels requirements executors image"
      >
        <ul className="st-list" aria-label="Routing rules">
          {d.rules.map((r) => {
            const t = describe(r.rule);
            return (
              <li key={`${r.source}-${r.index}`}>
                <div className="st-item">
                  <span className="st-item-name text-[13px]">{t.matches}</span>
                  <span className="st-item-meta mono">{t.adds}</span>
                  <span className="st-item-meta ml-auto">{r.source}</span>
                </div>
              </li>
            );
          })}
        </ul>
        {d.rules.length === 0 && <p className="st-note">No rules: a leg goes to any runner that carries its labels.</p>}
        <p className="st-note">
          GitHub&rsquo;s hosted labels (ubuntu-latest and the rest){" "}
          {d.github_labels ? "map to kiln's runners by the built-in rules." : "route only to runners that carry them, until [ci.routing] github_labels is on."} A repository&rsquo;s own rules are its
          trunk&rsquo;s .kiln/routing.yaml.
        </p>
      </SettingsGroup>
      <ServerRules initial={server} onSaved={() => res.reload()} />
      <RoutingTester />
    </>
  );
}

function ServerRules({ initial, onSaved }: { initial: CiRoutingRule[]; onSaved: () => void }) {
  const [rules, setRules] = React.useState<CiRoutingRule[]>(initial);
  const [busy, setBusy] = React.useState(false);
  const [problem, setProblem] = React.useState<string | null>(null);
  const [saved, setSaved] = React.useState(false);
  // What the saved rules do now, as the server warns of it (W20-R03).
  const [warnings, setWarnings] = React.useState<string[]>([]);
  const change = (i: number, f: (r: CiRoutingRule) => CiRoutingRule) => {
    setSaved(false);
    setRules((rs) => rs.map((r, j) => (j === i ? f(r) : r)));
  };
  const move = (i: number, by: number) => {
    setSaved(false);
    setRules((rs) => {
      const next = [...rs];
      const [r] = next.splice(i, 1);
      if (r) next.splice(i + by, 0, r);
      return next;
    });
  };
  const save = async () => {
    setBusy(true);
    setProblem(null);
    try {
      const r = await ci.setRoutingRules("server", rules);
      setWarnings((r.diagnostics ?? []).map((d) => d.message));
      setSaved(true);
      onSaved();
    } catch (e) {
      setProblem(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <SettingsGroup
      id="ci.routing.server"
      title="The server's rules"
      about="Tried before the defaults, after a repository's and its organization's. Each field takes a comma-separated list; requirements take one a line."
      words="server rules edit add remove order"
      actions={
        <Button
          size="xs"
          variant="secondary"
          disabled={busy}
          onClick={() => {
            setSaved(false);
            setRules((rs) => [...rs, { match: { labels: [] }, require: [] }]);
          }}
        >
          Add a rule
        </Button>
      }
    >
      {rules.length === 0 && <p className="st-note">None of the server&rsquo;s own.</p>}
      <ol className="st-list" aria-label="The server's rules">
        {rules.map((r, i) => (
          <li key={i}>
            <fieldset className="st-fields" disabled={busy} aria-label={`Rule ${i + 1}`}>
              <RuleField label="Labels" placeholder="gpu" value={(r.match?.labels ?? []).join(", ")} onChange={(v) => change(i, (x) => ({ ...x, match: { ...x.match, labels: list(v) } }))} />
              <RuleField
                label="Repositories"
                placeholder="acme/* (any when empty)"
                value={(r.match?.repos ?? []).join(", ")}
                onChange={(v) => change(i, (x) => ({ ...x, match: { ...x.match, repos: list(v) } }))}
              />
              <RuleField label="Executors" placeholder="docker, host" value={(r.executor ?? []).join(", ")} onChange={(v) => change(i, (x) => ({ ...x, executor: list(v) }))} />
              <RuleField label="Image" placeholder="kiln/runner-ubuntu-24.04" value={r.image ?? ""} onChange={(v) => change(i, (x) => ({ ...x, image: v.trim() || null }))} />
              <div className="col-span-full">
                <label className="field-label" htmlFor={`st-rule-${i}-require`}>
                  Requirements
                </label>
                <div className="field">
                  <textarea
                    id={`st-rule-${i}-require`}
                    rows={2}
                    spellCheck={false}
                    className="font-mono text-[12px]"
                    placeholder="gpu.count >= 1"
                    value={(r.require ?? []).join("\n")}
                    onChange={(e) => change(i, (x) => ({ ...x, require: list(e.target.value, /\n/) }))}
                  />
                </div>
              </div>
            </fieldset>
            <div className="mt-2 flex flex-wrap gap-1">
              <Button size="xs" variant="ghost" disabled={busy || i === 0} onClick={() => move(i, -1)}>
                Earlier
              </Button>
              <Button size="xs" variant="ghost" disabled={busy || i === rules.length - 1} onClick={() => move(i, 1)}>
                Later
              </Button>
              <Button
                size="xs"
                variant="ghost"
                disabled={busy}
                onClick={() => {
                  setSaved(false);
                  setRules((rs) => rs.filter((_, j) => j !== i));
                }}
              >
                Remove
              </Button>
            </div>
          </li>
        ))}
      </ol>
      <div className="mt-3 flex items-center justify-end gap-3">
        {saved && (
          <span className="st-note mt-0" role="status">
            Saved.
          </span>
        )}
        <Button size="xs" variant="secondary" disabled={busy} onClick={() => void save()}>
          Save the rules
        </Button>
      </div>
      {saved &&
        warnings.map((w) => (
          <p key={w} className="st-note" data-tone="warn">
            {w}
          </p>
        ))}
      {problem && (
        <p className="st-note" data-tone="error" role="status">
          {problem}
        </p>
      )}
    </SettingsGroup>
  );
}

function RuleField({ label, value, placeholder, onChange }: { label: string; value: string; placeholder: string; onChange: (v: string) => void }) {
  const id = React.useId();
  return (
    <div>
      <label htmlFor={id} className="field-label">
        {label}
      </label>
      <div className="field">
        <input id={id} type="text" spellCheck={false} placeholder={placeholder} value={value} onChange={(e) => onChange(e.target.value)} />
      </div>
    </div>
  );
}

function RoutingTester() {
  const repos = useRes(reposRes).data;
  const [labels, setLabels] = React.useState("");
  const [repo, setRepo] = React.useState(repos[0]?.name ?? "");
  const [requires, setRequires] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const [problem, setProblem] = React.useState<string | null>(null);
  const [found, setFound] = React.useState<CiRouteExplain | null>(null);
  return (
    <SettingsGroup
      id="ci.routing.tester"
      title="Where would it go?"
      about="A leg with these labels and requirements, against the rules and the runners connected now."
      words="tester explain where runner eligible"
    >
      <form
        noValidate
        onSubmit={async (e) => {
          e.preventDefault();
          setBusy(true);
          setProblem(null);
          try {
            setFound(await ci.routingExplain({ labels: list(labels), repo: repo || undefined, requires: list(requires, /\n/) }));
          } catch (e2) {
            setProblem(errorMessage(e2));
          } finally {
            setBusy(false);
          }
        }}
      >
        <div className="st-fields">
          <RuleField label="Labels (runs-on)" placeholder="ubuntu-latest" value={labels} onChange={setLabels} />
          <div>
            <label htmlFor="st-route-repo" className="field-label">
              Repository
            </label>
            <div className="select">
              <select id="st-route-repo" value={repo} onChange={(e) => setRepo(e.target.value)}>
                <option value="">None in particular</option>
                {repos.map((r) => (
                  <option key={r.id} value={r.name}>
                    {r.name}
                  </option>
                ))}
              </select>
            </div>
          </div>
          <div className="col-span-full">
            <label className="field-label" htmlFor="st-route-requires">
              Requirements of its own
            </label>
            <div className="field">
              <textarea
                id="st-route-requires"
                rows={2}
                spellCheck={false}
                className="font-mono text-[12px]"
                placeholder="mem.total >= 16Gi"
                value={requires}
                onChange={(e) => setRequires(e.target.value)}
              />
            </div>
          </div>
        </div>
        <div className="mt-3 flex justify-end">
          <Button type="submit" size="xs" variant="secondary" disabled={busy}>
            Explain
          </Button>
        </div>
      </form>
      {problem && (
        <p className="st-note" data-tone="error" role="status">
          {problem}
        </p>
      )}
      {found && <Explained e={found} />}
    </SettingsGroup>
  );
}

function Explained({ e }: { e: CiRouteExplain }) {
  return (
    <div className="mt-4" aria-label="Where it would go">
      <SettingRow label="Its routing" help={e.rules.length ? `From ${e.rules.length} rule${e.rules.length === 1 ? "" : "s"}: ${e.rules.map((r) => r.source).join(", ")}` : "No rule matched."}>
        <span className="st-value">
          {[
            e.labels.length ? `labels ${e.labels.join(", ")}` : "",
            e.constraints.length ? e.constraints.join(" && ") : "",
            e.executors.length ? `on ${e.executors.join(" or ")}` : "",
            e.image ? `image ${e.image}` : "",
          ]
            .filter(Boolean)
            .join(" · ") || "any runner"}
        </span>
      </SettingRow>
      <ul className="st-list mt-2" aria-label="Runners">
        {e.runners.map((r) => {
          const failed = (r.checks ?? []).filter((c) => !c.pass);
          return (
            <li key={r.name}>
              <div className="st-item">
                <i className="dot" data-status={r.eligible ? (r.room === false ? "queued" : "succeeded") : "failed"} aria-hidden="true" />
                <span className="st-item-name">{r.name}</span>
                <span className="st-item-meta">
                  {r.eligible
                    ? r.room === false
                      ? "eligible, busy now"
                      : "eligible"
                    : `not eligible: ${failed.map((c) => `${c.requirement}${c.value ? ` (has ${c.value})` : ""}`).join("; ") || "its labels"}`}
                </span>
              </div>
            </li>
          );
        })}
      </ul>
      {e.runners.length === 0 && <p className="st-note">No runners are connected.</p>}
    </div>
  );
}

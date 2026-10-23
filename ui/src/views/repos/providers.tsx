/**
 * What runs a repository's CI (docs/ci.md W23-R07): KilnCI by default, its host's own CI, or
 * several at once. Shown when its host makes a choice possible.
 */
import * as React from "react";
import { ci, type CiProviders } from "@/api/ci";
import { Button } from "@/components/ui/button";
import { useRes, type Res } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";

const providersRes = (repo: string): Res<CiProviders> => ({ key: `ci/providers/${repo}`, fetch: (s) => ci.providers(repo, s) });

const NAMES: Record<string, string> = { kiln: "KilnCI", "github-actions": "GitHub Actions", "gitlab-ci": "GitLab CI/CD", "bitbucket-pipelines": "Bitbucket Pipelines" };

export function Providers({ repo }: { repo: string }) {
  const res = useRes(providersRes(repo));
  const p = res.data;
  const [picked, setPicked] = React.useState<string[] | null>(null);
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  if (p.available.length < 2) return null;
  const chosen = picked ?? p.providers;
  const toggle = (name: string) => setPicked(chosen.includes(name) ? chosen.filter((x) => x !== name) : [...chosen, name]);
  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      await ci.setProviders(repo, chosen);
      res.reload();
      setPicked(null);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <section className="repo-section" aria-label="What runs its CI">
      <h4 className="field-label">What runs its CI</h4>
      <div className="flex flex-wrap gap-x-5 gap-y-2">
        {p.available.map((name) => (
          <label key={name} className="flex items-center gap-2 text-[13px] text-[#c5cdd8]">
            <input type="checkbox" checked={chosen.includes(name)} onChange={() => toggle(name)} disabled={busy} />
            {NAMES[name] ?? name}
          </label>
        ))}
      </div>
      {picked && (
        <div className="flex flex-wrap items-center gap-2">
          <Button size="xs" onClick={() => void save()} disabled={busy || chosen.length === 0}>
            Save
          </Button>
          <Button size="xs" variant="ghost" onClick={() => setPicked(null)} disabled={busy}>
            Cancel
          </Button>
          {chosen.length === 0 && <span className="text-[12.5px] text-[#8c8f98]">At least one runs it.</span>}
        </div>
      )}
      {error && <p className="text-[12.5px] text-[#d1aaad]">{error}</p>}
    </section>
  );
}

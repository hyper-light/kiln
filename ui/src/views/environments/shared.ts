/** What the environments board and an environment's page both say (docs/ci.md W15, W21-R10). */
import type { CiDeployment, CiEnvironment, CiEnvKind, CiPreview, CiProtection } from "@/api/ci";
import type { Status } from "@/api/types";

const enc = encodeURIComponent;

export const environmentsHref = "/deployments/environments";
export const environmentHref = (repo: string, name: string) => `/deployments/environments/${enc(name)}?repo=${enc(repo)}`;

/** Server events after which an environment's deployments and waits may have moved. */
export const ENV_EVENTS = ["run"] as const;

/** Each kind, and what it deploys to (W15-R02). */
export const KINDS: { value: CiEnvKind; label: string; about: string }[] = [
  { value: "kubernetes", label: "Kubernetes", about: "kiln CD's clusters: CD syncs its apps there." },
  { value: "host", label: "Host", about: "Runners: the jobs deploying here run on them." },
  { value: "container", label: "Container", about: "Runners with Docker: the jobs deploying here run on them." },
  { value: "microvm", label: "microVM", about: "Runners with Firecracker or shards: the jobs deploying here run on them." },
  { value: "platform", label: "Platform", about: "A cloud or hosting platform: the jobs deploying here get its integration's credentials." },
];

export const kindLabel = (k: CiEnvKind | null | undefined) => KINDS.find((x) => x.value === k)?.label ?? null;

/** Where it deploys, in a line; null when it has no targets of its own. */
export function whereWords(e: Pick<CiEnvironment, "kind" | "targets">): string | null {
  const t = e.targets ?? {};
  switch (e.kind) {
    case "kubernetes":
      return [t.clusters?.join(", "), t.namespace].filter(Boolean).join(" / ") || null;
    case "host":
    case "container":
    case "microvm":
      return t.runners ?? null;
    case "platform":
      return t.integration ?? null;
    default:
      return null;
  }
}

/** Its protection and limit, as short phrases. */
export function protectionWords(p: CiProtection, concurrency: number | null): string[] {
  const out: string[] = [];
  if (p.reviewers?.length) out.push(`${p.reviewers.join(" or ")} ${p.reviewers.length === 1 ? "reviews" : "review"}`);
  if (p["wait-timer"]) out.push(`waits ${p["wait-timer"]} min`);
  if (p.branches) out.push(p.branches.length ? `${p.branches.join(", ")} only` : "no branch");
  if (p.tags?.length) out.push(`tags ${p.tags.join(", ")}`);
  if (p.after) out.push(`after ${p.after}${p.soak ? ` for ${p.soak}` : ""}`);
  if (p.windows?.length) out.push(`${p.windows.length} ${p.windows.length === 1 ? "window" : "windows"}`);
  if (p.gates?.length) out.push(`gates: ${p.gates.map((g) => g.workflow ?? g.check).join(", ")}`);
  if (concurrency) out.push(`${concurrency} at a time`);
  return out;
}

export const SOURCE_WORDS: Record<string, string> = {
  file: "from .kiln/environments.yaml",
  kiln: "made in kiln",
  run: "named by a job",
  preview: "a preview, from .kiln/environments.yaml's previews",
};

/** Where what a preview is for is reviewed (W15-R07): a diff's, a stack's or a change request's page; none for a host's pull request. */
export const previewOfHref = (p: CiPreview) => (p.kind === "pull_request" ? null : `/review/${enc(p.of)}`);

/** A deployment's state as a status dot shows it. */
export function deploymentStatus(state: string | null): Status {
  switch (state) {
    case "success":
      return "succeeded";
    case "failure":
    case "error":
      return "failed";
    case "in_progress":
      return "running";
    case "queued":
    case "pending":
      return "queued";
    default:
      return "skipped";
  }
}

/** A deployment's ref as people say it: a branch's or tag's name. */
export const refName = (d: CiDeployment) => d.ref.replace(/^refs\/(heads|tags)\//, "");

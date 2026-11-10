/**
 * Skills kiln installs for agents (docs/agents.md, "Skills kiln installs"; `/api/skills`): look at
 * a source, install what it holds for an agent, every agent of yours, or an agent's repository
 * folder, and see each package's state on the machines it went to.
 */
import { request } from "./client";

export interface SkillItem {
  name: string;
  description?: string;
}

export interface SkillSetup {
  skill: string;
  /** "Python", "Go", "Node", "Rust". */
  stack: string;
  command: string;
}

export interface SkillSource {
  /** "link", "folder", "upload" or "write". */
  kind: string;
  /** As shown: `cursor/plugins`, `adacachy:~/skills/perf`, `release-notes.md`. */
  label: string;
  url?: string;
  git_ref?: string;
  path?: string;
  machine?: string;
}

export type SkillState = "fetching" | "setting-up" | "ready" | "needs" | "failed";

export interface SkillMachine {
  machine: string;
  state: SkillState;
  /** What a machine lacks ("gh missing"), or why its setup failed. */
  note: string;
  /** Its setup's output, the last of it. */
  log: string;
}

export interface SkillPackage {
  id: string;
  name: string;
  version: string;
  description: string;
  source: SkillSource;
  sha: string;
  shape: string;
  skills: SkillItem[];
  agents: SkillItem[];
  hooks: string[];
  mcp: string[];
  setup: SkillSetup[];
  needs: string[];
  /** "This agent", "All my agents" or "This repo". */
  scope: string;
  on: boolean;
  /** A newer commit its link's branch is at. */
  update?: string | null;
  machines: SkillMachine[];
  by: string;
}

export interface SkillsList {
  packages: SkillPackage[];
  /** The agent's machine. */
  machine: string;
  /** Whether its machine's kiln installs skills (an older one doesn't). */
  installs: boolean;
  /** Whether this harness loads the installed packages (Claude Code currently). */
  loads?: boolean;
}

export interface SkillDraft {
  draft: string;
  name: string;
  version: string;
  license: string;
  description: string;
  shape: string;
  source: SkillSource;
  sha: string;
  skills: SkillItem[];
  agents: SkillItem[];
  hooks: string[];
  mcp: string[];
  setup: SkillSetup[];
  needs: string[];
}

/** What looking found: a package to install, or a marketplace's plugins to choose among. */
export type LookAnswer = SkillDraft | { choose: SkillItem[] };

export interface SkillPicks {
  skills: boolean;
  agents: boolean;
  hooks: boolean;
  mcp: boolean;
  setup: boolean;
}

export type SkillScope = "agent" | "mine" | "repo";

export type LookFrom = { from: "link"; link: string; plugin?: string } | { from: "folder"; path: string; plugin?: string } | { from: "write"; name: string; description: string; body: string };

const enc = encodeURIComponent;

export const skillsApi = {
  list: (agent: string) => request<SkillsList>("GET", `/api/skills?agent=${enc(agent)}`),
  look: (agent: string, from: LookFrom) => request<LookAnswer>("POST", "/api/skills/look", { agent, ...from }),
  upload: (agent: string, file: File, plugin?: string) =>
    request<LookAnswer>("POST", `/api/skills/upload?agent=${enc(agent)}&name=${enc(file.name)}${plugin ? `&plugin=${enc(plugin)}` : ""}`, undefined, {
      file,
      type: file.type || "application/octet-stream",
    }),
  install: (agent: string, draft: string, scope: SkillScope, picks: SkillPicks) => request<SkillPackage>("POST", "/api/skills", { agent, draft, scope, picks }),
  set: (id: string, on: boolean) => request<SkillPackage>("PATCH", `/api/skills/${enc(id)}`, { on }),
  update: (id: string) => request<SkillPackage>("POST", `/api/skills/${enc(id)}/update`, {}),
  remove: (id: string) => request<{ removed: string }>("DELETE", `/api/skills/${enc(id)}`),
  read: (id: string, skill: string) => request<{ content: string }>("GET", `/api/skills/${enc(id)}/skills/${enc(skill)}`),
};

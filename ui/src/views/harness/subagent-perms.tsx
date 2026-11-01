/**
 * What a person lets a subagent do, under its thread's crumbs (no popup): Read, Edit, Run and Web,
 * each Allow, Ask or Off; the folder its file calls stay in; how many minutes and tokens it may
 * use. A choice wider than what it's under lets is off (subagent-tree.ts `wider`). Each change goes
 * at once (`Command::Perms`): to a subagent kiln started, its session; to one of its harness's own,
 * by its task, its agent's session. Every choice sits at the panel's right edge.
 */
import * as React from "react";
import { Clock, Eye, Folder, Globe, Pencil, SquareTerminal, X } from "lucide-react";
import { harnessApi } from "@/api/harness";
import type { Grant } from "@/api/wire/Grant";
import type { Perms } from "@/api/wire/Perms";
import { toast } from "@/components/ui/toast";
import { errorMessage } from "@/lib/hooks";
import { wider, type SubagentNode } from "./subagent-tree";

const KINDS: { key: "read" | "edit" | "run" | "web"; label: string; icon: React.ReactNode }[] = [
  { key: "read", label: "Read", icon: <Eye size={14} aria-hidden="true" /> },
  { key: "edit", label: "Edit", icon: <Pencil size={14} aria-hidden="true" /> },
  { key: "run", label: "Run", icon: <SquareTerminal size={14} aria-hidden="true" /> },
  { key: "web", label: "Web", icon: <Globe size={14} aria-hidden="true" /> },
];
const GRANTS: Grant[] = ["allow", "ask", "off"];
const WORD: Record<Grant, string> = { allow: "Allow", ask: "Ask", off: "Off" };
const OPEN: Perms = { read: "allow", edit: "allow", run: "allow", web: "allow" };

/** A count from an input, or none for an empty one. */
const count = (v: string) => (v.trim() === "" ? undefined : Math.max(1, Math.floor(Number(v)) || 1));

export function PermsPanel({ n, can }: { n: Pick<SubagentNode, "kind" | "session" | "task" | "name" | "perms" | "over">; can: boolean }) {
  // What it's set to here, at once (its next state says so too); typed in, until it's left.
  const [shown, setShown] = React.useState<Perms | undefined>(n.perms);
  const now = shown ?? n.over ?? OPEN;
  const [folder, setFolder] = React.useState(now.folder ?? "");
  const [minutes, setMinutes] = React.useState(now.minutes != null ? String(now.minutes) : "");
  const [tokens, setTokens] = React.useState(now.tokens != null ? String(now.tokens) : "");
  // Each of its states is new: what it's set to is taken again only when that changed.
  const key = JSON.stringify(n.perms ?? null);
  const [was, setWas] = React.useState(key);
  if (key !== was) {
    const p = n.perms ?? n.over ?? OPEN;
    setWas(key);
    setShown(n.perms);
    setFolder(p.folder ?? "");
    setMinutes(p.minutes != null ? String(p.minutes) : "");
    setTokens(p.tokens != null ? String(p.tokens) : "");
  }
  const set = (next: Perms | undefined) => {
    setShown(next);
    harnessApi
      .command(n.session, { type: "perms", ...(next ? { perms: next } : {}), ...(n.kind === "harness" && n.task ? { task: n.task } : {}) })
      .catch((e: unknown) => toast.error(`Couldn't change what ${n.name} may do`, errorMessage(e)));
  };
  const change = (part: Partial<Perms>) => set({ ...now, ...part });
  const commit = (part: Partial<Perms>, key: keyof Perms) => {
    if (part[key] !== now[key]) change(part);
  };
  return (
    <div className="hx-perm" role="group" aria-label={`What ${n.name} may do`}>
      {KINDS.map((k) => (
        <React.Fragment key={k.key}>
          <span className="hx-perm-k">
            {k.icon}
            {k.label}
          </span>
          <span className="hx-perm-seg" role="group" aria-label={k.label}>
            {GRANTS.map((g) => (
              <button key={g} type="button" aria-pressed={now[k.key] === g} disabled={!can || wider(g, n.over?.[k.key])} onClick={() => change({ [k.key]: g })}>
                {WORD[g]}
              </button>
            ))}
          </span>
        </React.Fragment>
      ))}
      <span className="hx-perm-k">
        <Folder size={14} aria-hidden="true" />
        Where
      </span>
      <span className="hx-perm-where">
        <input
          value={folder}
          placeholder={n.over?.folder ?? "anywhere"}
          aria-label="Folder"
          disabled={!can}
          onChange={(e) => setFolder(e.target.value)}
          onBlur={() => commit({ folder: folder.trim() || undefined }, "folder")}
          onKeyDown={(e) => e.key === "Enter" && e.currentTarget.blur()}
        />
        <button
          type="button"
          className="hx-icon"
          aria-label="Anywhere"
          disabled={!can || !now.folder}
          onClick={() => {
            setFolder("");
            change({ folder: undefined });
          }}
        >
          <X size={12} />
        </button>
      </span>
      <span className="hx-perm-k">
        <Clock size={14} aria-hidden="true" />
        Limit
      </span>
      <span className="hx-perm-limits">
        <input
          value={minutes}
          inputMode="numeric"
          placeholder="minutes"
          aria-label="Minutes"
          disabled={!can}
          onChange={(e) => setMinutes(e.target.value.replace(/\D/g, ""))}
          onBlur={() => commit({ minutes: count(minutes) }, "minutes")}
          onKeyDown={(e) => e.key === "Enter" && e.currentTarget.blur()}
        />
        <input
          value={tokens}
          inputMode="numeric"
          placeholder="tokens"
          aria-label="Tokens"
          disabled={!can}
          onChange={(e) => setTokens(e.target.value.replace(/\D/g, ""))}
          onBlur={() => commit({ tokens: count(tokens) }, "tokens")}
          onKeyDown={(e) => e.key === "Enter" && e.currentTarget.blur()}
        />
      </span>
    </div>
  );
}

/**
 * The agent's MCP servers, for any harness: what's connected and what each server offers,
 * enable/reconnect/sign in/add/remove (through the harness's own CLI). Its skills are `skills.tsx`'s.
 */
import * as React from "react";
import { ChevronRight, ExternalLink, Plus, RefreshCw, Trash2 } from "lucide-react";
import { type McpServer, type SessionStore, type State } from "@/api/harness";
import { Button } from "@/components/ui/button";
import { toast } from "@/components/ui/toast";
import { useAction } from "@/lib/action";
import { cn } from "@/lib/utils";
import { deferredState } from "@/api/state-pages";
import { DeferredState } from "./deferred-state";

const MCP_DOT: Record<string, string> = { connected: "succeeded", pending: "queued", failed: "failed", needs_auth: "timed_out", disabled: "cancelled" };
const MCP_LABEL: Record<string, string> = { connected: "connected", pending: "connecting", failed: "failed", needs_auth: "needs sign-in", disabled: "off" };

function pairs(text: string): Record<string, string> | undefined {
  const out: Record<string, string> = {};
  for (const line of text.split("\n")) {
    const i = line.indexOf("=");
    if (i > 0) out[line.slice(0, i).trim()] = line.slice(i + 1).trim();
  }
  return Object.keys(out).length ? out : undefined;
}

function AddServer({ store, onDone }: { store: SessionStore; onDone: () => void }) {
  const [name, setName] = React.useState("");
  const [scope, setScope] = React.useState("local");
  const [transport, setTransport] = React.useState("stdio");
  const [command, setCommand] = React.useState("");
  const [url, setUrl] = React.useState("");
  const [env, setEnv] = React.useState("");
  const [headers, setHeaders] = React.useState("");
  const stdio = transport === "stdio";
  const ready = name.trim() && (stdio ? command.trim() : url.trim());
  const submit = (e: React.FormEvent) => {
    e.preventDefault();
    if (!ready) return;
    const [cmd, ...args] = command.trim().split(/\s+/);
    store.send({
      type: "mcp_add",
      scope,
      name: name.trim(),
      transport,
      ...(stdio ? { command: cmd, args, env: pairs(env) } : { url: url.trim(), headers: pairs(headers) }),
    });
    onDone();
  };
  return (
    <form className="hx-mcp-add" onSubmit={submit}>
      <div className="hx-row2">
        <label className="hx-field flex-1">
          <span className="hx-eyebrow">Name</span>
          <input className="hx-input" value={name} onChange={(e) => setName(e.target.value)} placeholder="github" spellCheck={false} autoCapitalize="off" />
        </label>
        <label className="hx-field">
          <span className="hx-eyebrow">Scope</span>
          <select className="hx-select" value={scope} onChange={(e) => setScope(e.target.value)}>
            <option value="local">This folder (just me)</option>
            <option value="project">This project (shared)</option>
            <option value="user">All my projects</option>
          </select>
        </label>
      </div>
      <label className="hx-field">
        <span className="hx-eyebrow">Transport</span>
        <div className="hx-modes" role="radiogroup" aria-label="Transport">
          {["stdio", "http", "sse"].map((t) => (
            <button key={t} type="button" role="radio" aria-checked={transport === t} className="hx-mode" data-mode="queue" onClick={() => setTransport(t)}>
              {t === "stdio" ? "Command" : t.toUpperCase()}
            </button>
          ))}
        </div>
      </label>
      {stdio ? (
        <>
          <label className="hx-field">
            <span className="hx-eyebrow">Command</span>
            <input
              className="hx-input hx-mono"
              value={command}
              onChange={(e) => setCommand(e.target.value)}
              placeholder="npx -y @modelcontextprotocol/server-github"
              spellCheck={false}
              autoCapitalize="off"
            />
          </label>
          <label className="hx-field">
            <span className="hx-eyebrow">Environment (KEY=value per line)</span>
            <textarea className="hx-input hx-mono" rows={2} value={env} onChange={(e) => setEnv(e.target.value)} spellCheck={false} autoCapitalize="off" />
          </label>
        </>
      ) : (
        <>
          <label className="hx-field">
            <span className="hx-eyebrow">URL</span>
            <input className="hx-input hx-mono" value={url} onChange={(e) => setUrl(e.target.value)} placeholder="https://mcp.example.com/mcp" spellCheck={false} autoCapitalize="off" />
          </label>
          <label className="hx-field">
            <span className="hx-eyebrow">Headers (Name=value per line)</span>
            <textarea className="hx-input hx-mono" rows={2} value={headers} onChange={(e) => setHeaders(e.target.value)} spellCheck={false} autoCapitalize="off" />
          </label>
        </>
      )}
      <div className="flex justify-end gap-2">
        <Button type="button" size="sm" variant="ghost" onClick={onDone}>
          Cancel
        </Button>
        <Button type="submit" size="sm" disabled={!ready}>
          <Plus /> Add server
        </Button>
      </div>
    </form>
  );
}

function ServerRow({ s, store }: { s: McpServer; store: SessionStore }) {
  const [open, setOpen] = React.useState(false);
  const [confirm, setConfirm] = React.useState(false);
  const [authUrl, setAuthUrl] = React.useState<string | null>(null);
  // Sign in says it's on its way while its harness is asked for the page (W21-R15). It answers with
  // the page, or that none is needed (what it says is said), or why it couldn't; no answer says so.
  const [signIn, signingIn] = useAction(async () => {
    const v = (await store.request((client) => ({ type: "mcp_auth", name: s.name, client }), 15000)) as { url?: string; ok?: boolean; message?: string; error?: string } | null;
    if (v?.url) {
      setAuthUrl(v.url);
      window.open(v.url, "_blank", "noopener,noreferrer");
    } else if (v && !v.error && v.ok !== false) {
      if (v.message) toast({ title: v.message, tone: "info" });
    } else throw new Error(v?.error ?? "Its harness didn't answer.");
  }, "Couldn't sign in");
  return (
    <li className="hx-mcp" data-status={s.status}>
      <div className="hx-mcp-head">
        <button type="button" className="hx-mcp-name" aria-expanded={open} onClick={() => setOpen(!open)}>
          <ChevronRight size={12} className={cn("hx-chev", open && "rotate-90")} />
          <i className="dot" data-status={MCP_DOT[s.status] ?? "skipped"} aria-hidden="true" />
          <span className="hx-trunc">{s.name}</span>
        </button>
        <span className="hx-dim text-[11px]">{MCP_LABEL[s.status] ?? s.status}</span>
        <span className="hx-mcp-tags">
          <span className="hx-tag">{s.transport}</span>
          <span className="hx-tag">{s.scope}</span>
          {s.tools.length > 0 && <span className="hx-meta-num">{s.tools.length} tools</span>}
        </span>
        <span className="hx-mcp-actions">
          {s.status === "needs_auth" && (
            <Button size="xs" pending={signingIn} onClick={signIn}>
              Sign in
            </Button>
          )}
          <button type="button" className="hx-icon" aria-label={`Reconnect ${s.name}`} title="Reconnect" onClick={() => store.send({ type: "mcp_reconnect", name: s.name })}>
            <RefreshCw size={12} />
          </button>
          <button
            type="button"
            role="switch"
            aria-checked={s.enabled}
            aria-label={`${s.name} on`}
            className="hx-switch"
            onClick={() => store.send({ type: "mcp_enable", name: s.name, enabled: !s.enabled })}
          >
            <span />
          </button>
        </span>
      </div>
      {authUrl && (
        <div className="hx-dim text-[11.5px]">
          A sign-in page opened.{" "}
          <a className="text-link" href={authUrl} target="_blank" rel="noopener noreferrer">
            Open it again <ExternalLink size={10} className="inline" />
          </a>
        </div>
      )}
      {open && (
        <div className="hx-mcp-body">
          {s.target && <code className="hx-mcp-target">{s.target}</code>}
          {s.error && <div className="hx-bad">{s.error}</div>}
          {s.tools.length > 0 ? (
            <ul className="hx-mcp-tools">
              {s.tools.map((t) => (
                <li key={t.name}>
                  <code>{t.name}</code>
                  {t.description && <span className="hx-dim">{t.description}</span>}
                </li>
              ))}
            </ul>
          ) : (
            <div className="hx-dim text-[12px]">No tools reported.</div>
          )}
          {(s.resources || s.prompts) && (
            <div className="hx-dim text-[11.5px]">
              {s.resources ? `${s.resources} resources` : ""}
              {s.resources && s.prompts ? " · " : ""}
              {s.prompts ? `${s.prompts} prompts` : ""}
            </div>
          )}
          <div>
            {confirm ? (
              <span className="flex items-center gap-2 text-[12px]">
                Remove {s.name} from {s.scope} settings?
                <Button
                  size="xs"
                  variant="danger"
                  onClick={() => {
                    store.send({ type: "mcp_remove", scope: s.scope, name: s.name });
                    setConfirm(false);
                  }}
                >
                  Remove
                </Button>
                <Button size="xs" variant="ghost" onClick={() => setConfirm(false)}>
                  Keep
                </Button>
              </span>
            ) : (
              <Button size="xs" variant="ghost" onClick={() => setConfirm(true)}>
                <Trash2 /> Remove server
              </Button>
            )}
          </div>
        </div>
      )}
    </li>
  );
}

export function McpPanel({ state, store }: { state: State | null; store: SessionStore }) {
  const [adding, setAdding] = React.useState(false);
  if (deferredState(state, "mcp") || deferredState(state, "inventory"))
    return (
      <>
        <DeferredState store={store} field="mcp" />
        <DeferredState store={store} field="inventory" />
      </>
    );
  const known = state?.inventory?.mcp;
  const servers = state?.mcp;
  if (known === "unsupported" || (servers === undefined && !known)) return <p className="in-none">This agent's harness doesn't tell kiln about its MCP servers.</p>;
  if (known === "loading" && !servers?.length) return <p className="in-none">Asking the agent for its MCP servers…</p>;
  if (servers === undefined) return null;
  const connected = servers.filter((s) => s.status === "connected").length;
  return (
    <div className="hx-stack">
      <div className="hx-section-head">
        <span className="hx-eyebrow">
          {servers.length} server{servers.length === 1 ? "" : "s"} · {connected} connected
        </span>
        {!adding && (
          <Button size="xs" variant="secondary" onClick={() => setAdding(true)}>
            <Plus /> Add
          </Button>
        )}
      </div>
      {adding && <AddServer store={store} onDone={() => setAdding(false)} />}
      {servers.length === 0 && !adding && <p className="in-none">None configured for this agent.</p>}
      <ul className="hx-mcps">
        {servers.map((s) => (
          <ServerRow key={`${s.scope}:${s.name}`} s={s} store={store} />
        ))}
      </ul>
    </div>
  );
}

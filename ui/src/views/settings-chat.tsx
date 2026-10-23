/**
 * Settings → You → Chat accounts (docs/deploy-api.md §12.3): the chat accounts that act as you in
 * kiln. A command said in Slack, Discord, Telegram or Teams (deploy, approve, reject, rollback,
 * status) runs as you, and each environment's gates hold it as they hold the API. A code links one:
 * say `link <code>` to kiln in that chat within ten minutes. In Slack, the account whose email is
 * yours acts as you without one. A bot linked with your code acts as an agent for you, as far as
 * each environment lets agents.
 */
import * as React from "react";
import { api, type ChatLinks } from "@/api/client";
import { Button } from "@/components/ui/button";
import { Hold } from "@/components/ui/hold";
import { SettingsGroup } from "@/components/ui/settings";
import { relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { useRes, type Res } from "@/lib/data";

const ID = "you.chat";
const TITLE = "Chat accounts";
const ABOUT = "Accounts in Slack, Discord, Telegram and Teams whose commands to kiln run as you.";
const WORDS = "chat slack discord telegram teams link code commands deploy approve reject rollback status bot";

export const chatLinksRes: Res<ChatLinks> = { key: "me/chat", fetch: (s) => api.myChat(s) };

const KIND_TITLES: Record<string, string> = { slack: "Slack", discord: "Discord", telegram: "Telegram", teams: "Teams" };

export function ChatAccountsHold() {
  return (
    <SettingsGroup id={ID} title={TITLE} about={ABOUT} words={WORDS}>
      <Hold height={80} />
    </SettingsGroup>
  );
}

export function ChatAccountsGroup() {
  const res = useRes(chatLinksRes);
  const { links, chats } = res.data;
  const [code, setCode] = React.useState<{ code: string; expires_at: number } | null>(null);
  const [busy, setBusy] = React.useState(false);
  const [problem, setProblem] = React.useState<string | null>(null);
  const newCode = async () => {
    setBusy(true);
    setProblem(null);
    try {
      setCode(await api.chatCode());
    } catch (e) {
      setProblem(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  const unlink = async (integration: string, account: string) => {
    setBusy(true);
    setProblem(null);
    try {
      await api.unlinkChat(integration, account);
      res.setData((d) => ({ ...d, links: d.links.filter((l) => !(l.id === integration && l.account === account)) }));
    } catch (e) {
      setProblem(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <SettingsGroup id={ID} title={TITLE} about={ABOUT} words={WORDS}>
      {chats.length === 0 ? (
        <p className="st-note">kiln has no chat yet: an administrator adds Slack, Discord, Telegram or Teams under Deployments → Accounts.</p>
      ) : links.length === 0 ? (
        <p className="st-note">None linked. In Slack, the account with your email acts as you already.</p>
      ) : (
        <ul className="st-list" aria-label="Your chat accounts">
          {links.map((l) => (
            <li key={`${l.id}/${l.account}`}>
              <div className="st-item">
                <span className="st-item-name">{l.name || l.account}</span>
                <span className="st-item-meta">
                  {KIND_TITLES[l.kind ?? ""] ?? l.kind ?? "a chat kiln no longer has"}
                  {l.integration ? ` · ${l.integration}` : ""} · linked {relativeTime(l.created_at)}
                </span>
                <span className="st-item-acts">
                  <Button size="xs" variant="ghost" disabled={busy} onClick={() => void unlink(l.id, l.account)}>
                    Unlink
                  </Button>
                </span>
              </div>
            </li>
          ))}
        </ul>
      )}
      {chats.length > 0 &&
        (code ? (
          <div className="st-trust mt-3" role="status">
            <p>
              In {chats.map((c) => c.name).join(", ")}, say to kiln: <code>link {code.code}</code> (Slack: <code>/kiln link {code.code}</code>; Telegram: <code>/link {code.code}</code>; Teams: <code>@kiln link {code.code}</code>).
            </p>
            <p>
              It works once, until {new Date(code.expires_at * 1000).toLocaleTimeString()}. Type it yourself: whoever says it acts as you there.
            </p>
          </div>
        ) : (
          <div className="mt-3 flex justify-end">
            <Button size="xs" variant="secondary" disabled={busy} onClick={() => void newCode()}>
              Link a chat account
            </Button>
          </div>
        ))}
      {problem && (
        <p className="st-note" data-tone="error" role="status">
          {problem}
        </p>
      )}
    </SettingsGroup>
  );
}

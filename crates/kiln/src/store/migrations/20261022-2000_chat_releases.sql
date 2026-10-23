-- Chat (docs/deploy-api.md §12.3): each release's message in a channel that can change a message
-- afterwards (a Slack bot's, a Discord webhook's, a Telegram bot's), edited as the release moves;
-- the chat accounts people linked to theirs, and the codes that link them; and an integration's
-- signing secret (a Slack app's, a Teams outgoing webhook's), kept beside its token.
CREATE TABLE IF NOT EXISTS release_chat_posts (
  release_id INTEGER NOT NULL,
  spec TEXT NOT NULL,                     -- the channel, as named: slack:#deploys
  integration TEXT NOT NULL,              -- the integration's id
  channel TEXT NOT NULL,                  -- where it is: a Slack channel's id, a Telegram chat's
  message TEXT NOT NULL,                  -- which: Slack's ts, Discord's or Telegram's id
  shown TEXT NOT NULL,                    -- what it says now, hashed
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (release_id, spec)
);

CREATE TABLE IF NOT EXISTS chat_links (
  integration TEXT NOT NULL,              -- the integration's id
  chat_user TEXT NOT NULL,                -- the account there: Slack's, Discord's, Telegram's, Teams'
  user_id TEXT NOT NULL,
  name TEXT NOT NULL DEFAULT '',          -- as the chat named it
  created_at INTEGER NOT NULL,
  PRIMARY KEY (integration, chat_user)
);
CREATE INDEX IF NOT EXISTS chat_links_user ON chat_links (user_id);

CREATE TABLE IF NOT EXISTS chat_link_codes (
  code TEXT PRIMARY KEY,                  -- its SHA-256
  user_id TEXT NOT NULL,
  expires_at INTEGER NOT NULL
);

ALTER TABLE ci_integrations ADD COLUMN signing_ref TEXT;

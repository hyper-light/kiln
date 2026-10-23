-- Replies to kiln's review messages (docs/ci.md W14-R05): each email's reply address, by its token,
-- for one thread and the address it went to; and each Slack message a bot posted about a thread,
-- so a reply in its Slack thread finds the review thread.
CREATE TABLE review_reply_tokens (
  token TEXT PRIMARY KEY,
  thread_id TEXT NOT NULL,
  address TEXT NOT NULL COLLATE NOCASE,
  created_at INTEGER NOT NULL
);
CREATE INDEX review_reply_tokens_age ON review_reply_tokens (created_at);

CREATE TABLE review_chat_posts (
  integration TEXT NOT NULL,              -- the integration's id
  channel TEXT NOT NULL,
  ts TEXT NOT NULL,                       -- the message's, as Slack names it
  thread_id TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (integration, channel, ts)
);

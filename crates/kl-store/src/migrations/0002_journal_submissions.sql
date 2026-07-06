-- klayer migration 0002 — repo-scoped session memory and the marketplace
-- publish queue. Both tables are additive and guarded by IF NOT EXISTS so this
-- migration is safe to run on top of an existing 0001 database.

-- Repo-scoped session memory: a curated journal the model writes to (log_work)
-- and replays at session start (recall_session) so it re-establishes context and
-- stops repeating mistakes. Distinct from the noisy auto-logged `episodes` trace.
CREATE TABLE IF NOT EXISTS journal (
  id    INTEGER PRIMARY KEY,
  repo  TEXT    NOT NULL,          -- canonical repo path or friendly name
  kind  TEXT    NOT NULL,          -- 'done'|'failed'|'avoid'|'decision'|'note'
  title TEXT    NOT NULL,
  body  TEXT,
  ts    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS journal_repo ON journal(repo, ts);

-- Marketplace publish queue: a user publishes a local domain, it becomes a
-- pending submission, the admin reviews it in the dashboard and approves (append
-- to marketplace.json) or denies (with a reason).
CREATE TABLE IF NOT EXISTS submissions (
  id           INTEGER PRIMARY KEY,
  slug         TEXT    NOT NULL,
  description  TEXT,
  query_hint   TEXT,
  items_json   TEXT    NOT NULL,   -- serialized Vec<MarketplaceItem>
  status       TEXT    NOT NULL DEFAULT 'pending',  -- pending|approved|denied
  note         TEXT,               -- admin review / denial reason
  submitted_at INTEGER NOT NULL,
  reviewed_at  INTEGER
);
CREATE INDEX IF NOT EXISTS submissions_status ON submissions(status, submitted_at);

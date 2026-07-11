-- klayer migration 0002 — the marketplace publish queue. Additive and guarded
-- by IF NOT EXISTS so this migration is safe to run on top of an existing
-- 0001 database.
--
-- Repo-scoped session memory (formerly the `journal` table here) now lives in
-- its own kl-session crate/DB — see crates/kl-session/src/migrations.

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

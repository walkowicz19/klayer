-- klayer migration 0003 — domain author attribution.
-- A single local author identity registered once and reused for every domain the
-- user publishes. The name can be changed from Settings but only once every 14
-- days (cooldown enforced in kl-store::set_author). The submissions.author column
-- is added separately in Rust with a guarded ALTER (SQLite has no ADD COLUMN
-- IF NOT EXISTS).

CREATE TABLE IF NOT EXISTS author (
  id            INTEGER PRIMARY KEY CHECK (id = 1),  -- single-row identity
  name          TEXT    NOT NULL,
  registered_at INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL
);

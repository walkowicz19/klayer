-- kl-session migration 0001 — repo-scoped session memory, carved out of
-- klayer.db's `journal` table (kl-store migration 0002) as its own store.
--
-- A curated journal the model writes to (log_work) and replays at session
-- start (recall_session) so it re-establishes context and stops repeating
-- mistakes. Distinct from the noisy auto-logged `episodes` trace (kl-store).

PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS journal (
  id            INTEGER PRIMARY KEY,
  repo          TEXT    NOT NULL,          -- canonical repo path or friendly name
  kind          TEXT    NOT NULL,          -- 'done'|'failed'|'avoid'|'decision'|'note'
  title         TEXT    NOT NULL,
  body          TEXT,
  ts            INTEGER NOT NULL,
  is_checkpoint INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS journal_repo ON journal(repo, ts);
CREATE INDEX IF NOT EXISTS journal_checkpoint ON journal(repo, is_checkpoint, ts);

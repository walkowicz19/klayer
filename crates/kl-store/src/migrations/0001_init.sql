-- klayer migration 0001 — keyword-only default (no vector table).
-- The vector tier (chunks_vec via sqlite-vec) is added by the `embed-local`
-- build and is intentionally absent here so the default build needs zero
-- extra native deps. See README "Vector retrieval".

PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS sources (
  id           INTEGER PRIMARY KEY,
  kind         TEXT    NOT NULL,
  uri          TEXT,
  title        TEXT,
  domain       TEXT    NOT NULL,
  fetched_at   INTEGER NOT NULL,
  content_hash TEXT,
  trust        TEXT    NOT NULL DEFAULT 'untrusted'
);

CREATE TABLE IF NOT EXISTS chunks (
  id        INTEGER PRIMARY KEY,
  source_id INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
  domain    TEXT    NOT NULL,
  ord       INTEGER NOT NULL,
  text      TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS chunks_domain ON chunks(domain);

-- Full-text index over the reference tier. rowid == chunks.id.
CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(text);

CREATE TABLE IF NOT EXISTS knowledge (
  id          INTEGER PRIMARY KEY,
  kind        TEXT    NOT NULL CHECK (kind IN ('fact','rule','procedure')),
  domain      TEXT    NOT NULL,
  stage       TEXT,
  title       TEXT    NOT NULL,
  body        TEXT    NOT NULL,
  trigger     TEXT,
  severity    TEXT,
  remediation TEXT,
  source_id   INTEGER REFERENCES sources(id),
  trust       TEXT    NOT NULL DEFAULT 'proposed',
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS knowledge_lookup ON knowledge(domain, stage, kind, trust);

CREATE TABLE IF NOT EXISTS preferences (
  id         INTEGER PRIMARY KEY,
  scope      TEXT    NOT NULL DEFAULT 'global',
  statement  TEXT    NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS domains (
  name         TEXT PRIMARY KEY,
  description  TEXT,
  query_hint   TEXT,
  doc_count    INTEGER DEFAULT 0,
  rule_count   INTEGER DEFAULT 0,
  last_updated INTEGER
);

CREATE TABLE IF NOT EXISTS stages (
  taxonomy    TEXT    NOT NULL,
  name        TEXT    NOT NULL,
  ordinal     INTEGER NOT NULL,
  description TEXT,
  PRIMARY KEY (taxonomy, name)
);

CREATE TABLE IF NOT EXISTS episodes (
  id          INTEGER PRIMARY KEY,
  run_id      TEXT    NOT NULL,
  step        INTEGER NOT NULL,
  stage       TEXT,
  action      TEXT,
  observation TEXT,
  outcome     TEXT,
  ts          INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS episodes_run ON episodes(run_id, step);

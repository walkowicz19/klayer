-- kl-train migration 0001 — trust-gated training-data layer.
--
-- A parallel store (KLAYER_TRAIN_DB, default klayer_train.db), intentionally
-- separate from klayer.db (knowledge/episodes) and klayer_code.db (code memory).
-- Every row carries provenance {student|teacher|human} and a trust tier reused
-- from kl_core::Trust. The collapse guard (no promoting student rows) and the
-- export gate (only reviewed+user) are enforced in code over this schema.

PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS training_examples (
  id                INTEGER PRIMARY KEY,
  domain            TEXT    NOT NULL,
  system_prompt     TEXT,
  user_content      TEXT    NOT NULL,
  assistant_content TEXT,                       -- NULL/empty for stubs awaiting a teacher answer
  label_type        TEXT    NOT NULL CHECK (label_type IN ('grounded','refusal')),
  trust             TEXT    NOT NULL DEFAULT 'proposed',
  provenance        TEXT    NOT NULL CHECK (provenance IN ('student','teacher','human')),
  retrieval_ref     TEXT,                       -- e.g. "knowledge:#42", "episode:run/step", "stage:name"
  verify_log        TEXT,                       -- verifier output from the external teacher project
  created_at        INTEGER NOT NULL,
  updated_at        INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS training_domain_trust ON training_examples(domain, trust);
CREATE INDEX IF NOT EXISTS training_provenance   ON training_examples(provenance);

-- Plain FTS5 (no content=): owns its data so standard DELETE works. rowid ==
-- training_examples.id. Synced manually on insert/delete, exactly as kl-store's
-- chunks_fts and kl-code's code_fts.
CREATE VIRTUAL TABLE IF NOT EXISTS training_fts USING fts5(body);

-- klayer migration 0004 — media attachments (Stage G: images first, per the
-- roadmap's explicit scoping). Video and an object-store (S3-compatible)
-- backend are deliberately deferred later increments — see
-- crates/kl-store/src/media.rs.
--
-- `trust` is NULL for standalone media (no `knowledge_id` yet): it mirrors how
-- other untethered ingestion in this codebase stays unpromoted until reviewed
-- or attached, and is only populated (inheriting the linked item's trust) once
-- `attach_media`/an attach-at-ingest-time call links it to a knowledge item.

CREATE TABLE IF NOT EXISTS media_attachments (
  media_id     INTEGER PRIMARY KEY,
  storage_ref  TEXT    NOT NULL,
  mime_type    TEXT    NOT NULL,
  size_bytes   INTEGER NOT NULL,
  caption      TEXT,
  knowledge_id INTEGER REFERENCES knowledge(id) ON DELETE SET NULL,
  domain       TEXT,
  trust        TEXT,
  created_at   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS media_domain ON media_attachments(domain);
CREATE INDEX IF NOT EXISTS media_knowledge ON media_attachments(knowledge_id);

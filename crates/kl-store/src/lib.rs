//! kl-store — SQLite persistence and retrieval for klayer.
//!
//! Concurrency model: a single `Connection` behind a `Mutex`. All DB work is
//! synchronous and never held across an `.await`, so this is Send + Sync and
//! safe to share via `Arc` inside the async MCP server. For higher concurrency,
//! swap in a pool (r2d2) — the public API here would not change.
//!
//! Module layout: this file is the composition root — connection lifecycle
//! (`open`/`migrate`), the `Store` struct, and a handful of small standalone
//! concerns (preferences, author identity) that don't cleanly belong to any
//! one domain module. Everything else is split by responsibility:
//! `domains` (domain registry + marketplace submissions), `knowledge`
//! (curated knowledge CRUD, recall, media-attachment rows), `sources`
//! (reference-tier ingestion), `acl` (trust/permission enforcement lookups),
//! `episodes` (agentic episodic memory + retention sweep), `model_registry`
//! (model registry + routing rules), and `media` (filesystem storage backend
//! for media bytes, pre-existing). Each module adds its methods to `Store`
//! via its own `impl Store { ... }` block — multiple impl blocks for one
//! type across files is ordinary Rust, so the public API on `Store` is
//! unaffected by where a method's body physically lives.

pub mod media;

mod acl;
mod domains;
mod episodes;
mod knowledge;
mod model_registry;
mod sources;

use std::sync::Mutex;

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};

const MIGRATION: &str = include_str!("migrations/0001_init.sql");
const MIGRATION_2: &str = include_str!("migrations/0002_journal_submissions.sql");
const MIGRATION_3: &str = include_str!("migrations/0003_author.sql");
const MIGRATION_4: &str = include_str!("migrations/0004_media.sql");

/// Seconds in the author-name change cooldown window (14 days).
pub const AUTHOR_COOLDOWN_SECS: i64 = 14 * 24 * 60 * 60;

/// Result of `set_author` — either the first registration, an allowed change, or
/// a change blocked by the 14-day cooldown (carrying when it next unlocks).
pub enum AuthorSetOutcome {
    Registered,
    Updated,
    Blocked { next_allowed_at: i64 },
}

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &str) -> Result<Self> {
        // NOTE: this opens a plain rusqlite (aliased libsql-rusqlite)
        // `Connection` directly rather than going through
        // `kl_core::open_db()` (the libsql-native path other crates use).
        // This is intentional — see the workspace-level `Cargo.toml`'s
        // comment on the `rusqlite` dependency for the full explanation of
        // the SQLite dual-link conflict this avoids. Do not "unify" this
        // with `kl_core::open_db()`.
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
        let conn = Connection::open(path).with_context(|| format!("opening db at {path}"))?;
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "foreign_keys", "ON").ok();
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn migrate(&self) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute_batch(MIGRATION)
            .context("running migration 0001")?;
        c.execute_batch(MIGRATION_2)
            .context("running migration 0002")?;
        c.execute_batch(MIGRATION_3)
            .context("running migration 0003")?;
        c.execute_batch(MIGRATION_4)
            .context("running migration 0004")?;
        // SQLite has no ADD COLUMN IF NOT EXISTS; guard it so migrate() is idempotent.
        ensure_column(&c, "submissions", "author", "TEXT")?;
        ensure_column(&c, "knowledge", "conflict_with_id", "INTEGER")?;
        ensure_column(&c, "knowledge", "conflict_status", "TEXT")?;
        ensure_column(
            &c,
            "episodes",
            "knowledge_ids_used",
            "TEXT NOT NULL DEFAULT '[]'",
        )?;
        c.execute(
            "CREATE INDEX IF NOT EXISTS knowledge_conflicts ON knowledge(domain, conflict_status)",
            [],
        )?;
        c.execute_batch("CREATE TABLE IF NOT EXISTS domain_permissions (identity TEXT NOT NULL, domain TEXT NOT NULL, allowed INTEGER NOT NULL DEFAULT 1, PRIMARY KEY(identity, domain)); CREATE INDEX IF NOT EXISTS domain_permissions_domain ON domain_permissions(domain);")?;
        c.execute_batch("CREATE TABLE IF NOT EXISTS model_registry (harness TEXT NOT NULL, model_id TEXT NOT NULL, capability_tier TEXT NOT NULL, cost_weight REAL NOT NULL, sub_agent_name TEXT, PRIMARY KEY(harness, model_id, sub_agent_name)); CREATE TABLE IF NOT EXISTS routing_rules (harness TEXT NOT NULL, domain_type TEXT NOT NULL, task_type TEXT NOT NULL, complexity_tier TEXT NOT NULL, model_id TEXT NOT NULL, PRIMARY KEY(harness, domain_type, task_type, complexity_tier));")?;
        ensure_column(&c, "domains", "enforced", "BOOLEAN NOT NULL DEFAULT 0")?;
        ensure_column(
            &c,
            "domains",
            "redact_enabled",
            "BOOLEAN NOT NULL DEFAULT 1",
        )?;
        ensure_column(&c, "episodes", "domain", "TEXT")?;
        // Best-effort, self-reported usage metadata (Stage D). MCP has no
        // standard field for token/cost accounting, so these are never
        // populated automatically — only when a caller passes model/
        // tokens_used/cost explicitly to remember/ingest/recall.
        ensure_column(&c, "episodes", "model", "TEXT")?;
        ensure_column(&c, "episodes", "tokens_used", "INTEGER")?;
        ensure_column(&c, "episodes", "cost", "REAL")?;
        ensure_column(&c, "domains", "retention_days", "INTEGER")?;
        ensure_column(
            &c,
            "domains",
            "is_marketplace_template",
            "BOOLEAN NOT NULL DEFAULT 0",
        )?;
        ensure_column(&c, "knowledge", "retention_days", "INTEGER")?;
        c.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS chunks_vec USING vec0(chunk_id INTEGER PRIMARY KEY, embedding float[384]);
             CREATE VIRTUAL TABLE IF NOT EXISTS knowledge_vec USING vec0(knowledge_id INTEGER PRIMARY KEY, embedding float[384]);"
        ).ok();
        Ok(())
    }

    // ---- vector search (sqlite-vec) ---------------------------------------

    pub fn insert_chunk_vector(&self, chunk_id: i64, embedding: &[f32]) -> Result<()> {
        let c = self.conn.lock().unwrap();
        let bytes = zerocopy::IntoBytes::as_bytes(embedding);
        c.execute(
            "INSERT OR REPLACE INTO chunks_vec(chunk_id, embedding) VALUES (?1, ?2)",
            params![chunk_id, bytes],
        )?;
        Ok(())
    }

    pub fn insert_knowledge_vector(&self, knowledge_id: i64, embedding: &[f32]) -> Result<()> {
        let c = self.conn.lock().unwrap();
        let bytes = zerocopy::IntoBytes::as_bytes(embedding);
        c.execute(
            "INSERT OR REPLACE INTO knowledge_vec(knowledge_id, embedding) VALUES (?1, ?2)",
            params![knowledge_id, bytes],
        )?;
        Ok(())
    }

    pub fn search_knowledge_vector(&self, embedding: &[f32], limit: usize) -> Result<Vec<(i64, f64)>> {
        let c = self.conn.lock().unwrap();
        let bytes = zerocopy::IntoBytes::as_bytes(embedding);
        let mut stmt = c.prepare(
            "SELECT knowledge_id, distance FROM knowledge_vec WHERE embedding MATCH ?1 ORDER BY distance LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![bytes, limit as i64], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ---- preferences ------------------------------------------------------

    pub fn set_preference(&self, scope: &str, statement: &str) -> Result<i64> {
        let now = Utc::now().timestamp();
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO preferences (scope, statement, created_at) VALUES (?1, ?2, ?3)",
            params![scope, statement, now],
        )?;
        Ok(c.last_insert_rowid())
    }

    pub fn list_preferences(&self) -> Result<Vec<String>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare("SELECT statement FROM preferences ORDER BY created_at ASC")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ---- author identity (14-day change cooldown) -------------------------

    /// The registered author: (name, registered_at, updated_at), or None if unset.
    pub fn get_author(&self) -> Result<Option<(String, i64, i64)>> {
        let c = self.conn.lock().unwrap();
        Ok(c.query_row(
            "SELECT name, registered_at, updated_at FROM author WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?)
    }

    /// Register (first time, always allowed) or change the author name. A change
    /// is only permitted once every `AUTHOR_COOLDOWN_SECS`; otherwise the current
    /// name stands and the caller is told when the next change unlocks.
    pub fn set_author(&self, name: &str, cooldown_secs: i64) -> Result<AuthorSetOutcome> {
        let now = Utc::now().timestamp();
        let c = self.conn.lock().unwrap();
        let existing: Option<i64> = c
            .query_row("SELECT updated_at FROM author WHERE id = 1", [], |r| {
                r.get(0)
            })
            .optional()?;
        match existing {
            None => {
                c.execute(
                    "INSERT INTO author (id, name, registered_at, updated_at) VALUES (1, ?1, ?2, ?2)",
                    params![name, now],
                )?;
                Ok(AuthorSetOutcome::Registered)
            }
            Some(updated_at) => {
                let next_allowed_at = updated_at + cooldown_secs;
                if now >= next_allowed_at {
                    c.execute(
                        "UPDATE author SET name = ?1, updated_at = ?2 WHERE id = 1",
                        params![name, now],
                    )?;
                    Ok(AuthorSetOutcome::Updated)
                } else {
                    Ok(AuthorSetOutcome::Blocked { next_allowed_at })
                }
            }
        }
    }
}

/// Add a column to a table if it does not already exist (idempotent migration
/// helper — SQLite lacks ADD COLUMN IF NOT EXISTS).
fn ensure_column(c: &Connection, table: &str, col: &str, decl: &str) -> Result<()> {
    let mut stmt = c.prepare(&format!("PRAGMA table_info({table})"))?;
    let has = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<String>>>()?
        .iter()
        .any(|n| n == col);
    drop(stmt);
    if !has {
        c.execute(&format!("ALTER TABLE {table} ADD COLUMN {col} {decl}"), [])?;
    }
    Ok(())
}

/// Clamp a requested retention value down to `max` (the per-tenant
/// `KLAYER_MAX_RETENTION_DAYS` ceiling), if both are present and the request
/// exceeds it. `None` (no expiration) is never clamped — a ceiling caps how
/// long data may be *kept*, it doesn't force an expiration onto data that
/// wasn't given one.
pub(crate) fn clamp_retention(value: Option<i64>, max: Option<i64>) -> Option<i64> {
    match (value, max) {
        (Some(n), Some(max)) if n > max => Some(max),
        (v, _) => v,
    }
}

#[cfg(test)]
mod vec_tests {
    use super::*;

    #[test]
    fn test_sqlite_vec_vector_search() {
        let store = Store::open(":memory:").unwrap();
        store.migrate().unwrap();

        let vec1 = vec![0.1f32; 384];
        let vec2 = vec![0.9f32; 384];

        store.insert_knowledge_vector(1, &vec1).unwrap();
        store.insert_knowledge_vector(2, &vec2).unwrap();

        let results = store.search_knowledge_vector(&vec1, 2).unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].0, 1);
    }
}

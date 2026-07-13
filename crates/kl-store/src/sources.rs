//! Reference-tier source ingestion: source records, chunk storage + FTS
//! mirroring, and deletion/clearing of ingested reference material.

use anyhow::Result;
use chrono::Utc;
use kl_core::SourceRow;
use rusqlite::{params, OptionalExtension};

use crate::Store;

impl Store {
    // ---- ingestion (reference tier) --------------------------------------

    pub fn add_source(
        &self,
        kind: &str,
        uri: Option<&str>,
        title: Option<&str>,
        domain: &str,
    ) -> Result<i64> {
        let now = Utc::now().timestamp();
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO sources (kind, uri, title, domain, fetched_at, trust)
             VALUES (?1, ?2, ?3, ?4, ?5, 'untrusted')",
            params![kind, uri, title, domain, now],
        )?;
        Ok(c.last_insert_rowid())
    }

    /// Insert reference chunks and mirror them into the FTS index (rowid == chunk id).
    pub fn add_chunks(&self, source_id: i64, domain: &str, chunks: &[String]) -> Result<usize> {
        let now = Utc::now().timestamp();
        let redact = self.domain_redact_enabled(domain)?;
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        for (ord, text) in chunks.iter().enumerate() {
            let redacted;
            let text: &str = if redact {
                redacted = kl_core::redact::redact(text);
                &redacted
            } else {
                text
            };
            tx.execute(
                "INSERT INTO chunks (source_id, domain, ord, text) VALUES (?1, ?2, ?3, ?4)",
                params![source_id, domain, ord as i64, text],
            )?;
            let id = tx.last_insert_rowid();
            tx.execute(
                "INSERT INTO chunks_fts (rowid, text) VALUES (?1, ?2)",
                params![id, text],
            )?;
        }
        // Fold the domain touch into the same transaction so we only hold the
        // mutex once. The original design called touch_domain_internal after
        // commit while c was still in scope, which deadlocked on every ingest.
        tx.execute(
            "INSERT INTO domains (name, doc_count, last_updated) VALUES (?1, 1, ?2)
             ON CONFLICT(name) DO UPDATE SET doc_count = domains.doc_count + 1, last_updated = ?2",
            params![domain, now],
        )?;
        tx.commit()?;
        Ok(chunks.len())
    }

    pub fn delete_source(&self, id: i64) -> Result<bool> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;

        let domain: Option<String> = tx
            .query_row(
                "SELECT domain FROM sources WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(domain) = domain else {
            return Ok(false);
        };

        tx.execute(
            "DELETE FROM chunks_fts WHERE rowid IN (SELECT id FROM chunks WHERE source_id = ?1)",
            params![id],
        )?;
        tx.execute("DELETE FROM chunks WHERE source_id = ?1", params![id])?;
        tx.execute(
            "UPDATE knowledge SET source_id = NULL WHERE source_id = ?1",
            params![id],
        )?;
        let n = tx.execute("DELETE FROM sources WHERE id = ?1", params![id])?;
        tx.execute(
            "UPDATE domains SET doc_count = MAX(doc_count - 1, 0), last_updated = ?2 WHERE name = ?1",
            params![domain, Utc::now().timestamp()],
        )?;
        tx.commit()?;
        Ok(n > 0)
    }

    /// List ingested sources for a domain (or all domains if None). Newest first, limit 100.
    pub fn list_sources(&self, domain: Option<&str>) -> Result<Vec<SourceRow>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(
            "SELECT id, kind, uri, title, domain, fetched_at, trust
               FROM sources
              WHERE (?1 IS NULL OR domain = ?1)
              ORDER BY fetched_at DESC
              LIMIT 100",
        )?;
        let rows = stmt.query_map(params![domain], |r| {
            Ok(SourceRow {
                id: r.get(0)?,
                kind: r.get(1)?,
                uri: r.get(2)?,
                title: r.get(3)?,
                domain: r.get(4)?,
                fetched_at: r.get(5)?,
                trust: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn clear_all_sources(&self) -> Result<u64> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        tx.execute(
            "UPDATE knowledge SET source_id = NULL WHERE source_id IS NOT NULL",
            [],
        )?;
        tx.execute("DELETE FROM chunks_fts", [])?;
        tx.execute("DELETE FROM chunks", [])?;
        let n = tx.execute("DELETE FROM sources", [])?;
        tx.execute("UPDATE domains SET doc_count = 0", [])?;
        tx.commit()?;
        Ok(n as u64)
    }
}

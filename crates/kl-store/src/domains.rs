//! Domain registration/listing, per-domain clearing, taxonomy stages, and the
//! marketplace submission (publish) queue.

use anyhow::Result;
use chrono::Utc;
use kl_core::{DomainRow, Kind, MarketplaceItem, StageRow, SubmissionRow};
use rusqlite::{params, OptionalExtension};

use crate::{clamp_retention, Store};

impl Store {
    // ---- registries (drive the router) -----------------------------------

    /// Register or update a domain.
    ///
    /// `retention_days` is tri-state to distinguish "leave whatever it
    /// currently is alone" from "explicitly clear back to no-expiration":
    /// `None` = don't touch; `Some(None)` = clear to no-expiration;
    /// `Some(Some(n))` = set to `n` days (clamped to `max_retention_days`
    /// if that ceiling is provided and `n` exceeds it — clamped rather than
    /// rejected, so a caller requesting an overly long retention still gets
    /// a working policy instead of an error).
    #[allow(clippy::too_many_arguments)]
    pub fn register_domain(
        &self,
        name: &str,
        description: Option<&str>,
        query_hint: Option<&str>,
        enforced: Option<bool>,
        redact_enabled: Option<bool>,
        retention_days: Option<Option<i64>>,
        is_marketplace_template: Option<bool>,
        max_retention_days: Option<i64>,
    ) -> Result<()> {
        let now = Utc::now().timestamp();
        let c = self.conn.lock().unwrap();
        let enforced_i = enforced.map(|b| b as i64);
        let redact_enabled_i = redact_enabled.map(|b| b as i64);
        let is_marketplace_template_i = is_marketplace_template.map(|b| b as i64);
        let (retention_touched, retention_value) = match retention_days {
            None => (0i64, None),
            Some(v) => (1i64, clamp_retention(v, max_retention_days)),
        };
        c.execute(
            "INSERT INTO domains (name, description, query_hint, enforced, redact_enabled, retention_days, is_marketplace_template, last_updated)
             VALUES (?1, ?2, ?3, COALESCE(?4, 0), COALESCE(?5, 1), ?6, COALESCE(?7, 0), ?8)
             ON CONFLICT(name) DO UPDATE SET
               description = COALESCE(excluded.description, domains.description),
               query_hint  = COALESCE(excluded.query_hint,  domains.query_hint),
               enforced    = COALESCE(?4, domains.enforced),
               redact_enabled = COALESCE(?5, domains.redact_enabled),
               retention_days = CASE WHEN ?9 = 1 THEN ?6 ELSE domains.retention_days END,
               is_marketplace_template = COALESCE(?7, domains.is_marketplace_template),
               last_updated = excluded.last_updated",
            params![
                name,
                description,
                query_hint,
                enforced_i,
                redact_enabled_i,
                retention_value,
                is_marketplace_template_i,
                now,
                retention_touched,
            ],
        )?;
        Ok(())
    }

    pub fn list_domains(&self) -> Result<Vec<DomainRow>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(
            "SELECT d.name, d.description, d.query_hint, d.doc_count,
                    (SELECT COUNT(*) FROM knowledge k WHERE k.domain = d.name AND k.kind = 'rule'
                       AND k.trust IN ('reviewed','user')) AS rule_count,
                    d.last_updated, d.enforced, d.retention_days, d.is_marketplace_template
               FROM domains d ORDER BY d.name",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(DomainRow {
                name: r.get(0)?,
                description: r.get(1)?,
                query_hint: r.get(2)?,
                doc_count: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                rule_count: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                last_updated: r.get(5)?,
                enforced: r.get::<_, i64>(6)? != 0,
                retention_days: r.get(7)?,
                is_marketplace_template: r.get::<_, i64>(8)? != 0,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn list_stages(&self, taxonomy: &str) -> Result<Vec<StageRow>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(
            "SELECT taxonomy, name, ordinal, description FROM stages
              WHERE taxonomy = ?1 ORDER BY ordinal ASC",
        )?;
        let rows = stmt.query_map(params![taxonomy], |r| {
            Ok(StageRow {
                taxonomy: r.get(0)?,
                name: r.get(1)?,
                ordinal: r.get(2)?,
                description: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Remove all ingested chunks and sources for a domain.
    /// If `chunks_only` is false, also removes all curated knowledge (facts, rules, procedures).
    pub fn clear_domain(&self, domain: &str, chunks_only: bool) -> Result<(usize, usize)> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;

        let knowledge_deleted = if !chunks_only {
            tx.execute("DELETE FROM knowledge WHERE domain = ?1", params![domain])?
        } else {
            tx.execute(
                "UPDATE knowledge SET source_id = NULL
                 WHERE source_id IN (SELECT id FROM sources WHERE domain = ?1)",
                params![domain],
            )?;
            0
        };

        // delete FTS index entries for this domain's chunks
        tx.execute(
            "DELETE FROM chunks_fts WHERE rowid IN (SELECT id FROM chunks WHERE domain = ?1)",
            params![domain],
        )?;
        let chunks_deleted = tx.execute("DELETE FROM chunks WHERE domain = ?1", params![domain])?;
        tx.execute("DELETE FROM sources WHERE domain = ?1", params![domain])?;
        tx.execute("DELETE FROM domains WHERE name = ?1", params![domain])?;

        tx.commit()?;
        Ok((chunks_deleted, knowledge_deleted))
    }

    /// Wipe every domain and all cascading data: knowledge, chunks, sources, domains.
    pub fn clear_all_domains(&self) -> Result<u64> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        tx.execute("DELETE FROM knowledge", [])?;
        tx.execute("DELETE FROM chunks_fts", [])?;
        tx.execute("DELETE FROM chunks", [])?;
        tx.execute("DELETE FROM sources", [])?;
        let n = tx.execute("DELETE FROM domains", [])?;
        tx.commit()?;
        Ok(n as u64)
    }

    // ---- manual edits (dashboard-only) ------------------------------------

    /// Overwrite a domain's description and query_hint. Unlike register_domain
    /// (which COALESCEs and cannot clear a field), this sets the values verbatim.
    pub fn update_domain(
        &self,
        name: &str,
        description: Option<&str>,
        query_hint: Option<&str>,
        enforced: Option<bool>,
    ) -> Result<bool> {
        let now = Utc::now().timestamp();
        let c = self.conn.lock().unwrap();
        let n = if let Some(enforced) = enforced {
            c.execute(
                "UPDATE domains SET description = ?2, query_hint = ?3, enforced = ?4, last_updated = ?5 WHERE name = ?1",
                params![name, description, query_hint, enforced, now],
            )?
        } else {
            c.execute(
                "UPDATE domains SET description = ?2, query_hint = ?3, last_updated = ?4 WHERE name = ?1",
                params![name, description, query_hint, now],
            )?
        };
        Ok(n > 0)
    }

    pub fn domain_exists(&self, name: &str) -> Result<bool> {
        let c = self.conn.lock().unwrap();
        let found: Option<String> = c
            .query_row(
                "SELECT name FROM domains WHERE name = ?1",
                params![name],
                |r| r.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    // ---- marketplace publish queue (submissions) --------------------------

    /// File a marketplace publish request at status='pending'. `items_json` is the
    /// serialized Vec<MarketplaceItem> snapshot (serialization happens in the caller).
    pub fn create_submission(
        &self,
        slug: &str,
        description: Option<&str>,
        query_hint: Option<&str>,
        items_json: &str,
        author: Option<&str>,
    ) -> Result<i64> {
        let now = Utc::now().timestamp();
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO submissions (slug, description, query_hint, items_json, status, submitted_at, author)
             VALUES (?1,?2,?3,?4,'pending',?5,?6)",
            params![slug, description, query_hint, items_json, now, author],
        )?;
        Ok(c.last_insert_rowid())
    }

    /// List submissions, newest first, optionally filtered by status.
    pub fn list_submissions(&self, status: Option<&str>) -> Result<Vec<SubmissionRow>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(
            "SELECT id, slug, description, query_hint, items_json, status, note, submitted_at, reviewed_at, author
               FROM submissions
              WHERE (?1 IS NULL OR status = ?1)
              ORDER BY submitted_at DESC, id DESC
              LIMIT 200",
        )?;
        let rows = stmt.query_map(params![status], submission_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Fetch one submission's summary row plus its raw items_json (for the review panel).
    pub fn get_submission(&self, id: i64) -> Result<Option<(SubmissionRow, String)>> {
        let c = self.conn.lock().unwrap();
        let row = c
            .query_row(
                "SELECT id, slug, description, query_hint, items_json, status, note, submitted_at, reviewed_at, author
                   FROM submissions WHERE id = ?1",
                params![id],
                |r| Ok((submission_from_row(r)?, r.get::<_, String>(4)?)),
            )
            .optional()?;
        Ok(row)
    }

    /// Withdraw a submission from the local queue. Returns true if a row was removed.
    pub fn delete_submission(&self, id: i64) -> Result<bool> {
        let c = self.conn.lock().unwrap();
        let n = c.execute("DELETE FROM submissions WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    /// Record an admin review decision: status becomes 'approved' or 'denied'.
    pub fn set_submission_status(&self, id: i64, status: &str, note: Option<&str>) -> Result<bool> {
        let now = Utc::now().timestamp();
        let c = self.conn.lock().unwrap();
        let n = c.execute(
            "UPDATE submissions SET status = ?2, note = ?3, reviewed_at = ?4 WHERE id = ?1",
            params![id, status, note, now],
        )?;
        Ok(n > 0)
    }

    /// Snapshot a domain's enforceable (reviewed + user) knowledge as marketplace
    /// items. Uses a dedicated select because KnowledgeRow drops trigger/remediation.
    pub fn export_domain_items(&self, domain: &str) -> Result<Vec<MarketplaceItem>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(
            "SELECT kind, stage, title, body, trigger, severity, remediation
               FROM knowledge
              WHERE domain = ?1 AND trust IN ('reviewed','user')
              ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![domain], |r| {
            let kind_s: String = r.get(0)?;
            Ok(MarketplaceItem {
                kind: Kind::parse(&kind_s).unwrap_or(Kind::Fact),
                stage: r.get(1)?,
                title: r.get(2)?,
                body: r.get(3)?,
                trigger: r.get(4)?,
                severity: r.get(5)?,
                remediation: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

fn submission_from_row(r: &rusqlite::Row) -> rusqlite::Result<SubmissionRow> {
    let items_json: String = r.get(4)?;
    let item_count = serde_json::from_str::<serde_json::Value>(&items_json)
        .ok()
        .and_then(|v| v.as_array().map(|a| a.len()))
        .unwrap_or(0) as i64;
    Ok(SubmissionRow {
        id: r.get(0)?,
        slug: r.get(1)?,
        description: r.get(2)?,
        query_hint: r.get(3)?,
        item_count,
        status: r.get(5)?,
        note: r.get(6)?,
        submitted_at: r.get(7)?,
        reviewed_at: r.get(8)?,
        author: r.get(9)?,
    })
}

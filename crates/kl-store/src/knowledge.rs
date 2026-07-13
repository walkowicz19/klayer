//! Curated knowledge item CRUD (facts/rules/procedures), hybrid retrieval
//! (`recall`), conflict tracking, per-item retention overrides, and the
//! DB-backed media-attachment rows (distinct from `media.rs`, which is the
//! filesystem storage backend for the underlying bytes).

use anyhow::Result;
use chrono::Utc;
use kl_core::{Kind, KnowledgeItemWithSource, KnowledgeRow, MediaRow, RecallHit, Trust};
use rusqlite::{params, OptionalExtension};

use crate::{clamp_retention, Store};

impl Store {
    // ---- curated knowledge tier ------------------------------------------

    /// User-authored fact: highest trust, immediately enforceable.
    pub fn remember(&self, domain: &str, statement: &str) -> Result<i64> {
        self.insert_knowledge(
            Kind::Fact,
            domain,
            None,
            statement,
            statement,
            None,
            None,
            None,
            None,
            Trust::User,
        )
    }

    /// LLM-extracted candidate: stored as `proposed`, NOT enforced until promoted.
    #[allow(clippy::too_many_arguments)]
    pub fn propose(
        &self,
        kind: Kind,
        domain: &str,
        stage: Option<&str>,
        title: &str,
        body: &str,
        trigger: Option<&str>,
        severity: Option<&str>,
        remediation: Option<&str>,
        source_id: Option<i64>,
    ) -> Result<i64> {
        self.insert_knowledge(
            kind,
            domain,
            stage,
            title,
            body,
            trigger,
            severity,
            remediation,
            source_id,
            Trust::Proposed,
        )
    }

    /// The validation gate: promote a proposed item to `reviewed` (enforceable).
    pub fn promote(&self, id: i64) -> Result<bool> {
        let now = Utc::now().timestamp();
        let c = self.conn.lock().unwrap();
        let n = c.execute(
            "UPDATE knowledge SET trust = 'reviewed', updated_at = ?2
             WHERE id = ?1 AND trust = 'proposed'",
            params![id, now],
        )?;
        Ok(n > 0)
    }

    pub fn forget(&self, id: i64) -> Result<bool> {
        let c = self.conn.lock().unwrap();
        let n = c.execute("DELETE FROM knowledge WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_knowledge(
        &self,
        kind: Kind,
        domain: &str,
        stage: Option<&str>,
        title: &str,
        body: &str,
        trigger: Option<&str>,
        severity: Option<&str>,
        remediation: Option<&str>,
        source_id: Option<i64>,
        trust: Trust,
    ) -> Result<i64> {
        let now = Utc::now().timestamp();
        // Redaction must happen before the INSERT and before the conflict-
        // detection query below, since that query matches on title/body text
        // and must never compare against (or persist) raw PII.
        let (title, body) = if self.domain_redact_enabled(domain)? {
            (
                kl_core::redact::redact(title),
                kl_core::redact::redact(body),
            )
        } else {
            (title.to_string(), body.to_string())
        };
        let title = title.as_str();
        let body = body.as_str();
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO knowledge
               (kind, domain, stage, title, body, trigger, severity, remediation, source_id, trust, created_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?11)",
            params![kind.as_str(), domain, stage, title, body, trigger, severity, remediation, source_id, trust.as_str(), now],
        )?;
        let id = c.last_insert_rowid();
        {
            let prior: Option<i64> = c
                .query_row(
                    "SELECT id FROM knowledge WHERE domain=?1 AND lower(title)=lower(?2)
                 AND id<>?3 AND trust IN ('reviewed','user') AND body<>?4
                 ORDER BY updated_at DESC LIMIT 1",
                    params![domain, title, id, body],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(old_id) = prior {
                c.execute("UPDATE knowledge SET conflict_with_id=?1, conflict_status='open', updated_at=?3 WHERE id=?2", params![id, old_id, now])?;
                c.execute("UPDATE knowledge SET conflict_with_id=?1, conflict_status='open', updated_at=?3 WHERE id=?2", params![old_id, id, now])?;
            }
        }
        Ok(id)
    }

    // ---- retrieval --------------------------------------------------------

    /// Hybrid-ish recall: FTS over the reference tier + a LIKE pass over curated
    /// knowledge, merged and trust-ranked. Every hit carries provenance + trust.
    pub fn recall(
        &self,
        domain: &str,
        query: &str,
        kind: Option<Kind>,
        k: usize,
    ) -> Result<Vec<RecallHit>> {
        let c = self.conn.lock().unwrap();
        let mut hits: Vec<RecallHit> = Vec::new();

        // 1) reference tier via FTS5 (bm25: lower is better)
        let match_expr = fts_match(query);
        if !match_expr.is_empty() {
            let mut stmt = c.prepare(
                "SELECT c.text, s.uri, s.fetched_at, bm25(chunks_fts) AS score
                   FROM chunks_fts
                   JOIN chunks c ON c.id = chunks_fts.rowid
                   JOIN sources s ON s.id = c.source_id
                  WHERE chunks_fts MATCH ?1 AND c.domain = ?2
                  ORDER BY score ASC
                  LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![match_expr, domain, k as i64], |r| {
                Ok(RecallHit {
                    source_kind: "chunk".into(),
                    kind: None,
                    title: String::new(),
                    body: r.get::<_, String>(0)?,
                    domain: domain.to_string(),
                    trust: "untrusted".into(),
                    enforceable: false,
                    provenance: r.get::<_, Option<String>>(1)?,
                    fetched_at: r.get::<_, Option<i64>>(2)?,
                    score: r.get::<_, f64>(3)?,
                })
            })?;
            for row in rows {
                hits.push(row?);
            }
        }

        // 2) curated knowledge via LIKE — match ANY query term, not the whole
        // phrase. Natural-language queries are the intended input to recall(), so
        // a single `%full query%` substring would almost never hit a curated rule.
        // Tokenize like the FTS pass and OR a per-term (title OR body) LIKE.
        let terms: Vec<String> = query
            .split_whitespace()
            .map(|t| t.replace('%', "").replace('_', ""))
            .filter(|t| !t.is_empty())
            .map(|t| format!("%{t}%"))
            .collect();
        let terms = if terms.is_empty() {
            vec![format!("%{}%", query.replace('%', "").replace('_', ""))]
        } else {
            terms
        };

        // Positional params: ?1 = domain, ?2.. = one per term, then optional kind.
        let mut bind: Vec<String> = vec![domain.to_string()];
        let mut clauses = Vec::with_capacity(terms.len());
        for term in &terms {
            let idx = bind.len() + 1;
            clauses.push(format!("(title LIKE ?{idx} OR body LIKE ?{idx})"));
            bind.push(term.clone());
        }
        let mut sql = format!(
            "SELECT kind, title, body, trust, source_id, created_at
               FROM knowledge
              WHERE domain = ?1 AND ({})",
            clauses.join(" OR ")
        );
        if let Some(kd) = kind {
            let idx = bind.len() + 1;
            sql.push_str(&format!(" AND kind = ?{idx}"));
            bind.push(kd.as_str().to_string());
        }
        sql.push_str(" ORDER BY (CASE trust WHEN 'user' THEN 3 WHEN 'reviewed' THEN 2 WHEN 'proposed' THEN 1 ELSE 0 END) DESC, updated_at DESC LIMIT 50");

        let mut stmt = c.prepare(&sql)?;
        let map = |r: &rusqlite::Row| -> rusqlite::Result<RecallHit> {
            let trust_s: String = r.get(3)?;
            Ok(RecallHit {
                source_kind: "knowledge".into(),
                kind: Some(r.get::<_, String>(0)?),
                title: r.get::<_, String>(1)?,
                body: r.get::<_, String>(2)?,
                domain: domain.to_string(),
                enforceable: Trust::parse(&trust_s).is_enforceable(),
                trust: trust_s,
                provenance: r
                    .get::<_, Option<i64>>(4)?
                    .map(|id| format!("knowledge:source#{id}")),
                fetched_at: r.get::<_, Option<i64>>(5)?,
                score: 0.0,
            })
        };
        let rows = stmt
            .query_map(rusqlite::params_from_iter(bind.iter()), map)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        hits.extend(rows);

        // trust first, then bm25 score for chunks
        hits.sort_by(|a, b| {
            let ta = Trust::parse(&a.trust).rank();
            let tb = Trust::parse(&b.trust).rank();
            tb.cmp(&ta).then(
                a.score
                    .partial_cmp(&b.score)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
        });
        hits.truncate(k.max(1));
        Ok(hits)
    }

    // ---- knowledge listing ------------------------------------------------

    /// List knowledge items with optional trust and kind filters.
    /// Returns up to 100 rows newest-first so callers can page if needed.
    pub fn list_knowledge(
        &self,
        domain: &str,
        trust: Option<&str>,
        kind: Option<Kind>,
    ) -> Result<Vec<KnowledgeRow>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(
            "SELECT id, kind, domain, stage, title, body, trust, severity, created_at, updated_at,
                    conflict_with_id, conflict_status, retention_days
               FROM knowledge
              WHERE domain = ?1
                AND (?2 IS NULL OR trust = ?2)
                AND (?3 IS NULL OR kind = ?3)
              ORDER BY updated_at DESC
              LIMIT 100",
        )?;
        let kind_str = kind.map(|k| k.as_str().to_string());
        let rows = stmt.query_map(params![domain, trust, kind_str], |r| {
            let trust_s: String = r.get(6)?;
            Ok(KnowledgeRow {
                id: r.get(0)?,
                kind: r.get(1)?,
                domain: r.get(2)?,
                stage: r.get(3)?,
                title: r.get(4)?,
                body: r.get(5)?,
                enforceable: Trust::parse(&trust_s).is_enforceable(),
                trust: trust_s,
                severity: r.get(7)?,
                created_at: r.get(8)?,
                updated_at: r.get(9)?,
                conflict_with_id: r.get(10)?,
                conflict_status: r.get(11)?,
                retention_days: r.get(12)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Like `list_knowledge` but unpaginated (no LIMIT) — used by the
    /// retention sweep, which must see every item in a domain, not just the
    /// newest 100.
    pub(crate) fn list_knowledge_all(&self, domain: &str) -> Result<Vec<KnowledgeRow>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(
            "SELECT id, kind, domain, stage, title, body, trust, severity, created_at, updated_at,
                    conflict_with_id, conflict_status, retention_days
               FROM knowledge
              WHERE domain = ?1",
        )?;
        let rows = stmt.query_map(params![domain], |r| {
            let trust_s: String = r.get(6)?;
            Ok(KnowledgeRow {
                id: r.get(0)?,
                kind: r.get(1)?,
                domain: r.get(2)?,
                stage: r.get(3)?,
                title: r.get(4)?,
                body: r.get(5)?,
                enforceable: Trust::parse(&trust_s).is_enforceable(),
                trust: trust_s,
                severity: r.get(7)?,
                created_at: r.get(8)?,
                updated_at: r.get(9)?,
                conflict_with_id: r.get(10)?,
                conflict_status: r.get(11)?,
                retention_days: r.get(12)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Fetch a single knowledge item by id, with its source title/uri (if any)
    /// for compliance reporting. `None` if the id no longer exists (e.g. forgotten).
    pub fn get_knowledge_by_id(&self, id: i64) -> Result<Option<KnowledgeItemWithSource>> {
        let c = self.conn.lock().unwrap();
        let row = c
            .query_row(
                "SELECT k.id, k.kind, k.domain, k.stage, k.title, k.body, k.trust, k.severity,
                        k.created_at, k.updated_at, k.conflict_with_id, k.conflict_status,
                        s.title, s.uri, k.retention_days
                   FROM knowledge k
                   LEFT JOIN sources s ON s.id = k.source_id
                  WHERE k.id = ?1",
                params![id],
                |r| {
                    let trust_s: String = r.get(6)?;
                    Ok(KnowledgeItemWithSource {
                        row: KnowledgeRow {
                            id: r.get(0)?,
                            kind: r.get(1)?,
                            domain: r.get(2)?,
                            stage: r.get(3)?,
                            title: r.get(4)?,
                            body: r.get(5)?,
                            enforceable: Trust::parse(&trust_s).is_enforceable(),
                            trust: trust_s,
                            severity: r.get(7)?,
                            created_at: r.get(8)?,
                            updated_at: r.get(9)?,
                            conflict_with_id: r.get(10)?,
                            conflict_status: r.get(11)?,
                            retention_days: r.get(14)?,
                        },
                        source_title: r.get(12)?,
                        source_uri: r.get(13)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn list_conflicts(&self, domain: Option<&str>) -> Result<Vec<KnowledgeRow>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(
            "SELECT id, kind, domain, stage, title, body, trust, severity, created_at, updated_at,
                    conflict_with_id, conflict_status, retention_days FROM knowledge
             WHERE conflict_status='open' AND (?1 IS NULL OR domain=?1)
             ORDER BY updated_at DESC LIMIT 200",
        )?;
        let rows = stmt.query_map(params![domain], |r| {
            let trust_s: String = r.get(6)?;
            Ok(KnowledgeRow {
                id: r.get(0)?,
                kind: r.get(1)?,
                domain: r.get(2)?,
                stage: r.get(3)?,
                title: r.get(4)?,
                body: r.get(5)?,
                trust: trust_s.clone(),
                enforceable: Trust::parse(&trust_s).is_enforceable(),
                severity: r.get(7)?,
                created_at: r.get(8)?,
                updated_at: r.get(9)?,
                conflict_with_id: r.get(10)?,
                conflict_status: r.get(11)?,
                retention_days: r.get(12)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn resolve_conflict(&self, id: i64, action: &str) -> Result<bool> {
        let c = self.conn.lock().unwrap();
        let other: Option<i64> = c
            .query_row(
                "SELECT conflict_with_id FROM knowledge WHERE id=?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let Some(other_id) = other else {
            return Ok(false);
        };
        match action {
            "keep" => {
                c.execute(
                    "UPDATE knowledge SET conflict_status='resolved' WHERE id IN (?1,?2)",
                    params![id, other_id],
                )?;
            }
            "accept" => {
                c.execute("DELETE FROM knowledge WHERE id=?1", params![other_id])?;
                c.execute("UPDATE knowledge SET conflict_with_id=NULL, conflict_status='resolved' WHERE id=?1", params![id])?;
            }
            "merge" => {
                c.execute("UPDATE knowledge SET body=body || char(10) || ?1, conflict_with_id=NULL, conflict_status='resolved' WHERE id=?2", params![format!("Merged from knowledge #{other_id}"), id])?;
                c.execute("DELETE FROM knowledge WHERE id=?1", params![other_id])?;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Set (or clear) the per-item retention override on a knowledge row.
    /// Same tri-state shape and clamping behavior as `register_domain`'s
    /// `retention_days`. Returns `false` if `id` doesn't exist.
    pub fn set_knowledge_retention(
        &self,
        id: i64,
        retention_days: Option<i64>,
        max_retention_days: Option<i64>,
    ) -> Result<bool> {
        let value = clamp_retention(retention_days, max_retention_days);
        let c = self.conn.lock().unwrap();
        let n = c.execute(
            "UPDATE knowledge SET retention_days = ?1 WHERE id = ?2",
            params![value, id],
        )?;
        Ok(n > 0)
    }

    pub fn clear_all_knowledge(&self) -> Result<u64> {
        let c = self.conn.lock().unwrap();
        let n = c.execute("DELETE FROM knowledge", [])?;
        Ok(n as u64)
    }

    /// Edit a knowledge item's fields in place. Trust and domain are unchanged.
    #[allow(clippy::too_many_arguments)]
    pub fn update_knowledge(
        &self,
        id: i64,
        title: &str,
        body: &str,
        stage: Option<&str>,
        trigger: Option<&str>,
        severity: Option<&str>,
        remediation: Option<&str>,
    ) -> Result<bool> {
        let now = Utc::now().timestamp();
        let c = self.conn.lock().unwrap();
        let n = c.execute(
            "UPDATE knowledge
                SET title = ?2, body = ?3, stage = ?4, trigger = ?5,
                    severity = ?6, remediation = ?7, updated_at = ?8
              WHERE id = ?1",
            params![id, title, body, stage, trigger, severity, remediation, now],
        )?;
        Ok(n > 0)
    }

    // ---- media attachments (Stage G: images only) --------------------------
    //
    // These are the DB-backed rows for the `media_attachments` table. The
    // filesystem storage backend for the underlying bytes lives in `media.rs`
    // (a separate, lower-level concern: content-hash naming, allowed MIME
    // types) — deliberately not merged with these DB methods.

    /// Insert a media row. If `knowledge_id` is given, the media inherits that
    /// item's current trust tier immediately; otherwise it is standalone and
    /// `trust` stays NULL until `attach_media` links it later.
    pub fn insert_media(
        &self,
        storage_ref: &str,
        mime_type: &str,
        size_bytes: i64,
        caption: Option<&str>,
        knowledge_id: Option<i64>,
        domain: Option<&str>,
    ) -> Result<i64> {
        let now = Utc::now().timestamp();
        let c = self.conn.lock().unwrap();
        let trust: Option<String> = match knowledge_id {
            Some(kid) => c
                .query_row(
                    "SELECT trust FROM knowledge WHERE id = ?1",
                    params![kid],
                    |r| r.get(0),
                )
                .optional()?,
            None => None,
        };
        c.execute(
            "INSERT INTO media_attachments
               (storage_ref, mime_type, size_bytes, caption, knowledge_id, domain, trust, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                storage_ref,
                mime_type,
                size_bytes,
                caption,
                knowledge_id,
                domain,
                trust,
                now
            ],
        )?;
        Ok(c.last_insert_rowid())
    }

    /// Link previously-standalone media to a knowledge item, inheriting that
    /// item's current trust tier. Returns false if `knowledge_id` or
    /// `media_id` does not exist.
    pub fn attach_media(&self, media_id: i64, knowledge_id: i64) -> Result<bool> {
        let c = self.conn.lock().unwrap();
        let trust: Option<String> = c
            .query_row(
                "SELECT trust FROM knowledge WHERE id = ?1",
                params![knowledge_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(trust) = trust else {
            return Ok(false);
        };
        let n = c.execute(
            "UPDATE media_attachments SET knowledge_id = ?1, trust = ?2 WHERE media_id = ?3",
            params![knowledge_id, trust, media_id],
        )?;
        Ok(n > 0)
    }

    /// List media, newest first, optionally filtered by domain and/or knowledge_id.
    pub fn list_media(
        &self,
        domain: Option<&str>,
        knowledge_id: Option<i64>,
    ) -> Result<Vec<MediaRow>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(
            "SELECT media_id, storage_ref, mime_type, size_bytes, caption, knowledge_id, domain, trust, created_at
               FROM media_attachments
              WHERE (?1 IS NULL OR domain = ?1)
                AND (?2 IS NULL OR knowledge_id = ?2)
              ORDER BY created_at DESC
              LIMIT 100",
        )?;
        let rows = stmt.query_map(params![domain, knowledge_id], media_row_from)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn get_media(&self, media_id: i64) -> Result<Option<MediaRow>> {
        let c = self.conn.lock().unwrap();
        let row = c
            .query_row(
                "SELECT media_id, storage_ref, mime_type, size_bytes, caption, knowledge_id, domain, trust, created_at
                   FROM media_attachments WHERE media_id = ?1",
                params![media_id],
                media_row_from,
            )
            .optional()?;
        Ok(row)
    }
}

fn media_row_from(r: &rusqlite::Row) -> rusqlite::Result<MediaRow> {
    Ok(MediaRow {
        media_id: r.get(0)?,
        storage_ref: r.get(1)?,
        mime_type: r.get(2)?,
        size_bytes: r.get(3)?,
        caption: r.get(4)?,
        knowledge_id: r.get(5)?,
        domain: r.get(6)?,
        trust: r.get(7)?,
        created_at: r.get(8)?,
    })
}

/// Build a safe FTS5 MATCH expression: each whitespace token quoted, OR-joined.
fn fts_match(query: &str) -> String {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|t| t.replace('"', ""))
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{t}\""))
        .collect();
    terms.join(" OR ")
}

#[cfg(test)]
mod media_tests {
    use super::*;

    fn fixture() -> Store {
        let store = Store::open(":memory:").expect("open in-memory store");
        store.migrate().expect("migrate");
        store
    }

    #[test]
    fn insert_media_standalone_has_no_trust() {
        let store = fixture();
        let id = store
            .insert_media(
                "/tmp/media/abc.png",
                "image/png",
                123,
                Some("a screenshot"),
                None,
                Some("secure-coding"),
            )
            .unwrap();
        let row = store.get_media(id).unwrap().unwrap();
        assert_eq!(row.trust, None);
        assert_eq!(row.domain.as_deref(), Some("secure-coding"));
        assert_eq!(row.size_bytes, 123);
    }

    #[test]
    fn insert_media_with_knowledge_id_inherits_trust() {
        let store = fixture();
        let kid = store
            .remember("secure-coding", "always validate input")
            .unwrap();
        let mid = store
            .insert_media("/tmp/media/def.png", "image/png", 42, None, Some(kid), None)
            .unwrap();
        let row = store.get_media(mid).unwrap().unwrap();
        assert_eq!(row.trust.as_deref(), Some("user"));
        assert_eq!(row.knowledge_id, Some(kid));
    }

    #[test]
    fn attach_media_updates_trust_from_standalone() {
        let store = fixture();
        let mid = store
            .insert_media("/tmp/media/ghi.png", "image/png", 10, None, None, Some("d"))
            .unwrap();
        assert_eq!(store.get_media(mid).unwrap().unwrap().trust, None);

        let kid = store.remember("d", "some fact").unwrap();
        let ok = store.attach_media(mid, kid).unwrap();
        assert!(ok);
        let row = store.get_media(mid).unwrap().unwrap();
        assert_eq!(row.trust.as_deref(), Some("user"));
        assert_eq!(row.knowledge_id, Some(kid));
    }

    #[test]
    fn attach_media_returns_false_for_missing_knowledge() {
        let store = fixture();
        let mid = store
            .insert_media("/tmp/media/jkl.png", "image/png", 10, None, None, None)
            .unwrap();
        let ok = store.attach_media(mid, 99999).unwrap();
        assert!(!ok);
    }

    #[test]
    fn list_media_filters_by_domain_and_knowledge_id() {
        let store = fixture();
        let kid = store.remember("d1", "fact").unwrap();
        let m1 = store
            .insert_media("/tmp/1.png", "image/png", 1, None, Some(kid), None)
            .unwrap();
        let m2 = store
            .insert_media("/tmp/2.png", "image/png", 2, None, None, Some("d2"))
            .unwrap();

        let by_domain = store.list_media(Some("d2"), None).unwrap();
        assert_eq!(by_domain.len(), 1);
        assert_eq!(by_domain[0].media_id, m2);

        let by_knowledge = store.list_media(None, Some(kid)).unwrap();
        assert_eq!(by_knowledge.len(), 1);
        assert_eq!(by_knowledge[0].media_id, m1);

        let all = store.list_media(None, None).unwrap();
        assert_eq!(all.len(), 2);
    }
}

#[cfg(test)]
mod redact_tests {
    use super::*;

    fn fixture() -> Store {
        let store = Store::open(":memory:").expect("open in-memory store");
        store.migrate().expect("migrate");
        store
    }

    #[test]
    fn remember_redacts_by_default() {
        let store = fixture();
        let id = store
            .remember("pii-domain", "reach me at leak@example.com anytime")
            .unwrap();
        let items = store.list_knowledge("pii-domain", None, None).unwrap();
        let row = items.into_iter().find(|r| r.id == id).unwrap();
        assert!(row.body.contains("[REDACTED:EMAIL]"));
        assert!(!row.body.contains("leak@example.com"));
    }

    #[test]
    fn propose_redacts_by_default() {
        let store = fixture();
        let id = store
            .propose(
                Kind::Fact,
                "pii-domain",
                None,
                "card on file",
                "card number is 4111-1111-1111-1111",
                None,
                None,
                None,
                None,
            )
            .unwrap();
        let items = store.list_knowledge("pii-domain", None, None).unwrap();
        let row = items.into_iter().find(|r| r.id == id).unwrap();
        assert!(row.body.contains("[REDACTED:CARD]"));
        assert!(!row.body.contains("4111"));
    }

    #[test]
    fn redact_disabled_domain_stores_raw_text() {
        let store = fixture();
        store
            .register_domain(
                "open-domain",
                None,
                None,
                None,
                Some(false),
                None,
                None,
                None,
            )
            .unwrap();
        let id = store
            .remember("open-domain", "reach me at leak@example.com anytime")
            .unwrap();
        let items = store.list_knowledge("open-domain", None, None).unwrap();
        let row = items.into_iter().find(|r| r.id == id).unwrap();
        assert!(row.body.contains("leak@example.com"));
        assert!(!row.body.contains("[REDACTED"));
    }

    #[test]
    fn add_chunks_redacts_by_default() {
        let store = fixture();
        let source_id = store.add_source("web", None, None, "pii-domain").unwrap();
        store
            .add_chunks(
                source_id,
                "pii-domain",
                &["call +1-555-123-4567 for support".to_string()],
            )
            .unwrap();
        let c = store.conn.lock().unwrap();
        let text: String = c
            .query_row(
                "SELECT text FROM chunks WHERE source_id = ?1",
                params![source_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(text.contains("[REDACTED:PHONE]"));
        assert!(!text.contains("555-123-4567"));
    }

    #[test]
    fn domain_redact_enabled_defaults_true_for_unknown_domain() {
        let store = fixture();
        assert!(store.domain_redact_enabled("never-registered").unwrap());
    }
}

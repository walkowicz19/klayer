//! kl-store — SQLite persistence and retrieval for klayer.
//!
//! Concurrency model: a single `Connection` behind a `Mutex`. All DB work is
//! synchronous and never held across an `.await`, so this is Send + Sync and
//! safe to share via `Arc` inside the async MCP server. For higher concurrency,
//! swap in a pool (r2d2) — the public API here would not change.

use std::sync::Mutex;

use anyhow::{Context, Result};
use chrono::Utc;
use kl_core::{DomainRow, EpisodeRow, Kind, KnowledgeRow, RecallHit, SourceRow, StageRow, Trust};
use rusqlite::{params, Connection, OptionalExtension};

const MIGRATION: &str = include_str!("migrations/0001_init.sql");

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening db at {path}"))?;
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "foreign_keys", "ON").ok();
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn migrate(&self) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute_batch(MIGRATION).context("running migration 0001")?;
        Ok(())
    }

    // ---- ingestion (reference tier) --------------------------------------

    pub fn add_source(&self, kind: &str, uri: Option<&str>, title: Option<&str>, domain: &str) -> Result<i64> {
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
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        for (ord, text) in chunks.iter().enumerate() {
            tx.execute(
                "INSERT INTO chunks (source_id, domain, ord, text) VALUES (?1, ?2, ?3, ?4)",
                params![source_id, domain, ord as i64, text],
            )?;
            let id = tx.last_insert_rowid();
            tx.execute("INSERT INTO chunks_fts (rowid, text) VALUES (?1, ?2)", params![id, text])?;
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

    // ---- curated knowledge tier ------------------------------------------

    /// User-authored fact: highest trust, immediately enforceable.
    pub fn remember(&self, domain: &str, statement: &str) -> Result<i64> {
        self.insert_knowledge(Kind::Fact, domain, None, statement, statement, None, None, None, None, Trust::User)
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
        self.insert_knowledge(kind, domain, stage, title, body, trigger, severity, remediation, source_id, Trust::Proposed)
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

    /// Remove all ingested chunks and sources for a domain.
    /// If `chunks_only` is false, also removes all curated knowledge (facts, rules, procedures).
    pub fn clear_domain(&self, domain: &str, chunks_only: bool) -> Result<(usize, usize)> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;

        // delete FTS index entries for this domain's chunks
        tx.execute(
            "DELETE FROM chunks_fts WHERE rowid IN (SELECT id FROM chunks WHERE domain = ?1)",
            params![domain],
        )?;
        let chunks_deleted = tx.execute("DELETE FROM chunks WHERE domain = ?1", params![domain])?;
        tx.execute("DELETE FROM sources WHERE domain = ?1", params![domain])?;
        tx.execute("DELETE FROM domains WHERE name = ?1", params![domain])?;

        let knowledge_deleted = if !chunks_only {
            tx.execute("DELETE FROM knowledge WHERE domain = ?1", params![domain])?
        } else {
            0
        };

        tx.commit()?;
        Ok((chunks_deleted, knowledge_deleted))
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
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO knowledge
               (kind, domain, stage, title, body, trigger, severity, remediation, source_id, trust, created_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?11)",
            params![kind.as_str(), domain, stage, title, body, trigger, severity, remediation, source_id, trust.as_str(), now],
        )?;
        Ok(c.last_insert_rowid())
    }

    // ---- retrieval --------------------------------------------------------

    /// Hybrid-ish recall: FTS over the reference tier + a LIKE pass over curated
    /// knowledge, merged and trust-ranked. Every hit carries provenance + trust.
    pub fn recall(&self, domain: &str, query: &str, kind: Option<Kind>, k: usize) -> Result<Vec<RecallHit>> {
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
                provenance: r.get::<_, Option<i64>>(4)?.map(|id| format!("knowledge:source#{id}")),
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
            tb.cmp(&ta).then(a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal))
        });
        hits.truncate(k.max(1));
        Ok(hits)
    }

    // ---- knowledge listing ------------------------------------------------

    /// List knowledge items with optional trust and kind filters.
    /// Returns up to 100 rows newest-first so callers can page if needed.
    pub fn list_knowledge(&self, domain: &str, trust: Option<&str>, kind: Option<Kind>) -> Result<Vec<KnowledgeRow>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(
            "SELECT id, kind, domain, stage, title, body, trust, severity, created_at, updated_at
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
            })
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

    // ---- registries (drive the router) -----------------------------------

    pub fn register_domain(&self, name: &str, description: Option<&str>, query_hint: Option<&str>) -> Result<()> {
        let now = Utc::now().timestamp();
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO domains (name, description, query_hint, last_updated)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(name) DO UPDATE SET
               description = COALESCE(excluded.description, domains.description),
               query_hint  = COALESCE(excluded.query_hint,  domains.query_hint),
               last_updated = excluded.last_updated",
            params![name, description, query_hint, now],
        )?;
        Ok(())
    }

    pub fn list_domains(&self) -> Result<Vec<DomainRow>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(
            "SELECT d.name, d.description, d.query_hint, d.doc_count,
                    (SELECT COUNT(*) FROM knowledge k WHERE k.domain = d.name AND k.kind = 'rule'
                       AND k.trust IN ('reviewed','user')) AS rule_count,
                    d.last_updated
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
            Ok(StageRow { taxonomy: r.get(0)?, name: r.get(1)?, ordinal: r.get(2)?, description: r.get(3)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ---- agentic episodic memory -----------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub fn log_episode(
        &self,
        run_id: &str,
        step: i64,
        stage: Option<&str>,
        action: Option<&str>,
        observation: Option<&str>,
        outcome: Option<&str>,
    ) -> Result<i64> {
        let now = Utc::now().timestamp();
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO episodes (run_id, step, stage, action, observation, outcome, ts)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![run_id, step, stage, action, observation, outcome, now],
        )?;
        Ok(c.last_insert_rowid())
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

    /// List agentic run episodes. Filter by run_id if provided. Newest first, limit 200.
    pub fn list_episodes(&self, run_id: Option<&str>) -> Result<Vec<EpisodeRow>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(
            "SELECT id, run_id, step, stage, action, observation, outcome, ts
               FROM episodes
              WHERE (?1 IS NULL OR run_id = ?1)
              ORDER BY ts DESC, step DESC
              LIMIT 200",
        )?;
        let rows = stmt.query_map(params![run_id], |r| {
            Ok(EpisodeRow {
                id: r.get(0)?,
                run_id: r.get(1)?,
                step: r.get(2)?,
                stage: r.get(3)?,
                action: r.get(4)?,
                observation: r.get(5)?,
                outcome: r.get(6)?,
                ts: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Wipe every domain and all cascading data: knowledge, chunks, sources, domains.
    pub fn clear_all_domains(&self) -> Result<u64> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        tx.execute("DELETE FROM chunks_fts", [])?;
        tx.execute("DELETE FROM chunks", [])?;
        tx.execute("DELETE FROM sources", [])?;
        tx.execute("DELETE FROM knowledge", [])?;
        let n = tx.execute("DELETE FROM domains", [])?;
        tx.commit()?;
        Ok(n as u64)
    }

    pub fn clear_all_knowledge(&self) -> Result<u64> {
        let c = self.conn.lock().unwrap();
        let n = c.execute("DELETE FROM knowledge", [])?;
        Ok(n as u64)
    }

    pub fn clear_all_sources(&self) -> Result<u64> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        tx.execute("DELETE FROM chunks_fts", [])?;
        tx.execute("DELETE FROM chunks", [])?;
        let n = tx.execute("DELETE FROM sources", [])?;
        tx.execute("UPDATE domains SET doc_count = 0", [])?;
        tx.commit()?;
        Ok(n as u64)
    }

    pub fn clear_all_episodes(&self) -> Result<u64> {
        let c = self.conn.lock().unwrap();
        let n = c.execute("DELETE FROM episodes", [])?;
        Ok(n as u64)
    }

    pub fn domain_exists(&self, name: &str) -> Result<bool> {
        let c = self.conn.lock().unwrap();
        let found: Option<String> = c
            .query_row("SELECT name FROM domains WHERE name = ?1", params![name], |r| r.get(0))
            .optional()?;
        Ok(found.is_some())
    }
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

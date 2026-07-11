//! kl-store — SQLite persistence and retrieval for klayer.
//!
//! Concurrency model: a single `Connection` behind a `Mutex`. All DB work is
//! synchronous and never held across an `.await`, so this is Send + Sync and
//! safe to share via `Arc` inside the async MCP server. For higher concurrency,
//! swap in a pool (r2d2) — the public API here would not change.

pub mod media;

use std::sync::Mutex;

use anyhow::{Context, Result};
use chrono::Utc;
use kl_core::{
    DomainRow, EpisodeRow, Kind, KnowledgeItemWithSource, KnowledgeRow, MarketplaceItem, MediaRow,
    ModelRegistryRow, RecallHit, RoutingRuleRow, SourceRow, StageRow, SubmissionRow, Trust,
};
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
        ensure_column(&c, "episodes", "domain", "TEXT")?;
        // Best-effort, self-reported usage metadata (Stage D). MCP has no
        // standard field for token/cost accounting, so these are never
        // populated automatically — only when a caller passes model/
        // tokens_used/cost explicitly to remember/ingest/recall.
        ensure_column(&c, "episodes", "model", "TEXT")?;
        ensure_column(&c, "episodes", "tokens_used", "INTEGER")?;
        ensure_column(&c, "episodes", "cost", "REAL")?;
        Ok(())
    }

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
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        for (ord, text) in chunks.iter().enumerate() {
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
                    conflict_with_id, conflict_status
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
                        s.title, s.uri
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
                    conflict_with_id, conflict_status FROM knowledge
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

    pub fn register_domain(
        &self,
        name: &str,
        description: Option<&str>,
        query_hint: Option<&str>,
        enforced: Option<bool>,
    ) -> Result<()> {
        let now = Utc::now().timestamp();
        let c = self.conn.lock().unwrap();
        let enforced_i = enforced.map(|b| b as i64);
        c.execute(
            "INSERT INTO domains (name, description, query_hint, enforced, last_updated)
             VALUES (?1, ?2, ?3, COALESCE(?4, 0), ?5)
             ON CONFLICT(name) DO UPDATE SET
               description = COALESCE(excluded.description, domains.description),
               query_hint  = COALESCE(excluded.query_hint,  domains.query_hint),
               enforced    = COALESCE(?4, domains.enforced),
               last_updated = excluded.last_updated",
            params![name, description, query_hint, enforced_i, now],
        )?;
        Ok(())
    }

    /// Whether a domain has the enforced flag set. Unknown domains are treated
    /// as not enforced (default 0), matching the column's DEFAULT 0.
    pub fn domain_enforced(&self, name: &str) -> Result<bool> {
        let c = self.conn.lock().unwrap();
        Ok(c.query_row(
            "SELECT enforced FROM domains WHERE name = ?1",
            params![name],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0)
            != 0)
    }

    pub fn domain_allowed(&self, identity: Option<&str>, domain: &str) -> Result<bool> {
        let c = self.conn.lock().unwrap();
        let configured: i64 = c.query_row(
            "SELECT COUNT(*) FROM domain_permissions WHERE domain=?1",
            params![domain],
            |r| r.get(0),
        )?;
        if configured == 0 {
            return Ok(true);
        }
        let id = identity.unwrap_or("default");
        Ok(c.query_row(
            "SELECT allowed FROM domain_permissions WHERE identity=?1 AND domain=?2",
            params![id, domain],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0)
            != 0)
    }

    pub fn set_domain_permission(&self, identity: &str, domain: &str, allowed: bool) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute("INSERT INTO domain_permissions(identity,domain,allowed) VALUES(?1,?2,?3) ON CONFLICT(identity,domain) DO UPDATE SET allowed=excluded.allowed", params![identity, domain, allowed as i64])?;
        Ok(())
    }

    pub fn configure_model(
        &self,
        harness: &str,
        model_id: &str,
        tier: &str,
        cost: f64,
        sub_agent: Option<&str>,
    ) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute("INSERT INTO model_registry(harness,model_id,capability_tier,cost_weight,sub_agent_name) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(harness,model_id,sub_agent_name) DO UPDATE SET capability_tier=excluded.capability_tier,cost_weight=excluded.cost_weight", params![harness,model_id,tier,cost,sub_agent])?;
        Ok(())
    }

    pub fn add_routing_rule(
        &self,
        harness: &str,
        domain_type: &str,
        task_type: &str,
        complexity_tier: &str,
        model_id: &str,
    ) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute("INSERT INTO routing_rules(harness,domain_type,task_type,complexity_tier,model_id) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(harness,domain_type,task_type,complexity_tier) DO UPDATE SET model_id=excluded.model_id", params![harness,domain_type,task_type,complexity_tier,model_id])?;
        Ok(())
    }

    /// Delete one `model_registry` row. `sub_agent_name` is matched with `IS`
    /// (not `=`) since it is part of the composite primary key and NULL for
    /// harness-level (non-sub-agent) entries — `=` never matches NULL in SQL.
    pub fn remove_model(
        &self,
        harness: &str,
        model_id: &str,
        sub_agent_name: Option<&str>,
    ) -> Result<bool> {
        let c = self.conn.lock().unwrap();
        let n = c.execute(
            "DELETE FROM model_registry WHERE harness=?1 AND model_id=?2 AND sub_agent_name IS ?3",
            params![harness, model_id, sub_agent_name],
        )?;
        Ok(n > 0)
    }

    pub fn remove_routing_rule(
        &self,
        harness: &str,
        domain_type: &str,
        task_type: &str,
        complexity_tier: &str,
    ) -> Result<bool> {
        let c = self.conn.lock().unwrap();
        let n = c.execute(
            "DELETE FROM routing_rules WHERE harness=?1 AND domain_type=?2 AND task_type=?3 AND complexity_tier=?4",
            params![harness, domain_type, task_type, complexity_tier],
        )?;
        Ok(n > 0)
    }

    pub fn list_model_registry(&self) -> Result<Vec<ModelRegistryRow>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(
            "SELECT harness, model_id, capability_tier, cost_weight, sub_agent_name
               FROM model_registry
              ORDER BY harness, capability_tier, model_id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ModelRegistryRow {
                    harness: r.get(0)?,
                    model_id: r.get(1)?,
                    capability_tier: r.get(2)?,
                    cost_weight: r.get(3)?,
                    sub_agent_name: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn list_routing_rules(&self) -> Result<Vec<RoutingRuleRow>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(
            "SELECT harness, domain_type, task_type, complexity_tier, model_id
               FROM routing_rules
              ORDER BY harness, domain_type, task_type, complexity_tier",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(RoutingRuleRow {
                    harness: r.get(0)?,
                    domain_type: r.get(1)?,
                    task_type: r.get(2)?,
                    complexity_tier: r.get(3)?,
                    model_id: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn recommend_model(
        &self,
        harness: &str,
        domain_type: &str,
        task_type: &str,
        complexity: &str,
    ) -> Result<Option<(String, f64, String)>> {
        let c = self.conn.lock().unwrap();
        let configured: Option<(String, f64)> = c.query_row("SELECT model_id,cost_weight FROM model_registry WHERE harness=?1 AND model_id=(SELECT model_id FROM routing_rules WHERE harness=?1 AND domain_type=?2 AND task_type=?3 AND complexity_tier=?4) LIMIT 1", params![harness,domain_type,task_type,complexity], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
        if let Some((model, cost)) = configured {
            return Ok(Some((model, cost, "routing rule match".into())));
        }
        let fallback: Option<(String,f64)> = c.query_row("SELECT model_id,cost_weight FROM model_registry WHERE harness=?1 ORDER BY cost_weight ASC LIMIT 1", params![harness], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
        Ok(fallback.map(|(m, cost)| (m, cost, "no exact rule; cheapest configured model".into())))
    }

    pub fn list_domains(&self) -> Result<Vec<DomainRow>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(
            "SELECT d.name, d.description, d.query_hint, d.doc_count,
                    (SELECT COUNT(*) FROM knowledge k WHERE k.domain = d.name AND k.kind = 'rule'
                       AND k.trust IN ('reviewed','user')) AS rule_count,
                    d.last_updated, d.enforced
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

    /// `domain` is the single domain this step acted on, when there is a clear
    /// one — e.g. recall/remember/propose/execute_change. Steps without a
    /// clear single-domain target (codebase search, dataset export, ...)
    /// leave it `None`; that's expected, not a bug.
    ///
    /// `model`/`tokens_used`/`cost` are best-effort, self-reported usage
    /// metadata (Stage D): MCP carries no standard token/cost accounting
    /// field, so klayer never measures these itself — they are `None` at
    /// every call site except the handful of tools (remember/ingest/recall)
    /// that accept them as optional caller-supplied parameters.
    #[allow(clippy::too_many_arguments)]
    pub fn log_episode_auto(
        &self,
        run_id: &str,
        stage: Option<&str>,
        action: Option<&str>,
        observation: Option<&str>,
        outcome: Option<&str>,
        domain: Option<&str>,
        model: Option<&str>,
        tokens_used: Option<i64>,
        cost: Option<f64>,
    ) -> Result<i64> {
        let now = Utc::now().timestamp();
        let c = self.conn.lock().unwrap();
        let step: i64 = c
            .query_row(
                "SELECT COALESCE(MAX(step), 0) + 1 FROM episodes WHERE run_id = ?1",
                params![run_id],
                |r| r.get(0),
            )
            .unwrap_or(1);
        c.execute(
            "INSERT INTO episodes (run_id, step, stage, action, observation, outcome, ts, domain, model, tokens_used, cost)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                run_id,
                step,
                stage,
                action,
                observation,
                outcome,
                now,
                domain,
                model,
                tokens_used,
                cost
            ],
        )?;
        Ok(c.last_insert_rowid())
    }

    /// Whether a `recall` episode against `domain` already exists earlier in
    /// `run_id` — the precondition `execute_change` gates on for enforced domains.
    pub fn has_prior_recall(&self, run_id: &str, domain: &str) -> Result<bool> {
        let c = self.conn.lock().unwrap();
        let n: i64 = c.query_row(
            "SELECT COUNT(*) FROM episodes WHERE run_id = ?1 AND domain = ?2 AND stage = 'recall'",
            params![run_id, domain],
            |r| r.get(0),
        )?;
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

    /// List agentic run episodes. Filter by run_id if provided. Newest first, limit 200.
    pub fn list_episodes(&self, run_id: Option<&str>) -> Result<Vec<EpisodeRow>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(
            "SELECT id, run_id, step, stage, action, observation, outcome, ts, knowledge_ids_used, domain, model, tokens_used, cost
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
                knowledge_ids_used: serde_json::from_str::<Vec<i64>>(&r.get::<_, String>(8)?)
                    .unwrap_or_default(),
                domain: r.get(9)?,
                model: r.get(10)?,
                tokens_used: r.get(11)?,
                cost: r.get(12)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn set_episode_knowledge_ids(&self, id: i64, ids: &[i64]) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "UPDATE episodes SET knowledge_ids_used=?1 WHERE id=?2",
            params![serde_json::to_string(ids)?, id],
        )?;
        Ok(())
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

    pub fn clear_all_knowledge(&self) -> Result<u64> {
        let c = self.conn.lock().unwrap();
        let n = c.execute("DELETE FROM knowledge", [])?;
        Ok(n as u64)
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

    pub fn clear_all_episodes(&self) -> Result<u64> {
        let c = self.conn.lock().unwrap();
        let n = c.execute("DELETE FROM episodes", [])?;
        Ok(n as u64)
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

    // ---- media attachments (Stage G: images only) --------------------------

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

#[cfg(test)]
mod model_registry_tests {
    use super::*;

    fn fixture() -> Store {
        let store = Store::open(":memory:").expect("open in-memory store");
        store.migrate().expect("migrate");
        store
    }

    #[test]
    fn add_routing_rule_persists_and_is_retrievable() {
        let store = fixture();
        store
            .add_routing_rule("claude-code", "frontend", "feature", "high", "opus")
            .unwrap();
        let rules = store.list_routing_rules().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].harness, "claude-code");
        assert_eq!(rules[0].model_id, "opus");

        // Upsert: same key, new model_id replaces rather than duplicates.
        store
            .add_routing_rule("claude-code", "frontend", "feature", "high", "sonnet")
            .unwrap();
        let rules = store.list_routing_rules().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].model_id, "sonnet");
    }

    #[test]
    fn recommend_model_uses_routing_rule_once_populated() {
        let store = fixture();
        store
            .configure_model("claude-code", "opus", "heavy-reasoning", 10.0, None)
            .unwrap();
        store
            .configure_model("claude-code", "haiku", "fast-cheap", 1.0, None)
            .unwrap();
        store
            .add_routing_rule("claude-code", "frontend", "feature", "high", "opus")
            .unwrap();
        let (model, _cost, reason) = store
            .recommend_model("claude-code", "frontend", "feature", "high")
            .unwrap()
            .unwrap();
        assert_eq!(model, "opus");
        assert_eq!(reason, "routing rule match");
    }

    #[test]
    fn remove_model_deletes_only_matching_row() {
        let store = fixture();
        store
            .configure_model("claude-code", "opus", "heavy-reasoning", 10.0, None)
            .unwrap();
        store
            .configure_model(
                "claude-code",
                "opus",
                "heavy-reasoning",
                10.0,
                Some("frontend-agent"),
            )
            .unwrap();

        let removed = store.remove_model("claude-code", "opus", None).unwrap();
        assert!(removed);
        let remaining = store.list_model_registry().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(
            remaining[0].sub_agent_name.as_deref(),
            Some("frontend-agent")
        );

        let removed_again = store.remove_model("claude-code", "opus", None).unwrap();
        assert!(
            !removed_again,
            "second delete of the same row should be a no-op"
        );
    }

    #[test]
    fn remove_routing_rule_deletes_the_right_row() {
        let store = fixture();
        store
            .add_routing_rule("claude-code", "frontend", "feature", "high", "opus")
            .unwrap();
        store
            .add_routing_rule("claude-code", "backend", "crud", "low", "haiku")
            .unwrap();

        let removed = store
            .remove_routing_rule("claude-code", "frontend", "feature", "high")
            .unwrap();
        assert!(removed);
        let remaining = store.list_routing_rules().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].domain_type, "backend");
    }

    #[test]
    fn list_model_registry_groups_are_orderable_by_harness_then_tier() {
        let store = fixture();
        store
            .configure_model("cursor", "gpt", "balanced", 5.0, None)
            .unwrap();
        store
            .configure_model("claude-code", "opus", "heavy-reasoning", 10.0, None)
            .unwrap();
        store
            .configure_model("claude-code", "haiku", "fast-cheap", 1.0, None)
            .unwrap();
        let rows = store.list_model_registry().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].harness, "claude-code");
        assert_eq!(rows[1].harness, "claude-code");
        assert_eq!(rows[2].harness, "cursor");
    }
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

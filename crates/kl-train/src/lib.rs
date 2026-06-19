//! kl-train — trust-gated training-data layer.
//!
//! Turns klayer's curated knowledge and agentic audit trail into fine-tuning
//! datasets, gated by the same trust lifecycle the rest of klayer uses. It is a
//! pure *store*: it captures candidate training pairs, gates them
//! (proposed -> reviewed | user), and exports the enforceable ones as chat JSONL.
//!
//! It deliberately does NOT generate labels or verify code — a separate project
//! runs the teacher model and the verifier, then deposits rows here. This crate
//! has no LLM/HTTP dependency and never shells out to a compiler.
//!
//! Storage lives in its own SQLite DB (`KLAYER_TRAIN_DB`, default
//! `klayer_train.db`), separate from `klayer.db` and `klayer_code.db`.
//!
//! Safety spine:
//!   * Every row records `provenance ∈ {student, teacher, human}` and a
//!     `kl_core::Trust` tier.
//!   * `promote_example` refuses to promote `student` rows (model-collapse guard).
//!   * `export_dataset` emits only `reviewed` + `user` rows.

use anyhow::{Context, Result};
use chrono::Utc;
use kl_core::{EpisodeRow, KnowledgeRow, StageRow};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::collections::HashSet;
use std::sync::Mutex;

const MIGRATION: &str = include_str!("migrations/0001_init.sql");

// ── Public types ───────────────────────────────────────────────────────────────

pub struct TrainStore {
    conn: Mutex<Connection>,
}

/// A single training example row. Mirrors the `training_examples` columns.
#[derive(Debug, Serialize, Clone)]
pub struct TrainingRow {
    pub id: i64,
    pub domain: String,
    pub system_prompt: Option<String>,
    pub user_content: String,
    pub assistant_content: Option<String>,
    pub label_type: String,
    pub trust: String,
    pub provenance: String,
    pub retrieval_ref: Option<String>,
    pub verify_log: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Aggregate counts for the dashboard.
#[derive(Debug, Serialize)]
pub struct TrainStats {
    pub total: i64,
    pub proposed: i64,
    pub reviewed: i64,
    pub user: i64,
    pub stubs: i64,
}

/// Outcome of a promote attempt — lets callers distinguish the collapse-guard
/// rejection from a plain "not found / not promotable".
#[derive(Debug, PartialEq, Eq)]
pub enum PromoteOutcome {
    Promoted,
    NotFound,
    BlockedStudent,
}

/// One exported per-domain dataset file.
#[derive(Debug, Serialize)]
pub struct ExportFile {
    pub domain: String,
    pub path: String,
    pub rows: usize,
}

// ── TrainStore impl ─────────────────────────────────────────────────────────────

impl TrainStore {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening train db at {path}"))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn migrate(&self) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute_batch(MIGRATION).context("train db schema")?;
        Ok(())
    }

    // ── capture & authoring ──────────────────────────────────────────────────

    /// Insert a candidate training pair at `trust='proposed'`. Provenance is
    /// supplied by the caller (the external teacher project deposits `teacher`
    /// rows; the faucets deposit `student` stubs). Promotable only if not student.
    #[allow(clippy::too_many_arguments)]
    pub fn capture_example(
        &self,
        domain: &str,
        system_prompt: Option<&str>,
        user_content: &str,
        assistant_content: Option<&str>,
        label_type: &str,
        provenance: &str,
        retrieval_ref: Option<&str>,
        verify_log: Option<&str>,
    ) -> Result<i64> {
        self.insert_row(
            domain,
            system_prompt,
            user_content,
            assistant_content,
            label_type,
            "proposed",
            provenance,
            retrieval_ref,
            verify_log,
        )
    }

    /// Insert a human-authored pair: `trust='user'`, `provenance='human'`,
    /// exportable immediately (mirrors `Store::remember`).
    #[allow(clippy::too_many_arguments)]
    pub fn author_example(
        &self,
        domain: &str,
        system_prompt: Option<&str>,
        user_content: &str,
        assistant_content: &str,
        label_type: &str,
        retrieval_ref: Option<&str>,
        verify_log: Option<&str>,
    ) -> Result<i64> {
        self.insert_row(
            domain,
            system_prompt,
            user_content,
            Some(assistant_content),
            label_type,
            "user",
            "human",
            retrieval_ref,
            verify_log,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_row(
        &self,
        domain: &str,
        system_prompt: Option<&str>,
        user_content: &str,
        assistant_content: Option<&str>,
        label_type: &str,
        trust: &str,
        provenance: &str,
        retrieval_ref: Option<&str>,
        verify_log: Option<&str>,
    ) -> Result<i64> {
        let now = now();
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO training_examples
               (domain, system_prompt, user_content, assistant_content, label_type,
                trust, provenance, retrieval_ref, verify_log, created_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?10)",
            params![
                domain,
                system_prompt,
                user_content,
                assistant_content,
                label_type,
                trust,
                provenance,
                retrieval_ref,
                verify_log,
                now
            ],
        )?;
        let id = c.last_insert_rowid();
        let body = match assistant_content {
            Some(a) if !a.is_empty() => format!("{user_content}\n{a}"),
            _ => user_content.to_string(),
        };
        c.execute(
            "INSERT INTO training_fts (rowid, body) VALUES (?1, ?2)",
            params![id, body],
        )?;
        Ok(id)
    }

    // ── the gate ─────────────────────────────────────────────────────────────

    /// Validation gate: promote a proposed row to `reviewed`. The model-collapse
    /// guard refuses any row with `provenance='student'`.
    pub fn promote_example(&self, id: i64) -> Result<PromoteOutcome> {
        let now = now();
        let c = self.conn.lock().unwrap();
        let n = c.execute(
            "UPDATE training_examples SET trust='reviewed', updated_at=?2
             WHERE id=?1 AND trust='proposed' AND provenance != 'student'",
            params![id, now],
        )?;
        if n > 0 {
            return Ok(PromoteOutcome::Promoted);
        }
        // Nothing changed — disambiguate the collapse-guard rejection from a
        // missing / already-promoted row so callers get an honest message.
        let row: Option<(String, String)> = c
            .query_row(
                "SELECT trust, provenance FROM training_examples WHERE id=?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match row {
            Some((trust, prov)) if prov == "student" && trust == "proposed" => {
                Ok(PromoteOutcome::BlockedStudent)
            }
            _ => Ok(PromoteOutcome::NotFound),
        }
    }

    // ── listing & export ─────────────────────────────────────────────────────

    /// List training rows, newest first, optionally filtered by domain and trust.
    pub fn list_training(
        &self,
        domain: Option<&str>,
        trust: Option<&str>,
    ) -> Result<Vec<TrainingRow>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(
            "SELECT id, domain, system_prompt, user_content, assistant_content,
                    label_type, trust, provenance, retrieval_ref, verify_log,
                    created_at, updated_at
               FROM training_examples
              WHERE (?1 IS NULL OR domain = ?1)
                AND (?2 IS NULL OR trust = ?2)
              ORDER BY updated_at DESC
              LIMIT 200",
        )?;
        let rows = stmt.query_map(params![domain, trust], |r| {
            Ok(TrainingRow {
                id: r.get(0)?,
                domain: r.get(1)?,
                system_prompt: r.get(2)?,
                user_content: r.get(3)?,
                assistant_content: r.get(4)?,
                label_type: r.get(5)?,
                trust: r.get(6)?,
                provenance: r.get(7)?,
                retrieval_ref: r.get(8)?,
                verify_log: r.get(9)?,
                created_at: r.get(10)?,
                updated_at: r.get(11)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Export the dataset as chat JSONL, one file (`<domain>.jsonl`) per domain.
    /// Only `reviewed` + `user` rows with a non-empty assistant turn are emitted
    /// (the export gate). Returns one `ExportFile` per non-empty domain.
    pub fn export_dataset(&self, domain: Option<&str>, out_dir: &str) -> Result<Vec<ExportFile>> {
        std::fs::create_dir_all(out_dir)
            .with_context(|| format!("creating export dir {out_dir}"))?;
        let c = self.conn.lock().unwrap();

        let domains: Vec<String> = if let Some(d) = domain {
            vec![d.to_string()]
        } else {
            let mut stmt = c.prepare(
                "SELECT DISTINCT domain FROM training_examples
                  WHERE trust IN ('reviewed','user') ORDER BY domain",
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };

        let mut out = Vec::new();
        for dom in domains {
            let mut stmt = c.prepare(
                "SELECT system_prompt, user_content, assistant_content
                   FROM training_examples
                  WHERE domain = ?1 AND trust IN ('reviewed','user')
                    AND assistant_content IS NOT NULL AND assistant_content != ''
                  ORDER BY id ASC",
            )?;
            let triples = stmt.query_map(params![dom], |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?;

            let mut buf = String::new();
            let mut n = 0usize;
            for triple in triples {
                let (system, user, assistant) = triple?;
                let mut messages = Vec::new();
                if let Some(s) = system {
                    messages.push(serde_json::json!({ "role": "system", "content": s }));
                }
                messages.push(serde_json::json!({ "role": "user", "content": user }));
                messages.push(serde_json::json!({ "role": "assistant", "content": assistant }));
                buf.push_str(&serde_json::to_string(
                    &serde_json::json!({ "messages": messages }),
                )?);
                buf.push('\n');
                n += 1;
            }
            if n == 0 {
                continue;
            }
            let path =
                std::path::Path::new(out_dir).join(format!("{}.jsonl", sanitize_filename(&dom)));
            std::fs::write(&path, buf).with_context(|| format!("writing {}", path.display()))?;
            out.push(ExportFile {
                domain: dom,
                path: path.to_string_lossy().to_string(),
                rows: n,
            });
        }
        Ok(out)
    }

    // ── capture faucet ───────────────────────────────────────────────────────

    /// Scan an agentic audit trail for `recall` queries the knowledge base could
    /// not answer (<= `threshold` hits) or refused, and emit them as proposed
    /// `student` question-stubs — the capture faucet. The external teacher project
    /// later answers and re-deposits them as `teacher` rows.
    ///
    /// Reads no DB but its own: episodes are passed in by `kl-mcp` from the main
    /// store, keeping this crate decoupled. Deduplicated by `(domain, query)`
    /// against both the current batch and rows already stored.
    pub fn queue_weak(&self, episodes: &[EpisodeRow], threshold: i64) -> Result<usize> {
        let mut seen: HashSet<(String, String)> = HashSet::new();
        let mut inserted = 0usize;
        for ep in episodes {
            if ep.stage.as_deref() != Some("recall") {
                continue;
            }
            let Some(action) = ep.action.as_deref() else {
                continue;
            };
            let Some((domain, query)) = parse_recall_action(action) else {
                continue;
            };

            let blocked = matches!(ep.outcome.as_deref(), Some("blocked") | Some("error"));
            let hits = ep.observation.as_deref().and_then(parse_hits);
            let weak = blocked || matches!(hits, Some(n) if n <= threshold);
            if !weak {
                continue;
            }

            let key = (domain.clone(), query.clone());
            if seen.contains(&key) {
                continue;
            }
            if self.row_exists(&domain, &query)? {
                seen.insert(key);
                continue;
            }

            let label_type = if blocked { "refusal" } else { "grounded" };
            let rref = format!("episode:{}/{}", ep.run_id, ep.step);
            self.capture_example(
                &domain,
                None,
                &query,
                None,
                label_type,
                "student",
                Some(&rref),
                None,
            )?;
            seen.insert(key);
            inserted += 1;
        }
        Ok(inserted)
    }

    // ── coverage faucet ──────────────────────────────────────────────────────

    /// Enumerate a domain's curated `knowledge` and `stages` into diverse proposed
    /// `student` question-stubs — the coverage faucet. Question type is varied
    /// across recall / application / debugging / "what's wrong with this" so the
    /// eventual dataset exercises a topic from several angles.
    ///
    /// Rows are passed in by `kl-mcp` from the main store; this method NEVER
    /// registers or creates a domain (the crate has no `domains` table) — it only
    /// writes `training_examples` for the `domain` it is told about. Deduplicated
    /// by `(domain, question)`.
    pub fn seed_from_topics(
        &self,
        domain: &str,
        stage: Option<&str>,
        knowledge: &[KnowledgeRow],
        stages: &[StageRow],
    ) -> Result<usize> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut inserted = 0usize;

        for k in knowledge {
            if k.domain != domain {
                continue;
            }
            if let Some(st) = stage {
                if k.stage.as_deref() != Some(st) {
                    continue;
                }
            }
            let rref = format!("knowledge:#{}", k.id);
            for q in topic_questions(k) {
                inserted += self.seed_one(domain, &q, &rref, &mut seen)?;
            }
        }

        for s in stages {
            if let Some(st) = stage {
                if s.name != st {
                    continue;
                }
            }
            let rref = format!("stage:{}", s.name);
            let q = match s.description.as_deref() {
                Some(d) if !d.is_empty() => {
                    format!("Walk through the \"{}\" stage ({d}). What must you verify, and in what order?", s.name)
                }
                _ => format!(
                    "Walk through the \"{}\" stage. What must you verify, and in what order?",
                    s.name
                ),
            };
            inserted += self.seed_one(domain, &q, &rref, &mut seen)?;
        }

        Ok(inserted)
    }

    /// Insert one stub if it is not a dup (batch or stored). Returns 1 if inserted.
    fn seed_one(
        &self,
        domain: &str,
        question: &str,
        retrieval_ref: &str,
        seen: &mut HashSet<String>,
    ) -> Result<usize> {
        if seen.contains(question) || self.row_exists(domain, question)? {
            seen.insert(question.to_string());
            return Ok(0);
        }
        self.capture_example(
            domain,
            None,
            question,
            None,
            "grounded",
            "student",
            Some(retrieval_ref),
            None,
        )?;
        seen.insert(question.to_string());
        Ok(1)
    }

    /// True if a row with this exact `(domain, user_content)` already exists — the
    /// dedup guard for the faucets. Locks and releases on its own (do not call
    /// while holding `conn`).
    fn row_exists(&self, domain: &str, user_content: &str) -> Result<bool> {
        let c = self.conn.lock().unwrap();
        let found: Option<i64> = c
            .query_row(
                "SELECT id FROM training_examples WHERE domain=?1 AND user_content=?2 LIMIT 1",
                params![domain, user_content],
                |r| r.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    // ── dashboard helpers ────────────────────────────────────────────────────

    pub fn stats(&self) -> Result<TrainStats> {
        let c = self.conn.lock().unwrap();
        let count = |sql: &str| -> Result<i64> { Ok(c.query_row(sql, [], |r| r.get(0))?) };
        Ok(TrainStats {
            total: count("SELECT COUNT(*) FROM training_examples")?,
            proposed: count("SELECT COUNT(*) FROM training_examples WHERE trust = 'proposed'")?,
            reviewed: count("SELECT COUNT(*) FROM training_examples WHERE trust = 'reviewed'")?,
            user: count("SELECT COUNT(*) FROM training_examples WHERE trust = 'user'")?,
            stubs: count(
                "SELECT COUNT(*) FROM training_examples \
                 WHERE assistant_content IS NULL OR assistant_content = ''",
            )?,
        })
    }

    pub fn clear_all(&self) -> Result<u64> {
        let c = self.conn.lock().unwrap();
        c.execute_batch("DELETE FROM training_fts; DELETE FROM training_examples;")?;
        let deleted: u64 = c
            .query_row("SELECT changes()", [], |r| r.get(0))
            .unwrap_or(0);
        Ok(deleted)
    }
}

// ── helpers ─────────────────────────────────────────────────────────────────────

/// Current unix timestamp (seconds). Every write path stamps identically.
fn now() -> i64 {
    Utc::now().timestamp()
}

/// Make a domain name safe to use as a filename stem.
fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|ch| {
            if ch.is_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

/// Build varied question-stubs for one knowledge item. Question type is chosen by
/// kind so the dataset covers recall, application, debugging, and "what's wrong".
fn topic_questions(k: &KnowledgeRow) -> Vec<String> {
    let title = &k.title;
    match k.kind.as_str() {
        "rule" => {
            let sev = k.severity.as_deref().unwrap_or("the stated severity");
            vec![
                format!("What does the rule \"{title}\" require, and why?"),
                format!("You hit a case that violates \"{title}\" ({sev}). What's the correct remediation?"),
                format!("What's wrong with code or a design that ignores \"{title}\"?"),
            ]
        }
        "procedure" => vec![
            format!("Outline the procedure \"{title}\" step by step."),
            format!("How would you apply the procedure \"{title}\" to a concrete task?"),
        ],
        // fact (and any unrecognized kind)
        _ => vec![
            format!("What does \"{title}\" state?"),
            format!("How would you apply \"{title}\" in practice?"),
        ],
    }
}

/// Parse a recall episode action — `"recall domain=X query=Y"` (logged by
/// `Klayer::recall`) — back into `(domain, query)`.
fn parse_recall_action(action: &str) -> Option<(String, String)> {
    let rest = action.strip_prefix("recall domain=")?;
    let (domain, query) = rest.split_once(" query=")?;
    if domain.is_empty() || query.is_empty() {
        return None;
    }
    Some((domain.to_string(), query.to_string()))
}

/// Parse the hit count out of a recall observation — `"returned N hits"`.
fn parse_hits(observation: &str) -> Option<i64> {
    observation
        .strip_prefix("returned ")?
        .split_whitespace()
        .next()?
        .parse::<i64>()
        .ok()
}

// ── tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> TrainStore {
        let s = TrainStore::open(":memory:").unwrap();
        s.migrate().unwrap();
        s
    }

    #[test]
    fn collapse_guard_blocks_student_promotion() {
        let s = store();
        // A student-provenance row must never be promotable.
        let sid = s
            .capture_example(
                "d",
                None,
                "q?",
                Some("a"),
                "grounded",
                "student",
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            s.promote_example(sid).unwrap(),
            PromoteOutcome::BlockedStudent
        );

        // It stays proposed after the rejected promote.
        let rows = s.list_training(Some("d"), Some("proposed")).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].trust, "proposed");

        // A teacher-provenance row promotes fine — the guard is provenance-specific.
        let tid = s
            .capture_example(
                "d",
                None,
                "q2?",
                Some("a2"),
                "grounded",
                "teacher",
                None,
                None,
            )
            .unwrap();
        assert_eq!(s.promote_example(tid).unwrap(), PromoteOutcome::Promoted);
    }

    #[test]
    fn export_emits_only_reviewed_and_user() {
        let s = store();
        // proposed teacher (must be EXCLUDED)
        s.capture_example(
            "d",
            Some("sys"),
            "PROPOSED_Q?",
            Some("pa"),
            "grounded",
            "teacher",
            None,
            None,
        )
        .unwrap();
        // reviewed teacher (must be included)
        let r = s
            .capture_example(
                "d",
                Some("sys"),
                "rq?",
                Some("ra"),
                "grounded",
                "teacher",
                None,
                None,
            )
            .unwrap();
        assert_eq!(s.promote_example(r).unwrap(), PromoteOutcome::Promoted);
        // user/human (must be included)
        s.author_example("d", None, "uq?", "ua", "grounded", None, None)
            .unwrap();

        let dir = std::env::temp_dir().join(format!("kltrain_test_{}", std::process::id()));
        let dir_s = dir.to_string_lossy().to_string();
        let files = s.export_dataset(None, &dir_s).unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].rows, 2); // reviewed + user only, NOT the proposed row

        let content = std::fs::read_to_string(&files[0].path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        for l in &lines {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            let msgs = v["messages"].as_array().unwrap();
            let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
            assert!(roles.contains(&"user"));
            assert!(roles.contains(&"assistant"));
        }
        // the proposed row's content must never leak into the dataset
        assert!(!content.contains("PROPOSED_Q?"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn queue_weak_captures_zero_hit_recalls_once() {
        let s = store();
        let ep = |step: i64, q: &str, hits: i64| EpisodeRow {
            id: step,
            run_id: "run1".into(),
            step,
            stage: Some("recall".into()),
            action: Some(format!("recall domain=secdev query={q}")),
            observation: Some(format!("returned {hits} hits")),
            outcome: Some("success".into()),
            ts: 0,
        };
        let eps = vec![
            ep(1, "how to sanitize input?", 0), // weak -> captured
            ep(2, "what is xss?", 5),           // strong -> skipped
            ep(3, "how to sanitize input?", 0), // dup of #1 -> skipped
        ];

        let n = s.queue_weak(&eps, 0).unwrap();
        assert_eq!(n, 1);

        let rows = s.list_training(Some("secdev"), Some("proposed")).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].provenance, "student");
        assert_eq!(rows[0].label_type, "grounded");
        assert!(rows[0]
            .assistant_content
            .as_deref()
            .unwrap_or("")
            .is_empty());

        // Re-running over the same trail inserts nothing (already stored).
        assert_eq!(s.queue_weak(&eps, 0).unwrap(), 0);
    }

    #[test]
    fn seed_from_topics_emits_varied_student_stubs() {
        let s = store();
        let k = |id: i64, kind: &str, title: &str| KnowledgeRow {
            id,
            kind: kind.into(),
            domain: "secdev".into(),
            stage: None,
            title: title.into(),
            body: "body".into(),
            trust: "reviewed".into(),
            enforceable: true,
            severity: Some("block".into()),
            created_at: 0,
            updated_at: 0,
        };
        let knowledge = vec![
            k(1, "rule", "validate all input"),
            k(2, "fact", "TLS 1.3 is preferred"),
        ];
        let stages: Vec<StageRow> = vec![];

        // 3 variants for the rule + 2 for the fact = 5 stubs.
        let n = s
            .seed_from_topics("secdev", None, &knowledge, &stages)
            .unwrap();
        assert_eq!(n, 5);

        let rows = s.list_training(Some("secdev"), Some("proposed")).unwrap();
        assert_eq!(rows.len(), 5);
        assert!(rows.iter().all(|r| r.provenance == "student"));
        assert!(rows
            .iter()
            .all(|r| r.assistant_content.as_deref().unwrap_or("").is_empty()));
        assert!(rows.iter().all(|r| r
            .retrieval_ref
            .as_deref()
            .unwrap_or("")
            .starts_with("knowledge:#")));

        // Idempotent: re-seeding the same topics inserts nothing.
        assert_eq!(
            s.seed_from_topics("secdev", None, &knowledge, &stages)
                .unwrap(),
            0
        );
    }
}

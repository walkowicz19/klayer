//! kl-code — persistent codebase memory store.
//!
//! Indexes local directories into a dedicated libsql DB (`KLAYER_CODE_DB`,
//! default `klayer_code.db`) with FTS5 over code chunks, so the LLM can
//! recall any function, struct, or pattern across sessions. When
//! `KLAYER_TURSO_URL`/`KLAYER_TURSO_TOKEN` are set, the DB is opened as an
//! embedded replica that periodically syncs against Turso; otherwise it is a
//! pure local file, identical in behavior to the old rusqlite store.
//!
//! Intentionally separate from `klayer.db` (knowledge/episodes).

mod indexing;
mod lang;
mod symbols;

use anyhow::{Context, Result};
use chrono::Utc;
use kl_core::{SyncHealth, SyncHealthSnapshot};
use libsql::{params, Connection, Database};
use serde::Serialize;
use std::path::Path;
use std::sync::Arc;

use indexing::{collect_files, fts_match, insert_repo_data, scalar_i64, scalar_i64_params};

const SCHEMA: &str = "
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS repos (
    id          INTEGER PRIMARY KEY,
    path        TEXT    NOT NULL UNIQUE,
    name        TEXT,
    indexed_at  INTEGER,
    file_count  INTEGER NOT NULL DEFAULT 0,
    chunk_count INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS code_files (
    id       INTEGER PRIMARY KEY,
    repo_id  INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    rel_path TEXT    NOT NULL,
    language TEXT,
    UNIQUE(repo_id, rel_path)
);

CREATE TABLE IF NOT EXISTS code_chunks (
    id          INTEGER PRIMARY KEY,
    file_id     INTEGER NOT NULL REFERENCES code_files(id) ON DELETE CASCADE,
    line_start  INTEGER NOT NULL DEFAULT 1,
    line_end    INTEGER NOT NULL DEFAULT 1,
    content     TEXT    NOT NULL,
    kind        TEXT,
    symbol_name TEXT
);

-- Plain FTS5 (no content=): owns its data so standard DELETE works.
CREATE VIRTUAL TABLE IF NOT EXISTS code_fts USING fts5(body);
";

// ── Public types ─────────────────────────────────────────────────────────────

pub struct CodeStore {
    conn: Connection,
    #[allow(dead_code)] // kept alive so the background sync task's Arc clone isn't orphaned
    db: Arc<Database>,
    health: Arc<SyncHealth>,
    remote_configured: bool,
}

#[derive(Debug, Serialize, Clone)]
pub struct RepoInfo {
    pub id: i64,
    pub path: String,
    pub name: Option<String>,
    pub indexed_at: Option<i64>,
    pub file_count: i64,
    pub chunk_count: i64,
}

#[derive(Debug, Serialize)]
pub struct CodeHit {
    pub repo_path: String,
    pub file_path: String,
    pub language: Option<String>,
    pub line_start: i64,
    pub line_end: i64,
    pub kind: Option<String>,
    pub symbol_name: Option<String>,
    pub snippet: String,
    pub score: f64,
}

#[derive(Debug, Serialize)]
pub struct CodeStats {
    pub repos: i64,
    pub files: i64,
    pub chunks: i64,
}

#[derive(Debug, Serialize)]
pub struct IndexStats {
    pub files: usize,
    pub chunks: usize,
    pub skipped: usize,
    pub skip_reasons: Vec<String>,
    pub warnings: Vec<String>,
}

// ── CodeStore impl ────────────────────────────────────────────────────────────

impl CodeStore {
    pub async fn open(path: &str) -> Result<Self> {
        let db = kl_core::open_db(path)
            .await
            .with_context(|| format!("opening code db at {path}"))?;
        let db = Arc::new(db);
        let conn = db.connect().context("opening code db connection")?;
        let remote_configured = kl_core::turso_config().is_some();
        let health = SyncHealth::new();
        kl_core::spawn_sync_task(Arc::clone(&db), Arc::clone(&health));
        Ok(Self {
            conn,
            db,
            health,
            remote_configured,
        })
    }

    pub async fn migrate(&self) -> Result<()> {
        self.conn
            .execute_batch(SCHEMA)
            .await
            .context("code db schema")?;
        Ok(())
    }

    pub fn health(&self) -> SyncHealthSnapshot {
        self.health.snapshot(self.remote_configured)
    }

    pub async fn stats(&self) -> Result<CodeStats> {
        let repos = scalar_i64(&self.conn, "SELECT COUNT(*) FROM repos").await?;
        let files = scalar_i64(&self.conn, "SELECT COUNT(*) FROM code_files").await?;
        let chunks = scalar_i64(&self.conn, "SELECT COUNT(*) FROM code_chunks").await?;
        Ok(CodeStats {
            repos,
            files,
            chunks,
        })
    }

    /// Same shape as `stats()`, scoped to a single repo (matched by canonical
    /// path or friendly name, same lookup `search`'s `repo_path` filter
    /// uses). Needed because `stats()` aggregates across every indexed repo,
    /// which is the wrong signal once more than one repo is indexed and a
    /// caller wants complexity scoped to just one of them. Returns all-zero
    /// stats (not an error) for an unknown repo, matching `forget_repo`'s
    /// "not found is a normal outcome" convention.
    pub async fn stats_for_repo(&self, repo: &str) -> Result<CodeStats> {
        let repo_id: Option<i64> = {
            let mut rows = self
                .conn
                .query(
                    "SELECT id FROM repos WHERE path = ?1 OR name = ?1",
                    params![repo],
                )
                .await?;
            match rows.next().await? {
                Some(r) => Some(r.get(0)?),
                None => None,
            }
        };
        let Some(repo_id) = repo_id else {
            return Ok(CodeStats {
                repos: 0,
                files: 0,
                chunks: 0,
            });
        };
        let files = scalar_i64_params(
            &self.conn,
            "SELECT COUNT(*) FROM code_files WHERE repo_id = ?1",
            params![repo_id],
        )
        .await?;
        let chunks = scalar_i64_params(
            &self.conn,
            "SELECT COUNT(*) FROM code_chunks cc JOIN code_files cf ON cf.id = cc.file_id WHERE cf.repo_id = ?1",
            params![repo_id],
        )
        .await?;
        Ok(CodeStats {
            repos: 1,
            files,
            chunks,
        })
    }

    pub async fn list_repos(&self) -> Result<Vec<RepoInfo>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id, path, name, indexed_at, file_count, chunk_count
                 FROM repos ORDER BY indexed_at DESC",
                (),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().await? {
            out.push(RepoInfo {
                id: r.get(0)?,
                path: r.get(1)?,
                name: r.get(2)?,
                indexed_at: r.get(3)?,
                file_count: r.get(4)?,
                chunk_count: r.get(5)?,
            });
        }
        Ok(out)
    }

    pub async fn forget_repo(&self, path: &str) -> Result<bool> {
        let repo_id: Option<i64> = {
            let mut rows = self
                .conn
                .query("SELECT id FROM repos WHERE path = ?1", params![path])
                .await?;
            match rows.next().await? {
                Some(r) => Some(r.get(0)?),
                None => None,
            }
        };
        let Some(repo_id) = repo_id else {
            return Ok(false);
        };

        // Collect chunk IDs before cascade-deleting (FTS5 must be cleaned first).
        let mut chunk_ids: Vec<i64> = Vec::new();
        let mut rows = self
            .conn
            .query(
                "SELECT cc.id FROM code_chunks cc
                 JOIN code_files cf ON cf.id = cc.file_id
                 WHERE cf.repo_id = ?1",
                params![repo_id],
            )
            .await?;
        while let Some(r) = rows.next().await? {
            chunk_ids.push(r.get(0)?);
        }

        // Remove from FTS5 then cascade-delete from repos
        for id in &chunk_ids {
            self.conn
                .execute("DELETE FROM code_fts WHERE rowid = ?1", params![*id])
                .await?;
        }
        self.conn
            .execute("DELETE FROM repos WHERE id = ?1", params![repo_id])
            .await?;
        Ok(true)
    }

    pub async fn clear_all(&self) -> Result<u64> {
        self.conn.execute("DELETE FROM code_fts", ()).await?;
        let deleted = self.conn.execute("DELETE FROM repos", ()).await?;
        Ok(deleted)
    }

    pub async fn search(
        &self,
        query: &str,
        repo_path: Option<&str>,
        limit: usize,
    ) -> Result<Vec<CodeHit>> {
        if query.trim().is_empty() {
            return Ok(vec![]);
        }
        let match_expr = fts_match(query);
        let lim = limit as i64;
        let mut hits = Vec::new();

        let mut rows = if let Some(rp) = repo_path {
            self.conn
                .query(
                    "SELECT cc.content, cc.kind, cc.symbol_name,
                            cc.line_start, cc.line_end,
                            cf.rel_path, cf.language, r.path,
                            bm25(code_fts) AS score
                     FROM code_fts
                     JOIN code_chunks cc ON cc.id = code_fts.rowid
                     JOIN code_files  cf ON cf.id = cc.file_id
                     JOIN repos        r ON  r.id = cf.repo_id
                     WHERE code_fts MATCH ?1 AND r.path = ?2
                     ORDER BY score ASC LIMIT ?3",
                    params![match_expr, rp, lim],
                )
                .await?
        } else {
            self.conn
                .query(
                    "SELECT cc.content, cc.kind, cc.symbol_name,
                            cc.line_start, cc.line_end,
                            cf.rel_path, cf.language, r.path,
                            bm25(code_fts) AS score
                     FROM code_fts
                     JOIN code_chunks cc ON cc.id = code_fts.rowid
                     JOIN code_files  cf ON cf.id = cc.file_id
                     JOIN repos        r ON  r.id = cf.repo_id
                     WHERE code_fts MATCH ?1
                     ORDER BY score ASC LIMIT ?2",
                    params![match_expr, lim],
                )
                .await?
        };

        while let Some(r) = rows.next().await? {
            hits.push(CodeHit {
                snippet: r.get(0)?,
                kind: r.get(1)?,
                symbol_name: r.get(2)?,
                line_start: r.get(3)?,
                line_end: r.get(4)?,
                file_path: r.get(5)?,
                language: r.get(6)?,
                repo_path: r.get(7)?,
                score: r.get(8)?,
            });
        }

        Ok(hits)
    }

    pub async fn index_repo(&self, dir_path: &str, name: Option<&str>) -> Result<IndexStats> {
        let canonical = std::fs::canonicalize(dir_path)
            .with_context(|| format!("resolving path: {dir_path}"))?;
        let canon_str = canonical.to_string_lossy().replace('\\', "/");
        let now = Utc::now().timestamp();

        // Collect files outside any DB call (blocking I/O, may be large).
        let mut file_data = Vec::new();
        let mut skip_reasons = Vec::new();
        collect_files(&canonical, &canonical, &mut file_data, &mut skip_reasons)?;
        let skipped = skip_reasons.len();
        let mut warnings = Vec::new();
        for repo in self.list_repos().await? {
            let existing = Path::new(&repo.path);
            if existing != canonical
                && (canonical.starts_with(existing) || existing.starts_with(&canonical))
            {
                warnings.push(format!(
                    "overlapping repository index detected: '{}' and '{}' are stored separately",
                    canon_str, repo.path
                ));
            }
        }

        let total_files = file_data.len();
        let total_chunks: usize = file_data.iter().map(|f| f.chunks.len()).sum();

        tracing::info!(
            "code-index: {} files, {} chunks, {} skipped — writing to DB",
            total_files,
            total_chunks,
            skipped
        );

        // Upsert repo record.
        self.conn
            .execute(
                "INSERT INTO repos (path, name, indexed_at, file_count, chunk_count)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(path) DO UPDATE SET
                     name        = COALESCE(?2, name),
                     indexed_at  = ?3,
                     file_count  = ?4,
                     chunk_count = ?5",
                params![
                    canon_str.clone(),
                    name,
                    now,
                    total_files as i64,
                    total_chunks as i64
                ],
            )
            .await?;
        let repo_id: i64 = {
            let mut rows = self
                .conn
                .query(
                    "SELECT id FROM repos WHERE path = ?1",
                    params![canon_str.clone()],
                )
                .await?;
            rows.next()
                .await?
                .context("repo row missing after upsert")?
                .get(0)?
        };

        // Collect old chunk IDs before cascade-deleting (FTS cleanup required).
        let mut old_chunk_ids: Vec<i64> = Vec::new();
        let mut rows = self
            .conn
            .query(
                "SELECT cc.id FROM code_chunks cc
                 JOIN code_files cf ON cf.id = cc.file_id
                 WHERE cf.repo_id = ?1",
                params![repo_id],
            )
            .await?;
        while let Some(r) = rows.next().await? {
            old_chunk_ids.push(r.get(0)?);
        }

        // Clean FTS5 for old entries, then delete old files (cascade → chunks).
        for id in &old_chunk_ids {
            self.conn
                .execute("DELETE FROM code_fts WHERE rowid = ?1", params![*id])
                .await?;
        }
        self.conn
            .execute(
                "DELETE FROM code_files WHERE repo_id = ?1",
                params![repo_id],
            )
            .await?;

        // Insert new files and chunks in one transaction.
        insert_repo_data(&self.conn, repo_id, &file_data).await?;

        Ok(IndexStats {
            files: total_files,
            chunks: total_chunks,
            skipped,
            skip_reasons,
            warnings,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unknown_extensions_fall_back_to_text_and_binary_skips_are_explained() {
        let root = std::env::temp_dir().join(format!("klayer-index-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("PROGRAM.CBLX"),
            "IDENTIFICATION DIVISION.\nPROGRAM-ID. HELLO.",
        )
        .unwrap();
        std::fs::write(root.join("artifact.bin"), b"abc\0def").unwrap();
        let db = root.join("code.db");
        let store = CodeStore::open(db.to_str().unwrap()).await.unwrap();
        store.migrate().await.unwrap();
        let stats = store
            .index_repo(root.to_str().unwrap(), Some("test"))
            .await
            .unwrap();
        assert!(stats.files >= 1);
        assert!(stats
            .skip_reasons
            .iter()
            .any(|r| r.contains("artifact.bin: binary content")));
        let hits = store.search("IDENTIFICATION", None, 5).await.unwrap();
        assert!(hits.iter().any(|h| h.file_path == "PROGRAM.CBLX"));
        drop(store);
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn stats_for_repo_scopes_to_one_repo_among_several() {
        let root = std::env::temp_dir().join(format!("klayer-stats-test-{}", std::process::id()));
        let repo_a = root.join("repo_a");
        let repo_b = root.join("repo_b");
        std::fs::create_dir_all(&repo_a).unwrap();
        std::fs::create_dir_all(&repo_b).unwrap();
        std::fs::write(repo_a.join("one.rs"), "fn one() {}\nfn two() {}\n").unwrap();
        std::fs::write(repo_b.join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(repo_b.join("b.rs"), "fn b() {}\n").unwrap();
        std::fs::write(repo_b.join("c.rs"), "fn c() {}\n").unwrap();

        let db = root.join("code.db");
        let store = CodeStore::open(db.to_str().unwrap()).await.unwrap();
        store.migrate().await.unwrap();
        store
            .index_repo(repo_a.to_str().unwrap(), Some("repo_a"))
            .await
            .unwrap();
        store
            .index_repo(repo_b.to_str().unwrap(), Some("repo_b"))
            .await
            .unwrap();

        let global = store.stats().await.unwrap();
        assert_eq!(global.files, 4);

        let scoped_a = store.stats_for_repo("repo_a").await.unwrap();
        assert_eq!(scoped_a.files, 1);
        let scoped_b = store.stats_for_repo("repo_b").await.unwrap();
        assert_eq!(scoped_b.files, 3);
        assert_ne!(scoped_a.files, global.files);

        let unknown = store.stats_for_repo("does-not-exist").await.unwrap();
        assert_eq!(unknown.files, 0);
        assert_eq!(unknown.repos, 0);

        drop(store);
        std::fs::remove_dir_all(root).ok();
    }
}

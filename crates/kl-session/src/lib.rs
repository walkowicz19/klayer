//! kl-session — repo-scoped session memory (the model's journal).
//!
//! Carved out of `kl-store` (which used to hold this alongside general
//! knowledge) into its own store, mirroring the `kl-code`/`kl-train` crate
//! shape. Storage lives in its own libsql DB (`KLAYER_SESSION_DB`, default
//! `klayer_session.db`). When `KLAYER_TURSO_URL`/`KLAYER_TURSO_TOKEN` are
//! set, it is opened as an embedded replica that periodically syncs against
//! Turso; otherwise it is a pure local file.
//!
//! A curated journal the model writes to (`log_work`) and replays at session
//! start (`recall_session`) so it re-establishes context and stops repeating
//! mistakes. Distinct from the noisy auto-logged `episodes` trace in kl-store.

use anyhow::{Context, Result};
use chrono::Utc;
use kl_core::{JournalRow, SyncHealth, SyncHealthSnapshot};
use libsql::{params, Connection, Database};
use std::sync::Arc;

const MIGRATION: &str = include_str!("migrations/0001_init.sql");

pub struct SessionStore {
    conn: Connection,
    #[allow(dead_code)] // kept alive so the background sync task's Arc clone isn't orphaned
    db: Arc<Database>,
    health: Arc<SyncHealth>,
    remote_configured: bool,
}

impl SessionStore {
    pub async fn open(path: &str) -> Result<Self> {
        let db = kl_core::open_db(path)
            .await
            .with_context(|| format!("opening session db at {path}"))?;
        let db = Arc::new(db);
        let conn = db.connect().context("opening session db connection")?;
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
            .execute_batch(MIGRATION)
            .await
            .context("session db schema")?;
        Ok(())
    }

    pub fn health(&self) -> SyncHealthSnapshot {
        self.health.snapshot(self.remote_configured)
    }

    // ---- repo-scoped session memory (journal) ----------------------------

    /// Append one curated entry to a repo's session journal.
    /// `kind` is one of 'done' | 'failed' | 'avoid' | 'decision' | 'note'.
    pub async fn log_work(
        &self,
        repo: &str,
        kind: &str,
        title: &str,
        body: Option<&str>,
        is_checkpoint: bool,
    ) -> Result<i64> {
        let now = Utc::now().timestamp();
        self.conn
            .execute(
                "INSERT INTO journal (repo, kind, title, body, ts, is_checkpoint) VALUES (?1,?2,?3,?4,?5,?6)",
                params![repo, kind, title, body, now, is_checkpoint],
            )
            .await?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Replay a repo's journal, newest first. Optional `kind` filter; `limit` caps rows.
    pub async fn recall_session(
        &self,
        repo: &str,
        kind: Option<&str>,
        limit: usize,
        checkpoints_only: bool,
    ) -> Result<Vec<JournalRow>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id, repo, kind, title, body, ts, is_checkpoint FROM journal
                  WHERE repo = ?1 AND (?2 IS NULL OR kind = ?2) AND (?4 = 0 OR is_checkpoint = 1)
                  ORDER BY ts DESC, id DESC
                  LIMIT ?3",
                params![repo, kind, limit as i64, checkpoints_only],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().await? {
            out.push(journal_from_row(&r)?);
        }
        Ok(out)
    }

    /// List journal entries for the dashboard: one repo, or all repos if None.
    pub async fn list_journal(&self, repo: Option<&str>) -> Result<Vec<JournalRow>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id, repo, kind, title, body, ts, is_checkpoint FROM journal
                  WHERE (?1 IS NULL OR repo = ?1)
                  ORDER BY ts DESC, id DESC
                  LIMIT 300",
                params![repo],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().await? {
            out.push(journal_from_row(&r)?);
        }
        Ok(out)
    }

    /// Clear a repo's journal, or every repo's journal if None. Returns rows deleted.
    pub async fn clear_journal(&self, repo: Option<&str>) -> Result<u64> {
        let n = self
            .conn
            .execute(
                "DELETE FROM journal WHERE (?1 IS NULL OR repo = ?1)",
                params![repo],
            )
            .await?;
        Ok(n)
    }
}

fn journal_from_row(r: &libsql::Row) -> Result<JournalRow> {
    Ok(JournalRow {
        id: r.get(0)?,
        repo: r.get(1)?,
        kind: r.get(2)?,
        title: r.get(3)?,
        body: r.get(4)?,
        ts: r.get(5)?,
        is_checkpoint: r.get::<i64>(6)? != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> SessionStore {
        let s = SessionStore::open(":memory:").await.unwrap();
        s.migrate().await.unwrap();
        s
    }

    #[tokio::test]
    async fn log_work_and_recall_session_roundtrip() {
        let s = store().await;
        s.log_work("repo1", "done", "did a thing", Some("details"), false)
            .await
            .unwrap();
        s.log_work("repo1", "avoid", "checkpoint entry", None, true)
            .await
            .unwrap();

        let all = s.recall_session("repo1", None, 10, false).await.unwrap();
        assert_eq!(all.len(), 2);

        let checkpoints = s.recall_session("repo1", None, 10, true).await.unwrap();
        assert_eq!(checkpoints.len(), 1);
        assert!(checkpoints[0].is_checkpoint);

        let cleared = s.clear_journal(Some("repo1")).await.unwrap();
        assert_eq!(cleared, 2);
        assert!(s
            .recall_session("repo1", None, 10, false)
            .await
            .unwrap()
            .is_empty());
    }
}

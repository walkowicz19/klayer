//! Agentic episodic memory: per-run step logging, retention sweeps over
//! curated knowledge, and episode retrieval/annotation.

use anyhow::Result;
use chrono::Utc;
use kl_core::EpisodeRow;
use rusqlite::params;

use crate::Store;

impl Store {
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

    /// Purge knowledge items whose effective retention window has elapsed.
    ///
    /// Effective retention per item is `knowledge.retention_days` if set,
    /// else the owning domain's `retention_days`; if both are `None` the
    /// item never expires. A marketplace-template domain (`is_marketplace_template`)
    /// is skipped entirely unless it has its own explicit `retention_days` —
    /// otherwise applying a template would silently start a countdown the
    /// user never asked for.
    ///
    /// Pure/synchronous and independent of the tokio sweep task's loop/sleep
    /// wrapper, so it can be exercised directly in tests. Each purge is
    /// logged via `log_episode_auto` under `stage="retention_sweep"`, tagged
    /// with `run_id` so it shows up in that run's episode trace.
    pub fn retention_sweep(&self, run_id: &str) -> Result<usize> {
        let now = Utc::now().timestamp();
        let mut purged = 0usize;
        for d in self.list_domains()? {
            if d.is_marketplace_template && d.retention_days.is_none() {
                continue;
            }
            for item in self.list_knowledge_all(&d.name)? {
                let Some(days) = item.retention_days.or(d.retention_days) else {
                    continue;
                };
                let cutoff = item.created_at + days * 86_400;
                if now < cutoff {
                    continue;
                }
                if self.forget(item.id)? {
                    purged += 1;
                    self.log_episode_auto(
                        run_id,
                        Some("retention_sweep"),
                        Some(&format!(
                            "purge knowledge id={} domain={} retention_days={days}",
                            item.id, item.domain
                        )),
                        None,
                        Some("success"),
                        Some(&item.domain),
                        None,
                        None,
                        None,
                    )
                    .ok();
                }
            }
        }
        Ok(purged)
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

    /// Most recent `ts` of an episode with the given `stage` whose `action`
    /// text contains `harness_needle` (e.g. `"harness=Claude Code"`) — a
    /// best-effort derivation over the free-text `action` field, not a clean
    /// indexed lookup (there is no dedicated harness column on `episodes`).
    pub fn last_episode_ts_for(&self, stage: &str, harness_needle: &str) -> Option<i64> {
        let c = self.conn.lock().ok()?;
        c.query_row(
            "SELECT MAX(ts) FROM episodes WHERE stage = ?1 AND action LIKE ?2",
            params![stage, format!("%{harness_needle}%")],
            |r| r.get(0),
        )
        .ok()
        .flatten()
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

    pub fn clear_all_episodes(&self) -> Result<u64> {
        let c = self.conn.lock().unwrap();
        let n = c.execute("DELETE FROM episodes", [])?;
        Ok(n as u64)
    }
}

#[cfg(test)]
mod retention_tests {
    use super::*;

    fn fixture() -> Store {
        let store = Store::open(":memory:").expect("open in-memory store");
        store.migrate().expect("migrate");
        store
    }

    /// Backdate a knowledge item's `created_at` directly, bypassing the
    /// public API (there's no supported way to set it in the past otherwise).
    fn backdate(store: &Store, id: i64, days_ago: i64) {
        let c = store.conn.lock().unwrap();
        let ts = Utc::now().timestamp() - days_ago * 86_400;
        c.execute(
            "UPDATE knowledge SET created_at = ?1 WHERE id = ?2",
            params![ts, id],
        )
        .unwrap();
    }

    #[test]
    fn sweep_purges_item_past_domain_retention() {
        let store = fixture();
        store
            .register_domain(
                "expiring",
                None,
                None,
                None,
                None,
                Some(Some(30)),
                None,
                None,
            )
            .unwrap();
        let id = store.remember("expiring", "an old fact").unwrap();
        backdate(&store, id, 31);

        let purged = store.retention_sweep("test-run").unwrap();
        assert_eq!(purged, 1);
        assert!(store.get_knowledge_by_id(id).unwrap().is_none());
    }

    #[test]
    fn sweep_leaves_item_within_domain_retention() {
        let store = fixture();
        store
            .register_domain(
                "expiring",
                None,
                None,
                None,
                None,
                Some(Some(30)),
                None,
                None,
            )
            .unwrap();
        let id = store.remember("expiring", "a fresh fact").unwrap();
        backdate(&store, id, 5);

        let purged = store.retention_sweep("test-run").unwrap();
        assert_eq!(purged, 0);
        assert!(store.get_knowledge_by_id(id).unwrap().is_some());
    }

    #[test]
    fn item_override_wins_over_domain_default() {
        let store = fixture();
        store
            .register_domain(
                "team-domain",
                None,
                None,
                None,
                None,
                Some(Some(365)),
                None,
                None,
            )
            .unwrap();
        let id = store.remember("team-domain", "short-lived fact").unwrap();
        store.set_knowledge_retention(id, Some(7), None).unwrap();
        backdate(&store, id, 10);

        let purged = store.retention_sweep("test-run").unwrap();
        assert_eq!(purged, 1);
        assert!(store.get_knowledge_by_id(id).unwrap().is_none());
    }

    #[test]
    fn marketplace_template_without_explicit_retention_is_skipped() {
        let store = fixture();
        store
            .register_domain(
                "template-domain",
                None,
                None,
                None,
                None,
                None,
                Some(true),
                None,
            )
            .unwrap();
        let id = store.remember("template-domain", "template fact").unwrap();
        backdate(&store, id, 9999);

        let purged = store.retention_sweep("test-run").unwrap();
        assert_eq!(purged, 0);
        assert!(store.get_knowledge_by_id(id).unwrap().is_some());
    }

    #[test]
    fn marketplace_template_with_explicit_retention_is_not_skipped() {
        let store = fixture();
        store
            .register_domain(
                "template-domain",
                None,
                None,
                None,
                None,
                Some(Some(10)),
                Some(true),
                None,
            )
            .unwrap();
        let id = store.remember("template-domain", "template fact").unwrap();
        backdate(&store, id, 11);

        let purged = store.retention_sweep("test-run").unwrap();
        assert_eq!(purged, 1);
        assert!(store.get_knowledge_by_id(id).unwrap().is_none());
    }

    #[test]
    fn register_domain_clamps_retention_to_ceiling() {
        let store = fixture();
        store
            .register_domain(
                "capped-domain",
                None,
                None,
                None,
                None,
                Some(Some(400)),
                None,
                Some(90),
            )
            .unwrap();
        let domains = store.list_domains().unwrap();
        let d = domains.iter().find(|d| d.name == "capped-domain").unwrap();
        assert_eq!(d.retention_days, Some(90));
    }

    #[test]
    fn register_domain_clear_retention_resets_to_no_expiration() {
        let store = fixture();
        store
            .register_domain(
                "clearable",
                None,
                None,
                None,
                None,
                Some(Some(30)),
                None,
                None,
            )
            .unwrap();
        store
            .register_domain("clearable", None, None, None, None, Some(None), None, None)
            .unwrap();
        let domains = store.list_domains().unwrap();
        let d = domains.iter().find(|d| d.name == "clearable").unwrap();
        assert_eq!(d.retention_days, None);
    }

    #[test]
    fn register_domain_unspecified_retention_leaves_it_unchanged() {
        let store = fixture();
        store
            .register_domain(
                "untouched",
                None,
                None,
                None,
                None,
                Some(Some(15)),
                None,
                None,
            )
            .unwrap();
        store
            .register_domain(
                "untouched",
                Some("updated description"),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        let domains = store.list_domains().unwrap();
        let d = domains.iter().find(|d| d.name == "untouched").unwrap();
        assert_eq!(d.retention_days, Some(15));
    }

    #[test]
    fn set_knowledge_retention_clamps_to_ceiling() {
        let store = fixture();
        let id = store.remember("some-domain", "a fact").unwrap();
        store
            .set_knowledge_retention(id, Some(200), Some(60))
            .unwrap();
        let item = store.get_knowledge_by_id(id).unwrap().unwrap();
        assert_eq!(item.row.retention_days, Some(60));
    }
}

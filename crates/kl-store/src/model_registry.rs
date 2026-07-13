//! Model registry (per-harness available models/capability tiers/cost) and
//! routing rules (which model to use for a domain/task/complexity combo).
//! Kept together: routing rules are meaningless without the registry they
//! reference, and both are small enough that a further split would be
//! artificial (see refactor plan, Phase C).

use anyhow::Result;
use kl_core::{ModelRegistryRow, RoutingRuleRow};
use rusqlite::{params, OptionalExtension};

use crate::Store;

impl Store {
    pub fn configure_model(
        &self,
        harness: &str,
        model_id: &str,
        tier: &str,
        cost: f64,
        sub_agent: Option<&str>,
    ) -> Result<()> {
        let c = self.conn.lock().unwrap();
        // `ON CONFLICT(harness,model_id,sub_agent_name)` never fires when
        // sub_agent_name is NULL — SQLite treats NULL as distinct from NULL
        // for uniqueness purposes, so the PRIMARY KEY doesn't actually
        // dedupe rows with no sub-agent. Delete-then-insert with `IS`
        // (which does match NULL correctly) instead of relying on the
        // conflict target.
        c.execute(
            "DELETE FROM model_registry WHERE harness=?1 AND model_id=?2 AND sub_agent_name IS ?3",
            params![harness, model_id, sub_agent],
        )?;
        c.execute("INSERT INTO model_registry(harness,model_id,capability_tier,cost_weight,sub_agent_name) VALUES(?1,?2,?3,?4,?5)", params![harness,model_id,tier,cost,sub_agent])?;
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

    /// Harness strings to try, in priority order: the exact string first (so
    /// a deliberately version-scoped registration still wins if present),
    /// then every other harness on record whose normalized form matches —
    /// covers both the version-suffix and casing mismatches above.
    fn candidate_harnesses(c: &rusqlite::Connection, harness: &str) -> Result<Vec<String>> {
        let mut stmt = c.prepare(
            "SELECT harness FROM model_registry UNION SELECT harness FROM routing_rules",
        )?;
        let all: Vec<String> = stmt
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let target = kl_core::normalize_harness(harness);
        let mut out = Vec::new();
        if all.iter().any(|h| h == harness) {
            out.push(harness.to_string());
        }
        for h in all {
            if h != harness && kl_core::normalize_harness(&h) == target {
                out.push(h);
            }
        }
        Ok(out)
    }

    pub fn recommend_model(
        &self,
        harness: &str,
        domain_type: &str,
        task_type: &str,
        complexity: &str,
    ) -> Result<Option<(String, f64, String)>> {
        let c = self.conn.lock().unwrap();
        let candidates = Self::candidate_harnesses(&c, harness)?;
        for h in &candidates {
            let configured: Option<(String, f64)> = c.query_row("SELECT model_id,cost_weight FROM model_registry WHERE harness=?1 AND model_id=(SELECT model_id FROM routing_rules WHERE harness=?1 AND domain_type=?2 AND task_type=?3 AND complexity_tier=?4) LIMIT 1", params![h,domain_type,task_type,complexity], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
            if let Some((model, cost)) = configured {
                return Ok(Some((model, cost, "routing rule match".into())));
            }
        }
        // A rule matched on (domain_type, task_type, complexity_tier) but its
        // model_id isn't registered under this harness — surface that
        // distinctly from "no rule at all" so a dangling reference doesn't
        // silently look identical to an unconfigured tier.
        for h in &candidates {
            let dangling: Option<String> = c
                .query_row(
                    "SELECT model_id FROM routing_rules WHERE harness=?1 AND domain_type=?2 AND task_type=?3 AND complexity_tier=?4",
                    params![h, domain_type, task_type, complexity],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(model_id) = dangling {
                return Ok(Some((
                    model_id.clone(),
                    0.0,
                    format!(
                        "routing rule points to unregistered model '{model_id}' for harness '{h}' — register it or fix the rule"
                    ),
                )));
            }
        }
        for h in &candidates {
            let fallback: Option<(String, f64)> = c.query_row("SELECT model_id,cost_weight FROM model_registry WHERE harness=?1 ORDER BY cost_weight ASC LIMIT 1", params![h], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
            if let Some((m, cost)) = fallback {
                return Ok(Some((
                    m,
                    cost,
                    "no exact rule; cheapest configured model".into(),
                )));
            }
        }
        Ok(None)
    }
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
    fn configure_model_upserts_rather_than_duplicates_with_null_sub_agent() {
        let store = fixture();
        store
            .configure_model("claude-code", "opus", "balanced", 4.0, None)
            .unwrap();
        // Re-configuring the same (harness, model_id) with sub_agent_name=NULL
        // must replace the existing row, not add a second one — this is the
        // exact case `ON CONFLICT` silently failed to dedupe.
        store
            .configure_model("claude-code", "opus", "heavy-reasoning", 20.0, None)
            .unwrap();
        let rows = store.list_model_registry().unwrap();
        let matching: Vec<_> = rows
            .iter()
            .filter(|r| r.harness == "claude-code" && r.model_id == "opus")
            .collect();
        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0].capability_tier, "heavy-reasoning");
        assert_eq!(matching[0].cost_weight, 20.0);
    }

    #[test]
    fn last_episode_ts_for_finds_most_recent_matching_action() {
        let store = fixture();
        store
            .log_episode_auto(
                "run-1",
                Some("model_registry"),
                Some("configure_model_registry action=add harness=claude-code model=opus"),
                Some("model registry updated"),
                Some("success"),
                None,
                None,
                None,
                None,
            )
            .unwrap();
        let second_id = store
            .log_episode_auto(
                "run-1",
                Some("model_registry"),
                Some("configure_model_registry action=add harness=claude-code model=haiku"),
                Some("model registry updated"),
                Some("success"),
                None,
                None,
                None,
                None,
            )
            .unwrap();

        let ts = store
            .last_episode_ts_for("model_registry", "harness=claude-code")
            .expect("expected a matching episode ts");
        let expected: i64 = store
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT ts FROM episodes WHERE id = ?1",
                params![second_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ts, expected);

        assert!(store
            .last_episode_ts_for("model_registry", "harness=cursor")
            .is_none());
        assert!(store
            .last_episode_ts_for("model_recommendation", "harness=claude-code")
            .is_none());
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
    fn recommend_model_matches_versioned_and_differently_cased_harness() {
        let store = fixture();
        store
            .configure_model("claude-code", "opus", "heavy-reasoning", 10.0, None)
            .unwrap();
        store
            .add_routing_rule("claude-code", "frontend", "feature", "high", "opus")
            .unwrap();
        // Auto-captured clientInfo harness carries a version suffix that
        // never equals what a user registered rules/models under by hand.
        let (model, _cost, reason) = store
            .recommend_model("claude-code/2.1.207", "frontend", "feature", "high")
            .unwrap()
            .unwrap();
        assert_eq!(model, "opus");
        assert_eq!(reason, "routing rule match");
        // Different casing (e.g. the literal clientInfo.name "Claude Code").
        let (model, ..) = store
            .recommend_model("Claude Code", "frontend", "feature", "high")
            .unwrap()
            .unwrap();
        assert_eq!(model, "opus");
    }

    #[test]
    fn recommend_model_reports_dangling_routing_rule_distinctly() {
        let store = fixture();
        store
            .configure_model("claude-code", "opus", "heavy-reasoning", 10.0, None)
            .unwrap();
        // Rule references a model that was never registered.
        store
            .add_routing_rule("claude-code", "general", "any", "low", "haiku-nonexistent")
            .unwrap();
        let (model, _cost, reason) = store
            .recommend_model("claude-code", "general", "any", "low")
            .unwrap()
            .unwrap();
        assert_eq!(model, "haiku-nonexistent");
        assert!(
            reason.contains("unregistered model"),
            "reason was: {reason}"
        );
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

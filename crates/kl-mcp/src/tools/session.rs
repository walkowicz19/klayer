//! Session/episode MCP tools: estimate_task_complexity, log_episode,
//! log_work, recall_session.

use rmcp::{
    handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router,
    ErrorData as McpError,
};

use super::{
    err, json_ok, resolve_harness, text_ok, ComplexityParams, EpisodeParams, Klayer,
    LogWorkParams, RecallSessionParams,
};

#[tool_router(router = session_tool_router, vis = "pub(crate)")]
impl Klayer {
    #[tool(
        description = "Estimate deterministic task complexity from indexed codebase size or ingested knowledge density and return a transparent advisory model recommendation."
    )]
    async fn estimate_task_complexity(
        &self,
        Parameters(p): Parameters<ComplexityParams>,
    ) -> Result<CallToolResult, McpError> {
        let harness =
            resolve_harness(p.harness.clone(), self.default_harness()).ok_or_else(|| {
                err(
                    "harness was not supplied and no clientInfo harness was captured for this \
                     connection — pass harness explicitly",
                )
            })?;
        let code = match p.repo.as_deref() {
            Some(repo) => self.code_store.stats_for_repo(repo).await.map_err(err)?,
            None => self.code_store.stats().await.map_err(err)?,
        };
        let (source, signal) = if code.files > 0 {
            ("codebase", (code.files + code.chunks / 4) as i64)
        } else {
            let n = p
                .domain
                .as_deref()
                .map(|d| {
                    self.store
                        .list_knowledge(d, None, None)
                        .map(|v| v.len())
                        .unwrap_or(0)
                })
                .unwrap_or(0);
            ("domain", n as i64)
        };
        let complexity = if signal >= 50 {
            "high"
        } else if signal >= 10 {
            "medium"
        } else {
            "low"
        };
        let recommendation = self
            .store
            .recommend_model(&harness, &p.domain_type, &p.task_type, complexity)
            .map_err(err)?;
        let recommended_summary = recommendation
            .as_ref()
            .map(|(model, _cost, reason)| format!("{model} ({reason})"))
            .unwrap_or_else(|| "none".to_string());
        self.store
            .log_episode_auto(
                &self.session_run_id,
                Some("model_recommendation"),
                Some(&format!(
                    "estimate_task_complexity harness={} domain_type={} task_type={} complexity={}",
                    harness, p.domain_type, p.task_type, complexity
                )),
                Some(&format!("recommended model={recommended_summary}")),
                Some("success"),
                p.domain.as_deref(),
                None,
                None,
                None,
            )
            .ok();
        json_ok(
            &serde_json::json!({"complexity_tier":complexity,"harness":harness,"signal_source":source,"signal":signal,"repo":p.repo,"recommendation":recommendation.map(|(model,cost,reason)|serde_json::json!({"model_id":model,"cost_weight":cost,"reason":reason})),"advisory":true}),
        )
    }

    #[tool(
        description = "Record one step of an agentic run into episodic memory for auditability."
    )]
    fn log_episode(
        &self,
        Parameters(p): Parameters<EpisodeParams>,
    ) -> Result<CallToolResult, McpError> {
        let id = self
            .store
            .log_episode(
                &p.run_id,
                p.step,
                p.stage.as_deref(),
                p.action.as_deref(),
                p.observation.as_deref(),
                p.outcome.as_deref(),
            )
            .map_err(err)?;
        text_ok(format!(
            "Logged episode #{id} (run={}, step={}).",
            p.run_id, p.step
        ))
    }

    #[tool(
        description = "Record one curated entry into a codebase's session journal — what you accomplished ('done'), what failed ('failed'), a mistake to NEVER repeat ('avoid'), a decision made ('decision'), or a 'note'. This is durable across sessions and per-repo. Log as you work so a future session can recall_session() and not repeat your mistakes."
    )]
    async fn log_work(
        &self,
        Parameters(p): Parameters<LogWorkParams>,
    ) -> Result<CallToolResult, McpError> {
        const KINDS: [&str; 5] = ["done", "failed", "avoid", "decision", "note"];
        if !KINDS.contains(&p.kind.as_str()) {
            return Err(err(
                "kind must be 'done', 'failed', 'avoid', 'decision', or 'note'",
            ));
        }
        let id = self
            .session_store
            .log_work(
                &p.repo,
                &p.kind,
                &p.title,
                p.body.as_deref(),
                p.is_checkpoint.unwrap_or(false),
            )
            .await
            .map_err(err)?;
        self.store
            .log_episode_auto(
                &self.session_run_id,
                Some("log_work"),
                Some(&format!("log_work repo={} kind={}", p.repo, p.kind)),
                Some(&format!("journaled entry #{id} ({})", p.kind)),
                Some("success"),
                None,
                None,
                None,
                None,
            )
            .ok();
        text_ok(format!(
            "Logged {} entry #{id} for '{}' session memory.",
            p.kind, p.repo
        ))
    }

    #[tool(
        description = "Replay a codebase's session journal (newest first) to re-establish context at the START of a session working on an indexed repo: what was accomplished, what failed, and mistakes to avoid. ALWAYS call this before starting substantial work on a repo you have journaled before, so you do not repeat past mistakes."
    )]
    async fn recall_session(
        &self,
        Parameters(p): Parameters<RecallSessionParams>,
    ) -> Result<CallToolResult, McpError> {
        let k = p.k.unwrap_or(30) as usize;
        let mode = p.mode.as_deref().unwrap_or("recent_context");
        if !matches!(mode, "recent_context" | "full_session_summary") {
            return Err(err(
                "mode must be 'recent_context' or 'full_session_summary'",
            ));
        }
        let rows = self
            .session_store
            .recall_session(
                &p.repo,
                p.kind.as_deref(),
                k,
                mode == "full_session_summary",
            )
            .await
            .map_err(err)?;
        self.store
            .log_episode_auto(
                &self.session_run_id,
                Some("recall_session"),
                Some(&format!("recall_session repo={}", p.repo)),
                Some(&format!("returned {} journal entries", rows.len())),
                Some("success"),
                None,
                None,
                None,
                None,
            )
            .ok();
        json_ok(&rows)
    }

}

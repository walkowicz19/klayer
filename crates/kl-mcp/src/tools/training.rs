//! Training-data MCP tools: capture/author/promote examples, dataset export,
//! weak-example queuing, and topic-seeded question stubs.

use kl_train::PromoteOutcome;
use rmcp::{
    handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router,
    ErrorData as McpError,
};

use super::{
    err, json_ok, text_ok, validate_label_type, AuthorExampleParams, CaptureExampleParams,
    ExportDatasetParams, IdParams, Klayer, ListTrainingParams, QueueWeakParams,
    SeedFromTopicsParams,
};

#[tool_router(router = training_tool_router, vis = "pub(crate)")]
impl Klayer {
    #[tool(
        description = "Capture a candidate training example into the training store at trust='proposed'. Provenance must be 'teacher' (a stronger model) or 'student' (the model being trained). STUDENT rows can NEVER be promoted (model-collapse guard). Omit assistant_content to file a question-stub awaiting a teacher answer. klayer only stores rows — labeling/verification happen in a separate project."
    )]
    async fn capture_example(
        &self,
        Parameters(p): Parameters<CaptureExampleParams>,
    ) -> Result<CallToolResult, McpError> {
        let label_type = p.label_type.as_deref().unwrap_or("grounded");
        validate_label_type(label_type)?;
        let provenance = p.provenance.as_deref().unwrap_or("teacher");
        if !matches!(provenance, "teacher" | "student") {
            return Err(err("provenance must be 'teacher' or 'student' (use author_example for human-authored rows)"));
        }
        let id = self
            .train_store
            .capture_example(
                &p.domain,
                p.system_prompt.as_deref(),
                &p.user_content,
                p.assistant_content.as_deref(),
                label_type,
                provenance,
                p.retrieval_ref.as_deref(),
                p.verify_log.as_deref(),
            )
            .await
            .map_err(err)?;
        let observation =
            format!("captured training example #{id} (provenance={provenance}, trust=proposed)");
        self.store
            .log_episode_auto(
                &self.session_run_id,
                Some("capture_example"),
                Some(&format!(
                    "capture_example domain={} provenance={}",
                    p.domain, provenance
                )),
                Some(&observation),
                Some("success"),
                Some(&p.domain),
                None,
                None,
                None,
            )
            .ok();
        text_ok(format!(
            "Captured training example #{id} in '{}' (provenance={provenance}, trust=proposed).",
            p.domain
        ))
    }

    #[tool(
        description = "Author a human-written training example. Stored at trust='user', provenance='human' — exportable immediately (no promotion needed). assistant_content is required."
    )]
    async fn author_example(
        &self,
        Parameters(p): Parameters<AuthorExampleParams>,
    ) -> Result<CallToolResult, McpError> {
        let label_type = p.label_type.as_deref().unwrap_or("grounded");
        validate_label_type(label_type)?;
        let id = self
            .train_store
            .author_example(
                &p.domain,
                p.system_prompt.as_deref(),
                &p.user_content,
                &p.assistant_content,
                label_type,
                p.retrieval_ref.as_deref(),
                p.verify_log.as_deref(),
            )
            .await
            .map_err(err)?;
        let observation = format!("authored training example #{id} (trust=user)");
        self.store
            .log_episode_auto(
                &self.session_run_id,
                Some("author_example"),
                Some(&format!("author_example domain={}", p.domain)),
                Some(&observation),
                Some("success"),
                Some(&p.domain),
                None,
                None,
                None,
            )
            .ok();
        text_ok(format!(
            "Authored training example #{id} in '{}' (trust=user, exportable).",
            p.domain
        ))
    }

    #[tool(
        description = "Validation gate for training data: promote a proposed example to 'reviewed' (exportable). REFUSES any row with provenance='student' — this is the model-collapse guard. Only teacher- and human-origin rows can become training data."
    )]
    async fn promote_example(
        &self,
        Parameters(p): Parameters<IdParams>,
    ) -> Result<CallToolResult, McpError> {
        let outcome = self.train_store.promote_example(p.id).await.map_err(err)?;
        let (observation, message) = match outcome {
            PromoteOutcome::Promoted => (
                format!("promoted training example #{} to trust=reviewed", p.id),
                format!("Promoted training example #{} to trust=reviewed.", p.id),
            ),
            PromoteOutcome::BlockedStudent => (
                format!("REFUSED to promote #{}: provenance=student (collapse guard)", p.id),
                format!("Refused: training example #{} has provenance='student' and can never be promoted (model-collapse guard). Capture a teacher-labeled version instead.", p.id),
            ),
            PromoteOutcome::NotFound => (
                format!("no proposed training example #{} to promote", p.id),
                format!("No proposed training example #{} to promote.", p.id),
            ),
        };
        self.store
            .log_episode_auto(
                &self.session_run_id,
                Some("promote_example"),
                Some(&format!("promote_example id={}", p.id)),
                Some(&observation),
                Some("success"),
                None,
                None,
                None,
                None,
            )
            .ok();
        text_ok(message)
    }

    #[tool(
        description = "List training examples, newest first, optionally filtered by domain and trust ('proposed' | 'reviewed' | 'user'). Use trust='proposed' to review the worklist (including student question-stubs awaiting teacher answers)."
    )]
    async fn list_training(
        &self,
        Parameters(p): Parameters<ListTrainingParams>,
    ) -> Result<CallToolResult, McpError> {
        let rows = self
            .train_store
            .list_training(p.domain.as_deref(), p.trust.as_deref())
            .await
            .map_err(err)?;
        json_ok(&rows)
    }

    #[tool(
        description = "Export the training dataset as chat JSONL — one '<domain>.jsonl' file per domain in out_dir. ONLY reviewed + user rows are exported (the enforcement gate); proposed rows and empty stubs are skipped. Each line is {\"messages\":[system?,user,assistant]}."
    )]
    async fn export_dataset(
        &self,
        Parameters(p): Parameters<ExportDatasetParams>,
    ) -> Result<CallToolResult, McpError> {
        let files = self
            .train_store
            .export_dataset(p.domain.as_deref(), &p.out_dir)
            .await
            .map_err(err)?;
        let total: usize = files.iter().map(|f| f.rows).sum();
        let observation = format!(
            "exported {} rows across {} file(s) to {}",
            total,
            files.len(),
            p.out_dir
        );
        self.store
            .log_episode_auto(
                &self.session_run_id,
                Some("export_dataset"),
                Some(&format!("export_dataset out_dir={}", p.out_dir)),
                Some(&observation),
                Some("success"),
                None,
                None,
                None,
                None,
            )
            .ok();
        json_ok(&files)
    }

    #[tool(
        description = "Capture faucet: scan the agentic audit trail for recall queries the knowledge base could not answer (<= threshold hits) and file them as proposed 'student' question-stubs for a teacher to answer later. Deduplicated against existing rows. Default threshold 0 (only zero-hit recalls)."
    )]
    async fn queue_weak(
        &self,
        Parameters(p): Parameters<QueueWeakParams>,
    ) -> Result<CallToolResult, McpError> {
        let threshold = p.threshold.unwrap_or(0);
        let episodes = self.store.list_episodes(None).map_err(err)?;
        let n = self
            .train_store
            .queue_weak(&episodes, threshold)
            .await
            .map_err(err)?;
        let observation = format!("queued {n} weak-query stubs (threshold={threshold})");
        self.store
            .log_episode_auto(
                &self.session_run_id,
                Some("queue_weak"),
                Some(&format!("queue_weak threshold={threshold}")),
                Some(&observation),
                Some("success"),
                None,
                None,
                None,
                None,
            )
            .ok();
        text_ok(format!("Queued {n} weak-query question-stubs (threshold={threshold}, trust=proposed, provenance=student)."))
    }

    #[tool(
        description = "Coverage faucet: enumerate an EXISTING domain's curated knowledge and stages into diverse proposed 'student' question-stubs (recall / application / debugging / what's-wrong). Does NOT create or register domains — the domain must already exist. Deduplicated against existing rows."
    )]
    async fn seed_from_topics(
        &self,
        Parameters(p): Parameters<SeedFromTopicsParams>,
    ) -> Result<CallToolResult, McpError> {
        if !self.store.domain_exists(&p.domain).map_err(err)? {
            return Err(err(format!("domain '{}' does not exist — seed_from_topics never creates domains; register it first", p.domain)));
        }
        let knowledge = self
            .store
            .list_knowledge(&p.domain, None, None)
            .map_err(err)?;
        let stages = self.store.list_stages("default").map_err(err)?;
        let n = self
            .train_store
            .seed_from_topics(&p.domain, p.stage.as_deref(), &knowledge, &stages)
            .await
            .map_err(err)?;
        let observation = format!("seeded {n} topic stubs for domain '{}'", p.domain);
        self.store
            .log_episode_auto(
                &self.session_run_id,
                Some("seed_from_topics"),
                Some(&format!(
                    "seed_from_topics domain={} stage={:?}",
                    p.domain, p.stage
                )),
                Some(&observation),
                Some("success"),
                Some(&p.domain),
                None,
                None,
                None,
            )
            .ok();
        text_ok(format!(
            "Seeded {n} question-stubs for '{}' (trust=proposed, provenance=student).",
            p.domain
        ))
    }

    // ----- repo-scoped session memory (journal) ----------------------------

}

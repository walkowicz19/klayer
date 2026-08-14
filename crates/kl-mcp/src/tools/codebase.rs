//! Codebase indexing MCP tools: index_codebase, search_code, list_repos.

use std::sync::Arc;

use rmcp::{
    handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router,
    ErrorData as McpError,
};

use super::{err, json_ok, text_ok, IndexCodebaseParams, Klayer, SearchCodeParams};

/// Clears the in-flight index flag even if the background task panics.
struct IndexFinish(Arc<kl_code::CodeStore>, String);
impl Drop for IndexFinish {
    fn drop(&mut self) {
        self.0.finish_index(&self.1);
    }
}

#[tool_router(router = codebase_tool_router, vis = "pub(crate)")]
impl Klayer {
    #[tool(
        description = "Start indexing a local codebase directory into persistent code memory. Returns immediately so the host does not time out on large trees — the walk and FTS write continue in the background. Call list_repos() until `indexing` is false and file/chunk counts settle, then search_code(). Re-indexing the same path refreshes the index; a second call while one is already running is a no-op."
    )]
    #[allow(dead_code)]
    async fn index_codebase(
        &self,
        Parameters(p): Parameters<IndexCodebaseParams>,
    ) -> Result<CallToolResult, McpError> {
        let canonical = std::fs::canonicalize(&p.path)
            .map_err(|e| err(anyhow::anyhow!("resolving path '{}': {e}", p.path)))?;
        let canon_str = canonical.to_string_lossy().replace('\\', "/");

        if !self.code_store.begin_index(&canon_str) {
            return text_ok(format!(
                "Indexing already in progress for '{canon_str}'. Call list_repos() — the `indexing` flag stays true until it finishes."
            ));
        }

        let code_store = Arc::clone(&self.code_store);
        let store = Arc::clone(&self.store);
        let run_id = self.session_run_id.clone();
        let path = p.path.clone();
        let name = p.name.clone();
        let canon_for_job = canon_str.clone();

        tokio::spawn(async move {
            let _guard = IndexFinish(Arc::clone(&code_store), canon_for_job);
            match code_store.index_repo(&path, name.as_deref()).await {
                Ok(stats) => {
                    let observation = format!(
                        "indexed repo '{}': {} files, {} chunks",
                        path, stats.files, stats.chunks
                    );
                    store
                        .log_episode_auto(
                            &run_id,
                            Some("indexing"),
                            Some(&format!("index_codebase path={path}")),
                            Some(&observation),
                            Some("success"),
                            None,
                            None,
                            None,
                            None,
                        )
                        .ok();
                    tracing::info!("{}", observation);
                }
                Err(e) => {
                    let observation = format!("index_codebase failed for '{path}': {e:#}");
                    store
                        .log_episode_auto(
                            &run_id,
                            Some("indexing"),
                            Some(&format!("index_codebase path={path}")),
                            Some(&observation),
                            Some("error"),
                            None,
                            None,
                            None,
                            None,
                        )
                        .ok();
                    tracing::error!("{}", observation);
                }
            }
        });

        text_ok(format!(
            "Indexing started in the background for '{canon_str}'. Large trees can take several minutes; this call returns immediately so the host does not time out. Call list_repos() until `indexing` is false and file/chunk counts settle, then search_code(). Do not re-run index_codebase on this path until it finishes."
        ))
    }

    #[tool(
        description = "Search indexed codebases using full-text search over function names, symbols, file paths, and code content. Returns grounded snippets with exact file paths and line numbers. Always call this before answering questions about an indexed codebase — it never forgets across sessions."
    )]
    #[allow(dead_code)]
    async fn search_code(
        &self,
        Parameters(p): Parameters<SearchCodeParams>,
    ) -> Result<CallToolResult, McpError> {
        let limit = p.limit.unwrap_or(8) as usize;
        let hits = self
            .code_store
            .search(&p.query, p.repo.as_deref(), limit)
            .await
            .map_err(err)?;
        let observation = format!("returned {} code hits", hits.len());
        self.store
            .log_episode_auto(
                &self.session_run_id,
                Some("code_search"),
                Some(&format!("search_code query={}", p.query)),
                Some(&observation),
                Some("success"),
                None,
                None,
                None,
                None,
            )
            .ok();
        json_ok(&hits)
    }

    #[tool(
        description = "List all indexed code repositories with their file/chunk counts, last-indexed timestamp, and whether a background index job is still running (`indexing`)."
    )]
    async fn list_repos(&self) -> Result<CallToolResult, McpError> {
        let repos = self.code_store.list_repos().await.map_err(err)?;
        json_ok(&repos)
    }
}

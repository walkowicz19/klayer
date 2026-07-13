//! Codebase indexing MCP tools: index_codebase, search_code, list_repos.

use rmcp::{
    handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router,
    ErrorData as McpError,
};

use super::{err, json_ok, text_ok, IndexCodebaseParams, Klayer, SearchCodeParams};

#[tool_router(router = codebase_tool_router, vis = "pub(crate)")]
impl Klayer {
    #[tool(
        description = "Index a local codebase directory into persistent code memory. After indexing, search_code() can recall any function, struct, file, or pattern across sessions — the LLM never forgets what was indexed. Re-indexing the same path refreshes the index."
    )]
    #[allow(dead_code)]
    async fn index_codebase(
        &self,
        Parameters(p): Parameters<IndexCodebaseParams>,
    ) -> Result<CallToolResult, McpError> {
        let stats = self
            .code_store
            .index_repo(&p.path, p.name.as_deref())
            .await
            .map_err(err)?;
        let observation = format!(
            "indexed repo '{}': {} files, {} chunks",
            p.path, stats.files, stats.chunks
        );
        self.store
            .log_episode_auto(
                &self.session_run_id,
                Some("indexing"),
                Some(&format!("index_codebase path={}", p.path)),
                Some(&observation),
                Some("success"),
                None,
                None,
                None,
                None,
            )
            .ok();
        let mut message = format!(
            "Indexed '{}': {} files, {} chunks ({} skipped).",
            p.path, stats.files, stats.chunks, stats.skipped
        );
        if !stats.skip_reasons.is_empty() {
            message.push_str("\nSkipped files:\n- ");
            message.push_str(&stats.skip_reasons.join("\n- "));
        }
        if !stats.warnings.is_empty() {
            message.push_str("\nWarnings:\n- ");
            message.push_str(&stats.warnings.join("\n- "));
        }
        message.push_str("\nUse search_code() to recall any symbol or pattern.");
        text_ok(message)
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
        description = "List all indexed code repositories with their file/chunk counts and last-indexed timestamp."
    )]
    async fn list_repos(&self) -> Result<CallToolResult, McpError> {
        let repos = self.code_store.list_repos().await.map_err(err)?;
        json_ok(&repos)
    }

}

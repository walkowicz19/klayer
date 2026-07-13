//! Knowledge-domain MCP tools: remember/propose/promote, recall, domain
//! registration and ACLs, conflicts, listing, and the clear_*/forget family.

use kl_core::Kind;
use rmcp::{
    handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router,
    ErrorData as McpError,
};

use super::{
    err, json_ok, recall_with_framing, text_ok, ClearDomainParams, ConflictListParams,
    ConflictResolveParams, DomainPermissionParams, ForgetRepoParams, IdParams, Klayer,
    ListEpisodesParams, ListKnowledgeParams, ListSourcesParams, PreferenceParams, ProposeParams,
    RecallParams, RegisterDomainParams, RememberParams,
};

#[tool_router(router = knowledge_tool_router, vis = "pub(crate)")]
impl Klayer {
    #[tool(
        description = "Retrieve grounded knowledge for a domain. Returns reference chunks and curated knowledge with provenance and trust. Call this BEFORE answering in a known domain."
    )]
    fn recall(&self, Parameters(p): Parameters<RecallParams>) -> Result<CallToolResult, McpError> {
        if !self
            .store
            .domain_allowed(p.identity.as_deref(), &p.domain)
            .map_err(err)?
        {
            self.notify.record_denial(&p.domain);
            return Err(err(format!(
                "access denied for identity '{}' to domain '{}'",
                p.identity.as_deref().unwrap_or("default"),
                p.domain
            )));
        }
        let kind = p.kind.as_deref().and_then(Kind::parse);
        let k = p.k.unwrap_or(6) as usize;
        let hits = recall_with_framing(&self.store, &p.domain, &p.query, kind, k).map_err(err)?;
        let observation = format!("returned {} hits", hits.len());
        let episode_id = self
            .store
            .log_episode_auto(
                &self.session_run_id,
                Some("recall"),
                Some(&format!("recall domain={} query={}", p.domain, p.query)),
                Some(&observation),
                Some("success"),
                Some(&p.domain),
                p.model.as_deref(),
                p.tokens_used,
                p.cost,
            )
            .map_err(err)?;
        let ids: Vec<i64> = hits
            .iter()
            .filter_map(|h| h.provenance.as_deref())
            .filter_map(|p| p.strip_prefix("knowledge:#"))
            .filter_map(|v| v.parse().ok())
            .collect();
        self.store
            .set_episode_knowledge_ids(episode_id, &ids)
            .map_err(err)?;
        json_ok(&hits)
    }

    #[tool(
        description = "Store a user-authored fact. Trust='user' (highest), immediately enforceable."
    )]
    fn remember(
        &self,
        Parameters(p): Parameters<RememberParams>,
    ) -> Result<CallToolResult, McpError> {
        if !self
            .store
            .domain_allowed(p.identity.as_deref(), &p.domain)
            .map_err(err)?
        {
            self.notify.record_denial(&p.domain);
            return Err(err(format!("access denied to domain '{}'", p.domain)));
        }
        let id = self.store.remember(&p.domain, &p.statement).map_err(err)?;
        self.notify_if_conflict(id);
        let observation = format!("remembered fact #{} (trust=user)", id);
        self.store
            .log_episode_auto(
                &self.session_run_id,
                Some("remember"),
                Some(&format!(
                    "remember domain={} statement={}",
                    p.domain, p.statement
                )),
                Some(&observation),
                Some("success"),
                Some(&p.domain),
                p.model.as_deref(),
                p.tokens_used,
                p.cost,
            )
            .ok();
        text_ok(format!(
            "Remembered fact #{id} in '{}' (trust=user).",
            p.domain
        ))
    }

    #[tool(
        description = "Propose candidate knowledge extracted from sources. Stored as trust='proposed' and NOT enforced until promote() is called."
    )]
    fn propose(
        &self,
        Parameters(p): Parameters<ProposeParams>,
    ) -> Result<CallToolResult, McpError> {
        if !self
            .store
            .domain_allowed(p.identity.as_deref(), &p.domain)
            .map_err(err)?
        {
            self.notify.record_denial(&p.domain);
            return Err(err(format!("access denied to domain '{}'", p.domain)));
        }
        let kind = Kind::parse(&p.kind)
            .ok_or_else(|| err("kind must be 'fact', 'rule', or 'procedure'"))?;
        let id = self
            .store
            .propose(
                kind,
                &p.domain,
                p.stage.as_deref(),
                &p.title,
                &p.body,
                p.trigger.as_deref(),
                p.severity.as_deref(),
                p.remediation.as_deref(),
                None,
            )
            .map_err(err)?;
        self.notify_if_conflict(id);
        let observation = format!("proposed candidate {} #{}", p.kind, id);
        self.store
            .log_episode_auto(
                &self.session_run_id,
                Some("propose"),
                Some(&format!(
                    "propose domain={} kind={} title={}",
                    p.domain, p.kind, p.title
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
            "Proposed {} #{id} in '{}' (trust=proposed, not enforced).",
            p.kind, p.domain
        ))
    }

    #[tool(
        description = "Validation gate: promote a proposed item to 'reviewed' (enforceable). This is the only path from suggestion to enforced rule."
    )]
    fn promote(&self, Parameters(p): Parameters<IdParams>) -> Result<CallToolResult, McpError> {
        let ok = self.store.promote(p.id).map_err(err)?;
        let observation = if ok {
            format!("promoted knowledge #{} to trust=reviewed", p.id)
        } else {
            format!("no proposed item #{} found to promote", p.id)
        };
        self.store
            .log_episode_auto(
                &self.session_run_id,
                Some("promote"),
                Some(&format!("promote id={}", p.id)),
                Some(&observation),
                Some("success"),
                None,
                None,
                None,
                None,
            )
            .ok();
        if ok {
            text_ok(format!("Promoted knowledge #{} to trust=reviewed.", p.id))
        } else {
            text_ok(format!("No proposed item #{} to promote.", p.id))
        }
    }

    #[tool(description = "Delete a knowledge item by id.")]
    fn forget(&self, Parameters(p): Parameters<IdParams>) -> Result<CallToolResult, McpError> {
        let ok = self.store.forget(p.id).map_err(err)?;
        let observation = if ok {
            format!("deleted knowledge item #{}", p.id)
        } else {
            format!("no knowledge item #{} found to delete", p.id)
        };
        self.store
            .log_episode_auto(
                &self.session_run_id,
                Some("forget"),
                Some(&format!("forget id={}", p.id)),
                Some(&observation),
                Some("success"),
                None,
                None,
                None,
                None,
            )
            .ok();
        text_ok(if ok {
            format!("Forgot knowledge #{}.", p.id)
        } else {
            format!("No item #{}.", p.id)
        })
    }

    #[tool(description = "Store a durable user preference (always honored, outranks web data).")]
    fn set_preference(
        &self,
        Parameters(p): Parameters<PreferenceParams>,
    ) -> Result<CallToolResult, McpError> {
        let scope = p.scope.as_deref().unwrap_or("global");
        let id = self
            .store
            .set_preference(scope, &p.statement)
            .map_err(err)?;
        let observation = format!("stored preference #{} (scope={})", id, scope);
        self.store
            .log_episode_auto(
                &self.session_run_id,
                Some("set_preference"),
                Some(&format!(
                    "set_preference scope={} statement={}",
                    scope, p.statement
                )),
                Some(&observation),
                Some("success"),
                None,
                None,
                None,
                None,
            )
            .ok();
        text_ok(format!("Stored preference #{id} (scope={scope})."))
    }

    #[tool(description = "List registered domains with doc and enforced-rule counts.")]
    fn list_domains(&self) -> Result<CallToolResult, McpError> {
        let domains = self.store.list_domains().map_err(err)?;
        json_ok(&domains)
    }

    #[tool(
        description = "Register or update a domain with a description and an authored query hint."
    )]
    fn register_domain(
        &self,
        Parameters(p): Parameters<RegisterDomainParams>,
    ) -> Result<CallToolResult, McpError> {
        let retention_days = if p.clear_retention == Some(true) {
            Some(None)
        } else {
            p.retention_days.map(Some)
        };
        self.store
            .register_domain(
                &p.name,
                p.description.as_deref(),
                p.query_hint.as_deref(),
                p.enforced,
                p.redact_enabled,
                retention_days,
                None,
                self.max_retention_days,
            )
            .map_err(err)?;
        let observation = format!("registered/updated domain '{}'", p.name);
        self.store
            .log_episode_auto(
                &self.session_run_id,
                Some("register_domain"),
                Some(&format!("register_domain name={}", p.name)),
                Some(&observation),
                Some("success"),
                Some(&p.name),
                None,
                None,
                None,
            )
            .ok();
        text_ok(format!("Registered domain '{}'.", p.name))
    }

    #[tool(
        description = "Configure opt-in domain access control for an identity. Once any permission exists for a domain, identities without an allowed row are denied."
    )]
    fn set_domain_permission(
        &self,
        Parameters(p): Parameters<DomainPermissionParams>,
    ) -> Result<CallToolResult, McpError> {
        self.store
            .set_domain_permission(&p.identity, &p.domain, p.allowed)
            .map_err(err)?;
        self.store
            .log_episode_auto(
                &self.session_run_id,
                Some("domain_acl"),
                Some(&format!(
                    "set_domain_permission identity={} domain={} allowed={}",
                    p.identity, p.domain, p.allowed
                )),
                Some("permission updated"),
                Some("success"),
                Some(&p.domain),
                None,
                None,
                None,
            )
            .ok();
        text_ok(format!(
            "Domain permission updated for '{}' on '{}': {}.",
            p.identity, p.domain, p.allowed
        ))
    }

    #[tool(
        description = "List knowledge items in a domain. Use trust='proposed' to review pending items and get their ids for promote() or forget()."
    )]
    fn list_knowledge(
        &self,
        Parameters(p): Parameters<ListKnowledgeParams>,
    ) -> Result<CallToolResult, McpError> {
        let kind = p.kind.as_deref().and_then(Kind::parse);
        let rows = self
            .store
            .list_knowledge(&p.domain, p.trust.as_deref(), kind)
            .map_err(err)?;
        json_ok(&rows)
    }

    #[tool(
        description = "List open knowledge conflicts detected between enforceable items for explicit human review."
    )]
    fn list_conflicts(
        &self,
        Parameters(p): Parameters<ConflictListParams>,
    ) -> Result<CallToolResult, McpError> {
        json_ok(
            &self
                .store
                .list_conflicts(p.domain.as_deref())
                .map_err(err)?,
        )
    }

    #[tool(description = "Resolve a knowledge conflict explicitly: keep, accept, or merge.")]
    fn resolve_conflict(
        &self,
        Parameters(p): Parameters<ConflictResolveParams>,
    ) -> Result<CallToolResult, McpError> {
        if !matches!(p.action.as_str(), "keep" | "accept" | "merge") {
            return Err(err("action must be keep, accept, or merge"));
        }
        let ok = self.store.resolve_conflict(p.id, &p.action).map_err(err)?;
        self.store
            .log_episode_auto(
                &self.session_run_id,
                Some("conflict_resolution"),
                Some(&format!("resolve_conflict id={} action={}", p.id, p.action)),
                Some(if ok { "resolved" } else { "conflict not found" }),
                Some(if ok { "success" } else { "error" }),
                None,
                None,
                None,
                None,
            )
            .ok();
        text_ok(if ok {
            format!("Conflict #{} resolved with action '{}'.", p.id, p.action)
        } else {
            format!("No open conflict found for #{}.", p.id)
        })
    }

    #[tool(
        description = "List ingested sources (files/URLs) for a domain or all domains. Shows id, URI, title, fetch time, and trust."
    )]
    fn list_sources(
        &self,
        Parameters(p): Parameters<ListSourcesParams>,
    ) -> Result<CallToolResult, McpError> {
        let rows = self.store.list_sources(p.domain.as_deref()).map_err(err)?;
        json_ok(&rows)
    }

    #[tool(
        description = "List agentic run episodes from the audit trail. Filter by run_id to inspect a specific run, or omit to see all recent episodes."
    )]
    fn list_episodes(
        &self,
        Parameters(p): Parameters<ListEpisodesParams>,
    ) -> Result<CallToolResult, McpError> {
        let rows = self.store.list_episodes(p.run_id.as_deref()).map_err(err)?;
        json_ok(&rows)
    }

    #[tool(
        description = "Fully delete a domain and all its data (chunks, sources, knowledge, registry entry). Use chunks_only=true to re-ingest updated documents while keeping promoted rules. Use chunks_only=false (default) to wipe everything including the domain itself."
    )]
    fn clear_domain(
        &self,
        Parameters(p): Parameters<ClearDomainParams>,
    ) -> Result<CallToolResult, McpError> {
        let chunks_only = p.chunks_only.unwrap_or(false);
        let (chunks, knowledge) = self
            .store
            .clear_domain(&p.domain, chunks_only)
            .map_err(err)?;
        let knowledge_msg = if chunks_only {
            "knowledge kept".to_string()
        } else {
            format!("{knowledge} knowledge items deleted")
        };
        let observation = format!(
            "cleared domain '{}': {} chunks deleted, {}",
            p.domain, chunks, knowledge_msg
        );
        self.store
            .log_episode_auto(
                &self.session_run_id,
                Some("clear_domain"),
                Some(&format!(
                    "clear_domain name={} chunks_only={}",
                    p.domain, chunks_only
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
            "Cleared domain '{}': {chunks} chunks deleted, {knowledge_msg}.",
            p.domain
        ))
    }

    // ----- codebase memory tools -------------------------------------------

    #[tool(
        description = "Remove a repository from the code memory index. The path must match exactly as shown by list_repos()."
    )]
    async fn forget_repo(
        &self,
        Parameters(p): Parameters<ForgetRepoParams>,
    ) -> Result<CallToolResult, McpError> {
        let ok = self.code_store.forget_repo(&p.path).await.map_err(err)?;
        let observation = if ok {
            format!("removed repo '{}' from codebase memory", p.path)
        } else {
            format!("no repo found at '{}'", p.path)
        };
        self.store
            .log_episode_auto(
                &self.session_run_id,
                Some("indexing"),
                Some(&format!("forget_repo path={}", p.path)),
                Some(&observation),
                Some("success"),
                None,
                None,
                None,
                None,
            )
            .ok();
        if ok {
            text_ok(format!("Removed '{}' from code memory.", p.path))
        } else {
            text_ok(format!(
                "No indexed repo found at '{}'. Check list_repos() for exact paths.",
                p.path
            ))
        }
    }

    #[tool(
        description = "Clear ALL indexed codebase memory — removes every repository, file, and chunk from the code store. Use forget_repo() to remove a single repository instead."
    )]
    #[allow(dead_code)]
    async fn clear_codebase(&self) -> Result<CallToolResult, McpError> {
        self.code_store.clear_all().await.map_err(err)?;
        self.store
            .log_episode_auto(
                &self.session_run_id,
                Some("indexing"),
                Some("clear_codebase"),
                Some("codebase memory cleared"),
                Some("success"),
                None,
                None,
                None,
                None,
            )
            .ok();
        text_ok("Codebase memory cleared. All indexed repositories, files, and chunks have been removed.")
    }

    #[tool(
        description = "Clear ALL domains and ALL cascading data — knowledge, sources, chunks, and domain registrations. Codebase memory (indexed repos) and training data (training examples) are NOT affected — they live in separate databases; use clear_codebase() for code memory. This is a full wipe of the knowledge store. Use clear_domain() to remove a single domain instead."
    )]
    #[allow(dead_code)]
    fn clear_domains(&self) -> Result<CallToolResult, McpError> {
        self.store.clear_all_domains().map_err(err)?;
        self.store
            .log_episode_auto(
                &self.session_run_id,
                Some("clear_domains"),
                Some("clear_domains"),
                Some("all domains cleared"),
                Some("success"),
                None,
                None,
                None,
                None,
            )
            .ok();
        text_ok("All domains cleared. Knowledge, sources, chunks, and domain registrations have been removed. Codebase memory and training data are unaffected.")
    }

    #[tool(
        description = "Clear ALL knowledge items (facts, rules, procedures) across every domain. Domain registrations and ingested sources are kept. This cannot be undone — use forget() to remove a single item instead."
    )]
    #[allow(dead_code)]
    fn clear_knowledge(&self) -> Result<CallToolResult, McpError> {
        self.store.clear_all_knowledge().map_err(err)?;
        self.store
            .log_episode_auto(
                &self.session_run_id,
                Some("clear_knowledge"),
                Some("clear_knowledge"),
                Some("all knowledge items cleared"),
                Some("success"),
                None,
                None,
                None,
                None,
            )
            .ok();
        text_ok("All knowledge items cleared. Domain registrations and sources are unaffected.")
    }

    #[tool(
        description = "Clear ALL ingested sources and their reference chunks across every domain. Knowledge items (facts, rules) and domain registrations are kept. This cannot be undone — use clear_domain(chunks_only=true) to clear a single domain's sources instead."
    )]
    #[allow(dead_code)]
    fn clear_sources(&self) -> Result<CallToolResult, McpError> {
        self.store.clear_all_sources().map_err(err)?;
        self.store
            .log_episode_auto(
                &self.session_run_id,
                Some("clear_sources"),
                Some("clear_sources"),
                Some("all sources and chunks cleared"),
                Some("success"),
                None,
                None,
                None,
                None,
            )
            .ok();
        text_ok("All sources and reference chunks cleared. Knowledge items and domain registrations are unaffected.")
    }

    #[tool(
        description = "Clear ALL agentic run episodes from the audit trail. Knowledge, sources, and domain registrations are unaffected. This cannot be undone."
    )]
    #[allow(dead_code)]
    fn clear_episodes(&self) -> Result<CallToolResult, McpError> {
        self.store.clear_all_episodes().map_err(err)?;
        self.store
            .log_episode_auto(
                &self.session_run_id,
                Some("clear_episodes"),
                Some("clear_episodes"),
                Some("all episodes cleared"),
                Some("success"),
                None,
                None,
                None,
                None,
            )
            .ok();
        text_ok("All episodes cleared from the audit trail. Knowledge and sources are unaffected.")
    }

    // ----- training-data layer tools ---------------------------------------

}

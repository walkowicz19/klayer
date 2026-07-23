//! Dashboard HTTP server: axum router construction (`start_dashboard`), all
//! `dash_*` API handlers, marketplace-template helpers, and bearer-token
//! auth for `--mode=server`.

use std::sync::Arc;

use anyhow::Result;
use axum::{
    extract::{Query, Request, State},
    http::header,
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router,
};
use kl_code::CodeStore;
use kl_core::{
    DomainRow, EpisodeRow, JournalRow, Kind, KnowledgeRow, MarketplaceItem, MarketplaceTemplate,
    SourceRow, SubmissionRow,
};
use kl_session::SessionStore;
use kl_store::Store;
use kl_train::TrainStore;
use serde::Deserialize;
use tower_http::cors::CorsLayer;

use crate::bootstrap::get_klayer_dir;
use crate::compliance;
use crate::tools::ExplainabilityParams;

// ----- dashboard HTTP server -----------------------------------------------

const DASHBOARD_HTML_EMBEDDED: &str = include_str!("dashboard.html");

/// Load dashboard HTML: env override → file next to binary → embedded fallback.
/// Leaks into `'static` so the axum handler can return it without cloning.
pub(crate) fn load_dashboard_html() -> &'static str {
    if let Ok(path) = std::env::var("KLAYER_DASHBOARD_HTML") {
        if let Ok(s) = std::fs::read_to_string(&path) {
            tracing::info!("dashboard HTML loaded from {path}");
            return Box::leak(s.into_boxed_str());
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("dashboard.html");
            if let Ok(s) = std::fs::read_to_string(&p) {
                tracing::info!("dashboard HTML loaded from {}", p.display());
                return Box::leak(s.into_boxed_str());
            }
        }
    }
    DASHBOARD_HTML_EMBEDDED
}

// Query param structs for API endpoints.
#[derive(Deserialize)]
struct ApiKnowledgeQuery {
    domain: Option<String>,
    trust: Option<String>,
    kind: Option<String>,
}

#[derive(Deserialize)]
struct ApiDomainFilter {
    domain: Option<String>,
}

#[derive(Deserialize)]
struct ApiDomainDelete {
    name: String,
}

#[derive(Deserialize)]
struct ApiIdDelete {
    id: i64,
}

#[derive(Deserialize)]
struct ApiCodeRepoDelete {
    path: String,
}

#[derive(Deserialize)]
struct ApiRunFilter {
    run_id: Option<String>,
}

#[derive(Deserialize)]
struct CodeSearchQuery {
    q: Option<String>,
    repo: Option<String>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct ApiMarketplaceApply {
    template: String,
}

#[derive(Clone)]
struct DashState {
    store: Arc<Store>,
    code_store: Arc<CodeStore>,
    train_store: Arc<TrainStore>,
    session_store: Arc<SessionStore>,
    /// Shared with the `Klayer` MCP server instance running in the same
    /// process (see `main()`), so the dashboard can show which harness, if
    /// any, is the live MCP connection right now. In `--dashboard`-only mode
    /// there is no MCP server, so this stays `None` forever — expected, not
    /// a bug.
    captured_harness: Arc<std::sync::Mutex<Option<String>>>,
    html: &'static str,
}

impl axum::extract::FromRef<DashState> for Arc<Store> {
    fn from_ref(s: &DashState) -> Self {
        s.store.clone()
    }
}

impl axum::extract::FromRef<DashState> for Arc<CodeStore> {
    fn from_ref(s: &DashState) -> Self {
        s.code_store.clone()
    }
}

impl axum::extract::FromRef<DashState> for Arc<TrainStore> {
    fn from_ref(s: &DashState) -> Self {
        s.train_store.clone()
    }
}

impl axum::extract::FromRef<DashState> for Arc<SessionStore> {
    fn from_ref(s: &DashState) -> Self {
        s.session_store.clone()
    }
}

impl axum::extract::FromRef<DashState> for Arc<std::sync::Mutex<Option<String>>> {
    fn from_ref(s: &DashState) -> Self {
        s.captured_harness.clone()
    }
}

async fn dash_index(State(s): State<DashState>) -> Response {
    ([(header::CACHE_CONTROL, "no-store")], Html(s.html)).into_response()
}

async fn dash_stats(State(store): State<Arc<Store>>) -> Json<serde_json::Value> {
    let domains = store.list_domains().unwrap_or_default();
    let sources = store.list_sources(None).unwrap_or_default();
    let episodes = store.list_episodes(None).unwrap_or_default();
    let prefs = store.list_preferences().unwrap_or_default();
    let total_docs: i64 = domains.iter().map(|d| d.doc_count).sum();
    let total_rules: i64 = domains.iter().map(|d| d.rule_count).sum();
    let mut proposed = 0usize;
    for d in &domains {
        if let Ok(rows) = store.list_knowledge(&d.name, Some("proposed"), None) {
            proposed += rows.len();
        }
    }
    Json(serde_json::json!({
        "domains":     domains.len(),
        "documents":   total_docs,
        "rules":       total_rules,
        "proposed":    proposed,
        "sources":     sources.len(),
        "episodes":    episodes.len(),
        "preferences": prefs.len(),
    }))
}

async fn dash_domains(State(store): State<Arc<Store>>) -> Json<Vec<DomainRow>> {
    Json(store.list_domains().unwrap_or_default())
}

async fn dash_knowledge(
    State(store): State<Arc<Store>>,
    Query(q): Query<ApiKnowledgeQuery>,
) -> Json<Vec<KnowledgeRow>> {
    let kind = q.kind.as_deref().and_then(Kind::parse);
    let rows = if let Some(domain) = &q.domain {
        store
            .list_knowledge(domain, q.trust.as_deref(), kind)
            .unwrap_or_default()
    } else {
        let domains = store.list_domains().unwrap_or_default();
        let mut all = Vec::new();
        for d in &domains {
            if let Ok(rows) = store.list_knowledge(&d.name, q.trust.as_deref(), kind) {
                all.extend(rows);
            }
        }
        all
    };
    Json(rows)
}

async fn dash_sources(
    State(store): State<Arc<Store>>,
    Query(q): Query<ApiDomainFilter>,
) -> Json<Vec<SourceRow>> {
    Json(store.list_sources(q.domain.as_deref()).unwrap_or_default())
}

async fn dash_episodes(
    State(store): State<Arc<Store>>,
    Query(q): Query<ApiRunFilter>,
) -> Json<Vec<EpisodeRow>> {
    Json(store.list_episodes(q.run_id.as_deref()).unwrap_or_default())
}

async fn dash_preferences(State(store): State<Arc<Store>>) -> Json<Vec<String>> {
    Json(store.list_preferences().unwrap_or_default())
}

async fn dash_marketplace_apply(
    State(store): State<Arc<Store>>,
    Query(q): Query<ApiMarketplaceApply>,
) -> Json<serde_json::Value> {
    let Some(template) = marketplace_template(&q.template) else {
        return Json(serde_json::json!({
            "ok": false,
            "error": format!("unknown marketplace template '{}'", q.template)
        }));
    };

    match apply_marketplace_template(&store, &template) {
        Ok(outcome) => Json(serde_json::json!({
            "ok": true,
            "domain": template.slug,
            "inserted": outcome.inserted,
            "skipped": outcome.skipped,
            "source_created": outcome.source_created
        })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn dash_marketplace_templates() -> Json<serde_json::Value> {
    let list = load_marketplace_templates()
        .iter()
        .map(|t| {
            serde_json::json!({
                "slug": t.slug,
                "description": t.description,
                "query_hint": t.query_hint,
                "author": t.author,
                "item_count": t.items.len(),
            })
        })
        .collect::<Vec<_>>();
    Json(serde_json::json!(list))
}

struct MarketplaceApplyOutcome {
    inserted: u64,
    skipped: u64,
    source_created: bool,
}

fn apply_marketplace_template(
    store: &Store,
    template: &MarketplaceTemplate,
) -> Result<MarketplaceApplyOutcome> {
    store.register_domain(
        &template.slug,
        Some(&template.description),
        Some(&template.query_hint),
        None,
        None,
        None,
        Some(true),
        None,
    )?;

    let marketplace_uri = format!("marketplace://{}", template.slug);
    let existing_sources = store.list_sources(Some(&template.slug))?;
    let mut source_created = false;
    let source_id = if let Some(source) = existing_sources
        .iter()
        .find(|s| s.uri.as_deref() == Some(marketplace_uri.as_str()))
    {
        source.id
    } else {
        source_created = true;
        let id = store.add_source(
            "marketplace-template",
            Some(&marketplace_uri),
            Some(&format!("{} Marketplace Template", template.slug)),
            &template.slug,
        )?;
        let chunks = template
            .items
            .iter()
            .map(|item| format!("{}: {}", item.title, item.body))
            .collect::<Vec<_>>();
        store.add_chunks(id, &template.slug, &chunks)?;
        id
    };

    let existing_titles = store
        .list_knowledge(&template.slug, None, None)?
        .into_iter()
        .map(|k| k.title)
        .collect::<std::collections::HashSet<_>>();

    let mut inserted = 0;
    let mut skipped = 0;
    for item in &template.items {
        if existing_titles.contains(&item.title) {
            skipped += 1;
            continue;
        }
        let id = store.propose(
            item.kind,
            &template.slug,
            item.stage.as_deref(),
            &item.title,
            &item.body,
            item.trigger.as_deref(),
            item.severity.as_deref(),
            item.remediation.as_deref(),
            Some(source_id),
        )?;
        store.promote(id)?;
        inserted += 1;
    }

    Ok(MarketplaceApplyOutcome {
        inserted,
        skipped,
        source_created,
    })
}

fn marketplace_template(slug: &str) -> Option<MarketplaceTemplate> {
    load_marketplace_templates()
        .into_iter()
        .find(|t| t.slug == slug)
}

const MARKETPLACE_EMBEDDED: &str = include_str!("marketplace.json");

/// The single canonical marketplace file path — the same path is read (to list
/// templates) and written (when a submission is approved) so approvals go live.
/// KLAYER_MARKETPLACE overrides; a marketplace.json in CWD wins for dev checkouts;
/// otherwise it lives next to the databases under the klayer home dir.
fn marketplace_file_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("KLAYER_MARKETPLACE") {
        return std::path::PathBuf::from(p);
    }
    let cwd = std::path::PathBuf::from("marketplace.json");
    if cwd.exists() {
        return cwd;
    }
    get_klayer_dir().join("marketplace.json")
}

/// Read the marketplace fresh each call (no permanent cache) so an approval is
/// reflected immediately. Falls back to the embedded default if the file is
/// missing or unparseable.
fn load_marketplace_templates() -> Vec<MarketplaceTemplate> {
    if let Ok(content) = std::fs::read_to_string(marketplace_file_path()) {
        if let Ok(templates) = serde_json::from_str::<Vec<MarketplaceTemplate>>(&content) {
            return templates;
        }
    }
    serde_json::from_str::<Vec<MarketplaceTemplate>>(MARKETPLACE_EMBEDDED)
        .expect("embedded marketplace.json should be valid")
}

/// Append (or replace, deduped by slug) a template into the marketplace file.
fn append_marketplace_template(template: &MarketplaceTemplate) -> Result<()> {
    let mut templates = load_marketplace_templates();
    if let Some(existing) = templates.iter_mut().find(|t| t.slug == template.slug) {
        *existing = template.clone();
    } else {
        templates.push(template.clone());
    }
    let path = marketplace_file_path();
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(&path, serde_json::to_string_pretty(&templates)?)?;
    Ok(())
}

/// Whether this build is the admin build. The marketplace submission
/// review/approval workflow (approve/deny/import) is compiled in ONLY when the
/// `admin` cargo feature is enabled. Distributed user binaries are built without
/// it, so they can publish and manage their own domains but never approve.
fn is_admin() -> bool {
    cfg!(feature = "admin")
}

async fn dash_admin() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "admin": is_admin() }))
}

/// Liveness of each of the three databases. A trivial query is run against each
/// store; `true` means it responded. The dashboard's status pill uses this to
/// show that all databases are live, or exactly which one is failing.
async fn dash_health(State(s): State<DashState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "knowledge": s.store.list_domains().is_ok(),
        "code":      s.code_store.stats().await.is_ok(),
        "train":     s.train_store.stats().await.is_ok(),
        "session":   s.session_store.list_journal(None).await.is_ok(),
    }))
}

/// Build one `/api/storage-health` entry. Factored out (pure, no store access)
/// so it's directly unit-testable — `SyncHealthSnapshot`'s fields are all
/// public, so tests can construct one without spinning up a real libsql store.
fn storage_health_entry(
    engine: &str,
    healthy: bool,
    sync: Option<&kl_core::SyncHealthSnapshot>,
) -> serde_json::Value {
    match sync {
        Some(s) => serde_json::json!({ "engine": engine, "healthy": healthy, "sync": s }),
        None => serde_json::json!({ "engine": engine, "healthy": healthy }),
    }
}

/// Per-database storage health: `kl-store` (rusqlite, no replica — reachability
/// is the whole story) plus the three libsql-backed stores, each surfacing its
/// Stage A `SyncHealth` snapshot (last successful sync, consecutive failures,
/// cumulative fallback-to-local-only events).
async fn dash_storage_health(State(s): State<DashState>) -> Json<serde_json::Value> {
    let code_health = s.code_store.health();
    let train_health = s.train_store.health();
    let session_health = s.session_store.health();
    Json(serde_json::json!({
        "kl_store": storage_health_entry("sqlite", s.store.list_domains().is_ok(), None),
        "kl_code": storage_health_entry(
            "libsql",
            s.code_store.stats().await.is_ok(),
            Some(&code_health),
        ),
        "kl_train": storage_health_entry(
            "libsql",
            s.train_store.stats().await.is_ok(),
            Some(&train_health),
        ),
        "kl_session": storage_health_entry(
            "libsql",
            s.session_store.list_journal(None).await.is_ok(),
            Some(&session_health),
        ),
    }))
}

async fn dash_conflicts(
    State(store): State<Arc<Store>>,
    Query(q): Query<ApiDomainFilter>,
) -> Json<serde_json::Value> {
    match store.list_conflicts(q.domain.as_deref()) {
        Ok(rows) => Json(serde_json::json!(rows)),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

async fn dash_explainability(
    State(store): State<Arc<Store>>,
    Query(q): Query<ExplainabilityParams>,
) -> Response {
    if q.format.as_deref() == Some("pdf") {
        let report = match compliance::build_compliance_report(&store, q.run_id.as_deref()) {
            Ok(r) => r,
            Err(e) => {
                return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
                    .into_response()
            }
        };
        return match compliance::render_compliance_pdf(&report) {
            Ok(bytes) => (
                [
                    (header::CONTENT_TYPE, "application/pdf".to_string()),
                    (
                        header::CONTENT_DISPOSITION,
                        format!(
                            "attachment; filename=\"compliance-report{}.pdf\"",
                            q.run_id
                                .as_deref()
                                .map(|r| format!("-{r}"))
                                .unwrap_or_default()
                        ),
                    ),
                ],
                bytes,
            )
                .into_response(),
            Err(e) => {
                (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
            }
        };
    }
    match store.list_episodes(q.run_id.as_deref()) {
        Ok(episodes) => Json(
            serde_json::json!({"run_id": q.run_id, "episodes": episodes, "format": "reverse-explainability-v1"}),
        )
        .into_response(),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})).into_response(),
    }
}

/// Per-day cost/token rollup entry for `/api/usage`'s `daily_usage` trend.
/// `episodes_with_cost`/`episodes_with_tokens` count only episodes that
/// self-reported that field (most won't — see `log_episode_auto` doc comment
/// on why this is best-effort, not measured).
#[derive(Debug, Default, serde::Serialize)]
struct DailyUsage {
    tokens_used: i64,
    cost: f64,
    episodes_with_tokens: i64,
    episodes_with_cost: i64,
}

/// Build `/api/usage`'s JSON body from a set of episodes. Factored out (pure)
/// so the by_action/by_outcome/daily cost-token rollup is unit-testable
/// without a `Store`.
fn usage_rollup(rows: &[EpisodeRow]) -> serde_json::Value {
    let mut by_action = std::collections::BTreeMap::<String, i64>::new();
    let mut by_outcome = std::collections::BTreeMap::<String, i64>::new();
    let mut daily = std::collections::BTreeMap::<String, DailyUsage>::new();
    let mut total_tokens: i64 = 0;
    let mut total_cost: f64 = 0.0;
    for row in rows {
        *by_action
            .entry(row.action.clone().unwrap_or_else(|| "unknown".into()))
            .or_default() += 1;
        *by_outcome
            .entry(row.outcome.clone().unwrap_or_else(|| "unknown".into()))
            .or_default() += 1;

        let day = chrono::DateTime::from_timestamp(row.ts, 0)
            .map(|dt| dt.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "unknown".into());
        let entry = daily.entry(day).or_default();

        let action_len = row.action.as_ref().map_or(0, |a| a.len());
        let obs_len = row.observation.as_ref().map_or(0, |o| o.len());
        let payload_len = action_len + obs_len;
        let estimated_t = if payload_len == 0 {
            1
        } else {
            ((payload_len as i64 + 3) / 4).max(1)
        };

        let t = row.tokens_used.unwrap_or(estimated_t);
        entry.tokens_used += t;
        entry.episodes_with_tokens += 1;
        total_tokens += t;

        let c = row.cost.unwrap_or_else(|| (t as f64) * 0.000002);
        entry.cost += c;
        entry.episodes_with_cost += 1;
        total_cost += c;
    }
    serde_json::json!({
        "sample_size": rows.len(),
        "by_action": by_action,
        "by_outcome": by_outcome,
        "total_tokens_used": total_tokens,
        "total_cost": total_cost,
        "daily_usage": daily,
        "note": "Rollup covers the recent episode window returned by the audit API. tokens_used/cost are best-effort, self-reported by the calling harness (MCP has no standard field for this) — most episodes will not carry them."
    })
}

/// Build `/api/model-registry`'s JSON body: entries grouped by harness, then
/// by capability tier within it, matching how the Model Registry dashboard
/// page (Stage H) presents the data — one long undifferentiated table isn't
/// useful once a user has more than one harness configured.
///
/// `connected_now` and the two `*_ts` fields are best-effort derived data
/// (see `Store::last_episode_ts_for`), not a clean presence system — klayer
/// has exactly one real liveness signal (`captured_harness`, the current
/// MCP process's handshake), and no historical "last seen" beyond what the
/// `episodes` log incidentally records.
fn model_registry_grouped(
    rows: &[kl_core::ModelRegistryRow],
    store: &Store,
    captured_harness: &Option<String>,
) -> serde_json::Value {
    let mut by_harness =
        std::collections::BTreeMap::<String, std::collections::BTreeMap<String, Vec<_>>>::new();
    for r in rows {
        by_harness
            .entry(r.harness.clone())
            .or_default()
            .entry(r.capability_tier.clone())
            .or_default()
            .push(serde_json::json!({
                "model_id": r.model_id,
                "cost_weight": r.cost_weight,
                "sub_agent_name": r.sub_agent_name,
            }));
    }
    let harnesses: Vec<serde_json::Value> = by_harness
        .into_iter()
        .map(|(harness, tiers)| {
            let needle = format!("harness={harness}");
            serde_json::json!({
                "harness": harness,
                "tiers": tiers,
                "connected_now": captured_harness.as_deref().map(kl_core::normalize_harness)
                    == Some(kl_core::normalize_harness(&harness)),
                "last_edit_ts": store.last_episode_ts_for("model_registry", &needle),
                "last_recommendation_ts": store.last_episode_ts_for("model_recommendation", &needle),
            })
        })
        .collect();
    serde_json::json!({ "harnesses": harnesses })
}

async fn dash_model_registry(
    State(store): State<Arc<Store>>,
    State(captured_harness): State<Arc<std::sync::Mutex<Option<String>>>>,
) -> Json<serde_json::Value> {
    let captured = captured_harness.lock().ok().and_then(|g| g.clone());
    match store.list_model_registry() {
        Ok(rows) => Json(model_registry_grouped(&rows, &store, &captured)),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

/// Build `/api/routing-rules`'s JSON body: the `domain_type` x `task_type` x
/// `complexity_tier` matrix, grouped by harness (mirroring the Model
/// Registry grouping above) since the matrix is only meaningful within one
/// harness's own registry at a time.
fn routing_rules_grouped(rows: &[kl_core::RoutingRuleRow]) -> serde_json::Value {
    let mut by_harness = std::collections::BTreeMap::<String, Vec<_>>::new();
    for r in rows {
        by_harness
            .entry(r.harness.clone())
            .or_default()
            .push(serde_json::json!({
                "domain_type": r.domain_type,
                "task_type": r.task_type,
                "complexity_tier": r.complexity_tier,
                "model_id": r.model_id,
            }));
    }
    let harnesses: Vec<serde_json::Value> = by_harness
        .into_iter()
        .map(|(harness, rules)| serde_json::json!({"harness": harness, "rules": rules}))
        .collect();
    serde_json::json!({ "harnesses": harnesses })
}

async fn dash_routing_rules(State(store): State<Arc<Store>>) -> Json<serde_json::Value> {
    match store.list_routing_rules() {
        Ok(rows) => Json(routing_rules_grouped(&rows)),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

async fn dash_usage(State(store): State<Arc<Store>>) -> Json<serde_json::Value> {
    match store.list_episodes(None) {
        Ok(rows) => Json(usage_rollup(&rows)),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

// ----- code store dashboard handlers ----------------------------------------

async fn dash_code_stats(State(cs): State<Arc<CodeStore>>) -> Json<serde_json::Value> {
    let s = cs.stats().await.unwrap_or(kl_code::CodeStats {
        repos: 0,
        files: 0,
        chunks: 0,
    });
    Json(serde_json::json!({ "repos": s.repos, "files": s.files, "chunks": s.chunks }))
}

async fn dash_code_repos(State(cs): State<Arc<CodeStore>>) -> Json<Vec<kl_code::RepoInfo>> {
    Json(cs.list_repos().await.unwrap_or_default())
}

async fn dash_code_clear(State(cs): State<Arc<CodeStore>>) -> Json<serde_json::Value> {
    match cs.clear_all().await {
        Ok(_) => Json(serde_json::json!({ "ok": true })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn dash_code_repo_delete(
    State(cs): State<Arc<CodeStore>>,
    Query(q): Query<ApiCodeRepoDelete>,
) -> Json<serde_json::Value> {
    match cs.forget_repo(&q.path).await {
        Ok(ok) => Json(serde_json::json!({ "ok": true, "deleted": ok })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn dash_domains_clear(State(store): State<Arc<Store>>) -> Json<serde_json::Value> {
    match store.clear_all_domains() {
        Ok(n) => Json(serde_json::json!({ "ok": true, "deleted": n })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn dash_domain_delete(
    State(store): State<Arc<Store>>,
    Query(q): Query<ApiDomainDelete>,
) -> Json<serde_json::Value> {
    match store.clear_domain(&q.name, false) {
        Ok((chunks, knowledge)) => Json(serde_json::json!({
            "ok": true,
            "chunks": chunks,
            "knowledge": knowledge
        })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn dash_knowledge_clear(State(store): State<Arc<Store>>) -> Json<serde_json::Value> {
    match store.clear_all_knowledge() {
        Ok(n) => Json(serde_json::json!({ "ok": true, "deleted": n })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn dash_knowledge_delete(
    State(store): State<Arc<Store>>,
    Query(q): Query<ApiIdDelete>,
) -> Json<serde_json::Value> {
    match store.forget(q.id) {
        Ok(ok) => Json(serde_json::json!({ "ok": ok })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn dash_sources_clear(State(store): State<Arc<Store>>) -> Json<serde_json::Value> {
    match store.clear_all_sources() {
        Ok(n) => Json(serde_json::json!({ "ok": true, "deleted": n })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn dash_source_delete(
    State(store): State<Arc<Store>>,
    Query(q): Query<ApiIdDelete>,
) -> Json<serde_json::Value> {
    match store.delete_source(q.id) {
        Ok(ok) => Json(serde_json::json!({ "ok": ok })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn dash_episodes_clear(State(store): State<Arc<Store>>) -> Json<serde_json::Value> {
    match store.clear_all_episodes() {
        Ok(n) => Json(serde_json::json!({ "ok": true, "deleted": n })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn dash_code_search(
    State(cs): State<Arc<CodeStore>>,
    Query(q): Query<CodeSearchQuery>,
) -> Json<Vec<kl_code::CodeHit>> {
    let query = q.q.unwrap_or_default();
    let limit = q.limit.unwrap_or(10);
    Json(
        cs.search(&query, q.repo.as_deref(), limit)
            .await
            .unwrap_or_default(),
    )
}

// ----- training store dashboard handlers ------------------------------------

#[derive(Deserialize)]
struct ApiTrainingQuery {
    domain: Option<String>,
    trust: Option<String>,
}

async fn dash_training(
    State(ts): State<Arc<TrainStore>>,
    Query(q): Query<ApiTrainingQuery>,
) -> Json<Vec<kl_train::TrainingRow>> {
    Json(
        ts.list_training(q.domain.as_deref(), q.trust.as_deref())
            .await
            .unwrap_or_default(),
    )
}

async fn dash_training_stats(State(ts): State<Arc<TrainStore>>) -> Json<serde_json::Value> {
    let s = ts.stats().await.unwrap_or(kl_train::TrainStats {
        total: 0,
        proposed: 0,
        reviewed: 0,
        user: 0,
        stubs: 0,
    });
    Json(serde_json::json!({
        "total":    s.total,
        "proposed": s.proposed,
        "reviewed": s.reviewed,
        "user":     s.user,
        "stubs":    s.stubs,
    }))
}

async fn dash_training_clear(State(ts): State<Arc<TrainStore>>) -> Json<serde_json::Value> {
    match ts.clear_all().await {
        Ok(n) => Json(serde_json::json!({ "ok": true, "deleted": n })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    }
}

// ----- session journal dashboard handlers -----------------------------------

#[derive(Deserialize)]
struct ApiRepoFilter {
    repo: Option<String>,
}

async fn dash_journal(
    State(store): State<Arc<SessionStore>>,
    Query(q): Query<ApiRepoFilter>,
) -> Json<Vec<JournalRow>> {
    Json(
        store
            .list_journal(q.repo.as_deref())
            .await
            .unwrap_or_default(),
    )
}

async fn dash_journal_clear(
    State(store): State<Arc<SessionStore>>,
    Query(q): Query<ApiRepoFilter>,
) -> Json<serde_json::Value> {
    match store.clear_journal(q.repo.as_deref()).await {
        Ok(n) => Json(serde_json::json!({ "ok": true, "deleted": n })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    }
}

// ----- marketplace submissions dashboard handlers ---------------------------

#[derive(Deserialize)]
struct ApiStatusFilter {
    status: Option<String>,
}

#[derive(Deserialize)]
struct ApiPublish {
    domain: String,
}

#[derive(Deserialize)]
struct ApiReview {
    id: i64,
    action: String,
    note: Option<String>,
}

#[derive(Deserialize)]
struct ApiImport {
    json: String,
}

async fn dash_submissions(
    State(store): State<Arc<Store>>,
    Query(q): Query<ApiStatusFilter>,
) -> Json<Vec<SubmissionRow>> {
    Json(
        store
            .list_submissions(q.status.as_deref())
            .unwrap_or_default(),
    )
}

async fn dash_submission_get(
    State(store): State<Arc<Store>>,
    Query(q): Query<ApiIdDelete>,
) -> Json<serde_json::Value> {
    match store.get_submission(q.id) {
        Ok(Some((row, items_json))) => {
            let items: serde_json::Value =
                serde_json::from_str(&items_json).unwrap_or(serde_json::json!([]));
            Json(serde_json::json!({
                "ok": true,
                "id": row.id,
                "slug": row.slug,
                "description": row.description,
                "query_hint": row.query_hint,
                "author": row.author,
                "status": row.status,
                "note": row.note,
                "submitted_at": row.submitted_at,
                "reviewed_at": row.reviewed_at,
                "items": items,
            }))
        }
        Ok(None) => Json(serde_json::json!({ "ok": false, "error": "not found" })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn dash_submission_publish(
    State(store): State<Arc<Store>>,
    Json(p): Json<ApiPublish>,
) -> Json<serde_json::Value> {
    let items = match store.export_domain_items(&p.domain) {
        Ok(items) => items,
        Err(e) => return Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    };
    if items.is_empty() {
        return Json(serde_json::json!({
            "ok": false,
            "error": "domain has no enforceable (reviewed/user) knowledge to publish"
        }));
    }
    // You cannot re-publish a domain you applied from the marketplace — that would
    // re-submit someone else's authored template under your name. Such domains carry
    // a 'marketplace-template' source (uri marketplace://<slug>).
    if store
        .list_sources(Some(&p.domain))
        .unwrap_or_default()
        .iter()
        .any(|s| {
            s.kind == "marketplace-template"
                || s.uri
                    .as_deref()
                    .map(|u| u.starts_with("marketplace://"))
                    .unwrap_or(false)
        })
    {
        return Json(serde_json::json!({
            "ok": false,
            "from_marketplace": true,
            "error": "this domain was applied from the marketplace and cannot be re-published"
        }));
    }
    // Attribution: a publisher must have registered an author name once. The UI
    // prompts for it on first publish, then it is reused for every domain.
    let author = match store.get_author() {
        Ok(Some((name, _, _))) => name,
        Ok(None) => {
            return Json(serde_json::json!({
                "ok": false,
                "needs_author": true,
                "error": "register your author name before publishing"
            }))
        }
        Err(e) => return Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    };
    let domain = store
        .list_domains()
        .unwrap_or_default()
        .into_iter()
        .find(|d| d.name == p.domain);
    let description = domain.as_ref().and_then(|d| d.description.clone());
    let query_hint = domain.as_ref().and_then(|d| d.query_hint.clone());
    let items_json = match serde_json::to_string(&items) {
        Ok(s) => s,
        Err(e) => return Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    };
    match store.create_submission(
        &p.domain,
        description.as_deref(),
        query_hint.as_deref(),
        &items_json,
        Some(&author),
    ) {
        Ok(id) => Json(
            serde_json::json!({ "ok": true, "id": id, "items": items.len(), "author": author }),
        ),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn dash_submission_delete(
    State(store): State<Arc<Store>>,
    Query(q): Query<ApiIdDelete>,
) -> Json<serde_json::Value> {
    ok_or_err(store.delete_submission(q.id))
}

// ----- author identity dashboard handlers -----------------------------------

#[derive(Deserialize)]
struct ApiAuthorSet {
    name: String,
}

async fn dash_author_get(State(store): State<Arc<Store>>) -> Json<serde_json::Value> {
    match store.get_author() {
        Ok(Some((name, registered_at, updated_at))) => {
            let now = chrono::Utc::now().timestamp();
            let next_change_at = updated_at + kl_store::AUTHOR_COOLDOWN_SECS;
            Json(serde_json::json!({
                "ok": true,
                "registered": true,
                "name": name,
                "registered_at": registered_at,
                "updated_at": updated_at,
                "cooldown_days": 14,
                "can_change": now >= next_change_at,
                "next_change_at": next_change_at,
            }))
        }
        Ok(None) => Json(serde_json::json!({
            "ok": true, "registered": false, "can_change": true, "cooldown_days": 14
        })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn dash_author_set(
    State(store): State<Arc<Store>>,
    Json(p): Json<ApiAuthorSet>,
) -> Json<serde_json::Value> {
    let name = p.name.trim();
    if name.is_empty() {
        return Json(serde_json::json!({ "ok": false, "error": "name cannot be empty" }));
    }
    if name.chars().count() > 60 {
        return Json(
            serde_json::json!({ "ok": false, "error": "name too long (max 60 characters)" }),
        );
    }
    match store.set_author(name, kl_store::AUTHOR_COOLDOWN_SECS) {
        Ok(kl_store::AuthorSetOutcome::Registered) => {
            Json(serde_json::json!({ "ok": true, "status": "registered", "name": name }))
        }
        Ok(kl_store::AuthorSetOutcome::Updated) => {
            Json(serde_json::json!({ "ok": true, "status": "updated", "name": name }))
        }
        Ok(kl_store::AuthorSetOutcome::Blocked { next_allowed_at }) => Json(serde_json::json!({
            "ok": false,
            "blocked": true,
            "next_change_at": next_allowed_at,
            "error": "the author name can only be changed once every 14 days"
        })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn dash_submission_review(
    State(store): State<Arc<Store>>,
    Json(p): Json<ApiReview>,
) -> Json<serde_json::Value> {
    if !is_admin() {
        return Json(serde_json::json!({
            "ok": false,
            "error": "submission review is available only in the admin build"
        }));
    }
    match p.action.as_str() {
        "approve" => {
            let Ok(Some((row, items_json))) = store.get_submission(p.id) else {
                return Json(serde_json::json!({ "ok": false, "error": "submission not found" }));
            };
            let items: Vec<MarketplaceItem> = match serde_json::from_str(&items_json) {
                Ok(i) => i,
                Err(e) => return Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
            };
            let template = MarketplaceTemplate {
                slug: row.slug.clone(),
                description: row.description.clone().unwrap_or_default(),
                query_hint: row.query_hint.clone().unwrap_or_default(),
                author: row.author.clone(),
                items,
            };
            if let Err(e) = append_marketplace_template(&template) {
                return Json(serde_json::json!({ "ok": false, "error": e.to_string() }));
            }
            let _ = store.set_submission_status(p.id, "approved", p.note.as_deref());
            Json(serde_json::json!({ "ok": true, "status": "approved", "slug": row.slug }))
        }
        "deny" => match store.set_submission_status(p.id, "denied", p.note.as_deref()) {
            Ok(true) => Json(serde_json::json!({ "ok": true, "status": "denied" })),
            Ok(false) => Json(serde_json::json!({ "ok": false, "error": "submission not found" })),
            Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        },
        other => Json(serde_json::json!({
            "ok": false,
            "error": format!("unknown action '{other}' (expected 'approve' or 'deny')")
        })),
    }
}

async fn dash_submission_export(
    State(store): State<Arc<Store>>,
    Query(q): Query<ApiIdDelete>,
) -> Response {
    match store.get_submission(q.id) {
        Ok(Some((row, items_json))) => {
            let items: serde_json::Value =
                serde_json::from_str(&items_json).unwrap_or(serde_json::json!([]));
            let payload = serde_json::json!({
                "slug": row.slug,
                "description": row.description,
                "query_hint": row.query_hint,
                "author": row.author,
                "items": items,
            });
            let body = serde_json::to_string_pretty(&payload).unwrap_or_default();
            (
                [
                    (header::CONTENT_TYPE, "application/json"),
                    (
                        header::CONTENT_DISPOSITION,
                        &format!("attachment; filename=\"{}-submission.json\"", row.slug),
                    ),
                ],
                body,
            )
                .into_response()
        }
        _ => (axum::http::StatusCode::NOT_FOUND, "submission not found").into_response(),
    }
}

async fn dash_submission_import(
    State(store): State<Arc<Store>>,
    Json(p): Json<ApiImport>,
) -> Json<serde_json::Value> {
    if !is_admin() {
        return Json(serde_json::json!({
            "ok": false,
            "error": "importing submissions for review is available only in the admin build"
        }));
    }
    let template: MarketplaceTemplate = match serde_json::from_str(&p.json) {
        Ok(t) => t,
        Err(e) => {
            return Json(serde_json::json!({
                "ok": false,
                "error": format!("invalid submission JSON: {e}")
            }))
        }
    };
    let items_json = match serde_json::to_string(&template.items) {
        Ok(s) => s,
        Err(e) => return Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    };
    match store.create_submission(
        &template.slug,
        Some(&template.description),
        Some(&template.query_hint),
        &items_json,
        template.author.as_deref(),
    ) {
        Ok(id) => Json(serde_json::json!({ "ok": true, "id": id, "slug": template.slug })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    }
}

// ----- manual edit dashboard handlers ---------------------------------------

#[derive(Deserialize)]
struct ApiDomainUpdate {
    name: String,
    description: Option<String>,
    query_hint: Option<String>,
    enforced: Option<bool>,
}

#[derive(Deserialize)]
struct ApiKnowledgeUpdate {
    id: i64,
    title: String,
    body: String,
    stage: Option<String>,
    trigger: Option<String>,
    severity: Option<String>,
    remediation: Option<String>,
}

fn ok_or_err(result: Result<bool>) -> Json<serde_json::Value> {
    match result {
        Ok(ok) => Json(serde_json::json!({ "ok": ok })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn dash_domain_update(
    State(store): State<Arc<Store>>,
    Json(p): Json<ApiDomainUpdate>,
) -> Json<serde_json::Value> {
    ok_or_err(store.update_domain(
        &p.name,
        p.description.as_deref(),
        p.query_hint.as_deref(),
        p.enforced,
    ))
}

async fn dash_knowledge_update(
    State(store): State<Arc<Store>>,
    Json(p): Json<ApiKnowledgeUpdate>,
) -> Json<serde_json::Value> {
    ok_or_err(store.update_knowledge(
        p.id,
        &p.title,
        &p.body,
        p.stage.as_deref(),
        p.trigger.as_deref(),
        p.severity.as_deref(),
        p.remediation.as_deref(),
    ))
}

#[derive(Clone)]
struct AuthState {
    token: Arc<String>,
}

fn bearer_token_matches(expected: &str, header_value: Option<&str>) -> bool {
    match header_value.and_then(|v| v.strip_prefix("Bearer ")) {
        Some(presented) => presented == expected,
        None => false,
    }
}

async fn require_bearer_token(State(auth): State<AuthState>, req: Request, next: Next) -> Response {
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if bearer_token_matches(&auth.token, presented) {
        next.run(req).await
    } else {
        (axum::http::StatusCode::UNAUTHORIZED, "unauthorized").into_response()
    }
}

pub(crate) async fn start_dashboard(
    store: Arc<Store>,
    code_store: Arc<CodeStore>,
    train_store: Arc<TrainStore>,
    session_store: Arc<SessionStore>,
    captured_harness: Arc<std::sync::Mutex<Option<String>>>,
    port: u16,
    html: &'static str,
    // `Some` only in `--mode=server`; drives both the bind address (below) and
    // whether the auth layer is attached at all, so the default (localhost,
    // no auth) code path is untouched rather than a layer that always runs
    // and happens to no-op.
    server_auth_token: Option<Arc<String>>,
) {
    let state = DashState {
        store,
        code_store,
        train_store,
        session_store,
        captured_harness,
        html,
    };
    let app = Router::new()
        .route("/", get(dash_index))
        .route("/api/stats", get(dash_stats))
        .route("/api/health", get(dash_health))
        .route("/api/storage-health", get(dash_storage_health))
        .route("/api/domains", get(dash_domains))
        .route("/api/knowledge", get(dash_knowledge))
        .route("/api/conflicts", get(dash_conflicts))
        .route("/api/explainability", get(dash_explainability))
        .route("/api/usage", get(dash_usage))
        .route("/api/model-registry", get(dash_model_registry))
        .route("/api/routing-rules", get(dash_routing_rules))
        .route("/api/sources", get(dash_sources))
        .route("/api/episodes", get(dash_episodes))
        .route("/api/preferences", get(dash_preferences))
        .route("/api/marketplace/apply", get(dash_marketplace_apply))
        .route(
            "/api/marketplace/templates",
            get(dash_marketplace_templates),
        )
        .route("/api/journal", get(dash_journal))
        .route("/api/journal/clear", get(dash_journal_clear))
        .route("/api/admin", get(dash_admin))
        .route("/api/submissions", get(dash_submissions))
        .route("/api/submissions/get", get(dash_submission_get))
        .route(
            "/api/submissions/publish",
            axum::routing::post(dash_submission_publish),
        )
        .route(
            "/api/submissions/review",
            axum::routing::post(dash_submission_review),
        )
        .route("/api/submissions/export", get(dash_submission_export))
        .route(
            "/api/submissions/import",
            axum::routing::post(dash_submission_import),
        )
        .route("/api/submissions/delete", get(dash_submission_delete))
        .route("/api/author", get(dash_author_get).post(dash_author_set))
        .route(
            "/api/domain/update",
            axum::routing::post(dash_domain_update),
        )
        .route(
            "/api/knowledge/update",
            axum::routing::post(dash_knowledge_update),
        )
        .route("/api/code/stats", get(dash_code_stats))
        .route("/api/code/repos", get(dash_code_repos))
        .route("/api/code/search", get(dash_code_search))
        .route("/api/code/clear", get(dash_code_clear))
        .route("/api/code/repo/delete", get(dash_code_repo_delete))
        .route("/api/training", get(dash_training))
        .route("/api/training/stats", get(dash_training_stats))
        .route("/api/training/clear", get(dash_training_clear))
        .route("/api/domains/clear", get(dash_domains_clear))
        .route("/api/domain/delete", get(dash_domain_delete))
        .route("/api/knowledge/clear", get(dash_knowledge_clear))
        .route("/api/knowledge/delete", get(dash_knowledge_delete))
        .route("/api/sources/clear", get(dash_sources_clear))
        .route("/api/source/delete", get(dash_source_delete))
        .route("/api/episodes/clear", get(dash_episodes_clear))
        .layer(CorsLayer::permissive());

    let app = if let Some(token) = server_auth_token.clone() {
        app.layer(middleware::from_fn_with_state(
            AuthState { token },
            require_bearer_token,
        ))
    } else {
        app
    };
    let app = app.with_state(state);

    let bind_addr = if server_auth_token.is_some() {
        "0.0.0.0"
    } else {
        "127.0.0.1"
    };
    let listener = tokio::net::TcpListener::bind((bind_addr, port))
        .await
        .unwrap_or_else(|e| panic!("dashboard: cannot bind {bind_addr}:{port}: {e}"));

    axum::serve(listener, app).await.unwrap();
}


// Stage D tests. `resolve_harness` and `usage_rollup`/`storage_health_entry`
// are pure functions with no store dependency, so they're tested directly.
// The clientInfo capture path itself (`Klayer::initialize`) has no testable
// hook in rmcp 0.16 short of driving a real stdio handshake through a full
// Klayer (CodeStore/TrainStore/SessionStore, libsql) — out of reach of this
// rusqlite-only test binary per the stage_c_tests note above — so we test the
// fallback/default-harness resolution logic it feeds into instead.
#[cfg(test)]
mod stage_d_tests {
    use crate::tools::resolve_harness;
    use super::*;

    fn fixture() -> Store {
        let store = Store::open(":memory:").expect("open in-memory store");
        store.migrate().expect("migrate");
        store
    }

    #[test]
    fn resolve_harness_prefers_explicit_over_captured() {
        assert_eq!(
            resolve_harness(
                Some("explicit-harness".into()),
                Some("captured-harness".into())
            ),
            Some("explicit-harness".into())
        );
    }

    #[test]
    fn resolve_harness_falls_back_to_captured_clientinfo() {
        assert_eq!(
            resolve_harness(None, Some("captured-harness".into())),
            Some("captured-harness".into())
        );
    }

    #[test]
    fn resolve_harness_none_when_neither_supplied() {
        assert_eq!(resolve_harness(None, None), None);
    }

    #[test]
    fn episodes_table_round_trips_model_tokens_cost() {
        let store = fixture();
        store
            .log_episode_auto(
                "run-tokens",
                Some("recall"),
                Some("recall domain=x query=y"),
                Some("returned 2 hits"),
                Some("success"),
                Some("x"),
                Some("claude-sonnet-5"),
                Some(1234),
                Some(0.05),
            )
            .unwrap();
        let episodes = store.list_episodes(Some("run-tokens")).unwrap();
        assert_eq!(episodes.len(), 1);
        assert_eq!(episodes[0].model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(episodes[0].tokens_used, Some(1234));
        assert_eq!(episodes[0].cost, Some(0.05));
    }

    #[test]
    fn episode_defaults_to_no_usage_metadata() {
        let store = fixture();
        store
            .log_episode_auto(
                "run-no-tokens",
                Some("promote"),
                Some("promote id=1"),
                Some("promoted"),
                Some("success"),
                None,
                None,
                None,
                None,
            )
            .unwrap();
        let episodes = store.list_episodes(Some("run-no-tokens")).unwrap();
        assert_eq!(episodes[0].model, None);
        assert_eq!(episodes[0].tokens_used, None);
        assert_eq!(episodes[0].cost, None);
    }

    #[test]
    fn usage_rollup_sums_tokens_and_cost_including_estimated_fallbacks() {
        let store = fixture();
        store
            .log_episode_auto(
                "run-usage",
                Some("recall"),
                Some("recall"),
                Some("returned 1 hits"),
                Some("success"),
                Some("x"),
                Some("model-a"),
                Some(100),
                Some(0.01),
            )
            .unwrap();
        store
            .log_episode_auto(
                "run-usage",
                Some("remember"),
                Some("remember"),
                Some("remembered fact #1"),
                Some("success"),
                Some("x"),
                Some("model-a"),
                Some(50),
                Some(0.02),
            )
            .unwrap();
        // Episode logged without explicit usage — auto-estimates tokens and cost.
        store
            .log_episode_auto(
                "run-usage",
                Some("promote"),
                Some("promote id=1"),
                Some("promoted"),
                Some("success"),
                None,
                None,
                None,
                None,
            )
            .unwrap();

        let episodes = store.list_episodes(Some("run-usage")).unwrap();
        assert_eq!(episodes.len(), 3);
        let rollup = usage_rollup(&episodes);
        assert_eq!(rollup["sample_size"], 3);
        assert!(rollup["total_tokens_used"].as_i64().unwrap() >= 155);
        assert!(rollup["total_cost"].as_f64().unwrap() > 0.03);
        assert_eq!(rollup["by_action"]["recall"], 1);
        assert_eq!(rollup["by_action"]["remember"], 1);
        assert_eq!(rollup["by_action"]["promote id=1"], 1);

        let daily = rollup["daily_usage"].as_object().unwrap();
        assert_eq!(daily.len(), 1, "all three episodes logged the same day");
        let (_, day_entry) = daily.iter().next().unwrap();
        assert!(day_entry["tokens_used"].as_i64().unwrap() >= 155);
        assert_eq!(day_entry["episodes_with_tokens"], 3);
        assert_eq!(day_entry["episodes_with_cost"], 3);
    }

    #[test]
    fn storage_health_entry_kl_store_has_no_sync_field() {
        let v = storage_health_entry("sqlite", true, None);
        assert_eq!(v["engine"], "sqlite");
        assert_eq!(v["healthy"], true);
        assert!(v.get("sync").is_none());
    }

    #[test]
    fn storage_health_entry_libsql_surfaces_sync_snapshot() {
        let snap = kl_core::SyncHealthSnapshot {
            remote_configured: true,
            last_success_at: Some(1_700_000_000),
            consecutive_failures: 2,
            fallback_events: 5,
        };
        let v = storage_health_entry("libsql", false, Some(&snap));
        assert_eq!(v["engine"], "libsql");
        assert_eq!(v["healthy"], false);
        assert_eq!(v["sync"]["remote_configured"], true);
        assert_eq!(v["sync"]["last_success_at"], 1_700_000_000i64);
        assert_eq!(v["sync"]["consecutive_failures"], 2);
        assert_eq!(v["sync"]["fallback_events"], 5);
    }
}

// Stage E tests. `plan_model_registry_action` and the `/api/model-registry`
// `/api/routing-rules` shape builders are pure functions with no store
// dependency (same rationale as `resolve_harness` in stage_d_tests above),
// so they're tested directly without a full `Klayer`/libsql instance.
#[cfg(test)]
mod stage_e_tests {
    use crate::tools::{plan_model_registry_action, ModelRegistryAction, ModelRegistryParams};
    use super::*;

    fn base_params(action: &str) -> ModelRegistryParams {
        ModelRegistryParams {
            action: action.into(),
            harness: "claude-code".into(),
            model_id: "opus".into(),
            capability_tier: "heavy-reasoning".into(),
            cost_weight: 10.0,
            sub_agent_name: None,
            domain_type: None,
            task_type: None,
            complexity_tier: None,
            confirm: Some(true),
        }
    }

    #[test]
    fn add_model_plans_an_upsert() {
        let plan = plan_model_registry_action(&base_params("add_model")).unwrap();
        assert_eq!(plan, ModelRegistryAction::UpsertModel);
    }

    #[test]
    fn add_sub_agent_requires_sub_agent_name() {
        let p = base_params("add_sub_agent");
        let e = plan_model_registry_action(&p).unwrap_err();
        assert!(e.contains("sub_agent_name"));
    }

    #[test]
    fn add_sub_agent_with_name_plans_an_upsert() {
        let mut p = base_params("add_sub_agent");
        p.sub_agent_name = Some("frontend-agent".into());
        let plan = plan_model_registry_action(&p).unwrap();
        assert_eq!(plan, ModelRegistryAction::UpsertModel);
    }

    #[test]
    fn add_routing_rule_requires_all_three_routing_fields() {
        let mut p = base_params("add_routing_rule");
        p.domain_type = Some("frontend".into());
        // task_type / complexity_tier left unset.
        let e = plan_model_registry_action(&p).unwrap_err();
        assert!(e.contains("add_routing_rule requires"));
    }

    #[test]
    fn add_routing_rule_plans_correctly_with_all_fields() {
        let mut p = base_params("add_routing_rule");
        p.domain_type = Some("frontend".into());
        p.task_type = Some("feature".into());
        p.complexity_tier = Some("high".into());
        let plan = plan_model_registry_action(&p).unwrap();
        assert_eq!(
            plan,
            ModelRegistryAction::AddRoutingRule {
                domain_type: "frontend".into(),
                task_type: "feature".into(),
                complexity_tier: "high".into(),
            }
        );
    }

    #[test]
    fn remove_without_routing_fields_targets_the_model_registry() {
        let plan = plan_model_registry_action(&base_params("remove")).unwrap();
        assert_eq!(plan, ModelRegistryAction::RemoveModel);
    }

    #[test]
    fn remove_with_all_routing_fields_targets_a_routing_rule() {
        let mut p = base_params("remove");
        p.domain_type = Some("backend".into());
        p.task_type = Some("crud".into());
        p.complexity_tier = Some("low".into());
        let plan = plan_model_registry_action(&p).unwrap();
        assert_eq!(
            plan,
            ModelRegistryAction::RemoveRoutingRule {
                domain_type: "backend".into(),
                task_type: "crud".into(),
                complexity_tier: "low".into(),
            }
        );
    }

    #[test]
    fn remove_with_partial_routing_fields_is_rejected_not_silently_a_model_delete() {
        let mut p = base_params("remove");
        p.domain_type = Some("backend".into());
        // task_type / complexity_tier left unset.
        let e = plan_model_registry_action(&p).unwrap_err();
        assert!(e.contains("together"));
    }

    #[test]
    fn unknown_action_is_rejected() {
        let e = plan_model_registry_action(&base_params("delete")).unwrap_err();
        assert!(e.contains("action must be"));
    }

    #[test]
    fn model_registry_grouped_groups_by_harness_then_tier() {
        let rows = vec![
            kl_core::ModelRegistryRow {
                harness: "claude-code".into(),
                model_id: "opus".into(),
                capability_tier: "heavy-reasoning".into(),
                cost_weight: 10.0,
                sub_agent_name: None,
            },
            kl_core::ModelRegistryRow {
                harness: "claude-code".into(),
                model_id: "haiku".into(),
                capability_tier: "fast-cheap".into(),
                cost_weight: 1.0,
                sub_agent_name: None,
            },
            kl_core::ModelRegistryRow {
                harness: "cursor".into(),
                model_id: "gpt".into(),
                capability_tier: "balanced".into(),
                cost_weight: 5.0,
                sub_agent_name: Some("frontend-agent".into()),
            },
        ];
        let store = Store::open(":memory:").expect("open in-memory store");
        store.migrate().expect("migrate");
        let shaped = model_registry_grouped(&rows, &store, &Some("claude-code".to_string()));
        let harnesses = shaped["harnesses"].as_array().unwrap();
        assert_eq!(harnesses.len(), 2);
        let claude = harnesses
            .iter()
            .find(|h| h["harness"] == "claude-code")
            .unwrap();
        assert!(claude["tiers"]["heavy-reasoning"].as_array().unwrap().len() == 1);
        assert!(claude["tiers"]["fast-cheap"].as_array().unwrap().len() == 1);
        assert_eq!(claude["connected_now"], true);
        assert!(claude["last_edit_ts"].is_null());
        assert!(claude["last_recommendation_ts"].is_null());
        let cursor = harnesses.iter().find(|h| h["harness"] == "cursor").unwrap();
        assert_eq!(
            cursor["tiers"]["balanced"][0]["sub_agent_name"],
            "frontend-agent"
        );
        assert_eq!(cursor["connected_now"], false);
    }

    #[test]
    fn model_registry_grouped_surfaces_last_edit_ts_from_episodes() {
        let store = Store::open(":memory:").expect("open in-memory store");
        store.migrate().expect("migrate");
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
        let rows = vec![kl_core::ModelRegistryRow {
            harness: "claude-code".into(),
            model_id: "opus".into(),
            capability_tier: "heavy-reasoning".into(),
            cost_weight: 10.0,
            sub_agent_name: None,
        }];
        let shaped = model_registry_grouped(&rows, &store, &None);
        let harnesses = shaped["harnesses"].as_array().unwrap();
        let claude = harnesses[0].clone();
        assert!(claude["last_edit_ts"].as_i64().is_some());
        assert!(claude["last_recommendation_ts"].is_null());
        assert_eq!(claude["connected_now"], false);
    }

    #[test]
    fn routing_rules_grouped_groups_by_harness_as_a_matrix() {
        let rows = vec![
            kl_core::RoutingRuleRow {
                harness: "claude-code".into(),
                domain_type: "frontend".into(),
                task_type: "feature".into(),
                complexity_tier: "high".into(),
                model_id: "opus".into(),
            },
            kl_core::RoutingRuleRow {
                harness: "claude-code".into(),
                domain_type: "backend".into(),
                task_type: "crud".into(),
                complexity_tier: "low".into(),
                model_id: "haiku".into(),
            },
        ];
        let shaped = routing_rules_grouped(&rows);
        let harnesses = shaped["harnesses"].as_array().unwrap();
        assert_eq!(harnesses.len(), 1);
        let rules = harnesses[0]["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0]["model_id"], "opus");
    }
}

#[cfg(test)]
mod stage_f_tests {
    use super::*;

    #[test]
    fn bearer_token_matches_exact_token() {
        assert!(bearer_token_matches(
            "secret-token",
            Some("Bearer secret-token")
        ));
    }

    #[test]
    fn bearer_token_rejects_mismatched_token() {
        assert!(!bearer_token_matches(
            "secret-token",
            Some("Bearer wrong-token")
        ));
    }

    #[test]
    fn bearer_token_rejects_missing_header() {
        assert!(!bearer_token_matches("secret-token", None));
    }

    #[test]
    fn bearer_token_rejects_missing_bearer_prefix() {
        assert!(!bearer_token_matches("secret-token", Some("secret-token")));
    }

    #[test]
    fn bearer_token_rejects_empty_presented_value() {
        assert!(!bearer_token_matches("secret-token", Some("Bearer ")));
    }
}

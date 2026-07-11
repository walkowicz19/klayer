//! kl-core — shared types and traits for the klayer knowledge layer.
//!
//! The trust lifecycle is the safety spine of the whole system:
//!   untrusted -> proposed -> reviewed | user
//! Only `Reviewed` and `User` knowledge is ever *enforced*. Ingested web text is
//! always `Untrusted` and reaches the model only as cited data via `recall`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// What kind of curated knowledge an item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A statement the model may assert (with provenance).
    Fact,
    /// A constraint to enforce (trigger + severity + remediation).
    Rule,
    /// An ordered playbook an agent follows.
    Procedure,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Fact => "fact",
            Kind::Rule => "rule",
            Kind::Procedure => "procedure",
        }
    }
    pub fn parse(s: &str) -> Option<Kind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "fact" => Some(Kind::Fact),
            "rule" => Some(Kind::Rule),
            "procedure" => Some(Kind::Procedure),
            _ => None,
        }
    }
}

/// Trust tier. Higher rank wins on conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trust {
    Untrusted,
    Proposed,
    Reviewed,
    User,
}

impl Trust {
    pub fn as_str(&self) -> &'static str {
        match self {
            Trust::Untrusted => "untrusted",
            Trust::Proposed => "proposed",
            Trust::Reviewed => "reviewed",
            Trust::User => "user",
        }
    }
    pub fn parse(s: &str) -> Trust {
        match s.trim().to_ascii_lowercase().as_str() {
            "user" => Trust::User,
            "reviewed" => Trust::Reviewed,
            "proposed" => Trust::Proposed,
            _ => Trust::Untrusted,
        }
    }
    pub fn rank(&self) -> i64 {
        match self {
            Trust::Untrusted => 0,
            Trust::Proposed => 1,
            Trust::Reviewed => 2,
            Trust::User => 3,
        }
    }
    /// Only reviewed/user knowledge may be *enforced* by the model.
    pub fn is_enforceable(&self) -> bool {
        matches!(self, Trust::Reviewed | Trust::User)
    }
}

/// A single retrieval result handed to the model. Always carries provenance and
/// trust so the router's trust rules can be applied.
#[derive(Debug, Clone, Serialize)]
pub struct RecallHit {
    pub source_kind: String, // "chunk" (reference tier) | "knowledge"
    pub kind: Option<String>,
    pub title: String,
    pub body: String,
    pub domain: String,
    pub trust: String,
    pub enforceable: bool,
    pub provenance: Option<String>,
    pub fetched_at: Option<i64>,
    pub score: f64,
}

/// A web search hit. Note: search results are DATA, never instructions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// A row returned by list_knowledge. Carries the id so callers can
/// pass it directly to promote/forget without a separate recall.
#[derive(Debug, Clone, Serialize)]
pub struct KnowledgeRow {
    pub id: i64,
    pub kind: String,
    pub domain: String,
    pub stage: Option<String>,
    pub title: String,
    pub body: String,
    pub trust: String,
    pub enforceable: bool,
    pub severity: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub conflict_with_id: Option<i64>,
    pub conflict_status: Option<String>,
}

/// A knowledge row plus its originating source's title/uri (if any), used to
/// enrich compliance reports with provenance beyond the bare `source_id`.
#[derive(Debug, Clone, Serialize)]
pub struct KnowledgeItemWithSource {
    #[serde(flatten)]
    pub row: KnowledgeRow,
    pub source_title: Option<String>,
    pub source_uri: Option<String>,
}

/// Registry row that drives the generated router.
#[derive(Debug, Clone, Serialize)]
pub struct DomainRow {
    pub name: String,
    pub description: Option<String>,
    pub query_hint: Option<String>,
    pub doc_count: i64,
    pub rule_count: i64,
    pub last_updated: Option<i64>,
    pub enforced: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct StageRow {
    pub taxonomy: String,
    pub name: String,
    pub ordinal: i64,
    pub description: Option<String>,
}

/// A row returned by list_sources.
#[derive(Debug, Clone, Serialize)]
pub struct SourceRow {
    pub id: i64,
    pub kind: String,
    pub uri: Option<String>,
    pub title: Option<String>,
    pub domain: String,
    pub fetched_at: i64,
    pub trust: String,
}

/// A row returned by list_journal / recall_session — one entry of a repo-scoped
/// session journal (curated "what I did / failed / must not repeat" memory).
#[derive(Debug, Clone, Serialize)]
pub struct JournalRow {
    pub id: i64,
    pub repo: String,
    pub kind: String, // 'done' | 'failed' | 'avoid' | 'decision' | 'note'
    pub title: String,
    pub body: Option<String>,
    pub ts: i64,
    pub is_checkpoint: bool,
}

/// A row returned by list_media / get_media — an image attached as evidence to
/// a knowledge item, or standalone pending attachment. `trust` is `None` for
/// standalone media: it has no governance tier of its own until `attach_media`
/// links it to a knowledge item and inherits that item's trust.
#[derive(Debug, Clone, Serialize)]
pub struct MediaRow {
    pub media_id: i64,
    pub storage_ref: String,
    pub mime_type: String,
    pub size_bytes: i64,
    pub caption: Option<String>,
    pub knowledge_id: Option<i64>,
    pub domain: Option<String>,
    pub trust: Option<String>,
    pub created_at: i64,
}

/// A row returned by list_submissions — a marketplace publish request awaiting
/// admin review. `item_count` summarizes the snapshotted knowledge items.
#[derive(Debug, Clone, Serialize)]
pub struct SubmissionRow {
    pub id: i64,
    pub slug: String,
    pub description: Option<String>,
    pub query_hint: Option<String>,
    pub item_count: i64,
    pub status: String, // 'pending' | 'approved' | 'denied'
    pub note: Option<String>,
    pub submitted_at: i64,
    pub reviewed_at: Option<i64>,
    pub author: Option<String>,
}

/// A single curated knowledge item inside a marketplace template. Shared by the
/// server (apply) and the store (publish snapshot) so publish/apply are inverse.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplaceItem {
    pub kind: Kind,
    pub stage: Option<String>,
    pub title: String,
    pub body: String,
    pub trigger: Option<String>,
    pub severity: Option<String>,
    pub remediation: Option<String>,
}

/// A domain template in the marketplace: a slug, discovery metadata, and its
/// curated items. Serialized as one element of the marketplace.json array.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplaceTemplate {
    pub slug: String,
    pub description: String,
    pub query_hint: String,
    /// The publisher's registered author name. Optional so existing
    /// marketplace.json entries (which predate attribution) still deserialize.
    #[serde(default)]
    pub author: Option<String>,
    pub items: Vec<MarketplaceItem>,
}

/// A row returned by list_model_registry — one `(harness, model_id,
/// sub_agent_name?)` entry of the Model Registry.
#[derive(Debug, Clone, Serialize)]
pub struct ModelRegistryRow {
    pub harness: String,
    pub model_id: String,
    pub capability_tier: String,
    pub cost_weight: f64,
    pub sub_agent_name: Option<String>,
}

/// A row returned by list_routing_rules — one `(harness, domain_type,
/// task_type, complexity_tier)` -> `model_id` mapping.
#[derive(Debug, Clone, Serialize)]
pub struct RoutingRuleRow {
    pub harness: String,
    pub domain_type: String,
    pub task_type: String,
    pub complexity_tier: String,
    pub model_id: String,
}

/// A row returned by list_episodes.
#[derive(Debug, Clone, Serialize)]
pub struct EpisodeRow {
    pub id: i64,
    pub run_id: String,
    pub step: i64,
    pub stage: Option<String>,
    pub action: Option<String>,
    pub observation: Option<String>,
    pub outcome: Option<String>,
    pub ts: i64,
    pub knowledge_ids_used: Vec<i64>,
    /// The single domain this step acted on, when there is a clear one (e.g.
    /// recall/remember/propose/execute_change). `None` for steps without a
    /// clear single-domain target.
    pub domain: Option<String>,
    /// Best-effort, self-reported model identifier for this step (MCP has no
    /// standard field for this; `None` unless a caller passed it explicitly).
    pub model: Option<String>,
    /// Best-effort, self-reported token count for this step, if the caller
    /// chose to report one.
    pub tokens_used: Option<i64>,
    /// Best-effort, self-reported cost for this step, if the caller chose to
    /// report one.
    pub cost: Option<f64>,
}

/// Pluggable web search. Implemented in kl-search; held as a trait object so the
/// backend can be swapped (scraper today, real API later) without touching the server.
#[async_trait]
pub trait SearchBackend: Send + Sync {
    async fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<SearchResult>>;
}

/// Future extension point: vector embeddings. Default build is keyword-only and
/// never constructs an Embedder; the vector path lives behind a feature flag.
pub trait Embedder: Send + Sync {
    fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>>;
    fn dims(&self) -> usize;
}

// ── libsql storage engine helpers ───────────────────────────────────────────
//
// Shared by kl-code, kl-train, and kl-session: each opens a `libsql::Database`
// either as a pure local file (no Turso configured) or as an embedded replica
// syncing against a remote `libsql://...` URL. Kept here so the three stores
// don't duplicate the same Builder/health-tracking logic.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Env vars that opt a store into embedded-replica mode. Empty/unset means
/// pure local mode — no behavior change from plain SQLite-over-libsql.
pub const TURSO_URL_ENV: &str = "KLAYER_TURSO_URL";
pub const TURSO_TOKEN_ENV: &str = "KLAYER_TURSO_TOKEN";

/// Read `KLAYER_TURSO_URL`/`KLAYER_TURSO_TOKEN`; `Some` only if both are set
/// and non-empty.
pub fn turso_config() -> Option<(String, String)> {
    let url = std::env::var(TURSO_URL_ENV)
        .ok()
        .filter(|s| !s.is_empty())?;
    let token = std::env::var(TURSO_TOKEN_ENV)
        .ok()
        .filter(|s| !s.is_empty())?;
    Some((url, token))
}

/// Open a `libsql::Database` at `path`: an embedded replica of `KLAYER_TURSO_URL`
/// if configured, otherwise a pure local file.
pub async fn open_db(path: &str) -> anyhow::Result<libsql::Database> {
    let db = match turso_config() {
        Some((url, token)) => {
            libsql::Builder::new_remote_replica(path, url, token)
                .build()
                .await?
        }
        None => libsql::Builder::new_local(path).build().await?,
    };
    Ok(db)
}

/// Sync-health counters for an embedded-replica store. Cheap to snapshot from
/// any thread; updated only by the background sync task.
#[derive(Debug, Default)]
pub struct SyncHealth {
    last_success_at: AtomicI64,
    consecutive_failures: AtomicU64,
    fallback_events: AtomicU64,
}

/// Point-in-time read of `SyncHealth`, safe to serialize for a dashboard.
#[derive(Debug, Clone, Serialize)]
pub struct SyncHealthSnapshot {
    pub remote_configured: bool,
    /// Unix timestamp of the last successful `sync()`, if any.
    pub last_success_at: Option<i64>,
    pub consecutive_failures: u64,
    /// Monotonically increasing count of sync failures ever seen (a "we fell
    /// back to local-only data" event).
    pub fallback_events: u64,
}

impl SyncHealth {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn record_success(&self) {
        self.last_success_at
            .store(chrono::Utc::now().timestamp(), Ordering::Relaxed);
        self.consecutive_failures.store(0, Ordering::Relaxed);
    }

    fn record_failure(&self) {
        self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
        self.fallback_events.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self, remote_configured: bool) -> SyncHealthSnapshot {
        let last = self.last_success_at.load(Ordering::Relaxed);
        SyncHealthSnapshot {
            remote_configured,
            last_success_at: if last == 0 { None } else { Some(last) },
            consecutive_failures: self.consecutive_failures.load(Ordering::Relaxed),
            fallback_events: self.fallback_events.load(Ordering::Relaxed),
        }
    }
}

/// Spawn the periodic embedded-replica sync loop. A no-op unless Turso is
/// configured. Sync failures are swallowed (never propagated to callers) —
/// local reads/writes still work off the replica's on-disk copy, so a
/// transient network blip must not turn into an error for MCP tool callers.
pub fn spawn_sync_task(db: Arc<libsql::Database>, health: Arc<SyncHealth>) {
    if turso_config().is_none() {
        return;
    }
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            match db.sync().await {
                Ok(_) => health.record_success(),
                Err(e) => {
                    tracing::warn!("libsql embedded-replica sync failed: {e:#}");
                    health.record_failure();
                }
            }
        }
    });
}

#[cfg(test)]
mod sync_health_tests {
    use super::*;

    #[test]
    fn fresh_health_snapshot_is_all_zero() {
        let health = SyncHealth::new();
        let snap = health.snapshot(false);
        assert!(!snap.remote_configured);
        assert_eq!(snap.last_success_at, None);
        assert_eq!(snap.consecutive_failures, 0);
        assert_eq!(snap.fallback_events, 0);
    }

    #[test]
    fn failure_then_success_resets_consecutive_but_keeps_fallback_total() {
        let health = SyncHealth::new();
        health.record_failure();
        health.record_failure();
        let snap = health.snapshot(true);
        assert!(snap.remote_configured);
        assert_eq!(snap.consecutive_failures, 2);
        assert_eq!(snap.fallback_events, 2);
        assert_eq!(snap.last_success_at, None);

        health.record_success();
        let snap = health.snapshot(true);
        assert_eq!(snap.consecutive_failures, 0);
        assert_eq!(
            snap.fallback_events, 2,
            "fallback total is cumulative, not reset by success"
        );
        assert!(snap.last_success_at.is_some());
    }
}

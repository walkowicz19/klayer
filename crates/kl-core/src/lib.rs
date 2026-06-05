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
}

#[derive(Debug, Clone, Serialize)]
pub struct StageRow {
    pub taxonomy: String,
    pub name: String,
    pub ordinal: i64,
    pub description: Option<String>,
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

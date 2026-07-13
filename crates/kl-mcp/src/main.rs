//! klayer — a domain-agnostic, grounded knowledge layer exposed as one MCP server.
//!
//! Tools: recall, search_web, ingest, remember, propose, promote, forget,
//! set_preference, list_domains, register_domain, execute_change, log_episode,
//! index_codebase, search_code, list_repos, forget_repo, capture_example,
//! author_example, promote_example, list_training, export_dataset, queue_weak,
//! seed_from_topics, log_work, recall_session, ingest_media, attach_media,
//! list_media.
//!
//! Media (Stage G): images only — base64 bytes in, written to KLAYER_MEDIA_DIR
//! (default ~/.klayer/media) by kl-store::media, content-hash named. Standalone
//! media has no trust tier until attached to a knowledge item (inherits that
//! item's tier). Video and an object-store backend are deliberately deferred
//! later increments, not implemented here.
//!
//! Transport: stdio (works with Claude Code, Claude Desktop, Cursor, etc.).
//! Storage:   four DB files (SQLite via kl-store, libsql for the other three):
//!   KLAYER_DB         (default ./klayer.db)         — knowledge, episodes, preferences
//!   KLAYER_CODE_DB    (default ./klayer_code.db)    — indexed codebase memory
//!   KLAYER_TRAIN_DB   (default ./klayer_train.db)   — trust-gated training examples
//!   KLAYER_SESSION_DB (default ./klayer_session.db) — repo-scoped session journal
//! The libsql-backed stores become embedded replicas (periodic background sync)
//! when KLAYER_TURSO_URL / KLAYER_TURSO_TOKEN are set; otherwise pure local files.
//! Dashboard: HTTP on KLAYER_DASHBOARD_PORT (default 7474). URL logged to stderr on start.
//!
//! This file is intentionally a thin composition root: process bootstrap and
//! CLI handling live in `bootstrap`, the dashboard HTTP server lives in
//! `dashboard`, and the MCP tool surface (the `Klayer` server struct and its
//! `#[tool]` methods, split by workflow area) lives in `tools`.

mod bootstrap;
mod compliance;
mod dashboard;
mod notify;
mod tools;
mod tui;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    bootstrap::run().await
}

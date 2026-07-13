//! Process bootstrap and CLI entry point: config-file paths, DB path
//! resolution, server-mode auth token generation, background maintenance
//! tasks (notification watch, retention sweep), and the top-level `run()`
//! that `main.rs`'s `main()` delegates to.

use std::sync::Arc;

use anyhow::Result;
use kl_code::CodeStore;
use kl_session::SessionStore;
use kl_store::Store;
use kl_train::TrainStore;
use rmcp::{transport::stdio, ServiceExt};
use tracing_subscriber::EnvFilter;

use crate::dashboard::{load_dashboard_html, start_dashboard};
use crate::notify;
use crate::tools::Klayer;
use crate::tui;

fn get_claude_config_path() -> Option<std::path::PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var("APPDATA").ok()?;
        Some(
            std::path::PathBuf::from(appdata)
                .join("Claude")
                .join("claude_desktop_config.json"),
        )
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").ok()?;
        Some(
            std::path::PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("Claude")
                .join("claude_desktop_config.json"),
        )
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let home = std::env::var("HOME").ok()?;
        Some(
            std::path::PathBuf::from(home)
                .join(".config")
                .join("Claude")
                .join("claude_desktop_config.json"),
        )
    }
}

/// Resolves the user's home directory across the env vars IDEs/MCP clients
/// commonly strip when spawning the server subprocess with a minimal
/// environment. Deliberately does NOT fall back to "." (the process's
/// current working directory): that silent fallback used to make klayer
/// write a fresh `./.klayer/` — with its own databases — inside whatever
/// project folder happened to be open, so switching folders in an IDE
/// looked like klayer "duplicating" its databases. If no home directory can
/// be resolved at all, fail loudly instead of guessing.
fn resolve_home_dir() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("USERPROFILE") {
        if !p.is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    if let Ok(p) = std::env::var("HOME") {
        if !p.is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    if cfg!(target_os = "windows") {
        if let (Ok(drive), Ok(path)) = (std::env::var("HOMEDRIVE"), std::env::var("HOMEPATH")) {
            if !drive.is_empty() && !path.is_empty() {
                // HOMEPATH already carries its leading separator (e.g. "\Users\name"),
                // and PathBuf::join drops the separator entirely when joining onto a
                // bare drive prefix like "C:" (no root component) — concatenate the
                // strings directly instead of going through `join`.
                return std::path::PathBuf::from(format!("{drive}{path}"));
            }
        }
    }
    eprintln!(
        "klayer: could not resolve a home directory (none of USERPROFILE, HOME{} is set in \
         this process's environment). Refusing to fall back to the current directory, since \
         that would scatter a separate ./.klayer database per folder. Fix your MCP client's \
         launch environment to inherit USERPROFILE/HOME, or set KLAYER_DB/KLAYER_CODE_DB/\
         KLAYER_TRAIN_DB/KLAYER_SESSION_DB explicitly.",
        if cfg!(target_os = "windows") { "/HOMEDRIVE+HOMEPATH" } else { "" }
    );
    std::process::exit(1);
}

pub(crate) fn get_klayer_dir() -> std::path::PathBuf {
    resolve_home_dir().join(".klayer")
}

pub(crate) struct DbPaths {
    pub(crate) db: String,
    pub(crate) code_db: String,
    pub(crate) train_db: String,
    pub(crate) session_db: String,
}

/// Resolves the four DB file paths the same way `main()` always has (env var
/// override, else under `get_klayer_dir()`) — shared with `tui::open_stores`
/// so `klayer status`/`klayer tui` see the exact same databases the MCP
/// server and dashboard do.
pub(crate) fn resolve_db_paths() -> DbPaths {
    let klayer_dir = get_klayer_dir();
    let db = std::env::var("KLAYER_DB")
        .unwrap_or_else(|_| klayer_dir.join("klayer.db").to_string_lossy().to_string());
    let code_db = std::env::var("KLAYER_CODE_DB").unwrap_or_else(|_| {
        klayer_dir
            .join("klayer_code.db")
            .to_string_lossy()
            .to_string()
    });
    let train_db = std::env::var("KLAYER_TRAIN_DB").unwrap_or_else(|_| {
        klayer_dir
            .join("klayer_train.db")
            .to_string_lossy()
            .to_string()
    });
    let session_db = std::env::var("KLAYER_SESSION_DB").unwrap_or_else(|_| {
        klayer_dir
            .join("klayer_session.db")
            .to_string_lossy()
            .to_string()
    });
    DbPaths {
        db,
        code_db,
        train_db,
        session_db,
    }
}

fn generate_server_token() -> String {
    use rand::Rng;
    let bytes: [u8; 32] = rand::thread_rng().gen();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Resolves the server-mode auth token: `KLAYER_SERVER_TOKEN` wins if set;
/// otherwise a token is persisted under `get_klayer_dir()` so restarts reuse
/// the same value instead of invalidating every previously-issued client.
fn resolve_server_token(klayer_dir: &std::path::Path) -> String {
    if let Ok(token) = std::env::var("KLAYER_SERVER_TOKEN") {
        return token;
    }
    let token_path = klayer_dir.join("server_token.txt");
    if let Ok(existing) = std::fs::read_to_string(&token_path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    let token = generate_server_token();
    if let Err(e) = std::fs::create_dir_all(klayer_dir) {
        tracing::warn!("failed to create {}: {e}", klayer_dir.display());
    }
    if let Err(e) = std::fs::write(&token_path, &token) {
        tracing::warn!(
            "failed to persist server-mode auth token to {}: {e}",
            token_path.display()
        );
    }
    token
}

fn print_tls_warning_if_needed() {
    let tls_terminated = std::env::var("KLAYER_TLS_TERMINATED")
        .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false);
    if tls_terminated {
        return;
    }
    eprintln!(
        "\n\
         ############################################################\n\
         # WARNING: klayer is running in --mode=server WITHOUT TLS. #\n\
         # All traffic (including the auth token) is UNENCRYPTED.   #\n\
         # Put a reverse proxy (nginx, Caddy, etc.) in front of this#\n\
         # process to terminate TLS before exposing it beyond       #\n\
         # localhost.                                               #\n\
         # Set KLAYER_TLS_TERMINATED=1 to silence this warning once #\n\
         # a proxy is in place.                                     #\n\
         ############################################################\n"
    );
}

/// Root directory media bytes are written under. `KLAYER_MEDIA_DIR` overrides;
/// otherwise defaults alongside the other klayer state under `get_klayer_dir()`.
pub(crate) fn get_media_dir() -> std::path::PathBuf {
    std::env::var("KLAYER_MEDIA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| get_klayer_dir().join("media"))
}

pub(crate) fn ensure_parent_dir(path: &str) -> Result<()> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        if parent.as_os_str().len() > 0 {
            std::fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}

fn handle_install_or_print(print_config: bool, install_config: bool) -> Result<Option<()>> {
    if !print_config && !install_config {
        return Ok(None);
    }

    let exe_path = std::env::current_exe()?;
    let klayer_dir = get_klayer_dir();

    let (exe_str, db_str, code_db_str, train_db_str, session_db_str) =
        if cfg!(target_os = "windows") {
            (
                exe_path.to_string_lossy().replace("/", "\\"),
                klayer_dir
                    .join("klayer.db")
                    .to_string_lossy()
                    .replace("/", "\\"),
                klayer_dir
                    .join("klayer_code.db")
                    .to_string_lossy()
                    .replace("/", "\\"),
                klayer_dir
                    .join("klayer_train.db")
                    .to_string_lossy()
                    .replace("/", "\\"),
                klayer_dir
                    .join("klayer_session.db")
                    .to_string_lossy()
                    .replace("/", "\\"),
            )
        } else {
            (
                exe_path.to_string_lossy().replace("\\", "/"),
                klayer_dir
                    .join("klayer.db")
                    .to_string_lossy()
                    .replace("\\", "/"),
                klayer_dir
                    .join("klayer_code.db")
                    .to_string_lossy()
                    .replace("\\", "/"),
                klayer_dir
                    .join("klayer_train.db")
                    .to_string_lossy()
                    .replace("\\", "/"),
                klayer_dir
                    .join("klayer_session.db")
                    .to_string_lossy()
                    .replace("\\", "/"),
            )
        };

    if print_config {
        let config = serde_json::json!({
            "mcpServers": {
                "klayer": {
                    "command": exe_str,
                    "env": {
                        "KLAYER_DB": db_str,
                        "KLAYER_CODE_DB": code_db_str,
                        "KLAYER_TRAIN_DB": train_db_str,
                        "KLAYER_SESSION_DB": session_db_str
                    }
                }
            }
        });
        println!("{}", serde_json::to_string_pretty(&config)?);
        return Ok(Some(()));
    }

    if install_config {
        let config_path = get_claude_config_path();
        if let Some(path) = config_path {
            let mut root: serde_json::Value = if path.exists() {
                let s = std::fs::read_to_string(&path)?;
                serde_json::from_str(&s).unwrap_or(serde_json::json!({}))
            } else {
                serde_json::json!({})
            };

            if !root.is_object() {
                root = serde_json::json!({});
            }
            if root.get("mcpServers").is_none() {
                root["mcpServers"] = serde_json::json!({});
            }

            root["mcpServers"]["klayer"] = serde_json::json!({
                "command": exe_str,
                "env": {
                    "KLAYER_DB": db_str,
                    "KLAYER_CODE_DB": code_db_str,
                    "KLAYER_TRAIN_DB": train_db_str,
                    "KLAYER_SESSION_DB": session_db_str
                }
            });

            if let Some(p) = path.parent() {
                std::fs::create_dir_all(p)?;
            }

            let pretty = serde_json::to_string_pretty(&root)?;
            std::fs::write(&path, pretty)?;
            println!("Successfully configured Claude Desktop MCP server in:");
            println!("  {}", path.display());
        } else {
            return Err(anyhow::anyhow!(
                "Could not detect Claude Desktop config directory on this OS."
            ));
        }
        return Ok(Some(()));
    }

    Ok(None)
}

/// Periodic watch loop covering the two triggers with no natural call-site
/// hook: Proposed items aging past a threshold, and Turso→SQLite fallback
/// counter increases. Same cadence as Stage A's embedded-replica sync so the
/// two periodic loops are easy to reason about together.
fn spawn_notify_watch_task(
    store: Arc<Store>,
    code_store: Arc<CodeStore>,
    train_store: Arc<TrainStore>,
    session_store: Arc<SessionStore>,
    notify: Arc<notify::NotifyState>,
) {
    tokio::spawn(async move {
        let mut aging = notify::AgingTracker::default();
        let mut fallback = notify::FallbackTracker::default();
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            let now = chrono::Utc::now().timestamp();

            if let Ok(domains) = store.list_domains() {
                for d in domains {
                    let Ok(rows) = store.list_knowledge(&d.name, Some("proposed"), None) else {
                        continue;
                    };
                    for row in rows {
                        if aging.should_notify(
                            row.id,
                            row.created_at,
                            now,
                            notify.proposed_age_threshold_secs,
                        ) {
                            notify.handle.emit(notify::RelayEvent {
                                trigger: "proposed_item_aging".to_string(),
                                summary: format!(
                                    "Proposed item #{} in '{}' aging past threshold",
                                    row.id, row.domain
                                ),
                                detail: row.title.clone(),
                                count: 1,
                                ts: now,
                            });
                        }
                    }
                }
            }

            for (name, delta) in [
                (
                    "kl-code",
                    fallback.delta("kl-code", code_store.health().fallback_events),
                ),
                (
                    "kl-train",
                    fallback.delta("kl-train", train_store.health().fallback_events),
                ),
                (
                    "kl-session",
                    fallback.delta("kl-session", session_store.health().fallback_events),
                ),
            ] {
                if let Some(delta) = delta {
                    notify.handle.emit(notify::RelayEvent {
                        trigger: "sync_fallback".to_string(),
                        summary: format!("{name} fell back to local-only storage {delta} time(s)"),
                        detail: format!("Turso→SQLite fallback detected for {name}"),
                        count: delta as u32,
                        ts: now,
                    });
                }
            }
        }
    });
}

/// Periodic retention sweep: purges knowledge past its effective retention
/// window (see `Store::retention_sweep`) and session journal rows past
/// `KLAYER_SESSION_RETENTION_DAYS` (see `SessionStore::purge_older_than`).
/// Spawned unconditionally from `main()` — unlike `spawn_notify_watch_task`,
/// retention doesn't depend on notifications being configured. Runs hourly:
/// retention windows are day-granularity, so this doesn't need the
/// notify-watch task's 60-second cadence.
fn spawn_retention_sweep_task(
    store: Arc<Store>,
    session_store: Arc<SessionStore>,
    session_retention_days: Option<i64>,
    run_id: String,
) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60 * 60)).await;

            if let Ok(purged) = store.retention_sweep(&run_id) {
                if purged > 0 {
                    tracing::info!(purged, "retention sweep purged knowledge items");
                }
            } else {
                tracing::warn!("retention sweep over knowledge failed");
            }

            if let Some(days) = session_retention_days {
                match session_store.purge_older_than(days).await {
                    Ok(purged) => {
                        if purged > 0 {
                            store
                                .log_episode_auto(
                                    &run_id,
                                    Some("retention_sweep"),
                                    Some(&format!(
                                        "purge session journal rows older than {days} days"
                                    )),
                                    Some(&format!("purged {purged} row(s)")),
                                    Some("success"),
                                    None,
                                    None,
                                    None,
                                    None,
                                )
                                .ok();
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "session journal retention purge failed"),
                }
            }
        }
    });
}

pub(crate) async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let subcommand = std::env::args().nth(1);
    match subcommand.as_deref() {
        Some("status") => return tui::run_status().await,
        Some("tui") => return tui::run_tui().await,
        _ => {}
    }

    let print_config = std::env::args().any(|a| a == "--print-mcp-config");
    let install_config = std::env::args().any(|a| a == "--install" || a == "--install-mcp");

    if let Some(()) = handle_install_or_print(print_config, install_config)? {
        return Ok(());
    }

    let klayer_dir = get_klayer_dir();
    let DbPaths {
        db,
        code_db,
        train_db,
        session_db,
    } = resolve_db_paths();
    let port: u16 = std::env::var("KLAYER_DASHBOARD_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(7474);

    // libsql performs a process-wide, one-time SQLite threading-mode config on its first
    // connection open; rusqlite (aliased to libsql-rusqlite, see workspace Cargo.toml) shares
    // the same underlying SQLite build, so opening a rusqlite connection first locks in a
    // config libsql's own assertion then rejects. The libsql-backed stores must open first.
    ensure_parent_dir(&code_db)?;
    let code_store = Arc::new(CodeStore::open(&code_db).await?);
    code_store.migrate().await?;
    tracing::info!("klayer code store ready at {code_db}");

    ensure_parent_dir(&train_db)?;
    let train_store = Arc::new(TrainStore::open(&train_db).await?);
    train_store.migrate().await?;
    tracing::info!("klayer train store ready at {train_db}");

    ensure_parent_dir(&session_db)?;
    let session_store = Arc::new(SessionStore::open(&session_db).await?);
    session_store.migrate().await?;
    tracing::info!("klayer session store ready at {session_db}");

    ensure_parent_dir(&db)?;
    let store = Arc::new(Store::open(&db)?);
    store.migrate()?;
    tracing::info!("klayer store ready at {db}");

    let notify_config = notify::NotifyConfig::from_env();
    let notify_state = Arc::new(match &notify_config {
        Some(cfg) => {
            tracing::info!("notification relay enabled");
            notify::NotifyState::from_config(cfg)
        }
        None => notify::NotifyState::disabled(),
    });
    if notify_config.is_some() {
        spawn_notify_watch_task(
            Arc::clone(&store),
            Arc::clone(&code_store),
            Arc::clone(&train_store),
            Arc::clone(&session_store),
            Arc::clone(&notify_state),
        );
    }

    let session_retention_days = std::env::var("KLAYER_SESSION_RETENTION_DAYS")
        .ok()
        .and_then(|v| v.parse().ok());
    let retention_run_id =
        std::env::var("KLAYER_RUN_ID").unwrap_or_else(|_| "retention-sweep".to_string());
    spawn_retention_sweep_task(
        Arc::clone(&store),
        Arc::clone(&session_store),
        session_retention_days,
        retention_run_id,
    );

    let html = load_dashboard_html();
    // Shared with `Klayer` below so the dashboard can reflect the live MCP
    // connection's harness (see `DashState::captured_harness` doc comment).
    let captured_harness = Arc::new(std::sync::Mutex::new(None));

    let server_mode = std::env::args().any(|a| a == "--mode=server");
    let server_auth_token = if server_mode {
        let token = resolve_server_token(&klayer_dir);
        eprintln!("klayer server-mode auth token: {token}  (save this, printed once)");
        print_tls_warning_if_needed();
        Some(Arc::new(token))
    } else {
        None
    };

    let dashboard_only = std::env::args().any(|a| a == "--dashboard");
    if dashboard_only {
        tracing::info!("running in dashboard-only mode (no MCP server)");
        tracing::info!("klayer dashboard  →  http://localhost:{port}");
        eprintln!("\n  klayer dashboard  →  http://localhost:{port}\n  Press Ctrl+C to stop.\n");
        start_dashboard(
            store,
            code_store,
            train_store,
            session_store,
            captured_harness,
            port,
            html,
            server_auth_token,
        )
        .await;
        return Ok(());
    }

    tokio::spawn(start_dashboard(
        Arc::clone(&store),
        Arc::clone(&code_store),
        Arc::clone(&train_store),
        Arc::clone(&session_store),
        Arc::clone(&captured_harness),
        port,
        html,
        server_auth_token,
    ));
    tracing::info!("klayer dashboard  →  http://localhost:{port}");

    let service = Klayer::new(
        store,
        code_store,
        train_store,
        session_store,
        notify_state,
        captured_harness,
    )
    .serve(stdio())
    .await?;
    service.waiting().await?;
    Ok(())
}

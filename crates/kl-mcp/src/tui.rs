//! `klayer status` (one-shot, plain-text) and `klayer tui` (interactive,
//! read-only) — a second frontend over the same stores the dashboard already
//! queries. No new backend/API surface here.

use std::io::Stdout;
use std::sync::Arc;

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use kl_code::CodeStore;
use kl_core::{EpisodeRow, KnowledgeRow, SyncHealthSnapshot};
use kl_session::SessionStore;
use kl_store::Store;
use kl_train::TrainStore;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Frame, Terminal,
};

/// Tally of knowledge items by trust tier, plus the pending-promotion list —
/// pure and unit-testable without a `Store`.
pub struct TrustSummary {
    pub proposed: usize,
    pub reviewed: usize,
    pub user: usize,
    pub proposed_items: Vec<(String, String)>,
}

pub fn summarize_trust(rows: &[KnowledgeRow]) -> TrustSummary {
    let mut s = TrustSummary {
        proposed: 0,
        reviewed: 0,
        user: 0,
        proposed_items: Vec::new(),
    };
    for r in rows {
        match r.trust.as_str() {
            "proposed" => {
                s.proposed += 1;
                s.proposed_items.push((r.domain.clone(), r.title.clone()));
            }
            "reviewed" => s.reviewed += 1,
            "user" => s.user += 1,
            _ => {}
        }
    }
    s
}

pub fn format_episode_line(e: &EpisodeRow) -> String {
    let ts = chrono::DateTime::from_timestamp(e.ts, 0)
        .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| e.ts.to_string());
    format!(
        "{ts}  [{}] {}: {}",
        e.outcome.as_deref().unwrap_or("unknown"),
        e.stage.as_deref().unwrap_or("-"),
        e.action.as_deref().unwrap_or("-"),
    )
}

pub fn format_health_line(
    name: &str,
    engine: &str,
    healthy: bool,
    sync: Option<&SyncHealthSnapshot>,
) -> String {
    let status = if healthy { "healthy" } else { "UNHEALTHY" };
    match sync {
        Some(s) => format!(
            "{name:<10} ({engine})  {status}  remote_configured={}  last_success={}  consecutive_failures={}  fallback_events={}",
            s.remote_configured,
            s.last_success_at
                .map(|t| t.to_string())
                .unwrap_or_else(|| "never".into()),
            s.consecutive_failures,
            s.fallback_events,
        ),
        None => format!("{name:<10} ({engine})  {status}"),
    }
}

/// Compose the full plain-text `klayer status` report. Pure, so it's tested
/// against synthetic data rather than requiring a live DB.
pub fn build_status_report(
    trust: &TrustSummary,
    episodes: &[EpisodeRow],
    health_lines: &[String],
) -> String {
    let mut out = String::new();
    out.push_str("klayer status\n");
    out.push_str("=============\n\n");

    out.push_str("Trust Lifecycle\n");
    out.push_str(&format!(
        "  proposed: {}   reviewed: {}   user: {}\n",
        trust.proposed, trust.reviewed, trust.user
    ));
    if trust.proposed_items.is_empty() {
        out.push_str("  Pending promotion: none\n");
    } else {
        out.push_str("  Pending promotion (proposed):\n");
        for (domain, title) in &trust.proposed_items {
            out.push_str(&format!("    - [{domain}] {title}\n"));
        }
    }
    out.push('\n');

    out.push_str("Episode Log (recent)\n");
    if episodes.is_empty() {
        out.push_str("  no episodes recorded\n");
    } else {
        for e in episodes {
            out.push_str(&format!("  {}\n", format_episode_line(e)));
        }
    }
    out.push('\n');

    out.push_str("Storage Health\n");
    for line in health_lines {
        out.push_str(&format!("  {line}\n"));
    }
    out
}

/// Open all four stores using the same DB-path-resolution and open/migrate
/// order as `main()` (libsql-backed stores must open before the rusqlite-
/// backed `Store` — see `main()`'s comment on the process-wide SQLite config).
async fn open_stores() -> Result<(
    Arc<Store>,
    Arc<CodeStore>,
    Arc<TrainStore>,
    Arc<SessionStore>,
)> {
    let paths = crate::resolve_db_paths();

    crate::ensure_parent_dir(&paths.code_db)?;
    let code_store = Arc::new(CodeStore::open(&paths.code_db).await?);
    code_store.migrate().await?;

    crate::ensure_parent_dir(&paths.train_db)?;
    let train_store = Arc::new(TrainStore::open(&paths.train_db).await?);
    train_store.migrate().await?;

    crate::ensure_parent_dir(&paths.session_db)?;
    let session_store = Arc::new(SessionStore::open(&paths.session_db).await?);
    session_store.migrate().await?;

    crate::ensure_parent_dir(&paths.db)?;
    let store = Arc::new(Store::open(&paths.db)?);
    store.migrate()?;

    Ok((store, code_store, train_store, session_store))
}

async fn collect_trust_summary(store: &Store) -> Result<TrustSummary> {
    let domains = store.list_domains()?;
    let mut all = Vec::new();
    for d in &domains {
        all.extend(store.list_knowledge(&d.name, None, None)?);
    }
    Ok(summarize_trust(&all))
}

async fn collect_health_lines(
    store: &Store,
    code_store: &CodeStore,
    train_store: &TrainStore,
    session_store: &SessionStore,
) -> Vec<String> {
    vec![
        format_health_line("kl_store", "sqlite", store.list_domains().is_ok(), None),
        format_health_line(
            "kl_code",
            "libsql",
            code_store.stats().await.is_ok(),
            Some(&code_store.health()),
        ),
        format_health_line(
            "kl_train",
            "libsql",
            train_store.stats().await.is_ok(),
            Some(&train_store.health()),
        ),
        format_health_line(
            "kl_session",
            "libsql",
            session_store.list_journal(None).await.is_ok(),
            Some(&session_store.health()),
        ),
    ]
}

/// `klayer status` — one-shot, non-interactive: prints a plain-text summary
/// to stdout and exits. No TTY assumptions, safe over a bare SSH session.
pub async fn run_status() -> Result<()> {
    let (store, code_store, train_store, session_store) = open_stores().await?;

    let trust = collect_trust_summary(&store).await?;
    let episodes: Vec<EpisodeRow> = store.list_episodes(None)?.into_iter().take(5).collect();
    let health_lines =
        collect_health_lines(&store, &code_store, &train_store, &session_store).await;

    println!("{}", build_status_report(&trust, &episodes, &health_lines));
    Ok(())
}

fn init_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal() -> Result<()> {
    disable_raw_mode()?;
    execute!(std::io::stdout(), LeaveAlternateScreen)?;
    Ok(())
}

/// Installs a panic hook that restores the terminal (raw mode off, leave
/// alternate screen) before re-panicking — otherwise a panic mid-draw leaves
/// the user's shell in raw/alternate-screen mode with no visible prompt.
fn install_panic_hook() {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = restore_terminal();
        original_hook(panic_info);
    }));
}

fn draw(
    f: &mut Frame,
    trust: &TrustSummary,
    episodes: &[EpisodeRow],
    health_lines: &[String],
    scroll: usize,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(5),
            Constraint::Length(health_lines.len() as u16 + 2),
            Constraint::Length(1),
        ])
        .split(f.area());

    let mut trust_text = format!(
        "proposed: {}   reviewed: {}   user: {}",
        trust.proposed, trust.reviewed, trust.user
    );
    if !trust.proposed_items.is_empty() {
        let preview = trust
            .proposed_items
            .iter()
            .take(3)
            .map(|(d, t)| format!("[{d}] {t}"))
            .collect::<Vec<_>>()
            .join("; ");
        trust_text.push_str(&format!("\npending: {preview}"));
    }
    f.render_widget(
        Paragraph::new(trust_text).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Trust Lifecycle"),
        ),
        chunks[0],
    );

    let items: Vec<ListItem> = episodes
        .iter()
        .skip(scroll)
        .map(|e| ListItem::new(format_episode_line(e)))
        .collect();
    f.render_widget(
        List::new(items).block(Block::default().borders(Borders::ALL).title("Episode Log")),
        chunks[1],
    );

    f.render_widget(
        Paragraph::new(health_lines.join("\n")).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Storage Health"),
        ),
        chunks[2],
    );

    f.render_widget(
        Paragraph::new("q / Ctrl+C: quit   Up/Down: scroll episode log"),
        chunks[3],
    );
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    trust: &TrustSummary,
    episodes: &[EpisodeRow],
    health_lines: &[String],
) -> Result<()> {
    let mut scroll: usize = 0;
    loop {
        terminal.draw(|f| draw(f, trust, episodes, health_lines, scroll))?;

        if event::poll(std::time::Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                    KeyCode::Down => {
                        if scroll + 1 < episodes.len() {
                            scroll += 1;
                        }
                    }
                    KeyCode::Up => scroll = scroll.saturating_sub(1),
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

/// `klayer tui` — interactive, read-only: Trust Lifecycle, Episode Log
/// (scrollable), and Storage Health, all in one screen. No keybindings for
/// promote/resolve/delete — only scroll and quit.
pub async fn run_tui() -> Result<()> {
    let (store, code_store, train_store, session_store) = open_stores().await?;

    let trust = collect_trust_summary(&store).await?;
    let episodes: Vec<EpisodeRow> = store.list_episodes(None)?.into_iter().take(50).collect();
    let health_lines =
        collect_health_lines(&store, &code_store, &train_store, &session_store).await;

    install_panic_hook();
    let mut terminal = init_terminal()?;
    let result = event_loop(&mut terminal, &trust, &episodes, &health_lines);
    restore_terminal()?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn knowledge_row(trust: &str, domain: &str, title: &str) -> KnowledgeRow {
        KnowledgeRow {
            id: 1,
            kind: "fact".into(),
            domain: domain.into(),
            stage: None,
            title: title.into(),
            body: "body".into(),
            trust: trust.into(),
            enforceable: false,
            severity: None,
            created_at: 0,
            updated_at: 0,
            conflict_with_id: None,
            conflict_status: None,
            retention_days: None,
        }
    }

    #[test]
    fn summarize_trust_counts_and_collects_proposed() {
        let rows = vec![
            knowledge_row("proposed", "secure-coding", "Use parameterized queries"),
            knowledge_row("reviewed", "secure-coding", "Validate input"),
            knowledge_row("user", "secure-coding", "Team convention"),
            knowledge_row("proposed", "other", "Another proposal"),
        ];
        let s = summarize_trust(&rows);
        assert_eq!(s.proposed, 2);
        assert_eq!(s.reviewed, 1);
        assert_eq!(s.user, 1);
        assert_eq!(
            s.proposed_items,
            vec![
                (
                    "secure-coding".to_string(),
                    "Use parameterized queries".to_string()
                ),
                ("other".to_string(), "Another proposal".to_string()),
            ]
        );
    }

    #[test]
    fn format_health_line_reports_unhealthy_without_sync() {
        let line = format_health_line("kl_store", "sqlite", false, None);
        assert!(line.contains("UNHEALTHY"));
        assert!(line.contains("kl_store"));
    }

    #[test]
    fn format_health_line_includes_sync_snapshot_fields() {
        let snap = SyncHealthSnapshot {
            remote_configured: true,
            last_success_at: Some(1_700_000_000),
            consecutive_failures: 2,
            fallback_events: 5,
        };
        let line = format_health_line("kl_code", "libsql", true, Some(&snap));
        assert!(line.contains("healthy"));
        assert!(line.contains("remote_configured=true"));
        assert!(line.contains("consecutive_failures=2"));
        assert!(line.contains("fallback_events=5"));
    }

    #[test]
    fn build_status_report_lists_no_episodes_message_when_empty() {
        let trust = TrustSummary {
            proposed: 0,
            reviewed: 0,
            user: 0,
            proposed_items: Vec::new(),
        };
        let report = build_status_report(&trust, &[], &["kl_store (sqlite)  healthy".into()]);
        assert!(report.contains("Pending promotion: none"));
        assert!(report.contains("no episodes recorded"));
        assert!(report.contains("kl_store (sqlite)  healthy"));
    }
}

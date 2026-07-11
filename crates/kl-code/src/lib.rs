//! kl-code — persistent codebase memory store.
//!
//! Indexes local directories into a dedicated libsql DB (`KLAYER_CODE_DB`,
//! default `klayer_code.db`) with FTS5 over code chunks, so the LLM can
//! recall any function, struct, or pattern across sessions. When
//! `KLAYER_TURSO_URL`/`KLAYER_TURSO_TOKEN` are set, the DB is opened as an
//! embedded replica that periodically syncs against Turso; otherwise it is a
//! pure local file, identical in behavior to the old rusqlite store.
//!
//! Intentionally separate from `klayer.db` (knowledge/episodes).

use anyhow::{Context, Result};
use chrono::Utc;
use kl_core::{SyncHealth, SyncHealthSnapshot};
use libsql::{params, Connection, Database};
use serde::Serialize;
use std::path::Path;
use std::sync::Arc;

const CHUNK_LINES: usize = 80;
const MAX_FILE_BYTES: u64 = 512 * 1024;

const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "__pycache__",
    ".venv",
    "venv",
    "env",
    "dist",
    "build",
    "vendor",
    ".next",
    ".nuxt",
    "coverage",
    ".nyc_output",
    "out",
    ".cache",
    ".parcel-cache",
    ".turbo",
    ".svelte-kit",
];

const SCHEMA: &str = "
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS repos (
    id          INTEGER PRIMARY KEY,
    path        TEXT    NOT NULL UNIQUE,
    name        TEXT,
    indexed_at  INTEGER,
    file_count  INTEGER NOT NULL DEFAULT 0,
    chunk_count INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS code_files (
    id       INTEGER PRIMARY KEY,
    repo_id  INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    rel_path TEXT    NOT NULL,
    language TEXT,
    UNIQUE(repo_id, rel_path)
);

CREATE TABLE IF NOT EXISTS code_chunks (
    id          INTEGER PRIMARY KEY,
    file_id     INTEGER NOT NULL REFERENCES code_files(id) ON DELETE CASCADE,
    line_start  INTEGER NOT NULL DEFAULT 1,
    line_end    INTEGER NOT NULL DEFAULT 1,
    content     TEXT    NOT NULL,
    kind        TEXT,
    symbol_name TEXT
);

-- Plain FTS5 (no content=): owns its data so standard DELETE works.
CREATE VIRTUAL TABLE IF NOT EXISTS code_fts USING fts5(body);
";

// ── Public types ─────────────────────────────────────────────────────────────

pub struct CodeStore {
    conn: Connection,
    #[allow(dead_code)] // kept alive so the background sync task's Arc clone isn't orphaned
    db: Arc<Database>,
    health: Arc<SyncHealth>,
    remote_configured: bool,
}

#[derive(Debug, Serialize, Clone)]
pub struct RepoInfo {
    pub id: i64,
    pub path: String,
    pub name: Option<String>,
    pub indexed_at: Option<i64>,
    pub file_count: i64,
    pub chunk_count: i64,
}

#[derive(Debug, Serialize)]
pub struct CodeHit {
    pub repo_path: String,
    pub file_path: String,
    pub language: Option<String>,
    pub line_start: i64,
    pub line_end: i64,
    pub kind: Option<String>,
    pub symbol_name: Option<String>,
    pub snippet: String,
    pub score: f64,
}

#[derive(Debug, Serialize)]
pub struct CodeStats {
    pub repos: i64,
    pub files: i64,
    pub chunks: i64,
}

#[derive(Debug, Serialize)]
pub struct IndexStats {
    pub files: usize,
    pub chunks: usize,
    pub skipped: usize,
    pub skip_reasons: Vec<String>,
    pub warnings: Vec<String>,
}

// ── CodeStore impl ────────────────────────────────────────────────────────────

impl CodeStore {
    pub async fn open(path: &str) -> Result<Self> {
        let db = kl_core::open_db(path)
            .await
            .with_context(|| format!("opening code db at {path}"))?;
        let db = Arc::new(db);
        let conn = db.connect().context("opening code db connection")?;
        let remote_configured = kl_core::turso_config().is_some();
        let health = SyncHealth::new();
        kl_core::spawn_sync_task(Arc::clone(&db), Arc::clone(&health));
        Ok(Self {
            conn,
            db,
            health,
            remote_configured,
        })
    }

    pub async fn migrate(&self) -> Result<()> {
        self.conn
            .execute_batch(SCHEMA)
            .await
            .context("code db schema")?;
        Ok(())
    }

    pub fn health(&self) -> SyncHealthSnapshot {
        self.health.snapshot(self.remote_configured)
    }

    pub async fn stats(&self) -> Result<CodeStats> {
        let repos = scalar_i64(&self.conn, "SELECT COUNT(*) FROM repos").await?;
        let files = scalar_i64(&self.conn, "SELECT COUNT(*) FROM code_files").await?;
        let chunks = scalar_i64(&self.conn, "SELECT COUNT(*) FROM code_chunks").await?;
        Ok(CodeStats {
            repos,
            files,
            chunks,
        })
    }

    /// Same shape as `stats()`, scoped to a single repo (matched by canonical
    /// path or friendly name, same lookup `search`'s `repo_path` filter
    /// uses). Needed because `stats()` aggregates across every indexed repo,
    /// which is the wrong signal once more than one repo is indexed and a
    /// caller wants complexity scoped to just one of them. Returns all-zero
    /// stats (not an error) for an unknown repo, matching `forget_repo`'s
    /// "not found is a normal outcome" convention.
    pub async fn stats_for_repo(&self, repo: &str) -> Result<CodeStats> {
        let repo_id: Option<i64> = {
            let mut rows = self
                .conn
                .query(
                    "SELECT id FROM repos WHERE path = ?1 OR name = ?1",
                    params![repo],
                )
                .await?;
            match rows.next().await? {
                Some(r) => Some(r.get(0)?),
                None => None,
            }
        };
        let Some(repo_id) = repo_id else {
            return Ok(CodeStats {
                repos: 0,
                files: 0,
                chunks: 0,
            });
        };
        let files = scalar_i64_params(
            &self.conn,
            "SELECT COUNT(*) FROM code_files WHERE repo_id = ?1",
            params![repo_id],
        )
        .await?;
        let chunks = scalar_i64_params(
            &self.conn,
            "SELECT COUNT(*) FROM code_chunks cc JOIN code_files cf ON cf.id = cc.file_id WHERE cf.repo_id = ?1",
            params![repo_id],
        )
        .await?;
        Ok(CodeStats {
            repos: 1,
            files,
            chunks,
        })
    }

    pub async fn list_repos(&self) -> Result<Vec<RepoInfo>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id, path, name, indexed_at, file_count, chunk_count
                 FROM repos ORDER BY indexed_at DESC",
                (),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().await? {
            out.push(RepoInfo {
                id: r.get(0)?,
                path: r.get(1)?,
                name: r.get(2)?,
                indexed_at: r.get(3)?,
                file_count: r.get(4)?,
                chunk_count: r.get(5)?,
            });
        }
        Ok(out)
    }

    pub async fn forget_repo(&self, path: &str) -> Result<bool> {
        let repo_id: Option<i64> = {
            let mut rows = self
                .conn
                .query("SELECT id FROM repos WHERE path = ?1", params![path])
                .await?;
            match rows.next().await? {
                Some(r) => Some(r.get(0)?),
                None => None,
            }
        };
        let Some(repo_id) = repo_id else {
            return Ok(false);
        };

        // Collect chunk IDs before cascade-deleting (FTS5 must be cleaned first).
        let mut chunk_ids: Vec<i64> = Vec::new();
        let mut rows = self
            .conn
            .query(
                "SELECT cc.id FROM code_chunks cc
                 JOIN code_files cf ON cf.id = cc.file_id
                 WHERE cf.repo_id = ?1",
                params![repo_id],
            )
            .await?;
        while let Some(r) = rows.next().await? {
            chunk_ids.push(r.get(0)?);
        }

        // Remove from FTS5 then cascade-delete from repos
        for id in &chunk_ids {
            self.conn
                .execute("DELETE FROM code_fts WHERE rowid = ?1", params![*id])
                .await?;
        }
        self.conn
            .execute("DELETE FROM repos WHERE id = ?1", params![repo_id])
            .await?;
        Ok(true)
    }

    pub async fn clear_all(&self) -> Result<u64> {
        self.conn.execute("DELETE FROM code_fts", ()).await?;
        let deleted = self.conn.execute("DELETE FROM repos", ()).await?;
        Ok(deleted)
    }

    pub async fn search(
        &self,
        query: &str,
        repo_path: Option<&str>,
        limit: usize,
    ) -> Result<Vec<CodeHit>> {
        if query.trim().is_empty() {
            return Ok(vec![]);
        }
        let match_expr = fts_match(query);
        let lim = limit as i64;
        let mut hits = Vec::new();

        let mut rows = if let Some(rp) = repo_path {
            self.conn
                .query(
                    "SELECT cc.content, cc.kind, cc.symbol_name,
                            cc.line_start, cc.line_end,
                            cf.rel_path, cf.language, r.path,
                            bm25(code_fts) AS score
                     FROM code_fts
                     JOIN code_chunks cc ON cc.id = code_fts.rowid
                     JOIN code_files  cf ON cf.id = cc.file_id
                     JOIN repos        r ON  r.id = cf.repo_id
                     WHERE code_fts MATCH ?1 AND r.path = ?2
                     ORDER BY score ASC LIMIT ?3",
                    params![match_expr, rp, lim],
                )
                .await?
        } else {
            self.conn
                .query(
                    "SELECT cc.content, cc.kind, cc.symbol_name,
                            cc.line_start, cc.line_end,
                            cf.rel_path, cf.language, r.path,
                            bm25(code_fts) AS score
                     FROM code_fts
                     JOIN code_chunks cc ON cc.id = code_fts.rowid
                     JOIN code_files  cf ON cf.id = cc.file_id
                     JOIN repos        r ON  r.id = cf.repo_id
                     WHERE code_fts MATCH ?1
                     ORDER BY score ASC LIMIT ?2",
                    params![match_expr, lim],
                )
                .await?
        };

        while let Some(r) = rows.next().await? {
            hits.push(CodeHit {
                snippet: r.get(0)?,
                kind: r.get(1)?,
                symbol_name: r.get(2)?,
                line_start: r.get(3)?,
                line_end: r.get(4)?,
                file_path: r.get(5)?,
                language: r.get(6)?,
                repo_path: r.get(7)?,
                score: r.get(8)?,
            });
        }

        Ok(hits)
    }

    pub async fn index_repo(&self, dir_path: &str, name: Option<&str>) -> Result<IndexStats> {
        let canonical = std::fs::canonicalize(dir_path)
            .with_context(|| format!("resolving path: {dir_path}"))?;
        let canon_str = canonical.to_string_lossy().replace('\\', "/");
        let now = Utc::now().timestamp();

        // Collect files outside any DB call (blocking I/O, may be large).
        let mut file_data: Vec<FileEntry> = Vec::new();
        let mut skip_reasons = Vec::new();
        collect_files(&canonical, &canonical, &mut file_data, &mut skip_reasons)?;
        let skipped = skip_reasons.len();
        let mut warnings = Vec::new();
        for repo in self.list_repos().await? {
            let existing = Path::new(&repo.path);
            if existing != canonical
                && (canonical.starts_with(existing) || existing.starts_with(&canonical))
            {
                warnings.push(format!(
                    "overlapping repository index detected: '{}' and '{}' are stored separately",
                    canon_str, repo.path
                ));
            }
        }

        let total_files = file_data.len();
        let total_chunks: usize = file_data.iter().map(|f| f.chunks.len()).sum();

        tracing::info!(
            "code-index: {} files, {} chunks, {} skipped — writing to DB",
            total_files,
            total_chunks,
            skipped
        );

        // Upsert repo record.
        self.conn
            .execute(
                "INSERT INTO repos (path, name, indexed_at, file_count, chunk_count)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(path) DO UPDATE SET
                     name        = COALESCE(?2, name),
                     indexed_at  = ?3,
                     file_count  = ?4,
                     chunk_count = ?5",
                params![
                    canon_str.clone(),
                    name,
                    now,
                    total_files as i64,
                    total_chunks as i64
                ],
            )
            .await?;
        let repo_id: i64 = {
            let mut rows = self
                .conn
                .query(
                    "SELECT id FROM repos WHERE path = ?1",
                    params![canon_str.clone()],
                )
                .await?;
            rows.next()
                .await?
                .context("repo row missing after upsert")?
                .get(0)?
        };

        // Collect old chunk IDs before cascade-deleting (FTS cleanup required).
        let mut old_chunk_ids: Vec<i64> = Vec::new();
        let mut rows = self
            .conn
            .query(
                "SELECT cc.id FROM code_chunks cc
                 JOIN code_files cf ON cf.id = cc.file_id
                 WHERE cf.repo_id = ?1",
                params![repo_id],
            )
            .await?;
        while let Some(r) = rows.next().await? {
            old_chunk_ids.push(r.get(0)?);
        }

        // Clean FTS5 for old entries, then delete old files (cascade → chunks).
        for id in &old_chunk_ids {
            self.conn
                .execute("DELETE FROM code_fts WHERE rowid = ?1", params![*id])
                .await?;
        }
        self.conn
            .execute(
                "DELETE FROM code_files WHERE repo_id = ?1",
                params![repo_id],
            )
            .await?;

        // Insert new files and chunks in one transaction.
        insert_repo_data(&self.conn, repo_id, &file_data).await?;

        Ok(IndexStats {
            files: total_files,
            chunks: total_chunks,
            skipped,
            skip_reasons,
            warnings,
        })
    }
}

async fn scalar_i64(conn: &Connection, sql: &str) -> Result<i64> {
    let mut rows = conn.query(sql, ()).await?;
    Ok(rows
        .next()
        .await?
        .map(|r| r.get(0))
        .transpose()?
        .unwrap_or(0))
}

async fn scalar_i64_params(
    conn: &Connection,
    sql: &str,
    p: impl libsql::params::IntoParams,
) -> Result<i64> {
    let mut rows = conn.query(sql, p).await?;
    Ok(rows
        .next()
        .await?
        .map(|r| r.get(0))
        .transpose()?
        .unwrap_or(0))
}

async fn insert_repo_data(conn: &Connection, repo_id: i64, file_data: &[FileEntry]) -> Result<()> {
    let tx = conn.transaction().await?;
    for entry in file_data {
        tx.execute(
            "INSERT INTO code_files (repo_id, rel_path, language) VALUES (?1, ?2, ?3)",
            params![repo_id, entry.rel_path.clone(), entry.language.clone()],
        )
        .await?;
        let file_id = tx.last_insert_rowid();

        for chunk in &entry.chunks {
            tx.execute(
                "INSERT INTO code_chunks
                     (file_id, line_start, line_end, content, kind, symbol_name)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    file_id,
                    chunk.line_start as i64,
                    chunk.line_end as i64,
                    chunk.content.clone(),
                    chunk.kind.clone(),
                    chunk.symbol_name.clone()
                ],
            )
            .await?;
            let chunk_id = tx.last_insert_rowid();

            let body = format!(
                "{} {}\n{}",
                chunk.symbol_name.as_deref().unwrap_or(""),
                entry.rel_path,
                chunk.content
            );
            tx.execute(
                "INSERT INTO code_fts (rowid, body) VALUES (?1, ?2)",
                params![chunk_id, body],
            )
            .await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

// ── FTS helper ────────────────────────────────────────────────────────────────

fn fts_match(query: &str) -> String {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|t| t.replace('"', ""))
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{t}\""))
        .collect();
    if terms.is_empty() {
        return String::new();
    }
    terms.join(" OR ")
}

// ── File collection ──────────────────────────────────────────────────────────

struct FileEntry {
    rel_path: String,
    language: String,
    chunks: Vec<ChunkEntry>,
}

struct ChunkEntry {
    line_start: usize,
    line_end: usize,
    content: String,
    kind: Option<String>,
    symbol_name: Option<String>,
}

fn process_file_entry(
    root: &Path,
    path: &Path,
    lang: &str,
    out: &mut Vec<FileEntry>,
    skipped: &mut Vec<String>,
) -> Result<()> {
    let rel_str = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > MAX_FILE_BYTES {
            skipped.push(format!("{rel_str}: exceeds {MAX_FILE_BYTES} byte limit"));
            return Ok(());
        }
    }

    let Ok(bytes) = std::fs::read(path) else {
        skipped.push(format!("{rel_str}: unreadable"));
        return Ok(());
    };
    if bytes.iter().take(8192).any(|b| *b == 0) {
        skipped.push(format!("{rel_str}: binary content"));
        return Ok(());
    }
    let Ok(content) = String::from_utf8(bytes) else {
        skipped.push(format!("{rel_str}: not valid UTF-8 text"));
        return Ok(());
    };
    if content.trim().is_empty() {
        return Ok(());
    }

    let chunks = chunk_file(&content, lang);
    if !chunks.is_empty() {
        out.push(FileEntry {
            rel_path: rel_str,
            language: lang.to_string(),
            chunks,
        });
    }
    Ok(())
}

fn collect_files(
    root: &Path,
    dir: &Path,
    out: &mut Vec<FileEntry>,
    skipped: &mut Vec<String>,
) -> Result<()> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => {
            skipped.push(format!("{}: directory unreadable", dir.display()));
            return Ok(());
        }
    };

    for entry in rd.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let Ok(ft) = entry.file_type() else { continue };

        if ft.is_dir() {
            if name_str.starts_with('.') || SKIP_DIRS.contains(&name_str.as_ref()) {
                continue;
            }
            collect_files(root, &entry.path(), out, skipped)?;
            continue;
        }

        if !ft.is_file() {
            continue;
        }

        let path = entry.path();
        let lang = detect_language(&path);
        process_file_entry(root, &path, lang, out, skipped)?;
    }

    Ok(())
}

fn detect_language(path: &Path) -> &'static str {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    const EXT_MAP: &[(&str, &str)] = &[
        ("rs", "rust"),
        ("py", "python"),
        ("js", "javascript"),
        ("mjs", "javascript"),
        ("cjs", "javascript"),
        ("ts", "typescript"),
        ("mts", "typescript"),
        ("tsx", "tsx"),
        ("jsx", "jsx"),
        ("go", "go"),
        ("java", "java"),
        ("cpp", "cpp"),
        ("cc", "cpp"),
        ("cxx", "cpp"),
        ("c", "c"),
        ("h", "cpp"),
        ("hpp", "cpp"),
        ("cs", "csharp"),
        ("rb", "ruby"),
        ("kt", "kotlin"),
        ("kts", "kotlin"),
        ("swift", "swift"),
        ("md", "markdown"),
        ("markdown", "markdown"),
        ("toml", "toml"),
        ("json", "json"),
        ("yaml", "yaml"),
        ("yml", "yaml"),
        ("html", "html"),
        ("htm", "html"),
        ("css", "css"),
        ("scss", "css"),
        ("sass", "css"),
        ("sh", "shell"),
        ("bash", "shell"),
        ("sql", "sql"),
        ("php", "php"),
        ("lua", "lua"),
        ("zig", "zig"),
        ("cbl", "cobol"),
        ("cob", "cobol"),
        ("cpy", "cobol"),
        ("cobol", "cobol"),
        ("nsp", "natural"),
        ("nse", "natural"),
        ("nsd", "natural"),
        ("nsl", "natural"),
        ("nst", "natural"),
        ("rpg", "rpg"),
        ("rpgle", "rpg"),
        ("sqlrpgle", "rpg"),
        ("sru", "powerscript"),
        ("sra", "powerscript"),
        ("srd", "powerscript"),
        ("srw", "powerscript"),
        ("pbl", "powerscript"),
    ];

    EXT_MAP
        .iter()
        .find(|&&(e, _)| e.eq_ignore_ascii_case(ext))
        .map(|&(_, lang)| lang)
        .unwrap_or("text")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unknown_extensions_fall_back_to_text_and_binary_skips_are_explained() {
        let root = std::env::temp_dir().join(format!("klayer-index-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("PROGRAM.CBLX"),
            "IDENTIFICATION DIVISION.\nPROGRAM-ID. HELLO.",
        )
        .unwrap();
        std::fs::write(root.join("artifact.bin"), b"abc\0def").unwrap();
        let db = root.join("code.db");
        let store = CodeStore::open(db.to_str().unwrap()).await.unwrap();
        store.migrate().await.unwrap();
        let stats = store
            .index_repo(root.to_str().unwrap(), Some("test"))
            .await
            .unwrap();
        assert!(stats.files >= 1);
        assert!(stats
            .skip_reasons
            .iter()
            .any(|r| r.contains("artifact.bin: binary content")));
        let hits = store.search("IDENTIFICATION", None, 5).await.unwrap();
        assert!(hits.iter().any(|h| h.file_path == "PROGRAM.CBLX"));
        drop(store);
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn stats_for_repo_scopes_to_one_repo_among_several() {
        let root = std::env::temp_dir().join(format!("klayer-stats-test-{}", std::process::id()));
        let repo_a = root.join("repo_a");
        let repo_b = root.join("repo_b");
        std::fs::create_dir_all(&repo_a).unwrap();
        std::fs::create_dir_all(&repo_b).unwrap();
        std::fs::write(repo_a.join("one.rs"), "fn one() {}\nfn two() {}\n").unwrap();
        std::fs::write(repo_b.join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(repo_b.join("b.rs"), "fn b() {}\n").unwrap();
        std::fs::write(repo_b.join("c.rs"), "fn c() {}\n").unwrap();

        let db = root.join("code.db");
        let store = CodeStore::open(db.to_str().unwrap()).await.unwrap();
        store.migrate().await.unwrap();
        store
            .index_repo(repo_a.to_str().unwrap(), Some("repo_a"))
            .await
            .unwrap();
        store
            .index_repo(repo_b.to_str().unwrap(), Some("repo_b"))
            .await
            .unwrap();

        let global = store.stats().await.unwrap();
        assert_eq!(global.files, 4);

        let scoped_a = store.stats_for_repo("repo_a").await.unwrap();
        assert_eq!(scoped_a.files, 1);
        let scoped_b = store.stats_for_repo("repo_b").await.unwrap();
        assert_eq!(scoped_b.files, 3);
        assert_ne!(scoped_a.files, global.files);

        let unknown = store.stats_for_repo("does-not-exist").await.unwrap();
        assert_eq!(unknown.files, 0);
        assert_eq!(unknown.repos, 0);

        drop(store);
        std::fs::remove_dir_all(root).ok();
    }
}

fn chunk_file(content: &str, lang: &str) -> Vec<ChunkEntry> {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return vec![];
    }

    let mut chunks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let end = (i + CHUNK_LINES).min(lines.len());
        let slice = &lines[i..end];
        let (kind, symbol_name) = detect_symbol(lang, slice);
        chunks.push(ChunkEntry {
            line_start: i + 1,
            line_end: end,
            content: slice.join("\n"),
            kind,
            symbol_name,
        });
        i = end;
    }
    chunks
}

fn detect_symbol(lang: &str, lines: &[&str]) -> (Option<String>, Option<String>) {
    for line in lines.iter().take(8) {
        if let Some((k, n)) = parse_symbol(lang, line) {
            return (Some(k), Some(n));
        }
    }
    (None, None)
}

fn parse_rust_symbol(s: &str) -> Option<(String, String)> {
    for (prefix, kind) in [
        ("pub async fn ", "fn"),
        ("pub fn ", "fn"),
        ("async fn ", "fn"),
        ("fn ", "fn"),
        ("pub struct ", "struct"),
        ("struct ", "struct"),
        ("pub enum ", "enum"),
        ("enum ", "enum"),
        ("pub trait ", "trait"),
        ("trait ", "trait"),
        ("pub type ", "type"),
        ("type ", "type"),
    ] {
        if let Some(rest) = s.strip_prefix(prefix) {
            let name = rest.split(['(', '<', '{', ' ', '\n']).next()?.to_string();
            if valid_ident(&name) {
                return Some((kind.into(), name));
            }
        }
    }
    for prefix in ["pub impl ", "impl "] {
        if let Some(rest) = s.strip_prefix(prefix) {
            let part = rest.split_once(" for ").map(|(_, b)| b).unwrap_or(rest);
            let name = part.split(['<', '{', ' ']).next()?.to_string();
            if valid_ident(&name) {
                return Some(("impl".into(), name));
            }
        }
    }
    None
}

fn parse_python_symbol(s: &str) -> Option<(String, String)> {
    for (prefix, kind) in [("async def ", "fn"), ("def ", "fn"), ("class ", "class")] {
        if let Some(rest) = s.strip_prefix(prefix) {
            let name = rest.split(['(', ':', ' ']).next()?.to_string();
            if valid_ident(&name) {
                return Some((kind.into(), name));
            }
        }
    }
    None
}

fn parse_js_ts_symbol(s: &str) -> Option<(String, String)> {
    for (prefix, kind) in [
        ("export default async function ", "fn"),
        ("export async function ", "fn"),
        ("export function ", "fn"),
        ("async function ", "fn"),
        ("function ", "fn"),
        ("export default class ", "class"),
        ("export abstract class ", "class"),
        ("export class ", "class"),
        ("abstract class ", "class"),
        ("class ", "class"),
    ] {
        if let Some(rest) = s.strip_prefix(prefix) {
            let name = rest.split(['(', '<', '{', ' ']).next()?.to_string();
            if valid_ident(&name) {
                return Some((kind.into(), name));
            }
        }
    }
    for prefix in ["export const ", "const "] {
        if let Some(rest) = s.strip_prefix(prefix) {
            if rest.contains("=>") || rest.contains("= function") {
                let name = rest.split(['=', ':', ' ']).next()?.to_string();
                if valid_ident(&name) {
                    return Some(("const".into(), name));
                }
            }
        }
    }
    None
}

fn parse_go_symbol(s: &str) -> Option<(String, String)> {
    if let Some(rest) = s.strip_prefix("func ") {
        let rest = if rest.starts_with('(') {
            rest.splitn(2, ')').nth(1)?.trim_start_matches([' ', '\t'])
        } else {
            rest
        };
        let name = rest.split(['(', ' ']).next()?.to_string();
        if valid_ident(&name) {
            return Some(("fn".into(), name));
        }
    }
    if let Some(rest) = s.strip_prefix("type ") {
        let name = rest.split([' ', '[']).next()?.to_string();
        if valid_ident(&name) {
            return Some(("type".into(), name));
        }
    }
    None
}

fn parse_jvm_dotnet_symbol(s: &str) -> Option<(String, String)> {
    if s.contains('(')
        && !s.starts_with("if ")
        && !s.starts_with("for ")
        && !s.starts_with("while ")
    {
        let before = s.split('(').next()?;
        let name = before.split_whitespace().last()?.to_string();
        if valid_ident(&name) && name.len() > 1 {
            return Some(("method".into(), name));
        }
    }
    None
}

fn parse_cobol_symbol(s: &str) -> Option<(String, String)> {
    let su = s.to_uppercase();
    for (suffix, kind) in [(" DIVISION.", "division"), (" SECTION.", "section")] {
        if su.ends_with(suffix) {
            let name = su[..su.len() - suffix.len()]
                .split_whitespace()
                .last()?
                .to_string();
            if !name.is_empty() {
                return Some((kind.into(), name));
            }
        }
    }
    if su.ends_with('.') && !su.contains(' ') && su.len() > 1 {
        let name = su.trim_end_matches('.').to_string();
        if !name.is_empty() && name.len() <= 64 {
            return Some(("paragraph".into(), name));
        }
    }
    for prefix in ["PERFORM ", "CALL "] {
        if let Some(rest) = su.strip_prefix(prefix) {
            let name = rest
                .split_whitespace()
                .next()?
                .trim_end_matches(".")
                .to_string();
            if !name.is_empty() && name.len() <= 64 {
                return Some(("call".into(), name));
            }
        }
    }
    None
}

fn parse_natural_symbol(s: &str) -> Option<(String, String)> {
    let su = s.to_uppercase();
    for (prefix, kind) in [
        ("DEFINE SUBROUTINE ", "subroutine"),
        ("DEFINE FUNCTION ", "function"),
        ("DEFINE DATA", "data-section"),
        ("DEFINE WINDOW ", "window"),
    ] {
        if su.starts_with(prefix) {
            let rest = &su[prefix.len()..];
            let name = rest.split_whitespace().next().unwrap_or("").to_string();
            if !name.is_empty() {
                return Some((kind.into(), name));
            }
            if prefix.ends_with("DATA") {
                return Some(("data-section".into(), "DATA".into()));
            }
        }
    }
    for prefix in ["SUBROUTINE ", "FUNCTION "] {
        if let Some(rest) = su.strip_prefix(prefix) {
            let name = rest.split_whitespace().next().unwrap_or("").to_string();
            if valid_ident(&name) {
                return Some(("subroutine".into(), name));
            }
        }
    }
    None
}

fn parse_rpg_symbol(s: &str) -> Option<(String, String)> {
    let su = s.to_uppercase();
    for (prefix, kind) in [
        ("DCL-PROC ", "procedure"),
        ("DCL-DS ", "data-struct"),
        ("DCL-S ", "variable"),
        ("DCL-C ", "constant"),
        ("DCL-F ", "file"),
    ] {
        if let Some(rest) = su.strip_prefix(prefix) {
            let name = rest.split_whitespace().next().unwrap_or("").to_string();
            if valid_ident(&name) {
                return Some((kind.into(), name));
            }
        }
    }
    if su.starts_with('P') && su.len() > 1 {
        let name_part: String = su.chars().skip(6).take(14).collect();
        let name = name_part.trim().to_string();
        if valid_ident(&name) {
            return Some(("procedure".into(), name));
        }
    }
    for prefix in ["BEGSR ", "BEGSR\n"] {
        if su.starts_with(prefix) {
            let name = su[prefix.len()..]
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string();
            if valid_ident(&name) {
                return Some(("subroutine".into(), name));
            }
        }
    }
    None
}

fn parse_powerscript_symbol(s: &str) -> Option<(String, String)> {
    let sl = s.to_lowercase();
    for (prefix, kind) in [
        ("forward\n", "forward"),
        ("type ", "type"),
        ("global type ", "global-type"),
    ] {
        if sl.starts_with(prefix) {
            let rest = &s[prefix.len()..];
            let name = rest.split_whitespace().next().unwrap_or("").to_string();
            if valid_ident(&name) {
                return Some((kind.into(), name));
            }
        }
    }
    for (prefix, kind) in [
        ("public function ", "function"),
        ("private function ", "function"),
        ("protected function ", "function"),
        ("function ", "function"),
        ("public subroutine ", "subroutine"),
        ("private subroutine ", "subroutine"),
        ("subroutine ", "subroutine"),
        ("on ", "event"),
        ("event ", "event"),
    ] {
        if sl.starts_with(prefix) {
            let rest = &s[prefix.len()..];
            let name = rest.split(['(', ' ']).next().unwrap_or("").to_string();
            if valid_ident(&name) {
                return Some((kind.into(), name));
            }
        }
    }
    None
}

fn parse_symbol(lang: &str, line: &str) -> Option<(String, String)> {
    let s = line.trim();
    match lang {
        "rust" => parse_rust_symbol(s),
        "python" => parse_python_symbol(s),
        "javascript" | "typescript" | "tsx" | "jsx" => parse_js_ts_symbol(s),
        "go" => parse_go_symbol(s),
        "java" | "kotlin" | "csharp" => parse_jvm_dotnet_symbol(s),
        "cobol" => parse_cobol_symbol(s),
        "natural" => parse_natural_symbol(s),
        "rpg" => parse_rpg_symbol(s),
        "powerscript" => parse_powerscript_symbol(s),
        _ => None,
    }
}

fn valid_ident(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars().all(|c| c.is_alphanumeric() || c == '_')
        && s.chars()
            .next()
            .map(|c| !c.is_ascii_digit())
            .unwrap_or(false)
}

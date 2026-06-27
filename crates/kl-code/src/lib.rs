//! kl-code — persistent codebase memory store.
//!
//! Indexes local directories into a dedicated SQLite DB (`KLAYER_CODE_DB`,
//! default `klayer_code.db`) with FTS5 over code chunks, so the LLM can
//! recall any function, struct, or pattern across sessions.
//!
//! Intentionally separate from `klayer.db` (knowledge/episodes).

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::Serialize;
use std::path::Path;
use std::sync::Mutex;

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
PRAGMA journal_mode = WAL;
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
    conn: Mutex<Connection>,
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
}

// ── CodeStore impl ────────────────────────────────────────────────────────────

impl CodeStore {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening code db at {path}"))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn migrate(&self) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute_batch(SCHEMA).context("code db schema")?;
        Ok(())
    }

    pub fn stats(&self) -> Result<CodeStats> {
        let c = self.conn.lock().unwrap();
        let repos = c.query_row("SELECT COUNT(*) FROM repos", [], |r| r.get(0))?;
        let files = c.query_row("SELECT COUNT(*) FROM code_files", [], |r| r.get(0))?;
        let chunks = c.query_row("SELECT COUNT(*) FROM code_chunks", [], |r| r.get(0))?;
        Ok(CodeStats {
            repos,
            files,
            chunks,
        })
    }

    pub fn list_repos(&self) -> Result<Vec<RepoInfo>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(
            "SELECT id, path, name, indexed_at, file_count, chunk_count
             FROM repos ORDER BY indexed_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(RepoInfo {
                id: r.get(0)?,
                path: r.get(1)?,
                name: r.get(2)?,
                indexed_at: r.get(3)?,
                file_count: r.get(4)?,
                chunk_count: r.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn forget_repo(&self, path: &str) -> Result<bool> {
        let c = self.conn.lock().unwrap();

        let repo_id: Option<i64> = c
            .query_row("SELECT id FROM repos WHERE path = ?1", params![path], |r| {
                r.get(0)
            })
            .ok();
        let Some(repo_id) = repo_id else {
            return Ok(false);
        };

        // Collect chunk IDs before cascade-deleting (FTS5 must be cleaned first).
        let mut chunk_ids: Vec<i64> = Vec::new();
        let mut stmt = c.prepare(
            "SELECT cc.id FROM code_chunks cc
             JOIN code_files cf ON cf.id = cc.file_id
             WHERE cf.repo_id = ?1",
        )?;
        for row in stmt.query_map(params![repo_id], |r| r.get::<_, i64>(0))? {
            chunk_ids.push(row?);
        }
        drop(stmt); // Release borrow on c before execute_batch

        // Remove from FTS5 then cascade-delete from repos
        for id in &chunk_ids {
            c.execute("DELETE FROM code_fts WHERE rowid = ?1", params![id])?;
        }
        c.execute("DELETE FROM repos WHERE id = ?1", params![repo_id])?;
        Ok(true)
    }

    pub fn clear_all(&self) -> Result<u64> {
        let c = self.conn.lock().unwrap();
        c.execute_batch("DELETE FROM code_fts; DELETE FROM repos;")?;
        let deleted: u64 = c
            .query_row("SELECT changes()", [], |r| r.get(0))
            .unwrap_or(0);
        Ok(deleted)
    }

    pub fn search(
        &self,
        query: &str,
        repo_path: Option<&str>,
        limit: usize,
    ) -> Result<Vec<CodeHit>> {
        if query.trim().is_empty() {
            return Ok(vec![]);
        }
        let match_expr = fts_match(query);
        let c = self.conn.lock().unwrap();
        let lim = limit as i64;
        let mut hits = Vec::new();

        if let Some(rp) = repo_path {
            let mut stmt = c.prepare(
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
            )?;
            for row in stmt.query_map(params![match_expr, rp, lim], |r| {
                Ok(CodeHit {
                    snippet: r.get(0)?,
                    kind: r.get(1)?,
                    symbol_name: r.get(2)?,
                    line_start: r.get(3)?,
                    line_end: r.get(4)?,
                    file_path: r.get(5)?,
                    language: r.get(6)?,
                    repo_path: r.get(7)?,
                    score: r.get(8)?,
                })
            })? {
                hits.push(row?);
            }
        } else {
            let mut stmt = c.prepare(
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
            )?;
            for row in stmt.query_map(params![match_expr, lim], |r| {
                Ok(CodeHit {
                    snippet: r.get(0)?,
                    kind: r.get(1)?,
                    symbol_name: r.get(2)?,
                    line_start: r.get(3)?,
                    line_end: r.get(4)?,
                    file_path: r.get(5)?,
                    language: r.get(6)?,
                    repo_path: r.get(7)?,
                    score: r.get(8)?,
                })
            })? {
                hits.push(row?);
            }
        }

        Ok(hits)
    }

    pub fn index_repo(&self, dir_path: &str, name: Option<&str>) -> Result<IndexStats> {
        let canonical = std::fs::canonicalize(dir_path)
            .with_context(|| format!("resolving path: {dir_path}"))?;
        let canon_str = canonical.to_string_lossy().replace('\\', "/");
        let now = Utc::now().timestamp();

        // Collect files outside the lock (blocking I/O, may be large).
        let mut file_data: Vec<FileEntry> = Vec::new();
        let mut skipped = 0usize;
        collect_files(&canonical, &canonical, &mut file_data, &mut skipped)?;

        let total_files = file_data.len();
        let total_chunks: usize = file_data.iter().map(|f| f.chunks.len()).sum();

        tracing::info!(
            "code-index: {} files, {} chunks, {} skipped — writing to DB",
            total_files,
            total_chunks,
            skipped
        );

        let c = self.conn.lock().unwrap();

        // Upsert repo record.
        c.execute(
            "INSERT INTO repos (path, name, indexed_at, file_count, chunk_count)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(path) DO UPDATE SET
                 name        = COALESCE(?2, name),
                 indexed_at  = ?3,
                 file_count  = ?4,
                 chunk_count = ?5",
            params![
                canon_str,
                name,
                now,
                total_files as i64,
                total_chunks as i64
            ],
        )?;
        let repo_id: i64 = c.query_row(
            "SELECT id FROM repos WHERE path = ?1",
            params![canon_str],
            |r| r.get(0),
        )?;

        // Collect old chunk IDs before cascade-deleting (FTS cleanup required).
        let mut old_chunk_ids: Vec<i64> = Vec::new();
        let mut stmt = c.prepare(
            "SELECT cc.id FROM code_chunks cc
             JOIN code_files cf ON cf.id = cc.file_id
             WHERE cf.repo_id = ?1",
        )?;
        for row in stmt.query_map(params![repo_id], |r| r.get::<_, i64>(0))? {
            old_chunk_ids.push(row?);
        }
        drop(stmt);

        // Clean FTS5 for old entries, then delete old files (cascade → chunks).
        for id in &old_chunk_ids {
            c.execute("DELETE FROM code_fts WHERE rowid = ?1", params![id])?;
        }
        c.execute(
            "DELETE FROM code_files WHERE repo_id = ?1",
            params![repo_id],
        )?;

        // Insert new files and chunks in one transaction.
        insert_repo_data(&c, repo_id, &file_data)?;

        Ok(IndexStats {
            files: total_files,
            chunks: total_chunks,
            skipped,
        })
    }
}

fn insert_repo_data(c: &Connection, repo_id: i64, file_data: &[FileEntry]) -> Result<()> {
    c.execute_batch("SAVEPOINT index_repo;")?;
    let result = (|| -> Result<()> {
        for entry in file_data {
            c.execute(
                "INSERT INTO code_files (repo_id, rel_path, language) VALUES (?1, ?2, ?3)",
                params![repo_id, entry.rel_path, entry.language],
            )?;
            let file_id = c.last_insert_rowid();

            for chunk in &entry.chunks {
                c.execute(
                    "INSERT INTO code_chunks
                         (file_id, line_start, line_end, content, kind, symbol_name)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        file_id,
                        chunk.line_start as i64,
                        chunk.line_end as i64,
                        chunk.content,
                        chunk.kind,
                        chunk.symbol_name
                    ],
                )?;
                let chunk_id = c.last_insert_rowid();

                let body = format!(
                    "{} {}\n{}",
                    chunk.symbol_name.as_deref().unwrap_or(""),
                    entry.rel_path,
                    chunk.content
                );
                c.execute(
                    "INSERT INTO code_fts (rowid, body) VALUES (?1, ?2)",
                    params![chunk_id, body],
                )?;
            }
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            c.execute_batch("RELEASE SAVEPOINT index_repo;")?;
            Ok(())
        }
        Err(e) => {
            c.execute_batch("ROLLBACK TO SAVEPOINT index_repo;").ok();
            Err(e)
        }
    }
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
    skipped: &mut usize,
) -> Result<()> {
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > MAX_FILE_BYTES {
            *skipped += 1;
            return Ok(());
        }
    }

    let Ok(content) = std::fs::read_to_string(path) else {
        *skipped += 1;
        return Ok(());
    };
    if content.trim().is_empty() {
        return Ok(());
    }

    let rel = path.strip_prefix(root).unwrap_or(path);
    let rel_str = rel.to_string_lossy().replace('\\', "/");
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
    skipped: &mut usize,
) -> Result<()> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => {
            *skipped += 1;
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
        let Some(lang) = detect_language(&path) else {
            *skipped += 1;
            continue;
        };

        process_file_entry(root, &path, lang, out, skipped)?;
    }

    Ok(())
}

fn detect_language(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?;
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

    EXT_MAP.iter()
        .find(|&&(e, _)| e == ext)
        .map(|&(_, lang)| lang)
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

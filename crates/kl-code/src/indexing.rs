//! Repo scanning/ingestion: file collection, chunking, FTS5 writes.

use anyhow::Result;
use libsql::{params, Connection};
use std::path::Path;

use crate::lang::detect_language;
use crate::symbols::detect_symbol;

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

pub struct FileEntry {
    pub rel_path: String,
    pub language: String,
    pub chunks: Vec<ChunkEntry>,
}

pub struct ChunkEntry {
    pub line_start: usize,
    pub line_end: usize,
    pub content: String,
    pub kind: Option<String>,
    pub symbol_name: Option<String>,
}

pub async fn scalar_i64(conn: &Connection, sql: &str) -> Result<i64> {
    let mut rows = conn.query(sql, ()).await?;
    Ok(rows
        .next()
        .await?
        .map(|r| r.get(0))
        .transpose()?
        .unwrap_or(0))
}

pub async fn scalar_i64_params(
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

pub async fn insert_repo_data(
    conn: &Connection,
    repo_id: i64,
    file_data: &[FileEntry],
) -> Result<()> {
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

pub fn fts_match(query: &str) -> String {
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

pub fn collect_files(
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

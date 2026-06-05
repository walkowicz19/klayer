//! kl-ingest — fetch a URL, detect content-type, extract readable text, chunk it.
//!
//! Supported types: HTML, PDF, JSON, plain text / Markdown.
//!
//! IMPORTANT: scraper's `Html` is not `Send`. Keep HTML parsing synchronous and
//! never hold a parsed document across an `.await`.

use anyhow::{Context, Result};
use scraper::{Html, Selector};

pub struct Fetched {
    pub content_type: String,
    pub body: Vec<u8>,
}

pub async fn fetch(url: &str) -> Result<Fetched> {
    let client = reqwest::Client::builder()
        .user_agent("klayer/0.1 (+https://github.com/walkowicz19/klayer)")
        .build()?;
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()?;

    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .split(';')
        .next()
        .unwrap_or("application/octet-stream")
        .trim()
        .to_lowercase();

    let body = resp.bytes().await?.to_vec();
    Ok(Fetched { content_type, body })
}

/// Returns (title, plain_text). Dispatches on content-type.
pub fn extract(fetched: &Fetched) -> (String, String) {
    let ct = fetched.content_type.as_str();
    if ct.contains("html") {
        extract_html(&fetched.body)
    } else if ct == "application/pdf" || ct == "application/x-pdf" {
        extract_pdf(&fetched.body)
    } else if ct.contains("json") {
        extract_json(&fetched.body)
    } else if ct.starts_with("text/") || ct.contains("markdown") || ct.contains("xml") {
        extract_plain(&fetched.body)
    } else {
        (
            String::new(),
            format!(
                "[klayer] Unsupported content-type '{ct}'. \
                 Supported: HTML, PDF, JSON, plain text / Markdown."
            ),
        )
    }
}

fn extract_html(body: &[u8]) -> (String, String) {
    let html = String::from_utf8_lossy(body);
    let doc = Html::parse_document(&html);

    let title = Selector::parse("title")
        .ok()
        .and_then(|s| doc.select(&s).next().map(|t| t.text().collect::<String>()))
        .unwrap_or_default()
        .trim()
        .to_string();

    let sel = Selector::parse("p, li, h1, h2, h3, h4, pre, blockquote")
        .expect("static selector");
    let mut parts: Vec<String> = Vec::new();
    for el in doc.select(&sel) {
        let t = el.text().collect::<String>();
        let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
        if t.len() >= 3 {
            parts.push(t);
        }
    }
    (title, parts.join("\n"))
}

fn extract_pdf(body: &[u8]) -> (String, String) {
    match pdf_extract::extract_text_from_mem(body) {
        Ok(text) => {
            let text = text.trim().to_string();
            if text.is_empty() {
                (
                    String::new(),
                    "[klayer] PDF contained no extractable text (may be a scanned image).".to_string(),
                )
            } else {
                (String::new(), text)
            }
        }
        Err(e) => (String::new(), format!("[klayer] PDF extraction failed: {e}")),
    }
}

fn extract_json(body: &[u8]) -> (String, String) {
    let raw = String::from_utf8_lossy(body);
    let text = serde_json::from_str::<serde_json::Value>(&raw)
        .map(|v| serde_json::to_string_pretty(&v).unwrap_or_else(|_| raw.to_string()))
        .unwrap_or_else(|_| raw.to_string());
    (String::new(), text)
}

fn extract_plain(body: &[u8]) -> (String, String) {
    (String::new(), String::from_utf8_lossy(body).trim().to_string())
}

/// Greedy paragraph packing into ~`max`-char chunks (chars, not tokens).
pub fn chunk(text: &str, max: usize) -> Vec<String> {
    let max = max.max(200);
    let mut out = Vec::new();
    let mut cur = String::new();
    for para in text.split('\n') {
        let p = para.trim();
        if p.is_empty() {
            continue;
        }
        if !cur.is_empty() && cur.len() + p.len() + 1 > max {
            out.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push('\n');
        }
        cur.push_str(p);
        while cur.len() >= max {
            let split_at = cur
                .char_indices()
                .take_while(|(i, _)| *i < max)
                .last()
                .map(|(i, c)| i + c.len_utf8())
                .unwrap_or(cur.len());
            let rest = cur.split_off(split_at);
            out.push(std::mem::take(&mut cur));
            cur = rest;
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

//! kl-ingest — fetch a URL, extract readable text, and chunk it.
//!
//! IMPORTANT: scraper's `Html` is not `Send`. Keep all parsing synchronous and
//! never hold a parsed document across an `.await`. The pattern is:
//!   let body = fetch(url).await?;   // async
//!   let (title, text) = extract(&body);  // sync, no await inside

use anyhow::{Context, Result};
use scraper::{Html, Selector};

pub async fn fetch(url: &str) -> Result<String> {
    let client = reqwest::Client::builder()
        .user_agent("klayer/0.1 (+https://github.com/walkowicz19/klayer)")
        .build()?;
    let body = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()?
        .text()
        .await?;
    Ok(body)
}

/// Returns (title, plain_text). Pulls text from common content elements only.
pub fn extract(html: &str) -> (String, String) {
    let doc = Html::parse_document(html);

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

/// Greedy paragraph packing into ~`max`-char chunks (chars, not tokens — good
/// enough for v0; token-aware splitting is a later refinement).
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
            // hard-split an oversized single paragraph
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

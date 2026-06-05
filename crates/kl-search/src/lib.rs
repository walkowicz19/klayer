//! kl-search — pluggable web search behind the `SearchBackend` trait.
//!
//! Ships a no-API-key DuckDuckGo HTML backend (scraping). Scraping is inherently
//! fragile (engines change markup / rate-limit), which is exactly why the backend
//! is a trait: drop in a real API later without touching the server.

use anyhow::Result;
use async_trait::async_trait;
use kl_core::{SearchBackend, SearchResult};
use scraper::{Html, Selector};

pub struct DuckDuckGo {
    client: reqwest::Client,
}

impl Default for DuckDuckGo {
    fn default() -> Self {
        Self {
            client: reqwest::Client::builder()
                .user_agent("Mozilla/5.0 (compatible; klayer/0.1)")
                .build()
                .expect("reqwest client"),
        }
    }
}

#[async_trait]
impl SearchBackend for DuckDuckGo {
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        // async fetch first, then parse synchronously (Html is not Send).
        let body = self
            .client
            .post("https://html.duckduckgo.com/html/")
            .form(&[("q", query)])
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        Ok(parse(&body, limit))
    }
}

fn parse(html: &str, limit: usize) -> Vec<SearchResult> {
    let doc = Html::parse_document(html);
    let a = Selector::parse("a.result__a").expect("sel");
    let snip = Selector::parse(".result__snippet").expect("sel");

    let titles: Vec<(String, String)> = doc
        .select(&a)
        .map(|e| {
            let title = e.text().collect::<String>().trim().to_string();
            let url = e.value().attr("href").unwrap_or_default().to_string();
            (title, url)
        })
        .collect();
    let snippets: Vec<String> = doc
        .select(&snip)
        .map(|e| e.text().collect::<String>().trim().to_string())
        .collect();

    titles
        .into_iter()
        .enumerate()
        .take(limit)
        .map(|(i, (title, url))| SearchResult {
            title,
            url,
            snippet: snippets.get(i).cloned().unwrap_or_default(),
        })
        .collect()
}

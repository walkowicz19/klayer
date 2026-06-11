//! kl-search — pluggable web search behind the `SearchBackend` trait.
//!
//! Backends (all no-API-key unless noted):
//!   DuckDuckGo  — HTML scraping (default, can be rate-limited)
//!   Bing        — HTML scraping (fallback)
//!   Brave       — REST API (requires KLAYER_BRAVE_API_KEY, free tier: 2000 req/month)
//!   FallbackSearch — wraps two backends; tries primary, falls back on empty/error
//!
//! Select via KLAYER_SEARCH env var: duckduckgo | bing | brave | auto (default)
//! "auto" = DuckDuckGo with Bing as fallback.

use anyhow::Result;
use async_trait::async_trait;
use kl_core::{SearchBackend, SearchResult};
use scraper::{Html, Selector};

// ── DuckDuckGo ────────────────────────────────────────────────────────────────

pub struct DuckDuckGo {
    client: reqwest::Client,
}

impl Default for DuckDuckGo {
    fn default() -> Self {
        Self { client: build_client() }
    }
}

#[async_trait]
impl SearchBackend for DuckDuckGo {
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let clean_query = query.trim().trim_matches('"').trim_matches('\'').trim();
        let body = self
            .client
            .post("https://html.duckduckgo.com/html/")
            .form(&[("q", clean_query)])
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        Ok(parse_ddg(&body, limit))
    }
}

fn parse_ddg(html: &str, limit: usize) -> Vec<SearchResult> {
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

// ── Bing ──────────────────────────────────────────────────────────────────────

pub struct Bing {
    client: reqwest::Client,
}

impl Default for Bing {
    fn default() -> Self {
        Self { client: build_client() }
    }
}

#[async_trait]
impl SearchBackend for Bing {
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let clean_query = query.trim().trim_matches('"').trim_matches('\'').trim();
        let url = format!(
            "https://www.bing.com/search?q={}&count={}",
            urlencoding::encode(clean_query),
            limit
        );
        let body = self
            .client
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        Ok(parse_bing(&body, limit))
    }
}

fn parse_bing(html: &str, limit: usize) -> Vec<SearchResult> {
    let doc = Html::parse_document(html);
    let item_sel = Selector::parse("li.b_algo").expect("sel");
    let title_sel = Selector::parse("h2 a").expect("sel");
    let snip_sel = Selector::parse(".b_caption p, .b_algoSlug").expect("sel");

    doc.select(&item_sel)
        .take(limit)
        .filter_map(|item| {
            let title_el = item.select(&title_sel).next()?;
            let title = title_el.text().collect::<String>().trim().to_string();
            let url = title_el.value().attr("href").unwrap_or_default().to_string();
            let snippet = item
                .select(&snip_sel)
                .next()
                .map(|e| e.text().collect::<String>().trim().to_string())
                .unwrap_or_default();
            if title.is_empty() || url.is_empty() {
                None
            } else {
                Some(SearchResult { title, url, snippet })
            }
        })
        .collect()
}

// ── Brave Search API ──────────────────────────────────────────────────────────

pub struct Brave {
    client: reqwest::Client,
    api_key: String,
}

impl Brave {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self { client: build_client(), api_key: api_key.into() }
    }
}

#[async_trait]
impl SearchBackend for Brave {
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let clean_query = query.trim().trim_matches('"').trim_matches('\'').trim();
        let url = format!(
            "https://api.search.brave.com/res/v1/web/search?q={}&count={}",
            urlencoding::encode(clean_query),
            limit.min(20)
        );
        let body = self
            .client
            .get(&url)
            .header("Accept", "application/json")
            .header("X-Subscription-Token", &self.api_key)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        parse_brave(&body)
    }
}

fn parse_brave(json: &str) -> Result<Vec<SearchResult>> {
    let v: serde_json::Value = serde_json::from_str(json)?;
    let results = v["web"]["results"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| {
            let title = r["title"].as_str()?.to_string();
            let url = r["url"].as_str()?.to_string();
            let snippet = r["description"].as_str().unwrap_or("").to_string();
            Some(SearchResult { title, url, snippet })
        })
        .collect();
    Ok(results)
}

// ── FallbackSearch ────────────────────────────────────────────────────────────

/// Tries `primary`; if it errors or returns no results, tries `fallback`.
pub struct FallbackSearch {
    primary: Box<dyn SearchBackend>,
    fallback: Box<dyn SearchBackend>,
}

impl FallbackSearch {
    pub fn new(primary: Box<dyn SearchBackend>, fallback: Box<dyn SearchBackend>) -> Self {
        Self { primary, fallback }
    }
}

#[async_trait]
impl SearchBackend for FallbackSearch {
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        match self.primary.search(query, limit).await {
            Ok(results) if !results.is_empty() => Ok(results),
            _ => self.fallback.search(query, limit).await,
        }
    }
}

// ── Factory ───────────────────────────────────────────────────────────────────

/// Build a backend from KLAYER_SEARCH env var.
/// Values: "duckduckgo" | "bing" | "brave" | "auto" (default).
/// "brave" requires KLAYER_BRAVE_API_KEY.
/// "auto" = DuckDuckGo with Bing fallback.
pub fn from_env() -> Box<dyn SearchBackend> {
    let engine = std::env::var("KLAYER_SEARCH")
        .unwrap_or_else(|_| "auto".to_string())
        .to_lowercase();

    match engine.as_str() {
        "duckduckgo" | "ddg" => Box::new(DuckDuckGo::default()),
        "bing" => Box::new(Bing::default()),
        "brave" => {
            let key = std::env::var("KLAYER_BRAVE_API_KEY")
                .expect("KLAYER_BRAVE_API_KEY must be set when KLAYER_SEARCH=brave");
            Box::new(Brave::new(key))
        }
        _ => Box::new(FallbackSearch::new(
            Box::new(DuckDuckGo::default()),
            Box::new(Bing::default()),
        )),
    }
}

// ── shared helpers ────────────────────────────────────────────────────────────

fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36")
        .build()
        .expect("reqwest client")
}

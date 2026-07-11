//! Reverse-explainability compliance reporting: groups the episode audit
//! trail (kl_store::Store::list_episodes) by run_id and enriches each
//! knowledge_ids_used reference with the referenced item's trust tier,
//! source, and best-effort approver, then renders the result as JSON
//! (ComplianceReport is `Serialize`) or a downloadable PDF.
//!
//! This extends `export_explainability`'s existing episode -> knowledge_ids
//! join (see main.rs) rather than re-deriving it; the same
//! `store.list_episodes` / `store.get_knowledge_by_id` calls are reused.
//!
//! PDF rendering deliberately avoids printpdf's `PdfDocument::from_html`
//! (the azul-layout-backed XML/flexbox engine advertised in its README):
//! on this toolchain it segfaults (STATUS_ACCESS_VIOLATION) even on a
//! two-element HTML fixture, confirmed with a standalone repro outside this
//! crate. Instead we lay the table out manually with printpdf's stable
//! low-level `Op` API (`SetFont` / `ShowText` / `SetTextCursor`) against a
//! builtin Helvetica font (no TTF bundling needed), with hand-rolled page
//! breaking once a page's content exceeds its printable height.

use std::collections::{BTreeMap, HashSet};

use anyhow::{anyhow, Result};
use kl_core::EpisodeRow;
use kl_store::Store;
use printpdf::{
    BuiltinFont, Mm, Op, PdfDocument, PdfFontHandle, PdfPage, PdfSaveOptions, Point, Pt, TextItem,
};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct ComplianceKnowledgeItem {
    pub id: i64,
    pub title: String,
    pub trust: String,
    pub source: Option<String>,
    pub approver: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ComplianceRunSection {
    pub run_id: String,
    pub step_count: usize,
    pub first_ts: i64,
    pub last_ts: i64,
    pub steps: Vec<EpisodeRow>,
    pub knowledge_used: Vec<ComplianceKnowledgeItem>,
}

/// A retroactive-compliance finding: an enforced-domain action step that
/// either used the `override:true` escape hatch, or (defensively — the
/// execute_change gate should already prevent this) has no prior `recall`
/// episode against its domain earlier in the same run.
#[derive(Debug, Clone, Serialize)]
pub struct ComplianceGap {
    pub run_id: String,
    pub episode_id: i64,
    pub step: i64,
    pub domain: String,
    pub stage: Option<String>,
    pub action: Option<String>,
    pub reason: &'static str, // "override" | "no_prior_recall"
    pub ts: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ComplianceReport {
    pub format: &'static str,
    pub run_id: Option<String>,
    pub runs: Vec<ComplianceRunSection>,
    pub gaps: Vec<ComplianceGap>,
}

/// Klayer's trust lifecycle (untrusted -> proposed -> reviewed | user, see
/// kl-core) has no per-item approver identity table today — only the trust
/// tier itself is persisted. Rather than fabricate a name the schema doesn't
/// track, surface the tier's approval semantics as a best-effort label.
fn approver_for_trust(trust: &str) -> Option<String> {
    match trust {
        "user" => Some("user (direct entry)".to_string()),
        "reviewed" => Some("reviewed (admin promotion)".to_string()),
        _ => None,
    }
}

pub fn build_compliance_report(store: &Store, run_id: Option<&str>) -> Result<ComplianceReport> {
    let episodes = store.list_episodes(run_id)?;
    let mut by_run: BTreeMap<String, Vec<EpisodeRow>> = BTreeMap::new();
    for ep in episodes {
        by_run.entry(ep.run_id.clone()).or_default().push(ep);
    }

    let mut runs = Vec::with_capacity(by_run.len());
    let mut gaps = Vec::new();
    for (rid, mut steps) in by_run {
        steps.sort_by_key(|s| s.step);

        // A domain counts as "recalled" for the rest of the run once a
        // recall-stage episode against it is seen, in step order.
        let mut recalled_domains: HashSet<String> = HashSet::new();
        for ep in &steps {
            let Some(domain) = ep.domain.clone() else {
                continue;
            };
            let is_recall = ep.stage.as_deref() == Some("recall");
            if is_recall {
                recalled_domains.insert(domain);
                continue;
            }
            let reason = if ep.outcome.as_deref() == Some("override") {
                Some("override")
            } else if !recalled_domains.contains(&domain) && store.domain_enforced(&domain)? {
                Some("no_prior_recall")
            } else {
                None
            };
            if let Some(reason) = reason {
                gaps.push(ComplianceGap {
                    run_id: rid.clone(),
                    episode_id: ep.id,
                    step: ep.step,
                    domain,
                    stage: ep.stage.clone(),
                    action: ep.action.clone(),
                    reason,
                    ts: ep.ts,
                });
            }
        }

        let first_ts = steps.iter().map(|s| s.ts).min().unwrap_or(0);
        let last_ts = steps.iter().map(|s| s.ts).max().unwrap_or(0);

        let mut ids: Vec<i64> = steps
            .iter()
            .flat_map(|s| s.knowledge_ids_used.iter().copied())
            .collect();
        ids.sort_unstable();
        ids.dedup();

        let mut knowledge_used = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(item) = store.get_knowledge_by_id(id)? {
                knowledge_used.push(ComplianceKnowledgeItem {
                    id: item.row.id,
                    title: item.row.title,
                    approver: approver_for_trust(&item.row.trust),
                    trust: item.row.trust,
                    source: item.source_title.or(item.source_uri),
                    created_at: item.row.created_at,
                    updated_at: item.row.updated_at,
                });
            }
        }

        runs.push(ComplianceRunSection {
            run_id: rid,
            step_count: steps.len(),
            first_ts,
            last_ts,
            steps,
            knowledge_used,
        });
    }

    Ok(ComplianceReport {
        format: "reverse-explainability-compliance-v1",
        run_id: run_id.map(str::to_string),
        runs,
        gaps,
    })
}

fn fmt_ts(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| ts.to_string())
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    // Builtin PDF fonts use WinAnsiEncoding (~Latin-1), not arbitrary
    // Unicode, so an actual "…" glyph (U+2026) renders as mojibake; stick to
    // plain ASCII dots for truncation instead.
    let mut t: String = s.chars().take(max_chars.saturating_sub(3)).collect();
    t.push_str("...");
    t
}

// ---- page geometry (mm) ----------------------------------------------
const PAGE_W: f32 = 210.0;
const PAGE_H: f32 = 297.0;
const MARGIN_L: f32 = 14.0;
const MARGIN_TOP: f32 = 16.0;
const MARGIN_BOTTOM: f32 = 16.0;

// (label, x offset from MARGIN_L, max chars for truncation)
const COLUMNS: [(&str, f32, usize); 5] = [
    ("ID", 0.0, 6),
    ("Trust Tier", 16.0, 16),
    ("Source", 42.0, 32),
    ("Approver", 96.0, 24),
    ("Timestamp", 142.0, 24),
];

struct PdfWriter {
    font: PdfFontHandle,
    font_bold: PdfFontHandle,
    pages: Vec<PdfPage>,
    ops: Vec<Op>,
    /// Distance in mm from the top margin down to the next line's baseline.
    y_offset: f32,
    page_num: usize,
}

impl PdfWriter {
    fn new() -> Self {
        Self {
            font: PdfFontHandle::Builtin(BuiltinFont::Helvetica),
            font_bold: PdfFontHandle::Builtin(BuiltinFont::HelveticaBold),
            pages: Vec::new(),
            ops: Vec::new(),
            y_offset: 0.0,
            page_num: 1,
        }
    }

    fn printable_height(&self) -> f32 {
        PAGE_H - MARGIN_TOP - MARGIN_BOTTOM
    }

    /// Starts a new page if `next_line_height` would overflow the current one.
    fn ensure_space(&mut self, next_line_height: f32) {
        if self.y_offset + next_line_height > self.printable_height() {
            self.flush_page();
        }
    }

    fn flush_page(&mut self) {
        let ops = std::mem::take(&mut self.ops);
        self.pages.push(PdfPage::new(Mm(PAGE_W), Mm(PAGE_H), ops));
        self.y_offset = 0.0;
        self.page_num += 1;
    }

    /// Renders one or more cells sharing a single row slot of `row_height_mm`
    /// (e.g. every column in a table row, or a single full-width line), then
    /// advances the cursor past that slot exactly once. Each cell's baseline
    /// sits at 75% of the row height down from the slot's top edge, which
    /// keeps ascenders/descenders clear of the rows immediately above/below.
    fn row(
        &mut self,
        font: &PdfFontHandle,
        size_pt: f32,
        row_height_mm: f32,
        cells: &[(f32, &str)],
    ) {
        self.ensure_space(row_height_mm);
        let y_top = MARGIN_TOP + self.y_offset + row_height_mm * 0.75;
        let y = Mm(PAGE_H - y_top).into();
        for (x_mm, text) in cells {
            let pos = Point {
                x: Mm(MARGIN_L + x_mm).into(),
                y,
            };
            self.ops.push(Op::StartTextSection);
            self.ops.push(Op::SetTextCursor { pos });
            self.ops.push(Op::SetFont {
                font: font.clone(),
                size: Pt(size_pt),
            });
            self.ops.push(Op::ShowText {
                items: vec![TextItem::Text((*text).to_string())],
            });
            self.ops.push(Op::EndTextSection);
        }
        self.y_offset += row_height_mm;
    }

    fn line(
        &mut self,
        font: &PdfFontHandle,
        size_pt: f32,
        x_mm: f32,
        text: &str,
        row_height_mm: f32,
    ) {
        let font = font.clone();
        self.row(&font, size_pt, row_height_mm, &[(x_mm, text)]);
    }

    fn table_header(&mut self) {
        let cells: Vec<(f32, &str)> = COLUMNS.iter().map(|(label, x, _)| (*x, *label)).collect();
        let font = self.font_bold.clone();
        self.row(&font, 9.0, 7.0, &cells);
    }

    fn table_row(&mut self, item: &ComplianceKnowledgeItem) {
        let values = [
            item.id.to_string(),
            item.trust.clone(),
            item.source.clone().unwrap_or_else(|| "-".to_string()),
            item.approver.clone().unwrap_or_else(|| "-".to_string()),
            fmt_ts(item.updated_at),
        ];
        let truncated: Vec<String> = values
            .iter()
            .zip(COLUMNS.iter())
            .map(|(value, (_, _, max_chars))| truncate(value, *max_chars))
            .collect();
        let cells: Vec<(f32, &str)> = truncated
            .iter()
            .zip(COLUMNS.iter())
            .map(|(text, (_, x, _))| (*x, text.as_str()))
            .collect();
        let font = self.font.clone();
        self.row(&font, 8.0, 6.0, &cells);
    }

    fn finish(mut self) -> Vec<PdfPage> {
        if !self.ops.is_empty() || self.pages.is_empty() {
            self.flush_page();
        }
        self.pages
    }
}

/// Renders the same content `build_compliance_report` produces as a real PDF
/// document (title, per-run sections, a knowledge-item table). See the
/// module doc comment for why this uses printpdf's manual `Op` API instead
/// of `PdfDocument::from_html`.
pub fn render_compliance_pdf(report: &ComplianceReport) -> Result<Vec<u8>> {
    let mut w = PdfWriter::new();

    let title_font = w.font_bold.clone();
    w.line(
        &title_font,
        18.0,
        0.0,
        "Reverse-Explainability Compliance Report",
        10.0,
    );
    if let Some(rid) = &report.run_id {
        let font = w.font.clone();
        w.line(&font, 9.0, 0.0, &format!("Filtered to run: {rid}"), 6.0);
    }
    if report.runs.is_empty() {
        let font = w.font.clone();
        w.line(&font, 10.0, 0.0, "No episodes recorded.", 6.0);
    }

    for run in &report.runs {
        let header_font = w.font_bold.clone();
        w.line(
            &header_font,
            13.0,
            0.0,
            &format!("Run: {}", run.run_id),
            8.0,
        );
        let meta_font = w.font.clone();
        w.line(
            &meta_font,
            9.0,
            0.0,
            &format!(
                "{} step(s), {} to {}",
                run.step_count,
                fmt_ts(run.first_ts),
                fmt_ts(run.last_ts)
            ),
            6.0,
        );
        w.table_header();
        if run.knowledge_used.is_empty() {
            let font = w.font.clone();
            w.line(
                &font,
                8.0,
                0.0,
                "No knowledge items recorded for this run.",
                6.0,
            );
        } else {
            for item in &run.knowledge_used {
                w.table_row(item);
            }
        }
        w.y_offset += 4.0;
    }

    if !report.gaps.is_empty() {
        let header_font = w.font_bold.clone();
        w.line(
            &header_font,
            13.0,
            0.0,
            "Compliance Gaps (overrides / bypassed preconditions)",
            8.0,
        );
        for gap in &report.gaps {
            let font = w.font.clone();
            w.line(
                &font,
                8.0,
                0.0,
                &truncate(
                    &format!(
                        "run={} step={} domain={} reason={} action={}",
                        gap.run_id,
                        gap.step,
                        gap.domain,
                        gap.reason,
                        gap.action.as_deref().unwrap_or("-")
                    ),
                    120,
                ),
                6.0,
            );
        }
    }

    let pages = w.finish();
    let page_count = pages.len();
    let mut doc = PdfDocument::new("Reverse-Explainability Compliance Report");
    doc.with_pages(pages);

    // Footer page numbers, added as a second pass now that page_count is known.
    for (idx, page) in doc.pages.iter_mut().enumerate() {
        let font = PdfFontHandle::Builtin(BuiltinFont::Helvetica);
        let pos = Point {
            x: Mm(PAGE_W - MARGIN_L - 24.0).into(),
            y: Mm(MARGIN_BOTTOM - 6.0).into(),
        };
        page.ops.push(Op::StartTextSection);
        page.ops.push(Op::SetTextCursor { pos });
        page.ops.push(Op::SetFont {
            font,
            size: Pt(7.0),
        });
        page.ops.push(Op::ShowText {
            items: vec![TextItem::Text(format!(
                "Page {} of {}",
                idx + 1,
                page_count
            ))],
        });
        page.ops.push(Op::EndTextSection);
    }

    let mut warnings = Vec::new();
    let bytes = doc.save(&PdfSaveOptions::default(), &mut warnings);
    if bytes.is_empty() {
        return Err(anyhow!("compliance PDF render produced no bytes"));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_store() -> Store {
        let store = Store::open(":memory:").expect("open in-memory store");
        store.migrate().expect("migrate");
        let id = store
            .remember("compliance-test", "Always verify approvals before merge.")
            .expect("remember");
        store
            .log_episode(
                "run-1",
                1,
                Some("plan"),
                Some("recall"),
                Some("found rule"),
                Some("ok"),
            )
            .expect("log_episode");
        store
            .set_episode_knowledge_ids(1, &[id])
            .expect("set_episode_knowledge_ids");
        store
    }

    #[test]
    fn build_compliance_report_groups_by_run_and_enriches_items() {
        let store = fixture_store();
        let report = build_compliance_report(&store, None).expect("build report");
        assert_eq!(report.runs.len(), 1);
        let run = &report.runs[0];
        assert_eq!(run.run_id, "run-1");
        assert_eq!(run.knowledge_used.len(), 1);
        let item = &run.knowledge_used[0];
        assert_eq!(item.trust, "user");
        assert_eq!(item.approver.as_deref(), Some("user (direct entry)"));
    }

    #[test]
    fn render_compliance_pdf_produces_valid_pdf_bytes() {
        let store = fixture_store();
        let report = build_compliance_report(&store, None).expect("build report");
        let bytes = render_compliance_pdf(&report).expect("render pdf");
        assert!(!bytes.is_empty());
        assert!(
            bytes.starts_with(b"%PDF-"),
            "PDF must start with %PDF- header"
        );
        let tail = &bytes[bytes.len().saturating_sub(64)..];
        assert!(
            tail.windows(5).any(|w| w == b"%%EOF"),
            "PDF must end with %%EOF trailer"
        );
    }

    #[test]
    fn render_compliance_pdf_handles_empty_report() {
        let report = ComplianceReport {
            format: "reverse-explainability-compliance-v1",
            run_id: None,
            runs: Vec::new(),
            gaps: Vec::new(),
        };
        let bytes = render_compliance_pdf(&report).expect("render pdf");
        assert!(bytes.starts_with(b"%PDF-"));
    }

    #[test]
    fn render_compliance_pdf_paginates_large_runs() {
        let store = Store::open(":memory:").expect("open in-memory store");
        store.migrate().expect("migrate");
        let mut ids = Vec::new();
        for i in 0..80 {
            let id = store
                .remember("compliance-test", &format!("rule number {i}"))
                .expect("remember");
            ids.push(id);
        }
        store
            .log_episode("run-big", 1, None, Some("recall"), None, Some("ok"))
            .expect("log_episode");
        store
            .set_episode_knowledge_ids(1, &ids)
            .expect("set_episode_knowledge_ids");

        let report = build_compliance_report(&store, None).expect("build report");
        let bytes = render_compliance_pdf(&report).expect("render pdf");
        assert!(bytes.starts_with(b"%PDF-"));
        // 80 rows at ~6mm each is ~480mm, well over one A4 page's ~265mm of
        // printable height, so the writer must have paginated: the saved
        // PDF's page tree should list more than one leaf `/Type/Page` object
        // (distinct from the single `/Type/Pages` root, hence the `!= 's'`).
        let text = String::from_utf8_lossy(&bytes);
        let page_marker_count = text
            .match_indices("/Type/Page")
            .filter(|(i, m)| text.as_bytes().get(i + m.len()) != Some(&b's'))
            .count();
        assert!(
            page_marker_count > 1,
            "expected pagination for an 80-row run, got {page_marker_count} page marker(s)"
        );
    }
}

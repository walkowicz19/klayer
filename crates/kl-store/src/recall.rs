//! Hybrid `Store::recall`: FTS5 + LIKE + sqlite-vec, fused with RRF.

use anyhow::Result;
use kl_core::{Kind, RecallHit, Trust};
use rusqlite::{params, OptionalExtension};

use crate::Store;

impl Store {
    /// Hybrid recall: FTS5/BM25 + LIKE over curated knowledge + sqlite-vec
    /// nearest neighbors, fused with Reciprocal Rank Fusion.
    pub fn recall(
        &self,
        domain: &str,
        query: &str,
        kind: Option<Kind>,
        k: usize,
    ) -> Result<Vec<RecallHit>> {
        let k = k.max(1);
        let mut fts_chunk_ids: Vec<i64> = Vec::new();
        let like_knowledge_ids: Vec<i64>;
        let mut vec_chunk_ids: Vec<i64> = Vec::new();
        let mut vec_knowledge_ids: Vec<i64> = Vec::new();

        let match_expr = fts_match(query);
        {
            let c = self.conn.lock().unwrap();
            if !match_expr.is_empty() {
                let mut stmt = c.prepare(
                    "SELECT c.id FROM chunks_fts
                       JOIN chunks c ON c.id = chunks_fts.rowid
                      WHERE chunks_fts MATCH ?1 AND c.domain = ?2
                      ORDER BY bm25(chunks_fts) ASC
                      LIMIT ?3",
                )?;
                let rows = stmt.query_map(params![match_expr, domain, k as i64], |r| r.get(0))?;
                for row in rows {
                    fts_chunk_ids.push(row?);
                }
            }

            let terms: Vec<String> = query
                .split_whitespace()
                .map(|t| t.replace('%', "").replace('_', ""))
                .filter(|t| !t.is_empty())
                .map(|t| format!("%{t}%"))
                .collect();
            let terms = if terms.is_empty() {
                vec![format!("%{}%", query.replace('%', "").replace('_', ""))]
            } else {
                terms
            };
            let mut bind: Vec<String> = vec![domain.to_string()];
            let mut clauses = Vec::with_capacity(terms.len());
            for term in &terms {
                let idx = bind.len() + 1;
                clauses.push(format!("(title LIKE ?{idx} OR body LIKE ?{idx})"));
                bind.push(term.clone());
            }
            let mut sql = format!(
                "SELECT id FROM knowledge WHERE domain = ?1 AND ({})",
                clauses.join(" OR ")
            );
            if let Some(kd) = kind {
                let idx = bind.len() + 1;
                sql.push_str(&format!(" AND kind = ?{idx}"));
                bind.push(kd.as_str().to_string());
            }
            sql.push_str(
                " ORDER BY (CASE trust WHEN 'user' THEN 3 WHEN 'reviewed' THEN 2 WHEN 'proposed' THEN 1 ELSE 0 END) DESC, updated_at DESC LIMIT 50",
            );
            let mut stmt = c.prepare(&sql)?;
            like_knowledge_ids = stmt
                .query_map(rusqlite::params_from_iter(bind.iter()), |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<i64>>>()?;
        }

        if let Ok(qemb) = self.embedder().embed(query) {
            let limit = k.saturating_mul(3).max(8);
            if let Ok(nn) = self.search_chunks_vector(&qemb, limit) {
                let c = self.conn.lock().unwrap();
                for (cid, _) in nn {
                    let ok: bool = c
                        .query_row(
                            "SELECT 1 FROM chunks WHERE id = ?1 AND domain = ?2",
                            params![cid, domain],
                            |_| Ok(true),
                        )
                        .optional()?
                        .unwrap_or(false);
                    if ok {
                        vec_chunk_ids.push(cid);
                    }
                }
            }
            if let Ok(nn) = self.search_knowledge_vector(&qemb, limit) {
                let c = self.conn.lock().unwrap();
                for (kid, _) in nn {
                    let mut sql =
                        "SELECT 1 FROM knowledge WHERE id = ?1 AND domain = ?2".to_string();
                    let ok = if let Some(kd) = kind {
                        sql.push_str(" AND kind = ?3");
                        c.query_row(&sql, params![kid, domain, kd.as_str()], |_| Ok(true))
                            .optional()?
                            .unwrap_or(false)
                    } else {
                        c.query_row(&sql, params![kid, domain], |_| Ok(true))
                            .optional()?
                            .unwrap_or(false)
                    };
                    if ok {
                        vec_knowledge_ids.push(kid);
                    }
                }
            }
        }

        // Chunk/knowledge id spaces overlap — tag chunks into a high bit range for RRF.
        const CHUNK_TAG: i64 = 1 << 60;
        let tag_chunks =
            |ids: &[i64]| -> Vec<i64> { ids.iter().map(|id| id.wrapping_add(CHUNK_TAG)).collect() };
        let fused = kl_core::rrf_fuse(
            &[
                tag_chunks(&fts_chunk_ids),
                like_knowledge_ids.clone(),
                tag_chunks(&vec_chunk_ids),
                vec_knowledge_ids.clone(),
            ],
            60,
        );

        let mut ordered = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let c = self.conn.lock().unwrap();
        for (tagged, rrf_score) in fused {
            let is_chunk = tagged >= CHUNK_TAG;
            let id = if is_chunk {
                tagged.wrapping_sub(CHUNK_TAG)
            } else {
                tagged
            };
            if !seen.insert((is_chunk, id)) {
                continue;
            }
            let hit = if is_chunk {
                c.query_row(
                    "SELECT c.text, s.uri, s.fetched_at FROM chunks c
                       JOIN sources s ON s.id = c.source_id
                      WHERE c.id = ?1 AND c.domain = ?2",
                    params![id, domain],
                    |r| {
                        Ok(RecallHit {
                            source_kind: "chunk".into(),
                            kind: None,
                            title: String::new(),
                            body: r.get(0)?,
                            domain: domain.to_string(),
                            trust: "untrusted".into(),
                            enforceable: false,
                            provenance: r.get(1)?,
                            fetched_at: r.get(2)?,
                            score: -rrf_score,
                        })
                    },
                )
                .optional()?
            } else {
                c.query_row(
                    "SELECT kind, title, body, trust, source_id, created_at FROM knowledge
                      WHERE id = ?1 AND domain = ?2",
                    params![id, domain],
                    |r| {
                        let trust_s: String = r.get(3)?;
                        Ok(RecallHit {
                            source_kind: "knowledge".into(),
                            kind: Some(r.get::<_, String>(0)?),
                            title: r.get(1)?,
                            body: r.get(2)?,
                            domain: domain.to_string(),
                            enforceable: Trust::parse(&trust_s).is_enforceable(),
                            trust: trust_s,
                            provenance: r
                                .get::<_, Option<i64>>(4)?
                                .map(|sid| format!("knowledge:source#{sid}")),
                            fetched_at: r.get(5)?,
                            score: -rrf_score,
                        })
                    },
                )
                .optional()?
            };
            if let Some(hit) = hit {
                ordered.push(hit);
            }
            if ordered.len() >= k * 2 {
                break;
            }
        }
        drop(c);

        ordered.sort_by(|a, b| {
            let ta = Trust::parse(&a.trust).rank();
            let tb = Trust::parse(&b.trust).rank();
            tb.cmp(&ta).then(
                a.score
                    .partial_cmp(&b.score)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
        });
        ordered.truncate(k);
        Ok(ordered)
    }
}

fn fts_match(query: &str) -> String {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|t| t.replace('"', ""))
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{t}\""))
        .collect();
    terms.join(" OR ")
}

#[cfg(test)]
mod hybrid_tests {
    use super::*;
    use kl_core::Kind;

    fn fixture() -> Store {
        let store = Store::open(":memory:").expect("open");
        store.migrate().expect("migrate");
        store
    }

    #[test]
    fn recall_returns_remembered_fact_via_vector_or_like() {
        let store = fixture();
        store
            .remember(
                "auth",
                "Always verify JWT signatures before trusting claims",
            )
            .unwrap();
        let hits = store
            .recall("auth", "JWT signature verification", None, 5)
            .unwrap();
        assert!(
            hits.iter().any(|h| h.body.contains("JWT")),
            "expected JWT fact in hits: {hits:?}"
        );
    }

    #[test]
    fn recall_vector_finds_paraphrase_better_than_empty() {
        let store = fixture();
        store
            .remember("sec", "Rate-limit login endpoints to stop credential stuffing")
            .unwrap();
        store
            .remember("sec", "Use bcrypt with a work factor of at least 12")
            .unwrap();
        let hits = store
            .recall("sec", "throttle authentication attempts against stuffing", None, 3)
            .unwrap();
        assert!(!hits.is_empty());
        assert!(
            hits[0].body.to_lowercase().contains("rate")
                || hits[0].body.to_lowercase().contains("credential")
                || hits.iter().any(|h| h.body.contains("Rate-limit")),
            "unexpected ranking: {hits:?}"
        );
        let _ = Kind::Fact;
    }
}

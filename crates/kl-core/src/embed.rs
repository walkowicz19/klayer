//! Lightweight local embeddings: feature-hashing into a fixed 384-d unit
//! vector. No ONNX/model download — ships in every klayer binary so `recall`
//! and `search_code` can fuse FTS with vector hits out of the box. Quality is
//! below a MiniLM-class embedder; the `Embedder` trait is the swap point for
//! a heavier backend later (`embed-local`).

use crate::Embedder;
use anyhow::Result;

/// Dimension of every embedding produced by [`HashingEmbedder`] — must match
/// the `float[384]` vec0 tables in `kl-store`.
pub const EMBED_DIMS: usize = 384;

/// FNV-1a + signed feature hashing → L2-normalized 384-d vector.
#[derive(Debug, Default, Clone, Copy)]
pub struct HashingEmbedder;

impl Embedder for HashingEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let mut v = vec![0f32; EMBED_DIMS];
        let mut prev: Option<u64> = None;
        for tok in tokenize(text) {
            let h = fnv1a64(tok.as_bytes());
            accumulate(&mut v, h);
            if let Some(p) = prev {
                // Character-ngram-ish bigram of token hashes — cheap local context.
                accumulate(&mut v, p ^ h.rotate_left(13));
            }
            prev = Some(h);
        }
        if v.iter().all(|x| *x == 0.0) {
            // Empty / punctuation-only input: keep a tiny non-zero vector so
            // sqlite-vec MATCH does not reject an all-zero blob.
            v[0] = 1.0;
        }
        l2_normalize(&mut v);
        Ok(v)
    }

    fn dims(&self) -> usize {
        EMBED_DIMS
    }
}

impl HashingEmbedder {
    pub fn new() -> Self {
        Self
    }
}

/// Default shared embedder used by store/code paths.
pub fn default_embedder() -> std::sync::Arc<dyn Embedder> {
    std::sync::Arc::new(HashingEmbedder)
}

fn tokenize(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 2)
        .map(|t| t.to_lowercase())
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut h = OFFSET;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(PRIME);
    }
    h
}

fn accumulate(v: &mut [f32], h: u64) {
    let idx = (h as usize) % v.len();
    let sign = if (h & 1) == 0 { 1.0 } else { -1.0 };
    v[idx] += sign;
}

fn l2_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Cosine similarity for two equal-length L2-normalized (or raw) vectors.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0f64;
    let mut na = 0f64;
    let mut nb = 0f64;
    for i in 0..a.len() {
        let x = a[i] as f64;
        let y = b[i] as f64;
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom == 0.0 {
        0.0
    } else {
        dot / denom
    }
}

/// Reciprocal Rank Fusion over ranked id lists. `k` is the RRF constant (60 is
/// the classic default). Higher fused score = better.
pub fn rrf_fuse(ranked_lists: &[Vec<i64>], k: u32) -> Vec<(i64, f64)> {
    use std::collections::HashMap;
    let mut scores: HashMap<i64, f64> = HashMap::new();
    let kk = k as f64;
    for list in ranked_lists {
        for (rank, id) in list.iter().enumerate() {
            *scores.entry(*id).or_default() += 1.0 / (kk + rank as f64 + 1.0);
        }
    }
    let mut out: Vec<(i64, f64)> = scores.into_iter().collect();
    out.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashing_embedder_is_384d_and_unit_norm() {
        let e = HashingEmbedder;
        let v = e.embed("fn authenticate_user password hash").unwrap();
        assert_eq!(v.len(), EMBED_DIMS);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "norm={norm}");
    }

    #[test]
    fn similar_texts_rank_higher_than_unrelated() {
        let e = HashingEmbedder;
        let q = e.embed("JWT authentication middleware").unwrap();
        let near = e.embed("authenticate requests with JWT tokens").unwrap();
        let far = e.embed("css flexbox layout grid columns").unwrap();
        assert!(cosine_similarity(&q, &near) > cosine_similarity(&q, &far));
    }

    #[test]
    fn rrf_fuse_prefers_items_in_multiple_lists() {
        let fused = rrf_fuse(&[vec![1, 2, 3], vec![3, 4, 1]], 60);
        assert_eq!(fused[0].0, 1.min(3).max(1)); // 1 or 3 both appear twice; 1 ranks high in both
        let ids: Vec<i64> = fused.iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&1) && ids.contains(&3));
        // Item 1 is rank0 in list A and rank2 in list B; item 3 is rank2 in A and rank0 in B.
        // Equal RRF → either can win; both beat singletons 2 and 4.
        let score = |id: i64| fused.iter().find(|(i, _)| *i == id).unwrap().1;
        assert!(score(1) > score(2));
        assert!(score(3) > score(4));
    }
}

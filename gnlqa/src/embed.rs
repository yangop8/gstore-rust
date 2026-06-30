//! Embeddings + a brute-force vector index, used to link natural-language
//! mentions to KG entities/predicates/types by semantic similarity (C6).
//!
//! Two embedders ship: [`HashEmbedder`] (deterministic, offline — used by tests
//! and as a dependency-free baseline) and [`HttpEmbedder`] (an OpenAI-compatible
//! embeddings endpoint, e.g. Voyage/OpenAI/a local server). The query stack only
//! depends on the [`Embedder`] trait, so the backend is swappable.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::secret::Secret;

/// Anything that turns text into fixed-dimension vectors.
pub trait Embedder: Send + Sync {
    /// Embed a batch of texts (one vector each, all of length [`dim`](Self::dim)).
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    /// The embedding dimensionality.
    fn dim(&self) -> usize;

    /// Convenience: embed a single text.
    fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        let mut v = self.embed(std::slice::from_ref(&text.to_string()))?;
        v.pop().ok_or_else(|| Error::Llm("embedder returned no vector".into()))
    }
}

/// L2-normalize a vector in place (no-op for the zero vector).
pub fn normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Dot product (== cosine similarity for normalized vectors). The inputs must be
/// the same length (every caller is dim-guarded; checked in debug builds).
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "dot of mismatched-length vectors");
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// A deterministic, dependency-free embedder: a hashing bag-of-words vectorizer
/// (lowercased whitespace tokens hashed into `dim` buckets, then L2-normalized).
/// Weak semantically but stable and offline — good for tests and a baseline.
#[derive(Debug, Clone)]
pub struct HashEmbedder {
    dim: usize,
}

impl HashEmbedder {
    pub fn new(dim: usize) -> HashEmbedder {
        HashEmbedder { dim: dim.max(1) }
    }
    fn embed_one_vec(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0f32; self.dim];
        for tok in text.to_lowercase().split(|c: char| !c.is_alphanumeric()) {
            if tok.is_empty() {
                continue;
            }
            let mut h = DefaultHasher::new();
            tok.hash(&mut h);
            let idx = (h.finish() as usize) % self.dim;
            v[idx] += 1.0;
        }
        normalize(&mut v);
        v
    }
}

impl Default for HashEmbedder {
    fn default() -> Self {
        HashEmbedder::new(256)
    }
}

impl Embedder for HashEmbedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| self.embed_one_vec(t)).collect())
    }
    fn dim(&self) -> usize {
        self.dim
    }
}

/// An OpenAI-compatible embeddings client (`POST {base}/embeddings`,
/// `{model, input:[...]}` → `{data:[{embedding:[...]}]}`). Works with Voyage,
/// OpenAI, and local servers exposing that shape.
#[derive(Debug, Clone)]
pub struct HttpEmbedder {
    url: String,
    api_key: Secret,
    model: String,
    dim: usize,
    timeout: Duration,
}

impl HttpEmbedder {
    pub fn new(url: impl Into<String>, api_key: impl Into<String>, model: impl Into<String>, dim: usize) -> HttpEmbedder {
        HttpEmbedder {
            url: url.into(),
            api_key: Secret::new(api_key),
            model: model.into(),
            dim,
            timeout: Duration::from_secs(60),
        }
    }

    /// The request body (also used by tests).
    pub fn request_body(&self, texts: &[String]) -> Value {
        json!({ "model": self.model, "input": texts })
    }

    /// Parse `{data:[{embedding:[...],index?}]}` into vectors, in INPUT order.
    /// Honors the per-entry `index` field (OpenAI/Voyage include it and don't
    /// guarantee ordering); errors on non-numeric/non-finite components, missing
    /// embeddings, and duplicate/out-of-range indices. (Also used by tests.)
    pub fn parse_response(v: &Value) -> Result<Vec<Vec<f32>>> {
        let data = v["data"]
            .as_array()
            .ok_or_else(|| Error::Llm(format!("embeddings response has no data array: {}", snippet(v))))?;
        let n = data.len();
        let mut slots: Vec<Option<Vec<f32>>> = std::iter::repeat_with(|| None).take(n).collect();
        for (pos, d) in data.iter().enumerate() {
            let arr = d["embedding"]
                .as_array()
                .ok_or_else(|| Error::Llm("embeddings entry missing 'embedding'".into()))?;
            let mut vec = Vec::with_capacity(arr.len());
            for x in arr {
                let f = x
                    .as_f64()
                    .ok_or_else(|| Error::Llm("non-numeric embedding component".into()))?;
                if !f.is_finite() {
                    return Err(Error::Llm("non-finite embedding component".into()));
                }
                vec.push(f as f32);
            }
            let idx = match d.get("index").and_then(Value::as_u64) {
                Some(i) => i as usize,
                None => pos,
            };
            if idx >= n {
                return Err(Error::Llm(format!("embedding index {idx} out of range (n={n})")));
            }
            if slots[idx].is_some() {
                return Err(Error::Llm(format!("duplicate embedding index {idx}")));
            }
            slots[idx] = Some(vec);
        }
        slots
            .into_iter()
            .enumerate()
            .map(|(i, s)| s.ok_or_else(|| Error::Llm(format!("missing embedding for index {i}"))))
            .collect()
    }
}

impl Embedder for HttpEmbedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let resp = ureq::post(&self.url)
            .set("Authorization", &format!("Bearer {}", self.api_key.expose()))
            .set("content-type", "application/json")
            .timeout(self.timeout)
            .send_json(self.request_body(texts))
            .map_err(|e| match e {
                ureq::Error::Status(code, r) => {
                    Error::Http(format!("HTTP {code}: {}", r.into_string().unwrap_or_default()))
                }
                other => Error::Http(other.to_string()),
            })?;
        let v: Value = resp.into_json().map_err(|e| Error::Json(e.to_string()))?;
        let vecs = HttpEmbedder::parse_response(&v)?;
        if vecs.len() != texts.len() {
            return Err(Error::Llm(format!(
                "embedder returned {} vectors for {} inputs",
                vecs.len(),
                texts.len()
            )));
        }
        for vv in &vecs {
            if vv.len() != self.dim {
                return Err(Error::Llm(format!(
                    "embedding dim {} != configured {}",
                    vv.len(),
                    self.dim
                )));
            }
        }
        Ok(vecs)
    }
    fn dim(&self) -> usize {
        self.dim
    }
}

/// One scored search hit.
#[derive(Debug, Clone, PartialEq)]
pub struct Scored {
    pub id: String,
    pub text: String,
    pub score: f32,
}

/// A brute-force cosine-similarity vector index over `(id, text)` items.
/// Vectors are stored contiguously (one flat buffer) for cache-friendly scans;
/// adequate up to ~10⁵–10⁶ items on one machine. Swap for HNSW later.
#[derive(Debug, Clone, Default)]
pub struct VectorIndex {
    dim: usize,
    ids: Vec<String>,
    texts: Vec<String>,
    data: Vec<f32>, // ids.len() * dim, each row L2-normalized
}

impl VectorIndex {
    pub fn new(dim: usize) -> VectorIndex {
        VectorIndex { dim, ids: Vec::new(), texts: Vec::new(), data: Vec::new() }
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// The normalized vector of row `i`.
    fn row(&self, i: usize) -> &[f32] {
        &self.data[i * self.dim..(i + 1) * self.dim]
    }

    /// Add one already-embedded item (the vector is normalized on insert).
    pub fn add(&mut self, id: impl Into<String>, text: impl Into<String>, mut vec: Vec<f32>) -> Result<()> {
        if vec.len() != self.dim {
            return Err(Error::Llm(format!("vector dim {} != index dim {}", vec.len(), self.dim)));
        }
        normalize(&mut vec);
        self.ids.push(id.into());
        self.texts.push(text.into());
        self.data.extend_from_slice(&vec);
        Ok(())
    }

    /// Build an index by embedding `items` (`(id, text)`) with `embedder`,
    /// batched so a remote embedding endpoint isn't sent everything in one
    /// request.
    pub fn build(embedder: &dyn Embedder, items: &[(String, String)]) -> Result<VectorIndex> {
        const BATCH: usize = 512;
        let mut idx = VectorIndex::new(embedder.dim());
        for chunk in items.chunks(BATCH) {
            let texts: Vec<String> = chunk.iter().map(|(_, t)| t.clone()).collect();
            let vecs = embedder.embed(&texts)?;
            if vecs.len() != chunk.len() {
                return Err(Error::Llm(format!(
                    "embedder returned {} vectors for {} items",
                    vecs.len(),
                    chunk.len()
                )));
            }
            for ((id, text), v) in chunk.iter().zip(vecs) {
                idx.add(id.clone(), text.clone(), v)?;
            }
        }
        Ok(idx)
    }

    /// Top-`k` items by cosine similarity to `query`. O(N log k): scores are
    /// computed without cloning, the top-k selected with `select_nth`, and only
    /// the survivors materialized. Ties break by ascending id for determinism;
    /// non-finite scores sink to the bottom.
    pub fn search(&self, query: &[f32], k: usize) -> Vec<Scored> {
        if query.len() != self.dim || self.is_empty() || k == 0 {
            return Vec::new();
        }
        let mut q = query.to_vec();
        normalize(&mut q);
        let mut scored: Vec<(f32, usize)> = (0..self.ids.len())
            .map(|i| {
                let s = dot(&q, self.row(i));
                (if s.is_finite() { s } else { f32::NEG_INFINITY }, i)
            })
            .collect();

        // Order: score desc, then id asc.
        let cmp = |a: &(f32, usize), b: &(f32, usize)| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| self.ids[a.1].cmp(&self.ids[b.1]))
        };
        let k = k.min(scored.len());
        if k < scored.len() {
            scored.select_nth_unstable_by(k - 1, cmp);
            scored.truncate(k);
        }
        scored.sort_by(cmp);
        scored
            .into_iter()
            .map(|(score, i)| Scored { id: self.ids[i].clone(), text: self.texts[i].clone(), score })
            .collect()
    }

    /// Embed `query` with `embedder`, then [`search`](Self::search).
    pub fn search_text(&self, embedder: &dyn Embedder, query: &str, k: usize) -> Result<Vec<Scored>> {
        let q = embedder.embed_one(query)?;
        Ok(self.search(&q, k))
    }
}

/// Truncated debug rendering of a JSON value for error messages.
fn snippet(v: &Value) -> String {
    v.to_string().chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_embedder_is_deterministic_and_normalized() {
        let e = HashEmbedder::new(64);
        let a = e.embed_one("Berlin is in Germany").unwrap();
        let b = e.embed_one("Berlin is in Germany").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        let norm: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4);
    }

    #[test]
    fn dot_and_normalize() {
        let mut v = vec![3.0, 4.0];
        normalize(&mut v);
        assert!((v[0] - 0.6).abs() < 1e-6 && (v[1] - 0.8).abs() < 1e-6);
        assert!((dot(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn vector_index_finds_nearest() {
        let e = HashEmbedder::new(128);
        let items = vec![
            ("e1".to_string(), "capital of Germany Berlin".to_string()),
            ("e2".to_string(), "river Nile Egypt".to_string()),
            ("e3".to_string(), "mountain Everest Nepal".to_string()),
        ];
        let idx = VectorIndex::build(&e, &items).unwrap();
        assert_eq!(idx.len(), 3);
        let hits = idx.search_text(&e, "Berlin Germany capital", 2).unwrap();
        assert_eq!(hits[0].id, "e1"); // most similar
        assert!(hits[0].score >= hits[1].score);
    }

    #[test]
    fn exact_text_scores_near_one() {
        let e = HashEmbedder::new(256);
        let items = vec![("x".to_string(), "the quick brown fox".to_string())];
        let idx = VectorIndex::build(&e, &items).unwrap();
        let hits = idx.search_text(&e, "the quick brown fox", 1).unwrap();
        assert!((hits[0].score - 1.0).abs() < 1e-4);
    }

    #[test]
    fn dim_mismatch_is_rejected() {
        let mut idx = VectorIndex::new(4);
        assert!(idx.add("a", "a", vec![1.0, 2.0]).is_err());
        assert!(idx.search(&[1.0, 2.0], 1).is_empty());
    }

    #[test]
    fn http_embedder_body_and_parse() {
        let h = HttpEmbedder::new("http://x/embeddings", "k", "voyage-3", 3);
        let body = h.request_body(&["a".to_string(), "b".to_string()]);
        assert_eq!(body["model"], "voyage-3");
        assert_eq!(body["input"][1], "b");
        let resp = json!({"data": [{"embedding": [0.1, 0.2, 0.3]}, {"embedding": [1.0, 0.0, 0.0]}]});
        let vecs = HttpEmbedder::parse_response(&resp).unwrap();
        assert_eq!(vecs.len(), 2);
        assert_eq!(vecs[0], vec![0.1f32, 0.2, 0.3]);
    }

    #[test]
    fn parse_response_honors_index_and_rejects_bad() {
        // out-of-order data carrying `index` is restored to input order
        let resp = json!({"data": [{"embedding":[2.0],"index":1}, {"embedding":[1.0],"index":0}]});
        assert_eq!(HttpEmbedder::parse_response(&resp).unwrap(), vec![vec![1.0f32], vec![2.0f32]]);
        // non-numeric component, out-of-range index, non-finite → errors
        assert!(HttpEmbedder::parse_response(&json!({"data":[{"embedding":["x"]}]})).is_err());
        assert!(HttpEmbedder::parse_response(&json!({"data":[{"embedding":[1.0],"index":5}]})).is_err());
    }

    #[test]
    fn build_rejects_vector_count_mismatch() {
        struct BadEmbedder;
        impl Embedder for BadEmbedder {
            fn embed(&self, _t: &[String]) -> Result<Vec<Vec<f32>>> {
                Ok(vec![vec![1.0, 0.0]]) // always one vector
            }
            fn dim(&self) -> usize {
                2
            }
        }
        let items = vec![("a".to_string(), "a".to_string()), ("b".to_string(), "b".to_string())];
        assert!(VectorIndex::build(&BadEmbedder, &items).is_err());
    }

    #[test]
    fn search_k_larger_than_len_and_topk_order() {
        let e = HashEmbedder::new(64);
        let items: Vec<(String, String)> =
            (0..5).map(|i| (format!("e{i}"), format!("token{i} shared"))).collect();
        let idx = VectorIndex::build(&e, &items).unwrap();
        let all = idx.search_text(&e, "token2 shared", 99).unwrap(); // k > len
        assert_eq!(all.len(), 5);
        assert_eq!(all[0].id, "e2");
        // scores are monotonically non-increasing
        assert!(all.windows(2).all(|w| w[0].score >= w[1].score));
    }
}

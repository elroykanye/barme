//! The index socket. Stores vectors keyed by object id, scoped per tenant, and
//! answers nearest-neighbour queries. Tenant scoping is enforced here so search
//! can never cross a tenant boundary, the same wall as keyed dedup.
//!
//! `MemoryIndex` is the built-in default: a plain in-process map with cosine
//! similarity. Fine for dev and small deployments; users swap in an external
//! index (Qdrant and friends) by implementing this trait.

use crate::Result;
use async_trait::async_trait;
use barme_core::Hash;
use std::collections::HashMap;
use std::sync::Mutex;

/// A search hit: an object id and how close it scored (1.0 == identical).
#[derive(Debug, Clone, PartialEq)]
pub struct Match {
    pub id: Hash,
    pub score: f32,
}

#[async_trait]
pub trait VectorIndex: Send + Sync {
    /// Store (or replace) the vector for an object within a tenant.
    async fn upsert(&self, tenant: &str, id: Hash, vector: Vec<f32>) -> Result<()>;
    /// Top-k nearest vectors to `query` within a tenant, best first.
    async fn query(&self, tenant: &str, query: &[f32], k: usize) -> Result<Vec<Match>>;
}

#[derive(Default)]
pub struct MemoryIndex {
    // tenant -> (id -> vector)
    inner: Mutex<HashMap<String, HashMap<Hash, Vec<f32>>>>,
}

impl MemoryIndex {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl VectorIndex for MemoryIndex {
    async fn upsert(&self, tenant: &str, id: Hash, vector: Vec<f32>) -> Result<()> {
        let mut map = self.inner.lock().unwrap();
        map.entry(tenant.to_string())
            .or_default()
            .insert(id, vector);
        Ok(())
    }

    async fn query(&self, tenant: &str, query: &[f32], k: usize) -> Result<Vec<Match>> {
        let map = self.inner.lock().unwrap();
        let Some(vectors) = map.get(tenant) else {
            return Ok(vec![]);
        };
        let mut hits: Vec<Match> = vectors
            .iter()
            .map(|(id, v)| Match {
                id: *id,
                score: cosine(query, v),
            })
            .collect();
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        hits.truncate(k);
        Ok(hits)
    }
}

/// Cosine similarity. Zero-length vectors score 0 rather than NaN.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn nearest_vector_ranks_first() {
        let idx = MemoryIndex::new();
        let a = Hash::of(b"a");
        let b = Hash::of(b"b");
        idx.upsert("t", a, vec![1.0, 0.0]).await.unwrap();
        idx.upsert("t", b, vec![0.0, 1.0]).await.unwrap();

        let hits = idx.query("t", &[0.9, 0.1], 2).await.unwrap();
        assert_eq!(hits[0].id, a);
        assert_eq!(hits.len(), 2);
    }

    #[tokio::test]
    async fn tenants_are_isolated() {
        let idx = MemoryIndex::new();
        idx.upsert("t1", Hash::of(b"x"), vec![1.0, 0.0]).await.unwrap();
        assert!(idx.query("t2", &[1.0, 0.0], 5).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn upsert_replaces() {
        let idx = MemoryIndex::new();
        let id = Hash::of(b"x");
        idx.upsert("t", id, vec![1.0, 0.0]).await.unwrap();
        idx.upsert("t", id, vec![0.0, 1.0]).await.unwrap();
        let hits = idx.query("t", &[0.0, 1.0], 5).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].score > 0.99);
    }
}

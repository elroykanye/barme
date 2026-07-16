//! Orchestration: wires an [`Embedder`] to a [`VectorIndex`]. This is the part
//! Barme owns; the two halves are whatever the user plugged in.

use crate::{Embedder, Match, Result, VectorIndex};
use barme_core::Hash;

pub struct Semantic {
    embedder: Box<dyn Embedder>,
    index: Box<dyn VectorIndex>,
}

impl Semantic {
    pub fn new(embedder: Box<dyn Embedder>, index: Box<dyn VectorIndex>) -> Self {
        Semantic { embedder, index }
    }

    /// Embed an object and file it under its content hash. Called off the write
    /// path; keyed by id, so the same content is only ever embedded once.
    pub async fn understand(
        &self,
        tenant: &str,
        id: Hash,
        content_type: &str,
        bytes: &[u8],
    ) -> Result<()> {
        let vector = self.embedder.embed(content_type, bytes).await?;
        self.index.upsert(tenant, id, vector).await
    }

    /// Embed a query and return the nearest objects within a tenant.
    pub async fn search(
        &self,
        tenant: &str,
        query: &[u8],
        content_type: &str,
        k: usize,
    ) -> Result<Vec<Match>> {
        let vector = self.embedder.embed(content_type, query).await?;
        self.index.query(tenant, &vector, k).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryIndex;
    use async_trait::async_trait;

    /// A stand-in embedder: maps bytes to a tiny deterministic vector so the
    /// wiring can be tested without a model or a network.
    struct FakeEmbedder;

    #[async_trait]
    impl Embedder for FakeEmbedder {
        async fn embed(&self, _content_type: &str, bytes: &[u8]) -> Result<Vec<f32>> {
            let len = bytes.len() as f32;
            let sum: f32 = bytes.iter().map(|b| *b as f32).sum();
            Ok(vec![len, sum])
        }
    }

    #[tokio::test]
    async fn understand_then_search_finds_the_match() {
        let s = Semantic::new(Box::new(FakeEmbedder), Box::new(MemoryIndex::new()));
        let doc = b"a sunset over the water";
        let other = b"quarterly tax figures";

        let doc_id = Hash::of(doc);
        s.understand("t", doc_id, "text/plain", doc).await.unwrap();
        s.understand("t", Hash::of(other), "text/plain", other)
            .await
            .unwrap();

        // Querying with the exact bytes yields an identical vector, so that doc
        // scores top.
        let hits = s.search("t", doc, "text/plain", 2).await.unwrap();
        assert_eq!(hits[0].id, doc_id);
        assert!(hits[0].score > 0.99);
    }
}

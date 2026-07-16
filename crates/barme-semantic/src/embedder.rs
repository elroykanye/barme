//! The embedding socket. Barme hands an embedder some bytes and a content
//! type and gets back a vector; how that happens is the user's business.
//!
//! `HttpEmbedder` speaks a deliberately tiny contract so a user can point it at
//! anything by writing a thin shim in front of their model:
//!
//!   POST {url}
//!   -> { "model": "<name>", "content_type": "image/jpeg", "input_b64": "<..>" }
//!   <- { "embedding": [0.1, -0.2, ...] }
//!
//! Bytes are base64'd so the same request works for text and binary alike.
//!
//! The response may optionally carry `tags` and `text` alongside the embedding.
//! An endpoint that captions or OCRs an object returns them here; Barme is a
//! pure proxy and just stores whatever comes back. Endpoints that don't produce
//! them omit the fields and auto-tagging is silently skipped.

use crate::{Result, SemanticError};
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};

/// What an embedder returned for an object: the vector, plus any proxied tags
/// and free text. Tags/text are best-effort and usually empty.
#[derive(Debug, Clone, Default)]
pub struct Enrichment {
    pub vector: Vec<f32>,
    pub tags: Vec<String>,
    pub text: Option<String>,
}

#[async_trait]
pub trait Embedder: Send + Sync {
    /// Just the vector. Used on the query path.
    async fn embed(&self, content_type: &str, bytes: &[u8]) -> Result<Vec<f32>>;

    /// The vector plus any proxied tags/text. Default returns only the vector,
    /// so an embedder that has no enrichment need not implement this.
    async fn embed_rich(&self, content_type: &str, bytes: &[u8]) -> Result<Enrichment> {
        Ok(Enrichment {
            vector: self.embed(content_type, bytes).await?,
            tags: Vec::new(),
            text: None,
        })
    }
}

pub struct HttpEmbedder {
    client: reqwest::Client,
    url: String,
    model: String,
}

impl HttpEmbedder {
    pub fn new(url: impl Into<String>, model: impl Into<String>) -> Self {
        HttpEmbedder {
            client: reqwest::Client::new(),
            url: url.into(),
            model: model.into(),
        }
    }
}

#[derive(Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    content_type: &'a str,
    input_b64: String,
}

#[derive(Deserialize)]
struct EmbedResponse {
    embedding: Vec<f32>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    text: Option<String>,
}

impl HttpEmbedder {
    async fn request(&self, content_type: &str, bytes: &[u8]) -> Result<EmbedResponse> {
        let req = EmbedRequest {
            model: &self.model,
            content_type,
            input_b64: STANDARD.encode(bytes),
        };
        let resp: EmbedResponse = self
            .client
            .post(&self.url)
            .json(&req)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        if resp.embedding.is_empty() {
            return Err(SemanticError::EmptyEmbedding);
        }
        Ok(resp)
    }
}

#[async_trait]
impl Embedder for HttpEmbedder {
    async fn embed(&self, content_type: &str, bytes: &[u8]) -> Result<Vec<f32>> {
        Ok(self.request(content_type, bytes).await?.embedding)
    }

    async fn embed_rich(&self, content_type: &str, bytes: &[u8]) -> Result<Enrichment> {
        let resp = self.request(content_type, bytes).await?;
        Ok(Enrichment {
            vector: resp.embedding,
            tags: resp.tags,
            text: resp.text,
        })
    }
}

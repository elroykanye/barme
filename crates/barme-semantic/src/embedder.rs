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

use crate::{Result, SemanticError};
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};

#[async_trait]
pub trait Embedder: Send + Sync {
    async fn embed(&self, content_type: &str, bytes: &[u8]) -> Result<Vec<f32>>;
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
}

#[async_trait]
impl Embedder for HttpEmbedder {
    async fn embed(&self, content_type: &str, bytes: &[u8]) -> Result<Vec<f32>> {
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
        Ok(resp.embedding)
    }
}

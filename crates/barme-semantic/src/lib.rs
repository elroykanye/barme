//! Semantic layer: a vector index keyed by content hash.
//!
//! Barme owns the orchestration and stays out of the infra. Two sockets:
//! [`Embedder`] turns an object into a vector, [`VectorIndex`] stores and
//! searches vectors. Users bring whatever model and index they want and plug
//! them in; the shipped [`HttpEmbedder`] and [`MemoryIndex`] are just the
//! defaults.
//!
//! Everything here is derived and disposable: an embedding can always be
//! rebuilt from the stored bytes, so losing the index is a rebuild, not data
//! loss. Understanding runs off the write path; search is embed-then-query.

mod embedder;
mod index;
mod semantic;

pub use embedder::{Embedder, HttpEmbedder};
pub use index::{MemoryIndex, Match, VectorIndex};
pub use semantic::Semantic;

pub type Vector = Vec<f32>;

#[derive(Debug, thiserror::Error)]
pub enum SemanticError {
    #[error("embedding request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("embedder returned an empty vector")]
    EmptyEmbedding,
    #[error("index error: {0}")]
    Index(String),
}

pub type Result<T> = std::result::Result<T, SemanticError>;

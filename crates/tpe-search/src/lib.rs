//! Lexical and semantic search over the text that `tpe` extracted into its
//! ledger.
//!
//! The ledger is opened read-only. Page text is cut into overlapping,
//! sentence-aware word windows ([`Chunker`]), stored in a separate index
//! directory (`search.sqlite` with an FTS5 table for BM25 ranking, plus a
//! vector file), and queried lexically, semantically, or with reciprocal rank
//! fusion of both ([`SearchMode`]).
//!
//! Embedders: [`HashEmbedder`] (deterministic feature hashing, always
//! available, used by the tests) and `OnnxEmbedder` behind the `onnx`
//! feature. Vector stores: [`FlatStore`] (brute-force cosine, always
//! available) and `UsearchStore` behind the `usearch` feature.

#![allow(
    clippy::must_use_candidate,
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

pub mod chunker;
pub mod embed;
pub mod fusion;
pub mod index;
#[cfg(feature = "onnx")]
pub mod onnx;
mod permissions;
pub mod store;
#[cfg(feature = "usearch")]
pub mod usearch_store;

pub use chunker::{Chunk, Chunker};
pub use embed::{Embedder, HashEmbedder, cosine, l2_normalize};
pub use fusion::{RRF_K, reciprocal_rank_fusion};
pub use index::{Hit, IndexStats, IndexSummary, SearchIndex, SearchMode};
#[cfg(feature = "onnx")]
pub use onnx::OnnxEmbedder;
pub use store::{FlatStore, VectorStore};
#[cfg(feature = "usearch")]
pub use usearch_store::UsearchStore;

/// Errors raised while building or querying a search index.
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    /// An error from `SQLite` (ledger or index database).
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A filesystem error.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// The embedder failed.
    #[error("embedding failed: {0}")]
    Embed(String),
    /// The vector store failed or is not compiled in.
    #[error("vector store: {0}")]
    Store(String),
    /// A vector had the wrong number of dimensions.
    #[error("vector dimension mismatch: expected {expected}, found {found}")]
    DimensionMismatch { expected: usize, found: usize },
    /// The index was built with a different embedder than the one supplied.
    #[error("index was built with embedder `{index}` but `{given}` was supplied")]
    EmbedderMismatch { index: String, given: String },
    /// The linked `SQLite` has no FTS5 module.
    #[error("SQLite was built without FTS5: {0}")]
    Fts5Unavailable(String),
    /// An index file could not be decoded.
    #[error("corrupt index file: {0}")]
    Corrupt(String),
}

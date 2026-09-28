//! ONNX sentence embeddings via `fastembed` (feature `onnx`).
//!
//! The model is BGE-small-en-v1.5 (384 dimensions). `fastembed` downloads it
//! from Hugging Face on first use into its cache directory: the
//! `FASTEMBED_CACHE_DIR` environment variable, else `./.fastembed_cache`
//! (`fastembed-7.1.0/src/common.rs:21-22`); `HF_HOME`, when set, overrides
//! both (`common.rs`, `pull_from_hf`). Model files live under
//! `<cache>/models--Xenova--bge-small-en-v1.5/` (model code
//! `Xenova/bge-small-en-v1.5`, `src/models/text_embedding.rs:190-193`).
//!
//! API verified against `fastembed-7.1.0`: `TextInitOptions::new`,
//! `with_cache_dir`, `with_show_download_progress` (`src/init.rs:73,87,118`),
//! `TextEmbedding::try_new` (`src/text_embedding/impl.rs:32`) and
//! `TextEmbedding::embed(&mut self, texts, batch_size)` (`impl.rs:379`), which
//! needs `&mut self`, hence the `Mutex`.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};

use crate::SearchError;
use crate::embed::{Embedder, l2_normalize};

/// Output dimensions of BGE-small-en-v1.5.
pub const BGE_SMALL_DIM: usize = 384;
const MODEL_DIR: &str = "models--Xenova--bge-small-en-v1.5";

/// BGE-small-en-v1.5 embeddings through ONNX Runtime.
pub struct OnnxEmbedder {
    model: Mutex<TextEmbedding>,
}

impl OnnxEmbedder {
    /// Load (downloading if absent) the model from `cache_dir`.
    pub fn from_cache_dir(cache_dir: &Path) -> Result<Self, SearchError> {
        let options = TextInitOptions::new(EmbeddingModel::BGESmallENV15)
            .with_cache_dir(cache_dir.to_path_buf())
            .with_show_download_progress(false);
        let model =
            TextEmbedding::try_new(options).map_err(|e| SearchError::Embed(e.to_string()))?;
        Ok(Self {
            model: Mutex::new(model),
        })
    }

    /// Load the model from fastembed's default cache (`FASTEMBED_CACHE_DIR`).
    pub fn from_env() -> Result<Self, SearchError> {
        Self::from_cache_dir(&default_cache_dir())
    }

    /// Whether the model files are already present, so that loading will
    /// not need the network.
    pub fn model_cached(cache_dir: &Path) -> bool {
        let root =
            std::env::var_os("HF_HOME").map_or_else(|| cache_dir.to_path_buf(), PathBuf::from);
        root.join(MODEL_DIR).is_dir()
    }
}

/// fastembed's cache directory: `FASTEMBED_CACHE_DIR` or `.fastembed_cache`.
pub fn default_cache_dir() -> PathBuf {
    PathBuf::from(fastembed::get_cache_dir())
}

impl Embedder for OnnxEmbedder {
    fn dim(&self) -> usize {
        BGE_SMALL_DIM
    }

    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, SearchError> {
        let mut model = self
            .model
            .lock()
            .map_err(|_| SearchError::Embed("embedding model lock poisoned".to_string()))?;
        let mut vectors = model
            .embed(texts, None)
            .map_err(|e| SearchError::Embed(e.to_string()))?;
        for v in &mut vectors {
            if v.len() != BGE_SMALL_DIM {
                return Err(SearchError::DimensionMismatch {
                    expected: BGE_SMALL_DIM,
                    found: v.len(),
                });
            }
            l2_normalize(v);
        }
        Ok(vectors)
    }

    fn name(&self) -> String {
        "onnx-bge-small-en-v1.5".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::cosine;

    #[test]
    fn onnx_embedder_when_model_cached() {
        let dir = default_cache_dir();
        if !OnnxEmbedder::model_cached(&dir) {
            eprintln!("skipped: BGE-small-en-v1.5 not in {}", dir.display());
            return;
        }
        let e = OnnxEmbedder::from_cache_dir(&dir).unwrap();
        let v = e
            .embed(&[
                "Plants convert light into chemical energy.",
                "Photosynthesis happens in chloroplasts.",
                "The stock market fell sharply today.",
            ])
            .unwrap();
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].len(), BGE_SMALL_DIM);
        assert!(cosine(&v[0], &v[1]) > cosine(&v[0], &v[2]));
    }
}

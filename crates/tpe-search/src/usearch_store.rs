//! HNSW vector store backed by `usearch` (feature `usearch`).
//!
//! API verified against `usearch-2.26.2/rust/lib.rs`: `IndexOptions` fields
//! (line 370) and its `Default` (line 703), `Index::new` (1340), `add` (1507),
//! `search` returning `Matches { keys, distances }` (1449, struct at 329),
//! `reserve` (1555), `size`/`capacity` (1594/1599), `remove` (1626),
//! `contains` (1659), `save`/`load` (1714/1723). Public field access on
//! `IndexOptions`/`Matches` follows the crate's own doc example (lines 647, 662).

use std::path::Path;

use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

use crate::SearchError;
use crate::permissions::private_file;
use crate::store::VectorStore;

/// Approximate nearest-neighbour store (cosine metric, `f32` storage).
pub struct UsearchStore {
    index: Index,
    dim: usize,
}

fn store_err(e: &impl std::fmt::Display) -> SearchError {
    SearchError::Store(e.to_string())
}

fn path_str(path: &Path) -> Result<&str, SearchError> {
    path.to_str()
        .ok_or_else(|| SearchError::Store(format!("non-UTF-8 path {}", path.display())))
}

impl UsearchStore {
    /// An empty index for `dim`-dimensional vectors.
    pub fn new(dim: usize) -> Result<Self, SearchError> {
        let options = IndexOptions {
            dimensions: dim,
            metric: MetricKind::Cos,
            quantization: ScalarKind::F32,
            ..IndexOptions::default()
        };
        let index = Index::new(&options).map_err(|e| store_err(&e))?;
        Ok(Self { index, dim })
    }

    /// Load an index previously written by [`VectorStore::save`].
    pub fn load(path: &Path, dim: usize) -> Result<Self, SearchError> {
        let store = Self::new(dim)?;
        store
            .index
            .load(path_str(path)?)
            .map_err(|e| store_err(&e))?;
        let found = store.index.dimensions();
        if found != dim {
            return Err(SearchError::DimensionMismatch {
                expected: dim,
                found,
            });
        }
        Ok(store)
    }
}

impl VectorStore for UsearchStore {
    fn dim(&self) -> usize {
        self.dim
    }

    fn count(&self) -> usize {
        self.index.size()
    }

    fn add(&mut self, key: u64, vector: &[f32]) -> Result<(), SearchError> {
        if vector.len() != self.dim {
            return Err(SearchError::DimensionMismatch {
                expected: self.dim,
                found: vector.len(),
            });
        }
        if self.index.contains(key) {
            self.index.remove(key).map_err(|e| store_err(&e))?;
        }
        if self.index.size() >= self.index.capacity() {
            let target = (self.index.capacity() * 2).max(64);
            self.index.reserve(target).map_err(|e| store_err(&e))?;
        }
        self.index.add(key, vector).map_err(|e| store_err(&e))
    }

    fn remove(&mut self, key: u64) -> Result<(), SearchError> {
        if self.index.contains(key) {
            self.index.remove(key).map_err(|e| store_err(&e))?;
        }
        Ok(())
    }

    fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u64, f32)>, SearchError> {
        if query.len() != self.dim {
            return Err(SearchError::DimensionMismatch {
                expected: self.dim,
                found: query.len(),
            });
        }
        if k == 0 || self.index.size() == 0 {
            return Ok(Vec::new());
        }
        let matches = self.index.search(query, k).map_err(|e| store_err(&e))?;
        Ok(matches
            .keys
            .iter()
            .zip(matches.distances.iter())
            .map(|(key, distance)| (*key, 1.0 - *distance))
            .collect())
    }

    fn save(&self, path: &Path) -> Result<(), SearchError> {
        let tmp = path.with_extension("tmp");
        // Ensure usearch writes into an owner-only file rather than creating
        // one according to the process's ambient umask.
        drop(private_file(&tmp, true)?);
        self.index
            .save(path_str(&tmp)?)
            .map_err(|e| store_err(&e))?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usearch_store_top1_and_round_trip() {
        let mut s = UsearchStore::new(3).unwrap();
        s.add(1, &[1.0, 0.0, 0.0]).unwrap();
        s.add(2, &[0.0, 1.0, 0.0]).unwrap();
        s.add(3, &[0.0, 0.0, 1.0]).unwrap();
        assert_eq!(s.count(), 3);
        let hits = s.search(&[0.1, 0.9, 0.0], 1).unwrap();
        assert_eq!(hits[0].0, 2);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vectors.usearch");
        s.save(&path).unwrap();
        let loaded = UsearchStore::load(&path, 3).unwrap();
        assert_eq!(loaded.count(), 3);
        assert_eq!(loaded.search(&[0.0, 0.0, 2.0], 1).unwrap()[0].0, 3);
    }
}

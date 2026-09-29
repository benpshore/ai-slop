//! Vector stores: the [`VectorStore`] trait and the always-available
//! brute-force [`FlatStore`].

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::SearchError;
use crate::embed::l2_normalize;
use crate::permissions::private_file;

/// A keyed collection of vectors searchable by cosine similarity.
pub trait VectorStore {
    /// Number of dimensions every vector must have.
    fn dim(&self) -> usize;
    /// Number of stored vectors.
    fn count(&self) -> usize;
    /// Insert or replace the vector stored under `key`.
    fn add(&mut self, key: u64, vector: &[f32]) -> Result<(), SearchError>;
    /// Remove `key`; removing an absent key is not an error.
    fn remove(&mut self, key: u64) -> Result<(), SearchError>;
    /// The `k` most similar keys with their cosine similarity, best first.
    fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u64, f32)>, SearchError>;
    /// Persist the store to `path` (replacing any existing file).
    fn save(&self, path: &Path) -> Result<(), SearchError>;
}

const FLAT_MAGIC: &[u8; 8] = b"TPEFLAT1";
const FLAT_HEADER: usize = 8 + 4 + 8;

/// Brute-force cosine store. Vectors are normalised on insert, so a search
/// is one dot product per stored vector.
///
/// File format (little-endian): magic `TPEFLAT1`, `u32` dimensions, `u64`
/// count, then per vector a `u64` key followed by `dim` `f32` values.
#[derive(Clone, Debug, Default)]
pub struct FlatStore {
    dim: usize,
    items: Vec<(u64, Vec<f32>)>,
    positions: HashMap<u64, usize>,
}

impl FlatStore {
    /// An empty store for `dim`-dimensional vectors.
    pub fn new(dim: usize) -> Self {
        Self {
            dim,
            items: Vec::new(),
            positions: HashMap::new(),
        }
    }

    /// Read a store written by [`VectorStore::save`].
    pub fn load(path: &Path) -> Result<Self, SearchError> {
        let bytes = fs::read(path)?;
        Self::from_bytes(&bytes)
    }

    /// Decode the binary file format.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, SearchError> {
        if bytes.len() < FLAT_HEADER || !bytes.starts_with(FLAT_MAGIC) {
            return Err(SearchError::Corrupt("missing TPEFLAT1 header".to_string()));
        }
        let dim_u32 = u32::from_le_bytes(read_array::<4>(bytes, 8)?);
        let count_u64 = u64::from_le_bytes(read_array::<8>(bytes, 12)?);
        let dim = usize::try_from(dim_u32).map_err(|e| SearchError::Corrupt(e.to_string()))?;
        let count = usize::try_from(count_u64).map_err(|e| SearchError::Corrupt(e.to_string()))?;
        let record = dim
            .checked_mul(4)
            .and_then(|n| n.checked_add(8))
            .ok_or_else(|| SearchError::Corrupt("dimension overflow".to_string()))?;
        let expected = record
            .checked_mul(count)
            .and_then(|n| n.checked_add(FLAT_HEADER))
            .ok_or_else(|| SearchError::Corrupt("count overflow".to_string()))?;
        if bytes.len() != expected {
            return Err(SearchError::Corrupt(format!(
                "expected {expected} bytes, found {}",
                bytes.len()
            )));
        }
        let mut store = Self::new(dim);
        let mut at = FLAT_HEADER;
        for _ in 0..count {
            let key = u64::from_le_bytes(read_array::<8>(bytes, at)?);
            at += 8;
            let mut vector: Vec<f32> = Vec::with_capacity(dim);
            for _ in 0..dim {
                vector.push(f32::from_le_bytes(read_array::<4>(bytes, at)?));
                at += 4;
            }
            store.insert_normalised(key, vector);
        }
        Ok(store)
    }

    /// Encode the binary file format.
    pub fn to_bytes(&self) -> Result<Vec<u8>, SearchError> {
        let dim = u32::try_from(self.dim).map_err(|e| SearchError::Store(e.to_string()))?;
        let count =
            u64::try_from(self.items.len()).map_err(|e| SearchError::Store(e.to_string()))?;
        let mut out: Vec<u8> =
            Vec::with_capacity(FLAT_HEADER + self.items.len() * (8 + 4 * self.dim));
        out.extend_from_slice(FLAT_MAGIC);
        out.extend_from_slice(&dim.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        for (key, vector) in &self.items {
            out.extend_from_slice(&key.to_le_bytes());
            for x in vector {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
        Ok(out)
    }

    fn insert_normalised(&mut self, key: u64, vector: Vec<f32>) {
        if let Some(&pos) = self.positions.get(&key) {
            self.items[pos].1 = vector;
        } else {
            self.positions.insert(key, self.items.len());
            self.items.push((key, vector));
        }
    }
}

fn read_array<const N: usize>(bytes: &[u8], at: usize) -> Result<[u8; N], SearchError> {
    bytes
        .get(at..at + N)
        .and_then(|s| <[u8; N]>::try_from(s).ok())
        .ok_or_else(|| SearchError::Corrupt(format!("truncated at byte {at}")))
}

impl VectorStore for FlatStore {
    fn dim(&self) -> usize {
        self.dim
    }

    fn count(&self) -> usize {
        self.items.len()
    }

    fn add(&mut self, key: u64, vector: &[f32]) -> Result<(), SearchError> {
        if vector.len() != self.dim {
            return Err(SearchError::DimensionMismatch {
                expected: self.dim,
                found: vector.len(),
            });
        }
        let mut owned = vector.to_vec();
        l2_normalize(&mut owned);
        self.insert_normalised(key, owned);
        Ok(())
    }

    fn remove(&mut self, key: u64) -> Result<(), SearchError> {
        if let Some(pos) = self.positions.remove(&key) {
            self.items.swap_remove(pos);
            if let Some((moved, _)) = self.items.get(pos) {
                self.positions.insert(*moved, pos);
            }
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
        let mut q = query.to_vec();
        l2_normalize(&mut q);
        let mut scored: Vec<(u64, f32)> = self
            .items
            .iter()
            .map(|(key, v)| (*key, v.iter().zip(&q).map(|(a, b)| a * b).sum::<f32>()))
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        scored.truncate(k);
        Ok(scored)
    }

    fn save(&self, path: &Path) -> Result<(), SearchError> {
        let bytes = self.to_bytes()?;
        let tmp = path.with_extension("tmp");
        let mut file = private_file(&tmp, true)?;
        std::io::Write::write_all(&mut file, &bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    const EPS: f32 = 1e-5;

    fn sample() -> FlatStore {
        let mut s = FlatStore::new(3);
        s.add(1, &[1.0, 0.0, 0.0]).unwrap();
        s.add(2, &[0.0, 1.0, 0.0]).unwrap();
        s.add(3, &[1.0, 1.0, 0.0]).unwrap();
        s.add(4, &[0.0, 0.0, 5.0]).unwrap();
        s
    }

    #[test]
    fn flat_store_top_k() {
        let s = sample();
        let hits = s.search(&[2.0, 0.1, 0.0], 2).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, 1);
        assert_eq!(hits[1].0, 3);
        assert!(hits[0].1 > hits[1].1);
        assert!(hits[0].1 <= 1.0 + EPS);
        let all = s.search(&[0.0, 0.0, 1.0], 10).unwrap();
        assert_eq!(all.len(), 4);
        assert_eq!(all[0].0, 4);
        assert!((all[0].1 - 1.0).abs() < EPS);
    }

    #[test]
    fn flat_store_rejects_wrong_dimension() {
        let mut s = sample();
        assert!(matches!(
            s.add(9, &[1.0]),
            Err(SearchError::DimensionMismatch {
                expected: 3,
                found: 1
            })
        ));
        assert!(s.search(&[1.0, 2.0], 1).is_err());
    }

    #[test]
    fn flat_store_replace_and_remove() {
        let mut s = sample();
        s.add(1, &[0.0, 1.0, 0.0]).unwrap();
        assert_eq!(s.count(), 4);
        s.remove(2).unwrap();
        s.remove(42).unwrap();
        assert_eq!(s.count(), 3);
        let hits = s.search(&[0.0, 1.0, 0.0], 1).unwrap();
        assert_eq!(hits[0].0, 1);
        // Key 4 was moved by swap_remove; it must still be addressable.
        s.remove(4).unwrap();
        assert_eq!(s.count(), 2);
        assert!(
            s.search(&[0.0, 0.0, 1.0], 5)
                .unwrap()
                .iter()
                .all(|h| h.0 != 4)
        );
    }

    #[test]
    fn flat_store_round_trips_through_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vectors.flat");
        let s = sample();
        s.save(&path).unwrap();
        let loaded = FlatStore::load(&path).unwrap();
        assert_eq!(loaded.dim(), 3);
        assert_eq!(loaded.count(), 4);
        assert_eq!(
            loaded.search(&[1.0, 1.0, 0.0], 1).unwrap()[0].0,
            s.search(&[1.0, 1.0, 0.0], 1).unwrap()[0].0
        );
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn corrupt_bytes_are_rejected() {
        assert!(FlatStore::from_bytes(b"nope").is_err());
        let mut bytes = sample().to_bytes().unwrap();
        bytes.pop();
        assert!(matches!(
            FlatStore::from_bytes(&bytes),
            Err(SearchError::Corrupt(_))
        ));
    }
}

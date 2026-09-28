//! Text embedders: the [`Embedder`] trait and the always-available
//! [`HashEmbedder`].

use crate::SearchError;

/// Turns texts into fixed-size vectors for semantic search.
pub trait Embedder {
    /// Number of dimensions of every returned vector.
    fn dim(&self) -> usize;
    /// Embed each text; the result has one vector per input, in order.
    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, SearchError>;
    /// Stable identifier recorded in the index so that queries use the same
    /// embedder that built it (for example `hash-256`).
    fn name(&self) -> String;
}

/// Deterministic feature-hashing embedder: lower-cased alphanumeric word
/// unigrams and bigrams are hashed (64-bit `FNV-1a`) into `dim` signed buckets
/// and the vector is L2-normalised. It captures word overlap, not meaning,
/// but needs no model files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HashEmbedder {
    dim: usize,
}

impl Default for HashEmbedder {
    fn default() -> Self {
        Self { dim: 256 }
    }
}

impl HashEmbedder {
    /// An embedder with `dim` dimensions (at least 1).
    pub fn new(dim: usize) -> Self {
        Self { dim: dim.max(1) }
    }

    /// Embed one text.
    pub fn embed_one(&self, text: &str) -> Vec<f32> {
        let mut vector = vec![0.0_f32; self.dim];
        let tokens: Vec<String> = text
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(str::to_lowercase)
            .collect();
        for token in &tokens {
            self.add_feature(&mut vector, token);
        }
        for pair in tokens.windows(2) {
            let bigram = format!("{} {}", pair[0], pair[1]);
            self.add_feature(&mut vector, &bigram);
        }
        l2_normalize(&mut vector);
        vector
    }

    fn add_feature(&self, vector: &mut [f32], feature: &str) {
        let hash = fnv1a(feature.as_bytes());
        let dim = u64::try_from(self.dim).unwrap_or(u64::MAX);
        let bucket = usize::try_from(hash % dim).unwrap_or(0);
        let sign = if hash >> 63 == 0 { 1.0_f32 } else { -1.0_f32 };
        if let Some(slot) = vector.get_mut(bucket) {
            *slot += sign;
        }
    }
}

impl Embedder for HashEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, SearchError> {
        Ok(texts.iter().map(|t| self.embed_one(t)).collect())
    }

    fn name(&self) -> String {
        format!("hash-{}", self.dim)
    }
}

/// 64-bit `FNV-1a`; fixed constants, so results never change between runs,
/// platforms or Rust releases (unlike `std`'s `DefaultHasher`).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Scale `vector` to unit length in place; a zero vector is left as is.
pub fn l2_normalize(vector: &mut [f32]) {
    let norm = euclidean_norm(vector);
    if norm > 0.0 {
        for x in &mut *vector {
            *x /= norm;
        }
    }
}

/// Cosine similarity; 0 when the lengths differ or either vector is zero.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norms = euclidean_norm(a) * euclidean_norm(b);
    if norms > 0.0 { dot / norms } else { 0.0 }
}

fn euclidean_norm(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f32 = 1e-5;

    #[test]
    fn fnv_known_values() {
        // Reference values of 64-bit `FNV-1a`.
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn hash_embedder_is_deterministic_and_normalised() {
        let a = HashEmbedder::default();
        let b = HashEmbedder::new(256);
        let text = "Graphene has remarkable electronic properties.";
        let va = a.embed_one(text);
        let vb = b.embed(&[text]).unwrap().remove(0);
        assert_eq!(va, vb);
        assert_eq!(va.len(), 256);
        let norm: f32 = va.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < EPS);
        assert_eq!(a.name(), "hash-256");
    }

    #[test]
    fn hash_embedder_cosine_sanity() {
        let e = HashEmbedder::default();
        let q = e.embed_one("photosynthesis in green plant leaves");
        let near = e.embed_one("Green plant leaves perform photosynthesis using light.");
        let far = e.embed_one("Volcanic eruptions eject magma and ash into the atmosphere.");
        assert!((cosine(&q, &q) - 1.0).abs() < EPS);
        assert!(cosine(&q, &near) > cosine(&q, &far));
        assert!(cosine(&q, &near) > 0.3);
    }

    #[test]
    fn empty_text_gives_zero_vector() {
        let e = HashEmbedder::new(8);
        let v = e.embed_one(" ... ");
        assert!(v.iter().all(|x| x.abs() < EPS));
        assert!(cosine(&v, &v).abs() < EPS);
    }

    #[test]
    fn cosine_length_mismatch_is_zero() {
        assert!(cosine(&[1.0, 0.0], &[1.0]).abs() < EPS);
    }
}

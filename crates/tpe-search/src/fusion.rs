//! Weighted reciprocal rank fusion (Cormack, Clarke and Buettcher, 2009).

use std::collections::{HashMap, HashSet};

/// The usual RRF damping constant.
pub const RRF_K: f64 = 60.0;

/// Fuse ranked key lists. Each list contributes `weight / (k_const + rank)`
/// for every key it contains (rank starts at 1; only a key's first
/// occurrence in a list counts). Returns keys by fused score, best first;
/// ties are broken by ascending key so the order is deterministic.
pub fn reciprocal_rank_fusion(lists: &[(f64, &[u64])], k_const: f64) -> Vec<(u64, f64)> {
    let mut scores: HashMap<u64, f64> = HashMap::new();
    for (weight, keys) in lists {
        let mut seen: HashSet<u64> = HashSet::new();
        let mut rank = 0.0_f64;
        for key in *keys {
            if !seen.insert(*key) {
                continue;
            }
            rank += 1.0;
            *scores.entry(*key).or_insert(0.0) += weight / (k_const + rank);
        }
    }
    let mut fused: Vec<(u64, f64)> = scores.into_iter().collect();
    fused.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    fused
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(fused: &[(u64, f64)]) -> Vec<u64> {
        fused.iter().map(|(k, _)| *k).collect()
    }

    #[test]
    fn rrf_rewards_agreement() {
        let a: &[u64] = &[1, 2, 3];
        let b: &[u64] = &[2, 4, 5];
        let fused = reciprocal_rank_fusion(&[(1.0, a), (1.0, b)], RRF_K);
        // 2: 1/62 + 1/61; 1: 1/61; 4: 1/62; 3 and 5: 1/63 (tie -> key order).
        assert_eq!(keys(&fused), vec![2, 1, 4, 3, 5]);
        let expected = 1.0 / 62.0 + 1.0 / 61.0;
        assert!((fused[0].1 - expected).abs() < 1e-12);
    }

    #[test]
    fn rrf_weight_zero_ignores_a_list() {
        let a: &[u64] = &[7, 8, 9];
        let b: &[u64] = &[9, 8, 7];
        let fused = reciprocal_rank_fusion(&[(0.0, a), (1.0, b)], RRF_K);
        assert_eq!(keys(&fused), vec![9, 8, 7]);
        let fused = reciprocal_rank_fusion(&[(1.0, a), (0.0, b)], RRF_K);
        assert_eq!(keys(&fused), vec![7, 8, 9]);
    }

    #[test]
    fn rrf_counts_duplicates_once() {
        let a: &[u64] = &[1, 1, 2];
        let fused = reciprocal_rank_fusion(&[(1.0, a)], RRF_K);
        assert_eq!(keys(&fused), vec![1, 2]);
        assert!((fused[1].1 - 1.0 / 62.0).abs() < 1e-12);
    }

    #[test]
    fn rrf_empty() {
        assert!(reciprocal_rank_fusion(&[], RRF_K).is_empty());
    }
}

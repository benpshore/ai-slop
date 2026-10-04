//! Font advance lookup, independent of PDF object traversal and font decoding.
//!
//! The parent resolves `/Widths`, `/W`, defaults, and Type3 scaling into owned
//! values. Simple fonts index their widths directly. Composite fonts normalize
//! overlapping CID ranges once, preserving the first original match, then use
//! binary search for every glyph. No width is expanded into individual CIDs.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use super::{DEFAULT_WIDTH, THOUSANDTH};

/// Glyph widths of a simple (single-byte) font.
pub(super) struct SimpleWidths {
    pub(super) first_char: u32,
    /// `/Widths`, in glyph space.
    pub(super) widths: Vec<f32>,
    /// `/MissingWidth`, in glyph space.
    pub(super) missing: Option<f32>,
    /// Glyph space to text space: 1/1000 for `Type1`/`TrueType`, the horizontal
    /// scale of `/FontMatrix` for Type3.
    pub(super) glyph_scale: f32,
}

impl SimpleWidths {
    pub(super) fn unknown() -> Self {
        Self {
            first_char: 0,
            widths: Vec::new(),
            missing: None,
            glyph_scale: THOUSANDTH,
        }
    }

    /// Advance of `code` in text space (1.0 = the font size).
    fn width(&self, code: u32) -> f32 {
        let fallback = match self.missing {
            Some(missing) => missing * self.glyph_scale,
            None => DEFAULT_WIDTH * THOUSANDTH,
        };
        let Some(offset) = code.checked_sub(self.first_char) else {
            return fallback;
        };
        let Ok(index) = usize::try_from(offset) else {
            return fallback;
        };
        match self.widths.get(index) {
            Some(glyph_width) => glyph_width * self.glyph_scale,
            None => fallback,
        }
    }
}

/// Glyph widths of a composite (Type0) font, keyed by CID.
pub(super) struct CompositeWidths {
    /// Sorted, disjoint, inclusive runs. Construction resolves `/W` order once,
    /// so lookup never falls back to scanning the original overlapping array.
    ranges: Vec<(u32, u32, f32)>,
    default_width: f32,
}

impl CompositeWidths {
    pub(super) fn new(ranges: &[(u32, u32, f32)], default_width: f32) -> Self {
        Self {
            ranges: normalize_ranges(ranges),
            default_width,
        }
    }

    /// Advance of `cid` in text space (1.0 = the font size).
    fn width(&self, cid: u32) -> f32 {
        let after = self.ranges.partition_point(|&(first, _, _)| first <= cid);
        if let Some(&(_, last, glyph_width)) = after
            .checked_sub(1)
            .and_then(|index| self.ranges.get(index))
            && cid <= last
        {
            return glyph_width * THOUSANDTH;
        }
        self.default_width * THOUSANDTH
    }
}

/// Resolve overlaps with the first matching original `/W` entry winning.
///
/// Between consecutive endpoints the set of covering runs cannot change. Its
/// lowest original index therefore supplies exactly the width a linear scan
/// would find for every CID in that interval. Empty runs supply no endpoints.
fn normalize_ranges(ranges: &[(u32, u32, f32)]) -> Vec<(u32, u32, f32)> {
    let valid = ranges
        .iter()
        .filter(|&&(first, last, _)| first <= last)
        .count();
    // Two endpoints per valid run, at most 2n-1 output intervals, and one heap
    // entry per run. Reserve these bounds once: the sweep is O(n log n) time
    // and O(n) space, with no transient reallocations hidden in resource charges.
    let mut endpoints = Vec::with_capacity(valid * 2);
    for (index, &(first, last, _)) in ranges.iter().enumerate() {
        if first <= last {
            endpoints.push((u64::from(first), index));
            // Half-open endpoints need one extra bit to represent u32::MAX+1.
            // Saturation here would incorrectly lose the final CID.
            endpoints.push((u64::from(last) + 1, index));
        }
    }
    endpoints.sort_unstable_by_key(|&(position, _)| position);
    let mut active = BinaryHeap::<Reverse<usize>>::with_capacity(valid);
    let mut normalized = Vec::with_capacity((valid * 2).saturating_sub(1));
    let mut previous = 0;
    for group in endpoints.chunk_by(|a, b| a.0 == b.0) {
        let position = group[0].0;
        if let Some(&Reverse(index)) = active.peek() {
            // The heap winner was valid at `previous`, and no boundary lies
            // inside this interval. Both inclusive bounds fit u32, even when
            // the current endpoint is the extra u32::MAX+1 sentinel.
            normalized.push((previous as u32, (position - 1) as u32, ranges[index].2));
        }
        for &(at, index) in group {
            // An endpoint's original index identifies its kind without another
            // allocation: a nonempty run starts exactly at its first CID.
            if at == u64::from(ranges[index].0) {
                active.push(Reverse(index));
            }
        }
        // Expired lower-priority entries may stay buried. They cannot affect
        // the minimum; remove each only when it reaches the top. Every run is
        // pushed and popped at most once, even with completely nested ranges.
        while active
            .peek()
            .is_some_and(|&Reverse(index)| u64::from(ranges[index].1) < position)
        {
            active.pop();
        }
        previous = position;
    }
    // Do not merge equal floating-point widths: copying the winning value
    // preserves NaN payloads and signed zero without choosing equality rules.
    normalized
}

pub(super) enum Widths {
    Simple(SimpleWidths),
    Composite(CompositeWidths),
}

impl Widths {
    /// Advance of `code` in text space (1.0 = the font size).
    pub(super) fn text_width(&self, code: u32) -> f32 {
        match self {
            Self::Simple(simple) => simple.width(code),
            Self::Composite(composite) => composite.width(code),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(actual: f32, expected: f32) -> bool {
        (actual - expected).abs() < 1e-3
    }

    #[test]
    fn composite_widths_lookup_matches_first_match_scan() {
        let ranges = vec![
            (10, 10, 300.0),
            (1, 3, 100.0),
            (20, 25, 700.0),
            (5, 4, 999.0),
        ];
        let widths = CompositeWidths::new(&ranges, 1000.0);
        assert!(close(widths.width(2), 0.1));
        assert!(close(widths.width(10), 0.3));
        assert!(close(widths.width(25), 0.7));
        assert!(close(widths.width(0), 1.0));
        assert!(close(widths.width(4), 1.0));
        assert!(close(widths.width(11), 1.0));
        assert!(close(widths.width(26), 1.0));

        let overlapping = CompositeWidths::new(&[(1, 10, 100.0), (5, 5, 900.0)], 500.0);
        assert!(close(overlapping.width(5), 0.1));
        assert!(close(overlapping.width(11), 0.5));
    }

    /// Keep the original linear definition as an oracle: it depends on input
    /// order and contains no sweep or binary-search machinery.
    fn first_width(ranges: &[(u32, u32, f32)], cid: u32, fallback: f32) -> f32 {
        ranges
            .iter()
            .find(|&&(first, last, _)| (first..=last).contains(&cid))
            .map_or(fallback, |&(_, _, width)| width)
    }

    fn assert_matches(ranges: &[(u32, u32, f32)], fallback: f32, queries: &[u32]) {
        let normalized = CompositeWidths::new(ranges, fallback);
        assert!(
            normalized
                .ranges
                .iter()
                .all(|&(first, last, _)| first <= last)
        );
        assert!(
            normalized
                .ranges
                .windows(2)
                .all(|pair| pair[0].1 < pair[1].0)
        );
        assert!(normalized.ranges.len() <= (ranges.len() * 2).saturating_sub(1));
        for &cid in queries {
            let expected = first_width(ranges, cid, fallback);
            let raw_actual = first_width(&normalized.ranges, cid, fallback);
            // Compare stored bits before arithmetic: NaN payloads and signed
            // zero must survive selecting and splitting a winning range.
            assert_eq!(raw_actual.to_bits(), expected.to_bits(), "CID {cid}");
            let actual = normalized.width(cid);
            let expected = expected * THOUSANDTH;
            if expected.is_nan() {
                assert!(actual.is_nan(), "CID {cid}");
            } else {
                assert_eq!(actual.to_bits(), expected.to_bits(), "CID {cid}");
            }
        }
    }

    #[test]
    fn overlapping_widths_keep_input_precedence_across_every_boundary() {
        let queries: Vec<_> = (0..=40).collect();
        assert_matches(
            &[
                (10, 20, 100.0),
                (5, 15, 200.0),
                (20, 30, 300.0),
                (0, 40, 400.0),
                (10, 20, 500.0),
                (12, 18, 600.0),
                (32, 31, 700.0),
            ],
            1000.0,
            &queries,
        );
        // Expired low-priority runs stay buried beneath the first run; when it
        // ends, all expired entries must be discarded before selecting a width.
        assert_matches(
            &[(0, 20, 100.0), (2, 3, 200.0), (4, 5, 300.0), (6, 30, 400.0)],
            1000.0,
            &queries,
        );
    }

    #[test]
    fn maximum_cid_empty_runs_and_gaps_preserve_defaults() {
        let queries = [0, 1, 2, 3, u32::MAX - 2, u32::MAX - 1, u32::MAX];
        for fallback in [0.0, -0.0, 1000.0, f32::from_bits(0x7fc0_1234)] {
            assert_matches(&[], fallback, &queries);
            assert_matches(&[(1, 0, 700.0), (u32::MAX, 0, 900.0)], fallback, &queries);
            assert_matches(
                &[(u32::MAX, u32::MAX, 100.0), (0, u32::MAX, 200.0)],
                fallback,
                &queries,
            );
            assert_matches(
                &[(2, 2, 300.0), (u32::MAX - 1, u32::MAX, 400.0)],
                fallback,
                &queries,
            );
        }
    }

    #[test]
    fn normalization_preserves_nan_payloads_and_signed_zero() {
        assert_matches(
            &[
                (1, 2, f32::from_bits(0x7fc0_1234)),
                (3, 3, -0.0),
                (4, 4, 0.0),
                (5, 5, f32::from_bits(0xffc0_5678)),
                (0, 6, f32::INFINITY),
                (0, 6, f32::NEG_INFINITY),
            ],
            -0.0,
            &[0, 1, 2, 3, 4, 5, 6, 7],
        );
    }

    #[test]
    fn randomized_ranges_match_the_original_linear_oracle() {
        let mut state = 0x57ac_30de_17b9_40f2_u64;
        let mut next = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            (state >> 32) as u32
        };
        for case in 0..256 {
            let mut ranges = Vec::new();
            let mut queries = vec![0, u32::MAX];
            for _ in 0..case % 97 {
                let first = next() % 256;
                let last = next() % 256;
                let (first, last) = if next() % 4 == 0 {
                    (u32::MAX - first, u32::MAX - last)
                } else {
                    (first, last)
                };
                ranges.push((first, last, f32::from_bits(next())));
                // Check both sides of every endpoint, including degenerate
                // runs, to detect off-by-one errors at touching intervals.
                for cid in [first, last] {
                    queries.extend([cid.saturating_sub(1), cid, cid.saturating_add(1)]);
                }
            }
            queries.extend(0..=256);
            assert_matches(&ranges, f32::from_bits(next()), &queries);
        }
    }

    #[test]
    fn fragmented_ranges_have_a_linear_output_bound() {
        let count = 4096_u32;
        let mut ranges: Vec<_> = (0..count - 1)
            .map(|index| (2 * index + 1, 2 * index + 1, index as f32))
            .collect();
        ranges.push((0, 2 * count - 2, -1.0));
        let widths = CompositeWidths::new(&ranges, 1000.0);
        assert_eq!(widths.ranges.len(), 2 * count as usize - 1);
        for cid in 0..2 * count {
            assert_eq!(
                widths.width(cid).to_bits(),
                (first_width(&ranges, cid, 1000.0) * THOUSANDTH).to_bits()
            );
        }
    }

    #[test]
    fn simple_widths_keep_missing_width_and_type3_scaling() {
        let simple = SimpleWidths {
            first_char: 65,
            widths: vec![100.0, 200.0],
            missing: Some(300.0),
            glyph_scale: 0.02,
        };
        assert!(close(simple.width(64), 6.0));
        assert!(close(simple.width(65), 2.0));
        assert!(close(simple.width(66), 4.0));
        assert!(close(simple.width(u32::MAX), 6.0));
        let no_missing = SimpleWidths {
            missing: None,
            ..simple
        };
        assert!(close(no_missing.width(64), DEFAULT_WIDTH * THOUSANDTH));
        assert!(close(
            SimpleWidths::unknown().width(65),
            DEFAULT_WIDTH * THOUSANDTH
        ));
    }
}

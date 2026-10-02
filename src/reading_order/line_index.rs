//! Find the latest matching horizontal line without scanning every earlier one.
//!
//! Leaves keep build order. Internal nodes bound horizontal extent, largest
//! size, and lowest baseline; they only reject a subtree when no leaf can pass
//! the original predicate. Search visits the right child first, preserving the
//! reverse-scan winner. A bounded explicit stack replaces recursive traversal.

use super::{BASELINE_TOLERANCE, BBox, LINE_REACH, LineBuild, MAX_LINES};

/// Search visits, index updates, and conservative growth work per page.
pub(super) const MAX_HORIZONTAL_WORK: usize = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Limit {
    Work,
    Lines,
}

impl Limit {
    pub(super) fn warning(self) -> String {
        let (kind, limit) = match self {
            Self::Work => ("work", MAX_HORIZONTAL_WORK),
            Self::Lines => ("indexed lines", MAX_LINES),
        };
        format!(
            "resource_limit: horizontal grouping {kind} limit exceeded (limit={limit}); remaining ordinary spans kept separate"
        )
    }
}

#[derive(Clone, Copy, PartialEq)]
struct Bounds {
    left: f32,
    right: f32,
    size: f32,
    baseline: f32,
}

impl Bounds {
    const EMPTY: Self = Self {
        left: f32::INFINITY,
        right: f32::NEG_INFINITY,
        size: 0.0,
        baseline: f32::INFINITY,
    };

    fn of(line: &LineBuild) -> Self {
        Self {
            left: line.bbox.x0,
            right: line.bbox.x1,
            size: line.size,
            baseline: line.baseline,
        }
    }

    fn union(self, other: Self) -> Self {
        Self {
            left: self.left.min(other.left),
            right: self.right.max(other.right),
            size: self.size.max(other.size),
            baseline: self.baseline.min(other.baseline),
        }
    }

    fn might_match(self, bbox: BBox, size: f32, largest: f32) -> bool {
        let reach = LINE_REACH * size.max(self.size);
        // Keep the original subtraction/addition direction: replacing these
        // tests with algebraically equivalent bounds changes f32 edge cases.
        self.baseline - bbox.y0 <= BASELINE_TOLERANCE * largest
            && bbox.x0 <= self.right + reach
            && bbox.x1 >= self.left - reach
    }
}

fn matches(line: &LineBuild, bbox: BBox, size: f32) -> bool {
    let reference = size.max(line.size);
    let same_baseline = (line.baseline - bbox.y0).abs() <= BASELINE_TOLERANCE * reference;
    let reach = LINE_REACH * reference;
    same_baseline && bbox.x0 <= line.bbox.x1 + reach && bbox.x1 >= line.bbox.x0 - reach
}

pub(super) struct LineIndex {
    nodes: Vec<Bounds>,
    capacity: usize,
    stack: Vec<usize>,
    pending: Option<(usize, Bounds)>,
    work_left: usize,
    pub(super) limit: Option<Limit>,
}

impl LineIndex {
    pub(super) fn new(work: usize) -> Self {
        Self {
            nodes: Vec::new(),
            capacity: 0,
            stack: Vec::with_capacity(MAX_LINES.next_power_of_two().ilog2() as usize + 1),
            pending: None,
            work_left: work,
            limit: None,
        }
    }

    fn charge(&mut self, cost: usize) -> bool {
        if self.limit.is_some() {
            return false;
        }
        if let Some(left) = self.work_left.checked_sub(cost) {
            self.work_left = left;
            true
        } else {
            self.work_left = 0;
            self.limit = Some(Limit::Work);
            false
        }
    }

    pub(super) fn find(
        &mut self,
        builds: &[LineBuild],
        bbox: BBox,
        size: f32,
        largest: f32,
    ) -> Option<usize> {
        let last = builds.len().checked_sub(1)?;
        if !self.charge(1) {
            return None;
        }
        // Ordinary adjacent text usually joins the newest line. Preserve that
        // cheap case before consulting the tree, then omit that leaf below.
        if matches(&builds[last], bbox, size) {
            return Some(last);
        }
        self.flush();
        if self.limit.is_some() {
            return None;
        }
        self.stack.clear();
        self.stack.push(1);
        while let Some(node) = self.stack.pop() {
            if !self.charge(1) {
                return None;
            }
            if !self.nodes.get(node)?.might_match(bbox, size, largest) {
                continue;
            }
            if node >= self.capacity {
                let index = node - self.capacity;
                if index != last && matches(&builds[index], bbox, size) {
                    return Some(index);
                }
            } else {
                self.stack.push(node * 2);
                self.stack.push(node * 2 + 1);
            }
        }
        None
    }

    pub(super) fn update(&mut self, index: usize, line: &LineBuild) {
        if !self.charge(1) {
            return;
        }
        if index >= MAX_LINES {
            self.limit = Some(Limit::Lines);
            return;
        }
        // Consecutive words usually extend the same line. Keep only its newest
        // summary, and update ancestors when a search actually needs them.
        if self.pending.is_some_and(|(previous, _)| previous != index) {
            self.flush();
        }
        if self.limit.is_none() {
            self.pending = Some((index, Bounds::of(line)));
        }
    }

    fn flush(&mut self) {
        if let Some((index, bounds)) = self.pending.take() {
            self.store(index, bounds);
        }
    }

    fn store(&mut self, index: usize, mut bounds: Bounds) {
        if index >= self.capacity {
            let capacity = (index + 1).next_power_of_two();
            // Reserve before allocation/rebuild. At MAX_LINES, the new tree
            // is 1 MiB; an old half-size tree can coexist during this growth.
            if !self.charge(4 * capacity) {
                return;
            }
            let mut nodes = vec![Bounds::EMPTY; capacity * 2];
            if self.capacity > 0 {
                nodes[capacity..capacity + self.capacity]
                    .copy_from_slice(&self.nodes[self.capacity..]);
            }
            for node in (1..capacity).rev() {
                nodes[node] = nodes[node * 2].union(nodes[node * 2 + 1]);
            }
            self.nodes = nodes;
            self.capacity = capacity;
        }
        let mut node = self.capacity + index;
        loop {
            if !self.charge(1) {
                return;
            }
            if self.nodes[node] == bounds {
                break;
            }
            self.nodes[node] = bounds;
            node /= 2;
            if node == 0 {
                break;
            }
            bounds = self.nodes[node * 2].union(self.nodes[node * 2 + 1]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{find_line, union};
    use super::*;

    fn line(bbox: BBox, size: f32) -> LineBuild {
        LineBuild {
            baseline: bbox.y0,
            size,
            bbox,
            spans: Vec::new(),
            accents: Vec::new(),
        }
    }

    #[test]
    fn indexed_winners_match_the_naive_reverse_scan_at_every_prefix() {
        let mut seed = 0x8329_7513_u64;
        let mut cases = Vec::new();
        for _ in 0..6000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let x = ((seed >> 8) % 80) as f32 * 17.0;
            let y = ((seed >> 24) % 50) as f32 * 5.0;
            let size = [4.0, 9.0, 16.0, 32.0][(seed % 4) as usize];
            let width = [3.0, 9.0, 21.0, 61.0][((seed >> 4) % 4) as usize];
            cases.push((
                BBox {
                    x0: x,
                    x1: x + width,
                    y0: y,
                    y1: y + size,
                },
                size,
            ));
        }
        cases.sort_by(|(a, _), (b, _)| b.y0.total_cmp(&a.y0).then(a.x0.total_cmp(&b.x0)));
        let mut builds = Vec::new();
        let mut index = LineIndex::new(MAX_HORIZONTAL_WORK);
        for (step, (bbox, size)) in cases.into_iter().enumerate() {
            let expected = find_line(&builds, bbox, size, 32.0);
            assert_eq!(
                index.find(&builds, bbox, size, 32.0),
                expected,
                "prefix {step}"
            );
            let at = if let Some(at) = expected {
                builds[at].bbox = union(builds[at].bbox, bbox);
                builds[at].size = builds[at].size.max(size);
                at
            } else {
                builds.push(line(bbox, size));
                builds.len() - 1
            };
            index.update(at, &builds[at]);
            assert!(index.limit.is_none(), "prefix {step}");
        }
    }

    #[test]
    fn latest_matching_line_wins_after_pruning_and_bounds_refresh() {
        let mut builds: Vec<_> = [(10.0, 12.0), (40.0, 10.0), (100.0, 10.0)]
            .into_iter()
            .map(|(x, y)| {
                line(
                    BBox {
                        x0: x,
                        x1: x + 6.0,
                        y0: y,
                        y1: y + 4.0,
                    },
                    4.0,
                )
            })
            .collect();
        let mut index = LineIndex::new(MAX_HORIZONTAL_WORK);
        for (i, build) in builds.iter().enumerate() {
            index.update(i, build);
        }
        let bbox = BBox {
            x0: 14.0,
            x1: 39.0,
            y0: 10.5,
            y1: 18.5,
        };
        assert_eq!(find_line(&builds, bbox, 8.0, 20.0), Some(1));
        assert_eq!(index.find(&builds, bbox, 8.0, 20.0), Some(1));
        builds[2].bbox.x0 = 50.0;
        builds[2].size = 20.0;
        index.update(2, &builds[2]);
        assert_eq!(find_line(&builds, bbox, 8.0, 20.0), Some(2));
        assert_eq!(index.find(&builds, bbox, 8.0, 20.0), Some(2));
    }

    #[test]
    fn original_float_boundaries_are_preserved() {
        for x in [0.0, -0.0, -100.0, f32::MIN_POSITIVE, 1e20] {
            let mut builds = vec![line(
                BBox {
                    x0: x,
                    x1: x + 10.0,
                    y0: 10.0,
                    y1: 20.0,
                },
                10.0,
            )];
            // A distant newer leaf defeats the newest-line shortcut, so true
            // edge matches must survive the subtree bounds as well.
            builds.push(line(
                BBox {
                    x0: f32::MAX / 2.0,
                    x1: f32::MAX / 2.0,
                    y0: 10.0,
                    y1: 20.0,
                },
                10.0,
            ));
            let mut index = LineIndex::new(MAX_HORIZONTAL_WORK);
            index.update(0, &builds[0]);
            index.update(1, &builds[1]);
            for left in [(x + 20.0).next_down(), x + 20.0, (x + 20.0).next_up()] {
                for baseline in [6.0_f32.next_down(), 6.0, 6.0_f32.next_up()] {
                    let bbox = BBox {
                        x0: left,
                        x1: left + 10.0,
                        y0: baseline,
                        y1: baseline + 10.0,
                    };
                    assert_eq!(
                        index.find(&builds, bbox, 10.0, 10.0),
                        find_line(&builds, bbox, 10.0, 10.0)
                    );
                }
            }
        }
    }

    #[test]
    fn exhaustion_during_rebuild_or_update_disables_further_searches() {
        let builds = vec![line(
            BBox {
                x0: 0.0,
                x1: 10.0,
                y0: 0.0,
                y1: 10.0,
            },
            10.0,
        )];
        for work in [0, 3, 4] {
            let mut index = LineIndex::new(work);
            index.update(0, &builds[0]);
            let distant = BBox {
                x0: 100.0,
                x1: 110.0,
                y0: 0.0,
                y1: 10.0,
            };
            assert_eq!(index.find(&builds, distant, 10.0, 10.0), None);
            assert!(index.limit.is_some());
            assert_eq!(index.find(&builds, builds[0].bbox, 10.0, 10.0), None);
            assert_eq!(index.work_left, 0);
        }
    }

    #[test]
    fn separated_spans_remain_within_budget_and_index_memory_limit() {
        let mut builds = Vec::new();
        let mut index = LineIndex::new(MAX_HORIZONTAL_WORK);
        for i in 0..MAX_LINES {
            let x = i as f32 * 40.0;
            let bbox = BBox {
                x0: x,
                x1: x + 10.0,
                y0: 0.0,
                y1: 10.0,
            };
            assert_eq!(index.find(&builds, bbox, 10.0, 10.0), None);
            builds.push(line(bbox, 10.0));
            index.update(i, &builds[i]);
        }
        assert!(index.limit.is_none());
        assert!(index.nodes.len() * std::mem::size_of::<Bounds>() <= 1024 * 1024);
        assert!(MAX_HORIZONTAL_WORK - index.work_left < 650_000);
        index.update(MAX_LINES, &builds[0]);
        assert!(index.limit.is_some());
    }
}

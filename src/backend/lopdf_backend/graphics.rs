//! Accumulate already-transformed page-space boxes into stable figure output.
//!
//! The interpreter owns PDF state and applies the CTM before calling this module.
//! There is no PDF syntax or resource traversal here: a painted path is a box,
//! and a raster is its placed box plus optional pixel dimensions. Keeping this
//! accumulator separate makes the geometry bounds independent of lexer/cache
//! limits and preserves the same insertion order across page and Form programs.
//!
//! Vector clusters are bounded by count and comparison work. On exhaustion the
//! accumulator continues extending one covering box, while status flags let the
//! parent report the loss of detail. These are component bounds, not a bound on
//! all page memory: rules retain their existing unrestricted collection policy.

use crate::schema::{BBox, Figure};

/// A painted box thinner than this (points) and at least [`RULE_LENGTH`]
/// long is a `rule` figure.
const RULE_THICKNESS: f32 = 2.0;
/// Shortest `rule` figure, in points.
const RULE_LENGTH: f32 = 30.0;
/// Painted boxes closer than this (points) belong to one `vector` figure.
const CLUSTER_GAP: f32 = 6.0;
/// A `vector` cluster that fits in a square this wide (points) is dropped.
const MIN_VECTOR_SIDE: f32 = 8.0;
/// Most distinct vector clusters or image placements retained on one page.
/// Repeated/nearby paths merge before this bound is checked.
pub(super) const MAX_CLUSTER_BOXES: usize = 2000;
/// Bound repeated fixed-point scans independently of the number of paths.
/// On exhaustion the covering extent is retained with a partial outcome.
pub(super) const MAX_CLUSTER_COMPARISONS: usize = 4_000_000;

/// The smallest box holding every one of `corners`.
pub(super) fn box_of(corners: [(f32, f32); 4]) -> BBox {
    let mut bbox = BBox {
        x0: f32::MAX,
        y0: f32::MAX,
        x1: f32::MIN,
        y1: f32::MIN,
    };
    for (x, y) in corners {
        bbox.x0 = bbox.x0.min(x);
        bbox.y0 = bbox.y0.min(y);
        bbox.x1 = bbox.x1.max(x);
        bbox.y1 = bbox.y1.max(y);
    }
    bbox
}

/// The smallest box holding both `a` and `b`.
fn enclose(a: BBox, b: BBox) -> BBox {
    BBox {
        x0: a.x0.min(b.x0),
        y0: a.y0.min(b.y0),
        x1: a.x1.max(b.x1),
        y1: a.y1.max(b.y1),
    }
}

/// Whether `a` and `b` overlap or lie within `gap` of each other.
fn near(a: BBox, b: BBox, gap: f32) -> bool {
    a.x0 <= b.x1 + gap && b.x0 <= a.x1 + gap && a.y0 <= b.y1 + gap && b.y0 <= a.y1 + gap
}

fn contains(outer: BBox, inner: BBox) -> bool {
    inner.x0 <= inner.x1
        && inner.y0 <= inner.y1
        && outer.x0 <= inner.x0
        && inner.x1 <= outer.x1
        && outer.y0 <= inner.y0
        && inner.y1 <= outer.y1
}

/// Merge `boxes` into clusters: two boxes share a cluster when they (or
/// the clusters grown so far around them) overlap or lie within
/// [`CLUSTER_GAP`]. Each new box absorbs every cluster near it, rescanning
/// after each merge, so no two clusters left are near each other (a fixed
/// point). Clusters come top to bottom, then left to right.
#[cfg(test)]
pub(super) fn cluster(boxes: &[BBox]) -> Vec<BBox> {
    let mut clusters: Vec<BBox> = Vec::new();
    for &bbox in boxes {
        let mut grown = bbox;
        let mut at = 0;
        while at < clusters.len() {
            if near(clusters[at], grown, CLUSTER_GAP) {
                grown = enclose(grown, clusters.swap_remove(at));
                at = 0;
            } else {
                at += 1;
            }
        }
        clusters.push(grown);
    }
    clusters.sort_by(|a, b| b.y1.total_cmp(&a.y1).then(a.x0.total_cmp(&b.x0)));
    clusters
}

/// An Image `XObject` as placed on the page.
pub(super) struct Raster {
    pub(super) bbox: BBox,
    pub(super) width_px: Option<u32>,
    pub(super) height_px: Option<u32>,
}

/// Painted paths and images of one page, in page space, gathered while its
/// content runs.
#[derive(Default)]
pub(super) struct Graphics {
    /// Thin painted boxes (see [`RULE_THICKNESS`]).
    rules: Vec<BBox>,
    /// Fixed-point clusters in insertion order, at most [`MAX_CLUSTER_BOXES`].
    pub(super) shapes: Vec<BBox>,
    /// Union of every box that is not a rule.
    extent: Option<BBox>,
    /// Too many distinct clusters, or their comparison budget was exhausted.
    pub(super) overflow: bool,
    pub(super) comparisons: usize,
    pub(super) work_limited: bool,
    /// Image placements, at most [`MAX_CLUSTER_BOXES`].
    rasters: Vec<Raster>,
    pub(super) raster_overflow: bool,
}

impl Graphics {
    pub(super) fn add_path(&mut self, bbox: BBox) {
        let width = bbox.x1 - bbox.x0;
        let height = bbox.y1 - bbox.y0;
        let horizontal = height < RULE_THICKNESS && width >= RULE_LENGTH;
        let vertical = width < RULE_THICKNESS && height >= RULE_LENGTH;
        if horizontal || vertical {
            self.rules.push(bbox);
            return;
        }
        // Always extend the fallback before checking limits. After overflow it
        // must still cover later paints, including the paint that hit the bound.
        self.extent = Some(match self.extent {
            Some(so_far) => enclose(so_far, bbox),
            None => bbox,
        });
        if self.overflow {
            return;
        }
        // A cluster cannot be near any other retained cluster. A paint wholly
        // inside the last cluster therefore cannot merge with an earlier one.
        // Dense figures often paint many such details in succession: retain
        // the exact remove-last/append result without searching all clusters.
        // Failed containment probes consume the same work allowance as scans.
        if self.shapes.len() > 1 {
            if !self.compare() {
                return;
            }
            let last = self.shapes.len() - 1;
            if contains(self.shapes[last], bbox) {
                self.shapes[last] = enclose(bbox, self.shapes[last]);
                return;
            }
        }
        // This is the same insertion/fixed-point order as `cluster`, but a
        // dense figure keeps only its clusters instead of every painted path.
        let mut grown = bbox;
        let mut at = 0;
        while at < self.shapes.len() {
            if !self.compare() {
                return;
            }
            if near(self.shapes[at], grown, CLUSTER_GAP) {
                let previous = self.shapes.swap_remove(at);
                grown = enclose(grown, previous);
                // If this merge did not enlarge the old cluster, its existing
                // separation from every remaining cluster already proves the
                // fixed point. Keep swap_remove/append order, skip the rescan.
                if grown == previous {
                    break;
                }
                // The enlarged box can bridge an earlier, previously distant
                // cluster. Restart to reach a fixed point; the comparison
                // budget bounds repeated scans even for adversarial placements.
                at = 0;
            } else {
                at += 1;
            }
        }
        if self.shapes.len() == MAX_CLUSTER_BOXES {
            self.overflow = true;
        } else {
            self.shapes.push(grown);
        }
    }

    fn compare(&mut self) -> bool {
        if self.comparisons == MAX_CLUSTER_COMPARISONS {
            self.overflow = true;
            self.work_limited = true;
            false
        } else {
            self.comparisons += 1;
            true
        }
    }

    pub(super) fn add_raster(&mut self, raster: Raster) {
        if self.rasters.len() < MAX_CLUSTER_BOXES {
            self.rasters.push(raster);
        } else {
            self.raster_overflow = true;
        }
    }

    /// `rule` figures, then `vector` clusters at least [`MIN_VECTOR_SIDE`]
    /// wide or high, then `raster` figures; indexed in that order.
    pub(super) fn into_figures(self) -> Vec<Figure> {
        // Figure indices follow emission order. Preserve rule insertion order,
        // sort only vector clusters (PDF page-space y grows upward), then retain
        // raster placement order. Sorting everything would change stable output.
        let mut figures: Vec<Figure> = Vec::new();
        for bbox in self.rules {
            push_figure(&mut figures, "rule", bbox, None, None);
        }
        let mut clusters: Vec<BBox> = if self.overflow {
            self.extent.into_iter().collect()
        } else {
            self.shapes
        };
        clusters.sort_by(|a, b| b.y1.total_cmp(&a.y1).then(a.x0.total_cmp(&b.x0)));
        for bbox in clusters {
            if bbox.x1 - bbox.x0 >= MIN_VECTOR_SIDE || bbox.y1 - bbox.y0 >= MIN_VECTOR_SIDE {
                push_figure(&mut figures, "vector", bbox, None, None);
            }
        }
        for raster in self.rasters {
            let (width_px, height_px) = (raster.width_px, raster.height_px);
            push_figure(&mut figures, "raster", raster.bbox, width_px, height_px);
        }
        figures
    }
}

/// Append a figure of `kind` with the next index; no bytes are captured.
fn push_figure(
    figures: &mut Vec<Figure>,
    kind: &str,
    bbox: BBox,
    width_px: Option<u32>,
    height_px: Option<u32>,
) {
    let index = u32::try_from(figures.len()).unwrap_or(u32::MAX);
    figures.push(Figure {
        index,
        bbox: Some(bbox),
        kind: kind.to_string(),
        mime: None,
        width_px,
        height_px,
        sha256: None,
        file: None,
        caption: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed(x: f32, y: f32, width: f32, height: f32) -> BBox {
        BBox {
            x0: x,
            y0: y,
            x1: x + width,
            y1: y + height,
        }
    }

    fn reference_insert(clusters: &mut Vec<BBox>, bbox: BBox) {
        let mut grown = bbox;
        let mut at = 0;
        while at < clusters.len() {
            if near(clusters[at], grown, CLUSTER_GAP) {
                grown = enclose(grown, clusters.swap_remove(at));
                at = 0;
            } else {
                at += 1;
            }
        }
        clusters.push(grown);
    }

    #[test]
    fn containment_shortcuts_preserve_every_prefix_and_cluster_order() {
        let mut expected = Vec::new();
        let mut graphics = Graphics::default();
        let mut state = 0x7198_2513_u64;
        // Separate clusters, repeated contained paints, and bridges all occur.
        // Compare the unsorted vector, since swap_remove order affects which
        // comparisons a later insertion performs before its budget is spent.
        for step in 0..6000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let col = ((state >> 8) % 24) as f32;
            let row = ((state >> 16) % 12) as f32;
            let bbox = match step % 11 {
                0 | 1 => boxed(col * 35.0 + 2.0, row * 35.0 + 2.0, 8.0, 8.0),
                2 => boxed(col * 35.0, row * 35.0, 42.0, 14.0),
                _ => boxed(col * 35.0, row * 35.0, 14.0, 14.0),
            };
            reference_insert(&mut expected, bbox);
            graphics.add_path(bbox);
            assert!(!graphics.overflow, "prefix {step}");
            assert_eq!(graphics.shapes, expected, "prefix {step}");
        }
    }

    #[test]
    fn dense_details_inside_a_late_cluster_keep_all_vector_regions() {
        let mut graphics = Graphics::default();
        for col in 0..1000 {
            graphics.add_path(boxed(col as f32 * 30.0, 0.0, 14.0, 14.0));
        }
        let initial_work = graphics.comparisons;
        for _ in 0..10_000 {
            graphics.add_path(boxed(999.0 * 30.0 + 2.0, 2.0, 8.0, 8.0));
        }
        assert!(!graphics.overflow);
        assert_eq!(graphics.shapes.len(), 1000);
        assert_eq!(graphics.comparisons - initial_work, 10_000);
        assert_eq!(graphics.into_figures().len(), 1000);
    }

    #[test]
    fn inverted_boxes_do_not_take_the_containment_shortcut() {
        let mut expected = Vec::new();
        let mut graphics = Graphics::default();
        for bbox in [
            boxed(0.0, 0.0, 14.0, 14.0),
            boxed(100.0, 0.0, 14.0, 14.0),
            boxed(200.0, 2.0, -150.0, 8.0),
        ] {
            reference_insert(&mut expected, bbox);
            graphics.add_path(bbox);
            assert_eq!(graphics.shapes, expected);
        }
    }

    #[test]
    fn containment_is_charged_and_exhaustion_keeps_the_covering_extent() {
        let mut graphics = Graphics::default();
        graphics.add_path(boxed(-40.0, 0.0, 14.0, 14.0));
        graphics.add_path(boxed(40.0, 0.0, 14.0, 14.0));
        graphics.comparisons = MAX_CLUSTER_COMPARISONS;
        graphics.add_path(boxed(42.0, 2.0, 8.0, 8.0));
        assert!(graphics.work_limited);
        assert!(graphics.overflow);
        assert_eq!(graphics.comparisons, MAX_CLUSTER_COMPARISONS);
        graphics.add_path(boxed(80.0, 0.0, 14.0, 14.0));
        let figures = graphics.into_figures();
        assert_eq!(figures.len(), 1);
        assert_eq!(figures[0].bbox, Some(boxed(-40.0, 0.0, 134.0, 14.0)));
    }
}

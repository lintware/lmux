//! Split-tree maths: turn a `Layout` into concrete pane rectangles and
//! draggable divider strips, and back again when a divider is dragged.

use egui::Rect;
use lmux_proto::{Layout, SplitDirection};

pub const MIN_RATIO: f32 = 0.08;
pub const MAX_RATIO: f32 = 0.92;

#[derive(Clone, Debug, PartialEq)]
pub struct PaneSlot {
    pub pane: String,
    pub rect: Rect,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DividerSlot {
    /// Path into the tree: 0 = first, 1 = second.
    pub path: Vec<u8>,
    pub direction: SplitDirection,
    /// The interactive strip.
    pub rect: Rect,
    /// The whole region the split occupies (used to translate a drag to a ratio).
    pub region: Rect,
}

#[derive(Default)]
pub struct Solved {
    pub panes: Vec<PaneSlot>,
    pub dividers: Vec<DividerSlot>,
}

/// Recursively lay out `layout` inside `rect`, leaving `gap` pixels for each
/// divider.
pub fn solve(layout: &Layout, rect: Rect, gap: f32, out: &mut Solved) {
    solve_at(layout, rect, gap, &mut Vec::new(), out);
}

fn solve_at(layout: &Layout, rect: Rect, gap: f32, path: &mut Vec<u8>, out: &mut Solved) {
    match layout {
        Layout::Pane { pane } => out.panes.push(PaneSlot { pane: pane.clone(), rect }),
        Layout::Split { direction, ratio, first, second } => {
            let ratio = clamp_ratio(*ratio);
            let (first_rect, divider_rect, second_rect) = split_rects(rect, *direction, ratio, gap);
            out.dividers.push(DividerSlot {
                path: path.clone(),
                direction: *direction,
                rect: divider_rect,
                region: rect,
            });
            path.push(0);
            solve_at(first, first_rect, gap, path, out);
            path.pop();
            path.push(1);
            solve_at(second, second_rect, gap, path, out);
            path.pop();
        }
    }
}

fn split_rects(rect: Rect, dir: SplitDirection, ratio: f32, gap: f32) -> (Rect, Rect, Rect) {
    match dir {
        SplitDirection::Horizontal => {
            let avail = (rect.width() - gap).max(0.0);
            let w = avail * ratio;
            let a = Rect::from_min_max(rect.min, egui::pos2(rect.min.x + w, rect.max.y));
            let d = Rect::from_min_max(
                egui::pos2(rect.min.x + w, rect.min.y),
                egui::pos2(rect.min.x + w + gap, rect.max.y),
            );
            let b = Rect::from_min_max(egui::pos2(rect.min.x + w + gap, rect.min.y), rect.max);
            (a, d, b)
        }
        SplitDirection::Vertical => {
            let avail = (rect.height() - gap).max(0.0);
            let h = avail * ratio;
            let a = Rect::from_min_max(rect.min, egui::pos2(rect.max.x, rect.min.y + h));
            let d = Rect::from_min_max(
                egui::pos2(rect.min.x, rect.min.y + h),
                egui::pos2(rect.max.x, rect.min.y + h + gap),
            );
            let b = Rect::from_min_max(egui::pos2(rect.min.x, rect.min.y + h + gap), rect.max);
            (a, d, b)
        }
    }
}

pub fn clamp_ratio(r: f32) -> f32 {
    if !r.is_finite() {
        return 0.5;
    }
    r.clamp(MIN_RATIO, MAX_RATIO)
}

/// Convert a pointer position over `region` into the split ratio for `dir`.
pub fn ratio_from_pos(region: Rect, dir: SplitDirection, pos: f32, gap: f32) -> f32 {
    match dir {
        SplitDirection::Horizontal => {
            let avail = (region.width() - gap).max(1.0);
            clamp_ratio((pos - region.min.x) / avail)
        }
        SplitDirection::Vertical => {
            let avail = (region.height() - gap).max(1.0);
            clamp_ratio((pos - region.min.y) / avail)
        }
    }
}

/// Read the ratio of the split at `path`, if the path is valid.
pub fn get_ratio(layout: &Layout, path: &[u8]) -> Option<f32> {
    let mut node = layout;
    for step in path {
        match node {
            Layout::Split { first, second, .. } => {
                node = if *step == 0 { first } else { second };
            }
            Layout::Pane { .. } => return None,
        }
    }
    match node {
        Layout::Split { ratio, .. } => Some(*ratio),
        Layout::Pane { .. } => None,
    }
}

/// Set the ratio of the split at `path`. Returns true if it was applied.
pub fn set_ratio(layout: &mut Layout, path: &[u8], ratio: f32) -> bool {
    // An empty path targets the root split.
    let mut node = layout;
    for step in path {
        match node {
            Layout::Split { first, second, .. } => {
                node = if *step == 0 { first } else { second };
            }
            Layout::Pane { .. } => return false,
        }
    }
    match node {
        Layout::Split { ratio: r, .. } => {
            *r = clamp_ratio(ratio);
            true
        }
        Layout::Pane { .. } => false,
    }
}

/// The first leaf pane under the split at `path`.
pub fn first_leaf_at(layout: &Layout, path: &[u8]) -> Option<String> {
    let mut node = layout;
    for step in path {
        match node {
            Layout::Split { first, second, .. } => node = if *step == 0 { first } else { second },
            Layout::Pane { .. } => return None,
        }
    }
    Some(first_leaf(node))
}

fn first_leaf(node: &Layout) -> String {
    match node {
        Layout::Pane { pane } => pane.clone(),
        Layout::Split { first, .. } => first_leaf(first),
    }
}

/// Shrink surface chips to fit `budget`, never stretching them and never hiding
/// the active one. Returns one width per input chip; `0.0` means "do not show".
///
/// Chips are sized to their content, so when a pane is crowded the titles give
/// way first, and only if even the minimum widths do not fit are whole chips
/// dropped (the active chip is always kept).
pub fn fit_chips(natural: &[f32], min: f32, budget: f32, active: usize) -> Vec<f32> {
    let n = natural.len();
    if n == 0 {
        return Vec::new();
    }
    let total: f32 = natural.iter().sum();
    if total <= budget {
        return natural.to_vec();
    }

    // Everyone shrinks proportionally to the space they can give up.
    let total_slack: f32 = natural.iter().map(|w| (w - min).max(0.0)).sum();
    let over = total - budget;
    if total_slack >= over && total_slack > 0.0 {
        return natural
            .iter()
            .map(|w| (w - over * (w - min).max(0.0) / total_slack).max(min))
            .collect();
    }

    // Still too wide even at the minimum: keep the active chip and add
    // neighbours outward (right first) until the budget is used.
    let mut widths = vec![0.0f32; n];
    let active = active.min(n - 1);
    widths[active] = min;
    let mut used = min;
    let mut left = active as isize - 1;
    let mut right = active + 1;
    loop {
        let mut progressed = false;
        if right < n && used + min <= budget {
            widths[right] = min;
            used += min;
            right += 1;
            progressed = true;
        }
        if left >= 0 && used + min <= budget {
            widths[left as usize] = min;
            used += min;
            left -= 1;
            progressed = true;
        }
        if !progressed {
            break;
        }
    }
    widths
}

/// The pane refs in visual order (left-to-right, top-to-bottom for the tree).
pub fn pane_order(layout: &Layout) -> Vec<String> {
    let mut out = Solved::default();
    solve(layout, Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(100.0, 100.0)), 0.0, &mut out);
    out.panes.into_iter().map(|p| p.pane).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{pos2, vec2, Rect};

    fn pane(id: &str) -> Layout {
        Layout::Pane { pane: id.to_owned() }
    }

    fn area() -> Rect {
        Rect::from_min_size(pos2(0.0, 0.0), vec2(200.0, 100.0))
    }

    #[test]
    fn single_pane_fills_area() {
        let mut out = Solved::default();
        solve(&pane("pane:1"), area(), 6.0, &mut out);
        assert_eq!(out.panes.len(), 1);
        assert_eq!(out.panes[0].rect, area());
        assert!(out.dividers.is_empty());
    }

    #[test]
    fn horizontal_split_halves() {
        let l = Layout::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(pane("a")),
            second: Box::new(pane("b")),
        };
        let mut out = Solved::default();
        solve(&l, area(), 6.0, &mut out);
        assert_eq!(out.panes.len(), 2);
        assert_eq!(out.dividers.len(), 1);
        // 200 - 6 gap = 194, half = 97.
        let a = out.panes.iter().find(|p| p.pane == "a").unwrap();
        let b = out.panes.iter().find(|p| p.pane == "b").unwrap();
        assert!((a.rect.width() - 97.0).abs() < 0.01);
        assert!((b.rect.width() - 97.0).abs() < 0.01);
        assert!((a.rect.max.x - 97.0).abs() < 0.01);
        assert!((b.rect.min.x - 103.0).abs() < 0.01);
        // Divider sits between them.
        assert!((out.dividers[0].rect.min.x - 97.0).abs() < 0.01);
        assert!((out.dividers[0].rect.width() - 6.0).abs() < 0.01);
    }

    #[test]
    fn vertical_split_ratio() {
        let l = Layout::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.25,
            first: Box::new(pane("top")),
            second: Box::new(pane("bottom")),
        };
        let mut out = Solved::default();
        solve(&l, area(), 0.0, &mut out);
        let top = out.panes.iter().find(|p| p.pane == "top").unwrap();
        let bottom = out.panes.iter().find(|p| p.pane == "bottom").unwrap();
        assert!((top.rect.height() - 25.0).abs() < 0.01);
        assert!((bottom.rect.height() - 75.0).abs() < 0.01);
    }

    #[test]
    fn nested_paths_and_regions() {
        // horiz( a , vert( b, c ) )
        let l = Layout::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(pane("a")),
            second: Box::new(Layout::Split {
                direction: SplitDirection::Vertical,
                ratio: 0.5,
                first: Box::new(pane("b")),
                second: Box::new(pane("c")),
            }),
        };
        let mut out = Solved::default();
        solve(&l, area(), 0.0, &mut out);
        assert_eq!(out.panes.len(), 3);
        assert_eq!(out.dividers.len(), 2);
        let outer = out.dividers.iter().find(|d| d.path.is_empty()).unwrap();
        assert_eq!(outer.direction, SplitDirection::Horizontal);
        let inner = out.dividers.iter().find(|d| !d.path.is_empty()).unwrap();
        assert_eq!(inner.path, vec![1]);
        assert_eq!(inner.direction, SplitDirection::Vertical);
        assert_eq!(pane_order(&l), vec!["a", "b", "c"]);
    }

    #[test]
    fn ratio_from_pos_round_trips() {
        let r = area();
        let ratio = ratio_from_pos(r, SplitDirection::Horizontal, 100.0, 0.0);
        assert!((ratio - 0.5).abs() < 0.01);
        let mid = ratio_from_pos(r, SplitDirection::Vertical, 50.0, 0.0);
        assert!((mid - 0.5).abs() < 0.01);
    }

    #[test]
    fn get_and_set_ratio() {
        let mut l = Layout::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(pane("a")),
            second: Box::new(Layout::Split {
                direction: SplitDirection::Vertical,
                ratio: 0.25,
                first: Box::new(pane("b")),
                second: Box::new(pane("c")),
            }),
        };
        assert_eq!(get_ratio(&l, &[]), Some(0.5));
        assert_eq!(get_ratio(&l, &[1]), Some(0.25));
        assert!(set_ratio(&mut l, &[1], 0.75));
        assert_eq!(get_ratio(&l, &[1]), Some(0.75));
        assert!(set_ratio(&mut l, &[], 0.3));
        assert_eq!(get_ratio(&l, &[]), Some(0.3));
        assert!(!set_ratio(&mut l, &[0], 0.1)); // leaf, not a split
    }

    #[test]
    fn chips_keep_natural_width_when_they_fit() {
        let natural = [80.0, 100.0, 60.0];
        assert_eq!(fit_chips(&natural, 30.0, 400.0, 1), natural.to_vec());
    }

    #[test]
    fn chips_shrink_proportionally_never_below_min() {
        let natural = [80.0, 100.0, 60.0];
        let budget = 180.0;
        let w = fit_chips(&natural, 30.0, budget, 0);
        assert_eq!(w.len(), 3);
        assert!(w.iter().all(|x| *x >= 30.0), "{w:?}");
        let sum: f32 = w.iter().sum();
        assert!((sum - budget).abs() < 0.01, "sum was {sum}");
        // The widest chip gives up the most.
        assert!(w[1] < 100.0 && w[0] < 80.0);
    }

    #[test]
    fn chips_drop_from_the_edges_keeping_the_active_one() {
        // Minimum widths alone need 5 * 30 = 150 > 66.
        let natural = [80.0; 5];
        let w = fit_chips(&natural, 30.0, 66.0, 3);
        assert_eq!(w.len(), 5);
        assert!(w[3] > 0.0, "active chip must stay");
        assert_eq!(w.iter().filter(|x| **x > 0.0).count(), 2, "{w:?}");
        // Room for one neighbour, and the right one is preferred.
        assert!(w[4] > 0.0 && w[2] == 0.0, "{w:?}");
    }

    #[test]
    fn chips_keep_the_left_neighbour_when_the_active_one_is_last() {
        let natural = [80.0; 4];
        let w = fit_chips(&natural, 30.0, 66.0, 3);
        assert!(w[3] > 0.0 && w[2] > 0.0, "{w:?}");
        assert_eq!(w.iter().filter(|x| **x > 0.0).count(), 2, "{w:?}");
    }

    #[test]
    fn chips_keep_the_active_one_even_with_almost_no_room() {
        let natural = [80.0; 4];
        let w = fit_chips(&natural, 30.0, 20.0, 2);
        assert!(w[2] > 0.0);
        assert_eq!(w.iter().filter(|x| **x > 0.0).count(), 1, "{w:?}");
    }

    #[test]
    fn ratio_is_clamped() {
        let r = area();
        assert!((ratio_from_pos(r, SplitDirection::Horizontal, -1000.0, 0.0) - MIN_RATIO).abs() < 1e-6);
        assert!((ratio_from_pos(r, SplitDirection::Horizontal, 9999.0, 0.0) - MAX_RATIO).abs() < 1e-6);
    }
}

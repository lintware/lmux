//! Small text helpers (ellipsis truncation) used by the hand-painted sidebar.

use egui::{Color32, FontId};

pub fn measure(ctx: &egui::Context, text: &str, font: &FontId) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    ctx.fonts_mut(|f| {
        f.layout_no_wrap(text.to_owned(), font.clone(), Color32::WHITE)
            .size()
            .x
    })
}

/// Truncate `text` so it fits `max_w`, appending `…`. With `middle`, keep both
/// ends (used for cwd paths, which matter at the tail).
pub fn ellipsize(
    ctx: &egui::Context,
    text: &str,
    font: &FontId,
    max_w: f32,
    middle: bool,
) -> String {
    if max_w <= 1.0 || text.is_empty() {
        return String::new();
    }
    if measure(ctx, text, font) <= max_w {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let ell = '…';
    if middle {
        // Grow a head and a tail until head+…+tail no longer fits.
        let mut head = 0usize;
        let mut tail = 0usize;
        loop {
            let next_head = head + 1;
            let next_tail = tail + 1;
            let too_wide = |h: usize, t: usize| {
                let mut s: String = chars[..h.min(chars.len())].iter().collect();
                s.push(ell);
                s.extend(chars[chars.len().saturating_sub(t)..].iter());
                measure(ctx, &s, font) > max_w
            };
            let can_head = next_head + tail <= chars.len();
            let can_tail = head + next_tail <= chars.len();
            // Grow the head first so a truncated path keeps its start.
            if can_head && !too_wide(next_head, tail) {
                head = next_head;
            } else if can_tail && !too_wide(head, next_tail) {
                tail = next_tail;
            } else {
                break;
            }
        }
        let mut s: String = chars[..head].iter().collect();
        s.push(ell);
        s.extend(chars[chars.len().saturating_sub(tail)..].iter());
        s
    } else {
        let mut lo = 0usize;
        let mut hi = chars.len();
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            let mut s: String = chars[..mid].iter().collect();
            s.push(ell);
            if measure(ctx, &s, font) <= max_w {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        let mut s: String = chars[..lo].iter().collect();
        s.push(ell);
        s
    }
}

/// Greedy word wrap into at most `max_lines` lines of `max_w`.
pub fn wrap_lines(
    ctx: &egui::Context,
    text: &str,
    font: &FontId,
    max_w: f32,
    max_lines: usize,
) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let candidate = if current.is_empty() {
            word.to_owned()
        } else {
            format!("{current} {word}")
        };
        if measure(ctx, &candidate, font) <= max_w || current.is_empty() {
            current = candidate;
        } else {
            lines.push(std::mem::take(&mut current));
            current = word.to_owned();
            if lines.len() == max_lines {
                break;
            }
        }
    }
    if lines.len() < max_lines && !current.is_empty() {
        lines.push(current);
    }
    lines.truncate(max_lines);
    lines
}

/// Shorten a home directory to `~`.
pub fn tilde(path: &str) -> String {
    for prefix in ["/Users/", "/home/"] {
        if let Some(rest) = path.strip_prefix(prefix) {
            if let Some((_, tail)) = rest.split_once('/') {
                return format!("~/{tail}");
            }
            return "~".to_owned();
        }
    }
    path.to_owned()
}

// `ellipsize`/`wrap_lines` need a live egui context (fonts are only available
// after the first pass), so they are exercised by the kittest snapshot tests.

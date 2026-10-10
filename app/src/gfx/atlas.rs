//! Glyph atlas: a square texture packed with the skyline bottom-left
//! algorithm, ported from Ghostty's `src/font/Atlas.zig` (MIT).
//!
//! Glyphs are packed edge to edge inside a 1px border: quads are whole
//! pixels and sampled nearest, so neighbours never bleed. When full, the
//! atlas doubles and keeps every existing glyph at the same coordinates.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug)]
struct Node {
    x: u32,
    y: u32,
    width: u32,
}

pub struct Atlas {
    pub data: Vec<u8>,
    pub size: u32,
    /// Bytes per pixel: 1 (coverage) or 4 (BGRA).
    pub depth: u32,
    nodes: Vec<Node>,
    /// A stamp taken on every write, unique across all atlases, so the GPU
    /// copy knows when to re-upload (even after the atlas is replaced).
    pub modified: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub struct AtlasFull;

fn stamp() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

impl Atlas {
    pub fn new(size: u32, depth: u32) -> Self {
        let mut a = Self { data: vec![0; (size * size * depth) as usize], size, depth, nodes: Vec::new(), modified: 0 };
        a.clear();
        a
    }

    pub fn clear(&mut self) {
        self.data.fill(0);
        self.nodes.clear();
        // The 1px border keeps glyphs off the texture edge.
        self.nodes.push(Node { x: 1, y: 1, width: self.size - 2 });
        self.modified = stamp();
    }

    /// Reserve a `width`×`height` region, or `AtlasFull`.
    pub fn reserve(&mut self, width: u32, height: u32) -> Result<Region, AtlasFull> {
        let mut region = Region { x: 0, y: 0, width, height };
        if width == 0 && height == 0 {
            return Ok(region);
        }
        let mut best_height = u32::MAX;
        let mut best_width = u32::MAX;
        let mut chosen = None;
        for i in 0..self.nodes.len() {
            let Some(y) = self.fit(i, width, height) else { continue };
            let node = self.nodes[i];
            if y + height < best_height || (y + height == best_height && node.width > 0 && node.width < best_width) {
                chosen = Some(i);
                best_width = node.width;
                best_height = y + height;
                region.x = node.x;
                region.y = y;
            }
        }
        let best = chosen.ok_or(AtlasFull)?;
        self.nodes.insert(best, Node { x: region.x, y: region.y + height, width });

        let i = best + 1;
        while i < self.nodes.len() {
            let prev = self.nodes[i - 1];
            let node = &mut self.nodes[i];
            if node.x < prev.x + prev.width {
                let shrink = prev.x + prev.width - node.x;
                node.x += shrink;
                node.width = node.width.saturating_sub(shrink);
                if node.width == 0 {
                    self.nodes.remove(i);
                    continue;
                }
            }
            break;
        }
        self.merge();
        Ok(region)
    }

    fn fit(&self, idx: usize, width: u32, height: u32) -> Option<u32> {
        let node = self.nodes[idx];
        if node.x + width > self.size - 1 {
            return None;
        }
        let mut y = node.y;
        let mut i = idx;
        let mut width_left = width;
        while width_left > 0 {
            let n = self.nodes.get(i)?;
            y = y.max(n.y);
            if y + height > self.size - 1 {
                return None;
            }
            width_left = width_left.saturating_sub(n.width);
            i += 1;
        }
        Some(y)
    }

    fn merge(&mut self) {
        let mut i = 0;
        while i + 1 < self.nodes.len() {
            if self.nodes[i].y == self.nodes[i + 1].y {
                self.nodes[i].width += self.nodes[i + 1].width;
                self.nodes.remove(i + 1);
                continue;
            }
            i += 1;
        }
    }

    /// Copy a tightly packed bitmap (`region.width * depth` bytes per row,
    /// top row first) into the reserved region.
    pub fn set(&mut self, region: Region, src: &[u8]) {
        let d = self.depth as usize;
        let row = region.width as usize * d;
        for i in 0..region.height as usize {
            let dst = ((region.y as usize + i) * self.size as usize + region.x as usize) * d;
            self.data[dst..dst + row].copy_from_slice(&src[i * row..i * row + row]);
        }
        self.modified = stamp();
    }

    /// Double (or more) the atlas, keeping every glyph where it was.
    pub fn grow(&mut self, size_new: u32) {
        assert!(size_new > self.size);
        let size_old = self.size;
        let d = self.depth as usize;
        let old = std::mem::take(&mut self.data);
        self.data = vec![0; (size_new * size_new) as usize * d];
        self.size = size_new;
        // Rows are copied whole; the border columns are zero anyway.
        let row = size_old as usize * d;
        for y in 1..(size_old - 1) as usize {
            let dst = y * size_new as usize * d;
            self.data[dst..dst + row].copy_from_slice(&old[y * row..y * row + row]);
        }
        self.nodes.push(Node { x: size_old - 1, y: 1, width: size_new - size_old });
        self.modified = stamp();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_without_overlap_and_grows() {
        let mut a = Atlas::new(32, 1);
        let mut regions = Vec::new();
        for i in 0..40u32 {
            let (w, h) = (3 + i % 4, 5 + i % 3);
            let r = match a.reserve(w, h) {
                Ok(r) => r,
                Err(AtlasFull) => {
                    a.grow(a.size * 2);
                    a.reserve(w, h).expect("fits after growing")
                }
            };
            assert!(r.x >= 1 && r.y >= 1 && r.x + w <= a.size - 1 && r.y + h <= a.size - 1);
            regions.push(r);
        }
        for (i, p) in regions.iter().enumerate() {
            for q in &regions[i + 1..] {
                let apart = p.x + p.width <= q.x || q.x + q.width <= p.x || p.y + p.height <= q.y || q.y + q.height <= p.y;
                assert!(apart, "{p:?} overlaps {q:?}");
            }
        }
    }

    #[test]
    fn growing_keeps_pixels_in_place() {
        let mut a = Atlas::new(16, 1);
        let r = a.reserve(2, 2).unwrap();
        a.set(r, &[1, 2, 3, 4]);
        a.grow(32);
        let at = |x: u32, y: u32| a.data[(y * a.size + x) as usize];
        assert_eq!([at(r.x, r.y), at(r.x + 1, r.y), at(r.x, r.y + 1), at(r.x + 1, r.y + 1)], [1, 2, 3, 4]);
    }
}

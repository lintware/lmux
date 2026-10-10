//! Which face draws each character, and the atlas of rasterized glyphs,
//! after Ghostty's `SharedGrid` and `CodepointResolver` (MIT).
//!
//! Faces are tried in order: the user's font families, the bundled
//! JetBrains Mono (Ghostty's default), the bundled Nerd Font symbols, Apple
//! Color Emoji, then whatever CoreText picks for the character, sized to
//! match the primary face. Box drawing, blocks and the like never reach a
//! font: the sprite module draws them to the exact cell.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::atlas::{Atlas, AtlasFull, Region};
use super::constraint::{is_symbol, Constraint};
use super::font::{Bitmap, Face};
use super::metrics::Metrics;
use super::sprite;

static JETBRAINS_MONO: &[u8] = include_bytes!("../../assets/fonts/JetBrainsMono.ttf");
static JETBRAINS_MONO_ITALIC: &[u8] = include_bytes!("../../assets/fonts/JetBrainsMono-Italic.ttf");
static NERD_SYMBOLS: &[u8] = include_bytes!("../../assets/fonts/SymbolsNerdFont.ttf");

/// The largest atlas the GPU is asked for (wgpu's default limit).
const MAX_ATLAS: u32 = 8192;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Style {
    Regular = 0,
    Bold = 1,
    Italic = 2,
    BoldItalic = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Presentation {
    Text,
    Emoji,
}

/// A glyph's place in the atlas and where it sits in its cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Glyph {
    pub width: u32,
    pub height: u32,
    pub offset_x: i32,
    pub offset_y: i32,
    pub atlas_x: u32,
    pub atlas_y: u32,
    pub color: bool,
}

/// Both atlases, shared with the GPU side, which uploads them when
/// `modified` moves.
pub struct Atlases {
    pub gray: Atlas,
    pub color: Atlas,
}

pub type SharedAtlases = Arc<Mutex<Atlases>>;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Key {
    Text { face: u16, glyph: u16, width: u8, constraint: u8 },
    Sprite { cp: u32, width: u8 },
}

struct Entry {
    face: Face,
    /// Fallback faces only answer for their own presentation (a text face
    /// is never used for an emoji, nor the emoji face for text).
    fallback: bool,
}

/// What decides the faces and their size.
#[derive(Clone, Debug, PartialEq)]
pub struct FontConfig {
    pub families: [Vec<String>; 4],
    pub points: f32,
    pub scale: f32,
    pub thicken: bool,
    pub adjust_cell_height: Option<u32>,
}

pub struct Fonts {
    pub config: FontConfig,
    pub metrics: Metrics,
    entries: Vec<Entry>,
    styles: [Vec<usize>; 4],
    resolved: HashMap<(char, Style, Presentation), Option<usize>>,
    fallback_by_name: HashMap<String, usize>,
    glyphs: HashMap<Key, Option<Glyph>>,
    pub atlases: SharedAtlases,
}

/// Ghostty's pixel size: points × round(scale × 72) / 72.
pub fn pixel_size(points: f32, scale: f32) -> f64 {
    let dpi = (scale as f64 * 72.0).round();
    points as f64 * dpi / 72.0
}

impl Fonts {
    pub fn new(config: FontConfig) -> Fonts {
        let px = pixel_size(config.points, config.scale);
        let mut entries: Vec<Entry> = Vec::new();
        let mut styles: [Vec<usize>; 4] = Default::default();
        let mut add = |face: Face, style: usize, fallback: bool, entries: &mut Vec<Entry>| {
            entries.push(Entry { face, fallback });
            styles[style].push(entries.len() - 1);
        };

        // The user's families, per style; a style without its own uses the
        // regular list.
        for (style, names) in config.families.iter().enumerate() {
            for name in names {
                if let Some(face) = Face::from_name(name, px) {
                    add(face, style, false, &mut entries);
                }
            }
        }

        // JetBrains Mono, as Ghostty bundles it: the variable font at
        // weight 700 for bold, its italic file for italics.
        if let Some(regular) = Face::from_bytes(JETBRAINS_MONO, px) {
            let bold = regular.with_variation(b"wght", 700.0);
            add(regular, Style::Regular as usize, true, &mut entries);
            add(bold, Style::Bold as usize, true, &mut entries);
        }
        if let Some(italic) = Face::from_bytes(JETBRAINS_MONO_ITALIC, px) {
            let bold_italic = italic.with_variation(b"wght", 700.0);
            add(italic, Style::Italic as usize, true, &mut entries);
            add(bold_italic, Style::BoldItalic as usize, true, &mut entries);
        }
        if let Some(symbols) = Face::from_bytes(NERD_SYMBOLS, px) {
            add(symbols, Style::Regular as usize, true, &mut entries);
        }
        if let Some(emoji) = Face::from_name("Apple Color Emoji", px) {
            add(emoji, Style::Regular as usize, true, &mut entries);
        }

        // Grid metrics come from the first regular face.
        let primary = styles[0].first().copied().unwrap_or(0);
        let mut metrics = Metrics::calc(&entries[primary].face.metrics);
        if let Some(h) = config.adjust_cell_height {
            metrics.adjust_cell_height(h);
        }
        let atlases = Arc::new(Mutex::new(Atlases { gray: Atlas::new(512, 1), color: Atlas::new(512, 4) }));
        Fonts {
            config,
            metrics,
            entries,
            styles,
            resolved: HashMap::new(),
            fallback_by_name: HashMap::new(),
            glyphs: HashMap::new(),
            atlases,
        }
    }

    fn primary(&self) -> &Face {
        &self.entries[self.styles[0].first().copied().unwrap_or(0)].face
    }

    /// The face that draws `cp` in `style`, finding a system fallback if
    /// none of the loaded faces has it.
    fn resolve(&mut self, cp: char, style: Style, p: Presentation) -> Option<usize> {
        if let Some(r) = self.resolved.get(&(cp, style, p)) {
            return *r;
        }
        let r = self.find(cp, style, p);
        self.resolved.insert((cp, style, p), r);
        r
    }

    fn find(&mut self, cp: char, style: Style, p: Presentation) -> Option<usize> {
        let fits = |e: &Entry| {
            e.face.glyph(cp).is_some() && (!e.fallback || (e.face.sbix == (p == Presentation::Emoji)))
        };
        if let Some(&i) = self.styles[style as usize].iter().find(|&&i| fits(&self.entries[i])) {
            return Some(i);
        }
        // A styled miss prefers a regular glyph from a loaded face.
        if style != Style::Regular {
            if let Some(&i) = self.styles[0].iter().find(|&&i| fits(&self.entries[i])) {
                return Some(i);
            }
        }
        if let Some(i) = self.discover(cp) {
            if fits(&self.entries[i]) || self.entries[i].face.glyph(cp).is_some() {
                return Some(i);
            }
        }
        // Last resort: any loaded face with the glyph, whatever its
        // presentation.
        self.styles[0].iter().copied().find(|&i| self.entries[i].face.glyph(cp).is_some())
    }

    /// Ask CoreText for a face with `cp`, sized like the primary face
    /// (Ghostty sizes fallbacks by the width of '水', like CSS
    /// `font-size-adjust`).
    fn discover(&mut self, cp: char) -> Option<usize> {
        let font = self.primary().fallback_for(cp)?;
        let face = Face::new(font);
        let name = face.post_script_name();
        if let Some(&i) = self.fallback_by_name.get(&name) {
            return Some(i);
        }
        let px = pixel_size(self.config.points, self.config.scale);
        let face = face.with_size(px);
        let pm = self.primary().metrics;
        let fm = face.metrics;
        let ratio = |a: Option<f64>, b: Option<f64>| match (a, b) {
            (Some(a), Some(b)) if a > 0.0 && b > 0.0 => Some((a / pm.px_per_em) / (b / fm.px_per_em)),
            _ => None,
        };
        let scale = if face.sbix {
            None
        } else {
            ratio(Some(pm.ic_width()), fm.ic_width)
                .or_else(|| ratio(Some(pm.ex_height()), fm.ex_height))
                .or_else(|| ratio(Some(pm.cap_height()), fm.cap_height))
        };
        let face = match scale {
            Some(s) if (s - 1.0).abs() > 1e-3 => face.with_size(px * s),
            _ => face,
        };
        self.entries.push(Entry { face, fallback: true });
        let i = self.entries.len() - 1;
        self.styles[0].push(i);
        self.fallback_by_name.insert(name, i);
        Some(i)
    }

    /// The glyph for a cell's character. `grid_width` is 2 for a wide
    /// character; `constraint_width` is how many cells a symbol may take.
    pub fn glyph(&mut self, cp: char, style: Style, p: Presentation, grid_width: u8, constraint_width: u8) -> Option<Glyph> {
        if sprite::covers(cp as u32) {
            return self.sprite(cp as u32, grid_width);
        }
        let face = self.resolve(cp, style, p)?;
        let id = self.entries[face].face.glyph(cp)?;
        let emoji = self.entries[face].face.sbix;
        let (constraint, tag) = if emoji {
            (Constraint::EMOJI, 2)
        } else if is_symbol(cp as u32) {
            (Constraint::FIT, 1)
        } else {
            (Constraint::NONE, 0)
        };
        let width = if emoji { grid_width } else { constraint_width };
        let key = Key::Text { face: face as u16, glyph: id, width, constraint: tag };
        if let Some(g) = self.glyphs.get(&key) {
            return *g;
        }
        let m = self.metrics;
        let bitmap = self.entries[face].face.render(id, &m, constraint, width, self.config.thicken);
        let g = bitmap.and_then(|b| self.place(b));
        self.glyphs.insert(key, g);
        g
    }

    /// A sprite: a decoration, a cursor, or a drawn character.
    pub fn sprite(&mut self, cp: u32, grid_width: u8) -> Option<Glyph> {
        let key = Key::Sprite { cp, width: grid_width };
        if let Some(g) = self.glyphs.get(&key) {
            return *g;
        }
        let g = sprite::render(cp, grid_width as u32, &self.metrics).and_then(|t| {
            self.place(Bitmap {
                width: t.width,
                height: t.height,
                offset_x: t.offset_x,
                offset_y: t.offset_y,
                color: false,
                data: t.data,
            })
        });
        self.glyphs.insert(key, g);
        g
    }

    fn place(&mut self, b: Bitmap) -> Option<Glyph> {
        let mut atlases = self.atlases.lock().ok()?;
        let atlas = if b.color { &mut atlases.color } else { &mut atlases.gray };
        let region: Region = loop {
            match atlas.reserve(b.width, b.height) {
                Ok(r) => break r,
                Err(AtlasFull) if atlas.size < MAX_ATLAS => atlas.grow(atlas.size * 2),
                Err(AtlasFull) => return None,
            }
        };
        atlas.set(region, &b.data);
        Some(Glyph {
            width: b.width,
            height: b.height,
            offset_x: b.offset_x,
            offset_y: b.offset_y,
            atlas_x: region.x,
            atlas_y: region.y,
            color: b.color,
        })
    }
}

/// Unicode's default emoji presentation (`Emoji_Presentation=Yes`): these
/// draw as colour emoji unless followed by VS15.
pub fn emoji_presentation(cp: u32) -> bool {
    matches!(cp,
        0x231A..=0x231B | 0x23E9..=0x23EC | 0x23F0 | 0x23F3 | 0x25FD..=0x25FE | 0x2614..=0x2615
        | 0x2648..=0x2653 | 0x267F | 0x2693 | 0x26A1 | 0x26AA..=0x26AB | 0x26BD..=0x26BE | 0x26C4..=0x26C5
        | 0x26CE | 0x26D4 | 0x26EA | 0x26F2..=0x26F3 | 0x26F5 | 0x26FA | 0x26FD | 0x2705 | 0x270A..=0x270B
        | 0x2728 | 0x274C | 0x274E | 0x2753..=0x2755 | 0x2757 | 0x2795..=0x2797 | 0x27B0 | 0x27BF
        | 0x2B1B..=0x2B1C | 0x2B50 | 0x2B55 | 0x1F004 | 0x1F0CF | 0x1F18E | 0x1F191..=0x1F19A
        | 0x1F1E6..=0x1F1FF | 0x1F201 | 0x1F21A | 0x1F22F | 0x1F232..=0x1F236 | 0x1F238..=0x1F23A
        | 0x1F250..=0x1F251 | 0x1F300..=0x1F320 | 0x1F32D..=0x1F335 | 0x1F337..=0x1F37C | 0x1F37E..=0x1F393
        | 0x1F3A0..=0x1F3CA | 0x1F3CF..=0x1F3D3 | 0x1F3E0..=0x1F3F0 | 0x1F3F4 | 0x1F3F8..=0x1F43E | 0x1F440
        | 0x1F442..=0x1F4FC | 0x1F4FF..=0x1F53D | 0x1F54B..=0x1F54E | 0x1F550..=0x1F567 | 0x1F57A
        | 0x1F595..=0x1F596 | 0x1F5A4 | 0x1F5FB..=0x1F64F | 0x1F680..=0x1F6C5 | 0x1F6CC | 0x1F6D0..=0x1F6D2
        | 0x1F6D5..=0x1F6D7 | 0x1F6DC..=0x1F6DF | 0x1F6EB..=0x1F6EC | 0x1F6F4..=0x1F6FC | 0x1F7E0..=0x1F7EB
        | 0x1F7F0 | 0x1F90C..=0x1F93A | 0x1F93C..=0x1F945 | 0x1F947..=0x1F9FF | 0x1FA70..=0x1FAFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fonts() -> Fonts {
        Fonts::new(FontConfig { families: Default::default(), points: 13.0, scale: 2.0, thicken: false, adjust_cell_height: None })
    }

    #[test]
    fn jetbrains_mono_is_the_default_grid() {
        let f = fonts();
        // JetBrains Mono at 26px: 600/1000 em advance, 1320/1000 em line.
        assert_eq!(f.metrics.cell_width, 16);
        assert_eq!(f.metrics.cell_height, 34);
    }

    #[test]
    fn text_glyphs_sit_on_the_baseline() {
        let mut f = fonts();
        let x = f.glyph('x', Style::Regular, Presentation::Text, 1, 1).expect("x");
        let m = f.metrics;
        // The x-height glyph's bottom is at the baseline, give or take its
        // antialiasing row.
        let bottom = x.offset_y - x.height as i32;
        assert!((bottom - m.cell_baseline as i32).abs() <= 1, "bottom {bottom} baseline {}", m.cell_baseline);
        assert!(x.offset_x >= 0 && (x.offset_x as u32 + x.width) <= m.cell_width + 1);
    }

    #[test]
    fn bold_is_a_different_face() {
        let mut f = fonts();
        let r = f.glyph('M', Style::Regular, Presentation::Text, 1, 1).unwrap();
        let b = f.glyph('M', Style::Bold, Presentation::Text, 1, 1).unwrap();
        assert_ne!((r.atlas_x, r.atlas_y), (b.atlas_x, b.atlas_y));
    }

    #[test]
    fn emoji_are_colour_and_cover_two_cells() {
        let mut f = fonts();
        let g = f.glyph('\u{1F680}', Style::Regular, Presentation::Emoji, 2, 2).expect("rocket");
        assert!(g.color);
        let m = f.metrics;
        assert!(g.width <= 2 * m.cell_width && g.width > m.cell_width, "width {}", g.width);
    }

    #[test]
    fn cjk_and_symbols_find_a_fallback() {
        let mut f = fonts();
        assert!(f.glyph('\u{6F22}', Style::Regular, Presentation::Text, 2, 2).is_some(), "漢");
        assert!(f.glyph('\u{23FA}', Style::Regular, Presentation::Text, 1, 1).is_some(), "⏺");
        // A Nerd Font icon comes from the bundled symbols face.
        assert!(f.glyph('\u{F07B}', Style::Regular, Presentation::Text, 1, 2).is_some(), "folder icon");
    }

    #[test]
    fn a_glyph_is_rasterized_once() {
        let mut f = fonts();
        let a = f.glyph('a', Style::Regular, Presentation::Text, 1, 1);
        let before = f.atlases.lock().unwrap().gray.modified;
        let b = f.glyph('a', Style::Regular, Presentation::Text, 1, 1);
        assert_eq!(a, b);
        assert_eq!(f.atlases.lock().unwrap().gray.modified, before);
    }
}

//! CoreText faces: loading, measuring and rasterizing glyphs, ported from
//! Ghostty's `src/font/face/coretext.zig` (MIT).
//!
//! The font is created at its pixel size, so every CoreText measurement is
//! already in device pixels. A glyph's whole-pixel position becomes its
//! bearing and the fraction is drawn into the bitmap, so subpixel placement
//! survives with one atlas entry per glyph.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2_core_foundation::{CFData, CFNumber, CFRange, CFRetained, CFString, CGFloat, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    kCGColorSpaceLinearGray, kCGColorSpaceSRGB, CGBitmapContextCreate, CGColorSpace, CGContext, CGImageByteOrderInfo,
    CGImageAlphaInfo, CGTextDrawingMode,
};
use objc2_core_text::{
    CTFont, CTFontDescriptor, CTFontManagerCreateFontDescriptorFromData, CTFontOrientation, CTFontSymbolicTraits,
    CTFontTableOptions,
};

use super::constraint::{Constraint, GlyphBox};
use super::metrics::{FaceMetrics, Metrics};

pub struct Face {
    pub font: CFRetained<CTFont>,
    /// Every glyph is a colour bitmap (an `sbix` font: Apple Color Emoji).
    pub sbix: bool,
    pub metrics: FaceMetrics,
}

/// A rasterized glyph before it goes into an atlas: `width * height`
/// coverage bytes (or BGRA when `color`), top row first.
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    /// Pixels from the cell's left edge to the bitmap's left edge.
    pub offset_x: i32,
    /// Pixels from the cell's bottom edge up to the bitmap's top edge.
    pub offset_y: i32,
    pub color: bool,
    pub data: Vec<u8>,
}

fn tag(t: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*t)
}

fn be_i16(d: &[u8], at: usize) -> Option<i16> {
    d.get(at..at + 2).map(|b| i16::from_be_bytes([b[0], b[1]]))
}

fn be_u16(d: &[u8], at: usize) -> Option<u16> {
    d.get(at..at + 2).map(|b| u16::from_be_bytes([b[0], b[1]]))
}

impl Face {
    /// A face from font file bytes (one of the fonts bundled with lmux).
    pub fn from_bytes(bytes: &[u8], px: f64) -> Option<Face> {
        let data = CFData::from_bytes(bytes);
        let desc = unsafe { CTFontManagerCreateFontDescriptorFromData(&data) }?;
        let font = unsafe { CTFont::with_font_descriptor(&desc, px as CGFloat, std::ptr::null()) };
        Some(Face::new(font))
    }

    /// An installed font by family or PostScript name.
    pub fn from_name(name: &str, px: f64) -> Option<Face> {
        let font = unsafe { CTFont::with_name(&CFString::from_str(name), px as CGFloat, std::ptr::null()) };
        // CoreText answers any name with *some* font; keep it only if it is
        // the one asked for.
        let family = unsafe { font.family_name() }.to_string();
        let ps = unsafe { font.post_script_name() }.to_string();
        let want = name.to_lowercase();
        (family.to_lowercase() == want || ps.to_lowercase() == want).then(|| Face::new(font))
    }

    pub fn new(font: CFRetained<CTFont>) -> Face {
        let traits = unsafe { font.symbolic_traits() };
        let sbix = traits.contains(CTFontSymbolicTraits::ColorGlyphsTrait) && unsafe { font.has_table(tag(b"sbix")) };
        let metrics = measure(&font);
        Face { font, sbix, metrics }
    }

    /// The same face with one variation axis set (`wght` 700 for bold).
    pub fn with_variation(&self, axis: &[u8; 4], value: f64) -> Face {
        let desc = unsafe { self.font.font_descriptor() };
        let id = CFNumber::new_i32(tag(axis) as i32);
        let desc: CFRetained<CTFontDescriptor> = unsafe { desc.copy_with_variation(&id, value as CGFloat) };
        let font = unsafe { self.font.copy_with_attributes(0.0, std::ptr::null(), Some(&desc)) };
        Face::new(font)
    }

    pub fn with_size(&self, px: f64) -> Face {
        let font = unsafe { self.font.copy_with_attributes(px as CGFloat, std::ptr::null(), None) };
        Face::new(font)
    }

    pub fn post_script_name(&self) -> String {
        unsafe { self.font.post_script_name() }.to_string()
    }

    /// The glyph for `cp`, if this face has one.
    pub fn glyph(&self, cp: char) -> Option<u16> {
        let mut units = [0u16; 2];
        let n = cp.encode_utf16(&mut units).len();
        let mut glyphs = [0u16; 2];
        let ok = unsafe {
            self.font.glyphs_for_characters(
                NonNull::new_unchecked(units.as_mut_ptr()),
                NonNull::new_unchecked(glyphs.as_mut_ptr()),
                n as isize,
            )
        };
        (ok && glyphs[0] != 0).then_some(glyphs[0])
    }

    /// The font CoreText would use for `cp` when this one lacks it, unless
    /// that is the LastResort font.
    pub fn fallback_for(&self, cp: char) -> Option<CFRetained<CTFont>> {
        let s = CFString::from_str(cp.encode_utf8(&mut [0u8; 4]));
        let len = cp.len_utf16() as isize;
        let font = unsafe { self.font.for_string(&s, CFRange::new(0, len)) };
        let ps = unsafe { font.post_script_name() }.to_string();
        (ps != "LastResort").then_some(font)
    }

    /// Rasterize `glyph` for a grid with metrics `m`, constrained as asked.
    /// `None` for an empty glyph (a space).
    pub fn render(&self, glyph: u16, m: &Metrics, constraint: Constraint, constraint_width: u8, thicken: bool) -> Option<Bitmap> {
        let mut glyphs = [glyph];
        let rect = unsafe {
            self.font.bounding_rects_for_glyphs(
                CTFontOrientation::Horizontal,
                NonNull::new_unchecked(glyphs.as_mut_ptr()),
                std::ptr::null_mut(),
                1,
            )
        };
        let (rw, rh, rx, ry) = (rect.size.width, rect.size.height, rect.origin.x, rect.origin.y);
        if rw < 0.25 || rh < 0.25 {
            return None;
        }
        let cell_w = m.cell_width as f64;
        let cell_h = m.cell_height as f64;
        let g = constraint.constrain(
            GlyphBox { width: rw, height: rh, x: rx, y: ry + m.cell_baseline as f64 },
            m,
            constraint_width,
        );
        let (mut x, mut y, mut width, mut height) = (g.x, g.y, g.width, g.height);

        // Re-centre in the rounded cell; a narrower cell keeps only the
        // fractional part, for consistent subpixel placement.
        let dx = (cell_w - m.face_width) / 2.0;
        x += dx;
        if dx < 0.0 {
            x -= dx.trunc();
        }

        // Bitmap (emoji) glyphs snap to whole pixels.
        if self.sbix {
            width = cell_w - (cell_w - width - x).round() - x.round();
            height = cell_h - (cell_h - height - y).round() - y.round();
            x = x.round();
            y = y.round();
        }

        let pad: u32 = if thicken && !self.sbix { 1 } else { 0 };
        let px_x = x.floor() as i32 - pad as i32;
        let px_y = y.floor() as i32 - pad as i32;
        let frac_x = x - x.floor();
        let frac_y = y - y.floor();
        let px_w = (width + frac_x).ceil() as u32 + 2 * pad;
        let px_h = (height + frac_y).ceil() as u32 + 2 * pad;
        if px_w == 0 || px_h == 0 {
            return None;
        }

        let color = self.sbix;
        let depth = if color { 4 } else { 1 };
        let mut buf = vec![0u8; (px_w * px_h * depth) as usize];
        let (space, info) = if color {
            (
                CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB })),
                CGImageByteOrderInfo::Order32Little.0 | CGImageAlphaInfo::PremultipliedFirst.0,
            )
        } else {
            (CGColorSpace::with_name(Some(unsafe { kCGColorSpaceLinearGray })), CGImageAlphaInfo::Only.0)
        };
        let ctx = unsafe {
            CGBitmapContextCreate(
                buf.as_mut_ptr() as *mut c_void,
                px_w as usize,
                px_h as usize,
                8,
                (px_w * depth) as usize,
                space.as_deref(),
                info,
            )
        }?;
        let c = Some(&*ctx);
        CGContext::set_allows_font_smoothing(c, true);
        CGContext::set_should_smooth_fonts(c, thicken);
        CGContext::set_allows_font_subpixel_positioning(c, true);
        CGContext::set_should_subpixel_position_fonts(c, true);
        CGContext::set_allows_font_subpixel_quantization(c, false);
        CGContext::set_should_subpixel_quantize_fonts(c, false);
        CGContext::set_allows_antialiasing(c, true);
        CGContext::set_should_antialias(c, true);
        if color {
            CGContext::set_rgb_fill_color(c, 1.0, 1.0, 1.0, 1.0);
            CGContext::set_rgb_stroke_color(c, 1.0, 1.0, 1.0, 1.0);
        } else {
            CGContext::set_gray_fill_color(c, 1.0, 1.0);
            CGContext::set_gray_stroke_color(c, 1.0, 1.0);
        }
        CGContext::set_text_drawing_mode(c, CGTextDrawingMode::Fill);
        CGContext::translate_ctm(c, frac_x + pad as f64, frac_y + pad as f64);
        CGContext::scale_ctm(c, width / rw, height / rh);
        let mut pos = [CGPoint { x: -rx, y: -ry }];
        unsafe {
            self.font.draw_glyphs(
                NonNull::new_unchecked(glyphs.as_mut_ptr()),
                NonNull::new_unchecked(pos.as_mut_ptr()),
                1,
                &ctx,
            );
        }
        drop(ctx);

        Some(Bitmap {
            width: px_w,
            height: px_h,
            offset_x: px_x,
            offset_y: px_y + px_h as i32,
            color,
            data: buf,
        })
    }
}

/// Ghostty's face measurements: vertical metrics from the raw `hhea` and
/// `OS/2` tables (CoreText's own ascent/descent fold in heuristics), the
/// cell width from the widest printable ASCII advance.
fn measure(font: &CTFont) -> FaceMetrics {
    let table = |t: &[u8; 4]| unsafe { font.table(tag(t), CTFontTableOptions::NoOptions) }.map(|d| d.to_vec());
    let px_per_em = unsafe { font.size() } as f64;
    let head = table(b"head");
    let upm = head
        .as_deref()
        .and_then(|h| be_u16(h, 18))
        .map(|v| v as f64)
        .unwrap_or_else(|| unsafe { font.units_per_em() } as f64)
        .max(1.0);
    let ppu = px_per_em / upm;
    let hhea = table(b"hhea");
    let os2 = table(b"OS/2");
    let post = table(b"post");

    let (ascent, descent, line_gap) = match (&hhea, &os2) {
        (None, _) => unsafe { (font.ascent() as f64, -(font.descent() as f64), font.leading() as f64) },
        (Some(h), None) => (
            be_i16(h, 4).unwrap_or(0) as f64 * ppu,
            be_i16(h, 6).unwrap_or(0) as f64 * ppu,
            be_i16(h, 8).unwrap_or(0) as f64 * ppu,
        ),
        (Some(h), Some(o)) => {
            let (ha, hd, hg) = (be_i16(h, 4).unwrap_or(0), be_i16(h, 6).unwrap_or(0), be_i16(h, 8).unwrap_or(0));
            let (oa, od, og) = (be_i16(o, 68).unwrap_or(0), be_i16(o, 70).unwrap_or(0), be_i16(o, 72).unwrap_or(0));
            let use_typo = be_u16(o, 62).unwrap_or(0) & (1 << 7) != 0;
            if use_typo {
                (oa as f64 * ppu, od as f64 * ppu, og as f64 * ppu)
            } else if ha != 0 || hd != 0 {
                (ha as f64 * ppu, hd as f64 * ppu, hg as f64 * ppu)
            } else if oa != 0 || od != 0 {
                (oa as f64 * ppu, od as f64 * ppu, og as f64 * ppu)
            } else {
                let wa = be_u16(o, 74).unwrap_or(0) as f64;
                let wd = be_u16(o, 76).unwrap_or(0) as f64;
                (wa * ppu, -wd * ppu, 0.0)
            }
        }
    };

    // A thickness of 0 means the table is broken; a position of 0 too only
    // when the thickness is also 0.
    let (underline_position, underline_thickness) = match post.as_deref() {
        Some(p) => {
            let pos = be_i16(p, 8).unwrap_or(0);
            let thick = be_i16(p, 10).unwrap_or(0);
            (
                (!(pos == 0 && thick == 0)).then(|| pos as f64 * ppu),
                (thick != 0).then(|| thick as f64 * ppu),
            )
        }
        None => (None, None),
    };
    let (strikethrough_position, strikethrough_thickness) = match os2.as_deref() {
        Some(o) => {
            let thick = be_i16(o, 26).unwrap_or(0);
            let pos = be_i16(o, 28).unwrap_or(0);
            (
                (!(pos == 0 && thick == 0)).then(|| pos as f64 * ppu),
                (thick != 0).then(|| thick as f64 * ppu),
            )
        }
        None => (None, None),
    };
    let os2_version = os2.as_deref().and_then(|o| be_u16(o, 0)).unwrap_or(0);
    let (cap_height, ex_height) = match os2.as_deref() {
        Some(o) if os2_version >= 2 => (
            be_i16(o, 88).map(|v| v as f64 * ppu),
            be_i16(o, 86).map(|v| v as f64 * ppu),
        ),
        _ => unsafe { (Some(font.cap_height() as f64), Some(font.x_height() as f64)) },
    };

    // Widest printable ASCII advance and their combined height.
    let mut chars: Vec<u16> = (32u16..127).collect();
    let mut glyphs = vec![0u16; chars.len()];
    let mut advances = vec![CGSize { width: 0.0, height: 0.0 }; chars.len()];
    let (cell_width, ascii_height) = unsafe {
        font.glyphs_for_characters(
            NonNull::new_unchecked(chars.as_mut_ptr()),
            NonNull::new_unchecked(glyphs.as_mut_ptr()),
            chars.len() as isize,
        );
        font.advances_for_glyphs(
            CTFontOrientation::Horizontal,
            NonNull::new_unchecked(glyphs.as_mut_ptr()),
            advances.as_mut_ptr(),
            glyphs.len() as isize,
        );
        let rect: CGRect = font.bounding_rects_for_glyphs(
            CTFontOrientation::Horizontal,
            NonNull::new_unchecked(glyphs.as_mut_ptr()),
            std::ptr::null_mut(),
            glyphs.len() as isize,
        );
        let max = advances.iter().map(|a| a.width as f64).fold(0.0, f64::max);
        (max, rect.size.height as f64)
    };

    // '水' sizes fallback faces to the primary, when it fits its advance.
    let ic_width = {
        let mut ch = [0x6C34u16];
        let mut g = [0u16];
        unsafe {
            if font.glyphs_for_characters(NonNull::new_unchecked(ch.as_mut_ptr()), NonNull::new_unchecked(g.as_mut_ptr()), 1)
                && g[0] != 0
            {
                let mut adv = [CGSize { width: 0.0, height: 0.0 }];
                font.advances_for_glyphs(CTFontOrientation::Horizontal, NonNull::new_unchecked(g.as_mut_ptr()), adv.as_mut_ptr(), 1);
                let r = font.bounding_rects_for_glyphs(
                    CTFontOrientation::Horizontal,
                    NonNull::new_unchecked(g.as_mut_ptr()),
                    std::ptr::null_mut(),
                    1,
                );
                (r.size.width <= adv[0].width).then_some(adv[0].width as f64)
            } else {
                None
            }
        }
    };

    FaceMetrics {
        px_per_em,
        cell_width,
        ascent,
        descent,
        line_gap,
        underline_position,
        underline_thickness,
        strikethrough_position,
        strikethrough_thickness,
        cap_height,
        ex_height,
        ascii_height: Some(ascii_height),
        ic_width,
    }
}

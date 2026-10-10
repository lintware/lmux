//! Cell metrics from a face's measurements, ported from Ghostty's
//! `src/font/Metrics.zig` (MIT). Every number the grid and the sprite
//! glyphs use is a whole pixel, computed once per font size.

/// What a face reports, in pixels, +Y up, relative to the baseline.
#[derive(Clone, Copy, Debug, Default)]
pub struct FaceMetrics {
    pub px_per_em: f64,
    /// Widest advance among printable ASCII (unrounded).
    pub cell_width: f64,
    pub ascent: f64,
    /// Negative: below the baseline.
    pub descent: f64,
    pub line_gap: f64,
    /// Top of the underline stroke, relative to the baseline.
    pub underline_position: Option<f64>,
    pub underline_thickness: Option<f64>,
    pub strikethrough_position: Option<f64>,
    pub strikethrough_thickness: Option<f64>,
    pub cap_height: Option<f64>,
    pub ex_height: Option<f64>,
    pub ascii_height: Option<f64>,
    /// Advance of '水', for sizing fallback faces to the primary.
    pub ic_width: Option<f64>,
}

impl FaceMetrics {
    pub fn line_height(&self) -> f64 {
        self.ascent - self.descent + self.line_gap
    }
    pub fn cap_height(&self) -> f64 {
        self.cap_height.filter(|v| *v > 0.0).unwrap_or(0.75 * self.ascent)
    }
    pub fn ex_height(&self) -> f64 {
        self.ex_height.filter(|v| *v > 0.0).unwrap_or(0.75 * self.cap_height())
    }
    pub fn ascii_height(&self) -> f64 {
        self.ascii_height.filter(|v| *v > 0.0).unwrap_or(1.5 * self.cap_height())
    }
    pub fn ic_width(&self) -> f64 {
        self.ic_width.filter(|v| *v > 0.0).unwrap_or(self.ascii_height().min(2.0 * self.cell_width))
    }
    pub fn underline_thickness(&self) -> f64 {
        self.underline_thickness.filter(|v| *v > 0.0).unwrap_or(0.15 * self.ex_height())
    }
    pub fn strikethrough_thickness(&self) -> f64 {
        self.strikethrough_thickness.filter(|v| *v > 0.0).unwrap_or(self.underline_thickness())
    }
    pub fn underline_position(&self) -> f64 {
        self.underline_position.unwrap_or(-self.underline_thickness())
    }
    pub fn strikethrough_position(&self) -> f64 {
        self.strikethrough_position.unwrap_or((self.ex_height() + self.strikethrough_thickness()) * 0.5)
    }
}

/// The grid's metrics. Positions are pixels from the cell's top to the top
/// of the stroke, except `cell_baseline`, which is from the cell's bottom.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metrics {
    pub cell_width: u32,
    pub cell_height: u32,
    pub cell_baseline: u32,
    pub underline_position: u32,
    pub underline_thickness: u32,
    pub strikethrough_position: u32,
    pub strikethrough_thickness: u32,
    pub overline_position: i32,
    pub overline_thickness: u32,
    pub box_thickness: u32,
    pub cursor_thickness: u32,
    pub cursor_height: u32,
    pub icon_height: f64,
    pub icon_height_single: f64,
    pub face_width: f64,
    pub face_height: f64,
    pub face_y: f64,
}

impl Metrics {
    pub fn calc(face: &FaceMetrics) -> Metrics {
        let face_width = face.cell_width;
        let face_height = face.line_height();
        let cell_width = face_width.round();
        let cell_height = face_height.round();

        // Half the line gap above the text, half below.
        let half_line_gap = face.line_gap / 2.0;
        let face_baseline = half_line_gap - face.descent;
        let cell_baseline = (face_baseline - (cell_height - face_height) / 2.0).round();
        let face_y = cell_baseline - face_baseline;
        let top_to_baseline = cell_height - cell_baseline;

        let cap_height = face.cap_height();
        let underline_thickness = face.underline_thickness().ceil().max(1.0);
        let strikethrough_thickness = face.strikethrough_thickness().ceil().max(1.0);
        let underline_position = (top_to_baseline - face.underline_position()).round();
        let strikethrough_position = (top_to_baseline - face.strikethrough_position()).round();

        let mut m = Metrics {
            cell_width: cell_width as u32,
            cell_height: cell_height as u32,
            cell_baseline: cell_baseline.max(0.0) as u32,
            underline_position: underline_position.max(0.0) as u32,
            underline_thickness: underline_thickness as u32,
            strikethrough_position: strikethrough_position.max(0.0) as u32,
            strikethrough_thickness: strikethrough_thickness as u32,
            overline_position: 0,
            overline_thickness: underline_thickness as u32,
            box_thickness: underline_thickness as u32,
            cursor_thickness: 1,
            cursor_height: cell_height as u32,
            icon_height: face_height,
            icon_height_single: (2.0 * cap_height + face_height) / 3.0,
            face_width,
            face_height,
            face_y,
        };
        m.clamp();
        m
    }

    fn clamp(&mut self) {
        for v in [
            &mut self.cell_width,
            &mut self.cell_height,
            &mut self.underline_thickness,
            &mut self.strikethrough_thickness,
            &mut self.overline_thickness,
            &mut self.box_thickness,
            &mut self.cursor_thickness,
            &mut self.cursor_height,
        ] {
            *v = (*v).max(1);
        }
        self.icon_height = self.icon_height.max(1.0);
        self.icon_height_single = self.icon_height_single.max(1.0);
        self.face_width = self.face_width.max(1.0);
        self.face_height = self.face_height.max(1.0);
    }

    /// `adjust-cell-height`: grow or shrink the cell, keeping the text
    /// centred the way Ghostty does (the extra goes half above, half below).
    pub fn adjust_cell_height(&mut self, new: u32) {
        let new = new.max(1);
        let original = self.cell_height as f64;
        let diff = new as f64 - original;
        let half = diff / 2.0;
        let above_center = self.face_y - (original - self.face_height) / 2.0;
        let (top, bottom) = if above_center > 0.0 { (half.ceil(), half.floor()) } else { (half.floor(), half.ceil()) };
        let add = |v: u32, d: f64| (v as f64 + d).max(0.0) as u32;
        self.cell_height = new;
        self.cell_baseline = add(self.cell_baseline, bottom);
        self.face_y += bottom;
        self.underline_position = add(self.underline_position, top);
        self.strikethrough_position = add(self.strikethrough_position, top);
        self.overline_position += top as i32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// JetBrains Mono's table values at 16px: 21.12px of line height
    /// rounds to a 21px cell, and the baseline lands 5px up.
    #[test]
    fn jetbrains_mono_16px_grid() {
        let upm = 1000.0;
        let px = 16.0 / upm;
        let face = FaceMetrics {
            px_per_em: 16.0,
            cell_width: 600.0 * px,
            ascent: 1020.0 * px,
            descent: -300.0 * px,
            line_gap: 0.0,
            underline_position: Some(-150.0 * px),
            underline_thickness: Some(50.0 * px),
            strikethrough_position: Some(320.0 * px),
            strikethrough_thickness: Some(50.0 * px),
            cap_height: Some(730.0 * px),
            ex_height: Some(550.0 * px),
            ascii_height: None,
            ic_width: None,
        };
        let m = Metrics::calc(&face);
        assert_eq!((m.cell_width, m.cell_height, m.cell_baseline), (10, 21, 5));
        assert_eq!((m.underline_thickness, m.strikethrough_thickness), (1, 1));
        assert_eq!(m.box_thickness, m.underline_thickness);
        assert_eq!(m.cursor_height, m.cell_height);
        assert!((m.face_width - 9.6).abs() < 1e-9);
    }

    #[test]
    fn taller_cell_moves_baseline_and_lines_together() {
        let face = FaceMetrics { px_per_em: 20.0, cell_width: 12.0, ascent: 16.0, descent: -4.0, ..Default::default() };
        let mut m = Metrics::calc(&face);
        let (base, ul) = (m.cell_baseline, m.underline_position);
        m.adjust_cell_height(m.cell_height + 4);
        assert_eq!(m.cell_baseline, base + 2);
        assert_eq!(m.underline_position, ul + 2);
    }
}

//! Load real macOS faces (SF Pro / SF Mono) so the app does not render in
//! egui's default Ubuntu/Hack look. Falls back gracefully if a file is missing.

use std::sync::Arc;

use egui::{FontData, FontDefinitions, FontFamily};

use crate::theme;

struct Face {
    file: &'static str,
    index: u32,
    /// Variable-font weight, if the face is variable.
    weight: Option<f32>,
}

/// The pixel face of the site and the app icon (OFL, assets/fonts/Silkscreen-OFL.txt).
static PIXEL: &[u8] = include_bytes!("../assets/fonts/Silkscreen-Regular.ttf");

const UI_FACES: &[Face] = &[
    Face { file: "/System/Library/Fonts/SFNS.ttf", index: 0, weight: None },
    Face { file: "/System/Library/Fonts/SFNSDisplay.ttf", index: 0, weight: None },
    Face { file: "/System/Library/Fonts/HelveticaNeue.ttc", index: 0, weight: None },
    Face { file: "/System/Library/Fonts/Helvetica.ttc", index: 0, weight: None },
];

const UI_SEMIBOLD_FACES: &[Face] = &[
    Face { file: "/System/Library/Fonts/SFNS.ttf", index: 0, weight: Some(600.0) },
    Face { file: "/System/Library/Fonts/SFNSDisplay.ttf", index: 0, weight: Some(600.0) },
    Face { file: "/System/Library/Fonts/HelveticaNeue.ttc", index: 1, weight: None },
    Face { file: "/System/Library/Fonts/Helvetica.ttc", index: 1, weight: None },
];

const MONO_FACES: &[Face] = &[
    Face { file: "/System/Library/Fonts/SFNSMono.ttf", index: 0, weight: None },
    Face { file: "/System/Library/Fonts/Menlo.ttc", index: 0, weight: None },
    Face { file: "/System/Library/Fonts/Monaco.ttf", index: 0, weight: None },
];

/// Fallback faces for symbols, in order.
const SYMBOL_FACES: &[(&str, &str)] = &[
    ("menlo-fallback", "/System/Library/Fonts/Menlo.ttc"),
    ("apple-symbols", "/System/Library/Fonts/Apple Symbols.ttf"),
    // CJK, last: it is large and only consulted for glyphs the others lack.
    ("hiragino-sans-gb", "/System/Library/Fonts/Hiragino Sans GB.ttc"),
];

fn read_face(face: &Face) -> Option<FontData> {
    let bytes = std::fs::read(face.file).ok()?;
    let mut data = FontData { font: bytes.into(), index: face.index, tweak: Default::default() };
    if let Some(w) = face.weight {
        data.tweak.coords = egui::epaint::text::VariationCoords::new([("wght", w)]);
    }
    Some(data)
}

fn register(defs: &mut FontDefinitions, key: &str, faces: &[Face], families: &[FontFamily]) -> Option<String> {
    for face in faces {
        if let Some(data) = read_face(face) {
            defs.font_data.insert(key.to_owned(), Arc::new(data));
            for fam in families {
                defs.families.entry(fam.clone()).or_default().insert(0, key.to_owned());
            }
            return Some(face.file.to_owned());
        }
    }
    None
}

/// Install fonts on the context. Returns `(ui_source, mono_source)` for logging.
pub fn install(ctx: &egui::Context) -> (String, String) {
    let mut defs = FontDefinitions::default();

    // Keep egui's built-in families as fallbacks for emoji/pictographs.
    let prop_fallbacks = defs
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let mono_fallbacks = defs
        .families
        .get(&FontFamily::Monospace)
        .cloned()
        .unwrap_or_default();

    let ui = register(
        &mut defs,
        theme::UI_FAMILY,
        UI_FACES,
        &[FontFamily::Proportional, FontFamily::Name(theme::UI_FAMILY.into())],
    );
    let sb = register(
        &mut defs,
        theme::UI_SEMIBOLD_FAMILY,
        UI_SEMIBOLD_FACES,
        &[FontFamily::Name(theme::UI_SEMIBOLD_FAMILY.into())],
    );
    let mono = register(
        &mut defs,
        theme::MONO_FAMILY,
        MONO_FACES,
        &[FontFamily::Monospace, FontFamily::Name(theme::MONO_FAMILY.into())],
    );

    defs.font_data.insert(theme::PIXEL_FAMILY.to_owned(), Arc::new(FontData::from_static(PIXEL)));
    defs.families.insert(FontFamily::Name(theme::PIXEL_FAMILY.into()), vec![theme::PIXEL_FAMILY.to_owned()]);

    // Symbols SF lacks (◆ ⏺ ✻ ⎿ braille spinners, box drawing…) come from
    // Menlo and Apple Symbols, then egui's own faces. Every family gets the
    // whole chain, the named ones too: theme::ui() draws with a named family,
    // so a family without fallbacks paints `?` for a glyph its face lacks.
    let mut symbols: Vec<String> = Vec::new();
    for (key, file) in SYMBOL_FACES {
        if let Some(data) = read_face(&Face { file, index: 0, weight: None }) {
            defs.font_data.insert((*key).to_owned(), Arc::new(data));
            symbols.push((*key).to_owned());
        }
    }
    let ui_fams = [
        FontFamily::Proportional,
        FontFamily::Name(theme::PIXEL_FAMILY.into()),
        FontFamily::Name(theme::UI_FAMILY.into()),
        FontFamily::Name(theme::UI_SEMIBOLD_FAMILY.into()),
    ];
    let mono_fams = [FontFamily::Monospace, FontFamily::Name(theme::MONO_FAMILY.into())];
    for fam in &ui_fams {
        let list = defs.families.entry(fam.clone()).or_default();
        // The pixel face covers ASCII; anything else falls back to SF.
        let ui_key = ui.as_ref().map(|_| theme::UI_FAMILY.to_owned());
        for key in ui_key.iter().chain(symbols.iter()).chain(prop_fallbacks.iter()) {
            append_unique(list, key);
        }
    }
    for fam in &mono_fams {
        let list = defs.families.entry(fam.clone()).or_default();
        for key in symbols.iter().chain(mono_fallbacks.iter()).chain(prop_fallbacks.iter()) {
            append_unique(list, key);
        }
    }

    if ui.is_none() {
        eprintln!("[fonts] WARNING: no system UI font found; using egui defaults");
    }
    if sb.is_none() {
        eprintln!("[fonts] WARNING: no semibold face; titles will use the regular face");
    }
    if mono.is_none() {
        eprintln!("[fonts] WARNING: no system mono font found; using egui defaults");
    }

    ctx.set_fonts(defs);

    (
        ui.unwrap_or_else(|| "builtin".into()),
        mono.unwrap_or_else(|| "builtin".into()),
    )
}

fn append_unique(list: &mut Vec<String>, key: &str) {
    if !list.iter().any(|k| k == key) {
        list.push(key.to_owned());
    }
}

//! The user's Ghostty configuration and themes, ported from Ghostty's src/config/Config.zig, src/config/theme.zig,
//! src/terminal/color.zig and src/cli/args.zig (MIT).
//!
//! Only the keys the renderer honours are kept; every other key is ignored silently, as are values that fail to parse
//! (Ghostty reports a diagnostic and keeps the previous value). Files are read the way Ghostty reads them on macOS:
//! the theme is applied first and the user's lines are replayed on top, so explicit settings always win.
//!
//! This file is self-contained (std only) so it can be tested on its own with `rustc --test`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub type Rgb = [u8; 3];

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CursorStyle {
    Block,
    Bar,
    Underline,
    BlockHollow,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BoldColor {
    Bright,
    Color(Rgb),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CellColor {
    Color(Rgb),
    CellForeground,
    CellBackground,
}

/// A cell metric adjustment (Ghostty's `Metrics.Modifier`). `Percent` holds the multiplier, so `20%` is `1.2`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Modifier {
    Percent(f64),
    Absolute(i32),
}

#[derive(Clone, Debug, PartialEq)]
pub struct TermConfig {
    pub font_family: Vec<String>,
    pub font_family_bold: Vec<String>,
    pub font_family_italic: Vec<String>,
    pub font_family_bold_italic: Vec<String>,
    pub font_size: Option<f32>,
    pub font_thicken: bool,
    pub font_thicken_strength: u8,
    pub adjust_cell_width: Option<Modifier>,
    pub adjust_cell_height: Option<Modifier>,
    pub background: Rgb,
    pub foreground: Rgb,
    pub palette: [Rgb; 256],
    pub cursor_color: Option<CellColor>,
    pub cursor_text: Option<CellColor>,
    pub cursor_opacity: f32,
    pub cursor_style: CursorStyle,
    pub cursor_style_blink: Option<bool>,
    pub selection_background: Option<CellColor>,
    pub selection_foreground: Option<CellColor>,
    pub minimum_contrast: f32,
    pub faint_opacity: f32,
    pub bold_color: Option<BoldColor>,
    pub window_padding_x: (u32, u32),
    pub window_padding_y: (u32, u32),
    pub window_padding_balance: bool,
}

/// The longest config line Ghostty reads (cli/args.zig); longer lines are skipped.
const MAX_LINE: usize = 4096;
/// How deep `config-file` includes may nest before they are skipped.
const MAX_INCLUDE_DEPTH: u8 = 10;

/// The 16 base colours of lmux's default theme: the pixel-fire palette of the app icon and the site, warm ink and
/// cream with flame reds and oranges, muted so long agent transcripts stay easy to read. Ghostty's own default is a
/// Tomorrow Night set (terminal/color.zig:495); a `palette` or `theme` line replaces either.
const BASE16: [Rgb; 16] = [
    [0x24, 0x22, 0x1d],
    [0xe0, 0x50, 0x2c],
    [0xb3, 0xbc, 0x88],
    [0xe5, 0xad, 0x71],
    [0x7f, 0xa6, 0xbd],
    [0xc8, 0x83, 0xa6],
    [0x8c, 0xbc, 0xaa],
    [0xd8, 0xd4, 0xc9],
    [0x74, 0x72, 0x68],
    [0xf0, 0x7a, 0x3c],
    [0xcd, 0xd6, 0xa0],
    [0xff, 0xc2, 0x7a],
    [0x9c, 0xc2, 0xd8],
    [0xe0, 0x9e, 0xc0],
    [0xa8, 0xd8, 0xc6],
    [0xff, 0xf4, 0xe4],
];

/// The default 256-colour palette: the base 16 above, then the xterm cube and gray ramp (color.zig:12).
fn default_palette() -> [Rgb; 256] {
    let mut palette = [[0u8; 3]; 256];
    palette[..16].copy_from_slice(&BASE16);
    let level = |v: u8| if v == 0 { 0 } else { v * 40 + 55 };
    let mut i = 16;
    for r in 0..6u8 {
        for g in 0..6u8 {
            for b in 0..6u8 {
                palette[i] = [level(r), level(g), level(b)];
                i += 1;
            }
        }
    }
    for (n, entry) in palette[232..].iter_mut().enumerate() {
        let v = n as u8 * 10 + 8;
        *entry = [v, v, v];
    }
    palette
}

impl Default for TermConfig {
    /// Ghostty's defaults, with lmux's pixel-fire theme for the palette, cursor and selection, and the site's ink
    /// `#12110E` and cream `#EAE6DC` for background and foreground (they match the window around the terminal).
    fn default() -> Self {
        TermConfig {
            font_family: Vec::new(),
            font_family_bold: Vec::new(),
            font_family_italic: Vec::new(),
            font_family_bold_italic: Vec::new(),
            font_size: None,
            font_thicken: false,
            font_thicken_strength: 255,
            adjust_cell_width: None,
            adjust_cell_height: None,
            background: [0x12, 0x11, 0x0e],
            foreground: [0xea, 0xe6, 0xdc],
            palette: default_palette(),
            cursor_color: Some(CellColor::Color([0xf0, 0x7a, 0x3c])),
            cursor_text: Some(CellColor::Color([0x12, 0x11, 0x0e])),
            cursor_opacity: 1.0,
            cursor_style: CursorStyle::Block,
            cursor_style_blink: None,
            selection_background: Some(CellColor::Color([0x6b, 0x2c, 0x18])),
            selection_foreground: Some(CellColor::Color([0xff, 0xf4, 0xe4])),
            minimum_contrast: 1.0,
            faint_opacity: 0.5,
            bold_color: None,
            window_padding_x: (2, 2),
            window_padding_y: (2, 2),
            window_padding_balance: false,
        }
    }
}

/// State shared by every file of one load: the `config-file` queue (which grows while it is drained, like
/// Config.zig:4420 `loadRecursiveFiles`) and the last `theme` value seen.
#[derive(Default)]
struct Pending {
    /// Include paths with the nesting depth of the file that named them.
    includes: Vec<(PathBuf, u8)>,
    /// Index of the first include not yet processed.
    next: usize,
    /// Depth of the file currently being applied.
    depth: u8,
    theme: Option<String>,
}

impl TermConfig {
    /// Parse one config file's text into self (later lines win). `dir` resolves relative `config-file` paths.
    ///
    /// `config-file` includes are applied after the text, as Ghostty does. A `theme` key is ignored here, because
    /// choosing between its light and dark halves needs the appearance; [`TermConfig::load`] resolves themes.
    pub fn apply_text(&mut self, text: &str, dir: &Path, depth: u8) {
        let mut pending = Pending { depth, ..Pending::default() };
        self.apply_lines(text, dir, &mut pending, true);
        self.run_includes(&mut pending, &mut HashSet::new());
    }

    /// Load the user's config the way Ghostty does on macOS: the theme first, then the user's lines replayed on top.
    ///
    /// Files read, in order: `$XDG_CONFIG_HOME/ghostty/config` and `config.ghostty` (default `~/.config`), then
    /// `~/Library/Application Support/com.mitchellh.ghostty/config` and `config.ghostty`; then `config-file`
    /// includes (a `?` prefix marks one optional). `theme` is a name, an absolute path, or `light:A,dark:B`, and
    /// `dark` picks the half to use.
    pub fn load(dark: bool) -> TermConfig {
        Self::load_from(&default_config_files(), &theme_dirs(), dark)
    }

    fn load_from(files: &[PathBuf], theme_dirs: &[PathBuf], dark: bool) -> TermConfig {
        let mut cfg = TermConfig::default();
        let mut theme = None;
        cfg.replay_files(files, &mut theme);
        let Some(theme) = theme else { return cfg };
        let Some((light_name, dark_name)) = parse_theme(&theme) else { return cfg };
        let name = if dark { dark_name } else { light_name };
        let Some(path) = resolve_theme(&name, theme_dirs) else { return cfg };
        let Some(text) = read_text(&path) else { return cfg };

        // Config.zig:4647: the theme goes in first, then every user step is replayed over it. A theme file cannot
        // set `theme` or `config-file`.
        let mut themed = TermConfig::default();
        let dir = path.parent().unwrap_or_else(|| Path::new(""));
        themed.apply_lines(&text, dir, &mut Pending::default(), false);
        themed.replay_files(files, &mut None);
        themed
    }

    /// Apply each file, then everything they include; `theme` receives the last `theme` value seen.
    fn replay_files(&mut self, files: &[PathBuf], theme: &mut Option<String>) {
        let mut pending = Pending::default();
        let mut loaded = HashSet::new();
        for file in files {
            let Some(text) = read_text(file) else { continue };
            loaded.insert(canonical(file));
            let dir = file.parent().unwrap_or_else(|| Path::new(""));
            self.apply_lines(&text, dir, &mut pending, true);
        }
        self.run_includes(&mut pending, &mut loaded);
        *theme = pending.theme;
    }

    /// Drain the include queue; files named by an included file are appended and drained in turn.
    fn run_includes(&mut self, pending: &mut Pending, loaded: &mut HashSet<PathBuf>) {
        while pending.next < pending.includes.len() {
            let (path, depth) = pending.includes[pending.next].clone();
            pending.next += 1;
            if depth > MAX_INCLUDE_DEPTH {
                continue;
            }
            // Config.zig:4458: a file is loaded once, which also stops include cycles.
            if !loaded.insert(canonical(&path)) {
                continue;
            }
            let Some(text) = read_text(&path) else { continue };
            pending.depth = depth;
            let dir = path.parent().unwrap_or_else(|| Path::new(""));
            self.apply_lines(&text, dir, pending, true);
        }
    }

    /// Apply the `key = value` lines of one file. With `meta` false (a theme file) `theme` and `config-file` are
    /// ignored.
    fn apply_lines(&mut self, text: &str, dir: &Path, pending: &mut Pending, meta: bool) {
        for (key, value) in entries(text) {
            if key == "config-file" || key == "theme" {
                if meta {
                    if let Some(value) = value {
                        pending.meta(key, value, dir);
                    }
                }
                continue;
            }
            self.set(key, value);
        }
    }

    /// Set one key. Unknown keys and unparsable values leave the field alone; an empty value resets it to the
    /// default (cli/args.zig:318).
    fn set(&mut self, key: &str, value: Option<&str>) {
        let v = match value {
            Some(v) => v,
            // A bare key is `key = t` for booleans and a missing value otherwise (args.zig:416).
            None if is_bool_key(key) => "t",
            None => return,
        };
        let defaults = v.is_empty().then(TermConfig::default);

        // Assign `$parse` (an `Option<T>`) to the field, or the default when the value is empty.
        macro_rules! field {
            ($field:ident, $parse:expr) => {
                match defaults {
                    Some(d) => self.$field = d.$field,
                    None => {
                        if let Some(parsed) = $parse {
                            self.$field = parsed;
                        }
                    }
                }
            };
        }

        match key {
            "font-family" => list(&mut self.font_family, v),
            "font-family-bold" => list(&mut self.font_family_bold, v),
            "font-family-italic" => list(&mut self.font_family_italic, v),
            "font-family-bold-italic" => list(&mut self.font_family_bold_italic, v),
            "font-size" => field!(font_size, parse_float(v).filter(|s| *s > 0.0).map(Some)),
            "font-thicken" => field!(font_thicken, parse_bool(v)),
            "font-thicken-strength" => field!(font_thicken_strength, v.parse::<u8>().ok()),
            "adjust-cell-width" => field!(adjust_cell_width, parse_modifier(v).map(Some)),
            "adjust-cell-height" => field!(adjust_cell_height, parse_modifier(v).map(Some)),
            "background" => field!(background, parse_color(v)),
            "foreground" => field!(foreground, parse_color(v)),
            "palette" => {
                if defaults.is_some() {
                    self.palette = default_palette();
                } else if let Some((index, color)) = parse_palette_entry(v) {
                    self.palette[index] = color;
                }
            }
            "cursor-color" => field!(cursor_color, parse_cell_color(v).map(Some)),
            "cursor-text" => field!(cursor_text, parse_cell_color(v).map(Some)),
            "cursor-opacity" => field!(cursor_opacity, parse_float(v).map(|o| o.clamp(0.0, 1.0))),
            "cursor-style" => field!(cursor_style, parse_cursor_style(v)),
            "cursor-style-blink" => field!(cursor_style_blink, parse_bool(v).map(Some)),
            "selection-background" => field!(selection_background, parse_cell_color(v).map(Some)),
            "selection-foreground" => field!(selection_foreground, parse_cell_color(v).map(Some)),
            "minimum-contrast" => field!(minimum_contrast, parse_float(v).map(|c| c.clamp(1.0, 21.0))),
            "faint-opacity" => field!(faint_opacity, parse_float(v).map(|o| o.clamp(0.0, 1.0))),
            "bold-color" => field!(bold_color, parse_bold_color(v).map(Some)),
            // Config.zig:5095: the deprecated alias only ever turns `bright` on.
            "bold-is-bright" => {
                if parse_bool(v) == Some(true) {
                    self.bold_color = Some(BoldColor::Bright);
                }
            }
            "window-padding-x" => field!(window_padding_x, parse_padding(v)),
            "window-padding-y" => field!(window_padding_y, parse_padding(v)),
            "window-padding-balance" => field!(window_padding_balance, parse_balance(v)),
            _ => {}
        }
    }
}

impl Pending {
    /// Record a `config-file` or `theme` line.
    fn meta(&mut self, key: &str, value: &str, dir: &Path) {
        if key == "theme" {
            self.theme = (!value.is_empty()).then(|| value.to_string());
            return;
        }
        // An empty value clears the entries that have not been processed yet.
        if value.is_empty() {
            self.includes.truncate(self.next);
            return;
        }
        // Path.parse (path.zig:50): an optional `?`, then optional quotes. A missing file is skipped either way.
        let value = value.strip_prefix('?').unwrap_or(value);
        let value = strip_quotes(value);
        if value.is_empty() {
            return;
        }
        let path = PathBuf::from(expand_home(value));
        let path = if path.is_absolute() { path } else { dir.join(path) };
        self.includes.push((path, self.depth.saturating_add(1)));
    }
}

/// Where theme files are searched, in order: `~/.config/ghostty/themes` (`$XDG_CONFIG_HOME` when set), then
/// the Ghostty and cmux app bundles' resources.
pub fn theme_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(xdg) = xdg_config_home() {
        dirs.push(xdg.join("ghostty").join("themes"));
    }
    dirs.push(PathBuf::from("/Applications/Ghostty.app/Contents/Resources/ghostty/themes"));
    dirs.push(PathBuf::from("/Applications/cmux.app/Contents/Resources/ghostty/themes"));
    dirs
}

/// The config files Ghostty reads on macOS, in load order (file_load.zig, Config.zig:4238): within each directory
/// the legacy `config` comes before `config.ghostty`.
fn default_config_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    if let Some(xdg) = xdg_config_home() {
        dirs.push(xdg.join("ghostty"));
    }
    if let Some(home) = home_dir() {
        dirs.push(home.join("Library/Application Support/com.mitchellh.ghostty"));
    }
    for dir in dirs {
        files.push(dir.join("config"));
        files.push(dir.join("config.ghostty"));
    }
    files
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}

fn xdg_config_home() -> Option<PathBuf> {
    match std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => Some(dir),
        _ => home_dir().map(|h| h.join(".config")),
    }
}

/// `~/...` becomes `$HOME/...`; anything else is returned unchanged.
fn expand_home(path: &str) -> String {
    match (path.strip_prefix("~/"), home_dir()) {
        (Some(rest), Some(home)) => home.join(rest).to_string_lossy().into_owned(),
        _ => path.to_string(),
    }
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Read a regular file; anything else (missing, a directory, unreadable) is skipped like a Ghostty diagnostic.
fn read_text(path: &Path) -> Option<String> {
    if !std::fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Split a theme value into its `(light, dark)` names (Config.zig:10130). A bare name serves both appearances;
/// otherwise both `light:` and `dark:` are required, in either order.
fn parse_theme(input: &str) -> Option<(String, String)> {
    if input.is_empty() {
        return None;
    }
    if !input.contains([',', '=', ':']) {
        let name = input.trim_matches(is_ws).to_string();
        return Some((name.clone(), name));
    }
    let (mut light, mut dark) = (None, None);
    for entry in input.split(',') {
        let (key, value) = entry.split_once(':')?;
        let value = strip_quotes(value.trim_matches(is_ws)).to_string();
        match key.trim_matches(is_ws) {
            "light" => light = Some(value),
            "dark" => dark = Some(value),
            _ => return None,
        }
    }
    Some((light?, dark?))
}

/// Find a theme file (theme.zig:105): an absolute path is tried alone; a name must have no separators and is
/// looked up in each directory, first existing wins and must be a regular file.
fn resolve_theme(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    let name = expand_home(name);
    let path = Path::new(&name);
    if path.is_absolute() {
        return path.is_file().then(|| path.to_path_buf());
    }
    if name.is_empty() || name.contains('/') {
        return None;
    }
    for dir in dirs {
        let candidate = dir.join(&name);
        if candidate.exists() {
            return candidate.is_file().then_some(candidate);
        }
    }
    None
}

fn is_ws(c: char) -> bool {
    c == ' ' || c == '\t'
}

fn strip_quotes(s: &str) -> &str {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// The `key = value` entries of a config file (cli/args.zig:1440): whitespace and CR are trimmed, blank lines and
/// `#` comments are skipped, and a value in double quotes loses them. A line without `=` is a bare key.
fn entries(text: &str) -> impl Iterator<Item = (&str, Option<&str>)> {
    text.lines().filter_map(|raw| {
        let line = raw.trim_matches(|c| is_ws(c) || c == '\r');
        if line.is_empty() || line.starts_with('#') || line.len() > MAX_LINE {
            return None;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Some((line, None));
        };
        Some((key.trim_matches(is_ws), Some(strip_quotes(value.trim_matches(is_ws)))))
    })
}

fn is_bool_key(key: &str) -> bool {
    matches!(key, "font-thicken" | "cursor-style-blink" | "bold-is-bright" | "window-padding-balance")
}

/// Repeatable string options: an empty value clears the list.
fn list(families: &mut Vec<String>, value: &str) {
    if value.is_empty() {
        families.clear();
    } else {
        families.push(value.to_string());
    }
}

/// cli/args.zig:649.
fn parse_bool(v: &str) -> Option<bool> {
    match v {
        "1" | "t" | "T" | "true" => Some(true),
        "0" | "f" | "F" | "false" => Some(false),
        _ => None,
    }
}

fn parse_float(v: &str) -> Option<f32> {
    v.trim_matches(is_ws).parse::<f32>().ok().filter(|f| f.is_finite())
}

/// `window-padding-balance`: a boolean, or `equal` (an alternative balancing that still balances).
fn parse_balance(v: &str) -> Option<bool> {
    if v == "equal" {
        return Some(true);
    }
    parse_bool(v)
}

fn parse_cursor_style(v: &str) -> Option<CursorStyle> {
    match v {
        "block" => Some(CursorStyle::Block),
        "bar" => Some(CursorStyle::Bar),
        "underline" => Some(CursorStyle::Underline),
        "block_hollow" => Some(CursorStyle::BlockHollow),
        _ => None,
    }
}

fn parse_cell_color(v: &str) -> Option<CellColor> {
    match v {
        "cell-foreground" => Some(CellColor::CellForeground),
        "cell-background" => Some(CellColor::CellBackground),
        _ => parse_color(v).map(CellColor::Color),
    }
}

/// Config.zig:5848.
fn parse_bold_color(v: &str) -> Option<BoldColor> {
    if v == "bright" {
        return Some(BoldColor::Bright);
    }
    parse_color(v).map(BoldColor::Color)
}

/// `20%` is a 20% larger value (a multiplier of 1.2), a plain integer is an absolute delta (font/Metrics.zig:458).
fn parse_modifier(v: &str) -> Option<Modifier> {
    if v.is_empty() {
        return None;
    }
    if let Some(percent) = v.strip_suffix('%') {
        let percent = percent.parse::<f64>().ok()? / 100.0;
        if percent <= -1.0 {
            return Some(Modifier::Percent(0.0));
        }
        return Some(Modifier::Percent(1.0 + percent));
    }
    v.parse::<i32>().ok().map(Modifier::Absolute)
}

/// `a` sets both sides, `a,b` sets (top or left, bottom or right) (Config.zig:10439).
fn parse_padding(v: &str) -> Option<(u32, u32)> {
    let number = |s: &str| s.trim_matches(is_ws).parse::<u32>().ok();
    match v.split_once(',') {
        Some((a, b)) => Some((number(a)?, number(b)?)),
        None => number(v).map(|n| (n, n)),
    }
}

/// An integer with an optional `0x`, `0o` or `0b` prefix (Zig `parseInt` with base 0).
fn parse_u8_auto(s: &str) -> Option<u8> {
    let s = s.trim_matches(is_ws);
    let (digits, radix) = match s.get(..2) {
        Some("0x" | "0X") => (&s[2..], 16),
        Some("0o" | "0O") => (&s[2..], 8),
        Some("0b" | "0B") => (&s[2..], 2),
        _ => (s, 10),
    };
    if !digits.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    u8::from_str_radix(digits, radix).ok()
}

/// `N=COLOR` (color.zig:64): N is 0-255 in decimal, hex, octal or binary.
fn parse_palette_entry(v: &str) -> Option<(usize, Rgb)> {
    let (index, color) = v.split_once('=')?;
    Some((parse_u8_auto(index)? as usize, parse_color(color)?))
}

/// One colour channel written with 1 to 4 hex digits, scaled to 8 bits (color.zig:714).
fn channel(digits: &str) -> Option<u8> {
    if digits.is_empty() || digits.len() > 4 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let value = u32::from_str_radix(digits, 16).ok()?;
    let max = (1u32 << (4 * digits.len())) - 1;
    Some((value * 255 / max) as u8)
}

/// An `rgbi:` channel: a fraction in [0, 1] (color.zig:701).
fn intensity(s: &str) -> Option<u8> {
    let f = s.parse::<f64>().ok().filter(|f| (0.0..=1.0).contains(f))?;
    Some((f * 255.0) as u8)
}

/// Parse a colour the way Ghostty's `RGB.parse` does (color.zig:761): `#RGB`, `#RRGGBB`, `#RRRGGGBBB`,
/// `#RRRRGGGGBBBB`, an X11 name, bare `RGB` or `RRGGBB`, and `rgb:r/g/b` or `rgbi:r/g/b`.
pub fn parse_color(s: &str) -> Option<Rgb> {
    let input = s.trim_matches(is_ws);
    if input.is_empty() || !input.is_ascii() {
        return None;
    }
    if let Some(hex) = input.strip_prefix('#') {
        let width = match hex.len() {
            3 => 1,
            6 => 2,
            9 => 3,
            12 => 4,
            _ => return None,
        };
        return Some([channel(&hex[..width])?, channel(&hex[width..2 * width])?, channel(&hex[2 * width..])?]);
    }
    if let Some(rgb) = x11_color(input) {
        return Some(rgb);
    }
    match input.len() {
        3 => return Some([channel(&input[0..1])?, channel(&input[1..2])?, channel(&input[2..3])?]),
        6 => return Some([channel(&input[0..2])?, channel(&input[2..4])?, channel(&input[4..6])?]),
        _ => {}
    }
    let rest = input.strip_prefix("rgb")?;
    let (use_intensity, rest) = match rest.strip_prefix('i') {
        Some(rest) => (true, rest),
        None => (false, rest),
    };
    let rest = rest.strip_prefix(':')?;
    let mut parts = rest.splitn(3, '/');
    let (r, g, b) = (parts.next()?, parts.next()?, parts.next()?);
    let component = |s: &str| if use_intensity { intensity(s) } else { channel(s) };
    Some([component(r)?, component(g)?, component(b)?])
}

/// Look an X11 colour name up, ignoring case and spaces (`Dark Slate Gray` and `darkslategray` both match).
fn x11_color(name: &str) -> Option<Rgb> {
    let key: String = name.chars().filter(|c| *c != ' ').map(|c| c.to_ascii_lowercase()).collect();
    if key.is_empty() {
        return None;
    }
    X11_COLORS.lines().find_map(|line| {
        let (entry, hex) = line.split_once(' ')?;
        if entry != key {
            return None;
        }
        Some([channel(&hex[0..2])?, channel(&hex[2..4])?, channel(&hex[4..6])?])
    })
}

/// Ghostty's rgb.txt (src/terminal/res/rgb.txt) as lowercase `name RRGGBB` lines with spaces removed from names.
const X11_COLORS: &str = r"snow fffafa
ghostwhite f8f8ff
whitesmoke f5f5f5
gainsboro dcdcdc
floralwhite fffaf0
oldlace fdf5e6
linen faf0e6
antiquewhite faebd7
papayawhip ffefd5
blanchedalmond ffebcd
bisque ffe4c4
peachpuff ffdab9
navajowhite ffdead
moccasin ffe4b5
cornsilk fff8dc
ivory fffff0
lemonchiffon fffacd
seashell fff5ee
honeydew f0fff0
mintcream f5fffa
azure f0ffff
aliceblue f0f8ff
lavender e6e6fa
lavenderblush fff0f5
mistyrose ffe4e1
white ffffff
black 000000
darkslategray 2f4f4f
darkslategrey 2f4f4f
dimgray 696969
dimgrey 696969
slategray 708090
slategrey 708090
lightslategray 778899
lightslategrey 778899
gray bebebe
grey bebebe
x11gray bebebe
x11grey bebebe
webgray 808080
webgrey 808080
lightgrey d3d3d3
lightgray d3d3d3
midnightblue 191970
navy 000080
navyblue 000080
cornflowerblue 6495ed
darkslateblue 483d8b
slateblue 6a5acd
mediumslateblue 7b68ee
lightslateblue 8470ff
mediumblue 0000cd
royalblue 4169e1
blue 0000ff
dodgerblue 1e90ff
deepskyblue 00bfff
skyblue 87ceeb
lightskyblue 87cefa
steelblue 4682b4
lightsteelblue b0c4de
lightblue add8e6
powderblue b0e0e6
paleturquoise afeeee
darkturquoise 00ced1
mediumturquoise 48d1cc
turquoise 40e0d0
cyan 00ffff
aqua 00ffff
lightcyan e0ffff
cadetblue 5f9ea0
mediumaquamarine 66cdaa
aquamarine 7fffd4
darkgreen 006400
darkolivegreen 556b2f
darkseagreen 8fbc8f
seagreen 2e8b57
mediumseagreen 3cb371
lightseagreen 20b2aa
palegreen 98fb98
springgreen 00ff7f
lawngreen 7cfc00
green 00ff00
lime 00ff00
x11green 00ff00
webgreen 008000
chartreuse 7fff00
mediumspringgreen 00fa9a
greenyellow adff2f
limegreen 32cd32
yellowgreen 9acd32
forestgreen 228b22
olivedrab 6b8e23
darkkhaki bdb76b
khaki f0e68c
palegoldenrod eee8aa
lightgoldenrodyellow fafad2
lightyellow ffffe0
yellow ffff00
gold ffd700
lightgoldenrod eedd82
goldenrod daa520
darkgoldenrod b8860b
rosybrown bc8f8f
indianred cd5c5c
saddlebrown 8b4513
sienna a0522d
peru cd853f
burlywood deb887
beige f5f5dc
wheat f5deb3
sandybrown f4a460
tan d2b48c
chocolate d2691e
firebrick b22222
brown a52a2a
darksalmon e9967a
salmon fa8072
lightsalmon ffa07a
orange ffa500
darkorange ff8c00
coral ff7f50
lightcoral f08080
tomato ff6347
orangered ff4500
red ff0000
hotpink ff69b4
deeppink ff1493
pink ffc0cb
lightpink ffb6c1
palevioletred db7093
maroon b03060
x11maroon b03060
webmaroon 800000
mediumvioletred c71585
violetred d02090
magenta ff00ff
fuchsia ff00ff
violet ee82ee
plum dda0dd
orchid da70d6
mediumorchid ba55d3
darkorchid 9932cc
darkviolet 9400d3
blueviolet 8a2be2
purple a020f0
x11purple a020f0
webpurple 800080
mediumpurple 9370db
thistle d8bfd8
snow1 fffafa
snow2 eee9e9
snow3 cdc9c9
snow4 8b8989
seashell1 fff5ee
seashell2 eee5de
seashell3 cdc5bf
seashell4 8b8682
antiquewhite1 ffefdb
antiquewhite2 eedfcc
antiquewhite3 cdc0b0
antiquewhite4 8b8378
bisque1 ffe4c4
bisque2 eed5b7
bisque3 cdb79e
bisque4 8b7d6b
peachpuff1 ffdab9
peachpuff2 eecbad
peachpuff3 cdaf95
peachpuff4 8b7765
navajowhite1 ffdead
navajowhite2 eecfa1
navajowhite3 cdb38b
navajowhite4 8b795e
lemonchiffon1 fffacd
lemonchiffon2 eee9bf
lemonchiffon3 cdc9a5
lemonchiffon4 8b8970
cornsilk1 fff8dc
cornsilk2 eee8cd
cornsilk3 cdc8b1
cornsilk4 8b8878
ivory1 fffff0
ivory2 eeeee0
ivory3 cdcdc1
ivory4 8b8b83
honeydew1 f0fff0
honeydew2 e0eee0
honeydew3 c1cdc1
honeydew4 838b83
lavenderblush1 fff0f5
lavenderblush2 eee0e5
lavenderblush3 cdc1c5
lavenderblush4 8b8386
mistyrose1 ffe4e1
mistyrose2 eed5d2
mistyrose3 cdb7b5
mistyrose4 8b7d7b
azure1 f0ffff
azure2 e0eeee
azure3 c1cdcd
azure4 838b8b
slateblue1 836fff
slateblue2 7a67ee
slateblue3 6959cd
slateblue4 473c8b
royalblue1 4876ff
royalblue2 436eee
royalblue3 3a5fcd
royalblue4 27408b
blue1 0000ff
blue2 0000ee
blue3 0000cd
blue4 00008b
dodgerblue1 1e90ff
dodgerblue2 1c86ee
dodgerblue3 1874cd
dodgerblue4 104e8b
steelblue1 63b8ff
steelblue2 5cacee
steelblue3 4f94cd
steelblue4 36648b
deepskyblue1 00bfff
deepskyblue2 00b2ee
deepskyblue3 009acd
deepskyblue4 00688b
skyblue1 87ceff
skyblue2 7ec0ee
skyblue3 6ca6cd
skyblue4 4a708b
lightskyblue1 b0e2ff
lightskyblue2 a4d3ee
lightskyblue3 8db6cd
lightskyblue4 607b8b
slategray1 c6e2ff
slategray2 b9d3ee
slategray3 9fb6cd
slategray4 6c7b8b
lightsteelblue1 cae1ff
lightsteelblue2 bcd2ee
lightsteelblue3 a2b5cd
lightsteelblue4 6e7b8b
lightblue1 bfefff
lightblue2 b2dfee
lightblue3 9ac0cd
lightblue4 68838b
lightcyan1 e0ffff
lightcyan2 d1eeee
lightcyan3 b4cdcd
lightcyan4 7a8b8b
paleturquoise1 bbffff
paleturquoise2 aeeeee
paleturquoise3 96cdcd
paleturquoise4 668b8b
cadetblue1 98f5ff
cadetblue2 8ee5ee
cadetblue3 7ac5cd
cadetblue4 53868b
turquoise1 00f5ff
turquoise2 00e5ee
turquoise3 00c5cd
turquoise4 00868b
cyan1 00ffff
cyan2 00eeee
cyan3 00cdcd
cyan4 008b8b
darkslategray1 97ffff
darkslategray2 8deeee
darkslategray3 79cdcd
darkslategray4 528b8b
aquamarine1 7fffd4
aquamarine2 76eec6
aquamarine3 66cdaa
aquamarine4 458b74
darkseagreen1 c1ffc1
darkseagreen2 b4eeb4
darkseagreen3 9bcd9b
darkseagreen4 698b69
seagreen1 54ff9f
seagreen2 4eee94
seagreen3 43cd80
seagreen4 2e8b57
palegreen1 9aff9a
palegreen2 90ee90
palegreen3 7ccd7c
palegreen4 548b54
springgreen1 00ff7f
springgreen2 00ee76
springgreen3 00cd66
springgreen4 008b45
green1 00ff00
green2 00ee00
green3 00cd00
green4 008b00
chartreuse1 7fff00
chartreuse2 76ee00
chartreuse3 66cd00
chartreuse4 458b00
olivedrab1 c0ff3e
olivedrab2 b3ee3a
olivedrab3 9acd32
olivedrab4 698b22
darkolivegreen1 caff70
darkolivegreen2 bcee68
darkolivegreen3 a2cd5a
darkolivegreen4 6e8b3d
khaki1 fff68f
khaki2 eee685
khaki3 cdc673
khaki4 8b864e
lightgoldenrod1 ffec8b
lightgoldenrod2 eedc82
lightgoldenrod3 cdbe70
lightgoldenrod4 8b814c
lightyellow1 ffffe0
lightyellow2 eeeed1
lightyellow3 cdcdb4
lightyellow4 8b8b7a
yellow1 ffff00
yellow2 eeee00
yellow3 cdcd00
yellow4 8b8b00
gold1 ffd700
gold2 eec900
gold3 cdad00
gold4 8b7500
goldenrod1 ffc125
goldenrod2 eeb422
goldenrod3 cd9b1d
goldenrod4 8b6914
darkgoldenrod1 ffb90f
darkgoldenrod2 eead0e
darkgoldenrod3 cd950c
darkgoldenrod4 8b6508
rosybrown1 ffc1c1
rosybrown2 eeb4b4
rosybrown3 cd9b9b
rosybrown4 8b6969
indianred1 ff6a6a
indianred2 ee6363
indianred3 cd5555
indianred4 8b3a3a
sienna1 ff8247
sienna2 ee7942
sienna3 cd6839
sienna4 8b4726
burlywood1 ffd39b
burlywood2 eec591
burlywood3 cdaa7d
burlywood4 8b7355
wheat1 ffe7ba
wheat2 eed8ae
wheat3 cdba96
wheat4 8b7e66
tan1 ffa54f
tan2 ee9a49
tan3 cd853f
tan4 8b5a2b
chocolate1 ff7f24
chocolate2 ee7621
chocolate3 cd661d
chocolate4 8b4513
firebrick1 ff3030
firebrick2 ee2c2c
firebrick3 cd2626
firebrick4 8b1a1a
brown1 ff4040
brown2 ee3b3b
brown3 cd3333
brown4 8b2323
salmon1 ff8c69
salmon2 ee8262
salmon3 cd7054
salmon4 8b4c39
lightsalmon1 ffa07a
lightsalmon2 ee9572
lightsalmon3 cd8162
lightsalmon4 8b5742
orange1 ffa500
orange2 ee9a00
orange3 cd8500
orange4 8b5a00
darkorange1 ff7f00
darkorange2 ee7600
darkorange3 cd6600
darkorange4 8b4500
coral1 ff7256
coral2 ee6a50
coral3 cd5b45
coral4 8b3e2f
tomato1 ff6347
tomato2 ee5c42
tomato3 cd4f39
tomato4 8b3626
orangered1 ff4500
orangered2 ee4000
orangered3 cd3700
orangered4 8b2500
red1 ff0000
red2 ee0000
red3 cd0000
red4 8b0000
deeppink1 ff1493
deeppink2 ee1289
deeppink3 cd1076
deeppink4 8b0a50
hotpink1 ff6eb4
hotpink2 ee6aa7
hotpink3 cd6090
hotpink4 8b3a62
pink1 ffb5c5
pink2 eea9b8
pink3 cd919e
pink4 8b636c
lightpink1 ffaeb9
lightpink2 eea2ad
lightpink3 cd8c95
lightpink4 8b5f65
palevioletred1 ff82ab
palevioletred2 ee799f
palevioletred3 cd6889
palevioletred4 8b475d
maroon1 ff34b3
maroon2 ee30a7
maroon3 cd2990
maroon4 8b1c62
violetred1 ff3e96
violetred2 ee3a8c
violetred3 cd3278
violetred4 8b2252
magenta1 ff00ff
magenta2 ee00ee
magenta3 cd00cd
magenta4 8b008b
orchid1 ff83fa
orchid2 ee7ae9
orchid3 cd69c9
orchid4 8b4789
plum1 ffbbff
plum2 eeaeee
plum3 cd96cd
plum4 8b668b
mediumorchid1 e066ff
mediumorchid2 d15fee
mediumorchid3 b452cd
mediumorchid4 7a378b
darkorchid1 bf3eff
darkorchid2 b23aee
darkorchid3 9a32cd
darkorchid4 68228b
purple1 9b30ff
purple2 912cee
purple3 7d26cd
purple4 551a8b
mediumpurple1 ab82ff
mediumpurple2 9f79ee
mediumpurple3 8968cd
mediumpurple4 5d478b
thistle1 ffe1ff
thistle2 eed2ee
thistle3 cdb5cd
thistle4 8b7b8b
gray0 000000
grey0 000000
gray1 030303
grey1 030303
gray2 050505
grey2 050505
gray3 080808
grey3 080808
gray4 0a0a0a
grey4 0a0a0a
gray5 0d0d0d
grey5 0d0d0d
gray6 0f0f0f
grey6 0f0f0f
gray7 121212
grey7 121212
gray8 141414
grey8 141414
gray9 171717
grey9 171717
gray10 1a1a1a
grey10 1a1a1a
gray11 1c1c1c
grey11 1c1c1c
gray12 1f1f1f
grey12 1f1f1f
gray13 212121
grey13 212121
gray14 242424
grey14 242424
gray15 262626
grey15 262626
gray16 292929
grey16 292929
gray17 2b2b2b
grey17 2b2b2b
gray18 2e2e2e
grey18 2e2e2e
gray19 303030
grey19 303030
gray20 333333
grey20 333333
gray21 363636
grey21 363636
gray22 383838
grey22 383838
gray23 3b3b3b
grey23 3b3b3b
gray24 3d3d3d
grey24 3d3d3d
gray25 404040
grey25 404040
gray26 424242
grey26 424242
gray27 454545
grey27 454545
gray28 474747
grey28 474747
gray29 4a4a4a
grey29 4a4a4a
gray30 4d4d4d
grey30 4d4d4d
gray31 4f4f4f
grey31 4f4f4f
gray32 525252
grey32 525252
gray33 545454
grey33 545454
gray34 575757
grey34 575757
gray35 595959
grey35 595959
gray36 5c5c5c
grey36 5c5c5c
gray37 5e5e5e
grey37 5e5e5e
gray38 616161
grey38 616161
gray39 636363
grey39 636363
gray40 666666
grey40 666666
gray41 696969
grey41 696969
gray42 6b6b6b
grey42 6b6b6b
gray43 6e6e6e
grey43 6e6e6e
gray44 707070
grey44 707070
gray45 737373
grey45 737373
gray46 757575
grey46 757575
gray47 787878
grey47 787878
gray48 7a7a7a
grey48 7a7a7a
gray49 7d7d7d
grey49 7d7d7d
gray50 7f7f7f
grey50 7f7f7f
gray51 828282
grey51 828282
gray52 858585
grey52 858585
gray53 878787
grey53 878787
gray54 8a8a8a
grey54 8a8a8a
gray55 8c8c8c
grey55 8c8c8c
gray56 8f8f8f
grey56 8f8f8f
gray57 919191
grey57 919191
gray58 949494
grey58 949494
gray59 969696
grey59 969696
gray60 999999
grey60 999999
gray61 9c9c9c
grey61 9c9c9c
gray62 9e9e9e
grey62 9e9e9e
gray63 a1a1a1
grey63 a1a1a1
gray64 a3a3a3
grey64 a3a3a3
gray65 a6a6a6
grey65 a6a6a6
gray66 a8a8a8
grey66 a8a8a8
gray67 ababab
grey67 ababab
gray68 adadad
grey68 adadad
gray69 b0b0b0
grey69 b0b0b0
gray70 b3b3b3
grey70 b3b3b3
gray71 b5b5b5
grey71 b5b5b5
gray72 b8b8b8
grey72 b8b8b8
gray73 bababa
grey73 bababa
gray74 bdbdbd
grey74 bdbdbd
gray75 bfbfbf
grey75 bfbfbf
gray76 c2c2c2
grey76 c2c2c2
gray77 c4c4c4
grey77 c4c4c4
gray78 c7c7c7
grey78 c7c7c7
gray79 c9c9c9
grey79 c9c9c9
gray80 cccccc
grey80 cccccc
gray81 cfcfcf
grey81 cfcfcf
gray82 d1d1d1
grey82 d1d1d1
gray83 d4d4d4
grey83 d4d4d4
gray84 d6d6d6
grey84 d6d6d6
gray85 d9d9d9
grey85 d9d9d9
gray86 dbdbdb
grey86 dbdbdb
gray87 dedede
grey87 dedede
gray88 e0e0e0
grey88 e0e0e0
gray89 e3e3e3
grey89 e3e3e3
gray90 e5e5e5
grey90 e5e5e5
gray91 e8e8e8
grey91 e8e8e8
gray92 ebebeb
grey92 ebebeb
gray93 ededed
grey93 ededed
gray94 f0f0f0
grey94 f0f0f0
gray95 f2f2f2
grey95 f2f2f2
gray96 f5f5f5
grey96 f5f5f5
gray97 f7f7f7
grey97 f7f7f7
gray98 fafafa
grey98 fafafa
gray99 fcfcfc
grey99 fcfcfc
gray100 ffffff
grey100 ffffff
darkgrey a9a9a9
darkgray a9a9a9
darkblue 00008b
darkcyan 008b8b
darkmagenta 8b008b
darkred 8b0000
lightgreen 90ee90
crimson dc143c
indigo 4b0082
olive 808000
rebeccapurple 663399
silver c0c0c0
teal 008080";

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A fresh scratch directory under the system temp dir.
    fn tmpdir(tag: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("lmux-cfg-test-{}-{tag}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    fn parsed(text: &str) -> TermConfig {
        let mut cfg = TermConfig::default();
        cfg.apply_text(text, Path::new("/"), 0);
        cfg
    }

    #[test]
    fn defaults_are_ghostty_with_lmux_fire_colours() {
        let c = TermConfig::default();
        assert_eq!(c.background, [0x12, 0x11, 0x0e]);
        assert_eq!(c.foreground, [0xea, 0xe6, 0xdc]);
        assert_eq!(c.font_size, None);
        assert!(!c.font_thicken);
        assert_eq!(c.font_thicken_strength, 255);
        assert_eq!(c.cursor_color, Some(CellColor::Color([0xf0, 0x7a, 0x3c])));
        assert_eq!(c.cursor_text, Some(CellColor::Color([0x12, 0x11, 0x0e])));
        assert_eq!(c.cursor_opacity, 1.0);
        assert_eq!(c.cursor_style, CursorStyle::Block);
        assert_eq!(c.cursor_style_blink, None);
        assert_eq!(c.selection_background, Some(CellColor::Color([0x6b, 0x2c, 0x18])));
        assert_eq!(c.selection_foreground, Some(CellColor::Color([0xff, 0xf4, 0xe4])));
        assert_eq!(c.minimum_contrast, 1.0);
        assert_eq!(c.faint_opacity, 0.5);
        assert_eq!(c.bold_color, None);
        assert_eq!(c.window_padding_x, (2, 2));
        assert_eq!(c.window_padding_y, (2, 2));
        assert!(!c.window_padding_balance);
        assert!(c.font_family.is_empty());
    }

    #[test]
    fn default_palette_is_fire_base_plus_xterm_cube_and_grays() {
        let p = TermConfig::default().palette;
        assert_eq!(p[0], [0x24, 0x22, 0x1d]);
        assert_eq!(p[1], [0xe0, 0x50, 0x2c]);
        assert_eq!(p[7], [0xd8, 0xd4, 0xc9]);
        assert_eq!(p[8], [0x74, 0x72, 0x68]);
        assert_eq!(p[15], [0xff, 0xf4, 0xe4]);
        assert_eq!(p[16], [0, 0, 0]);
        assert_eq!(p[17], [0, 0, 95]);
        assert_eq!(p[21], [0, 0, 255]);
        assert_eq!(p[22], [0, 95, 0]);
        assert_eq!(p[196], [255, 0, 0]);
        assert_eq!(p[231], [255, 255, 255]);
        assert_eq!(p[16 + 36 * 2 + 6 * 3 + 4], [135, 175, 215]);
        assert_eq!(p[232], [8, 8, 8]);
        assert_eq!(p[233], [18, 18, 18]);
        assert_eq!(p[255], [238, 238, 238]);
    }

    #[test]
    fn palette_entries_take_decimal_hex_octal_and_binary_indices() {
        let c = parsed(
            "palette = 1=#ff0000\n\
             palette = 0x0a=#00ff00\n\
             palette = 0o17=#0000ff\n\
             palette = 0b101=#123456\n\
             palette =  200 = #abc \n\
             palette = 255=rgb:ff/00/ff\n",
        );
        assert_eq!(c.palette[1], [255, 0, 0]);
        assert_eq!(c.palette[10], [0, 255, 0]);
        assert_eq!(c.palette[15], [0, 0, 255]);
        assert_eq!(c.palette[5], [0x12, 0x34, 0x56]);
        assert_eq!(c.palette[200], [0xaa, 0xbb, 0xcc]);
        assert_eq!(c.palette[255], [255, 0, 255]);
        // Untouched entries keep their defaults.
        assert_eq!(c.palette[2], TermConfig::default().palette[2]);
    }

    #[test]
    fn invalid_palette_entries_are_ignored() {
        let c = parsed(
            "palette = 256=#ffffff\npalette = x=#ffffff\npalette = 3\npalette = 3=nothing\npalette = -1=#fff\n",
        );
        assert_eq!(c.palette, TermConfig::default().palette);
    }

    #[test]
    fn an_empty_palette_value_resets_the_palette() {
        let c = parsed("palette = 1=#ff0000\npalette =\n");
        assert_eq!(c.palette, TermConfig::default().palette);
        let c = parsed("palette = 1=#ff0000\npalette = \"\"\npalette = 2=#00ff00\n");
        assert_eq!(c.palette[1], TermConfig::default().palette[1]);
        assert_eq!(c.palette[2], [0, 255, 0]);
    }

    #[test]
    fn line_format_handles_comments_cr_quotes_and_whitespace() {
        let c = parsed(
            "# a comment\r\n\
             \r\n\
             \t  background   =   #112233  \r\n\
             # foreground = #ffffff\n\
             foreground=\"#445566\"\n\
             font-family = \"JetBrains Mono\"\n\
             unknown-key = whatever\n\
             this line has no equals sign\n\
             cursor-style = bar # not a comment, so the value is invalid\n",
        );
        assert_eq!(c.background, [0x11, 0x22, 0x33]);
        assert_eq!(c.foreground, [0x44, 0x55, 0x66]);
        assert_eq!(c.font_family, vec!["JetBrains Mono".to_string()]);
        assert_eq!(c.cursor_style, CursorStyle::Block);
    }

    #[test]
    fn later_lines_win_and_empty_values_reset() {
        let c = parsed("background = #111111\nbackground = #222222\nfont-size = 15\n");
        assert_eq!(c.background, [0x22, 0x22, 0x22]);
        assert_eq!(c.font_size, Some(15.0));
        let c = parsed("background = #111111\nbackground =\nfont-size = 15\nfont-size = \"\"\n");
        assert_eq!(c.background, TermConfig::default().background);
        assert_eq!(c.font_size, None);
    }

    #[test]
    fn font_families_repeat_and_reset() {
        let c = parsed(
            "font-family = A\nfont-family = B\nfont-family-bold = C\n\
             font-family-italic = D\nfont-family-bold-italic = E\n",
        );
        assert_eq!(c.font_family, vec!["A", "B"]);
        assert_eq!(c.font_family_bold, vec!["C"]);
        assert_eq!(c.font_family_italic, vec!["D"]);
        assert_eq!(c.font_family_bold_italic, vec!["E"]);
        let c = parsed("font-family = A\nfont-family = \nfont-family = B\n");
        assert_eq!(c.font_family, vec!["B"]);
    }

    #[test]
    fn booleans_parse_like_ghostty() {
        assert!(parsed("font-thicken = true\n").font_thicken);
        assert!(parsed("font-thicken = t\n").font_thicken);
        assert!(parsed("font-thicken = 1\n").font_thicken);
        assert!(parsed("font-thicken\n").font_thicken);
        assert!(!parsed("font-thicken = true\nfont-thicken = false\n").font_thicken);
        assert!(!parsed("font-thicken = true\nfont-thicken = 0\n").font_thicken);
        // Invalid spellings leave the value alone.
        assert!(parsed("font-thicken = true\nfont-thicken = yes\n").font_thicken);
        assert!(!parsed("font-thicken = TRUE\n").font_thicken);
        assert_eq!(parsed("font-thicken-strength = 128\n").font_thicken_strength, 128);
        assert_eq!(parsed("font-thicken-strength = 300\n").font_thicken_strength, 255);
        assert_eq!(parsed("cursor-style-blink = false\n").cursor_style_blink, Some(false));
        assert_eq!(parsed("cursor-style-blink\n").cursor_style_blink, Some(true));
        assert_eq!(parsed("cursor-style-blink = false\ncursor-style-blink =\n").cursor_style_blink, None);
    }

    #[test]
    fn colour_syntaxes() {
        assert_eq!(parse_color("#f0a"), Some([0xff, 0x00, 0xaa]));
        assert_eq!(parse_color("#ff00aa"), Some([0xff, 0x00, 0xaa]));
        assert_eq!(parse_color("  #FF00AA\t"), Some([0xff, 0x00, 0xaa]));
        assert_eq!(parse_color("#fff000aaa"), Some([0xff, 0x00, 0xaa]));
        assert_eq!(parse_color("#ffff0000aaaa"), Some([0xff, 0x00, 0xaa]));
        assert_eq!(parse_color("#808080"), Some([0x80, 0x80, 0x80]));
        assert_eq!(parse_color("ff00aa"), Some([0xff, 0x00, 0xaa]));
        assert_eq!(parse_color("f0a"), Some([0xff, 0x00, 0xaa]));
        assert_eq!(parse_color("rgb:ff/00/aa"), Some([0xff, 0x00, 0xaa]));
        assert_eq!(parse_color("rgb:f/0/a"), Some([0xff, 0x00, 0xaa]));
        assert_eq!(parse_color("rgb:ffff/0000/aaaa"), Some([0xff, 0x00, 0xaa]));
        assert_eq!(parse_color("rgbi:1.0/0.0/0.5"), Some([255, 0, 127]));
        assert_eq!(parse_color("red"), Some([255, 0, 0]));
        assert_eq!(parse_color("Red"), Some([255, 0, 0]));
        assert_eq!(parse_color("DarkSlateGray"), Some([47, 79, 79]));
        assert_eq!(parse_color("dark slate gray"), Some([47, 79, 79]));
        assert_eq!(parse_color("white"), Some([255, 255, 255]));
        assert_eq!(parse_color("black"), Some([0, 0, 0]));
        assert_eq!(parse_color("cornflowerblue"), Some([100, 149, 237]));
        assert_eq!(parse_color("gray50"), Some([127, 127, 127]));
        assert_eq!(parse_color(""), None);
        assert_eq!(parse_color("#"), None);
        assert_eq!(parse_color("#ff"), None);
        assert_eq!(parse_color("#gggggg"), None);
        assert_eq!(parse_color("#+f0a"), None);
        assert_eq!(parse_color("nosuchcolour"), None);
        assert_eq!(parse_color("rgb:ff/00"), None);
        assert_eq!(parse_color("rgb:ff/00/aa/11"), None);
        assert_eq!(parse_color("rgbi:2.0/0/0"), None);
        assert_eq!(parse_color("#ééé"), None);
    }

    #[test]
    fn every_x11_name_parses() {
        // Every table entry must parse back to its own hex value.
        for line in X11_COLORS.lines() {
            let (name, hex) = line.split_once(' ').unwrap();
            assert_eq!(hex.len(), 6, "{name}");
            let rgb = parse_color(name).unwrap_or_else(|| panic!("{name} does not parse"));
            assert_eq!(rgb, parse_color(&format!("#{hex}")).unwrap(), "{name}");
        }
        assert!(X11_COLORS.lines().count() > 600);
    }

    #[test]
    fn colour_keys_and_cell_colours() {
        let c = parsed(
            "cursor-color = cell-foreground\n\
             cursor-text = cell-background\n\
             selection-background = #102030\n\
             selection-foreground = cell-foreground\n",
        );
        assert_eq!(c.cursor_color, Some(CellColor::CellForeground));
        assert_eq!(c.cursor_text, Some(CellColor::CellBackground));
        assert_eq!(c.selection_background, Some(CellColor::Color([0x10, 0x20, 0x30])));
        assert_eq!(c.selection_foreground, Some(CellColor::CellForeground));
        let c = parsed("cursor-color = #ffffff\ncursor-color =\n");
        assert_eq!(c.cursor_color, TermConfig::default().cursor_color, "an empty value resets to the default");
        let c = parsed("cursor-color = #ffffff\ncursor-color = nonsense\n");
        assert_eq!(c.cursor_color, Some(CellColor::Color([255, 255, 255])));
    }

    #[test]
    fn cursor_style_and_opacity() {
        assert_eq!(parsed("cursor-style = bar\n").cursor_style, CursorStyle::Bar);
        assert_eq!(parsed("cursor-style = underline\n").cursor_style, CursorStyle::Underline);
        assert_eq!(parsed("cursor-style = block_hollow\n").cursor_style, CursorStyle::BlockHollow);
        assert_eq!(parsed("cursor-style = bar\ncursor-style = pipe\n").cursor_style, CursorStyle::Bar);
        assert_eq!(parsed("cursor-opacity = 0.25\n").cursor_opacity, 0.25);
        assert_eq!(parsed("cursor-opacity = 4\n").cursor_opacity, 1.0);
        assert_eq!(parsed("cursor-opacity = -1\n").cursor_opacity, 0.0);
    }

    #[test]
    fn contrast_and_faint_are_clamped() {
        assert_eq!(parsed("minimum-contrast = 4.5\n").minimum_contrast, 4.5);
        assert_eq!(parsed("minimum-contrast = 100\n").minimum_contrast, 21.0);
        assert_eq!(parsed("minimum-contrast = 0.2\n").minimum_contrast, 1.0);
        assert_eq!(parsed("minimum-contrast = abc\n").minimum_contrast, 1.0);
        assert_eq!(parsed("faint-opacity = 0.3\n").faint_opacity, 0.3);
        assert_eq!(parsed("faint-opacity = 3\n").faint_opacity, 1.0);
    }

    #[test]
    fn bold_color_and_the_deprecated_bold_is_bright() {
        assert_eq!(parsed("bold-color = bright\n").bold_color, Some(BoldColor::Bright));
        assert_eq!(parsed("bold-color = #ff8000\n").bold_color, Some(BoldColor::Color([255, 128, 0])));
        assert_eq!(parsed("bold-color = orange\n").bold_color, Some(BoldColor::Color([255, 165, 0])));
        assert_eq!(parsed("bold-is-bright = true\n").bold_color, Some(BoldColor::Bright));
        assert_eq!(parsed("bold-is-bright\n").bold_color, Some(BoldColor::Bright));
        assert_eq!(parsed("bold-is-bright = false\n").bold_color, None);
        // `false` does not undo an earlier bold-color.
        let c = parsed("bold-color = #ff8000\nbold-is-bright = false\n");
        assert_eq!(c.bold_color, Some(BoldColor::Color([255, 128, 0])));
        assert_eq!(parsed("bold-color = bright\nbold-color =\n").bold_color, None);
    }

    #[test]
    fn window_padding_takes_one_or_two_values() {
        let c = parsed("window-padding-x = 6\nwindow-padding-y = 3, 9\n");
        assert_eq!(c.window_padding_x, (6, 6));
        assert_eq!(c.window_padding_y, (3, 9));
        let c = parsed("window-padding-x = 6,10\nwindow-padding-y = x\n");
        assert_eq!(c.window_padding_x, (6, 10));
        assert_eq!(c.window_padding_y, (2, 2));
        let c = parsed("window-padding-x = 6\nwindow-padding-x =\n");
        assert_eq!(c.window_padding_x, (2, 2));
        assert_eq!(parsed("window-padding-x = -1\n").window_padding_x, (2, 2));
    }

    #[test]
    fn window_padding_balance_accepts_booleans_and_equal() {
        assert!(parsed("window-padding-balance = true\n").window_padding_balance);
        assert!(parsed("window-padding-balance\n").window_padding_balance);
        assert!(parsed("window-padding-balance = equal\n").window_padding_balance);
        assert!(!parsed("window-padding-balance = true\nwindow-padding-balance = false\n").window_padding_balance);
    }

    #[test]
    fn cell_adjustments_parse_percent_and_absolute() {
        let c = parsed("adjust-cell-width = 20%\nadjust-cell-height = -2\n");
        assert_eq!(c.adjust_cell_width, Some(Modifier::Percent(1.2)));
        assert_eq!(c.adjust_cell_height, Some(Modifier::Absolute(-2)));
        assert_eq!(parsed("adjust-cell-width = -100%\n").adjust_cell_width, Some(Modifier::Percent(0.0)));
        assert_eq!(parsed("adjust-cell-width = -50%\n").adjust_cell_width, Some(Modifier::Percent(0.5)));
        assert_eq!(parsed("adjust-cell-width = 3.5\n").adjust_cell_width, None);
        assert_eq!(parsed("adjust-cell-width = 20%\nadjust-cell-width =\n").adjust_cell_width, None);
    }

    #[test]
    fn font_size_must_be_positive() {
        assert_eq!(parsed("font-size = 14.5\n").font_size, Some(14.5));
        assert_eq!(parsed("font-size = 0\n").font_size, None);
        assert_eq!(parsed("font-size = big\n").font_size, None);
    }

    #[test]
    fn config_file_includes_apply_after_the_including_file() {
        let dir = tmpdir("include");
        write(&dir, "extra.conf", "foreground = #0a0b0c\nbackground = #010203\n");
        write(&dir, "nested.conf", "config-file = deeper.conf\nfont-size = 11\n");
        write(&dir, "deeper.conf", "font-size = 22\ncursor-style = bar\n");
        let main = write(
            &dir,
            "config",
            "config-file = extra.conf\n\
             background = #ffffff\n\
             config-file = ?missing.conf\n\
             config-file = \"?nested.conf\"\n",
        );
        let text = std::fs::read_to_string(&main).unwrap();
        let mut c = TermConfig::default();
        c.apply_text(&text, &dir, 0);
        // The include is replayed after the including file's own lines.
        assert_eq!(c.background, [1, 2, 3]);
        assert_eq!(c.foreground, [0x0a, 0x0b, 0x0c]);
        assert_eq!(c.font_size, Some(22.0));
        assert_eq!(c.cursor_style, CursorStyle::Bar);
    }

    #[test]
    fn include_cycles_and_missing_required_files_terminate() {
        let dir = tmpdir("cycle");
        write(&dir, "a.conf", "config-file = b.conf\nfont-size = 10\n");
        write(&dir, "b.conf", "config-file = a.conf\nconfig-file = nope.conf\nforeground = #010101\n");
        let mut c = TermConfig::default();
        c.apply_text("config-file = a.conf\n", &dir, 0);
        assert_eq!(c.font_size, Some(10.0));
        assert_eq!(c.foreground, [1, 1, 1]);
        // A file that includes itself.
        write(&dir, "self.conf", "config-file = self.conf\nbackground = #020202\n");
        let mut c = TermConfig::default();
        c.apply_text("config-file = self.conf\n", &dir, 0);
        assert_eq!(c.background, [2, 2, 2]);
    }

    #[test]
    fn a_long_include_chain_stops_at_the_depth_limit() {
        let dir = tmpdir("chain");
        for i in 0..30 {
            write(&dir, &format!("{i}.conf"), &format!("config-file = {}.conf\nfont-size = {}\n", i + 1, i + 1));
        }
        let mut c = TermConfig::default();
        c.apply_text("config-file = 0.conf\n", &dir, 0);
        let size = c.font_size.unwrap();
        assert!((1.0..=f32::from(MAX_INCLUDE_DEPTH) + 1.0).contains(&size), "{size}");
    }

    #[test]
    fn theme_supplies_colours_and_user_lines_override_it() {
        let root = tmpdir("theme");
        let themes = root.join("themes");
        std::fs::create_dir_all(&themes).unwrap();
        write(
            &themes,
            "Mine",
            "palette = 0=#000001\n\
             palette = 1=#000002\n\
             background = #282c34\n\
             foreground = #ffffff\n\
             cursor-color = #abcdef\n\
             selection-background = #111111\n\
             theme = Other\n\
             config-file = ignored.conf\n",
        );
        write(&themes, "ignored.conf", "font-size = 99\n");
        let user = write(
            &root,
            "config",
            "theme = Mine\n\
             background = #000000\n\
             palette = 1=#ff0000\n\
             font-size = 14\n",
        );
        let c = TermConfig::load_from(&[user], &[themes], true);
        // Explicit user lines win even though the theme line comes first.
        assert_eq!(c.background, [0, 0, 0]);
        assert_eq!(c.palette[1], [255, 0, 0]);
        // Theme values the user did not set apply.
        assert_eq!(c.foreground, [255, 255, 255]);
        assert_eq!(c.palette[0], [0, 0, 1]);
        assert_eq!(c.cursor_color, Some(CellColor::Color([0xab, 0xcd, 0xef])));
        assert_eq!(c.selection_background, Some(CellColor::Color([0x11, 0x11, 0x11])));
        assert_eq!(c.font_size, Some(14.0));
    }

    #[test]
    fn a_theme_applies_below_user_lines_that_come_before_the_theme_key() {
        let root = tmpdir("order");
        let themes = root.join("themes");
        std::fs::create_dir_all(&themes).unwrap();
        write(&themes, "T", "background = #282c34\nforeground = #ffffff\n");
        let user = write(&root, "config", "background = #000000\ntheme = T\n");
        let c = TermConfig::load_from(&[user], &[themes], true);
        assert_eq!(c.background, [0, 0, 0]);
        assert_eq!(c.foreground, [255, 255, 255]);
    }

    #[test]
    fn light_dark_theme_pair_follows_the_appearance() {
        let root = tmpdir("pair");
        let themes = root.join("themes");
        std::fs::create_dir_all(&themes).unwrap();
        write(&themes, "Day", "background = #ffffff\nforeground = #000000\n");
        write(&themes, "Night", "background = #000000\nforeground = #ffffff\n");
        let user = write(&root, "config", "theme = light:Day,dark:Night\n");
        let dark = TermConfig::load_from(&[user.clone()], &[themes.clone()], true);
        assert_eq!(dark.background, [0, 0, 0]);
        let light = TermConfig::load_from(&[user], &[themes.clone()], false);
        assert_eq!(light.background, [255, 255, 255]);
        assert_eq!(light.foreground, [0, 0, 0]);
        // The halves may come in either order, with whitespace.
        let user = write(&root, "config2", "theme = \" dark : Night , light : Day \"\n");
        assert_eq!(TermConfig::load_from(&[user.clone()], &[themes.clone()], true).background, [0, 0, 0]);
        assert_eq!(TermConfig::load_from(&[user], &[themes], false).background, [255, 255, 255]);
    }

    #[test]
    fn theme_pair_parsing() {
        let pair = |a: &str, b: &str| Some((a.to_string(), b.to_string()));
        assert_eq!(parse_theme("foo"), pair("foo", "foo"));
        assert_eq!(parse_theme("  foo  "), pair("foo", "foo"));
        assert_eq!(parse_theme("Catppuccin Mocha"), pair("Catppuccin Mocha", "Catppuccin Mocha"));
        assert_eq!(parse_theme("light:a,dark:b"), pair("a", "b"));
        assert_eq!(parse_theme("dark:b,light:a"), pair("a", "b"));
        assert_eq!(parse_theme(" light:foo,  dark : bar  "), pair("foo", "bar"));
        assert_eq!(parse_theme("light:a"), None);
        assert_eq!(parse_theme("dark:b"), None);
        assert_eq!(parse_theme("light:a,dusk:b"), None);
        assert_eq!(parse_theme(""), None);
        assert_eq!(parse_theme("/abs/path/Theme"), pair("/abs/path/Theme", "/abs/path/Theme"));
    }

    #[test]
    fn theme_by_absolute_path_and_bad_themes() {
        let root = tmpdir("abs");
        let theme = write(&root, "mytheme", "background = #123456\n");
        let user = write(&root, "config", &format!("theme = {}\n", theme.display()));
        let c = TermConfig::load_from(&[user], &[], true);
        assert_eq!(c.background, [0x12, 0x34, 0x56]);

        // A missing theme, a directory, and a relative path with separators leave the defaults in place.
        std::fs::create_dir_all(root.join("themes/adir")).unwrap();
        for value in ["nonexistent", "adir", "sub/theme", "../mytheme"] {
            let user = write(&root, "config-bad", &format!("theme = {value}\nfont-size = 12\n"));
            let c = TermConfig::load_from(&[user], &[root.join("themes")], true);
            assert_eq!(c.background, TermConfig::default().background, "{value}");
            assert_eq!(c.font_size, Some(12.0), "{value}");
        }
    }

    #[test]
    fn theme_search_uses_the_first_directory_with_the_file() {
        let root = tmpdir("search");
        let (first, second) = (root.join("first"), root.join("second"));
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        write(&second, "T", "background = #020202\n");
        let user = write(&root, "config", "theme = T\n");
        let dirs = [first.clone(), second.clone()];
        assert_eq!(TermConfig::load_from(&[user.clone()], &dirs, true).background, [2, 2, 2]);
        write(&first, "T", "background = #010101\n");
        assert_eq!(TermConfig::load_from(&[user], &dirs, true).background, [1, 1, 1]);
    }

    #[test]
    fn theme_named_in_an_included_file_is_honoured() {
        let root = tmpdir("incl-theme");
        let themes = root.join("themes");
        std::fs::create_dir_all(&themes).unwrap();
        write(&themes, "T", "background = #0f0f0f\nforeground = #f0f0f0\n");
        write(&root, "theme.conf", "theme = T\n");
        let user = write(&root, "config", "config-file = theme.conf\nforeground = #aaaaaa\n");
        let c = TermConfig::load_from(&[user], &[themes], true);
        assert_eq!(c.background, [0x0f, 0x0f, 0x0f]);
        assert_eq!(c.foreground, [0xaa, 0xaa, 0xaa]);
    }

    #[test]
    fn later_config_files_override_earlier_ones() {
        let root = tmpdir("files");
        let a = write(&root, "a", "background = #010101\nforeground = #020202\n");
        let b = write(&root, "b", "background = #030303\n");
        let missing = root.join("missing");
        let c = TermConfig::load_from(&[a, missing, b], &[], true);
        assert_eq!(c.background, [3, 3, 3]);
        assert_eq!(c.foreground, [2, 2, 2]);
    }

    #[test]
    fn no_files_gives_the_defaults() {
        assert_eq!(TermConfig::load_from(&[], &[], true), TermConfig::default());
    }

    #[test]
    fn default_search_paths() {
        let dirs = theme_dirs();
        assert!(dirs.len() >= 2);
        assert!(dirs.contains(&PathBuf::from("/Applications/Ghostty.app/Contents/Resources/ghostty/themes")));
        assert_eq!(
            dirs.last().unwrap(),
            &PathBuf::from("/Applications/cmux.app/Contents/Resources/ghostty/themes")
        );
        let files = default_config_files();
        assert!(files.iter().any(|f| f.ends_with("com.mitchellh.ghostty/config.ghostty")) || home_dir().is_none());
    }

    #[test]
    fn real_cmux_theme_files_parse() {
        let dir = Path::new("/Applications/cmux.app/Contents/Resources/ghostty/themes");
        let Ok(read) = std::fs::read_dir(dir) else {
            eprintln!("skipping: {} is missing", dir.display());
            return;
        };
        let defaults = TermConfig::default();
        let mut count = 0;
        for entry in read.flatten() {
            let Some(text) = read_text(&entry.path()) else { continue };
            let mut cfg = TermConfig::default();
            cfg.apply_text(&text, dir, 0);
            count += 1;
            // Every `background = ...` / `foreground = ...` line in a shipped theme must have been understood.
            for (key, value) in entries(&text) {
                let value = value.unwrap_or("");
                match key {
                    "background" => assert_eq!(Some(cfg.background), parse_color(value), "{:?}", entry.path()),
                    "foreground" => assert_eq!(Some(cfg.foreground), parse_color(value), "{:?}", entry.path()),
                    "palette" => {
                        let (i, c) = parse_palette_entry(value)
                            .unwrap_or_else(|| panic!("{:?}: {value}", entry.path()));
                        assert_eq!(cfg.palette[i], c);
                    }
                    _ => {}
                }
            }
            // The default palette is cmux's "Apple System Colors"; every other theme changes it.
            if entry.file_name() != "Apple System Colors" {
                assert_ne!(cfg.palette[..16], defaults.palette[..16], "{:?}", entry.path());
            }
        }
        assert!(count > 100, "only {count} themes were read");

        // A well-known theme end to end.
        let mut cfg = TermConfig::default();
        cfg.apply_text(&std::fs::read_to_string(dir.join("Dracula+")).unwrap(), dir, 0);
        assert_eq!(cfg.background, [0x21, 0x21, 0x21]);
        assert_eq!(cfg.foreground, [0xf8, 0xf8, 0xf2]);
        assert_eq!(cfg.palette[1], [0xff, 0x55, 0x55]);
        assert_eq!(cfg.cursor_color, Some(CellColor::Color([0xec, 0xef, 0xf4])));
        assert_eq!(cfg.selection_foreground, Some(CellColor::Color([0x54, 0x54, 0x54])));
    }
}

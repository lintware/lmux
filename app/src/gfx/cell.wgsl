// Terminal cell shaders, ported from Ghostty's `src/renderer/shaders/shaders.metal`
// (MIT, Copyright (c) 2024 Mitchell Hashimoto, Ghostty contributors).
//
// Three steps share one pass inside egui's frame: the pane's default
// background, the cell backgrounds (a full-viewport triangle that looks each
// pixel's cell up in a storage buffer), then every glyph, decoration and
// cursor as an instanced quad. lmux draws into egui's non-sRGB target, which
// is Ghostty's macOS default `alpha-blending = native`: colours are blended
// gamma-encoded, as AppKit blends text.

struct Uniforms {
    // The pane's top-left in framebuffer pixels, and its size.
    origin: vec2f,
    screen: vec2f,
    // Cell size in pixels (whole numbers).
    cell: vec2f,
    // Where the grid starts inside the pane: left, top padding in pixels.
    pad: vec2f,
    // cols | rows << 16
    grid: u32,
    min_contrast: f32,
    // x | y << 16; 0xFFFF when no block cursor.
    cursor_pos: u32,
    // RGBA8 for text under a block cursor.
    cursor_color: u32,
    // RGBA8 default background.
    bg_color: u32,
    // 1 = cursor is wide, 2 = colours are already Display P3,
    // 4 = the layer is Display P3 (convert sRGB inputs).
    bools: u32,
    _p0: u32,
    _p1: u32,
}

const CURSOR_WIDE: u32 = 1u;
const USE_DISPLAY_P3: u32 = 2u;
const CONVERT_TO_P3: u32 = 4u;

const NO_MIN_CONTRAST: u32 = 1u;
const IS_CURSOR_GLYPH: u32 = 2u;

const ATLAS_GRAYSCALE: u32 = 0u;
const ATLAS_COLOR: u32 = 1u;

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var<storage, read> bg_cells: array<u32>;
@group(1) @binding(0) var atlas_gray: texture_2d<f32>;
@group(1) @binding(1) var atlas_color: texture_2d<f32>;

// --- colour -----------------------------------------------------------------

fn linearize(srgb: vec4f) -> vec4f {
    let cutoff = srgb.rgb <= vec3f(0.04045);
    let lower = srgb.rgb / 12.92;
    let higher = pow((srgb.rgb + 0.055) / 1.055, vec3f(2.4));
    return vec4f(select(higher, lower, cutoff), srgb.a);
}

fn linearize1(v: f32) -> f32 {
    return select(pow((v + 0.055) / 1.055, 2.4), v / 12.92, v <= 0.04045);
}

fn unlinearize(lin: vec4f) -> vec4f {
    let cutoff = lin.rgb <= vec3f(0.0031308);
    let lower = lin.rgb * 12.92;
    let higher = pow(max(lin.rgb, vec3f(0.0)), vec3f(1.0 / 2.4)) * 1.055 - 0.055;
    return vec4f(select(higher, lower, cutoff), lin.a);
}

fn unlinearize1(v: f32) -> f32 {
    return select(pow(max(v, 0.0), 1.0 / 2.4) * 1.055 - 0.055, v * 12.92, v <= 0.0031308);
}

// D50-adapted sRGB -> XYZ, then XYZ -> Display P3 (rows as written).
fn srgb_to_display_p3(c: vec3f) -> vec3f {
    let srgb_xyz = transpose(mat3x3f(
        0.4360747, 0.3850649, 0.1430804,
        0.2225045, 0.7168786, 0.0606169,
        0.0139322, 0.0971045, 0.7141733,
    ));
    let xyz_dp3 = transpose(mat3x3f(
        2.40414768, -0.99010704, -0.39759019,
        -0.84239098, 1.79905954, 0.01597023,
        0.04838763, -0.09752546, 1.27393636,
    ));
    return (xyz_dp3 * srgb_xyz) * c;
}

fn luminance(c: vec3f) -> f32 {
    return dot(c, vec3f(0.2126, 0.7152, 0.0722));
}

fn contrast_ratio(a: vec3f, b: vec3f) -> f32 {
    let l1 = luminance(a);
    let l2 = luminance(b);
    return (max(l1, l2) + 0.05) / (min(l1, l2) + 0.05);
}

fn contrasted_color(min_ratio: f32, fg: vec4f, bg: vec4f) -> vec4f {
    if contrast_ratio(fg.rgb, bg.rgb) < min_ratio {
        let white = contrast_ratio(vec3f(1.0), bg.rgb);
        let black = contrast_ratio(vec3f(0.0), bg.rgb);
        if white > black {
            return vec4f(1.0);
        }
        return vec4f(0.0, 0.0, 0.0, 1.0);
    }
    return fg;
}

// RGBA8 (straight alpha, sRGB) -> premultiplied, linear if asked. When the
// layer is Display P3, sRGB inputs are converted so they show as sRGB would.
fn load_color(packed: u32, linear: bool) -> vec4f {
    var color = unpack4x8unorm(packed);
    let p3_in = (u.bools & USE_DISPLAY_P3) != 0u;
    let to_p3 = (u.bools & CONVERT_TO_P3) != 0u;
    if (p3_in || !to_p3) && !linear {
        return vec4f(color.rgb * color.a, color.a);
    }
    color = linearize(color);
    if to_p3 && !p3_in {
        color = vec4f(srgb_to_display_p3(color.rgb), color.a);
    }
    if !linear {
        color = unlinearize(color);
    }
    return vec4f(color.rgb * color.a, color.a);
}

// --- full-viewport steps ----------------------------------------------------

struct FullOut {
    @builtin(position) position: vec4f,
}

@vertex
fn full_screen_vertex(@builtin(vertex_index) vid: u32) -> FullOut {
    var out: FullOut;
    let x = select(-1.0, 3.0, vid == 2u);
    let y = select(1.0, -3.0, vid == 0u);
    out.position = vec4f(x, y, 1.0, 1.0);
    return out;
}

@fragment
fn bg_color_fragment(in: FullOut) -> @location(0) vec4f {
    return load_color(u.bg_color, false);
}

@fragment
fn cell_bg_fragment(in: FullOut) -> @location(0) vec4f {
    let cols = i32(u.grid & 0xFFFFu);
    let rows = i32(u.grid >> 16u);
    let local = in.position.xy - u.origin - u.pad;
    let pos = vec2i(floor(local / u.cell));
    // Outside the grid the pane's own background shows (Ghostty's
    // `window-padding-color = background`).
    if pos.x < 0 || pos.y < 0 || pos.x > cols - 1 || pos.y > rows - 1 {
        return vec4f(0.0);
    }
    return load_color(bg_cells[pos.y * cols + pos.x], false);
}

// --- text -------------------------------------------------------------------

struct TextIn {
    @location(0) glyph_pos: vec2u,
    @location(1) glyph_size: vec2u,
    @location(2) bearings: vec2i,
    @location(3) grid_pos: vec2u,
    @location(4) color: u32,
    // atlas | bools << 8
    @location(5) info: u32,
}

struct TextOut {
    @builtin(position) position: vec4f,
    @location(0) @interpolate(flat) atlas: u32,
    @location(1) @interpolate(flat) color: vec4f,
    @location(2) @interpolate(flat) bg_color: vec4f,
    @location(3) tex_coord: vec2f,
}

@vertex
fn cell_text_vertex(@builtin(vertex_index) vid: u32, in: TextIn) -> TextOut {
    var out: TextOut;
    let cols = u.grid & 0xFFFFu;

    //   0 --> 1
    //   |   .'|
    //   |  /  |
    //   | L   |
    //   2 --> 3
    let corner = vec2f(f32(vid == 1u || vid == 3u), f32(vid == 2u || vid == 3u));
    let size = vec2f(in.glyph_size);
    let offset = vec2f(f32(in.bearings.x), u.cell.y - f32(in.bearings.y));
    let p = u.cell * vec2f(in.grid_pos) + size * corner + offset + u.pad;
    out.position = vec4f(2.0 * p.x / u.screen.x - 1.0, 1.0 - 2.0 * p.y / u.screen.y, 0.0, 1.0);
    out.tex_coord = vec2f(in.glyph_pos) + size * corner;
    out.atlas = in.info & 0xFFu;
    let bools = (in.info >> 8u) & 0xFFu;

    // Always linear here, to make the contrast maths right.
    out.color = load_color(in.color, true);
    var bg = load_color(bg_cells[in.grid_pos.y * cols + in.grid_pos.x], true);
    let global_bg = load_color(u.bg_color, true);
    bg = bg + global_bg * (1.0 - bg.a);
    out.bg_color = bg;

    if u.min_contrast > 1.0 && (bools & NO_MIN_CONTRAST) == 0u {
        out.color = contrasted_color(u.min_contrast, out.color, out.bg_color);
    }

    let cx = u.cursor_pos & 0xFFFFu;
    let cy = u.cursor_pos >> 16u;
    let wide = (u.bools & CURSOR_WIDE) != 0u;
    let at_cursor = (in.grid_pos.x == cx || (wide && in.grid_pos.x == cx + 1u)) && in.grid_pos.y == cy;
    if (bools & IS_CURSOR_GLYPH) == 0u && at_cursor {
        out.color = load_color(u.cursor_color, true);
    }
    return out;
}

@fragment
fn cell_text_fragment(in: TextOut) -> @location(0) vec4f {
    let texel = vec2i(floor(in.tex_coord));
    if in.atlas == ATLAS_COLOR {
        // Emoji are stored premultiplied and gamma-encoded, ready to blend.
        return textureLoad(atlas_color, texel, 0);
    }
    // Native blending: re-apply the gamma encoding to the linear colour.
    var color = in.color;
    if color.a <= 0.0 {
        return vec4f(0.0);
    }
    color = vec4f(color.rgb / color.a, color.a);
    color = unlinearize(color);
    color = vec4f(color.rgb * color.a, color.a);
    let a = textureLoad(atlas_gray, texel, 0).r;
    return color * a;
}

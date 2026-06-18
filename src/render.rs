use crate::{
    cli::ColorMode,
    frame::{Frame, RenderFrame},
};

pub fn build_lut(ramp: &str) -> Vec<char> {
    let chars: Vec<char> = ramp.chars().collect();
    let n = chars.len();
    (0..=255)
        .map(|i| chars[(i * (n - 1) / 255).min(n - 1)])
        .collect()
}

pub fn compute_render_size(
    term_cols: usize,
    term_rows: usize,
    cam_w: usize,
    cam_h: usize,
    char_aspect: f32,
) -> (usize, usize) {
    let cam_aspect = cam_w as f32 / cam_h as f32;
    let mut render_cols = term_cols.max(1);
    let mut render_rows = term_rows.max(1);
    let effective = render_cols as f32 * char_aspect / render_rows as f32;
    if effective > cam_aspect {
        render_cols = (render_rows as f32 * cam_aspect / char_aspect).max(1.0) as usize;
    } else {
        render_rows = (render_cols as f32 * char_aspect / cam_aspect).max(1.0) as usize;
    }
    (render_cols.max(1), render_rows.max(1))
}

pub fn rotate_frame(frame: &Frame, rotation: u8) -> Frame {
    if rotation == 0 {
        return frame.clone();
    }
    let (new_w, new_h) = if rotation == 2 {
        (frame.width, frame.height)
    } else {
        (frame.height, frame.width)
    };
    let mut out = vec![0u8; frame.data.len()];
    for y in 0..frame.height {
        for x in 0..frame.width {
            let (dx, dy) = match rotation {
                1 => (y, frame.width - 1 - x),
                2 => (frame.width - 1 - x, frame.height - 1 - y),
                _ => (frame.height - 1 - y, x),
            };
            let src = (y * frame.width + x) * 3;
            let dst = (dy * new_w + dx) * 3;
            out[dst..dst + 3].copy_from_slice(&frame.data[src..src + 3]);
        }
    }
    Frame {
        data: out,
        width: new_w,
        height: new_h,
    }
}

pub fn render_frame(
    frame: &Frame,
    render_cols: usize,
    render_rows: usize,
    lut: &[char],
    contrast: f32,
    brightness: i16,
    color_mode: ColorMode,
) -> (RenderFrame, String) {
    let mut chars = Vec::with_capacity(render_rows);
    let mut colors = if color_mode.is_color() {
        Some(Vec::with_capacity(render_rows))
    } else {
        None
    };
    let mut ascii = String::new();
    for y in 0..render_rows {
        let sy = y * (frame.height - 1) / render_rows.saturating_sub(1).max(1);
        let mut row = String::with_capacity(render_cols);
        let mut color_row = Vec::with_capacity(render_cols);
        for x in 0..render_cols {
            let sx = x * (frame.width - 1) / render_cols.saturating_sub(1).max(1);
            let off = (sy * frame.width + sx) * 3;
            let (mut r, mut g, mut b) = (frame.data[off], frame.data[off + 1], frame.data[off + 2]);
            r = adjust(r, contrast, brightness);
            g = adjust(g, contrast, brightness);
            b = adjust(b, contrast, brightness);
            let gray = (0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) as usize;
            let ch = lut[gray.min(255)];
            row.push(ch);
            if color_mode.is_color() {
                let (er, eg, eb) = effective_rgb(r, g, b, color_mode);
                ascii.push_str(&rgb_to_ansi(r, g, b, color_mode));
                ascii.push(ch);
                color_row.push((er, eg, eb));
            } else {
                ascii.push(ch);
            }
        }
        chars.push(row);
        if let Some(colors) = colors.as_mut() {
            colors.push(color_row);
            ascii.push_str("\x1b[0m");
        }
        if y + 1 < render_rows {
            ascii.push('\n');
        }
    }
    (RenderFrame { chars, colors }, ascii)
}

fn adjust(v: u8, contrast: f32, brightness: i16) -> u8 {
    (128.0 + contrast * (v as f32 - 128.0) + brightness as f32).clamp(0.0, 255.0) as u8
}

fn rgb_to_ansi(r: u8, g: u8, b: u8, mode: ColorMode) -> String {
    match mode {
        ColorMode::TrueColor => format!("\x1b[38;2;{r};{g};{b}m"),
        ColorMode::Ansi256 => format!("\x1b[38;5;{}m", quantize_color(r, g, b)),
        ColorMode::Ansi16 => {
            let rr = if r > 85 { 1 } else { 0 };
            let gg = if g > 85 { 1 } else { 0 };
            let bb = if b > 85 { 1 } else { 0 };
            let mut code = 30 + (bb << 2 | gg << 1 | rr);
            if (r as u16 + g as u16 + b as u16) / 3 > 127 && (rr | gg | bb) != 0 {
                code += 60;
            }
            format!("\x1b[{code}m")
        }
        ColorMode::Gray => {
            let gray = (0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) as u8;
            format!("\x1b[38;5;{}m", 232 + (gray as u16 * 23 / 255) as u8)
        }
        ColorMode::Green => "\x1b[38;2;0;255;0m".to_string(),
        ColorMode::GreenGray => {
            let gray = (0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) as u8;
            format!("\x1b[38;2;0;{};0m", gray.max(20))
        }
        ColorMode::Red => "\x1b[38;2;255;0;0m".to_string(),
        ColorMode::RedGray => {
            let gray = (0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) as u8;
            format!("\x1b[38;2;{};0;0m", gray.max(20))
        }
        ColorMode::Off => String::new(),
    }
}

fn effective_rgb(r: u8, g: u8, b: u8, mode: ColorMode) -> (u8, u8, u8) {
    match mode {
        ColorMode::TrueColor => (r, g, b),
        ColorMode::Ansi256 => dequantize_color(quantize_color(r, g, b)),
        ColorMode::Ansi16 => (
            if r > 85 { 255 } else { 0 },
            if g > 85 { 255 } else { 0 },
            if b > 85 { 255 } else { 0 },
        ),
        ColorMode::Gray => {
            let gray = (0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) as u8;
            let v = (gray as u16 * 23 / 255 * 255 / 23) as u8;
            (v, v, v)
        }
        ColorMode::Green => (0, 255, 0),
        ColorMode::GreenGray => {
            let gray = (0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) as u8;
            (0, gray.max(20), 0)
        }
        ColorMode::Red => (255, 0, 0),
        ColorMode::RedGray => {
            let gray = (0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) as u8;
            (gray.max(20), 0, 0)
        }
        ColorMode::Off => (r, g, b),
    }
}

pub fn quantize_color(r: u8, g: u8, b: u8) -> u8 {
    let ri = ((r as f32 * 5.0 / 255.0).round() as u8).min(5);
    let gi = ((g as f32 * 5.0 / 255.0).round() as u8).min(5);
    let bi = ((b as f32 * 5.0 / 255.0).round() as u8).min(5);
    16 + 36 * ri + 6 * gi + bi
}

pub fn dequantize_color(idx: u8) -> (u8, u8, u8) {
    let idx = idx.saturating_sub(16);
    let b = (idx % 6) * 51;
    let g = ((idx / 6) % 6) * 51;
    let r = (idx / 36) * 51;
    (r, g, b)
}

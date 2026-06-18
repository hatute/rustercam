use anyhow::Result;
use crossterm::{
    cursor, execute,
    terminal::{self, ClearType},
};
use std::io::{self, Write};
use terminal_size::{terminal_size, Height, Width};

use crate::{
    cli::{Platform, HUD_LINES},
    recording::RecordingConfig,
};

pub struct HudState<'a> {
    pub recording: bool,
    pub preset: &'a str,
    pub color_mode_label: &'a str,
    pub contrast: f32,
    pub brightness: i16,
    pub camera_status: &'a str,
    pub invert: bool,
    pub rotation_degrees: u8,
}

pub fn terminal_canvas_size() -> (u16, u16) {
    if let Some((Width(w), Height(h))) = terminal_size() {
        (w.max(1), h.saturating_sub(HUD_LINES).max(1))
    } else {
        (80, 21)
    }
}

pub struct TerminalGuard;

impl TerminalGuard {
    pub fn enter() -> Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(io::stdout(), cursor::Hide, terminal::Clear(ClearType::All))?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            cursor::Show,
            crossterm::style::ResetColor,
            terminal::Clear(ClearType::All),
            cursor::MoveTo(0, 0)
        );
        let _ = terminal::disable_raw_mode();
    }
}

pub fn draw_screen(
    stdout: &mut io::Stdout,
    ascii: &str,
    render_cols: u16,
    render_rows: u16,
    fps: f32,
    backend: &str,
    hud: &HudState,
) -> Result<()> {
    let (cols, rows) = terminal_canvas_size();
    let pad = " ".repeat(cols.saturating_sub(render_cols) as usize / 2);
    write!(stdout, "\x1b[H")?;
    let mut line_count = 0u16;
    for line in ascii.split('\n') {
        writeln!(stdout, "{pad}{line}\x1b[K")?;
        line_count += 1;
    }
    for _ in line_count..rows {
        writeln!(stdout, "{}\x1b[K", " ".repeat(cols as usize))?;
    }
    let rec = if hud.recording {
        "  \x1b[31mREC\x1b[0m"
    } else {
        ""
    };
    let preset = hud.preset;
    writeln!(
        stdout,
        "\x1b[7m RUSTERCAM | {} | {:5.1} fps | {}(W) x {}(H) | {} ({}){} \x1b[0m\x1b[K",
        backend, fps, render_cols, render_rows, hud.color_mode_label, preset, rec
    )?;
    writeln!(
        stdout,
        " ↑/↓ Contrast {} {:3.1}  │  ←/→ Bright   {} {:+4}  │  Cam {}\x1b[K",
        make_bar(hud.contrast, 0.1, 3.0, 12),
        hud.contrast,
        make_bar(hud.brightness as f32, -100.0, 100.0, 12),
        hud.brightness,
        hud.camera_status
    )?;
    write!(
        stdout,
        " 1 invert:{}  2 rot:{}  3 rec  4 capture  5 preset  c camera  s settings  h help  q quit\x1b[K",
        if hud.invert { "on" } else { "off" },
        hud.rotation_degrees
    )?;
    stdout.flush()?;
    Ok(())
}

fn make_bar(value: f32, min: f32, max: f32, width: usize) -> String {
    let ratio = ((value - min) / (max - min)).clamp(0.0, 1.0);
    let filled = (ratio * width as f32).floor() as usize;
    format!(
        "{}{}",
        "█".repeat(filled),
        "░".repeat(width.saturating_sub(filled))
    )
}

pub fn draw_overlay(stdout: &mut io::Stdout, lines: &[String]) -> Result<()> {
    let (cols, rows) = if let Some((Width(w), Height(h))) = terminal_size() {
        (w, h)
    } else {
        (80, 24)
    };
    write!(stdout, "\x1b[H")?;
    for y in 0..rows {
        let text = lines.get(y as usize).map(String::as_str).unwrap_or("");
        writeln!(
            stdout,
            "\x1b[7m{:width$}\x1b[0m\x1b[K",
            truncate(text, cols as usize),
            width = cols as usize
        )?;
    }
    stdout.flush()?;
    Ok(())
}

pub fn help_text(platform: Platform) -> Vec<String> {
    [
        "",
        "  RUSTERCAM - KEYBOARD CONTROLS",
        "  --------------------------------",
        "",
        "  1         Toggle invert",
        "  2         Cycle rotation",
        "  3         Start / stop recording",
        "  4         Capture screenshot",
        "  Shift-H   Capture HTML screenshot",
        "  5         Cycle preset",
        "  c         Cycle camera device",
        "  Up/Down   Contrast",
        "  Left/Right Brightness",
        "  s         Settings",
        "  h         Close help",
        "  q         Quit",
        "",
        &format!("  Platform: {platform:?}"),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

pub fn settings_text(cfg: &RecordingConfig) -> Vec<String> {
    vec![
        "".into(),
        "  RUSTERCAM - SETTINGS".into(),
        "  ---------------------".into(),
        format!("  Preset: {}", cfg.preset_name().unwrap_or("custom")),
        format!("  Color: {}", cfg.color_mode.label()),
        format!("  Skip identical: {}", cfg.skip_identical),
        format!("  Quantize colors: {}", cfg.quantize_colors),
        format!("  Derive chars: {}", cfg.derive_chars),
        format!("  Flat indices: {}", cfg.flat_indices),
        format!("  Delta timestamps: {}", cfg.delta_timestamps),
        format!("  Zlib max: {}", cfg.zlib_max),
        "".into(),
        "  7 cycles color mode; s closes".into(),
    ]
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

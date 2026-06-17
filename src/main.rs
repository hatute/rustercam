use anyhow::{anyhow, Context, Result};
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use clap::{Parser, ValueEnum};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode},
    execute,
    terminal::{self, ClearType},
};
use flate2::{read::ZlibDecoder, write::ZlibEncoder, Compression};
use rand::Rng;
use std::{
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdout, Command, Stdio},
    time::{Duration, Instant},
};
use terminal_size::{terminal_size, Height, Width};

const RAMP_LONG: &str = " .'`^\",:;Il!i><~+_-?][}{1)(|/tfjrxnuvczXYUJCLQ0OZmwqpdbkhao*#MW&8%B@$";
const RAMP_SHORT: &str = " .:-=+*#%@";
const CHAR_ASPECT_FALLBACK: f32 = 0.45;
const HUD_LINES: u16 = 3;
const TCAM_MAGIC: &[u8; 4] = b"TCAM";
const TCAM_VERSION_1: u8 = 1;
const TCAM_VERSION_2: u8 = 2;
const TCAM_KEYFRAME_INTERVAL: usize = 30;
const TCAM_KEYFRAME: u8 = 0;
const TCAM_DELTA: u8 = 1;
const TCAM_SKIP: u8 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum Resolution {
    Low,
    Medium,
    High,
}

impl Resolution {
    fn size(self) -> (usize, usize) {
        match self {
            Self::Low => (320, 240),
            Self::Medium => (640, 480),
            Self::High => (1280, 720),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum Ramp {
    Long,
    Short,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum Platform {
    Auto,
    Macos,
    Linux,
    Termux,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ColorMode {
    TrueColor,
    Ansi256,
    Ansi16,
    Gray,
    Green,
    GreenGray,
    Red,
    RedGray,
    Off,
}

impl ColorMode {
    fn label(self) -> &'static str {
        match self {
            Self::TrueColor => "24bit",
            Self::Ansi256 => "256c",
            Self::Ansi16 => "16c",
            Self::Gray => "Gray",
            Self::Green => "Green",
            Self::GreenGray => "GrnGr",
            Self::Red => "Red",
            Self::RedGray => "RedGr",
            Self::Off => "Off",
        }
    }

    fn is_color(self) -> bool {
        self != Self::Off
    }

    fn cycle(self) -> Self {
        use ColorMode::*;
        match self {
            TrueColor => Ansi256,
            Ansi256 => Ansi16,
            Ansi16 => Gray,
            Gray => Green,
            Green => GreenGray,
            GreenGray => Red,
            Red => RedGray,
            RedGray => Off,
            Off => TrueColor,
        }
    }
}

#[derive(Parser, Debug)]
#[command(name = "rustercam", about = "ASCII art camera livestream")]
struct Args {
    #[arg(long)]
    no_color: bool,
    #[arg(long, value_enum, default_value_t = Resolution::Medium)]
    resolution: Resolution,
    #[arg(long, default_value_t = 1.0)]
    contrast: f32,
    #[arg(long, default_value_t = 0)]
    brightness: i16,
    #[arg(long, value_enum, default_value_t = Ramp::Long)]
    ramp: Ramp,
    #[arg(long)]
    invert: bool,
    #[arg(long)]
    char_aspect: Option<f32>,
    #[arg(long)]
    camera: Option<u32>,
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=3))]
    rotate: Option<u8>,
    #[arg(long, value_enum, default_value_t = Platform::Auto)]
    platform: Platform,
    #[arg(long)]
    record: Option<PathBuf>,
    #[arg(long)]
    play: Option<PathBuf>,
}

#[derive(Clone)]
struct Frame {
    data: Vec<u8>,
    width: usize,
    height: usize,
}

#[derive(Clone, Default)]
struct RenderFrame {
    chars: Vec<String>,
    colors: Option<Vec<Vec<(u8, u8, u8)>>>,
}

#[derive(Clone)]
struct RecordingConfig {
    skip_identical: bool,
    quantize_colors: bool,
    derive_chars: bool,
    flat_indices: bool,
    delta_timestamps: bool,
    zlib_max: bool,
    color_mode: ColorMode,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            skip_identical: true,
            quantize_colors: true,
            derive_chars: true,
            flat_indices: true,
            delta_timestamps: true,
            zlib_max: true,
            color_mode: ColorMode::TrueColor,
        }
    }
}

impl RecordingConfig {
    fn flags(&self, color: bool) -> u16 {
        let mut flags = 0;
        if color {
            flags |= 0x01;
        }
        if self.skip_identical {
            flags |= 0x02;
        }
        if self.quantize_colors {
            flags |= 0x04;
        }
        if self.derive_chars {
            flags |= 0x08;
        }
        if self.flat_indices {
            flags |= 0x10;
        }
        if self.delta_timestamps {
            flags |= 0x20;
        }
        if self.zlib_max {
            flags |= 0x40;
        }
        flags
    }

    fn from_flags(flags: u16) -> Self {
        Self {
            skip_identical: flags & 0x02 != 0,
            quantize_colors: flags & 0x04 != 0,
            derive_chars: flags & 0x08 != 0,
            flat_indices: flags & 0x10 != 0,
            delta_timestamps: flags & 0x20 != 0,
            zlib_max: flags & 0x40 != 0,
            color_mode: ColorMode::TrueColor,
        }
    }

    fn v1_defaults() -> Self {
        Self {
            skip_identical: false,
            quantize_colors: false,
            derive_chars: false,
            flat_indices: false,
            delta_timestamps: false,
            zlib_max: false,
            color_mode: ColorMode::TrueColor,
        }
    }

    fn apply_preset(&mut self, name: &str) {
        *self = match name {
            "raw" => Self {
                color_mode: ColorMode::TrueColor,
                skip_identical: false,
                quantize_colors: false,
                derive_chars: false,
                flat_indices: false,
                delta_timestamps: false,
                zlib_max: false,
            },
            "max" => Self {
                color_mode: ColorMode::Ansi256,
                ..Self::default()
            },
            "greyscale" => Self {
                color_mode: ColorMode::Gray,
                quantize_colors: false,
                derive_chars: false,
                ..Self::default()
            },
            "ascii" => Self {
                color_mode: ColorMode::Off,
                quantize_colors: false,
                ..Self::default()
            },
            "green" => Self {
                color_mode: ColorMode::Green,
                quantize_colors: false,
                ..Self::default()
            },
            "green_gray" => Self {
                color_mode: ColorMode::GreenGray,
                quantize_colors: false,
                ..Self::default()
            },
            "red" => Self {
                color_mode: ColorMode::Red,
                quantize_colors: false,
                ..Self::default()
            },
            "red_gray" => Self {
                color_mode: ColorMode::RedGray,
                quantize_colors: false,
                ..Self::default()
            },
            _ => self.clone(),
        };
    }

    fn preset_name(&self) -> Option<&'static str> {
        let names = [
            "raw",
            "max",
            "greyscale",
            "ascii",
            "green",
            "green_gray",
            "red",
            "red_gray",
        ];
        for name in names {
            let mut cfg = self.clone();
            cfg.apply_preset(name);
            if same_config(self, &cfg) {
                return Some(name);
            }
        }
        None
    }
}

fn same_config(a: &RecordingConfig, b: &RecordingConfig) -> bool {
    a.skip_identical == b.skip_identical
        && a.quantize_colors == b.quantize_colors
        && a.derive_chars == b.derive_chars
        && a.flat_indices == b.flat_indices
        && a.delta_timestamps == b.delta_timestamps
        && a.zlib_max == b.zlib_max
        && a.color_mode == b.color_mode
}

struct TcamEncoder {
    file: File,
    width: usize,
    height: usize,
    color: bool,
    config: RecordingConfig,
    frame_count: usize,
    prev_chars: Option<Vec<String>>,
    prev_colors: Option<Vec<Vec<(u8, u8, u8)>>>,
    start: Instant,
    prev_timestamp_ms: u32,
}

impl TcamEncoder {
    fn create(
        path: impl Into<PathBuf>,
        width: usize,
        height: usize,
        fps: u8,
        ramp: &str,
        color: bool,
        config: RecordingConfig,
    ) -> Result<Self> {
        let path = path.into();
        let mut file = File::create(&path).with_context(|| format!("create {}", path.display()))?;
        file.write_all(TCAM_MAGIC)?;
        file.write_u8(TCAM_VERSION_2)?;
        file.write_u16::<LittleEndian>(width as u16)?;
        file.write_u16::<LittleEndian>(height as u16)?;
        file.write_u8(fps)?;
        file.write_u16::<LittleEndian>(config.flags(color))?;
        file.write_u8(ramp.len() as u8)?;
        file.write_all(ramp.as_bytes())?;
        Ok(Self {
            file,
            width,
            height,
            color,
            config,
            frame_count: 0,
            prev_chars: None,
            prev_colors: None,
            start: Instant::now(),
            prev_timestamp_ms: 0,
        })
    }

    fn write_frame(&mut self, frame: &RenderFrame) -> Result<()> {
        let timestamp_ms = self.start.elapsed().as_millis().min(u32::MAX as u128) as u32;
        let keyframe = self.frame_count % TCAM_KEYFRAME_INTERVAL == 0 || self.prev_chars.is_none();
        let (frame_type, data) = if keyframe {
            (TCAM_KEYFRAME, self.encode_keyframe(frame)?)
        } else if let Some(data) = self.encode_delta(frame)? {
            (TCAM_DELTA, data)
        } else {
            self.write_skip(timestamp_ms)?;
            self.frame_count += 1;
            return Ok(());
        };

        let mut encoder = ZlibEncoder::new(
            Vec::new(),
            if self.config.zlib_max {
                Compression::best()
            } else {
                Compression::default()
            },
        );
        encoder.write_all(&data)?;
        let compressed = encoder.finish()?;
        self.write_frame_header(frame_type, timestamp_ms, compressed.len() as u32)?;
        self.file.write_all(&compressed)?;
        self.prev_chars = Some(frame.chars.clone());
        self.prev_colors = frame.colors.clone();
        self.prev_timestamp_ms = timestamp_ms;
        self.frame_count += 1;
        Ok(())
    }

    fn write_frame_header(
        &mut self,
        frame_type: u8,
        timestamp_ms: u32,
        data_len: u32,
    ) -> Result<()> {
        self.file.write_u8(frame_type)?;
        if self.config.delta_timestamps {
            let delta = timestamp_ms
                .saturating_sub(self.prev_timestamp_ms)
                .min(u16::MAX as u32) as u16;
            self.file.write_u16::<LittleEndian>(delta)?;
        } else {
            self.file.write_u32::<LittleEndian>(timestamp_ms)?;
        }
        self.file.write_u32::<LittleEndian>(data_len)?;
        Ok(())
    }

    fn write_skip(&mut self, timestamp_ms: u32) -> Result<()> {
        self.write_frame_header(TCAM_SKIP, timestamp_ms, 0)?;
        self.prev_timestamp_ms = timestamp_ms;
        Ok(())
    }

    fn encode_keyframe(&self, frame: &RenderFrame) -> Result<Vec<u8>> {
        let derive = self.config.derive_chars && self.color;
        let mut out = Vec::new();
        if derive {
            out.write_u32::<LittleEndian>(0)?;
        } else {
            let flat: Vec<u8> = frame.chars.join("").bytes().collect();
            let rle = rle_encode(&flat);
            out.write_u32::<LittleEndian>(rle.len() as u32)?;
            out.extend(rle);
        }

        if self.color {
            if let Some(colors) = &frame.colors {
                let mut data = Vec::new();
                for row in colors {
                    for &(r, g, b) in row {
                        if self.config.quantize_colors {
                            data.push(quantize_color(r, g, b));
                        } else {
                            data.extend([r, g, b]);
                        }
                    }
                }
                out.write_u32::<LittleEndian>(data.len() as u32)?;
                out.extend(data);
            } else {
                out.write_u32::<LittleEndian>(0)?;
            }
        } else {
            out.write_u32::<LittleEndian>(0)?;
        }
        Ok(out)
    }

    fn encode_delta(&self, frame: &RenderFrame) -> Result<Option<Vec<u8>>> {
        let prev_chars = self
            .prev_chars
            .as_ref()
            .ok_or_else(|| anyhow!("missing previous chars"))?;
        let prev_colors = self.prev_colors.as_ref();
        let derive = self.config.derive_chars && self.color;
        let mut changes = Vec::new();

        for y in 0..self.height {
            let row = frame.chars.get(y).map(String::as_str).unwrap_or("");
            let prev_row = prev_chars.get(y).map(String::as_str).unwrap_or("");
            for x in 0..self.width {
                let ch = row.as_bytes().get(x).copied().unwrap_or(b' ');
                let prev_ch = prev_row.as_bytes().get(x).copied().unwrap_or(b' ');
                let ch_changed = !derive && ch != prev_ch;
                let cur_color = frame
                    .colors
                    .as_ref()
                    .and_then(|c| c.get(y))
                    .and_then(|r| r.get(x))
                    .copied()
                    .unwrap_or((0, 0, 0));
                let prev_color = prev_colors
                    .and_then(|c| c.get(y))
                    .and_then(|r| r.get(x))
                    .copied()
                    .unwrap_or((0, 0, 0));
                let color_changed = self.color
                    && if self.config.quantize_colors {
                        quantize_color(cur_color.0, cur_color.1, cur_color.2)
                            != quantize_color(prev_color.0, prev_color.1, prev_color.2)
                    } else {
                        cur_color != prev_color
                    };

                if ch_changed || color_changed {
                    if self.config.flat_indices {
                        changes.write_u16::<LittleEndian>((y * self.width + x) as u16)?;
                    } else {
                        changes.write_u16::<LittleEndian>(x as u16)?;
                        changes.write_u16::<LittleEndian>(y as u16)?;
                    }
                    if !derive {
                        changes.push(ch);
                    }
                    if self.color {
                        if self.config.quantize_colors {
                            changes.push(quantize_color(cur_color.0, cur_color.1, cur_color.2));
                        } else {
                            changes.extend([cur_color.0, cur_color.1, cur_color.2]);
                        }
                    }
                }
            }
        }

        if self.config.skip_identical && changes.is_empty() {
            return Ok(None);
        }
        let mut out = Vec::new();
        let entry_size = if self.config.flat_indices { 2 } else { 4 }
            + if derive { 0 } else { 1 }
            + if self.color {
                if self.config.quantize_colors {
                    1
                } else {
                    3
                }
            } else {
                0
            };
        out.write_u32::<LittleEndian>((changes.len() / entry_size) as u32)?;
        out.extend(changes);
        Ok(Some(out))
    }
}

struct TcamDecoder {
    file: File,
    width: usize,
    height: usize,
    fps: u8,
    color: bool,
    config: RecordingConfig,
    lut: Vec<char>,
    current: RenderFrame,
    accumulated_ms: u32,
}

impl TcamDecoder {
    fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let mut magic = [0u8; 4];
        file.read_exact(&mut magic)?;
        if &magic != TCAM_MAGIC {
            return Err(anyhow!("not a .tcam file"));
        }
        let version = file.read_u8()?;
        let (width, height, fps, color, config) = match version {
            TCAM_VERSION_1 => {
                let width = file.read_u16::<LittleEndian>()? as usize;
                let height = file.read_u16::<LittleEndian>()? as usize;
                let fps = file.read_u8()?;
                let flags = file.read_u8()?;
                (
                    width,
                    height,
                    fps,
                    flags & 0x01 != 0,
                    RecordingConfig::v1_defaults(),
                )
            }
            TCAM_VERSION_2 => {
                let width = file.read_u16::<LittleEndian>()? as usize;
                let height = file.read_u16::<LittleEndian>()? as usize;
                let fps = file.read_u8()?;
                let flags = file.read_u16::<LittleEndian>()?;
                (
                    width,
                    height,
                    fps,
                    flags & 0x01 != 0,
                    RecordingConfig::from_flags(flags),
                )
            }
            _ => return Err(anyhow!("unsupported .tcam version {version}")),
        };
        let ramp_len = file.read_u8()? as usize;
        let mut ramp_bytes = vec![0; ramp_len];
        file.read_exact(&mut ramp_bytes)?;
        let ramp = String::from_utf8(ramp_bytes)?;
        let lut = build_lut(&ramp);
        Ok(Self {
            file,
            width,
            height,
            fps,
            color,
            config,
            lut,
            current: RenderFrame::default(),
            accumulated_ms: 0,
        })
    }

    fn read_frame(&mut self) -> Result<Option<(RenderFrame, u32)>> {
        let frame_type = match self.file.read_u8() {
            Ok(v) => v,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let timestamp_ms = if self.config.delta_timestamps {
            let delta = self.file.read_u16::<LittleEndian>()? as u32;
            self.accumulated_ms = self.accumulated_ms.saturating_add(delta);
            self.accumulated_ms
        } else {
            self.file.read_u32::<LittleEndian>()?
        };
        let data_len = self.file.read_u32::<LittleEndian>()? as usize;
        if frame_type == TCAM_SKIP {
            if data_len > 0 {
                let mut discard = vec![0; data_len];
                self.file.read_exact(&mut discard)?;
            }
            return Ok(Some((self.current.clone(), timestamp_ms)));
        }
        let mut compressed = vec![0; data_len];
        self.file.read_exact(&mut compressed)?;
        let mut decoder = ZlibDecoder::new(&compressed[..]);
        let mut data = Vec::new();
        decoder.read_to_end(&mut data)?;
        match frame_type {
            TCAM_KEYFRAME => self.decode_keyframe(&data)?,
            TCAM_DELTA => self.decode_delta(&data)?,
            _ => {}
        }
        Ok(Some((self.current.clone(), timestamp_ms)))
    }

    fn decode_keyframe(&mut self, data: &[u8]) -> Result<()> {
        let mut cur = io::Cursor::new(data);
        let derive = self.config.derive_chars && self.color;
        let rle_len = cur.read_u32::<LittleEndian>()? as usize;
        if rle_len > 0 {
            let mut rle = vec![0; rle_len];
            cur.read_exact(&mut rle)?;
            let flat = rle_decode(&rle);
            let mut chars = Vec::with_capacity(self.height);
            for y in 0..self.height {
                let start = y * self.width;
                let end = (start + self.width).min(flat.len());
                let mut row = String::from_utf8_lossy(&flat[start..end]).to_string();
                while row.len() < self.width {
                    row.push(' ');
                }
                chars.push(row);
            }
            self.current.chars = chars;
        }

        let color_len = cur.read_u32::<LittleEndian>()? as usize;
        if color_len > 0 && self.color {
            let mut color_data = vec![0; color_len];
            cur.read_exact(&mut color_data)?;
            let mut idx = 0;
            let mut colors = vec![vec![(0, 0, 0); self.width]; self.height];
            for y in 0..self.height {
                for x in 0..self.width {
                    colors[y][x] = if self.config.quantize_colors {
                        let c = color_data.get(idx).copied().unwrap_or(16);
                        idx += 1;
                        dequantize_color(c)
                    } else {
                        let rgb = (
                            color_data.get(idx).copied().unwrap_or(0),
                            color_data.get(idx + 1).copied().unwrap_or(0),
                            color_data.get(idx + 2).copied().unwrap_or(0),
                        );
                        idx += 3;
                        rgb
                    };
                }
            }
            self.current.colors = Some(colors);
        } else {
            self.current.colors = None;
        }

        if derive && rle_len == 0 {
            if let Some(colors) = &self.current.colors {
                self.current.chars = colors
                    .iter()
                    .map(|row| {
                        row.iter()
                            .map(|&(r, g, b)| self.derive_char(r, g, b))
                            .collect()
                    })
                    .collect();
            }
        }
        Ok(())
    }

    fn decode_delta(&mut self, data: &[u8]) -> Result<()> {
        let mut cur = io::Cursor::new(data);
        let derive = self.config.derive_chars && self.color;
        let changes = cur.read_u32::<LittleEndian>()? as usize;
        let mut chars: Vec<Vec<u8>> = self
            .current
            .chars
            .iter()
            .map(|r| r.as_bytes().to_vec())
            .collect();
        let mut colors = self.current.colors.clone();
        for _ in 0..changes {
            let (x, y) = if self.config.flat_indices {
                let flat = cur.read_u16::<LittleEndian>()? as usize;
                (flat % self.width, flat / self.width)
            } else {
                (
                    cur.read_u16::<LittleEndian>()? as usize,
                    cur.read_u16::<LittleEndian>()? as usize,
                )
            };
            let mut ch = if derive { None } else { Some(cur.read_u8()?) };
            if self.color {
                let (r, g, b) = if self.config.quantize_colors {
                    dequantize_color(cur.read_u8()?)
                } else {
                    (cur.read_u8()?, cur.read_u8()?, cur.read_u8()?)
                };
                if let Some(grid) = colors.as_mut() {
                    if y < grid.len() && x < grid[y].len() {
                        grid[y][x] = (r, g, b);
                    }
                }
                if derive {
                    ch = Some(self.derive_char(r, g, b) as u8);
                }
            }
            if y < chars.len() && x < chars[y].len() {
                if let Some(ch) = ch {
                    chars[y][x] = ch;
                }
            }
        }
        self.current.chars = chars
            .into_iter()
            .map(|r| String::from_utf8_lossy(&r).to_string())
            .collect();
        self.current.colors = colors;
        Ok(())
    }

    fn derive_char(&self, r: u8, g: u8, b: u8) -> char {
        let gray = (0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) as usize;
        self.lut[gray.min(255)]
    }
}

struct Camera {
    child: Child,
    stdout: ChildStdout,
    frame_bytes: usize,
    width: usize,
    height: usize,
}

impl Camera {
    fn start(
        platform: Platform,
        camera: u32,
        fps: u8,
        width: usize,
        height: usize,
    ) -> Result<Self> {
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-hide_banner", "-loglevel", "error"]);
        match platform {
            Platform::Macos => {
                cmd.args([
                    "-f",
                    "avfoundation",
                    "-framerate",
                    &fps.to_string(),
                    "-video_size",
                    &format!("{width}x{height}"),
                    "-pixel_format",
                    "yuyv422",
                    "-i",
                    &camera.to_string(),
                ]);
            }
            Platform::Linux | Platform::Auto | Platform::Termux => {
                cmd.args([
                    "-f",
                    "v4l2",
                    "-framerate",
                    &fps.to_string(),
                    "-video_size",
                    &format!("{width}x{height}"),
                    "-i",
                    &format!("/dev/video{camera}"),
                ]);
            }
        }
        cmd.args([
            "-vf", "hflip", "-f", "rawvideo", "-pix_fmt", "rgb24", "-an", "pipe:1",
        ]);
        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("start ffmpeg")?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("missing ffmpeg stdout"))?;
        Ok(Self {
            child,
            stdout,
            frame_bytes: width * height * 3,
            width,
            height,
        })
    }

    fn read_frame(&mut self) -> Result<Frame> {
        let mut data = vec![0; self.frame_bytes];
        self.stdout
            .read_exact(&mut data)
            .context("read ffmpeg frame")?;
        Ok(Frame {
            data,
            width: self.width,
            height: self.height,
        })
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct TermuxCamera {
    camera: u32,
    target_width: usize,
    photo_path: PathBuf,
}

impl TermuxCamera {
    fn new(camera: u32, target_width: usize) -> Self {
        let mut photo_path = std::env::temp_dir();
        photo_path.push(format!(
            "rustercam_{}_{}.jpg",
            std::process::id(),
            rand::thread_rng().gen_range(100000..=999999)
        ));
        Self {
            camera,
            target_width,
            photo_path,
        }
    }

    fn read_frame(&mut self) -> Result<Frame> {
        let status = Command::new("termux-camera-photo")
            .args(["-c", &self.camera.to_string()])
            .arg(&self.photo_path)
            .status()
            .context("run termux-camera-photo")?;
        if !status.success() {
            return Err(anyhow!("termux-camera-photo failed"));
        }

        let output = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-i",
                self.photo_path.to_string_lossy().as_ref(),
                "-vf",
                &format!("scale={}:-2,hflip", self.target_width),
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgb24",
                "-frames:v",
                "1",
                "pipe:1",
            ])
            .output()
            .context("decode Termux camera JPEG with ffmpeg")?;
        if !output.status.success() || output.stdout.is_empty() {
            return Err(anyhow!("ffmpeg failed to decode Termux camera frame"));
        }
        let row_bytes = self.target_width * 3;
        if output.stdout.len() % row_bytes != 0 {
            return Err(anyhow!("decoded Termux frame has unexpected byte length"));
        }
        let height = output.stdout.len() / row_bytes;
        Ok(Frame {
            data: output.stdout,
            width: self.target_width,
            height,
        })
    }
}

impl Drop for TermuxCamera {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.photo_path);
    }
}

enum Capture {
    Ffmpeg(Camera),
    Termux(TermuxCamera),
}

impl Capture {
    fn start(
        platform: Platform,
        camera: u32,
        fps: u8,
        width: usize,
        height: usize,
    ) -> Result<Self> {
        if platform == Platform::Termux {
            Ok(Self::Termux(TermuxCamera::new(camera, width)))
        } else {
            Ok(Self::Ffmpeg(Camera::start(
                platform, camera, fps, width, height,
            )?))
        }
    }

    fn read_frame(&mut self) -> Result<Frame> {
        match self {
            Self::Ffmpeg(cam) => cam.read_frame(),
            Self::Termux(cam) => cam.read_frame(),
        }
    }
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
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

struct App {
    platform: Platform,
    camera: u32,
    fps: u8,
    contrast: f32,
    brightness: i16,
    invert: bool,
    char_aspect: f32,
    cam_w: usize,
    cam_h: usize,
    ramp_base: String,
    lut: Vec<char>,
    rotation: u8,
    record_path: Option<PathBuf>,
    rec_config: RecordingConfig,
    encoder: Option<TcamEncoder>,
    last_frame: RenderFrame,
    last_ascii: String,
}

impl App {
    fn new(args: Args) -> Self {
        let platform = match args.platform {
            Platform::Auto => detect_platform(),
            p => p,
        };
        let camera = args
            .camera
            .unwrap_or(if platform == Platform::Termux { 1 } else { 0 });
        let rotation = args
            .rotate
            .unwrap_or(if platform == Platform::Termux { 3 } else { 0 });
        let (cam_w, cam_h) = args.resolution.size();
        let mut rec_config = RecordingConfig::default();
        if args.no_color {
            rec_config.color_mode = ColorMode::Off;
        }
        let ramp_base = match args.ramp {
            Ramp::Long => RAMP_LONG,
            Ramp::Short => RAMP_SHORT,
        }
        .to_string();
        let ramp_for_lut = if args.invert {
            ramp_base.chars().rev().collect::<String>()
        } else {
            ramp_base.clone()
        };
        let lut = build_lut(&ramp_for_lut);
        Self {
            platform,
            camera,
            fps: 30,
            contrast: args.contrast,
            brightness: args.brightness,
            invert: args.invert,
            char_aspect: args.char_aspect.unwrap_or(CHAR_ASPECT_FALLBACK),
            cam_w,
            cam_h,
            ramp_base,
            lut,
            rotation,
            record_path: args.record,
            rec_config,
            encoder: None,
            last_frame: RenderFrame::default(),
            last_ascii: String::new(),
        }
    }

    fn run(&mut self) -> Result<()> {
        let _guard = TerminalGuard::enter()?;
        let mut camera =
            Capture::start(self.platform, self.camera, self.fps, self.cam_w, self.cam_h)?;
        let mut stdout = io::stdout();
        let mut frames = 0usize;
        let mut fps_window = Instant::now();
        let mut fps_actual = 0.0f32;
        let mut show_help = false;
        let mut show_settings = false;
        let mut preset_idx = 0usize;

        loop {
            while event::poll(Duration::from_millis(0))? {
                if let Event::Key(key) = event::read()? {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Char('Q') => return Ok(()),
                        KeyCode::Char('h') => show_help = !show_help,
                        KeyCode::Char('s') => show_settings = !show_settings,
                        KeyCode::Up => self.contrast = (self.contrast + 0.1).min(3.0),
                        KeyCode::Down => self.contrast = (self.contrast - 0.1).max(0.1),
                        KeyCode::Right => self.brightness = (self.brightness + 5).min(100),
                        KeyCode::Left => self.brightness = (self.brightness - 5).max(-100),
                        KeyCode::Char('1') => {
                            self.invert = !self.invert;
                            self.rebuild_lut();
                        }
                        KeyCode::Char('2') => self.rotation = (self.rotation + 1) % 4,
                        KeyCode::Char('3') => self.toggle_recording()?,
                        KeyCode::Char('4') => self.capture_svg()?,
                        KeyCode::Char('H') => self.capture_html()?,
                        KeyCode::Char('5') => {
                            let presets = [
                                "raw",
                                "max",
                                "greyscale",
                                "ascii",
                                "green",
                                "green_gray",
                                "red",
                                "red_gray",
                            ];
                            preset_idx = (preset_idx + 1) % presets.len();
                            self.rec_config.apply_preset(presets[preset_idx]);
                        }
                        KeyCode::Char('7') if show_settings => {
                            self.rec_config.color_mode = self.rec_config.color_mode.cycle()
                        }
                        _ => {}
                    }
                }
            }

            if show_help {
                draw_overlay(&mut stdout, &help_text(self.platform))?;
                std::thread::sleep(Duration::from_millis(40));
                continue;
            }
            if show_settings {
                draw_overlay(&mut stdout, &settings_text(&self.rec_config))?;
                std::thread::sleep(Duration::from_millis(40));
                continue;
            }

            let mut frame = camera.read_frame()?;
            if self.rotation != 0 {
                frame = rotate_frame(&frame, self.rotation);
            }
            let (cols, rows) = terminal_canvas_size();
            let (render_cols, render_rows) = compute_render_size(
                cols as usize,
                rows as usize,
                frame.width,
                frame.height,
                self.char_aspect,
            );
            let rendered = render_frame(
                &frame,
                render_cols,
                render_rows,
                &self.lut,
                self.contrast,
                self.brightness,
                self.rec_config.color_mode,
            );
            self.last_frame = rendered.0;
            self.last_ascii = rendered.1;

            if self.encoder.is_none() {
                if let Some(path) = self.record_path.clone() {
                    let ramp = self.current_ramp();
                    self.encoder = Some(TcamEncoder::create(
                        path,
                        render_cols,
                        render_rows,
                        self.fps,
                        &ramp,
                        self.rec_config.color_mode.is_color(),
                        self.rec_config.clone(),
                    )?);
                }
            }
            if let Some(enc) = self.encoder.as_mut() {
                enc.write_frame(&self.last_frame)?;
            }

            draw_screen(
                &mut stdout,
                &self.last_ascii,
                render_cols as u16,
                render_rows as u16,
                fps_actual,
                self,
            )?;
            frames += 1;
            let elapsed = fps_window.elapsed();
            if elapsed >= Duration::from_millis(500) {
                fps_actual = frames as f32 / elapsed.as_secs_f32();
                frames = 0;
                fps_window = Instant::now();
            }
        }
    }

    fn rebuild_lut(&mut self) {
        let ramp = self.current_ramp();
        self.lut = build_lut(&ramp);
    }

    fn current_ramp(&self) -> String {
        if self.invert {
            self.ramp_base.chars().rev().collect()
        } else {
            self.ramp_base.clone()
        }
    }

    fn toggle_recording(&mut self) -> Result<()> {
        if let Some(mut enc) = self.encoder.take() {
            enc.file.flush()?;
            self.record_path = None;
        } else {
            let dir = Path::new("recording");
            fs::create_dir_all(dir).context("create recording directory")?;
            self.record_path = Some(dir.join(format!(
                "recording_{}.tcam",
                rand::thread_rng().gen_range(100000..=999999)
            )));
        }
        Ok(())
    }

    fn capture_html(&self) -> Result<()> {
        if self.last_ascii.is_empty() {
            return Ok(());
        }
        let dir = Path::new("capture");
        fs::create_dir_all(dir).context("create capture directory")?;
        let name = dir.join(format!(
            "capture_{}.html",
            rand::thread_rng().gen_range(100000..=999999)
        ));
        let body = ansi_to_html(&self.last_ascii);
        let html = format!(
            "<!doctype html><html><head><meta charset=\"utf-8\"><title>rustercam capture</title><style>body{{background:#101014;margin:0;padding:20px}}pre{{font:10px/1 monospace;color:#ddd}}</style></head><body><pre>{body}</pre></body></html>"
        );
        fs::write(name, html)?;
        Ok(())
    }

    fn capture_svg(&self) -> Result<()> {
        if self.last_frame.chars.is_empty() {
            return Ok(());
        }
        let dir = Path::new("capture");
        fs::create_dir_all(dir).context("create capture directory")?;
        let name = dir.join(format!(
            "capture_{}.svg",
            rand::thread_rng().gen_range(100000..=999999)
        ));
        let svg = render_frame_to_svg(&self.last_frame);
        fs::write(name, svg)?;
        Ok(())
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    if let Some(path) = args.play.clone() {
        return run_playback(&path);
    }
    App::new(args).run()
}

fn detect_platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::Macos
    } else if std::env::var("TERMUX_VERSION").is_ok() || Path::new("/data/data/com.termux").exists()
    {
        Platform::Termux
    } else {
        Platform::Linux
    }
}

fn build_lut(ramp: &str) -> Vec<char> {
    let chars: Vec<char> = ramp.chars().collect();
    let n = chars.len();
    (0..=255)
        .map(|i| chars[(i * (n - 1) / 255).min(n - 1)])
        .collect()
}

fn terminal_canvas_size() -> (u16, u16) {
    if let Some((Width(w), Height(h))) = terminal_size() {
        (w.max(1), h.saturating_sub(HUD_LINES).max(1))
    } else {
        (80, 21)
    }
}

fn compute_render_size(
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

fn rotate_frame(frame: &Frame, rotation: u8) -> Frame {
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

fn render_frame(
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

fn quantize_color(r: u8, g: u8, b: u8) -> u8 {
    let ri = ((r as f32 * 5.0 / 255.0).round() as u8).min(5);
    let gi = ((g as f32 * 5.0 / 255.0).round() as u8).min(5);
    let bi = ((b as f32 * 5.0 / 255.0).round() as u8).min(5);
    16 + 36 * ri + 6 * gi + bi
}

fn dequantize_color(idx: u8) -> (u8, u8, u8) {
    let idx = idx.saturating_sub(16);
    let b = (idx % 6) * 51;
    let g = ((idx / 6) % 6) * 51;
    let r = (idx / 36) * 51;
    (r, g, b)
}

fn rle_encode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let val = data[i];
        let mut count = 1usize;
        while i + count < data.len() && data[i + count] == val && count < 255 {
            count += 1;
        }
        out.push(count as u8);
        out.push(val);
        i += count;
    }
    out
}

fn rle_decode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < data.len() {
        out.extend(std::iter::repeat(data[i + 1]).take(data[i] as usize));
        i += 2;
    }
    out
}

fn draw_screen(
    stdout: &mut io::Stdout,
    ascii: &str,
    render_cols: u16,
    render_rows: u16,
    fps: f32,
    app: &App,
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
    let rec = if app.encoder.is_some() {
        "  \x1b[31mREC\x1b[0m"
    } else {
        ""
    };
    let preset = app.rec_config.preset_name().unwrap_or("custom");
    writeln!(
        stdout,
        "\x1b[7m RUSTERCAM | {:5.1} fps | {}(W) x {}(H) | {} ({}){} \x1b[0m\x1b[K",
        fps,
        render_cols,
        render_rows,
        app.rec_config.color_mode.label(),
        preset,
        rec
    )?;
    writeln!(
        stdout,
        " ↑/↓ Contrast {} {:3.1}  │  ←/→ Bright   {} {:+4}\x1b[K",
        make_bar(app.contrast, 0.1, 3.0, 12),
        app.contrast,
        make_bar(app.brightness as f32, -100.0, 100.0, 12),
        app.brightness
    )?;
    write!(
        stdout,
        " 1 invert:{}  2 rot:{}  3 rec  4 capture  5 preset  s settings  h help  q quit\x1b[K",
        if app.invert { "on" } else { "off" },
        app.rotation * 90
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

fn draw_overlay(stdout: &mut io::Stdout, lines: &[String]) -> Result<()> {
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

fn help_text(platform: Platform) -> Vec<String> {
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

fn settings_text(cfg: &RecordingConfig) -> Vec<String> {
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

fn ansi_to_html(input: &str) -> String {
    let mut out = String::new();
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            while let Some(c) = chars.next() {
                if c == 'm' {
                    break;
                }
            }
            continue;
        }
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

fn render_frame_to_svg(frame: &RenderFrame) -> String {
    let cell_w = 8usize;
    let cell_h = 12usize;
    let baseline = 10usize;
    let width_chars = frame
        .chars
        .iter()
        .map(|row| row.chars().count())
        .max()
        .unwrap_or(0);
    let height_chars = frame.chars.len();
    let width = width_chars * cell_w;
    let height = height_chars * cell_h;

    let mut svg = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}">
<rect width="100%" height="100%" fill="#101014"/>
<style>text{{font-family:Menlo,Monaco,'Courier New',monospace;font-size:12px;white-space:pre}}</style>
"##
    );

    for (y, row) in frame.chars.iter().enumerate() {
        if let Some(colors) = &frame.colors {
            let mut current_color: Option<(u8, u8, u8)> = None;
            let mut run = String::new();
            let mut run_start = 0usize;

            for (x, ch) in row.chars().enumerate() {
                let color = colors
                    .get(y)
                    .and_then(|row| row.get(x))
                    .copied()
                    .unwrap_or((220, 220, 220));
                if current_color == Some(color) {
                    run.push(ch);
                } else {
                    push_svg_text_run(
                        &mut svg,
                        run_start,
                        y,
                        baseline,
                        cell_w,
                        cell_h,
                        current_color,
                        &run,
                    );
                    current_color = Some(color);
                    run.clear();
                    run.push(ch);
                    run_start = x;
                }
            }
            push_svg_text_run(
                &mut svg,
                run_start,
                y,
                baseline,
                cell_w,
                cell_h,
                current_color,
                &run,
            );
        } else {
            push_svg_text_run(
                &mut svg,
                0,
                y,
                baseline,
                cell_w,
                cell_h,
                Some((220, 220, 220)),
                row,
            );
        }
    }

    svg.push_str("</svg>\n");
    svg
}

fn push_svg_text_run(
    svg: &mut String,
    x: usize,
    y: usize,
    baseline: usize,
    cell_w: usize,
    cell_h: usize,
    color: Option<(u8, u8, u8)>,
    text: &str,
) {
    if text.is_empty() {
        return;
    }
    let (r, g, b) = color.unwrap_or((220, 220, 220));
    svg.push_str(&format!(
        r#"<text x="{}" y="{}" fill="rgb({r},{g},{b})">{}</text>
"#,
        x * cell_w,
        y * cell_h + baseline,
        escape_xml(text)
    ));
}

fn escape_xml(input: &str) -> String {
    let mut out = String::new();
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

fn run_playback(path: &Path) -> Result<()> {
    let _guard = TerminalGuard::enter()?;
    let mut decoder = TcamDecoder::open(path)?;
    let mut stdout = io::stdout();
    let start = Instant::now();
    let mut frame_num = 0usize;
    loop {
        if event::poll(Duration::from_millis(0))? {
            if let Event::Key(k) = event::read()? {
                if matches!(k.code, KeyCode::Char('q') | KeyCode::Char('Q')) {
                    break;
                }
            }
        }
        let Some((frame, timestamp_ms)) = decoder.read_frame()? else {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        };
        write!(stdout, "\x1b[H")?;
        for (y, row) in frame.chars.iter().enumerate() {
            if let Some(colors) = &frame.colors {
                for (x, ch) in row.chars().enumerate() {
                    let (r, g, b) = colors
                        .get(y)
                        .and_then(|r| r.get(x))
                        .copied()
                        .unwrap_or((255, 255, 255));
                    write!(stdout, "\x1b[38;2;{r};{g};{b}m{ch}")?;
                }
                writeln!(stdout, "\x1b[0m\x1b[K")?;
            } else {
                writeln!(stdout, "{row}\x1b[K")?;
            }
        }
        writeln!(
            stdout,
            "\x1b[7m RUSTERCAM PLAYBACK | frame {frame_num} | {}x{} | {} fps | q quit \x1b[0m\x1b[K",
            decoder.width, decoder.height, decoder.fps
        )?;
        stdout.flush()?;
        frame_num += 1;
        let target = Duration::from_millis(timestamp_ms as u64);
        if target > start.elapsed() {
            std::thread::sleep(target - start.elapsed());
        }
    }
    Ok(())
}

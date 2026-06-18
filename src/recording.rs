use anyhow::{anyhow, Context, Result};
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use flate2::{read::ZlibDecoder, write::ZlibEncoder, Compression};
use std::{
    fs::File,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::Instant,
};

use crate::{
    cli::ColorMode,
    frame::RenderFrame,
    render::{build_lut, dequantize_color, quantize_color},
};

const TCAM_MAGIC: &[u8; 4] = b"TCAM";
const TCAM_VERSION_1: u8 = 1;
const TCAM_VERSION_2: u8 = 2;
const TCAM_KEYFRAME_INTERVAL: usize = 30;
const TCAM_KEYFRAME: u8 = 0;
const TCAM_DELTA: u8 = 1;
const TCAM_SKIP: u8 = 2;

#[derive(Clone)]
pub struct RecordingConfig {
    pub skip_identical: bool,
    pub quantize_colors: bool,
    pub derive_chars: bool,
    pub flat_indices: bool,
    pub delta_timestamps: bool,
    pub zlib_max: bool,
    pub color_mode: ColorMode,
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
    pub fn flags(&self, color: bool) -> u16 {
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

    pub fn from_flags(flags: u16) -> Self {
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

    pub fn v1_defaults() -> Self {
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

    pub fn apply_preset(&mut self, name: &str) {
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

    pub fn preset_name(&self) -> Option<&'static str> {
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

pub struct TcamEncoder {
    pub(crate) file: File,
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
    pub fn create(
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

    pub fn write_frame(&mut self, frame: &RenderFrame) -> Result<()> {
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

    pub fn write_frame_header(
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

pub struct TcamDecoder {
    pub(crate) file: File,
    pub width: usize,
    pub height: usize,
    pub fps: u8,
    color: bool,
    config: RecordingConfig,
    lut: Vec<char>,
    current: RenderFrame,
    accumulated_ms: u32,
}

impl TcamDecoder {
    pub fn open(path: &Path) -> Result<Self> {
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

    pub fn read_frame(&mut self) -> Result<Option<(RenderFrame, u32)>> {
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

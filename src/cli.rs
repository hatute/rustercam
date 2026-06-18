use clap::{Parser, ValueEnum};
use std::path::PathBuf;

pub const RAMP_LONG: &str =
    " .'`^\",:;Il!i><~+_-?][}{1)(|/tfjrxnuvczXYUJCLQ0OZmwqpdbkhao*#MW&8%B@$";
pub const RAMP_SHORT: &str = " .:-=+*#%@";
pub const CHAR_ASPECT_FALLBACK: f32 = 0.45;
pub const HUD_LINES: u16 = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Resolution {
    Low,
    Medium,
    High,
}

impl Resolution {
    pub fn size(self) -> (usize, usize) {
        match self {
            Self::Low => (320, 240),
            Self::Medium => (640, 480),
            Self::High => (1280, 720),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Ramp {
    Long,
    Short,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Platform {
    Auto,
    Macos,
    Linux,
    Termux,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorMode {
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
    pub fn label(self) -> &'static str {
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

    pub fn is_color(self) -> bool {
        self != Self::Off
    }

    pub fn cycle(self) -> Self {
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
pub struct Args {
    #[arg(long)]
    pub no_color: bool,
    #[arg(long, value_enum, default_value_t = Resolution::Medium)]
    pub resolution: Resolution,
    #[arg(long, default_value_t = 1.0)]
    pub contrast: f32,
    #[arg(long, default_value_t = 0)]
    pub brightness: i16,
    #[arg(long, value_enum, default_value_t = Ramp::Long)]
    pub ramp: Ramp,
    #[arg(long)]
    pub invert: bool,
    #[arg(long)]
    pub char_aspect: Option<f32>,
    #[arg(long)]
    pub camera: Option<u32>,
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=3))]
    pub rotate: Option<u8>,
    #[arg(long, value_enum, default_value_t = Platform::Auto)]
    pub platform: Platform,
    #[arg(long)]
    pub record: Option<PathBuf>,
    #[arg(long)]
    pub play: Option<PathBuf>,
}

pub fn detect_platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::Macos
    } else if std::env::var("TERMUX_VERSION").is_ok()
        || std::path::Path::new("/data/data/com.termux").exists()
    {
        Platform::Termux
    } else {
        Platform::Linux
    }
}

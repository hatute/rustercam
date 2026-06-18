use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode};
use rand::Rng;
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use crate::{
    capture::{ensure_camera_device, query_camera_devices, CameraDevice, Capture},
    cli::{
        detect_platform, Args, ColorMode, Platform, Ramp, CHAR_ASPECT_FALLBACK, RAMP_LONG,
        RAMP_SHORT,
    },
    export::{ansi_to_html, render_frame_to_svg},
    frame::RenderFrame,
    recording::{RecordingConfig, TcamEncoder},
    render::{
        build_lut, compute_render_size, flip_frame_horizontal, render_frame_with_stabilizer,
        rotate_frame, RenderStabilizer,
    },
    ui::{
        draw_overlay, draw_screen, help_text, settings_text, terminal_canvas_size, HudState,
        TerminalGuard,
    },
};

pub struct App {
    platform: Platform,
    camera: u32,
    devices: Vec<CameraDevice>,
    fps: u8,
    contrast: f32,
    brightness: i16,
    invert: bool,
    flip: bool,
    char_aspect: f32,
    cam_w: usize,
    cam_h: usize,
    ramp_base: String,
    lut: Vec<char>,
    rotation: u8,
    record_path: Option<PathBuf>,
    rec_config: RecordingConfig,
    encoder: Option<TcamEncoder>,
    render_stabilizer: RenderStabilizer,
    last_frame: RenderFrame,
    last_ascii: String,
}

impl App {
    pub fn new(args: Args) -> Self {
        let platform = match args.platform {
            Platform::Auto => detect_platform(),
            p => p,
        };
        let camera = args
            .camera
            .unwrap_or(if platform == Platform::Termux { 1 } else { 0 });
        let devices = query_camera_devices(camera);
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
            devices,
            fps: 30,
            contrast: args.contrast,
            brightness: args.brightness,
            invert: args.invert,
            flip: args.flip,
            char_aspect: args.char_aspect.unwrap_or(CHAR_ASPECT_FALLBACK),
            cam_w,
            cam_h,
            ramp_base,
            lut,
            rotation,
            record_path: args.record,
            rec_config,
            encoder: None,
            render_stabilizer: RenderStabilizer::default(),
            last_frame: RenderFrame::default(),
            last_ascii: String::new(),
        }
    }

    pub fn run(&mut self) -> Result<()> {
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
                        KeyCode::Char('f') | KeyCode::Char('F') => {
                            self.flip = !self.flip;
                            self.render_stabilizer.reset();
                        }
                        KeyCode::Char('3') => self.toggle_recording()?,
                        KeyCode::Char('4') => self.capture_svg()?,
                        KeyCode::Char('H') => self.capture_html()?,
                        KeyCode::Char('c') | KeyCode::Char('C') => {
                            if let Some(next_camera) = self.next_camera_id() {
                                let next = Capture::start(
                                    self.platform,
                                    next_camera,
                                    self.fps,
                                    self.cam_w,
                                    self.cam_h,
                                )?;
                                camera = next;
                                self.camera = next_camera;
                                self.render_stabilizer.reset();
                                ensure_camera_device(&mut self.devices, next_camera);
                            }
                        }
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
            if self.flip {
                frame = flip_frame_horizontal(&frame);
            }
            let (cols, rows) = terminal_canvas_size();
            let (render_cols, render_rows) = compute_render_size(
                cols as usize,
                rows as usize,
                frame.width,
                frame.height,
                self.char_aspect,
            );
            let rendered = render_frame_with_stabilizer(
                &frame,
                render_cols,
                render_rows,
                &self.lut,
                self.contrast,
                self.brightness,
                self.rec_config.color_mode,
                Some(&mut self.render_stabilizer),
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

            let camera_status = self.camera_status();
            let hud = HudState {
                recording: self.encoder.is_some(),
                preset: self.rec_config.preset_name().unwrap_or("custom"),
                color_mode_label: self.rec_config.color_mode.label(),
                contrast: self.contrast,
                brightness: self.brightness,
                camera_status: &camera_status,
                invert: self.invert,
                flip: self.flip,
                rotation_degrees: self.rotation * 90,
            };
            draw_screen(
                &mut stdout,
                &self.last_ascii,
                render_cols as u16,
                render_rows as u16,
                fps_actual,
                camera.backend_label(),
                &hud,
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
        self.render_stabilizer.reset();
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

    fn next_camera_id(&self) -> Option<u32> {
        if self.devices.len() < 2 {
            return None;
        }
        let pos = self
            .devices
            .iter()
            .position(|device| device.id == self.camera)
            .unwrap_or(0);
        Some(self.devices[(pos + 1) % self.devices.len()].id)
    }

    fn camera_status(&self) -> String {
        let total = self.devices.len().max(1);
        let current_index = self
            .devices
            .iter()
            .position(|device| device.id == self.camera)
            .unwrap_or(0);
        let name = self
            .devices
            .get(current_index)
            .map(|device| device.name.as_str())
            .unwrap_or("Camera");
        format!("{name} ({}/{total})", current_index + 1)
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
        let svg = render_frame_to_svg(&self.last_frame, self.char_aspect);
        fs::write(name, svg)?;
        Ok(())
    }
}

use anyhow::{anyhow, Context, Result};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use nokhwa::{
    pixel_format::RgbFormat,
    utils::{
        ApiBackend, CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType,
    },
    Camera as NokhwaCamera,
};
use rand::Rng;
#[cfg(target_os = "macos")]
use std::sync::mpsc;
use std::{
    fs,
    io::Read,
    path::PathBuf,
    process::{Child, ChildStdout, Command, Stdio},
    time::Duration,
};

use crate::{cli::Platform, frame::Frame};

#[derive(Clone)]
pub struct CameraDevice {
    pub id: u32,
    pub name: String,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) struct NativeCamera {
    camera: NokhwaCamera,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl NativeCamera {
    pub fn start(camera: u32, fps: u8, width: usize, height: usize) -> Result<Self> {
        init_native_camera()?;
        let requested = RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(
            CameraFormat::new_from(width as u32, height as u32, FrameFormat::RAWRGB, fps as u32),
        ));
        let mut camera = NokhwaCamera::new(CameraIndex::Index(camera), requested)
            .context("start native camera")?;
        camera.open_stream().context("open native camera stream")?;
        Ok(Self { camera })
    }

    pub fn read_frame(&mut self) -> Result<Frame> {
        let frame = self.camera.frame().context("read native camera frame")?;
        let resolution = frame.resolution();
        let image = frame
            .decode_image::<RgbFormat>()
            .context("decode native camera frame")?;
        Ok(Frame {
            data: image.into_raw(),
            width: resolution.width() as usize,
            height: resolution.height() as usize,
        })
    }
}

#[cfg(target_os = "macos")]
fn init_native_camera() -> Result<()> {
    let (tx, rx) = mpsc::channel();
    nokhwa::nokhwa_initialize(move |ready| {
        let _ = tx.send(ready);
    });
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(true) => Ok(()),
        Ok(false) => Err(anyhow!("native camera permission was denied")),
        Err(_) => Err(anyhow!("timed out while initializing native camera")),
    }
}

#[cfg(all(
    any(target_os = "macos", target_os = "linux"),
    not(target_os = "macos")
))]
fn init_native_camera() -> Result<()> {
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn query_camera_devices(default_camera: u32) -> Vec<CameraDevice> {
    let mut devices: Vec<CameraDevice> = nokhwa::query(ApiBackend::Auto)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|info| {
            let id = info.index().as_index().ok()?;
            let name = info.human_name();
            Some(CameraDevice {
                id,
                name: if name.trim().is_empty() {
                    format!("Camera {id}")
                } else {
                    name
                },
            })
        })
        .collect();
    devices.sort_by_key(|device| device.id);
    devices.dedup_by_key(|device| device.id);
    ensure_camera_device(&mut devices, default_camera);
    devices
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn query_camera_devices(default_camera: u32) -> Vec<CameraDevice> {
    vec![CameraDevice {
        id: default_camera,
        name: format!("Camera {default_camera}"),
    }]
}

pub fn ensure_camera_device(devices: &mut Vec<CameraDevice>, camera: u32) {
    if devices.iter().all(|device| device.id != camera) {
        devices.push(CameraDevice {
            id: camera,
            name: format!("Camera {camera}"),
        });
        devices.sort_by_key(|device| device.id);
    }
}

pub(crate) struct FfmpegCamera {
    child: Child,
    stdout: ChildStdout,
    frame_bytes: usize,
    width: usize,
    height: usize,
}

impl FfmpegCamera {
    pub fn start(
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

    pub fn read_frame(&mut self) -> Result<Frame> {
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

impl Drop for FfmpegCamera {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub(crate) struct TermuxCamera {
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

    pub fn read_frame(&mut self) -> Result<Frame> {
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

pub enum Capture {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    Native(NativeCamera),
    Ffmpeg(FfmpegCamera),
    Termux(TermuxCamera),
}

impl Capture {
    pub fn start(
        platform: Platform,
        camera: u32,
        fps: u8,
        width: usize,
        height: usize,
    ) -> Result<Self> {
        if platform == Platform::Termux {
            Ok(Self::Termux(TermuxCamera::new(camera, width)))
        } else {
            let native_error = match Self::start_native(camera, fps, width, height) {
                Ok(camera) => return Ok(camera),
                Err(err) => err,
            };
            let fallback = FfmpegCamera::start(platform, camera, fps, width, height).with_context(
                || format!("native camera backend failed first: {native_error:#}; ffmpeg fallback also failed"),
            )?;
            Ok(Self::Ffmpeg(fallback))
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub fn start_native(camera: u32, fps: u8, width: usize, height: usize) -> Result<Self> {
        Ok(Self::Native(NativeCamera::start(
            camera, fps, width, height,
        )?))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    pub fn start_native(_camera: u32, _fps: u8, _width: usize, _height: usize) -> Result<Self> {
        Err(anyhow!(
            "native camera backend is not available on this platform"
        ))
    }

    pub fn read_frame(&mut self) -> Result<Frame> {
        match self {
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            Self::Native(cam) => cam.read_frame(),
            Self::Ffmpeg(cam) => cam.read_frame(),
            Self::Termux(cam) => cam.read_frame(),
        }
    }

    pub fn backend_label(&self) -> &'static str {
        match self {
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            Self::Native(_) => "nokhwa",
            Self::Ffmpeg(_) => "ffmpeg",
            Self::Termux(_) => "termux",
        }
    }
}

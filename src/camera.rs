//! Camera worker thread. Owns the nokhwa camera, decodes frames, publishes a
//! downscaled live preview and serves full resolution capture requests.

use crate::decode::{decode, ALL_FORMATS};
use crate::imageops::{average, box_downscale, RgbImage, PREVIEW_MAX};
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{
    ApiBackend, CameraFormat, CameraIndex, ControlValueDescription, ControlValueSetter, FrameFormat,
    KnownCameraControl, KnownCameraControlFlag, RequestedFormat, RequestedFormatType,
};
use nokhwa::Camera;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone)]
pub struct CamInfo {
    pub name: String,
    pub index: CameraIndex,
}

#[derive(Clone)]
pub struct CtrlInfo {
    pub id: KnownCameraControl,
    pub name: String,
    pub desc: ControlValueDescription,
    pub read_only: bool,
}

pub enum Cmd {
    List,
    Open { index: CameraIndex, format: Option<CameraFormat> },
    Close,
    SetControl(KnownCameraControl, ControlValueSetter),
    SetControls(Vec<(KnownCameraControl, ControlValueSetter)>),
    Capture { avg: u32 },
}

pub enum Ev {
    List(Vec<CamInfo>),
    Opened { name: String, format: CameraFormat, formats: Vec<CameraFormat> },
    Controls(Vec<CtrlInfo>),
    Closed,
    Error(String),
    Captured(RgbImage),
}

pub struct LiveFrame {
    pub seq: u64,
    pub preview: Arc<RgbImage>,
    pub full_w: usize,
    pub full_h: usize,
}

pub struct CameraHandle {
    tx: Sender<Cmd>,
    pub rx: Receiver<Ev>,
    pub live: Arc<Mutex<Option<LiveFrame>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl CameraHandle {
    pub fn spawn(ctx: eframe::egui::Context) -> Self {
        let (tx, crx) = channel();
        let (etx, rx) = channel();
        let live = Arc::new(Mutex::new(None));
        let l2 = live.clone();
        let thread = std::thread::spawn(move || worker(crx, etx, l2, ctx));
        Self { tx, rx, live, thread: Some(thread) }
    }

    pub fn send(&self, c: Cmd) {
        let _ = self.tx.send(c);
    }

    /// Ask the worker to release the camera and exit. Not joined: a stalled driver
    /// must not hang application exit.
    pub fn shutdown(&mut self) {
        let _ = self.tx.send(Cmd::Close);
        let (dead_tx, _) = channel();
        self.tx = dead_tx; // dropping the real sender ends the worker loop
        if let Some(t) = self.thread.take() {
            let deadline = std::time::Instant::now() + Duration::from_millis(1500);
            while !t.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

fn panic_msg(p: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "camera backend panicked".into()
    }
}

fn guarded<T>(f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(p) => Err(panic_msg(p)),
    }
}

fn is_compressed(f: FrameFormat) -> bool {
    matches!(f, FrameFormat::MJPEG)
}

/// Highest pixel count first. At that size prefer uncompressed if it still gives a usable
/// live view (>= 10 fps), otherwise the fastest format.
pub fn pick_best(formats: &[CameraFormat]) -> Option<CameraFormat> {
    let max_px = formats.iter().map(|f| f.resolution().width() as u64 * f.resolution().height() as u64).max()?;
    let mut top: Vec<CameraFormat> = formats
        .iter()
        .copied()
        .filter(|f| f.resolution().width() as u64 * f.resolution().height() as u64 == max_px)
        .collect();
    top.sort_by_key(|f| std::cmp::Reverse(f.frame_rate()));
    top.iter()
        .find(|f| !is_compressed(f.format()) && f.frame_rate() >= 10)
        .or_else(|| top.first())
        .copied()
}

pub fn sorted_formats(mut formats: Vec<CameraFormat>) -> Vec<CameraFormat> {
    formats.sort_by_key(|f| {
        (
            std::cmp::Reverse(f.resolution().width() as u64 * f.resolution().height() as u64),
            is_compressed(f.format()),
            std::cmp::Reverse(f.frame_rate()),
        )
    });
    formats.dedup();
    formats
}

fn read_controls(cam: &Camera) -> Vec<CtrlInfo> {
    guarded(|| cam.camera_controls().map_err(|e| e.to_string()))
        .unwrap_or_default()
        .into_iter()
        .filter(|c| !matches!(c.description(), ControlValueDescription::None))
        .map(|c| CtrlInfo {
            id: c.control(),
            name: c.name().to_string(),
            desc: c.description().clone(),
            read_only: c.flag().contains(&KnownCameraControlFlag::ReadOnly)
                || c.flag().contains(&KnownCameraControlFlag::Disabled),
        })
        .collect()
}

fn set_control(cam: &mut Camera, id: KnownCameraControl, v: ControlValueSetter) -> Result<(), String> {
    let first = guarded(|| cam.set_camera_control(id, v.clone()).map_err(|e| e.to_string()));
    if first.is_err() {
        // some backends want menu values as EnumValue
        if let ControlValueSetter::Integer(i) = v {
            if guarded(|| cam.set_camera_control(id, ControlValueSetter::EnumValue(i)).map_err(|e| e.to_string())).is_ok() {
                return Ok(());
            }
        }
    }
    first
}

fn open_camera(index: &CameraIndex, format: Option<CameraFormat>) -> Result<(Camera, Vec<CameraFormat>), String> {
    let req = RequestedFormat::with_formats(RequestedFormatType::AbsoluteHighestResolution, ALL_FORMATS);
    let mut cam = guarded(|| Camera::new(index.clone(), req).map_err(|e| e.to_string()))
        .or_else(|_| {
            let req = RequestedFormat::new::<RgbFormat>(RequestedFormatType::None);
            guarded(|| Camera::new(index.clone(), req).map_err(|e| e.to_string()))
        })?;
    let formats = sorted_formats(
        guarded(|| cam.compatible_camera_formats().map_err(|e| e.to_string()))
            .unwrap_or_default()
            .into_iter()
            .filter(|f| ALL_FORMATS.contains(&f.format()))
            .collect(),
    );
    let want = format.or_else(|| pick_best(&formats));
    if let Some(f) = want {
        if f != cam.camera_format() {
            let r = RequestedFormat::with_formats(RequestedFormatType::Exact(f), ALL_FORMATS);
            if let Err(e) = guarded(|| cam.set_camera_requset(r).map(|_| ()).map_err(|e| e.to_string())) {
                eprintln!("format {f} rejected: {e}");
            }
        }
    }
    guarded(|| cam.open_stream().map_err(|e| e.to_string()))?;
    Ok((cam, formats))
}

struct CaptureReq {
    want: usize,
    skip: usize,
    frames: Vec<RgbImage>,
}

fn worker(rx: Receiver<Cmd>, tx: Sender<Ev>, live: Arc<Mutex<Option<LiveFrame>>>, ctx: eframe::egui::Context) {
    let mut cam: Option<Camera> = None;
    // Dev aid: TURTLEFRAME_FAKE_CAMERA=/path/image.png adds a still-image "camera".
    let fake_path = std::env::var_os("TURTLEFRAME_FAKE_CAMERA").map(std::path::PathBuf::from);
    let mut fake: Option<RgbImage> = None;
    let mut seq: u64 = 0;
    let mut errors = 0u32;
    let mut capture: Option<CaptureReq> = None;
    let send = |e: Ev| {
        let _ = tx.send(e);
        ctx.request_repaint();
    };

    loop {
        // commands
        let mut cmds = Vec::new();
        if cam.is_some() || fake.is_some() {
            loop {
                match rx.try_recv() {
                    Ok(c) => cmds.push(c),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return,
                }
            }
        } else {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(c) => cmds.push(c),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        for c in cmds {
            match c {
                Cmd::List => {
                    let list = guarded(|| nokhwa::query(ApiBackend::Auto).map_err(|e| e.to_string()));
                    match list {
                        Ok(l) => {
                            let mut list: Vec<CamInfo> =
                                l.into_iter().map(|i| CamInfo { name: i.human_name(), index: i.index().clone() }).collect();
                            if fake_path.is_some() {
                                list.insert(0, CamInfo { name: "Fake camera".into(), index: CameraIndex::String("fake".into()) });
                            }
                            send(Ev::List(list))
                        }
                        Err(_) if fake_path.is_some() => send(Ev::List(vec![CamInfo {
                            name: "Fake camera".into(),
                            index: CameraIndex::String("fake".into()),
                        }])),
                        Err(e) => send(Ev::Error(format!("Listing cameras failed: {e}"))),
                    }
                }
                Cmd::Open { index, format } => {
                    if let Some(mut c) = cam.take() {
                        let _ = guarded(|| c.stop_stream().map_err(|e| e.to_string()));
                    }
                    *live.lock().unwrap() = None;
                    capture = None;
                    fake = None;
                    if let (CameraIndex::String(s), Some(fp)) = (&index, &fake_path) {
                        if s == "fake" {
                            match crate::imageops::load_rgb(fp) {
                                Ok(img) => {
                                    let fmt = CameraFormat::new_from(img.w as u32, img.h as u32, FrameFormat::RAWRGB, 15);
                                    send(Ev::Opened { name: "Fake camera".into(), format: fmt, formats: vec![fmt] });
                                    send(Ev::Controls(Vec::new()));
                                    fake = Some(img);
                                }
                                Err(e) => send(Ev::Error(e)),
                            }
                            continue;
                        }
                    }
                    match open_camera(&index, format) {
                        Ok((c, formats)) => {
                            let name = c.info().human_name();
                            let fmt = c.camera_format();
                            send(Ev::Opened { name, format: fmt, formats });
                            send(Ev::Controls(read_controls(&c)));
                            errors = 0;
                            cam = Some(c);
                        }
                        Err(e) => send(Ev::Error(format!("Could not open camera: {e}"))),
                    }
                }
                Cmd::Close => {
                    fake = None;
                    if let Some(mut c) = cam.take() {
                        let _ = guarded(|| c.stop_stream().map_err(|e| e.to_string()));
                    }
                    *live.lock().unwrap() = None;
                    if capture.take().is_some() {
                        send(Ev::Error("Capture cancelled: camera closed".into()));
                    }
                    send(Ev::Closed);
                }
                Cmd::SetControl(id, v) => {
                    if let Some(c) = cam.as_mut() {
                        if let Err(e) = set_control(c, id, v) {
                            send(Ev::Error(format!("Camera control {id}: {e}")));
                        }
                        send(Ev::Controls(read_controls(c)));
                    }
                }
                Cmd::SetControls(list) => {
                    if let Some(c) = cam.as_mut() {
                        let mut failed = Vec::new();
                        for (id, v) in list {
                            if set_control(c, id, v).is_err() {
                                failed.push(id.to_string());
                            }
                        }
                        if !failed.is_empty() {
                            send(Ev::Error(format!("Controls not applied: {}", failed.join(", "))));
                        }
                        send(Ev::Controls(read_controls(c)));
                    }
                }
                Cmd::Capture { avg } => {
                    if cam.is_some() || fake.is_some() {
                        capture = Some(CaptureReq { want: avg.clamp(1, 16) as usize, skip: 1, frames: Vec::new() });
                    } else {
                        send(Ev::Error("No camera open".into()));
                    }
                }
            }
        }

        // frames
        let got = if let Some(f) = &fake {
            std::thread::sleep(Duration::from_millis(66));
            Ok(f.clone())
        } else {
            let Some(c) = cam.as_mut() else { continue };
            guarded(|| {
                let buf = c.frame().map_err(|e| e.to_string())?;
                let res = buf.resolution();
                decode(buf.source_frame_format(), res.width() as usize, res.height() as usize, buf.buffer())
            })
        };
        match got {
            Ok(full) => {
                errors = 0;
                seq += 1;
                let preview = Arc::new(box_downscale(&full, PREVIEW_MAX));
                *live.lock().unwrap() = Some(LiveFrame { seq, preview, full_w: full.w, full_h: full.h });
                ctx.request_repaint();
                if let Some(req) = capture.as_mut() {
                    if req.skip > 0 {
                        req.skip -= 1;
                    } else {
                        req.frames.push(full);
                        if req.frames.len() >= req.want {
                            let req = capture.take().unwrap();
                            if let Some(img) = average(&req.frames) {
                                send(Ev::Captured(img));
                            }
                        }
                    }
                }
            }
            Err(e) => {
                errors += 1;
                if errors == 1 || errors % 20 == 0 {
                    eprintln!("frame error: {e}");
                }
                if errors > 60 {
                    if let Some(mut c) = cam.take() {
                        let _ = guarded(|| c.stop_stream().map_err(|e| e.to_string()));
                    }
                    *live.lock().unwrap() = None;
                    capture = None;
                    send(Ev::Error(format!("Camera stopped after repeated errors: {e}")));
                    send(Ev::Closed);
                } else {
                    std::thread::sleep(Duration::from_millis(15));
                }
            }
        }
    }
}

// ---- persistence helpers for camera controls ----

pub fn control_key(id: &KnownCameraControl) -> String {
    match id {
        KnownCameraControl::Other(n) => format!("other:{n}"),
        k => format!("{k:?}"),
    }
}

pub fn control_from_key(s: &str) -> Option<KnownCameraControl> {
    if let Some(n) = s.strip_prefix("other:") {
        return n.parse().ok().map(KnownCameraControl::Other);
    }
    nokhwa::utils::all_known_camera_controls().into_iter().find(|k| format!("{k:?}") == s)
}

pub fn value_to_str(v: &ControlValueSetter) -> Option<String> {
    Some(match v {
        ControlValueSetter::Integer(i) => format!("i:{i}"),
        ControlValueSetter::EnumValue(i) => format!("i:{i}"),
        ControlValueSetter::Float(f) => format!("f:{f}"),
        ControlValueSetter::Boolean(b) => format!("b:{}", *b as u8),
        _ => return None,
    })
}

pub fn value_from_str(s: &str) -> Option<ControlValueSetter> {
    let (t, v) = s.split_once(':')?;
    Some(match t {
        "i" => ControlValueSetter::Integer(v.parse().ok()?),
        "f" => ControlValueSetter::Float(v.parse().ok()?),
        "b" => ControlValueSetter::Boolean(v == "1"),
        _ => return None,
    })
}

pub fn current_value(d: &ControlValueDescription) -> Option<ControlValueSetter> {
    Some(match d {
        ControlValueDescription::Integer { value, .. } | ControlValueDescription::IntegerRange { value, .. } => {
            ControlValueSetter::Integer(*value)
        }
        ControlValueDescription::Enum { value, .. } => ControlValueSetter::Integer(*value),
        ControlValueDescription::Float { value, .. } | ControlValueDescription::FloatRange { value, .. } => {
            ControlValueSetter::Float(*value)
        }
        ControlValueDescription::Boolean { value, .. } => ControlValueSetter::Boolean(*value),
        _ => return None,
    })
}

pub fn default_value(d: &ControlValueDescription) -> Option<ControlValueSetter> {
    Some(match d {
        ControlValueDescription::Integer { default, .. } | ControlValueDescription::IntegerRange { default, .. } => {
            ControlValueSetter::Integer(*default)
        }
        ControlValueDescription::Enum { default, .. } => ControlValueSetter::Integer(*default),
        ControlValueDescription::Float { default, .. } | ControlValueDescription::FloatRange { default, .. } => {
            ControlValueSetter::Float(*default)
        }
        ControlValueDescription::Boolean { default, .. } => ControlValueSetter::Boolean(*default),
        _ => return None,
    })
}

pub fn format_label(f: &CameraFormat) -> String {
    format!("{} x {}  {}  {} fps", f.resolution().width(), f.resolution().height(), f.format(), f.frame_rate())
}

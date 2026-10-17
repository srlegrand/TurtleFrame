use crate::browser::FolderBrowser;
use crate::camera::{self, CamInfo, CameraHandle, Cmd, CtrlInfo, Ev};
use crate::config::{home_dir, Config};
use crate::imageops::{self, box_downscale, RgbImage, PREVIEW_MAX, THUMB_MAX};
use crate::project::{Project, Take};
use crate::store::{FrameStore, Loaded};
use crate::undistort::{self, Aspect, Lut, Solved, MAX_LINES};
use eframe::egui::{self, pos2, vec2, Color32, Key, Pos2, Rect, Sense, Stroke, TextureHandle, TextureOptions};
use nokhwa::utils::{CameraFormat, ControlValueDescription, ControlValueSetter};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BACK_TINT: [u8; 3] = [255, 40, 40];
const FWD_TINT: [u8; 3] = [40, 230, 70];
const HANDLE_RADIUS: f32 = 7.0;
const PICK_RADIUS: f32 = 14.0;

#[derive(Clone, Copy, PartialEq, Debug)]
enum View {
    Live,
    Frame(usize),
}

#[derive(Clone, Copy, PartialEq, Debug, Hash)]
enum OnionMode {
    Lines,
    Edges,
    Image,
}

struct Onion {
    enabled: bool,
    mode: OnionMode,
    back: usize,
    fwd: usize,
    opacity: f32,
    threshold: f32,
}

struct LutEntry {
    rev: u64,
    w: usize,
    h: usize,
    lut: Arc<Lut>,
    solved: Arc<Solved>,
}

struct Play {
    start: Instant,
    from: usize,
    last_idx: usize,
}

struct ExportJob {
    done: Arc<AtomicUsize>,
    total: usize,
    finished: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    dir: PathBuf,
}

enum Drag {
    Handle(usize, usize),
    NewLine([f32; 2]),
}

pub struct App {
    ctx: egui::Context,
    cam: CameraHandle,
    cameras: Vec<CamInfo>,
    cam_sel: usize,
    cam_open: Option<(String, CameraFormat)>,
    cam_formats: Vec<CameraFormat>,
    controls: Vec<CtrlInfo>,
    ctrl_local: HashMap<String, f64>,
    auto_open_tried: bool,
    avg_frames: u32,
    capture_pending: bool,

    live_tex: Option<TextureHandle>,
    live_key: (u64, u64, bool),
    live_dims: Option<(usize, usize)>, // full res of live camera

    project: Option<Project>,
    take: Option<Take>,
    store: FrameStore,
    thumbs: HashMap<PathBuf, TextureHandle>,

    view: View,
    cursor: Option<usize>,
    insert_mode: bool,
    scroll_to_current: bool,

    onion: Onion,
    onion_cache: HashMap<PathBuf, TextureHandle>,
    onion_key: u64,

    frame_tex: Option<TextureHandle>,
    frame_key: Option<(PathBuf, u64, bool)>,
    frame_full: Option<(usize, usize)>,

    luts: Vec<LutEntry>,
    status_cache: Option<(u64, (usize, usize), String)>,
    rev: u64,
    meta_dirty: bool,

    editing_lines: bool,
    add_armed: bool,
    drag: Option<Drag>,

    play: Option<Play>,
    loop_play: bool,

    undo: Vec<Vec<String>>,
    redo: Vec<Vec<String>>,

    browser: FolderBrowser,
    config: Config,
    status: String,
    status_at: Instant,

    export: Option<ExportJob>,
    export_holds: bool,
    new_take_name: String,
    rename_buf: String,
}

fn fit_rect(avail: Rect, w: f32, h: f32) -> Rect {
    let s = (avail.width() / w).min(avail.height() / h);
    Rect::from_center_size(avail.center(), vec2(w * s, h * s))
}

fn uv_full() -> Rect {
    Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0))
}

fn tint(c: [u8; 3], a: f32) -> Color32 {
    let a = (a.clamp(0.0, 1.0) * 255.0) as u8;
    Color32::from_rgba_unmultiplied(c[0], c[1], c[2], a)
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let ctx = cc.egui_ctx.clone();
        ctx.set_visuals(egui::Visuals::dark());
        let cam = CameraHandle::spawn(ctx.clone());
        cam.send(Cmd::List);
        let config = Config::load();
        let start = config.last_browse.clone().unwrap_or_else(home_dir);
        let mut app = Self {
            ctx: ctx.clone(),
            cam,
            cameras: Vec::new(),
            cam_sel: 0,
            cam_open: None,
            cam_formats: Vec::new(),
            controls: Vec::new(),
            ctrl_local: HashMap::new(),
            auto_open_tried: false,
            avg_frames: 1,
            capture_pending: false,
            live_tex: None,
            live_key: (0, u64::MAX, false),
            live_dims: None,
            project: None,
            take: None,
            store: FrameStore::new(ctx.clone()),
            thumbs: HashMap::new(),
            view: View::Live,
            cursor: None,
            insert_mode: false,
            scroll_to_current: false,
            onion: Onion { enabled: true, mode: OnionMode::Lines, back: 2, fwd: 1, opacity: 0.6, threshold: 0.7 },
            onion_cache: HashMap::new(),
            onion_key: 0,
            frame_tex: None,
            frame_key: None,
            frame_full: None,
            luts: Vec::new(),
            status_cache: None,
            rev: 1,
            meta_dirty: false,
            editing_lines: false,
            add_armed: false,
            drag: None,
            play: None,
            loop_play: true,
            undo: Vec::new(),
            redo: Vec::new(),
            browser: FolderBrowser::new(start),
            config,
            status: String::new(),
            status_at: Instant::now(),
            export: None,
            export_holds: true,
            new_take_name: String::new(),
            rename_buf: String::new(),
        };
        if let Some(last) = app.config.recent.first().cloned() {
            if last.join(crate::project::MARKER).is_file() {
                app.open_project(last);
            }
        }
        app
    }

    fn set_status(&mut self, s: impl Into<String>) {
        self.status = s.into();
        self.status_at = Instant::now();
    }

    // ---------------------------------------------------------------- project

    fn open_project(&mut self, root: PathBuf) {
        self.save_meta_now();
        match Project::open_or_init(&root) {
            Ok(p) => match p.load_take(&p.current_take) {
                Ok(t) => {
                    self.set_status(format!("Opened {}", root.display()));
                    self.project = Some(p);
                    self.take = Some(t);
                    self.reset_take_state();
                    self.config.last_browse = root.parent().map(|p| p.to_path_buf());
                    self.config.push_recent(root);
                }
                Err(e) => self.set_status(format!("Could not load take: {e}")),
            },
            Err(e) => self.set_status(format!("Could not open project: {e}")),
        }
    }

    fn switch_take(&mut self, dir: &str) {
        self.save_meta_now();
        let Some(p) = self.project.as_mut() else { return };
        match p.load_take(dir) {
            Ok(t) => {
                p.current_take = t.dir_name();
                let _ = p.save();
                self.take = Some(t);
                self.reset_take_state();
            }
            Err(e) => self.set_status(format!("Could not load take: {e}")),
        }
    }

    fn reset_take_state(&mut self) {
        self.view = View::Live;
        self.cursor = None;
        self.undo.clear();
        self.redo.clear();
        self.frame_key = None;
        self.onion_cache.clear();
        self.store.clear_queue();
        self.play = None;
        self.drag = None;
        self.add_armed = false;
        self.rev += 1;
        self.rename_buf = self.take.as_ref().map(|t| t.name.clone()).unwrap_or_default();
        self.scroll_to_current = true;
    }

    fn save_meta_now(&mut self) {
        if let Some(t) = &self.take {
            if let Err(e) = t.save_meta() {
                self.status = format!("Save failed: {e}");
            }
        }
        self.meta_dirty = false;
    }

    fn seq_len(&self) -> usize {
        self.take.as_ref().map(|t| t.seq.len()).unwrap_or(0)
    }

    fn frame_path(&self, i: usize) -> Option<PathBuf> {
        let t = self.take.as_ref()?;
        t.seq.get(i).map(|id| t.frame_path(id))
    }

    /// Where the next capture lands.
    fn insert_pos(&self) -> usize {
        let n = self.seq_len();
        if self.insert_mode {
            self.cursor.map(|c| (c + 1).min(n)).unwrap_or(0)
        } else {
            n
        }
    }

    fn edit_seq(&mut self, f: impl FnOnce(&mut Vec<String>)) {
        let Some(t) = self.take.as_mut() else { return };
        self.undo.push(t.seq.clone());
        if self.undo.len() > 200 {
            self.undo.remove(0);
        }
        self.redo.clear();
        f(&mut t.seq);
        if let Err(e) = t.save_seq() {
            self.status = format!("Save failed: {e}");
        }
    }

    fn undo_redo(&mut self, redo: bool) {
        let Some(t) = self.take.as_mut() else { return };
        let (from, to) = if redo { (&mut self.redo, &mut self.undo) } else { (&mut self.undo, &mut self.redo) };
        if let Some(prev) = from.pop() {
            to.push(std::mem::replace(&mut t.seq, prev));
            let _ = t.save_seq();
            let n = t.seq.len();
            if let View::Frame(i) = self.view {
                if i >= n {
                    self.view = if n == 0 { View::Live } else { View::Frame(n - 1) };
                }
            }
            if let Some(c) = self.cursor {
                if c >= n {
                    self.cursor = n.checked_sub(1);
                }
            }
            self.set_status(if redo { "Redo" } else { "Undo" });
        }
    }

    fn delete_current(&mut self) {
        let n = self.seq_len();
        if n == 0 {
            return;
        }
        let idx = match self.view {
            View::Frame(i) => i,
            View::Live => match self.insert_pos().checked_sub(1) {
                Some(i) => i,
                None => return,
            },
        };
        if idx >= n {
            return;
        }
        self.edit_seq(|s| {
            s.remove(idx);
        });
        let n = n - 1;
        if let View::Frame(_) = self.view {
            self.view = if n == 0 { View::Live } else { View::Frame(idx.min(n - 1)) };
        }
        self.cursor = if n == 0 { None } else { Some(idx.saturating_sub(if self.view == View::Live { 1 } else { 0 }).min(n - 1)) };
        self.set_status(format!("Deleted frame {}", idx + 1));
    }

    fn duplicate_current(&mut self) {
        if let View::Frame(i) = self.view {
            self.edit_seq(|s| {
                let id = s[i].clone();
                s.insert(i + 1, id);
            });
            self.view = View::Frame(i + 1);
            self.cursor = Some(i + 1);
        }
    }

    fn move_current(&mut self, right: bool) {
        if let View::Frame(i) = self.view {
            let n = self.seq_len();
            let j = if right { i + 1 } else { i.wrapping_sub(1) };
            if j < n {
                self.edit_seq(|s| s.swap(i, j));
                self.view = View::Frame(j);
                self.cursor = Some(j);
            }
        }
    }

    fn step(&mut self, delta: isize) {
        let n = self.seq_len();
        if n == 0 {
            return;
        }
        self.play = None;
        self.view = match self.view {
            View::Live => {
                if delta < 0 {
                    View::Frame(self.insert_pos().saturating_sub(1).min(n - 1))
                } else {
                    View::Live
                }
            }
            View::Frame(i) => {
                let j = i as isize + delta;
                if j < 0 {
                    View::Frame(0)
                } else if j as usize >= n {
                    View::Live
                } else {
                    View::Frame(j as usize)
                }
            }
        };
        if let View::Frame(i) = self.view {
            self.cursor = Some(i);
        }
        self.scroll_to_current = true;
    }

    fn toggle_flip(&mut self) {
        self.play = None;
        self.view = match self.view {
            View::Live => match self.insert_pos().checked_sub(1) {
                Some(i) if i < self.seq_len() => View::Frame(i),
                _ => View::Live,
            },
            View::Frame(_) => View::Live,
        };
    }

    fn capture(&mut self) {
        if self.take.is_none() {
            self.set_status("Open a project first");
            return;
        }
        if self.cam_open.is_none() {
            self.set_status("No camera open");
            return;
        }
        if self.capture_pending {
            return;
        }
        self.play = None;
        self.view = View::Live;
        self.capture_pending = true;
        self.cam.send(Cmd::Capture { avg: self.avg_frames });
    }

    fn on_captured(&mut self, img: RgbImage) {
        self.capture_pending = false;
        let pos = self.insert_pos();
        let Some(t) = self.take.as_mut() else { return };
        let id = t.alloc_id();
        let path = t.frame_path(&id);
        let _ = t.save_meta(); // persist next_id
        let full = Arc::new(img);
        let preview = Arc::new(box_downscale(&full, PREVIEW_MAX));
        self.store.insert(path.clone(), Loaded { full_w: full.w, full_h: full.h, preview: preview.clone() });
        self.store.save_async(path, full.clone(), preview);
        self.edit_seq(|s| s.insert(pos.min(s.len()), id));
        if self.insert_mode {
            self.cursor = Some(pos);
        }
        self.scroll_to_current = true;
        self.set_status(format!("Captured frame {} ({} x {})", pos + 1, full.w, full.h));
    }

    fn toggle_play(&mut self) {
        if self.play.is_some() {
            self.play = None;
            return;
        }
        let n = self.seq_len();
        if n == 0 {
            return;
        }
        let from = match self.view {
            View::Frame(i) if i + 1 < n => i,
            _ => 0,
        };
        self.play = Some(Play { start: Instant::now(), from, last_idx: from });
        self.view = View::Frame(from);
    }

    // ---------------------------------------------------------------- correction

    fn corrected(&self) -> bool {
        !self.editing_lines && self.take.as_ref().map(|t| t.undistort.active()).unwrap_or(false)
    }

    fn get_lut(&mut self, w: usize, h: usize) -> Option<(Arc<Lut>, Arc<Solved>)> {
        let t = self.take.as_ref()?;
        if let Some(e) = self.luts.iter().find(|e| e.rev == self.rev && e.w == w && e.h == h) {
            return Some((e.lut.clone(), e.solved.clone()));
        }
        let solved = Arc::new(undistort::solve(&t.undistort, w, h));
        let lut = Arc::new(undistort::build_lut(&solved, w, h, PREVIEW_MAX));
        self.luts.retain(|e| e.rev == self.rev);
        self.luts.push(LutEntry { rev: self.rev, w, h, lut: lut.clone(), solved: solved.clone() });
        if self.luts.len() > 4 {
            self.luts.remove(0);
        }
        Some((lut, solved))
    }

    fn prepare(&mut self, img: &RgbImage, full_w: usize, full_h: usize, corrected: bool) -> RgbImage {
        if corrected {
            if let Some((lut, solved)) = self.get_lut(full_w, full_h) {
                if !solved.is_identity() {
                    return undistort::apply_lut(img, &lut);
                }
            }
        }
        img.clone()
    }

    fn current_solved_status(&mut self) -> Option<String> {
        let dims = match self.view {
            View::Live => self.live_dims,
            View::Frame(_) => self.frame_full.or(self.live_dims),
        }?;
        if let Some((rev, d, s)) = &self.status_cache {
            if *rev == self.rev && *d == dims {
                return Some(s.clone());
            }
        }
        let t = self.take.as_ref()?;
        let s = undistort::solve(&t.undistort, dims.0, dims.1).status;
        self.status_cache = Some((self.rev, dims, s.clone()));
        Some(s)
    }

    // ---------------------------------------------------------------- events

    fn poll_events(&mut self) {
        while let Ok(ev) = self.cam.rx.try_recv() {
            match ev {
                Ev::List(list) => {
                    self.cameras = list;
                    if self.cam_sel >= self.cameras.len() {
                        self.cam_sel = 0;
                    }
                    if !self.auto_open_tried && !self.cameras.is_empty() {
                        self.auto_open_tried = true;
                        let pick = self
                            .cameras
                            .iter()
                            .position(|c| c.name == self.config.last_camera)
                            .unwrap_or(0);
                        self.cam_sel = pick;
                        self.open_camera(None);
                    }
                    if self.cameras.is_empty() {
                        self.set_status("No cameras found");
                    }
                }
                Ev::Opened { name, format, formats } => {
                    self.set_status(format!("Camera: {name}  {}", camera::format_label(&format)));
                    self.config.last_camera = name.clone();
                    self.config.save();
                    self.cam_open = Some((name, format));
                    self.cam_formats = formats;
                    self.live_dims = Some((format.resolution().width() as usize, format.resolution().height() as usize));
                }
                Ev::Controls(c) => self.controls = c,
                Ev::Closed => {
                    self.cam_open = None;
                    self.controls.clear();
                    self.live_tex = None;
                    self.capture_pending = false;
                }
                Ev::Error(e) => {
                    if e.starts_with("Capture") || e.starts_with("No camera") {
                        self.capture_pending = false;
                    }
                    self.set_status(e);
                }
                Ev::Captured(img) => self.on_captured(img),
            }
        }
        self.store.poll();
        let errs: Vec<String> = self.store.save_errors.lock().unwrap().drain(..).collect();
        if let Some(e) = errs.last() {
            self.set_status(format!("Save error: {e}"));
        }
        // thumbnails for anything freshly loaded
        for path in self.store.take_newly_loaded() {
            if self.thumbs.contains_key(&path) {
                continue;
            }
            if let Some(l) = self.store.get(&path, false) {
                let th = box_downscale(&l.preview, THUMB_MAX);
                let tex = self.cam_ctx().load_texture("thumb", th.to_color_image(), TextureOptions::LINEAR);
                self.thumbs.insert(path, tex);
            }
        }
        if let Some(job) = &self.export {
            if job.finished.load(Ordering::SeqCst) {
                let msg = match job.error.lock().unwrap().take() {
                    Some(e) => format!("Export failed: {e}"),
                    None => format!("Exported {} frames to {}", job.total, job.dir.display()),
                };
                self.export = None;
                self.set_status(msg);
            }
        }
    }

    fn cam_ctx(&self) -> egui::Context {
        self.ctx.clone()
    }

    fn open_camera(&mut self, format: Option<CameraFormat>) {
        if let Some(c) = self.cameras.get(self.cam_sel).cloned() {
            self.set_status(format!("Opening {}...", c.name));
            self.cam.send(Cmd::Open { index: c.index, format });
        }
    }

    // ---------------------------------------------------------------- input

    fn handle_keys(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        // consume shortcut keys so focused buttons don't also react to them
        let (space, enter, left, right, home, end, del, undo, redo, live, onion, flip, esc) = ctx.input_mut(|i| {
            use egui::Modifiers as M;
            // redo first: Ctrl+Z also matches Ctrl+Shift+Z logically
            let redo = i.consume_key(M::COMMAND | M::SHIFT, Key::Z) || i.consume_key(M::COMMAND, Key::Y);
            let undo = i.consume_key(M::COMMAND, Key::Z);
            (
                i.consume_key(M::NONE, Key::Space),
                i.consume_key(M::NONE, Key::Enter),
                i.consume_key(M::NONE, Key::ArrowLeft),
                i.consume_key(M::NONE, Key::ArrowRight),
                i.consume_key(M::NONE, Key::Home),
                i.consume_key(M::NONE, Key::End),
                i.consume_key(M::NONE, Key::Delete) || i.consume_key(M::NONE, Key::Backspace),
                undo,
                redo,
                i.consume_key(M::NONE, Key::L),
                i.consume_key(M::NONE, Key::O),
                i.consume_key(M::NONE, Key::T),
                i.consume_key(M::NONE, Key::Escape),
            )
        });
        if space {
            self.capture();
        }
        if enter {
            self.toggle_play();
        }
        if left {
            self.step(-1);
        }
        if right {
            self.step(1);
        }
        if home && self.seq_len() > 0 {
            self.play = None;
            self.view = View::Frame(0);
            self.cursor = Some(0);
            self.scroll_to_current = true;
        }
        if end {
            self.play = None;
            self.view = View::Live;
            self.scroll_to_current = true;
        }
        if del {
            self.delete_current();
        }
        if undo {
            self.undo_redo(false);
        }
        if redo {
            self.undo_redo(true);
        }
        if live {
            self.play = None;
            self.view = View::Live;
        }
        if onion {
            self.onion.enabled = !self.onion.enabled;
        }
        if flip {
            self.toggle_flip();
        }
        if esc {
            self.add_armed = false;
            self.play = None;
        }
    }
}

// ==================================================================== UI

impl App {
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("top").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("TurtleFrame");
                ui.separator();
                if ui.button("Projects...").clicked() {
                    let start = self
                        .project
                        .as_ref()
                        .and_then(|p| p.root.parent().map(|x| x.to_path_buf()))
                        .or_else(|| self.config.last_browse.clone())
                        .unwrap_or_else(home_dir);
                    self.browser.open_at(start);
                }
                if let (Some(p), Some(t)) = (&self.project, &self.take) {
                    ui.label(format!("{}  |  {}", p.name(), t.name));
                    ui.separator();
                    let pos = match self.view {
                        View::Live => format!("LIVE (next frame {})", self.insert_pos() + 1),
                        View::Frame(i) => format!("Frame {} / {}", i + 1, t.seq.len()),
                    };
                    ui.label(pos);
                    ui.label(format!("{} frames", t.seq.len()));
                } else {
                    ui.label("No project open");
                }
                ui.separator();
                match &self.cam_open {
                    Some((n, f)) => ui.label(format!("{n}  {}", camera::format_label(f))),
                    None => ui.label("No camera"),
                };
                let saving = self.store.saving.load(Ordering::SeqCst);
                if saving > 0 {
                    ui.separator();
                    ui.label(format!("saving {saving}..."));
                }
                if self.capture_pending {
                    ui.separator();
                    ui.colored_label(Color32::YELLOW, "capturing...");
                }
            });
            // fixed height status row so the layout never jumps
            let msg = if self.status_at.elapsed() < Duration::from_secs(8) { self.status.as_str() } else { "" };
            ui.label(if msg.is_empty() { " " } else { msg });
        });
    }

    fn left_panel(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("left").resizable(true).default_size(320.0).show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                self.ui_project(ui);
                self.ui_camera(ui);
                self.ui_undistort(ui);
                self.ui_onion(ui);
                self.ui_playback(ui);
                self.ui_export(ui);
                egui::CollapsingHeader::new("Keys").default_open(false).show(ui, |ui| {
                    ui.label(
                        "Space  capture\nEnter  play / stop\nLeft / Right  step\nHome / End  first frame / live\n\
                         Delete  remove frame (in live: last frame)\nT  flip live / last frame\nL  live\n\
                         O  onion on / off\nCtrl+Z / Ctrl+Shift+Z  undo / redo\nEsc  cancel",
                    );
                });
            });
        });
    }

    fn ui_project(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Project and takes").default_open(true).show(ui, |ui| {
            let Some(p) = &self.project else {
                ui.label("Open or create a project folder to start.");
                if ui.button("Open / create project folder...").clicked() {
                    let start = self.config.last_browse.clone().unwrap_or_else(home_dir);
                    self.browser.open_at(start);
                }
                return;
            };
            ui.label(p.root.display().to_string());
            let takes = p.takes.clone();
            let current = p.current_take.clone();
            let mut switch = None;
            for (dir, name) in &takes {
                if ui.selectable_label(*dir == current, format!("{name}   ({dir})")).clicked() && *dir != current {
                    switch = Some(dir.clone());
                }
            }
            if let Some(d) = switch {
                self.switch_take(&d);
            }
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.new_take_name).hint_text("new take name").desired_width(160.0));
                if ui.button("New take").clicked() {
                    self.save_meta_now();
                    let name = self.new_take_name.replace(['\n', '\r'], " ");
                    let tpl = self.take.clone();
                    if let Some(p) = self.project.as_mut() {
                        match p.create_take(&name, tpl.as_ref()) {
                            Ok(t) => {
                                p.current_take = t.dir_name();
                                let _ = p.save();
                                self.take = Some(t);
                                self.new_take_name.clear();
                                self.reset_take_state();
                                self.set_status("New take (copied fps and undistort from previous)");
                            }
                            Err(e) => self.set_status(format!("Could not create take: {e}")),
                        }
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.rename_buf).desired_width(160.0));
                if ui.button("Rename take").clicked() && !self.rename_buf.trim().is_empty() {
                    if let Some(t) = self.take.as_mut() {
                        t.name = self.rename_buf.trim().replace(['\n', '\r'], " ");
                        let _ = t.save_meta();
                    }
                    if let Some(p) = self.project.as_mut() {
                        p.scan_takes();
                    }
                }
            });
            ui.horizontal(|ui| {
                if ui.button("Purge unused frames").on_hover_text("Moves frame files no longer in the sequence into the take's trash folder").clicked() {
                    if let Some(t) = &self.take {
                        match t.purge_unused() {
                            Ok(n) => {
                                self.undo.clear();
                                self.redo.clear();
                                self.set_status(format!("Moved {n} unused frames to trash (undo history cleared)"));
                            }
                            Err(e) => self.set_status(format!("Purge failed: {e}")),
                        }
                    }
                }
            });
        });
    }

    fn ui_camera(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Camera").default_open(true).show(ui, |ui| {
            ui.horizontal(|ui| {
                let sel_name = self.cameras.get(self.cam_sel).map(|c| c.name.clone()).unwrap_or_else(|| "(none)".into());
                egui::ComboBox::from_id_salt("cam_combo").width(200.0).selected_text(sel_name).show_ui(ui, |ui| {
                    for (i, c) in self.cameras.iter().enumerate() {
                        ui.selectable_value(&mut self.cam_sel, i, &c.name);
                    }
                });
                if ui.button("Refresh").clicked() {
                    self.cam.send(Cmd::List);
                }
            });
            ui.horizontal(|ui| {
                if ui.button(if self.cam_open.is_some() { "Reopen" } else { "Open" }).clicked() {
                    self.open_camera(None);
                }
                if self.cam_open.is_some() && ui.button("Close").clicked() {
                    self.cam.send(Cmd::Close);
                }
            });
            if let Some((_, cur)) = self.cam_open {
                let mut chosen = None;
                egui::ComboBox::from_id_salt("fmt_combo")
                    .width(260.0)
                    .selected_text(camera::format_label(&cur))
                    .show_ui(ui, |ui| {
                        for f in &self.cam_formats {
                            if ui.selectable_label(*f == cur, camera::format_label(f)).clicked() {
                                chosen = Some(*f);
                            }
                        }
                    });
                if let Some(f) = chosen {
                    self.open_camera(Some(f));
                }
            }
            ui.add(egui::Slider::new(&mut self.avg_frames, 1..=8).text("frames averaged per capture"))
                .on_hover_text("Averages several frames into one still to reduce sensor noise");

            egui::CollapsingHeader::new("Camera controls").default_open(false).show(ui, |ui| {
                if self.cam_open.is_none() {
                    ui.label("Open a camera first.");
                    return;
                }
                if self.controls.is_empty() {
                    ui.label("This camera/driver exposes no controls.");
                }
                ui.label("Untouched controls stay on the camera's automatic defaults.");
                let mut sends: Vec<(nokhwa::utils::KnownCameraControl, ControlValueSetter)> = Vec::new();
                for c in &self.controls {
                    let key = camera::control_key(&c.id);
                    ui.add_enabled_ui(!c.read_only, |ui| match &c.desc {
                        ControlValueDescription::IntegerRange { min, max, value, step, .. } => {
                            let mut v = self.ctrl_local.get(&key).copied().unwrap_or(*value as f64);
                            let r = ui.add(
                                egui::Slider::new(&mut v, *min as f64..=*max as f64)
                                    .step_by((*step).max(1) as f64)
                                    .text(&c.name),
                            );
                            if r.dragged() {
                                self.ctrl_local.insert(key.clone(), v);
                            }
                            if r.drag_stopped() || (r.changed() && !r.dragged()) {
                                self.ctrl_local.remove(&key);
                                sends.push((c.id, ControlValueSetter::Integer(v.round() as i64)));
                            }
                        }
                        ControlValueDescription::Integer { value, .. } => {
                            let mut v = self.ctrl_local.get(&key).copied().unwrap_or(*value as f64);
                            ui.horizontal(|ui| {
                                let r = ui.add(egui::DragValue::new(&mut v).speed(1.0));
                                ui.label(&c.name);
                                if r.dragged() {
                                    self.ctrl_local.insert(key.clone(), v);
                                }
                                if r.drag_stopped() || (r.changed() && !r.dragged()) {
                                    self.ctrl_local.remove(&key);
                                    sends.push((c.id, ControlValueSetter::Integer(v.round() as i64)));
                                }
                            });
                        }
                        ControlValueDescription::FloatRange { min, max, value, .. } => {
                            let mut v = self.ctrl_local.get(&key).copied().unwrap_or(*value);
                            let r = ui.add(egui::Slider::new(&mut v, *min..=*max).text(&c.name));
                            if r.dragged() {
                                self.ctrl_local.insert(key.clone(), v);
                            }
                            if r.drag_stopped() || (r.changed() && !r.dragged()) {
                                self.ctrl_local.remove(&key);
                                sends.push((c.id, ControlValueSetter::Float(v)));
                            }
                        }
                        ControlValueDescription::Float { value, .. } => {
                            let mut v = self.ctrl_local.get(&key).copied().unwrap_or(*value);
                            ui.horizontal(|ui| {
                                let r = ui.add(egui::DragValue::new(&mut v).speed(0.01));
                                ui.label(&c.name);
                                if r.dragged() {
                                    self.ctrl_local.insert(key.clone(), v);
                                }
                                if r.drag_stopped() || (r.changed() && !r.dragged()) {
                                    self.ctrl_local.remove(&key);
                                    sends.push((c.id, ControlValueSetter::Float(v)));
                                }
                            });
                        }
                        ControlValueDescription::Boolean { value, .. } => {
                            let mut v = *value;
                            if ui.checkbox(&mut v, &c.name).changed() {
                                sends.push((c.id, ControlValueSetter::Boolean(v)));
                            }
                        }
                        ControlValueDescription::Enum { value, possible, .. } => {
                            let mut v = *value;
                            ui.horizontal(|ui| {
                                egui::ComboBox::from_id_salt(format!("enum_{key}")).selected_text(v.to_string()).show_ui(ui, |ui| {
                                    for p in possible {
                                        ui.selectable_value(&mut v, *p, p.to_string());
                                    }
                                });
                                ui.label(&c.name);
                            });
                            if v != *value {
                                sends.push((c.id, ControlValueSetter::Integer(v)));
                            }
                        }
                        _ => {}
                    });
                }
                for (id, v) in sends {
                    self.cam.send(Cmd::SetControl(id, v));
                }
                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Reset all to defaults").clicked() {
                        let list = self
                            .controls
                            .iter()
                            .filter(|c| !c.read_only)
                            .filter_map(|c| camera::default_value(&c.desc).map(|v| (c.id, v)))
                            .collect();
                        self.cam.send(Cmd::SetControls(list));
                    }
                    if ui.button("Save to project").clicked() {
                        let list: Vec<(String, String)> = self
                            .controls
                            .iter()
                            .filter(|c| !c.read_only)
                            .filter_map(|c| {
                                let v = camera::current_value(&c.desc)?;
                                Some((camera::control_key(&c.id), camera::value_to_str(&v)?))
                            })
                            .collect();
                        if let Some(p) = self.project.as_mut() {
                            p.saved_controls = list;
                            match p.save() {
                                Ok(_) => self.status = "Camera controls saved to project".into(),
                                Err(e) => self.status = format!("Save failed: {e}"),
                            }
                            self.status_at = Instant::now();
                        }
                    }
                    let has_saved = self.project.as_ref().map(|p| !p.saved_controls.is_empty()).unwrap_or(false);
                    if ui.add_enabled(has_saved, egui::Button::new("Apply saved")).clicked() {
                        if let Some(p) = &self.project {
                            let list = p
                                .saved_controls
                                .iter()
                                .filter_map(|(k, v)| Some((camera::control_from_key(k)?, camera::value_from_str(v)?)))
                                .collect();
                            self.cam.send(Cmd::SetControls(list));
                        }
                    }
                });
            });
        });
    }

    fn ui_undistort(&mut self, ui: &mut egui::Ui) {
        let status = self.current_solved_status();
        egui::CollapsingHeader::new("Undistort").default_open(true).show(ui, |ui| {
            let editing = self.editing_lines;
            let Some(t) = self.take.as_mut() else {
                ui.label("Open a project first.");
                return;
            };
            let u = &mut t.undistort;
            ui.horizontal(|ui| {
                if ui.selectable_label(editing, "Edit lines").clicked() {
                    self.editing_lines = !editing;
                    self.add_armed = false;
                }
                let can_add = u.lines.len() < MAX_LINES;
                if ui.add_enabled(can_add, egui::Button::selectable(self.add_armed, "Add line")).clicked() {
                    self.add_armed = !self.add_armed;
                    self.editing_lines = true;
                }
                if ui.add_enabled(!u.lines.is_empty(), egui::Button::new("Clear")).clicked() {
                    u.lines.clear();
                }
            });
            if self.editing_lines {
                ui.label(if self.add_armed {
                    "Drag along a paper edge, corner to corner. Click Add line again (or Esc) to adjust handles."
                } else {
                    "Drag handles onto the paper edge. Pull the middle handle onto the bowed edge. Right-click a handle to remove its line."
                });
            }
            ui.label(format!("{} / {MAX_LINES} lines", u.lines.len()));
            ui.checkbox(&mut u.lens, "Lens: straighten bowed edges");
            ui.add_enabled(u.lens, egui::Slider::new(&mut u.k_adjust, -0.3..=0.3).text("fine tune"));
            ui.checkbox(&mut u.flatten, "Flatten paper (needs 4 lines)");
            ui.add_enabled_ui(u.flatten, |ui| {
                let mut a = u.aspect;
                egui::ComboBox::from_id_salt("aspect").width(200.0).selected_text(a.label()).show_ui(ui, |ui| {
                    for opt in [Aspect::Auto, Aspect::Iso, Aspect::Letter, Aspect::Wide16x9, Aspect::Tv4x3] {
                        ui.selectable_value(&mut a, opt, opt.label());
                    }
                    let custom = if let Aspect::Custom(r) = a { r } else { 1.5 };
                    ui.selectable_value(&mut a, Aspect::Custom(custom), "Custom");
                });
                if let Aspect::Custom(mut r) = a {
                    ui.add(egui::Slider::new(&mut r, 1.0..=3.0).text("long / short"));
                    a = Aspect::Custom(r);
                }
                u.aspect = a;
            });
            if let Some(s) = status {
                ui.label(egui::RichText::new(s).small());
            }
            if editing && u.active() {
                ui.label(egui::RichText::new("Correction is hidden while editing lines.").small());
            }
        });
    }

    fn ui_onion(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Onion skin").default_open(true).show(ui, |ui| {
            let o = &mut self.onion;
            ui.checkbox(&mut o.enabled, "Show onion skin (O)");
            ui.horizontal(|ui| {
                ui.selectable_value(&mut o.mode, OnionMode::Lines, "Pencil lines");
                ui.selectable_value(&mut o.mode, OnionMode::Edges, "Edges");
                ui.selectable_value(&mut o.mode, OnionMode::Image, "Image");
            });
            ui.add(egui::Slider::new(&mut o.back, 0..=5).text("back (red)"));
            ui.add(egui::Slider::new(&mut o.fwd, 0..=5).text("forward (green)"));
            ui.add(egui::Slider::new(&mut o.opacity, 0.05..=1.0).text("opacity"));
            match o.mode {
                OnionMode::Lines => {
                    ui.add(egui::Slider::new(&mut o.threshold, 0.3..=0.95).text("line darkness"));
                }
                OnionMode::Edges => {
                    ui.add(egui::Slider::new(&mut o.threshold, 0.0..=1.0).text("edge sensitivity"));
                }
                OnionMode::Image => {}
            }
            ui.checkbox(&mut self.insert_mode, "Insert after selected frame")
                .on_hover_text("Captures go after the last selected frame instead of the end. Green shows the frames after the insert point.");
        });
    }

    fn ui_playback(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Playback").default_open(true).show(ui, |ui| {
            let Some(t) = self.take.as_mut() else { return };
            ui.add(egui::Slider::new(&mut t.fps, 1..=60).text("fps"));
            ui.horizontal(|ui| {
                ui.label("Shoot on");
                for h in 1..=3u32 {
                    ui.selectable_value(&mut t.hold, h, match h {
                        1 => "ones",
                        2 => "twos",
                        _ => "threes",
                    });
                }
            });
            ui.checkbox(&mut self.loop_play, "Loop");
        });
    }

    fn ui_export(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Export").default_open(false).show(ui, |ui| {
            ui.checkbox(&mut self.export_holds, "Repeat frames for twos / threes");
            if let Some(job) = &self.export {
                let d = job.done.load(Ordering::SeqCst);
                ui.add(egui::ProgressBar::new(d as f32 / job.total.max(1) as f32).text(format!("{d} / {}", job.total)));
                return;
            }
            let ready = self.take.as_ref().map(|t| !t.seq.is_empty()).unwrap_or(false);
            if ui.add_enabled(ready, egui::Button::new("Export corrected PNG sequence")).clicked() {
                self.start_export();
            }
            if let (Some(p), Some(t)) = (&self.project, &self.take) {
                ui.label(egui::RichText::new(p.export_dir(t).display().to_string()).small());
            }
        });
    }

    fn start_export(&mut self) {
        let (Some(p), Some(t)) = (&self.project, &self.take) else { return };
        if self.store.saving.load(Ordering::SeqCst) > 0 {
            self.set_status("Wait for frames to finish saving");
            return;
        }
        let dir = p.export_dir(t);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.set_status(format!("Export failed: {e}"));
            return;
        }
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                if e.path().extension().and_then(|x| x.to_str()) == Some("png") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        let paths: Vec<PathBuf> = t.seq.iter().map(|id| t.frame_path(id)).collect();
        let settings = t.undistort.clone();
        let holds = if self.export_holds { t.hold as usize } else { 1 };
        let prefix = t.dir_name();
        let done = Arc::new(AtomicUsize::new(0));
        let finished = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        let job = ExportJob { done: done.clone(), total: paths.len(), finished: finished.clone(), error: error.clone(), dir: dir.clone() };
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let mut out_i = 1;
            for p in paths {
                let r = imageops::load_rgb(&p).and_then(|img| {
                    let out = undistort::render_full(&img, &settings);
                    for _ in 0..holds {
                        imageops::save_png(&dir.join(format!("{prefix}_{out_i:04}.png")), &out)?;
                        out_i += 1;
                    }
                    Ok(())
                });
                if let Err(e) = r {
                    *error.lock().unwrap() = Some(e);
                    break;
                }
                done.fetch_add(1, Ordering::SeqCst);
                ctx.request_repaint();
            }
            finished.store(true, Ordering::SeqCst);
            ctx.request_repaint();
        });
        self.export = Some(job);
    }

    // ---------------------------------------------------------------- timeline

    fn timeline(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("timeline").resizable(false).exact_size(150.0).show(ui, |ui| {
            ui.horizontal(|ui| {
                let has = self.seq_len() > 0;
                if ui.add_enabled(has, egui::Button::new("|<")).on_hover_text("First frame (Home)").clicked() {
                    self.play = None;
                    self.view = View::Frame(0);
                    self.cursor = Some(0);
                    self.scroll_to_current = true;
                }
                if ui.add_enabled(has, egui::Button::new("<")).on_hover_text("Step back (Left)").clicked() {
                    self.step(-1);
                }
                let play_label = if self.play.is_some() { "Stop" } else { "Play" };
                if ui.add_enabled(has, egui::Button::new(play_label)).on_hover_text("Enter").clicked() {
                    self.toggle_play();
                }
                if ui.add_enabled(has, egui::Button::new(">")).on_hover_text("Step forward (Right)").clicked() {
                    self.step(1);
                }
                if ui.button("Live").on_hover_text("L / End").clicked() {
                    self.play = None;
                    self.view = View::Live;
                    self.scroll_to_current = true;
                }
                ui.separator();
                let can_cap = self.cam_open.is_some() && self.take.is_some() && !self.capture_pending;
                if ui
                    .add_enabled(can_cap, egui::Button::new(egui::RichText::new("  CAPTURE  ").strong()).fill(Color32::from_rgb(150, 30, 30)))
                    .on_hover_text("Space")
                    .clicked()
                {
                    self.capture();
                }
                ui.separator();
                let on_frame = matches!(self.view, View::Frame(_));
                if ui.add_enabled(has, egui::Button::new("Delete")).on_hover_text("Delete / Backspace").clicked() {
                    self.delete_current();
                }
                if ui.add_enabled(on_frame, egui::Button::new("Hold +1")).on_hover_text("Repeat this frame").clicked() {
                    self.duplicate_current();
                }
                if ui.add_enabled(on_frame, egui::Button::new("Move <")).clicked() {
                    self.move_current(false);
                }
                if ui.add_enabled(on_frame, egui::Button::new("Move >")).clicked() {
                    self.move_current(true);
                }
                ui.separator();
                if ui.add_enabled(!self.undo.is_empty(), egui::Button::new("Undo")).clicked() {
                    self.undo_redo(false);
                }
                if ui.add_enabled(!self.redo.is_empty(), egui::Button::new("Redo")).clicked() {
                    self.undo_redo(true);
                }
            });
            self.thumb_strip(ui);
        });
    }

    fn thumb_strip(&mut self, ui: &mut egui::Ui) {
        let Some(t) = self.take.as_ref() else { return };
        let ids: Vec<String> = t.seq.clone();
        let paths: Vec<PathBuf> = ids.iter().map(|id| t.frame_path(id)).collect();
        let n = paths.len();
        let th = 90.0f32;
        let aspect = self.live_dims.map(|(w, h)| w as f32 / h as f32).unwrap_or(16.0 / 9.0);
        let tw = th * aspect;
        let gap = 4.0;
        let slot = tw + gap;
        let total = (n + 1) as f32 * slot;
        let current = match self.view {
            View::Live => n,
            View::Frame(i) => i,
        };
        let insert = if self.insert_mode { Some(self.insert_pos()) } else { None };
        let mut clicked: Option<View> = None;
        let scroll_to = std::mem::take(&mut self.scroll_to_current);

        egui::ScrollArea::horizontal().auto_shrink([false, false]).show_viewport(ui, |ui, viewport| {
            ui.set_min_size(vec2(total, th + 18.0));
            let origin = ui.min_rect().min;
            let first = ((viewport.min.x / slot).floor().max(0.0)) as usize;
            let last = (((viewport.max.x / slot).ceil()) as usize + 1).min(n + 1);
            for i in first..last {
                let r = Rect::from_min_size(origin + vec2(i as f32 * slot, 0.0), vec2(tw, th));
                let resp = ui.interact(r, ui.scope_id().with(("tf_thumb", i)), Sense::click());
                let painter = ui.painter();
                if i < n {
                    let path = &paths[i];
                    if !self.thumbs.contains_key(path) {
                        if let Some(l) = self.store.get(path, false) {
                            let small = box_downscale(&l.preview, THUMB_MAX);
                            let tex = self.ctx.load_texture("thumb", small.to_color_image(), TextureOptions::LINEAR);
                            self.thumbs.insert(path.clone(), tex);
                        }
                    }
                    match self.thumbs.get(path) {
                        Some(tex) => {
                            painter.image(tex.id(), r, uv_full(), Color32::WHITE);
                        }
                        None => {
                            let msg = if self.store.failed(path).is_some() { "missing" } else { "..." };
                            painter.rect_filled(r, 2.0, Color32::from_gray(40));
                            painter.text(r.center(), egui::Align2::CENTER_CENTER, msg, egui::FontId::proportional(12.0), Color32::GRAY);
                        }
                    }
                    painter.text(
                        r.left_bottom() + vec2(2.0, 2.0),
                        egui::Align2::LEFT_TOP,
                        format!("{}", i + 1),
                        egui::FontId::monospace(11.0),
                        Color32::LIGHT_GRAY,
                    );
                    if resp.clicked() {
                        clicked = Some(View::Frame(i));
                    }
                } else {
                    painter.rect_filled(r, 2.0, Color32::from_rgb(60, 20, 20));
                    painter.text(r.center(), egui::Align2::CENTER_CENTER, "LIVE", egui::FontId::proportional(16.0), Color32::WHITE);
                    if resp.clicked() {
                        clicked = Some(View::Live);
                    }
                }
                if i == current {
                    painter.rect_stroke(r.expand(1.5), 2.0, Stroke::new(2.5, Color32::from_rgb(255, 200, 0)), egui::StrokeKind::Outside);
                }
                if let Some(ip) = insert {
                    if ip == i {
                        let x = r.left() - gap / 2.0;
                        painter.line_segment([pos2(x, r.top() - 2.0), pos2(x, r.bottom() + 2.0)], Stroke::new(3.0, Color32::from_rgb(255, 140, 0)));
                    }
                }
            }
            if scroll_to {
                let r = Rect::from_min_size(origin + vec2(current as f32 * slot, 0.0), vec2(tw, th));
                ui.scroll_to_rect(r, None);
            }
        });
        if let Some(v) = clicked {
            self.play = None;
            self.view = v;
            if let View::Frame(i) = v {
                self.cursor = Some(i);
            }
        }
    }

    // ---------------------------------------------------------------- viewer

    /// Upload the image for the current view; returns (texture, full res dims).
    fn view_texture(&mut self, corrected: bool) -> Option<(egui::TextureId, [usize; 2], (usize, usize))> {
        match self.view {
            View::Live => {
                let lf = {
                    let g = self.cam.live.lock().unwrap();
                    g.as_ref().map(|l| (l.seq, l.preview.clone(), l.full_w, l.full_h))
                };
                let (seq, prev, fw, fh) = lf?;
                self.live_dims = Some((fw, fh));
                let key = (seq, self.rev, corrected);
                if self.live_tex.is_none() || self.live_key != key {
                    let img = self.prepare(&prev, fw, fh, corrected);
                    match &mut self.live_tex {
                        Some(t) if t.size() == [img.w, img.h] => t.set(img.to_color_image(), TextureOptions::LINEAR),
                        _ => self.live_tex = Some(self.ctx.load_texture("live", img.to_color_image(), TextureOptions::LINEAR)),
                    }
                    self.live_key = key;
                }
                let t = self.live_tex.as_ref()?;
                Some((t.id(), t.size(), (fw, fh)))
            }
            View::Frame(i) => {
                let path = self.frame_path(i)?;
                if let Some(l) = self.store.get(&path, true) {
                    let key = (path.clone(), self.rev, corrected);
                    if self.frame_key.as_ref() != Some(&key) || self.frame_tex.is_none() {
                        let img = self.prepare(&l.preview, l.full_w, l.full_h, corrected);
                        match &mut self.frame_tex {
                            Some(t) if t.size() == [img.w, img.h] => t.set(img.to_color_image(), TextureOptions::LINEAR),
                            _ => self.frame_tex = Some(self.ctx.load_texture("frame", img.to_color_image(), TextureOptions::LINEAR)),
                        }
                        self.frame_key = Some(key);
                        self.frame_full = Some((l.full_w, l.full_h));
                    }
                }
                // while loading, keep showing whatever was there
                let t = self.frame_tex.as_ref()?;
                Some((t.id(), t.size(), self.frame_full.unwrap_or((t.size()[0], t.size()[1]))))
            }
        }
    }

    fn onion_textures(&mut self, corrected: bool) -> Vec<(egui::TextureId, Color32)> {
        if !self.onion.enabled || self.play.is_some() {
            return Vec::new();
        }
        let n = self.seq_len();
        // anchor: frames before it are "back", from it onward are "forward"
        let (back_from, fwd_from) = match self.view {
            View::Live => {
                let p = self.insert_pos();
                (p, p)
            }
            View::Frame(i) => (i, i + 1),
        };
        let mut key_h = std::collections::hash_map::DefaultHasher::new();
        (self.onion.mode, self.onion.threshold.to_bits(), self.rev, corrected).hash(&mut key_h);
        let key = key_h.finish();
        if key != self.onion_key {
            self.onion_cache.clear();
            self.onion_key = key;
        }
        let mut wanted: Vec<(usize, Color32)> = Vec::new();
        for k in 1..=self.onion.back {
            if let Some(idx) = back_from.checked_sub(k) {
                let a = self.onion.opacity * 0.65f32.powi(k as i32 - 1);
                wanted.push((idx, tint(BACK_TINT, a)));
            }
        }
        for k in 0..self.onion.fwd {
            let idx = fwd_from + k;
            if idx < n {
                let a = self.onion.opacity * 0.65f32.powi(k as i32);
                wanted.push((idx, tint(FWD_TINT, a)));
            }
        }
        // farthest first so the nearest ends on top
        wanted.reverse();
        let mut out = Vec::new();
        let mut keep = Vec::new();
        for (idx, col) in wanted {
            let Some(path) = self.frame_path(idx) else { continue };
            keep.push(path.clone());
            if !self.onion_cache.contains_key(&path) {
                let Some(l) = self.store.get(&path, false) else { continue };
                let img = self.prepare(&l.preview, l.full_w, l.full_h, corrected);
                let ci = match self.onion.mode {
                    OnionMode::Lines => imageops::alpha_to_color_image(img.w, img.h, &imageops::mask_lines(&img, self.onion.threshold)),
                    OnionMode::Edges => imageops::alpha_to_color_image(img.w, img.h, &imageops::mask_edges(&img, self.onion.threshold)),
                    OnionMode::Image => imageops::alpha_to_color_image(img.w, img.h, &imageops::mask_image(&img)),
                };
                let tex = self.ctx.load_texture("onion", ci, TextureOptions::LINEAR);
                self.onion_cache.insert(path.clone(), tex);
            }
            if let Some(t) = self.onion_cache.get(&path) {
                out.push((t.id(), col));
            }
        }
        if self.onion_cache.len() > 24 {
            self.onion_cache.retain(|p, _| keep.contains(p));
        }
        out
    }

    fn viewer(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(Color32::from_gray(18))).show(ui, |ui| {
            if self.project.is_none() {
                self.start_screen(ui);
                return;
            }
            // playback clock
            let n = self.seq_len();
            let fps = self.take.as_ref().map(|t| t.fps).unwrap_or(12) as f64;
            let hold = self.take.as_ref().map(|t| t.hold).unwrap_or(1) as f64;
            if let Some(p) = &mut self.play {
                let step = (p.start.elapsed().as_secs_f64() * fps / hold) as usize;
                let raw = p.from + step;
                if n == 0 {
                    self.play = None;
                } else if raw >= n && !self.loop_play {
                    self.play = None;
                    self.view = View::Live;
                } else {
                    let idx = raw % n;
                    p.last_idx = idx;
                    self.view = View::Frame(idx);
                    for k in 1..=6 {
                        if let Some(path) = self.frame_path((idx + k) % n) {
                            self.store.get(&path, false);
                        }
                    }
                    self.ctx.request_repaint_after(Duration::from_millis(5));
                }
            }

            let corrected = self.corrected();
            let avail = ui.available_rect_before_wrap();
            let sense = if self.editing_lines { Sense::click_and_drag() } else { Sense::click() };
            let (resp, painter) = ui.allocate_painter(avail.size(), sense);
            let Some((tex, size, _full)) = self.view_texture(corrected) else {
                let msg = match self.view {
                    View::Live if self.cam_open.is_none() => "No camera open",
                    View::Live => "Waiting for camera...",
                    View::Frame(_) => "Loading...",
                };
                painter.text(resp.rect.center(), egui::Align2::CENTER_CENTER, msg, egui::FontId::proportional(20.0), Color32::GRAY);
                return;
            };
            let rect = fit_rect(resp.rect.shrink(8.0), size[0] as f32, size[1] as f32);
            painter.image(tex, rect, uv_full(), Color32::WHITE);
            for (otex, col) in self.onion_textures(corrected) {
                painter.image(otex, rect, uv_full(), col);
            }
            if let View::Frame(_) = self.view {
                painter.rect_stroke(rect, 0.0, Stroke::new(2.0, Color32::from_rgb(255, 200, 0)), egui::StrokeKind::Outside);
            } else {
                painter.rect_stroke(rect, 0.0, Stroke::new(2.0, Color32::from_rgb(200, 40, 40)), egui::StrokeKind::Outside);
            }
            if self.editing_lines {
                self.edit_lines(&resp, &painter, rect, tex);
            }
        });
    }

    fn start_screen(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(80.0);
            ui.heading("TurtleFrame");
            ui.label("A project is a folder. Takes, frames and settings live inside it.");
            ui.add_space(16.0);
            if ui.button("Open / create project folder...").clicked() {
                let start = self.config.last_browse.clone().unwrap_or_else(home_dir);
                self.browser.open_at(start);
            }
            ui.add_space(16.0);
            let recent: Vec<PathBuf> = self.config.recent.iter().filter(|p| p.is_dir()).cloned().collect();
            if !recent.is_empty() {
                ui.label("Recent:");
                for r in recent {
                    if ui.link(r.display().to_string()).clicked() {
                        self.open_project(r);
                    }
                }
            }
        });
    }

    fn edit_lines(&mut self, resp: &egui::Response, painter: &egui::Painter, rect: Rect, tex: egui::TextureId) {
        let Some(t) = self.take.as_mut() else { return };
        let to_screen = |p: [f32; 2]| pos2(rect.min.x + p[0] * rect.width(), rect.min.y + p[1] * rect.height());
        let to_norm = |p: Pos2| [((p.x - rect.min.x) / rect.width()).clamp(0.0, 1.0), ((p.y - rect.min.y) / rect.height()).clamp(0.0, 1.0)];
        let pointer = resp.interact_pointer_pos().or(resp.hover_pos());

        let nearest = |lines: &Vec<undistort::Line>, p: Pos2| -> Option<(usize, usize)> {
            let mut best: Option<(usize, usize, f32)> = None;
            for (li, l) in lines.iter().enumerate() {
                for (hi, h) in l.iter().enumerate() {
                    let d = to_screen(*h).distance(p);
                    if d < PICK_RADIUS && best.map(|b| d < b.2).unwrap_or(true) {
                        best = Some((li, hi, d));
                    }
                }
            }
            best.map(|b| (b.0, b.1))
        };

        if resp.drag_started() {
            // use where the button went down, not where the drag threshold was crossed
            let origin = resp.ctx.input(|i| i.pointer.press_origin()).or(pointer);
            if let Some(p) = origin {
                // while "Add line" is armed, new lines win over existing handles so
                // consecutive edges can start at a shared corner
                if self.add_armed && t.undistort.lines.len() < MAX_LINES {
                    self.drag = Some(Drag::NewLine(to_norm(p)));
                } else if let Some((li, hi)) = nearest(&t.undistort.lines, p) {
                    self.drag = Some(Drag::Handle(li, hi));
                }
            }
        }
        if resp.dragged() {
            if let (Some(Drag::Handle(li, hi)), Some(p)) = (&self.drag, pointer) {
                if let Some(l) = t.undistort.lines.get_mut(*li) {
                    l[*hi] = to_norm(p);
                }
            }
        }
        if resp.drag_stopped() {
            if let (Some(Drag::NewLine(a)), Some(p)) = (&self.drag, pointer) {
                let b = to_norm(p);
                let len = ((b[0] - a[0]) * rect.width()).hypot((b[1] - a[1]) * rect.height());
                if len > 20.0 {
                    let m = [(a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0];
                    t.undistort.lines.push([*a, m, b]);
                    if t.undistort.lines.len() >= MAX_LINES {
                        self.add_armed = false;
                    }
                }
            }
            self.drag = None;
        }
        if resp.secondary_clicked() {
            if let Some(p) = pointer {
                if let Some((li, _)) = nearest(&t.undistort.lines, p) {
                    t.undistort.lines.remove(li);
                }
            }
        }

        // draw
        let colors = [
            Color32::from_rgb(0, 200, 255),
            Color32::from_rgb(255, 0, 200),
            Color32::from_rgb(255, 230, 0),
            Color32::from_rgb(120, 255, 120),
        ];
        let hover = pointer.and_then(|p| nearest(&t.undistort.lines, p));
        for (li, l) in t.undistort.lines.iter().enumerate() {
            let c = colors[li % colors.len()];
            let p: Vec<Pos2> = l.iter().map(|h| to_screen(*h)).collect();
            painter.line_segment([p[0], p[2]], Stroke::new(1.0, c.gamma_multiply(0.5)));
            painter.line_segment([p[0], p[1]], Stroke::new(2.0, c));
            painter.line_segment([p[1], p[2]], Stroke::new(2.0, c));
            for (hi, q) in p.iter().enumerate() {
                let hot = hover == Some((li, hi));
                painter.circle_stroke(*q, HANDLE_RADIUS + if hot { 3.0 } else { 0.0 }, Stroke::new(2.0, c));
                painter.circle_filled(*q, 2.0, c);
            }
        }
        if let (Some(Drag::NewLine(a)), Some(p)) = (&self.drag, pointer) {
            painter.line_segment([to_screen(*a), p], Stroke::new(2.0, Color32::WHITE));
        }
        // loupe while dragging
        if let (Some(_), Some(p)) = (&self.drag, pointer) {
            let zoom = 5.0;
            let box_size = 180.0;
            let half_uv = vec2(box_size / zoom / rect.width() / 2.0, box_size / zoom / rect.height() / 2.0);
            let c = to_norm(p);
            let uv = Rect::from_min_max(pos2(c[0] - half_uv.x, c[1] - half_uv.y), pos2(c[0] + half_uv.x, c[1] + half_uv.y));
            let mut at = p + vec2(30.0, -box_size - 30.0);
            if at.y < rect.top() {
                at.y = p.y + 30.0;
            }
            if at.x + box_size > rect.right() {
                at.x = p.x - box_size - 30.0;
            }
            let lr = Rect::from_min_size(at, vec2(box_size, box_size));
            painter.rect_filled(lr, 0.0, Color32::BLACK);
            painter.image(tex, lr, uv, Color32::WHITE);
            painter.rect_stroke(lr, 0.0, Stroke::new(1.0, Color32::WHITE), egui::StrokeKind::Outside);
            painter.line_segment([lr.center_top(), lr.center_bottom()], Stroke::new(1.0, Color32::from_white_alpha(120)));
            painter.line_segment([lr.left_center(), lr.right_center()], Stroke::new(1.0, Color32::from_white_alpha(120)));
        }
        if self.add_armed && self.drag.is_none() {
            painter.text(rect.left_top() + vec2(8.0, 8.0), egui::Align2::LEFT_TOP, "Drag to add a line", egui::FontId::proportional(16.0), Color32::WHITE);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_events();
        let before = self.take.as_ref().map(|t| (t.undistort.clone(), t.fps, t.hold));

        self.handle_keys(&ctx);
        self.top_bar(ui);
        self.left_panel(ui);
        self.timeline(ui);
        self.viewer(ui);

        if let Some(p) = self.browser.show(&ctx) {
            self.open_project(p);
        }

        let after = self.take.as_ref().map(|t| (t.undistort.clone(), t.fps, t.hold));
        if before != after {
            if before.as_ref().map(|b| &b.0) != after.as_ref().map(|a| &a.0) {
                self.rev += 1;
            }
            self.meta_dirty = true;
        }
        if self.meta_dirty && !ctx.input(|i| i.pointer.any_down()) {
            self.save_meta_now();
        }
        if self.export.is_some() || self.store.saving.load(Ordering::SeqCst) > 0 {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_meta_now();
        self.cam.shutdown();
        self.store.shutdown();
    }
}

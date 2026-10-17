//! Background frame loading (previews) and saving (full-res PNG + preview JPEG).

use crate::imageops::{box_downscale, image_dims, load_rgb, save_jpeg, save_png, RgbImage, PREVIEW_MAX};
use crate::project::preview_path;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};

pub struct Loaded {
    pub full_w: usize,
    pub full_h: usize,
    pub preview: Arc<RgbImage>,
}

type LoadResult = (PathBuf, Result<Loaded, String>);

struct Queue {
    items: Mutex<(VecDeque<PathBuf>, bool)>, // (queue, shutdown)
    cv: Condvar,
}

struct SaveJob {
    png: PathBuf,
    full: Arc<RgbImage>,
    preview: Arc<RgbImage>,
}

pub struct FrameStore {
    cache: HashMap<PathBuf, Arc<Loaded>>,
    lru: VecDeque<PathBuf>,
    pending: HashSet<PathBuf>,
    failed: HashMap<PathBuf, String>,
    cap: usize,
    queue: Arc<Queue>,
    res_rx: Receiver<LoadResult>,
    newly_loaded: Vec<PathBuf>,
    save_tx: Option<Sender<SaveJob>>,
    pub saving: Arc<AtomicUsize>,
    pub save_errors: Arc<Mutex<Vec<String>>>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

fn load_one(png: &Path) -> Result<Loaded, String> {
    let (full_w, full_h) = image_dims(png)?;
    let pp = preview_path(png);
    if let Ok(p) = load_rgb(&pp) {
        return Ok(Loaded { full_w, full_h, preview: Arc::new(p) });
    }
    let full = load_rgb(png)?;
    let prev = box_downscale(&full, PREVIEW_MAX);
    if let Some(dir) = pp.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    save_jpeg(&pp, &prev, 90).ok();
    Ok(Loaded { full_w, full_h, preview: Arc::new(prev) })
}

/// How many previews to keep in memory: 1/8 of system RAM at ~3 MB each,
/// so 512 MB and 1 GB Raspberry Pis stay usable. Non-Linux: 160.
fn preview_cache_cap() -> usize {
    let mem_kb = std::fs::read_to_string("/proc/meminfo").ok().and_then(|t| {
        t.lines()
            .find(|l| l.starts_with("MemTotal:"))
            .and_then(|l| l.split_whitespace().nth(1)?.parse::<usize>().ok())
    });
    match mem_kb {
        Some(kb) => (kb / 8 / 3072).clamp(12, 160),
        None => 160,
    }
}

impl FrameStore {
    pub fn new(ctx: eframe::egui::Context) -> Self {
        let queue = Arc::new(Queue { items: Mutex::new((VecDeque::new(), false)), cv: Condvar::new() });
        let (res_tx, res_rx) = channel::<LoadResult>();
        let mut threads = Vec::new();
        for _ in 0..2 {
            let q = queue.clone();
            let tx = res_tx.clone();
            let ctx = ctx.clone();
            threads.push(std::thread::spawn(move || loop {
                let path = {
                    let mut g = q.items.lock().unwrap();
                    loop {
                        if g.1 {
                            return;
                        }
                        if let Some(p) = g.0.pop_front() {
                            break p;
                        }
                        g = q.cv.wait(g).unwrap();
                    }
                };
                let r = load_one(&path);
                if tx.send((path, r)).is_err() {
                    return;
                }
                ctx.request_repaint();
            }));
        }
        let (save_tx, save_rx) = channel::<SaveJob>();
        let saving = Arc::new(AtomicUsize::new(0));
        let save_errors = Arc::new(Mutex::new(Vec::new()));
        {
            let saving = saving.clone();
            let errs = save_errors.clone();
            let ctx = ctx.clone();
            threads.push(std::thread::spawn(move || {
                while let Ok(job) = save_rx.recv() {
                    if let Err(e) = save_png(&job.png, &job.full) {
                        errs.lock().unwrap().push(e);
                    }
                    let pp = preview_path(&job.png);
                    if let Some(dir) = pp.parent() {
                        std::fs::create_dir_all(dir).ok();
                    }
                    if let Err(e) = save_jpeg(&pp, &job.preview, 90) {
                        errs.lock().unwrap().push(e);
                    }
                    saving.fetch_sub(1, Ordering::SeqCst);
                    ctx.request_repaint();
                }
            }));
        }
        Self {
            cache: HashMap::new(),
            lru: VecDeque::new(),
            pending: HashSet::new(),
            failed: HashMap::new(),
            cap: preview_cache_cap(),
            queue,
            res_rx,
            newly_loaded: Vec::new(),
            save_tx: Some(save_tx),
            saving,
            save_errors,
            threads,
        }
    }

    /// Pull finished loads into the cache. Call once per UI frame.
    pub fn poll(&mut self) {
        while let Ok((path, r)) = self.res_rx.try_recv() {
            self.pending.remove(&path);
            match r {
                Ok(l) => self.insert(path, l),
                Err(e) => {
                    // a missing file may still be in the save queue; allow a retry later
                    if path.exists() {
                        self.failed.insert(path, e);
                    }
                }
            }
        }
    }

    pub fn insert(&mut self, path: PathBuf, l: Loaded) {
        self.failed.remove(&path);
        self.cache.insert(path.clone(), Arc::new(l));
        self.lru.retain(|p| *p != path);
        self.lru.push_back(path.clone());
        self.newly_loaded.push(path);
        while self.lru.len() > self.cap {
            if let Some(old) = self.lru.pop_front() {
                self.cache.remove(&old);
            }
        }
    }

    /// Get a preview if cached; otherwise queue it (urgent ones jump the queue).
    pub fn get(&mut self, path: &Path, urgent: bool) -> Option<Arc<Loaded>> {
        if let Some(l) = self.cache.get(path) {
            let l = l.clone();
            if self.lru.back().map(|b| b != path).unwrap_or(true) {
                self.lru.retain(|p| p != path);
                self.lru.push_back(path.to_path_buf());
            }
            return Some(l);
        }
        if self.failed.contains_key(path) {
            return None;
        }
        let mut g = self.queue.items.lock().unwrap();
        if self.pending.contains(path) {
            if urgent {
                if let Some(pos) = g.0.iter().position(|p| p == path) {
                    let p = g.0.remove(pos).unwrap();
                    g.0.push_front(p);
                }
            }
            return None;
        }
        self.pending.insert(path.to_path_buf());
        if urgent {
            g.0.push_front(path.to_path_buf());
        } else {
            g.0.push_back(path.to_path_buf());
        }
        self.queue.cv.notify_one();
        None
    }

    pub fn failed(&self, path: &Path) -> Option<&String> {
        self.failed.get(path)
    }

    pub fn take_newly_loaded(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.newly_loaded)
    }

    /// Drop queued (not yet started) background loads, e.g. when switching takes.
    pub fn clear_queue(&mut self) {
        let mut g = self.queue.items.lock().unwrap();
        for p in g.0.drain(..) {
            self.pending.remove(&p);
        }
    }

    pub fn save_async(&self, png: PathBuf, full: Arc<RgbImage>, preview: Arc<RgbImage>) {
        if let Some(tx) = &self.save_tx {
            self.saving.fetch_add(1, Ordering::SeqCst);
            if tx.send(SaveJob { png, full, preview }).is_err() {
                self.saving.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }

    /// Finish pending saves and stop threads.
    pub fn shutdown(&mut self) {
        {
            let mut g = self.queue.items.lock().unwrap();
            g.1 = true;
            g.0.clear();
        }
        self.queue.cv.notify_all();
        self.save_tx = None;
        for t in self.threads.drain(..) {
            t.join().ok();
        }
    }
}

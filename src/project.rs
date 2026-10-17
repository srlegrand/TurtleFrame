//! Projects are folders. Each project holds takes; each take holds its frames,
//! its frame order and its undistort settings. All metadata is plain text.
//!
//! <project>/
//!   turtleframe.project          marker + project settings
//!   takes/take_001/take.txt      name, fps, holds, undistort
//!   takes/take_001/sequence.txt  frame order, one id per line
//!   takes/take_001/frames/000001.png         full resolution capture (raw)
//!   takes/take_001/frames/preview/000001.jpg preview for fast playback
//!   export/take_001/...          corrected PNG sequences

use crate::undistort::{Aspect, Settings};
use std::path::{Path, PathBuf};

pub const MARKER: &str = "turtleframe.project";

fn parse_kv(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') {
                return None;
            }
            let (k, v) = l.split_once('=')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

pub struct Project {
    pub root: PathBuf,
    pub takes: Vec<(String, String)>, // (dir name, display name)
    pub current_take: String,
    /// Saved camera controls: (control key, value) in driver order.
    pub saved_controls: Vec<(String, String)>,
}

impl Project {
    pub fn name(&self) -> String {
        self.root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| self.root.display().to_string())
    }

    /// Open a folder as a project, initialising it if it has no marker yet.
    pub fn open_or_init(root: &Path) -> Result<Project, String> {
        std::fs::create_dir_all(root.join("takes")).map_err(|e| format!("{}: {e}", root.display()))?;
        let marker = root.join(MARKER);
        let mut p = Project {
            root: root.to_path_buf(),
            takes: Vec::new(),
            current_take: String::new(),
            saved_controls: Vec::new(),
        };
        if let Ok(text) = std::fs::read_to_string(&marker) {
            for (k, v) in parse_kv(&text) {
                match k.as_str() {
                    "current_take" => p.current_take = v,
                    "ctrl" => {
                        if let Some((a, b)) = v.split_once('|') {
                            p.saved_controls.push((a.to_string(), b.to_string()));
                        }
                    }
                    _ => {}
                }
            }
        }
        p.scan_takes();
        if p.takes.is_empty() {
            let t = p.create_take("Take 1", None)?;
            p.current_take = t.dir_name();
        }
        if !p.takes.iter().any(|(d, _)| *d == p.current_take) {
            p.current_take = p.takes[0].0.clone();
        }
        p.save()?;
        Ok(p)
    }

    pub fn scan_takes(&mut self) {
        let mut takes = Vec::new();
        if let Ok(rd) = std::fs::read_dir(self.root.join("takes")) {
            for e in rd.flatten() {
                let path = e.path();
                if path.join("take.txt").is_file() {
                    let dir = e.file_name().to_string_lossy().to_string();
                    let name = std::fs::read_to_string(path.join("take.txt"))
                        .ok()
                        .and_then(|t| parse_kv(&t).into_iter().find(|(k, _)| k == "name").map(|(_, v)| v))
                        .unwrap_or_else(|| dir.clone());
                    takes.push((dir, name));
                }
            }
        }
        takes.sort();
        self.takes = takes;
    }

    pub fn save(&self) -> Result<(), String> {
        let mut s = String::from("# TurtleFrame project\nversion=1\n");
        s += &format!("current_take={}\n", self.current_take);
        for (k, v) in &self.saved_controls {
            s += &format!("ctrl={k}|{v}\n");
        }
        write_atomic(&self.root.join(MARKER), &s)
    }

    pub fn create_take(&mut self, name: &str, template: Option<&Take>) -> Result<Take, String> {
        let mut n = self.takes.len() + 1;
        let dir = loop {
            let d = self.root.join("takes").join(format!("take_{n:03}"));
            if !d.exists() {
                break d;
            }
            n += 1;
        };
        std::fs::create_dir_all(dir.join("frames").join("preview")).map_err(|e| format!("{}: {e}", dir.display()))?;
        let mut t = Take {
            dir,
            name: if name.trim().is_empty() { format!("Take {n}") } else { name.trim().to_string() },
            fps: 12,
            hold: 1,
            undistort: Settings::default(),
            seq: Vec::new(),
            next_id: 1,
        };
        if let Some(tp) = template {
            t.fps = tp.fps;
            t.hold = tp.hold;
            t.undistort = tp.undistort.clone();
        }
        t.save_meta()?;
        t.save_seq()?;
        self.scan_takes();
        Ok(t)
    }

    pub fn load_take(&self, dir_name: &str) -> Result<Take, String> {
        Take::load(&self.root.join("takes").join(dir_name))
    }

    pub fn export_dir(&self, take: &Take) -> PathBuf {
        self.root.join("export").join(take.dir_name())
    }
}

#[derive(Clone)]
pub struct Take {
    pub dir: PathBuf,
    pub name: String,
    pub fps: u32,
    /// Exposures per frame during playback (on ones, twos, threes).
    pub hold: u32,
    pub undistort: Settings,
    pub seq: Vec<String>,
    pub next_id: u64,
}

impl Take {
    pub fn dir_name(&self) -> String {
        self.dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
    }

    pub fn frame_path(&self, id: &str) -> PathBuf {
        self.dir.join("frames").join(format!("{id}.png"))
    }

    pub fn load(dir: &Path) -> Result<Take, String> {
        let text = std::fs::read_to_string(dir.join("take.txt")).map_err(|e| format!("{}: {e}", dir.display()))?;
        let mut t = Take {
            dir: dir.to_path_buf(),
            name: String::new(),
            fps: 12,
            hold: 1,
            undistort: Settings { lines: Vec::new(), ..Default::default() },
            seq: Vec::new(),
            next_id: 1,
        };
        for (k, v) in parse_kv(&text) {
            match k.as_str() {
                "name" => t.name = v,
                "fps" => t.fps = v.parse().unwrap_or(12).clamp(1, 60),
                "hold" => t.hold = v.parse().unwrap_or(1).clamp(1, 4),
                "next_id" => t.next_id = v.parse().unwrap_or(1),
                "lens" => t.undistort.lens = v == "1",
                "flatten" => t.undistort.flatten = v == "1",
                "aspect" => t.undistort.aspect = Aspect::parse(&v),
                "k_adjust" => t.undistort.k_adjust = v.parse().unwrap_or(0.0),
                "line" => {
                    let pts: Vec<[f32; 2]> = v
                        .split(';')
                        .filter_map(|p| {
                            let (a, b) = p.split_once(',')?;
                            Some([a.trim().parse().ok()?, b.trim().parse().ok()?])
                        })
                        .collect();
                    if pts.len() == 3 && t.undistort.lines.len() < crate::undistort::MAX_LINES {
                        t.undistort.lines.push([pts[0], pts[1], pts[2]]);
                    }
                }
                _ => {}
            }
        }
        if t.name.is_empty() {
            t.name = t.dir_name();
        }
        if let Ok(seq) = std::fs::read_to_string(dir.join("sequence.txt")) {
            t.seq = seq.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
        }
        // never hand out an id that is already on disk
        let max_on_disk = std::fs::read_dir(dir.join("frames"))
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| e.path().file_stem().and_then(|s| s.to_str()?.parse::<u64>().ok()))
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        t.next_id = t.next_id.max(max_on_disk + 1);
        std::fs::create_dir_all(dir.join("frames").join("preview")).ok();
        Ok(t)
    }

    pub fn save_meta(&self) -> Result<(), String> {
        let u = &self.undistort;
        let mut s = String::from("# TurtleFrame take\n");
        s += &format!("name={}\nfps={}\nhold={}\nnext_id={}\n", self.name, self.fps, self.hold, self.next_id);
        s += &format!(
            "lens={}\nflatten={}\naspect={}\nk_adjust={}\n",
            u.lens as u8,
            u.flatten as u8,
            u.aspect.to_str(),
            u.k_adjust
        );
        for l in &u.lines {
            s += &format!(
                "line={},{};{},{};{},{}\n",
                l[0][0], l[0][1], l[1][0], l[1][1], l[2][0], l[2][1]
            );
        }
        write_atomic(&self.dir.join("take.txt"), &s)
    }

    pub fn save_seq(&self) -> Result<(), String> {
        let mut s = self.seq.join("\n");
        s.push('\n');
        write_atomic(&self.dir.join("sequence.txt"), &s)
    }

    pub fn alloc_id(&mut self) -> String {
        loop {
            let id = format!("{:06}", self.next_id);
            self.next_id += 1;
            if !self.frame_path(&id).exists() {
                return id;
            }
        }
    }

    /// Move frame files no longer referenced by the sequence into trash/.
    pub fn purge_unused(&self) -> Result<usize, String> {
        let trash = self.dir.join("trash");
        let mut moved = 0;
        let rd = std::fs::read_dir(self.dir.join("frames")).map_err(|e| e.to_string())?;
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("png") {
                continue;
            }
            let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else { continue };
            if self.seq.iter().any(|id| id == stem) {
                continue;
            }
            std::fs::create_dir_all(trash.join("preview")).map_err(|e| e.to_string())?;
            std::fs::rename(&p, trash.join(format!("{stem}.png"))).map_err(|e| e.to_string())?;
            let prev = self.dir.join("frames").join("preview").join(format!("{stem}.jpg"));
            if prev.exists() {
                std::fs::rename(&prev, trash.join("preview").join(format!("{stem}.jpg"))).ok();
            }
            moved += 1;
        }
        Ok(moved)
    }
}

pub fn preview_path(png: &Path) -> PathBuf {
    let stem = png.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    png.parent().unwrap_or(Path::new(".")).join("preview").join(format!("{stem}.jpg"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_roundtrip() {
        let root = std::env::temp_dir().join(format!("tf_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut p = Project::open_or_init(&root).unwrap();
        assert_eq!(p.takes.len(), 1);
        let mut t = p.load_take(&p.current_take.clone()).unwrap();
        t.undistort.lines.push([[0.1, 0.2], [0.5, 0.21], [0.9, 0.2]]);
        t.undistort.aspect = Aspect::Letter;
        t.fps = 24;
        let a = t.alloc_id();
        let b = t.alloc_id();
        t.seq = vec![a.clone(), b, a];
        t.save_meta().unwrap();
        t.save_seq().unwrap();
        let t2 = p.load_take(&t.dir_name()).unwrap();
        assert_eq!(t2.undistort, t.undistort);
        assert_eq!(t2.seq, t.seq);
        assert_eq!(t2.fps, 24);
        let t3 = p.create_take("Walk cycle", Some(&t2)).unwrap();
        assert_eq!(t3.undistort, t.undistort);
        p.saved_controls.push(("Exposure".into(), "i:100".into()));
        p.current_take = t3.dir_name();
        p.save().unwrap();
        let p2 = Project::open_or_init(&root).unwrap();
        assert_eq!(p2.takes.len(), 2);
        assert_eq!(p2.current_take, t3.dir_name());
        assert_eq!(p2.saved_controls.len(), 1);
        std::fs::remove_dir_all(&root).ok();
    }
}

//! Small per-user config: recent projects and last camera.

use std::path::PathBuf;

pub fn config_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library").join("Application Support"));
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    base.map(|b| b.join("TurtleFrame"))
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[derive(Default)]
pub struct Config {
    pub recent: Vec<PathBuf>,
    pub last_camera: String,
    pub last_browse: Option<PathBuf>,
}

impl Config {
    pub fn load() -> Config {
        let mut c = Config::default();
        let Some(dir) = config_dir() else { return c };
        let Ok(text) = std::fs::read_to_string(dir.join("config.txt")) else { return c };
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                match k {
                    "recent" => c.recent.push(PathBuf::from(v)),
                    "last_camera" => c.last_camera = v.to_string(),
                    "last_browse" => c.last_browse = Some(PathBuf::from(v)),
                    _ => {}
                }
            }
        }
        c
    }

    pub fn save(&self) {
        let Some(dir) = config_dir() else { return };
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let mut s = String::new();
        for r in &self.recent {
            s += &format!("recent={}\n", r.display());
        }
        s += &format!("last_camera={}\n", self.last_camera);
        if let Some(b) = &self.last_browse {
            s += &format!("last_browse={}\n", b.display());
        }
        let _ = std::fs::write(dir.join("config.txt"), s);
    }

    pub fn push_recent(&mut self, p: PathBuf) {
        self.recent.retain(|r| *r != p);
        self.recent.insert(0, p);
        self.recent.truncate(12);
        self.save();
    }
}

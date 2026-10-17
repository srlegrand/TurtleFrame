//! Minimal in-app folder picker (avoids native dialog dependencies).

use crate::project::MARKER;
use eframe::egui;
use std::path::PathBuf;

pub struct FolderBrowser {
    pub open: bool,
    path: PathBuf,
    path_text: String,
    entries: Vec<(String, bool)>, // (name, is project)
    new_name: String,
    error: Option<String>,
}

impl FolderBrowser {
    pub fn new(start: PathBuf) -> Self {
        let mut b = Self {
            open: false,
            path: start.clone(),
            path_text: start.display().to_string(),
            entries: Vec::new(),
            new_name: String::new(),
            error: None,
        };
        b.refresh();
        b
    }

    pub fn open_at(&mut self, p: PathBuf) {
        self.open = true;
        self.set_path(p);
    }

    fn set_path(&mut self, p: PathBuf) {
        self.path = p;
        self.path_text = self.path.display().to_string();
        self.refresh();
    }

    fn refresh(&mut self) {
        self.error = None;
        self.entries.clear();
        match std::fs::read_dir(&self.path) {
            Ok(rd) => {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().to_string();
                    if name.starts_with('.') {
                        continue;
                    }
                    if e.path().is_dir() {
                        let is_proj = e.path().join(MARKER).is_file();
                        self.entries.push((name, is_proj));
                    }
                }
                self.entries.sort_by_key(|(n, _)| n.to_lowercase());
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    /// Returns the folder chosen this frame, if any.
    pub fn show(&mut self, ctx: &egui::Context) -> Option<PathBuf> {
        if !self.open {
            return None;
        }
        let mut chosen = None;
        let mut open = self.open;
        let mut go_to: Option<PathBuf> = None;
        egui::Window::new("Open or create project folder")
            .open(&mut open)
            .collapsible(false)
            .default_size([560.0, 480.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("Up").clicked() {
                        if let Some(p) = self.path.parent() {
                            go_to = Some(p.to_path_buf());
                        }
                    }
                    if ui.button("Home").clicked() {
                        go_to = Some(crate::config::home_dir());
                    }
                    #[cfg(target_os = "windows")]
                    for d in b'A'..=b'Z' {
                        let root = format!("{}:\\", d as char);
                        if std::path::Path::new(&root).exists() && ui.small_button(format!("{}:", d as char)).clicked() {
                            go_to = Some(PathBuf::from(root));
                        }
                    }
                });
                ui.horizontal(|ui| {
                    let r = ui.add(egui::TextEdit::singleline(&mut self.path_text).desired_width(440.0));
                    if ui.button("Go").clicked() || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) {
                        go_to = Some(PathBuf::from(self.path_text.trim()));
                    }
                });
                if let Some(e) = &self.error {
                    ui.colored_label(egui::Color32::LIGHT_RED, e);
                }
                ui.separator();
                egui::ScrollArea::vertical().max_height(300.0).auto_shrink([false, false]).show(ui, |ui| {
                    for (name, is_proj) in &self.entries {
                        let label = if *is_proj { format!("[project] {name}") } else { format!("{name}/") };
                        if ui.selectable_label(false, label).clicked() {
                            go_to = Some(self.path.join(name));
                        }
                    }
                });
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("New folder:");
                    ui.text_edit_singleline(&mut self.new_name);
                    if ui.button("Create").clicked() && !self.new_name.trim().is_empty() {
                        let p = self.path.join(self.new_name.trim());
                        match std::fs::create_dir_all(&p) {
                            Ok(_) => {
                                self.new_name.clear();
                                go_to = Some(p);
                            }
                            Err(e) => self.error = Some(e.to_string()),
                        }
                    }
                });
                ui.separator();
                let is_proj = self.path.join(MARKER).is_file();
                ui.label(if is_proj {
                    "This folder is a TurtleFrame project."
                } else {
                    "This folder will become a new project."
                });
                if ui.button(format!("Use this folder: {}", self.path.display())).clicked() {
                    chosen = Some(self.path.clone());
                }
            });
        if let Some(p) = go_to {
            self.set_path(p);
        }
        self.open = open && chosen.is_none();
        chosen
    }
}

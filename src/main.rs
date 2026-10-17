#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod browser;
mod camera;
mod config;
mod decode;
mod imageops;
mod project;
mod store;
mod undistort;

use eframe::egui;

fn main() -> eframe::Result<()> {
    // macOS needs camera permission before devices can be listed.
    #[cfg(target_os = "macos")]
    nokhwa::nokhwa_initialize(|granted| {
        if !granted {
            eprintln!("Camera permission not granted");
        }
    });

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([900.0, 600.0])
            .with_title("TurtleFrame"),
        ..Default::default()
    };
    eframe::run_native("TurtleFrame", options, Box::new(|cc| Ok(Box::new(app::App::new(cc)))))
}

// GUI-subsystem exe on Windows release builds: double-click opens the window without a console.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

//! Latency Tester Suite - Cross-platform latency measurement application
//! 
//! Measures CPU, memory, GPU, and input latency with cryptographic result verification

mod timer;
mod system_info;
mod memory_benchmark;
mod cpu_benchmark;
mod gpu_benchmark;
mod input_latency;
mod result_logger;
mod gui;
mod virtualization;
mod verification;
mod topology;
mod cancel;
mod input_test;
mod rig;
mod hwinfo;
mod session;
mod report;
mod server;
mod sensors;
mod progress;
mod run_all;
mod app_core;
mod timer_info;
mod pattern_window;
mod render_setup;
mod lhm;

use anyhow::Result;
use eframe::egui;
use gui::LatencyTesterApp;

mod cli;

fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // Headless mode: `latency-tester --cli [--out results.json]`
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--cli" || a == "--serve" || a == "--report" || a == "--help" || a == "-h") {
        attach_parent_console();
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", cli::USAGE);
        return Ok(());
    }
    if args.iter().any(|a| a == "--serve") {
        return cli::serve(&args);
    }
    if args.iter().any(|a| a == "--report") {
        return cli::report(&args);
    }
    if args.iter().any(|a| a == "--cli") {
        return cli::run(&args);
    }
    // Precise VSync-locked display pattern in its own window (started from the Input tab)
    if args.iter().any(|a| a == "--pattern") {
        return pattern_window::run(&args);
    }

    // Run the GUI application
    // VSync off by default: input timestamps are taken once per frame, so a fast unsynchronised frame
    // loop keeps that quantisation to ~1 ms. `--vsync` restores normal presentation.
    let vsync = args.iter().any(|a| a == "--vsync");
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1200.0, 900.0])
        .with_min_inner_size([800.0, 600.0])
        .with_title("Latency Tester Suite v1.0");
    // Window / taskbar icon (the exe's file icon is embedded by build.rs)
    if let Ok(icon) = window_icon() {
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    }
    // Vulkan (wgpu, low-latency presentation) when available, OpenGL otherwise; --renderer overrides
    render_setup::run_with_fallback("Latency Tester Suite", render_setup::parse(&args), vsync, viewport, |cc| {
        let mut app = LatencyTesterApp::new(cc);
        app.set_renderer(render_setup::describe(cc, vsync));
        Box::new(app)
    })?;

    Ok(())
}
/// The application icon (assets/icon.svg rendered at 256 px)
fn window_icon() -> Result<egui::IconData, String> {
    eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon_256.png")).map_err(|e| e.to_string())
}

/// The Windows release exe has no console of its own; attach to the launching terminal
/// so `--cli` / `--help` output is visible there.
#[cfg(all(windows, not(debug_assertions)))]
fn attach_parent_console() {
    use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

#[cfg(not(all(windows, not(debug_assertions))))]
fn attach_parent_console() {}

#[cfg(test)]
mod tests {
    #[test]
    fn the_window_icon_decodes() {
        let icon = super::window_icon().expect("assets/icon_256.png must be a valid PNG");
        assert_eq!((icon.width, icon.height), (256, 256));
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
        // rounded tile: the corner is transparent, the middle is not
        assert_eq!(icon.rgba[3], 0);
        assert!(icon.rgba[(128 * 256 + 128) * 4 + 3] > 200);
    }
}

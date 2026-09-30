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

    // Run the GUI application
    // VSync off by default: input timestamps are taken once per frame, so a fast unsynchronised frame
    // loop keeps that quantisation to ~1 ms. `--vsync` restores normal presentation.
    let vsync = args.iter().any(|a| a == "--vsync");
    let options = eframe::NativeOptions {
        vsync,
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 900.0])
            .with_min_inner_size([800.0, 600.0])
            .with_title("Latency Tester Suite v1.0"),
        ..Default::default()
    };

    eframe::run_native(
        "Latency Tester Suite",
        options,
        Box::new(|cc| Ok(Box::new(LatencyTesterApp::new(cc)))),
    )
    .map_err(|e| anyhow::anyhow!("GUI error: {}", e))?;

    Ok(())
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

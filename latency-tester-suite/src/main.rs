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

use anyhow::Result;
use eframe::egui;
use gui::LatencyTesterApp;

fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // Run the GUI application
    let options = eframe::NativeOptions {
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
    )?;

    Ok(())
}
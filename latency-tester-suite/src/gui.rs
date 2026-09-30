//! GUI for the Latency Tester Suite using egui/eframe
//! 
//! Tabs: Dashboard, Memory, CPU, GPU, Input Latency, Virtualization, Results

use eframe::egui;
use egui::{Color32, RichText, Ui, Sense};
use rand::Rng;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

mod core_select;
mod memory_ui;
mod results_ui;
mod suites_ui;

use crate::cancel::{self, CancelFlag};
use crate::gpu_benchmark::GpuBenchmarkConfig;
use crate::input_latency::{InputLatencyTester, InputLatencyConfig, InputTestMode, measure_timer_resolution, TimerResolutionInfo};
use crate::memory_benchmark::{MemoryBenchmarkConfig, MemoryBenchmarkSummary, QuickMemoryResult};
use crate::result_logger::{ResultLogger, VerifiedResult, generate_shareable_summary};
use crate::system_info::SystemInfo;
use crate::virtualization::{VirtualizationDetector, VirtualizationStatus};

#[derive(Default, Clone)]
enum Tab {
    #[default]
    Dashboard,
    Memory,
    Cpu,
    Gpu,
    Input,
    Virtualization,
    Graphs,
    Results,
}

pub struct LatencyTesterApp {
    tab: Tab,
    
    // System info
    system_info: Option<SystemInfo>,
    virt_status: Option<VirtualizationStatus>,
    last_refresh: Instant,
    info_refreshing: bool,
    
    // Result logger
    logger: Option<ResultLogger>,
    
    // Background task results
    last_memory_result: Option<MemoryBenchmarkSummary>,
    last_cpu_result: Option<crate::cpu_benchmark::CpuBenchmarkSummary>,
    last_gpu_result: Option<crate::gpu_benchmark::GpuBenchmarkSummary>,
    last_input_result: Option<crate::input_latency::InputLatencySummary>,
    last_logged: Option<(VerifiedResult, crate::result_logger::VerificationResult)>,
    all_results: Option<Vec<VerifiedResult>>,
    
    // Running task (completed tasks are stored in pending)
    running: Arc<std::sync::Mutex<Option<RunningTaskState>>>,
    cancel: CancelFlag,
    quick_mem_result: Option<QuickMemoryResult>,
    quick_mem_error: Option<String>,
    task_status: String,
    
    // Memory config
    mem_config: MemoryBenchmarkConfig,
    mem_ui: memory_ui::MemUi,
    mem_progress: crate::memory_benchmark::ProgressHandle,
    mem_config_error: Option<String>,
    results_ui: results_ui::ResultsUi,
    
    // CPU config
    cpu_ui: suites_ui::CpuUi,
    cpu_progress: crate::progress::SharedProgress,
    cpu_partial: crate::progress::SharedResults<crate::cpu_benchmark::CpuBenchmarkResult>,
    gpu_progress: crate::progress::SharedProgress,
    gpu_partial: crate::progress::SharedResults<crate::gpu_benchmark::GpuBenchmarkResult>,
    input_ui: suites_ui::InputUi,
    input_progress: crate::progress::SharedProgress,
    sampler: Option<Arc<crate::sensors::Sampler>>,
    sampler_active: bool,
    last_timeline: Vec<crate::sensors::Snapshot>,
    sensor_notes: Vec<String>,
    sensor_probe: Option<(Vec<String>, crate::sensors::Snapshot)>,
    
    // GPU config
    gpu_config: GpuBenchmarkConfig,
    gpu_custom_k: u32,
    
    // Input latency interactive state
    input_tester: InputLatencyTester,
    input_state: InputState,
    input_waiting_until: Option<f64>,  // Instant seconds since app start
    ready_time_ticks: u64,
    input_start_time: Instant,
    click_samples: Vec<f64>,
    last_click_latency: Option<f64>,
    timer_info: Option<TimerResolutionInfo>,
    show_log_panel: bool,
    log_text: String,
}

#[derive(Default, Clone, Copy, PartialEq)]
enum InputState {
    #[default]
    Idle,       // Press space / click to start
    Waiting,    // Red screen - wait for green
    Ready,      // Green screen - CLICK NOW
    Result,     // Show result
    FalseStart, // Clicked too early
}

#[allow(dead_code)] // kept for display/diagnostics
struct RunningTaskState {
    kind: String,
    started: Instant,
}

impl LatencyTesterApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // Setup egui fonts and visuals
        egui_extras::install_image_loaders(&cc.egui_ctx);
        
        // Keep results next to the exe so the app stays portable (falls back to the working dir)
        let config_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("latency_results")))
            .filter(|d| std::fs::create_dir_all(d).is_ok())
            .unwrap_or_else(|| std::path::PathBuf::from("latency_results"));
        let _ = std::fs::create_dir_all(&config_dir);
        
        let logger = ResultLogger::new(
            env!("CARGO_PKG_VERSION").to_string(),
            config_dir.to_string_lossy().to_string(),
        ).ok();
        
        let core_kinds = crate::topology::detect_core_kinds();
        let input_tester = InputLatencyTester::new(InputLatencyConfig::default());
        
        Self {
            tab: Tab::Dashboard,
            system_info: None,
            virt_status: None,
            last_refresh: Instant::now(),
            info_refreshing: false,
            logger,
            last_memory_result: None,
            last_cpu_result: None,
            last_gpu_result: None,
            last_input_result: None,
            last_logged: None,
            all_results: None,
            running: Arc::new(std::sync::Mutex::new(None)),
            cancel: cancel::new_flag(),
            quick_mem_result: None,
            quick_mem_error: None,
            task_status: String::from("Ready"),
            mem_config: MemoryBenchmarkConfig::default(),
            mem_ui: memory_ui::MemUi::new(core_kinds.clone()),
            mem_progress: crate::memory_benchmark::new_progress(),
            mem_config_error: None,
            results_ui: results_ui::ResultsUi::new(),
            cpu_ui: suites_ui::CpuUi::new(core_kinds.clone()),
            cpu_progress: crate::progress::new(),
            cpu_partial: crate::progress::new_results(),
            gpu_progress: crate::progress::new(),
            gpu_partial: crate::progress::new_results(),
            input_ui: suites_ui::InputUi::new(core_kinds.clone()),
            input_progress: crate::progress::new(),
            sampler: None,
            sampler_active: false,
            last_timeline: Vec::new(),
            sensor_notes: Vec::new(),
            sensor_probe: None,
            gpu_config: GpuBenchmarkConfig::default(),
            gpu_custom_k: 512,
            input_tester,
            input_state: InputState::Idle,
            input_waiting_until: None,
            input_start_time: Instant::now(),
            click_samples: Vec::new(),
            last_click_latency: None,
            ready_time_ticks: 0,
            timer_info: None,
            show_log_panel: true,
            log_text: String::new(),
        }
    }

    fn log(&mut self, msg: &str) {
        let timestamp = chrono::Local::now().format("%H:%M:%S%.3f");
        self.log_text = format!("[{}] {}\n{}", timestamp, msg, self.log_text);
        if self.log_text.len() > 60_000 {
            // Keep log bounded
            // Newest entries come first, so drop the tail (on a line boundary)
            let cut = self.log_text[..40_000].rfind('\n').unwrap_or(40_000);
            self.log_text.truncate(cut + 1);
        }
    }

    fn refresh_system_info(&mut self) {
        if self.info_refreshing {
            return;
        }
        self.info_refreshing = true;

        // Collected on a worker thread so the GUI never blocks; picked up in check_completed_tasks
        thread::spawn(|| {
            let sys = crate::system_info::collect_system_info();
            let virt = VirtualizationDetector::detect();
            COMPLETE_SYSINFO.lock().unwrap().replace((sys, virt));
        });
        // One-shot sensor probe (needs ~1 s of samples, so it runs on its own thread)
        thread::spawn(|| {
            let sampler = crate::sensors::Sampler::start(Duration::from_millis(250));
            thread::sleep(Duration::from_millis(1400));
            sampler.stop();
            if let Some(last) = sampler.timeline().pop() {
                *COMPLETE_PROBE.lock().unwrap() = Some((sampler.notes(), last));
            }
        });
    }

    /// Ask the running task (if any) to stop as soon as it reaches its next checkpoint
    fn stop_running(&mut self) {
        if self.running.lock().unwrap().is_some() {
            self.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            self.task_status = "Stopping...".to_string();
            self.log("Stop requested");
        }
    }

    fn is_running(&self) -> bool {
        self.running.lock().unwrap().is_some()
    }

    /// Stop button shown next to every Run button while a task is active
    fn stop_button(&mut self, ui: &mut Ui) {
        if self.is_running() {
            let stopping = self.cancel.load(std::sync::atomic::Ordering::Relaxed);
            if ui
                .add_enabled(!stopping, egui::Button::new(RichText::new("⏹ Stop").color(Color32::from_rgb(255, 120, 120))))
                .clicked()
            {
                self.stop_running();
            }
            let kind = self.running.lock().unwrap().as_ref().map(|r| (r.kind.clone(), r.started.elapsed().as_secs()));
            if let Some((kind, secs)) = kind {
                ui.spinner();
                ui.label(format!("{} running... {}s", kind, secs));
            }
        }
    }

    fn start_quick_memory_test(&mut self, size: usize) {
        if self.is_running() {
            return;
        }
        self.cancel.store(false, std::sync::atomic::Ordering::Relaxed);
        self.quick_mem_error = None;
        *self.running.lock().unwrap() = Some(RunningTaskState {
            kind: "quick memory test".to_string(),
            started: Instant::now(),
        });
        self.task_status = "Running quick memory test...".to_string();
        self.log(&format!("Started quick memory test ({})", human_size(size)));

        *self.mem_progress.lock().unwrap() = crate::memory_benchmark::MemProgress::default();
        let progress = self.mem_progress.clone();
        let running = self.running.clone();
        let cancel_flag = self.cancel.clone();
        thread::spawn(move || {
            let result = crate::memory_benchmark::quick_memory_latency_test(size, 20, cancel_flag, Some(progress));
            COMPLETE_QUICK_MEMORY.lock().unwrap().replace(result);
            if let Ok(mut guard) = running.lock() {
                *guard = None;
            }
        });
    }

    fn check_completed_tasks(&mut self) {
        self.finish_sampler_if_idle();
        if let Some(p) = COMPLETE_PROBE.lock().unwrap().take() {
            self.sensor_probe = Some(p);
        }
        if let Some((sys, virt)) = COMPLETE_SYSINFO.lock().unwrap().take() {
            self.info_refreshing = false;
            self.last_refresh = Instant::now();
            match sys {
                Ok(sys) => self.system_info = Some(sys),
                Err(e) => self.log(&format!("System info error: {}", e)),
            }
            match virt {
                Ok(virt) => self.virt_status = Some(virt),
                Err(e) => self.log(&format!("Virtualization detection error: {}", e)),
            }
        }

        if let Some(result) = COMPLETE_QUICK_MEMORY.lock().unwrap().take() {
            self.task_status = "Idle".to_string();
            match result {
                Ok(r) => {
                    self.log(&format!("Quick memory test finished in {:.0} ms", r.elapsed_ms));
                    self.quick_mem_result = Some(r);
                }
                Err(e) if cancel::is_cancel_error(&e) => {
                    self.log("Quick memory test cancelled");
                    self.quick_mem_error = Some("Cancelled".to_string());
                }
                Err(e) => {
                    self.log(&format!("Quick memory test error: {}", e));
                    self.quick_mem_error = Some(e.to_string());
                }
            }
        }

        // Check for completed tasks
        if let Some(result) = COMPLETE_MEMORY_RESULT.lock().unwrap().take() {
            self.task_status = "Idle".to_string();
            match result {
                Ok(summary) => {
                    self.last_memory_result = Some(summary.clone());
                    self.log(&format!("Memory benchmark complete: {} results", summary.results.len()));
                }
                Err(e) if cancel::is_cancel_error(&e) => self.log("Memory benchmark cancelled"),
                Err(e) => self.log(&format!("Memory benchmark error: {}", e)),
            }
        }
        
        if let Some(result) = COMPLETE_CPU_RESULT.lock().unwrap().take() {
            self.task_status = "Idle".to_string();
            match result {
                Ok(summary) => {
                    self.last_cpu_result = Some(summary.clone());
                    self.log(&format!("CPU benchmark complete: {} results", summary.results.len()));
                }
                Err(e) if cancel::is_cancel_error(&e) => self.log("CPU benchmark cancelled"),
                Err(e) => self.log(&format!("CPU benchmark error: {}", e)),
            }
        }
        
        if let Some(result) = COMPLETE_GPU_RESULT.lock().unwrap().take() {
            self.task_status = "Idle".to_string();
            match result {
                Ok(summary) => {
                    self.last_gpu_result = Some(summary.clone());
                    self.log(&format!("GPU benchmark complete: {} results", summary.results.len()));
                }
                Err(e) if cancel::is_cancel_error(&e) => self.log("GPU benchmark cancelled"),
                Err(e) => self.log(&format!("GPU benchmark error: {}", e)),
            }
        }
        
        if let Some(result) = COMPLETE_INPUT_RESULT.lock().unwrap().take() {
            self.task_status = "Idle".to_string();
            match result {
                Ok(summary) => {
                    self.last_input_result = Some(summary.clone());
                    self.log(&format!("Input benchmark complete: {} results", summary.results.len()));
                }
                Err(e) if cancel::is_cancel_error(&e) => self.log("Input benchmark cancelled"),
                Err(e) => self.log(&format!("Input benchmark error: {}", e)),
            }
        }
        
        // Auto-refresh system info every 5 seconds
        if !self.info_refreshing && self.last_refresh.elapsed() > Duration::from_secs(30) {
            self.refresh_system_info();
        }
    }

    fn log_last_result(&mut self) {
        let logger = match &self.logger {
            Some(l) => l,
            None => {
                self.log("No result logger available");
                return;
            }
        };
        
        let sys_info = match &self.system_info {
            Some(s) => s,
            None => {
                self.log("No system info collected yet");
                return;
            }
        };
        
        let (test_type, config, results) = if let Some(r) = &self.last_memory_result {
            ("memory", json!(r), json!(r))
        } else if let Some(r) = &self.last_cpu_result {
            ("cpu", json!(r), json!(r))
        } else if let Some(r) = &self.last_input_result {
            ("input", json!(r), json!(r))
        } else if let Some(r) = &self.last_gpu_result {
            ("gpu", json!(r), json!(r))
        } else {
            self.log("Run a benchmark first to log results");
            return;
        };
        
        let metadata: HashMap<String, String> = HashMap::new();
        
        match logger.log_result(test_type, sys_info, &config, &results, metadata) {
            Ok(verified) => {
                let verification = logger.verify_result(&verified);
                self.last_logged = Some((verified.clone(), verification.clone()));
                self.log(&format!(
                    "Result logged and verified: {} (signature: {}...)",
                    verification.message,
                    &verified.signature.sig[..16]
                ));
            }
            Err(e) => {
                self.log(&format!("Failed to log result: {}", e));
            }
        }
    }

    /// Handle the interactive input latency test (click reaction)
    fn handle_input_test(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let now = self.input_start_time.elapsed().as_secs_f64();
        
        // Reserve the test area first so pointer presses can be limited to it
        let rect = ui.available_rect_before_wrap();
        let _response = ui.allocate_rect(rect, Sense::click());

        // React on press (not release) to measure true input latency
        let space_pressed = ui.input(|i| i.key_pressed(egui::Key::Space));
        let pointer_clicked = ui.input(|i| {
            i.pointer.primary_pressed()
                && i.pointer.interact_pos().map_or(false, |p| rect.contains(p))
        });
        
        // State machine
        match self.input_state {
            InputState::Idle | InputState::Result | InputState::FalseStart => {
                if space_pressed || pointer_clicked {
                    // Start: random delay 1.5-4 seconds
                    let mut rng = rand::thread_rng();
                    let delay = rng.gen_range(1.5..4.0); // seconds
                    self.input_waiting_until = Some(now + delay);
                    self.input_state = InputState::Waiting;
                    self.last_click_latency = None;
                }
            }
            InputState::Waiting => {
                if space_pressed || pointer_clicked {
                    self.input_state = InputState::FalseStart;
                    self.last_click_latency = None;
                }
                
                if self.input_waiting_until.map(|t| now >= t).unwrap_or(false) {
                    // Switch to Ready - record trigger time with QPC
                    self.input_state = InputState::Ready;
                    self.input_tester.trigger_stimulus(InputTestMode::MouseClick);
                    self.ready_time_ticks = self.input_tester.timer.now_ticks();
                }
            }
            InputState::Ready => {
                if space_pressed || pointer_clicked {
                    // Record click time - compute latency
                    let click_ticks = self.input_tester.timer.now_ticks();
                    let latency_ms = self
                        .input_tester
                        .timer
                        .ticks_to_ms_f64(click_ticks.saturating_sub(self.ready_time_ticks));
                    self.last_click_latency = Some(latency_ms);
                    self.click_samples.push(latency_ms);
                    self.input_state = InputState::Result;
                    self.log(&format!("Click reaction: {:.3} ms (sample #{})", latency_ms, self.click_samples.len()));
                }
            }
        }
        
        // Render the test area
        let color = match self.input_state {
            InputState::Idle => Color32::from_rgb(40, 40, 40),
            InputState::Waiting => Color32::from_rgb(180, 50, 50),
            InputState::Ready => Color32::from_rgb(50, 190, 50),
            InputState::Result => Color32::from_rgb(50, 50, 180),
            InputState::FalseStart => Color32::from_rgb(150, 150, 50),
        };
        
        ui.painter().rect_filled(rect, 8.0, color);
        
        let (text, text_color) = match self.input_state {
            InputState::Idle => ("Click or press SPACE to start".to_string(), Color32::WHITE),
            InputState::Waiting => ("Wait for GREEN...".to_string(), Color32::WHITE),
            InputState::Ready => ("CLICK NOW!".to_string(), Color32::WHITE),
            InputState::Result => {
                let lat = self.last_click_latency.unwrap_or(0.0);
                let avg = if self.click_samples.is_empty() { 0.0 } else {
                    self.click_samples.iter().sum::<f64>() / self.click_samples.len() as f64
                };
                let text = format!(
                    "Latency: {:.3} ms\nAverage ({}): {:.3} ms\n\nPress SPACE to go again",
                    lat, self.click_samples.len(), avg
                );
                (text, Color32::WHITE)
            }
            InputState::FalseStart => ("Too early! Click / SPACE to try again".to_string(), Color32::WHITE),
        };
        
        let center = rect.center();
        ui.painter()
            .text(center, egui::Align2::CENTER_CENTER, &text, egui::FontId::proportional(28.0), text_color);

        // The state machine is time driven, so keep frames coming while a round is active
        if matches!(self.input_state, InputState::Waiting | InputState::Ready) {
            ctx.request_repaint();
        }
    }

    fn render_dashboard(&mut self, ui: &mut Ui) {
        ui.heading("System Dashboard");
        ui.separator();
        
        // System info
        if let Some(sys) = &self.system_info {
            egui::Grid::new("sysinfo").show(ui, |ui| {
                ui.label("CPU:");
                ui.label(RichText::new(&sys.cpu.name).strong());
                ui.end_row();
                
                ui.label("Cores/Threads:");
                ui.label(format!("{} physical / {} logical (P: {}, E: {})", 
                    sys.cpu.cores, sys.cpu.threads, sys.cpu.p_cores, sys.cpu.e_cores));
                ui.end_row();
                
                ui.label("Architecture:");
                ui.label(format!("{} ({})", sys.cpu.architecture, sys.cpu.microarchitecture));
                ui.end_row();
                
                ui.label("Features:");
                ui.label(sys.cpu.features.join(", "));
                ui.end_row();
                
                ui.label("Memory:");
                ui.label(format!("{:.1} GB total, {:.1} GB available",
                    sys.memory.total as f64 / 1_073_741_824.0,
                    sys.memory.available as f64 / 1_073_741_824.0));
                ui.end_row();
                
                if let Some(gpu) = &sys.gpu {
                    ui.label("GPU:");
                    ui.label(&gpu.name);
                    ui.end_row();
                }
                
                ui.label("OS:");
                ui.label(&sys.os.name);
                ui.end_row();
            });
        } else {
            ui.label("Collecting system information...");
        }
        
        ui.separator();
        
        // Virtualization summary
        ui.heading("Virtualization Status");
        if let Some(virt) = &self.virt_status {
            egui::Grid::new("virt").show(ui, |ui| {
                ui.label("BIOS VT-x/SVM:");
                ui.label(format!("{}", virt.bios.vt_x_enabled || virt.bios.svm_enabled));
                ui.end_row();
                
                if let Some(w) = &virt.windows {
                    ui.label("Hyper-V:");
                    ui.label(format!("{}", w.hyper_v_enabled));
                    ui.end_row();
                    
                    ui.label("VBS:");
                    ui.label(format!("{}", w.vbs_enabled));
                    ui.end_row();
                    
                    ui.label("Memory Integrity:");
                    ui.label(format!("{}", w.memory_integrity_enabled));
                    ui.end_row();
                }
                
                if let Some(l) = &virt.linux {
                    ui.label("KVM:");
                    ui.label(format!("{}", l.kvm_enabled));
                    ui.end_row();
                }
            });
        }
        
        ui.separator();
        ui.heading("Sensors");
        match &self.sensor_probe {
            None => {
                ui.label("Probing sensors...");
            }
            Some((notes, snap)) => {
                egui::Grid::new("sensor_probe").show(ui, |ui| {
                    if let Some(t) = snap.cpu_package_c {
                        ui.label("CPU package:");
                        ui.label(format!("{:.0} °C", t));
                        ui.end_row();
                    }
                    if !snap.core_temps_c.is_empty() {
                        ui.label("Per-core temps:");
                        let hottest = snap.core_temps_c.iter().fold(0.0f32, |m, c| m.max(c.1));
                        ui.label(format!("{} cores, hottest {:.0} °C", snap.core_temps_c.len(), hottest));
                        ui.end_row();
                    }
                    if let Some(g) = &snap.gpu {
                        ui.label("GPU:");
                        let mut parts = vec![g.name.clone()];
                        if let Some(t) = g.temp_c { parts.push(format!("{:.0} °C", t)); }
                        if let (Some(u), Some(tot)) = (g.vram_used_mb, g.vram_total_mb) {
                            parts.push(format!("VRAM {:.0} / {:.0} MB", u, tot));
                        }
                        ui.label(parts.join(" · "));
                        ui.end_row();
                    }
                    ui.label("RAM:");
                    ui.label(format!("{:.1} / {:.1} GB", snap.ram_used_mb / 1024.0, snap.ram_total_mb / 1024.0));
                    ui.end_row();
                });
                for n in notes {
                    ui.label(RichText::new(format!("• {}", n)).weak().small());
                }
            }
        }
        ui.separator();
        ui.label(format!("Last refresh: {:.0} seconds ago", self.last_refresh.elapsed().as_secs()));
    }

    fn render_gpu_tab(&mut self, ui: &mut Ui) {
        ui.heading("GPU Benchmark (Vulkan)");
        ui.separator();
        
        ui.group(|ui| {
            ui.add_enabled_ui(!self.is_running(), |ui| {
                ui.label("Workload sizes (elements per dispatch):");
                ui.horizontal_wrapped(|ui| {
                    for shift in [16u32, 18, 20, 22, 24, 26] {
                        let size = 1u64 << shift;
                        let mut on = self.gpu_config.workload_sizes.contains(&size);
                        if ui.checkbox(&mut on, human_count(size)).changed() {
                            if on {
                                self.gpu_config.workload_sizes.push(size);
                                self.gpu_config.workload_sizes.sort_unstable();
                            } else {
                                self.gpu_config.workload_sizes.retain(|&s| s != size);
                            }
                        }
                    }
                    ui.separator();
                    ui.label("Custom (thousands):");
                    ui.add(egui::DragValue::new(&mut self.gpu_custom_k).range(1..=1_000_000));
                    if ui.small_button("Add").clicked() {
                        let size = self.gpu_custom_k as u64 * 1000;
                        if !self.gpu_config.workload_sizes.contains(&size) {
                            self.gpu_config.workload_sizes.push(size);
                            self.gpu_config.workload_sizes.sort_unstable();
                        }
                    }
                });
                if self.gpu_config.workload_sizes.is_empty() {
                    ui.colored_label(Color32::YELLOW, "⚠ Select at least one size");
                }
            });
            ui.horizontal(|ui| {
                ui.label("Iterations:");
                ui.add(egui::DragValue::new(&mut self.gpu_config.iterations).range(1..=10000));
                ui.label("Warmup:");
                ui.add(egui::DragValue::new(&mut self.gpu_config.warmup_iterations).range(0..=1000));
            });

            ui.horizontal(|ui| {
                if ui.add_enabled(!self.is_running() && !self.gpu_config.workload_sizes.is_empty(), egui::Button::new("Run GPU Benchmark")).clicked() {
                    self.start_gpu_benchmark();
                }
                self.stop_button(ui);
            });
        });

        self.gpu_progress_and_partial(ui);

        if let Some(summary) = &self.last_gpu_result {
            ui.separator();
            ui.heading("Latest Results");

            let vulkan_info = &summary.vulkan_info;
            ui.label(format!("Vulkan device: {} ({})", vulkan_info.device_name, vulkan_info.device_type));
            ui.label(format!("API: {} Driver: {}", vulkan_info.api_version, vulkan_info.driver_version));

            egui::ScrollArea::vertical().show(ui, |ui| {
                for result in summary.results.iter().take(20) {
                    ui.horizontal(|ui| {
                        ui.label(format!("{} elements", result.workload_size));
                        ui.label(format!("{:.3} ms avg", result.avg_latency_ms));
                        ui.label(format!("p95: {:.3} ms", result.percentile_95_ms));
                        ui.label(format!("p99: {:.3} ms", result.percentile_99_ms));
                        ui.label(format!("{:.1} GOPS", result.throughput_geops));
                    });
                }
            });
        } else {
            ui.label("No GPU results yet. Requires a Vulkan driver (a software driver such as lavapipe also works).");
        }
    }

    fn render_input_tab(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        ui.heading("Input Latency Test");
        ui.separator();
        
        // Timer resolution info
        if self.timer_info.is_none() {
            if let Ok(info) = measure_timer_resolution() {
                self.timer_info = Some(info);
            }
        }
        if let Some(info) = &self.timer_info {
            ui.horizontal(|ui| {
                ui.label(format!("Timer: {} Hz ({} ns resolution)", info.frequency_hz, info.resolution_ns));
                ui.label(format!("min measurable: {} ns", info.min_measurable_interval_ns));
                ui.label(format!("overhead: {} ns", info.overhead_ns));
            });
        }
        
        // Interactive test area
        let available = ui.available_size();
        let test_height = (available.y * 0.5).max(200.0);
        egui::ScrollArea::both().show(ui, |ui| {
            ui.vertical(|ui| {
                ui.allocate_ui(egui::vec2(available.x, test_height), |ui| {
                    self.handle_input_test(ui, ctx);
                });
                
                // Statistics
                ui.separator();
                ui.horizontal(|ui| {
                    if !self.click_samples.is_empty() {
                        let avg = self.click_samples.iter().sum::<f64>() / self.click_samples.len() as f64;
                        let mut sorted = self.click_samples.clone();
                        sorted.sort_by(|a, b| a.total_cmp(b));
                        let min = sorted[0];
                        let max = *sorted.last().unwrap();
                        let p95 = sorted[((sorted.len() as f64 * 0.95) as usize).min(sorted.len() - 1)];
                        
                        ui.label(format!("Samples: {}", self.click_samples.len()));
                        ui.label(format!("Min: {:.3} ms", min));
                        ui.label(format!("Avg: {:.3} ms", avg));
                        ui.label(format!("p95: {:.3} ms", p95));
                        ui.label(format!("Max: {:.3} ms", max));
                    }
                    if ui.button("Clear Samples").clicked() {
                        self.click_samples.clear();
                        self.last_click_latency = None;
                        self.input_state = InputState::Idle;
                    }
                    if ui.add_enabled(!self.is_running(), egui::Button::new("Run Full Input Suite")).clicked() {
                        self.start_input_benchmark();
                    }
                    self.stop_button(ui);
                });
                self.input_suite_options(ui);
            });
        });
        
        if let Some(summary) = &self.last_input_result {
            ui.separator();
            ui.heading("Full Suite Results");
            for result in &summary.results {
                ui.horizontal(|ui| {
                    ui.label(format!("{:?}", result.mode));
                    if let Some(c) = result.core {
                        ui.label(format!("core {}", c));
                    }
                    ui.label(format!("avg: {:.3} ms", result.avg_latency_ms));
                    ui.label(format!("p99: {:.3} ms", result.percentile_99_ms));
                    if let Some(rate) = result.polling_rate_hz {
                        ui.label(format!("polling: {:.0} Hz", rate));
                    }
                });
            }
        }
    }

    fn render_virtualization_tab(&mut self, ui: &mut Ui) {
        ui.heading("Virtualization Settings");
        ui.separator();
        
        if let Some(virt) = &self.virt_status {
            match virt.platform {
                crate::virtualization::Platform::Windows => {
                    ui.label("Windows: To reduce latency, disable these features:");
                    if let Some(w) = &virt.windows {
                        self.virt_toggle(ui, "Hyper-V", w.hyper_v_enabled, "Windows Features > uncheck Hyper-V");
                        self.virt_toggle(ui, "VBS (Virtualization-Based Security)", w.vbs_enabled, "Group Policy: Computer > Admin > System > Device Guard");
                        self.virt_toggle(ui, "Memory Integrity (HVCI)", w.memory_integrity_enabled, "Windows Security > Device Security > Core Isolation");
                        self.virt_toggle(ui, "WSL", w.wsl_enabled, "Turn off Windows Subsystem for Linux");
                        self.virt_toggle(ui, "Virtual Machine Platform", w.virtual_machine_platform_enabled, "Windows Features");
                    }
                    self.virt_toggle(ui, "BIOS VT-x/VT-d", virt.bios.vt_x_enabled || virt.bios.vt_d_enabled, "BIOS: CPU > Intel Virtualization Technology");
                }
                crate::virtualization::Platform::Linux => {
                    ui.label("Linux: To reduce latency, disable virtualization:");
                    if let Some(l) = &virt.linux {
                        self.virt_toggle(ui, "KVM", l.kvm_enabled, "modprobe -r kvm_intel / kvm_amd");
                        self.virt_toggle(ui, "VMX/SVM in CPU", l.vmx_svm_in_cpuinfo, "BIOS: virtualization support");
                        self.virt_toggle(ui, "Kernel cmdline disabled", l.kernel_cmdline_virt_disabled, "kvm_intel.enable_virt_at_load=0");
                    }
                    self.virt_toggle(ui, "BIOS VT-x/SVM", virt.bios.vt_x_enabled || virt.bios.svm_enabled, "BIOS: CPU virtualization");
                    self.virt_toggle(ui, "IOMMU", virt.bios.iommu_enabled, "BIOS: IOMMU / kernel: iommu=off");
                }
                crate::virtualization::Platform::Unknown => {
                    ui.label("Unknown platform");
                }
            }
            
            ui.separator();
            ui.heading("Recommendations");
            for rec in &virt.recommendations {
                ui.label(format!("• {}", rec));
            }
            
            ui.separator();
            ui.collapsing("How to disable on this platform", |ui| {
                let instructions = if virt.platform == crate::virtualization::Platform::Windows {
                    VirtualizationDetector::get_windows_disable_instructions()
                } else {
                    VirtualizationDetector::get_linux_disable_instructions()
                };
                for line in &instructions {
                    ui.label(line);
                }
            });
        }
    }

    fn virt_toggle(&self, ui: &mut Ui, label: &str, enabled: bool, hint: &str) {
        ui.horizontal(|ui| {
            let icon = if enabled { "🔵" } else { "⚪" };
            ui.label(format!("{} {}", icon, label));
            ui.label(RichText::new(if enabled { "ENABLED (latency overhead)" } else { "disabled (optimal)" })
                .color(if enabled { Color32::YELLOW } else { Color32::GREEN }));
            ui.label(RichText::new(format!("({})", hint)).weak().small());
        });
    }

    fn render_results_tab(&mut self, ui: &mut Ui) {
        ui.heading("Results & Verification");
        ui.separator();
        
        ui.group(|ui| {
            if ui.button("Log Latest Result (signed)").clicked() {
                self.log_last_result();
            }
            
            if ui.button("Verify Entire Log (signatures + chain)").clicked() {
                match self.logger.as_ref().map(|l| l.verify_log()) {
                    Some(Ok(v)) => self.log(&format!("Log verification: {} - {}", if v.valid { "OK" } else { "FAILED" }, v.message)),
                    Some(Err(e)) => self.log(&format!("Log verification error: {}", e)),
                    None => self.log("No result logger available"),
                }
            }

            if ui.button("Refresh Results").clicked() {
                match self.logger.as_ref().map(|l| l.load_all_results()) {
                    Some(Ok(results)) => {
                        self.log(&format!("Loaded {} results from master log", results.len()));
                        self.all_results = Some(results);
                    }
                    Some(Err(e)) => self.log(&format!("Failed to load results: {}", e)),
                    None => self.log("No result logger available"),
                }
            }
        });
        
        if let Some((verified, verification)) = &self.last_logged {
            ui.separator();
            ui.heading("Last Logged Result");
            ui.label(format!("Valid: {}", verification.valid));
            ui.label(format!("Message: {}", verification.message));
            ui.label(format!("Signature: {}", &verified.signature.sig));
            ui.label(format!("Public key: {}", &verified.signature.public_key));
            ui.label(format!("Salt: {}", &verified.header.salt));
            ui.label(format!("Nonce: {}", &verified.header.nonce));
            ui.label(format!("App hash: {}", &verified.header.app_hash));
            
            if ui.button("Copy Shareable Summary").clicked() {
                let summary = generate_shareable_summary(verified, verification);
                ui.ctx().copy_text(summary);
                self.log("Shareable summary copied to clipboard");
            }
        }
        
        if let Some(results) = &self.all_results {
            ui.separator();
            ui.heading(format!("All Results ({})", results.len()));
            egui::ScrollArea::vertical().show(ui, |ui| {
                for (i, r) in results.iter().rev().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(format!("#{}: {}", i + 1, r.payload.test_type));
                        ui.label(&r.header.timestamp);
                        ui.label(&r.header.app_version);
                        ui.label(format!("sig: {}...", &r.signature.sig[..16]));
                    });
                }
            });
        }
    }

    fn render_log_panel(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label("Log:");
            ui.checkbox(&mut self.show_log_panel, "show");
            if ui.button("Clear").clicked() {
                self.log_text.clear();
            }
        });
        
        if self.show_log_panel {
            egui::ScrollArea::vertical()
                .auto_shrink([false, true])
                .max_height(150.0)
                .show(ui, |ui| {
                    for line in self.log_text.lines().take(100) {
                        ui.label(egui::RichText::new(line).monospace().size(11.0));
                    }
                });
        }
    }
}

impl eframe::App for LatencyTesterApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.check_completed_tasks();

        // Poll for finished background work even when the user is idle
        ctx.request_repaint_after(Duration::from_millis(200));
        
        // Refresh system info on first frame
        if self.system_info.is_none() && !self.info_refreshing {
            self.refresh_system_info();
        }
        
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("⚡ Latency Tester Suite v1.0");
                
                if self.is_running() {
                    self.stop_button(ui);
                }
                
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(format!("{}", self.task_status));
                    if ui.button("🔄 Refresh Info").clicked() {
                        self.refresh_system_info();
                    }
                });
            });
        });
        
        egui::TopBottomPanel::bottom("bottom").show(ctx, |ui| {
            self.render_log_panel(ui);
        });
        
        egui::SidePanel::left("sidebar").resizable(true).show(ctx, |ui| {
            ui.heading("Tabs:");
            ui.separator();
            if ui.button("📊 Dashboard").clicked() { self.tab = Tab::Dashboard; }
            if ui.button("🧠 Memory").clicked() { self.tab = Tab::Memory; }
            if ui.button("⚙️ CPU").clicked() { self.tab = Tab::Cpu; }
            if ui.button("🎮 GPU (Vulkan)").clicked() { self.tab = Tab::Gpu; }
            if ui.button("🖱️ Input Latency").clicked() { self.tab = Tab::Input; }
            if ui.button("🔒 Virtualization").clicked() { self.tab = Tab::Virtualization; }
            if ui.button("📈 Results & Graphs").clicked() { self.tab = Tab::Graphs; }
            if ui.button("📋 Signed Log & Verify").clicked() { self.tab = Tab::Results; }
        });
        
        egui::CentralPanel::default().show(ctx, |ui| {
            match self.tab {
                Tab::Dashboard => self.render_dashboard(ui),
                Tab::Memory => self.render_memory_tab(ui),
                Tab::Cpu => self.render_cpu_tab(ui),
                Tab::Gpu => self.render_gpu_tab(ui),
                Tab::Input => self.render_input_tab(ui, ctx),
                Tab::Virtualization => self.render_virtualization_tab(ui),
                Tab::Graphs => self.render_graphs_tab(ui),
                Tab::Results => self.render_results_tab(ui),
            }
        });
    }
}

/// Human-readable size
fn human_count(n: u64) -> String {
    if n >= 1_000_000 { format!("{:.1}M", n as f64 / 1_048_576.0) } else { format!("{}K", n / 1024) }
}

fn human_size(bytes: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = 1024 * KB;
    const GB: usize = 1024 * MB;
    
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.0} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

// Static channels for communicating with background task threads
use std::sync::Mutex;

pub(crate) static COMPLETE_MEMORY_RESULT: Mutex<Option<Result<MemoryBenchmarkSummary, anyhow::Error>>> = 
    Mutex::new(None);
pub(crate) static COMPLETE_CPU_RESULT: Mutex<Option<Result<crate::cpu_benchmark::CpuBenchmarkSummary, anyhow::Error>>> = 
    Mutex::new(None);
pub(crate) static COMPLETE_GPU_RESULT: Mutex<Option<Result<crate::gpu_benchmark::GpuBenchmarkSummary, anyhow::Error>>> = 
    Mutex::new(None);
pub(crate) static COMPLETE_INPUT_RESULT: Mutex<Option<Result<crate::input_latency::InputLatencySummary, anyhow::Error>>> = 
    Mutex::new(None);
static COMPLETE_SYSINFO: Mutex<
    Option<(
        Result<SystemInfo, anyhow::Error>,
        Result<VirtualizationStatus, anyhow::Error>,
    )>,
> = Mutex::new(None);
static COMPLETE_QUICK_MEMORY: Mutex<Option<Result<QuickMemoryResult, anyhow::Error>>> = Mutex::new(None);
static COMPLETE_PROBE: Mutex<Option<(Vec<String>, crate::sensors::Snapshot)>> = Mutex::new(None);

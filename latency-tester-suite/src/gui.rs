//! GUI for the Latency Tester Suite using egui/eframe
//! 
//! Tabs: Dashboard, Memory, CPU, GPU, Input Latency, Virtualization, Results

use eframe::egui;
use egui::{Color32, RichText, Ui};
use std::collections::HashMap;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

mod core_select;
mod input_ui;
mod memory_ui;
mod polling_ui;
mod aim_ui;
mod results_ui;
mod run_all_ui;
mod suites_ui;

use crate::cancel::{self, CancelFlag};
use crate::gpu_benchmark::GpuBenchmarkConfig;
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
    last_mem_config: Option<MemoryBenchmarkConfig>,
    last_cpu_config: Option<crate::cpu_benchmark::CpuBenchmarkConfig>,
    last_gpu_config: Option<GpuBenchmarkConfig>,
    /// Further passes of a combined "run all" (each core on its own, ...)
    mem_extra_configs: Vec<MemoryBenchmarkConfig>,
    cpu_extra_configs: Vec<crate::cpu_benchmark::CpuBenchmarkConfig>,
    run_all: run_all_ui::RunAllUi,
    
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
    last_phases: Vec<crate::sensors::Phase>,
    sensor_notes: Vec<String>,
    sensor_probe: Option<(Vec<String>, crate::sensors::Snapshot)>,
    
    // GPU config
    gpu_config: GpuBenchmarkConfig,
    gpu_custom_k: u32,
    
    // Input latency interactive state
    show_log_panel: bool,
    input_test: input_ui::InputTestUi,
    /// Input tab, "Mouse polling"
    polling: polling_ui::PollingUi,
    /// Input tab, "Reflex game"
    aim: aim_ui::AimUi,
    display_whole_area: bool,
    log_text: String,
    log_saved_note: String,
    web_server: Option<crate::server::Server>,
    web_port: u16,
    web_lan: bool,
    web_note: String,
    results_dir_path: std::path::PathBuf,
    /// Start of the previous frame (the frame cap while tests run)
    last_frame: Instant,
    /// Finest system timer resolution, held while the app runs (Windows)
    _timer_res: crate::timer_info::HighResolutionTimer,
    /// Clock sources and timer resolution, read after the request above
    timers: crate::timer_info::TimerSources,
    /// How the window is drawn (Vulkan via wgpu or OpenGL), shown to the user
    renderer: String,
    is_admin: bool,
    /// Result of the LibreHardwareMonitor download, filled by its worker thread
    lhm_fetch: Arc<std::sync::Mutex<Option<Result<String, String>>>>,
    lhm_fetching: bool,
    lhm_note: String,
    /// Installed PawnIO driver version (Windows; LibreHardwareMonitor ≥ 0.9.5 needs it)
    pawnio: Option<String>,
    pawnio_installing: bool,
}

/// See `LatencyTesterApp::session_snapshot`
struct SessionSnapshot {
    mem: Vec<crate::memory_benchmark::MemoryBenchmarkResult>,
    mem_planned: usize,
    cpu: Vec<crate::cpu_benchmark::CpuBenchmarkResult>,
    gpu: Vec<crate::gpu_benchmark::GpuBenchmarkResult>,
    trials: Vec<(crate::input_test::RunSummary, crate::rig::RigCalibration)>,
    virtualization: Option<serde_json::Value>,
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
        Self::with_results_dir(config_dir)
    }

    /// The app without a window, saving into `config_dir` (also used by the tests)
    fn with_results_dir(config_dir: std::path::PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&config_dir);

        let logger = ResultLogger::new(
            env!("CARGO_PKG_VERSION").to_string(),
            config_dir.to_string_lossy().to_string(),
        ).ok();

        let core_kinds = crate::topology::cached_core_kinds().map(|k| k.to_vec());

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
            last_mem_config: None,
            last_cpu_config: None,
            last_gpu_config: None,
            mem_extra_configs: Vec::new(),
            cpu_extra_configs: Vec::new(),
            run_all: run_all_ui::RunAllUi::new(),
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
            last_phases: Vec::new(),
            sensor_notes: Vec::new(),
            sensor_probe: None,
            gpu_config: GpuBenchmarkConfig::default(),
            gpu_custom_k: 512,
            show_log_panel: true,
            input_test: input_ui::InputTestUi::new(),
            polling: polling_ui::PollingUi::new(),
            aim: aim_ui::AimUi::new(),
            display_whole_area: false,
            log_text: String::new(),
            log_saved_note: String::new(),
            web_server: None,
            web_port: 8787,
            web_lan: false,
            web_note: String::new(),
            results_dir_path: config_dir.clone(),
            last_frame: Instant::now(),
            _timer_res: crate::timer_info::HighResolutionTimer::acquire(),
            timers: crate::timer_info::query(),
            renderer: String::new(),
            is_admin: crate::lhm::is_admin(),
            lhm_fetch: Arc::new(std::sync::Mutex::new(None)),
            lhm_fetching: false,
            lhm_note: String::new(),
            pawnio: crate::lhm::pawnio_version(),
            pawnio_installing: false,
        }
    }

    pub fn set_renderer(&mut self, r: String) {
        self.log(&format!("Drawing with: {}", r));
        self.renderer = r;
    }

    /// Folder holding the signed result files (next to the executable)
    fn results_dir(&self) -> std::path::PathBuf {
        self.results_dir_path.clone()
    }

    fn log(&mut self, msg: &str) {
        let timestamp = chrono::Local::now().format("%H:%M:%S%.3f");
        if !self.log_text.is_empty() {
            self.log_text.push('\n');
        }
        self.log_text.push_str(&format!("[{}] {}", timestamp, msg));
        if self.log_text.len() > 200_000 {
            // Keep the log bounded: drop the oldest lines (on a line boundary)
            let mut cut = self.log_text.len() - 150_000;
            while !self.log_text.is_char_boundary(cut) {
                cut += 1;
            }
            let cut = self.log_text[cut..].find('\n').map(|i| cut + i + 1).unwrap_or(cut);
            self.log_text.drain(..cut);
        }
    }

    fn refresh_system_info(&mut self) {
        if self.info_refreshing {
            return;
        }
        self.info_refreshing = true;

        // Collected on a worker thread so the GUI never blocks; picked up in check_completed_tasks
        thread::spawn(|| {
            let sys = crate::system_info::collect_system_info_fresh();
            let virt = VirtualizationDetector::detect();
            COMPLETE_SYSINFO.lock().unwrap().replace((sys, virt));
        });
        // One-shot sensor probe (needs ~1 s of samples, so it runs on its own thread)
        thread::spawn(|| {
            let sampler = crate::sensors::Sampler::start(Duration::from_millis(250));
            // The Windows sensor helper (PowerShell) needs a few seconds before its first reading
            thread::sleep(Duration::from_millis(if cfg!(target_os = "windows") { 4500 } else { 1400 }));
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
        // follow the core reservation (tests may have moved the app away from the core they measure)
        crate::app_core::apply_gui();
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

    /// Sign and save the current session (or one suite of it) as a single record
    fn log_session(&mut self, scope: crate::session::Scope) {
        self.log_session_from(scope, 0, None);
    }

    /// Copies of what a session record is built from, so no lock is held while it is signed
    fn session_snapshot(&self, skip_trials: usize) -> SessionSnapshot {
        let (mem, mem_planned) = {
            let p = self.mem_progress.lock().unwrap();
            (p.completed.clone(), p.total_tests)
        };
        SessionSnapshot {
            mem,
            mem_planned,
            cpu: self.cpu_partial.lock().unwrap().clone(),
            gpu: self.gpu_partial.lock().unwrap().clone(),
            trials: self.input_test.runs.iter().skip(skip_trials).map(|r| (r.summary.clone(), r.cal.clone())).collect(),
            virtualization: self.virt_status.as_ref().and_then(|v| serde_json::to_value(v).ok()),
        }
    }

    fn session_data<'a>(&'a self, snap: &'a SessionSnapshot, run_info: Option<serde_json::Value>) -> crate::session::SessionData<'a> {
        crate::session::SessionData {
            memory_config: self.last_mem_config.as_ref(),
            memory: &snap.mem,
            memory_planned: snap.mem_planned,
            cpu_config: self.last_cpu_config.as_ref(),
            cpu_topology: self.last_cpu_result.as_ref().map(|s| &s.core_topology),
            cpu: &snap.cpu,
            gpu_config: self.last_gpu_config.as_ref(),
            gpu_vulkan: self.last_gpu_result.as_ref().map(|s| &s.vulkan_info),
            gpu: &snap.gpu,
            input_suite: self.last_input_result.as_ref(),
            trials: &snap.trials,
            timeline: &self.last_timeline,
            phases: &self.last_phases,
            sensor_notes: &self.sensor_notes,
            virtualization: snap.virtualization.clone(),
            timers: serde_json::to_value(&self.timers).ok(),
            calibration: Some(&self.input_test.cal),
            memory_extra_configs: &self.mem_extra_configs,
            cpu_extra_configs: &self.cpu_extra_configs,
            run_info,
            mouse_polling: &self.polling.results,
            reflex_game: &self.aim.results,
        }
    }

    /// Like `log_session`, leaving out the first `skip_trials` manual click / key runs. Returns the
    /// signed record and the file it was saved to.
    fn log_session_from(
        &mut self,
        scope: crate::session::Scope,
        skip_trials: usize,
        run_info: Option<serde_json::Value>,
    ) -> Option<(VerifiedResult, std::path::PathBuf)> {
        use crate::session;
        let Some(logger) = &self.logger else {
            self.log("No result logger available");
            return None;
        };
        let Some(sys_info) = &self.system_info else {
            self.log("No system info collected yet");
            return None;
        };
        let snap = self.session_snapshot(skip_trials);
        let data = self.session_data(&snap, run_info);
        if !session::has_data(&data, scope) {
            self.log("Nothing to log yet: run a test first");
            return None;
        }
        let (config, results) = session::build(&data, scope);
        match logger.log_result(scope.test_type(), sys_info, &config, &results, HashMap::new()) {
            Ok(verified) => {
                let verification = logger.verify_result(&verified);
                let path = logger.result_path(&verified);
                let parts: Vec<String> = results.as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
                self.last_logged = Some((verified.clone(), verification.clone()));
                self.log(&format!(
                    "Logged {} ({}): {} (signature {}…)",
                    scope.test_type(),
                    parts.join(", "),
                    verification.message,
                    &verified.signature.sig[..16]
                ));
                Some((verified, path))
            }
            Err(e) => {
                self.log(&format!("Failed to log result: {}", e));
                None
            }
        }
    }

    fn render_dashboard(&mut self, ui: &mut Ui) {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            self.render_run_all(ui);
            ui.add_space(8.0);
            self.render_dashboard_body(ui);
        });
    }

    fn render_dashboard_body(&mut self, ui: &mut Ui) {
        ui.heading("System Dashboard");
        ui.separator();
        
        // System info
        if let Some(sys) = &self.system_info {
            egui::Grid::new("sysinfo").show(ui, |ui| {
                ui.label("CPU:");
                ui.label(RichText::new(&sys.cpu.name).strong());
                ui.end_row();
                
                ui.label("Cores/Threads:");
                ui.label(if sys.cpu.e_cores > 0 {
                    format!("{} physical / {} logical (P: {}, E: {})", sys.cpu.cores, sys.cpu.threads, sys.cpu.p_cores, sys.cpu.e_cores)
                } else {
                    format!("{} physical / {} logical", sys.cpu.cores, sys.cpu.threads)
                });
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
        ui.heading("Display");
        ui.label(format!("Renderer: {}", if self.renderer.is_empty() { "unknown" } else { &self.renderer }));
        ui.label(RichText::new("Start with --renderer opengl or --renderer vulkan to force one; the precise pattern window always presents with VSync.").weak().small());

        ui.separator();
        ui.heading("Timers");
        let t = &self.timers;
        egui::Grid::new("timers").show(ui, |ui| {
            ui.label("Time source:");
            ui.label(format!("{} · {} Hz", t.qpc_source, t.qpc_hz));
            ui.end_row();
            if let Some(cur) = t.timer_res_current_ms {
                ui.label("Timer resolution:");
                ui.label(format!(
                    "{:.3} ms now (finest {:.3} ms, default {:.3} ms)",
                    cur,
                    t.timer_res_finest_ms.unwrap_or(0.0),
                    t.timer_res_coarsest_ms.unwrap_or(0.0)
                ));
                ui.end_row();
            }
            if let Some(avail) = &t.clocksources_available {
                ui.label("Available clocksources:");
                ui.label(avail);
                ui.end_row();
            }
        });
        for n in &t.notes {
            ui.colored_label(Color32::YELLOW, format!("⚠ {}", n));
        }
        if ui.small_button("Re-check timers").clicked() {
            self.timers = crate::timer_info::query();
        }

        ui.separator();
        ui.heading("Sensors");
        self.lhm_panel(ui);
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

    /// Where the extra sensors come from, and the buttons to get more of them
    fn lhm_panel(&mut self, ui: &mut Ui) {
        let fetched = self.lhm_fetch.lock().unwrap().take();
        if let Some(r) = fetched {
            let pawn = std::mem::take(&mut self.pawnio_installing);
            self.lhm_fetching = false;
            self.lhm_note = match r {
                Ok(t) => format!("{} · sensor helper restarted", t.lines().last().unwrap_or("done")),
                Err(e) => format!("{} failed: {}", if pawn { "PawnIO install" } else { "download" }, e.lines().last().unwrap_or("")),
            };
            self.pawnio = crate::lhm::pawnio_version();
            // the helper opened LibreHardwareMonitor before the library or the driver existed
            crate::sensors::restart_windows_helpers();
            let n = self.lhm_note.clone();
            self.log(&format!("LibreHardwareMonitor: {}", n));
        }
        ui.horizontal_wrapped(|ui| {
            ui.label(format!("Administrator: {}", if self.is_admin { "yes" } else { "no" }));
            if !self.is_admin
                && ui
                    .button("Restart as administrator")
                    .on_hover_text("Board, VRM, memory and CPU sensors (LibreHardwareMonitor driver, RAPL on Linux) need administrator rights")
                    .clicked()
            {
                match crate::lhm::restart_as_admin() {
                    Ok(()) => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close),
                    Err(e) => self.lhm_note = e,
                }
            }
        });
        if cfg!(target_os = "windows") {
            ui.horizontal_wrapped(|ui| {
                match crate::lhm::dir() {
                    Some(d) => {
                        ui.label(format!("LibreHardwareMonitor library: {}", d.display()));
                    }
                    None => {
                        ui.label("LibreHardwareMonitor library: not installed (sensors come from the LibreHardwareMonitor app if it runs, else ACPI)");
                    }
                }
                let label = if crate::lhm::dir().is_some() { "Update LibreHardwareMonitor" } else { "Download LibreHardwareMonitor" };
                if ui
                    .add_enabled(!self.lhm_fetching, egui::Button::new(label))
                    .on_hover_text("Fetches the latest release from github.com/LibreHardwareMonitor/LibreHardwareMonitor (MPL-2.0) into a folder next to the exe")
                    .clicked()
                {
                    self.lhm_fetching = true;
                    self.lhm_note = "downloading…".into();
                    let slot = self.lhm_fetch.clone();
                    thread::spawn(move || {
                        let r = crate::lhm::fetch();
                        *slot.lock().unwrap() = Some(r);
                    });
                }
            });
        }
        if cfg!(target_os = "windows") {
            ui.horizontal_wrapped(|ui| {
                match &self.pawnio {
                    Some(v) => ui.label(format!("PawnIO driver: {}", v)),
                    None => ui.label(
                        RichText::new("PawnIO driver: not installed. LibreHardwareMonitor 0.9.5+ needs it for CPU core temperatures, board (VRM, fans, voltages) and memory sensors")
                            .color(Color32::from_rgb(255, 170, 60)),
                    ),
                };
                if self.pawnio.is_none()
                    && ui
                        .add_enabled(!self.lhm_fetching && crate::lhm::dir().is_some(), egui::Button::new("Install PawnIO"))
                        .on_hover_text("Runs the PawnIO setup that ships inside LibreHardwareMonitor.exe (signed driver by namazso, pawnio.eu), the same way LibreHardwareMonitor does on its first start. Needs administrator rights.")
                        .clicked()
                {
                    self.lhm_fetching = true;
                    self.pawnio_installing = true;
                    self.lhm_note = "installing PawnIO…".into();
                    let slot = self.lhm_fetch.clone();
                    thread::spawn(move || {
                        let r = crate::lhm::install_pawnio();
                        *slot.lock().unwrap() = Some(r);
                    });
                }
            });
        }
        if !self.lhm_note.is_empty() {
            ui.label(RichText::new(&self.lhm_note).weak().small());
        }
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
            ui.label(RichText::new("Sign and save (one record per click):").strong());
            ui.horizontal_wrapped(|ui| {
                use crate::session::Scope;
                if ui.button(RichText::new("💾 Log everything").strong()).on_hover_text("All suites with data, manual click/key trials, sensors and virtualization state in one signed record").clicked() {
                    self.log_session(Scope::Everything);
                }
                for (label, scope) in [("Memory", Scope::Memory), ("CPU", Scope::Cpu), ("GPU", Scope::Gpu), ("Input", Scope::Input), ("Sensors", Scope::Sensors)] {
                    if ui.button(label).clicked() {
                        self.log_session(scope);
                    }
                }
            });
            
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
        
        self.web_viewer_panel(ui);

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

    /// Browser view of the saved JSON results: a local read-only web server, or a standalone HTML file
    fn web_viewer_panel(&mut self, ui: &mut Ui) {
        ui.add_space(6.0);
        ui.group(|ui| {
            ui.label(RichText::new("Web viewer for saved results").strong());
            ui.label(RichText::new("Charts, tables and switches for every saved JSON file, in your browser. Read-only.").weak().small());
            ui.horizontal_wrapped(|ui| {
                let running = self.web_server.is_some();
                ui.add_enabled_ui(!running, |ui| {
                    ui.label("Port:");
                    ui.add(egui::DragValue::new(&mut self.web_port).range(1024..=65535));
                    ui.checkbox(&mut self.web_lan, "allow other computers on my network")
                        .on_hover_text("Listen on all network interfaces so another PC can open http://<this-pc>:<port>/. Anyone who can reach the port can read the results.");
                });
                if !running {
                    if ui.button(RichText::new("▶ Start server and open in browser").strong()).clicked() {
                        let bind = if self.web_lan { "0.0.0.0" } else { "127.0.0.1" };
                        match crate::server::Server::start(self.results_dir(), bind, self.web_port) {
                            Ok(server) => {
                                let url = server.url();
                                crate::server::open_in_browser(&url);
                                self.web_note = format!("serving {}", url);
                                self.log(&format!("Web viewer started: {}", url));
                                self.web_server = Some(server);
                            }
                            Err(e) => {
                                self.web_note = format!("could not start: {}", e);
                                self.log(&format!("Web viewer failed to start: {}", e));
                            }
                        }
                    }
                } else if let Some(server) = &self.web_server {
                    let url = server.url();
                    if ui.button("Open in browser").clicked() {
                        crate::server::open_in_browser(&url);
                    }
                    if ui.button(RichText::new("⏹ Stop server").color(Color32::from_rgb(255, 120, 120))).clicked() {
                        self.web_server = None;
                        self.web_note = "server stopped".into();
                        self.log("Web viewer stopped");
                    }
                    let mut shown: &str = &url;
                    ui.add(egui::TextEdit::singleline(&mut shown).desired_width(220.0));
                }
            });
            ui.horizontal_wrapped(|ui| {
                if ui.button("Export HTML report").on_hover_text("One self-contained .html file with all saved results; opens offline and can be e-mailed").clicked() {
                    let entries = crate::report::list(&self.results_dir());
                    if entries.is_empty() {
                        self.web_note = "no saved results to export yet".into();
                    } else {
                        let file = self.results_dir().join(format!("latency_report_{}.html", chrono::Local::now().format("%Y%m%d_%H%M%S")));
                        match std::fs::write(&file, crate::report::render_static(&entries)) {
                            Ok(()) => {
                                self.web_note = format!("wrote {} result(s) to {}", entries.len(), file.display());
                                crate::server::open_in_browser(&format!("file:///{}", file.to_string_lossy().replace('\\', "/")));
                            }
                            Err(e) => self.web_note = format!("could not write report: {}", e),
                        }
                    }
                }
                ui.label(RichText::new(&self.web_note).weak().small());
            });
        });
    }

    fn render_log_panel(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label("Log:");
            ui.checkbox(&mut self.show_log_panel, "show");
            if ui.button("Copy all").on_hover_text("Copy the whole log to the clipboard").clicked() {
                ui.ctx().copy_text(self.log_text.clone());
            }
            if ui.button("Save…").on_hover_text("Write the log to latency_log.txt next to the program").clicked() {
                let path = self.results_dir().join("latency_log.txt");
                match std::fs::write(&path, &self.log_text) {
                    Ok(()) => self.log_saved_note = format!("saved to {}", path.display()),
                    Err(e) => self.log_saved_note = format!("could not save: {}", e),
                }
            }
            if ui.button("Clear").clicked() {
                self.log_text.clear();
            }
            ui.label(RichText::new(format!("{} lines · drag to select, Ctrl+A / Ctrl+C in the box", self.log_text.lines().count())).weak().small());
            if !self.log_saved_note.is_empty() {
                ui.label(RichText::new(&self.log_saved_note).weak().small());
            }
        });

        if self.show_log_panel {
            // A read-only multi-line editor: selectable with the mouse, Ctrl+A selects everything,
            // Ctrl+C copies the selection, and the panel can be dragged taller.
            egui::ScrollArea::vertical()
                .id_salt("log_scroll")
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    let mut text: &str = &self.log_text;
                    ui.add(
                        egui::TextEdit::multiline(&mut text)
                            .font(egui::TextStyle::Monospace)
                            .desired_width(f32::INFINITY)
                            .desired_rows(4)
                            .frame(false)
                            .id_salt("log_text"),
                    );
                });
        }
    }
}

impl eframe::App for LatencyTesterApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // VSync is off and spinners ask for a new frame every frame, so the GUI thread would otherwise
        // spin at 100 % on one core for the whole test. While a benchmark runs (and no interactive
        // trial or display pattern needs exact frame timing) draw at most ~20 frames per second.
        let timing_screen = self.input_test.engine.is_active() || self.input_test.display.is_running();
        if self.is_running() && !timing_screen {
            let min_frame = Duration::from_millis(50);
            let spent = self.last_frame.elapsed();
            if spent < min_frame {
                thread::sleep(min_frame - spent);
            }
        }
        self.last_frame = Instant::now();

        self.check_completed_tasks();
        self.run_all_tick();

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
                    if ui
                        .add_enabled(!self.is_running(), egui::Button::new(RichText::new("▶ Run all tests…").strong()))
                        .on_hover_text("Opens the Dashboard with the plan for running every test")
                        .clicked()
                    {
                        self.tab = Tab::Dashboard;
                    }
                });
            });
        });
        
        self.run_all_bar(ctx);

        egui::TopBottomPanel::bottom("bottom")
            .resizable(true)
            .min_height(70.0)
            .default_height(150.0)
            .show(ctx, |ui| {
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
        self.run_all_overlay(ctx);
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

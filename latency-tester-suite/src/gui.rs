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

use crate::cpu_benchmark::{CpuBenchmark, CpuBenchmarkConfig, WorkloadType, AffinityMode};
use crate::gpu_benchmark::{GpuBenchmark, GpuBenchmarkConfig};
use crate::input_latency::{InputLatencyTester, InputLatencyConfig, InputTestMode, measure_timer_resolution, TimerResolutionInfo};
use crate::memory_benchmark::{MemoryBenchmark, MemoryBenchmarkConfig, MemoryBenchmarkSummary};
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
    task_status: String,
    
    // Memory config
    mem_config: MemoryBenchmarkConfig,
    mem_quick_size: usize,
    
    // CPU config
    cpu_workload: WorkloadType,
    cpu_threads: usize,
    
    // GPU config
    gpu_config: GpuBenchmarkConfig,
    
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
        
        let config_dir = std::env::current_dir()
            .map(|p| p.join("latency_results"))
            .unwrap_or_else(|_| std::path::PathBuf::from("latency_results"));
        let _ = std::fs::create_dir_all(&config_dir);
        
        let logger = ResultLogger::new(
            env!("CARGO_PKG_VERSION").to_string(),
            config_dir.to_string_lossy().to_string(),
        ).ok();
        
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
            task_status: String::from("Ready"),
            mem_config: MemoryBenchmarkConfig::default(),
            mem_quick_size: 64 * 1024 * 1024,
            cpu_workload: WorkloadType::GameSim,
            cpu_threads: num_cpus::get(),
            gpu_config: GpuBenchmarkConfig::default(),
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
    }

    fn start_memory_benchmark(&mut self) {
        if self.running.lock().unwrap().is_some() {
            return;
        }
        
        let config = self.mem_config.clone();
        let bench = MemoryBenchmark::new(config.clone());
        
        *self.running.lock().unwrap() = Some(RunningTaskState {
            kind: "memory".to_string(),
            started: Instant::now(),
        });
        self.task_status = "Running memory benchmark...".to_string();
        self.log(&format!("Started memory benchmark: {} sizes x {} patterns x {} thread counts",
            config.sizes.len(), config.patterns.len(), config.thread_counts.len()));

        let running = self.running.clone();
        thread::spawn(move || {
            let mut bench = bench;
            let result = bench.run();
            COMPLETE_MEMORY_RESULT.lock().unwrap().replace(result);
            if let Ok(mut guard) = running.lock() {
                *guard = None;
            }
        });
    }

    fn start_cpu_benchmark(&mut self) {
        if self.running.lock().unwrap().is_some() {
            return;
        }
        
        let config = CpuBenchmarkConfig {
            workload_types: vec![self.cpu_workload],
            thread_counts: vec![self.cpu_threads],
            affinity_modes: vec![AffinityMode::AllCores],
            duration_seconds: 5,
            warmup_seconds: 1,
            iterations: 3,
        };
        
        let bench = match CpuBenchmark::new(config.clone()) {
            Ok(b) => b,
            Err(e) => {
                self.log(&format!("Failed to create CPU benchmark: {}", e));
                return;
            }
        };
        
        *self.running.lock().unwrap() = Some(RunningTaskState {
            kind: "cpu".to_string(),
            started: Instant::now(),
        });
        self.task_status = "Running CPU benchmark...".to_string();
        self.log(&format!("Started CPU benchmark: {:?}, {} threads", self.cpu_workload, self.cpu_threads));

        let running = self.running.clone();
        thread::spawn(move || {
            let mut bench = bench;
            let result = bench.run();
            COMPLETE_CPU_RESULT.lock().unwrap().replace(result);
            if let Ok(mut guard) = running.lock() {
                *guard = None;
            }
        });
    }

    fn start_gpu_benchmark(&mut self) {
        if self.running.lock().unwrap().is_some() {
            return;
        }

        let config = self.gpu_config.clone();
        *self.running.lock().unwrap() = Some(RunningTaskState {
            kind: "gpu".to_string(),
            started: Instant::now(),
        });
        self.task_status = "Running GPU benchmark...".to_string();
        self.log("Started GPU benchmark (Vulkan)");

        let running = self.running.clone();
        thread::spawn(move || {
            // Vulkan objects are created and destroyed on the worker thread
            let result = GpuBenchmark::new(config).and_then(|mut bench| bench.run());
            COMPLETE_GPU_RESULT.lock().unwrap().replace(result);
            if let Ok(mut guard) = running.lock() {
                *guard = None;
            }
        });
    }

    fn start_input_benchmark(&mut self) {
        if self.running.lock().unwrap().is_some() {
            return;
        }
        
        let config = InputLatencyConfig::default();
        let tester = InputLatencyTester::new(config.clone());
        
        *self.running.lock().unwrap() = Some(RunningTaskState {
            kind: "input".to_string(),
            started: Instant::now(),
        });
        self.task_status = "Running input latency tests...".to_string();
        self.log("Started input latency tests");

        let running = self.running.clone();
        thread::spawn(move || {
            let mut tester = tester;
            let result = tester.run();
            COMPLETE_INPUT_RESULT.lock().unwrap().replace(result);
            if let Ok(mut guard) = running.lock() {
                *guard = None;
            }
        });
    }

    fn check_completed_tasks(&mut self) {
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

        // Check for completed tasks
        if let Some(result) = COMPLETE_MEMORY_RESULT.lock().unwrap().take() {
            self.task_status = "Idle".to_string();
            match result {
                Ok(summary) => {
                    self.last_memory_result = Some(summary.clone());
                    self.log(&format!("Memory benchmark complete: {} results", summary.results.len()));
                }
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
                Err(e) => self.log(&format!("Input benchmark error: {}", e)),
            }
        }
        
        // Auto-refresh system info every 5 seconds
        if self.last_refresh.elapsed() > Duration::from_secs(5) {
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
        ui.label(format!("Last refresh: {:.0} seconds ago", self.last_refresh.elapsed().as_secs()));
    }

    fn render_memory_tab(&mut self, ui: &mut Ui) {
        ui.heading("Memory Latency Benchmark");
        ui.separator();
        
        // Quick test
        ui.group(|ui| {
            ui.label("Quick Test (1 thread, random read + pointer chase):");
            // Removed global_text_height as it's not a valid function in egui 0.29
            let size_names: Vec<(&str, usize)> = vec![
                ("4 KB (L1)", 4 * 1024),
                ("256 KB (L2)", 256 * 1024),
                ("4 MB (L3)", 4 * 1024 * 1024),
                ("64 MB", 64 * 1024 * 1024),
                ("256 MB", 256 * 1024 * 1024),
            ];
            
            ui.horizontal(|ui| {
                for (name, size) in size_names {
                    if ui.selectable_value(&mut self.mem_quick_size, size, name).clicked() {
                        self.mem_quick_size = size;
                    }
                }
            });
            
            if ui.button("Run Quick Test").clicked() {
                let size = self.mem_quick_size;
                let result = crate::memory_benchmark::quick_memory_latency_test(size, 50);
                match result {
                    Ok(latency) => {
                        self.log(&format!("Quick memory test ({}): {:.0} ns avg latency", 
                            self.mem_quick_size, latency));
                    }
                    Err(e) => self.log(&format!("Quick test error: {}", e)),
                }
            }
        });
        
        ui.separator();
        
        // Full config
        ui.collapsing("Full Benchmark Configuration", |ui| {
            ui.label(format!("Sizes: {} ({} KB to {} MB)", 
                self.mem_config.sizes.len(),
                self.mem_config.sizes.first().copied().unwrap_or(0) / 1024,
                self.mem_config.sizes.last().copied().unwrap_or(0) / 1024 / 1024));
            ui.label(format!("Patterns: {}", self.mem_config.patterns.len()));
            ui.label(format!("Thread counts: {:?}", self.mem_config.thread_counts));
            
            ui.horizontal(|ui| {
                ui.label("Iterations:");
                ui.add(egui::DragValue::new(&mut self.mem_config.iterations).range(1..=10000));
            });
        });
        
        if ui.button("Run Full Memory Benchmark").clicked() {
            self.start_memory_benchmark();
        }
        
        // Results
        if let Some(summary) = &self.last_memory_result {
            ui.separator();
            ui.heading("Latest Results");
            
            // Show top results
            egui::ScrollArea::vertical().show(ui, |ui| {
                for result in summary.results.iter().take(20) {
                    ui.horizontal(|ui| {
                        ui.label(format!("{:>10}", human_size(result.size)));
                        ui.label(format!("{:?} ({})", result.pattern, result.thread_count));
                        ui.label(format!("{:.0} ns", result.latency_ns));
                        ui.label(format!("{:.2} GB/s", result.bandwidth_gb_s));
                        ui.label(format!("p99: {:.0} ns", result.percentile_99_ns));
                    });
                }
            });
        }
    }

    fn render_cpu_tab(&mut self, ui: &mut Ui) {
        ui.heading("CPU Benchmark");
        ui.separator();
        
        ui.group(|ui| {
            ui.label("Workload:");
            let workloads: Vec<(&str, WorkloadType)> = vec![
                ("Game Sim (physics/AI)", WorkloadType::GameSim),
                ("Compilation Sim", WorkloadType::CompilationSim),
                ("Mixed", WorkloadType::MixedWorkload),
                ("Vector FMA (AVX)", WorkloadType::VectorFma),
                ("Float FMA", WorkloadType::FloatFma),
                ("Memory Latency", WorkloadType::MemoryLatency),
                ("Crypto (AES-like)", WorkloadType::CryptoAes),
                ("Integer Add", WorkloadType::IntegerAdd),
            ];
            
            for (name, wl) in workloads {
                if ui.radio_value(&mut self.cpu_workload, wl, name).changed() {
                    self.cpu_workload = wl;
                }
            }
            
            ui.separator();
            ui.label("Threads:");
            ui.horizontal(|ui| {
                for n in [1, 2, 4, 8, 16, 32, 64] {
                    if n <= num_cpus::get() || n == 1 {
                        if ui.radio_value(&mut self.cpu_threads, n, format!("{}", n)).changed() {
                            self.cpu_threads = n;
                        }
                    }
                }
            });
            
            if ui.button("Run CPU Benchmark (5s)").clicked() {
                self.start_cpu_benchmark();
            }
        });
        
        if let Some(summary) = &self.last_cpu_result {
            ui.separator();
            ui.heading("Latest Results");
            for result in &summary.results {
                ui.horizontal(|ui| {
                    ui.label(format!("{:?} ({} threads, {:?})", result.workload, result.thread_count, result.affinity_mode));
                    ui.label(format!("{:.0} M ops/s", result.operations_per_second / 1_000_000.0));
                    ui.label(format!("{:.1} MHz", result.frequency_mhz as f64));
                });
            }
            
            // Topology info
            ui.separator();
            ui.heading("Core Topology");
            ui.label(format!("{} logical cores ({} physical)", 
                summary.core_topology.total_logical, summary.core_topology.total_physical));
            ui.label(format!("P-cores: {:?} E-cores: {:?}", 
                summary.core_topology.performance_cores, summary.core_topology.efficiency_cores));
        }
    }

    fn render_gpu_tab(&mut self, ui: &mut Ui) {
        ui.heading("GPU Benchmark (Vulkan)");
        ui.separator();
        
        ui.group(|ui| {
            ui.label(format!("Workload sizes (elements): {:?}", self.gpu_config.workload_sizes));
            ui.horizontal(|ui| {
                ui.label("Iterations:");
                ui.add(egui::DragValue::new(&mut self.gpu_config.iterations).range(1..=10000));
                ui.label("Warmup:");
                ui.add(egui::DragValue::new(&mut self.gpu_config.warmup_iterations).range(0..=1000));
            });

            if ui.button("Run GPU Benchmark").clicked() {
                self.start_gpu_benchmark();
            }
        });

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
                    if ui.button("Run Full Input Suite").clicked() {
                        self.start_input_benchmark();
                    }
                });
            });
        });
        
        if let Some(summary) = &self.last_input_result {
            ui.separator();
            ui.heading("Full Suite Results");
            for result in &summary.results {
                ui.horizontal(|ui| {
                    ui.label(format!("{:?}", result.mode));
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
                
                if self.running.lock().unwrap().is_some() {
                    ui.colored_label(Color32::YELLOW, " ⏳ Running...");
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
            if ui.button("📋 Results & Verify").clicked() { self.tab = Tab::Results; }
        });
        
        egui::CentralPanel::default().show(ctx, |ui| {
            match self.tab {
                Tab::Dashboard => self.render_dashboard(ui),
                Tab::Memory => self.render_memory_tab(ui),
                Tab::Cpu => self.render_cpu_tab(ui),
                Tab::Gpu => self.render_gpu_tab(ui),
                Tab::Input => self.render_input_tab(ui, ctx),
                Tab::Virtualization => self.render_virtualization_tab(ui),
                Tab::Results => self.render_results_tab(ui),
            }
        });
    }
}

/// Human-readable size
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

static COMPLETE_MEMORY_RESULT: Mutex<Option<Result<MemoryBenchmarkSummary, anyhow::Error>>> = 
    Mutex::new(None);
static COMPLETE_CPU_RESULT: Mutex<Option<Result<crate::cpu_benchmark::CpuBenchmarkSummary, anyhow::Error>>> = 
    Mutex::new(None);
static COMPLETE_GPU_RESULT: Mutex<Option<Result<crate::gpu_benchmark::GpuBenchmarkSummary, anyhow::Error>>> = 
    Mutex::new(None);
static COMPLETE_INPUT_RESULT: Mutex<Option<Result<crate::input_latency::InputLatencySummary, anyhow::Error>>> = 
    Mutex::new(None);
static COMPLETE_SYSINFO: Mutex<
    Option<(
        Result<SystemInfo, anyhow::Error>,
        Result<VirtualizationStatus, anyhow::Error>,
    )>,
> = Mutex::new(None);

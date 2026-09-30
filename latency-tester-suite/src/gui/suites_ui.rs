//! CPU / GPU / Input tabs: run controls, live progress and the plumbing that starts each suite
//! with cancellation, progress reporting and a sensor sampler attached.

use eframe::egui;
use egui::{Color32, RichText, Ui};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::core_select::CoreSelector;
use super::{LatencyTesterApp, RunningTaskState};
use crate::cpu_benchmark::{AffinityMode, CpuBenchmark, CpuBenchmarkConfig, CpuBenchmarkResult, WorkloadType};
use crate::gpu_benchmark::{GpuBenchmark, GpuBenchmarkResult};
use crate::input_latency::{InputLatencyConfig, InputTestMode};
use crate::progress::RunProgress;
use crate::sensors::Sampler;
use crate::topology::CoreKind;

pub(super) const WORKLOADS: [(&str, &str, WorkloadType); 12] = [
    ("Game Sim", "physics + collision + AI decisions", WorkloadType::GameSim),
    ("Compilation Sim", "allocation, pointer chasing, branching", WorkloadType::CompilationSim),
    ("Mixed", "int/float/branch mix", WorkloadType::MixedWorkload),
    ("Vector FMA", "4-wide fused multiply-add (AVX-like)", WorkloadType::VectorFma),
    ("Float FMA", "scalar fused multiply-add chain", WorkloadType::FloatFma),
    ("Float Div", "latency-bound divide chain", WorkloadType::FloatDiv),
    ("Integer Add", "dependent integer add chain", WorkloadType::IntegerAdd),
    ("Integer Mul", "dependent integer multiply chain", WorkloadType::IntegerMul),
    ("Crypto (AES-like)", "rotate / multiply mixing", WorkloadType::CryptoAes),
    ("Branch Prediction", "data-dependent branches", WorkloadType::BranchPrediction),
    ("Memory Copy", "small-buffer copies", WorkloadType::MemoryCopy),
    ("Memory Latency", "L1-resident dependent loads", WorkloadType::MemoryLatency),
];

pub(super) struct CpuUi {
    pub cores: CoreSelector,
    pub workloads: Vec<(WorkloadType, bool)>,
    pub threads: usize,
    pub threads_all: bool,
    pub duration_s: u64,
    pub warmup_s: u64,
    pub iterations: u32,
}

impl CpuUi {
    pub fn new(kinds: Option<Vec<CoreKind>>) -> Self {
        Self {
            cores: CoreSelector::new(kinds, "cpu"),
            // Same profile as "Run all tests": every workload on all cores, 10 s runs x 10, 10 s warmup
            workloads: WORKLOADS.iter().map(|w| (w.2, true)).collect(),
            threads: num_cpus::get(),
            threads_all: true,
            duration_s: 10,
            warmup_s: 10,
            iterations: 10,
        }
    }

    pub fn build_config(&self) -> Result<CpuBenchmarkConfig, String> {
        if let Some(e) = self.cores.error() {
            return Err(e);
        }
        let workload_types: Vec<WorkloadType> = self.workloads.iter().filter(|(_, on)| *on).map(|(w, _)| *w).collect();
        if workload_types.is_empty() {
            return Err("Select at least one workload".to_string());
        }
        let (ids, _) = self.cores.resolve();
        let (thread_counts, affinity_modes) = if self.cores.sweep {
            let modes: Vec<AffinityMode> = ids.iter().filter(|&&c| c < 64).map(|&c| AffinityMode::CustomMask(1u64 << c)).collect();
            if modes.is_empty() {
                return Err("Per-core testing supports cores 0-63".to_string());
            }
            (vec![1], modes)
        } else if ids.is_empty() {
            let t = if self.threads_all { self.cores.logical() } else { self.threads.clamp(1, self.cores.logical()) };
            (vec![t], vec![AffinityMode::AllCores])
        } else {
            let usable = ids.iter().filter(|&&c| c < 64).count();
            if usable == 0 {
                return Err("Pinned runs support cores 0-63".to_string());
            }
            let t = if self.threads_all { usable } else { self.threads.clamp(1, usable) };
            (vec![t], vec![AffinityMode::CustomMask(self.cores.mask())])
        };
        Ok(CpuBenchmarkConfig {
            workload_types,
            thread_counts,
            affinity_modes,
            duration_seconds: self.duration_s.max(1),
            warmup_seconds: self.warmup_s,
            iterations: self.iterations.max(1),
            core_by_core: self.cores.core_by_core,
        })
    }
}

/// Input-suite options (timing-stack tests, optionally repeated per core)
pub(super) struct InputUi {
    pub cores: CoreSelector,
    pub samples: u32,
    pub modes: Vec<(InputTestMode, bool)>,
}

impl InputUi {
    pub fn new(kinds: Option<Vec<CoreKind>>) -> Self {
        let defaults = InputLatencyConfig::default();
        Self {
            cores: CoreSelector::new(kinds, "input"),
            samples: 500,
            modes: [InputTestMode::MouseMove, InputTestMode::RawInput, InputTestMode::PollingRate, InputTestMode::Jitter]
                .into_iter()
                .map(|m| (m, defaults.test_modes.contains(&m)))
                .collect(),
        }
    }
}

fn fmt_duration(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{:02}:{:02}", s / 60, s % 60)
}

/// Progress bar + "what is running now" block shared by the CPU, GPU and input tabs
pub(super) fn progress_panel(ui: &mut Ui, p: &RunProgress, running: bool) {
    if p.total == 0 {
        return;
    }
    ui.add_space(6.0);
    ui.group(|ui| {
        // `running` only says some task is busy (during "run all" that is always true): this suite
        // itself is live only until all its tests are done or it was marked finished
        let running = running && p.active();
        let elapsed = p.elapsed_s();
        let eta = if running && p.done > 0 {
            format!(" · ETA ~{}", fmt_duration(elapsed / p.done as f64 * (p.total - p.done) as f64))
        } else {
            String::new()
        };
        ui.add(
            egui::ProgressBar::new(p.done as f32 / p.total as f32)
                .desired_width(ui.available_width())
                .text(format!("{} / {} tests · {} elapsed{}", p.done, p.total, fmt_duration(elapsed), eta)),
        );
        if running {
            ui.label(RichText::new(format!("▶ {}", p.title)).size(17.0).strong());
            ui.label(&p.detail);
        } else {
            ui.label(RichText::new(format!("Finished {} of {} tests", p.done, p.total)).weak());
        }
    });
}

impl LatencyTesterApp {
    // ------------------------------------------------------------------ sensors

    pub(super) fn begin_sampler(&mut self) -> Arc<Sampler> {
        if let Some(old) = self.sampler.take() {
            old.stop();
        }
        // Keep the app's own threads on one quiet core while tests run (moved out of the way of
        // every pinned test by the benchmarks)
        if !crate::app_core::is_active() {
            if let Some(core) = crate::app_core::reserve() {
                crate::app_core::apply_gui();
                self.log(&format!("App threads moved to logical CPU {} while tests run (the least busy, furthest core)", core));
            }
        }
        let s = Sampler::start(Duration::from_millis(500));
        self.sampler = Some(s.clone());
        self.sampler_active = true;
        s
    }

    /// Freeze the sampler's timeline once the task that used it has ended
    pub(super) fn finish_sampler_if_idle(&mut self) {
        if self.sampler_active && !self.is_running() {
            if let Some(s) = &self.sampler {
                s.stop();
                self.last_timeline = s.timeline();
                self.last_phases = s.phases();
                self.sensor_notes = s.notes();
            }
            self.sampler_active = false;
            if crate::app_core::is_active() {
                let moves = crate::app_core::moves();
                crate::app_core::release();
                crate::app_core::apply_gui();
                if moves > 0 {
                    self.log(&format!("The app moved out of the way of the tested core {} time(s); it may use every core again", moves));
                }
            }
        }
    }

    pub(super) fn mark_running(&mut self, kind: &str, status: &str) {
        self.cancel.store(false, std::sync::atomic::Ordering::Relaxed);
        *self.running.lock().unwrap() = Some(RunningTaskState { kind: kind.to_string(), started: Instant::now() });
        self.task_status = status.to_string();
    }

    // ------------------------------------------------------------------ CPU

    pub(super) fn start_cpu_benchmark(&mut self) {
        if self.is_running() {
            return;
        }
        let config = match self.cpu_ui.build_config() {
            Ok(c) => c,
            Err(e) => {
                self.log(&format!("Cannot start CPU benchmark: {}", e));
                return;
            }
        };
        self.last_cpu_config = Some(config.clone());
        let sampler = self.begin_sampler();
        *self.cpu_progress.lock().unwrap() = RunProgress::default();
        self.cpu_partial.lock().unwrap().clear();
        let bench = match CpuBenchmark::new(config.clone()) {
            Ok(b) => b
                .with_cancel(self.cancel.clone())
                .with_progress(self.cpu_progress.clone(), self.cpu_partial.clone())
                .with_sensors(sampler),
            Err(e) => {
                self.log(&format!("Failed to create CPU benchmark: {}", e));
                return;
            }
        };
        self.mark_running("cpu benchmark", "Running CPU benchmark...");
        self.log(&format!(
            "Started CPU benchmark: {} workload(s) × {} core set(s), {} thread(s)",
            config.workload_types.len(),
            config.affinity_modes.len(),
            config.thread_counts[0]
        ));

        let running = self.running.clone();
        std::thread::spawn(move || {
            let mut bench = bench;
            let result = bench.run();
            super::COMPLETE_CPU_RESULT.lock().unwrap().replace(result);
            if let Ok(mut guard) = running.lock() {
                *guard = None;
            }
        });
    }

    pub(super) fn render_cpu_tab(&mut self, ui: &mut Ui) {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.heading("CPU Benchmark");
            ui.separator();
            let busy = self.is_running();
            ui.add_enabled_ui(!busy, |ui| {
                egui::CollapsingHeader::new("Cores").default_open(true).show(ui, |ui| {
                    self.cpu_ui.cores.ui(ui);
                });

                egui::CollapsingHeader::new("Threads").default_open(true).show(ui, |ui| {
                    if self.cpu_ui.cores.sweep {
                        ui.label(RichText::new("Per-core mode: each core runs one pinned thread.").weak());
                    } else {
                        let max = self.cpu_ui.cores.max_threads().max(1);
                        ui.horizontal_wrapped(|ui| {
                            ui.checkbox(&mut self.cpu_ui.threads_all, format!("Use all selected cores ({} threads)", max));
                            ui.add_enabled_ui(!self.cpu_ui.threads_all, |ui| {
                                ui.label("or exactly");
                                ui.add(egui::DragValue::new(&mut self.cpu_ui.threads).range(1..=max));
                                ui.label("thread(s)");
                            });
                        });
                        if !self.cpu_ui.threads_all {
                            ui.horizontal_wrapped(|ui| {
                                ui.label("Quick pick:");
                                let mut n = 1;
                                while n <= max {
                                    if ui.small_button(n.to_string()).clicked() {
                                        self.cpu_ui.threads = n;
                                    }
                                    n = if n < 8 { n * 2 } else { n + 8 };
                                }
                            });
                        }
                    }
                });

                let selected = self.cpu_ui.workloads.iter().filter(|(_, on)| *on).count();
                egui::CollapsingHeader::new(format!("Workloads ({} selected)", selected))
                    .id_salt("cpu_workloads")
                    .default_open(true)
                    .show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            for (w, on) in self.cpu_ui.workloads.iter_mut() {
                                let (name, tip) = WORKLOADS.iter().find(|x| x.2 == *w).map(|x| (x.0, x.1)).unwrap_or(("?", ""));
                                ui.checkbox(on, name).on_hover_text(tip);
                            }
                        });
                        ui.horizontal(|ui| {
                            if ui.small_button("All").clicked() {
                                self.cpu_ui.workloads.iter_mut().for_each(|(_, on)| *on = true);
                            }
                            if ui.small_button("None").clicked() {
                                self.cpu_ui.workloads.iter_mut().for_each(|(_, on)| *on = false);
                            }
                        });
                    });

                egui::CollapsingHeader::new("Timing").default_open(true).show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Run length:");
                        ui.add(egui::DragValue::new(&mut self.cpu_ui.duration_s).range(1..=600).suffix(" s"));
                        ui.label("Runs per test:");
                        ui.add(egui::DragValue::new(&mut self.cpu_ui.iterations).range(1..=1000));
                        ui.label("Warmup:");
                        ui.add(egui::DragValue::new(&mut self.cpu_ui.warmup_s).range(0..=60).suffix(" s"));
                    });
                });
            });

            ui.separator();
            let cfg = self.cpu_ui.build_config();
            match &cfg {
                Ok(c) => {
                    let tests = c.workload_types.len() * c.affinity_modes.len() * c.thread_counts.len();
                    let secs = tests as u64 * (c.warmup_seconds + c.duration_seconds * c.iterations as u64);
                    ui.label(format!(
                        "{} workload(s) × {} core set(s) = {} tests on {} · about {}",
                        c.workload_types.len(),
                        c.affinity_modes.len(),
                        tests,
                        self.cpu_ui.cores.resolve().1,
                        fmt_duration(secs as f64)
                    ));
                }
                Err(e) => {
                    ui.colored_label(Color32::YELLOW, format!("⚠ {}", e));
                }
            }
            ui.horizontal(|ui| {
                if ui.add_enabled(!self.is_running() && cfg.is_ok(), egui::Button::new(RichText::new("▶ Run CPU benchmark").strong())).clicked() {
                    self.start_cpu_benchmark();
                }
                if ui.add_enabled(!self.is_running(), egui::Button::new("Reset")).clicked() {
                    let kinds = self.cpu_ui.cores.kinds.clone();
                    self.cpu_ui = CpuUi::new(kinds);
                }
                self.stop_button(ui);
            });

            let prog = self.cpu_progress.lock().unwrap().clone();
            progress_panel(ui, &prog, self.is_running());
            self.cpu_results_table(ui);
        });
    }

    fn cpu_results_table(&mut self, ui: &mut Ui) {
        let results: Vec<CpuBenchmarkResult> = self.cpu_partial.lock().unwrap().clone();
        if results.is_empty() {
            return;
        }
        ui.add_space(6.0);
        ui.heading(format!("Results ({})", results.len()));
        ui.label(RichText::new("Open the Results & Graphs tab for charts, temperatures and column toggles.").weak().small());
        egui::ScrollArea::horizontal().show(ui, |ui| {
            egui::Grid::new("cpu_results").striped(true).spacing([18.0, 4.0]).show(ui, |ui| {
                for h in ["Workload", "Threads", "Cores", "Throughput", "Per call", "Clock", "CPU °C (max)", "Hottest core", "Coolest core", "Core avg °C"] {
                    ui.label(RichText::new(h).weak());
                }
                ui.end_row();
                for r in results.iter().rev() {
                    ui.label(format!("{:?}", r.workload));
                    ui.label(r.thread_count.to_string());
                    ui.label(&r.cores);
                    ui.label(RichText::new(format!("{:.2} M calls/s", r.operations_per_second / 1e6)).strong());
                    ui.label(format!("{:.0} ns", r.latency_ns));
                    ui.label(if r.frequency_mhz > 0 { format!("{} MHz", r.frequency_mhz) } else { "—".into() });
                    ui.label(r.telemetry.cpu_temp_max_c.map(|t| format!("{:.0}", t)).unwrap_or_else(|| "—".into()));
                    // per-core sensors: shown for pinned and all-core (OS decides) runs alike
                    let core = |c: Option<(usize, f32)>| c.map(|(c, t)| format!("core {} · {:.0}", c, t)).unwrap_or_else(|| "—".into());
                    ui.label(core(r.telemetry.hottest_core()));
                    ui.label(core(r.telemetry.coolest_core()));
                    ui.label(r.telemetry.core_temp_avg_c.map(|t| format!("{:.1}", t)).unwrap_or_else(|| "—".into()));
                    ui.end_row();
                }
            });
        });
    }

    // ------------------------------------------------------------------ GPU

    pub(super) fn start_gpu_benchmark(&mut self) {
        if self.is_running() {
            return;
        }
        let config = self.gpu_config.clone();
        self.last_gpu_config = Some(config.clone());
        let cancel_flag = self.cancel.clone();
        let sampler = self.begin_sampler();
        *self.gpu_progress.lock().unwrap() = RunProgress::default();
        self.gpu_partial.lock().unwrap().clear();
        let (prog, partial) = (self.gpu_progress.clone(), self.gpu_partial.clone());
        self.mark_running("gpu benchmark", "Running GPU benchmark...");
        self.log("Started GPU benchmark (Vulkan)");

        let running = self.running.clone();
        std::thread::spawn(move || {
            // Vulkan objects are created and destroyed on the worker thread
            let result = GpuBenchmark::new(config).and_then(|bench| {
                bench.with_cancel(cancel_flag).with_progress(prog, partial).with_sensors(sampler).run_owned()
            });
            super::COMPLETE_GPU_RESULT.lock().unwrap().replace(result);
            if let Ok(mut guard) = running.lock() {
                *guard = None;
            }
        });
    }

    pub(super) fn gpu_progress_and_partial(&mut self, ui: &mut Ui) {
        let prog = self.gpu_progress.lock().unwrap().clone();
        progress_panel(ui, &prog, self.is_running());
        let partial: Vec<GpuBenchmarkResult> = self.gpu_partial.lock().unwrap().clone();
        if self.is_running() && !partial.is_empty() {
            ui.label(RichText::new("Finished so far").strong());
            for r in partial.iter().rev() {
                ui.label(format!(
                    "{} elements: {:.3} ms avg, p99 {:.3} ms, {:.1} GOPS{}",
                    r.workload_size,
                    r.avg_latency_ms,
                    r.percentile_99_ms,
                    r.throughput_geops,
                    r.telemetry.gpu_temp_max_c.map(|t| format!(", GPU {:.0} °C", t)).unwrap_or_default()
                ));
            }
        }
    }

    // ------------------------------------------------------------------ input suite

    pub(super) fn start_input_benchmark(&mut self) {
        if self.is_running() {
            return;
        }
        if let Some(e) = self.input_ui.cores.error() {
            self.log(&format!("Cannot start input tests: {}", e));
            return;
        }
        let modes: Vec<InputTestMode> = self.input_ui.modes.iter().filter(|(_, on)| *on).map(|(m, _)| *m).collect();
        if modes.is_empty() {
            self.log("Cannot start input tests: select at least one test");
            return;
        }
        let (ids, _) = self.input_ui.cores.resolve();
        // sweep: one pass per core; otherwise a single pass pinned to the first selected core (or unpinned)
        let passes: Vec<Option<usize>> = if self.input_ui.cores.sweep {
            ids.iter().map(|&c| Some(c)).collect()
        } else {
            vec![ids.first().copied()]
        };
        let base = InputLatencyConfig {
            test_modes: modes,
            sample_count: self.input_ui.samples,
            warmup_samples: 0,
            ..InputLatencyConfig::default()
        };

        let sampler = self.begin_sampler();
        *self.input_progress.lock().unwrap() = RunProgress::default();
        let (cancel_flag, prog) = (self.cancel.clone(), self.input_progress.clone());
        self.mark_running("input tests", "Running input latency tests...");
        self.log(&format!("Started input timing tests on {} core pass(es)", passes.len()));

        let running = self.running.clone();
        std::thread::spawn(move || {
            let result = crate::input_latency::run_passes(base, passes, cancel_flag, prog, sampler);
            super::COMPLETE_INPUT_RESULT.lock().unwrap().replace(result);
            if let Ok(mut guard) = running.lock() {
                *guard = None;
            }
        });
    }

    /// Options block for the input tab's timing suite
    pub(super) fn input_suite_options(&mut self, ui: &mut Ui) {
        ui.add_enabled_ui(!self.is_running(), |ui| {
            egui::CollapsingHeader::new("Timing-suite options").id_salt("input_opts").show(ui, |ui| {
                ui.label(RichText::new("Measures the OS timing / scheduling stack (not real hardware input). Pin it to a core, or sweep every core to spot one with bad timer jitter.").weak().small());
                self.input_ui.cores.ui(ui);
                ui.horizontal_wrapped(|ui| {
                    ui.label("Samples per test:");
                    ui.add(egui::DragValue::new(&mut self.input_ui.samples).range(10..=100_000));
                    for (m, on) in self.input_ui.modes.iter_mut() {
                        ui.checkbox(on, format!("{:?}", m));
                    }
                });
            });
        });
        let prog = self.input_progress.lock().unwrap().clone();
        progress_panel(ui, &prog, self.is_running());
    }
}

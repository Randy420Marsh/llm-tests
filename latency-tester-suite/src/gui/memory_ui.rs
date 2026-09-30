//! Memory tab: manual test selection (cores, sizes, patterns, threads, iterations),
//! live progress, and a results table that fills in while the benchmark runs.

use eframe::egui;
use egui::{Color32, RichText, Ui};
use std::time::Instant;

use super::core_select::CoreSelector;
use super::{human_size, LatencyTesterApp, RunningTaskState};
use crate::memory_benchmark::{
    AccessPattern, MemProgress, MemoryBenchmark, MemoryBenchmarkConfig, MemoryBenchmarkResult,
};
use crate::topology::CoreKind;

const SIZE_PRESETS: [usize; 18] = [
    4 << 10, 16 << 10, 32 << 10, 64 << 10, 128 << 10, 256 << 10, 512 << 10,
    1 << 20, 2 << 20, 4 << 20, 8 << 20, 16 << 20, 32 << 20, 64 << 20,
    128 << 20, 256 << 20, 512 << 20, 1 << 30,
];
const THREAD_PRESETS: [usize; 8] = [1, 2, 4, 8, 12, 16, 24, 32];

/// Everything the user can tweak on the Memory tab
pub(super) struct MemUi {
    pub cores: CoreSelector,
    pub custom_threads: usize,
    pub sizes: Vec<(usize, bool)>,
    pub patterns: Vec<(AccessPattern, bool)>,
    pub threads: Vec<(usize, bool)>,
    pub all_selected_threads: bool,
    pub custom_mb: u32,
    pub quick_size: usize,
}

impl MemUi {
    pub fn new(kinds: Option<Vec<CoreKind>>) -> Self {
        let defaults = MemoryBenchmarkConfig::default();
        Self {
            cores: CoreSelector::new(kinds, "mem"),
            custom_threads: 3,
            sizes: SIZE_PRESETS.iter().map(|&s| (s, defaults.sizes.contains(&s))).collect(),
            patterns: AccessPattern::all_default()
                .into_iter()
                .map(|p| (p, defaults.patterns.contains(&p)))
                .collect(),
            threads: THREAD_PRESETS.iter().map(|&t| (t, t == 1)).collect(),
            all_selected_threads: false,
            custom_mb: 100,
            quick_size: 64 << 20,
        }
    }

    pub fn build_config(&self, base: &MemoryBenchmarkConfig) -> Result<MemoryBenchmarkConfig, String> {
        if let Some(e) = self.cores.error() {
            return Err(e);
        }
        let (core_ids, core_label) = self.cores.resolve();
        let mut cfg = base.clone();
        cfg.core_ids = core_ids;
        cfg.core_label = core_label;
        cfg.per_core = self.cores.sweep;
        cfg.sizes = self.sizes.iter().filter(|(_, on)| *on).map(|(s, _)| *s).collect();
        cfg.patterns = self.patterns.iter().filter(|(_, on)| *on).map(|(p, _)| *p).collect();
        let max = self.cores.max_threads();
        cfg.thread_counts = self.threads.iter().filter(|(_, on)| *on).map(|(t, _)| *t).collect();
        if self.all_selected_threads {
            cfg.thread_counts.push(max);
        }
        cfg.thread_counts.sort_unstable();
        cfg.thread_counts.dedup();
        if cfg.sizes.is_empty() {
            return Err("Select at least one buffer size".to_string());
        }
        if cfg.patterns.is_empty() {
            return Err("Select at least one access pattern".to_string());
        }
        if cfg.effective_thread_counts().is_empty() {
            return Err(format!("No thread count fits the selected cores (max {})", max));
        }
        Ok(cfg)
    }
}

fn fmt_duration(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{:02}:{:02}", s / 60, s % 60)
}

/// Rough RAM needed by the largest selected buffer (data + optional second buffer + chase table)
fn memory_needed(cfg: &MemoryBenchmarkConfig) -> usize {
    let max = cfg.sizes.iter().copied().max().unwrap_or(0);
    let aux = cfg.patterns.iter().any(|p| {
        matches!(p, AccessPattern::StreamCopy | AccessPattern::StreamAdd | AccessPattern::StreamTriad)
    });
    let chase = cfg.patterns.iter().any(|p| matches!(p, AccessPattern::PointerChase));
    max + if aux { max } else { 0 } + if chase { max + max / 8 } else { 0 }
}

impl LatencyTesterApp {
    pub(super) fn start_memory_benchmark(&mut self) {
        if self.is_running() {
            return;
        }
        let config = match self.mem_ui.build_config(&self.mem_config) {
            Ok(c) => c,
            Err(e) => {
                self.log(&format!("Cannot start memory benchmark: {}", e));
                self.mem_config_error = Some(e);
                return;
            }
        };
        self.mem_config_error = None;
        self.cancel.store(false, std::sync::atomic::Ordering::Relaxed);
        *self.mem_progress.lock().unwrap() = MemProgress::default();
        let sampler = self.begin_sampler();
        let bench = MemoryBenchmark::new(config.clone())
            .with_cancel(self.cancel.clone())
            .with_progress(self.mem_progress.clone())
            .with_sensors(sampler);

        *self.running.lock().unwrap() = Some(RunningTaskState {
            kind: "memory benchmark".to_string(),
            started: Instant::now(),
        });
        self.task_status = "Running memory benchmark...".to_string();
        self.log(&format!(
            "Started memory benchmark: {} sizes x {} patterns x {} thread counts = {} tests on {}",
            config.sizes.len(),
            config.patterns.len(),
            config.effective_thread_counts().len() * config.core_groups().len(),
            config.test_count(),
            config.core_label
        ));

        let running = self.running.clone();
        std::thread::spawn(move || {
            let mut bench = bench;
            let result = bench.run();
            super::COMPLETE_MEMORY_RESULT.lock().unwrap().replace(result);
            if let Ok(mut guard) = running.lock() {
                *guard = None;
            }
        });
    }

    pub(super) fn render_memory_tab(&mut self, ui: &mut Ui) {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.heading("Memory Latency Benchmark");
            ui.separator();
            self.mem_controls(ui);
            ui.separator();
            self.mem_run_bar(ui);
            self.mem_progress_panel(ui);
            self.mem_quick_result_panel(ui);
            self.mem_results_table(ui);
        });
    }

    fn mem_controls(&mut self, ui: &mut Ui) {
        let busy = self.is_running();
        ui.add_enabled_ui(!busy, |ui| {
            // ---------- cores ----------
            egui::CollapsingHeader::new("Cores").default_open(true).show(ui, |ui| {
                self.mem_ui.cores.ui(ui);
            });

            // ---------- sizes ----------
            egui::CollapsingHeader::new("Buffer sizes").default_open(true).show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for (size, on) in self.mem_ui.sizes.iter_mut() {
                        ui.checkbox(on, human_size(*size));
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    let mut preset = |ui: &mut Ui, name: &str, pick: &dyn Fn(usize) -> bool| {
                        if ui.small_button(name).clicked() {
                            for (s, on) in self.mem_ui.sizes.iter_mut() {
                                *on = pick(*s);
                            }
                        }
                    };
                    preset(ui, "All", &|_| true);
                    preset(ui, "None", &|_| false);
                    preset(ui, "Caches (≤ 32 MB)", &|s| s <= 32 << 20);
                    preset(ui, "DRAM (≥ 64 MB)", &|s| s >= 64 << 20);
                    preset(ui, "Default", &|s| MemoryBenchmarkConfig::default().sizes.contains(&s));
                    ui.separator();
                    ui.label("Custom:");
                    ui.add(egui::DragValue::new(&mut self.mem_ui.custom_mb).range(1..=8192).suffix(" MB"));
                    if ui.small_button("Add").clicked() {
                        let bytes = self.mem_ui.custom_mb as usize * (1 << 20);
                        match self.mem_ui.sizes.iter_mut().find(|(s, _)| *s == bytes) {
                            Some((_, on)) => *on = true,
                            None => {
                                self.mem_ui.sizes.push((bytes, true));
                                self.mem_ui.sizes.sort_by_key(|(s, _)| *s);
                            }
                        }
                    }
                });
            });

            // ---------- patterns ----------
            let selected_patterns = self.mem_ui.patterns.iter().filter(|(_, on)| *on).count();
            egui::CollapsingHeader::new(format!("Access patterns ({} selected)", selected_patterns))
                .id_salt("patterns")
                .default_open(true)
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        for (pattern, on) in self.mem_ui.patterns.iter_mut() {
                            ui.checkbox(on, pattern.label()).on_hover_text(pattern.describe());
                        }
                    });
                    ui.horizontal_wrapped(|ui| {
                        let mut preset = |ui: &mut Ui, name: &str, pick: &dyn Fn(&AccessPattern) -> bool| {
                            if ui.small_button(name).clicked() {
                                for (p, on) in self.mem_ui.patterns.iter_mut() {
                                    *on = pick(p);
                                }
                            }
                        };
                        preset(ui, "All", &|_| true);
                        preset(ui, "None", &|_| false);
                        preset(ui, "Latency only", &|p| {
                            matches!(p, AccessPattern::PointerChase | AccessPattern::DependentRead | AccessPattern::RandomRead)
                        });
                        preset(ui, "Bandwidth only", &|p| {
                            matches!(
                                p,
                                AccessPattern::SequentialRead | AccessPattern::SequentialWrite
                                    | AccessPattern::StreamCopy | AccessPattern::StreamScale
                                    | AccessPattern::StreamAdd | AccessPattern::StreamTriad
                                    | AccessPattern::IndependentRead
                            )
                        });
                        preset(ui, "Default", &|p| MemoryBenchmarkConfig::default().patterns.contains(p));
                    });
                    ui.label(RichText::new("Hover a pattern to see what it does.").weak().small());
                });

            // ---------- threads + runs ----------
            egui::CollapsingHeader::new("Threads and repetitions").default_open(true).show(ui, |ui| {
                let max = self.mem_ui.cores.max_threads();
                if self.mem_ui.cores.sweep {
                    ui.label(RichText::new("Per-core mode: every core runs with 1 thread.").weak());
                } else {
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Threads:");
                        for (t, on) in self.mem_ui.threads.iter_mut() {
                            if *t <= max {
                                ui.checkbox(on, t.to_string());
                            }
                        }
                        ui.checkbox(&mut self.mem_ui.all_selected_threads, format!("all selected ({})", max));
                        ui.separator();
                        ui.label("Custom:");
                        ui.add(egui::DragValue::new(&mut self.mem_ui.custom_threads).range(1..=max.max(1)));
                        if ui.small_button("Add").clicked() {
                            let n = self.mem_ui.custom_threads;
                            match self.mem_ui.threads.iter_mut().find(|(t, _)| *t == n) {
                                Some((_, on)) => *on = true,
                                None => {
                                    self.mem_ui.threads.push((n, true));
                                    self.mem_ui.threads.sort_by_key(|(t, _)| *t);
                                }
                            }
                        }
                    });
                }
                ui.horizontal_wrapped(|ui| {
                    ui.label("Runs per test:");
                    ui.add(egui::DragValue::new(&mut self.mem_config.iterations).range(1..=100_000));
                    ui.label("Warmup runs:");
                    ui.add(egui::DragValue::new(&mut self.mem_config.warmup_iterations).range(0..=1000));
                    ui.label("Time limit per test:");
                    let mut secs = self.mem_config.time_budget_ms as f64 / 1000.0;
                    if ui
                        .add(egui::DragValue::new(&mut secs).range(0.0..=600.0).speed(0.1).suffix(" s"))
                        .on_hover_text("A test stops repeating after this long (at least 3 runs are always done). 0 = no limit.")
                        .changed()
                    {
                        self.mem_config.time_budget_ms = (secs * 1000.0) as u64;
                    }
                });
            });
        });
    }

    fn mem_run_bar(&mut self, ui: &mut Ui) {
        let cfg = self.mem_ui.build_config(&self.mem_config);
        match &cfg {
            Ok(c) => {
                let tests = c.test_count();
                let worst = if c.time_budget_ms > 0 {
                    format!("at most ~{}", fmt_duration(tests as f64 * (c.time_budget_ms as f64 / 1000.0 * 1.5)))
                } else {
                    "no time limit set".to_string()
                };
                ui.label(format!(
                    "{} sizes × {} patterns × {} thread/core combos = {} tests on {}  ({})",
                    c.sizes.len(),
                    c.patterns.len(),
                    c.effective_thread_counts().len() * c.core_groups().len(),
                    tests,
                    c.core_label,
                    worst
                ));
                let need = memory_needed(c);
                let avail = self.system_info.as_ref().map(|s| s.memory.available as usize).unwrap_or(usize::MAX);
                if need > avail / 10 * 8 {
                    ui.colored_label(
                        Color32::YELLOW,
                        format!("⚠ The largest test needs about {} of RAM; only {} is free.", human_size(need), human_size(avail)),
                    );
                }
            }
            Err(e) => {
                ui.colored_label(Color32::YELLOW, format!("⚠ {}", e));
            }
        }
        if let Some(e) = &self.mem_config_error {
            ui.colored_label(Color32::LIGHT_RED, e);
        }

        ui.horizontal(|ui| {
            let can_run = !self.is_running() && cfg.is_ok();
            if ui.add_enabled(can_run, egui::Button::new(RichText::new("▶ Run selected tests").strong())).clicked() {
                self.start_memory_benchmark();
            }
            if ui.add_enabled(!self.is_running(), egui::Button::new("Quick test")).on_hover_text("One buffer size, random read + pointer chase, 1 thread").clicked() {
                self.start_quick_memory_test(self.mem_ui.quick_size);
            }
            ui.add_enabled_ui(!self.is_running(), |ui| {
                egui::ComboBox::from_id_salt("quick_size")
                    .selected_text(format!("quick: {}", human_size(self.mem_ui.quick_size)))
                    .show_ui(ui, |ui| {
                        for s in [4 << 10, 256 << 10, 4 << 20, 64 << 20, 256 << 20] {
                            ui.selectable_value(&mut self.mem_ui.quick_size, s, human_size(s));
                        }
                    });
                if ui.button("Reset").clicked() {
                    let kinds = self.mem_ui.cores.kinds.clone();
                    self.mem_ui = MemUi::new(kinds);
                    self.mem_config = MemoryBenchmarkConfig::default();
                }
            });
            self.stop_button(ui);
        });
    }

    /// Live "what is being tested right now" panel
    fn mem_progress_panel(&mut self, ui: &mut Ui) {
        let p: MemProgress = self.mem_progress.lock().unwrap().clone();
        if p.total_tests == 0 {
            return;
        }
        let running = self.is_running();
        ui.add_space(6.0);
        ui.group(|ui| {
            let frac = p.done_tests as f32 / p.total_tests as f32;
            let elapsed = p.started.map(|s| s.elapsed().as_secs_f64()).unwrap_or(0.0);
            let eta = if p.done_tests > 0 && running {
                format!(" · ETA ~{}", fmt_duration(elapsed / p.done_tests as f64 * (p.total_tests - p.done_tests) as f64))
            } else {
                String::new()
            };
            ui.add(
                egui::ProgressBar::new(frac)
                    .desired_width(ui.available_width())
                    .text(format!(
                        "{} / {} tests · {} elapsed{}",
                        p.done_tests, p.total_tests, fmt_duration(elapsed), eta
                    )),
            );
            if running {
                if let Some(pattern) = p.pattern {
                    ui.label(
                        RichText::new(format!(
                            "▶ {} · {} · {} thread(s) · {}",
                            human_size(p.size), pattern.label(), p.threads, p.cores
                        ))
                        .size(17.0)
                        .strong(),
                    );
                    let phase = match p.phase.as_str() {
                        "warmup" => format!("warming up (run {}/{})", p.iteration, p.iterations),
                        "measuring" => format!(
                            "measuring run {}/{} · last run {}",
                            p.iteration,
                            p.iterations,
                            fmt_ns(p.last_run_ns)
                        ),
                        "preparing" => "building the access pattern (untimed setup)...".to_string(),
                        other => other.to_string(),
                    };
                    ui.label(phase);
                    ui.label(RichText::new(pattern.describe()).weak());
                } else {
                    ui.label(format!("▶ {} · allocating and filling the buffer...", human_size(p.size)));
                }
            } else {
                ui.label(RichText::new(format!("Finished {} of {} tests", p.done_tests, p.total_tests)).weak());
            }
        });
    }

    fn mem_quick_result_panel(&mut self, ui: &mut Ui) {
        if let Some(err) = &self.quick_mem_error {
            ui.colored_label(Color32::YELLOW, format!("Last quick test: {}", err));
        }
        if let Some(r) = &self.quick_mem_result {
            ui.add_space(6.0);
            ui.group(|ui| {
                ui.label(RichText::new(format!("Quick test — {} buffer", human_size(r.size))).strong());
                egui::Grid::new("quick_mem_result").num_columns(4).spacing([24.0, 6.0]).show(ui, |ui| {
                    ui.label(RichText::new("Pattern").weak());
                    ui.label(RichText::new("Latency / access").weak());
                    ui.label(RichText::new("Bandwidth").weak());
                    ui.label(RichText::new("p99 run").weak());
                    ui.end_row();
                    for row in &r.rows {
                        ui.label(&row.pattern);
                        ui.label(RichText::new(format!("{:.1} ns", row.ns_per_access)).size(22.0).strong().color(Color32::LIGHT_GREEN));
                        ui.label(format!("{:.2} GB/s", row.bandwidth_gb_s));
                        ui.label(format!("{:.2} ms", row.p99_run_ms));
                        ui.end_row();
                    }
                });
            });
        }
    }

    /// Completed tests, newest first; fills in live and stays after a stop
    fn mem_results_table(&mut self, ui: &mut Ui) {
        let completed: Vec<MemoryBenchmarkResult> = self.mem_progress.lock().unwrap().completed.clone();
        if completed.is_empty() {
            return;
        }
        ui.add_space(6.0);
        ui.heading(format!("Results ({})", completed.len()));
        egui::ScrollArea::horizontal().show(ui, |ui| {
            egui::Grid::new("mem_results").striped(true).spacing([18.0, 4.0]).show(ui, |ui| {
                for h in ["Buffer", "Pattern", "Threads", "Cores", "Latency/access", "Bandwidth", "Avg run", "p99 run", "Runs"] {
                    ui.label(RichText::new(h).weak());
                }
                ui.end_row();
                for r in completed.iter().rev() {
                    ui.label(human_size(r.size));
                    ui.label(r.pattern.label());
                    ui.label(r.thread_count.to_string());
                    ui.label(&r.cores);
                    ui.label(RichText::new(format!("{:.1} ns", r.ns_per_access)).strong());
                    ui.label(format!("{:.2} GB/s", r.bandwidth_gb_s));
                    ui.label(fmt_ns(r.latency_ns));
                    ui.label(fmt_ns(r.percentile_99_ns));
                    ui.label(r.iterations.to_string());
                    ui.end_row();
                }
            });
        });
    }
}

fn fmt_ns(ns: f64) -> String {
    if ns >= 1e9 {
        format!("{:.2} s", ns / 1e9)
    } else if ns >= 1e6 {
        format!("{:.2} ms", ns / 1e6)
    } else if ns >= 1e3 {
        format!("{:.1} µs", ns / 1e3)
    } else {
        format!("{:.0} ns", ns)
    }
}

#[cfg(test)]
mod tests {
    use super::super::core_select::CoreMode;
    use super::*;

    fn ui_with(kinds: Option<Vec<CoreKind>>, n: usize) -> MemUi {
        let mut u = MemUi::new(kinds);
        u.cores.specific = vec![false; n];
        u
    }

    #[test]
    fn build_config_validates_selection() {
        let base = MemoryBenchmarkConfig::default();
        let mut u = ui_with(None, 4);
        assert!(u.build_config(&base).is_ok());

        u.cores.mode = CoreMode::Specific; // nothing ticked
        assert!(u.build_config(&base).is_err());
        u.cores.specific[2] = true;
        u.threads.iter_mut().for_each(|(t, on)| *on = *t == 4); // 4 threads on 1 core
        assert!(u.build_config(&base).is_err());
        u.all_selected_threads = true; // = 1 thread
        let cfg = u.build_config(&base).unwrap();
        assert_eq!(cfg.effective_thread_counts(), vec![1]);
        assert_eq!(cfg.core_ids, vec![2]);

        u.patterns.iter_mut().for_each(|(_, on)| *on = false);
        assert!(u.build_config(&base).is_err());
    }

    #[test]
    fn sweep_builds_per_core_config() {
        let base = MemoryBenchmarkConfig::default();
        let mut u = ui_with(None, 4);
        u.cores.sweep = true;
        let cfg = u.build_config(&base).unwrap();
        assert!(cfg.per_core);
        assert_eq!(cfg.core_ids, vec![0, 1, 2, 3]);
        assert_eq!(cfg.core_groups().len(), 4);
        assert_eq!(cfg.test_count(), cfg.sizes.len() * cfg.patterns.len() * 4);
    }
}

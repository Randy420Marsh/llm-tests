//! "Run all tests": the plan editor, the countdown / prompt, live progress, the mouse and keyboard
//! trials (they need the GUI, so the worker in `crate::run_all` asks for them), and what happens
//! when the run ends: everything is signed, saved, exported and shown in the Results & Graphs tab.

use eframe::egui;
use egui::{Color32, RichText, Ui};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::suites_ui::progress_panel;
use super::{LatencyTesterApp, Tab};
use crate::input_test::InputKind;
use crate::memory_benchmark::MemProgress;
use crate::progress::RunProgress;
use crate::run_all::{self, fmt_span, HostInfo, RunAllHandles, RunAllJob, RunAllOutput, RunAllPlan, RunAllStatus, SharedStatus, StepSpec};
use crate::session::Scope;

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Phase {
    Idle,
    /// Waiting for the fresh system data
    Collecting { since: Instant },
    /// The pause before the first test, so the user can get ready
    Countdown { until: Instant },
    Running,
}

/// What the last run produced, shown on the Dashboard afterwards
pub(super) struct RunAllReport {
    pub folder: Option<PathBuf>,
    pub html: Option<PathBuf>,
    pub json: Option<PathBuf>,
    pub files: Vec<String>,
    pub errors: Vec<String>,
    pub cancelled: bool,
    pub steps_done: usize,
    pub steps_total: usize,
    pub took: Duration,
}

pub(super) struct RunAllUi {
    pub plan: RunAllPlan,
    pub phase: Phase,
    status: SharedStatus,
    worker: Option<std::thread::JoinHandle<()>>,
    job: Option<RunAllJob>,
    started: Option<Instant>,
    /// Manual click / key runs that existed before this run (they are not part of its record)
    runs_at_start: usize,
    /// The trial run the worker is waiting for, while the user is doing it
    trial_running: Option<InputKind>,
    /// Trial kinds already answered (the worker needs a moment to notice)
    finished_kinds: Vec<InputKind>,
    skip_trials: bool,
    pub report: Option<RunAllReport>,
}

impl RunAllUi {
    pub fn new() -> Self {
        Self {
            plan: RunAllPlan::default(),
            phase: Phase::Idle,
            status: Arc::new(Mutex::new(RunAllStatus::default())),
            worker: None,
            job: None,
            started: None,
            runs_at_start: 0,
            trial_running: None,
            finished_kinds: Vec::new(),
            skip_trials: false,
            report: None,
        }
    }
}

fn step_icon(i: usize, current: usize, running: bool) -> &'static str {
    if !running {
        "○"
    } else if i < current {
        "✔"
    } else if i == current {
        "▶"
    } else {
        "○"
    }
}

impl LatencyTesterApp {
    fn run_all_host(&self) -> HostInfo {
        HostInfo::detect(self.system_info.as_ref().map(|s| s.memory.available as usize))
    }

    fn run_all_active(&self) -> bool {
        self.run_all.phase != Phase::Idle
    }

    // ------------------------------------------------------------------ start / stop

    pub(super) fn run_all_start(&mut self) {
        if self.is_running() || self.run_all.plan.is_empty() {
            return;
        }
        let host = self.run_all_host();
        let job = self.run_all.plan.job(&host);
        self.run_all_begin(job);
    }

    /// Reset the shown results and start with the system data, then the countdown, then `job`
    fn run_all_begin(&mut self, job: RunAllJob) {
        if !job.skipped_sizes.is_empty() {
            let list: Vec<String> = job.skipped_sizes.iter().map(|&s| super::human_size(s)).collect();
            self.log(&format!("Run all: leaving out buffer size(s) {} because they do not fit in free RAM", list.join(", ")));
        }
        if job.unpinnable_cores > 0 {
            self.log(&format!("Run all: {} core(s) above 63 cannot be pinned individually and are skipped in the per-core CPU test", job.unpinnable_cores));
        }

        // A fresh slate: what is shown and saved afterwards is this run's
        *self.mem_progress.lock().unwrap() = MemProgress::default();
        *self.cpu_progress.lock().unwrap() = RunProgress::default();
        *self.gpu_progress.lock().unwrap() = RunProgress::default();
        *self.input_progress.lock().unwrap() = RunProgress::default();
        self.cpu_partial.lock().unwrap().clear();
        self.gpu_partial.lock().unwrap().clear();
        *self.bench3d.progress.lock().unwrap() = RunProgress::default();
        self.bench3d.partial.lock().unwrap().clear();
        self.bench3d.set_result(None);
        self.last_memory_result = None;
        self.last_cpu_result = None;
        self.last_gpu_result = None;
        self.last_input_result = None;
        self.last_timeline.clear();
        self.last_phases.clear();
        self.sensor_notes.clear();
        self.mem_extra_configs.clear();
        self.cpu_extra_configs.clear();
        self.last_mem_config = None;
        self.last_cpu_config = None;
        self.last_gpu_config = None;

        let est = job.estimate();
        self.log(&format!(
            "Run all tests: {} steps, typically about {}, at most {}",
            job.steps.len(),
            fmt_span(est.expected_s),
            fmt_span(est.worst_s)
        ));
        self.run_all.status = Arc::new(Mutex::new(RunAllStatus::default()));
        self.run_all.job = Some(job);
        self.run_all.started = Some(Instant::now());
        self.run_all.runs_at_start = self.input_test.runs.len();
        self.run_all.trial_running = None;
        self.run_all.finished_kinds.clear();
        self.run_all.skip_trials = false;
        self.run_all.report = None;
        self.mark_running("run all tests", "Run all: collecting system data...");
        self.run_all.phase = Phase::Collecting { since: Instant::now() };
        self.refresh_system_info();
        self.log("Run all: collecting system data first");
    }

    fn run_all_abort(&mut self, why: &str) {
        *self.running.lock().unwrap() = None;
        self.run_all.phase = Phase::Idle;
        self.run_all.job = None;
        self.task_status = "Idle".to_string();
        self.log(why);
    }

    fn run_all_launch(&mut self) {
        let Some(job) = self.run_all.job.clone() else { return };
        let sampler = self.begin_sampler();
        let handles = RunAllHandles {
            mem: self.mem_progress.clone(),
            cpu_progress: self.cpu_progress.clone(),
            cpu_partial: self.cpu_partial.clone(),
            gpu_progress: self.gpu_progress.clone(),
            gpu_partial: self.gpu_partial.clone(),
            gpu3d_progress: self.bench3d.progress.clone(),
            gpu3d_partial: self.bench3d.partial.clone(),
            input_progress: self.input_progress.clone(),
        };
        self.run_all.worker = Some(run_all::spawn(job, handles, sampler, self.cancel.clone(), self.run_all.status.clone()));
        self.run_all.phase = Phase::Running;
        self.task_status = "Run all tests running...".to_string();
        self.log("Run all: started");
    }

    /// Called every frame: moves the run through its phases and answers the worker's requests
    pub(super) fn run_all_tick(&mut self) {
        let cancelled = self.cancel.load(std::sync::atomic::Ordering::Relaxed);
        match self.run_all.phase {
            Phase::Idle => {}
            Phase::Collecting { since } => {
                if cancelled {
                    self.run_all_abort("Run all cancelled before it started");
                } else if !self.info_refreshing && self.last_refresh > since {
                    let secs = self.run_all.plan.countdown_s;
                    self.log(&format!("Run all: system data collected, starting in {} s", secs));
                    self.run_all.phase = Phase::Countdown { until: Instant::now() + Duration::from_secs(secs) };
                }
            }
            Phase::Countdown { until } => {
                if cancelled {
                    self.run_all_abort("Run all cancelled before it started");
                } else if Instant::now() >= until {
                    self.run_all_launch();
                }
            }
            Phase::Running => {
                let (waiting, output, lines) = {
                    let mut s = self.run_all.status.lock().unwrap_or_else(|e| e.into_inner());
                    (s.waiting_for_user, s.output.take(), std::mem::take(&mut s.log))
                };
                for l in lines {
                    self.log(&format!("Run all: {}", l));
                }
                if let Some(kind) = waiting {
                    self.run_all_drive_trials(kind);
                }
                if let Some(out) = output {
                    if let Some(h) = self.run_all.worker.take() {
                        let _ = h.join();
                    }
                    self.run_all_finalize(out);
                } else if self.run_all.worker.as_ref().is_some_and(|h| h.is_finished()) {
                    // The worker ended without reporting: keep what finished
                    self.run_all.worker = None;
                    self.run_all_finalize(RunAllOutput {
                        errors: vec!["the test worker stopped unexpectedly".into()],
                        cancelled: true,
                        ..Default::default()
                    });
                }
            }
        }
    }

    /// The worker asked for a mouse / keyboard trial run: put the trial screen up, start it, and
    /// tell the worker when it is over
    fn run_all_drive_trials(&mut self, kind: InputKind) {
        if self.run_all.finished_kinds.contains(&kind) {
            // Already answered; the worker notices within ~100 ms. Do not touch the tab meanwhile,
            // or the switch to the Dashboard after the last trial is undone.
            return;
        }
        // The trials only advance while the trial screen is drawn, so keep it in front for now
        self.tab = Tab::Input;
        let answer = |app: &mut Self| {
            run_all::finish_user_step(&app.run_all.status);
            app.run_all.finished_kinds.push(kind);
            app.run_all.trial_running = None;
            if kind == InputKind::KeyPress {
                app.tab = Tab::Dashboard; // the rest runs by itself: show the progress
            }
        };
        if self.run_all.skip_trials {
            self.input_test.engine.abort();
            answer(self);
            return;
        }
        match self.run_all.trial_running {
            Some(k) if k == kind => {
                if !self.input_test.engine.is_active() {
                    answer(self); // finished (or aborted with the Abort button)
                }
            }
            _ => {
                self.input_test.begin_run(kind);
                self.run_all.trial_running = Some(kind);
                self.log(&format!("Run all: your turn, {} test ({} trials)", kind.label().to_lowercase(), self.input_test.cfg.trials));
            }
        }
    }

    // ------------------------------------------------------------------ finish: save, export, show

    fn run_all_finalize(&mut self, out: RunAllOutput) {
        let took = self.run_all.started.map(|t| t.elapsed()).unwrap_or_default();
        *self.running.lock().unwrap() = None;
        self.run_all.phase = Phase::Idle;
        self.finish_sampler_if_idle();
        self.task_status = "Idle".to_string();
        let job = self.run_all.job.take().unwrap_or_default();

        self.last_input_result = out.input_suite;
        self.last_cpu_result = out.cpu_summary;
        self.last_gpu_result = out.gpu_summary;
        self.bench3d.set_result(out.gpu3d_summary);
        self.last_mem_config = out.memory_configs.first().cloned();
        self.mem_extra_configs = out.memory_configs.iter().skip(1).cloned().collect();
        self.last_cpu_config = out.cpu_configs.first().cloned();
        self.cpu_extra_configs = out.cpu_configs.iter().skip(1).cloned().collect();
        self.last_gpu_config = out.gpu_config.clone();

        if out.cancelled {
            self.log("Run all: stopped early, saving what finished");
        }
        for e in &out.errors {
            self.log(&format!("Run all: step failed: {}", e));
        }

        let skip = self.run_all.runs_at_start.min(self.input_test.runs.len());
        let run_info = serde_json::json!({
            "plan": self.run_all.plan,
            "steps": job.steps.iter().map(|s| s.label()).collect::<Vec<_>>(),
            "steps_done": out.steps_done,
            "steps_total": out.steps_total.max(job.steps.len()),
            "cancelled": out.cancelled,
            "errors": out.errors,
            "duration_s": took.as_secs_f64(),
            "buffer_sizes_left_out_for_ram": job.skipped_sizes,
        });
        let saved = self.log_session_from(Scope::Everything, skip, Some(run_info));

        let (folder, html, files, notes) = self.run_all_export(saved.as_ref().map(|s| &s.1), skip, &out.errors, out.cancelled, took);
        for n in notes {
            self.log(&n);
        }
        self.run_all.report = Some(RunAllReport {
            folder,
            html,
            json: saved.map(|s| s.1),
            files,
            errors: out.errors,
            cancelled: out.cancelled,
            steps_done: out.steps_done,
            steps_total: out.steps_total.max(job.steps.len()),
            took,
        });
        self.tab = Tab::Graphs;
        self.log(&format!("Run all: done in {}. Results are shown in Results & Graphs", fmt_span(took.as_secs_f64())));
    }

    /// CSV per suite, the HTML report and a text summary in one folder next to the signed JSON.
    /// Returns (folder, html report, file names, log lines).
    fn run_all_export(
        &self,
        json_path: Option<&PathBuf>,
        skip_trials: usize,
        errors: &[String],
        cancelled: bool,
        took: Duration,
    ) -> (Option<PathBuf>, Option<PathBuf>, Vec<String>, Vec<String>) {
        let mut notes = Vec::new();
        let dir = self.results_dir().join(format!("run_all_{}", chrono::Local::now().format("%Y%m%d_%H%M%S")));
        if let Err(e) = std::fs::create_dir_all(&dir) {
            notes.push(format!("Run all: could not create {}: {}", dir.display(), e));
            return (None, None, Vec::new(), notes);
        }
        let mut names = Vec::new();
        let snap = self.session_snapshot(skip_trials);
        let data = self.session_data(&snap, None);
        for (name, text) in crate::session::csv_files(&data) {
            match std::fs::write(dir.join(&name), text) {
                Ok(()) => names.push(name),
                Err(e) => notes.push(format!("Run all: could not write {}: {}", name, e)),
            }
        }
        let mut html = None;
        if let Some(path) = json_path {
            let entries = crate::report::load_files(std::slice::from_ref(path));
            if entries.is_empty() {
                notes.push("Run all: the saved record could not be read back for the HTML report".into());
            } else {
                let file = dir.join("report.html");
                match std::fs::write(&file, crate::report::render_static(&entries)) {
                    Ok(()) => {
                        names.push("report.html".into());
                        html = Some(file);
                    }
                    Err(e) => notes.push(format!("Run all: could not write the HTML report: {}", e)),
                }
            }
        }
        let mut summary = String::new();
        summary.push_str("Latency Tester Suite: run all tests\n");
        summary.push_str(&format!("Finished: {}\n", chrono::Local::now().format("%Y-%m-%d %H:%M:%S")));
        summary.push_str(&format!("Duration: {}\n", fmt_span(took.as_secs_f64())));
        summary.push_str(if cancelled { "Ended: stopped early (partial results)\n" } else { "Ended: completed\n" });
        for e in errors {
            summary.push_str(&format!("Step failed: {}\n", e));
        }
        if let Some(sys) = &self.system_info {
            summary.push_str(&format!(
                "\nSystem\n  CPU: {} ({} cores, {} threads)\n  Memory: {:.1} GB\n  OS: {}\n",
                sys.cpu.name,
                sys.cpu.cores,
                sys.cpu.threads,
                sys.memory.total as f64 / 1_073_741_824.0,
                sys.os.name
            ));
            if let Some(g) = &sys.gpu {
                summary.push_str(&format!("  GPU: {}\n", g.name));
            }
        }
        summary.push_str("\nResults\n");
        summary.push_str(&format!(
            "  memory tests: {}\n  cpu tests: {}\n  gpu tests: {}\n  input timing results: {}\n  click / key runs: {}\n  sensor samples: {}\n",
            snap.mem.len(),
            snap.cpu.len(),
            snap.gpu.len(),
            self.last_input_result.as_ref().map_or(0, |s| s.results.len()),
            snap.trials.len(),
            self.last_timeline.len()
        ));
        if let Some(path) = json_path {
            summary.push_str(&format!("\nSigned record: {}\n", path.display()));
        }
        if let Some((verified, verification)) = &self.last_logged {
            summary.push_str(&format!(
                "Signature: {}\nPublic key: {}\nVerification: {}\n",
                verified.signature.sig, verified.signature.public_key, verification.message
            ));
        }
        summary.push_str(&format!("\nFiles in this folder: {}\n", names.join(", ")));
        if std::fs::write(dir.join("summary.txt"), summary).is_ok() {
            names.push("summary.txt".into());
        }
        notes.push(format!("Run all: exported {} file(s) to {}", names.len(), dir.display()));
        (Some(dir), html, names, notes)
    }

    // ------------------------------------------------------------------ screens

    /// The panel at the top of the Dashboard: plan editor, progress, and the last run's files
    pub(super) fn render_run_all(&mut self, ui: &mut Ui) {
        ui.group(|ui| {
            ui.heading("▶ Run all tests");
            if self.run_all_active() {
                self.run_all_progress(ui);
            } else {
                self.run_all_plan_editor(ui);
                self.run_all_report_ui(ui);
            }
        });
    }

    fn run_all_plan_editor(&mut self, ui: &mut Ui) {
        ui.label(
            "Collects the system data, waits, then runs every test one after the other, saves the signed result, \
             exports CSV / HTML files and shows the graphs.",
        );
        ui.label(
            RichText::new("👉 The first test needs you: mouse clicks, then key presses (about a minute). After that everything runs by itself.")
                .color(Color32::from_rgb(255, 200, 90))
                .strong(),
        );
        let idle = !self.is_running();
        let mut plan = self.run_all.plan.clone();
        ui.add_enabled_ui(idle, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Full profile").on_hover_text("Everything, with the numbers below (takes hours)").clicked() {
                    plan = RunAllPlan::default();
                }
                if ui.button("Quick check").on_hover_text("Small buffers, short CPU runs: a few minutes, to see that everything works").clicked() {
                    plan = RunAllPlan::quick();
                }
            });
            egui::Grid::new("run_all_plan").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
                ui.checkbox(&mut plan.interactive_input, "Input latency (you)");
                ui.label(RichText::new("mouse click and key press, 10 trials each; needs a person, so it runs first").weak());
                ui.end_row();

                ui.checkbox(&mut plan.auto_input, "Input timing tests");
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("timer, jitter, polling: no interaction. Samples").weak());
                    ui.add(egui::DragValue::new(&mut plan.input_samples).range(10..=100_000));
                    ui.checkbox(&mut plan.input_each_core, "and every core on its own");
                });
                ui.end_row();

                ui.checkbox(&mut plan.memory, "Memory");
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("all access patterns, sizes up to").weak());
                    egui::ComboBox::from_id_salt("run_all_max_size")
                        .selected_text(super::human_size(plan.mem_max_size))
                        .show_ui(ui, |ui| {
                            for s in [16usize << 20, 64 << 20, 256 << 20, 1 << 30] {
                                ui.selectable_value(&mut plan.mem_max_size, s, super::human_size(s));
                            }
                        });
                    ui.checkbox(&mut plan.mem_all_threads, "all thread counts");
                    ui.label(RichText::new("runs").weak());
                    ui.add(egui::DragValue::new(&mut plan.mem_iterations).range(1..=1000));
                    ui.label(RichText::new("warmup").weak());
                    ui.add(egui::DragValue::new(&mut plan.mem_warmup).range(0..=100));
                    ui.label(RichText::new("time limit per test").weak());
                    ui.add(egui::DragValue::new(&mut plan.mem_time_limit_s).range(0.0..=600.0).speed(0.1).suffix(" s"));
                    ui.checkbox(&mut plan.memory_each_core, "also every core on its own")
                        .on_hover_text("Thousands more tests: every size x pattern on each core. Off by default.");
                });
                ui.end_row();

                ui.checkbox(&mut plan.cpu, "CPU");
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("all workloads on all cores. Run length").weak());
                    ui.add(egui::DragValue::new(&mut plan.cpu_run_s).range(1..=600).suffix(" s"));
                    ui.label(RichText::new("runs").weak());
                    ui.add(egui::DragValue::new(&mut plan.cpu_runs).range(1..=1000));
                    ui.label(RichText::new("warmup").weak());
                    ui.add(egui::DragValue::new(&mut plan.cpu_warmup_s).range(0..=600).suffix(" s"));
                    ui.checkbox(&mut plan.cpu_each_core, "and every core on its own:");
                    ui.add_enabled_ui(plan.cpu_each_core, |ui| {
                        ui.add(egui::DragValue::new(&mut plan.cpu_core_run_s).range(1..=600).suffix(" s"))
                            .on_hover_text("Run length per core. The per-core pass is workloads × cores tests, so keep it short");
                        ui.label(RichText::new("×").weak());
                        ui.add(egui::DragValue::new(&mut plan.cpu_core_runs).range(1..=1000));
                        ui.label(RichText::new("warmup").weak());
                        ui.add(egui::DragValue::new(&mut plan.cpu_core_warmup_s).range(0..=600).suffix(" s"));
                    });
                });
                ui.end_row();

                ui.checkbox(&mut plan.gpu, "GPU (Vulkan)");
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("six sizes, each dispatched for at least").weak());
                    ui.add(egui::DragValue::new(&mut plan.gpu_min_sample_ms).range(0..=60_000).suffix(" ms"));
                });
                ui.end_row();

                ui.checkbox(&mut plan.gpu3d, "3D graphics");
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("720p, 1080p, 1440p and 4K, rendered for").weak());
                    ui.add(egui::DragValue::new(&mut plan.gpu3d_duration_s).range(1.0..=300.0).suffix(" s"));
                    ui.label(RichText::new("each (+ latency and warm-up)").weak());
                });
                ui.end_row();

                ui.label("Per-core order");
                ui.horizontal_wrapped(|ui| {
                    ui.radio_value(&mut plan.core_by_core, false, "rotate cores between tests (cooler)");
                    ui.radio_value(&mut plan.core_by_core, true, "all tests on one core, then the next");
                });
                ui.end_row();

                ui.label("Wait before the first test");
                ui.add(egui::DragValue::new(&mut plan.countdown_s).range(0..=600).suffix(" s"));
                ui.end_row();
            });
        });
        self.run_all.plan = plan.clone();

        let job = plan.job(&self.run_all_host());
        let est = job.estimate();
        egui::CollapsingHeader::new(format!("{} steps · typically about {} · at most {}", job.steps.len(), fmt_span(est.expected_s), fmt_span(est.worst_s)))
            .id_salt("run_all_steps")
            .show(ui, |ui| {
                for (i, s) in job.steps.iter().enumerate() {
                    ui.label(format!("{}. {}  ·  ~{}", i + 1, s.label(), fmt_span(s.estimate().expected_s)));
                }
                if !job.skipped_sizes.is_empty() {
                    ui.colored_label(Color32::YELLOW, "Some buffer sizes do not fit in free RAM and are left out.");
                }
            });
        if est.expected_s > 3.0 * 3600.0 {
            ui.colored_label(
                Color32::from_rgb(255, 200, 90),
                format!(
                    "⚠ This profile takes about {} (the per-core CPU test alone is one run per workload per core). \
                     Try \"Quick check\" first, or untick a part to shorten it. Stop keeps and saves what already finished.",
                    fmt_span(est.expected_s)
                ),
            );
        }
        ui.horizontal(|ui| {
            let can = idle && !plan.is_empty() && self.logger.is_some();
            if ui.add_enabled(can, egui::Button::new(RichText::new("▶ Run all tests").size(18.0).strong())).clicked() {
                self.run_all_start();
            }
            if self.logger.is_none() {
                ui.colored_label(Color32::LIGHT_RED, "no result logger: results could not be saved");
            } else if !idle {
                ui.label(RichText::new("another test is running").weak());
            }
        });
    }

    fn run_all_report_ui(&mut self, ui: &mut Ui) {
        let Some(r) = &self.run_all.report else { return };
        ui.separator();
        ui.label(RichText::new("Last run").strong());
        ui.label(format!(
            "{} · {} of {} steps · {}",
            if r.cancelled { "stopped early" } else { "completed" },
            r.steps_done,
            r.steps_total,
            fmt_span(r.took.as_secs_f64())
        ));
        for e in &r.errors {
            ui.colored_label(Color32::YELLOW, format!("⚠ {}", e));
        }
        if let Some(j) = &r.json {
            ui.label(format!("Signed record: {}", j.display()));
        }
        if let Some(f) = &r.folder {
            ui.label(format!("Exports ({}): {}", r.files.join(", "), f.display()));
        }
        let (html, tab_graphs) = (r.html.clone(), ui.button("Show results & graphs").clicked());
        if tab_graphs {
            self.tab = Tab::Graphs;
        }
        if let Some(h) = html {
            if ui.button("Open the HTML report").clicked() {
                let path = std::fs::canonicalize(&h).unwrap_or(h);
                crate::server::open_in_browser(&format!("file:///{}", path.to_string_lossy().trim_start_matches(r"\\?\").replace('\\', "/")));
            }
        }
    }

    fn run_all_progress(&mut self, ui: &mut Ui) {
        let (step, total, waiting) = {
            let s = self.run_all.status.lock().unwrap_or_else(|e| e.into_inner());
            (s.step, s.steps_total, s.waiting_for_user)
        };
        let running = self.run_all.phase == Phase::Running;
        match self.run_all.phase {
            Phase::Collecting { .. } => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Collecting system data…");
                });
            }
            Phase::Countdown { until } => {
                let left = until.saturating_duration_since(Instant::now()).as_secs() + 1;
                ui.label(RichText::new(format!("Starting in {} s", left)).size(18.0).strong());
            }
            _ => {}
        }
        let Some(job) = self.run_all.job.clone() else { return };
        let elapsed = self.run_all.started.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
        let est = job.estimate();
        ui.label(format!(
            "elapsed {} · typically about {} in total · at most {}",
            fmt_span(elapsed),
            fmt_span(est.expected_s),
            fmt_span(est.worst_s)
        ));
        ui.add_space(4.0);
        for (i, s) in job.steps.iter().enumerate() {
            let text = format!("{} {}. {}", step_icon(i, step, running), i + 1, s.label());
            let text = if running && i == step { RichText::new(text).strong() } else if running && i < step { RichText::new(text).weak() } else { RichText::new(text) };
            ui.label(text);
        }
        if waiting.is_some() {
            ui.add_space(4.0);
            ui.colored_label(Color32::from_rgb(255, 200, 90), "👉 Your turn: use the trial screen. Everything after this runs by itself.");
        }
        if running && step < total.max(job.steps.len()) {
            ui.add_space(6.0);
            match job.steps.get(step) {
                Some(StepSpec::Memory { .. }) => self.mem_progress_panel(ui),
                Some(StepSpec::Cpu { .. }) => {
                    let p = self.cpu_progress.lock().unwrap().clone();
                    progress_panel(ui, &p, true);
                }
                Some(StepSpec::Gpu { .. }) => self.gpu_progress_and_partial(ui),
                Some(StepSpec::Gpu3d { .. }) => {
                    let p = self.bench3d.progress.lock().unwrap().clone();
                    progress_panel(ui, &p, true);
                }
                Some(StepSpec::InputSuite { .. }) => {
                    let p = self.input_progress.lock().unwrap().clone();
                    progress_panel(ui, &p, true);
                }
                _ => {}
            }
        }
    }

    /// Thin bar under the title while a run is active (every tab)
    pub(super) fn run_all_bar(&mut self, ctx: &egui::Context) {
        if !self.run_all_active() {
            return;
        }
        egui::TopBottomPanel::top("run_all_bar").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                match self.run_all.phase {
                    Phase::Collecting { .. } => {
                        ui.label(RichText::new("▶ Run all tests: collecting system data…").strong());
                    }
                    Phase::Countdown { until } => {
                        let left = until.saturating_duration_since(Instant::now()).as_secs() + 1;
                        ui.label(RichText::new(format!("▶ Run all tests: starting in {} s", left)).strong());
                    }
                    _ => {
                        let (step, total, label, waiting) = {
                            let s = self.run_all.status.lock().unwrap_or_else(|e| e.into_inner());
                            (s.step, s.steps_total, s.label.clone(), s.waiting_for_user)
                        };
                        ui.label(RichText::new(format!("▶ Run all tests: step {}/{} · {}", step + 1, total.max(1), label)).strong());
                        if let Some(kind) = waiting {
                            let what = match kind {
                                InputKind::MouseClick => "click when the screen turns GREEN",
                                InputKind::KeyPress => "press any key when the screen turns GREEN",
                            };
                            ui.label(RichText::new(format!("👉 Your turn: {}", what)).color(Color32::from_rgb(255, 200, 90)).strong());
                            if ui.button("Skip the click / key tests").clicked() {
                                self.run_all.skip_trials = true;
                            }
                        }
                    }
                }
            });
        });
    }

    /// The "get ready" prompt while the data is collected and the countdown runs
    pub(super) fn run_all_overlay(&mut self, ctx: &egui::Context) {
        let phase = self.run_all.phase;
        if !matches!(phase, Phase::Collecting { .. } | Phase::Countdown { .. }) {
            return;
        }
        let interactive = self.run_all.plan.interactive_input;
        egui::Window::new("Run all tests")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(460.0);
                match phase {
                    Phase::Collecting { .. } => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(RichText::new("Collecting system data…").size(18.0));
                        });
                    }
                    Phase::Countdown { until } => {
                        let left = until.saturating_duration_since(Instant::now()).as_secs() + 1;
                        ui.label(RichText::new(format!("Starting in {} s", left)).size(26.0).strong());
                    }
                    _ => {}
                }
                ui.add_space(6.0);
                if interactive {
                    ui.colored_label(
                        Color32::from_rgb(255, 200, 90),
                        RichText::new("⚠ The first test needs you.").size(17.0).strong(),
                    );
                    ui.label(
                        "The Input Latency page opens: wait for the screen to turn GREEN, then click the mouse (10 trials), \
                         then press a key when it turns green (10 trials). About a minute.\n\
                         Everything after that runs by itself, so you can walk away.",
                    );
                } else {
                    ui.label("Everything runs by itself from here.");
                }
                ui.add_space(6.0);
                if ui.button("Cancel").clicked() {
                    self.stop_running();
                }
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu_benchmark::{AffinityMode, CpuBenchmarkConfig, WorkloadType};
    use crate::input_latency::{InputLatencyConfig, InputTestMode};
    use crate::memory_benchmark::{AccessPattern, MemoryBenchmarkConfig};

    fn tiny_job() -> RunAllJob {
        let mem = |label: &str, each_core: bool| StepSpec::Memory {
            label: label.into(),
            config: MemoryBenchmarkConfig {
                sizes: vec![64 << 10],
                patterns: vec![AccessPattern::SequentialRead, AccessPattern::PointerChase],
                iterations: 3,
                warmup_iterations: 0,
                thread_counts: vec![1],
                time_budget_ms: 0,
                core_ids: if each_core { vec![0] } else { Vec::new() },
                per_core: each_core,
                core_label: label.into(),
                ..MemoryBenchmarkConfig::default()
            },
        };
        RunAllJob {
            steps: vec![
                StepSpec::Interactive(InputKind::MouseClick),
                StepSpec::Interactive(InputKind::KeyPress),
                StepSpec::InputSuite {
                    base: InputLatencyConfig { test_modes: vec![InputTestMode::Jitter], sample_count: 50, warmup_samples: 0, ..InputLatencyConfig::default() },
                    passes: vec![None],
                },
                mem("all cores", false),
                mem("each core", true),
                StepSpec::Cpu {
                    label: "all cores".into(),
                    config: CpuBenchmarkConfig {
                        workload_types: vec![WorkloadType::IntegerAdd],
                        thread_counts: vec![1],
                        affinity_modes: vec![AffinityMode::AllCores],
                        duration_seconds: 1,
                        warmup_seconds: 0,
                        iterations: 1,
                        core_by_core: false,
                    },
                },
            ],
            ..Default::default()
        }
    }

    /// The app hands system data over through process-wide statics, so only one app may run at a time
    static ONE_APP_AT_A_TIME: Mutex<()> = Mutex::new(());

    /// Frame loop without a window: what `update` does for the run
    fn pump(app: &mut LatencyTesterApp) {
        app.check_completed_tasks();
        app.run_all_tick();
    }

    #[test]
    fn a_run_goes_from_system_data_to_saved_exports() {
        let _one = ONE_APP_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let mut app = LatencyTesterApp::with_results_dir(dir.path().to_path_buf());
        app.run_all.plan.countdown_s = 1;
        app.run_all_begin(tiny_job());
        assert!(matches!(app.run_all.phase, Phase::Collecting { .. }));
        assert!(app.is_running(), "the other Run buttons are disabled for the whole run");

        let started = Instant::now();
        let (mut saw_countdown, mut trials_asked) = (false, Vec::new());
        while app.run_all.phase != Phase::Idle {
            assert!(started.elapsed() < Duration::from_secs(180), "run did not finish; log:\n{}", app.log_text);
            pump(&mut app);
            saw_countdown |= matches!(app.run_all.phase, Phase::Countdown { .. });
            if let Some(kind) = app.run_all.trial_running {
                if !trials_asked.contains(&kind) {
                    trials_asked.push(kind);
                    // the trial screen is up and the Input tab is forced
                    assert!(matches!(app.tab, Tab::Input));
                    assert!(app.input_test.engine.is_active(), "the trials start by themselves");
                    // stand-in for a person who gives up: the Abort button
                    app.input_test.engine.abort();
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(saw_countdown, "there is a wait between the system data and the first test");
        assert_eq!(trials_asked, vec![InputKind::MouseClick, InputKind::KeyPress], "the interactive tests come first, mouse then keyboard");
        assert!(!app.is_running());

        // results are in the app and on screen
        assert!(matches!(app.tab, Tab::Graphs));
        assert_eq!(app.mem_progress.lock().unwrap().completed.len(), 4, "both memory passes");
        assert_eq!(app.cpu_partial.lock().unwrap().len(), 1);
        assert_eq!(app.last_input_result.as_ref().map(|s| s.results.len()), Some(1));
        assert_eq!(app.mem_extra_configs.len(), 1);
        assert!(!app.last_timeline.is_empty(), "the sensor timeline covers the run");

        // saved, signed, exported
        let report = app.run_all.report.as_ref().expect("report");
        assert_eq!((report.steps_done, report.steps_total, report.cancelled), (6, 6, false), "errors: {:?}", report.errors);
        let json = report.json.clone().expect("signed record saved");
        assert!(json.exists());
        let (verified, verification) = app.last_logged.clone().unwrap();
        assert!(verification.valid, "{}", verification.message);
        assert_eq!(verified.payload.test_type, "session");
        let results = &verified.payload.benchmark_results;
        assert_eq!(results["memory"]["results"].as_array().unwrap().len(), 4);
        assert_eq!(results["cpu"]["results"].as_array().unwrap().len(), 1);
        assert!(results["input_timing_suite"].is_object() && results["sensors"].is_object());
        assert_eq!(verified.payload.benchmark_config["run_all"]["steps_done"], 6);
        assert_eq!(verified.payload.benchmark_config["memory_extra_passes"].as_array().unwrap().len(), 1);

        let folder = report.folder.clone().expect("export folder");
        for f in ["memory.csv", "cpu.csv", "input_timing.csv", "sensors.csv", "report.html", "summary.txt"] {
            assert!(folder.join(f).exists(), "missing {} (have {:?})", f, report.files);
        }
        let summary = std::fs::read_to_string(folder.join("summary.txt")).unwrap();
        assert!(summary.len() < 4000 && summary.contains("memory tests: 4") && summary.contains("Verification: "), "{}", summary);
        assert_eq!(std::fs::read_to_string(folder.join("memory.csv")).unwrap().lines().count(), 5, "header + 4 tests");
        let html = std::fs::read_to_string(folder.join("report.html")).unwrap();
        assert!(html.contains("\"benchmark_results\"") && !html.contains("/*__EMBEDDED__*/null"), "report embeds the record");
        assert!(app.log_text.contains("Run all: done in"), "{}", app.log_text);
    }

    #[test]
    fn stop_during_the_countdown_runs_nothing_and_saves_nothing() {
        let _one = ONE_APP_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let mut app = LatencyTesterApp::with_results_dir(dir.path().to_path_buf());
        app.run_all.plan.countdown_s = 600;
        app.run_all_begin(tiny_job());
        let started = Instant::now();
        while !matches!(app.run_all.phase, Phase::Countdown { .. }) {
            assert!(started.elapsed() < Duration::from_secs(60), "never reached the countdown");
            pump(&mut app);
            std::thread::sleep(Duration::from_millis(20));
        }
        app.stop_running();
        pump(&mut app);
        assert!(app.run_all.phase == Phase::Idle && !app.is_running());
        assert!(app.run_all.report.is_none() && app.mem_progress.lock().unwrap().completed.is_empty());
        assert!(std::fs::read_dir(dir.path()).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().ends_with(".json") || e.file_name().to_string_lossy().contains("key")), "no result record was written");
    }

    #[test]
    fn skip_answers_the_click_and_key_tests_without_a_person() {
        let _one = ONE_APP_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let mut app = LatencyTesterApp::with_results_dir(dir.path().to_path_buf());
        app.run_all.plan.countdown_s = 0;
        let mut job = tiny_job();
        job.steps.truncate(2); // only the two interactive steps
        app.run_all_begin(job);
        app.run_all.skip_trials = true;
        let started = Instant::now();
        while app.run_all.phase != Phase::Idle {
            assert!(started.elapsed() < Duration::from_secs(90), "{}", app.log_text);
            pump(&mut app);
            std::thread::sleep(Duration::from_millis(20));
        }
        let report = app.run_all.report.as_ref().unwrap();
        assert_eq!((report.steps_done, report.cancelled), (2, false));
        assert!(matches!(app.tab, Tab::Graphs));
    }

    #[test]
    fn every_screen_renders_in_every_phase() {
        let _one = ONE_APP_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let mut app = LatencyTesterApp::with_results_dir(dir.path().to_path_buf());
        let ctx = egui::Context::default();
        let frame = |app: &mut LatencyTesterApp| {
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                app.run_all_bar(ctx);
                egui::CentralPanel::default().show(ctx, |ui| app.render_run_all(ui));
                app.run_all_overlay(ctx);
            });
        };
        // idle: plan editor with every control
        frame(&mut app);
        app.run_all.plan = RunAllPlan::quick();
        frame(&mut app);
        // collecting / countdown / running with each kind of step on screen
        app.run_all.job = Some(tiny_job());
        for phase in [
            Phase::Collecting { since: Instant::now() },
            Phase::Countdown { until: Instant::now() + Duration::from_secs(5) },
            Phase::Running,
        ] {
            app.run_all.phase = phase;
            for step in 0..6 {
                app.run_all.status.lock().unwrap().step = step;
                app.run_all.status.lock().unwrap().steps_total = 6;
                app.run_all.status.lock().unwrap().waiting_for_user = if step < 2 { Some(InputKind::MouseClick) } else { None };
                frame(&mut app);
            }
        }
        // after a run: the report block
        app.run_all.phase = Phase::Idle;
        app.run_all.report = Some(RunAllReport {
            folder: Some(dir.path().join("run_all_x")),
            html: None,
            json: Some(dir.path().join("a.json")),
            files: vec!["memory.csv".into()],
            errors: vec!["GPU (Vulkan): no device".into()],
            cancelled: true,
            steps_done: 3,
            steps_total: 6,
            took: Duration::from_secs(3700),
        });
        frame(&mut app);
    }
}

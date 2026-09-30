//! GPU tab, "3D graphics benchmark" (see `bench3d`)

use eframe::egui;
use egui::{Color32, RichText, Ui};
use egui_plot::{Line, Plot, PlotPoints};
use std::sync::{Arc, Mutex};

use super::LatencyTesterApp;
use crate::bench3d::{Bench3d, Bench3dConfig, Bench3dResult, Bench3dSummary, Detail, RESOLUTIONS};
use crate::progress::{RunProgress, SharedProgress, SharedResults};

type Done = Arc<Mutex<Option<anyhow::Result<Bench3dSummary>>>>;

pub(super) struct Bench3dUi {
    pub cfg: Bench3dConfig,
    pub progress: SharedProgress,
    pub partial: SharedResults<Bench3dResult>,
    pub last: Option<Bench3dSummary>,
    done: Done,
    preview: Option<egui::TextureHandle>,
    preview_dirty: bool,
}

impl Bench3dUi {
    pub fn new() -> Self {
        Self {
            cfg: Bench3dConfig::default(),
            progress: crate::progress::new(),
            partial: crate::progress::new_results(),
            last: None,
            done: Default::default(),
            preview: None,
            preview_dirty: false,
        }
    }

    /// A finished run from elsewhere (Run all)
    pub fn set_result(&mut self, s: Option<Bench3dSummary>) {
        self.preview_dirty = s.as_ref().is_some_and(|s| s.preview.is_some());
        self.last = s;
    }
}

const FRAME_COLORS: [Color32; 4] = [
    Color32::from_rgb(86, 180, 233),
    Color32::from_rgb(230, 159, 0),
    Color32::from_rgb(0, 158, 115),
    Color32::from_rgb(204, 121, 167),
];

impl LatencyTesterApp {
    pub(super) fn start_bench3d(&mut self) {
        if self.is_running() {
            return;
        }
        if self.bench3d.cfg.resolutions.is_empty() {
            self.log("3D benchmark: select at least one resolution");
            return;
        }
        let cfg = self.bench3d.cfg.clone();
        let cancel = self.cancel.clone();
        let sampler = self.begin_sampler();
        *self.bench3d.progress.lock().unwrap() = RunProgress::default();
        self.bench3d.partial.lock().unwrap().clear();
        let (prog, partial, done, running) = (self.bench3d.progress.clone(), self.bench3d.partial.clone(), self.bench3d.done.clone(), self.running.clone());
        self.mark_running("3d benchmark", "Running the 3D graphics benchmark...");
        self.log(&format!("Started the 3D graphics benchmark: {} resolution(s), {}", cfg.resolutions.len(), cfg.detail.label()));
        std::thread::spawn(move || {
            let r = Bench3d::new(cfg).with_cancel(cancel).with_progress(prog, partial).with_sensors(sampler).run();
            *done.lock().unwrap() = Some(r);
            if let Ok(mut g) = running.lock() {
                *g = None;
            }
        });
    }

    /// Pick up a finished 3D run (called every frame)
    pub(super) fn poll_bench3d(&mut self) {
        let Some(r) = self.bench3d.done.lock().unwrap().take() else { return };
        self.task_status = "Idle".to_string();
        match r {
            Ok(s) => {
                let lines: Vec<String> = s.results.iter().map(|r| format!("{} {:.0} FPS (1 % low {:.0})", r.name, r.fps_avg, r.fps_1pct_low)).collect();
                self.log(&format!("3D benchmark complete on {} ({}): {}", s.adapter, s.backend, lines.join(" · ")));
                self.bench3d.set_result(Some(s));
            }
            Err(e) if crate::cancel::is_cancel_error(&e) => self.log("3D benchmark cancelled"),
            Err(e) => self.log(&format!("3D benchmark error: {}", e)),
        }
    }

    pub(super) fn bench3d_panel(&mut self, ui: &mut Ui) {
        ui.add_space(8.0);
        ui.heading("3D graphics benchmark");
        ui.label(
            RichText::new(
                "A lit scene of spinning, textured cubes rendered with wgpu (Vulkan first) off screen at each resolution, so \
                 the window, the display and VSync do not limit it. Frame times come from rendering back to back with two \
                 frames in flight; latency is one frame at a time, from submitting it to the GPU having finished it.",
            )
            .weak()
            .small(),
        );
        let running = self.is_running();
        ui.add_enabled_ui(!running, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label("Resolutions:");
                for (w, h, name) in RESOLUTIONS {
                    let mut on = self.bench3d.cfg.resolutions.contains(&(w, h));
                    if ui.checkbox(&mut on, format!("{} ({}×{})", name, w, h)).changed() {
                        if on {
                            self.bench3d.cfg.resolutions.push((w, h));
                            self.bench3d.cfg.resolutions.sort_by_key(|r| r.0 * r.1);
                        } else {
                            self.bench3d.cfg.resolutions.retain(|r| *r != (w, h));
                        }
                    }
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.label("Detail:");
                egui::ComboBox::from_id_salt("bench3d_detail").selected_text(self.bench3d.cfg.detail.label()).show_ui(ui, |ui| {
                    for d in Detail::ALL {
                        ui.selectable_value(&mut self.bench3d.cfg.detail, d, d.label());
                    }
                });
                let mut msaa = self.bench3d.cfg.msaa >= 4;
                if ui.checkbox(&mut msaa, "4× MSAA").changed() {
                    self.bench3d.cfg.msaa = if msaa { 4 } else { 1 };
                }
                ui.label("Each resolution:");
                ui.add(egui::DragValue::new(&mut self.bench3d.cfg.duration_s).range(1.0..=300.0).suffix(" s"));
                ui.label("+ latency");
                ui.add(egui::DragValue::new(&mut self.bench3d.cfg.latency_s).range(0.5..=60.0).suffix(" s"));
                ui.label("+ warm-up");
                ui.add(egui::DragValue::new(&mut self.bench3d.cfg.warmup_s).range(0.0..=60.0).suffix(" s"));
            });
        });
        ui.horizontal(|ui| {
            if ui.add_enabled(!running, egui::Button::new(RichText::new("▶ Run 3D benchmark").strong())).clicked() {
                self.start_bench3d();
            }
            self.stop_button(ui);
        });
        let prog = self.bench3d.progress.lock().unwrap().clone();
        super::suites_ui::progress_panel(ui, &prog, running);

        let partial = self.bench3d.partial.lock().unwrap().clone();
        let results: Vec<Bench3dResult> = if running || self.bench3d.last.is_none() { partial } else { self.bench3d.last.as_ref().map(|s| s.results.clone()).unwrap_or_default() };
        if results.is_empty() {
            return;
        }
        if let Some(s) = &self.bench3d.last {
            ui.label(RichText::new(format!("{} · {} · {}", s.adapter, s.backend, s.driver)).weak().small());
        }
        egui::Grid::new("bench3d_results").striped(true).spacing([16.0, 3.0]).show(ui, |ui| {
            for h in ["Resolution", "Avg FPS", "1 % low", "0.1 % low", "Frame avg", "p99", "Worst", "Latency avg", "p99", "GPU time", "GPU °C", "GPU W"] {
                ui.label(RichText::new(h).weak());
            }
            ui.end_row();
            for r in &results {
                ui.label(format!("{} ({}×{})", r.name, r.width, r.height));
                ui.label(RichText::new(format!("{:.0}", r.fps_avg)).strong());
                ui.label(format!("{:.0}", r.fps_1pct_low));
                ui.label(format!("{:.0}", r.fps_01pct_low));
                ui.label(format!("{:.2} ms", r.frametime_avg_ms));
                ui.label(format!("{:.2} ms", r.frametime_p99_ms));
                ui.label(format!("{:.2} ms", r.frametime_max_ms));
                ui.label(format!("{:.2} ms", r.latency_avg_ms));
                ui.label(format!("{:.2} ms", r.latency_p99_ms));
                ui.label(r.gpu_time_avg_ms.map(|g| format!("{:.2} ms", g)).unwrap_or_else(|| "—".into()));
                ui.label(r.telemetry.gpu_temp_max_c.map(|t| format!("{:.0}", t)).unwrap_or_else(|| "—".into()));
                ui.label(r.telemetry.gpu_power_max_w.map(|w| format!("{:.0}", w)).unwrap_or_else(|| "—".into()));
                ui.end_row();
            }
        });
        ui.label(RichText::new("Frame times (ms): spikes are stutters").strong());
        Plot::new("bench3d_frames").height(200.0).x_axis_label("frame").y_axis_label("ms").allow_scroll(false).include_y(0.0).show(ui, |p| {
            for (i, r) in results.iter().enumerate() {
                let pts: Vec<[f64; 2]> = r.frametimes_ms.iter().enumerate().map(|(k, t)| [k as f64, *t]).collect();
                p.line(Line::new(PlotPoints::from(pts)).name(&r.name).color(FRAME_COLORS[i % FRAME_COLORS.len()]));
            }
        });
        ui.horizontal_wrapped(|ui| {
            for (i, r) in results.iter().enumerate() {
                ui.label(RichText::new(format!("■ {}", r.name)).color(FRAME_COLORS[i % FRAME_COLORS.len()]).small());
            }
        });
        // what was rendered
        if self.bench3d.preview_dirty {
            self.bench3d.preview_dirty = false;
            if let Some((w, h, px)) = self.bench3d.last.as_ref().and_then(|s| s.preview.clone()) {
                let img = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &px);
                self.bench3d.preview = Some(ui.ctx().load_texture("bench3d_preview", img, Default::default()));
            }
        }
        if let Some(t) = &self.bench3d.preview {
            ui.label(RichText::new("The rendered scene (last frame of the first resolution)").weak().small());
            ui.image((t.id(), t.size_vec2()));
        }
    }
}

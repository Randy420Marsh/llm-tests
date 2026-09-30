//! Input tab, "Reflex game": click the red circles as fast as possible (see `aim_game`).

use eframe::egui;
use egui::{Color32, RichText, Ui};
use egui_plot::{Line, Plot, PlotPoints, Points};
use std::time::{Duration, Instant};

use super::LatencyTesterApp;
use crate::aim_game::{AimConfig, AimGame, AimMode, AimResult};
use crate::timer::HighResTimer;

const COUNTDOWN: Duration = Duration::from_secs(3);

pub(super) struct AimUi {
    pub cfg: AimConfig,
    /// Targets on screen in the "several at a time" mode
    pub multi: u32,
    pub sound: bool,
    game: Option<AimGame>,
    countdown_from: Option<Instant>,
    clock: HighResTimer,
    pub results: Vec<AimResult>,
    last_hit_ms: Option<f64>,
}

impl AimUi {
    pub fn new() -> Self {
        Self {
            cfg: AimConfig::default(),
            multi: 4,
            sound: true,
            game: None,
            countdown_from: None,
            clock: HighResTimer::new(),
            results: Vec::new(),
            last_hit_ms: None,
        }
    }

    pub fn is_active(&self) -> bool {
        self.game.is_some() || self.countdown_from.is_some()
    }

    fn now_ms(&self) -> f64 {
        self.clock.now_ns() as f64 / 1e6
    }

    fn stop(&mut self) {
        self.game = None;
        self.countdown_from = None;
    }
}

impl LatencyTesterApp {
    pub(super) fn aim_panel(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        ui.label(
            "Click the red circles as fast as you can. Each hit brings the next one somewhere else; clicks beside a circle \
             count as misses. The times include everything from seeing the circle to the click reaching the app.",
        );
        let active = self.aim.is_active();
        ui.add_enabled_ui(!active, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label("Circles:");
                ui.add(egui::DragValue::new(&mut self.aim.cfg.targets).range(5..=1000));
                ui.label("Size:");
                ui.add(egui::Slider::new(&mut self.aim.cfg.radius, 8.0..=90.0).suffix(" px radius"));
                ui.separator();
                let single = matches!(self.aim.cfg.mode, AimMode::Single);
                if ui.radio(single, "one at a time").clicked() {
                    self.aim.cfg.mode = AimMode::Single;
                }
                if ui.radio(!single, "several at a time:").clicked() {
                    self.aim.cfg.mode = AimMode::Multi(self.aim.multi);
                }
                if ui.add(egui::DragValue::new(&mut self.aim.multi).range(2..=8)).changed() && !single {
                    self.aim.cfg.mode = AimMode::Multi(self.aim.multi);
                }
                ui.separator();
                ui.checkbox(&mut self.aim.sound, "🔊 Sound on hit");
            });
        });
        ui.horizontal(|ui| {
            if !active {
                if ui.button("▶ Start").clicked() {
                    self.aim.countdown_from = Some(Instant::now());
                    self.aim.last_hit_ms = None;
                }
                if !self.aim.results.is_empty() && ui.small_button("Clear results").clicked() {
                    self.aim.results.clear();
                }
            } else if ui.button(RichText::new("⏹ Stop (Esc)").color(Color32::from_rgb(255, 120, 120))).clicked()
                || ctx.input(|i| i.key_pressed(egui::Key::Escape))
            {
                self.aim.stop();
            }
        });

        // play area
        let height = (ctx.screen_rect().height() * 0.62).clamp(320.0, 900.0);
        // only what is visible: a long row elsewhere on the tab can make the layout wider than the window,
        // and circles must never spawn off-screen
        let width = (ui.clip_rect().right() - ui.cursor().min.x - 12.0).min(ui.available_width()).max(200.0);
        let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 6.0, Color32::from_rgb(22, 24, 30));
        if self.aim.is_active() {
            ctx.request_repaint();
        }
        if let Some(t0) = self.aim.countdown_from {
            let left = COUNTDOWN.saturating_sub(t0.elapsed());
            if left.is_zero() {
                self.aim.countdown_from = None;
                let now = self.aim.now_ms();
                let seed = now.to_bits() ^ 0x5DEECE66D;
                self.aim.game = Some(AimGame::new(self.aim.cfg.clone(), rect.width(), rect.height(), now, seed));
            } else {
                painter.text(rect.center(), egui::Align2::CENTER_CENTER, format!("{}", left.as_secs() + 1), egui::FontId::proportional(96.0), Color32::from_gray(200));
            }
        }
        let mut finished = None;
        if let Some(g) = self.aim.game.as_mut() {
            g.resize(rect.width(), rect.height());
            // every press this frame, in order (a fast double click lands two presses in one frame)
            let presses: Vec<egui::Pos2> = ctx.input(|i| {
                i.events
                    .iter()
                    .filter_map(|e| match e {
                        egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, .. } if rect.contains(*pos) => Some(*pos),
                        _ => None,
                    })
                    .collect()
            });
            for p in presses {
                let now = self.aim.clock.now_ns() as f64 / 1e6;
                if g.click(p.x - rect.min.x, p.y - rect.min.y, now) {
                    self.aim.last_hit_ms = g.hits.last().map(|h| h.time_ms);
                    if self.aim.sound {
                        crate::sound::play_hit();
                    }
                }
            }
            let r = g.cfg.radius;
            for t in &g.targets {
                let c = rect.min + egui::vec2(t.x, t.y);
                painter.circle_filled(c, r, Color32::from_rgb(220, 50, 50));
                painter.circle_stroke(c, r, egui::Stroke::new(2.0, Color32::from_rgb(255, 140, 140)));
                painter.circle_filled(c, (r * 0.18).max(2.0), Color32::from_rgb(255, 210, 210));
            }
            let now = self.aim.clock.now_ns() as f64 / 1e6;
            let res = g.result(now);
            let hud = format!(
                "{} hit · {} to go · {} missed · {:.0} % accuracy{}",
                res.hits,
                g.remaining(),
                res.misses,
                res.accuracy_pct,
                self.aim.last_hit_ms.map(|t| format!(" · last {:.0} ms", t)).unwrap_or_default()
            );
            painter.text(rect.left_top() + egui::vec2(10.0, 8.0), egui::Align2::LEFT_TOP, hud, egui::FontId::proportional(15.0), Color32::from_gray(190));
            if g.is_over() {
                finished = Some(res);
            }
        } else if self.aim.countdown_from.is_none() {
            painter.text(rect.center(), egui::Align2::CENTER_CENTER, "Press Start", egui::FontId::proportional(28.0), Color32::from_gray(120));
        }
        if let Some(res) = finished {
            self.aim.game = None;
            self.log(&format!(
                "Reflex game ({}): {} circles · avg {:.0} ms · median {:.0} ms · best {:.0} ms · {:.0} % accuracy · {:.2} bits/s",
                res.mode, res.hits, res.avg_ms, res.median_ms, res.best_ms, res.accuracy_pct, res.throughput_bits_s
            ));
            self.aim.results.push(res);
        }

        let Some(r) = self.aim.results.last().cloned() else { return };
        ui.add_space(6.0);
        egui::Grid::new("aim_stats").striped(true).spacing([18.0, 3.0]).show(ui, |ui| {
            let mut row = |k: &str, v: String| {
                ui.label(RichText::new(k).weak());
                ui.label(v);
                ui.end_row();
            };
            row("Time per circle", format!("average {:.0} ms · median {:.0} ms · 90 % under {:.0} ms", r.avg_ms, r.median_ms, r.p90_ms));
            row("Best / worst", format!("{:.0} ms / {:.0} ms", r.best_ms, r.worst_ms));
            row("Accuracy", format!("{:.1} % ({} hits, {} misses) · hits land {:.0} % of a radius from the centre on average", r.accuracy_pct, r.hits, r.misses, r.avg_off_centre * 100.0));
            row("Pace", format!("{:.2} circles/s over {:.1} s · Fitts throughput {:.2} bits/s", r.targets_per_s, r.total_s, r.throughput_bits_s));
            row("Setup", format!("{} circles, {:.0} px radius, {}", r.targets, r.radius, r.mode));
        });
        ui.label(RichText::new("Time per circle (ms)").strong());
        let pts: Vec<[f64; 2]> = r.times_ms.iter().enumerate().map(|(i, t)| [(i + 1) as f64, *t]).collect();
        Plot::new("aim_times").height(200.0).x_axis_label("circle").y_axis_label("ms").allow_scroll(false).include_y(0.0).show(ui, |p| {
            p.line(Line::new(PlotPoints::from(pts.clone())).color(Color32::from_rgb(220, 80, 80)).name("time"));
            p.points(Points::new(PlotPoints::from(pts)).radius(2.5).color(Color32::from_rgb(255, 140, 140)));
        });
        if self.aim.results.len() > 1 {
            ui.label(RichText::new("All games").strong());
            egui::Grid::new("aim_runs").striped(true).spacing([14.0, 2.0]).show(ui, |ui| {
                for h in ["#", "Mode", "Circles", "Avg ms", "Median ms", "Best ms", "Accuracy %", "bits/s"] {
                    ui.label(RichText::new(h).weak());
                }
                ui.end_row();
                for (i, r) in self.aim.results.iter().enumerate() {
                    ui.label(format!("{}", i + 1));
                    ui.label(&r.mode);
                    ui.label(format!("{}", r.hits));
                    ui.label(format!("{:.0}", r.avg_ms));
                    ui.label(format!("{:.0}", r.median_ms));
                    ui.label(format!("{:.0}", r.best_ms));
                    ui.label(format!("{:.1}", r.accuracy_pct));
                    ui.label(format!("{:.2}", r.throughput_bits_s));
                    ui.end_row();
                }
            });
        }
    }
}

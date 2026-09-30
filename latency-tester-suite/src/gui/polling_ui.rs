//! Input tab, "Mouse polling": move the mouse, every report is timestamped (see `mouse_poll`).

use eframe::egui;
use egui::{Color32, RichText, Ui};
use egui_plot::{Line, Plot, PlotPoints, Points};
use std::time::Instant;

use super::LatencyTesterApp;
use crate::mouse_poll::{self, Capture, MousePollResult};

pub(super) struct PollingUi {
    /// Seconds of movement to record (counted from the first report)
    pub duration_s: f32,
    capture: Option<Capture>,
    first_report: Option<Instant>,
    started: Option<Instant>,
    pub results: Vec<MousePollResult>,
    pub note: String,
}

impl PollingUi {
    pub fn new() -> Self {
        Self { duration_s: 5.0, capture: None, first_report: None, started: None, results: Vec::new(), note: String::new() }
    }

    pub fn is_active(&self) -> bool {
        self.capture.is_some()
    }

    fn start(&mut self) {
        match mouse_poll::start() {
            Ok(c) => {
                self.note = format!("reading {}", c.source);
                self.capture = Some(c);
                self.first_report = None;
                self.started = Some(Instant::now());
            }
            Err(e) => self.note = format!("could not start: {}", e),
        }
    }

    /// Stop the capture and analyse it; returns a log line
    fn finish(&mut self) -> String {
        let Some(c) = self.capture.take() else { return String::new() };
        let source = c.source.clone();
        let reports = c.stop();
        match mouse_poll::analyze(&reports, &source) {
            Some(r) => {
                let line = format!(
                    "Mouse polling: {:.0} Hz (set {:.0} Hz) · interval {:.1} µs median, jitter {:.1} µs, {:.1} % on time · {} reports",
                    r.rate_hz, r.nominal_hz, r.interval_median_us, r.jitter_us, r.on_time_pct, r.reports
                );
                self.note = line.clone();
                self.results.push(r);
                line
            }
            None => {
                self.note = format!("only {} reports: move the mouse continuously (fast circles) while it records", reports.len());
                self.note.clone()
            }
        }
    }

    /// Advance the capture; Some(log line) when it just finished
    fn tick(&mut self) -> Option<String> {
        let c = self.capture.as_ref()?;
        if let Some(e) = c.error() {
            self.capture = None;
            self.note = format!("capture failed: {}", e);
            return Some(self.note.clone());
        }
        if self.first_report.is_none() && c.count() > 0 {
            self.first_report = Some(Instant::now());
        }
        let done = self.first_report.is_some_and(|t| t.elapsed().as_secs_f32() >= self.duration_s);
        // nobody moved the mouse for 30 s: give up
        let idle = self.first_report.is_none() && self.started.is_some_and(|t| t.elapsed().as_secs() >= 30);
        if idle {
            self.capture = None;
            self.note = "no mouse movement seen for 30 s, stopped".into();
            return Some(self.note.clone());
        }
        done.then(|| self.finish())
    }
}

impl LatencyTesterApp {
    pub(super) fn polling_panel(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        if let Some(line) = self.polling.tick() {
            self.log(&line);
        }
        ui.label(
            "Move the mouse continuously (fast circles work best) while it records. Every report the mouse sends is \
             timestamped on arrival, like MouseTester: the real polling rate, how evenly the reports come (jitter) and \
             how many counts each carries.",
        );
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!self.polling.is_active(), |ui| {
                ui.label("Record");
                ui.add(egui::DragValue::new(&mut self.polling.duration_s).range(1.0..=60.0).speed(0.5).suffix(" s"));
                ui.label("of movement");
            });
            if !self.polling.is_active() {
                if ui.button("▶ Start").clicked() {
                    self.polling.start();
                }
            } else if ui.button(RichText::new("⏹ Stop").color(Color32::from_rgb(255, 120, 120))).clicked() {
                let line = self.polling.finish();
                self.log(&line);
            }
            if !self.polling.results.is_empty() && !self.polling.is_active() && ui.small_button("Clear results").clicked() {
                self.polling.results.clear();
            }
        });
        if let Some(c) = &self.polling.capture {
            ctx.request_repaint();
            let n = c.count();
            let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width().min(700.0), 160.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, 6.0, Color32::from_rgb(30, 36, 48));
            let text = match self.polling.first_report {
                None => "Move the mouse now…".to_string(),
                Some(t) => format!("Recording… {:.1} / {:.0} s · {} reports", t.elapsed().as_secs_f32(), self.polling.duration_s, n),
            };
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(20.0), Color32::WHITE);
        }
        if !self.polling.note.is_empty() {
            ui.label(RichText::new(&self.polling.note).weak().small());
        }
        let Some(r) = self.polling.results.last().cloned() else { return };
        ui.add_space(6.0);
        egui::Grid::new("poll_stats").striped(true).spacing([18.0, 3.0]).show(ui, |ui| {
            let mut row = |k: &str, v: String| {
                ui.label(RichText::new(k).weak());
                ui.label(v);
                ui.end_row();
            };
            row("Polling rate", format!("{:.0} Hz (median interval) · set to {:.0} Hz", r.rate_hz, r.nominal_hz));
            row("Interval", format!("median {:.1} µs · average {:.1} µs · 1 % {:.1} µs · 99 % {:.1} µs", r.interval_median_us, r.interval_avg_us, r.interval_p1_us, r.interval_p99_us));
            row("Jitter (std dev)", format!("{:.1} µs · min {:.1} µs · max {:.1} µs", r.jitter_us, r.interval_min_us, r.interval_max_us));
            row("On time (±10 %)", format!("{:.1} % of reports · {:.2} % came a whole interval late or merged", r.on_time_pct, r.skipped_pct));
            row("Counts per report", format!("average {:.1} · max {:.0}", r.counts_avg, r.counts_max));
            row("Recorded", format!("{} reports over {:.1} s of movement · {}", r.reports, r.moving_s, r.source));
        });
        let iv: Vec<[f64; 2]> = r.intervals.iter().map(|p| [p.0, p.1]).collect();
        let nominal = 1e6 / r.nominal_hz.max(1.0);
        ui.label(RichText::new("Interval vs time (µs): a steady mouse is a flat band at the set rate").strong());
        Plot::new("poll_intervals").height(220.0).x_axis_label("s").y_axis_label("µs").allow_scroll(false).include_y(0.0).show(ui, |p| {
            p.line(Line::new(PlotPoints::from(vec![[iv.first().map_or(0.0, |q| q[0]), nominal], [iv.last().map_or(1.0, |q| q[0]), nominal]])).color(Color32::from_gray(110)).name("set rate"));
            p.points(Points::new(PlotPoints::from(iv)).radius(1.5_f32).color(Color32::from_rgb(86, 180, 233)).name("interval"));
        });
        let xc: Vec<[f64; 2]> = r.x_counts.iter().map(|p| [p.0, p.1 as f64]).collect();
        ui.label(RichText::new("X counts vs time: smooth curves mean smooth tracking").strong());
        Plot::new("poll_xcounts").height(180.0).x_axis_label("s").y_axis_label("counts").allow_scroll(false).show(ui, |p| {
            p.points(Points::new(PlotPoints::from(xc)).radius(1.5_f32).color(Color32::from_rgb(230, 159, 0)).name("x counts"));
        });
        if self.polling.results.len() > 1 {
            ui.label(RichText::new("All runs").strong());
            egui::Grid::new("poll_runs").striped(true).spacing([14.0, 2.0]).show(ui, |ui| {
                for h in ["#", "Rate Hz", "Set Hz", "Median µs", "Jitter µs", "On time %", "Reports"] {
                    ui.label(RichText::new(h).weak());
                }
                ui.end_row();
                for (i, r) in self.polling.results.iter().enumerate() {
                    ui.label(format!("{}", i + 1));
                    ui.label(format!("{:.0}", r.rate_hz));
                    ui.label(format!("{:.0}", r.nominal_hz));
                    ui.label(format!("{:.1}", r.interval_median_us));
                    ui.label(format!("{:.1}", r.jitter_us));
                    ui.label(format!("{:.1}", r.on_time_pct));
                    ui.label(format!("{}", r.reports));
                    ui.end_row();
                }
            });
        }
    }
}

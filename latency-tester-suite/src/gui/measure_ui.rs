//! Markers and measured ranges on the app's charts (the web report has the full set of tools)

use eframe::egui;
use egui::{Color32, RichText, Ui};
use egui_plot::{PlotPoint, PlotUi, Text, VLine};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum MeasMode {
    Marker,
    Range,
}

#[derive(Default)]
pub(super) struct Measure {
    pub mode: Option<MeasMode>,
    /// (name, x)
    pub markers: Vec<(String, f64)>,
    /// (name, from, to)
    pub ranges: Vec<(String, f64, f64)>,
    /// First click of a range
    pending: Option<f64>,
}

const COLORS: [Color32; 6] = [
    Color32::from_rgb(255, 93, 143),
    Color32::from_rgb(255, 209, 102),
    Color32::from_rgb(6, 214, 160),
    Color32::from_rgb(76, 201, 240),
    Color32::from_rgb(247, 127, 0),
    Color32::from_rgb(199, 125, 255),
];

/// Value of a line at x: the point there, or linear between its neighbours (None outside it)
pub(super) fn value_at(pts: &[[f64; 2]], x: f64) -> Option<f64> {
    let (first, last) = (pts.first()?, pts.last()?);
    if x < first[0] - 1e-9 || x > last[0] + 1e-9 {
        return None;
    }
    let i = pts.partition_point(|p| p[0] < x - 1e-9);
    let b = pts.get(i)?;
    if (b[0] - x).abs() < 1e-9 || i == 0 {
        return Some(b[1]);
    }
    let a = pts[i - 1];
    Some(if b[0] == a[0] { a[1] } else { a[1] + (b[1] - a[1]) * (x - a[0]) / (b[0] - a[0]) })
}

/// (points, min, average, max, std dev, last - first) of the values in [x0, x1]
pub(super) fn range_stats(pts: &[[f64; 2]], x0: f64, x1: f64) -> Option<(usize, f64, f64, f64, f64, f64)> {
    let v: Vec<f64> = pts.iter().filter(|p| p[0] >= x0 - 1e-9 && p[0] <= x1 + 1e-9).map(|p| p[1]).collect();
    if v.is_empty() {
        return None;
    }
    let n = v.len() as f64;
    let mean = v.iter().sum::<f64>() / n;
    let sd = (v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n).sqrt();
    let (lo, hi) = v.iter().fold((f64::MAX, f64::MIN), |(a, b), &x| (a.min(x), b.max(x)));
    Some((v.len(), lo, mean, hi, sd, v[v.len() - 1] - v[0]))
}

impl Measure {
    /// Buttons above a chart
    pub fn tools(&mut self, ui: &mut Ui, range_hint: &str) {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Measure:").weak());
            for (m, label, tip) in [
                (MeasMode::Marker, "📍 marker", "Click the chart to drop a marker: every line's value there, and the change between markers"),
                (MeasMode::Range, "↔ range", range_hint),
            ] {
                if ui.selectable_label(self.mode == Some(m), label).on_hover_text(tip).clicked() {
                    self.mode = if self.mode == Some(m) { None } else { Some(m) };
                    self.pending = None;
                }
            }
            if (!self.markers.is_empty() || !self.ranges.is_empty()) && ui.small_button("clear").clicked() {
                self.markers.clear();
                self.ranges.clear();
                self.pending = None;
            }
            if let Some(p) = self.pending {
                ui.label(RichText::new(format!("range starts at {:.2}: click where it ends", p)).color(Color32::from_rgb(255, 209, 102)).small());
            }
        });
    }

    /// Draw markers and ranges inside the plot. `top` = y of the labels: a fixed value from the data
    /// (the plot's own bounds would grow to include the labels, every frame)
    pub fn draw(&self, plot_ui: &mut PlotUi, top: f64) {
        for (i, (name, x)) in self.markers.iter().enumerate() {
            let c = COLORS[i % COLORS.len()];
            plot_ui.vline(VLine::new(*x).color(c).width(1.5).style(egui_plot::LineStyle::dashed_dense()));
            plot_ui.text(Text::new(PlotPoint::new(*x, top), RichText::new(name).color(c).strong()));
        }
        for (i, (name, a, b)) in self.ranges.iter().enumerate() {
            let c = COLORS[(i + 3) % COLORS.len()];
            plot_ui.vline(VLine::new(*a).color(c).width(1.0));
            plot_ui.vline(VLine::new(*b).color(c).width(1.0));
            plot_ui.text(Text::new(PlotPoint::new((a + b) / 2.0, top), RichText::new(name).color(c)).anchor(egui::Align2::CENTER_TOP));
        }
        if let Some(p) = self.pending {
            plot_ui.vline(VLine::new(p).color(Color32::from_rgb(255, 209, 102)).width(1.0).style(egui_plot::LineStyle::dotted_dense()));
        }
    }

    /// A click on the chart at data x; `band` = the test under the pointer (start, end, name)
    pub fn click(&mut self, x: f64, shift: bool, band: Option<(f64, f64, String)>) {
        match self.mode {
            Some(MeasMode::Marker) => {
                let n = self.markers.len();
                self.markers.push((((b'A' + (n % 26) as u8) as char).to_string(), x));
            }
            Some(MeasMode::Range) => {
                if let (true, Some((a, b, name))) = (shift, band) {
                    self.ranges.push((name, a, b));
                    self.pending = None;
                } else if let Some(p) = self.pending.take() {
                    let n = self.ranges.len();
                    self.ranges.push((format!("R{}", n + 1), p.min(x), p.max(x)));
                } else {
                    self.pending = Some(x);
                }
            }
            None => {}
        }
    }

    /// Tables under the chart: `lines` = (name, points sorted by x), `unit` shown with the values
    pub fn panel(&mut self, ui: &mut Ui, id: &str, lines: &[(String, Vec<[f64; 2]>)], unit: &str, x_fmt: &dyn Fn(f64) -> String) {
        let v = |x: f64| super::results_ui::format_value(x, unit);
        if !self.markers.is_empty() {
            let mut markers: Vec<(String, f64)> = self.markers.clone();
            markers.sort_by(|a, b| a.1.total_cmp(&b.1));
            ui.label(RichText::new("Markers").strong());
            egui::Grid::new((id, "markers")).striped(true).spacing([14.0, 2.0]).show(ui, |ui| {
                ui.label(RichText::new("Line").weak());
                for (name, x) in &markers {
                    ui.label(RichText::new(format!("{} @ {}", name, x_fmt(*x))).weak());
                }
                if markers.len() >= 2 {
                    ui.label(RichText::new(format!("{} − {}", markers[markers.len() - 1].0, markers[0].0)).weak());
                    ui.label(RichText::new("change").weak());
                }
                ui.end_row();
                for (name, pts) in lines.iter().take(40) {
                    ui.label(name);
                    let vals: Vec<Option<f64>> = markers.iter().map(|m| value_at(pts, m.1)).collect();
                    for x in &vals {
                        ui.label(x.map(v).unwrap_or_else(|| "—".into()));
                    }
                    if let (true, Some(Some(a)), Some(Some(b))) = (markers.len() >= 2, vals.first(), vals.last()) {
                        ui.label(format!("{}{}", if b >= a { "+" } else { "" }, v(b - a)));
                        ui.label(if *a != 0.0 { format!("{:+.1} %", (b - a) / a.abs() * 100.0) } else { "—".into() });
                    }
                    ui.end_row();
                }
            });
        }
        let mut remove = None;
        for (ri, (name, a, b)) in self.ranges.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("Range {}: {} – {}", name, x_fmt(*a), x_fmt(*b))).strong());
                if ui.small_button("×").clicked() {
                    remove = Some(ri);
                }
            });
            egui::Grid::new((id, "range", ri)).striped(true).spacing([14.0, 2.0]).show(ui, |ui| {
                for h in ["Line", "points", "min", "average", "max", "std dev", "last - first"] {
                    ui.label(RichText::new(h).weak());
                }
                ui.end_row();
                for (lname, pts) in lines.iter().take(40) {
                    ui.label(lname);
                    match range_stats(pts, *a, *b) {
                        Some((n, lo, mean, hi, sd, d)) => {
                            ui.label(n.to_string());
                            for x in [lo, mean, hi, sd] {
                                ui.label(v(x));
                            }
                            ui.label(format!("{}{}", if d >= 0.0 { "+" } else { "" }, v(d)));
                        }
                        None => {
                            ui.label(RichText::new("no points").weak());
                        }
                    }
                    ui.end_row();
                }
            });
        }
        if let Some(i) = remove {
            self.ranges.remove(i);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_ranges_and_clicks() {
        let pts = [[0.0, 10.0], [1.0, 20.0], [3.0, 40.0]];
        assert_eq!(value_at(&pts, 1.0), Some(20.0));
        assert_eq!(value_at(&pts, 2.0), Some(30.0), "between points: linear");
        assert_eq!(value_at(&pts, 5.0), None);
        let (n, lo, mean, hi, _sd, d) = range_stats(&pts, 0.5, 3.0).unwrap();
        assert_eq!((n, lo, mean, hi, d), (2, 20.0, 30.0, 40.0, 20.0));
        let mut m = Measure { mode: Some(MeasMode::Range), ..Default::default() };
        m.click(5.0, false, None);
        m.click(2.0, false, None);
        assert_eq!(m.ranges, vec![("R1".to_string(), 2.0, 5.0)]);
        m.click(9.0, true, Some((8.0, 12.0, "CPU · IntegerAdd".into())));
        assert_eq!(m.ranges[1], ("CPU · IntegerAdd".to_string(), 8.0, 12.0), "shift + click takes the test under the pointer");
        m.mode = Some(MeasMode::Marker);
        m.click(1.0, false, None);
        m.click(3.0, false, None);
        assert_eq!(m.markers, vec![("A".to_string(), 1.0), ("B".to_string(), 3.0)]);
    }
}

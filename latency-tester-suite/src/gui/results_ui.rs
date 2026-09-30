//! "Results & Graphs" tab: every measured value (including per-test temperatures, clocks, RAM,
//! GPU and VRAM readings) as charts and a table, with switches to hide/show any series or column.

use eframe::egui;
use egui::{Color32, RichText, Ui};
use egui_plot::{Bar, BarChart, Legend, Line, Plot, PlotPoints, Points};
use std::collections::{BTreeMap, HashSet};

use super::{human_size, LatencyTesterApp};
use crate::cpu_benchmark::CpuBenchmarkResult;
use crate::gpu_benchmark::GpuBenchmarkResult;
use crate::input_latency::InputLatencyResult;
use crate::memory_benchmark::MemoryBenchmarkResult;
use crate::sensors::{Snapshot, Telemetry};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) enum Dataset {
    Memory,
    Cpu,
    Gpu,
    Input,
    Sensors,
}

impl Dataset {
    const ALL: [Dataset; 5] = [Dataset::Memory, Dataset::Cpu, Dataset::Gpu, Dataset::Input, Dataset::Sensors];

    fn name(self) -> &'static str {
        match self {
            Dataset::Memory => "Memory",
            Dataset::Cpu => "CPU",
            Dataset::Gpu => "GPU",
            Dataset::Input => "Input",
            Dataset::Sensors => "Sensors over time",
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Dataset::Memory => "mem",
            Dataset::Cpu => "cpu",
            Dataset::Gpu => "gpu",
            Dataset::Input => "inp",
            Dataset::Sensors => "sen",
        }
    }
}

/// One measured point: which line it belongs to, where on the x axis, and every value
#[derive(Debug, Clone)]
pub(super) struct Row {
    pub series: String,
    pub x: f64,
    pub x_label: String,
    pub values: BTreeMap<&'static str, f64>,
}

/// (key, label, unit) for every metric a dataset can hold
pub(super) type Metric = (&'static str, &'static str, &'static str);

const TELEMETRY_METRICS: [Metric; 12] = [
    ("cpu_temp_max", "CPU temp (max)", "°C"),
    ("cpu_temp_avg", "CPU temp (avg)", "°C"),
    ("hottest_core", "Hottest core", "°C"),
    ("cpu_freq", "CPU clock (avg)", "MHz"),
    ("cpu_usage", "CPU load (avg)", "%"),
    ("ram_used", "RAM used (max)", "MB"),
    ("gpu_temp", "GPU temp (max)", "°C"),
    ("gpu_util", "GPU load (avg)", "%"),
    ("gpu_clock", "GPU clock (avg)", "MHz"),
    ("gpu_power", "GPU power (max)", "W"),
    ("vram_used", "VRAM used (max)", "MB"),
    ("vram_total", "VRAM total", "MB"),
];

const MEMORY_METRICS: [Metric; 6] = [
    ("ns_per_access", "Latency / access", "ns"),
    ("bandwidth", "Bandwidth", "GB/s"),
    ("avg_run", "Avg run", "ms"),
    ("p99_run", "p99 run", "ms"),
    ("min_run", "Best run", "ms"),
    ("runs", "Runs", ""),
];
const CPU_METRICS: [Metric; 5] = [
    ("mcalls", "Throughput", "M calls/s"),
    ("ns_per_call", "Time per call", "ns"),
    ("clock", "Clock (during run)", "MHz"),
    ("this_core_temp", "Temp of this core", "°C"),
    ("threads", "Threads", ""),
];
const GPU_METRICS: [Metric; 5] = [
    ("avg_ms", "Avg dispatch", "ms"),
    ("p95_ms", "p95 dispatch", "ms"),
    ("p99_ms", "p99 dispatch", "ms"),
    ("gops", "Throughput", "GOPS"),
    ("min_ms", "Best dispatch", "ms"),
];
const INPUT_METRICS: [Metric; 6] = [
    ("avg_ms", "Average", "ms"),
    ("p99_ms", "p99", "ms"),
    ("max_ms", "Worst", "ms"),
    ("jitter_ms", "Jitter", "ms"),
    ("polling_hz", "Loop rate", "Hz"),
    ("samples", "Samples", ""),
];

pub(super) fn metrics_for(d: Dataset) -> Vec<Metric> {
    let own: &[Metric] = match d {
        Dataset::Memory => &MEMORY_METRICS,
        Dataset::Cpu => &CPU_METRICS,
        Dataset::Gpu => &GPU_METRICS,
        Dataset::Input => &INPUT_METRICS,
        Dataset::Sensors => &[],
    };
    let mut v = own.to_vec();
    if d != Dataset::Sensors {
        v.extend_from_slice(&TELEMETRY_METRICS);
    }
    v
}

fn put(values: &mut BTreeMap<&'static str, f64>, key: &'static str, v: Option<f32>) {
    if let Some(v) = v {
        values.insert(key, v as f64);
    }
}

fn telemetry_values(t: &Telemetry, values: &mut BTreeMap<&'static str, f64>) {
    if t.samples == 0 {
        return;
    }
    put(values, "cpu_temp_max", t.cpu_temp_max_c);
    put(values, "cpu_temp_avg", t.cpu_temp_avg_c);
    put(values, "hottest_core", t.core_temp_max_c.iter().map(|c| c.1).fold(None, |m: Option<f32>, x| Some(m.map_or(x, |m| m.max(x)))));
    put(values, "cpu_freq", t.cpu_freq_avg_mhz);
    put(values, "cpu_usage", t.cpu_usage_avg_pct);
    put(values, "ram_used", t.ram_used_max_mb);
    put(values, "gpu_temp", t.gpu_temp_max_c);
    put(values, "gpu_util", t.gpu_util_avg_pct);
    put(values, "gpu_clock", t.gpu_clock_avg_mhz);
    put(values, "gpu_power", t.gpu_power_max_w);
    put(values, "vram_used", t.vram_used_max_mb);
    put(values, "vram_total", t.vram_total_mb);
}

pub(super) fn memory_rows(results: &[MemoryBenchmarkResult]) -> Vec<Row> {
    results
        .iter()
        .map(|r| {
            let mut v = BTreeMap::new();
            v.insert("ns_per_access", r.ns_per_access);
            v.insert("bandwidth", r.bandwidth_gb_s);
            v.insert("avg_run", r.latency_ns / 1e6);
            v.insert("p99_run", r.percentile_99_ns / 1e6);
            v.insert("min_run", r.min_latency_ns / 1e6);
            v.insert("runs", r.iterations as f64);
            telemetry_values(&r.telemetry, &mut v);
            let cores = if r.cores.is_empty() || r.cores.starts_with("All cores") { String::new() } else { format!(" · {}", r.cores) };
            Row {
                series: format!("{} · {}T{}", r.pattern.label(), r.thread_count, cores),
                x: (r.size as f64).log2(),
                x_label: human_size(r.size),
                values: v,
            }
        })
        .collect()
}

pub(super) fn cpu_rows(results: &[CpuBenchmarkResult]) -> Vec<Row> {
    results
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let mut v = BTreeMap::new();
            v.insert("mcalls", r.operations_per_second / 1e6);
            v.insert("ns_per_call", r.latency_ns);
            v.insert("threads", r.thread_count as f64);
            if r.frequency_mhz > 0 {
                v.insert("clock", r.frequency_mhz as f64);
            }
            telemetry_values(&r.telemetry, &mut v);
            let single = (r.core_mask.count_ones() == 1).then(|| r.core_mask.trailing_zeros() as usize);
            if let Some(core) = single {
                if let Some(t) = r.telemetry.core_temp_max_c.iter().find(|c| c.0 == core) {
                    v.insert("this_core_temp", t.1 as f64);
                }
            }
            Row {
                series: format!("{:?}", r.workload),
                x: single.map(|c| c as f64).unwrap_or(i as f64),
                x_label: r.cores.clone(),
                values: v,
            }
        })
        .collect()
}

pub(super) fn gpu_rows(results: &[GpuBenchmarkResult]) -> Vec<Row> {
    results
        .iter()
        .map(|r| {
            let mut v = BTreeMap::new();
            v.insert("avg_ms", r.avg_latency_ms);
            v.insert("p95_ms", r.percentile_95_ms);
            v.insert("p99_ms", r.percentile_99_ms);
            v.insert("min_ms", r.min_latency_ms);
            v.insert("gops", r.throughput_geops);
            telemetry_values(&r.telemetry, &mut v);
            Row {
                series: "GPU compute".to_string(),
                x: (r.workload_size as f64).log2(),
                x_label: format!("{} elements", r.workload_size),
                values: v,
            }
        })
        .collect()
}

pub(super) fn input_rows(results: &[InputLatencyResult]) -> Vec<Row> {
    results
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let mut v = BTreeMap::new();
            v.insert("avg_ms", r.avg_latency_ms);
            v.insert("p99_ms", r.percentile_99_ms);
            v.insert("max_ms", r.max_latency_ms);
            v.insert("jitter_ms", r.jitter_ms);
            v.insert("samples", r.sample_count as f64);
            if let Some(hz) = r.polling_rate_hz {
                v.insert("polling_hz", hz);
            }
            telemetry_values(&r.telemetry, &mut v);
            let x = r.core.map(|c| c as f64).unwrap_or(i as f64);
            Row {
                series: format!("{:?}", r.mode),
                x,
                x_label: r.core.map(|c| format!("core {}", c)).unwrap_or_else(|| "OS scheduled".into()),
                values: v,
            }
        })
        .collect()
}

/// Cores whose throughput is well below the median of the single-core CPU results
pub(super) fn slow_cores(results: &[CpuBenchmarkResult], threshold: f64) -> Vec<(String, usize, f64)> {
    let mut by_workload: BTreeMap<String, Vec<(usize, f64)>> = BTreeMap::new();
    for r in results {
        if r.core_mask.count_ones() == 1 {
            by_workload
                .entry(format!("{:?}", r.workload))
                .or_default()
                .push((r.core_mask.trailing_zeros() as usize, r.operations_per_second));
        }
    }
    let mut out = Vec::new();
    for (w, mut cores) in by_workload {
        if cores.len() < 3 {
            continue;
        }
        let mut sorted: Vec<f64> = cores.iter().map(|c| c.1).collect();
        sorted.sort_by(|a, b| a.total_cmp(b));
        let median = sorted[sorted.len() / 2];
        cores.sort_by_key(|c| c.0);
        for (core, ops) in cores {
            if median > 0.0 && ops < median * (1.0 - threshold) {
                out.push((w.clone(), core, (1.0 - ops / median) * 100.0));
            }
        }
    }
    out
}

const PALETTE: [Color32; 12] = [
    Color32::from_rgb(86, 180, 233),
    Color32::from_rgb(230, 159, 0),
    Color32::from_rgb(0, 158, 115),
    Color32::from_rgb(240, 228, 66),
    Color32::from_rgb(204, 121, 167),
    Color32::from_rgb(213, 94, 0),
    Color32::from_rgb(150, 150, 255),
    Color32::from_rgb(120, 220, 120),
    Color32::from_rgb(255, 130, 130),
    Color32::from_rgb(180, 130, 255),
    Color32::from_rgb(90, 220, 220),
    Color32::from_rgb(200, 200, 200),
];

fn color_for(i: usize) -> Color32 {
    PALETTE[i % PALETTE.len()]
}

/// State of the tab: what to show and what is switched off
pub(super) struct ResultsUi {
    pub dataset: Dataset,
    /// Chart metric per dataset (key from `metrics_for`)
    pub chart_metric: BTreeMap<&'static str, &'static str>,
    /// "tag:series" entries hidden from chart and table
    pub hidden_series: HashSet<String>,
    /// "tag:metric" entries hidden from the table (and unselectable in the chart)
    pub hidden_cols: HashSet<String>,
    pub show_chart: bool,
    pub show_table: bool,
    pub show_points: bool,
    /// Start the Y axis at zero (otherwise it zooms to the data)
    pub zero_y: bool,
    /// Logarithmic Y axis (latency spans orders of magnitude across cache levels)
    pub log_y: bool,
    /// Sensors chart: which group of lines to draw
    pub sensor_group: SensorGroup,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SensorGroup {
    Temperatures,
    Clocks,
    Load,
    Memory,
}

impl ResultsUi {
    pub fn new() -> Self {
        Self {
            dataset: Dataset::Memory,
            chart_metric: BTreeMap::new(),
            hidden_series: HashSet::new(),
            hidden_cols: HashSet::new(),
            show_chart: true,
            show_table: true,
            show_points: true,
            zero_y: true,
            log_y: false,
            sensor_group: SensorGroup::Temperatures,
        }
    }

    fn series_hidden(&self, d: Dataset, s: &str) -> bool {
        self.hidden_series.contains(&format!("{}:{}", d.tag(), s))
    }

    fn col_hidden(&self, d: Dataset, m: &str) -> bool {
        self.hidden_cols.contains(&format!("{}:{}", d.tag(), m))
    }

    fn toggle_series(&mut self, d: Dataset, s: &str, visible: bool) {
        let k = format!("{}:{}", d.tag(), s);
        if visible { self.hidden_series.remove(&k); } else { self.hidden_series.insert(k); }
    }

    fn toggle_col(&mut self, d: Dataset, m: &str, visible: bool) {
        let k = format!("{}:{}", d.tag(), m);
        if visible { self.hidden_cols.remove(&k); } else { self.hidden_cols.insert(k); }
    }
}

fn format_value(v: f64, unit: &str) -> String {
    let s = if v.fract() == 0.0 && v.abs() < 1e9 { format!("{:.0}", v) } else if v.abs() >= 1000.0 { format!("{:.0}", v) } else if v.abs() >= 10.0 { format!("{:.1}", v) } else { format!("{:.2}", v) };
    if unit.is_empty() { s } else { format!("{} {}", s, unit) }
}

/// CSV of the rows/columns currently visible
pub(super) fn to_csv(rows: &[&Row], cols: &[Metric]) -> String {
    let mut out = String::from("series,x");
    for c in cols {
        out.push_str(&format!(",{} ({})", c.1, c.2));
    }
    out.push('\n');
    for r in rows {
        out.push_str(&format!("\"{}\",\"{}\"", r.series, r.x_label));
        for c in cols {
            out.push(',');
            if let Some(v) = r.values.get(c.0) {
                out.push_str(&format!("{}", v));
            }
        }
        out.push('\n');
    }
    out
}

impl LatencyTesterApp {
    fn dataset_rows(&self, d: Dataset) -> Vec<Row> {
        match d {
            Dataset::Memory => memory_rows(&self.mem_progress.lock().unwrap().completed),
            Dataset::Cpu => cpu_rows(&self.cpu_partial.lock().unwrap()),
            Dataset::Gpu => gpu_rows(&self.gpu_partial.lock().unwrap()),
            Dataset::Input => self.last_input_result.as_ref().map(|s| input_rows(&s.results)).unwrap_or_default(),
            Dataset::Sensors => Vec::new(),
        }
    }

    pub(super) fn render_graphs_tab(&mut self, ui: &mut Ui) {
        ui.heading("Results & Graphs");
        ui.horizontal_wrapped(|ui| {
            for d in Dataset::ALL {
                let n = if d == Dataset::Sensors { self.last_timeline.len() } else { self.dataset_rows(d).len() };
                let label = format!("{} ({})", d.name(), n);
                ui.selectable_value(&mut self.results_ui.dataset, d, label);
            }
        });
        ui.separator();

        let d = self.results_ui.dataset;
        if d == Dataset::Sensors {
            self.sensors_view(ui);
            return;
        }
        let rows = self.dataset_rows(d);
        if rows.is_empty() {
            ui.label("No results yet — run a test on its tab; results appear here while it runs.");
            return;
        }

        let mut series: Vec<String> = rows.iter().map(|r| r.series.clone()).collect();
        series.sort();
        series.dedup();
        let all_metrics = metrics_for(d);
        // only metrics that at least one row actually has a value for
        let metrics: Vec<Metric> = all_metrics.iter().copied().filter(|m| rows.iter().any(|r| r.values.contains_key(m.0))).collect();

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            // ---------------- show / hide panel ----------------
            egui::CollapsingHeader::new("Show / hide lines").default_open(true).show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui.small_button("All").clicked() {
                        for s in &series { self.results_ui.toggle_series(d, s, true); }
                    }
                    if ui.small_button("None").clicked() {
                        for s in &series { self.results_ui.toggle_series(d, s, false); }
                    }
                    if ui.small_button("Invert").clicked() {
                        for s in &series {
                            let vis = self.results_ui.series_hidden(d, s);
                            self.results_ui.toggle_series(d, s, vis);
                        }
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    for (i, s) in series.iter().enumerate() {
                        let mut vis = !self.results_ui.series_hidden(d, s);
                        if ui.checkbox(&mut vis, RichText::new(s).color(color_for(i))).changed() {
                            self.results_ui.toggle_series(d, s, vis);
                        }
                    }
                });
            });
            egui::CollapsingHeader::new("Show / hide values (columns)").default_open(false).show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui.small_button("All").clicked() {
                        for m in &metrics { self.results_ui.toggle_col(d, m.0, true); }
                    }
                    if ui.small_button("None").clicked() {
                        for m in &metrics { self.results_ui.toggle_col(d, m.0, false); }
                    }
                    if ui.small_button("Performance only").clicked() {
                        for m in &metrics {
                            let is_telemetry = TELEMETRY_METRICS.iter().any(|t| t.0 == m.0);
                            self.results_ui.toggle_col(d, m.0, !is_telemetry);
                        }
                    }
                    if ui.small_button("Sensors only").clicked() {
                        for m in &metrics {
                            let is_telemetry = TELEMETRY_METRICS.iter().any(|t| t.0 == m.0);
                            self.results_ui.toggle_col(d, m.0, is_telemetry);
                        }
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    for m in &metrics {
                        let mut vis = !self.results_ui.col_hidden(d, m.0);
                        if ui.checkbox(&mut vis, format!("{} ({})", m.1, m.2)).changed() {
                            self.results_ui.toggle_col(d, m.0, vis);
                        }
                    }
                });
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.results_ui.show_chart, "Chart");
                ui.checkbox(&mut self.results_ui.show_table, "Table");
                ui.checkbox(&mut self.results_ui.show_points, "Markers");
                ui.checkbox(&mut self.results_ui.zero_y, "Start Y at 0");
                ui.checkbox(&mut self.results_ui.log_y, "Log Y");
            });

            if d == Dataset::Cpu {
                let results = self.cpu_partial.lock().unwrap().clone();
                for (w, core, pct) in slow_cores(&results, 0.08) {
                    ui.colored_label(Color32::from_rgb(255, 170, 60), format!("⚠ {}: core {} is {:.0}% slower than the median core", w, core, pct));
                }
            }

            // ---------------- chart ----------------
            let selectable: Vec<Metric> = metrics.iter().copied().filter(|m| !self.results_ui.col_hidden(d, m.0)).collect();
            if self.results_ui.show_chart {
                let key = *self.results_ui.chart_metric.entry(d.tag()).or_insert(selectable.first().map(|m| m.0).unwrap_or(""));
                let mut current = selectable.iter().copied().find(|m| m.0 == key).or(selectable.first().copied());
                if let Some(cur) = current.as_mut() {
                    ui.horizontal(|ui| {
                        ui.label("Chart:");
                        egui::ComboBox::from_id_salt(("metric", d.tag()))
                            .selected_text(format!("{} ({})", cur.1, cur.2))
                            .show_ui(ui, |ui| {
                                for m in &selectable {
                                    if ui.selectable_label(m.0 == cur.0, format!("{} ({})", m.1, m.2)).clicked() {
                                        self.results_ui.chart_metric.insert(d.tag(), m.0);
                                        *cur = *m;
                                    }
                                }
                            });
                    });
                    self.draw_chart(ui, d, &rows, &series, *cur);
                } else {
                    ui.label("All values are hidden — enable some under \"Show / hide values\".");
                }
            }

            // ---------------- table ----------------
            if self.results_ui.show_table {
                let vis_rows: Vec<&Row> = rows.iter().filter(|r| !self.results_ui.series_hidden(d, &r.series)).collect();
                let cols: Vec<Metric> = selectable.clone();
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{} rows", vis_rows.len())).weak());
                    if ui.small_button("Copy table as CSV").clicked() {
                        ui.ctx().copy_text(to_csv(&vis_rows, &cols));
                    }
                });
                egui::ScrollArea::horizontal().id_salt("results_table_scroll").show(ui, |ui| {
                    egui::Grid::new(("results_table", d.tag())).striped(true).spacing([16.0, 3.0]).show(ui, |ui| {
                        ui.label(RichText::new("Line").weak());
                        ui.label(RichText::new("At").weak());
                        for c in &cols {
                            ui.label(RichText::new(format!("{} ({})", c.1, c.2)).weak());
                        }
                        ui.end_row();
                        for r in vis_rows.iter().rev() {
                            let idx = series.iter().position(|s| *s == r.series).unwrap_or(0);
                            ui.label(RichText::new(&r.series).color(color_for(idx)));
                            ui.label(&r.x_label);
                            for c in &cols {
                                match r.values.get(c.0) {
                                    Some(v) => ui.label(format_value(*v, "")),
                                    None => ui.label(RichText::new("—").weak()),
                                };
                            }
                            ui.end_row();
                        }
                    });
                });
            }
        });
    }

    fn draw_chart(&self, ui: &mut Ui, d: Dataset, rows: &[Row], series: &[String], metric: Metric) {
        let log_x = matches!(d, Dataset::Memory | Dataset::Gpu);
        let show_points = self.results_ui.show_points;
        let log_y = self.results_ui.log_y;
        let zero_y = self.results_ui.zero_y && !log_y;
        let ylabel = format!("{} ({}){}", metric.1, metric.2, if log_y { " — log scale" } else { "" });
        let labels: BTreeMap<i64, String> = rows.iter().map(|r| (r.x.round() as i64, r.x_label.clone())).collect();
        let mut plot = Plot::new(("plot", d.tag()))
            .height(320.0)
            .legend(Legend::default())
            .y_axis_label(ylabel)
            .allow_scroll(false);
        if log_y {
            plot = plot.y_axis_formatter(|mark, _| {
                let v = 10f64.powf(mark.value);
                if v >= 100.0 { format!("{:.0}", v) } else if v >= 1.0 { format!("{:.1}", v) } else { format!("{:.2}", v) }
            });
        }
        plot = if log_x {
            plot.x_axis_formatter(move |mark, _| {
                let s = 2f64.powf(mark.value);
                if mark.value.fract().abs() < 1e-6 { human_size(s as usize) } else { String::new() }
            })
        } else {
            plot.x_axis_formatter(move |mark, _| {
                labels.get(&(mark.value.round() as i64)).cloned().unwrap_or_else(|| format!("{:.0}", mark.value))
            })
        };
        if zero_y {
            plot = plot.include_y(0.0);
        }
        plot.show(ui, |plot_ui| {
            for (i, s) in series.iter().enumerate() {
                if self.results_ui.series_hidden(d, s) {
                    continue;
                }
                let mut pts: Vec<[f64; 2]> = rows
                    .iter()
                    .filter(|r| &r.series == s)
                    .filter_map(|r| r.values.get(metric.0).filter(|v| !log_y || **v > 0.0).map(|v| [r.x, if log_y { v.log10() } else { *v }]))
                    .collect();
                if pts.is_empty() {
                    continue;
                }
                pts.sort_by(|a, b| a[0].total_cmp(&b[0]));
                let color = color_for(i);
                plot_ui.line(Line::new(PlotPoints::from(pts.clone())).name(s).color(color));
                if show_points {
                    plot_ui.points(Points::new(PlotPoints::from(pts)).name(s).color(color).radius(3.5));
                }
            }
        });
    }

    // ------------------------------------------------------------------ sensors over time

    fn sensors_view(&mut self, ui: &mut Ui) {
        if self.last_timeline.is_empty() {
            ui.label("No sensor data yet — run any test; sensors are recorded while it runs.");
            return;
        }
        for n in &self.sensor_notes {
            ui.label(RichText::new(format!("• {}", n)).weak().small());
        }
        ui.horizontal_wrapped(|ui| {
            for (g, name) in [
                (SensorGroup::Temperatures, "Temperatures"),
                (SensorGroup::Clocks, "Clocks"),
                (SensorGroup::Load, "Load"),
                (SensorGroup::Memory, "RAM / VRAM"),
            ] {
                ui.selectable_value(&mut self.results_ui.sensor_group, g, name);
            }
        });
        let lines = sensor_lines(&self.last_timeline, self.results_ui.sensor_group);
        if lines.is_empty() {
            ui.label("This sensor group has no data on this machine.");
            return;
        }
        egui::CollapsingHeader::new("Show / hide lines").default_open(true).show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.small_button("All").clicked() {
                    for (name, _) in &lines { self.results_ui.toggle_series(Dataset::Sensors, name, true); }
                }
                if ui.small_button("None").clicked() {
                    for (name, _) in &lines { self.results_ui.toggle_series(Dataset::Sensors, name, false); }
                }
            });
            ui.horizontal_wrapped(|ui| {
                for (i, (name, _)) in lines.iter().enumerate() {
                    let mut vis = !self.results_ui.series_hidden(Dataset::Sensors, name);
                    if ui.checkbox(&mut vis, RichText::new(name).color(color_for(i))).changed() {
                        self.results_ui.toggle_series(Dataset::Sensors, name, vis);
                    }
                }
            });
        });
        Plot::new("sensor_plot")
            .height(320.0)
            .legend(Legend::default())
            .x_axis_label("seconds since the test started")
            .show(ui, |plot_ui| {
                for (i, (name, pts)) in lines.iter().enumerate() {
                    if self.results_ui.series_hidden(Dataset::Sensors, name) {
                        continue;
                    }
                    plot_ui.line(Line::new(PlotPoints::from(pts.clone())).name(name).color(color_for(i)));
                }
            });

        // Peak temperature per core over the whole run
        let peaks = core_peaks(&self.last_timeline);
        if !peaks.is_empty() {
            ui.add_space(8.0);
            ui.label(RichText::new("Peak temperature per core").strong());
            let bars: Vec<Bar> = peaks.iter().map(|(c, t)| Bar::new(*c as f64, *t as f64).name(format!("core {}", c)).width(0.7)).collect();
            Plot::new("core_peaks").height(200.0).x_axis_label("core").y_axis_label("°C").allow_scroll(false).show(ui, |plot_ui| {
                plot_ui.bar_chart(BarChart::new(bars).color(Color32::from_rgb(230, 130, 60)));
            });
        }
    }
}

/// Lines (name, points[t seconds, value]) for one group of sensor readings
pub(super) fn sensor_lines(tl: &[Snapshot], group: SensorGroup) -> Vec<(String, Vec<[f64; 2]>)> {
    let t = |s: &Snapshot| s.t_ms as f64 / 1000.0;
    let mut lines: BTreeMap<String, Vec<[f64; 2]>> = BTreeMap::new();
    let mut add = |name: &str, x: f64, y: Option<f32>| {
        if let Some(y) = y {
            lines.entry(name.to_string()).or_default().push([x, y as f64]);
        }
    };
    for s in tl {
        let x = t(s);
        match group {
            SensorGroup::Temperatures => {
                add("CPU package °C", x, s.cpu_package_c);
                add("Hottest core °C", x, s.core_temps_c.iter().map(|c| c.1).fold(None, |m: Option<f32>, v| Some(m.map_or(v, |m| m.max(v)))));
                for (c, temp) in &s.core_temps_c {
                    add(&format!("Core {} °C", c), x, Some(*temp));
                }
                add("GPU °C", x, s.gpu.as_ref().and_then(|g| g.temp_c));
            }
            SensorGroup::Clocks => {
                add("CPU avg MHz", x, avg_f32(&s.core_freq_mhz));
                add("CPU fastest MHz", x, s.core_freq_mhz.iter().copied().reduce(f32::max));
                add("CPU slowest MHz", x, s.core_freq_mhz.iter().copied().filter(|f| *f > 0.0).reduce(f32::min));
                add("GPU MHz", x, s.gpu.as_ref().and_then(|g| g.clock_mhz));
            }
            SensorGroup::Load => {
                add("CPU load %", x, avg_f32(&s.core_usage_pct));
                add("Busiest CPU %", x, s.core_usage_pct.iter().copied().reduce(f32::max));
                add("GPU load %", x, s.gpu.as_ref().and_then(|g| g.util_pct));
                add("GPU power W", x, s.gpu.as_ref().and_then(|g| g.power_w));
            }
            SensorGroup::Memory => {
                add("RAM used MB", x, Some(s.ram_used_mb));
                add("VRAM used MB", x, s.gpu.as_ref().and_then(|g| g.vram_used_mb));
            }
        }
    }
    lines.into_iter().collect()
}

fn avg_f32(v: &[f32]) -> Option<f32> {
    (!v.is_empty()).then(|| v.iter().sum::<f32>() / v.len() as f32)
}

/// Highest temperature seen on each core across the timeline
pub(super) fn core_peaks(tl: &[Snapshot]) -> Vec<(usize, f32)> {
    let mut m: BTreeMap<usize, f32> = BTreeMap::new();
    for s in tl {
        for &(c, t) in &s.core_temps_c {
            let e = m.entry(c).or_insert(t);
            *e = e.max(t);
        }
    }
    m.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu_benchmark::{AffinityMode, WorkloadType};
    use crate::memory_benchmark::AccessPattern;
    use crate::sensors::GpuSensors;

    fn cpu_result(core: usize, ops: f64) -> CpuBenchmarkResult {
        CpuBenchmarkResult {
            workload: WorkloadType::IntegerAdd,
            thread_count: 1,
            affinity_mode: AffinityMode::CustomMask(1 << core),
            core_mask: 1 << core,
            operations_per_second: ops,
            latency_ns: 1e9 / ops,
            instructions_per_cycle: None,
            cycles_per_operation: None,
            frequency_mhz: 4000,
            temperature_c: None,
            power_watts: None,
            iteration_results: vec![],
            cores: format!("Core {}", core),
            telemetry: Telemetry {
                samples: 2,
                core_temp_max_c: vec![(core, 60.0 + core as f32)],
                cpu_temp_max_c: Some(70.0),
                ..Default::default()
            },
        }
    }

    #[test]
    fn cpu_rows_carry_core_position_and_own_temperature() {
        let rows = cpu_rows(&[cpu_result(3, 2e6), cpu_result(5, 2e6)]);
        assert_eq!(rows[0].x, 3.0);
        assert_eq!(rows[0].values["this_core_temp"], 63.0);
        assert_eq!(rows[1].values["mcalls"], 2.0);
        assert_eq!(rows[1].x_label, "Core 5");
    }

    #[test]
    fn slow_core_is_flagged() {
        let mut v: Vec<_> = (0..6).map(|c| cpu_result(c, 1e6)).collect();
        v[4] = cpu_result(4, 0.7e6);
        let slow = slow_cores(&v, 0.08);
        assert_eq!(slow.len(), 1);
        assert_eq!(slow[0].1, 4);
        assert!((slow[0].2 - 30.0).abs() < 0.01);
        assert!(slow_cores(&v[..2], 0.08).is_empty()); // too few cores to judge
    }

    #[test]
    fn memory_rows_include_telemetry_and_log_size_axis() {
        let r = MemoryBenchmarkResult {
            size: 1 << 20,
            pattern: AccessPattern::PointerChase,
            thread_count: 2,
            latency_ns: 2e6,
            bandwidth_gb_s: 1.5,
            iterations: 5,
            min_latency_ns: 1e6,
            max_latency_ns: 3e6,
            std_dev_ns: 0.0,
            percentile_50_ns: 0.0,
            percentile_95_ns: 0.0,
            percentile_99_ns: 2.5e6,
            percentile_999_ns: 0.0,
            ns_per_access: 12.5,
            cores: "P-cores 0-7".into(),
            telemetry: Telemetry {
                samples: 1,
                gpu_temp_max_c: Some(41.0),
                vram_used_max_mb: Some(900.0),
                ..Default::default()
            },
        };
        let rows = memory_rows(&[r]);
        assert_eq!(rows[0].x, 20.0);
        assert_eq!(rows[0].values["ns_per_access"], 12.5);
        assert_eq!(rows[0].values["gpu_temp"], 41.0);
        assert_eq!(rows[0].values["vram_used"], 900.0);
        assert!(rows[0].series.contains("P-cores 0-7"));
        // no sensors -> no telemetry keys
        assert!(!rows[0].values.contains_key("cpu_temp_max"));
    }

    #[test]
    fn csv_has_header_and_blank_for_missing() {
        let rows = memory_rows(&[]);
        assert!(rows.is_empty());
        let mut v = BTreeMap::new();
        v.insert("bandwidth", 2.0);
        let row = Row { series: "a".into(), x: 1.0, x_label: "1 MB".into(), values: v };
        let csv = to_csv(&[&row], &[MEMORY_METRICS[0], MEMORY_METRICS[1]]);
        assert_eq!(csv.lines().next().unwrap(), "series,x,Latency / access (ns),Bandwidth (GB/s)");
        assert_eq!(csv.lines().nth(1).unwrap(), "\"a\",\"1 MB\",,2");
    }

    #[test]
    fn integers_print_without_decimals() {
        assert_eq!(format_value(3.0, ""), "3");
        assert_eq!(format_value(2800.0, "MHz"), "2800 MHz");
        assert_eq!(format_value(0.5613, "ms"), "0.56 ms");
        assert_eq!(format_value(202.04, ""), "202.0");
    }

    #[test]
    fn hide_and_show_series_and_columns() {
        let mut ui = ResultsUi::new();
        assert!(!ui.series_hidden(Dataset::Memory, "x"));
        ui.toggle_series(Dataset::Memory, "x", false);
        assert!(ui.series_hidden(Dataset::Memory, "x"));
        assert!(!ui.series_hidden(Dataset::Cpu, "x")); // per dataset
        ui.toggle_series(Dataset::Memory, "x", true);
        assert!(!ui.series_hidden(Dataset::Memory, "x"));
        ui.toggle_col(Dataset::Gpu, "gops", false);
        assert!(ui.col_hidden(Dataset::Gpu, "gops"));
    }

    #[test]
    fn sensor_lines_and_peaks() {
        let snap = |t, pkg, cores: &[(usize, f32)]| Snapshot {
            t_ms: t,
            cpu_package_c: pkg,
            core_temps_c: cores.to_vec(),
            core_freq_mhz: vec![3000.0, 5000.0],
            core_usage_pct: vec![10.0, 90.0],
            ram_used_mb: 100.0,
            ram_total_mb: 1000.0,
            gpu: Some(GpuSensors { temp_c: Some(40.0), vram_used_mb: Some(512.0), ..Default::default() }),
        };
        let tl = vec![snap(0, Some(50.0), &[(0, 50.0), (1, 60.0)]), snap(1000, Some(55.0), &[(0, 58.0), (1, 52.0)])];
        let temps = sensor_lines(&tl, SensorGroup::Temperatures);
        let names: Vec<_> = temps.iter().map(|l| l.0.as_str()).collect();
        assert!(names.contains(&"CPU package °C") && names.contains(&"Core 1 °C") && names.contains(&"GPU °C"));
        let clocks = sensor_lines(&tl, SensorGroup::Clocks);
        assert!(clocks.iter().any(|l| l.0 == "CPU slowest MHz" && l.1[0][1] == 3000.0));
        let mem = sensor_lines(&tl, SensorGroup::Memory);
        assert!(mem.iter().any(|l| l.0 == "VRAM used MB"));
        assert_eq!(core_peaks(&tl), vec![(0, 58.0), (1, 60.0)]);
        assert!(sensor_lines(&[], SensorGroup::Load).is_empty());
    }
}

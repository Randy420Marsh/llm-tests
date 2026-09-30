//! "Results & Graphs" tab: every measured value (including per-test temperatures, clocks, RAM,
//! GPU and VRAM readings) as charts and a table, with switches to hide/show any series or column.

use eframe::egui;
use egui::{Color32, RichText, Ui};
use egui_plot::{Bar, BarChart, Line, Plot, PlotPoints, Points, Polygon};
use std::collections::{BTreeMap, HashSet};

use super::input_ui::RunRecord;
use super::{human_size, LatencyTesterApp};
use crate::cpu_benchmark::CpuBenchmarkResult;
use crate::gpu_benchmark::GpuBenchmarkResult;
use crate::input_latency::InputLatencyResult;
use crate::memory_benchmark::MemoryBenchmarkResult;
use crate::sensors::{Phase, SensorKind, Snapshot, Telemetry};
use crate::topology::CoreKind;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) enum Dataset {
    Memory,
    Cpu,
    Gpu,
    Gpu3d,
    Input,
    Sensors,
}

impl Dataset {
    const ALL: [Dataset; 6] = [Dataset::Memory, Dataset::Cpu, Dataset::Gpu, Dataset::Gpu3d, Dataset::Input, Dataset::Sensors];

    fn name(self) -> &'static str {
        match self {
            Dataset::Memory => "Memory",
            Dataset::Cpu => "CPU",
            Dataset::Gpu => "GPU",
            Dataset::Gpu3d => "3D",
            Dataset::Input => "Input",
            Dataset::Sensors => "Sensors over time",
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Dataset::Memory => "mem",
            Dataset::Cpu => "cpu",
            Dataset::Gpu => "gpu",
            Dataset::Gpu3d => "g3d",
            Dataset::Input => "inp",
            Dataset::Sensors => "sen",
        }
    }
}

/// One measured point: which line it belongs to, where on the x axis, and every value
#[derive(Debug, Clone, Default)]
pub(super) struct Row {
    pub series: String,
    pub x: f64,
    pub x_label: String,
    pub values: BTreeMap<&'static str, f64>,
    /// Other programs that were busy during this test ("chrome.exe 12 % CPU")
    pub others: String,
}

/// (key, label, unit) for every metric a dataset can hold
pub(super) type Metric = (&'static str, &'static str, &'static str);

const TELEMETRY_METRICS: [Metric; 16] = [
    ("cpu_temp_max", "CPU temp (max)", "°C"),
    ("cpu_temp_avg", "CPU temp (avg)", "°C"),
    ("hottest_core", "Hottest core", "°C"),
    ("coolest_core", "Coolest core", "°C"),
    ("core_temp_avg", "Core temp (avg of all cores)", "°C"),
    ("cpu_freq", "CPU clock (avg)", "MHz"),
    ("cpu_usage", "CPU load (avg)", "%"),
    ("ram_used", "RAM used (max)", "MB"),
    ("gpu_temp", "GPU temp (max)", "°C"),
    ("gpu_util", "GPU load (avg)", "%"),
    ("gpu_clock", "GPU clock (avg)", "MHz"),
    ("gpu_power", "GPU power (max)", "W"),
    ("vram_used", "VRAM used (max)", "MB"),
    ("vram_total", "VRAM total", "MB"),
    ("others_cpu", "Other programs: CPU (avg)", "%"),
    ("others_gpu", "Other programs: GPU (avg)", "%"),
];

const MEMORY_METRICS: [Metric; 6] = [
    ("ns_per_access", "Latency / access", "ns"),
    ("bandwidth", "Bandwidth", "GB/s"),
    ("avg_run", "Avg run", "ms"),
    ("p99_run", "p99 run", "ms"),
    ("min_run", "Best run", "ms"),
    ("runs", "Runs", ""),
];
const CPU_METRICS: [Metric; 7] = [
    ("mcalls", "Throughput", "M calls/s"),
    ("mcalls_thread", "Throughput per thread", "M calls/s"),
    ("ns_per_call", "Time per call", "ns"),
    ("run_spread", "Run-to-run spread", "%"),
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
const GPU3D_METRICS: [Metric; 9] = [
    ("fps_avg", "Average FPS", "FPS"),
    ("fps_1pct", "1 % low", "FPS"),
    ("fps_01pct", "0.1 % low", "FPS"),
    ("ft_avg", "Frame time avg", "ms"),
    ("ft_p99", "Frame time p99", "ms"),
    ("ft_max", "Worst frame", "ms"),
    ("lat_avg", "Latency (submit to done)", "ms"),
    ("lat_p99", "Latency p99", "ms"),
    ("gpu_ms", "GPU time per frame", "ms"),
];
const INPUT_METRICS: [Metric; 8] = [
    ("trial_ms", "Trial latency", "ms"),
    ("trial_corrected_ms", "Trial latency (rig-corrected)", "ms"),
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
        Dataset::Gpu3d => &GPU3D_METRICS,
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
    put(values, "hottest_core", t.hottest_core().map(|c| c.1));
    put(values, "coolest_core", t.coolest_core().map(|c| c.1));
    put(values, "core_temp_avg", t.core_temp_avg_c);
    put(values, "cpu_freq", t.cpu_freq_avg_mhz);
    put(values, "cpu_usage", t.cpu_usage_avg_pct);
    put(values, "ram_used", t.ram_used_max_mb);
    put(values, "gpu_temp", t.gpu_temp_max_c);
    put(values, "gpu_util", t.gpu_util_avg_pct);
    put(values, "gpu_clock", t.gpu_clock_avg_mhz);
    put(values, "gpu_power", t.gpu_power_max_w);
    put(values, "vram_used", t.vram_used_max_mb);
    put(values, "vram_total", t.vram_total_mb);
    put(values, "others_cpu", t.others_cpu_avg_pct);
    put(values, "others_gpu", t.others_gpu_avg_pct);
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
                others: r.telemetry.others_text(),
            }
        })
        .collect()
}

/// One line per click / key-press run; x is the trial number
pub(super) fn manual_rows(runs: &[RunRecord]) -> Vec<Row> {
    let mut rows = Vec::new();
    for (ri, r) in runs.iter().enumerate() {
        let s = &r.summary;
        let series = format!("{} · {} #{}", s.kind.label(), if s.robot { "robot" } else { "human" }, ri + 1);
        for (i, ms) in s.samples_ms.iter().enumerate() {
            let mut v = BTreeMap::new();
            v.insert("trial_ms", *ms);
            if s.robot {
                v.insert("trial_corrected_ms", r.corrected(*ms).minus_robot_and_display_ms);
            }
            rows.push(Row { series: series.clone(), x: (i + 1) as f64, x_label: format!("trial {}", i + 1), values: v, ..Default::default() });
        }
    }
    rows
}

pub(super) fn cpu_rows(results: &[CpuBenchmarkResult], kinds: Option<&[CoreKind]>) -> Vec<Row> {
    let single_of = |r: &CpuBenchmarkResult| (r.core_mask.count_ones() == 1).then(|| r.core_mask.trailing_zeros() as usize);
    let any_single = results.iter().any(|r| single_of(r).is_some());
    let mut multi_keys: Vec<(usize, String)> = Vec::new();
    for r in results.iter().filter(|r| single_of(r).is_none()) {
        let k = (r.thread_count, r.cores.clone());
        if !multi_keys.contains(&k) {
            multi_keys.push(k);
        }
    }
    results
        .iter()
        .map(|r| {
            let threads = r.thread_count.max(1);
            let mut v = BTreeMap::new();
            v.insert("mcalls", r.operations_per_second / 1e6);
            v.insert("mcalls_thread", r.operations_per_second / 1e6 / threads as f64);
            v.insert("ns_per_call", r.latency_ns);
            v.insert("threads", r.thread_count as f64);
            if let Some(spread) = run_spread_pct(r) {
                v.insert("run_spread", spread);
            }
            if r.frequency_mhz > 0 {
                v.insert("clock", r.frequency_mhz as f64);
            }
            telemetry_values(&r.telemetry, &mut v);
            let all_cores = r.cores.is_empty() || r.cores.starts_with("All cores");
            let (series, x, x_label) = match single_of(r) {
                Some(core) => {
                    if let Some(t) = r.telemetry.core_temp_max_c.iter().find(|c| c.0 == core) {
                        v.insert("this_core_temp", t.1 as f64);
                    }
                    let class = crate::topology::class_label(kinds, core);
                    let label = if class.is_empty() { format!("Core {}", core) } else { format!("Core {} · {}", core, class) };
                    (format!("{:?}", r.workload), core as f64, label)
                }
                // a thread-count sweep: threads on the x axis
                None if !any_single => (
                    if all_cores { format!("{:?}", r.workload) } else { format!("{:?} · {}", r.workload, r.cores) },
                    threads as f64,
                    format!("{} thread{}", threads, if threads == 1 { "" } else { "s" }),
                ),
                // next to per-core runs: its own line and slot left of core 0, never on a core's position
                None => {
                    let k = multi_keys.iter().position(|m| m.0 == r.thread_count && m.1 == r.cores).unwrap_or(0);
                    let place = if all_cores { "all cores".to_string() } else { r.cores.clone() };
                    (format!("{:?} · {}T {}", r.workload, threads, place), -2.0 - k as f64, format!("{}T {}", threads, place))
                }
            };
            Row { series, x, x_label, values: v, others: r.telemetry.others_text() }
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
                others: r.telemetry.others_text(),
            }
        })
        .collect()
}

pub(super) fn gpu3d_rows(results: &[crate::bench3d::Bench3dResult]) -> Vec<Row> {
    results
        .iter()
        .map(|r| {
            let mut v = BTreeMap::new();
            v.insert("fps_avg", r.fps_avg);
            v.insert("fps_1pct", r.fps_1pct_low);
            v.insert("fps_01pct", r.fps_01pct_low);
            v.insert("ft_avg", r.frametime_avg_ms);
            v.insert("ft_p99", r.frametime_p99_ms);
            v.insert("ft_max", r.frametime_max_ms);
            v.insert("lat_avg", r.latency_avg_ms);
            v.insert("lat_p99", r.latency_p99_ms);
            if let Some(g) = r.gpu_time_avg_ms {
                v.insert("gpu_ms", g);
            }
            telemetry_values(&r.telemetry, &mut v);
            let detail = r.detail.split(" (").next().unwrap_or(&r.detail);
            Row { series: format!("{} · MSAA {}×", detail, r.msaa), x: r.height as f64, x_label: r.name.clone(), values: v, others: r.telemetry.others_text() }
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
                series: r.mode.label().to_string(),
                x,
                x_label: r.core.map(|c| format!("core {}", c)).unwrap_or_else(|| "OS scheduled".into()),
                values: v,
                others: r.telemetry.others_text(),
            }
        })
        .collect()
}

/// Standard deviation of the per-run throughput in % of its mean (None with fewer than two runs)
fn run_spread_pct(r: &CpuBenchmarkResult) -> Option<f64> {
    let runs: Vec<f64> = r.iteration_results.iter().filter(|i| i.duration_ns > 0).map(|i| i.operations as f64 / i.duration_ns as f64).collect();
    if runs.len() < 2 {
        return None;
    }
    let mean = runs.iter().sum::<f64>() / runs.len() as f64;
    let var = runs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / runs.len() as f64;
    (mean > 0.0).then(|| var.sqrt() / mean * 100.0)
}

/// A value that stands out, with the reason
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Anomaly {
    pub series: String,
    pub x: f64,
    pub x_label: String,
    pub why: String,
    pub strong: bool,
}

fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    let h = s.len() / 2;
    if s.is_empty() { 0.0 } else if s.len() % 2 == 1 { s[h] } else { (s[h - 1] + s[h]) / 2.0 }
}

/// Points that stand out from their neighbours on a line, cores that differ from cores of their own
/// kind, and results whose repeated runs disagree (same rules as the web report)
pub(super) fn find_anomalies(d: Dataset, rows: &[Row]) -> Vec<Anomaly> {
    let mut out = Vec::new();
    let mut add = |r: &Row, why: String, strong: bool| out.push(Anomaly { series: r.series.clone(), x: r.x, x_label: r.x_label.clone(), why, strong });
    let mut by_series: BTreeMap<&str, Vec<&Row>> = BTreeMap::new();
    for r in rows {
        by_series.entry(&r.series).or_default().push(r);
    }
    // spikes and dips along a line (GPU: throughput, its dispatch time grows with the size by design)
    let spike = match d {
        Dataset::Memory => Some(("ns_per_access", "time per access", " ns")),
        Dataset::Gpu => Some(("gops", "throughput", " GOPS")),
        Dataset::Input => Some(("avg_ms", "average", " ms")),
        _ => None,
    };
    if let Some((key, name, unit)) = spike {
        for rs in by_series.values() {
            let mut pts: Vec<(&Row, f64)> = rs.iter().filter_map(|r| r.values.get(key).filter(|v| **v > 0.0).map(|v| (*r, *v))).collect();
            if pts.len() < 4 {
                continue;
            }
            pts.sort_by(|a, b| a.0.x.total_cmp(&b.0.x));
            for i in 0..pts.len() {
                // the value the line would have here: between its neighbours on a log scale (geometric
                // mean), or extrapolated from the next two at either end; steady trends and cache steps
                // are then not flagged
                let v = pts[i].1;
                let val = |j: usize| pts[j].1;
                let near: Vec<f64> = [i.checked_sub(1), Some(i + 1)].into_iter().flatten().filter(|&j| j < pts.len()).map(val).collect();
                let around: Vec<f64> = [i.checked_sub(2), i.checked_sub(1), Some(i + 1), Some(i + 2)].into_iter().flatten().filter(|&j| j < pts.len()).map(val).collect();
                let edge = near.len() < 2;
                let exp = if !edge {
                    (near[0] * near[1]).sqrt()
                } else if i == 0 {
                    val(1).powi(2) / val(2)
                } else {
                    val(i - 1).powi(2) / val(i - 2)
                };
                let f = if edge { 2.5 } else { 1.6 };
                // a spike must also beat its direct neighbours; a dip must be below every point within two places
                let hi = near.iter().copied().fold(f64::MIN, f64::max);
                let lo = around.iter().copied().fold(f64::MAX, f64::min);
                let show = |x: f64| format!("{}{}", format_value(x, ""), unit);
                if v > exp * f && v > hi * 1.25 {
                    add(pts[i].0, format!("{} {:.1}× what the points around it suggest ({} vs {})", name, v / exp, show(v), show(exp)), v > exp * 3.0);
                } else if v < exp / (f + 0.5) && v < lo / 1.5 {
                    add(pts[i].0, format!("{} {:.1}× lower than the points around it suggest ({} vs {})", name, exp / v, show(v), show(exp)), v < exp / 3.0);
                }
            }
        }
    }
    if d == Dataset::Cpu {
        // single cores against the median of their own kind
        let mut peers: BTreeMap<(String, String), Vec<&Row>> = BTreeMap::new();
        for r in rows.iter().filter(|r| r.x >= 0.0 && r.x_label.starts_with("Core ")) {
            let class = r.x_label.rsplit(" · ").next().filter(|c| *c == "P" || *c == "E").unwrap_or("").to_string();
            peers.entry((r.series.clone(), class)).or_default().push(r);
        }
        for ((_, class), rs) in &peers {
            if rs.len() < 3 {
                continue;
            }
            let m = median(&rs.iter().filter_map(|r| r.values.get("mcalls").copied()).collect::<Vec<_>>());
            for r in rs {
                let Some(v) = r.values.get("mcalls") else { continue };
                let dev = if m > 0.0 { v / m - 1.0 } else { 0.0 };
                if dev.abs() > 0.08 {
                    let kind = if class.is_empty() { "core".to_string() } else { format!("{}-core", class) };
                    add(r, format!("{:.0}% {} than the median {}", dev.abs() * 100.0, if dev < 0.0 { "slower" } else { "faster" }, kind), dev.abs() > 0.25);
                }
            }
        }
    }
    for r in rows {
        let v = |k: &str| r.values.get(k).copied().unwrap_or(0.0);
        // another program was busy: the result may be lower than the machine can do
        let (oc, og) = (v("others_cpu"), v("others_gpu"));
        if oc >= 8.0 || og >= 8.0 {
            let what = if og > oc { format!("{:.0} % of the GPU", og) } else { format!("{:.0} % of the CPU", oc) };
            let who = if r.others.is_empty() { String::new() } else { format!(": {}", r.others) };
            add(r, format!("other programs used {} during this test{}", what, who), oc >= 25.0 || og >= 25.0);
        }
        match d {
            Dataset::Cpu if v("run_spread") > 5.0 => {
                add(r, format!("runs vary ±{:.1}% (something else used the core, or the clock changed)", v("run_spread")), v("run_spread") > 15.0)
            }
            Dataset::Memory if v("p99_run") > 0.0 && v("min_run") > 0.0 && v("p99_run") / v("min_run") > 2.0 => {
                add(r, format!("unstable: slowest run {:.1}× the best", v("p99_run") / v("min_run")), v("p99_run") / v("min_run") > 4.0)
            }
            Dataset::Gpu if v("p99_ms") > 0.0 && v("avg_ms") > 0.0 && v("p99_ms") / v("avg_ms") > 2.0 => {
                add(r, format!("p99 dispatch {:.1}× the average", v("p99_ms") / v("avg_ms")), v("p99_ms") / v("avg_ms") > 4.0)
            }
            Dataset::Gpu3d if v("ft_p99") > 0.0 && v("ft_avg") > 0.0 && v("ft_p99") / v("ft_avg") > 2.0 => {
                add(r, format!("stutter: p99 frame {:.1}× the average frame", v("ft_p99") / v("ft_avg")), v("ft_p99") / v("ft_avg") > 4.0)
            }
            Dataset::Input if v("p99_ms") > 0.0 && v("avg_ms") > 0.0 && v("p99_ms") / v("avg_ms") > 1.5 && !r.x_label.starts_with("trial") => {
                add(r, format!("p99 {:.1}× the average", v("p99_ms") / v("avg_ms")), v("p99_ms") / v("avg_ms") > 3.0)
            }
            _ => {}
        }
    }
    out
}

/// Compare names with their numbers as numbers: "2T" before "12T", "Core 9" before "Core 10"
pub(super) fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    fn chunks(s: &str) -> Vec<(bool, String)> {
        let mut out: Vec<(bool, String)> = Vec::new();
        for ch in s.chars() {
            let digit = ch.is_ascii_digit();
            match out.last_mut() {
                Some((d, run)) if *d == digit => run.push(ch),
                _ => out.push((digit, ch.to_string())),
            }
        }
        out
    }
    let (ca, cb) = (chunks(a), chunks(b));
    for (x, y) in ca.iter().zip(&cb) {
        let o = match (x.0, y.0) {
            (true, true) => {
                let (tx, ty) = (x.1.trim_start_matches('0'), y.1.trim_start_matches('0'));
                tx.len().cmp(&ty.len()).then_with(|| tx.cmp(ty))
            }
            _ => x.1.to_lowercase().cmp(&y.1.to_lowercase()),
        };
        if o != std::cmp::Ordering::Equal {
            return o;
        }
    }
    ca.len().cmp(&cb.len()).then_with(|| a.cmp(b))
}

/// "StreamAdd · 12T" -> ("StreamAdd", "12T"); "GameSim · 24T all cores" -> ("GameSim", "24T all cores");
/// lines without a thread count keep an empty column
pub(super) fn split_series(s: &str) -> (String, String) {
    let seg: Vec<&str> = s.split(" · ").collect();
    let is_threads = |x: &&str| {
        let digits = x.chars().take_while(|c| c.is_ascii_digit()).count();
        digits > 0 && x[digits..].starts_with('T') && x[digits + 1..].chars().next().map_or(true, |c| c == ' ')
    };
    match seg.iter().position(is_threads) {
        Some(k) => (seg.iter().enumerate().filter(|(i, _)| *i != k).map(|(_, x)| *x).collect::<Vec<_>>().join(" · "), seg[k].to_string()),
        None => (s.to_string(), String::new()),
    }
}

/// Legend under a chart, right-aligned and wrapped; folded away when there are many lines (the
/// coloured line switches above the chart name them too)
fn legend_below<'a>(ui: &mut Ui, id: impl std::hash::Hash, items: impl Iterator<Item = (Color32, &'a str)>) {
    let items: Vec<(Color32, &str)> = items.collect();
    if items.is_empty() {
        return;
    }
    let draw = |ui: &mut Ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP).with_main_wrap(true), |ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            // right-to-left: add in reverse so the first line ends up leftmost in reading order
            for (color, name) in items.iter().rev() {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    ui.label(RichText::new(*name).small());
                    ui.label(RichText::new("■").color(*color).small());
                });
            }
        });
    };
    if items.len() <= 16 {
        draw(ui);
    } else {
        egui::CollapsingHeader::new(RichText::new(format!("Legend ({} lines)", items.len())).small()).id_salt(id).default_open(false).show(ui, draw);
    }
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
    /// Opacity of the per-test background bands on the sensors chart (0 = off)
    pub phase_opacity: f32,
    /// Charts to reset to their full view on the next frame ("Reset view" button)
    pub reset_view: HashSet<&'static str>,
    /// Points ringed on the chart as unusual values (series, x)
    pub marks: Vec<(String, f64)>,
    /// Markers and measured ranges per chart ("sen" or a dataset tag)
    pub measures: std::collections::HashMap<&'static str, super::measure_ui::Measure>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SensorGroup {
    Temperatures,
    Clocks,
    Load,
    Power,
    Fans,
    Voltages,
    Memory,
}

/// Background colour of each kind of test on the sensors timeline
pub(super) fn phase_color(kind: &str) -> Color32 {
    match kind {
        "memory" => Color32::from_rgb(229, 72, 77),
        "cpu" => Color32::from_rgb(62, 139, 255),
        "gpu" => Color32::from_rgb(48, 192, 112),
        "input" => Color32::from_rgb(240, 180, 41),
        _ => Color32::from_rgb(154, 163, 178),
    }
}

/// Which test of its category a phase is: the CPU workload, the memory pattern, the input mode,
/// the GPU size ("CPU · IntegerAdd · 1 thread(s) · Core 3" -> "IntegerAdd")
pub(super) fn phase_test_key(p: &Phase) -> &str {
    let segs: Vec<&str> = p.label.split(" · ").collect();
    let i = if p.kind == "memory" { 2 } else { 1 };
    segs.get(i).or(segs.last()).copied().unwrap_or("")
}

/// Shade `i` of `n` of a category colour: hue steps across ±30°, alternate shades darker, so
/// neighbouring tests stand apart while the category stays recognisable
pub(super) fn phase_shade(kind: &str, i: usize, n: usize) -> Color32 {
    let base = phase_color(kind);
    if n <= 1 {
        return base;
    }
    let hsva = egui::ecolor::Hsva::from(base);
    let t = i as f32 / (n - 1) as f32;
    let h = (hsva.h + (t - 0.5) * 0.17).rem_euclid(1.0);
    let v = if i % 2 == 0 { hsva.v } else { hsva.v * 0.62 };
    Color32::from(egui::ecolor::Hsva::new(h, hsva.s, v, 1.0))
}

/// Tests of each category in order of first appearance
pub(super) fn phase_keys(phases: &[Phase]) -> BTreeMap<String, Vec<String>> {
    let mut m: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for p in phases {
        let keys = m.entry(p.kind.clone()).or_default();
        let k = phase_test_key(p);
        if !keys.iter().any(|x| x == k) {
            keys.push(k.to_string());
        }
    }
    m
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
            phase_opacity: 0.18,
            reset_view: HashSet::new(),
            marks: Vec::new(),
            measures: Default::default(),
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

pub(super) fn format_value(v: f64, unit: &str) -> String {
    let a = v.abs();
    let s = if (v.fract() == 0.0 && a < 1e9) || a >= 1000.0 {
        format!("{:.0}", v)
    } else if a >= 10.0 {
        format!("{:.1}", v)
    } else if a >= 0.1 {
        format!("{:.2}", v)
    } else {
        // small values keep three significant digits instead of turning into 0.00
        let digits = (2 - a.log10().floor() as i32).clamp(3, 9) as usize;
        format!("{:.*}", digits, v)
    };
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
            Dataset::Cpu => cpu_rows(&self.cpu_partial.lock().unwrap(), crate::topology::cached_core_kinds()),
            Dataset::Gpu => gpu_rows(&self.gpu_partial.lock().unwrap()),
            Dataset::Gpu3d => {
                let partial = self.bench3d.partial.lock().unwrap().clone();
                match (&self.bench3d.last, partial.is_empty()) {
                    (Some(s), true) => gpu3d_rows(&s.results),
                    _ => gpu3d_rows(&partial),
                }
            }
            Dataset::Input => {
                let mut rows = manual_rows(&self.input_test.runs);
                rows.extend(self.last_input_result.as_ref().map(|s| input_rows(&s.results)).unwrap_or_default());
                rows
            }
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
            egui::ScrollArea::vertical().id_salt("sensors_scroll").auto_shrink([false, false]).show(ui, |ui| self.sensors_view(ui));
            return;
        }
        let rows = self.dataset_rows(d);
        if rows.is_empty() {
            ui.label("No results yet — run a test on its tab; results appear here while it runs.");
            return;
        }

        let mut series: Vec<String> = rows.iter().map(|r| r.series.clone()).collect();
        series.sort_by(|a, b| natural_cmp(a, b));
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
                self.series_picker(ui, d, &series);
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
                if ui.button("⟲ Reset view").on_hover_text("Back to the whole chart after zooming or panning (double-click the chart does the same)").clicked() {
                    self.results_ui.reset_view.insert(d.tag());
                }
            });

            let anomalies = find_anomalies(d, &rows);
            if anomalies.is_empty() {
                ui.label(RichText::new("✔ No unusual values: every point fits its neighbours and its runs agree.").weak().small());
            } else {
                let strong = anomalies.iter().filter(|a| a.strong).count();
                let title = format!("⚠ {} unusual value{}{}: ringed on the chart", anomalies.len(), if anomalies.len() == 1 { "" } else { "s" }, if strong > 0 { format!(" ({} strong)", strong) } else { String::new() });
                egui::CollapsingHeader::new(RichText::new(title).color(Color32::from_rgb(255, 170, 60)))
                    .id_salt(("anoms", d.tag()))
                    .default_open(anomalies.len() <= 12)
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical().id_salt(("anoms_scroll", d.tag())).max_height(220.0).show(ui, |ui| {
                            egui::Grid::new(("anoms_grid", d.tag())).striped(true).spacing([14.0, 2.0]).show(ui, |ui| {
                                for a in anomalies.iter().take(200) {
                                    let idx = series.iter().position(|s| *s == a.series).unwrap_or(0);
                                    ui.label(RichText::new(&a.series).color(color_for(idx)));
                                    ui.label(&a.x_label);
                                    ui.label(RichText::new(&a.why).color(if a.strong { Color32::from_rgb(255, 110, 110) } else { Color32::from_rgb(255, 170, 60) }));
                                    ui.end_row();
                                }
                            });
                        });
                    });
            }
            self.results_ui.marks = anomalies.iter().map(|a| (a.series.clone(), a.x)).collect();

            // ---------------- chart ----------------
            let selectable: Vec<Metric> = metrics.iter().copied().filter(|m| !self.results_ui.col_hidden(d, m.0)).collect();
            if self.results_ui.show_chart {
                // per-core runs next to all-core runs: the 24-thread total would dwarf every core, so
                // the chart starts on the per-thread value
                let mixed = d == Dataset::Cpu && rows.iter().any(|r| r.x < 0.0) && rows.iter().any(|r| r.x >= 0.0);
                let default = if mixed && selectable.iter().any(|m| m.0 == "mcalls_thread") { "mcalls_thread" } else { selectable.first().map(|m| m.0).unwrap_or("") };
                let key = *self.results_ui.chart_metric.entry(d.tag()).or_insert(default);
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

    /// Line switches: a grid with one row per test and one column per thread count (clicking a row or
    /// column name switches that whole group) when the lines have both, otherwise a wrapped list
    fn series_picker(&mut self, ui: &mut Ui, d: Dataset, series: &[String]) {
        let parts: Vec<(String, String)> = series.iter().map(|s| split_series(s)).collect();
        let mut bases: Vec<&String> = parts.iter().map(|p| &p.0).collect();
        let mut cols: Vec<&String> = parts.iter().map(|p| &p.1).collect();
        bases.sort_by(|a, b| natural_cmp(a, b));
        bases.dedup();
        cols.sort_by(|a, b| natural_cmp(a, b));
        cols.dedup();
        if bases.len() < 2 || cols.len() < 2 {
            ui.horizontal_wrapped(|ui| {
                for (i, s) in series.iter().enumerate() {
                    let mut vis = !self.results_ui.series_hidden(d, s);
                    if ui.checkbox(&mut vis, RichText::new(s).color(color_for(i))).changed() {
                        self.results_ui.toggle_series(d, s, vis);
                    }
                }
            });
            return;
        }
        let mut toggle: Option<Vec<usize>> = None;
        egui::ScrollArea::vertical().id_salt(("sergrid_scroll", d.tag())).max_height(280.0).show(ui, |ui| {
            egui::Grid::new(("sergrid", d.tag())).spacing([10.0, 2.0]).show(ui, |ui| {
                ui.label(RichText::new("click a name to switch its group").weak().small());
                for c in &cols {
                    let name = if c.is_empty() { "—" } else { c.as_str() };
                    if ui.small_button(name).on_hover_text(format!("Show / hide every {} line", name)).clicked() {
                        toggle = Some((0..series.len()).filter(|&i| parts[i].1 == **c).collect());
                    }
                }
                ui.end_row();
                for b in &bases {
                    if ui.small_button(b.as_str()).on_hover_text(format!("Show / hide every {} line", b)).clicked() {
                        toggle = Some((0..series.len()).filter(|&i| parts[i].0 == **b).collect());
                    }
                    for c in &cols {
                        match (0..series.len()).find(|&i| parts[i].0 == **b && parts[i].1 == **c) {
                            Some(i) => {
                                let mut vis = !self.results_ui.series_hidden(d, &series[i]);
                                if ui.checkbox(&mut vis, RichText::new("■").color(color_for(i))).on_hover_text(&series[i]).changed() {
                                    self.results_ui.toggle_series(d, &series[i], vis);
                                }
                            }
                            None => {
                                ui.label("");
                            }
                        }
                    }
                    ui.end_row();
                }
            });
        });
        if let Some(group) = toggle {
            let all_on = group.iter().all(|&i| !self.results_ui.series_hidden(d, &series[i]));
            for i in group {
                self.results_ui.toggle_series(d, &series[i], !all_on);
            }
        }
    }

    fn draw_chart(&mut self, ui: &mut Ui, d: Dataset, rows: &[Row], series: &[String], metric: Metric) {
        let log_x = matches!(d, Dataset::Memory | Dataset::Gpu);
        let show_points = self.results_ui.show_points;
        let marks = self.results_ui.marks.clone();
        let log_y = self.results_ui.log_y;
        let zero_y = self.results_ui.zero_y && !log_y;
        let ylabel = format!("{} ({}){}", metric.1, metric.2, if log_y { " — log scale" } else { "" });
        let labels: BTreeMap<i64, String> = rows.iter().map(|r| (r.x.round() as i64, r.x_label.clone())).collect();
        let x_label_of = labels.clone();
        let mut meas = self.results_ui.measures.remove(d.tag()).unwrap_or_default();
        meas.tools(ui, "Click where a range starts, then where it ends: min / average / max of every line in it");
        // no legend inside the plot: with dozens of lines it covered the data; it is drawn below instead
        let mut plot = Plot::new(("plot", d.tag()))
            .height(320.0)
            .y_axis_label(ylabel)
            .allow_drag(meas.mode.is_none())
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
        if self.results_ui.reset_view.remove(d.tag()) {
            plot = plot.reset();
        }
        let mut pointer_x = None;
        let y_top = rows
            .iter()
            .filter(|r| !self.results_ui.series_hidden(d, &r.series))
            .filter_map(|r| r.values.get(metric.0).copied())
            .filter(|v| !log_y || *v > 0.0)
            .map(|v| if log_y { v.log10() } else { v })
            .fold(f64::MIN, f64::max);
        let resp = plot.show(ui, |plot_ui| {
            if y_top > f64::MIN {
                meas.draw(plot_ui, y_top);
            }
            pointer_x = plot_ui.pointer_coordinate().map(|p| p.x);
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
                let ringed: Vec<[f64; 2]> = pts.iter().copied().filter(|p| marks.iter().any(|m| m.0 == *s && (m.1 - p[0]).abs() < 1e-9)).collect();
                if !ringed.is_empty() {
                    plot_ui.points(Points::new(PlotPoints::from(ringed)).color(Color32::from_rgb(255, 93, 93)).radius(8.0).filled(false).shape(egui_plot::MarkerShape::Circle));
                }
                if show_points {
                    plot_ui.points(Points::new(PlotPoints::from(pts)).name(s).color(color).radius(3.5_f32));
                }
            }
        });
        if resp.response.clicked() {
            if let Some(x) = pointer_x {
                // snap to the nearest measured x (buffer sizes, cores)
                let snapped = rows.iter().map(|r| r.x).min_by(|a, b| (a - x).abs().total_cmp(&(b - x).abs())).unwrap_or(x);
                meas.click(snapped, false, None);
            }
        }
        let shown: Vec<(usize, &String)> = series.iter().enumerate().filter(|(_, s)| !self.results_ui.series_hidden(d, s)).collect();
        legend_below(ui, ("legend", d.tag()), shown.iter().map(|(i, s)| (color_for(*i), s.as_str())));
        if !meas.markers.is_empty() || !meas.ranges.is_empty() {
            let lines: Vec<(String, Vec<[f64; 2]>)> = shown
                .iter()
                .map(|(_, s)| {
                    let mut pts: Vec<[f64; 2]> = rows.iter().filter(|r| &r.series == *s).filter_map(|r| r.values.get(metric.0).map(|v| [r.x, *v])).collect();
                    pts.sort_by(|a, b| a[0].total_cmp(&b[0]));
                    ((*s).clone(), pts)
                })
                .collect();
            let fmt_x = move |x: f64| if log_x { human_size(2f64.powf(x) as usize) } else { x_label_of.get(&(x.round() as i64)).cloned().unwrap_or_else(|| format!("{:.2}", x)) };
            meas.panel(ui, d.tag(), &lines, metric.2, &fmt_x);
        }
        self.results_ui.measures.insert(d.tag(), meas);
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
                (SensorGroup::Power, "Power"),
                (SensorGroup::Fans, "Fans"),
                (SensorGroup::Voltages, "Voltages / currents"),
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
        // per-test background bands: one colour per category, one shade of it per test
        let phases: Vec<Phase> = self.last_phases.clone();
        let keys = phase_keys(&phases);
        let shade_of = |p: &Phase| -> Color32 {
            let ks = keys.get(&p.kind).map(|v| v.as_slice()).unwrap_or(&[]);
            let k = phase_test_key(p);
            phase_shade(&p.kind, ks.iter().position(|x| x == k).unwrap_or(0), ks.len())
        };
        if !phases.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.label("Test backgrounds:");
                for (k, name) in [("input", "Input"), ("memory", "Memory"), ("cpu", "CPU"), ("gpu", "GPU")] {
                    let n = phases.iter().filter(|p| p.kind == k).count();
                    if n > 0 {
                        ui.label(RichText::new(format!("■ {} ({})", name, n)).color(phase_color(k)));
                    }
                }
                ui.add(egui::Slider::new(&mut self.results_ui.phase_opacity, 0.0..=0.6).text("opacity"));
                ui.label(RichText::new("hover the chart to see which test ran").weak().small());
            });
            egui::CollapsingHeader::new(RichText::new("Test colours").small()).default_open(false).show(ui, |ui| {
                for (k, name) in [("input", "Input"), ("memory", "Memory"), ("cpu", "CPU"), ("gpu", "GPU")] {
                    let Some(ks) = keys.get(k) else { continue };
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(format!("{}:", name)).small());
                        for (i, key) in ks.iter().enumerate() {
                            ui.label(RichText::new(format!("■ {}", key)).small().color(phase_shade(k, i, ks.len())));
                        }
                    });
                }
            });
        }
        let visible: Vec<&(String, Vec<[f64; 2]>)> =
            lines.iter().filter(|(n, _)| !self.results_ui.series_hidden(Dataset::Sensors, n)).collect();
        let ys = visible.iter().flat_map(|(_, p)| p.iter().map(|q| q[1]));
        let (lo, hi) = ys.fold((f64::MAX, f64::MIN), |(a, b), y| (a.min(y), b.max(y)));
        let pad = if hi > lo { (hi - lo) * 0.05 } else { 1.0 };
        let (band_lo, band_hi) = (lo - pad, hi + pad);
        let opacity = self.results_ui.phase_opacity;
        if ui.button("⟲ Reset view").on_hover_text("Back to the whole run after zooming or panning (double-click the chart does the same)").clicked() {
            self.results_ui.reset_view.insert("sen");
        }
        let mut meas = self.results_ui.measures.remove("sen").unwrap_or_default();
        meas.tools(ui, "Click where a range starts, then where it ends; Shift + click measures the test under the pointer");
        // the mouse wheel scrolls the page (zoom: Ctrl + wheel or a box drag), like the results charts
        let mut sensor_plot = Plot::new("sensor_plot").height(420.0).x_axis_label("seconds since the test started").allow_drag(meas.mode.is_none()).allow_scroll(false);
        if self.results_ui.reset_view.remove("sen") {
            sensor_plot = sensor_plot.reset();
        }
        let mut pointer_x = None;
        let plot = sensor_plot.show(ui, |plot_ui| {
            if band_lo < band_hi {
                meas.draw(plot_ui, band_hi);
            }
            pointer_x = plot_ui.pointer_coordinate().map(|p| p.x);
            if opacity > 0.0 && band_lo < band_hi {
                for p in &phases {
                    let (x0, x1) = (p.start_ms as f64 / 1000.0, (p.end_ms.max(p.start_ms + 50)) as f64 / 1000.0);
                    let c = shade_of(p);
                    let fill = Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (opacity * 255.0) as u8);
                    plot_ui.polygon(
                        Polygon::new(PlotPoints::from(vec![[x0, band_lo], [x1, band_lo], [x1, band_hi], [x0, band_hi]]))
                            .fill_color(fill)
                            .stroke(egui::Stroke::NONE),
                    );
                }
            }
            for (i, (name, pts)) in lines.iter().enumerate() {
                if self.results_ui.series_hidden(Dataset::Sensors, name) {
                    continue;
                }
                plot_ui.line(Line::new(PlotPoints::from(pts.clone())).name(name).color(color_for(i)));
            }
            // which test was running under the pointer
            let x = plot_ui.pointer_coordinate()?.x;
            let p = phases.iter().rev().find(|p| x >= p.start_ms as f64 / 1000.0 && x <= p.end_ms.max(p.start_ms + 50) as f64 / 1000.0)?;
            Some(format!("{}\n{:.1} – {:.1} s ({:.1} s)", p.label, p.start_ms as f64 / 1000.0, p.end_ms as f64 / 1000.0, (p.end_ms.saturating_sub(p.start_ms)) as f64 / 1000.0))
        });
        if plot.response.clicked() {
            if let Some(x) = pointer_x {
                let shift = ui.input(|i| i.modifiers.shift);
                let band = phases
                    .iter()
                    .rev()
                    .find(|p| x >= p.start_ms as f64 / 1000.0 && x <= p.end_ms.max(p.start_ms + 50) as f64 / 1000.0)
                    .map(|p| (p.start_ms as f64 / 1000.0, p.end_ms as f64 / 1000.0, p.label.chars().take(48).collect::<String>()));
                meas.click(x, shift, band);
            }
        }
        if let Some(text) = plot.inner {
            plot.response.on_hover_text_at_pointer(text);
        }
        let shown: Vec<(usize, &String)> =
            lines.iter().map(|l| &l.0).enumerate().filter(|(_, n)| !self.results_ui.series_hidden(Dataset::Sensors, n)).collect();
        legend_below(ui, "sensor_legend", shown.iter().map(|(i, n)| (color_for(*i), n.as_str())));
        if !meas.markers.is_empty() || !meas.ranges.is_empty() {
            let vis: Vec<(String, Vec<[f64; 2]>)> = lines.iter().filter(|(n, _)| !self.results_ui.series_hidden(Dataset::Sensors, n)).cloned().collect();
            meas.panel(ui, "sen", &vis, "", &|x: f64| format!("{:.1} s", x));
        }
        self.results_ui.measures.insert("sen", meas);

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
            }
            SensorGroup::Power => {
                add("GPU power W", x, s.gpu.as_ref().and_then(|g| g.power_w));
            }
            SensorGroup::Fans | SensorGroup::Voltages => {}
            SensorGroup::Memory => {
                add("RAM used MB", x, Some(s.ram_used_mb));
                add("VRAM used MB", x, s.gpu.as_ref().and_then(|g| g.vram_used_mb));
            }
        }
        // every extra sensor (board, VRM, DIMM, drives, fans, PSU, RAPL ...) in the group of its unit
        let kinds: &[SensorKind] = match group {
            SensorGroup::Temperatures => &[SensorKind::Temp],
            SensorGroup::Power => &[SensorKind::Power],
            SensorGroup::Fans => &[SensorKind::Fan],
            SensorGroup::Voltages => &[SensorKind::Voltage, SensorKind::Current],
            // other programs' CPU / GPU share
            SensorGroup::Load => &[SensorKind::Load],
            _ => &[],
        };
        for r in s.sensors.iter().filter(|r| kinds.contains(&r.kind)) {
            add(&format!("{} {}", r.name, r.kind.unit()), x, Some(r.value));
        }
    }
    let mut out: Vec<(String, Vec<[f64; 2]>)> = lines.into_iter().collect();
    out.sort_by(|a, b| natural_cmp(&a.0, &b.0));
    out
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

    #[test]
    fn small_values_keep_significant_digits() {
        assert_eq!(format_value(0.00042, "ms"), "0.000420 ms");
        assert_eq!(format_value(0.036, ""), "0.0360");
        assert_eq!(format_value(0.5, ""), "0.50");
        assert_eq!(format_value(12.34, ""), "12.3");
        assert_eq!(format_value(5000.0, "MHz"), "5000 MHz");
        assert_eq!(format_value(0.0, ""), "0");
    }
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
        let rows = cpu_rows(&[cpu_result(3, 2e6), cpu_result(5, 2e6)], None);
        assert_eq!(rows[0].x, 3.0);
        assert_eq!(rows[0].values["this_core_temp"], 63.0);
        assert_eq!(rows[1].values["mcalls"], 2.0);
        assert_eq!(rows[1].x_label, "Core 5");
    }

    #[test]
    fn slow_core_is_flagged() {
        let mut v: Vec<_> = (0..6).map(|c| cpu_result(c, 1e6)).collect();
        v[4] = cpu_result(4, 0.7e6);
        let slow = find_anomalies(Dataset::Cpu, &cpu_rows(&v, None));
        assert_eq!(slow.len(), 1);
        assert_eq!(slow[0].x, 4.0);
        assert_eq!(slow[0].why, "30% slower than the median core");
        assert!(find_anomalies(Dataset::Cpu, &cpu_rows(&v[..2], None)).is_empty()); // too few cores to judge
    }

    #[test]
    fn anomalies_find_spikes_peers_and_unstable_runs() {
        let mem = |x: f64, ns: f64, p99: f64| {
            let mut v = BTreeMap::new();
            v.insert("ns_per_access", ns);
            v.insert("min_run", 100.0);
            v.insert("p99_run", p99);
            Row { series: "StridedRead(4096 B) · 1T".into(), x, x_label: format!("{} KB", x), values: v, ..Default::default() }
        };
        // a cache step (1 -> 5 ns) is fine; the 128 KB spike and the unstable last point are not
        let rows = vec![mem(12.0, 1.0, 110.0), mem(13.0, 1.1, 110.0), mem(14.0, 1.0, 110.0), mem(15.0, 9.0, 110.0), mem(16.0, 1.0, 110.0), mem(17.0, 5.0, 110.0), mem(18.0, 5.2, 110.0), mem(19.0, 5.1, 500.0)];
        let a = find_anomalies(Dataset::Memory, &rows);
        assert_eq!(a.iter().filter(|a| a.why.starts_with("time per access")).map(|a| a.x).collect::<Vec<_>>(), vec![15.0]);
        assert!(a.iter().any(|a| a.x == 19.0 && a.why.starts_with("unstable") && a.strong));
        // CPU: one P core 20% slow, E cores differ from P cores by design and are not flagged
        use CoreKind::{Efficiency as E, Performance as P};
        let kinds = [P, P, P, E, E, E];
        let mut v: Vec<_> = (0..6).map(|c| cpu_result(c, if kinds[c] == P { 5e6 } else { 4e6 })).collect();
        v[1] = cpu_result(1, 4e6);
        let a = find_anomalies(Dataset::Cpu, &cpu_rows(&v, Some(&kinds)));
        assert_eq!(a.len(), 1);
        assert_eq!((a[0].x, a[0].why.as_str()), (1.0, "20% slower than the median P-core"));
    }

    #[test]
    fn names_sort_naturally_and_split_into_test_and_threads() {
        let mut v = vec!["StreamAdd · 12T", "StreamAdd · 2T", "StreamAdd · 1T", "Core 10 °C", "Core 9 °C", "PointerChase · 24T"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, vec!["Core 9 °C", "Core 10 °C", "PointerChase · 24T", "StreamAdd · 1T", "StreamAdd · 2T", "StreamAdd · 12T"]);
        assert_eq!(split_series("StreamAdd · 12T"), ("StreamAdd".into(), "12T".into()));
        assert_eq!(split_series("StridedRead(64 B) · 4T · P-cores 0-7"), ("StridedRead(64 B) · P-cores 0-7".into(), "4T".into()));
        assert_eq!(split_series("GameSim · 24T all cores"), ("GameSim".into(), "24T all cores".into()));
        assert_eq!(split_series("GameSim"), ("GameSim".into(), String::new()));
        assert_eq!(split_series("Tiles · 12Tx"), ("Tiles · 12Tx".into(), String::new()));
    }

    #[test]
    fn hybrid_cores_are_compared_with_their_own_kind() {
        use CoreKind::{Efficiency as E, Performance as P};
        // IntegerAdd on Arrow Lake: E cores 5x the P cores; no core is slow within its kind
        let kinds = [P, P, E, E, E, E, P, P];
        let v: Vec<_> = (0..8).map(|c| cpu_result(c, if kinds[c] == P { 0.8e6 } else { 4.0e6 })).collect();
        assert!(find_anomalies(Dataset::Cpu, &cpu_rows(&v, Some(&kinds))).is_empty());
        assert_eq!(find_anomalies(Dataset::Cpu, &cpu_rows(&v, None)).len(), 8, "without kinds P and E cores look like outliers");
        let rows = cpu_rows(&v, Some(&kinds));
        assert_eq!(rows[2].x_label, "Core 2 · E");
    }

    #[test]
    fn all_core_runs_get_their_own_slot_next_to_per_core_runs() {
        let mut all = cpu_result(0, 110e6);
        all.core_mask = 0xFF_FFFF;
        all.thread_count = 24;
        all.cores = "All cores (OS scheduled)".into();
        let rows = cpu_rows(&[all.clone(), cpu_result(0, 5e6), cpu_result(1, 5e6)], None);
        assert_eq!(rows[0].x, -2.0, "not on core 0's position");
        assert!(rows[0].series.ends_with("· 24T all cores"), "{}", rows[0].series);
        assert!((rows[0].values["mcalls_thread"] - 110.0 / 24.0).abs() < 1e-9);
        assert_eq!(rows[1].x, 0.0);
        // all-core runs alone: threads on the x axis
        let only = cpu_rows(&[all], None);
        assert_eq!((only[0].x, only[0].x_label.as_str()), (24.0, "24 threads"));
    }

    #[test]
    fn memory_rows_include_telemetry_and_log_size_axis() {
        let r = MemoryBenchmarkResult {
            passes_per_run: 1,
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
        let row = Row { series: "a".into(), x: 1.0, x_label: "1 MB".into(), values: v, ..Default::default() };
        let csv = to_csv(&[&row], &[MEMORY_METRICS[0], MEMORY_METRICS[1]]);
        assert_eq!(csv.lines().next().unwrap(), "series,x,Latency / access (ns),Bandwidth (GB/s)");
        assert_eq!(csv.lines().nth(1).unwrap(), "\"a\",\"1 MB\",,2");
    }

    #[test]
    fn manual_runs_become_one_line_each_with_corrected_values() {
        use crate::input_test::{summarize, InputKind};
        use crate::rig::RigCalibration;
        let human = RunRecord { summary: summarize(InputKind::MouseClick, false, &[200.0, 210.0], 0, 0, (500.0, 2000.0)), cal: RigCalibration::default() };
        let robot = RunRecord {
            summary: summarize(InputKind::KeyPress, true, &[30.0, 32.0, 31.0], 0, 0, (500.0, 2000.0)),
            cal: RigCalibration { keyboard_robot_ms: 6.0, subtract_robot: true, ..Default::default() },
        };
        let rows = manual_rows(&[human, robot]);
        assert_eq!(rows.len(), 5);
        assert!(rows[0].series.starts_with("Mouse click · human"));
        assert!(!rows[0].values.contains_key("trial_corrected_ms"));
        assert_eq!(rows[2].series, "Keyboard press · robot #2");
        assert_eq!(rows[2].values["trial_ms"], 30.0);
        assert_eq!(rows[2].values["trial_corrected_ms"], 24.0);
        assert_eq!(rows[4].x, 3.0);
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
            sensors: Vec::new(),
            ..Default::default()
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

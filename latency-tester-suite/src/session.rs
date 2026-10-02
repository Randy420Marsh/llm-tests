//! Builds the JSON that is signed and saved: every suite that has data goes into one record,
//! so nothing measured in a session is lost (partial results of stopped runs, manual click/key
//! trials, the virtualization state and the sensor timeline included).

use serde_json::{json, Map, Value};

use crate::cpu_benchmark::{CoreTopology, CpuBenchmarkConfig, CpuBenchmarkResult};
use crate::gpu_benchmark::{GpuBenchmarkConfig, GpuBenchmarkResult, VulkanInfo};
use crate::input_latency::InputLatencySummary;
use crate::input_test::RunSummary;
use crate::memory_benchmark::{MemoryBenchmarkConfig, MemoryBenchmarkResult};
use crate::rig::RigCalibration;
use crate::sensors::{Phase, Snapshot};

/// Which parts of the session to save
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    Everything,
    Memory,
    Cpu,
    Gpu,
    Input,
    Sensors,
}

impl Scope {
    /// `test_type` written into the signed record
    pub fn test_type(self) -> &'static str {
        match self {
            Scope::Everything => "session",
            Scope::Memory => "memory",
            Scope::Cpu => "cpu",
            Scope::Gpu => "gpu",
            Scope::Input => "input",
            Scope::Sensors => "sensors",
        }
    }

    fn includes(self, other: Scope) -> bool {
        self == Scope::Everything || self == other
    }
}

/// Borrowed view of everything the app currently holds
#[derive(Default)]
pub struct SessionData<'a> {
    pub memory_config: Option<&'a MemoryBenchmarkConfig>,
    pub memory: &'a [MemoryBenchmarkResult],
    pub memory_planned: usize,
    pub cpu_config: Option<&'a CpuBenchmarkConfig>,
    pub cpu_topology: Option<&'a CoreTopology>,
    pub cpu: &'a [CpuBenchmarkResult],
    pub gpu_config: Option<&'a GpuBenchmarkConfig>,
    pub gpu_vulkan: Option<&'a VulkanInfo>,
    pub gpu: &'a [GpuBenchmarkResult],
    pub input_suite: Option<&'a InputLatencySummary>,
    /// Manual click / key-press runs with the calibration that was active for each
    pub trials: &'a [(RunSummary, RigCalibration)],
    pub timeline: &'a [Snapshot],
    /// Every test that ran while the timeline was recorded (coloured bands in the report)
    pub phases: &'a [Phase],
    pub sensor_notes: &'a [String],
    pub virtualization: Option<Value>,
    /// Clock source and timer resolution (crate::timer_info)
    pub timers: Option<Value>,
    pub calibration: Option<&'a RigCalibration>,
    /// Configs of further memory / CPU passes of a combined run (e.g. "each core on its own"); the
    /// results of all passes are in `memory` / `cpu` and say which cores they ran on
    pub memory_extra_configs: &'a [MemoryBenchmarkConfig],
    pub cpu_extra_configs: &'a [CpuBenchmarkConfig],
    /// How a "run all tests" run was set up and how it ended
    pub run_info: Option<Value>,
    /// Mouse polling runs (the user moved the mouse; `mouse_poll`)
    pub mouse_polling: &'a [crate::mouse_poll::MousePollResult],
    /// Reflex game results (`aim_game`)
    pub reflex_game: &'a [crate::aim_game::AimResult],
    /// 3D graphics benchmark (`bench3d`)
    pub gpu3d: Option<&'a crate::bench3d::Bench3dSummary>,
}

/// Timeline points kept in the saved record (a long run has tens of thousands of samples)
pub const MAX_TIMELINE_POINTS: usize = 1500;

pub fn downsample_timeline(tl: &[Snapshot], max_points: usize) -> Vec<Value> {
    if tl.is_empty() || max_points == 0 {
        return Vec::new();
    }
    let step = tl.len().div_ceil(max_points).max(1);
    tl.iter()
        .step_by(step)
        .map(|s| {
            let freqs: Vec<f32> = s.core_freq_mhz.iter().copied().filter(|f| *f > 0.0).collect();
            json!({
                "t_s": s.t_ms as f64 / 1000.0,
                "cpu_package_c": s.cpu_package_c,
                "hottest_core_c": s.core_temps_c.iter().map(|c| c.1).fold(None, |m: Option<f32>, v| Some(m.map_or(v, |m| m.max(v)))),
                "cpu_freq_avg_mhz": if freqs.is_empty() { None } else { Some(freqs.iter().sum::<f32>() / freqs.len() as f32) },
                "cpu_load_pct": if s.core_usage_pct.is_empty() { None } else { Some(s.core_usage_pct.iter().sum::<f32>() / s.core_usage_pct.len() as f32) },
                "ram_used_mb": s.ram_used_mb,
                "gpu_temp_c": s.gpu.as_ref().and_then(|g| g.temp_c),
                "gpu_util_pct": s.gpu.as_ref().and_then(|g| g.util_pct),
                "gpu_power_w": s.gpu.as_ref().and_then(|g| g.power_w),
                "vram_used_mb": s.gpu.as_ref().and_then(|g| g.vram_used_mb),
                "cpu_power_w": s.cpu_package_power_w(),
                // every other sensor by name (board, VRM, DIMM, drive temperatures, fans, power, voltages)
                "sensors": s.sensors.iter().map(|r| (r.name.clone(), json!(r.value))).collect::<Map<String, Value>>(),
            })
        })
        .collect()
}

pub fn core_peaks(tl: &[Snapshot]) -> Vec<(usize, f32)> {
    let mut m: std::collections::BTreeMap<usize, f32> = Default::default();
    for s in tl {
        for &(c, t) in &s.core_temps_c {
            let e = m.entry(c).or_insert(t);
            *e = e.max(t);
        }
    }
    m.into_iter().collect()
}

/// name -> {kind, unit} for every extra sensor seen in the timeline
pub fn sensor_kinds(tl: &[Snapshot]) -> Map<String, Value> {
    let mut m = Map::new();
    for s in tl {
        for r in &s.sensors {
            m.entry(r.name.clone()).or_insert_with(|| json!({ "kind": r.kind, "unit": r.kind.unit() }));
        }
    }
    m
}

fn to_value<T: serde::Serialize>(t: &T) -> Value {
    serde_json::to_value(t).unwrap_or(Value::Null)
}

/// (benchmark_config, benchmark_results) for `log_result`. A part is only included if it has data.
pub fn build(data: &SessionData, scope: Scope) -> (Value, Value) {
    let mut config = Map::new();
    let mut results = Map::new();

    if scope.includes(Scope::Memory) && !data.memory.is_empty() {
        config.insert("memory".into(), to_value(&data.memory_config));
        if !data.memory_extra_configs.is_empty() {
            config.insert("memory_extra_passes".into(), to_value(&data.memory_extra_configs));
        }
        results.insert(
            "memory".into(),
            json!({
                "complete": data.memory_planned == 0 || data.memory.len() >= data.memory_planned,
                "tests_done": data.memory.len(),
                "tests_planned": data.memory_planned,
                "results": data.memory,
            }),
        );
    }
    if scope.includes(Scope::Cpu) && !data.cpu.is_empty() {
        config.insert("cpu".into(), to_value(&data.cpu_config));
        if !data.cpu_extra_configs.is_empty() {
            config.insert("cpu_extra_passes".into(), to_value(&data.cpu_extra_configs));
        }
        results.insert("cpu".into(), json!({ "core_topology": data.cpu_topology, "results": data.cpu }));
    }
    if scope.includes(Scope::Gpu) && !data.gpu.is_empty() {
        config.insert("gpu".into(), to_value(&data.gpu_config));
        results.insert("gpu".into(), json!({ "vulkan": data.gpu_vulkan, "results": data.gpu }));
    }
    if let Some(g) = data.gpu3d.filter(|g| scope.includes(Scope::Gpu) && !g.results.is_empty()) {
        config.insert("gpu3d".into(), to_value(&g.config));
        results.insert("gpu3d".into(), json!({ "adapter": g.adapter, "backend": g.backend, "driver": g.driver, "results": g.results }));
    }
    if scope.includes(Scope::Input) {
        if let Some(s) = data.input_suite {
            config.insert("input_timing_suite".into(), to_value(&s.config));
            results.insert("input_timing_suite".into(), json!({ "results": s.results }));
        }
        if !data.trials.is_empty() {
            let runs: Vec<Value> = data
                .trials
                .iter()
                .map(|(r, cal)| {
                    let c = cal.correct(r.kind, r.robot, r.mean_ms);
                    json!({
                        "summary": r,
                        "corrected_mean_ms": { "raw": c.raw_ms, "minus_robot": c.minus_robot_ms, "minus_robot_and_display": c.minus_robot_and_display_ms },
                        "calibration_used": cal,
                    })
                })
                .collect();
            config.insert("input_trials".into(), json!({ "rig_calibration": data.calibration }));
            results.insert("input_trials".into(), json!({ "runs": runs }));
        }
        if !data.mouse_polling.is_empty() {
            results.insert("mouse_polling".into(), json!({ "runs": data.mouse_polling }));
        }
        if !data.reflex_game.is_empty() {
            results.insert("reflex_game".into(), json!({ "runs": data.reflex_game }));
        }
    }
    if scope.includes(Scope::Sensors) && !data.timeline.is_empty() {
        config.insert("sensors".into(), json!({ "sample_interval_ms": 500, "max_points": MAX_TIMELINE_POINTS }));
        results.insert(
            "sensors".into(),
            json!({
                "sources": data.sensor_notes,
                "samples_recorded": data.timeline.len(),
                "duration_s": data.timeline.last().map(|s| s.t_ms as f64 / 1000.0),
                "core_peak_temps_c": core_peaks(data.timeline),
                // other programs over the whole run, busiest first
                "programs": crate::sensors::program_usage(data.timeline).map(|v| v.into_iter().take(20).map(|p| json!({
                    "name": p.0, "cpu_avg_pct": p.1, "cpu_max_pct": p.2, "gpu_avg_pct": p.3, "gpu_max_pct": p.4,
                })).collect::<Vec<_>>()),
                "sensor_kinds": sensor_kinds(data.timeline),
                "phases": data.phases.iter().map(|p| json!({
                    "kind": p.kind,
                    "label": p.label,
                    "t0_s": p.start_ms as f64 / 1000.0,
                    "t1_s": p.end_ms as f64 / 1000.0,
                })).collect::<Vec<_>>(),
                "timeline": downsample_timeline(data.timeline, MAX_TIMELINE_POINTS),
            }),
        );
    }
    if scope == Scope::Everything {
        if let Some(v) = &data.run_info {
            config.insert("run_all".into(), v.clone());
        }
        if let Some(v) = &data.timers {
            results.insert("timers".into(), v.clone());
        }
        if let Some(v) = &data.virtualization {
            results.insert("virtualization".into(), v.clone());
        }
    }
    (Value::Object(config), Value::Object(results))
}

/// True if `build` would produce anything for this scope
pub fn has_data(data: &SessionData, scope: Scope) -> bool {
    let (_, r) = build(data, scope);
    r.as_object().is_some_and(|o| o.keys().any(|k| k != "virtualization"))
}

// ---------------------------------------------------------------------------------------------
// CSV export
// ---------------------------------------------------------------------------------------------

/// Flatten nested objects into `a.b.c` columns; arrays are written as compact JSON text
fn flatten(prefix: &str, v: &Value, out: &mut Vec<(String, String)>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                let key = if prefix.is_empty() { k.clone() } else { format!("{}.{}", prefix, k) };
                flatten(&key, x, out);
            }
        }
        Value::Null => out.push((prefix.to_string(), String::new())),
        Value::String(s) => out.push((prefix.to_string(), s.clone())),
        Value::Array(a) if a.is_empty() => out.push((prefix.to_string(), String::new())),
        other => out.push((prefix.to_string(), other.to_string())),
    }
}

/// One CSV: a column for every field that appears in any row, in first-seen order
pub fn rows_to_csv(rows: &[Value]) -> String {
    let flat: Vec<Vec<(String, String)>> = rows
        .iter()
        .map(|r| {
            let mut o = Vec::new();
            flatten("", r, &mut o);
            o
        })
        .collect();
    let mut cols: Vec<String> = Vec::new();
    for row in &flat {
        for (k, _) in row {
            if !cols.contains(k) {
                cols.push(k.clone());
            }
        }
    }
    let mut w = csv::Writer::from_writer(Vec::new());
    let _ = w.write_record(&cols);
    for row in &flat {
        let rec: Vec<&str> = cols.iter().map(|c| row.iter().find(|(k, _)| k == c).map_or("", |(_, v)| v.as_str())).collect();
        let _ = w.write_record(&rec);
    }
    String::from_utf8(w.into_inner().unwrap_or_default()).unwrap_or_default()
}

/// (file name, contents) for every suite that has data: memory, cpu, gpu, input_timing, input_trials, sensors
pub fn csv_files(data: &SessionData) -> Vec<(String, String)> {
    let mut files = Vec::new();
    let mut add = |name: &str, rows: Vec<Value>| {
        if !rows.is_empty() {
            files.push((name.to_string(), rows_to_csv(&rows)));
        }
    };
    add("memory.csv", data.memory.iter().map(to_value).collect());
    add("cpu.csv", data.cpu.iter().map(to_value).collect());
    add("gpu.csv", data.gpu.iter().map(to_value).collect());
    add(
        "gpu3d.csv",
        data.gpu3d
            .map(|g| {
                g.results
                    .iter()
                    .map(|r| {
                        let mut v = to_value(r);
                        if let Some(o) = v.as_object_mut() {
                            o.remove("frametimes_ms");
                        }
                        v
                    })
                    .collect()
            })
            .unwrap_or_default(),
    );
    add("input_timing.csv", data.input_suite.map(|s| s.results.iter().map(to_value).collect()).unwrap_or_default());
    let mut trials = Vec::new();
    for (i, (run, cal)) in data.trials.iter().enumerate() {
        for (n, ms) in run.samples_ms.iter().enumerate() {
            let c = cal.correct(run.kind, run.robot, *ms);
            trials.push(json!({
                "run": i + 1,
                "kind": run.kind.label(),
                "robot": run.robot,
                "trial": n + 1,
                "latency_ms": ms,
                "minus_robot_ms": c.minus_robot_ms,
                "minus_robot_and_display_ms": c.minus_robot_and_display_ms,
            }));
        }
    }
    add("input_trials.csv", trials);
    // one row per run; the per-report series stay in the signed record
    add(
        "mouse_polling.csv",
        data.mouse_polling
            .iter()
            .map(|r| {
                let mut v = to_value(r);
                if let Some(o) = v.as_object_mut() {
                    o.remove("intervals");
                    o.remove("x_counts");
                }
                v
            })
            .collect(),
    );
    // one row per circle hit
    let mut hits = Vec::new();
    for (g, r) in data.reflex_game.iter().enumerate() {
        for (i, t) in r.times_ms.iter().enumerate() {
            hits.push(json!({ "game": g + 1, "mode": r.mode, "radius_px": r.radius, "circle": i + 1, "time_ms": t }));
        }
    }
    add("reflex_game.csv", hits);
    add("sensors.csv", downsample_timeline(data.timeline, data.timeline.len().max(1)));
    add(
        "phases.csv",
        data.phases
            .iter()
            .map(|p| json!({ "kind": p.kind, "test": p.label, "start_s": p.start_ms as f64 / 1000.0, "end_s": p.end_ms as f64 / 1000.0 }))
            .collect(),
    );
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu_benchmark::{AffinityMode, WorkloadType};
    use crate::input_test::{summarize, InputKind};
    use crate::memory_benchmark::AccessPattern;
    use crate::sensors::{GpuSensors, Telemetry};

    fn mem_result(size: usize) -> MemoryBenchmarkResult {
        MemoryBenchmarkResult {
            passes_per_run: 1,
            size,
            pattern: AccessPattern::RandomRead,
            thread_count: 1,
            latency_ns: 1e6,
            bandwidth_gb_s: 2.0,
            iterations: 5,
            min_latency_ns: 0.9e6,
            max_latency_ns: 1.2e6,
            std_dev_ns: 1e4,
            percentile_50_ns: 1e6,
            percentile_95_ns: 1.1e6,
            percentile_99_ns: 1.2e6,
            percentile_999_ns: 1.2e6,
            ns_per_access: 20.0,
            cores: "Core 3".into(),
            measured_ms: 0.0,
            telemetry: Telemetry { samples: 2, cpu_temp_max_c: Some(61.0), ..Default::default() },
        }
    }

    fn cpu_result() -> CpuBenchmarkResult {
        CpuBenchmarkResult {
            workload: WorkloadType::IntegerAdd,
            thread_count: 1,
            affinity_mode: AffinityMode::CustomMask(8),
            core_mask: 8,
            operations_per_second: 1e6,
            latency_ns: 1000.0,
            instructions_per_cycle: None,
            cycles_per_operation: None,
            frequency_mhz: 5200,
            temperature_c: Some(60.0),
            power_watts: None,
            iteration_results: vec![],
            cores: "Core 3".into(),
            telemetry: Telemetry::default(),
        }
    }

    fn snap(t: u64) -> Snapshot {
        Snapshot {
            t_ms: t,
            cpu_package_c: Some(50.0 + t as f32 / 1000.0),
            core_temps_c: vec![(0, 48.0), (1, 55.0 + t as f32 / 1000.0)],
            core_freq_mhz: vec![5000.0, 0.0, 4000.0],
            core_usage_pct: vec![10.0, 30.0],
            ram_used_mb: 1000.0,
            ram_total_mb: 64000.0,
            gpu: Some(GpuSensors { name: "g".into(), temp_c: Some(40.0), vram_used_mb: Some(500.0), ..Default::default() }),
            sensors: vec![crate::sensors::SensorReading { name: "nct6798: VRM MOS".into(), kind: crate::sensors::SensorKind::Temp, value: 61.5 }, crate::sensors::SensorReading { name: "RAPL: package-0".into(), kind: crate::sensors::SensorKind::Power, value: 88.0 }],
            ..Default::default()
        }
    }

    #[test]
    fn mouse_polling_runs_are_saved_and_exported() {
        let reports: Vec<crate::mouse_poll::Report> = (1..=200).map(|i| crate::mouse_poll::Report { t_ns: i * 1_000_000, dx: 2, dy: 0 }).collect();
        let r = crate::mouse_poll::analyze(&reports, "test").unwrap();
        let runs = vec![r];
        let data = SessionData { mouse_polling: &runs, ..Default::default() };
        let (_, res) = build(&data, Scope::Input);
        assert_eq!(res["mouse_polling"]["runs"][0]["nominal_hz"], 1000.0);
        let files = csv_files(&data);
        let (name, csv) = files.iter().find(|f| f.0 == "mouse_polling.csv").unwrap();
        assert_eq!(name, "mouse_polling.csv");
        assert!(csv.contains("rate_hz") && !csv.contains("intervals"), "the per-report series stay out of the CSV");
        assert!(!build(&data, Scope::Memory).1.as_object().unwrap().contains_key("mouse_polling"));
    }

    #[test]
    fn everything_scope_holds_every_suite_with_data() {
        let mem = vec![mem_result(1 << 20), mem_result(1 << 22)];
        let cpu = vec![cpu_result()];
        let timeline: Vec<Snapshot> = (0..4).map(|i| snap(i * 500)).collect();
        let trials = vec![(summarize(InputKind::MouseClick, true, &[20.0, 22.0], 0, 0, (500.0, 2000.0)), RigCalibration { mouse_robot_ms: 1.5, ..Default::default() })];
        let cal = RigCalibration::default();
        let data = SessionData {
            memory: &mem,
            memory_planned: 4,
            cpu: &cpu,
            trials: &trials,
            timeline: &timeline,
            sensor_notes: &["CPU temperature: hwmon".to_string()],
            virtualization: Some(json!({"platform": "Linux"})),
            calibration: Some(&cal),
            ..Default::default()
        };
        let (cfg, res) = build(&data, Scope::Everything);
        let r = res.as_object().unwrap();
        for k in ["memory", "cpu", "input_trials", "sensors", "virtualization"] {
            assert!(r.contains_key(k), "missing {}", k);
        }
        assert!(!r.contains_key("gpu"), "no GPU data -> no GPU section");
        // config and results are different documents now
        assert_ne!(cfg, res);
        // stopped run: 2 of 4 tests, flagged incomplete but kept
        assert_eq!(res["memory"]["complete"], false);
        assert_eq!(res["memory"]["tests_done"], 2);
        assert_eq!(res["memory"]["results"].as_array().unwrap().len(), 2);
        assert_eq!(res["memory"]["results"][0]["telemetry"]["cpu_temp_max_c"], 61.0);
        // robot run keeps raw and corrected values
        assert_eq!(res["input_trials"]["runs"][0]["corrected_mean_ms"]["raw"], 21.0);
        assert_eq!(res["input_trials"]["runs"][0]["corrected_mean_ms"]["minus_robot"], 19.5);
        // sensors
        assert_eq!(res["sensors"]["core_peak_temps_c"][1][0], 1);
        assert_eq!(res["sensors"]["timeline"].as_array().unwrap().len(), 4);
        // every extra sensor is kept per point, with its unit, and CPU package power is picked out
        assert_eq!(res["sensors"]["timeline"][0]["sensors"]["nct6798: VRM MOS"], 61.5);
        assert_eq!(res["sensors"]["timeline"][0]["cpu_power_w"], 88.0);
        assert_eq!(res["sensors"]["sensor_kinds"]["RAPL: package-0"]["unit"], "W");
    }

    #[test]
    fn extra_passes_and_run_info_are_recorded() {
        let mem = vec![mem_result(1 << 20)];
        let extra = [MemoryBenchmarkConfig::default()];
        let data = SessionData {
            memory: &mem,
            memory_extra_configs: &extra,
            run_info: Some(json!({ "cancelled": false })),
            ..Default::default()
        };
        let (cfg, _) = build(&data, Scope::Everything);
        assert_eq!(cfg["memory_extra_passes"].as_array().unwrap().len(), 1);
        assert_eq!(cfg["run_all"]["cancelled"], false);
        let (cfg, _) = build(&data, Scope::Memory);
        assert!(cfg.get("run_all").is_none(), "run info only belongs to the whole-session record");
    }

    #[test]
    fn csv_export_has_a_column_per_field_and_a_row_per_result() {
        let mem = vec![mem_result(1 << 20), mem_result(1 << 22)];
        let cpu = vec![cpu_result()];
        let trials = vec![(summarize(InputKind::KeyPress, false, &[180.0, 200.0], 0, 0, (500.0, 2000.0)), RigCalibration::default())];
        let timeline: Vec<Snapshot> = (0..3).map(|i| snap(i * 500)).collect();
        let data = SessionData { memory: &mem, cpu: &cpu, trials: &trials, timeline: &timeline, ..Default::default() };
        let files = csv_files(&data);
        let names: Vec<&str> = files.iter().map(|f| f.0.as_str()).collect();
        assert_eq!(names, vec!["memory.csv", "cpu.csv", "input_trials.csv", "sensors.csv"], "only suites with data");
        let memory = &files[0].1;
        let mut rd = csv::Reader::from_reader(memory.as_bytes());
        let header: Vec<String> = rd.headers().unwrap().iter().map(String::from).collect();
        assert!(header.contains(&"size".to_string()) && header.contains(&"telemetry.cpu_temp_max_c".to_string()), "{:?}", header);
        let rows: Vec<csv::StringRecord> = rd.records().map(|r| r.unwrap()).collect();
        assert_eq!(rows.len(), 2);
        let col = |name: &str| header.iter().position(|h| h == name).unwrap();
        assert_eq!(&rows[1][col("size")], "4194304");
        assert_eq!(&rows[0][col("telemetry.cpu_temp_max_c")], "61.0");
        assert_eq!(files[2].1.lines().count(), 3, "header + 2 trials");
        assert_eq!(files[3].1.lines().count(), 4, "header + 3 samples");
        // quoting keeps commas in values from shifting columns
        assert_eq!(rows_to_csv(&[json!({"a": "x,y", "b": 1})]).lines().nth(1), Some("\"x,y\",1"));
        // a field that only some rows have still gets a column
        let csv = rows_to_csv(&[json!({"a": 1}), json!({"a": 2, "b": 3})]);
        assert_eq!(csv.lines().collect::<Vec<_>>(), vec!["a,b", "1,", "2,3"]);
    }

    #[test]
    fn scopes_pick_only_their_part() {
        let mem = vec![mem_result(1 << 20)];
        let cpu = vec![cpu_result()];
        let data = SessionData { memory: &mem, cpu: &cpu, ..Default::default() };
        let (_, only_cpu) = build(&data, Scope::Cpu);
        assert_eq!(only_cpu.as_object().unwrap().keys().collect::<Vec<_>>(), vec!["cpu"]);
        assert!(has_data(&data, Scope::Memory));
        assert!(!has_data(&data, Scope::Gpu));
        assert!(!has_data(&SessionData::default(), Scope::Everything));
        assert_eq!(Scope::Everything.test_type(), "session");
    }

    #[test]
    fn timeline_is_downsampled_but_keeps_the_ends() {
        let tl: Vec<Snapshot> = (0..10_000).map(|i| snap(i * 500)).collect();
        let pts = downsample_timeline(&tl, 1500);
        assert!(pts.len() <= 1500 && pts.len() > 1000);
        assert_eq!(pts[0]["t_s"], 0.0);
        // zero clocks are ignored in the average: (5000 + 4000) / 2
        assert_eq!(pts[0]["cpu_freq_avg_mhz"], 4500.0);
        assert!(downsample_timeline(&[], 10).is_empty());
    }
}

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
use crate::sensors::Snapshot;

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
    pub sensor_notes: &'a [String],
    pub virtualization: Option<Value>,
    pub calibration: Option<&'a RigCalibration>,
}

/// Timeline points kept in the saved record (a long run has tens of thousands of samples)
pub const MAX_TIMELINE_POINTS: usize = 1500;

pub fn downsample_timeline(tl: &[Snapshot], max_points: usize) -> Vec<Value> {
    if tl.is_empty() || max_points == 0 {
        return Vec::new();
    }
    let step = ((tl.len() + max_points - 1) / max_points).max(1);
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

fn to_value<T: serde::Serialize>(t: &T) -> Value {
    serde_json::to_value(t).unwrap_or(Value::Null)
}

/// (benchmark_config, benchmark_results) for `log_result`. A part is only included if it has data.
pub fn build(data: &SessionData, scope: Scope) -> (Value, Value) {
    let mut config = Map::new();
    let mut results = Map::new();

    if scope.includes(Scope::Memory) && !data.memory.is_empty() {
        config.insert("memory".into(), to_value(&data.memory_config));
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
        results.insert("cpu".into(), json!({ "core_topology": data.cpu_topology, "results": data.cpu }));
    }
    if scope.includes(Scope::Gpu) && !data.gpu.is_empty() {
        config.insert("gpu".into(), to_value(&data.gpu_config));
        results.insert("gpu".into(), json!({ "vulkan": data.gpu_vulkan, "results": data.gpu }));
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
                "timeline": downsample_timeline(data.timeline, MAX_TIMELINE_POINTS),
            }),
        );
    }
    if scope == Scope::Everything {
        if let Some(v) = &data.virtualization {
            results.insert("virtualization".into(), v.clone());
        }
    }
    (Value::Object(config), Value::Object(results))
}

/// True if `build` would produce anything for this scope
pub fn has_data(data: &SessionData, scope: Scope) -> bool {
    let (_, r) = build(data, scope);
    r.as_object().map_or(false, |o| o.keys().any(|k| k != "virtualization"))
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
        }
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

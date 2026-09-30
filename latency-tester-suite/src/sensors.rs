//! Hardware sensor sampling (temperatures, clocks, RAM, GPU, VRAM).
//!
//! A background [`Sampler`] records [`Snapshot`]s while a benchmark runs; each test then asks
//! for a [`Telemetry`] summary of the samples that fall inside its time window.
//!
//! Sources (whatever is available is used, nothing needs admin rights):
//! * Linux: `/sys/class/hwmon` (coretemp / k10temp per-core temperatures, amdgpu) and
//!   `/sys/class/drm` (AMD VRAM)
//! * NVIDIA GPUs on any OS: `nvidia-smi`
//! * Windows: per-core CPU temperatures need a helper that exposes them, so the sampler reads
//!   LibreHardwareMonitor / OpenHardwareMonitor through WMI when one of them is running,
//!   falling back to the ACPI thermal-zone counters (a package-level value, not per core)
//! * everywhere: per-CPU clocks / load and RAM use via `sysinfo`

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GpuSensors {
    pub name: String,
    pub temp_c: Option<f32>,
    pub util_pct: Option<f32>,
    pub vram_used_mb: Option<f32>,
    pub vram_total_mb: Option<f32>,
    pub power_w: Option<f32>,
    pub clock_mhz: Option<f32>,
}

/// What an extra sensor measures
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SensorKind {
    Temp,
    Fan,
    Power,
    Voltage,
    Current,
    /// A share of the machine in %, e.g. how much CPU or GPU another program used
    Load,
}

impl SensorKind {
    pub fn unit(self) -> &'static str {
        match self {
            SensorKind::Temp => "°C",
            SensorKind::Fan => "RPM",
            SensorKind::Power => "W",
            SensorKind::Voltage => "V",
            SensorKind::Current => "A",
            SensorKind::Load => "%",
        }
    }

    /// Readings outside this range are sensor glitches or unconnected inputs
    fn plausible(self, v: f32) -> bool {
        v.is_finite()
            && match self {
                SensorKind::Temp => (-40.0..150.0).contains(&v) && v != 0.0,
                SensorKind::Fan => (0.0..30_000.0).contains(&v),
                SensorKind::Power => (0.0..5_000.0).contains(&v),
                SensorKind::Voltage => (0.0..60.0).contains(&v),
                SensorKind::Current => (0.0..500.0).contains(&v),
                SensorKind::Load => (0.0..=400.0).contains(&v),
            }
    }

    fn from_lhm(s: &str) -> Option<Self> {
        Some(match s {
            "Temperature" => SensorKind::Temp,
            "Fan" => SensorKind::Fan,
            "Power" => SensorKind::Power,
            "Voltage" => SensorKind::Voltage,
            "Current" => SensorKind::Current,
            _ => return None,
        })
    }
}

/// One reading of any other sensor the machine exposes: board / VRM / DIMM / drive temperatures,
/// fans, power draw (CPU package, memory, PSU), voltages and currents
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SensorReading {
    /// "nct6798: VRM MOS", "RAPL: package-0", "Corsair HX1000i: Total power" ...
    pub name: String,
    pub kind: SensorKind,
    pub value: f32,
}

/// Most extra readings kept per sample (a large board exposes about a hundred)
const MAX_EXTRA_SENSORS: usize = 256;

/// One test while the sampler ran: drawn as a coloured band on the timeline
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Phase {
    /// "memory", "cpu", "gpu", "input"
    pub kind: String,
    pub label: String,
    pub start_ms: u64,
    pub end_ms: u64,
}

/// Another program's share of the machine at one sample (processes with the same name added up)
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProcUsage {
    pub name: String,
    /// % of all CPU cores together (100 = every core busy)
    pub cpu_pct: f32,
    /// % of the busiest GPU engine type (3D, compute, copy, video) it used, like Task Manager (Windows)
    pub gpu_pct: f32,
}

/// Programs kept per sample (the busiest ones)
const MAX_PROCS: usize = 8;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    /// Milliseconds since the sampler started
    pub t_ms: u64,
    pub cpu_package_c: Option<f32>,
    /// (core number as the sensor reports it, temperature)
    pub core_temps_c: Vec<(usize, f32)>,
    /// Per logical CPU
    pub core_freq_mhz: Vec<f32>,
    pub core_usage_pct: Vec<f32>,
    pub ram_used_mb: f32,
    pub ram_total_mb: f32,
    pub gpu: Option<GpuSensors>,
    /// Every other sensor found (see [`SensorReading`])
    #[serde(default)]
    pub sensors: Vec<SensorReading>,
    /// The busiest other programs (this app and its sensor helper left out)
    #[serde(default)]
    pub procs: Vec<ProcUsage>,
    /// Whether programs were sampled at all (an empty `procs` then means nothing else was busy)
    #[serde(default)]
    pub procs_sampled: bool,
}

impl Snapshot {
    /// CPU package power: RAPL on Linux, the "CPU Package" power sensor of LibreHardwareMonitor on Windows
    pub fn cpu_package_power_w(&self) -> Option<f32> {
        self.sensors
            .iter()
            .find(|r| {
                let n = r.name.to_lowercase();
                r.kind == SensorKind::Power && n.contains("package") && !n.contains("gpu")
            })
            .map(|r| r.value)
    }
}

/// Summary of the samples taken during one test
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Telemetry {
    pub samples: usize,
    pub cpu_temp_avg_c: Option<f32>,
    pub cpu_temp_max_c: Option<f32>,
    /// Peak temperature per core during the test
    pub core_temp_max_c: Vec<(usize, f32)>,
    /// Average of every per-core reading during the test (all cores, all samples)
    #[serde(default)]
    pub core_temp_avg_c: Option<f32>,
    /// Average CPU package power during the test, when a power sensor exists
    #[serde(default)]
    pub cpu_power_avg_w: Option<f32>,
    pub cpu_freq_avg_mhz: Option<f32>,
    /// Average clock per logical CPU during the test
    pub core_freq_avg_mhz: Vec<f32>,
    pub cpu_usage_avg_pct: Option<f32>,
    pub ram_used_max_mb: Option<f32>,
    pub gpu_temp_max_c: Option<f32>,
    pub gpu_util_avg_pct: Option<f32>,
    pub vram_used_max_mb: Option<f32>,
    pub vram_total_mb: Option<f32>,
    pub gpu_power_max_w: Option<f32>,
    pub gpu_clock_avg_mhz: Option<f32>,
    /// CPU share of every other program together during the test, average % of the whole CPU
    #[serde(default)]
    pub others_cpu_avg_pct: Option<f32>,
    /// GPU share of every other program together, average % (Windows)
    #[serde(default)]
    pub others_gpu_avg_pct: Option<f32>,
    /// The busiest other programs during the test: (name, average CPU %, average GPU %)
    #[serde(default)]
    pub others_top: Vec<(String, f32, f32)>,
}

impl Telemetry {
    /// "chrome.exe 12 % CPU, obs64.exe 8 % GPU" (empty when nothing else was busy)
    pub fn others_text(&self) -> String {
        self.others_top
            .iter()
            .filter(|t| t.1 >= 0.5 || t.2 >= 0.5)
            .map(|(n, c, g)| {
                let mut parts = Vec::new();
                if *c >= 0.5 {
                    parts.push(format!("{:.0} % CPU", c));
                }
                if *g >= 0.5 {
                    parts.push(format!("{:.0} % GPU", g));
                }
                format!("{} {}", n, parts.join(" / "))
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Average use of every other program over `samples`: (name, avg CPU %, max CPU %, avg GPU %, max GPU %),
/// busiest first; None when programs were not sampled
pub fn program_usage(samples: &[Snapshot]) -> Option<Vec<(String, f32, f32, f32, f32)>> {
    let sampled: Vec<&Snapshot> = samples.iter().filter(|s| s.procs_sampled).collect();
    if sampled.is_empty() {
        return None;
    }
    let mut by: std::collections::BTreeMap<&str, (f32, f32, f32, f32)> = Default::default();
    for s in &sampled {
        for p in &s.procs {
            let e = by.entry(&p.name).or_default();
            e.0 += p.cpu_pct;
            e.1 = e.1.max(p.cpu_pct);
            e.2 += p.gpu_pct;
            e.3 = e.3.max(p.gpu_pct);
        }
    }
    let n = sampled.len() as f32;
    let mut v: Vec<(String, f32, f32, f32, f32)> = by.into_iter().map(|(k, e)| (k.to_string(), e.0 / n, e.1, e.2 / n, e.3)).collect();
    v.sort_by(|a, b| (b.1 + b.3).total_cmp(&(a.1 + a.3)));
    Some(v)
}

fn avg(v: impl Iterator<Item = f32>) -> Option<f32> {
    let (sum, n) = v.fold((0.0f32, 0usize), |(s, n), x| (s + x, n + 1));
    (n > 0).then(|| sum / n as f32)
}

fn max(v: impl Iterator<Item = f32>) -> Option<f32> {
    v.fold(None, |m: Option<f32>, x| Some(m.map_or(x, |m| m.max(x))))
}

impl Telemetry {
    /// (core, peak °C) of the core that got hottest
    pub fn hottest_core(&self) -> Option<(usize, f32)> {
        self.core_temp_max_c.iter().copied().fold(None, |m, c| match m {
            Some(x) if x.1 >= c.1 => Some(x),
            _ => Some(c),
        })
    }

    /// (core, peak °C) of the core that stayed coolest
    pub fn coolest_core(&self) -> Option<(usize, f32)> {
        self.core_temp_max_c.iter().copied().fold(None, |m, c| match m {
            Some(x) if x.1 <= c.1 => Some(x),
            _ => Some(c),
        })
    }
}

/// Reduce samples to one [`Telemetry`]
pub fn summarize(samples: &[Snapshot]) -> Telemetry {
    let mut t = Telemetry { samples: samples.len(), ..Default::default() };
    if samples.is_empty() {
        return t;
    }
    // "CPU temperature" = package sensor, or the hottest core when there is no package sensor
    let cpu_temp = |s: &Snapshot| -> Option<f32> {
        s.cpu_package_c.or_else(|| max(s.core_temps_c.iter().map(|c| c.1)))
    };
    t.cpu_temp_avg_c = avg(samples.iter().filter_map(cpu_temp));
    t.cpu_temp_max_c = max(samples.iter().flat_map(|s| {
        s.cpu_package_c.into_iter().chain(s.core_temps_c.iter().map(|c| c.1))
    }));

    let mut per_core: std::collections::BTreeMap<usize, f32> = Default::default();
    for s in samples {
        for &(core, c) in &s.core_temps_c {
            let e = per_core.entry(core).or_insert(c);
            *e = e.max(c);
        }
    }
    t.core_temp_max_c = per_core.into_iter().collect();
    t.core_temp_avg_c = avg(samples.iter().flat_map(|s| s.core_temps_c.iter().map(|c| c.1)));
    t.cpu_power_avg_w = avg(samples.iter().filter_map(|s| s.cpu_package_power_w()));

    t.cpu_freq_avg_mhz = avg(samples.iter().filter_map(|s| avg(s.core_freq_mhz.iter().copied().filter(|f| *f > 0.0))));
    let n_cpu = samples.iter().map(|s| s.core_freq_mhz.len()).max().unwrap_or(0);
    t.core_freq_avg_mhz = (0..n_cpu)
        .map(|i| avg(samples.iter().filter_map(|s| s.core_freq_mhz.get(i).copied())).unwrap_or(0.0))
        .collect();
    t.cpu_usage_avg_pct = avg(samples.iter().filter_map(|s| avg(s.core_usage_pct.iter().copied())));
    t.ram_used_max_mb = max(samples.iter().map(|s| s.ram_used_mb)).filter(|v| *v > 0.0);

    let gpus = || samples.iter().filter_map(|s| s.gpu.as_ref());
    t.gpu_temp_max_c = max(gpus().filter_map(|g| g.temp_c));
    t.gpu_util_avg_pct = avg(gpus().filter_map(|g| g.util_pct));
    t.vram_used_max_mb = max(gpus().filter_map(|g| g.vram_used_mb));
    t.vram_total_mb = max(gpus().filter_map(|g| g.vram_total_mb));
    t.gpu_power_max_w = max(gpus().filter_map(|g| g.power_w));
    t.gpu_clock_avg_mhz = avg(gpus().filter_map(|g| g.clock_mhz));
    if let Some(progs) = program_usage(samples) {
        t.others_cpu_avg_pct = Some(progs.iter().map(|p| p.1).sum());
        let gpu: f32 = progs.iter().map(|p| p.3).sum();
        t.others_gpu_avg_pct = samples.iter().any(|s| s.procs.iter().any(|p| p.gpu_pct > 0.0)).then_some(gpu);
        t.others_top = progs.iter().take(3).map(|p| (p.0.clone(), p.1, p.3)).collect();
    }
    t
}

// ---------------------------------------------------------------------------------------------
// Linux hwmon / drm
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct HwmonReading {
    pub package_c: Option<f32>,
    pub cores: Vec<(usize, f32)>,
    /// GPU exposed through hwmon (amdgpu): temperature only, VRAM comes from drm
    pub amdgpu_temp_c: Option<f32>,
}

fn read_trim(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

/// Read every `hwmonN` directory under `root` (normally `/sys/class/hwmon`)
pub fn read_hwmon(root: &Path) -> HwmonReading {
    let mut out = HwmonReading::default();
    let Ok(dirs) = std::fs::read_dir(root) else { return out };
    for dir in dirs.flatten() {
        let dir = dir.path();
        let name = read_trim(&dir.join("name")).unwrap_or_default();
        let Ok(files) = std::fs::read_dir(&dir) else { continue };
        for f in files.flatten() {
            let fname = f.file_name().to_string_lossy().to_string();
            let Some(idx) = fname.strip_prefix("temp").and_then(|r| r.strip_suffix("_input")) else { continue };
            let Some(milli) = read_trim(&f.path()).and_then(|v| v.parse::<f32>().ok()) else { continue };
            let c = milli / 1000.0;
            if !(0.0..150.0).contains(&c) {
                continue;
            }
            let label = read_trim(&dir.join(format!("temp{}_label", idx))).unwrap_or_default();
            match name.as_str() {
                "coretemp" => {
                    if label.starts_with("Package") {
                        out.package_c = Some(out.package_c.map_or(c, |p: f32| p.max(c)));
                    } else if let Some(n) = label.strip_prefix("Core ").and_then(|n| n.trim().parse().ok()) {
                        out.cores.push((n, c));
                    }
                }
                "k10temp" | "zenpower" => match label.as_str() {
                    "Tctl" | "Tdie" | "" => out.package_c = Some(out.package_c.map_or(c, |p: f32| p.max(c))),
                    l if l.starts_with("Tccd") => {
                        if let Some(n) = l[4..].parse::<usize>().ok() {
                            out.cores.push((n.saturating_sub(1), c));
                        }
                    }
                    _ => {}
                },
                "amdgpu" if idx == "1" => out.amdgpu_temp_c = Some(c),
                _ => {}
            }
        }
    }
    out.cores.sort_by_key(|c| c.0);
    out.cores.dedup_by_key(|c| c.0);
    out
}

/// Every temperature / fan / power / voltage / current input under `root` (normally `/sys/class/hwmon`):
/// board chips (VRM, chipset, system temps, fans, Vcore), DIMM sensors (spd5118, jc42), drives (nvme,
/// drivetemp), PSUs with a hwmon driver (corsair-psu, nzxt), ... CPU core / package temperatures are
/// reported separately and are skipped here.
pub fn read_hwmon_all(root: &Path) -> Vec<SensorReading> {
    let mut out = Vec::new();
    let Ok(dirs) = std::fs::read_dir(root) else { return out };
    let mut dirs: Vec<_> = dirs.flatten().map(|d| d.path()).collect();
    dirs.sort();
    for dir in dirs {
        let chip = read_trim(&dir.join("name")).unwrap_or_else(|| "hwmon".into());
        let Ok(files) = std::fs::read_dir(&dir) else { continue };
        let mut files: Vec<String> = files.flatten().map(|f| f.file_name().to_string_lossy().to_string()).collect();
        files.sort();
        for fname in files {
            // (prefix, suffix, kind, scale to the unit)
            let spec = [
                ("temp", "_input", SensorKind::Temp, 1e-3),
                ("fan", "_input", SensorKind::Fan, 1.0),
                ("power", "_input", SensorKind::Power, 1e-6),
                ("power", "_average", SensorKind::Power, 1e-6),
                ("in", "_input", SensorKind::Voltage, 1e-3),
                ("curr", "_input", SensorKind::Current, 1e-3),
            ];
            let Some((prefix, idx, kind, scale)) = spec.iter().find_map(|(p, suf, k, sc)| {
                fname.strip_prefix(p).and_then(|r| r.strip_suffix(suf)).filter(|i| i.chars().all(|c| c.is_ascii_digit()) && !i.is_empty()).map(|i| (*p, i.to_string(), *k, *sc))
            }) else { continue };
            if kind == SensorKind::Temp && matches!(chip.as_str(), "coretemp" | "k10temp" | "zenpower") {
                continue;
            }
            let Some(raw) = read_trim(&dir.join(&fname)).and_then(|v| v.parse::<f64>().ok()) else { continue };
            let value = (raw * scale) as f32;
            if !kind.plausible(value) {
                continue;
            }
            let label = read_trim(&dir.join(format!("{}{}_label", prefix, idx))).unwrap_or_else(|| format!("{}{}", prefix, idx));
            out.push(SensorReading { name: format!("{}: {}", chip, label), kind, value });
            if out.len() >= MAX_EXTRA_SENSORS {
                return out;
            }
        }
    }
    out
}

/// CPU package / core / DRAM power from the RAPL energy counters (`/sys/class/powercap/intel-rapl*`,
/// also used for AMD Zen). Power = energy difference between two samples / time. The counters are
/// root-only on many distributions; then nothing is reported.
#[derive(Default)]
struct Rapl {
    last: std::collections::HashMap<std::path::PathBuf, (Instant, u64)>,
}

impl Rapl {
    fn read(&mut self, root: &Path) -> Vec<SensorReading> {
        let mut out = Vec::new();
        let Ok(dirs) = std::fs::read_dir(root) else { return out };
        let now = Instant::now();
        let mut dirs: Vec<_> = dirs.flatten().map(|d| d.path()).filter(|p| p.to_string_lossy().contains("rapl:")).collect();
        dirs.sort();
        for dir in dirs {
            let Some(energy) = read_trim(&dir.join("energy_uj")).and_then(|v| v.parse::<u64>().ok()) else { continue };
            let name = read_trim(&dir.join("name")).unwrap_or_else(|| "domain".into());
            let range = read_trim(&dir.join("max_energy_range_uj")).and_then(|v| v.parse::<u64>().ok()).unwrap_or(u64::MAX);
            if let Some((t, e)) = self.last.insert(dir.clone(), (now, energy)) {
                let dt = now.duration_since(t).as_secs_f64();
                let de = if energy >= e { energy - e } else { range.saturating_sub(e) + energy }; // counter wrapped
                if dt > 0.05 {
                    let w = (de as f64 / 1e6 / dt) as f32;
                    if SensorKind::Power.plausible(w) {
                        out.push(SensorReading { name: format!("RAPL: {}", name), kind: SensorKind::Power, value: w });
                    }
                }
            }
        }
        out
    }
}

/// AMD VRAM (used, total) in MB from `/sys/class/drm/card*/device/mem_info_vram_*`
pub fn read_amd_vram(drm_root: &Path) -> Option<(f32, f32)> {
    for card in std::fs::read_dir(drm_root).ok()?.flatten() {
        let dev = card.path().join("device");
        let used = read_trim(&dev.join("mem_info_vram_used")).and_then(|v| v.parse::<f64>().ok());
        let total = read_trim(&dev.join("mem_info_vram_total")).and_then(|v| v.parse::<f64>().ok());
        if let (Some(u), Some(t)) = (used, total) {
            return Some(((u / 1048576.0) as f32, (t / 1048576.0) as f32));
        }
    }
    None
}

/// AMD GPU load in percent from `/sys/class/drm/card*/device/gpu_busy_percent`
pub fn read_amd_busy(drm_root: &Path) -> Option<f32> {
    for card in std::fs::read_dir(drm_root).ok()?.flatten() {
        if let Some(v) = read_trim(&card.path().join("device").join("gpu_busy_percent")).and_then(|v| v.parse::<f32>().ok()) {
            return Some(v);
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// nvidia-smi
// ---------------------------------------------------------------------------------------------

/// Parse one line of
/// `nvidia-smi --query-gpu=name,temperature.gpu,utilization.gpu,memory.used,memory.total,power.draw,clocks.gr --format=csv,noheader,nounits`
pub fn parse_nvidia_smi(out: &str) -> Option<GpuSensors> {
    let line = out.lines().find(|l| !l.trim().is_empty())?;
    let f: Vec<&str> = line.split(',').map(str::trim).collect();
    if f.len() < 7 {
        return None;
    }
    let num = |s: &str| s.parse::<f32>().ok(); // "[N/A]" / "N/A" -> None
    Some(GpuSensors {
        name: f[0].to_string(),
        temp_c: num(f[1]),
        util_pct: num(f[2]),
        vram_used_mb: num(f[3]),
        vram_total_mb: num(f[4]),
        power_w: num(f[5]),
        clock_mhz: num(f[6]),
    })
}

pub(crate) fn hidden_command(program: &str) -> std::process::Command {
    #[allow(unused_mut)]
    let mut c = std::process::Command::new(program);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c
}

fn query_nvidia_smi() -> Option<GpuSensors> {
    let out = hidden_command("nvidia-smi")
        .args([
            "--query-gpu=name,temperature.gpu,utilization.gpu,memory.used,memory.total,power.draw,clocks.gr",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_nvidia_smi(&String::from_utf8_lossy(&out.stdout))
}

// ---------------------------------------------------------------------------------------------
// Windows sensor stream (LibreHardwareMonitor / OpenHardwareMonitor / thermal zones)
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct WinSensors {
    pub package_c: Option<f32>,
    pub cores: Vec<(usize, f32)>,
    pub gpu_c: Option<f32>,
    /// Actual per-logical-CPU clock in MHz ("% Processor Performance" x base clock, like Task Manager)
    pub core_freq_mhz: Vec<f32>,
    /// "LibreHardwareMonitor", "OpenHardwareMonitor" or "ACPI thermal zone"
    pub source: String,
    /// Every ACPI thermal zone in °C (only filled for the ACPI fallback). Many boards have zones
    /// that never change, so the hottest one is not always the useful one, see [`ZonePicker`].
    pub zones_c: Vec<f32>,
    /// Every LibreHardwareMonitor temperature, fan, power, voltage and current sensor
    pub extra: Vec<SensorReading>,
    /// LibreHardwareMonitor's library folder was passed to the helper
    pub lhm_present: bool,
    /// Why the library could not be loaded or opened
    pub lhm_error: Option<String>,
    /// Installed PawnIO driver version (LibreHardwareMonitor ≥ 0.9.5 reads CPU / board / memory through it)
    pub pawnio: Option<String>,
    /// P and E cores LibreHardwareMonitor names in its temperatures ("P-Core #n", "E-Core #n")
    pub lhm_pe: Option<(usize, usize)>,
    /// GPU use per process id, % of its busiest engine type ("GPU Engine" counters)
    pub gpu_procs: Vec<(u32, f32)>,
}

const ACPI_SOURCE: &str = "ACPI thermal zone";

/// Parse one JSON line printed by [`WIN_STREAM_SCRIPT`] (per-core names placed by their number alone)
#[cfg(test)]
pub fn parse_win_sensor_line(line: &str) -> Option<WinSensors> {
    parse_win_sensor_line_with(line, &crate::topology::CoreMap::default())
}

/// Parse one JSON line; `map` places per-core sensors ("P-Core #2", "E-Core #5") on logical CPUs
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn parse_win_sensor_line_with(line: &str, map: &crate::topology::CoreMap) -> Option<WinSensors> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let as_list = |v: &serde_json::Value| -> Vec<serde_json::Value> {
        match v {
            serde_json::Value::Array(a) => a.clone(),
            serde_json::Value::Null => Vec::new(),
            other => vec![other.clone()],
        }
    };
    let mut w = WinSensors::default();
    for t in as_list(&v["t"]) {
        let (Some(name), Some(val)) = (t["n"].as_str(), t["v"].as_f64()) else { continue };
        let val = val as f32;
        if !(0.0..150.0).contains(&val) {
            continue;
        }
        let lower = name.to_lowercase();
        if let Some(cpus) = map.logical_for(name) {
            w.cores.extend(cpus.into_iter().map(|c| (c, val)));
        } else if lower.contains("cpu package") || lower.contains("tctl") || lower.contains("core (tdie)") || lower == "core average" {
            w.package_c = Some(w.package_c.map_or(val, |p| p.max(val)));
        } else if lower == "gpu core" || lower.starts_with("gpu core") {
            w.gpu_c = Some(val);
        }
    }
    w.cores.sort_by_key(|c| c.0);
    w.cores.dedup_by_key(|c| c.0);
    let names: Vec<String> = as_list(&v["t"]).iter().filter_map(|t| t["n"].as_str().map(str::to_lowercase)).collect();
    let count = |prefix: &str| names.iter().filter(|n| n.strip_prefix(prefix).map_or(false, |r| r.trim().parse::<usize>().is_ok())).count();
    let (pc, ec) = (count("p-core #"), count("e-core #"));
    if pc + ec > 0 {
        w.lhm_pe = Some((pc, ec));
    }
    // Actual clocks: every instance is "group,cpu"; % Processor Performance is relative to the base clock
    let base = v["base"].as_f64().unwrap_or(0.0);
    if base > 0.0 {
        let mut freqs: Vec<(usize, f32)> = Vec::new();
        for p in as_list(&v["perf"]) {
            let (Some(name), Some(pct)) = (p["n"].as_str(), p["v"].as_f64()) else { continue };
            let mut it = name.split(',');
            let (Some(g), Some(c)) = (it.next().and_then(|x| x.trim().parse::<usize>().ok()), it.next().and_then(|x| x.trim().parse::<usize>().ok())) else { continue };
            if pct > 0.0 && pct < 1000.0 {
                freqs.push((g * 64 + c, (pct * base / 100.0) as f32));
            }
        }
        freqs.sort_by_key(|f| f.0);
        w.core_freq_mhz = freqs.into_iter().map(|f| f.1).collect();
    }
    // LibreHardwareMonitor's own per-core clocks are better than the performance counter
    let mut lhm_clocks: Vec<(usize, f32)> = Vec::new();
    for c in as_list(&v["cl"]) {
        let (Some(name), Some(mhz)) = (c["n"].as_str(), c["v"].as_f64()) else { continue };
        if let Some(cpus) = map.logical_for(name).filter(|_| mhz > 100.0 && mhz < 10_000.0) {
            lhm_clocks.extend(cpus.into_iter().map(|cpu| (cpu, mhz as f32)));
        }
    }
    if let Some(n) = lhm_clocks.iter().map(|c| c.0 + 1).max() {
        let mut f = vec![0.0f32; n.max(w.core_freq_mhz.len())];
        f[..w.core_freq_mhz.len()].copy_from_slice(&w.core_freq_mhz);
        for (cpu, mhz) in lhm_clocks {
            f[cpu] = mhz;
        }
        w.core_freq_mhz = f;
    }
    // GPU Engine counters: one per process and engine; Task Manager's figure is the busiest engine type
    let mut per: std::collections::HashMap<(u32, String), f32> = Default::default();
    for g in as_list(&v["gp"]) {
        let (Some(pid), Some(e), Some(val)) = (g["p"].as_u64(), g["e"].as_str(), g["v"].as_f64()) else { continue };
        *per.entry((pid as u32, e.to_string())).or_default() += val as f32;
    }
    let mut by_pid: std::collections::HashMap<u32, f32> = Default::default();
    for ((pid, _), v) in per {
        let e = by_pid.entry(pid).or_default();
        *e = e.max(v.min(100.0));
    }
    w.gpu_procs = by_pid.into_iter().collect();
    w.gpu_procs.sort_by_key(|p| p.0);
    w.lhm_present = v["lhm"].as_bool().unwrap_or(false);
    w.lhm_error = v["lerr"].as_str().map(str::trim).filter(|e| !e.is_empty()).map(String::from);
    w.pawnio = v["pawn"].as_str().map(str::trim).filter(|e| !e.is_empty()).map(String::from);
    if let Some(src) = v["src"].as_str().filter(|s| !s.is_empty()) {
        w.source = if src == "lib" {
            "LibreHardwareMonitor library"
        } else if src.contains("Libre") {
            "LibreHardwareMonitor"
        } else {
            "OpenHardwareMonitor"
        }
        .to_string();
    }
    for x in as_list(&v["x"]) {
        let (Some(name), Some(kind), Some(val)) = (x["n"].as_str(), x["k"].as_str().and_then(SensorKind::from_lhm), x["v"].as_f64()) else { continue };
        let val = val as f32;
        if kind.plausible(val) && w.extra.len() < MAX_EXTRA_SENSORS {
            w.extra.push(SensorReading { name: name.to_string(), kind, value: val });
        }
    }
    if w.package_c.is_none() && w.cores.is_empty() {
        // Thermal-zone counters are Kelvin ("Temperature": whole K, "High Precision Temperature":
        // tenths of K, so slow changes are not hidden by 1 K steps)
        let zones = |key: &str, div: f64| -> Vec<f32> {
            as_list(&v[key])
                .iter()
                .filter_map(|k| k.as_f64())
                .map(|k| k / div)
                .map(|k| if k > 200.0 { k - 273.15 } else { k } as f32)
                .filter(|c| (1.0..150.0).contains(c))
                .collect()
        };
        let mut z = zones("tzh", 10.0);
        if z.is_empty() {
            z = zones("tz", 1.0);
        }
        if let Some(hottest) = max(z.iter().copied()) {
            w.package_c = Some(hottest);
            w.zones_c = z;
            w.source = ACPI_SOURCE.to_string();
        }
    }
    (w.package_c.is_some() || !w.cores.is_empty() || w.gpu_c.is_some() || !w.core_freq_mhz.is_empty() || !w.extra.is_empty() || w.lhm_error.is_some())
        .then_some(w)
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
const WIN_STREAM_SCRIPT: &str = r#"
$ErrorActionPreference = 'SilentlyContinue'
$base = (Get-CimInstance -ClassName Win32_Processor | Select-Object -First 1).MaxClockSpeed
$hp = '\Thermal Zone Information(*)\High Precision Temperature'
$tzc = '\Thermal Zone Information(*)\Temperature'
$pf = '\Processor Information(*)\% Processor Performance'
# GPU use per process and engine (instances "pid_1234_luid_..._engtype_3D")
$ge = '\GPU Engine(*)\Utilization Percentage'
# A counter set that does not exist on this PC (no ACPI zones, older Windows) fails the whole call,
# so fall back to smaller sets instead of losing the clocks as well
$sets = @(@($hp, $tzc, $pf, $ge), @($tzc, $pf, $ge), @($pf, $ge), @($hp, $tzc, $pf), @($tzc, $pf), @($pf))
# 1) LibreHardwareMonitor's own library next to the exe ($env:LTS_LHM_DIR): no separate app needed.
#    Since v0.9.5 it reads CPU (MSR), board (Super I/O: VRM, fans, voltages) and memory (SPD) sensors
#    through the PawnIO driver, which LibreHardwareMonitor.exe installs on its first start. Without
#    PawnIO, or without administrator rights, only GPU, drive and similar sensors appear.
$computer = $null; $lerr = ''
$pawn = ''
foreach ($k in 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\PawnIO', 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\PawnIO') {
  if (-not $pawn) { $pawn = [string](Get-ItemProperty $k -ErrorAction SilentlyContinue).DisplayVersion }
}
$lhm = $env:LTS_LHM_DIR
if ($lhm -and (Test-Path (Join-Path $lhm 'LibreHardwareMonitorLib.dll'))) {
  try {
    $ErrorActionPreference = 'Stop'
    if (Get-ChildItem $lhm -Filter '*.runtimeconfig.json') {
      throw 'this is the .NET 10 build of LibreHardwareMonitor, which Windows PowerShell cannot load: press Update LibreHardwareMonitor on the Dashboard to get the .NET Framework build (LibreHardwareMonitor.zip)'
    }
    # files from a browser download carry a "downloaded from the internet" mark that blocks loading
    Get-ChildItem $lhm -Recurse -File | ForEach-Object { try { Unblock-File $_.FullName } catch {} }
    # the library's dependencies may want other versions than the ones next to it (the exe has
    # binding redirects in its .config, PowerShell does not): hand over whatever is there
    $resolve = [ResolveEventHandler]{
      param($sender, $e)
      $name = (New-Object Reflection.AssemblyName($e.Name)).Name
      foreach ($a in [AppDomain]::CurrentDomain.GetAssemblies()) { if ($a.GetName().Name -eq $name) { return $a } }
      $p = Join-Path $env:LTS_LHM_DIR ($name + '.dll')
      if (Test-Path $p) { return [Reflection.Assembly]::UnsafeLoadFrom($p) }
      return $null
    }
    [AppDomain]::CurrentDomain.add_AssemblyResolve($resolve)
    [void][Reflection.Assembly]::UnsafeLoadFrom((Join-Path $lhm 'LibreHardwareMonitorLib.dll'))
    $computer = New-Object LibreHardwareMonitor.Hardware.Computer
    foreach ($p in 'IsCpuEnabled','IsGpuEnabled','IsMemoryEnabled','IsMotherboardEnabled','IsControllerEnabled','IsPsuEnabled','IsStorageEnabled','IsBatteryEnabled') {
      try { $computer.$p = $true } catch {}
    }
    $computer.Open()
  } catch {
    $lerr = [string]$_.Exception.GetBaseException().Message
    if (-not $lerr) { $lerr = [string]$_ }
    $computer = $null
  }
  $ErrorActionPreference = 'SilentlyContinue'
}
function Read-Lhm($hw, $out) {
  try { $hw.Update() } catch {}
  foreach ($sub in $hw.SubHardware) { Read-Lhm $sub $out }
  foreach ($s in $hw.Sensors) {
    if ($s.Value -ne $null) { $out.Add(@{ h = [string]$hw.Name; ht = [string]$hw.HardwareType; n = [string]$s.Name; k = [string]$s.SensorType; v = [double]$s.Value }) }
  }
}
while ($true) {
  $ns = $null; $all = $null; $s = $null; $x = @(); $t = @(); $cl = @(); $cpuT = $false
  if ($computer) {
    $list = New-Object System.Collections.ArrayList
    foreach ($hw in $computer.Hardware) { Read-Lhm $hw $list }
    $ns = 'lib'
    $t = @($list | Where-Object { $_.k -eq 'Temperature' -and ($_.ht -eq 'Cpu' -or $_.ht -like 'Gpu*') } | ForEach-Object { @{ n = $_.n; v = $_.v } })
    # per-core clocks ("P-Core #1", "E-Core #3", "CPU Core #2")
    $cl = @($list | Where-Object { $_.k -eq 'Clock' -and $_.ht -eq 'Cpu' } | ForEach-Object { @{ n = $_.n; v = $_.v } })
    $x = @($list | Where-Object { 'Temperature','Fan','Power','Voltage','Current' -contains $_.k } | ForEach-Object { @{ n = "$($_.h): $($_.n)"; k = $_.k; v = $_.v } })
    $cpuT = [bool]($list | Where-Object { $_.k -eq 'Temperature' -and $_.ht -eq 'Cpu' })
  }
  if (-not $cpuT) {
  # 2) the LibreHardwareMonitor / OpenHardwareMonitor app, if it runs, through WMI
  foreach ($n in 'root/LibreHardwareMonitor', 'root/OpenHardwareMonitor') {
    $all = Get-CimInstance -Namespace $n -ClassName Sensor
    if ($all) { $ns = $n; break }
  }
  }
  if ($all) {
    $s = $all | Where-Object { $_.SensorType -eq 'Temperature' }
    $cl = @($all | Where-Object { $_.SensorType -eq 'Clock' -and [string]$_.Parent -like '*cpu*' } | ForEach-Object { @{ n = [string]$_.Name; v = [double]$_.Value } })
    # every sensor with its hardware's name: "Nuvoton NCT6798D: VRM MOS", "DIMM #1: Temperature", "Corsair HX1000i: Total"
    $hw = @{}; Get-CimInstance -Namespace $ns -ClassName Hardware | ForEach-Object { $hw[[string]$_.Identifier] = $_.Name }
    $x = @($all | Where-Object { 'Temperature','Fan','Power','Voltage','Current' -contains $_.SensorType } | ForEach-Object {
      $h = $hw[[string]$_.Parent]; if (-not $h) { $h = [string]$_.Parent }
      @{ n = "$($h): $($_.Name)"; k = [string]$_.SensorType; v = [double]$_.Value } })
  }
  if ($s) { $t = @($s | ForEach-Object { @{ n = $_.Name; v = [double]$_.Value } }) }
  $tz = @(); $tzh = @(); $perf = @(); $gp = @(); $samples = $null
  foreach ($set in $sets) {
    try { $samples = (Get-Counter -Counter $set -ErrorAction Stop).CounterSamples; break } catch {}
  }
  if ($samples) {
    $tzh = @($samples | Where-Object { $_.Path -like '*\high precision temperature' } | ForEach-Object { [double]$_.CookedValue })
    $tz = @($samples | Where-Object { $_.Path -like '*thermal zone*' -and $_.Path -like '*\temperature' } | ForEach-Object { [double]$_.CookedValue })
    $perf = @($samples | Where-Object { $_.Path -like '*processor performance*' -and $_.InstanceName -notmatch '_total' } | ForEach-Object { @{ n = $_.InstanceName; v = [double]$_.CookedValue } })
    $gp = @($samples | Where-Object { $_.Path -like '*gpu engine*' -and $_.CookedValue -gt 0.3 -and $_.InstanceName -match 'pid_(\d+)_.*engtype_(\w+)' } | ForEach-Object {
      if ($_.InstanceName -match 'pid_(\d+)_.*engtype_(\w+)') { @{ p = [int]$matches[1]; e = $matches[2]; v = [double]$_.CookedValue } } })
  }
  [pscustomobject]@{ src = $ns; t = $t; x = $x; cl = $cl; tz = $tz; tzh = $tzh; perf = $perf; gp = $gp; base = $base; lhm = [bool]$lhm; lerr = $lerr; pawn = $pawn } | ConvertTo-Json -Compress -Depth 4
  Start-Sleep -Milliseconds 500
}
"#;

/// What is missing for LibreHardwareMonitor to deliver CPU / board / memory sensors, and whether it
/// agrees with the suite about which cores are P and E cores
fn lhm_notes(w: &WinSensors, admin: bool) -> Vec<String> {
    let mut n = Vec::new();
    if let Some((lp, le)) = w.lhm_pe {
        let (sp, se) = match crate::topology::cached_core_kinds() {
            Some(k) => (
                k.iter().filter(|&&c| c == crate::topology::CoreKind::Performance).count(),
                k.iter().filter(|&&c| c == crate::topology::CoreKind::Efficiency).count(),
            ),
            None => (0, 0),
        };
        let groups = crate::app_core::sibling_groups();
        // LibreHardwareMonitor counts physical cores; with Hyper-Threading a P core has two threads
        let phys = |want| {
            groups.iter().filter(|g| crate::topology::cached_core_kinds().and_then(|k| k.get(g[0]).copied()) == Some(want)).count()
        };
        let (pp, pe) = (phys(crate::topology::CoreKind::Performance), phys(crate::topology::CoreKind::Efficiency));
        if sp + se == 0 {
            n.push(format!("LibreHardwareMonitor sees {} P-cores and {} E-cores, but the suite could not tell them apart on this PC", lp, le));
        } else if (pp, pe) != (lp, le) {
            n.push(format!("Core types differ: LibreHardwareMonitor sees {} P + {} E cores, the suite {} P + {} E", lp, le, pp, pe));
        } else {
            n.push(format!("Core types: {} P-cores and {} E-cores (the suite and LibreHardwareMonitor agree)", pp, pe));
        }
    }
    let cpu_ok = !w.cores.is_empty();
    if let Some(e) = &w.lhm_error {
        n.push(format!("LibreHardwareMonitor library could not be loaded: {}", e));
    } else if !w.lhm_present && !cpu_ok {
        n.push("Per-core CPU, board, VRM, fan, memory and PSU sensors: press Download LibreHardwareMonitor on the Dashboard (or run the LibreHardwareMonitor app)".into());
    }
    if w.lhm_present && w.lhm_error.is_none() && !cpu_ok {
        if w.pawnio.is_none() {
            n.push("The PawnIO driver is not installed: LibreHardwareMonitor 0.9.5 and later read CPU, motherboard (VRM, fans, voltages) and memory sensors through it. Press Install PawnIO on the Dashboard, or start LibreHardwareMonitor.exe once and accept its PawnIO prompt.".into());
        } else if !admin {
            n.push("LibreHardwareMonitor needs administrator rights for CPU, board and memory sensors: restart the app as administrator".into());
        } else {
            n.push(format!(
                "LibreHardwareMonitor is loaded (PawnIO {}) but reports no CPU temperature for this CPU; a newer LibreHardwareMonitor may add it",
                w.pawnio.as_deref().unwrap_or("?")
            ));
        }
    }
    n
}

/// Physical cores of this PC (detected once: it briefly pins a thread to every logical CPU)
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn core_map() -> &'static crate::topology::CoreMap {
    static MAP: std::sync::OnceLock<crate::topology::CoreMap> = std::sync::OnceLock::new();
    MAP.get_or_init(crate::topology::CoreMap::detect)
}

/// What the PowerShell reader thread has seen so far
#[derive(Default)]
struct WinShared {
    reading: Option<WinSensors>,
    /// When `reading` was parsed
    reading_at: Option<Instant>,
    /// When the child last printed anything at all (even a line without usable sensors)
    line_at: Option<Instant>,
}

impl WinShared {
    /// The last reading, unless it is older than `max_age`. A hung or dead helper must not keep
    /// feeding its last value into every new snapshot, that is what a "stuck" temperature looks like.
    fn fresh(&self, now: Instant, max_age: Duration) -> Option<&WinSensors> {
        let at = self.reading_at?;
        if now.saturating_duration_since(at) <= max_age {
            self.reading.as_ref()
        } else {
            None
        }
    }
}

/// Readings older than this are dropped (the helper prints one about every 1.5 s)
const WIN_STALE: Duration = Duration::from_secs(6);
/// The helper is restarted after this long without any output, or as soon as it exits
const WIN_HUNG: Duration = Duration::from_secs(12);

/// Bumped to make every Windows sensor helper start over (new LibreHardwareMonitor or driver)
static WIN_HELPER_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Restart the Windows sensor helpers so they load LibreHardwareMonitor / PawnIO again
pub fn restart_windows_helpers() {
    WIN_HELPER_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Long-running PowerShell child that prints one JSON line of sensor data every second or two
struct WinStream {
    shared: Arc<Mutex<WinShared>>,
    child: Option<std::process::Child>,
    spawned: Instant,
    generation: u64,
}

impl WinStream {
    fn start() -> Self {
        let shared = Arc::new(Mutex::new(WinShared::default()));
        let generation = WIN_HELPER_GENERATION.load(std::sync::atomic::Ordering::Relaxed);
        let child = Self::spawn(&shared);
        Self { shared, child, spawned: Instant::now(), generation }
    }

    #[cfg(target_os = "windows")]
    fn spawn(shared: &Arc<Mutex<WinShared>>) -> Option<std::process::Child> {
        use std::io::{BufRead, BufReader};
        use std::process::Stdio;
        let mut child = hidden_command("powershell")
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", WIN_STREAM_SCRIPT])
            .env("LTS_LHM_DIR", crate::lhm::dir().map(|d| d.to_string_lossy().to_string()).unwrap_or_default())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .stdin(Stdio::null())
            .spawn()
            .ok()?;
        if let Some(stdout) = child.stdout.take() {
            let shared = shared.clone();
            std::thread::spawn(move || {
                let map = core_map();
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    let now = Instant::now();
                    let parsed = parse_win_sensor_line_with(&line, map);
                    if let Ok(mut sh) = shared.lock() {
                        sh.line_at = Some(now);
                        if let Some(w) = parsed {
                            sh.reading = Some(w);
                            sh.reading_at = Some(now);
                        }
                    }
                }
            });
        }
        Some(child)
    }

    #[cfg(not(target_os = "windows"))]
    fn spawn(_shared: &Arc<Mutex<WinShared>>) -> Option<std::process::Child> {
        None
    }

    /// Pin the helper process to `core` (None = every core)
    #[cfg(target_os = "windows")]
    fn set_affinity(&self, core: Option<usize>) {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessAffinityMask, SetProcessAffinityMask};
        let Some(child) = self.child.as_ref() else { return };
        unsafe {
            let mask = match core {
                Some(c) if c < 64 => 1usize << c,
                _ => {
                    let (mut p, mut s) = (0usize, 0usize);
                    if GetProcessAffinityMask(GetCurrentProcess(), &mut p, &mut s).is_err() || p == 0 {
                        return;
                    }
                    p
                }
            };
            let _ = SetProcessAffinityMask(HANDLE(child.as_raw_handle()), mask);
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn set_affinity(&self, _core: Option<usize>) {}

    /// The latest reading if it is recent enough to still be true
    fn fresh(&self) -> Option<WinSensors> {
        self.shared.lock().ok()?.fresh(Instant::now(), WIN_STALE).cloned()
    }

    /// Has the helper ever delivered a usable reading?
    fn ever_delivered(&self) -> bool {
        self.shared.lock().map_or(false, |s| s.reading_at.is_some())
    }

    /// Seconds since the helper was (re)started
    fn age_s(&self) -> f32 {
        self.spawned.elapsed().as_secs_f32()
    }

    /// Restart the helper if it exited or went silent (a hung `Get-Counter`, a killed process)
    fn revive(&mut self) {
        let exited = match self.child.as_mut() {
            Some(c) => !matches!(c.try_wait(), Ok(None)),
            None => true,
        };
        let last_output = self.shared.lock().ok().and_then(|s| s.line_at).unwrap_or(self.spawned);
        let silent = last_output.elapsed() > WIN_HUNG;
        let generation = WIN_HELPER_GENERATION.load(std::sync::atomic::Ordering::Relaxed);
        let asked = generation != self.generation;
        if asked || ((exited || silent) && self.spawned.elapsed() > Duration::from_secs(5)) {
            if let Some(mut c) = self.child.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
            if asked {
                // readings from the old helper describe the old setup
                if let Ok(mut sh) = self.shared.lock() {
                    *sh = WinShared::default();
                }
            }
            self.child = Self::spawn(&self.shared);
            self.spawned = Instant::now();
            self.generation = generation;
        }
    }
}

impl Drop for WinStream {
    fn drop(&mut self) {
        if let Some(c) = self.child.as_mut() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Chooses which ACPI thermal zone stands in for the CPU temperature. Boards commonly expose zones
/// that are fixed numbers (e.g. 27.8 °C) next to one that moves, and the hottest zone can be a fixed one,
/// so prefer the zone that has actually changed recently and only fall back to the hottest.
#[derive(Default)]
struct ZonePicker {
    history: Vec<std::collections::VecDeque<f32>>,
}

impl ZonePicker {
    const KEEP: usize = 60;
    /// Smaller changes than this are treated as no movement
    const MOVED: f32 = 0.05;

    fn pick(&mut self, zones: &[f32]) -> Option<f32> {
        if zones.is_empty() {
            self.history.clear();
            return None;
        }
        if self.history.len() != zones.len() {
            self.history = vec![Default::default(); zones.len()];
        }
        for (h, &z) in self.history.iter_mut().zip(zones) {
            if h.len() >= Self::KEEP {
                h.pop_front();
            }
            h.push_back(z);
        }
        let range = |h: &std::collections::VecDeque<f32>| {
            let lo = h.iter().copied().fold(f32::MAX, f32::min);
            let hi = h.iter().copied().fold(f32::MIN, f32::max);
            hi - lo
        };
        let mut best: Option<(f32, f32)> = None; // (range, current temperature)
        for (h, &z) in self.history.iter().zip(zones) {
            let r = range(h);
            if r > Self::MOVED && best.map_or(true, |(br, bt)| r > br || (r == br && z > bt)) {
                best = Some((r, z));
            }
        }
        best.map(|b| b.1).or_else(|| max(zones.iter().copied()))
    }
}

/// Spots a temperature series that never moves while the CPU is busy. On a working sensor a loaded CPU
/// changes by at least a degree within seconds, so a dead-flat line is almost always a fixed
/// value (a static ACPI zone, a stuck helper) rather than the real temperature.
#[derive(Default)]
struct FlatlineWatch {
    /// (t_ms, temperature, busiest core %)
    samples: std::collections::VecDeque<(u64, f32, f32)>,
}

impl FlatlineWatch {
    const WINDOW_MS: u64 = 20_000;
    const MIN_SPAN_MS: u64 = 15_000;
    const MIN_SAMPLES: usize = 8;
    const MIN_LOAD_PCT: f32 = 50.0;
    const MOVED: f32 = 0.05;

    fn push(&mut self, t_ms: u64, temp: Option<f32>, busiest_core_pct: f32) {
        let Some(temp) = temp else {
            self.samples.clear();
            return;
        };
        self.samples.push_back((t_ms, temp, busiest_core_pct));
        while self.samples.front().map_or(false, |f| t_ms.saturating_sub(f.0) > Self::WINDOW_MS) {
            self.samples.pop_front();
        }
    }

    /// Seconds the value has been dead flat under load, once that is long enough to matter
    fn flat_for_s(&self) -> Option<u64> {
        let (first, last) = (self.samples.front()?, self.samples.back()?);
        let span = last.0.saturating_sub(first.0);
        if span < Self::MIN_SPAN_MS || self.samples.len() < Self::MIN_SAMPLES {
            return None;
        }
        let lo = self.samples.iter().map(|s| s.1).fold(f32::MAX, f32::min);
        let hi = self.samples.iter().map(|s| s.1).fold(f32::MIN, f32::max);
        let load = self.samples.iter().map(|s| s.2).sum::<f32>() / self.samples.len() as f32;
        (hi - lo <= Self::MOVED && load >= Self::MIN_LOAD_PCT).then_some(span / 1000)
    }
}

// ---------------------------------------------------------------------------------------------
// Sampler
// ---------------------------------------------------------------------------------------------

const MAX_SAMPLES: usize = 40_000;

/// Records [`Snapshot`]s on a background thread until dropped or stopped
pub struct Sampler {
    start: Instant,
    samples: Arc<Mutex<Vec<Snapshot>>>,
    notes: Arc<Mutex<Vec<String>>>,
    phases: Arc<Mutex<Vec<Phase>>>,
    stop: Arc<AtomicBool>,
}

impl Drop for Sampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Sampler {
    pub fn start(interval: Duration) -> Arc<Sampler> {
        let s = Arc::new(Sampler {
            start: Instant::now(),
            samples: Arc::new(Mutex::new(Vec::new())),
            notes: Arc::new(Mutex::new(Vec::new())),
            phases: Arc::new(Mutex::new(Vec::new())),
            stop: Arc::new(AtomicBool::new(false)),
        });
        let (samples, notes, stop, start) = (s.samples.clone(), s.notes.clone(), s.stop.clone(), s.start);
        std::thread::spawn(move || {
            let mut collector = Collector::new();
            while !stop.load(Ordering::Relaxed) {
                // stay on the app's reserved core, away from the core being measured
                crate::app_core::apply_helper();
                collector.follow_app_core();
                let began = Instant::now();
                let mut snap = collector.collect();
                snap.t_ms = start.elapsed().as_millis() as u64;
                if let Ok(mut n) = notes.lock() {
                    if *n != collector.notes {
                        *n = collector.notes.clone();
                    }
                }
                if let Ok(mut v) = samples.lock() {
                    if v.len() >= MAX_SAMPLES {
                        // keep the whole run at half the resolution instead of forgetting its start
                        let mut i = 0;
                        v.retain(|_| { i += 1; i % 2 == 0 });
                    }
                    v.push(snap);
                }
                while began.elapsed() < interval && !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
        });
        s
    }

    /// Stop sampling; the samples taken so far stay available
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    /// Summary of the samples in `[from_ms, to_ms]`; if none fall inside (a very short test) the
    /// closest earlier sample is used, else the next one.
    pub fn window(&self, from_ms: u64, to_ms: u64) -> Telemetry {
        let v = self.samples.lock().unwrap();
        let inside: Vec<Snapshot> = v.iter().filter(|s| s.t_ms >= from_ms && s.t_ms <= to_ms).cloned().collect();
        if !inside.is_empty() {
            return summarize(&inside);
        }
        let nearest = v.iter().rev().find(|s| s.t_ms <= to_ms).or_else(|| v.iter().find(|s| s.t_ms > to_ms));
        nearest.map(|s| summarize(std::slice::from_ref(s))).unwrap_or_default()
    }

    /// Record that a test of `kind` ("memory", "cpu", "gpu", "input") ran in `[from_ms, to_ms]` and
    /// return the summary of the samples taken meanwhile
    pub fn record(&self, kind: &str, label: String, from_ms: u64, to_ms: u64) -> Telemetry {
        if let Ok(mut p) = self.phases.lock() {
            p.push(Phase { kind: kind.to_string(), label, start_ms: from_ms, end_ms: to_ms });
        }
        self.window(from_ms, to_ms)
    }

    /// Every test recorded so far, in order
    pub fn phases(&self) -> Vec<Phase> {
        self.phases.lock().unwrap().clone()
    }

    pub fn timeline(&self) -> Vec<Snapshot> {
        self.samples.lock().unwrap().clone()
    }

    /// Which sensor sources are actually delivering data (and what is missing)
    pub fn notes(&self) -> Vec<String> {
        self.notes.lock().unwrap().clone()
    }
}

struct Collector {
    sys: sysinfo::System,
    hwmon_root: std::path::PathBuf,
    drm_root: std::path::PathBuf,
    nvidia_ok: Option<bool>,
    win: Option<WinStream>,
    zones: ZonePicker,
    flat: FlatlineWatch,
    began: Instant,
    notes: Vec<String>,
    warmup: u32,
    helper_core: Option<usize>,
    rapl: Rapl,
    powercap_root: std::path::PathBuf,
}

impl Collector {
    fn new() -> Self {
        let mut sys = sysinfo::System::new();
        sys.refresh_cpu();
        Self {
            sys,
            hwmon_root: "/sys/class/hwmon".into(),
            drm_root: "/sys/class/drm".into(),
            nvidia_ok: None,
            win: if cfg!(target_os = "windows") { Some(WinStream::start()) } else { None },
            zones: ZonePicker::default(),
            flat: FlatlineWatch::default(),
            began: Instant::now(),
            notes: Vec::new(),
            warmup: 0,
            helper_core: None,
            rapl: Rapl::default(),
            powercap_root: "/sys/class/powercap".into(),
        }
    }

    /// The busiest other programs right now; processes of the same name are added up (a browser is
    /// dozens of processes). CPU is % of the whole CPU; GPU comes from `gpu_by_pid` (Windows).
    fn sample_programs(&mut self, helper: Option<u32>, gpu_by_pid: &[(u32, f32)]) -> Vec<ProcUsage> {
        self.sys.refresh_processes();
        let ncpu = self.sys.cpus().len().max(1) as f32;
        let me = std::process::id();
        let mut by: std::collections::HashMap<String, (f32, f32)> = Default::default();
        for (pid, p) in self.sys.processes() {
            let pid = pid.as_u32();
            if pid == me || Some(pid) == helper || p.parent().map(|pp| pp.as_u32()) == Some(me) {
                continue;
            }
            let cpu = p.cpu_usage() / ncpu;
            let gpu = gpu_by_pid.iter().find(|g| g.0 == pid).map_or(0.0, |g| g.1);
            if cpu >= 0.2 || gpu >= 0.2 {
                let e = by.entry(p.name().to_string()).or_default();
                e.0 += cpu;
                e.1 = (e.1 + gpu).min(100.0);
            }
        }
        let mut v: Vec<ProcUsage> = by.into_iter().map(|(name, (c, g))| ProcUsage { name, cpu_pct: c.min(100.0), gpu_pct: g }).collect();
        v.sort_by(|a, b| (b.cpu_pct + b.gpu_pct).total_cmp(&(a.cpu_pct + a.gpu_pct)));
        v.truncate(MAX_PROCS);
        v
    }

    /// Keep the Windows sensor helper process on the app's core as well
    fn follow_app_core(&mut self) {
        // re-applied every sample: cheap, and covers a helper that was restarted meanwhile
        let want = crate::app_core::current();
        if want.is_some() || self.helper_core.is_some() {
            if let Some(win) = &self.win {
                win.set_affinity(want);
            }
        }
        self.helper_core = want;
    }

    fn collect(&mut self) -> Snapshot {
        let mut snap = Snapshot::default();
        let mut notes = Vec::new();

        self.sys.refresh_cpu();
        self.sys.refresh_memory();
        snap.core_freq_mhz = self.sys.cpus().iter().map(|c| c.frequency() as f32).collect();
        snap.core_usage_pct = self.sys.cpus().iter().map(|c| c.cpu_usage()).collect();
        snap.ram_total_mb = self.sys.total_memory() as f32 / 1048576.0;
        snap.ram_used_mb = self.sys.used_memory() as f32 / 1048576.0;
        // sysinfo needs two refreshes before usage is meaningful
        if self.warmup < 2 {
            self.warmup += 1;
            snap.core_usage_pct.clear();
        }

        // CPU temperatures
        let hw = read_hwmon(&self.hwmon_root);
        let mut cpu_source = String::new();
        if hw.package_c.is_some() || !hw.cores.is_empty() {
            snap.cpu_package_c = hw.package_c;
            snap.core_temps_c = hw.cores.clone();
            cpu_source = "Linux hwmon".into();
            notes.push(format!(
                "CPU temperature: Linux hwmon ({} per-core sensor{})",
                hw.cores.len(),
                if hw.cores.len() == 1 { "" } else { "s" }
            ));
        }
        if let Some(win) = self.win.as_mut() {
            win.revive();
        }
        if let Some(win) = &self.win {
            match win.fresh() {
                Some(w) => {
                    if snap.cpu_package_c.is_none() && snap.core_temps_c.is_empty() {
                        snap.cpu_package_c = if w.source == ACPI_SOURCE && !w.zones_c.is_empty() {
                            self.zones.pick(&w.zones_c)
                        } else {
                            w.package_c
                        };
                        snap.core_temps_c = w.cores.clone();
                        cpu_source = w.source.clone();
                    }
                    notes.extend(lhm_notes(&w, crate::lhm::is_admin()));
                    if w.cores.is_empty() {
                        notes.push(format!("CPU temperature: {} (package only, no per-core values)", w.source));
                    } else {
                        notes.push(format!("CPU temperature: {} ({} per-core sensors)", w.source, w.cores.len()));
                    }
                    if w.core_freq_mhz.len() >= snap.core_freq_mhz.len().min(1) && !w.core_freq_mhz.is_empty() {
                        // real clocks instead of sysinfo's nominal values
                        let n = snap.core_freq_mhz.len().max(w.core_freq_mhz.len());
                        snap.core_freq_mhz = (0..n).map(|i| w.core_freq_mhz.get(i).copied().unwrap_or(0.0)).collect();
                    }
                    if let Some(g) = w.gpu_c {
                        snap.gpu = Some(GpuSensors { name: "GPU".into(), temp_c: Some(g), ..Default::default() });
                    }
                }
                None if win.ever_delivered() => notes.push(
                    "CPU temperature: the Windows sensor helper stopped updating, so old readings are being discarded (restarting it)".into(),
                ),
                None if win.age_s() < 10.0 => notes.push("CPU temperature: waiting for the Windows sensor helper to start".into()),
                None => {}
            }
        }
        if snap.cpu_package_c.is_none() && snap.core_temps_c.is_empty() {
            let starting = self.win.as_ref().map_or(false, |w| !w.ever_delivered() && w.age_s() < 10.0);
            if !starting && !notes.iter().any(|n| n.contains("stopped updating")) {
                notes.push("CPU temperature: no sensor available (Windows: run LibreHardwareMonitor for per-core values)".into());
            }
        }

        // A CPU temperature that does not move at all while the CPU is busy is not a real reading
        let headline = snap.cpu_package_c.or_else(|| max(snap.core_temps_c.iter().map(|c| c.1)));
        let busiest = max(snap.core_usage_pct.iter().copied()).unwrap_or(0.0);
        self.flat.push(self.began.elapsed().as_millis() as u64, headline, busiest);
        if let Some(secs) = self.flat.flat_for_s() {
            notes.push(if cpu_source == ACPI_SOURCE {
                format!(
                    "CPU temperature has not changed for {} s while the CPU was busy: this PC's ACPI thermal zone reports a fixed value, not the real CPU temperature. Run LibreHardwareMonitor (or OpenHardwareMonitor) for live readings.",
                    secs
                )
            } else {
                format!(
                    "CPU temperature has not changed for {} s while the CPU was busy: the sensor may be stuck, compare it with another monitoring tool.",
                    secs
                )
            });
        }

        // Everything else the machine exposes
        snap.sensors = read_hwmon_all(&self.hwmon_root);
        snap.sensors.extend(self.rapl.read(&self.powercap_root));
        let win_now = self.win.as_ref().and_then(|w| w.fresh());
        if let Some(w) = &win_now {
            snap.sensors.extend(w.extra.clone());
        }
        snap.sensors.truncate(MAX_EXTRA_SENSORS);

        // Other programs: which ones used the CPU / GPU while the tests ran
        let helper = self.win.as_ref().and_then(|w| w.child.as_ref().map(|c| c.id()));
        let gpu_by_pid = win_now.as_ref().map(|w| w.gpu_procs.clone()).unwrap_or_default();
        snap.procs = self.sample_programs(helper, &gpu_by_pid);
        snap.procs_sampled = true;
        for p in snap.procs.iter().take(5) {
            if p.cpu_pct >= 1.0 {
                snap.sensors.push(SensorReading { name: format!("Program {}: CPU", p.name), kind: SensorKind::Load, value: p.cpu_pct });
            }
            if p.gpu_pct >= 1.0 {
                snap.sensors.push(SensorReading { name: format!("Program {}: GPU", p.name), kind: SensorKind::Load, value: p.gpu_pct });
            }
        }
        if snap.sensors.iter().any(|r| r.kind != SensorKind::Load) {
            let count = |k: SensorKind| snap.sensors.iter().filter(|r| r.kind == k).count();
            notes.push(format!(
                "Other sensors: {} temperatures, {} fans, {} power, {} voltages, {} currents",
                count(SensorKind::Temp),
                count(SensorKind::Fan),
                count(SensorKind::Power),
                count(SensorKind::Voltage),
                count(SensorKind::Current)
            ));
        } else if cfg!(target_os = "windows") {
            notes.push("Board, memory, VRM, fan and power sensors: none yet (see the LibreHardwareMonitor notes above)".into());
        }

        // GPU
        if self.nvidia_ok != Some(false) {
            match query_nvidia_smi() {
                Some(g) => {
                    self.nvidia_ok = Some(true);
                    notes.push(format!("GPU: nvidia-smi ({})", g.name));
                    snap.gpu = Some(g);
                }
                None => self.nvidia_ok = Some(false),
            }
        }
        if snap.gpu.as_ref().map_or(true, |g| g.vram_total_mb.is_none()) {
            if let Some(temp) = hw.amdgpu_temp_c {
                let (used, total) = read_amd_vram(&self.drm_root).unzip();
                snap.gpu = Some(GpuSensors {
                    name: "AMD GPU".into(),
                    temp_c: Some(temp),
                    util_pct: read_amd_busy(&self.drm_root),
                    vram_used_mb: used,
                    vram_total_mb: total,
                    ..Default::default()
                });
                notes.push("GPU: Linux amdgpu sysfs".into());
            }
        }
        match &snap.gpu {
            None => notes.push("GPU/VRAM: no supported sensor (NVIDIA needs nvidia-smi on PATH)".into()),
            Some(g) if g.util_pct.is_none() => notes.push("GPU load: this sensor source does not report it".into()),
            Some(_) => {}
        }

        self.notes = notes;
        snap
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(dir: &Path, file: &str, content: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(file), content).unwrap();
    }

    #[test]
    fn other_programs_are_averaged_per_test() {
        let prog = |n: &str, c: f32, g: f32| ProcUsage { name: n.into(), cpu_pct: c, gpu_pct: g };
        let a = Snapshot { t_ms: 0, procs: vec![prog("chrome.exe", 20.0, 0.0), prog("obs64.exe", 2.0, 30.0)], procs_sampled: true, ..Default::default() };
        let b = Snapshot { t_ms: 500, procs: vec![prog("chrome.exe", 10.0, 0.0)], procs_sampled: true, ..Default::default() };
        let t = summarize(&[a, b]);
        assert_eq!(t.others_cpu_avg_pct, Some(16.0), "15 % chrome + 1 % obs");
        assert_eq!(t.others_gpu_avg_pct, Some(15.0));
        assert_eq!(t.others_top[0], ("obs64.exe".to_string(), 1.0, 15.0), "busiest (CPU + GPU) first");
        assert_eq!(t.others_text(), "obs64.exe 1 % CPU / 15 % GPU, chrome.exe 15 % CPU");
        // not sampled at all: unknown, not zero
        assert_eq!(summarize(&[Snapshot::default()]).others_cpu_avg_pct, None);
        let quiet = summarize(&[Snapshot { procs_sampled: true, ..Default::default() }]);
        assert_eq!((quiet.others_cpu_avg_pct, quiet.others_gpu_avg_pct), (Some(0.0), None));
    }

    #[test]
    fn gpu_engine_counters_become_per_process_use() {
        // pid 42: 3D engines 30 + 25 = 55 %, copy 10 %  ->  55 %; pid 7: video decode 12 %
        let line = r#"{"src":null,"t":[],"tz":[300.0],"gp":[{"p":42,"e":"3D","v":30.0},{"p":42,"e":"3D","v":25.0},{"p":42,"e":"Copy","v":10.0},{"p":7,"e":"VideoDecode","v":12.0}]}"#;
        let w = parse_win_sensor_line(line).unwrap();
        assert_eq!(w.gpu_procs, vec![(7, 12.0), (42, 55.0)]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_busy_program_is_seen() {
        let mut c = Collector::new();
        // `sh` is our child and left out (the app's own helpers are); `yes` under it is another program
        let mut child = std::process::Command::new("sh").args(["-c", "exec 2>/dev/null; yes > /dev/null & sleep 3; kill $!"]).spawn().unwrap();
        // a process's share shows from its second sample on: let it start first
        std::thread::sleep(std::time::Duration::from_millis(300));
        // like `collect`: the CPU totals are refreshed each sample (process shares are relative to them)
        c.sys.refresh_cpu();
        c.sample_programs(None, &[]);
        std::thread::sleep(std::time::Duration::from_millis(1200));
        c.sys.refresh_cpu();
        let v = c.sample_programs(None, &[]);
        let _ = child.wait();
        let yes = v.iter().find(|p| p.name == "yes").unwrap_or_else(|| panic!("yes not seen in {:?}", v));
        assert!(yes.cpu_pct > 1.0, "{:?}", yes);
        assert!(v.iter().all(|p| p.name != "sh"), "our own child is left out");
        assert!(v.len() <= MAX_PROCS);
    }

    #[test]
    fn hybrid_core_temps_clocks_and_lhm_status_are_read() {
        use crate::topology::{CoreKind::*, CoreMap};
        let mut kinds = vec![Efficiency; 24];
        for p in [0, 1, 10, 11, 12, 13, 22, 23] {
            kinds[p] = Performance;
        }
        let map = CoreMap::from_parts((0..24).map(|c| vec![c]).collect(), Some(&kinds));
        let line = r#"{"src":"lib","t":[{"n":"CPU Package","v":61.0},{"n":"P-Core #3","v":70.0},{"n":"E-Core #1","v":55.0},{"n":"P-Core #3 Distance to TjMax","v":30.0}],
            "cl":[{"n":"P-Core #1","v":5500.0},{"n":"E-Core #16","v":4600.0}],"x":[],"lhm":true,"lerr":"","pawn":"2.0.1.0"}"#;
        let w = parse_win_sensor_line_with(&line.replace('\n', ""), &map).unwrap();
        assert_eq!(w.cores, vec![(2, 55.0), (10, 70.0)], "P-Core #3 is CPU 10, E-Core #1 is CPU 2");
        assert_eq!(w.package_c, Some(61.0));
        assert_eq!((w.core_freq_mhz[0], w.core_freq_mhz[21]), (5500.0, 4600.0));
        assert_eq!((w.lhm_present, w.lhm_error.as_deref(), w.pawnio.as_deref()), (true, None, Some("2.0.1.0")));
        assert_eq!(w.lhm_pe, Some((1, 1)));
        // a failed load is reported even without any temperature
        let e = parse_win_sensor_line(r#"{"src":null,"t":[],"tz":[],"lhm":true,"lerr":"could not load","pawn":""}"#).unwrap();
        assert_eq!((e.lhm_error.as_deref(), e.pawnio.as_deref()), (Some("could not load"), None));
        assert!(lhm_notes(&e, true)[0].contains("could not load"));
        let no_pawn = WinSensors { lhm_present: true, ..Default::default() };
        assert!(lhm_notes(&no_pawn, true).iter().any(|n| n.contains("PawnIO")));
    }

    #[test]
    fn full_sample_buffer_is_thinned_not_truncated() {
        // the same rule the sampler thread applies at MAX_SAMPLES
        let mut v: Vec<u64> = (0..MAX_SAMPLES as u64).collect();
        let mut i = 0;
        v.retain(|_| { i += 1; i % 2 == 0 });
        v.push(MAX_SAMPLES as u64);
        assert_eq!(v.len(), MAX_SAMPLES / 2 + 1);
        assert!(v[0] <= 1, "the start of the run is still covered");
        assert!(v.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn hwmon_all_reads_board_memory_fans_power_and_voltages() {
        let root = tempfile::tempdir().unwrap();
        let board = root.path().join("hwmon2");
        write(&board, "name", "nct6798");
        write(&board, "temp1_label", "VRM MOS");
        write(&board, "temp1_input", "61500");
        write(&board, "fan2_input", "1180");
        write(&board, "in0_label", "Vcore");
        write(&board, "in0_input", "1236");
        write(&board, "temp9_input", "0"); // unconnected input: dropped
        let dimm = root.path().join("hwmon5");
        write(&dimm, "name", "spd5118");
        write(&dimm, "temp1_input", "44250");
        let psu = root.path().join("hwmon7");
        write(&psu, "name", "corsairpsu");
        write(&psu, "power1_label", "power total");
        write(&psu, "power1_input", "412000000");
        write(&psu, "curr1_input", "33500");
        let cpu = root.path().join("hwmon0");
        write(&cpu, "name", "coretemp");
        write(&cpu, "temp1_input", "70000"); // CPU temps are reported elsewhere
        let r = read_hwmon_all(root.path());
        let get = |n: &str| r.iter().find(|x| x.name == n).map(|x| (x.kind, x.value));
        assert_eq!(get("nct6798: VRM MOS"), Some((SensorKind::Temp, 61.5)));
        assert_eq!(get("nct6798: fan2"), Some((SensorKind::Fan, 1180.0)));
        assert!((get("nct6798: Vcore").unwrap().1 - 1.236).abs() < 1e-4);
        assert_eq!(get("spd5118: temp1"), Some((SensorKind::Temp, 44.25)));
        assert_eq!(get("corsairpsu: power total"), Some((SensorKind::Power, 412.0)));
        assert_eq!(get("corsairpsu: curr1"), Some((SensorKind::Current, 33.5)));
        assert!(get("nct6798: temp9").is_none() && !r.iter().any(|x| x.name.starts_with("coretemp")));
        assert!(read_hwmon_all(Path::new("/definitely/not/here")).is_empty());
    }

    #[test]
    fn rapl_turns_energy_counters_into_watts_and_survives_a_wrap() {
        let root = tempfile::tempdir().unwrap();
        let pkg = root.path().join("intel-rapl:0");
        write(&pkg, "name", "package-0");
        write(&pkg, "max_energy_range_uj", "1000000000");
        write(&pkg, "energy_uj", "999000000");
        let mut rapl = Rapl::default();
        assert!(rapl.read(root.path()).is_empty(), "the first sample only sets the baseline");
        // pretend the first sample was 0.5 s ago, then the counter wrapped past the range
        for v in rapl.last.values_mut() {
            v.0 -= Duration::from_millis(500);
        }
        write(&pkg, "energy_uj", "39000000"); // +1 J to the wrap, +39 J after it = 40 J
        let r = rapl.read(root.path());
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].name, "RAPL: package-0");
        assert!((r[0].value - 80.0).abs() < 2.0, "40 J in 0.5 s = 80 W, got {}", r[0].value);
    }

    #[test]
    fn windows_lhm_extra_sensors_and_package_power() {
        let line = r#"{"src":"root/LibreHardwareMonitor","t":[{"n":"CPU Package","v":55.0}],"x":[
            {"n":"Nuvoton NCT6798D: VRM MOS","k":"Temperature","v":58.0},
            {"n":"DIMM #1: Temperature","k":"Temperature","v":41.5},
            {"n":"Nuvoton NCT6798D: CPU Fan","k":"Fan","v":1320.0},
            {"n":"Intel Core Ultra 7 270K Plus: CPU Package","k":"Power","v":125.5},
            {"n":"Corsair HX1000i: Total","k":"Power","v":402.0},
            {"n":"Nuvoton NCT6798D: Vcore","k":"Voltage","v":1.25},
            {"n":"Something: Load","k":"Load","v":50.0},
            {"n":"Nuvoton NCT6798D: Temperature #6","k":"Temperature","v":-128.0}],"tz":[]}"#;
        let w = parse_win_sensor_line(line).unwrap();
        assert_eq!(w.extra.len(), 6, "loads and impossible readings are dropped: {:?}", w.extra);
        let snap = Snapshot { sensors: w.extra, ..Default::default() };
        assert_eq!(snap.cpu_package_power_w(), Some(125.5));
        assert_eq!(summarize(&[snap]).cpu_power_avg_w, Some(125.5));
    }

    #[test]
    fn tests_are_recorded_as_phases() {
        let s = Sampler::start(Duration::from_millis(20));
        std::thread::sleep(Duration::from_millis(80));
        let t = s.record("cpu", "CPU · IntegerAdd".into(), 0, s.now_ms());
        assert!(t.samples >= 1);
        let p = s.phases();
        assert_eq!(p.len(), 1);
        assert_eq!((p[0].kind.as_str(), p[0].label.as_str(), p[0].start_ms), ("cpu", "CPU · IntegerAdd", 0));
        assert!(p[0].end_ms >= 80);
    }

    #[test]
    fn hwmon_coretemp_per_core() {
        let root = tempfile::tempdir().unwrap();
        let d = root.path().join("hwmon3");
        write(&d, "name", "coretemp\n");
        write(&d, "temp1_label", "Package id 0\n");
        write(&d, "temp1_input", "61000\n");
        write(&d, "temp2_label", "Core 0\n");
        write(&d, "temp2_input", "55000\n");
        write(&d, "temp3_label", "Core 4\n");
        write(&d, "temp3_input", "72500\n");
        let r = read_hwmon(root.path());
        assert_eq!(r.package_c, Some(61.0));
        assert_eq!(r.cores, vec![(0, 55.0), (4, 72.5)]);
    }

    #[test]
    fn hwmon_k10temp_and_amdgpu_and_junk() {
        let root = tempfile::tempdir().unwrap();
        write(&root.path().join("hwmon0"), "name", "k10temp");
        write(&root.path().join("hwmon0"), "temp1_label", "Tctl");
        write(&root.path().join("hwmon0"), "temp1_input", "48250");
        write(&root.path().join("hwmon1"), "name", "amdgpu");
        write(&root.path().join("hwmon1"), "temp1_input", "39000");
        write(&root.path().join("hwmon2"), "name", "nvme");
        write(&root.path().join("hwmon2"), "temp1_input", "35000");
        write(&root.path().join("hwmon4"), "name", "coretemp");
        write(&root.path().join("hwmon4"), "temp1_input", "notanumber");
        let r = read_hwmon(root.path());
        assert_eq!(r.package_c, Some(48.25));
        assert_eq!(r.amdgpu_temp_c, Some(39.0));
        assert!(r.cores.is_empty());
        assert_eq!(read_hwmon(Path::new("/definitely/not/here")), HwmonReading::default());
    }

    #[test]
    fn amd_vram() {
        let root = tempfile::tempdir().unwrap();
        let dev = root.path().join("card0").join("device");
        write(&dev, "mem_info_vram_used", "1073741824");
        write(&dev, "mem_info_vram_total", "17179869184");
        assert_eq!(read_amd_vram(root.path()), Some((1024.0, 16384.0)));
    }

    #[test]
    fn nvidia_smi_lines() {
        let g = parse_nvidia_smi("NVIDIA GeForce RTX 4080, 37, 2, 1234, 16376, 24.55, 210\n").unwrap();
        assert_eq!(g.name, "NVIDIA GeForce RTX 4080");
        assert_eq!(g.temp_c, Some(37.0));
        assert_eq!(g.vram_used_mb, Some(1234.0));
        assert_eq!(g.vram_total_mb, Some(16376.0));
        assert_eq!(g.power_w, Some(24.55));
        let na = parse_nvidia_smi("GPU X, 40, [N/A], 10, 20, [N/A], [N/A]").unwrap();
        assert_eq!(na.util_pct, None);
        assert_eq!(na.power_w, None);
        assert!(parse_nvidia_smi("").is_none());
        assert!(parse_nvidia_smi("garbage").is_none());
    }

    #[test]
    fn windows_lhm_per_core_json() {
        let line = r#"{"src":"root/LibreHardwareMonitor","t":[{"n":"CPU Package","v":51.0},{"n":"CPU Core #1","v":47.0},{"n":"CPU Core #2","v":62.5},{"n":"GPU Core","v":36.0},{"n":"CPU Core #3","v":999.0}],"tz":[]}"#;
        let w = parse_win_sensor_line(line).unwrap();
        assert_eq!(w.source, "LibreHardwareMonitor");
        assert_eq!(w.package_c, Some(51.0));
        assert_eq!(w.cores, vec![(0, 47.0), (1, 62.5)]);
        assert_eq!(w.gpu_c, Some(36.0));
    }

    #[test]
    fn windows_actual_clocks_from_processor_performance() {
        let line = r#"{"src":null,"t":[],"tz":[],"perf":[{"n":"0,0","v":143.0},{"n":"0,1","v":100.0},{"n":"0,2","v":0.0},{"n":"junk","v":1.0}],"base":3700}"#;
        let w = parse_win_sensor_line(line).unwrap();
        // 143% of a 3.7 GHz base clock = 5.29 GHz; zero / malformed instances are skipped
        assert_eq!(w.core_freq_mhz.len(), 2);
        assert!((w.core_freq_mhz[0] - 5291.0).abs() < 1.0);
        assert!((w.core_freq_mhz[1] - 3700.0).abs() < 1.0);
    }

    #[test]
    fn windows_thermal_zone_fallback_is_package_only() {
        let w = parse_win_sensor_line(r#"{"src":null,"t":[],"tz":[303.15,318.15]}"#).unwrap();
        assert_eq!(w.source, "ACPI thermal zone");
        assert!(w.cores.is_empty());
        assert!((w.package_c.unwrap() - 45.0).abs() < 0.01);
        // PowerShell collapses one-element arrays to scalars
        let w = parse_win_sensor_line(r#"{"src":null,"t":{"n":"CPU Package","v":40.0},"tz":300.0}"#).unwrap();
        assert_eq!(w.package_c, Some(40.0));
        assert!(parse_win_sensor_line(r#"{"src":null,"t":[],"tz":[]}"#).is_none());
        assert!(parse_win_sensor_line("not json").is_none());
    }

    #[test]
    fn windows_high_precision_zone_temps_and_all_zones_kept() {
        // tenths of Kelvin: 3011 = 301.1 K = 27.95 °C; preferred over the whole-Kelvin counter
        let w = parse_win_sensor_line(r#"{"src":null,"t":[],"tz":[301,331],"tzh":[3011,3315]}"#).unwrap();
        assert_eq!(w.source, "ACPI thermal zone");
        assert_eq!(w.zones_c.len(), 2);
        assert!((w.zones_c[0] - 27.95).abs() < 0.01, "{:?}", w.zones_c);
        assert!((w.package_c.unwrap() - 58.35).abs() < 0.01);
        // helper without the high precision counter: whole-Kelvin values are used
        let w = parse_win_sensor_line(r#"{"src":null,"t":[],"tz":[301],"tzh":[]}"#).unwrap();
        assert!((w.zones_c[0] - 27.85).abs() < 0.01);
    }

    #[test]
    fn stale_windows_reading_is_dropped() {
        let now = Instant::now();
        let reading = WinSensors { package_c: Some(45.0), ..Default::default() };
        let mut sh = WinShared { reading: Some(reading), reading_at: now.checked_sub(Duration::from_secs(2)), line_at: None };
        assert_eq!(sh.fresh(now, WIN_STALE).and_then(|w| w.package_c), Some(45.0));
        // the helper hung 30 s ago: the old value must not keep being reported
        sh.reading_at = now.checked_sub(Duration::from_secs(30));
        assert!(sh.fresh(now, WIN_STALE).is_none());
        assert!(WinShared::default().fresh(now, WIN_STALE).is_none());
    }

    #[test]
    fn zone_picker_prefers_the_zone_that_moves() {
        let mut p = ZonePicker::default();
        // zone 0 is a fixed 60 °C, zone 1 is the real one and reads cooler
        assert_eq!(p.pick(&[60.0, 41.0]), Some(60.0)); // nothing has moved yet: hottest
        p.pick(&[60.0, 43.5]);
        assert_eq!(p.pick(&[60.0, 47.0]), Some(47.0));
        // a different number of zones starts over
        assert_eq!(p.pick(&[30.0]), Some(30.0));
        assert_eq!(p.pick(&[]), None);
    }

    #[test]
    fn flatline_is_reported_only_for_a_busy_cpu_with_a_dead_flat_temperature() {
        let feed = |temps: &dyn Fn(u64) -> f32, load: f32| {
            let mut w = FlatlineWatch::default();
            for i in 0..40u64 {
                w.push(i * 500, Some(temps(i)), load);
            }
            w.flat_for_s()
        };
        // busy and never moving for 20 s: flagged
        assert!(feed(&|_| 27.8, 90.0).is_some());
        // same value on an idle CPU is normal
        assert!(feed(&|_| 27.8, 3.0).is_none());
        // a moving temperature is fine
        assert!(feed(&|i| 40.0 + (i / 4) as f32, 90.0).is_none());
        // too little history to judge
        let mut short = FlatlineWatch::default();
        for i in 0..6u64 {
            short.push(i * 500, Some(27.8), 90.0);
        }
        assert!(short.flat_for_s().is_none());
        // losing the sensor clears the history
        let mut w = FlatlineWatch::default();
        for i in 0..40u64 {
            w.push(i * 500, Some(27.8), 90.0);
        }
        w.push(20_000, None, 90.0);
        assert!(w.flat_for_s().is_none());
    }

    #[test]
    fn amd_busy_percent() {
        let root = tempfile::tempdir().unwrap();
        write(&root.path().join("card0").join("device"), "gpu_busy_percent", "87\n");
        assert_eq!(read_amd_busy(root.path()), Some(87.0));
        assert_eq!(read_amd_busy(Path::new("/definitely/not/here")), None);
    }

    #[test]
    fn collector_follows_hwmon_changes() {
        // Guards against a cached / stale read: every collect() must reflect the file as it is now
        let root = tempfile::tempdir().unwrap();
        let d = root.path().join("hwmon0");
        write(&d, "name", "coretemp");
        write(&d, "temp1_label", "Package id 0");
        write(&d, "temp2_label", "Core 0");
        let mut c = Collector::new();
        c.hwmon_root = root.path().to_path_buf();
        c.win = None;
        let mut seen = Vec::new();
        for milli in [41000, 55000, 68000] {
            write(&d, "temp1_input", &milli.to_string());
            write(&d, "temp2_input", &(milli - 3000).to_string());
            let s = c.collect();
            seen.push((s.cpu_package_c.unwrap(), s.core_temps_c[0].1));
        }
        assert_eq!(seen, vec![(41.0, 38.0), (55.0, 52.0), (68.0, 65.0)]);
    }

    fn snap(t: u64, pkg: Option<f32>, cores: &[(usize, f32)], gpu_t: Option<f32>, vram: Option<f32>) -> Snapshot {
        Snapshot {
            t_ms: t,
            cpu_package_c: pkg,
            core_temps_c: cores.to_vec(),
            core_freq_mhz: vec![4000.0, 5000.0],
            core_usage_pct: vec![50.0, 100.0],
            ram_used_mb: 1000.0 + t as f32,
            ram_total_mb: 64000.0,
            gpu: gpu_t.map(|t| GpuSensors { name: "g".into(), temp_c: Some(t), vram_used_mb: vram, vram_total_mb: Some(16000.0), ..Default::default() }),
            sensors: Vec::new(),
            ..Default::default()
        }
    }

    #[test]
    fn summarize_takes_peaks_and_averages() {
        let t = summarize(&[
            snap(0, Some(50.0), &[(0, 48.0), (1, 55.0)], Some(40.0), Some(500.0)),
            snap(500, Some(60.0), &[(0, 58.0), (1, 52.0)], Some(45.0), Some(900.0)),
        ]);
        assert_eq!(t.samples, 2);
        assert_eq!(t.cpu_temp_avg_c, Some(55.0));
        assert_eq!(t.cpu_temp_max_c, Some(60.0));
        assert_eq!(t.core_temp_max_c, vec![(0, 58.0), (1, 55.0)]);
        // (48 + 55 + 58 + 52) / 4
        assert_eq!(t.core_temp_avg_c, Some(53.25));
        assert_eq!(t.hottest_core(), Some((0, 58.0)));
        assert_eq!(t.coolest_core(), Some((1, 55.0)));
        assert_eq!(t.cpu_freq_avg_mhz, Some(4500.0));
        assert_eq!(t.core_freq_avg_mhz, vec![4000.0, 5000.0]);
        assert_eq!(t.gpu_temp_max_c, Some(45.0));
        assert_eq!(t.vram_used_max_mb, Some(900.0));
        assert_eq!(t.vram_total_mb, Some(16000.0));
        assert_eq!(t.ram_used_max_mb, Some(1500.0));
        assert_eq!(summarize(&[]).samples, 0);
    }

    #[test]
    fn hottest_core_is_used_when_no_package_sensor() {
        let t = summarize(&[snap(0, None, &[(0, 41.0), (1, 66.0)], None, None)]);
        assert_eq!(t.cpu_temp_avg_c, Some(66.0));
        assert!(t.gpu_temp_max_c.is_none());
    }

    #[test]
    fn sampler_collects_and_windows() {
        let s = Sampler::start(Duration::from_millis(50));
        std::thread::sleep(Duration::from_millis(400));
        let now = s.now_ms();
        let w = s.window(0, now);
        assert!(w.samples >= 2, "samples = {}", w.samples);
        assert!(w.ram_used_max_mb.unwrap_or(0.0) > 0.0);
        // A zero-width window still resolves to the nearest sample
        assert_eq!(s.window(now + 10_000, now + 10_001).samples, 1);
        assert!(!s.timeline().is_empty());
        assert!(!s.notes().is_empty());
    }
}

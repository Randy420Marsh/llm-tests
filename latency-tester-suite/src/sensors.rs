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
}

/// Summary of the samples taken during one test
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Telemetry {
    pub samples: usize,
    pub cpu_temp_avg_c: Option<f32>,
    pub cpu_temp_max_c: Option<f32>,
    /// Peak temperature per core during the test
    pub core_temp_max_c: Vec<(usize, f32)>,
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
}

fn avg(v: impl Iterator<Item = f32>) -> Option<f32> {
    let (sum, n) = v.fold((0.0f32, 0usize), |(s, n), x| (s + x, n + 1));
    (n > 0).then(|| sum / n as f32)
}

fn max(v: impl Iterator<Item = f32>) -> Option<f32> {
    v.fold(None, |m: Option<f32>, x| Some(m.map_or(x, |m| m.max(x))))
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

fn hidden_command(program: &str) -> std::process::Command {
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
    /// "LibreHardwareMonitor", "OpenHardwareMonitor" or "ACPI thermal zone"
    pub source: String,
}

/// Parse one JSON line printed by [`WIN_STREAM_SCRIPT`]
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn parse_win_sensor_line(line: &str) -> Option<WinSensors> {
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
        if let Some(rest) = lower.strip_prefix("cpu core #") {
            if let Ok(n) = rest.trim().parse::<usize>() {
                w.cores.push((n.saturating_sub(1), val));
            }
        } else if lower.contains("cpu package") || lower.contains("tctl") || lower.contains("core (tdie)") || lower == "core average" {
            w.package_c = Some(w.package_c.map_or(val, |p| p.max(val)));
        } else if lower == "gpu core" || lower.starts_with("gpu core") {
            w.gpu_c = Some(val);
        }
    }
    w.cores.sort_by_key(|c| c.0);
    w.cores.dedup_by_key(|c| c.0);
    if let Some(src) = v["src"].as_str().filter(|s| !s.is_empty()) {
        w.source = if src.contains("Libre") { "LibreHardwareMonitor" } else { "OpenHardwareMonitor" }.to_string();
    }
    if w.package_c.is_none() && w.cores.is_empty() {
        // Thermal-zone counters are Kelvin; the hottest zone is the best package stand-in
        let z = as_list(&v["tz"]).iter().filter_map(|k| k.as_f64()).map(|k| if k > 200.0 { k - 273.15 } else { k } as f32)
            .filter(|c| (1.0..150.0).contains(c))
            .fold(None, |m: Option<f32>, c| Some(m.map_or(c, |m| m.max(c))));
        if let Some(c) = z {
            w.package_c = Some(c);
            w.source = "ACPI thermal zone".to_string();
        }
    }
    (w.package_c.is_some() || !w.cores.is_empty() || w.gpu_c.is_some()).then_some(w)
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
const WIN_STREAM_SCRIPT: &str = r#"
$ErrorActionPreference = 'SilentlyContinue'
while ($true) {
  $ns = $null; $s = $null
  foreach ($n in 'root/LibreHardwareMonitor', 'root/OpenHardwareMonitor') {
    $s = Get-CimInstance -Namespace $n -ClassName Sensor | Where-Object { $_.SensorType -eq 'Temperature' }
    if ($s) { $ns = $n; break }
  }
  $t = @(); if ($s) { $t = @($s | ForEach-Object { @{ n = $_.Name; v = [double]$_.Value } }) }
  $tz = @()
  try { $tz = @((Get-Counter '\Thermal Zone Information(*)\Temperature' -ErrorAction Stop).CounterSamples | ForEach-Object { [double]$_.CookedValue }) } catch {}
  [pscustomobject]@{ src = $ns; t = $t; tz = $tz } | ConvertTo-Json -Compress -Depth 4
  Start-Sleep -Milliseconds 500
}
"#;

/// Long-running PowerShell child that prints one JSON line of sensor data per second
struct WinStream {
    latest: Arc<Mutex<Option<WinSensors>>>,
    child: Option<std::process::Child>,
}

impl WinStream {
    #[cfg(target_os = "windows")]
    fn start() -> Self {
        use std::io::{BufRead, BufReader};
        use std::process::Stdio;
        let latest = Arc::new(Mutex::new(None));
        let child = hidden_command("powershell")
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", WIN_STREAM_SCRIPT])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .stdin(Stdio::null())
            .spawn()
            .ok();
        let mut child = child;
        if let Some(stdout) = child.as_mut().and_then(|c| c.stdout.take()) {
            let latest = latest.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if let Some(w) = parse_win_sensor_line(&line) {
                        *latest.lock().unwrap() = Some(w);
                    }
                }
            });
        }
        Self { latest, child }
    }

    #[cfg(not(target_os = "windows"))]
    fn start() -> Self {
        Self { latest: Arc::new(Mutex::new(None)), child: None }
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

// ---------------------------------------------------------------------------------------------
// Sampler
// ---------------------------------------------------------------------------------------------

const MAX_SAMPLES: usize = 40_000;

/// Records [`Snapshot`]s on a background thread until dropped or stopped
pub struct Sampler {
    start: Instant,
    samples: Arc<Mutex<Vec<Snapshot>>>,
    notes: Arc<Mutex<Vec<String>>>,
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
            stop: Arc::new(AtomicBool::new(false)),
        });
        let (samples, notes, stop, start) = (s.samples.clone(), s.notes.clone(), s.stop.clone(), s.start);
        std::thread::spawn(move || {
            let mut collector = Collector::new();
            while !stop.load(Ordering::Relaxed) {
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
                        v.remove(0);
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
    notes: Vec<String>,
    warmup: u32,
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
            notes: Vec::new(),
            warmup: 0,
        }
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
        if hw.package_c.is_some() || !hw.cores.is_empty() {
            snap.cpu_package_c = hw.package_c;
            snap.core_temps_c = hw.cores.clone();
            notes.push(format!(
                "CPU temperature: Linux hwmon ({} per-core sensor{})",
                hw.cores.len(),
                if hw.cores.len() == 1 { "" } else { "s" }
            ));
        }
        if let Some(win) = &self.win {
            if let Some(w) = win.latest.lock().unwrap().clone() {
                if snap.cpu_package_c.is_none() && snap.core_temps_c.is_empty() {
                    snap.cpu_package_c = w.package_c;
                    snap.core_temps_c = w.cores.clone();
                }
                if w.cores.is_empty() {
                    notes.push(format!(
                        "CPU temperature: {} (package only). Per-core temperatures need LibreHardwareMonitor or OpenHardwareMonitor running.",
                        w.source
                    ));
                } else {
                    notes.push(format!("CPU temperature: {} ({} per-core sensors)", w.source, w.cores.len()));
                }
                if let Some(g) = w.gpu_c {
                    snap.gpu = Some(GpuSensors { name: "GPU".into(), temp_c: Some(g), ..Default::default() });
                }
            }
        }
        if snap.cpu_package_c.is_none() && snap.core_temps_c.is_empty() {
            notes.push("CPU temperature: no sensor available (Windows: run LibreHardwareMonitor for per-core values)".into());
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
                    vram_used_mb: used,
                    vram_total_mb: total,
                    ..Default::default()
                });
                notes.push("GPU: Linux amdgpu sysfs".into());
            }
        }
        if snap.gpu.is_none() {
            notes.push("GPU/VRAM: no supported sensor (NVIDIA needs nvidia-smi on PATH)".into());
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

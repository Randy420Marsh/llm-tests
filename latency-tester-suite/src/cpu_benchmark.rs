//! CPU benchmark with core affinity testing for P-cores, E-cores, and combined workloads

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::thread;
use crate::cancel::{self, CancelFlag};
use crate::progress::{self, SharedProgress, SharedResults};
use crate::sensors::{Sampler, Telemetry};
use crate::timer::HighResTimer;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuBenchmarkConfig {
    pub workload_types: Vec<WorkloadType>,
    pub thread_counts: Vec<usize>,
    pub affinity_modes: Vec<AffinityMode>,
    pub duration_seconds: u64,
    pub warmup_seconds: u64,
    pub iterations: u32,
    /// With several core sets (e.g. each core on its own): run every workload on one core set before
    /// the next (false = rotate the core sets between workloads, which spreads the heat)
    #[serde(default)]
    pub core_by_core: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkloadType {
    IntegerAdd,
    IntegerMul,
    IntegerDiv,
    FloatAdd,
    FloatMul,
    FloatDiv,
    FloatFma,
    VectorAdd,
    VectorMul,
    VectorFma,
    MemoryCopy,
    MemoryLatency,
    BranchPrediction,
    CryptoAes,
    CryptoSha,
    MixedWorkload,
    CompilationSim,    // Simulate compilation workload
    GameSim,           // Simulate game workload
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AffinityMode {
    AllCores,          // No affinity, OS scheduler decides
    PerformanceCores,  // Only P-cores
    EfficiencyCores,   // Only E-cores
    SingleCore,        // Single core (iterates through each)
    HyperThreadPairs,  // HT pairs (logical cores sharing physical)
    CustomMask(u64),   // Custom affinity mask
}

impl Default for CpuBenchmarkConfig {
    fn default() -> Self {
        let logical_cores = num_cpus::get();
        let mut thread_counts = vec![1];
        
        // Add powers of 2 up to logical cores
        let mut count = 2;
        while count <= logical_cores {
            thread_counts.push(count);
            count *= 2;
        }
        // Add logical core count if not already present
        if thread_counts.last() != Some(&logical_cores) {
            thread_counts.push(logical_cores);
        }

        Self {
            workload_types: vec![
                WorkloadType::IntegerAdd,
                WorkloadType::FloatFma,
                WorkloadType::VectorFma,
                WorkloadType::MemoryLatency,
                WorkloadType::MixedWorkload,
                WorkloadType::CompilationSim,
                WorkloadType::GameSim,
            ],
            thread_counts,
            affinity_modes: vec![
                AffinityMode::AllCores,
                AffinityMode::PerformanceCores,
                AffinityMode::EfficiencyCores,
            ],
            duration_seconds: 10,
            warmup_seconds: 2,
            iterations: 5,
            core_by_core: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuBenchmarkResult {
    pub workload: WorkloadType,
    pub thread_count: usize,
    pub affinity_mode: AffinityMode,
    pub core_mask: u64,
    pub operations_per_second: f64,
    pub latency_ns: f64,
    pub instructions_per_cycle: Option<f64>,
    pub cycles_per_operation: Option<f64>,
    pub frequency_mhz: u64,
    pub temperature_c: Option<f32>,
    pub power_watts: Option<f32>,
    pub iteration_results: Vec<IterationResult>,
    /// Which cores the threads were pinned to ("All cores (OS scheduled)" when unpinned)
    #[serde(default)]
    pub cores: String,
    /// Temperatures, clocks, RAM and GPU/VRAM readings taken while this test ran
    #[serde(default)]
    pub telemetry: Telemetry,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IterationResult {
    pub iteration: u32,
    pub operations: u64,
    pub duration_ns: u64,
    pub frequency_mhz: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuBenchmarkSummary {
    pub results: Vec<CpuBenchmarkResult>,
    pub config: CpuBenchmarkConfig,
    pub system_info: crate::system_info::SystemInfo,
    pub timestamp: String,
    pub core_topology: CoreTopology,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreTopology {
    pub total_logical: usize,
    pub total_physical: usize,
    pub performance_cores: Vec<usize>,  // Logical core IDs for P-cores
    pub efficiency_cores: Vec<usize>,   // Logical core IDs for E-cores
    pub ht_pairs: Vec<(usize, usize)>,  // Pairs of logical cores sharing physical
    pub cache_topology: Vec<CacheLevel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheLevel {
    pub level: u32,
    pub size_kb: usize,
    pub associativity: u32,
    pub line_size: usize,
    pub shared_by: Vec<usize>, // Logical cores sharing this cache
}

pub struct CpuBenchmark {
    config: CpuBenchmarkConfig,
    timer: HighResTimer,
    topology: CoreTopology,
    cancel: CancelFlag,
    progress: Option<SharedProgress>,
    partial: Option<SharedResults<CpuBenchmarkResult>>,
    sensors: Option<Arc<Sampler>>,
}

impl CpuBenchmark {
    pub fn new(config: CpuBenchmarkConfig) -> Result<Self> {
        let topology = Self::detect_topology()?;
        Ok(Self {
            config,
            timer: HighResTimer::new(),
            topology,
            cancel: cancel::new_flag(),
            progress: None,
            partial: None,
            sensors: None,
        })
    }

    /// Publish live progress and finished results so the GUI can show them while running
    pub fn with_progress(mut self, progress: SharedProgress, partial: SharedResults<CpuBenchmarkResult>) -> Self {
        self.progress = Some(progress);
        self.partial = Some(partial);
        self
    }

    /// Attach a sensor sampler so every result carries temperatures / clocks / VRAM
    pub fn with_sensors(mut self, sampler: Arc<Sampler>) -> Self {
        self.sensors = Some(sampler);
        self
    }

    /// Share a flag that stops the run early when set
    pub fn with_cancel(mut self, flag: CancelFlag) -> Self {
        self.cancel = flag;
        self
    }

    fn detect_topology() -> Result<CoreTopology> {
        let logical = num_cpus::get();
        let physical = Self::detect_physical_cores(logical);
        
        // Detect P-cores and E-cores on Intel hybrid
        let (p_cores, e_cores, ht_pairs) = Self::detect_hybrid_topology();
        
        Ok(CoreTopology {
            total_logical: logical,
            total_physical: physical,
            performance_cores: p_cores,
            efficiency_cores: e_cores,
            ht_pairs,
            cache_topology: Vec::new(), // Would need platform-specific detection
        })
    }

    /// Physical core count (falls back to the logical count when unknown)
    fn detect_physical_cores(logical: usize) -> usize {
        sysinfo::System::new()
            .physical_core_count()
            .filter(|&n| n > 0)
            .unwrap_or(logical)
            .min(logical)
    }

    #[cfg(target_arch = "x86_64")]
    fn detect_hybrid_topology() -> (Vec<usize>, Vec<usize>, Vec<(usize, usize)>) {
        // Intel convention (Alder/Raptor/Arrow Lake): logical thread IDs 0..p are the
        // P-core threads (HT-interleaved where SMT exists), followed by E-core threads.
        let logical = num_cpus::get();
        let physical = Self::detect_physical_cores(logical);
        let (p_threads, e_threads) = hybrid_thread_counts();

        let p_end = p_threads.min(logical);
        // Exact per-CPU classification when CPUID can tell us, else the "P threads first" convention
        let (p_cores, e_cores): (Vec<usize>, Vec<usize>) = match crate::topology::cached_core_kinds() {
            Some(kinds) => {
                let ids = |want| kinds.iter().enumerate().filter(|(_, &k)| k == want).map(|(i, _)| i).collect();
                (ids(crate::topology::CoreKind::Performance), ids(crate::topology::CoreKind::Efficiency))
            }
            None => (
                (0..p_end).collect(),
                if e_threads > 0 { (p_end..logical).collect() } else { Vec::new() },
            ),
        };

        // HT pairs exist only when SMT is enabled (more logical threads than physical cores),
        // and Intel interleaves HT pairs within the P-core range: (0,1), (2,3), ...
        let mut ht_pairs = Vec::new();
        if logical > physical {
            for i in (0..p_end.max(2)).step_by(2) {
                if i + 1 < p_end {
                    ht_pairs.push((i, i + 1));
                }
            }
        }

        (p_cores, e_cores, ht_pairs)
    }

    #[cfg(not(target_arch = "x86_64"))]
    fn detect_hybrid_topology() -> (Vec<usize>, Vec<usize>, Vec<(usize, usize)>) {
        let logical = num_cpus::get();
        let mut p_cores = Vec::new();
        let mut ht_pairs = Vec::new();
        
        for i in 0..logical {
            p_cores.push(i);
        }
        for i in (0..logical).step_by(2) {
            if i + 1 < logical {
                ht_pairs.push((i, i + 1));
            }
        }
        
        (p_cores, Vec::new(), ht_pairs)
    }

    pub fn run(&mut self) -> Result<CpuBenchmarkSummary> {
        let system_info = crate::system_info::collect_system_info()?;
        let mut results = Vec::new();

        let combos: Vec<(WorkloadType, usize, AffinityMode)> = {
            let mut v = Vec::new();
            for &w in &self.config.workload_types {
                for &t in &self.config.thread_counts {
                    for &m in &self.config.affinity_modes {
                        if self.is_valid_combination(t, m) {
                            v.push((w, t, m));
                        }
                    }
                }
            }
            v
        };
        if combos.is_empty() {
            return Err(anyhow::anyhow!("No valid combination of workload, threads and cores to run"));
        }
        let mut combos = combos;
        if self.config.core_by_core {
            // stable sort: workload / thread order is kept within each core set
            let modes = self.config.affinity_modes.clone();
            combos.sort_by_key(|c| modes.iter().position(|m| *m == c.2).unwrap_or(usize::MAX));
        }
        progress::update(&self.progress, |p| {
            *p = progress::RunProgress { total: combos.len(), started: Some(std::time::Instant::now()), ..Default::default() }
        });

        for (workload, thread_count, affinity_mode) in combos {
            cancel::check(&self.cancel)?;
            let label = Self::mask_label(self.get_affinity_mask(thread_count, affinity_mode), affinity_mode);
            progress::update(&self.progress, |p| {
                p.title = format!("{:?} · {} thread(s) · {}", workload, thread_count, label);
                p.detail = "starting".into();
            });
            let result = self.run_workload(workload, thread_count, affinity_mode)?;
            progress::update(&self.progress, |p| p.done += 1);
            if let Some(partial) = &self.partial {
                partial.lock().unwrap().push(result.clone());
            }
            results.push(result);
        }

        progress::update(&self.progress, |p| p.finished = Some(std::time::Instant::now()));
        Ok(CpuBenchmarkSummary {
            results,
            config: self.config.clone(),
            system_info,
            timestamp: chrono::Utc::now().to_rfc3339(),
            core_topology: self.topology.clone(),
        })
    }

    /// "0-7,10" style description of the cores in `mask`; unpinned runs say so
    pub fn mask_label(mask: u64, mode: AffinityMode) -> String {
        if mode == AffinityMode::AllCores {
            return "All cores (OS scheduled)".to_string();
        }
        let ids: Vec<usize> = (0..64).filter(|&i| (mask >> i) & 1 == 1).collect();
        let mut parts = Vec::new();
        let mut i = 0;
        while i < ids.len() {
            let mut j = i;
            while j + 1 < ids.len() && ids[j + 1] == ids[j] + 1 {
                j += 1;
            }
            parts.push(if j > i { format!("{}-{}", ids[i], ids[j]) } else { ids[i].to_string() });
            i = j + 1;
        }
        if ids.len() == 1 { format!("Core {}", parts[0]) } else { format!("Cores {}", parts.join(",")) }
    }

    fn is_valid_combination(&self, thread_count: usize, affinity_mode: AffinityMode) -> bool {
        match affinity_mode {
            AffinityMode::PerformanceCores => {
                thread_count <= self.topology.performance_cores.len()
            }
            AffinityMode::EfficiencyCores => {
                thread_count <= self.topology.efficiency_cores.len()
            }
            AffinityMode::SingleCore => thread_count == 1,
            AffinityMode::CustomMask(mask) => thread_count >= 1 && thread_count <= mask.count_ones() as usize,
            AffinityMode::HyperThreadPairs => {
                thread_count <= self.topology.ht_pairs.len() * 2
            }
            _ => thread_count <= self.topology.total_logical,
        }
    }

    fn run_workload(
        &mut self,
        workload: WorkloadType,
        thread_count: usize,
        affinity_mode: AffinityMode,
    ) -> Result<CpuBenchmarkResult> {
        let core_mask = self.get_affinity_mask(thread_count, affinity_mode);
        if affinity_mode != AffinityMode::AllCores {
            // move the app's own threads off the core(s) this test is pinned to
            let cores: Vec<usize> = (0..64).filter(|&i| (core_mask >> i) & 1 == 1).collect();
            crate::app_core::keep_off(&cores);
        }
        let sensor_start = self.sensors.as_ref().map(|s| s.now_ms());
        let mut iteration_results = Vec::new();
        let mut total_ops = 0u64;
        let mut total_time_ns = 0u64;

        for iter in 0..self.config.iterations {
            cancel::check(&self.cancel)?;
            // Warmup
            if iter == 0 && self.config.warmup_seconds > 0 {
                progress::update(&self.progress, |p| p.detail = format!("warming up ({} s)", self.config.warmup_seconds));
                self.run_workload_internal(workload, thread_count, core_mask, affinity_mode, self.config.warmup_seconds)?;
            }
            
            // Actual measurement
            progress::update(&self.progress, |p| p.detail = format!("measuring run {}/{} ({} s each)", iter + 1, self.config.iterations, self.config.duration_seconds));
            let (ops, duration_ns, freq) = self.run_workload_internal(
                workload,
                thread_count,
                core_mask,
                affinity_mode,
                self.config.duration_seconds
            )?;
            
            iteration_results.push(IterationResult {
                iteration: iter,
                operations: ops,
                duration_ns,
                frequency_mhz: freq,
            });
            
            total_ops += ops;
            total_time_ns += duration_ns;
        }

        let avg_ops_per_sec = (total_ops as f64 / total_time_ns as f64) * 1_000_000_000.0;
        let avg_latency_ns = total_time_ns as f64 / total_ops as f64;
        let avg_freq = iteration_results.iter().map(|r| r.frequency_mhz).sum::<u64>() / iteration_results.len() as u64;

        let telemetry = match (&self.sensors, sensor_start) {
            (Some(s), Some(t0)) => s.record(
                "cpu",
                format!("CPU · {:?} · {} thread(s) · {}", workload, thread_count, Self::mask_label(core_mask, affinity_mode)),
                t0,
                s.now_ms(),
            ),
            _ => Telemetry::default(),
        };
        Ok(CpuBenchmarkResult {
            workload,
            thread_count,
            affinity_mode,
            core_mask,
            operations_per_second: avg_ops_per_sec,
            latency_ns: avg_latency_ns,
            instructions_per_cycle: None, // Would need PMU
            cycles_per_operation: None,
            // Sampler clocks (real, per CPU) beat the single nominal value sysinfo reports on Windows
            frequency_mhz: telemetry.cpu_freq_avg_mhz.map(|f| f as u64).filter(|f| *f > 0).unwrap_or(avg_freq),
            temperature_c: telemetry.cpu_temp_max_c,
            power_watts: None,
            iteration_results,
            cores: Self::mask_label(core_mask, affinity_mode),
            telemetry,
        })
    }

    fn get_affinity_mask(&self, thread_count: usize, affinity_mode: AffinityMode) -> u64 {
        match affinity_mode {
            AffinityMode::AllCores => {
                // Not pinned; the mask only records which cores were available
                let n = self.topology.total_logical.min(64);
                if n >= 64 { u64::MAX } else { (1u64 << n) - 1 }
            }
            AffinityMode::PerformanceCores => {
                let mut mask = 0u64;
                for &core in self.topology.performance_cores.iter().take(thread_count) {
                    if core < 64 {
                        mask |= 1u64 << core;
                    }
                }
                mask
            }
            AffinityMode::EfficiencyCores => {
                let mut mask = 0u64;
                for &core in self.topology.efficiency_cores.iter().take(thread_count) {
                    if core < 64 {
                        mask |= 1u64 << core;
                    }
                }
                mask
            }
            AffinityMode::SingleCore => {
                // Will be set per-thread in run_workload_internal
                1
            }
            AffinityMode::HyperThreadPairs => {
                let mut mask = 0u64;
                for (i, &(core1, core2)) in self.topology.ht_pairs.iter().enumerate() {
                    if i >= thread_count / 2 {
                        break;
                    }
                    if core1 < 64 { mask |= 1u64 << core1; }
                    if core2 < 64 { mask |= 1u64 << core2; }
                }
                mask
            }
            AffinityMode::CustomMask(mask) => mask,
        }
    }

    fn run_workload_internal(
        &self,
        workload: WorkloadType,
        thread_count: usize,
        core_mask: u64,
        affinity_mode: AffinityMode,
        duration_seconds: u64,
    ) -> Result<(u64, u64, u64)> {
        let mut handles = Vec::new();
        let duration_ns = duration_seconds * 1_000_000_000;
        let window_ticks = (duration_ns as u128 * self.timer.frequency() as u128 / 1_000_000_000) as u64;
        // Every worker (and this thread) waits here, so all of them start measuring together: a thread
        // that is created late (many threads, a busy machine) still gets its full window and no
        // thread ends up with zero work
        let start_line = Arc::new(std::sync::Barrier::new(thread_count + 1));

        for i in 0..thread_count {
            let workload = workload;
            let timer = HighResTimer::new();
            let core_id = self.get_core_for_thread(i, thread_count, core_mask, affinity_mode);
            let start_line = start_line.clone();

            let cancel_flag = self.cancel.clone();
            let pin = affinity_mode != AffinityMode::AllCores;
            let handle = thread::spawn(move || {
                // AllCores = leave scheduling to the OS; every other mode pins to a chosen core
                if pin {
                    crate::topology::pin_current_thread(core_id);
                }
                start_line.wait();
                let target_end = timer.now_ticks() + window_ticks;

                // at least one call, so a result can never be 0 operations
                let mut ops = 0u64;
                loop {
                    std::hint::black_box(Self::execute_workload(workload, ops));
                    ops += 1;
                    if timer.now_ticks() >= target_end || cancel::is_cancelled(&cancel_flag) {
                        break;
                    }
                }
                ops
            });
            handles.push(handle);
        }

        start_line.wait();
        let start_time = self.timer.now_ticks();
        let mut total_ops = 0u64;
        for handle in handles {
            total_ops += handle.join().map_err(|_| anyhow::anyhow!("CPU worker thread panicked"))?;
        }

        cancel::check(&self.cancel)?;
        let end_time = self.timer.now_ticks();
        let elapsed_ticks = end_time - start_time;
        let elapsed_ns = (elapsed_ticks as u128 * 1_000_000_000 / self.timer.frequency() as u128) as u64;
        
        // Get average frequency (simplified)
        let freq = self.get_current_frequency();

        Ok((total_ops, elapsed_ns, freq))
    }

    fn get_core_for_thread(&self, thread_idx: usize, _thread_count: usize, core_mask: u64, affinity_mode: AffinityMode) -> usize {
        match affinity_mode {
            AffinityMode::SingleCore => {
                // Round-robin through available cores
                let available: Vec<usize> = (0..64).filter(|&i| (core_mask >> i) & 1 == 1).collect();
                available[thread_idx % available.len()]
            }
            _ => {
                // Distribute threads across available cores in mask
                let available: Vec<usize> = (0..64).filter(|&i| (core_mask >> i) & 1 == 1).collect();
                if available.is_empty() { 0 } else { available[thread_idx % available.len()] }
            }
        }
    }

    fn get_current_frequency(&self) -> u64 {
        // Linux: average "cpu MHz" from /proc/cpuinfo is the most reliable source
        #[cfg(target_os = "linux")]
        if let Ok(info) = std::fs::read_to_string("/proc/cpuinfo") {
            let mhz: Vec<f64> = info
                .lines()
                .filter(|l| l.starts_with("cpu MHz"))
                .filter_map(|l| l.split(':').nth(1)?.trim().parse().ok())
                .collect();
            if !mhz.is_empty() {
                return (mhz.iter().sum::<f64>() / mhz.len() as f64) as u64;
            }
        }
        let mut sys = sysinfo::System::new();
        sys.refresh_cpu();
        sys.global_cpu_info().frequency()
    }

    fn execute_workload(workload: WorkloadType, seed: u64) -> u64 {
        let mut x = seed.wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(1);
        
        match workload {
            WorkloadType::IntegerAdd => {
                for _ in 0..1000 {
                    x = opaque(x).wrapping_add(0x123456789ABCDEF);
                }
            }
            WorkloadType::IntegerMul => {
                for _ in 0..1000 {
                    x = opaque(x).wrapping_mul(0x123456789ABCDEF);
                }
            }
            WorkloadType::IntegerDiv => {
                for _ in 0..1000 {
                    x = opaque(x).wrapping_div(0x123456789ABCDEF | 1);
                }
            }
            WorkloadType::FloatAdd => {
                let mut f = x as f64;
                for _ in 0..1000 {
                    f += 1.23456789;
                }
                x = f as u64;
            }
            WorkloadType::FloatMul => {
                let mut f = x as f64;
                for _ in 0..1000 {
                    f *= 1.23456789;
                }
                x = f as u64;
            }
            WorkloadType::FloatDiv => {
                let mut f = x as f64;
                for _ in 0..1000 {
                    f /= 1.23456789;
                }
                x = f as u64;
            }
            WorkloadType::FloatFma => {
                let mut f = x as f64;
                for _ in 0..1000 {
                    f = f.mul_add(1.23456789, 9.87654321);
                }
                x = f as u64;
            }
            WorkloadType::VectorAdd => {
                // Simulate AVX2 256-bit vector add (4 x f64)
                let mut v = [x as f64; 4];
                for _ in 0..250 {
                    v[0] += 1.0; v[1] += 2.0; v[2] += 3.0; v[3] += 4.0;
                }
                x = v[0] as u64;
            }
            WorkloadType::VectorMul => {
                let mut v = [x as f64; 4];
                for _ in 0..250 {
                    v[0] *= 1.1; v[1] *= 1.2; v[2] *= 1.3; v[3] *= 1.4;
                }
                x = v[0] as u64;
            }
            WorkloadType::VectorFma => {
                let mut v = [x as f64; 4];
                for _ in 0..250 {
                    v[0] = v[0].mul_add(1.1, 2.2);
                    v[1] = v[1].mul_add(1.2, 2.3);
                    v[2] = v[2].mul_add(1.3, 2.4);
                    v[3] = v[3].mul_add(1.4, 2.5);
                }
                x = v[0] as u64;
            }
            WorkloadType::MemoryCopy => {
                let mut src = [0u8; 64];
                let mut dst = [0u8; 64];
                src[0] = x as u8;
                for i in 0..1000 {
                    dst.copy_from_slice(std::hint::black_box(&src));
                    src[i % 64] = dst[(i + 1) % 64].wrapping_add(1);
                }
                x = x.wrapping_add(dst[0] as u64 + 1);
            }
            WorkloadType::MemoryLatency => {
                // Dependent loads through a small cyclic table (L1-resident)
                let mut table = [0u16; 1024];
                for (i, t) in table.iter_mut().enumerate() {
                    *t = ((i * 5 + 1) % 1024) as u16;
                }
                let mut idx = (x % 1024) as usize;
                for _ in 0..1000 {
                    idx = opaque(table[idx] as u64) as usize;
                }
                x = x.wrapping_add(idx as u64 + 1);
            }
            WorkloadType::BranchPrediction => {
                for i in 0..1000 {
                    if (x & 1) == 0 {
                        x = x.wrapping_add(i);
                    } else {
                        x = x.wrapping_sub(i);
                    }
                }
            }
            WorkloadType::CryptoAes => {
                // Simplified AES-like operations
                for _ in 0..1000 {
                    x ^= x.rotate_left(13);
                    x = x.wrapping_mul(0x9E3779B97F4A7C15);
                    x ^= x.rotate_right(7);
                }
            }
            WorkloadType::CryptoSha => {
                // Simplified SHA-like operations
                for _ in 0..1000 {
                    x = x.wrapping_add(0x5A827999);
                    x ^= x.rotate_right(2);
                    x = x.wrapping_mul(0x6ED9EBA1);
                }
            }
            WorkloadType::MixedWorkload => {
                for i in 0..1000 {
                    match i % 6 {
                        0 => x = x.wrapping_add(0x123456789ABCDEF),
                        1 => x = x.wrapping_mul(0x123456789ABCDEF),
                        2 => {
                            let f = (x as f64).mul_add(1.23456789, 9.87654321);
                            x = f as u64;
                        }
                        3 => x ^= x.rotate_left(13),
                        4 => x = x.wrapping_div(0x123456789ABCDEF | 1),
                        _ => {
                            x = opaque(x);
                        }
                    }
                }
            }
            WorkloadType::CompilationSim => {
                // Simulate compilation: lots of pointer chasing, branching, memory allocation
                let mut nodes = Vec::with_capacity(1000);
                for i in 0..1000 {
                    nodes.push(Box::new(i as u64));
                }
                for _ in 0..100 {
                    for node in &nodes {
                        x = x.wrapping_add(**node);
                    }
                }
            }
            WorkloadType::GameSim => {
                // Simulate game workload: math, physics, AI
                let mut pos = [x as f32, 0.0, 0.0];
                let mut vel = [1.0, 2.0, 3.0];
                for _ in 0..1000 {
                    // Physics update
                    pos[0] += vel[0] * 0.016;
                    pos[1] += vel[1] * 0.016;
                    pos[2] += vel[2] * 0.016;
                    
                    // Collision detection (simplified)
                    if pos[0] > 100.0 { vel[0] = -vel[0]; }
                    if pos[1] > 100.0 { vel[1] = -vel[1]; }
                    if pos[2] > 100.0 { vel[2] = -vel[2]; }
                    
                    // AI decision
                    x = x.wrapping_add((pos[0] * 1000.0) as u64);
                }
            }
        }
        
        x
    }
}

/// Hides a value from the optimiser (so a dependent chain is not folded away) while it stays in a
/// register. `std::hint::black_box` stores the value to the stack and loads it back, so a chain of
/// them measures store-to-load forwarding instead of the instruction: IntegerAdd then ran 5x slower
/// on Arrow Lake P-cores than on its E-cores, which forward stores differently.
#[inline(always)]
fn opaque(mut x: u64) -> u64 {
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    unsafe {
        std::arch::asm!("/* {0} */", inout(reg) x, options(nomem, nostack, preserves_flags));
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        x = std::hint::black_box(x);
    }
    x
}

/// Parse a Linux cpulist such as "0-7,16,18-19" into a count of CPUs
#[cfg(target_os = "linux")]
fn parse_cpulist_len(list: &str) -> usize {
    list.trim()
        .split(',')
        .filter(|p| !p.is_empty())
        .map(|part| match part.split_once('-') {
            Some((lo, hi)) => match (lo.trim().parse::<usize>(), hi.trim().parse::<usize>()) {
                (Ok(lo), Ok(hi)) if hi >= lo => hi - lo + 1,
                _ => 0,
            },
            None => usize::from(part.trim().parse::<usize>().is_ok()),
        })
        .sum()
}

/// Returns `(p_thread_count, e_thread_count)` for hybrid CPUs.
///
/// On Linux this reads the kernel's hybrid PMU lists (`/sys/devices/cpu_core`
/// and `/sys/devices/cpu_atom`). Elsewhere, or on non-hybrid CPUs, it reports
/// `(logical, 0)`. Intel convention is assumed: P threads have the lowest IDs.
pub fn hybrid_thread_counts() -> (usize, usize) {
    let logical = num_cpus::get();
    // CPUID per pinned core works on Windows and Linux
    if let Some(kinds) = crate::topology::cached_core_kinds() {
        let p = kinds.iter().filter(|&&k| k == crate::topology::CoreKind::Performance).count();
        let e = kinds.iter().filter(|&&k| k == crate::topology::CoreKind::Efficiency).count();
        if p > 0 && e > 0 {
            return (p, e);
        }
    }
    #[cfg(target_os = "linux")]
    {
        let read = |p: &str| std::fs::read_to_string(p).ok();
        if let (Some(core), Some(atom)) = (
            read("/sys/devices/cpu_core/cpus"),
            read("/sys/devices/cpu_atom/cpus"),
        ) {
            let (p, e) = (parse_cpulist_len(&core), parse_cpulist_len(&atom));
            if p > 0 && e > 0 && p + e <= logical {
                return (p, e);
            }
        }
    }
    (logical, 0)
}

#[allow(dead_code)]
/// Quick CPU test for GUI
pub fn quick_cpu_test(workload: WorkloadType, thread_count: usize, duration_sec: u64) -> Result<f64> {
    let config = CpuBenchmarkConfig {
        workload_types: vec![workload],
        thread_counts: vec![thread_count],
        affinity_modes: vec![AffinityMode::AllCores],
        duration_seconds: duration_sec,
        warmup_seconds: 1,
        iterations: 3,
        core_by_core: false,
    };
    
    let mut bench = CpuBenchmark::new(config)?;
    let summary = bench.run()?;
    
    summary
        .results
        .first()
        .map(|r| r.operations_per_second)
        .ok_or_else(|| anyhow::anyhow!("no valid CPU benchmark combination for {} threads", thread_count))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_topology_detection() {
        let topology = CpuBenchmark::detect_topology().unwrap();
        assert!(topology.total_logical > 0);
        assert!(topology.total_physical > 0);
    }

    #[test]
    fn test_core_by_core_runs_every_workload_on_one_core_first() {
        if num_cpus::get() < 2 {
            return;
        }
        let run = |core_by_core| {
            let mut b = CpuBenchmark::new(CpuBenchmarkConfig {
                workload_types: vec![WorkloadType::IntegerAdd, WorkloadType::IntegerMul],
                thread_counts: vec![1],
                affinity_modes: vec![AffinityMode::CustomMask(1), AffinityMode::CustomMask(2)],
                duration_seconds: 1,
                warmup_seconds: 0,
                iterations: 1,
                core_by_core,
            })
            .unwrap();
            b.run().unwrap().results.iter().map(|r| (r.workload, r.core_mask)).collect::<Vec<_>>()
        };
        use WorkloadType::*;
        assert_eq!(run(false), vec![(IntegerAdd, 1), (IntegerAdd, 2), (IntegerMul, 1), (IntegerMul, 2)]);
        assert_eq!(run(true), vec![(IntegerAdd, 1), (IntegerMul, 1), (IntegerAdd, 2), (IntegerMul, 2)]);
    }

    #[test]
    fn test_short_run_produces_ops() {
        let mut bench = CpuBenchmark::new(CpuBenchmarkConfig {
            workload_types: vec![WorkloadType::IntegerAdd, WorkloadType::MemoryCopy, WorkloadType::MemoryLatency],
            thread_counts: vec![1, 2],
            affinity_modes: vec![AffinityMode::AllCores],
            duration_seconds: 1,
            warmup_seconds: 0,
            iterations: 1,
            core_by_core: false,
        })
        .unwrap();
        let summary = bench.run().unwrap();
        assert_eq!(summary.results.len(), 3 * 2);
        for r in &summary.results {
            assert!(r.operations_per_second > 0.0 && r.latency_ns > 0.0, "{:?}", r.workload);
        }
    }

    #[test]
    fn test_every_workload_executes() {
        for wl in [
            WorkloadType::IntegerAdd, WorkloadType::IntegerMul, WorkloadType::IntegerDiv,
            WorkloadType::FloatAdd, WorkloadType::FloatMul, WorkloadType::FloatDiv,
            WorkloadType::FloatFma, WorkloadType::VectorAdd, WorkloadType::VectorMul,
            WorkloadType::VectorFma, WorkloadType::MemoryCopy, WorkloadType::MemoryLatency,
            WorkloadType::BranchPrediction, WorkloadType::CryptoAes, WorkloadType::CryptoSha,
            WorkloadType::MixedWorkload, WorkloadType::CompilationSim, WorkloadType::GameSim,
        ] {
            CpuBenchmark::execute_workload(wl, 7);
        }
    }

    #[test]
    fn test_invalid_combinations_are_skipped() {
        let bench = CpuBenchmark::new(CpuBenchmarkConfig::default()).unwrap();
        assert!(!bench.is_valid_combination(usize::MAX, AffinityMode::AllCores));
        assert!(!bench.is_valid_combination(2, AffinityMode::SingleCore));
    }

    #[test]
    fn integer_chains_compute_the_real_result() {
        // the barrier must not change the arithmetic: 1000 dependent adds / muls of a constant
        let x0 = 7u64.wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(1);
        let add = CpuBenchmark::execute_workload(WorkloadType::IntegerAdd, 7);
        assert_eq!(add, x0.wrapping_add(0x123456789ABCDEFu64.wrapping_mul(1000)));
        let mul = CpuBenchmark::execute_workload(WorkloadType::IntegerMul, 7);
        assert_eq!(mul, (0..1000).fold(x0, |x, _| x.wrapping_mul(0x123456789ABCDEF)));
        assert_eq!(opaque(42), 42);
    }

    #[test]
    fn test_integer_add() {
        let result = CpuBenchmark::execute_workload(WorkloadType::IntegerAdd, 0);
        assert_ne!(result, 0);
    }
}
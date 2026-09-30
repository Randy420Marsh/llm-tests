//! "Run all tests": one ordered job that runs every suite back to back.
//!
//! [`RunAllPlan`] holds what the user can tune (all defaults are the full profile), and
//! [`RunAllPlan::job`] turns it into a list of [`StepSpec`]s: the interactive mouse / keyboard trials
//! first (they need the user, everything after them runs on its own), then the timing suite, memory
//! (all cores, optionally each core on its own), CPU (all cores and each core on its own) and GPU.
//!
//! [`spawn`] executes a job on a worker thread. It reports into the same shared progress handles the
//! manual tabs use, so the tabs fill in live, and it never touches the GUI: the interactive trials are
//! a hand-shake through [`RunAllStatus`] (the worker asks, the GUI runs the trials and answers).

use anyhow::{anyhow, Result};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::cancel::{self, CancelFlag};
use crate::cpu_benchmark::{AffinityMode, CpuBenchmark, CpuBenchmarkConfig, CpuBenchmarkResult, CpuBenchmarkSummary, WorkloadType};
use crate::gpu_benchmark::{GpuBenchmark, GpuBenchmarkConfig, GpuBenchmarkResult, GpuBenchmarkSummary};
use crate::input_latency::{InputLatencyConfig, InputLatencySummary, InputTestMode};
use crate::input_test::InputKind;
use crate::memory_benchmark::{
    memory_needed_for, AccessPattern, MemoryBenchmark, MemoryBenchmarkConfig, ProgressHandle, SIZE_PRESETS, THREAD_PRESETS,
};
use crate::progress::{SharedProgress, SharedResults};
use crate::sensors::Sampler;

/// Every CPU workload the CPU tab offers
pub const ALL_WORKLOADS: [WorkloadType; 12] = [
    WorkloadType::GameSim,
    WorkloadType::CompilationSim,
    WorkloadType::MixedWorkload,
    WorkloadType::VectorFma,
    WorkloadType::FloatFma,
    WorkloadType::FloatDiv,
    WorkloadType::IntegerAdd,
    WorkloadType::IntegerMul,
    WorkloadType::CryptoAes,
    WorkloadType::BranchPrediction,
    WorkloadType::MemoryCopy,
    WorkloadType::MemoryLatency,
];

/// GPU workload sizes (elements per dispatch): 64 K to 64 M
pub const GPU_SIZES: [u64; 6] = [1 << 16, 1 << 18, 1 << 20, 1 << 22, 1 << 24, 1 << 26];

/// The tests of the timing suite that need nobody at the mouse
pub const AUTO_INPUT_MODES: [InputTestMode; 4] =
    [InputTestMode::MouseMove, InputTestMode::RawInput, InputTestMode::PollingRate, InputTestMode::Jitter];

// ---------------------------------------------------------------------------------------------
// plan
// ---------------------------------------------------------------------------------------------

/// What the user can tune before starting. `Default` is the full profile.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct RunAllPlan {
    /// Wait after the system data is collected, before the first test starts
    pub countdown_s: u64,

    /// Mouse-click and key-press trials with the user at the machine (run first)
    pub interactive_input: bool,
    /// Timer resolution / jitter / polling tests, no interaction needed
    pub auto_input: bool,
    pub input_each_core: bool,
    pub input_samples: u32,

    pub memory: bool,
    /// Also run every core on its own (1 thread pinned to it)
    pub memory_each_core: bool,
    /// Largest buffer to test (bytes); the default covers every size up to 1 GB
    pub mem_max_size: usize,
    /// All thread counts up to the number of logical CPUs (otherwise just 1 thread)
    pub mem_all_threads: bool,
    pub mem_iterations: u32,
    pub mem_warmup: u32,
    /// A test stops repeating after this long (at least 3 runs are always done)
    pub mem_time_limit_s: f64,

    pub cpu: bool,
    pub cpu_each_core: bool,
    pub cpu_run_s: u64,
    pub cpu_runs: u32,
    pub cpu_warmup_s: u64,
    /// Timings of the "each core on its own" pass: it runs every workload on every core, so with the
    /// all-core timings (110 s per test) a 24-thread CPU would need about 9 hours for it alone
    /// Per-core passes: every test on one core before the next (false = rotate cores between tests)
    pub core_by_core: bool,
    pub cpu_core_run_s: u64,
    pub cpu_core_runs: u32,
    pub cpu_core_warmup_s: u64,

    pub gpu: bool,
    /// Every GPU size keeps dispatching for at least this long so load, clocks and power register
    pub gpu_min_sample_ms: u64,
}

impl Default for RunAllPlan {
    fn default() -> Self {
        Self {
            countdown_s: 10,
            interactive_input: true,
            auto_input: true,
            input_each_core: true,
            input_samples: 500,
            memory: true,
            // Every core on its own for memory alone is thousands of tests, so it is opt-in
            memory_each_core: false,
            mem_max_size: 1 << 30,
            mem_all_threads: true,
            mem_iterations: 10,
            mem_warmup: 1,
            mem_time_limit_s: 5.0,
            cpu: true,
            cpu_each_core: true,
            cpu_run_s: 10,
            cpu_runs: 10,
            cpu_warmup_s: 10,
            core_by_core: false,
            cpu_core_run_s: 2,
            cpu_core_runs: 3,
            cpu_core_warmup_s: 1,
            gpu: true,
            gpu_min_sample_ms: 2000,
        }
    }
}

impl RunAllPlan {
    /// A few minutes instead of many hours: to check that everything works and how the results look
    pub fn quick() -> Self {
        Self {
            countdown_s: 3,
            input_each_core: false,
            input_samples: 100,
            mem_max_size: 16 << 20,
            mem_all_threads: false,
            mem_iterations: 3,
            mem_warmup: 0,
            mem_time_limit_s: 0.3,
            cpu_each_core: false,
            cpu_run_s: 2,
            cpu_runs: 2,
            cpu_warmup_s: 1,
            cpu_core_run_s: 1,
            cpu_core_runs: 1,
            cpu_core_warmup_s: 0,
            gpu_min_sample_ms: 400,
            ..Self::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        !(self.interactive_input || self.auto_input || self.memory || self.cpu || self.gpu)
    }
}

/// What the machine offers, used to size the job
#[derive(Debug, Clone)]
pub struct HostInfo {
    pub logical_cores: usize,
    /// Free RAM in bytes, when known; buffers that would not fit are left out
    pub available_ram: Option<usize>,
}

impl HostInfo {
    pub fn detect(available_ram: Option<usize>) -> Self {
        Self { logical_cores: num_cpus::get().max(1), available_ram }
    }
}

// ---------------------------------------------------------------------------------------------
// job
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum StepSpec {
    /// The user clicks / presses keys (10 trials); the GUI runs it
    Interactive(InputKind),
    /// The timing suite; one pass per entry (`None` = unpinned, `Some(core)` = pinned)
    InputSuite { base: InputLatencyConfig, passes: Vec<Option<usize>> },
    Memory { label: String, config: MemoryBenchmarkConfig },
    Cpu { label: String, config: CpuBenchmarkConfig },
    Gpu { config: GpuBenchmarkConfig },
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Estimate {
    /// Typical duration
    pub expected_s: f64,
    /// Longest it can reasonably take (each memory test used its whole time limit)
    pub worst_s: f64,
}

impl std::ops::AddAssign for Estimate {
    fn add_assign(&mut self, o: Estimate) {
        self.expected_s += o.expected_s;
        self.worst_s += o.worst_s;
    }
}

impl StepSpec {
    pub fn label(&self) -> String {
        match self {
            StepSpec::Interactive(k) => format!("Input latency: {} (you)", k.label().to_lowercase()),
            StepSpec::InputSuite { passes, base } => {
                format!("Input timing tests ({} tests × {} core pass(es))", base.test_modes.len(), passes.len())
            }
            StepSpec::Memory { label, config } => format!("Memory: {} ({} tests)", label, config.test_count()),
            StepSpec::Cpu { label, config } => {
                let n = config.workload_types.len() * config.affinity_modes.len() * config.thread_counts.len();
                format!("CPU: {} ({} tests)", label, n)
            }
            StepSpec::Gpu { config } => format!("GPU (Vulkan): {} sizes", config.workload_sizes.len()),
        }
    }

    pub fn estimate(&self) -> Estimate {
        match self {
            StepSpec::Interactive(_) => Estimate { expected_s: 40.0, worst_s: 120.0 },
            StepSpec::InputSuite { base, passes } => {
                let n = (base.test_modes.len() * passes.len()) as f64;
                Estimate { expected_s: n * 2.0, worst_s: n * 6.0 }
            }
            StepSpec::Memory { config, .. } => {
                let tests = config.test_count() as f64;
                let limit = if config.time_budget_ms > 0 { config.time_budget_ms as f64 / 1000.0 } else { 10.0 };
                // a test ends at its time limit (plus the run in flight); many end far sooner
                Estimate { expected_s: tests * (limit * 0.5 + 0.2), worst_s: tests * (limit * 1.5 + 0.5) }
            }
            StepSpec::Cpu { config, .. } => {
                let tests = (config.workload_types.len() * config.affinity_modes.len() * config.thread_counts.len()) as f64;
                let per = config.warmup_seconds as f64 + (config.duration_seconds * config.iterations as u64) as f64;
                Estimate { expected_s: tests * (per + 0.3), worst_s: tests * (per * 1.05 + 1.0) }
            }
            StepSpec::Gpu { config } => {
                let n = config.workload_sizes.len() as f64;
                let per = config.min_sample_ms as f64 / 1000.0 + 1.0;
                Estimate { expected_s: n * per, worst_s: n * (per * 1.5 + 2.0) }
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RunAllJob {
    pub steps: Vec<StepSpec>,
    /// Buffer sizes left out because they would not fit in free RAM
    pub skipped_sizes: Vec<usize>,
    /// Cores 64 and up cannot be pinned individually by the CPU suite
    pub unpinnable_cores: usize,
}

impl RunAllJob {
    pub fn estimate(&self) -> Estimate {
        let mut e = Estimate::default();
        for s in &self.steps {
            e += s.estimate();
        }
        e
    }
}

fn thread_counts(host: &HostInfo, all: bool) -> Vec<usize> {
    if !all {
        return vec![1];
    }
    let mut v: Vec<usize> = THREAD_PRESETS.iter().copied().filter(|&t| t <= host.logical_cores).collect();
    v.push(host.logical_cores);
    v.sort_unstable();
    v.dedup();
    v
}

impl RunAllPlan {
    /// Memory config for all cores (`each_core = false`) or one core at a time; also returns the
    /// sizes that were dropped because they do not fit in free RAM
    pub fn memory_config(&self, host: &HostInfo, each_core: bool) -> (MemoryBenchmarkConfig, Vec<usize>) {
        let patterns = AccessPattern::all_default();
        let (mut sizes, mut skipped) = (Vec::new(), Vec::new());
        for &s in SIZE_PRESETS.iter().filter(|&&s| s <= self.mem_max_size) {
            match host.available_ram {
                Some(ram) if memory_needed_for(s, &patterns) > ram / 10 * 8 => skipped.push(s),
                _ => sizes.push(s),
            }
        }
        sizes.sort_unstable();
        skipped.sort_unstable();
        let ids: Vec<usize> = (0..host.logical_cores).collect();
        let config = MemoryBenchmarkConfig {
            sizes,
            iterations: self.mem_iterations.max(1),
            warmup_iterations: self.mem_warmup,
            patterns,
            thread_counts: if each_core { vec![1] } else { thread_counts(host, self.mem_all_threads) },
            use_huge_pages: false,
            core_ids: if each_core { ids.clone() } else { Vec::new() },
            core_label: if each_core {
                format!("Each core one at a time (0-{})", host.logical_cores - 1)
            } else {
                "All cores (OS scheduled)".to_string()
            },
            time_budget_ms: (self.mem_time_limit_s.max(0.0) * 1000.0) as u64,
            per_core: each_core,
            core_by_core: self.core_by_core,
        };
        (config, skipped)
    }

    pub fn cpu_config(&self, host: &HostInfo, each_core: bool) -> (CpuBenchmarkConfig, usize) {
        let pinnable = host.logical_cores.min(64);
        let (thread_counts, affinity_modes) = if each_core {
            (vec![1], (0..pinnable).map(|c| AffinityMode::CustomMask(1u64 << c)).collect())
        } else {
            (vec![host.logical_cores], vec![AffinityMode::AllCores])
        };
        (
            CpuBenchmarkConfig {
                workload_types: ALL_WORKLOADS.to_vec(),
                thread_counts,
                affinity_modes,
                duration_seconds: if each_core { self.cpu_core_run_s } else { self.cpu_run_s }.max(1),
                warmup_seconds: if each_core { self.cpu_core_warmup_s } else { self.cpu_warmup_s },
                iterations: if each_core { self.cpu_core_runs } else { self.cpu_runs }.max(1),
                core_by_core: self.core_by_core,
            },
            host.logical_cores - pinnable,
        )
    }

    pub fn gpu_config(&self) -> GpuBenchmarkConfig {
        GpuBenchmarkConfig {
            workload_sizes: GPU_SIZES.to_vec(),
            iterations: 100,
            warmup_iterations: 10,
            min_sample_ms: self.gpu_min_sample_ms,
        }
    }

    /// The ordered steps. The steps that need the user come first so the rest can run unattended.
    pub fn job(&self, host: &HostInfo) -> RunAllJob {
        let mut job = RunAllJob::default();
        if self.interactive_input {
            job.steps.push(StepSpec::Interactive(InputKind::MouseClick));
            job.steps.push(StepSpec::Interactive(InputKind::KeyPress));
        }
        if self.auto_input {
            let mut passes = vec![None];
            if self.input_each_core {
                passes.extend((0..host.logical_cores).map(Some));
            }
            let base = InputLatencyConfig {
                test_modes: AUTO_INPUT_MODES.to_vec(),
                sample_count: self.input_samples.max(10),
                warmup_samples: 0,
                ..InputLatencyConfig::default()
            };
            job.steps.push(StepSpec::InputSuite { base, passes });
        }
        if self.memory {
            let (config, skipped) = self.memory_config(host, false);
            job.skipped_sizes = skipped;
            job.steps.push(StepSpec::Memory { label: "all cores".into(), config });
            if self.memory_each_core {
                let (config, _) = self.memory_config(host, true);
                job.steps.push(StepSpec::Memory { label: "each core on its own".into(), config });
            }
        }
        if self.cpu {
            let (config, _) = self.cpu_config(host, false);
            job.steps.push(StepSpec::Cpu { label: "all cores".into(), config });
            if self.cpu_each_core {
                let (config, unpinnable) = self.cpu_config(host, true);
                job.unpinnable_cores = unpinnable;
                job.steps.push(StepSpec::Cpu { label: "each core on its own".into(), config });
            }
        }
        if self.gpu {
            job.steps.push(StepSpec::Gpu { config: self.gpu_config() });
        }
        job
    }
}

/// 67108864 -> "64 M", 65536 -> "64 K"
fn humanize_count(n: u64) -> String {
    if n >= 1 << 20 { format!("{} M", n >> 20) } else if n >= 1 << 10 { format!("{} K", n >> 10) } else { n.to_string() }
}

/// "2 h 05 min", "14 min", "45 s"
pub fn fmt_span(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    match (s / 3600, (s % 3600) / 60) {
        (0, 0) => format!("{} s", s),
        (0, m) => format!("{} min", m),
        (h, m) => format!("{} h {:02} min", h, m),
    }
}

// ---------------------------------------------------------------------------------------------
// execution
// ---------------------------------------------------------------------------------------------

/// Shared handles the suites publish into (the same ones the manual tabs read)
#[derive(Clone)]
pub struct RunAllHandles {
    pub mem: ProgressHandle,
    pub cpu_progress: SharedProgress,
    pub cpu_partial: SharedResults<CpuBenchmarkResult>,
    pub gpu_progress: SharedProgress,
    pub gpu_partial: SharedResults<GpuBenchmarkResult>,
    pub input_progress: SharedProgress,
}

/// What the worker tells the GUI, and what the GUI tells the worker
#[derive(Default)]
pub struct RunAllStatus {
    /// 0-based index of the step being run
    pub step: usize,
    pub steps_total: usize,
    pub label: String,
    pub step_started: Option<Instant>,
    /// The worker is waiting for the user to do this trial run
    pub waiting_for_user: Option<InputKind>,
    /// Set by the GUI when the trials are finished (or skipped)
    pub user_done: bool,
    /// Lines for the log panel, drained by the GUI
    pub log: Vec<String>,
    pub output: Option<RunAllOutput>,
}

pub type SharedStatus = Arc<Mutex<RunAllStatus>>;

impl RunAllStatus {
    fn say(&mut self, msg: impl Into<String>) {
        self.log.push(msg.into());
    }
}

/// Everything the GUI needs after the run to save and show it
#[derive(Default)]
pub struct RunAllOutput {
    pub input_suite: Option<InputLatencySummary>,
    pub cpu_summary: Option<CpuBenchmarkSummary>,
    pub gpu_summary: Option<GpuBenchmarkSummary>,
    /// One per memory / CPU step, in run order (the results are in the shared handles)
    pub memory_configs: Vec<MemoryBenchmarkConfig>,
    pub cpu_configs: Vec<CpuBenchmarkConfig>,
    pub gpu_config: Option<GpuBenchmarkConfig>,
    /// Steps that failed (the rest still ran)
    pub errors: Vec<String>,
    pub cancelled: bool,
    pub steps_done: usize,
    pub steps_total: usize,
}

fn with_status<R>(status: &SharedStatus, f: impl FnOnce(&mut RunAllStatus) -> R) -> R {
    let mut g = status.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut g)
}

/// Ask the running worker to carry on (the GUI calls this once the trials are done or skipped)
pub fn finish_user_step(status: &SharedStatus) {
    with_status(status, |s| s.user_done = true);
}

/// Run every step in order on the calling thread; a failing step is recorded and the rest still run
pub fn execute(
    job: &RunAllJob,
    handles: &RunAllHandles,
    sampler: &Arc<Sampler>,
    cancel_flag: &CancelFlag,
    status: &SharedStatus,
) -> RunAllOutput {
    let mut out = RunAllOutput { steps_total: job.steps.len(), ..Default::default() };
    let mut memory_passes = 0;
    with_status(status, |s| s.steps_total = job.steps.len());

    for (i, step) in job.steps.iter().enumerate() {
        if cancel::is_cancelled(cancel_flag) {
            out.cancelled = true;
            break;
        }
        let label = step.label();
        let began = Instant::now();
        with_status(status, |s| {
            s.step = i;
            s.label = label.clone();
            s.step_started = Some(began);
            s.say(format!("Step {}/{}: {}", i + 1, job.steps.len(), label));
        });

        let result: Result<()> = match step {
            StepSpec::Interactive(kind) => wait_for_user(*kind, cancel_flag, status),
            StepSpec::InputSuite { base, passes } => {
                crate::input_latency::run_passes(base.clone(), passes.clone(), cancel_flag.clone(), handles.input_progress.clone(), sampler.clone())
                    .map(|summary| out.input_suite = Some(summary))
            }
            StepSpec::Memory { config, .. } => {
                let r = MemoryBenchmark::new(config.clone())
                    .with_cancel(cancel_flag.clone())
                    .with_progress(handles.mem.clone())
                    .with_sensors(sampler.clone())
                    .carry_over(memory_passes > 0)
                    .run();
                memory_passes += 1;
                out.memory_configs.push(config.clone());
                r.map(|_| ())
            }
            StepSpec::Cpu { config, .. } => {
                out.cpu_configs.push(config.clone());
                CpuBenchmark::new(config.clone()).and_then(|b| {
                    let mut b = b
                        .with_cancel(cancel_flag.clone())
                        .with_progress(handles.cpu_progress.clone(), handles.cpu_partial.clone())
                        .with_sensors(sampler.clone());
                    b.run()
                })
                .map(|summary| out.cpu_summary = Some(summary))
            }
            StepSpec::Gpu { config } => {
                out.gpu_config = Some(config.clone());
                // Vulkan objects are created and destroyed inside this call
                GpuBenchmark::new(config.clone())
                    .and_then(|b| {
                        b.with_cancel(cancel_flag.clone())
                            .with_progress(handles.gpu_progress.clone(), handles.gpu_partial.clone())
                            .with_sensors(sampler.clone())
                            .run_owned()
                    })
                    .map(|summary| {
                        if !summary.skipped_sizes.is_empty() {
                            let list: Vec<String> = summary.skipped_sizes.iter().map(|s| humanize_count(*s)).collect();
                            with_status(status, |st| st.say(format!("GPU: this device cannot run the size(s) {}; the others were tested", list.join(", "))));
                        }
                        out.gpu_summary = Some(summary);
                    })
            }
        };

        // Whatever happened, this step's progress panel must stop counting
        match step {
            StepSpec::Memory { .. } => {
                if let Ok(mut p) = handles.mem.lock() {
                    p.finished.get_or_insert_with(Instant::now);
                }
            }
            StepSpec::Cpu { .. } => crate::progress::finish(&handles.cpu_progress),
            StepSpec::Gpu { .. } => crate::progress::finish(&handles.gpu_progress),
            StepSpec::InputSuite { .. } => crate::progress::finish(&handles.input_progress),
            StepSpec::Interactive(_) => {}
        }

        match result {
            Ok(()) => {
                out.steps_done += 1;
                with_status(status, |s| s.say(format!("Finished: {} in {}", label, fmt_span(began.elapsed().as_secs_f64()))));
            }
            Err(e) if cancel::is_cancel_error(&e) => {
                out.cancelled = true;
                with_status(status, |s| s.say(format!("Stopped during: {}", label)));
                break;
            }
            Err(e) => {
                let msg = format!("{}: {}", label, e);
                with_status(status, |s| s.say(format!("Step failed, carrying on with the next one. {}", msg)));
                out.errors.push(msg);
            }
        }
    }
    out
}

fn wait_for_user(kind: InputKind, cancel_flag: &CancelFlag, status: &SharedStatus) -> Result<()> {
    with_status(status, |s| {
        s.user_done = false;
        s.waiting_for_user = Some(kind);
    });
    let result = loop {
        if cancel::is_cancelled(cancel_flag) {
            break Err(anyhow!(cancel::CANCELLED_MSG));
        }
        if with_status(status, |s| s.user_done) {
            break Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    with_status(status, |s| s.waiting_for_user = None);
    result
}

/// Run the job on a worker thread; the result lands in `status.output` when it ends
pub fn spawn(
    job: RunAllJob,
    handles: RunAllHandles,
    sampler: Arc<Sampler>,
    cancel_flag: CancelFlag,
    status: SharedStatus,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let out = execute(&job, &handles, &sampler, &cancel_flag, &status);
        with_status(&status, |s| s.output = Some(out));
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress;

    fn host(cores: usize) -> HostInfo {
        HostInfo { logical_cores: cores, available_ram: Some(64 << 30) }
    }

    #[test]
    fn full_plan_matches_the_requested_profile() {
        let plan = RunAllPlan::default();
        let job = plan.job(&host(24));
        let labels: Vec<String> = job.steps.iter().map(|s| s.label()).collect();
        // the interactive trials come first, then everything that runs unattended
        assert!(matches!(job.steps[0], StepSpec::Interactive(InputKind::MouseClick)));
        assert!(matches!(job.steps[1], StepSpec::Interactive(InputKind::KeyPress)));
        assert!(matches!(job.steps[2], StepSpec::InputSuite { .. }));
        assert_eq!(job.steps.len(), 2 + 1 + 1 + 2 + 1, "{:?}", labels);

        let StepSpec::Memory { config, .. } = &job.steps[3] else { panic!("memory expected: {:?}", labels) };
        assert_eq!(config.sizes.len(), 18, "every buffer size");
        assert_eq!(config.patterns.len(), AccessPattern::all_default().len(), "every access pattern");
        assert_eq!((config.iterations, config.warmup_iterations, config.time_budget_ms), (10, 1, 5000));
        assert_eq!(config.thread_counts, vec![1, 2, 4, 8, 12, 16, 24], "every thread count up to the 24 logical CPUs");
        assert!(config.core_ids.is_empty() && !config.per_core, "all cores, OS decides");

        let StepSpec::Cpu { config, .. } = &job.steps[4] else { panic!("cpu expected") };
        assert_eq!(config.workload_types.len(), 12);
        assert_eq!((config.duration_seconds, config.iterations, config.warmup_seconds), (10, 10, 10));
        assert_eq!((config.thread_counts.clone(), config.affinity_modes.clone()), (vec![24], vec![AffinityMode::AllCores]));

        let StepSpec::Cpu { config, .. } = &job.steps[5] else { panic!("per-core cpu expected") };
        assert_eq!(config.thread_counts, vec![1]);
        assert_eq!(config.affinity_modes.len(), 24, "each core on its own");
        assert_eq!(config.affinity_modes[5], AffinityMode::CustomMask(1 << 5));
        // the per-core pass has its own short timings: 12 workloads × 24 cores must not take 9 hours
        assert_eq!((config.duration_seconds, config.iterations, config.warmup_seconds), (2, 3, 1));
        let per_core = StepSpec::Cpu { label: String::new(), config: config.clone() }.estimate().expected_s;
        assert!(per_core < 3600.0, "per-core CPU pass on 24 threads: {}", fmt_span(per_core));
        assert!(matches!(job.steps[6], StepSpec::Gpu { .. }));
    }

    #[test]
    fn input_suite_sweeps_every_core_after_the_unpinned_pass() {
        let job = RunAllPlan::default().job(&host(4));
        let StepSpec::InputSuite { base, passes } = &job.steps[2] else { panic!() };
        assert_eq!(passes, &vec![None, Some(0), Some(1), Some(2), Some(3)]);
        assert_eq!(base.test_modes, AUTO_INPUT_MODES.to_vec());
        // none of the automatic tests is the human click / key test
        assert!(!base.test_modes.contains(&InputTestMode::MouseClick) && !base.test_modes.contains(&InputTestMode::KeyPress));
    }

    #[test]
    fn memory_each_core_is_an_extra_step_when_asked() {
        let plan = RunAllPlan { memory_each_core: true, ..RunAllPlan::default() };
        let job = plan.job(&host(8));
        let mem: Vec<_> = job.steps.iter().filter_map(|s| if let StepSpec::Memory { config, .. } = s { Some(config) } else { None }).collect();
        assert_eq!(mem.len(), 2);
        assert!(mem[1].per_core && mem[1].core_ids == (0..8).collect::<Vec<_>>());
        assert_eq!(mem[1].core_groups().len(), 8);
        assert_eq!(mem[1].effective_thread_counts(), vec![1]);
    }

    #[test]
    fn buffers_that_do_not_fit_in_ram_are_dropped() {
        let h = HostInfo { logical_cores: 4, available_ram: Some(2 << 30) };
        let (cfg, skipped) = RunAllPlan::default().memory_config(&h, false);
        // 3.125 x size must stay under 80 % of the free RAM: on 2 GB, 512 MB fits and 1 GB does not
        assert_eq!(cfg.sizes.last(), Some(&(512 << 20)));
        assert_eq!(skipped, vec![1 << 30]);
        let (small, skipped) = RunAllPlan::default().memory_config(&HostInfo { logical_cores: 4, available_ram: Some(1 << 30) }, false);
        assert_eq!((small.sizes.last(), skipped), (Some(&(256 << 20)), vec![512 << 20, 1 << 30]));
        let (all, none) = RunAllPlan::default().memory_config(&HostInfo { logical_cores: 4, available_ram: None }, false);
        assert_eq!((all.sizes.len(), none.len()), (18, 0));
    }

    #[test]
    fn thread_counts_follow_the_cpu() {
        let h = |n| HostInfo { logical_cores: n, available_ram: None };
        assert_eq!(thread_counts(&h(6), true), vec![1, 2, 4, 6]);
        assert_eq!(thread_counts(&h(32), true), vec![1, 2, 4, 8, 12, 16, 24, 32]);
        assert_eq!(thread_counts(&h(64), true).last(), Some(&64));
        assert_eq!(thread_counts(&h(16), false), vec![1]);
    }

    #[test]
    fn cores_beyond_63_cannot_be_pinned_individually() {
        let plan = RunAllPlan::default();
        let (cfg, unpinnable) = plan.cpu_config(&host(72), true);
        assert_eq!((cfg.affinity_modes.len(), unpinnable), (64, 8));
        assert_eq!(plan.job(&host(72)).unpinnable_cores, 8);
    }

    #[test]
    fn estimate_follows_the_settings() {
        let plan = RunAllPlan::default();
        let h = host(24);
        let (cfg, _) = plan.cpu_config(&h, false);
        // 12 workloads x (10 s warmup + 10 runs x 10 s)
        let e = StepSpec::Cpu { label: String::new(), config: cfg }.estimate();
        assert!((e.expected_s - 12.0 * 110.3).abs() < 0.01, "{}", e.expected_s);
        let quick = RunAllPlan::quick().job(&h).estimate();
        let full = plan.job(&h).estimate();
        assert!(quick.worst_s < 20.0 * 60.0, "quick preset is a few minutes: {}", fmt_span(quick.worst_s));
        assert!(full.expected_s > 3600.0 && full.expected_s < 3.0 * 3600.0, "the full profile is one to three hours: {}", fmt_span(full.expected_s));
        assert!(full.worst_s >= full.expected_s);
    }

    #[test]
    fn span_formatting() {
        assert_eq!(fmt_span(45.0), "45 s");
        assert_eq!(fmt_span(14.0 * 60.0), "14 min");
        assert_eq!(fmt_span(3.0 * 3600.0 + 125.0), "3 h 02 min");
    }

    #[test]
    fn empty_plan_is_detected() {
        let plan = RunAllPlan { interactive_input: false, auto_input: false, memory: false, cpu: false, gpu: false, ..RunAllPlan::default() };
        assert!(plan.is_empty() && plan.job(&host(4)).steps.is_empty());
        assert!(!RunAllPlan::default().is_empty());
    }

    fn handles() -> RunAllHandles {
        RunAllHandles {
            mem: crate::memory_benchmark::new_progress(),
            cpu_progress: progress::new(),
            cpu_partial: progress::new_results(),
            gpu_progress: progress::new(),
            gpu_partial: progress::new_results(),
            input_progress: progress::new(),
        }
    }

    fn tiny_job() -> RunAllJob {
        let mem = |label: &str, each_core: bool| StepSpec::Memory {
            label: label.into(),
            config: MemoryBenchmarkConfig {
                sizes: vec![64 << 10],
                patterns: vec![AccessPattern::SequentialRead, AccessPattern::PointerChase],
                iterations: 3,
                warmup_iterations: 0,
                thread_counts: vec![1],
                time_budget_ms: 0,
                core_ids: if each_core { vec![0] } else { Vec::new() },
                per_core: each_core,
                core_label: label.into(),
                ..MemoryBenchmarkConfig::default()
            },
        };
        RunAllJob {
            steps: vec![
                StepSpec::Interactive(InputKind::MouseClick),
                StepSpec::InputSuite {
                    base: InputLatencyConfig { test_modes: vec![InputTestMode::Jitter], sample_count: 50, warmup_samples: 0, ..InputLatencyConfig::default() },
                    passes: vec![None],
                },
                mem("all cores", false),
                mem("one core", true),
                StepSpec::Cpu {
                    label: "all cores".into(),
                    config: CpuBenchmarkConfig {
                        workload_types: vec![WorkloadType::IntegerAdd],
                        thread_counts: vec![1],
                        affinity_modes: vec![AffinityMode::AllCores],
                        duration_seconds: 1,
                        warmup_seconds: 0,
                        iterations: 1,
                        core_by_core: false,
                    },
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn a_whole_job_runs_in_order_and_collects_every_pass() {
        let (h, status, cancel_flag) = (handles(), Arc::new(Mutex::new(RunAllStatus::default())), cancel::new_flag());
        let sampler = Sampler::start(Duration::from_millis(100));
        let worker = spawn(tiny_job(), h.clone(), sampler, cancel_flag, status.clone());

        // The worker asks for the interactive trials first and waits until the GUI answers
        let started = Instant::now();
        loop {
            let asking = with_status(&status, |s| s.waiting_for_user);
            if asking == Some(InputKind::MouseClick) {
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(20), "the worker never asked for the mouse trials");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(h.mem.lock().unwrap().completed.is_empty(), "nothing else may run before the user step is done");
        finish_user_step(&status);
        worker.join().unwrap();

        let out = with_status(&status, |s| s.output.take()).expect("output");
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert!(!out.cancelled);
        assert_eq!((out.steps_done, out.steps_total), (5, 5));
        assert_eq!(out.input_suite.as_ref().map(|s| s.results.len()), Some(1));
        assert_eq!(out.memory_configs.len(), 2);
        // both memory passes end up in one list: 2 patterns each
        let mem = h.mem.lock().unwrap();
        assert_eq!((mem.completed.len(), mem.done_tests, mem.total_tests), (4, 4, 4));
        assert_eq!(mem.completed[0].cores, "all cores");
        assert_eq!(mem.completed[3].cores, "Core 0", "the per-core pass is labelled by core");
        assert_eq!(h.cpu_partial.lock().unwrap().len(), 1);
        assert!(out.cpu_summary.is_some());
        let log = with_status(&status, |s| s.log.join("\n"));
        assert!(log.contains("Step 1/5") && log.contains("Step 5/5"), "{}", log);
    }

    #[test]
    fn stop_during_the_user_step_ends_the_job_cleanly() {
        let (h, status, cancel_flag) = (handles(), Arc::new(Mutex::new(RunAllStatus::default())), cancel::new_flag());
        let sampler = Sampler::start(Duration::from_millis(100));
        let worker = spawn(tiny_job(), h.clone(), sampler, cancel_flag.clone(), status.clone());
        while with_status(&status, |s| s.waiting_for_user).is_none() {
            std::thread::sleep(Duration::from_millis(20));
        }
        cancel_flag.store(true, std::sync::atomic::Ordering::Relaxed);
        worker.join().unwrap();
        let out = with_status(&status, |s| s.output.take()).unwrap();
        assert!(out.cancelled && out.steps_done == 0);
        assert!(h.mem.lock().unwrap().completed.is_empty());
    }

    #[test]
    fn a_failing_step_does_not_stop_the_rest() {
        let (h, status, cancel_flag) = (handles(), Arc::new(Mutex::new(RunAllStatus::default())), cancel::new_flag());
        let sampler = Sampler::start(Duration::from_millis(100));
        let mut job = tiny_job();
        job.steps.remove(0); // no user step
        job.steps.remove(0); // no input suite
        // a memory step with nothing to run fails immediately
        job.steps.insert(0, StepSpec::Memory { label: "broken".into(), config: MemoryBenchmarkConfig { sizes: vec![], ..MemoryBenchmarkConfig::default() } });
        let out = execute(&job, &h, &sampler, &cancel_flag, &status);
        assert_eq!(out.errors.len(), 1, "{:?}", out.errors);
        assert!(out.errors[0].contains("broken"));
        assert_eq!(out.steps_done, 3, "the other memory passes and the CPU step still ran");
        assert_eq!(h.mem.lock().unwrap().completed.len(), 4);
    }
}

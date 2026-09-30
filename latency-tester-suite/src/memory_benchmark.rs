//! Memory latency benchmarks with different access patterns and sizes
//! Tests sequential, random, strided, and pointer-chasing access patterns

use anyhow::Result;
use rand::{Rng, SeedableRng};
use rand::rngs::StdRng;
use serde::{Deserialize, Serialize};
use crate::cancel::{self, CancelFlag};
use std::sync::{Arc, Barrier, Mutex};
use crate::sensors::{Sampler, Telemetry};
use crate::timer::HighResTimer;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryBenchmarkConfig {
    pub sizes: Vec<usize>,           // Buffer sizes in bytes
    pub iterations: u32,             // Iterations per test
    pub warmup_iterations: u32,      // Warmup iterations
    pub patterns: Vec<AccessPattern>, // Access patterns to test
    pub thread_counts: Vec<usize>,   // Thread counts to test
    pub use_huge_pages: bool,        // Use huge pages if available
    /// Logical CPUs the worker threads are pinned to (worker i -> core_ids[i % len]).
    /// Empty = let the OS scheduler decide.
    #[serde(default)]
    pub core_ids: Vec<usize>,
    /// Human-readable description of the core selection ("P-cores 0-7", "All cores", ...)
    #[serde(default)]
    pub core_label: String,
    /// Stop repeating a test once it has used this much wall time (0 = no limit).
    /// Always completes at least MIN_ITERATIONS runs.
    #[serde(default = "default_time_budget_ms")]
    pub time_budget_ms: u64,
    /// Run every selected core on its own (1 thread pinned to that core) so a slow, hot or
    /// faulty core stands out. Thread counts are ignored in this mode.
    #[serde(default)]
    pub per_core: bool,
    /// Per-core mode only: run every test on one core before moving to the next (false = rotate the
    /// cores between tests, which spreads the heat more evenly)
    #[serde(default)]
    pub core_by_core: bool,
}

/// Every buffer size the app offers, 4 KB to 1 GB
pub const SIZE_PRESETS: [usize; 18] = [
    4 << 10, 16 << 10, 32 << 10, 64 << 10, 128 << 10, 256 << 10, 512 << 10,
    1 << 20, 2 << 20, 4 << 20, 8 << 20, 16 << 20, 32 << 20, 64 << 20,
    128 << 20, 256 << 20, 512 << 20, 1 << 30,
];
/// Thread counts offered as presets (the number of logical CPUs is added on top)
pub const THREAD_PRESETS: [usize; 8] = [1, 2, 4, 8, 12, 16, 24, 32];

/// Rough RAM needed to run `patterns` on one `size` buffer: the data, a second buffer for the
/// STREAM patterns and the pointer-chase table
pub fn memory_needed_for(size: usize, patterns: &[AccessPattern]) -> usize {
    let aux = patterns.iter().any(|p| matches!(p, AccessPattern::StreamCopy | AccessPattern::StreamAdd | AccessPattern::StreamTriad));
    let chase = patterns.iter().any(|p| matches!(p, AccessPattern::PointerChase));
    size + if aux { size } else { 0 } + if chase { size + size / 8 } else { 0 }
}

fn one() -> u32 {
    1
}

/// A timed run lasts at least this long: small buffers repeat their pass inside the run, otherwise one
/// pass takes nanoseconds and the timer read, a TLB miss or an interrupt decides the result
const MIN_RUN_NS: f64 = 50_000.0;
/// Most passes repeated inside one timed run
const MAX_PASSES_PER_RUN: u32 = 100_000;

/// Fewest measured runs a test does before the time budget may cut it short
const MIN_ITERATIONS: usize = 3;

fn default_time_budget_ms() -> u64 {
    1000
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccessPattern {
    SequentialRead,
    SequentialWrite,
    SequentialReadWrite,
    RandomRead,
    RandomWrite,
    StridedRead { stride: usize },
    PointerChase,
    DependentRead,      // Each read depends on previous (latency bound)
    IndependentRead,    // Multiple independent reads (bandwidth bound)
    StreamCopy,         // memcpy-like
    StreamScale,        // scale array
    StreamAdd,          // add two arrays
    StreamTriad,        // a = b + c * d
}

impl Default for MemoryBenchmarkConfig {
    /// A run that finishes in a couple of minutes: cache-level sizes up to 256 MB,
    /// the most informative patterns, one thread.
    fn default() -> Self {
        Self {
            sizes: vec![
                32 * 1024,         // L1
                256 * 1024,        // L2
                1024 * 1024,
                4 * 1024 * 1024,   // L2/L3
                16 * 1024 * 1024,
                64 * 1024 * 1024,  // L3 / DRAM
                256 * 1024 * 1024, // DRAM
            ],
            iterations: 10,
            warmup_iterations: 1,
            patterns: vec![
                AccessPattern::SequentialRead,
                AccessPattern::RandomRead,
                AccessPattern::PointerChase,
                AccessPattern::DependentRead,
                AccessPattern::StreamCopy,
                AccessPattern::StreamTriad,
            ],
            thread_counts: vec![1],
            use_huge_pages: false,
            core_ids: Vec::new(),
            core_label: "All cores (OS scheduled)".to_string(),
            time_budget_ms: 5000,
            per_core: false,
            core_by_core: false,
        }
    }
}

impl MemoryBenchmarkConfig {
    /// Every size from 4 KB to 1 GB, every pattern, several thread counts. Can take hours.
    #[allow(dead_code)]
    pub fn exhaustive() -> Self {
        let mut sizes = Vec::new();
        let mut s = 4 * 1024;
        while s <= 1024 * 1024 * 1024 {
            sizes.push(s);
            s *= 2;
        }
        Self {
            sizes,
            iterations: 100,
            warmup_iterations: 10,
            patterns: AccessPattern::all_default(),
            thread_counts: vec![1, 2, 4, 8, 16, 32, 64],
            time_budget_ms: 0,
            ..Self::default()
        }
    }

    /// Number of (size, pattern, thread-count, core-group) tests this configuration will run
    pub fn test_count(&self) -> usize {
        self.sizes.len() * self.patterns.len() * self.effective_thread_counts().len() * self.core_groups().len()
    }

    /// Thread counts that fit the selected cores
    pub fn valid_thread_counts(&self) -> Vec<usize> {
        let max = self.max_threads();
        self.thread_counts.iter().copied().filter(|&t| t >= 1 && t <= max).collect()
    }

    /// Thread counts that will actually run (always just 1 in per-core mode)
    pub fn effective_thread_counts(&self) -> Vec<usize> {
        if self.per_core && !self.core_ids.is_empty() { vec![1] } else { self.valid_thread_counts() }
    }

    /// (pinned cores, label) for every group of cores that is benchmarked separately
    pub fn core_groups(&self) -> Vec<(Vec<usize>, String)> {
        if self.per_core && !self.core_ids.is_empty() {
            self.core_ids.iter().map(|&c| (vec![c], format!("Core {}", c))).collect()
        } else {
            vec![(self.core_ids.clone(), self.core_label.clone())]
        }
    }

    pub fn max_threads(&self) -> usize {
        if self.core_ids.is_empty() { num_cpus::get() } else { self.core_ids.len() }
    }
}

impl AccessPattern {
    /// Every pattern, with the two strides worth looking at
    pub fn all_default() -> Vec<AccessPattern> {
        vec![
            AccessPattern::SequentialRead,
            AccessPattern::SequentialWrite,
            AccessPattern::SequentialReadWrite,
            AccessPattern::RandomRead,
            AccessPattern::RandomWrite,
            AccessPattern::StridedRead { stride: 64 },
            AccessPattern::StridedRead { stride: 4096 },
            AccessPattern::PointerChase,
            AccessPattern::DependentRead,
            AccessPattern::IndependentRead,
            AccessPattern::StreamCopy,
            AccessPattern::StreamScale,
            AccessPattern::StreamAdd,
            AccessPattern::StreamTriad,
        ]
    }

    /// Short display name
    pub fn label(&self) -> String {
        match self {
            AccessPattern::StridedRead { stride } => format!("StridedRead({} B)", stride),
            other => format!("{:?}", other),
        }
    }

    /// What the pattern actually does, for the progress display
    pub fn describe(&self) -> &'static str {
        match self {
            AccessPattern::SequentialRead => "Reads the buffer front to back in 64-bit words (AVX2 where available): best case for the prefetcher, shows read bandwidth",
            AccessPattern::SequentialWrite => "Writes the buffer front to back in 64-bit words: shows write bandwidth",
            AccessPattern::SequentialReadWrite => "Reads and writes back every word in order (read-modify-write)",
            AccessPattern::RandomRead => "Reads one byte per cache line in random order: defeats the prefetcher, shows random-access cost",
            AccessPattern::RandomWrite => "Writes one byte per cache line in random order",
            AccessPattern::StridedRead { .. } => "Reads with a fixed stride: probes cache-line / page (TLB) behaviour",
            AccessPattern::PointerChase => "Follows a random cycle of dependent loads, one per cache line: each address comes from the previous load, so this is true memory latency",
            AccessPattern::DependentRead => "Follows a sequential chain of dependent loads: latency with a prefetch-friendly layout",
            AccessPattern::IndependentRead => "Reads cache lines with 4 independent accumulators: overlaps misses, shows memory-level parallelism",
            AccessPattern::StreamCopy => "STREAM copy: dst = src",
            AccessPattern::StreamScale => "STREAM scale: a = a · k (in place, 64-bit words)",
            AccessPattern::StreamAdd => "STREAM add: a = a + b (two buffers, 64-bit words: 2 reads + 1 write)",
            AccessPattern::StreamTriad => "STREAM triad: a = a + k · b (two buffers, 64-bit words: 2 reads + 1 write)",
        }
    }

    /// Bytes represented by one "access" when converting run time to ns per access
    fn access_unit(&self) -> f64 {
        match self {
            AccessPattern::PointerChase | AccessPattern::DependentRead => 8.0,
            _ => 64.0, // one cache line
        }
    }

    fn needs_aux(&self) -> bool {
        matches!(self, AccessPattern::StreamCopy | AccessPattern::StreamAdd | AccessPattern::StreamTriad)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryBenchmarkResult {
    /// Passes over the buffer inside each timed run (small buffers repeat so a run lasts ≥ 50 µs);
    /// all times are per pass
    #[serde(default = "one")]
    pub passes_per_run: u32,
    pub size: usize,
    pub pattern: AccessPattern,
    pub thread_count: usize,
    pub latency_ns: f64,           // Average latency per access in nanoseconds
    pub bandwidth_gb_s: f64,       // Bandwidth in GB/s
    pub iterations: u32,
    pub min_latency_ns: f64,
    pub max_latency_ns: f64,
    pub std_dev_ns: f64,
    pub percentile_50_ns: f64,
    pub percentile_95_ns: f64,
    pub percentile_99_ns: f64,
    pub percentile_999_ns: f64,
    /// Average time per access (per cache line, or per dependent load for the chase patterns)
    #[serde(default)]
    pub ns_per_access: f64,
    /// Core selection this result was measured with
    #[serde(default)]
    pub cores: String,
    /// Temperatures, clocks, RAM and GPU/VRAM readings taken while this test ran
    #[serde(default)]
    pub telemetry: Telemetry,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryBenchmarkSummary {
    pub results: Vec<MemoryBenchmarkResult>,
    pub config: MemoryBenchmarkConfig,
    pub system_info: crate::system_info::SystemInfo,
    pub timestamp: String,
}

/// Live state of a running benchmark, polled by the GUI
#[derive(Debug, Clone, Default)]
pub struct MemProgress {
    pub total_tests: usize,
    pub done_tests: usize,
    pub size: usize,
    pub pattern: Option<AccessPattern>,
    pub threads: usize,
    pub cores: String,
    /// "allocating", "preparing", "warmup" or "measuring"
    pub phase: String,
    pub iteration: u32,
    pub iterations: u32,
    /// Duration of the most recent run, ns
    pub last_run_ns: f64,
    pub completed: Vec<MemoryBenchmarkResult>,
    pub started: Option<std::time::Instant>,
    /// Set when the run ended (finished, stopped or failed)
    pub finished: Option<std::time::Instant>,
}

pub type ProgressHandle = Arc<Mutex<MemProgress>>;

pub fn new_progress() -> ProgressHandle {
    Arc::new(Mutex::new(MemProgress::default()))
}

pub struct MemoryBenchmark {
    progress: Option<ProgressHandle>,
    /// Keep the results already in the progress handle and add this run's to them (several passes
    /// of one combined "run all" show as a single list)
    carry_over: bool,
    sensors: Option<Arc<Sampler>>,
    /// Cores worker threads are pinned to right now (one group of `config.core_groups()`)
    active_cores: Vec<usize>,
    active_label: String,
    config: MemoryBenchmarkConfig,
    timer: HighResTimer,
    rng: StdRng,
    cancel: CancelFlag,
    /// Passes over the buffer inside one timed run (see MIN_RUN_NS)
    passes: std::sync::atomic::AtomicU32,
}

/// Per-(size, pattern, threads) setup data built once, outside the timed region and
/// reused by every iteration
enum Prepared {
    None,
    Indices(Vec<usize>),
    PerThread(Vec<Vec<usize>>),
    /// `pos`: where each thread's chase stands; it carries on from there in the next pass and run,
    /// so a big buffer is walked through instead of the same few MB being revisited (which then sit
    /// in the cache and made "1 GB" report L3 latency)
    Chain { chain: Vec<usize>, pos: Vec<std::sync::atomic::AtomicUsize> },
}

/// Split `buf` into `tc` contiguous, disjoint, mutable chunks, the same ranges as [`share`]
fn split_mut(mut buf: &mut [u8], tc: usize) -> Vec<&mut [u8]> {
    let tc = tc.max(1);
    let n = buf.len();
    let mut out = Vec::with_capacity(tc);
    for i in 0..tc {
        let (head, tail) = buf.split_at_mut(share(n, tc, i).len());
        out.push(head);
        buf = tail;
    }
    out
}

/// Bytes of one cache line: threads split buffers on line boundaries, so no two threads ever write
/// the same line (false sharing made tiny buffers with many threads look several times slower)
const LINE: usize = 64;

/// Range of bytes owned by worker `i` when `n` bytes are shared between `tc` workers: whole cache
/// lines each (the last worker also takes the tail); falls back to plain bytes for tiny buffers
fn share(n: usize, tc: usize, i: usize) -> std::ops::Range<usize> {
    let tc = tc.max(1);
    let units = |n: usize, i: usize| {
        let base = n / tc;
        let rem = n % tc;
        let start = i * base + i.min(rem);
        start..start + base + usize::from(i < rem)
    };
    let lines = n / LINE;
    if lines < tc {
        return units(n, i);
    }
    let r = units(lines, i);
    r.start * LINE..if i + 1 == tc { n } else { r.end * LINE }
}

/// A buffer whose data starts at a chosen offset from a 4 KB page boundary
#[derive(Default)]
pub(crate) struct AlignedBuf {
    raw: Vec<u8>,
    off: usize,
    len: usize,
}

impl std::ops::Deref for AlignedBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.raw[self.off..self.off + self.len]
    }
}

impl std::ops::DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.raw[self.off..self.off + self.len]
    }
}

/// Offset of the second (STREAM) buffer from a page boundary. Two page-aligned buffers put src[i] and
/// dst[i] at the same position within a 4 KB page, so every load looks like it may depend on the store
/// just before it ("4K aliasing"); where the allocator happened to do that, StreamCopy dipped to half
/// speed at one size. Half a page plus a few lines keeps the two streams apart.
const AUX_OFFSET: usize = 2048 + 3 * LINE;

/// 64-bit words of a byte slice (the unaligned head and tail bytes stay outside)
fn words(b: &[u8]) -> (&[u8], &[u64], &[u8]) {
    // SAFETY: every bit pattern is a valid u64
    unsafe { b.align_to::<u64>() }
}

fn words_mut(b: &mut [u8]) -> (&mut [u8], &mut [u64], &mut [u8]) {
    // SAFETY: every bit pattern is a valid u64
    unsafe { b.align_to_mut::<u64>() }
}

/// Defines a kernel twice: compiled for AVX2 (used when the CPU has it) and for the default x86-64
/// target, which only uses SSE2 and so halves what one core can pull from L1 / L2
macro_rules! kernel {
    ($(#[$m:meta])* fn $name:ident($($a:ident: $t:ty),*) $(-> $r:ty)? $body:block) => {
        $(#[$m])*
        fn $name($($a: $t),*) $(-> $r)? {
            #[inline(always)]
            fn portable($($a: $t),*) $(-> $r)? $body
            #[cfg(target_arch = "x86_64")]
            {
                #[target_feature(enable = "avx2")]
                unsafe fn avx2($($a: $t),*) $(-> $r)? {
                    portable($($a),*)
                }
                if std::arch::is_x86_feature_detected!("avx2") {
                    // SAFETY: the CPU supports AVX2 (checked above)
                    return unsafe { avx2($($a),*) };
                }
            }
            portable($($a),*)
        }
    };
}

kernel! {
    /// Sum of all words with independent accumulators, so the adds never wait on each other
    fn k_sum(w: &[u64]) -> u64 {
        let mut acc = [0u64; 8];
        let mut it = w.chunks_exact(8);
        for c in &mut it {
            for k in 0..8 {
                acc[k] = acc[k].wrapping_add(c[k]);
            }
        }
        let rest = it.remainder().iter().fold(0u64, |a, &b| a.wrapping_add(b));
        acc.iter().fold(rest, |a, &b| a.wrapping_add(b))
    }
}

kernel! {
    /// Plain stores of a changing value (a constant would be turned into memset)
    fn k_fill(w: &mut [u64], seed: u64) {
        for (i, x) in w.iter_mut().enumerate() {
            *x = seed ^ i as u64;
        }
    }
}

kernel! {
    fn k_inc(w: &mut [u64]) {
        for x in w.iter_mut() {
            *x = x.wrapping_add(1);
        }
    }
}

kernel! {
    fn k_scale(w: &mut [u64]) {
        for x in w.iter_mut() {
            *x = x.wrapping_mul(3);
        }
    }
}

kernel! {
    /// a += b, or a += 3·b for the triad
    fn k_add(a: &mut [u64], b: &[u64], triad: bool) {
        if triad {
            for (x, y) in a.iter_mut().zip(b) {
                *x = x.wrapping_add(y.wrapping_mul(3));
            }
        } else {
            for (x, y) in a.iter_mut().zip(b) {
                *x = x.wrapping_add(*y);
            }
        }
    }
}

fn sum_bytes(b: &[u8]) -> u64 {
    let (head, mid, tail) = words(b);
    let edge = head.iter().chain(tail).fold(0u64, |a, &x| a.wrapping_add(x as u64));
    edge.wrapping_add(k_sum(mid))
}

fn fill_bytes(b: &mut [u8], seed: u64) {
    let (head, mid, tail) = words_mut(b);
    head.iter_mut().chain(tail.iter_mut()).for_each(|x| *x = seed as u8);
    k_fill(mid, seed);
}

/// Word-wise update of `b` in place: +1 (read-modify-write) or ×3 (STREAM scale)
fn update_bytes(b: &mut [u8], scale: bool) {
    let (head, mid, tail) = words_mut(b);
    for x in head.iter_mut().chain(tail.iter_mut()) {
        *x = if scale { x.wrapping_mul(3) } else { x.wrapping_add(1) };
    }
    if scale { k_scale(mid) } else { k_inc(mid) }
}

/// dst += src (or dst += 3·src), word-wise when both have the same alignment
fn add_bytes(dst: &mut [u8], src: &[u8], triad: bool) {
    let n = dst.len().min(src.len());
    let (dst, src) = (&mut dst[..n], &src[..n]);
    let byte = |x: u8, y: u8| if triad { x.wrapping_add(y.wrapping_mul(3)) } else { x.wrapping_add(y) };
    if dst.as_ptr() as usize % 8 != src.as_ptr() as usize % 8 {
        dst.iter_mut().zip(src).for_each(|(x, y)| *x = byte(*x, *y));
        return;
    }
    let (dh, dm, dt) = words_mut(dst);
    let (sh, sm, st) = words(src);
    dh.iter_mut().zip(sh).chain(dt.iter_mut().zip(st)).for_each(|(x, y)| *x = byte(*x, *y));
    k_add(dm, sm, triad);
}

impl MemoryBenchmark {
    pub fn new(config: MemoryBenchmarkConfig) -> Self {
        Self {
            config,
            timer: HighResTimer::new(),
            rng: StdRng::seed_from_u64(0xDEADBEEF_CAFEBABE),
            cancel: cancel::new_flag(),
            progress: None,
            carry_over: false,
            passes: std::sync::atomic::AtomicU32::new(1),
            sensors: None,
            active_cores: Vec::new(),
            active_label: String::new(),
        }
    }

    /// Attach a sensor sampler so every result carries temperatures / clocks / VRAM
    pub fn with_sensors(mut self, sampler: Arc<Sampler>) -> Self {
        self.sensors = Some(sampler);
        self
    }

    /// Publish live progress to `handle`
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Add this run's results after those already in the progress handle instead of replacing them
    pub fn carry_over(mut self, yes: bool) -> Self {
        self.carry_over = yes;
        self
    }

    fn update_progress(&self, f: impl FnOnce(&mut MemProgress)) {
        if let Some(p) = &self.progress {
            if let Ok(mut g) = p.lock() {
                f(&mut g);
            }
        }
    }

    /// Share a flag that stops the run early when set
    pub fn with_cancel(mut self, flag: CancelFlag) -> Self {
        self.cancel = flag;
        self
    }

    pub fn run(&mut self) -> Result<MemoryBenchmarkSummary> {
        let system_info = crate::system_info::collect_system_info()?;
        let mut results = Vec::new();

        let thread_counts = self.config.effective_thread_counts();
        if thread_counts.is_empty() {
            return Err(anyhow::anyhow!(
                "No usable thread count: the selected cores allow at most {} thread(s)",
                self.config.max_threads()
            ));
        }
        if self.config.sizes.is_empty() || self.config.patterns.is_empty() {
            return Err(anyhow::anyhow!("Select at least one size and one pattern"));
        }
        let groups = self.config.core_groups();
        let total = self.config.test_count();
        let label = self.config.core_label.clone();
        let carry = self.carry_over;
        self.update_progress(|p| {
            let (done, planned, completed, started) = if carry {
                (p.done_tests, p.total_tests, std::mem::take(&mut p.completed), p.started)
            } else {
                (0, 0, Vec::new(), None)
            };
            *p = MemProgress {
                total_tests: planned + total,
                done_tests: done,
                completed,
                cores: label.clone(),
                started: started.or_else(|| Some(std::time::Instant::now())),
                ..Default::default()
            }
        });

        let sizes = self.config.sizes.clone();
        let needs_aux = self.config.patterns.iter().any(|p| p.needs_aux());
        // (size, core group) in run order: sizes outermost rotates the cores between tests; with
        // core_by_core every size and pattern finishes on one core before the next core starts
        let order: Vec<(usize, usize)> = if self.config.core_by_core {
            (0..groups.len()).flat_map(|g| sizes.iter().map(move |&s| (s, g))).collect()
        } else {
            sizes.iter().flat_map(|&s| (0..groups.len()).map(move |g| (s, g))).collect()
        };
        let (mut data, mut aux, mut have) = (AlignedBuf::default(), AlignedBuf::default(), None::<usize>);
        for (size, gi) in order {
            cancel::check(&self.cancel)?;
            if have != Some(size) {
                self.update_progress(|p| {
                    p.size = size;
                    p.pattern = None;
                    p.phase = "allocating".into();
                });
                // Allocate once per size and reuse across patterns, threads (and cores, when rotating);
                // the second buffer only exists for the STREAM patterns that need it
                // free the previous size before allocating the next
                drop(std::mem::take(&mut data));
                drop(std::mem::take(&mut aux));
                data = self.allocate_buffer(size)?;
                if needs_aux {
                    aux = self.allocate_at(size, AUX_OFFSET)?;
                }
                have = Some(size);
            }
            {
                let (group_cores, group_label) = &groups[gi];
                if self.active_cores != *group_cores {
                    // the app's own threads must not share the core(s) being measured
                    crate::app_core::keep_off(group_cores);
                }
                self.active_cores = group_cores.clone();
                self.active_label = group_label.clone();
                for pattern in self.config.patterns.clone() {
                    for &thread_count in &thread_counts {
                        cancel::check(&self.cancel)?;
                        let result =
                            self.run_single_test(&mut data, &mut aux, size, pattern, thread_count)?;
                        self.update_progress(|p| {
                            p.done_tests += 1;
                            p.completed.push(result.clone());
                        });
                        results.push(result);
                    }
                }
            }
        }
        self.active_cores.clear();
        self.update_progress(|p| p.finished = Some(std::time::Instant::now()));

        Ok(MemoryBenchmarkSummary {
            results,
            config: self.config.clone(),
            system_info,
            timestamp: chrono::Utc::now().to_rfc3339(),
        })
    }

    fn run_single_test(
        &mut self,
        data: &mut [u8],
        aux: &mut [u8],
        size: usize,
        pattern: AccessPattern,
        thread_count: usize,
    ) -> Result<MemoryBenchmarkResult> {
        let test_start = std::time::Instant::now();
        let budget = std::time::Duration::from_millis(self.config.time_budget_ms);
        let over_budget = |n_done: usize| {
            !budget.is_zero() && n_done >= MIN_ITERATIONS && test_start.elapsed() > budget
        };
        let cores = self.active_label.clone();
        let sensor_start = self.sensors.as_ref().map(|s| s.now_ms());
        let planned = self.config.iterations.max(1);
        self.update_progress(|p| {
            p.size = size;
            p.pattern = Some(pattern);
            p.threads = thread_count;
            p.cores = cores.clone();
            p.phase = "preparing".into();
            p.iteration = 0;
            p.iterations = planned;
        });

        // Warmup (single thread); stops early if it alone eats the budget
        let warm_prep = self.prepare(pattern, size, 1);
        for w in 0..self.config.warmup_iterations {
            cancel::check(&self.cancel)?;
            if !budget.is_zero() && test_start.elapsed() > budget / 2 {
                break;
            }
            self.update_progress(|p| {
                p.phase = "warmup".into();
                p.iteration = w + 1;
                p.iterations = self.config.warmup_iterations;
            });
            self.run_pattern(data, aux, size, pattern, 1, &warm_prep)?;
        }
        let prep = if thread_count == 1 { warm_prep } else { self.prepare(pattern, size, thread_count) };

        // How many passes make a timed run long enough to measure: double until one untimed probe
        // run lasts MIN_RUN_NS (a single probe pass would include thread start-up and look too slow)
        let mut passes = 1u32;
        loop {
            self.passes.store(passes, std::sync::atomic::Ordering::Relaxed);
            let (per_pass_ns, _) = self.run_pattern(data, aux, size, pattern, thread_count, &prep)?;
            if per_pass_ns * passes as f64 >= MIN_RUN_NS || passes >= MAX_PASSES_PER_RUN {
                break;
            }
            let want = (MIN_RUN_NS / per_pass_ns.max(1.0) / passes as f64).ceil().clamp(2.0, 16.0) as u32;
            passes = passes.saturating_mul(want).min(MAX_PASSES_PER_RUN);
        }

        // Actual benchmark
        let iterations = planned;
        let mut latencies = Vec::with_capacity(iterations as usize);
        let mut total_bytes = 0u64;

        for i in 0..iterations {
            cancel::check(&self.cancel)?;
            if over_budget(latencies.len()) {
                break;
            }
            self.update_progress(|p| {
                p.phase = "measuring".into();
                p.iteration = i + 1;
                p.iterations = iterations;
            });
            let (latency_ns, bytes) = self.run_pattern(data, aux, size, pattern, thread_count, &prep)?;
            self.update_progress(|p| p.last_run_ns = latency_ns);
            latencies.push(latency_ns);
            total_bytes += bytes;
        }

        // Calculate statistics
        latencies.sort_by(|a, b| a.total_cmp(b));

        let sum: f64 = latencies.iter().sum();
        let avg = sum / latencies.len() as f64;
        let min = latencies[0];
        let max = latencies[latencies.len() - 1];

        let variance: f64 = latencies.iter()
            .map(|&x| (x - avg).powi(2))
            .sum::<f64>() / latencies.len() as f64;
        let std_dev = variance.sqrt();

        let p_idx = |p: f64| ((latencies.len() as f64 * p) as usize).min(latencies.len() - 1);
        let p50 = latencies[p_idx(0.50)];
        let p95 = latencies[p_idx(0.95)];
        let p99 = latencies[p_idx(0.99)];
        let p999 = latencies[p_idx(0.999)];

        // Calculate bandwidth (bytes/ns == GB/s)
        let bandwidth_gb_s = if sum > 0.0 { total_bytes as f64 / sum } else { 0.0 };

        let passes_per_run = self.passes.swap(1, std::sync::atomic::Ordering::Relaxed);
        Ok(MemoryBenchmarkResult {
            passes_per_run,
            size,
            pattern,
            thread_count,
            latency_ns: avg,
            bandwidth_gb_s,
            iterations: latencies.len() as u32,
            min_latency_ns: min,
            max_latency_ns: max,
            std_dev_ns: std_dev,
            percentile_50_ns: p50,
            percentile_95_ns: p95,
            percentile_99_ns: p99,
            percentile_999_ns: p999,
            ns_per_access: {
                // accesses per run = bytes per run / bytes per access; workers run in parallel,
                // so each thread performs 1/threads of them
                let bytes_per_run = total_bytes as f64 / latencies.len() as f64;
                if bytes_per_run > 0.0 {
                    avg / (bytes_per_run / pattern.access_unit() / thread_count as f64)
                } else {
                    0.0
                }
            },
            telemetry: match (&self.sensors, sensor_start) {
                (Some(s), Some(t0)) => s.record(
                    "memory",
                    format!("Memory · {} KB · {} · {} thread(s) · {}", size / 1024, pattern.label(), thread_count, cores),
                    t0,
                    s.now_ms(),
                ),
                _ => Telemetry::default(),
            },
            cores,
        })
    }

    /// Allocate a page-touched buffer with non-constant contents, starting `offset` bytes after a
    /// 4 KB page boundary
    fn allocate_at(&mut self, size: usize, offset: usize) -> Result<AlignedBuf> {
        const PAGE: usize = 4096;
        let mut raw = Vec::new();
        raw.try_reserve_exact(size + PAGE + offset)
            .map_err(|e| anyhow::anyhow!("Failed to allocate {} bytes: {}", size, e))?;
        raw.resize(size + PAGE + offset, 0);
        let off = (PAGE - raw.as_ptr() as usize % PAGE) % PAGE + offset;
        let mut buf = AlignedBuf { raw, off, len: size };
        // Cheap non-constant fill that also faults every page in (no lazy zero pages)
        let seed: u8 = self.rng.gen();
        for (i, chunk) in buf.chunks_mut(4096).enumerate() {
            let base = (i as u8).wrapping_mul(31).wrapping_add(seed);
            for (j, b) in chunk.iter_mut().enumerate() {
                *b = base.wrapping_add(j as u8);
            }
        }
        Ok(buf)
    }

    /// The main buffer: page aligned
    fn allocate_buffer(&mut self, size: usize) -> Result<AlignedBuf> {
        self.allocate_at(size, 0)
    }

    /// Pin worker `i` to its configured core (no-op when no cores were selected)
    fn pin_worker(&self, i: usize) {
        let cores = &self.active_cores;
        if !cores.is_empty() {
            crate::topology::pin_current_thread(cores[i % cores.len()]);
        }
    }

    /// Run `worker(i)` on `tc` scoped threads. Threads are pinned, then released together
    /// from a barrier and each times only its own work, so thread start-up is excluded.
    /// Returns (total bytes reported by workers, slowest worker time in ns).
    fn run_parallel<F>(&self, tc: usize, worker: F) -> (u64, f64)
    where
        F: Fn(usize) -> usize + Sync,
    {
        let barrier = Barrier::new(tc);
        let passes = self.passes.load(std::sync::atomic::Ordering::Relaxed).max(1);
        let results: Vec<(usize, u64)> = std::thread::scope(|s| {
            let (worker, barrier) = (&worker, &barrier);
            let handles: Vec<_> = (0..tc)
                .map(|i| {
                    s.spawn(move || {
                        self.pin_worker(i);
                        barrier.wait();
                        let t0 = self.timer.now_ticks();
                        let mut bytes = 0;
                        for _ in 0..passes {
                            bytes += worker(i);
                        }
                        (bytes, self.timer.now_ticks() - t0)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().expect("benchmark worker panicked")).collect()
        });
        self.aggregate(results)
    }

    /// Like `run_parallel` but each worker gets exclusive access to one chunk of `buf`
    fn run_parallel_mut<F>(&self, buf: &mut [u8], tc: usize, worker: F) -> (u64, f64)
    where
        F: Fn(usize, &mut [u8]) -> usize + Sync,
    {
        let chunks = split_mut(buf, tc);
        let barrier = Barrier::new(tc);
        let passes = self.passes.load(std::sync::atomic::Ordering::Relaxed).max(1);
        let results: Vec<(usize, u64)> = std::thread::scope(|s| {
            let (worker, barrier) = (&worker, &barrier);
            let handles: Vec<_> = chunks
                .into_iter()
                .enumerate()
                .map(|(i, chunk)| {
                    s.spawn(move || {
                        self.pin_worker(i);
                        barrier.wait();
                        let t0 = self.timer.now_ticks();
                        let mut bytes = 0;
                        for _ in 0..passes {
                            bytes += worker(i, &mut chunk[..]);
                        }
                        (bytes, self.timer.now_ticks() - t0)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().expect("benchmark worker panicked")).collect()
        });
        self.aggregate(results)
    }

    fn aggregate(&self, results: Vec<(usize, u64)>) -> (u64, f64) {
        let bytes: usize = results.iter().map(|r| r.0).sum();
        let ticks = results.iter().map(|r| r.1).max().unwrap_or(0);
        (bytes as u64, self.timer.ticks_to_ns(ticks).max(1) as f64)
    }

    /// Returns (elapsed_ns, bytes_accessed)
    fn run_pattern(
        &mut self,
        data: &mut [u8],
        aux: &mut [u8],
        _size: usize,
        pattern: AccessPattern,
        thread_count: usize,
        prep: &Prepared,
    ) -> Result<(f64, u64)> {
        let tc = thread_count.max(1);
        let (bytes, ns) = match pattern {
            AccessPattern::SequentialRead => self.sequential_read(data, tc),
            AccessPattern::SequentialWrite => self.sequential_write(data, tc),
            AccessPattern::SequentialReadWrite => self.sequential_read_write(data, tc),
            AccessPattern::RandomRead => self.random_read(data, tc, prep),
            AccessPattern::RandomWrite => self.random_write(data, tc, prep),
            AccessPattern::StridedRead { stride } => self.strided_read(data, stride, tc),
            AccessPattern::PointerChase => self.pointer_chase(tc, prep),
            AccessPattern::DependentRead => self.dependent_read(tc, prep),
            AccessPattern::IndependentRead => self.independent_read(data, tc, prep),
            AccessPattern::StreamCopy => self.stream_copy(data, aux, tc),
            AccessPattern::StreamScale => self.stream_scale(data, tc),
            AccessPattern::StreamAdd => self.stream_add(data, aux, tc),
            AccessPattern::StreamTriad => self.stream_triad(data, aux, tc),
        };
        // one "run" is reported per pass over the buffer
        let passes = self.passes.load(std::sync::atomic::Ordering::Relaxed).max(1);
        Ok((ns / passes as f64, bytes / passes as u64))
    }

    // Sequential read - measures memory read bandwidth
    fn sequential_read(&self, buffer: &[u8], tc: usize) -> (u64, f64) {
        self.run_parallel(tc, |i| {
            let r = share(buffer.len(), tc, i);
            std::hint::black_box(sum_bytes(&buffer[r.clone()]));
            r.len()
        })
    }

    // Sequential write - measures memory write bandwidth
    fn sequential_write(&self, buffer: &mut [u8], tc: usize) -> (u64, f64) {
        self.run_parallel_mut(buffer, tc, |i, chunk| {
            fill_bytes(chunk, 0x9E37_79B9_7F4A_7C15 ^ i as u64);
            std::hint::black_box(&*chunk);
            chunk.len()
        })
    }

    // Sequential read-write
    fn sequential_read_write(&self, buffer: &mut [u8], tc: usize) -> (u64, f64) {
        self.run_parallel_mut(buffer, tc, |_, chunk| {
            update_bytes(chunk, false);
            std::hint::black_box(&*chunk);
            chunk.len() * 2 // Read + write
        })
    }

    /// Build the untimed setup data for a pattern
    fn prepare(&self, pattern: AccessPattern, size: usize, tc: usize) -> Prepared {
        use rand::seq::SliceRandom;
        match pattern {
            AccessPattern::RandomRead => {
                let mut idx: Vec<usize> = (0..size).step_by(64).collect(); // cache-line aligned
                idx.shuffle(&mut StdRng::seed_from_u64(0xFEEDFACE));
                Prepared::Indices(idx)
            }
            AccessPattern::IndependentRead => Prepared::Indices((0..size).step_by(64).collect()),
            AccessPattern::RandomWrite => Prepared::PerThread(
                (0..tc)
                    .map(|i| {
                        let len = share(size, tc, i).len();
                        let mut idxs: Vec<usize> = (0..len).step_by(64).collect();
                        idxs.shuffle(&mut StdRng::seed_from_u64(0xFEEDFACEu64.wrapping_add(i as u64)));
                        idxs
                    })
                    .collect(),
            ),
            AccessPattern::PointerChase => {
                // One node per 64-byte cache line (word index = line * 8), linked in a single
                // random cycle, so every hop is a fresh line and nothing is prefetchable
                let lines = (size / 64).max(tc.max(2));
                let mut perm: Vec<usize> = (0..lines).collect();
                perm.shuffle(&mut StdRng::seed_from_u64(0xBADF00D));
                let mut chain = vec![0usize; lines * 8];
                for i in 0..lines {
                    chain[perm[i] * 8] = perm[(i + 1) % lines] * 8;
                }
                // threads start evenly spread around the cycle (adjacent starts would make every
                // thread walk right behind the first one and find its lines already cached)
                let pos = (0..tc).map(|i| std::sync::atomic::AtomicUsize::new(perm[(i * lines / tc.max(1)) % lines] * 8)).collect();
                Prepared::Chain { chain, pos }
            }
            AccessPattern::DependentRead => {
                // Sequential chain: node i -> node i+1, last wraps to 0
                let num_nodes = (size / 8).max(tc);
                let chain = (0..num_nodes).map(|i| (i + 1) % num_nodes).collect();
                let pos = (0..tc).map(|i| std::sync::atomic::AtomicUsize::new((i * num_nodes) / tc.max(1))).collect();
                Prepared::Chain { chain, pos }
            }
            _ => Prepared::None,
        }
    }

    // Random read - measures random access latency
    fn random_read(&self, buffer: &[u8], tc: usize, prep: &Prepared) -> (u64, f64) {
        let Prepared::Indices(shuffled) = prep else { return (0, 1.0) };
        self.run_parallel(tc, |i| {
            let idxs = &shuffled[share(shuffled.len(), tc, i)];
            let mut sum = 0u64;
            for &idx in idxs {
                sum = sum.wrapping_add(buffer[idx] as u64);
            }
            std::hint::black_box(sum);
            idxs.len() * 64
        })
    }

    // Random write
    fn random_write(&self, buffer: &mut [u8], tc: usize, prep: &Prepared) -> (u64, f64) {
        let Prepared::PerThread(per_thread) = prep else { return (0, 1.0) };
        self.run_parallel_mut(buffer, tc, |i, chunk| {
            for &idx in &per_thread[i] {
                chunk[idx] = (idx & 0xFF) as u8;
            }
            per_thread[i].len() * 64
        })
    }

    // Strided read - measures cache behavior with specific stride
    fn strided_read(&self, buffer: &[u8], stride: usize, tc: usize) -> (u64, f64) {
        let stride = stride.max(1);
        self.run_parallel(tc, |i| {
            let r = share(buffer.len(), tc, i);
            let mut sum = 0u64;
            let mut count = 0usize;
            let mut idx = r.start;
            while idx < r.end {
                sum = sum.wrapping_add(buffer[idx] as u64);
                idx += stride;
                count += 1;
            }
            std::hint::black_box(sum);
            count * 64
        })
    }

    // Pointer chasing - measures pointer dereference latency
    fn pointer_chase(&self, tc: usize, prep: &Prepared) -> (u64, f64) {
        self.chase(tc, prep)
    }

    // Dependent read - each read address depends on previous read value
    fn dependent_read(&self, tc: usize, prep: &Prepared) -> (u64, f64) {
        self.chase(tc, prep)
    }

    fn chase(&self, tc: usize, prep: &Prepared) -> (u64, f64) {
        use std::sync::atomic::Ordering::Relaxed;
        let Prepared::Chain { chain, pos } = prep else { return (0, 1.0) };
        let steps: usize = 1 << 16; // Chase depth per thread per pass (latency-bound)
        self.run_parallel(tc, |i| {
            let mut current = pos[i % pos.len()].load(Relaxed);
            for _ in 0..steps {
                current = chain[current];
            }
            pos[i % pos.len()].store(current, Relaxed);
            steps * 8
        })
    }

    // Independent reads - multiple independent memory accesses (bandwidth bound)
    fn independent_read(&self, buffer: &[u8], tc: usize, prep: &Prepared) -> (u64, f64) {
        let Prepared::Indices(indices) = prep else { return (0, 1.0) };
        self.run_parallel(tc, |i| {
            let idxs = &indices[share(indices.len(), tc, i)];
            let mut sums = [0u64; 4]; // 4 independent accumulators
            for (j, &idx) in idxs.iter().enumerate() {
                sums[j % 4] = sums[j % 4].wrapping_add(buffer[idx] as u64);
            }
            for s in sums {
                std::hint::black_box(s);
            }
            idxs.len() * 64
        })
    }

    // Stream copy - memcpy pattern (src -> dst)
    fn stream_copy(&self, src: &[u8], dst: &mut [u8], tc: usize) -> (u64, f64) {
        self.run_parallel_mut(dst, tc, |i, chunk| {
            let start = share(src.len(), tc, i).start;
            chunk.copy_from_slice(&src[start..start + chunk.len()]);
            std::hint::black_box(&*chunk);
            chunk.len() * 2 // Read + write
        })
    }

    // Stream scale - a = a * scalar
    fn stream_scale(&self, buffer: &mut [u8], tc: usize) -> (u64, f64) {
        self.run_parallel_mut(buffer, tc, |_, chunk| {
            update_bytes(chunk, true);
            std::hint::black_box(&*chunk);
            chunk.len() * 2 // Read + write
        })
    }

    // Stream add - a = a + b (reads `dst` and `src`, writes `dst`: the same traffic as STREAM's c = a + b)
    fn stream_add(&self, src: &[u8], dst: &mut [u8], tc: usize) -> (u64, f64) {
        let n = src.len();
        self.run_parallel_mut(dst, tc, |i, chunk| {
            let start = share(n, tc, i).start;
            add_bytes(chunk, &src[start..(start + chunk.len()).min(n)], false);
            std::hint::black_box(&*chunk);
            chunk.len() * 3 // 2 reads + 1 write
        })
    }

    // Stream triad - a = a + k·b (2 reads + 1 write, like STREAM's a = b + k·c)
    fn stream_triad(&self, src: &[u8], dst: &mut [u8], tc: usize) -> (u64, f64) {
        let n = src.len();
        self.run_parallel_mut(dst, tc, |i, chunk| {
            let start = share(n, tc, i).start;
            add_bytes(chunk, &src[start..(start + chunk.len()).min(n)], true);
            std::hint::black_box(&*chunk);
            chunk.len() * 3 // 2 reads + 1 write
        })
    }
}

/// One row of a quick test
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuickMemoryRow {
    pub pattern: String,
    /// Average time per memory access (random read: per cache line; pointer chase: per dependent load)
    pub ns_per_access: f64,
    pub bandwidth_gb_s: f64,
    pub p99_run_ms: f64,
}

/// Result of the quick test shown in the GUI
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuickMemoryResult {
    pub size: usize,
    pub iterations: u32,
    pub rows: Vec<QuickMemoryRow>,
    pub elapsed_ms: f64,
}

/// Quick memory latency test (simplified for GUI): single thread, random read + pointer chase.
/// Runs to completion or until `cancel` is set.
pub fn quick_memory_latency_test(
    size: usize,
    iterations: u32,
    cancel: CancelFlag,
    progress: Option<ProgressHandle>,
) -> Result<QuickMemoryResult> {
    let started = std::time::Instant::now();
    let mut bench = MemoryBenchmark::new(MemoryBenchmarkConfig {
        sizes: vec![size],
        iterations,
        warmup_iterations: 2,
        patterns: vec![AccessPattern::RandomRead, AccessPattern::PointerChase],
        thread_counts: vec![1],
        core_label: "All cores (OS scheduled)".to_string(),
        ..MemoryBenchmarkConfig::default()
    })
    .with_cancel(cancel);
    if let Some(p) = progress {
        bench = bench.with_progress(p);
    }

    let summary = bench.run()?;
    let rows = summary
        .results
        .iter()
        .map(|r| QuickMemoryRow {
            pattern: r.pattern.label(),
            ns_per_access: r.ns_per_access,
            bandwidth_gb_s: r.bandwidth_gb_s,
            p99_run_ms: r.percentile_99_ns / 1e6,
        })
        .collect();

    Ok(QuickMemoryResult {
        size,
        iterations,
        rows,
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bench() -> MemoryBenchmark {
        MemoryBenchmark::new(MemoryBenchmarkConfig::default())
    }

    /// Prints ns/access and GB/s per pattern and size (cargo test --release probe_patterns -- --ignored --nocapture)
    #[test]
    #[ignore]
    fn probe_patterns() {
        let sizes: Vec<usize> = std::env::var("PROBE_SIZES").ok().map(|v| v.split(',').filter_map(|x| x.parse::<usize>().ok()).map(|k| k << 10).collect()).unwrap_or_else(|| vec![16 << 10, 128 << 10, 256 << 10, 512 << 10, 4 << 20, 64 << 20, 256 << 20]);
        let threads: Vec<usize> = std::env::var("PROBE_THREADS").ok().map(|v| v.split(',').filter_map(|x| x.parse().ok()).collect()).unwrap_or_else(|| vec![1]);
        let cfg = MemoryBenchmarkConfig {
            sizes: sizes.clone(),
            patterns: AccessPattern::all_default(),
            iterations: 5,
            warmup_iterations: 1,
            thread_counts: threads,
            time_budget_ms: 0,
            ..MemoryBenchmarkConfig::default()
        };
        let s = MemoryBenchmark::new(cfg).run().unwrap();
        for r in &s.results {
            println!("{:<22} {:>2}T {:>7} KB  {:>8.2} ns/access  {:>8.1} GB/s  max/min {:.2}", r.pattern.label(), r.thread_count, r.size >> 10, r.ns_per_access, r.bandwidth_gb_s, r.max_latency_ns / r.min_latency_ns);
        }
    }

    #[test]
    fn test_sequential_read() {
        let mut b = bench();
        let buf = b.allocate_buffer(1024 * 1024).unwrap();
        let (bytes, ns) = b.sequential_read(&buf, 1);
        assert_eq!(bytes, 1024 * 1024);
        assert!(ns > 0.0);
    }

    #[test]
    fn test_sequential_read_multithread_uneven() {
        let mut b = bench();
        let buf = b.allocate_buffer(1000).unwrap();
        assert_eq!(b.sequential_read(&buf, 3).0, 1000);
    }

    #[test]
    fn test_pointer_chase() {
        let b = bench();
        let prep = b.prepare(AccessPattern::PointerChase, 1024 * 1024, 2);
        let (bytes, _) = b.pointer_chase(2, &prep);
        assert_eq!(bytes, 2 * (1 << 16) * 8);
    }

    #[test]
    fn test_all_patterns_run() {
        let mut b = bench();
        let mut data = b.allocate_buffer(64 * 1024 + 7).unwrap();
        let mut aux = b.allocate_buffer(64 * 1024 + 7).unwrap();
        let size = data.len();
        let patterns = [
            AccessPattern::SequentialRead,
            AccessPattern::SequentialWrite,
            AccessPattern::SequentialReadWrite,
            AccessPattern::RandomRead,
            AccessPattern::RandomWrite,
            AccessPattern::StridedRead { stride: 64 },
            AccessPattern::PointerChase,
            AccessPattern::DependentRead,
            AccessPattern::IndependentRead,
            AccessPattern::StreamCopy,
            AccessPattern::StreamScale,
            AccessPattern::StreamAdd,
            AccessPattern::StreamTriad,
        ];
        for p in patterns {
            for tc in [1, 3] {
                let prep = b.prepare(p, size, tc);
                let (ns, bytes) = b.run_pattern(&mut data, &mut aux, size, p, tc, &prep).unwrap();
                assert!(ns > 0.0 && bytes > 0, "{:?} tc={}", p, tc);
            }
        }
    }

    #[test]
    fn test_stream_copy_copies() {
        let mut b = bench();
        let src = b.allocate_buffer(1001).unwrap();
        let mut dst = vec![0u8; 1001];
        b.stream_copy(&src, &mut dst, 4);
        assert_eq!(&src[..], &dst[..]);
    }

    #[test]
    fn test_quick_test_reports_per_access_latency() {
        let r = quick_memory_latency_test(256 * 1024, 3, cancel::new_flag(), None).unwrap();
        assert_eq!(r.rows.len(), 2);
        for row in &r.rows {
            // a memory access takes between a fraction of a ns and a few microseconds
            assert!(row.ns_per_access > 0.01 && row.ns_per_access < 10_000.0, "{:?}", row);
        }
    }

    #[test]
    fn test_cancel_stops_run() {
        let flag = cancel::new_flag();
        flag.store(true, std::sync::atomic::Ordering::Relaxed);
        let err = quick_memory_latency_test(64 * 1024 * 1024, 1000, flag, None).unwrap_err();
        assert!(cancel::is_cancel_error(&err));
    }

    #[test]
    fn test_small_run() {
        let mut b = MemoryBenchmark::new(MemoryBenchmarkConfig {
            sizes: vec![16 * 1024],
            iterations: 3,
            warmup_iterations: 1,
            patterns: vec![AccessPattern::SequentialRead, AccessPattern::PointerChase],
            thread_counts: vec![1],
            ..MemoryBenchmarkConfig::default()
        });
        let s = b.run().unwrap();
        assert_eq!(s.results.len(), 2);
        assert!(s.results.iter().all(|r| r.latency_ns > 0.0 && r.bandwidth_gb_s > 0.0));
    }

    #[test]
    fn test_time_budget_cuts_long_runs_short() {
        let mut b = MemoryBenchmark::new(MemoryBenchmarkConfig {
            sizes: vec![32 * 1024 * 1024],
            iterations: 100_000,
            warmup_iterations: 100_000,
            patterns: vec![AccessPattern::SequentialRead],
            thread_counts: vec![1],
            time_budget_ms: 300,
            ..MemoryBenchmarkConfig::default()
        });
        let started = std::time::Instant::now();
        let s = b.run().unwrap();
        assert!(started.elapsed().as_secs_f64() < 5.0, "budget ignored: {:?}", started.elapsed());
        let n = s.results[0].iterations;
        assert!(n >= 3 && n < 100_000, "iterations = {}", n);
    }

    #[test]
    fn test_short_runs_repeat_their_pass_until_they_are_measurable() {
        let run = |size: usize, stride: usize| {
            let cfg = MemoryBenchmarkConfig {
                sizes: vec![size],
                patterns: vec![AccessPattern::StridedRead { stride }],
                iterations: 5,
                warmup_iterations: 1,
                thread_counts: vec![1],
                time_budget_ms: 0,
                ..MemoryBenchmarkConfig::default()
            };
            MemoryBenchmark::new(cfg).run().unwrap().results.remove(0)
        };
        // 128 KB with a 4 KB stride is 32 loads: one pass is far below a microsecond
        let small = run(128 * 1024, 4096);
        assert!(small.passes_per_run > 1, "passes {}", small.passes_per_run);
        assert!(small.min_latency_ns * small.passes_per_run as f64 >= MIN_RUN_NS * 0.5, "a timed run lasts tens of µs");
        assert!(small.min_latency_ns < 50_000.0, "times are reported per pass");
        // a big buffer already takes long enough per pass
        let big = run(64 << 20, 64);
        assert_eq!(big.passes_per_run, 1);
    }

    #[test]
    fn test_core_order_modes() {
        let n = num_cpus::get().min(3);
        if n < 2 {
            return;
        }
        let cfg = |core_by_core| MemoryBenchmarkConfig {
            sizes: vec![16 * 1024, 32 * 1024],
            patterns: vec![AccessPattern::SequentialRead],
            iterations: 3,
            warmup_iterations: 0,
            thread_counts: vec![1],
            time_budget_ms: 0,
            core_ids: (0..n).collect(),
            per_core: true,
            core_by_core,
            ..MemoryBenchmarkConfig::default()
        };
        let order = |c| {
            let s = MemoryBenchmark::new(cfg(c)).run().unwrap();
            s.results.iter().map(|r| (r.size / 1024, r.cores.clone())).collect::<Vec<_>>()
        };
        // rotating: size 16 KB on every core, then 32 KB on every core
        let rot = order(false);
        assert_eq!(rot[0], (16, "Core 0".to_string()));
        assert_eq!(rot[1], (16, "Core 1".to_string()));
        // core by core: every size on core 0 first
        let cbc = order(true);
        assert_eq!(cbc[0], (16, "Core 0".to_string()));
        assert_eq!(cbc[1], (32, "Core 0".to_string()));
        assert_eq!(cbc[2], (16, "Core 1".to_string()));
        assert_eq!(rot.len(), cbc.len());
    }

    #[test]
    fn test_carry_over_accumulates_two_passes() {
        let handle = new_progress();
        let cfg = MemoryBenchmarkConfig {
            sizes: vec![64 * 1024],
            patterns: vec![AccessPattern::SequentialRead],
            iterations: 3,
            warmup_iterations: 0,
            thread_counts: vec![1],
            time_budget_ms: 0,
            ..MemoryBenchmarkConfig::default()
        };
        MemoryBenchmark::new(cfg.clone()).with_progress(handle.clone()).run().unwrap();
        assert_eq!(handle.lock().unwrap().completed.len(), 1);
        // a plain second run replaces the first...
        MemoryBenchmark::new(cfg.clone()).with_progress(handle.clone()).run().unwrap();
        assert_eq!(handle.lock().unwrap().completed.len(), 1);
        // ...a carried-over one adds to it and keeps the totals consistent
        MemoryBenchmark::new(cfg).with_progress(handle.clone()).carry_over(true).run().unwrap();
        let p = handle.lock().unwrap();
        assert_eq!((p.completed.len(), p.done_tests, p.total_tests), (2, 2, 2));
    }

    #[test]
    fn test_progress_reports_every_test() {
        let progress = new_progress();
        let mut b = MemoryBenchmark::new(MemoryBenchmarkConfig {
            sizes: vec![16 * 1024, 64 * 1024],
            iterations: 3,
            warmup_iterations: 1,
            patterns: vec![AccessPattern::SequentialRead, AccessPattern::PointerChase],
            thread_counts: vec![1, 2],
            ..MemoryBenchmarkConfig::default()
        })
        .with_progress(progress.clone());
        b.run().unwrap();
        let p = progress.lock().unwrap();
        assert_eq!(p.total_tests, 8);
        assert_eq!(p.done_tests, 8);
        assert_eq!(p.completed.len(), 8);
        assert!(p.completed.iter().all(|r| r.ns_per_access > 0.0));
    }

    #[test]
    fn test_thread_counts_limited_by_selected_cores() {
        let cfg = MemoryBenchmarkConfig {
            thread_counts: vec![1, 2, 4],
            core_ids: vec![0, 1],
            ..MemoryBenchmarkConfig::default()
        };
        assert_eq!(cfg.valid_thread_counts(), vec![1, 2]);
        assert_eq!(cfg.test_count(), cfg.sizes.len() * cfg.patterns.len() * 2);

        let mut b = MemoryBenchmark::new(MemoryBenchmarkConfig { thread_counts: vec![8], core_ids: vec![0], ..MemoryBenchmarkConfig::default() });
        assert!(b.run().is_err());
    }

    #[test]
    fn test_empty_selection_is_an_error() {
        let mut b = MemoryBenchmark::new(MemoryBenchmarkConfig { patterns: vec![], ..MemoryBenchmarkConfig::default() });
        assert!(b.run().is_err());
    }

    #[test]
    fn test_pointer_chase_visits_whole_cycle() {
        let b = bench();
        let prep = b.prepare(AccessPattern::PointerChase, 64 * 1024, 1);
        let Prepared::Chain { chain, pos } = prep else { panic!() };
        let lines = 64 * 1024 / 64;
        let mut seen = std::collections::HashSet::new();
        let start = pos[0].load(std::sync::atomic::Ordering::Relaxed);
        let mut cur = start;
        for _ in 0..lines {
            assert!(seen.insert(cur), "revisited a line before completing the cycle");
            cur = chain[cur];
        }
        assert_eq!(cur, start);
    }

    #[test]
    fn chase_carries_on_and_threads_start_apart() {
        use std::sync::atomic::Ordering::Relaxed;
        let b = bench();
        // 64 MB = 1 M lines: one pass of 65536 hops must not return to where it began
        let prep = b.prepare(AccessPattern::PointerChase, 64 << 20, 4);
        let Prepared::Chain { chain, pos } = &prep else { panic!() };
        let before: Vec<usize> = pos.iter().map(|p| p.load(Relaxed)).collect();
        b.pointer_chase(4, &prep);
        let after: Vec<usize> = pos.iter().map(|p| p.load(Relaxed)).collect();
        assert!(before.iter().zip(&after).all(|(a, b)| a != b), "each thread moved on");
        // thread 1 starts a quarter of the cycle after thread 0, not one hop behind it
        let (mut cur, mut hops) = (before[0], 0usize);
        while cur != before[1] {
            cur = chain[cur];
            hops += 1;
        }
        assert_eq!(hops, (64 << 20) / 64 / 4);
    }

    #[test]
    fn threads_split_on_cache_lines() {
        // 4 KB between 24 threads: 64 lines, no line shared by two threads
        for i in 0..24 {
            let r = share(4096, 24, i);
            assert_eq!(r.start % 64, 0);
            assert!(r.end % 64 == 0 || r.end == 4096);
        }
        assert_eq!((0..24).map(|i| share(4096, 24, i).len()).sum::<usize>(), 4096);
        assert_eq!((0..3).map(|i| share(1000, 3, i).len()).sum::<usize>(), 1000);
        assert_eq!(share(100, 3, 2).end, 100, "tiny buffers still split");
    }

    #[test]
    fn kernels_compute_what_they_claim() {
        let mut b = bench();
        let mut a = b.allocate_at(4096 + 13, 0).unwrap();
        let mut c = b.allocate_at(4096 + 13, AUX_OFFSET).unwrap();
        assert_eq!(a.as_ptr() as usize % 4096, 0);
        assert_eq!(c.as_ptr() as usize % 4096, AUX_OFFSET);
        // the read kernel adds whole 64-bit words (plus any unaligned edge bytes)
        let (h, w, t) = words(&a);
        let expect = w.iter().chain(h.iter().chain(t).map(|&b| b as u64).collect::<Vec<_>>().iter()).fold(0u64, |x, &y| x.wrapping_add(y));
        assert_eq!(sum_bytes(&a), expect);
        let (a0, c0) = (a.to_vec(), c.to_vec());
        add_bytes(&mut c, &a, true);
        // word-wise arithmetic (carries cross bytes): check through u64 lanes
        let (_, cw, _) = words(&c);
        let (_, c0w, _) = words(&c0);
        let (_, aw, _) = words(&a0);
        assert!(cw.iter().zip(c0w).zip(aw).all(|((x, y), z)| *x == y.wrapping_add(z.wrapping_mul(3))));
        update_bytes(&mut a, false);
        let (_, a1, _) = words(&a);
        assert!(a1.iter().zip(aw).all(|(x, y)| *x == y.wrapping_add(1)));
        fill_bytes(&mut a, 5);
        let (_, a2, _) = words(&a);
        assert!(a2.iter().enumerate().all(|(i, x)| *x == 5 ^ i as u64));
    }

    #[test]
    fn test_pinned_run_matches_unpinned_shape() {
        let mut b = MemoryBenchmark::new(MemoryBenchmarkConfig {
            sizes: vec![64 * 1024],
            iterations: 3,
            warmup_iterations: 0,
            patterns: vec![AccessPattern::RandomRead],
            thread_counts: vec![1],
            core_ids: vec![0],
            core_label: "Core 0".into(),
            ..MemoryBenchmarkConfig::default()
        });
        let s = b.run().unwrap();
        assert_eq!(s.results[0].cores, "Core 0");
    }

    #[test]
    fn test_per_core_sweep_runs_each_core_once() {
        let cores = vec![0, 1];
        let cfg = MemoryBenchmarkConfig {
            sizes: vec![32 * 1024],
            iterations: 3,
            warmup_iterations: 0,
            patterns: vec![AccessPattern::SequentialRead],
            thread_counts: vec![4, 8], // ignored in per-core mode
            core_ids: cores.clone(),
            core_label: "Cores 0,1".into(),
            per_core: true,
            ..MemoryBenchmarkConfig::default()
        };
        assert_eq!(cfg.test_count(), 2);
        let s = MemoryBenchmark::new(cfg).run().unwrap();
        let labels: Vec<_> = s.results.iter().map(|r| r.cores.as_str()).collect();
        assert_eq!(labels, vec!["Core 0", "Core 1"]);
        assert!(s.results.iter().all(|r| r.thread_count == 1));
    }

    #[test]
    fn test_results_carry_telemetry_when_sampler_attached() {
        let sampler = crate::sensors::Sampler::start(std::time::Duration::from_millis(50));
        std::thread::sleep(std::time::Duration::from_millis(150));
        let s = MemoryBenchmark::new(MemoryBenchmarkConfig {
            sizes: vec![64 * 1024],
            iterations: 3,
            warmup_iterations: 0,
            patterns: vec![AccessPattern::SequentialRead],
            ..MemoryBenchmarkConfig::default()
        })
        .with_sensors(sampler)
        .run()
        .unwrap();
        assert!(s.results[0].telemetry.samples >= 1);
        assert!(s.results[0].telemetry.ram_used_max_mb.unwrap_or(0.0) > 0.0);
    }
}

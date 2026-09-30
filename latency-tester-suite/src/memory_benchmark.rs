//! Memory latency benchmarks with different access patterns and sizes
//! Tests sequential, random, strided, and pointer-chasing access patterns

use anyhow::Result;
use rand::{Rng, SeedableRng};
use rand::rngs::StdRng;
use serde::{Deserialize, Serialize};
use crate::cancel::{self, CancelFlag};
use crate::timer::HighResTimer;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryBenchmarkConfig {
    pub sizes: Vec<usize>,           // Buffer sizes in bytes
    pub iterations: u32,             // Iterations per test
    pub warmup_iterations: u32,      // Warmup iterations
    pub patterns: Vec<AccessPattern>, // Access patterns to test
    pub thread_counts: Vec<usize>,   // Thread counts to test
    pub use_huge_pages: bool,        // Use huge pages if available
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
    fn default() -> Self {
        Self {
            sizes: vec![
                4 * 1024,           // 4 KB - L1 cache
                16 * 1024,          // 16 KB
                32 * 1024,          // 32 KB - L1
                64 * 1024,          // 64 KB
                128 * 1024,         // 128 KB
                256 * 1024,         // 256 KB - L2
                512 * 1024,         // 512 KB
                1024 * 1024,        // 1 MB - L2/L3 boundary
                2 * 1024 * 1024,    // 2 MB
                4 * 1024 * 1024,    // 4 MB - L3
                8 * 1024 * 1024,    // 8 MB
                16 * 1024 * 1024,   // 16 MB
                32 * 1024 * 1024,   // 32 MB
                64 * 1024 * 1024,   // 64 MB
                128 * 1024 * 1024,  // 128 MB
                256 * 1024 * 1024,  // 256 MB
                512 * 1024 * 1024,  // 512 MB
                1024 * 1024 * 1024, // 1 GB
            ],
            iterations: 100,
            warmup_iterations: 10,
            patterns: vec![
                AccessPattern::SequentialRead,
                AccessPattern::SequentialWrite,
                AccessPattern::RandomRead,
                AccessPattern::RandomWrite,
                AccessPattern::StridedRead { stride: 64 },    // Cache line stride
                AccessPattern::StridedRead { stride: 4096 },  // Page stride
                AccessPattern::PointerChase,
                AccessPattern::DependentRead,
                AccessPattern::IndependentRead,
                AccessPattern::StreamCopy,
                AccessPattern::StreamScale,
                AccessPattern::StreamAdd,
                AccessPattern::StreamTriad,
            ],
            thread_counts: vec![1, 2, 4, 8, 16, 32, 64],
            use_huge_pages: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryBenchmarkResult {
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryBenchmarkSummary {
    pub results: Vec<MemoryBenchmarkResult>,
    pub config: MemoryBenchmarkConfig,
    pub system_info: crate::system_info::SystemInfo,
    pub timestamp: String,
}

pub struct MemoryBenchmark {
    config: MemoryBenchmarkConfig,
    timer: HighResTimer,
    rng: StdRng,
    cancel: CancelFlag,
}

/// Per-(size, pattern, threads) setup data built once, outside the timed region and
/// reused by every iteration
enum Prepared {
    None,
    Indices(Vec<usize>),
    PerThread(Vec<Vec<usize>>),
    Chain { chain: Vec<usize>, starts: Vec<usize> },
}

/// Split `buf` into `tc` contiguous, disjoint, mutable chunks (remainder spread over the first chunks)
fn split_mut(mut buf: &mut [u8], tc: usize) -> Vec<&mut [u8]> {
    let tc = tc.max(1);
    let base = buf.len() / tc;
    let rem = buf.len() % tc;
    let mut out = Vec::with_capacity(tc);
    for i in 0..tc {
        let len = base + usize::from(i < rem);
        let (head, tail) = buf.split_at_mut(len);
        out.push(head);
        buf = tail;
    }
    out
}

/// Range of items owned by worker `i` when `n` items are shared between `tc` workers
fn share(n: usize, tc: usize, i: usize) -> std::ops::Range<usize> {
    let base = n / tc;
    let rem = n % tc;
    let start = i * base + i.min(rem);
    start..start + base + usize::from(i < rem)
}

impl MemoryBenchmark {
    pub fn new(config: MemoryBenchmarkConfig) -> Self {
        Self {
            config,
            timer: HighResTimer::new(),
            rng: StdRng::seed_from_u64(0xDEADBEEF_CAFEBABE),
            cancel: cancel::new_flag(),
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

        for size in self.config.sizes.clone() {
            cancel::check(&self.cancel)?;
            // Allocate once per size and reuse across all patterns and thread counts
            let mut data = self.allocate_buffer(size)?;
            let mut aux = self.allocate_buffer(size)?;

            for pattern in self.config.patterns.clone() {
                for &thread_count in &self.config.thread_counts.clone() {
                    cancel::check(&self.cancel)?;
                    if thread_count > num_cpus::get() {
                        continue; // Skip thread counts higher than available CPUs
                    }

                    let result =
                        self.run_single_test(&mut data, &mut aux, size, pattern, thread_count)?;
                    results.push(result);
                }
            }
        }

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
        // Warmup (single thread)
        let warm_prep = self.prepare(pattern, size, 1);
        for _ in 0..self.config.warmup_iterations {
            cancel::check(&self.cancel)?;
            self.run_pattern(data, aux, size, pattern, 1, &warm_prep)?;
        }
        let prep = if thread_count == 1 { warm_prep } else { self.prepare(pattern, size, thread_count) };

        // Actual benchmark
        let iterations = self.config.iterations.max(1);
        let mut latencies = Vec::with_capacity(iterations as usize);
        let mut total_bytes = 0u64;

        for _ in 0..iterations {
            cancel::check(&self.cancel)?;
            let (latency_ns, bytes) = self.run_pattern(data, aux, size, pattern, thread_count, &prep)?;
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

        Ok(MemoryBenchmarkResult {
            size,
            pattern,
            thread_count,
            latency_ns: avg,
            bandwidth_gb_s,
            iterations,
            min_latency_ns: min,
            max_latency_ns: max,
            std_dev_ns: std_dev,
            percentile_50_ns: p50,
            percentile_95_ns: p95,
            percentile_99_ns: p99,
            percentile_999_ns: p999,
        })
    }

    /// Allocate a buffer filled with pseudo-random data
    fn allocate_buffer(&mut self, size: usize) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        buf.try_reserve_exact(size)
            .map_err(|e| anyhow::anyhow!("Failed to allocate {} bytes: {}", size, e))?;
        buf.resize(size, 0);
        self.rng.fill(&mut buf[..]);
        Ok(buf)
    }

    /// Run `worker(i)` on `tc` scoped threads, timing only the parallel phase.
    /// Returns (total bytes reported by workers, elapsed nanoseconds).
    fn run_parallel<F>(&self, tc: usize, worker: F) -> (u64, f64)
    where
        F: Fn(usize) -> usize + Sync,
    {
        let start = self.timer.now_ticks();
        let total: usize = std::thread::scope(|s| {
            let worker = &worker;
            let handles: Vec<_> = (0..tc).map(|i| s.spawn(move || worker(i))).collect();
            handles.into_iter().map(|h| h.join().expect("benchmark worker panicked")).sum()
        });
        let elapsed = self.timer.now_ticks() - start;
        (total as u64, self.timer.ticks_to_ns(elapsed).max(1) as f64)
    }

    /// Like `run_parallel` but each worker gets exclusive access to one chunk of `buf`
    fn run_parallel_mut<F>(&self, buf: &mut [u8], tc: usize, worker: F) -> (u64, f64)
    where
        F: Fn(usize, &mut [u8]) -> usize + Sync,
    {
        let chunks = split_mut(buf, tc);
        let start = self.timer.now_ticks();
        let total: usize = std::thread::scope(|s| {
            let worker = &worker;
            let handles: Vec<_> = chunks
                .into_iter()
                .enumerate()
                .map(|(i, chunk)| s.spawn(move || worker(i, chunk)))
                .collect();
            handles.into_iter().map(|h| h.join().expect("benchmark worker panicked")).sum()
        });
        let elapsed = self.timer.now_ticks() - start;
        (total as u64, self.timer.ticks_to_ns(elapsed).max(1) as f64)
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
        Ok((ns, bytes))
    }

    // Sequential read - measures memory read bandwidth
    fn sequential_read(&self, buffer: &[u8], tc: usize) -> (u64, f64) {
        self.run_parallel(tc, |i| {
            let r = share(buffer.len(), tc, i);
            let mut sum = 0u64;
            for &val in &buffer[r.clone()] {
                sum = sum.wrapping_add(val as u64);
            }
            std::hint::black_box(sum);
            r.len()
        })
    }

    // Sequential write - measures memory write bandwidth
    fn sequential_write(&self, buffer: &mut [u8], tc: usize) -> (u64, f64) {
        let base = buffer.len() / tc;
        let rem = buffer.len() % tc;
        self.run_parallel_mut(buffer, tc, |i, chunk| {
            let start = i * base + i.min(rem);
            for (j, byte) in chunk.iter_mut().enumerate() {
                *byte = ((start + j) & 0xFF) as u8;
            }
            chunk.len()
        })
    }

    // Sequential read-write
    fn sequential_read_write(&self, buffer: &mut [u8], tc: usize) -> (u64, f64) {
        self.run_parallel_mut(buffer, tc, |_, chunk| {
            for byte in chunk.iter_mut() {
                *byte = byte.wrapping_add(1);
            }
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
                // Random permutation cycle over 8-byte nodes (a single long chain)
                let num_nodes = (size / 8).max(tc);
                let mut perm: Vec<usize> = (0..num_nodes).collect();
                perm.shuffle(&mut StdRng::seed_from_u64(0xBADF00D));
                let mut chain = vec![0usize; num_nodes];
                for i in 0..num_nodes {
                    chain[perm[i]] = perm[(i + 1) % num_nodes];
                }
                // Disjoint start nodes so threads do not share hot cache lines
                let starts = (0..tc).map(|i| perm[i % num_nodes]).collect();
                Prepared::Chain { chain, starts }
            }
            AccessPattern::DependentRead => {
                // Sequential chain: node i -> node i+1, last wraps to 0
                let num_nodes = (size / 8).max(tc);
                let chain = (0..num_nodes).map(|i| (i + 1) % num_nodes).collect();
                let starts = (0..tc).map(|i| (i * num_nodes) / tc).collect();
                Prepared::Chain { chain, starts }
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
        let Prepared::Chain { chain, starts } = prep else { return (0, 1.0) };
        let steps: usize = 1 << 16; // Chase depth per thread (latency-bound)
        self.run_parallel(tc, |i| {
            let mut current = starts[i];
            for _ in 0..steps {
                current = chain[current];
            }
            std::hint::black_box(current);
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

    // Stream scale - a = b * scalar
    fn stream_scale(&self, buffer: &mut [u8], tc: usize) -> (u64, f64) {
        self.run_parallel_mut(buffer, tc, |_, chunk| {
            for byte in chunk.iter_mut() {
                *byte = byte.wrapping_mul(3);
            }
            chunk.len() * 2 // Read + write
        })
    }

    // Stream add - a = b + c (reads `src`, writes `dst`)
    fn stream_add(&self, src: &[u8], dst: &mut [u8], tc: usize) -> (u64, f64) {
        let n = src.len();
        self.run_parallel_mut(dst, tc, |i, chunk| {
            let start = share(n, tc, i).start;
            for (j, byte) in chunk.iter_mut().enumerate() {
                let i = start + j;
                let a = src[i];
                let b = src[if i + 1 == n { 0 } else { i + 1 }];
                *byte = a.wrapping_add(b);
            }
            chunk.len() * 3 // 2 reads + 1 write
        })
    }

    // Stream triad - a = b + c * d (reads `src`, writes `dst`)
    fn stream_triad(&self, src: &[u8], dst: &mut [u8], tc: usize) -> (u64, f64) {
        let n = src.len();
        self.run_parallel_mut(dst, tc, |i, chunk| {
            let start = share(n, tc, i).start;
            for (j, byte) in chunk.iter_mut().enumerate() {
                let i = start + j;
                let b = src[i];
                let c = src[if i + 1 >= n { i + 1 - n } else { i + 1 }];
                let d = src[if i + 2 >= n { i + 2 - n } else { i + 2 }];
                *byte = b.wrapping_add(c.wrapping_mul(d));
            }
            chunk.len() * 4 // 3 reads + 1 write
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
pub fn quick_memory_latency_test(size: usize, iterations: u32, cancel: CancelFlag) -> Result<QuickMemoryResult> {
    let started = std::time::Instant::now();
    let mut bench = MemoryBenchmark::new(MemoryBenchmarkConfig {
        sizes: vec![size],
        iterations,
        warmup_iterations: 2,
        patterns: vec![AccessPattern::RandomRead, AccessPattern::PointerChase],
        thread_counts: vec![1],
        use_huge_pages: false,
    })
    .with_cancel(cancel);

    let summary = bench.run()?;
    let rows = summary
        .results
        .iter()
        .map(|r| {
            // bytes moved per run = bandwidth (bytes/ns) * run time (ns); one access per 64 B
            // (random read) or per 8 B node (pointer chase)
            let bytes = r.bandwidth_gb_s * r.latency_ns;
            let unit = if matches!(r.pattern, AccessPattern::PointerChase) { 8.0 } else { 64.0 };
            QuickMemoryRow {
                pattern: format!("{:?}", r.pattern),
                ns_per_access: if bytes > 0.0 { r.latency_ns / (bytes / unit) } else { 0.0 },
                bandwidth_gb_s: r.bandwidth_gb_s,
                p99_run_ms: r.percentile_99_ns / 1e6,
            }
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
        assert_eq!(src, dst);
    }

    #[test]
    fn test_quick_test_reports_per_access_latency() {
        let r = quick_memory_latency_test(256 * 1024, 3, cancel::new_flag()).unwrap();
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
        let err = quick_memory_latency_test(64 * 1024 * 1024, 1000, flag).unwrap_err();
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
            use_huge_pages: false,
        });
        let s = b.run().unwrap();
        assert_eq!(s.results.len(), 2);
        assert!(s.results.iter().all(|r| r.latency_ns > 0.0 && r.bandwidth_gb_s > 0.0));
    }
}

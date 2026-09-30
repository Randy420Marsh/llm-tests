//! Memory latency benchmarks with different access patterns and sizes
//! Tests sequential, random, strided, and pointer-chasing access patterns

use anyhow::Result;
use rand::{Rng, SeedableRng};
use rand::rngs::StdRng;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use crate::timer::{HighResTimer, IntervalTimer};

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
}

impl MemoryBenchmark {
    pub fn new(config: MemoryBenchmarkConfig) -> Self {
        Self {
            config,
            timer: HighResTimer::new(),
            rng: StdRng::seed_from_u64(0xDEADBEEF_CAFEBABE),
        }
    }

    pub fn run(&mut self) -> Result<MemoryBenchmarkSummary> {
        let system_info = crate::system_info::collect_system_info()?;
        let mut results = Vec::new();

        for &size in &self.config.sizes {
            // Allocate once per size and reuse across all patterns and thread counts
            let buffer = self.allocate_buffer(size)?;
            
            for &pattern in &self.config.patterns {
                for &thread_count in &self.config.thread_counts {
                    if thread_count > num_cpus::get() {
                        continue; // Skip thread counts higher than available CPUs
                    }
                    
                    let result = self.run_single_test(&buffer, size, pattern, thread_count)?;
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
        buffer: &Arc<Vec<u8>>,
        size: usize,
        pattern: AccessPattern,
        thread_count: usize,
    ) -> Result<MemoryBenchmarkResult> {
        // Warmup
        for _ in 0..self.config.warmup_iterations {
            self.run_pattern(buffer, size, pattern, 1)?;
        }

        // Actual benchmark
        let mut latencies = Vec::with_capacity(self.config.iterations as usize);
        let mut total_bytes = 0u64;

        for _ in 0..self.config.iterations {
            let (latency_ns, bytes) = self.run_pattern(buffer, size, pattern, thread_count)?;
            latencies.push(latency_ns);
            total_bytes += bytes;
        }

        // Calculate statistics
        latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
        
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
        let total_time_ns: f64 = latencies.iter().sum();
        let bandwidth_gb_s = total_bytes as f64 / total_time_ns; // GB/s

        Ok(MemoryBenchmarkResult {
            size,
            pattern,
            thread_count,
            latency_ns: avg,
            bandwidth_gb_s,
            iterations: self.config.iterations,
            min_latency_ns: min,
            max_latency_ns: max,
            std_dev_ns: std_dev,
            percentile_50_ns: p50,
            percentile_95_ns: p95,
            percentile_99_ns: p99,
            percentile_999_ns: p999,
        })
    }

    fn allocate_buffer(&mut self, size: usize) -> Result<Arc<Vec<u8>>> {
        // Use aligned allocation for better performance
        let align = 4096; // Page aligned
        let layout = std::alloc::Layout::from_size_align(size, align)?;
        let ptr = unsafe { std::alloc::alloc(layout) };
        
        if ptr.is_null() {
            return Err(anyhow::anyhow!("Failed to allocate {} bytes", size));
        }

        // Initialize with random data
        let slice = unsafe { std::slice::from_raw_parts_mut(ptr, size) };
        self.rng.fill(slice);

        Ok(Arc::new(unsafe { Vec::from_raw_parts(ptr, size, size) }))
    }

    fn run_pattern(
        &mut self,
        buffer: &Arc<Vec<u8>>,
        size: usize,
        pattern: AccessPattern,
        thread_count: usize,
    ) -> Result<(f64, u64)> {
        let mut interval = IntervalTimer::new();
        
        let bytes_accessed = match pattern {
            AccessPattern::SequentialRead => self.sequential_read(buffer, size, thread_count)?,
            AccessPattern::SequentialWrite => self.sequential_write(buffer, size, thread_count)?,
            AccessPattern::SequentialReadWrite => self.sequential_read_write(buffer, size, thread_count)?,
            AccessPattern::RandomRead => self.random_read(buffer, size, thread_count)?,
            AccessPattern::RandomWrite => self.random_write(buffer, size, thread_count)?,
            AccessPattern::StridedRead { stride } => self.strided_read(buffer, size, stride, thread_count)?,
            AccessPattern::PointerChase => self.pointer_chase(buffer, size, thread_count)?,
            AccessPattern::DependentRead => self.dependent_read(buffer, size, thread_count)?,
            AccessPattern::IndependentRead => self.independent_read(buffer, size, thread_count)?,
            AccessPattern::StreamCopy => self.stream_copy(buffer, size, thread_count)?,
            AccessPattern::StreamScale => self.stream_scale(buffer, size, thread_count)?,
            AccessPattern::StreamAdd => self.stream_add(buffer, size, thread_count)?,
            AccessPattern::StreamTriad => self.stream_triad(buffer, size, thread_count)?,
        };

        let elapsed_ns = interval.lap_ns() as f64;
        Ok((elapsed_ns, bytes_accessed))
    }

    // Sequential read - measures memory read bandwidth
    fn sequential_read(&self, buffer: &[u8], size: usize, thread_count: usize) -> Result<u64> {
        let chunk_size = size / thread_count.max(1);
        let mut handles = Vec::new();
        
        for i in 0..thread_count {
            let buf = buffer.clone();
            let start = i * chunk_size;
            let end = if i == thread_count - 1 { size } else { (i + 1) * chunk_size };
            
            let handle = std::thread::spawn(move || {
                let mut sum = 0u64;
                let slice = &buf[start..end];
                for &val in slice {
                    sum = sum.wrapping_add(val as u64);
                }
                std::hint::black_box(sum);
                end - start
            });
            handles.push(handle);
        }

        let mut total = 0;
        for handle in handles {
            total += handle.join().unwrap();
        }
        Ok(total as u64)
    }

    // Sequential write - measures memory write bandwidth
    fn sequential_write(&self, buffer: &[u8], size: usize, thread_count: usize) -> Result<u64> {
        // Need mutable access - use a copy for benchmarking
        let mut buf = buffer.to_vec();
        let tc = thread_count.max(1);
        let base = size / tc;
        let rem = size % tc;
        let mut offset = 0;
        let mut handles = Vec::new();
        
        for i in 0..tc {
            let len = base + if i < rem { 1 } else { 0 };
            let start = offset;
            let chunk = &mut buf[offset..offset + len];
            offset += len;
            
            let handle = std::thread::spawn(move || {
                for (j, byte) in chunk.iter_mut().enumerate() {
                    *byte = ((start + j) & 0xFF) as u8;
                }
                len
            });
            handles.push(handle);
        }

        let mut total = 0;
        for handle in handles {
            total += handle.join().unwrap();
        }
        Ok(total as u64)
    }

    // Sequential read-write
    fn sequential_read_write(&self, buffer: &[u8], size: usize, thread_count: usize) -> Result<u64> {
        let mut buf = buffer.to_vec();
        let tc = thread_count.max(1);
        let base = size / tc;
        let rem = size % tc;
        let mut offset = 0;
        let mut handles = Vec::new();
        
        for i in 0..tc {
            let len = base + if i < rem { 1 } else { 0 };
            let chunk = &mut buf[offset..offset + len];
            offset += len;
            
            let handle = std::thread::spawn(move || {
                for byte in chunk.iter_mut() {
                    let val = *byte;
                    *byte = val.wrapping_add(1);
                }
                len * 2 // Read + write
            });
            handles.push(handle);
        }

        let mut total = 0;
        for handle in handles {
            total += handle.join().unwrap();
        }
        Ok(total as u64)
    }

    // Random read - measures random access latency
    fn random_read(&self, buffer: &[u8], size: usize, thread_count: usize) -> Result<u64> {
        let indices: Vec<usize> = (0..size).step_by(64).collect(); // Cache line aligned
        let mut rng = StdRng::seed_from_u64(0xFEEDFACE);
        let mut shuffled = indices.clone();
        use rand::seq::SliceRandom;
        shuffled.shuffle(&mut rng);
        
        let tc = thread_count.max(1);
        let base = shuffled.len() / tc;
        let rem = shuffled.len() % tc;
        let mut handles = Vec::new();
        
        for i in 0..tc {
            let buf = buffer.clone();
            let start = i * base + i.min(rem);
            let end = start + base + if i < rem { 1 } else { 0 };
            let indices = shuffled[start..end].to_vec();
            
            let handle = std::thread::spawn(move || {
                let mut sum = 0u64;
                for &idx in &indices {
                    sum = sum.wrapping_add(buf[idx] as u64);
                }
                std::hint::black_box(sum);
                indices.len() * 64
            });
            handles.push(handle);
        }

        let mut total = 0;
        for handle in handles {
            total += handle.join().unwrap();
        }
        Ok(total as u64)
    }

    // Random write
    fn random_write(&self, buffer: &[u8], size: usize, thread_count: usize) -> Result<u64> {
        let mut buf = buffer.to_vec();
        let tc = thread_count.max(1);
        let base = size / tc;
        let rem = size % tc;
        let mut offset = 0;
        let mut handles = Vec::new();
        
        for i in 0..tc {
            let len = base + if i < rem { 1 } else { 0 };
            // Random cache-line-aligned offsets within this thread's private region
            let indices: Vec<usize> = (0..len).step_by(64).collect();
            let mut rng = rand::rngs::StdRng::seed_from_u64(0xFEEDFACEu64.wrapping_add(i as u64));
            let mut shuffled = indices;
            use rand::seq::SliceRandom;
            shuffled.shuffle(&mut rng);
            
            let chunk = &mut buf[offset..offset + len];
            offset += len;
            
            let handle = std::thread::spawn(move || {
                for &idx in &shuffled {
                    chunk[idx] = (idx & 0xFF) as u8;
                }
                shuffled.len() * 64
            });
            handles.push(handle);
        }

        let mut total = 0;
        for handle in handles {
            total += handle.join().unwrap();
        }
        Ok(total as u64)
    }

    // Strided read - measures cache behavior with specific stride
    fn strided_read(&self, buffer: &[u8], size: usize, stride: usize, thread_count: usize) -> Result<u64> {
        let chunk_size = size / thread_count.max(1);
        let mut handles = Vec::new();
        
        for i in 0..thread_count {
            let buf = buffer.clone();
            let start = i * chunk_size;
            let end = if i == thread_count - 1 { size } else { (i + 1) * chunk_size };
            
            let handle = std::thread::spawn(move || {
                let mut sum = 0u64;
                let mut idx = start;
                while idx < end {
                    sum = sum.wrapping_add(buf[idx] as u64);
                    idx += stride;
                }
                std::hint::black_box(sum);
                ((end - start) / stride) * 64
            });
            handles.push(handle);
        }

        let mut total = 0;
        for handle in handles {
            total += handle.join().unwrap();
        }
        Ok(total as u64)
    }

    // Pointer chasing - measures pointer dereference latency
    fn pointer_chase(&self, _buffer: &[u8], size: usize, thread_count: usize) -> Result<u64> {
        // Build a random permutation cycle over 8-byte nodes (a single long chain)
        let num_nodes = (size / 8).max(thread_count.max(1));
        let mut buf: Vec<usize> = vec![0usize; num_nodes];
        let mut perm: Vec<usize> = (0..num_nodes).collect();
        let mut rng = rand::rngs::StdRng::seed_from_u64(0xBADF00D);
        use rand::seq::SliceRandom;
        perm.shuffle(&mut rng);
        
        // Node perm[i] points to perm[i+1]; last wraps to first (circular)
        for i in 0..num_nodes {
            buf[perm[i]] = perm[(i + 1) % num_nodes];
        }
        
        let tc = thread_count.max(1);
        let steps: usize = 512; // Chase depth per thread (latency-bound)
        let base_ptr = buf.as_ptr() as usize;
        let mut handles = Vec::new();
        
        for i in 0..tc {
            // Disjoint start nodes so threads do not share hot cache lines
            let start = perm[i % num_nodes];
            
            let handle = std::thread::spawn(move || {
                let mut current = start;
                let ptr = base_ptr as *const usize;
                for _ in 0..steps {
                    current = unsafe { *ptr.add(current) };
                }
                std::hint::black_box(current);
                steps * 8
            });
            handles.push(handle);
        }

        let mut total = 0;
        for handle in handles {
            total += handle.join().unwrap();
        }
        Ok(total as u64)
    }

    // Dependent read - each read address depends on previous read value
    fn dependent_read(&self, _buffer: &[u8], size: usize, thread_count: usize) -> Result<u64> {
        // Sequential chain: node i -> node i+1, last wraps to 0
        let num_nodes = (size / 8).max(thread_count.max(1));
        let buf: Vec<usize> = (0..num_nodes).map(|i| (i + 1) % num_nodes).collect();
        
        let tc = thread_count.max(1);
        let steps: usize = 512;
        let base_ptr = buf.as_ptr() as usize;
        let mut handles = Vec::new();
        
        for i in 0..tc {
            // Disjoint sequential start positions
            let start = ((i * num_nodes) / tc).min(num_nodes - 1);
            
            let handle = std::thread::spawn(move || {
                let mut current = start;
                let ptr = base_ptr as *const usize;
                for _ in 0..steps {
                    current = unsafe { *ptr.add(current) };
                }
                std::hint::black_box(current);
                steps * 8
            });
            handles.push(handle);
        }

        let mut total = 0;
        for handle in handles {
            total += handle.join().unwrap();
        }
        Ok(total as u64)
    }

    // Independent reads - multiple independent memory accesses (bandwidth bound)
    fn independent_read(&self, buffer: &[u8], size: usize, thread_count: usize) -> Result<u64> {
        let indices: Vec<usize> = (0..size).step_by(64).collect();
        let tc = thread_count.max(1);
        let base = indices.len() / tc;
        let rem = indices.len() % tc;
        let mut handles = Vec::new();
        
        for i in 0..tc {
            let buf = buffer.clone();
            let start = i * base + i.min(rem);
            let end = start + base + if i < rem { 1 } else { 0 };
            let indices = indices[start..end].to_vec();
            
            let handle = std::thread::spawn(move || {
                let mut sums = [0u64; 4]; // 4 independent accumulators
                for (j, &idx) in indices.iter().enumerate() {
                    sums[j % 4] = sums[j % 4].wrapping_add(buf[idx] as u64);
                }
                for s in sums {
                    std::hint::black_box(s);
                }
                indices.len() * 64
            });
            handles.push(handle);
        }

        let mut total = 0;
        for handle in handles {
            total += handle.join().unwrap();
        }
        Ok(total as u64)
    }

    // Stream copy - memcpy pattern
    fn stream_copy(&self, buffer: &[u8], size: usize, thread_count: usize) -> Result<u64> {
        let tc = thread_count.max(1);
        let base = size / tc;
        let rem = size % tc;
        let mut handles = Vec::new();
        
        for i in 0..tc {
            let start = i * base + i.min(rem);
            let len = base + if i < rem { 1 } else { 0 };
            let src = buffer.clone();
            
            let handle = std::thread::spawn(move || {
                let mut dst = vec![0u8; len];
                dst.copy_from_slice(&src[start..start + len]);
                len * 2 // Read + write
            });
            handles.push(handle);
        }

        let mut total = 0;
        for handle in handles {
            total += handle.join().unwrap();
        }
        Ok(total as u64)
    }

    // Stream scale - a = b * scalar
    fn stream_scale(&self, buffer: &[u8], size: usize, thread_count: usize) -> Result<u64> {
        let mut buf = buffer.to_vec();
        let tc = thread_count.max(1);
        let base = size / tc;
        let rem = size % tc;
        let mut offset = 0;
        let mut handles = Vec::new();
        
        for i in 0..tc {
            let len = base + if i < rem { 1 } else { 0 };
            let chunk = &mut buf[offset..offset + len];
            offset += len;
            
            let handle = std::thread::spawn(move || {
                for byte in chunk.iter_mut() {
                    *byte = byte.wrapping_mul(3);
                }
                len * 2 // Read + write
            });
            handles.push(handle);
        }

        let mut total = 0;
        for handle in handles {
            total += handle.join().unwrap();
        }
        Ok(total as u64)
    }

    // Stream add - a = b + c
    fn stream_add(&self, buffer: &[u8], size: usize, thread_count: usize) -> Result<u64> {
        let mut buf = buffer.to_vec();
        let shared = buffer.to_vec(); // Read-only neighbor access (cross-region reads)
        let tc = thread_count.max(1);
        let base = size / tc;
        let rem = size % tc;
        let mut offset = 0;
        let mut handles = Vec::new();
        
        for i in 0..tc {
            let len = base + if i < rem { 1 } else { 0 };
            let start = offset;
            let chunk = &mut buf[offset..offset + len];
            offset += len;
            
            let handle = std::thread::spawn(move || {
                for (j, byte) in chunk.iter_mut().enumerate() {
                    let a = shared[start + j];
                    let b = shared[(start + j + 1) % size];
                    *byte = a.wrapping_add(b);
                }
                len * 3 // 2 reads + 1 write
            });
            handles.push(handle);
        }

        let mut total = 0;
        for handle in handles {
            total += handle.join().unwrap();
        }
        Ok(total as u64)
    }

    // Stream triad - a = b + c * d
    fn stream_triad(&self, buffer: &[u8], size: usize, thread_count: usize) -> Result<u64> {
        let mut buf = buffer.to_vec();
        let shared = buffer.to_vec(); // Read-only neighbor access (cross-region reads)
        let tc = thread_count.max(1);
        let base = size / tc;
        let rem = size % tc;
        let mut offset = 0;
        let mut handles = Vec::new();
        
        for i in 0..tc {
            let len = base + if i < rem { 1 } else { 0 };
            let start = offset;
            let chunk = &mut buf[offset..offset + len];
            offset += len;
            
            let handle = std::thread::spawn(move || {
                for (j, byte) in chunk.iter_mut().enumerate() {
                    let b = shared[start + j];
                    let c = shared[(start + j + 1) % size];
                    let d = shared[(start + j + 2) % size];
                    *byte = b.wrapping_add(c.wrapping_mul(d));
                }
                len * 4 // 3 reads + 1 write
            });
            handles.push(handle);
        }

        let mut total = 0;
        for handle in handles {
            total += handle.join().unwrap();
        }
        Ok(total as u64)
    }
}

/// Quick memory latency test (simplified for GUI)
pub fn quick_memory_latency_test(size: usize, iterations: u32) -> Result<f64> {
    let mut bench = MemoryBenchmark::new(MemoryBenchmarkConfig {
        sizes: vec![size],
        iterations,
        warmup_iterations: 5,
        patterns: vec![AccessPattern::RandomRead, AccessPattern::PointerChase],
        thread_counts: vec![1],
        use_huge_pages: false,
    });
    
    let summary = bench.run()?;
    let avg_latency = summary.results.iter()
        .map(|r| r.latency_ns)
        .sum::<f64>() / summary.results.len() as f64;
    
    Ok(avg_latency)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sequential_read() {
        let config = MemoryBenchmarkConfig::default();
        let mut bench = MemoryBenchmark::new(config);
        let buffer = bench.allocate_buffer(1024 * 1024).unwrap();
        let bytes = bench.sequential_read(&buffer, 1024 * 1024, 1).unwrap();
        assert_eq!(bytes, 1024 * 1024);
    }

    #[test]
    fn test_pointer_chase() {
        let config = MemoryBenchmarkConfig::default();
        let mut bench = MemoryBenchmark::new(config);
        let buffer = bench.allocate_buffer(1024 * 1024).unwrap();
        let bytes = bench.pointer_chase(&buffer, 1024 * 1024, 1).unwrap();
        assert!(bytes > 0);
    }
}
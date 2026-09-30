//! Headless command-line mode: runs a quick pass of every suite and prints a report

use anyhow::{anyhow, Result};
use serde_json::json;
use std::collections::HashMap;

use crate::cpu_benchmark::{AffinityMode, CpuBenchmark, CpuBenchmarkConfig, WorkloadType};
use crate::gpu_benchmark::{GpuBenchmark, GpuBenchmarkConfig};
use crate::input_latency::{InputLatencyConfig, InputLatencyTester, InputTestMode};
use crate::memory_benchmark::{AccessPattern, MemoryBenchmark, MemoryBenchmarkConfig};
use crate::result_logger::ResultLogger;

pub const USAGE: &str = "\
Latency Tester Suite

USAGE:
    latency-tester                 Launch the GUI
    latency-tester --cli [OPTIONS] Run a quick headless pass of all suites

OPTIONS (with --cli):
    --out <DIR>     Directory for signed result files (default: ./latency_results in the current directory)
    --skip <SUITE>  Skip a suite: memory, cpu, gpu, input (repeatable)
    -h, --help      Show this help";

struct Options {
    out_dir: String,
    skip: Vec<String>,
}

fn parse(args: &[String]) -> Result<Options> {
    let mut opts = Options { out_dir: "latency_results".to_string(), skip: Vec::new() };
    let mut it = args.iter().filter(|a| *a != "--cli");
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--out" => opts.out_dir = it.next().ok_or_else(|| anyhow!("--out needs a value"))?.clone(),
            "--skip" => {
                let v = it.next().ok_or_else(|| anyhow!("--skip needs a value"))?;
                if !["memory", "cpu", "gpu", "input"].contains(&v.as_str()) {
                    return Err(anyhow!("unknown suite '{}'", v));
                }
                opts.skip.push(v.clone());
            }
            other => return Err(anyhow!("unknown argument '{}'\n\n{}", other, USAGE)),
        }
    }
    Ok(opts)
}

pub fn run(args: &[String]) -> Result<()> {
    let opts = parse(args)?;
    let run_suite = |name: &str| !opts.skip.iter().any(|s| s == name);

    let logger = ResultLogger::new(env!("CARGO_PKG_VERSION").to_string(), opts.out_dir.clone())?;
    let sys = crate::system_info::collect_system_info()?;
    println!("CPU: {} ({} cores / {} threads)", sys.cpu.name, sys.cpu.cores, sys.cpu.threads);

    let timer = crate::input_latency::measure_timer_resolution()?;
    println!(
        "Timer: {} Hz, overhead {} ns, min interval {} ns",
        timer.frequency_hz, timer.overhead_ns, timer.min_measurable_interval_ns
    );

    let mut failures = Vec::new();
    let log = |kind: &str, config: serde_json::Value, results: serde_json::Value| -> Result<()> {
        let signed = logger.log_result(kind, &sys, &config, &results, HashMap::new())?;
        let verdict = logger.verify_result(&signed);
        println!("  logged + verified: {} ({})", verdict.valid, verdict.message);
        if verdict.valid { Ok(()) } else { Err(anyhow!("verification failed for {}", kind)) }
    };

    if run_suite("memory") {
        println!("== Memory ==");
        let summary = MemoryBenchmark::new(MemoryBenchmarkConfig {
            sizes: vec![32 * 1024, 1024 * 1024, 32 * 1024 * 1024],
            iterations: 10,
            warmup_iterations: 2,
            patterns: vec![
                AccessPattern::SequentialRead,
                AccessPattern::RandomRead,
                AccessPattern::PointerChase,
                AccessPattern::StreamTriad,
            ],
            thread_counts: vec![1],
            use_huge_pages: false,
        })
        .run()?;
        for r in &summary.results {
            println!(
                "  {:>9} B {:<22} {:>12.0} ns  {:>8.2} GB/s",
                r.size, format!("{:?}", r.pattern), r.latency_ns, r.bandwidth_gb_s
            );
        }
        log("memory", json!(summary.config), json!(summary))
            .unwrap_or_else(|e| failures.push(e.to_string()));
    }

    if run_suite("cpu") {
        println!("== CPU ==");
        let summary = CpuBenchmark::new(CpuBenchmarkConfig {
            workload_types: vec![WorkloadType::IntegerAdd, WorkloadType::FloatFma, WorkloadType::GameSim],
            thread_counts: vec![1],
            affinity_modes: vec![AffinityMode::AllCores],
            duration_seconds: 1,
            warmup_seconds: 1,
            iterations: 2,
        })?
        .run()?;
        for r in &summary.results {
            println!(
                "  {:<14} {:>10.2} M calls/s  {:>10.0} ns/call  {} MHz",
                format!("{:?}", r.workload),
                r.operations_per_second / 1e6,
                r.latency_ns,
                r.frequency_mhz
            );
        }
        log("cpu", json!(summary.config), json!(summary))
            .unwrap_or_else(|e| failures.push(e.to_string()));
    }

    if run_suite("gpu") {
        println!("== GPU (Vulkan compute) ==");
        match GpuBenchmark::new(GpuBenchmarkConfig {
            workload_sizes: vec![1 << 18, 1 << 20],
            iterations: 20,
            warmup_iterations: 3,
        }) {
            Ok(mut bench) => {
                bench.verify_shader(1 << 14)?;
                println!("  shader output verified against CPU reference");
                let summary = bench.run()?;
                println!("  device: {} ({})", summary.vulkan_info.device_name, summary.vulkan_info.device_type);
                for r in &summary.results {
                    println!(
                        "  {:>9} elems  avg {:.3} ms  p99 {:.3} ms  {:.1} GOPS",
                        r.workload_size, r.avg_latency_ms, r.percentile_99_ms, r.throughput_geops
                    );
                }
                log("gpu", json!(summary.config), json!(summary))
                    .unwrap_or_else(|e| failures.push(e.to_string()));
            }
            Err(e) => println!("  skipped: no usable Vulkan device ({})", e),
        }
    }

    if run_suite("input") {
        println!("== Input stack timing ==");
        let summary = InputLatencyTester::new(InputLatencyConfig {
            test_modes: vec![InputTestMode::MouseMove, InputTestMode::RawInput, InputTestMode::Jitter],
            sample_count: 200,
            warmup_samples: 0,
            delay_range_ms: (0, 0),
            measure_display_latency: false,
        })
        .run()?;
        for r in &summary.results {
            println!(
                "  {:<10} avg {:.4} ms  p99 {:.4} ms  jitter {:.4} ms",
                format!("{:?}", r.mode), r.avg_latency_ms, r.percentile_99_ms, r.jitter_ms
            );
        }
        log("input", json!(summary.config), json!(summary))
            .unwrap_or_else(|e| failures.push(e.to_string()));
    }

    println!("Signing public key: {}", logger.public_key_hex());
    match logger.verify_log() {
        Ok(v) if v.valid => println!("Log integrity: {}", v.message),
        Ok(v) => failures.push(format!("log integrity: {}", v.message)),
        Err(e) => failures.push(format!("log integrity: {}", e)),
    }

    if failures.is_empty() {
        println!("All requested suites completed.");
        Ok(())
    } else {
        Err(anyhow!("{} logging failure(s): {}", failures.len(), failures.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_options() {
        let o = parse(&a(&["--cli", "--out", "x", "--skip", "gpu", "--skip", "cpu"])).unwrap();
        assert_eq!(o.out_dir, "x");
        assert_eq!(o.skip, vec!["gpu", "cpu"]);
    }

    #[test]
    fn rejects_bad_options() {
        assert!(parse(&a(&["--cli", "--bogus"])).is_err());
        assert!(parse(&a(&["--cli", "--skip", "nope"])).is_err());
        assert!(parse(&a(&["--cli", "--out"])).is_err());
    }
}

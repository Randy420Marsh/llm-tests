//! Headless command-line mode: runs a quick pass of every suite and prints a report

use anyhow::{anyhow, Result};
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
    latency-tester --serve [OPTIONS]   Serve saved results as a web page (read-only)
    latency-tester --report [FILES]    Write a self-contained HTML report of saved results

OPTIONS (with --serve):
    --dir <DIR>     Results folder (default: latency_results next to the program)
    --port <N>      Port (default 8787; 0 = any free port)
    --bind <ADDR>   Address to listen on (default 127.0.0.1; use 0.0.0.0 to view from other computers)
    --open          Open the page in the default browser

OPTIONS (with --report):
    --dir <DIR>     Read every result in this folder (default: latency_results next to the program)
    --out <FILE>    Output file (default: latency_report.html)
    FILES           Or list individual result .json files

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

fn default_results_dir() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("latency_results")))
        .filter(|d| d.is_dir())
        .unwrap_or_else(|| std::path::PathBuf::from("latency_results"))
}

/// `--serve`: blocks until interrupted
pub fn serve(args: &[String]) -> Result<()> {
    let (mut dir, mut port, mut bind, mut open) = (default_results_dir(), 8787u16, "127.0.0.1".to_string(), false);
    let mut it = args.iter().filter(|a| *a != "--serve");
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dir" => dir = it.next().ok_or_else(|| anyhow!("--dir needs a value"))?.into(),
            "--port" => port = it.next().ok_or_else(|| anyhow!("--port needs a value"))?.parse().map_err(|_| anyhow!("invalid port"))?,
            "--bind" => bind = it.next().ok_or_else(|| anyhow!("--bind needs a value"))?.clone(),
            "--open" => open = true,
            other => return Err(anyhow!("unknown argument '{}'\n\n{}", other, USAGE)),
        }
    }
    if !dir.is_dir() {
        return Err(anyhow!("results folder '{}' does not exist", dir.display()));
    }
    let server = crate::server::Server::start(dir.clone(), &bind, port)?;
    println!("Serving {} result(s) from {}", crate::report::list(&dir).len(), dir.display());
    println!("Open {}", server.url());
    if bind != "127.0.0.1" && bind != "localhost" {
        println!("Note: listening on {} — anyone who can reach this port can read the results (read-only).", bind);
    }
    if open {
        crate::server::open_in_browser(&server.url());
    }
    println!("Press Ctrl+C to stop.");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

/// `--report`: one self-contained HTML file
pub fn report(args: &[String]) -> Result<()> {
    let (mut dir, mut out, mut files) = (None::<std::path::PathBuf>, std::path::PathBuf::from("latency_report.html"), Vec::<std::path::PathBuf>::new());
    let mut it = args.iter().filter(|a| *a != "--report");
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dir" => dir = Some(it.next().ok_or_else(|| anyhow!("--dir needs a value"))?.into()),
            "--out" => out = it.next().ok_or_else(|| anyhow!("--out needs a value"))?.into(),
            f if !f.starts_with("--") => files.push(f.into()),
            other => return Err(anyhow!("unknown argument '{}'\n\n{}", other, USAGE)),
        }
    }
    let entries = if files.is_empty() {
        crate::report::list(&dir.unwrap_or_else(default_results_dir))
    } else {
        crate::report::load_files(&files)
    };
    if entries.is_empty() {
        return Err(anyhow!("no readable result files found"));
    }
    std::fs::write(&out, crate::report::render_static(&entries))?;
    println!("Wrote {} result(s) to {}", entries.len(), out.display());
    Ok(())
}

pub fn run(args: &[String]) -> Result<()> {
    use crate::session::{self, Scope, SessionData};
    let opts = parse(args)?;
    let run_suite = |name: &str| !opts.skip.iter().any(|s| s == name);

    let logger = ResultLogger::new(env!("CARGO_PKG_VERSION").to_string(), opts.out_dir.clone())?;
    let sys = crate::system_info::collect_system_info()?;
    println!("CPU: {} ({} cores / {} threads)", sys.cpu.name, sys.cpu.cores, sys.cpu.threads);
    if let Some(g) = &sys.gpu {
        println!("GPU: {}", g.name);
    }

    let timer = crate::input_latency::measure_timer_resolution()?;
    println!(
        "Timer: {} Hz, overhead {} ns, min interval {} ns",
        timer.frequency_hz, timer.overhead_ns, timer.min_measurable_interval_ns
    );

    // Temperatures, clocks, RAM and GPU/VRAM are recorded for every test
    let sampler = crate::sensors::Sampler::start(std::time::Duration::from_millis(500));

    let mut mem_cfg = None;
    let mut mem_results = Vec::new();
    let mut cpu_cfg = None;
    let mut cpu_summary = None;
    let mut gpu_cfg = None;
    let mut gpu_summary = None;
    let mut input_summary = None;

    if run_suite("memory") {
        println!("== Memory ==");
        let cfg = MemoryBenchmarkConfig {
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
            ..MemoryBenchmarkConfig::default()
        };
        let summary = MemoryBenchmark::new(cfg.clone()).with_sensors(sampler.clone()).run()?;
        for r in &summary.results {
            println!(
                "  {:>9} B {:<22} {:>8.1} ns/access {:>8.2} GB/s",
                r.size, r.pattern.label(), r.ns_per_access, r.bandwidth_gb_s
            );
        }
        mem_cfg = Some(cfg);
        mem_results = summary.results;
    }

    if run_suite("cpu") {
        println!("== CPU ==");
        let cfg = CpuBenchmarkConfig {
            workload_types: vec![WorkloadType::IntegerAdd, WorkloadType::FloatFma, WorkloadType::GameSim],
            thread_counts: vec![1],
            affinity_modes: vec![AffinityMode::AllCores],
            duration_seconds: 1,
            warmup_seconds: 1,
            iterations: 2,
        };
        let summary = CpuBenchmark::new(cfg.clone())?.with_sensors(sampler.clone()).run()?;
        for r in &summary.results {
            println!(
                "  {:<14} {:>10.2} M calls/s  {:>10.0} ns/call  {} MHz{}",
                format!("{:?}", r.workload),
                r.operations_per_second / 1e6,
                r.latency_ns,
                r.frequency_mhz,
                r.telemetry.cpu_temp_max_c.map(|t| format!("  {:.0} °C", t)).unwrap_or_default()
            );
        }
        cpu_cfg = Some(cfg);
        cpu_summary = Some(summary);
    }

    if run_suite("gpu") {
        println!("== GPU (Vulkan compute) ==");
        let cfg = GpuBenchmarkConfig { workload_sizes: vec![1 << 18, 1 << 20], iterations: 20, warmup_iterations: 3, min_sample_ms: 1000 };
        match GpuBenchmark::new(cfg.clone()) {
            Ok(bench) => {
                let mut bench = bench.with_sensors(sampler.clone());
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
                gpu_cfg = Some(cfg);
                gpu_summary = Some(summary);
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
            pin_core: None,
        })
        .with_sensors(sampler.clone())
        .run()?;
        for r in &summary.results {
            println!(
                "  {:<10} avg {:.4} ms  p99 {:.4} ms  jitter {:.4} ms",
                format!("{:?}", r.mode), r.avg_latency_ms, r.percentile_99_ms, r.jitter_ms
            );
        }
        input_summary = Some(summary);
    }

    sampler.stop();
    let timeline = sampler.timeline();
    let notes = sampler.notes();
    for n in &notes {
        println!("  sensor: {}", n);
    }

    // One signed record with everything that was measured
    let virtualization = crate::virtualization::VirtualizationDetector::detect().ok().and_then(|v| serde_json::to_value(v).ok());
    let data = SessionData {
        memory_config: mem_cfg.as_ref(),
        memory: &mem_results,
        memory_planned: mem_cfg.as_ref().map(|c| c.test_count()).unwrap_or(0),
        cpu_config: cpu_cfg.as_ref(),
        cpu_topology: cpu_summary.as_ref().map(|s| &s.core_topology),
        cpu: cpu_summary.as_ref().map(|s| s.results.as_slice()).unwrap_or(&[]),
        gpu_config: gpu_cfg.as_ref(),
        gpu_vulkan: gpu_summary.as_ref().map(|s| &s.vulkan_info),
        gpu: gpu_summary.as_ref().map(|s| s.results.as_slice()).unwrap_or(&[]),
        input_suite: input_summary.as_ref(),
        trials: &[],
        timeline: &timeline,
        sensor_notes: &notes,
        virtualization,
        calibration: None,
        ..Default::default()
    };
    let mut failures = Vec::new();
    if session::has_data(&data, Scope::Everything) {
        let (config, results) = session::build(&data, Scope::Everything);
        let signed = logger.log_result(Scope::Everything.test_type(), &sys, &config, &results, HashMap::new())?;
        let verdict = logger.verify_result(&signed);
        println!("Saved one signed session record: {} ({})", verdict.valid, verdict.message);
        if !verdict.valid {
            failures.push(format!("verification failed: {}", verdict.message));
        }
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

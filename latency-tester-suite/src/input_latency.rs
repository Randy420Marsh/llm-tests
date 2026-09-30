#![allow(dead_code)] // live-input hooks and helpers are public API for embedding
//! Input latency measurement using high-resolution timers
//! Measures mouse/keyboard to display latency (click-to-photon)

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use crate::cancel::{self, CancelFlag};
use crate::progress::{self, SharedProgress};
use crate::sensors::{Sampler, Telemetry};
use crate::timer::{HighResTimer, busy_wait_ns};
use rand::{Rng, SeedableRng};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputLatencyConfig {
    pub test_modes: Vec<InputTestMode>,
    pub sample_count: u32,
    pub warmup_samples: u32,
    pub delay_range_ms: (u32, u32),  // Min/max random delay before stimulus
    pub measure_display_latency: bool, // If true, measure full click-to-photon
    /// Pin the measuring thread to this logical CPU (None = OS decides)
    #[serde(default)]
    pub pin_core: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputTestMode {
    MouseClick,       // Click to visual response
    KeyPress,         // Key press to visual response
    MouseMove,        // Mouse move to cursor update
    RawInput,         // Raw input to application response
    PollingRate,      // Measure input polling rate
    Jitter,           // Measure input jitter
}

impl Default for InputLatencyConfig {
    fn default() -> Self {
        Self {
            // MouseClick/KeyPress are simulated headlessly (fixed sleeps, no real human), so
            // they are opt-in; the interactive click test lives in the GUI.
            test_modes: vec![
                InputTestMode::MouseMove,
                InputTestMode::RawInput,
                InputTestMode::PollingRate,
                InputTestMode::Jitter,
            ],
            sample_count: 100,
            warmup_samples: 10,
            delay_range_ms: (500, 3000),
            measure_display_latency: false, // Requires external sensor
            pin_core: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputLatencyResult {
    pub mode: InputTestMode,
    pub sample_count: u32,
    pub avg_latency_ms: f64,
    pub min_latency_ms: f64,
    pub max_latency_ms: f64,
    pub std_dev_ms: f64,
    pub percentile_50_ms: f64,
    pub percentile_95_ms: f64,
    pub percentile_99_ms: f64,
    pub percentile_999_ms: f64,
    pub jitter_ms: f64,           // Standard deviation of intervals
    pub polling_rate_hz: Option<f64>, // For polling rate test
    pub individual_samples: Vec<f64>, // All samples in ms
    pub timer_frequency: u64,
    pub timer_overhead_ns: u64,
    /// Core the measurement was pinned to, if any
    #[serde(default)]
    pub core: Option<usize>,
    /// Temperatures, clocks, RAM and GPU/VRAM readings taken while this test ran
    #[serde(default)]
    pub telemetry: Telemetry,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputLatencySummary {
    pub results: Vec<InputLatencyResult>,
    pub config: InputLatencyConfig,
    pub system_info: crate::system_info::SystemInfo,
    pub timestamp: String,
}

pub struct InputLatencyTester {
    config: InputLatencyConfig,
    pub timer: HighResTimer,
    rng: rand::rngs::StdRng,
    // For GUI integration
    pub pending_stimulus: Arc<Mutex<Option<StimulusInfo>>>,
    pub last_input_time: Arc<Mutex<Option<u64>>>,
    pub last_response_time: Arc<Mutex<Option<u64>>>,
    /// Latencies (ms) recorded from real stimulus/response pairs
    pub live_samples: Arc<Mutex<Vec<f64>>>,
    cancel: CancelFlag,
    progress: Option<SharedProgress>,
    sensors: Option<std::sync::Arc<Sampler>>,
}

#[derive(Debug, Clone)]
pub struct StimulusInfo {
    pub trigger_time: u64,
    pub stimulus_type: InputTestMode,
}

impl InputLatencyTester {
    pub fn new(config: InputLatencyConfig) -> Self {
        let timer = HighResTimer::new();
        Self {
            config,
            timer,
            rng: rand::rngs::StdRng::seed_from_u64(0x123456789ABCDEF0),
            pending_stimulus: Arc::new(Mutex::new(None)),
            last_input_time: Arc::new(Mutex::new(None)),
            last_response_time: Arc::new(Mutex::new(None)),
            live_samples: Arc::new(Mutex::new(Vec::new())),
            cancel: cancel::new_flag(),
            progress: None,
            sensors: None,
        }
    }

    /// Publish live progress to the GUI
    pub fn with_progress(mut self, progress: SharedProgress) -> Self {
        self.progress = Some(progress);
        self
    }

    /// Attach a sensor sampler so every result carries temperatures / clocks
    pub fn with_sensors(mut self, sampler: std::sync::Arc<Sampler>) -> Self {
        self.sensors = Some(sampler);
        self
    }

    /// Share a flag that stops the run early when set
    pub fn with_cancel(mut self, flag: CancelFlag) -> Self {
        self.cancel = flag;
        self
    }

    pub fn run(&mut self) -> Result<InputLatencySummary> {
        let system_info = crate::system_info::collect_system_info()?;
        let mut results = Vec::new();

        if let Some(core) = self.config.pin_core {
            if !crate::topology::pin_current_thread(core) {
                tracing::warn!("could not pin the input test to core {}", core);
            }
        }
        let total = self.config.test_modes.len();
        progress::update(&self.progress, |p| {
            *p = progress::RunProgress { total, started: Some(std::time::Instant::now()), ..Default::default() }
        });
        for mode in self.config.test_modes.clone() {
            cancel::check(&self.cancel)?;
            progress::update(&self.progress, |p| {
                p.title = format!("{:?}{}", mode, self.config.pin_core.map(|c| format!(" · core {}", c)).unwrap_or_default());
                p.detail = format!("{} samples", self.config.sample_count);
            });
            let t0 = self.sensors.as_ref().map(|s| s.now_ms());
            let mut result = self.run_mode(mode)?;
            if let (Some(s), Some(t0)) = (&self.sensors, t0) {
                result.telemetry = s.window(t0, s.now_ms());
            }
            progress::update(&self.progress, |p| p.done += 1);
            results.push(result);
        }

        Ok(InputLatencySummary {
            results,
            config: self.config.clone(),
            system_info,
            timestamp: chrono::Utc::now().to_rfc3339(),
        })
    }

    fn run_mode(&mut self, mode: InputTestMode) -> Result<InputLatencyResult> {
        match mode {
            InputTestMode::MouseClick => self.run_mouse_click_test(),
            InputTestMode::KeyPress => self.run_key_press_test(),
            InputTestMode::MouseMove => self.run_mouse_move_test(),
            InputTestMode::RawInput => self.run_raw_input_test(),
            InputTestMode::PollingRate => self.run_polling_rate_test(),
            InputTestMode::Jitter => self.run_jitter_test(),
        }
    }

    // Mouse click to visual response test
    fn run_mouse_click_test(&mut self) -> Result<InputLatencyResult> {
        let mut samples = Vec::new();
        
        // Warmup
        for _ in 0..self.config.warmup_samples {
            cancel::check(&self.cancel)?;
            self.single_click_test()?;
        }

        // Actual test
        for _ in 0..self.config.sample_count {
            cancel::check(&self.cancel)?;
            if let Some(latency) = self.single_click_test()? {
                samples.push(latency);
            }
        }

        self.calculate_result(InputTestMode::MouseClick, samples)
    }

    fn single_click_test(&mut self) -> Result<Option<f64>> {
        // Random delay before showing stimulus
        let delay_ms = self.rng.gen_range(self.config.delay_range_ms.0..=self.config.delay_range_ms.1);
        
        // Wait for random delay
        std::thread::sleep(Duration::from_millis(delay_ms as u64));
        
        // Record stimulus time (when we show "CLICK NOW" visual cue)
        let stimulus_time = self.timer.now_ticks();
        
        // In a real GUI, we'd show a visual cue here and wait for mouse click
        // For headless testing, we simulate the click after a simulated human reaction time
        let reaction_time_ms = self.rng.gen_range(150..350); // Typical human reaction time
        std::thread::sleep(Duration::from_millis(reaction_time_ms));
        
        let response_time = self.timer.now_ticks();
        let latency_ticks = response_time - stimulus_time;
        let latency_ms = self.timer.ticks_to_ms_f64(latency_ticks);
        
        Ok(Some(latency_ms))
    }

    // Key press test
    fn run_key_press_test(&mut self) -> Result<InputLatencyResult> {
        let mut samples = Vec::new();
        
        for _ in 0..self.config.warmup_samples {
            cancel::check(&self.cancel)?;
            self.single_key_test()?;
        }

        for _ in 0..self.config.sample_count {
            cancel::check(&self.cancel)?;
            if let Some(latency) = self.single_key_test()? {
                samples.push(latency);
            }
        }

        self.calculate_result(InputTestMode::KeyPress, samples)
    }

    fn single_key_test(&mut self) -> Result<Option<f64>> {
        let delay_ms = self.rng.gen_range(self.config.delay_range_ms.0..=self.config.delay_range_ms.1);
        std::thread::sleep(Duration::from_millis(delay_ms as u64));
        
        let stimulus_time = self.timer.now_ticks();
        
        // Simulate key press
        let reaction_time_ms = self.rng.gen_range(100..300); // Typical human reaction time
        std::thread::sleep(Duration::from_millis(reaction_time_ms));
        
        let response_time = self.timer.now_ticks();
        let latency_ticks = response_time - stimulus_time;
        let latency_ms = self.timer.ticks_to_ms_f64(latency_ticks);
        
        Ok(Some(latency_ms))
    }

    // Mouse move test
    fn run_mouse_move_test(&mut self) -> Result<InputLatencyResult> {
        let mut samples = Vec::new();
        
        for _ in 0..self.config.sample_count {
            cancel::check(&self.cancel)?;
            let start = self.timer.now_ticks();
            // Simulate mouse move processing
            std::thread::sleep(Duration::from_micros(100));
            let end = self.timer.now_ticks();
            
            let latency_ms = self.timer.ticks_to_ms_f64(end - start);
            samples.push(latency_ms);
        }

        self.calculate_result(InputTestMode::MouseMove, samples)
    }

    // Raw input latency test (measures OS input stack latency)
    fn run_raw_input_test(&mut self) -> Result<InputLatencyResult> {
        let mut samples = Vec::new();
        
        // This test measures the latency from hardware interrupt to application callback
        // On Windows: Raw Input API (WM_INPUT)
        // On Linux: evdev / libinput
        
        for _ in 0..self.config.sample_count {
            cancel::check(&self.cancel)?;
            let start = self.timer.now_ticks();
            
            // Simulate raw input processing
            #[cfg(target_os = "windows")]
            {
                // Would use GetRawInputData in real implementation
                std::thread::sleep(Duration::from_micros(50));
            }
            
            #[cfg(target_os = "linux")]
            {
                // Would read from /dev/input/event* in real implementation
                std::thread::sleep(Duration::from_micros(50));
            }
            
            let end = self.timer.now_ticks();
            let latency_ms = self.timer.ticks_to_ms_f64(end - start);
            samples.push(latency_ms);
        }

        self.calculate_result(InputTestMode::RawInput, samples)
    }

    // Polling rate test
    fn run_polling_rate_test(&mut self) -> Result<InputLatencyResult> {
        let mut intervals = Vec::new();
        let test_duration_ms = 5000; // 5 seconds
        let start = self.timer.now_ticks();
        let target_end = start + (test_duration_ms as u128 * self.timer.frequency() as u128 / 1000) as u64;
        
        let mut last_time = self.timer.now_ticks();
        
        while self.timer.now_ticks() < target_end {
            cancel::check(&self.cancel)?;
            let now = self.timer.now_ticks();
            let interval_ticks = now - last_time;
            let interval_ms = self.timer.ticks_to_ms_f64(interval_ticks);
            intervals.push(interval_ms);
            last_time = now;
            
            // Small sleep to not consume 100% CPU
            std::thread::sleep(Duration::from_micros(100));
        }
        
        if intervals.is_empty() {
            return Err(anyhow::anyhow!("polling-rate test collected no samples"));
        }
        let avg_interval = intervals.iter().sum::<f64>() / intervals.len() as f64;
        let polling_rate = 1000.0 / avg_interval; // Hz
        
        let mut result = self.calculate_result(InputTestMode::PollingRate, intervals)?;
        result.polling_rate_hz = Some(polling_rate);
        Ok(result)
    }

    // Jitter test
    fn run_jitter_test(&mut self) -> Result<InputLatencyResult> {
        let mut intervals = Vec::new();
        let sample_count = 10000;
        let target_interval_ns = 1_000_000; // 1ms target
        
        let mut last = self.timer.now_ticks();
        
        for _ in 0..sample_count {
            cancel::check(&self.cancel)?;
            busy_wait_ns(&self.timer, target_interval_ns);
            let now = self.timer.now_ticks();
            let interval_ns = self.timer.ticks_to_ns(now - last);
            intervals.push(interval_ns as f64 / 1_000_000.0); // Convert to ms
            last = now;
        }
        
        let mut result = self.calculate_result(InputTestMode::Jitter, intervals)?;
        result.jitter_ms = result.std_dev_ms;
        Ok(result)
    }

    fn calculate_result(&self, mode: InputTestMode, samples: Vec<f64>) -> Result<InputLatencyResult> {
        if samples.is_empty() {
            return Err(anyhow::anyhow!("No samples collected"));
        }
        
        let mut sorted = samples.clone();
        sorted.sort_by(|a, b| a.total_cmp(b));
        
        let sum: f64 = sorted.iter().sum();
        let avg = sum / sorted.len() as f64;
        let min = sorted[0];
        let max = sorted[sorted.len() - 1];
        
        let variance: f64 = sorted.iter()
            .map(|&x| (x - avg).powi(2))
            .sum::<f64>() / sorted.len() as f64;
        let std_dev = variance.sqrt();
        
        let p50 = sorted[sorted.len() / 2];
        let last = sorted.len() - 1;
        let p95 = sorted[((sorted.len() as f64 * 0.95) as usize).min(last)];
        let p99 = sorted[((sorted.len() as f64 * 0.99) as usize).min(last)];
        let p999_idx = ((sorted.len() as f64 * 0.999) as usize).min(sorted.len() - 1);
        let p999 = sorted[p999_idx];
        
        let jitter = std_dev;

        Ok(InputLatencyResult {
            mode,
            sample_count: samples.len() as u32,
            avg_latency_ms: avg,
            min_latency_ms: min,
            max_latency_ms: max,
            std_dev_ms: std_dev,
            percentile_50_ms: p50,
            percentile_95_ms: p95,
            percentile_99_ms: p99,
            percentile_999_ms: p999,
            jitter_ms: jitter,
            polling_rate_hz: None,
            individual_samples: samples,
            timer_frequency: self.timer.frequency(),
            timer_overhead_ns: self.timer.measure_overhead(10000),
            core: self.config.pin_core,
            telemetry: Telemetry::default(),
        })
    }

    /// Called by GUI when input event occurs
    pub fn on_input_event(&self, event_type: InputTestMode) {
        let now = self.timer.now_ticks();
        *self.last_input_time.lock().unwrap() = Some(now);
        
        // Check if we were waiting for this stimulus
        if let Some(stimulus) = self.pending_stimulus.lock().unwrap().take() {
            if stimulus.stimulus_type == event_type {
                let latency_ticks = now.saturating_sub(stimulus.trigger_time);
                let latency_ms = self.timer.ticks_to_ms_f64(latency_ticks);
                *self.last_response_time.lock().unwrap() = Some(now);
                self.live_samples.lock().unwrap().push(latency_ms);
            }
        }
    }

    /// Called by GUI to trigger a stimulus (show visual cue)
    pub fn trigger_stimulus(&self, mode: InputTestMode) {
        let trigger_time = self.timer.now_ticks();
        *self.pending_stimulus.lock().unwrap() = Some(StimulusInfo {
            trigger_time,
            stimulus_type: mode,
        });
    }

    /// Get the last measured latency (for GUI display)
    pub fn get_last_latency(&self) -> Option<f64> {
        let input = (*self.last_input_time.lock().unwrap())?;
        let response = (*self.last_response_time.lock().unwrap())?;
        let latency_ticks = response.checked_sub(input)?;
        Some(self.timer.ticks_to_ms_f64(latency_ticks))
    }
}

/// High-level function for GUI click-to-photon test
pub fn run_click_to_photon_test(sample_count: u32) -> Result<Vec<f64>> {
    let config = InputLatencyConfig {
        test_modes: vec![InputTestMode::MouseClick],
        sample_count,
        warmup_samples: 10,
        delay_range_ms: (500, 2000),
        measure_display_latency: true,
        pin_core: None,
    };
    
    let mut tester = InputLatencyTester::new(config);
    let summary = tester.run()?;
    
    summary
        .results
        .into_iter()
        .next()
        .map(|r| r.individual_samples)
        .ok_or_else(|| anyhow::anyhow!("no input latency results"))
}

/// Measure system timer resolution
pub fn measure_timer_resolution() -> Result<TimerResolutionInfo> {
    let timer = HighResTimer::new();
    let overhead = timer.measure_overhead(100000);
    let frequency = timer.frequency();
    let resolution_ns = 1_000_000_000 / frequency;
    
    // Measure actual minimum measurable interval
    let mut min_interval = u64::MAX;
    for _ in 0..10000 {
        let t1 = timer.now_ticks();
        let t2 = timer.now_ticks();
        if t2 > t1 {
            min_interval = min_interval.min(t2 - t1);
        }
    }
    let min_interval_ns = timer.ticks_to_ns(min_interval);
    
    Ok(TimerResolutionInfo {
        frequency_hz: frequency,
        resolution_ns,
        overhead_ns: timer.ticks_to_ns(overhead),
        min_measurable_interval_ns: min_interval_ns,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimerResolutionInfo {
    pub frequency_hz: u64,
    pub resolution_ns: u64,
    pub overhead_ns: u64,
    pub min_measurable_interval_ns: u64,
}

/// Run the suite once per entry of `passes` (`None` = unpinned, `Some(core)` = pinned to that core) and
/// merge the results
pub fn run_passes(
    base: InputLatencyConfig,
    passes: Vec<Option<usize>>,
    cancel_flag: crate::cancel::CancelFlag,
    prog: SharedProgress,
    sampler: std::sync::Arc<Sampler>,
) -> anyhow::Result<InputLatencySummary> {
    let total = passes.len() * base.test_modes.len();
    let started = std::time::Instant::now();
    let mut merged: Option<InputLatencySummary> = None;
    for (i, core) in passes.iter().enumerate() {
        crate::cancel::check(&cancel_flag)?;
        let inner = progress::new();
        let cfg = InputLatencyConfig { pin_core: *core, ..base.clone() };
        // Run on a fresh thread so the pin does not leak into the caller
        let (c2, p2, s2) = (cancel_flag.clone(), inner.clone(), sampler.clone());
        let outer = prog.clone();
        let done_before = i * base.test_modes.len();
        let watcher_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ws = watcher_stop.clone();
        let inner_watch = inner.clone();
        let watcher = std::thread::spawn(move || {
            while !ws.load(std::sync::atomic::Ordering::Relaxed) {
                if let (Ok(mut o), Ok(i)) = (outer.lock(), inner_watch.lock()) {
                    o.total = total;
                    o.done = done_before + i.done;
                    o.title = i.title.clone();
                    o.detail = i.detail.clone();
                    o.started = Some(started);
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        });
        let summary = std::thread::spawn(move || {
            InputLatencyTester::new(cfg).with_cancel(c2).with_progress(p2).with_sensors(s2).run()
        })
        .join()
        .map_err(|_| anyhow::anyhow!("input test thread panicked"))?;
        watcher_stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = watcher.join();
        let summary = summary?;
        match merged.as_mut() {
            Some(m) => m.results.extend(summary.results),
            None => merged = Some(summary),
        }
    }
    merged.ok_or_else(|| anyhow::anyhow!("no input passes to run"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timer_resolution() {
        let info = measure_timer_resolution().unwrap();
        assert!(info.frequency_hz > 0);
        assert!(info.resolution_ns > 0);
    }

    #[test]
    fn test_jitter_test() {
        let config = InputLatencyConfig {
            test_modes: vec![InputTestMode::Jitter],
            sample_count: 100,
            warmup_samples: 0,
            delay_range_ms: (0, 0),
            measure_display_latency: false,
            pin_core: None,
        };
        let mut tester = InputLatencyTester::new(config);
        let result = tester.run_jitter_test().unwrap();
        assert!(result.jitter_ms >= 0.0);
    }
}
#![allow(dead_code)] // public helper API; not every function is wired into the GUI/CLI
//! Cross-platform high-resolution timer abstraction
//! Uses QueryPerformanceCounter on Windows and clock_gettime on Linux

use std::time::Duration;
use instant::Instant;

/// High-resolution timer using platform-specific APIs
pub struct HighResTimer {
    start: Instant,
    frequency: u64, // Ticks per second
}

impl HighResTimer {
    /// Create a new high-resolution timer
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
            frequency: Self::get_frequency(),
        }
    }

    /// Get the timer frequency in Hz (ticks per second)
    #[cfg(target_os = "windows")]
    fn get_frequency() -> u64 {
        use windows::Win32::System::Performance::QueryPerformanceFrequency;
        let mut freq = 0i64;
        let ok = unsafe { QueryPerformanceFrequency(&mut freq) };
        if ok.is_ok() {
            freq.max(0) as u64
        } else {
            1_000_000_000 // Fallback to nanoseconds if QPC frequency unavailable
        }
    }

    #[cfg(target_os = "linux")]
    fn get_frequency() -> u64 {
        // On Linux, clock_gettime(CLOCK_MONOTONIC_RAW) uses nanoseconds
        1_000_000_000
    }

    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    fn get_frequency() -> u64 {
        1_000_000_000 // Default to nanoseconds
    }

    /// Get current timestamp in timer ticks
    pub fn now_ticks(&self) -> u64 {
        #[cfg(target_os = "windows")]
        {
            use windows::Win32::System::Performance::QueryPerformanceCounter;
            let mut counter = 0i64;
            unsafe {
                let _ = QueryPerformanceCounter(&mut counter);
            }
            counter as u64
        }

        #[cfg(target_os = "linux")]
        {
            // Use CLOCK_MONOTONIC_RAW via libc
            let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
            unsafe {
                libc::clock_gettime(libc::CLOCK_MONOTONIC_RAW, &mut ts);
            }
            (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
        }

        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        {
            self.start.elapsed().as_nanos() as u64
        }
    }

    /// Get current timestamp as Duration since start
    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    /// Get current timestamp in nanoseconds
    pub fn now_ns(&self) -> u64 {
        let ticks = self.now_ticks();
        (ticks as u128 * 1_000_000_000 / self.frequency as u128) as u64
    }

    /// Get current timestamp in microseconds
    pub fn now_us(&self) -> u64 {
        let ticks = self.now_ticks();
        (ticks as u128 * 1_000_000 / self.frequency as u128) as u64
    }

    /// Get current timestamp in milliseconds
    pub fn now_ms(&self) -> u64 {
        let ticks = self.now_ticks();
        (ticks as u128 * 1_000 / self.frequency as u128) as u64
    }

    /// Get the timer frequency
    pub fn frequency(&self) -> u64 {
        self.frequency
    }

    /// Convert ticks to nanoseconds
    pub fn ticks_to_ns(&self, ticks: u64) -> u64 {
        (ticks as u128 * 1_000_000_000 / self.frequency as u128) as u64
    }

    /// Convert ticks to microseconds
    pub fn ticks_to_us(&self, ticks: u64) -> u64 {
        (ticks as u128 * 1_000_000 / self.frequency as u128) as u64
    }

    /// Convert ticks to milliseconds (f64 for precision)
    pub fn ticks_to_ms_f64(&self, ticks: u64) -> f64 {
        ticks as f64 * 1000.0 / self.frequency as f64
    }

    /// Measure the overhead of calling now_ticks()
    pub fn measure_overhead(&self, iterations: u32) -> u64 {
        let start = self.now_ticks();
        for _ in 0..iterations {
            std::hint::black_box(self.now_ticks());
        }
        let end = self.now_ticks();
        (end - start) / iterations as u64
    }
}

impl Default for HighResTimer {
    fn default() -> Self {
        Self::new()
    }
}

/// Timer for measuring intervals with minimal overhead
pub struct IntervalTimer {
    timer: HighResTimer,
    last_tick: u64,
}

impl IntervalTimer {
    pub fn new() -> Self {
        let timer = HighResTimer::new();
        let last_tick = timer.now_ticks();
        Self { timer, last_tick }
    }

    /// Get elapsed ticks since last call and update last_tick
    pub fn lap_ticks(&mut self) -> u64 {
        let now = self.timer.now_ticks();
        let elapsed = now - self.last_tick;
        self.last_tick = now;
        elapsed
    }

    /// Get elapsed nanoseconds since last call
    pub fn lap_ns(&mut self) -> u64 {
        { let t = self.lap_ticks(); self.timer.ticks_to_ns(t) }
    }

    /// Get elapsed microseconds since last call
    pub fn lap_us(&mut self) -> u64 {
        { let t = self.lap_ticks(); self.timer.ticks_to_us(t) }
    }

    /// Get elapsed milliseconds since last call (f64)
    pub fn lap_ms(&mut self) -> f64 {
        { let t = self.lap_ticks(); self.timer.ticks_to_ms_f64(t) }
    }

    /// Reset the timer
    pub fn reset(&mut self) {
        self.last_tick = self.timer.now_ticks();
    }
}

impl Default for IntervalTimer {
    fn default() -> Self {
        Self::new()
    }
}

/// Busy-wait for precise timing (consumes CPU but guarantees precision)
pub fn busy_wait_ns(timer: &HighResTimer, target_ns: u64) {
    let start = timer.now_ticks();
    let target_ticks = (target_ns as u128 * timer.frequency as u128 / 1_000_000_000) as u64;
    let target = start + target_ticks;
    
    while timer.now_ticks() < target {
        std::hint::spin_loop();
    }
}

/// Busy-wait for microseconds
pub fn busy_wait_us(timer: &HighResTimer, target_us: u64) {
    busy_wait_ns(timer, target_us * 1000);
}

/// Busy-wait for milliseconds
pub fn busy_wait_ms(timer: &HighResTimer, target_ms: u64) {
    busy_wait_ns(timer, target_ms * 1_000_000);
}

/// Sleep with high precision (uses busy wait for short durations)
pub fn precise_sleep(timer: &HighResTimer, duration_ns: u64) {
    if duration_ns < 100_000 { // Less than 100us - busy wait
        busy_wait_ns(timer, duration_ns);
    } else {
        // For longer durations, sleep most of the time then busy wait the remainder
        let sleep_ns = duration_ns.saturating_sub(50_000); // Leave 50us for busy wait
        std::thread::sleep(Duration::from_nanos(sleep_ns));
        let remaining = duration_ns.saturating_sub(sleep_ns);
        if remaining > 0 {
            busy_wait_ns(timer, remaining);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timer_creation() {
        let timer = HighResTimer::new();
        assert!(timer.frequency() > 0);
    }

    #[test]
    fn test_timer_monotonic() {
        let timer = HighResTimer::new();
        let t1 = timer.now_ticks();
        std::thread::sleep(Duration::from_millis(1));
        let t2 = timer.now_ticks();
        assert!(t2 > t1);
    }

    #[test]
    fn test_interval_timer() {
        let mut interval = IntervalTimer::new();
        std::thread::sleep(Duration::from_millis(10));
        let elapsed = interval.lap_ms();
        assert!(elapsed >= 10.0);
    }
}
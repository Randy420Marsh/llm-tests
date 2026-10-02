//! System-wide CPU time counters, read synchronously at the edges of a measured section.
//!
//! The background [`crate::sensors::Sampler`] takes a snapshot every few hundred ms, and each
//! snapshot's CPU load describes the interval *before* it was taken. Matching those snapshots to a
//! test that lasts a few ms (or that starts mid-interval) puts the previous test's load on this
//! test's row. Reading the OS counters right before and right after the timed loop gives the load of
//! exactly that loop instead.
//!
//! Resolution: the OS charges CPU time in scheduler ticks (Windows ≈ 15.6 ms, Linux 10 ms per CPU),
//! so a window much shorter than [`MIN_WINDOW_MS`] cannot be measured and is reported as `None`.

/// Shortest measured section whose load is reported; below this the tick granularity dominates
pub const MIN_WINDOW_MS: f64 = 200.0;

/// Cumulative busy and total CPU time of all logical CPUs, in arbitrary but consistent units
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuTimes {
    pub busy: u64,
    pub total: u64,
}

impl CpuTimes {
    /// Current counters, or None where the platform offers none
    pub fn now() -> Option<CpuTimes> {
        imp::now()
    }

    /// Average load of the whole CPU between `self` and `later`, in percent (0–100)
    pub fn load_pct_until(&self, later: &CpuTimes) -> Option<f32> {
        let total = later.total.checked_sub(self.total)?;
        let busy = later.busy.checked_sub(self.busy)?;
        (total > 0).then(|| (busy as f64 / total as f64 * 100.0).clamp(0.0, 100.0) as f32)
    }
}

/// Load over a measured section, or None when it was too short to measure
pub fn load_over(start: Option<CpuTimes>, end: Option<CpuTimes>, window_ms: f64) -> Option<f32> {
    if window_ms < MIN_WINDOW_MS {
        return None;
    }
    start?.load_pct_until(&end?)
}

/// First line of /proc/stat: "cpu  user nice system idle iowait irq softirq steal guest guest_nice"
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_proc_stat(text: &str) -> Option<CpuTimes> {
    let line = text.lines().find(|l| l.starts_with("cpu "))?;
    let v: Vec<u64> = line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
    if v.len() < 4 {
        return None;
    }
    let get = |i: usize| v.get(i).copied().unwrap_or(0);
    // guest time is already included in user / nice
    let idle = get(3) + get(4);
    let busy = get(0) + get(1) + get(2) + get(5) + get(6) + get(7);
    Some(CpuTimes { busy, total: busy + idle })
}

#[cfg(target_os = "linux")]
mod imp {
    pub fn now() -> Option<super::CpuTimes> {
        super::parse_proc_stat(&std::fs::read_to_string("/proc/stat").ok()?)
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::GetSystemTimes;

    fn ft(f: FILETIME) -> u64 {
        ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64
    }

    pub fn now() -> Option<super::CpuTimes> {
        let (mut idle, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
        // SAFETY: three valid out-pointers to FILETIME
        unsafe { GetSystemTimes(Some(&mut idle as *mut _), Some(&mut kernel as *mut _), Some(&mut user as *mut _)) }.ok()?;
        // kernel time includes idle time
        let total = ft(kernel) + ft(user);
        Some(super::CpuTimes { busy: total.saturating_sub(ft(idle)), total })
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
mod imp {
    pub fn now() -> Option<super::CpuTimes> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_stat_line_is_parsed() {
        let t = parse_proc_stat("cpu  100 5 50 800 20 3 2 0 0 0\ncpu0 1 2 3 4\n").unwrap();
        assert_eq!(t, CpuTimes { busy: 160, total: 980 });
    }

    #[test]
    fn load_between_two_readings() {
        let a = CpuTimes { busy: 100, total: 1000 };
        let b = CpuTimes { busy: 400, total: 2000 };
        assert_eq!(a.load_pct_until(&b), Some(30.0));
        // counters going backwards (wrap, bad read) are not a load
        assert_eq!(b.load_pct_until(&a), None);
    }

    #[test]
    fn short_windows_are_not_reported() {
        let (a, b) = (Some(CpuTimes { busy: 0, total: 10 }), Some(CpuTimes { busy: 5, total: 20 }));
        assert_eq!(load_over(a, b, MIN_WINDOW_MS - 1.0), None);
        assert_eq!(load_over(a, b, MIN_WINDOW_MS), Some(50.0));
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    #[test]
    fn busy_threads_show_up_in_the_load() {
        let n = num_cpus::get();
        let start = CpuTimes::now().unwrap();
        let t0 = std::time::Instant::now();
        std::thread::scope(|s| {
            for _ in 0..n {
                s.spawn(|| {
                    let mut x = 0u64;
                    while t0.elapsed().as_millis() < 400 {
                        x = std::hint::black_box(x.wrapping_add(1));
                    }
                });
            }
        });
        let load = load_over(Some(start), CpuTimes::now(), t0.elapsed().as_secs_f64() * 1000.0).unwrap();
        assert!(load > 60.0, "every CPU spun for 400 ms but the load was {} %", load);
    }
}

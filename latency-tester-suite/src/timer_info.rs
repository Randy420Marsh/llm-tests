//! Which clocks the OS uses, and asking it for the finest timer resolution.
//!
//! * Windows: the QPC frequency tells the time source behind QueryPerformanceCounter (10 MHz = the
//!   invariant TSC, 14.318 MHz = HPET forced with `bcdedit /set useplatformclock true`, 3.58 MHz =
//!   ACPI PM timer). The system timer resolution (sleeps, waits, the GUI's event loop) is read and
//!   raised with NtQueryTimerResolution / NtSetTimerResolution and timeBeginPeriod(1).
//! * Linux: /sys/devices/system/clocksource (tsc, hpet, acpi_pm).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TimerSources {
    /// QueryPerformanceFrequency (Windows) or the clock the app's timer reports
    pub qpc_hz: u64,
    /// "TSC", "HPET", "ACPI PM timer", ...
    pub qpc_source: String,
    /// Windows timer resolution in ms: coarsest allowed, finest allowed, current
    pub timer_res_coarsest_ms: Option<f64>,
    pub timer_res_finest_ms: Option<f64>,
    pub timer_res_current_ms: Option<f64>,
    /// Linux: current and available clocksources
    pub clocksource: Option<String>,
    pub clocksources_available: Option<String>,
    pub notes: Vec<String>,
}

/// Time source behind a QPC frequency
pub fn classify_qpc(hz: u64) -> &'static str {
    match hz {
        10_000_000 => "TSC (invariant, via QPC at 10 MHz)",
        14_318_180 => "HPET (useplatformclock is on)",
        3_579_545 => "ACPI PM timer",
        1_000_000_000 => "nanosecond clock (TSC / clock_gettime)",
        h if h > 1_000_000_000 => "TSC (raw)",
        _ => "other",
    }
}

#[cfg(target_os = "windows")]
#[link(name = "ntdll")]
extern "system" {
    fn NtQueryTimerResolution(coarsest: *mut u32, finest: *mut u32, current: *mut u32) -> i32;
    fn NtSetTimerResolution(desired: u32, set: u8, current: *mut u32) -> i32;
}

#[cfg(target_os = "windows")]
#[link(name = "winmm")]
extern "system" {
    fn timeBeginPeriod(ms: u32) -> u32;
    fn timeEndPeriod(ms: u32) -> u32;
}

pub fn query() -> TimerSources {
    let mut t = TimerSources::default();
    #[cfg(target_os = "windows")]
    unsafe {
        let mut hz = 0i64;
        if windows::Win32::System::Performance::QueryPerformanceFrequency(&mut hz).is_ok() {
            t.qpc_hz = hz as u64;
        }
        let (mut c, mut f, mut cur) = (0u32, 0u32, 0u32);
        if NtQueryTimerResolution(&mut c, &mut f, &mut cur) == 0 {
            // units of 100 ns; "coarsest" is typically 15.625 ms, "finest" 0.5 ms
            t.timer_res_coarsest_ms = Some(c as f64 / 10_000.0);
            t.timer_res_finest_ms = Some(f as f64 / 10_000.0);
            t.timer_res_current_ms = Some(cur as f64 / 10_000.0);
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        t.qpc_hz = crate::timer::HighResTimer::new().frequency();
    }
    #[cfg(target_os = "linux")]
    {
        let rd = |f: &str| std::fs::read_to_string(format!("/sys/devices/system/clocksource/clocksource0/{}", f)).ok().map(|s| s.trim().to_string());
        t.clocksource = rd("current_clocksource");
        t.clocksources_available = rd("available_clocksource");
    }
    t.qpc_source = match &t.clocksource {
        Some(cs) => format!("clocksource {}", cs),
        None => classify_qpc(t.qpc_hz).to_string(),
    };
    t.notes = advice(&t);
    t
}

fn advice(t: &TimerSources) -> Vec<String> {
    let mut n = Vec::new();
    if t.qpc_hz == 14_318_180 {
        n.push("QPC runs on HPET (bcdedit useplatformclock = true): every timestamp is a slow HPET read. For latency testing remove it: bcdedit /deletevalue useplatformclock (admin), then reboot.".into());
    }
    if t.qpc_hz == 3_579_545 {
        n.push("QPC runs on the ACPI PM timer: timestamps are slow and coarse. Check the BIOS / bcdedit timer settings.".into());
    }
    if let (Some(cur), Some(fin)) = (t.timer_res_current_ms, t.timer_res_finest_ms) {
        if cur > fin + 0.05 {
            n.push(format!("Timer resolution is {:.3} ms; the app asks for {:.3} ms while it runs (sleeps and the event loop wake up on this grid).", cur, fin));
        }
    }
    if matches!(t.clocksource.as_deref(), Some("hpet") | Some("acpi_pm")) {
        n.push("Linux uses a slow clocksource (hpet / acpi_pm): usually the TSC was marked unstable. Timestamps cost more and are coarser.".into());
    }
    n
}

/// Holds the finest timer resolution for as long as it lives (Windows; no-op elsewhere)
pub struct HighResolutionTimer {
    #[cfg(target_os = "windows")]
    set: bool,
}

impl HighResolutionTimer {
    pub fn acquire() -> Self {
        #[cfg(target_os = "windows")]
        unsafe {
            let (mut c, mut f, mut cur) = (0u32, 0u32, 0u32);
            let _ = timeBeginPeriod(1);
            let set = NtQueryTimerResolution(&mut c, &mut f, &mut cur) == 0 && NtSetTimerResolution(f, 1, &mut cur) == 0;
            Self { set }
        }
        #[cfg(not(target_os = "windows"))]
        Self {}
    }
}

impl Drop for HighResolutionTimer {
    fn drop(&mut self) {
        #[cfg(target_os = "windows")]
        unsafe {
            let mut cur = 0u32;
            if self.set {
                let _ = NtSetTimerResolution(0, 0, &mut cur);
            }
            let _ = timeEndPeriod(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qpc_frequencies_map_to_their_source() {
        assert!(classify_qpc(10_000_000).starts_with("TSC"));
        assert!(classify_qpc(14_318_180).starts_with("HPET"));
        assert!(classify_qpc(3_579_545).starts_with("ACPI"));
    }

    #[test]
    fn hpet_as_qpc_is_flagged() {
        let t = TimerSources { qpc_hz: 14_318_180, ..Default::default() };
        assert!(advice(&t).iter().any(|n| n.contains("useplatformclock")));
        let ok = TimerSources { qpc_hz: 10_000_000, timer_res_current_ms: Some(0.5), timer_res_finest_ms: Some(0.5), ..Default::default() };
        assert!(advice(&ok).is_empty());
    }

    #[test]
    fn query_reports_a_clock() {
        let t = query();
        assert!(t.qpc_hz > 0 && !t.qpc_source.is_empty());
        let _hold = HighResolutionTimer::acquire();
    }
}

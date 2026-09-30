//! Mouse polling test: the user moves the mouse and every report the mouse sends is timestamped, the
//! way MouseTester does it. From the reports come the real polling rate, how evenly the reports arrive
//! (interval jitter), and the counts per report.
//!
//! * Windows: a dedicated thread with a message-only window registered for raw input (WM_INPUT, also
//!   when the app is not focused); each report is stamped with QueryPerformanceCounter on arrival.
//! * Linux: evdev (/dev/input/event*), stamped by the kernel when the report came in. Reading the
//!   devices needs the `input` group (or root).
//!
//! The older "MouseMove / RawInput / PollingRate / Jitter" modes of the input timing suite do not read
//! the mouse: they time the app's own wake-ups (see `input_latency`).

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// One report from the mouse
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Report {
    /// Arrival time, ns since the capture started
    pub t_ns: u64,
    pub dx: i32,
    pub dy: i32,
}

/// A running capture; `stop()` (or drop) ends it
pub struct Capture {
    stop: Arc<AtomicBool>,
    reports: Arc<Mutex<Vec<Report>>>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Where the reports come from ("raw input (WM_INPUT) + QPC", "evdev kernel timestamps")
    pub source: String,
    error: Arc<Mutex<Option<String>>>,
}

impl Capture {
    /// Reports so far
    pub fn reports(&self) -> Vec<Report> {
        self.reports.lock().map(|r| r.clone()).unwrap_or_default()
    }

    pub fn count(&self) -> usize {
        self.reports.lock().map(|r| r.len()).unwrap_or(0)
    }

    /// Error from the capture thread, if it failed after starting
    pub fn error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|e| e.clone())
    }

    /// Stop and return every report
    pub fn stop(mut self) -> Vec<Report> {
        self.finish();
        self.reports()
    }

    fn finish(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Start capturing mouse reports
pub fn start() -> Result<Capture, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let reports = Arc::new(Mutex::new(Vec::with_capacity(1 << 16)));
    let error = Arc::new(Mutex::new(None));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<String, String>>();
    let thread = {
        let (stop, reports, error) = (stop.clone(), reports.clone(), error.clone());
        std::thread::Builder::new()
            .name("mouse-poll".into())
            .spawn(move || {
                raise_priority();
                if let Err(e) = platform::run(&stop, &reports, &ready_tx) {
                    let _ = ready_tx.send(Err(e.clone()));
                    if let Ok(mut slot) = error.lock() {
                        *slot = Some(e);
                    }
                }
            })
            .map_err(|e| e.to_string())?
    };
    match ready_rx.recv_timeout(std::time::Duration::from_secs(3)) {
        Ok(Ok(source)) => Ok(Capture { stop, reports, thread: Some(thread), source, error }),
        Ok(Err(e)) => {
            stop.store(true, Ordering::Relaxed);
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            stop.store(true, Ordering::Relaxed);
            Err("the mouse capture did not start".into())
        }
    }
}

/// The capture thread runs above normal priority so it is woken promptly for every report
fn raise_priority() {
    #[cfg(target_os = "windows")]
    unsafe {
        use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST};
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::Report;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::Sender;
    use std::sync::Mutex;
    use windows::core::w;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    use windows::Win32::UI::Input::{
        GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RIDEV_INPUTSINK, RIDEV_REMOVE,
        RID_INPUT, RIM_TYPEMOUSE,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, MsgWaitForMultipleObjects, PeekMessageW, RegisterClassW, HWND_MESSAGE,
        MSG, PM_REMOVE, QS_ALLINPUT, WINDOW_EX_STYLE, WINDOW_STYLE, WM_INPUT, WNDCLASSW,
    };

    unsafe extern "system" fn wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        DefWindowProcW(h, m, w, l)
    }

    fn qpc() -> i64 {
        let mut t = 0i64;
        unsafe {
            let _ = QueryPerformanceCounter(&mut t);
        }
        t
    }

    pub fn run(stop: &AtomicBool, out: &Mutex<Vec<Report>>, ready: &Sender<Result<String, String>>) -> Result<(), String> {
        unsafe {
            let mut freq = 0i64;
            QueryPerformanceFrequency(&mut freq).map_err(|e| e.to_string())?;
            let hinst = GetModuleHandleW(None).map_err(|e| e.to_string())?;
            let class = w!("LatencyTesterMousePoll");
            let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: hinst.into(), lpszClassName: class, ..Default::default() };
            // a second capture finds the class already registered, which is fine
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(WINDOW_EX_STYLE(0), class, w!(""), WINDOW_STYLE(0), 0, 0, 0, 0, HWND_MESSAGE, None, hinst, None)
                .map_err(|e| format!("could not create the raw-input window: {}", e))?;
            // generic desktop page, mouse; INPUTSINK = also while another window has the focus
            let dev = RAWINPUTDEVICE { usUsagePage: 0x01, usUsage: 0x02, dwFlags: RIDEV_INPUTSINK, hwndTarget: hwnd };
            if let Err(e) = RegisterRawInputDevices(&[dev], std::mem::size_of::<RAWINPUTDEVICE>() as u32) {
                let _ = DestroyWindow(hwnd);
                return Err(format!("could not register for raw mouse input: {}", e));
            }
            let _ = ready.send(Ok(format!("raw input (WM_INPUT), QueryPerformanceCounter at {} Hz", freq)));
            let t0 = qpc();
            let mut msg = MSG::default();
            let mut buf = vec![0u8; 256];
            while !stop.load(Ordering::Relaxed) {
                let _ = MsgWaitForMultipleObjects(None, false, 20, QS_ALLINPUT);
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    if msg.message == WM_INPUT {
                        let now = qpc();
                        let mut size = buf.len() as u32;
                        let got = GetRawInputData(
                            HRAWINPUT(msg.lParam.0 as *mut _),
                            RID_INPUT,
                            Some(buf.as_mut_ptr() as *mut _),
                            &mut size,
                            std::mem::size_of::<RAWINPUTHEADER>() as u32,
                        );
                        if got != u32::MAX && got as usize >= std::mem::size_of::<RAWINPUTHEADER>() {
                            let raw = &*(buf.as_ptr() as *const RAWINPUT);
                            // relative moves only (usFlags bit 0 = absolute: tablets, remote desktop)
                            if raw.header.dwType == RIM_TYPEMOUSE.0 && raw.data.mouse.usFlags.0 & 1 == 0 {
                                let (dx, dy) = (raw.data.mouse.lLastX, raw.data.mouse.lLastY);
                                if dx != 0 || dy != 0 {
                                    let t_ns = ((now - t0) as i128 * 1_000_000_000 / freq as i128) as u64;
                                    if let Ok(mut v) = out.lock() {
                                        v.push(Report { t_ns, dx, dy });
                                    }
                                }
                            }
                        }
                    }
                    DispatchMessageW(&msg);
                }
            }
            let remove = RAWINPUTDEVICE { usUsagePage: 0x01, usUsage: 0x02, dwFlags: RIDEV_REMOVE, hwndTarget: HWND::default() };
            let _ = RegisterRawInputDevices(&[remove], std::mem::size_of::<RAWINPUTDEVICE>() as u32);
            let _ = DestroyWindow(hwnd);
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::Report;
    use std::io::Read;
    use std::os::unix::io::AsRawFd;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::Sender;
    use std::sync::Mutex;

    const EV_SYN: u16 = 0;
    const EV_REL: u16 = 2;
    const REL_X: u16 = 0;
    const REL_Y: u16 = 1;
    /// EVIOCGBIT(EV_REL, len): which relative axes a device has
    fn eviocgbit_rel(len: usize) -> libc::c_ulong {
        (2 << 30 | (len as libc::c_ulong) << 16 | (b'E' as libc::c_ulong) << 8 | (0x20 + EV_REL as libc::c_ulong)) as libc::c_ulong
    }
    /// EVIOCSCLOCKID: stamp events with CLOCK_MONOTONIC
    const EVIOCSCLOCKID: libc::c_ulong = (1 << 30 | 4 << 16 | (b'E' as libc::c_ulong) << 8 | 0xa0) as libc::c_ulong;

    /// Devices that report relative X/Y movement (mice, touchpads in relative mode)
    fn open_mice() -> (Vec<std::fs::File>, usize) {
        let mut out = Vec::new();
        let mut denied = 0;
        let Ok(dir) = std::fs::read_dir("/dev/input") else { return (out, 0) };
        for e in dir.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.starts_with("event") {
                continue;
            }
            match std::fs::OpenOptions::new().read(true).custom_flags_nonblock().open(e.path()) {
                Ok(f) => {
                    let mut bits = [0u8; 2];
                    let ok = unsafe { libc::ioctl(f.as_raw_fd(), eviocgbit_rel(bits.len()), bits.as_mut_ptr()) } >= 0;
                    if ok && bits[0] & 0b11 == 0b11 {
                        let clock: libc::c_int = libc::CLOCK_MONOTONIC;
                        unsafe { libc::ioctl(f.as_raw_fd(), EVIOCSCLOCKID, &clock) };
                        out.push(f);
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => denied += 1,
                Err(_) => {}
            }
        }
        (out, denied)
    }

    trait NonBlock {
        fn custom_flags_nonblock(&mut self) -> &mut Self;
    }
    impl NonBlock for std::fs::OpenOptions {
        fn custom_flags_nonblock(&mut self) -> &mut Self {
            use std::os::unix::fs::OpenOptionsExt;
            self.custom_flags(libc::O_NONBLOCK)
        }
    }

    fn mono_ns() -> i128 {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        ts.tv_sec as i128 * 1_000_000_000 + ts.tv_nsec as i128
    }

    pub fn run(stop: &AtomicBool, out: &Mutex<Vec<Report>>, ready: &Sender<Result<String, String>>) -> Result<(), String> {
        let (mut mice, denied) = open_mice();
        if mice.is_empty() {
            return Err(if denied > 0 {
                "no permission to read /dev/input (add yourself to the \"input\" group or run as root)".into()
            } else {
                "no mouse found under /dev/input".into()
            });
        }
        let _ = ready.send(Ok(format!("evdev ({} device{}), kernel timestamps", mice.len(), if mice.len() == 1 { "" } else { "s" })));
        let t0 = mono_ns();
        // pending movement per device until its SYN_REPORT closes the report
        let mut pending: Vec<(i32, i32, i128)> = vec![(0, 0, 0); mice.len()];
        let size = std::mem::size_of::<libc::input_event>();
        let mut buf = vec![0u8; size * 64];
        while !stop.load(Ordering::Relaxed) {
            let mut fds: Vec<libc::pollfd> = mice.iter().map(|f| libc::pollfd { fd: f.as_raw_fd(), events: libc::POLLIN, revents: 0 }).collect();
            unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 20) };
            for (k, f) in mice.iter_mut().enumerate() {
                if fds[k].revents & libc::POLLIN == 0 {
                    continue;
                }
                while let Ok(n) = f.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    for chunk in buf[..n].chunks_exact(size) {
                        let ev: libc::input_event = unsafe { std::ptr::read_unaligned(chunk.as_ptr() as *const libc::input_event) };
                        let t = ev.time.tv_sec as i128 * 1_000_000_000 + ev.time.tv_usec as i128 * 1000;
                        match (ev.type_, ev.code) {
                            (EV_REL, REL_X) => pending[k].0 += ev.value,
                            (EV_REL, REL_Y) => pending[k].1 += ev.value,
                            (EV_SYN, 0) => {
                                let (dx, dy, _) = pending[k];
                                if dx != 0 || dy != 0 {
                                    if let Ok(mut v) = out.lock() {
                                        v.push(Report { t_ns: (t - t0).max(0) as u64, dx, dy });
                                    }
                                }
                                pending[k] = (0, 0, t);
                            }
                            _ => {}
                        }
                    }
                    if n < buf.len() {
                        break;
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
mod platform {
    use super::Report;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc::Sender;
    use std::sync::Mutex;
    pub fn run(_stop: &AtomicBool, _out: &Mutex<Vec<Report>>, _ready: &Sender<Result<String, String>>) -> Result<(), String> {
        Err("the mouse polling test needs Windows or Linux".into())
    }
}

// ---------------------------------------------------------------------------------------------
// analysis
// ---------------------------------------------------------------------------------------------

/// A pause longer than this ends a stretch of movement (its interval is not a polling interval)
pub const PAUSE_NS: u64 = 40_000_000;

/// Polling rates mice are set to
const STANDARD_RATES: [f64; 8] = [125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0];

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MousePollResult {
    pub source: String,
    pub reports: usize,
    /// Time with the mouse moving (pauses excluded), s
    pub moving_s: f64,
    /// Rate from the median interval, Hz
    pub rate_hz: f64,
    /// Nearest standard setting (125 … 8000 Hz)
    pub nominal_hz: f64,
    pub interval_avg_us: f64,
    pub interval_median_us: f64,
    pub interval_min_us: f64,
    pub interval_max_us: f64,
    pub interval_p1_us: f64,
    pub interval_p99_us: f64,
    /// Standard deviation of the intervals, µs
    pub jitter_us: f64,
    /// Intervals within ±10 % of the nominal one, %
    pub on_time_pct: f64,
    /// Intervals of about two or more nominal intervals (a report was skipped or merged), %
    pub skipped_pct: f64,
    pub counts_avg: f64,
    pub counts_max: f64,
    /// (time s, interval µs) for the "interval vs time" chart (thinned to at most 20 000 points)
    pub intervals: Vec<(f64, f64)>,
    /// (time s, x counts) for the "x counts vs time" chart (thinned the same way)
    pub x_counts: Vec<(f64, i32)>,
    #[serde(default)]
    pub timestamp: String,
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() as f64 * p) as usize).min(sorted.len() - 1)]
}

/// Statistics of a capture (None with too few reports to say anything)
pub fn analyze(reports: &[Report], source: &str) -> Option<MousePollResult> {
    if reports.len() < 20 {
        return None;
    }
    let mut iv: Vec<(f64, f64)> = Vec::new(); // (t s, interval µs)
    for w in reports.windows(2) {
        let d = w[1].t_ns.saturating_sub(w[0].t_ns);
        if d > 0 && d < PAUSE_NS {
            iv.push((w[1].t_ns as f64 / 1e9, d as f64 / 1000.0));
        }
    }
    if iv.len() < 10 {
        return None;
    }
    let mut sorted: Vec<f64> = iv.iter().map(|x| x.1).collect();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let n = sorted.len() as f64;
    let avg = sorted.iter().sum::<f64>() / n;
    let median = pct(&sorted, 0.5);
    let jitter = (sorted.iter().map(|x| (x - avg).powi(2)).sum::<f64>() / n).sqrt();
    let rate = 1e6 / median;
    let nominal = STANDARD_RATES.iter().copied().min_by(|a, b| ((a / rate).ln().abs()).total_cmp(&(b / rate).ln().abs())).unwrap_or(rate);
    let nominal_us = 1e6 / nominal;
    let on_time = sorted.iter().filter(|x| (**x / nominal_us - 1.0).abs() <= 0.10).count() as f64 / n * 100.0;
    let skipped = sorted.iter().filter(|x| **x >= nominal_us * 1.75).count() as f64 / n * 100.0;
    let counts: Vec<f64> = reports.iter().map(|r| ((r.dx as f64).powi(2) + (r.dy as f64).powi(2)).sqrt()).collect();
    let step = (iv.len() / 20_000).max(1);
    let rstep = (reports.len() / 20_000).max(1);
    Some(MousePollResult {
        source: source.to_string(),
        reports: reports.len(),
        moving_s: iv.iter().map(|x| x.1).sum::<f64>() / 1e6,
        rate_hz: rate,
        nominal_hz: nominal,
        interval_avg_us: avg,
        interval_median_us: median,
        interval_min_us: sorted[0],
        interval_max_us: sorted[sorted.len() - 1],
        interval_p1_us: pct(&sorted, 0.01),
        interval_p99_us: pct(&sorted, 0.99),
        jitter_us: jitter,
        on_time_pct: on_time,
        skipped_pct: skipped,
        counts_avg: counts.iter().sum::<f64>() / counts.len() as f64,
        counts_max: counts.iter().copied().fold(0.0, f64::max),
        intervals: iv.iter().step_by(step).copied().collect(),
        x_counts: reports.iter().step_by(rstep).map(|r| (r.t_ns as f64 / 1e9, r.dx)).collect(),
        timestamp: chrono::Local::now().to_rfc3339(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(us: &[u64]) -> Vec<Report> {
        let mut t = 0;
        us.iter()
            .map(|d| {
                t += d * 1000;
                Report { t_ns: t, dx: 3, dy: 4 }
            })
            .collect()
    }

    #[test]
    fn a_steady_1000_hz_mouse() {
        let r = analyze(&at(&[1000; 500]), "test").unwrap();
        assert_eq!(r.nominal_hz, 1000.0);
        assert!((r.rate_hz - 1000.0).abs() < 1e-6);
        assert!(r.jitter_us < 1e-9 && r.on_time_pct == 100.0 && r.skipped_pct == 0.0);
        assert_eq!((r.counts_avg, r.counts_max), (5.0, 5.0));
    }

    #[test]
    fn jitter_skips_and_pauses() {
        // 8 kHz mouse (125 µs) with some late reports, one skipped report and a pause in the middle
        let mut v = vec![125u64; 400];
        for i in (0..400).step_by(10) {
            v[i] = 150;
        }
        v[200] = 250;
        v[300] = 500_000; // 0.5 s pause: not a polling interval
        let r = analyze(&at(&v), "test").unwrap();
        assert_eq!(r.nominal_hz, 8000.0);
        assert!(r.interval_max_us < 1000.0, "the pause is excluded");
        assert!(r.on_time_pct > 85.0 && r.on_time_pct < 95.0, "{}", r.on_time_pct);
        assert!(r.skipped_pct > 0.0 && r.jitter_us > 0.0);
        assert!(analyze(&at(&[1000; 5]), "test").is_none(), "too few reports");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_missing_device_is_a_clear_error() {
        if !std::path::Path::new("/dev/input").exists() {
            let e = start().err().expect("no devices, no capture");
            assert!(e.contains("no mouse") || e.contains("permission"), "{}", e);
        }
    }

    #[test]
    fn rates_snap_to_the_nearest_setting() {
        assert_eq!(analyze(&at(&[2000; 100]), "t").unwrap().nominal_hz, 500.0);
        assert_eq!(analyze(&at(&[520; 100]), "t").unwrap().nominal_hz, 2000.0);
        assert_eq!(analyze(&at(&[8000; 100]), "t").unwrap().nominal_hz, 125.0);
    }
}

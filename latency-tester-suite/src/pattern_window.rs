//! Precise display pattern: a separate full-screen window (`latency-tester --pattern ...`) that runs
//! VSync-locked and switches white / black on whole refresh periods, or moves lines / a square across
//! the screen for ghosting (motion blur, overdrive) tests.
//!
//! The main app runs with VSync off (for input timing), so its pattern changes whenever a frame
//! happens to be drawn and the compositor shows whatever was newest at each refresh: a 1 ms white
//! phase is then sometimes visible and sometimes not. A display cannot show anything shorter than one
//! refresh anyway, so here every phase is a whole number of frames (at least one) and each frame is
//! presented on a refresh: the flashes are exactly uniform, and late (dropped) frames are counted.

use eframe::egui;
use std::time::Instant;

/// What moves in the ghosting test
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    /// A vertical line sweeping left to right
    Vertical,
    /// A horizontal line sweeping top to bottom
    Horizontal,
    /// Both lines, in step: they cross exactly at the centre of the screen
    Both,
    /// A square sweeping left to right through the middle
    Square,
}

impl Motion {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "vertical" | "v" => Some(Motion::Vertical),
            "horizontal" | "h" => Some(Motion::Horizontal),
            "both" | "cross" => Some(Motion::Both),
            "square" => Some(Motion::Square),
            _ => None,
        }
    }

    pub fn arg(self) -> &'static str {
        match self {
            Motion::Vertical => "vertical",
            Motion::Horizontal => "horizontal",
            Motion::Both => "both",
            Motion::Square => "square",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PatternArgs {
    pub on_ms: f64,
    pub off_ms: f64,
    /// 0 = until Esc
    pub cycles: u32,
    pub windowed: bool,
    /// Ghosting test instead of flashes
    pub motion: Option<Motion>,
    /// Line width / square size, px
    pub width: f32,
    /// Time for one sweep across the screen, ms
    pub sweep_ms: f64,
    /// Display to open on (index into `displays::list()`), None = where the OS puts it
    pub display: Option<usize>,
    /// Black screen before the pattern starts (the refresh rate is measured meanwhile), ms
    pub delay_ms: f64,
}

impl Default for PatternArgs {
    fn default() -> Self {
        Self { on_ms: 100.0, off_ms: 100.0, cycles: 0, windowed: false, motion: None, width: 8.0, sweep_ms: 2000.0, display: None, delay_ms: 5000.0 }
    }
}

pub fn parse_args(args: &[String]) -> PatternArgs {
    let mut p = PatternArgs::default();
    let val = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse::<f64>().ok());
    if let Some(v) = val("--on-ms") {
        p.on_ms = v.max(0.0);
    }
    if let Some(v) = val("--off-ms") {
        p.off_ms = v.max(0.0);
    }
    if let Some(v) = val("--cycles") {
        p.cycles = v.max(0.0) as u32;
    }
    if let Some(v) = val("--width") {
        p.width = v.clamp(1.0, 2000.0) as f32;
    }
    if let Some(v) = val("--sweep-ms") {
        p.sweep_ms = v.clamp(100.0, 60_000.0);
    }
    if let Some(v) = val("--delay-ms") {
        p.delay_ms = v.clamp(0.0, 600_000.0);
    }
    if let Some(v) = val("--display") {
        p.display = (v >= 1.0).then(|| v as usize - 1);
    }
    p.motion = args.iter().position(|a| a == "--motion").and_then(|i| args.get(i + 1)).and_then(|m| Motion::parse(m));
    p.windowed = args.iter().any(|a| a == "--windowed");
    p
}

/// Frames per sweep: whole frames, and an even number so the middle of the sweep is a frame of its own
/// (with "both", the two lines then cross exactly at the centre)
pub fn sweep_frames(sweep_ms: f64, refresh_ms: f64) -> u32 {
    let n = if refresh_ms > 0.0 { (sweep_ms / refresh_ms).round() as u32 } else { 2 };
    let n = n.max(2);
    n + n % 2
}

/// Where the moving object is on frame `k` of a sweep of `n` frames, as a fraction 0 ≤ f < 1
pub fn sweep_pos(k: u64, n: u32) -> f64 {
    (k % n as u64) as f64 / n as f64
}

/// Whole frames for a phase of `ms` at `refresh_ms` per frame: rounded, and never less than one
pub fn frames_for(ms: f64, refresh_ms: f64) -> u32 {
    if refresh_ms <= 0.0 {
        return 1;
    }
    ((ms / refresh_ms).round() as u32).max(1)
}

/// Frames used to measure the refresh period before the pattern starts
const CALIBRATION_FRAMES: usize = 120;

pub struct PatternApp {
    args: PatternArgs,
    _timer: crate::timer_info::HighResolutionTimer,
    last: Option<Instant>,
    calib: Vec<f64>,
    refresh_ms: Option<f64>,
    on_frames: u32,
    off_frames: u32,
    white: bool,
    frame_in_phase: u32,
    cycles_done: u32,
    intervals: Vec<f64>,
    late: u32,
    show_info: bool,
    done: bool,
    /// How this window is drawn (shown in the info line)
    pub renderer: String,
    opened: Instant,
    /// Frames drawn since the motion started
    motion_frame: u64,
    sweep_n: u32,
    /// The window still has to go full screen (after being placed on its display)
    pending_fullscreen: bool,
}

impl PatternApp {
    pub fn new(args: PatternArgs) -> Self {
        Self {
            args,
            _timer: crate::timer_info::HighResolutionTimer::acquire(),
            last: None,
            calib: Vec::new(),
            refresh_ms: None,
            on_frames: 1,
            off_frames: 1,
            white: false,
            frame_in_phase: 0,
            cycles_done: 0,
            intervals: Vec::new(),
            late: 0,
            show_info: true,
            done: false,
            renderer: String::new(),
            opened: Instant::now(),
            motion_frame: 0,
            sweep_n: 2,
            pending_fullscreen: false,
        }
    }

    /// Still in the black lead-in before the pattern starts (also until the refresh rate is known)
    fn waiting(&self) -> bool {
        self.refresh_ms.is_none() || self.opened.elapsed().as_secs_f64() * 1000.0 < self.args.delay_ms
    }

    /// Advance one presented frame of `dt_ms`; returns whether this frame is white
    fn step(&mut self, dt_ms: Option<f64>) -> bool {
        let Some(refresh) = self.refresh_ms else {
            if let Some(dt) = dt_ms {
                self.calib.push(dt);
            }
            if self.calib.len() >= CALIBRATION_FRAMES {
                let mut v = self.calib.clone();
                v.sort_by(|a, b| a.total_cmp(b));
                let r = v[v.len() / 2];
                self.refresh_ms = Some(r);
                self.on_frames = frames_for(self.args.on_ms, r);
                self.off_frames = frames_for(self.args.off_ms, r);
                self.white = true;
                self.frame_in_phase = 0;
            }
            return false;
        };
        if let Some(dt) = dt_ms {
            self.intervals.push(dt);
            if self.intervals.len() > 10_000 {
                self.intervals.drain(..5_000);
            }
            if dt > refresh * 1.5 {
                self.late += 1; // a refresh was missed: that phase was one frame too long
            }
        }
        if self.done {
            return false;
        }
        let shown_white = self.white;
        self.frame_in_phase += 1;
        let limit = if self.white { self.on_frames } else { self.off_frames };
        if self.frame_in_phase >= limit {
            self.frame_in_phase = 0;
            if !self.white {
                self.cycles_done += 1;
                if self.args.cycles > 0 && self.cycles_done >= self.args.cycles {
                    self.done = true;
                }
            }
            self.white = !self.white;
        }
        shown_white
    }
}

impl eframe::App for PatternApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if std::mem::take(&mut self.pending_fullscreen) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
        }
        let now = Instant::now();
        let dt = self.last.map(|l| now.duration_since(l).as_secs_f64() * 1000.0);
        self.last = Some(now);
        let waiting = self.waiting();
        // the flash sequence (and the refresh calibration) runs from the first frame; its flashes only
        // show after the lead-in
        let flash_white = if self.args.motion.is_none() && !waiting { self.step(dt) } else {
            if self.refresh_ms.is_none() {
                self.step(dt);
            } else if let Some(d) = dt {
                self.intervals.push(d);
            }
            false
        };
        ctx.request_repaint();

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if ctx.input(|i| i.key_pressed(egui::Key::I)) {
            self.show_info = !self.show_info;
        }

        egui::CentralPanel::default().frame(egui::Frame::none()).show(ctx, |ui| {
            let rect = ui.max_rect();
            let painter = ui.painter();
            match self.args.motion {
                Some(m) if !waiting => {
                    painter.rect_filled(rect, 0.0, egui::Color32::BLACK);
                    let f = sweep_pos(self.motion_frame, self.sweep_n) as f32;
                    self.motion_frame += 1;
                    let w = self.args.width;
                    let x = rect.left() + f * rect.width();
                    let y = rect.top() + f * rect.height();
                    let white = egui::Color32::WHITE;
                    if matches!(m, Motion::Vertical | Motion::Both) {
                        painter.rect_filled(egui::Rect::from_min_max(egui::pos2(x - w / 2.0, rect.top()), egui::pos2(x + w / 2.0, rect.bottom())), 0.0, white);
                    }
                    if matches!(m, Motion::Horizontal | Motion::Both) {
                        painter.rect_filled(egui::Rect::from_min_max(egui::pos2(rect.left(), y - w / 2.0), egui::pos2(rect.right(), y + w / 2.0)), 0.0, white);
                    }
                    if m == Motion::Square {
                        painter.rect_filled(egui::Rect::from_center_size(egui::pos2(x, rect.center().y), egui::vec2(w, w)), 0.0, white);
                    }
                }
                _ => {
                    painter.rect_filled(rect, 0.0, if flash_white { egui::Color32::WHITE } else { egui::Color32::BLACK });
                }
            }
            if waiting {
                let left = (self.args.delay_ms / 1000.0 - self.opened.elapsed().as_secs_f64()).max(0.0);
                let text = if self.refresh_ms.is_none() { format!("measuring the refresh rate… starts in {:.0} s", left.ceil()) } else { format!("starts in {:.0} s", left.ceil()) };
                painter.text(rect.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(22.0), egui::Color32::from_gray(90));
            }
            if self.show_info && !waiting {
                let text = match (self.refresh_ms, self.args.motion) {
                    (Some(r), Some(m)) => {
                        let mut v = self.intervals.clone();
                        v.sort_by(|a, b| a.total_cmp(b));
                        let pct = |p: f64| v.get(((v.len() as f64 * p) as usize).min(v.len().saturating_sub(1))).copied().unwrap_or(0.0);
                        format!(
                            "{:.2} Hz · {} · {} px · sweep {} frames = {:.0} ms ({:.1} px/frame across) · frame p50 {:.3} / p99 {:.3} ms · late frames {}   ·   I = hide text, Esc = close",
                            1000.0 / r,
                            m.arg(),
                            self.args.width,
                            self.sweep_n,
                            self.sweep_n as f64 * r,
                            rect.width() as f64 / self.sweep_n as f64,
                            pct(0.5),
                            pct(0.99),
                            self.late,
                        )
                    }
                    (None, _) => format!("measuring the refresh rate… ({}/{})", self.calib.len(), CALIBRATION_FRAMES),
                    (Some(r), None) => {
                        let mut v = self.intervals.clone();
                        v.sort_by(|a, b| a.total_cmp(b));
                        let pct = |p: f64| v.get(((v.len() as f64 * p) as usize).min(v.len().saturating_sub(1))).copied().unwrap_or(0.0);
                        format!(
                            "{:.2} Hz ({:.3} ms/frame) · white {} frame(s) = {:.2} ms, black {} = {:.2} ms (asked {} / {} ms) · cycle {}{} · \
                             frame p50 {:.3} / p99 {:.3} ms · late frames {}{}   ·   I = hide text, Esc = close",
                            1000.0 / r,
                            r,
                            self.on_frames,
                            self.on_frames as f64 * r,
                            self.off_frames,
                            self.off_frames as f64 * r,
                            self.args.on_ms,
                            self.args.off_ms,
                            self.cycles_done,
                            if self.args.cycles > 0 { format!("/{}", self.args.cycles) } else { String::new() },
                            pct(0.5),
                            pct(0.99),
                            self.late,
                            if self.done { " · finished" } else { "" },
                        )
                    }
                };
                let text = format!("{}\n{}", text, self.renderer);
                let pos = rect.left_bottom() + egui::vec2(12.0, -12.0);
                painter.text(pos, egui::Align2::LEFT_BOTTOM, text, egui::FontId::monospace(13.0), egui::Color32::from_gray(128));
            }
        });
        if let (Some(r), true) = (self.refresh_ms, self.sweep_n == 2 && self.args.motion.is_some()) {
            self.sweep_n = sweep_frames(self.args.sweep_ms, r);
        }
    }
}

/// Put the window on `d` exactly (physical pixels), before it goes full screen there
#[cfg(target_os = "windows")]
fn place_on(cc: &eframe::CreationContext<'_>, d: &crate::displays::Display) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER};
    if let Ok(h) = cc.window_handle() {
        if let RawWindowHandle::Win32(w) = h.as_raw() {
            unsafe {
                let _ = SetWindowPos(HWND(w.hwnd.get() as *mut _), None, d.x, d.y, d.w, d.h, SWP_NOZORDER | SWP_NOACTIVATE);
            }
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn place_on(_cc: &eframe::CreationContext<'_>, _d: &crate::displays::Display) {}

/// Run the pattern window (blocks until it is closed)
pub fn run(args: &[String]) -> anyhow::Result<()> {
    let p = parse_args(args);
    let title = match p.motion {
        Some(_) => "Latency Tester: ghosting test (Esc closes)",
        None => "Latency Tester: display pattern (Esc closes)",
    };
    let display = p.display.and_then(|i| crate::displays::list().into_iter().nth(i));
    let mut viewport = egui::ViewportBuilder::default().with_title(title);
    viewport = match (&display, p.windowed) {
        (_, true) => viewport.with_inner_size([900.0, 600.0]),
        // placed on its display first (exactly, on Windows), then full screen there
        (Some(d), false) => viewport.with_position([d.x as f32 + 40.0, d.y as f32 + 40.0]).with_inner_size([640.0, 400.0]),
        (None, false) => viewport.with_fullscreen(true),
    };
    // VSync-locked; Vulkan with FIFO presentation when available, OpenGL otherwise
    crate::render_setup::run_with_fallback("Latency Tester pattern", crate::render_setup::parse(args), true, viewport, move |cc| {
        let mut app = PatternApp::new(p.clone());
        app.renderer = crate::render_setup::describe(cc, true);
        if let (Some(d), false) = (&display, p.windowed) {
            place_on(cc, d);
            app.pending_fullscreen = true;
        }
        Box::new(app)
    })
}

/// Arguments for the pattern window
pub fn command_args(p: &PatternArgs) -> Vec<String> {
    let mut a = vec!["--pattern".to_string(), "--delay-ms".into(), p.delay_ms.to_string()];
    match p.motion {
        Some(m) => a.extend(["--motion".into(), m.arg().into(), "--width".into(), p.width.to_string(), "--sweep-ms".into(), p.sweep_ms.to_string()]),
        None => a.extend(["--on-ms".into(), p.on_ms.to_string(), "--off-ms".into(), p.off_ms.to_string(), "--cycles".into(), p.cycles.to_string()]),
    }
    if let Some(d) = p.display {
        a.extend(["--display".into(), (d + 1).to_string()]);
    }
    if p.windowed {
        a.push("--windowed".into());
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phases_are_whole_frames_and_never_zero() {
        let r = 1000.0 / 144.0; // 6.94 ms
        assert_eq!(frames_for(1.0, r), 1, "1 ms white = exactly one frame");
        assert_eq!(frames_for(500.0, r), 72);
        assert_eq!(frames_for(0.0, r), 1);
        assert_eq!(frames_for(10.0, 1000.0 / 60.0), 1);
    }

    #[test]
    fn args_parse() {
        let a: Vec<String> = ["--pattern", "--on-ms", "1", "--off-ms", "500", "--cycles", "20", "--windowed"].iter().map(|s| s.to_string()).collect();
        assert_eq!(parse_args(&a), PatternArgs { on_ms: 1.0, off_ms: 500.0, cycles: 20, windowed: true, ..Default::default() });
        // the GUI's arguments come back as they went in
        let m = PatternArgs { motion: Some(Motion::Both), width: 12.0, sweep_ms: 1500.0, display: Some(1), delay_ms: 3000.0, ..Default::default() };
        assert_eq!(parse_args(&command_args(&m)), m);
        let f = PatternArgs { on_ms: 2.0, off_ms: 30.0, cycles: 5, display: None, ..Default::default() };
        assert_eq!(parse_args(&command_args(&f)), f);
        assert_eq!(PatternArgs::default().delay_ms, 5000.0, "5 s lead-in by default");
    }

    #[test]
    fn both_lines_cross_exactly_at_the_centre() {
        let r = 1000.0 / 144.0;
        let n = sweep_frames(2000.0, r);
        assert_eq!(n % 2, 0);
        assert_eq!(n, 288);
        // frame n/2: the vertical line is at half the width and the horizontal one at half the height
        assert_eq!(sweep_pos(n as u64 / 2, n), 0.5);
        assert_eq!(sweep_pos(n as u64, n), 0.0, "and it starts over");
        assert_eq!(sweep_frames(100.0, 1000.0 / 60.0), 6);
        assert_eq!(sweep_frames(10.0, 1000.0 / 60.0), 2, "at least two frames");
        assert_eq!(Motion::parse("square"), Some(Motion::Square));
    }

    #[test]
    fn a_uniform_pattern_after_calibration() {
        let mut app = PatternApp::new(PatternArgs { on_ms: 1.0, off_ms: 20.0, cycles: 3, windowed: true, ..Default::default() });
        let r = 1000.0 / 144.0;
        // calibration frames are black
        for _ in 0..CALIBRATION_FRAMES {
            assert!(!app.step(Some(r)));
        }
        assert_eq!((app.on_frames, app.off_frames), (1, 3));
        let seq: Vec<bool> = (0..12).map(|_| app.step(Some(r))).collect();
        // white 1 frame, black 3 frames, repeated exactly; stops after 3 cycles
        assert_eq!(seq, vec![true, false, false, false, true, false, false, false, true, false, false, false]);
        assert!(app.done && !app.step(Some(r)));
        assert_eq!(app.late, 0);
        app.step(Some(r * 2.1));
        assert_eq!(app.late, 1, "a missed refresh is counted");
    }
}

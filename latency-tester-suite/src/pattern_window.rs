//! Precise display pattern: a separate full-screen window (`latency-tester --pattern ...`) that runs
//! VSync-locked and switches white / black on whole refresh periods.
//!
//! The main app runs with VSync off (for input timing), so its pattern changes whenever a frame
//! happens to be drawn and the compositor shows whatever was newest at each refresh: a 1 ms white
//! phase is then sometimes visible and sometimes not. A display cannot show anything shorter than one
//! refresh anyway, so here every phase is a whole number of frames (at least one) and each frame is
//! presented on a refresh: the flashes are exactly uniform, and late (dropped) frames are counted.

use eframe::egui;
use std::time::Instant;

#[derive(Debug, Clone, PartialEq)]
pub struct PatternArgs {
    pub on_ms: f64,
    pub off_ms: f64,
    /// 0 = until Esc
    pub cycles: u32,
    pub windowed: bool,
}

impl Default for PatternArgs {
    fn default() -> Self {
        Self { on_ms: 100.0, off_ms: 100.0, cycles: 0, windowed: false }
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
    p.windowed = args.iter().any(|a| a == "--windowed");
    p
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
        }
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
        let now = Instant::now();
        let dt = self.last.map(|l| now.duration_since(l).as_secs_f64() * 1000.0);
        self.last = Some(now);
        let white = self.step(dt);
        ctx.request_repaint();

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if ctx.input(|i| i.key_pressed(egui::Key::I)) {
            self.show_info = !self.show_info;
        }

        egui::CentralPanel::default().frame(egui::Frame::none()).show(ctx, |ui| {
            let rect = ui.max_rect();
            ui.painter().rect_filled(rect, 0.0, if white { egui::Color32::WHITE } else { egui::Color32::BLACK });
            if self.show_info {
                let text = match self.refresh_ms {
                    None => format!("measuring the refresh rate… ({}/{})", self.calib.len(), CALIBRATION_FRAMES),
                    Some(r) => {
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
                ui.painter().text(pos, egui::Align2::LEFT_BOTTOM, text, egui::FontId::monospace(13.0), egui::Color32::from_gray(128));
            }
        });
    }
}

/// Run the pattern window (blocks until it is closed)
pub fn run(args: &[String]) -> anyhow::Result<()> {
    let p = parse_args(args);
    let mut viewport = egui::ViewportBuilder::default().with_title("Latency Tester: display pattern (Esc closes)");
    viewport = if p.windowed { viewport.with_inner_size([900.0, 600.0]) } else { viewport.with_fullscreen(true) };
    // VSync-locked; Vulkan with FIFO presentation when available, OpenGL otherwise
    crate::render_setup::run_with_fallback("Latency Tester pattern", crate::render_setup::parse(args), true, viewport, move |cc| {
        let mut app = PatternApp::new(p.clone());
        app.renderer = crate::render_setup::describe(cc, true);
        Box::new(app)
    })
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
        assert_eq!(parse_args(&a), PatternArgs { on_ms: 1.0, off_ms: 500.0, cycles: 20, windowed: true });
    }

    #[test]
    fn a_uniform_pattern_after_calibration() {
        let mut app = PatternApp::new(PatternArgs { on_ms: 1.0, off_ms: 20.0, cycles: 3, windowed: true });
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

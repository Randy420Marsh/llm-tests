//! Measurement-rig support: calibration offsets for the automatic press/click robot and the
//! display-test patterns used with the photodiode + Arduino rig (see docs/latency-rig).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::input_test::InputKind;

/// Delays that belong to the measuring hardware, not to the system under test.
/// Measure them with the Arduino sketch's calibration command and enter them here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RigCalibration {
    /// Light sensor + microcontroller + transistor + switch closing, for the mouse tap (ms)
    pub mouse_robot_ms: f64,
    /// Same for the keyboard robot, including the solenoid's travel to the actuation point (ms)
    pub keyboard_robot_ms: f64,
    /// Time from "frame submitted" to "light emitted": display latency (ms). Only subtract this if
    /// you want the input stack alone.
    pub display_ms: f64,
    pub subtract_robot: bool,
    pub subtract_display: bool,
}

impl Default for RigCalibration {
    fn default() -> Self {
        Self { mouse_robot_ms: 0.0, keyboard_robot_ms: 0.0, display_ms: 0.0, subtract_robot: true, subtract_display: false }
    }
}

/// One latency with the rig's own delays removed step by step
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Corrected {
    pub raw_ms: f64,
    pub minus_robot_ms: f64,
    pub minus_robot_and_display_ms: f64,
}

impl RigCalibration {
    pub fn robot_ms(&self, kind: InputKind) -> f64 {
        match kind {
            InputKind::MouseClick => self.mouse_robot_ms,
            InputKind::KeyPress => self.keyboard_robot_ms,
        }
    }

    /// Human runs are never corrected (a person has no "robot delay")
    pub fn correct(&self, kind: InputKind, robot: bool, raw_ms: f64) -> Corrected {
        if !robot {
            return Corrected { raw_ms, minus_robot_ms: raw_ms, minus_robot_and_display_ms: raw_ms };
        }
        let after_robot = if self.subtract_robot { raw_ms - self.robot_ms(kind) } else { raw_ms };
        let after_display = if self.subtract_display { after_robot - self.display_ms } else { after_robot };
        Corrected { raw_ms, minus_robot_ms: after_robot, minus_robot_and_display_ms: after_display }
    }

    /// `rig_calibration.json` next to the executable (falls back to the working directory)
    pub fn default_path() -> PathBuf {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("rig_calibration.json")))
            .unwrap_or_else(|| PathBuf::from("rig_calibration.json"))
    }

    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        std::fs::write(path, serde_json::to_string_pretty(self).unwrap_or_default())
    }
}

// ---------------------------------------------------------------------------------------------
// Display test patterns
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternMode {
    /// White / black alternating for known times: the rig measures rise, fall, on-time, period
    SquareWave,
    /// Stays black; every trigger key (the Arduino sends F13) makes a white flash: the rig
    /// measures trigger-to-light time
    FlashOnKey,
}

#[derive(Debug, Clone)]
pub struct DisplayTestConfig {
    pub mode: PatternMode,
    pub on_ms: f64,
    pub off_ms: f64,
    pub cycles: u32,
    pub flash_ms: f64,
}

impl Default for DisplayTestConfig {
    fn default() -> Self {
        Self { mode: PatternMode::SquareWave, on_ms: 50.0, off_ms: 50.0, cycles: 40, flash_ms: 100.0 }
    }
}

/// Drives the pattern; like the trial engine it takes injected time and counts drawn frames so
/// every phase is shown for at least one frame.
pub struct DisplayTest {
    pub cfg: DisplayTestConfig,
    running: bool,
    white: bool,
    phase_start: f64,
    frames_in_phase: u32,
    cycles_done: u32,
    /// Time each white / black phase really lasted (ms, measured between frames)
    pub on_shown_ms: Vec<f64>,
    pub off_shown_ms: Vec<f64>,
    pub triggers: u32,
    /// Trigger event → first frame that shows white (host side only)
    pub trigger_to_frame_ms: Vec<f64>,
    pending_trigger: Option<f64>,
}

impl DisplayTest {
    pub fn new(cfg: DisplayTestConfig) -> Self {
        Self {
            cfg,
            running: false,
            white: false,
            phase_start: 0.0,
            frames_in_phase: 0,
            cycles_done: 0,
            on_shown_ms: Vec::new(),
            off_shown_ms: Vec::new(),
            triggers: 0,
            trigger_to_frame_ms: Vec::new(),
            pending_trigger: None,
        }
    }

    pub fn start(&mut self, now: f64) {
        self.running = true;
        self.white = false;
        self.phase_start = now;
        self.frames_in_phase = 0;
        self.cycles_done = 0;
        self.on_shown_ms.clear();
        self.off_shown_ms.clear();
        self.triggers = 0;
        self.trigger_to_frame_ms.clear();
        self.pending_trigger = None;
    }

    pub fn stop(&mut self) {
        self.running = false;
        self.white = false;
    }

    pub fn is_running(&self) -> bool {
        self.running
    }
    pub fn is_white(&self) -> bool {
        self.running && self.white
    }
    pub fn cycles_done(&self) -> u32 {
        self.cycles_done
    }

    /// The trigger key arrived (FlashOnKey mode)
    pub fn on_trigger(&mut self, now: f64) {
        if self.running && self.cfg.mode == PatternMode::FlashOnKey && !self.white && self.pending_trigger.is_none() {
            self.triggers += 1;
            self.pending_trigger = Some(now);
        }
    }

    /// Call once per frame before drawing
    pub fn tick(&mut self, now: f64) {
        if !self.running {
            return;
        }
        match self.cfg.mode {
            PatternMode::SquareWave => {
                let dur = if self.white { self.cfg.on_ms } else { self.cfg.off_ms };
                if self.frames_in_phase >= 1 && now - self.phase_start >= dur {
                    let shown = now - self.phase_start;
                    if self.white {
                        self.on_shown_ms.push(shown);
                        self.cycles_done += 1;
                        if self.cycles_done >= self.cfg.cycles {
                            self.running = false;
                            self.white = false;
                            return;
                        }
                    } else if self.cycles_done > 0 || self.phase_start > 0.0 {
                        // the first black phase is only a settling period
                        self.off_shown_ms.push(shown);
                    }
                    self.white = !self.white;
                    self.phase_start = now;
                    self.frames_in_phase = 0;
                }
                self.frames_in_phase += 1;
            }
            PatternMode::FlashOnKey => {
                if !self.white {
                    if let Some(t) = self.pending_trigger.take() {
                        self.white = true;
                        self.phase_start = now;
                        self.frames_in_phase = 0;
                        self.trigger_to_frame_ms.push(now - t);
                    }
                } else if self.frames_in_phase >= 1 && now - self.phase_start >= self.cfg.flash_ms {
                    self.on_shown_ms.push(now - self.phase_start);
                    self.white = false;
                    self.phase_start = now;
                    self.frames_in_phase = 0;
                }
                self.frames_in_phase += 1;
            }
        }
    }
}

pub fn stats(v: &[f64]) -> Option<(f64, f64, f64)> {
    if v.is_empty() {
        return None;
    }
    let min = v.iter().cloned().fold(f64::MAX, f64::min);
    let max = v.iter().cloned().fold(f64::MIN, f64::max);
    Some((min, v.iter().sum::<f64>() / v.len() as f64, max))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn robot_delay_is_subtracted_only_for_robot_runs() {
        let cal = RigCalibration { mouse_robot_ms: 1.2, keyboard_robot_ms: 6.5, display_ms: 8.0, subtract_robot: true, subtract_display: true };
        let c = cal.correct(InputKind::KeyPress, true, 30.0);
        assert!((c.minus_robot_ms - 23.5).abs() < 1e-9);
        assert!((c.minus_robot_and_display_ms - 15.5).abs() < 1e-9);
        let m = cal.correct(InputKind::MouseClick, true, 20.0);
        assert!((m.minus_robot_ms - 18.8).abs() < 1e-9);
        let human = cal.correct(InputKind::MouseClick, false, 230.0);
        assert_eq!(human.minus_robot_and_display_ms, 230.0);
    }

    #[test]
    fn toggles_control_what_is_subtracted() {
        let cal = RigCalibration { mouse_robot_ms: 2.0, display_ms: 5.0, subtract_robot: false, subtract_display: true, ..Default::default() };
        let c = cal.correct(InputKind::MouseClick, true, 20.0);
        assert_eq!(c.minus_robot_ms, 20.0);
        assert_eq!(c.minus_robot_and_display_ms, 15.0);
    }

    #[test]
    fn calibration_round_trips_and_missing_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cal.json");
        assert_eq!(RigCalibration::load(&path), RigCalibration::default());
        let cal = RigCalibration { mouse_robot_ms: 0.8, keyboard_robot_ms: 7.25, display_ms: 3.5, subtract_robot: true, subtract_display: true };
        cal.save(&path).unwrap();
        assert_eq!(RigCalibration::load(&path), cal);
        std::fs::write(&path, "garbage").unwrap();
        assert_eq!(RigCalibration::load(&path), RigCalibration::default());
    }

    #[test]
    fn square_wave_runs_the_requested_cycles_and_records_shown_times() {
        let mut t = DisplayTest::new(DisplayTestConfig { mode: PatternMode::SquareWave, on_ms: 50.0, off_ms: 30.0, cycles: 5, flash_ms: 0.0 });
        let mut now = 1.0;
        t.start(now);
        let mut seen_white = 0;
        while t.is_running() && now < 10_000.0 {
            t.tick(now);
            if t.is_white() {
                seen_white += 1;
            }
            now += 4.0; // 250 Hz
        }
        assert_eq!(t.cycles_done(), 5);
        assert_eq!(t.on_shown_ms.len(), 5);
        assert!(seen_white > 0);
        let (min, avg, _) = stats(&t.on_shown_ms).unwrap();
        assert!(min >= 50.0 && avg < 58.0, "on times {:?}", t.on_shown_ms);
        let (omin, _, _) = stats(&t.off_shown_ms).unwrap();
        assert!(omin >= 30.0);
    }

    #[test]
    fn every_phase_gets_a_frame_even_on_slow_displays() {
        let mut t = DisplayTest::new(DisplayTestConfig { mode: PatternMode::SquareWave, on_ms: 5.0, off_ms: 5.0, cycles: 3, flash_ms: 0.0 });
        let mut now = 1.0;
        t.start(now);
        let mut whites = 0;
        let mut last = false;
        while t.is_running() && now < 5000.0 {
            t.tick(now);
            let w = t.is_white();
            if w && !last {
                whites += 1;
            }
            last = w;
            now += 16.7; // 60 Hz: 5 ms phases stretch to one frame
        }
        assert_eq!(whites, 3);
        assert!(t.on_shown_ms.iter().all(|d| *d >= 16.0));
    }

    #[test]
    fn flash_on_key_reports_trigger_to_frame_delay() {
        let mut t = DisplayTest::new(DisplayTestConfig { mode: PatternMode::FlashOnKey, flash_ms: 40.0, ..Default::default() });
        t.start(0.0);
        t.tick(1.0);
        assert!(!t.is_white());
        t.on_trigger(10.0);
        t.tick(12.5);
        assert!(t.is_white());
        assert_eq!(t.trigger_to_frame_ms, vec![2.5]);
        t.on_trigger(20.0); // ignored while white
        assert_eq!(t.triggers, 1);
        t.tick(30.0);
        assert!(t.is_white());
        t.tick(60.0);
        assert!(!t.is_white());
        assert_eq!(t.on_shown_ms.len(), 1);
        // a trigger before start is ignored
        let mut idle = DisplayTest::new(DisplayTestConfig::default());
        idle.on_trigger(1.0);
        assert_eq!(idle.triggers, 0);
    }
}

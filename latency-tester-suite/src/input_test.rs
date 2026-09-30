//! Click / key press latency test engine (mouse and keyboard as separate tests).
//!
//! Pure logic with injected time so it can be unit tested. The GUI calls [`Engine::tick`] once per
//! frame *before* drawing, draws [`Engine::visual`], and forwards input events with
//! [`Engine::on_input`].
//!
//! One trial: RED screen with a BLACK bar on the right, for a random 500–2000 ms → GREEN screen
//! with a WHITE bar (the stimulus; the clock starts on the first frame that shows it) → the
//! input arrives → BLUE result screen whose bar flashes a RED / BLACK / RED "finish" signal
//! (5 ms each by default, but never less than one drawn frame) so an automatic rig knows the
//! trial is over and can release its button.

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum InputKind {
    MouseClick,
    KeyPress,
}

impl InputKind {
    pub fn label(self) -> &'static str {
        match self {
            InputKind::MouseClick => "Mouse click",
            InputKind::KeyPress => "Keyboard press",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrialConfig {
    pub kind: InputKind,
    /// Trials averaged into one result
    pub trials: u32,
    pub wait_min_ms: f64,
    pub wait_max_ms: f64,
    /// Length of each of the three finish-signal segments (red, black, red)
    pub finish_segment_ms: f64,
    /// How long the blue result screen stays up
    pub result_hold_ms: f64,
    /// An automatic rig presses instead of a person (no false starts, tighter timeout)
    pub robot: bool,
    /// Width of the signalling bar as a fraction of the test area
    pub bar_fraction: f32,
}

impl Default for TrialConfig {
    fn default() -> Self {
        Self {
            kind: InputKind::MouseClick,
            trials: 10,
            wait_min_ms: 500.0,
            wait_max_ms: 2000.0,
            finish_segment_ms: 5.0,
            result_hold_ms: 350.0,
            robot: false,
            bar_fraction: 0.10,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Idle,
    Waiting,
    FalseStart,
    Ready,
    Result,
    Summary,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bg {
    Idle,
    Red,
    Green,
    Blue,
    Yellow,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bar {
    /// Dark grey (idle / summary)
    Dim,
    Black,
    White,
    Red,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Visual {
    pub bg: Bg,
    pub bar: Bar,
    pub label: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunSummary {
    pub kind: InputKind,
    pub robot: bool,
    pub samples_ms: Vec<f64>,
    pub mean_ms: f64,
    pub median_ms: f64,
    pub min_ms: f64,
    pub max_ms: f64,
    pub std_ms: f64,
    pub false_starts: u32,
    pub missed: u32,
    pub wait_range_ms: (f64, f64),
    pub timestamp: String,
}

pub fn summarize(kind: InputKind, robot: bool, samples: &[f64], false_starts: u32, missed: u32, wait: (f64, f64)) -> RunSummary {
    let n = samples.len().max(1) as f64;
    let mean = samples.iter().sum::<f64>() / n;
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let median = match sorted.len() {
        0 => 0.0,
        l if l % 2 == 1 => sorted[l / 2],
        l => (sorted[l / 2 - 1] + sorted[l / 2]) / 2.0,
    };
    let std = if samples.len() > 1 {
        (samples.iter().map(|s| (s - mean).powi(2)).sum::<f64>() / (samples.len() - 1) as f64).sqrt()
    } else {
        0.0
    };
    RunSummary {
        kind,
        robot,
        samples_ms: samples.to_vec(),
        mean_ms: mean,
        median_ms: median,
        min_ms: sorted.first().copied().unwrap_or(0.0),
        max_ms: sorted.last().copied().unwrap_or(0.0),
        std_ms: std,
        false_starts,
        missed,
        wait_range_ms: wait,
        timestamp: chrono::Utc::now().to_rfc3339(),
    }
}

const FALSE_START_MS: f64 = 900.0;
/// Human: give up on a trial after this long on green
const HUMAN_TIMEOUT_MS: f64 = 5000.0;
/// Robot: the rig should react within milliseconds
const ROBOT_TIMEOUT_MS: f64 = 1500.0;

pub struct Engine {
    pub cfg: TrialConfig,
    phase: Phase,
    phase_start: f64,
    wait_ms: f64,
    ready_at: f64,
    samples: Vec<f64>,
    last_ms: Option<f64>,
    false_starts: u32,
    missed: u32,
    /// Waits actually used (for verification / display)
    waits_used: Vec<f64>,
    /// finish signal: index of the segment being shown (0,1,2), None when not active
    finish_step: Option<usize>,
    finish_step_start: f64,
    frames_in_step: u32,
    summary: Option<RunSummary>,
    rng: StdRng,
}

impl Engine {
    pub fn new(cfg: TrialConfig, seed: Option<u64>) -> Self {
        Self {
            cfg,
            phase: Phase::Idle,
            phase_start: 0.0,
            wait_ms: 0.0,
            ready_at: 0.0,
            samples: Vec::new(),
            last_ms: None,
            false_starts: 0,
            missed: 0,
            waits_used: Vec::new(),
            finish_step: None,
            finish_step_start: 0.0,
            frames_in_step: 0,
            summary: None,
            rng: match seed {
                Some(s) => StdRng::seed_from_u64(s),
                None => StdRng::from_entropy(),
            },
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn samples(&self) -> &[f64] {
        &self.samples
    }
    #[allow(dead_code)]
    pub fn last_ms(&self) -> Option<f64> {
        self.last_ms
    }
    pub fn false_starts(&self) -> u32 {
        self.false_starts
    }
    pub fn missed(&self) -> u32 {
        self.missed
    }
    pub fn waits_used(&self) -> &[f64] {
        &self.waits_used
    }
    pub fn summary(&self) -> Option<&RunSummary> {
        self.summary.as_ref()
    }
    pub fn is_active(&self) -> bool {
        !matches!(self.phase, Phase::Idle | Phase::Summary)
    }
    /// 1-based number of the trial in progress
    pub fn trial_number(&self) -> usize {
        (self.samples.len() + 1).min(self.cfg.trials as usize)
    }

    fn draw_wait(&mut self) -> f64 {
        let (lo, hi) = (self.cfg.wait_min_ms.min(self.cfg.wait_max_ms), self.cfg.wait_min_ms.max(self.cfg.wait_max_ms));
        if hi - lo < 1e-9 { lo } else { self.rng.gen_range(lo..=hi) }
    }

    fn begin_wait(&mut self, now: f64) {
        self.wait_ms = self.draw_wait();
        self.waits_used.push(self.wait_ms);
        self.phase = Phase::Waiting;
        self.phase_start = now;
    }

    /// Start a fresh run of `cfg.trials` trials
    pub fn start(&mut self, now: f64) {
        self.samples.clear();
        self.waits_used.clear();
        self.last_ms = None;
        self.false_starts = 0;
        self.missed = 0;
        self.summary = None;
        self.finish_step = None;
        self.begin_wait(now);
    }

    pub fn abort(&mut self) {
        self.phase = Phase::Idle;
        self.finish_step = None;
    }

    /// The configured input (mouse press / key press) happened at `now` (ms)
    pub fn on_input(&mut self, now: f64) {
        match self.phase {
            Phase::Waiting => {
                if !self.cfg.robot {
                    self.false_starts += 1;
                    self.phase = Phase::FalseStart;
                    self.phase_start = now;
                }
            }
            Phase::Ready => {
                let latency = now - self.ready_at;
                self.samples.push(latency);
                self.last_ms = Some(latency);
                self.phase = Phase::Result;
                self.phase_start = now;
                self.finish_step = Some(0);
                self.finish_step_start = now;
                self.frames_in_step = 0;
            }
            _ => {}
        }
    }

    /// Advance timers; call once per frame before [`Engine::visual`]
    pub fn tick(&mut self, now: f64) {
        match self.phase {
            Phase::Waiting => {
                if now - self.phase_start >= self.wait_ms {
                    self.phase = Phase::Ready;
                    self.phase_start = now;
                    self.ready_at = now;
                }
            }
            Phase::FalseStart => {
                if now - self.phase_start >= FALSE_START_MS {
                    self.begin_wait(now);
                }
            }
            Phase::Ready => {
                let limit = if self.cfg.robot { ROBOT_TIMEOUT_MS } else { HUMAN_TIMEOUT_MS };
                if now - self.ready_at >= limit {
                    self.missed += 1;
                    self.begin_wait(now);
                }
            }
            Phase::Result => {
                if let Some(step) = self.finish_step {
                    // every segment lasts at least the configured time AND at least one drawn frame
                    if self.frames_in_step >= 1 && now - self.finish_step_start >= self.cfg.finish_segment_ms {
                        self.frames_in_step = 0;
                        self.finish_step_start = now;
                        self.finish_step = if step + 1 >= 3 { None } else { Some(step + 1) };
                    }
                    if self.finish_step.is_some() {
                        self.frames_in_step += 1;
                    }
                }
                if self.finish_step.is_none() && now - self.phase_start >= self.cfg.result_hold_ms {
                    if self.samples.len() as u32 >= self.cfg.trials {
                        self.summary = Some(summarize(
                            self.cfg.kind,
                            self.cfg.robot,
                            &self.samples,
                            self.false_starts,
                            self.missed,
                            (self.cfg.wait_min_ms, self.cfg.wait_max_ms),
                        ));
                        self.phase = Phase::Summary;
                    } else {
                        self.begin_wait(now);
                    }
                }
            }
            Phase::Idle | Phase::Summary => {}
        }
    }

    pub fn visual(&self) -> Visual {
        let n = self.samples.len();
        let progress = format!("Trial {}/{}", self.trial_number(), self.cfg.trials);
        let verb = match self.cfg.kind {
            InputKind::MouseClick => "CLICK",
            InputKind::KeyPress => "PRESS A KEY",
        };
        match self.phase {
            Phase::Idle => Visual { bg: Bg::Idle, bar: Bar::Dim, label: "Press Start".into() },
            Phase::Waiting => Visual {
                bg: Bg::Red,
                bar: Bar::Black,
                label: format!("{} — wait for GREEN…", progress),
            },
            Phase::FalseStart => Visual { bg: Bg::Yellow, bar: Bar::Black, label: "Too early! That trial restarts".into() },
            Phase::Ready => Visual { bg: Bg::Green, bar: Bar::White, label: format!("{} NOW!", verb) },
            Phase::Result => {
                // after the red-black-red end marker the sensor bar stays black (its idle level) until the
                // next trial; it used to take the blue result background, which a photodiode reads as
                // a different level in the robot test's wait
                let bar = match self.finish_step {
                    Some(0) | Some(2) => Bar::Red,
                    _ => Bar::Black,
                };
                Visual {
                    bg: Bg::Blue,
                    bar,
                    label: format!("{:.1} ms   ({}/{} done)", self.last_ms.unwrap_or(0.0), n, self.cfg.trials),
                }
            }
            Phase::Summary => Visual { bg: Bg::Idle, bar: Bar::Dim, label: "Done".into() },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the engine at a fixed frame rate; `press_after_ms` is the reaction time after green.
    fn run(cfg: TrialConfig, frame_ms: f64, press_after_ms: f64, seed: u64) -> Engine {
        let mut e = Engine::new(cfg, Some(seed));
        let mut now = 0.0;
        e.start(now);
        let mut ready_seen: Option<f64> = None;
        for _ in 0..2_000_000 {
            e.tick(now);
            match e.phase() {
                Phase::Ready => {
                    let t0 = *ready_seen.get_or_insert(now);
                    if now - t0 >= press_after_ms {
                        e.on_input(now);
                        ready_seen = None;
                    }
                }
                Phase::Summary => break,
                _ => ready_seen = None,
            }
            now += frame_ms;
        }
        e
    }

    #[test]
    fn ten_trials_with_random_waits_in_range() {
        let e = run(TrialConfig::default(), 1.0, 200.0, 7);
        assert_eq!(e.phase(), Phase::Summary);
        assert_eq!(e.samples().len(), 10);
        let waits = e.waits_used();
        assert_eq!(waits.len(), 10);
        assert!(waits.iter().all(|w| (500.0..=2000.0).contains(w)), "{:?}", waits);
        // unpredictable: not all the same, and spread over the range
        let min = waits.iter().cloned().fold(f64::MAX, f64::min);
        let max = waits.iter().cloned().fold(0.0, f64::max);
        assert!(max - min > 300.0, "waits too uniform: {:?}", waits);
        let s = e.summary().unwrap();
        assert!((s.mean_ms - 200.0).abs() < 1.5);
    }

    #[test]
    fn different_seeds_give_different_wait_sequences() {
        let a = run(TrialConfig::default(), 2.0, 100.0, 1);
        let b = run(TrialConfig::default(), 2.0, 100.0, 2);
        assert_ne!(a.waits_used(), b.waits_used());
    }

    #[test]
    fn latency_is_measured_from_the_ready_frame() {
        let mut e = Engine::new(TrialConfig { trials: 1, wait_min_ms: 500.0, wait_max_ms: 500.0, ..Default::default() }, Some(1));
        e.start(0.0);
        e.tick(100.0);
        assert_eq!(e.phase(), Phase::Waiting);
        e.tick(500.0);
        assert_eq!(e.phase(), Phase::Ready);
        e.on_input(512.25);
        assert!((e.last_ms().unwrap() - 12.25).abs() < 1e-9);
        assert_eq!(e.phase(), Phase::Result);
    }

    #[test]
    fn early_press_is_a_false_start_and_the_trial_repeats() {
        let mut e = Engine::new(TrialConfig { trials: 1, wait_min_ms: 500.0, wait_max_ms: 500.0, ..Default::default() }, Some(1));
        e.start(0.0);
        e.tick(100.0);
        e.on_input(150.0);
        assert_eq!(e.phase(), Phase::FalseStart);
        assert_eq!(e.false_starts(), 1);
        e.tick(1100.0); // > 900 ms later
        assert_eq!(e.phase(), Phase::Waiting);
        assert!(e.samples().is_empty());
        assert_eq!(e.visual().bar, Bar::Black);
    }

    #[test]
    fn robot_mode_ignores_early_input_and_times_out() {
        let mut e = Engine::new(TrialConfig { robot: true, trials: 1, wait_min_ms: 500.0, wait_max_ms: 500.0, ..Default::default() }, Some(1));
        e.start(0.0);
        e.on_input(100.0);
        assert_eq!(e.phase(), Phase::Waiting);
        assert_eq!(e.false_starts(), 0);
        e.tick(500.0);
        assert_eq!(e.phase(), Phase::Ready);
        e.tick(500.0 + 1600.0);
        assert_eq!(e.phase(), Phase::Waiting);
        assert_eq!(e.missed(), 1);
    }

    #[test]
    fn bar_colours_follow_the_protocol() {
        let mut e = Engine::new(TrialConfig { trials: 2, wait_min_ms: 500.0, wait_max_ms: 500.0, ..Default::default() }, Some(1));
        assert_eq!(e.visual().bg, Bg::Idle);
        e.start(0.0);
        let v = e.visual();
        assert_eq!((v.bg, v.bar), (Bg::Red, Bar::Black));
        e.tick(500.0);
        let v = e.visual();
        assert_eq!((v.bg, v.bar), (Bg::Green, Bar::White));
        e.on_input(510.0);
        let v = e.visual();
        assert_eq!((v.bg, v.bar), (Bg::Blue, Bar::Red));
    }

    #[test]
    fn finish_signal_is_red_black_red_each_at_least_5ms_and_one_frame() {
        for frame_ms in [1.0, 4.2, 6.9, 16.7] {
            let mut e = Engine::new(TrialConfig { trials: 1, wait_min_ms: 500.0, wait_max_ms: 500.0, ..Default::default() }, Some(1));
            e.start(0.0);
            let mut now = 500.0;
            e.tick(now);
            e.on_input(now + 3.0);
            // record what each drawn frame shows after the input
            let mut frames: Vec<(f64, Bar)> = Vec::new();
            now += 3.0;
            for _ in 0..200 {
                e.tick(now);
                if e.phase() != Phase::Result {
                    break;
                }
                frames.push((now, e.visual().bar));
                now += frame_ms;
            }
            // collapse into runs
            let mut runs: Vec<(Bar, f64, u32)> = Vec::new(); // bar, first time, frame count
            for (t, b) in &frames {
                match runs.last_mut() {
                    Some(r) if r.0 == *b => r.2 += 1,
                    _ => runs.push((*b, *t, 1)),
                }
            }
            let seq: Vec<Bar> = runs.iter().map(|r| r.0).collect();
            assert_eq!(&seq[..4], &[Bar::Red, Bar::Black, Bar::Red, Bar::Black], "frame {} ms: {:?}", frame_ms, seq);
            for i in 0..3 {
                assert!(runs[i].2 >= 1);
                let dur = runs[i + 1].1 - runs[i].1;
                assert!(dur >= 5.0 - 1e-9, "segment {} lasted {} ms at {} ms frames", i, dur, frame_ms);
            }
        }
    }

    #[test]
    fn segment_length_is_configurable_for_slow_monitors() {
        let mut e = Engine::new(TrialConfig { trials: 1, wait_min_ms: 500.0, wait_max_ms: 500.0, finish_segment_ms: 20.0, ..Default::default() }, Some(1));
        e.start(0.0);
        e.tick(500.0);
        e.on_input(501.0);
        e.tick(501.0);
        e.tick(510.0);
        assert_eq!(e.visual().bar, Bar::Red);
        e.tick(525.0);
        assert_eq!(e.visual().bar, Bar::Black);
    }

    #[test]
    fn summary_statistics() {
        let s = summarize(InputKind::KeyPress, false, &[10.0, 20.0, 30.0, 40.0], 1, 0, (500.0, 2000.0));
        assert_eq!(s.mean_ms, 25.0);
        assert_eq!(s.median_ms, 25.0);
        assert_eq!((s.min_ms, s.max_ms), (10.0, 40.0));
        assert!((s.std_ms - 12.9099).abs() < 1e-3);
        let one = summarize(InputKind::MouseClick, true, &[7.0], 0, 0, (0.0, 0.0));
        assert_eq!((one.std_ms, one.median_ms), (0.0, 7.0));
        assert_eq!(summarize(InputKind::MouseClick, true, &[], 0, 0, (0.0, 0.0)).mean_ms, 0.0);
    }

    #[test]
    fn extra_input_during_result_is_ignored() {
        let mut e = Engine::new(TrialConfig { trials: 3, wait_min_ms: 500.0, wait_max_ms: 500.0, ..Default::default() }, Some(1));
        e.start(0.0);
        e.tick(500.0);
        e.on_input(520.0);
        e.on_input(521.0); // double click / key bounce
        assert_eq!(e.samples().len(), 1);
    }

    #[test]
    fn keyboard_and_mouse_share_the_engine_but_label_differently() {
        let m = Engine::new(TrialConfig::default(), Some(1));
        let k = Engine::new(TrialConfig { kind: InputKind::KeyPress, ..Default::default() }, Some(1));
        assert_ne!(m.cfg.kind.label(), k.cfg.kind.label());
    }
}

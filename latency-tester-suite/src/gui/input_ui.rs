//! Input tab: separate mouse-click and keyboard-press tests (10 trials averaged, random 500–2000 ms
//! waits, signalling bar for the photodiode rig), display test patterns, rig calibration, and the
//! existing OS timing suite.

use eframe::egui;
use egui::{Color32, RichText, Sense, Ui};

use super::LatencyTesterApp;
use crate::input_latency::measure_timer_resolution;
use crate::input_test::{Bar, Bg, Engine, InputKind, Phase, RunSummary, TrialConfig};
use crate::rig::{stats, Corrected, DisplayTest, DisplayTestConfig, PatternMode, RigCalibration};
use crate::timer::HighResTimer;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SubTab {
    Mouse,
    Keyboard,
    Display,
    /// Move the mouse: polling rate and jitter from its raw reports
    Polling,
    /// Click red circles as fast as possible
    Aim,
}

/// A finished run plus the calibration that was active when it finished
#[derive(Clone)]
pub(super) struct RunRecord {
    pub summary: RunSummary,
    pub cal: RigCalibration,
}

impl RunRecord {
    pub fn corrected(&self, raw_ms: f64) -> Corrected {
        self.cal.correct(self.summary.kind, self.summary.robot, raw_ms)
    }
}

pub(super) struct InputTestUi {
    pub sub: SubTab,
    pub cfg: TrialConfig,
    pub engine: Engine,
    pub cal: RigCalibration,
    pub cal_path: std::path::PathBuf,
    pub cal_note: String,
    pub runs: Vec<RunRecord>,
    recorded: bool,
    pub display: DisplayTest,
    clock: HighResTimer,
    timer_info: Option<crate::input_latency::TimerResolutionInfo>,
    /// Precise pattern window: flash or ghosting settings, display and lead-in
    pub pattern: crate::pattern_window::PatternArgs,
    displays: Vec<crate::displays::Display>,
}

impl InputTestUi {
    pub fn new() -> Self {
        let cfg = TrialConfig::default();
        let cal_path = RigCalibration::default_path();
        Self {
            sub: SubTab::Mouse,
            engine: Engine::new(cfg.clone(), None),
            cfg,
            cal: RigCalibration::load(&cal_path),
            cal_path,
            cal_note: String::new(),
            runs: Vec::new(),
            recorded: false,
            display: DisplayTest::new(DisplayTestConfig::default()),
            clock: HighResTimer::new(),
            timer_info: None,
            pattern: crate::pattern_window::PatternArgs::default(),
            displays: crate::displays::list(),
        }
    }

    fn now_ms(&self) -> f64 {
        self.clock.now_ns() as f64 / 1e6
    }

    /// Show the `kind` sub-tab and start a fresh run of trials (same as pressing Start)
    pub fn begin_run(&mut self, kind: InputKind) {
        self.sub = match kind {
            InputKind::MouseClick => SubTab::Mouse,
            InputKind::KeyPress => SubTab::Keyboard,
        };
        self.cfg.kind = kind;
        self.engine = Engine::new(self.cfg.clone(), None);
        self.recorded = false;
        let now = self.now_ms();
        self.engine.start(now);
    }

    fn kind(&self) -> Option<InputKind> {
        match self.sub {
            SubTab::Mouse => Some(InputKind::MouseClick),
            SubTab::Keyboard => Some(InputKind::KeyPress),
            SubTab::Display | SubTab::Polling | SubTab::Aim => None,
        }
    }
}

fn bg_color(bg: Bg) -> Color32 {
    match bg {
        Bg::Idle => Color32::from_rgb(40, 40, 40),
        Bg::Red => Color32::from_rgb(180, 50, 50),
        Bg::Green => Color32::from_rgb(50, 190, 50),
        Bg::Blue => Color32::from_rgb(50, 50, 180),
        Bg::Yellow => Color32::from_rgb(170, 150, 40),
    }
}

/// Pure colours for the sensor bar: black and white must be exactly 0 / 255
fn bar_color(bar: Bar) -> Color32 {
    match bar {
        Bar::Dim => Color32::from_rgb(25, 25, 25),
        Bar::Black => Color32::BLACK,
        Bar::White => Color32::WHITE,
        Bar::Red => Color32::from_rgb(255, 0, 0),
    }
}

impl LatencyTesterApp {
    pub(super) fn render_input_tab(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        if self.input_test.timer_info.is_none() {
            self.input_test.timer_info = measure_timer_resolution().ok();
        }
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.heading("Input Latency");
            ui.horizontal_wrapped(|ui| {
                let active = self.input_test.engine.is_active() || self.input_test.display.is_running() || self.polling.is_active() || self.aim.is_active();
                ui.add_enabled_ui(!active, |ui| {
                    ui.selectable_value(&mut self.input_test.sub, SubTab::Mouse, "🖱 Mouse click test");
                    ui.selectable_value(&mut self.input_test.sub, SubTab::Keyboard, "⌨ Keyboard press test");
                    ui.selectable_value(&mut self.input_test.sub, SubTab::Polling, "🖱 Mouse polling (move the mouse)");
                    ui.selectable_value(&mut self.input_test.sub, SubTab::Aim, "🎯 Reflex game");
                    ui.selectable_value(&mut self.input_test.sub, SubTab::Display, "🖥 Display / rig patterns");
                });
                ui.label(RichText::new(format!("drawn with {}", self.renderer)).weak().small());
                if let Some(info) = &self.input_test.timer_info {
                    ui.label(RichText::new(format!("timer {} Hz · overhead {} ns", info.frequency_hz, info.overhead_ns)).weak().small());
                }
            });
            ui.separator();

            match self.input_test.sub {
                SubTab::Mouse | SubTab::Keyboard => self.trial_panel(ui, ctx),
                SubTab::Display => self.display_panel(ui, ctx),
                SubTab::Polling => self.polling_panel(ui, ctx),
                SubTab::Aim => self.aim_panel(ui, ctx),
            }

            ui.separator();
            self.calibration_panel(ui);
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(RichText::new("OS timing suite").strong());
                if ui.add_enabled(!self.is_running(), egui::Button::new("Run timing suite")).clicked() {
                    self.start_input_benchmark();
                }
                self.stop_button(ui);
            });
            self.input_suite_options(ui);
            self.input_suite_results(ui);
        });
    }

    // ------------------------------------------------------------------ mouse / keyboard trials

    fn trial_panel(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let kind = self.input_test.kind().unwrap_or(InputKind::MouseClick);
        self.input_test.cfg.kind = kind;
        let active = self.input_test.engine.is_active();

        let (what, key_hint) = match kind {
            InputKind::MouseClick => ("click the mouse", ""),
            InputKind::KeyPress => ("press any key", " A harmless key such as Scroll Lock or F13 is best for a robot."),
        };
        ui.label(format!(
            "Wait for the screen to turn GREEN (the bar on its right turns white), then {} as fast as you can. \
             The wait between trials is random (500–2000 ms) so you cannot anticipate it.{}",
            what, key_hint
        ));

        ui.add_enabled_ui(!active, |ui| {
            egui::CollapsingHeader::new("Test options").default_open(false).show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Trials averaged:");
                    ui.add(egui::DragValue::new(&mut self.input_test.cfg.trials).range(1..=1000));
                    ui.label("Wait between trials:");
                    ui.add(egui::DragValue::new(&mut self.input_test.cfg.wait_min_ms).range(0.0..=60_000.0).speed(10.0).suffix(" ms"));
                    ui.label("to");
                    let lo = self.input_test.cfg.wait_min_ms;
                    ui.add(egui::DragValue::new(&mut self.input_test.cfg.wait_max_ms).range(lo..=60_000.0).speed(10.0).suffix(" ms"));
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label("Finish-signal segment:");
                    ui.add(egui::DragValue::new(&mut self.input_test.cfg.finish_segment_ms).range(1.0..=100.0).speed(0.5).suffix(" ms"))
                        .on_hover_text("The bar flashes red / black / red at the end of each trial so a rig knows the trial is over. Each segment is shown for at least one frame, so on a 60 Hz monitor raise this to ~17 ms or more.");
                    ui.label("Result screen:");
                    ui.add(egui::DragValue::new(&mut self.input_test.cfg.result_hold_ms).range(50.0..=5000.0).speed(10.0).suffix(" ms"));
                });
                ui.checkbox(
                    &mut self.input_test.cfg.robot,
                    "Automated rig: a robot (photodiode + Arduino) presses when it sees the white bar",
                )
                .on_hover_text("No false starts, a short timeout, and the rig-calibration offsets below are applied to the result.");
            });
        });

        ui.horizontal(|ui| {
            if !active {
                let label = if self.input_test.cfg.robot { "▶ Start automated run" } else { "▶ Start test" };
                if ui.button(RichText::new(label).strong()).clicked() {
                    self.input_test.begin_run(kind);
                    self.log(&format!("Started {} test ({} trials)", kind.label(), self.input_test.cfg.trials));
                }
            } else if ui.button(RichText::new("⏹ Abort").color(Color32::from_rgb(255, 120, 120))).clicked() {
                self.input_test.engine.abort();
                self.log("Input test aborted");
            }
            let e = &self.input_test.engine;
            if active {
                ui.label(format!(
                    "trial {}/{} · false starts {} · missed {}",
                    e.trial_number(),
                    e.cfg.trials,
                    e.false_starts(),
                    e.missed()
                ));
            }
        });

        self.draw_trial_surface(ui, ctx, kind);
        self.record_finished_run();
        self.trial_results(ui);
    }

    fn draw_trial_surface(&mut self, ui: &mut Ui, ctx: &egui::Context, kind: InputKind) {
        let height = (ui.available_height() * 0.55).clamp(220.0, 420.0);
        let (rect, _resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height), Sense::click());

        // Timestamp first, then handle the input, then advance time: a press that arrives in the same
        // frame as the switch to GREEN happened before GREEN was drawn and is not counted as a reaction.
        let now = self.input_test.now_ms();
        let pressed = match kind {
            InputKind::MouseClick => ui.input(|i| {
                i.pointer.primary_pressed() && i.pointer.interact_pos().map_or(false, |p| rect.contains(p))
            }),
            InputKind::KeyPress => ui.input(|i| {
                i.events.iter().any(|e| matches!(e, egui::Event::Key { pressed: true, repeat: false, .. }))
            }),
        };
        if self.input_test.engine.is_active() {
            if pressed {
                self.input_test.engine.on_input(now);
            }
            self.input_test.engine.tick(now);
            ctx.request_repaint(); // keep frames coming so timing is not tied to input events
        }

        let visual = self.input_test.engine.visual();
        let bg = bg_color(visual.bg);
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, bg);
        let bar_w = rect.width() * self.input_test.cfg.bar_fraction;
        let bar_rect = egui::Rect::from_min_max(egui::pos2(rect.right() - bar_w, rect.top()), rect.right_bottom());
        painter.rect_filled(bar_rect, 0.0, bar_color(visual.bar));
        let text_center = egui::pos2(rect.left() + (rect.width() - bar_w) / 2.0, rect.center().y);
        painter.text(text_center, egui::Align2::CENTER_CENTER, &visual.label, egui::FontId::proportional(30.0), Color32::WHITE);
    }

    /// Store a finished run once
    fn record_finished_run(&mut self) {
        if self.input_test.recorded {
            return;
        }
        if let Some(summary) = self.input_test.engine.summary().cloned() {
            self.input_test.recorded = true;
            let rec = RunRecord { summary, cal: self.input_test.cal.clone() };
            let c = rec.corrected(rec.summary.mean_ms);
            self.log(&format!(
                "{} test finished: mean {:.2} ms over {} trials (median {:.2}, σ {:.2}){}",
                rec.summary.kind.label(),
                rec.summary.mean_ms,
                rec.summary.samples_ms.len(),
                rec.summary.median_ms,
                rec.summary.std_ms,
                if rec.summary.robot { format!(", rig-corrected {:.2} ms", c.minus_robot_and_display_ms) } else { String::new() }
            ));
            self.input_test.runs.push(rec);
            if self.input_test.runs.len() > 50 {
                self.input_test.runs.remove(0);
            }
        }
    }

    fn trial_results(&mut self, ui: &mut Ui) {
        let kind = self.input_test.kind().unwrap_or(InputKind::MouseClick);
        let e = &self.input_test.engine;
        let live: Vec<f64> = e.samples().to_vec();
        if !live.is_empty() {
            ui.add_space(6.0);
            let running_mean = live.iter().sum::<f64>() / live.len() as f64;
            let title = if e.summary().is_some() { "Result" } else { "So far" };
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{}: average {:.2} ms", title, running_mean)).size(22.0).strong().color(Color32::LIGHT_GREEN));
                    ui.label(RichText::new(format!("over {} of {} trials", live.len(), e.cfg.trials)).weak());
                });
                if let Some(s) = e.summary() {
                    ui.label(format!(
                        "median {:.2} · min {:.2} · max {:.2} · σ {:.2} ms   ·   false starts {} · missed {}",
                        s.median_ms, s.min_ms, s.max_ms, s.std_ms, s.false_starts, s.missed
                    ));
                    if s.robot {
                        let cal = &self.input_test.cal;
                        let c = cal.correct(kind, true, s.mean_ms);
                        ui.label(format!(
                            "Robot run: raw {:.2} ms → minus robot delay ({:.2} ms) = {:.2} ms → minus display delay ({:.2} ms) = {:.2} ms",
                            c.raw_ms, cal.robot_ms(kind), c.minus_robot_ms, cal.display_ms, c.minus_robot_and_display_ms
                        ));
                        ui.label(RichText::new("Only the subtractions enabled under Rig calibration are applied to the graphs and log.").weak().small());
                    }
                }
                ui.horizontal_wrapped(|ui| {
                    for (i, s) in live.iter().enumerate() {
                        ui.label(RichText::new(format!("#{} {:.1}", i + 1, s)).monospace());
                    }
                });
                let waits: Vec<String> = e.waits_used().iter().map(|w| format!("{:.0}", w)).collect();
                ui.label(RichText::new(format!("random waits used (ms): {}", waits.join(", "))).weak().small());
            });
        }
        if !self.input_test.runs.is_empty() {
            egui::CollapsingHeader::new(format!("Earlier runs ({})", self.input_test.runs.len())).show(ui, |ui| {
                for r in self.input_test.runs.iter().rev() {
                    let s = &r.summary;
                    ui.label(format!(
                        "{} · {} · mean {:.2} ms · median {:.2} · σ {:.2} · {} trials",
                        s.kind.label(),
                        if s.robot { "robot" } else { "human" },
                        s.mean_ms,
                        s.median_ms,
                        s.std_ms,
                        s.samples_ms.len()
                    ));
                }
                if ui.small_button("Clear history").clicked() {
                    self.input_test.runs.clear();
                }
            });
        }
    }

    // ------------------------------------------------------------------ display patterns

    fn display_panel(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        ui.label(
            "Patterns for the photodiode + Arduino rig (docs/latency-rig). The sensor goes on the bar at the right edge of the \
             pattern area (or anywhere, with \"whole area\").",
        );
        let running = self.input_test.display.is_running();
        ui.add_enabled_ui(!running, |ui| {
            let d = &mut self.input_test.display.cfg;
            ui.horizontal_wrapped(|ui| {
                ui.radio_value(&mut d.mode, PatternMode::SquareWave, "Square wave (rise / fall / on-time / frame timing)");
                ui.radio_value(&mut d.mode, PatternMode::FlashOnKey, "Flash when the Arduino sends F13 (click-to-photon latency)");
            });
            ui.horizontal_wrapped(|ui| match d.mode {
                PatternMode::SquareWave => {
                    ui.label("White:");
                    ui.add(egui::DragValue::new(&mut d.on_ms).range(1.0..=2000.0).speed(1.0).suffix(" ms"));
                    ui.label("Black:");
                    ui.add(egui::DragValue::new(&mut d.off_ms).range(1.0..=2000.0).speed(1.0).suffix(" ms"));
                    ui.label("Cycles:");
                    ui.add(egui::DragValue::new(&mut d.cycles).range(1..=10_000));
                }
                PatternMode::FlashOnKey => {
                    ui.label("Flash length:");
                    ui.add(egui::DragValue::new(&mut d.flash_ms).range(5.0..=2000.0).speed(1.0).suffix(" ms"));
                }
            });
            ui.checkbox(&mut self.display_whole_area, "Use the whole area (not just the right-hand bar)");
        });
        ui.group(|ui| {
            ui.label(RichText::new("Precise pattern window (recommended for the rig)").strong());
            ui.label(
                RichText::new(
                    "This app runs with VSync off, so the pattern below changes whenever a frame happens to be drawn and short phases \
                     are shown irregularly. The precise window runs full screen, VSync-locked, at the highest timer resolution and \
                     switches only on whole refresh periods: every flash is identical. A display cannot show less than one refresh, \
                     so e.g. 1 ms white becomes exactly one frame. Esc closes it, I hides the text.",
                )
                .weak()
                .small(),
            );
            use crate::pattern_window::Motion;
            let pat = &mut self.input_test.pattern;
            ui.horizontal_wrapped(|ui| {
                ui.label("Show:");
                ui.radio_value(&mut pat.motion, None, "white / black flashes");
                if ui.radio(pat.motion.is_some(), "ghosting test (moving lines / square)").clicked() && pat.motion.is_none() {
                    pat.motion = Some(Motion::Both);
                }
            });
            if let Some(m) = pat.motion.as_mut() {
                ui.horizontal_wrapped(|ui| {
                    ui.radio_value(m, Motion::Vertical, "vertical line, left → right");
                    ui.radio_value(m, Motion::Horizontal, "horizontal line, top → bottom");
                    ui.radio_value(m, Motion::Both, "both (they cross exactly at the centre)");
                    ui.radio_value(m, Motion::Square, "square");
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label(if *m == Motion::Square { "Square size:" } else { "Line width:" });
                    ui.add(egui::DragValue::new(&mut pat.width).range(1.0..=600.0).suffix(" px"));
                    ui.label("One sweep takes:");
                    ui.add(egui::DragValue::new(&mut pat.sweep_ms).range(100.0..=60_000.0).speed(10.0).suffix(" ms"));
                    ui.label(RichText::new("(whole frames; a fast sweep shows more ghosting)").weak().small());
                });
            }
            ui.horizontal_wrapped(|ui| {
                ui.label("Display:");
                let current = match pat.display {
                    Some(i) => self.input_test.displays.get(i).map(|d| d.label(i)).unwrap_or_else(|| format!("display {}", i + 1)),
                    None => "where Windows puts it".to_string(),
                };
                egui::ComboBox::from_id_salt("pattern_display").selected_text(current).show_ui(ui, |ui| {
                    ui.selectable_value(&mut pat.display, None, "where Windows puts it");
                    for (i, d) in self.input_test.displays.iter().enumerate() {
                        ui.selectable_value(&mut pat.display, Some(i), d.label(i));
                    }
                });
                if ui.small_button("⟳").on_hover_text("Look for displays again").clicked() {
                    self.input_test.displays = crate::displays::list();
                }
                ui.label("Start after:");
                let mut secs = pat.delay_ms / 1000.0;
                if ui.add(egui::DragValue::new(&mut secs).range(0.0..=600.0).speed(0.1).suffix(" s")).changed() {
                    pat.delay_ms = secs * 1000.0;
                }
            });
            ui.horizontal(|ui| {
                let d = self.input_test.display.cfg.clone();
                if ui.button(RichText::new("▶ Open precise pattern window").strong()).clicked() {
                    let mut p = self.input_test.pattern.clone();
                    (p.on_ms, p.off_ms, p.cycles) = (d.on_ms, d.off_ms, d.cycles);
                    let args = crate::pattern_window::command_args(&p);
                    let what = match p.motion {
                        Some(m) => format!("ghosting test ({}, {} px, sweep {} ms)", m.arg(), p.width, p.sweep_ms),
                        None => format!("white {} ms, black {} ms, {} cycles", d.on_ms, d.off_ms, d.cycles),
                    };
                    match std::env::current_exe().and_then(|exe| std::process::Command::new(exe).args(&args).spawn()) {
                        Ok(_) => self.log(&format!("Opened the precise pattern window: {} · starts after {:.1} s", what, p.delay_ms / 1000.0)),
                        Err(e) => self.log(&format!("Could not open the pattern window: {}", e)),
                    }
                }
                if self.input_test.pattern.motion.is_none() {
                    ui.label(RichText::new("uses the White / Black / Cycles values below").weak().small());
                }
            });
        });
        ui.horizontal(|ui| {
            if !running {
                if ui.button(RichText::new("▶ Start pattern").strong()).clicked() {
                    let now = self.input_test.now_ms();
                    self.input_test.display.start(now);
                }
            } else if ui.button(RichText::new("⏹ Stop").color(Color32::from_rgb(255, 120, 120))).clicked() {
                self.input_test.display.stop();
            }
            if running {
                match self.input_test.display.cfg.mode {
                    PatternMode::SquareWave => ui.label(format!("cycle {}/{}", self.input_test.display.cycles_done() + 1, self.input_test.display.cfg.cycles)),
                    PatternMode::FlashOnKey => ui.label(format!("triggers received: {}", self.input_test.display.triggers)),
                };
            }
        });

        let height = (ui.available_height() * 0.55).clamp(220.0, 420.0);
        let (rect, _resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height), Sense::click());
        let now = self.input_test.now_ms();
        if running {
            if self.input_test.display.cfg.mode == PatternMode::FlashOnKey {
                // The rig's trigger is a key press (F13 by default); any key counts so it also works with other triggers
                let trig = ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Key { pressed: true, repeat: false, .. })));
                if trig {
                    self.input_test.display.on_trigger(now);
                }
            }
            self.input_test.display.tick(now);
            ctx.request_repaint();
        }
        let white = self.input_test.display.is_white();
        let painter = ui.painter_at(rect);
        let bar_w = rect.width() * 0.10;
        let level = if white { Color32::WHITE } else { Color32::BLACK };
        if self.display_whole_area {
            painter.rect_filled(rect, 0.0, level);
        } else {
            painter.rect_filled(rect, 0.0, Color32::from_rgb(35, 35, 35));
            let bar = egui::Rect::from_min_max(egui::pos2(rect.right() - bar_w, rect.top()), rect.right_bottom());
            painter.rect_filled(bar, 0.0, level);
        }
        if !running {
            painter.text(rect.center(), egui::Align2::CENTER_CENTER, "pattern stopped", egui::FontId::proportional(24.0), Color32::GRAY);
        }

        let t = &self.input_test.display;
        ui.add_space(6.0);
        ui.group(|ui| {
            ui.label(RichText::new("What this program displayed (host side, for comparison with the rig's numbers)").strong());
            if let Some((mn, av, mx)) = stats(&t.on_shown_ms) {
                ui.label(format!("white shown for: min {:.2} · avg {:.2} · max {:.2} ms  ({} phases)", mn, av, mx, t.on_shown_ms.len()));
            }
            if let Some((mn, av, mx)) = stats(&t.off_shown_ms) {
                ui.label(format!("black shown for: min {:.2} · avg {:.2} · max {:.2} ms  ({} phases)", mn, av, mx, t.off_shown_ms.len()));
            }
            if let Some((mn, av, mx)) = stats(&t.trigger_to_frame_ms) {
                ui.label(format!("trigger key → white frame (software part only): min {:.2} · avg {:.2} · max {:.2} ms  ({} triggers)", mn, av, mx, t.triggers));
            }
            ui.label(RichText::new("The rig (Arduino serial monitor) reports what the photodiode saw: rise/fall time, on-time, period and, in F13 mode, trigger-to-light latency.").weak().small());
        });
    }

    // ------------------------------------------------------------------ calibration

    fn calibration_panel(&mut self, ui: &mut Ui) {
        egui::CollapsingHeader::new("Rig calibration (subtract the robot's own delay)").show(ui, |ui| {
            ui.label(
                RichText::new(
                    "Run the sketch's calibrate command (serial monitor: 'c') and type its printed values here. \
                     They are subtracted only from automated-rig runs.",
                )
                .weak()
                .small(),
            );
            {
                let cal = &mut self.input_test.cal;
                ui.horizontal_wrapped(|ui| {
                    ui.label("Mouse robot:");
                    ui.add(egui::DragValue::new(&mut cal.mouse_robot_ms).range(0.0..=100.0).speed(0.01).suffix(" ms"));
                    ui.label("Keyboard robot (solenoid, to actuation):");
                    ui.add(egui::DragValue::new(&mut cal.keyboard_robot_ms).range(0.0..=100.0).speed(0.05).suffix(" ms"));
                    ui.label("Display (frame → light):");
                    ui.add(egui::DragValue::new(&mut cal.display_ms).range(0.0..=200.0).speed(0.05).suffix(" ms"));
                });
                ui.horizontal_wrapped(|ui| {
                    ui.checkbox(&mut cal.subtract_robot, "subtract robot delay");
                    ui.checkbox(&mut cal.subtract_display, "also subtract display delay (input-stack only)");
                });
            }
            ui.horizontal_wrapped(|ui| {
                if ui.button("Save").clicked() {
                    let path = self.input_test.cal_path.clone();
                    self.input_test.cal_note = match self.input_test.cal.save(&path) {
                        Ok(()) => format!("saved to {}", path.display()),
                        Err(e) => format!("could not save: {}", e),
                    };
                }
                ui.label(RichText::new(&self.input_test.cal_note).weak().small());
            });
        });
    }

    fn input_suite_results(&mut self, ui: &mut Ui) {
        if let Some(summary) = &self.last_input_result {
            ui.separator();
            ui.heading("Timing-suite results");
            for result in &summary.results {
                ui.horizontal(|ui| {
                    ui.label(format!("{:?}", result.mode));
                    if let Some(c) = result.core {
                        ui.label(format!("core {}", c));
                    }
                    ui.label(format!("avg: {:.3} ms", result.avg_latency_ms));
                    ui.label(format!("p99: {:.3} ms", result.percentile_99_ms));
                    if let Some(rate) = result.polling_rate_hz {
                        ui.label(format!("polling: {:.0} Hz", rate));
                    }
                });
            }
        }
    }
}

impl InputTestUi {
    /// Phase of the trial engine (used by tests / the graphs tab)
    #[allow(dead_code)]
    pub fn phase(&self) -> Phase {
        self.engine.phase()
    }
}

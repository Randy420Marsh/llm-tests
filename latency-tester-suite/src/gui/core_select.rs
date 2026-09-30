//! Reusable "which cores?" control: all / P-cores / E-cores / P+E / specific cores,
//! plus a "test each core on its own" sweep for finding slow, hot or faulty cores.

use eframe::egui;
use egui::{Color32, RichText, Ui};

use crate::topology::CoreKind;

pub(super) const P_COLOR: Color32 = Color32::from_rgb(255, 170, 80);
pub(super) const E_COLOR: Color32 = Color32::from_rgb(120, 190, 255);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum CoreMode {
    All,
    PCores,
    ECores,
    PAndE,
    Specific,
}

#[derive(Clone)]
pub(super) struct CoreSelector {
    pub mode: CoreMode,
    pub specific: Vec<bool>,
    pub kinds: Option<Vec<CoreKind>>,
    /// Run each resolved core on its own instead of all together
    pub sweep: bool,
    /// With `sweep`: every test on one core before the next core (false = rotate the cores between tests)
    pub core_by_core: bool,
    id: &'static str,
}

impl CoreSelector {
    pub fn new(kinds: Option<Vec<CoreKind>>, id: &'static str) -> Self {
        Self { mode: CoreMode::All, specific: vec![false; num_cpus::get()], kinds, sweep: false, core_by_core: false, id }
    }

    pub fn logical(&self) -> usize {
        self.specific.len()
    }

    fn ids_of(&self, want: CoreKind) -> Vec<usize> {
        self.kinds
            .as_ref()
            .map(|k| k.iter().enumerate().filter(|(_, &c)| c == want).map(|(i, _)| i).collect())
            .unwrap_or_default()
    }

    /// (core ids to pin to — empty means "OS decides", description)
    pub fn resolve(&self) -> (Vec<usize>, String) {
        let all: Vec<usize> = (0..self.logical()).collect();
        match self.mode {
            CoreMode::All if self.sweep => (all.clone(), format!("Each core one at a time ({})", ranges(&all))),
            CoreMode::All => (Vec::new(), "All cores (OS scheduled)".to_string()),
            CoreMode::PCores => {
                let ids = self.ids_of(CoreKind::Performance);
                let label = format!("P-cores {}", ranges(&ids));
                (ids, label)
            }
            CoreMode::ECores => {
                let ids = self.ids_of(CoreKind::Efficiency);
                let label = format!("E-cores {}", ranges(&ids));
                (ids, label)
            }
            CoreMode::PAndE => {
                let mut ids = self.ids_of(CoreKind::Performance);
                ids.extend(self.ids_of(CoreKind::Efficiency));
                let label = format!("P+E cores pinned ({})", ids.len());
                (ids, label)
            }
            CoreMode::Specific => {
                let ids: Vec<usize> = all.into_iter().filter(|&i| self.specific[i]).collect();
                let label = format!("Cores {}", ranges(&ids));
                (ids, label)
            }
        }
    }

    /// Most threads the selection can host
    pub fn max_threads(&self) -> usize {
        let (ids, _) = self.resolve();
        if self.sweep {
            1
        } else if ids.is_empty() {
            self.logical()
        } else {
            ids.len()
        }
    }

    pub fn error(&self) -> Option<String> {
        (self.mode != CoreMode::All && self.resolve().0.is_empty()).then(|| "No cores selected".to_string())
    }

    /// Bit mask of the resolved cores (cores ≥ 64 are not representable)
    pub fn mask(&self) -> u64 {
        self.resolve().0.iter().filter(|&&c| c < 64).fold(0u64, |m, &c| m | (1u64 << c))
    }

    pub fn ui(&mut self, ui: &mut Ui) {
        let hybrid = self.kinds.is_some();
        ui.horizontal_wrapped(|ui| {
            ui.radio_value(&mut self.mode, CoreMode::All, "All (OS decides)");
            ui.add_enabled_ui(hybrid, |ui| {
                ui.radio_value(&mut self.mode, CoreMode::PCores, "P-cores only");
                ui.radio_value(&mut self.mode, CoreMode::ECores, "E-cores only");
                ui.radio_value(&mut self.mode, CoreMode::PAndE, "P + E (pinned)");
            });
            ui.radio_value(&mut self.mode, CoreMode::Specific, "Specific cores");
        });
        if !hybrid {
            ui.label(RichText::new("No P/E core split detected on this CPU, so the P/E options are unavailable.").weak().small());
        }
        if self.mode == CoreMode::Specific {
            ui.horizontal(|ui| {
                if ui.small_button("All").clicked() {
                    self.specific.iter_mut().for_each(|c| *c = true);
                }
                if ui.small_button("None").clicked() {
                    self.specific.iter_mut().for_each(|c| *c = false);
                }
                if ui.small_button("Invert").clicked() {
                    self.specific.iter_mut().for_each(|c| *c = !*c);
                }
                if hybrid {
                    if ui.small_button("P only").clicked() {
                        let kinds = self.kinds.clone().unwrap();
                        for (i, c) in self.specific.iter_mut().enumerate() {
                            *c = kinds[i] == CoreKind::Performance;
                        }
                    }
                    if ui.small_button("E only").clicked() {
                        let kinds = self.kinds.clone().unwrap();
                        for (i, c) in self.specific.iter_mut().enumerate() {
                            *c = kinds[i] == CoreKind::Efficiency;
                        }
                    }
                }
            });
            ui.horizontal_wrapped(|ui| {
                for i in 0..self.specific.len() {
                    let (tag, color) = match self.kinds.as_ref().map(|k| k[i]) {
                        Some(CoreKind::Performance) => ("P", P_COLOR),
                        Some(CoreKind::Efficiency) => ("E", E_COLOR),
                        _ => ("", Color32::GRAY),
                    };
                    ui.checkbox(&mut self.specific[i], RichText::new(format!("{}{}", tag, i)).color(color));
                }
            });
        }
        ui.checkbox(&mut self.sweep, "Test each core on its own (finds slow, hot or faulty cores)")
            .on_hover_text("Runs the test once per core, pinned to just that core. With \"All\" it sweeps every core.");
        if self.sweep {
            ui.horizontal_wrapped(|ui| {
                ui.label("Order:");
                ui.radio_value(&mut self.core_by_core, false, "rotate cores between tests")
                    .on_hover_text("Test 1 on core 0, 1, 2 …, then test 2 on every core: the heat is spread evenly");
                ui.radio_value(&mut self.core_by_core, true, "all tests on one core, then the next")
                    .on_hover_text("Core 0 runs every selected test, then core 1, …: each core is measured in one go");
            });
            ui.label(RichText::new("The app's own threads are moved off the tested core (and its hyper-threading sibling) before each test.").weak().small());
        }
        let (_, label) = self.resolve();
        let extra = if self.sweep { String::new() } else { format!(" — up to {} thread(s)", self.max_threads()) };
        ui.label(RichText::new(format!("{}{}", label, extra)).weak());
        let _ = self.id;
    }
}

/// "0-7,10,12-13"
pub(super) fn ranges(ids: &[usize]) -> String {
    let mut out = Vec::new();
    let mut i = 0;
    while i < ids.len() {
        let mut j = i;
        while j + 1 < ids.len() && ids[j + 1] == ids[j] + 1 {
            j += 1;
        }
        out.push(if j > i { format!("{}-{}", ids[i], ids[j]) } else { ids[i].to_string() });
        i = j + 1;
    }
    if out.is_empty() { "none".to_string() } else { out.join(",") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use CoreKind::*;

    fn sel(kinds: Option<Vec<CoreKind>>, n: usize) -> CoreSelector {
        let mut s = CoreSelector::new(kinds, "t");
        s.specific = vec![false; n];
        s
    }

    #[test]
    fn range_formatting() {
        assert_eq!(ranges(&[0, 1, 2, 3, 10, 12, 13]), "0-3,10,12-13");
        assert_eq!(ranges(&[]), "none");
        assert_eq!(ranges(&[5]), "5");
    }

    #[test]
    fn modes_resolve_to_core_ids() {
        let mut s = sel(Some(vec![Performance, Performance, Efficiency, Efficiency, Efficiency]), 5);
        assert_eq!(s.resolve().0, Vec::<usize>::new());
        assert!(s.error().is_none());
        s.mode = CoreMode::PCores;
        assert_eq!(s.resolve().0, vec![0, 1]);
        assert_eq!(s.mask(), 0b00011);
        s.mode = CoreMode::ECores;
        assert_eq!(s.resolve().0, vec![2, 3, 4]);
        assert_eq!(s.max_threads(), 3);
        s.mode = CoreMode::PAndE;
        assert_eq!(s.resolve().0.len(), 5);
        s.mode = CoreMode::Specific;
        assert!(s.error().is_some());
        s.specific[1] = true;
        s.specific[4] = true;
        let (ids, label) = s.resolve();
        assert_eq!(ids, vec![1, 4]);
        assert_eq!(label, "Cores 1,4");
        assert_eq!(s.mask(), 0b10010);
    }

    #[test]
    fn sweep_covers_every_core_and_uses_one_thread() {
        let mut s = sel(None, 4);
        s.sweep = true;
        assert_eq!(s.resolve().0, vec![0, 1, 2, 3]);
        assert_eq!(s.max_threads(), 1);
        s.mode = CoreMode::Specific;
        s.specific[2] = true;
        assert_eq!(s.resolve().0, vec![2]);
    }
}

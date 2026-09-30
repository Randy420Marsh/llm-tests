//! Reflex mini-game: click red circles as fast as possible (like Aim Labs "gridshot" / "spidershot").
//! Pure game logic; the Input tab draws it and feeds it clicks.
//!
//! Times include the whole chain (see the target, move, click, the click reaching the app), so they
//! compare players and setups rather than isolate the mouse.

use rand::Rng;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AimMode {
    /// One target; each hit spawns the next one somewhere else
    Single,
    /// This many targets on screen; each hit spawns a replacement
    Multi(u32),
}

impl AimMode {
    pub fn on_screen(self) -> usize {
        match self {
            AimMode::Single => 1,
            AimMode::Multi(n) => n.max(1) as usize,
        }
    }

    pub fn label(self) -> String {
        match self {
            AimMode::Single => "one at a time".into(),
            AimMode::Multi(n) => format!("{} at a time", n),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AimConfig {
    pub targets: u32,
    /// Circle radius in points
    pub radius: f32,
    pub mode: AimMode,
}

impl Default for AimConfig {
    fn default() -> Self {
        Self { targets: 100, radius: 28.0, mode: AimMode::Single }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Target {
    /// Centre, in points inside the play area
    pub x: f32,
    pub y: f32,
    /// When it appeared (ms, the game's clock)
    pub spawned_ms: f64,
}

/// One hit
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Hit {
    /// From the moment the target could be aimed at (it appeared, or the previous hit when several are
    /// on screen) to the click, ms
    pub time_ms: f64,
    /// Distance from the previous click to the target's centre, points
    pub distance: f32,
    /// How far from the centre the click landed, in radii (0 = dead centre, 1 = edge)
    pub off_centre: f32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AimResult {
    pub mode: String,
    pub targets: u32,
    pub radius: f32,
    pub hits: usize,
    pub misses: usize,
    pub accuracy_pct: f64,
    pub avg_ms: f64,
    pub median_ms: f64,
    pub p90_ms: f64,
    pub best_ms: f64,
    pub worst_ms: f64,
    pub total_s: f64,
    pub targets_per_s: f64,
    /// Fitts' law throughput: log2(distance / width + 1) bits per second of aiming, averaged over hits
    pub throughput_bits_s: f64,
    /// Average distance of a hit from the centre, in radii
    pub avg_off_centre: f64,
    /// Time of every hit, ms, in order
    pub times_ms: Vec<f64>,
    #[serde(default)]
    pub timestamp: String,
}

pub struct AimGame {
    pub cfg: AimConfig,
    /// Play area size, points
    w: f32,
    h: f32,
    pub targets: Vec<Target>,
    spawned: u32,
    pub hits: Vec<Hit>,
    pub misses: usize,
    started_ms: f64,
    last_hit_ms: f64,
    last_click: Option<(f32, f32)>,
    pub finished_ms: Option<f64>,
    rng: rand::rngs::StdRng,
}

impl AimGame {
    /// A new game in a `w` x `h` area, starting at `now_ms`
    pub fn new(cfg: AimConfig, w: f32, h: f32, now_ms: f64, seed: u64) -> Self {
        use rand::SeedableRng;
        let mut g = Self {
            cfg,
            w,
            h,
            targets: Vec::new(),
            spawned: 0,
            hits: Vec::new(),
            misses: 0,
            started_ms: now_ms,
            last_hit_ms: now_ms,
            last_click: None,
            finished_ms: None,
            rng: rand::rngs::StdRng::seed_from_u64(seed),
        };
        for _ in 0..g.cfg.mode.on_screen().min(g.cfg.targets as usize) {
            g.spawn(now_ms);
        }
        g
    }

    pub fn is_over(&self) -> bool {
        self.finished_ms.is_some()
    }

    /// Targets left to hit (on screen and still to come)
    pub fn remaining(&self) -> u32 {
        self.cfg.targets.saturating_sub(self.hits.len() as u32)
    }

    /// The play area changed size: keep targets inside it
    pub fn resize(&mut self, w: f32, h: f32) {
        let r = self.cfg.radius;
        let (sx, sy) = (w / self.w.max(1.0), h / self.h.max(1.0));
        for t in &mut self.targets {
            t.x = (t.x * sx).clamp(r, (w - r).max(r));
            t.y = (t.y * sy).clamp(r, (h - r).max(r));
        }
        self.w = w;
        self.h = h;
    }

    /// A new target at a random free spot (not overlapping the others)
    fn spawn(&mut self, now_ms: f64) {
        if self.spawned >= self.cfg.targets {
            return;
        }
        let r = self.cfg.radius;
        let (lo_x, hi_x, lo_y, hi_y) = (r, (self.w - r).max(r + 1.0), r, (self.h - r).max(r + 1.0));
        let mut best = (lo_x, lo_y);
        for attempt in 0..60 {
            let (x, y) = (self.rng.gen_range(lo_x..hi_x), self.rng.gen_range(lo_y..hi_y));
            best = (x, y);
            let clear = self.targets.iter().all(|t| ((t.x - x).powi(2) + (t.y - y).powi(2)).sqrt() > 2.5 * r);
            // the next target should not sit right under the cursor either
            let away = self.last_click.map_or(true, |(cx, cy)| ((cx - x).powi(2) + (cy - y).powi(2)).sqrt() > 3.0 * r) || attempt > 40;
            if clear && away {
                break;
            }
        }
        self.targets.push(Target { x: best.0, y: best.1, spawned_ms: now_ms });
        self.spawned += 1;
    }

    /// A click at (x, y); returns true for a hit
    pub fn click(&mut self, x: f32, y: f32, now_ms: f64) -> bool {
        if self.is_over() {
            return false;
        }
        let r = self.cfg.radius;
        // several overlapping candidates can't happen (spawns keep apart), but take the nearest anyway
        let hit = self
            .targets
            .iter()
            .enumerate()
            .map(|(i, t)| (i, ((t.x - x).powi(2) + (t.y - y).powi(2)).sqrt()))
            .filter(|(_, d)| *d <= r)
            .min_by(|a, b| a.1.total_cmp(&b.1));
        let Some((i, d)) = hit else {
            self.misses += 1;
            self.last_click = Some((x, y));
            return false;
        };
        let t = self.targets.remove(i);
        let from = self.last_click.unwrap_or((self.w / 2.0, self.h / 2.0));
        let distance = ((t.x - from.0).powi(2) + (t.y - from.1).powi(2)).sqrt();
        let aim_from = t.spawned_ms.max(self.last_hit_ms);
        self.hits.push(Hit { time_ms: (now_ms - aim_from).max(0.0), distance, off_centre: d / r });
        self.last_hit_ms = now_ms;
        self.last_click = Some((x, y));
        self.spawn(now_ms);
        if self.hits.len() as u32 >= self.cfg.targets {
            self.finished_ms = Some(now_ms);
        }
        true
    }

    /// Results so far (or of the finished game)
    pub fn result(&self, now_ms: f64) -> AimResult {
        let times: Vec<f64> = self.hits.iter().map(|h| h.time_ms).collect();
        let mut sorted = times.clone();
        sorted.sort_by(|a, b| a.total_cmp(b));
        let at = |p: f64| sorted.get(((sorted.len() as f64 * p) as usize).min(sorted.len().saturating_sub(1))).copied().unwrap_or(0.0);
        let n = self.hits.len();
        let clicks = n + self.misses;
        let end = self.finished_ms.unwrap_or(now_ms);
        let total_s = ((end - self.started_ms) / 1000.0).max(0.0);
        let width = 2.0 * self.cfg.radius as f64;
        let fitts: Vec<f64> = self
            .hits
            .iter()
            .filter(|h| h.time_ms > 0.0)
            .map(|h| (h.distance as f64 / width + 1.0).log2() / (h.time_ms / 1000.0))
            .collect();
        AimResult {
            mode: self.cfg.mode.label(),
            targets: self.cfg.targets,
            radius: self.cfg.radius,
            hits: n,
            misses: self.misses,
            accuracy_pct: if clicks > 0 { n as f64 / clicks as f64 * 100.0 } else { 0.0 },
            avg_ms: if n > 0 { times.iter().sum::<f64>() / n as f64 } else { 0.0 },
            median_ms: at(0.5),
            p90_ms: at(0.9),
            best_ms: sorted.first().copied().unwrap_or(0.0),
            worst_ms: sorted.last().copied().unwrap_or(0.0),
            total_s,
            targets_per_s: if total_s > 0.0 { n as f64 / total_s } else { 0.0 },
            throughput_bits_s: if fitts.is_empty() { 0.0 } else { fitts.iter().sum::<f64>() / fitts.len() as f64 },
            avg_off_centre: if n > 0 { self.hits.iter().map(|h| h.off_centre as f64).sum::<f64>() / n as f64 } else { 0.0 },
            times_ms: times,
            timestamp: chrono::Local::now().to_rfc3339(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn game(mode: AimMode, targets: u32) -> AimGame {
        AimGame::new(AimConfig { targets, radius: 20.0, mode }, 800.0, 500.0, 0.0, 7)
    }

    #[test]
    fn single_mode_hits_misses_and_finishes() {
        let mut g = game(AimMode::Single, 3);
        assert_eq!(g.targets.len(), 1);
        assert!(!g.click(-100.0, -100.0, 100.0), "a click outside every target is a miss");
        for k in 0..3 {
            let t = g.targets[0];
            assert!(g.click(t.x + 5.0, t.y, 400.0 * (k + 1) as f64));
            assert!(g.targets.len() <= 1);
        }
        assert!(g.is_over() && g.targets.is_empty());
        let r = g.result(2000.0);
        assert_eq!((r.hits, r.misses), (3, 1));
        assert!((r.accuracy_pct - 75.0).abs() < 1e-9);
        assert_eq!(r.times_ms, vec![400.0, 400.0, 400.0]);
        assert!((r.avg_off_centre - 0.25).abs() < 1e-6);
        assert!(r.throughput_bits_s > 0.0 && (r.total_s - 1.2).abs() < 1e-9);
        assert!(!g.click(10.0, 10.0, 3000.0), "no clicks after the end");
    }

    #[test]
    fn multi_mode_keeps_four_on_screen_and_times_from_the_last_hit() {
        let mut g = game(AimMode::Multi(4), 10);
        assert_eq!(g.targets.len(), 4);
        for (i, t) in g.targets.iter().enumerate() {
            for u in &g.targets[i + 1..] {
                assert!(((t.x - u.x).powi(2) + (t.y - u.y).powi(2)).sqrt() > 2.5 * 20.0, "targets never overlap");
            }
            assert!(t.x >= 20.0 && t.x <= 780.0 && t.y >= 20.0 && t.y <= 480.0, "inside the area");
        }
        let mut now = 0.0;
        while !g.is_over() {
            now += 250.0;
            let t = g.targets[0];
            assert!(g.click(t.x, t.y, now));
            let left = g.remaining() as usize;
            assert_eq!(g.targets.len(), left.min(4));
        }
        let r = g.result(now);
        assert_eq!(r.hits, 10);
        assert!(r.times_ms.iter().all(|t| (*t - 250.0).abs() < 1e-9), "{:?}", r.times_ms);
        assert_eq!(r.mode, "4 at a time");
    }
}

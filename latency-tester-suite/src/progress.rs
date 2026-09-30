//! Lightweight live progress shared between a running suite and the GUI

use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Debug, Clone, Default)]
pub struct RunProgress {
    pub total: usize,
    pub done: usize,
    /// What is running now, e.g. "IntegerAdd · 4 threads · core 6"
    pub title: String,
    /// Extra detail, e.g. "measuring run 2/3"
    pub detail: String,
    pub started: Option<Instant>,
    /// Set when the suite ended (finished, stopped or failed): the panel stops counting and says so
    pub finished: Option<Instant>,
}

impl RunProgress {
    /// Still going (not every test done and not marked finished)
    pub fn active(&self) -> bool {
        self.finished.is_none() && self.done < self.total
    }

    /// Seconds from the start to now, or to the end once finished
    pub fn elapsed_s(&self) -> f64 {
        match (self.started, self.finished) {
            (Some(s), Some(f)) => f.saturating_duration_since(s).as_secs_f64(),
            (Some(s), None) => s.elapsed().as_secs_f64(),
            _ => 0.0,
        }
    }
}

/// Mark the suite behind `handle` as ended
pub fn finish(handle: &SharedProgress) {
    if let Ok(mut g) = handle.lock() {
        if g.finished.is_none() {
            g.finished = Some(Instant::now());
        }
    }
}

pub type SharedProgress = Arc<Mutex<RunProgress>>;

pub fn new() -> SharedProgress {
    Arc::new(Mutex::new(RunProgress::default()))
}

/// Apply `f` to the progress if a handle is attached
pub fn update(handle: &Option<SharedProgress>, f: impl FnOnce(&mut RunProgress)) {
    if let Some(h) = handle {
        if let Ok(mut g) = h.lock() {
            f(&mut g);
        }
    }
}

/// Results of finished tests, readable while the suite is still running (and after a Stop)
pub type SharedResults<T> = Arc<Mutex<Vec<T>>>;

pub fn new_results<T>() -> SharedResults<T> {
    Arc::new(Mutex::new(Vec::new()))
}

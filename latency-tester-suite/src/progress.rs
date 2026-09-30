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

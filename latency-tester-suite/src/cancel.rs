//! Cooperative cancellation shared by all benchmark suites

use anyhow::{anyhow, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Set to `true` (e.g. by a Stop button) to ask a running suite to finish early
pub type CancelFlag = Arc<AtomicBool>;

/// Error text used when a run is stopped on request; the GUI matches on it
pub const CANCELLED_MSG: &str = "cancelled by user";

pub fn new_flag() -> CancelFlag {
    Arc::new(AtomicBool::new(false))
}

pub fn is_cancelled(flag: &CancelFlag) -> bool {
    flag.load(Ordering::Relaxed)
}

/// `Err` once cancellation was requested; call between units of work with `?`
pub fn check(flag: &CancelFlag) -> Result<()> {
    if is_cancelled(flag) {
        Err(anyhow!(CANCELLED_MSG))
    } else {
        Ok(())
    }
}

pub fn is_cancel_error(e: &anyhow::Error) -> bool {
    e.to_string() == CANCELLED_MSG
}

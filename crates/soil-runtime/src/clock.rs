//! Injected time. The engine never reads the system clock directly.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub trait Clock: Send + Sync {
    /// Milliseconds since the Unix epoch.
    fn now_ms(&self) -> u64;
}

/// Deterministic clock: starts at `start` and advances by `step` on every read.
pub struct LogicalClock {
    next: AtomicU64,
    step: u64,
}

impl LogicalClock {
    pub fn new(start: u64, step: u64) -> Self {
        Self {
            next: AtomicU64::new(start),
            step,
        }
    }
}

impl Clock for LogicalClock {
    fn now_ms(&self) -> u64 {
        self.next.fetch_add(self.step, Ordering::SeqCst)
    }
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}
